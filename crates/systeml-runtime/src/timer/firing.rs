//! Timer firing engine — the missing half of `systeml-runtime/src/timer/`.
//!
//! `next_overall` is pure: it computes "what's the next instant this timer
//! should fire?" but nothing wakes up at that instant and triggers the
//! linked service. This module is that wake-up loop.
//!
//! # Design
//!
//! One [`TimerScheduler`] per [`Manager`]. It runs as a single tokio task
//! and holds a clone of `Arc<RwLock<Manager>>`. Every iteration:
//!
//! 1. Lock the manager (read), iterate all `.timer` units in
//!    [`ActiveState::Active`], compute each one's [`next_overall`], take
//!    the earliest as the "next deadline".
//! 2. `tokio::select!` between sleeping until that deadline and a
//!    [`TimerControl`] message on a channel (used by `daemon-reload` to
//!    tell us to recompute).
//! 3. When the sleep wakes, lock the manager (write), call
//!    `start_unit` on the linked `.service`, persist the fire timestamp.
//! 4. Loop.
//!
//! When there are no active timers, the loop blocks indefinitely on the
//! control channel until something prods it.
//!
//! # Persistence
//!
//! `Persistent=yes` timers store their last-fire stamp at
//! `$XDG_STATE_HOME/systeml/timers/<name>.timer` (via `read_last_fire` /
//! `write_last_fire` in this module's parent). The scheduler also caches
//! last-fires in memory across iterations; persistence is the source of
//! truth across daemon restarts.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use systeml_deps::JobMode;
use systeml_unit::name::UnitKind;
use systeml_unit::{UnitName, UnitTypeData};
use time::OffsetDateTime;
use tokio::sync::{broadcast, mpsc, RwLock};
use tracing::{debug, info, warn};

use crate::manager::UnitEvent;
use crate::state::ActiveState;
use crate::timer::{next_overall, read_last_fire, write_last_fire};
use crate::Manager;

/// Control messages the scheduler accepts.
#[derive(Debug, Clone)]
pub enum TimerControl {
    /// Recompute the schedule. Sent after `daemon-reload` or any
    /// `start_unit`/`stop_unit` of a `.timer`.
    Refresh,
    /// Stop the scheduler task.
    Shutdown,
}

/// Sender side of the scheduler's control channel.
pub type TimerControlSender = mpsc::Sender<TimerControl>;

/// Spawn a timer scheduler bound to `manager`. Returns the control
/// sender so the manager can poke the scheduler when units change.
///
/// The scheduler task lives until [`TimerControl::Shutdown`] is sent or
/// the receiver is dropped (which happens when the returned sender's
/// last clone is dropped).
pub fn spawn(manager: Arc<RwLock<Manager>>) -> TimerControlSender {
    let (tx, rx) = mpsc::channel(64);
    tokio::spawn(async move {
        let events = manager.read().await.subscribe();
        let scheduler = TimerScheduler {
            manager,
            manager_start: OffsetDateTime::now_utc(),
            rx,
            events,
            last_fires: HashMap::new(),
            triggers: HashMap::new(),
        };
        run(scheduler).await;
    });
    tx
}

struct TimerScheduler {
    manager: Arc<RwLock<Manager>>,
    manager_start: OffsetDateTime,
    rx: mpsc::Receiver<TimerControl>,
    /// Subscriber to manager state-change events. Used to mirror systemd's
    /// `timer_trigger_notify`: when a unit we triggered deactivates, flip
    /// its timer back from "running" to "waiting".
    events: broadcast::Receiver<UnitEvent>,
    last_fires: HashMap<UnitName, OffsetDateTime>,
    /// Trigger unit → timer that fired it. Populated when `fire()` queues
    /// a long-lived target whose substate is still `Active` after dispatch;
    /// drained when the target deactivates.
    triggers: HashMap<UnitName, UnitName>,
}

