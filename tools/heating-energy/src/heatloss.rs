//! The heat loss coefficient: what the building loses for every degree it is kept above
//! the outside.
//!
//! Degree days take the inside to be one fixed temperature; the rooms' thermometers say
//! what it was. The house's temperature is the rooms', weighted by their floor area, and
//! every hour's difference between it and the outside adds up, month by month, to
//! degree-hours. Through the heating months' heating energy against their degree-hours
//! runs a straight line: its slope is what the building loses for every kelvin it is kept
//! above the outside, every hour - in watts per kelvin, the heat loss coefficient - and
//! where it meets no degree-hours, below nothing, the heat the sun and the household give
//! for free, which the heating need not make up. Neither the weather nor how warm the house
//! is kept moves the slope: only the building does. Set beside the kWh per degree day,
//! which both move, it tells which of the two changed; and times a season's hours, it is
//! what keeping the house a degree warmer costs.

use std::collections::BTreeMap;

use chrono_tz::Tz;

use ha_recorder::Sample;
use ha_recorder::timeutil::{day_to_date, local_day};

use crate::energy::{Energy, Fit, Month, MonthPoint, fit_spread, format_whole};

/// Share of a month's hours the house's temperature and the outside's must both be known
/// for: the degree-hours of a month missing a few are scaled up to all of them, as its
/// degree days are.
pub const MIN_HOUR_COVERAGE: f64 = 0.9;

/// A change in kWh per degree day smaller than this share is no change to explain.
const HELD: f64 = 0.03;

/// Smallest spread of degree-hours, from the mildest heating month to the coldest, that a
/// line can be fitted along: fifteen degree days' worth.
const MIN_DEGREE_HOUR_SPREAD: f64 = 15.0 * 24.0;

/// What the rule of thumb says a degree warmer costs: about 6% more heating energy.
pub const RULE_OF_THUMB: f64 = 0.06;

const HOUR: i64 = 3_600;

/// One hour's mean temperature: the instant the hour starts, in UTC, and the mean in °C.
pub type HourMean = (i64, f64);

/// Every hour the samples cover at least `min_coverage` of, with its time-weighted mean,
/// in order. Hours run from one full UTC hour to the next.
pub fn hourly_means(samples: &[Sample], min_coverage: f64) -> Vec<HourMean> {
    let mut hours: BTreeMap<i64, (f64, f64)> = BTreeMap::new();
    for sample in samples {
        if sample.duration() <= 0 || !sample.value.is_finite() {
            continue;
        }
        let mut cursor = sample.start_ts;
        while cursor < sample.end_ts {
            let hour = cursor - cursor.rem_euclid(HOUR);
            let next = (hour + HOUR).min(sample.end_ts);
            let seconds = (next - cursor) as f64;
            let (weighted, covered) = hours.entry(hour).or_default();
            *weighted += sample.value * seconds;
            *covered += seconds;
            cursor = next;
        }
    }
    hours
        .into_iter()
        .filter(|(_, (_, covered))| *covered >= min_coverage * HOUR as f64)
        .map(|(hour, (weighted, covered))| (hour, weighted / covered))
        .collect()
}

/// A room's hours: on each, the plain mean of its thermometers that have it, every
/// thermometer counting alike, as the room's days do.
pub fn room_hours(thermometers: &[Vec<HourMean>]) -> Vec<HourMean> {
    if let [only] = thermometers {
        return only.clone();
    }
    let mut hours: BTreeMap<i64, (f64, u32)> = BTreeMap::new();
    for thermometer in thermometers {
        for (hour, mean) in thermometer {
            let (sum, count) = hours.entry(*hour).or_default();
            *sum += mean;
            *count += 1;
        }
    }
    hours
        .into_iter()
        .map(|(hour, (sum, count))| (hour, sum / f64::from(count)))
        .collect()
}

