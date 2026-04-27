//! `org.freedesktop.systemd1.Manager` interface implementation.
//!
//! This is the entry point upstream `systemctl --user` calls into. The
//! method bodies are thin shims that take the `Manager` lock, dispatch, and
//! translate `systeml_runtime` results back into the D-Bus tuples upstream
//! expects.

use crate::{unit_object_path, MANAGER_PATH};
use std::str::FromStr;
use std::sync::Arc;
use systeml_deps::JobMode;
use systeml_runtime::manager::EnableChanges;
use systeml_runtime::Manager;
use systeml_unit::UnitName;
use tokio::sync::RwLock;
use zbus::object_server::SignalContext;
use zbus::zvariant::OwnedObjectPath;

/// Tuple returned from `ListUnits` / `ListUnitsByPatterns`.
///
/// Layout matches systemd's `(ssssssouso)`:
/// `(name, description, load_state, active_state, sub_state, follower,
///   unit_path, job_id, job_type, job_path)`.
pub type UnitListEntry = (
    String,
    String,
    String,
    String,
    String,
    String,
    OwnedObjectPath,
    u32,
    String,
    OwnedObjectPath,
);

/// systemd `(sss)` change-record for enable/disable/mask/unmask:
/// `(type, target, source)`.
pub type UnitFileChangeTuple = (String, String, String);

/// The Manager interface state. Holds a shared handle to the runtime
/// `Manager`; method calls take the lock as briefly as possible.
pub struct ManagerIface {
    manager: Arc<RwLock<Manager>>,
}

impl ManagerIface {
    /// Create a new manager interface bound to the given runtime.
    #[must_use]
    pub fn new(manager: Arc<RwLock<Manager>>) -> Self {
        Self { manager }
    }
}

fn parse_name(name: &str) -> zbus::fdo::Result<UnitName> {
    UnitName::from_str(name)
        .map_err(|e| zbus::fdo::Error::InvalidArgs(format!("bad unit name {name:?}: {e}")))
}

fn parse_mode(mode: &str) -> zbus::fdo::Result<JobMode> {
    Ok(match mode {
        "fail" => JobMode::Fail,
        "replace" | "" => JobMode::Replace,
        "replace-irreversibly" => JobMode::ReplaceIrreversibly,
        "isolate" => JobMode::Isolate,
        "flush" => JobMode::Flush,
        "ignore-dependencies" => JobMode::IgnoreDependencies,
        "ignore-requirements" => JobMode::IgnoreRequirements,
        other => {
            return Err(zbus::fdo::Error::InvalidArgs(format!(
                "unknown job mode {other:?}"
            )));
        }
    })
}

fn convert_changes(c: &EnableChanges) -> Vec<UnitFileChangeTuple> {
    c.changes
        .iter()
        .map(|c| {
            (
                c.change_type.clone(),
                c.target.display().to_string(),
                c.source.display().to_string(),
            )
        })
        .collect()
}

/// Map an [`ActiveState`](systeml_runtime::ActiveState) to its
/// systemd-canonical string.
#[must_use]
pub fn active_state_str(s: systeml_runtime::ActiveState) -> &'static str {
    use systeml_runtime::ActiveState as A;
    match s {
        A::Inactive => "inactive",
        A::Activating => "activating",
        A::Active => "active",
        A::Deactivating => "deactivating",
        A::Failed => "failed",
        A::Reloading => "reloading",
        A::Maintenance => "maintenance",
    }
}

/// Map a [`LoadState`](systeml_runtime::LoadState) to its systemd-canonical
/// string.
#[must_use]
pub fn load_state_str(s: systeml_runtime::LoadState) -> &'static str {
    use systeml_runtime::LoadState as L;
    match s {
        L::Loaded => "loaded",
        L::Error => "error",
        L::Stub => "stub",
        L::Masked => "masked",
        L::NotFound => "not-found",
        L::Merged => "merged",
        L::BadSetting => "bad-setting",
    }
}

#[zbus::interface(name = "org.freedesktop.systemd1.Manager")]
impl ManagerIface {
    // -------- methods --------

