//! Shared session lifecycle types.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionState {
    Created,
    Negotiating,
    Active,
    Closing,
    Closed,
}