/// The house's hours: on every hour each room has, the rooms' temperatures weighted by
/// their floor area, Σ(area × temperature) / Σ area. An hour a room is missing from is left
/// out rather than taken from the others alone, which would read it warm or cold.
pub fn house_hours(rooms: &[(f64, Vec<HourMean>)]) -> Vec<HourMean> {
    let area: f64 = rooms.iter().map(|(area, _)| area).sum();
    let mut hours: BTreeMap<i64, (f64, usize)> = BTreeMap::new();
    for (room_area, room) in rooms {
        for (hour, mean) in room {
            let (weighted, count) = hours.entry(*hour).or_default();
            *weighted += room_area * mean;
            *count += 1;
        }
    }
    hours
        .into_iter()
        .filter(|(_, (_, count))| *count == rooms.len())
        .map(|(hour, (weighted, _))| (hour, weighted / area))
        .collect()
}

/// One month's degree-hours.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MonthHours {
    pub month: Month,
    /// Hours in the month: 744 in January, an hour less or more when the clocks change.
    pub hours: u32,
    /// Of those, the hours both the house's temperature and the outside's are known.
    pub counted_hours: u32,
    /// The house's temperature less the outside's, summed over the counted hours, in K·h.
    pub degree_hours: f64,
    /// The house's temperature summed over the counted hours.
    pub house_sum: f64,
}

impl MonthHours {
    /// Share of the month's hours counted, 0 to 1.
    pub fn coverage(&self) -> f64 {
        f64::from(self.counted_hours) / f64::from(self.hours.max(1))
    }

    /// Degree-hours over the whole month: the counted ones, scaled up by the hours that
    /// went uncounted.
    pub fn whole_degree_hours(&self) -> f64 {
        if self.counted_hours == 0 {
            return 0.0;
        }
        self.degree_hours * f64::from(self.hours) / f64::from(self.counted_hours)
    }

    /// The house's mean temperature over the counted hours.
    pub fn house_mean(&self) -> Option<f64> {
        (self.counted_hours > 0).then(|| self.house_sum / f64::from(self.counted_hours))
    }
}

/// Every month's degree-hours: the house's temperature less the outside's, hour by hour,
/// over the hours both are known. An hour belongs to the local month its middle falls in.
pub fn month_hours(house: &[HourMean], outside: &[HourMean], tz: Tz) -> Vec<MonthHours> {
    let mut months: BTreeMap<Month, (u32, f64, f64)> = BTreeMap::new();
    let mut outside = outside.iter().peekable();
    for (hour, inside) in house {
        while outside.next_if(|(at, _)| at < hour).is_some() {}
        let Some((_, out)) = outside.next_if(|(at, _)| at == hour) else {
            continue;
        };
        let Some(date) = day_to_date(local_day(hour + HOUR / 2, tz)) else {
            continue;
        };
        let (counted, degree_hours, house_sum) = months.entry(Month::of(date)).or_default();
        *counted += 1;
        *degree_hours += inside - out;
        *house_sum += inside;
    }
    months
        .into_iter()
        .map(
            |(month, (counted_hours, degree_hours, house_sum))| MonthHours {
                month,
                hours: hours_in(month, tz),
                counted_hours,
                degree_hours,
                house_sum,
            },
        )
        .collect()
}

/// How many hours a local month has.
fn hours_in(month: Month, tz: Tz) -> u32 {
    match (month.start(tz), month.next().start(tz)) {
        (Some(start), Some(end)) if end > start => {
            u32::try_from((end - start) / HOUR).unwrap_or(month.days() * 24)
        }
        _ => month.days() * 24,
    }
}

/// Why a month is not in the heat loss coefficient.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotCounted {
    /// Its energy or its weather is not sound, so it is left out of the line too.
    Energy,
    /// Warmer on average outside than the heating base: the sun and the household cover
    /// much of what the building loses, and the heating is off or nearly so.
    Mild,
    /// The house's temperature and the outside's are both known for too few of its hours.
    FewHours,
}

