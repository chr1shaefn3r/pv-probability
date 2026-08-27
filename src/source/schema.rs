//! Probing the recorder database layout.
//!
//! Home Assistant has reshaped the recorder schema several times: timestamps moved from
//! text columns (`start`, `last_updated`) to unix epoch floats (`start_ts`,
//! `last_updated_ts`), and `states.entity_id` was replaced by a `metadata_id` foreign key
//! into `states_meta`. Rather than assuming a version, everything here is discovered with
//! `sqlite_master` and `PRAGMA table_info`.

use anyhow::{Context, Result, anyhow, bail};
use rusqlite::Connection;
use rusqlite::types::Value;

use crate::timeutil::parse_ha_datetime;

/// A column holding a point in time, and how to read it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeColumn {
    /// Unix epoch seconds, stored as REAL or INTEGER (modern schemas).
    Epoch(&'static str),
    /// UTC timestamp text such as `2023-06-01 12:00:00.000000` (legacy schemas).
    Text(&'static str),
}

impl TimeColumn {
    pub fn name(self) -> &'static str {
        match self {
            TimeColumn::Epoch(name) | TimeColumn::Text(name) => name,
        }
    }

    /// True when the column can be compared against unix timestamps in SQL, which lets
    /// the date range filter be pushed down into the query.
    pub fn supports_range_pushdown(self) -> bool {
        matches!(self, TimeColumn::Epoch(_))
    }
}

/// Which statistics table to read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatisticsTable {
    /// `statistics`: one row per hour, kept for years.
    LongTerm,
    /// `statistics_short_term`: one row per five minutes, kept for ~10 days.
    ShortTerm,
}

impl StatisticsTable {
    pub fn table_name(self) -> &'static str {
        match self {
            StatisticsTable::LongTerm => "statistics",
            StatisticsTable::ShortTerm => "statistics_short_term",
        }
    }

    /// How long one row of this table is in effect, in seconds.
    pub fn interval_seconds(self) -> i64 {
        match self {
            StatisticsTable::LongTerm => 3_600,
            StatisticsTable::ShortTerm => 300,
        }
    }
}

/// Which aggregate of an hour of readings to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum ValueColumn {
    Mean,
    Min,
    Max,
}

impl ValueColumn {
    pub fn column_name(self) -> &'static str {
        match self {
            ValueColumn::Mean => "mean",
            ValueColumn::Min => "min",
            ValueColumn::Max => "max",
        }
    }
}

/// How raw states are keyed to an entity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatesLayout {
    /// Modern: `states.metadata_id` -> `states_meta.entity_id`.
    MetadataId { time: TimeColumn },
    /// Legacy: `states.entity_id` holds the entity directly.
    InlineEntityId { time: TimeColumn },
}

impl StatesLayout {
    pub fn time(self) -> TimeColumn {
        match self {
            StatesLayout::MetadataId { time } | StatesLayout::InlineEntityId { time } => time,
        }
    }
}

/// Does a table (or view) exist?
pub fn table_exists(conn: &Connection, name: &str) -> Result<bool> {
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type IN ('table', 'view') AND name = ?1",
            [name],
            |row| row.get(0),
        )
        .with_context(|| format!("failed to look up table {name}"))?;
    Ok(count > 0)
}

/// Column names of a table, lower-cased.
pub fn column_names(conn: &Connection, table: &str) -> Result<Vec<String>> {
    let mut stmt = conn
        .prepare(&format!(
            "PRAGMA table_info(\"{}\")",
            table.replace('"', "")
        ))
        .with_context(|| format!("failed to inspect table {table}"))?;
    let names = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<String>>>()?
        .into_iter()
        .map(|name| name.to_ascii_lowercase())
        .collect();
    Ok(names)
}

/// Choose the first available timestamp column from a preference list.
pub fn pick_time_column(columns: &[String], candidates: &[TimeColumn]) -> Option<TimeColumn> {
    candidates
        .iter()
        .copied()
        .find(|candidate| columns.iter().any(|column| column == candidate.name()))
}

