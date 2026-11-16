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

use heating_energy::daily::{daily_means, room_days};
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
            let thermometers: Vec<_> = room
                .sensors
                .iter()
                .map(|sensor| {
                    let loaded = load(conn, &options(sensor)).expect("room loads");
                    daily_means(&loaded.samples, UTC)
                })
                .collect();
            room_days(&thermometers, 0.9).days
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
    assert_eq!(notes(&stdout), [WAITING_WITHOUT_ENERGY], "{stdout}");
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

/// Ten days at 5 °C outside, and every `(thermometer, °C, days)` reporting that constant
/// hourly mean from the first day for that many days.
fn living_room_database(path: &Path, thermometers: &[(&str, f64, i64)]) {
    let conn = Connection::open(path).unwrap();
    testdb::create_schema(&conn, Flavour::Modern);
    let start = ts("2024-01-01 00:00:00");
    let mut all = vec![(OUTSIDE_ENTITY, 5.0, 10)];
    all.extend_from_slice(thermometers);
    for (entity, value, days) in all {
        let id = testdb::insert_statistic_meta(&conn, entity, Some("°C"));
        for hour in 0..days * 24 {
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
}

/// Run the binary on `db` with one room, written as `entry`.
fn one_room(scratch: &Scratch, db: &Path, entry: &str) -> std::process::Output {
    let rooms = scratch.path("rooms.toml");
    std::fs::write(
        &rooms,
        format!(
            "[[floor]]\nname = \"Ground floor\"\nrooms = [{{ name = \"Living room\"{entry} }}]\n"
        ),
    )
    .unwrap();
    heating_energy(&[
        "--db".as_ref(),
        db.as_os_str(),
        "--outside-entity".as_ref(),
        OUTSIDE_ENTITY.as_ref(),
        "--rooms".as_ref(),
        rooms.as_os_str(),
        "--tz".as_ref(),
        "UTC".as_ref(),
        "-o".as_ref(),
        scratch.path("report.html").as_os_str(),
    ])
}

#[test]
fn a_room_with_several_thermometers_is_their_mean() {
    let scratch = Scratch::new("several");
    let db = scratch.path("ha.db");
    living_room_database(
        &db,
        &[
            ("sensor.sofa_temperature", 20.0, 10),
            ("sensor.desk_temperature", 22.0, 10),
        ],
    );

    let output = one_room(
        &scratch,
        &db,
        ", sensors = [\"sensor.sofa_temperature\", \"sensor.desk_temperature\"]",
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");
    assert!(stdout.contains("indoors: Ground floor 21 °C"), "{stdout}");
    assert_eq!(notes(&stdout), [WAITING_WITHOUT_ENERGY], "{stdout}");
    let html = std::fs::read_to_string(scratch.path("report.html")).unwrap();
    assert!(
        html.contains(
            "<strong>Ground floor / Living room</strong> is the mean of its 2 thermometers, \
             sensor.sofa_temperature and sensor.desk_temperature, every one counting alike on \
             the days it covered.</p>"
        ),
        "the report says what the room is made of"
    );
}

#[test]
fn days_only_some_thermometers_covered_rest_on_those_and_are_counted() {
    let scratch = Scratch::new("several-partial");
    let db = scratch.path("ha.db");
    // The desk thermometer only recorded the first four days.
    living_room_database(
        &db,
        &[
            ("sensor.sofa_temperature", 20.0, 10),
            ("sensor.desk_temperature", 22.0, 4),
        ],
    );

    let output = one_room(
        &scratch,
        &db,
        ", sensors = [\"sensor.sofa_temperature\", \"sensor.desk_temperature\"]",
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");
    // Four days at 21 °C and six at 20 °C.
    assert!(stdout.contains("indoors: Ground floor 20.4 °C"), "{stdout}");
    assert!(
        stdout.contains(
            "note: Ground floor / Living room: on 6 days only some of its 2 thermometers \
             covered enough of the day; they are the mean of those that did"
        ),
        "{stdout}"
    );

    // A thermometer that is not there at all is named, with the room.
    let output = one_room(
        &scratch,
        &db,
        ", sensors = [\"sensor.sofa_temperature\", \"sensor.window_temperature\"]",
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(
        stderr.contains(
            "reading the thermometer sensor.window_temperature of Ground floor / Living room"
        ),
        "{stderr}"
    );
}

/// The heating's energy as a running total, as Home Assistant names it.
const HEATING_ENERGY: &str = "number.heizol_energy";

/// Outside in 2024, every month at its own steady temperature: June to September at or
/// above the heating base.
const MONTHLY_TEMPERATURES: [f64; 12] = [
    0.0, 2.0, 5.0, 9.0, 13.0, 17.0, 19.0, 18.0, 15.5, 10.0, 5.0, 1.0,
];

/// 2024 hour by hour outside, at [`MONTHLY_TEMPERATURES`], and the 1st of January 2025,
/// so December is over in the record.
fn heating_year() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    testdb::create_schema(&conn, Flavour::Modern);
    let outside = testdb::insert_statistic_meta(&conn, OUTSIDE_ENTITY, Some("°C"));
    let mut hour = ts("2024-01-01 00:00:00");
    while hour < ts("2025-01-02 00:00:00") {
        testdb::insert_statistics_row(
            &conn,
            Flavour::Modern,
            "statistics",
            outside,
            hour,
            Some(temperature_at(hour)),
        );
        hour += 3_600;
    }
    conn
}

fn temperature_at(ts: i64) -> f64 {
    use chrono::Datelike;
    let month = chrono::DateTime::from_timestamp(ts, 0).unwrap().month0() as usize;
    MONTHLY_TEMPERATURES[month]
}

/// The running total at `at`, for a house heating at two kWh per heating degree day and
/// heating ten kWh of water a day since the start of 2024, from 10,000 kWh: once the water
/// heating is taken off, every month lies exactly on the line.
fn total_at(at: i64) -> f64 {
    let mut total = 10_000.0;
    let mut day = ts("2024-01-01 00:00:00");
    while day < at {
        let share = (at - day).min(86_400) as f64 / 86_400.0;
        total += share * (2.0 * (15.0f64 - temperature_at(day)).max(0.0) + 10.0);
        day += 86_400;
    }
    total
}

/// Midnight UTC on the first of every month from January 2024 to January 2025: readings
/// that each close the month before.
fn month_starts() -> Vec<i64> {
    let mut starts: Vec<i64> = (1..=12)
        .map(|month| ts(&format!("2024-{month:02}-01 00:00:00")))
        .collect();
    starts.push(ts("2025-01-01 00:00:00"));
    starts
}

/// Run the binary on `conn`, written to a file, with the heating energy from `entity`.
fn with_energy(scratch: &Scratch, conn: &Connection, entity: &str) -> (String, String, String) {
    let db = scratch.path("ha.db");
    let _ = std::fs::remove_file(&db);
    conn.execute("VACUUM INTO ?1", [db.to_str().unwrap()])
        .unwrap();
    let out = scratch.path("report.html");
    let output = heating_energy(&[
        "--db".as_ref(),
        db.as_os_str(),
        "--outside-entity".as_ref(),
        OUTSIDE_ENTITY.as_ref(),
        "--energy-entity".as_ref(),
        entity.as_ref(),
        "--tz".as_ref(),
        "UTC".as_ref(),
        "-v".as_ref(),
        "-o".as_ref(),
        out.as_os_str(),
    ]);
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(output.status.success(), "{stdout}{stderr}");
    (stdout, stderr, std::fs::read_to_string(out).unwrap())
}

/// Write the running total as raw states at `times`, each again at a restart a week on.
fn raw_states(conn: &Connection, times: &[i64]) {
    let id = testdb::insert_states_meta(conn, HEATING_ENERGY);
    for at in times {
        let value = format!("{:.3}", total_at(*at));
        testdb::insert_state(conn, Flavour::Modern, "", id, *at, &value);
    }
    for at in times {
        let value = format!("{:.3}", total_at(*at));
        testdb::insert_state(conn, Flavour::Modern, "", id, at + 7 * 86_400 - 1, &value);
    }
}

/// The lines of the run that say what the heating energy came to.
fn efficiency_lines(stdout: &str) -> Vec<&str> {
    stdout
        .lines()
        .filter(|line| line.starts_with("efficiency:") || line.starts_with("  20"))
        .collect()
}

#[test]
fn a_monthly_meter_gives_the_houses_kwh_per_degree_day() {
    let scratch = Scratch::new("energy-monthly");
    let conn = heating_year();
    raw_states(&conn, &month_starts());

    let (stdout, stderr, html) = with_energy(&scratch, &conn, HEATING_ENERGY);
    let lines = efficiency_lines(&stdout);
    assert_eq!(lines.len(), 3, "{stdout}");
    assert_eq!(
        lines[0],
        "efficiency: 2.00 kWh per heating degree day (± 0.00), once water heating of 307 kWh a \
         month on average is taken off - a line through 12 months of number.heizol_energy"
    );
    // January to June has no summer of its own on record.
    assert_eq!(
        lines[1],
        "  2023/24: 2.00 kWh per degree day (± 0.00) over 6 months; water heating 307 kWh a \
         month, 2024/25's"
    );
    assert!(
        lines[2].starts_with("  2024/25: 2.00 kWh per degree day (± 0.00) over 6 months, 0% "),
        "{stdout}"
    );
    assert!(
        lines[2].ends_with(
            "; water heating 307 kWh a month, from July 2024, August 2024 and September 2024"
        ),
        "{stdout}"
    );
    assert!(
        stderr.contains("read 13 readings of number.heizol_energy from its raw states"),
        "{stderr}"
    );
    assert!(html.contains("<h2>Heating energy</h2>"));
    assert!(html.contains("<strong>2.00 kWh</strong> per heating degree day"));
    assert_eq!(html.matches("class=\"dot\"").count(), 12);
    assert!(html.contains("once water heating of <strong>307 kWh a month</strong> is taken off"));
    // 31 days of 15 degree days each at two kWh apiece, and of ten kWh of water heating.
    assert!(
        html.contains(
            "<th scope=\"row\">January 2024</th><td>1,240 kWh</td><td>310 kWh</td>\
             <td>930 kWh</td><td>465</td>"
        ),
        "{html}"
    );
    assert!(html.contains("<td>310 kWh, one of the warmest</td>"));
}

#[test]
fn reading_the_meter_mid_month_as_well_changes_nothing() {
    let scratch = Scratch::new("energy-fortnightly");
    let monthly = {
        let conn = heating_year();
        raw_states(&conn, &month_starts());
        with_energy(&scratch, &conn, HEATING_ENERGY).0
    };
    let conn = heating_year();
    let mut times = month_starts();
    times.extend((1..=12).map(|month| ts(&format!("2024-{month:02}-15 12:00:00"))));
    times.sort_unstable();
    raw_states(&conn, &times);

    let (stdout, stderr, _) = with_energy(&scratch, &conn, HEATING_ENERGY);
    assert!(stderr.contains("read 25 readings"), "{stderr}");
    assert_eq!(efficiency_lines(&stdout), efficiency_lines(&monthly));
}

#[test]
fn a_month_not_read_is_named_and_left_out_with_the_one_after() {
    let scratch = Scratch::new("energy-skipped");
    let conn = heating_year();
    // Nobody read the meter at the end of July.
    let times: Vec<i64> = month_starts()
        .into_iter()
        .filter(|at| *at != ts("2024-08-01 00:00:00"))
        .collect();
    raw_states(&conn, &times);

    let (stdout, _, html) = with_energy(&scratch, &conn, HEATING_ENERGY);
    assert!(stdout.contains("a line through 10 months"), "{stdout}");
    // With July and August out, 2024/25 has one warm month left, and 2023/24 two.
    assert!(
        stdout.contains(
            "note: water heating is left in number.heizol_energy: no season has three months \
             from May to September on record to tell it by"
        ),
        "{stdout}"
    );
    assert!(
        stdout.contains(
            "note: July 2024 of number.heizol_energy left out of the fit: the total did not \
             change over it - not read, or nothing used"
        ),
        "{stdout}"
    );
    assert!(
        stdout.contains(
            "note: August 2024 of number.heizol_energy left out of the fit: the total did not \
             change over the month before, so it may hold both"
        ),
        "{stdout}"
    );
    assert!(html.contains("<td>no, it may hold the month before</td>"));
}

#[test]
fn a_meter_kept_as_long_term_statistics_is_read_from_them() {
    let scratch = Scratch::new("energy-statistics");
    let conn = heating_year();
    // A sensor with a state class of total_increasing, which Home Assistant keeps for good,
    // here in MWh. Its hourly state carries the total from each reading on.
    let id = testdb::insert_statistic_meta(&conn, "sensor.heizol_energy_total", Some("MWh"));
    let readings = month_starts();
    let mut hour = readings[0] - 3_600;
    while hour < *readings.last().unwrap() {
        let read = readings
            .iter()
            .rev()
            .find(|at| **at <= hour + 3_600)
            .unwrap();
        testdb::insert_statistics_sum_row(
            &conn,
            Flavour::Modern,
            "statistics",
            id,
            hour,
            total_at(*read) / 1_000.0,
        );
        hour += 3_600;
    }

    let (stdout, stderr, _) = with_energy(&scratch, &conn, "sensor.heizol_energy_total");
    assert!(
        stdout.contains("efficiency: 2.00 kWh per heating degree day"),
        "MWh are read as kWh: {stdout}"
    );
    assert!(stdout.contains("a line through 12 months"), "{stdout}");
    assert!(stderr.contains("from its long-term statistics"), "{stderr}");
}

#[test]
fn a_short_history_says_what_it_has_and_how_to_keep_more() {
    let scratch = Scratch::new("energy-short");
    let conn = heating_year();
    // Ten days of raw states hold the last reading, and nothing before.
    raw_states(&conn, &month_starts()[12..]);

    let (stdout, _, html) = with_energy(&scratch, &conn, HEATING_ENERGY);
    assert!(
        stdout.contains(
            "efficiency: no line yet - 1 reading of number.heizol_energy covers 0 whole months \
             where the outside was recorded, 0 fit to use, and a line needs 3"
        ),
        "{stdout}"
    );
    assert!(
        stdout.contains("see the README for keeping them"),
        "{stdout}"
    );
    assert!(html.contains("There is no line to draw yet"));
}

/// Two rooms kept at their own steady temperature: the living room at 21 °C over 30 m²
/// and the bedroom at 17 °C over 10 m², which makes the house 20 °C.
const HOUSE_ROOMS: [(&str, &str, f64, f64); 2] = [
    ("Living room", "sensor.living_room_temperature", 21.0, 30.0),
    ("Bedroom", "sensor.bedroom_temperature", 17.0, 10.0),
];

/// The running total at `at` for a house losing 150 W for every kelvin it is kept above
/// the outside, heated whenever the outside is colder than 15 °C, and heating ten kWh of
/// water a day, from 10,000 kWh at the start of 2024.
fn house_total_at(at: i64) -> f64 {
    let mut total = 10_000.0;
    let mut day = ts("2024-01-01 00:00:00");
    while day < at {
        let share = (at - day).min(86_400) as f64 / 86_400.0;
        let outside = temperature_at(day);
        let heating = if outside < 15.0 {
            0.150 * 24.0 * (20.0 - outside)
        } else {
            0.0
        };
        total += share * (heating + 10.0);
        day += 86_400;
    }
    total
}

#[test]
fn rooms_with_a_floor_area_give_the_houses_heat_loss_coefficient() {
    let scratch = Scratch::new("energy-heat-loss");
    let conn = heating_year();
    for (_, sensor, temperature, _) in HOUSE_ROOMS {
        let id = testdb::insert_statistic_meta(&conn, sensor, Some("°C"));
        let mut hour = ts("2024-01-01 00:00:00");
        while hour < ts("2025-01-02 00:00:00") {
            testdb::insert_statistics_row(
                &conn,
                Flavour::Modern,
                "statistics",
                id,
                hour,
                Some(temperature),
            );
            hour += 3_600;
        }
    }
    let meter = testdb::insert_states_meta(&conn, HEATING_ENERGY);
    for at in month_starts() {
        let value = format!("{:.3}", house_total_at(at));
        testdb::insert_state(&conn, Flavour::Modern, "", meter, at, &value);
    }
    let db = scratch.path("ha.db");
    conn.execute("VACUUM INTO ?1", [db.to_str().unwrap()])
        .unwrap();
    let rooms = scratch.path("rooms.toml");
    let entries: Vec<String> = HOUSE_ROOMS
        .iter()
        .map(|(name, sensor, _, area)| {
            format!("{{ name = \"{name}\", sensor = \"{sensor}\", area = {area} }}")
        })
        .collect();
    std::fs::write(
        &rooms,
        format!(
            "[house]\ntotal_area = 80\n\n[[floor]]\nname = \"Ground floor\"\nrooms = [{}, {{ name = \
             \"Hall\", sensor = \"{OUTSIDE_ENTITY}\" }}]\n",
            entries.join(", ")
        ),
    )
    .unwrap();
    let out = scratch.path("report.html");
    let output = heating_energy(&[
        "--db".as_ref(),
        db.as_os_str(),
        "--outside-entity".as_ref(),
        OUTSIDE_ENTITY.as_ref(),
        "--energy-entity".as_ref(),
        HEATING_ENERGY.as_ref(),
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
    // January to May and October to December are colder than the heating base.
    assert!(
        stdout.contains(
            "heat loss: 150 W/K over 8 heating months, the house kept at 20 °C - its 2 rooms \
             weighted by 40 m² of floor"
        ),
        "{stdout}"
    );
    assert!(
        !stdout.contains("for free"),
        "no free heat in this house: {stdout}"
    );
    assert!(
        stdout.contains("  2023/24: 150 W/K at 20 °C over 5 months"),
        "{stdout}"
    );
    assert!(
        stdout.contains("  2024/25: 150 W/K at 20 °C over 3 months; against 2023/24: "),
        "{stdout}"
    );
    assert!(
        stdout.contains(
            "note: Ground floor / Hall has no area in the rooms file, so the house's \
             temperature leaves it out"
        ),
        "{stdout}"
    );
    let html = std::fs::read_to_string(out).unwrap();
    assert!(html.contains("<h3>Building or behaviour</h3>"));
    assert!(html.contains("<strong>150 W/K</strong> heat loss, the house kept at <strong>20 °C"));
    // January: 20 kelvin for 744 hours.
    assert!(
        html.contains("<td>20 °C</td><td>14,880</td><td>yes</td>"),
        "{html}"
    );
    assert!(html.contains("<td>no, a mild month</td>"));

    // No free heat in this house: every degree-hour costs the whole 150 W/K. January to May
    // are 3,648 hours, and their heating 7,754 kWh.
    assert!(
        stdout.contains(
            "a degree warmer: 547 kWh more heating energy in a season like 2023/24, with its 5 \
             heating months: 7.1% of its heating energy - the rule of thumb says 6%; each \
             degree-hour costs 150 Wh"
        ),
        "{stdout}"
    );
    assert!(
        stdout.contains("  2024/25: 331 kWh over 3 heating months, 6.8% of its heating energy"),
        "{stdout}"
    );
    assert!(html.contains("<h2>A degree warmer</h2>"));

    // 2024's heating, 12,611 kWh, over the 40 m² of the rooms and the 80 m² of the house.
    assert!(
        stdout.contains(
            "benchmark: 315 kWh/(m²·a) over the 40 m² heated, class H, and 158 kWh/(m²·a) over \
             all 80 m², class E - the latest twelve whole months, January 2024 to December 2024"
        ),
        "{stdout}"
    );
    assert!(html.contains("<h2>Annual benchmark</h2>"));
    assert!(html.contains(
        "<th scope=\"row\">January 2024 to December 2024 (the latest)</th><td>12,611 kWh</td>"
    ));
    assert!(html.contains(
        "<strong>547 kWh</strong> more heating energy for every degree warmer in a season like \
         2023/24, with its 5 heating months: <strong>7.1%</strong> of its heating energy - \
         against the rule of thumb's 6%."
    ));
}

#[test]
fn rooms_without_a_floor_area_say_how_to_get_the_heat_loss() {
    let scratch = Scratch::new("energy-no-area");
    let conn = heating_year();
    raw_states(&conn, &month_starts());
    let db = scratch.path("ha.db");
    conn.execute("VACUUM INTO ?1", [db.to_str().unwrap()])
        .unwrap();
    let rooms = scratch.path("rooms.toml");
    std::fs::write(
        &rooms,
        format!(
            "[[floor]]\nname = \"Ground floor\"\nrooms = [{{ name = \"Hall\", sensor = \
             \"{OUTSIDE_ENTITY}\" }}]\n"
        ),
    )
    .unwrap();
    let output = heating_energy(&[
        "--db".as_ref(),
        db.as_os_str(),
        "--outside-entity".as_ref(),
        OUTSIDE_ENTITY.as_ref(),
        "--energy-entity".as_ref(),
        HEATING_ENERGY.as_ref(),
        "--rooms".as_ref(),
        rooms.as_os_str(),
        "--tz".as_ref(),
        "UTC".as_ref(),
        "-o".as_ref(),
        scratch.path("report.html").as_os_str(),
    ]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");
    assert!(
        stdout.contains(
            "note: Building or behaviour, A degree warmer and Annual benchmark wait for data - \
             the rooms' floor areas in the rooms file; the report says what each needs"
        ),
        "{stdout}"
    );
    assert!(!stdout.contains("heat loss:"));
    let html = std::fs::read_to_string(scratch.path("report.html")).unwrap();
    // Every heading is there, those without their data saying what they need.
    for heading in [
        "<h3>Building or behaviour</h3>",
        "<h2>A degree warmer</h2>",
        "<h2>Annual benchmark</h2>",
    ] {
        assert!(
            html.contains(&format!("{heading}<p class=\"waiting\">")),
            "{heading}"
        );
    }
    assert!(html.contains("It needs every room's floor area in the rooms file"));
    assert!(
        html.contains("<h2>Heating energy</h2><p>"),
        "drawn, not waiting"
    );
}

/// The note a run with rooms but no heating energy ends on.
const WAITING_WITHOUT_ENERGY: &str = "note: Heating energy, Building or behaviour, A degree \
     warmer and Annual benchmark wait for data - --energy-entity and the rooms' floor areas in \
     the rooms file; the report says what each needs";

/// The lines of a run that are notes.
fn notes(stdout: &str) -> Vec<&str> {
    stdout
        .lines()
        .filter(|line| line.starts_with("note:"))
        .collect()
}
