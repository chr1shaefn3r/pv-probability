//! The payback report: what every battery size would have been worth, as one
//! self-contained HTML file.

use std::fmt::Write as _;

use chrono_tz::Tz;

use crate::coverage::Coverage;
use crate::model::Grouping;
use crate::render::layout::{
    CHROME_CSS, Shell, coverage_section, document, metadata_list, theme_stylesheet,
};
use crate::render::svg::nice_ticks;
use crate::render::{color, escape, format_kwh, format_money, format_percent, format_years};
use crate::storage::pair::Paired;
use crate::storage::sweep::{Budget, SizeResult, Sweep};

/// Everything the page needs beyond the sweep itself.
#[derive(Debug, Clone, PartialEq)]
pub struct PaybackOptions {
    pub import_entity: String,
    pub export_entity: String,
    /// Facts about the run: source table, timezone, date range, assumptions.
    pub metadata: Vec<(String, String)>,
    pub coverage: Option<Coverage>,
    pub tz: Tz,
    /// How far either side of the price and cost the sensitivity grid looks, as a
    /// fraction: 0.25 for a quarter.
    pub sensitivity: f64,
    /// The payback period the report works backwards from, in years.
    pub target_payback_years: f64,
}

impl Default for PaybackOptions {
    fn default() -> Self {
        Self {
            import_entity: String::new(),
            export_entity: String::new(),
            metadata: Vec::new(),
            coverage: None,
            tz: Tz::UTC,
            sensitivity: 0.25,
            target_payback_years: 5.0,
        }
    }
}

/// Render the complete HTML document.
pub fn page(sweep: &Sweep, paired: &Paired, options: &PaybackOptions) -> String {
    let title = "Energy storage payback period".to_string();
    let mut body = String::with_capacity(32 * 1024);

    body.push_str(&metadata_list(&metadata_entries(sweep, options)));
    body.push_str(&sweep_section(sweep, options));
    body.push_str(&recommendation(sweep));
    body.push_str(&budget_section(sweep, options));
    body.push_str(&sensitivity_section(sweep, options));
    body.push_str(&measured_section(sweep, paired));
    if let Some(coverage) = &options.coverage {
        body.push_str(&coverage_section(
            coverage,
            options.tz,
            Grouping::Month,
            None,
        ));
    }

    document(
        &Shell {
            title: &title,
            lede: &lede(sweep),
            stylesheet: &stylesheet(),
            defs: "",
            footer: &footer(sweep),
        },
        &body,
    )
}

fn lede(sweep: &Sweep) -> String {
    format!(
        "Every battery size below was replayed over the recorded history: it stores the \
         surplus that was fed into the grid and gives it back when the house would \
         otherwise have bought power. What it saves at {} a kilowatt hour, against what it \
         costs to install, is its payback period.",
        format_money(sweep.economics.price_per_kwh, &sweep.economics.currency)
    )
}

fn metadata_entries(sweep: &Sweep, options: &PaybackOptions) -> Vec<(String, String)> {
    let currency = &sweep.economics.currency;
    let mut entries = vec![
        ("Import".to_string(), options.import_entity.clone()),
        ("Export".to_string(), options.export_entity.clone()),
    ];
    entries.extend(options.metadata.iter().cloned());
    entries.push((
        "Price".to_string(),
        format!(
            "{} per kWh",
            format_money(sweep.economics.price_per_kwh, currency)
        ),
    ));
    if sweep.economics.feed_in_price > 0.0 {
        entries.push((
            "Feed-in".to_string(),
            format!(
                "{} per kWh",
                format_money(sweep.economics.feed_in_price, currency)
            ),
        ));
    }
    entries.push((
        "Battery cost".to_string(),
        format!(
            "{} + {} per kWh",
            format_money(sweep.economics.base_cost, currency),
            format_money(sweep.economics.cost_per_kwh, currency)
        ),
    ));
    entries
}

/// The chart and the table: every size, what it saves and what it takes to pay for itself.
fn sweep_section(sweep: &Sweep, options: &PaybackOptions) -> String {
    let mut html = String::from("<section class=\"panel sweep\"><h2>Payback by battery size</h2>");
    if sweep.results.is_empty() {
        html.push_str("<p class=\"empty\">No battery sizes were simulated.</p></section>");
        return html;
    }
    html.push_str(&sweep_chart(sweep, options.target_payback_years));
    html.push_str(&sweep_table(sweep, options.target_payback_years));
    html.push_str("</section>");
    html
}

// Chart geometry, in user units. The viewBox scales to whatever width the panel has.
const BAR_WIDTH: f64 = 30.0;
const MARGIN_LEFT: f64 = 62.0;
const MARGIN_RIGHT: f64 = 14.0;
/// Room above the plot for the hover readout, which is why it must clear the panels.
const MARGIN_TOP: f64 = 26.0;
const READOUT_BASELINE: f64 = 14.0;
const _: () = assert!(READOUT_BASELINE < MARGIN_TOP);
const PANEL_HEIGHT: f64 = 130.0;
const PANEL_GAP: f64 = 40.0;
const MARGIN_BOTTOM: f64 = 42.0;

