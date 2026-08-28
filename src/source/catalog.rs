//! What a recorder database actually offers.
//!
//! The first real run of this tool failed on `sensor.…_energy`: the entity existed, its
//! statistics rows existed, and every one of them was skipped, because Home Assistant
//! writes `sum`/`state` and leaves `mean` NULL for a cumulative counter. Nothing in the
//! error said so, and nothing helped find the `…_power` sibling that was actually wanted.
//!
//! This module answers both questions - what is in here, and what can this tool plot -
//! from the rows themselves rather than from metadata flags, because `has_mean` was
//! replaced by `mean_type` in recent recorder releases and neither is worth trusting.

use std::collections::HashMap;

use anyhow::Result;
use chrono_tz::Tz;
use rusqlite::Connection;

use crate::source::schema::{StatisticsTable, column_names, table_exists, unit_scale_to_watts};
use crate::timeutil::{day_to_date, local_day};

/// What the rows of a statistic really carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatisticKind {
    /// Hourly means: a measurement, which is what this tool plots.
    Mean,
    /// Cumulative totals: an energy counter, which it cannot.
    Total,
    /// Both columns are populated.
    Both,
    /// Rows exist but carry neither, or there are no rows at all.
    Empty,
}

impl StatisticKind {
    fn from_counts(means: i64, sums: i64) -> Self {
        match (means > 0, sums > 0) {
            (true, true) => StatisticKind::Both,
            (true, false) => StatisticKind::Mean,
            (false, true) => StatisticKind::Total,
            (false, false) => StatisticKind::Empty,
        }
    }

    /// Whether this tool can read power out of it.
    pub fn has_mean(self) -> bool {
        matches!(self, StatisticKind::Mean | StatisticKind::Both)
    }

    pub fn label(self) -> &'static str {
        match self {
            StatisticKind::Mean => "mean",
            StatisticKind::Total => "total",
            StatisticKind::Both => "mean+total",
            StatisticKind::Empty => "empty",
        }
    }
}

/// One statistic in the database, with what it holds.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub statistic_id: String,
    pub unit: Option<String>,
    pub kind: StatisticKind,
    pub rows: i64,
    pub first_ts: Option<i64>,
    pub last_ts: Option<i64>,
}

impl Candidate {
    /// Whether this is a power sensor this tool can plot straight away.
    pub fn is_plottable(&self) -> bool {
        self.kind.has_mean() && self.is_power_unit() && self.rows > 0
    }

    /// Whether its unit is one that converts to watts.
    pub fn is_power_unit(&self) -> bool {
        self.unit.as_deref().and_then(unit_scale_to_watts).is_some()
    }

    /// Whether it looks like an energy counter rather than a power reading.
    pub fn is_energy_counter(&self) -> bool {
        matches!(self.kind, StatisticKind::Total | StatisticKind::Both)
            && self
                .unit
                .as_deref()
                .is_some_and(|unit| unit.trim().ends_with("Wh"))
    }

    /// Ranking group: plottable power sensors, then energy counters, then everything else.
    fn group(&self) -> u8 {
        if self.is_plottable() {
            0
        } else if self.is_energy_counter() {
            1
        } else {
            2
        }
    }
}

