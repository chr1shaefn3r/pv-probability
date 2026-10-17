//! Heating and cooling degree days from a Home Assistant outdoor thermometer, beside the
//! indoor temperature of every room.

use std::fs;
use std::time::Instant;

use anyhow::{Context, Result};
use chrono::Datelike;
use chrono_tz::Tz;
use clap::Parser;
use rayon::prelude::*;

use ha_recorder::coverage::Coverage;
use ha_recorder::model::Grouping;
use ha_recorder::source::{self, LoadedSamples, SourceKind};
use ha_recorder::{Quantity, Sample};
use ha_report::{format_duration, open_command};

use heating_energy::cli::{Args, Config};
use heating_energy::daily::{DayMean, daily_means};
use heating_energy::degree::{Analysis, analyse};
use heating_energy::report::{ReportOptions, format_celsius, format_degree_days, page};
use heating_energy::rooms::House;

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
    let tz = config.recorder.tz;

    if let Some(threads) = args.threads {
        rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .build_global()
            .context("failed to configure the worker threads")?;
    }

    let started = Instant::now();
    let conn = args.recorder.open(args.verbose > 0)?;
    if args.recorder.print_listing(&conn, tz)? {
        return Ok(());
    }
    // Read the house before the database, so a typo in the file is reported in a moment
    // rather than after every thermometer has been loaded.
    let house = args.rooms.as_deref().map(House::read).transpose()?;

    let outside = read(&args, &config, args.outside_entity())?;
    let rooms: Vec<LoadedSamples> = match &house {
        Some(house) => house
            .rooms()
            .collect::<Vec<_>>()
            .into_par_iter()
            .map(|(floor, room)| {
                read(&args, &config, &room.sensor).with_context(|| {
                    format!("reading the thermometer of {} / {}", floor.name, room.name)
                })
            })
            .collect::<Result<_>>()?,
        None => Vec::new(),
    };
    if args.verbose > 0 {
        eprintln!(
            "read {} outside rows from `{}` and {} rows for {} rooms in {:.2?} on {} threads",
            outside.rows,
            outside.source,
            rooms.iter().map(|room| room.rows).sum::<usize>(),
            rooms.len(),
            started.elapsed(),
            rayon::current_num_threads()
        );
    }

    let counting_started = Instant::now();
    let outside_days = daily_means(&outside.samples, tz);
    let room_days: Vec<Vec<DayMean>> = rooms
        .par_iter()
        .map(|room| daily_means(&room.samples, tz))
        .collect();
    let analysis = analyse(
        &outside_days,
        &room_days,
        house.as_ref(),
        config.bases,
        config.min_day_coverage,
        args.group,
    );
    if args.verbose > 0 {
        eprintln!(
            "counted {} of {} days into {} periods in {:.2?}",
            analysis.record.days,
            outside_days.len(),
            analysis.periods.len(),
            counting_started.elapsed()
        );
    }

    let coverage = Coverage::describe(
        &outside.samples,
        outside_days.len() as u32,
        outside_days
            .iter()
            .filter(|day| day.covered_seconds >= 20.0 * 3_600.0)
            .count() as u32,
        &present_months(&outside_days),
        Grouping::Month,
        tz,
        config.recorder.gap_threshold_seconds,
    );

    let html = page(
        &analysis,
        &ReportOptions {
            outside_entity: args.outside_entity().to_string(),
            house: house.clone(),
            metadata: metadata(&args, &outside, tz),
            coverage: coverage.clone(),
            tz,
        },
    );
    fs::write(&args.out, &html)
        .with_context(|| format!("failed to write {}", args.out.display()))?;

    report(&args, &analysis, house.as_ref(), html.len(), started);
    if let Some(coverage) = &coverage {
        println!("{}", coverage_line(coverage, tz));
    }

    // The last line is the one worth copying.
    println!("\n{}", open_command(&args.out));
    Ok(())
}

/// Load one thermometer on its own read-only connection, explaining an empty result.
fn read(args: &Args, config: &Config, entity: &str) -> Result<LoadedSamples> {
    let conn = source::open_database(&args.recorder.db)?;
    // Temperatures below zero are the whole point in winter: never clamp them.
    let options =
        args.recorder
            .load_options(entity, Quantity::Temperature, &config.recorder, false);
    let loaded = source::load(&conn, &options)?;
    if loaded.samples.is_empty() {
        return Err(source::explain_no_samples(
            &conn,
            &options,
            &loaded,
            config.recorder.tz,
        ));
    }
    Ok(loaded)
}

