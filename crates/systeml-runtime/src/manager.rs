//! Manager facade. The daemon owns one `Manager`; the bus and supervisor
//! both call into it.
//!
//! This module is the **public contract** the bus, CLI, and daemon all
//! consume. Every `start`/`stop`/`enable`/`reload` flows through here.

use crate::install;
use crate::service::ServiceRunner;
use crate::state::{ActiveState, LoadState, UnitStatus};
use crate::timer::firing::{TimerControl, TimerControlSender};
use anyhow::{anyhow, Result};
use indexmap::IndexMap;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Weak};
use systeml_deps::{JobId, JobMode, JobOutcome, JobType};
use systeml_unit::name::UnitKind;
use systeml_unit::{
    is_activatable, load_unit_in, search::user_search_paths, LoadedUnit, UnitName, UnitTypeData,
};
use tokio::sync::RwLock;
use tracing::{info, warn};

/// Why an enable/disable operation made a change. Mirrors systemd's
/// `UnitFileChange` struct: `(type, target, source)`.
#[derive(Debug, Clone)]
pub struct UnitFileChange {
    /// `"symlink"`, `"unlink"`, etc.
    pub change_type: String,
    /// The path of the link.
    pub target: PathBuf,
    /// What it points to (for symlink), or empty.
    pub source: PathBuf,
}

/// Result of an enable/disable/mask/unmask op.
#[derive(Debug, Default, Clone)]
pub struct EnableChanges {
    /// Whether the unit files declared `[Install]` info.
    pub carries_install_info: bool,
    /// File-system level changes performed.
    pub changes: Vec<UnitFileChange>,
}

/// One event emitted on the manager's broadcast channel.
#[derive(Debug, Clone)]
pub enum UnitEvent {
    /// Unit was loaded.
    UnitNew(UnitName),
    /// Unit was unloaded.
    UnitRemoved(UnitName),
    /// Job was queued.
    JobNew {
        /// Job id.
        id: JobId,
        /// Target unit.
        unit: UnitName,
        /// Job kind.
        kind: JobType,
    },
    /// Job completed.
    JobRemoved {
        /// Job id.
        id: JobId,
        /// Target unit.
        unit: UnitName,
        /// Final outcome.
        outcome: JobOutcome,
    },
    /// Unit's `ActiveState`/`SubState` changed.
    StateChanged {
        /// Affected unit.
        unit: UnitName,
        /// New ActiveState.
        active: ActiveState,
        /// New SubState.
        sub: String,
    },
}

/// In-memory unit registry plus runtime state.
pub struct Manager {
    /// All loaded units, keyed by canonical name.
    pub units: IndexMap<UnitName, LoadedUnit>,
    /// Per-unit runtime state.
    pub status: IndexMap<UnitName, UnitStatus>,
    /// Per-service runtime supervisors.
    pub services: IndexMap<UnitName, Arc<ServiceRunner>>,
    /// Broadcast channel for unit events.
    pub events: tokio::sync::broadcast::Sender<UnitEvent>,
    /// Monotonic job-id counter.
    pub next_job_id: u32,
    /// Control sender for the timer firing engine. `None` if no
    /// scheduler is attached (tests, headless usage).
    pub timer_control: Option<TimerControlSender>,
    /// Weak handle to the `Arc<RwLock<Manager>>` that owns us. Set by
    /// `attach_self` after the daemon wraps the manager. Used by spawned
    /// supervisor tasks (see `supervise_service`) so they can broadcast
    /// state changes when a child process exits without holding a strong
    /// reference cycle. `None` until attached or in headless tests.
    pub self_weak: Option<Weak<RwLock<Manager>>>,
}

impl Default for Manager {
    fn default() -> Self {
        Self::new()
    }
}

