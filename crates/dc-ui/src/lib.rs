//! Shared UI state and presentation abstractions.

mod preview;

pub use preview::PreviewWindowSink;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ApplicationRole {
    Host,
    Viewer,
}
