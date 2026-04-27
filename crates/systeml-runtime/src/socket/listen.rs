//! Socket bind + accept helpers.
//!
//! Each `Listen*=` directive becomes one or more `Listener`s. For
//! `Accept=no`, we hold the listening fd and pass it to the launched service
//! via `LISTEN_FDS=`. For `Accept=yes`, we accept connections in SystemL and
//! spawn per-instance services.

use anyhow::{anyhow, Context, Result};
use nix::fcntl::{fcntl, FcntlArg, FdFlag};
use nix::sys::socket::{
    bind, listen, setsockopt, socket, sockopt, AddressFamily, Backlog, SockFlag, SockType,
    SockaddrIn, SockaddrIn6, UnixAddr,
};
use std::net::{SocketAddr, SocketAddrV4, SocketAddrV6};
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use systeml_unit::socket::{ListenSpec, SocketAddrSpec, SocketUnit};
use tracing::warn;

/// Set FD_CLOEXEC on a freshly-created socket. macOS lacks `SOCK_CLOEXEC`,
/// so we emulate it post-creation. Errors propagate (cloexec is a correctness
/// concern, not optional).
fn set_cloexec(fd: &OwnedFd) -> Result<()> {
    let cur = fcntl(fd.as_raw_fd(), FcntlArg::F_GETFD).context("F_GETFD")?;
    let mut flags = FdFlag::from_bits_truncate(cur);
    flags.insert(FdFlag::FD_CLOEXEC);
    fcntl(fd.as_raw_fd(), FcntlArg::F_SETFD(flags)).context("F_SETFD")?;
    Ok(())
}

fn make_backlog(b: usize) -> Backlog {
    Backlog::new(b as i32).unwrap_or(Backlog::MAXCONN)
}

/// One realised listening socket.
pub struct Listener {
    /// The listening fd (kept open for fd passing).
    pub fd: OwnedFd,
    /// Filesystem cleanup target (for Unix sockets / FIFOs).
    pub remove_on_stop: Option<PathBuf>,
    /// Friendly name for `LISTEN_FDNAMES=`.
    pub name: String,
}

impl Listener {
    /// Numeric fd (for fd-passing).
    pub fn raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}

/// Bind every `Listen*=` in `unit`. Returns the open listeners in declared
/// order. The caller must keep these alive until the unit is stopped.
pub fn bind_all(unit: &SocketUnit) -> Result<Vec<Listener>> {
    let mut out = Vec::new();
    for (i, spec) in unit.listen.iter().enumerate() {
        match bind_one(spec, unit) {
            Ok(l) => {
                let mut l = l;
                if let Some(name) = &unit.file_descriptor_name {
                    l.name.clone_from(name);
                } else if l.name.is_empty() {
                    l.name = format!("fd{i}");
                }
                out.push(l);
            }
            Err(e) => return Err(e),
        }
    }
    Ok(out)
}

fn bind_one(spec: &ListenSpec, unit: &SocketUnit) -> Result<Listener> {
    let backlog = if unit.backlog == 0 { 128 } else { unit.backlog as usize };
    match spec {
        ListenSpec::Stream(addr) => bind_stream(addr, backlog, unit),
        ListenSpec::Datagram(addr) => bind_datagram(addr, unit),
        ListenSpec::SequentialPacket(addr) => bind_seqpacket(addr, backlog, unit),
        ListenSpec::Fifo(p) => bind_fifo(p, unit),
        ListenSpec::Special(p) => bind_special(p),
        ListenSpec::Netlink(_)
        | ListenSpec::MessageQueue(_)
        | ListenSpec::UsbFunction(_) => Err(anyhow!("Linux-only Listen* directive")),
    }
}

