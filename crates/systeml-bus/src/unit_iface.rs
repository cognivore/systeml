//! `org.freedesktop.systemd1.Unit` interface implementation.
//!
//! One instance per loaded unit. All getters take a brief read lock on the
//! shared `Manager` and project a single property out of the live state.

use std::sync::Arc;
use systeml_runtime::Manager;
use systeml_unit::{Unit, UnitName, UnitTypeData};
use tokio::sync::RwLock;

/// Per-unit interface state.
pub struct UnitIface {
    name: UnitName,
    manager: Arc<RwLock<Manager>>,
}

impl UnitIface {
    /// Create a new `Unit` interface bound to a name + the shared manager.
    #[must_use]
    pub fn new(name: UnitName, manager: Arc<RwLock<Manager>>) -> Self {
        Self { name, manager }
    }
}

fn names_to_strings<'a>(it: impl Iterator<Item = &'a UnitName>) -> Vec<String> {
    it.map(ToString::to_string).collect()
}

fn project<R>(unit: Option<&Unit>, default: R, f: impl FnOnce(&Unit) -> R) -> R {
    match unit {
        Some(u) => f(u),
        None => default,
    }
}

#[zbus::interface(name = "org.freedesktop.systemd1.Unit")]
impl UnitIface {
    // --- name + ID ---

    /// `Names` — primary name plus aliases declared in `[Install]`.
    #[zbus(property)]
    async fn names(&self) -> Vec<String> {
        let m = self.manager.read().await;
        let mut out = vec![self.name.to_string()];
        if let Some(lu) = m.units.get(&self.name) {
            out.extend(names_to_strings(lu.unit.install.alias.iter()));
        }
        out
    }

    /// `Id` — primary unit name.
    #[zbus(property)]
    fn id(&self) -> String {
        self.name.to_string()
    }

    /// `Description`.
    #[zbus(property)]
    async fn description(&self) -> String {
        let m = self.manager.read().await;
        m.unit_status(&self.name).description
    }

    /// `Following` — name of the unit we follow (zero-string when not).
    #[zbus(property)]
    fn following(&self) -> String {
        // TODO(systemctl-compat): unit following requires runtime state.
        String::new()
    }

    // --- dependencies ---

    /// `Requires`.
    #[zbus(property)]
    async fn requires(&self) -> Vec<String> {
        let m = self.manager.read().await;
        project(m.units.get(&self.name).map(|lu| &lu.unit), Vec::new(), |u| {
            names_to_strings(u.deps.requires.iter())
        })
    }

    /// `Wants`.
    #[zbus(property)]
    async fn wants(&self) -> Vec<String> {
        let m = self.manager.read().await;
        project(m.units.get(&self.name).map(|lu| &lu.unit), Vec::new(), |u| {
            names_to_strings(u.deps.wants.iter())
        })
    }

    /// `BindsTo`.
    #[zbus(property)]
    async fn binds_to(&self) -> Vec<String> {
        let m = self.manager.read().await;
        project(m.units.get(&self.name).map(|lu| &lu.unit), Vec::new(), |u| {
            names_to_strings(u.deps.binds_to.iter())
        })
    }

    /// `Conflicts`.
    #[zbus(property)]
    async fn conflicts(&self) -> Vec<String> {
        let m = self.manager.read().await;
        project(m.units.get(&self.name).map(|lu| &lu.unit), Vec::new(), |u| {
            names_to_strings(u.deps.conflicts.iter())
        })
    }

    /// `Before`.
    #[zbus(property)]
    async fn before(&self) -> Vec<String> {
        let m = self.manager.read().await;
        project(m.units.get(&self.name).map(|lu| &lu.unit), Vec::new(), |u| {
            names_to_strings(u.deps.before.iter())
        })
    }

