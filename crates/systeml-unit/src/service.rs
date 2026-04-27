//! `[Service]` section: typed view + parsing helpers.

use crate::duration::SdDuration;
use crate::env::EnvironmentFile;
use crate::exec::ExecCommand;
use std::path::PathBuf;
use std::time::Duration;

/// `Type=` — service main-process model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum ServiceType {
    /// Process is the main service. Started when fork+exec returns. **Default.**
    #[default]
    Simple,
    /// Like simple but signals start completion at the moment of `execve`.
    Exec,
    /// Process forks; the original exits, the child is the main process.
    Forking,
    /// Process runs to completion. Default `RemainAfterExit=no`.
    Oneshot,
    /// Process notifies readiness via `sd_notify(READY=1)`.
    Notify,
    /// Like Notify but only stops on explicit `STOPPING=1`.
    NotifyReload,
    /// Process acquires `BusName=` on D-Bus.
    Dbus,
    /// Like simple but delayed until current jobs complete.
    Idle,
}

impl ServiceType {
    /// Parse from systemd directive value.
    pub fn parse(s: &str) -> Result<Self, String> {
        Ok(match s {
            "simple" => Self::Simple,
            "exec" => Self::Exec,
            "forking" => Self::Forking,
            "oneshot" => Self::Oneshot,
            "notify" => Self::Notify,
            "notify-reload" => Self::NotifyReload,
            "dbus" => Self::Dbus,
            "idle" => Self::Idle,
            other => return Err(format!("unknown Type= {other:?}")),
        })
    }
}

/// `Restart=` policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Restart {
    /// `no` — never auto-restart. **Default.**
    #[default]
    No,
    /// `always`.
    Always,
    /// `on-success` — only when exit code is in SuccessExitStatus.
    OnSuccess,
    /// `on-failure` — non-zero exit, signal, watchdog, or core dump.
    OnFailure,
    /// `on-abnormal` — signal, watchdog, or core dump (not non-zero exit).
    OnAbnormal,
    /// `on-watchdog`.
    OnWatchdog,
    /// `on-abort` — uncaught fatal signal only.
    OnAbort,
}

impl Restart {
    /// Parse from systemd directive value.
    pub fn parse(s: &str) -> Result<Self, String> {
        Ok(match s {
            "no" => Self::No,
            "always" => Self::Always,
            "on-success" => Self::OnSuccess,
            "on-failure" => Self::OnFailure,
            "on-abnormal" => Self::OnAbnormal,
            "on-watchdog" => Self::OnWatchdog,
            "on-abort" => Self::OnAbort,
            other => return Err(format!("unknown Restart= {other:?}")),
        })
    }

    /// Should we restart given an outcome?
    #[must_use]
    pub fn should_restart(self, outcome: ProcessOutcome) -> bool {
        use ProcessOutcome::*;
        use Restart::*;
        match (self, outcome) {
            (No, _) => false,
            (Always, _) => true,
            (OnSuccess, ExitedSuccess) => true,
            (OnFailure, ExitedNonZero | Signaled | Watchdog | CoreDumped) => true,
            (OnAbnormal, Signaled | Watchdog | CoreDumped) => true,
            (OnWatchdog, Watchdog) => true,
            (OnAbort, Signaled | CoreDumped) => true,
            _ => false,
        }
    }
}

/// What happened to a child process — used for restart decisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessOutcome {
    /// exit(0) (or in SuccessExitStatus).
    ExitedSuccess,
    /// exit(non-zero) (and not in SuccessExitStatus).
    ExitedNonZero,
    /// Killed by signal.
    Signaled,
    /// Watchdog fired.
    Watchdog,
    /// Core dumped.
    CoreDumped,
}

/// `NotifyAccess=`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum NotifyAccess {
    /// Default for non-notify types.
    #[default]
    None,
    /// Only main PID may notify.
    Main,
    /// Only main PID and `Exec*=` children may notify.
    Exec,
    /// Any process in the cgroup may notify.
    All,
}

