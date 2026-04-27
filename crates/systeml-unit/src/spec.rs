//! The unified `Unit` AST and the typed parser that turns an `Ini` plus a
//! unit name into a fully resolved unit.
//!
//! Directive coverage follows `man systemd.unit`, `systemd.service`,
//! `systemd.socket`, `systemd.timer`, `systemd.path`. Linux-kernel-only
//! directives are accepted with a warning (see `linux_only.rs`); unknown
//! directives also warn but are preserved on the raw `Ini` for round-trip.

use crate::calendar::CalendarSpec;
use crate::condition::{Condition, Conditions};
use crate::deps::UnitDeps;
use crate::duration::{parse_duration, SdDuration};
use crate::env::{parse_environment_line, EnvironmentFile};
use crate::error::{ParseError, ParseWarning};
use crate::exec::ExecCommand;
use crate::ini::{Ini, parse as parse_ini};
use crate::install::Install;
use crate::linux_only;
use crate::name::{UnitKind, UnitName};
use crate::path_unit::{PathUnit, PathWatch};
use crate::service::{
    ExitStatus, KillMode, NotifyAccess, RLimit, Restart, ServiceType, ServiceUnit,
    StandardStream,
};
use crate::socket::{BindIPv6Only, ListenSpec, SocketAddrSpec, SocketUnit};
use crate::target::{ScopeUnit, TargetUnit};
use crate::timer::TimerUnit;
use std::path::{Path, PathBuf};
use std::str::FromStr;

/// Per-unit-type typed data.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[allow(missing_docs)]
pub enum UnitTypeData {
    Service(ServiceUnit),
    Socket(SocketUnit),
    Timer(TimerUnit),
    Path(PathUnit),
    Target(TargetUnit),
    Scope(ScopeUnit),
    /// Mount/Automount/Swap/Device/Slice — parsed names only; bodies inert.
    Other,
}

/// The fully-parsed unit.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Unit {
    /// Unit name.
    pub name: UnitName,
    /// Fragment paths in load order (main file then drop-ins).
    pub fragment_paths: Vec<PathBuf>,
    /// `Description=`.
    pub description: String,
    /// `Documentation=` URLs/paths.
    pub documentation: Vec<String>,
    /// `[Unit]` deps.
    pub deps: UnitDeps,
    /// `[Install]`.
    pub install: Install,
    /// `Condition*=` / `Assert*=`.
    pub conditions: Conditions,
    /// `DefaultDependencies=` (default true).
    pub default_dependencies: bool,
    /// `StopWhenUnneeded=`.
    pub stop_when_unneeded: bool,
    /// `RefuseManualStart=`.
    pub refuse_manual_start: bool,
    /// `RefuseManualStop=`.
    pub refuse_manual_stop: bool,
    /// `AllowIsolate=`.
    pub allow_isolate: bool,
    /// `JobTimeoutSec=`.
    pub job_timeout_sec: Option<SdDuration>,
    /// `JobRunningTimeoutSec=`.
    pub job_running_timeout_sec: Option<SdDuration>,
    /// `OnFailureJobMode=`.
    pub on_failure_job_mode: Option<String>,
    /// `IgnoreOnIsolate=`.
    pub ignore_on_isolate: bool,
    /// `CollectMode=`.
    pub collect_mode: Option<String>,
    /// `FailureAction=`.
    pub failure_action: Option<String>,
    /// `SuccessAction=`.
    pub success_action: Option<String>,
    /// `RebootArgument=`.
    pub reboot_argument: Option<String>,
    /// `SourcePath=`.
    pub source_path: Option<PathBuf>,
    /// Per-type typed data.
    pub kind: UnitTypeData,
    /// Preserved raw ini for `cat`/`show`.
    pub raw: Ini,
}

impl Unit {
    /// Bootstrap a default-empty Unit for the given name.
    pub fn empty(name: UnitName) -> Self {
        let kind = match name.kind {
            UnitKind::Service => UnitTypeData::Service(ServiceUnit::default()),
            UnitKind::Socket => UnitTypeData::Socket(SocketUnit::default()),
            UnitKind::Timer => UnitTypeData::Timer(TimerUnit::default()),
            UnitKind::Path => UnitTypeData::Path(PathUnit::default()),
            UnitKind::Target => UnitTypeData::Target(TargetUnit),
            UnitKind::Scope => UnitTypeData::Scope(ScopeUnit),
            _ => UnitTypeData::Other,
        };
        Self {
            name,
            fragment_paths: Vec::new(),
            description: String::new(),
            documentation: Vec::new(),
            deps: UnitDeps::default(),
            install: Install::default(),
            conditions: Conditions::default(),
            default_dependencies: true,
            stop_when_unneeded: false,
            refuse_manual_start: false,
            refuse_manual_stop: false,
            allow_isolate: false,
            job_timeout_sec: None,
            job_running_timeout_sec: None,
            on_failure_job_mode: None,
            ignore_on_isolate: false,
            collect_mode: None,
            failure_action: None,
            success_action: None,
            reboot_argument: None,
            source_path: None,
            kind,
            raw: Ini::default(),
        }
    }

