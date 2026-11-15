//! Heating energy against degree days: what one more degree day costs the house.
//!
//! Plot every month's heating energy against its heating degree days and the months fall
//! along a straight line. Its slope, in kWh per degree day, is the house's efficiency
//! figure: how much heat each degree below the base costs for a day. The weather only
//! moves a month along the line, so insulation, new windows or a boiler tune-up show as
//! a lower slope however cold the winter was. Where the line meets zero degree days is
//! what the house uses without heating at all - hot water, standby losses.

use std::collections::BTreeMap;

use chrono::{Datelike, NaiveDate};
use chrono_tz::Tz;

use ha_recorder::timeutil::{day_to_date, local_day};

use crate::daily::DayMean;
use crate::degree::Bases;

/// Share of its days a month's degree days must cover for the month to be fitted. The
/// energy covers the whole month, so a month missing a day or two has its degree days
/// scaled up to all of it; one missing more is left out rather than guessed at.
pub const MIN_MONTH_COVERAGE: f64 = 0.9;

/// Fewest months a line is fitted through.
pub const MIN_FIT_MONTHS: usize = 3;

/// Smallest spread of degree days, from the mildest month to the coldest, that a line can
/// be fitted along: months that all asked for the same heat say nothing about its cost.
const MIN_SPREAD: f64 = 1.0;

/// A calendar month.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Month {
    pub year: i32,
    /// 1 to 12.
    pub month: u32,
}

impl Month {
    pub fn of(date: NaiveDate) -> Self {
        Self {
            year: date.year(),
            month: date.month(),
        }
    }

    fn first_day(self) -> NaiveDate {
        NaiveDate::from_ymd_opt(self.year, self.month, 1).expect("a month has a first day")
    }

    /// How many days it has.
    pub fn days(self) -> u32 {
        let next = if self.month == 12 {
            NaiveDate::from_ymd_opt(self.year + 1, 1, 1)
        } else {
            NaiveDate::from_ymd_opt(self.year, self.month + 1, 1)
        };
        next.map_or(31, |next| (next - self.first_day()).num_days() as u32)
    }

    /// `January 2024`.
    pub fn label(self) -> String {
        self.first_day().format("%B %Y").to_string()
    }

    /// The month after.
    pub fn next(self) -> Self {
        if self.month == 12 {
            Self {
                year: self.year + 1,
                month: 1,
            }
        } else {
            Self {
                year: self.year,
                month: self.month + 1,
            }
        }
    }

    /// The first year of the heating season it belongs to: seasons run from July to June,
    /// so one winter is never split between two of them.
    pub fn season(self) -> i32 {
        if self.month >= 7 {
            self.year
        } else {
            self.year - 1
        }
    }
}

/// One month's heating energy, as the entity gave it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Reading {
    pub month: Month,
    pub kwh: f64,
    /// When the value was set.
    pub set_at: i64,
}

/// The month every value in an entity's history is for, from when it was set.
///
/// A value is set on the last day of its month, so it counts for the month the day
/// before it falls in: one set just after midnight on the 1st still counts for the month
/// that has just ended. A value repeated unchanged - Home Assistant writes the current
/// one again at every restart - is the same report, not a new one; a month given two
/// different values keeps the later, as a correction. `scale` turns the entity's unit into
/// kWh.
pub fn monthly_values(history: &[(i64, f64)], scale: f64, tz: Tz) -> Vec<Reading> {
    let mut reports: Vec<(i64, f64)> = Vec::with_capacity(history.len());
    for &(ts, value) in history {
        if reports.last().is_none_or(|(_, last)| *last != value) {
            reports.push((ts, value));
        }
    }
    let mut months: BTreeMap<Month, Reading> = BTreeMap::new();
    for (ts, value) in reports {
        let Some(date) = day_to_date(local_day(ts - 86_400, tz)) else {
            continue;
        };
        let month = Month::of(date);
        months.insert(
            month,
            Reading {
                month,
                kwh: value * scale,
                set_at: ts,
            },
        );
    }
    months.into_values().collect()
}

