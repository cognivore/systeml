//! `[Install]` symlink management — implements `enable`/`disable`/`mask`/
//! `unmask` and `is-enabled` introspection.
//!
//! Layout (mirrors systemd-stable user-mode):
//! ```text
//! $XDG_CONFIG_HOME/systemd/user/
//!     <wanted-target>.wants/<unit>     -> link to fragment
//!     <required-target>.requires/<unit> -> link to fragment
//!     <unit>                            -> /dev/null  (mask)
//! ```

use crate::manager::{EnableChanges, UnitFileChange};
use anyhow::{anyhow, Context, Result};
use std::path::{Path, PathBuf};
use systeml_unit::install::Install;
use systeml_unit::search::user_search_paths;
use systeml_unit::UnitName;
use tracing::warn;

/// Where on disk we drop `enable` symlinks.
///
/// systemd uses `$XDG_CONFIG_HOME/systemd/user`; on macOS, `dirs::config_dir`
/// resolves to `~/Library/Application Support`, which is wrong. Prefer the
/// XDG var if set; otherwise fall back to `~/.config/systemd/user`.
pub fn config_dir() -> Result<PathBuf> {
    if let Some(x) = std::env::var_os("XDG_CONFIG_HOME") {
        return Ok(PathBuf::from(x).join("systemd/user"));
    }
    if let Some(home) = dirs::home_dir() {
        return Ok(home.join(".config/systemd/user"));
    }
    Err(anyhow!("no XDG_CONFIG_HOME or HOME for symlinks"))
}

/// Walk the search path for the given unit, return the first fragment path.
pub fn locate_fragment(name: &UnitName) -> Option<PathBuf> {
    let want = name.filename();
    for base in user_search_paths() {
        let p = base.join(&want);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

/// Create the appropriate `.wants/`, `.requires/`, `.upholds/` symlinks for
/// the given unit's `[Install]` block. Returns the on-disk changes performed.
pub fn enable(
    name: &UnitName,
    install: &Install,
    runtime: bool,
    force: bool,
) -> Result<EnableChanges> {
    let mut out = EnableChanges {
        carries_install_info: !install.is_empty(),
        ..Default::default()
    };
    if !out.carries_install_info {
        return Ok(out);
    }
    let base = if runtime {
        // Runtime drops live in /run/user/$UID/systemd/user when present.
        // Fall back to config dir on macOS where we don't have a runtime
        // tree by default.
        match systeml_unit::search::runtime_dir() {
            Some(rt) => rt.join("systemd/user"),
            None => config_dir()?,
        }
    } else {
        config_dir()?
    };

    let fragment = locate_fragment(name);
    let source: PathBuf = match fragment {
        Some(p) => p,
        None => {
            // No fragment yet — link target points at the bare filename so a
            // later `daemon-reload` resolves it.
            PathBuf::from(name.filename())
        }
    };

    for tgt in &install.wanted_by {
        let dir = base.join(format!("{}.wants", tgt));
        link_one(&dir, &name.filename(), &source, force, &mut out)?;
    }
    for tgt in &install.required_by {
        let dir = base.join(format!("{}.requires", tgt));
        link_one(&dir, &name.filename(), &source, force, &mut out)?;
    }
    for tgt in &install.upheld_by {
        let dir = base.join(format!("{}.upholds", tgt));
        link_one(&dir, &name.filename(), &source, force, &mut out)?;
    }
    // Aliases: link <alias> -> <unit-fragment>
    for alias in &install.alias {
        let dir = base.clone();
        link_one(&dir, &alias.filename(), &source, force, &mut out)?;
    }
    Ok(out)
}

fn link_one(
    dir: &Path,
    link_name: &str,
    target: &Path,
    force: bool,
    out: &mut EnableChanges,
) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("mkdir -p {dir:?}"))?;
    let link = dir.join(link_name);
    if link.exists() || link.is_symlink() {
        if !force {
            return Ok(());
        }
        std::fs::remove_file(&link).ok();
    }
    #[allow(unsafe_code)]
    {
        // std::os::unix::fs::symlink is safe.
        std::os::unix::fs::symlink(target, &link)
            .with_context(|| format!("symlink {link:?} -> {target:?}"))?;
    }
    out.changes.push(UnitFileChange {
        change_type: "symlink".into(),
        target: link,
        source: target.to_owned(),
    });
    Ok(())
}

