//! Reading the raw `states` table.
//!
//! Rows here are written whenever the sensor changes, so they are not evenly spaced.
//! Counting rows would over-weight the volatile parts of the day; instead each reading is
//! weighted by how long it stayed in effect, up to `--max-gap` so that a recorder outage
//! does not get counted as hours of steady production.

use anyhow::Result;
use rusqlite::Connection;
use rusqlite::types::Value;

use crate::model::Sample;
use crate::source::schema::{
    self, StatesLayout, find_state_metadata_id, not_found_error, states_layout, value_to_f64,
    value_to_timestamp,
};
use crate::source::statistics::{Loaded, scale_watts};

/// What to read out of the states table.
#[derive(Debug, Clone)]
pub struct Request<'a> {
    pub entity_id: &'a str,
    pub from: Option<i64>,
    pub to: Option<i64>,
    /// Longest span a single reading may be assumed to remain valid, in seconds.
    pub max_gap: i64,
    pub scale: Option<f64>,
    pub clamp_negative: bool,
}

/// Load raw states as time-weighted samples.
pub fn load(conn: &Connection, request: &Request<'_>) -> Result<Loaded> {
    let layout = states_layout(conn)?;
    let time_column = layout.time().name();

    let (sql, params): (String, Vec<Value>) = match layout {
        StatesLayout::MetadataId { .. } => {
            let metadata_id =
                find_state_metadata_id(conn, request.entity_id)?.ok_or_else(|| {
                    not_found_error(
                        "entity",
                        request.entity_id,
                        &schema::suggest_ids(conn, "states_meta", "entity_id", request.entity_id),
                    )
                })?;
            (
                format!(
                    "SELECT \"{time_column}\", state FROM states WHERE metadata_id = ?1 \
                     ORDER BY \"{time_column}\""
                ),
                vec![Value::Integer(metadata_id)],
            )
        }
        StatesLayout::InlineEntityId { .. } => (
            format!(
                "SELECT \"{time_column}\", state FROM states WHERE entity_id = ?1 \
                 ORDER BY \"{time_column}\""
            ),
            vec![Value::Text(request.entity_id.to_string())],
        ),
    };

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(params), |row| {
        Ok((row.get::<_, Value>(0)?, row.get::<_, Value>(1)?))
    })?;

    let scale = request.scale.unwrap_or(1.0);
    let mut readings: Vec<(i64, Option<f64>)> = Vec::new();
    let mut row_count = 0usize;
    for row in rows {
        let (raw_time, raw_state) = row?;
        row_count += 1;
        let Some(ts) = value_to_timestamp(&raw_time) else {
            continue;
        };
        // `unknown`, `unavailable` and friends keep their timestamp: they end the
        // previous reading without contributing one of their own.
        let watts =
            value_to_f64(&raw_state).map(|value| scale_watts(value, scale, request.clamp_negative));
        readings.push((ts, watts));
    }

    if readings.is_empty() && row_count == 0 {
        // The entity resolved but has no rows at all; let the caller report that.
        return Ok(Loaded {
            samples: Vec::new(),
            unit: None,
            scale,
            rows: 0,
        });
    }

    readings.sort_by_key(|(ts, _)| *ts);
    let samples = samples_from_readings(&readings, request.max_gap)
        .into_iter()
        .filter_map(|sample| sample.clipped(request.from, request.to))
        .collect();

    Ok(Loaded {
        samples,
        unit: None,
        scale,
        rows: row_count,
    })
}

