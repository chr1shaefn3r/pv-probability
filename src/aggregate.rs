//! Folding samples into a grid (in parallel) and turning weights into probabilities.

use chrono_tz::Tz;
use rayon::prelude::*;

use crate::model::{BucketSpec, DayWindow, Grid, Grouping, HOURS_PER_DAY, Metric, Sample};
use crate::timeutil::for_each_hour_slice;

/// Samples per rayon work unit. Each work unit allocates one partial [`Grid`], so this
/// trades allocation overhead against parallelism; a few tens of thousands of samples
/// per chunk keeps both small.
const CHUNK_SIZE: usize = 32_768;

/// The span of local days the samples touch, which sizes the grid's day bitset.
pub fn day_window(samples: &[Sample], tz: Tz) -> DayWindow {
    let span = samples
        .par_iter()
        .filter(|sample| sample.duration() > 0)
        .map(|sample| (sample.start_ts, sample.end_ts))
        .reduce_with(|left, right| (left.0.min(right.0), left.1.max(right.1)));
    match span {
        // The last instant is exclusive, so a sample ending exactly at midnight must not
        // stretch the window into the next day.
        Some((first, last)) => DayWindow::from_span(first, last.saturating_sub(1).max(first), tz),
        None => DayWindow::empty(),
    }
}

/// Fold every sample into a facet × hour × bucket grid of observation seconds.
///
/// Grids are additive, so disjoint chunks are folded independently and merged.
pub fn build_grid(samples: &[Sample], grouping: Grouping, buckets: BucketSpec, tz: Tz) -> Grid {
    let days = day_window(samples, tz);
    samples
        .par_chunks(CHUNK_SIZE)
        .map(|chunk| {
            let mut grid = Grid::new(grouping, buckets, days);
            for sample in chunk {
                accumulate(&mut grid, sample, grouping, tz);
            }
            grid
        })
        .reduce(|| Grid::new(grouping, buckets, days), Grid::merge)
}

/// Sequential equivalent of [`build_grid`], kept for tests and tiny inputs.
pub fn build_grid_sequential(
    samples: &[Sample],
    grouping: Grouping,
    buckets: BucketSpec,
    tz: Tz,
) -> Grid {
    let mut grid = Grid::new(grouping, buckets, day_window(samples, tz));
    for sample in samples {
        accumulate(&mut grid, sample, grouping, tz);
    }
    grid
}

fn accumulate(grid: &mut Grid, sample: &Sample, grouping: Grouping, tz: Tz) {
    if sample.duration() <= 0 || !sample.watts.is_finite() {
        return;
    }
    let watts = sample.watts;
    for_each_hour_slice(sample.start_ts, sample.end_ts, tz, |parts, seconds| {
        if let Some(facet) = grouping.facet_index(&parts) {
            grid.add(facet, usize::from(parts.hour), watts, seconds, parts.day);
        }
    });
}

/// Pick a sensible top of the watt axis: the `quantile` of the time-weighted reading
/// distribution, rounded up to the next whole step.
///
/// Using a quantile rather than the raw maximum keeps a single glitchy spike from
/// stretching the axis over empty space; everything above it still shows up in the
/// open-ended top bucket.
pub fn suggest_max_watts(samples: &[Sample], step: f64, quantile: f64) -> f64 {
    let mut weighted: Vec<(f64, f64)> = samples
        .par_iter()
        .filter(|sample| sample.duration() > 0 && sample.watts.is_finite() && sample.watts > 0.0)
        .map(|sample| (sample.watts, sample.duration() as f64))
        .collect();

    if weighted.is_empty() {
        return step;
    }
    weighted.par_sort_unstable_by(|a, b| a.0.total_cmp(&b.0));

    let total: f64 = weighted.iter().map(|(_, weight)| weight).sum();
    let target = total * quantile.clamp(0.0, 1.0);
    let mut running = 0.0;
    let mut cutoff = weighted[weighted.len() - 1].0;
    for (watts, weight) in &weighted {
        running += weight;
        if running >= target {
            cutoff = *watts;
            break;
        }
    }

    let steps = (cutoff / step).ceil().max(1.0);
    steps * step
}

/// How much evidence stands behind one column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnStatus {
    /// The recorder never covered this hour: nothing is known about it.
    Empty,
    /// Observed, but on fewer than `--min-days` distinct days.
    Thin,
    /// Backed by at least `--min-days` distinct days.
    Sufficient,
}

