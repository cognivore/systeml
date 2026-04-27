//! Transaction builder.
//!
//! Given a [`DepGraph`], a [`ManagerView`], and a root job request, produce
//! a topologically-sorted [`Transaction`] of [`Job`]s that respects the same
//! dependency expansion rules as systemd's
//! `transaction_add_job_and_dependencies` (see
//! `systemd-stable/src/core/transaction.c`).
//!
//! # Expansion rules
//!
//! - `Start unit`: also start every `Wants=`/`Requires=`/`BindsTo=`
//!   target; verify-active for `Requisite=`; stop every `Conflicts=` target.
//! - `Stop unit`: also stop everything that `BindsTo=` it (reverse), every
//!   `PartOf=`-source, and follow `PropagatesStopTo=`.
//! - `Restart unit`: stop+start the unit, plus propagate try-restart to
//!   units with `PartOf=this`.
//! - `Reload unit`: only the reload, plus a reload to every
//!   `PropagatesReloadTo=` target.
//! - `VerifyActive unit`: no expansion; collapses to `Nop` if already active.
//!
//! # Modes
//!
//! - `Fail`: refuse if any new job would replace an existing one for the
//!   same unit with a different kind.
//! - `Replace` / `ReplaceIrreversibly`: cancel conflicting existing jobs.
//! - `Isolate`: queue a `Stop` for every loaded unit not in the dep closure
//!   of the root and not `IgnoreOnIsolate=yes`. Requires `AllowIsolate=yes`
//!   on the root.
//! - `Flush`: drop all queued jobs first.
//! - `IgnoreDependencies`: emit only the root job.
//! - `IgnoreRequirements`: expand pull deps but skip ordering.

use crate::graph::{DepEdge, DepGraph};
use crate::job::{Job, JobId, JobIdAlloc, JobMode, JobType};
use indexmap::{IndexMap, IndexSet};
use systeml_unit::UnitName;
use thiserror::Error;
use tracing::debug;

/// A read-only view of the manager state needed to build a transaction.
///
/// The runtime crate impls this on its real `Manager`. Tests in this crate
/// use a tiny in-memory mock.
pub trait ManagerView {
    /// Whether the unit is loaded (parsed and registered).
    fn is_loaded(&self, name: &UnitName) -> bool;
    /// Whether the unit is currently active.
    fn is_active(&self, name: &UnitName) -> bool;
    /// `AllowIsolate=yes` on the unit.
    fn allow_isolate(&self, name: &UnitName) -> bool;
    /// Existing queued or in-flight jobs.
    fn existing_jobs(&self) -> &[Job];
}

/// A built transaction.
#[derive(Debug, Default, Clone)]
pub struct Transaction {
    /// Jobs in dependency-respecting order. Run head-to-tail.
    pub jobs: Vec<Job>,
    /// Existing job ids the runtime should cancel before applying.
    pub cancel: Vec<JobId>,
}

/// Errors that prevent a transaction from being built.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum TransactionError {
    /// Cyclic ordering dependency (`After=` chain).
    #[error("cyclic dependency involving {0}")]
    Cycle(UnitName),
    /// Unit not loaded.
    #[error("unit {0} is not loaded")]
    NotLoaded(UnitName),
    /// Conflicting job already exists.
    #[error("conflicting job exists for {0}")]
    Conflict(UnitName),
    /// Mode is `Fail` and dependencies would require replacing existing jobs.
    #[error("would require replacing existing jobs (mode=fail)")]
    WouldReplaceFail,
    /// `Isolate` requested on a unit without `AllowIsolate=yes`.
    #[error("isolate not allowed for {0}")]
    IsolateNotAllowed(UnitName),
    /// Requested job type is not applicable to this unit.
    #[error("job type {1:?} not applicable to {0}")]
    NotApplicable(UnitName, JobType),
}

