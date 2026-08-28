//! The whole pipeline, from a synthetic Home Assistant recorder database to the HTML
//! report, without ever touching a real Home Assistant instance.

use std::fs;

use chrono_tz::Europe::Berlin;
use chrono_tz::Tz::UTC;
use rusqlite::Connection;

use pv_probability::aggregate::{Analysis, ColumnStatus, analyse, build_grid, suggest_max_watts};
use pv_probability::coverage::{Coverage, possible_days_per_facet};
use pv_probability::model::{BucketSpec, Grouping, Metric};
use pv_probability::render::{PageOptions, page};
use pv_probability::source::schema::ValueColumn;
use pv_probability::source::testdb::{self, ENTITY};
use pv_probability::source::{LoadOptions, LoadedSamples, SourceKind, load};

/// Two years of hourly statistics for a synthetic 8 kWp array.
fn database() -> Connection {
    testdb::synthetic_year_database("2023-01-01 00:00:00", 730, 0xC0FFEE)
}

fn options<'a>(entity: &'a str) -> LoadOptions<'a> {
    LoadOptions {
        entity,
        source: SourceKind::Auto,
        value: ValueColumn::Mean,
        from: None,
        to: None,
        max_gap: 900,
        scale: None,
        clamp_negative: true,
        tz: UTC,
    }
}

/// Days of the calendar each facet could have covered, given what the data spans.
fn possible_days(loaded: &LoadedSamples, grouping: Grouping) -> Vec<u32> {
    match (loaded.first_ts, loaded.last_ts) {
        (Some(first), Some(last)) => {
            possible_days_per_facet(first, last.saturating_sub(1).max(first), Berlin, grouping)
        }
        _ => Vec::new(),
    }
}

fn run(conn: &Connection, grouping: Grouping, metric: Metric) -> (LoadedSamples, Analysis) {
    let loaded = load(conn, &options(ENTITY)).expect("samples load");
    let max_watts = suggest_max_watts(&loaded.samples, 50.0, 0.999);
    let buckets = BucketSpec::new(50.0, max_watts).expect("bucket axis");
    let grid = build_grid(&loaded.samples, grouping, buckets, Berlin);
    let analysis = analyse(&grid, metric, 3, &possible_days(&loaded, grouping));
    (loaded, analysis)
}

#[test]
fn two_years_of_statistics_produce_twelve_monthly_facets() {
    let conn = database();
    let (loaded, analysis) = run(&conn, Grouping::Month, Metric::Exceedance);

    assert_eq!(loaded.source, SourceKind::Statistics);
    assert_eq!(loaded.rows, 730 * 24);
    assert_eq!(loaded.samples.len(), 730 * 24);
    assert_eq!(analysis.facets.len(), 12, "every month should carry data");

    // Every observed second is accounted for exactly once.
    let expected: f64 = loaded.observed_seconds() as f64;
    assert!(
        (analysis.total_weight_seconds - expected).abs() < 1.0,
        "weights {} did not match observed time {expected}",
        analysis.total_weight_seconds
    );
}

#[test]
fn midday_summer_power_is_far_more_likely_than_midday_winter_power() {
    let conn = database();
    let (_, analysis) = run(&conn, Grouping::Month, Metric::Exceedance);

    let june = analysis.facet(5).expect("June");
    let december = analysis.facet(11).expect("December");
    let bucket = analysis.buckets.index(2_000.0);

    let june_noon = june.value(13, bucket);
    let december_noon = december.value(13, bucket);
    assert!(
        june_noon > 0.3,
        "June has only a {june_noon:.3} chance of 2 kW at 13:00"
    );
    assert!(
        december_noon < june_noon / 2.0,
        "December ({december_noon:.3}) should be well below June ({june_noon:.3})"
    );
}

#[test]
fn nothing_is_available_at_night() {
    let conn = database();
    let (_, analysis) = run(&conn, Grouping::Month, Metric::Exceedance);
    let june = analysis.facet(5).expect("June");
    let bucket = analysis.buckets.index(50.0);

    for hour in [0, 1, 2, 23] {
        assert_eq!(
            june.value(hour, bucket),
            0.0,
            "June should never produce 50 W at {hour:02}:00"
        );
    }
    // ... and the zero bucket is still a certainty, because the hour was observed.
    assert_eq!(june.value(0, 0), 1.0);
}

