//! Command line surface and the validation of everything the user can type.

use std::path::PathBuf;

use anyhow::{Result, bail, ensure};
use chrono_tz::Tz;
use clap::Parser;

use crate::model::{Grouping, Metric};
use crate::render::color;
use crate::source::SourceKind;
use crate::source::schema::ValueColumn;
use crate::timeutil::{parse_local_date, resolve_timezone};

/// Build a "how likely is this much solar power?" heatmap from a Home Assistant
/// recorder database.
#[derive(Debug, Clone, Parser)]
// Negative numbers are accepted so that a typo like `--step-watts -50` is answered with
// a message about the value rather than about an unknown flag.
#[command(name = "pv-probability", version, about, long_about = None, allow_negative_numbers = true)]
pub struct Args {
    /// Path to a copy of home-assistant_v2.db (opened read-only).
    #[arg(long, value_name = "FILE")]
    pub db: PathBuf,

    /// Entity to analyse, e.g. sensor.solar_power.
    #[arg(long, value_name = "ENTITY_ID")]
    pub entity: String,

    /// One heatmap per calendar month or per ISO week.
    #[arg(long, value_enum, default_value_t = Grouping::Month)]
    pub group: Grouping,

    /// Height of one power bucket, in watts.
    #[arg(long, value_name = "WATTS", default_value_t = 50.0)]
    pub step_watts: f64,

    /// Top of the power axis; defaults to a high quantile of the data.
    #[arg(long, value_name = "WATTS")]
    pub max_watts: Option<f64>,

    /// Quantile used to pick the power axis when --max-watts is not given.
    #[arg(long, value_name = "Q", default_value_t = 0.999)]
    pub max_quantile: f64,

    /// Which recorder table to read.
    #[arg(long, value_enum, default_value_t = SourceKind::Auto)]
    pub source: SourceKind,

    /// Which statistics column to use when reading a statistics table.
    #[arg(long, value_enum, default_value_t = ValueColumn::Mean)]
    pub stat: ValueColumn,

    /// IANA timezone for the hour-of-day axis, e.g. Europe/Berlin. Defaults to the
    /// machine's timezone.
    #[arg(long, value_name = "TZ")]
    pub tz: Option<String>,

    /// Only consider data from this local date on (YYYY-MM-DD).
    #[arg(long, value_name = "DATE")]
    pub from: Option<String>,

    /// Only consider data before this local date (YYYY-MM-DD).
    #[arg(long, value_name = "DATE")]
    pub to: Option<String>,

    /// Cell meaning: exceedance is "at least this many watts".
    #[arg(long, value_enum, default_value_t = Metric::Exceedance)]
    pub metric: Metric,

    /// Hours backed by fewer distinct days than this are drawn as "not enough data".
    ///
    /// Days, not readings: one chatty morning is one day of evidence however many rows
    /// the recorder wrote.
    #[arg(long, value_name = "N", default_value_t = 3)]
    pub min_days: u32,

    /// Report outages longer than this. The default of a full day keeps inverters that
    /// go offline every night from filling the report with nightly "gaps".
    #[arg(long, value_name = "HOURS", default_value_t = 24.0)]
    pub gap_threshold_hours: f64,

    /// Cells below this probability are left blank.
    #[arg(long, value_name = "P", default_value_t = 0.005)]
    pub min_probability: f64,

    /// Colour ramp shaping; below 1 lifts rare-but-real cells out of the background.
    #[arg(long, value_name = "G", default_value_t = 0.6)]
    pub gamma: f64,

    /// Number of colour steps on the likelihood scale.
    #[arg(long, value_name = "N", default_value_t = color::DEFAULT_LEVELS)]
    pub levels: usize,

    /// How long a single raw state reading is assumed to stay valid, in seconds.
    #[arg(long, value_name = "SECONDS", default_value_t = 900)]
    pub max_gap: i64,

