//! `.service` supervisor.
//!
//! `runner` holds the per-unit state machine, `notify` parses `sd_notify(3)`
//! datagrams, `pid_file` handles `Type=forking`'s `PIDFile=` polling.

pub mod notify;
pub mod pid_file;
pub mod runner;
pub mod supervise;

pub use runner::{ServiceRunner, StartOutcome};
