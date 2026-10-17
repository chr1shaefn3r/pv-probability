//! Degree days: how much heating, or cooling, the weather asked for.
//!
//! Pick a base temperature, the outside temperature above which the house needs no
//! heating. A day averaging below it needs heating in proportion to how far below:
//!
//! * heating degree days (HDD) = max(0, heating base - daily mean outside temperature)
//! * cooling degree days (CDD) = max(0, daily mean outside temperature - cooling base)
//!
//! With a 15 °C base, a day averaging 3 °C counts 12 HDD and a day averaging 16 °C none.
//! Summed over a month they measure the weather's demand, so a house's consumption can be
//! compared across months and years with the weather taken out.

use std::collections::BTreeMap;

use chrono::{Datelike, NaiveDate};

use ha_recorder::model::Grouping;
use ha_recorder::timeutil::DayNumber;

use crate::daily::DayMean;
use crate::rooms::House;

/// The default heating base: the outside temperature above which a typical house in
/// Central Europe stops heating.
pub const DEFAULT_HEATING_BASE: f64 = 15.0;
/// The default cooling base, as Eurostat counts cooling degree days.
pub const DEFAULT_COOLING_BASE: f64 = 21.0;

/// The two base temperatures, in °C.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bases {
    pub heating: f64,
    pub cooling: f64,
}

impl Default for Bases {
    fn default() -> Self {
        Self {
            heating: DEFAULT_HEATING_BASE,
            cooling: DEFAULT_COOLING_BASE,
        }
    }
}

impl Bases {
    /// Heating degree days for a day with this mean outside temperature.
    pub fn hdd(self, mean: f64) -> f64 {
        (self.heating - mean).max(0.0)
    }

    /// Cooling degree days for a day with this mean outside temperature.
    pub fn cdd(self, mean: f64) -> f64 {
        (mean - self.cooling).max(0.0)
    }
}

/// A stretch of calendar a sum is taken over.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Period {
    Month {
        year: i32,
        month: u32,
    },
    /// An ISO week, numbered within its ISO year.
    Week {
        year: i32,
        week: u32,
    },
    Year {
        year: i32,
    },
    /// Everything recorded.
    Record,
}

impl Period {
    /// The month or week a date falls in.
    pub fn of(date: NaiveDate, grouping: Grouping) -> Period {
        match grouping {
            Grouping::Month => Period::Month {
                year: date.year(),
                month: date.month(),
            },
            Grouping::Week => {
                let week = date.iso_week();
                Period::Week {
                    year: week.year(),
                    week: week.week(),
                }
            }
        }
    }

    /// `January 2024`, `Week 05, 2024`, `2024`, `Whole record`.
    pub fn label(self) -> String {
        match self {
            Period::Month { year, month } => format!("{} {year}", month_name(month)),
            Period::Week { year, week } => format!("Week {week:02}, {year}"),
            Period::Year { year } => year.to_string(),
            Period::Record => "Whole record".to_string(),
        }
    }

    /// A label for a crowded axis: `Jan`, `W05`, `2024`.
    pub fn short_label(self) -> String {
        match self {
            Period::Month { month, .. } => month_name(month)[..3].to_string(),
            Period::Week { week, .. } => format!("W{week:02}"),
            Period::Year { year } => year.to_string(),
            Period::Record => "All".to_string(),
        }
    }

    /// The calendar year a period starts in, for marking year boundaries on an axis.
    pub fn year(self) -> Option<i32> {
        match self {
            Period::Month { year, .. } | Period::Week { year, .. } | Period::Year { year } => {
                Some(year)
            }
            Period::Record => None,
        }
    }

    fn contains(self, date: NaiveDate) -> bool {
        match self {
            Period::Month { .. } => Period::of(date, Grouping::Month) == self,
            Period::Week { .. } => Period::of(date, Grouping::Week) == self,
            Period::Year { year } => date.year() == year,
            Period::Record => true,
        }
    }
}