    /// `StartUnit(name, mode) -> ObjectPath` — enqueue a start job and
    /// return the per-job object path.
    async fn start_unit(&self, name: &str, mode: &str) -> zbus::fdo::Result<OwnedObjectPath> {
        let unit_name = parse_name(name)?;
        let job_mode = parse_mode(mode)?;
        let mut m = self.manager.write().await;
        let _ = m
            .start_unit(unit_name.clone(), job_mode)
            .await
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;
        // TODO(systemctl-compat): return real per-job object path once
        // `systeml-deps` exposes one.
        Ok(unit_object_path(&unit_name))
    }

    /// `StopUnit(name, mode) -> ObjectPath`.
    async fn stop_unit(&self, name: &str, mode: &str) -> zbus::fdo::Result<OwnedObjectPath> {
        let unit_name = parse_name(name)?;
        let job_mode = parse_mode(mode)?;
        let mut m = self.manager.write().await;
        let _ = m
            .stop_unit(unit_name.clone(), job_mode)
            .await
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;
        Ok(unit_object_path(&unit_name))
    }

    /// `RestartUnit(name, mode) -> ObjectPath`.
    async fn restart_unit(&self, name: &str, mode: &str) -> zbus::fdo::Result<OwnedObjectPath> {
        let unit_name = parse_name(name)?;
        let job_mode = parse_mode(mode)?;
        let mut m = self.manager.write().await;
        let _ = m
            .restart_unit(unit_name.clone(), job_mode)
            .await
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;
        Ok(unit_object_path(&unit_name))
    }

    /// `ReloadUnit(name, mode) -> ObjectPath`.
    async fn reload_unit(&self, name: &str, mode: &str) -> zbus::fdo::Result<OwnedObjectPath> {
        let unit_name = parse_name(name)?;
        let job_mode = parse_mode(mode)?;
        let mut m = self.manager.write().await;
        let _ = m
            .reload_unit(unit_name.clone(), job_mode)
            .await
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;
        Ok(unit_object_path(&unit_name))
    }

    /// `ReloadOrRestartUnit` — reload if active, else restart.
    async fn reload_or_restart_unit(
        &self,
        name: &str,
        mode: &str,
    ) -> zbus::fdo::Result<OwnedObjectPath> {
        // TODO(systemctl-compat): semantically distinct from restart once
        // runtime supports active-state inspection at job-create time.
        self.restart_unit(name, mode).await
    }

    /// `TryRestartUnit` — restart only if currently active.
    async fn try_restart_unit(
        &self,
        name: &str,
        mode: &str,
    ) -> zbus::fdo::Result<OwnedObjectPath> {
        // TODO(systemctl-compat): conditional on current active state.
        self.restart_unit(name, mode).await
    }

    /// `KillUnit(name, who, signal)` — send a signal to processes in the
    /// unit.
    async fn kill_unit(
        &self,
        _name: &str,
        _who: &str,
        _signal: i32,
    ) -> zbus::fdo::Result<()> {
        // TODO(systemctl-compat): runtime needs a `kill_unit` API that
        // dispatches to the supervisor.
        Ok(())
    }

    /// `GetUnit(name) -> ObjectPath` — return the object path of an
    /// already-loaded unit.
    async fn get_unit(&self, name: &str) -> zbus::fdo::Result<OwnedObjectPath> {
        let unit_name = parse_name(name)?;
        let m = self.manager.read().await;
        if !m.units.contains_key(&unit_name) {
            return Err(zbus::fdo::Error::Failed(format!(
                "unit not loaded: {unit_name}"
            )));
        }
        Ok(unit_object_path(&unit_name))
    }

    /// `LoadUnit(name) -> ObjectPath` — load if necessary, return path.
    async fn load_unit(&self, name: &str) -> zbus::fdo::Result<OwnedObjectPath> {
        let unit_name = parse_name(name)?;
        // TODO(systemctl-compat): trigger an actual load when not yet
        // present (runtime API is still TBD).
        Ok(unit_object_path(&unit_name))
    }

