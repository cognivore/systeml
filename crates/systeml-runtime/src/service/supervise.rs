//! Per-service exit watcher.
//!
//! After a `Type=simple|exec|idle|notify|notify-reload|forking` service
//! reaches `Active`, the manager spawns one supervisor task per running
//! unit. The supervisor:
//!
//! 1. Owns the `tokio::process::Child` (taken out of `ServiceRunner.child`)
//!    or, for `Type=forking`, the daemon's PID. Direct children are awaited
//!    via `Child::wait`; forked daemons are detected via periodic
//!    `kill(pid, 0)` polling, since the daemon is a grandchild that the
//!    parent reaped.
//! 2. On exit, classifies the status, updates the runner's `state`/`sub`,
//!    and calls `Manager::mark_state` so subscribers (timers, the bus) see
//!    `StateChanged` immediately.
//! 3. Bails out without touching state if `ServiceRunner.is_stopping()` is
//!    set — the explicit `stop()` path will set the final state itself.
//!
//! This is the moral equivalent of systemd's `service_sigchld_event`: a
//! single point where unexpected child death turns into a unit state
//! transition. Without this, a `Type=simple` service that crashes would
//! stay reported as `Active/running` forever.

use crate::manager::Manager;
use crate::service::ServiceRunner;
use crate::state::ActiveState;
use std::sync::{Arc, Weak};
use std::time::Duration;
use systeml_unit::service::ServiceType;
use systeml_unit::UnitName;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

/// Spawn a supervisor task for `runner`. Returns immediately; the task runs
/// until the underlying process exits or [`ServiceRunner::request_stop`]
/// is signalled.
pub fn spawn(manager: Weak<RwLock<Manager>>, runner: Arc<ServiceRunner>, name: UnitName) {
    tokio::spawn(supervise(manager, runner, name));
}

async fn supervise(manager: Weak<RwLock<Manager>>, runner: Arc<ServiceRunner>, name: UnitName) {
    debug!(unit = %name, "service supervisor started");
    let exit = wait_for_exit(&runner).await;
    if runner.is_stopping() {
        debug!(unit = %name, "supervisor: stop requested; leaving final state to stop()");
        return;
    }

    // Classify and update the runner's local state.
    let (active, sub) = match exit {
        ExitObservation::Clean => (ActiveState::Inactive, "dead"),
        ExitObservation::Failed => (ActiveState::Failed, "failed"),
        ExitObservation::NoChild => {
            // Nothing to watch (Type=oneshot reached this path or the
            // child was already taken). Don't touch state.
            return;
        }
    };
    *runner.state.lock().await = active;
    *runner.sub.lock().await = sub.into();
    *runner.main_pid.lock().await = None;

    info!(unit = %name, ?active, sub, "service exited");

    let Some(arc) = manager.upgrade() else {
        // Manager is gone (daemon shutting down). Nothing to broadcast.
        return;
    };
    let mut mgr = arc.write().await;
    mgr.mark_state(&name, active, sub);
}

enum ExitObservation {
    Clean,
    Failed,
    NoChild,
}

async fn wait_for_exit(runner: &ServiceRunner) -> ExitObservation {
    match runner.svc.service_type {
        ServiceType::Forking => wait_forking(runner).await,
        ServiceType::Oneshot => {
            // Oneshot lifecycle is fully resolved inside `runner.start()`.
            // The supervisor should never be spawned for oneshot.
            ExitObservation::NoChild
        }
        ServiceType::Dbus => {
            // Dbus is rejected at start time; supervisor must never be
            // spawned for it.
            ExitObservation::NoChild
        }
        ServiceType::Simple
        | ServiceType::Exec
        | ServiceType::Idle
        | ServiceType::Notify
        | ServiceType::NotifyReload => wait_direct_child(runner).await,
    }
}

/// Direct-child types: take the `Child` out of the runner and await its
/// exit. If another path (a racing `stop()`) already took it, fall back to
/// PID-based waiting against the recorded `main_pid`.
async fn wait_direct_child(runner: &ServiceRunner) -> ExitObservation {
    let mut child = runner.child.lock().await.take();
    if let Some(child) = child.as_mut() {
        match child.wait().await {
            Ok(status) => {
                use std::os::unix::process::ExitStatusExt;
                if status.success() {
                    return ExitObservation::Clean;
                }
                if status.code().is_some() || status.signal().is_some() {
                    return ExitObservation::Failed;
                }
                return ExitObservation::Failed;
            }
            Err(e) => {
                warn!(error = %e, "child.wait() failed; falling back to pid poll");
            }
        }
    }
    // No Child handle (or wait failed) — poll the recorded PID.
    let pid = *runner.main_pid.lock().await;
    match pid {
        Some(p) => poll_pid_gone(p).await,
        None => ExitObservation::NoChild,
    }
}

/// `Type=forking`: the daemon is a grandchild. We have its PID (read from
/// `PIDFile=` during `start_forking`) but no `Child` handle. Poll
/// `kill(pid, 0)` until the kernel reports `ESRCH`.
async fn wait_forking(runner: &ServiceRunner) -> ExitObservation {
    let pid = *runner.main_pid.lock().await;
    match pid {
        Some(p) => poll_pid_gone(p).await,
        None => ExitObservation::NoChild,
    }
}

/// Poll `kill(pid, 0)` every second until the kernel reports the process
/// is gone. Always returns `Failed` if the process was killed by signal,
/// `Clean` only when we observe a graceful disappearance — but `kill(pid,
/// 0)` doesn't surface exit codes, so this returns `Clean` for any
/// disappearance and lets the caller (or the user inspecting the journal)
/// figure out whether the daemon really succeeded. This is consistent with
/// systemd's behavior for `Type=forking`: without cgroup membership, the
/// main-PID exit only tells you "gone," not "succeeded."
async fn poll_pid_gone(pid: i32) -> ExitObservation {
    loop {
        match nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None) {
            Ok(()) => {
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            Err(_) => return ExitObservation::Clean,
        }
    }
}