fn sweep_chart(sweep: &Sweep, target_years: f64) -> String {
    let count = sweep.results.len();
    let plot_width = BAR_WIDTH * count as f64;
    let width = MARGIN_LEFT + plot_width + MARGIN_RIGHT;
    let height = MARGIN_TOP + PANEL_HEIGHT * 2.0 + PANEL_GAP + MARGIN_BOTTOM;
    let savings_top = MARGIN_TOP;
    let payback_top = MARGIN_TOP + PANEL_HEIGHT + PANEL_GAP;

    let max_savings = sweep
        .results
        .iter()
        .map(|result| result.annual_savings)
        .fold(0.0f64, f64::max);
    // A single battery that never pays back must not stretch the axis to infinity; the
    // target line has to fit on it, though, or it could not be read against the bars.
    let max_payback = sweep
        .results
        .iter()
        .filter_map(|result| result.payback_years)
        .fold(target_years.max(0.0), f64::max);

    let mut svg = String::with_capacity(8 * 1024);
    let _ = write!(
        svg,
        "<svg class=\"sweep-chart\" viewBox=\"0 0 {width:.0} {height:.0}\" role=\"img\" \
         aria-label=\"Annual savings and payback period by battery size\" \
         preserveAspectRatio=\"xMidYMid meet\">"
    );

    for (top, max, label, money) in [
        (savings_top, max_savings, "saved per year", true),
        (payback_top, max_payback, "years to pay back", false),
    ] {
        let (ticks, span) = panel_scale(max);
        svg.push_str("<g class=\"grid\">");
        for tick in &ticks {
            let y = top + PANEL_HEIGHT - (tick / span) * PANEL_HEIGHT;
            let _ = write!(
                svg,
                "<line x1=\"{MARGIN_LEFT:.1}\" y1=\"{y:.1}\" x2=\"{:.1}\" y2=\"{y:.1}\"/>",
                MARGIN_LEFT + plot_width
            );
            let text = if money {
                format_money(*tick, &sweep.economics.currency)
            } else {
                format!("{tick:.0}")
            };
            let _ = write!(
                svg,
                "<text class=\"tick\" x=\"{:.1}\" y=\"{:.1}\" text-anchor=\"end\">{}</text>",
                MARGIN_LEFT - 5.0,
                y + 4.0,
                escape(&text)
            );
        }
        let _ = write!(
            svg,
            "<text class=\"axis-title\" x=\"{MARGIN_LEFT:.1}\" y=\"{:.1}\">{label}</text></g>",
            top - 6.0
        );
    }

    // Bars, in a group per size so that hovering either panel reads out the whole row.
    svg.push_str("<g class=\"series\">");
    let savings_span = panel_scale(max_savings).1;
    let payback_span = panel_scale(max_payback).1;

    for (index, result) in sweep.results.iter().enumerate() {
        let x = MARGIN_LEFT + index as f64 * BAR_WIDTH + 3.0;
        let bar_width = BAR_WIDTH - 6.0;
        let best = sweep.best == Some(index);

        let savings_height =
            (result.annual_savings / savings_span * PANEL_HEIGHT).clamp(0.0, PANEL_HEIGHT);
        let mut marks = format!(
            "<rect class=\"savings\" x=\"{x:.1}\" y=\"{:.1}\" width=\"{bar_width:.1}\" \
             height=\"{savings_height:.1}\"/>",
            savings_top + PANEL_HEIGHT - savings_height
        );
        match result.payback_years {
            Some(years) => {
                let bar = (years / payback_span * PANEL_HEIGHT).clamp(0.0, PANEL_HEIGHT);
                let _ = write!(
                    marks,
                    "<rect class=\"payback\" x=\"{x:.1}\" y=\"{:.1}\" width=\"{bar_width:.1}\" \
                     height=\"{bar:.1}\"/>",
                    payback_top + PANEL_HEIGHT - bar
                );
            }
            None => {
                // Nothing to draw, but the reader still needs to see that the size was tried.
                let _ = write!(
                    marks,
                    "<rect class=\"never\" x=\"{x:.1}\" y=\"{:.1}\" width=\"{bar_width:.1}\" \
                     height=\"3\"/>",
                    payback_top + PANEL_HEIGHT - 3.0
                );
            }
        }

        let _ = write!(
            svg,
            "<g class=\"bar{}\">{marks}<text class=\"readout\" x=\"{MARGIN_LEFT:.1}\" \
             y=\"{READOUT_BASELINE:.1}\">{}</text></g>",
            if best { " best" } else { "" },
            escape(&describe(result, &sweep.economics.currency))
        );
    }
    svg.push_str("</g>");

    // The target, drawn over the bars: everything below the line is a battery that pays
    // for itself in time.
    if target_years > 0.0 {
        let y = payback_top + PANEL_HEIGHT - (target_years / payback_span) * PANEL_HEIGHT;
        let _ = write!(
            svg,
            "<g class=\"target\"><line x1=\"{MARGIN_LEFT:.1}\" y1=\"{y:.1}\" x2=\"{:.1}\" \
             y2=\"{y:.1}\"/><text class=\"target-label\" x=\"{:.1}\" y=\"{:.1}\" \
             text-anchor=\"end\">{}</text></g>",
            MARGIN_LEFT + plot_width,
            MARGIN_LEFT + plot_width,
            y - 4.0,
            escape(&format!("target {}", format_years(Some(target_years))))
        );
    }

    // One x label per size while they fit, otherwise every second or third.
    let every = (count / 12).max(1);
    svg.push_str("<g class=\"axis\">");
    let _ = write!(
        svg,
        "<line x1=\"{MARGIN_LEFT:.1}\" y1=\"{:.1}\" x2=\"{:.1}\" y2=\"{:.1}\"/>",
        payback_top + PANEL_HEIGHT,
        MARGIN_LEFT + plot_width,
        payback_top + PANEL_HEIGHT
    );
    for (index, result) in sweep.results.iter().enumerate().step_by(every) {
        let _ = write!(
            svg,
            "<text class=\"tick\" x=\"{:.1}\" y=\"{:.1}\" text-anchor=\"middle\">{}</text>",
            MARGIN_LEFT + index as f64 * BAR_WIDTH + BAR_WIDTH / 2.0,
            payback_top + PANEL_HEIGHT + 16.0,
            escape(&trim_capacity(result.capacity_kwh))
        );
    }
    let _ = write!(
        svg,
        "<text class=\"axis-title\" x=\"{:.1}\" y=\"{:.1}\" text-anchor=\"middle\">battery size in kWh</text>",
        MARGIN_LEFT + plot_width / 2.0,
        height - 8.0
    );
    svg.push_str("</g></svg>");
    svg
}