impl Transaction {
    /// Build a transaction from a root job request.
    ///
    /// `alloc` is consumed for ids; pass the manager's shared
    /// [`JobIdAlloc`].
    pub fn build<M: ManagerView>(
        graph: &DepGraph,
        manager: &M,
        alloc: &JobIdAlloc,
        root_unit: UnitName,
        root_kind: JobType,
        mode: JobMode,
    ) -> Result<Self, TransactionError> {
        debug!(?root_unit, ?root_kind, ?mode, "transaction.build");

        // VerifyActive collapses to a no-op transaction if the unit is
        // already active (matches systemd: the verify is a precondition,
        // not an action).
        if root_kind == JobType::VerifyActive && manager.is_active(&root_unit) {
            return Ok(Self::default());
        }

        if !manager.is_loaded(&root_unit) {
            return Err(TransactionError::NotLoaded(root_unit));
        }

        if mode == JobMode::Isolate && !manager.allow_isolate(&root_unit) {
            return Err(TransactionError::IsolateNotAllowed(root_unit));
        }

        let mut builder = TxBuilder::new(graph, manager, mode);
        builder.add_job_and_deps(root_unit.clone(), root_kind, /*via*/ None)?;

        if mode == JobMode::Isolate {
            builder.add_isolate_stops(&root_unit);
        }

        // Resolve mode interactions with existing jobs.
        let cancel = builder.apply_mode(mode)?;

        // Topologically sort the resulting jobs.
        let names: Vec<UnitName> = builder.jobs.keys().cloned().collect();
        let order = if mode == JobMode::IgnoreRequirements {
            names
        } else {
            graph
                .topo_order(&names)
                .map_err(TransactionError::Cycle)?
        };

        let mut jobs = Vec::with_capacity(order.len());
        for name in order {
            if let Some(mut j) = builder.jobs.shift_remove(&name) {
                j.id = alloc.alloc();
                j.mode = mode;
                jobs.push(j);
            }
        }

        Ok(Self { jobs, cancel })
    }

    /// Convenience: build a single-job transaction without expansion. Used
    /// by tests and the bus dispatcher when a caller explicitly wants
    /// `JobMode::IgnoreDependencies`.
    pub fn root(unit: UnitName, kind: JobType, mode: JobMode) -> Self {
        let mut t = Self::default();
        t.jobs.push(Job::new(JobId::PLACEHOLDER, unit, kind, mode));
        t
    }
}

// ---------------- internal builder ----------------

struct TxBuilder<'a, M: ManagerView> {
    graph: &'a DepGraph,
    manager: &'a M,
    mode: JobMode,
    /// Jobs being assembled, keyed by unit name. We dedupe at this level —
    /// adding the same unit twice keeps the strongest job kind (mirrors
    /// systemd's job-type merging).
    jobs: IndexMap<UnitName, Job>,
}

impl<'a, M: ManagerView> TxBuilder<'a, M> {
    fn new(graph: &'a DepGraph, manager: &'a M, mode: JobMode) -> Self {
        Self {
            graph,
            manager,
            mode,
            jobs: IndexMap::new(),
        }
    }

    /// Insert or merge a job for `unit`. Returns true if newly inserted.
    fn insert_job(&mut self, unit: UnitName, kind: JobType) -> bool {
        // Collapse TryRestart on an inactive unit to Nop (we still record it
        // so ordering is preserved if anything else attaches via After/Before).
        let kind = if kind == JobType::TryRestart && !self.manager.is_active(&unit) {
            JobType::Nop
        } else {
            kind
        };

        if let Some(existing) = self.jobs.get_mut(&unit) {
            // Merge: same kind → no-op; conflicting kinds → upgrade Stop+Start
            // to Restart, anything+Nop → keep stronger.
            existing.kind = merge_kinds(existing.kind, kind);
            return false;
        }

        self.jobs.insert(
            unit.clone(),
            Job::new(JobId::PLACEHOLDER, unit, kind, self.mode),
        );
        true
    }

    /// Add a job and recursively expand its dependencies according to
    /// systemd's expansion rules.
    fn add_job_and_deps(
        &mut self,
        unit: UnitName,
        kind: JobType,
        _via: Option<&UnitName>,
    ) -> Result<(), TransactionError> {
        if !self.manager.is_loaded(&unit) {
            // VerifyActive always demands existence; for soft pulls
            // (Wants), a missing unit is silently dropped — same as
            // systemd's `-EBADR`/`-ENOENT` swallow.
            return Err(TransactionError::NotLoaded(unit));
        }

        let is_new = self.insert_job(unit.clone(), kind);
        if !is_new {
            return Ok(());
        }

        if self.mode == JobMode::IgnoreDependencies {
            return Ok(());
        }

        match kind {
            JobType::Start | JobType::ReloadOrStart => {
                self.expand_start(&unit)?;
            }
            JobType::Restart => {
                self.expand_start(&unit)?;
                self.expand_stop_propagation(&unit, /*restart=*/ true);
            }
            JobType::Stop => {
                self.expand_stop(&unit);
            }
            JobType::TryRestart => {
                // Only expand if the unit is currently active; otherwise the
                // whole sub-job is a Nop and there's nothing to propagate.
                if self.manager.is_active(&unit) {
                    self.expand_start(&unit)?;
                    self.expand_stop_propagation(&unit, /*restart=*/ true);
                }
            }
            JobType::Reload => {
                self.expand_reload(&unit);
            }
            JobType::VerifyActive | JobType::Nop => {
                // No expansion needed.
            }
        }

        Ok(())
    }

