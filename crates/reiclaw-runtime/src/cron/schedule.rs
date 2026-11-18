use crate::cron::Schedule;
use anyhow::{Context, Result};
use jiff::{
    SignedDuration, Timestamp,
    tz::{AmbiguousOffset, TimeZone},
};
use jiff_cron::{Schedule as CronExprSchedule, TimeUnitSpec};
use reiclaw_api::time::{format_rfc3339, format_rfc3339_zoned};
use std::str::FromStr;

pub fn next_run_for_schedule(schedule: &Schedule, from: Timestamp) -> Result<Timestamp> {
    match schedule {
        Schedule::Cron { expr, tz } => next_cron_run(expr, tz.as_deref(), from),
        Schedule::At { at } => Ok(*at),
        Schedule::Every { every_ms } => {
            if *every_ms == 0 {
                anyhow::bail!("Invalid schedule: every_ms must be > 0");
            }
            let ms = i64::try_from(*every_ms).context("every_ms is too large")?;
            let delta = SignedDuration::from_millis(ms);
            from.checked_add(delta).map_err(|error| {
                ::reiclaw_log::record!(
                    ERROR,
                    ::reiclaw_log::Event::new(module_path!(), ::reiclaw_log::Action::Fail)
                        .with_outcome(::reiclaw_log::EventOutcome::Failure)
                        .with_attrs(::serde_json::json!({"every_ms": *every_ms})),
                    "cron schedule: every_ms overflowed timestamp arithmetic"
                );
                anyhow::Error::new(error).context("every_ms overflowed timestamp")
            })
        }
    }
}

fn next_cron_run(expr: &str, tz_name: Option<&str>, from: Timestamp) -> Result<Timestamp> {
    let normalized = normalize_expression(expr)?;
    let cron = CronExprSchedule::from_str(&normalized)
        .with_context(|| format!("Invalid cron expression: {expr}"))?;
    // Keep weekday authority in the parsed schedule, but evaluate the other
    // fields separately so an excluded weekday can be skipped as one day.
    // jiff-cron's own weekday loop otherwise visits each second for schedules
    // such as `* * * * * MON` while searching across excluded days.
    let calendar_expression = normalized
        .split_whitespace()
        .enumerate()
        .map(|(index, field)| if index == 5 { "*" } else { field })
        .collect::<Vec<_>>()
        .join(" ");
    let calendar = CronExprSchedule::from_str(&calendar_expression)
        .with_context(|| format!("Invalid cron expression: {expr}"))?;
    let timezone = match tz_name {
        Some(name) => {
            TimeZone::get(name).with_context(|| format!("Invalid IANA timezone: {name}"))?
        }
        None => TimeZone::try_system().context("Failed to determine local timezone for cron")?,
    };

    // Enumerate civil fields in UTC, then resolve them in the real timezone.
    // This preserves the retained scheduler's wall-clock policy: nonexistent
    // local times are skipped; a repeated local time uses its earlier instant.
    // It also keeps zoned arithmetic from shifting a 02:30 job into a DST gap.
    // Clearing fractions avoids jiff-cron 0.3's rounded-candidate fast path,
    // which checks date/time fields but does not check the weekday.
    let mut cursor = from
        .to_zoned(timezone.clone())
        .datetime()
        .with()
        .subsec_nanosecond(0)
        .build()?
        .to_zoned(TimeZone::UTC)?;
    loop {
        let previous = cursor.timestamp();
        let Some(wall) = calendar.after(cursor).next() else {
            break;
        };
        if wall.timestamp() <= previous {
            anyhow::bail!("Cron expression did not advance: {expr}");
        }
        if !cron
            .days_of_week()
            .includes(wall.weekday().to_sunday_one_offset() as u32)
        {
            cursor = wall
                .with()
                .hour(23)
                .minute(59)
                .second(59)
                .subsec_nanosecond(0)
                .build()?;
            continue;
        }
        cursor = wall.clone();
        let candidate = timezone.to_ambiguous_timestamp(wall.datetime());
        if matches!(candidate.offset(), AmbiguousOffset::Gap { .. }) {
            continue;
        }
        let candidate = candidate.earlier()?;
        // In the second half of a fall-back fold, the earlier instance of a
        // later wall-clock value can already be in the past. Never return it
        // and make the scheduler repeatedly select an already-due job.
        if candidate > from {
            return Ok(candidate);
        }
    }

    ::reiclaw_log::record!(
        WARN,
        ::reiclaw_log::Event::new(module_path!(), ::reiclaw_log::Action::Fail)
            .with_outcome(::reiclaw_log::EventOutcome::Failure)
            .with_attrs(::serde_json::json!({"expr": expr})),
        "cron schedule: no future occurrence for expression"
    );
    anyhow::bail!("No future occurrence for expression: {expr}")
}

