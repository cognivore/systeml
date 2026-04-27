//! `.path` activation engine.
//!
//! Watches paths via kqueue (macOS). Re-evaluates `PathExists*` /
//! `DirectoryNotEmpty=` predicates on every event. Initial-true semantics:
//! if the predicate holds at load time, fire once immediately.

#[cfg(target_os = "macos")]
pub mod kqueue;

use anyhow::Result;
use std::path::Path;
use std::time::{Duration, Instant};
use systeml_unit::path_unit::{PathUnit, PathWatch};

/// Returns `true` if any of the unit's predicates are currently satisfied.
pub fn predicate_holds(p: &PathUnit) -> bool {
    p.watches.iter().any(predicate_for_watch)
}

/// Per-watch predicate evaluation (without filesystem events).
pub fn predicate_for_watch(w: &PathWatch) -> bool {
    match w {
        PathWatch::Exists(p) => p.exists(),
        PathWatch::ExistsGlob(g) => glob_any_exists(g),
        PathWatch::Changed(p) | PathWatch::Modified(p) => p.exists(),
        PathWatch::DirectoryNotEmpty(d) => directory_not_empty(d),
    }
}

/// Naive glob: handles a single trailing `*` and exact paths. systemd allows
/// full glob(3) but we keep it simple for Phase 2.
pub fn glob_any_exists(pattern: &str) -> bool {
    if let Some(idx) = pattern.find('*') {
        let prefix = &pattern[..idx];
        let suffix = &pattern[idx + 1..];
        let dir_path = Path::new(prefix);
        let (dir, head) = if dir_path.is_dir() {
            (dir_path.to_path_buf(), String::new())
        } else if let Some(parent) = dir_path.parent() {
            let head = dir_path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_owned();
            (parent.to_path_buf(), head)
        } else {
            (Path::new(".").to_path_buf(), String::new())
        };
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => return false,
        };
        for entry in entries.flatten() {
            let n = entry.file_name();
            let s = n.to_string_lossy();
            if s.starts_with(&head) && s.ends_with(suffix) {
                return true;
            }
        }
        false
    } else {
        Path::new(pattern).exists()
    }
}

fn directory_not_empty(p: &Path) -> bool {
    match std::fs::read_dir(p) {
        Ok(mut it) => it.next().is_some(),
        Err(_) => false,
    }
}

/// `MakeDirectory=` + `DirectoryMode=` realisation.
pub fn ensure_directories(p: &PathUnit) -> Result<()> {
    if !p.make_directory {
        return Ok(());
    }
    let mode = p.directory_mode.unwrap_or(0o755);
    for w in &p.watches {
        let parent_to_make = match w {
            PathWatch::Exists(pp)
            | PathWatch::Changed(pp)
            | PathWatch::Modified(pp)
            | PathWatch::DirectoryNotEmpty(pp) => pp.parent().map(|p| p.to_path_buf()),
            PathWatch::ExistsGlob(_) => None,
        };
        if let Some(d) = parent_to_make {
            std::fs::create_dir_all(&d)?;
            #[allow(unused_imports)]
            use std::os::unix::fs::PermissionsExt;
            if let Ok(meta) = std::fs::metadata(&d) {
                let mut perms = meta.permissions();
                perms.set_mode(mode);
                let _ = std::fs::set_permissions(&d, perms);
            }
        }
    }
    Ok(())
}

/// Rate limiter: `TriggerLimitIntervalSec=` / `TriggerLimitBurst=`.
#[derive(Debug, Default)]
pub struct TriggerLimiter {
    /// Bursts allowed per interval. Default 200.
    pub burst: u32,
    /// Window length.
    pub interval: Option<Duration>,
    /// Window start.
    pub window_started: Option<Instant>,
    /// Trigger count in current window.
    pub count: u32,
}

impl TriggerLimiter {
    /// Build from a unit. `None` if no limit configured.
    pub fn from_unit(p: &PathUnit) -> Self {
        let mut me = Self {
            burst: p.trigger_limit_burst.unwrap_or(200),
            interval: p.trigger_limit_interval_sec.map(|d| d.as_std()),
            window_started: None,
            count: 0,
        };
        // Default interval per systemd is 2s if burst is given without it.
        if me.interval.is_none() && p.trigger_limit_burst.is_some() {
            me.interval = Some(Duration::from_secs(2));
        }
        me
    }

    /// Returns true if a trigger is allowed now; updates the counter.
    pub fn allow(&mut self) -> bool {
        let now = Instant::now();
        let interval = match self.interval {
            Some(d) => d,
            None => return true,
        };
        match self.window_started {
            Some(start) if now.duration_since(start) <= interval => {
                if self.count >= self.burst {
                    false
                } else {
                    self.count += 1;
                    true
                }
            }
            _ => {
                self.window_started = Some(now);
                self.count = 1;
                true
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use systeml_unit::path_unit::{PathUnit, PathWatch};

    #[test]
    fn exists_predicate() {
        let mut p = PathUnit::default();
        let tmp = tempfile::NamedTempFile::new().unwrap();
        p.watches.push(PathWatch::Exists(tmp.path().to_owned()));
        assert!(predicate_holds(&p));
    }

    #[test]
    fn does_not_exist() {
        let mut p = PathUnit::default();
        p.watches
            .push(PathWatch::Exists(PathBuf::from("/no/such/path/zzz")));
        assert!(!predicate_holds(&p));
    }

    #[test]
    fn glob_matches() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("hello.txt"), "").unwrap();
        let pattern = format!("{}/*.txt", dir.path().display());
        assert!(glob_any_exists(&pattern));
    }

    #[test]
    fn limiter_burst() {
        let p = PathUnit {
            trigger_limit_burst: Some(2),
            ..Default::default()
        };
        let mut l = TriggerLimiter::from_unit(&p);
        assert!(l.allow());
        assert!(l.allow());
        assert!(!l.allow());
    }
}
