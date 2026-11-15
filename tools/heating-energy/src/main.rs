//! Heating and cooling degree days from a Home Assistant outdoor thermometer, beside the
//! indoor temperature of every room.

use std::fs;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use chrono::Datelike;
use chrono_tz::Tz;
use clap::Parser;
use rayon::prelude::*;

use ha_recorder::coverage::Coverage;
use ha_recorder::model::Grouping;
use ha_recorder::source::{self, LoadedSamples, SourceKind, states, statistics};
use ha_recorder::{Quantity, Sample};
use ha_report::{format_duration, open_command};

use heating_energy::cli::{Args, Config};
use heating_energy::daily::{DayMean, daily_means, room_days};
use heating_energy::degree::{Analysis, analyse};
use heating_energy::energy::{
    self, LeftOut, Reading, format_energy, format_monthly, format_slope, month_list, season_label,
};
use heating_energy::offset::{self, Check, Correction, Finding, format_offset, local_stamp};
use heating_energy::report::{
    EnergySection, ReportOptions, format_celsius, format_degree_days, page,
};
use heating_energy::rooms::{Floor, House, Room};

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

    // Every thermometer is read on a connection of its own, the outside one among the
    // rooms', and its daily means are worked out as soon as it is in, while the others
    // are still being read.
    let mut thermometers = vec![(args.outside_entity(), None)];
    if let Some(house) = &house {
        thermometers.extend(
            house
                .sensors()
                .map(|(floor, room, sensor)| (sensor, Some((floor, room)))),
        );
    }
    // A thermostat's offset is looked for and taken back out in its own task, before its
    // days are counted, and the monthly heating energy, if asked for, is read beside them.
    let (read_all, energy_read): (Vec<Result<Thermometer>>, _) = rayon::join(
        || read_thermometers(&args, &config, &thermometers),
        || {
            args.energy_entity
                .as_deref()
                .map(|entity| read_energy(&args, entity))
                .transpose()
        },
    );
    // The first thermometer that failed, in the file's order, is the one reported.
    let mut read_all = read_all.into_iter().collect::<Result<Vec<_>>>()?;
    let energy_read = energy_read?;
    let outside = read_all.remove(0);
    let (outside, outside_days) = (outside.loaded, outside.days);
    // Each room's thermometers, read in the file's order, become the room's days.
    let mut read_all = read_all.into_iter();
    let mut indoor_days = Vec::new();
    let mut findings: Vec<(String, Finding)> = Vec::new();
    let mut partial: Vec<(String, u32, usize)> = Vec::new();
    let mut room_notes: Vec<(String, String)> = Vec::new();
    let mut room_rows = 0;
    for (floor, room) in house.iter().flat_map(House::rooms) {
        let label = format!("{} / {}", floor.name, room.name);
        let mut thermometers = Vec::with_capacity(room.sensors.len());
        let mut corrections = Vec::new();
        for sensor in &room.sensors {
            let thermometer = read_all.next().expect("one thermometer read per sensor");
            room_rows += thermometer.loaded.rows;
            thermometers.push(thermometer.days);
            if let Some(finding) = thermometer.finding {
                let label = if room.sensors.len() == 1 {
                    label.clone()
                } else {
                    format!("{label}, {sensor}")
                };
                if let Finding::Corrected(correction) = &finding {
                    corrections.push((
                        label.clone(),
                        format!(
                            "is shown as its thermostat measured rather than as it reported: {}",
                            correction.sentence(tz)
                        ),
                    ));
                }
                findings.push((label, finding));
            }
        }
        let days = room_days(&thermometers, config.min_day_coverage);
        if room.sensors.len() > 1 {
            room_notes.push((
                label.clone(),
                format!(
                    "is the mean of its {} thermometers, {}, every one counting alike on the days \
                     it covered{}",
                    room.sensors.len(),
                    joined(&room.sensors),
                    match days.partial_days {
                        0 => String::new(),
                        1 => "; on one day only some of them did".to_string(),
                        days => format!("; on {days} days only some of them did"),
                    }
                ),
            ));
        }
        if days.partial_days > 0 {
            partial.push((label, days.partial_days, room.sensors.len()));
        }
        room_notes.extend(corrections);
        indoor_days.push(days.days);
    }
    if args.verbose > 0 {
        eprintln!(
            "read {} outside rows from `{}` and {} rows for {} rooms in {:.2?} on {} threads",
            outside.rows,
            outside.source,
            room_rows,
            house.as_ref().map_or(0, House::room_count),
            started.elapsed(),
            rayon::current_num_threads()
        );
    }

    // The History block only needs the outside thermometer: worked out beside the count.
    let counting_started = Instant::now();
    let (analysis, coverage) = rayon::join(
        || {
            analyse(
                &outside_days,
                &indoor_days,
                house.as_ref(),
                config.bases,
                config.min_day_coverage,
                args.group,
            )
        },
        || {
            Coverage::describe(
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
            )
        },
    );
    // Every month's heating energy, from the total at its start and its end, beside its
    // degree days, and the line through them.
    let energy = match (&args.energy_entity, energy_read) {
        (Some(entity), Some(read)) => {
            if args.verbose > 0 {
                eprintln!(
                    "read {} reading{} of {entity} from its {}",
                    read.readings.len(),
                    if read.readings.len() == 1 { "" } else { "s" },
                    if read.from_raw_states {
                        "raw states"
                    } else {
                        "long-term statistics"
                    }
                );
            }
            Some(EnergySection {
                entity: entity.clone(),
                energy: energy::analyse(
                    &read.readings,
                    &outside_days,
                    config.bases,
                    config.min_day_coverage,
                    tz,
                ),
                from_raw_states: read.from_raw_states,
            })
        }
        _ => None,
    };
    if args.verbose > 0 {
        eprintln!(
            "counted {} of {} days into {} periods in {:.2?}",
            analysis.record.days,
            outside_days.len(),
            analysis.periods.len(),
            counting_started.elapsed()
        );
    }

    let html = page(
        &analysis,
        &ReportOptions {
            outside_entity: args.outside_entity().to_string(),
            house: house.clone(),
            metadata: metadata(&args, &outside, tz),
            coverage: coverage.clone(),
            tz,
            room_notes,
            energy: energy.clone(),
        },
    );
    fs::write(&args.out, &html)
        .with_context(|| format!("failed to write {}", args.out.display()))?;

    report(&args, &analysis, house.as_ref(), html.len(), started);
    if let Some(section) = &energy {
        for line in energy_lines(section) {
            println!("{line}");
        }
    }
    for line in finding_lines(&findings, tz) {
        println!("{line}");
    }
    for (label, days, thermometers) in &partial {
        println!(
            "note: {label}: on {days} day{} only some of its {thermometers} thermometers \
             covered enough of the day; {} the mean of those that did",
            if *days == 1 { "" } else { "s" },
            if *days == 1 { "it is" } else { "they are" }
        );
    }
    if let Some(coverage) = &coverage {
        println!("{}", coverage_line(coverage, tz));
    }

    // The last line is the one worth copying.
    println!("\n{}", open_command(&args.out));
    Ok(())
}

