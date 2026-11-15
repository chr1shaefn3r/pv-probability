//! Heating energy against degree days: what one more degree day costs the house.
//!
//! The heating's energy comes as a running total in kWh - a meter, read monthly, every two
//! weeks, or whenever - and every stretch between two readings is a period: the energy the
//! total grew by, against the heating degree days of exactly those days. Per day, the
//! periods fall along a straight line. Its slope, in kWh per degree day, is the house's
//! efficiency figure: the weather only moves a period along the line, so insulation, new
//! windows or a boiler tune-up show as a lower slope however cold the winter was. Where the
//! line meets zero degree days is what the house uses a day without heating - hot water,
//! standby losses.

use chrono::{Datelike, NaiveDate, NaiveTime, TimeZone};
use chrono_tz::Tz;

use ha_recorder::timeutil::{DayNumber, day_to_date, local_day};

use crate::daily::DayMean;
use crate::degree::Bases;

/// Share of a period the outside thermometer must have covered for the period to be
/// fitted. The energy covers all of it, so a period missing a day or two has its degree
/// days scaled up to the whole; one missing more is left out rather than guessed at.
pub const MIN_PERIOD_COVERAGE: f64 = 0.9;

/// Fewest periods a line is fitted through.
pub const MIN_FIT_PERIODS: usize = 3;

/// Shortest period, in seconds: readings closer together are taken as one longer period,
/// so a total that changes every hour still gives periods with a week of weather in them.
pub const MIN_PERIOD_SECONDS: i64 = 7 * 86_400;

/// Readings within this many seconds of the first of them are one reading, the last
/// standing: a typo put right, or a value written twice.
const SETTLE_SECONDS: i64 = 2 * 86_400;

/// Smallest spread of degree days a day, from the mildest period to the coldest, that a
/// line can be fitted along: periods that all asked for the same heat say nothing about
/// what it costs.
const MIN_SPREAD: f64 = 0.5;

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

    /// `January 2024`.
    pub fn label(self) -> String {
        NaiveDate::from_ymd_opt(self.year, self.month, 1)
            .map_or_else(String::new, |date| date.format("%B %Y").to_string())
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

/// The readings in a running total's history: every time its value changed.
///
/// A value repeated unchanged - Home Assistant writes the current one again at every
/// restart - is no new reading, and a reading changed again within two days is one
/// reading, the later value standing, so a typo put right does not count twice. `scale`
/// turns the entity's unit into kWh.
pub fn readings(history: &[(i64, f64)], scale: f64) -> Vec<Reading> {
    let mut readings: Vec<Reading> = Vec::new();
    let mut settling_since = i64::MIN;
    for &(at, value) in history {
        let total = value * scale;
        match readings.last_mut() {
            Some(last) if last.total == total => {}
            Some(last) if at - settling_since < SETTLE_SECONDS => *last = Reading { at, total },
            _ => {
                readings.push(Reading { at, total });
                settling_since = at;
            }
        }
    }
    readings
}

/// The stretch between two readings, with the weather over it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Period {
    pub from: i64,
    pub to: i64,
    /// What the total grew by.
    pub kwh: f64,
    /// Seconds of the period on days the outside thermometer covered well enough to count.
    pub covered_seconds: f64,
    /// Heating degree days over those seconds.
    pub hdd: f64,
}

/// Why a period is not in the fit.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LeftOut {
    /// The outside thermometer covered this share of it, too little.
    FewDays(f64),
    /// The total went down over it: a meter reset, or a reading put right too late.
    WentDown,
}

impl Period {
    pub fn days(&self) -> f64 {
        (self.to - self.from) as f64 / 86_400.0
    }

    /// Share of the period the outside thermometer covered, 0 to 1.
    pub fn coverage(&self) -> f64 {
        (self.covered_seconds / (self.to - self.from) as f64).clamp(0.0, 1.0)
    }

    /// Why it is not fitted, if it is not.
    pub fn left_out(&self) -> Option<LeftOut> {
        if self.kwh < 0.0 {
            Some(LeftOut::WentDown)
        } else if self.coverage() < MIN_PERIOD_COVERAGE {
            Some(LeftOut::FewDays(self.coverage()))
        } else {
            None
        }
    }