/// Remove all `[Install]` symlinks for the given unit. Walks every
/// `<target>.wants/`, `.requires/`, `.upholds/` directory under the config
/// dir and removes any link whose basename matches `name`.
pub fn disable(name: &UnitName, runtime: bool) -> Result<EnableChanges> {
    let mut out = EnableChanges::default();
    let base = if runtime {
        match systeml_unit::search::runtime_dir() {
            Some(rt) => rt.join("systemd/user"),
            None => config_dir()?,
        }
    } else {
        config_dir()?
    };
    if !base.exists() {
        return Ok(out);
    }
    let want = name.filename();
    let entries = match std::fs::read_dir(&base) {
        Ok(e) => e,
        Err(_) => return Ok(out),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let kind = match entry.file_type() {
            Ok(k) => k,
            Err(_) => continue,
        };
        if !kind.is_dir() {
            // Could be an alias symlink at the top level.
            if kind.is_symlink() && entry.file_name() == std::ffi::OsStr::new(&want) {
                let _ = std::fs::remove_file(&path);
                out.changes.push(UnitFileChange {
                    change_type: "unlink".into(),
                    target: path,
                    source: PathBuf::new(),
                });
            }
            continue;
        }
        let dir_name = entry.file_name();
        let dn = dir_name.to_string_lossy();
        if !(dn.ends_with(".wants") || dn.ends_with(".requires") || dn.ends_with(".upholds")) {
            continue;
        }
        let link = path.join(&want);
        if link.is_symlink() || link.exists() {
            let _ = std::fs::remove_file(&link);
            out.changes.push(UnitFileChange {
                change_type: "unlink".into(),
                target: link,
                source: PathBuf::new(),
            });
        }
    }
    Ok(out)
}

/// `mask` — link `<unit>` to `/dev/null` in the config dir.
pub fn mask(name: &UnitName, runtime: bool, force: bool) -> Result<EnableChanges> {
    let mut out = EnableChanges::default();
    let base = if runtime {
        match systeml_unit::search::runtime_dir() {
            Some(rt) => rt.join("systemd/user"),
            None => config_dir()?,
        }
    } else {
        config_dir()?
    };
    std::fs::create_dir_all(&base).with_context(|| format!("mkdir -p {base:?}"))?;
    let link = base.join(name.filename());
    if link.exists() || link.is_symlink() {
        if !force {
            return Ok(out);
        }
        let _ = std::fs::remove_file(&link);
    }
    let target = PathBuf::from("/dev/null");
    std::os::unix::fs::symlink(&target, &link)
        .with_context(|| format!("symlink {link:?} -> /dev/null"))?;
    out.changes.push(UnitFileChange {
        change_type: "symlink".into(),
        target: link,
        source: target,
    });
    Ok(out)
}

/// `unmask` — remove the `/dev/null` link if present.
pub fn unmask(name: &UnitName, runtime: bool) -> Result<EnableChanges> {
    let mut out = EnableChanges::default();
    let base = if runtime {
        match systeml_unit::search::runtime_dir() {
            Some(rt) => rt.join("systemd/user"),
            None => config_dir()?,
        }
    } else {
        config_dir()?
    };
    let link = base.join(name.filename());
    if !link.is_symlink() {
        return Ok(out);
    }
    match std::fs::read_link(&link) {
        Ok(t) if t == Path::new("/dev/null") => {
            std::fs::remove_file(&link)
                .with_context(|| format!("unlink {link:?}"))?;
            out.changes.push(UnitFileChange {
                change_type: "unlink".into(),
                target: link,
                source: PathBuf::from("/dev/null"),
            });
        }
        Ok(_) => {
            warn!("{link:?} is a symlink but not to /dev/null; leaving alone");
        }
        Err(e) => warn!("readlink {link:?}: {e}"),
    }
    Ok(out)
}