#[test]
fn exceedance_curves_fall_off_monotonically_everywhere() {
    let conn = database();
    let (_, analysis) = run(&conn, Grouping::Month, Metric::Exceedance);

    for facet in &analysis.facets {
        for column in &facet.columns {
            if !column.status.is_sufficient() {
                continue;
            }
            assert_eq!(
                column.values[0], 1.0,
                "{} {:02}:00",
                facet.label, column.hour
            );
            for (bucket, pair) in column.values.windows(2).enumerate() {
                assert!(
                    pair[0] >= pair[1],
                    "{} {:02}:00 rose from bucket {bucket} to {}",
                    facet.label,
                    column.hour,
                    bucket + 1
                );
            }
        }
    }
}

#[test]
fn the_week_grouping_covers_the_whole_year() {
    let conn = database();
    let (_, analysis) = run(&conn, Grouping::Week, Metric::Exceedance);
    assert!(
        analysis.facets.len() >= 52,
        "expected a facet per ISO week, got {}",
        analysis.facets.len()
    );
    assert!(
        analysis
            .facets
            .iter()
            .all(|facet| facet.label.starts_with("Week"))
    );
}

#[test]
fn the_report_is_a_self_contained_html_file() {
    let conn = database();
    let (loaded, analysis) = run(&conn, Grouping::Month, Metric::Exceedance);
    let html = page(
        &analysis,
        &PageOptions {
            entity: ENTITY.to_string(),
            metadata: vec![
                ("Source".to_string(), loaded.source.to_string()),
                ("Timezone".to_string(), Berlin.to_string()),
            ],
            ..PageOptions::default()
        },
    );

    assert!(html.starts_with("<!doctype html>"));
    assert_eq!(html.matches("class=\"facet-plot\"").count(), 12);
    assert_eq!(html.matches("<details class=\"table-view\"").count(), 12);
    assert!(html.contains("June"));
    assert!(html.contains("December"));
    assert!(html.contains(ENTITY));
    assert!(!html.contains("http://") && !html.contains("https://"));
    assert!(!html.contains("<script"));
    assert!(!html.contains("NaN"));

    // Run length encoding has to keep the page to a sane size: without it this is well
    // over a hundred thousand rectangles.
    let rects = html.matches("<rect").count();
    assert!(
        rects < 12 * 24 * 40,
        "run length encoding is not doing its job: {rects} rects"
    );
    assert!(
        html.len() < 4 * 1024 * 1024,
        "report grew to {} bytes",
        html.len()
    );

    // Keep a copy for eyeballing after a test run.
    let out = std::env::temp_dir().join("pv-probability-example.html");
    fs::write(&out, &html).expect("write example report");
}

#[test]
fn a_date_range_narrows_the_report_to_the_months_it_covers() {
    let conn = database();
    let mut options = options(ENTITY);
    options.from = Some(testdb::ts("2023-06-01 00:00:00"));
    options.to = Some(testdb::ts("2023-07-01 00:00:00"));

    let loaded = load(&conn, &options).expect("samples load");
    let buckets = BucketSpec::new(50.0, 8_000.0).unwrap();
    let grid = build_grid(&loaded.samples, Grouping::Month, buckets, Berlin);
    let analysis = analyse(&grid, Metric::Exceedance, 3, &[]);

    // June in Berlin starts two hours before June in UTC, so May keeps a sliver.
    let months: Vec<usize> = analysis.facets.iter().map(|facet| facet.index).collect();
    assert!(months.contains(&5), "June must be present: {months:?}");
    assert!(!months.contains(&7), "August must be absent: {months:?}");
    assert!(months.len() <= 3, "expected a narrow range, got {months:?}");
}

