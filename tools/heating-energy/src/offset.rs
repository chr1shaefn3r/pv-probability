//! Taking a thermostat's own offset back out of its readings.
//!
//! A thermostat on a radiator reads warm, so it is often given an offset: set it to -2
//! and it reports two degrees less than it measures, and heats until the far side of the
//! room is warm enough. The reading Home Assistant records then has the offset in it, and
//! cannot be compared with a plain thermometer's. Knowing the offset at every moment, the
//! tool takes it back out again: what remains is what the thermostat measured.

use anyhow::{Context, Result, anyhow};
use chrono_tz::Tz;
use rusqlite::Connection;

use ha_recorder::Sample;
use ha_recorder::source::{LoadedSamples, states};
use ha_recorder::timeutil::{day_to_date, local_day};

use crate::rooms::{Applied, Offset, Room};

/// Words an offset setting's entity id is made of, in the languages Home Assistant names
/// them in.
pub const OFFSET_WORDS: [&str; 5] = [
    "offset",
    "calibration",
    "kalibrierung",
    "versatz",
    "korrektur",
];

/// The values a thermostat's offset took, and from when, earliest first.
///
/// Each value holds from its time until the next. Before the first, the first is assumed:
/// Home Assistant keeps raw states, where a setting lives, for ten days by default, while
/// a thermometer's hourly statistics go back years, and an offset is usually set once,
/// when the thermostat goes in.
#[derive(Debug, Clone, PartialEq)]
pub struct Steps(Vec<(i64, f64)>);

impl Steps {
    /// The same offset at every moment.
    pub fn fixed(degrees: f64) -> Self {
        Self(vec![(i64::MIN, degrees)])
    }

    /// The steps a recorded history of the setting makes, or `None` when it holds no value.
    /// Repeats of the value in force - written at every restart - are not changes.
    pub fn from_history(history: &[(i64, f64)]) -> Option<Self> {
        let mut steps: Vec<(i64, f64)> = Vec::with_capacity(history.len().min(16));
        for &(ts, degrees) in history {
            if steps.last().is_none_or(|(_, last)| *last != degrees) {
                steps.push((ts, degrees));
            }
        }
        (!steps.is_empty()).then_some(Self(steps))
    }

    /// When the first value is on record, or `None` for a fixed offset.
    pub fn recorded_from(&self) -> Option<i64> {
        self.0
            .first()
            .map(|(ts, _)| *ts)
            .filter(|ts| *ts != i64::MIN)
    }

    /// The values, in the order they were set.
    pub fn values(&self) -> impl Iterator<Item = f64> + '_ {
        self.0.iter().map(|(_, degrees)| *degrees)
    }

    /// Every change: when, from what, to what.
    pub fn changes(&self) -> impl Iterator<Item = (i64, f64, f64)> + '_ {
        self.0
            .windows(2)
            .map(|pair| (pair[1].0, pair[0].1, pair[1].1))
    }

    /// The offset in force at `ts`.
    pub fn at(&self, ts: i64) -> f64 {
        let index = self.0.partition_point(|(from, _)| *from <= ts);
        self.0[index.saturating_sub(1)].1
    }
}

/// The samples with the offset taken back out.
///
/// `scale` turns the thermostat's unit into degrees Celsius, as its readings were
/// converted. A sample during which the offset changed is cut where it changed, and each
/// part keeps the offset that held for it: an hour's mean is the mean of what was
/// reported, so taking out the offset's own mean over the hour leaves the mean of what was
/// measured.
pub fn take_out(samples: &[Sample], steps: &Steps, applied: Applied, scale: f64) -> Vec<Sample> {
    let shift = |degrees: f64| applied.sign() * scale * degrees;
    let mut corrected = Vec::with_capacity(samples.len() + steps.0.len());
    for sample in samples {
        let mut start = sample.start_ts;
        let mut index = steps
            .0
            .partition_point(|(from, _)| *from <= start)
            .saturating_sub(1);
        loop {
            let end = match steps.0.get(index + 1) {
                Some((from, _)) if *from < sample.end_ts => *from,
                _ => sample.end_ts,
            };
            corrected.push(Sample::new(
                start,
                end,
                sample.value - shift(steps.0[index].1),
            ));
            if end >= sample.end_ts {
                break;
            }
            start = end;
            index += 1;
        }
    }
    corrected
}