    /// Pretty `cat`-style output.
    pub fn render_cat(&self) -> String {
        let mut s = String::new();
        for (section, entries) in &self.raw.sections {
            s.push('[');
            s.push_str(section);
            s.push_str("]\n");
            for e in entries {
                s.push_str(&e.key);
                s.push('=');
                s.push_str(&e.value);
                s.push('\n');
            }
            s.push('\n');
        }
        s
    }
}

/// Result of parsing: unit + warnings.
pub struct ParsedUnit {
    /// The parsed unit.
    pub unit: Unit,
    /// Any non-fatal warnings.
    pub warnings: Vec<ParseWarning>,
}

/// Parse a single fragment file from disk.
pub fn parse_unit_file(path: &Path) -> Result<ParsedUnit, ParseError> {
    let source = std::fs::read_to_string(path).map_err(|e| ParseError::Io {
        path: path.to_owned(),
        source: e,
    })?;
    let name = UnitName::from_path(path)?;
    let mut p = parse_unit_str(name, &source, Some(path))?;
    p.unit.fragment_paths.push(path.to_owned());
    Ok(p)
}

/// Parse from in-memory source.
pub fn parse_unit_str(
    name: UnitName,
    source: &str,
    path_for_errors: Option<&Path>,
) -> Result<ParsedUnit, ParseError> {
    let path = path_for_errors.unwrap_or(Path::new("<memory>"));
    let ini = parse_ini(path, source)?;
    let mut warnings = Vec::new();
    let mut unit = Unit::empty(name);
    unit.raw = ini.clone();

    // [Unit] section
    if let Some(entries) = ini.sections.get("Unit") {
        for e in entries {
            apply_unit_section(path, &mut unit, e, &mut warnings)?;
        }
    }
    // [Install]
    if let Some(entries) = ini.sections.get("Install") {
        for e in entries {
            apply_install_section(path, &mut unit, e, &mut warnings)?;
        }
    }
    // Type-specific.
    match unit.kind {
        UnitTypeData::Service(_) => parse_service(&ini, &mut unit, path, &mut warnings)?,
        UnitTypeData::Socket(_) => parse_socket(&ini, &mut unit, path, &mut warnings)?,
        UnitTypeData::Timer(_) => parse_timer(&ini, &mut unit, path, &mut warnings)?,
        UnitTypeData::Path(_) => parse_path(&ini, &mut unit, path, &mut warnings)?,
        UnitTypeData::Target(_) | UnitTypeData::Scope(_) | UnitTypeData::Other => {
            // No type-specific section to parse.
        }
    }

    Ok(ParsedUnit { unit, warnings })
}

// ---------- helpers ----------

fn parse_bool(s: &str) -> Option<bool> {
    Some(match s.to_ascii_lowercase().as_str() {
        "1" | "yes" | "true" | "on" => true,
        "0" | "no" | "false" | "off" => false,
        _ => return None,
    })
}

/// Whitespace + comma-separated list.
fn parse_list(s: &str) -> Vec<String> {
    s.split(|c: char| c.is_whitespace() || c == ',')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_owned())
        .collect()
}

fn parse_units_list(s: &str) -> Vec<UnitName> {
    parse_list(s)
        .into_iter()
        .filter_map(|n| UnitName::from_str(&n).ok())
        .collect()
}

fn bad(
    path: &Path,
    section: &str,
    e: &crate::ini::Entry,
    reason: impl Into<String>,
) -> ParseError {
    ParseError::BadDirective {
        path: path.to_owned(),
        line: e.line,
        section: section.to_owned(),
        key: e.key.clone(),
        value: e.value.clone(),
        reason: reason.into(),
    }
}

// ---------- [Unit] / [Install] ----------

