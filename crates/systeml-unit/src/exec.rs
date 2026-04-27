//! ExecStart= / ExecStop= / etc. parsing.
//!
//! Format: optional flag chars (`-`, `+`, `:`, `!`, `!!`, `@`) followed by an
//! argv. Argv is shell-like: whitespace-separated, supports `'...'` and
//! `"..."` quoting and `\` escapes. systemd allows `${VAR}` substitution; we
//! capture the raw form and defer expansion to runtime.

use bitflags::bitflags;
use std::fmt;

bitflags! {
    /// Per-`Exec*=` line flags from leading characters.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
    #[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
    pub struct ExecFlags: u8 {
        /// `-` — failure of the process is ignored.
        const IGNORE_FAILURE = 0b0000_0001;
        /// `+` — run with full privileges (skip User=, Group=, NoNewPrivileges=).
        const FULL_PRIVS = 0b0000_0010;
        /// `!` — run as user but skip namespace/seccomp setup.
        const RAISE_PRIV = 0b0000_0100;
        /// `!!` — like `!` but no-op on systems without ambient caps.
        const AMBIENT_RAISE = 0b0000_1000;
        /// `:` — disable variable substitution in this command.
        const NO_ENV_SUBST = 0b0001_0000;
        /// `@` — set argv[0] explicitly (next token after path).
        const SET_ARGV0 = 0b0010_0000;
    }
}

/// One `ExecStart=` (or sibling) line.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ExecCommand {
    /// Original raw value as written.
    pub raw: String,
    /// Parsed argv. `argv[0]` is the program path unless `SET_ARGV0` is set,
    /// in which case `argv[0]` is the explicit argv[0] and `program` holds
    /// the path.
    pub argv: Vec<String>,
    /// The actual binary to invoke. Same as `argv[0]` unless `SET_ARGV0`.
    pub program: String,
    /// Flags from leading chars.
    pub flags: ExecFlags,
}

impl ExecCommand {
    /// Parse a single `ExecStart=` value.
    ///
    /// # Errors
    /// Returns a string describing why parsing failed (e.g. unterminated quote).
    pub fn parse(raw: &str) -> Result<Self, String> {
        let raw = raw.trim();
        if raw.is_empty() {
            return Err("empty exec line".into());
        }
        let (flags, rest) = strip_flags(raw);
        let argv = shell_split(rest)?;
        if argv.is_empty() {
            return Err("no command after flags".into());
        }
        let (program, argv) = if flags.contains(ExecFlags::SET_ARGV0) {
            // form: `@/path/to/bin argv0 args...`
            // After @ flag, the next token is the path, then the rest is argv.
            let mut it = argv.into_iter();
            let path = it.next().ok_or("missing path after @")?;
            let new_argv: Vec<String> = it.collect();
            if new_argv.is_empty() {
                return Err("@flag requires explicit argv[0]".into());
            }
            (path, new_argv)
        } else {
            (argv[0].clone(), argv)
        };
        Ok(Self {
            raw: raw.to_owned(),
            argv,
            program,
            flags,
        })
    }
}

impl fmt::Display for ExecCommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.raw)
    }
}

fn strip_flags(s: &str) -> (ExecFlags, &str) {
    let mut flags = ExecFlags::empty();
    let mut rest = s;
    loop {
        // `!!` must be matched before `!`.
        if let Some(r) = rest.strip_prefix("!!") {
            flags |= ExecFlags::AMBIENT_RAISE;
            rest = r;
        } else if let Some(r) = rest.strip_prefix('!') {
            flags |= ExecFlags::RAISE_PRIV;
            rest = r;
        } else if let Some(r) = rest.strip_prefix('-') {
            flags |= ExecFlags::IGNORE_FAILURE;
            rest = r;
        } else if let Some(r) = rest.strip_prefix('+') {
            flags |= ExecFlags::FULL_PRIVS;
            rest = r;
        } else if let Some(r) = rest.strip_prefix(':') {
            flags |= ExecFlags::NO_ENV_SUBST;
            rest = r;
        } else if let Some(r) = rest.strip_prefix('@') {
            flags |= ExecFlags::SET_ARGV0;
            rest = r;
        } else {
            break;
        }
        rest = rest.trim_start();
    }
    (flags, rest)
}

/// Shell-style argv split with `'...'` (literal) and `"..."` (escapes) quoting.
pub fn shell_split(s: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_token = false;
    let mut chars = s.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            ' ' | '\t' if !in_token => {}
            ' ' | '\t' => {
                out.push(std::mem::take(&mut cur));
                in_token = false;
            }
            '\\' => {
                let n = chars.next().ok_or("trailing backslash")?;
                cur.push(n);
                in_token = true;
            }
            '\'' => {
                in_token = true;
                while let Some(c) = chars.next() {
                    if c == '\'' {
                        break;
                    }
                    cur.push(c);
                }
            }
            '"' => {
                in_token = true;
                while let Some(c) = chars.next() {
                    match c {
                        '"' => break,
                        '\\' => {
                            let n = chars.next().ok_or("trailing backslash in \"\"")?;
                            match n {
                                '\\' => cur.push('\\'),
                                '"' => cur.push('"'),
                                'n' => cur.push('\n'),
                                't' => cur.push('\t'),
                                'r' => cur.push('\r'),
                                _ => {
                                    cur.push('\\');
                                    cur.push(n);
                                }
                            }
                        }
                        _ => cur.push(c),
                    }
                }
            }
            _ => {
                cur.push(c);
                in_token = true;
            }
        }
    }
    if in_token {
        out.push(cur);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain() {
        let e = ExecCommand::parse("/bin/echo hello world").unwrap();
        assert_eq!(e.program, "/bin/echo");
        assert_eq!(e.argv, vec!["/bin/echo", "hello", "world"]);
        assert!(e.flags.is_empty());
    }

    #[test]
    fn ignore_failure_flag() {
        let e = ExecCommand::parse("-/bin/false").unwrap();
        assert!(e.flags.contains(ExecFlags::IGNORE_FAILURE));
    }

    #[test]
    fn quoted() {
        let e = ExecCommand::parse(r#"/usr/bin/env sh -c "echo 'hi there'""#).unwrap();
        assert_eq!(e.argv.last().unwrap(), "echo 'hi there'");
    }

    #[test]
    fn set_argv0() {
        let e = ExecCommand::parse("@/usr/bin/sh login -l").unwrap();
        assert!(e.flags.contains(ExecFlags::SET_ARGV0));
        assert_eq!(e.program, "/usr/bin/sh");
        assert_eq!(e.argv, vec!["login", "-l"]);
    }

    #[test]
    fn double_bang() {
        let e = ExecCommand::parse("!!/foo").unwrap();
        assert!(e.flags.contains(ExecFlags::AMBIENT_RAISE));
        assert!(!e.flags.contains(ExecFlags::RAISE_PRIV));
    }

    #[test]
    fn flag_then_dash() {
        let e = ExecCommand::parse("-+/foo").unwrap();
        assert!(e.flags.contains(ExecFlags::IGNORE_FAILURE));
        assert!(e.flags.contains(ExecFlags::FULL_PRIVS));
    }
}
