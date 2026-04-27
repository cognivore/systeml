//! `[Path]` section. Named `path_unit` to avoid clashing with `std::path`.

use crate::duration::SdDuration;
use crate::name::UnitName;
use std::path::PathBuf;

/// One path-watch directive.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum PathWatch {
    /// `PathExists=` — fires once when path appears.
    Exists(PathBuf),
    /// `PathExistsGlob=` — fires when any glob-matching path exists.
    ExistsGlob(String),
    /// `PathChanged=` — close-after-write on file (or any change in dir).
    Changed(PathBuf),
    /// `PathModified=` — like Changed but also write-while-open.
    Modified(PathBuf),
    /// `DirectoryNotEmpty=` — fires while directory has entries.
    DirectoryNotEmpty(PathBuf),
}

impl PathWatch {
    /// The watched path, regardless of variant. (For globs, returns a synthetic.)
    #[must_use]
    pub fn path(&self) -> PathBuf {
        match self {
            Self::Exists(p)
            | Self::Changed(p)
            | Self::Modified(p)
            | Self::DirectoryNotEmpty(p) => p.clone(),
            Self::ExistsGlob(g) => PathBuf::from(g),
        }
    }
}

/// `[Path]` directives.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct PathUnit {
    /// All `Path*=` and `DirectoryNotEmpty=` directives in declaration order.
    pub watches: Vec<PathWatch>,
    /// `Unit=` — unit to start when triggered. Defaults to
    /// `<this-name>.service`.
    pub unit: Option<UnitName>,
    /// `MakeDirectory=`.
    pub make_directory: bool,
    /// `DirectoryMode=` — octal.
    pub directory_mode: Option<u32>,
    /// `TriggerLimitIntervalSec=`.
    pub trigger_limit_interval_sec: Option<SdDuration>,
    /// `TriggerLimitBurst=`.
    pub trigger_limit_burst: Option<u32>,
}
