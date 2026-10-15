//! Turning an [`crate::aggregate::Analysis`] into a self-contained HTML page, inside the
//! shell every report shares (`ha_report::layout`).

pub mod html;
pub mod svg;

pub use html::{PageOptions, describe_window, page};