    /// Pull-in expansion for Start/Restart/ReloadOrStart.
    fn expand_start(&mut self, unit: &UnitName) -> Result<(), TransactionError> {
        // Snapshot edges to avoid borrow issues during recursion.
        let edges: Vec<(DepEdge, UnitName)> = self
            .graph
            .outgoing(unit)
            .map(|(e, n)| (e, n.clone()))
            .collect();

        for (edge, target) in edges {
            match edge {
                DepEdge::Requires | DepEdge::BindsTo => {
                    // Hard pull — propagate failure.
                    if self.manager.is_loaded(&target) {
                        self.add_job_and_deps(target, JobType::Start, Some(unit))?;
                    } else {
                        return Err(TransactionError::NotLoaded(target));
                    }
                }
                DepEdge::Wants | DepEdge::Upholds if self.manager.is_loaded(&target) => {
                    // Soft pull — missing target is ignored, soft errors swallowed.
                    if let Err(e) = self.add_job_and_deps(target, JobType::Start, Some(unit)) {
                        debug!(?e, "soft pull failed; ignoring");
                    }
                }
                DepEdge::Requisite => {
                    self.add_job_and_deps(target, JobType::VerifyActive, Some(unit))?;
                }
                DepEdge::Conflicts if self.manager.is_loaded(&target) => {
                    // Starting this stops the conflict target.
                    self.add_job_and_deps(target, JobType::Stop, Some(unit))?;
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Reverse-edge expansion for Stop.
    ///
    /// We stop the unit, plus everything that has `BindsTo=this` (reverse
    /// BindsTo edge into `unit`), everything `PartOf=this`, and anything
    /// reached by `PropagatesStopTo=` (forward).
    fn expand_stop(&mut self, unit: &UnitName) {
        // Reverse: anyone with `BindsTo=unit` must also stop.
        let inbound: Vec<(DepEdge, UnitName)> = self
            .graph
            .incoming(unit)
            .map(|(e, n)| (e, n.clone()))
            .collect();
        for (edge, source) in &inbound {
            if matches!(edge, DepEdge::BindsTo | DepEdge::PartOf)
                && self.manager.is_loaded(source)
            {
                let _ = self.add_job_and_deps(source.clone(), JobType::Stop, Some(unit));
            }
        }

        // Forward: PropagatesStopTo on this unit.
        let outbound: Vec<(DepEdge, UnitName)> = self
            .graph
            .outgoing(unit)
            .map(|(e, n)| (e, n.clone()))
            .collect();
        for (edge, target) in &outbound {
            if matches!(edge, DepEdge::PropagatesStopTo) && self.manager.is_loaded(target) {
                let _ = self.add_job_and_deps(target.clone(), JobType::Stop, Some(unit));
            }
        }
    }

    /// Restart-style propagation: anyone with `PartOf=this` gets a TryRestart.
    fn expand_stop_propagation(&mut self, unit: &UnitName, restart: bool) {
        let inbound: Vec<(DepEdge, UnitName)> = self
            .graph
            .incoming(unit)
            .map(|(e, n)| (e, n.clone()))
            .collect();
        for (edge, source) in &inbound {
            if matches!(edge, DepEdge::PartOf) && self.manager.is_loaded(source) {
                let kind = if restart { JobType::TryRestart } else { JobType::Stop };
                let _ = self.add_job_and_deps(source.clone(), kind, Some(unit));
            }
        }
    }

    /// `PropagatesReloadTo=` cascade for a Reload.
    fn expand_reload(&mut self, unit: &UnitName) {
        let outbound: Vec<(DepEdge, UnitName)> = self
            .graph
            .outgoing(unit)
            .map(|(e, n)| (e, n.clone()))
            .collect();
        for (edge, target) in &outbound {
            if matches!(edge, DepEdge::PropagatesReloadTo) && self.manager.is_loaded(target) {
                let _ = self.add_job_and_deps(target.clone(), JobType::Reload, Some(unit));
            }
        }
    }

    /// Compute and queue stops for everything outside the dep closure for
    /// `JobMode::Isolate`.
    ///
    /// Closure is the set of nodes reachable from `root` via pull edges
    /// (`Wants`/`Requires`/`BindsTo`/`Upholds`) or ordering edges. Anything
    /// loaded but not in the closure and not `IgnoreOnIsolate=yes` becomes
    /// a `Stop`.
    fn add_isolate_stops(&mut self, root: &UnitName) {
        let closure = self.dep_closure(root);
        let mut to_stop: Vec<UnitName> = Vec::new();
        for n in self.graph.nodes() {
            if !self.manager.is_loaded(n) {
                continue;
            }
            if closure.contains(n) {
                continue;
            }
            if self.graph.ignore_on_isolate(n) {
                continue;
            }
            if !self.manager.is_active(n) {
                continue;
            }
            to_stop.push(n.clone());
        }
        for n in to_stop {
            // Use Stop directly; isolate stops do not cascade further.
            let prior = self.mode;
            self.mode = JobMode::Replace;
            let _ = self.add_job_and_deps(n, JobType::Stop, Some(root));
            self.mode = prior;
        }
    }

    /// Pull-edge closure starting at `root`.
    fn dep_closure(&self, root: &UnitName) -> IndexSet<UnitName> {
        let mut seen: IndexSet<UnitName> = IndexSet::new();
        let mut stack = vec![root.clone()];
        while let Some(u) = stack.pop() {
            if !seen.insert(u.clone()) {
                continue;
            }
            for (edge, target) in self.graph.outgoing(&u) {
                if matches!(
                    edge,
                    DepEdge::Wants
                        | DepEdge::Requires
                        | DepEdge::BindsTo
                        | DepEdge::Upholds
                        | DepEdge::Requisite
                        | DepEdge::PartOf
                        | DepEdge::After
                        | DepEdge::Before
                ) {
                    stack.push(target.clone());
                }
            }
        }
        seen
    }

    /// Resolve interaction with existing jobs based on `JobMode`. Returns
    /// the list of existing job ids the runtime should cancel before
    /// applying this transaction.
    fn apply_mode(&mut self, mode: JobMode) -> Result<Vec<JobId>, TransactionError> {
        let existing = self.manager.existing_jobs();
        let mut cancel = Vec::new();

        match mode {
            JobMode::Flush => {
                cancel.extend(existing.iter().map(|j| j.id));
            }
            JobMode::Fail => {
                for ej in existing {
                    if let Some(new) = self.jobs.get(&ej.unit) {
                        if new.kind != ej.kind {
                            return Err(TransactionError::WouldReplaceFail);
                        }
                    }
                }
            }
            JobMode::Replace | JobMode::ReplaceIrreversibly | JobMode::Isolate => {
                for ej in existing {
                    if let Some(new) = self.jobs.get(&ej.unit) {
                        if new.kind != ej.kind {
                            cancel.push(ej.id);
                        }
                    }
                }
            }
            JobMode::IgnoreDependencies | JobMode::IgnoreRequirements => {
                // No interaction with existing jobs beyond same-unit
                // replacement, which Replace-style mode would do too.
                for ej in existing {
                    if let Some(new) = self.jobs.get(&ej.unit) {
                        if new.kind != ej.kind {
                            cancel.push(ej.id);
                        }
                    }
                }
            }
        }
        Ok(cancel)
    }
}

/// Merge two job kinds for the same unit. Used when the expansion visits a
/// unit twice. Mirrors `job_type_merge` in `systemd/src/core/job.c` for the
/// kinds we care about; full-systemd has many more cases (Reload+Restart =
/// Restart, etc.).
fn merge_kinds(a: JobType, b: JobType) -> JobType {
    use JobType::{Nop, Reload, ReloadOrStart, Restart, Start, Stop, TryRestart, VerifyActive};
    match (a, b) {
        (x, Nop) | (Nop, x) => x,
        (x, y) if x == y => x,
        // Stop+Start = Restart, in either order.
        (Stop, Start) | (Start, Stop) => Restart,
        // Restart subsumes Reload, Start, TryRestart.
        (Restart, _) | (_, Restart) => Restart,
        // Reload+Start = ReloadOrStart.
        (Reload, Start) | (Start, Reload) => ReloadOrStart,
        // VerifyActive is the weakest assertion: anything that activates
        // strictly subsumes it.
        (VerifyActive, x) | (x, VerifyActive) if x.results_in_active() => x,
        // TryRestart upgrades to Restart when paired with anything
        // activating.
        (TryRestart, x) | (x, TryRestart) if x.results_in_active() => Restart,
        // Default: prefer the right-hand (more recently requested) action.
        (_, b) => b,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use indexmap::IndexMap;
    use systeml_unit::{Unit, UnitName};

    // ---------------- mock manager ----------------

    #[derive(Default)]
    struct MockMgr {
        loaded: IndexSet<UnitName>,
        active: IndexSet<UnitName>,
        allow_isolate: IndexSet<UnitName>,
        existing: Vec<Job>,
    }

    impl MockMgr {
        fn with_loaded(names: &[&str]) -> Self {
            let mut m = Self::default();
            for n in names {
                m.loaded.insert(name(n));
            }
            m
        }

        fn set_active(mut self, n: &str) -> Self {
            self.active.insert(name(n));
            self
        }

        fn set_allow_isolate(mut self, n: &str) -> Self {
            self.allow_isolate.insert(name(n));
            self
        }

        fn with_existing(mut self, j: Job) -> Self {
            self.existing.push(j);
            self
        }
    }

    impl ManagerView for MockMgr {
        fn is_loaded(&self, n: &UnitName) -> bool {
            self.loaded.contains(n)
        }
        fn is_active(&self, n: &UnitName) -> bool {
            self.active.contains(n)
        }
        fn allow_isolate(&self, n: &UnitName) -> bool {
            self.allow_isolate.contains(n)
        }
        fn existing_jobs(&self) -> &[Job] {
            &self.existing
        }
    }

    // ---------------- helpers ----------------

    fn name(s: &str) -> UnitName {
        s.parse().unwrap()
    }

    fn empty_unit(n: &str) -> Unit {
        Unit::empty(name(n))
    }

    /// Build a graph from the given units.
    fn graph_of(units: Vec<Unit>) -> (DepGraph, Vec<Unit>) {
        let map: IndexMap<UnitName, &Unit> =
            units.iter().map(|u| (u.name.clone(), u)).collect();
        let g = DepGraph::build(&map);
        (g, units)
    }

    fn job_units(t: &Transaction) -> Vec<String> {
        t.jobs.iter().map(|j| j.unit.to_string()).collect()
    }

    // ---------------- tests ----------------

    #[test]
    fn linear_chain_wants() {
        let mut a = empty_unit("a.service");
        a.deps.wants.insert(name("b.service"));
        let mut b = empty_unit("b.service");
        b.deps.wants.insert(name("c.service"));
        let c = empty_unit("c.service");
        let (g, _u) = graph_of(vec![a, b, c]);
        let mgr = MockMgr::with_loaded(&["a.service", "b.service", "c.service"]);
        let alloc = JobIdAlloc::new();
        let t = Transaction::build(
            &g,
            &mgr,
            &alloc,
            name("a.service"),
            JobType::Start,
            JobMode::Replace,
        )
        .unwrap();
        assert_eq!(t.jobs.len(), 3);
        assert!(t.jobs.iter().all(|j| j.kind == JobType::Start));
        let units = job_units(&t);
        assert!(units.contains(&"a.service".to_owned()));
        assert!(units.contains(&"b.service".to_owned()));
        assert!(units.contains(&"c.service".to_owned()));
    }

    #[test]
    fn diamond_no_duplicate_jobs() {
        // a wants b, c; b wants d; c wants d
        let mut a = empty_unit("a.service");
        a.deps.wants.insert(name("b.service"));
        a.deps.wants.insert(name("c.service"));
        let mut b = empty_unit("b.service");
        b.deps.wants.insert(name("d.service"));
        let mut c = empty_unit("c.service");
        c.deps.wants.insert(name("d.service"));
        let d = empty_unit("d.service");
        let (g, _u) = graph_of(vec![a, b, c, d]);
        let mgr = MockMgr::with_loaded(&["a.service", "b.service", "c.service", "d.service"]);
        let alloc = JobIdAlloc::new();
        let t = Transaction::build(
            &g,
            &mgr,
            &alloc,
            name("a.service"),
            JobType::Start,
            JobMode::Replace,
        )
        .unwrap();
        assert_eq!(t.jobs.len(), 4, "got: {:?}", job_units(&t));
    }

    #[test]
    fn cycle_after_returns_error() {
        let mut a = empty_unit("a.service");
        let mut b = empty_unit("b.service");
        a.deps.wants.insert(name("b.service"));
        a.deps.after.insert(name("b.service"));
        b.deps.after.insert(name("a.service"));
        let (g, _u) = graph_of(vec![a, b]);
        let mgr = MockMgr::with_loaded(&["a.service", "b.service"]);
        let alloc = JobIdAlloc::new();
        let err = Transaction::build(
            &g,
            &mgr,
            &alloc,
            name("a.service"),
            JobType::Start,
            JobMode::Replace,
        )
        .unwrap_err();
        assert!(matches!(err, TransactionError::Cycle(_)));
    }

    #[test]
    fn conflicts_queues_stop() {
        let mut a = empty_unit("a.service");
        a.deps.conflicts.insert(name("b.service"));
        let b = empty_unit("b.service");
        let (g, _u) = graph_of(vec![a, b]);
        let mgr = MockMgr::with_loaded(&["a.service", "b.service"]);
        let alloc = JobIdAlloc::new();
        let t = Transaction::build(
            &g,
            &mgr,
            &alloc,
            name("a.service"),
            JobType::Start,
            JobMode::Replace,
        )
        .unwrap();
        let stop = t
            .jobs
            .iter()
            .find(|j| j.unit == name("b.service"))
            .expect("b stop queued");
        assert_eq!(stop.kind, JobType::Stop);
        let start = t
            .jobs
            .iter()
            .find(|j| j.unit == name("a.service"))
            .expect("a start queued");
        assert_eq!(start.kind, JobType::Start);
    }

    #[test]
    fn binds_to_reverse_propagates_stop() {
        // b BindsTo a → stopping a stops b too.
        let a = empty_unit("a.service");
        let mut b = empty_unit("b.service");
        b.deps.binds_to.insert(name("a.service"));
        let (g, _u) = graph_of(vec![a, b]);
        let mgr = MockMgr::with_loaded(&["a.service", "b.service"]).set_active("b.service");
        let alloc = JobIdAlloc::new();
        let t = Transaction::build(
            &g,
            &mgr,
            &alloc,
            name("a.service"),
            JobType::Stop,
            JobMode::Replace,
        )
        .unwrap();
        assert_eq!(t.jobs.len(), 2);
        assert!(t.jobs.iter().all(|j| j.kind == JobType::Stop));
        let names: Vec<_> = job_units(&t);
        assert!(names.contains(&"a.service".to_owned()));
        assert!(names.contains(&"b.service".to_owned()));
    }

    #[test]
    fn part_of_propagates_stop() {
        // b PartOf a → stopping a stops b too.
        let a = empty_unit("a.target");
        let mut b = empty_unit("b.service");
        b.deps.part_of.insert(name("a.target"));
        let (g, _u) = graph_of(vec![a, b]);
        let mgr = MockMgr::with_loaded(&["a.target", "b.service"]);
        let alloc = JobIdAlloc::new();
        let t = Transaction::build(
            &g,
            &mgr,
            &alloc,
            name("a.target"),
            JobType::Stop,
            JobMode::Replace,
        )
        .unwrap();
        let names: Vec<_> = job_units(&t);
        assert!(names.contains(&"b.service".to_owned()));
    }

    #[test]
    fn isolate_stops_outside_closure() {
        // root: target.t with Wants=hello.service; quux.service is loaded+active
        // and should be stopped under isolate.
        let mut root = empty_unit("root.target");
        root.allow_isolate = true;
        root.deps.wants.insert(name("hello.service"));
        let hello = empty_unit("hello.service");
        let quux = empty_unit("quux.service");
        let (g, _u) = graph_of(vec![root, hello, quux]);
        let mgr = MockMgr::with_loaded(&["root.target", "hello.service", "quux.service"])
            .set_active("quux.service")
            .set_allow_isolate("root.target");
        let alloc = JobIdAlloc::new();
        let t = Transaction::build(
            &g,
            &mgr,
            &alloc,
            name("root.target"),
            JobType::Start,
            JobMode::Isolate,
        )
        .unwrap();
        let stop = t
            .jobs
            .iter()
            .find(|j| j.unit == name("quux.service"))
            .expect("quux stop queued");
        assert_eq!(stop.kind, JobType::Stop);
    }

    #[test]
    fn isolate_requires_allow_isolate() {
        let root = empty_unit("root.target");
        let (g, _u) = graph_of(vec![root]);
        let mgr = MockMgr::with_loaded(&["root.target"]);
        let alloc = JobIdAlloc::new();
        let err = Transaction::build(
            &g,
            &mgr,
            &alloc,
            name("root.target"),
            JobType::Start,
            JobMode::Isolate,
        )
        .unwrap_err();
        assert!(matches!(err, TransactionError::IsolateNotAllowed(_)));
    }

    #[test]
    fn fail_mode_rejects_replacement() {
        let a = empty_unit("a.service");
        let (g, _u) = graph_of(vec![a]);
        let mgr = MockMgr::with_loaded(&["a.service"]).with_existing(Job::new(
            JobId(42),
            name("a.service"),
            JobType::Stop,
            JobMode::Replace,
        ));
        let alloc = JobIdAlloc::new();
        let err = Transaction::build(
            &g,
            &mgr,
            &alloc,
            name("a.service"),
            JobType::Start,
            JobMode::Fail,
        )
        .unwrap_err();
        assert_eq!(err, TransactionError::WouldReplaceFail);
    }

    #[test]
    fn replace_mode_cancels_conflict() {
        let a = empty_unit("a.service");
        let (g, _u) = graph_of(vec![a]);
        let mgr = MockMgr::with_loaded(&["a.service"]).with_existing(Job::new(
            JobId(42),
            name("a.service"),
            JobType::Stop,
            JobMode::Replace,
        ));
        let alloc = JobIdAlloc::new();
        let t = Transaction::build(
            &g,
            &mgr,
            &alloc,
            name("a.service"),
            JobType::Start,
            JobMode::Replace,
        )
        .unwrap();
        assert_eq!(t.cancel, vec![JobId(42)]);
    }

    #[test]
    fn flush_mode_cancels_everything() {
        let a = empty_unit("a.service");
        let (g, _u) = graph_of(vec![a]);
        let mgr = MockMgr::with_loaded(&["a.service"])
            .with_existing(Job::new(
                JobId(7),
                name("other.service"),
                JobType::Start,
                JobMode::Replace,
            ))
            .with_existing(Job::new(
                JobId(8),
                name("yet.service"),
                JobType::Stop,
                JobMode::Replace,
            ));
        let alloc = JobIdAlloc::new();
        let t = Transaction::build(
            &g,
            &mgr,
            &alloc,
            name("a.service"),
            JobType::Start,
            JobMode::Flush,
        )
        .unwrap();
        assert_eq!(t.cancel.len(), 2);
    }

    #[test]
    fn ignore_dependencies_keeps_only_root() {
        let mut a = empty_unit("a.service");
        a.deps.wants.insert(name("b.service"));
        let b = empty_unit("b.service");
        let (g, _u) = graph_of(vec![a, b]);
        let mgr = MockMgr::with_loaded(&["a.service", "b.service"]);
        let alloc = JobIdAlloc::new();
        let t = Transaction::build(
            &g,
            &mgr,
            &alloc,
            name("a.service"),
            JobType::Start,
            JobMode::IgnoreDependencies,
        )
        .unwrap();
        assert_eq!(t.jobs.len(), 1);
        assert_eq!(t.jobs[0].unit, name("a.service"));
    }

    #[test]
    fn ignore_requirements_skips_topo() {
        // Cycle would normally fail; with IgnoreRequirements we skip topo.
        let mut a = empty_unit("a.service");
        let mut b = empty_unit("b.service");
        a.deps.after.insert(name("b.service"));
        a.deps.wants.insert(name("b.service"));
        b.deps.after.insert(name("a.service"));
        let (g, _u) = graph_of(vec![a, b]);
        let mgr = MockMgr::with_loaded(&["a.service", "b.service"]);
        let alloc = JobIdAlloc::new();
        let t = Transaction::build(
            &g,
            &mgr,
            &alloc,
            name("a.service"),
            JobType::Start,
            JobMode::IgnoreRequirements,
        )
        .unwrap();
        // 2 jobs even though there's a cycle.
        assert_eq!(t.jobs.len(), 2);
    }

    #[test]
    fn verify_active_already_active_returns_empty() {
        let a = empty_unit("a.service");
        let (g, _u) = graph_of(vec![a]);
        let mgr = MockMgr::with_loaded(&["a.service"]).set_active("a.service");
        let alloc = JobIdAlloc::new();
        let t = Transaction::build(
            &g,
            &mgr,
            &alloc,
            name("a.service"),
            JobType::VerifyActive,
            JobMode::Replace,
        )
        .unwrap();
        assert!(t.jobs.is_empty());
    }

    #[test]
    fn requisite_emits_verify_active() {
        let mut a = empty_unit("a.service");
        a.deps.requisite.insert(name("b.service"));
        let b = empty_unit("b.service");
        let (g, _u) = graph_of(vec![a, b]);
        let mgr = MockMgr::with_loaded(&["a.service", "b.service"]);
        let alloc = JobIdAlloc::new();
        let t = Transaction::build(
            &g,
            &mgr,
            &alloc,
            name("a.service"),
            JobType::Start,
            JobMode::Replace,
        )
        .unwrap();
        let verify = t
            .jobs
            .iter()
            .find(|j| j.unit == name("b.service"))
            .unwrap();
        assert_eq!(verify.kind, JobType::VerifyActive);
    }

    #[test]
    fn missing_unit_returns_not_loaded() {
        let g = DepGraph::new();
        let mgr = MockMgr::default();
        let alloc = JobIdAlloc::new();
        let err = Transaction::build(
            &g,
            &mgr,
            &alloc,
            name("nope.service"),
            JobType::Start,
            JobMode::Replace,
        )
        .unwrap_err();
        assert!(matches!(err, TransactionError::NotLoaded(_)));
    }

    #[test]
    fn topo_orders_after() {
        let mut a = empty_unit("a.service");
        a.deps.wants.insert(name("b.service"));
        a.deps.after.insert(name("b.service"));
        let b = empty_unit("b.service");
        let (g, _u) = graph_of(vec![a, b]);
        let mgr = MockMgr::with_loaded(&["a.service", "b.service"]);
        let alloc = JobIdAlloc::new();
        let t = Transaction::build(
            &g,
            &mgr,
            &alloc,
            name("a.service"),
            JobType::Start,
            JobMode::Replace,
        )
        .unwrap();
        let units = job_units(&t);
        let pos_a = units.iter().position(|n| n == "a.service").unwrap();
        let pos_b = units.iter().position(|n| n == "b.service").unwrap();
        assert!(pos_b < pos_a, "b must precede a; got {units:?}");
    }

    #[test]
    fn reload_propagates_via_propagates_reload_to() {
        let mut a = empty_unit("a.service");
        a.deps.propagates_reload_to.insert(name("b.service"));
        let b = empty_unit("b.service");
        let (g, _u) = graph_of(vec![a, b]);
        let mgr = MockMgr::with_loaded(&["a.service", "b.service"]);
        let alloc = JobIdAlloc::new();
        let t = Transaction::build(
            &g,
            &mgr,
            &alloc,
            name("a.service"),
            JobType::Reload,
            JobMode::Replace,
        )
        .unwrap();
        assert_eq!(t.jobs.len(), 2);
        assert!(t.jobs.iter().all(|j| j.kind == JobType::Reload));
    }

    #[test]
    fn restart_expands_and_propagates_part_of() {
        // c PartOf=a → restart of a try-restarts c.
        let mut a = empty_unit("a.service");
        a.deps.wants.insert(name("b.service"));
        let b = empty_unit("b.service");
        let mut c = empty_unit("c.service");
        c.deps.part_of.insert(name("a.service"));
        let (g, _u) = graph_of(vec![a, b, c]);
        let mgr = MockMgr::with_loaded(&["a.service", "b.service", "c.service"])
            .set_active("c.service");
        let alloc = JobIdAlloc::new();
        let t = Transaction::build(
            &g,
            &mgr,
            &alloc,
            name("a.service"),
            JobType::Restart,
            JobMode::Replace,
        )
        .unwrap();
        let c_job = t.jobs.iter().find(|j| j.unit == name("c.service")).unwrap();
        // We collapse TryRestart on inactive units to Nop; c is active, so
        // it stays TryRestart.
        assert_eq!(c_job.kind, JobType::TryRestart);
    }

    #[test]
    fn requires_pulls_in_target() {
        let mut a = empty_unit("a.service");
        a.deps.requires.insert(name("b.service"));
        let b = empty_unit("b.service");
        let (g, _u) = graph_of(vec![a, b]);
        let mgr = MockMgr::with_loaded(&["a.service", "b.service"]);
        let alloc = JobIdAlloc::new();
        let t = Transaction::build(
            &g,
            &mgr,
            &alloc,
            name("a.service"),
            JobType::Start,
            JobMode::Replace,
        )
        .unwrap();
        assert_eq!(t.jobs.len(), 2);
    }
}