/// Every statistic in the database, best candidates first.
///
/// `filter` keeps only ids containing that text, case-insensitively.
pub fn list_statistics(conn: &Connection, filter: Option<&str>) -> Result<Vec<Candidate>> {
    if !table_exists(conn, "statistics_meta")? {
        return Ok(Vec::new());
    }
    let has_unit = column_names(conn, "statistics_meta")?
        .iter()
        .any(|column| column == "unit_of_measurement");
    let sql = if has_unit {
        "SELECT id, statistic_id, unit_of_measurement FROM statistics_meta"
    } else {
        "SELECT id, statistic_id, NULL FROM statistics_meta"
    };

    let needle = filter
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_ascii_lowercase);

    let mut stmt = conn.prepare(sql)?;
    let metas: Vec<(i64, String, Option<String>)> = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .into_iter()
        .filter(|(_, statistic_id, _)| match &needle {
            Some(needle) => statistic_id.to_ascii_lowercase().contains(needle),
            None => true,
        })
        .collect();

    let ids: Vec<i64> = metas.iter().map(|(id, _, _)| *id).collect();
    let stats = row_summary(conn, &ids)?;

    let mut candidates: Vec<Candidate> = metas
        .into_iter()
        .map(|(id, statistic_id, unit)| {
            let summary = stats.get(&id).copied().unwrap_or_default();
            Candidate {
                statistic_id,
                unit,
                kind: StatisticKind::from_counts(summary.means, summary.sums),
                rows: summary.rows,
                first_ts: summary.first_ts,
                last_ts: summary.last_ts,
            }
        })
        .collect();

    candidates.sort_by(|left, right| {
        left.group()
            .cmp(&right.group())
            .then(right.rows.cmp(&left.rows))
            .then(left.statistic_id.cmp(&right.statistic_id))
    });
    Ok(candidates)
}

#[derive(Debug, Clone, Copy, Default)]
struct RowSummary {
    rows: i64,
    means: i64,
    sums: i64,
    first_ts: Option<i64>,
    last_ts: Option<i64>,
}

/// One grouped pass over `statistics`, so the kinds come from the data.
///
/// `COUNT(column)` ignores NULLs, which is exactly the question being asked: how many of
/// these rows actually carry a mean, and how many carry a total.
fn row_summary(conn: &Connection, ids: &[i64]) -> Result<HashMap<i64, RowSummary>> {
    let mut summaries = HashMap::new();
    if ids.is_empty() || !table_exists(conn, StatisticsTable::LongTerm.table_name())? {
        return Ok(summaries);
    }
    let columns = column_names(conn, "statistics")?;
    let time = if columns.iter().any(|column| column == "start_ts") {
        "start_ts"
    } else {
        "start"
    };
    let sum_column = if columns.iter().any(|column| column == "sum") {
        "sum"
    } else {
        "NULL"
    };

    // Narrow the scan when a filter already picked a handful of entities out.
    let restrict = if ids.len() <= 64 {
        format!(
            " WHERE metadata_id IN ({})",
            ids.iter()
                .map(i64::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        )
    } else {
        String::new()
    };
    let sql = format!(
        "SELECT metadata_id, COUNT(*), COUNT(mean), COUNT({sum_column}), \
         MIN(\"{time}\"), MAX(\"{time}\") FROM statistics{restrict} GROUP BY metadata_id"
    );

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            RowSummary {
                rows: row.get(1)?,
                means: row.get(2)?,
                sums: row.get(3)?,
                first_ts: crate::source::schema::value_to_timestamp(&row.get(4)?),
                last_ts: crate::source::schema::value_to_timestamp(&row.get(5)?),
            },
        ))
    })?;
    for row in rows {
        let (id, summary) = row?;
        summaries.insert(id, summary);
    }
    Ok(summaries)
}

/// Candidates that look like a replacement for `wanted`.
///
/// Entities sharing a prefix come first, so `sensor.eve_energy_x_energy` is answered with
/// `sensor.eve_energy_x_power` rather than with whatever else happens to be in the
/// database.
pub fn closest_power_sensors(
    candidates: &[Candidate],
    wanted: &str,
    limit: usize,
) -> Vec<Candidate> {
    let wanted = wanted.to_ascii_lowercase();
    let mut plottable: Vec<&Candidate> = candidates
        .iter()
        .filter(|candidate| candidate.is_plottable())
        .collect();
    plottable.sort_by_key(|candidate| {
        (
            usize::MAX - shared_prefix(&wanted, &candidate.statistic_id.to_ascii_lowercase()),
            -candidate.rows,
        )
    });
    plottable.into_iter().take(limit).cloned().collect()
}

/// Length of the common prefix of two ids, in bytes.
fn shared_prefix(left: &str, right: &str) -> usize {
    left.bytes()
        .zip(right.bytes())
        .take_while(|(a, b)| a == b)
        .count()
}

