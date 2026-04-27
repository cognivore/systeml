//! Low-level INI parser for systemd unit files.
//!
//! Recognises:
//! - `# ...` and `; ...` comments (only at start of trimmed line).
//! - `[Section]` headers.
//! - `Key=Value` lines.
//! - Trailing `\` line continuation (continued line is concatenated with a
//!   single space).
//! - Multiple `Key=Value` lines accumulate into a list of values per key.
//! - Empty `Key=` resets the accumulated list to empty (systemd convention).
//!
//! Returns the parsed structure as a list of `(section, [(key, value, line)])`.
//! The caller is responsible for typed interpretation per directive.

use crate::error::ParseError;
use indexmap::IndexMap;
use std::path::Path;

/// One key=value line with its source line number.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Directive key (e.g. `ExecStart`).
    pub key: String,
    /// Raw value, line continuations folded.
    pub value: String,
    /// 1-indexed source line of the first character of the directive.
    pub line: u32,
}

/// Parsed INI file: ordered sections, each an ordered list of entries.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Ini {
    /// Section name -> entries. Insertion order preserved.
    pub sections: IndexMap<String, Vec<Entry>>,
}

impl Ini {
    /// Look up the last (or only) value for a key in a section.
    #[must_use]
    pub fn last(&self, section: &str, key: &str) -> Option<&str> {
        self.sections
            .get(section)?
            .iter()
            .rev()
            .find(|e| e.key == key)
            .map(|e| e.value.as_str())
    }

    /// Iterate all entries with the given key in declaration order.
    pub fn all<'a>(
        &'a self,
        section: &'a str,
        key: &'a str,
    ) -> impl Iterator<Item = &'a Entry> + 'a {
        self.sections
            .get(section)
            .into_iter()
            .flatten()
            .filter(move |e| e.key == key)
    }

    /// Iterate entries respecting the "empty value resets list" convention:
    /// returns the list of *effective* values for `key` after applying every
    /// reset.
    pub fn effective_list(&self, section: &str, key: &str) -> Vec<&Entry> {
        let mut out: Vec<&Entry> = Vec::new();
        if let Some(entries) = self.sections.get(section) {
            for e in entries.iter().filter(|e| e.key == key) {
                if e.value.trim().is_empty() {
                    out.clear();
                } else {
                    out.push(e);
                }
            }
        }
        out
    }
}

/// Parse the contents of a unit file from a string. `path` is purely for
/// error reporting.
pub fn parse(path: &Path, source: &str) -> Result<Ini, ParseError> {
    let mut ini = Ini::default();
    let mut current_section: Option<String> = None;

    let lines: Vec<&str> = source.split('\n').collect();
    let mut i = 0usize;
    while i < lines.len() {
        let lineno = (i as u32) + 1;
        let mut line = lines[i].to_owned();
        // Strip trailing CR.
        if line.ends_with('\r') {
            line.pop();
        }

        // Line continuation: trailing `\` (after whitespace stripped from end).
        let mut accum = line;
        while accum.trim_end().ends_with('\\') {
            // Drop the trailing backslash.
            let trimmed = accum.trim_end();
            let without_bs = &trimmed[..trimmed.len() - 1];
            accum = without_bs.to_owned();
            i += 1;
            if i >= lines.len() {
                return Err(ParseError::Syntax {
                    path: path.to_owned(),
                    line: lineno,
                    message: "trailing backslash with no continuation".into(),
                });
            }
            let mut nxt = lines[i].to_owned();
            if nxt.ends_with('\r') {
                nxt.pop();
            }
            accum.push(' ');
            accum.push_str(nxt.trim_start());
        }

        let trimmed = accum.trim();
        i += 1;

        // Skip blanks and comments.
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with(';') {
            continue;
        }

        // Section header.
        if let Some(rest) = trimmed.strip_prefix('[') {
            let name = rest.strip_suffix(']').ok_or_else(|| ParseError::Syntax {
                path: path.to_owned(),
                line: lineno,
                message: "unterminated section header".into(),
            })?;
            let name = name.trim().to_owned();
            current_section = Some(name.clone());
            ini.sections.entry(name).or_default();
            continue;
        }

        // Directive.
        let section = current_section.as_deref().ok_or_else(|| ParseError::Syntax {
            path: path.to_owned(),
            line: lineno,
            message: "directive outside any section".into(),
        })?;
        let (key, value) = trimmed.split_once('=').ok_or_else(|| ParseError::Syntax {
            path: path.to_owned(),
            line: lineno,
            message: "expected Key=Value".into(),
        })?;
        let key = key.trim().to_owned();
        let value = value.trim().to_owned();
        ini.sections
            .entry(section.to_owned())
            .or_default()
            .push(Entry {
                key,
                value,
                line: lineno,
            });
    }
    Ok(ini)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn parses_simple() {
        let src = "[Unit]\nDescription=hi\n\n[Service]\nExecStart=/bin/true\n";
        let i = parse(&PathBuf::from("x"), src).unwrap();
        assert_eq!(i.last("Unit", "Description"), Some("hi"));
        assert_eq!(i.last("Service", "ExecStart"), Some("/bin/true"));
    }

    #[test]
    fn comments_skipped() {
        let src = "# top\n[Unit]\n; comment\nDescription=hi\n";
        let i = parse(&PathBuf::from("x"), src).unwrap();
        assert_eq!(i.last("Unit", "Description"), Some("hi"));
    }

    #[test]
    fn line_continuation() {
        let src = "[Service]\nExecStart=/bin/echo \\\n  hello \\\n  world\n";
        let i = parse(&PathBuf::from("x"), src).unwrap();
        assert_eq!(
            i.last("Service", "ExecStart"),
            Some("/bin/echo  hello  world")
        );
    }

    #[test]
    fn empty_resets_list() {
        let src = "[Service]\nEnvironment=A=1\nEnvironment=B=2\nEnvironment=\nEnvironment=C=3\n";
        let i = parse(&PathBuf::from("x"), src).unwrap();
        let eff = i.effective_list("Service", "Environment");
        assert_eq!(eff.len(), 1);
        assert_eq!(eff[0].value, "C=3");
    }

    #[test]
    fn errors_on_directive_before_section() {
        let src = "Description=hi\n";
        assert!(parse(&PathBuf::from("x"), src).is_err());
    }

    #[test]
    fn errors_on_unterminated_section() {
        let src = "[Unit\n";
        assert!(parse(&PathBuf::from("x"), src).is_err());
    }
}