pub fn validate_schedule(schedule: &Schedule, now: Timestamp) -> Result<()> {
    match schedule {
        Schedule::Cron { expr, .. } => {
            let _ = normalize_expression(expr)?;
            let _ = next_run_for_schedule(schedule, now)?;
            Ok(())
        }
        Schedule::At { at } => {
            if *at <= now {
                let local = TimeZone::system();
                anyhow::bail!(
                    "Invalid schedule: 'at' must be in the future \
                     (now_utc={}, now_local={}, at_utc={}, at_local={}, delta_seconds={})",
                    format_rfc3339(now),
                    format_rfc3339_zoned(&now.to_zoned(local.clone())),
                    format_rfc3339(*at),
                    format_rfc3339_zoned(&at.to_zoned(local)),
                    at.duration_since(now).as_secs()
                );
            }
            Ok(())
        }
        Schedule::Every { every_ms } => {
            if *every_ms == 0 {
                anyhow::bail!("Invalid schedule: every_ms must be > 0");
            }
            Ok(())
        }
    }
}

pub fn schedule_cron_expression(schedule: &Schedule) -> Option<String> {
    match schedule {
        Schedule::Cron { expr, .. } => Some(expr.clone()),
        _ => None,
    }
}

pub fn normalize_expression(expression: &str) -> Result<String> {
    let expression = expression.trim();
    let field_count = expression.split_whitespace().count();

    match field_count {
        // standard crontab syntax: minute hour day month weekday
        // Normalize weekday field from standard crontab semantics (0/7=Sun, 1=Mon, …, 6=Sat)
        // to cron-crate semantics (1=Sun, 2=Mon, …, 7=Sat).
        5 => {
            let mut fields: Vec<&str> = expression.split_whitespace().collect();
            let weekday = fields[4];
            let normalized_weekday = normalize_weekday_field(weekday)?;
            fields[4] = &normalized_weekday;
            Ok(format!(
                "0 {} {} {} {} {}",
                fields[0], fields[1], fields[2], fields[3], fields[4]
            ))
        }
        // crate-native syntax includes seconds (+ optional year)
        6 | 7 => Ok(expression.to_string()),
        _ => anyhow::bail!(
            "Invalid cron expression: {expression} (expected 5, 6, or 7 fields, got {field_count})"
        ),
    }
}

/// Translate a single numeric weekday value from standard crontab semantics
/// (0 or 7 = Sunday, 1 = Monday, …, 6 = Saturday) to cron-crate semantics
/// (1 = Sunday, 2 = Monday, …, 7 = Saturday).
fn translate_weekday_value(val: u8) -> Result<u8> {
    match val {
        0 | 7 => Ok(1), // Sunday
        1..=6 => Ok(val + 1),
        _ => anyhow::bail!("Invalid weekday value: {val} (expected 0-7)"),
    }
}

/// Enumerate the standard-crontab weekday values covered by `start-end`.
fn standard_weekday_range_values(start: u8, end: u8) -> Vec<u8> {
    // Enumerate the standard weekday values covered by the range, wrapping
    // around the week (Sun=0..=6=Sat) when `end < start` (e.g. `5-0`).
    let mut standard_days: Vec<u8> = Vec::new();
    let mut day = start;
    loop {
        standard_days.push(day);
        if day == end {
            break;
        }
        // Step through the 0..=7 numbering used by standard crontab, treating 7
        // as the Sunday alias so that a `…-7` endpoint terminates the loop.
        day = if day >= 7 { 0 } else { day + 1 };
    }

    standard_days
}

