//! The page: header, scale legend, one heatmap per facet, and a table view of the same
//! numbers so nothing is readable only by colour or only on hover.

use std::fmt::Write as _;

use rayon::prelude::*;

use crate::aggregate::{Analysis, Facet};
use crate::model::{BucketSpec, HOURS_PER_DAY, Metric};
use crate::render::color;
use crate::render::svg::{NO_DATA_PATTERN_ID, SvgOptions, axis_ticks, facet_svg};
use crate::render::{escape, format_duration, format_percent, format_watts};

/// Everything the page needs beyond the analysis itself.
#[derive(Debug, Clone, PartialEq)]
pub struct PageOptions {
    /// Entity the report is about, shown in the header.
    pub entity: String,
    /// Facts about the run: source table, timezone, date range and so on.
    pub metadata: Vec<(String, String)>,
    pub levels: usize,
    pub gamma: f64,
    pub min_probability: f64,
}

impl Default for PageOptions {
    fn default() -> Self {
        Self {
            entity: String::new(),
            metadata: Vec::new(),
            levels: color::DEFAULT_LEVELS,
            gamma: 0.6,
            min_probability: 0.005,
        }
    }
}

/// Render the complete HTML document.
pub fn page(analysis: &Analysis, options: &PageOptions) -> String {
    let svg_options = SvgOptions {
        levels: options.levels,
        gamma: options.gamma,
        min_probability: options.min_probability,
        metric: analysis.metric,
    };

    // Facets are independent, so they are drawn in parallel and joined in order.
    let facets: Vec<String> = analysis
        .facets
        .par_iter()
        .map(|facet| facet_section(facet, &analysis.buckets, analysis.metric, &svg_options))
        .collect();

    let title = format!("Solar power likelihood - {}", options.entity);
    let mut html = String::with_capacity(64 * 1024 + facets.iter().map(String::len).sum::<usize>());

    html.push_str("<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n");
    html.push_str("<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n");
    let _ = writeln!(html, "<title>{}</title>", escape(&title));
    let _ = writeln!(html, "<style>\n{}\n</style>", stylesheet(options.levels));
    html.push_str("</head>\n<body>\n");
    html.push_str(&hidden_defs());

    let _ = write!(
        html,
        "<header class=\"page-header\"><h1>{}</h1><p class=\"lede\">{}</p></header>",
        escape(&title),
        escape(&lede(analysis))
    );

    html.push_str(&metadata_list(analysis, options));
    html.push_str(&legend(analysis, options));

    if analysis.facets.is_empty() {
        html.push_str(
            "<p class=\"empty\">No readings matched the selected entity and date range, \
             so there is nothing to plot.</p>",
        );
    } else {
        html.push_str("<main class=\"facets\">");
        for facet in facets {
            html.push_str(&facet);
        }
        html.push_str("</main>");
    }

    html.push_str(&footer(analysis));
    html.push_str("\n</body>\n</html>\n");
    html
}

fn lede(analysis: &Analysis) -> String {
    match analysis.metric {
        Metric::Exceedance => format!(
            "Each cell is the share of observed time at that hour with at least that much \
             power available, per {}. Darker red means more likely.",
            analysis.grouping
        ),
        Metric::Density => format!(
            "Each cell is the share of observed time at that hour spent inside that power \
             band, per {}. Darker red means more likely.",
            analysis.grouping
        ),
    }
}

fn metadata_list(analysis: &Analysis, options: &PageOptions) -> String {
    let mut html = String::from("<section class=\"meta\"><dl>");
    let _ = write!(
        html,
        "<div><dt>Entity</dt><dd>{}</dd></div>",
        escape(&options.entity)
    );
    for (key, value) in &options.metadata {
        let _ = write!(
            html,
            "<div><dt>{}</dt><dd>{}</dd></div>",
            escape(key),
            escape(value)
        );
    }
    let _ = write!(
        html,
        "<div><dt>Observed</dt><dd>{} over {} readings</dd></div>",
        escape(&format_duration(analysis.total_weight_seconds)),
        analysis.total_samples
    );
    let _ = write!(
        html,
        "<div><dt>Buckets</dt><dd>{} steps up to {}</dd></div>",
        escape(&format_watts(analysis.buckets.step())),
        escape(&format_watts(analysis.buckets.top_watts()))
    );
    html.push_str("</dl></section>");
    html
}

