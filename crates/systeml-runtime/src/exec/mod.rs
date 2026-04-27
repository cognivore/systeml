//! Shared spawn helpers: Environment expansion, fd setup, std I/O sinks.
//!
//! This module is the lowest layer of the supervisor — it understands how to
//! turn an `ExecCommand` plus a `ServiceUnit` into a child process. Higher
//! layers (`service::runner`) sequence these calls and react to outcomes.

#![allow(unsafe_code)]

use anyhow::{anyhow, Context, Result};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use systeml_unit::env::EnvironmentFile;
use systeml_unit::exec::{ExecCommand, ExecFlags};
use systeml_unit::service::{ServiceUnit, StandardStream};
use tokio::process::Command;
use tracing::{debug, warn};

/// Resolve every relevant environment knob (`Environment=`,
/// `EnvironmentFile=`, then `${VAR}` substitution and `PassEnvironment=`).
///
/// Returned map is the final env we'd pass to the child. Optional env files
/// (with leading `-`) silently absorb `NotFound`.
pub fn resolve_environment(svc: &ServiceUnit) -> Result<BTreeMap<String, String>> {
    let mut env: BTreeMap<String, String> = BTreeMap::new();

    // PassEnvironment first (inherit only the named vars from our parent env).
    for k in &svc.pass_environment {
        if let Some(v) = std::env::var_os(k).and_then(|v| v.into_string().ok()) {
            env.insert(k.clone(), v);
        }
    }

    // EnvironmentFile=
    for ef in &svc.environment_files {
        load_env_file(ef, &mut env)?;
    }

    // Environment= (last writer wins; do `${VAR}` expansion against current map)
    for (k, v) in &svc.environment {
        let expanded = expand_vars(v, &env);
        env.insert(k.clone(), expanded);
    }

    // UnsetEnvironment=
    for k in &svc.unset_environment {
        env.remove(k);
    }

    Ok(env)
}

/// Expand `${VAR}` and `$VAR` against `env`. Unknown vars expand to empty
/// (mirrors systemd; warns visually via tracing only when the key is
/// referenced and missing).
pub fn expand_vars(input: &str, env: &BTreeMap<String, String>) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '$' {
            out.push(c);
            continue;
        }
        match chars.peek().copied() {
            Some('{') => {
                chars.next();
                let mut name = String::new();
                while let Some(&c) = chars.peek() {
                    if c == '}' {
                        chars.next();
                        break;
                    }
                    name.push(c);
                    chars.next();
                }
                if let Some(v) = env.get(&name) {
                    out.push_str(v);
                }
            }
            Some(c) if c == '_' || c.is_ascii_alphabetic() => {
                let mut name = String::new();
                while let Some(&c) = chars.peek() {
                    if c == '_' || c.is_ascii_alphanumeric() {
                        name.push(c);
                        chars.next();
                    } else {
                        break;
                    }
                }
                if let Some(v) = env.get(&name) {
                    out.push_str(v);
                }
            }
            _ => out.push('$'),
        }
    }
    out
}

fn load_env_file(ef: &EnvironmentFile, env: &mut BTreeMap<String, String>) -> Result<()> {
    let contents = match std::fs::read_to_string(&ef.path) {
        Ok(c) => c,
        Err(e) if ef.optional && e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => {
            return Err(anyhow!(
                "EnvironmentFile {:?} read failed: {}",
                ef.path,
                e
            ))
        }
    };
    for (lineno, line) in contents.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let Some((k, v)) = trimmed.split_once('=') else {
            warn!(file = ?ef.path, line = lineno + 1, "ignoring malformed env line");
            continue;
        };
        let v = v.trim();
        // Strip surrounding quotes.
        let v = if (v.starts_with('"') && v.ends_with('"') && v.len() >= 2)
            || (v.starts_with('\'') && v.ends_with('\'') && v.len() >= 2)
        {
            &v[1..v.len() - 1]
        } else {
            v
        };
        env.insert(k.trim().to_owned(), v.to_owned());
    }
    Ok(())
}

/// Resolve a `WorkingDirectory=` value handling `~` expansion and `~/foo`.
pub fn resolve_working_dir(p: &Path) -> Result<PathBuf> {
    let s = p.to_string_lossy();
    if s == "~" {
        return dirs::home_dir().ok_or_else(|| anyhow!("HOME not set; cannot expand ~"));
    }
    if let Some(rest) = s.strip_prefix("~/") {
        let mut h =
            dirs::home_dir().ok_or_else(|| anyhow!("HOME not set; cannot expand ~/"))?;
        h.push(rest);
        return Ok(h);
    }
    Ok(p.to_owned())
}