fn apply_unit_section(
    path: &Path,
    unit: &mut Unit,
    e: &crate::ini::Entry,
    warnings: &mut Vec<ParseWarning>,
) -> Result<(), ParseError> {
    let v = e.value.as_str();
    match e.key.as_str() {
        "Description" => unit.description = v.to_owned(),
        "Documentation" => unit.documentation.extend(parse_list(v)),
        "After" => unit.deps.after.extend(parse_units_list(v)),
        "Before" => unit.deps.before.extend(parse_units_list(v)),
        "Wants" => unit.deps.wants.extend(parse_units_list(v)),
        "Requires" => unit.deps.requires.extend(parse_units_list(v)),
        "Requisite" => unit.deps.requisite.extend(parse_units_list(v)),
        "BindsTo" => unit.deps.binds_to.extend(parse_units_list(v)),
        "PartOf" => unit.deps.part_of.extend(parse_units_list(v)),
        "Upholds" => unit.deps.upholds.extend(parse_units_list(v)),
        "Conflicts" => unit.deps.conflicts.extend(parse_units_list(v)),
        "OnFailure" => unit.deps.on_failure.extend(parse_units_list(v)),
        "OnSuccess" => unit.deps.on_success.extend(parse_units_list(v)),
        "PropagatesReloadTo" => unit
            .deps
            .propagates_reload_to
            .extend(parse_units_list(v)),
        "ReloadPropagatedFrom" => unit
            .deps
            .reload_propagated_from
            .extend(parse_units_list(v)),
        "PropagatesStopTo" => unit.deps.propagates_stop_to.extend(parse_units_list(v)),
        "StopPropagatedFrom" => unit.deps.stop_propagated_from.extend(parse_units_list(v)),
        "JoinsNamespaceOf" => unit
            .deps
            .joins_namespace_of
            .extend(parse_units_list(v)),
        "DefaultDependencies" => {
            unit.default_dependencies =
                parse_bool(v).ok_or_else(|| bad(path, "Unit", e, "expected bool"))?;
        }
        "StopWhenUnneeded" => {
            unit.stop_when_unneeded =
                parse_bool(v).ok_or_else(|| bad(path, "Unit", e, "expected bool"))?;
        }
        "RefuseManualStart" => {
            unit.refuse_manual_start =
                parse_bool(v).ok_or_else(|| bad(path, "Unit", e, "expected bool"))?;
        }
        "RefuseManualStop" => {
            unit.refuse_manual_stop =
                parse_bool(v).ok_or_else(|| bad(path, "Unit", e, "expected bool"))?;
        }
        "AllowIsolate" => {
            unit.allow_isolate =
                parse_bool(v).ok_or_else(|| bad(path, "Unit", e, "expected bool"))?;
        }
        "JobTimeoutSec" => {
            unit.job_timeout_sec = Some(
                parse_duration(v, 1).map_err(|r| bad(path, "Unit", e, r))?,
            );
        }
        "JobRunningTimeoutSec" => {
            unit.job_running_timeout_sec = Some(
                parse_duration(v, 1).map_err(|r| bad(path, "Unit", e, r))?,
            );
        }
        "OnFailureJobMode" => unit.on_failure_job_mode = Some(v.to_owned()),
        "IgnoreOnIsolate" => {
            unit.ignore_on_isolate =
                parse_bool(v).ok_or_else(|| bad(path, "Unit", e, "expected bool"))?;
        }
        "CollectMode" => unit.collect_mode = Some(v.to_owned()),
        "FailureAction" => unit.failure_action = Some(v.to_owned()),
        "SuccessAction" => unit.success_action = Some(v.to_owned()),
        "RebootArgument" => unit.reboot_argument = Some(v.to_owned()),
        "SourcePath" => unit.source_path = Some(PathBuf::from(v)),
        // Conditions / asserts.
        k if k.starts_with("Condition") => {
            let c = Condition::parse(&k["Condition".len()..], v);
            unit.conditions.conditions.push(c);
        }
        k if k.starts_with("Assert") => {
            let c = Condition::parse(&k["Assert".len()..], v);
            unit.conditions.asserts.push(c);
        }
        _ => {
            warnings.push(ParseWarning::unknown(
                Some(path.to_owned()),
                e.line,
                "Unit",
                &e.key,
            ));
        }
    }
    Ok(())
}

fn apply_install_section(
    path: &Path,
    unit: &mut Unit,
    e: &crate::ini::Entry,
    warnings: &mut Vec<ParseWarning>,
) -> Result<(), ParseError> {
    let v = e.value.as_str();
    match e.key.as_str() {
        "WantedBy" => unit.install.wanted_by.extend(parse_units_list(v)),
        "RequiredBy" => unit.install.required_by.extend(parse_units_list(v)),
        "UpheldBy" => unit.install.upheld_by.extend(parse_units_list(v)),
        "Also" => unit.install.also.extend(parse_units_list(v)),
        "Alias" => unit.install.alias.extend(parse_units_list(v)),
        "DefaultInstance" => unit.install.default_instance = Some(v.to_owned()),
        _ => {
            warnings.push(ParseWarning::unknown(
                Some(path.to_owned()),
                e.line,
                "Install",
                &e.key,
            ));
        }
    }
    Ok(())
}

// ---------- [Service] ----------

