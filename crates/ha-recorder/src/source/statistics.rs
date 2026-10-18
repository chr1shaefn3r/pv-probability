//! Reading Home Assistant's long-term and short-term statistics tables.
//!
//! These tables are what makes a full year of history available: the recorder keeps raw
//! states for about ten days by default, but hourly statistics for as long as the
//! database lives.

use anyhow::{Context, Result};
use rusqlite::Connection;
use rusqlite::types::Value;

use crate::model::Sample;
use crate::quantity::{Conversion, Quantity};
use crate::source::schema::{
    self, StatisticsTable, TimeColumn, ValueColumn, find_statistic, not_found_error,
    statistics_layout, value_to_f64, value_to_timestamp,
};
use crate::source::slices::{self, Slice};

/// What to read out of a statistics table.
#[derive(Debug, Clone)]
pub struct Request<'a> {
    pub statistic_id: &'a str,
    pub table: StatisticsTable,
    pub value: ValueColumn,
    pub from: Option<i64>,
    pub to: Option<i64>,
    /// What the statistic should measure, which decides the unit conversion.
    pub quantity: Quantity,
    /// Explicit scaling; `None` derives the conversion from the recorded unit.
    pub scale: Option<f64>,
    pub clamp_negative: bool,
    /// Only used to state dates in error messages.
    pub tz: chrono_tz::Tz,
}

/// Samples plus what was learned about them on the way.
#[derive(Debug, Clone, PartialEq)]
pub struct Loaded {
    pub samples: Vec<Sample>,
    pub unit: Option<String>,
    pub conversion: Conversion,
    /// Rows the query returned, including ones dropped for being NULL.
    pub rows: usize,
}

/// How many rows a statistic has in a table.
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

/// Whether a statistic has any row in a table; used to resolve `--source auto`.
///
/// Counting them would walk every index entry the statistic has, which for years of
/// hourly rows costs a good part of what reading them does.
pub fn has_rows(conn: &Connection, table: StatisticsTable, metadata_id: i64) -> Result<bool> {
    let sql = format!(
        "SELECT EXISTS (SELECT 1 FROM \"{}\" WHERE metadata_id = ?1)",
        table.table_name()
    );
    let exists = conn
        .query_row(&sql, [metadata_id], |row| row.get(0))
        .with_context(|| format!("failed to look for rows in {}", table.table_name()))?;
    Ok(exists)
}

/// Load statistics rows as time-weighted samples.
///
/// A long history is read in time slices, each on a connection of its own, in parallel,
/// and put back together in order - the same rows, in the same order, as one query.
pub fn load(conn: &Connection, request: &Request<'_>) -> Result<Loaded> {
    let time = statistics_layout(conn, request.table, request.value)?;
    let meta = find_statistic(conn, request.statistic_id)?.ok_or_else(|| {
        anyhow::anyhow!(
            "{}{}",
            not_found_error(
                "statistic",
                request.statistic_id,
                &schema::suggest_ids(
                    conn,
                    "statistics_meta",
                    "statistic_id",
                    request.statistic_id,
                ),
            ),
            crate::source::catalog::suggest_block(
                conn,
                request.statistic_id,
                request.quantity,
                request.tz
            )
        )
    })?;

    let conversion = resolve_conversion(request.quantity, request.scale, meta.unit.as_deref())
        .ok_or_else(|| {
            wrong_unit_error(
                conn,
                request.statistic_id,
                meta.unit.as_deref().unwrap_or_default(),
                request.quantity,
                request.tz,
            )
        })?;
    let interval = request.table.interval_seconds();
    let table_name = request.table.table_name();
    let value_column = request.value.column_name();
    let time_column = time.name();
    let key = Value::Integer(meta.metadata_id);

    // A row that starts just before the window can still reach into it. Text timestamps
    // cannot be compared in SQL, so a legacy table is read whole, in one query, and
    // clipped below.
    let slices = if time.supports_range_pushdown() {
        let window = Slice::window(request.from.map(|from| from - interval), request.to);
        match slices::time_bounds(conn, table_name, "metadata_id", &key, time_column, window)? {
            Some((first, last)) => slices::plan(
                window,
                first,
                last,
                (last - first) / interval + 1,
                rayon::current_num_threads(),
            ),
            None => vec![window],
        }
    } else {
        vec![Slice::window(None, None)]
    };

    let parts = slices::read_all(conn, &slices, |conn, slice| {
        let mut params = vec![key.clone()];
        let condition = slice.condition(time_column, &mut params);
        let sql = format!(
            "SELECT \"{time_column}\", \"{value_column}\" FROM \"{table_name}\" \
             WHERE metadata_id = ?1{condition} ORDER BY \"{time_column}\""
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
            let (Some(start), Some(value)) =
                (value_to_timestamp(&raw_time), value_to_f64(&raw_value))
            else {
                // NULL means the sensor reported nothing that hour; there is no reading to
                // weight, so the hour simply contributes no observation time.
                continue;
            };
            let value = convert(value, conversion, request.clamp_negative);
            if let Some(sample) =
                Sample::new(start, start + interval, value).clipped(request.from, request.to)
            {
                samples.push(sample);
            }
        }
        Ok((samples, row_count))
    })?;

    let rows = parts.iter().map(|(_, rows)| rows).sum();
    let mut samples = Vec::with_capacity(parts.iter().map(|(samples, _)| samples.len()).sum());
    for (part, _) in parts {
        samples.extend(part);
    }
    Ok(Loaded {
        samples,
        unit: meta.unit,
        conversion,
        rows,
    })
}