/// Gridlines for one panel, and the value its full height stands for.
///
/// [`nice_ticks`] stops at or below the maximum, so the tallest bar would be clipped if
/// the last tick set the scale; the panel is as tall as the data, with round gridlines
/// wherever they fall inside it.
fn panel_scale(max: f64) -> (Vec<f64>, f64) {
    let ticks = nice_ticks(max, 4);
    let span = ticks
        .last()
        .copied()
        .unwrap_or(0.0)
        .max(max)
        .max(f64::MIN_POSITIVE);
    (ticks, span)
}

/// The line the hover readout shows, and the one the table row repeats.
fn describe(result: &SizeResult, currency: &str) -> String {
    format!(
        "{} kWh: {} a year, {} to install, pays back in {}",
        trim_capacity(result.capacity_kwh),
        format_money(result.annual_savings, currency),
        format_money(result.investment, currency),
        format_years(result.payback_years)
    )
}

/// `13.5` rather than `13.500000000001`, and `10` rather than `10.0`.
fn trim_capacity(capacity_kwh: f64) -> String {
    let text = format!("{capacity_kwh:.2}");
    text.trim_end_matches('0').trim_end_matches('.').to_string()
}

fn sweep_table(sweep: &Sweep, target_years: f64) -> String {
    let currency = &sweep.economics.currency;
    let mut html = format!(
        "<div class=\"table-scroll\"><table class=\"sweep-table\"><caption>Every size that \
         was simulated, with the fastest payback highlighted</caption><thead><tr>\
         <th scope=\"col\">Size</th><th scope=\"col\">Investment</th>\
         <th scope=\"col\">Saved per year</th><th scope=\"col\">Import avoided</th>\
         <th scope=\"col\">Cycles per year</th><th scope=\"col\">Payback</th>\
         <th scope=\"col\">The step up</th>\
         <th scope=\"col\">Budget for {}</th></tr></thead><tbody>",
        escape(&format_years(Some(target_years)))
    );
    for (index, result) in sweep.results.iter().enumerate() {
        let budget = sweep.budget(result, target_years);
        let _ = write!(
            html,
            "<tr{}><th scope=\"row\">{} kWh</th><td>{}</td><td>{}</td><td>{} ({})</td>\
             <td>{:.0}</td><td>{}</td><td>{}</td><td class=\"{}\">{}</td></tr>",
            if sweep.best == Some(index) {
                " class=\"best\""
            } else {
                ""
            },
            escape(&trim_capacity(result.capacity_kwh)),
            escape(&format_money(result.investment, currency)),
            escape(&format_money(result.annual_savings, currency)),
            escape(&format_kwh(
                result.simulation.avoided_import_kwh * sweep.annualisation
            )),
            escape(&format_percent(result.import_reduction)),
            result.cycles_per_year,
            escape(&format_years(result.payback_years)),
            escape(&format_years(result.marginal_payback_years)),
            if budget.met {
                "in-budget"
            } else {
                "over-budget"
            },
            escape(&describe_budget(&budget, currency))
        );
    }
    html.push_str("</tbody></table></div>");
    html
}

/// The budget cell: what the installation may cost, and how far that is from the quote.
fn describe_budget(budget: &Budget, currency: &str) -> String {
    if budget.investment <= 0.0 {
        return "no price is low enough".to_string();
    }
    match budget.discount {
        Some(discount) => format!(
            "{} ({} less)",
            format_money(budget.investment, currency),
            format_percent(discount)
        ),
        None => format!("{} (met)", format_money(budget.investment, currency)),
    }
}

