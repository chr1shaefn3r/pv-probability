//! Builders for synthetic Home Assistant recorder databases.
//!
//! Home Assistant is deliberately never contacted by any test in this workspace, so every
//! reader is exercised against databases created here instead: one shaped like a current
//! recorder schema, one shaped like a pre-2023 schema, and multi-year synthetic histories
//! for the end-to-end tests and the demo database.
//!
//! This crate depends on nothing else in the workspace, so every other crate can take it
//! as a dev-dependency without a cycle.

use rusqlite::Connection;

/// Entity used throughout the tests.
pub const ENTITY: &str = "sensor.solar_power";

/// The grid sensors the payback tool reads, as a real installation would name them.
pub const IMPORT_ENTITY: &str = "sensor.grid_import_power";
pub const EXPORT_ENTITY: &str = "sensor.grid_export_power";

/// The outdoor thermometer the heating tool reads.
pub const OUTSIDE_ENTITY: &str = "sensor.outdoor_temperature";

/// The synthetic house: floor, room, its thermometer, and the temperature it is heated to.
pub const ROOMS: [(&str, &str, &str, f64); 5] = [
    (
        "Ground floor",
        "Living room",
        "sensor.living_room_temperature",
        21.0,
    ),
    (
        "Ground floor",
        "Kitchen",
        "sensor.kitchen_temperature",
        20.0,
    ),
    ("Ground floor", "Office", "sensor.office_temperature", 20.5),
    ("First floor", "Bedroom", "sensor.bedroom_temperature", 18.5),
    (
        "First floor",
        "Bathroom",
        "sensor.bathroom_temperature",
        22.0,
    ),
];

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

/// Register a measurement statistic (hourly means) and return its `metadata_id`.
pub fn insert_statistic_meta(conn: &Connection, statistic_id: &str, unit: Option<&str>) -> i64 {
    insert_statistic_meta_with_flags(conn, statistic_id, unit, true, false)
}