/// How readings recorded in `unit` become `quantity`'s working unit.
///
/// An explicit `--scale` wins, and is a plain multiplication. Otherwise a unit of the
/// quantity converts, and a unit nobody recognises is taken as already right. `None`
/// means the unit belongs to a *different* quantity - a thermometer handed to a power
/// tool - which no conversion could make sense of.
pub fn resolve_conversion(
    quantity: Quantity,
    explicit: Option<f64>,
    unit: Option<&str>,
) -> Option<Conversion> {
    if let Some(scale) = explicit {
        return Some(Conversion::factor(scale));
    }
    let Some(unit) = unit else {
        return Some(Conversion::IDENTITY);
    };
    match quantity.conversion(unit) {
        Some(conversion) => Some(conversion),
        None if Quantity::of_unit(unit).is_some() => None,
        None => Some(Conversion::IDENTITY),
    }
}

/// The error for a statistic recorded in another quantity's unit.
fn wrong_unit_error(
    conn: &Connection,
    statistic_id: &str,
    unit: &str,
    quantity: Quantity,
    tz: chrono_tz::Tz,
) -> anyhow::Error {
    let measures = Quantity::of_unit(unit).map_or("something else", Quantity::name);
    anyhow::anyhow!(
        "`{statistic_id}` is recorded in {unit}, which measures {measures}, but a {} is \
         needed here. Pass --scale to read it anyway.{}",
        quantity.sensor_noun(),
        crate::source::catalog::suggest_block(conn, statistic_id, quantity, tz)
    )
}