fn month_name(month: u32) -> &'static str {
    const NAMES: [&str; 12] = [
        "January",
        "February",
        "March",
        "April",
        "May",
        "June",
        "July",
        "August",
        "September",
        "October",
        "November",
        "December",
    ];
    NAMES[(month.clamp(1, 12) - 1) as usize]
}

/// One period's degree days, and the indoor temperatures over the same days.
#[derive(Debug, Clone, PartialEq)]
pub struct Summary {
    pub period: Period,
    /// Days that counted: the outside thermometer covered enough of them.
    pub days: u32,
    /// Days of the period inside the span the outside thermometer was recorded over.
    pub possible_days: u32,
    /// Mean of the counted days' mean outside temperatures, in °C.
    pub outside: Option<f64>,
    pub hdd: f64,
    pub cdd: f64,
    /// Each room's mean indoor temperature over the counted days it was also covered on,
    /// in the house's order; `None` when it never was.
    pub rooms: Vec<Option<f64>>,
    /// Each floor's mean: the mean of its rooms' means, every room counting alike.
    pub floors: Vec<Option<f64>>,
}

impl Summary {
    /// Whether every day the period could have had counted.
    pub fn is_complete(&self) -> bool {
        self.days >= self.possible_days
    }
}

/// Everything the report needs.
#[derive(Debug, Clone, PartialEq)]
pub struct Analysis {
    pub bases: Bases,
    pub grouping: Grouping,
    pub min_coverage: f64,
    /// One summary per month or week that has any counted day, in calendar order.
    pub periods: Vec<Summary>,
    /// One per calendar year with any counted day.
    pub years: Vec<Summary>,
    pub record: Summary,
    /// Days the outside thermometer touched but covered too little of to count.
    pub short_days: u32,
    /// The first and last local day the outside thermometer touched at all.
    pub span: Option<(NaiveDate, NaiveDate)>,
}

/// Count degree days from the outside thermometer's daily means and sum them per period,
/// with every room's indoor mean over the same days.
///
/// A day counts when the outside thermometer covered at least `min_coverage` of it: a day
/// whose night is missing would read warm and understate the heating it needed. `rooms`
/// holds each room's daily means in the house's order, and a room's day counts by the same
/// rule.
pub fn analyse(
    outside: &[DayMean],
    rooms: &[Vec<DayMean>],
    house: Option<&House>,
    bases: Bases,
    min_coverage: f64,
    grouping: Grouping,
) -> Analysis {
    let counted: Vec<(NaiveDate, f64)> = outside
        .iter()
        .filter(|day| day.coverage() >= min_coverage)
        .filter_map(|day| Some((day.date()?, day.mean)))
        .collect();
    let short_days = (outside.len() - counted.len()) as u32;
    let span = outside
        .first()
        .and_then(DayMean::date)
        .zip(outside.last().and_then(DayMean::date));

    let indoor: Vec<BTreeMap<DayNumber, f64>> = rooms
        .iter()
        .map(|days| {
            days.iter()
                .filter(|day| day.coverage() >= min_coverage)
                .map(|day| (day.day, day.mean))
                .collect()
        })
        .collect();

    let summarise = |period: Period| -> Summary {
        let days: Vec<(NaiveDate, f64)> = counted
            .iter()
            .copied()
            .filter(|(date, _)| period.contains(*date))
            .collect();
        let possible_days = span.map_or(0, |(first, last)| {
            first
                .iter_days()
                .take_while(|date| *date <= last)
                .filter(|date| period.contains(*date))
                .count() as u32
        });
        let room_means: Vec<Option<f64>> = indoor
            .iter()
            .map(|room| {
                mean(
                    days.iter()
                        .filter_map(|(date, _)| room.get(&date.num_days_from_ce()).copied()),
                )
            })
            .collect();
        let floors = match house {
            Some(house) => {
                let mut next = 0;
                house
                    .floors
                    .iter()
                    .map(|floor| {
                        let own = &room_means[next..next + floor.rooms.len()];
                        next += floor.rooms.len();
                        mean(own.iter().flatten().copied())
                    })
                    .collect()
            }
            None => Vec::new(),
        };
        Summary {
            period,
            days: days.len() as u32,
            possible_days,
            outside: mean(days.iter().map(|(_, mean)| *mean)),
            hdd: days.iter().map(|(_, mean)| bases.hdd(*mean)).sum(),
            cdd: days.iter().map(|(_, mean)| bases.cdd(*mean)).sum(),
            rooms: room_means,
            floors,
        }
    };

    let distinct = |key: &dyn Fn(NaiveDate) -> Period| -> Vec<Period> {
        let mut periods: Vec<Period> = counted.iter().map(|(date, _)| key(*date)).collect();
        periods.sort_unstable();
        periods.dedup();
        periods
    };
    let periods = distinct(&|date| Period::of(date, grouping))
        .into_iter()
        .map(summarise)
        .collect();
    let years = distinct(&|date| Period::Year { year: date.year() })
        .into_iter()
        .map(summarise)
        .collect();

    Analysis {
        bases,
        grouping,
        min_coverage,
        periods,
        years,
        record: summarise(Period::Record),
        short_days,
        span,
    }
}

