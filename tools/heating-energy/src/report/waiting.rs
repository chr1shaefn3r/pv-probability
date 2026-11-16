//! The sections the report cannot draw for want of data. Each keeps its heading, and says
//! what it would show, what it needs - on the command line or in the rooms file - and
//! where the README says more, so that nothing is left out silently.

use std::fmt::Write as _;

use super::ReportOptions;

/// The README the hints link to. A link is the one thing in the report from elsewhere,
/// and it loads nothing: the page stays self-contained.
pub const README: &str = concat!(
    env!("CARGO_PKG_REPOSITORY"),
    "/blob/main/tools/heating-energy/README.md"
);

/// A section the report draws only when it is given what it needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    HeatingEnergy,
    /// A part of the Heating energy section.
    BuildingOrBehaviour,
    DegreeWarmer,
    Benchmark,
    IndoorTemperatures,
}

impl Section {
    pub fn title(self) -> &'static str {
        match self {
            Section::HeatingEnergy => "Heating energy",
            Section::BuildingOrBehaviour => "Building or behaviour",
            Section::DegreeWarmer => "A degree warmer",
            Section::Benchmark => "Annual benchmark",
            Section::IndoorTemperatures => "Indoor temperatures",
        }
    }

    /// What it would show, in a sentence.
    fn shows(self) -> &'static str {
        match self {
            Section::HeatingEnergy => {
                "What one more heating degree day costs the house, season by season: every \
                 month's heating energy, water heating taken off, against its degree days."
            }
            Section::BuildingOrBehaviour => {
                "Whether a change came from the building or from how warm the house was kept: \
                 its heat loss coefficient, from the rooms' temperatures weighted by their \
                 floor area."
            }
            Section::DegreeWarmer => {
                "What keeping the house a degree warmer costs a season, from its own heat loss, \
                 against the rule of thumb's 6%."
            }
            Section::Benchmark => {
                "A year's heating energy per m² of floor, on the scale an Energieausweis uses."
            }
            Section::IndoorTemperatures => {
                "How warm every room and floor was kept, over the days the degree days were \
                 counted on."
            }
        }
    }

    /// Where the README tells of it.
    fn anchor(self) -> &'static str {
        match self {
            Section::HeatingEnergy => "heating-energy-and-efficiency",
            Section::BuildingOrBehaviour => "building-or-behaviour-the-heat-loss-coefficient",
            Section::DegreeWarmer => "a-degree-warmer",
            Section::Benchmark => "annual-benchmark",
            Section::IndoorTemperatures => "the-rooms-file",
        }
    }
}

/// Something a section needs that the run was not given.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Need {
    /// `--energy-entity`.
    EnergyEntity,
    /// `--rooms`.
    Rooms,
    /// A floor area for the rooms in the rooms file.
    RoomAreas,
    /// A floor area the heating heats: the rooms', or the house's `heated_area`.
    HeatedArea,
}

/// A section the report cannot draw, and what it needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Waiting {
    pub section: Section,
    pub needs: Vec<Need>,
}

/// Every section the run was not given enough for, in the report's order.
pub fn waiting(options: &ReportOptions) -> Vec<Waiting> {
    let energy = options.energy.is_some();
    let house = options.house.as_ref();
    let rooms = house.is_some();
    let areas = house.is_some_and(|house| house.area() > 0.0);
    let heated = house.is_some_and(|house| house.heated_area().is_some());
    let needs = |pairs: &[(bool, Need)]| -> Vec<Need> {
        pairs
            .iter()
            .filter(|(given, _)| !given)
            .map(|(_, need)| *need)
            .collect()
    };
    let heat_loss = needs(&[
        (energy, Need::EnergyEntity),
        (rooms, Need::Rooms),
        (areas, Need::RoomAreas),
    ]);
    [
        (
            Section::HeatingEnergy,
            needs(&[(energy, Need::EnergyEntity)]),
        ),
        (Section::BuildingOrBehaviour, heat_loss.clone()),
        (Section::DegreeWarmer, heat_loss),
        (
            Section::Benchmark,
            needs(&[
                (energy, Need::EnergyEntity),
                (rooms, Need::Rooms),
                (heated, Need::HeatedArea),
            ]),
        ),
        (Section::IndoorTemperatures, needs(&[(rooms, Need::Rooms)])),
    ]
    .into_iter()
    .filter(|(_, needs)| !needs.is_empty())
    .map(|(section, needs)| Waiting { section, needs })
    .collect()
}

/// What the needs are, as a phrase for the report: the rooms file and what it should say
/// in it read as one.
fn needs_html(needs: &[Need]) -> String {
    let has = |need: Need| needs.contains(&need);
    let mut items: Vec<String> = Vec::new();
    if has(Need::EnergyEntity) {
        items.push(
            "the heating's energy as a running total in kWh, with <code>--energy-entity</code>"
                .to_string(),
        );
    }
    let rooms_file = "a rooms file, <code>--rooms</code>,";
    match (
        has(Need::Rooms),
        has(Need::RoomAreas),
        has(Need::HeatedArea),
    ) {
        (true, true, _) => items.push(format!(
            "{rooms_file} giving every room its floor area, <code>area = …</code> in m²"
        )),
        (true, false, true) => items.push(format!(
            "{rooms_file} with the rooms' floor areas or the house's \
             <code>heated_area</code>"
        )),
        (true, false, false) => items.push(format!("{rooms_file} naming every room's thermometer")),
        (false, true, _) => items.push(
            "every room's floor area in the rooms file, <code>area = …</code> in m²".to_string(),
        ),
        (false, false, true) => items.push(
            "a floor area in the rooms file: every room's <code>area</code>, or the house's \
             <code>heated_area</code> under <code>[house]</code>"
                .to_string(),
        ),
        (false, false, false) => {}
    }
    items.join(", and ")
}

