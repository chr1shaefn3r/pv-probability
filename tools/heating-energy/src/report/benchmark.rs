//! The Annual benchmark section: a year's heating energy over the floor area, on the scale
//! an Energieausweis puts it on - the efficiency classes A+ to H on a band from green to
//! red, the house's figure marked on it.

use std::fmt::Write as _;

use ha_report::escape;

use super::energy::EnergySection;
use crate::benchmark::{Benchmark, CLASSES, Year, class, format_per_square_metre};
use crate::energy::format_energy;

// Geometry, in user units, as wide as the charts above.
const WIDTH: f64 = 880.0;
const LEFT: f64 = 24.0;
const RIGHT: f64 = 24.0;
/// Where the scale ends: H runs on from 250, drawn as far again as half of G.
const SCALE_END: f64 = 275.0;
const READOUT_BASELINE: f64 = 14.0;
const TOP_LABEL: f64 = 38.0;
const BAND_TOP: f64 = 54.0;
const BAND_HEIGHT: f64 = 32.0;
const TICKS: f64 = 104.0;
const BAR_TOP: f64 = 112.0;
const BAR_HEIGHT: f64 = 16.0;
const BOTTOM_LABEL: f64 = 164.0;
const HEIGHT: f64 = 174.0;
/// Room a marker's label takes either side of it before it is turned inwards.
const LABEL_ROOM: f64 = 120.0;

/// The colours along the scale, as the certificate draws them, by kWh/(m²·a): green
/// through yellow to red. The band is the certificate's in light and dark alike; the text
/// beside it wears the page's ink.
const STOPS: [(f64, &str); 7] = [
    (0.0, "#2f9e44"),
    (50.0, "#8cc63f"),
    (100.0, "#ffe600"),
    (160.0, "#ffb400"),
    (200.0, "#f7931e"),
    (250.0, "#e8531f"),
    (275.0, "#d7261e"),
];

fn x(per_square_metre: f64) -> f64 {
    LEFT + per_square_metre.clamp(0.0, SCALE_END) / SCALE_END * (WIDTH - LEFT - RIGHT)
}

/// The section, when the rooms file gives a floor area: empty otherwise.
pub fn section(section: &EnergySection) -> String {
    let Some(benchmark) = &section.benchmark else {
        return String::new();
    };
    let mut html =
        String::from("<section class=\"panel energy benchmark\"><h2>Annual benchmark</h2>");
    let _ = write!(
        html,
        "<p>Twelve whole months of heating energy from {}, water heating taken off, over the \
         floor area it heats - {} m²{} - in kWh per m² and year, on the scale an \
         Energieausweis uses.</p>",
        escape(&section.entity),
        escape(&format_area(benchmark.heated_area)),
        benchmark
            .total_area
            .map_or_else(String::new, |total| format!(
                ", and over all the floor, heated or not, {} m²",
                escape(&format_area(total))
            ))
    );
    let Some(latest) = benchmark.latest else {
        let months = section.energy.months.len();
        let _ = write!(
            html,
            "<p class=\"empty\">There is no whole year yet: the benchmark needs twelve months \
             in a row whose energy is sound, and the total covers {months} whole month{} where \
             the outside was recorded.</p></section>",
            if months == 1 { "" } else { "s" }
        );
        return html;
    };

    let heated = latest.per_square_metre(benchmark.heated_area);
    let total = benchmark
        .total_area
        .map(|area| latest.per_square_metre(area));
    let _ = write!(
        html,
        "<p class=\"headline\">{}: <strong>{}</strong> over the floor it heats - class \
         <strong>{}</strong>{}.</p>",
        escape(&match latest.season {
            Some(_) => format!("For {}, the latest twelve whole months", latest.label()),
            None => format!("For the latest twelve whole months, {}", latest.label()),
        }),
        escape(&format_per_square_metre(heated)),
        class(heated),
        total.map_or_else(String::new, |total| format!(
            ", and <strong>{}</strong> over all the floor - class <strong>{}</strong>",
            escape(&format_per_square_metre(total)),
            class(total)
        ))
    );
    html.push_str(&scale(heated, total));
    html.push_str(&table(benchmark));

    let mut note = String::from(
        "The classes are an Energieausweis's for homes, from A+ under 30 kWh/(m²·a) to H at \
         250 and more. ",
    );
    let with_water = latest.kwh / benchmark.heated_area;
    let _ = write!(
        note,
        "An Energieausweis counts water heating in, and evens the year out for its weather; \
         with water heating, these twelve months come to {} over the floor heated - class {}. \
         Here the year's own weather stands, so a cold one reads higher. ",
        format_per_square_metre(with_water),
        class(with_water)
    );
    note.push_str(if benchmark.heated_from_rooms {
        "The floor heated is the rooms' areas added up; give the house its heated_area in the \
         rooms file if some heated rooms are not listed."
    } else {
        "The floor heated is the house's heated_area from the rooms file."
    });
    if benchmark.total_area.is_none() {
        note.push_str(
            " Give the house its total_area in the rooms file to set the heating against all \
             the floor too.",
        );
    }
    let _ = write!(html, "<p class=\"note\">{}</p></section>", escape(&note));
    html
}

