//! Command line surface for `energy-storage-payback-period`.

use std::path::PathBuf;

use anyhow::{Result, bail, ensure};
use chrono_tz::Tz;
use clap::Parser;

use crate::source::SourceKind;
use crate::source::schema::ValueColumn;
use crate::storage::simulate::BatterySpec;
use crate::storage::sweep::{Economics, size_range};
use crate::timeutil::{parse_local_date, resolve_timezone};

/// Work out how long a home battery would take to pay for itself, from the grid import
/// and export power sensors a Home Assistant recorder database already holds.
#[derive(Debug, Clone, Parser)]
// Negative numbers are accepted so that a typo like `--price-per-kwh -0.3` is answered
// with a message about the value rather than about an unknown flag.
#[command(name = "energy-storage-payback-period", version, about, long_about = None, allow_negative_numbers = true)]
pub struct Args {
    /// Path to a copy of home-assistant_v2.db (opened read-only).
    #[arg(long, value_name = "FILE")]
    pub db: PathBuf,

    /// Power sensor for energy drawn from the grid, e.g. sensor.grid_import_power.
    #[arg(
        long,
        value_name = "ENTITY_ID",
        required_unless_present = "list_entities"
    )]
    pub import_entity: Option<String>,

    /// Power sensor for energy fed back into the grid, e.g. sensor.grid_export_power.
    #[arg(
        long,
        value_name = "ENTITY_ID",
        required_unless_present = "list_entities"
    )]
    pub export_entity: Option<String>,

    /// List the statistics this database holds and exit, power sensors first.
    #[arg(long, value_name = "FILTER", num_args = 0..=1, default_missing_value = "")]
    pub list_entities: Option<String>,

    /// IANA timezone used for the dates in the report. Defaults to the machine's.
    #[arg(long, value_name = "TZ")]
    pub tz: Option<String>,

    /// Only consider data from this local date on (YYYY-MM-DD).
    #[arg(long, value_name = "DATE")]
    pub from: Option<String>,

    /// Only consider data before this local date (YYYY-MM-DD).
    #[arg(long, value_name = "DATE")]
    pub to: Option<String>,

    /// Which recorder table to read.
    #[arg(long, value_enum, default_value_t = SourceKind::Auto)]
    pub source: SourceKind,

    /// Which statistics column to use when reading a statistics table.
    #[arg(long, value_enum, default_value_t = ValueColumn::Mean)]
    pub stat: ValueColumn,

    /// Length of one simulation slot, in minutes. Must divide an hour.
    ///
    /// Hourly is the finest an entire year of statistics offers; five pairs naturally
    /// with `--source short-term`, which covers about ten days.
    #[arg(long, value_name = "MINUTES", default_value_t = 60)]
    pub slot_minutes: i64,

    /// How much of a slot both sensors must have covered for it to be simulated.
    #[arg(long, value_name = "F", default_value_t = 0.9)]
    pub min_slot_coverage: f64,

    /// What a kilowatt hour from the grid costs.
    #[arg(long, value_name = "PRICE", default_value_t = DEFAULT_PRICE_PER_KWH)]
    pub price_per_kwh: f64,

    /// What a kilowatt hour fed back earns. Zero when the export is gifted.
    #[arg(long, value_name = "PRICE", default_value_t = 0.0)]
    pub feed_in_price: f64,

    /// Installed battery cost per kilowatt hour of capacity.
    #[arg(long, value_name = "COST", default_value_t = DEFAULT_COST_PER_KWH)]
    pub cost_per_kwh: f64,

    /// The part of the bill that does not scale with capacity: inverter, wiring, labour.
    #[arg(long, value_name = "COST", default_value_t = DEFAULT_BASE_COST)]
    pub base_cost: f64,

    /// Currency symbol used in the report.
    #[arg(long, value_name = "SYMBOL", default_value = "EUR")]
    pub currency: String,

    /// The payback period worth aiming for, in years.
    ///
    /// The report answers the question backwards as well: what the installation would
    /// have to cost for each size to be square within this many years.
    #[arg(long, value_name = "YEARS", default_value_t = DEFAULT_TARGET_PAYBACK_YEARS)]
    pub target_payback_years: f64,

    /// Smallest battery to try, in kWh.
    #[arg(long, value_name = "KWH", default_value_t = 1.0)]
    pub min_size: f64,

    /// Largest battery to try, in kWh.
    #[arg(long, value_name = "KWH", default_value_t = 20.0)]
    pub max_size: f64,

    /// Step between the sizes tried, in kWh.
    ///
    /// Sizes are simulated in parallel, so a finer sweep costs little wall-clock time.
    #[arg(long, value_name = "KWH", default_value_t = 1.0)]
    pub size_step: f64,

    /// Exact sizes to try instead of the range, e.g. --sizes 5,10,13.5.
    #[arg(long, value_name = "KWH", value_delimiter = ',', num_args = 1..)]
    pub sizes: Option<Vec<f64>>,

    /// Round trip efficiency, charged half on the way in and half on the way out.
    #[arg(long, value_name = "F", default_value_t = 0.9)]
    pub round_trip: f64,

    /// Share of the nameplate capacity a battery is actually cycled over.
    #[arg(long, value_name = "F", default_value_t = 0.9)]
    pub usable_fraction: f64,

    /// Charge power ceiling in kW; unlimited when absent.
    #[arg(long, value_name = "KW")]
    pub max_charge_kw: Option<f64>,

    /// Discharge power ceiling in kW; unlimited when absent.
    #[arg(long, value_name = "KW")]
    pub max_discharge_kw: Option<f64>,

    /// How far either side of the price and cost the sensitivity block looks, in percent.
    #[arg(long, value_name = "PCT", default_value_t = 25.0)]
    pub sensitivity: f64,

    /// Report outages longer than this, in hours.
    #[arg(long, value_name = "HOURS", default_value_t = 24.0)]
    pub gap_threshold_hours: f64,

    /// How long a single raw state reading is assumed to stay valid, in seconds.
    #[arg(long, value_name = "SECONDS", default_value_t = 900)]
    pub max_gap: i64,

    /// Multiply every reading by this factor (use 1000 for a sensor reporting kW).
    #[arg(long, value_name = "FACTOR")]
    pub scale: Option<f64>,

    /// Limit the rayon worker threads; defaults to one per core.
    #[arg(long, value_name = "N")]
    pub threads: Option<usize>,

    /// Where to write the HTML report.
    #[arg(
        long,
        short,
        value_name = "FILE",
        default_value = "energy-storage-payback-period.html"
    )]
    pub out: PathBuf,

    /// Print timings and row counts while working.
    #[arg(long, short, action = clap::ArgAction::Count)]
    pub verbose: u8,
}

