//! The payback pipeline end to end, from a synthetic recorder database to the report.

use chrono_tz::Europe::Berlin;
use chrono_tz::Tz::UTC;
use rusqlite::Connection;

use pv_probability::render::{PaybackOptions, payback_page};
use pv_probability::source::schema::ValueColumn;
use pv_probability::source::testdb::{self, EXPORT_ENTITY, IMPORT_ENTITY};
use pv_probability::source::{LoadOptions, SourceKind, load};
use pv_probability::storage::pair::{Paired, pair};
use pv_probability::storage::series::{accumulate, grid_for};
use pv_probability::storage::simulate::{BatterySpec, simulate};
use pv_probability::storage::sweep::{Economics, Sweep, size_range, sweep};

const HOUR: i64 = 3_600;

/// Two years of a household with a solar array, as hourly power statistics.
fn database(days: i64) -> Connection {
    testdb::synthetic_grid_database(
        "2023-01-01 00:00:00",
        days,
        0xC0FFEE,
        &testdb::Outages::none(),
    )
}

fn options(entity: &str) -> LoadOptions<'_> {
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

fn economics() -> Economics {
    Economics {
        price_per_kwh: 0.35,
        feed_in_price: 0.0,
        cost_per_kwh: 500.0,
        base_cost: 1_500.0,
        currency: "EUR".to_string(),
    }
}

fn paired_history(conn: &Connection, slot_seconds: i64) -> Paired {
    let import = load(conn, &options(IMPORT_ENTITY)).expect("import samples load");
    let export = load(conn, &options(EXPORT_ENTITY)).expect("export samples load");
    let grid = grid_for(&import.samples, &export.samples, slot_seconds).expect("a timeline");
    pair(
        &accumulate(&import.samples, grid),
        &accumulate(&export.samples, grid),
        0.9,
    )
}

fn run(conn: &Connection) -> (Paired, Sweep) {
    let paired = paired_history(conn, HOUR);
    let result = sweep(
        &paired,
        &size_range(1.0, 20.0, 1.0),
        &BatterySpec::new(0.0, 0.9, 0.9),
        &economics(),
    );
    (paired, result)
}

#[test]
fn two_years_of_power_sensors_become_a_paired_hourly_timeline() {
    let conn = database(730);
    let paired = paired_history(&conn, HOUR);

    assert_eq!(paired.steps.len(), 730 * 24);
    assert_eq!(paired.dropped(), 0, "both sensors report every hour");
    assert!(paired.import_kwh > 0.0 && paired.export_kwh > 0.0);
    // A household with a big array buys less than it would without one, but the evenings
    // and the winter still cost money.
    assert!(
        paired.import_kwh > paired.export_kwh,
        "import {} should exceed export {}",
        paired.import_kwh,
        paired.export_kwh
    );
}

#[test]
fn a_bigger_battery_never_saves_less_and_eventually_pays_back_more_slowly() {
    let conn = database(730);
    let (_, result) = run(&conn);

    let savings: Vec<f64> = result.results.iter().map(|r| r.annual_savings).collect();
    for pair in savings.windows(2) {
        assert!(pair[1] >= pair[0] - 1e-9, "savings fell: {savings:?}");
    }

    let best = result
        .best
        .expect("something pays back on a synthetic house");
    assert!(
        best > 0 && best + 1 < result.results.len(),
        "the sweet spot should be inside the sweep, not at an end: {best}"
    );
    let best_years = result.results[best].payback_years.unwrap();
    let largest_years = result.results.last().unwrap().payback_years.unwrap();
    assert!(
        largest_years > best_years,
        "20 kWh ({largest_years}) should pay back more slowly than the best ({best_years})"
    );
}

#[test]
fn the_battery_can_only_ever_shift_what_crossed_the_meter() {
    let conn = database(365);
    let (paired, result) = run(&conn);

    for size in &result.results {
        let avoided = size.simulation.avoided_import_kwh;
        assert!(avoided <= paired.import_kwh + 1e-6, "{avoided} > import");
        assert!(avoided <= paired.export_kwh + 1e-6, "{avoided} > export");
        assert!(size.import_reduction <= 1.0);
    }
}

#[test]
fn a_costlier_kilowatt_hour_pays_a_battery_off_sooner() {
    let conn = database(365);
    let paired = paired_history(&conn, HOUR);
    let sizes = size_range(5.0, 5.0, 1.0);
    let template = BatterySpec::new(0.0, 0.9, 0.9);

    let cheap = sweep(&paired, &sizes, &template, &economics());
    let dear = sweep(
        &paired,
        &sizes,
        &template,
        &Economics {
            price_per_kwh: 0.70,
            ..economics()
        },
    );
    let (cheap, dear) = (
        cheap.results[0].payback_years.unwrap(),
        dear.results[0].payback_years.unwrap(),
    );
    assert!((cheap / dear - 2.0).abs() < 1e-6, "{cheap} vs {dear}");
}

#[test]
fn losses_and_a_shallower_depth_of_discharge_both_cost_savings() {
    let conn = database(365);
    let paired = paired_history(&conn, HOUR);
    let saved = |spec: BatterySpec| simulate(&paired.steps, &spec).avoided_import_kwh;

    let perfect = saved(BatterySpec::new(10.0, 1.0, 1.0));
    assert!(saved(BatterySpec::new(10.0, 1.0, 0.81)) < perfect, "losses");
    assert!(
        saved(BatterySpec::new(10.0, 0.8, 1.0)) < perfect,
        "headroom"
    );
    assert_eq!(saved(BatterySpec::new(0.0, 1.0, 1.0)), 0.0);
}

