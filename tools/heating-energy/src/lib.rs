//! Heating and cooling degree days from a Home Assistant recorder database, beside the
//! indoor temperature of every room.
//!
//! 1. [`ha_recorder::source`] loads the outdoor thermometer and every room's, in °C.
//! 2. [`daily`] turns each into one time-weighted mean per local day, and says how much
//!    of the day stands behind it.
//! 3. [`degree`] counts heating and cooling degree days from the outside means, sums them
//!    per month or week and per year, and averages each room and floor over the same days.
//! 4. [`report`] writes it all out as a self-contained HTML page, in the shell every
//!    report shares.
//!
//! [`rooms`] reads the house - its floors, rooms and thermometers - from a TOML file.

pub mod cli;
pub mod daily;
pub mod degree;
pub mod report;
pub mod rooms;
