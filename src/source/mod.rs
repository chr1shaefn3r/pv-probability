//! Reading power readings out of a copy of `home-assistant_v2.db`.

pub mod catalog;
pub mod schema;
pub mod states;
pub mod statistics;
pub mod testdb;

use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use chrono_tz::Tz;
use rusqlite::{Connection, OpenFlags};

use crate::model::Sample;
use crate::source::schema::{StatisticsTable, ValueColumn, find_statistic, table_exists};

/// Which recorder table to take the readings from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum SourceKind {
    /// Pick the table with the most history for this entity.
    Auto,
    /// `statistics`: hourly means, kept for years. The default for month/week views.
    Statistics,
    /// `statistics_short_term`: five minute means, kept for about ten days.
    ShortTerm,
    /// `states`: every reported change, kept for about ten days.
    States,
}

impl std::fmt::Display for SourceKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            SourceKind::Auto => "auto",
            SourceKind::Statistics => "statistics",
            SourceKind::ShortTerm => "statistics_short_term",
            SourceKind::States => "states",
        })
    }
}

/// Everything the loader needs to know.
#[derive(Debug, Clone)]
pub struct LoadOptions<'a> {
    pub entity: &'a str,
    pub source: SourceKind,
    pub value: ValueColumn,
    pub from: Option<i64>,
    pub to: Option<i64>,
    pub max_gap: i64,
    pub scale: Option<f64>,
    pub clamp_negative: bool,
    /// Only used to state dates in error messages in the timezone the user asked for.
    pub tz: Tz,
}

/// Samples plus a description of where they came from, for the report header.
#[derive(Debug, Clone, PartialEq)]
pub struct LoadedSamples {
    pub samples: Vec<Sample>,
    /// The table actually used, with `Auto` already resolved.
    pub source: SourceKind,
    pub unit: Option<String>,
    pub scale: f64,
    pub rows: usize,
    pub first_ts: Option<i64>,
    pub last_ts: Option<i64>,
}

impl LoadedSamples {
    /// Total observation time behind the samples, in seconds.
    pub fn observed_seconds(&self) -> i64 {
        self.samples.iter().map(Sample::duration).sum()
    }
}

/// Open a recorder database read-only. The file is never written to, so it is safe to
/// point at a copy taken while Home Assistant is running.
pub fn open_database(path: &Path) -> Result<Connection> {
    if !path.exists() {
        bail!("{} does not exist", path.display());
    }
    Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).with_context(|| {
        format!(
            "failed to open {} read-only. If Home Assistant is still writing to it, copy it first \
             with: sqlite3 home-assistant_v2.db \".backup 'copy.db'\"",
            path.display()
        )
    })
}

/// Load samples, resolving [`SourceKind::Auto`] against what the database actually holds.
pub fn load(conn: &Connection, options: &LoadOptions<'_>) -> Result<LoadedSamples> {
    let resolved = match options.source {
        SourceKind::Auto => resolve_auto(conn, options.entity)?,
        explicit => explicit,
    };

    let loaded = match resolved {
        SourceKind::Statistics | SourceKind::ShortTerm => {
            let table = if resolved == SourceKind::Statistics {
                StatisticsTable::LongTerm
            } else {
                StatisticsTable::ShortTerm
            };
            statistics::load(
                conn,
                &statistics::Request {
                    statistic_id: options.entity,
                    table,
                    value: options.value,
                    from: options.from,
                    to: options.to,
                    scale: options.scale,
                    clamp_negative: options.clamp_negative,
                    tz: options.tz,
                },
            )?
        }
        SourceKind::States => states::load(
            conn,
            &states::Request {
                entity_id: options.entity,
                from: options.from,
                to: options.to,
                max_gap: options.max_gap,
                scale: options.scale,
                clamp_negative: options.clamp_negative,
                tz: options.tz,
            },
        )?,
        SourceKind::Auto => unreachable!("auto was resolved above"),
    };

    let first_ts = loaded.samples.iter().map(|sample| sample.start_ts).min();
    let last_ts = loaded.samples.iter().map(|sample| sample.end_ts).max();

    Ok(LoadedSamples {
        samples: loaded.samples,
        source: resolved,
        unit: loaded.unit,
        scale: loaded.scale,
        rows: loaded.rows,
        first_ts,
        last_ts,
    })
}

