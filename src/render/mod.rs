//! Turning an [`crate::aggregate::Analysis`] into a self-contained HTML page, inside the
//! shell every report shares (`ha_report::layout`).

pub mod html;
pub mod payback;
pub mod svg;

pub use html::{PageOptions, describe_window, page};
pub use payback::{PaybackOptions, page as payback_page};
