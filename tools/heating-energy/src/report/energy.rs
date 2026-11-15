//! The Heating energy section: every month's energy against its heating degree days, the
//! line through them, and its slope season by season.

use std::fmt::Write as _;

use ha_report::axis::nice_ticks;
use ha_report::escape;

use super::{column, format_degree_days};
use crate::energy::{Energy, Fit, MonthPoint, Season, format_energy, format_slope};

/// What the report needs to say about heating energy.
#[derive(Debug, Clone, PartialEq)]
pub struct EnergySection {
    /// The entity the monthly energy came from.
    pub entity: String,
    pub energy: Energy,
    /// Whether it came from raw states, which Home Assistant purges after ten days by
    /// default, rather than from long-term statistics.
    pub from_raw_states: bool,
}

// Geometry, in user units, matching the degree day chart above it.
const WIDTH: f64 = 880.0;
const LEFT: f64 = 64.0;
const RIGHT: f64 = 16.0;
/// Room above the plot for the hover readout.
const TOP: f64 = 30.0;
const READOUT_BASELINE: f64 = 14.0;
const SCATTER_HEIGHT: f64 = 260.0;
const BARS_HEIGHT: f64 = 160.0;
const BOTTOM: f64 = 40.0;
/// Markers 8px across, with a ring of surface round each so overlapping months stay
/// apart.
const DOT_RADIUS: f64 = 4.0;
/// Hover targets bigger than the marks.
const HIT_RADIUS: f64 = 10.0;
const MAX_BAR: f64 = 48.0;
/// Room the line's label takes beside it, at 10px.
const LINE_LABEL_WIDTH: f64 = 130.0;

pub fn section(section: &EnergySection) -> String {
    let energy = &section.energy;
    let mut html = String::from("<section class=\"panel energy\"><h2>Heating energy</h2>");
    let _ = write!(
        html,
        "<p>Each month's heating energy, from {}, against its heating degree days. The slope of \
         the line through the months is what one more heating degree day costs this house: \
         the weather only moves a month along the line, so insulation, new windows or a boiler \
         tune-up show as a lower slope however cold the winter was. Where the line meets zero \
         degree days is what the house uses without heating - hot water, standby.</p>",
        escape(&section.entity)
    );

    let Some(overall) = energy.overall else {
        let _ = write!(
            html,
            "<p class=\"empty\">There is no line to draw yet: {} - a line needs {} months, \
             spread over milder and colder ones.{}</p></section>",
            escape(&months_on_record(energy)),
            crate::energy::MIN_FIT_MONTHS,
            if section.from_raw_states {
                " Home Assistant keeps this entity's values only in its raw states, ten days \
                 by default; see the README for keeping them."
            } else {
                ""
            }
        );
        return html;
    };

    let _ = write!(
        html,
        "<p class=\"headline\"><strong>{} kWh</strong> per heating degree day (± {}), and \
         <strong>{}</strong> a month without heating - the line accounts for {} of the months' \
         spread over {} months.</p>",
        escape(&format_slope(overall.slope)),
        escape(&format_slope(overall.slope_error)),
        escape(&format_energy(overall.base)),
        escape(&percent(overall.r_squared)),
        overall.months
    );
    html.push_str(&scatter(energy, &overall));

    let fitted: Vec<&Season> = energy
        .seasons
        .iter()
        .filter(|season| season.fit.is_some())
        .collect();
    html.push_str("<h3>Per heating season</h3>");
    if fitted.len() >= 2 {
        html.push_str(&season_bars(&fitted));
    }
    html.push_str(&season_table(energy, &overall));
    html.push_str(&month_table(energy));

    let left_out = energy
        .months
        .iter()
        .filter(|month| !month.is_fitted())
        .count();
    let mut note = String::from(
        "Seasons run from July to June, so a winter is never split. The ± is the slope's \
         standard error: two seasons further apart than their errors put together differ by \
         more than chance.",
    );
    if left_out > 0 {
        let _ = write!(
            note,
            " {left_out} month{} with a value {} left out: the outside thermometer counted \
             less than nine in ten of {} days.",
            if left_out == 1 { "" } else { "s" },
            if left_out == 1 { "is" } else { "are" },
            if left_out == 1 { "its" } else { "their" }
        );
    }
    let _ = write!(
        note,
        " A month missing fewer days has its degree days scaled up to all of them, since the \
         energy covers the whole month."
    );
    if !energy.unrecorded.is_empty() {
        let names: Vec<String> = energy
            .unrecorded
            .iter()
            .map(|month| month.label())
            .collect();
        let _ = write!(
            note,
            " No value is on record for {}: setting {} to the value it already had leaves no \
             new row in the database, so {} is not guessed at.",
            names.join(", "),
            section.entity,
            if names.len() == 1 { "it" } else { "they are" }
        );
    }
    let _ = write!(html, "<p class=\"note\">{}</p></section>", escape(&note));
    html
}