/// Work out how to read a statistics table.
pub fn statistics_layout(
    conn: &Connection,
    table: StatisticsTable,
    value: ValueColumn,
) -> Result<TimeColumn> {
    let name = table.table_name();
    if !table_exists(conn, name)? {
        bail!(
            "this database has no `{name}` table - it does not look like a Home Assistant recorder database"
        );
    }
    let columns = column_names(conn, name)?;
    if !columns.iter().any(|column| column == value.column_name()) {
        bail!(
            "table `{name}` has no `{}` column; try a different --stat",
            value.column_name()
        );
    }
    pick_time_column(
        &columns,
        &[TimeColumn::Epoch("start_ts"), TimeColumn::Text("start")],
    )
    .ok_or_else(|| anyhow!("table `{name}` has neither a `start_ts` nor a `start` column"))
}

/// Work out how raw states are stored.
pub fn states_layout(conn: &Connection) -> Result<StatesLayout> {
    if !table_exists(conn, "states")? {
        bail!("this database has no `states` table");
    }
    let columns = column_names(conn, "states")?;
    let time = pick_time_column(
        &columns,
        &[
            TimeColumn::Epoch("last_updated_ts"),
            TimeColumn::Text("last_updated"),
            TimeColumn::Epoch("last_changed_ts"),
            TimeColumn::Text("last_changed"),
        ],
    )
    .ok_or_else(|| anyhow!("table `states` has no recognisable timestamp column"))?;

    if columns.iter().any(|column| column == "metadata_id") && table_exists(conn, "states_meta")? {
        Ok(StatesLayout::MetadataId { time })
    } else if columns.iter().any(|column| column == "entity_id") {
        Ok(StatesLayout::InlineEntityId { time })
    } else {
        bail!("table `states` has neither a `metadata_id` nor an `entity_id` column")
    }
}

/// Metadata of a long-term statistic.
#[derive(Debug, Clone, PartialEq)]
pub struct StatisticMeta {
    pub metadata_id: i64,
    pub statistic_id: String,
    pub unit: Option<String>,
}

/// Resolve a `statistic_id` (e.g. `sensor.solar_power`) to its metadata row.
pub fn find_statistic(conn: &Connection, statistic_id: &str) -> Result<Option<StatisticMeta>> {
    if !table_exists(conn, "statistics_meta")? {
        bail!("this database has no `statistics_meta` table");
    }
    let has_unit = column_names(conn, "statistics_meta")?
        .iter()
        .any(|column| column == "unit_of_measurement");
    let sql = if has_unit {
        "SELECT id, statistic_id, unit_of_measurement FROM statistics_meta WHERE statistic_id = ?1"
    } else {
        "SELECT id, statistic_id, NULL FROM statistics_meta WHERE statistic_id = ?1"
    };
    let mut stmt = conn.prepare(sql)?;
    let mut rows = stmt.query_map([statistic_id], |row| {
        Ok(StatisticMeta {
            metadata_id: row.get(0)?,
            statistic_id: row.get(1)?,
            unit: row.get(2)?,
        })
    })?;
    match rows.next() {
        Some(meta) => Ok(Some(meta?)),
        None => Ok(None),
    }
}

/// Resolve an `entity_id` to the `states_meta` row id used by modern schemas.
pub fn find_state_metadata_id(conn: &Connection, entity_id: &str) -> Result<Option<i64>> {
    if !table_exists(conn, "states_meta")? {
        return Ok(None);
    }
    let mut stmt = conn.prepare("SELECT metadata_id FROM states_meta WHERE entity_id = ?1")?;
    let mut rows = stmt.query_map([entity_id], |row| row.get::<_, i64>(0))?;
    match rows.next() {
        Some(id) => Ok(Some(id?)),
        None => Ok(None),
    }
}

