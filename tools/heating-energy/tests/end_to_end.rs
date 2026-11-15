//! The whole pipeline, from a synthetic Home Assistant recorder database to the HTML
//! report, without ever touching a real Home Assistant instance.

use std::path::{Path, PathBuf};
use std::process::Command;

use chrono_tz::Tz::UTC;
use rusqlite::Connection;

use ha_recorder::Quantity;
use ha_recorder::model::Grouping;
use ha_recorder::source::schema::ValueColumn;
use ha_recorder::source::{LoadOptions, SourceKind, load};
use ha_testdb::{self as testdb, Flavour, OUTSIDE_ENTITY, Outages, ROOMS, ts};

use heating_energy::daily::daily_means;
use heating_energy::degree::{Analysis, Bases, analyse};
use heating_energy::report::{ReportOptions, page};
use heating_energy::rooms::House;

fn options(entity: &str) -> LoadOptions<'_> {
    LoadOptions {
        entity,
        source: SourceKind::Auto,
        value: ValueColumn::Mean,
        from: None,
        to: None,
        max_gap: 900,
        quantity: Quantity::Temperature,
        scale: None,
        clamp_negative: false,
        tz: UTC,
    }
}

/// The synthetic house as a rooms file would describe it.
fn house() -> House {
    House::parse(include_str!("../rooms.example.toml")).expect("the example parses")
}

fn run(conn: &Connection, grouping: Grouping) -> Analysis {
    let outside = load(conn, &options(OUTSIDE_ENTITY)).expect("outside loads");
    let house = house();
    let rooms: Vec<_> = house
        .rooms()
        .map(|(_, room)| {
            let loaded = load(conn, &options(&room.sensor)).expect("room loads");
            daily_means(&loaded.samples, UTC)
        })
        .collect();
    analyse(
        &daily_means(&outside.samples, UTC),
        &rooms,
        Some(&house),
        Bases::default(),
        0.9,
        grouping,
    )
}

/// A database holding one thermometer at a constant hourly mean.
fn constant(entity: &str, unit: &str, value: f64, days: i64) -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    testdb::create_schema(&conn, Flavour::Modern);
    let id = testdb::insert_statistic_meta(&conn, entity, Some(unit));
    for hour in 0..days * 24 {
        testdb::insert_statistics_row(
            &conn,
            Flavour::Modern,
            "statistics",
            id,
            ts("2024-01-01 00:00:00") + hour * 3_600,
            Some(value),
        );
    }
    conn
}

#[test]
fn ten_days_at_five_degrees_are_a_hundred_heating_degree_days() {
    let conn = constant(OUTSIDE_ENTITY, "°C", 5.0, 10);
    let outside = load(&conn, &options(OUTSIDE_ENTITY)).unwrap();
    let analysis = analyse(
        &daily_means(&outside.samples, UTC),
        &[],
        None,
        Bases::default(),
        0.9,
        Grouping::Month,
    );
    assert_eq!(analysis.record.days, 10);
    assert!((analysis.record.hdd - 100.0).abs() < 1e-9);
    assert_eq!(analysis.record.cdd, 0.0);
}

#[test]
fn a_fahrenheit_thermometer_counts_the_same_degree_days() {
    // 41 °F is 5 °C.
    let conn = constant(OUTSIDE_ENTITY, "°F", 41.0, 10);
    let outside = load(&conn, &options(OUTSIDE_ENTITY)).unwrap();
    let analysis = analyse(
        &daily_means(&outside.samples, UTC),
        &[],
        None,
        Bases::default(),
        0.9,
        Grouping::Month,
    );
    assert!(
        (analysis.record.hdd - 100.0).abs() < 1e-9,
        "{}",
        analysis.record.hdd
    );
}