/// The "here is what this database does hold" block appended to entity errors.
///
/// Every error about an entity ends here, because the useful next step is always the same:
/// see which power sensors exist, ideally the sibling of whatever was asked for.
pub fn suggest_block(conn: &Connection, wanted: &str, tz: Tz) -> String {
    let candidates = list_statistics(conn, None).unwrap_or_default();
    let closest = closest_power_sensors(&candidates, wanted, 5);
    if closest.is_empty() {
        "\n\nThis database holds no power sensor with hourly statistics at all. \
         Run with --list-entities to see what it does hold."
            .to_string()
    } else {
        format!(
            "\n\nPower sensors in this database:\n{}\nRun with --list-entities to see them all.",
            format_candidates(&closest, tz, 5)
        )
    }
}

/// Render candidates as an aligned table, grouped and capped.
pub fn format_candidates(candidates: &[Candidate], tz: Tz, limit: usize) -> String {
    if candidates.is_empty() {
        return "  (none)\n".to_string();
    }
    let shown = &candidates[..candidates.len().min(limit.max(1))];
    let id_width = shown
        .iter()
        .map(|candidate| candidate.statistic_id.len())
        .max()
        .unwrap_or(0);
    let unit_width = shown
        .iter()
        .map(|candidate| candidate.unit.as_deref().unwrap_or("-").len())
        .max()
        .unwrap_or(0);

    let mut out = String::new();
    for candidate in shown {
        out.push_str(&format!(
            "  {:id_width$}  {:unit_width$}  {:>7} rows  {}\n",
            candidate.statistic_id,
            candidate.unit.as_deref().unwrap_or("-"),
            candidate.rows,
            date_range(candidate, tz),
        ));
    }
    if candidates.len() > shown.len() {
        out.push_str(&format!(
            "  ... and {} more\n",
            candidates.len() - shown.len()
        ));
    }
    out
}

/// The local dates a candidate spans, for the listing.
pub fn date_range(candidate: &Candidate, tz: Tz) -> String {
    match (candidate.first_ts, candidate.last_ts) {
        (Some(first), Some(last)) => {
            let format = |ts: i64| {
                day_to_date(local_day(ts, tz))
                    .map(|date| date.to_string())
                    .unwrap_or_else(|| "?".to_string())
            };
            format!("{} to {}", format(first), format(last))
        }
        _ => "no rows".to_string(),
    }
}