fn legend(analysis: &Analysis, options: &PageOptions) -> String {
    let edges = color::level_edges(options.levels, options.gamma);
    let mut html =
        String::from("<section class=\"legend\"><h2>Likelihood</h2><ul class=\"scale\">");
    for (index, edge) in edges.iter().enumerate() {
        let upper = edges.get(index + 1).copied().unwrap_or(1.0);
        let _ = write!(
            html,
            "<li><span class=\"swatch s{index}\" aria-hidden=\"true\"></span>\
             <span class=\"swatch-label\">{}</span></li>",
            escape(&format!(
                "{}-{}",
                format_percent(*edge),
                format_percent(upper)
            ))
        );
    }
    let _ = write!(
        html,
        "</ul><ul class=\"scale extra\">\
         <li><span class=\"swatch blank\" aria-hidden=\"true\"></span>\
         <span class=\"swatch-label\">below {}</span></li>\
         <li><span class=\"swatch hatched\" aria-hidden=\"true\"></span>\
         <span class=\"swatch-label\">fewer than {} readings</span></li></ul>\
         <p class=\"note\">{}</p></section>",
        escape(&format_percent(options.min_probability)),
        analysis.min_samples,
        escape(&match analysis.metric {
            Metric::Exceedance => format!(
                "Read a column upwards: the height at which the colour fades is roughly the \
                 power you can count on at that hour. The axis starts at {}, because \"at \
                 least nothing\" is always certain.",
                format_watts(analysis.buckets.step())
            ),
            Metric::Density =>
                "Read a column upwards: the brightest band is the power this hour spends most \
                 of its time at."
                    .to_string(),
        })
    );
    html
}

fn facet_section(
    facet: &Facet,
    buckets: &BucketSpec,
    metric: Metric,
    options: &SvgOptions,
) -> String {
    let mut html = String::with_capacity(16 * 1024);
    let _ = write!(
        html,
        "<figure class=\"facet\"><figcaption><span class=\"facet-name\">{}</span>\
         <span class=\"facet-stats\">{} over {} readings</span></figcaption>",
        escape(&facet.label),
        escape(&format_duration(facet.weight_seconds)),
        facet.samples
    );
    html.push_str(&facet_svg(facet, buckets, options));
    html.push_str(&facet_table(facet, buckets, metric, options));
    html.push_str("</figure>");
    html
}

/// The table twin of a facet: the same probabilities as text, at the gridline levels.
fn facet_table(
    facet: &Facet,
    buckets: &BucketSpec,
    metric: Metric,
    options: &SvgOptions,
) -> String {
    let bottom = buckets.lower_edge(options.first_bucket(buckets));
    let thresholds = axis_ticks(bottom, buckets.top_watts());

    let mut html = String::with_capacity(4 * 1024);
    let _ = write!(
        html,
        "<details class=\"table-view\"><summary>{} as a table</summary>\
         <div class=\"table-scroll\"><table><caption>{}</caption><thead><tr>\
         <th scope=\"col\">Power</th>",
        escape(&facet.label),
        escape(&match metric {
            Metric::Exceedance => format!(
                "{}: probability of at least the given power, by hour of day",
                facet.label
            ),
            Metric::Density => format!(
                "{}: probability of being inside the given power band, by hour of day",
                facet.label
            ),
        })
    );
    for hour in 0..HOURS_PER_DAY {
        let _ = write!(html, "<th scope=\"col\">{hour:02}</th>");
    }
    html.push_str("</tr></thead><tbody>");

    for threshold in thresholds.iter().rev() {
        let bucket = buckets.index(*threshold);
        let _ = write!(
            html,
            "<tr><th scope=\"row\">{}</th>",
            escape(&format_watts(*threshold))
        );
        for hour in 0..HOURS_PER_DAY {
            let sufficient = facet
                .columns
                .get(hour)
                .is_some_and(|column| column.sufficient);
            let cell = if sufficient {
                format_percent(facet.value(hour, bucket))
            } else {
                "-".to_string()
            };
            let _ = write!(html, "<td>{}</td>", escape(&cell));
        }
        html.push_str("</tr>");
    }
    html.push_str("</tbody></table></div></details>");
    html
}

