//! Versioned protocol primitives shared by hosts, viewers, and CLI clients.

pub const PROTOCOL_VERSION: ProtocolVersion = ProtocolVersion { major: 0, minor: 1 };

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct ProtocolVersion {
    pub major: u16,
    pub minor: u16,
}

impl ProtocolVersion {
    pub const fn compatible_with(self, other: Self) -> bool {
        self.major == other.major
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ServiceKind {
    Desktop,
    Terminal,
    Command,
    FileTransfer,
    Clipboard,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Capabilities {
    pub desktop: bool,
    pub terminal: bool,
    pub command_execution: bool,
    pub file_transfer: bool,
    pub clipboard: bool,
    pub ssh_compatibility: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Hello {
    pub protocol_version: ProtocolVersion,
    pub capabilities: Capabilities,
}

impl Default for Hello {
    fn default() -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            capabilities: Capabilities::default(),
        }
    }
}

pub fn validate_version(version: ProtocolVersion) -> dc_common::Result<()> {
    if PROTOCOL_VERSION.compatible_with(version) {
        Ok(())
    } else {
        Err(dc_common::DcError::Unsupported(format!(
            "protocol version {}.{} (expected major {})",
            version.major, version.minor, PROTOCOL_VERSION.major
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compatible_versions_share_a_major_version() {
        assert!(PROTOCOL_VERSION.compatible_with(ProtocolVersion {
            major: 0,
            minor: 99
        }));
        assert!(!PROTOCOL_VERSION.compatible_with(ProtocolVersion { major: 1, minor: 0 }));
    }
}