    /// `GetUnitFileState(name) -> String`.
    async fn get_unit_file_state(&self, name: &str) -> zbus::fdo::Result<String> {
        let unit_name = parse_name(name)?;
        let m = self.manager.read().await;
        Ok(m.unit_file_state(&unit_name))
    }

    /// `EnableUnitFiles(names, runtime, force) -> (bool, [(t,d,s)])`.
    async fn enable_unit_files(
        &self,
        names: Vec<String>,
        runtime: bool,
        force: bool,
    ) -> zbus::fdo::Result<(bool, Vec<UnitFileChangeTuple>)> {
        let parsed: Vec<UnitName> = names
            .iter()
            .map(|s| parse_name(s))
            .collect::<Result<_, _>>()?;
        let mut m = self.manager.write().await;
        let res = m
            .enable_units(&parsed, runtime, force)
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;
        Ok((res.carries_install_info, convert_changes(&res)))
    }

    /// `DisableUnitFiles(names, runtime) -> [(t,d,s)]`.
    async fn disable_unit_files(
        &self,
        names: Vec<String>,
        runtime: bool,
    ) -> zbus::fdo::Result<Vec<UnitFileChangeTuple>> {
        let parsed: Vec<UnitName> = names
            .iter()
            .map(|s| parse_name(s))
            .collect::<Result<_, _>>()?;
        let mut m = self.manager.write().await;
        let res = m
            .disable_units(&parsed, runtime)
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;
        Ok(convert_changes(&res))
    }

    /// `MaskUnitFiles(names, runtime, force) -> [(t,d,s)]`.
    async fn mask_unit_files(
        &self,
        names: Vec<String>,
        runtime: bool,
        force: bool,
    ) -> zbus::fdo::Result<Vec<UnitFileChangeTuple>> {
        let parsed: Vec<UnitName> = names
            .iter()
            .map(|s| parse_name(s))
            .collect::<Result<_, _>>()?;
        let mut m = self.manager.write().await;
        let res = m
            .mask_units(&parsed, runtime, force)
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;
        Ok(convert_changes(&res))
    }

    /// `UnmaskUnitFiles(names, runtime) -> [(t,d,s)]`.
    async fn unmask_unit_files(
        &self,
        names: Vec<String>,
        runtime: bool,
    ) -> zbus::fdo::Result<Vec<UnitFileChangeTuple>> {
        let parsed: Vec<UnitName> = names
            .iter()
            .map(|s| parse_name(s))
            .collect::<Result<_, _>>()?;
        let mut m = self.manager.write().await;
        let res = m
            .unmask_units(&parsed, runtime)
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;
        Ok(convert_changes(&res))
    }

    /// `ListUnits() -> Vec<UnitListEntry>`.
    async fn list_units(&self) -> zbus::fdo::Result<Vec<UnitListEntry>> {
        let m = self.manager.read().await;
        let mut out = Vec::with_capacity(m.status.len());
        for (name, status) in &m.status {
            let path = unit_object_path(name);
            let description = status.description.clone();
            out.push((
                name.to_string(),
                description,
                load_state_str(status.load).to_owned(),
                active_state_str(status.active).to_owned(),
                status.sub.clone(),
                String::new(),
                path,
                0u32,
                String::new(),
                OwnedObjectPath::try_from("/").expect("/ is a valid path"),
            ));
        }
        Ok(out)
    }

    /// `ListUnitFiles() -> [(path, state)]`.
    async fn list_unit_files(&self) -> zbus::fdo::Result<Vec<(String, String)>> {
        let m = self.manager.read().await;
        Ok(m.list_unit_files()
            .into_iter()
            .map(|(_n, p, st)| (p.display().to_string(), st))
            .collect())
    }

