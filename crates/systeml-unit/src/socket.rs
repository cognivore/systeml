//! `[Socket]` section: typed view + listen-spec parsing.
//!
//! See `man systemd.socket`.

use crate::name::UnitName;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::str::FromStr;

/// Address-family + payload for a `Listen*=` value.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum SocketAddrSpec {
    /// `0.0.0.0:80`, `[::]:80`, etc.
    Inet(SocketAddr),
    /// Bare port number — listen on both v4 and v6.
    Port(u16),
    /// Filesystem-bound unix socket: `/tmp/foo.sock`.
    UnixPath(PathBuf),
    /// Linux abstract namespace `@foo`. Unsupported on macOS — kept for
    /// round-trip and emits a warning at activation time.
    UnixAbstract(String),
    /// `vsock:CID:PORT` — Linux/QEMU only.
    Vsock(String),
    /// `netlink ROUTE 0` etc. — Linux-only.
    Netlink(String),
}

impl SocketAddrSpec {
    /// Parse a `ListenStream=` / `ListenDatagram=` value.
    pub fn parse(value: &str) -> Result<Self, String> {
        let v = value.trim();
        if v.is_empty() {
            return Err("empty listen spec".into());
        }
        // Bare port?
        if let Ok(port) = v.parse::<u16>() {
            return Ok(Self::Port(port));
        }
        // Path-based unix?
        if v.starts_with('/') {
            return Ok(Self::UnixPath(PathBuf::from(v)));
        }
        // Abstract unix?
        if let Some(rest) = v.strip_prefix('@') {
            return Ok(Self::UnixAbstract(rest.to_owned()));
        }
        // vsock: prefix.
        if let Some(rest) = v.strip_prefix("vsock:") {
            return Ok(Self::Vsock(rest.to_owned()));
        }
        // Netlink: keyword form.
        if v.starts_with("ROUTE")
            || v.starts_with("FIREWALL")
            || v.starts_with("INET")
            || v.starts_with("XFRM")
            || v.starts_with("NETFILTER")
        {
            return Ok(Self::Netlink(v.to_owned()));
        }
        // host:port (with brackets for IPv6).
        if let Ok(sa) = SocketAddr::from_str(v) {
            return Ok(Self::Inet(sa));
        }
        Err(format!("cannot parse listen spec {v:?}"))
    }
}

/// Concrete socket-listen entry: the kind plus its address.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum ListenSpec {
    /// `ListenStream=` (SOCK_STREAM).
    Stream(SocketAddrSpec),
    /// `ListenDatagram=` (SOCK_DGRAM).
    Datagram(SocketAddrSpec),
    /// `ListenSequentialPacket=` (SOCK_SEQPACKET; rare on macOS but supported via unix).
    SequentialPacket(SocketAddrSpec),
    /// `ListenFIFO=` — named pipe.
    Fifo(PathBuf),
    /// `ListenSpecial=` — character device.
    Special(PathBuf),
    /// `ListenNetlink=` — Linux-only.
    Netlink(String),
    /// `ListenMessageQueue=` — Linux POSIX MQ — unsupported.
    MessageQueue(String),
    /// `ListenUSBFunction=` — Linux-only.
    UsbFunction(PathBuf),
}

/// `BindIPv6Only=`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum BindIPv6Only {
    /// Default per kernel.
    #[default]
    Default,
    /// `both`.
    Both,
    /// `ipv6-only`.
    Ipv6Only,
}

impl BindIPv6Only {
    /// Parse.
    pub fn parse(s: &str) -> Result<Self, String> {
        Ok(match s {
            "default" => Self::Default,
            "both" => Self::Both,
            "ipv6-only" => Self::Ipv6Only,
            other => return Err(format!("unknown BindIPv6Only= {other:?}")),
        })
    }
}

