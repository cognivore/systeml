//! `systemlctl` — systemctl-compatible CLI client for SystemL.
//!
//! Connects to the SystemL daemon's private D-Bus over a unix-domain socket
//! at `$XDG_RUNTIME_DIR/systeml/private` and dispatches one of the
//! `systemctl --user` subcommands. Output formatting mimics upstream's
//! tabular layout where reasonable.
//!
//! Exit codes follow `man systemctl`'s "Exit Status":
//! - 0: success.
//! - 1: failure (other).
//! - 3: `is-active` says the unit is inactive (or `is-enabled` says disabled).
//! - 4: not-found / does-not-exist.

#![warn(rust_2018_idioms)]

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::process::ExitCode;
use zbus::zvariant::{ObjectPath, OwnedObjectPath};

const MANAGER_PATH: &str = "/org/freedesktop/systemd1";
const MANAGER_IFACE: &str = "org.freedesktop.systemd1.Manager";
const UNIT_IFACE: &str = "org.freedesktop.systemd1.Unit";
const SERVICE_IFACE: &str = "org.freedesktop.systemd1.Service";

#[derive(Parser, Debug)]
#[command(
    name = "systemlctl",
    version,
    about = "Control the SystemL service manager (systemctl-compatible)."
)]
struct Cli {
    /// User-mode (the only mode SystemL has).
    #[arg(long, default_value_t = true)]
    user: bool,
    /// Don't pipe output through a pager.
    #[arg(long)]
    no_pager: bool,
    /// Suppress decorative output.
    #[arg(long)]
    quiet: bool,
    /// Do not block waiting for jobs to complete.
    #[arg(long)]
    no_block: bool,
    /// Conflict-resolution mode passed to the daemon.
    #[arg(long, default_value = "replace")]
    mode: String,
    /// Override the bus socket path (default `$XDG_RUNTIME_DIR/systeml/private`).
    #[arg(long)]
    socket: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Activate units.
    Start {
        units: Vec<String>,
    },
    /// Deactivate units.
    Stop {
        units: Vec<String>,
    },
    /// Restart units.
    Restart {
        units: Vec<String>,
    },
    /// Reload units (`ExecReload=`).
    Reload {
        units: Vec<String>,
    },
    /// Enable units (create `[Install]` symlinks).
    Enable {
        units: Vec<String>,
    },
    /// Disable units.
    Disable {
        units: Vec<String>,
    },
    /// Mask units (forbid activation).
    Mask {
        units: Vec<String>,
    },
    /// Unmask units.
    Unmask {
        units: Vec<String>,
    },
    /// One-line activity status. Exit 0 if active, 3 otherwise.
    IsActive {
        units: Vec<String>,
    },
    /// One-line enabled status. Exit 0 if enabled, 1 otherwise.
    IsEnabled {
        units: Vec<String>,
    },
    /// Show full status.
    Status {
        units: Vec<String>,
    },
    /// Print fragment + drop-ins.
    Cat {
        units: Vec<String>,
    },
    /// Show unit properties (D-Bus property dump).
    Show {
        units: Vec<String>,
    },
    /// List loaded units, systemctl-style.
    ListUnits,
    /// List on-disk unit files with state.
    ListUnitFiles,
    /// Reload manager configuration.
    DaemonReload,
    /// Print system running state. Exit 0 if up.
    IsSystemRunning,
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    let cli = Cli::parse();
    match run(cli).await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("systemlctl: {e:#}");
            ExitCode::from(1)
        }
    }
}

