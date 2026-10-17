//! Just enough of the proleptic Gregorian calendar, in UTC, to resolve date literals.

use std::time::{SystemTime, UNIX_EPOCH};

const DAY_MS: i64 = 86_400_000;

/// Milliseconds since the Unix epoch, now.
pub fn now_ms() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(since) => since.as_millis() as i64,
        Err(before) => -(before.duration().as_millis() as i64),
    }
}

/// Days since the Unix epoch of a civil date. The date need not be valid: an
/// out-of-range day runs on into the following month.
pub fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year.rem_euclid(400);
    let month = month as i64;
    let day_of_year = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day as i64 - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// The civil date of a number of days since the Unix epoch.
pub fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let days = days + 719_468;
    let era = days.div_euclid(146_097);
    let day_of_era = days.rem_euclid(146_097);
    let year_of_era = (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * shifted + 2) / 5 + 1) as u32;
    let month = if shifted < 10 { shifted + 3 } else { shifted - 9 } as u32;
    let year = year_of_era + era * 400 + if month <= 2 { 1 } else { 0 };
    (year, month, day)
}

/// `YYYY-MM-DD` for a number of days since the Unix epoch.
pub fn format_day(days: i64) -> String {
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}")
}

/// `YYYY-MM-DDTHH:MM:SS.mmmZ` for milliseconds since the Unix epoch.
pub fn format_instant(ms: i64) -> String {
    let of_day = ms.rem_euclid(DAY_MS);
    let (hours, minutes) = (of_day / 3_600_000, of_day / 60_000 % 60);
    let (seconds, millis) = (of_day / 1_000 % 60, of_day % 1_000);
    format!("{}T{hours:02}:{minutes:02}:{seconds:02}.{millis:03}Z", format_day(ms.div_euclid(DAY_MS)))
}

/// Days since the Unix epoch of the UTC day an instant falls on.
pub fn day_of(ms: i64) -> i64 {
    ms.div_euclid(DAY_MS)
}

/// Milliseconds since the Unix epoch of UTC midnight on a civil date.
pub fn utc_ms(year: i64, month: u32, day: u32) -> i64 {
    days_from_civil(year, month, day) * DAY_MS
}

/// Milliseconds since the Unix epoch of an ISO date (`YYYY-MM-DD`) or UTC timestamp
/// (`YYYY-MM-DDTHH:MM:SS[.fraction]Z`). Anything finer than a millisecond is dropped.
pub fn parse_iso_ms(text: &str) -> Option<i64> {
    let number = |range: std::ops::Range<usize>| text.get(range)?.parse::<i64>().ok();
    let day = days_from_civil(number(0..4)?, number(5..7)? as u32, number(8..10)? as u32);
    if text.len() == 10 {
        return Some(day * DAY_MS);
    }
    let clock = number(11..13)? * 3_600_000 + number(14..16)? * 60_000 + number(17..19)? * 1_000;
    let fraction = text.get(19..)?.trim_end_matches('Z').trim_start_matches('.');
    let digits: String = fraction.chars().chain("000".chars()).take(3).collect();
    Some(day * DAY_MS + clock + digits.parse::<i64>().ok()?)
}
