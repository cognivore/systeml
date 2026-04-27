//! Bridge from `systeml_runtime::manager::UnitEvent` broadcast channel onto
//! D-Bus signals.
//!
//! One `EventBridge` is created per accepted connection — when the event
//! fires, we forward it as a signal addressed at the connection's manager
//! object. Dropping the bridge cancels the forwarding task.

use crate::manager_iface::ManagerIface;
use crate::{unit_object_path, MANAGER_PATH};
use std::sync::Arc;
use systeml_runtime::manager::UnitEvent;
use systeml_runtime::Manager;
use tokio::sync::RwLock;
use tokio::task::JoinHandle;
use zbus::object_server::SignalContext;
use zbus::Connection;

/// Holds the spawned task that translates events to signals. Drop to stop.
pub struct EventBridge {
    handle: JoinHandle<()>,
}

impl EventBridge {
    /// Subscribe to the manager's broadcast channel and start forwarding.
    pub fn spawn(conn: Connection, manager: Arc<RwLock<Manager>>) -> Self {
        let handle = tokio::spawn(async move {
            let rx = {
                let m = manager.read().await;
                m.subscribe()
            };
            let mut rx = rx;
            let ctxt = match SignalContext::new(&conn, MANAGER_PATH) {
                Ok(c) => c.into_owned(),
                Err(e) => {
                    tracing::warn!(error = %e, "event bridge: bad manager path");
                    return;
                }
            };
            loop {
                match rx.recv().await {
                    Ok(ev) => {
                        if let Err(e) = forward(&ctxt, ev).await {
                            tracing::debug!(error = %e, "event bridge: signal emit failed");
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(skipped = n, "event bridge: receiver lagged");
                    }
                }
            }
        });
        Self { handle }
    }
}

impl Drop for EventBridge {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

async fn forward(ctxt: &SignalContext<'static>, ev: UnitEvent) -> zbus::Result<()> {
    match ev {
        UnitEvent::UnitNew(name) => {
            let path = unit_object_path(&name);
            ManagerIface::unit_new(ctxt, name.to_string(), path).await
        }
        UnitEvent::UnitRemoved(name) => {
            let path = unit_object_path(&name);
            ManagerIface::unit_removed(ctxt, name.to_string(), path).await
        }
        UnitEvent::JobNew { id, unit, .. } => {
            // TODO(systemctl-compat): real per-job object path. For now we
            // re-use the unit path to give clients _something_ identifying.
            let path = unit_object_path(&unit);
            ManagerIface::job_new(ctxt, id.0, path, unit.to_string()).await
        }
        UnitEvent::JobRemoved { id, unit, outcome } => {
            let path = unit_object_path(&unit);
            let result = job_outcome_str(outcome).to_owned();
            ManagerIface::job_removed(ctxt, id.0, path, unit.to_string(), result).await
        }
        UnitEvent::StateChanged { .. } => {
            // State transitions do not have a dedicated bus signal; clients
            // listen for `PropertiesChanged` on the unit interface instead.
            // TODO(systemctl-compat): emit `PropertiesChanged` for
            // `ActiveState` / `SubState` on the affected unit object.
            Ok(())
        }
    }
}

fn job_outcome_str(o: systeml_deps::JobOutcome) -> &'static str {
    use systeml_deps::JobOutcome as J;
    match o {
        J::Done => "done",
        J::Canceled => "canceled",
        J::Timeout => "timeout",
        J::Failed => "failed",
        J::DependencyFailed => "dependency",
        J::Skipped => "skipped",
    }
}
