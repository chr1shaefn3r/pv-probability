//! Reading Home Assistant's long-term and short-term statistics tables.
//!
//! These tables are what makes a full year of history available: the recorder keeps raw
//! states for about ten days by default, but hourly statistics for as long as the
//! database lives.

use anyhow::{Context, Result};
use rusqlite::Connection;
use rusqlite::types::Value;

use crate::model::Sample;
use crate::source::schema::{
    self, StatisticsTable, TimeColumn, ValueColumn, find_statistic, not_found_error,
    statistics_layout, unit_scale_to_watts, value_to_f64, value_to_timestamp,
};

/// What to read out of a statistics table.
#[derive(Debug, Clone)]
pub struct Request<'a> {
    pub statistic_id: &'a str,
    pub table: StatisticsTable,
    pub value: ValueColumn,
    pub from: Option<i64>,
    pub to: Option<i64>,
    /// Explicit watt scaling; `None` derives it from the recorded unit.
    pub scale: Option<f64>,
    pub clamp_negative: bool,
}

/// Samples plus what was learned about them on the way.
#[derive(Debug, Clone, PartialEq)]
pub struct Loaded {
    pub samples: Vec<Sample>,
    pub unit: Option<String>,
    pub scale: f64,
    /// Rows the query returned, including ones dropped for being NULL.
    pub rows: usize,
}

/// How many rows a statistic has in a table; used to resolve `--source auto`.
pub fn row_count(conn: &Connection, table: StatisticsTable, metadata_id: i64) -> Result<i64> {
    let sql = format!(
        "SELECT COUNT(*) FROM \"{}\" WHERE metadata_id = ?1",
        table.table_name()
    );
    let count = conn
        .query_row(&sql, [metadata_id], |row| row.get(0))
        .with_context(|| format!("failed to count rows in {}", table.table_name()))?;
    Ok(count)
}

/// Load statistics rows as time-weighted samples.
pub fn load(conn: &Connection, request: &Request<'_>) -> Result<Loaded> {
    let time = statistics_layout(conn, request.table, request.value)?;
    let meta = find_statistic(conn, request.statistic_id)?.ok_or_else(|| {
        not_found_error(
            "statistic",
            request.statistic_id,
            &schema::suggest_ids(
                conn,
                "statistics_meta",
                "statistic_id",
                request.statistic_id,
            ),
        )
    })?;

    let scale = resolve_scale(request.scale, meta.unit.as_deref());
    let interval = request.table.interval_seconds();
    let table_name = request.table.table_name();
    let value_column = request.value.column_name();
    let time_column = time.name();

    // A row that starts just before the window can still reach into it.
    let (where_clause, params): (String, Vec<Value>) = if time.supports_range_pushdown() {
        let mut clause = String::new();
        let mut params = vec![Value::Integer(meta.metadata_id)];
        if let Some(from) = request.from {
            params.push(Value::Integer(from - interval));
            clause.push_str(&format!(" AND \"{time_column}\" >= ?{}", params.len()));
        }
        if let Some(to) = request.to {
            params.push(Value::Integer(to));
            clause.push_str(&format!(" AND \"{time_column}\" < ?{}", params.len()));
        }
        (clause, params)
    } else {
        (String::new(), vec![Value::Integer(meta.metadata_id)])
    };

    let sql = format!(
        "SELECT \"{time_column}\", \"{value_column}\" FROM \"{table_name}\" \
         WHERE metadata_id = ?1{where_clause} ORDER BY \"{time_column}\""
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(params), |row| {
        Ok((row.get::<_, Value>(0)?, row.get::<_, Value>(1)?))
    })?;

    let mut samples = Vec::new();
    let mut row_count = 0usize;
    for row in rows {
        let (raw_time, raw_value) = row?;
        row_count += 1;
        let (Some(start), Some(value)) = (value_to_timestamp(&raw_time), value_to_f64(&raw_value))
        else {
            // NULL means the sensor reported nothing that hour; there is no reading to
            // weight, so the hour simply contributes no observation time.
            continue;
        };
        let watts = scale_watts(value, scale, request.clamp_negative);
        if let Some(sample) =
            Sample::new(start, start + interval, watts).clipped(request.from, request.to)
        {
            samples.push(sample);
        }
    }

    Ok(Loaded {
        samples,
        unit: meta.unit,
        scale,
        rows: row_count,
    })
}

