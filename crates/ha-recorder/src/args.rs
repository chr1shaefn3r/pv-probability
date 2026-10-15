//! The command line options every tool shares: which database, which table, which
//! stretch of time, and how to read it.
//!
//! Each tool flattens [`RecorderArgs`] into its own arguments, so these options are
//! spelled, documented and validated the same way everywhere, and a new tool gets them
//! for free.

use std::path::PathBuf;

use anyhow::{Result, bail, ensure};
use chrono_tz::Tz;
use rusqlite::Connection;

use crate::source::schema::ValueColumn;
use crate::source::{self, LoadOptions, SourceKind, catalog};
use crate::timeutil::{parse_local_date, resolve_timezone};

/// How many candidates `--list-entities` prints per kind.
const LISTING_LIMIT: usize = 40;

/// The `--help` section these options are listed under, after the tool's own.
///
/// Set on every field rather than with `next_help_heading`, which would carry on to the
/// options a tool declares after flattening this struct in.
pub const HELP_HEADING: &str = "Reading the database";

/// How to read the recorder database.
///
/// ```ignore
/// #[derive(clap::Parser)]
/// struct Args {
///     #[command(flatten)]
///     recorder: RecorderArgs,
///     // the tool's own options
/// }
/// ```
#[derive(Debug, Clone, clap::Args)]
pub struct RecorderArgs {
    /// Path to a copy of home-assistant_v2.db (opened read-only).
    #[arg(long, help_heading = HELP_HEADING, value_name = "FILE")]
    pub db: PathBuf,

    /// List the statistics this database holds and exit, power sensors first.
    ///
    /// Takes an optional word to filter the ids by, e.g. `--list-entities solar`.
    #[arg(long, help_heading = HELP_HEADING, value_name = "FILTER", num_args = 0..=1, default_missing_value = "")]
    pub list_entities: Option<String>,

    /// Which recorder table to read.
    #[arg(long, help_heading = HELP_HEADING, value_enum, default_value_t = SourceKind::Auto)]
    pub source: SourceKind,

    /// Which statistics column to use when reading a statistics table.
    #[arg(long, help_heading = HELP_HEADING, value_enum, default_value_t = ValueColumn::Mean)]
    pub stat: ValueColumn,

    /// IANA timezone for the hours and dates in the report, e.g. Europe/Berlin. Defaults
    /// to the machine's timezone.
    #[arg(long, help_heading = HELP_HEADING, value_name = "TZ")]
    pub tz: Option<String>,

    /// Only consider data from this local date on (YYYY-MM-DD).
    #[arg(long, help_heading = HELP_HEADING, value_name = "DATE")]
    pub from: Option<String>,

    /// Only consider data before this local date (YYYY-MM-DD).
    #[arg(long, help_heading = HELP_HEADING, value_name = "DATE")]
    pub to: Option<String>,

    /// How long a single raw state reading is assumed to stay valid, in seconds.
    #[arg(long, help_heading = HELP_HEADING, value_name = "SECONDS", default_value_t = 900)]
    pub max_gap: i64,

    /// Multiply every reading by this factor (use 1000 for a sensor reporting kW).
    #[arg(long, help_heading = HELP_HEADING, value_name = "FACTOR")]
    pub scale: Option<f64>,

    /// Report outages longer than this, in hours. The default of a full day keeps sensors
    /// that go offline every night from filling the report with nightly "gaps".
    #[arg(long, help_heading = HELP_HEADING, value_name = "HOURS", default_value_t = 24.0)]
    pub gap_threshold_hours: f64,
}

/// [`RecorderArgs`] after validation, with the timezone and the dates resolved.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RecorderConfig {
    pub tz: Tz,
    /// Inclusive lower bound as a UTC timestamp.
    pub from: Option<i64>,
    /// Exclusive upper bound as a UTC timestamp.
    pub to: Option<i64>,
    /// Outages at least this long are reported.
    pub gap_threshold_seconds: i64,
}

impl RecorderArgs {
    /// Check everything that cannot be expressed in the clap declaration and resolve the
    /// timezone-dependent values.
    pub fn resolve(&self) -> Result<RecorderConfig> {
        ensure!(
            self.max_gap > 0,
            "--max-gap must be positive, got {}",
            self.max_gap
        );
        ensure!(
            self.gap_threshold_hours.is_finite() && self.gap_threshold_hours > 0.0,
            "--gap-threshold-hours must be a positive number, got {}",
            self.gap_threshold_hours
        );
        if let Some(scale) = self.scale {
            ensure!(
                scale.is_finite() && scale != 0.0,
                "--scale must be a non-zero number, got {scale}"
            );
        }

        let tz = resolve_timezone(self.tz.as_deref())?;
        let from = self
            .from
            .as_deref()
            .map(|date| parse_local_date(date, tz))
            .transpose()?;
        let to = self
            .to
            .as_deref()
            .map(|date| parse_local_date(date, tz))
            .transpose()?;
        if let (Some(from), Some(to)) = (from, to)
            && to <= from
        {
            bail!("--to must be after --from");
        }

        Ok(RecorderConfig {
            tz,
            from,
            to,
            gap_threshold_seconds: (self.gap_threshold_hours * 3_600.0).round() as i64,
        })
    }

    /// Open the database read-only, saying how under `-v`.
    pub fn open(&self, verbose: bool) -> Result<Connection> {
        let conn = source::open_database(&self.db)?;
        if verbose {
            eprintln!(
                "opened {} {}",
                self.db.display(),
                source::open_mode(&self.db).describe()
            );
        }
        Ok(conn)
    }

