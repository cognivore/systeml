//! `ServiceRunner`: spawn + supervise one `.service` unit.
//!
//! Layered:
//! 1. `start()` runs `ExecCondition`, `ExecStartPre`, then `ExecStart`,
//!    waits for readiness per `Type=`, then runs `ExecStartPost`.
//! 2. While running, `ServiceRunner` listens for child exit + (for notify)
//!    `sd_notify` traffic.
//! 3. On exit, applies `Restart=` policy with `RestartSec=` backoff and
//!    `RestartSteps=` exponential ramp.
//! 4. `stop()` runs `ExecStop`, sends `KillSignal`, then `SIGKILL` after
//!    `TimeoutStopSec`.

#![allow(unsafe_code)]

use crate::exec::{build_command, resolve_environment, setup_journal_loggers};
use crate::service::notify::{bind_socket, NotifyMessage};
use crate::service::pid_file::read_with_retry;
use crate::state::ActiveState;
use anyhow::{anyhow, Context, Result};
use std::collections::BTreeMap;
use std::process::ExitStatus;
use std::sync::Arc;
use std::time::{Duration, Instant};
use systeml_unit::exec::{ExecCommand, ExecFlags};
use systeml_unit::service::{
    KillMode, ProcessOutcome, ServiceType, ServiceUnit, StandardStream,
};
use systeml_unit::UnitName;
use tokio::process::Child;
use tokio::sync::Mutex;
use tokio::time::sleep;
use tracing::{info, warn};

/// Outcome of one supervised `start()` cycle.
#[derive(Debug, Clone)]
pub struct StartOutcome {
    /// Whether we reached `Active`.
    pub active: bool,
    /// Final state if start failed.
    pub error: Option<String>,
}

/// Supervises one .service unit. Owned by the manager and cloned by Arc.
pub struct ServiceRunner {
    /// Unit name (used for logging + journal-fallback paths).
    pub unit: UnitName,
    /// Parsed service definition.
    pub svc: ServiceUnit,
    /// Tracked main child (if any).
    pub child: Mutex<Option<Child>>,
    /// Last observed pid (used for forking + notify protocols).
    pub main_pid: Mutex<Option<i32>>,
    /// Current top-level state.
    pub state: Mutex<ActiveState>,
    /// Detailed sub-state (string per systemd convention).
    pub sub: Mutex<String>,
    /// Last status message reported via `sd_notify(STATUS=)`.
    pub status: Mutex<Option<String>>,
    /// Most recent restart count (for backoff).
    pub restarts: Mutex<u32>,
    /// Last process outcome (used for `Restart=` decisions).
    pub last_outcome: Mutex<Option<ProcessOutcome>>,
}

impl ServiceRunner {
    /// New, idle runner.
    pub fn new(unit: UnitName, svc: ServiceUnit) -> Arc<Self> {
        Arc::new(Self {
            unit,
            svc,
            child: Mutex::new(None),
            main_pid: Mutex::new(None),
            state: Mutex::new(ActiveState::Inactive),
            sub: Mutex::new("dead".into()),
            status: Mutex::new(None),
            restarts: Mutex::new(0),
            last_outcome: Mutex::new(None),
        })
    }

