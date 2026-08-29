//! Turn a Home Assistant recorder database into "flame graph" heatmaps of how much
//! photovoltaic power is likely to be available at a given hour of the day.
//!
//! The pipeline is:
//!
//! 1. [`source`] reads weighted [`model::Sample`]s out of a copy of `home-assistant_v2.db`.
//! 2. [`aggregate`] folds those samples into a [`model::Grid`] (facet × hour × watt bucket)
//!    in parallel with rayon, then turns the accumulated weights into probabilities.
//! 3. [`render`] writes the probabilities out as a single self-contained HTML file.
//!
//! [`storage`] answers a second question from the same database - how long a home battery
//! would take to pay for itself - reusing [`source`], [`coverage`] and [`render`].

pub mod aggregate;
pub mod cli;
pub mod coverage;
pub mod model;
pub mod render;
pub mod source;
pub mod storage;
pub mod timeutil;

pub use model::{BucketSpec, Grid, Grouping, Sample};