fn footer(analysis: &Analysis) -> String {
    format!(
        "<footer class=\"page-footer\"><p>Generated by pv-probability {} - \
         {} facets, {} metric, grouped by {}.</p></footer>",
        env!("CARGO_PKG_VERSION"),
        analysis.facets.len(),
        analysis.metric,
        analysis.grouping
    )
}

/// A zero-sized SVG carrying the hatch pattern every facet references.
fn hidden_defs() -> String {
    format!(
        "<svg class=\"defs\" width=\"0\" height=\"0\" aria-hidden=\"true\" focusable=\"false\">\
         <defs><pattern id=\"{NO_DATA_PATTERN_ID}\" width=\"6\" height=\"6\" \
         patternUnits=\"userSpaceOnUse\" patternTransform=\"rotate(45)\">\
         <line x1=\"0\" y1=\"0\" x2=\"0\" y2=\"6\" class=\"hatch\"/></pattern></defs></svg>"
    )
}

fn stylesheet(levels: usize) -> String {
    let light = color::ramp(levels, false);
    let dark = color::ramp(levels, true);
    let mut css = String::with_capacity(4 * 1024);

    css.push_str(
        ":root {\n  color-scheme: light dark;\n\
         \x20 --surface: #fcfcfb;\n  --plane: #f9f9f7;\n  --ink: #0b0b0b;\n\
         \x20 --ink-secondary: #52514e;\n  --ink-muted: #898781;\n  --grid: #e1e0d9;\n\
         \x20 --axis: #c3c2b7;\n  --border: rgba(11, 11, 11, 0.10);\n",
    );
    for (index, colour) in light.iter().enumerate() {
        let _ = writeln!(css, "  --heat-{index}: {colour};");
    }
    css.push_str("}\n@media (prefers-color-scheme: dark) {\n  :root {\n");
    css.push_str(
        "    --surface: #1a1a19;\n    --plane: #0d0d0d;\n    --ink: #ffffff;\n\
         \x20   --ink-secondary: #c3c2b7;\n    --ink-muted: #898781;\n    --grid: #2c2c2a;\n\
         \x20   --axis: #383835;\n    --border: rgba(255, 255, 255, 0.10);\n",
    );
    for (index, colour) in dark.iter().enumerate() {
        let _ = writeln!(css, "    --heat-{index}: {colour};");
    }
    css.push_str("  }\n}\n");

    for index in 0..levels {
        let _ = writeln!(
            css,
            ".cells .c{index} {{ fill: var(--heat-{index}); }}\n.swatch.s{index} {{ background: var(--heat-{index}); }}"
        );
    }

    css.push_str(
        "
* { box-sizing: border-box; }
body {
  margin: 0;
  padding: 2rem clamp(1rem, 4vw, 3rem) 3rem;
  background: var(--plane);
  color: var(--ink);
  font-family: system-ui, -apple-system, 'Segoe UI', sans-serif;
  font-size: 15px;
  line-height: 1.5;
}
svg.defs { position: absolute; }
h1 { font-size: 1.35rem; font-weight: 650; margin: 0 0 0.35rem; }
h2 { font-size: 0.85rem; font-weight: 600; margin: 0 0 0.6rem; color: var(--ink-secondary); }
.lede { margin: 0; max-width: 68ch; color: var(--ink-secondary); }
.meta dl {
  display: flex;
  flex-wrap: wrap;
  gap: 0.35rem 2rem;
  margin: 1.25rem 0 0;
  padding: 0.85rem 1rem;
  background: var(--surface);
  border: 1px solid var(--border);
  border-radius: 10px;
}
.meta dt { font-size: 0.7rem; text-transform: uppercase; letter-spacing: 0.04em; color: var(--ink-muted); margin: 0; }
.meta dd { margin: 0; font-variant-numeric: tabular-nums; }
.legend { margin: 1.25rem 0 0; padding: 0.85rem 1rem; background: var(--surface); border: 1px solid var(--border); border-radius: 10px; }
.scale { display: flex; flex-wrap: wrap; gap: 0.25rem 0.9rem; list-style: none; margin: 0; padding: 0; }
.scale.extra { margin-top: 0.5rem; }
.scale li { display: flex; align-items: center; gap: 0.35rem; font-size: 0.75rem; color: var(--ink-secondary); font-variant-numeric: tabular-nums; }
.swatch { width: 15px; height: 15px; border-radius: 3px; display: inline-block; border: 1px solid var(--border); }
.swatch.blank { background: var(--surface); }
.swatch.hatched {
  background: repeating-linear-gradient(45deg, var(--surface) 0 3px, var(--grid) 3px 5px);
}
.note { margin: 0.6rem 0 0; font-size: 0.78rem; color: var(--ink-muted); max-width: 72ch; }
.facets {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(360px, 1fr));
  gap: 1rem;
  margin-top: 1.25rem;
}
.facet {
  margin: 0;
  padding: 0.75rem 0.75rem 0.5rem;
  background: var(--surface);
  border: 1px solid var(--border);
  border-radius: 10px;
  min-width: 0;
}
.facet figcaption { display: flex; justify-content: space-between; align-items: baseline; gap: 0.75rem; margin-bottom: 0.35rem; }
.facet-name { font-weight: 600; }
.facet-stats { font-size: 0.72rem; color: var(--ink-muted); font-variant-numeric: tabular-nums; }
.facet-plot { width: 100%; height: auto; display: block; }
.facet-plot .grid line { stroke: var(--grid); stroke-width: 1; }
.facet-plot .axis line { stroke: var(--axis); stroke-width: 1; }
.facet-plot .tick { fill: var(--ink-muted); font-size: 11px; font-variant-numeric: tabular-nums; }
.facet-plot .axis-title { fill: var(--ink-muted); font-size: 10px; }
.facet-plot .no-data { stroke: none; }
.facet-plot .hatch { stroke: var(--grid); stroke-width: 2; }
.table-view { margin-top: 0.5rem; font-size: 0.75rem; }
.table-view summary { cursor: pointer; color: var(--ink-muted); }
.table-scroll { overflow-x: auto; margin-top: 0.5rem; }
.table-view table { border-collapse: collapse; width: 100%; font-variant-numeric: tabular-nums; }
.table-view caption { text-align: left; color: var(--ink-muted); padding-bottom: 0.4rem; }
.table-view th, .table-view td { padding: 0.15rem 0.35rem; text-align: right; white-space: nowrap; }
.table-view thead th { color: var(--ink-muted); font-weight: 500; border-bottom: 1px solid var(--border); }
.table-view tbody th { text-align: left; font-weight: 500; color: var(--ink-secondary); }
.table-view tbody tr:nth-child(even) { background: var(--plane); }
.empty { margin-top: 1.25rem; color: var(--ink-secondary); }
.page-footer { margin-top: 2rem; font-size: 0.75rem; color: var(--ink-muted); }
",
    );
    css
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono_tz::Tz::UTC;

    use crate::aggregate::{analyse, build_grid};
    use crate::model::{Grid, Grouping, Sample};

    fn analysis(metric: Metric) -> Analysis {
        let buckets = BucketSpec::new(500.0, 2_000.0).unwrap();
        let samples: Vec<Sample> = (0..24)
            .map(|hour| {
                let start = 1_718_928_000 + hour * 3_600; // 2024-06-21, UTC
                Sample::new(start, start + 3_600, f64::from(hour as i32) * 100.0)
            })
            .collect();
        let grid = build_grid(&samples, Grouping::Month, buckets, UTC);
        analyse(&grid, metric, 1)
    }

    fn options() -> PageOptions {
        PageOptions {
            entity: "sensor.solar_power".to_string(),
            metadata: vec![("Source".to_string(), "statistics".to_string())],
            ..PageOptions::default()
        }
    }

    #[test]
    fn renders_a_complete_document() {
        let html = page(&analysis(Metric::Exceedance), &options());
        assert!(html.starts_with("<!doctype html>"));
        assert!(html.trim_end().ends_with("</html>"));
        assert!(html.contains("<title>Solar power likelihood - sensor.solar_power</title>"));
        assert!(html.contains("<style>"));
        assert!(html.contains("sensor.solar_power"));
        assert!(html.contains("statistics"));
    }

    #[test]
    fn renders_one_plot_per_facet_with_a_table_twin() {
        let analysis = analysis(Metric::Exceedance);
        let html = page(&analysis, &options());
        assert_eq!(analysis.facets.len(), 1, "the sample data is all in June");
        assert_eq!(html.matches("class=\"facet-plot\"").count(), 1);
        assert_eq!(html.matches("<table>").count(), 1);
        assert!(html.contains("June as a table"));
        assert!(html.contains("<figcaption>"));
    }

    #[test]
    fn is_self_contained() {
        let html = page(&analysis(Metric::Exceedance), &options());
        // No network references of any kind: the file has to work from a USB stick.
        assert!(!html.contains("http://"), "external reference in output");
        assert!(!html.contains("https://"), "external reference in output");
        assert!(!html.contains("<script"), "the page needs no javascript");
        assert!(!html.contains("<link"), "no external stylesheet");
    }

    #[test]
    fn defines_a_colour_for_every_level_in_both_themes() {
        let mut options = options();
        options.levels = 7;
        let html = page(&analysis(Metric::Exceedance), &options);
        for index in 0..7 {
            assert!(
                html.contains(&format!("--heat-{index}:")),
                "missing level {index}"
            );
            assert!(
                html.contains(&format!(".cells .c{index}")),
                "missing rule {index}"
            );
        }
        assert!(html.contains("prefers-color-scheme: dark"));
        // Both ramps must be present, not one flipped at runtime.
        assert!(html.contains(color::HEAT_LIGHT[0]));
        assert!(html.contains(color::HEAT_DARK[0]));
    }

    #[test]
    fn legend_covers_the_whole_probability_range() {
        let html = page(&analysis(Metric::Exceedance), &options());
        assert!(html.contains("Likelihood"));
        assert!(html.contains("class=\"swatch s0\""));
        assert!(html.contains(&format!("class=\"swatch s{}\"", color::DEFAULT_LEVELS - 1)));
        assert!(html.contains("fewer than"));
    }

    #[test]
    fn describes_the_metric_that_was_used() {
        let exceedance = page(&analysis(Metric::Exceedance), &options());
        assert!(exceedance.contains("at least that much"));
        assert!(exceedance.contains("exceedance metric"));

        let density = page(&analysis(Metric::Density), &options());
        assert!(density.contains("inside that power band"));
        assert!(density.contains("density metric"));
    }

    #[test]
    fn an_analysis_without_data_says_so() {
        let buckets = BucketSpec::new(50.0, 200.0).unwrap();
        let empty = analyse(&Grid::new(Grouping::Month, buckets), Metric::Exceedance, 1);
        let html = page(&empty, &options());
        assert!(html.contains("nothing to plot"));
        assert!(!html.contains("class=\"facet-plot\""));
    }

    #[test]
    fn user_supplied_text_is_escaped() {
        let mut options = options();
        options.entity = "sensor.<script>alert(1)</script>".to_string();
        options.metadata = vec![("Note".to_string(), "a & b".to_string())];
        let html = page(&analysis(Metric::Exceedance), &options);
        assert!(!html.contains("<script>alert"));
        assert!(html.contains("&lt;script&gt;"));
        assert!(html.contains("a &amp; b"));
    }
}
