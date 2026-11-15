//! Taking a thermostat's own offset back out of its readings.
//!
//! A thermostat on a radiator reads warm, so it is often given an offset: set it to -2
//! and it reports two degrees less than it measures, and heats until the far side of the
//! room is warm enough. The reading Home Assistant records then has the offset in it, and
//! cannot be compared with a plain thermometer's. Knowing the offset at every moment, the
//! tool takes it back out again: what remains is what the thermostat measured.
//!
//! Nothing has to be configured. A thermostat's reading and its offset setting are
//! entities of one device, so their ids start with the same device name:
//! `sensor.bad_og_heizkorper_temperature` and `number.bad_og_heizkorper_temperature_offset`.
//! A room's thermometer with such a setting beside it is corrected, and the run says so.

use anyhow::Result;
use chrono_tz::Tz;
use rusqlite::Connection;

use ha_recorder::Sample;
use ha_recorder::source::{LoadedSamples, states};
use ha_recorder::timeutil::{day_to_date, local_day};

/// Words an offset setting's entity id is made of, in the languages Home Assistant names
/// them in. Matched within a word too, so `temperaturversatz` counts.
pub const OFFSET_WORDS: [&str; 5] = [
    "offset",
    "calibration",
    "kalibrierung",
    "versatz",
    "korrektur",
];

/// Words that say what an entity of a thermostat measures or sets rather than which
/// device it belongs to, as they end its id: `_temperature`, `_local_temperature`.
const MEASURE_WORDS: [&str; 6] = [
    "temperature",
    "temperatur",
    "local",
    "lokale",
    "lokaler",
    "current",
];

/// How a thermostat applies its offset to what it measures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Applied {
    /// It reports what it measures plus the offset: an offset of -2 reads two degrees
    /// low. This is how Zigbee and Matter define a thermostat's calibration.
    Added,
    /// It reports what it measures minus the offset.
    Subtracted,
}

impl Applied {
    /// The sign the offset was applied with: the reading is what was measured plus this
    /// times the offset.
    pub fn sign(self) -> f64 {
        match self {
            Applied::Added => 1.0,
            Applied::Subtracted => -1.0,
        }
    }
}

/// The values a thermostat's offset took, and from when, earliest first.
///
/// Each value holds from its time until the next. Before the first, the first is assumed:
/// Home Assistant keeps raw states, where a setting lives, for ten days by default, while
/// a thermometer's hourly statistics go back years, and an offset is usually set once,
/// when the thermostat goes in.
#[derive(Debug, Clone, PartialEq)]
pub struct Steps(Vec<(i64, f64)>);

impl Steps {
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

