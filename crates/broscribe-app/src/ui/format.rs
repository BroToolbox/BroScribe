//! Local dates, playback positions and durations for the desktop UI.

use chrono::{DateTime, Datelike, FixedOffset, Local, NaiveDate};

pub use broscribe_core::display_units::{bytes, estimate, length};

pub(crate) fn spoken_local_date(at: &DateTime<FixedOffset>) -> String {
    spoken_day(
        at.with_timezone(&Local).date_naive(),
        Local::now().date_naive(),
    )
}

pub fn local_date(at: &DateTime<FixedOffset>) -> String {
    date(
        at.with_timezone(&Local).date_naive(),
        Local::now().date_naive(),
    )
}

pub fn local_date_time(at: &DateTime<FixedOffset>) -> String {
    date_time(&at.with_timezone(&Local), Local::now().date_naive())
}

pub fn local_time(at: &DateTime<FixedOffset>) -> String {
    at.with_timezone(&Local).format("%H:%M").to_string()
}

pub(crate) fn spoken_date_time(at: &DateTime<FixedOffset>) -> String {
    let local = at.with_timezone(&Local);
    spoken_date_time_on(&local, Local::now().date_naive())
}

fn spoken_date_time_on<Tz: chrono::TimeZone>(at: &DateTime<Tz>, today: NaiveDate) -> String
where
    Tz::Offset: std::fmt::Display,
{
    format!(
        "{} at {}",
        spoken_day(at.date_naive(), today),
        at.format("%H:%M")
    )
}

fn spoken_day(day: NaiveDate, today: NaiveDate) -> String {
    if day == today || today.pred_opt() == Some(day) {
        date(day, today).to_ascii_lowercase()
    } else if day.year() == today.year() {
        day.format("%A %-d %B").to_string()
    } else {
        day.format("%A %-d %B %Y").to_string()
    }
}

fn date_time<Tz: chrono::TimeZone>(at: &DateTime<Tz>, today: NaiveDate) -> String
where
    Tz::Offset: std::fmt::Display,
{
    format!("{}, {}", date(at.date_naive(), today), at.format("%H:%M"))
}

fn date(day: NaiveDate, today: NaiveDate) -> String {
    if day == today {
        "Today".to_owned()
    } else if today.pred_opt() == Some(day) {
        "Yesterday".to_owned()
    } else if day.year() == today.year() {
        day.format("%a %-d %b").to_string()
    } else {
        day.format("%-d %b %Y").to_string()
    }
}

/// A due date is a calendar day, already in the user's local calendar.
pub(crate) fn due(day: NaiveDate, today: NaiveDate, overdue: bool) -> (String, String) {
    let (shown, spoken) = if today.succ_opt() == Some(day) {
        ("Tomorrow".to_owned(), "tomorrow".to_owned())
    } else {
        (date(day, today), spoken_day(day, today))
    };
    let suffix = if overdue { ", overdue" } else { "" };
    (shown, format!("Due {spoken}{suffix}"))
}

/// Group headings spell out the weekday and month.
pub(crate) fn date_heading(day: NaiveDate, today: NaiveDate) -> String {
    if day == today || today.pred_opt() == Some(day) {
        date(day, today)
    } else {
        spoken_day(day, today)
    }
}

pub(crate) fn spoken_estimate(seconds: f64) -> String {
    match broscribe_core::display_units::estimated_seconds(seconds) {
        0 => "under 1 minute".to_owned(),
        seconds => format!("about {}", spoken_duration(seconds)),
    }
}

pub(crate) fn spoken_length(seconds: f64) -> String {
    spoken_duration(broscribe_core::display_units::displayed_length_seconds(
        seconds,
    ))
}

/// Clock position: `m:ss` under one hour, `h:mm:ss` from one hour on (MTG-17).
pub fn format_clock(seconds: f64) -> String {
    let t = clock_seconds(seconds);
    if t >= 3600 {
        format!("{}:{:02}:{:02}", t / 3600, (t / 60) % 60, t % 60)
    } else {
        format!("{}:{:02}", t / 60, t % 60)
    }
}

