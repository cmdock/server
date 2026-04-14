//! Named date resolution for Taskwarrior filter expressions.
//!
//! Resolves named dates like "today", "tomorrow", "eow", "eom" to
//! UTC DateTimes.

use chrono::{DateTime, Datelike, Duration, NaiveDate, NaiveDateTime, NaiveTime, Utc, Weekday};

/// Try to resolve a named date string to a UTC DateTime.
/// Returns None if the string is not a recognised named date.
pub fn resolve_named_date(s: &str) -> Option<DateTime<Utc>> {
    resolve_named_date_at(s, Utc::now())
}

/// Resolve a named date relative to a given reference time.
/// Testable variant that avoids calling `Utc::now()`.
pub fn resolve_named_date_at(s: &str, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let today = now.date_naive();

    match s.to_lowercase().as_str() {
        "now" => Some(now),
        "today" | "sod" => Some(start_of_day(today)),
        "yesterday" => Some(start_of_day(today - Duration::days(1))),
        "tomorrow" => Some(start_of_day(today + Duration::days(1))),
        "eod" => Some(end_of_day(today)),

        // Week boundaries (Monday = start of week)
        "sow" => Some(start_of_day(start_of_week(today))),
        "eow" => Some(end_of_day(start_of_week(today) + Duration::days(6))),
        "sonw" => Some(start_of_day(start_of_week(today) + Duration::days(7))),
        "eonw" => Some(end_of_day(start_of_week(today) + Duration::days(13))),
        "sopw" => Some(start_of_day(start_of_week(today) - Duration::days(7))),
        "eopw" => Some(end_of_day(start_of_week(today) - Duration::days(1))),

        // Month boundaries
        "som" => Some(start_of_day(NaiveDate::from_ymd_opt(
            today.year(),
            today.month(),
            1,
        )?)),
        "eom" => Some(end_of_day(end_of_month(today)?)),
        "sonm" => {
            let next = next_month(today)?;
            Some(start_of_day(NaiveDate::from_ymd_opt(
                next.year(),
                next.month(),
                1,
            )?))
        }
        "eonm" => Some(end_of_day(end_of_month(next_month(today)?)?)),
        "sopm" => {
            let prev = prev_month(today)?;
            Some(start_of_day(NaiveDate::from_ymd_opt(
                prev.year(),
                prev.month(),
                1,
            )?))
        }
        "eopm" => Some(end_of_day(end_of_month(prev_month(today)?)?)),

        // Year boundaries
        "soy" => Some(start_of_day(NaiveDate::from_ymd_opt(today.year(), 1, 1)?)),
        "eoy" => Some(end_of_day(NaiveDate::from_ymd_opt(today.year(), 12, 31)?)),

        // Day names — next occurrence
        "monday" | "mon" => Some(start_of_day(next_weekday(today, Weekday::Mon))),
        "tuesday" | "tue" => Some(start_of_day(next_weekday(today, Weekday::Tue))),
        "wednesday" | "wed" => Some(start_of_day(next_weekday(today, Weekday::Wed))),
        "thursday" | "thu" => Some(start_of_day(next_weekday(today, Weekday::Thu))),
        "friday" | "fri" => Some(start_of_day(next_weekday(today, Weekday::Fri))),
        "saturday" | "sat" => Some(start_of_day(next_weekday(today, Weekday::Sat))),
        "sunday" | "sun" => Some(start_of_day(next_weekday(today, Weekday::Sun))),

        "later" | "someday" => Some(start_of_day(NaiveDate::from_ymd_opt(9999, 12, 30)?)),

        _ => None,
    }
}

/// Try to parse a date value — first as named date, then as TW format, then ISO.
pub fn parse_date_value(s: &str) -> Option<DateTime<Utc>> {
    // Named dates
    if let Some(dt) = resolve_named_date(s) {
        return Some(dt);
    }

    // TW format: YYYYMMDDTHHmmssZ
    if let Ok(naive) = NaiveDateTime::parse_from_str(s, "%Y%m%dT%H%M%SZ") {
        return Some(naive.and_utc());
    }

    // ISO format: YYYY-MM-DD
    if let Ok(date) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return Some(start_of_day(date));
    }

    // Relative duration: Nd (days), Nw (weeks), Nm (months), Ny (years)
    // e.g., "7d" = 7 days from now, "2w" = 14 days from now
    if let Some(dt) = parse_relative_duration(s) {
        return Some(dt);
    }

    // Epoch seconds
    if let Ok(secs) = s.parse::<i64>() {
        return DateTime::from_timestamp(secs, 0);
    }

    None
}

