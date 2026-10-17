//! Command line surface for `heating-energy`.

use std::path::PathBuf;

use anyhow::{Result, ensure};
use clap::Parser;

use ha_recorder::model::Grouping;
use ha_recorder::{RecorderArgs, RecorderConfig};

use crate::degree::{Bases, DEFAULT_COOLING_BASE, DEFAULT_HEATING_BASE};

/// Count heating and cooling degree days from an outdoor thermometer in a Home Assistant
/// recorder database, beside the indoor temperature of every room.
#[derive(Debug, Clone, Parser)]
// Negative numbers are accepted: a base temperature below zero is unusual, but it is a
// temperature, not a typo for a flag.
#[command(name = "heating-energy", version, about, long_about = None, allow_negative_numbers = true)]
pub struct Args {
    /// The database, the table, the dates: the options every tool shares.
    #[command(flatten)]
    pub recorder: RecorderArgs,

    /// Outdoor thermometer, e.g. sensor.outdoor_temperature.
    #[arg(
        long,
        value_name = "ENTITY_ID",
        required_unless_present = "list_entities"
    )]
    pub outside_entity: Option<String>,

    /// TOML file naming every floor, the rooms on it and each room's thermometer.
    ///
    /// See rooms.example.toml beside this tool's README. Without it the report covers the
    /// outside alone.
    #[arg(long, value_name = "FILE")]
    pub rooms: Option<PathBuf>,

    /// Outside temperature above which a day needs no heating, in °C.
    #[arg(long, value_name = "CELSIUS", default_value_t = DEFAULT_HEATING_BASE)]
    pub heating_base: f64,

    /// Outside temperature above which a day needs cooling, in °C.
    #[arg(long, value_name = "CELSIUS", default_value_t = DEFAULT_COOLING_BASE)]
    pub cooling_base: f64,

    /// Sum the degree days per calendar month or per ISO week.
    #[arg(long, value_enum, default_value_t = Grouping::Month)]
    pub group: Grouping,

    /// How much of a day a thermometer must have covered for the day to count.
    ///
    /// A day missing its night reads warm and would understate the heating it needed.
    #[arg(long, value_name = "F", default_value_t = 0.9)]
    pub min_day_coverage: f64,

    /// Limit the rayon worker threads; defaults to one per core.
    #[arg(long, value_name = "N")]
    pub threads: Option<usize>,

    /// Where to write the HTML report.
    #[arg(
        long,
        short,
        value_name = "FILE",
        default_value = "heating-energy.html"
    )]
    pub out: PathBuf,

    /// Print timings and row counts while working.
    #[arg(long, short, action = clap::ArgAction::Count)]
    pub verbose: u8,
}

/// Arguments after validation, with dates and bases resolved.
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub recorder: RecorderConfig,
    pub bases: Bases,
    pub min_day_coverage: f64,
}

impl Args {
    /// The outdoor thermometer. Only absent in listing mode, which clap enforces.
    pub fn outside_entity(&self) -> &str {
        self.outside_entity.as_deref().unwrap_or_default()
    }

    /// Check everything clap cannot express, and resolve what depends on a timezone.
    pub fn resolve(&self) -> Result<Config> {
        for (name, value) in [
            ("--heating-base", self.heating_base),
            ("--cooling-base", self.cooling_base),
        ] {
            ensure!(
                value.is_finite() && (-50.0..=50.0).contains(&value),
                "{name} must be an outside temperature in °C between -50 and 50, got {value}"
            );
        }
        ensure!(
            self.cooling_base >= self.heating_base,
            "--cooling-base ({}) must not be below --heating-base ({}): a day cannot need \
             heating and cooling at once",
            self.cooling_base,
            self.heating_base
        );
        ensure!(
            self.min_day_coverage.is_finite()
                && self.min_day_coverage > 0.0
                && self.min_day_coverage <= 1.0,
            "--min-day-coverage must be greater than 0 and at most 1, got {}",
            self.min_day_coverage
        );
        if let Some(threads) = self.threads {
            ensure!(threads > 0, "--threads must be at least 1");
        }

        Ok(Config {
            recorder: self.recorder.resolve()?,
            bases: Bases {
                heating: self.heating_base,
                cooling: self.cooling_base,
            },
            min_day_coverage: self.min_day_coverage,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(argv: &[&str]) -> Result<Args, clap::Error> {
        let mut all = vec!["heating-energy"];
        all.extend_from_slice(argv);
        Args::try_parse_from(all)
    }

    fn args(extra: &[&str]) -> Args {
        let mut argv = vec![
            "--db",
            "ha.db",
            "--outside-entity",
            "sensor.outdoor_temperature",
        ];
        argv.extend_from_slice(extra);
        parse(&argv).expect("arguments parse")
    }

    #[test]
    fn defaults_are_the_documented_ones() {
        let args = args(&[]);
        assert_eq!(args.heating_base, 15.0);
        assert_eq!(args.cooling_base, 21.0);
        assert_eq!(args.group, Grouping::Month);
        assert_eq!(args.min_day_coverage, 0.9);
        assert_eq!(args.rooms, None);
        assert_eq!(args.out, PathBuf::from("heating-energy.html"));

        let config = args.resolve().unwrap();
        assert_eq!(config.bases, Bases::default());
    }

    #[test]
    fn the_outside_thermometer_is_required_unless_listing() {
        assert!(parse(&["--db", "ha.db"]).is_err());
        let listing = parse(&["--db", "ha.db", "--list-entities", "temp"]).unwrap();
        assert_eq!(listing.recorder.list_entities.as_deref(), Some("temp"));
        assert_eq!(listing.outside_entity(), "");
    }

    #[test]
    fn rejects_impossible_numbers() {
        assert!(args(&["--heating-base", "nan"]).resolve().is_err());
        assert!(args(&["--heating-base", "80"]).resolve().is_err());
        assert!(
            args(&["--heating-base", "18", "--cooling-base", "16"])
                .resolve()
                .is_err(),
            "the bases cannot overlap"
        );
        assert!(args(&["--min-day-coverage", "0"]).resolve().is_err());
        assert!(args(&["--min-day-coverage", "1.5"]).resolve().is_err());
        assert!(args(&["--threads", "0"]).resolve().is_err());
        assert!(
            args(&["--max-gap", "0"]).resolve().is_err(),
            "the shared options too"
        );
    }

    #[test]
    fn accepts_sensible_numbers() {
        let config = args(&["--heating-base", "18", "--cooling-base", "18"])
            .resolve()
            .unwrap();
        assert_eq!(config.bases.heating, 18.0);
        assert_eq!(
            config.bases.cooling, 18.0,
            "one base for both is the US convention"
        );
        assert!(args(&["--heating-base", "-2"]).resolve().is_ok());
        assert!(args(&["--min-day-coverage", "1"]).resolve().is_ok());
        assert_eq!(args(&["--group", "week"]).group, Grouping::Week);
        assert_eq!(
            args(&["--rooms", "house.toml"]).rooms,
            Some(PathBuf::from("house.toml"))
        );
    }
}
