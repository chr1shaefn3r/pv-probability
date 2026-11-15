//! The Heating energy section: every month's heating energy - what the running total grew
//! by over it, less the water heating its season's warmest months tell - against its
//! heating degree days, the line through them, and its slope season by season.

use std::fmt::Write as _;

use ha_report::axis::nice_ticks;
use ha_report::escape;

use super::{column, format_degree_days};
use crate::energy::{
    Energy, Fit, LeftOut, MIN_FIT_MONTHS, Month, MonthPoint, Season, format_energy, format_monthly,
    format_slope, month_list, season_label,
};

/// What the report needs to say about heating energy.
#[derive(Debug, Clone, PartialEq)]
pub struct EnergySection {
    /// The running total the energy came from.
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
        "<p>Each month's heating energy - what the running total {} grew by from the month's \
         start to its end, less what heating water took - against its heating degree days. \
         Water heating does not follow the weather: what each season's warmest months used a \
         day, with the heating off, is taken off every day of the season. The slope of the line \
         through the months is what one more heating degree day costs this house: the weather \
         only moves a month along the line, so insulation, new windows or a boiler tune-up show \
         as a lower slope however cold the winter was.</p>",
        escape(&section.entity)
    );

    let Some(overall) = energy.overall else {
        let _ = write!(
            html,
            "<p class=\"empty\">There is no line to draw yet: {} - a line needs {} months, \
             spread over milder and colder ones.{}</p></section>",
            escape(&months_on_record(energy)),
            MIN_FIT_MONTHS,
            if section.from_raw_states {
                " Home Assistant keeps this entity's readings only in its raw states, ten \
                 days by default; see the README for keeping them."
            } else {
                ""
            }
        );
        return html;
    };

    let water = match energy.water_per_month {
        Some(per_month) => {
            let own = energy
                .seasons
                .iter()
                .filter(|season| {
                    season
                        .water
                        .as_ref()
                        .is_some_and(|water| water.from_season == season.start_year)
                })
                .count();
            format!(
                ", once water heating of <strong>{}</strong>{} is taken off",
                escape(&format_monthly(per_month)),
                if own > 1 {
                    " on average over the seasons"
                } else {
                    ""
                }
            )
        }
        // No summer to tell water heating by: it stays in, and the line's base holds it.
        None => format!(
            ", and <strong>{}</strong> without heating",
            escape(&format_monthly(overall.base))
        ),
    };
    let _ = write!(
        html,
        "<p class=\"headline\"><strong>{} kWh</strong> per heating degree day (± {}){water} - \
         the line accounts for {} of the months' spread over {} months.</p>",
        escape(&format_slope(overall.slope)),
        escape(&format_slope(overall.slope_error)),
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

    let mut note = String::from(
        "Seasons run from July to June, so a winter is never split. The ± is the slope's \
         standard error: two seasons further apart than their errors put together differ by \
         more than chance. A month the outside thermometer missed a day or two of has its \
         degree days scaled up to all of them, since the energy covers the whole month. ",
    );
    note.push_str(&water_note(energy));
    for (reason, why) in [
        (
            LeftOut::FewDays,
            "the outside thermometer counted less than nine in ten of its days",
        ),
        (
            LeftOut::Unchanged,
            "the total did not change over it - not read, or nothing used",
        ),
        (
            LeftOut::AfterUnchanged,
            "the total did not change over the month before, so it may hold both",
        ),
        (
            LeftOut::WentDown,
            "the total went down over it - a meter replaced, or a reading put right late",
        ),
    ] {
        let names: Vec<String> = energy
            .months
            .iter()
            .filter(|month| month.left_out() == Some(reason))
            .map(|month| month.month.label())
            .collect();
        if !names.is_empty() {
            let _ = write!(note, " Left out, {why}: {}.", names.join(", "));
        }
    }
    let _ = write!(html, "<p class=\"note\">{}</p></section>", escape(&note));
    html
}

/// Where every season's water heating came from.
fn water_note(energy: &Energy) -> String {
    if energy.water_per_month.is_none() {
        return "Water heating is left in: no season has three months from May to September \
                on record to tell it by, so the line's base holds it."
            .to_string();
    }
    let sources: Vec<String> = energy
        .seasons
        .iter()
        .filter_map(|season| {
            let water = season.water.as_ref()?;
            Some(if water.from_season == season.start_year {
                format!("{}'s from {}", season.label(), month_list(&water.months))
            } else {
                format!(
                    "{} takes {}'s, having fewer of its own on record",
                    season.label(),
                    season_label(water.from_season)
                )
            })
        })
        .collect();
    format!(
        "Water heating is what a season's three warmest months from May to September used a \
         day, with the heating off, taken off every day of the season: {}.",
        sources.join("; ")
    )
}

