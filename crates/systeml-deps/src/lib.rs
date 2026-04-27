//! `systeml-deps` — dependency graph and transaction/job engine.
//!
//! Two layers:
//!
//! 1. [`graph`] turns a snapshot of loaded `Unit`s into a [`DepGraph`] — a
//!    directed multigraph keyed by [`UnitName`] with typed [`DepEdge`]s.
//!    Synthesizes systemd's "implicit" deps (timer→service via `Unit=`,
//!    `WantedBy=` reverse pulls under `DefaultDependencies=yes`).
//! 2. [`transaction`] takes a [`DepGraph`] plus a [`ManagerView`] and a root
//!    job request, then expands it into a topologically-sorted batch of
//!    [`Job`]s following the same expansion rules as
//!    `systemd-stable/src/core/transaction.c`'s
//!    `transaction_add_job_and_dependencies` — minus the cgroup machinery.

#![warn(rust_2018_idioms)]

pub mod graph;
pub mod job;
pub mod transaction;

pub use graph::{DepEdge, DepGraph};
pub use job::{Job, JobId, JobIdAlloc, JobMode, JobOutcome, JobType};
pub use transaction::{ManagerView, Transaction, TransactionError};