/// The certificate's scale: the classes on a band from green to red, the ticks beneath
/// them and a bar beneath those, the floor heated marked above and all the floor below.
fn scale(heated: f64, total: Option<f64>) -> String {
    let ours: Vec<&str> = std::iter::once(class(heated))
        .chain(total.map(class))
        .collect();
    let mut svg = format!(
        "<svg class=\"energy-chart ea-chart\" viewBox=\"0 0 {WIDTH:.0} {HEIGHT:.0}\" role=\"img\" \
         aria-label=\"{}\" preserveAspectRatio=\"xMidYMid meet\"><defs><linearGradient \
         id=\"ea-scale\" gradientUnits=\"userSpaceOnUse\" x1=\"{LEFT:.1}\" y1=\"0\" \
         x2=\"{:.1}\" y2=\"0\">",
        escape(&format!(
            "Energy efficiency scale from A+ to H: {} over the floor heated, class {}{}",
            format_per_square_metre(heated),
            class(heated),
            total.map_or_else(String::new, |total| format!(
                "; {} over all the floor, class {}",
                format_per_square_metre(total),
                class(total)
            ))
        )),
        WIDTH - RIGHT
    );
    for (at, colour) in STOPS {
        let _ = write!(
            svg,
            "<stop offset=\"{:.4}\" stop-color=\"{colour}\"/>",
            at / SCALE_END
        );
    }
    let radius = BAND_HEIGHT / 2.0;
    let _ = write!(
        svg,
        "</linearGradient></defs><rect class=\"ea-band\" x=\"{LEFT:.1}\" y=\"{BAND_TOP:.1}\" \
         width=\"{:.1}\" height=\"{BAND_HEIGHT:.1}\" rx=\"{radius:.1}\" \
         fill=\"url(#ea-scale)\"/><rect class=\"ea-band\" x=\"{LEFT:.1}\" y=\"{BAR_TOP:.1}\" \
         width=\"{:.1}\" height=\"{BAR_HEIGHT:.1}\" rx=\"{:.1}\" fill=\"url(#ea-scale)\"/>",
        WIDTH - LEFT - RIGHT,
        WIDTH - LEFT - RIGHT,
        BAR_HEIGHT / 2.0
    );

    // The classes, each with its letter between its bounds, the house's larger.
    let mut from = 0.0;
    for (index, (letter, below)) in CLASSES.iter().enumerate() {
        let to = below.min(SCALE_END);
        if index > 0 {
            let _ = write!(
                svg,
                "<line class=\"ea-sep\" x1=\"{0:.1}\" y1=\"{BAND_TOP:.1}\" x2=\"{0:.1}\" \
                 y2=\"{1:.1}\"/>",
                x(from),
                BAND_TOP + BAND_HEIGHT
            );
        }
        let range = if index == 0 {
            format!("under {below:.0}")
        } else if below.is_finite() {
            format!("{from:.0} to under {below:.0}")
        } else {
            format!("{from:.0} and more")
        };
        let _ = write!(
            svg,
            "<g class=\"slot\"><rect class=\"hit\" x=\"{:.1}\" y=\"{BAND_TOP:.1}\" \
             width=\"{:.1}\" height=\"{BAND_HEIGHT:.1}\"/><text class=\"ea-class{}\" \
             x=\"{:.1}\" y=\"{:.1}\" text-anchor=\"middle\">{letter}</text><text \
             class=\"readout\" x=\"{LEFT:.1}\" y=\"{READOUT_BASELINE:.1}\">{}</text></g>",
            x(from),
            x(to) - x(from),
            if ours.contains(letter) { " ours" } else { "" },
            (x(from) + x(to)) / 2.0,
            BAND_TOP + BAND_HEIGHT / 2.0 + if ours.contains(letter) { 8.5 } else { 5.0 },
            escape(&format!("Class {letter}: {range} kWh/(m²·a)"))
        );
        from = *below;
    }

    // The ticks, every 25 up to 250, and past it the rest of H.
    for tick in (0..=10).map(|step| f64::from(step) * 25.0) {
        let _ = write!(
            svg,
            "<text class=\"ea-tick\" x=\"{:.1}\" y=\"{TICKS:.1}\" text-anchor=\"middle\">\
             {tick:.0}</text>",
            x(tick)
        );
    }
    let _ = write!(
        svg,
        "<text class=\"ea-tick\" x=\"{:.1}\" y=\"{TICKS:.1}\" text-anchor=\"end\">&gt;250</text>",
        WIDTH - RIGHT
    );

    svg.push_str(&marker(
        heated,
        &format!("{} heated", format_per_square_metre(heated)),
        "over the floor heated",
        true,
    ));
    if let Some(total) = total {
        svg.push_str(&marker(
            total,
            &format!("{} over all the floor", format_per_square_metre(total)),
            "over all the floor",
            false,
        ));
    }
    svg.push_str("</svg>");
    svg
}