    /// When the first value is on record.
    pub fn first_ts(&self) -> i64 {
        self.0[0].0
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

/// One change of the offset - when, from what, to what - and how far the reading moved.
pub type Moved = ((i64, f64, f64), f64);

/// What the recorded readings say about how the offset is applied.
#[derive(Debug, Clone, PartialEq)]
pub enum Check {
    /// No change of the offset with readings on both sides of it to judge by.
    Untested,
    /// At every change judged, the reading moved with the offset: it is added.
    Added { changes: usize },
    /// At every change judged, the reading moved against it: it is subtracted. The first
    /// such change.
    Subtracted { changes: usize, first: Moved },
    /// The reading did not move when the offset changed, as though it were reported
    /// without the offset in it. The first such change.
    Unmoved { first: Moved },
    /// Some changes one way, some the other.
    Mixed { added: usize, subtracted: usize },
}

impl Check {
    /// Which way the offset is taken out: the way the readings show, or as Zigbee and
    /// Matter define it when they show nothing clear.
    pub fn applied(&self) -> Applied {
        match self {
            Check::Subtracted { .. } => Applied::Subtracted,
            _ => Applied::Added,
        }
    }
}

/// Judge from the thermostat's raw readings, `(time, value)` in time order, how it applies
/// its offset.
///
/// When the offset changes by some amount, the reported reading jumps by the same amount:
/// up for an offset that is added and raised, down for one that is subtracted. The room
/// itself barely moves in the few minutes it takes the thermostat to report.
pub fn check(readings: &[(i64, f64)], steps: &Steps) -> Check {
    let (mut added, mut subtracted) = (0, 0);
    let (mut first_subtracted, mut first_still) = (None, None);
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
        if moved.abs() < step.abs() / 2.0 {
            first_still.get_or_insert((change, moved));
        } else if moved.signum() == step.signum() {
            added += 1;
        } else {
            subtracted += 1;
            first_subtracted.get_or_insert((change, moved));
        }
    }
    match (added, subtracted, first_subtracted, first_still) {
        (0, 0, _, None) => Check::Untested,
        (0, 0, _, Some(first)) => Check::Unmoved { first },
        (added, 0, _, _) => Check::Added { changes: added },
        (0, changes, Some(first), _) => Check::Subtracted { changes, first },
        (added, subtracted, _, _) => Check::Mixed { added, subtracted },
    }
}

/// The candidates that are `sensor`'s own offset setting: the same device name, followed
/// by nothing but words for a temperature and its offset.
///
/// `sensor.bad_og_heizkorper_temperature` is the device `bad_og_heizkorper` measuring a
/// temperature, so `number.bad_og_heizkorper_temperature_offset` and
/// `number.bad_og_heizkorper_local_temperature_calibration` are its offset, while
/// `number.bad_temperature_offset` belongs to another device, and so does
/// `number.bad_og_heizkorper_2_temperature_offset`.
pub fn find<'a>(sensor: &str, candidates: &'a [String]) -> Vec<&'a String> {
    let mut device: Vec<&str> = object_id(sensor).split('_').collect();
    while device
        .last()
        .is_some_and(|word| MEASURE_WORDS.contains(word))
    {
        device.pop();
    }
    if device.is_empty() {
        return Vec::new();
    }
    let prefix = format!("{}_", device.join("_"));

    candidates
        .iter()
        .filter(|candidate| {
            let Some(rest) = object_id(candidate).strip_prefix(&prefix) else {
                return false;
            };
            let is_offset = |word: &str| OFFSET_WORDS.iter().any(|offset| word.contains(offset));
            let words: Vec<&str> = rest.split('_').collect();
            words.iter().any(|word| is_offset(word))
                && words
                    .iter()
                    .all(|word| MEASURE_WORDS.contains(word) || is_offset(word))
        })
        .collect()
}

/// An entity id without its domain: `bad_temperature` for `sensor.bad_temperature`.
fn object_id(entity: &str) -> &str {
    entity.split_once('.').map_or(entity, |(_, object)| object)
}

/// What became of one room's offset.
#[derive(Debug, Clone, PartialEq)]
pub enum Finding {
    /// It was taken back out of the readings.
    Corrected(Correction),
    /// More than one setting is this thermostat's offset by its id; none was used.
    Ambiguous(Vec<String>),
    /// The thermostat's offset setting has no value on record; the readings are as
    /// reported.
    Unrecorded(String),
}

/// An offset that was taken back out, and what is known about it.
#[derive(Debug, Clone, PartialEq)]
pub struct Correction {
    /// The setting it came from.
    pub entity: String,
    pub steps: Steps,
    /// What the readings say about how it is applied, which decides how it is taken out.
    pub check: Check,
    /// The unit the thermostat records in, which its offset is in too.
    pub unit: String,
    /// When the setting's recorded history starts, if there are readings from before it:
    /// those rest on its earliest value having held then too.
    pub assumed_before: Option<i64>,
}

impl Correction {
    /// The values the setting took, in order: `-2 °C`, or `-1 °C, then -2 °C`.
    pub fn values(&self) -> String {
        listed(
            self.steps
                .values()
                .map(|degrees| format_offset(degrees, &self.unit)),
        )
    }

    /// What taking it out did to the readings: `raised by 2 °C`, or `raised by 1 °C,
    /// then raised by 2 °C`.
    pub fn effect(&self) -> String {
        let sign = self.check.applied().sign();
        listed(self.steps.values().map(|degrees| {
            let change = -sign * degrees;
            let size = format_offset(change.abs(), &self.unit);
            let size = size.trim_start_matches('+');
            if change > 0.0 {
                format!("raised by {size}")
            } else if change < 0.0 {
                format!("lowered by {size}")
            } else {
                "left as they were".to_string()
            }
        }))
    }

    /// How far back the setting is on record, when the readings go back further.
    pub fn since(&self, tz: Tz) -> Option<String> {
        self.assumed_before.map(|from| {
            format!(
                "on record since {}, and taken to have been the same before",
                local_date(from, tz)
            )
        })
    }