    /// Run the start sequence end-to-end. Sets `state` to `Active` on success.
    pub async fn start(&self) -> Result<StartOutcome> {
        *self.state.lock().await = ActiveState::Activating;
        *self.sub.lock().await = "start-pre".into();

        let env = resolve_environment(&self.svc)?;

        // ExecCondition — failure (non-zero) skips the unit silently.
        for cmd in &self.svc.exec_condition {
            let st = self.run_oneshot(cmd, &env, &[]).await?;
            if !st.success() && !cmd.flags.contains(ExecFlags::IGNORE_FAILURE) {
                *self.state.lock().await = ActiveState::Inactive;
                *self.sub.lock().await = "dead".into();
                return Ok(StartOutcome {
                    active: false,
                    error: Some("ExecCondition failed".into()),
                });
            }
        }

        // ExecStartPre.
        for cmd in &self.svc.exec_start_pre {
            let st = self.run_oneshot(cmd, &env, &[]).await?;
            if !st.success() && !cmd.flags.contains(ExecFlags::IGNORE_FAILURE) {
                *self.state.lock().await = ActiveState::Failed;
                *self.sub.lock().await = "failed".into();
                return Ok(StartOutcome {
                    active: false,
                    error: Some("ExecStartPre failed".into()),
                });
            }
        }

        // ExecStart.
        *self.sub.lock().await = "start".into();
        let exec_start = match self.svc.exec_start.first() {
            Some(c) => c,
            None if matches!(self.svc.service_type, ServiceType::Oneshot) => {
                // Oneshot can have no ExecStart (used as a placeholder).
                *self.state.lock().await = if self.svc.remain_after_exit {
                    ActiveState::Active
                } else {
                    ActiveState::Inactive
                };
                *self.sub.lock().await = "dead".into();
                return Ok(StartOutcome {
                    active: self.svc.remain_after_exit,
                    error: None,
                });
            }
            None => {
                return Ok(StartOutcome {
                    active: false,
                    error: Some("no ExecStart".into()),
                });
            }
        };

        // For notify/notify-reload, set up the socket *before* spawn so
        // NOTIFY_SOCKET is in env.
        let mut notify_extra: Vec<(String, String)> = Vec::new();
        let mut notify_sock_path: Option<std::path::PathBuf> = None;
        let mut notify_sock = None;
        if matches!(
            self.svc.service_type,
            ServiceType::Notify | ServiceType::NotifyReload
        ) {
            let runtime_dir = systeml_unit::search::runtime_dir()
                .or_else(|| std::env::temp_dir().into())
                .ok_or_else(|| anyhow!("no runtime dir for notify socket"))?;
            std::fs::create_dir_all(&runtime_dir).ok();
            let sock_path = runtime_dir.join(format!("notify.{}.sock", self.unit));
            let sock = bind_socket(&sock_path)
                .with_context(|| format!("bind notify socket at {sock_path:?}"))?;
            notify_extra.push((
                "NOTIFY_SOCKET".into(),
                sock_path.to_string_lossy().into_owned(),
            ));
            notify_sock_path = Some(sock_path);
            notify_sock = Some(sock);
        }

        // For socket-activated services, the manager wires LISTEN_FDS *via*
        // build_command's extra_env list before calling us. We don't do
        // anything special here.

        match self.svc.service_type {
            ServiceType::Oneshot => self.start_oneshot(exec_start, &env).await,
            ServiceType::Forking => self.start_forking(exec_start, &env).await,
            ServiceType::Simple | ServiceType::Exec | ServiceType::Idle => {
                self.start_simple(exec_start, &env).await
            }
            ServiceType::Notify | ServiceType::NotifyReload => {
                let sock = notify_sock.take().expect("notify socket built above");
                self.start_notify(exec_start, &env, &notify_extra, sock).await
            }
            ServiceType::Dbus => {
                warn!(unit = %self.unit, "Type=dbus is not supported on macOS; treating as simple");
                self.start_simple(exec_start, &env).await
            }
        }
        .inspect(|_| {
            // Cleanup notify socket file once start completes (sock is closed
            // when dropped; just remove the path).
            if let Some(p) = notify_sock_path.clone() {
                let _ = std::fs::remove_file(p);
            }
        })
    }

    async fn start_simple(
        &self,
        cmd: &ExecCommand,
        env: &BTreeMap<String, String>,
    ) -> Result<StartOutcome> {
        let mut command = build_command(&self.unit.to_string(), cmd, &self.svc, env, &[])?;
        let mut child = command
            .spawn()
            .with_context(|| format!("spawn {}", cmd.program))?;
        setup_journal_loggers(&mut child, &self.svc, &self.unit.to_string());
        if let Some(pid) = child.id() {
            *self.main_pid.lock().await = Some(pid as i32);
        }
        *self.child.lock().await = Some(child);
        *self.state.lock().await = ActiveState::Active;
        *self.sub.lock().await = "running".into();

        // ExecStartPost (best-effort).
        for c in &self.svc.exec_start_post {
            let _ = self.run_oneshot(c, env, &[]).await;
        }
        Ok(StartOutcome {
            active: true,
            error: None,
        })
    }