/// Fully-typed `[Socket]` section.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct SocketUnit {
    /// All `Listen*=` directives in declaration order.
    pub listen: Vec<ListenSpec>,

    /// `Accept=` (default `no`).
    pub accept: bool,
    /// `Service=` — explicit target service. Default is the matching
    /// `<name>.service` (or template `<name>@.service` if `Accept=yes`).
    pub service: Option<UnitName>,

    /// `MaxConnections=`.
    pub max_connections: u32,
    /// `MaxConnectionsPerSource=`.
    pub max_connections_per_source: u32,
    /// `Backlog=`.
    pub backlog: u32,
    /// `KeepAlive=`.
    pub keep_alive: bool,
    /// `KeepAliveTimeSec=`.
    pub keep_alive_time_sec: Option<crate::duration::SdDuration>,
    /// `KeepAliveIntervalSec=`.
    pub keep_alive_interval_sec: Option<crate::duration::SdDuration>,
    /// `KeepAliveProbes=`.
    pub keep_alive_probes: Option<u32>,
    /// `NoDelay=` (TCP_NODELAY).
    pub no_delay: bool,
    /// `Priority=` — SO_PRIORITY.
    pub priority: Option<i32>,
    /// `DeferAcceptSec=`.
    pub defer_accept_sec: Option<crate::duration::SdDuration>,
    /// `ReceiveBuffer=`.
    pub receive_buffer: Option<u64>,
    /// `SendBuffer=`.
    pub send_buffer: Option<u64>,
    /// `IPTOS=`.
    pub ip_tos: Option<String>,
    /// `IPTTL=`.
    pub ip_ttl: Option<u32>,
    /// `Mark=`.
    pub mark: Option<i32>,
    /// `ReusePort=`.
    pub reuse_port: bool,
    /// `Transparent=`.
    pub transparent: bool,
    /// `Broadcast=`.
    pub broadcast: bool,
    /// `PassCredentials=` — Linux-only.
    pub pass_credentials: bool,
    /// `PassSecurity=` — Linux-only.
    pub pass_security: bool,
    /// `PassPacketInfo=`.
    pub pass_packet_info: bool,
    /// `BindIPv6Only=`.
    pub bind_ipv6_only: BindIPv6Only,
    /// `BindToDevice=` — Linux-only.
    pub bind_to_device: Option<String>,

    /// `SocketUser=`.
    pub socket_user: Option<String>,
    /// `SocketGroup=`.
    pub socket_group: Option<String>,
    /// `SocketMode=` — octal.
    pub socket_mode: Option<u32>,
    /// `DirectoryMode=` — octal.
    pub directory_mode: Option<u32>,
    /// `RemoveOnStop=`.
    pub remove_on_stop: bool,
    /// `Symlinks=`.
    pub symlinks: Vec<PathBuf>,

    /// `FileDescriptorName=`.
    pub file_descriptor_name: Option<String>,
    /// `TriggerLimitIntervalSec=`.
    pub trigger_limit_interval_sec: Option<crate::duration::SdDuration>,
    /// `TriggerLimitBurst=`.
    pub trigger_limit_burst: Option<u32>,

    /// `MessageQueueMaxMessages=`.
    pub mq_max_messages: Option<i64>,
    /// `MessageQueueMessageSize=`.
    pub mq_message_size: Option<i64>,

    /// Anything we accepted but didn't model: `(key, value)`.
    pub passthrough: Vec<(String, String)>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_port() {
        assert!(matches!(SocketAddrSpec::parse("80").unwrap(), SocketAddrSpec::Port(80)));
    }

    #[test]
    fn parse_path() {
        let s = SocketAddrSpec::parse("/tmp/foo.sock").unwrap();
        assert!(matches!(s, SocketAddrSpec::UnixPath(_)));
    }

    #[test]
    fn parse_v6() {
        let s = SocketAddrSpec::parse("[::1]:8080").unwrap();
        assert!(matches!(s, SocketAddrSpec::Inet(_)));
    }

    #[test]
    fn parse_abstract() {
        let s = SocketAddrSpec::parse("@foo").unwrap();
        assert!(matches!(s, SocketAddrSpec::UnixAbstract(_)));
    }
}