impl Manager {
    /// New empty manager. Constructs a broadcast channel with capacity 1024.
    #[must_use]
    pub fn new() -> Self {
        let (tx, _rx) = tokio::sync::broadcast::channel(1024);
        Self {
            units: IndexMap::new(),
            status: IndexMap::new(),
            services: IndexMap::new(),
            events: tx,
            next_job_id: 1,
            timer_control: None,
            self_weak: None,
        }
    }

    /// Record a weak handle to the `Arc<RwLock<Manager>>` that owns us. The
    /// daemon calls this immediately after wrapping `Manager::new()` so the
    /// service supervisor (spawned per started unit) can take the lock to
    /// broadcast `StateChanged` when a child exits.
    pub fn attach_self(&mut self, arc: &Arc<RwLock<Manager>>) {
        self.self_weak = Some(Arc::downgrade(arc));
    }

    /// Attach a timer-firing scheduler's control sender. The daemon
    /// passes this in after spawning [`crate::timer::firing::spawn`].
    /// Subsequent `daemon_reload`s and timer state changes will poke
    /// the scheduler via this channel.
    pub fn attach_timer_control(&mut self, tx: TimerControlSender) {
        self.timer_control = Some(tx);
    }

    /// Best-effort: tell the timer scheduler something changed.
    fn poke_timer_scheduler(&self) {
        if let Some(tx) = &self.timer_control {
            // try_send returns Err if the channel is full (drop the
            // message — Refresh is idempotent, the next one will
            // suffice) or closed (scheduler shut down — fine).
            let _ = tx.try_send(TimerControl::Refresh);
        }
    }

    /// Update a unit's active/sub state and broadcast `StateChanged`. No-op
    /// if the unit has no status entry.
    pub(crate) fn mark_state(&mut self, name: &UnitName, active: ActiveState, sub: &str) {
        if let Some(st) = self.status.get_mut(name) {
            st.active = active;
            st.sub = sub.into();
        }
        let _ = self.events.send(UnitEvent::StateChanged {
            unit: name.clone(),
            active,
            sub: sub.into(),
        });
    }

    /// Auto-activate every loaded unit that has an enable-link
    /// (`<target>.wants/<unit>`, `.requires/`, or `.upholds/`) in any
    /// search path. Mirrors what systemd PID 1 does at boot when it
    /// starts `default.target` and the transitive `wants/` closure
    /// pulls in everything enabled.
    ///
    /// Without this, every daemon restart leaves enabled timers stuck
    /// at `Inactive` until something starts them — `systemctl start`,
    /// or the next `home-manager switch` invoking the sd-switch shim
    /// against a *changed* unit. Untouched-but-enabled timers (the
    /// common case for stable schedules like a daily backup) just
    /// silently never fire.
    ///
    /// Skips already-Active units so a SIGHUP `daemon-reload` is a no-op
    /// on the activation front (matching systemd's reload semantics).
    pub async fn activate_enabled_units(&mut self) -> Result<()> {
        let mut started = 0usize;
        let mut skipped = 0usize;
        let names: Vec<UnitName> = self.units.keys().cloned().collect();
        for name in names {
            // Only timer/service/path/socket units are activatable in this
            // sense; targets are pulled in implicitly when needed.
            match name.kind {
                UnitKind::Timer
                | UnitKind::Service
                | UnitKind::Path
                | UnitKind::Socket => {}
                _ => continue,
            }
            if self
                .status
                .get(&name)
                .map(|s| s.active == ActiveState::Active)
                .unwrap_or(false)
            {
                skipped += 1;
                continue;
            }
            if !install::has_install_link(&name) {
                continue;
            }
            match self.start_unit(name.clone(), JobMode::Replace).await {
                Ok(_) => {
                    started += 1;
                    info!(unit = %name, "auto-started enabled unit");
                }
                Err(e) => warn!(unit = %name, error = %e, "auto-start failed"),
            }
        }
        info!(started, already_active = skipped, "activate_enabled_units complete");
        Ok(())
    }

