//! systemd `OnCalendar=` parsing.
//!
//! Implements a usable subset of `man systemd.time` calendar events:
//! shortcuts (`hourly`, `daily`, `weekly`, …), weekday prefixes, date and
//! time fields with `*` wildcards, `,` lists, `..` ranges, and `/N` steps.
//!
//! Next-fire computation lives in `systeml-runtime` to keep this crate a
//! pure parser.

use std::str::FromStr;

/// Per-field set of allowed values, after wildcard / list / range / step
/// expansion. `None` means wildcard (any).
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct FieldSet {
    /// `None` means wildcard. Otherwise a sorted, deduplicated list of values.
    pub values: Option<Vec<u32>>,
}

impl FieldSet {
    /// Wildcard.
    pub const fn any() -> Self {
        Self { values: None }
    }
    /// Single value.
    pub fn single(v: u32) -> Self {
        Self { values: Some(vec![v]) }
    }
    /// Whether this set matches `v`.
    #[must_use]
    pub fn matches(&self, v: u32) -> bool {
        match &self.values {
            None => true,
            Some(vs) => vs.binary_search(&v).is_ok(),
        }
    }
}

/// Parsed `OnCalendar=` value.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct CalendarSpec {
    /// Day-of-week mask, 1=Mon..7=Sun. Wildcard = any day.
    pub weekdays: FieldSet,
    /// Year (`*` allowed).
    pub year: FieldSet,
    /// Month 1..=12.
    pub month: FieldSet,
    /// Day-of-month 1..=31.
    pub day: FieldSet,
    /// Hour 0..=23.
    pub hour: FieldSet,
    /// Minute 0..=59.
    pub minute: FieldSet,
    /// Second 0..=59.
    pub second: FieldSet,
    /// Optional timezone string (e.g. `UTC`, `Europe/Berlin`).
    pub timezone: Option<String>,
    /// Original string for round-tripping.
    pub raw: String,
}

impl CalendarSpec {
    /// Parse a single `OnCalendar=` value.
    pub fn parse(input: &str) -> Result<Self, String> {
        let raw = input.trim().to_owned();
        let mut spec = Self {
            weekdays: FieldSet::any(),
            year: FieldSet::any(),
            month: FieldSet::any(),
            day: FieldSet::any(),
            hour: FieldSet::any(),
            minute: FieldSet::any(),
            second: FieldSet::any(),
            timezone: None,
            raw: raw.clone(),
        };

        // Shortcuts.
        if let Some(s) = expand_shortcut(&raw) {
            return Self::parse(&s);
        }

        // Tokenise on whitespace.
        let mut parts: Vec<&str> = raw.split_whitespace().collect();

        // Optional trailing timezone (anything that looks like a tz: contains '/'
        // or known names — but to keep it simple, treat the *last* token that
        // contains no digit and no `:` as a tz.
        if let Some(last) = parts.last() {
            if !last.contains(':')
                && !last.contains('-')
                && !last.contains('*')
                && !last.contains(',')
                && (last.contains('/') || is_likely_tz(last))
            {
                spec.timezone = Some((*last).to_owned());
                parts.pop();
            }
        }

        // Optional weekday prefix.
        let mut idx = 0;
        if !parts.is_empty() && is_weekday_field(parts[0]) {
            spec.weekdays = parse_weekdays(parts[0])?;
            idx += 1;
        }

        // Date and time, in that order.
        let mut have_date = false;
        let mut have_time = false;

        while idx < parts.len() {
            let tok = parts[idx];
            if tok.contains(':') && !have_time {
                let (h, m, s) = parse_time(tok)?;
                spec.hour = h;
                spec.minute = m;
                spec.second = s;
                have_time = true;
            } else if (tok.contains('-') || tok == "*") && !have_date {
                let (y, m, d) = parse_date(tok)?;
                spec.year = y;
                spec.month = m;
                spec.day = d;
                have_date = true;
            } else {
                return Err(format!("unexpected calendar token {tok:?}"));
            }
            idx += 1;
        }

        // If no time given and a date was, default to 00:00:00.
        if !have_time && have_date {
            spec.hour = FieldSet::single(0);
            spec.minute = FieldSet::single(0);
            spec.second = FieldSet::single(0);
        }
        // If no date given and time was, default to any year/month/day.
        // (Already wildcards.)

        // If only a weekday, default to 00:00:00.
        if !have_time && !have_date && parts.len() <= 1 && idx == 1 {
            spec.hour = FieldSet::single(0);
            spec.minute = FieldSet::single(0);
            spec.second = FieldSet::single(0);
        }

        Ok(spec)
    }
}

