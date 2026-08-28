//! Write a synthetic Home Assistant recorder database, so the tool can be tried out
//! without touching a real instance.
//!
//! ```text
//! cargo run --release --example demo_database -- demo.db 730 dense
//! cargo run --release --example demo_database -- demo.db 200 spotty
//! cargo run --release -- --db demo.db --entity sensor.solar_power --tz Europe/Berlin
//! ```

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use rusqlite::Connection;

use pv_probability::source::testdb::{self, ENTITY};

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let path = PathBuf::from(args.next().unwrap_or_else(|| "demo.db".to_string()));
    let days: i64 = args
        .next()
        .unwrap_or_else(|| "730".to_string())
        .parse()
        .context("the second argument is the number of days to generate")?;

    let outages = match args.next().as_deref().unwrap_or("dense") {
        "dense" => testdb::Outages::none(),
        "spotty" => testdb::Outages::spotty(days),
        other => bail!("the third argument is `dense` or `spotty`, not {other:?}"),
    };

    if path.exists() {
        std::fs::remove_file(&path)
            .with_context(|| format!("failed to replace {}", path.display()))?;
    }
    let conn =
        Connection::open(&path).with_context(|| format!("failed to create {}", path.display()))?;
    testdb::create_schema(&conn, testdb::Flavour::Modern);
    let start = testdb::ts("2023-01-01 00:00:00");
    testdb::insert_synthetic_history(&conn, start, days, 0xC0FFEE, &outages);
    // A real integration exposes both, and picking the wrong one is the easiest mistake
    // to make, so the demo database offers the same trap.
    testdb::insert_synthetic_energy_counter(
        &conn,
        "sensor.solar_energy",
        start,
        days,
        0xC0FFEE,
        &outages,
    );

    println!(
        "wrote {} with {} of {days} days of hourly statistics for {ENTITY} \
         (and a cumulative sensor.solar_energy counter beside it)",
        path.display(),
        outages.covered_days(days)
    );
    Ok(())
}