/// The heading of a section the report cannot draw, what it would show and what it
/// needs: a panel of its own, holding `inner` after its hint, or - for a part of a
/// section - a lesser heading.
pub fn placeholder(waiting: &Waiting, inner: &str) -> String {
    let section = waiting.section;
    let hint = format!(
        "<p class=\"waiting\">{} It needs {}. <a href=\"{README}#{}\">The README</a> says how.\
         </p>",
        section.shows(),
        needs_html(&waiting.needs),
        section.anchor()
    );
    let mut html = String::new();
    if section == Section::BuildingOrBehaviour {
        let _ = write!(html, "<h3>{}</h3>{hint}{inner}", section.title());
    } else {
        let _ = write!(
            html,
            "<section class=\"panel waiting\"><h2>{}</h2>{hint}{inner}</section>",
            section.title()
        );
    }
    html
}

/// What the waiting sections need, all together, as the terminal says it: `--energy-entity
/// and --rooms with the rooms' floor areas`.
pub fn needs_in_short(waiting: &[Waiting]) -> String {
    let any = |need: Need| waiting.iter().any(|waiting| waiting.needs.contains(&need));
    let mut items: Vec<&str> = Vec::new();
    if any(Need::EnergyEntity) {
        items.push("--energy-entity");
    }
    match (
        any(Need::Rooms),
        any(Need::RoomAreas) || any(Need::HeatedArea),
    ) {
        (true, true) => items.push("--rooms with the rooms' floor areas"),
        (true, false) => items.push("--rooms"),
        (false, true) if any(Need::RoomAreas) => {
            items.push("the rooms' floor areas in the rooms file")
        }
        (false, true) => items.push("a floor area in the rooms file"),
        (false, false) => {}
    }
    items.join(" and ")
}

/// The rules the hints need, on every page.
pub const CSS: &str = "
.waiting { margin-top: 0.5rem; color: var(--ink-secondary); }
section.waiting h3 { font-size: 0.85rem; font-weight: 600; margin: 1rem 0 0.25rem; color: var(--ink); }
.waiting code { font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; font-size: 0.85em; }
.waiting a { color: var(--ink); }
";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rooms::House;

    fn house(rooms: &str) -> House {
        House::parse(&format!(
            "[[floor]]\nname = \"Ground floor\"\nrooms = [{rooms}]\n"
        ))
        .unwrap()
    }

    fn sections(waiting: &[Waiting]) -> Vec<(&'static str, Vec<Need>)> {
        waiting
            .iter()
            .map(|waiting| (waiting.section.title(), waiting.needs.clone()))
            .collect()
    }

    #[test]
    fn a_bare_run_waits_on_every_section_for_what_it_lacks() {
        let waiting = waiting(&ReportOptions::default());
        assert_eq!(
            sections(&waiting),
            [
                ("Heating energy", vec![Need::EnergyEntity]),
                (
                    "Building or behaviour",
                    vec![Need::EnergyEntity, Need::Rooms, Need::RoomAreas]
                ),
                (
                    "A degree warmer",
                    vec![Need::EnergyEntity, Need::Rooms, Need::RoomAreas]
                ),
                (
                    "Annual benchmark",
                    vec![Need::EnergyEntity, Need::Rooms, Need::HeatedArea]
                ),
                ("Indoor temperatures", vec![Need::Rooms]),
            ]
        );
        assert_eq!(
            needs_in_short(&waiting),
            "--energy-entity and --rooms with the rooms' floor areas"
        );
        let html = placeholder(&waiting[4], "");
        assert!(html.contains("<h2>Indoor temperatures</h2>"));
        assert!(html.contains(
            "It needs a rooms file, <code>--rooms</code>, naming every room's thermometer. <a href=\"https://github.com/chr1shaefn3r/pv-probability/blob/main/\
             tools/heating-energy/README.md#the-rooms-file\">The README</a> says how."
        ));
        let html = placeholder(&waiting[1], "");
        assert!(
            html.starts_with("<h3>Building or behaviour</h3>"),
            "a part, not a panel"
        );
        assert!(html.contains("It needs the heating's energy as a running total"));
    }

    #[test]
    fn rooms_without_an_area_wait_only_for_the_areas() {
        let options = ReportOptions {
            house: Some(house("{ name = \"Hall\", sensor = \"sensor.a\" }")),
            ..ReportOptions::default()
        };
        let waiting = waiting(&options);
        assert_eq!(
            waiting[1].needs,
            [Need::EnergyEntity, Need::RoomAreas],
            "the rooms file is there"
        );
        assert!(
            !waiting
                .iter()
                .any(|waiting| waiting.section == Section::IndoorTemperatures)
        );
        assert!(
            placeholder(&waiting[3], "")
                .contains("It needs the heating's energy as a running total in kWh, with <code>")
        );
        assert!(
            placeholder(&waiting[3], "").contains("a floor area in the rooms file: every room's")
        );
        assert_eq!(
            needs_in_short(&waiting),
            "--energy-entity and the rooms' floor areas in the rooms file"
        );
        // A room with an area is all the rooms file can add.
        let options = ReportOptions {
            house: Some(house(
                "{ name = \"Hall\", sensor = \"sensor.a\", area = 12 }",
            )),
            ..ReportOptions::default()
        };
        assert!(
            super::waiting(&options)
                .iter()
                .all(|waiting| waiting.needs == [Need::EnergyEntity])
        );
    }
}
