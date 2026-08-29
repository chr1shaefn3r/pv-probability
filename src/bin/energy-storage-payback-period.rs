//! How long a home battery would take to pay for itself, from the grid import and export
//! power sensors a Home Assistant recorder database already holds.

use std::collections::HashMap;
use std::fs;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use chrono::DateTime;
use chrono_tz::Tz;
use clap::Parser;

use pv_probability::cli::open_command;
use pv_probability::coverage::Coverage;
use pv_probability::model::Grouping;
use pv_probability::render::{
    PaybackOptions, format_kwh, format_money, format_percent, format_years, payback_page,
};
use pv_probability::source::catalog;
use pv_probability::source::{self, LoadOptions, LoadedSamples};
use pv_probability::storage::cli::{Args, Config};
use pv_probability::storage::pair::{Paired, pair};
use pv_probability::storage::series::{accumulate, grid_for};
use pv_probability::storage::sweep::{Sweep, sweep};
use pv_probability::timeutil::{local_day, local_parts};

fn main() {
    if let Err(error) = run() {
        // A diagnostic is the whole point of most of these errors, so print it plainly
        // rather than letting anyhow follow it with a backtrace.
        eprintln!("Error: {error}");
        for cause in error.chain().skip(1) {
            eprintln!("  caused by: {cause}");
        }
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args = Args::parse();
    let config = args.resolve()?;

    if let Some(threads) = args.threads {
        rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .build_global()
            .context("failed to configure the worker threads")?;
    }

    let started = Instant::now();
    let conn = source::open_database(&args.db)?;

    if let Some(filter) = &args.list_entities {
        let candidates = catalog::list_statistics(&conn, Some(filter))?;
        if candidates.is_empty() {
            println!(
                "No statistics in {}{}.",
                args.db.display(),
                if filter.trim().is_empty() {
                    String::new()
                } else {
                    format!(" matching {filter:?}")
                }
            );
        } else {
            print!("{}", catalog::format_listing(&candidates, config.tz, 40));
        }
        return Ok(());
    }

    // Two independent queries against a read-only file: reading them at the same time on
    // their own connections halves the part of the run that rayon cannot help with.
    let (import, export) = rayon::join(
        || read(&args, &config, args.import_entity()),
        || read(&args, &config, args.export_entity()),
    );
    let (import, export) = (import?, export?);
    if args.verbose > 0 {
        eprintln!(
            "read {} import rows from `{}` and {} export rows from `{}` in {:.2?} on {} threads",
            import.rows,
            import.source,
            export.rows,
            export.source,
            started.elapsed(),
            rayon::current_num_threads()
        );
    }
    for (entity, loaded) in [
        (args.import_entity(), &import),
        (args.export_entity(), &export),
    ] {
        if loaded.samples.is_empty() {
            return Err(source::explain_no_samples(
                &conn,
                &load_options(&args, &config, entity),
                loaded,
                config.tz,
            ));
        }
    }

    let aggregation_started = Instant::now();
    let grid = grid_for(&import.samples, &export.samples, config.slot_seconds)
        .context("the two sensors have no readings to build a timeline from")?;
    let (import_slots, export_slots) = rayon::join(
        || accumulate(&import.samples, grid),
        || accumulate(&export.samples, grid),
    );
    let paired = pair(&import_slots, &export_slots, config.min_slot_coverage);
    if paired.is_empty() {
        bail!(
            "`{}` and `{}` never covered the same {} minute slot, so there is nothing to \
             simulate. {} slots were dropped: {} with only one sensor recording, {} covered \
             too thinly. Try a wider --from/--to, --slot-minutes 60, or a lower \
             --min-slot-coverage.",
            args.import_entity(),
            args.export_entity(),
            config.slot_seconds / 60,
            paired.dropped(),
            paired.dropped_unpaired,
            paired.dropped_partial
        );
    }

    let result = sweep(&paired, &config.sizes, &config.battery, &config.economics);
    if args.verbose > 0 {
        eprintln!(
            "simulated {} sizes over {} slots in {:.2?}",
            result.results.len(),
            paired.steps.len(),
            aggregation_started.elapsed()
        );
    }

    let samples = paired.samples();
    let (observed_days, full_days) = day_counts(&paired, config.tz);
    let coverage = Coverage::describe(
        &samples,
        observed_days,
        full_days,
        &present_months(&paired, config.tz),
        Grouping::Month,
        config.tz,
        config.gap_threshold_seconds,
    );

    let html = payback_page(
        &result,
        &paired,
        &PaybackOptions {
            import_entity: args.import_entity().to_string(),
            export_entity: args.export_entity().to_string(),
            metadata: metadata(&args, &config, &import, &export),
            coverage: coverage.clone(),
            tz: config.tz,
            sensitivity: config.sensitivity,
            target_payback_years: config.target_payback_years,
        },
    );
    fs::write(&args.out, &html)
        .with_context(|| format!("failed to write {}", args.out.display()))?;

    report(&args, &config, &result, &paired, html.len(), started);
    if let Some(coverage) = &coverage {
        println!("{}", coverage_line(coverage, config.tz));
    }

    // The last line is the one worth copying.
    println!("\n{}", open_command(&args.out));
    Ok(())
}

/// Load one sensor on its own read-only connection.
fn read(args: &Args, config: &Config, entity: &str) -> Result<LoadedSamples> {
    let conn = source::open_database(&args.db)?;
    source::load(&conn, &load_options(args, config, entity))
}

fn load_options<'a>(args: &'a Args, config: &Config, entity: &'a str) -> LoadOptions<'a> {
    LoadOptions {
        entity,
        source: args.source,
        value: args.stat,
        from: config.from,
        to: config.to,
        max_gap: args.max_gap,
        scale: args.scale,
        // A grid meter reading below zero is noise, not generation.
        clamp_negative: true,
        tz: config.tz,
    }
}

/// What the run found, in the order it matters.
fn report(
    args: &Args,
    config: &Config,
    result: &Sweep,
    paired: &Paired,
    bytes: usize,
    started: Instant,
) {
    let currency = &config.economics.currency;
    println!(
        "wrote {}: {} sizes over {} slots of {} minutes, {:.0} kB, {:.2?}",
        args.out.display(),
        result.results.len(),
        paired.steps.len(),
        config.slot_seconds / 60,
        bytes as f64 / 1024.0,
        started.elapsed()
    );
    println!(
        "measured {} imported and {} exported",
        format_kwh(paired.import_kwh),
        format_kwh(paired.export_kwh)
    );
    let target = config.target_payback_years;
    match result.best_result() {
        Some(best) => {
            println!(
                "best payback: {} kWh for {} saves {} a year - paid off in {}",
                trim(best.capacity_kwh),
                format_money(best.investment, currency),
                format_money(best.annual_savings, currency),
                format_years(best.payback_years)
            );
            let budget = result.budget(best, target);
            let per_kwh = match budget.cost_per_kwh {
                Some(per_kwh) => format!(
                    ", or {} per kWh on top of the {} base cost",
                    format_money(per_kwh, currency),
                    format_money(config.economics.base_cost, currency)
                ),
                None => format!(
                    ", which the {} base cost alone already exceeds",
                    format_money(config.economics.base_cost, currency)
                ),
            };
            if budget.met {
                println!(
                    "for {}: already there - it could cost up to {}{}",
                    format_years(Some(target)),
                    format_money(budget.investment, currency),
                    per_kwh
                );
            } else {
                println!(
                    "for {}: it would have to cost {} instead of {} ({} less){}",
                    format_years(Some(target)),
                    format_money(budget.investment, currency),
                    format_money(budget.quoted, currency),
                    format_percent(budget.discount.unwrap_or_default()),
                    per_kwh
                );
            }
        }
        None => println!(
            "best payback: none - no battery between {} and {} kWh pays for itself here, so \
             no price would reach {}",
            trim(config.sizes.first().copied().unwrap_or_default()),
            trim(config.sizes.last().copied().unwrap_or_default()),
            format_years(Some(target))
        ),
    }
    if result.annualisation > 1.01 {
        println!(
            "note: the history is shorter than a year, so annual figures are the observed \
             period multiplied by {:.2}",
            result.annualisation
        );
    }
}

fn trim(capacity_kwh: f64) -> String {
    format!("{capacity_kwh:.2}")
        .trim_end_matches('0')
        .trim_end_matches('.')
        .to_string()
}

/// Local days the paired timeline touched, and how many of those were whole days.
fn day_counts(paired: &Paired, tz: Tz) -> (u32, u32) {
    let mut seconds_per_day: HashMap<i32, f64> = HashMap::new();
    for step in &paired.steps {
        *seconds_per_day
            .entry(local_day(step.start_ts, tz))
            .or_default() += step.seconds;
    }
    let full = seconds_per_day
        .values()
        .filter(|seconds| **seconds >= 20.0 * 3_600.0)
        .count();
    (seconds_per_day.len() as u32, full as u32)
}

fn present_months(paired: &Paired, tz: Tz) -> Vec<usize> {
    let mut months: Vec<usize> = paired
        .steps
        .iter()
        .map(|step| usize::from(local_parts(step.start_ts, tz).month) - 1)
        .collect();
    months.sort_unstable();
    months.dedup();
    months
}

/// One line saying what the recorder really covered.
fn coverage_line(coverage: &Coverage, tz: Tz) -> String {
    let (first, last) = coverage.local_dates(tz);
    let span = match (first, last) {
        (Some(first), Some(last)) => format!("{first} to {last}"),
        _ => "unknown span".to_string(),
    };
    format!(
        "covers {span} ({} days): {} days paired ({} of them full days), {} outages over {} h",
        coverage.span_days,
        coverage.observed_days,
        coverage.full_days,
        coverage.gaps.len(),
        coverage.gap_threshold_seconds / 3_600
    )
}

/// The facts worth recording in the report header.
fn metadata(
    args: &Args,
    config: &Config,
    import: &LoadedSamples,
    export: &LoadedSamples,
) -> Vec<(String, String)> {
    let mut metadata = vec![
        ("Source".to_string(), import.source.to_string()),
        ("Timezone".to_string(), config.tz.to_string()),
        (
            "Slot".to_string(),
            format!("{} minutes", config.slot_seconds / 60),
        ),
    ];
    let first = import.first_ts.into_iter().chain(export.first_ts).min();
    let last = import.last_ts.into_iter().chain(export.last_ts).max();
    if let (Some(first), Some(last)) = (first, last) {
        metadata.push((
            "Range".to_string(),
            format!(
                "{} to {}",
                local_date(first, config.tz),
                local_date(last, config.tz)
            ),
        ));
    }
    if let Some(unit) = &import.unit {
        let scaled = if import.scale == 1.0 {
            unit.clone()
        } else {
            format!("{unit} x{}", import.scale)
        };
        metadata.push(("Unit".to_string(), scaled));
    }
    metadata.push((
        "Battery".to_string(),
        format!(
            "{:.0}% usable, {:.0}% round trip{}",
            config.battery.usable_fraction * 100.0,
            config.battery.round_trip * 100.0,
            match (
                config.battery.max_charge_kw,
                config.battery.max_discharge_kw
            ) {
                (None, None) => String::new(),
                (charge, discharge) => format!(
                    ", {} charge, {} discharge",
                    power_limit(charge),
                    power_limit(discharge)
                ),
            }
        ),
    ));
    if args.source == pv_probability::source::SourceKind::States {
        metadata.push(("Max gap".to_string(), format!("{} s", args.max_gap)));
    }
    metadata
}

fn power_limit(kw: Option<f64>) -> String {
    match kw {
        Some(kw) => format!("{kw} kW"),
        None => "unlimited".to_string(),
    }
}

fn local_date(ts: i64, tz: Tz) -> String {
    DateTime::from_timestamp(ts, 0)
        .map(|utc| utc.with_timezone(&tz).format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| "?".to_string())
}
