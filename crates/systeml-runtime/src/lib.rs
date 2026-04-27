//! `systeml-runtime` — process supervisor and activation engines.
//!
//! This crate holds **all the live state**. The bus and the daemon are
//! thin shims around it; the real work — fork/exec, supervise, schedule,
//! watch, activate — happens here.
//!
//! # Architecture
//!
//! ```text
//!                            Manager
//!                  (registry + status + events)
//!                              │
//!     ┌────────────┬───────────┼───────────┬────────────┐
//!     ▼            ▼           ▼           ▼            ▼
//!  service       timer        path       socket       install
//! ServiceRunner  next_fire   kqueue     bind+FDs     symlinks
//!   per-unit     calendar    watcher    LISTEN_*      enable/
//!   tokio task   scheduler              passing       disable
//! ```
//!
//! The [`Manager`] is the **single contract** every other crate consumes.
//! Every method on it is stable (whether implemented or `todo!()`-stubbed),
//! and the bus / CLI / daemon all call into it through a
//! `tokio::sync::RwLock<Manager>`.
//!
//! # Module guide
//!
//! - [`manager`] — the registry. `daemon_reload` walks the search path,
//!   start/stop/restart/reload dispatch through the per-type runners,
//!   enable/disable/mask/unmask manage `[Install]` symlinks via
//!   [`install`], `show_properties` exposes the systemd-style property
//!   dump.
//! - [`state`] — [`LoadState`] / [`ActiveState`] / `SubState` /
//!   [`UnitStatus`]. Mirrors `org.freedesktop.systemd1.Unit` properties.
//! - [`service`] — the heart of the supervisor. `ServiceRunner` is one
//!   tokio task per active service; it handles the `Type=` lifecycle
//!   (`simple` / `exec` / `oneshot` / `forking` / `notify` /
//!   `notify-reload` / `idle`), `sd_notify` parsing, `Restart=` policy
//!   with exponential backoff, `Exec*=` sequencing, `KillMode=`
//!   semantics via `setsid` + `killpg`.
//! - [`timer`] — `next_fire` over a typed [`systeml_unit::CalendarSpec`]
//!   plus monotonic triggers (`OnBootSec`, `OnUnitActiveSec`). Persistent
//!   timer state is read/written under `$XDG_STATE_HOME/systeml/timers/`.
//! - [`path`] — kqueue-based file system event watcher. Predicate
//!   evaluation for `PathExists`, `PathExistsGlob`, `PathChanged`,
//!   `PathModified`, `DirectoryNotEmpty`. `MakeDirectory` and trigger
//!   rate-limiting.
//! - [`socket`] — listening-socket binder for TCP/IPv6/Unix path/FIFO/
//!   special files. Fd-passing via `LISTEN_FDS=N` + `LISTEN_PID=<pid>` +
//!   `LISTEN_FDNAMES=` env vars set up via `pre_exec` `dup2` (the only
//!   safe path inside `fork()`-then-`exec()`).
//! - [`exec`] — shared spawn helpers: `Environment=` resolution with
//!   `${VAR}` expansion, `EnvironmentFile=` loading, fd setup, privilege
//!   drop in `pre_exec`.
//! - [`install`] — `[Install]` symlink management. `enable_units` writes
//!   `<wanted>.wants/<unit>` symlinks under `user_config_dir()` per the
//!   systemd convention; `mask` symlinks to `/dev/null`.
//!
//! # Unsafe footprint
//!
//! Workspace policy is `unsafe_code = "deny"`. This crate is the only
//! one with `#[allow(unsafe_code)]` carve-outs, all in known callsites:
//! `pre_exec` for fork-safe fd dup and `setenv` of `LISTEN_PID`, and the
//! kqueue FFI in [`path`]. The `nix` crate covers everything else with
//! safe wrappers.

#![warn(rust_2018_idioms)]
#![allow(dead_code)]

pub mod exec;
pub mod install;
pub mod manager;
pub mod path;
pub mod service;
pub mod socket;
pub mod state;
pub mod timer;

pub use manager::Manager;
pub use state::{ActiveState, LoadState, SubState, UnitStatus};