fn parse_service(
    ini: &Ini,
    unit: &mut Unit,
    path: &Path,
    warnings: &mut Vec<ParseWarning>,
) -> Result<(), ParseError> {
    let UnitTypeData::Service(ref mut svc) = unit.kind else { unreachable!() };
    svc.restart_sec = ServiceUnit::DEFAULT_RESTART_SEC;
    svc.guess_main_pid = true;

    let Some(entries) = ini.sections.get("Service") else { return Ok(()) };
    for e in entries {
        let v = e.value.as_str();
        if linux_only::is_linux_only("Service", &e.key) {
            warnings.push(ParseWarning::linux_only(
                Some(path.to_owned()),
                e.line,
                "Service",
                &e.key,
            ));
            svc.passthrough.push((
                "Service".to_owned(),
                e.key.clone(),
                e.value.clone(),
            ));
            continue;
        }
        if linux_only::is_known_passthrough("Service", &e.key) {
            svc.passthrough.push((
                "Service".to_owned(),
                e.key.clone(),
                e.value.clone(),
            ));
            continue;
        }
        match e.key.as_str() {
            "Type" => {
                svc.service_type = ServiceType::parse(v).map_err(|r| bad(path, "Service", e, r))?;
            }
            "RemainAfterExit" => {
                svc.remain_after_exit =
                    parse_bool(v).ok_or_else(|| bad(path, "Service", e, "expected bool"))?;
            }
            "GuessMainPID" => {
                svc.guess_main_pid =
                    parse_bool(v).ok_or_else(|| bad(path, "Service", e, "expected bool"))?;
            }
            "PIDFile" => svc.pid_file = Some(PathBuf::from(v)),
            "BusName" => svc.bus_name = Some(v.to_owned()),
            "NotifyAccess" => {
                svc.notify_access =
                    NotifyAccess::parse(v).map_err(|r| bad(path, "Service", e, r))?;
            }
            "ExecStartPre" => svc
                .exec_start_pre
                .push(ExecCommand::parse(v).map_err(|r| bad(path, "Service", e, r))?),
            "ExecStart" => svc
                .exec_start
                .push(ExecCommand::parse(v).map_err(|r| bad(path, "Service", e, r))?),
            "ExecStartPost" => svc
                .exec_start_post
                .push(ExecCommand::parse(v).map_err(|r| bad(path, "Service", e, r))?),
            "ExecCondition" => svc
                .exec_condition
                .push(ExecCommand::parse(v).map_err(|r| bad(path, "Service", e, r))?),
            "ExecReload" => svc
                .exec_reload
                .push(ExecCommand::parse(v).map_err(|r| bad(path, "Service", e, r))?),
            "ExecStop" => svc
                .exec_stop
                .push(ExecCommand::parse(v).map_err(|r| bad(path, "Service", e, r))?),
            "ExecStopPost" => svc
                .exec_stop_post
                .push(ExecCommand::parse(v).map_err(|r| bad(path, "Service", e, r))?),
            "Restart" => {
                svc.restart = Restart::parse(v).map_err(|r| bad(path, "Service", e, r))?;
            }
            "RestartSec" => {
                svc.restart_sec = parse_duration(v, 1).map_err(|r| bad(path, "Service", e, r))?;
            }
            "RestartSteps" => {
                svc.restart_steps = v.parse().map_err(|_| bad(path, "Service", e, "u32"))?;
            }
            "RestartMaxDelaySec" => {
                svc.restart_max_delay_sec =
                    Some(parse_duration(v, 1).map_err(|r| bad(path, "Service", e, r))?);
            }
            "TimeoutStartSec" => {
                svc.timeout_start_sec =
                    Some(parse_duration(v, 1).map_err(|r| bad(path, "Service", e, r))?);
            }
            "TimeoutStopSec" => {
                svc.timeout_stop_sec =
                    Some(parse_duration(v, 1).map_err(|r| bad(path, "Service", e, r))?);
            }
            "TimeoutAbortSec" => {
                svc.timeout_abort_sec =
                    Some(parse_duration(v, 1).map_err(|r| bad(path, "Service", e, r))?);
            }
            "TimeoutSec" => {
                let d = parse_duration(v, 1).map_err(|r| bad(path, "Service", e, r))?;
                svc.timeout_start_sec = Some(d);
                svc.timeout_stop_sec = Some(d);
            }
            "RuntimeMaxSec" => {
                svc.runtime_max_sec =
                    Some(parse_duration(v, 1).map_err(|r| bad(path, "Service", e, r))?);
            }
            "RuntimeRandomizedExtraSec" => {
                svc.runtime_random_extra_sec =
                    Some(parse_duration(v, 1).map_err(|r| bad(path, "Service", e, r))?);
            }
            "WatchdogSec" => {
                svc.watchdog_sec =
                    Some(parse_duration(v, 1).map_err(|r| bad(path, "Service", e, r))?);
            }
            "StartLimitIntervalSec" | "StartLimitInterval" => {
                svc.start_limit_interval_sec =
                    Some(parse_duration(v, 1).map_err(|r| bad(path, "Service", e, r))?);
            }
            "StartLimitBurst" => {
                svc.start_limit_burst = v.parse().map_err(|_| bad(path, "Service", e, "u32"))?;
            }
            "StartLimitAction" => svc.start_limit_action = Some(v.to_owned()),
            "Environment" => svc
                .environment
                .extend(parse_environment_line(v).map_err(|r| bad(path, "Service", e, r))?),
            "EnvironmentFile" => svc.environment_files.push(EnvironmentFile::parse(v)),
            "PassEnvironment" => svc.pass_environment.extend(parse_list(v)),
            "UnsetEnvironment" => svc.unset_environment.extend(parse_list(v)),
            "WorkingDirectory" => svc.working_directory = Some(PathBuf::from(v)),
            "RootDirectory" => svc.root_directory = Some(PathBuf::from(v)),
            "User" => svc.user = Some(v.to_owned()),
            "Group" => svc.group = Some(v.to_owned()),
            "SupplementaryGroups" => svc.supplementary_groups.extend(parse_list(v)),
            "UMask" => {
                svc.umask = Some(
                    u32::from_str_radix(v.trim_start_matches("0o").trim_start_matches('0'), 8)
                        .or_else(|_| u32::from_str_radix(v, 8))
                        .map_err(|_| bad(path, "Service", e, "octal mode"))?,
                );
            }
            "Nice" => {
                svc.nice = Some(v.parse().map_err(|_| bad(path, "Service", e, "i32"))?);
            }
            "StandardInput" => {
                svc.standard_input =
                    StandardStream::parse(v).map_err(|r| bad(path, "Service", e, r))?;
            }
            "StandardOutput" => {
                svc.standard_output =
                    StandardStream::parse(v).map_err(|r| bad(path, "Service", e, r))?;
            }
            "StandardError" => {
                svc.standard_error =
                    StandardStream::parse(v).map_err(|r| bad(path, "Service", e, r))?;
            }
            "KillMode" => {
                svc.kill_mode = KillMode::parse(v).map_err(|r| bad(path, "Service", e, r))?;
            }
            "KillSignal" => svc.kill_signal = Some(v.to_owned()),
            "RestartKillSignal" => svc.restart_kill_signal = Some(v.to_owned()),
            "FinalKillSignal" => svc.final_kill_signal = Some(v.to_owned()),
            "WatchdogSignal" => svc.watchdog_signal = Some(v.to_owned()),
            "SendSIGKILL" => {
                svc.send_sigkill =
                    parse_bool(v).ok_or_else(|| bad(path, "Service", e, "expected bool"))?;
            }
            "SendSIGHUP" => {
                svc.send_sighup =
                    parse_bool(v).ok_or_else(|| bad(path, "Service", e, "expected bool"))?;
            }
            "SuccessExitStatus" => {
                for tok in parse_list(v) {
                    svc.success_exit_status
                        .push(ExitStatus::parse(&tok).map_err(|r| bad(path, "Service", e, r))?);
                }
            }
            "RestartPreventExitStatus" => {
                for tok in parse_list(v) {
                    svc.restart_prevent_exit_status
                        .push(ExitStatus::parse(&tok).map_err(|r| bad(path, "Service", e, r))?);
                }
            }
            "RestartForceExitStatus" => {
                for tok in parse_list(v) {
                    svc.restart_force_exit_status
                        .push(ExitStatus::parse(&tok).map_err(|r| bad(path, "Service", e, r))?);
                }
            }
            "LimitNOFILE" => svc.limit_nofile = Some(RLimit::parse(v).map_err(|r| bad(path, "Service", e, r))?),
            "LimitNPROC" => svc.limit_nproc = Some(RLimit::parse(v).map_err(|r| bad(path, "Service", e, r))?),
            "LimitCORE" => svc.limit_core = Some(RLimit::parse(v).map_err(|r| bad(path, "Service", e, r))?),
            "LimitAS" => svc.limit_as = Some(RLimit::parse(v).map_err(|r| bad(path, "Service", e, r))?),
            "LimitDATA" => svc.limit_data = Some(RLimit::parse(v).map_err(|r| bad(path, "Service", e, r))?),
            "LimitSTACK" => svc.limit_stack = Some(RLimit::parse(v).map_err(|r| bad(path, "Service", e, r))?),
            "LimitFSIZE" => svc.limit_fsize = Some(RLimit::parse(v).map_err(|r| bad(path, "Service", e, r))?),
            "LimitCPU" => svc.limit_cpu = Some(RLimit::parse(v).map_err(|r| bad(path, "Service", e, r))?),
            "LimitRSS" => svc.limit_rss = Some(RLimit::parse(v).map_err(|r| bad(path, "Service", e, r))?),
            "LimitMEMLOCK" => svc.limit_memlock = Some(RLimit::parse(v).map_err(|r| bad(path, "Service", e, r))?),
            "LimitMSGQUEUE" => svc.limit_msgqueue = Some(RLimit::parse(v).map_err(|r| bad(path, "Service", e, r))?),
            "LimitNICE" => svc.limit_nice = Some(RLimit::parse(v).map_err(|r| bad(path, "Service", e, r))?),
            "LimitRTPRIO" => svc.limit_rtprio = Some(RLimit::parse(v).map_err(|r| bad(path, "Service", e, r))?),
            "LimitRTTIME" => svc.limit_rttime = Some(RLimit::parse(v).map_err(|r| bad(path, "Service", e, r))?),
            "LimitSIGPENDING" => svc.limit_sigpending = Some(RLimit::parse(v).map_err(|r| bad(path, "Service", e, r))?),
            "LimitLOCKS" => svc.limit_locks = Some(RLimit::parse(v).map_err(|r| bad(path, "Service", e, r))?),
            "Sockets" => svc.sockets.extend(parse_list(v)),
            "FileDescriptorStoreMax" => {
                svc.fd_store_max = v.parse().map_err(|_| bad(path, "Service", e, "u32"))?;
            }
            "FileDescriptorStorePreserve" => svc.fd_store_preserve = Some(v.to_owned()),
            _ => {
                warnings.push(ParseWarning::unknown(
                    Some(path.to_owned()),
                    e.line,
                    "Service",
                    &e.key,
                ));
                svc.passthrough.push((
                    "Service".to_owned(),
                    e.key.clone(),
                    e.value.clone(),
                ));
            }
        }
    }
    Ok(())
}