/// How cheap the installation would have to be, which is the payback sum read backwards.
fn budget_section(sweep: &Sweep, options: &PaybackOptions) -> String {
    let currency = &sweep.economics.currency;
    let target = options.target_payback_years;
    let years = format_years(Some(target));

    let mut html = format!(
        "<section class=\"panel budget\"><h2>What it would have to cost to pay back in \
         {}</h2>",
        escape(&years)
    );

    let met: Vec<&SizeResult> = sweep
        .results
        .iter()
        .filter(|result| sweep.budget(result, target).met)
        .collect();
    let Some(closest) = sweep.closest_to_target() else {
        let _ = write!(
            html,
            "<p>Nothing here saves anything, so no price is low enough: a battery that \
             never avoids an import cannot pay for itself in {} or in a hundred.</p>\
             </section>",
            escape(&years)
        );
        return html;
    };
    let budget = sweep.budget(closest, target);

    if met.is_empty() {
        let _ = write!(
            html,
            "<p>No size reaches it at today's prices. The nearest is the \
             <strong>{} kWh</strong> battery, which pays back in {}: to be square within \
             {}, the whole installation would have to cost <strong>{}</strong> rather than \
             {}{}.</p>",
            escape(&trim_capacity(closest.capacity_kwh)),
            escape(&format_years(closest.payback_years)),
            escape(&years),
            escape(&format_money(budget.investment, currency)),
            escape(&format_money(budget.quoted, currency)),
            match budget.discount {
                Some(discount) => format!(" - {} less", format_percent(discount)),
                None => String::new(),
            }
        );
    } else {
        let cheapest = met
            .iter()
            .min_by(|left, right| left.investment.total_cmp(&right.investment))
            .expect("a non-empty list has a minimum");
        let _ = write!(
            html,
            "<p><strong>{} of the {} sizes</strong> already pay back within {} at today's \
             prices, from the {} kWh battery at {} upwards. The fastest is the \
             <strong>{} kWh</strong> one, at {}.</p>",
            met.len(),
            sweep.results.len(),
            escape(&years),
            escape(&trim_capacity(cheapest.capacity_kwh)),
            escape(&format_money(cheapest.investment, currency)),
            escape(&trim_capacity(closest.capacity_kwh)),
            escape(&format_years(closest.payback_years))
        );
        let _ = write!(
            html,
            "<p>The {} kWh battery has room to spare: up to <strong>{}</strong> would still \
             be square within {}.</p>",
            escape(&trim_capacity(closest.capacity_kwh)),
            escape(&format_money(budget.investment, currency)),
            escape(&years)
        );
    }

    match budget.cost_per_kwh {
        Some(per_kwh) => {
            let _ = write!(
                html,
                "<p>With the {} that does not scale with capacity paid first, that leaves \
                 <strong>{} per kWh</strong> for the {} kWh itself, against the {} per kWh \
                 this report was run with.</p>",
                escape(&format_money(sweep.economics.base_cost, currency)),
                escape(&format_money(per_kwh, currency)),
                escape(&trim_capacity(closest.capacity_kwh)),
                escape(&format_money(sweep.economics.cost_per_kwh, currency))
            );
        }
        None => {
            let _ = write!(
                html,
                "<p>The {} that does not scale with capacity is on its own more than that \
                 budget, so no cell price - not even zero - would get this size there. Only \
                 a cheaper installation, a dearer kilowatt hour, or a longer target can.</p>",
                escape(&format_money(sweep.economics.base_cost, currency))
            );
        }
    }

    let _ = write!(
        html,
        "<p class=\"note\">Read backwards from the same sum as everything else: payback is \
         the installed price divided by what it saves in a year, so the budget for {} is \
         simply {} times the annual saving. The last column of the table above does this \
         for every size.</p></section>",
        escape(&years),
        escape(&years)
    );
    html
}

/// What the sweep actually recommends, in words.
fn recommendation(sweep: &Sweep) -> String {
    let currency = &sweep.economics.currency;
    let mut html = String::from("<section class=\"panel recommendation\"><h2>What it says</h2>");
    let Some(best) = sweep.best_result() else {
        html.push_str(
            "<p>No battery in this sweep pays for itself. That happens when little of the \
             energy bought from the grid could have come from the surplus fed into it - \
             either because the two rarely swap places over a day, or because the export is \
             too small to fill a battery worth installing.</p>",
        );
        html.push_str(
            "<p>The sensitivity block below shows what a different price would do, and \
             --sizes can try smaller batteries than this sweep did.</p></section>",
        );
        return html;
    };

    let _ = write!(
        html,
        "<p>A <strong>{} kWh</strong> battery pays for itself fastest: {} to install, \
         {} a year of grid imports avoided, paid off in <strong>{}</strong>.</p>",
        escape(&trim_capacity(best.capacity_kwh)),
        escape(&format_money(best.investment, currency)),
        escape(&format_money(best.annual_savings, currency)),
        escape(&format_years(best.payback_years))
    );
    let _ = write!(
        html,
        "<p>It covers {} of everything the house bought from the grid, cycling {:.0} times \
         a year.</p>",
        escape(&format_percent(best.import_reduction)),
        best.cycles_per_year
    );

    match sweep.next_after_best() {
        Some(next) => {
            let _ = write!(
                html,
                "<p>Going one step further to {} kWh adds {} a year for another {}, which on \
                 its own takes {} to pay back - that is where extra capacity stops earning \
                 its keep.</p>",
                escape(&trim_capacity(next.capacity_kwh)),
                escape(&format_money(
                    next.annual_savings - best.annual_savings,
                    currency
                )),
                escape(&format_money(next.investment - best.investment, currency)),
                escape(&format_years(next.marginal_payback_years))
            );
        }
        None => {
            html.push_str(
                "<p>It is also the largest size that was tried, so a bigger battery might \
                 still be worth looking at: raise --max-size to find out.</p>",
            );
        }
    }
    html.push_str("</section>");
    html
}

