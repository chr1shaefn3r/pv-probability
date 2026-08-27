//! Builders for synthetic Home Assistant recorder databases.
//!
//! Home Assistant is deliberately never contacted by this crate's tests, so every reader
//! is exercised against databases created here instead: one shaped like a current
//! recorder schema, one shaped like a pre-2023 schema, and a multi-year synthetic solar
//! year for the end-to-end test.

use rusqlite::Connection;

use crate::timeutil::parse_ha_datetime;

/// Entity used throughout the tests.
pub const ENTITY: &str = "sensor.solar_power";

const MODERN_SCHEMA: &str = "
CREATE TABLE statistics_meta (
    id INTEGER PRIMARY KEY,
    statistic_id VARCHAR(255),
    source VARCHAR(32),
    unit_of_measurement VARCHAR(255),
    has_mean BOOLEAN,
    has_sum BOOLEAN,
    name VARCHAR(255)
);
CREATE TABLE statistics (
    id INTEGER PRIMARY KEY,
    created_ts FLOAT,
    metadata_id INTEGER,
    start_ts FLOAT,
    mean FLOAT,
    min FLOAT,
    max FLOAT,
    last_reset_ts FLOAT,
    state FLOAT,
    sum FLOAT
);
CREATE TABLE statistics_short_term (
    id INTEGER PRIMARY KEY,
    created_ts FLOAT,
    metadata_id INTEGER,
    start_ts FLOAT,
    mean FLOAT,
    min FLOAT,
    max FLOAT,
    last_reset_ts FLOAT,
    state FLOAT,
    sum FLOAT
);
CREATE TABLE states_meta (
    metadata_id INTEGER PRIMARY KEY,
    entity_id VARCHAR(255)
);
CREATE TABLE states (
    state_id INTEGER PRIMARY KEY,
    metadata_id INTEGER,
    state VARCHAR(255),
    last_updated_ts FLOAT,
    last_changed_ts FLOAT,
    attributes_id INTEGER
);
";

const LEGACY_SCHEMA: &str = "
CREATE TABLE statistics_meta (
    id INTEGER PRIMARY KEY,
    statistic_id VARCHAR(255),
    source VARCHAR(32),
    unit_of_measurement VARCHAR(255),
    has_mean BOOLEAN,
    has_sum BOOLEAN,
    name VARCHAR(255)
);
CREATE TABLE statistics (
    id INTEGER PRIMARY KEY,
    created DATETIME,
    metadata_id INTEGER,
    start DATETIME,
    mean FLOAT,
    min FLOAT,
    max FLOAT,
    last_reset DATETIME,
    state FLOAT,
    sum FLOAT
);
CREATE TABLE statistics_short_term (
    id INTEGER PRIMARY KEY,
    created DATETIME,
    metadata_id INTEGER,
    start DATETIME,
    mean FLOAT,
    min FLOAT,
    max FLOAT,
    last_reset DATETIME,
    state FLOAT,
    sum FLOAT
);
CREATE TABLE states (
    state_id INTEGER PRIMARY KEY,
    entity_id VARCHAR(255),
    state VARCHAR(255),
    last_updated DATETIME,
    last_changed DATETIME
);
";

/// Which recorder generation to emulate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flavour {
    /// Epoch timestamps and `states_meta` (Home Assistant 2023.4 and later).
    Modern,
    /// Text timestamps and `states.entity_id`.
    Legacy,
}

impl Flavour {
    fn schema(self) -> &'static str {
        match self {
            Flavour::Modern => MODERN_SCHEMA,
            Flavour::Legacy => LEGACY_SCHEMA,
        }
    }
}

/// Create the recorder tables in an empty database.
pub fn create_schema(conn: &Connection, flavour: Flavour) {
    conn.execute_batch(flavour.schema())
        .expect("test schema is valid SQL");
}

/// Register a statistic and return its `metadata_id`.
pub fn insert_statistic_meta(conn: &Connection, statistic_id: &str, unit: Option<&str>) -> i64 {
    conn.execute(
        "INSERT INTO statistics_meta (statistic_id, source, unit_of_measurement, has_mean, has_sum, name)
         VALUES (?1, 'recorder', ?2, 1, 0, NULL)",
        rusqlite::params![statistic_id, unit],
    )
    .expect("insert statistics_meta");
    conn.last_insert_rowid()
}

/// Register an entity for raw states and return its `metadata_id` (modern flavour only).
pub fn insert_states_meta(conn: &Connection, entity_id: &str) -> i64 {
    conn.execute(
        "INSERT INTO states_meta (entity_id) VALUES (?1)",
        [entity_id],
    )
    .expect("insert states_meta");
    conn.last_insert_rowid()
}