/// The whole listing printed by `--list-entities`, in sections.
pub fn format_listing(candidates: &[Candidate], tz: Tz, limit: usize) -> String {
    let plottable: Vec<Candidate> = candidates
        .iter()
        .filter(|candidate| candidate.is_plottable())
        .cloned()
        .collect();
    let energy: Vec<Candidate> = candidates
        .iter()
        .filter(|candidate| !candidate.is_plottable() && candidate.is_energy_counter())
        .cloned()
        .collect();
    let rest: Vec<Candidate> = candidates
        .iter()
        .filter(|candidate| !candidate.is_plottable() && !candidate.is_energy_counter())
        .cloned()
        .collect();

    let mut out = String::new();
    out.push_str("Power sensors this tool can plot:\n");
    out.push_str(&format_candidates(&plottable, tz, limit));
    if !energy.is_empty() {
        out.push_str("\nEnergy counters (cumulative totals - not plottable, see the README):\n");
        out.push_str(&format_candidates(&energy, tz, limit));
    }
    if !rest.is_empty() {
        out.push_str("\nOther statistics:\n");
        out.push_str(&format_candidates(&rest, tz, limit));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono_tz::Tz::UTC;

    use crate::source::testdb::{self, ENTITY, Flavour, ts};

    /// A database with a power sensor, its energy counter sibling, and a thermometer.
    fn database() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        testdb::create_schema(&conn, Flavour::Modern);
        let power = testdb::insert_statistic_meta(&conn, ENTITY, Some("W"));
        // The counter claims a mean in its metadata but carries none in its rows, the way
        // a recent recorder schema does; the data has the last word.
        let energy = testdb::insert_statistic_meta_with_flags(
            &conn,
            "sensor.solar_energy",
            Some("kWh"),
            true,
            true,
        );
        let temperature = testdb::insert_statistic_meta(&conn, "sensor.attic_temp", Some("°C"));

        let base = ts("2025-06-01 00:00:00");
        for hour in 0..10 {
            testdb::insert_statistics_row(
                &conn,
                Flavour::Modern,
                "statistics",
                power,
                base + hour * 3_600,
                Some(1_000.0),
            );
        }
        for hour in 0..6 {
            testdb::insert_statistics_sum_row(
                &conn,
                Flavour::Modern,
                "statistics",
                energy,
                base + hour * 3_600,
                hour as f64,
            );
        }
        testdb::insert_statistics_row(
            &conn,
            Flavour::Modern,
            "statistics",
            temperature,
            base,
            Some(21.0),
        );
        conn
    }

    fn find<'a>(candidates: &'a [Candidate], id: &str) -> &'a Candidate {
        candidates
            .iter()
            .find(|candidate| candidate.statistic_id == id)
            .unwrap_or_else(|| panic!("{id} missing from {candidates:?}"))
    }

    #[test]
    fn kinds_come_from_the_rows_not_the_metadata() {
        let candidates = list_statistics(&database(), None).unwrap();

        let power = find(&candidates, ENTITY);
        assert_eq!(power.kind, StatisticKind::Mean);
        assert_eq!(power.rows, 10);
        assert!(power.is_plottable());
        assert!(!power.is_energy_counter());

        // Its metadata says has_mean = 1, but not one row carries a mean.
        let energy = find(&candidates, "sensor.solar_energy");
        assert_eq!(energy.kind, StatisticKind::Total);
        assert_eq!(energy.rows, 6);
        assert!(!energy.is_plottable());
        assert!(energy.is_energy_counter());
    }

    #[test]
    fn power_sensors_rank_above_counters_and_everything_else() {
        let candidates = list_statistics(&database(), None).unwrap();
        let order: Vec<&str> = candidates
            .iter()
            .map(|candidate| candidate.statistic_id.as_str())
            .collect();
        assert_eq!(
            order,
            vec![ENTITY, "sensor.solar_energy", "sensor.attic_temp"]
        );
    }

    #[test]
    fn a_thermometer_is_neither_plottable_nor_a_counter() {
        let candidates = list_statistics(&database(), None).unwrap();
        let temperature = find(&candidates, "sensor.attic_temp");
        assert!(!temperature.is_plottable(), "°C is not power");
        assert!(!temperature.is_energy_counter());
        assert_eq!(temperature.kind, StatisticKind::Mean);
    }

    #[test]
    fn the_filter_narrows_by_substring() {
        let conn = database();
        let solar = list_statistics(&conn, Some("solar")).unwrap();
        assert_eq!(solar.len(), 2);
        assert!(
            list_statistics(&conn, Some("SOLAR")).unwrap().len() == 2,
            "case insensitive"
        );
        assert_eq!(list_statistics(&conn, Some("attic")).unwrap().len(), 1);
        assert!(
            list_statistics(&conn, Some("nothing here"))
                .unwrap()
                .is_empty()
        );
        // An empty filter means everything, so `--list-entities` with no value works.
        assert_eq!(list_statistics(&conn, Some("  ")).unwrap().len(), 3);
    }

    #[test]
    fn candidates_carry_their_date_range() {
        let candidates = list_statistics(&database(), None).unwrap();
        let power = find(&candidates, ENTITY);
        assert_eq!(power.first_ts, Some(ts("2025-06-01 00:00:00")));
        assert_eq!(power.last_ts, Some(ts("2025-06-01 09:00:00")));
        assert_eq!(date_range(power, UTC), "2025-06-01 to 2025-06-01");
    }

    #[test]
    fn a_statistic_without_rows_is_listed_as_empty() {
        let conn = Connection::open_in_memory().unwrap();
        testdb::create_schema(&conn, Flavour::Modern);
        testdb::insert_statistic_meta(&conn, ENTITY, Some("W"));
        let candidates = list_statistics(&conn, None).unwrap();

        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].kind, StatisticKind::Empty);
        assert_eq!(candidates[0].rows, 0);
        assert!(!candidates[0].is_plottable());
        assert_eq!(date_range(&candidates[0], UTC), "no rows");
    }

    #[test]
    fn a_database_without_statistics_lists_nothing() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE unrelated (id INTEGER)")
            .unwrap();
        assert!(list_statistics(&conn, None).unwrap().is_empty());
    }

    #[test]
    fn the_legacy_schema_is_listed_too() {
        let conn = testdb::legacy_database();
        let candidates = list_statistics(&conn, None).unwrap();
        let power = find(&candidates, ENTITY);
        assert_eq!(power.rows, 4, "text timestamps and all");
        assert_eq!(power.kind, StatisticKind::Mean);
        assert_eq!(power.first_ts, Some(ts("2024-06-21 09:00:00")));
    }

    #[test]
    fn the_closest_power_sensor_is_the_one_sharing_a_prefix() {
        let conn = Connection::open_in_memory().unwrap();
        testdb::create_schema(&conn, Flavour::Modern);
        let base = ts("2025-06-01 00:00:00");
        for (index, id) in [
            "sensor.roof_array_power",
            "sensor.eve_energy_20ecm8301_power",
        ]
        .into_iter()
        .enumerate()
        {
            let metadata_id = testdb::insert_statistic_meta(&conn, id, Some("W"));
            // The unrelated sensor has far more rows, so only the prefix can win.
            for hour in 0..(50 - index as i64 * 40) {
                testdb::insert_statistics_row(
                    &conn,
                    Flavour::Modern,
                    "statistics",
                    metadata_id,
                    base + hour * 3_600,
                    Some(500.0),
                );
            }
        }

        let candidates = list_statistics(&conn, None).unwrap();
        let closest = closest_power_sensors(&candidates, "sensor.eve_energy_20ecm8301_energy", 2);
        assert_eq!(closest[0].statistic_id, "sensor.eve_energy_20ecm8301_power");
        assert_eq!(closest.len(), 2);

        // Only plottable sensors are ever suggested.
        assert!(closest.iter().all(Candidate::is_plottable));
    }

    #[test]
    fn formatting_aligns_columns_and_caps_the_tail() {
        let candidates = list_statistics(&database(), None).unwrap();
        let table = format_candidates(&candidates, UTC, 2);

        let lines: Vec<&str> = table.lines().collect();
        assert_eq!(lines.len(), 3, "two rows and a tail: {table}");
        assert!(lines[0].contains(ENTITY));
        assert!(lines[0].contains(" W "), "the unit is shown: {}", lines[0]);
        assert!(lines[0].contains("10 rows"));
        assert!(lines[2].contains("and 1 more"));

        // Columns line up, so the ids are padded to a common width.
        let unit_at = |line: &str| line.find(" W ").or_else(|| line.find(" kWh ")).unwrap();
        assert_eq!(unit_at(lines[0]), unit_at(lines[1]));
    }

    #[test]
    fn formatting_an_empty_list_says_so() {
        assert_eq!(format_candidates(&[], UTC, 10), "  (none)\n");
    }

    #[test]
    fn the_listing_separates_power_from_counters() {
        let listing = format_listing(&list_statistics(&database(), None).unwrap(), UTC, 20);
        assert!(listing.contains("Power sensors this tool can plot:"));
        assert!(listing.contains("Energy counters"));
        assert!(listing.contains("Other statistics:"));

        // The power sensor is above the counter, which is above the thermometer.
        let position = |needle: &str| listing.find(needle).expect("listed");
        assert!(position(ENTITY) < position("sensor.solar_energy"));
        assert!(position("sensor.solar_energy") < position("sensor.attic_temp"));
    }
}