/// The same battery at other prices and other quotes.
fn sensitivity_section(sweep: &Sweep, options: &PaybackOptions) -> String {
    let Some(best) = sweep.best_result() else {
        return String::new();
    };
    let currency = &sweep.economics.currency;
    let spread = options.sensitivity;
    let price = sweep.economics.price_per_kwh;
    let prices = [price * (1.0 - spread), price, price * (1.0 + spread)];
    let costs = [1.0 - spread, 1.0, 1.0 + spread];

    let mut html =
        String::from("<section class=\"panel sensitivity\"><h2>If the numbers move</h2>");
    let _ = write!(
        html,
        "<p>Payback of the {} kWh battery at other electricity prices and other quotes. \
         Nothing is re-simulated: the energy it shifts does not depend on what anything \
         costs.</p>",
        escape(&trim_capacity(best.capacity_kwh))
    );
    html.push_str(
        "<div class=\"table-scroll\"><table class=\"sensitivity-table\">\
         <thead><tr><th scope=\"col\">Price per kWh</th>",
    );
    for factor in costs {
        let _ = write!(
            html,
            "<th scope=\"col\">{}</th>",
            escape(&format_money(best.investment * factor, currency))
        );
    }
    html.push_str("</tr></thead><tbody>");
    for price in prices {
        let _ = write!(
            html,
            "<tr><th scope=\"row\">{}</th>",
            escape(&format_money(price, currency))
        );
        for factor in costs {
            let years = sweep.payback_at(best, price, factor);
            let _ = write!(html, "<td>{}</td>", escape(&format_years(years)));
        }
        html.push_str("</tr>");
    }
    html.push_str("</tbody></table></div></section>");
    html
}

/// What the history actually said, and what it could not say.
fn measured_section(sweep: &Sweep, paired: &Paired) -> String {
    let mut html = String::from("<section class=\"panel measured\"><h2>What was measured</h2>");
    let slot = paired.slot_seconds / 60;
    let _ = write!(
        html,
        "<p>{} bought from the grid and {} fed back over {} slots of {} minutes, {} of \
         history in all.</p>",
        escape(&format_kwh(paired.import_kwh)),
        escape(&format_kwh(paired.export_kwh)),
        paired.steps.len(),
        slot,
        escape(&format_span(paired.observed_seconds))
    );
    if sweep.annualisation > 1.01 {
        let _ = write!(
            html,
            "<p class=\"caution\">That is less than a year, so every annual figure above is \
             the observed period multiplied by {:.2}. A history that is mostly summer will \
             flatter a battery, and one that is mostly winter will do the opposite.</p>",
            sweep.annualisation
        );
    }
    if paired.dropped() > 0 {
        let _ = write!(
            html,
            "<p>{} slots were left out: {} where only one of the two sensors was recording, \
             and {} where one of them covered too little of the slot to trust. A battery \
             cannot be sized against a surplus nobody recorded.</p>",
            paired.dropped(),
            paired.dropped_unpaired,
            paired.dropped_partial
        );
    }
    let _ = write!(
        html,
        "<p>Import and export inside one {slot} minute slot are not netted off against each \
         other: had they been simultaneous the house would never have imported at all, so \
         they happened at different minutes and bridging them is exactly what a battery \
         does. A finer slot would still be more faithful - try --slot-minutes 5 with \
         --source short-term to see how much it moves.</p>"
    );
    html.push_str(
        "<p>These sensors show what crossed the meter, not what the house used or the roof \
         made, so everything here is stated as grid import avoided rather than as \
         self-sufficiency, which this data cannot support.</p></section>",
    );
    html
}

/// "1.8 years" / "7 months" / "12 days", for a span of observation time.
fn format_span(seconds: f64) -> String {
    let days = seconds / 86_400.0;
    if days < 90.0 {
        return format!("{days:.0} days");
    }
    if days < 550.0 {
        return format!("{:.0} months", days / 30.44);
    }
    format!("{:.1} years", days / 365.2425)
}

fn footer(sweep: &Sweep) -> String {
    format!(
        "Generated by energy-storage-payback-period {} - {} sizes simulated over {} slots.",
        env!("CARGO_PKG_VERSION"),
        sweep.results.len(),
        (sweep.observed_seconds / 3_600.0).round() as i64
    )
}

fn stylesheet() -> String {
    let mut css = theme_stylesheet(color::DEFAULT_LEVELS);
    css.push_str(CHROME_CSS);
    css.push_str(PAYBACK_CSS);
    css
}

