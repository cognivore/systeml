//! Unit names: parsing, display, instance handling.
//!
//! systemd unit names look like:
//! - `foo.service` — plain unit
//! - `foo@bar.service` — instantiated template
//! - `foo@.service` — template (uninstantiated)
//! - `dev-disk-by\x2dlabel-foo.device` — escaped path-like name
//!
//! See `man systemd.unit` "Unit Names" section.

use crate::error::ParseError;
use std::fmt;
use std::path::Path;
use std::str::FromStr;

/// Suffix-typed unit kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum UnitKind {
    /// `.service`
    Service,
    /// `.socket`
    Socket,
    /// `.timer`
    Timer,
    /// `.path`
    Path,
    /// `.target`
    Target,
    /// `.mount`
    Mount,
    /// `.automount`
    Automount,
    /// `.swap`
    Swap,
    /// `.device`
    Device,
    /// `.slice`
    Slice,
    /// `.scope`
    Scope,
}

impl UnitKind {
    /// Suffix including the leading `.`.
    #[must_use]
    pub const fn suffix(self) -> &'static str {
        match self {
            Self::Service => ".service",
            Self::Socket => ".socket",
            Self::Timer => ".timer",
            Self::Path => ".path",
            Self::Target => ".target",
            Self::Mount => ".mount",
            Self::Automount => ".automount",
            Self::Swap => ".swap",
            Self::Device => ".device",
            Self::Slice => ".slice",
            Self::Scope => ".scope",
        }
    }

    /// All known suffixes, longest-first for greedy matching.
    pub const ALL: [Self; 11] = [
        Self::Automount,
        Self::Service,
        Self::Socket,
        Self::Target,
        Self::Device,
        Self::Mount,
        Self::Scope,
        Self::Slice,
        Self::Timer,
        Self::Path,
        Self::Swap,
    ];

    /// Parse a suffix (without leading `.` or with).
    pub fn from_suffix(s: &str) -> Option<Self> {
        let s = s.strip_prefix('.').unwrap_or(s);
        Self::ALL
            .into_iter()
            .find(|k| k.suffix().trim_start_matches('.') == s)
    }

    /// Whether this unit kind is meaningful in user-mode on macOS.
    /// (Mount/automount/swap/device/slice are Linux-only; we parse them but
    /// won't activate.)
    #[must_use]
    pub const fn supported(self) -> bool {
        matches!(
            self,
            Self::Service
                | Self::Socket
                | Self::Timer
                | Self::Path
                | Self::Target
                | Self::Scope
        )
    }
}

impl fmt::Display for UnitKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.suffix().trim_start_matches('.'))
    }
}

/// A parsed unit name with optional template instance.
///
/// Display roundtrips: `foo@bar.service` parses then formats back identically.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct UnitName {
    /// Stem before any `@` or suffix. e.g. `foo` in `foo@bar.service`.
    pub prefix: String,
    /// Instance name between `@` and `.`. `None` if no `@`.
    /// `Some("")` means a bare template (`foo@.service`).
    pub instance: Option<String>,
    /// Unit kind from suffix.
    pub kind: UnitKind,
}

impl UnitName {
    /// Construct a non-template unit name.
    pub fn plain(prefix: impl Into<String>, kind: UnitKind) -> Self {
        Self {
            prefix: prefix.into(),
            instance: None,
            kind,
        }
    }

    /// Construct a template (uninstantiated) unit name.
    pub fn template(prefix: impl Into<String>, kind: UnitKind) -> Self {
        Self {
            prefix: prefix.into(),
            instance: Some(String::new()),
            kind,
        }
    }

    /// Construct an instantiated template.
    pub fn instance(
        prefix: impl Into<String>,
        instance: impl Into<String>,
        kind: UnitKind,
    ) -> Self {
        Self {
            prefix: prefix.into(),
            instance: Some(instance.into()),
            kind,
        }
    }

    /// True if this name is a template (`foo@.service`).
    #[must_use]
    pub fn is_template(&self) -> bool {
        matches!(&self.instance, Some(s) if s.is_empty())
    }

    /// True if this is an instantiated template (`foo@bar.service`).
    #[must_use]
    pub fn is_instance(&self) -> bool {
        matches!(&self.instance, Some(s) if !s.is_empty())
    }