/// `the total covers 3 whole months where the outside was recorded (January to March
/// 2025), 2 of them fit to use`, for when there is no line.
fn months_on_record(energy: &Energy) -> String {
    let fitted = energy.fitted().count();
    match (energy.months.first(), energy.months.last()) {
        (Some(first), Some(last)) => format!(
            "the total covers {} whole month{} where the outside was recorded ({}{}), {fitted} \
             of {} fit to use",
            energy.months.len(),
            if energy.months.len() == 1 { "" } else { "s" },
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
        _ => format!(
            "{} reading{} of the total cover{} no whole month where the outside was recorded",
            energy.readings,
            if energy.readings == 1 { "" } else { "s" },
            if energy.readings == 1 { "s" } else { "" }
        ),
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
        .map(|month| month.heating_kwh())
        .fold(overall.base.min(line_end), f64::min);
    let high_kwh = months
        .iter()
        .map(|month| month.heating_kwh())
        .fold(overall.base.max(line_end), f64::max);
    // A quarter more room to the right than the coldest month needs, where the line's
    // label goes: no month can be there.
    let (x_ticks, _, x_top) = axis(0.0, max_hdd * 1.25);
    // A summer month's heating comes out a little either side of nothing once its water
    // heating is taken off: a few kWh below it sit on the baseline rather than open up a
    // whole step of axis.
    let axis_low = if low_kwh > -0.05 * high_kwh {
        low_kwh.max(0.0)
    } else {
        low_kwh
    };
    let (y_ticks, y_bottom, y_top) = axis(axis_low, high_kwh);
    let x = |hdd: f64| LEFT + hdd / x_top * (WIDTH - LEFT - RIGHT);
    let y = |kwh: f64| TOP + (y_top - kwh) / (y_top - y_bottom) * SCATTER_HEIGHT;
    let height = TOP + SCATTER_HEIGHT + BOTTOM;

    let mut svg = format!(
        "<svg class=\"energy-chart\" viewBox=\"0 0 {WIDTH:.0} {height:.0}\" role=\"img\" \
         aria-label=\"Monthly heating energy, water heating taken off, against heating degree \
         days, with the line through them\" preserveAspectRatio=\"xMidYMid meet\"><g class=\"grid\">"
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
        "<text class=\"axis-title\" x=\"{LEFT:.1}\" y=\"{:.1}\">heating kWh a month</text>\
         <text class=\"axis-title\" x=\"{:.1}\" y=\"{:.1}\" text-anchor=\"end\">heating degree \
         days a month</text></g>",
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
        let (cx, cy) = (x(month.whole_month_hdd()), y(month.heating_kwh()));
        let expected = overall.base + overall.slope * month.whole_month_hdd();
        let _ = write!(
            svg,
            "<g class=\"slot\"><circle class=\"hit\" cx=\"{cx:.1}\" cy=\"{cy:.1}\" \
             r=\"{HIT_RADIUS:.1}\"/><circle class=\"dot\" cx=\"{cx:.1}\" cy=\"{cy:.1}\" \
             r=\"{DOT_RADIUS:.1}\"/><text class=\"readout\" x=\"{LEFT:.1}\" \
             y=\"{READOUT_BASELINE:.1}\">{}</text></g>",
            escape(&format!(
                "{}: {} for heating at {} heating degree days - {} {} the line",
                month.month.label(),
                format_energy(month.heating_kwh()),
                format_degree_days(month.whole_month_hdd()),
                format_energy((month.heating_kwh() - expected).abs()),
                if month.heating_kwh() >= expected {
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
                "{}: {} kWh per degree day ± {}, over {} months{}",
                season.label(),
                format_slope(fit.slope),
                format_slope(fit.slope_error),
                fit.months,
                season.water.as_ref().map_or(String::new(), |water| format!(
                    ", after {} of water heating",
                    format_monthly(water.per_month)
                ))
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
         <th scope=\"col\">Heating energy</th><th scope=\"col\">Water heating</th>\
         <th scope=\"col\">kWh per degree day</th><th scope=\"col\">±</th>\
         <th scope=\"col\">Fit</th><th scope=\"col\">Against the season before</th></tr>\
         </thead><tbody>",
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
            "<tr><th scope=\"row\">{}</th><td>{}</td><td>{}</td><td>{}</td>{}<td>{}</td></tr>",
            escape(&season.label()),
            escape(&months),
            escape(&format_energy(season.kwh)),
            escape(&season.water.as_ref().map_or_else(
                || "-".to_string(),
                |water| {
                    if water.from_season == season.start_year {
                        format_monthly(water.per_month)
                    } else {
                        format!(
                            "{}, {}'s",
                            format_monthly(water.per_month),
                            season_label(water.from_season)
                        )
                    }
                }
            )),
            fit_cells(season.fit.as_ref()),
            escape(&change)
        );
        if let Some(fit) = season.fit {
            previous = Some(fit.slope);
        }
    }
    let _ = write!(
        html,
        "</tbody><tfoot><tr><th scope=\"row\">All</th><td>{}</td><td>{}</td><td>{}</td>{}\
         <td></td></tr></tfoot></table></div>",
        overall.months,
        escape(&format_energy(
            energy.fitted().map(MonthPoint::heating_kwh).sum::<f64>()
        )),
        escape(&energy.water_per_month.map_or_else(
            || "-".to_string(),
            |per_month| format!("{} on average", format_monthly(per_month))
        )),
        fit_cells(Some(overall))
    );
    html
}

fn fit_cells(fit: Option<&Fit>) -> String {
    match fit {
        Some(fit) => format!(
            "<td>{}</td><td>{}</td><td>{}</td>",
            escape(&format_slope(fit.slope)),
            escape(&format_slope(fit.slope_error)),
            escape(&percent(fit.r_squared))
        ),
        None => "<td>-</td><td>-</td><td>-</td>".to_string(),
    }
}

/// Every whole month the total covers, fitted or not, behind a fold.
fn month_table(energy: &Energy) -> String {
    let mut html = String::from(
        "<details class=\"floor\"><summary>Every month</summary><div class=\"table-scroll\">\
         <table><caption>What the total grew by over each month, the water heating taken \
         off it, the heating energy left, and the month's heating degree days, scaled up to the \
         whole month where a few days went uncounted.</caption><thead><tr><th scope=\"col\">\
         Month</th><th scope=\"col\">Energy</th><th scope=\"col\">Water heating</th>\
         <th scope=\"col\">Heating energy</th><th scope=\"col\">Heating degree days</th>\
         <th scope=\"col\">Days counted</th><th scope=\"col\">In the fit</th></tr></thead>\
         <tbody>",
    );
    // The months each season's water heating was told by.
    let warmest: Vec<Month> = energy
        .seasons
        .iter()
        .filter_map(|season| season.water.as_ref())
        .flat_map(|water| water.months.iter().copied())
        .collect();
    for month in &energy.months {
        let water = if month.water_kwh == 0.0 {
            "-".to_string()
        } else if warmest.contains(&month.month) {
            format!("{}, one of the warmest", format_energy(month.water_kwh))
        } else {
            format_energy(month.water_kwh)
        };
        let _ = write!(
            html,
            "<tr><th scope=\"row\">{}</th><td>{}</td><td>{}</td><td>{}</td><td>{}</td>\
             <td>{} of {}</td><td>{}</td></tr>",
            escape(&month.month.label()),
            escape(&format_energy(month.kwh)),
            escape(&water),
            escape(&format_energy(month.heating_kwh())),
            escape(&format_degree_days(month.whole_month_hdd())),
            month.counted_days,
            month.month.days(),
            match month.left_out() {
                None => "yes",
                Some(LeftOut::FewDays) => "no, too few days",
                Some(LeftOut::Unchanged) => "no, the total did not change",
                Some(LeftOut::AfterUnchanged) => "no, it may hold the month before",
                Some(LeftOut::WentDown) => "no, the total went down",
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
    use crate::energy::{Month, Season, Water};

    fn point(year: i32, month: u32, hdd: f64, kwh: f64) -> MonthPoint {
        let month = Month { year, month };
        MonthPoint {
            month,
            kwh,
            counted_days: month.days(),
            hdd,
            outside_mean: Some(15.0 - hdd / f64::from(month.days())),
            follows_unchanged: false,
            water_kwh: 0.0,
        }
    }

    /// The section's figures for `months`, with `water_per_day` kWh of water heating a day
    /// taken off every one: 2023/24's own, from its warmest months, which 2022/23 takes.
    fn energy(mut months: Vec<MonthPoint>, water_per_day: Option<f64>) -> Energy {
        let water = water_per_day.map(|per_day| Water {
            per_day,
            per_month: per_day * 30.0,
            months: vec![
                Month {
                    year: 2023,
                    month: 7,
                },
                Month {
                    year: 2023,
                    month: 8,
                },
                Month {
                    year: 2024,
                    month: 6,
                },
            ],
            from_season: 2023,
        });
        for month in &mut months {
            month.water_kwh =
                water_per_day.map_or(0.0, |per_day| per_day * f64::from(month.month.days()));
        }
        let fit = |months: &[MonthPoint]| {
            crate::energy::fit(
                &months
                    .iter()
                    .map(|month| (month.whole_month_hdd(), month.heating_kwh()))
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
                kwh: own.iter().map(MonthPoint::heating_kwh).sum(),
                water: water.clone(),
                fit: fit(&own),
            });
        }
        Energy {
            readings: months.len() + 1,
            overall: fit(&months),
            seasons,
            months,
            beyond_record: 0,
            water_per_month: water.map(|water| water.per_month),
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
        let html = section_of(energy(months, None));
        assert!(html.contains("<h2>Heating energy</h2>"));
        assert!(html.contains("number.heizol_energy"));
        assert!(html.contains(" kWh a month</strong> without heating"));
        assert_eq!(html.matches("class=\"dot\"").count(), 6, "one dot a month");
        assert_eq!(html.matches("class=\"fit\"").count(), 1, "one line");
        assert_eq!(html.matches("class=\"bar\"").count(), 2, "a bar a season");
        assert!(html.contains("<th scope=\"row\">2023/24</th>"));
        assert!(html.contains("<td>-20%</td>"), "2.4 against 3.0");
        assert!(html.contains("January 2024: 1,600 kWh for heating at 500 heating degree days"));
        assert!(html.contains("Water heating is left in: no season has three months"));
        assert!(html.contains("heating degree days a month</text>"));
    }

    #[test]
    fn months_left_out_are_named_with_why() {
        let mut months = vec![
            point(2023, 11, 300.0, 1_300.0),
            point(2023, 12, 450.0, 1_750.0),
            point(2024, 1, 480.0, 1_840.0),
            point(2024, 2, 400.0, 0.0),
            point(2024, 3, 250.0, -40.0),
        ];
        months[2].counted_days = 20;
        let html = section_of(energy(months, None));
        assert!(html.contains(
            "Left out, the outside thermometer counted less than nine in ten of its days: \
             January 2024."
        ));
        assert!(html.contains(
            "Left out, the total did not change over it - not read, or nothing used: February \
             2024."
        ));
        assert!(html.contains("a reading put right late: March 2024."));
        assert!(html.contains("<td>no, the total went down</td>"));
    }

    #[test]
    fn without_enough_months_it_says_what_there_is_and_why() {
        let html = section_of(energy(vec![point(2024, 1, 450.0, 1_800.0)], None));
        assert!(html.contains("no line to draw yet"), "{html}");
        assert!(html.contains(
            "the total covers 1 whole month where the outside was recorded (January 2024), 1 \
             of it fit to use - a line needs 3 months"
        ));
        assert!(html.contains("ten days by default"));
        assert!(!html.contains("<svg"));
    }

    #[test]
    fn water_heating_is_taken_off_and_said_where_it_came_from() {
        // Three kWh per degree day for heating, and ten a day heating water.
        let months: Vec<MonthPoint> = [
            (2023, 1, 480.0),
            (2023, 3, 310.0),
            (2023, 7, 0.0),
            (2023, 11, 300.0),
            (2024, 1, 500.0),
        ]
        .iter()
        .map(|(year, number, hdd)| {
            let days = f64::from(
                Month {
                    year: *year,
                    month: *number,
                }
                .days(),
            );
            point(*year, *number, *hdd, 3.0 * hdd + 10.0 * days)
        })
        .collect();
        let html = section_of(energy(months, Some(10.0)));
        assert!(
            html.contains(
                "<strong>3.00 kWh</strong> per heating degree day (± 0.00), once water heating \
                 of <strong>300 kWh a month</strong> is taken off - the line"
            ),
            "{html}"
        );
        assert!(html.contains("<td>300 kWh a month, 2023/24&#39;s</td>"));
        assert!(html.contains("<td>300 kWh a month on average</td>"));
        assert!(html.contains(
            "2022/23 takes 2023/24&#39;s, having fewer of its own on record; 2023/24&#39;s from \
             July 2023, August 2023 and June 2024."
        ));
        assert!(html.contains(
            "<th scope=\"row\">January 2024</th><td>1,810 kWh</td><td>310 kWh</td>\
             <td>1,500 kWh</td>"
        ));
        assert!(html.contains("<td>310 kWh, one of the warmest</td>"));
        assert!(html.contains("January 2024: 1,500 kWh for heating"));
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
