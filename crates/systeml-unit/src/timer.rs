//! `[Timer]` section.

use crate::calendar::CalendarSpec;
use crate::duration::SdDuration;
use crate::name::UnitName;

/// `[Timer]` directives.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct TimerUnit {
    /// All `OnCalendar=` directives.
    pub on_calendar: Vec<CalendarSpec>,
    /// `OnActiveSec=` — relative to timer activation.
    pub on_active_sec: Option<SdDuration>,
    /// `OnBootSec=` — relative to system boot.
    pub on_boot_sec: Option<SdDuration>,
    /// `OnStartupSec=` — relative to manager start.
    pub on_startup_sec: Option<SdDuration>,
    /// `OnUnitActiveSec=` — relative to last activation of the linked unit.
    pub on_unit_active_sec: Option<SdDuration>,
    /// `OnUnitInactiveSec=` — relative to last deactivation.
    pub on_unit_inactive_sec: Option<SdDuration>,
    /// `OnClockChange=`.
    pub on_clock_change: bool,
    /// `OnTimezoneChange=`.
    pub on_timezone_change: bool,

    /// `AccuracySec=` (default 1min).
    pub accuracy_sec: Option<SdDuration>,
    /// `RandomizedDelaySec=`.
    pub randomized_delay_sec: Option<SdDuration>,
    /// `FixedRandomDelay=`.
    pub fixed_random_delay: bool,
    /// `Persistent=`.
    pub persistent: bool,
    /// `WakeSystem=`.
    pub wake_system: bool,
    /// `RemainAfterElapse=` (default true).
    pub remain_after_elapse: bool,
    /// `DeferReactivation=`.
    pub defer_reactivation: bool,

    /// `Unit=` — explicit linked unit. Default `<name>.service`.
    pub unit: Option<UnitName>,
}

impl TimerUnit {
    /// Default `AccuracySec=` per systemd: 1 minute.
    pub const DEFAULT_ACCURACY: SdDuration =
        SdDuration::Finite(std::time::Duration::from_secs(60));
}