impl ColumnStatus {
    /// Whether the probabilities should be drawn as colour rather than masked.
    pub fn is_sufficient(self) -> bool {
        matches!(self, ColumnStatus::Sufficient)
    }
}

/// One hour-of-day column of a facet.
#[derive(Debug, Clone, PartialEq)]
pub struct Column {
    pub hour: usize,
    /// Probability per bucket, following the analysis [`Metric`].
    pub values: Vec<f64>,
    /// Observation time behind the column, in seconds.
    pub weight_seconds: f64,
    /// Number of source readings behind the column.
    pub samples: u64,
    /// Distinct local days behind the column - the evidence that actually counts.
    pub days: u32,
    /// Whether there is enough of that evidence to colour the column in.
    pub status: ColumnStatus,
}

/// One heatmap: a single month or ISO week.
#[derive(Debug, Clone, PartialEq)]
pub struct Facet {
    pub index: usize,
    pub label: String,
    pub short_label: String,
    pub columns: Vec<Column>,
    pub weight_seconds: f64,
    pub samples: u64,
    /// Distinct local days this facet saw at any hour.
    pub days: u32,
    /// Days of the calendar this facet could have covered, given the data's span.
    pub possible_days: u32,
}

impl Facet {
    /// Probability for one hour/bucket cell.
    pub fn value(&self, hour: usize, bucket: usize) -> f64 {
        self.columns
            .get(hour)
            .and_then(|column| column.values.get(bucket))
            .copied()
            .unwrap_or(0.0)
    }
}

/// Everything the renderer needs.
#[derive(Debug, Clone, PartialEq)]
pub struct Analysis {
    pub grouping: Grouping,
    pub buckets: BucketSpec,
    pub metric: Metric,
    /// Distinct days a column needs before it is drawn as colour.
    pub min_days: u32,
    /// Facets that carry data, in calendar order.
    pub facets: Vec<Facet>,
    pub total_weight_seconds: f64,
    pub total_samples: u64,
    /// Distinct local days observed anywhere.
    pub observed_days: u32,
}

impl Analysis {
    pub fn facet(&self, index: usize) -> Option<&Facet> {
        self.facets.iter().find(|facet| facet.index == index)
    }
}

/// Turn accumulated weights into probabilities.
///
/// For [`Metric::Exceedance`] a cell is `P(watts >= lower edge of the bucket)`, computed
/// as a reverse cumulative sum along the bucket axis; for [`Metric::Density`] it is the
/// share of the column that landed in that bucket.
/// `possible_days` says, per facet, how many days of the calendar the data's span could
/// have covered; pass an empty slice when that is not known.
pub fn analyse(grid: &Grid, metric: Metric, min_days: u32, possible_days: &[u32]) -> Analysis {
    let grouping = grid.grouping();
    let buckets = *grid.buckets();

    let facets = grid
        .non_empty_facets()
        .into_par_iter()
        .map(|index| {
            let columns: Vec<Column> = (0..HOURS_PER_DAY)
                .map(|hour| column_probabilities(grid, index, hour, metric, min_days))
                .collect();
            Facet {
                index,
                label: grouping.facet_label(index),
                short_label: grouping.facet_short_label(index),
                weight_seconds: columns.iter().map(|column| column.weight_seconds).sum(),
                samples: columns.iter().map(|column| column.samples).sum(),
                days: grid.facet_days(index),
                possible_days: possible_days.get(index).copied().unwrap_or(0),
                columns,
            }
        })
        .collect::<Vec<_>>();

    let mut facets = facets;
    facets.sort_by_key(|facet| facet.index);

    Analysis {
        grouping,
        buckets,
        metric,
        min_days,
        facets,
        total_weight_seconds: grid.total_weight(),
        total_samples: grid.total_samples(),
        observed_days: grid.observed_days(),
    }
}