/// Defaults worth naming: a German household tariff and a mid-range installed battery
/// price, both easy to override and both stated in the report so the answer is never
/// mistaken for a quote.
pub const DEFAULT_PRICE_PER_KWH: f64 = 0.35;
pub const DEFAULT_COST_PER_KWH: f64 = 500.0;
pub const DEFAULT_BASE_COST: f64 = 1_500.0;

/// Five years is the period people usually have in mind when they ask whether a battery
/// is worth it; long enough to be plausible, short enough to be a real test.
pub const DEFAULT_TARGET_PAYBACK_YEARS: f64 = 5.0;

/// Arguments after validation, with dates, sizes and assumptions resolved.
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub tz: Tz,
    pub from: Option<i64>,
    pub to: Option<i64>,
    pub slot_seconds: i64,
    pub min_slot_coverage: f64,
    pub sizes: Vec<f64>,
    pub battery: BatterySpec,
    pub economics: Economics,
    pub gap_threshold_seconds: i64,
    /// Sensitivity as a fraction, e.g. 0.25.
    pub sensitivity: f64,
    /// The payback period the report works backwards from.
    pub target_payback_years: f64,
}

impl Args {
    /// The sensor to read grid import from. Only absent in listing mode, which clap
    /// enforces.
    pub fn import_entity(&self) -> &str {
        self.import_entity.as_deref().unwrap_or_default()
    }

    pub fn export_entity(&self) -> &str {
        self.export_entity.as_deref().unwrap_or_default()
    }