/// `3 months have a value (January to March 2025)`, for when there is no line.
fn months_on_record(energy: &Energy) -> String {
    let fitted = energy.fitted().count();
    match (energy.months.first(), energy.months.last()) {
        (Some(first), Some(last)) => format!(
            "{} month{} {} a value where the outside was recorded ({}{}), {fitted} of {} \
             recorded fully enough",
            energy.months.len(),
            if energy.months.len() == 1 { "" } else { "s" },
            if energy.months.len() == 1 {
                "has"
            } else {
                "have"
            },
            first.month.label(),
            if first.month == last.month {
                String::new()
            } else {
                format!(" to {}", last.month.label())
            },
            if energy.months.len() == 1 {
                "it"
            } else {
                "them"
            }
        ),
        _ => "no month has a value where the outside was recorded".to_string(),
    }
}

/// `97%`.
fn percent(share: f64) -> String {
    format!("{:.0}%", share * 100.0)
}

/// An axis from `low` to `high` on round steps: the steps, and where it starts and ends.
fn axis(low: f64, high: f64) -> (Vec<f64>, f64, f64) {
    let span = (high - low.min(0.0)).max(f64::MIN_POSITIVE);
    let step = nice_ticks(span, 4).get(1).copied().unwrap_or(1.0);
    let bottom = (low.min(0.0) / step).floor() * step;
    let top = ((high / step).ceil() * step).max(bottom + step);
    let mut ticks = Vec::new();
    let mut tick = bottom;
    while tick <= top + step * 1e-9 {
        ticks.push(tick);
        tick += step;
    }
    (ticks, bottom, top)
}

