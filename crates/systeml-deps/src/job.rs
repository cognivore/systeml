//! Job types and IDs.
//!
//! A [`Job`] is one (unit, action, mode) triple queued in the manager. The
//! transaction engine produces a `Vec<Job>`; the runtime executes them in
//! the order returned. IDs are allocated centrally via [`JobIdAlloc`] so that
//! job lifetimes are observable across crates without sharing mutable state.

use std::sync::atomic::{AtomicU32, Ordering};
use systeml_unit::UnitName;

/// Opaque, monotonically-allocated job id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct JobId(pub u32);

impl JobId {
    /// Sentinel placeholder used internally by the transaction builder
    /// before [`JobIdAlloc::alloc`] is called. The runtime never sees this
    /// value because `Transaction::build` finalises ids before returning.
    pub const PLACEHOLDER: Self = Self(0);
}

impl std::fmt::Display for JobId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Atomic id allocator. Hand one to the transaction builder.
///
/// Wraps an `AtomicU32` starting at 1. `JobId(0)` is reserved as a sentinel.
#[derive(Debug, Default)]
pub struct JobIdAlloc {
    next: AtomicU32,
}

impl JobIdAlloc {
    /// New allocator starting at 1 (skips the sentinel).
    #[must_use]
    pub fn new() -> Self {
        Self {
            next: AtomicU32::new(1),
        }
    }

    /// New allocator starting at the given value. Useful for resuming after
    /// a daemon restart with persisted job ids.
    #[must_use]
    pub fn starting_at(start: u32) -> Self {
        Self {
            next: AtomicU32::new(start.max(1)),
        }
    }

    /// Allocate a fresh id.
    pub fn alloc(&self) -> JobId {
        JobId(self.next.fetch_add(1, Ordering::Relaxed))
    }
}

/// What action a job performs on a unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum JobType {
    /// Activate.
    Start,
    /// Deactivate.
    Stop,
    /// Stop then start.
    Restart,
    /// Restart only if currently active; if inactive, do nothing.
    TryRestart,
    /// Reload (`ExecReload=`).
    Reload,
    /// Reload if active else start.
    ReloadOrStart,
    /// Verify the unit is active. Fails if inactive — never starts.
    VerifyActive,
    /// No-op placeholder used for ordering / collapsed propagation.
    Nop,
}

impl JobType {
    /// True if this job will, on success, leave the unit active.
    pub fn results_in_active(self) -> bool {
        matches!(
            self,
            Self::Start | Self::Restart | Self::ReloadOrStart | Self::Reload | Self::VerifyActive
        )
    }

    /// True if this job will, on success, leave the unit inactive.
    pub fn results_in_inactive(self) -> bool {
        matches!(self, Self::Stop)
    }

    /// systemd's `job_type_collapse` for the case where a `TryRestart` is
    /// scheduled against an inactive unit: collapse to `Nop`.
    pub fn collapse_for_inactive(self) -> Self {
        match self {
            Self::TryRestart => Self::Nop,
            other => other,
        }
    }
}

/// systemd `--mode=` semantic for how a new job interacts with existing ones.
///
/// See `man systemctl` "Mode" section, and `transaction.c`'s
/// `transaction_apply` for the precise dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum JobMode {
    /// Refuse if existing jobs would be replaced.
    Fail,
    /// Replace conflicting jobs (default).
    #[default]
    Replace,
    /// Like `Replace` but also force-cancel `Irreversible` jobs.
    ReplaceIrreversibly,
    /// Stop everything not in the dependency closure of the new root.
    /// Only valid for units with `AllowIsolate=yes`.
    Isolate,
    /// Like `Replace` but flush queued jobs first.
    Flush,
    /// Bypass all dependency expansion.
    IgnoreDependencies,
    /// Expand pull dependencies but skip ordering.
    IgnoreRequirements,
}

/// Final outcome of a job once it completes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobOutcome {
    /// Job ran to completion successfully.
    Done,
    /// Job was cancelled by another job.
    Canceled,
    /// Timed out.
    Timeout,
    /// Failed (process exit, condition false, etc).
    Failed,
    /// Dependency failed and propagated.
    DependencyFailed,
    /// Skipped because a `Condition*=` was not met.
    Skipped,
}

/// One queued or in-flight job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    /// Unique job id.
    pub id: JobId,
    /// Target unit name.
    pub unit: UnitName,
    /// What action.
    pub kind: JobType,
    /// Conflict resolution mode.
    pub mode: JobMode,
}

impl Job {
    /// Construct a new job with the given fields.
    pub fn new(id: JobId, unit: UnitName, kind: JobType, mode: JobMode) -> Self {
        Self {
            id,
            unit,
            kind,
            mode,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alloc_monotonic() {
        let a = JobIdAlloc::new();
        assert_eq!(a.alloc(), JobId(1));
        assert_eq!(a.alloc(), JobId(2));
        assert_eq!(a.alloc(), JobId(3));
    }

    #[test]
    fn alloc_starting_at_skips_sentinel() {
        let a = JobIdAlloc::starting_at(0);
        assert_eq!(a.alloc(), JobId(1));
    }

    #[test]
    fn try_restart_collapses_to_nop() {
        assert_eq!(JobType::TryRestart.collapse_for_inactive(), JobType::Nop);
        assert_eq!(JobType::Start.collapse_for_inactive(), JobType::Start);
    }
}