/// One month's heating energy beside its degree-hours.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MonthLoss {
    pub month: Month,
    /// Heating energy, the water heating taken off, in kWh.
    pub heating_kwh: f64,
    pub hours: Option<MonthHours>,
    pub not_counted: Option<NotCounted>,
}

/// The heat loss coefficient over some months: the line through their heating energy
/// against their degree-hours.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Loss {
    /// Heating kWh against degree-hours: its slope, in kWh per K·h, is what the building
    /// loses for every kelvin, every hour, and where it meets no degree-hours, below
    /// nothing, the heat the sun and the household give for free.
    pub fit: Fit,
    /// The house's mean temperature over the months' counted hours, in °C.
    pub house: f64,
    /// The months' hours.
    pub hours: u32,
}

impl Loss {
    /// The heat loss coefficient, in W/K.
    pub fn coefficient(&self) -> f64 {
        self.fit.slope * 1_000.0
    }

    pub fn months(&self) -> usize {
        self.fit.months
    }

    /// What the sun and the household gave on average over the months, in W, if the line
    /// says they gave more than a few watts.
    pub fn free_heat(&self) -> Option<f64> {
        let hours_a_month = f64::from(self.hours) / self.fit.months as f64;
        let watts = -self.fit.base * 1_000.0 / hours_a_month;
        (watts >= 10.0).then_some(watts)
    }

    /// What keeping the house a degree warmer costs over `hours`, in kWh.
    pub fn cost(&self, hours: u32) -> f64 {
        self.fit.slope * f64::from(hours)
    }
}

/// What changed from one season to the next, and what it came from.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Change {
    /// The season before's, to say what it is against.
    pub before: i32,
    /// kWh per degree day against the season before, as a share: -0.18 for 18% less.
    pub per_degree_day: f64,
    /// The heat loss coefficient against the season before, as a share.
    pub heat_loss: f64,
    /// The house's temperature against the season before, in K.
    pub house: f64,
    pub verdict: Verdict,
}

/// What a change in kWh per degree day came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// kWh per degree day hardly moved.
    Held,
    /// The heat loss coefficient moved with it: the building changed.
    Building,
    /// The heat loss coefficient held while it moved: the house was kept warmer or cooler.
    Behaviour,
    /// Some of each.
    Both,
}

impl Verdict {
    pub fn text(self) -> &'static str {
        match self {
            Verdict::Held => "kWh per degree day held",
            Verdict::Building => "mostly the building",
            Verdict::Behaviour => "mostly how warm the house was kept",
            Verdict::Both => "the building and how warm the house was kept alike",
        }
    }
}

/// One heating season's heat loss coefficient.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SeasonLoss {
    pub start_year: i32,
    /// From its own heating months, if they carry a line.
    pub loss: Option<Loss>,
    /// Against the latest season before it with both a line and a heat loss coefficient.
    pub change: Option<Change>,
    /// Its heating months, and their hours.
    pub heating_months: usize,
    pub heating_hours: u32,
    /// Its heating energy, water heating taken off, over the months whose energy is sound.
    pub heating_kwh: f64,
}

impl SeasonLoss {
    /// What a degree warmer costs over its heating months by `loss`, and that as a share of
    /// its heating energy.
    pub fn cost(&self, loss: &Loss) -> (f64, Option<f64>) {
        let kwh = loss.cost(self.heating_hours);
        (
            kwh,
            (self.heating_kwh > 0.0).then(|| kwh / self.heating_kwh),
        )
    }
}

/// Everything the report and the terminal say about heat loss.
#[derive(Debug, Clone, PartialEq)]
pub struct HeatLoss {
    /// Floor area the house's temperature is weighted by, in m².
    pub area: f64,
    /// Rooms that give an area.
    pub rooms: usize,
    /// Rooms that give none, left out of the house's temperature, as `floor / room`.
    pub without_area: Vec<String>,
    /// Every month of the heating energy, with its degree-hours where it has them.
    pub months: Vec<MonthLoss>,
    /// From every heating month.
    pub overall: Option<Loss>,
    /// Every season's heating energy, water heating taken off, together.
    pub heating_kwh: f64,
    /// Every heating season of the heating energy, in order.
    pub seasons: Vec<SeasonLoss>,
}