/// Smallest change of the offset worth judging the direction by: a thermostat reports in
/// steps of a tenth to half a degree, and the room drifts too.
const MIN_JUDGED_CHANGE: f64 = 0.5;

/// How long before an offset change the reading taken as "before" may be, in seconds.
const BEFORE_WINDOW: i64 = 1_800;

/// How long after an offset change the thermostat is given to report again, in seconds.
/// It usually does at once; the reading and the setting may even arrive in either order,
/// a few moments apart.
const AFTER_WINDOW: i64 = 300;

/// Readings this close before a change may already carry the new offset.
const GRACE: i64 = 60;

/// What the recorded readings say about how the offset is applied.
#[derive(Debug, Clone, PartialEq)]
pub enum Check {
    /// No change of the offset with readings on both sides of it to judge by.
    Untested,
    /// The reading moved the way `offset_applied` says at every change judged.
    Agrees { changes: usize },
    /// The reading moved the other way: the thermostat applies its offset the other way
    /// round. The first such change, and how far the reading moved.
    Disagrees { change: (i64, f64, f64), moved: f64 },
    /// The reading did not move when the offset changed, as though it were reported
    /// without the offset in it. The first such change.
    Unmoved { change: (i64, f64, f64) },
    /// Some changes one way, some the other.
    Mixed { with: usize, against: usize },
}

/// Judge from the thermostat's raw readings, `(time, value)` in time order, whether it
/// applies its offset the way `applied` says.
///
/// When the offset changes by some amount, the reported reading jumps by the same amount:
/// up for an offset that is added and raised, down for one that is subtracted. The room
/// itself barely moves in the few minutes it takes the thermostat to report.
pub fn check(readings: &[(i64, f64)], steps: &Steps, applied: Applied) -> Check {
    let (mut with, mut against) = (0, 0);
    let (mut first_against, mut first_still) = (None, None);
    for change @ (at, from, to) in steps.changes() {
        let step = to - from;
        if step.abs() < MIN_JUDGED_CHANGE {
            continue;
        }
        let split = readings.partition_point(|(ts, _)| *ts < at - GRACE);
        let Some(&(before_ts, before)) = split.checked_sub(1).map(|index| &readings[index]) else {
            continue;
        };
        if at - before_ts > BEFORE_WINDOW + GRACE {
            continue;
        }
        let moved = readings[split..]
            .iter()
            .take_while(|(ts, _)| *ts <= at + AFTER_WINDOW)
            .map(|(_, value)| value - before)
            .max_by(|left, right| left.abs().total_cmp(&right.abs()));
        let Some(moved) = moved else {
            continue;
        };
        let expected = applied.sign() * step;
        if moved.abs() < step.abs() / 2.0 {
            first_still.get_or_insert(change);
        } else if moved.signum() == expected.signum() {
            with += 1;
        } else {
            against += 1;
            first_against.get_or_insert((change, moved));
        }
    }
    match (with, against, first_against, first_still) {
        (0, 0, _, None) => Check::Untested,
        (0, 0, _, Some(change)) => Check::Unmoved { change },
        (with, 0, _, _) => Check::Agrees { changes: with },
        (0, _, Some((change, moved)), _) => Check::Disagrees { change, moved },
        (with, against, _, _) => Check::Mixed { with, against },
    }
}

/// The candidate that looks like `sensor`'s own offset setting, if exactly one does.
///
/// Entity ids are made from device names, so a thermostat's reading and its offset
/// usually share most of their words - `sensor.bad_og_heizkorper_temperature` and
/// `number.bad_heizkorper_temperature_offset` share all but one. More than half of the
/// two ids' device words have to be shared, and no other candidate may match as well.
pub fn suggest<'a>(sensor: &str, candidates: &'a [String]) -> Option<&'a String> {
    let own = device_words(sensor);
    let mut best: Option<(f64, &String)> = None;
    let mut tied = false;
    for candidate in candidates {
        let words = device_words(candidate);
        let shared = words.iter().filter(|word| own.contains(word)).count();
        let all = own.len() + words.len() - shared;
        if shared == 0 || all == 0 {
            continue;
        }
        let score = shared as f64 / all as f64;
        match best {
            Some((top, _)) if score < top => {}
            Some((top, _)) if score == top => tied = true,
            _ => {
                best = Some((score, candidate));
                tied = false;
            }
        }
    }
    best.filter(|(score, _)| *score > 0.5 && !tied)
        .map(|(_, candidate)| candidate)
}

