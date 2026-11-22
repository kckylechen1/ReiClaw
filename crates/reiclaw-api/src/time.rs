//! Wire and storage timestamp contracts shared by the retained consumers.
//!
//! Instants and arithmetic are Jiff types. These helpers only own the existing
//! RFC 3339 representation: SQL columns use numeric UTC offsets, while serde
//! uses `Z`; both keep 0/3/6/9 fractional digits instead of shortening them.

use anyhow::{Context, Result, ensure};
use jiff::fmt::temporal::DateTimePrinter;
use jiff::tz::{Offset, TimeZone};
use jiff::{Timestamp, ZonedDateTime};

/// Unix seconds rounded down, including instants before the Unix epoch.
/// Jiff's whole-unit accessors truncate toward zero instead.
pub fn epoch_second(timestamp: Timestamp) -> i64 {
    timestamp.as_second() - i64::from(timestamp.subsec_nanosecond() < 0)
}

/// Unix milliseconds rounded down, preserving existing numeric wire fields.
pub fn epoch_millisecond(timestamp: Timestamp) -> i64 {
    timestamp.as_millisecond() - i64::from(timestamp.subsec_nanosecond() % 1_000_000 < 0)
}

fn auto_precision(timestamp: Timestamp) -> u8 {
    let nanos = timestamp.subsec_nanosecond().unsigned_abs();
    if nanos == 0 {
        0
    } else if nanos.is_multiple_of(1_000_000) {
        3
    } else if nanos.is_multiple_of(1_000) {
        6
    } else {
        9
    }
}

fn stored_year_width(formatted: String) -> String {
    // Jiff's Temporal printer uses six digits for negative years; the existing
    // storage and serde format uses four within Jiff's supported year range.
    if let Some(year) = formatted.strip_prefix("-00") {
        format!("-{year}")
    } else {
        formatted
    }
}

/// Format a UTC instant for existing RFC 3339 text columns and explicit output.
pub fn format_rfc3339(timestamp: Timestamp) -> String {
    stored_year_width(
        DateTimePrinter::new()
            .precision(Some(auto_precision(timestamp)))
            .timestamp_with_offset_to_string(&timestamp, Offset::UTC),
    )
}

/// Format the existing serde representation, including grouped fractional digits.
pub fn format_rfc3339_z(timestamp: Timestamp) -> String {
    stored_year_width(
        DateTimePrinter::new()
            .precision(Some(auto_precision(timestamp)))
            .timestamp_to_string(&timestamp),
    )
}

/// Format a local or fixed-offset datetime without adding a time-zone annotation.
pub fn format_rfc3339_zoned(datetime: &ZonedDateTime) -> String {
    let timestamp = datetime.timestamp();
    stored_year_width(
        DateTimePrinter::new()
            .precision(Some(auto_precision(timestamp)))
            .timestamp_with_offset_to_string(&timestamp, datetime.offset()),
    )
}

/// Canonical millisecond precision used by the portable memory contract.
pub fn format_rfc3339_millis(timestamp: Timestamp) -> String {
    stored_year_width(
        DateTimePrinter::new()
            .precision(Some(3))
            .timestamp_to_string(&timestamp),
    )
}

/// Canonical nanosecond precision used by stable memory ordering.
pub fn format_rfc3339_nanos(timestamp: Timestamp) -> String {
    stored_year_width(
        DateTimePrinter::new()
            .precision(Some(9))
            .timestamp_to_string(&timestamp),
    )
}

/// Parse a complete RFC 3339 instant without accepting Temporal annotations,
/// abbreviated offsets or a missing seconds field. As before, `t`/`z` and a
/// single space separator are accepted and excess fractional digits truncate.
/// Leap seconds are rejected because Jiff cannot represent them losslessly.
pub fn parse_rfc3339(raw: &str) -> Result<Timestamp> {
    parse_parts(raw).map(|(timestamp, _)| timestamp)
}

/// Parse an RFC 3339 instant while retaining the supplied offset's calendar date.
pub fn parse_rfc3339_zoned(raw: &str) -> Result<ZonedDateTime> {
    let (timestamp, offset) = parse_parts(raw)?;
    Ok(timestamp.to_zoned_datetime(TimeZone::fixed(offset)))
}