impl HeatLoss {
    /// The months that went into the coefficient.
    pub fn counted(&self) -> impl Iterator<Item = &MonthLoss> {
        self.months
            .iter()
            .filter(|month| month.not_counted.is_none())
    }

    /// What a degree warmer costs over every heating month, and that as a share of every
    /// season's heating energy.
    pub fn cost(&self) -> Option<(f64, Option<f64>)> {
        let overall = self.overall?;
        let kwh = overall.cost(overall.hours);
        Some((
            kwh,
            (self.heating_kwh > 0.0).then(|| kwh / self.heating_kwh),
        ))
    }

    /// The season with the most heating months, the latest of those that tie: the one a
    /// season's cost is told by.
    pub fn fullest_season(&self) -> Option<&SeasonLoss> {
        self.seasons
            .iter()
            .filter(|season| season.heating_months > 0)
            .max_by_key(|season| (season.heating_months, season.start_year))
    }
}

/// The floor the house's temperature stands on.
#[derive(Debug, Clone, PartialEq)]
pub struct Floorspace {
    pub area: f64,
    pub rooms: usize,
    pub without_area: Vec<String>,
}

/// Put every month's heating energy beside its degree-hours, and fit the line through the
/// months the heating ran in: those whose energy and weather are sound, colder on average
/// than `heating_base` and with the house's temperature known for most of their hours. A
/// season needs three such months, milder and colder ones, for a line of its own.
pub fn analyse(
    energy: &Energy,
    hours: &[MonthHours],
    heating_base: f64,
    floorspace: Floorspace,
) -> HeatLoss {
    let months: Vec<MonthLoss> = energy
        .months
        .iter()
        .map(|point: &MonthPoint| {
            let month_hours = hours
                .iter()
                .find(|hours| hours.month == point.month)
                .copied();
            let not_counted = if !point.is_fitted() {
                Some(NotCounted::Energy)
            } else if point.outside_mean.is_none_or(|mean| mean >= heating_base) {
                Some(NotCounted::Mild)
            } else if month_hours.is_none_or(|hours| hours.coverage() < MIN_HOUR_COVERAGE) {
                Some(NotCounted::FewHours)
            } else {
                None
            };
            MonthLoss {
                month: point.month,
                heating_kwh: point.heating_kwh(),
                hours: month_hours,
                not_counted,
            }
        })
        .collect();

    let counted = |season: Option<i32>| -> Vec<&MonthLoss> {
        months
            .iter()
            .filter(|month| month.not_counted.is_none())
            .filter(|month| season.is_none_or(|season| month.month.season() == season))
            .collect()
    };
    let overall = loss(&counted(None));

    let mut seasons: Vec<SeasonLoss> = Vec::with_capacity(energy.seasons.len());
    let mut before: Option<(i32, Fit, Loss)> = None;
    for season in &energy.seasons {
        let heating_months = counted(Some(season.start_year));
        let loss = loss(&heating_months);
        let change = match (season.fit, loss, &before) {
            (Some(fit), Some(loss), Some((year, fit_before, loss_before))) => {
                change((fit_before, loss_before), (&fit, &loss)).map(|mut change| {
                    change.before = *year;
                    change
                })
            }
            _ => None,
        };
        if let (Some(fit), Some(loss)) = (season.fit, loss) {
            before = Some((season.start_year, fit, loss));
        }
        seasons.push(SeasonLoss {
            start_year: season.start_year,
            loss,
            change,
            heating_months: heating_months.len(),
            heating_hours: heating_months
                .iter()
                .filter_map(|month| month.hours)
                .map(|hours| hours.hours)
                .sum(),
            heating_kwh: season.kwh,
        });
    }

    HeatLoss {
        area: floorspace.area,
        rooms: floorspace.rooms,
        without_area: floorspace.without_area,
        months,
        overall,
        heating_kwh: energy.seasons.iter().map(|season| season.kwh).sum(),
        seasons,
    }
}

