//! Calendar next-fire computation. Walks forward minute-by-minute with a
//! month skip-ahead optimisation.

use systeml_unit::calendar::{CalendarSpec, FieldSet};
use time::{Date, Month, OffsetDateTime, PrimitiveDateTime, Time};

/// Compute the next instant strictly after `now` (and strictly after `last`,
/// if given) at which `spec` fires. Returns `None` if no time within ~10
/// years matches (essentially "never").
///
/// The algorithm mirrors systemd-stable's `calendarspec_next_usec()`:
/// - Decompose into Y/M/D/h/m/s.
/// - For each field from coarsest to finest, advance to the next value that
///   matches the FieldSet. If a coarser field changed, reset finer fields
///   to their minimum match.
/// - Continue until the result is in the future *and* the weekday matches.
pub fn next_fire(
    now: OffsetDateTime,
    spec: &CalendarSpec,
    last: Option<OffsetDateTime>,
) -> Option<OffsetDateTime> {
    let baseline = match last {
        Some(l) if l > now => l,
        _ => now,
    };
    let mut candidate = baseline + time::Duration::seconds(1);

    // Search bound: 10 years from baseline.
    let limit = baseline + time::Duration::days(366 * 10);

    while candidate < limit {
        // Year.
        if !spec.year.matches(candidate.year() as u32) {
            // Skip to Jan 1 of next allowed year.
            let next_year = next_in_set(&spec.year, candidate.year() as u32, 1970, 2199);
            let y = next_year?;
            if let Some(c) = at_year_start(y as i32, candidate.offset()) {
                candidate = c;
            } else {
                return None;
            }
            continue;
        }
        // Month.
        if !spec.month.matches(candidate.month() as u32) {
            let cur = candidate.month() as u32;
            let next_month = next_in_set(&spec.month, cur, 1, 12);
            match next_month {
                Some(m) if m > cur => {
                    if let Some(c) = at_month_start(
                        candidate.year(),
                        m,
                        candidate.offset(),
                    ) {
                        candidate = c;
                    } else {
                        return None;
                    }
                }
                _ => {
                    // Wrap to January next year.
                    if let Some(c) = at_year_start(candidate.year() + 1, candidate.offset()) {
                        candidate = c;
                    } else {
                        return None;
                    }
                }
            }
            continue;
        }
        // Day.
        if !spec.day.matches(candidate.day() as u32) {
            let cur = candidate.day() as u32;
            let max_day = days_in_month(candidate.year(), candidate.month());
            let next_day = next_in_set(&spec.day, cur, 1, max_day);
            match next_day {
                Some(d) if d > cur && d <= max_day => {
                    if let Some(c) = at_day_start(
                        candidate.year(),
                        candidate.month() as u32,
                        d,
                        candidate.offset(),
                    ) {
                        candidate = c;
                    } else {
                        return None;
                    }
                }
                _ => {
                    // Bump month.
                    let new_month = candidate.month() as u32 + 1;
                    if new_month > 12 {
                        if let Some(c) = at_year_start(candidate.year() + 1, candidate.offset()) {
                            candidate = c;
                        } else {
                            return None;
                        }
                    } else if let Some(c) = at_month_start(candidate.year(), new_month, candidate.offset()) {
                        candidate = c;
                    } else {
                        return None;
                    }
                }
            }
            continue;
        }
        // Weekday (1=Mon..7=Sun in spec; time crate ISO is the same).
        let wd_iso = candidate.weekday().number_from_monday() as u32;
        if !spec.weekdays.matches(wd_iso) {
            // Bump to next day at 00:00.
            let new_day = candidate.day() as u32 + 1;
            let max_day = days_in_month(candidate.year(), candidate.month());
            if new_day > max_day {
                let new_month = candidate.month() as u32 + 1;
                if new_month > 12 {
                    if let Some(c) = at_year_start(candidate.year() + 1, candidate.offset()) {
                        candidate = c;
                    } else {
                        return None;
                    }
                } else if let Some(c) = at_month_start(candidate.year(), new_month, candidate.offset()) {
                    candidate = c;
                } else {
                    return None;
                }
            } else if let Some(c) = at_day_start(
                candidate.year(),
                candidate.month() as u32,
                new_day,
                candidate.offset(),
            ) {
                candidate = c;
            } else {
                return None;
            }
            continue;
        }
        // Hour.
        if !spec.hour.matches(candidate.hour() as u32) {
            let cur = candidate.hour() as u32;
            match next_in_set(&spec.hour, cur, 0, 23) {
                Some(h) if h > cur => {
                    if let Some(c) = at_hour(candidate, h) {
                        candidate = c;
                    } else {
                        return None;
                    }
                }
                _ => {
                    // Bump day.
                    candidate = match candidate.checked_add(time::Duration::days(1)) {
                        Some(d) => d.replace_time(Time::from_hms(0, 0, 0).ok()?),
                        None => return None,
                    };
                }
            }
            continue;
        }
        // Minute.
        if !spec.minute.matches(candidate.minute() as u32) {
            let cur = candidate.minute() as u32;
            match next_in_set(&spec.minute, cur, 0, 59) {
                Some(m) if m > cur => {
                    if let Some(c) = at_minute(candidate, m) {
                        candidate = c;
                    } else {
                        return None;
                    }
                }
                _ => {
                    candidate = match candidate.checked_add(time::Duration::hours(1)) {
                        Some(d) => match d.replace_minute(0).and_then(|d| d.replace_second(0)) {
                            Ok(d) => d,
                            Err(_) => return None,
                        },
                        None => return None,
                    };
                }
            }
            continue;
        }
        // Second.
        if !spec.second.matches(candidate.second() as u32) {
            let cur = candidate.second() as u32;
            match next_in_set(&spec.second, cur, 0, 59) {
                Some(s) if s >= cur => {
                    if let Some(c) = at_second(candidate, s) {
                        candidate = c;
                    } else {
                        return None;
                    }
                }
                _ => {
                    candidate = match candidate.checked_add(time::Duration::minutes(1)) {
                        Some(d) => match d.replace_second(0) {
                            Ok(d) => d,
                            Err(_) => return None,
                        },
                        None => return None,
                    };
                }
            }
            continue;
        }
        // All match!
        if candidate > baseline {
            return Some(candidate);
        }
        candidate += time::Duration::seconds(1);
    }
    None
}