/// Read every thermometer on a connection of its own, in parallel, with its daily means,
/// in the order given.
fn read_thermometers(
    args: &Args,
    config: &Config,
    thermometers: &[(&str, Option<(&Floor, &Room)>)],
) -> Vec<Result<Thermometer>> {
    let tz = config.recorder.tz;
    thermometers
        .par_iter()
        .map(|(entity, room)| {
            let (loaded, finding) = match room {
                Some((floor, room)) => read_room(args, config, entity).with_context(|| {
                    if room.sensors.len() == 1 {
                        format!("reading the thermometer of {} / {}", floor.name, room.name)
                    } else {
                        format!(
                            "reading the thermometer {entity} of {} / {}",
                            floor.name, room.name
                        )
                    }
                })?,
                None => (read(args, config, entity)?, None),
            };
            let days = daily_means(&loaded.samples, tz);
            Ok(Thermometer {
                loaded,
                days,
                finding,
            })
        })
        .collect()
}

/// The readings of the heating energy's running total, and where they were read from.
struct EnergyRead {
    readings: Vec<Reading>,
    from_raw_states: bool,
}

/// Read the heating energy's running total on its own connection: from its long-term
/// statistics where it has them, which Home Assistant keeps for good, otherwise from its
/// raw states.
fn read_energy(args: &Args, entity: &str) -> Result<EnergyRead> {
    let conn = source::open_database(&args.recorder.db)?;
    let (history, scale, from_raw_states) = match statistics::hourly_values(&conn, entity)? {
        Some(hourly) if !hourly.values.is_empty() => (
            hourly.values,
            energy_scale(entity, hourly.unit.as_deref())?,
            false,
        ),
        _ => (
            states::history(&conn, entity)
                .with_context(|| format!("reading the heating energy, {entity}"))?,
            1.0,
            true,
        ),
    };
    Ok(EnergyRead {
        readings: energy::readings(&history, scale),
        from_raw_states,
    })
}