/// The line through `months`' heating energy against their degree-hours, if there are
/// enough of them, spread out enough, for one that rises.
fn loss(months: &[&MonthLoss]) -> Option<Loss> {
    let points: Vec<(f64, f64)> = months
        .iter()
        .filter_map(|month| Some((month.hours?.whole_degree_hours(), month.heating_kwh)))
        .collect();
    let fit = fit_spread(&points, MIN_DEGREE_HOUR_SPREAD)?;
    let hours = months.iter().filter_map(|month| month.hours);
    let counted: u32 = hours.clone().map(|hours| hours.counted_hours).sum();
    let house_sum: f64 = hours.clone().map(|hours| hours.house_sum).sum();
    (fit.slope > 0.0 && counted > 0).then(|| Loss {
        fit,
        house: house_sum / f64::from(counted),
        hours: hours.map(|hours| hours.hours).sum(),
    })
}

/// What changed from `before` to `after`, each a season's line and heat loss coefficient.
///
/// kWh per degree day moves with both the building and how warm the house is kept; the
/// heat loss coefficient with the building alone. The share of the change in kWh per
/// degree day the coefficient's change makes up, on a log scale so that the two shares
/// add up, says which it was: two thirds or more the building, a third or less how warm
/// the house was kept.
pub fn change(before: (&Fit, &Loss), after: (&Fit, &Loss)) -> Option<Change> {
    let positive = [
        before.0.slope,
        after.0.slope,
        before.1.coefficient(),
        after.1.coefficient(),
    ]
    .iter()
    .all(|value| value.is_finite() && *value > 0.0);
    if !positive {
        return None;
    }
    let per_degree_day = after.0.slope / before.0.slope;
    let heat_loss = after.1.coefficient() / before.1.coefficient();
    let verdict = if (per_degree_day - 1.0).abs() < HELD {
        Verdict::Held
    } else {
        let share = heat_loss.ln() / per_degree_day.ln();
        if share >= 2.0 / 3.0 {
            Verdict::Building
        } else if share <= 1.0 / 3.0 {
            Verdict::Behaviour
        } else {
            Verdict::Both
        }
    };
    Some(Change {
        before: 0,
        per_degree_day: per_degree_day - 1.0,
        heat_loss: heat_loss - 1.0,
        house: after.1.house - before.1.house,
        verdict,
    })
}

/// A heat loss coefficient as it is written: `158 W/K`.
pub fn format_coefficient(watts_per_kelvin: f64) -> String {
    format!("{} W/K", format_whole(watts_per_kelvin))
}

/// A change as a share, signed: `-18%`, `+4%`, `0%`.
pub fn format_share(share: f64) -> String {
    let percent = (share * 100.0).round();
    if percent == 0.0 {
        "0%".to_string()
    } else {
        format!("{}{percent:.0}%", if percent > 0.0 { "+" } else { "" })
    }
}

