//! systemd-style duration parsing.
//!
//! Accepts the forms in `man systemd.time` "PARSING TIME SPANS":
//! - `5s`, `30sec`, `30seconds`
//! - `5m`, `5min`, `5minutes`
//! - `2h`, `3hr`, `3hours`
//! - `7d`, `2w` (weeks), `1M` (months ≈ 30.44 d), `1y` (≈ 365.25 d)
//! - composite: `1h 30m 5s`, `1h30m5s`
//! - `infinity`
//! - bare integer means seconds (when default unit is seconds, the typical
//!   case for `*Sec=` directives)

use std::time::Duration;

/// Parsed systemd duration. Wraps `std::time::Duration` plus an "infinity"
/// sentinel matching `RestartSec=infinity` etc.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum SdDuration {
    /// Finite duration.
    Finite(Duration),
    /// `infinity`.
    Infinity,
}

impl Default for SdDuration {
    fn default() -> Self {
        Self::ZERO
    }
}

impl SdDuration {
    /// Zero.
    pub const ZERO: Self = Self::Finite(Duration::ZERO);

    /// As `std::time::Duration`. `Infinity` becomes `Duration::MAX`.
    #[must_use]
    pub fn as_std(self) -> Duration {
        match self {
            Self::Finite(d) => d,
            Self::Infinity => Duration::MAX,
        }
    }

    /// Whether this is finite.
    #[must_use]
    pub fn is_finite(self) -> bool {
        matches!(self, Self::Finite(_))
    }
}

impl From<Duration> for SdDuration {
    fn from(d: Duration) -> Self {
        Self::Finite(d)
    }
}

/// Parse a systemd-style duration string. `default_unit_secs` is the unit
/// applied to bare numeric values (typically 1 for `*Sec=` directives).
///
/// # Errors
/// Returns the offending substring if parsing fails.
pub fn parse_duration(input: &str, default_unit_secs: u64) -> Result<SdDuration, String> {
    let s = input.trim();
    if s.eq_ignore_ascii_case("infinity") {
        return Ok(SdDuration::Infinity);
    }
    if s.is_empty() {
        return Err("empty duration".into());
    }

    // Bare integer? Apply default unit.
    if s.bytes().all(|b| b.is_ascii_digit()) {
        let n: u64 = s.parse().map_err(|e: std::num::ParseIntError| e.to_string())?;
        return Ok(SdDuration::Finite(Duration::from_secs(
            n.saturating_mul(default_unit_secs),
        )));
    }

    // Composite: alternating number / unit pairs.
    let mut total = Duration::ZERO;
    let mut chars = s.chars().peekable();

    while let Some(&c) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
            continue;
        }
        // Read number (with optional decimal point).
        let mut num = String::new();
        while let Some(&c) = chars.peek() {
            if c.is_ascii_digit() || c == '.' {
                num.push(c);
                chars.next();
            } else {
                break;
            }
        }
        if num.is_empty() {
            return Err(format!("expected digit, got {c:?}"));
        }
        let n: f64 = num.parse().map_err(|_| format!("bad number {num:?}"))?;

        // Skip whitespace before unit.
        while let Some(&c) = chars.peek() {
            if c.is_whitespace() {
                chars.next();
            } else {
                break;
            }
        }

        // Read unit letters.
        let mut unit = String::new();
        while let Some(&c) = chars.peek() {
            if c.is_ascii_alphabetic() {
                unit.push(c);
                chars.next();
            } else {
                break;
            }
        }

        let unit_secs = if unit.is_empty() {
            default_unit_secs as f64
        } else {
            unit_to_seconds(&unit).ok_or_else(|| format!("unknown unit {unit:?}"))?
        };
        let added = n * unit_secs;
        if !added.is_finite() || added < 0.0 {
            return Err("non-finite or negative".into());
        }
        let secs = added as u64;
        let nanos = ((added - secs as f64) * 1_000_000_000.0) as u32;
        total = total
            .checked_add(Duration::new(secs, nanos))
            .ok_or_else(|| "overflow".to_string())?;
    }

    Ok(SdDuration::Finite(total))
}

fn unit_to_seconds(u: &str) -> Option<f64> {
    let lower = u.to_ascii_lowercase();
    Some(match lower.as_str() {
        "ns" | "nsec" => 1e-9,
        "us" | "µs" | "usec" => 1e-6,
        "ms" | "msec" => 1e-3,
        "s" | "sec" | "second" | "seconds" => 1.0,
        "m" | "min" | "minute" | "minutes" => 60.0,
        "h" | "hr" | "hour" | "hours" => 3600.0,
        "d" | "day" | "days" => 86_400.0,
        "w" | "week" | "weeks" => 7.0 * 86_400.0,
        // "M" exact case for months per systemd. We accept lowercased "month" too.
        "month" | "months" => 30.44 * 86_400.0,
        "y" | "year" | "years" => 365.25 * 86_400.0,
        _ => {
            // case-sensitive distinction: M (month) vs m (minute) only matters
            // when the original was exactly "M".
            if u == "M" {
                30.44 * 86_400.0
            } else {
                return None;
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_integer_uses_default() {
        assert_eq!(
            parse_duration("30", 1).unwrap(),
            SdDuration::Finite(Duration::from_secs(30))
        );
    }

    #[test]
    fn simple_units() {
        assert_eq!(
            parse_duration("5s", 1).unwrap().as_std(),
            Duration::from_secs(5)
        );
        assert_eq!(
            parse_duration("5min", 1).unwrap().as_std(),
            Duration::from_secs(300)
        );
        assert_eq!(
            parse_duration("2h", 1).unwrap().as_std(),
            Duration::from_secs(7200)
        );
    }

    #[test]
    fn composite() {
        assert_eq!(
            parse_duration("1h 30min 15s", 1).unwrap().as_std(),
            Duration::from_secs(3600 + 1800 + 15)
        );
        assert_eq!(
            parse_duration("1h30min15s", 1).unwrap().as_std(),
            Duration::from_secs(3600 + 1800 + 15)
        );
    }

    #[test]
    fn fractional() {
        assert_eq!(
            parse_duration("0.5s", 1).unwrap().as_std(),
            Duration::from_millis(500)
        );
    }

    #[test]
    fn infinity() {
        assert_eq!(parse_duration("infinity", 1).unwrap(), SdDuration::Infinity);
        assert_eq!(parse_duration("Infinity", 1).unwrap(), SdDuration::Infinity);
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse_duration("", 1).is_err());
        assert!(parse_duration("5xyz", 1).is_err());
    }
}
