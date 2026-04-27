//! macOS-specific kqueue path watcher.
//!
//! systemd's `.path` semantics map onto kqueue VNODE filter flags as follows:
//! - `PathChanged=` — `NOTE_WRITE` on the file (close-after-write).
//! - `PathModified=` — `NOTE_WRITE | NOTE_EXTEND | NOTE_ATTRIB` on the file.
//! - `PathExists=` — initial poll then watch parent dir for `NOTE_WRITE`
//!   (directory contents changed).
//! - `DirectoryNotEmpty=` — same as `PathExists=` but checks directory
//!   non-emptiness on every event.

use anyhow::{anyhow, Result};
use kqueue::{EventFilter, FilterFlag, Watcher};
use std::path::{Path, PathBuf};
use std::time::Duration;
use systeml_unit::path_unit::{PathUnit, PathWatch};

/// Per-`.path`-unit watcher state.
pub struct PathWatcher {
    /// Underlying kqueue watcher.
    inner: Watcher,
    /// Tracked watch records.
    watches: Vec<TrackedWatch>,
}

/// What we registered with kqueue, plus the original directive.
struct TrackedWatch {
    /// On-disk path passed to kqueue (file or parent dir).
    path: PathBuf,
    /// Original `PathWatch` directive (used to evaluate predicate on event).
    spec: PathWatch,
}

impl PathWatcher {
    /// Construct a new watcher and register all of `unit`'s watches.
    pub fn new(unit: &PathUnit) -> Result<Self> {
        let mut inner = Watcher::new().map_err(|e| anyhow!("kqueue init: {e}"))?;
        let mut watches = Vec::new();
        for w in &unit.watches {
            register_watch(&mut inner, w, &mut watches)?;
        }
        inner
            .watch()
            .map_err(|e| anyhow!("kqueue activate: {e}"))?;
        Ok(Self { inner, watches })
    }

    /// Block until any watched path event arrives or `timeout` elapses.
    /// Returns `Some(idx)` of the matching watch, or `None` on timeout.
    pub fn poll(&self, timeout: Duration) -> Option<usize> {
        let _ev = self.inner.poll(Some(timeout))?;
        // We don't strictly map the event back — re-evaluate every predicate.
        for (i, w) in self.watches.iter().enumerate() {
            if crate::path::predicate_for_watch(&w.spec) {
                return Some(i);
            }
        }
        // Even if no predicate held (e.g. just a delete), we received an event;
        // the caller should re-check.
        Some(0)
    }
}

fn register_watch(
    inner: &mut Watcher,
    w: &PathWatch,
    out: &mut Vec<TrackedWatch>,
) -> Result<()> {
    match w {
        PathWatch::Exists(p) | PathWatch::DirectoryNotEmpty(p) => {
            // Watch the parent dir for changes; if path itself exists already,
            // also watch it directly.
            if let Some(parent) = p.parent() {
                if parent.exists() {
                    add_dir(inner, parent)?;
                }
            }
            if p.exists() {
                add_file(inner, p, FilterFlag::NOTE_WRITE | FilterFlag::NOTE_DELETE)?;
            }
            out.push(TrackedWatch {
                path: p.clone(),
                spec: w.clone(),
            });
        }
        PathWatch::Changed(p) => {
            if p.exists() {
                add_file(inner, p, FilterFlag::NOTE_WRITE | FilterFlag::NOTE_DELETE)?;
            }
            if let Some(parent) = p.parent() {
                if parent.exists() {
                    add_dir(inner, parent)?;
                }
            }
            out.push(TrackedWatch {
                path: p.clone(),
                spec: w.clone(),
            });
        }
        PathWatch::Modified(p) => {
            if p.exists() {
                add_file(
                    inner,
                    p,
                    FilterFlag::NOTE_WRITE
                        | FilterFlag::NOTE_EXTEND
                        | FilterFlag::NOTE_ATTRIB
                        | FilterFlag::NOTE_DELETE,
                )?;
            }
            if let Some(parent) = p.parent() {
                if parent.exists() {
                    add_dir(inner, parent)?;
                }
            }
            out.push(TrackedWatch {
                path: p.clone(),
                spec: w.clone(),
            });
        }
        PathWatch::ExistsGlob(g) => {
            // For globs, we watch the directory part.
            let pattern = Path::new(g);
            let dir = if let Some(parent) = pattern.parent() {
                if parent.exists() {
                    parent.to_path_buf()
                } else {
                    return Ok(());
                }
            } else {
                Path::new(".").to_path_buf()
            };
            add_dir(inner, &dir)?;
            out.push(TrackedWatch {
                path: dir,
                spec: w.clone(),
            });
        }
    }
    Ok(())
}

fn add_dir(w: &mut Watcher, p: &Path) -> Result<()> {
    w.add_filename(
        p,
        EventFilter::EVFILT_VNODE,
        FilterFlag::NOTE_WRITE | FilterFlag::NOTE_DELETE | FilterFlag::NOTE_RENAME,
    )
    .map_err(|e| anyhow!("kqueue add_filename {p:?}: {e}"))
}

fn add_file(w: &mut Watcher, p: &Path, flags: FilterFlag) -> Result<()> {
    w.add_filename(p, EventFilter::EVFILT_VNODE, flags)
        .map_err(|e| anyhow!("kqueue add_filename {p:?}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use systeml_unit::path_unit::{PathUnit, PathWatch};

    #[test]
    fn create_watcher_for_existing_file() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let mut p = PathUnit::default();
        p.watches.push(PathWatch::Exists(tmp.path().to_owned()));
        let _w = PathWatcher::new(&p).unwrap();
    }
}