    pub fn is_fitted(&self) -> bool {
        self.left_out().is_none()
    }

    /// Heating degree days over the whole period: those counted, scaled up by the share
    /// that went uncounted.
    pub fn whole_hdd(&self) -> f64 {
        if self.covered_seconds <= 0.0 {
            return 0.0;
        }
        self.hdd / self.coverage()
    }

    pub fn hdd_per_day(&self) -> f64 {
        self.whole_hdd() / self.days()
    }

    pub fn kwh_per_day(&self) -> f64 {
        self.kwh / self.days()
    }

    /// The heating season its middle falls in.
    pub fn season(&self, tz: Tz) -> i32 {
        date(self.from + (self.to - self.from) / 2, tz).map_or(0, |date| Month::of(date).season())
    }

    /// `February 2024` for a period from the end of one month to the end of the next, as
    /// monthly readings make them; otherwise its dates, `29 Feb - 14 Mar 2024`.
    pub fn label(&self, tz: Tz) -> String {
        if let (Some(opened), Some(closed)) = (closes(self.from, tz), closes(self.to, tz))
            && opened.next() == closed
        {
            return closed.label();
        }
        let (Some(from), Some(to)) = (date(self.from, tz), date(self.to, tz)) else {
            return String::new();
        };
        if from.year() == to.year() {
            format!("{} - {}", from.format("%-d %b"), to.format("%-d %b %Y"))
        } else {
            format!("{} - {}", from.format("%-d %b %Y"), to.format("%-d %b %Y"))
        }
    }
}

/// The month a reading taken at `ts` closes, if it closes one: taken on its last day, or
/// on the first of the next.
fn closes(ts: i64, tz: Tz) -> Option<Month> {
    let date = date(ts, tz)?;
    if date.succ_opt().is_some_and(|next| next.day() == 1) {
        Some(Month::of(date))
    } else if date.day() == 1 {
        date.pred_opt().map(Month::of)
    } else {
        None
    }
}

/// The local date an instant falls on.
fn date(ts: i64, tz: Tz) -> Option<NaiveDate> {
    day_to_date(local_day(ts, tz))
}

/// When a local day starts, as a UTC timestamp.
fn day_start(day: DayNumber, tz: Tz) -> Option<i64> {
    let date = day_to_date(day)?;
    tz.from_local_datetime(&date.and_time(NaiveTime::MIN))
        .earliest()
        .map(|instant| instant.timestamp())
}

/// A straight line through periods, per day: `kWh a day = slope × degree days a day +
/// base`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Fit {
    /// kWh per heating degree day: the efficiency figure.
    pub slope: f64,
    /// kWh a day with no heating degree days at all.
    pub base: f64,
    /// Share of the periods' spread the line accounts for, 0 to 1.
    pub r_squared: f64,
    /// Standard error of the slope: how far it could move with other periods like these.
    pub slope_error: f64,
    pub periods: usize,
}

/// The weighted least-squares line through `(degree days a day, kWh a day, days)`
/// points, if there are enough of them spread out enough to carry one. A period counts
/// for as many days as it lasted, so two fortnights weigh what one month does.
pub fn fit(points: &[(f64, f64, f64)]) -> Option<Fit> {
    if points.len() < MIN_FIT_PERIODS {
        return None;
    }
    let (lowest, highest) = points.iter().fold(
        (f64::INFINITY, f64::NEG_INFINITY),
        |(low, high), (x, _, _)| (low.min(*x), high.max(*x)),
    );
    if highest - lowest < MIN_SPREAD {
        return None;
    }
    let weight: f64 = points.iter().map(|(_, _, w)| w).sum();
    let mean_x = points.iter().map(|(x, _, w)| w * x).sum::<f64>() / weight;
    let mean_y = points.iter().map(|(_, y, w)| w * y).sum::<f64>() / weight;
    let sxx: f64 = points
        .iter()
        .map(|(x, _, w)| w * (x - mean_x).powi(2))
        .sum();
    let sxy: f64 = points
        .iter()
        .map(|(x, y, w)| w * (x - mean_x) * (y - mean_y))
        .sum();
    let slope = sxy / sxx;
    let base = mean_y - slope * mean_x;
    let residual: f64 = points
        .iter()
        .map(|(x, y, w)| w * (y - (base + slope * x)).powi(2))
        .sum();
    let total: f64 = points
        .iter()
        .map(|(_, y, w)| w * (y - mean_y).powi(2))
        .sum();
    Some(Fit {
        slope,
        base,
        r_squared: if total > 0.0 {
            (1.0 - residual / total).max(0.0)
        } else {
            1.0
        },
        slope_error: (residual / (points.len() as f64 - 2.0) / sxx).sqrt(),
        periods: points.len(),
    })
}