/// Every fitted month as a dot, kWh up and heating degree days across, and the line
/// through them.
fn scatter(energy: &Energy, overall: &Fit) -> String {
    let months: Vec<&MonthPoint> = energy.fitted().collect();
    let max_hdd = months
        .iter()
        .map(|month| month.whole_month_hdd())
        .fold(0.0, f64::max);
    let line_end = overall.base + overall.slope * max_hdd;
    let low_kwh = months
        .iter()
        .map(|month| month.kwh)
        .fold(overall.base.min(line_end), f64::min);
    let high_kwh = months
        .iter()
        .map(|month| month.kwh)
        .fold(overall.base.max(line_end), f64::max);
    let (x_ticks, _, x_top) = axis(0.0, max_hdd);
    let (y_ticks, y_bottom, y_top) = axis(low_kwh, high_kwh);
    let x = |hdd: f64| LEFT + hdd / x_top * (WIDTH - LEFT - RIGHT);
    let y = |kwh: f64| TOP + (y_top - kwh) / (y_top - y_bottom) * SCATTER_HEIGHT;
    let height = TOP + SCATTER_HEIGHT + BOTTOM;

    let mut svg = format!(
        "<svg class=\"energy-chart\" viewBox=\"0 0 {WIDTH:.0} {height:.0}\" role=\"img\" \
         aria-label=\"Monthly heating energy against heating degree days, with the line \
         through them\" preserveAspectRatio=\"xMidYMid meet\"><g class=\"grid\">"
    );
    for tick in &y_ticks {
        let _ = write!(
            svg,
            "<line x1=\"{LEFT:.1}\" y1=\"{0:.1}\" x2=\"{1:.1}\" y2=\"{0:.1}\"/><text \
             class=\"tick\" x=\"{2:.1}\" y=\"{3:.1}\" text-anchor=\"end\">{4}</text>",
            y(*tick),
            WIDTH - RIGHT,
            LEFT - 6.0,
            y(*tick) + 4.0,
            escape(&format_energy(*tick).replace(" kWh", ""))
        );
    }
    for tick in &x_ticks {
        let _ = write!(
            svg,
            "<text class=\"tick\" x=\"{:.1}\" y=\"{:.1}\" text-anchor=\"middle\">{}</text>",
            x(*tick),
            TOP + SCATTER_HEIGHT + 16.0,
            escape(&format_degree_days(*tick))
        );
    }
    let _ = write!(
        svg,
        "<text class=\"axis-title\" x=\"{LEFT:.1}\" y=\"{:.1}\">kWh in the month</text>\
         <text class=\"axis-title\" x=\"{:.1}\" y=\"{:.1}\" text-anchor=\"end\">heating degree \
         days in the month</text></g>",
        TOP - 4.0,
        WIDTH - RIGHT,
        TOP + SCATTER_HEIGHT + 32.0
    );
    let _ = write!(
        svg,
        "<g class=\"axis\"><line x1=\"{LEFT:.1}\" y1=\"{0:.1}\" x2=\"{1:.1}\" y2=\"{0:.1}\"/>\
         </g>",
        y(y_bottom.max(0.0).min(y_top)),
        WIDTH - RIGHT
    );

    // The line under the months, labelled with what it says: past its far end, where the
    // axis has room and no month can be, or else under it, away from the coldest month.
    let (label_x, label_y, anchor) = if WIDTH - RIGHT - x(max_hdd) >= LINE_LABEL_WIDTH {
        (x(max_hdd) + 8.0, y(line_end) + 4.0, "start")
    } else {
        (x(max_hdd), y(line_end) + 18.0, "end")
    };
    let _ = write!(
        svg,
        "<line class=\"fit\" x1=\"{:.1}\" y1=\"{:.1}\" x2=\"{:.1}\" y2=\"{:.1}\"/><text \
         class=\"value\" x=\"{label_x:.1}\" y=\"{label_y:.1}\" text-anchor=\"{anchor}\">{} \
         kWh per degree day</text>",
        x(0.0),
        y(overall.base),
        x(max_hdd),
        y(line_end),
        escape(&format_slope(overall.slope))
    );

    svg.push_str("<g class=\"series\">");
    for month in &months {
        let (cx, cy) = (x(month.whole_month_hdd()), y(month.kwh));
        let expected = overall.base + overall.slope * month.whole_month_hdd();
        let _ = write!(
            svg,
            "<g class=\"slot\"><circle class=\"hit\" cx=\"{cx:.1}\" cy=\"{cy:.1}\" \
             r=\"{HIT_RADIUS:.1}\"/><circle class=\"dot\" cx=\"{cx:.1}\" cy=\"{cy:.1}\" \
             r=\"{DOT_RADIUS:.1}\"/><text class=\"readout\" x=\"{LEFT:.1}\" \
             y=\"{READOUT_BASELINE:.1}\">{}</text></g>",
            escape(&format!(
                "{}: {} at {} heating degree days - {} {} the line",
                month.month.label(),
                format_energy(month.kwh),
                format_degree_days(month.whole_month_hdd()),
                format_energy((month.kwh - expected).abs()),
                if month.kwh >= expected {
                    "above"
                } else {
                    "below"
                }
            ))
        );
    }
    svg.push_str("</g></svg>");
    svg
}