/// Parse a relative duration like "7d", "2w", "1m", "1y" into a DateTime.
/// Returns now + duration.
fn parse_relative_duration(s: &str) -> Option<DateTime<Utc>> {
    let s = s.trim();
    let (unit_idx, unit) = s.char_indices().last()?;
    if unit_idx == 0 {
        return None;
    }
    let num_str = &s[..unit_idx];
    let num: i64 = num_str.parse().ok()?;
    let now = Utc::now();
    let delta = match unit {
        's' => Duration::try_seconds(num),
        'd' => Duration::try_days(num),
        'w' => Duration::try_weeks(num),
        'm' => num.checked_mul(30).and_then(Duration::try_days), // approximate
        'y' => num.checked_mul(365).and_then(Duration::try_days), // approximate
        _ => None,
    }?;
    now.checked_add_signed(delta)
}

fn start_of_day(date: NaiveDate) -> DateTime<Utc> {
    date.and_time(NaiveTime::MIN).and_utc()
}

fn end_of_day(date: NaiveDate) -> DateTime<Utc> {
    date.and_hms_opt(23, 59, 59).unwrap().and_utc()
}

fn start_of_week(date: NaiveDate) -> NaiveDate {
    let days_since_monday = date.weekday().num_days_from_monday();
    date - Duration::days(days_since_monday as i64)
}

fn next_weekday(today: NaiveDate, target: Weekday) -> NaiveDate {
    let today_wd = today.weekday().num_days_from_monday();
    let target_wd = target.num_days_from_monday();
    let days_ahead = if target_wd > today_wd {
        target_wd - today_wd
    } else {
        7 - (today_wd - target_wd)
    };
    today + Duration::days(days_ahead as i64)
}

fn end_of_month(date: NaiveDate) -> Option<NaiveDate> {
    if date.month() == 12 {
        NaiveDate::from_ymd_opt(date.year() + 1, 1, 1).map(|d| d - Duration::days(1))
    } else {
        NaiveDate::from_ymd_opt(date.year(), date.month() + 1, 1).map(|d| d - Duration::days(1))
    }
}

fn next_month(date: NaiveDate) -> Option<NaiveDate> {
    if date.month() == 12 {
        NaiveDate::from_ymd_opt(date.year() + 1, 1, 1)
    } else {
        NaiveDate::from_ymd_opt(date.year(), date.month() + 1, 1)
    }
}