#[test]
fn density_columns_are_a_distribution() {
    let conn = database();
    let (_, analysis) = run(&conn, Grouping::Month, Metric::Density);

    for facet in &analysis.facets {
        for column in &facet.columns {
            if !column.status.is_sufficient() {
                continue;
            }
            let total: f64 = column.values.iter().sum();
            assert!(
                (total - 1.0).abs() < 1e-9,
                "{} {:02}:00 summed to {total}",
                facet.label,
                column.hour
            );
        }
    }
}

#[test]
fn raw_states_and_hourly_statistics_agree_on_a_flat_day() {
    // A sensor that reports 4 kW for a whole day must give the same picture whichever
    // table it is read from.
    let conn = Connection::open_in_memory().unwrap();
    testdb::create_schema(&conn, testdb::Flavour::Modern);
    let statistic_id = testdb::insert_statistic_meta(&conn, ENTITY, Some("W"));
    let states_id = testdb::insert_states_meta(&conn, ENTITY);
    let start = testdb::ts("2024-06-21 00:00:00");

    for hour in 0..24 {
        testdb::insert_statistics_row(
            &conn,
            testdb::Flavour::Modern,
            "statistics",
            statistic_id,
            start + hour * 3_600,
            Some(4_000.0),
        );
        for slot in 0..6 {
            testdb::insert_state(
                &conn,
                testdb::Flavour::Modern,
                ENTITY,
                states_id,
                start + hour * 3_600 + slot * 600,
                "4000",
            );
        }
    }

    let buckets = BucketSpec::new(50.0, 5_000.0).unwrap();
    let analyse_source = |source| {
        let mut options = options(ENTITY);
        options.source = source;
        options.to = Some(start + 24 * 3_600);
        let loaded = load(&conn, &options).expect("samples load");
        let grid = build_grid(&loaded.samples, Grouping::Month, buckets, Berlin);
        analyse(&grid, Metric::Exceedance, 1, &[])
    };

    let from_statistics = analyse_source(SourceKind::Statistics);
    let from_states = analyse_source(SourceKind::States);
    let bucket = buckets.index(4_000.0);

    for hour in 0..24 {
        let statistics = from_statistics.facet(5).unwrap().value(hour, bucket);
        let states = from_states.facet(5).unwrap().value(hour, bucket);
        assert_eq!(statistics, states, "hour {hour} disagreed");
        assert_eq!(statistics, 1.0, "4 kW was constant all day");
    }
}

// --- partial history and outages -------------------------------------------------
//
// The real database this tool is aimed at is not a tidy block of years: it is a few
// months, with the recorder down in the middle of them.

/// Five months of history (March to July), a nine day outage in June, and a scattering
/// of lost days throughout.
fn sparse_database() -> (Connection, testdb::Outages, i64) {
    let days = 150; // 1 March to late July.
    let outages = testdb::Outages {
        missing_days: (0..days).filter(|day| day % 7 == 3).collect(),
        // Day 100 of the run is 9 June.
        missing_ranges: vec![(100, 109)],
        missing_hours: Vec::new(),
    };
    let conn = testdb::synthetic_history_database("2025-03-01 00:00:00", days, 0xBEEF, &outages);
    (conn, outages, days)
}

/// These scenarios are about which days exist, so they run in UTC: the generator lays
/// its synthetic days out in UTC, and a local offset would smear each of them across two
/// calendar dates and make the day counts ambiguous. Local-time behaviour has its own
/// tests in `timeutil`.
fn sparse_analysis(conn: &Connection) -> (LoadedSamples, Analysis, Coverage) {
    let loaded = load(conn, &options(ENTITY)).expect("samples load");
    let buckets = BucketSpec::new(50.0, 8_000.0).expect("bucket axis");
    let grid = build_grid(&loaded.samples, Grouping::Month, buckets, UTC);
    let possible = match (loaded.first_ts, loaded.last_ts) {
        (Some(first), Some(last)) => possible_days_per_facet(
            first,
            last.saturating_sub(1).max(first),
            UTC,
            Grouping::Month,
        ),
        _ => Vec::new(),
    };
    let analysis = analyse(&grid, Metric::Exceedance, 3, &possible);
    let coverage = Coverage::describe(
        &loaded.samples,
        analysis.observed_days,
        grid.days_covering_hours(20),
        &analysis
            .facets
            .iter()
            .map(|facet| facet.index)
            .collect::<Vec<_>>(),
        Grouping::Month,
        UTC,
        24 * 3_600,
    )
    .expect("coverage");
    (loaded, analysis, coverage)
}