fn bind_stream(spec: &SocketAddrSpec, backlog: usize, unit: &SocketUnit) -> Result<Listener> {
    match spec {
        SocketAddrSpec::Inet(SocketAddr::V4(v4)) => bind_inet_v4(*v4, SockType::Stream, backlog, unit),
        SocketAddrSpec::Inet(SocketAddr::V6(v6)) => bind_inet_v6(*v6, SockType::Stream, backlog, unit),
        SocketAddrSpec::Port(p) => bind_inet_v4(
            SocketAddrV4::new(std::net::Ipv4Addr::UNSPECIFIED, *p),
            SockType::Stream,
            backlog,
            unit,
        ),
        SocketAddrSpec::UnixPath(p) => bind_unix(p, SockType::Stream, Some(backlog), unit),
        SocketAddrSpec::UnixAbstract(name) => {
            warn!(
                name = %name,
                "Linux abstract namespace not supported on macOS; falling back to /tmp/{name}"
            );
            let fallback = PathBuf::from(format!("/tmp/{name}"));
            bind_unix(&fallback, SockType::Stream, Some(backlog), unit)
        }
        SocketAddrSpec::Vsock(_) | SocketAddrSpec::Netlink(_) => {
            Err(anyhow!("address family not supported on macOS"))
        }
    }
}

fn bind_datagram(spec: &SocketAddrSpec, unit: &SocketUnit) -> Result<Listener> {
    match spec {
        SocketAddrSpec::Inet(SocketAddr::V4(v4)) => bind_inet_v4(*v4, SockType::Datagram, 0, unit),
        SocketAddrSpec::Inet(SocketAddr::V6(v6)) => bind_inet_v6(*v6, SockType::Datagram, 0, unit),
        SocketAddrSpec::Port(p) => bind_inet_v4(
            SocketAddrV4::new(std::net::Ipv4Addr::UNSPECIFIED, *p),
            SockType::Datagram,
            0,
            unit,
        ),
        SocketAddrSpec::UnixPath(p) => bind_unix(p, SockType::Datagram, None, unit),
        SocketAddrSpec::UnixAbstract(_) => Err(anyhow!("abstract namespace unsupported")),
        SocketAddrSpec::Vsock(_) | SocketAddrSpec::Netlink(_) => {
            Err(anyhow!("address family not supported on macOS"))
        }
    }
}

fn bind_seqpacket(spec: &SocketAddrSpec, backlog: usize, unit: &SocketUnit) -> Result<Listener> {
    match spec {
        SocketAddrSpec::UnixPath(p) => bind_unix(p, SockType::SeqPacket, Some(backlog), unit),
        _ => Err(anyhow!("SOCK_SEQPACKET only supported on unix paths")),
    }
}

fn bind_inet_v4(
    addr: SocketAddrV4,
    sty: SockType,
    backlog: usize,
    unit: &SocketUnit,
) -> Result<Listener> {
    let fd = socket(
        AddressFamily::Inet,
        sty,
        SockFlag::empty(),
        None,
    )
    .context("socket(AF_INET)")?;
    if unit.reuse_port {
        setsockopt(&fd, sockopt::ReusePort, &true).ok();
    }
    setsockopt(&fd, sockopt::ReuseAddr, &true).ok();
    let sa: SockaddrIn = SockaddrIn::from(addr);
    bind(fd.as_raw_fd(), &sa).context("bind(AF_INET)")?;
    if matches!(sty, SockType::Stream | SockType::SeqPacket) {
        listen(&fd, make_backlog(backlog)).context("listen(AF_INET)")?;
    }
    apply_tcp_opts(&fd, unit);
    set_cloexec(&fd)?;
    Ok(Listener {
        fd,
        remove_on_stop: None,
        name: format!("{}:{}", addr.ip(), addr.port()),
    })
}

fn bind_inet_v6(
    addr: SocketAddrV6,
    sty: SockType,
    backlog: usize,
    unit: &SocketUnit,
) -> Result<Listener> {
    let fd = socket(
        AddressFamily::Inet6,
        sty,
        SockFlag::empty(),
        None,
    )
    .context("socket(AF_INET6)")?;
    if unit.reuse_port {
        setsockopt(&fd, sockopt::ReusePort, &true).ok();
    }
    setsockopt(&fd, sockopt::ReuseAddr, &true).ok();
    let sa: SockaddrIn6 = SockaddrIn6::from(addr);
    bind(fd.as_raw_fd(), &sa).context("bind(AF_INET6)")?;
    if matches!(sty, SockType::Stream | SockType::SeqPacket) {
        listen(&fd, make_backlog(backlog)).context("listen(AF_INET6)")?;
    }
    apply_tcp_opts(&fd, unit);
    set_cloexec(&fd)?;
    Ok(Listener {
        fd,
        remove_on_stop: None,
        name: format!("[{}]:{}", addr.ip(), addr.port()),
    })
}