/// Explicit `--scale` wins; otherwise a recognised unit is converted to watts and
/// anything else is left alone.
pub fn resolve_scale(explicit: Option<f64>, unit: Option<&str>) -> f64 {
    explicit
        .or_else(|| unit.and_then(unit_scale_to_watts))
        .unwrap_or(1.0)
}

/// Apply the unit scale and the negative-reading policy.
pub fn scale_watts(value: f64, scale: f64, clamp_negative: bool) -> f64 {
    let scaled = value * scale;
    if clamp_negative {
        scaled.max(0.0)
    } else {
        scaled
    }
}

/// Timestamp column used for a statistics table, exposed for diagnostics.
pub fn time_column(
    conn: &Connection,
    table: StatisticsTable,
    value: ValueColumn,
) -> Result<TimeColumn> {
    statistics_layout(conn, table, value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::testdb::{self, ENTITY, Flavour, ts};

    fn request<'a>(statistic_id: &'a str) -> Request<'a> {
        Request {
            statistic_id,
            table: StatisticsTable::LongTerm,
            value: ValueColumn::Mean,
            from: None,
            to: None,
            scale: None,
            clamp_negative: true,
        }
    }

    #[test]
    fn reads_hourly_statistics_as_hour_long_samples() {
        let db = testdb::modern_database();
        let loaded = load(&db, &request(ENTITY)).unwrap();

        // Four rows, one of which has a NULL mean.
        assert_eq!(loaded.rows, 4);
        assert_eq!(loaded.samples.len(), 3);
        assert_eq!(loaded.unit.as_deref(), Some("W"));
        assert_eq!(loaded.scale, 1.0);

        let first = loaded.samples[0];
        assert_eq!(first.start_ts, ts("2024-06-21 09:00:00"));
        assert_eq!(first.end_ts, ts("2024-06-21 10:00:00"));
        assert_eq!(first.watts, 1_000.0);
        assert!(
            loaded
                .samples
                .iter()
                .all(|sample| sample.duration() == 3_600)
        );
    }

    #[test]
    fn reads_the_legacy_text_timestamp_schema() {
        let db = testdb::legacy_database();
        let loaded = load(&db, &request(ENTITY)).unwrap();
        assert_eq!(loaded.samples.len(), 3);
        assert_eq!(loaded.samples[0].start_ts, ts("2024-06-21 09:00:00"));
        assert_eq!(loaded.samples[0].watts, 1_000.0);
    }

    #[test]
    fn reads_short_term_statistics_as_five_minute_samples() {
        let db = testdb::modern_database();
        let mut request = request(ENTITY);
        request.table = StatisticsTable::ShortTerm;
        let loaded = load(&db, &request).unwrap();
        assert_eq!(loaded.samples.len(), 3);
        assert!(loaded.samples.iter().all(|sample| sample.duration() == 300));
    }

    #[test]
    fn honours_the_stat_column() {
        let db = testdb::modern_database();
        let mut request = request(ENTITY);
        request.value = ValueColumn::Max;
        let loaded = load(&db, &request).unwrap();
        // The builder stores max as 1.5x the mean.
        assert_eq!(loaded.samples[0].watts, 1_500.0);

        request.value = ValueColumn::Min;
        let loaded = load(&db, &request).unwrap();
        assert_eq!(loaded.samples[0].watts, 500.0);
    }

    #[test]
    fn clips_samples_to_the_requested_window() {
        let db = testdb::modern_database();
        let mut request = request(ENTITY);
        // Half way through the first hour to half way through the second.
        request.from = Some(ts("2024-06-21 09:30:00"));
        request.to = Some(ts("2024-06-21 10:30:00"));
        let loaded = load(&db, &request).unwrap();

        assert_eq!(loaded.samples.len(), 2);
        assert_eq!(loaded.samples[0].start_ts, ts("2024-06-21 09:30:00"));
        assert_eq!(loaded.samples[0].duration(), 1_800);
        assert_eq!(loaded.samples[1].end_ts, ts("2024-06-21 10:30:00"));
        assert_eq!(loaded.samples[1].duration(), 1_800);
    }

    #[test]
    fn clips_the_legacy_schema_without_query_pushdown() {
        let db = testdb::legacy_database();
        let mut request = request(ENTITY);
        request.from = Some(ts("2024-06-21 10:30:00"));
        let loaded = load(&db, &request).unwrap();
        assert_eq!(loaded.samples.len(), 2);
        assert_eq!(loaded.samples[0].start_ts, ts("2024-06-21 10:30:00"));
    }

    #[test]
    fn converts_kilowatt_sensors_to_watts() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        testdb::create_schema(&conn, Flavour::Modern);
        let metadata_id = testdb::insert_statistic_meta(&conn, ENTITY, Some("kW"));
        testdb::insert_statistics_row(
            &conn,
            Flavour::Modern,
            "statistics",
            metadata_id,
            ts("2024-06-21 09:00:00"),
            Some(3.2),
        );

        let loaded = load(&conn, &request(ENTITY)).unwrap();
        assert_eq!(loaded.scale, 1_000.0);
        assert!((loaded.samples[0].watts - 3_200.0).abs() < 1e-9);

        // An explicit scale overrides the unit.
        let mut explicit = request(ENTITY);
        explicit.scale = Some(1.0);
        let loaded = load(&conn, &explicit).unwrap();
        assert_eq!(loaded.samples[0].watts, 3.2);
    }

    #[test]
    fn clamps_negative_readings_when_asked() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        testdb::create_schema(&conn, Flavour::Modern);
        let metadata_id = testdb::insert_statistic_meta(&conn, ENTITY, Some("W"));
        testdb::insert_statistics_row(
            &conn,
            Flavour::Modern,
            "statistics",
            metadata_id,
            ts("2024-06-21 02:00:00"),
            Some(-12.0),
        );

        assert_eq!(load(&conn, &request(ENTITY)).unwrap().samples[0].watts, 0.0);

        let mut keep_negative = request(ENTITY);
        keep_negative.clamp_negative = false;
        assert_eq!(load(&conn, &keep_negative).unwrap().samples[0].watts, -12.0);
    }

    #[test]
    fn unknown_statistics_produce_a_helpful_error() {
        let db = testdb::modern_database();
        let error = load(&db, &request("sensor.solar_powr"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("sensor.solar_powr"), "{error}");
        assert!(error.contains("did you mean"), "{error}");
        assert!(error.contains(ENTITY), "{error}");
    }

    #[test]
    fn counts_rows_per_table() {
        let db = testdb::modern_database();
        let meta = find_statistic(&db, ENTITY).unwrap().unwrap();
        assert_eq!(
            row_count(&db, StatisticsTable::LongTerm, meta.metadata_id).unwrap(),
            4
        );
        assert_eq!(
            row_count(&db, StatisticsTable::ShortTerm, meta.metadata_id).unwrap(),
            4
        );
        assert_eq!(row_count(&db, StatisticsTable::LongTerm, 4_242).unwrap(), 0);
    }

    #[test]
    fn scale_resolution_prefers_the_explicit_value() {
        assert_eq!(resolve_scale(Some(2.0), Some("kW")), 2.0);
        assert_eq!(resolve_scale(None, Some("kW")), 1_000.0);
        assert_eq!(resolve_scale(None, Some("bananas")), 1.0);
        assert_eq!(resolve_scale(None, None), 1.0);
    }

    #[test]
    fn scaling_applies_the_negative_policy() {
        assert_eq!(scale_watts(1.5, 1_000.0, true), 1_500.0);
        assert_eq!(scale_watts(-5.0, 1.0, true), 0.0);
        assert_eq!(scale_watts(-5.0, 1.0, false), -5.0);
    }
}