#[test]
fn two_years_add_up_to_what_sql_says_they_should() {
    let conn = testdb::synthetic_temperature_database(
        "2023-01-01 00:00:00",
        730,
        0xC0FFEE,
        &Outages::none(),
    );
    let analysis = run(&conn, Grouping::Month);

    // The same sums straight from the table: every UTC day is 24 whole hourly rows, so
    // its mean is the plain average of them.
    let (hdd, cdd, days): (f64, f64, i64) = conn
        .query_row(
            "SELECT SUM(MAX(0, 15 - mean)), SUM(MAX(0, mean - 21)), COUNT(*) FROM ( \
               SELECT AVG(s.mean) AS mean FROM statistics s \
               JOIN statistics_meta m ON m.id = s.metadata_id \
               WHERE m.statistic_id = ?1 \
               GROUP BY date(s.start_ts, 'unixepoch'))",
            [OUTSIDE_ENTITY],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(analysis.record.days as i64, days);
    assert!(
        (analysis.record.hdd - hdd).abs() < 1e-6,
        "{} vs {hdd}",
        analysis.record.hdd
    );
    assert!(
        (analysis.record.cdd - cdd).abs() < 1e-6,
        "{} vs {cdd}",
        analysis.record.cdd
    );

    assert_eq!(analysis.periods.len(), 24);
    assert_eq!(analysis.years.len(), 2);
    let by_period: f64 = analysis.periods.iter().map(|period| period.hdd).sum();
    let by_year: f64 = analysis.years.iter().map(|year| year.hdd).sum();
    assert!((by_period - analysis.record.hdd).abs() < 1e-6);
    assert!((by_year - analysis.record.hdd).abs() < 1e-6);
}

#[test]
fn winter_asks_for_heat_and_the_rooms_hold_their_setpoints() {
    let conn =
        testdb::synthetic_temperature_database("2024-01-01 00:00:00", 366, 7, &Outages::none());
    let analysis = run(&conn, Grouping::Month);
    let month = |index: usize| &analysis.periods[index];

    assert!(month(0).hdd > 300.0, "January: {}", month(0).hdd);
    assert!(month(6).hdd < 20.0, "July: {}", month(6).hdd);
    assert!(month(0).hdd > 10.0 * month(6).hdd.max(1.0));

    // In January every room sits on its setpoint, give or take the noise.
    for (index, (_, _, _, setpoint)) in ROOMS.iter().enumerate() {
        let january = month(0).rooms[index].expect("recorded");
        assert!((january - setpoint).abs() < 0.2, "room {index}: {january}");
    }
    // The ground floor is the mean of its three rooms.
    let ground = (21.0 + 20.0 + 20.5) / 3.0;
    assert!((month(0).floors[0].unwrap() - ground).abs() < 0.2);
}

#[test]
fn weeks_and_months_count_the_same_days() {
    let conn =
        testdb::synthetic_temperature_database("2024-01-01 00:00:00", 120, 3, &Outages::none());
    let months = run(&conn, Grouping::Month);
    let weeks = run(&conn, Grouping::Week);
    assert!((months.record.hdd - weeks.record.hdd).abs() < 1e-9);
    assert_eq!(months.record.days, weeks.record.days);
    let summed: f64 = weeks.periods.iter().map(|week| week.hdd).sum();
    assert!((summed - weeks.record.hdd).abs() < 1e-6);
    assert!(weeks.periods.len() > 15);
}

#[test]
fn outages_leave_their_days_out_and_say_so() {
    let conn = testdb::synthetic_temperature_database(
        "2024-01-01 00:00:00",
        90,
        11,
        &Outages {
            missing_days: vec![3, 4, 5],
            missing_ranges: Vec::new(),
            missing_hours: Vec::new(),
        },
    );
    let analysis = run(&conn, Grouping::Month);
    let january = &analysis.periods[0];
    assert_eq!(january.days, 28);
    assert_eq!(january.possible_days, 31);
    assert!(!january.is_complete());

    let html = page(
        &analysis,
        &ReportOptions {
            outside_entity: OUTSIDE_ENTITY.to_string(),
            house: Some(house()),
            ..ReportOptions::default()
        },
    );
    assert!(html.contains("28 of 31 days"));
}

// --- the binary itself -------------------------------------------------------------

/// A scratch directory for one test, removed again when it ends.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("heating-energy-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn path(&self, file: &str) -> PathBuf {
        self.0.join(file)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The demo house written to a file, with grid power sensors beside the thermometers.
fn demo_database(path: &Path) {
    let conn = Connection::open(path).unwrap();
    testdb::create_schema(&conn, Flavour::Modern);
    let start = ts("2024-01-01 00:00:00");
    testdb::insert_synthetic_temperatures(&conn, start, 60, 5, &Outages::none());
    testdb::insert_synthetic_grid_power(&conn, start, 60, 5, &Outages::none());
}

fn heating_energy(args: &[&std::ffi::OsStr]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_heating-energy"))
        .args(args)
        .output()
        .expect("the binary runs")
}

#[test]
fn the_binary_writes_a_report_and_sums_it_up() {
    let scratch = Scratch::new("report");
    let db = scratch.path("ha.db");
    let out = scratch.path("report.html");
    demo_database(&db);
    let rooms = Path::new(env!("CARGO_MANIFEST_DIR")).join("rooms.example.toml");

    let output = heating_energy(&[
        "--db".as_ref(),
        db.as_os_str(),
        "--outside-entity".as_ref(),
        OUTSIDE_ENTITY.as_ref(),
        "--rooms".as_ref(),
        rooms.as_os_str(),
        "--tz".as_ref(),
        "UTC".as_ref(),
        "-o".as_ref(),
        out.as_os_str(),
    ]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "{stdout}{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("degree days:"), "{stdout}");
    assert!(stdout.contains("indoors: Ground floor"), "{stdout}");
    let html = std::fs::read_to_string(&out).unwrap();
    assert!(html.contains("Living room"));
    assert!(html.contains("<svg class=\"degree-chart\""));
}

#[test]
fn a_room_given_a_power_sensor_is_named_in_the_error() {
    let scratch = Scratch::new("wrong-sensor");
    let db = scratch.path("ha.db");
    demo_database(&db);
    let rooms = scratch.path("rooms.toml");
    std::fs::write(
        &rooms,
        "[[floor]]\nname = \"Cellar\"\nrooms = [{ name = \"Boiler room\", sensor = \
         \"sensor.grid_import_power\" }]\n",
    )
    .unwrap();

    let output = heating_energy(&[
        "--db".as_ref(),
        db.as_os_str(),
        "--outside-entity".as_ref(),
        OUTSIDE_ENTITY.as_ref(),
        "--rooms".as_ref(),
        rooms.as_os_str(),
        "-o".as_ref(),
        scratch.path("report.html").as_os_str(),
    ]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(stderr.contains("Cellar / Boiler room"), "{stderr}");
    assert!(stderr.contains("measures power"), "{stderr}");
    assert!(
        stderr.contains("Temperature sensors in this database"),
        "{stderr}"
    );
}

#[test]
fn a_broken_rooms_file_is_reported_before_anything_is_read() {
    let scratch = Scratch::new("broken-file");
    let rooms = scratch.path("rooms.toml");
    std::fs::write(&rooms, "[[floor]]\nname = \"Attic\"\n").unwrap();
    let db = scratch.path("ha.db");
    demo_database(&db);

    let output = heating_energy(&[
        "--db".as_ref(),
        db.as_os_str(),
        "--outside-entity".as_ref(),
        OUTSIDE_ENTITY.as_ref(),
        "--rooms".as_ref(),
        rooms.as_os_str(),
    ]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(stderr.contains("rooms.toml"), "{stderr}");
    assert!(stderr.contains("\"Attic\" has no rooms"), "{stderr}");
}

/// The thermostat on the bathroom radiator, as Home Assistant names it.
const THERMOSTAT: &str = "sensor.bad_og_heizkorper_temperature";
/// Its offset setting: another entity of the same device.
const THERMOSTAT_OFFSET: &str = "number.bad_og_heizkorper_temperature_offset";

/// Ten days at 5 °C outside, the thermostat reporting 19 °C every hour, an offset setting
/// `offset` taking the `(hour, value)`s given, and, if asked, raw readings every five
/// minutes that move the way a thermostat applying its offset with `sign` would.
fn thermostat_database(path: &Path, offset: &str, values: &[(i64, &str)], raw_sign: Option<f64>) {
    let conn = Connection::open(path).unwrap();
    testdb::create_schema(&conn, Flavour::Modern);
    let start = ts("2024-01-01 00:00:00");
    for (entity, value) in [(OUTSIDE_ENTITY, 5.0), (THERMOSTAT, 19.0)] {
        let id = testdb::insert_statistic_meta(&conn, entity, Some("°C"));
        for hour in 0..240 {
            testdb::insert_statistics_row(
                &conn,
                Flavour::Modern,
                "statistics",
                id,
                start + hour * 3_600,
                Some(value),
            );
        }
    }
    let offset_id = testdb::insert_states_meta(&conn, offset);
    for (hour, value) in values {
        let at = start + hour * 3_600;
        testdb::insert_state(&conn, Flavour::Modern, "", offset_id, at, value);
    }
    if let Some(sign) = raw_sign {
        let sensor_id = testdb::insert_states_meta(&conn, THERMOSTAT);
        let steps: Vec<(i64, f64)> = values
            .iter()
            .filter_map(|(hour, value)| Some((start + hour * 3_600, value.parse().ok()?)))
            .collect();
        for index in 0..(240 * 12) {
            let at = start + index * 300 + 7;
            let offset = steps
                .iter()
                .rev()
                .find(|(from, _)| *from <= at)
                .map_or(0.0, |(_, value)| *value);
            let reading = format!("{:.1}", 21.0 + sign * offset);
            testdb::insert_state(&conn, Flavour::Modern, "", sensor_id, at, &reading);
        }
    }
}

/// Run the binary on `db` with the thermostat as the only room.
fn bathroom(scratch: &Scratch, db: &Path) -> (String, String) {
    let rooms = scratch.path("rooms.toml");
    std::fs::write(
        &rooms,
        format!("[[floor]]\nname = \"Upstairs\"\nrooms = [{{ name = \"Bath\", sensor = \"{THERMOSTAT}\" }}]\n"),
    )
    .unwrap();
    let out = scratch.path("report.html");
    let output = heating_energy(&[
        "--db".as_ref(),
        db.as_os_str(),
        "--outside-entity".as_ref(),
        OUTSIDE_ENTITY.as_ref(),
        "--rooms".as_ref(),
        rooms.as_os_str(),
        "--tz".as_ref(),
        "UTC".as_ref(),
        "-o".as_ref(),
        out.as_os_str(),
    ]);
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    assert!(
        output.status.success(),
        "{stdout}{}",
        String::from_utf8_lossy(&output.stderr)
    );
    (stdout, std::fs::read_to_string(out).unwrap())
}

#[test]
fn a_thermostats_offset_is_found_and_taken_back_out_and_said_so() {
    let scratch = Scratch::new("offset");
    let db = scratch.path("ha.db");
    thermostat_database(
        &db,
        THERMOSTAT_OFFSET,
        &[(48, "-2"), (100, "unavailable"), (101, "-2")],
        None,
    );

    let (stdout, html) = bathroom(&scratch, &db);
    assert!(stdout.contains("indoors: Upstairs 21 °C"), "{stdout}");
    assert!(
        stdout.contains(
            "offset: Upstairs / Bath: readings raised by 2 °C to take out its thermostat's \
             offset number.bad_og_heizkorper_temperature_offset (-2 °C)"
        ),
        "{stdout}"
    );
    assert!(
        stdout.contains(
            "the offset is on record since 2024-01-03, and taken to have been the same before"
        ),
        "{stdout}"
    );
    assert!(stdout.contains("not checked"), "{stdout}");
    assert!(
        html.contains(
            "<strong>Upstairs / Bath</strong> is shown as its thermostat measured rather than as \
             it reported: readings raised by 2 °C"
        ),
        "the report says so too"
    );
}

#[test]
fn another_devices_offset_is_left_alone() {
    let scratch = Scratch::new("offset-other-device");
    let db = scratch.path("ha.db");
    thermostat_database(
        &db,
        "number.bad_heizkorper_temperature_offset",
        &[(0, "-2")],
        None,
    );

    let (stdout, html) = bathroom(&scratch, &db);
    assert!(stdout.contains("indoors: Upstairs 19 °C"), "{stdout}");
    assert!(!stdout.contains("offset:"), "{stdout}");
    assert!(!stdout.contains("note:"), "{stdout}");
    assert!(!html.contains("as its thermostat measured"));
}

#[test]
fn a_thermostat_subtracting_its_offset_is_read_from_its_readings() {
    let scratch = Scratch::new("offset-direction");
    let db = scratch.path("ha.db");
    // Changed from -1 to -2 at hour 100, and the readings rose by one degree when it was:
    // this thermostat reports what it measures minus its offset.
    thermostat_database(
        &db,
        THERMOSTAT_OFFSET,
        &[(0, "-1"), (100, "-2")],
        Some(-1.0),
    );

    let (stdout, _) = bathroom(&scratch, &db);
    assert!(
        stdout.contains(
            "checked: when the offset went from -1 °C to -2 °C at 2024-01-05 04:00, the reading \
             moved by +1 °C - the other way (once), so this thermostat subtracts it"
        ),
        "{stdout}"
    );
    assert!(
        stdout.contains("readings lowered by 1 °C, then lowered by 2 °C"),
        "{stdout}"
    );
    assert!(!stdout.contains("warning:"), "{stdout}");

    // One that adds it moves with it, and is said to.
    let db = scratch.path("adds.db");
    thermostat_database(&db, THERMOSTAT_OFFSET, &[(0, "-1"), (100, "-2")], Some(1.0));
    let (stdout, _) = bathroom(&scratch, &db);
    assert!(
        stdout.contains("checked: the reading moved with the offset when it changed (once)"),
        "{stdout}"
    );
    assert!(
        stdout.contains("readings raised by 1 °C, then raised by 2 °C"),
        "{stdout}"
    );
}