fn prev_month(date: NaiveDate) -> Option<NaiveDate> {
    if date.month() == 1 {
        NaiveDate::from_ymd_opt(date.year() - 1, 12, 1)
    } else {
        NaiveDate::from_ymd_opt(date.year(), date.month() - 1, 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fixed reference time for deterministic tests — avoids UTC midnight flakiness.
    fn fixed_now() -> DateTime<Utc> {
        // 2026-03-29 12:00:00 UTC (a Sunday)
        NaiveDate::from_ymd_opt(2026, 3, 29)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap()
            .and_utc()
    }

    #[test]
    fn test_resolve_today() {
        let now = fixed_now();
        let result = resolve_named_date_at("today", now).unwrap();
        assert_eq!(
            result.date_naive(),
            now.date_naive(),
            "today should resolve to the reference date"
        );
        // Should be midnight
        assert_eq!(result.time(), NaiveTime::MIN);
    }

    #[test]
    fn test_resolve_tomorrow() {
        let now = fixed_now();
        let result = resolve_named_date_at("tomorrow", now).unwrap();
        let expected = now.date_naive() + Duration::days(1);
        assert_eq!(
            result.date_naive(),
            expected,
            "tomorrow should resolve to reference date + 1"
        );
        assert_eq!(result.time(), NaiveTime::MIN);
    }

    #[test]
    fn test_resolve_yesterday() {
        let now = fixed_now();
        let result = resolve_named_date_at("yesterday", now).unwrap();
        let expected = now.date_naive() - Duration::days(1);
        assert_eq!(
            result.date_naive(),
            expected,
            "yesterday should resolve to reference date - 1"
        );
        assert_eq!(result.time(), NaiveTime::MIN);
    }

    #[test]
    fn test_parse_relative_duration_rejects_non_char_boundary_suffix() {
        assert_eq!(parse_date_value("\u{069f}"), None);
        assert_eq!(parse_date_value("1\u{069f}"), None);
    }

    #[test]
    fn test_parse_tw_datetime() {
        let result = parse_date_value("20260330T143000Z").unwrap();
        assert_eq!(result.year(), 2026);
        assert_eq!(result.month(), 3);
        assert_eq!(result.day(), 30);
        assert_eq!(result.hour(), 14);
        assert_eq!(result.minute(), 30);
        assert_eq!(result.second(), 0);
    }

    #[test]
    fn test_parse_iso_date() {
        let result = parse_date_value("2026-03-30").unwrap();
        assert_eq!(
            result.date_naive(),
            NaiveDate::from_ymd_opt(2026, 3, 30).unwrap()
        );
        // ISO date without time should resolve to midnight
        assert_eq!(result.time(), NaiveTime::MIN);
    }

    #[test]
    fn test_parse_epoch() {
        // 2024-03-30T16:00:00Z
        let result = parse_date_value("1711814400").unwrap();
        assert_eq!(
            result,
            DateTime::from_timestamp(1711814400, 0).unwrap(),
            "Epoch seconds should parse correctly"
        );
    }

    #[test]
    fn test_parse_invalid() {
        assert!(
            parse_date_value("not-a-date").is_none(),
            "Invalid string should return None"
        );
        assert!(
            parse_date_value("").is_none(),
            "Empty string should return None"
        );
        assert!(
            parse_date_value("abc123xyz").is_none(),
            "Gibberish should return None"
        );
    }

    #[test]
    fn test_eow_boundary() {
        let now = fixed_now();
        let result = resolve_named_date_at("eow", now).unwrap();
        let today = now.date_naive();
        let sow = start_of_week(today);
        let expected_eow = sow + Duration::days(6); // Sunday

        assert_eq!(
            result.date_naive(),
            expected_eow,
            "eow should be the Sunday of the current week"
        );
        // eow should be end of day (23:59:59)
        assert_eq!(result.hour(), 23);
        assert_eq!(result.minute(), 59);
        assert_eq!(result.second(), 59);
    }

    #[test]
    fn test_eom_boundary() {
        let now = fixed_now(); // March 29
        let result = resolve_named_date_at("eom", now).unwrap();
        assert_eq!(
            result.date_naive(),
            NaiveDate::from_ymd_opt(2026, 3, 31).unwrap(),
            "eom in March should be March 31"
        );
        assert_eq!(result.hour(), 23);
        assert_eq!(result.minute(), 59);
        assert_eq!(result.second(), 59);
    }

    #[test]
    fn test_resolve_named_date_case_insensitive() {
        assert!(resolve_named_date("TODAY").is_some());
        assert!(resolve_named_date("Today").is_some());
        assert!(resolve_named_date("TOMORROW").is_some());
    }

    #[test]
    fn test_resolve_unknown_returns_none() {
        assert!(resolve_named_date("notadate").is_none());
        assert!(resolve_named_date("").is_none());
    }

    #[test]
    fn test_start_of_week_is_monday() {
        // 2026-03-29 is a Sunday
        let sunday = NaiveDate::from_ymd_opt(2026, 3, 29).unwrap();
        let sow = start_of_week(sunday);
        assert_eq!(sow.weekday(), Weekday::Mon);
        assert_eq!(sow, NaiveDate::from_ymd_opt(2026, 3, 23).unwrap());
    }

    use chrono::Timelike;

    #[test]
    fn test_end_of_month_regular() {
        let march = NaiveDate::from_ymd_opt(2026, 3, 15).unwrap();
        let eom = end_of_month(march).unwrap();
        assert_eq!(eom, NaiveDate::from_ymd_opt(2026, 3, 31).unwrap());
    }

    #[test]
    fn test_end_of_month_february_leap() {
        let feb_leap = NaiveDate::from_ymd_opt(2028, 2, 1).unwrap();
        let eom = end_of_month(feb_leap).unwrap();
        assert_eq!(eom, NaiveDate::from_ymd_opt(2028, 2, 29).unwrap());
    }

    #[test]
    fn test_end_of_month_december() {
        let dec = NaiveDate::from_ymd_opt(2026, 12, 1).unwrap();
        let eom = end_of_month(dec).unwrap();
        assert_eq!(eom, NaiveDate::from_ymd_opt(2026, 12, 31).unwrap());
    }

    // ========== Additional named date tests ==========

    #[test]
    fn test_resolve_sow() {
        let now = fixed_now(); // 2026-03-29 (Sunday)
        let result = resolve_named_date_at("sow", now).unwrap();
        // Start of week = Monday = 2026-03-23
        assert_eq!(
            result.date_naive(),
            NaiveDate::from_ymd_opt(2026, 3, 23).unwrap(),
            "sow should resolve to Monday of current week"
        );
        assert_eq!(result.time(), NaiveTime::MIN);
    }

    #[test]
    fn test_resolve_som() {
        let now = fixed_now(); // 2026-03-29
        let result = resolve_named_date_at("som", now).unwrap();
        assert_eq!(
            result.date_naive(),
            NaiveDate::from_ymd_opt(2026, 3, 1).unwrap(),
            "som should resolve to first day of current month"
        );
        assert_eq!(result.time(), NaiveTime::MIN);
    }

    #[test]
    fn test_resolve_eoy() {
        let now = fixed_now(); // 2026-03-29
        let result = resolve_named_date_at("eoy", now).unwrap();
        assert_eq!(
            result.date_naive(),
            NaiveDate::from_ymd_opt(2026, 12, 31).unwrap(),
            "eoy should resolve to Dec 31 of current year"
        );
        assert_eq!(result.hour(), 23);
        assert_eq!(result.minute(), 59);
        assert_eq!(result.second(), 59);
    }

    #[test]
    fn test_resolve_soy() {
        let now = fixed_now(); // 2026-03-29
        let result = resolve_named_date_at("soy", now).unwrap();
        assert_eq!(
            result.date_naive(),
            NaiveDate::from_ymd_opt(2026, 1, 1).unwrap(),
            "soy should resolve to Jan 1 of current year"
        );
        assert_eq!(result.time(), NaiveTime::MIN);
    }

    #[test]
    fn test_resolve_later() {
        let now = fixed_now();
        let result = resolve_named_date_at("later", now).unwrap();
        assert_eq!(
            result.date_naive(),
            NaiveDate::from_ymd_opt(9999, 12, 30).unwrap(),
            "later should resolve to a far future date"
        );
    }

    #[test]
    fn test_resolve_someday() {
        let now = fixed_now();
        let later = resolve_named_date_at("later", now).unwrap();
        let someday = resolve_named_date_at("someday", now).unwrap();
        assert_eq!(
            later, someday,
            "someday should resolve to the same date as later"
        );
    }
}

#[cfg(test)]
mod duration_tests {
    use super::*;

    #[test]
    fn test_parse_relative_days() {
        let dt = parse_relative_duration("7d").unwrap();
        let expected = Utc::now() + Duration::days(7);
        assert!(
            (dt - expected).num_seconds().abs() < 2,
            "7d should be ~7 days from now"
        );
    }

    #[test]
    fn test_parse_relative_weeks() {
        let dt = parse_relative_duration("2w").unwrap();
        let expected = Utc::now() + Duration::weeks(2);
        assert!(
            (dt - expected).num_seconds().abs() < 2,
            "2w should be ~14 days from now"
        );
    }

    #[test]
    fn test_parse_relative_months() {
        let dt = parse_relative_duration("1m").unwrap();
        let expected = Utc::now() + Duration::days(30);
        assert!(
            (dt - expected).num_seconds().abs() < 2,
            "1m should be ~30 days from now"
        );
    }

    #[test]
    fn test_parse_relative_years() {
        let dt = parse_relative_duration("1y").unwrap();
        let expected = Utc::now() + Duration::days(365);
        assert!(
            (dt - expected).num_seconds().abs() < 2,
            "1y should be ~365 days from now"
        );
    }

    #[test]
    fn test_parse_relative_seconds() {
        let dt = parse_relative_duration("3600s").unwrap();
        let expected = Utc::now() + Duration::seconds(3600);
        assert!(
            (dt - expected).num_seconds().abs() < 2,
            "3600s should be ~1 hour from now"
        );
    }

    #[test]
    fn test_parse_relative_invalid() {
        assert!(parse_relative_duration("").is_none());
        assert!(parse_relative_duration("d").is_none());
        assert!(parse_relative_duration("abc").is_none());
        assert!(parse_relative_duration("7x").is_none());
    }

    #[test]
    fn test_parse_relative_overflow_returns_none() {
        assert!(parse_relative_duration("9223372036854775807s").is_none());
        assert!(parse_relative_duration("307445734561825861m").is_none());
        assert!(parse_relative_duration("-307445734561825861y").is_none());
    }

    #[test]
    fn test_parse_date_value_with_duration() {
        // parse_date_value should now handle relative durations
        assert!(
            parse_date_value("7d").is_some(),
            "7d should parse as relative duration"
        );
        assert!(
            parse_date_value("2w").is_some(),
            "2w should parse as relative duration"
        );
        assert!(
            parse_date_value("1m").is_some(),
            "1m should parse as relative duration"
        );
    }

    #[test]
    fn test_parse_date_value_named_still_works() {
        assert!(parse_date_value("tomorrow").is_some());
        assert!(parse_date_value("today").is_some());
        assert!(parse_date_value("eow").is_some());
    }

    #[test]
    fn test_parse_date_value_tw_format_still_works() {
        assert!(parse_date_value("20260401T090000Z").is_some());
    }

    #[test]
    fn test_parse_date_value_iso_still_works() {
        assert!(parse_date_value("2026-04-01").is_some());
    }
}