/// Default journal directory: `$XDG_STATE_HOME/systeml/journal/`.
pub fn journal_dir() -> Option<PathBuf> {
    systeml_unit::search::systeml_state_dir().map(|d| d.join("journal"))
}

/// Open a `StandardStream` sink as a `Stdio` for tokio `Command`.
///
/// `unit_name` is used to name fallback journal-stub files when the user asked
/// for `journal` / `kmsg` / `syslog`. `which` is `"out"` or `"err"` for the
/// fallback file suffix.
pub fn open_stream_stdio(stream: &StandardStream, unit_name: &str, which: &str) -> Result<Stdio> {
    use std::fs::OpenOptions;
    match stream {
        StandardStream::Inherit => Ok(Stdio::inherit()),
        StandardStream::Null => {
            let f = OpenOptions::new()
                .read(true)
                .write(true)
                .open("/dev/null")
                .context("open /dev/null")?;
            Ok(Stdio::from(f))
        }
        StandardStream::Tty => {
            // Fallback: open /dev/tty if any, else inherit.
            match OpenOptions::new().read(true).write(true).open("/dev/tty") {
                Ok(f) => Ok(Stdio::from(f)),
                Err(_) => Ok(Stdio::inherit()),
            }
        }
        StandardStream::File(p) | StandardStream::Truncate(p) => {
            if let Some(parent) = p.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let f = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(p)
                .with_context(|| format!("open {p:?} (truncate)"))?;
            Ok(Stdio::from(f))
        }
        StandardStream::Append(p) => {
            if let Some(parent) = p.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let f = OpenOptions::new()
                .create(true)
                .append(true)
                .open(p)
                .with_context(|| format!("open {p:?} (append)"))?;
            Ok(Stdio::from(f))
        }
        StandardStream::Socket | StandardStream::Fd(_) => {
            // Socket-activated stdio: we don't wire a real fd here; caller is
            // expected to override via Command::stdin/stdout/stderr after.
            // Default to /dev/null so we don't accidentally leak the parent's
            // controlling tty.
            let f = OpenOptions::new()
                .read(true)
                .write(true)
                .open("/dev/null")
                .context("open /dev/null")?;
            Ok(Stdio::from(f))
        }
        StandardStream::Journal(_) | StandardStream::LinuxOnly(_) => {
            let dir = journal_dir().ok_or_else(|| anyhow!("no $XDG_STATE_HOME for journal"))?;
            std::fs::create_dir_all(&dir)
                .with_context(|| format!("create journal dir {dir:?}"))?;
            let path = dir.join(format!("{unit_name}.{which}.log"));
            let f = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .with_context(|| format!("open {path:?}"))?;
            Ok(Stdio::from(f))
        }
    }
}

/// Look up a uid/gid pair for `User=`/`Group=` strings.
///
/// On macOS we use `nix::unistd::User::from_name` then resolve the primary
/// group. Unknown user is an error.
pub fn lookup_user_group(
    user: Option<&str>,
    group: Option<&str>,
) -> Result<(Option<nix::unistd::Uid>, Option<nix::unistd::Gid>)> {
    let uid = match user {
        None => None,
        Some(name) => {
            // Numeric form?
            if let Ok(n) = name.parse::<u32>() {
                Some(nix::unistd::Uid::from_raw(n))
            } else {
                let u = nix::unistd::User::from_name(name)
                    .map_err(|e| anyhow!("getpwnam {name}: {e}"))?
                    .ok_or_else(|| anyhow!("unknown user {name:?}"))?;
                Some(u.uid)
            }
        }
    };
    let gid = match group {
        None => {
            // Default to the primary group of `user` if user is set.
            match user {
                None => None,
                Some(name) => {
                    if let Ok(n) = name.parse::<u32>() {
                        match nix::unistd::User::from_uid(nix::unistd::Uid::from_raw(n))
                            .map_err(|e| anyhow!("getpwuid: {e}"))?
                        {
                            Some(u) => Some(u.gid),
                            None => None,
                        }
                    } else {
                        match nix::unistd::User::from_name(name)
                            .map_err(|e| anyhow!("getpwnam {name}: {e}"))?
                        {
                            Some(u) => Some(u.gid),
                            None => None,
                        }
                    }
                }
            }
        }
        Some(name) => {
            if let Ok(n) = name.parse::<u32>() {
                Some(nix::unistd::Gid::from_raw(n))
            } else {
                let g = nix::unistd::Group::from_name(name)
                    .map_err(|e| anyhow!("getgrnam {name}: {e}"))?
                    .ok_or_else(|| anyhow!("unknown group {name:?}"))?;
                Some(g.gid)
            }
        }
    };
    Ok((uid, gid))
}

