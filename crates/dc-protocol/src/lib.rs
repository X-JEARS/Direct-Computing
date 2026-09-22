//! Versioned protocol primitives shared by hosts, viewers, and CLI clients.

pub const PROTOCOL_VERSION: ProtocolVersion = ProtocolVersion { major: 0, minor: 1 };

/// Maximum encoded protocol message accepted from a peer.  Keeping this limit at the
/// protocol boundary prevents a malformed length field from turning into an allocation
/// request controlled by the remote endpoint.
pub const MAX_MESSAGE_SIZE: usize = 16 * 1024 * 1024;

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
    pub control_input: bool,
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InputEvent {
    Pointer { x: i32, y: i32, buttons: u8 },
    Key { code: u32, pressed: bool },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WireMessage {
    Hello(Hello),
    Challenge {
        nonce: [u8; 32],
        salt: String,
    },
    Authenticate {
        proof: [u8; 32],
    },
    Authenticated {
        permissions: Capabilities,
    },
    Rejected {
        reason: String,
    },
    OpenDesktop,
    Video {
        codec: u8,
        sequence: u64,
        timestamp_millis: u64,
        keyframe: bool,
        width: u32,
        height: u32,
        pixel_format: u8,
        stride: u32,
        data: Vec<u8>,
    },
    Input(InputEvent),
    RateHint {
        bitrate: u32,
        frames_per_second: u16,
    },
    /// Ask the encoder to emit a fresh intra frame after a lossy media gap.
    KeyframeRequest {
        last_sequence: u64,
    },
    FileOffer {
        transfer_id: [u8; 16],
        name: String,
        size: u64,
        chunk_size: u32,
        sha256: [u8; 32],
    },
    FileChunk {
        transfer_id: [u8; 16],
        offset: u64,
        data: Vec<u8>,
        sha256: [u8; 32],
    },
    FileAck {
        transfer_id: [u8; 16],
        next_offset: u64,
    },
    Close,
}

impl WireMessage {
    /// Encode one message without a transport-specific framing header.
    pub fn encode(&self) -> dc_common::Result<Vec<u8>> {
        let mut output = Vec::new();
        match self {
            Self::Hello(hello) => {
                output.push(1);
                put_u16(&mut output, hello.protocol_version.major);
                put_u16(&mut output, hello.protocol_version.minor);
                put_capabilities(&mut output, hello.capabilities);
            }
            Self::Challenge { nonce, salt } => {
                output.push(2);
                output.extend_from_slice(nonce);
                // Challenge salts are public and allow a client to derive the same verifier.
                // They are still length-delimited to keep the decoder allocation-bounded.
                put_string(&mut output, salt)?;
            }
            Self::Authenticate { proof } => {
                output.push(3);
                output.extend_from_slice(proof);
            }
            Self::Authenticated { permissions } => {
                output.push(4);
                put_capabilities(&mut output, *permissions);
            }
            Self::Rejected { reason } => {
                output.push(5);
                put_string(&mut output, reason)?;
            }
            Self::OpenDesktop => output.push(6),
            Self::Video {
                codec,
                sequence,
                timestamp_millis,
                keyframe,
                width,
                height,
                pixel_format,
                stride,
                data,
            } => {
                output.push(7);
                output.push(*codec);
                put_u64(&mut output, *sequence);
                put_u64(&mut output, *timestamp_millis);
                output.push(u8::from(*keyframe));
                put_u32(&mut output, *width);
                put_u32(&mut output, *height);
                output.push(*pixel_format);
                put_u32(&mut output, *stride);
                put_bytes(&mut output, data)?;
            }
            Self::Input(event) => {
                output.push(8);
                match event {
                    InputEvent::Pointer { x, y, buttons } => {
                        output.push(1);
                        put_i32(&mut output, *x);
                        put_i32(&mut output, *y);
                        output.push(*buttons);
                    }
                    InputEvent::Key { code, pressed } => {
                        output.push(2);
                        put_u32(&mut output, *code);
                        output.push(u8::from(*pressed));
                    }
                }
            }
            Self::RateHint {
                bitrate,
                frames_per_second,
            } => {
                output.push(9);
                put_u32(&mut output, *bitrate);
                put_u16(&mut output, *frames_per_second);
            }
            Self::KeyframeRequest { last_sequence } => {
                output.push(14);
                put_u64(&mut output, *last_sequence);
            }
            Self::FileOffer {
                transfer_id,
                name,
                size,
                chunk_size,
                sha256,
            } => {
                output.push(10);
                output.extend_from_slice(transfer_id);
                put_string(&mut output, name)?;
                put_u64(&mut output, *size);
                put_u32(&mut output, *chunk_size);
                output.extend_from_slice(sha256);
            }
            Self::FileChunk {
                transfer_id,
                offset,
                data,
                sha256,
            } => {
                output.push(11);
                output.extend_from_slice(transfer_id);
                put_u64(&mut output, *offset);
                put_bytes(&mut output, data)?;
                output.extend_from_slice(sha256);
            }
            Self::FileAck {
                transfer_id,
                next_offset,
            } => {
                output.push(12);
                output.extend_from_slice(transfer_id);
                put_u64(&mut output, *next_offset);
            }
            Self::Close => output.push(13),
        }
        if output.len() > MAX_MESSAGE_SIZE {
            return Err(dc_common::DcError::InvalidInput(
                "protocol message exceeds size limit".into(),
            ));
        }
        Ok(output)
    }

    pub fn decode(input: &[u8]) -> dc_common::Result<Self> {
        if input.is_empty() || input.len() > MAX_MESSAGE_SIZE {
            return Err(dc_common::DcError::Codec(
                "invalid protocol message length".into(),
            ));
        }
        let mut reader = Reader { input, offset: 1 };
        let kind = input[0];
        let message = match kind {
            1 => Self::Hello(Hello {
                protocol_version: ProtocolVersion {
                    major: reader.u16()?,
                    minor: reader.u16()?,
                },
                capabilities: reader.capabilities()?,
            }),
            2 => Self::Challenge {
                nonce: reader.array_32()?,
                salt: reader.string()?,
            },
            3 => Self::Authenticate {
                proof: reader.array_32()?,
            },
            4 => Self::Authenticated {
                permissions: reader.capabilities()?,
            },
            5 => Self::Rejected {
                reason: reader.string()?,
            },
            6 => Self::OpenDesktop,
            7 => Self::Video {
                codec: reader.u8()?,
                sequence: reader.u64()?,
                timestamp_millis: reader.u64()?,
                keyframe: reader.u8()? != 0,
                width: reader.u32()?,
                height: reader.u32()?,
                pixel_format: reader.u8()?,
                stride: reader.u32()?,
                data: reader.bytes()?,
            },
            8 => match reader.u8()? {
                1 => Self::Input(InputEvent::Pointer {
                    x: reader.i32()?,
                    y: reader.i32()?,
                    buttons: reader.u8()?,
                }),
                2 => Self::Input(InputEvent::Key {
                    code: reader.u32()?,
                    pressed: reader.u8()? != 0,
                }),
                _ => return Err(dc_common::DcError::Codec("unknown input event".into())),
            },
            9 => Self::RateHint {
                bitrate: reader.u32()?,
                frames_per_second: reader.u16()?,
            },
            14 => Self::KeyframeRequest {
                last_sequence: reader.u64()?,
            },
            10 => Self::FileOffer {
                transfer_id: reader.array_16()?,
                name: reader.string()?,
                size: reader.u64()?,
                chunk_size: reader.u32()?,
                sha256: reader.array_32()?,
            },
            11 => Self::FileChunk {
                transfer_id: reader.array_16()?,
                offset: reader.u64()?,
                data: reader.bytes()?,
                sha256: reader.array_32()?,
            },
            12 => Self::FileAck {
                transfer_id: reader.array_16()?,
                next_offset: reader.u64()?,
            },
            13 => Self::Close,
            _ => {
                return Err(dc_common::DcError::Codec(format!(
                    "unknown protocol message type {kind}"
                )))
            }
        };
        if reader.offset != input.len() {
            return Err(dc_common::DcError::Codec(
                "trailing bytes in protocol message".into(),
            ));
        }
        Ok(message)
    }
}

fn put_u16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_be_bytes());
}
fn put_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_be_bytes());
}
fn put_u64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_be_bytes());
}
fn put_i32(out: &mut Vec<u8>, value: i32) {
    out.extend_from_slice(&value.to_be_bytes());
}
fn put_capabilities(out: &mut Vec<u8>, value: Capabilities) {
    out.push(
        u8::from(value.desktop)
            | (u8::from(value.terminal) << 1)
            | (u8::from(value.command_execution) << 2)
            | (u8::from(value.file_transfer) << 3)
            | (u8::from(value.clipboard) << 4)
            | (u8::from(value.ssh_compatibility) << 5)
            | (u8::from(value.control_input) << 6),
    );
}
fn put_bytes(out: &mut Vec<u8>, value: &[u8]) -> dc_common::Result<()> {
    let length = u32::try_from(value.len())
        .map_err(|_| dc_common::DcError::InvalidInput("payload is too large".into()))?;
    put_u32(out, length);
    out.extend_from_slice(value);
    Ok(())
}
fn put_string(out: &mut Vec<u8>, value: &str) -> dc_common::Result<()> {
    put_bytes(out, value.as_bytes())
}