/// Add one row to `statistics` or `statistics_short_term`.
pub fn insert_statistics_row(
    conn: &Connection,
    flavour: Flavour,
    table: &str,
    metadata_id: i64,
    start_ts: i64,
    mean: Option<f64>,
) {
    let (min, max) = match mean {
        Some(value) => (Some(value * 0.5), Some(value * 1.5)),
        None => (None, None),
    };
    match flavour {
        Flavour::Modern => {
            conn.execute(
                &format!(
                    "INSERT INTO {table} (created_ts, metadata_id, start_ts, mean, min, max) VALUES (?1, ?2, ?3, ?4, ?5, ?6)"
                ),
                rusqlite::params![start_ts as f64, metadata_id, start_ts as f64, mean, min, max],
            )
            .expect("insert statistics row");
        }
        Flavour::Legacy => {
            let text = format_utc(start_ts);
            conn.execute(
                &format!(
                    "INSERT INTO {table} (created, metadata_id, start, mean, min, max) VALUES (?1, ?2, ?3, ?4, ?5, ?6)"
                ),
                rusqlite::params![text, metadata_id, text, mean, min, max],
            )
            .expect("insert statistics row");
        }
    }
}

/// Add one row to `states`. `state` is stored verbatim, so `"unavailable"` works.
pub fn insert_state(
    conn: &Connection,
    flavour: Flavour,
    key: &str,
    metadata_id: i64,
    ts: i64,
    state: &str,
) {
    match flavour {
        Flavour::Modern => {
            conn.execute(
                "INSERT INTO states (metadata_id, state, last_updated_ts, last_changed_ts)
                 VALUES (?1, ?2, ?3, ?3)",
                rusqlite::params![metadata_id, state, ts as f64],
            )
            .expect("insert state");
        }
        Flavour::Legacy => {
            let text = format_utc(ts);
            conn.execute(
                "INSERT INTO states (entity_id, state, last_updated, last_changed)
                 VALUES (?1, ?2, ?3, ?3)",
                rusqlite::params![key, state, text],
            )
            .expect("insert state");
        }
    }
}

/// Format a unix timestamp the way legacy recorder schemas stored it.
pub fn format_utc(ts: i64) -> String {
    chrono::DateTime::from_timestamp(ts, 0)
        .expect("timestamp in range")
        .format("%Y-%m-%d %H:%M:%S.%6f")
        .to_string()
}

/// Parse a UTC timestamp in tests.
pub fn ts(text: &str) -> i64 {
    parse_ha_datetime(text).expect("test timestamp parses")
}

/// A small in-memory database in the modern schema: three hourly statistics rows, three
/// short-term rows and a handful of raw states, all for [`ENTITY`].
pub fn modern_database() -> Connection {
    small_database(Flavour::Modern)
}

/// The same content in the legacy schema.
pub fn legacy_database() -> Connection {
    small_database(Flavour::Legacy)
}

fn small_database(flavour: Flavour) -> Connection {
    let conn = Connection::open_in_memory().expect("open in-memory database");
    create_schema(&conn, flavour);
    let statistic_id = insert_statistic_meta(&conn, ENTITY, Some("W"));
    let states_id = match flavour {
        Flavour::Modern => insert_states_meta(&conn, ENTITY),
        Flavour::Legacy => 0,
    };
    // A second entity, so "did you mean" suggestions have something to find.
    insert_statistic_meta(&conn, "sensor.grid_power", Some("W"));

    let base = ts("2024-06-21 09:00:00");
    for (index, mean) in [Some(1_000.0), Some(2_000.0), None, Some(3_000.0)]
        .into_iter()
        .enumerate()
    {
        insert_statistics_row(
            &conn,
            flavour,
            "statistics",
            statistic_id,
            base + index as i64 * 3_600,
            mean,
        );
        insert_statistics_row(
            &conn,
            flavour,
            "statistics_short_term",
            statistic_id,
            base + index as i64 * 300,
            mean,
        );
    }

    for (offset, state) in [
        (0, "500"),
        (600, "1500"),
        (1_200, "unavailable"),
        (1_800, "2500"),
        (2_400, "0"),
    ] {
        insert_state(&conn, flavour, ENTITY, states_id, base + offset, state);
    }

    conn
}

/// Deterministic generator, so synthetic solar years are reproducible without pulling in
/// a random number crate.
pub struct Lcg(u64);

impl Lcg {
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }

    pub fn next_u32(&mut self) -> u32 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 33) as u32
    }

    /// Uniform float in `[0, 1)`.
    pub fn next_unit(&mut self) -> f64 {
        f64::from(self.next_u32()) / f64::from(u32::MAX)
    }
}