/// The rules only this report needs; the chrome it shares lives in
/// [`crate::render::layout::CHROME_CSS`].
const PAYBACK_CSS: &str = "
.panel {
  margin: 1.25rem 0 0;
  padding: 0.85rem 1rem;
  background: var(--surface);
  border: 1px solid var(--border);
  border-radius: 10px;
}
.panel p { margin: 0 0 0.5rem; color: var(--ink-secondary); max-width: 78ch; }
.panel p:last-child { margin-bottom: 0; }
.panel p strong { color: var(--ink); font-variant-numeric: tabular-nums; }
.panel .caution { color: var(--ink); }
.sweep-chart { width: 100%; height: auto; display: block; margin: 0.25rem 0 0.75rem; }
.sweep-chart .grid line { stroke: var(--grid); stroke-width: 1; }
.sweep-chart .axis line { stroke: var(--axis); stroke-width: 1; }
.sweep-chart .tick { fill: var(--ink-muted); font-size: 11px; font-variant-numeric: tabular-nums; }
.sweep-chart .axis-title { fill: var(--ink-muted); font-size: 10px; }
.sweep-chart .savings { fill: var(--heat-4); }
.sweep-chart .payback { fill: var(--heat-7); }
.sweep-chart .never { fill: var(--axis); }
.sweep-chart .target line { stroke: var(--ink); stroke-width: 1; stroke-dasharray: 4 3; }
.sweep-chart .target-label {
  fill: var(--ink);
  font-size: 10px;
  font-variant-numeric: tabular-nums;
  /* The line crosses the bars, so the label needs a halo to stay readable over them. */
  stroke: var(--surface);
  stroke-width: 3px;
  paint-order: stroke;
}
.sweep-chart .bar.best rect { stroke: var(--ink); stroke-width: 1; paint-order: stroke; }
.sweep-chart .readout {
  display: none;
  fill: var(--ink);
  font-size: 11px;
  font-variant-numeric: tabular-nums;
  pointer-events: none;
}
.sweep-chart .bar:hover .readout { display: block; }
.sweep-chart .bar:hover rect { stroke: var(--ink); stroke-width: 1; paint-order: stroke; }
.table-scroll { overflow-x: auto; margin-top: 0.5rem; }
.panel table { border-collapse: collapse; width: 100%; font-size: 0.8rem; font-variant-numeric: tabular-nums; }
.panel caption { text-align: left; color: var(--ink-muted); padding-bottom: 0.4rem; font-size: 0.78rem; }
.panel th, .panel td { padding: 0.2rem 0.5rem; text-align: right; white-space: nowrap; }
.panel thead th { color: var(--ink-muted); font-weight: 500; border-bottom: 1px solid var(--border); }
.panel tbody th { text-align: left; font-weight: 500; color: var(--ink-secondary); }
.panel tbody tr:nth-child(even) { background: var(--plane); }
.panel tbody tr.best { background: var(--heat-0); color: var(--ink); font-weight: 600; }
.panel tbody tr.best th { color: var(--ink); font-weight: 600; }
.panel td.in-budget { color: var(--ink); font-weight: 600; }
.panel td.over-budget { color: var(--ink-muted); }
.empty { margin-top: 0.5rem; color: var(--ink-secondary); }
";

#[cfg(test)]
mod tests {
    use super::*;
    use chrono_tz::Tz::UTC;

    use crate::storage::pair::PairedStep;
    use crate::storage::simulate::BatterySpec;
    use crate::storage::sweep::{Economics, size_range, sweep as run_sweep};

    fn economics() -> Economics {
        Economics {
            price_per_kwh: 0.35,
            feed_in_price: 0.0,
            cost_per_kwh: 500.0,
            base_cost: 1_500.0,
            currency: "EUR".to_string(),
        }
    }

    /// A year of a day that exports 8 kWh around noon and buys 6 kWh in the evening.
    fn paired(days: i64) -> Paired {
        let mut steps = Vec::new();
        for day in 0..days {
            for hour in 0..24 {
                let (import_kwh, export_kwh) = match hour {
                    11 | 12 => (0.0, 4.0),
                    18 | 19 => (3.0, 0.0),
                    _ => (0.1, 0.0),
                };
                steps.push(PairedStep {
                    start_ts: day * 86_400 + hour * 3_600,
                    seconds: 3_600.0,
                    import_kwh,
                    export_kwh,
                });
            }
        }
        Paired {
            slot_seconds: 3_600,
            dropped_partial: 2,
            dropped_unpaired: 5,
            import_kwh: steps.iter().map(|step| step.import_kwh).sum(),
            export_kwh: steps.iter().map(|step| step.export_kwh).sum(),
            observed_seconds: steps.len() as f64 * 3_600.0,
            steps,
        }
    }

    fn sweep_of(paired: &Paired) -> Sweep {
        run_sweep(
            paired,
            &size_range(1.0, 12.0, 1.0),
            &BatterySpec::new(0.0, 0.9, 0.9),
            &economics(),
        )
    }

    fn options() -> PaybackOptions {
        PaybackOptions {
            import_entity: "sensor.grid_import_power".to_string(),
            export_entity: "sensor.grid_export_power".to_string(),
            metadata: vec![("Source".to_string(), "statistics".to_string())],
            ..PaybackOptions::default()
        }
    }

    fn report(days: i64) -> String {
        let paired = paired(days);
        page(&sweep_of(&paired), &paired, &options())
    }

    #[test]
    fn renders_a_complete_document() {
        let html = report(365);
        assert!(html.starts_with("<!doctype html>"));
        assert!(html.trim_end().ends_with("</html>"));
        assert!(html.contains("<title>Energy storage payback period</title>"));
        assert!(html.contains("sensor.grid_import_power"));
        assert!(html.contains("sensor.grid_export_power"));
        assert!(html.contains("statistics"));
    }

    #[test]
    fn is_self_contained() {
        let html = report(365);
        assert!(!html.contains("http://"), "external reference in output");
        assert!(!html.contains("https://"), "external reference in output");
        assert!(!html.contains("<script"), "the page needs no javascript");
        assert!(!html.contains("<link"), "no external stylesheet");
        assert!(!html.contains("NaN"));
    }

