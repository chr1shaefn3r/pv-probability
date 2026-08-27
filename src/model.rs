//! The data types shared by the ingest, aggregation and rendering stages.

use std::fmt;

use anyhow::{Result, ensure};

/// One power reading that was in effect over the half-open UTC interval
/// `[start_ts, end_ts)`.
///
/// Long-term statistics rows cover a fixed hour, short-term rows five minutes, and raw
/// recorder states cover the time until the next state change (capped by `--max-gap`).
/// Expressing all three the same way lets the aggregation weight every reading by how
/// long it actually applied instead of by how often the sensor happened to report.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sample {
    pub start_ts: i64,
    pub end_ts: i64,
    pub watts: f64,
}

impl Sample {
    pub fn new(start_ts: i64, end_ts: i64, watts: f64) -> Self {
        Self {
            start_ts,
            end_ts,
            watts,
        }
    }

    /// Duration in seconds, never negative.
    pub fn duration(&self) -> i64 {
        (self.end_ts - self.start_ts).max(0)
    }

    /// Clip the sample to `[from, to)`, returning `None` if nothing is left.
    pub fn clipped(self, from: Option<i64>, to: Option<i64>) -> Option<Self> {
        let start = from.map_or(self.start_ts, |f| self.start_ts.max(f));
        let end = to.map_or(self.end_ts, |t| self.end_ts.min(t));
        (end > start).then_some(Self {
            start_ts: start,
            end_ts: end,
            watts: self.watts,
        })
    }
}

/// How the year is sliced into facets: one heatmap per calendar month or per ISO week.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Grouping {
    Month,
    Week,
}

impl Grouping {
    /// Number of facets a grid reserves for this grouping.
    pub fn facet_count(self) -> usize {
        match self {
            Grouping::Month => 12,
            // ISO weeks run 1..=53.
            Grouping::Week => 53,
        }
    }

    /// Facet index for a local instant, or `None` if it falls outside the range.
    pub fn facet_index(self, parts: &crate::timeutil::LocalParts) -> Option<usize> {
        let one_based = match self {
            Grouping::Month => usize::from(parts.month),
            Grouping::Week => usize::from(parts.iso_week),
        };
        (1..=self.facet_count())
            .contains(&one_based)
            .then(|| one_based - 1)
    }

    /// Human readable facet label, e.g. `June` or `Week 23`.
    pub fn facet_label(self, index: usize) -> String {
        match self {
            Grouping::Month => MONTH_NAMES.get(index).map_or_else(
                || format!("Month {}", index + 1),
                |name| (*name).to_string(),
            ),
            Grouping::Week => format!("Week {:02}", index + 1),
        }
    }

    /// Short facet label used where space is tight.
    pub fn facet_short_label(self, index: usize) -> String {
        match self {
            Grouping::Month => MONTH_NAMES
                .get(index)
                .map_or_else(|| format!("M{}", index + 1), |name| name[..3].to_string()),
            Grouping::Week => format!("W{:02}", index + 1),
        }
    }
}

impl fmt::Display for Grouping {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Grouping::Month => f.write_str("month"),
            Grouping::Week => f.write_str("week"),
        }
    }
}

const MONTH_NAMES: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

/// Whether a cell states "at least this much power" or "exactly this much power".
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Metric {
    /// `P(watts >= bucket lower edge)` — the default, and what makes the plot a flame.
    Exceedance,
    /// `P(bucket lower edge <= watts < upper edge)`.
    Density,
}

impl fmt::Display for Metric {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Metric::Exceedance => f.write_str("exceedance"),
            Metric::Density => f.write_str("density"),
        }
    }
}

/// The watt axis: `n_buckets` bins of `step` watts each, the last one collecting
/// everything above the configured maximum.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BucketSpec {
    step: f64,
    n_buckets: usize,
}

