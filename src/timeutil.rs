//! Timezone handling: everything Home Assistant stores is UTC, everything the user
//! wants to see ("power at 14:00 in June") is local wall-clock time.

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Datelike, NaiveDate, NaiveDateTime, TimeZone, Timelike, Utc};
use chrono_tz::Tz;

/// The local wall-clock properties of a UTC instant that the aggregation cares about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalParts {
    /// Hour of the day, 0..=23.
    pub hour: u8,
    /// Calendar month, 1..=12.
    pub month: u8,
    /// ISO-8601 week number, 1..=53.
    pub iso_week: u8,
    /// The ISO-8601 week-based year (differs from `year` around New Year).
    pub iso_year: i32,
    /// Calendar year.
    pub year: i32,
}

/// Resolve a timezone name. `None` or `"local"` means "figure out the machine's zone".
///
/// Resolution order for the local zone is `$TZ`, `/etc/timezone`, the `/etc/localtime`
/// symlink target, and finally UTC.
pub fn resolve_timezone(spec: Option<&str>) -> Result<Tz> {
    match spec {
        None | Some("local") | Some("Local") | Some("") => Ok(detect_local_timezone()),
        Some(name) => name.parse::<Tz>().map_err(|_| {
            anyhow!("unknown timezone {name:?}; expected an IANA name like Europe/Berlin")
        }),
    }
}

fn detect_local_timezone() -> Tz {
    if let Ok(tz) = std::env::var("TZ")
        && let Ok(tz) = tz.trim().parse::<Tz>()
    {
        return tz;
    }
    if let Ok(contents) = std::fs::read_to_string("/etc/timezone")
        && let Ok(tz) = contents.trim().parse::<Tz>()
    {
        return tz;
    }
    if let Ok(target) = std::fs::read_link("/etc/localtime")
        && let Some(name) = tz_name_from_zoneinfo_path(&target.to_string_lossy())
        && let Ok(tz) = name.parse::<Tz>()
    {
        return tz;
    }
    Tz::UTC
}