/// The house's figure on the scale: an arrow onto the band from above, or onto the bar
/// from below, with its label beside it.
fn marker(per_square_metre: f64, label: &str, what: &str, above: bool) -> String {
    let at = x(per_square_metre);
    let (anchor, label_x) = if at < LEFT + LABEL_ROOM {
        ("start", (at - 10.0).max(LEFT))
    } else if at > WIDTH - RIGHT - LABEL_ROOM {
        ("end", (at + 10.0).min(WIDTH - RIGHT))
    } else {
        ("middle", at)
    };
    let (arrow, label_y, hit_y) = if above {
        (
            format!(
                "M{:.1},{:.1}H{:.1}L{at:.1},{:.1}Z",
                at - 7.0,
                TOP_LABEL + 4.0,
                at + 7.0,
                BAND_TOP - 2.0
            ),
            TOP_LABEL,
            TOP_LABEL - 14.0,
        )
    } else {
        let tip = BAR_TOP + BAR_HEIGHT + 2.0;
        (
            format!(
                "M{:.1},{:.1}H{:.1}L{at:.1},{tip:.1}Z",
                at - 7.0,
                tip + 14.0,
                at + 7.0
            ),
            BOTTOM_LABEL,
            tip,
        )
    };
    format!(
        "<g class=\"slot\"><rect class=\"hit\" x=\"{:.1}\" y=\"{hit_y:.1}\" width=\"{:.1}\" \
         height=\"30\"/><path class=\"ea-arrow\" d=\"{arrow}\"/><text class=\"ea-label\" \
         x=\"{label_x:.1}\" y=\"{label_y:.1}\" text-anchor=\"{anchor}\">{}</text><text \
         class=\"readout\" x=\"{LEFT:.1}\" y=\"{READOUT_BASELINE:.1}\">{}</text></g>",
        at - LABEL_ROOM,
        2.0 * LABEL_ROOM,
        escape(label),
        escape(&format!(
            "{} {what}: class {}",
            format_per_square_metre(per_square_metre),
            class(per_square_metre)
        ))
    )
}

/// The table twin of the scale: the latest twelve months and every whole season.
fn table(benchmark: &Benchmark) -> String {
    let mut html = String::from(
        "<div class=\"table-scroll\"><table><caption>Twelve whole months at a time: every \
         heating season, July to June, that is whole, and the latest twelve months.</caption>\
         <thead><tr><th scope=\"col\">Year</th><th scope=\"col\">Heating energy</th>\
         <th scope=\"col\">Over the floor heated</th><th scope=\"col\">Class</th>",
    );
    if benchmark.total_area.is_some() {
        html.push_str("<th scope=\"col\">Over all the floor</th><th scope=\"col\">Class</th>");
    }
    html.push_str("</tr></thead><tbody>");
    let mut years: Vec<(Year, bool)> = benchmark
        .seasons
        .iter()
        .map(|season| (*season, Some(*season) == benchmark.latest))
        .collect();
    if let Some(latest) = benchmark.latest
        && latest.season.is_none()
    {
        years.push((latest, true));
    }
    for (year, latest) in years {
        let heated = year.per_square_metre(benchmark.heated_area);
        let _ = write!(
            html,
            "<tr><th scope=\"row\">{}{}</th><td>{}</td><td>{}</td><td>{}</td>",
            escape(&year.label()),
            if latest { " (the latest)" } else { "" },
            escape(&format_energy(year.heating_kwh)),
            escape(&format_per_square_metre(heated)),
            class(heated)
        );
        if let Some(area) = benchmark.total_area {
            let total = year.per_square_metre(area);
            let _ = write!(
                html,
                "<td>{}</td><td>{}</td>",
                escape(&format_per_square_metre(total)),
                class(total)
            );
        }
        html.push_str("</tr>");
    }
    html.push_str("</tbody></table></div>");
    html
}