#[test]
fn a_partial_year_is_annualised_and_says_so() {
    let conn = database(180);
    let (paired, result) = run(&conn);

    assert!(
        (result.annualisation - 365.2425 / 180.0).abs() < 0.02,
        "{}",
        result.annualisation
    );
    let best = result.best_result().expect("something pays back");
    assert!((best.annual_savings - best.observed_savings * result.annualisation).abs() < 1e-9);

    let html = payback_page(&result, &paired, &PaybackOptions::default());
    assert!(html.contains("less than a year"));
    assert!(html.contains("multiplied by"));
}

#[test]
fn an_outage_drops_the_slots_it_swallowed_rather_than_inventing_them() {
    let outages = testdb::Outages {
        missing_ranges: vec![(40, 55)],
        ..Default::default()
    };
    let conn = testdb::synthetic_grid_database("2023-01-01 00:00:00", 120, 0xC0FFEE, &outages);
    let paired = paired_history(&conn, HOUR);

    assert_eq!(
        paired.steps.len(),
        (120 - 15) * 24,
        "the fortnight the recorder was down cannot be simulated"
    );
    assert_eq!(
        paired.dropped(),
        0,
        "neither sensor recorded it, so nothing is dropped"
    );
    assert!(
        paired.observed_seconds < 120.0 * 86_400.0,
        "and the annualisation is based on what was really seen"
    );
}

#[test]
fn a_sensor_that_stops_early_only_costs_the_hours_it_missed() {
    let conn = database(30);
    // Read the export sensor for a shorter window than the import sensor.
    let import = load(&conn, &options(IMPORT_ENTITY)).expect("import loads");
    let mut narrowed = options(EXPORT_ENTITY);
    narrowed.to = Some(testdb::ts("2023-01-21 00:00:00"));
    let export = load(&conn, &narrowed).expect("export loads");

    let grid = grid_for(&import.samples, &export.samples, HOUR).expect("a timeline");
    let paired = pair(
        &accumulate(&import.samples, grid),
        &accumulate(&export.samples, grid),
        0.9,
    );

    assert_eq!(paired.steps.len(), 20 * 24, "only the overlap is simulated");
    assert_eq!(
        paired.dropped_unpaired,
        10 * 24,
        "the hours only the import sensor saw are counted, not silently used"
    );
}

#[test]
fn the_report_is_a_self_contained_html_file() {
    let conn = database(730);
    let (paired, result) = run(&conn);
    let html = payback_page(
        &result,
        &paired,
        &PaybackOptions {
            import_entity: IMPORT_ENTITY.to_string(),
            export_entity: EXPORT_ENTITY.to_string(),
            metadata: vec![("Timezone".to_string(), Berlin.to_string())],
            tz: Berlin,
            ..PaybackOptions::default()
        },
    );

    assert!(html.starts_with("<!doctype html>"));
    assert!(html.contains(IMPORT_ENTITY) && html.contains(EXPORT_ENTITY));
    assert!(!html.contains("http://") && !html.contains("https://"));
    assert!(!html.contains("<script"));
    assert!(!html.contains("NaN"));
    assert!(
        html.len() < 1024 * 1024,
        "report grew to {} bytes",
        html.len()
    );

    // Keep a copy for eyeballing after a test run.
    let out = std::env::temp_dir().join("energy-storage-payback-example.html");
    std::fs::write(&out, &html).expect("write example report");
}

#[test]
fn finer_slots_split_the_same_energy_into_more_steps() {
    // Within an hour the house can import and export in turn; an hourly slot still sees
    // both, but the finer the slot the less a battery is credited with bridging.
    let conn = database(60);
    let hourly = paired_history(&conn, HOUR);
    let quarter = paired_history(&conn, 15 * 60);

    assert!(quarter.steps.len() > hourly.steps.len());
    assert!(
        (quarter.import_kwh - hourly.import_kwh).abs() < 1e-6,
        "the energy is the same however it is sliced"
    );
    assert!((quarter.export_kwh - hourly.export_kwh).abs() < 1e-6);
}

#[test]
fn an_energy_counter_passed_by_mistake_is_diagnosed_rather_than_read() {
    let conn = Connection::open_in_memory().expect("in-memory database");
    testdb::create_schema(&conn, testdb::Flavour::Modern);
    let start = testdb::ts("2023-01-01 00:00:00");
    testdb::insert_synthetic_grid_power(&conn, start, 10, 0xC0FFEE, &testdb::Outages::none());
    testdb::insert_synthetic_energy_counter(
        &conn,
        "sensor.grid_import_energy",
        start,
        10,
        0xC0FFEE,
        &testdb::Outages::none(),
    );

    let loaded = load(&conn, &options("sensor.grid_import_energy")).expect("the query runs");
    assert!(
        loaded.samples.is_empty(),
        "a kWh counter has no mean to read"
    );

    let error = pv_probability::source::explain_no_samples(
        &conn,
        &options("sensor.grid_import_energy"),
        &loaded,
        UTC,
    )
    .to_string();
    assert!(error.contains("sensor.grid_import_energy"), "{error}");
    assert!(
        error.contains(IMPORT_ENTITY),
        "the power sensor beside it must be named: {error}"
    );
}