struct Reader<'a> {
    input: &'a [u8],
    offset: usize,
}
impl<'a> Reader<'a> {
    fn take(&mut self, count: usize) -> dc_common::Result<&'a [u8]> {
        let end = self
            .offset
            .checked_add(count)
            .ok_or_else(|| dc_common::DcError::Codec("message offset overflow".into()))?;
        let value = self
            .input
            .get(self.offset..end)
            .ok_or_else(|| dc_common::DcError::Codec("truncated protocol message".into()))?;
        self.offset = end;
        Ok(value)
    }
    fn u8(&mut self) -> dc_common::Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> dc_common::Result<u16> {
        Ok(u16::from_be_bytes(self.take(2)?.try_into().unwrap()))
    }
    fn u32(&mut self) -> dc_common::Result<u32> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> dc_common::Result<u64> {
        Ok(u64::from_be_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn i32(&mut self) -> dc_common::Result<i32> {
        Ok(i32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn array_32(&mut self) -> dc_common::Result<[u8; 32]> {
        Ok(self.take(32)?.try_into().unwrap())
    }
    fn array_16(&mut self) -> dc_common::Result<[u8; 16]> {
        Ok(self.take(16)?.try_into().unwrap())
    }
    fn bytes(&mut self) -> dc_common::Result<Vec<u8>> {
        let len = self.u32()? as usize;
        Ok(self.take(len)?.to_vec())
    }
    fn string(&mut self) -> dc_common::Result<String> {
        String::from_utf8(self.bytes()?)
            .map_err(|_| dc_common::DcError::Codec("invalid UTF-8 string".into()))
    }
    fn capabilities(&mut self) -> dc_common::Result<Capabilities> {
        let flags = self.u8()?;
        Ok(Capabilities {
            desktop: flags & 1 != 0,
            control_input: flags & 64 != 0,
            terminal: flags & 2 != 0,
            command_execution: flags & 4 != 0,
            file_transfer: flags & 8 != 0,
            clipboard: flags & 16 != 0,
            ssh_compatibility: flags & 32 != 0,
        })
    }
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

    #[test]
    fn wire_messages_round_trip_with_binary_framing() {
        let message = WireMessage::Video {
            codec: 1,
            sequence: 42,
            timestamp_millis: 900,
            keyframe: true,
            width: 640,
            height: 480,
            pixel_format: 2,
            stride: 2560,
            data: vec![1, 2, 3, 4],
        };
        let encoded = message.encode().unwrap();
        assert_eq!(WireMessage::decode(&encoded).unwrap(), message);
    }

    #[test]
    fn wire_messages_reject_trailing_data() {
        let mut encoded = WireMessage::Close.encode().unwrap();
        encoded.push(0);
        assert!(WireMessage::decode(&encoded).is_err());
    }

    #[test]
    fn keyframe_requests_round_trip() {
        let message = WireMessage::KeyframeRequest { last_sequence: 99 };
        assert_eq!(
            WireMessage::decode(&message.encode().unwrap()).unwrap(),
            message
        );
    }

    #[test]
    fn file_messages_round_trip_with_checksums() {
        let message = WireMessage::FileChunk {
            transfer_id: [7; 16],
            offset: 256,
            data: vec![1, 2, 3],
            sha256: [9; 32],
        };
        assert_eq!(
            WireMessage::decode(&message.encode().unwrap()).unwrap(),
            message
        );
    }

    #[test]
    fn capability_round_trip_preserves_control_permission() {
        let message = WireMessage::Authenticated {
            permissions: Capabilities {
                desktop: true,
                control_input: true,
                ..Capabilities::default()
            },
        };
        assert_eq!(
            WireMessage::decode(&message.encode().unwrap()).unwrap(),
            message
        );
    }
}