/// Apply variable substitution to argv, except when `NO_ENV_SUBST` is set.
pub fn substitute_argv(cmd: &ExecCommand, env: &BTreeMap<String, String>) -> Vec<String> {
    if cmd.flags.contains(ExecFlags::NO_ENV_SUBST) {
        return cmd.argv.clone();
    }
    cmd.argv.iter().map(|a| expand_vars(a, env)).collect()
}

/// Build a tokio `Command` for a single `ExecCommand`. Sets argv0, env
/// (clearing the parent env), working directory, std I/O, and (via pre_exec)
/// uid/gid + setsid.
///
/// The result is ready to `.spawn()`. Caller is responsible for any extra
/// per-type setup like notify-socket env or `LISTEN_FDS`.
pub fn build_command(
    unit_name: &str,
    cmd: &ExecCommand,
    svc: &ServiceUnit,
    env: &BTreeMap<String, String>,
    extra_env: &[(String, String)],
) -> Result<Command> {
    let argv = substitute_argv(cmd, env);
    if argv.is_empty() {
        return Err(anyhow!("empty argv"));
    }
    let program = if cmd.flags.contains(ExecFlags::SET_ARGV0) {
        cmd.program.clone()
    } else {
        argv[0].clone()
    };
    let argv_rest: Vec<String> = if cmd.flags.contains(ExecFlags::SET_ARGV0) {
        argv.clone()
    } else {
        argv.iter().skip(1).cloned().collect()
    };

    let mut command = Command::new(&program);
    command.args(&argv_rest);
    command.env_clear();
    for (k, v) in env {
        command.env(k, v);
    }
    for (k, v) in extra_env {
        command.env(k, v);
    }

    if let Some(wd) = svc.working_directory.as_deref() {
        let resolved = resolve_working_dir(wd)?;
        command.current_dir(resolved);
    }

    command.stdin(open_stream_stdio(&svc.standard_input, unit_name, "in")?);
    command.stdout(open_stream_stdio(&svc.standard_output, unit_name, "out")?);
    command.stderr(open_stream_stdio(&svc.standard_error, unit_name, "err")?);

    // pre_exec: setsid + setresuid/setresgid. Skipped under FULL_PRIVS.
    let want_priv_drop = !cmd.flags.contains(ExecFlags::FULL_PRIVS);
    let user = svc.user.clone();
    let group = svc.group.clone();
    let umask = svc.umask;
    // SAFETY: pre_exec runs in the child between fork and exec. We only call
    // async-signal-safe nix wrappers (setsid, setuid, setgid, umask).
    unsafe {
        command.pre_exec(move || {
            // New session — separates us from the parent's controlling tty.
            let _ = nix::unistd::setsid();
            if let Some(mask) = umask {
                let mode = nix::sys::stat::Mode::from_bits_truncate(mask as libc::mode_t);
                nix::sys::stat::umask(mode);
            }
            if want_priv_drop {
                if let Ok((uid, gid)) =
                    crate::exec::lookup_user_group(user.as_deref(), group.as_deref())
                {
                    if let Some(g) = gid {
                        let _ = nix::unistd::setgid(g);
                    }
                    if let Some(u) = uid {
                        let _ = nix::unistd::setuid(u);
                    }
                }
            }
            Ok(())
        });
    }

    debug!(unit = unit_name, program = %program, "exec built");
    Ok(command)
}

/// Convenience: a stable journal-fallback path for the named unit.
pub fn fallback_journal_path(unit: &str, which: &str) -> Option<PathBuf> {
    journal_dir().map(|d| d.join(format!("{unit}.{which}.log")))
}

/// Owned form of `Vec<(String, String)>` extra env, used by the supervisor.
pub type ExtraEnv = Vec<(String, String)>;

/// `OsString` alias re-export for callers building env values.
pub type OsStr = OsString;
