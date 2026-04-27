//! `.timer` activation engine.
//!
//! [`schedule`] is a pure-function next-fire calculator. Persistent state for
//! `Persistent=yes` lives at `$XDG_STATE_HOME/systeml/timers/<name>.timer`.

pub mod schedule;

use anyhow::{anyhow, Result};
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use systeml_unit::timer::TimerUnit;
use systeml_unit::UnitName;
use time::OffsetDateTime;

pub use schedule::next_fire;

/// Where we persist last-fire timestamps.
pub fn state_file(name: &UnitName) -> Option<PathBuf> {
    systeml_unit::search::systeml_state_dir().map(|d| d.join("timers").join(name.filename()))
}

/// Read the last persisted fire timestamp, if any.
pub fn read_last_fire(name: &UnitName) -> Option<OffsetDateTime> {
    let path = state_file(name)?;
    let s = std::fs::read_to_string(&path).ok()?;
    let secs: i64 = s.trim().parse().ok()?;
    OffsetDateTime::from_unix_timestamp(secs).ok()
}

/// Write a fire timestamp atomically.
pub fn write_last_fire(name: &UnitName, t: OffsetDateTime) -> Result<()> {
    let path = state_file(name).ok_or_else(|| anyhow!("no state dir for timers"))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, t.unix_timestamp().to_string())?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Compute the next fire time across every `OnCalendar=` plus monotonic
/// triggers (`OnActiveSec`, `OnUnitActiveSec`, …). Returns the *earliest*
/// instant > `now`. `manager_start` is the manager's startup time, used for
/// `OnStartupSec`/`OnBootSec`.
pub fn next_overall(
    now: OffsetDateTime,
    manager_start: OffsetDateTime,
    activated_at: Option<OffsetDateTime>,
    last_unit_active: Option<OffsetDateTime>,
    last_unit_inactive: Option<OffsetDateTime>,
    timer: &TimerUnit,
    last_fire: Option<OffsetDateTime>,
) -> Option<OffsetDateTime> {
    let mut best: Option<OffsetDateTime> = None;

    for spec in &timer.on_calendar {
        if let Some(t) = schedule::next_fire(now, spec, last_fire) {
            best = Some(min_t(best, t));
        }
    }
    if let Some(d) = timer.on_active_sec {
        if let Some(at) = activated_at {
            let t = at + duration(d.as_std());
            if t > now {
                best = Some(min_t(best, t));
            }
        }
    }
    if let Some(d) = timer.on_startup_sec {
        let t = manager_start + duration(d.as_std());
        if t > now {
            best = Some(min_t(best, t));
        }
    }
    if let Some(d) = timer.on_boot_sec {
        // We don't track boot time separately; alias to manager_start.
        let t = manager_start + duration(d.as_std());
        if t > now {
            best = Some(min_t(best, t));
        }
    }
    if let Some(d) = timer.on_unit_active_sec {
        if let Some(la) = last_unit_active {
            let t = la + duration(d.as_std());
            if t > now {
                best = Some(min_t(best, t));
            }
        }
    }
    if let Some(d) = timer.on_unit_inactive_sec {
        if let Some(li) = last_unit_inactive {
            let t = li + duration(d.as_std());
            if t > now {
                best = Some(min_t(best, t));
            }
        }
    }
    best
}

fn duration(d: Duration) -> time::Duration {
    let s = d.as_secs() as i64;
    let n = d.subsec_nanos() as i32;
    time::Duration::new(s, n)
}

fn min_t(a: Option<OffsetDateTime>, b: OffsetDateTime) -> OffsetDateTime {
    match a {
        None => b,
        Some(x) if x < b => x,
        _ => b,
    }
}

/// Convenience: now() at UTC offset (no panic).
pub fn now_utc() -> OffsetDateTime {
    OffsetDateTime::now_utc()
}

/// Compute `tokio::time::Instant` for an absolute `OffsetDateTime`.
/// Returns `Some(Instant)` only if the moment is in the future.
pub fn to_instant(t: OffsetDateTime) -> Option<tokio::time::Instant> {
    let target = SystemTime::UNIX_EPOCH + Duration::from_secs(t.unix_timestamp() as u64);
    let now_sys = SystemTime::now();
    let delta = target.duration_since(now_sys).ok()?;
    let _ = UNIX_EPOCH;
    Some(tokio::time::Instant::now() + delta)
}

#[cfg(test)]
mod tests {
    use super::*;
    use systeml_unit::CalendarSpec;
    use time::macros::datetime;

    #[test]
    fn next_overall_picks_earliest() {
        let mut t = TimerUnit::default();
        t.on_calendar
            .push(CalendarSpec::parse("*-*-* *:*:00").unwrap());
        let now = datetime!(2026-04-27 10:00:30 UTC);
        let n = next_overall(now, now, None, None, None, &t, None).unwrap();
        assert_eq!(n.minute(), 1);
    }
}
