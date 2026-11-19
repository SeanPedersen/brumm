//! Local-time formatting for message timestamps, chat-list times and day separators.

use jiff::{Timestamp, civil::Date, tz::TimeZone};

fn local_date(unix_ms: u64) -> Option<(Date, jiff::ZonedDateTime)> {
    let zoned = Timestamp::from_millisecond(unix_ms as i64)
        .ok()?
        .to_zoned_datetime(TimeZone::system());
    Some((zoned.date(), zoned))
}

/// `14:05`
pub fn clock(unix_ms: u64) -> String {
    local_date(unix_ms)
        .map(|(_, zoned)| zoned.strftime("%H:%M").to_string())
        .unwrap_or_default()
}

/// Day identifier used to detect where a new day starts in a conversation.
pub fn day_of(unix_ms: u64) -> Option<Date> {
    local_date(unix_ms).map(|(date, _)| date)
}

/// "Today", "Yesterday" or "March 4" (with the year when it is not the current one).
pub fn day_label(date: Date) -> String {
    let today = Timestamp::now()
        .to_zoned_datetime(TimeZone::system())
        .date();
    if date == today {
        return "Today".to_owned();
    }
    if today.yesterday().ok() == Some(date) {
        return "Yesterday".to_owned();
    }
    let format = if date.year() == today.year() {
        "%B %-d"
    } else {
        "%B %-d, %Y"
    };
    date.strftime(format).to_string()
}

/// Chat-list timestamp: the clock for today, otherwise a short date.
pub fn list_stamp(unix_ms: u64) -> String {
    let Some((date, zoned)) = local_date(unix_ms) else {
        return String::new();
    };
    let today = Timestamp::now()
        .to_zoned_datetime(TimeZone::system())
        .date();
    if date == today {
        return zoned.strftime("%H:%M").to_string();
    }
    if date.year() == today.year() {
        zoned.strftime("%b %-d").to_string()
    } else {
        zoned.strftime("%d.%m.%y").to_string()
    }
}
