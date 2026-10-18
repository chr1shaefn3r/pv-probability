//! Daily mean temperatures: the unit degree days are counted in.

use std::collections::BTreeMap;

use chrono::{NaiveDate, NaiveTime, TimeZone};
use chrono_tz::Tz;

use ha_recorder::Sample;
use ha_recorder::timeutil::{DayNumber, day_to_date, for_each_hour_slice, local_day};
use rayon::prelude::*;

/// One local day's mean temperature, and how much of the day stands behind it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DayMean {
    pub day: DayNumber,
    /// Time-weighted mean over the part of the day the sensor covered, in °C.
    pub mean: f64,
    /// Seconds of the day the sensor covered.
    pub covered_seconds: f64,
    /// Length of the local day: 86 400 seconds, or an hour less or more when the clocks
    /// change.
    pub day_seconds: f64,
}

impl DayMean {
    /// Share of the day the sensor covered, 0 to 1.
    pub fn coverage(&self) -> f64 {
        (self.covered_seconds / self.day_seconds).clamp(0.0, 1.0)
    }

    pub fn date(&self) -> Option<NaiveDate> {
        day_to_date(self.day)
    }
}

/// Fewest samples worth a rayon task of their own: placing them on the local calendar
/// takes a few hundred microseconds.
const MIN_SAMPLES_PER_CHUNK: usize = 2_048;

/// The time-weighted mean of every local day the samples touch, in day order.
///
/// Each reading counts for as long as it held, split at local midnight, so an hourly
/// statistic and a chatty raw history of the same weather give the same mean - and a day
/// with its night missing is visible as such in [`DayMean::coverage`] instead of
/// silently reading warm.
///
/// The samples are summed in parallel, cut into runs of whole local days so that every
/// day is still summed by one worker, in the order of the samples - the same additions,
/// to the last bit, as one pass over them all. Should a day turn up in two runs anyway,
/// the samples being out of order, it is all done again in that one pass.
pub fn daily_means(samples: &[Sample], tz: Tz) -> Vec<DayMean> {
    let runs = whole_day_runs(samples, tz, rayon::current_num_threads());
    let sums: Vec<Vec<(DayNumber, (f64, f64))>> = runs
        .par_iter()
        .map(|run| day_sums(run, tz).into_iter().collect())
        .collect();
    let mut days: Vec<(DayNumber, (f64, f64))> = sums.into_iter().flatten().collect();
    if !days.windows(2).all(|pair| pair[0].0 < pair[1].0) {
        days = day_sums(samples, tz).into_iter().collect();
    }

    days.into_par_iter()
        .filter(|(_, (_, covered))| *covered > 0.0)
        .map(|(day, (weighted, covered))| DayMean {
            day,
            mean: weighted / covered,
            covered_seconds: covered,
            day_seconds: day_seconds(day, tz),
        })
        .collect()
}

/// Every local day's time-weighted sum and covered seconds, in one pass.
fn day_sums(samples: &[Sample], tz: Tz) -> BTreeMap<DayNumber, (f64, f64)> {
    let mut days: BTreeMap<DayNumber, (f64, f64)> = BTreeMap::new();
    for sample in samples {
        if sample.duration() <= 0 || !sample.value.is_finite() {
            continue;
        }
        for_each_hour_slice(sample.start_ts, sample.end_ts, tz, |parts, seconds| {
            let (weighted, covered) = days.entry(parts.day).or_default();
            *weighted += sample.value * seconds;
            *covered += seconds;
        });
    }
    days
}

/// Cut time-ordered samples into up to `runs` runs that each start at a local midnight,
/// with no sample reaching across it.
///
/// A cut that cannot be made cleanly near where it should go - the clocks skip that
/// midnight, or a reading spans it - is left out, so the runs may be fewer.
fn whole_day_runs(samples: &[Sample], tz: Tz, runs: usize) -> Vec<&[Sample]> {
    let runs = (samples.len() / MIN_SAMPLES_PER_CHUNK).clamp(1, runs.max(1));
    let mut cuts = vec![0];
    for run in 1..runs {
        let nominal = samples.len() * run / runs;
        let Some(midnight) = midnight_after(samples[nominal].start_ts, tz) else {
            continue;
        };
        // The first sample from that midnight on, if the one before it is over by then.
        let cut = samples.partition_point(|sample| sample.start_ts < midnight);
        let clean = cut > 0 && cut < samples.len() && samples[cut - 1].end_ts <= midnight;
        if clean && cut > *cuts.last().unwrap_or(&0) {
            cuts.push(cut);
        }
    }
    cuts.push(samples.len());
    cuts.windows(2)
        .map(|pair| &samples[pair[0]..pair[1]])
        .collect()
}

