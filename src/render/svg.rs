//! Drawing one facet as an SVG heatmap.
//!
//! Cells are emitted run-length encoded: consecutive buckets in an hour column that land
//! on the same colour level become a single `<rect>`, and cells below the visibility
//! threshold are not emitted at all. A year of 50 W buckets is 12 × 24 × ~150 cells, which
//! would otherwise be a very large file for something that reads as a handful of bands.

use std::fmt::Write as _;

use crate::aggregate::Facet;
use crate::model::{BucketSpec, HOURS_PER_DAY, Metric};
use crate::render::color;
use crate::render::{escape, format_percent, format_watts};

/// Width of one hour column, in user units.
const CELL_WIDTH: f64 = 20.0;
/// Height of the plotting area, in user units.
const PLOT_HEIGHT: f64 = 260.0;
const MARGIN_LEFT: f64 = 46.0;
const MARGIN_RIGHT: f64 = 8.0;
const MARGIN_TOP: f64 = 10.0;
const MARGIN_BOTTOM: f64 = 34.0;
const PLOT_WIDTH: f64 = CELL_WIDTH * HOURS_PER_DAY as f64;

/// Id of the shared "not enough data" hatch pattern defined once per page.
pub const NO_DATA_PATTERN_ID: &str = "pv-no-data";

/// Rendering knobs shared by every facet on a page.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SvgOptions {
    pub levels: usize,
    pub gamma: f64,
    pub min_probability: f64,
    pub metric: Metric,
}

impl SvgOptions {
    /// Lowest bucket worth drawing.
    ///
    /// "At least 0 W" is true whenever the hour was observed at all, so drawing it would
    /// paint a certainty band across the night. The exceedance plot therefore starts one
    /// step up, at "at least one bucket of power".
    pub fn first_bucket(&self, buckets: &BucketSpec) -> usize {
        match self.metric {
            Metric::Exceedance => 1.min(buckets.len().saturating_sub(1)),
            Metric::Density => 0,
        }
    }
}

/// A vertical run of buckets that share a colour level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Run {
    /// First bucket of the run.
    pub start: usize,
    /// Last bucket of the run, inclusive.
    pub end: usize,
    pub level: usize,
}

/// Group a column's cells into runs of equal colour level, skipping blank cells and
/// everything below `first_bucket`.
pub fn runs(values: &[f64], first_bucket: usize, options: &SvgOptions) -> Vec<Run> {
    let mut runs: Vec<Run> = Vec::new();
    for (bucket, value) in values.iter().enumerate().skip(first_bucket) {
        let Some(level) = color::level(
            *value,
            options.levels,
            options.gamma,
            options.min_probability,
        ) else {
            continue;
        };
        match runs.last_mut() {
            Some(run) if run.level == level && run.end + 1 == bucket => run.end = bucket,
            _ => runs.push(Run {
                start: bucket,
                end: bucket,
                level,
            }),
        }
    }
    runs
}

/// Gridline positions for a watt axis running from 0 to `max`.
///
/// Steps are the familiar 1 / 2 / 2.5 / 5 × 10^n sequence, so labels land on round
/// numbers like 1 kW rather than 1 137 W.
pub fn nice_ticks(max: f64, target: usize) -> Vec<f64> {
    if !max.is_finite() || max <= 0.0 || target == 0 {
        return vec![0.0];
    }
    let rough = max / target as f64;
    let magnitude = 10f64.powf(rough.log10().floor());
    let normalised = rough / magnitude;
    let step = if normalised <= 1.0 {
        1.0
    } else if normalised <= 2.0 {
        2.0
    } else if normalised <= 2.5 {
        2.5
    } else if normalised <= 5.0 {
        5.0
    } else {
        10.0
    } * magnitude;

    let mut ticks = Vec::new();
    let mut value = 0.0;
    while value <= max + step * 1e-9 {
        ticks.push(value);
        value += step;
    }
    ticks
}

