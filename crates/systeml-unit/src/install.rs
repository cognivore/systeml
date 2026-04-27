//! `[Install]` section.

use crate::name::UnitName;
use std::collections::BTreeSet;

/// Contents of `[Install]`. Used by `systemlctl enable/disable` to manage
/// `~/.config/systemd/user/{wants,requires,upholds}/{target}/{unit}` symlinks.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Install {
    /// `WantedBy=` — creates `<target>.wants/<unit>` symlinks.
    pub wanted_by: BTreeSet<UnitName>,
    /// `RequiredBy=`.
    pub required_by: BTreeSet<UnitName>,
    /// `UpheldBy=`.
    pub upheld_by: BTreeSet<UnitName>,
    /// `Also=` — units that should be enabled/disabled together.
    pub also: BTreeSet<UnitName>,
    /// `Alias=` — alternate names for this unit.
    pub alias: BTreeSet<UnitName>,
    /// `DefaultInstance=` — instance for templates when none is given.
    pub default_instance: Option<String>,
}

impl Install {
    /// True if there is anything in this section that drives enable/disable.
    pub fn is_empty(&self) -> bool {
        self.wanted_by.is_empty()
            && self.required_by.is_empty()
            && self.upheld_by.is_empty()
            && self.also.is_empty()
            && self.alias.is_empty()
            && self.default_instance.is_none()
    }
}
