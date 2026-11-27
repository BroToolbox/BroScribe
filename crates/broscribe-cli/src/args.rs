//! Parsers for command-line values: time bounds, ages, clock times, ranges.

use chrono::{DateTime, Duration, FixedOffset, Local, NaiveDate, NaiveDateTime, TimeZone};

use broscribe_core::{Error, Result};

/// A time bound resolved at the current time ([`parse_time_bound_at`]).
pub(crate) fn parse_time_bound(s: &str, end_of_day: bool) -> Result<DateTime<FixedOffset>> {
    parse_time_bound_at(s, end_of_day, Local::now().fixed_offset())
}

/// An age (`12h`, `30d`, `2w`): `queue prune --older-than`.
pub(crate) fn parse_age(s: &str) -> Result<std::time::Duration> {
    let s = s.trim();
    relative_age(s)
        .flatten()
        .and_then(|d| d.to_std().ok())
        .ok_or_else(|| Error::InvalidInput(format!("not an age (expected 12h, 30d or 2w): {s:?}")))
}

/// `<n>h`, `<n>d` or `<n>w`, `n >= 0`: `None` when `s` is not a number
/// followed by one letter, `Some(None)` when it is one but no valid age.
fn relative_age(s: &str) -> Option<Option<Duration>> {
    let unit = s.chars().last().filter(char::is_ascii_alphabetic)?;
    let n = s[..s.len() - 1].parse::<i64>().ok()?;
    let age = match unit {
        'h' => Duration::try_hours(n),
        'd' => Duration::try_days(n),
        'w' => Duration::try_weeks(n),
        _ => None,
    };
    Some(age.filter(|_| n >= 0))
}

/// A `--since` / `--until` bound: a relative age (`30d`, `12h`, `2w`)
/// counted back from `now`, a date (`YYYY-MM-DD`, local midnight; with
/// `end_of_day` the next midnight, so the day is included),
/// `YYYY-MM-DD HH:MM` (local) or RFC 3339. Paged reads pass their first
/// page's anchor as `now` (`query::resolve_at`).
pub(crate) fn parse_time_bound_at(
    s: &str,
    end_of_day: bool,
    now: DateTime<FixedOffset>,
) -> Result<DateTime<FixedOffset>> {
    let s = s.trim();
    let invalid = || {
        Error::InvalidInput(format!(
            "not a time (expected 30d, 12h, 2w, YYYY-MM-DD or RFC 3339): {s:?}"
        ))
    };
    if let Some(age) = relative_age(s) {
        return age
            .and_then(|d| now.checked_sub_signed(d))
            .ok_or_else(invalid);
    }
    if let Ok(t) = DateTime::parse_from_rfc3339(s) {
        return Ok(t);
    }
    if let Ok(date) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        let date = if end_of_day {
            date.succ_opt().ok_or_else(invalid)?
        } else {
            date
        };
        return local(date.and_hms_opt(0, 0, 0).ok_or_else(invalid)?).ok_or_else(invalid);
    }
    for fmt in [
        "%Y-%m-%d %H:%M",
        "%Y-%m-%dT%H:%M",
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%dT%H:%M:%S",
    ] {
        if let Ok(t) = NaiveDateTime::parse_from_str(s, fmt) {
            return local(t).ok_or_else(invalid);
        }
    }
    Err(invalid())
}

/// A meeting start for `import --date`: like [`parse_time_bound`] without
/// relative ages.
pub(crate) fn parse_start(s: &str) -> Result<DateTime<FixedOffset>> {
    if s.trim().ends_with(|c: char| c.is_ascii_alphabetic()) && !s.contains('-') {
        return Err(Error::InvalidInput(format!(
            "not a date (expected YYYY-MM-DD, \"YYYY-MM-DD HH:MM\" or RFC 3339): {s:?}"
        )));
    }
    parse_time_bound(s, false)
}

fn local(t: NaiveDateTime) -> Option<DateTime<FixedOffset>> {
    Local
        .from_local_datetime(&t)
        .earliest()
        .map(|d| d.fixed_offset())
}