/// Turn timestamped readings into samples that each cover the time until the next
/// reading, capped at `max_gap` seconds.
///
/// Readings whose value is `None` (`unavailable`, `unknown`, non-numeric text) terminate
/// the previous sample without starting one of their own.
pub fn samples_from_readings(readings: &[(i64, Option<f64>)], max_gap: i64) -> Vec<Sample> {
    let max_gap = max_gap.max(0);
    let mut samples = Vec::with_capacity(readings.len());
    for (index, (start, value)) in readings.iter().enumerate() {
        let Some(watts) = value else {
            continue;
        };
        let horizon = start.saturating_add(max_gap);
        let end = match readings.get(index + 1) {
            Some((next, _)) => (*next).min(horizon),
            // The last reading is only assumed to hold for one more gap window.
            None => horizon,
        };
        if end > *start {
            samples.push(Sample::new(*start, end, *watts));
        }
    }
    samples
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::testdb::{self, ENTITY, Flavour, ts};

    fn request<'a>(entity_id: &'a str) -> Request<'a> {
        Request {
            entity_id,
            from: None,
            to: None,
            max_gap: 900,
            scale: None,
            clamp_negative: true,
        }
    }

    #[test]
    fn weights_readings_by_how_long_they_were_in_effect() {
        let readings = vec![(0, Some(100.0)), (600, Some(200.0)), (1_200, Some(300.0))];
        let samples = samples_from_readings(&readings, 900);
        assert_eq!(samples.len(), 3);
        assert_eq!(samples[0], Sample::new(0, 600, 100.0));
        assert_eq!(samples[1], Sample::new(600, 1_200, 200.0));
        // The final reading is only carried forward by one gap window.
        assert_eq!(samples[2], Sample::new(1_200, 2_100, 300.0));
    }

    #[test]
    fn caps_long_gaps_so_outages_are_not_counted_as_production() {
        // Four hours between readings, but the sensor only vouches for 15 minutes.
        let readings = vec![(0, Some(4_000.0)), (14_400, Some(10.0))];
        let samples = samples_from_readings(&readings, 900);
        assert_eq!(samples[0], Sample::new(0, 900, 4_000.0));
    }

    #[test]
    fn unavailable_readings_end_the_previous_sample() {
        let readings = vec![(0, Some(100.0)), (300, None), (600, Some(200.0))];
        let samples = samples_from_readings(&readings, 900);
        assert_eq!(samples.len(), 2);
        assert_eq!(samples[0], Sample::new(0, 300, 100.0));
        assert_eq!(samples[1], Sample::new(600, 1_500, 200.0));
    }

    #[test]
    fn duplicate_timestamps_do_not_create_zero_length_samples() {
        let readings = vec![(0, Some(100.0)), (0, Some(200.0)), (600, Some(300.0))];
        let samples = samples_from_readings(&readings, 900);
        assert_eq!(samples.len(), 2);
        assert_eq!(samples[0], Sample::new(0, 600, 200.0));
    }

    #[test]
    fn a_zero_gap_leaves_no_observation_time_at_all() {
        // Nothing is ever in effect for longer than the gap, so nothing is observed.
        // The command line rejects this, but the function must not panic on it.
        let readings = vec![(0, Some(100.0)), (600, Some(200.0))];
        assert!(samples_from_readings(&readings, 0).is_empty());
        assert!(samples_from_readings(&readings, -5).is_empty());
    }

    #[test]
    fn reads_raw_states_from_the_modern_schema() {
        let db = testdb::modern_database();
        let loaded = load(&db, &request(ENTITY)).unwrap();

        assert_eq!(loaded.rows, 5);
        // 500 W, 1500 W, unavailable, 2500 W, 0 W => four numeric readings.
        assert_eq!(loaded.samples.len(), 4);
        let base = ts("2024-06-21 09:00:00");
        assert_eq!(loaded.samples[0], Sample::new(base, base + 600, 500.0));
        assert_eq!(
            loaded.samples[1],
            Sample::new(base + 600, base + 1_200, 1_500.0)
        );
        // The `unavailable` row ends the 1500 W reading and contributes nothing itself.
        assert_eq!(
            loaded.samples[2],
            Sample::new(base + 1_800, base + 2_400, 2_500.0)
        );
    }

    #[test]
    fn reads_raw_states_from_the_legacy_schema() {
        let db = testdb::legacy_database();
        let loaded = load(&db, &request(ENTITY)).unwrap();
        assert_eq!(loaded.samples.len(), 4);
        assert_eq!(loaded.samples[0].watts, 500.0);
    }

    #[test]
    fn clips_states_to_the_requested_window() {
        let db = testdb::modern_database();
        let base = ts("2024-06-21 09:00:00");
        let mut request = request(ENTITY);
        request.from = Some(base + 300);
        request.to = Some(base + 900);
        let loaded = load(&db, &request).unwrap();

        assert_eq!(loaded.samples.len(), 2);
        assert_eq!(
            loaded.samples[0],
            Sample::new(base + 300, base + 600, 500.0)
        );
        assert_eq!(
            loaded.samples[1],
            Sample::new(base + 600, base + 900, 1_500.0)
        );
    }

    #[test]
    fn scales_and_clamps_state_values() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        testdb::create_schema(&conn, Flavour::Modern);
        let metadata_id = testdb::insert_states_meta(&conn, ENTITY);
        let base = ts("2024-06-21 09:00:00");
        testdb::insert_state(&conn, Flavour::Modern, ENTITY, metadata_id, base, "1.5");
        testdb::insert_state(
            &conn,
            Flavour::Modern,
            ENTITY,
            metadata_id,
            base + 600,
            "-0.2",
        );

        let mut request = request(ENTITY);
        request.scale = Some(1_000.0);
        let loaded = load(&conn, &request).unwrap();
        assert_eq!(loaded.samples[0].watts, 1_500.0);
        assert_eq!(loaded.samples[1].watts, 0.0);

        request.clamp_negative = false;
        let loaded = load(&conn, &request).unwrap();
        assert!((loaded.samples[1].watts + 200.0).abs() < 1e-9);
    }

    #[test]
    fn unknown_entities_produce_a_helpful_error() {
        let db = testdb::modern_database();
        let error = load(&db, &request("sensor.solar_powr"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("sensor.solar_powr"), "{error}");
        assert!(error.contains(ENTITY), "{error}");
    }

    #[test]
    fn an_entity_with_no_rows_loads_nothing() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        testdb::create_schema(&conn, Flavour::Modern);
        testdb::insert_states_meta(&conn, ENTITY);
        let loaded = load(&conn, &request(ENTITY)).unwrap();
        assert!(loaded.samples.is_empty());
        assert_eq!(loaded.rows, 0);
    }
}