impl FromStr for CalendarSpec {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

fn is_likely_tz(s: &str) -> bool {
    matches!(
        s,
        "UTC" | "GMT" | "PST" | "PDT" | "EST" | "EDT" | "CST" | "CDT" | "MST" | "MDT"
    ) || s.contains('/')
}

fn is_weekday_field(s: &str) -> bool {
    let head: String = s
        .chars()
        .take_while(|c| c.is_ascii_alphabetic())
        .collect();
    matches!(
        head.as_str(),
        "Mon" | "Tue" | "Wed" | "Thu" | "Fri" | "Sat" | "Sun"
    )
}

fn weekday_num(s: &str) -> Result<u32, String> {
    Ok(match s {
        "Mon" => 1,
        "Tue" => 2,
        "Wed" => 3,
        "Thu" => 4,
        "Fri" => 5,
        "Sat" => 6,
        "Sun" => 7,
        _ => return Err(format!("unknown weekday {s:?}")),
    })
}

fn parse_weekdays(s: &str) -> Result<FieldSet, String> {
    let mut out = Vec::new();
    for chunk in s.split(',') {
        if let Some((a, b)) = chunk.split_once("..") {
            let a = weekday_num(a)?;
            let b = weekday_num(b)?;
            if a <= b {
                for n in a..=b {
                    out.push(n);
                }
            } else {
                // wrap
                for n in a..=7 {
                    out.push(n);
                }
                for n in 1..=b {
                    out.push(n);
                }
            }
        } else {
            out.push(weekday_num(chunk)?);
        }
    }
    out.sort_unstable();
    out.dedup();
    Ok(FieldSet { values: Some(out) })
}

fn parse_date(s: &str) -> Result<(FieldSet, FieldSet, FieldSet), String> {
    if s == "*" {
        return Ok((FieldSet::any(), FieldSet::any(), FieldSet::any()));
    }
    let parts: Vec<&str> = s.split('-').collect();
    let (y, m, d) = match parts.len() {
        3 => (parts[0], parts[1], parts[2]),
        2 => ("*", parts[0], parts[1]),
        _ => return Err(format!("invalid date {s:?}")),
    };
    Ok((
        parse_field(y, 1970, 2199)?,
        parse_field(m, 1, 12)?,
        parse_field(d, 1, 31)?,
    ))
}

fn parse_time(s: &str) -> Result<(FieldSet, FieldSet, FieldSet), String> {
    let parts: Vec<&str> = s.split(':').collect();
    let (h, m, sec) = match parts.len() {
        3 => (parts[0], parts[1], parts[2]),
        2 => (parts[0], parts[1], "0"),
        _ => return Err(format!("invalid time {s:?}")),
    };
    Ok((
        parse_field(h, 0, 23)?,
        parse_field(m, 0, 59)?,
        parse_field(sec, 0, 59)?,
    ))
}

fn parse_field(s: &str, lo: u32, hi: u32) -> Result<FieldSet, String> {
    // `*`, `*/N`, `N`, `N,M,O`, `A..B`, `A..B/N`, combinations via `,`.
    if s == "*" {
        return Ok(FieldSet::any());
    }

    let mut values: Vec<u32> = Vec::new();
    for chunk in s.split(',') {
        let (range_part, step) = if let Some((r, st)) = chunk.split_once('/') {
            (r, st.parse::<u32>().map_err(|e| e.to_string())?)
        } else {
            (chunk, 1u32)
        };
        let (from, to) = if range_part == "*" {
            (lo, hi)
        } else if let Some((a, b)) = range_part.split_once("..") {
            (a.parse::<u32>().map_err(|e| e.to_string())?, b.parse::<u32>().map_err(|e| e.to_string())?)
        } else {
            let v: u32 = range_part.parse().map_err(|e: std::num::ParseIntError| e.to_string())?;
            (v, v)
        };
        if from > to || from < lo || to > hi {
            return Err(format!("range {from}..{to} out of [{lo},{hi}]"));
        }
        let mut v = from;
        while v <= to {
            values.push(v);
            if step == 0 {
                return Err("step zero".into());
            }
            v += step;
        }
    }
    values.sort_unstable();
    values.dedup();
    Ok(FieldSet { values: Some(values) })
}

fn expand_shortcut(s: &str) -> Option<String> {
    Some(
        match s {
            "minutely" => "*-*-* *:*:00",
            "hourly" => "*-*-* *:00:00",
            "daily" => "*-*-* 00:00:00",
            "weekly" => "Mon *-*-* 00:00:00",
            "monthly" => "*-*-01 00:00:00",
            "quarterly" => "*-01,04,07,10-01 00:00:00",
            "semiannually" | "semi-annually" => "*-01,07-01 00:00:00",
            "yearly" | "annually" => "*-01-01 00:00:00",
            _ => return None,
        }
        .to_owned(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shortcut_daily() {
        let s = CalendarSpec::parse("daily").unwrap();
        assert_eq!(s.hour, FieldSet::single(0));
        assert_eq!(s.minute, FieldSet::single(0));
        assert_eq!(s.second, FieldSet::single(0));
    }

    #[test]
    fn time_only() {
        let s = CalendarSpec::parse("12:30:00").unwrap();
        assert_eq!(s.hour, FieldSet::single(12));
        assert_eq!(s.minute, FieldSet::single(30));
    }

    #[test]
    fn weekday_range_with_time() {
        let s = CalendarSpec::parse("Mon..Fri 09:00:00").unwrap();
        assert_eq!(s.weekdays.values.as_deref(), Some(&[1u32, 2, 3, 4, 5][..]));
        assert_eq!(s.hour, FieldSet::single(9));
    }

    #[test]
    fn step_minutes() {
        let s = CalendarSpec::parse("*-*-* *:*/15:00").unwrap();
        assert_eq!(s.minute.values.as_deref(), Some(&[0u32, 15, 30, 45][..]));
    }

    #[test]
    fn list() {
        let s = CalendarSpec::parse("*-*-* 00,06,12,18:00:00").unwrap();
        assert_eq!(s.hour.values.as_deref(), Some(&[0u32, 6, 12, 18][..]));
    }
}