    async fn start_oneshot(
        &self,
        cmd: &ExecCommand,
        env: &BTreeMap<String, String>,
    ) -> Result<StartOutcome> {
        let st = self.run_oneshot(cmd, env, &[]).await?;
        if !st.success() && !cmd.flags.contains(ExecFlags::IGNORE_FAILURE) {
            *self.state.lock().await = ActiveState::Failed;
            *self.sub.lock().await = "failed".into();
            return Ok(StartOutcome {
                active: false,
                error: Some(format!("oneshot exit {st:?}")),
            });
        }
        let active = self.svc.remain_after_exit;
        *self.state.lock().await = if active {
            ActiveState::Active
        } else {
            ActiveState::Inactive
        };
        *self.sub.lock().await = "dead".into();

        for c in &self.svc.exec_start_post {
            let _ = self.run_oneshot(c, env, &[]).await;
        }
        Ok(StartOutcome { active, error: None })
    }

    async fn start_forking(
        &self,
        cmd: &ExecCommand,
        env: &BTreeMap<String, String>,
    ) -> Result<StartOutcome> {
        let mut command = build_command(&self.unit.to_string(), cmd, &self.svc, env, &[])?;
        let mut child = command
            .spawn()
            .with_context(|| format!("spawn {}", cmd.program))?;
        setup_journal_loggers(&mut child, &self.svc, &self.unit.to_string());
        // Wait for original to exit.
        let _ = child.wait().await;
        // Then read PIDFile (with retry).
        let timeout = self
            .svc
            .timeout_start_sec
            .map(|d| d.as_std())
            .unwrap_or(Duration::from_secs(90));
        if let Some(pf) = self.svc.pid_file.as_deref() {
            let deadline = Instant::now() + timeout;
            if let Some(pid) = read_with_retry(pf, deadline).await {
                *self.main_pid.lock().await = Some(pid);
            } else if !self.svc.guess_main_pid {
                *self.state.lock().await = ActiveState::Failed;
                *self.sub.lock().await = "failed".into();
                return Ok(StartOutcome {
                    active: false,
                    error: Some("PIDFile not produced in TimeoutStartSec".into()),
                });
            }
        }
        *self.state.lock().await = ActiveState::Active;
        *self.sub.lock().await = "running".into();
        for c in &self.svc.exec_start_post {
            let _ = self.run_oneshot(c, env, &[]).await;
        }
        Ok(StartOutcome {
            active: true,
            error: None,
        })
    }