/// The words of an entity id that name its device rather than what it is.
fn device_words(entity: &str) -> Vec<&str> {
    const GENERIC: [&str; 9] = [
        "temperature",
        "temperatur",
        "local",
        "current",
        "offset",
        "calibration",
        "kalibrierung",
        "versatz",
        "korrektur",
    ];
    let id = entity.split_once('.').map_or(entity, |(_, id)| id);
    id.split('_')
        .filter(|word| !word.is_empty() && !GENERIC.contains(word))
        .collect()
}

/// What became of one room's offset.
#[derive(Debug, Clone, PartialEq)]
pub enum Finding {
    /// It was taken back out of the readings.
    Corrected(Correction),
    /// The rooms file names none, but this entity looks like the thermostat's offset.
    Suggested(String),
}

/// An offset that was taken back out, and what is known about it.
#[derive(Debug, Clone, PartialEq)]
pub struct Correction {
    /// The setting it came from; `None` for a value the rooms file gives.
    pub entity: Option<String>,
    pub steps: Steps,
    pub applied: Applied,
    pub check: Check,
    /// The unit the thermostat records in, which its offset is in too.
    pub unit: String,
    /// When the setting's recorded history starts, if there are readings from before it:
    /// those rest on its earliest value having held then too.
    pub assumed_before: Option<i64>,
}

impl Correction {
    /// The values it took, in order: `-2 °C`, or `-1 °C, then -2 °C`.
    pub fn values(&self) -> String {
        let values: Vec<String> = self
            .steps
            .values()
            .map(|degrees| format_offset(degrees, &self.unit))
            .collect();
        match values.as_slice() {
            [first, .., last] if values.len() > 3 => {
                format!("{first}, then {} more, then {last}", values.len() - 2)
            }
            _ => values.join(", then "),
        }
    }

    /// Where the offset came from and how far back it is known.
    pub fn source(&self, tz: Tz) -> String {
        match (&self.entity, self.assumed_before) {
            (Some(entity), Some(from)) => format!(
                "from {entity} (on record since {}; the first value is assumed before that)",
                local_date(from, tz)
            ),
            (Some(entity), None) => format!("from {entity}"),
            (None, _) => "as the rooms file gives it".to_string(),
        }
    }

    /// What was taken out, and where it came from, as the report says it.
    pub fn sentence(&self, tz: Tz) -> String {
        let source = self.source(tz);
        let separator = if self.entity.is_some() { " " } else { ", " };
        format!("{}{separator}{source}", self.values())
    }
}

