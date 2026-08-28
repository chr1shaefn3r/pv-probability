//! The page: header, scale legend, one heatmap per facet, and a table view of the same
//! numbers so nothing is readable only by colour or only on hover.

use std::fmt::Write as _;

use rayon::prelude::*;

use chrono_tz::Tz;

use crate::aggregate::{Analysis, Facet};
use crate::coverage::{Coverage, Gap};
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
    /// What the recorder actually covered, and where the outages are.
    pub coverage: Option<Coverage>,
    /// Timezone the coverage dates are stated in.
    pub tz: Tz,
    pub levels: usize,
    pub gamma: f64,
    pub min_probability: f64,
}

impl Default for PageOptions {
    fn default() -> Self {
        Self {
            entity: String::new(),
            metadata: Vec::new(),
            coverage: None,
            tz: Tz::UTC,
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
    html.push_str(&theme_inputs());
    html.push_str("<div class=\"page\">");

    let _ = write!(
        html,
        "<header class=\"page-header\"><div class=\"heading\"><h1>{}</h1>{}</div>\
         <p class=\"lede\">{}</p></header>",
        escape(&title),
        theme_switch(),
        escape(&lede(analysis))
    );

    html.push_str(&metadata_list(analysis, options));

    // The plots come first: they are what the report is for. The key and the caveats
    // about the history follow, where they are still to hand without standing in front of
    // the data.
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

    html.push_str(&legend(analysis, options));
    if let Some(coverage) = &options.coverage {
        html.push_str(&coverage_section(coverage, analysis, options.tz));
    }

    html.push_str(&footer(analysis));
    html.push_str("</div>\n</body>\n</html>\n");
    html
}

/// The radios that drive the colour theme.
///
/// They sit at the top of the body, before everything they style, because the stylesheet
/// selects the page with `#id:checked ~ .page`. A sibling selector rather than `:has()`:
/// this has worked in every browser since custom properties themselves, whereas `:has()`
/// only reached Firefox at the end of 2023, and a report is often opened in whatever
/// browser happens to be to hand. Either way the page needs no JavaScript.
fn theme_inputs() -> String {
    let mut html = String::new();
    for (id, checked) in [
        (THEME_AUTO_ID, true),
        (THEME_LIGHT_ID, false),
        (THEME_DARK_ID, false),
    ] {
        let _ = write!(
            html,
            "<input type=\"radio\" class=\"theme-input\" name=\"pv-theme\" id=\"{id}\"{}>",
            if checked { " checked" } else { "" }
        );
    }
    html
}

/// The visible switch. Labels can live anywhere, so the control sits in the header while
/// the radios it drives stay at the top of the body.
fn theme_switch() -> String {
    let mut html = String::from(
        "<fieldset class=\"theme-switch\"><legend class=\"visually-hidden\">Colour theme</legend>",
    );
    for (id, label) in [
        (THEME_AUTO_ID, "Auto"),
        (THEME_LIGHT_ID, "Light"),
        (THEME_DARK_ID, "Dark"),
    ] {
        let _ = write!(html, "<label for=\"{id}\">{label}</label>");
    }
    html.push_str("</fieldset>");
    html
}

/// Ids the stylesheet keys off; kept beside the markup that emits them.
const THEME_AUTO_ID: &str = "pv-theme-auto";
const THEME_LIGHT_ID: &str = "pv-theme-light";
const THEME_DARK_ID: &str = "pv-theme-dark";

fn lede(analysis: &Analysis) -> String {
    match analysis.metric {
        Metric::Exceedance => format!(
            "Each cell is the share of observed time at that hour with at least that much \
             power available, per {}. Darker red means more likely; the full scale, and \
             what the hatching means, are below the plots.",
            analysis.grouping
        ),
        Metric::Density => format!(
            "Each cell is the share of observed time at that hour spent inside that power \
             band, per {}. Darker red means more likely; the full scale, and what the \
             hatching means, are below the plots.",
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
        "<div><dt>Observed</dt><dd>{} days</dd></div>\
         <div><dt>Readings</dt><dd>{}</dd></div>",
        analysis.observed_days, analysis.total_samples
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
         <li><span class=\"swatch hatched thin\" aria-hidden=\"true\"></span>\
         <span class=\"swatch-label\">fewer than {} days recorded</span></li>\
         <li><span class=\"swatch hatched empty\" aria-hidden=\"true\"></span>\
         <span class=\"swatch-label\">never recorded</span></li>\
         </ul><ul class=\"scale extra\">\
         <li><span class=\"swatch-label\">days recorded per hour</span></li>\
         <li><span class=\"swatch cov-none\" aria-hidden=\"true\"></span>\
         <span class=\"swatch-label\">none</span></li>\
         {}\
         </ul>\
         <p class=\"note\">{}</p></section>",
        escape(&format_percent(options.min_probability)),
        analysis.min_days,
        (0..color::COVERAGE_LEVELS)
            .map(|level| format!(
                "<li><span class=\"swatch cov{level}\" aria-hidden=\"true\"></span>\
                 <span class=\"swatch-label\">{}</span></li>",
                if level + 1 == color::COVERAGE_LEVELS {
                    "most days".to_string()
                } else {
                    format!("{}%", (level + 1) * 100 / color::COVERAGE_LEVELS)
                }
            ))
            .collect::<String>(),
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

/// The header block that says what the recorder really covered.
///
/// A report built from a handful of scattered days is not wrong, but it means something
/// different from one built from two solid years, and the reader cannot tell the two
/// apart from the heatmaps alone.
fn coverage_section(coverage: &Coverage, analysis: &Analysis, tz: Tz) -> String {
    let (first, last) = coverage.local_dates(tz);
    let dates = match (first, last) {
        (Some(first), Some(last)) => format!("{first} to {last}"),
        _ => "an unknown span".to_string(),
    };

    let mut html = String::from("<section class=\"coverage-summary\">");
    let _ = write!(
        html,
        "<h2>History</h2><p>Covers <strong>{}</strong> ({} days), with data on \
         <strong>{} of them</strong> ({} of the span){}.</p>",
        escape(&dates),
        coverage.span_days,
        coverage.observed_days,
        escape(&format_percent(coverage.day_fraction())),
        // A local day picks up data as soon as one of its hours does, so say how many
        // were recorded properly rather than in passing.
        if coverage.full_days < coverage.observed_days {
            format!(", {} of them right through the day", coverage.full_days)
        } else {
            String::new()
        }
    );

    let threshold = format_duration(coverage.gap_threshold_seconds as f64);
    if coverage.gaps.is_empty() {
        let _ = write!(
            html,
            "<p>No outage longer than {} interrupts it.</p>",
            escape(&threshold)
        );
    } else {
        let longest = coverage
            .longest_gap()
            .expect("a non-empty gap list has a longest");
        let _ = write!(
            html,
            "<p>{} outage{} longer than {} ({} missing in total). The longest ran {}.</p>",
            coverage.gaps.len(),
            if coverage.gaps.len() == 1 { "" } else { "s" },
            escape(&threshold),
            escape(&format_duration(coverage.missing_seconds() as f64)),
            escape(&describe_gap(longest, tz))
        );
    }

    if !coverage.missing_facets.is_empty() {
        let _ = write!(
            html,
            "<p>No data at all for {}.</p>",
            escape(&coverage.missing_facets.join(", "))
        );
    }

    if !coverage.covers_full_year() {
        let _ = write!(
            html,
            "<p class=\"caution\">Less than a year of history: each {} rests on a single \
             season rather than an average of several, and the {}s that are absent simply \
             have not been recorded yet.</p>",
            analysis.grouping, analysis.grouping
        );
    }
    if coverage.is_sparse() {
        html.push_str(
            "<p class=\"caution\">Large parts of the span were never recorded, so treat \
             the percentages as indicative.</p>",
        );
    }
    if coverage.needs_caution() {
        let _ = write!(
            html,
            "<p>Every percentage in the plots above is conditional on the time that was \
             actually recorded: a sunny week the recorder missed is not represented at \
             all. The strip under each plot shows how many days back each hour, and hours \
             backed by fewer than {} days are hatched rather than coloured.</p>",
            analysis.min_days
        );
    }
    html.push_str("</section>");
    html
}

/// "3 d 4 h from 2025-07-03 to 2025-07-06".
fn describe_gap(gap: &Gap, tz: Tz) -> String {
    let format = |ts: i64| {
        chrono::DateTime::from_timestamp(ts, 0)
            .map(|utc| utc.with_timezone(&tz).format("%Y-%m-%d %H:%M").to_string())
            .unwrap_or_else(|| "?".to_string())
    };
    format!(
        "{} from {} to {}",
        format_duration(gap.seconds() as f64),
        format(gap.start_ts),
        format(gap.end_ts)
    )
}

/// The caption line under a facet's name: how much calendar it rests on.
fn facet_stats(facet: &Facet) -> String {
    if facet.possible_days > 0 {
        format!(
            "{} of {} days",
            facet.days.min(facet.possible_days.max(facet.days)),
            facet.possible_days
        )
    } else {
        format!("{} days", facet.days)
    }
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
         <span class=\"facet-stats\">{}</span></figcaption>",
        escape(&facet.label),
        escape(&facet_stats(facet))
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
                .is_some_and(|column| column.status.is_sufficient());
            let cell = if sufficient {
                format_percent(facet.value(hour, bucket))
            } else {
                "-".to_string()
            };
            let _ = write!(html, "<td>{}</td>", escape(&cell));
        }
        html.push_str("</tr>");
    }
    html.push_str("<tr class=\"days-row\"><th scope=\"row\">days</th>");
    for hour in 0..HOURS_PER_DAY {
        let days = facet.columns.get(hour).map_or(0, |column| column.days);
        let _ = write!(html, "<td>{days}</td>");
    }
    html.push_str("</tr></tbody></table></div></details>");
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

/// The light palette, as CSS custom property declarations.
fn light_tokens(levels: usize) -> String {
    let mut css = String::from(
        "  color-scheme: light;\n  --surface: #fcfcfb;\n  --plane: #f9f9f7;\n\
         \x20 --ink: #0b0b0b;\n  --ink-secondary: #52514e;\n  --ink-muted: #898781;\n\
         \x20 --grid: #e1e0d9;\n  --axis: #c3c2b7;\n  --border: rgba(11, 11, 11, 0.10);\n",
    );
    for (index, colour) in color::ramp(levels, false).iter().enumerate() {
        let _ = writeln!(css, "  --heat-{index}: {colour};");
    }
    for (index, colour) in color::COVERAGE_LIGHT.iter().enumerate() {
        let _ = writeln!(css, "  --cov-{index}: {colour};");
    }
    css
}

/// The dark palette, stepped for the dark surface rather than flipped.
fn dark_tokens(levels: usize) -> String {
    let mut css = String::from(
        "  color-scheme: dark;\n  --surface: #1a1a19;\n  --plane: #0d0d0d;\n\
         \x20 --ink: #ffffff;\n  --ink-secondary: #c3c2b7;\n  --ink-muted: #898781;\n\
         \x20 --grid: #2c2c2a;\n  --axis: #383835;\n  --border: rgba(255, 255, 255, 0.10);\n",
    );
    for (index, colour) in color::ramp(levels, true).iter().enumerate() {
        let _ = writeln!(css, "  --heat-{index}: {colour};");
    }
    for (index, colour) in color::COVERAGE_DARK.iter().enumerate() {
        let _ = writeln!(css, "  --cov-{index}: {colour};");
    }
    css
}

fn stylesheet(levels: usize) -> String {
    let mut css = String::with_capacity(8 * 1024);

    // Light is the base, and the system preference swaps it. This pair drives the page
    // chrome (the area behind an overscroll, for instance) and is what "Auto" means.
    let _ = writeln!(css, ":root {{\n{}}}", light_tokens(levels));
    let _ = writeln!(
        css,
        "@media (prefers-color-scheme: dark) {{\n  #{THEME_AUTO_ID}:checked ~ .page {{\n{}  }}\n\
         \x20 :root {{\n{}  }}\n}}",
        dark_tokens(levels),
        dark_tokens(levels)
    );
    // An explicit choice overrides both, in either direction: these are more specific than
    // `:root`, and apply whatever the system setting says.
    let _ = writeln!(
        css,
        "#{THEME_LIGHT_ID}:checked ~ .page {{\n{}}}",
        light_tokens(levels)
    );
    let _ = writeln!(
        css,
        "#{THEME_DARK_ID}:checked ~ .page {{\n{}}}",
        dark_tokens(levels)
    );

    for index in 0..levels {
        let _ = writeln!(
            css,
            ".cells .c{index} {{ fill: var(--heat-{index}); }}\n.swatch.s{index} {{ background: var(--heat-{index}); }}"
        );
    }
    for index in 0..color::COVERAGE_LEVELS {
        let _ = writeln!(
            css,
            ".coverage .cov{index} {{ fill: var(--cov-{index}); }}\n.swatch.cov{index} {{ background: var(--cov-{index}); }}"
        );
    }

    css.push_str(
        "
* { box-sizing: border-box; }
body {
  margin: 0;
  background: var(--plane);
  color: var(--ink);
  font-family: system-ui, -apple-system, 'Segoe UI', sans-serif;
  font-size: 15px;
  line-height: 1.5;
}
.page {
  min-height: 100vh;
  padding: 2rem clamp(1rem, 4vw, 3rem) 3rem;
  background: var(--plane);
  color: var(--ink);
}
.theme-input {
  position: absolute;
  width: 1px;
  height: 1px;
  opacity: 0;
}
svg.defs { position: absolute; }
h1 { font-size: 1.35rem; font-weight: 650; margin: 0 0 0.35rem; }
.heading {
  display: flex;
  flex-wrap: wrap;
  align-items: baseline;
  justify-content: space-between;
  gap: 0.5rem 1.5rem;
}
.visually-hidden {
  position: absolute;
  width: 1px;
  height: 1px;
  overflow: hidden;
  clip-path: inset(50%);
  white-space: nowrap;
}
.theme-switch {
  display: flex;
  margin: 0;
  padding: 2px;
  border: 1px solid var(--border);
  border-radius: 999px;
  background: var(--surface);
}
.theme-switch label {
  padding: 0.15rem 0.6rem;
  border-radius: 999px;
  font-size: 0.75rem;
  color: var(--ink-muted);
  cursor: pointer;
  user-select: none;
}
.theme-switch label:hover { color: var(--ink); }
#pv-theme-auto:checked ~ .page label[for='pv-theme-auto'],
#pv-theme-light:checked ~ .page label[for='pv-theme-light'],
#pv-theme-dark:checked ~ .page label[for='pv-theme-dark'] {
  background: var(--surface);
  color: var(--ink);
  box-shadow: inset 0 0 0 1px var(--border);
}
#pv-theme-auto:focus-visible ~ .page label[for='pv-theme-auto'],
#pv-theme-light:focus-visible ~ .page label[for='pv-theme-light'],
#pv-theme-dark:focus-visible ~ .page label[for='pv-theme-dark'] {
  outline: 2px solid var(--ink-secondary);
  outline-offset: 1px;
}
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
.swatch.hatched.empty { opacity: 0.4; }
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
.facet-plot .readout {
  display: none;
  fill: var(--ink);
  font-size: 11px;
  font-variant-numeric: tabular-nums;
  pointer-events: none;
}
.facet-plot .cell:hover .readout { display: block; }
.facet-plot .cell:hover rect { stroke: var(--ink); stroke-width: 1; paint-order: stroke; }
.facet-plot .axis-title { fill: var(--ink-muted); font-size: 10px; }
.facet-plot .no-data { stroke: none; opacity: 0.4; }
.facet-plot .thin-data { stroke: none; opacity: 0.9; }
.facet-plot .hatch { stroke: var(--grid); stroke-width: 2; }
.facet-plot .strip-label { fill: var(--ink-muted); font-size: 9px; }
.facet-plot .coverage rect { stroke: none; }
.facet-plot .cov-none { fill: none; }
.coverage-summary {
  margin: 1.25rem 0 0;
  padding: 0.85rem 1rem;
  background: var(--surface);
  border: 1px solid var(--border);
  border-radius: 10px;
}
.coverage-summary p { margin: 0 0 0.35rem; color: var(--ink-secondary); max-width: 78ch; }
.coverage-summary p:last-child { margin-bottom: 0; }
.coverage-summary .caution { color: var(--ink); }
.swatch.cov-none { background: var(--surface); }
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
    use crate::model::{DayWindow, Grid, Grouping, Sample};

    fn analysis(metric: Metric) -> Analysis {
        let buckets = BucketSpec::new(500.0, 2_000.0).unwrap();
        let samples: Vec<Sample> = (0..24)
            .map(|hour| {
                let start = 1_718_928_000 + hour * 3_600; // 2024-06-21, UTC
                Sample::new(start, start + 3_600, f64::from(hour as i32) * 100.0)
            })
            .collect();
        let grid = build_grid(&samples, Grouping::Month, buckets, UTC);
        analyse(&grid, metric, 1, &[])
    }

    /// The same options, plus a History block, so the order test sees every section.
    fn options_with_coverage() -> PageOptions {
        let samples: Vec<Sample> = (0..24)
            .map(|hour| {
                let start = 1_718_928_000 + hour * 3_600;
                Sample::new(start, start + 3_600, 500.0)
            })
            .collect();
        PageOptions {
            coverage: Coverage::describe(&samples, 1, 1, &[5], Grouping::Month, UTC, 24 * 3_600),
            ..options()
        }
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
    fn hovering_a_cell_is_what_reveals_its_readout() {
        let html = page(&analysis(Metric::Exceedance), &options());
        assert!(
            html.contains(".facet-plot .readout {"),
            "the readout is styled"
        );
        assert!(
            html.contains("display: none"),
            "readouts start hidden: {html:.0}"
        );
        assert!(
            html.contains(".facet-plot .cell:hover .readout { display: block; }"),
            "a plain :hover rule reveals it - no script, and no reliance on <title>"
        );
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
    fn the_theme_switch_offers_auto_light_and_dark() {
        let html = page(&analysis(Metric::Exceedance), &options());

        assert_eq!(html.matches("name=\"pv-theme\"").count(), 3);
        assert!(
            html.contains("id=\"pv-theme-auto\" checked"),
            "auto is the default"
        );
        assert!(html.contains(">Light</label>"));
        assert!(html.contains(">Dark</label>"));
        assert!(html.contains("Colour theme"), "the group is labelled");
        assert_eq!(html.matches("<label for=\"pv-theme-").count(), 3);
        // Still no script: the switch is the stylesheet reacting to a checked radio.
        assert!(!html.contains("<script"));
    }

    #[test]
    fn an_explicit_choice_beats_the_system_setting_in_both_directions() {
        let html = page(&analysis(Metric::Exceedance), &options());

        // The system setting only decides while "Auto" is selected ...
        assert!(
            html.contains("#pv-theme-auto:checked ~ .page"),
            "the system rule must be scoped to the auto choice"
        );
        // ... and either explicit choice overrides it, in both directions.
        assert!(html.contains("#pv-theme-light:checked ~ .page"));
        assert!(html.contains("#pv-theme-dark:checked ~ .page"));
        assert!(
            !html.contains(":has("),
            "the switch must not depend on :has(), which is too new to rely on"
        );
    }

    #[test]
    fn the_plots_come_before_the_key_and_the_caveats() {
        let html = page(&analysis(Metric::Exceedance), &options_with_coverage());
        let at = |needle: &str| {
            html.find(needle)
                .unwrap_or_else(|| panic!("missing {needle}"))
        };

        assert!(
            at("class=\"meta\"") < at("class=\"facets\""),
            "the plots follow the header"
        );
        assert!(
            at("class=\"facets\"") < at("class=\"legend\""),
            "the key follows the plots"
        );
        assert!(
            at("class=\"legend\"") < at("class=\"coverage-summary\""),
            "the history is last"
        );
        assert!(
            at("class=\"coverage-summary\"") < at("class=\"page-footer\""),
            "the footer stays at the bottom"
        );
    }

    #[test]
    fn the_radios_come_before_the_page_they_style() {
        // A sibling selector only reaches forwards, so the inputs have to be first.
        let html = page(&analysis(Metric::Exceedance), &options());
        let first_input = html.find("class=\"theme-input\"").expect("the radios");
        let wrapper = html.find("<div class=\"page\">").expect("the wrapper");
        assert!(first_input < wrapper, "the radios must precede .page");
        assert!(html.contains("</div>\n</body>"), "the wrapper is closed");
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
        let empty = analyse(
            &Grid::new(Grouping::Month, buckets, DayWindow::empty()),
            Metric::Exceedance,
            1,
            &[],
        );
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
