//! `systeml-bus` — `org.freedesktop.systemd1` D-Bus surface.
//!
//! This crate hosts the D-Bus interfaces upstream `systemctl --user` and
//! other systemd-aware tools speak. We listen on a private bus at
//! `$XDG_RUNTIME_DIR/systeml/private` (a unix-domain socket) and serve
//! peer-to-peer connections — one zbus `Connection` per accepted client.
//!
//! ## Layout
//! - [`BusServer`] — the top-level server. Holds an
//!   `Arc<RwLock<systeml_runtime::Manager>>` and the bound `UnixListener`.
//! - [`manager_iface::ManagerIface`] — `org.freedesktop.systemd1.Manager` at
//!   [`MANAGER_PATH`].
//! - [`unit_iface::UnitIface`] — per-unit `org.freedesktop.systemd1.Unit`,
//!   one path per loaded unit.
//! - [`service_iface::ServiceIface`] — `org.freedesktop.systemd1.Service`
//!   for `.service` units.
//! - [`stub_ifaces`] — empty `Socket`/`Timer`/`Path`/`Target`/`Scope`
//!   placeholders so `Properties.GetAll` doesn't error on systemctl.
//! - [`events`] — translates `systeml_runtime::manager::UnitEvent`s into
//!   bus signals on the manager and per-unit interfaces.
//!
//! ## Example
//! ```no_run
//! use std::sync::Arc;
//! use tokio::sync::RwLock;
//! use systeml_runtime::Manager;
//! use systeml_bus::{default_socket_path, BusServer};
//!
//! # async fn run() -> anyhow::Result<()> {
//! let mgr = Arc::new(RwLock::new(Manager::new()));
//! let server = BusServer::bind(default_socket_path(), mgr).await?;
//! server.run().await?;
//! # Ok(())
//! # }
//! ```

#![warn(rust_2018_idioms)]

use std::path::PathBuf;
use std::sync::Arc;
use systeml_runtime::Manager;
use systeml_unit::UnitName;
use tokio::net::UnixListener;
use tokio::sync::RwLock;
use zbus::zvariant::OwnedObjectPath;

pub mod events;
pub mod manager_iface;
pub mod mock;
pub mod service_iface;
pub mod stub_ifaces;
pub mod unit_iface;

/// D-Bus service name we publish under.
pub const SERVICE_NAME: &str = "org.freedesktop.systemd1";
/// Object path of the manager.
pub const MANAGER_PATH: &str = "/org/freedesktop/systemd1";
/// Manager interface name.
pub const MANAGER_INTERFACE: &str = "org.freedesktop.systemd1.Manager";
/// Unit interface name.
pub const UNIT_INTERFACE: &str = "org.freedesktop.systemd1.Unit";
/// Service interface name.
pub const SERVICE_INTERFACE: &str = "org.freedesktop.systemd1.Service";
/// Object-path prefix for per-unit objects.
pub const UNIT_PATH_PREFIX: &str = "/org/freedesktop/systemd1/unit/";

/// Default socket path on macOS where SystemL listens for D-Bus connections.
#[must_use]
pub fn default_socket_path() -> PathBuf {
    systeml_unit::search::runtime_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("systeml/private")
}

/// Encode a unit name into the systemd-canonical D-Bus object path.
///
/// systemd's `dbus_path_encode_unit_name` replaces every byte that is not in
/// `[A-Za-z0-9_]` with `_<two-hex>`. So `foo.service` becomes
/// `foo_2eservice` and `getty@tty1.service` becomes
/// `getty_40tty1_2eservice`. The path returned has the
/// [`UNIT_PATH_PREFIX`] prepended.
#[must_use]
pub fn unit_object_path(name: &UnitName) -> OwnedObjectPath {
    let escaped = encode_path_component(&name.filename());
    let path = format!("{UNIT_PATH_PREFIX}{escaped}");
    // Path is built from validated escape; this can only fail under bug.
    OwnedObjectPath::try_from(path).expect("escaped unit path must be a valid object path")
}

/// systemd's `bus_path_escape` algorithm. Public so tests and downstream
/// crates can verify round-trip behaviour.
#[must_use]
pub fn encode_path_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b == b'_' {
            out.push(b as char);
        } else {
            out.push('_');
            out.push_str(&format!("{:02x}", b));
        }
    }
    out
}

/// A bound D-Bus server. Drop the value to release the listening socket.
pub struct BusServer {
    listener: UnixListener,
    manager: Arc<RwLock<Manager>>,
    socket_path: PathBuf,
    guid: zbus::Guid<'static>,
}

impl BusServer {
    /// Bind the listening socket. The parent directory is created if missing
    /// and any stale socket file at `socket_path` is unlinked first.
    pub async fn bind(
        socket_path: PathBuf,
        manager: Arc<RwLock<Manager>>,
    ) -> anyhow::Result<Self> {
        if let Some(dir) = socket_path.parent() {
            tokio::fs::create_dir_all(dir).await.ok();
        }
        // Best-effort cleanup of a previous run's socket.
        let _ = tokio::fs::remove_file(&socket_path).await;
        let listener = UnixListener::bind(&socket_path)?;
        let guid = zbus::Guid::generate().to_owned();
        tracing::info!(
            socket = %socket_path.display(),
            "systeml bus listening"
        );
        Ok(Self {
            listener,
            manager,
            socket_path,
            guid,
        })
    }