    async fn start_notify(
        &self,
        cmd: &ExecCommand,
        env: &BTreeMap<String, String>,
        extra_env: &[(String, String)],
        sock: tokio::net::UnixDatagram,
    ) -> Result<StartOutcome> {
        let mut command = build_command(&self.unit.to_string(), cmd, &self.svc, env, extra_env)?;
        let mut child = command
            .spawn()
            .with_context(|| format!("spawn {}", cmd.program))?;
        setup_journal_loggers(&mut child, &self.svc, &self.unit.to_string());
        if let Some(pid) = child.id() {
            *self.main_pid.lock().await = Some(pid as i32);
        }
        *self.child.lock().await = Some(child);

        // Wait for READY=1 with TimeoutStartSec.
        let timeout = self
            .svc
            .timeout_start_sec
            .map(|d| d.as_std())
            .unwrap_or(Duration::from_secs(90));
        let deadline = Instant::now() + timeout;
        loop {
            let mut buf = [0u8; 4096];
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(StartOutcome {
                    active: false,
                    error: Some("Timed out waiting for READY=1".into()),
                });
            }
            let n = match tokio::time::timeout(remaining, sock.recv(&mut buf)).await {
                Ok(Ok(n)) => n,
                Ok(Err(e)) => {
                    return Ok(StartOutcome {
                        active: false,
                        error: Some(format!("notify recv: {e}")),
                    });
                }
                Err(_) => {
                    return Ok(StartOutcome {
                        active: false,
                        error: Some("Timed out waiting for READY=1".into()),
                    });
                }
            };
            let msg = NotifyMessage::parse(&buf[..n]);
            if let Some(pid) = msg.main_pid {
                *self.main_pid.lock().await = Some(pid);
            }
            if let Some(s) = msg.status {
                *self.status.lock().await = Some(s);
            }
            if msg.ready {
                break;
            }
        }
        *self.state.lock().await = ActiveState::Active;
        *self.sub.lock().await = "running".into();
        for c in &self.svc.exec_start_post {
            let _ = self.run_oneshot(c, env, &[]).await;
        }
        Ok(StartOutcome {
            active: true,
            error: None,
        })
    }

    /// Run an `Exec*=` synchronously and wait for completion.
    pub async fn run_oneshot(
        &self,
        cmd: &ExecCommand,
        env: &BTreeMap<String, String>,
        extra_env: &[(String, String)],
    ) -> Result<ExitStatus> {
        let mut command = build_command(&self.unit.to_string(), cmd, &self.svc, env, extra_env)?;
        let mut child = command
            .spawn()
            .with_context(|| format!("spawn {}", cmd.program))?;
        setup_journal_loggers(&mut child, &self.svc, &self.unit.to_string());
        let timeout = self
            .svc
            .timeout_start_sec
            .map(|d| d.as_std())
            .unwrap_or(Duration::from_secs(90));
        match tokio::time::timeout(timeout, child.wait()).await {
            Ok(Ok(st)) => Ok(st),
            Ok(Err(e)) => Err(anyhow!("wait: {e}")),
            Err(_) => {
                // Timeout — kill.
                let _ = child.kill().await;
                Err(anyhow!("oneshot timed out"))
            }
        }
    }

    /// Stop the unit. Runs `ExecStop`, sends `KillSignal` (default `SIGTERM`),
    /// then `SIGKILL` after `TimeoutStopSec`.
    pub async fn stop(&self) -> Result<()> {
        *self.state.lock().await = ActiveState::Deactivating;
        *self.sub.lock().await = "stop".into();
        let env = resolve_environment(&self.svc).unwrap_or_default();

        for cmd in self.svc.exec_stop.clone() {
            let _ = self.run_oneshot(&cmd, &env, &[]).await;
        }

        let pid = *self.main_pid.lock().await;
        if let Some(pid) = pid {
            let signal = parse_signal(self.svc.kill_signal.as_deref()).unwrap_or(nix::sys::signal::Signal::SIGTERM);
            let kill_target = match self.svc.kill_mode {
                KillMode::Process => Target::Pid(pid),
                KillMode::ControlGroup | KillMode::Mixed => Target::ProcessGroup(pid),
                KillMode::None => Target::None,
            };
            kill_target.send(signal);
            let timeout = self
                .svc
                .timeout_stop_sec
                .map(|d| d.as_std())
                .unwrap_or(Duration::from_secs(90));
            // Wait for the child to exit; if it has already, ok.
            let mut g = self.child.lock().await;
            let exited = if let Some(child) = g.as_mut() {
                tokio::time::timeout(timeout, child.wait()).await.is_ok()
            } else {
                // We never had a Child handle (forking) — poll via kill(0).
                wait_pid_gone(pid, timeout).await
            };
            if !exited && self.svc.send_sigkill {
                kill_target.send(nix::sys::signal::Signal::SIGKILL);
                if let Some(child) = g.as_mut() {
                    let _ = child.wait().await;
                }
            }
        }

        for cmd in self.svc.exec_stop_post.clone() {
            let _ = self.run_oneshot(&cmd, &env, &[]).await;
        }

        *self.state.lock().await = ActiveState::Inactive;
        *self.sub.lock().await = "dead".into();
        *self.main_pid.lock().await = None;
        *self.child.lock().await = None;
        Ok(())
    }

    /// `ExecReload` runner (synchronous).
    pub async fn reload(&self) -> Result<()> {
        let env = resolve_environment(&self.svc)?;
        *self.sub.lock().await = "reload".into();
        for cmd in &self.svc.exec_reload {
            let _ = self.run_oneshot(cmd, &env, &[]).await;
        }
        *self.sub.lock().await = "running".into();
        Ok(())
    }

    /// Compute the next restart delay in `RestartSec` *exponential* backoff.
    pub async fn next_restart_delay(&self) -> Duration {
        let base = match self.svc.restart_sec {
            systeml_unit::SdDuration::Finite(d) => d,
            systeml_unit::SdDuration::Infinity => Duration::from_secs(60 * 60 * 24),
        };
        let n = *self.restarts.lock().await;
        if self.svc.restart_steps == 0 || n == 0 {
            return base;
        }
        // Exponential: base * 2^(min(n, restart_steps-1)).
        let mult = 1u64
            << std::cmp::min(n as u64, self.svc.restart_steps.saturating_sub(1) as u64).min(20);
        let candidate = base.saturating_mul(mult.try_into().unwrap_or(1));
        let cap = self
            .svc
            .restart_max_delay_sec
            .map(|d| d.as_std())
            .unwrap_or(candidate);
        std::cmp::min(candidate, cap)
    }

    /// Apply `Restart=` policy after the supervised child exits.
    pub async fn handle_exit(&self, status: ExitStatus) -> Option<ProcessOutcome> {
        let outcome = classify_exit(status, &self.svc);
        *self.last_outcome.lock().await = Some(outcome);

        if !self.svc.restart.should_restart(outcome) {
            *self.state.lock().await = match outcome {
                ProcessOutcome::ExitedSuccess => ActiveState::Inactive,
                _ => ActiveState::Failed,
            };
            *self.sub.lock().await = "dead".into();
            return Some(outcome);
        }
        let delay = self.next_restart_delay().await;
        info!(unit = %self.unit, ?delay, "Restart= policy: scheduling restart");
        sleep(delay).await;
        *self.restarts.lock().await += 1;
        Some(outcome)
    }
}