impl BucketSpec {
    /// Build a bucket axis covering `0..=max_watts` in `step` watt steps.
    pub fn new(step: f64, max_watts: f64) -> Result<Self> {
        ensure!(
            step.is_finite() && step > 0.0,
            "--step-watts must be a positive number"
        );
        ensure!(
            max_watts.is_finite() && max_watts > 0.0,
            "--max-watts must be a positive number"
        );
        ensure!(
            max_watts >= step,
            "--max-watts ({max_watts}) must be at least one step ({step})"
        );
        // One bucket per step plus the final open-ended bucket at `max_watts`.
        let n_buckets = (max_watts / step).ceil() as usize + 1;
        Ok(Self { step, n_buckets })
    }

    pub fn step(&self) -> f64 {
        self.step
    }

    pub fn len(&self) -> usize {
        self.n_buckets
    }

    pub fn is_empty(&self) -> bool {
        self.n_buckets == 0
    }

    /// Bucket index for a reading. Negative and unreadable values land in bucket 0,
    /// readings above the axis land in the final open-ended bucket.
    pub fn index(&self, watts: f64) -> usize {
        if watts.is_nan() || watts <= 0.0 {
            return 0;
        }
        let raw = (watts / self.step).floor();
        // Written as a negated comparison so that infinity lands in the top bucket.
        if raw < (self.n_buckets - 1) as f64 {
            raw as usize
        } else {
            self.n_buckets - 1
        }
    }

    /// Lower edge of a bucket in watts.
    pub fn lower_edge(&self, index: usize) -> f64 {
        index as f64 * self.step
    }

    /// Upper edge of the axis in watts (the lower edge of the open-ended bucket).
    pub fn top_watts(&self) -> f64 {
        self.lower_edge(self.n_buckets - 1)
    }
}

/// Accumulated observation weight per facet, hour of day and watt bucket.
///
/// Weights are seconds: a reading contributes the number of seconds it was in effect to
/// the hour(s) it covered. Grids are additive, which is what lets rayon fold disjoint
/// chunks of samples independently and merge the results.
#[derive(Debug, Clone, PartialEq)]
pub struct Grid {
    grouping: Grouping,
    buckets: BucketSpec,
    /// facet × hour × bucket
    weights: Vec<f64>,
    /// facet × hour, total weight regardless of bucket
    column_weight: Vec<f64>,
    /// facet × hour, number of source readings that touched the column
    column_samples: Vec<u64>,
}

pub const HOURS_PER_DAY: usize = 24;

impl Grid {
    pub fn new(grouping: Grouping, buckets: BucketSpec) -> Self {
        let columns = grouping.facet_count() * HOURS_PER_DAY;
        Self {
            grouping,
            buckets,
            weights: vec![0.0; columns * buckets.len()],
            column_weight: vec![0.0; columns],
            column_samples: vec![0; columns],
        }
    }

    pub fn grouping(&self) -> Grouping {
        self.grouping
    }

    pub fn buckets(&self) -> &BucketSpec {
        &self.buckets
    }

    fn column_index(&self, facet: usize, hour: usize) -> usize {
        facet * HOURS_PER_DAY + hour
    }

    /// Add `seconds` of observation time at `watts` to one facet/hour column.
    pub fn add(&mut self, facet: usize, hour: usize, watts: f64, seconds: f64) {
        if facet >= self.grouping.facet_count() || hour >= HOURS_PER_DAY {
            return;
        }
        if !seconds.is_finite() || seconds <= 0.0 {
            return;
        }
        let column = self.column_index(facet, hour);
        let bucket = self.buckets.index(watts);
        self.weights[column * self.buckets.len() + bucket] += seconds;
        self.column_weight[column] += seconds;
        self.column_samples[column] += 1;
    }

    /// Element-wise sum, used to merge the partial grids produced by rayon workers.
    pub fn merge(mut self, other: Grid) -> Grid {
        debug_assert_eq!(self.grouping, other.grouping);
        debug_assert_eq!(self.buckets, other.buckets);
        for (target, value) in self.weights.iter_mut().zip(other.weights.iter()) {
            *target += value;
        }
        for (target, value) in self
            .column_weight
            .iter_mut()
            .zip(other.column_weight.iter())
        {
            *target += value;
        }
        for (target, value) in self
            .column_samples
            .iter_mut()
            .zip(other.column_samples.iter())
        {
            *target += value;
        }
        self
    }

