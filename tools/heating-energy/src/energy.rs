//! Heating energy against degree days: what one more degree day costs the house.
//!
//! The heating's energy comes as a running total in kWh - a meter, read at the end of every
//! month or more often - and what it grew by from one month's end to the next is that
//! month's heating energy. Plot every month's energy against its heating degree days and
//! the months fall along a straight line. Its slope, in kWh per degree day, is the house's
//! efficiency figure: the weather only moves a month along the line, so insulation, new
//! windows or a boiler tune-up show as a lower slope however cold the winter was. Where the
//! line meets zero degree days is what the house uses a month without heating - hot water,
//! standby losses.

use std::collections::BTreeMap;

use chrono::{Datelike, NaiveDate, NaiveTime, TimeZone};
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

/// Smallest spread of heating degree days, from the mildest month to the coldest, that a
/// line can be fitted along: months that all asked for the same heat say nothing about
/// what it costs.
const MIN_SPREAD: f64 = 15.0;

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
        let next = self.next();
        let days = (next.first_day() - self.first_day()).num_days();
        u32::try_from(days).unwrap_or(31)
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

/// One reading of the running total.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Reading {
    pub at: i64,
    /// kWh.
    pub total: f64,
}

/// The readings in a running total's history: every time its value changed. A value
/// repeated unchanged - Home Assistant writes the current one again at every restart, and
/// long-term statistics every hour - is no new reading. `scale` turns the entity's unit
/// into kWh.
pub fn readings(history: &[(i64, f64)], scale: f64) -> Vec<Reading> {
    let mut readings: Vec<Reading> = Vec::new();
    for &(at, value) in history {
        let total = value * scale;
        if readings.last().is_none_or(|last| last.total != total) {
            readings.push(Reading { at, total });
        }
    }
    readings
}

/// When a local date starts, as a UTC timestamp.
fn midnight(date: NaiveDate, tz: Tz) -> Option<i64> {
    tz.from_local_datetime(&date.and_time(NaiveTime::MIN))
        .earliest()
        .map(|instant| instant.timestamp())
}

/// The total standing at `at`: the last reading at or before it.
fn total_at(readings: &[Reading], at: i64) -> Option<f64> {
    let after = readings.partition_point(|reading| reading.at <= at);
    after.checked_sub(1).map(|last| readings[last].total)
}

/// The total between the month before `month` and `month` itself: the one standing at the
/// midnight `month` starts. A meter read not on the last day of the month before but on
/// the 1st was read a little late, and a value brought down on the 1st was put right a
/// little late: either way the total at the end of the 1st stands instead.
fn total_at_start(readings: &[Reading], month: Month, tz: Tz) -> Option<f64> {
    let first = month.first_day();
    let start = midnight(first, tz)?;
    let last_day = midnight(first.pred_opt()?, tz)?;
    let after_the_first = total_at(readings, midnight(first.succ_opt()?, tz)?);
    let read_on_the_last_day = readings
        .get(readings.partition_point(|reading| reading.at <= last_day))
        .is_some_and(|reading| reading.at <= start);
    match (total_at(readings, start), after_the_first) {
        // The history starts on the 1st: its first reading opens the month.
        (None, Some(_)) => readings.first().map(|reading| reading.total),
        (Some(at_start), Some(after)) if !read_on_the_last_day || after < at_start => Some(after),
        (at_start, _) => at_start,
    }
}

/// One month with both its energy and its degree days.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MonthPoint {
    pub month: Month,
    /// What the total grew by over the month.
    pub kwh: f64,
    /// Days the outside thermometer covered well enough to count.
    pub counted_days: u32,
    /// Heating degree days over the counted days.
    pub hdd: f64,
    /// Whether the total did not change over the month before: a reading missed there
    /// leaves both months' energy in this one.
    pub follows_unchanged: bool,
}

/// Why a month is not in the fit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeftOut {
    /// The total went down over it: a meter replaced, or a reading put right too late.
    WentDown,
    /// The total did not change over it: the meter was not read, or nothing was used.
    Unchanged,
    /// The total did not change over the month before, so this one may hold both.
    AfterUnchanged,
    /// The outside thermometer counted too few of its days.
    FewDays,
}