fn translated_weekday_list(mut standard_days: Vec<u8>) -> Result<String> {
    let mut translated: Vec<u8> = standard_days
        .drain(..)
        .map(translate_weekday_value)
        .collect::<Result<Vec<u8>>>()?;
    translated.sort_unstable();
    translated.dedup();

    Ok(translated
        .into_iter()
        .map(|v| v.to_string())
        .collect::<Vec<_>>()
        .join(","))
}

fn expand_weekday_range(start: u8, end: u8) -> Result<String> {
    translated_weekday_list(standard_weekday_range_values(start, end))
}

fn expand_stepped_weekday_range(start: u8, end: u8, step: u8) -> Result<String> {
    if step == 0 {
        anyhow::bail!("Invalid weekday step: 0");
    }

    let standard_days = standard_weekday_range_values(start, end)
        .into_iter()
        .step_by(step as usize)
        .collect();

    translated_weekday_list(standard_days)
}

/// Normalize the weekday field of a 5-field cron expression from standard
/// crontab numbering to cron-crate numbering. Passes through `*`, named days
/// (e.g. `MON`, `MON-FRI`), and already-valid tokens unchanged.
fn normalize_weekday_field(field: &str) -> Result<String> {
    // Asterisk and wildcard variants pass through unchanged.
    if field == "*" || field == "?" {
        return Ok(field.to_string());
    }

    // If the field contains any alphabetic character it uses named days
    // (e.g. MON-FRI) which the cron crate handles natively.
    if field.chars().any(|c| c.is_ascii_alphabetic()) {
        return Ok(field.to_string());
    }

    // The field may be a comma-separated list of items, where each item is
    // either a single value, a range (start-end), or a range/value with a
    // step (/N).
    let parts: Vec<&str> = field.split(',').collect();
    let mut result_parts = Vec::with_capacity(parts.len());

    for part in parts {
        // Split off optional step suffix first (e.g. "1-5/2" → "1-5" + "2").
        let (range_part, step) = if let Some((r, s)) = part.split_once('/') {
            (r, Some(s))
        } else {
            (part, None)
        };

        let (translated, expanded_step_applied) =
            if let Some((start_s, end_s)) = range_part.split_once('-') {
                let start: u8 = start_s
                    .parse()
                    .with_context(|| format!("Invalid weekday in range: {start_s}"))?;
                let end: u8 = end_s
                    .parse()
                    .with_context(|| format!("Invalid weekday in range: {end_s}"))?;
                let new_start = translate_weekday_value(start)?;
                let new_end = translate_weekday_value(end)?;
                if new_start < new_end || (new_start == new_end && start == end) {
                    (format!("{new_start}-{new_end}"), false)
                } else {
                    if let Some(s) = step {
                        let step: u8 = s
                            .parse()
                            .with_context(|| format!("Invalid weekday step: {s}"))?;
                        (expand_stepped_weekday_range(start, end, step)?, true)
                    } else {
                        (expand_weekday_range(start, end)?, false)
                    }
                }
            } else if range_part == "*" {
                ("*".to_string(), false)
            } else {
                let val: u8 = range_part
                    .parse()
                    .with_context(|| format!("Invalid weekday value: {range_part}"))?;
                (translate_weekday_value(val)?.to_string(), false)
            };

        if let Some(s) = step.filter(|_| !expanded_step_applied) {
            result_parts.push(format!("{translated}/{s}"));
        } else {
            result_parts.push(translated);
        }
    }

    Ok(result_parts.join(","))
}

#[cfg(test)]
mod tests {
    use super::*;
    use jiff::civil::{Weekday, date};

    fn utc(year: i16, month: i8, day: i8, hour: i8, minute: i8, second: i8) -> Timestamp {
        date(year, month, day)
            .at(hour, minute, second, 0)
            .to_zoned(TimeZone::UTC)
            .unwrap()
            .timestamp()
    }

