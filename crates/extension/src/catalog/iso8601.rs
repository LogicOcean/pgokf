//! Defensive ISO 8601 / RFC 3339 timestamp parsing shared across projections.
//!
//! OKF frontmatter (provenance `generated.at`, `verified[].at`, `stale_after`,
//! source `last_modified`, `usage_window`) and the reserved `log.md` activity
//! log both carry producer-authored timestamps that must project to a
//! `timestamptz` without ever aborting the surrounding sync. This module owns
//! the single implementation both use: it parses a restricted ISO 8601 / RFC
//! 3339 instant to Unix-epoch seconds entirely in Rust, so a malformed or
//! calendar-invalid value degrades to `None` (→ SQL `NULL`) instead of throwing
//! from a SQL cast. Callers convert the epoch to `timestamptz` in SQL with
//! `to_timestamp`, exactly as the sync engine already handles filesystem
//! modification times.

/// Whether a Gregorian year is a leap year.
fn is_leap_year(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

/// Number of days in a month, honoring leap years; `None` for an invalid month.
fn days_in_month(year: i64, month: u32) -> Option<u32> {
    Some(match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        _ => return None,
    })
}

/// Days from the Unix epoch (1970-01-01) to a valid civil date, via Howard
/// Hinnant's `days_from_civil` algorithm (valid across the full proleptic
/// Gregorian range).
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = (if year >= 0 { year } else { year - 399 }) / 400;
    let year_of_era = year - era * 400;
    let month = i64::from(month);
    let day = i64::from(day);
    let day_of_year = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// Split the time-and-zone remainder into its time and zone substrings at the
/// first zone marker (`Z`, `z`, `+`, or `-`).
fn split_time_zone(remainder: &str) -> (&str, &str) {
    match remainder.find(['Z', 'z', '+', '-']) {
        Some(index) => (&remainder[..index], &remainder[index..]),
        None => (remainder, ""),
    }
}

/// Split the digits following a zone sign into `(hours, minutes)` substrings.
///
/// Accepts the `HH:MM`, `HHMM`, and bare `HH` (implicit `:00`) shapes; any other
/// length is malformed and yields `None`.
fn split_zone_body(body: &str) -> Option<(&str, &str)> {
    if let Some((hours, minutes)) = body.split_once(':') {
        Some((hours, minutes))
    } else if body.len() == 4 {
        Some((&body[..2], &body[2..]))
    } else if body.len() == 2 {
        Some((body, "0"))
    } else {
        None
    }
}

/// Parse an ISO 8601 zone designator into an offset in seconds east of UTC.
///
/// Accepts `Z`/`z`, `±HH`, `±HHMM`, and `±HH:MM`. An empty designator (a naive
/// timestamp) is treated as UTC. Returns `None` for any malformed zone.
fn parse_zone_offset_secs(zone: &str) -> Option<i64> {
    match zone {
        "" | "Z" | "z" => Some(0),
        _ => {
            let sign = match zone.as_bytes()[0] {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            let (hours, minutes) = split_zone_body(&zone[1..])?;
            let hours: i64 = hours.parse().ok()?;
            let minutes: i64 = minutes.parse().ok()?;
            if !(0..=23).contains(&hours) || !(0..=59).contains(&minutes) {
                return None;
            }
            Some(sign * (hours * 3600 + minutes * 60))
        }
    }
}

/// The parsed clock time plus zone offset of an ISO 8601 timestamp.
struct ClockTime {
    hour: i64,
    minute: i64,
    second: i64,
    fraction: f64,
    offset_secs: i64,
}

/// Parse the seconds field of a clock time into `(whole_seconds, fraction)`.
///
/// A bare `SS` has a zero fraction; `SS.fff` keeps the fractional seconds. A
/// trailing dot or a non-digit fraction is malformed and yields `None`.
fn parse_seconds_field(field: &str) -> Option<(i64, f64)> {
    match field.split_once('.') {
        Some((whole, frac)) => {
            if frac.is_empty() || !frac.bytes().all(|byte| byte.is_ascii_digit()) {
                return None;
            }
            Some((whole.parse().ok()?, format!("0.{frac}").parse().ok()?))
        }
        None => Some((field.parse().ok()?, 0.0)),
    }
}

/// Parse the `HH:MM[:SS[.fff]][zone]` portion following the date.
fn parse_clock_time(time_and_zone: &str) -> Option<ClockTime> {
    let (time, zone) = split_time_zone(time_and_zone);
    let mut fields = time.split(':');
    let hour: i64 = fields.next()?.parse().ok()?;
    let minute: i64 = fields.next()?.parse().ok()?;
    let (second, fraction) = match fields.next() {
        Some(seconds) => parse_seconds_field(seconds)?,
        None => (0, 0.0),
    };
    if fields.next().is_some() {
        return None;
    }
    if !(0..=23).contains(&hour) || !(0..=59).contains(&minute) || !(0..=60).contains(&second) {
        return None;
    }
    Some(ClockTime {
        hour,
        minute,
        second,
        fraction,
        offset_secs: parse_zone_offset_secs(zone)?,
    })
}

/// Parse a restricted ISO 8601 / RFC 3339 timestamp into seconds since the Unix
/// epoch, defensively.
///
/// Accepts a zero-padded `YYYY-MM-DD` date, optionally followed by a `T`/`t`/
/// space separator and an `HH:MM[:SS[.fff]]` time with an optional zone
/// (`Z`, `±HH`, `±HHMM`, `±HH:MM`). A bare date is midnight UTC; a naive time is
/// treated as UTC. Every calendar field is range-checked (month, day-of-month
/// with leap years, hour, minute, second), so a shape-valid but impossible
/// instant such as `2026-02-30` yields `None`. Returning `None` rather than
/// throwing guarantees a malformed producer timestamp projects a SQL `NULL` and
/// never aborts the sync.
#[allow(
    clippy::cast_precision_loss,
    reason = "epoch seconds fit f64 exactly across every timestamp OKF can carry; \
              the value is only ever passed to SQL to_timestamp"
)]
#[must_use]
pub(crate) fn parse_iso8601_epoch(value: &str) -> Option<f64> {
    let text = value.trim();
    let (year, month, day) = parse_civil_date(text)?;
    let clock = parse_time_section(&text[10..])?;

    let seconds = days_from_civil(year, month, day) * 86_400
        + clock.hour * 3600
        + clock.minute * 60
        + clock.second
        - clock.offset_secs;
    Some(seconds as f64 + clock.fraction)
}

