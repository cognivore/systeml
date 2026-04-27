//! Environment= / EnvironmentFile= parsing.

use std::path::PathBuf;

/// Result of parsing one `Environment=` line.
///
/// Multiple `Environment=` directives accumulate; later ones override earlier
/// keys. Format is `KEY=VALUE` whitespace-separated, with `'...'` or `"..."`
/// quoting around values that contain spaces.
pub fn parse_environment_line(s: &str) -> Result<Vec<(String, String)>, String> {
    let pairs = crate::exec::shell_split(s)?;
    let mut out = Vec::with_capacity(pairs.len());
    for p in pairs {
        let (k, v) = p
            .split_once('=')
            .ok_or_else(|| format!("missing = in environment entry {p:?}"))?;
        if k.is_empty() {
            return Err(format!("empty key in environment entry {p:?}"))
        }
        out.push((k.to_owned(), v.to_owned()));
    }
    Ok(out)
}

/// `EnvironmentFile=`. Leading `-` makes load failure non-fatal.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct EnvironmentFile {
    /// Path to read.
    pub path: PathBuf,
    /// If true, missing file is OK.
    pub optional: bool,
}

impl EnvironmentFile {
    /// Parse `[-]/path/to/env`.
    pub fn parse(s: &str) -> Self {
        let s = s.trim();
        if let Some(rest) = s.strip_prefix('-') {
            Self {
                path: PathBuf::from(rest),
                optional: true,
            }
        } else {
            Self {
                path: PathBuf::from(s),
                optional: false,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pair() {
        let v = parse_environment_line("FOO=1 BAR=baz").unwrap();
        assert_eq!(v, vec![("FOO".into(), "1".into()), ("BAR".into(), "baz".into())]);
    }

    #[test]
    fn quoted_value_with_space() {
        let v = parse_environment_line(r#"MSG="hello world""#).unwrap();
        assert_eq!(v, vec![("MSG".into(), "hello world".into())]);
    }

    #[test]
    fn env_file_optional() {
        let f = EnvironmentFile::parse("-/etc/foo.env");
        assert!(f.optional);
        assert_eq!(f.path, PathBuf::from("/etc/foo.env"));
    }
}