/// Prefer the table that holds the most history: long-term statistics, then short-term
/// statistics, then raw states.
///
/// `value` is the column the caller intends to read, because a table full of rows that do
/// not carry that column is no use. A cumulative counter (energy in kWh) is the case that
/// matters: its statistics rows hold `sum` and `state` and no `mean`, and its raw states
/// hold a monotonically climbing total. Falling back to those states would produce a
/// confident, completely wrong picture, so a statistic that exists but carries no usable
/// values is left for the caller to explain rather than routed around.
pub fn resolve_auto(conn: &Connection, entity: &str) -> Result<SourceKind> {
    if table_exists(conn, "statistics_meta")?
        && let Some(meta) = find_statistic(conn, entity)?
    {
        for (kind, table) in [
            (SourceKind::Statistics, StatisticsTable::LongTerm),
            (SourceKind::ShortTerm, StatisticsTable::ShortTerm),
        ] {
            if table_exists(conn, table.table_name())?
                && statistics::row_count(conn, table, meta.metadata_id)? > 0
            {
                // Rows that carry no usable value still stop the search here: falling
                // through to `states` for a counter would plot a climbing kWh total as if
                // it were watts. `explain_no_samples` reports why instead.
                return Ok(kind);
            }
        }
    }
    // Raw states are only worth trying when they actually know this entity. Falling
    // through blindly produces a `states`-flavoured "no such entity" for what is really a
    // typo in a statistic id, and buries the fact that the statistics table could have
    // answered.
    if table_exists(conn, "states")? && states::entity_exists(conn, entity)? {
        return Ok(SourceKind::States);
    }
    if table_exists(conn, "statistics")? {
        return Ok(SourceKind::Statistics);
    }
    if table_exists(conn, "states")? {
        return Ok(SourceKind::States);
    }
    bail!("this database has no statistics and no states tables to read from")
}

/// Explain why a load that found no usable readings found none.
///
/// This is where a first run goes wrong, so it is worth being specific: the generic
/// "try a different --source" helps nobody, and the most common cause - pointing at an
/// energy counter rather than a power sensor - has an exact answer.
///
/// The numbers come from the catalogue rather than from `loaded`, because a date filter
/// pushed into the query can leave `loaded.rows` at zero while the entity is full of data.
pub fn explain_no_samples(
    conn: &Connection,
    options: &LoadOptions<'_>,
    loaded: &LoadedSamples,
    tz: Tz,
) -> anyhow::Error {
    let entity = options.entity;
    let hint = catalog::suggest_block(conn, entity, tz);
    let known = catalog::list_statistics(conn, Some(entity))
        .unwrap_or_default()
        .into_iter()
        .find(|candidate| candidate.statistic_id == entity);

    let Some(candidate) = known else {
        // Not a statistic at all: either raw states with nothing usable, or unknown.
        return anyhow!(
            "`{entity}` has no usable readings in `{}`.{hint}",
            loaded.source
        );
    };

    if candidate.rows == 0 {
        return anyhow!(
            "`{entity}` is known to this database but has no statistics rows at all.{hint}"
        );
    }

    // Rows exist, but not one carries the column being read - the energy counter case.
    if !candidate.kind.has_mean() {
        let column = options.value.column_name();
        let unit = candidate
            .unit
            .as_deref()
            .map(|unit| format!(" Its unit is recorded as `{unit}`."))
            .unwrap_or_default();
        return anyhow!(
            "`{entity}` has {} rows in `statistics`, but not one of them carries a \
             `{column}`.\n\n\
             Home Assistant writes `sum` and `state` - not `{column}` - for cumulative \
             counters such as an energy total in kWh, because the average of a running \
             total means nothing. This tool plots instantaneous power, so it needs a \
             sensor measured in W or kW (`state_class: measurement`).{unit} For an energy \
             counter the matching power sensor is usually the same name with `_power` \
             instead of `_energy`.{hint}",
            candidate.rows
        );
    }

    // It does hold usable values, so the window must have excluded them.
    let asked = match (options.from, options.to) {
        (Some(from), Some(to)) => format!(
            " between {} and {}",
            local_date(from, tz),
            local_date(to, tz)
        ),
        (Some(from), None) => format!(" from {} on", local_date(from, tz)),
        (None, Some(to)) => format!(" before {}", local_date(to, tz)),
        (None, None) => String::new(),
    };
    let available = catalog::date_range(&candidate, tz);
    anyhow!(
        "`{entity}` has {} rows in `statistics` covering {available}, but none of them \
         are usable{asked}. Widen --from/--to, or drop them entirely.",
        candidate.rows
    )
}