/// What turns the energy entity's unit into kWh. Raw states carry no unit, and are taken
/// to be in kWh.
fn energy_scale(entity: &str, unit: Option<&str>) -> Result<f64> {
    match unit.map(str::trim) {
        None | Some("") | Some("kWh") => Ok(1.0),
        Some("Wh") => Ok(0.001),
        Some("MWh") => Ok(1_000.0),
        Some(other) => {
            bail!("{entity} is recorded in {other}; the heating energy has to be in kWh, Wh or MWh")
        }
    }
}

/// One thermometer read, its days counted, and what became of its offset.
struct Thermometer {
    loaded: LoadedSamples,
    days: Vec<DayMean>,
    finding: Option<Finding>,
}

/// Load one thermometer on its own read-only connection, explaining an empty result.
fn read(args: &Args, config: &Config, entity: &str) -> Result<LoadedSamples> {
    let conn = source::open_database(&args.recorder.db)?;
    load_thermometer(&conn, args, config, entity)
}

/// Load a room's thermometer, and take its thermostat's offset back out if it has one.
fn read_room(
    args: &Args,
    config: &Config,
    sensor: &str,
) -> Result<(LoadedSamples, Option<Finding>)> {
    let conn = source::open_database(&args.recorder.db)?;
    let mut loaded = load_thermometer(&conn, args, config, sensor)?;
    let finding = offset::resolve(&conn, sensor, &mut loaded)?;
    Ok((loaded, finding))
}