    /// Weight accumulated in one cell, in seconds.
    pub fn cell_weight(&self, facet: usize, hour: usize, bucket: usize) -> f64 {
        let column = self.column_index(facet, hour);
        self.weights
            .get(column * self.buckets.len() + bucket)
            .copied()
            .unwrap_or(0.0)
    }

    /// Total weight of one facet/hour column, in seconds.
    pub fn column_weight(&self, facet: usize, hour: usize) -> f64 {
        self.column_weight
            .get(self.column_index(facet, hour))
            .copied()
            .unwrap_or(0.0)
    }

    /// Number of readings that contributed to one facet/hour column.
    pub fn column_samples(&self, facet: usize, hour: usize) -> u64 {
        self.column_samples
            .get(self.column_index(facet, hour))
            .copied()
            .unwrap_or(0)
    }

    /// The bucket weights of one facet/hour column.
    pub fn column(&self, facet: usize, hour: usize) -> &[f64] {
        let column = self.column_index(facet, hour);
        let start = column * self.buckets.len();
        &self.weights[start..start + self.buckets.len()]
    }

    /// Total weight over the whole grid, in seconds.
    pub fn total_weight(&self) -> f64 {
        self.column_weight.iter().sum()
    }

    /// Total number of readings that landed anywhere in the grid.
    pub fn total_samples(&self) -> u64 {
        self.column_samples.iter().sum()
    }