/// Do what the rooms file says about `room`'s offset to its loaded readings, and say what
/// was done.
///
/// A named setting is read from the raw states, and its changes are checked against the
/// thermostat's own raw readings. A room that names nothing is only given a suggestion,
/// from the offset settings no other room names: its readings stay as they are, because a
/// name that merely looks right is not enough to change what a room reports.
pub fn resolve(
    conn: &Connection,
    room: &Room,
    loaded: &mut LoadedSamples,
    named_elsewhere: &[&str],
) -> Result<Option<Finding>> {
    let unit = loaded.unit.clone().unwrap_or_else(|| "°C".to_string());
    let (steps, entity, applied) = match &room.offset {
        Offset::Off => return Ok(None),
        Offset::Unset => {
            let candidates: Vec<String> = states::entities_matching(conn, "number", &OFFSET_WORDS)?
                .into_iter()
                .filter(|candidate| !named_elsewhere.contains(&candidate.as_str()))
                .collect();
            return Ok(suggest(&room.sensor, &candidates)
                .cloned()
                .map(Finding::Suggested));
        }
        Offset::Fixed { degrees, applied } => (Steps::fixed(*degrees), None, *applied),
        Offset::Entity { entity, applied } => {
            let history = states::history(conn, entity)
                .with_context(|| format!("reading its thermostat's offset, {entity}"))?;
            let steps = Steps::from_history(&history).ok_or_else(|| {
                anyhow!(
                    "{entity}, its thermostat's offset, has no value on record. Home Assistant \
                     keeps a setting only in its raw states, ten days by default, and only if \
                     the recorder includes it; give the offset as a number instead, e.g. \
                     offset = -2"
                )
            })?;
            (steps, Some(entity.clone()), *applied)
        }
    };

    // Raw readings are what can show which way the offset is applied; a thermostat
    // without any just goes unchecked.
    let check = if steps.changes().next().is_some() {
        states::history(conn, &room.sensor).map_or(Check::Untested, |readings| {
            check(&readings, &steps, applied)
        })
    } else {
        Check::Untested
    };
    let assumed_before = steps
        .recorded_from()
        .filter(|from| loaded.first_ts.is_some_and(|first| first < *from));
    loaded.samples = take_out(&loaded.samples, &steps, applied, loaded.conversion.scale);
    Ok(Some(Finding::Corrected(Correction {
        entity,
        steps,
        applied,
        check,
        unit,
        assumed_before,
    })))
}

/// An offset as a person writes it: `-2 °C`, `+0.5 °C`.
pub fn format_offset(degrees: f64, unit: &str) -> String {
    let text = format!("{degrees:+.2}");
    let text = text.trim_end_matches('0').trim_end_matches('.');
    let text = match text {
        "+0" | "-0" => "0",
        other => other,
    };
    format!("{text} {unit}")
}

/// A local date and time, for saying when an offset changed.
pub fn local_stamp(ts: i64, tz: Tz) -> String {
    chrono::DateTime::from_timestamp(ts, 0)
        .map(|utc| utc.with_timezone(&tz).format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| "?".to_string())
}

