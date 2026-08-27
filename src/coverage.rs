//! What the recorder actually covered, and where the holes are.
//!
//! A photovoltaic history is rarely a tidy block of years: Home Assistant gets restarted,
//! SD cards fill up, the recorder is purged, an entity is renamed. None of that biases a
//! single column's probabilities - those are normalised by the time actually observed -
//! but it does decide how much weight the numbers deserve, and a reader cannot judge that
//! without being told what is missing.
//!
//! Everything here is a pure function over the loaded samples, so it is exercised
//! entirely by unit tests.

use chrono::{Datelike, NaiveDate};
use chrono_tz::Tz;

use crate::model::{Grouping, Sample};
use crate::timeutil::{DayNumber, day_to_date, local_day, local_parts};

/// A stretch of time between two observations with nothing in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Gap {
    pub start_ts: i64,
    pub end_ts: i64,
}

impl Gap {
    pub fn seconds(&self) -> i64 {
        (self.end_ts - self.start_ts).max(0)
    }
}

/// Find every hole longer than `threshold_seconds` between the first and the last
/// observation.
///
/// Samples are merged first, so overlapping or touching readings do not invent gaps, and
/// the time before the first and after the last reading is not a gap - the recorder was
/// simply not asked about it.
pub fn gaps(samples: &[Sample], threshold_seconds: i64) -> Vec<Gap> {
    let mut spans: Vec<(i64, i64)> = samples
        .iter()
        .filter(|sample| sample.duration() > 0)
        .map(|sample| (sample.start_ts, sample.end_ts))
        .collect();
    if spans.len() < 2 {
        return Vec::new();
    }
    spans.sort_unstable();

    let threshold = threshold_seconds.max(1);
    let mut gaps = Vec::new();
    let mut covered_until = spans[0].1;
    for (start, end) in spans.into_iter().skip(1) {
        if start - covered_until >= threshold {
            gaps.push(Gap {
                start_ts: covered_until,
                end_ts: start,
            });
        }
        covered_until = covered_until.max(end);
    }
    gaps
}

/// How many days of the calendar each facet could have covered, given the span the data
/// runs over.
///
/// This is the denominator behind "11 of 30 days": for a report covering 14 March to
/// 20 August, March can only ever contribute 18 days, and December none at all.
pub fn possible_days_per_facet(
    first_ts: i64,
    last_ts: i64,
    tz: Tz,
    grouping: Grouping,
) -> Vec<u32> {
    let mut possible = vec![0u32; grouping.facet_count()];
    let (first, last) = (
        local_day(first_ts.min(last_ts), tz),
        local_day(last_ts.max(first_ts), tz),
    );
    for day in first..=last {
        let Some(date) = day_to_date(day) else {
            continue;
        };
        let index = match grouping {
            Grouping::Month => date.month() as usize,
            Grouping::Week => date.iso_week().week() as usize,
        };
        if (1..=possible.len()).contains(&index) {
            possible[index - 1] += 1;
        }
    }
    possible
}

/// The overall shape of the available history.
#[derive(Debug, Clone, PartialEq)]
pub struct Coverage {
    /// First and last instant covered by any reading.
    pub first_ts: i64,
    pub last_ts: i64,
    /// Calendar days between them, inclusive.
    pub span_days: u32,
    /// Distinct local days that carry at least one reading.
    pub observed_days: u32,
    /// Of those, the days recorded for most of their hours rather than a sliver.
    pub full_days: u32,
    /// Total observation time, in seconds.
    pub observed_seconds: f64,
    /// Holes longer than the reporting threshold, in chronological order.
    pub gaps: Vec<Gap>,
    /// The threshold those gaps were found with, in seconds.
    pub gap_threshold_seconds: i64,
    /// Facet labels with no data at all, e.g. the months never recorded.
    pub missing_facets: Vec<String>,
}

impl Coverage {
    /// Describe what a set of samples covers. `observed_days` comes from the grid, which
    /// already knows which local days were touched.
    pub fn describe(
        samples: &[Sample],
        observed_days: u32,
        full_days: u32,
        present_facets: &[usize],
        grouping: Grouping,
        tz: Tz,
        gap_threshold_seconds: i64,
    ) -> Option<Self> {
        let first_ts = samples
            .iter()
            .filter(|sample| sample.duration() > 0)
            .map(|sample| sample.start_ts)
            .min()?;
        let last_ts = samples
            .iter()
            .filter(|sample| sample.duration() > 0)
            .map(|sample| sample.end_ts)
            .max()?;

        let first_day = local_day(first_ts, tz);
        // The end instant is exclusive: a run finishing at midnight covered the day before.
        let last_day = local_day(last_ts.saturating_sub(1).max(first_ts), tz);

        let missing_facets = (0..grouping.facet_count())
            .filter(|index| !present_facets.contains(index))
            .filter(|index| facet_is_reachable(*index, first_day, last_day, grouping))
            .map(|index| grouping.facet_short_label(index))
            .collect();

        Some(Self {
            first_ts,
            last_ts,
            span_days: (last_day - first_day + 1).max(1) as u32,
            observed_days,
            full_days,
            observed_seconds: samples.iter().map(|sample| sample.duration() as f64).sum(),
            gaps: gaps(samples, gap_threshold_seconds),
            gap_threshold_seconds,
            missing_facets,
        })
    }