/// Seconds from `hh:mm:ss`, `mm:ss` or plain seconds (fractions allowed).
pub(crate) fn parse_clock(s: &str) -> Result<f64> {
    let invalid = || Error::InvalidInput(format!("not a time (expected hh:mm:ss): {s:?}"));
    let parts: Vec<&str> = s.trim().split(':').collect();
    if parts.len() > 3 || parts.iter().any(|p| p.is_empty()) {
        return Err(invalid());
    }
    let mut total = 0.0;
    for (i, p) in parts.iter().enumerate() {
        let v: f64 = p.parse().map_err(|_| invalid())?;
        if !v.is_finite() || v < 0.0 || (i > 0 && v >= 60.0) {
            return Err(invalid());
        }
        total = total * 60.0 + v;
    }
    Ok(total)
}

/// `start-end` seconds, `start < end`.
pub(crate) fn parse_range(s: &str) -> Result<(f64, f64)> {
    let invalid = || {
        Error::InvalidInput(format!(
            "not a range (expected start-end in seconds, e.g. 12.5-40): {s:?}"
        ))
    };
    let (a, b) = s.trim().split_once('-').ok_or_else(invalid)?;
    let a = parse_clock(a).map_err(|_| invalid())?;
    let b = parse_clock(b).map_err(|_| invalid())?;
    if a >= b {
        return Err(invalid());
    }
    Ok((a, b))
}

/// `+tag`/`tag` adds, `-tag` removes.
pub(crate) fn split_tag_changes(changes: &[String]) -> (Vec<String>, Vec<String>) {
    let mut add = Vec::new();
    let mut remove = Vec::new();
    for c in changes {
        if let Some(t) = c.strip_prefix('-') {
            remove.push(t.to_owned());
        } else {
            add.push(c.strip_prefix('+').unwrap_or(c).to_owned());
        }
    }
    (add, remove)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_bounds() {
        let now = DateTime::parse_from_rfc3339("2026-11-14T12:00:00Z").unwrap();
        assert_eq!(
            parse_time_bound_at("30d", false, now).unwrap(),
            DateTime::parse_from_rfc3339("2026-10-15T12:00:00Z").unwrap()
        );
        assert_eq!(
            parse_time_bound_at("2w", false, now).unwrap(),
            DateTime::parse_from_rfc3339("2026-10-31T12:00:00Z").unwrap()
        );
        assert_eq!(
            parse_time_bound_at("2026-11-10T09:00:00+02:00", false, now).unwrap(),
            DateTime::parse_from_rfc3339("2026-11-10T07:00:00Z").unwrap()
        );
        let start = parse_time_bound_at("2026-11-10", false, now).unwrap();
        let end = parse_time_bound_at("2026-11-10", true, now).unwrap();
        assert!(end > start);
        assert!(parse_time_bound_at("soon", false, now).is_err());
        assert!(parse_time_bound_at("3y", false, now).is_err());
    }

    #[test]
    fn clock_and_range() {
        assert_eq!(parse_clock("01:02:03").unwrap(), 3723.0);
        assert_eq!(parse_clock("10:00").unwrap(), 600.0);
        assert_eq!(parse_clock("42.5").unwrap(), 42.5);
        assert!(parse_clock("1:61").is_err());
        assert_eq!(parse_range("12.5-40").unwrap(), (12.5, 40.0));
        assert!(parse_range("40-12").is_err());
    }

    #[test]
    fn ages() {
        use std::time::Duration as StdDuration;
        assert_eq!(parse_age("12h").unwrap(), StdDuration::from_secs(12 * 3600));
        assert_eq!(
            parse_age(" 2w ").unwrap(),
            StdDuration::from_secs(14 * 86_400)
        );
        assert_eq!(parse_age("0d").unwrap(), StdDuration::ZERO);
        for bad in ["-1d", "3y", "2026-11-10", "d", "", "30"] {
            assert!(parse_age(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn tag_changes() {
        let (add, remove) = split_tag_changes(&["+a".to_owned(), "b".to_owned(), "-c".to_owned()]);
        assert_eq!(add, ["a", "b"]);
        assert_eq!(remove, ["c"]);
    }
}