/// Ids similar to `needle`, to put in an error message when nothing matched.
///
/// Tries the whole id first, then the part after the domain (`sensor.`), then the
/// longest word in it, so `sensor.solar_power` still finds `sensor.pv_solar_total`.
pub fn suggest_ids(conn: &Connection, table: &str, column: &str, needle: &str) -> Vec<String> {
    let mut patterns = vec![needle.to_string()];
    if let Some((_, rest)) = needle.split_once('.') {
        patterns.push(rest.to_string());
    }
    if let Some(word) = needle
        .split(['.', '_', '-', ' '])
        .max_by_key(|word| word.len())
        .filter(|word| word.len() >= 3)
    {
        patterns.push(word.to_string());
    }

    for pattern in patterns {
        let sql = format!(
            "SELECT DISTINCT \"{column}\" FROM \"{table}\" WHERE \"{column}\" LIKE ?1 ORDER BY \"{column}\" LIMIT 10"
        );
        let Ok(mut stmt) = conn.prepare(&sql) else {
            return Vec::new();
        };
        let Ok(rows) = stmt.query_map([format!("%{pattern}%")], |row| row.get::<_, String>(0))
        else {
            return Vec::new();
        };
        let matches: Vec<String> = rows.flatten().collect();
        if !matches.is_empty() {
            return matches;
        }
    }
    Vec::new()
}

/// Build the "I could not find that entity" error, with suggestions where possible.
pub fn not_found_error(kind: &str, needle: &str, suggestions: &[String]) -> anyhow::Error {
    if suggestions.is_empty() {
        anyhow!("no {kind} named {needle:?} in this database")
    } else {
        anyhow!(
            "no {kind} named {needle:?} in this database; did you mean one of:\n  {}",
            suggestions.join("\n  ")
        )
    }
}

/// Factor converting a recorder unit into watts, if it is a power unit we know.
pub fn unit_scale_to_watts(unit: &str) -> Option<f64> {
    match unit.trim() {
        "W" => Some(1.0),
        "kW" => Some(1_000.0),
        "MW" => Some(1_000_000.0),
        "mW" => Some(0.001),
        _ => None,
    }
}

/// Interpret a stored timestamp, whatever type the column happens to hold.
pub fn value_to_timestamp(value: &Value) -> Option<i64> {
    match value {
        Value::Integer(seconds) => Some(*seconds),
        Value::Real(seconds) => seconds.is_finite().then(|| seconds.floor() as i64),
        Value::Text(text) => parse_ha_datetime(text),
        Value::Null | Value::Blob(_) => None,
    }
}