    /// Multiply every reading by this factor (use 1000 for a sensor reporting kW).
    #[arg(long, value_name = "FACTOR")]
    pub scale: Option<f64>,

    /// Keep negative readings instead of clamping them to zero.
    #[arg(long)]
    pub keep_negative: bool,

    /// Limit the rayon worker threads; defaults to one per core.
    #[arg(long, value_name = "N")]
    pub threads: Option<usize>,

    /// Where to write the HTML report.
    #[arg(
        long,
        short,
        value_name = "FILE",
        default_value = "pv-probability.html"
    )]
    pub out: PathBuf,

    /// Print timings and data ranges while working.
    #[arg(long, short, action = clap::ArgAction::Count)]
    pub verbose: u8,
}

/// Command line arguments after validation, with dates and timezones resolved.
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub tz: Tz,
    /// Inclusive lower bound as a UTC timestamp.
    pub from: Option<i64>,
    /// Exclusive upper bound as a UTC timestamp.
    pub to: Option<i64>,
    pub clamp_negative: bool,
    /// Outages at least this long are reported.
    pub gap_threshold_seconds: i64,
}

impl Args {
    /// Check everything that cannot be expressed in the clap declaration and resolve the
    /// timezone-dependent values.
    pub fn resolve(&self) -> Result<Config> {
        ensure!(
            self.step_watts.is_finite() && self.step_watts > 0.0,
            "--step-watts must be a positive number, got {}",
            self.step_watts
        );
        if let Some(max) = self.max_watts {
            ensure!(
                max.is_finite() && max > 0.0,
                "--max-watts must be a positive number, got {max}"
            );
            ensure!(
                max >= self.step_watts,
                "--max-watts ({max}) must be at least one --step-watts ({})",
                self.step_watts
            );
        }
        ensure!(
            (0.0..=1.0).contains(&self.max_quantile),
            "--max-quantile must be between 0 and 1, got {}",
            self.max_quantile
        );
        ensure!(
            (0.0..=1.0).contains(&self.min_probability),
            "--min-probability must be between 0 and 1, got {}",
            self.min_probability
        );
        ensure!(
            self.gamma.is_finite() && self.gamma > 0.0,
            "--gamma must be a positive number, got {}",
            self.gamma
        );
        ensure!(
            (color::MIN_LEVELS..=color::MAX_LEVELS).contains(&self.levels),
            "--levels must be between {} and {}, got {}",
            color::MIN_LEVELS,
            color::MAX_LEVELS,
            self.levels
        );
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
        if let Some(threads) = self.threads {
            ensure!(threads > 0, "--threads must be at least 1");
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

        Ok(Config {
            tz,
            from,
            to,
            clamp_negative: !self.keep_negative,
            gap_threshold_seconds: (self.gap_threshold_hours * 3_600.0).round() as i64,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono_tz::Europe::Berlin;

    use crate::timeutil::parse_ha_datetime;

    fn args(extra: &[&str]) -> Args {
        let mut argv = vec![
            "pv-probability",
            "--db",
            "ha.db",
            "--entity",
            "sensor.solar_power",
        ];
        argv.extend_from_slice(extra);
        Args::try_parse_from(argv).expect("arguments parse")
    }

    #[test]
    fn defaults_are_the_documented_ones() {
        let args = args(&[]);
        assert_eq!(args.group, Grouping::Month);
        assert_eq!(args.step_watts, 50.0);
        assert_eq!(args.metric, Metric::Exceedance);
        assert_eq!(args.source, SourceKind::Auto);
        assert_eq!(args.stat, ValueColumn::Mean);
        assert_eq!(args.max_gap, 900);
        assert_eq!(args.min_days, 3);
        assert_eq!(args.gap_threshold_hours, 24.0);
        assert_eq!(args.out, PathBuf::from("pv-probability.html"));
        assert!(!args.keep_negative);

        let config = args.resolve().unwrap();
        assert!(config.clamp_negative);
        assert_eq!(config.from, None);
        assert_eq!(config.to, None);
        assert_eq!(config.gap_threshold_seconds, 24 * 3_600);
    }

    #[test]
    fn required_arguments_are_required() {
        assert!(Args::try_parse_from(["pv-probability"]).is_err());
        assert!(Args::try_parse_from(["pv-probability", "--db", "ha.db"]).is_err());
    }

    #[test]
    fn value_enums_accept_kebab_case_names() {
        assert_eq!(args(&["--group", "week"]).group, Grouping::Week);
        assert_eq!(args(&["--metric", "density"]).metric, Metric::Density);
        assert_eq!(
            args(&["--source", "short-term"]).source,
            SourceKind::ShortTerm
        );
        assert_eq!(args(&["--stat", "max"]).stat, ValueColumn::Max);
        assert!(
            Args::try_parse_from([
                "pv-probability",
                "--db",
                "ha.db",
                "--entity",
                "sensor.x",
                "--group",
                "fortnight",
            ])
            .is_err()
        );
    }

    #[test]
    fn rejects_impossible_numbers() {
        assert!(args(&["--step-watts", "0"]).resolve().is_err());
        assert!(args(&["--step-watts", "-50"]).resolve().is_err());
        assert!(args(&["--max-watts", "0"]).resolve().is_err());
        assert!(
            args(&["--step-watts", "500", "--max-watts", "100"])
                .resolve()
                .is_err()
        );
        assert!(args(&["--max-quantile", "1.5"]).resolve().is_err());
        assert!(args(&["--min-probability", "-0.1"]).resolve().is_err());
        assert!(args(&["--gamma", "0"]).resolve().is_err());
        assert!(args(&["--max-gap", "0"]).resolve().is_err());
        assert!(args(&["--levels", "2"]).resolve().is_err());
        assert!(args(&["--levels", "17"]).resolve().is_err());
        assert!(args(&["--gap-threshold-hours", "0"]).resolve().is_err());
        assert!(args(&["--gap-threshold-hours", "-6"]).resolve().is_err());
        assert!(args(&["--scale", "0"]).resolve().is_err());
        assert!(args(&["--threads", "0"]).resolve().is_err());
    }

    #[test]
    fn accepts_sensible_numbers() {
        assert!(
            args(&["--step-watts", "250", "--max-watts", "10000"])
                .resolve()
                .is_ok()
        );
        assert!(args(&["--max-quantile", "1"]).resolve().is_ok());
        assert!(args(&["--min-probability", "0"]).resolve().is_ok());
        assert!(args(&["--scale", "1000"]).resolve().is_ok());
        assert!(args(&["--levels", "12"]).resolve().is_ok());
        assert_eq!(
            args(&["--gap-threshold-hours", "6"])
                .resolve()
                .unwrap()
                .gap_threshold_seconds,
            6 * 3_600
        );
        // A day count of zero is legitimate: it means "show me everything".
        assert!(args(&["--min-days", "0"]).resolve().is_ok());
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
        ])
        .resolve()
        .unwrap();
        assert_eq!(config.tz, Berlin);
        assert_eq!(config.from, parse_ha_datetime("2024-06-20 22:00:00"));
        assert_eq!(config.to, parse_ha_datetime("2024-06-21 22:00:00"));
    }

    #[test]
    fn rejects_bad_timezones_and_dates() {
        assert!(args(&["--tz", "Mars/Olympus_Mons"]).resolve().is_err());
        assert!(args(&["--from", "yesterday"]).resolve().is_err());
        assert!(
            args(&["--from", "2024-06-22", "--to", "2024-06-21"])
                .resolve()
                .is_err()
        );
        assert!(
            args(&["--from", "2024-06-21", "--to", "2024-06-21"])
                .resolve()
                .is_err()
        );
    }

    #[test]
    fn keep_negative_disables_clamping() {
        assert!(!args(&["--keep-negative"]).resolve().unwrap().clamp_negative);
    }
}