/// The slope of every season that has one, as a bar with its standard error.
fn season_bars(seasons: &[&Season]) -> String {
    let fits: Vec<Fit> = seasons.iter().filter_map(|season| season.fit).collect();
    let high = fits
        .iter()
        .map(|fit| fit.slope + fit.slope_error)
        .fold(0.0, f64::max);
    // Headroom over the tallest whisker for its label, clear of the axis title.
    let (ticks, _, top) = axis(0.0, high * 1.15);
    let height = TOP + BARS_HEIGHT + BOTTOM;
    let baseline = TOP + BARS_HEIGHT;
    let y = |slope: f64| baseline - slope.max(0.0) / top * BARS_HEIGHT;
    let slot = (WIDTH - LEFT - RIGHT) / seasons.len() as f64;
    let bar = (slot * 0.5).min(MAX_BAR);

    let mut svg = format!(
        "<svg class=\"energy-chart\" viewBox=\"0 0 {WIDTH:.0} {height:.0}\" role=\"img\" \
         aria-label=\"kWh per heating degree day, season by season\" \
         preserveAspectRatio=\"xMidYMid meet\"><g class=\"grid\">"
    );
    for tick in ticks.iter().skip(1) {
        let _ = write!(
            svg,
            "<line x1=\"{LEFT:.1}\" y1=\"{0:.1}\" x2=\"{1:.1}\" y2=\"{0:.1}\"/><text \
             class=\"tick\" x=\"{2:.1}\" y=\"{3:.1}\" text-anchor=\"end\">{4}</text>",
            y(*tick),
            WIDTH - RIGHT,
            LEFT - 6.0,
            y(*tick) + 4.0,
            escape(&format_slope(*tick))
        );
    }
    let _ = write!(
        svg,
        "<text class=\"axis-title\" x=\"{LEFT:.1}\" y=\"{:.1}\">kWh per heating degree day\
         </text></g><g class=\"series\">",
        TOP - 4.0
    );
    for (index, season) in seasons.iter().enumerate() {
        let Some(fit) = season.fit else {
            continue;
        };
        let left = LEFT + index as f64 * slot;
        let centre = left + slot / 2.0;
        let mut marks = format!(
            "<rect class=\"hit\" x=\"{left:.1}\" y=\"{TOP:.1}\" width=\"{slot:.1}\" \
             height=\"{BARS_HEIGHT:.1}\"/>"
        );
        if fit.slope > 0.0 {
            let _ = write!(
                marks,
                "<path class=\"bar\" d=\"{}\"/>",
                column(centre - bar / 2.0, bar, baseline, y(fit.slope))
            );
        }
        let (low, high) = (
            y(fit.slope - fit.slope_error),
            y(fit.slope + fit.slope_error),
        );
        let _ = write!(
            marks,
            "<path class=\"whisker\" d=\"M{centre:.1},{low:.1}V{high:.1}M{:.1},{high:.1}H{:.1}\
             M{:.1},{low:.1}H{:.1}\"/><text class=\"value\" x=\"{centre:.1}\" y=\"{:.1}\" \
             text-anchor=\"middle\">{}</text>",
            centre - 5.0,
            centre + 5.0,
            centre - 5.0,
            centre + 5.0,
            high - 6.0,
            escape(&format_slope(fit.slope))
        );
        let _ = write!(
            svg,
            "<g class=\"slot\">{marks}<text class=\"readout\" x=\"{LEFT:.1}\" \
             y=\"{READOUT_BASELINE:.1}\">{}</text></g>",
            escape(&format!(
                "{}: {} kWh per degree day ± {}, {} a month without heating, over {} months",
                season.label(),
                format_slope(fit.slope),
                format_slope(fit.slope_error),
                format_energy(fit.base),
                fit.months
            ))
        );
    }
    let _ = write!(
        svg,
        "</g><g class=\"axis\"><line x1=\"{LEFT:.1}\" y1=\"{baseline:.1}\" x2=\"{:.1}\" \
         y2=\"{baseline:.1}\"/>",
        WIDTH - RIGHT
    );
    for (index, season) in seasons.iter().enumerate() {
        let _ = write!(
            svg,
            "<text class=\"tick\" x=\"{:.1}\" y=\"{:.1}\" text-anchor=\"middle\">{}</text>",
            LEFT + (index as f64 + 0.5) * slot,
            baseline + 16.0,
            escape(&season.label())
        );
    }
    svg.push_str("</g></svg>");
    svg
}

