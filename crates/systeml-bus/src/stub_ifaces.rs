//! Empty placeholder interfaces for unit types whose properties we don't
//! populate yet. Their only job is to satisfy
//! `org.freedesktop.DBus.Properties.GetAll` calls from upstream tools so
//! that `systemctl status` etc. don't error out with `UnknownInterface`.
//!
//! These will be fleshed out per Phase 2.

/// `.socket` placeholder.
#[derive(Debug, Default, Clone, Copy)]
pub struct SocketStub;

#[zbus::interface(name = "org.freedesktop.systemd1.Socket")]
impl SocketStub {
    /// Number of accepted connections so far.
    #[zbus(property)]
    fn n_accepted(&self) -> u32 {
        0
    }

    /// Number of currently connected peers.
    #[zbus(property)]
    fn n_connections(&self) -> u32 {
        0
    }

    /// Result of last activation.
    #[zbus(property)]
    fn result(&self) -> String {
        // TODO(systemctl-compat): real socket result tracking.
        "success".to_owned()
    }
}

/// `.timer` placeholder.
#[derive(Debug, Default, Clone, Copy)]
pub struct TimerStub;

#[zbus::interface(name = "org.freedesktop.systemd1.Timer")]
impl TimerStub {
    /// Microseconds until next elapse (UTC).
    #[zbus(property, name = "NextElapseUSecRealtime")]
    fn next_elapse_u_sec_realtime(&self) -> u64 {
        // TODO(systemctl-compat): pull from timer engine.
        0
    }

    /// Microseconds until next elapse (monotonic).
    #[zbus(property, name = "NextElapseUSecMonotonic")]
    fn next_elapse_u_sec_monotonic(&self) -> u64 {
        // TODO(systemctl-compat): pull from timer engine.
        0
    }

    /// Last time the timer fired.
    #[zbus(property, name = "LastTriggerUSec")]
    fn last_trigger_u_sec(&self) -> u64 {
        // TODO(systemctl-compat): persist last fire timestamp.
        0
    }

    /// Result of last activation.
    #[zbus(property)]
    fn result(&self) -> String {
        // TODO(systemctl-compat): real timer result tracking.
        "success".to_owned()
    }
}

/// `.path` placeholder.
#[derive(Debug, Default, Clone, Copy)]
pub struct PathStub;

#[zbus::interface(name = "org.freedesktop.systemd1.Path")]
impl PathStub {
    /// Result of last activation.
    #[zbus(property)]
    fn result(&self) -> String {
        // TODO(systemctl-compat): real path result tracking.
        "success".to_owned()
    }
}

/// `.target` placeholder. Targets have no per-type properties beyond `Unit`.
#[derive(Debug, Default, Clone, Copy)]
pub struct TargetStub;

#[zbus::interface(name = "org.freedesktop.systemd1.Target")]
impl TargetStub {
    /// systemd exposes no `.target`-specific properties; this getter exists
    /// solely so the empty interface is well-formed.
    #[zbus(property, name = "TargetVersion")]
    fn target_version(&self) -> u32 {
        0
    }
}

/// `.scope` placeholder.
#[derive(Debug, Default, Clone, Copy)]
pub struct ScopeStub;

#[zbus::interface(name = "org.freedesktop.systemd1.Scope")]
impl ScopeStub {
    /// Result of last operation.
    #[zbus(property)]
    fn result(&self) -> String {
        "success".to_owned()
    }
}