/// Watts a 8 kWp south-facing array might plausibly produce at a UTC instant, with a
/// day/night cycle, a seasonal envelope and cloudy days mixed in.
pub fn synthetic_watts(ts: i64, rng: &mut Lcg, cloud_factor: f64) -> f64 {
    const SECONDS_PER_DAY: f64 = 86_400.0;
    let day_of_year = (ts as f64 / SECONDS_PER_DAY) % 365.25;
    let hour = (ts as f64 % SECONDS_PER_DAY) / 3_600.0;

    // Seasonal peak output: ~7.5 kW at midsummer, ~1.8 kW at midwinter.
    let season = ((day_of_year - 172.0) / 365.25 * std::f64::consts::TAU).cos();
    let peak = 4_650.0 + 2_850.0 * season;
    // Daylight window, wider in summer than in winter.
    let half_day = 4.0 + 2.5 * season;
    let solar_noon = 12.0;
    let from_noon = (hour - solar_noon).abs();
    if from_noon >= half_day {
        return 0.0;
    }
    let shape = (from_noon / half_day * std::f64::consts::FRAC_PI_2).cos();
    let jitter = 0.9 + 0.2 * rng.next_unit();
    (peak * shape * shape * cloud_factor * jitter).max(0.0)
}

/// Fill `conn` with hourly long-term statistics covering `days` days of a synthetic solar
/// year starting at `start_ts`, returning the entity's `metadata_id`.
pub fn insert_synthetic_year(conn: &Connection, start_ts: i64, days: i64, seed: u64) -> i64 {
    let metadata_id = insert_statistic_meta(conn, ENTITY, Some("W"));
    let mut rng = Lcg::new(seed);
    conn.execute_batch("BEGIN").expect("begin transaction");
    for day in 0..days {
        // Roughly one day in three is overcast.
        let cloud_factor = if rng.next_unit() < 0.34 {
            0.15 + 0.35 * rng.next_unit()
        } else {
            0.85 + 0.15 * rng.next_unit()
        };
        for hour in 0..24 {
            let start = start_ts + day * 86_400 + hour * 3_600;
            // Average the hour, the way Home Assistant's hourly statistics do.
            let mean = (0..6)
                .map(|slot| synthetic_watts(start + slot * 600, &mut rng, cloud_factor))
                .sum::<f64>()
                / 6.0;
            insert_statistics_row(
                conn,
                Flavour::Modern,
                "statistics",
                metadata_id,
                start,
                Some(mean),
            );
        }
    }
    conn.execute_batch("COMMIT").expect("commit transaction");
    metadata_id
}

/// A modern-schema database holding `days` days of synthetic hourly statistics.
pub fn synthetic_year_database(start: &str, days: i64, seed: u64) -> Connection {
    let conn = Connection::open_in_memory().expect("open in-memory database");
    create_schema(&conn, Flavour::Modern);
    insert_synthetic_year(&conn, ts(start), days, seed);
    conn
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_both_schema_flavours() {
        for conn in [modern_database(), legacy_database()] {
            let rows: i64 = conn
                .query_row("SELECT COUNT(*) FROM statistics", [], |row| row.get(0))
                .unwrap();
            assert_eq!(rows, 4);
            let states: i64 = conn
                .query_row("SELECT COUNT(*) FROM states", [], |row| row.get(0))
                .unwrap();
            assert_eq!(states, 5);
        }
    }

    #[test]
    fn legacy_timestamps_round_trip() {
        let stamp = ts("2024-06-21 09:00:00");
        assert_eq!(parse_ha_datetime(&format_utc(stamp)), Some(stamp));
    }

    #[test]
    fn synthetic_output_follows_a_day_night_cycle() {
        let mut rng = Lcg::new(1);
        let midsummer = ts("2024-06-21 00:00:00");
        let midnight = synthetic_watts(midsummer, &mut rng, 1.0);
        let noon = synthetic_watts(midsummer + 12 * 3_600, &mut rng, 1.0);
        assert_eq!(midnight, 0.0);
        assert!(noon > 4_000.0, "midsummer noon was {noon} W");

        let midwinter = ts("2024-12-21 12:00:00");
        let winter_noon = synthetic_watts(midwinter, &mut rng, 1.0);
        assert!(
            winter_noon < noon,
            "winter noon {winter_noon} W should be below summer noon {noon} W"
        );
    }

    #[test]
    fn synthetic_years_are_reproducible() {
        let count = |seed| {
            let conn = synthetic_year_database("2024-01-01 00:00:00", 5, seed);
            conn.query_row::<f64, _, _>("SELECT SUM(mean) FROM statistics", [], |row| row.get(0))
                .unwrap()
        };
        assert_eq!(count(7), count(7));
        assert_ne!(count(7), count(8));
    }
}
