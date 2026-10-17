//! The reading every tool works from, and how the year is sliced into facets.

use std::fmt;

use crate::timeutil::LocalParts;

/// One reading that was in effect over the half-open UTC interval `[start_ts, end_ts)`.
///
/// The value is in the working unit of whatever [`crate::Quantity`] was read: watts for a
/// power sensor, degrees Celsius for a thermometer.
///
/// Long-term statistics rows cover a fixed hour, short-term rows five minutes, and raw
/// recorder states cover the time until the next state change (capped by `--max-gap`).
/// Expressing all three the same way lets the aggregation weight every reading by how
/// long it actually applied instead of by how often the sensor happened to report.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sample {
    pub start_ts: i64,
    pub end_ts: i64,
    pub value: f64,
}

impl Sample {
    pub fn new(start_ts: i64, end_ts: i64, value: f64) -> Self {
        Self {
            start_ts,
            end_ts,
            value,
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
            value: self.value,
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
    pub fn facet_index(self, parts: &LocalParts) -> Option<usize> {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn parts(month: u8, iso_week: u8) -> LocalParts {
        LocalParts {
            hour: 12,
            month,
            iso_week,
            iso_year: 2024,
            year: 2024,
            day: 739_000,
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
}
