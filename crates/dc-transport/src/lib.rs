//! Transport abstractions. QUIC/TLS support is implemented in stage 2.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConnectionState {
    Disconnected,
    Connecting,
    Authenticating,
    Connected,
    Closing,
}