    /// Check everything clap cannot express, and resolve what depends on a timezone.
    pub fn resolve(&self) -> Result<Config> {
        for (name, value) in [
            ("--price-per-kwh", self.price_per_kwh),
            ("--cost-per-kwh", self.cost_per_kwh),
        ] {
            ensure!(
                value.is_finite() && value > 0.0,
                "{name} must be a positive number, got {value}"
            );
        }
        for (name, value) in [
            ("--feed-in-price", self.feed_in_price),
            ("--base-cost", self.base_cost),
        ] {
            ensure!(
                value.is_finite() && value >= 0.0,
                "{name} must not be negative, got {value}"
            );
        }
        for (name, value) in [
            ("--round-trip", self.round_trip),
            ("--usable-fraction", self.usable_fraction),
            ("--min-slot-coverage", self.min_slot_coverage),
        ] {
            ensure!(
                value.is_finite() && value > 0.0 && value <= 1.0,
                "{name} must be greater than 0 and at most 1, got {value}"
            );
        }
        for (name, value) in [
            ("--max-charge-kw", self.max_charge_kw),
            ("--max-discharge-kw", self.max_discharge_kw),
        ] {
            if let Some(value) = value {
                ensure!(
                    value.is_finite() && value > 0.0,
                    "{name} must be a positive number, got {value}"
                );
            }
        }
        ensure!(
            self.slot_minutes > 0 && 60 % self.slot_minutes == 0,
            "--slot-minutes must divide an hour (1, 2, 3, 4, 5, 6, 10, 12, 15, 20, 30 or \
             60), got {}",
            self.slot_minutes
        );
        ensure!(
            self.sensitivity.is_finite() && (0.0..100.0).contains(&self.sensitivity),
            "--sensitivity must be between 0 and 100 percent, got {}",
            self.sensitivity
        );
        ensure!(
            self.target_payback_years.is_finite() && self.target_payback_years > 0.0,
            "--target-payback-years must be a positive number, got {}",
            self.target_payback_years
        );
        ensure!(
            self.gap_threshold_hours.is_finite() && self.gap_threshold_hours > 0.0,
            "--gap-threshold-hours must be a positive number, got {}",
            self.gap_threshold_hours
        );
        ensure!(
            self.max_gap > 0,
            "--max-gap must be positive, got {}",
            self.max_gap
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

        let sizes = match &self.sizes {
            Some(sizes) => {
                ensure!(!sizes.is_empty(), "--sizes needs at least one capacity");
                for size in sizes {
                    ensure!(
                        size.is_finite() && *size > 0.0,
                        "--sizes must be positive numbers, got {size}"
                    );
                }
                let mut sizes = sizes.clone();
                sizes.sort_by(f64::total_cmp);
                sizes.dedup();
                sizes
            }
            None => {
                ensure!(
                    self.min_size.is_finite() && self.min_size > 0.0,
                    "--min-size must be a positive number, got {}",
                    self.min_size
                );
                ensure!(
                    self.max_size >= self.min_size,
                    "--max-size ({}) must be at least --min-size ({})",
                    self.max_size,
                    self.min_size
                );
                ensure!(
                    self.size_step.is_finite() && self.size_step > 0.0,
                    "--size-step must be a positive number, got {}",
                    self.size_step
                );
                let sizes = size_range(self.min_size, self.max_size, self.size_step);
                ensure!(!sizes.is_empty(), "the size range covers no battery at all");
                sizes
            }
        };

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
            slot_seconds: self.slot_minutes * 60,
            min_slot_coverage: self.min_slot_coverage,
            sizes,
            battery: BatterySpec {
                capacity_kwh: 0.0,
                usable_fraction: self.usable_fraction,
                round_trip: self.round_trip,
                max_charge_kw: self.max_charge_kw,
                max_discharge_kw: self.max_discharge_kw,
            },
            economics: Economics {
                price_per_kwh: self.price_per_kwh,
                feed_in_price: self.feed_in_price,
                cost_per_kwh: self.cost_per_kwh,
                base_cost: self.base_cost,
                currency: self.currency.clone(),
            },
            gap_threshold_seconds: (self.gap_threshold_hours * 3_600.0).round() as i64,
            sensitivity: self.sensitivity / 100.0,
            target_payback_years: self.target_payback_years,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono_tz::Europe::Berlin;

    fn parse(argv: &[&str]) -> Result<Args, clap::Error> {
        let mut all = vec!["energy-storage-payback-period"];
        all.extend_from_slice(argv);
        Args::try_parse_from(all)
    }

    fn args(extra: &[&str]) -> Args {
        let mut argv = vec![
            "energy-storage-payback-period",
            "--db",
            "ha.db",
            "--import-entity",
            "sensor.grid_import_power",
            "--export-entity",
            "sensor.grid_export_power",
        ];
        argv.extend_from_slice(extra);
        Args::try_parse_from(argv).expect("arguments parse")
    }

    #[test]
    fn defaults_are_the_documented_ones() {
        let args = args(&[]);
        assert_eq!(args.price_per_kwh, 0.35);
        assert_eq!(args.feed_in_price, 0.0, "the export is gifted by default");
        assert_eq!(args.cost_per_kwh, 500.0);
        assert_eq!(args.base_cost, 1_500.0);
        assert_eq!(args.slot_minutes, 60);
        assert_eq!(args.target_payback_years, 5.0);
        assert_eq!(args.round_trip, 0.9);
        assert_eq!(args.usable_fraction, 0.9);
        assert_eq!(args.source, SourceKind::Auto);
        assert_eq!(args.stat, ValueColumn::Mean);
        assert_eq!(
            args.out,
            PathBuf::from("energy-storage-payback-period.html")
        );

        let config = args.resolve().unwrap();
        assert_eq!(config.slot_seconds, 3_600);
        assert_eq!(config.sizes.first(), Some(&1.0));
        assert_eq!(config.sizes.last(), Some(&20.0));
        assert_eq!(config.sizes.len(), 20);
        assert_eq!(config.sensitivity, 0.25);
        assert_eq!(config.target_payback_years, 5.0);
        assert_eq!(config.gap_threshold_seconds, 24 * 3_600);
        assert_eq!(config.battery.capacity_kwh, 0.0, "filled in per size");
    }

    #[test]
    fn both_sensors_are_required_unless_listing() {
        assert!(parse(&["--db", "ha.db"]).is_err());
        assert!(parse(&["--db", "ha.db", "--import-entity", "sensor.a"]).is_err());
        assert!(parse(&["--db", "ha.db", "--export-entity", "sensor.b"]).is_err());
        let listing = parse(&["--db", "ha.db", "--list-entities"]).expect("no sensors needed");
        assert_eq!(listing.list_entities.as_deref(), Some(""));
        assert_eq!(listing.import_entity(), "");

        let filtered = parse(&["--db", "ha.db", "--list-entities", "grid"]).unwrap();
        assert_eq!(filtered.list_entities.as_deref(), Some("grid"));
    }

    #[test]
    fn explicit_sizes_replace_the_range_and_are_sorted() {
        let config = args(&["--sizes", "13.5,5,10,5"]).resolve().unwrap();
        assert_eq!(config.sizes, vec![5.0, 10.0, 13.5]);
    }

    #[test]
    fn rejects_impossible_numbers() {
        assert!(args(&["--price-per-kwh", "0"]).resolve().is_err());
        assert!(args(&["--price-per-kwh", "-0.3"]).resolve().is_err());
        assert!(args(&["--cost-per-kwh", "0"]).resolve().is_err());
        assert!(args(&["--base-cost", "-1"]).resolve().is_err());
        assert!(args(&["--feed-in-price", "-0.1"]).resolve().is_err());
        assert!(args(&["--round-trip", "0"]).resolve().is_err());
        assert!(args(&["--round-trip", "1.2"]).resolve().is_err());
        assert!(args(&["--usable-fraction", "0"]).resolve().is_err());
        assert!(args(&["--min-slot-coverage", "1.5"]).resolve().is_err());
        assert!(args(&["--slot-minutes", "7"]).resolve().is_err());
        assert!(args(&["--slot-minutes", "0"]).resolve().is_err());
        assert!(args(&["--sensitivity", "-5"]).resolve().is_err());
        assert!(args(&["--sensitivity", "100"]).resolve().is_err());
        assert!(args(&["--target-payback-years", "0"]).resolve().is_err());
        assert!(args(&["--target-payback-years", "-5"]).resolve().is_err());
        assert!(args(&["--min-size", "0"]).resolve().is_err());
        assert!(
            args(&["--min-size", "10", "--max-size", "5"])
                .resolve()
                .is_err()
        );
        assert!(args(&["--size-step", "0"]).resolve().is_err());
        assert!(args(&["--sizes", "-5"]).resolve().is_err());
        assert!(args(&["--max-charge-kw", "0"]).resolve().is_err());
        assert!(args(&["--max-gap", "0"]).resolve().is_err());
        assert!(args(&["--scale", "0"]).resolve().is_err());
        assert!(args(&["--threads", "0"]).resolve().is_err());
        assert!(args(&["--gap-threshold-hours", "0"]).resolve().is_err());
    }

    #[test]
    fn accepts_sensible_numbers() {
        assert!(args(&["--slot-minutes", "5"]).resolve().is_ok());
        assert!(args(&["--slot-minutes", "15"]).resolve().is_ok());
        assert!(args(&["--round-trip", "1"]).resolve().is_ok());
        assert!(args(&["--feed-in-price", "0"]).resolve().is_ok());
        assert!(args(&["--sensitivity", "0"]).resolve().is_ok());
        assert_eq!(
            args(&["--target-payback-years", "8.5"])
                .resolve()
                .unwrap()
                .target_payback_years,
            8.5
        );
        assert!(
            args(&["--min-size", "5", "--max-size", "5"])
                .resolve()
                .is_ok(),
            "a single size is a legitimate question"
        );
        let config = args(&["--max-charge-kw", "5", "--max-discharge-kw", "3.5"])
            .resolve()
            .unwrap();
        assert_eq!(config.battery.max_charge_kw, Some(5.0));
        assert_eq!(config.battery.max_discharge_kw, Some(3.5));
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
        assert!(config.from.unwrap() < config.to.unwrap());
        assert!(
            args(&["--from", "2024-06-22", "--to", "2024-06-21"])
                .resolve()
                .is_err()
        );
        assert!(args(&["--tz", "Mars/Olympus_Mons"]).resolve().is_err());
    }
}