/// How much warmer or cooler the house was kept: `0.4 K cooler`, `as warm`.
pub fn format_warmer(kelvin: f64) -> String {
    let rounded = (kelvin * 10.0).round() / 10.0;
    if rounded == 0.0 {
        "as warm".to_string()
    } else {
        format!(
            "{:.1} K {}",
            rounded.abs(),
            if rounded > 0.0 { "warmer" } else { "cooler" }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::energy::Season;
    use chrono_tz::Europe::Berlin;
    use chrono_tz::Tz::UTC;
    use ha_testdb::ts;

    #[test]
    fn an_hour_is_the_time_weighted_mean_of_what_covered_it() {
        let samples = [
            // 20 °C for the first quarter of the hour, 22 °C for the rest, then on into the
            // next hour for ten minutes only.
            Sample::new(ts("2024-01-01 10:00:00"), ts("2024-01-01 10:15:00"), 20.0),
            Sample::new(ts("2024-01-01 10:15:00"), ts("2024-01-01 11:10:00"), 22.0),
        ];
        assert_eq!(
            hourly_means(&samples, 0.9),
            [(ts("2024-01-01 10:00:00"), 21.5)],
            "the hour from 11:00 is too thinly covered"
        );
        assert_eq!(hourly_means(&samples, 0.1).len(), 2);
    }

    #[test]
    fn the_house_is_its_rooms_weighted_by_floor_area() {
        let hour = |at: &str| ts(at);
        let living = vec![
            (hour("2024-01-01 10:00:00"), 21.0),
            (hour("2024-01-01 11:00:00"), 21.0),
        ];
        // The bedroom's thermometer missed the second hour.
        let bedroom = vec![(hour("2024-01-01 10:00:00"), 17.0)];
        let house = house_hours(&[(30.0, living), (10.0, bedroom)]);
        assert_eq!(house, [(hour("2024-01-01 10:00:00"), 20.0)]);
        // Two thermometers in a room count alike.
        assert_eq!(
            room_hours(&[vec![(0, 20.0), (HOUR, 20.0)], vec![(0, 22.0)],]),
            [(0, 21.0), (HOUR, 20.0)]
        );
    }

    /// Every hour of `first` to `last` (local dates, UTC) at `temperature`.
    fn steady(first: &str, last: &str, temperature: f64) -> Vec<HourMean> {
        let start = ts(&format!("{first} 00:00:00"));
        let end = ts(&format!("{last} 00:00:00")) + 86_400;
        (0..(end - start) / HOUR)
            .map(|index| (start + index * HOUR, temperature))
            .collect()
    }

    #[test]
    fn a_months_degree_hours_add_up_the_difference_hour_by_hour() {
        let mut house = steady("2024-01-01", "2024-02-29", 20.0);
        // The house's temperature is unknown for the last day of February.
        house.retain(|(hour, _)| *hour < ts("2024-02-29 00:00:00"));
        let outside = steady("2024-01-01", "2024-02-29", 2.0);
        let months = month_hours(&house, &outside, UTC);
        assert_eq!(months.len(), 2);
        assert_eq!(months[0].hours, 744);
        assert_eq!(months[0].counted_hours, 744);
        assert_eq!(months[0].degree_hours, 18.0 * 744.0);
        assert_eq!(months[0].house_mean(), Some(20.0));
        assert_eq!(months[1].hours, 696);
        assert_eq!(months[1].counted_hours, 672);
        assert!((months[1].whole_degree_hours() - 18.0 * 696.0).abs() < 1e-9);
        // Berlin's March is an hour short, the clocks going forward.
        let march = month_hours(
            &steady("2024-03-01", "2024-03-31", 20.0),
            &steady("2024-03-01", "2024-03-31", 5.0),
            Berlin,
        );
        assert_eq!(
            march
                .iter()
                .find(|month| month.month.month == 3)
                .unwrap()
                .hours,
            743
        );
    }

    fn fit(slope: f64) -> Fit {
        Fit {
            slope,
            base: 0.0,
            r_squared: 1.0,
            slope_error: 0.0,
            months: 6,
        }
    }

    fn loss_of(coefficient: f64, house: f64) -> Loss {
        Loss {
            fit: fit(coefficient / 1_000.0),
            house,
            hours: 4 * 744,
        }
    }

    #[test]
    fn a_change_is_put_down_to_the_building_or_to_how_warm_it_was_kept() {
        let verdict = |slope: f64, coefficient: f64| {
            change(
                (&fit(3.0), &loss_of(150.0, 21.0)),
                (&fit(slope), &loss_of(coefficient, 20.0)),
            )
            .unwrap()
            .verdict
        };
        assert_eq!(verdict(2.4, 120.0), Verdict::Building, "both a fifth less");
        assert_eq!(
            verdict(2.7, 149.0),
            Verdict::Behaviour,
            "only the slope moved"
        );
        assert_eq!(verdict(2.4, 135.0), Verdict::Both, "half of it each");
        assert_eq!(verdict(3.05, 140.0), Verdict::Held);
        let change = change(
            (&fit(3.0), &loss_of(150.0, 21.0)),
            (&fit(2.7), &loss_of(150.0, 20.0)),
        )
        .unwrap();
        assert!((change.per_degree_day + 0.1).abs() < 1e-12);
        assert_eq!(change.heat_loss, 0.0);
        assert_eq!(change.house, -1.0);
        assert_eq!(
            super::change(
                (&fit(3.0), &loss_of(150.0, 21.0)),
                (&fit(-1.0), &loss_of(150.0, 21.0))
            ),
            None
        );
    }

    /// A month at `outside` °C outside and `house` °C inside, every hour of it known, heated
    /// by a house losing `coefficient` W/K.
    fn month(
        year: i32,
        number: u32,
        outside: f64,
        house: f64,
        coefficient: f64,
    ) -> (MonthPoint, MonthHours) {
        let month = Month {
            year,
            month: number,
        };
        let hours = month.days() * 24;
        let degree_hours = (house - outside) * f64::from(hours);
        let kwh = (coefficient * degree_hours / 1_000.0).max(0.0);
        (
            MonthPoint {
                month,
                kwh,
                counted_days: month.days(),
                hdd: (15.0 - outside).max(0.0) * f64::from(month.days()),
                outside_mean: Some(outside),
                follows_unchanged: false,
                water_kwh: 0.0,
            },
            MonthHours {
                month,
                hours,
                counted_hours: hours,
                degree_hours,
                house_sum: house * f64::from(hours),
            },
        )
    }

    #[test]
    fn the_coefficient_is_the_slope_through_the_heating_months() {
        // 2023/24 kept at 21 °C, 2024/25 a degree cooler; the building the same.
        let mut pairs = vec![
            month(2023, 9, 16.0, 21.0, 150.0),
            month(2023, 11, 6.0, 21.0, 150.0),
            month(2023, 12, 2.0, 21.0, 150.0),
            month(2024, 1, 0.0, 21.0, 150.0),
            month(2024, 11, 6.0, 20.0, 150.0),
            month(2024, 12, 2.0, 20.0, 150.0),
            month(2025, 1, 0.0, 20.0, 150.0),
            month(2025, 2, 1.0, 20.0, 150.0),
        ];
        // February 2025's house temperature was known for half its hours only.
        pairs[7].1.counted_hours /= 2;
        pairs[7].1.degree_hours /= 2.0;
        pairs[7].1.house_sum /= 2.0;
        let (points, hours): (Vec<MonthPoint>, Vec<MonthHours>) = pairs.into_iter().unzip();
        let season = |start_year: i32, slope: f64| Season {
            start_year,
            months: 4,
            fitted: 4,
            kwh: 0.0,
            water: None,
            fit: Some(fit(slope)),
        };
        let energy = Energy {
            readings: 9,
            months: points,
            overall: Some(fit(2.9)),
            seasons: vec![season(2023, 3.0), season(2024, 2.8)],
            beyond_record: 0,
            water_per_month: None,
        };
        let heat_loss = analyse(
            &energy,
            &hours,
            15.0,
            Floorspace {
                area: 120.0,
                rooms: 5,
                without_area: Vec::new(),
            },
        );
        let not_counted: Vec<Option<NotCounted>> = heat_loss
            .months
            .iter()
            .map(|month| month.not_counted)
            .collect();
        assert_eq!(not_counted[0], Some(NotCounted::Mild));
        assert_eq!(not_counted[7], Some(NotCounted::FewHours));
        assert_eq!(heat_loss.counted().count(), 6);
        let overall = heat_loss.overall.unwrap();
        assert!((overall.coefficient() - 150.0).abs() < 1e-9, "{overall:?}");
        assert_eq!(overall.free_heat(), None, "no free heat in this house");

        let first = heat_loss.seasons[0].loss.unwrap();
        assert_eq!((first.months(), first.house), (3, 21.0));
        let second = heat_loss.seasons[1];
        assert_eq!(second.loss.unwrap().house, 20.0);
        let change = second.change.unwrap();
        assert_eq!(change.before, 2023);
        assert_eq!(change.verdict, Verdict::Behaviour, "{change:?}");
        assert!(change.heat_loss.abs() < 1e-12);
        assert_eq!(change.house, -1.0);
    }

    #[test]
    fn free_heat_lowers_the_heating_but_not_the_coefficient() {
        // A house losing 150 W/K whose heating the sun and household spare 0.6 kW of every
        // hour, kept at 20 °C: its heating over its degree-hours is well under 150 W/K, but
        // every degree-hour more still costs the whole 150 Wh. Months of 31 days alike, so
        // the free heat is the same in every one.
        let pairs: Vec<(MonthPoint, MonthHours)> = [
            (2023, 10, 8.0),
            (2023, 12, 2.0),
            (2024, 1, 0.0),
            (2024, 3, 5.0),
        ]
        .iter()
        .map(|(year, number, outside)| {
            let (mut point, hours) = month(*year, *number, *outside, 20.0, 150.0);
            point.kwh -= 0.6 * f64::from(hours.hours);
            (point, hours)
        })
        .collect();
        let heating: f64 = pairs.iter().map(|(point, _)| point.kwh).sum();
        let degree_hours: f64 = pairs.iter().map(|(_, hours)| hours.degree_hours).sum();
        assert!(
            heating * 1_000.0 / degree_hours < 125.0,
            "the plain ratio reads low"
        );
        let (points, hours): (Vec<MonthPoint>, Vec<MonthHours>) = pairs.into_iter().unzip();
        let energy = Energy {
            readings: 5,
            months: points,
            overall: Some(fit(2.9)),
            seasons: vec![Season {
                start_year: 2023,
                months: 4,
                fitted: 4,
                kwh: heating,
                water: None,
                fit: Some(fit(2.9)),
            }],
            beyond_record: 0,
            water_per_month: None,
        };
        let heat_loss = analyse(
            &energy,
            &hours,
            15.0,
            Floorspace {
                area: 100.0,
                rooms: 4,
                without_area: Vec::new(),
            },
        );
        let overall = heat_loss.overall.unwrap();
        assert!((overall.coefficient() - 150.0).abs() < 1e-6, "{overall:?}");
        assert!((overall.free_heat().unwrap() - 600.0).abs() < 1e-6);
        let hours = 4 * 744;
        assert_eq!(overall.hours, hours);
        let (kwh, share) = heat_loss.cost().unwrap();
        assert!((kwh - 0.15 * f64::from(hours)).abs() < 1e-6);
        assert!((share.unwrap() - kwh / heating).abs() < 1e-12);
        let season = heat_loss.fullest_season().unwrap();
        assert_eq!((season.start_year, season.heating_months), (2023, 4));
        assert_eq!(season.loss, Some(overall));
        assert_eq!(season.cost(&overall), (kwh, share));
    }

    #[test]
    fn figures_are_written_plainly() {
        assert_eq!(format_coefficient(157.6), "158 W/K");
        assert_eq!(format_coefficient(1_234.0), "1,234 W/K");
        assert_eq!(format_share(-0.183), "-18%");
        assert_eq!(format_share(0.04), "+4%");
        assert_eq!(format_share(-0.001), "0%");
        assert_eq!(format_warmer(-0.36), "0.4 K cooler");
        assert_eq!(format_warmer(0.04), "as warm");
        assert_eq!(format_warmer(1.0), "1.0 K warmer");
    }
}