/// Convert a wait-status into `ProcessOutcome` per `SuccessExitStatus=`.
pub fn classify_exit(status: ExitStatus, svc: &ServiceUnit) -> ProcessOutcome {
    use std::os::unix::process::ExitStatusExt;
    if let Some(code) = status.code() {
        let in_success = code == 0
            || svc.success_exit_status.iter().any(|s| match s {
                systeml_unit::ExitStatus::Code(c) => *c == code as u8,
                systeml_unit::ExitStatus::Signal(_) => false,
            });
        if in_success {
            ProcessOutcome::ExitedSuccess
        } else {
            ProcessOutcome::ExitedNonZero
        }
    } else if status.signal().is_some() {
        if status.core_dumped() {
            ProcessOutcome::CoreDumped
        } else {
            ProcessOutcome::Signaled
        }
    } else {
        ProcessOutcome::ExitedNonZero
    }
}

/// Where to direct a kill signal.
#[derive(Debug, Clone, Copy)]
enum Target {
    Pid(i32),
    ProcessGroup(i32),
    None,
}

impl Target {
    fn send(self, signal: nix::sys::signal::Signal) {
        match self {
            Target::None => {}
            Target::Pid(p) => {
                let _ = nix::sys::signal::kill(nix::unistd::Pid::from_raw(p), signal);
            }
            Target::ProcessGroup(p) => {
                // killpg semantics: send to whole process group.
                let _ = nix::sys::signal::killpg(nix::unistd::Pid::from_raw(p), signal);
            }
        }
    }
}