    /// Insert a freshly loaded unit. Sub-state remains "dead" until activated.
    pub fn insert_loaded(&mut self, name: UnitName, lu: LoadedUnit) {
        let st = UnitStatus {
            unit: Some(name.clone()),
            load: LoadState::Loaded,
            active: ActiveState::Inactive,
            sub: "dead".into(),
            description: lu.unit.description.clone(),
        };
        // Build a service runner for `.service` units.
        if let UnitTypeData::Service(svc) = &lu.unit.kind {
            let runner = ServiceRunner::new(name.clone(), svc.clone());
            self.services.insert(name.clone(), runner);
        }
        self.units.insert(name.clone(), lu);
        self.status.insert(name.clone(), st);
        let _ = self.events.send(UnitEvent::UnitNew(name));
    }

    /// Look up the live status of a unit.
    pub fn unit_status(&self, name: &UnitName) -> UnitStatus {
        self.status
            .get(name)
            .cloned()
            .unwrap_or_else(|| UnitStatus {
                unit: Some(name.clone()),
                load: LoadState::NotFound,
                ..UnitStatus::default()
            })
    }

    /// `daemon-reload` — rescan search paths, parse, and refresh `units`.
    pub async fn daemon_reload(&mut self) -> Result<()> {
        let paths = user_search_paths();
        let mut seen: IndexMap<UnitName, PathBuf> = IndexMap::new();

        for base in &paths {
            let entries = match std::fs::read_dir(base) {
                Ok(e) => e,
                Err(_) => continue,
            };
            for entry in entries.flatten() {
                let p = entry.path();
                if !p.is_file() {
                    continue;
                }
                let name = match UnitName::from_path(&p) {
                    Ok(n) => n,
                    Err(_) => continue,
                };
                seen.entry(name).or_insert(p);
            }
        }

        // For every found unit, run load_unit_in to layer drop-ins.
        let mut new_units: IndexMap<UnitName, LoadedUnit> = IndexMap::new();
        for (name, _path) in &seen {
            match load_unit_in(name, &paths) {
                Ok(lu) => {
                    if !is_activatable(&lu.unit) {
                        continue;
                    }
                    new_units.insert(name.clone(), lu);
                }
                Err(e) => warn!(unit = %name, error = %e, "failed to load unit"),
            }
        }

        // Drop status/services for units no longer present.
        let removed: Vec<UnitName> = self
            .units
            .keys()
            .filter(|n| !new_units.contains_key(*n))
            .cloned()
            .collect();
        for r in removed {
            self.status.shift_remove(&r);
            self.services.shift_remove(&r);
            let _ = self.events.send(UnitEvent::UnitRemoved(r));
        }

        // Apply new units.
        for (name, lu) in new_units {
            // If we already had a status, keep its active/sub but refresh
            // description.
            let st = self
                .status
                .shift_remove(&name)
                .map(|mut s| {
                    s.description.clone_from(&lu.unit.description);
                    s.load = LoadState::Loaded;
                    s
                })
                .unwrap_or_else(|| UnitStatus {
                    unit: Some(name.clone()),
                    load: LoadState::Loaded,
                    description: lu.unit.description.clone(),
                    ..UnitStatus::default()
                });
            // Service runner: rebuild on every reload so a unit-file
            // change (e.g. a new ExecStart from `home-manager switch`)
            // takes effect at the next start. Real systemd's
            // `manager_reload` (src/core/manager.c:3555) serializes
            // runtime state, drops every Unit struct, re-parses every
            // fragment from disk, then deserializes the runtime state
            // back — net effect: the spec is always fresh, but PIDs
            // and child handles survive. We approximate that here:
            // construct a new `ServiceRunner` from the freshly-parsed
            // `ServiceUnit`, then transfer the live PID/child handle
            // from the old runner so `stop()` can still signal the
            // process. The supervisor task already holds an `Arc` to
            // the old runner via `Arc::clone`, so its in-flight
            // `child.wait()` is unaffected by us swapping the map
            // entry.
            //
            // Without this, `daemon-reload` re-read the unit file but
            // `start_unit` kept re-running the *old* `ExecStart`
            // because `ServiceRunner` cached the original `svc` at
            // construction. That bit a backup-home update: the wrapper
            // path changed (new exclude file inside it), but
            // post-`daemon-reload` runs invoked the previous wrapper
            // and kept failing on TCC paths the new excludes had
            // already covered.
            if let UnitTypeData::Service(svc) = &lu.unit.kind {
                let new_runner = ServiceRunner::new(name.clone(), svc.clone());
                if let Some(old) = self.services.get(&name) {
                    // Transfer in-flight tracking. State/sub mirror
                    // what the manager's status already says (we
                    // re-publish on the next start/stop), but the PID
                    // and child handle are load-bearing for stop().
                    *new_runner.state.lock().await = *old.state.lock().await;
                    *new_runner.sub.lock().await = old.sub.lock().await.clone();
                    *new_runner.main_pid.lock().await = *old.main_pid.lock().await;
                    *new_runner.child.lock().await = old.child.lock().await.take();
                }
                self.services.insert(name.clone(), new_runner);
            }
            self.units.insert(name.clone(), lu);
            self.status.insert(name.clone(), st);
            let _ = self.events.send(UnitEvent::UnitNew(name));
        }
        info!("daemon-reload: {} units loaded", self.units.len());
        // Wake the timer scheduler so it picks up new/removed timers.
        self.poke_timer_scheduler();
        Ok(())
    }

