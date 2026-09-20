//! Shared UI state and presentation abstractions.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ApplicationRole {
    Host,
    Viewer,
}