/// What the run found, in the order it matters.
fn report(args: &Args, analysis: &Analysis, house: Option<&House>, bytes: usize, started: Instant) {
    let unit = match args.group {
        Grouping::Month => "months",
        Grouping::Week => "weeks",
    };
    println!(
        "wrote {}: {} {unit} from {} days, {:.0} kB, {:.2?}",
        args.out.display(),
        analysis.periods.len(),
        analysis.record.days,
        bytes as f64 / 1024.0,
        started.elapsed()
    );
    println!(
        "degree days: {} heating and {} cooling over {} days (bases {} and {})",
        format_degree_days(analysis.record.hdd),
        format_degree_days(analysis.record.cdd),
        analysis.record.days,
        format_celsius(analysis.bases.heating),
        format_celsius(analysis.bases.cooling)
    );
    if let Some(coldest) = analysis
        .periods
        .iter()
        .filter(|period| period.hdd > 0.0)
        .max_by(|left, right| left.hdd.total_cmp(&right.hdd))
    {
        println!(
            "coldest: {} with {} heating degree days{}",
            coldest.period.label(),
            format_degree_days(coldest.hdd),
            coldest
                .outside
                .map(|outside| format!(" at {} on average", format_celsius(outside)))
                .unwrap_or_default()
        );
    }
    if let Some(house) = house {
        let floors: Vec<String> = house
            .floors
            .iter()
            .zip(&analysis.record.floors)
            .map(|(floor, mean)| match mean {
                Some(mean) => format!("{} {}", floor.name, format_celsius(*mean)),
                None => format!("{} not recorded", floor.name),
            })
            .collect();
        println!("indoors: {} on the same days", floors.join(", "));
    }
    if analysis.short_days > 0 {
        println!(
            "note: {} day{} recorded too thinly to count (under {:.0}% of the day)",
            analysis.short_days,
            if analysis.short_days == 1 {
                " was"
            } else {
                "s were"
            },
            args.min_day_coverage * 100.0
        );
    }
}

/// Calendar months, zero-based, the outside thermometer recorded anything in.
fn present_months(days: &[DayMean]) -> Vec<usize> {
    let mut months: Vec<usize> = days
        .iter()
        .filter_map(DayMean::date)
        .map(|date| date.month0() as usize)
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
    let outages = match coverage.longest_gap() {
        Some(longest) => format!(
            "{} outages over {} (longest {})",
            coverage.gaps.len(),
            format_duration(coverage.gap_threshold_seconds as f64),
            format_duration(longest.seconds() as f64)
        ),
        None => format!(
            "no outage over {}",
            format_duration(coverage.gap_threshold_seconds as f64)
        ),
    };
    format!(
        "covers {span} ({} days): {} days observed ({} of them full days), {outages}",
        coverage.span_days, coverage.observed_days, coverage.full_days
    )
}

/// The facts worth recording in the report header.
fn metadata(args: &Args, outside: &LoadedSamples, tz: Tz) -> Vec<(String, String)> {
    let mut metadata = vec![
        ("Source".to_string(), outside.source.to_string()),
        ("Grouping".to_string(), args.group.to_string()),
        ("Timezone".to_string(), tz.to_string()),
    ];
    if matches!(
        outside.source,
        SourceKind::Statistics | SourceKind::ShortTerm
    ) {
        metadata.push((
            "Statistic".to_string(),
            format!("{:?}", args.recorder.stat).to_lowercase(),
        ));
    }
    let first = outside
        .samples
        .first()
        .map(|sample: &Sample| sample.start_ts);
    let last = outside
        .samples
        .last()
        .map(|sample: &Sample| sample.end_ts - 1);
    if let (Some(first), Some(last)) = (first, last) {
        let date = |ts: i64| {
            ha_recorder::timeutil::day_to_date(ha_recorder::timeutil::local_day(ts, tz))
                .map(|date| date.to_string())
                .unwrap_or_else(|| "?".to_string())
        };
        metadata.push((
            "Range".to_string(),
            format!("{} to {}", date(first), date(last)),
        ));
    }
    if let Some(unit) = &outside.unit {
        let converted = if outside.conversion.is_identity() {
            unit.clone()
        } else {
            format!("{unit}, converted to °C")
        };
        metadata.push(("Unit".to_string(), converted));
    }
    if outside.source == SourceKind::States {
        metadata.push((
            "Max gap".to_string(),
            format!("{} s", args.recorder.max_gap),
        ));
    }
    metadata
}
