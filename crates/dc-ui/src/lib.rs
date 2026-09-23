//! Shared UI state and presentation abstractions.

mod preview;

pub use preview::{PreviewWindowSink, DIRTY_REGION_DEBUG_HOLD};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ApplicationRole {
    Host,
    Viewer,
}