    /// Share of the calendar span that carries any data at all, 0..=1.
    pub fn day_fraction(&self) -> f64 {
        if self.span_days == 0 {
            return 0.0;
        }
        f64::from(self.observed_days) / f64::from(self.span_days)
    }

    /// The longest hole, if any.
    pub fn longest_gap(&self) -> Option<&Gap> {
        self.gaps.iter().max_by_key(|gap| gap.seconds())
    }

    /// Total time inside reported gaps, in seconds.
    pub fn missing_seconds(&self) -> i64 {
        self.gaps.iter().map(Gap::seconds).sum()
    }

    /// First and last local date, for labels.
    pub fn local_dates(&self, tz: Tz) -> (Option<NaiveDate>, Option<NaiveDate>) {
        (
            day_to_date(local_day(self.first_ts, tz)),
            day_to_date(local_day(
                self.last_ts.saturating_sub(1).max(self.first_ts),
                tz,
            )),
        )
    }

    /// Whether the span reaches all the way round the seasons.
    ///
    /// Below a year, every facet rests on a single season rather than an average of
    /// several, and whole months are missing simply because they have not happened yet.
    pub fn covers_full_year(&self) -> bool {
        self.span_days >= 365
    }

    /// Share of the span lost to reported outages, 0..=1.
    ///
    /// This is the honest measure of how holed the history is: unlike a day count it
    /// cannot be inflated by an hour of data spilling over a local midnight, and unlike
    /// raw observation time it is not dragged down by an inverter that sleeps at night.
    pub fn missing_fraction(&self) -> f64 {
        let span_seconds = f64::from(self.span_days) * 86_400.0;
        if span_seconds <= 0.0 {
            return 0.0;
        }
        (self.missing_seconds() as f64 / span_seconds).clamp(0.0, 1.0)
    }

    /// Whether large parts of the span were never recorded.
    pub fn is_sparse(&self) -> bool {
        self.observed_days < 30 || self.missing_fraction() > 0.15
    }

    /// Whether the reader should be warned about the history at all.
    pub fn needs_caution(&self) -> bool {
        !self.covers_full_year() || self.is_sparse()
    }
}

/// Could a facet have been recorded at all between two days?
///
/// Without this a three week report would claim that the other eleven months are
/// "missing", which is noise rather than information.
fn facet_is_reachable(
    index: usize,
    first_day: DayNumber,
    last_day: DayNumber,
    grouping: Grouping,
) -> bool {
    // A full year in the data means every facet was reachable.
    if last_day - first_day >= 365 {
        return true;
    }
    (first_day..=last_day).any(|day| {
        day_to_date(day).is_some_and(|date| match grouping {
            Grouping::Month => date.month() as usize == index + 1,
            Grouping::Week => date.iso_week().week() as usize == index + 1,
        })
    })
}