    /// `ListUnitsByPatterns(states, patterns) -> Vec<UnitListEntry>`.
    async fn list_units_by_patterns(
        &self,
        states: Vec<String>,
        patterns: Vec<String>,
    ) -> zbus::fdo::Result<Vec<UnitListEntry>> {
        let all = self.list_units().await?;
        Ok(all
            .into_iter()
            .filter(|row| {
                if !states.is_empty() && !states.iter().any(|s| s == &row.3) {
                    return false;
                }
                if patterns.is_empty() {
                    return true;
                }
                patterns.iter().any(|p| glob_match(p, &row.0))
            })
            .collect())
    }

    /// `Reload()` — equivalent to `systemctl daemon-reload`.
    async fn reload(&self, #[zbus(signal_context)] ctxt: SignalContext<'_>) -> zbus::fdo::Result<()> {
        Self::reloading(&ctxt, true).await.ok();
        let mut m = self.manager.write().await;
        m.daemon_reload()
            .await
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;
        drop(m);
        Self::reloading(&ctxt, false).await.ok();
        Ok(())
    }

    /// `Reexecute()` — re-exec the daemon. We treat as a soft reload.
    async fn reexecute(
        &self,
        #[zbus(signal_context)] ctxt: SignalContext<'_>,
    ) -> zbus::fdo::Result<()> {
        // TODO(systemctl-compat): true exec-self once the daemon binary
        // wires it up.
        self.reload(ctxt).await
    }

    /// `Subscribe()` — clients call this to opt in to signals; we always
    /// emit, so it's a no-op.
    async fn subscribe(&self) -> zbus::fdo::Result<()> {
        Ok(())
    }

    /// `Unsubscribe()` — counterpart to `Subscribe`. No-op.
    async fn unsubscribe(&self) -> zbus::fdo::Result<()> {
        Ok(())
    }

    /// `GetUnitProcesses(name) -> [(cgroup, pid, command)]`.
    async fn get_unit_processes(
        &self,
        _name: &str,
    ) -> zbus::fdo::Result<Vec<(String, u32, String)>> {
        // TODO(systemctl-compat): supervisor must expose live PID list.
        Ok(Vec::new())
    }

    /// `ResetFailedUnit(name)`.
    async fn reset_failed_unit(&self, _name: &str) -> zbus::fdo::Result<()> {
        // TODO(systemctl-compat): clear the unit's failed state.
        Ok(())
    }

    /// `ResetFailed()` — clear all failed units.
    async fn reset_failed(&self) -> zbus::fdo::Result<()> {
        // TODO(systemctl-compat): scan and clear all failed states.
        Ok(())
    }

    // -------- properties --------

    /// `Version` property — distinguishable from upstream by the prefix.
    #[zbus(property)]
    fn version(&self) -> String {
        format!("systeml {}", env!("CARGO_PKG_VERSION"))
    }

    /// `Architecture` property — runtime triple.
    #[zbus(property)]
    fn architecture(&self) -> String {
        std::env::consts::ARCH.to_owned()
    }

    /// `Features` property — empty (no compiled-in features advertised).
    #[zbus(property)]
    fn features(&self) -> String {
        String::new()
    }

    /// `UnitPath` property — the unit search paths in priority order.
    #[zbus(property)]
    fn unit_path(&self) -> Vec<String> {
        systeml_unit::search::user_search_paths()
            .into_iter()
            .map(|p| p.display().to_string())
            .collect()
    }

    /// `Environment` property — the daemon's `KEY=VAL` exports.
    #[zbus(property)]
    fn environment(&self) -> Vec<String> {
        // TODO(systemctl-compat): mirror `Manager`-managed environment once
        // the runtime exposes it.
        Vec::new()
    }

    /// `NNames` — count of well-known names. Always 0 on a p2p bus.
    #[zbus(property, name = "NNames")]
    fn nnames(&self) -> u32 {
        0
    }

    /// `NJobs` — currently running jobs.
    #[zbus(property, name = "NJobs")]
    async fn njobs(&self) -> u32 {
        // TODO(systemctl-compat): count active jobs once the queue is real.
        0
    }

    /// `NInstalledJobs` — total ever installed.
    #[zbus(property, name = "NInstalledJobs")]
    async fn ninstalled_jobs(&self) -> u32 {
        let m = self.manager.read().await;
        m.next_job_id.saturating_sub(1)
    }