fn next_in_set(fs: &FieldSet, after: u32, lo: u32, hi: u32) -> Option<u32> {
    match &fs.values {
        None => Some(after.max(lo)),
        Some(vs) => {
            for &v in vs {
                if v >= lo && v <= hi && v >= after {
                    return Some(v);
                }
            }
            None
        }
    }
}

fn days_in_month(year: i32, month: time::Month) -> u32 {
    let next_year = if month as u8 == 12 { year + 1 } else { year };
    let next_month = if month as u8 == 12 {
        Month::January
    } else {
        // Safe: we just guarded month != 12 above.
        Month::try_from((month as u8) + 1).unwrap_or(Month::December)
    };
    let first_next = Date::from_calendar_date(next_year, next_month, 1).ok();
    let first_this = Date::from_calendar_date(year, month, 1).ok();
    match (first_next, first_this) {
        (Some(nn), Some(nt)) => (nn - nt).whole_days() as u32,
        _ => 31,
    }
}

fn at_year_start(year: i32, offset: time::UtcOffset) -> Option<OffsetDateTime> {
    let date = Date::from_calendar_date(year, Month::January, 1).ok()?;
    let dt = PrimitiveDateTime::new(date, Time::from_hms(0, 0, 0).ok()?);
    Some(dt.assume_offset(offset))
}

fn at_month_start(year: i32, month: u32, offset: time::UtcOffset) -> Option<OffsetDateTime> {
    let m = Month::try_from(month as u8).ok()?;
    let date = Date::from_calendar_date(year, m, 1).ok()?;
    let dt = PrimitiveDateTime::new(date, Time::from_hms(0, 0, 0).ok()?);
    Some(dt.assume_offset(offset))
}

fn at_day_start(year: i32, month: u32, day: u32, offset: time::UtcOffset) -> Option<OffsetDateTime> {
    let m = Month::try_from(month as u8).ok()?;
    let date = Date::from_calendar_date(year, m, day as u8).ok()?;
    let dt = PrimitiveDateTime::new(date, Time::from_hms(0, 0, 0).ok()?);
    Some(dt.assume_offset(offset))
}

fn at_hour(t: OffsetDateTime, h: u32) -> Option<OffsetDateTime> {
    let nt = Time::from_hms(h as u8, 0, 0).ok()?;
    Some(t.replace_time(nt))
}

fn at_minute(t: OffsetDateTime, m: u32) -> Option<OffsetDateTime> {
    let nt = Time::from_hms(t.hour(), m as u8, 0).ok()?;
    Some(t.replace_time(nt))
}

fn at_second(t: OffsetDateTime, s: u32) -> Option<OffsetDateTime> {
    let nt = Time::from_hms(t.hour(), t.minute(), s as u8).ok()?;
    Some(t.replace_time(nt))
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    #[test]
    fn fires_every_minute() {
        let spec = CalendarSpec::parse("*-*-* *:*:00").unwrap();
        let now = datetime!(2026-04-27 10:30:25 UTC);
        let next = next_fire(now, &spec, None).unwrap();
        // Next minute boundary: 10:31:00.
        assert_eq!(next.hour(), 10);
        assert_eq!(next.minute(), 31);
        assert_eq!(next.second(), 0);
    }

    #[test]
    fn fires_on_specific_hour() {
        let spec = CalendarSpec::parse("*-*-* 09:00:00").unwrap();
        let now = datetime!(2026-04-27 10:30:00 UTC);
        let next = next_fire(now, &spec, None).unwrap();
        // Tomorrow 09:00:00.
        assert_eq!(next.day(), 28);
        assert_eq!(next.hour(), 9);
    }

    #[test]
    fn weekday_constraint() {
        let spec = CalendarSpec::parse("Mon *-*-* 09:00:00").unwrap();
        // 2026-04-27 is a Monday.
        let now = datetime!(2026-04-27 10:00:00 UTC);
        let next = next_fire(now, &spec, None).unwrap();
        // Next Monday at 9 → 2026-05-04.
        assert_eq!(next.weekday().number_from_monday(), 1);
    }

    #[test]
    fn daily_shortcut() {
        let spec = CalendarSpec::parse("daily").unwrap();
        let now = datetime!(2026-04-27 12:00:00 UTC);
        let next = next_fire(now, &spec, None).unwrap();
        // Tomorrow 00:00:00.
        assert_eq!(next.hour(), 0);
        assert_eq!(next.day(), 28);
    }
}