/// The facet a UTC instant belongs to, used when checking a window against a grouping.
pub fn facet_of(ts: i64, tz: Tz, grouping: Grouping) -> Option<usize> {
    grouping.facet_index(&local_parts(ts, tz))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono_tz::Europe::Berlin;
    use chrono_tz::Tz::UTC;

    use crate::timeutil::parse_ha_datetime;

    fn ts(text: &str) -> i64 {
        parse_ha_datetime(text).expect("test timestamp parses")
    }

    const HOUR: i64 = 3_600;
    const DAY: i64 = 86_400;

    #[test]
    fn continuous_data_has_no_gaps() {
        let samples: Vec<Sample> = (0..48)
            .map(|hour| Sample::new(hour * HOUR, (hour + 1) * HOUR, 100.0))
            .collect();
        assert!(gaps(&samples, HOUR).is_empty());
    }

    #[test]
    fn a_hole_longer_than_the_threshold_is_reported() {
        let samples = vec![
            Sample::new(0, HOUR, 100.0),
            Sample::new(10 * HOUR, 11 * HOUR, 100.0),
        ];
        assert_eq!(
            gaps(&samples, 6 * HOUR),
            vec![Gap {
                start_ts: HOUR,
                end_ts: 10 * HOUR
            }]
        );
        assert_eq!(gaps(&samples, 6 * HOUR)[0].seconds(), 9 * HOUR);
    }

    #[test]
    fn a_hole_below_the_threshold_is_ignored() {
        let samples = vec![
            Sample::new(0, HOUR, 100.0),
            Sample::new(3 * HOUR, 4 * HOUR, 100.0),
        ];
        assert!(gaps(&samples, 6 * HOUR).is_empty());
        assert_eq!(gaps(&samples, 2 * HOUR).len(), 1);
    }

    #[test]
    fn overlapping_and_touching_samples_do_not_invent_gaps() {
        // A long sample that swallows the next two, then a real hole.
        let samples = vec![
            Sample::new(0, 10 * HOUR, 100.0),
            Sample::new(HOUR, 2 * HOUR, 100.0),
            Sample::new(2 * HOUR, 3 * HOUR, 100.0),
            Sample::new(30 * HOUR, 31 * HOUR, 100.0),
        ];
        let gaps = gaps(&samples, 6 * HOUR);
        assert_eq!(gaps.len(), 1);
        assert_eq!(gaps[0].start_ts, 10 * HOUR);
        assert_eq!(gaps[0].end_ts, 30 * HOUR);
    }

    #[test]
    fn unsorted_input_is_handled() {
        let samples = vec![
            Sample::new(30 * HOUR, 31 * HOUR, 100.0),
            Sample::new(0, HOUR, 100.0),
        ];
        assert_eq!(gaps(&samples, 6 * HOUR).len(), 1);
    }

    #[test]
    fn the_time_before_and_after_the_data_is_not_a_gap() {
        let samples = vec![Sample::new(1_000 * DAY, 1_000 * DAY + HOUR, 100.0)];
        assert!(gaps(&samples, HOUR).is_empty());
        assert!(gaps(&[], HOUR).is_empty());
    }

    #[test]
    fn zero_length_samples_are_ignored() {
        let samples = vec![
            Sample::new(0, HOUR, 100.0),
            Sample::new(5 * HOUR, 5 * HOUR, 100.0),
            Sample::new(30 * HOUR, 31 * HOUR, 100.0),
        ];
        let gaps = gaps(&samples, 6 * HOUR);
        assert_eq!(gaps.len(), 1, "the empty sample must not split the hole");
        assert_eq!(gaps[0].seconds(), 29 * HOUR);
    }

    #[test]
    fn possible_days_count_the_calendar_the_span_reaches() {
        // 14 March to 20 April: 18 days of March, 20 of April, nothing else.
        let possible = possible_days_per_facet(
            ts("2025-03-14 00:00:00"),
            ts("2025-04-20 12:00:00"),
            UTC,
            Grouping::Month,
        );
        assert_eq!(possible[2], 18, "March");
        assert_eq!(possible[3], 20, "April");
        assert_eq!(possible[0], 0, "January was never in range");
        assert_eq!(possible.len(), 12);
    }

    #[test]
    fn possible_days_include_the_leap_day() {
        let possible = possible_days_per_facet(
            ts("2024-02-01 00:00:00"),
            ts("2024-02-29 23:00:00"),
            UTC,
            Grouping::Month,
        );
        assert_eq!(possible[1], 29, "February 2024 has 29 days");
    }

    #[test]
    fn possible_days_work_per_iso_week() {
        let possible = possible_days_per_facet(
            ts("2025-03-14 00:00:00"),
            ts("2025-03-27 12:00:00"),
            UTC,
            Grouping::Week,
        );
        let total: u32 = possible.iter().sum();
        assert_eq!(total, 14, "two weeks of days");
        assert!(possible.iter().filter(|days| **days > 0).count() >= 2);
    }

    #[test]
    fn possible_days_follow_the_local_timezone() {
        // 22:30 UTC on 31 May is already 1 June in Berlin.
        let possible = possible_days_per_facet(
            ts("2025-05-31 22:30:00"),
            ts("2025-05-31 23:30:00"),
            Berlin,
            Grouping::Month,
        );
        assert_eq!(possible[4], 0, "May, locally, was never reached");
        assert_eq!(possible[5], 1, "June");
    }

    fn describe(samples: &[Sample], present: &[usize]) -> Coverage {
        Coverage::describe(samples, 2, 2, present, Grouping::Month, UTC, 24 * HOUR)
            .expect("samples cover something")
    }

    #[test]
    fn coverage_describes_the_span_and_the_holes() {
        let base = ts("2025-06-01 00:00:00");
        let samples = vec![
            Sample::new(base, base + HOUR, 100.0),
            Sample::new(base + 9 * DAY, base + 9 * DAY + HOUR, 100.0),
        ];
        let coverage = describe(&samples, &[5]);

        assert_eq!(coverage.first_ts, base);
        assert_eq!(coverage.span_days, 10);
        assert_eq!(coverage.observed_days, 2);
        assert_eq!(coverage.observed_seconds, 2.0 * HOUR as f64);
        assert_eq!(coverage.gaps.len(), 1);
        assert_eq!(coverage.longest_gap().unwrap().seconds(), 9 * DAY - HOUR);
        assert_eq!(coverage.missing_seconds(), 9 * DAY - HOUR);
        assert!((coverage.day_fraction() - 0.2).abs() < 1e-9);
        assert!(coverage.is_sparse(), "two days out of ten is sparse");
        assert!(!coverage.covers_full_year());
        assert!(coverage.needs_caution());
    }

    #[test]
    fn coverage_names_only_the_facets_that_were_reachable() {
        // Ten days of June: the other months were never in range, so they are not
        // "missing", they were never possible.
        let base = ts("2025-06-01 00:00:00");
        let samples = vec![Sample::new(base, base + 10 * DAY, 100.0)];
        let coverage = describe(&samples, &[5]);
        assert!(
            coverage.missing_facets.is_empty(),
            "unexpected {:?}",
            coverage.missing_facets
        );

        // With a year of span, a month that carries nothing really is missing.
        let samples = vec![Sample::new(base, base + 400 * DAY, 100.0)];
        let coverage = describe(&samples, &[5]);
        assert!(coverage.missing_facets.contains(&"Jan".to_string()));
        assert!(!coverage.missing_facets.contains(&"Jun".to_string()));
    }

    #[test]
    fn coverage_reports_the_local_dates_it_spans() {
        let samples = vec![Sample::new(
            ts("2025-03-14 08:00:00"),
            ts("2025-03-20 00:00:00"),
            100.0,
        )];
        let coverage = describe(&samples, &[2]);
        let (first, last) = coverage.local_dates(UTC);
        assert_eq!(first.unwrap().to_string(), "2025-03-14");
        // The last instant is exclusive: midnight belongs to the 19th.
        assert_eq!(last.unwrap().to_string(), "2025-03-19");
    }

    #[test]
    fn coverage_needs_at_least_one_real_sample() {
        assert!(Coverage::describe(&[], 0, 0, &[], Grouping::Month, UTC, HOUR).is_none());
        let empty = vec![Sample::new(10, 10, 5.0)];
        assert!(Coverage::describe(&empty, 0, 0, &[], Grouping::Month, UTC, HOUR).is_none());
    }

    #[test]
    fn dense_history_is_not_thin() {
        let base = ts("2024-01-01 00:00:00");
        let samples = vec![Sample::new(base, base + 200 * DAY, 100.0)];
        let coverage =
            Coverage::describe(&samples, 200, 200, &[0], Grouping::Month, UTC, 24 * HOUR)
                .expect("coverage");
        assert!(!coverage.is_sparse(), "unbroken data is not sparse");
        assert!(coverage.gaps.is_empty());
        // ... but two hundred days is still less than a year of seasons.
        assert!(!coverage.covers_full_year());
        assert!(coverage.needs_caution());
    }

    #[test]
    fn a_solid_year_needs_no_caution() {
        let base = ts("2024-01-01 00:00:00");
        let samples = vec![Sample::new(base, base + 400 * DAY, 100.0)];
        let coverage = Coverage::describe(&samples, 400, 400, &[], Grouping::Month, UTC, 24 * HOUR)
            .expect("coverage");
        assert!(coverage.covers_full_year());
        assert!(!coverage.is_sparse());
        assert!(!coverage.needs_caution());
    }

    #[test]
    fn a_long_but_patchy_history_is_still_sparse() {
        // Two years of span, but the recorder was only up for a hundred days of it.
        let base = ts("2024-01-01 00:00:00");
        let samples = vec![
            Sample::new(base, base + 100 * DAY, 100.0),
            Sample::new(base + 700 * DAY, base + 701 * DAY, 100.0),
        ];
        let coverage = Coverage::describe(&samples, 101, 101, &[], Grouping::Month, UTC, 24 * HOUR)
            .expect("coverage");
        assert!(coverage.covers_full_year());
        assert!(coverage.is_sparse());
        assert!(coverage.needs_caution());
    }

    #[test]
    fn facets_of_instants_follow_the_grouping() {
        assert_eq!(
            facet_of(ts("2025-06-15 12:00:00"), UTC, Grouping::Month),
            Some(5)
        );
        assert_eq!(
            facet_of(ts("2025-01-02 12:00:00"), UTC, Grouping::Week),
            Some(0)
        );
    }
}
