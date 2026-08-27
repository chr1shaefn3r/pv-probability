//! Folding samples into a grid (in parallel) and turning weights into probabilities.

use chrono_tz::Tz;
use rayon::prelude::*;

use crate::model::{BucketSpec, Grid, Grouping, HOURS_PER_DAY, Metric, Sample};
use crate::timeutil::for_each_hour_slice;

/// Samples per rayon work unit. Each work unit allocates one partial [`Grid`], so this
/// trades allocation overhead against parallelism; a few tens of thousands of samples
/// per chunk keeps both small.
const CHUNK_SIZE: usize = 32_768;

/// Fold every sample into a facet × hour × bucket grid of observation seconds.
///
/// Grids are additive, so disjoint chunks are folded independently and merged.
pub fn build_grid(samples: &[Sample], grouping: Grouping, buckets: BucketSpec, tz: Tz) -> Grid {
    samples
        .par_chunks(CHUNK_SIZE)
        .map(|chunk| {
            let mut grid = Grid::new(grouping, buckets);
            for sample in chunk {
                accumulate(&mut grid, sample, grouping, tz);
            }
            grid
        })
        .reduce(|| Grid::new(grouping, buckets), Grid::merge)
}

/// Sequential equivalent of [`build_grid`], kept for tests and tiny inputs.
pub fn build_grid_sequential(
    samples: &[Sample],
    grouping: Grouping,
    buckets: BucketSpec,
    tz: Tz,
) -> Grid {
    let mut grid = Grid::new(grouping, buckets);
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
            grid.add(facet, usize::from(parts.hour), watts, seconds);
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
    /// False when the column has too little data to be trusted (`--min-samples`).
    pub sufficient: bool,
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
    pub min_samples: u64,
    /// Facets that carry data, in calendar order.
    pub facets: Vec<Facet>,
    pub total_weight_seconds: f64,
    pub total_samples: u64,
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
pub fn analyse(grid: &Grid, metric: Metric, min_samples: u64) -> Analysis {
    let grouping = grid.grouping();
    let buckets = *grid.buckets();

    let facets = grid
        .non_empty_facets()
        .into_par_iter()
        .map(|index| {
            let columns: Vec<Column> = (0..HOURS_PER_DAY)
                .map(|hour| column_probabilities(grid, index, hour, metric, min_samples))
                .collect();
            Facet {
                index,
                label: grouping.facet_label(index),
                short_label: grouping.facet_short_label(index),
                weight_seconds: columns.iter().map(|column| column.weight_seconds).sum(),
                samples: columns.iter().map(|column| column.samples).sum(),
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
        min_samples,
        facets,
        total_weight_seconds: grid.total_weight(),
        total_samples: grid.total_samples(),
    }
}

fn column_probabilities(
    grid: &Grid,
    facet: usize,
    hour: usize,
    metric: Metric,
    min_samples: u64,
) -> Column {
    let weights = grid.column(facet, hour);
    let total = grid.column_weight(facet, hour);
    let samples = grid.column_samples(facet, hour);

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

    Column {
        hour,
        values,
        weight_seconds: total,
        samples,
        sufficient: total > 0.0 && samples >= min_samples,
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

    fn grid_with(column: &[(f64, f64)]) -> Grid {
        let mut grid = Grid::new(Grouping::Month, spec());
        for (watts, seconds) in column {
            grid.add(5, 12, *watts, *seconds);
        }
        grid
    }

    #[test]
    fn exceedance_is_a_reverse_cumulative_share() {
        // 25% of the time at 0 W, 25% at 60 W, 50% at 160 W.
        let grid = grid_with(&[(0.0, 900.0), (60.0, 900.0), (160.0, 1800.0)]);
        let analysis = analyse(&grid, Metric::Exceedance, 0);
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
        let analysis = analyse(&grid, Metric::Exceedance, 0);
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
        let analysis = analyse(&grid, Metric::Density, 0);
        let facet = analysis.facet(5).unwrap();
        let total: f64 = facet.columns[12].values.iter().sum();
        assert!((total - 1.0).abs() < 1e-12, "density summed to {total}");
        assert_eq!(facet.value(12, 1), 0.25);
        assert_eq!(facet.value(12, 3), 0.5);
    }

    #[test]
    fn empty_columns_are_zero_and_insufficient() {
        let grid = grid_with(&[(60.0, 900.0)]);
        let analysis = analyse(&grid, Metric::Exceedance, 1);
        let facet = analysis.facet(5).unwrap();

        assert!(facet.columns[12].sufficient);
        let empty = &facet.columns[0];
        assert!(!empty.sufficient);
        assert_eq!(empty.weight_seconds, 0.0);
        assert!(empty.values.iter().all(|value| *value == 0.0));
    }

    #[test]
    fn thin_columns_are_marked_insufficient() {
        let grid = grid_with(&[(60.0, 900.0), (70.0, 900.0)]);
        let analysis = analyse(&grid, Metric::Exceedance, 5);
        let facet = analysis.facet(5).unwrap();
        assert_eq!(facet.columns[12].samples, 2);
        assert!(
            !facet.columns[12].sufficient,
            "2 samples is below the threshold of 5"
        );
    }

    #[test]
    fn analysis_only_keeps_facets_with_data_and_orders_them() {
        let mut grid = Grid::new(Grouping::Month, spec());
        grid.add(11, 9, 60.0, 600.0);
        grid.add(2, 9, 60.0, 600.0);
        let analysis = analyse(&grid, Metric::Exceedance, 0);
        let indices: Vec<usize> = analysis.facets.iter().map(|facet| facet.index).collect();
        assert_eq!(indices, vec![2, 11]);
        assert_eq!(analysis.facets[0].label, "March");
        assert_eq!(analysis.total_weight_seconds, 1200.0);
        assert_eq!(analysis.total_samples, 2);
    }
}