// ---------- [Socket] ----------

fn parse_socket(
    ini: &Ini,
    unit: &mut Unit,
    path: &Path,
    warnings: &mut Vec<ParseWarning>,
) -> Result<(), ParseError> {
    let UnitTypeData::Socket(ref mut sock) = unit.kind else { unreachable!() };
    let Some(entries) = ini.sections.get("Socket") else { return Ok(()) };
    for e in entries {
        let v = e.value.as_str();
        match e.key.as_str() {
            "ListenStream" => sock
                .listen
                .push(ListenSpec::Stream(SocketAddrSpec::parse(v).map_err(|r| bad(path, "Socket", e, r))?)),
            "ListenDatagram" => sock
                .listen
                .push(ListenSpec::Datagram(SocketAddrSpec::parse(v).map_err(|r| bad(path, "Socket", e, r))?)),
            "ListenSequentialPacket" => sock
                .listen
                .push(ListenSpec::SequentialPacket(SocketAddrSpec::parse(v).map_err(|r| bad(path, "Socket", e, r))?)),
            "ListenFIFO" => sock.listen.push(ListenSpec::Fifo(PathBuf::from(v))),
            "ListenSpecial" => sock.listen.push(ListenSpec::Special(PathBuf::from(v))),
            "ListenNetlink" => sock.listen.push(ListenSpec::Netlink(v.to_owned())),
            "ListenMessageQueue" => sock.listen.push(ListenSpec::MessageQueue(v.to_owned())),
            "ListenUSBFunction" => sock.listen.push(ListenSpec::UsbFunction(PathBuf::from(v))),
            "Accept" => {
                sock.accept = parse_bool(v).ok_or_else(|| bad(path, "Socket", e, "bool"))?;
            }
            "Service" => {
                sock.service = Some(UnitName::from_str(v).map_err(|_| bad(path, "Socket", e, "unit name"))?);
            }
            "MaxConnections" => {
                sock.max_connections = v.parse().map_err(|_| bad(path, "Socket", e, "u32"))?;
            }
            "MaxConnectionsPerSource" => {
                sock.max_connections_per_source =
                    v.parse().map_err(|_| bad(path, "Socket", e, "u32"))?;
            }
            "Backlog" => {
                sock.backlog = v.parse().map_err(|_| bad(path, "Socket", e, "u32"))?;
            }
            "KeepAlive" => {
                sock.keep_alive = parse_bool(v).ok_or_else(|| bad(path, "Socket", e, "bool"))?;
            }
            "KeepAliveTimeSec" => {
                sock.keep_alive_time_sec =
                    Some(parse_duration(v, 1).map_err(|r| bad(path, "Socket", e, r))?);
            }
            "KeepAliveIntervalSec" => {
                sock.keep_alive_interval_sec =
                    Some(parse_duration(v, 1).map_err(|r| bad(path, "Socket", e, r))?);
            }
            "KeepAliveProbes" => {
                sock.keep_alive_probes = Some(v.parse().map_err(|_| bad(path, "Socket", e, "u32"))?);
            }
            "NoDelay" => {
                sock.no_delay = parse_bool(v).ok_or_else(|| bad(path, "Socket", e, "bool"))?;
            }
            "Priority" => {
                sock.priority = Some(v.parse().map_err(|_| bad(path, "Socket", e, "i32"))?);
            }
            "DeferAcceptSec" => {
                sock.defer_accept_sec =
                    Some(parse_duration(v, 1).map_err(|r| bad(path, "Socket", e, r))?);
            }
            "ReceiveBuffer" => {
                sock.receive_buffer =
                    Some(v.parse().map_err(|_| bad(path, "Socket", e, "u64"))?);
            }
            "SendBuffer" => {
                sock.send_buffer = Some(v.parse().map_err(|_| bad(path, "Socket", e, "u64"))?);
            }
            "IPTOS" => sock.ip_tos = Some(v.to_owned()),
            "IPTTL" => {
                sock.ip_ttl = Some(v.parse().map_err(|_| bad(path, "Socket", e, "u32"))?);
            }
            "Mark" => {
                sock.mark = Some(v.parse().map_err(|_| bad(path, "Socket", e, "i32"))?);
            }
            "ReusePort" => {
                sock.reuse_port = parse_bool(v).ok_or_else(|| bad(path, "Socket", e, "bool"))?;
            }
            "Transparent" => {
                sock.transparent = parse_bool(v).ok_or_else(|| bad(path, "Socket", e, "bool"))?;
            }
            "Broadcast" => {
                sock.broadcast = parse_bool(v).ok_or_else(|| bad(path, "Socket", e, "bool"))?;
            }
            "PassCredentials" => {
                sock.pass_credentials =
                    parse_bool(v).ok_or_else(|| bad(path, "Socket", e, "bool"))?;
                warnings.push(ParseWarning::linux_only(Some(path.to_owned()), e.line, "Socket", "PassCredentials"));
            }
            "PassSecurity" => {
                sock.pass_security =
                    parse_bool(v).ok_or_else(|| bad(path, "Socket", e, "bool"))?;
                warnings.push(ParseWarning::linux_only(Some(path.to_owned()), e.line, "Socket", "PassSecurity"));
            }
            "PassPacketInfo" => {
                sock.pass_packet_info =
                    parse_bool(v).ok_or_else(|| bad(path, "Socket", e, "bool"))?;
            }
            "BindIPv6Only" => {
                sock.bind_ipv6_only =
                    BindIPv6Only::parse(v).map_err(|r| bad(path, "Socket", e, r))?;
            }
            "BindToDevice" => {
                sock.bind_to_device = Some(v.to_owned());
                warnings.push(ParseWarning::linux_only(Some(path.to_owned()), e.line, "Socket", "BindToDevice"));
            }
            "SocketUser" => sock.socket_user = Some(v.to_owned()),
            "SocketGroup" => sock.socket_group = Some(v.to_owned()),
            "SocketMode" => {
                sock.socket_mode = Some(
                    u32::from_str_radix(v, 8)
                        .map_err(|_| bad(path, "Socket", e, "octal mode"))?,
                );
            }
            "DirectoryMode" => {
                sock.directory_mode = Some(
                    u32::from_str_radix(v, 8)
                        .map_err(|_| bad(path, "Socket", e, "octal mode"))?,
                );
            }
            "RemoveOnStop" => {
                sock.remove_on_stop =
                    parse_bool(v).ok_or_else(|| bad(path, "Socket", e, "bool"))?;
            }
            "Symlinks" => sock.symlinks.extend(parse_list(v).into_iter().map(PathBuf::from)),
            "FileDescriptorName" => sock.file_descriptor_name = Some(v.to_owned()),
            "TriggerLimitIntervalSec" => {
                sock.trigger_limit_interval_sec =
                    Some(parse_duration(v, 1).map_err(|r| bad(path, "Socket", e, r))?);
            }
            "TriggerLimitBurst" => {
                sock.trigger_limit_burst =
                    Some(v.parse().map_err(|_| bad(path, "Socket", e, "u32"))?);
            }
            "MessageQueueMaxMessages" => {
                sock.mq_max_messages =
                    Some(v.parse().map_err(|_| bad(path, "Socket", e, "i64"))?);
            }
            "MessageQueueMessageSize" => {
                sock.mq_message_size =
                    Some(v.parse().map_err(|_| bad(path, "Socket", e, "i64"))?);
            }
            _ => {
                warnings.push(ParseWarning::unknown(
                    Some(path.to_owned()),
                    e.line,
                    "Socket",
                    &e.key,
                ));
                sock.passthrough.push((e.key.clone(), e.value.clone()));
            }
        }
    }
    Ok(())
}

