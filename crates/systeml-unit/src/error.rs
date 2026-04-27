//! Parse errors and structured warnings.

use std::path::PathBuf;
use thiserror::Error;

/// A hard error that prevents a unit file from being loaded.
#[derive(Debug, Error)]
pub enum ParseError {
    /// The file could not be read.
    #[error("cannot read {path}: {source}")]
    Io {
        /// The path that failed to read.
        path: PathBuf,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// The unit name cannot be parsed (bad suffix, empty stem, illegal char).
    #[error("invalid unit name {name:?}: {reason}")]
    InvalidUnitName {
        /// The offending name.
        name: String,
        /// Why it's invalid.
        reason: &'static str,
    },
    /// A directive value failed to parse.
    #[error("{path}:{line}: {section}: {key}={value:?}: {reason}")]
    BadDirective {
        /// File this came from.
        path: PathBuf,
        /// Line number.
        line: u32,
        /// Section name (e.g. "Service").
        section: String,
        /// Directive key.
        key: String,
        /// Raw value.
        value: String,
        /// Human-readable reason.
        reason: String,
    },
    /// Syntactic error in the INI structure (unterminated section, etc).
    #[error("{path}:{line}: {message}")]
    Syntax {
        /// File.
        path: PathBuf,
        /// Line.
        line: u32,
        /// Message.
        message: String,
    },
    /// A required directive was missing.
    #[error("{path}: [{section}] {key} is required for {kind} units")]
    Missing {
        /// File.
        path: PathBuf,
        /// Section.
        section: &'static str,
        /// Key.
        key: &'static str,
        /// Unit kind name, e.g. "service".
        kind: &'static str,
    },
}

/// A non-fatal warning emitted during parsing.
///
/// Examples: a Linux-kernel-only directive (`PrivateNetwork=`), an unknown
/// directive name, a deprecated form. The unit still loads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseWarning {
    /// File this came from, or `None` if parsed from a string.
    pub path: Option<PathBuf>,
    /// Line number, 1-indexed.
    pub line: u32,
    /// Category for filtering / metrics.
    pub kind: WarningKind,
    /// Human-readable message.
    pub message: String,
}

/// Categorisation for parse warnings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WarningKind {
    /// Directive is parsed but ignored — Linux-kernel-only feature.
    LinuxOnly,
    /// Directive is unknown to SystemL.
    Unknown,
    /// Directive is deprecated.
    Deprecated,
    /// Value parsed but is malformed in a recoverable way (e.g. extra tokens
    /// after a duration).
    Recoverable,
}

impl ParseWarning {
    /// Construct a Linux-only warning.
    pub fn linux_only(
        path: Option<PathBuf>,
        line: u32,
        section: &str,
        key: &str,
    ) -> Self {
        Self {
            path,
            line,
            kind: WarningKind::LinuxOnly,
            message: format!(
                "[{section}] {key}= is a Linux-kernel-only directive; \
                 parsed but not enforced on macOS"
            ),
        }
    }

    /// Construct an unknown-directive warning.
    pub fn unknown(
        path: Option<PathBuf>,
        line: u32,
        section: &str,
        key: &str,
    ) -> Self {
        Self {
            path,
            line,
            kind: WarningKind::Unknown,
            message: format!("[{section}] unknown directive {key}="),
        }
    }
}
