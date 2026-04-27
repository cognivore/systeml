//! `systeml-unit` — INI parser and typed AST for systemd unit files.
//!
//! This is the lingua franca of the SystemL workspace: every other crate
//! consumes the types declared here. It is deliberately I/O-light — the
//! only filesystem touches are [`parse_unit_file`] and [`load_unit`],
//! which read fragments on demand.
//!
//! # Layering
//!
//! ```text
//!   parse_unit_str (name, source) ─┐
//!                                  ├──▶  ParsedUnit
//!   parse_unit_file (path)        ─┘     ├─ unit:     Unit
//!                                        └─ warnings: Vec<ParseWarning>
//!
//!   load_unit (name)              ──▶  LoadedUnit
//!                                        ├─ unit:          Unit
//!                                        ├─ warnings:      Vec<ParseWarning>
//!                                        └─ from_template: bool
//! ```
//!
//! `parse_unit_*` is pure: it turns one or more bytes-on-disk into a typed
//! [`Unit`]. `load_unit` walks the standard search path (see [`search`]),
//! finds the main fragment, layers any `<name>.<kind>.d/*.conf` drop-ins,
//! and falls back to a template (`foo@.service`) when the requested name
//! is an instance.
//!
//! # Type system
//!
//! Every parsed unit becomes a [`Unit`] with a per-type variant under
//! [`UnitTypeData`]:
//!
//! - [`ServiceUnit`] — `Type=`, every `Exec*=`, `Restart*`, `Timeout*`,
//!   stdio, kill mode, exit-status sets, every `Limit*`, environment,
//!   user/group, sockets-FDs.
//! - [`SocketUnit`] — every `Listen*=`, `Accept`, `MaxConnections`, fd
//!   permissions, IPv6 binding mode.
//! - [`TimerUnit`] — every `On*Sec=`, every `OnCalendar=` parsed into
//!   typed [`CalendarSpec`]s, persistence flags.
//! - [`PathUnit`] — every `Path*=` predicate plus `MakeDirectory` and
//!   trigger rate-limits.
//! - [`TargetUnit`] / [`ScopeUnit`] — empty placeholders; the
//!   interesting state for these unit kinds lives in `[Unit]`.
//! - `Other` — `.mount`/`.automount`/`.swap`/`.device`/`.slice`. Their
//!   names parse, their bodies are preserved on the raw [`Ini`] for
//!   round-trip but typed as inert.
//!
//! # Coverage and round-trip
//!
//! - All six user-mode-meaningful unit kinds (service / socket / timer /
//!   path / target / scope) are fully typed.
//! - Linux-kernel-only directives are accepted with a structured warning
//!   (see [`linux_only`] for the catalogue and [`ParseWarning`] for the
//!   shape). They are kept on `ServiceUnit::passthrough` and the unit's
//!   `raw` [`Ini`] so [`Unit::render_cat`] reproduces the original file
//!   verbatim.
//! - Unknown directives also warn but never error — forward-compatibility
//!   with future systemd directives is by design.
//!
//! # Module guide
//!
//! - [`name`] — `UnitName` parsing and the 11 [`UnitKind`]s.
//! - [`error`] — [`ParseError`] (hard) vs [`ParseWarning`] (soft).
//! - [`ini`] — the low-level INI parser. Section ordering, comments,
//!   line continuations, `Key=` resets-list semantics.
//! - [`exec`] — [`ExecCommand`] parsing including the `-`/`+`/`@`/`!`/
//!   `!!`/`:` prefix flags.
//! - [`env`] — `Environment=` and `EnvironmentFile=` parsers.
//! - [`condition`] — `Condition*=` and `Assert*=` parsing (evaluation
//!   lives in `systeml-runtime`).
//! - [`calendar`] — `OnCalendar=` parser. Next-fire computation lives in
//!   `systeml-runtime`.
//! - [`duration`] — `man systemd.time` "PARSING TIME SPANS" — `5s`,
//!   `1h 30m`, `infinity`, etc.
//! - [`service`] / [`socket`] / [`timer`] / [`path_unit`] / [`target`] —
//!   per-type directive blocks, fully typed.
//! - [`spec`] — pulls everything together: [`Unit`] and the typed
//!   parser that walks an [`Ini`] section-by-section.
//! - [`load`] — search-path walker + drop-in stacker.
//! - [`search`] — XDG search paths. `~/.config/systemd/user/` etc.
//!
//! # Example
//!
//! ```
//! # use systeml_unit::*;
//! let src = "[Unit]\nDescription=hi\n[Service]\nExecStart=/bin/true\n";
//! let parsed = parse_unit_str("foo.service".parse().unwrap(), src, None).unwrap();
//! assert_eq!(parsed.unit.description, "hi");
//! match &parsed.unit.kind {
//!     UnitTypeData::Service(svc) => assert_eq!(svc.exec_start.len(), 1),
//!     _ => unreachable!(),
//! }
//! ```

#![warn(rust_2018_idioms)]

pub mod calendar;
pub mod condition;
pub mod deps;
pub mod duration;
pub mod env;
pub mod error;
pub mod exec;
pub mod ini;
pub mod install;
pub mod linux_only;
pub mod load;
pub mod name;
pub mod path_unit;
pub mod search;
pub mod service;
pub mod socket;
pub mod spec;
pub mod target;
pub mod timer;

pub use calendar::CalendarSpec;
pub use condition::{Condition, ConditionCheck, Conditions};
pub use deps::UnitDeps;
pub use duration::{parse_duration, SdDuration};
pub use env::EnvironmentFile;
pub use error::{ParseError, ParseWarning, WarningKind};
pub use exec::{ExecCommand, ExecFlags};
pub use ini::Ini;
pub use install::Install;
pub use load::{is_activatable, list_unit_files, load_unit, load_unit_in, LoadedUnit};
pub use name::{UnitKind, UnitName};
pub use path_unit::{PathUnit, PathWatch};
pub use service::{
    ExitStatus, KillMode, NotifyAccess, ProcessOutcome, RLimit, Restart, ServiceType,
    ServiceUnit, StandardStream,
};
pub use socket::{BindIPv6Only, ListenSpec, SocketAddrSpec, SocketUnit};
pub use spec::{parse_unit_file, parse_unit_str, ParsedUnit, Unit, UnitTypeData};
pub use target::{ScopeUnit, TargetUnit};
pub use timer::TimerUnit;
