//! `sd_notify(3)` protocol: `READY=1`, `STATUS=`, `MAINPID=`, `WATCHDOG=1`,
//! `STOPPING=1`, `RELOADING=1`, `BARRIER=1`.

use std::path::Path;
use tokio::net::UnixDatagram;

/// Parsed message from a single `sd_notify` datagram.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct NotifyMessage {
    /// `READY=1`.
    pub ready: bool,
    /// `STOPPING=1`.
    pub stopping: bool,
    /// `RELOADING=1`.
    pub reloading: bool,
    /// `WATCHDOG=1` ping.
    pub watchdog: bool,
    /// `BARRIER=1`.
    pub barrier: bool,
    /// `MAINPID=N`.
    pub main_pid: Option<i32>,
    /// `STATUS=...`.
    pub status: Option<String>,
    /// `WATCHDOG_USEC=N`.
    pub watchdog_usec: Option<u64>,
    /// `ERRNO=N`.
    pub errno: Option<i32>,
}

impl NotifyMessage {
    /// Parse a `sd_notify` datagram. Lines are `KEY=VALUE`, separated by `\n`.
    /// Unknown keys are ignored (forward-compat).
    pub fn parse(buf: &[u8]) -> Self {
        let mut out = Self::default();
        let s = match std::str::from_utf8(buf) {
            Ok(s) => s,
            Err(_) => return out,
        };
        for line in s.lines() {
            let Some((k, v)) = line.split_once('=') else { continue };
            match k {
                "READY" if v == "1" => out.ready = true,
                "STOPPING" if v == "1" => out.stopping = true,
                "RELOADING" if v == "1" => out.reloading = true,
                "WATCHDOG" if v == "1" => out.watchdog = true,
                "BARRIER" if v == "1" => out.barrier = true,
                "MAINPID" => out.main_pid = v.parse().ok(),
                "STATUS" => out.status = Some(v.to_owned()),
                "WATCHDOG_USEC" => out.watchdog_usec = v.parse().ok(),
                "ERRNO" => out.errno = v.parse().ok(),
                _ => {}
            }
        }
        out
    }
}

/// Bind a fresh `AF_UNIX` `SOCK_DGRAM` socket at `path`. Returns the bound
/// socket; caller is responsible for cleanup (`std::fs::remove_file`).
pub fn bind_socket(path: &Path) -> std::io::Result<UnixDatagram> {
    if path.exists() {
        let _ = std::fs::remove_file(path);
    }
    UnixDatagram::bind(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ready() {
        let m = NotifyMessage::parse(b"READY=1\nSTATUS=foo\nMAINPID=123\n");
        assert!(m.ready);
        assert_eq!(m.status.as_deref(), Some("foo"));
        assert_eq!(m.main_pid, Some(123));
    }

    #[test]
    fn parse_watchdog() {
        let m = NotifyMessage::parse(b"WATCHDOG=1\n");
        assert!(m.watchdog);
    }

    #[test]
    fn parse_unknown_key_ignored() {
        let m = NotifyMessage::parse(b"FRESH=novel\nREADY=1\n");
        assert!(m.ready);
    }
}