impl NotifyAccess {
    /// Parse from directive value.
    pub fn parse(s: &str) -> Result<Self, String> {
        Ok(match s {
            "none" => Self::None,
            "main" => Self::Main,
            "exec" => Self::Exec,
            "all" => Self::All,
            other => return Err(format!("unknown NotifyAccess= {other:?}")),
        })
    }
}

/// `KillMode=`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum KillMode {
    /// Send signal to every process in the cgroup. **systemd default.** On
    /// macOS we approximate via process-group kill.
    #[default]
    ControlGroup,
    /// Kill only the main process.
    Process,
    /// Send SIGTERM to main, SIGKILL to whole group on timeout.
    Mixed,
    /// Don't send anything; rely on `ExecStop=`.
    None,
}

impl KillMode {
    /// Parse.
    pub fn parse(s: &str) -> Result<Self, String> {
        Ok(match s {
            "control-group" | "cgroup" => Self::ControlGroup,
            "process" => Self::Process,
            "mixed" => Self::Mixed,
            "none" => Self::None,
            other => return Err(format!("unknown KillMode= {other:?}")),
        })
    }
}

/// `StandardInput=` / `StandardOutput=` / `StandardError=`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum StandardStream {
    /// Inherit from manager.
    #[default]
    Inherit,
    /// `/dev/null`.
    Null,
    /// TTY at `TTYPath=`.
    Tty,
    /// Read/append to a file (`file:/path`, `append:/path`, `truncate:/path`).
    File(PathBuf),
    /// Append to a file.
    Append(PathBuf),
    /// Truncate to a file.
    Truncate(PathBuf),
    /// Connect to a socket — used for stdio of socket-activated services.
    Socket,
    /// `fd:NAME` — connect to named fd from socket activation.
    Fd(String),
    /// `journal` / `journal+console` / etc. — accepted but routed to file
    /// fallback under SystemL (no journald). Stored verbatim for round-trip.
    Journal(String),
    /// `kmsg`, `syslog` — Linux-only sinks; logged-and-ignored.
    LinuxOnly(String),
}

impl StandardStream {
    /// Parse a `StandardOutput=`-style directive value.
    pub fn parse(s: &str) -> Result<Self, String> {
        Ok(match s {
            "inherit" => Self::Inherit,
            "null" => Self::Null,
            "tty" => Self::Tty,
            "socket" => Self::Socket,
            s if s.starts_with("file:") => Self::File(PathBuf::from(&s[5..])),
            s if s.starts_with("append:") => Self::Append(PathBuf::from(&s[7..])),
            s if s.starts_with("truncate:") => Self::Truncate(PathBuf::from(&s[9..])),
            s if s.starts_with("fd:") => Self::Fd(s[3..].to_owned()),
            s if s.starts_with("journal") => Self::Journal(s.to_owned()),
            s if s.starts_with("kmsg") || s.starts_with("syslog") => {
                Self::LinuxOnly(s.to_owned())
            }
            other => return Err(format!("unknown stdio sink {other:?}")),
        })
    }
}

/// Exit-status spec: a numeric code or a signal name.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum ExitStatus {
    /// Numeric exit code.
    Code(u8),
    /// Signal name (e.g. `SIGTERM`).
    Signal(String),
}

impl ExitStatus {
    /// Parse a single token.
    pub fn parse(s: &str) -> Result<Self, String> {
        if let Ok(n) = s.parse::<u8>() {
            Ok(Self::Code(n))
        } else if s.starts_with("SIG") {
            Ok(Self::Signal(s.to_owned()))
        } else {
            Err(format!("not an exit code or signal: {s:?}"))
        }
    }
}

/// `LimitNOFILE=` etc. — soft and optional hard limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct RLimit {
    /// Soft limit. `None` means infinity.
    pub soft: Option<u64>,
    /// Hard limit. `None` means infinity.
    pub hard: Option<u64>,
}