    /// Snapshot of every loaded unit's status.
    #[must_use]
    pub fn list_units(&self) -> Vec<UnitStatus> {
        self.status.values().cloned().collect()
    }

    /// Walk the search path and report every unit-file present, with its
    /// enable state.
    #[must_use]
    pub fn list_unit_files(&self) -> Vec<(UnitName, PathBuf, String)> {
        let mut out: IndexMap<UnitName, PathBuf> = IndexMap::new();
        for base in user_search_paths() {
            let entries = match std::fs::read_dir(&base) {
                Ok(e) => e,
                Err(_) => continue,
            };
            for entry in entries.flatten() {
                let p = entry.path();
                if !p.is_file() {
                    continue;
                }
                let name = match UnitName::from_path(&p) {
                    Ok(n) => n,
                    Err(_) => continue,
                };
                out.entry(name).or_insert(p);
            }
        }
        out.into_iter()
            .map(|(name, p)| {
                let install = self.units.get(&name).map(|lu| &lu.unit.install);
                let state = install::unit_file_state(&name, install);
                (name, p, state)
            })
            .collect()
    }

    /// `(unit_file_state)` per `systemctl is-enabled`.
    #[must_use]
    pub fn unit_file_state(&self, name: &UnitName) -> String {
        let install = self.units.get(name).map(|lu| &lu.unit.install);
        install::unit_file_state(name, install)
    }

    /// Path of the main fragment.
    #[must_use]
    pub fn fragment_path(&self, name: &UnitName) -> Option<PathBuf> {
        self.units
            .get(name)
            .and_then(|lu| lu.unit.fragment_paths.first().cloned())
    }

    /// `cat` rendering: main + drop-ins concatenated.
    pub fn cat(&self, name: &UnitName) -> Result<String> {
        let lu = self
            .units
            .get(name)
            .ok_or_else(|| anyhow!("unit not loaded: {name}"))?;
        Ok(lu.unit.render_cat())
    }