/// The first local midnight after the instant `ts`.
fn midnight_after(ts: i64, tz: Tz) -> Option<i64> {
    let tomorrow = day_to_date(local_day(ts, tz))?.succ_opt()?;
    tz.from_local_datetime(&tomorrow.and_time(NaiveTime::MIN))
        .earliest()
        .map(|instant| instant.timestamp())
}

/// Length of a local day in seconds, from one local midnight to the next.
pub fn day_seconds(day: DayNumber, tz: Tz) -> f64 {
    let midnight = |date: NaiveDate| {
        tz.from_local_datetime(&date.and_time(NaiveTime::MIN))
            .earliest()
            .map(|instant| instant.timestamp())
    };
    let start = day_to_date(day).and_then(midnight);
    let end = day_to_date(day)
        .and_then(|date| date.succ_opt())
        .and_then(midnight);
    match (start, end) {
        (Some(start), Some(end)) if end > start => (end - start) as f64,
        // A zone whose clocks skip midnight itself: call it a plain day.
        _ => 86_400.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono_tz::Europe::Berlin;
    use chrono_tz::Tz::UTC;
    use ha_testdb::ts;

    fn hourly(start: &str, values: &[f64]) -> Vec<Sample> {
        values
            .iter()
            .enumerate()
            .map(|(hour, value)| {
                let from = ts(start) + hour as i64 * 3_600;
                Sample::new(from, from + 3_600, *value)
            })
            .collect()
    }

    #[test]
    fn a_day_is_the_time_weighted_mean_of_its_readings() {
        // Twelve hours at 0 °C, then twelve at 10 °C.
        let mut values = vec![0.0; 12];
        values.extend([10.0; 12]);
        let days = daily_means(&hourly("2024-01-10 00:00:00", &values), UTC);
        assert_eq!(days.len(), 1);
        assert_eq!(days[0].mean, 5.0);
        assert_eq!(days[0].coverage(), 1.0);
        assert_eq!(days[0].date(), NaiveDate::from_ymd_opt(2024, 1, 10));

        // A reading counts for as long as it held, not once per row.
        let uneven = vec![
            Sample::new(ts("2024-01-10 00:00:00"), ts("2024-01-10 18:00:00"), 0.0),
            Sample::new(ts("2024-01-10 18:00:00"), ts("2024-01-10 18:10:00"), 30.0),
            Sample::new(ts("2024-01-10 18:10:00"), ts("2024-01-11 00:00:00"), 0.0),
        ];
        let day = daily_means(&uneven, UTC)[0];
        assert!((day.mean - 30.0 * 600.0 / 86_400.0).abs() < 1e-9);
    }

    #[test]
    fn days_are_cut_at_local_midnight() {
        // 22:00 to 02:00 UTC is 23:00 to 03:00 in Berlin in winter: one hour on the 10th.
        let samples = vec![Sample::new(
            ts("2024-01-10 22:00:00"),
            ts("2024-01-11 02:00:00"),
            4.0,
        )];
        let days = daily_means(&samples, Berlin);
        assert_eq!(days.len(), 2);
        assert_eq!(days[0].covered_seconds, 3_600.0);
        assert_eq!(days[1].covered_seconds, 3.0 * 3_600.0);
    }

    #[test]
    fn a_day_with_hours_missing_says_so() {
        let days = daily_means(&hourly("2024-01-10 00:00:00", &[3.0; 18]), UTC);
        assert_eq!(days[0].mean, 3.0);
        assert_eq!(days[0].coverage(), 0.75);
    }

    #[test]
    fn days_when_the_clocks_change_are_measured_by_the_clock() {
        let spring = ha_recorder::timeutil::local_day(ts("2024-03-31 12:00:00"), Berlin);
        let autumn = ha_recorder::timeutil::local_day(ts("2024-10-27 12:00:00"), Berlin);
        assert_eq!(day_seconds(spring, Berlin), 23.0 * 3_600.0);
        assert_eq!(day_seconds(autumn, Berlin), 25.0 * 3_600.0);
        assert_eq!(day_seconds(spring, UTC), 86_400.0);

        // So 23 recorded hours are the whole of the short day.
        let days = daily_means(&hourly("2024-03-30 23:00:00", &[8.0; 23]), Berlin);
        let short = days.iter().find(|day| day.day == spring).unwrap();
        assert_eq!(short.coverage(), 1.0);
    }

    /// Readings every `step` seconds from `start` with a value that never repeats a
    /// pattern for long, so a different order of additions would show in the last bits.
    fn uneven(start: &str, count: i64, step: i64) -> Vec<Sample> {
        (0..count)
            .map(|index| {
                let from = ts(start) + index * step;
                let value = (index as f64 * 0.731).sin() * 7.3 + (index % 13) as f64 * 0.917;
                Sample::new(from, from + step, value)
            })
            .collect()
    }

    #[test]
    fn summing_in_parallel_changes_no_bit_of_any_day() {
        let cases = [
            // Hours across two changes of the clocks.
            (uneven("2023-01-01 00:00:00", 17_520, 3_600), Berlin),
            // Raw readings every 97 seconds, none of them on a midnight.
            (uneven("2024-03-20 00:00:13", 40_000, 97), Berlin),
            (uneven("2024-03-20 00:00:13", 40_000, 97), UTC),
        ];
        for (samples, tz) in cases {
            let one_pass: Vec<DayMean> = day_sums(&samples, tz)
                .into_iter()
                .map(|(day, (weighted, covered))| DayMean {
                    day,
                    mean: weighted / covered,
                    covered_seconds: covered,
                    day_seconds: day_seconds(day, tz),
                })
                .collect();
            for threads in [1, 2, 3, 8] {
                let parallel = rayon::ThreadPoolBuilder::new()
                    .num_threads(threads)
                    .build()
                    .unwrap()
                    .install(|| daily_means(&samples, tz));
                assert_eq!(parallel.len(), one_pass.len());
                for (left, right) in parallel.iter().zip(&one_pass) {
                    assert_eq!(left.day, right.day);
                    assert_eq!(
                        left.mean.to_bits(),
                        right.mean.to_bits(),
                        "{threads} threads"
                    );
                    assert_eq!(left.covered_seconds, right.covered_seconds);
                }
            }
        }
    }

    #[test]
    fn runs_start_at_local_midnights() {
        let samples = uneven("2024-01-01 00:00:00", 24 * 400, 3_600);
        let runs = whole_day_runs(&samples, Berlin, 4);
        assert_eq!(runs.len(), 4);
        assert_eq!(
            runs.iter().map(|run| run.len()).sum::<usize>(),
            samples.len()
        );
        for run in &runs[1..] {
            let first = run[0].start_ts;
            assert_eq!(
                midnight_after(first - 1, Berlin),
                Some(first),
                "a run opens a day"
            );
        }
        // Too few samples to be worth cutting.
        assert_eq!(whole_day_runs(&samples[..100], Berlin, 4).len(), 1);
    }

    #[test]
    fn readings_out_of_order_are_summed_in_one_pass() {
        let mut samples = uneven("2024-01-01 00:00:00", 24 * 400, 3_600);
        // A late reading for the first day, at the very end.
        samples.push(Sample::new(
            ts("2024-01-01 05:00:00"),
            ts("2024-01-01 05:30:00"),
            40.0,
        ));
        let means = rayon::ThreadPoolBuilder::new()
            .num_threads(4)
            .build()
            .unwrap()
            .install(|| daily_means(&samples, UTC));
        let first = means[0];
        assert_eq!(first.covered_seconds, 86_400.0 + 1_800.0);
        assert_eq!(means.len(), 400);
        assert!(means.windows(2).all(|pair| pair[0].day < pair[1].day));
    }

    #[test]
    fn empty_and_unreadable_samples_contribute_nothing() {
        let samples = vec![
            Sample::new(ts("2024-01-10 00:00:00"), ts("2024-01-10 00:00:00"), 5.0),
            Sample::new(
                ts("2024-01-10 01:00:00"),
                ts("2024-01-10 02:00:00"),
                f64::NAN,
            ),
        ];
        assert!(daily_means(&samples, UTC).is_empty());
    }
}