/// One month with both its energy and its degree days.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MonthPoint {
    pub month: Month,
    pub kwh: f64,
    /// Days the outside thermometer covered well enough to count.
    pub counted_days: u32,
    /// Heating degree days over the counted days.
    pub hdd: f64,
}

impl MonthPoint {
    /// Whether enough of the month was counted for it to be fitted.
    pub fn is_fitted(&self) -> bool {
        f64::from(self.counted_days) >= MIN_MONTH_COVERAGE * f64::from(self.month.days())
    }

    /// The month's heating degree days over all its days: the counted ones, scaled up by
    /// the days that went uncounted.
    pub fn whole_month_hdd(&self) -> f64 {
        if self.counted_days == 0 {
            return 0.0;
        }
        self.hdd * f64::from(self.month.days()) / f64::from(self.counted_days)
    }
}

/// A straight line through months: `kWh = slope × heating degree days + base`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Fit {
    /// kWh per heating degree day: the efficiency figure.
    pub slope: f64,
    /// kWh a month with no heating degree days at all would use.
    pub base: f64,
    /// Share of the months' spread in energy the line accounts for, 0 to 1.
    pub r_squared: f64,
    /// Standard error of the slope: how far it could move with other months like these.
    pub slope_error: f64,
    pub months: usize,
}

/// The least-squares line through `(heating degree days, kWh)` points, if there are
/// enough of them spread out enough to carry one.
pub fn fit(points: &[(f64, f64)]) -> Option<Fit> {
    if points.len() < MIN_FIT_MONTHS {
        return None;
    }
    let n = points.len() as f64;
    let (lowest, highest) = points
        .iter()
        .fold((f64::INFINITY, f64::NEG_INFINITY), |(low, high), (x, _)| {
            (low.min(*x), high.max(*x))
        });
    if highest - lowest < MIN_SPREAD {
        return None;
    }
    let mean_x = points.iter().map(|(x, _)| x).sum::<f64>() / n;
    let mean_y = points.iter().map(|(_, y)| y).sum::<f64>() / n;
    let sxx: f64 = points.iter().map(|(x, _)| (x - mean_x).powi(2)).sum();
    let sxy: f64 = points
        .iter()
        .map(|(x, y)| (x - mean_x) * (y - mean_y))
        .sum();
    let slope = sxy / sxx;
    let base = mean_y - slope * mean_x;
    let residual: f64 = points
        .iter()
        .map(|(x, y)| (y - (base + slope * x)).powi(2))
        .sum();
    let total: f64 = points.iter().map(|(_, y)| (y - mean_y).powi(2)).sum();
    Some(Fit {
        slope,
        base,
        r_squared: if total > 0.0 {
            (1.0 - residual / total).max(0.0)
        } else {
            1.0
        },
        slope_error: (residual / (n - 2.0) / sxx).sqrt(),
        months: points.len(),
    })
}

/// One heating season, July to June.
#[derive(Debug, Clone, PartialEq)]
pub struct Season {
    pub start_year: i32,
    /// Months with an energy value.
    pub months: usize,
    /// Of those, the months fitted.
    pub fitted: usize,
    pub kwh: f64,
    pub fit: Option<Fit>,
}

impl Season {
    /// `2023/24`.
    pub fn label(&self) -> String {
        format!(
            "{}/{:02}",
            self.start_year,
            (self.start_year + 1).rem_euclid(100)
        )
    }
}

/// Everything the report and the terminal say about heating energy.
#[derive(Debug, Clone, PartialEq)]
pub struct Energy {
    /// Every month with an energy value inside the outside thermometer's record, in order.
    pub months: Vec<MonthPoint>,
    /// The line through every fitted month.
    pub overall: Option<Fit>,
    /// The same per heating season, in order.
    pub seasons: Vec<Season>,
    /// Values for months the outside thermometer's record does not reach.
    pub beyond_record: usize,
    /// Months between the first value and the last with none of their own, inside the
    /// outside thermometer's record. Setting a helper to the value it already has leaves
    /// no new row in the database, so such a month may have used exactly what the month
    /// before did - or have been skipped. Either way it is not guessed at.
    pub unrecorded: Vec<Month>,
}