    /// All unit properties for `systemctl show`.
    pub fn show_properties(&self, name: &UnitName) -> Result<BTreeMap<String, String>> {
        let lu = self
            .units
            .get(name)
            .ok_or_else(|| anyhow!("unit not loaded: {name}"))?;
        let st = self.unit_status(name);
        let mut out = BTreeMap::new();
        out.insert("Id".into(), name.to_string());
        out.insert("Names".into(), name.to_string());
        out.insert("Description".into(), lu.unit.description.clone());
        out.insert(
            "LoadState".into(),
            format!("{:?}", st.load).to_ascii_lowercase(),
        );
        out.insert(
            "ActiveState".into(),
            format!("{:?}", st.active).to_ascii_lowercase(),
        );
        out.insert("SubState".into(), st.sub.clone());
        out.insert(
            "FragmentPath".into(),
            lu.unit
                .fragment_paths
                .first()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
        );
        out.insert(
            "DefaultDependencies".into(),
            yesno(lu.unit.default_dependencies),
        );
        out.insert("StopWhenUnneeded".into(), yesno(lu.unit.stop_when_unneeded));
        out.insert(
            "RefuseManualStart".into(),
            yesno(lu.unit.refuse_manual_start),
        );
        out.insert("RefuseManualStop".into(), yesno(lu.unit.refuse_manual_stop));
        out.insert("Documentation".into(), lu.unit.documentation.join(" "));
        // Per-type properties.
        match &lu.unit.kind {
            UnitTypeData::Service(svc) => {
                out.insert(
                    "Type".into(),
                    format!("{:?}", svc.service_type).to_ascii_lowercase(),
                );
                out.insert(
                    "Restart".into(),
                    format!("{:?}", svc.restart).to_ascii_lowercase(),
                );
                out.insert("RemainAfterExit".into(), yesno(svc.remain_after_exit));
                if let Some(p) = &svc.pid_file {
                    out.insert("PIDFile".into(), p.display().to_string());
                }
                if let Some(u) = &svc.user {
                    out.insert("User".into(), u.clone());
                }
                if let Some(g) = &svc.group {
                    out.insert("Group".into(), g.clone());
                }
                out.insert(
                    "ExecStart".into(),
                    svc.exec_start
                        .iter()
                        .map(|c| c.raw.clone())
                        .collect::<Vec<_>>()
                        .join("; "),
                );
                if !svc.exec_stop.is_empty() {
                    out.insert(
                        "ExecStop".into(),
                        svc.exec_stop
                            .iter()
                            .map(|c| c.raw.clone())
                            .collect::<Vec<_>>()
                            .join("; "),
                    );
                }
                if let Some(d) = svc.timeout_start_sec {
                    out.insert(
                        "TimeoutStartUSec".into(),
                        format!("{}us", d.as_std().as_micros()),
                    );
                }
                if let Some(d) = svc.timeout_stop_sec {
                    out.insert(
                        "TimeoutStopUSec".into(),
                        format!("{}us", d.as_std().as_micros()),
                    );
                }
            }
            UnitTypeData::Timer(t) => {
                out.insert(
                    "OnCalendar".into(),
                    t.on_calendar
                        .iter()
                        .map(|s| s.raw.clone())
                        .collect::<Vec<_>>()
                        .join(" "),
                );
                out.insert("Persistent".into(), yesno(t.persistent));
                if let Some(u) = &t.unit {
                    out.insert("Unit".into(), u.to_string());
                }
            }
            UnitTypeData::Path(p) => {
                if let Some(u) = &p.unit {
                    out.insert("Unit".into(), u.to_string());
                }
                out.insert("MakeDirectory".into(), yesno(p.make_directory));
            }
            UnitTypeData::Socket(s) => {
                out.insert("Accept".into(), yesno(s.accept));
                if let Some(svc) = &s.service {
                    out.insert("Service".into(), svc.to_string());
                }
            }
            UnitTypeData::Target(_) | UnitTypeData::Scope(_) | UnitTypeData::Other => {}
        }
        // Install info.
        out.insert("WantedBy".into(), install_join(&lu.unit.install.wanted_by));
        out.insert(
            "RequiredBy".into(),
            install_join(&lu.unit.install.required_by),
        );
        Ok(out)
    }

    /// Subscribe to live events.
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<UnitEvent> {
        self.events.subscribe()
    }

