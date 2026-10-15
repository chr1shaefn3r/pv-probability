//! Reading a copy of a Home Assistant recorder database, for every tool in the workspace.
//!
//! 1. [`source`] finds an entity in whichever schema generation the database uses and
//!    reads it as weighted [`Sample`]s, each one a reading and how long it held.
//! 2. [`coverage`] says what that history really covers: the days recorded and the
//!    outages between them.
//! 3. [`timeutil`] turns UTC timestamps into local hours, days and weeks, DST included.
//!
//! [`RecorderArgs`] is the command line surface for all of it, shared by every tool.
//!
//! Nothing here knows what question a tool answers. Changing this crate rebuilds every
//! tool, so code that serves one tool belongs in that tool's crate.

pub mod args;
pub mod coverage;
pub mod model;
pub mod source;
pub mod timeutil;

pub use args::{RecorderArgs, RecorderConfig};
pub use model::{Grouping, Sample};