/// Interpret a stored reading, whatever type the column happens to hold.
pub fn value_to_f64(value: &Value) -> Option<f64> {
    match value {
        Value::Integer(number) => Some(*number as f64),
        Value::Real(number) => Some(*number),
        Value::Text(text) => text.trim().parse::<f64>().ok(),
        Value::Null | Value::Blob(_) => None,
    }
    .filter(|number| number.is_finite())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::testdb;

    #[test]
    fn detects_the_modern_statistics_layout() {
        let db = testdb::modern_database();
        assert_eq!(
            statistics_layout(&db, StatisticsTable::LongTerm, ValueColumn::Mean).unwrap(),
            TimeColumn::Epoch("start_ts")
        );
        assert_eq!(
            statistics_layout(&db, StatisticsTable::ShortTerm, ValueColumn::Mean).unwrap(),
            TimeColumn::Epoch("start_ts")
        );
        assert_eq!(
            states_layout(&db).unwrap(),
            StatesLayout::MetadataId {
                time: TimeColumn::Epoch("last_updated_ts")
            }
        );
    }

    #[test]
    fn detects_the_legacy_statistics_layout() {
        let db = testdb::legacy_database();
        assert_eq!(
            statistics_layout(&db, StatisticsTable::LongTerm, ValueColumn::Mean).unwrap(),
            TimeColumn::Text("start")
        );
        assert_eq!(
            states_layout(&db).unwrap(),
            StatesLayout::InlineEntityId {
                time: TimeColumn::Text("last_updated")
            }
        );
    }

    #[test]
    fn reports_a_database_that_is_not_a_recorder_database() {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE unrelated (id INTEGER)")
            .unwrap();
        let error = statistics_layout(&db, StatisticsTable::LongTerm, ValueColumn::Mean)
            .unwrap_err()
            .to_string();
        assert!(error.contains("statistics"), "unhelpful error: {error}");
        assert!(states_layout(&db).is_err());
    }

    #[test]
    fn rejects_a_stat_column_the_table_does_not_have() {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE statistics (id INTEGER, start_ts REAL, mean REAL)")
            .unwrap();
        let error = statistics_layout(&db, StatisticsTable::LongTerm, ValueColumn::Max)
            .unwrap_err()
            .to_string();
        assert!(error.contains("--stat"), "unhelpful error: {error}");
    }

    #[test]
    fn picks_time_columns_in_preference_order() {
        let columns = vec!["start".to_string(), "start_ts".to_string()];
        assert_eq!(
            pick_time_column(
                &columns,
                &[TimeColumn::Epoch("start_ts"), TimeColumn::Text("start")]
            ),
            Some(TimeColumn::Epoch("start_ts"))
        );
        assert_eq!(
            pick_time_column(&[], &[TimeColumn::Epoch("start_ts")]),
            None
        );
        assert!(TimeColumn::Epoch("start_ts").supports_range_pushdown());
        assert!(!TimeColumn::Text("start").supports_range_pushdown());
    }

    #[test]
    fn resolves_entities_in_both_layouts() {
        let db = testdb::modern_database();
        let meta = find_statistic(&db, testdb::ENTITY).unwrap().unwrap();
        assert_eq!(meta.statistic_id, testdb::ENTITY);
        assert_eq!(meta.unit.as_deref(), Some("W"));
        assert!(find_statistic(&db, "sensor.nope").unwrap().is_none());
        assert_eq!(
            find_state_metadata_id(&db, testdb::ENTITY).unwrap(),
            Some(1)
        );
        assert_eq!(find_state_metadata_id(&db, "sensor.nope").unwrap(), None);

        let legacy = testdb::legacy_database();
        assert!(find_statistic(&legacy, testdb::ENTITY).unwrap().is_some());
        assert_eq!(
            find_state_metadata_id(&legacy, testdb::ENTITY).unwrap(),
            None
        );
    }

    #[test]
    fn suggests_similar_ids() {
        let db = testdb::modern_database();
        let suggestions = suggest_ids(&db, "statistics_meta", "statistic_id", "sensor.solar_powr");
        assert!(
            suggestions.contains(&testdb::ENTITY.to_string()),
            "expected a suggestion, got {suggestions:?}"
        );

        // A needle with nothing in common yields nothing rather than noise.
        assert!(suggest_ids(&db, "statistics_meta", "statistic_id", "xyzzy").is_empty());
    }

    #[test]
    fn not_found_errors_mention_suggestions() {
        let plain = not_found_error("statistic", "sensor.a", &[]).to_string();
        assert!(plain.contains("sensor.a"));
        assert!(!plain.contains("did you mean"));

        let with_hint =
            not_found_error("statistic", "sensor.a", &["sensor.ab".to_string()]).to_string();
        assert!(with_hint.contains("did you mean"));
        assert!(with_hint.contains("sensor.ab"));
    }

    #[test]
    fn converts_known_power_units() {
        assert_eq!(unit_scale_to_watts("W"), Some(1.0));
        assert_eq!(unit_scale_to_watts(" kW "), Some(1_000.0));
        assert_eq!(unit_scale_to_watts("MW"), Some(1_000_000.0));
        assert_eq!(unit_scale_to_watts("kWh"), None);
        assert_eq!(unit_scale_to_watts(""), None);
    }

    #[test]
    fn reads_timestamps_and_values_of_any_storage_class() {
        assert_eq!(value_to_timestamp(&Value::Integer(120)), Some(120));
        assert_eq!(value_to_timestamp(&Value::Real(120.9)), Some(120));
        assert_eq!(
            value_to_timestamp(&Value::Text("1970-01-01 00:02:00".into())),
            Some(120)
        );
        assert_eq!(value_to_timestamp(&Value::Null), None);
        assert_eq!(value_to_timestamp(&Value::Real(f64::NAN)), None);

        assert_eq!(value_to_f64(&Value::Integer(7)), Some(7.0));
        assert_eq!(value_to_f64(&Value::Real(7.5)), Some(7.5));
        assert_eq!(value_to_f64(&Value::Text("7.5".into())), Some(7.5));
        assert_eq!(value_to_f64(&Value::Text("unavailable".into())), None);
        assert_eq!(value_to_f64(&Value::Null), None);
        assert_eq!(value_to_f64(&Value::Real(f64::INFINITY)), None);
    }
}