/// Extract `Europe/Berlin` from something like `/usr/share/zoneinfo/Europe/Berlin`.
pub fn tz_name_from_zoneinfo_path(path: &str) -> Option<String> {
    let idx = path.find("zoneinfo/")?;
    let name = &path[idx + "zoneinfo/".len()..];
    let name = name
        .trim_start_matches("posix/")
        .trim_start_matches("right/");
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

/// Local wall-clock properties of a UTC unix timestamp.
pub fn local_parts(ts: i64, tz: Tz) -> LocalParts {
    let local = utc_datetime(ts).with_timezone(&tz);
    let iso = local.iso_week();
    LocalParts {
        hour: local.hour() as u8,
        month: local.month() as u8,
        iso_week: iso.week() as u8,
        iso_year: iso.year(),
        year: local.year(),
    }
}

fn utc_datetime(ts: i64) -> DateTime<Utc> {
    Utc.timestamp_opt(ts, 0)
        .single()
        .unwrap_or_else(|| DateTime::<Utc>::from_timestamp_nanos(0))
}

/// Split the half-open interval `[start, end)` at local hour boundaries and hand each
/// slice, with its duration in seconds, to `f`.
///
/// This is what makes a reading that was in effect from 13:50 to 14:20 contribute 600
/// seconds to hour 13 and 1200 seconds to hour 14. Daylight saving transitions are
/// handled implicitly: the distance to the next local hour boundary is derived from the
/// local minute/second of the current instant, so a repeated hour simply accumulates
/// twice as much weight and a skipped hour accumulates none.
pub fn for_each_hour_slice<F>(start: i64, end: i64, tz: Tz, mut f: F)
where
    F: FnMut(LocalParts, f64),
{
    if end <= start {
        return;
    }
    let mut cursor = start;
    while cursor < end {
        let local = utc_datetime(cursor).with_timezone(&tz);
        let into_hour = i64::from(local.minute()) * 60 + i64::from(local.second());
        // Always at least one second, so the loop is guaranteed to make progress.
        let step = (3600 - into_hour).max(1);
        let next = (cursor + step).min(end);
        let iso = local.iso_week();
        f(
            LocalParts {
                hour: local.hour() as u8,
                month: local.month() as u8,
                iso_week: iso.week() as u8,
                iso_year: iso.year(),
                year: local.year(),
            },
            (next - cursor) as f64,
        );
        cursor = next;
    }
}

/// Parse the textual UTC timestamps used by legacy recorder schemas, e.g.
/// `2023-06-01 12:00:00.000000` or `2023-06-01T12:00:00+00:00`.
pub fn parse_ha_datetime(raw: &str) -> Option<i64> {
    let text = raw.trim();
    if text.is_empty() {
        return None;
    }
    if let Ok(dt) = DateTime::parse_from_rfc3339(text) {
        return Some(dt.timestamp());
    }
    let normalized = text.trim_end_matches('Z').replace('T', " ");
    for format in ["%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%d %H:%M"] {
        if let Ok(naive) = NaiveDateTime::parse_from_str(&normalized, format) {
            return Some(naive.and_utc().timestamp());
        }
    }
    None
}

/// Parse a `YYYY-MM-DD` boundary given on the command line into the UTC timestamp of
/// local midnight on that day.
pub fn parse_local_date(date: &str, tz: Tz) -> Result<i64> {
    let day = NaiveDate::parse_from_str(date.trim(), "%Y-%m-%d")
        .with_context(|| format!("invalid date {date:?}, expected YYYY-MM-DD"))?;
    let naive = day.and_hms_opt(0, 0, 0).expect("midnight is always valid");
    let local = tz
        .from_local_datetime(&naive)
        .earliest()
        // Midnight can be skipped by a DST transition (e.g. Brazil); step forward.
        .or_else(|| {
            tz.from_local_datetime(&day.and_hms_opt(1, 0, 0).expect("01:00 is valid"))
                .earliest()
        })
        .ok_or_else(|| anyhow!("date {date} does not exist in timezone {tz}"))?;
    Ok(local.timestamp())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono_tz::Europe::Berlin;
    use chrono_tz::Tz::UTC;

    fn ts(text: &str) -> i64 {
        parse_ha_datetime(text).expect("test timestamp parses")
    }

    #[test]
    fn resolves_named_timezones() {
        assert_eq!(resolve_timezone(Some("Europe/Berlin")).unwrap(), Berlin);
        assert!(resolve_timezone(Some("Middle/Earth")).is_err());
    }

    #[test]
    fn resolves_local_timezone_from_tz_env() {
        // `resolve_timezone(None)` must never fail, whatever the machine looks like.
        assert!(resolve_timezone(None).is_ok());
        assert!(resolve_timezone(Some("local")).is_ok());
    }

    #[test]
    fn extracts_zone_name_from_localtime_link() {
        assert_eq!(
            tz_name_from_zoneinfo_path("/usr/share/zoneinfo/Europe/Berlin").as_deref(),
            Some("Europe/Berlin")
        );
        assert_eq!(
            tz_name_from_zoneinfo_path("../usr/share/zoneinfo/posix/UTC").as_deref(),
            Some("UTC")
        );
        assert_eq!(tz_name_from_zoneinfo_path("/etc/localtime"), None);
    }

    #[test]
    fn converts_utc_instants_to_local_parts() {
        // 2024-06-21 10:30 UTC is 12:30 in Berlin (CEST, +2).
        let parts = local_parts(ts("2024-06-21 10:30:00"), Berlin);
        assert_eq!(parts.hour, 12);
        assert_eq!(parts.month, 6);
        assert_eq!(parts.year, 2024);
        assert_eq!(parts.iso_week, 25);

        // In winter Berlin is +1.
        let parts = local_parts(ts("2024-12-21 10:30:00"), Berlin);
        assert_eq!(parts.hour, 11);
        assert_eq!(parts.month, 12);
    }

    #[test]
    fn iso_week_wraps_across_the_year_boundary() {
        // 2021-01-01 belongs to ISO week 53 of 2020.
        let parts = local_parts(ts("2021-01-01 12:00:00"), UTC);
        assert_eq!(parts.iso_week, 53);
        assert_eq!(parts.iso_year, 2020);
        assert_eq!(parts.year, 2021);

        // 2019-12-30 already belongs to ISO week 1 of 2020.
        let parts = local_parts(ts("2019-12-30 12:00:00"), UTC);
        assert_eq!(parts.iso_week, 1);
        assert_eq!(parts.iso_year, 2020);
        assert_eq!(parts.year, 2019);
    }

    fn collect_slices(start: i64, end: i64, tz: Tz) -> Vec<(u8, f64)> {
        let mut out = Vec::new();
        for_each_hour_slice(start, end, tz, |parts, seconds| {
            out.push((parts.hour, seconds))
        });
        out
    }

    #[test]
    fn splits_an_interval_at_local_hour_boundaries() {
        // 13:50 -> 14:20 local time in Berlin (summer, so 11:50 -> 12:20 UTC).
        let slices = collect_slices(ts("2024-06-21 11:50:00"), ts("2024-06-21 12:20:00"), Berlin);
        assert_eq!(slices, vec![(13, 600.0), (14, 1200.0)]);
    }

    #[test]
    fn does_not_split_an_interval_inside_one_hour() {
        let slices = collect_slices(ts("2024-06-21 11:05:00"), ts("2024-06-21 11:35:00"), Berlin);
        assert_eq!(slices, vec![(13, 1800.0)]);
    }

    #[test]
    fn ignores_empty_and_inverted_intervals() {
        assert!(collect_slices(1000, 1000, UTC).is_empty());
        assert!(collect_slices(2000, 1000, UTC).is_empty());
    }

    #[test]
    fn slice_durations_always_sum_to_the_interval_length() {
        let start = ts("2024-03-31 00:00:00");
        let end = start + 6 * 3600 + 137;
        let total: f64 = collect_slices(start, end, Berlin)
            .iter()
            .map(|(_, s)| s)
            .sum();
        assert_eq!(total, (end - start) as f64);
    }

    #[test]
    fn spring_forward_skips_the_missing_local_hour() {
        // Berlin jumps 02:00 -> 03:00 local on 2024-03-31, i.e. 01:00 UTC.
        // Cover 00:30 UTC (01:30 local) to 02:30 UTC (04:30 local).
        let slices = collect_slices(ts("2024-03-31 00:30:00"), ts("2024-03-31 02:30:00"), Berlin);
        let hours: Vec<u8> = slices.iter().map(|(h, _)| *h).collect();
        // Local hour 2 does not exist that day.
        assert!(!hours.contains(&2), "hours were {hours:?}");
        assert_eq!(hours, vec![1, 3, 4]);
        assert_eq!(slices[0], (1, 1800.0));
        assert_eq!(slices[1], (3, 3600.0));
        assert_eq!(slices[2], (4, 1800.0));
    }

    #[test]
    fn fall_back_gives_the_repeated_local_hour_double_weight() {
        // Berlin falls back 03:00 -> 02:00 local on 2024-10-27, i.e. 01:00 UTC.
        // Cover 00:00 UTC (02:00 local, CEST) to 03:00 UTC (04:00 local, CET).
        let slices = collect_slices(ts("2024-10-27 00:00:00"), ts("2024-10-27 03:00:00"), Berlin);
        let mut hour_two = 0.0;
        for (hour, seconds) in &slices {
            if *hour == 2 {
                hour_two += seconds;
            }
        }
        assert_eq!(hour_two, 7200.0, "local hour 2 happens twice: {slices:?}");
        let total: f64 = slices.iter().map(|(_, s)| s).sum();
        assert_eq!(total, 3.0 * 3600.0);
    }

    #[test]
    fn parses_recorder_timestamp_formats() {
        assert_eq!(parse_ha_datetime("1970-01-01 00:00:00"), Some(0));
        assert_eq!(parse_ha_datetime("1970-01-01 00:00:00.000000"), Some(0));
        assert_eq!(parse_ha_datetime("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_ha_datetime("1970-01-01T00:00:00+00:00"), Some(0));
        assert_eq!(parse_ha_datetime("1970-01-01T01:00:00+01:00"), Some(0));
        assert_eq!(
            parse_ha_datetime("2023-06-01 12:00:00"),
            Some(1_685_620_800)
        );
        assert_eq!(parse_ha_datetime(""), None);
        assert_eq!(parse_ha_datetime("not a date"), None);
    }

    #[test]
    fn parses_cli_dates_as_local_midnight() {
        // Local midnight in Berlin in summer is 22:00 UTC the previous day.
        assert_eq!(
            parse_local_date("2024-06-21", Berlin).unwrap(),
            ts("2024-06-20 22:00:00")
        );
        assert_eq!(
            parse_local_date("2024-06-21", UTC).unwrap(),
            ts("2024-06-21 00:00:00")
        );
        assert!(parse_local_date("21.06.2024", UTC).is_err());
    }
}
