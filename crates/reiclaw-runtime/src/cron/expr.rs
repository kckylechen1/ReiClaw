use anyhow::{Context, Result, bail, ensure};
use jiff::SignedDuration;
use jiff::civil::{Date, DateTime, Weekday};

const MIN_YEAR: i16 = 1970;
const MAX_YEAR: i16 = 2099;

/// Compiled 6- or 7-field normalized cron expression evaluated over civil fields.
#[derive(Debug, Clone)]
pub(super) struct CronExpr {
    seconds: u64,
    minutes: u64,
    hours: u32,
    days: u32,
    months: u16,
    weekdays: u8,
    years: Vec<i16>,
}

impl CronExpr {
    pub(super) fn parse(normalized: &str) -> Result<Self> {
        let fields: Vec<&str> = normalized.split_whitespace().collect();
        ensure!(
            matches!(fields.len(), 6 | 7),
            "expected 6 or 7 cron fields, got {}",
            fields.len()
        );
        let seconds = parse_mask(fields[0], 0, 59, false, |s, lo, hi| {
            parse_bounded_u16(s, lo, hi, "second")
        })?;
        let minutes = parse_mask(fields[1], 0, 59, false, |s, lo, hi| {
            parse_bounded_u16(s, lo, hi, "minute")
        })?;
        let hours = parse_mask(fields[2], 0, 23, false, |s, lo, hi| {
            parse_bounded_u16(s, lo, hi, "hour")
        })? as u32;
        let days = parse_mask(fields[3], 1, 31, true, |s, lo, hi| {
            parse_bounded_u16(s, lo, hi, "day")
        })? as u32;
        let months = parse_mask(fields[4], 1, 12, false, parse_month)? as u16;
        let weekdays = parse_mask(fields[5], 1, 7, true, parse_weekday)? as u8;
        let years = match fields.get(6) {
            Some(spec) => parse_year_list(spec)?,
            None => (MIN_YEAR..=MAX_YEAR).collect(),
        };
        Ok(Self {
            seconds,
            minutes,
            hours,
            days,
            months,
            weekdays,
            years,
        })
    }

    pub(super) fn next_after(&self, after: DateTime) -> Option<DateTime> {
        let start = after.checked_add(SignedDuration::from_secs(1)).ok()?;
        let first_hour = lowest_bit(self.hours as u64, 0, 23)?;
        let first_minute = lowest_bit(self.minutes, 0, 59)?;
        let first_second = lowest_bit(self.seconds, 0, 59)?;

        for &year in &self.years {
            if year < start.year() {
                continue;
            }
            let same_year = year == start.year();
            let start_month = if same_year { start.month() as u8 } else { 1 };
            for month in iter_bits(self.months as u64, start_month, 12) {
                let first_of_month = Date::new(year, month as i8, 1).ok()?;
                let max_day = first_of_month.days_in_month() as u8;
                let same_month = same_year && month == start.month() as u8;
                let start_day = if same_month { start.day() as u8 } else { 1 };

                for day in iter_bits(self.days as u64, start_day, max_day) {
                    let Ok(date) = Date::new(year, month as i8, day as i8) else {
                        continue;
                    };
                    let dow = sunday_one_weekday(date.weekday());
                    if (self.weekdays & (1u8 << dow)) == 0 {
                        continue;
                    }
                    let same_day = same_month && day == start.day() as u8;
                    if !same_day {
                        return DateTime::new(
                            year,
                            month as i8,
                            day as i8,
                            first_hour as i8,
                            first_minute as i8,
                            first_second as i8,
                            0,
                        )
                        .ok();
                    }
                    if let Some((h, m, s)) = self.earliest_hms_at_or_after(
                        start.hour() as u8,
                        start.minute() as u8,
                        start.second() as u8,
                        first_minute,
                        first_second,
                    ) {
                        return DateTime::new(
                            year,
                            month as i8,
                            day as i8,
                            h as i8,
                            m as i8,
                            s as i8,
                            0,
                        )
                        .ok();
                    }
                }
            }
        }
        None
    }

    fn earliest_hms_at_or_after(
        &self,
        cur_hour: u8,
        cur_minute: u8,
        cur_second: u8,
        first_minute: u8,
        first_second: u8,
    ) -> Option<(u8, u8, u8)> {
        if ((self.hours as u64) >> cur_hour) & 1 != 0 {
            if (self.minutes >> cur_minute) & 1 != 0
                && let Some(sec) = lowest_bit(self.seconds, cur_second, 59)
            {
                return Some((cur_hour, cur_minute, sec));
            }
            if let Some(min) = lowest_bit(self.minutes, cur_minute.saturating_add(1), 59) {
                return Some((cur_hour, min, first_second));
            }
        }
        let hour = lowest_bit(self.hours as u64, cur_hour.saturating_add(1), 23)?;
        Some((hour, first_minute, first_second))
    }
}

fn sunday_one_weekday(weekday: Weekday) -> u8 {
    match weekday {
        Weekday::Sunday => 1,
        Weekday::Monday => 2,
        Weekday::Tuesday => 3,
        Weekday::Wednesday => 4,
        Weekday::Thursday => 5,
        Weekday::Friday => 6,
        Weekday::Saturday => 7,
    }
}

fn lowest_bit(mask: u64, min: u8, max: u8) -> Option<u8> {
    if min > max {
        return None;
    }
    let shifted = mask >> min;
    if shifted == 0 {
        return None;
    }
    let bit = min.saturating_add(shifted.trailing_zeros() as u8);
    (bit <= max).then_some(bit)
}