/// Parse and calendar-validate the leading zero-padded `YYYY-MM-DD` date.
///
/// Requires at least ten characters with `-` separators at the fixed positions,
/// and range-checks the month and day (with leap years), so a shape-valid but
/// impossible date such as `2026-02-30` yields `None`.
fn parse_civil_date(text: &str) -> Option<(i64, u32, u32)> {
    if text.len() < 10 {
        return None;
    }
    let bytes = text.as_bytes();
    if bytes[4] != b'-' || bytes[7] != b'-' {
        return None;
    }
    let year: i64 = text.get(0..4)?.parse().ok()?;
    let month: u32 = text.get(5..7)?.parse().ok()?;
    let day: u32 = text.get(8..10)?.parse().ok()?;
    if !(1..=12).contains(&month) || day < 1 || day > days_in_month(year, month)? {
        return None;
    }
    Some((year, month, day))
}

/// Parse the optional time section following the date: an empty remainder is
/// midnight UTC, otherwise a `T`/`t`/space separator must precede the clock
/// time.
fn parse_time_section(rest: &str) -> Option<ClockTime> {
    match rest {
        "" => Some(ClockTime {
            hour: 0,
            minute: 0,
            second: 0,
            fraction: 0.0,
            offset_secs: 0,
        }),
        rest => {
            let separator = rest.as_bytes()[0];
            if separator != b'T' && separator != b't' && separator != b' ' {
                return None;
            }
            parse_clock_time(&rest[1..])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::parse_iso8601_epoch;

    #[test]
    fn parse_iso8601_epoch_reads_a_utc_instant() {
        // Arrange: the OKF unix epoch reference plus a known non-zero instant.
        // Act / Assert
        assert_eq!(parse_iso8601_epoch("1970-01-01T00:00:00Z"), Some(0.0));
        // 2026-07-01T12:00:00Z = 1_782_907_200 seconds since the epoch.
        assert_eq!(
            parse_iso8601_epoch("2026-07-01T12:00:00Z"),
            Some(1_782_907_200.0)
        );
    }

    #[test]
    fn parse_iso8601_epoch_applies_the_zone_offset() {
        // Arrange: the same wall clock in UTC and in +02:00 differ by 7200s.
        // Act
        let utc = parse_iso8601_epoch("2026-07-01T12:00:00Z").expect("utc parses");
        let plus_two = parse_iso8601_epoch("2026-07-01T12:00:00+02:00").expect("offset parses");

        // Assert: an eastern offset is earlier in absolute time.
        assert!((utc - plus_two - 7200.0).abs() < f64::EPSILON);
    }

    #[test]
    fn parse_iso8601_epoch_accepts_a_bare_date_as_utc_midnight() {
        // Arrange / Act / Assert
        assert_eq!(
            parse_iso8601_epoch("2026-01-01"),
            parse_iso8601_epoch("2026-01-01T00:00:00Z")
        );
    }

    #[test]
    fn parse_iso8601_epoch_keeps_fractional_seconds() {
        // Arrange / Act
        let epoch = parse_iso8601_epoch("1970-01-01T00:00:00.5Z").expect("fraction parses");

        // Assert
        assert!((epoch - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn parse_iso8601_epoch_rejects_calendar_invalid_instants() {
        // Arrange: shape-valid but impossible dates and out-of-range fields must
        // degrade to None, never throw — a NULL column, not an aborted sync.
        // Act / Assert
        assert_eq!(parse_iso8601_epoch("2026-02-30T00:00:00Z"), None);
        assert_eq!(parse_iso8601_epoch("2026-13-01T00:00:00Z"), None);
        assert_eq!(parse_iso8601_epoch("2026-07-01T25:00:00Z"), None);
        assert_eq!(parse_iso8601_epoch("not-a-timestamp"), None);
        assert_eq!(parse_iso8601_epoch(""), None);
    }

    #[test]
    fn parse_iso8601_epoch_honors_leap_day() {
        // Arrange: 2024 is a leap year, 2026 is not.
        // Act / Assert
        assert!(parse_iso8601_epoch("2024-02-29T00:00:00Z").is_some());
        assert_eq!(parse_iso8601_epoch("2026-02-29T00:00:00Z"), None);
    }
}