    /// `NFailedJobs` — total failed.
    #[zbus(property, name = "NFailedJobs")]
    fn nfailed_jobs(&self) -> u32 {
        // TODO(systemctl-compat): count from supervisor history.
        0
    }

    /// `Progress` — 1.0 means "ready, no startup jobs in flight".
    #[zbus(property)]
    fn progress(&self) -> f64 {
        1.0
    }

    /// `LogLevel` property.
    #[zbus(property)]
    fn log_level(&self) -> String {
        "info".to_owned()
    }

    /// `LogTarget` property.
    #[zbus(property)]
    fn log_target(&self) -> String {
        "journal-or-kmsg".to_owned()
    }

    // -------- signals --------

    /// `UnitNew(name, path)`.
    #[zbus(signal)]
    pub async fn unit_new(
        ctxt: &SignalContext<'_>,
        id: String,
        path: OwnedObjectPath,
    ) -> zbus::Result<()>;

    /// `UnitRemoved(name, path)`.
    #[zbus(signal)]
    pub async fn unit_removed(
        ctxt: &SignalContext<'_>,
        id: String,
        path: OwnedObjectPath,
    ) -> zbus::Result<()>;

    /// `JobNew(id, path, name)`.
    #[zbus(signal)]
    pub async fn job_new(
        ctxt: &SignalContext<'_>,
        id: u32,
        job: OwnedObjectPath,
        unit: String,
    ) -> zbus::Result<()>;

    /// `JobRemoved(id, path, name, result)`.
    #[zbus(signal)]
    pub async fn job_removed(
        ctxt: &SignalContext<'_>,
        id: u32,
        job: OwnedObjectPath,
        unit: String,
        result: String,
    ) -> zbus::Result<()>;

    /// `Reloading(active)`.
    #[zbus(signal)]
    pub async fn reloading(ctxt: &SignalContext<'_>, active: bool) -> zbus::Result<()>;

    /// `StartupFinished(firmware,loader,kernel,initrd,userspace,total)`.
    #[zbus(signal)]
    pub async fn startup_finished(
        ctxt: &SignalContext<'_>,
        firmware: u64,
        loader: u64,
        kernel: u64,
        initrd: u64,
        userspace: u64,
        total: u64,
    ) -> zbus::Result<()>;
}

/// Build a `SignalContext` aimed at the manager object. Helpful when emitting
/// signals from outside this module's interface methods (e.g. from the
/// runtime-event bridge in [`crate::events`]).
pub fn manager_signal_context(
    conn: &zbus::Connection,
) -> zbus::Result<SignalContext<'static>> {
    SignalContext::new(conn, MANAGER_PATH).map(|c| c.into_owned())
}

/// Tiny `*`/`?` glob matcher. Mirrors systemd's `fnmatch(name, FNM_NOESCAPE)`.
fn glob_match(pattern: &str, name: &str) -> bool {
    fn helper(p: &[u8], n: &[u8]) -> bool {
        match (p.first(), n.first()) {
            (None, None) => true,
            (Some(b'*'), _) => helper(&p[1..], n) || (!n.is_empty() && helper(p, &n[1..])),
            (Some(b'?'), Some(_)) => helper(&p[1..], &n[1..]),
            (Some(a), Some(b)) if a == b => helper(&p[1..], &n[1..]),
            _ => false,
        }
    }
    helper(pattern.as_bytes(), name.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_modes() {
        assert!(matches!(parse_mode("replace"), Ok(JobMode::Replace)));
        assert!(matches!(parse_mode(""), Ok(JobMode::Replace)));
        assert!(matches!(parse_mode("isolate"), Ok(JobMode::Isolate)));
        assert!(parse_mode("nonsense").is_err());
    }

    #[test]
    fn glob_basic() {
        assert!(glob_match("*", "foo.service"));
        assert!(glob_match("*.service", "foo.service"));
        assert!(!glob_match("*.timer", "foo.service"));
        assert!(glob_match("foo?bar", "fooXbar"));
    }
}
