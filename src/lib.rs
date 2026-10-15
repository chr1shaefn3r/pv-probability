//! Turn a Home Assistant recorder database into "flame graph" heatmaps of how much
//! photovoltaic power is likely to be available at a given hour of the day.
//!
//! The pipeline is:
//!
//! 1. [`ha_recorder::source`] reads weighted [`ha_recorder::Sample`]s out of a copy of
//!    `home-assistant_v2.db`.
//! 2. [`aggregate`] folds those samples into a [`model::Grid`] (facet × hour × watt bucket)
//!    in parallel with rayon, then turns the accumulated weights into probabilities.
//! 3. [`render`] writes the probabilities out as a single self-contained HTML file.
//!
//! [`storage`] answers a second question from the same database - how long a home battery
//! would take to pay for itself - reusing [`ha_recorder`] and [`render`].

pub mod aggregate;
pub mod cli;
pub mod model;
pub mod render;
pub mod storage;

pub use model::{BucketSpec, Grid};