    #[test]
    fn next_run_for_schedule_supports_every_and_at() {
        let now = Timestamp::now();
        let every = Schedule::Every { every_ms: 60_000 };
        let next = next_run_for_schedule(&every, now).unwrap();
        assert!(next > now);

        let at = now + SignedDuration::from_mins(10);
        let at_schedule = Schedule::At { at };
        let next_at = next_run_for_schedule(&at_schedule, now).unwrap();
        assert_eq!(next_at, at);
    }

    #[test]
    fn next_run_for_schedule_supports_timezone() {
        let from = utc(2026, 2, 16, 0, 0, 0);
        let schedule = Schedule::Cron {
            expr: "0 9 * * *".into(),
            tz: Some("America/Los_Angeles".into()),
        };

        let next = next_run_for_schedule(&schedule, from).unwrap();
        assert_eq!(next, utc(2026, 2, 16, 17, 0, 0));
    }

    #[test]
    fn normalize_weekday_field_translates_standard_crontab_values() {
        // Single values: standard crontab → cron crate
        assert_eq!(normalize_weekday_field("0").unwrap(), "1"); // Sun
        assert_eq!(normalize_weekday_field("1").unwrap(), "2"); // Mon
        assert_eq!(normalize_weekday_field("5").unwrap(), "6"); // Fri
        assert_eq!(normalize_weekday_field("6").unwrap(), "7"); // Sat
        assert_eq!(normalize_weekday_field("7").unwrap(), "1"); // Sun (alias)
    }

    #[test]
    fn normalize_weekday_field_translates_ranges() {
        // 1-5 (Mon-Fri) → 2-6
        assert_eq!(normalize_weekday_field("1-5").unwrap(), "2-6");
        // 0-6 (Sun-Sat) → 1-7
        assert_eq!(normalize_weekday_field("0-6").unwrap(), "1-7");
    }

    #[test]
    fn normalize_weekday_field_handles_sunday_alias_in_range() {
        // 0-7 (Sun..Sun = the full week) → every cron-crate weekday.
        assert_eq!(normalize_weekday_field("0-7").unwrap(), "1,2,3,4,5,6,7");
        // 6-7 (Sat,Sun) → cron-crate 7 (Sat) + 1 (Sun).
        assert_eq!(normalize_weekday_field("6-7").unwrap(), "1,7");
        // 5-7 (Fri,Sat,Sun) → cron-crate 6,7,1.
        assert_eq!(normalize_weekday_field("5-7").unwrap(), "1,6,7");
        // 5-0 (Fri,Sat,Sun, wrapping through Sunday) → cron-crate 6,7,1.
        assert_eq!(normalize_weekday_field("5-0").unwrap(), "1,6,7");

        // Ascending ranges that do not touch a Sunday alias keep the compact
        // range form (regression guard for the unchanged path).
        assert_eq!(normalize_weekday_field("1-5").unwrap(), "2-6");
        assert_eq!(normalize_weekday_field("0-6").unwrap(), "1-7");
    }

    #[test]
    fn normalize_weekday_field_applies_steps_to_expanded_sunday_alias_ranges() {
        // The step belongs to the original standard-crontab range. Once we
        // expand Sunday-alias ranges into comma-separated values, the step must
        // be consumed before joining; otherwise `0-7/2` becomes the invalid
        // `1,2,3,4,5,6,7/2` shape where `/2` applies only to the final item.
        assert_eq!(normalize_weekday_field("0-7/2").unwrap(), "1,3,5,7");
        assert_eq!(normalize_weekday_field("6-7/2").unwrap(), "7");
        assert_eq!(normalize_weekday_field("5-7/2").unwrap(), "1,6");

        // Compact translated ranges still keep cron-crate's native step form.
        assert_eq!(normalize_weekday_field("1-5/2").unwrap(), "2-6/2");
        assert_eq!(normalize_weekday_field("0-6/2").unwrap(), "1-7/2");

        assert!(normalize_weekday_field("6-7/0").is_err());
    }

    #[test]
    fn normalize_weekday_field_translates_lists() {
        // 0,6 (Sun,Sat) → 1,7
        assert_eq!(normalize_weekday_field("0,6").unwrap(), "1,7");
        // 1,3,5 (Mon,Wed,Fri) → 2,4,6
        assert_eq!(normalize_weekday_field("1,3,5").unwrap(), "2,4,6");
    }