// ---------- [Timer] ----------

fn parse_timer(
    ini: &Ini,
    unit: &mut Unit,
    path: &Path,
    warnings: &mut Vec<ParseWarning>,
) -> Result<(), ParseError> {
    let UnitTypeData::Timer(ref mut tm) = unit.kind else { unreachable!() };
    tm.remain_after_elapse = true;

    let Some(entries) = ini.sections.get("Timer") else { return Ok(()) };
    for e in entries {
        let v = e.value.as_str();
        match e.key.as_str() {
            "OnCalendar" => {
                tm.on_calendar.push(
                    CalendarSpec::parse(v).map_err(|r| bad(path, "Timer", e, r))?,
                );
            }
            "OnActiveSec" => {
                tm.on_active_sec =
                    Some(parse_duration(v, 1).map_err(|r| bad(path, "Timer", e, r))?);
            }
            "OnBootSec" => {
                tm.on_boot_sec =
                    Some(parse_duration(v, 1).map_err(|r| bad(path, "Timer", e, r))?);
            }
            "OnStartupSec" => {
                tm.on_startup_sec =
                    Some(parse_duration(v, 1).map_err(|r| bad(path, "Timer", e, r))?);
            }
            "OnUnitActiveSec" => {
                tm.on_unit_active_sec =
                    Some(parse_duration(v, 1).map_err(|r| bad(path, "Timer", e, r))?);
            }
            "OnUnitInactiveSec" => {
                tm.on_unit_inactive_sec =
                    Some(parse_duration(v, 1).map_err(|r| bad(path, "Timer", e, r))?);
            }
            "OnClockChange" => {
                tm.on_clock_change =
                    parse_bool(v).ok_or_else(|| bad(path, "Timer", e, "bool"))?;
            }
            "OnTimezoneChange" => {
                tm.on_timezone_change =
                    parse_bool(v).ok_or_else(|| bad(path, "Timer", e, "bool"))?;
            }
            "AccuracySec" => {
                tm.accuracy_sec =
                    Some(parse_duration(v, 1).map_err(|r| bad(path, "Timer", e, r))?);
            }
            "RandomizedDelaySec" => {
                tm.randomized_delay_sec =
                    Some(parse_duration(v, 1).map_err(|r| bad(path, "Timer", e, r))?);
            }
            "FixedRandomDelay" => {
                tm.fixed_random_delay =
                    parse_bool(v).ok_or_else(|| bad(path, "Timer", e, "bool"))?;
            }
            "Persistent" => {
                tm.persistent = parse_bool(v).ok_or_else(|| bad(path, "Timer", e, "bool"))?;
            }
            "WakeSystem" => {
                tm.wake_system = parse_bool(v).ok_or_else(|| bad(path, "Timer", e, "bool"))?;
            }
            "RemainAfterElapse" => {
                tm.remain_after_elapse =
                    parse_bool(v).ok_or_else(|| bad(path, "Timer", e, "bool"))?;
            }
            "DeferReactivation" => {
                tm.defer_reactivation =
                    parse_bool(v).ok_or_else(|| bad(path, "Timer", e, "bool"))?;
            }
            "Unit" => {
                tm.unit = Some(
                    UnitName::from_str(v).map_err(|_| bad(path, "Timer", e, "unit name"))?,
                );
            }
            _ => {
                warnings.push(ParseWarning::unknown(
                    Some(path.to_owned()),
                    e.line,
                    "Timer",
                    &e.key,
                ));
            }
        }
    }
    Ok(())
}