    /// `After`.
    #[zbus(property)]
    async fn after(&self) -> Vec<String> {
        let m = self.manager.read().await;
        project(m.units.get(&self.name).map(|lu| &lu.unit), Vec::new(), |u| {
            names_to_strings(u.deps.after.iter())
        })
    }

    /// `RequiredBy` — reverse `RequiredBy=` (from [Install]).
    #[zbus(property)]
    async fn required_by(&self) -> Vec<String> {
        let m = self.manager.read().await;
        project(m.units.get(&self.name).map(|lu| &lu.unit), Vec::new(), |u| {
            names_to_strings(u.install.required_by.iter())
        })
    }

    /// `WantedBy` — reverse `WantedBy=` (from [Install]).
    #[zbus(property)]
    async fn wanted_by(&self) -> Vec<String> {
        let m = self.manager.read().await;
        project(m.units.get(&self.name).map(|lu| &lu.unit), Vec::new(), |u| {
            names_to_strings(u.install.wanted_by.iter())
        })
    }

    // --- state ---

    /// `LoadState`.
    #[zbus(property)]
    async fn load_state(&self) -> String {
        let m = self.manager.read().await;
        crate::manager_iface::load_state_str(m.unit_status(&self.name).load).to_owned()
    }

    /// `ActiveState`.
    #[zbus(property)]
    async fn active_state(&self) -> String {
        let m = self.manager.read().await;
        crate::manager_iface::active_state_str(m.unit_status(&self.name).active).to_owned()
    }

    /// `SubState`.
    #[zbus(property)]
    async fn sub_state(&self) -> String {
        let m = self.manager.read().await;
        m.unit_status(&self.name).sub
    }

    /// `FragmentPath`.
    #[zbus(property)]
    async fn fragment_path(&self) -> String {
        let m = self.manager.read().await;
        m.fragment_path(&self.name)
            .map(|p| p.display().to_string())
            .unwrap_or_default()
    }

    /// `UnitFileState`.
    #[zbus(property)]
    async fn unit_file_state(&self) -> String {
        let m = self.manager.read().await;
        m.unit_file_state(&self.name)
    }

    /// `UnitFilePreset`.
    #[zbus(property)]
    fn unit_file_preset(&self) -> String {
        // TODO(systemctl-compat): preset evaluation.
        String::new()
    }

    // --- timestamps ---

    /// `ActiveEnterTimestamp` (microseconds since epoch).
    #[zbus(property)]
    fn active_enter_timestamp(&self) -> u64 {
        // TODO(systemctl-compat): wire to runtime timestamps.
        0
    }

    /// `ActiveExitTimestamp`.
    #[zbus(property)]
    fn active_exit_timestamp(&self) -> u64 {
        0
    }

    /// `InactiveEnterTimestamp`.
    #[zbus(property)]
    fn inactive_enter_timestamp(&self) -> u64 {
        0
    }

    /// `InactiveExitTimestamp`.
    #[zbus(property)]
    fn inactive_exit_timestamp(&self) -> u64 {
        0
    }

    // --- can-* ---

    /// `CanStart`.
    #[zbus(property)]
    async fn can_start(&self) -> bool {
        let m = self.manager.read().await;
        m.units
            .get(&self.name)
            .map(|lu| !lu.unit.refuse_manual_start)
            .unwrap_or(true)
    }

    /// `CanStop`.
    #[zbus(property)]
    async fn can_stop(&self) -> bool {
        let m = self.manager.read().await;
        m.units
            .get(&self.name)
            .map(|lu| !lu.unit.refuse_manual_stop)
            .unwrap_or(true)
    }

    /// `CanReload` — true iff the unit declares `ExecReload=`.
    #[zbus(property)]
    async fn can_reload(&self) -> bool {
        let m = self.manager.read().await;
        match m.units.get(&self.name).map(|lu| &lu.unit.kind) {
            Some(UnitTypeData::Service(s)) => !s.exec_reload.is_empty(),
            _ => false,
        }
    }
}
