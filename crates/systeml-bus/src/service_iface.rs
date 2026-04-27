//! `org.freedesktop.systemd1.Service` interface.
//!
//! Only registered for `.service` units. Most of the properties read out of
//! the typed `ServiceUnit` AST directly; runtime-tracked fields like
//! `MainPID` are stubs for now.

use std::sync::Arc;
use systeml_runtime::Manager;
use systeml_unit::{ServiceUnit, UnitName, UnitTypeData};
use tokio::sync::RwLock;

/// systemd's `ExecStart=` tuple type. The fields, in order, are:
/// `(path, argv, ignore_failure, start_realtime_us, start_monotonic_us,
///   exit_realtime_us, exit_monotonic_us, pid, exit_code, status)`.
pub type ExecTuple = (
    String,
    Vec<String>,
    bool,
    u64,
    u64,
    u64,
    u64,
    u32,
    i32,
    i32,
);

/// Per-unit Service interface state.
pub struct ServiceIface {
    name: UnitName,
    manager: Arc<RwLock<Manager>>,
}

impl ServiceIface {
    /// Create a new Service interface bound to a `.service` unit.
    #[must_use]
    pub fn new(name: UnitName, manager: Arc<RwLock<Manager>>) -> Self {
        Self { name, manager }
    }
}

fn service_type_str(t: systeml_unit::ServiceType) -> &'static str {
    use systeml_unit::ServiceType as T;
    match t {
        T::Simple => "simple",
        T::Exec => "exec",
        T::Forking => "forking",
        T::Oneshot => "oneshot",
        T::Notify => "notify",
        T::NotifyReload => "notify-reload",
        T::Dbus => "dbus",
        T::Idle => "idle",
    }
}

fn restart_str(r: systeml_unit::Restart) -> &'static str {
    use systeml_unit::Restart as R;
    match r {
        R::No => "no",
        R::Always => "always",
        R::OnSuccess => "on-success",
        R::OnFailure => "on-failure",
        R::OnAbnormal => "on-abnormal",
        R::OnWatchdog => "on-watchdog",
        R::OnAbort => "on-abort",
    }
}

fn duration_us(d: systeml_unit::SdDuration) -> u64 {
    match d {
        systeml_unit::SdDuration::Finite(d) => {
            u64::try_from(d.as_micros()).unwrap_or(u64::MAX)
        }
        systeml_unit::SdDuration::Infinity => u64::MAX,
    }
}

fn project<R>(svc: Option<&ServiceUnit>, default: R, f: impl FnOnce(&ServiceUnit) -> R) -> R {
    match svc {
        Some(s) => f(s),
        None => default,
    }
}

#[zbus::interface(name = "org.freedesktop.systemd1.Service")]
impl ServiceIface {
    /// `Type` — `simple`, `oneshot`, etc.
    #[zbus(property)]
    async fn type_(&self) -> String {
        let m = self.manager.read().await;
        let svc = service_of(&m, &self.name);
        project(svc, "simple".to_owned(), |s| {
            service_type_str(s.service_type).to_owned()
        })
    }

    /// `Restart` policy.
    #[zbus(property)]
    async fn restart(&self) -> String {
        let m = self.manager.read().await;
        let svc = service_of(&m, &self.name);
        project(svc, "no".to_owned(), |s| restart_str(s.restart).to_owned())
    }

    /// `RestartUSec`.
    #[zbus(property, name = "RestartUSec")]
    async fn restart_usec(&self) -> u64 {
        let m = self.manager.read().await;
        let svc = service_of(&m, &self.name);
        project(svc, 0, |s| duration_us(s.restart_sec))
    }

    /// `TimeoutStartUSec`.
    #[zbus(property, name = "TimeoutStartUSec")]
    async fn timeout_start_usec(&self) -> u64 {
        let m = self.manager.read().await;
        let svc = service_of(&m, &self.name);
        project(svc, 0, |s| {
            s.timeout_start_sec.map_or(0, duration_us)
        })
    }

    /// `TimeoutStopUSec`.
    #[zbus(property, name = "TimeoutStopUSec")]
    async fn timeout_stop_usec(&self) -> u64 {
        let m = self.manager.read().await;
        let svc = service_of(&m, &self.name);
        project(svc, 0, |s| s.timeout_stop_sec.map_or(0, duration_us))
    }

    /// `WatchdogUSec`.
    #[zbus(property, name = "WatchdogUSec")]
    async fn watchdog_usec(&self) -> u64 {
        let m = self.manager.read().await;
        let svc = service_of(&m, &self.name);
        project(svc, 0, |s| s.watchdog_sec.map_or(0, duration_us))
    }

    /// `ExecMainPID` — main process pid (0 if not running).
    #[zbus(property, name = "ExecMainPID")]
    fn exec_main_pid(&self) -> u32 {
        // TODO(systemctl-compat): runtime-tracked PID.
        0
    }

    /// `MainPID` — alias for `ExecMainPID`.
    #[zbus(property, name = "MainPID")]
    fn main_pid(&self) -> u32 {
        // TODO(systemctl-compat): runtime-tracked PID.
        0
    }

    /// `ControlPID` — auxiliary control PID (e.g. ExecStop), 0 if none.
    #[zbus(property, name = "ControlPID")]
    fn control_pid(&self) -> u32 {
        // TODO(systemctl-compat): runtime-tracked PID.
        0
    }

    /// `Result` — last completion outcome.
    #[zbus(property)]
    fn result(&self) -> String {
        // TODO(systemctl-compat): track from supervisor exit handler.
        "success".to_owned()
    }

    /// `ExecStart` — every `ExecStart=` tuple.
    #[zbus(property)]
    async fn exec_start(&self) -> Vec<ExecTuple> {
        let m = self.manager.read().await;
        let Some(svc) = service_of(&m, &self.name) else {
            return Vec::new();
        };
        svc.exec_start
            .iter()
            .map(|cmd| {
                (
                    cmd.program.clone(),
                    cmd.argv.clone(),
                    cmd.flags
                        .contains(systeml_unit::ExecFlags::IGNORE_FAILURE),
                    0u64,
                    0u64,
                    0u64,
                    0u64,
                    0u32,
                    0i32,
                    0i32,
                )
            })
            .collect()
    }
}

fn service_of<'a>(m: &'a Manager, name: &UnitName) -> Option<&'a ServiceUnit> {
    let lu = m.units.get(name)?;
    if let UnitTypeData::Service(s) = &lu.unit.kind {
        Some(s)
    } else {
        None
    }
}