fn load_thermometer(
    conn: &rusqlite::Connection,
    args: &Args,
    config: &Config,
    entity: &str,
) -> Result<LoadedSamples> {
    // Temperatures below zero are the whole point in winter: never clamp them.
    let options =
        args.recorder
            .load_options(entity, Quantity::Temperature, &config.recorder, false);
    let loaded = source::load(conn, &options)?;
    if loaded.samples.is_empty() {
        return Err(source::explain_no_samples(
            conn,
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

/// The efficiency figure and how it moved season by season, as lines for the terminal.
fn energy_lines(section: &EnergySection) -> Vec<String> {
    let energy = &section.energy;
    let entity = &section.entity;
    let mut lines = Vec::new();
    match &energy.overall {
        Some(fit) => lines.push(format!(
            "efficiency: {} kWh per heating degree day (± {}){} - a line through {} months of \
             {entity}",
            format_slope(fit.slope),
            format_slope(fit.slope_error),
            match energy.water_per_month {
                Some(per_month) => format!(
                    ", once water heating of {} on average is taken off",
                    format_monthly(per_month)
                ),
                None => format!(", and {} without heating", format_monthly(fit.base)),
            },
            fit.months
        )),
        None => {
            let mut line = format!(
                "efficiency: no line yet - {} reading{} of {entity} cover{} {} whole month{} \
                 where the outside was recorded, {} fit to use, and a line needs {} spread over \
                 milder and colder ones",
                energy.readings,
                if energy.readings == 1 { "" } else { "s" },
                if energy.readings == 1 { "s" } else { "" },
                energy.months.len(),
                if energy.months.len() == 1 { "" } else { "s" },
                energy.fitted().count(),
                energy::MIN_FIT_MONTHS
            );
            if section.from_raw_states {
                line.push_str(
                    ". Home Assistant keeps this entity's readings only in its raw states, ten \
                     days by default: see the README for keeping them",
                );
            }
            lines.push(line);
        }
    }
    if energy.water_per_month.is_none() && !energy.months.is_empty() {
        lines.push(format!(
            "note: water heating is left in {entity}: no season has three months from May to \
             September on record to tell it by"
        ));
    }
    let mut previous: Option<(String, f64)> = None;
    for season in &energy.seasons {
        let water = match &season.water {
            Some(water) if water.from_season == season.start_year => format!(
                "; water heating {}, from {}",
                format_monthly(water.per_month),
                month_list(&water.months)
            ),
            Some(water) => format!(
                "; water heating {}, {}'s",
                format_monthly(water.per_month),
                season_label(water.from_season)
            ),
            None => String::new(),
        };
        match season.fit {
            Some(fit) => {
                let change = match &previous {
                    Some((label, before)) if *before > 0.0 => {
                        let change = fit.slope / before - 1.0;
                        format!(
                            ", {:.0}% {} than {label}",
                            change.abs() * 100.0,
                            if change < 0.0 { "less" } else { "more" }
                        )
                    }
                    _ => String::new(),
                };
                lines.push(format!(
                    "  {}: {} kWh per degree day (± {}) over {} months{change}{water}",
                    season.label(),
                    format_slope(fit.slope),
                    format_slope(fit.slope_error),
                    fit.months
                ));
                previous = Some((season.label(), fit.slope));
            }
            None if energy.overall.is_some() => lines.push(format!(
                "  {}: too few months for a line of its own ({} fitted){water}",
                season.label(),
                season.fitted
            )),
            None => {}
        }
    }
    for month in &energy.months {
        let why = match month.left_out() {
            None => continue,
            Some(LeftOut::FewDays) => format!(
                "the outside thermometer counted {} of its {} days",
                month.counted_days,
                month.month.days()
            ),
            Some(LeftOut::Unchanged) => {
                "the total did not change over it - not read, or nothing used".to_string()
            }
            Some(LeftOut::AfterUnchanged) => {
                "the total did not change over the month before, so it may hold both".to_string()
            }
            Some(LeftOut::WentDown) => format!(
                "the total went down over it, by {} - a meter replaced, or a reading put right \
                 late",
                format_energy(-month.kwh)
            ),
        };
        lines.push(format!(
            "note: {} of {entity} left out of the fit: {why}",
            month.month.label()
        ));
    }
    if energy.beyond_record > 0 {
        lines.push(format!(
            "note: {} month{} of {entity} lie{} outside the outside thermometer's record",
            energy.beyond_record,
            if energy.beyond_record == 1 { "" } else { "s" },
            if energy.beyond_record == 1 { "s" } else { "" }
        ));
    }
    lines
}

/// What became of every room's thermostat offset, as lines for the terminal: what was
/// taken out and by how much, and what the readings say about which way.
fn finding_lines(findings: &[(String, Finding)], tz: Tz) -> Vec<String> {
    let mut lines = Vec::new();
    for (label, finding) in findings {
        match finding {
            Finding::Corrected(correction) => {
                lines.push(format!(
                    "offset: {label}: readings {} to take out its thermostat's offset {} ({})",
                    correction.effect(),
                    correction.entity,
                    correction.values()
                ));
                if let Some(since) = correction.since(tz) {
                    lines.push(format!("  the offset is {since}"));
                }
                lines.push(check_line(label, correction, tz));
            }
            Finding::Ambiguous(entities) => lines.push(format!(
                "note: {label}: {} all look like its thermostat's offset, so none is taken out",
                entities.join(", ")
            )),
            Finding::Unrecorded(entity) => lines.push(format!(
                "note: {label}: its thermostat's offset {entity} has no value on record - Home \
                 Assistant keeps settings only in its raw states, ten days by default - so its \
                 readings are used as reported"
            )),
        }
    }
    lines
}

/// What the raw readings say about which way a room's offset is applied, and so how it
/// was taken out.
fn check_line(label: &str, correction: &Correction, tz: Tz) -> String {
    let unit = &correction.unit;
    let change = |(at, from, to): (i64, f64, f64)| {
        format!(
            "went from {} to {} at {}",
            format_offset(from, unit),
            format_offset(to, unit),
            local_stamp(at, tz)
        )
    };
    let times = |count: usize| {
        if count == 1 {
            "once".to_string()
        } else {
            format!("{count} times")
        }
    };
    match &correction.check {
        Check::Added { changes } => format!(
            "  checked: the reading moved with the offset when it changed ({}), so the \
             thermostat adds it to what it measures",
            times(*changes)
        ),
        Check::Subtracted {
            changes,
            first: (at, moved),
        } => format!(
            "  checked: when the offset {}, the reading moved by {} - the other way ({}), so \
             this thermostat subtracts it from what it measures, and it is taken out that way",
            change(*at),
            format_offset(*moved, unit),
            times(*changes)
        ),
        Check::Untested => "  not checked: the offset has not changed while raw readings were \
             kept, so it is taken to be added to what the thermostat measures, as Zigbee and \
             Matter thermostats do"
            .to_string(),
        Check::Unmoved { first: (at, _) } => format!(
            "warning: {label}: the reading did not move when its offset {}, as though it were \
             recorded without it; it is taken out all the same",
            change(*at)
        ),
        Check::Mixed { added, subtracted } => format!(
            "warning: {label}: the reading moved with the offset at {added} of its changes and \
             against it at {subtracted}; it is taken to be added, as Zigbee and Matter \
             thermostats do"
        ),
    }
}

/// `a`, `a and b`, `a, b and c`.
fn joined(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [only] => only.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
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