    /// Path of the bound socket.
    #[must_use]
    pub fn socket_path(&self) -> &std::path::Path {
        &self.socket_path
    }

    /// Accept connections forever, serving each in its own task. Returns only
    /// if accept errors out catastrophically.
    pub async fn run(self) -> anyhow::Result<()> {
        loop {
            let (stream, _peer) = match self.listener.accept().await {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(error = %e, "bus accept failed");
                    return Err(e.into());
                }
            };
            let mgr = self.manager.clone();
            let guid = self.guid.clone();
            tokio::spawn(async move {
                if let Err(e) = serve_connection(stream, guid, mgr).await {
                    tracing::warn!(error = %e, "bus connection ended with error");
                }
            });
        }
    }
}

/// Build a peer-to-peer zbus [`Connection`](zbus::Connection) for one client
/// stream and serve until the client disconnects.
async fn serve_connection(
    stream: tokio::net::UnixStream,
    guid: zbus::Guid<'static>,
    manager: Arc<RwLock<Manager>>,
) -> anyhow::Result<()> {
    let mut builder = zbus::connection::Builder::unix_stream(stream)
        .server(guid)?
        .p2p()
        .auth_mechanism(zbus::AuthMechanism::External);

    // Manager interface lives at the well-known path.
    builder = builder.serve_at(
        MANAGER_PATH,
        manager_iface::ManagerIface::new(manager.clone()),
    )?;

    // Pre-register every currently-loaded unit. New units are added via the
    // event bridge after the connection is up.
    let initial_units: Vec<UnitName> = {
        let m = manager.read().await;
        m.units.keys().cloned().collect()
    };
    for name in &initial_units {
        let path = unit_object_path(name);
        builder = builder.serve_at(
            path.clone(),
            unit_iface::UnitIface::new(name.clone(), manager.clone()),
        )?;
        match name.kind {
            systeml_unit::UnitKind::Service => {
                builder = builder.serve_at(
                    path.clone(),
                    service_iface::ServiceIface::new(name.clone(), manager.clone()),
                )?;
            }
            systeml_unit::UnitKind::Socket => {
                builder = builder.serve_at(path.clone(), stub_ifaces::SocketStub)?;
            }
            systeml_unit::UnitKind::Timer => {
                builder = builder.serve_at(path.clone(), stub_ifaces::TimerStub)?;
            }
            systeml_unit::UnitKind::Path => {
                builder = builder.serve_at(path.clone(), stub_ifaces::PathStub)?;
            }
            systeml_unit::UnitKind::Target => {
                builder = builder.serve_at(path.clone(), stub_ifaces::TargetStub)?;
            }
            systeml_unit::UnitKind::Scope => {
                builder = builder.serve_at(path.clone(), stub_ifaces::ScopeStub)?;
            }
            _ => {}
        }
    }

    let connection = builder.build().await?;
    tracing::debug!("bus client connection up");

    // Bridge runtime events to bus signals on this connection.
    let bridge = events::EventBridge::spawn(connection.clone(), manager.clone());

    // Drain the message stream until the peer disconnects. The
    // `ObjectServer` automatically handles all method calls; we just have to
    // keep the stream polled. When the underlying socket closes, the stream
    // ends and we drop the connection (cleaning up the bridge with it).
    use futures::StreamExt;
    let mut stream = zbus::MessageStream::from(connection.clone());
    while stream.next().await.is_some() {}
    drop(bridge);
    tracing::debug!("bus client connection closed");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use systeml_unit::UnitKind;

    #[test]
    fn unit_object_path_basic() {
        let n = UnitName::plain("foo", UnitKind::Service);
        let p = unit_object_path(&n);
        assert_eq!(p.as_str(), "/org/freedesktop/systemd1/unit/foo_2eservice");
    }

    #[test]
    fn unit_object_path_instance() {
        let n = UnitName::instance("getty", "tty1", UnitKind::Service);
        let p = unit_object_path(&n);
        assert_eq!(
            p.as_str(),
            "/org/freedesktop/systemd1/unit/getty_40tty1_2eservice"
        );
    }

    #[test]
    fn unit_object_path_dashes() {
        // dash -> _2d in the systemd algorithm.
        let n: UnitName = "dev-disk-by.device".parse().unwrap();
        let p = unit_object_path(&n);
        assert_eq!(
            p.as_str(),
            "/org/freedesktop/systemd1/unit/dev_2ddisk_2dby_2edevice"
        );
    }

    #[test]
    fn unit_object_path_underscore_passthrough() {
        // `_` is in the systemd "safe" set and is kept literal.
        let n: UnitName = "foo_bar.service".parse().unwrap();
        let p = unit_object_path(&n);
        assert_eq!(
            p.as_str(),
            "/org/freedesktop/systemd1/unit/foo_bar_2eservice"
        );
    }
}
