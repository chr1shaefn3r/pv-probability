# Home Assistant energy analyzer

[![CI](https://github.com/chr1shaefn3r/pv-probability/actions/workflows/ci.yml/badge.svg)](https://github.com/chr1shaefn3r/pv-probability/actions/workflows/ci.yml)

Command line tools that read a copy of your Home Assistant recorder database and each
answer one question about your home's energy as a self-contained HTML report:

| Tool | Question |
|---|---|
| **[`pv-probability`](tools/pv-probability/README.md)** | How much solar power can I count on, hour by hour and month by month? |
| **[`energy-storage-payback-period`](tools/energy-storage-payback-period/README.md)** | How long would a home battery take to pay for itself? |
| **[`heating-energy`](tools/heating-energy/README.md)** | How much heating did the weather ask for, and how warm was every room kept? |

![Monthly likelihood heatmaps from pv-probability](tools/pv-probability/docs/example-light.png)

Every report is one HTML file: no JavaScript, no web fonts, no network access. It follows
your system's light/dark setting, with an Auto / Light / Dark switch in the header to
override it, and ends with a **History** block that says what the recorder really
covered - how many days, how many outages, the worst of them - because every figure
above it is conditional on the time that was actually recorded.

Nothing is ever sent anywhere: the tools are local CLIs that read SQLite and write HTML.

## Quick start

```sh
# 1. Build every tool
cargo build --release

# 2. Take a copy of the recorder database (see "Getting the database" below)
scp homeassistant.local:/config/home-assistant_v2.db ha.db

# 3. Ask a question
./target/release/pv-probability --db ha.db --entity sensor.solar_power --tz Europe/Berlin
./target/release/energy-storage-payback-period --db ha.db --tz Europe/Berlin \
  --import-entity sensor.grid_import_power --export-entity sensor.grid_export_power
./target/release/heating-energy --db ha.db --tz Europe/Berlin \
  --outside-entity sensor.outdoor_temperature --rooms rooms.toml
```

Each finishes by printing the command that opens its report, ready to paste.

No Home Assistant to hand? Generate a synthetic one, either solid or full of holes. It
carries a solar power sensor, an energy counter, a pair of grid meters, an outdoor
thermometer and one in each room of a two-storey house, so every tool has something to
read:

```sh
cargo run --release -p ha-testdb --example demo_database -- demo.db 730 dense
cargo run --release -p ha-testdb --example demo_database -- demo.db 165 spotty  # 5 months, outages
cargo run --release -p pv-probability -- --db demo.db --entity sensor.solar_power \
  --tz Europe/Berlin
cargo run --release -p energy-storage-payback-period -- --db demo.db \
  --import-entity sensor.grid_import_power --export-entity sensor.grid_export_power
cargo run --release -p heating-energy -- --db demo.db \
  --outside-entity sensor.outdoor_temperature --rooms tools/heating-energy/rooms.example.toml
```

## Finding the right entity

The tools read **measurements**: a power sensor for the solar and battery tools, a
thermometer for the heating one. Home Assistant usually exposes **two** sensors per
metered device, and only one of them is any use here:

| Sensor | Unit | `state_class` | What it holds | Readable |
|---|---|---|---|---|
| `sensor.…_power` | `W` / `kW` | `measurement` | watts right now | **yes**, as power |
| `sensor.…_temperature` | `°C` / `°F` | `measurement` | degrees right now | **yes**, as temperature |
| `sensor.…_energy` | `kWh` | `total_increasing` | a running total | no |

The recorder stores them differently: a measurement gets an hourly `mean`, a counter gets a
`sum` and no mean at all, because the average of a running total means nothing. Point a
tool at a counter and there is nothing for it to read - it says so, and names the power
sensor to use instead. Point it at a sensor of the wrong quantity - a thermometer where
power is wanted - and it says that too, with the sensors that would fit.

To see what your database holds:

```sh
pv-probability --db ha.db --list-entities          # everything, power sensors first
pv-probability --db ha.db --list-entities solar    # only ids containing "solar"
```

```
Power sensors these tools can read:
  sensor.solar_power          W     3408 rows  2023-01-01 to 2023-06-15

Temperature sensors these tools can read:
  sensor.outdoor_temperature  °C    3408 rows  2023-01-01 to 2023-06-15

Energy counters (cumulative totals - not plottable, see the README):
  sensor.solar_energy         kWh   3408 rows  2023-01-01 to 2023-06-15
```

No entity is needed when listing. Every error about an entity - unknown, empty, or the
wrong kind - ends with the same shortlist, so the next command is always in front of you.

## Getting the database

The tools only ever open the file **read-only**, but SQLite still needs a consistent file.
Home Assistant writes continuously, and the recorder runs in WAL mode, so a plain `cp` of
`home-assistant_v2.db` on a running system can miss everything still sitting in the
`-wal` sidecar.

The safe way, from an SSH or Terminal add-on on the Home Assistant machine:

```sh
sqlite3 /config/home-assistant_v2.db "VACUUM INTO '/config/pv-copy.db'"
```

Then copy `pv-copy.db` off the box (Samba share, `scp`, the File Editor add-on) and point
`--db` at it. On a Home Assistant Green the database lives at `/config/home-assistant_v2.db`
and is typically a few hundred MB.

`VACUUM INTO` rather than `.backup`: both give a consistent copy including whatever is
still in the `-wal`, but `.backup` copies the WAL *mode* along with the data, and every
later reader of a WAL database has to create a `-shm` and a `-wal` beside it. A
`VACUUM INTO` copy is a plain rollback-journal database, so nothing ever appears next to
it - and it is smaller, because vacuuming rewrites it without the free pages.

If you would rather copy the raw file, stop Home Assistant first, or copy
`home-assistant_v2.db`, `home-assistant_v2.db-wal` and `home-assistant_v2.db-shm`
together.

### Why no `-shm` and `-wal` appear next to your copy

A read-only connection to a WAL database has to build SQLite's log index, and - unlike a
writer - it cannot run the checkpoint that removes the index again when it closes. So the
naive thing to do would leave a `-shm` and a `-wal` behind after every run.

Instead, when no `-wal` or `-journal` sits beside the database, the tools tell SQLite the
file is immutable: it then needs no index and takes no locks, and creates nothing. When a
journal *is* there, they fall back to an ordinary read-only open, because `immutable`
would silently skip the rows that are still in it - the sidecars stay, but they were
already there. `-v` says which of the two happened.

## Which data the tools read

Home Assistant keeps three different records of a sensor, and they have very different
retention. `--source auto` (the default) picks the one with the most history:

| Source | Table | Resolution | Retention | Good for |
|---|---|---|---|---|
| `statistics` | `statistics` | one row per hour (`mean`/`min`/`max`) | years | **month and week views** |
| `short-term` | `statistics_short_term` | one row per 5 minutes | ~10 days | recent detail |
| `states` | `states` | every reported change | ~10 days (`purge_keep_days`) | recent detail |

Because the recorder purges raw states after ten days by default, a full year of history
practically has to come from the hourly `statistics` table. That table stores the **mean**
of each hour, so short cloud-edge spikes are smoothed away; `--stat max` shows the peak
within each hour instead, and `--stat min` the floor.

Both the modern schema (epoch timestamps, `states_meta`) and the pre-2023 schema (text
timestamps, `states.entity_id`) are detected automatically.

If the entity name is wrong - or right but unusable - the error says which, and lists the
power sensors that would work. See [Finding the right entity](#finding-the-right-entity).

## Shared options

Every tool takes the same options for reading the database. `--help` lists them under
**Reading the database**, after the tool's own:

| Option | Default | What it does |
|---|---|---|
| `--db <FILE>` | required | Copy of `home-assistant_v2.db`, opened read-only |
| `--list-entities [FILTER]` | off | List what the database holds and exit, power sensors first |
| `--source <auto\|statistics\|short-term\|states>` | `auto` | Which recorder table to read |
| `--stat <mean\|min\|max>` | `mean` | Which statistics column to use |
| `--tz <TZ>` | machine zone | IANA timezone for hours and dates, e.g. `Europe/Berlin` |
| `--from <YYYY-MM-DD>` / `--to <YYYY-MM-DD>` | all data | Local date window, `--to` exclusive |
| `--max-gap <SECONDS>` | `900` | How long one raw state reading is assumed to hold |
| `--scale <FACTOR>` | auto | Multiply readings; `kW`/`MW` and `°F`/`K` are converted automatically |
| `--gap-threshold-hours <H>` | `24` | Outages at least this long are reported |

Each tool also takes `--threads <N>` to cap its rayon worker threads, `-o` for where to
write the report, and `-v` for timings and row counts.

## Development

```sh
cargo test --workspace                            # 320+ unit and integration tests
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo fmt --all
cargo test -p pv-probability                      # one tool and what it depends on
```

The test suite never touches a real Home Assistant: `crates/ha-testdb` builds
synthetic recorder databases in both schema generations, including reproducible
synthetic histories - with or without outages - for each tool's end-to-end test.
GitHub Actions runs formatting, clippy, the whole suite and a release build on every push
to `main` and on every pull request (`.github/workflows/ci.yml`).

### Layout

The repository is a Cargo workspace with one crate per tool, so a change to one tool
rebuilds that tool alone:

```
crates/
  ha-recorder/   reading the database: schema probing, the statistics / states readers,
                 the entity catalogue, outage coverage, DST-safe time, shared options
  ha-report/     the page every report shares: HTML shell, light and dark palettes,
                 colour scales, number formatting
  ha-testdb/     synthetic recorder databases for the tests (dev-only), and the demo
tools/
  pv-probability/                   the heatmap tool
  energy-storage-payback-period/    the battery payback tool
  heating-energy/                   the degree day tool
```

Dependencies only point one way: each tool depends on `ha-report` and `ha-recorder`,
`ha-report` on `ha-recorder`, and `ha-testdb` on nothing in the workspace. A tool's own
code therefore never makes another tool recompile; on four cores, touching one file in a
tool rebuilds just that tool, in about 13 s for a release build (down from 25 s when both
tools were one crate) and about 1.3 s for a debug one. A change to a shared crate still
rebuilds every tool, which is why the shared crates hold only what does not care which
question is asked.

Every crate takes its dependencies from `[workspace.dependencies]` in the root
`Cargo.toml`, so a single tool built with `-p` resolves the same features as the whole
workspace and never forces the shared dependencies to rebuild.

### Adding a tool

1. `tools/<name>/` with a `Cargo.toml` that takes its dependencies with
   `.workspace = true`, a `src/lib.rs` for the logic and a thin `src/main.rs`. The
   workspace's `tools/*` glob picks it up; the root `Cargo.toml` needs no edit.
2. Flatten `ha_recorder::RecorderArgs` into its clap arguments: that brings every shared
   option, its validation, `--list-entities` and the loader options. Tell
   `load_options` which `ha_recorder::Quantity` each sensor measures - power or
   temperature - and the readers convert its unit and suggest the right sensors.
3. Wrap the report in `ha_report::layout::document`, passing the colour scales it draws
   with to `theme_stylesheet`, and end it with `coverage_section`.
4. Test it end to end against a database from `ha-testdb`, adding whatever sensors it
   needs there, and give it a `README.md` linked from the table at the top of this one.

## Licence

MIT - see [LICENSE](LICENSE).
