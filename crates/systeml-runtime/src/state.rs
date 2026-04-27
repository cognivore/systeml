//! Unit runtime states. Mirrors `org.freedesktop.systemd1.Unit` properties.

use systeml_unit::UnitName;

/// `LoadState` property: where the unit data came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LoadState {
    /// Unit is loaded successfully.
    Loaded,
    /// Unit could not be parsed.
    Error,
    /// Stub state before load.
    #[default]
    Stub,
    /// Masked: `/dev/null` symlink in config.
    Masked,
    /// File not found in any search path.
    NotFound,
    /// Loaded but merged into another unit.
    Merged,
    /// Bad unit file.
    BadSetting,
}

/// `ActiveState` property: top-level lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ActiveState {
    /// Inactive.
    #[default]
    Inactive,
    /// Activating (starting up).
    Activating,
    /// Active and running.
    Active,
    /// Deactivating (shutting down).
    Deactivating,
    /// Failed.
    Failed,
    /// Reloading.
    Reloading,
    /// Maintenance state (rare).
    Maintenance,
}

/// `SubState` property: per-unit-type fine state. String for transparency
/// across upstream tools.
pub type SubState = String;

/// Top-level user-visible status.
#[derive(Debug, Clone, Default)]
pub struct UnitStatus {
    /// Unit name.
    pub unit: Option<UnitName>,
    /// Load state.
    pub load: LoadState,
    /// Top-level state.
    pub active: ActiveState,
    /// Detailed sub-state.
    pub sub: SubState,
    /// Description.
    pub description: String,
}