async fn run(mut sched: TimerScheduler) {
    info!("timer scheduler started");
    loop {
        let next = sched.compute_next().await;
        match next {
            Some((deadline, unit)) => {
                let sleep_dur = deadline_to_sleep(deadline);
                debug!(timer = %unit, sleep_seconds = sleep_dur.as_secs(),
                    "next firing");
                tokio::select! {
                    _ = tokio::time::sleep(sleep_dur) => {
                        sched.fire(&unit, deadline).await;
                    }
                    msg = sched.rx.recv() => {
                        match msg {
                            Some(TimerControl::Refresh) => {
                                debug!("scheduler refresh requested");
                                continue;
                            }
                            Some(TimerControl::Shutdown) | None => break,
                        }
                    }
                    evt = sched.events.recv() => {
                        sched.handle_event(evt).await;
                    }
                }
            }
            None => {
                // No active timers. Park until something changes.
                debug!("no active timers; waiting for refresh");
                tokio::select! {
                    msg = sched.rx.recv() => {
                        match msg {
                            Some(TimerControl::Refresh) => continue,
                            Some(TimerControl::Shutdown) | None => break,
                        }
                    }
                    evt = sched.events.recv() => {
                        sched.handle_event(evt).await;
                    }
                }
            }
        }
    }
    info!("timer scheduler exiting");
}

impl TimerScheduler {
    /// Walk every loaded `.timer` in [`ActiveState::Active`] and find the
    /// earliest next-fire across all of them.
    async fn compute_next(&self) -> Option<(OffsetDateTime, UnitName)> {
        let mgr = self.manager.read().await;
        let now = OffsetDateTime::now_utc();
        let mut best: Option<(OffsetDateTime, UnitName)> = None;

        for (name, lu) in &mgr.units {
            if name.kind != UnitKind::Timer {
                continue;
            }
            let active = mgr
                .status
                .get(name)
                .map(|s| s.active)
                .unwrap_or(ActiveState::Inactive);
            if active != ActiveState::Active {
                continue;
            }
            let UnitTypeData::Timer(t) = &lu.unit.kind else {
                continue;
            };
            let last_fire = self
                .last_fires
                .get(name)
                .copied()
                .or_else(|| read_last_fire(name));
            let nf = next_overall(now, self.manager_start, None, None, None, t, last_fire);
            if let Some(t) = nf {
                match &best {
                    None => best = Some((t, name.clone())),
                    Some((cur_best, _)) if t < *cur_best => {
                        best = Some((t, name.clone()));
                    }
                    _ => {}
                }
            }
        }
        best
    }

    /// Fire one timer: invoke `start_unit` on the linked `.service`,
    /// persist the fire timestamp.
    async fn fire(&mut self, timer_name: &UnitName, fire_time: OffsetDateTime) {
        // Resolve the linked unit. systemd default: same stem, kind=service.
        let target = {
            let mgr = self.manager.read().await;
            let Some(lu) = mgr.units.get(timer_name) else {
                warn!(timer = %timer_name, "timer disappeared between scheduling and fire");
                return;
            };
            let UnitTypeData::Timer(t) = &lu.unit.kind else {
                return;
            };
            t.unit.clone().unwrap_or_else(|| UnitName {
                prefix: timer_name.prefix.clone(),
                instance: timer_name.instance.clone(),
                kind: UnitKind::Service,
            })
        };

        info!(timer = %timer_name, target = %target, "timer fires");
        let mut mgr = self.manager.write().await;
        // Mirror systemd's TIMER_RUNNING transition: while we're triggering
        // the target, the timer's sub-state is "running".
        mgr.mark_state(timer_name, ActiveState::Active, "running");
        let result = mgr.start_unit(target.clone(), JobMode::Replace).await;
        // Decide whether to flip back to "waiting" now or wait for the
        // trigger to deactivate. For Type=oneshot services, start_unit
        // already waited for the process to finish, so by the time we get
        // here the trigger is no longer Active and we can return to
        // "waiting" immediately. For long-lived triggers (simple/forking/
        // notify) the trigger is still Active; record it in `triggers` and
        // let `handle_event` flip the timer back when it eventually
        // deactivates — same role systemd's timer_trigger_notify plays.
        let trigger_still_active = mgr
            .status
            .get(&target)
            .map(|s| s.active == ActiveState::Active)
            .unwrap_or(false);
        if trigger_still_active {
            self.triggers.insert(target.clone(), timer_name.clone());
        } else {
            mgr.mark_state(timer_name, ActiveState::Active, "waiting");
        }
        drop(mgr);
        match result {
            Ok(_) => {
                self.last_fires.insert(timer_name.clone(), fire_time);
                if let Err(e) = write_last_fire(timer_name, fire_time) {
                    warn!(timer = %timer_name, error = %e,
                        "failed to persist last-fire stamp");
                }
            }
            Err(e) => warn!(timer = %timer_name, target = %target, error = %e,
                "linked service failed to start"),
        }
    }

