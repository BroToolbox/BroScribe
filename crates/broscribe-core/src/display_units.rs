//! Compact units for user-facing display. Machine output has its own format.

/// Match the displayed precision when naming the same duration for a screen reader.
pub fn displayed_length_seconds(seconds: f64) -> u64 {
    let seconds = if seconds.is_finite() {
        seconds.max(0.) as u64
    } else {
        0
    };
    if seconds < 60 {
        seconds
    } else {
        (seconds.saturating_add(30) / 60) * 60
    }
}

/// Compact duration in seconds, rounded minutes or hours and minutes.
pub fn length(seconds: f64) -> String {
    let seconds = displayed_length_seconds(seconds);
    let minutes = seconds / 60;
    match minutes {
        0 => format!("{seconds} s"),
        1..=59 => format!("{minutes} min"),
        _ if minutes.is_multiple_of(60) => format!("{} h", minutes / 60),
        _ => format!("{} h {:02} min", minutes / 60, minutes % 60),
    }
}

/// The recording time we can conservatively estimate, rounded down to a minute.
pub fn estimated_seconds(seconds: f64) -> u64 {
    if seconds.is_finite() {
        (seconds as u64 / 60) * 60
    } else {
        0
    }
}

pub fn estimate(seconds: f64) -> String {
    match estimated_seconds(seconds) {
        0 => "under 1 min".to_owned(),
        seconds => format!("about {}", length(seconds as f64)),
    }
}

/// Decimal storage units, with one decimal below 10 in the selected unit.
pub fn bytes(bytes: u64) -> String {
    for (unit, divisor) in [
        ("TB", 1_000_000_000_000),
        ("GB", 1_000_000_000),
        ("MB", 1_000_000),
        ("KB", 1_000),
    ] {
        if bytes >= divisor {
            let value = bytes as f64 / divisor as f64;
            return if value < 9.95 {
                format!("{value:.1} {unit}")
            } else {
                format!("{value:.0} {unit}")
            };
        }
    }
    format!("{bytes} B")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn storage_uses_decimal_units_and_rounds_large_labels() {
        assert_eq!(bytes(999), "999 B");
        assert_eq!(bytes(1_500), "1.5 KB");
        assert_eq!(bytes(2_100_000), "2.1 MB");
        assert_eq!(bytes(512_000_000), "512 MB");
        assert_eq!(bytes(1_200_000_000), "1.2 GB");
        assert_eq!(bytes(1_000_000_000_000), "1.0 TB");
    }

    #[test]
    fn estimates_never_round_available_recording_time_up() {
        assert_eq!(estimate(59.9), "under 1 min");
        assert_eq!(estimate(3599.), "about 59 min");
        assert_eq!(estimate(3659.), "about 1 h");
        assert_eq!(estimate(f64::NAN), "under 1 min");
        assert_eq!(bytes(12_600_000), "13 MB");
    }

    #[test]
    fn length_uses_the_displayed_unit_and_rounds_minutes() {
        assert_eq!(length(45.), "45 s");
        assert_eq!(length(60.), "1 min");
        assert_eq!(length(427.), "7 min");
        assert_eq!(length(3900.), "1 h 05 min");
        assert_eq!(length(7200.), "2 h");
        assert_eq!(length(f64::NAN), "0 s");
        assert_eq!(length(-1.), "0 s");
    }
}