/// The table twin of the season bars, every season included.
fn season_table(energy: &Energy, overall: &Fit) -> String {
    let mut html = String::from(
        "<div class=\"table-scroll\"><table><caption>Per heating season, July to June. A \
         season needs three fitted months, milder and colder ones, for a line of its own.\
         </caption><thead><tr><th scope=\"col\">Season</th><th scope=\"col\">Months</th>\
         <th scope=\"col\">Heating energy</th><th scope=\"col\">kWh per degree day</th>\
         <th scope=\"col\">±</th><th scope=\"col\">Without heating</th><th scope=\"col\">Fit\
         </th><th scope=\"col\">Against the season before</th></tr></thead><tbody>",
    );
    let mut previous: Option<f64> = None;
    for season in &energy.seasons {
        let months = if season.fitted == season.months {
            season.months.to_string()
        } else {
            format!("{} of {}", season.fitted, season.months)
        };
        let change = match (season.fit, previous) {
            (Some(fit), Some(before)) if before > 0.0 => {
                let change = fit.slope / before - 1.0;
                format!(
                    "{}{:.0}%",
                    if change > 0.0 { "+" } else { "" },
                    change * 100.0
                )
            }
            _ => "-".to_string(),
        };
        let _ = write!(
            html,
            "<tr><th scope=\"row\">{}</th><td>{}</td><td>{}</td>{}<td>{}</td></tr>",
            escape(&season.label()),
            escape(&months),
            escape(&format_energy(season.kwh)),
            fit_cells(season.fit.as_ref()),
            escape(&change)
        );
        if let Some(fit) = season.fit {
            previous = Some(fit.slope);
        }
    }
    let _ = write!(
        html,
        "</tbody><tfoot><tr><th scope=\"row\">All</th><td>{}</td><td>{}</td>{}<td></td></tr>\
         </tfoot></table></div>",
        overall.months,
        escape(&format_energy(
            energy.fitted().map(|month| month.kwh).sum::<f64>()
        )),
        fit_cells(Some(overall))
    );
    html
}

fn fit_cells(fit: Option<&Fit>) -> String {
    match fit {
        Some(fit) => format!(
            "<td>{}</td><td>{}</td><td>{}</td><td>{}</td>",
            escape(&format_slope(fit.slope)),
            escape(&format_slope(fit.slope_error)),
            escape(&format_energy(fit.base)),
            escape(&percent(fit.r_squared))
        ),
        None => "<td>-</td><td>-</td><td>-</td><td>-</td>".to_string(),
    }
}

/// Every month with a value, fitted or not, behind a fold.
fn month_table(energy: &Energy) -> String {
    let mut html = String::from(
        "<details class=\"floor\"><summary>Every month</summary><div class=\"table-scroll\">\
         <table><caption>Each month's energy and heating degree days, scaled up to the whole \
         month where a few days went uncounted.</caption><thead><tr><th scope=\"col\">Month\
         </th><th scope=\"col\">Heating energy</th><th scope=\"col\">Heating degree days</th>\
         <th scope=\"col\">Days counted</th><th scope=\"col\">In the fit</th></tr></thead>\
         <tbody>",
    );
    for month in &energy.months {
        let _ = write!(
            html,
            "<tr><th scope=\"row\">{}</th><td>{}</td><td>{}</td><td>{} of {}</td><td>{}</td>\
             </tr>",
            escape(&month.month.label()),
            escape(&format_energy(month.kwh)),
            escape(&format_degree_days(month.whole_month_hdd())),
            month.counted_days,
            month.month.days(),
            if month.is_fitted() {
                "yes"
            } else {
                "no, too few days"
            }
        );
    }
    html.push_str("</tbody></table></div></details>");
    html
}