fn parse_signal(name: Option<&str>) -> Option<nix::sys::signal::Signal> {
    use nix::sys::signal::Signal::*;
    Some(match name? {
        "SIGTERM" => SIGTERM,
        "SIGKILL" => SIGKILL,
        "SIGINT" => SIGINT,
        "SIGHUP" => SIGHUP,
        "SIGUSR1" => SIGUSR1,
        "SIGUSR2" => SIGUSR2,
        "SIGQUIT" => SIGQUIT,
        "SIGABRT" => SIGABRT,
        _ => return None,
    })
}

async fn wait_pid_gone(pid: i32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        match nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None) {
            Ok(()) => {}
            Err(_) => return true,
        }
        if Instant::now() >= deadline {
            return false;
        }
        sleep(Duration::from_millis(100)).await;
    }
}

// Suppress lints from unused imports under cfg gating.
#[allow(unused_imports)]
use StandardStream as _;

#[cfg(test)]
mod tests {
    use super::*;
    use systeml_unit::exec::ExecCommand;
    use systeml_unit::name::UnitName;
    use systeml_unit::service::{ServiceType, ServiceUnit};

    fn unit_name(s: &str) -> UnitName {
        s.parse().unwrap()
    }

    /// Locate a binary on PATH. Used to skip cleanly when the build sandbox
    /// has neither `/bin/X` nor a discoverable path.
    fn find_in_path(name: &str) -> Option<String> {
        let path = std::env::var_os("PATH")?;
        for d in std::env::split_paths(&path) {
            let p = d.join(name);
            if p.is_file() {
                return Some(p.to_string_lossy().into_owned());
            }
        }
        let common = format!("/bin/{name}");
        if std::path::Path::new(&common).is_file() {
            return Some(common);
        }
        let common = format!("/usr/bin/{name}");
        if std::path::Path::new(&common).is_file() {
            return Some(common);
        }
        None
    }

    #[tokio::test]
    async fn oneshot_true() {
        let Some(path) = find_in_path("true") else {
            return;
        };
        let svc = ServiceUnit {
            service_type: ServiceType::Oneshot,
            exec_start: vec![ExecCommand::parse(&path).unwrap()],
            standard_output: StandardStream::Null,
            standard_error: StandardStream::Null,
            ..Default::default()
        };
        let r = ServiceRunner::new(unit_name("t.service"), svc);
        let out = r.start().await.unwrap();
        assert!(out.error.is_none());
        // Without remain_after_exit, ends inactive.
        assert_eq!(*r.state.lock().await, ActiveState::Inactive);
    }

    #[tokio::test]
    async fn oneshot_false_fails() {
        let Some(path) = find_in_path("false") else {
            return;
        };
        let svc = ServiceUnit {
            service_type: ServiceType::Oneshot,
            exec_start: vec![ExecCommand::parse(&path).unwrap()],
            standard_output: StandardStream::Null,
            standard_error: StandardStream::Null,
            ..Default::default()
        };
        let r = ServiceRunner::new(unit_name("t.service"), svc);
        let out = r.start().await.unwrap();
        assert!(out.error.is_some());
        assert_eq!(*r.state.lock().await, ActiveState::Failed);
    }

    #[tokio::test]
    async fn simple_starts_active() {
        let Some(path) = find_in_path("sleep") else {
            return;
        };
        let svc = ServiceUnit {
            service_type: ServiceType::Simple,
            exec_start: vec![ExecCommand::parse(&format!("{path} 5")).unwrap()],
            standard_output: StandardStream::Null,
            standard_error: StandardStream::Null,
            ..Default::default()
        };
        let r = ServiceRunner::new(unit_name("t.service"), svc);
        let out = r.start().await.unwrap();
        assert!(out.active);
        assert_eq!(*r.state.lock().await, ActiveState::Active);
        // Tear down.
        r.stop().await.unwrap();
        assert_eq!(*r.state.lock().await, ActiveState::Inactive);
    }
}
