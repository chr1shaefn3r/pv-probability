//! Reading power readings out of a copy of `home-assistant_v2.db`.

pub mod schema;
pub mod states;
pub mod statistics;
pub mod testdb;

use std::path::Path;

use anyhow::{Context, Result, bail};
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
pub fn resolve_auto(conn: &Connection, entity: &str) -> Result<SourceKind> {
    if table_exists(conn, "statistics_meta")?
        && let Some(meta) = find_statistic(conn, entity)?
    {
        if table_exists(conn, "statistics")?
            && statistics::row_count(conn, StatisticsTable::LongTerm, meta.metadata_id)? > 0
        {
            return Ok(SourceKind::Statistics);
        }
        if table_exists(conn, "statistics_short_term")?
            && statistics::row_count(conn, StatisticsTable::ShortTerm, meta.metadata_id)? > 0
        {
            return Ok(SourceKind::ShortTerm);
        }
    }
    if table_exists(conn, "states")? {
        return Ok(SourceKind::States);
    }
    bail!("this database has no statistics and no states tables to read from")
}

#[cfg(test)]
mod tests {
    use super::*;
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
