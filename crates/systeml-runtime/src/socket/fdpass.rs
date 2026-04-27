//! `LISTEN_FDS=` protocol — pass listening fds to a child via env vars.
//!
//! Per `sd_listen_fds(3)`:
//! - `LISTEN_FDS=N` — number of fds. They start at fd 3 and are contiguous.
//! - `LISTEN_PID=<pid>` — must equal `getpid()` of the receiving process to
//!   guard against accidental inheritance.
//! - `LISTEN_FDNAMES=name1:name2:...` — colon-separated names, one per fd.
//!
//! On macOS we don't have `pidfd`, so the standard pattern is: in the child's
//! `pre_exec`, `dup2(listener_fd, 3+i)` and unset `FD_CLOEXEC` on the new fds.

#![allow(unsafe_code)]

use crate::socket::listen::Listener;
use anyhow::{Context, Result};
use nix::fcntl::{fcntl, FcntlArg, FdFlag};
use std::os::fd::RawFd;
use tokio::process::Command;

/// Apply the `LISTEN_FDS=` env vars and a `pre_exec` step that dup2s every
/// listener into fd 3, 4, … of the child.
///
/// `command` will run with `LISTEN_FDS=<n>`, `LISTEN_PID` set in the child
/// post-fork, and the listener fds available with FD_CLOEXEC cleared.
pub fn install(command: &mut Command, listeners: &[Listener]) -> Result<()> {
    if listeners.is_empty() {
        return Ok(());
    }
    let n = listeners.len();
    let names: Vec<String> = listeners.iter().map(|l| l.name.clone()).collect();
    let raw_fds: Vec<RawFd> = listeners.iter().map(|l| l.raw_fd()).collect();

    command.env("LISTEN_FDS", n.to_string());
    command.env("LISTEN_FDNAMES", names.join(":"));

    // SAFETY: pre_exec runs in the forked child between fork() and execve().
    // We only call async-signal-safe operations: dup2, fcntl, getpid, setenv
    // (via libc). We avoid Rust allocations inside the closure.
    unsafe {
        let raw_fds = raw_fds.clone();
        command.pre_exec(move || {
            // Dup each listener into 3+i. Note dup2 clears FD_CLOEXEC on the
            // destination, which is what we want.
            for (i, src) in raw_fds.iter().enumerate() {
                let dst: RawFd = (3 + i) as RawFd;
                let mut attempt = 0;
                loop {
                    let r = libc::dup2(*src, dst);
                    if r == dst {
                        break;
                    }
                    if r == -1 {
                        let err = std::io::Error::last_os_error();
                        if err.raw_os_error() == Some(libc::EINTR) && attempt < 5 {
                            attempt += 1;
                            continue;
                        }
                        return Err(err);
                    }
                    break;
                }
                // Make sure FD_CLOEXEC is *off* on the dst.
                let flags = libc::fcntl(dst, libc::F_GETFD);
                if flags >= 0 {
                    libc::fcntl(dst, libc::F_SETFD, flags & !libc::FD_CLOEXEC);
                }
            }
            // LISTEN_PID needs to be our pid, set after fork.
            let pid = libc::getpid();
            // Build "LISTEN_PID=<pid>" without allocating.
            let mut buf = [0u8; 32];
            let mut idx = 0;
            for c in b"LISTEN_PID=" {
                buf[idx] = *c;
                idx += 1;
            }
            // Manual int → ascii.
            let mut digits = [0u8; 12];
            let mut dlen = 0;
            let mut p = pid;
            if p < 0 {
                p = 0;
            }
            if p == 0 {
                digits[0] = b'0';
                dlen = 1;
            } else {
                while p > 0 && dlen < digits.len() {
                    digits[dlen] = b'0' + (p % 10) as u8;
                    p /= 10;
                    dlen += 1;
                }
                digits[..dlen].reverse();
            }
            for d in &digits[..dlen] {
                if idx < buf.len() {
                    buf[idx] = *d;
                    idx += 1;
                }
            }
            if idx < buf.len() {
                buf[idx] = 0;
            }
            // putenv requires the string remain valid; use setenv instead.
            // Find the '=' to split.
            let val_start = b"LISTEN_PID=".len();
            let key = b"LISTEN_PID\0";
            // Construct value cstr
            let mut val = [0u8; 16];
            let val_len = idx - val_start;
            val[..val_len].copy_from_slice(&buf[val_start..val_start + val_len]);
            val[val_len] = 0;
            libc::setenv(
                key.as_ptr() as *const libc::c_char,
                val.as_ptr() as *const libc::c_char,
                1,
            );
            Ok(())
        });
    }

    Ok(())
}

/// Clear `FD_CLOEXEC` on a fd directly (used for tests / direct fd manip).
pub fn clear_cloexec(fd: RawFd) -> Result<()> {
    let flags = fcntl(fd, FcntlArg::F_GETFD).context("F_GETFD")?;
    let mut new = FdFlag::from_bits_truncate(flags);
    new.remove(FdFlag::FD_CLOEXEC);
    fcntl(fd, FcntlArg::F_SETFD(new)).context("F_SETFD")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_install_is_noop() {
        let mut c = Command::new("/bin/true");
        install(&mut c, &[]).unwrap();
    }
}