/// A floor area: `142`, `18.5`.
fn format_area(square_metres: f64) -> String {
    let text = format!("{square_metres:.1}");
    text.strip_suffix(".0").unwrap_or(&text).to_string()
}

/// The rules only this section needs, added with the heating energy's.
pub const CSS: &str = "
.ea-chart .ea-sep { stroke: #1a1a1a; stroke-opacity: 0.55; stroke-width: 1.5; }
.ea-chart .ea-class { fill: #1a1a1a; font-size: 14px; font-weight: 600; pointer-events: none; }
.ea-chart .ea-class.ours { font-size: 26px; font-weight: 800; }
.ea-chart .ea-tick { fill: var(--ink); font-size: 12px; font-weight: 600; font-variant-numeric: tabular-nums; }
.ea-chart .ea-arrow { fill: var(--ink); }
.ea-chart .ea-label { fill: var(--ink); font-size: 12px; font-weight: 600; font-variant-numeric: tabular-nums; }
.ea-chart .slot:hover .ea-arrow { fill: var(--ink-secondary); }
.ea-chart rect.hit:hover { fill: #ffffff; opacity: 0.3; }
";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::energy::{Energy, Month};

    fn section_of(benchmark: Option<Benchmark>) -> String {
        section(&EnergySection {
            entity: "number.heizol_energy".to_string(),
            energy: Energy {
                readings: 13,
                months: Vec::new(),
                overall: None,
                seasons: Vec::new(),
                beyond_record: 0,
                water_per_month: None,
            },
            from_raw_states: false,
            heat_loss: None,
            benchmark,
        })
    }

    fn season(start_year: i32, heating_kwh: f64) -> Year {
        Year {
            first: Month {
                year: start_year,
                month: 7,
            },
            last: Month {
                year: start_year + 1,
                month: 6,
            },
            heating_kwh,
            kwh: heating_kwh + 4_000.0,
            season: Some(start_year),
        }
    }

    #[test]
    fn the_house_is_marked_on_the_certificates_scale() {
        let html = section_of(Some(Benchmark {
            heated_area: 80.0,
            heated_from_rooms: true,
            total_area: Some(120.0),
            latest: Some(season(2023, 7_200.0)),
            seasons: vec![season(2022, 8_800.0), season(2023, 7_200.0)],
        }));
        assert!(html.contains("<h2>Annual benchmark</h2>"));
        assert!(html.contains(
            "For 2023/24, the latest twelve whole months: <strong>90 kWh/(m²·a)</strong> over \
             the floor it heats - class <strong>C</strong>, and <strong>60 kWh/(m²·a)</strong> \
             over all the floor - class <strong>B</strong>."
        ));
        // Both classes stand out on the band, and nine classes are drawn.
        assert_eq!(html.matches("class=\"ea-class ours\"").count(), 2);
        assert_eq!(html.matches("class=\"ea-class").count(), 9);
        assert_eq!(html.matches("class=\"ea-arrow\"").count(), 2);
        assert!(html.contains("Class C: 75 to under 100 kWh/(m²·a)"));
        assert!(html.contains("Class H: 250 and more kWh/(m²·a)"));
        assert!(html.contains("&gt;250</text>"));
        assert!(html.contains("<th scope=\"row\">2023/24 (the latest)</th><td>7,200 kWh</td>"));
        assert!(html.contains("<th scope=\"row\">2022/23</th><td>8,800 kWh</td><td>110 kWh/(m²·a)</td><td>D</td><td>73 kWh/(m²·a)</td><td>B</td>"));
        // With water heating: 11,200 kWh over 80 m².
        assert!(html.contains("come to 140 kWh/(m²·a) over the floor heated - class E"));
    }

    #[test]
    fn without_a_whole_year_it_says_what_it_needs() {
        let html = section_of(Some(Benchmark {
            heated_area: 80.0,
            heated_from_rooms: false,
            total_area: None,
            latest: None,
            seasons: Vec::new(),
        }));
        assert!(html.contains("There is no whole year yet"));
        assert!(!html.contains("<svg"));
        assert_eq!(section_of(None), "");
    }

    #[test]
    fn a_figure_past_the_scale_sits_at_its_end() {
        assert_eq!(x(400.0), WIDTH - RIGHT);
        assert_eq!(x(0.0), LEFT);
    }
}