/// Read the relaxed timestamp grammar previously accepted by stored typed
/// fields: unpadded components, component whitespace, `UTC`, and minute offsets
/// with optional colons. Strict protocol inputs must use [`parse_rfc3339`].
pub fn parse_stored_timestamp(raw: &str) -> Result<Timestamp> {
    if let Ok(timestamp) = parse_rfc3339(raw) {
        return Ok(timestamp);
    }

    fn number(rest: &mut &str, width: usize) -> Result<i16> {
        *rest = rest.trim_start();
        let len = rest
            .bytes()
            .take(width)
            .take_while(u8::is_ascii_digit)
            .count();
        ensure!(len != 0, "missing timestamp component");
        let value = rest[..len].parse()?;
        *rest = &rest[len..];
        Ok(value)
    }

    fn delimiter(rest: &mut &str, expected: char) -> Result<()> {
        *rest = rest
            .trim_start()
            .strip_prefix(expected)
            .context("invalid timestamp separator")?;
        Ok(())
    }

    let mut rest = raw.trim();
    let signed = rest.starts_with(['+', '-']);
    let negative = rest.starts_with('-');
    if signed {
        rest = &rest[1..];
        ensure!(
            rest.as_bytes().first().is_some_and(u8::is_ascii_digit),
            "missing signed year"
        );
    }
    let width = if signed { rest.len() } else { 4 };
    let year = number(&mut rest, width)? * if negative { -1 } else { 1 };
    delimiter(&mut rest, '-')?;
    let month = number(&mut rest, 2)?;
    delimiter(&mut rest, '-')?;
    let day = number(&mut rest, 2)?;
    ensure!(
        rest.starts_with(['T', 't', ' ']),
        "missing date/time separator"
    );
    rest = &rest[1..];
    let hour = number(&mut rest, 2)?;
    delimiter(&mut rest, ':')?;
    let minute = number(&mut rest, 2)?;
    delimiter(&mut rest, ':')?;
    let second = number(&mut rest, 2)?;
    let mut nanos = 0;
    if let Some(fraction) = rest.strip_prefix('.') {
        let len = fraction.bytes().take_while(u8::is_ascii_digit).count();
        ensure!(len != 0, "empty fractional seconds");
        let precision = len.min(9);
        nanos = fraction[..precision].parse::<i32>()? * 10_i32.pow(9 - precision as u32);
        rest = &fraction[len..];
    }
    let civil = jiff::civil::DateTime::new(
        year,
        month.try_into()?,
        day.try_into()?,
        hour.try_into()?,
        minute.try_into()?,
        second.try_into()?,
        nanos,
    )?;
    let zone = rest.trim();
    let offset = if zone.eq_ignore_ascii_case("utc") || zone.eq_ignore_ascii_case("z") {
        Offset::UTC
    } else {
        let sign = zone.chars().next().context("missing UTC offset")?;
        ensure!(matches!(sign, '+' | '-' | '−'), "invalid UTC offset sign");
        let digits = &zone[sign.len_utf8()..];
        ensure!(
            digits.len() >= 4 && digits.as_bytes()[..2].iter().all(u8::is_ascii_digit),
            "invalid UTC offset hours"
        );
        let hours: i32 = digits[..2].parse()?;
        let minutes = digits[2..].trim_start_matches(|c: char| c == ':' || c.is_whitespace());
        ensure!(
            minutes.len() == 2 && minutes.as_bytes().iter().all(u8::is_ascii_digit),
            "invalid UTC offset minutes"
        );
        let minutes: i32 = minutes.parse()?;
        ensure!(hours <= 23 && minutes <= 59, "UTC offset out of range");
        Offset::from_seconds((hours * 3600 + minutes * 60) * if sign == '+' { 1 } else { -1 })?
    };
    offset
        .to_timestamp(civil)
        .context("invalid stored timestamp")
}

