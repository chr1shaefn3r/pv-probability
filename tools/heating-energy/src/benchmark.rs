//! The annual benchmark: a year's heating energy over the floor area, in kWh per m² and
//! year, on the scale an Energieausweis puts it on.
//!
//! A year is twelve whole months in a row whose energy is sound. The latest such year is
//! the benchmark; every whole heating season, July to June, is given beside it, so the
//! years can be compared.

use crate::energy::{Energy, Month, MonthPoint, season_label};

/// The Energieausweis's efficiency classes for homes, by final energy in kWh/(m²·a): each
/// up to below its bound, H from 250 on.
pub const CLASSES: [(&str, f64); 9] = [
    ("A+", 30.0),
    ("A", 50.0),
    ("B", 75.0),
    ("C", 100.0),
    ("D", 130.0),
    ("E", 160.0),
    ("F", 200.0),
    ("G", 250.0),
    ("H", f64::INFINITY),
];

/// The class a figure in kWh/(m²·a) falls in.
pub fn class(per_square_metre: f64) -> &'static str {
    CLASSES
        .iter()
        .find(|(_, below)| per_square_metre < *below)
        .map_or("H", |(class, _)| class)
}

/// Twelve whole months in a row.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Year {
    pub first: Month,
    pub last: Month,
    /// Heating energy, water heating taken off, in kWh.
    pub heating_kwh: f64,
    /// All the energy, water heating in, in kWh.
    pub kwh: f64,
    /// The heating season it is, if it runs from July to June.
    pub season: Option<i32>,
}

impl Year {
    /// `2023/24` for a heating season, else `December 2023 to November 2024`.
    pub fn label(&self) -> String {
        match self.season {
            Some(start_year) => season_label(start_year),
            None => format!("{} to {}", self.first.label(), self.last.label()),
        }
    }

    /// Its heating energy over `area` m², in kWh/(m²·a).
    pub fn per_square_metre(&self, area: f64) -> f64 {
        self.heating_kwh / area
    }
}

/// Everything the report and the terminal say about the annual benchmark.
#[derive(Debug, Clone, PartialEq)]
pub struct Benchmark {
    /// The floor area the heating heats, in m².
    pub heated_area: f64,
    /// Whether that is the rooms' areas added up, rather than the house's own figure.
    pub heated_from_rooms: bool,
    /// All the floor, heated or not, in m², if the rooms file gives it.
    pub total_area: Option<f64>,
    /// The latest twelve whole months in a row whose energy is sound.
    pub latest: Option<Year>,
    /// Every whole heating season, July to June, in order.
    pub seasons: Vec<Year>,
}

/// The year twelve months make, if they run on one after the other and every one's energy
/// is sound.
fn year(months: &[MonthPoint]) -> Option<Year> {
    let [first, .., last] = months else {
        return None;
    };
    let whole = months.len() == 12
        && months
            .windows(2)
            .all(|pair| pair[0].month.next() == pair[1].month)
        && months.iter().all(MonthPoint::has_energy);
    whole.then(|| Year {
        first: first.month,
        last: last.month,
        heating_kwh: months.iter().map(MonthPoint::heating_kwh).sum(),
        kwh: months.iter().map(|month| month.kwh).sum(),
        season: (first.month.month == 7).then_some(first.month.year),
    })
}

/// Find the years in the heating energy, and set them against the floor areas.
pub fn analyse(
    energy: &Energy,
    heated_area: f64,
    heated_from_rooms: bool,
    total_area: Option<f64>,
) -> Benchmark {
    let latest = energy.months.windows(12).rev().find_map(year);
    let seasons = energy
        .months
        .windows(12)
        .filter(|months| months[0].month.month == 7)
        .filter_map(year)
        .collect();
    Benchmark {
        heated_area,
        heated_from_rooms,
        total_area,
        latest,
        seasons,
    }
}

/// A figure in kWh/(m²·a) as it is written: `73 kWh/(m²·a)`.
pub fn format_per_square_metre(per_square_metre: f64) -> String {
    format!("{per_square_metre:.0} kWh/(m²·a)")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn month(year: i32, number: u32, kwh: f64, water_kwh: f64) -> MonthPoint {
        let month = Month {
            year,
            month: number,
        };
        MonthPoint {
            month,
            kwh,
            counted_days: month.days(),
            hdd: 0.0,
            outside_mean: Some(5.0),
            follows_unchanged: false,
            water_kwh,
        }
    }

    /// Every month from July 2023 for `count` months, 1,000 kWh each, 200 of it water
    /// heating.
    fn months(count: usize) -> Vec<MonthPoint> {
        let mut at = Month {
            year: 2023,
            month: 7,
        };
        let mut months = Vec::new();
        for _ in 0..count {
            months.push(month(at.year, at.month, 1_000.0, 200.0));
            at = at.next();
        }
        months
    }

    fn energy(months: Vec<MonthPoint>) -> Energy {
        Energy {
            readings: months.len() + 1,
            months,
            overall: None,
            seasons: Vec::new(),
            beyond_record: 0,
            water_per_month: None,
        }
    }

    #[test]
    fn a_figure_falls_in_its_class() {
        assert_eq!(class(12.0), "A+");
        assert_eq!(class(30.0), "A", "a bound belongs to the class above");
        assert_eq!(class(99.9), "C");
        assert_eq!(class(130.0), "E");
        assert_eq!(class(250.0), "H");
        assert_eq!(class(900.0), "H");
        assert_eq!(format_per_square_metre(73.4), "73 kWh/(m²·a)");
    }

    #[test]
    fn a_year_is_the_latest_twelve_whole_months_and_every_whole_season_is_one() {
        // July 2023 to November 2024: one whole season, and a later twelve months.
        let benchmark = analyse(&energy(months(17)), 80.0, true, Some(100.0));
        let latest = benchmark.latest.unwrap();
        assert_eq!(latest.label(), "December 2023 to November 2024");
        assert_eq!(latest.season, None);
        assert_eq!(latest.heating_kwh, 12.0 * 800.0);
        assert_eq!(latest.kwh, 12.0 * 1_000.0);
        assert_eq!(latest.per_square_metre(80.0), 120.0);
        assert_eq!(benchmark.seasons.len(), 1);
        assert_eq!(benchmark.seasons[0].label(), "2023/24");
        assert_eq!(benchmark.seasons[0].season, Some(2023));
    }

    #[test]
    fn a_month_whose_energy_is_not_sound_breaks_the_year() {
        let mut twelve = months(13);
        // The meter was not read at the end of December 2023: December and January out.
        twelve[5].kwh = 0.0;
        twelve[6].follows_unchanged = true;
        let benchmark = analyse(&energy(twelve.clone()), 80.0, true, None);
        assert_eq!(benchmark.latest, None, "no twelve sound months in a row");
        assert!(benchmark.seasons.is_empty());
        // Nor do twelve months with one missing in between make a year.
        twelve.remove(3);
        assert_eq!(analyse(&energy(twelve), 80.0, true, None).latest, None);
    }
}