fn local_date(ts: i64, tz: Tz) -> String {
    crate::timeutil::day_to_date(crate::timeutil::local_day(ts, tz))
        .map(|date| date.to_string())
        .unwrap_or_else(|| "?".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono_tz::Tz::UTC;

    use crate::source::testdb::{self, ENTITY, Flavour, ts};

    fn options(entity: &str) -> LoadOptions<'_> {
        LoadOptions {
            entity,
            source: SourceKind::Auto,
            value: ValueColumn::Mean,
            from: None,
            to: None,
            max_gap: 900,
            scale: None,
            clamp_negative: true,
            tz: UTC,
        }
    }

    #[test]
    fn auto_prefers_long_term_statistics() {
        let db = testdb::modern_database();
        assert_eq!(resolve_auto(&db, ENTITY).unwrap(), SourceKind::Statistics);

        let loaded = load(&db, &options(ENTITY)).unwrap();
        assert_eq!(loaded.source, SourceKind::Statistics);
        assert_eq!(loaded.samples.len(), 3);
        assert_eq!(loaded.first_ts, Some(ts("2024-06-21 09:00:00")));
        assert_eq!(loaded.last_ts, Some(ts("2024-06-21 13:00:00")));
        assert_eq!(loaded.observed_seconds(), 3 * 3_600);
    }

    #[test]
    fn auto_falls_back_to_short_term_statistics() {
        let conn = Connection::open_in_memory().unwrap();
        testdb::create_schema(&conn, Flavour::Modern);
        let metadata_id = testdb::insert_statistic_meta(&conn, ENTITY, Some("W"));
        testdb::insert_statistics_row(
            &conn,
            Flavour::Modern,
            "statistics_short_term",
            metadata_id,
            ts("2024-06-21 09:00:00"),
            Some(900.0),
        );
        assert_eq!(resolve_auto(&conn, ENTITY).unwrap(), SourceKind::ShortTerm);
        assert_eq!(
            load(&conn, &options(ENTITY)).unwrap().source,
            SourceKind::ShortTerm
        );
    }

    #[test]
    fn auto_falls_back_to_raw_states() {
        let conn = Connection::open_in_memory().unwrap();
        testdb::create_schema(&conn, Flavour::Modern);
        let metadata_id = testdb::insert_states_meta(&conn, ENTITY);
        testdb::insert_state(
            &conn,
            Flavour::Modern,
            ENTITY,
            metadata_id,
            ts("2024-06-21 09:00:00"),
            "1234",
        );
        assert_eq!(resolve_auto(&conn, ENTITY).unwrap(), SourceKind::States);

        let loaded = load(&conn, &options(ENTITY)).unwrap();
        assert_eq!(loaded.source, SourceKind::States);
        assert_eq!(loaded.samples[0].watts, 1_234.0);
    }

    #[test]
    fn an_explicit_source_is_used_even_when_another_has_more_data() {
        let db = testdb::modern_database();
        let mut options = options(ENTITY);
        options.source = SourceKind::States;
        let loaded = load(&db, &options).unwrap();
        assert_eq!(loaded.source, SourceKind::States);
        assert_eq!(loaded.samples.len(), 4);
    }

    /// A power sensor and its energy-counter sibling, the shape that made a first real
    /// run fail.
    fn power_and_counter() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        testdb::create_schema(&conn, Flavour::Modern);
        let power = testdb::insert_statistic_meta(&conn, "sensor.eve_plug_power", Some("W"));
        let energy = testdb::insert_statistic_meta_with_flags(
            &conn,
            "sensor.eve_plug_energy",
            Some("kWh"),
            false,
            true,
        );
        let base = ts("2025-06-01 00:00:00");
        for hour in 0..8 {
            testdb::insert_statistics_row(
                &conn,
                Flavour::Modern,
                "statistics",
                power,
                base + hour * 3_600,
                Some(900.0),
            );
            testdb::insert_statistics_sum_row(
                &conn,
                Flavour::Modern,
                "statistics",
                energy,
                base + hour * 3_600,
                hour as f64 * 0.9,
            );
        }
        conn
    }

    fn load_failure(conn: &Connection, entity: &str) -> String {
        let options = options(entity);
        let loaded = load(conn, &options).expect("the entity resolves");
        assert!(loaded.samples.is_empty(), "expected no usable samples");
        explain_no_samples(conn, &options, &loaded, UTC).to_string()
    }

    #[test]
    fn an_energy_counter_is_diagnosed_and_redirected() {
        let conn = power_and_counter();
        let error = load_failure(&conn, "sensor.eve_plug_energy");

        assert!(error.contains("8 rows"), "the rows are counted: {error}");
        assert!(
            error.contains("`mean`"),
            "the missing column is named: {error}"
        );
        assert!(error.contains("kWh"), "the recorded unit is named: {error}");
        assert!(
            error.contains("cumulative") && error.contains("W or kW"),
            "the reason is explained: {error}"
        );
        assert!(
            error.contains("sensor.eve_plug_power"),
            "the sibling power sensor is offered: {error}"
        );
        assert!(error.contains("--list-entities"), "{error}");
    }

    #[test]
    fn a_counter_is_never_read_from_raw_states_instead() {
        // Its states are a climbing kWh total; plotting them as watts would be nonsense,
        // so `auto` must stop at the statistics table rather than fall through.
        let conn = power_and_counter();
        let states_id = testdb::insert_states_meta(&conn, "sensor.eve_plug_energy");
        let base = ts("2025-06-01 00:00:00");
        for hour in 0..8 {
            testdb::insert_state(
                &conn,
                Flavour::Modern,
                "sensor.eve_plug_energy",
                states_id,
                base + hour * 3_600,
                "7.2",
            );
        }

        assert_eq!(
            resolve_auto(&conn, "sensor.eve_plug_energy").unwrap(),
            SourceKind::Statistics
        );
        let error = load_failure(&conn, "sensor.eve_plug_energy");
        assert!(
            error.contains("not one of them carries a `mean`"),
            "{error}"
        );
    }

    #[test]
    fn an_unknown_entity_is_told_what_is_available() {
        let conn = power_and_counter();
        let error = load(&conn, &options("sensor.nope"))
            .unwrap_err()
            .to_string();
        // The existing "did you mean" still applies when something looks similar.
        let close = load(&conn, &options("sensor.eve_plug_powr"))
            .unwrap_err()
            .to_string();
        assert!(close.contains("did you mean"), "{close}");
        assert!(close.contains("sensor.eve_plug_power"), "{close}");
        assert!(error.contains("sensor.nope"), "{error}");
    }

    #[test]
    fn an_empty_window_reports_the_range_that_exists() {
        let conn = power_and_counter();
        let mut options = options("sensor.eve_plug_power");
        options.from = Some(ts("2030-01-01 00:00:00"));
        let loaded = load(&conn, &options).expect("the entity resolves");
        assert!(loaded.samples.is_empty());

        let error = explain_no_samples(&conn, &options, &loaded, UTC).to_string();
        assert!(error.contains("2030-01-01"), "the window is named: {error}");
        assert!(error.contains("--from"), "{error}");
    }

    #[test]
    fn an_entity_with_no_rows_at_all_gets_the_candidate_list() {
        let conn = power_and_counter();
        testdb::insert_statistic_meta(&conn, "sensor.brand_new", Some("W"));
        let error = load_failure(&conn, "sensor.brand_new");

        assert!(error.contains("no statistics rows at all"), "{error}");
        assert!(
            error.contains("sensor.eve_plug_power"),
            "a usable sensor is still offered: {error}"
        );
    }

    #[test]
    fn a_database_without_recorder_tables_is_rejected() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE unrelated (id INTEGER)")
            .unwrap();
        let error = resolve_auto(&conn, ENTITY).unwrap_err().to_string();
        assert!(error.contains("no statistics"), "{error}");
    }

    #[test]
    fn opening_a_missing_file_is_an_error() {
        let error = open_database(Path::new("/definitely/not/here.db"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("does not exist"), "{error}");
    }

    #[test]
    fn source_kinds_render_as_table_names() {
        assert_eq!(SourceKind::Statistics.to_string(), "statistics");
        assert_eq!(SourceKind::ShortTerm.to_string(), "statistics_short_term");
        assert_eq!(SourceKind::States.to_string(), "states");
    }
}