    /// Allocate the next job id.
    fn alloc_job_id(&mut self) -> JobId {
        let id = JobId(self.next_job_id);
        self.next_job_id = self.next_job_id.saturating_add(1).max(1);
        id
    }

    /// Start a unit. Returns the queued job's id.
    ///
    /// For service units the actual `runner.start()` work runs in a
    /// background task rather than inline under our caller's lock —
    /// `Type=oneshot` services can run for hours (a daily backup is the
    /// canonical example) and we must not pin the manager's write lock
    /// for the duration. The bus method that called us reply-times out
    /// otherwise, and any other timer that fires during the run gets
    /// blocked behind the lock.
    pub async fn start_unit(&mut self, name: UnitName, mode: JobMode) -> Result<JobId> {
        let id = self.alloc_job_id();
        let _ = self.events.send(UnitEvent::JobNew {
            id,
            unit: name.clone(),
            kind: JobType::Start,
        });
        if name.kind == UnitKind::Service {
            self.spawn_service_start(name, mode, id)?;
            return Ok(id);
        }
        let outcome = self.run_start(&name, mode).await?;
        let _ = self.events.send(UnitEvent::JobRemoved {
            id,
            unit: name,
            outcome,
        });
        Ok(id)
    }

    /// Hand off the service-start work to a tokio task that *does not*
    /// hold the manager's write lock while awaiting the spawned process.
    /// The task acquires the lock briefly twice: once at the end to
    /// publish the final state via `mark_state`, and once to attach the
    /// exit-detection supervisor for long-lived service types.
    fn spawn_service_start(&mut self, name: UnitName, _mode: JobMode, id: JobId) -> Result<()> {
        let runner = self
            .services
            .get(&name)
            .cloned()
            .ok_or_else(|| anyhow!("no runner for {name}"))?;
        // Publish Activating up-front so callers querying `status`
        // immediately see "the start is in flight." Without this, a
        // 10-hour backup would read as `Inactive (dead)` until it
        // finished, since the supervisor's `mark_state` only runs at
        // exit time.
        self.mark_state(&name, ActiveState::Activating, "start");
        let weak = self
            .self_weak
            .clone()
            .ok_or_else(|| anyhow!("manager self-weak not attached"))?;
        let events = self.events.clone();
        tokio::spawn(async move {
            let outcome_result = runner.start().await;
            let active = *runner.state.lock().await;
            let sub = runner.sub.lock().await.clone();
            if let Some(arc) = weak.upgrade() {
                let mut mgr = arc.write().await;
                mgr.mark_state(&name, active, &sub);
                if active == ActiveState::Active
                    && !matches!(
                        runner.svc.service_type,
                        systeml_unit::service::ServiceType::Oneshot
                            | systeml_unit::service::ServiceType::Dbus
                    )
                {
                    if let Some(weak2) = mgr.self_weak.clone() {
                        crate::service::supervise::spawn(weak2, runner.clone(), name.clone());
                    }
                }
            }
            let outcome = match outcome_result {
                Ok(out) if out.error.is_none() => JobOutcome::Done,
                _ => JobOutcome::Failed,
            };
            let _ = events.send(UnitEvent::JobRemoved {
                id,
                unit: name,
                outcome,
            });
        });
        Ok(())
    }

    /// Stop a unit.
    pub async fn stop_unit(&mut self, name: UnitName, _mode: JobMode) -> Result<JobId> {
        let id = self.alloc_job_id();
        let _ = self.events.send(UnitEvent::JobNew {
            id,
            unit: name.clone(),
            kind: JobType::Stop,
        });
        let outcome = self.run_stop(&name).await?;
        let _ = self.events.send(UnitEvent::JobRemoved {
            id,
            unit: name,
            outcome,
        });
        Ok(id)
    }