/// The rules only this section needs, added to the page when it is there.
pub const CSS: &str = "
.energy h3 { font-size: 0.85rem; font-weight: 600; margin: 1rem 0 0.25rem; color: var(--ink); }
.energy .headline { color: var(--ink-secondary); }
.energy-chart { width: 100%; height: auto; display: block; margin: 0.25rem 0 0.75rem; }
.energy-chart .grid line { stroke: var(--grid); stroke-width: 1; }
.energy-chart .axis line { stroke: var(--axis); stroke-width: 1; }
.energy-chart .tick { fill: var(--ink-muted); font-size: 11px; font-variant-numeric: tabular-nums; }
.energy-chart .axis-title { fill: var(--ink-muted); font-size: 10px; }
.energy-chart .value { fill: var(--ink-secondary); font-size: 10px; font-variant-numeric: tabular-nums; }
.energy-chart .fit { stroke: var(--ink-secondary); stroke-width: 2; stroke-linecap: round; }
.energy-chart .dot { fill: var(--hdd); stroke: var(--surface); stroke-width: 2; }
.energy-chart .bar { fill: var(--hdd); }
.energy-chart .whisker { stroke: var(--ink-secondary); stroke-width: 1.5; fill: none; }
.energy-chart .hit { fill: transparent; }
.energy-chart .slot:hover .dot { stroke: var(--ink); }
.energy-chart rect.hit:hover { fill: var(--grid); opacity: 0.45; }
.energy-chart .readout {
  display: none;
  fill: var(--ink);
  font-size: 11px;
  font-variant-numeric: tabular-nums;
  pointer-events: none;
}
.energy-chart .slot:hover .readout { display: block; }
";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::energy::{Month, Season};

    fn point(year: i32, month: u32, hdd: f64, kwh: f64) -> MonthPoint {
        let month = Month { year, month };
        MonthPoint {
            month,
            kwh,
            counted_days: month.days(),
            hdd,
        }
    }

    fn energy(months: Vec<MonthPoint>) -> Energy {
        let fit = |months: &[MonthPoint]| {
            crate::energy::fit(
                &months
                    .iter()
                    .map(|month| (month.whole_month_hdd(), month.kwh))
                    .collect::<Vec<_>>(),
            )
        };
        let mut seasons: Vec<Season> = Vec::new();
        for start_year in [2022, 2023] {
            let own: Vec<MonthPoint> = months
                .iter()
                .copied()
                .filter(|month| month.month.season() == start_year)
                .collect();
            seasons.push(Season {
                start_year,
                months: own.len(),
                fitted: own.len(),
                kwh: own.iter().map(|month| month.kwh).sum(),
                fit: fit(&own),
            });
        }
        Energy {
            overall: fit(&months),
            seasons,
            months,
            beyond_record: 0,
            unrecorded: Vec::new(),
        }
    }

    fn section_of(energy: Energy) -> String {
        section(&EnergySection {
            entity: "number.heizol_energy".to_string(),
            energy,
            from_raw_states: true,
        })
    }

    #[test]
    fn the_section_states_the_slope_draws_the_months_and_compares_seasons() {
        let mut months = Vec::new();
        for (year, month, hdd) in [(2022, 11, 300.0), (2022, 12, 450.0), (2023, 1, 480.0)] {
            months.push(point(year, month, hdd, 3.0 * hdd + 400.0));
        }
        for (year, month, hdd) in [(2023, 11, 310.0), (2023, 12, 440.0), (2024, 1, 500.0)] {
            months.push(point(year, month, hdd, 2.4 * hdd + 400.0));
        }
        let html = section_of(energy(months));
        assert!(html.contains("<h2>Heating energy</h2>"));
        assert!(html.contains("number.heizol_energy"));
        assert!(html.contains("<strong>"), "a headline figure");
        assert_eq!(html.matches("class=\"dot\"").count(), 6, "one dot a month");
        assert_eq!(html.matches("class=\"fit\"").count(), 1, "one line");
        assert_eq!(html.matches("class=\"bar\"").count(), 2, "a bar a season");
        assert!(html.contains("<th scope=\"row\">2023/24</th>"));
        assert!(html.contains("<td>-20%</td>"), "2.4 against 3.0");
        assert!(html.contains("January 2024: 1,600 kWh at 500 heating degree days"));
    }

    #[test]
    fn without_enough_months_it_says_what_there_is_and_why() {
        let html = section_of(energy(vec![point(2024, 1, 450.0, 1_800.0)]));
        assert!(html.contains("no line to draw yet"), "{html}");
        assert!(html.contains("1 month has a value where the outside was recorded (January 2024)"));
        assert!(html.contains("ten days by default"));
        assert!(!html.contains("<svg"));
    }

    #[test]
    fn axes_start_at_zero_and_end_on_a_round_step() {
        let (ticks, bottom, top) = axis(0.0, 480.0);
        assert_eq!(ticks, [0.0, 200.0, 400.0, 600.0]);
        assert_eq!((bottom, top), (0.0, 600.0));
        let (ticks, bottom, top) = axis(-120.0, 1_900.0);
        assert!(bottom <= -120.0, "a line below zero is not cut off");
        assert!(top >= 1_900.0);
        assert!(ticks.contains(&0.0));
        assert_eq!(ticks.first(), Some(&bottom));
        assert_eq!(ticks.last(), Some(&top));
    }
}