    #[test]
    fn normalize_weekday_field_translates_steps() {
        // 1-5/2 (Mon-Fri every other) → 2-6/2
        assert_eq!(normalize_weekday_field("1-5/2").unwrap(), "2-6/2");
        // */2 (every other day) → */2
        assert_eq!(normalize_weekday_field("*/2").unwrap(), "*/2");
    }

    #[test]
    fn normalize_weekday_field_passes_through_wildcards_and_names() {
        assert_eq!(normalize_weekday_field("*").unwrap(), "*");
        assert_eq!(normalize_weekday_field("?").unwrap(), "?");
        assert_eq!(normalize_weekday_field("MON-FRI").unwrap(), "MON-FRI");
        assert_eq!(
            normalize_weekday_field("MON,WED,FRI").unwrap(),
            "MON,WED,FRI"
        );
    }

    #[test]
    fn normalize_expression_applies_weekday_fix_to_5_field() {
        // "0 9 * * 1-5" should become "0 0 9 * * 2-6"
        let result = normalize_expression("0 9 * * 1-5").unwrap();
        assert_eq!(result, "0 0 9 * * 2-6");
    }

    #[test]
    fn normalize_expression_does_not_modify_6_field() {
        // 6-field expressions already use cron-crate semantics
        let result = normalize_expression("0 0 9 * * 1-5").unwrap();
        assert_eq!(result, "0 0 9 * * 1-5");
    }

    #[test]
    fn weekday_1_5_schedules_monday_through_friday() {
        // 2026-02-16 is a Monday. With "0 9 * * 1-5" (Mon-Fri at 09:00 UTC),
        // the next run from Sunday 2026-02-15 should be Monday 2026-02-16.
        let sunday = utc(2026, 2, 15, 0, 0, 0);
        let schedule = Schedule::Cron {
            expr: "0 9 * * 1-5".into(),
            tz: Some("UTC".into()),
        };
        let next = next_run_for_schedule(&schedule, sunday).unwrap();
        // Should be Monday 2026-02-16 at 09:00 UTC (weekday = Mon)
        assert_eq!(next, utc(2026, 2, 16, 9, 0, 0));
        assert_eq!(next.to_zoned(TimeZone::UTC).weekday(), Weekday::Monday);
    }

    #[test]
    fn weekday_1_5_does_not_fire_on_saturday_or_sunday() {
        // From Friday evening, next run should skip Sat/Sun → Monday
        let friday_evening = utc(2026, 2, 20, 18, 0, 0);
        let schedule = Schedule::Cron {
            expr: "0 9 * * 1-5".into(),
            tz: Some("UTC".into()),
        };
        let next = next_run_for_schedule(&schedule, friday_evening).unwrap();
        // Should be Monday 2026-02-23 at 09:00 UTC
        assert_eq!(next, utc(2026, 2, 23, 9, 0, 0));
        assert_eq!(next.to_zoned(TimeZone::UTC).weekday(), Weekday::Monday);
    }

    #[test]
    fn weekday_0_means_sunday() {
        // "0 10 * * 0" should fire on Sunday only
        let monday = utc(2026, 2, 16, 0, 0, 0);
        let schedule = Schedule::Cron {
            expr: "0 10 * * 0".into(),
            tz: Some("UTC".into()),
        };
        let next = next_run_for_schedule(&schedule, monday).unwrap();
        assert_eq!(next.to_zoned(TimeZone::UTC).weekday(), Weekday::Sunday);
    }

    #[test]
    fn weekday_7_means_sunday() {
        // "0 10 * * 7" should also fire on Sunday (alias)
        let monday = utc(2026, 2, 16, 0, 0, 0);
        let schedule = Schedule::Cron {
            expr: "0 10 * * 7".into(),
            tz: Some("UTC".into()),
        };
        let next = next_run_for_schedule(&schedule, monday).unwrap();
        assert_eq!(next.to_zoned(TimeZone::UTC).weekday(), Weekday::Sunday);
    }