impl MonthPoint {
    /// Why it is not fitted, if it is not.
    pub fn left_out(&self) -> Option<LeftOut> {
        if self.kwh < 0.0 {
            Some(LeftOut::WentDown)
        } else if self.kwh == 0.0 {
            Some(LeftOut::Unchanged)
        } else if self.follows_unchanged {
            Some(LeftOut::AfterUnchanged)
        } else if f64::from(self.counted_days) < MIN_MONTH_COVERAGE * f64::from(self.month.days()) {
            Some(LeftOut::FewDays)
        } else {
            None
        }
    }

    pub fn is_fitted(&self) -> bool {
        self.left_out().is_none()
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
    /// Months with an energy figure.
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
    /// How many readings the total's history holds.
    pub readings: usize,
    /// Every whole month the total was read over, inside the outside thermometer's record,
    /// in order.
    pub months: Vec<MonthPoint>,
    /// The line through every fitted month.
    pub overall: Option<Fit>,
    /// The same per heating season, in order.
    pub seasons: Vec<Season>,
    /// Whole months the total was read over that the outside thermometer's record does not
    /// reach.
    pub beyond_record: usize,
}

impl Energy {
    /// The months that went into the fit.
    pub fn fitted(&self) -> impl Iterator<Item = &MonthPoint> {
        self.months.iter().filter(|month| month.is_fitted())
    }
}

/// Take every whole month's energy from the total at its start and its end, put it beside
/// the month's heating degree days, and fit the lines.
///
/// A month counts once the 1st after it is over too, since a reading made then still
/// closes it. Degree days come from the outside thermometer's days that covered
/// `min_coverage` of the day, as everywhere else in the report.
pub fn analyse(
    readings: &[Reading],
    outside: &[DayMean],
    bases: Bases,
    min_coverage: f64,
    tz: Tz,
) -> Energy {
    let mut weather: BTreeMap<Month, (u32, f64)> = BTreeMap::new();
    for day in outside.iter().filter(|day| day.coverage() >= min_coverage) {
        if let Some(date) = day.date() {
            let (count, hdd) = weather.entry(Month::of(date)).or_default();
            *count += 1;
            *hdd += bases.hdd(day.mean);
        }
    }
    let record = outside
        .first()
        .and_then(DayMean::date)
        .zip(outside.last().and_then(DayMean::date));
    // The data runs to the later of the last reading and the end of the outside record:
    // a total that stands unchanged leaves no row of its own while it does.
    let data_end = readings.last().map(|reading| reading.at).max(
        record
            .and_then(|(_, last)| last.succ_opt())
            .and_then(|end| midnight(end, tz)),
    );

    let mut months = Vec::new();
    let mut beyond_record = 0;
    if let (Some(first), Some(data_end)) = (readings.first(), data_end) {
        let mut month = day_to_date(local_day(first.at, tz)).map(Month::of);
        let mut opening = month.and_then(|month| total_at_start(readings, month, tz));
        while let Some(this) = month {
            let next = this.next();
            let Some(closed) = next
                .first_day()
                .succ_opt()
                .and_then(|day| midnight(day, tz))
            else {
                break;
            };
            if closed > data_end {
                break;
            }
            let closing = total_at_start(readings, next, tz);
            if let (Some(opening), Some(closing)) = (opening, closing) {
                if record
                    .is_none_or(|(first, last)| this < Month::of(first) || this > Month::of(last))
                {
                    beyond_record += 1;
                } else {
                    let (counted_days, hdd) = weather.get(&this).copied().unwrap_or_default();
                    let follows_unchanged = months.last().is_some_and(|before: &MonthPoint| {
                        before.month.next() == this && before.kwh == 0.0
                    });
                    months.push(MonthPoint {
                        month: this,
                        kwh: closing - opening,
                        counted_days,
                        hdd,
                        follows_unchanged,
                    });
                }
            }
            opening = closing;
            month = Some(next);
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
        readings: readings.len(),
        months,
        overall,
        seasons,
        beyond_record,
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

/// Energy a month: `372 kWh a month`.
pub fn format_monthly(kwh: f64) -> String {
    format!("{} a month", format_energy(kwh))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono_tz::Europe::Berlin;
    use chrono_tz::Tz::UTC;
    use ha_testdb::ts;

    const DAY: i64 = 86_400;

    fn month(year: i32, month: u32) -> Month {
        Month { year, month }
    }

    #[test]
    fn months_know_their_length_label_and_season() {
        assert_eq!(month(2024, 2).days(), 29);
        assert_eq!(month(2023, 2).days(), 28);
        assert_eq!(month(2024, 12).days(), 31);
        assert_eq!(month(2024, 1).label(), "January 2024");
        assert_eq!(month(2024, 12).next(), month(2025, 1));
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
    fn a_reading_is_a_change_of_the_total() {
        let history = [
            (ts("2024-01-31 20:00:00"), 10_000.0),
            // A restart writes the same total again.
            (ts("2024-02-10 07:12:00"), 10_000.0),
            (ts("2024-02-29 20:00:00"), 11_850.0),
        ];
        let found: Vec<(i64, f64)> = readings(&history, 1.0)
            .iter()
            .map(|reading| (reading.at, reading.total))
            .collect();
        assert_eq!(
            found,
            [
                (ts("2024-01-31 20:00:00"), 10_000.0),
                (ts("2024-02-29 20:00:00"), 11_850.0),
            ]
        );
        assert_eq!(
            readings(&history[..1], 1_000.0)[0].total,
            10_000_000.0,
            "MWh into kWh"
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

    fn readings_of(totals: &[(&str, f64)]) -> Vec<Reading> {
        totals
            .iter()
            .map(|(at, total)| Reading {
                at: ts(at),
                total: *total,
            })
            .collect()
    }

    fn kwh_by_month(energy: &Energy) -> Vec<(String, f64)> {
        energy
            .months
            .iter()
            .map(|point| (point.month.label(), point.kwh))
            .collect()
    }

    #[test]
    fn a_month_is_what_the_total_grew_by_from_its_start_to_its_end() {
        let outside = days("2024-01-01", "2024-05-01", 5.0);
        let readings = readings_of(&[
            ("2024-01-31 20:00:00", 10_000.0),
            // Read mid-month too: it changes nothing.
            ("2024-02-15 20:00:00", 10_900.0),
            ("2024-02-29 20:00:00", 11_850.0),
            ("2024-03-15 20:00:00", 12_500.0),
            ("2024-03-31 20:00:00", 13_100.0),
        ]);
        let energy = analyse(&readings, &outside, Bases::default(), 0.9, UTC);
        assert_eq!(energy.readings, 5);
        assert_eq!(
            kwh_by_month(&energy),
            [
                ("February 2024".to_string(), 1_850.0),
                ("March 2024".to_string(), 1_250.0),
                // April's end lies past the last reading but inside the outside record:
                // the total stood still over it.
                ("April 2024".to_string(), 0.0),
            ]
        );
        assert_eq!(energy.months[2].left_out(), Some(LeftOut::Unchanged));
        assert_eq!(energy.months[0].counted_days, 29);
        assert_eq!(energy.months[0].hdd, 290.0);
    }

    #[test]
    fn a_reading_on_the_first_still_closes_the_month_before() {
        let outside = days("2024-01-01", "2024-05-01", 5.0);
        let readings = readings_of(&[
            // Read just after midnight, and then a day late.
            ("2024-02-01 00:20:00", 10_000.0),
            ("2024-03-01 09:00:00", 11_850.0),
            // Read on the last day, with a typo put right the next morning.
            ("2024-03-31 19:00:00", 131_000.0),
            ("2024-04-01 08:00:00", 13_100.0),
            // Read on the last day, and the next reading on the 1st is new energy.
            ("2024-04-30 19:00:00", 14_000.0),
            ("2024-05-01 10:00:00", 14_020.0),
        ]);
        let energy = analyse(&readings, &outside, Bases::default(), 0.9, Berlin);
        assert_eq!(
            kwh_by_month(&energy),
            [
                ("February 2024".to_string(), 1_850.0),
                ("March 2024".to_string(), 1_250.0),
                ("April 2024".to_string(), 900.0),
            ]
        );
    }

    #[test]
    fn a_meter_counting_all_the_time_closes_its_months_at_midnight() {
        let outside = days("2024-01-01", "2024-04-01", 5.0);
        // One kWh every hour from the 10th of January to the 2nd of April, as long-term
        // statistics give it: the total at each hour's end.
        let start = ts("2024-01-10 00:00:00");
        let history: Vec<(i64, f64)> = (1..=83 * 24)
            .map(|hour| (start + hour * 3_600, hour as f64))
            .collect();
        let energy = analyse(
            &readings(&history, 1.0),
            &outside,
            Bases::default(),
            0.9,
            UTC,
        );
        let hours = |days: f64| days * 24.0;
        assert_eq!(
            kwh_by_month(&energy),
            [
                ("February 2024".to_string(), hours(29.0)),
                ("March 2024".to_string(), hours(31.0)),
            ],
            "January began before the first reading"
        );
    }

    #[test]
    fn a_month_counts_once_the_first_after_it_is_over() {
        let outside = days("2024-01-01", "2024-03-01", 5.0);
        let readings = readings_of(&[
            ("2024-01-31 20:00:00", 10_000.0),
            ("2024-02-29 20:00:00", 11_850.0),
        ]);
        let energy = analyse(&readings, &outside, Bases::default(), 0.9, UTC);
        assert_eq!(
            kwh_by_month(&energy),
            [("February 2024".to_string(), 1_850.0)]
        );
        // Recorded only to the end of February, a reading on the 1st may yet come.
        let outside = days("2024-01-01", "2024-02-29", 5.0);
        let energy = analyse(&readings, &outside, Bases::default(), 0.9, UTC);
        assert!(energy.months.is_empty());
    }

    #[test]
    fn a_month_not_read_is_left_out_with_the_one_holding_it() {
        let outside = days("2024-01-01", "2024-05-01", 5.0);
        let readings = readings_of(&[
            ("2024-01-31 20:00:00", 10_000.0),
            ("2024-02-29 20:00:00", 11_850.0),
            // Nobody read the meter at the end of March.
            ("2024-04-30 20:00:00", 14_350.0),
        ]);
        let energy = analyse(&readings, &outside, Bases::default(), 0.9, UTC);
        let left_out: Vec<(String, Option<LeftOut>)> = energy
            .months
            .iter()
            .map(|point| (point.month.label(), point.left_out()))
            .collect();
        assert_eq!(
            left_out,
            [
                ("February 2024".to_string(), None),
                ("March 2024".to_string(), Some(LeftOut::Unchanged)),
                ("April 2024".to_string(), Some(LeftOut::AfterUnchanged)),
            ]
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
        // y = 2x + 1 with residuals +1, -1, -1, +1, ten times over.
        let points = [(0.0, 20.0), (10.0, 20.0), (20.0, 40.0), (30.0, 80.0)];
        let fit = fit(&points).unwrap();
        // Worked by hand: slope 2, base 10, residuals sum to 400 over sxx = 500.
        assert!((fit.slope - 2.0).abs() < 1e-12);
        assert!((fit.base - 10.0).abs() < 1e-12);
        assert!((fit.slope_error - (400.0f64 / 2.0 / 500.0).sqrt()).abs() < 1e-12);
        assert!((fit.r_squared - (1.0 - 400.0 / 2_400.0)).abs() < 1e-12);
    }

    #[test]
    fn too_few_or_too_alike_months_carry_no_line() {
        assert_eq!(fit(&[(100.0, 300.0), (200.0, 500.0)]), None);
        assert_eq!(
            fit(&[(0.0, 300.0), (4.0, 310.0), (9.0, 290.0)]),
            None,
            "summer months alone say nothing about heating"
        );
    }

    /// The total read at 20:00 on the last day of every month from `first` to `last`, for
    /// a house using `slope` kWh per degree day and `base` kWh a day, at a steady `mean`
    /// °C outside.
    fn month_ends(first: Month, last: Month, mean: f64, slope: f64, base: f64) -> Vec<Reading> {
        let mut readings = Vec::new();
        let mut total = 20_000.0;
        let mut month = first;
        while month <= last {
            let next = month.next();
            readings.push(Reading {
                at: midnight(next.first_day(), UTC).unwrap() - 4 * 3_600,
                total,
            });
            let days = f64::from(next.days());
            total += days * (slope * Bases::default().hdd(mean) + base);
            month = next;
        }
        readings
    }

    #[test]
    fn months_are_fitted_from_their_whole_months_degree_days() {
        // Ten degrees below the 15 °C base every day from November to January, then mild.
        let mut outside = days("2023-11-01", "2024-01-31", 5.0);
        outside.extend(days("2024-02-01", "2024-03-01", 15.0));
        // Three days of December went unrecorded.
        outside.retain(|day| {
            day.date()
                .is_none_or(|date| !(date.month() == 12 && date.day() > 28))
        });
        let readings = readings_of(&[
            ("2023-09-30 20:00:00", 1_000.0),
            // October lies before the outside record.
            ("2023-10-31 20:00:00", 2_000.0),
            ("2023-11-30 20:00:00", 2_000.0 + 300.0 * 2.0 + 300.0),
            ("2023-12-31 20:00:00", 2_900.0 + 310.0 * 2.0 + 300.0),
            ("2024-01-31 20:00:00", 3_820.0 + 310.0 * 2.0 + 300.0),
            ("2024-02-29 20:00:00", 4_740.0 + 300.0),
        ]);
        let energy = analyse(&readings, &outside, Bases::default(), 0.9, UTC);
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
    fn months_the_outside_did_not_cover_or_that_went_down_are_left_out() {
        let mut outside = days("2023-05-01", "2024-10-31", 8.0);
        // Most of June 2024 went unrecorded.
        outside.retain(|day| {
            day.date()
                .is_none_or(|date| !(date.year() == 2024 && date.month() == 6 && date.day() > 5))
        });
        let mut readings = month_ends(month(2023, 4), month(2024, 9), 8.0, 2.0, 10.0);
        // August's reading was put right only after September's: the total went down.
        let august = readings.len() - 2;
        readings[august].total = readings[august - 1].total - 100.0;
        let energy = analyse(&readings, &outside, Bases::default(), 0.9, UTC);
        let left_out = |label: &str| {
            energy
                .months
                .iter()
                .find(|point| point.month.label() == label)
                .unwrap()
                .left_out()
        };
        assert_eq!(left_out("June 2024"), Some(LeftOut::FewDays));
        assert_eq!(left_out("August 2024"), Some(LeftOut::WentDown));
        assert_eq!(left_out("July 2024"), None);
        let labels: Vec<String> = energy.seasons.iter().map(Season::label).collect();
        assert_eq!(labels, ["2022/23", "2023/24", "2024/25"]);
        assert_eq!(energy.seasons[1].months, 12);
        assert_eq!(energy.seasons[1].fitted, 11);
        assert_eq!(energy.seasons[2].months, 3);
    }

    #[test]
    fn monthly_and_fortnightly_readings_give_the_same_months() {
        let outside = days("2024-01-01", "2024-07-31", 5.0);
        let monthly = month_ends(month(2024, 1), month(2024, 6), 5.0, 2.0, 10.0);
        // The same, read on the 15th as well.
        let mut fortnightly = Vec::new();
        for pair in monthly.windows(2) {
            fortnightly.push(pair[0]);
            fortnightly.push(Reading {
                at: pair[0].at + 15 * DAY,
                total: pair[0].total + 15.0 * 30.0,
            });
        }
        fortnightly.push(*monthly.last().unwrap());
        let of = |readings: &[Reading]| {
            kwh_by_month(&analyse(readings, &outside, Bases::default(), 0.9, UTC))
        };
        assert_eq!(of(&monthly), of(&fortnightly));
        assert_eq!(of(&monthly).len(), 5);
    }

    #[test]
    fn amounts_and_slopes_are_written_plainly() {
        assert_eq!(format_energy(1_850.4), "1,850 kWh");
        assert_eq!(format_energy(-12.0), "-12 kWh");
        assert_eq!(format_energy(0.2), "0 kWh");
        assert_eq!(format_monthly(372.2), "372 kWh a month");
        assert_eq!(format_slope(2.314), "2.31");
        assert_eq!(format_slope(12.44), "12.4");
        assert_eq!(format_slope(140.2), "140");
    }
}
