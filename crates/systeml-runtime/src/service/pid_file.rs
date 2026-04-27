//! `PIDFile=` reader for `Type=forking`.
//!
//! Polls the file with backoff up to a deadline, matching systemd-stable's
//! behaviour: forking services often write their pid asynchronously after
//! the parent exits.

use std::path::Path;
use std::time::{Duration, Instant};
use tokio::time::sleep;

/// Read a pid from `path`, retrying with 50ms backoff up to `deadline`.
/// Returns `None` on timeout.
pub async fn read_with_retry(path: &Path, deadline: Instant) -> Option<i32> {
    loop {
        if let Ok(s) = std::fs::read_to_string(path) {
            if let Some(line) = s.lines().next() {
                if let Ok(pid) = line.trim().parse::<i32>() {
                    if pid > 0 {
                        return Some(pid);
                    }
                }
            }
        }
        if Instant::now() >= deadline {
            return None;
        }
        sleep(Duration::from_millis(50)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn reads_immediate() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), "9999\n").unwrap();
        let p = read_with_retry(tmp.path(), Instant::now() + Duration::from_millis(200)).await;
        assert_eq!(p, Some(9999));
    }

    #[tokio::test]
    async fn times_out() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::remove_file(tmp.path()).ok();
        let p = read_with_retry(tmp.path(), Instant::now() + Duration::from_millis(150)).await;
        assert_eq!(p, None);
    }
}