/// Register a statistic with explicit `has_mean` / `has_sum` flags.
///
/// Home Assistant sets `has_sum` for cumulative counters (energy in kWh) and `has_mean`
/// for measurements (power in W), and writes a different set of columns for each. The
/// flags are deliberately allowed to disagree with the rows in some tests: recent
/// recorder releases replaced `has_mean` with `mean_type`, so the readers trust the data
/// rather than the metadata.
pub fn insert_statistic_meta_with_flags(
    conn: &Connection,
    statistic_id: &str,
    unit: Option<&str>,
    has_mean: bool,
    has_sum: bool,
) -> i64 {
    conn.execute(
        "INSERT INTO statistics_meta (statistic_id, source, unit_of_measurement, has_mean, has_sum, name)
         VALUES (?1, 'recorder', ?2, ?3, ?4, NULL)",
        rusqlite::params![statistic_id, unit, has_mean as i64, has_sum as i64],
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

/// Add one cumulative-counter row: `sum` and `state` populated, `mean` left NULL.
///
/// This is what Home Assistant writes for a `total_increasing` sensor such as an energy
/// meter, and it is the shape that made the first real run of this tool fail.
pub fn insert_statistics_sum_row(
    conn: &Connection,
    flavour: Flavour,
    table: &str,
    metadata_id: i64,
    start_ts: i64,
    sum: f64,
) {
    match flavour {
        Flavour::Modern => {
            conn.execute(
                &format!(
                    "INSERT INTO {table} (created_ts, metadata_id, start_ts, mean, min, max, state, sum) \
                     VALUES (?1, ?2, ?3, NULL, NULL, NULL, ?4, ?4)"
                ),
                rusqlite::params![start_ts as f64, metadata_id, start_ts as f64, sum],
            )
            .expect("insert statistics sum row");
        }
        Flavour::Legacy => {
            let text = format_utc(start_ts);
            conn.execute(
                &format!(
                    "INSERT INTO {table} (created, metadata_id, start, mean, min, max, state, sum) \
                     VALUES (?1, ?2, ?3, NULL, NULL, NULL, ?4, ?4)"
                ),
                rusqlite::params![text, metadata_id, text, sum],
            )
            .expect("insert statistics sum row");
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

/// Parse a UTC timestamp written `YYYY-MM-DD HH:MM:SS` in tests.
pub fn ts(text: &str) -> i64 {
    chrono::NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S")
        .expect("test timestamp parses")
        .and_utc()
        .timestamp()
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

    /// Uniform float in `[0, 0.5]` - not `[0, 1)`, as it was meant to be.
    ///
    /// [`Lcg::next_u32`] keeps 31 bits, so dividing by `u32::MAX` never passes one half.
    /// The synthetic solar and grid histories were generated with it, and the tests and
    /// the README's figures are built on them as they are, so it stays; new generators use
    /// [`Lcg::next_fraction`].
    pub fn next_unit(&mut self) -> f64 {
        f64::from(self.next_u32()) / f64::from(u32::MAX)
    }

    /// Uniform float in `[0, 1)`.
    pub fn next_fraction(&mut self) -> f64 {
        f64::from(self.next_u32()) / f64::from(1u32 << 31)
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

/// Which days of a synthetic history the recorder was actually up for.
///
/// Real Home Assistant histories are not solid blocks: restarts, purges and SD card
/// trouble leave holes, and a report has to survive them. The plans here let the tests
/// and the demo database reproduce that on purpose.
#[derive(Debug, Clone, Default)]
pub struct Outages {
    /// Whole days, counted from the start of the run, with no data at all.
    pub missing_days: Vec<i64>,
    /// Half-open ranges of days `[start, end)` with no data at all.
    pub missing_ranges: Vec<(i64, i64)>,
    /// Hours of the day the sensor never reports, e.g. an inverter that sleeps at night.
    pub missing_hours: Vec<i64>,
}

impl Outages {
    /// An unbroken history.
    pub fn none() -> Self {
        Self::default()
    }

    /// A plausibly messy history: a scattering of lost days and one long outage.
    pub fn spotty(days: i64) -> Self {
        Self {
            missing_days: (0..days).filter(|day| day % 11 == 4).collect(),
            missing_ranges: vec![(days / 3, days / 3 + 9)],
            missing_hours: Vec::new(),
        }
    }

    /// Whether a given day of the run carries any data.
    pub fn covers(&self, day: i64) -> bool {
        !self.missing_days.contains(&day)
            && !self
                .missing_ranges
                .iter()
                .any(|(start, end)| (*start..*end).contains(&day))
    }

    fn covers_hour(&self, hour: i64) -> bool {
        !self.missing_hours.contains(&hour)
    }

    /// How many of the first `days` days carry data.
    pub fn covered_days(&self, days: i64) -> i64 {
        (0..days).filter(|day| self.covers(*day)).count() as i64
    }
}

/// Fill `conn` with hourly long-term statistics covering `days` days of a synthetic solar
/// year starting at `start_ts`, returning the entity's `metadata_id`.
pub fn insert_synthetic_year(conn: &Connection, start_ts: i64, days: i64, seed: u64) -> i64 {
    insert_synthetic_history(conn, start_ts, days, seed, &Outages::none())
}

/// The same, with the recorder down for the days the outage plan names.
pub fn insert_synthetic_history(
    conn: &Connection,
    start_ts: i64,
    days: i64,
    seed: u64,
    outages: &Outages,
) -> i64 {
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
        if !outages.covers(day) {
            continue;
        }
        for hour in 0..24 {
            if !outages.covers_hour(hour) {
                continue;
            }
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

/// A synthetic household load in watts: a base load with a morning and an evening peak.
///
/// Real consumption peaks when the sun is low, which is exactly why a battery is worth
/// anything - the surplus at noon and the demand at seven o'clock never meet.
pub fn synthetic_load_watts(ts: i64, rng: &mut Lcg) -> f64 {
    const SECONDS_PER_DAY: f64 = 86_400.0;
    let hour = (ts as f64 % SECONDS_PER_DAY) / 3_600.0;
    let peak = |centre: f64, width: f64, height: f64| {
        let distance = (hour - centre) / width;
        height * (-distance * distance).exp()
    };
    let base = 250.0 + 120.0 * rng.next_unit();
    let cooking = if rng.next_unit() < 0.12 { 2_000.0 } else { 0.0 };
    base + peak(7.5, 1.1, 1_400.0) + peak(19.0, 1.8, 2_200.0) + cooking
}

/// Write the two grid power sensors a payback report needs, and return their
/// `metadata_id`s as `(import, export)`.
///
/// The house runs [`synthetic_load_watts`] against the same solar curve the other
/// sensors use, and whichever way the meter turns in a ten minute slot becomes import or
/// export - so both can appear inside the same hour, which is what a battery lives on.
pub fn insert_synthetic_grid_power(
    conn: &Connection,
    start_ts: i64,
    days: i64,
    seed: u64,
    outages: &Outages,
) -> (i64, i64) {
    let import_id = insert_statistic_meta(conn, IMPORT_ENTITY, Some("W"));
    let export_id = insert_statistic_meta(conn, EXPORT_ENTITY, Some("W"));
    let mut rng = Lcg::new(seed);
    conn.execute_batch("BEGIN").expect("begin transaction");
    for day in 0..days {
        let cloud_factor = if rng.next_unit() < 0.34 {
            0.15 + 0.35 * rng.next_unit()
        } else {
            0.85 + 0.15 * rng.next_unit()
        };
        if !outages.covers(day) {
            continue;
        }
        for hour in 0..24 {
            if !outages.covers_hour(hour) {
                continue;
            }
            let start = start_ts + day * 86_400 + hour * 3_600;
            let mut import = 0.0;
            let mut export = 0.0;
            for slot in 0..6 {
                let ts = start + slot * 600;
                let solar = synthetic_watts(ts, &mut rng, cloud_factor);
                let load = synthetic_load_watts(ts, &mut rng);
                import += (load - solar).max(0.0) / 6.0;
                export += (solar - load).max(0.0) / 6.0;
            }
            for (metadata_id, mean) in [(import_id, import), (export_id, export)] {
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
    }
    conn.execute_batch("COMMIT").expect("commit transaction");
    (import_id, export_id)
}

/// A modern-schema database holding `days` days of the two grid power sensors.
pub fn synthetic_grid_database(start: &str, days: i64, seed: u64, outages: &Outages) -> Connection {
    let conn = Connection::open_in_memory().expect("open in-memory database");
    create_schema(&conn, Flavour::Modern);
    insert_synthetic_grid_power(&conn, ts(start), days, seed, outages);
    conn
}

/// Write a cumulative energy counter (kWh) alongside a power sensor, the way a real
/// integration exposes both, and return its `metadata_id`.
///
/// The counter integrates the same synthetic solar curve, so the two entities describe
/// the same array - one as instantaneous watts, one as a monotonic total.
pub fn insert_synthetic_energy_counter(
    conn: &Connection,
    statistic_id: &str,
    start_ts: i64,
    days: i64,
    seed: u64,
    outages: &Outages,
) -> i64 {
    let metadata_id =
        insert_statistic_meta_with_flags(conn, statistic_id, Some("kWh"), false, true);
    let mut rng = Lcg::new(seed);
    let mut total = 0.0;
    conn.execute_batch("BEGIN").expect("begin transaction");
    for day in 0..days {
        let cloud_factor = if rng.next_unit() < 0.34 {
            0.15 + 0.35 * rng.next_unit()
        } else {
            0.85 + 0.15 * rng.next_unit()
        };
        for hour in 0..24 {
            let start = start_ts + day * 86_400 + hour * 3_600;
            let watts = (0..6)
                .map(|slot| synthetic_watts(start + slot * 600, &mut rng, cloud_factor))
                .sum::<f64>()
                / 6.0;
            // An hour at this average power adds this many kilowatt hours to the total.
            total += watts / 1_000.0;
            if !outages.covers(day) {
                continue;
            }
            insert_statistics_sum_row(
                conn,
                Flavour::Modern,
                "statistics",
                metadata_id,
                start,
                total,
            );
        }
    }
    conn.execute_batch("COMMIT").expect("commit transaction");
    metadata_id
}

/// Outside temperature in °C at a UTC instant, around a Central European climate.
///
/// The season peaks in mid-July at about 19 °C and bottoms out in mid-January around
/// 0 °C; each day is coldest before dawn and warmest mid-afternoon, about 8 °C apart.
/// `weather` shifts a whole day warmer or colder than the season, which is what makes one
/// January colder than the next.
pub fn synthetic_outside_celsius(ts: i64, weather: f64) -> f64 {
    const SECONDS_PER_DAY: f64 = 86_400.0;
    let day_of_year = (ts as f64 / SECONDS_PER_DAY) % 365.25;
    let hour = (ts as f64 % SECONDS_PER_DAY) / 3_600.0;
    let season = ((day_of_year - 197.0) / 365.25 * std::f64::consts::TAU).cos();
    let daily = ((hour - 15.0) / 24.0 * std::f64::consts::TAU).cos();
    9.5 + 9.5 * season + 4.0 * daily + weather
}

/// A room's temperature in °C, given what it is heated to and how warm it is outside.
///
/// Heating holds the setpoint while it is cold out; on warm days the room drifts above it,
/// following the outside temperature at a fraction of its swing.
pub fn synthetic_room_celsius(setpoint: f64, outside: f64, rng: &mut Lcg) -> f64 {
    let drift = 0.6 * (outside - (setpoint - 6.0)).max(0.0);
    setpoint + drift + 0.4 * (rng.next_fraction() - 0.5)
}

/// Write the outdoor thermometer and every room in [`ROOMS`] as hourly means in °C, and
/// return the outdoor thermometer's `metadata_id`.
///
/// The weather is a slow random walk around the season, so mild and cold spells last a
/// few days rather than flipping every midnight.
pub fn insert_synthetic_temperatures(
    conn: &Connection,
    start_ts: i64,
    days: i64,
    seed: u64,
    outages: &Outages,
) -> i64 {
    let outside_id = insert_statistic_meta(conn, OUTSIDE_ENTITY, Some("°C"));
    let room_ids: Vec<(i64, f64)> = ROOMS
        .iter()
        .map(|(_, _, sensor, setpoint)| {
            (insert_statistic_meta(conn, sensor, Some("°C")), *setpoint)
        })
        .collect();
    // Separate generators: drawing the weather at a fixed stride of one LCG would sample
    // it at a lattice of its outputs, which an LCG makes far from random.
    let mut weather_rng = Lcg::new(seed);
    let mut room_rng = Lcg::new(seed ^ 0x9E37_79B9_7F4A_7C15);
    let mut weather = 0.0;
    conn.execute_batch("BEGIN").expect("begin transaction");
    for day in 0..days {
        weather = 0.75 * weather + 5.0 * (weather_rng.next_fraction() - 0.5);
        if !outages.covers(day) {
            continue;
        }
        for hour in 0..24 {
            if !outages.covers_hour(hour) {
                continue;
            }
            let start = start_ts + day * 86_400 + hour * 3_600;
            // Average the hour, the way Home Assistant's hourly statistics do.
            let outside = (0..6)
                .map(|slot| synthetic_outside_celsius(start + slot * 600, weather))
                .sum::<f64>()
                / 6.0;
            insert_statistics_row(
                conn,
                Flavour::Modern,
                "statistics",
                outside_id,
                start,
                Some(outside),
            );
            for (metadata_id, setpoint) in &room_ids {
                let room = synthetic_room_celsius(*setpoint, outside, &mut room_rng);
                insert_statistics_row(
                    conn,
                    Flavour::Modern,
                    "statistics",
                    *metadata_id,
                    start,
                    Some(room),
                );
            }
        }
    }
    conn.execute_batch("COMMIT").expect("commit transaction");
    outside_id
}

/// A modern-schema database holding `days` days of the synthetic house's thermometers.
pub fn synthetic_temperature_database(
    start: &str,
    days: i64,
    seed: u64,
    outages: &Outages,
) -> Connection {
    let conn = Connection::open_in_memory().expect("open in-memory database");
    create_schema(&conn, Flavour::Modern);
    insert_synthetic_temperatures(&conn, ts(start), days, seed, outages);
    conn
}

/// A modern-schema database holding `days` days of synthetic hourly statistics.
pub fn synthetic_year_database(start: &str, days: i64, seed: u64) -> Connection {
    synthetic_history_database(start, days, seed, &Outages::none())
}

/// The same, with outages.
pub fn synthetic_history_database(
    start: &str,
    days: i64,
    seed: u64,
    outages: &Outages,
) -> Connection {
    let conn = Connection::open_in_memory().expect("open in-memory database");
    create_schema(&conn, Flavour::Modern);
    insert_synthetic_history(&conn, ts(start), days, seed, outages);
    conn
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_synthetic_grid_sensors_import_at_night_and_export_at_noon() {
        let conn = synthetic_grid_database("2024-06-01 00:00:00", 3, 0xBEEF, &Outages::none());
        let read = |entity: &str, hour: i64| -> f64 {
            conn.query_row(
                "SELECT mean FROM statistics s JOIN statistics_meta m ON m.id = s.metadata_id \
                 WHERE m.statistic_id = ?1 AND s.start_ts = ?2",
                rusqlite::params![entity, (ts("2024-06-01 00:00:00") + hour * 3_600) as f64],
                |row| row.get::<_, f64>(0),
            )
            .expect("a row for that hour")
        };

        assert!(
            read(IMPORT_ENTITY, 2) > 0.0,
            "the house draws power at night"
        );
        assert_eq!(read(EXPORT_ENTITY, 2), 0.0, "and exports nothing then");
        assert!(read(EXPORT_ENTITY, 12) > 1_000.0, "midday is a big surplus");
        assert!(read(IMPORT_ENTITY, 20) > 0.0, "the evening peak is bought");
    }

    #[test]
    fn the_synthetic_climate_is_cold_in_winter_and_warm_in_summer() {
        let conn = synthetic_temperature_database("2024-01-01 00:00:00", 366, 7, &Outages::none());
        let monthly_mean = |entity: &str, month: &str| -> f64 {
            conn.query_row(
                "SELECT AVG(mean) FROM statistics s JOIN statistics_meta m ON m.id = s.metadata_id \
                 WHERE m.statistic_id = ?1 AND strftime('%m', s.start_ts, 'unixepoch') = ?2",
                rusqlite::params![entity, month],
                |row| row.get::<_, f64>(0),
            )
            .expect("a mean for that month")
        };

        let january = monthly_mean(OUTSIDE_ENTITY, "01");
        let july = monthly_mean(OUTSIDE_ENTITY, "07");
        assert!(
            (-4.0..5.0).contains(&january),
            "January averaged {january} °C"
        );
        assert!((15.0..23.0).contains(&july), "July averaged {july} °C");

        // Rooms hold their setpoint in winter and run warmer in summer.
        let living_january = monthly_mean("sensor.living_room_temperature", "01");
        let living_july = monthly_mean("sensor.living_room_temperature", "07");
        assert!((living_january - 21.0).abs() < 0.3, "{living_january}");
        assert!(living_july > living_january + 1.0, "{living_july}");
        let bedroom = monthly_mean("sensor.bedroom_temperature", "01");
        assert!(
            bedroom < living_january - 2.0,
            "the bedroom is kept cooler: {bedroom}"
        );

        let units: Vec<String> = conn
            .prepare("SELECT DISTINCT unit_of_measurement FROM statistics_meta")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(units, ["°C"]);
    }

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
    fn test_timestamps_are_utc() {
        assert_eq!(ts("1970-01-01 00:00:00"), 0);
        assert_eq!(ts("2024-06-21 09:00:00"), 1_718_960_400);
        assert_eq!(
            format_utc(ts("2024-06-21 09:00:00")),
            "2024-06-21 09:00:00.000000"
        );
    }

    #[test]
    fn fractions_cover_the_whole_unit_interval() {
        let mut rng = Lcg::new(3);
        let draws: Vec<f64> = (0..10_000).map(|_| rng.next_fraction()).collect();
        let mean = draws.iter().sum::<f64>() / draws.len() as f64;
        assert!((mean - 0.5).abs() < 0.02, "mean {mean}");
        assert!(draws.iter().all(|draw| (0.0..1.0).contains(draw)));
        assert!(draws.iter().any(|draw| *draw > 0.9));
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
    fn outage_plans_say_which_days_survive() {
        let plan = Outages {
            missing_days: vec![3, 4],
            missing_ranges: vec![(10, 13)],
            missing_hours: vec![0, 1],
        };
        assert!(plan.covers(2));
        assert!(!plan.covers(3));
        assert!(!plan.covers(11));
        assert!(plan.covers(13), "the range end is exclusive");
        assert_eq!(plan.covered_days(20), 15);
        assert!(Outages::none().covers(0));
        assert_eq!(Outages::none().covered_days(30), 30);
    }

    #[test]
    fn a_spotty_history_really_has_holes_in_it() {
        let days = 200;
        let plan = Outages::spotty(days);
        let dense = synthetic_year_database("2025-01-01 00:00:00", days, 1);
        let spotty = synthetic_history_database("2025-01-01 00:00:00", days, 1, &plan);

        let rows = |conn: &Connection| {
            conn.query_row::<i64, _, _>("SELECT COUNT(*) FROM statistics", [], |row| row.get(0))
                .unwrap()
        };
        assert_eq!(rows(&dense), days * 24);
        assert_eq!(rows(&spotty), plan.covered_days(days) * 24);
        assert!(
            plan.covered_days(days) < days - 20,
            "not enough was removed"
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