    /// True if neither template nor instance — a plain unit.
    #[must_use]
    pub fn is_plain(&self) -> bool {
        self.instance.is_none()
    }

    /// The template form of an instance: `foo@bar.service` -> `foo@.service`.
    /// For plain or template names, returns `None`.
    #[must_use]
    pub fn template_form(&self) -> Option<UnitName> {
        if self.is_instance() {
            Some(UnitName::template(&self.prefix, self.kind))
        } else {
            None
        }
    }

    /// Filename on disk: `foo.service`, `foo@bar.service`, `foo@.service`.
    #[must_use]
    pub fn filename(&self) -> String {
        self.to_string()
    }

    /// Try to read a unit name from a file system path's basename.
    pub fn from_path(p: &Path) -> Result<Self, ParseError> {
        let name = p
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or_else(|| ParseError::InvalidUnitName {
                name: p.display().to_string(),
                reason: "non-utf8 filename",
            })?;
        Self::from_str(name)
    }
}

impl fmt::Display for UnitName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.instance {
            None => write!(f, "{}{}", self.prefix, self.kind.suffix()),
            Some(inst) => write!(
                f,
                "{}@{}{}",
                self.prefix,
                inst,
                self.kind.suffix()
            ),
        }
    }
}

impl FromStr for UnitName {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (kind, stem) = split_kind(s).ok_or_else(|| ParseError::InvalidUnitName {
            name: s.to_owned(),
            reason: "no recognised unit suffix",
        })?;
        if stem.is_empty() {
            return Err(ParseError::InvalidUnitName {
                name: s.to_owned(),
                reason: "empty unit prefix",
            });
        }
        validate_chars(stem).map_err(|reason| ParseError::InvalidUnitName {
            name: s.to_owned(),
            reason,
        })?;

        let (prefix, instance) = match stem.split_once('@') {
            Some((p, i)) => (p.to_owned(), Some(i.to_owned())),
            None => (stem.to_owned(), None),
        };
        if prefix.is_empty() {
            return Err(ParseError::InvalidUnitName {
                name: s.to_owned(),
                reason: "empty prefix before '@'",
            });
        }
        Ok(Self {
            prefix,
            instance,
            kind,
        })
    }
}

fn split_kind(s: &str) -> Option<(UnitKind, &str)> {
    for k in UnitKind::ALL {
        if let Some(stem) = s.strip_suffix(k.suffix()) {
            return Some((k, stem));
        }
    }
    None
}

fn validate_chars(s: &str) -> Result<(), &'static str> {
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            continue;
        }
        match c {
            '@' | '-' | '_' | '.' | '\\' | ':' | '\u{0}'..='\u{1f}' | '\u{7f}' => {}
            _ => {
                if c.is_ascii() && !c.is_ascii_control() && !c.is_whitespace() {
                    // permit other printable ASCII (systemd is liberal here)
                    continue;
                }
            }
        }
        if c.is_whitespace() {
            return Err("unit name contains whitespace");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain() {
        let n: UnitName = "foo.service".parse().unwrap();
        assert_eq!(n.prefix, "foo");
        assert_eq!(n.instance, None);
        assert_eq!(n.kind, UnitKind::Service);
        assert_eq!(n.to_string(), "foo.service");
    }

    #[test]
    fn parses_template() {
        let n: UnitName = "foo@.service".parse().unwrap();
        assert!(n.is_template());
        assert_eq!(n.to_string(), "foo@.service");
    }

    #[test]
    fn parses_instance() {
        let n: UnitName = "foo@bar.service".parse().unwrap();
        assert!(n.is_instance());
        assert_eq!(n.prefix, "foo");
        assert_eq!(n.instance.as_deref(), Some("bar"));
        assert_eq!(n.template_form().unwrap().to_string(), "foo@.service");
    }

    #[test]
    fn rejects_unknown_suffix() {
        assert!("foo.bogus".parse::<UnitName>().is_err());
        assert!("noprefix".parse::<UnitName>().is_err());
    }

    #[test]
    fn all_kinds_roundtrip() {
        for k in UnitKind::ALL {
            let n = UnitName::plain("foo", k);
            assert_eq!(n.to_string().parse::<UnitName>().unwrap(), n);
        }
    }
}
