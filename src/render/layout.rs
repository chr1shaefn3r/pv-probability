//! The document shell both reports share: the theme switch, the palette, the page
//! chrome, the metadata card and the History block.
//!
//! Two tools read the same database and answer different questions, and a reader moving
//! between their reports should not have to learn a second page. Everything here is
//! deliberately question-agnostic; what is specific to a report lives beside that
//! report's markup.

use std::fmt::Write as _;

use chrono_tz::Tz;

use crate::coverage::{Coverage, Gap};
use crate::model::Grouping;
use crate::render::color;
use crate::render::{escape, format_duration, format_percent};

/// Ids the stylesheet keys off; kept beside the markup that emits them.
pub const THEME_AUTO_ID: &str = "pv-theme-auto";
pub const THEME_LIGHT_ID: &str = "pv-theme-light";
pub const THEME_DARK_ID: &str = "pv-theme-dark";

/// Everything the shell needs to wrap a report body.
#[derive(Debug, Clone)]
pub struct Shell<'a> {
    /// Document title, also the page heading.
    pub title: &'a str,
    /// One sentence under the heading saying what the reader is looking at.
    pub lede: &'a str,
    /// The complete stylesheet, usually [`theme_stylesheet`] + [`CHROME_CSS`] + the
    /// report's own rules.
    pub stylesheet: &'a str,
    /// Markup placed before the theme radios, for SVG `<defs>` and the like.
    pub defs: &'a str,
    /// The line at the foot of the page.
    pub footer: &'a str,
}

/// Wrap a report body in the shared document.
pub fn document(shell: &Shell<'_>, body: &str) -> String {
    let mut html = String::with_capacity(body.len() + shell.stylesheet.len() + 4 * 1024);

    html.push_str("<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n");
    html.push_str("<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n");
    let _ = writeln!(html, "<title>{}</title>", escape(shell.title));
    let _ = writeln!(html, "<style>\n{}\n</style>", shell.stylesheet);
    html.push_str("</head>\n<body>\n");
    html.push_str(shell.defs);
    html.push_str(&theme_inputs());
    html.push_str("<div class=\"page\">");

    let _ = write!(
        html,
        "<header class=\"page-header\"><div class=\"heading\"><h1>{}</h1>{}</div>\
         <p class=\"lede\">{}</p></header>",
        escape(shell.title),
        theme_switch(),
        escape(shell.lede)
    );

    html.push_str(body);

    let _ = write!(
        html,
        "<footer class=\"page-footer\"><p>{}</p></footer>",
        escape(shell.footer)
    );
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
pub fn theme_inputs() -> String {
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
pub fn theme_switch() -> String {
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

/// The definition list at the top of a report: what was read, and how.
pub fn metadata_list(entries: &[(String, String)]) -> String {
    let mut html = String::from("<section class=\"meta\"><dl>");
    for (key, value) in entries {
        let _ = write!(
            html,
            "<div><dt>{}</dt><dd>{}</dd></div>",
            escape(key),
            escape(value)
        );
    }
    html.push_str("</dl></section>");
    html
}

/// The block that says what the recorder really covered.
///
/// A report built from a handful of scattered days is not wrong, but it means something
/// different from one built from two solid years, and the reader cannot tell the two
/// apart from the figures alone. `min_days` is the heatmap's evidence threshold; a report
/// that has no such rule passes `None`.
pub fn coverage_section(
    coverage: &Coverage,
    tz: Tz,
    grouping: Grouping,
    min_days: Option<u32>,
) -> String {
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
            "<p class=\"caution\">Less than a year of history: each {grouping} rests on a \
             single season rather than an average of several, and the {grouping}s that are \
             absent simply have not been recorded yet.</p>"
        );
    }
    if coverage.is_sparse() {
        html.push_str(
            "<p class=\"caution\">Large parts of the span were never recorded, so treat \
             the percentages as indicative.</p>",
        );
    }
    if let Some(min_days) = min_days
        && coverage.needs_caution()
    {
        let _ = write!(
            html,
            "<p>Every percentage in the plots above is conditional on the time that was \
             actually recorded: a sunny week the recorder missed is not represented at \
             all. The strip under each plot shows how many days back each hour, and hours \
             backed by fewer than {min_days} days are hatched rather than coloured.</p>"
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

/// The theme half of a stylesheet: the palette, the system preference, and the override.
pub fn theme_stylesheet(levels: usize) -> String {
    let mut css = String::with_capacity(4 * 1024);

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
    css
}

/// The light palette, as CSS custom property declarations.
pub fn light_tokens(levels: usize) -> String {
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
pub fn dark_tokens(levels: usize) -> String {
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

/// The page chrome every report wears: the body, the header, the theme switch, the
/// metadata card and the History block.
pub const CHROME_CSS: &str = "
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
.note { margin: 0.6rem 0 0; font-size: 0.78rem; color: var(--ink-muted); max-width: 72ch; }
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
.empty { margin-top: 1.25rem; color: var(--ink-secondary); }
.page-footer { margin-top: 2rem; font-size: 0.75rem; color: var(--ink-muted); }
";
