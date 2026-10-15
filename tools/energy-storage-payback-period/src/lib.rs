//! How long a home battery would take to pay for itself, worked out from the grid import
//! and export power sensors a Home Assistant recorder database already holds.
//!
//! The pipeline mirrors the heatmap tool's:
//!
//! 1. [`ha_recorder::source`] loads the two grid power sensors as weighted samples.
//! 2. [`series`] integrates each into energy per time slot, and [`pair`] keeps the slots
//!    both sensors really covered.
//! 3. [`simulate`] replays one battery over that timeline, and [`sweep`] does it for every
//!    size at once, in parallel, then prices the result.
//! 4. [`report`] writes the answer out as a self-contained HTML page, in the shell every
//!    report shares.

pub mod cli;
pub mod pair;
pub mod report;
pub mod series;
pub mod simulate;
pub mod sweep;