fn clock_seconds(seconds: f64) -> u64 {
    if seconds.is_finite() {
        seconds.max(0.0) as u64
    } else {
        0
    }
}

pub(crate) fn spoken_position(seconds: f64) -> String {
    spoken_duration(clock_seconds(seconds))
}

pub(crate) fn plural<'a>(count: u64, one: &'a str, many: &'a str) -> &'a str {
    if count == 1 { one } else { many }
}

pub(crate) fn count(count: u64, one: &str, many: &str) -> String {
    format!("{count} {}", plural(count, one, many))
}

pub(crate) fn percent(value: u8) -> String {
    format!("{value}%")
}

/// A configured similarity threshold displayed as a percentage in Settings.
pub(crate) fn threshold_percent(fraction: f32) -> String {
    let number = format!("{:.2}", fraction * 100.0);
    format!("{}%", number.trim_end_matches('0').trim_end_matches('.'))
}

pub(crate) fn progress_percent(fraction: f64) -> Option<String> {
    progress_value(fraction).map(percent)
}

pub(crate) fn seconds(value: u64) -> String {
    format!("{value} s")
}

pub(crate) fn progress_value(fraction: f64) -> Option<u8> {
    fraction
        .is_finite()
        .then(|| (fraction.clamp(0., 1.) * 100.).round() as u8)
}

/// Language names are display data. The registry supplies model support.
pub(crate) const LANGUAGE_NAMES: &[(&str, &str)] = &[
    ("en", "English"),
    ("pt", "Português · Portuguese"),
    ("pt-BR", "Português (Brasil) · Portuguese (Brazil)"),
    ("pt-PT", "Português (Portugal) · Portuguese (Portugal)"),
    ("es", "Español · Spanish"),
    ("fr", "Français · French"),
    ("de", "Deutsch · German"),
    ("it", "Italiano · Italian"),
    ("nl", "Nederlands · Dutch"),
    ("ja", "日本語 · Japanese"),
    ("zh", "中文 · Chinese"),
    ("ko", "한국어 · Korean"),
    ("ar", "العربية · Arabic"),
    ("hi", "हिन्दी · Hindi"),
    ("ru", "Русский · Russian"),
    ("uk", "Українська · Ukrainian"),
    ("pl", "Polski · Polish"),
    ("sv", "Svenska · Swedish"),
    ("da", "Dansk · Danish"),
    ("nb", "Norsk bokmål · Norwegian Bokmål"),
    ("no", "Norsk · Norwegian"),
    ("fi", "Suomi · Finnish"),
    ("el", "Ελληνικά · Greek"),
    ("cs", "Čeština · Czech"),
    ("sk", "Slovenčina · Slovak"),
    ("sl", "Slovenščina · Slovenian"),
    ("hr", "Hrvatski · Croatian"),
    ("ro", "Română · Romanian"),
    ("bg", "Български · Bulgarian"),
    ("hu", "Magyar · Hungarian"),
    ("et", "Eesti · Estonian"),
    ("lv", "Latviešu · Latvian"),
    ("lt", "Lietuvių · Lithuanian"),
    ("mt", "Malti · Maltese"),
    ("tr", "Türkçe · Turkish"),
    ("vi", "Tiếng Việt · Vietnamese"),
    ("id", "Bahasa Indonesia · Indonesian"),
    ("th", "ไทย · Thai"),
    ("ca", "Català · Catalan"),
];

pub(crate) fn language_name(tag: &str) -> &'static str {
    if tag.is_empty() || tag.eq_ignore_ascii_case("auto") {
        return "Detect automatically";
    }
    let base = tag.split('-').next().unwrap_or(tag);
    let name = |code: &str| {
        LANGUAGE_NAMES
            .iter()
            .find(|(known, _)| known.eq_ignore_ascii_case(code))
            .map(|(_, name)| *name)
    };
    name(tag).or_else(|| name(base)).unwrap_or("Other language")
}

/// Spoken countdown value in seconds (`"1 second"`, `"8 seconds"`).
pub(crate) fn spoken_seconds(seconds: u64) -> String {
    format!("{seconds} {}", plural(seconds, "second", "seconds"))
}

