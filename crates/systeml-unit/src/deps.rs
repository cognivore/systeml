//! `[Unit]` dependency declarations.

use crate::name::UnitName;
use std::collections::BTreeSet;

/// All dependency relations declared in a unit's `[Unit]` section.
///
/// `After=`/`Before=` are pure ordering. `Wants=`/`Requires=`/`Requisite=`/
/// `BindsTo=`/`PartOf=`/`Upholds=` are pull relations of varying strictness.
/// `Conflicts=` is mutual exclusion. `On{Failure,Success}=` are propagation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct UnitDeps {
    /// Order: this unit starts after these.
    pub after: BTreeSet<UnitName>,
    /// Order: this unit starts before these.
    pub before: BTreeSet<UnitName>,

    /// Soft pull: bring these up when this is brought up; no failure
    /// propagation.
    pub wants: BTreeSet<UnitName>,
    /// Hard pull: bring these up; if any fails, this fails.
    pub requires: BTreeSet<UnitName>,
    /// Like `Requires=` but immediate — the targets must already be active.
    pub requisite: BTreeSet<UnitName>,
    /// Mutual lifecycle: if any target fails or stops, this stops.
    pub binds_to: BTreeSet<UnitName>,
    /// Reverse `Requires=`: if this stops, the target stops.
    pub part_of: BTreeSet<UnitName>,
    /// Soft sticky pull: keep target up while this references it.
    pub upholds: BTreeSet<UnitName>,

    /// Conflict: starting this stops these.
    pub conflicts: BTreeSet<UnitName>,

    /// Activate these when this fails.
    pub on_failure: BTreeSet<UnitName>,
    /// Activate these when this succeeds (oneshot).
    pub on_success: BTreeSet<UnitName>,

    /// When this is reloaded, propagate reload to these.
    pub propagates_reload_to: BTreeSet<UnitName>,
    /// Receive reload propagation from these.
    pub reload_propagated_from: BTreeSet<UnitName>,
    /// When this stops, also stop these.
    pub propagates_stop_to: BTreeSet<UnitName>,
    /// Receive stop propagation from these.
    pub stop_propagated_from: BTreeSet<UnitName>,

    /// `JoinsNamespaceOf=` (Linux-only; we accept and ignore).
    pub joins_namespace_of: BTreeSet<UnitName>,
}

impl UnitDeps {
    /// All dep names referenced anywhere in this struct.
    pub fn all(&self) -> impl Iterator<Item = &UnitName> {
        self.after
            .iter()
            .chain(&self.before)
            .chain(&self.wants)
            .chain(&self.requires)
            .chain(&self.requisite)
            .chain(&self.binds_to)
            .chain(&self.part_of)
            .chain(&self.upholds)
            .chain(&self.conflicts)
            .chain(&self.on_failure)
            .chain(&self.on_success)
            .chain(&self.propagates_reload_to)
            .chain(&self.reload_propagated_from)
            .chain(&self.propagates_stop_to)
            .chain(&self.stop_propagated_from)
            .chain(&self.joins_namespace_of)
    }
}