/// Compute `is-enabled` state. Mirrors `systemctl is-enabled`:
///
/// - `masked` — `<unit>` symlink to `/dev/null`.
/// - `enabled` — at least one `<target>.wants/<unit>` link in any search dir.
/// - `static` — fragment exists, has `[Install]` info, but no link present.
/// - `disabled` — fragment exists, has `[Install]` info, but no link present.
///   (We collapse `static`/`disabled` distinction into:
///   `static` = no install info; `disabled` = install info but no link.)
/// - `linked` — fragment lives outside the search path but is symlinked in.
/// - `not-found` — no fragment at all.
pub fn unit_file_state(name: &UnitName, install: Option<&Install>) -> String {
    // Masked check first.
    if let Ok(cfg) = config_dir() {
        let link = cfg.join(name.filename());
        if link.is_symlink() {
            if let Ok(t) = std::fs::read_link(&link) {
                if t == Path::new("/dev/null") {
                    return "masked".into();
                }
                // Pointed at something outside search path → linked
                if !t.starts_with("/") || !t.is_absolute() {
                    // Ignored — relative links count as linked, too.
                }
                return "linked".into();
            }
        }
    }
    // Walk every search dir for `.wants/.requires/.upholds/<name>`.
    let want = name.filename();
    for base in user_search_paths() {
        let entries = match std::fs::read_dir(&base) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let dn = match path.file_name().and_then(|s| s.to_str()) {
                Some(s) => s,
                None => continue,
            };
            if !(dn.ends_with(".wants") || dn.ends_with(".requires") || dn.ends_with(".upholds"))
            {
                continue;
            }
            if path.join(&want).is_symlink() || path.join(&want).exists() {
                return "enabled".into();
            }
        }
    }
    // No links — distinguish static vs disabled vs not-found by fragment.
    let fragment = locate_fragment(name);
    match (fragment, install) {
        (None, _) => "not-found".into(),
        (Some(_), Some(i)) if !i.is_empty() => "disabled".into(),
        (Some(_), _) => "static".into(),
    }
}

/// Whether this unit has a `<target>.wants/`, `<target>.requires/`, or
/// `<target>.upholds/` symlink in any search path. Independent of whether
/// the unit file itself is a symlink (which `unit_file_state` confounds
/// with `linked`). Used at daemon startup to decide which units to
/// auto-activate — the moral equivalent of systemd PID 1 starting
/// `default.target` and letting the transitive `wants/` closure pull
/// units in.
pub fn has_install_link(name: &UnitName) -> bool {
    let want = name.filename();
    for base in user_search_paths() {
        let entries = match std::fs::read_dir(&base) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let dn = match path.file_name().and_then(|s| s.to_str()) {
                Some(s) => s,
                None => continue,
            };
            if !(dn.ends_with(".wants") || dn.ends_with(".requires") || dn.ends_with(".upholds")) {
                continue;
            }
            let candidate = path.join(&want);
            if candidate.is_symlink() || candidate.exists() {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use systeml_unit::name::{UnitKind, UnitName};

    #[test]
    fn link_one_creates() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("default.target.wants");
        let mut out = EnableChanges::default();
        // target file doesn't have to exist for symlink to work
        let target = tmp.path().join("hello.service");
        std::fs::write(&target, "").unwrap();
        link_one(&dir, "hello.service", &target, false, &mut out).unwrap();
        let link = dir.join("hello.service");
        assert!(link.is_symlink());
    }

    /// Mutex used to serialize tests that touch `XDG_CONFIG_HOME` (process-
    /// global state shared across all parallel test threads).
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn enable_with_xdg_isolated() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("XDG_CONFIG_HOME", tmp.path());
        let name = UnitName::plain("hello", UnitKind::Service);
        let mut install = Install::default();
        install
            .wanted_by
            .insert(UnitName::plain("default", UnitKind::Target));
        let r = enable(&name, &install, false, false).unwrap();
        assert!(r.carries_install_info);
        assert!(!r.changes.is_empty());
        let link = tmp
            .path()
            .join("systemd/user/default.target.wants/hello.service");
        assert!(link.is_symlink(), "link {:?} missing", link);
        // disable
        let r2 = disable(&name, false).unwrap();
        assert!(!r2.changes.is_empty());
        assert!(!link.exists());
        std::env::remove_var("XDG_CONFIG_HOME");
        // BTreeSet is just to avoid unused import warning
        let _ = BTreeSet::<UnitName>::new();
    }

    #[test]
    fn mask_unmask_roundtrip() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("XDG_CONFIG_HOME", tmp.path());
        let name = UnitName::plain("foo", UnitKind::Service);
        let m = mask(&name, false, false).unwrap();
        assert!(!m.changes.is_empty());
        let link = tmp.path().join("systemd/user/foo.service");
        assert!(link.is_symlink());
        assert_eq!(unit_file_state(&name, None), "masked");
        let _ = unmask(&name, false).unwrap();
        assert!(!link.exists());
        std::env::remove_var("XDG_CONFIG_HOME");
    }
}