/// Spoken elapsed duration in words (`"45 seconds"`, `"1 minute 23 seconds"`).
pub(crate) fn spoken_duration(seconds: u64) -> String {
    let hours = seconds / 3600;
    let minutes = (seconds / 60) % 60;
    let secs = seconds % 60;
    let mut parts = Vec::new();
    if hours > 0 {
        parts.push(format!("{hours} {}", plural(hours, "hour", "hours")));
    }
    if minutes > 0 {
        parts.push(format!(
            "{minutes} {}",
            plural(minutes, "minute", "minutes")
        ));
    }
    if secs > 0 || parts.is_empty() {
        parts.push(spoken_seconds(secs));
    }
    parts.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_dates_use_relative_days_and_include_a_different_year() {
        let today = NaiveDate::from_ymd_opt(2026, 11, 22).unwrap();
        let show = |iso| date_time(&DateTime::parse_from_rfc3339(iso).unwrap(), today);
        assert_eq!(show("2026-11-22T10:30:00+01:00"), "Today, 10:30");
        assert_eq!(show("2026-11-21T08:00:00+01:00"), "Yesterday, 08:00");
        assert_eq!(show("2026-11-19T08:37:00+01:00"), "Thu 19 Nov, 08:37");
        assert_eq!(show("2025-11-19T08:37:00+01:00"), "19 Nov 2025, 08:37");
        assert_eq!(spoken_length(427.), "7 minutes");
        assert_eq!(spoken_length(3900.), "1 hour 5 minutes");
    }

    #[test]
    fn due_dates_follow_the_calendar_and_speak_overdue() {
        let today = NaiveDate::from_ymd_opt(2026, 12, 31).unwrap();
        let due_on = |iso: &str, overdue| due(iso.parse().unwrap(), today, overdue);
        assert_eq!(
            due_on("2026-12-31", false),
            ("Today".into(), "Due today".into())
        );
        assert_eq!(
            due_on("2027-01-01", false),
            ("Tomorrow".into(), "Due tomorrow".into())
        );
        assert_eq!(
            due_on("2027-01-02", false),
            ("2 Jan 2027".into(), "Due Saturday 2 January 2027".into())
        );
        assert_eq!(
            due_on("2026-12-29", true),
            (
                "Tue 29 Dec".into(),
                "Due Tuesday 29 December, overdue".into()
            )
        );
    }

    #[test]
    fn spoken_positions_match_the_clock_precision() {
        assert_eq!(spoken_position(3.9), "3 seconds");
        assert_eq!(spoken_position(61.9), "1 minute 1 second");
        assert_eq!(spoken_position(3723.), "1 hour 2 minutes 3 seconds");
        assert_eq!(spoken_position(f64::INFINITY), "0 seconds");
        assert_eq!(spoken_position(-1.), "0 seconds");
    }

    #[test]
    fn spoken_dates_name_the_day_and_time() {
        let today = NaiveDate::from_ymd_opt(2026, 11, 22).unwrap();
        let speak = |iso| spoken_date_time_on(&DateTime::parse_from_rfc3339(iso).unwrap(), today);
        assert_eq!(speak("2026-11-22T10:30:00+01:00"), "today at 10:30");
        assert_eq!(speak("2026-11-21T08:00:00+01:00"), "yesterday at 08:00");
        assert_eq!(
            speak("2026-11-19T08:37:00+01:00"),
            "Thursday 19 November at 08:37"
        );
        assert_eq!(
            speak("2025-11-19T08:37:00+01:00"),
            "Wednesday 19 November 2025 at 08:37"
        );
    }

    #[test]
    fn counts_and_progress_follow_the_desktop_copy() {
        assert_eq!(plural(0, "event", "events"), "events");
        assert_eq!(plural(1, "event", "events"), "event");
        assert_eq!(plural(2, "event", "events"), "events");
        assert_eq!(progress_percent(0.425).as_deref(), Some("43%"));
        assert_eq!(progress_percent(1.2).as_deref(), Some("100%"));
        assert_eq!(progress_percent(f64::NAN), None);
    }
}