/// Total size of a facet drawing, in user units.
pub fn facet_size() -> (f64, f64) {
    (
        MARGIN_LEFT + PLOT_WIDTH + MARGIN_RIGHT,
        MARGIN_TOP + PLOT_HEIGHT + MARGIN_BOTTOM,
    )
}

/// Watt values that deserve a gridline, given that the axis starts at `bottom`.
///
/// The bottom of the axis is always labelled, so it is clear that the plot does not
/// start at zero; round numbers too close to it are dropped to avoid overlap.
pub fn axis_ticks(bottom: f64, top: f64) -> Vec<f64> {
    let mut ticks = vec![bottom];
    let span = (top - bottom).max(f64::MIN_POSITIVE);
    ticks.extend(
        nice_ticks(top, 5)
            .into_iter()
            .filter(|tick| *tick <= top && *tick - bottom > span * 0.08),
    );
    ticks
}

/// Render one facet as a standalone `<svg>` element.
pub fn facet_svg(facet: &Facet, buckets: &BucketSpec, options: &SvgOptions) -> String {
    let (width, height) = facet_size();
    let first_bucket = options.first_bucket(buckets);
    let drawn_buckets = buckets.len() - first_bucket;
    let cell_height = PLOT_HEIGHT / drawn_buckets as f64;
    let bottom_watts = buckets.lower_edge(first_bucket);
    // Where a watt value sits vertically, given that the axis starts at `bottom_watts`.
    let y_of = |watts: f64| {
        let span = (buckets.top_watts() - bottom_watts).max(f64::MIN_POSITIVE);
        MARGIN_TOP + PLOT_HEIGHT - ((watts - bottom_watts) / span) * PLOT_HEIGHT
    };
    let mut svg = String::with_capacity(8 * 1024);

    let _ = write!(
        svg,
        "<svg class=\"facet-plot\" viewBox=\"0 0 {width:.0} {height:.0}\" \
         role=\"img\" aria-label=\"{}: likelihood of solar power by hour of day\" \
         preserveAspectRatio=\"xMidYMid meet\">",
        escape(&facet.label)
    );

    // Gridlines and watt labels sit behind the cells.
    svg.push_str("<g class=\"grid\">");
    for tick in axis_ticks(bottom_watts, buckets.top_watts()) {
        let y = y_of(tick);
        let _ = write!(
            svg,
            "<line x1=\"{MARGIN_LEFT:.1}\" y1=\"{y:.1}\" x2=\"{:.1}\" y2=\"{y:.1}\"/>",
            MARGIN_LEFT + PLOT_WIDTH
        );
        let _ = write!(
            svg,
            "<text class=\"tick\" x=\"{:.1}\" y=\"{:.1}\" text-anchor=\"end\">{}</text>",
            MARGIN_LEFT - 5.0,
            y + 4.0,
            escape(&format_watts(tick))
        );
    }
    svg.push_str("</g>");

    // Cells.
    svg.push_str("<g class=\"cells\">");
    for hour in 0..HOURS_PER_DAY {
        let x = MARGIN_LEFT + hour as f64 * CELL_WIDTH;
        let Some(column) = facet.columns.get(hour) else {
            continue;
        };

        if !column.sufficient {
            let _ = write!(
                svg,
                "<rect class=\"no-data\" x=\"{x:.1}\" y=\"{MARGIN_TOP:.1}\" \
                 width=\"{CELL_WIDTH:.1}\" height=\"{PLOT_HEIGHT:.1}\" \
                 fill=\"url(#{NO_DATA_PATTERN_ID})\"><title>{}</title></rect>",
                escape(&format!(
                    "{} {hour:02}:00 - not enough data ({} readings)",
                    facet.label, column.samples
                ))
            );
            continue;
        }

        for run in runs(&column.values, first_bucket, options) {
            let y = MARGIN_TOP + PLOT_HEIGHT - (run.end + 1 - first_bucket) as f64 * cell_height;
            let height = (run.end - run.start + 1) as f64 * cell_height;
            let _ = write!(
                svg,
                "<rect class=\"c{}\" x=\"{x:.1}\" y=\"{y:.2}\" width=\"{CELL_WIDTH:.1}\" \
                 height=\"{height:.2}\"><title>{}</title></rect>",
                run.level,
                escape(&run_title(facet, buckets, hour, &run, options.metric))
            );
        }
    }
    svg.push_str("</g>");

    // Axis frame and hour labels.
    let _ = write!(
        svg,
        "<g class=\"axis\"><line x1=\"{MARGIN_LEFT:.1}\" y1=\"{:.1}\" x2=\"{:.1}\" y2=\"{:.1}\"/>\
         <line x1=\"{MARGIN_LEFT:.1}\" y1=\"{MARGIN_TOP:.1}\" x2=\"{MARGIN_LEFT:.1}\" y2=\"{:.1}\"/>",
        MARGIN_TOP + PLOT_HEIGHT,
        MARGIN_LEFT + PLOT_WIDTH,
        MARGIN_TOP + PLOT_HEIGHT,
        MARGIN_TOP + PLOT_HEIGHT
    );
    for hour in (0..HOURS_PER_DAY).step_by(3) {
        let x = MARGIN_LEFT + (hour as f64 + 0.5) * CELL_WIDTH;
        let _ = write!(
            svg,
            "<text class=\"tick\" x=\"{x:.1}\" y=\"{:.1}\" text-anchor=\"middle\">{hour:02}</text>",
            MARGIN_TOP + PLOT_HEIGHT + 14.0
        );
    }
    let _ = write!(
        svg,
        "<text class=\"axis-title\" x=\"{:.1}\" y=\"{:.1}\" text-anchor=\"middle\">hour of day</text>",
        MARGIN_LEFT + PLOT_WIDTH / 2.0,
        MARGIN_TOP + PLOT_HEIGHT + 28.0
    );
    svg.push_str("</g></svg>");
    svg
}