fn column_probabilities(
    grid: &Grid,
    facet: usize,
    hour: usize,
    metric: Metric,
    min_days: u32,
) -> Column {
    let weights = grid.column(facet, hour);
    let total = grid.column_weight(facet, hour);
    let samples = grid.column_samples(facet, hour);
    let days = grid.column_days(facet, hour);

    let values = if total > 0.0 {
        match metric {
            Metric::Exceedance => {
                let mut values = vec![0.0; weights.len()];
                let mut running = 0.0;
                for bucket in (0..weights.len()).rev() {
                    running += weights[bucket];
                    values[bucket] = (running / total).clamp(0.0, 1.0);
                }
                values
            }
            Metric::Density => weights
                .iter()
                .map(|weight| (weight / total).clamp(0.0, 1.0))
                .collect(),
        }
    } else {
        vec![0.0; weights.len()]
    };

    let status = if total <= 0.0 || days == 0 {
        ColumnStatus::Empty
    } else if days < min_days {
        ColumnStatus::Thin
    } else {
        ColumnStatus::Sufficient
    };

    Column {
        hour,
        values,
        weight_seconds: total,
        samples,
        days,
        status,
    }
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

    fn spec() -> BucketSpec {
        BucketSpec::new(50.0, 200.0).unwrap()
    }

    /// Tiny deterministic generator so the parallel/sequential parity test does not
    /// need a random number dependency.
    struct Lcg(u64);

    impl Lcg {
        fn next_u32(&mut self) -> u32 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (self.0 >> 33) as u32
        }

        fn next_in(&mut self, range: u32) -> u32 {
            self.next_u32() % range
        }
    }

    #[test]
    fn build_grid_weights_hours_by_observation_time() {
        // One hour of 120 W over 2024-06-21, 12:00-13:00 Berlin time.
        let samples = vec![Sample::new(
            ts("2024-06-21 10:00:00"),
            ts("2024-06-21 11:00:00"),
            120.0,
        )];
        let grid = build_grid(&samples, Grouping::Month, spec(), Berlin);
        assert_eq!(grid.cell_weight(5, 12, 2), 3600.0);
        assert_eq!(grid.column_weight(5, 12), 3600.0);
        assert_eq!(grid.total_weight(), 3600.0);
    }

    #[test]
    fn build_grid_splits_samples_across_hours_and_facets() {
        // 30 minutes either side of local midnight on 1 July: half lands in June.
        let samples = vec![Sample::new(
            ts("2024-06-30 21:30:00"),
            ts("2024-06-30 22:30:00"),
            60.0,
        )];
        let grid = build_grid(&samples, Grouping::Month, spec(), Berlin);
        assert_eq!(grid.cell_weight(5, 23, 1), 1800.0, "June, hour 23");
        assert_eq!(grid.cell_weight(6, 0, 1), 1800.0, "July, hour 0");
    }

    #[test]
    fn build_grid_ignores_degenerate_samples() {
        let samples = vec![
            Sample::new(100, 100, 500.0),
            Sample::new(200, 100, 500.0),
            Sample::new(100, 200, f64::NAN),
        ];
        let grid = build_grid(&samples, Grouping::Month, spec(), UTC);
        assert_eq!(grid.total_weight(), 0.0);
        assert_eq!(grid.total_samples(), 0);
    }

    #[test]
    fn parallel_and_sequential_grids_agree() {
        let mut rng = Lcg(0x5EED);
        let base = ts("2023-01-01 00:00:00");
        let samples: Vec<Sample> = (0..80_000)
            .map(|index| {
                let start = base + i64::from(index) * 300 + i64::from(rng.next_in(60));
                let end = start + 300 + i64::from(rng.next_in(600));
                Sample::new(start, end, f64::from(rng.next_in(9_000)))
            })
            .collect();

        let buckets = BucketSpec::new(50.0, 8_000.0).unwrap();
        let parallel = build_grid(&samples, Grouping::Month, buckets, Berlin);
        let sequential = build_grid_sequential(&samples, Grouping::Month, buckets, Berlin);
        assert_eq!(parallel, sequential);
        assert!(parallel.total_weight() > 0.0);

        // The same must hold for the week grouping, which has more facets.
        let buckets = BucketSpec::new(250.0, 9_000.0).unwrap();
        assert_eq!(
            build_grid(&samples, Grouping::Week, buckets, Berlin),
            build_grid_sequential(&samples, Grouping::Week, buckets, Berlin)
        );
    }

    #[test]
    fn suggested_maximum_rounds_up_to_a_whole_step() {
        let samples = vec![
            Sample::new(0, 3600, 100.0),
            Sample::new(3600, 7200, 4_010.0),
        ];
        assert_eq!(suggest_max_watts(&samples, 50.0, 1.0), 4_050.0);
    }

    #[test]
    fn suggested_maximum_ignores_a_rare_spike() {
        let mut samples: Vec<Sample> = (0..1_000)
            .map(|index| Sample::new(index * 3600, (index + 1) * 3600, 3_000.0))
            .collect();
        samples.push(Sample::new(9_000_000, 9_003_600, 60_000.0));

        // At the default quantile the glitch is outside the axis ...
        assert_eq!(suggest_max_watts(&samples, 50.0, 0.999), 3_000.0);
        // ... but asking for the full range includes it.
        assert_eq!(suggest_max_watts(&samples, 50.0, 1.0), 60_000.0);
    }

    #[test]
    fn suggested_maximum_falls_back_to_one_step_without_data() {
        assert_eq!(suggest_max_watts(&[], 50.0, 0.999), 50.0);
        assert_eq!(
            suggest_max_watts(&[Sample::new(0, 3600, 0.0)], 50.0, 0.999),
            50.0
        );
    }

    /// A grid with one June 13:00 column, each entry landing on its own day so the
    /// column counts as well evidenced unless a test says otherwise.
    fn grid_with(column: &[(f64, f64)]) -> Grid {
        grid_with_days(column, true)
    }

    fn grid_with_days(column: &[(f64, f64)], distinct_days: bool) -> Grid {
        let mut grid = Grid::new(Grouping::Month, spec(), DayWindow::new(0, 400));
        for (index, (watts, seconds)) in column.iter().enumerate() {
            let day = if distinct_days { index as i32 } else { 0 };
            grid.add(5, 12, *watts, *seconds, day);
        }
        grid
    }

    #[test]
    fn exceedance_is_a_reverse_cumulative_share() {
        // 25% of the time at 0 W, 25% at 60 W, 50% at 160 W.
        let grid = grid_with(&[(0.0, 900.0), (60.0, 900.0), (160.0, 1800.0)]);
        let analysis = analyse(&grid, Metric::Exceedance, 0, &[]);
        let facet = analysis.facet(5).expect("June has data");

        assert_eq!(facet.value(12, 0), 1.0, "at least 0 W is certain");
        assert_eq!(facet.value(12, 1), 0.75, "at least 50 W");
        assert_eq!(facet.value(12, 2), 0.5, "at least 100 W");
        assert_eq!(facet.value(12, 3), 0.5, "at least 150 W");
        assert_eq!(facet.value(12, 4), 0.0, "at least 200 W");
    }

    #[test]
    fn exceedance_is_monotonically_non_increasing() {
        let grid = grid_with(&[(10.0, 100.0), (80.0, 300.0), (199.0, 50.0), (500.0, 25.0)]);
        let analysis = analyse(&grid, Metric::Exceedance, 0, &[]);
        let facet = analysis.facet(5).unwrap();
        let values = &facet.columns[12].values;
        for pair in values.windows(2) {
            assert!(pair[0] >= pair[1], "not monotonic: {values:?}");
        }
        assert_eq!(values[0], 1.0);
    }

    #[test]
    fn density_columns_sum_to_one() {
        let grid = grid_with(&[(0.0, 900.0), (60.0, 900.0), (160.0, 1800.0)]);
        let analysis = analyse(&grid, Metric::Density, 0, &[]);
        let facet = analysis.facet(5).unwrap();
        let total: f64 = facet.columns[12].values.iter().sum();
        assert!((total - 1.0).abs() < 1e-12, "density summed to {total}");
        assert_eq!(facet.value(12, 1), 0.25);
        assert_eq!(facet.value(12, 3), 0.5);
    }

    #[test]
    fn unobserved_columns_are_zero_and_empty() {
        let grid = grid_with(&[(60.0, 900.0)]);
        let analysis = analyse(&grid, Metric::Exceedance, 1, &[]);
        let facet = analysis.facet(5).unwrap();

        assert_eq!(facet.columns[12].status, ColumnStatus::Sufficient);
        let empty = &facet.columns[0];
        assert_eq!(empty.status, ColumnStatus::Empty);
        assert_eq!(empty.days, 0);
        assert_eq!(empty.weight_seconds, 0.0);
        assert!(empty.values.iter().all(|value| *value == 0.0));
    }

    #[test]
    fn columns_backed_by_too_few_days_are_thin() {
        let grid = grid_with(&[(60.0, 900.0), (70.0, 900.0)]);
        let analysis = analyse(&grid, Metric::Exceedance, 5, &[]);
        let facet = analysis.facet(5).unwrap();
        assert_eq!(facet.columns[12].days, 2);
        assert_eq!(
            facet.columns[12].status,
            ColumnStatus::Thin,
            "two days is below the threshold of five"
        );
        assert!(!facet.columns[12].status.is_sufficient());
    }

    #[test]
    fn a_chatty_sensor_on_one_day_is_still_one_day_of_evidence() {
        // Five hundred readings, all on the same day: the old reading count would have
        // waved this through as plenty of data.
        let column: Vec<(f64, f64)> = (0..500).map(|_| (2_000.0, 7.2)).collect();
        let grid = grid_with_days(&column, false);
        let analysis = analyse(&grid, Metric::Exceedance, 3, &[]);
        let facet = analysis.facet(5).unwrap();

        assert_eq!(facet.columns[12].samples, 500);
        assert_eq!(facet.columns[12].days, 1);
        assert_eq!(facet.columns[12].status, ColumnStatus::Thin);

        // The same readings spread over five hundred days are, of course, plenty.
        let spread = analyse(&grid_with_days(&column, true), Metric::Exceedance, 3, &[]);
        assert_eq!(
            spread.facet(5).unwrap().columns[12].status,
            ColumnStatus::Sufficient
        );
    }

    #[test]
    fn a_threshold_of_zero_shows_everything_that_was_observed() {
        let grid = grid_with_days(&[(60.0, 900.0)], false);
        let analysis = analyse(&grid, Metric::Exceedance, 0, &[]);
        let facet = analysis.facet(5).unwrap();
        assert_eq!(facet.columns[12].status, ColumnStatus::Sufficient);
        // Never observed still means empty, whatever the threshold.
        assert_eq!(facet.columns[0].status, ColumnStatus::Empty);
    }

    #[test]
    fn facets_report_their_days_and_the_calendar_behind_them() {
        let mut grid = Grid::new(Grouping::Month, spec(), DayWindow::new(0, 400));
        for day in 0..4 {
            for hour in 9..12 {
                grid.add(5, hour, 1_000.0, 3_600.0, day);
            }
        }
        let mut possible = vec![0; 12];
        possible[5] = 30;
        let analysis = analyse(&grid, Metric::Exceedance, 3, &possible);
        let facet = analysis.facet(5).unwrap();

        assert_eq!(facet.days, 4, "four days, three hours each");
        assert_eq!(facet.possible_days, 30);
        assert_eq!(facet.columns[9].days, 4);
        assert_eq!(facet.columns[13].days, 0);
        assert_eq!(analysis.observed_days, 4);
    }

    #[test]
    fn analysis_only_keeps_facets_with_data_and_orders_them() {
        let mut grid = Grid::new(Grouping::Month, spec(), DayWindow::new(0, 400));
        grid.add(11, 9, 60.0, 600.0, 1);
        grid.add(2, 9, 60.0, 600.0, 2);
        let analysis = analyse(&grid, Metric::Exceedance, 0, &[]);
        let indices: Vec<usize> = analysis.facets.iter().map(|facet| facet.index).collect();
        assert_eq!(indices, vec![2, 11]);
        assert_eq!(analysis.facets[0].label, "March");
        assert_eq!(analysis.total_weight_seconds, 1200.0);
        assert_eq!(analysis.total_samples, 2);
        assert_eq!(analysis.observed_days, 2);
        // Without a calendar to compare against, the facet reports none.
        assert_eq!(analysis.facets[0].possible_days, 0);
    }

    #[test]
    fn the_day_window_covers_the_local_days_the_samples_touch() {
        let samples = vec![
            Sample::new(ts("2024-06-21 00:00:00"), ts("2024-06-21 01:00:00"), 100.0),
            Sample::new(ts("2024-06-25 00:00:00"), ts("2024-06-25 01:00:00"), 100.0),
        ];
        assert_eq!(day_window(&samples, UTC).len(), 5);
        // A sample ending exactly at midnight belongs to the day before.
        let midnight = vec![Sample::new(
            ts("2024-06-21 23:00:00"),
            ts("2024-06-22 00:00:00"),
            100.0,
        )];
        assert_eq!(day_window(&midnight, UTC).len(), 1);
        assert_eq!(day_window(&[], UTC).len(), 1);
    }
}
