//! The dependency graph.
//!
//! [`DepGraph`] is a typed directed multigraph keyed by [`UnitName`]. Every
//! source unit has a list of `(DepEdge, target)` pairs. Some [`DepEdge`]s
//! imply a dual edge in the reverse direction (e.g. `WantedBy=foo` on unit
//! `bar` means "`foo` Wants `bar`"); those duals are materialised here so
//! consumers don't have to reason about implicit reversal.
//!
//! Construction is purely a function of the unit set passed in — there is no
//! global state and no I/O. The graph is self-consistent for a given snapshot
//! of `IndexMap<UnitName, &Unit>`.

use indexmap::{IndexMap, IndexSet};
use systeml_unit::{Unit, UnitName, UnitTypeData};

/// Edge kind in the dependency graph.
///
/// Mirrors systemd's `UnitDependency` atoms but trimmed to the subset that
/// actually drives transaction expansion (we drop reload-propagated-from
/// etc. — those are reverse views of the same data).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum DepEdge {
    /// `Wants=` — soft pull. Failure of target does not fail source.
    Wants,
    /// `Requires=` — hard pull. Failure of target fails source.
    Requires,
    /// `Requisite=` — target must already be active.
    Requisite,
    /// `BindsTo=` — mutual lifecycle (Requires + reverse stop on target loss).
    BindsTo,
    /// `PartOf=` — reverse `Requires`: stopping target stops source.
    PartOf,
    /// `Upholds=` — soft sticky pull.
    Upholds,
    /// `After=` — order only; source starts after target.
    After,
    /// `Before=` — order only; source starts before target.
    Before,
    /// `Conflicts=` — mutual exclusion: starting source stops target.
    Conflicts,
    /// `OnFailure=` — start target when source fails.
    OnFailure,
    /// `OnSuccess=` — start target when source succeeds (oneshot).
    OnSuccess,
    /// `PropagatesReloadTo=` — reload of source propagates to target.
    PropagatesReloadTo,
    /// `PropagatesStopTo=` — stop of source propagates to target.
    PropagatesStopTo,
}

impl DepEdge {
    /// True if this edge is purely about ordering, not about pulling units in.
    pub fn is_ordering(self) -> bool {
        matches!(self, Self::After | Self::Before)
    }
}

/// The dependency graph.
///
/// Constructed from a snapshot of loaded units via [`DepGraph::build`]. Edges
/// are stored both forward (source→targets) and reverse (target→sources)
/// for O(deg) [`DepGraph::incoming`] without re-scanning.
#[derive(Debug, Default, Clone)]
pub struct DepGraph {
    /// Forward adjacency keyed by source. Entry order is iteration order.
    forward: IndexMap<UnitName, Vec<(DepEdge, UnitName)>>,
    /// Reverse adjacency keyed by target.
    reverse: IndexMap<UnitName, Vec<(DepEdge, UnitName)>>,
    /// All names that appeared as either source or target.
    nodes: IndexSet<UnitName>,
    /// `IgnoreOnIsolate=yes` opt-out flags carried from the unit snapshot.
    ignore_on_isolate: IndexSet<UnitName>,
}

impl DepGraph {
    /// Construct an empty graph.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Build a graph from a snapshot of loaded units.
    ///
    /// Walks every `unit.deps`, every `unit.install` reverse pull, and every
    /// implicit edge (timer `Unit=`, path `Unit=`, socket `Service=`).
    pub fn build(units: &IndexMap<UnitName, &Unit>) -> Self {
        let mut g = Self::new();

        // Seed all nodes first so isolated units are still reachable.
        for name in units.keys() {
            g.nodes.insert(name.clone());
        }

        for (name, unit) in units {
            if unit.ignore_on_isolate {
                g.ignore_on_isolate.insert(name.clone());
            }
            g.add_unit_edges(name, unit);
            g.add_install_reverse_edges(name, unit);
            g.add_implicit_edges(name, unit);
        }

        g
    }