/// A local date, for saying from when an offset is on record.
pub fn local_date(ts: i64, tz: Tz) -> String {
    day_to_date(local_day(ts, tz)).map_or_else(|| "?".to_string(), |date| date.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR: i64 = 3_600;

    fn steps(history: &[(i64, f64)]) -> Steps {
        Steps::from_history(history).unwrap()
    }

    #[test]
    fn a_recorded_history_becomes_the_steps_it_changed_in() {
        let steps = steps(&[(100, -1.0), (200, -1.0), (300, -2.0), (400, -2.0)]);
        assert_eq!(
            steps.0,
            [(100, -1.0), (300, -2.0)],
            "restarts repeat a value"
        );
        assert_eq!(steps.recorded_from(), Some(100));
        assert_eq!(steps.changes().collect::<Vec<_>>(), [(300, -1.0, -2.0)]);
        assert_eq!(steps.at(0), -1.0, "the first value is assumed before it");
        assert_eq!(steps.at(299), -1.0);
        assert_eq!(steps.at(300), -2.0);
        assert!(Steps::from_history(&[]).is_none());
        assert_eq!(Steps::fixed(1.5).recorded_from(), None);
        assert_eq!(Steps::fixed(1.5).at(i64::MIN), 1.5);
    }

    #[test]
    fn an_added_offset_is_taken_back_out() {
        // A thermostat set to -2 reports 19 °C where it measures 21.
        let samples = [
            Sample::new(0, HOUR, 19.0),
            Sample::new(HOUR, 2 * HOUR, 19.5),
        ];
        let corrected = take_out(&samples, &Steps::fixed(-2.0), Applied::Added, 1.0);
        assert_eq!(
            corrected,
            [
                Sample::new(0, HOUR, 21.0),
                Sample::new(HOUR, 2 * HOUR, 21.5)
            ]
        );
        // One that subtracts its offset of 2 reported the same 19 °C.
        let corrected = take_out(&samples, &Steps::fixed(2.0), Applied::Subtracted, 1.0);
        assert_eq!(corrected[0].value, 21.0);
    }

    #[test]
    fn an_hour_in_which_the_offset_changed_is_cut_where_it_changed() {
        let samples = [
            Sample::new(0, HOUR, 20.0),
            Sample::new(HOUR, 2 * HOUR, 20.0),
        ];
        let steps = steps(&[(-HOUR, -1.0), (900, -2.0), (HOUR, -3.0)]);
        let corrected = take_out(&samples, &steps, Applied::Added, 1.0);
        assert_eq!(
            corrected,
            [
                Sample::new(0, 900, 21.0),
                Sample::new(900, HOUR, 22.0),
                Sample::new(HOUR, 2 * HOUR, 23.0),
            ]
        );
        // The hour's time-weighted mean takes out the offset's own mean over it.
        let weighted: f64 = corrected[..2]
            .iter()
            .map(|sample| sample.value * sample.duration() as f64)
            .sum();
        assert_eq!(weighted / HOUR as f64, 20.0 + 1.75);
    }

    #[test]
    fn a_fahrenheit_offset_is_taken_out_in_celsius() {
        // Readings converted from °F, so an offset of -3.6 °F is two degrees Celsius.
        let samples = [Sample::new(0, HOUR, 19.0)];
        let corrected = take_out(&samples, &Steps::fixed(-3.6), Applied::Added, 5.0 / 9.0);
        assert!((corrected[0].value - 21.0).abs() < 1e-12);
    }

    /// Readings every five minutes at 21 °C reported with an offset of `before`, which
    /// changes to `after` at `at`; the thermostat reports with `sign` times the offset.
    fn readings(at: i64, before: f64, after: f64, sign: f64) -> Vec<(i64, f64)> {
        (0..48)
            .map(|index| {
                let ts = index * 300 + 13;
                let offset = if ts < at { before } else { after };
                (ts, 21.0 + sign * offset)
            })
            .collect()
    }

    #[test]
    fn the_readings_say_which_way_the_offset_is_applied() {
        let at = 2 * HOUR;
        let steps = steps(&[(0, -1.0), (at, -2.0)]);
        let added = readings(at, -1.0, -2.0, 1.0);
        let subtracted = readings(at, -1.0, -2.0, -1.0);
        assert_eq!(
            check(&added, &steps, Applied::Added),
            Check::Agrees { changes: 1 }
        );
        assert_eq!(
            check(&subtracted, &steps, Applied::Subtracted),
            Check::Agrees { changes: 1 }
        );
        assert_eq!(
            check(&subtracted, &steps, Applied::Added),
            Check::Disagrees {
                change: (at, -1.0, -2.0),
                moved: 1.0
            }
        );
        let unmoved: Vec<(i64, f64)> = (0..48).map(|index| (index * 300, 21.0)).collect();
        assert_eq!(
            check(&unmoved, &steps, Applied::Added),
            Check::Unmoved {
                change: (at, -1.0, -2.0)
            }
        );
    }

    #[test]
    fn a_reading_and_setting_arriving_in_either_order_still_count() {
        // The reading with the new offset is written a moment before the setting is.
        let at = 2 * HOUR;
        let mut readings: Vec<(i64, f64)> = (0..20).map(|index| (index * 600, 20.0)).collect();
        readings.retain(|(ts, _)| (ts - at).abs() > 600);
        readings.push((at - 2, 18.0));
        readings.sort_by_key(|(ts, _)| *ts);
        let steps = steps(&[(0, 0.0), (at, -2.0)]);
        assert_eq!(
            check(&readings, &steps, Applied::Added),
            Check::Agrees { changes: 1 }
        );
    }

    #[test]
    fn nothing_to_judge_by_is_untested() {
        let readings = readings(2 * HOUR, -1.0, -2.0, 1.0);
        assert_eq!(
            check(&readings, &Steps::fixed(-2.0), Applied::Added),
            Check::Untested,
            "a fixed offset never changes"
        );
        let small = steps(&[(0, -1.0), (2 * HOUR, -1.2)]);
        assert_eq!(
            check(&readings, &small, Applied::Added),
            Check::Untested,
            "a change smaller than the room's own drift proves nothing"
        );
        let unrecorded = steps(&[(0, -1.0), (100 * HOUR, -2.0)]);
        assert_eq!(
            check(&readings, &unrecorded, Applied::Added),
            Check::Untested,
            "no readings around the change"
        );
    }

    #[test]
    fn changes_both_ways_are_mixed() {
        let steps = steps(&[(0, 0.0), (HOUR, -1.0), (3 * HOUR, 0.0)]);
        let mut readings: Vec<(i64, f64)> = Vec::new();
        for index in 0..48 {
            let ts = index * 300 + 13;
            // Moves with the first change and against the second.
            let value = if ts < HOUR {
                21.0
            } else if ts < 3 * HOUR {
                20.0
            } else {
                19.0
            };
            readings.push((ts, value));
        }
        assert_eq!(
            check(&readings, &steps, Applied::Added),
            Check::Mixed {
                with: 1,
                against: 1
            }
        );
    }

    #[test]
    fn a_thermostats_offset_is_found_by_the_words_they_share() {
        let candidates: Vec<String> = [
            "number.bad_heizkorper_temperature_offset",
            "number.wohnzimmer_heizkorper_temperature_offset",
            "number.kuche_local_temperature_calibration",
        ]
        .map(String::from)
        .to_vec();
        let suggest = |sensor| suggest(sensor, &candidates).map(String::as_str);
        assert_eq!(
            suggest("sensor.bad_og_heizkorper_temperature"),
            Some("number.bad_heizkorper_temperature_offset")
        );
        assert_eq!(
            suggest("sensor.wohnzimmer_heizkorper_temperature"),
            Some("number.wohnzimmer_heizkorper_temperature_offset")
        );
        assert_eq!(
            suggest("sensor.kuche_temperature"),
            Some("number.kuche_local_temperature_calibration")
        );
        assert_eq!(
            suggest("sensor.bad_temperature"),
            None,
            "a plain thermometer in the same room shares too little"
        );
        assert_eq!(suggest("sensor.office_temperature"), None);

        let twins: Vec<String> = ["number.bad_a_offset", "number.bad_b_offset"]
            .map(String::from)
            .to_vec();
        assert_eq!(
            super::suggest("sensor.bad_temperature", &twins),
            None,
            "two equally good matches are no match"
        );
    }

    /// A database with one thermostat reporting `reported` °C every hour for two days,
    /// and the `(hours from the start, value)` its offset setting took.
    fn thermostat(reported: f64, offsets: &[(i64, &str)]) -> (Connection, LoadedSamples) {
        use ha_testdb::{self as testdb, Flavour, ts};
        let conn = Connection::open_in_memory().unwrap();
        testdb::create_schema(&conn, Flavour::Modern);
        let start = ts("2024-11-01 00:00:00");
        let sensor = "sensor.bath_radiator_temperature";
        let id = testdb::insert_statistic_meta(&conn, sensor, Some("°C"));
        for hour in 0..48 {
            testdb::insert_statistics_row(
                &conn,
                Flavour::Modern,
                "statistics",
                id,
                start + hour * HOUR,
                Some(reported),
            );
        }
        let offset = testdb::insert_states_meta(&conn, "number.bath_radiator_temperature_offset");
        for (hour, value) in offsets {
            let at = start + hour * HOUR;
            testdb::insert_state(&conn, Flavour::Modern, "", offset, at, value);
        }
        let loaded = ha_recorder::source::load(
            &conn,
            &ha_recorder::source::LoadOptions {
                entity: sensor,
                source: ha_recorder::source::SourceKind::Auto,
                value: ha_recorder::source::schema::ValueColumn::Mean,
                from: None,
                to: None,
                max_gap: 900,
                quantity: ha_recorder::Quantity::Temperature,
                scale: None,
                clamp_negative: false,
                tz: Tz::UTC,
            },
        )
        .unwrap();
        (conn, loaded)
    }

    fn room(offset: Offset) -> Room {
        Room {
            name: "Bath".to_string(),
            sensor: "sensor.bath_radiator_temperature".to_string(),
            offset,
        }
    }

    #[test]
    fn a_named_setting_is_read_and_taken_back_out() {
        let (conn, mut loaded) = thermostat(19.0, &[(12, "-2"), (30, "unavailable"), (31, "-2")]);
        let entity = "number.bath_radiator_temperature_offset".to_string();
        let finding = resolve(
            &conn,
            &room(Offset::Entity {
                entity: entity.clone(),
                applied: Applied::Added,
            }),
            &mut loaded,
            &[],
        )
        .unwrap();
        let Some(Finding::Corrected(correction)) = finding else {
            panic!("{finding:?}");
        };
        assert!(loaded.samples.iter().all(|sample| sample.value == 21.0));
        assert_eq!(correction.values(), "-2 °C");
        assert_eq!(correction.check, Check::Untested, "it never changed");
        let from = ha_testdb::ts("2024-11-01 12:00:00");
        assert_eq!(
            correction.assumed_before,
            Some(from),
            "the first 12 hours rest on it"
        );
        assert_eq!(
            correction.sentence(Tz::UTC),
            "-2 °C from number.bath_radiator_temperature_offset (on record since 2024-11-01; \
             the first value is assumed before that)"
        );
    }

    #[test]
    fn a_setting_with_nothing_on_record_is_an_error_that_says_what_to_do() {
        let (conn, mut loaded) = thermostat(19.0, &[(3, "unavailable")]);
        let error = resolve(
            &conn,
            &room(Offset::Entity {
                entity: "number.bath_radiator_temperature_offset".to_string(),
                applied: Applied::Added,
            }),
            &mut loaded,
            &[],
        )
        .unwrap_err();
        let text = format!("{error:#}");
        assert!(text.contains("has no value on record"), "{text}");
        assert!(text.contains("offset = -2"), "{text}");
        assert!(loaded.samples.iter().all(|sample| sample.value == 19.0));
    }

    #[test]
    fn a_room_that_names_no_offset_is_only_given_a_suggestion() {
        let (conn, mut loaded) = thermostat(19.0, &[(0, "-2")]);
        let before = loaded.clone();
        assert_eq!(
            resolve(&conn, &room(Offset::Unset), &mut loaded, &[]).unwrap(),
            Some(Finding::Suggested(
                "number.bath_radiator_temperature_offset".to_string()
            ))
        );
        assert_eq!(loaded, before, "nothing is taken out on a guess");
        assert_eq!(
            resolve(
                &conn,
                &room(Offset::Unset),
                &mut loaded,
                &["number.bath_radiator_temperature_offset"]
            )
            .unwrap(),
            None,
            "another room's offset is not suggested"
        );
        assert_eq!(
            resolve(&conn, &room(Offset::Off), &mut loaded, &[]).unwrap(),
            None
        );
    }

    #[test]
    fn a_fixed_offset_needs_nothing_from_the_database() {
        let (conn, mut loaded) = thermostat(19.0, &[]);
        let finding = resolve(
            &conn,
            &room(Offset::Fixed {
                degrees: 1.5,
                applied: Applied::Subtracted,
            }),
            &mut loaded,
            &[],
        )
        .unwrap();
        let Some(Finding::Corrected(correction)) = finding else {
            panic!("{finding:?}");
        };
        assert!(loaded.samples.iter().all(|sample| sample.value == 20.5));
        assert_eq!(
            correction.sentence(Tz::UTC),
            "+1.5 °C, as the rooms file gives it"
        );
    }

    #[test]
    fn many_values_are_summed_up() {
        let correction = |values: &[f64]| Correction {
            entity: None,
            steps: Steps(
                values
                    .iter()
                    .enumerate()
                    .map(|(i, v)| (i as i64, *v))
                    .collect(),
            ),
            applied: Applied::Added,
            check: Check::Untested,
            unit: "°C".to_string(),
            assumed_before: None,
        };
        assert_eq!(correction(&[-1.0, -2.0]).values(), "-1 °C, then -2 °C");
        assert_eq!(
            correction(&[-1.0, -2.0, -1.5, -3.0, -2.5]).values(),
            "-1 °C, then 3 more, then -2.5 °C"
        );
    }

    #[test]
    fn offsets_are_written_with_their_sign() {
        assert_eq!(format_offset(-2.0, "°C"), "-2 °C");
        assert_eq!(format_offset(0.5, "°C"), "+0.5 °C");
        assert_eq!(format_offset(-1.25, "°F"), "-1.25 °F");
        assert_eq!(format_offset(0.0, "°C"), "0 °C");
        assert_eq!(format_offset(-0.001, "°C"), "0 °C");
    }
}
