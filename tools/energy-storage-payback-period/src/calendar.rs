//! Which local days and months the paired timeline covers, for the History block.

use std::collections::BTreeMap;

use chrono_tz::Tz;
use rayon::prelude::*;

use ha_recorder::timeutil::{DayNumber, local_parts};

use crate::pair::Paired;

/// Recorded time a day needs to count as a full one, in seconds.
const FULL_DAY_SECONDS: f64 = 20.0 * 3_600.0;

/// Fewest steps worth a rayon task of their own: placing them on the local calendar
/// takes a few hundred microseconds.
const MIN_STEPS_PER_CHUNK: usize = 2_048;

/// The local calendar behind the paired steps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Calendar {
    /// Local days with at least one paired step.
    pub observed_days: u32,
    /// Of those, the days with at least 20 hours of them.
    pub full_days: u32,
    /// Zero-based calendar months with at least one paired step, in order.
    pub months: Vec<usize>,
}

/// Place every paired step on the local calendar of `tz`.
///
/// Finding a step's local day is nearly all the work, so the steps are placed in
/// parallel, and the days each worker saw are merged afterwards. A step's seconds are
/// whole seconds, which a double adds exactly, so the merge cannot change a day's total.
pub fn calendar(paired: &Paired, tz: Tz) -> Calendar {
    let chunk_len = paired
        .steps
        .len()
        .div_ceil(rayon::current_num_threads())
        .max(MIN_STEPS_PER_CHUNK);
    let (days, months) = paired
        .steps
        .par_chunks(chunk_len)
        .map(|steps| {
            let mut days: BTreeMap<DayNumber, f64> = BTreeMap::new();
            let mut months = 0u16;
            for step in steps {
                let parts = local_parts(step.start_ts, tz);
                *days.entry(parts.day).or_default() += step.seconds;
                months |= 1 << (parts.month - 1);
            }
            (days, months)
        })
        .reduce(
            || (BTreeMap::new(), 0),
            |(mut days, months), (more_days, more_months)| {
                for (day, seconds) in more_days {
                    *days.entry(day).or_default() += seconds;
                }
                (days, months | more_months)
            },
        );

    Calendar {
        observed_days: days.len() as u32,
        full_days: days
            .values()
            .filter(|seconds| **seconds >= FULL_DAY_SECONDS)
            .count() as u32,
        months: (0..12).filter(|month| months & (1 << month) != 0).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pair::PairedStep;
    use chrono_tz::Europe::Berlin;
    use ha_testdb::ts;

    fn paired(steps: Vec<PairedStep>) -> Paired {
        Paired {
            observed_seconds: steps.iter().map(|step| step.seconds).sum(),
            steps,
            slot_seconds: 3_600,
            dropped_partial: 0,
            dropped_unpaired: 0,
            import_kwh: 0.0,
            export_kwh: 0.0,
        }
    }

    fn hours(from: &str, count: i64) -> Vec<PairedStep> {
        (0..count)
            .map(|hour| PairedStep {
                start_ts: ts(from) + hour * 3_600,
                seconds: 3_600.0,
                import_kwh: 0.1,
                export_kwh: 0.0,
            })
            .collect()
    }

    #[test]
    fn days_and_months_are_local_ones() {
        // 22:00 UTC on 31 January is 23:00 in Berlin: a step on the last day of January,
        // and the next one already on 1 February.
        let calendar = calendar(&paired(hours("2024-01-31 22:00:00", 2)), Berlin);
        assert_eq!(calendar.observed_days, 2);
        assert_eq!(calendar.full_days, 0);
        assert_eq!(calendar.months, [0, 1]);
    }

    #[test]
    fn a_day_counts_as_full_from_twenty_hours() {
        // Berlin midnight is 23:00 UTC in winter: 20 hours of 10 January, then 19 of the
        // 12th.
        let mut steps = hours("2024-01-09 23:00:00", 20);
        steps.extend(hours("2024-01-11 23:00:00", 19));
        let calendar = calendar(&paired(steps), Berlin);
        assert_eq!(calendar.observed_days, 2);
        assert_eq!(calendar.full_days, 1);
        assert_eq!(calendar.months, [0]);
    }

    #[test]
    fn the_answer_does_not_depend_on_how_the_work_is_split() {
        // Three years of hours, partly half covered, so days straddle the chunks.
        let mut steps = hours("2022-12-31 23:00:00", 3 * 8_760);
        for (index, step) in steps.iter_mut().enumerate() {
            if index % 7 == 0 {
                step.seconds = 1_800.0;
            }
        }
        let paired = paired(steps);
        let expected = rayon::ThreadPoolBuilder::new()
            .num_threads(1)
            .build()
            .unwrap()
            .install(|| calendar(&paired, Berlin));
        for threads in [2, 3, 8] {
            let split = rayon::ThreadPoolBuilder::new()
                .num_threads(threads)
                .build()
                .unwrap()
                .install(|| calendar(&paired, Berlin));
            assert_eq!(split, expected, "{threads} threads");
        }
        assert_eq!(expected.observed_days, 1_095);
        assert_eq!(expected.months, (0..12).collect::<Vec<_>>());
    }

    #[test]
    fn nothing_paired_covers_nothing() {
        let calendar = calendar(&paired(Vec::new()), Berlin);
        assert_eq!(calendar.observed_days, 0);
        assert_eq!(calendar.full_days, 0);
        assert!(calendar.months.is_empty());
    }
}
