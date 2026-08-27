use std::fs;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use chrono::DateTime;
use chrono_tz::Tz;
use clap::Parser;

use pv_probability::aggregate::{analyse, build_grid, suggest_max_watts};
use pv_probability::cli::Args;
use pv_probability::model::BucketSpec;
use pv_probability::render::{PageOptions, format_duration, format_watts, page};
use pv_probability::source::{self, LoadOptions, LoadedSamples, SourceKind};

fn main() -> Result<()> {
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
    let loaded = source::load(
        &conn,
        &LoadOptions {
            entity: &args.entity,
            source: args.source,
            value: args.stat,
            from: config.from,
            to: config.to,
            max_gap: args.max_gap,
            scale: args.scale,
            clamp_negative: config.clamp_negative,
        },
    )?;
    if args.verbose > 0 {
        eprintln!(
            "read {} rows from `{}` in {:.2?}",
            loaded.rows,
            loaded.source,
            started.elapsed()
        );
    }

    if loaded.samples.is_empty() {
        bail!(
            "`{}` has no usable readings in `{}` for the selected range. \
             Try a different --source, widen --from/--to, or check the entity name.",
            args.entity,
            loaded.source
        );
    }

    let max_watts = match args.max_watts {
        Some(max) => max,
        None => suggest_max_watts(&loaded.samples, args.step_watts, args.max_quantile),
    };
    let buckets = BucketSpec::new(args.step_watts, max_watts)?;

    let aggregation_started = Instant::now();
    let grid = build_grid(&loaded.samples, args.group, buckets, config.tz);
    let analysis = analyse(&grid, args.metric, args.min_samples);
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
            entity: args.entity.clone(),
            metadata: metadata(&args, &loaded, config.tz),
            levels: args.levels,
            gamma: args.gamma,
            min_probability: args.min_probability,
        },
    );
    fs::write(&args.out, &html)
        .with_context(|| format!("failed to write {}", args.out.display()))?;

    println!(
        "{} - {} facets from {} readings ({}), {:.0} kB, {:.2?}",
        args.out.display(),
        analysis.facets.len(),
        analysis.total_samples,
        format_duration(analysis.total_weight_seconds),
        html.len() as f64 / 1024.0,
        started.elapsed()
    );
    Ok(())
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
            format!("{:?}", args.stat).to_lowercase(),
        ));
    }
    if let (Some(first), Some(last)) = (loaded.first_ts, loaded.last_ts) {
        metadata.push((
            "Range".to_string(),
            format!("{} to {}", local_date(first, tz), local_date(last, tz)),
        ));
    }
    if let Some(unit) = &loaded.unit {
        let scaled = if loaded.scale == 1.0 {
            unit.clone()
        } else {
            format!("{unit} x{}", loaded.scale)
        };
        metadata.push(("Unit".to_string(), scaled));
    } else if loaded.scale != 1.0 {
        metadata.push(("Scale".to_string(), format!("x{}", loaded.scale)));
    }
    if loaded.source == SourceKind::States {
        metadata.push(("Max gap".to_string(), format!("{} s", args.max_gap)));
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