    #[test]
    fn weekend_range_6_7_fires_sat_and_sun() {
        // `6-7` is the standard-crontab weekend range (Sat,Sun). Before the
        // Sunday-alias fix it normalized to the non-ascending `7-1`, which
        // cron 0.15 rejects, so this would have errored (and the `.unwrap()`
        // below would panic) instead of producing the next weekend run.
        let monday = utc(2026, 2, 16, 0, 0, 0);
        let schedule = Schedule::Cron {
            expr: "0 10 * * 6-7".into(),
            tz: Some("UTC".into()),
        };

        // First run is Saturday 2026-02-21 at 10:00 UTC.
        let saturday = next_run_for_schedule(&schedule, monday).unwrap();
        assert_eq!(saturday, utc(2026, 2, 21, 10, 0, 0));
        assert_eq!(
            saturday.to_zoned(TimeZone::UTC).weekday(),
            Weekday::Saturday
        );

        // The following run is the very next day, Sunday 2026-02-22.
        let sunday = next_run_for_schedule(&schedule, saturday).unwrap();
        assert_eq!(sunday, utc(2026, 2, 22, 10, 0, 0));
        assert_eq!(sunday.to_zoned(TimeZone::UTC).weekday(), Weekday::Sunday);
    }

    #[test]
    fn stepped_full_week_sunday_alias_range_fires_every_other_day() {
        let monday = utc(2026, 2, 16, 0, 0, 0);
        let schedule = Schedule::Cron {
            expr: "0 10 * * 0-7/2".into(),
            tz: Some("UTC".into()),
        };

        let next = next_run_for_schedule(&schedule, monday).unwrap();
        assert_eq!(next, utc(2026, 2, 17, 10, 0, 0));
        assert_eq!(next.to_zoned(TimeZone::UTC).weekday(), Weekday::Tuesday);
    }

    #[test]
    fn no_tz_defaults_to_local_timezone() {
        let from = utc(2026, 6, 15, 12, 0, 0);
        let schedule_no_tz = Schedule::Cron {
            expr: "0 9 * * *".into(),
            tz: None,
        };
        let schedule_utc = Schedule::Cron {
            expr: "0 9 * * *".into(),
            tz: Some("UTC".into()),
        };
        let next_local = next_run_for_schedule(&schedule_no_tz, from).unwrap();
        let next_utc = next_run_for_schedule(&schedule_utc, from).unwrap();
        assert!(next_local > from);
        assert!(next_utc > from);
        let local_offset = from.to_zoned(TimeZone::system()).offset().seconds();
        if local_offset == 0 {
            assert_eq!(next_local, next_utc);
        } else {
            assert_ne!(next_local, next_utc);
        }
    }

    #[test]
    fn fractional_start_still_checks_weekday_and_excludes_the_current_second() {
        let monday_only = Schedule::Cron {
            expr: "10 * * * MON".into(),
            tz: Some("UTC".into()),
        };
        // Rounding this Tuesday up to 12:10 must not bypass the Monday field.
        let from = "2026-02-17T12:09:59.500Z".parse().unwrap();
        assert_eq!(
            next_run_for_schedule(&monday_only, from).unwrap(),
            utc(2026, 2, 23, 0, 10, 0),
        );
        let every_minute = Schedule::Cron {
            expr: "* * * * *".into(),
            tz: Some("UTC".into()),
        };
        for from in [
            "2026-02-17T12:10:00Z",
            "2026-02-17T12:10:00.000000001Z",
            "2026-02-17T12:10:59.999999999Z",
        ] {
            let from = from.parse().unwrap();
            assert_eq!(
                next_run_for_schedule(&every_minute, from).unwrap(),
                utc(2026, 2, 17, 12, 11, 0),
            );
        }
    }

    #[test]
    fn spring_gap_is_skipped_across_repeated_production_calls() {
        let schedule = Schedule::Cron {
            expr: "30 2 * * *".into(),
            tz: Some("America/Los_Angeles".into()),
        };
        let last_before_gap = utc(2026, 3, 7, 10, 30, 0);
        let first = next_run_for_schedule(&schedule, last_before_gap).unwrap();
        let second = next_run_for_schedule(&schedule, first).unwrap();
        // March 8 has no 02:30. Do not turn that missing occurrence into 03:00
        // or 03:30; the retained policy moves to the next scheduled civil day.
        assert_eq!(first, utc(2026, 3, 9, 9, 30, 0));
        assert_eq!(second, utc(2026, 3, 10, 9, 30, 0));
    }

