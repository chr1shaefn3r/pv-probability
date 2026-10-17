use std::fs;
use std::time::Instant;

use anyhow::{Context, Result};
use chrono::DateTime;
use chrono_tz::Tz;
use clap::Parser;

use ha_recorder::Quantity;
use ha_recorder::coverage::{Coverage, possible_days_per_facet};
use ha_recorder::source::{self, LoadedSamples, SourceKind};
use ha_report::{format_duration, format_watts, open_command};

use pv_probability::aggregate::{analyse, build_grid, suggest_max_watts};
use pv_probability::cli::Args;
use pv_probability::model::BucketSpec;
use pv_probability::render::{PageOptions, describe_window, page};

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
    let conn = args.recorder.open(args.verbose > 0)?;
    if args.recorder.print_listing(&conn, config.recorder.tz)? {
        return Ok(());
    }

    let load_options = args.recorder.load_options(
        args.entity(),
        Quantity::Power,
        &config.recorder,
        config.clamp_negative,
    );
    let loaded = source::load(&conn, &load_options)?;
    if args.verbose > 0 {
        eprintln!(
            "read {} rows from `{}` in {:.2?}",
            loaded.rows,
            loaded.source,
            started.elapsed()
        );
    }

    if loaded.samples.is_empty() {
        return Err(source::explain_no_samples(
            &conn,
            &load_options,
            &loaded,
            config.recorder.tz,
        ));
    }

    let max_watts = match args.max_watts {
        Some(max) => max,
        None => suggest_max_watts(&loaded.samples, args.step_watts, args.max_quantile),
    };
    let buckets = BucketSpec::new(args.step_watts, max_watts)?;

    let aggregation_started = Instant::now();
    let grid = build_grid(&loaded.samples, args.group, buckets, config.recorder.tz);
    let possible_days = match (loaded.first_ts, loaded.last_ts) {
        (Some(first), Some(last)) => possible_days_per_facet(
            first,
            last.saturating_sub(1).max(first),
            config.recorder.tz,
            args.group,
        ),
        _ => Vec::new(),
    };
    let mut analysis = analyse(&grid, args.metric, args.min_days, &possible_days);
    analysis.pool_seasons_by_month(&loaded.samples, config.recorder.tz);
    let coverage = Coverage::describe(
        &loaded.samples,
        analysis.observed_days,
        grid.days_covering_hours(20),
        &analysis
            .facets
            .iter()
            .map(|facet| facet.index)
            .collect::<Vec<_>>(),
        args.group,
        config.recorder.tz,
        config.recorder.gap_threshold_seconds,
    );
    if args.verbose > 0 {
        eprintln!(
            "aggregated {} samples into {} facets x 24 hours x {} buckets in {:.2?}",
            loaded.samples.len(),
            analysis.facets.len(),
            buckets.len(),
            aggregation_started.elapsed()
        );
    }

    let html = page(
        &analysis,
        &PageOptions {
            entity: args.entity().to_string(),
            metadata: metadata(&args, &loaded, config.recorder.tz),
            coverage: coverage.clone(),
            tz: config.recorder.tz,
            levels: args.levels,
            gamma: args.gamma,
            min_probability: args.min_probability,
            reliable_watts: args.reliable_watts,
            reliable_probability: args.reliable_probability,
        },
    );
    fs::write(&args.out, &html)
        .with_context(|| format!("failed to write {}", args.out.display()))?;

    println!(
        "wrote {}: {} facets from {} readings ({}), {:.0} kB, {:.2?}",
        args.out.display(),
        analysis.facets.len(),
        analysis.total_samples,
        format_duration(analysis.total_weight_seconds),
        html.len() as f64 / 1024.0,
        started.elapsed()
    );
    println!(
        "reliable window: {}",
        describe_window(&analysis.overall_window(args.reliable_watts, args.reliable_probability))
    );
    for season in &analysis.seasons {
        let window = if season.is_recorded() {
            describe_window(&analysis.window(
                &season.columns,
                args.reliable_watts,
                args.reliable_probability,
            ))
        } else {
            "nothing recorded".to_string()
        };
        println!(
            "  {} ({}): {window}",
            season.season.label().to_lowercase(),
            season.season.span()
        );
    }
    if let Some(coverage) = &coverage {
        println!("{}", coverage_line(coverage, config.recorder.tz));
        if args.verbose > 0 {
            for gap in &coverage.gaps {
                eprintln!(
                    "  outage: {} from {} to {}",
                    format_duration(gap.seconds() as f64),
                    local_stamp(gap.start_ts, config.recorder.tz),
                    local_stamp(gap.end_ts, config.recorder.tz)
                );
            }
        }
        if coverage.needs_caution() {
            println!(
                "note: {} - percentages are conditional on the days actually recorded, and \
                 hours backed by fewer than {} days are hatched.",
                if coverage.covers_full_year() {
                    "large parts of the span were never recorded"
                } else {
                    "less than a year of history"
                },
                args.min_days
            );
        }
    }

    // The last line is the one worth copying.
    println!("\n{}", open_command(&args.out));
    Ok(())
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

fn local_stamp(ts: i64, tz: Tz) -> String {
    DateTime::from_timestamp(ts, 0)
        .map(|utc| utc.with_timezone(&tz).format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| "?".to_string())
}

/// The facts worth recording in the report header.
fn metadata(args: &Args, loaded: &LoadedSamples, tz: Tz) -> Vec<(String, String)> {
    let mut metadata = vec![
        ("Source".to_string(), loaded.source.to_string()),
        ("Grouping".to_string(), args.group.to_string()),
        ("Metric".to_string(), args.metric.to_string()),
        ("Timezone".to_string(), tz.to_string()),
    ];

    if matches!(
        loaded.source,
        SourceKind::Statistics | SourceKind::ShortTerm
    ) {
        metadata.push((
            "Statistic".to_string(),
            format!("{:?}", args.recorder.stat).to_lowercase(),
        ));
    }
    if let (Some(first), Some(last)) = (loaded.first_ts, loaded.last_ts) {
        metadata.push((
            "Range".to_string(),
            format!("{} to {}", local_date(first, tz), local_date(last, tz)),
        ));
    }
    if let Some(unit) = &loaded.unit {
        let scaled = if loaded.conversion.scale == 1.0 {
            unit.clone()
        } else {
            format!("{unit} x{}", loaded.conversion.scale)
        };
        metadata.push(("Unit".to_string(), scaled));
    } else if loaded.conversion.scale != 1.0 {
        metadata.push(("Scale".to_string(), format!("x{}", loaded.conversion.scale)));
    }
    if loaded.source == SourceKind::States {
        metadata.push((
            "Max gap".to_string(),
            format!("{} s", args.recorder.max_gap),
        ));
    }
    if args.max_watts.is_some() {
        metadata.push((
            "Axis".to_string(),
            format!(
                "fixed to {}",
                format_watts(args.max_watts.unwrap_or_default())
            ),
        ));
    }
    metadata
}

fn local_date(ts: i64, tz: Tz) -> String {
    DateTime::from_timestamp(ts, 0)
        .map(|utc| utc.with_timezone(&tz).format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| "?".to_string())
}