async fn run(cli: Cli) -> Result<ExitCode> {
    let socket = cli.socket.clone().unwrap_or_else(default_socket_path);
    let conn = connect(&socket).await.with_context(|| {
        format!(
            "could not connect to SystemL bus at {}. Is the systeml daemon running?",
            socket.display()
        )
    })?;
    let proxy = ManagerProxy { conn: &conn };

    let mode = cli.mode.as_str();
    match cli.cmd {
        Cmd::Start { units } => {
            for u in units {
                let _ = proxy.call_unit_action("StartUnit", &u, mode).await?;
                if !cli.quiet {
                    println!("Started {u}");
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Stop { units } => {
            for u in units {
                let _ = proxy.call_unit_action("StopUnit", &u, mode).await?;
                if !cli.quiet {
                    println!("Stopped {u}");
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Restart { units } => {
            for u in units {
                let _ = proxy.call_unit_action("RestartUnit", &u, mode).await?;
                if !cli.quiet {
                    println!("Restarted {u}");
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Reload { units } => {
            for u in units {
                let _ = proxy.call_unit_action("ReloadUnit", &u, mode).await?;
                if !cli.quiet {
                    println!("Reloaded {u}");
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Enable { units } => {
            if std::env::var("SYSTEML_ALLOW_IMPERATIVE_ENABLE").as_deref() == Ok("1") {
                let (carries, changes) = proxy.enable_unit_files(&units).await?;
                print_changes(&changes);
                if !carries && !cli.quiet {
                    eprintln!("warning: none of the listed units carry [Install] info");
                }
                Ok(ExitCode::SUCCESS)
            } else {
                refuse_imperative("enable", &units);
                Ok(ExitCode::from(2))
            }
        }
        Cmd::Disable { units } => {
            if std::env::var("SYSTEML_ALLOW_IMPERATIVE_ENABLE").as_deref() == Ok("1") {
                let changes = proxy.disable_unit_files(&units).await?;
                print_changes(&changes);
                Ok(ExitCode::SUCCESS)
            } else {
                refuse_imperative("disable", &units);
                Ok(ExitCode::from(2))
            }
        }
        Cmd::Mask { units } => {
            let changes = proxy.mask_unit_files(&units).await?;
            print_changes(&changes);
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Unmask { units } => {
            let changes = proxy.unmask_unit_files(&units).await?;
            print_changes(&changes);
            Ok(ExitCode::SUCCESS)
        }
        Cmd::IsActive { units } => {
            let mut all_active = true;
            for u in &units {
                let state = proxy.get_unit_property(u, UNIT_IFACE, "ActiveState").await?;
                println!("{state}");
                if state != "active" && state != "reloading" {
                    all_active = false;
                }
            }
            Ok(if all_active {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(3)
            })
        }
        Cmd::IsEnabled { units } => {
            let mut all_enabled = true;
            for u in &units {
                let state = proxy.get_unit_file_state(u).await.unwrap_or_else(|_| "not-found".into());
                println!("{state}");
                if state != "enabled" && state != "static" && state != "alias" {
                    all_enabled = false;
                }
            }
            Ok(if all_enabled {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            })
        }
        Cmd::Status { units } => {
            for u in &units {
                if let Err(e) = print_status(&proxy, u).await {
                    eprintln!("systemlctl: {u}: {e:#}");
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Cat { units } => {
            for u in &units {
                let path = proxy.get_unit_property(u, UNIT_IFACE, "FragmentPath").await?;
                if path.is_empty() {
                    eprintln!("systemlctl: {u}: no fragment path");
                    continue;
                }
                println!("# {path}");
                match std::fs::read_to_string(&path) {
                    Ok(s) => print!("{s}"),
                    Err(e) => eprintln!("systemlctl: {path}: {e}"),
                }
                println!();
            }
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Show { units } => {
            for u in &units {
                println!("# {u}");
                for prop in [
                    "Id",
                    "Description",
                    "LoadState",
                    "ActiveState",
                    "SubState",
                    "FragmentPath",
                    "UnitFileState",
                ] {
                    let v = proxy
                        .get_unit_property(u, UNIT_IFACE, prop)
                        .await
                        .unwrap_or_default();
                    println!("{prop}={v}");
                }
                if u.ends_with(".service") {
                    for prop in ["Type", "Restart"] {
                        let v = proxy
                            .get_unit_property(u, SERVICE_IFACE, prop)
                            .await
                            .unwrap_or_default();
                        println!("{prop}={v}");
                    }
                }
                println!();
            }
            Ok(ExitCode::SUCCESS)
        }
        Cmd::ListUnits => {
            let units = proxy.list_units().await?;
            println!(
                "{:<35} {:<8} {:<10} {:<10} DESCRIPTION",
                "UNIT", "LOAD", "ACTIVE", "SUB"
            );
            for entry in units {
                println!(
                    "{:<35} {:<8} {:<10} {:<10} {}",
                    truncate(&entry.0, 35),
                    entry.2,
                    entry.3,
                    entry.4,
                    entry.1
                );
            }
            Ok(ExitCode::SUCCESS)
        }
        Cmd::ListUnitFiles => {
            let files = proxy.list_unit_files().await?;
            println!("{:<45} STATE", "UNIT FILE");
            for (path, state) in files {
                let basename = std::path::Path::new(&path)
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or(path);
                println!("{:<45} {}", truncate(&basename, 45), state);
            }
            Ok(ExitCode::SUCCESS)
        }
        Cmd::DaemonReload => {
            proxy.daemon_reload().await?;
            Ok(ExitCode::SUCCESS)
        }
        Cmd::IsSystemRunning => {
            // We're connected, so the daemon is up. systemd's full state set
            // (`initializing` / `starting` / `running` / `degraded` / etc.)
            // doesn't really apply at the user-mode scope; we always return
            // "running" and rely on the connect attempt above to fail if the
            // daemon isn't around.
            println!("running");
            Ok(ExitCode::SUCCESS)
        }
    }
}

fn truncate(s: &str, n: usize) -> &str {
    if s.len() <= n {
        s
    } else {
        &s[..n]
    }
}

fn print_changes(changes: &[(String, String, String)]) {
    for (kind, target, source) in changes {
        match kind.as_str() {
            "symlink" => println!("Created symlink {target} -> {source}"),
            "unlink" => println!("Removed {target}"),
            other => println!("{other} {target} {source}"),
        }
    }
}

/// Refuse `enable` / `disable` when home-manager is the source of truth.
///
/// We expect units to be installed declaratively via
/// `systemd.user.services.<name>` etc. in a home-manager config.
/// Imperative `systemlctl enable` would create symlinks on disk that
/// the next `home-manager switch` would either tear down or overwrite,
/// causing confusing drift. Refuse loudly with a clear redirect; offer
/// an env-var escape hatch for emergencies (debugging, recovery).
fn refuse_imperative(action: &str, units: &[String]) {
    eprintln!(
        "systemlctl: refusing to {action} unit(s): {}",
        units.join(", ")
    );
    eprintln!();
    eprintln!(
        "  home-manager owns enable/disable state for SystemL. To {action} a unit:"
    );
    eprintln!();
    eprintln!(
        "    1. Edit your home-manager config so the unit's [Install] section"
    );
    eprintln!(
        "       is {}.",
        if action == "enable" {
            "reachable from a target your machine wants (e.g. WantedBy=timers.target)"
        } else {
            "removed (or remove the unit declaration entirely)"
        }
    );
    eprintln!("    2. Run `home-manager switch`.");
    eprintln!();
    eprintln!(
        "  Imperative {action} would drift from the declarative state and be"
    );
    eprintln!(
        "  overwritten by the next switch. If you really need to bypass this"
    );
    eprintln!(
        "  guard (e.g. for debugging), set SYSTEML_ALLOW_IMPERATIVE_ENABLE=1."
    );
}

async fn print_status(proxy: &ManagerProxy<'_>, unit: &str) -> Result<()> {
    let id = proxy
        .get_unit_property(unit, UNIT_IFACE, "Id")
        .await
        .unwrap_or_else(|_| unit.to_owned());
    let desc = proxy
        .get_unit_property(unit, UNIT_IFACE, "Description")
        .await
        .unwrap_or_default();
    let load = proxy
        .get_unit_property(unit, UNIT_IFACE, "LoadState")
        .await
        .unwrap_or_default();
    let active = proxy
        .get_unit_property(unit, UNIT_IFACE, "ActiveState")
        .await
        .unwrap_or_default();
    let sub = proxy
        .get_unit_property(unit, UNIT_IFACE, "SubState")
        .await
        .unwrap_or_default();
    let frag = proxy
        .get_unit_property(unit, UNIT_IFACE, "FragmentPath")
        .await
        .unwrap_or_default();
    let unit_file = proxy
        .get_unit_property(unit, UNIT_IFACE, "UnitFileState")
        .await
        .unwrap_or_default();

    println!("● {id} - {desc}");
    println!("     Loaded: {load}{}", if frag.is_empty() { String::new() } else { format!(" ({frag}; {unit_file})") });
    println!("     Active: {active} ({sub})");
    println!();
    Ok(())
}

fn default_socket_path() -> PathBuf {
    if let Some(rt) = std::env::var_os("XDG_RUNTIME_DIR") {
        return PathBuf::from(rt).join("systeml/private");
    }
    let uid = nix::unistd::Uid::current().as_raw();
    std::env::temp_dir()
        .join(format!("systeml-{uid}"))
        .join("systeml/private")
}

async fn connect(socket: &std::path::Path) -> Result<zbus::Connection> {
    let stream = tokio::net::UnixStream::connect(socket).await?;
    let conn = zbus::connection::Builder::unix_stream(stream)
        .p2p()
        .auth_mechanism(zbus::AuthMechanism::External)
        .build()
        .await?;
    Ok(conn)
}

/// Thin wrapper around the bus connection. Each method dispatches one D-Bus
/// call against the well-known Manager path and decodes the reply.
struct ManagerProxy<'c> {
    conn: &'c zbus::Connection,
}

impl ManagerProxy<'_> {
    async fn call_unit_action(&self, method: &str, name: &str, mode: &str) -> Result<OwnedObjectPath> {
        let reply = self
            .conn
            .call_method(
                None::<&str>,
                MANAGER_PATH,
                Some(MANAGER_IFACE),
                method,
                &(name, mode),
            )
            .await?;
        let path: OwnedObjectPath = reply.body().deserialize()?;
        Ok(path)
    }

    async fn enable_unit_files(&self, names: &[String]) -> Result<(bool, Vec<(String, String, String)>)> {
        let names_ref: Vec<&str> = names.iter().map(String::as_str).collect();
        let reply = self
            .conn
            .call_method(
                None::<&str>,
                MANAGER_PATH,
                Some(MANAGER_IFACE),
                "EnableUnitFiles",
                &(names_ref, false, false),
            )
            .await?;
        let body = reply.body();
        let v: (bool, Vec<(String, String, String)>) = body.deserialize()?;
        Ok(v)
    }

    async fn disable_unit_files(&self, names: &[String]) -> Result<Vec<(String, String, String)>> {
        let names_ref: Vec<&str> = names.iter().map(String::as_str).collect();
        let reply = self
            .conn
            .call_method(
                None::<&str>,
                MANAGER_PATH,
                Some(MANAGER_IFACE),
                "DisableUnitFiles",
                &(names_ref, false),
            )
            .await?;
        let v: Vec<(String, String, String)> = reply.body().deserialize()?;
        Ok(v)
    }

    async fn mask_unit_files(&self, names: &[String]) -> Result<Vec<(String, String, String)>> {
        let names_ref: Vec<&str> = names.iter().map(String::as_str).collect();
        let reply = self
            .conn
            .call_method(
                None::<&str>,
                MANAGER_PATH,
                Some(MANAGER_IFACE),
                "MaskUnitFiles",
                &(names_ref, false, false),
            )
            .await?;
        let v: Vec<(String, String, String)> = reply.body().deserialize()?;
        Ok(v)
    }

    async fn unmask_unit_files(&self, names: &[String]) -> Result<Vec<(String, String, String)>> {
        let names_ref: Vec<&str> = names.iter().map(String::as_str).collect();
        let reply = self
            .conn
            .call_method(
                None::<&str>,
                MANAGER_PATH,
                Some(MANAGER_IFACE),
                "UnmaskUnitFiles",
                &(names_ref, false),
            )
            .await?;
        let v: Vec<(String, String, String)> = reply.body().deserialize()?;
        Ok(v)
    }

    async fn get_unit_file_state(&self, name: &str) -> Result<String> {
        let reply = self
            .conn
            .call_method(
                None::<&str>,
                MANAGER_PATH,
                Some(MANAGER_IFACE),
                "GetUnitFileState",
                &(name,),
            )
            .await?;
        let s: String = reply.body().deserialize()?;
        Ok(s)
    }

    async fn list_units(&self) -> Result<Vec<UnitListEntry>> {
        let reply = self
            .conn
            .call_method(
                None::<&str>,
                MANAGER_PATH,
                Some(MANAGER_IFACE),
                "ListUnits",
                &(),
            )
            .await?;
        let v: Vec<UnitListEntry> = reply.body().deserialize()?;
        Ok(v)
    }

    async fn list_unit_files(&self) -> Result<Vec<(String, String)>> {
        let reply = self
            .conn
            .call_method(
                None::<&str>,
                MANAGER_PATH,
                Some(MANAGER_IFACE),
                "ListUnitFiles",
                &(),
            )
            .await?;
        let v: Vec<(String, String)> = reply.body().deserialize()?;
        Ok(v)
    }

    async fn daemon_reload(&self) -> Result<()> {
        self.conn
            .call_method(
                None::<&str>,
                MANAGER_PATH,
                Some(MANAGER_IFACE),
                "Reload",
                &(),
            )
            .await?;
        Ok(())
    }

    /// Get a property by going through `org.freedesktop.DBus.Properties.Get`
    /// on the unit's object path.
    async fn get_unit_property(
        &self,
        unit: &str,
        iface: &str,
        prop: &str,
    ) -> Result<String> {
        // Get the unit object path via Manager.LoadUnit (or GetUnit if loaded).
        let unit_path = self.get_or_load_unit(unit).await?;
        let reply = self
            .conn
            .call_method(
                None::<&str>,
                ObjectPath::try_from(unit_path.as_str())?,
                Some("org.freedesktop.DBus.Properties"),
                "Get",
                &(iface, prop),
            )
            .await?;
        let body = reply.body();
        let value: zbus::zvariant::Value<'_> = body.deserialize()?;
        Ok(stringify_value(&value))
    }

    async fn get_or_load_unit(&self, name: &str) -> Result<OwnedObjectPath> {
        let reply = self
            .conn
            .call_method(
                None::<&str>,
                MANAGER_PATH,
                Some(MANAGER_IFACE),
                "GetUnit",
                &(name,),
            )
            .await;
        if let Ok(r) = reply {
            return Ok(r.body().deserialize()?);
        }
        let reply = self
            .conn
            .call_method(
                None::<&str>,
                MANAGER_PATH,
                Some(MANAGER_IFACE),
                "LoadUnit",
                &(name,),
            )
            .await?;
        Ok(reply.body().deserialize()?)
    }
}

type UnitListEntry = (
    String,                  // name
    String,                  // description
    String,                  // load state
    String,                  // active state
    String,                  // sub state
    String,                  // follower
    OwnedObjectPath,         // unit path
    u32,                     // job id
    String,                  // job type
    OwnedObjectPath,         // job path
);

fn stringify_value(v: &zbus::zvariant::Value<'_>) -> String {
    use zbus::zvariant::Value;
    match v {
        Value::Str(s) => s.to_string(),
        Value::ObjectPath(p) => p.to_string(),
        Value::Signature(s) => s.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::U8(n) => n.to_string(),
        Value::I16(n) => n.to_string(),
        Value::U16(n) => n.to_string(),
        Value::I32(n) => n.to_string(),
        Value::U32(n) => n.to_string(),
        Value::I64(n) => n.to_string(),
        Value::U64(n) => n.to_string(),
        Value::F64(n) => n.to_string(),
        Value::Array(a) => format!("{a:?}"),
        Value::Dict(d) => format!("{d:?}"),
        Value::Structure(s) => format!("{s:?}"),
        Value::Fd(f) => format!("fd:{f:?}"),
        _ => format!("{v:?}"),
    }
}
