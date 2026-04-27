//! `systeml` — the SystemL daemon.
//!
//! Lifecycle:
//! 1. Resolve `XDG_RUNTIME_DIR` (creating a per-uid fallback under
//!    `$TMPDIR/systeml-$UID` when macOS hasn't set one).
//! 2. Build a [`Manager`] and run an initial `daemon_reload` to populate
//!    units from the standard search path.
//! 3. Bind the D-Bus server at `<runtime>/systeml/private`.
//! 4. Spawn the bus server task; install signal handlers for `SIGHUP`
//!    (daemon-reload) and `SIGTERM`/`SIGINT` (graceful shutdown).
//! 5. Run forever.

#![warn(rust_2018_idioms)]

use anyhow::{Context, Result};
use clap::Parser;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;
use systeml_bus::BusServer;
use systeml_runtime::Manager;
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::RwLock;
use tracing::{error, info, warn};

#[derive(Parser, Debug)]
#[command(
    name = "systeml",
    version,
    about = "User-mode systemd-compatible service manager for macOS."
)]
struct Cli {
    /// Listen socket path. Defaults to `$XDG_RUNTIME_DIR/systeml/private`.
    #[arg(long)]
    socket: Option<PathBuf>,
    /// Don't fork; stay attached to the controlling terminal. Currently a
    /// no-op (we never daemonize) but accepted for compatibility with the
    /// LaunchAgent plist.
    #[arg(long)]
    foreground: bool,
    /// Tracing filter (e.g. `info`, `debug`, `systeml=trace`).
    #[arg(long, default_value = "info,systeml=debug")]
    log_level: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    init_tracing(&cli.log_level);
    info!(version = env!("CARGO_PKG_VERSION"), "systeml starting");

    // Ensure XDG_RUNTIME_DIR exists on macOS.
    let runtime_dir = ensure_runtime_dir()?;
    info!(runtime_dir = %runtime_dir.display(), "runtime dir");

    let manager = Arc::new(RwLock::new(Manager::new()));

    // Load every unit from the search path.
    {
        let mut m = manager.write().await;
        if let Err(e) = m.daemon_reload().await {
            warn!(error = %e, "initial daemon-reload failed");
        }
        info!(units = m.units.len(), "units loaded");
    }

    // Bind the bus.
    let socket_path = cli
        .socket
        .clone()
        .unwrap_or_else(systeml_bus::default_socket_path);
    if let Some(dir) = socket_path.parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("create {}", dir.display()))?;
        // 0700 — only the owner can talk to us.
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).ok();
    }
    let server = BusServer::bind(socket_path.clone(), manager.clone())
        .await
        .with_context(|| format!("bind bus at {}", socket_path.display()))?;
    info!(socket = %socket_path.display(), "bus listening");
    let server_task = tokio::spawn(async move {
        if let Err(e) = server.run().await {
            error!(error = %e, "bus server exited");
        }
    });

    // Signal handling.
    let mut sighup = signal(SignalKind::hangup()).context("install SIGHUP")?;
    let mut sigterm = signal(SignalKind::terminate()).context("install SIGTERM")?;
    let mut sigint = signal(SignalKind::interrupt()).context("install SIGINT")?;

    info!("systeml ready");

    loop {
        tokio::select! {
            _ = sighup.recv() => {
                info!("SIGHUP — daemon-reload");
                let mut m = manager.write().await;
                if let Err(e) = m.daemon_reload().await {
                    warn!(error = %e, "daemon-reload failed");
                }
            }
            _ = sigterm.recv() => {
                info!("SIGTERM — shutting down");
                break;
            }
            _ = sigint.recv() => {
                info!("SIGINT — shutting down");
                break;
            }
        }
    }

    server_task.abort();
    // Best-effort: drop the socket file so a fresh start can rebind.
    let _ = std::fs::remove_file(&socket_path);
    info!("systeml stopped");
    Ok(())
}

fn init_tracing(filter: &str) {
    use tracing_subscriber::EnvFilter;
    let env = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(filter));
    tracing_subscriber::fmt()
        .with_env_filter(env)
        .with_writer(std::io::stderr)
        .init();
}

/// Compute the runtime directory: respect `$XDG_RUNTIME_DIR`, otherwise
/// create a per-uid fallback under `$TMPDIR` (macOS has no XDG runtime dir
/// by convention). Sets the env var so child crates see it.
fn ensure_runtime_dir() -> Result<PathBuf> {
    if let Some(rt) = std::env::var_os("XDG_RUNTIME_DIR") {
        let rt = PathBuf::from(rt);
        std::fs::create_dir_all(&rt).with_context(|| format!("create {}", rt.display()))?;
        return Ok(rt);
    }
    let uid = nix::unistd::Uid::current().as_raw();
    let dir = std::env::temp_dir().join(format!("systeml-{uid}"));
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("create {}", dir.display()))?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
        .context("set perms on runtime dir")?;
    // Persist for the bus and runtime crates.
    std::env::set_var("XDG_RUNTIME_DIR", &dir);
    Ok(dir)
}