    /// Restart a unit. Service-kind restarts borrow the same off-the-lock
    /// pattern as `start_unit` so a long-running oneshot's restart
    /// doesn't pin the manager either.
    pub async fn restart_unit(&mut self, name: UnitName, mode: JobMode) -> Result<JobId> {
        let id = self.alloc_job_id();
        let _ = self.events.send(UnitEvent::JobNew {
            id,
            unit: name.clone(),
            kind: JobType::Restart,
        });
        let _ = self.run_stop(&name).await;
        if name.kind == UnitKind::Service {
            self.spawn_service_start(name, mode, id)?;
            return Ok(id);
        }
        let outcome = self.run_start(&name, mode).await?;
        let _ = self.events.send(UnitEvent::JobRemoved {
            id,
            unit: name,
            outcome,
        });
        Ok(id)
    }

    /// Reload a unit.
    pub async fn reload_unit(&mut self, name: UnitName, _mode: JobMode) -> Result<JobId> {
        let id = self.alloc_job_id();
        let _ = self.events.send(UnitEvent::JobNew {
            id,
            unit: name.clone(),
            kind: JobType::Reload,
        });
        let outcome = match self.services.get(&name) {
            Some(r) => match r.reload().await {
                Ok(()) => JobOutcome::Done,
                Err(_) => JobOutcome::Failed,
            },
            None => JobOutcome::Skipped,
        };
        let _ = self.events.send(UnitEvent::JobRemoved {
            id,
            unit: name,
            outcome,
        });
        Ok(id)
    }

    async fn run_start(&mut self, name: &UnitName, _mode: JobMode) -> Result<JobOutcome> {
        // For .service units, drive the runner directly.
        if name.kind == UnitKind::Service {
            let runner = self
                .services
                .get(name)
                .cloned()
                .ok_or_else(|| anyhow!("no runner for {name}"))?;
            let out = runner.start().await?;
            let active = *runner.state.lock().await;
            let sub = runner.sub.lock().await.clone();
            if let Some(st) = self.status.get_mut(name) {
                st.active = active;
                st.sub.clone_from(&sub);
            }
            let _ = self.events.send(UnitEvent::StateChanged {
                unit: name.clone(),
                active,
                sub,
            });
            // If the service is now Active and its lifecycle isn't already
            // resolved by start() (i.e. anything except Type=oneshot, which
            // returns synchronously, and Type=dbus, which we refuse), spawn
            // a supervisor task so unexpected child exit lands in the
            // manager and propagates to subscribers (timers, the bus).
            if active == ActiveState::Active
                && !matches!(
                    runner.svc.service_type,
                    systeml_unit::service::ServiceType::Oneshot
                        | systeml_unit::service::ServiceType::Dbus
                )
            {
                if let Some(weak) = self.self_weak.clone() {
                    crate::service::supervise::spawn(weak, runner.clone(), name.clone());
                } else {
                    warn!(unit = %name,
                        "manager has no self-weak; service exit will not be detected");
                }
            }
            return Ok(if out.error.is_some() {
                JobOutcome::Failed
            } else {
                JobOutcome::Done
            });
        }
        // Targets: simply mark active.
        if name.kind == UnitKind::Target {
            self.mark_state(name, ActiveState::Active, "active");
            return Ok(JobOutcome::Done);
        }
        // Timers/paths/sockets: load-time activation handled by per-type
        // engines; here we just mark active. The sub-state mirrors systemd:
        // a started-but-idle timer/path is "waiting" (it transitions to
        // "running" only while triggering its target); a started socket is
        // "listening".
        let sub = match name.kind {
            UnitKind::Timer | UnitKind::Path => "waiting",
            UnitKind::Socket => "listening",
            _ => "running",
        };
        self.mark_state(name, ActiveState::Active, sub);
        // A newly-active timer needs the scheduler to recompute. We also
        // touch the persistent stamp on first activation, matching
        // systemd's `timer_start` → `touch_file` behavior. Without this,
        // a `Persistent=yes` timer that's never fired has no anchor for
        // catch-up to compare against on subsequent restarts.
        if name.kind == UnitKind::Timer {
            self.stamp_persistent_timer_if_needed(name);
            self.poke_timer_scheduler();
        }
        Ok(JobOutcome::Done)
    }