/// One heating season, July to June.
#[derive(Debug, Clone, PartialEq)]
pub struct Season {
    pub start_year: i32,
    /// Periods whose middle falls in it.
    pub periods: usize,
    /// Of those, the periods fitted.
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
    /// Every period that overlaps the outside thermometer's record, in order.
    pub periods: Vec<Period>,
    /// The line through every fitted period.
    pub overall: Option<Fit>,
    /// The same per heating season, in order.
    pub seasons: Vec<Season>,
    /// Periods entirely outside the outside thermometer's record.
    pub beyond_record: usize,
}

impl Energy {
    /// The periods that went into the fit.
    pub fn fitted(&self) -> impl Iterator<Item = &Period> {
        self.periods.iter().filter(|period| period.is_fitted())
    }
}

/// Cut the readings into periods, put each beside its weather, and fit the lines.
///
/// Readings closer than a week to the start of a period are folded into it. Degree days
/// come from the outside thermometer's days that covered `min_coverage` of the day, as
/// everywhere else in the report; a period starting or ending in the middle of a day
/// takes that day's share.
pub fn analyse(
    readings: &[Reading],
    outside: &[DayMean],
    bases: Bases,
    min_coverage: f64,
    tz: Tz,
) -> Energy {
    // Every counted day as the instants it runs between, and its degree days.
    let days: Vec<(i64, i64, f64)> = outside
        .iter()
        .filter(|day| day.coverage() >= min_coverage)
        .filter_map(|day| {
            let start = day_start(day.day, tz)?;
            let end = day_start(day.day + 1, tz)?;
            (end > start).then(|| (start, end, bases.hdd(day.mean)))
        })
        .collect();
    let record = outside
        .first()
        .and_then(|first| day_start(first.day, tz))
        .zip(outside.last().and_then(|last| day_start(last.day + 1, tz)));

    let mut periods = Vec::new();
    let mut beyond_record = 0;
    let mut start = readings.first().copied();
    for reading in readings.iter().skip(1) {
        let Some(opened) = start else {
            break;
        };
        if reading.at - opened.at < MIN_PERIOD_SECONDS {
            continue;
        }
        start = Some(*reading);
        if record.is_none_or(|(first, last)| reading.at <= first || opened.at >= last) {
            beyond_record += 1;
            continue;
        }
        let (from, to) = (opened.at, reading.at);
        let mut covered_seconds = 0.0;
        let mut hdd = 0.0;
        let first = days.partition_point(|(_, end, _)| *end <= from);
        for (day_start, day_end, day_hdd) in &days[first..] {
            if *day_start >= to {
                break;
            }
            let overlap = (to.min(*day_end) - from.max(*day_start)) as f64;
            covered_seconds += overlap;
            hdd += day_hdd * overlap / (day_end - day_start) as f64;
        }
        periods.push(Period {
            from,
            to,
            kwh: reading.total - opened.total,
            covered_seconds,
            hdd,
        });
    }

    let points = |periods: &mut dyn Iterator<Item = &Period>| -> Vec<(f64, f64, f64)> {
        periods
            .filter(|period| period.is_fitted())
            .map(|period| (period.hdd_per_day(), period.kwh_per_day(), period.days()))
            .collect()
    };
    let overall = fit(&points(&mut periods.iter()));

    let mut seasons: Vec<Season> = Vec::new();
    for start_year in periods.iter().map(|period| period.season(tz)) {
        if seasons.iter().any(|season| season.start_year == start_year) {
            continue;
        }
        let of_season = || {
            periods
                .iter()
                .filter(move |period| period.season(tz) == start_year)
        };
        let fitted = points(&mut of_season());
        seasons.push(Season {
            start_year,
            periods: of_season().count(),
            fitted: fitted.len(),
            kwh: of_season().map(|period| period.kwh).sum(),
            fit: fit(&fitted),
        });
    }

    Energy {
        readings: readings.len(),
        periods,
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

/// Energy a day: `11.2 kWh a day`.
pub fn format_daily(kwh_per_day: f64) -> String {
    if !kwh_per_day.is_finite() {
        return "-".to_string();
    }
    let text = if kwh_per_day.abs() < 100.0 {
        format!("{kwh_per_day:.1}")
    } else {
        format!("{kwh_per_day:.0}")
    };
    let text = if text == "-0.0" { "0.0" } else { &text };
    format!("{text} kWh a day")
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
    fn months_know_their_label_and_season() {
        assert_eq!(month(2024, 1).label(), "January 2024");
        assert_eq!(month(2024, 12).next(), month(2025, 1));
        assert_eq!(month(2024, 6).season(), 2023);
        assert_eq!(month(2024, 7).season(), 2024);
        let season = Season {
            start_year: 2099,
            periods: 0,
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
            // A typo, put right the next morning.
            (ts("2024-03-31 19:00:00"), 131_000.0),
            (ts("2024-04-01 08:00:00"), 13_100.0),
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
                (ts("2024-04-01 08:00:00"), 13_100.0),
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

    /// A day's heating degree days at `mean` °C, against the default 15 °C base.
    fn hdd(mean: f64) -> f64 {
        Bases::default().hdd(mean)
    }

    /// Readings at `times`, the total growing as a house using `slope` kWh per degree
    /// day and `base` kWh a day would make it grow, at a steady `mean` °C outside.
    fn meter(times: &[i64], mean: f64, slope: f64, base: f64) -> Vec<Reading> {
        let mut total = 50_000.0;
        let mut readings = vec![Reading {
            at: times[0],
            total,
        }];
        for pair in times.windows(2) {
            let days = (pair[1] - pair[0]) as f64 / DAY as f64;
            total += days * (slope * hdd(mean) + base);
            readings.push(Reading { at: pair[1], total });
        }
        readings
    }

    #[test]
    fn a_period_takes_the_weather_of_exactly_its_days() {
        let outside = days("2024-01-01", "2024-03-31", 5.0);
        // From 18:00 on 31 January to 06:00 on 29 February: 28.5 days.
        let readings = [
            Reading {
                at: ts("2024-01-31 18:00:00"),
                total: 1_000.0,
            },
            Reading {
                at: ts("2024-02-29 06:00:00"),
                total: 1_570.0,
            },
        ];
        let energy = analyse(&readings, &outside, Bases::default(), 0.9, UTC);
        let period = energy.periods[0];
        assert_eq!(period.days(), 28.5);
        assert_eq!(period.kwh, 570.0);
        assert!((period.hdd - 285.0).abs() < 1e-9, "{}", period.hdd);
        assert_eq!(period.coverage(), 1.0);
        assert_eq!(period.kwh_per_day(), 20.0);
        assert!((period.hdd_per_day() - 10.0).abs() < 1e-12);
    }

    #[test]
    fn a_line_through_exact_periods_is_exact() {
        let points: Vec<(f64, f64, f64)> = [(0.0, 30.0), (4.0, 31.0), (9.5, 29.0), (15.0, 31.0)]
            .iter()
            .map(|(x, days)| (*x, 2.5 * x + 12.0, *days))
            .collect();
        let fit = fit(&points).unwrap();
        assert!((fit.slope - 2.5).abs() < 1e-12);
        assert!((fit.base - 12.0).abs() < 1e-12);
        assert!((fit.r_squared - 1.0).abs() < 1e-12);
        assert!(fit.slope_error < 1e-9);
        assert_eq!(fit.periods, 4);
    }

    #[test]
    fn a_longer_period_weighs_more() {
        // Two short periods on y = x and one long one well above it.
        let short = fit(&[(0.0, 0.0, 1.0), (10.0, 10.0, 1.0), (5.0, 9.0, 1.0)]).unwrap();
        let long = fit(&[(0.0, 0.0, 1.0), (10.0, 10.0, 1.0), (5.0, 9.0, 30.0)]).unwrap();
        assert!(long.base > short.base, "pulled towards the long period");
        // Equal weights are ordinary least squares: worked by hand, base 4/3, slope 1.
        assert!((short.slope - 1.0).abs() < 1e-12);
        assert!((short.base - 4.0 / 3.0).abs() < 1e-12);
    }

    #[test]
    fn too_few_or_too_alike_periods_carry_no_line() {
        assert_eq!(fit(&[(1.0, 30.0, 30.0), (12.0, 50.0, 30.0)]), None);
        assert_eq!(
            fit(&[(0.0, 10.0, 30.0), (0.1, 11.0, 31.0), (0.3, 9.0, 30.0)]),
            None,
            "summer alone says nothing about heating"
        );
    }

    /// The last day of every month from `first` to `last`, at 20:00 UTC.
    fn month_ends(first: Month, last: Month) -> Vec<i64> {
        let mut ends = Vec::new();
        let mut month = first;
        while month <= last {
            let next = month.next();
            let start = NaiveDate::from_ymd_opt(next.year, next.month, 1).unwrap();
            ends.push(ts(&format!("{start} 20:00:00")) - DAY);
            month = next;
        }
        ends
    }

    /// Outside at 0 °C in winter, 5 °C either side and 12 °C in summer, every day of
    /// `first` to `last`.
    fn seasons_outside(first: &str, last: &str) -> Vec<DayMean> {
        let mut outside = days(first, last, 0.0);
        for day in &mut outside {
            day.mean = match day.date().unwrap().month() {
                12 | 1 | 2 => 0.0,
                3 | 4 | 10 | 11 => 5.0,
                _ => 12.0,
            };
        }
        outside
    }

    /// Readings at `times` of a house using 2.5 kWh per degree day over 12 kWh a day,
    /// the total following the weather of `outside` exactly.
    fn house_meter(times: &[i64], outside: &[DayMean]) -> Vec<Reading> {
        // The days run on without a gap, so a day's place is its distance from the first.
        let at_day = |ts: i64| {
            let index = local_day(ts, UTC) - outside[0].day;
            usize::try_from(index)
                .ok()
                .and_then(|index| outside.get(index))
                .map_or(0.0, |day| hdd(day.mean))
        };
        let mut total = 20_000.0;
        let mut readings = vec![Reading {
            at: times[0],
            total,
        }];
        for pair in times.windows(2) {
            // Hour by hour, so a period's energy is exactly its weather's.
            let mut hour = pair[0];
            while hour < pair[1] {
                let step = (pair[1] - hour).min(3_600) as f64 / DAY as f64;
                total += step * (2.5 * at_day(hour) + 12.0);
                hour += 3_600;
            }
            readings.push(Reading { at: pair[1], total });
        }
        readings
    }

    #[test]
    fn monthly_and_fortnightly_readings_tell_the_same_story() {
        let outside = seasons_outside("2023-07-01", "2025-06-30");
        let monthly = month_ends(month(2023, 7), month(2025, 5));
        let fortnightly: Vec<i64> = (0..48).map(|index| monthly[0] + index * 14 * DAY).collect();
        for (times, periods) in [(monthly, 22), (fortnightly, 47)] {
            let energy = analyse(
                &house_meter(&times, &outside),
                &outside,
                Bases::default(),
                0.9,
                UTC,
            );
            assert_eq!(energy.periods.len(), periods);
            let fit = energy.overall.unwrap();
            assert!((fit.slope - 2.5).abs() < 1e-6, "{fit:?}");
            assert!((fit.base - 12.0).abs() < 1e-6, "{fit:?}");
            let labels: Vec<String> = energy.seasons.iter().map(Season::label).collect();
            assert_eq!(labels, ["2023/24", "2024/25"]);
        }
    }

    #[test]
    fn readings_less_than_a_week_apart_fold_into_one_period() {
        let outside = days("2024-01-01", "2024-03-31", 5.0);
        // Every day for two months.
        let times: Vec<i64> = (0..60)
            .map(|index| ts("2024-01-02 00:00:00") + index * DAY)
            .collect();
        let energy = analyse(
            &meter(&times, 5.0, 2.0, 10.0),
            &outside,
            Bases::default(),
            0.9,
            UTC,
        );
        assert_eq!(energy.readings, 60);
        assert_eq!(energy.periods.len(), 8, "weeks, not days");
        assert!(energy.periods.iter().all(|period| period.days() >= 7.0));
        assert!(
            energy
                .periods
                .iter()
                .all(|period| (period.kwh_per_day() - 30.0).abs() < 1e-9)
        );
    }

    #[test]
    fn periods_the_outside_did_not_cover_or_that_went_down_are_left_out() {
        let mut outside = days("2024-01-01", "2024-04-30", 5.0);
        // Most of February went unrecorded.
        outside.retain(|day| {
            day.date()
                .is_none_or(|date| !(date.month() == 2 && date.day() > 5))
        });
        let mut readings = meter(&month_ends(month(2023, 11), month(2024, 4)), 5.0, 2.0, 10.0);
        // March's reading was put right only after April's: the total went down.
        readings[5].total = readings[4].total - 100.0;
        let energy = analyse(&readings, &outside, Bases::default(), 0.9, UTC);
        assert_eq!(energy.beyond_record, 1, "December lies before the record");
        let left: Vec<(String, Option<LeftOut>)> = energy
            .periods
            .iter()
            .map(|period| (period.label(UTC), period.left_out()))
            .collect();
        assert_eq!(left[0].0, "January 2024");
        assert_eq!(left[0].1, None);
        assert!(matches!(left[1].1, Some(LeftOut::FewDays(share)) if share < 0.2));
        assert_eq!(left[2].1, None);
        assert_eq!(left[3].1, Some(LeftOut::WentDown));
    }

    #[test]
    fn periods_are_named_by_their_month_or_their_dates() {
        let period = |from: &str, to: &str| Period {
            from: ts(from),
            to: ts(to),
            kwh: 0.0,
            covered_seconds: 0.0,
            hdd: 0.0,
        };
        assert_eq!(
            period("2024-01-31 20:00:00", "2024-02-29 20:00:00").label(UTC),
            "February 2024"
        );
        assert_eq!(
            period("2024-02-01 00:10:00", "2024-03-01 00:10:00").label(Berlin),
            "February 2024",
            "read just after midnight on the 1st"
        );
        assert_eq!(
            period("2024-02-29 20:00:00", "2024-03-14 20:00:00").label(UTC),
            "29 Feb - 14 Mar 2024"
        );
        assert_eq!(
            period("2023-12-28 12:00:00", "2024-01-11 12:00:00").label(UTC),
            "28 Dec 2023 - 11 Jan 2024"
        );
    }

    #[test]
    fn amounts_and_slopes_are_written_plainly() {
        assert_eq!(format_energy(1_850.4), "1,850 kWh");
        assert_eq!(format_energy(-12.0), "-12 kWh");
        assert_eq!(format_energy(0.2), "0 kWh");
        assert_eq!(format_slope(2.314), "2.31");
        assert_eq!(format_slope(12.44), "12.4");
        assert_eq!(format_slope(140.2), "140");
        assert_eq!(format_daily(11.24), "11.2 kWh a day");
        assert_eq!(format_daily(-0.01), "0.0 kWh a day");
        assert_eq!(format_daily(140.4), "140 kWh a day");
    }
}