fn iter_bits(mask: u64, min: u8, max: u8) -> impl Iterator<Item = u8> {
    (min..=max).filter(move |&bit| (mask >> bit) & 1 != 0)
}

fn parse_range_items(
    spec: &str,
    min: u16,
    max: u16,
    allow_question: bool,
    parse_val: impl Fn(&str, u16, u16) -> Result<u16>,
    mut emit: impl FnMut(u16, u16, usize),
) -> Result<()> {
    ensure!(!spec.is_empty(), "empty cron field");
    for item in spec.split(',') {
        ensure!(!item.is_empty(), "empty item in cron field '{spec}'");
        if item == "?" {
            ensure!(
                allow_question,
                "'?' is only valid in day-of-month and weekday fields"
            );
            emit(min, max, 1);
            continue;
        }
        let (base, step) = match item.split_once('/') {
            Some((b, s)) => {
                ensure!(!s.contains('/'), "invalid cron step '{item}'");
                let step: usize = s
                    .parse()
                    .with_context(|| format!("invalid cron step '{s}'"))?;
                ensure!(step >= 1, "cron step must be >= 1 in '{item}'");
                (b, Some(step))
            }
            None => (item, None),
        };
        let (start, end) = if base == "*" {
            (min, max)
        } else if let Some((lo, hi)) = base.split_once('-') {
            ensure!(!hi.contains('-'), "invalid cron range '{item}'");
            let lo = parse_val(lo, min, max)?;
            let hi = parse_val(hi, min, max)?;
            ensure!(lo <= hi, "descending cron range '{item}'");
            (lo, hi)
        } else {
            let v = parse_val(base, min, max)?;
            (v, if step.is_some() { max } else { v })
        };
        emit(start, end, step.unwrap_or(1));
    }
    Ok(())
}

fn parse_mask(
    spec: &str,
    min: u16,
    max: u16,
    allow_question: bool,
    parse_val: impl Fn(&str, u16, u16) -> Result<u16>,
) -> Result<u64> {
    let mut mask = 0u64;
    parse_range_items(spec, min, max, allow_question, parse_val, |lo, hi, step| {
        for v in (lo..=hi).step_by(step) {
            mask |= 1u64 << v;
        }
    })?;
    ensure!(mask != 0, "cron field '{spec}' matched no values");
    Ok(mask)
}

fn parse_year_list(spec: &str) -> Result<Vec<i16>> {
    let mut years = Vec::new();
    parse_range_items(
        spec,
        MIN_YEAR as u16,
        MAX_YEAR as u16,
        false,
        |s, lo, hi| parse_bounded_u16(s, lo, hi, "year"),
        |lo, hi, step| {
            for y in (lo..=hi).step_by(step) {
                years.push(y as i16);
            }
        },
    )?;
    years.sort_unstable();
    years.dedup();
    ensure!(!years.is_empty(), "cron year field matched no values");
    Ok(years)
}

fn parse_bounded_u16(raw: &str, min: u16, max: u16, label: &str) -> Result<u16> {
    let val: u16 = raw
        .parse()
        .with_context(|| format!("invalid cron {label} '{raw}'"))?;
    if !(min..=max).contains(&val) {
        bail!("cron {label} {val} out of range {min}..={max}");
    }
    Ok(val)
}

fn parse_month(raw: &str, min: u16, max: u16) -> Result<u16> {
    match raw.to_ascii_uppercase().as_str() {
        "JAN" | "JANUARY" => Ok(1),
        "FEB" | "FEBRUARY" => Ok(2),
        "MAR" | "MARCH" => Ok(3),
        "APR" | "APRIL" => Ok(4),
        "MAY" => Ok(5),
        "JUN" | "JUNE" => Ok(6),
        "JUL" | "JULY" => Ok(7),
        "AUG" | "AUGUST" => Ok(8),
        "SEP" | "SEPTEMBER" => Ok(9),
        "OCT" | "OCTOBER" => Ok(10),
        "NOV" | "NOVEMBER" => Ok(11),
        "DEC" | "DECEMBER" => Ok(12),
        _ => parse_bounded_u16(raw, min, max, "month"),
    }
}

fn parse_weekday(raw: &str, min: u16, max: u16) -> Result<u16> {
    match raw.to_ascii_uppercase().as_str() {
        "SUN" | "SUNDAY" => Ok(1),
        "MON" | "MONDAY" => Ok(2),
        "TUE" | "TUESDAY" => Ok(3),
        "WED" | "WEDNESDAY" => Ok(4),
        "THU" | "THURSDAY" => Ok(5),
        "FRI" | "FRIDAY" => Ok(6),
        "SAT" | "SATURDAY" => Ok(7),
        _ => parse_bounded_u16(raw, min, max, "weekday"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_rejects_malformed_and_out_of_range_fields() {
        for bad in [
            "* * * * *",
            "60 * * * * *",
            "* 60 * * * *",
            "* * 24 * * *",
            "* * * 0 * *",
            "* * * 32 * *",
            "* * * * 0 *",
            "* * * * 13 *",
            "* * * * * 0",
            "* * * * * 8",
            "* * * 10-5 * *",
            "* * * */0 * *",
            "* * * * * * 1969",
            "* * * * * * 2100",
            "0 ? * * * *",
        ] {
            assert!(CronExpr::parse(bad).is_err(), "expected error for {bad}");
        }
    }

    #[test]
    fn impossibly_short_month_returns_none_without_panicking() {
        let expr = CronExpr::parse("0 0 10 31 2 * 2027").unwrap();
        let after = DateTime::new(2027, 1, 1, 0, 0, 0, 0).unwrap();
        assert_eq!(expr.next_after(after), None);
    }
}