    /// Everything that was done, as the report says it.
    pub fn sentence(&self, tz: Tz) -> String {
        let mut sentence = format!(
            "readings {}, taking out its thermostat's offset {} ({})",
            self.effect(),
            self.entity,
            self.values()
        );
        if let Some(since) = self.since(tz) {
            sentence.push_str(", ");
            sentence.push_str(&since);
        }
        sentence
    }
}

/// `a`, `a, then b`, `a, then b, then c`; for longer lists the first, how many more, and
/// the last.
fn listed(items: impl Iterator<Item = String>) -> String {
    let items: Vec<String> = items.collect();
    match items.as_slice() {
        [first, .., last] if items.len() > 3 => {
            format!("{first}, then {} more, then {last}", items.len() - 2)
        }
        _ => items.join(", then "),
    }
}

/// Look for the offset setting of the thermostat behind `sensor`, and take it back out of
/// its loaded readings if there is one.
///
/// The setting is read from the raw states. Which way it is taken out is read from the
/// thermostat's own raw readings where they show it, and is otherwise the Zigbee and
/// Matter convention.
pub fn resolve(
    conn: &Connection,
    sensor: &str,
    loaded: &mut LoadedSamples,
) -> Result<Option<Finding>> {
    let candidates = states::entities_matching(conn, "number", &OFFSET_WORDS)?;
    let entity = match find(sensor, &candidates).as_slice() {
        [] => return Ok(None),
        [one] => (*one).clone(),
        several => {
            return Ok(Some(Finding::Ambiguous(
                several.iter().map(|id| (*id).clone()).collect(),
            )));
        }
    };
    let Some(steps) = Steps::from_history(&states::history(conn, &entity)?) else {
        return Ok(Some(Finding::Unrecorded(entity)));
    };

    // A thermostat without raw readings just goes unchecked.
    let check = if steps.changes().next().is_some() {
        states::history(conn, sensor).map_or(Check::Untested, |readings| check(&readings, &steps))
    } else {
        Check::Untested
    };
    let assumed_before =
        Some(steps.first_ts()).filter(|from| loaded.first_ts.is_some_and(|first| first < *from));
    loaded.samples = take_out(
        &loaded.samples,
        &steps,
        check.applied(),
        loaded.conversion.scale,
    );
    Ok(Some(Finding::Corrected(Correction {
        entity,
        steps,
        check,
        unit: loaded.unit.clone().unwrap_or_else(|| "°C".to_string()),
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

    /// One offset, in force since long before any reading.
    fn constant(degrees: f64) -> Steps {
        steps(&[(-1_000 * HOUR, degrees)])
    }

    #[test]
    fn a_recorded_history_becomes_the_steps_it_changed_in() {
        let steps = steps(&[(100, -1.0), (200, -1.0), (300, -2.0), (400, -2.0)]);
        assert_eq!(
            steps.0,
            [(100, -1.0), (300, -2.0)],
            "restarts repeat a value"
        );
        assert_eq!(steps.first_ts(), 100);
        assert_eq!(steps.changes().collect::<Vec<_>>(), [(300, -1.0, -2.0)]);
        assert_eq!(steps.at(0), -1.0, "the first value is assumed before it");
        assert_eq!(steps.at(299), -1.0);
        assert_eq!(steps.at(300), -2.0);
        assert!(Steps::from_history(&[]).is_none());
    }

    #[test]
    fn an_added_offset_is_taken_back_out() {
        // A thermostat set to -2 reports 19 °C where it measures 21.
        let samples = [
            Sample::new(0, HOUR, 19.0),
            Sample::new(HOUR, 2 * HOUR, 19.5),
        ];
        let corrected = take_out(&samples, &constant(-2.0), Applied::Added, 1.0);
        assert_eq!(
            corrected,
            [
                Sample::new(0, HOUR, 21.0),
                Sample::new(HOUR, 2 * HOUR, 21.5)
            ]
        );
        // One that subtracts its offset of 2 reported the same 19 °C.
        let corrected = take_out(&samples, &constant(2.0), Applied::Subtracted, 1.0);
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
        let corrected = take_out(&samples, &constant(-3.6), Applied::Added, 5.0 / 9.0);
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
        let added = check(&readings(at, -1.0, -2.0, 1.0), &steps);
        assert_eq!(added, Check::Added { changes: 1 });
        assert_eq!(added.applied(), Applied::Added);

        let subtracted = check(&readings(at, -1.0, -2.0, -1.0), &steps);
        assert_eq!(
            subtracted,
            Check::Subtracted {
                changes: 1,
                first: ((at, -1.0, -2.0), 1.0)
            }
        );
        assert_eq!(subtracted.applied(), Applied::Subtracted);

        let still: Vec<(i64, f64)> = (0..48).map(|index| (index * 300, 21.0)).collect();
        let unmoved = check(&still, &steps);
        assert_eq!(
            unmoved,
            Check::Unmoved {
                first: ((at, -1.0, -2.0), 0.0)
            }
        );
        assert_eq!(
            unmoved.applied(),
            Applied::Added,
            "taken out as Zigbee says"
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
        assert_eq!(check(&readings, &steps), Check::Added { changes: 1 });
    }

    #[test]
    fn nothing_to_judge_by_is_untested() {
        let readings = readings(2 * HOUR, -1.0, -2.0, 1.0);
        assert_eq!(check(&readings, &constant(-2.0)), Check::Untested);
        let small = steps(&[(0, -1.0), (2 * HOUR, -1.2)]);
        assert_eq!(
            check(&readings, &small),
            Check::Untested,
            "a change smaller than the room's own drift proves nothing"
        );
        let unrecorded = steps(&[(0, -1.0), (100 * HOUR, -2.0)]);
        assert_eq!(
            check(&readings, &unrecorded),
            Check::Untested,
            "no readings around the change"
        );
        assert_eq!(Check::Untested.applied(), Applied::Added);
    }

    #[test]
    fn changes_both_ways_are_mixed() {
        let steps = steps(&[(0, 0.0), (HOUR, -1.0), (3 * HOUR, 0.0)]);
        let readings: Vec<(i64, f64)> = (0..48)
            .map(|index| {
                let ts = index * 300 + 13;
                // Moves with the first change and against the second.
                let value = if ts < HOUR {
                    21.0
                } else if ts < 3 * HOUR {
                    20.0
                } else {
                    19.0
                };
                (ts, value)
            })
            .collect();
        let mixed = check(&readings, &steps);
        assert_eq!(
            mixed,
            Check::Mixed {
                added: 1,
                subtracted: 1
            }
        );
        assert_eq!(mixed.applied(), Applied::Added);
    }

    #[test]
    fn a_thermostats_offset_is_the_setting_of_the_same_device() {
        let candidates: Vec<String> = [
            "number.bad_og_heizkorper_temperature_offset",
            "number.bad_og_heizkorper_2_temperature_offset",
            "number.bad_temperature_offset",
            "number.bad_og_heizkorper_window_open_offset",
            "number.kuche_local_temperature_calibration",
            "number.wohnzimmer_heizkorper_temperaturversatz",
        ]
        .map(String::from)
        .to_vec();
        let find = |sensor| -> Vec<&str> {
            find(sensor, &candidates)
                .into_iter()
                .map(String::as_str)
                .collect()
        };
        assert_eq!(
            find("sensor.bad_og_heizkorper_temperature"),
            ["number.bad_og_heizkorper_temperature_offset"]
        );
        assert_eq!(
            find("sensor.bad_og_heizkorper_local_temperature"),
            ["number.bad_og_heizkorper_temperature_offset"],
            "the measurement's own words are not the device's"
        );
        assert_eq!(
            find("sensor.kuche_temperature"),
            ["number.kuche_local_temperature_calibration"]
        );
        assert_eq!(
            find("sensor.wohnzimmer_heizkorper_temperature"),
            ["number.wohnzimmer_heizkorper_temperaturversatz"]
        );
        assert_eq!(
            find("sensor.bad_temperature"),
            ["number.bad_temperature_offset"],
            "and not the thermostat next to it, whose name only starts the same"
        );
        assert!(find("sensor.office_temperature").is_empty());
        assert!(
            find("sensor.temperature").is_empty(),
            "no device name at all"
        );

        let both: Vec<String> = [
            "number.bad_temperature_offset",
            "number.bad_local_temperature_calibration",
        ]
        .map(String::from)
        .to_vec();
        assert_eq!(super::find("sensor.bad_temperature", &both).len(), 2);
    }

    /// A database with one thermostat reporting `reported` °C every hour for two days,
    /// and the `(hour from the start, value)`s its offset setting `offset` took.
    fn thermostat(
        reported: f64,
        offset: &str,
        values: &[(i64, &str)],
    ) -> (Connection, LoadedSamples) {
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
        let offset_id = testdb::insert_states_meta(&conn, offset);
        for (hour, value) in values {
            testdb::insert_state(
                &conn,
                Flavour::Modern,
                "",
                offset_id,
                start + hour * HOUR,
                value,
            );
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

    const SENSOR: &str = "sensor.bath_radiator_temperature";
    const OFFSET: &str = "number.bath_radiator_temperature_offset";

    #[test]
    fn the_thermostats_offset_is_found_and_taken_back_out() {
        let (conn, mut loaded) =
            thermostat(19.0, OFFSET, &[(12, "-2"), (30, "unavailable"), (31, "-2")]);
        let finding = resolve(&conn, SENSOR, &mut loaded).unwrap();
        let Some(Finding::Corrected(correction)) = finding else {
            panic!("{finding:?}");
        };
        assert!(loaded.samples.iter().all(|sample| sample.value == 21.0));
        assert_eq!(correction.entity, OFFSET);
        assert_eq!(correction.values(), "-2 °C");
        assert_eq!(correction.effect(), "raised by 2 °C");
        assert_eq!(correction.check, Check::Untested, "it never changed");
        assert_eq!(
            correction.sentence(Tz::UTC),
            "readings raised by 2 °C, taking out its thermostat's offset \
             number.bath_radiator_temperature_offset (-2 °C), on record since 2024-11-01, and \
             taken to have been the same before"
        );
    }

    #[test]
    fn another_devices_setting_is_left_alone() {
        let (conn, mut loaded) = thermostat(19.0, "number.bath_temperature_offset", &[(0, "-2")]);
        let before = loaded.clone();
        assert_eq!(resolve(&conn, SENSOR, &mut loaded).unwrap(), None);
        assert_eq!(loaded, before);
    }

    #[test]
    fn a_setting_with_nothing_on_record_changes_nothing() {
        let (conn, mut loaded) = thermostat(19.0, OFFSET, &[(3, "unavailable")]);
        let before = loaded.clone();
        assert_eq!(
            resolve(&conn, SENSOR, &mut loaded).unwrap(),
            Some(Finding::Unrecorded(OFFSET.to_string()))
        );
        assert_eq!(loaded, before);
    }

    #[test]
    fn two_settings_that_both_fit_change_nothing() {
        let (conn, mut loaded) = thermostat(19.0, OFFSET, &[(0, "-2")]);
        ha_testdb::insert_states_meta(&conn, "number.bath_radiator_local_temperature_calibration");
        let before = loaded.clone();
        assert_eq!(
            resolve(&conn, SENSOR, &mut loaded).unwrap(),
            Some(Finding::Ambiguous(vec![
                "number.bath_radiator_local_temperature_calibration".to_string(),
                OFFSET.to_string()
            ]))
        );
        assert_eq!(loaded, before);
    }

    #[test]
    fn effects_say_which_way_the_readings_moved() {
        let correction = |values: &[f64], check: Check| Correction {
            entity: OFFSET.to_string(),
            steps: Steps(
                values
                    .iter()
                    .enumerate()
                    .map(|(index, value)| (index as i64, *value))
                    .collect(),
            ),
            check,
            unit: "°C".to_string(),
            assumed_before: None,
        };
        let added = correction(&[-1.0, -2.0], Check::Untested);
        assert_eq!(added.values(), "-1 °C, then -2 °C");
        assert_eq!(added.effect(), "raised by 1 °C, then raised by 2 °C");
        let subtracted = Check::Subtracted {
            changes: 1,
            first: ((1, -1.0, -2.0), 1.0),
        };
        assert_eq!(
            correction(&[-1.0, 0.0], subtracted).effect(),
            "lowered by 1 °C, then left as they were"
        );
        assert_eq!(
            correction(&[-1.0, -2.0, -1.5, -3.0, -2.5], Check::Untested).values(),
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