    /// Mirror of systemd's `timer_trigger_notify`: when a unit we triggered
    /// deactivates, flip the timer that fired it back from "running" to
    /// "waiting". Lagged events fall back to walking every watched trigger.
    async fn handle_event(&mut self, evt: Result<UnitEvent, broadcast::error::RecvError>) {
        match evt {
            Ok(UnitEvent::StateChanged { unit, active, .. }) => {
                if active == ActiveState::Active {
                    return;
                }
                let Some(timer) = self.triggers.remove(&unit) else {
                    return;
                };
                self.flip_to_waiting(&timer).await;
            }
            Ok(UnitEvent::UnitRemoved(unit)) => {
                // Trigger went away entirely; clear watch and flip timer.
                if let Some(timer) = self.triggers.remove(&unit) {
                    self.flip_to_waiting(&timer).await;
                }
            }
            Err(broadcast::error::RecvError::Lagged(n)) => {
                // We dropped n events. Resync by polling current trigger
                // states; flip timers whose triggers are no longer Active.
                debug!(dropped = n, "scheduler events lagged; resyncing");
                let mut to_flip: Vec<UnitName> = Vec::new();
                {
                    let mgr = self.manager.read().await;
                    self.triggers.retain(|trigger, timer| {
                        let still_active = mgr
                            .status
                            .get(trigger)
                            .map(|s| s.active == ActiveState::Active)
                            .unwrap_or(false);
                        if !still_active {
                            to_flip.push(timer.clone());
                            return false;
                        }
                        true
                    });
                }
                for timer in to_flip {
                    self.flip_to_waiting(&timer).await;
                }
            }
            // Other event variants are not relevant to the trigger watch.
            Ok(_) | Err(broadcast::error::RecvError::Closed) => {}
        }
    }

    async fn flip_to_waiting(&self, timer: &UnitName) {
        let mut mgr = self.manager.write().await;
        // Only flip if the timer is still Active — a concurrent stop_unit
        // may have moved it to Inactive/dead, and we mustn't override that.
        let still_active = mgr
            .status
            .get(timer)
            .map(|s| s.active == ActiveState::Active)
            .unwrap_or(false);
        if still_active {
            mgr.mark_state(timer, ActiveState::Active, "waiting");
        }
    }
}

/// How long to sleep until `deadline`. Floors at zero — if the deadline
/// is already past, fire immediately.
fn deadline_to_sleep(deadline: OffsetDateTime) -> Duration {
    let now = OffsetDateTime::now_utc();
    let delta = deadline - now;
    if delta.is_negative() {
        return Duration::ZERO;
    }
    let secs = delta.whole_seconds().max(0) as u64;
    let nanos = delta.subsec_nanoseconds().max(0) as u32;
    Duration::new(secs, nanos)
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    #[test]
    fn deadline_zero_when_past() {
        let past = datetime!(2000-01-01 00:00:00 UTC);
        assert_eq!(deadline_to_sleep(past), Duration::ZERO);
    }

    #[test]
    fn deadline_positive_when_future() {
        let future = OffsetDateTime::now_utc() + time::Duration::seconds(10);
        let dur = deadline_to_sleep(future);
        // Allow a couple seconds of slop for test scheduling jitter.
        assert!(dur.as_secs() >= 8 && dur.as_secs() <= 11, "got {dur:?}");
    }
}