/// Hover text for one run of cells.
fn run_title(
    facet: &Facet,
    buckets: &BucketSpec,
    hour: usize,
    run: &Run,
    metric: Metric,
) -> String {
    let low = buckets.lower_edge(run.start);
    let high = buckets.lower_edge(run.end);
    let top = run.end + 1 == buckets.len();

    let power = match (metric, run.start == run.end, top) {
        (Metric::Exceedance, true, _) => format!("at least {}", format_watts(low)),
        (Metric::Exceedance, false, _) => {
            format!("at least {} to {}", format_watts(low), format_watts(high))
        }
        (Metric::Density, true, true) => format!("{} and above", format_watts(low)),
        (Metric::Density, true, false) => format!(
            "{} to {}",
            format_watts(low),
            format_watts(low + buckets.step())
        ),
        (Metric::Density, false, _) => format!(
            "{} to {}",
            format_watts(low),
            format_watts(high + buckets.step())
        ),
    };

    let highest = facet.value(hour, run.start);
    let lowest = facet.value(hour, run.end);
    let chance = if (highest - lowest).abs() < 5e-4 {
        format_percent(highest)
    } else {
        format!("{} to {}", format_percent(lowest), format_percent(highest))
    };

    format!("{} {hour:02}:00 - {power}: {chance}", facet.label)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aggregate::{Column, Facet};

    fn options() -> SvgOptions {
        SvgOptions {
            levels: 10,
            gamma: 1.0,
            min_probability: 0.005,
            metric: Metric::Exceedance,
        }
    }

    fn facet_from(columns: Vec<Vec<f64>>, sufficient: bool) -> Facet {
        let columns = columns
            .into_iter()
            .enumerate()
            .map(|(hour, values)| Column {
                hour,
                values,
                weight_seconds: 3_600.0,
                samples: 10,
                sufficient,
            })
            .collect::<Vec<_>>();
        Facet {
            index: 5,
            label: "June".to_string(),
            short_label: "Jun".to_string(),
            weight_seconds: 3_600.0 * columns.len() as f64,
            samples: 10 * columns.len() as u64,
            columns,
        }
    }

    #[test]
    fn runs_merge_neighbouring_cells_of_the_same_level() {
        // Four cells at the top level, then four in the middle.
        let values = vec![1.0, 1.0, 1.0, 1.0, 0.5, 0.5, 0.5, 0.5];
        let runs = runs(&values, 0, &options());
        assert_eq!(
            runs,
            vec![
                Run {
                    start: 0,
                    end: 3,
                    level: 9
                },
                Run {
                    start: 4,
                    end: 7,
                    level: 5
                },
            ]
        );
    }

    #[test]
    fn runs_skip_blank_cells_and_do_not_bridge_them() {
        let values = vec![0.5, 0.0, 0.5];
        let runs = runs(&values, 0, &options());
        assert_eq!(runs.len(), 2, "a blank cell must break the run: {runs:?}");
        assert_eq!(
            runs[0],
            Run {
                start: 0,
                end: 0,
                level: 5
            }
        );
        assert_eq!(
            runs[1],
            Run {
                start: 2,
                end: 2,
                level: 5
            }
        );
    }

    #[test]
    fn runs_honour_the_visibility_threshold() {
        let values = vec![0.5, 0.001, 0.4];
        let mut options = options();
        options.min_probability = 0.01;
        assert_eq!(runs(&values, 0, &options).len(), 2);

        options.min_probability = 0.0;
        assert_eq!(runs(&values, 0, &options).len(), 3);
    }

    #[test]
    fn an_empty_column_produces_no_runs() {
        assert!(runs(&[0.0, 0.0, 0.0], 0, &options()).is_empty());
        assert!(runs(&[], 0, &options()).is_empty());
    }

    #[test]
    fn ticks_land_on_round_numbers() {
        assert_eq!(
            nice_ticks(1_000.0, 5),
            vec![0.0, 200.0, 400.0, 600.0, 800.0, 1_000.0]
        );
        assert_eq!(
            nice_ticks(8_000.0, 5),
            vec![0.0, 2_000.0, 4_000.0, 6_000.0, 8_000.0]
        );
        assert_eq!(nice_ticks(450.0, 5), vec![0.0, 100.0, 200.0, 300.0, 400.0]);
        assert_eq!(nice_ticks(0.0, 5), vec![0.0]);
        assert_eq!(nice_ticks(f64::NAN, 5), vec![0.0]);
        assert_eq!(nice_ticks(100.0, 0), vec![0.0]);
    }

    #[test]
    fn ticks_stay_inside_the_axis() {
        for max in [50.0, 137.0, 999.0, 4_050.0, 12_345.0] {
            let ticks = nice_ticks(max, 5);
            assert!(
                ticks.iter().all(|tick| *tick <= max + 1e-9),
                "{ticks:?} for {max}"
            );
            assert!(ticks.len() >= 2, "{ticks:?} for {max}");
        }
    }

    #[test]
    fn a_facet_renders_one_svg_with_cells_and_axes() {
        let buckets = BucketSpec::new(50.0, 200.0).unwrap();
        let facet = facet_from(vec![vec![1.0, 0.8, 0.4, 0.1, 0.0]; 24], true);
        let svg = facet_svg(&facet, &buckets, &options());

        assert_eq!(svg.matches("<svg").count(), 1);
        assert!(svg.ends_with("</svg>"));
        assert!(svg.contains("hour of day"));
        assert!(svg.contains("<rect"));
        assert!(svg.contains("<title>June 00:00"));
        // Nothing may leak a non-finite coordinate into the document.
        assert!(!svg.contains("NaN"), "{svg}");
        assert!(!svg.contains("inf"), "{svg}");
    }

    #[test]
    fn facets_without_enough_data_are_hatched_instead_of_coloured() {
        let buckets = BucketSpec::new(50.0, 200.0).unwrap();
        let facet = facet_from(vec![vec![1.0, 0.8, 0.4, 0.1, 0.0]; 24], false);
        let svg = facet_svg(&facet, &buckets, &options());

        assert_eq!(svg.matches("class=\"no-data\"").count(), 24);
        assert!(svg.contains("not enough data"));
        assert!(!svg.contains("class=\"c9\""));
    }

    #[test]
    fn the_exceedance_plot_starts_one_step_above_zero() {
        // "At least 0 W" is certain whenever the hour was observed at all, so drawing it
        // would paint a solid band across the night.
        let buckets = BucketSpec::new(50.0, 200.0).unwrap();
        let facet = facet_from(vec![vec![1.0, 0.0, 0.0, 0.0, 0.0]; 24], true);
        let svg = facet_svg(&facet, &buckets, &options());
        assert!(
            !svg.contains("<rect class=\"c"),
            "bucket 0 must not be drawn: {svg}"
        );
        assert!(
            svg.contains(">50 W</text>"),
            "the axis should start at 50 W"
        );

        // The density plot keeps every bucket, including the lowest.
        let mut density = options();
        density.metric = Metric::Density;
        let svg = facet_svg(&facet, &buckets, &density);
        assert!(svg.contains("<rect class=\"c"));
        assert!(svg.contains(">0 W</text>"));
    }

    #[test]
    fn cells_stack_from_the_baseline_upwards() {
        let buckets = BucketSpec::new(50.0, 200.0).unwrap();
        // Certain at 50 W, less so above it.
        let facet = facet_from(vec![vec![1.0, 1.0, 0.5, 0.0, 0.0]; 24], true);
        let svg = facet_svg(&facet, &buckets, &options());

        // Four buckets remain once bucket 0 is dropped.
        let cell_height = PLOT_HEIGHT / (buckets.len() - 1) as f64;
        let expected_y = MARGIN_TOP + PLOT_HEIGHT - cell_height;
        assert!(
            svg.contains(&format!("y=\"{expected_y:.2}\"")),
            "the lowest drawn bucket should sit on the baseline, expected y={expected_y:.2}"
        );
        // Nothing may be drawn below the baseline or above the top of the plot.
        assert!(!svg.contains(&format!("y=\"{:.2}\"", MARGIN_TOP + PLOT_HEIGHT)));
    }

    #[test]
    fn run_titles_describe_power_and_likelihood() {
        let buckets = BucketSpec::new(500.0, 2_000.0).unwrap();
        let facet = facet_from(vec![vec![1.0, 0.62, 0.62, 0.05, 0.0]; 24], true);

        let single = run_title(
            &facet,
            &buckets,
            13,
            &Run {
                start: 0,
                end: 0,
                level: 9,
            },
            Metric::Exceedance,
        );
        assert_eq!(single, "June 13:00 - at least 0 W: 100%");

        let merged = run_title(
            &facet,
            &buckets,
            13,
            &Run {
                start: 1,
                end: 2,
                level: 5,
            },
            Metric::Exceedance,
        );
        assert_eq!(merged, "June 13:00 - at least 500 W to 1 kW: 62%");

        let density = run_title(
            &facet,
            &buckets,
            13,
            &Run {
                start: 1,
                end: 1,
                level: 5,
            },
            Metric::Density,
        );
        assert_eq!(density, "June 13:00 - 500 W to 1 kW: 62%");
    }

    #[test]
    fn run_titles_show_a_range_when_the_run_spans_probabilities() {
        let buckets = BucketSpec::new(500.0, 2_000.0).unwrap();
        let facet = facet_from(vec![vec![1.0, 0.62, 0.55, 0.05, 0.0]; 24], true);
        let title = run_title(
            &facet,
            &buckets,
            9,
            &Run {
                start: 1,
                end: 2,
                level: 5,
            },
            Metric::Exceedance,
        );
        assert_eq!(title, "June 09:00 - at least 500 W to 1 kW: 55% to 62%");
    }
}