    /// Walk `unit.deps`. Each entry adds a forward edge; ordering edges add a
    /// matching `Before` on the other side so the topo pass sees both
    /// directions even if only `After=` was declared.
    fn add_unit_edges(&mut self, src: &UnitName, unit: &Unit) {
        let d = &unit.deps;
        for t in &d.wants {
            self.add_edge(src.clone(), DepEdge::Wants, t.clone());
        }
        for t in &d.requires {
            self.add_edge(src.clone(), DepEdge::Requires, t.clone());
        }
        for t in &d.requisite {
            self.add_edge(src.clone(), DepEdge::Requisite, t.clone());
        }
        for t in &d.binds_to {
            self.add_edge(src.clone(), DepEdge::BindsTo, t.clone());
        }
        for t in &d.part_of {
            self.add_edge(src.clone(), DepEdge::PartOf, t.clone());
        }
        for t in &d.upholds {
            self.add_edge(src.clone(), DepEdge::Upholds, t.clone());
        }
        for t in &d.conflicts {
            self.add_edge(src.clone(), DepEdge::Conflicts, t.clone());
        }
        for t in &d.on_failure {
            self.add_edge(src.clone(), DepEdge::OnFailure, t.clone());
        }
        for t in &d.on_success {
            self.add_edge(src.clone(), DepEdge::OnSuccess, t.clone());
        }
        for t in &d.propagates_reload_to {
            self.add_edge(src.clone(), DepEdge::PropagatesReloadTo, t.clone());
        }
        for t in &d.propagates_stop_to {
            self.add_edge(src.clone(), DepEdge::PropagatesStopTo, t.clone());
        }
        // Ordering: After means src starts after target → target precedes
        // src. We store both as the natural direction the user declared.
        for t in &d.after {
            self.add_edge(src.clone(), DepEdge::After, t.clone());
        }
        for t in &d.before {
            self.add_edge(src.clone(), DepEdge::Before, t.clone());
        }
    }

    /// `[Install] WantedBy=foo` on unit `bar` synthesizes an edge
    /// `foo Wants bar`. Same for `RequiredBy=` → `Requires`, and
    /// `UpheldBy=` → `Upholds`. systemd does this only when
    /// `DefaultDependencies=yes`.
    fn add_install_reverse_edges(&mut self, src: &UnitName, unit: &Unit) {
        if !unit.default_dependencies {
            return;
        }
        for t in &unit.install.wanted_by {
            self.add_edge(t.clone(), DepEdge::Wants, src.clone());
        }
        for t in &unit.install.required_by {
            self.add_edge(t.clone(), DepEdge::Requires, src.clone());
        }
        for t in &unit.install.upheld_by {
            self.add_edge(t.clone(), DepEdge::Upholds, src.clone());
        }
    }

    /// Type-specific implicit edges:
    ///
    /// - A `.timer` whose `[Timer] Unit=foo.service` is set adds
    ///   `timer Wants foo.service` and `timer Before foo.service` so timers
    ///   start their target unit when they elapse.
    /// - A `.path` does the same (`[Path] Unit=`).
    /// - A `.socket` does the same (`[Socket] Service=`).
    fn add_implicit_edges(&mut self, src: &UnitName, unit: &Unit) {
        match &unit.kind {
            UnitTypeData::Timer(t) => {
                if let Some(target) = &t.unit {
                    self.add_edge(src.clone(), DepEdge::Wants, target.clone());
                    self.add_edge(src.clone(), DepEdge::Before, target.clone());
                }
            }
            UnitTypeData::Path(p) => {
                if let Some(target) = &p.unit {
                    self.add_edge(src.clone(), DepEdge::Wants, target.clone());
                    self.add_edge(src.clone(), DepEdge::Before, target.clone());
                }
            }
            UnitTypeData::Socket(s) => {
                if let Some(target) = &s.service {
                    self.add_edge(src.clone(), DepEdge::Wants, target.clone());
                    self.add_edge(src.clone(), DepEdge::Before, target.clone());
                }
            }
            _ => {}
        }
    }

    /// Add a directed edge. Idempotent: duplicates are dropped.
    pub fn add_edge(&mut self, from: UnitName, kind: DepEdge, to: UnitName) {
        self.nodes.insert(from.clone());
        self.nodes.insert(to.clone());
        let f = self.forward.entry(from.clone()).or_default();
        if !f.iter().any(|(k, n)| *k == kind && *n == to) {
            f.push((kind, to.clone()));
        }
        let r = self.reverse.entry(to).or_default();
        if !r.iter().any(|(k, n)| *k == kind && *n == from) {
            r.push((kind, from));
        }
    }