    fn stamp_persistent_timer_if_needed(&self, name: &UnitName) {
        let Some(lu) = self.units.get(name) else {
            return;
        };
        let UnitTypeData::Timer(t) = &lu.unit.kind else {
            return;
        };
        if !t.persistent {
            return;
        }
        if crate::timer::read_last_fire(name).is_some() {
            return;
        }
        let now = time::OffsetDateTime::now_utc();
        if let Err(e) = crate::timer::write_last_fire(name, now) {
            warn!(unit = %name, error = %e, "failed to stamp persistent timer");
        }
    }

    async fn run_stop(&mut self, name: &UnitName) -> Result<JobOutcome> {
        if name.kind == UnitKind::Service {
            if let Some(r) = self.services.get(name).cloned() {
                let _ = r.stop().await;
                self.mark_state(name, ActiveState::Inactive, "dead");
                return Ok(JobOutcome::Done);
            }
        }
        self.mark_state(name, ActiveState::Inactive, "dead");
        // A newly-inactive timer should drop out of the scheduler's view.
        if name.kind == UnitKind::Timer {
            self.poke_timer_scheduler();
        }
        Ok(JobOutcome::Done)
    }

    /// Enable units (creates `[Install]` symlinks).
    pub fn enable_units(
        &mut self,
        names: &[UnitName],
        runtime: bool,
        force: bool,
    ) -> Result<EnableChanges> {
        let mut total = EnableChanges::default();
        for name in names {
            let install = self
                .units
                .get(name)
                .map(|lu| lu.unit.install.clone())
                .unwrap_or_default();
            let r = install::enable(name, &install, runtime, force)?;
            total.carries_install_info = total.carries_install_info || r.carries_install_info;
            for c in r.changes {
                total.changes.push(UnitFileChange {
                    change_type: c.change_type,
                    target: c.target,
                    source: c.source,
                });
            }
        }
        Ok(total)
    }

    /// Disable units (removes `[Install]` symlinks).
    pub fn disable_units(&mut self, names: &[UnitName], runtime: bool) -> Result<EnableChanges> {
        let mut total = EnableChanges::default();
        for name in names {
            let r = install::disable(name, runtime)?;
            for c in r.changes {
                total.changes.push(UnitFileChange {
                    change_type: c.change_type,
                    target: c.target,
                    source: c.source,
                });
            }
        }
        Ok(total)
    }

    /// Mask units (links unit name to /dev/null).
    pub fn mask_units(
        &mut self,
        names: &[UnitName],
        runtime: bool,
        force: bool,
    ) -> Result<EnableChanges> {
        let mut total = EnableChanges::default();
        for name in names {
            let r = install::mask(name, runtime, force)?;
            for c in r.changes {
                total.changes.push(UnitFileChange {
                    change_type: c.change_type,
                    target: c.target,
                    source: c.source,
                });
            }
        }
        Ok(total)
    }

    /// Unmask units.
    pub fn unmask_units(&mut self, names: &[UnitName], runtime: bool) -> Result<EnableChanges> {
        let mut total = EnableChanges::default();
        for name in names {
            let r = install::unmask(name, runtime)?;
            for c in r.changes {
                total.changes.push(UnitFileChange {
                    change_type: c.change_type,
                    target: c.target,
                    source: c.source,
                });
            }
        }
        Ok(total)
    }

    /// Manually inject a parsed unit (used by daemon-reload helpers + tests).
    pub fn add_parsed(&mut self, name: UnitName, lu: LoadedUnit) {
        self.insert_loaded(name, lu);
    }
}

fn yesno(b: bool) -> String {
    if b {
        "yes".into()
    } else {
        "no".into()
    }
}

fn install_join(set: &std::collections::BTreeSet<UnitName>) -> String {
    set.iter()
        .map(|u| u.to_string())
        .collect::<Vec<_>>()
        .join(" ")
}