    #[test]
    fn fall_fold_uses_one_earlier_instance_and_never_returns_a_past_run() {
        let schedule = Schedule::Cron {
            expr: "30 1 * * *".into(),
            tz: Some("America/Los_Angeles".into()),
        };
        let before_fold = utc(2026, 10, 31, 8, 30, 0);
        let first = next_run_for_schedule(&schedule, before_fold).unwrap();
        let second = next_run_for_schedule(&schedule, first).unwrap();
        assert_eq!(first, utc(2026, 11, 1, 8, 30, 0));
        assert_eq!(second, utc(2026, 11, 2, 9, 30, 0));

        // 01:15 in the later (PST) half of the fold. Resolving the next 01:30
        // to its earlier PDT instance would go backwards to 08:30 UTC.
        let later_half = utc(2026, 11, 1, 9, 15, 0);
        let next = next_run_for_schedule(&schedule, later_half).unwrap();
        assert_eq!(next, second);
        assert!(next > later_half);
    }

    #[test]
    fn native_fields_preserve_weekday_and_day_intersection_semantics() {
        let from = utc(2026, 2, 16, 12, 0, 0);
        let native_sunday = Schedule::Cron {
            expr: "0 0 10 * * 1".into(),
            tz: Some("UTC".into()),
        };
        assert_eq!(
            next_run_for_schedule(&native_sunday, from).unwrap(),
            utc(2026, 2, 22, 10, 0, 0),
        );
        let day_and_weekday = Schedule::Cron {
            expr: "0 0 10 1 * MON 2027".into(),
            tz: Some("UTC".into()),
        };
        assert_eq!(
            next_run_for_schedule(&day_and_weekday, utc(2027, 1, 2, 0, 0, 0)).unwrap(),
            utc(2027, 2, 1, 10, 0, 0),
        );
        let expired = Schedule::Cron {
            expr: "0 0 10 * * * 2025".into(),
            tz: Some("UTC".into()),
        };
        assert!(next_run_for_schedule(&expired, from).is_err());
        let invalid_timezone = Schedule::Cron {
            expr: "0 10 * * *".into(),
            tz: Some("Invalid/Timezone".into()),
        };
        assert!(next_run_for_schedule(&invalid_timezone, from).is_err());
    }

    #[test]
    fn second_level_weekday_search_preserves_month_day_and_year_constraints() {
        let every_second_on_monday = Schedule::Cron {
            expr: "* * * * * MON".into(),
            tz: Some("UTC".into()),
        };
        assert_eq!(
            next_run_for_schedule(&every_second_on_monday, utc(2026, 2, 17, 0, 0, 0)).unwrap(),
            utc(2026, 2, 23, 0, 0, 0),
        );
        let first_monday_in_2027 = Schedule::Cron {
            expr: "* * * 1 Jan-Mar MON 2027".into(),
            tz: Some("UTC".into()),
        };
        assert_eq!(
            next_run_for_schedule(&first_monday_in_2027, utc(2026, 12, 31, 23, 59, 59)).unwrap(),
            utc(2027, 2, 1, 0, 0, 0),
        );
    }

    #[test]
    fn interval_schedule_preserves_nanoseconds_and_checks_overflow() {
        let schedule = Schedule::Every { every_ms: 1_250 };
        let from = "2026-02-17T12:10:00.123456789Z".parse().unwrap();
        assert_eq!(
            next_run_for_schedule(&schedule, from).unwrap(),
            "2026-02-17T12:10:01.373456789Z"
                .parse::<Timestamp>()
                .unwrap(),
        );
        assert!(next_run_for_schedule(&schedule, Timestamp::MAX).is_err());
        assert!(next_run_for_schedule(&Schedule::Every { every_ms: u64::MAX }, from).is_err());
    }
}