impl RLimit {
    /// Parse `Limit*=N` or `Limit*=N:M` or `Limit*=infinity`.
    pub fn parse(s: &str) -> Result<Self, String> {
        let one = |t: &str| -> Result<Option<u64>, String> {
            if t.eq_ignore_ascii_case("infinity") {
                Ok(None)
            } else {
                Ok(Some(t.parse().map_err(|e: std::num::ParseIntError| e.to_string())?))
            }
        };
        if let Some((s, h)) = s.split_once(':') {
            Ok(Self {
                soft: one(s)?,
                hard: one(h)?,
            })
        } else {
            let v = one(s)?;
            Ok(Self { soft: v, hard: v })
        }
    }
}

/// Fully-typed `[Service]` section.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ServiceUnit {
    /// `Type=` (default `simple`).
    pub service_type: ServiceType,
    /// `RemainAfterExit=`.
    pub remain_after_exit: bool,
    /// `GuessMainPID=`.
    pub guess_main_pid: bool,
    /// `PIDFile=`.
    pub pid_file: Option<PathBuf>,
    /// `BusName=` (for `Type=dbus`).
    pub bus_name: Option<String>,
    /// `NotifyAccess=`.
    pub notify_access: NotifyAccess,

    /// `ExecStartPre=`.
    pub exec_start_pre: Vec<ExecCommand>,
    /// `ExecStart=`.
    pub exec_start: Vec<ExecCommand>,
    /// `ExecStartPost=`.
    pub exec_start_post: Vec<ExecCommand>,
    /// `ExecCondition=`.
    pub exec_condition: Vec<ExecCommand>,
    /// `ExecReload=`.
    pub exec_reload: Vec<ExecCommand>,
    /// `ExecStop=`.
    pub exec_stop: Vec<ExecCommand>,
    /// `ExecStopPost=`.
    pub exec_stop_post: Vec<ExecCommand>,

    /// `Restart=` (default `no`).
    pub restart: Restart,
    /// `RestartSec=` (default 100ms).
    pub restart_sec: SdDuration,
    /// `RestartSteps=`.
    pub restart_steps: u32,
    /// `RestartMaxDelaySec=`.
    pub restart_max_delay_sec: Option<SdDuration>,
    /// `TimeoutStartSec=`.
    pub timeout_start_sec: Option<SdDuration>,
    /// `TimeoutStopSec=`.
    pub timeout_stop_sec: Option<SdDuration>,
    /// `TimeoutAbortSec=`.
    pub timeout_abort_sec: Option<SdDuration>,
    /// `RuntimeMaxSec=`.
    pub runtime_max_sec: Option<SdDuration>,
    /// `RuntimeRandomizedExtraSec=`.
    pub runtime_random_extra_sec: Option<SdDuration>,
    /// `WatchdogSec=`.
    pub watchdog_sec: Option<SdDuration>,
    /// `StartLimitIntervalSec=`.
    pub start_limit_interval_sec: Option<SdDuration>,
    /// `StartLimitBurst=`.
    pub start_limit_burst: u32,
    /// `StartLimitAction=`.
    pub start_limit_action: Option<String>,

    /// `Environment=`.
    pub environment: Vec<(String, String)>,
    /// `EnvironmentFile=`.
    pub environment_files: Vec<EnvironmentFile>,
    /// `PassEnvironment=`.
    pub pass_environment: Vec<String>,
    /// `UnsetEnvironment=`.
    pub unset_environment: Vec<String>,

    /// `WorkingDirectory=`. `~` expands to user home.
    pub working_directory: Option<PathBuf>,
    /// `RootDirectory=`.
    pub root_directory: Option<PathBuf>,
    /// `User=`.
    pub user: Option<String>,
    /// `Group=`.
    pub group: Option<String>,
    /// `SupplementaryGroups=`.
    pub supplementary_groups: Vec<String>,
    /// `UMask=` — octal.
    pub umask: Option<u32>,
    /// `Nice=`.
    pub nice: Option<i32>,

    /// `StandardInput=`.
    pub standard_input: StandardStream,
    /// `StandardOutput=`.
    pub standard_output: StandardStream,
    /// `StandardError=`.
    pub standard_error: StandardStream,

    /// `KillMode=`.
    pub kill_mode: KillMode,
    /// `KillSignal=` (default SIGTERM).
    pub kill_signal: Option<String>,
    /// `RestartKillSignal=`.
    pub restart_kill_signal: Option<String>,
    /// `FinalKillSignal=`.
    pub final_kill_signal: Option<String>,
    /// `WatchdogSignal=`.
    pub watchdog_signal: Option<String>,
    /// `SendSIGKILL=`.
    pub send_sigkill: bool,
    /// `SendSIGHUP=`.
    pub send_sighup: bool,

    /// `SuccessExitStatus=`.
    pub success_exit_status: Vec<ExitStatus>,
    /// `RestartPreventExitStatus=`.
    pub restart_prevent_exit_status: Vec<ExitStatus>,
    /// `RestartForceExitStatus=`.
    pub restart_force_exit_status: Vec<ExitStatus>,

    /// `LimitNOFILE=`.
    pub limit_nofile: Option<RLimit>,
    /// `LimitNPROC=`.
    pub limit_nproc: Option<RLimit>,
    /// `LimitCORE=`.
    pub limit_core: Option<RLimit>,
    /// `LimitAS=`.
    pub limit_as: Option<RLimit>,
    /// `LimitDATA=`.
    pub limit_data: Option<RLimit>,
    /// `LimitSTACK=`.
    pub limit_stack: Option<RLimit>,
    /// `LimitFSIZE=`.
    pub limit_fsize: Option<RLimit>,
    /// `LimitCPU=`.
    pub limit_cpu: Option<RLimit>,
    /// `LimitRSS=`.
    pub limit_rss: Option<RLimit>,
    /// `LimitMEMLOCK=`.
    pub limit_memlock: Option<RLimit>,
    /// `LimitMSGQUEUE=`.
    pub limit_msgqueue: Option<RLimit>,
    /// `LimitNICE=`.
    pub limit_nice: Option<RLimit>,
    /// `LimitRTPRIO=`.
    pub limit_rtprio: Option<RLimit>,
    /// `LimitRTTIME=`.
    pub limit_rttime: Option<RLimit>,
    /// `LimitSIGPENDING=`.
    pub limit_sigpending: Option<RLimit>,
    /// `LimitLOCKS=`.
    pub limit_locks: Option<RLimit>,
    /// `LimitNICE=`.

    /// `Sockets=` — explicit socket-units to attach for fd passing.
    pub sockets: Vec<String>,

    /// `FileDescriptorStoreMax=`.
    pub fd_store_max: u32,
    /// `FileDescriptorStorePreserve=`.
    pub fd_store_preserve: Option<String>,

    /// Linux-only / unknown directives we accepted verbatim. `(section, key, value)`.
    pub passthrough: Vec<(String, String, String)>,
}

impl ServiceUnit {
    /// Default `RestartSec=` per systemd: 100ms.
    pub const DEFAULT_RESTART_SEC: SdDuration =
        SdDuration::Finite(Duration::from_millis(100));
    /// Default `TimeoutStartSec=` per systemd: 90s.
    pub const DEFAULT_TIMEOUT_START_SEC: SdDuration =
        SdDuration::Finite(Duration::from_secs(90));
    /// Default `TimeoutStopSec=` per systemd: 90s.
    pub const DEFAULT_TIMEOUT_STOP_SEC: SdDuration =
        SdDuration::Finite(Duration::from_secs(90));
}