    /// Facet indices that carry any data at all, in ascending order.
    pub fn non_empty_facets(&self) -> Vec<usize> {
        (0..self.grouping.facet_count())
            .filter(|facet| (0..HOURS_PER_DAY).any(|hour| self.column_weight(*facet, hour) > 0.0))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::timeutil::LocalParts;

    fn parts(month: u8, iso_week: u8) -> LocalParts {
        LocalParts {
            hour: 12,
            month,
            iso_week,
            iso_year: 2024,
            year: 2024,
        }
    }

    #[test]
    fn sample_duration_is_never_negative() {
        assert_eq!(Sample::new(100, 160, 5.0).duration(), 60);
        assert_eq!(Sample::new(160, 100, 5.0).duration(), 0);
    }

    #[test]
    fn samples_clip_to_the_requested_window() {
        let sample = Sample::new(100, 200, 5.0);
        assert_eq!(sample.clipped(None, None), Some(sample));
        assert_eq!(
            sample.clipped(Some(150), None),
            Some(Sample::new(150, 200, 5.0))
        );
        assert_eq!(
            sample.clipped(None, Some(150)),
            Some(Sample::new(100, 150, 5.0))
        );
        assert_eq!(sample.clipped(Some(200), None), None);
        assert_eq!(sample.clipped(None, Some(100)), None);
        assert_eq!(sample.clipped(Some(180), Some(120)), None);
    }

    #[test]
    fn grouping_maps_local_parts_to_facets() {
        assert_eq!(Grouping::Month.facet_count(), 12);
        assert_eq!(Grouping::Week.facet_count(), 53);
        assert_eq!(Grouping::Month.facet_index(&parts(1, 1)), Some(0));
        assert_eq!(Grouping::Month.facet_index(&parts(12, 1)), Some(11));
        assert_eq!(Grouping::Week.facet_index(&parts(6, 53)), Some(52));
        // Defensive: out-of-range values are dropped rather than panicking.
        assert_eq!(Grouping::Month.facet_index(&parts(13, 1)), None);
        assert_eq!(Grouping::Week.facet_index(&parts(6, 54)), None);
        assert_eq!(Grouping::Week.facet_index(&parts(6, 0)), None);
    }

    #[test]
    fn grouping_labels_read_naturally() {
        assert_eq!(Grouping::Month.facet_label(5), "June");
        assert_eq!(Grouping::Month.facet_short_label(5), "Jun");
        assert_eq!(Grouping::Week.facet_label(22), "Week 23");
        assert_eq!(Grouping::Week.facet_short_label(22), "W23");
    }

    #[test]
    fn bucket_spec_rejects_nonsense() {
        assert!(BucketSpec::new(0.0, 1000.0).is_err());
        assert!(BucketSpec::new(-50.0, 1000.0).is_err());
        assert!(BucketSpec::new(50.0, 0.0).is_err());
        assert!(BucketSpec::new(f64::NAN, 1000.0).is_err());
        assert!(BucketSpec::new(500.0, 100.0).is_err());
        assert!(BucketSpec::new(50.0, 50.0).is_ok());
    }

    #[test]
    fn bucket_indices_follow_the_lower_edge() {
        let spec = BucketSpec::new(50.0, 200.0).unwrap();
        // 0, 50, 100, 150, 200+ => five buckets.
        assert_eq!(spec.len(), 5);
        assert_eq!(spec.index(0.0), 0);
        assert_eq!(spec.index(49.999), 0);
        assert_eq!(spec.index(50.0), 1);
        assert_eq!(spec.index(99.0), 1);
        assert_eq!(spec.index(150.0), 3);
        assert_eq!(spec.lower_edge(3), 150.0);
        assert_eq!(spec.top_watts(), 200.0);
    }

    #[test]
    fn bucket_indices_clamp_outside_the_axis() {
        let spec = BucketSpec::new(50.0, 200.0).unwrap();
        assert_eq!(spec.index(-1.0), 0);
        assert_eq!(spec.index(f64::NAN), 0);
        assert_eq!(spec.index(f64::NEG_INFINITY), 0);
        assert_eq!(spec.index(200.0), 4);
        assert_eq!(spec.index(9_999.0), 4);
        assert_eq!(spec.index(f64::INFINITY), 4);
    }

    #[test]
    fn bucket_spec_handles_a_step_that_does_not_divide_the_maximum() {
        let spec = BucketSpec::new(300.0, 1000.0).unwrap();
        // 0, 300, 600, 900, 1200+ => the axis rounds up past the requested maximum.
        assert_eq!(spec.len(), 5);
        assert_eq!(spec.top_watts(), 1200.0);
        assert_eq!(spec.index(1000.0), 3);
        assert_eq!(spec.index(1300.0), 4);
    }

    #[test]
    fn grid_accumulates_weight_per_cell_and_column() {
        let spec = BucketSpec::new(50.0, 200.0).unwrap();
        let mut grid = Grid::new(Grouping::Month, spec);
        grid.add(5, 12, 120.0, 600.0);
        grid.add(5, 12, 130.0, 300.0);
        grid.add(5, 13, 20.0, 100.0);

        assert_eq!(grid.cell_weight(5, 12, 2), 900.0);
        assert_eq!(grid.column_weight(5, 12), 900.0);
        assert_eq!(grid.column_samples(5, 12), 2);
        assert_eq!(grid.cell_weight(5, 13, 0), 100.0);
        assert_eq!(grid.total_weight(), 1000.0);
        assert_eq!(grid.total_samples(), 3);
        assert_eq!(grid.non_empty_facets(), vec![5]);
    }

    #[test]
    fn grid_ignores_out_of_range_and_zero_weight_additions() {
        let spec = BucketSpec::new(50.0, 200.0).unwrap();
        let mut grid = Grid::new(Grouping::Month, spec);
        grid.add(12, 0, 100.0, 60.0); // no 13th month
        grid.add(0, 24, 100.0, 60.0); // no 25th hour
        grid.add(0, 0, 100.0, 0.0); // zero duration
        grid.add(0, 0, 100.0, f64::NAN); // non-finite duration
        assert_eq!(grid.total_weight(), 0.0);
        assert_eq!(grid.total_samples(), 0);
        assert!(grid.non_empty_facets().is_empty());
    }

    #[test]
    fn grids_merge_element_wise() {
        let spec = BucketSpec::new(50.0, 200.0).unwrap();
        let mut left = Grid::new(Grouping::Month, spec);
        left.add(0, 0, 60.0, 100.0);
        let mut right = Grid::new(Grouping::Month, spec);
        right.add(0, 0, 60.0, 50.0);
        right.add(1, 5, 10.0, 25.0);

        let merged = left.merge(right);
        assert_eq!(merged.cell_weight(0, 0, 1), 150.0);
        assert_eq!(merged.column_samples(0, 0), 2);
        assert_eq!(merged.cell_weight(1, 5, 0), 25.0);
        assert_eq!(merged.total_weight(), 175.0);
    }
}