#[test]
fn a_partial_year_only_reports_the_months_it_has() {
    let (conn, _, _) = sparse_database();
    let (_, analysis, coverage) = sparse_analysis(&conn);

    let months: Vec<usize> = analysis.facets.iter().map(|facet| facet.index).collect();
    assert_eq!(
        months,
        vec![2, 3, 4, 5, 6],
        "March through July, nothing else"
    );
    assert!(
        coverage.missing_facets.is_empty(),
        "months outside the span were never possible: {:?}",
        coverage.missing_facets
    );
}

#[test]
fn the_outage_is_found_with_the_right_length() {
    let (conn, _, _) = sparse_database();
    let (_, _, coverage) = sparse_analysis(&conn);

    let longest = coverage.longest_gap().expect("the nine day outage");
    let days = longest.seconds() as f64 / 86_400.0;
    assert!(
        (8.5..=9.5).contains(&days),
        "expected the nine day outage, found {days:.2} days"
    );

    // The weekly single-day holes are exactly a day long, so they are reported too - and
    // nothing else is.
    for gap in &coverage.gaps {
        let length = gap.seconds() as f64 / 86_400.0;
        assert!(
            (0.9..=1.1).contains(&length) || (8.5..=9.5).contains(&length),
            "unexpected {length:.2} day gap"
        );
    }
    assert!(
        coverage.gaps.len() > 5,
        "the weekly holes should be visible too"
    );
}

#[test]
fn coverage_counts_days_rather_than_readings() {
    let (conn, outages, days) = sparse_database();
    let (_, analysis, coverage) = sparse_analysis(&conn);

    assert_eq!(
        coverage.observed_days,
        outages.covered_days(days) as u32,
        "every day the recorder was up, and no others"
    );
    assert!(coverage.observed_days < days as u32, "days were lost");
    assert_eq!(coverage.span_days, days as u32);
    assert!(
        !coverage.covers_full_year(),
        "five months is not a full year of seasons"
    );
    assert!(coverage.needs_caution());
    assert!(coverage.day_fraction() < 1.0);

    // June lost nine days in one block plus its share of the weekly holes.
    let june = analysis.facet(5).expect("June");
    assert_eq!(june.possible_days, 30);
    assert!(
        (17..=21).contains(&june.days),
        "June should have around 19 days, found {}",
        june.days
    );
    assert!(june.days < june.possible_days);
}

#[test]
fn a_month_with_only_a_couple_of_days_is_hatched_rather_than_coloured() {
    // A history that stops two days into June.
    let conn = testdb::synthetic_history_database(
        "2025-05-01 00:00:00",
        33,
        0xFEED,
        &testdb::Outages::none(),
    );
    let (_, analysis, _) = sparse_analysis(&conn);

    let june = analysis.facet(5).expect("June");
    assert!(june.days <= 3, "only a sliver of June exists");
    let midday = &june.columns[13];
    assert_ne!(
        midday.status,
        ColumnStatus::Sufficient,
        "{} days must not be presented as a settled likelihood",
        midday.days
    );

    // May, which is complete, is drawn normally.
    let may = analysis.facet(4).expect("May");
    assert_eq!(may.columns[13].status, ColumnStatus::Sufficient);
}