    #[test]
    fn the_blocks_follow_the_order_the_reader_needs_them_in() {
        let paired = paired(365);
        let mut options = options();
        options.coverage = Coverage::describe(
            &paired.samples(),
            365,
            365,
            &[0],
            Grouping::Month,
            UTC,
            24 * 3_600,
        );
        let html = page(&sweep_of(&paired), &paired, &options);
        let at = |needle: &str| {
            html.find(needle)
                .unwrap_or_else(|| panic!("missing {needle}"))
        };

        assert!(at("class=\"meta\"") < at("class=\"panel sweep\""));
        assert!(at("class=\"panel sweep\"") < at("class=\"panel recommendation\""));
        assert!(at("class=\"panel recommendation\"") < at("class=\"panel budget\""));
        assert!(at("class=\"panel budget\"") < at("class=\"panel sensitivity\""));
        assert!(at("class=\"panel sensitivity\"") < at("class=\"panel measured\""));
        assert!(at("class=\"panel measured\"") < at("class=\"coverage-summary\""));
        assert!(at("class=\"coverage-summary\"") < at("class=\"page-footer\""));
    }

    #[test]
    fn every_size_gets_a_bar_and_a_row() {
        let paired = paired(365);
        let sweep = sweep_of(&paired);
        let html = page(&sweep, &paired, &options());
        assert_eq!(html.matches("class=\"bar").count(), sweep.results.len());
        assert_eq!(
            html.matches("<tr").count(),
            // One row per size, plus the header rows of both tables and three
            // sensitivity rows.
            sweep.results.len() + 2 + 3
        );
        assert!(html.contains("12 kWh"), "the largest size is in the table");
    }

    #[test]
    fn the_fastest_payback_is_marked_in_the_chart_and_the_table() {
        let paired = paired(365);
        let sweep = sweep_of(&paired);
        let best = sweep.best_result().expect("something pays back");
        let html = page(&sweep, &paired, &options());

        assert_eq!(html.matches("class=\"bar best\"").count(), 1);
        assert_eq!(html.matches("<tr class=\"best\">").count(), 1);
        assert!(html.contains(&format!(
            "<strong>{} kWh</strong> battery pays for itself fastest",
            trim_capacity(best.capacity_kwh)
        )));
    }

    #[test]
    fn a_hover_readout_states_the_whole_row_without_javascript() {
        let html = report(365);
        assert!(html.contains(".sweep-chart .readout {"));
        assert!(html.contains(".sweep-chart .bar:hover .readout { display: block; }"));
        assert!(html.contains("pays back in"));
        assert_eq!(
            html.matches("<title>").count(),
            1,
            "the only <title> is the document's: SVG ones are invisible in Safari"
        );
    }

    #[test]
    fn a_battery_that_never_pays_back_is_said_so_in_words() {
        // One day, with the import before the sun: the battery is empty when it is needed
        // and full when the history ends.
        let steps: Vec<PairedStep> = (0..24)
            .map(|hour| PairedStep {
                start_ts: hour * 3_600,
                seconds: 3_600.0,
                import_kwh: if hour == 3 { 5.0 } else { 0.0 },
                export_kwh: if hour == 22 { 5.0 } else { 0.0 },
            })
            .collect();
        let paired = Paired {
            slot_seconds: 3_600,
            dropped_partial: 0,
            dropped_unpaired: 0,
            import_kwh: 5.0,
            export_kwh: 5.0,
            observed_seconds: 24.0 * 3_600.0,
            steps,
        };
        let html = page(&sweep_of(&paired), &paired, &options());

        assert!(html.contains("No battery in this sweep pays for itself"));
        assert!(html.contains("never"), "the table says so per size too");
        // With no best size there is nothing to run a sensitivity on.
        assert!(!html.contains("class=\"panel sensitivity\""));
        assert!(
            html.contains("class=\"never\""),
            "the bars still show the size was tried"
        );
    }

    #[test]
    fn a_short_history_says_that_its_annual_figures_are_extrapolated() {
        let html = report(120);
        assert!(html.contains("less than a year"));
        assert!(html.contains("multiplied by"));

        // A full year needs no such warning.
        assert!(!report(365).contains("multiplied by"));
    }

    #[test]
    fn the_measured_block_owns_up_to_what_was_left_out() {
        let html = report(365);
        assert!(html.contains("7 slots were left out"));
        assert!(html.contains("5 where only one of the two sensors was recording"));
        assert!(html.contains("2 where one of them covered too little"));
        assert!(
            html.contains("grid import avoided rather than as self-sufficiency"),
            "the report must not claim knowledge of the household load"
        );
    }

    #[test]
    fn a_target_out_of_reach_says_what_the_installation_would_have_to_cost() {
        let paired = paired(365);
        let sweep = sweep_of(&paired);
        let best = sweep.best_result().expect("something pays back");
        let mut options = options();
        options.target_payback_years = 2.0;
        let budget = sweep.budget(best, 2.0);
        assert!(
            !budget.met,
            "the fixture must not reach two years on its own"
        );

        let html = page(&sweep, &paired, &options);
        assert!(html.contains("What it would have to cost to pay back in 2 years"));
        assert!(html.contains("No size reaches it at today's prices"));
        assert!(
            html.contains(&format!(
                "cost <strong>{}</strong> rather than {}",
                format_money(budget.investment, "EUR"),
                format_money(budget.quoted, "EUR")
            )),
            "the budget and the quote have to be stated side by side"
        );
        assert!(html.contains(&format_percent(budget.discount.unwrap())));
    }