fn parse_parts(raw: &str) -> Result<(Timestamp, Offset)> {
    let bytes = raw.as_bytes();
    ensure!(bytes.len() >= 20, "incomplete RFC3339 timestamp");
    ensure!(
        [0, 1, 2, 3, 5, 6, 8, 9, 11, 12, 14, 15, 17, 18]
            .into_iter()
            .all(|index| bytes[index].is_ascii_digit())
            && bytes[4] == b'-'
            && bytes[7] == b'-'
            && matches!(bytes[10], b'T' | b't' | b' ')
            && bytes[13] == b':'
            && bytes[16] == b':',
        "invalid RFC3339 date/time syntax"
    );
    ensure!(
        &bytes[17..19] != b"60",
        "leap seconds are not representable"
    );
    let mut offset_start = 19;
    if bytes[offset_start] == b'.' {
        offset_start += 1;
        let fraction_start = offset_start;
        while bytes.get(offset_start).is_some_and(u8::is_ascii_digit) {
            offset_start += 1;
        }
        ensure!(offset_start > fraction_start, "empty fractional seconds");
    }
    // Retain the previous parser's Unicode minus sign support in UTC offsets.
    let normalized;
    let raw = if let Some(offset) = raw[offset_start..].strip_prefix('−') {
        normalized = format!("{}-{offset}", &raw[..offset_start]);
        normalized.as_str()
    } else {
        raw
    };
    let suffix = &raw.as_bytes()[offset_start..];
    let offset = if matches!(suffix, b"Z" | b"z") {
        Offset::UTC
    } else {
        ensure!(
            suffix.len() == 6
                && matches!(suffix[0], b'+' | b'-')
                && suffix[3] == b':'
                && [1, 2, 4, 5]
                    .into_iter()
                    .all(|index| suffix[index].is_ascii_digit()),
            "invalid RFC3339 UTC offset"
        );
        let hours = i32::from(suffix[1] - b'0') * 10 + i32::from(suffix[2] - b'0');
        let minutes = i32::from(suffix[4] - b'0') * 10 + i32::from(suffix[5] - b'0');
        ensure!(
            hours <= 23 && minutes <= 59,
            "RFC3339 UTC offset out of range"
        );
        let seconds = (hours * 3600 + minutes * 60) * if suffix[0] == b'-' { -1 } else { 1 };
        Offset::from_seconds(seconds)?
    };
    // RFC 3339 permits more than nanosecond precision; retained consumers have
    // always truncated the additional digits rather than rounding the instant.
    let timestamp = if offset_start > 29 {
        format!("{}{}", &raw[..29], &raw[offset_start..])
            .parse::<Timestamp>()
            .context("invalid RFC3339 timestamp")?
    } else {
        raw.parse::<Timestamp>()
            .context("invalid RFC3339 timestamp")?
    };
    Ok((timestamp, offset))
}

/// Preserve the existing JSON/TOML timestamp representation for typed fields.
pub mod serde {
    use super::{format_rfc3339_z, parse_stored_timestamp};
    use ::serde::{Deserialize, Deserializer, Serializer};
    use jiff::Timestamp;

    pub fn serialize<S: Serializer>(value: &Timestamp, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&format_rfc3339_z(*value))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Timestamp, D::Error> {
        let raw = String::deserialize(deserializer)?;
        parse_stored_timestamp(&raw).map_err(::serde::de::Error::custom)
    }

    pub mod optional {
        use super::*;

        pub fn serialize<S: Serializer>(
            value: &Option<Timestamp>,
            serializer: S,
        ) -> Result<S::Ok, S::Error> {
            match value {
                Some(timestamp) => serializer.serialize_some(&format_rfc3339_z(*timestamp)),
                None => serializer.serialize_none(),
            }
        }

