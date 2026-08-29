//! Working out what a home battery would have been worth.
//!
//! The pipeline mirrors the heatmap tool's:
//!
//! 1. [`crate::source`] loads the two grid power sensors as weighted samples.
//! 2. [`series`] integrates each into energy per time slot, and [`pair`] keeps the slots
//!    both sensors really covered.
//! 3. [`simulate`] replays one battery over that timeline, and [`sweep`] does it for every
//!    size at once, in parallel, then prices the result.

pub mod cli;
pub mod pair;
pub mod series;
pub mod simulate;
pub mod sweep;