/// Apply the unit conversion and the negative-reading policy.
pub fn convert(value: f64, conversion: Conversion, clamp_negative: bool) -> f64 {
    let converted = conversion.apply(value);
    if clamp_negative {
        converted.max(0.0)
    } else {
        converted
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
    use ha_testdb::{self as testdb, ENTITY, Flavour, ts};

    fn request<'a>(statistic_id: &'a str) -> Request<'a> {
        Request {
            statistic_id,
            table: StatisticsTable::LongTerm,
            value: ValueColumn::Mean,
            from: None,
            to: None,
            quantity: Quantity::Power,
            scale: None,
            clamp_negative: true,
            tz: chrono_tz::Tz::UTC,
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
        assert_eq!(loaded.conversion, Conversion::IDENTITY);

        let first = loaded.samples[0];
        assert_eq!(first.start_ts, ts("2024-06-21 09:00:00"));
        assert_eq!(first.end_ts, ts("2024-06-21 10:00:00"));
        assert_eq!(first.value, 1_000.0);
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
        assert_eq!(loaded.samples[0].value, 1_000.0);
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
        assert_eq!(loaded.samples[0].value, 1_500.0);

        request.value = ValueColumn::Min;
        let loaded = load(&db, &request).unwrap();
        assert_eq!(loaded.samples[0].value, 500.0);
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
        assert_eq!(loaded.conversion, Conversion::factor(1_000.0));
        assert!((loaded.samples[0].value - 3_200.0).abs() < 1e-9);

        // An explicit scale overrides the unit.
        let mut explicit = request(ENTITY);
        explicit.scale = Some(1.0);
        let loaded = load(&conn, &explicit).unwrap();
        assert_eq!(loaded.samples[0].value, 3.2);
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

        assert_eq!(load(&conn, &request(ENTITY)).unwrap().samples[0].value, 0.0);

        let mut keep_negative = request(ENTITY);
        keep_negative.clamp_negative = false;
        assert_eq!(load(&conn, &keep_negative).unwrap().samples[0].value, -12.0);
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

    /// A database holding one hourly mean for `id`, recorded in `unit`.
    fn one_reading(id: &str, unit: &str, mean: f64) -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        testdb::create_schema(&conn, Flavour::Modern);
        let metadata_id = testdb::insert_statistic_meta(&conn, id, Some(unit));
        testdb::insert_statistics_row(
            &conn,
            Flavour::Modern,
            "statistics",
            metadata_id,
            ts("2024-01-10 06:00:00"),
            Some(mean),
        );
        conn
    }

    #[test]
    fn converts_fahrenheit_thermometers_to_celsius() {
        let conn = one_reading("sensor.outdoor_temperature", "°F", 23.0);
        let mut request = request("sensor.outdoor_temperature");
        request.quantity = Quantity::Temperature;
        request.clamp_negative = false;
        let loaded = load(&conn, &request).unwrap();
        assert!(
            (loaded.samples[0].value + 5.0).abs() < 1e-9,
            "23 °F is -5 °C"
        );
        assert!(!loaded.conversion.is_identity());
    }

    #[test]
    fn a_sensor_of_another_quantity_is_refused_by_name() {
        let conn = one_reading("sensor.outdoor_temperature", "°C", 4.0);
        let error = load(&conn, &request("sensor.outdoor_temperature"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("recorded in °C"), "{error}");
        assert!(error.contains("measures temperature"), "{error}");
        assert!(error.contains("a power sensor is needed"), "{error}");

        // --scale is the way out for anyone who really means it.
        let mut explicit = request("sensor.outdoor_temperature");
        explicit.scale = Some(1.0);
        assert_eq!(load(&conn, &explicit).unwrap().samples[0].value, 4.0);
    }

    #[test]
    fn conversion_prefers_the_explicit_scale_then_the_unit() {
        let power = Quantity::Power;
        assert_eq!(
            resolve_conversion(power, Some(2.0), Some("kW")),
            Some(Conversion::factor(2.0))
        );
        assert_eq!(
            resolve_conversion(power, None, Some("kW")),
            Some(Conversion::factor(1_000.0))
        );
        assert_eq!(
            resolve_conversion(power, None, Some("bananas")),
            Some(Conversion::IDENTITY),
            "a unit nobody knows is taken at its word"
        );
        assert_eq!(
            resolve_conversion(power, None, None),
            Some(Conversion::IDENTITY)
        );
        assert_eq!(resolve_conversion(power, None, Some("°C")), None);
        assert_eq!(
            resolve_conversion(Quantity::Temperature, None, Some("kW")),
            None
        );
    }

    #[test]
    fn conversion_applies_the_negative_policy() {
        assert_eq!(convert(1.5, Conversion::factor(1_000.0), true), 1_500.0);
        assert_eq!(convert(-5.0, Conversion::IDENTITY, true), 0.0);
        assert_eq!(convert(-5.0, Conversion::IDENTITY, false), -5.0);
    }
}