        pub fn deserialize<'de, D: Deserializer<'de>>(
            deserializer: D,
        ) -> Result<Option<Timestamp>, D::Error> {
            Option::<String>::deserialize(deserializer)?
                .map(|raw| parse_stored_timestamp(&raw).map_err(::serde::de::Error::custom))
                .transpose()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unix_units_floor_negative_instants_without_changing_duration_rounding() {
        for nanos in [
            -1_000_000_001,
            -1_000_000_000,
            -1_000_001,
            -1_000_000,
            -1,
            0,
            1,
            1_000_001,
        ] {
            let timestamp = Timestamp::from_nanosecond(nanos).unwrap();
            assert_eq!(
                i128::from(epoch_second(timestamp)),
                nanos.div_euclid(1_000_000_000)
            );
            assert_eq!(
                i128::from(epoch_millisecond(timestamp)),
                nanos.div_euclid(1_000_000)
            );
        }
    }

    #[test]
    fn wire_formats_preserve_grouped_precision_and_numeric_storage_offset() {
        for (raw, fraction) in [
            ("2026-01-02T03:04:05Z", ""),
            ("2026-01-02T03:04:05.1Z", ".100"),
            ("2026-01-02T03:04:05.1234Z", ".123400"),
            ("2026-01-02T03:04:05.123456789Z", ".123456789"),
            ("1969-12-31T23:59:59.1Z", ".100"),
        ] {
            let timestamp = parse_rfc3339(raw).unwrap();
            let second = &raw[..19];
            assert_eq!(
                format_rfc3339(timestamp),
                format!("{second}{fraction}+00:00")
            );
            assert_eq!(format_rfc3339_z(timestamp), format!("{second}{fraction}Z"));
        }
        let timestamp = parse_rfc3339("2026-01-02T03:04:05.123456789Z").unwrap();
        assert_eq!(format_rfc3339_millis(timestamp), "2026-01-02T03:04:05.123Z");
        assert_eq!(
            format_rfc3339_nanos(timestamp),
            "2026-01-02T03:04:05.123456789Z"
        );
    }

    #[test]
    fn new_storage_values_keep_order_against_existing_rows() {
        let before = "2026-01-02T03:04:05+00:00";
        let middle = format_rfc3339(parse_rfc3339("2026-01-02T03:04:05.000001Z").unwrap());
        let after = "2026-01-02T03:04:05.001+00:00";
        assert!(before < middle.as_str());
        assert!(middle.as_str() < after);
    }

    #[test]
    fn parsing_retains_offset_dates_and_the_existing_nanosecond_truncation() {
        let zoned = parse_rfc3339_zoned("2026-01-02T00:30:00+09:00").unwrap();
        assert_eq!(zoned.date().to_string(), "2026-01-02");
        assert_eq!(format_rfc3339_z(zoned.timestamp()), "2026-01-01T15:30:00Z");
        assert_eq!(format_rfc3339_zoned(&zoned), "2026-01-02T00:30:00+09:00");
        assert_eq!(
            parse_rfc3339("2026-01-02t03:04:05.123456789999z").unwrap(),
            parse_rfc3339("2026-01-02 03:04:05.123456789Z").unwrap()
        );
        let zoned = parse_rfc3339_zoned("2026-01-02T23:30:00.123456789999−03:30").unwrap();
        assert_eq!(zoned.date().to_string(), "2026-01-02");
        assert_eq!(
            format_rfc3339_z(zoned.timestamp()),
            "2026-01-03T03:00:00.123456789Z"
        );
        assert_eq!(
            format_rfc3339_zoned(&zoned),
            "2026-01-02T23:30:00.123456789-03:30"
        );
    }

    #[test]
    fn parser_rejects_annotations_missing_fields_and_unrepresentable_seconds() {
        for raw in [
            "2026-01-02T03:04Z",
            "2026-01-02T03:04:05+09",
            "2026-01-02T03:04:05Z[UTC]",
            "2026-01-02T03:04:05+24:00",
            "2026-01-02T03:04:05.Z",
            "2026-01-02T03:04:60Z",
            "2026-02-30T03:04:05Z",
            "2026−01-02T03:04:05Z",
            "2026-01-02T03:04:05−0330",
        ] {
            assert!(parse_rfc3339(raw).is_err(), "accepted {raw}");
        }
    }

    #[test]
    fn stored_timestamp_grammar_remains_separate_from_strict_protocol_inputs() {
        let expected = parse_rfc3339("2026-01-02T03:04:05.123456789Z").unwrap();
        for raw in [
            "2026-01-02 03:04:05.123456789 UTC",
            "2026-01-02 03:04:05.123456789+0000",
            " 2026-1-2 3 : 4 : 5.123456789999 utc ",
            "+002026-01-02T08:34:05.123456789+05:: 30",
            "2026-01-02T03:04:05.123456789−0000",
        ] {
            assert_eq!(parse_stored_timestamp(raw).unwrap(), expected, "{raw}");
            assert!(parse_rfc3339(raw).is_err(), "strict input accepted {raw}");
        }
        for raw in [
            "2026-01-02T03:04:60 UTC",
            "+010000-01-02T03:04:05Z",
            "2026-01-02T03:04:05+000030",
            "2026-01-02T03:04:05Z[UTC]",
        ] {
            assert!(parse_stored_timestamp(raw).is_err(), "accepted {raw}");
        }
        let negative_year = parse_stored_timestamp("-0001-01-02T03:04:05.100Z").unwrap();
        assert_eq!(
            format_rfc3339(negative_year),
            "-0001-01-02T03:04:05.100+00:00"
        );
        assert_eq!(format_rfc3339_z(negative_year), "-0001-01-02T03:04:05.100Z");
    }

    #[test]
    fn typed_serde_preserves_existing_json_and_optional_fields() {
        #[derive(Debug, PartialEq, ::serde::Serialize, ::serde::Deserialize)]
        struct Record {
            #[serde(with = "super::serde")]
            timestamp: Timestamp,
            #[serde(default, with = "super::serde::optional")]
            expiry: Option<Timestamp>,
        }
        let raw = r#"{"timestamp":"2026-01-02T03:04:05.100Z","expiry":null}"#;
        let record: Record = serde_json::from_str(raw).unwrap();
        assert_eq!(serde_json::to_string(&record).unwrap(), raw);
        assert_eq!(
            serde_json::from_str::<Record>(r#"{"timestamp":"2026-01-02T03:04:05.100Z"}"#).unwrap(),
            record
        );
        assert_eq!(
            serde_json::from_str::<Record>(r#"{"timestamp":"2026-1-2 3:4:5.1 UTC","expiry":null}"#)
                .unwrap(),
            record
        );
    }
}