fn bind_unix(
    path: &Path,
    sty: SockType,
    backlog: Option<usize>,
    unit: &SocketUnit,
) -> Result<Listener> {
    if path.exists() {
        let _ = std::fs::remove_file(path);
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let fd = socket(
        AddressFamily::Unix,
        sty,
        SockFlag::empty(),
        None,
    )
    .context("socket(AF_UNIX)")?;
    let addr = UnixAddr::new(path).context("UnixAddr")?;
    bind(fd.as_raw_fd(), &addr).context("bind(AF_UNIX)")?;
    if let Some(b) = backlog {
        listen(&fd, make_backlog(b)).context("listen(AF_UNIX)")?;
    }
    if let Some(mode) = unit.socket_mode {
        #[allow(unused_imports)]
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(mode);
        let _ = std::fs::set_permissions(path, perms);
    }
    let remove = if unit.remove_on_stop {
        Some(path.to_owned())
    } else {
        None
    };
    set_cloexec(&fd)?;
    Ok(Listener {
        fd,
        remove_on_stop: remove,
        name: path.display().to_string(),
    })
}

fn bind_fifo(path: &Path, unit: &SocketUnit) -> Result<Listener> {
    use nix::sys::stat::Mode;
    let mode_bits = unit.socket_mode.unwrap_or(0o660);
    if !path.exists() {
        nix::unistd::mkfifo(path, Mode::from_bits_truncate(mode_bits as libc::mode_t))
            .context("mkfifo")?;
    }
    let f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .context("open fifo")?;
    Ok(Listener {
        fd: OwnedFd::from(f),
        remove_on_stop: if unit.remove_on_stop { Some(path.to_owned()) } else { None },
        name: path.display().to_string(),
    })
}

fn bind_special(path: &Path) -> Result<Listener> {
    let f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .with_context(|| format!("open special {path:?}"))?;
    Ok(Listener {
        fd: OwnedFd::from(f),
        remove_on_stop: None,
        name: path.display().to_string(),
    })
}

fn apply_tcp_opts(fd: &OwnedFd, unit: &SocketUnit) {
    if unit.no_delay {
        let _ = setsockopt(fd, sockopt::TcpNoDelay, &true);
    }
    if unit.keep_alive {
        let _ = setsockopt(fd, sockopt::KeepAlive, &true);
    }
    if let Some(bs) = unit.send_buffer {
        let _ = setsockopt(fd, sockopt::SndBuf, &(bs as usize));
    }
    if let Some(rb) = unit.receive_buffer {
        let _ = setsockopt(fd, sockopt::RcvBuf, &(rb as usize));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;
    use systeml_unit::socket::{ListenSpec, SocketAddrSpec, SocketUnit};

    #[test]
    fn binds_localhost_stream() {
        let mut unit = SocketUnit::default();
        unit.listen.push(ListenSpec::Stream(SocketAddrSpec::Inet(
            std::net::SocketAddr::from_str("127.0.0.1:0").unwrap(),
        )));
        let ls = bind_all(&unit).unwrap();
        assert_eq!(ls.len(), 1);
        assert!(ls[0].raw_fd() >= 0);
    }

    #[test]
    fn binds_unix_path() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("s.sock");
        let mut unit = SocketUnit::default();
        unit.listen
            .push(ListenSpec::Stream(SocketAddrSpec::UnixPath(p.clone())));
        unit.remove_on_stop = true;
        let ls = bind_all(&unit).unwrap();
        assert_eq!(ls.len(), 1);
        assert!(p.exists());
    }
}