fn mean(values: impl Iterator<Item = f64>) -> Option<f64> {
    let (sum, count) = values.fold((0.0, 0u32), |(sum, count), value| (sum + value, count + 1));
    (count > 0).then(|| sum / f64::from(count))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rooms::House;

    fn day(date: &str, mean: f64, coverage: f64) -> DayMean {
        let date = NaiveDate::parse_from_str(date, "%Y-%m-%d").unwrap();
        DayMean {
            day: date.num_days_from_ce(),
            mean,
            covered_seconds: coverage * 86_400.0,
            day_seconds: 86_400.0,
        }
    }

    /// `count` consecutive whole days from `start`, each at `mean`.
    fn days(start: &str, count: usize, mean: f64) -> Vec<DayMean> {
        let first = NaiveDate::parse_from_str(start, "%Y-%m-%d").unwrap();
        first
            .iter_days()
            .take(count)
            .map(|date| day(&date.to_string(), mean, 1.0))
            .collect()
    }

    #[test]
    fn the_examples_from_the_definition() {
        let bases = Bases::default();
        assert_eq!(bases.hdd(3.0), 12.0, "a day averaging 3 °C counts 12 HDD");
        assert_eq!(bases.hdd(16.0), 0.0, "a day averaging 16 °C counts none");
        assert_eq!(bases.hdd(15.0), 0.0);
        assert_eq!(bases.cdd(16.0), 0.0);
        assert_eq!(bases.cdd(24.5), 3.5);
    }

    #[test]
    fn a_month_sums_its_days() {
        // Ten days at 3 °C and five at 16 °C, all in January.
        let mut outside = days("2024-01-01", 10, 3.0);
        outside.extend(days("2024-01-11", 5, 16.0));
        let analysis = analyse(&outside, &[], None, Bases::default(), 0.9, Grouping::Month);

        assert_eq!(analysis.periods.len(), 1);
        let january = &analysis.periods[0];
        assert_eq!(
            january.period,
            Period::Month {
                year: 2024,
                month: 1
            }
        );
        assert_eq!(january.period.label(), "January 2024");
        assert_eq!(january.days, 15);
        assert_eq!(
            january.possible_days, 15,
            "the record spans only these days"
        );
        assert_eq!(january.hdd, 120.0);
        assert_eq!(january.cdd, 0.0);
        assert!((january.outside.unwrap() - (10.0 * 3.0 + 5.0 * 16.0) / 15.0).abs() < 1e-12);
        assert!(january.is_complete());
        assert_eq!(analysis.record.hdd, 120.0);
        assert_eq!(analysis.years.len(), 1);
    }

    #[test]
    fn a_day_the_thermometer_mostly_missed_does_not_count() {
        let outside = vec![
            day("2024-01-01", 0.0, 1.0),
            day("2024-01-02", 10.0, 0.5), // only the afternoon: reads warm
            day("2024-01-03", 0.0, 0.95),
        ];
        let analysis = analyse(&outside, &[], None, Bases::default(), 0.9, Grouping::Month);
        let january = &analysis.periods[0];
        assert_eq!(january.days, 2);
        assert_eq!(january.possible_days, 3);
        assert!(!january.is_complete());
        assert_eq!(january.hdd, 30.0);
        assert_eq!(analysis.short_days, 1);
    }

    #[test]
    fn periods_follow_the_calendar() {
        // 30 December 2024 to 2 January 2025: two months, two years, but one ISO week -
        // week 1 of 2025 starts on Monday 30 December.
        let outside = days("2024-12-30", 4, 5.0);
        let months = analyse(&outside, &[], None, Bases::default(), 0.9, Grouping::Month);
        let labels: Vec<String> = months.periods.iter().map(|s| s.period.label()).collect();
        assert_eq!(labels, ["December 2024", "January 2025"]);
        let years: Vec<String> = months.years.iter().map(|s| s.period.label()).collect();
        assert_eq!(years, ["2024", "2025"]);
        assert_eq!(months.years[0].hdd, 20.0);

        let weeks = analyse(&outside, &[], None, Bases::default(), 0.9, Grouping::Week);
        assert_eq!(weeks.periods.len(), 1);
        assert_eq!(
            weeks.periods[0].period,
            Period::Week {
                year: 2025,
                week: 1
            }
        );
        assert_eq!(weeks.periods[0].period.label(), "Week 01, 2025");
        assert_eq!(weeks.periods[0].hdd, 40.0);
    }

    #[test]
    fn hot_days_count_towards_cooling_and_not_heating() {
        let mut outside = days("2024-07-01", 3, 25.0);
        outside.extend(days("2024-07-04", 2, 18.0));
        let analysis = analyse(&outside, &[], None, Bases::default(), 0.9, Grouping::Month);
        assert_eq!(analysis.record.cdd, 12.0);
        assert_eq!(analysis.record.hdd, 0.0);

        let warm_house = Bases {
            heating: 19.0,
            cooling: 24.0,
        };
        let analysis = analyse(&outside, &[], None, warm_house, 0.9, Grouping::Month);
        assert_eq!(analysis.record.cdd, 3.0);
        assert_eq!(analysis.record.hdd, 2.0);
    }

    #[test]
    fn rooms_and_floors_are_averaged_over_the_counted_days() {
        let house = House::parse(
            r#"
            [[floor]]
            name = "Ground"
            rooms = [{ name = "A", sensor = "s.a" }, { name = "B", sensor = "s.b" }]
            [[floor]]
            name = "Top"
            rooms = [{ name = "C", sensor = "s.c" }]
            "#,
        )
        .unwrap();
        let mut outside = days("2024-01-01", 3, 0.0);
        outside.push(day("2024-01-04", 0.0, 0.2)); // not counted
        let rooms = vec![
            days("2024-01-01", 4, 20.0),
            // Room B is 22 °C on the counted days and 30 °C on the one that does not count.
            vec![
                day("2024-01-01", 22.0, 1.0),
                day("2024-01-02", 22.0, 1.0),
                day("2024-01-04", 30.0, 1.0),
            ],
            Vec::new(), // C was never recorded
        ];
        let analysis = analyse(
            &outside,
            &rooms,
            Some(&house),
            Bases::default(),
            0.9,
            Grouping::Month,
        );
        let january = &analysis.periods[0];
        assert_eq!(january.rooms, [Some(20.0), Some(22.0), None]);
        assert_eq!(january.floors, [Some(21.0), None], "each room counts alike");
        assert_eq!(analysis.record.rooms, january.rooms);
    }

    #[test]
    fn no_counted_days_means_no_periods() {
        let outside = vec![day("2024-01-01", 0.0, 0.1)];
        let analysis = analyse(&outside, &[], None, Bases::default(), 0.9, Grouping::Month);
        assert!(analysis.periods.is_empty());
        assert_eq!(analysis.record.days, 0);
        assert_eq!(analysis.record.outside, None);
        assert_eq!(analysis.record.possible_days, 1);
    }
}