    #[test]
    fn a_target_already_met_says_how_much_room_is_left() {
        let paired = paired(365);
        let sweep = sweep_of(&paired);
        let mut options = options();
        options.target_payback_years = sweep.best_result().unwrap().payback_years.unwrap() + 5.0;

        let html = page(&sweep, &paired, &options);
        assert!(html.contains("already pay back within"));
        assert!(html.contains("has room to spare"));
        assert!(!html.contains("No size reaches it at today's prices"));
    }

    #[test]
    fn a_budget_the_fixed_cost_eats_is_called_out_rather_than_quoted_as_negative() {
        let paired = paired(365);
        let sweep = sweep_of(&paired);
        let mut options = options();
        options.target_payback_years = 1.0;
        let html = page(&sweep, &paired, &options);

        assert!(html.contains("is on its own more than that budget"));
        assert!(html.contains("not even zero"));

        // And no negative amount is quoted in its place.
        let start = html
            .find("class=\"panel budget\"")
            .expect("the budget block");
        let block = &html[start..start + html[start..].find("</section>").unwrap()];
        assert!(
            !["-0", "-1", "-2", "-3", "-4", "-5", "-6", "-7", "-8", "-9"]
                .iter()
                .any(|negative| block.contains(negative)),
            "a negative amount slipped into the budget block: {block}"
        );
    }

    #[test]
    fn every_row_carries_the_budget_for_the_target() {
        let paired = paired(365);
        let sweep = sweep_of(&paired);
        let mut options = options();
        options.target_payback_years = 8.0;
        let html = page(&sweep, &paired, &options);

        assert!(html.contains("<th scope=\"col\">Budget for 8 years</th>"));
        assert_eq!(
            html.matches("class=\"over-budget\"").count()
                + html.matches("class=\"in-budget\"").count(),
            sweep.results.len(),
            "one budget cell per size"
        );
        // The cell states the sum the payback maths would need.
        let best = sweep.best_result().unwrap();
        assert!(html.contains(&format_money(sweep.budget(best, 8.0).investment, "EUR")));
    }

    #[test]
    fn the_target_is_drawn_across_the_payback_panel() {
        let paired = paired(365);
        let mut options = options();
        options.target_payback_years = 6.0;
        let html = page(&sweep_of(&paired), &paired, &options);

        assert!(html.contains("class=\"target\""));
        assert!(html.contains(">target 6 years</text>"));
        assert!(html.contains(".sweep-chart .target line {"));
    }

    #[test]
    fn a_target_beyond_every_bar_still_fits_on_the_panel() {
        // The line has to be visible even when it sits above the tallest bar, or it could
        // not be read against them at all.
        let paired = paired(365);
        let sweep = sweep_of(&paired);
        let longest = sweep
            .results
            .iter()
            .filter_map(|result| result.payback_years)
            .fold(0.0f64, f64::max);
        let mut options = options();
        options.target_payback_years = longest + 20.0;

        let chart = sweep_chart(&sweep, options.target_payback_years);
        let (_, span) = panel_scale(longest.max(options.target_payback_years));
        assert!(span >= options.target_payback_years);
        assert!(chart.contains("class=\"target\""));
    }

    #[test]
    fn the_sensitivity_grid_brackets_the_price_and_the_quote() {
        let html = report(365);
        assert!(html.contains("If the numbers move"));
        // 0.35 EUR either side by a quarter.
        assert!(html.contains("0.26 EUR"));
        assert!(html.contains("0.44 EUR"));
        assert!(html.contains("0.35 EUR"));
    }

    #[test]
    fn user_supplied_text_is_escaped() {
        let paired = paired(30);
        let mut options = options();
        options.import_entity = "sensor.<script>alert(1)</script>".to_string();
        options.metadata = vec![("Note".to_string(), "a & b".to_string())];
        let html = page(&sweep_of(&paired), &paired, &options);
        assert!(!html.contains("<script>alert"));
        assert!(html.contains("&lt;script&gt;"));
        assert!(html.contains("a &amp; b"));
    }

    #[test]
    fn capacities_read_as_people_write_them() {
        assert_eq!(trim_capacity(10.0), "10");
        assert_eq!(trim_capacity(13.5), "13.5");
        assert_eq!(trim_capacity(0.5), "0.5");
    }

    #[test]
    fn a_panel_is_as_tall_as_its_tallest_bar() {
        // nice_ticks stops at or below the maximum, so the scale has to stretch past it.
        let (ticks, span) = panel_scale(583.0);
        assert_eq!(ticks.last().copied(), Some(400.0), "round gridlines");
        assert_eq!(span, 583.0, "but the bar still fits");
        assert!(
            panel_scale(0.0).1 > 0.0,
            "an empty panel does not divide by zero"
        );
    }

    #[test]
    fn spans_are_stated_in_units_a_reader_can_feel() {
        assert_eq!(format_span(10.0 * 86_400.0), "10 days");
        assert_eq!(format_span(200.0 * 86_400.0), "7 months");
        assert_eq!(format_span(730.0 * 86_400.0), "2.0 years");
    }
}