#[test]
fn an_hour_the_recorder_never_covered_is_marked_as_never_recorded() {
    // An inverter that only reports between 06:00 and 20:00 local.
    let outages = testdb::Outages {
        missing_days: Vec::new(),
        missing_ranges: Vec::new(),
        missing_hours: (0..24).filter(|hour| !(5..19).contains(hour)).collect(),
    };
    let conn = testdb::synthetic_history_database("2025-05-01 00:00:00", 60, 0xCAFE, &outages);
    let (_, analysis, _) = sparse_analysis(&conn);

    let may = analysis.facet(4).expect("May");
    assert_eq!(
        may.columns[3].status,
        ColumnStatus::Empty,
        "03:00 was never recorded"
    );
    assert_eq!(may.columns[3].days, 0);
    assert_eq!(may.columns[13].status, ColumnStatus::Sufficient);
}

#[test]
fn the_report_tells_the_reader_what_is_missing() {
    let (conn, _, _) = sparse_database();
    let (loaded, analysis, coverage) = sparse_analysis(&conn);
    let html = page(
        &analysis,
        &PageOptions {
            entity: ENTITY.to_string(),
            metadata: vec![("Source".to_string(), loaded.source.to_string())],
            coverage: Some(coverage.clone()),
            tz: Berlin,
            ..PageOptions::default()
        },
    );

    assert!(html.contains("History"), "the coverage block is present");
    assert!(html.contains(&format!("{} of them", coverage.observed_days)));
    assert!(html.contains("outage"), "the outage is named");
    assert!(
        html.contains("Less than a year of history"),
        "the partial-history caution is shown"
    );
    assert!(html.contains("conditional on the time that was actually recorded"));
    // One coverage strip cell per hour of every facet.
    assert_eq!(
        html.matches("<rect class=\"cov").count(),
        analysis.facets.len() * 24
    );
    assert!(
        html.contains("days recorded per hour"),
        "the strip is explained"
    );
}

#[test]
fn hourly_statistics_and_chatty_raw_states_agree_on_how_many_days_they_saw() {
    // The same three days, recorded twice: once as 24 hourly statistics rows a day, once
    // as a state every two minutes. A reading count would call the second one 30 times
    // better evidenced; a day count knows they are the same three days.
    let conn = Connection::open_in_memory().unwrap();
    testdb::create_schema(&conn, testdb::Flavour::Modern);
    let statistic_id = testdb::insert_statistic_meta(&conn, ENTITY, Some("W"));
    let states_id = testdb::insert_states_meta(&conn, ENTITY);
    let start = testdb::ts("2025-06-02 00:00:00");

    for day in 0..3 {
        for hour in 0..24 {
            let hour_start = start + day * 86_400 + hour * 3_600;
            testdb::insert_statistics_row(
                &conn,
                testdb::Flavour::Modern,
                "statistics",
                statistic_id,
                hour_start,
                Some(2_000.0),
            );
            for slot in 0..30 {
                testdb::insert_state(
                    &conn,
                    testdb::Flavour::Modern,
                    ENTITY,
                    states_id,
                    hour_start + slot * 120,
                    "2000",
                );
            }
        }
    }

    let buckets = BucketSpec::new(50.0, 4_000.0).unwrap();
    let day_counts = |source| {
        let mut options = options(ENTITY);
        options.source = source;
        options.to = Some(start + 3 * 86_400);
        let loaded = load(&conn, &options).expect("samples load");
        let grid = build_grid(&loaded.samples, Grouping::Month, buckets, UTC);
        let analysis = analyse(&grid, Metric::Exceedance, 3, &[]);
        let facet = analysis.facet(5).expect("June").clone();
        (loaded.samples.len(), facet)
    };

    let (statistics_samples, from_statistics) = day_counts(SourceKind::Statistics);
    let (states_samples, from_states) = day_counts(SourceKind::States);

    assert!(
        states_samples > statistics_samples * 20,
        "the raw states really are far chattier ({states_samples} vs {statistics_samples})"
    );
    assert_eq!(from_statistics.days, 3);
    assert_eq!(from_states.days, 3, "the same three days, however chatty");
    for hour in 0..24 {
        assert_eq!(
            from_statistics.columns[hour].days, from_states.columns[hour].days,
            "hour {hour} disagreed on days"
        );
        assert_eq!(
            from_statistics.columns[hour].status, from_states.columns[hour].status,
            "hour {hour} disagreed on status"
        );
    }
}