impl Energy {
    /// The months that went into the fit.
    pub fn fitted(&self) -> impl Iterator<Item = &MonthPoint> {
        self.months.iter().filter(|month| month.is_fitted())
    }
}

/// Put every month's energy beside its heating degree days, and fit the lines.
///
/// Degree days come from the outside thermometer's days that covered `min_coverage` of
/// the day, as everywhere else in the report.
pub fn analyse(
    readings: &[Reading],
    outside: &[DayMean],
    bases: Bases,
    min_coverage: f64,
) -> Energy {
    let mut weather: BTreeMap<Month, (u32, f64)> = BTreeMap::new();
    for day in outside.iter().filter(|day| day.coverage() >= min_coverage) {
        if let Some(date) = day.date() {
            let (count, hdd) = weather.entry(Month::of(date)).or_default();
            *count += 1;
            *hdd += bases.hdd(day.mean);
        }
    }
    let span = outside
        .first()
        .and_then(DayMean::date)
        .zip(outside.last().and_then(DayMean::date))
        .map(|(first, last)| (Month::of(first), Month::of(last)));

    let mut months = Vec::with_capacity(readings.len());
    let mut beyond_record = 0;
    for reading in readings {
        if span.is_none_or(|(first, last)| reading.month < first || reading.month > last) {
            beyond_record += 1;
            continue;
        }
        let (counted_days, hdd) = weather.get(&reading.month).copied().unwrap_or_default();
        months.push(MonthPoint {
            month: reading.month,
            kwh: reading.kwh,
            counted_days,
            hdd,
        });
    }

    let mut unrecorded = Vec::new();
    if let (Some(first), Some(last), Some((record_first, record_last))) =
        (months.first(), months.last(), span)
    {
        let mut month = first.month;
        let last = last.month;
        while month < last {
            if month >= record_first
                && month <= record_last
                && !months.iter().any(|point| point.month == month)
            {
                unrecorded.push(month);
            }
            month = month.next();
        }
    }

    let points = |months: &mut dyn Iterator<Item = &MonthPoint>| -> Vec<(f64, f64)> {
        months
            .filter(|month| month.is_fitted())
            .map(|month| (month.whole_month_hdd(), month.kwh))
            .collect()
    };
    let overall = fit(&points(&mut months.iter()));

    let mut seasons: Vec<Season> = Vec::new();
    for start_year in months.iter().map(|month| month.month.season()) {
        if seasons
            .last()
            .is_some_and(|season| season.start_year == start_year)
        {
            continue;
        }
        let of_season = || {
            months
                .iter()
                .filter(move |month| month.month.season() == start_year)
        };
        let fitted = points(&mut of_season());
        seasons.push(Season {
            start_year,
            months: of_season().count(),
            fitted: fitted.len(),
            kwh: of_season().map(|month| month.kwh).sum(),
            fit: fit(&fitted),
        });
    }

    Energy {
        months,
        overall,
        seasons,
        beyond_record,
        unrecorded,
    }
}

/// A slope as it is written: `2.31`, `12.4`, `140`.
pub fn format_slope(kwh_per_degree_day: f64) -> String {
    match kwh_per_degree_day.abs() {
        value if value < 10.0 => format!("{kwh_per_degree_day:.2}"),
        value if value < 100.0 => format!("{kwh_per_degree_day:.1}"),
        _ => format!("{kwh_per_degree_day:.0}"),
    }
}