// ---------- [Path] ----------

fn parse_path(
    ini: &Ini,
    unit: &mut Unit,
    path: &Path,
    warnings: &mut Vec<ParseWarning>,
) -> Result<(), ParseError> {
    let UnitTypeData::Path(ref mut p) = unit.kind else { unreachable!() };
    let Some(entries) = ini.sections.get("Path") else { return Ok(()) };
    for e in entries {
        let v = e.value.as_str();
        match e.key.as_str() {
            "PathExists" => p.watches.push(PathWatch::Exists(PathBuf::from(v))),
            "PathExistsGlob" => p.watches.push(PathWatch::ExistsGlob(v.to_owned())),
            "PathChanged" => p.watches.push(PathWatch::Changed(PathBuf::from(v))),
            "PathModified" => p.watches.push(PathWatch::Modified(PathBuf::from(v))),
            "DirectoryNotEmpty" => p
                .watches
                .push(PathWatch::DirectoryNotEmpty(PathBuf::from(v))),
            "Unit" => {
                p.unit = Some(UnitName::from_str(v).map_err(|_| bad(path, "Path", e, "unit name"))?);
            }
            "MakeDirectory" => {
                p.make_directory = parse_bool(v).ok_or_else(|| bad(path, "Path", e, "bool"))?;
            }
            "DirectoryMode" => {
                p.directory_mode = Some(
                    u32::from_str_radix(v, 8)
                        .map_err(|_| bad(path, "Path", e, "octal mode"))?,
                );
            }
            "TriggerLimitIntervalSec" => {
                p.trigger_limit_interval_sec =
                    Some(parse_duration(v, 1).map_err(|r| bad(path, "Path", e, r))?);
            }
            "TriggerLimitBurst" => {
                p.trigger_limit_burst =
                    Some(v.parse().map_err(|_| bad(path, "Path", e, "u32"))?);
            }
            _ => {
                warnings.push(ParseWarning::unknown(
                    Some(path.to_owned()),
                    e.line,
                    "Path",
                    &e.key,
                ));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_service() {
        let src = "[Unit]\nDescription=hello\n[Service]\nExecStart=/bin/true\n";
        let p = parse_unit_str("foo.service".parse().unwrap(), src, None).unwrap();
        assert_eq!(p.unit.description, "hello");
        let UnitTypeData::Service(svc) = &p.unit.kind else {
            panic!()
        };
        assert_eq!(svc.exec_start.len(), 1);
        assert_eq!(svc.exec_start[0].program, "/bin/true");
    }

    #[test]
    fn linux_only_warns_but_loads() {
        let src = "[Service]\nExecStart=/bin/true\nPrivateTmp=yes\nMemoryMax=100M\n";
        let p = parse_unit_str("foo.service".parse().unwrap(), src, None).unwrap();
        assert!(p.warnings.iter().any(|w| w.message.contains("PrivateTmp")));
        assert!(p.warnings.iter().any(|w| w.message.contains("MemoryMax")));
    }

    #[test]
    fn install_section() {
        let src = "[Service]\nExecStart=/bin/true\n[Install]\nWantedBy=default.target multi-user.target\n";
        let p = parse_unit_str("foo.service".parse().unwrap(), src, None).unwrap();
        assert_eq!(p.unit.install.wanted_by.len(), 2);
    }

    #[test]
    fn timer_oncalendar() {
        let src = "[Timer]\nOnCalendar=daily\nPersistent=yes\n";
        let p = parse_unit_str("foo.timer".parse().unwrap(), src, None).unwrap();
        let UnitTypeData::Timer(t) = &p.unit.kind else {
            panic!()
        };
        assert_eq!(t.on_calendar.len(), 1);
        assert!(t.persistent);
    }

    #[test]
    fn socket_listen_stream() {
        let src = "[Socket]\nListenStream=/tmp/x.sock\nAccept=yes\n";
        let p = parse_unit_str("foo.socket".parse().unwrap(), src, None).unwrap();
        let UnitTypeData::Socket(s) = &p.unit.kind else {
            panic!()
        };
        assert!(s.accept);
        assert_eq!(s.listen.len(), 1);
    }

    #[test]
    fn path_watch() {
        let src = "[Path]\nPathChanged=/tmp/foo\n[Install]\nWantedBy=default.target\n";
        let p = parse_unit_str("foo.path".parse().unwrap(), src, None).unwrap();
        let UnitTypeData::Path(pa) = &p.unit.kind else {
            panic!()
        };
        assert_eq!(pa.watches.len(), 1);
    }
}