    /// Print what `--list-entities` asked for, if it was given.
    ///
    /// Returns whether it was, in which case the listing is the whole run.
    pub fn print_listing(&self, conn: &Connection, tz: Tz) -> Result<bool> {
        let Some(filter) = &self.list_entities else {
            return Ok(false);
        };
        let candidates = catalog::list_statistics(conn, Some(filter))?;
        if candidates.is_empty() {
            println!(
                "No statistics in {}{}.",
                self.db.display(),
                if filter.trim().is_empty() {
                    String::new()
                } else {
                    format!(" matching {filter:?}")
                }
            );
        } else {
            print!(
                "{}",
                catalog::format_listing(&candidates, tz, LISTING_LIMIT)
            );
        }
        Ok(true)
    }

    /// What the loader needs to read one entity under these options.
    pub fn load_options<'a>(
        &self,
        entity: &'a str,
        config: &RecorderConfig,
        clamp_negative: bool,
    ) -> LoadOptions<'a> {
        LoadOptions {
            entity,
            source: self.source,
            value: self.stat,
            from: config.from,
            to: config.to,
            max_gap: self.max_gap,
            scale: self.scale,
            clamp_negative,
            tz: config.tz,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono_tz::Europe::Berlin;
    use clap::Parser;

    use crate::timeutil::parse_ha_datetime;

    /// The smallest tool there is: nothing but the shared options.
    #[derive(Debug, Parser)]
    #[command(allow_negative_numbers = true)]
    struct Tool {
        #[command(flatten)]
        recorder: RecorderArgs,
    }

    fn args(extra: &[&str]) -> RecorderArgs {
        let mut argv = vec!["tool", "--db", "ha.db"];
        argv.extend_from_slice(extra);
        Tool::try_parse_from(argv)
            .expect("arguments parse")
            .recorder
    }

    #[test]
    fn defaults_are_the_documented_ones() {
        let args = args(&[]);
        assert_eq!(args.db, PathBuf::from("ha.db"));
        assert_eq!(args.list_entities, None);
        assert_eq!(args.source, SourceKind::Auto);
        assert_eq!(args.stat, ValueColumn::Mean);
        assert_eq!(args.max_gap, 900);
        assert_eq!(args.scale, None);
        assert_eq!(args.gap_threshold_hours, 24.0);

        let config = args.resolve().unwrap();
        assert_eq!(config.from, None);
        assert_eq!(config.to, None);
        assert_eq!(config.gap_threshold_seconds, 24 * 3_600);
    }

    #[test]
    fn the_database_is_required() {
        assert!(Tool::try_parse_from(["tool"]).is_err());
    }

    #[test]
    fn listing_takes_an_optional_filter() {
        assert_eq!(
            args(&["--list-entities"]).list_entities.as_deref(),
            Some("")
        );
        assert_eq!(
            args(&["--list-entities", "solar"]).list_entities.as_deref(),
            Some("solar")
        );
    }

    #[test]
    fn rejects_impossible_numbers() {
        assert!(args(&["--max-gap", "0"]).resolve().is_err());
        assert!(args(&["--max-gap", "-60"]).resolve().is_err());
        assert!(args(&["--gap-threshold-hours", "0"]).resolve().is_err());
        assert!(args(&["--gap-threshold-hours", "-6"]).resolve().is_err());
        assert!(args(&["--scale", "0"]).resolve().is_err());
        assert!(args(&["--scale", "1000"]).resolve().is_ok());
        assert!(
            args(&["--scale", "-1"]).resolve().is_ok(),
            "a sign flip is fine"
        );
    }

    #[test]
    fn resolves_dates_in_the_requested_timezone() {
        let config = args(&[
            "--tz",
            "Europe/Berlin",
            "--from",
            "2024-06-21",
            "--to",
            "2024-06-22",
            "--gap-threshold-hours",
            "6",
        ])
        .resolve()
        .unwrap();
        assert_eq!(config.tz, Berlin);
        assert_eq!(config.from, parse_ha_datetime("2024-06-20 22:00:00"));
        assert_eq!(config.to, parse_ha_datetime("2024-06-21 22:00:00"));
        assert_eq!(config.gap_threshold_seconds, 6 * 3_600);
    }

    #[test]
    fn rejects_bad_timezones_and_dates() {
        assert!(args(&["--tz", "Mars/Olympus_Mons"]).resolve().is_err());
        assert!(args(&["--from", "yesterday"]).resolve().is_err());
        for to in ["2024-06-21", "2024-06-20"] {
            assert!(
                args(&["--from", "2024-06-21", "--to", to])
                    .resolve()
                    .is_err(),
                "--to {to} is not after --from"
            );
        }
    }

    #[test]
    fn load_options_carry_the_resolved_window() {
        let args = args(&[
            "--tz",
            "Europe/Berlin",
            "--from",
            "2024-06-21",
            "--source",
            "states",
            "--stat",
            "max",
            "--max-gap",
            "60",
            "--scale",
            "1000",
        ]);
        let config = args.resolve().unwrap();
        let options = args.load_options("sensor.x", &config, false);
        assert_eq!(options.entity, "sensor.x");
        assert_eq!(options.source, SourceKind::States);
        assert_eq!(options.value, ValueColumn::Max);
        assert_eq!(options.from, config.from);
        assert_eq!(options.to, None);
        assert_eq!(options.max_gap, 60);
        assert_eq!(options.scale, Some(1000.0));
        assert!(!options.clamp_negative);
        assert_eq!(options.tz, Berlin);
    }
}