/// An amount of energy in whole kWh, grouped from a thousand: `1,850 kWh`.
pub fn format_energy(kwh: f64) -> String {
    if !kwh.is_finite() {
        return "-".to_string();
    }
    let digits = format!("{:.0}", kwh.abs());
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    let sign = if kwh < 0.0 && digits != "0" { "-" } else { "" };
    format!("{sign}{grouped} kWh")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono_tz::Europe::Berlin;
    use ha_testdb::ts;

    fn month(year: i32, month: u32) -> Month {
        Month { year, month }
    }

    #[test]
    fn months_know_their_length_label_and_season() {
        assert_eq!(month(2024, 2).days(), 29);
        assert_eq!(month(2023, 2).days(), 28);
        assert_eq!(month(2024, 12).days(), 31);
        assert_eq!(month(2024, 1).label(), "January 2024");
        assert_eq!(month(2024, 6).season(), 2023);
        assert_eq!(month(2024, 7).season(), 2024);
        let season = Season {
            start_year: 2099,
            months: 0,
            fitted: 0,
            kwh: 0.0,
            fit: None,
        };
        assert_eq!(season.label(), "2099/00");
    }

    #[test]
    fn a_value_counts_for_the_month_it_was_set_at_the_end_of() {
        let history = [
            (ts("2024-01-31 20:00:00"), 1_850.0),
            // A restart writes the same value again: not February's.
            (ts("2024-02-10 07:12:00"), 1_850.0),
            // Set by an automation just after midnight: still February's.
            (ts("2024-02-29 23:00:30"), 1_600.0),
            // Corrected the next morning.
            (ts("2024-03-01 08:00:00"), 1_620.0),
            (ts("2024-03-31 19:00:00"), 1_200.0),
        ];
        let readings = monthly_values(&history, 1.0, Berlin);
        let found: Vec<(Month, f64)> = readings.iter().map(|r| (r.month, r.kwh)).collect();
        assert_eq!(
            found,
            [
                (month(2024, 1), 1_850.0),
                (month(2024, 2), 1_620.0),
                (month(2024, 3), 1_200.0)
            ]
        );
        assert_eq!(
            monthly_values(&history[..1], 1_000.0, Berlin)[0].kwh,
            1_850_000.0,
            "MWh into kWh"
        );
    }

    #[test]
    fn a_line_through_exact_points_is_exact() {
        let points: Vec<(f64, f64)> = [0.0, 120.0, 300.0, 450.0]
            .iter()
            .map(|hdd| (*hdd, 2.5 * hdd + 400.0))
            .collect();
        let fit = fit(&points).unwrap();
        assert!((fit.slope - 2.5).abs() < 1e-12);
        assert!((fit.base - 400.0).abs() < 1e-9);
        assert!((fit.r_squared - 1.0).abs() < 1e-12);
        assert!(fit.slope_error < 1e-9);
        assert_eq!(fit.months, 4);
    }

    #[test]
    fn a_noisy_line_has_an_error_and_less_than_a_perfect_fit() {
        // y = 2x + 1 with residuals +1, -1, -1, +1.
        let points = [(0.0, 2.0), (1.0, 2.0), (2.0, 4.0), (3.0, 8.0)];
        let fit = fit(&points).unwrap();
        // Worked by hand: slope 2, base 1, residuals sum to 4 over sxx = 5.
        assert!((fit.slope - 2.0).abs() < 1e-12);
        assert!((fit.base - 1.0).abs() < 1e-12);
        assert!((fit.slope_error - (4.0f64 / 2.0 / 5.0).sqrt()).abs() < 1e-12);
        assert!((fit.r_squared - (1.0 - 4.0 / 24.0)).abs() < 1e-12);
    }

    #[test]
    fn too_few_or_too_alike_months_carry_no_line() {
        assert_eq!(fit(&[(100.0, 300.0), (200.0, 500.0)]), None);
        assert_eq!(
            fit(&[(0.0, 300.0), (0.0, 310.0), (0.5, 290.0)]),
            None,
            "summer months alone say nothing about heating"
        );
    }

    /// Every day from `first` to `last` at `mean` °C, fully covered.
    fn days(first: &str, last: &str, mean: f64) -> Vec<DayMean> {
        let first = NaiveDate::parse_from_str(first, "%Y-%m-%d").unwrap();
        let last = NaiveDate::parse_from_str(last, "%Y-%m-%d").unwrap();
        first
            .iter_days()
            .take_while(|date| *date <= last)
            .map(|date| DayMean {
                day: date.num_days_from_ce(),
                mean,
                covered_seconds: 86_400.0,
                day_seconds: 86_400.0,
            })
            .collect()
    }

    fn reading(year: i32, number: u32, kwh: f64) -> Reading {
        Reading {
            month: month(year, number),
            kwh,
            set_at: 0,
        }
    }

    #[test]
    fn months_are_fitted_from_their_whole_months_degree_days() {
        // Ten degrees below the 15 °C base every day from November to January, then mild.
        let mut outside = days("2023-11-01", "2024-01-31", 5.0);
        outside.extend(days("2024-02-01", "2024-02-29", 15.0));
        // Three days of December went unrecorded.
        outside.retain(|day| {
            day.date()
                .is_none_or(|date| !(date.month() == 12 && date.day() > 28))
        });
        let readings = [
            reading(2023, 10, 999.0), // before the outside record
            reading(2023, 11, 300.0 * 2.0 + 300.0),
            reading(2023, 12, 310.0 * 2.0 + 300.0),
            reading(2024, 1, 310.0 * 2.0 + 300.0),
            reading(2024, 2, 300.0),
        ];
        let energy = analyse(&readings, &outside, Bases::default(), 0.9);
        assert_eq!(energy.beyond_record, 1);
        assert_eq!(energy.months.len(), 4);
        let december = energy.months[1];
        assert_eq!(december.counted_days, 28);
        assert_eq!(december.hdd, 280.0);
        assert!(december.is_fitted(), "28 of 31 days is enough");
        assert!((december.whole_month_hdd() - 310.0).abs() < 1e-9);

        let overall = energy.overall.unwrap();
        assert!((overall.slope - 2.0).abs() < 1e-9, "{overall:?}");
        assert!((overall.base - 300.0).abs() < 1e-6);
        assert_eq!(energy.seasons.len(), 1);
        assert_eq!(energy.seasons[0].label(), "2023/24");
        assert_eq!(energy.seasons[0].fitted, 4);
    }

    #[test]
    fn a_thinly_recorded_month_is_left_out_and_seasons_split_in_july() {
        let mut outside = days("2023-05-01", "2024-10-31", 8.0);
        // Most of June 2024 went unrecorded.
        outside.retain(|day| {
            day.date()
                .is_none_or(|date| !(date.year() == 2024 && date.month() == 6 && date.day() > 5))
        });
        let readings: Vec<Reading> = [
            (2023, 5),
            (2023, 6),
            (2023, 7),
            (2024, 5),
            (2024, 6),
            (2024, 7),
        ]
        .iter()
        .map(|(year, number)| reading(*year, *number, 1_000.0))
        .collect();
        let energy = analyse(&readings, &outside, Bases::default(), 0.9);
        let june = energy
            .months
            .iter()
            .find(|point| point.month == month(2024, 6))
            .unwrap();
        assert!(!june.is_fitted());
        assert_eq!(energy.fitted().count(), 5);
        let labels: Vec<String> = energy.seasons.iter().map(Season::label).collect();
        assert_eq!(labels, ["2022/23", "2023/24", "2024/25"]);
        assert_eq!(energy.seasons[1].months, 3);
        assert_eq!(energy.seasons[1].fitted, 2);
        assert_eq!(energy.seasons[1].fit, None, "two months carry no line");
    }

    #[test]
    fn a_month_with_no_value_of_its_own_is_named_not_guessed() {
        let outside = days("2024-01-01", "2024-12-31", 10.0);
        let readings = [
            reading(2024, 5, 300.0),
            reading(2024, 6, 300.0),
            // July and August used just as much, and setting the helper to the value it
            // had left no row.
            reading(2024, 9, 450.0),
        ];
        let energy = analyse(&readings, &outside, Bases::default(), 0.9);
        assert_eq!(energy.unrecorded, [month(2024, 7), month(2024, 8)]);
        assert_eq!(energy.months.len(), 3);
        assert_eq!(month(2024, 12).next(), month(2025, 1));
    }

    #[test]
    fn amounts_and_slopes_are_written_plainly() {
        assert_eq!(format_energy(1_850.4), "1,850 kWh");
        assert_eq!(format_energy(-12.0), "-12 kWh");
        assert_eq!(format_energy(0.2), "0 kWh");
        assert_eq!(format_slope(2.314), "2.31");
        assert_eq!(format_slope(12.44), "12.4");
        assert_eq!(format_slope(140.2), "140");
    }
}