    /// Iterate `(edge, target)` pairs leaving `name`.
    pub fn outgoing<'a>(
        &'a self,
        name: &UnitName,
    ) -> impl Iterator<Item = (DepEdge, &'a UnitName)> + 'a {
        self.forward
            .get(name)
            .into_iter()
            .flat_map(|v| v.iter().map(|(k, n)| (*k, n)))
    }

    /// Iterate `(edge, source)` pairs entering `name`. The edge kind is the
    /// kind as declared on the source.
    pub fn incoming<'a>(
        &'a self,
        name: &UnitName,
    ) -> impl Iterator<Item = (DepEdge, &'a UnitName)> + 'a {
        self.reverse
            .get(name)
            .into_iter()
            .flat_map(|v| v.iter().map(|(k, n)| (*k, n)))
    }

    /// All unit names seen on either side of an edge or seeded by
    /// [`DepGraph::build`].
    pub fn nodes(&self) -> impl Iterator<Item = &UnitName> {
        self.nodes.iter()
    }

    /// Whether `name` was marked `IgnoreOnIsolate=yes`.
    pub fn ignore_on_isolate(&self, name: &UnitName) -> bool {
        self.ignore_on_isolate.contains(name)
    }

    /// Topologically sort the given names by `After`/`Before` edges.
    ///
    /// `After=A B` and `Before=B A` are normalised to a single direction:
    /// for every node u, every neighbour visited via `After` must precede u;
    /// every neighbour visited via `Before` must follow u.
    ///
    /// Returns the names in start order. If a cycle is detected, returns
    /// `Err(name)` where `name` is on the cycle.
    pub fn topo_order(&self, units: &[UnitName]) -> Result<Vec<UnitName>, UnitName> {
        // Build a restricted dependency map: for each `u` in the input set,
        // collect the predecessors (those that must run first). Edges to
        // names outside `units` are ignored; this is what we want when sorting
        // a transaction subset.
        let in_set: IndexSet<&UnitName> = units.iter().collect();
        let mut preds: IndexMap<UnitName, IndexSet<UnitName>> = IndexMap::new();
        for u in units {
            preds.entry(u.clone()).or_default();
        }
        for u in units {
            for (edge, other) in self.outgoing(u) {
                if !in_set.contains(other) {
                    continue;
                }
                match edge {
                    // `u After other` → other precedes u.
                    DepEdge::After => {
                        preds.entry(u.clone()).or_default().insert(other.clone());
                    }
                    // `u Before other` → u precedes other.
                    DepEdge::Before => {
                        preds.entry(other.clone()).or_default().insert(u.clone());
                    }
                    _ => {}
                }
            }
        }

        // Kahn's algorithm with deterministic order from `units`.
        let mut indeg: IndexMap<UnitName, usize> =
            preds.iter().map(|(k, v)| (k.clone(), v.len())).collect();
        // We need successors too for decrementing.
        let mut succs: IndexMap<UnitName, Vec<UnitName>> = IndexMap::new();
        for (node, ps) in &preds {
            for p in ps {
                succs.entry(p.clone()).or_default().push(node.clone());
            }
        }
        let mut ready: Vec<UnitName> = units
            .iter()
            .filter(|u| indeg.get(*u).copied().unwrap_or(0) == 0)
            .cloned()
            .collect();
        let mut out: Vec<UnitName> = Vec::with_capacity(units.len());
        while let Some(u) = ready.first().cloned() {
            ready.remove(0);
            out.push(u.clone());
            if let Some(ss) = succs.get(&u) {
                for s in ss.clone() {
                    if let Some(d) = indeg.get_mut(&s) {
                        *d = d.saturating_sub(1);
                        if *d == 0 {
                            ready.push(s);
                        }
                    }
                }
            }
        }
        if out.len() != units.len() {
            // Anything still with indeg > 0 is on or downstream of a cycle.
            let cycle = indeg
                .iter()
                .find(|(_, d)| **d > 0)
                .map(|(n, _)| n.clone())
                .unwrap_or_else(|| units[0].clone());
            return Err(cycle);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use systeml_unit::Unit;

    fn name(s: &str) -> UnitName {
        s.parse().unwrap()
    }

    fn unit(n: &str) -> Unit {
        Unit::empty(name(n))
    }

    #[test]
    fn build_empty() {
        let units: IndexMap<UnitName, &Unit> = IndexMap::new();
        let g = DepGraph::build(&units);
        assert_eq!(g.nodes().count(), 0);
    }

    #[test]
    fn wants_creates_forward_and_reverse() {
        let mut a = unit("a.service");
        a.deps.wants.insert(name("b.service"));
        let b = unit("b.service");
        let mut units = IndexMap::new();
        units.insert(a.name.clone(), &a);
        units.insert(b.name.clone(), &b);
        let g = DepGraph::build(&units);
        let outgoing: Vec<_> = g.outgoing(&name("a.service")).collect();
        assert_eq!(outgoing, vec![(DepEdge::Wants, &name("b.service"))]);
        let incoming: Vec<_> = g.incoming(&name("b.service")).collect();
        assert_eq!(incoming, vec![(DepEdge::Wants, &name("a.service"))]);
    }

    #[test]
    fn install_wantedby_synthesizes_edge() {
        let mut svc = unit("hello.service");
        svc.install.wanted_by.insert(name("default.target"));
        let tgt = unit("default.target");
        let mut units = IndexMap::new();
        units.insert(svc.name.clone(), &svc);
        units.insert(tgt.name.clone(), &tgt);
        let g = DepGraph::build(&units);
        let outgoing: Vec<_> = g.outgoing(&name("default.target")).collect();
        assert_eq!(outgoing, vec![(DepEdge::Wants, &name("hello.service"))]);
    }

    #[test]
    fn timer_unit_synthesizes_wants_and_before() {
        let mut t = unit("hello.timer");
        if let UnitTypeData::Timer(ref mut tm) = t.kind {
            tm.unit = Some(name("hello.service"));
        }
        let s = unit("hello.service");
        let mut units = IndexMap::new();
        units.insert(t.name.clone(), &t);
        units.insert(s.name.clone(), &s);
        let g = DepGraph::build(&units);
        let outgoing: Vec<_> = g.outgoing(&name("hello.timer")).collect();
        assert!(outgoing.contains(&(DepEdge::Wants, &name("hello.service"))));
        assert!(outgoing.contains(&(DepEdge::Before, &name("hello.service"))));
    }

    #[test]
    fn topo_after_chain() {
        let mut a = unit("a.service");
        let mut b = unit("b.service");
        let c = unit("c.service");
        a.deps.after.insert(name("b.service"));
        b.deps.after.insert(name("c.service"));
        let mut units = IndexMap::new();
        units.insert(a.name.clone(), &a);
        units.insert(b.name.clone(), &b);
        units.insert(c.name.clone(), &c);
        let g = DepGraph::build(&units);
        let order = g
            .topo_order(&[name("a.service"), name("b.service"), name("c.service")])
            .unwrap();
        assert_eq!(
            order,
            vec![name("c.service"), name("b.service"), name("a.service")]
        );
    }

    #[test]
    fn topo_cycle_detected() {
        let mut a = unit("a.service");
        let mut b = unit("b.service");
        a.deps.after.insert(name("b.service"));
        b.deps.after.insert(name("a.service"));
        let mut units = IndexMap::new();
        units.insert(a.name.clone(), &a);
        units.insert(b.name.clone(), &b);
        let g = DepGraph::build(&units);
        let err = g
            .topo_order(&[name("a.service"), name("b.service")])
            .unwrap_err();
        assert!(err == name("a.service") || err == name("b.service"));
    }

    #[test]
    fn add_edge_is_idempotent() {
        let mut g = DepGraph::new();
        g.add_edge(name("a.service"), DepEdge::Wants, name("b.service"));
        g.add_edge(name("a.service"), DepEdge::Wants, name("b.service"));
        let n = g.outgoing(&name("a.service")).count();
        assert_eq!(n, 1);
    }
}
