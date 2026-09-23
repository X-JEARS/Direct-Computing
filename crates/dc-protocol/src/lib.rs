//! Versioned protocol primitives shared by hosts, viewers, and CLI clients.

pub const PROTOCOL_VERSION: ProtocolVersion = ProtocolVersion { major: 2, minor: 0 };

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
    /// Supports reliable keyframes plus QUIC DATAGRAM inter frames.
    pub hybrid_video: bool,
    /// Supports dirty-region updates and a separately composited pointer.
    pub desktop_optimizations: bool,
    /// Supports H.264 NAL/slice-oriented QUIC Datagram packetization.
    pub h264_nal_datagrams: bool,
}

pub const MAX_DESKTOP_REGIONS: usize = 256;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DesktopRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CursorShape {
    pub kind: u8,
    pub width: u32,
    pub height: u32,
    pub hotspot_x: u32,
    pub hotspot_y: u32,
    pub pitch: u32,
    pub data: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Hello {
    pub protocol_version: ProtocolVersion,
    pub capabilities: Capabilities,
}

macro_rules! keyboard_keys {
    ($($name:ident = $value:literal),+ $(,)?) => {
        #[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
        #[repr(u16)]
        pub enum KeyboardKey {
            $($name = $value),+
        }

        impl KeyboardKey {
            pub const fn wire_code(self) -> u16 {
                self as u16
            }

            fn from_wire(value: u16) -> dc_common::Result<Self> {
                match value {
                    $($value => Ok(Self::$name),)+
                    _ => Err(dc_common::DcError::Codec(format!(
                        "unknown keyboard key {value}"
                    ))),
                }
            }
        }
    };
}

keyboard_keys! {
    Digit0 = 0x0001, Digit1 = 0x0002, Digit2 = 0x0003, Digit3 = 0x0004,
    Digit4 = 0x0005, Digit5 = 0x0006, Digit6 = 0x0007, Digit7 = 0x0008,
    Digit8 = 0x0009, Digit9 = 0x000a,
    A = 0x0010, B = 0x0011, C = 0x0012, D = 0x0013, E = 0x0014,
    F = 0x0015, G = 0x0016, H = 0x0017, I = 0x0018, J = 0x0019,
    K = 0x001a, L = 0x001b, M = 0x001c, N = 0x001d, O = 0x001e,
    P = 0x001f, Q = 0x0020, R = 0x0021, S = 0x0022, T = 0x0023,
    U = 0x0024, V = 0x0025, W = 0x0026, X = 0x0027, Y = 0x0028,
    Z = 0x0029,
    F1 = 0x0030, F2 = 0x0031, F3 = 0x0032, F4 = 0x0033, F5 = 0x0034,
    F6 = 0x0035, F7 = 0x0036, F8 = 0x0037, F9 = 0x0038, F10 = 0x0039,
    F11 = 0x003a, F12 = 0x003b, F13 = 0x003c, F14 = 0x003d, F15 = 0x003e,
    ArrowDown = 0x0040, ArrowLeft = 0x0041, ArrowRight = 0x0042,
    ArrowUp = 0x0043, Apostrophe = 0x0044, Backquote = 0x0045,
    Backslash = 0x0046, Comma = 0x0047, Equal = 0x0048, LeftBracket = 0x0049,
    Minus = 0x004a, Period = 0x004b, RightBracket = 0x004c,
    Semicolon = 0x004d, Slash = 0x004e,
    Backspace = 0x0050, Delete = 0x0051, End = 0x0052, Enter = 0x0053,
    Escape = 0x0054, Home = 0x0055, Insert = 0x0056, ContextMenu = 0x0057,
    PageDown = 0x0058, PageUp = 0x0059, Pause = 0x005a, Space = 0x005b,
    Tab = 0x005c, NumLock = 0x005d, CapsLock = 0x005e, ScrollLock = 0x005f,
    LeftShift = 0x0060, RightShift = 0x0061, LeftControl = 0x0062,
    RightControl = 0x0063,
    Numpad0 = 0x0070, Numpad1 = 0x0071, Numpad2 = 0x0072, Numpad3 = 0x0073,
    Numpad4 = 0x0074, Numpad5 = 0x0075, Numpad6 = 0x0076, Numpad7 = 0x0077,
    Numpad8 = 0x0078, Numpad9 = 0x0079, NumpadDecimal = 0x007a,
    NumpadDivide = 0x007b, NumpadMultiply = 0x007c, NumpadSubtract = 0x007d,
    NumpadAdd = 0x007e, NumpadEnter = 0x007f,
    LeftAlt = 0x0080, RightAlt = 0x0081, LeftSuper = 0x0082,
    RightSuper = 0x0083,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InputEvent {
    Pointer { x: i32, y: i32, buttons: u8 },
    Wheel { delta_x: i32, delta_y: i32 },
    Key { key: KeyboardKey, pressed: bool },
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
    /// Confirms that a reliable recovery anchor was decoded and presented.
    /// The Host uses this to avoid queuing duplicate keyframes on a narrow link.
    KeyframeAck {
        sequence: u64,
    },
    DesktopUpdate {
        sequence: u64,
        timestamp_millis: u64,
        desktop_width: u32,
        desktop_height: u32,
        encoding: u8,
        regions: Vec<DesktopRect>,
        data: Vec<u8>,
    },
    Cursor {
        sequence: u64,
        visible: bool,
        x: i32,
        y: i32,
        shape: Option<CursorShape>,
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
                    InputEvent::Key { key, pressed } => {
                        output.push(2);
                        put_u16(&mut output, key.wire_code());
                        output.push(u8::from(*pressed));
                    }
                    InputEvent::Wheel { delta_x, delta_y } => {
                        output.push(3);
                        put_i32(&mut output, *delta_x);
                        put_i32(&mut output, *delta_y);
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
            Self::KeyframeAck { sequence } => {
                output.push(15);
                put_u64(&mut output, *sequence);
            }
            Self::DesktopUpdate {
                sequence,
                timestamp_millis,
                desktop_width,
                desktop_height,
                encoding,
                regions,
                data,
            } => {
                if regions.is_empty() || regions.len() > MAX_DESKTOP_REGIONS {
                    return Err(dc_common::DcError::InvalidInput(
                        "desktop update has an invalid region count".into(),
                    ));
                }
                output.push(16);
                put_u64(&mut output, *sequence);
                put_u64(&mut output, *timestamp_millis);
                put_u32(&mut output, *desktop_width);
                put_u32(&mut output, *desktop_height);
                output.push(*encoding);
                put_u16(&mut output, regions.len() as u16);
                for region in regions {
                    put_u32(&mut output, region.x);
                    put_u32(&mut output, region.y);
                    put_u32(&mut output, region.width);
                    put_u32(&mut output, region.height);
                }
                put_bytes(&mut output, data)?;
            }
            Self::Cursor {
                sequence,
                visible,
                x,
                y,
                shape,
            } => {
                output.push(17);
                put_u64(&mut output, *sequence);
                output.push(u8::from(*visible));
                put_i32(&mut output, *x);
                put_i32(&mut output, *y);
                output.push(u8::from(shape.is_some()));
                if let Some(shape) = shape {
                    output.push(shape.kind);
                    put_u32(&mut output, shape.width);
                    put_u32(&mut output, shape.height);
                    put_u32(&mut output, shape.hotspot_x);
                    put_u32(&mut output, shape.hotspot_y);
                    put_u32(&mut output, shape.pitch);
                    put_bytes(&mut output, &shape.data)?;
                }
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
                    key: KeyboardKey::from_wire(reader.u16()?)?,
                    pressed: reader.u8()? != 0,
                }),
                3 => Self::Input(InputEvent::Wheel {
                    delta_x: reader.i32()?,
                    delta_y: reader.i32()?,
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
            15 => Self::KeyframeAck {
                sequence: reader.u64()?,
            },
            16 => {
                let sequence = reader.u64()?;
                let timestamp_millis = reader.u64()?;
                let desktop_width = reader.u32()?;
                let desktop_height = reader.u32()?;
                let encoding = reader.u8()?;
                let region_count = usize::from(reader.u16()?);
                if region_count == 0 || region_count > MAX_DESKTOP_REGIONS {
                    return Err(dc_common::DcError::Codec(
                        "desktop update has an invalid region count".into(),
                    ));
                }
                let mut regions = Vec::with_capacity(region_count);
                for _ in 0..region_count {
                    regions.push(DesktopRect {
                        x: reader.u32()?,
                        y: reader.u32()?,
                        width: reader.u32()?,
                        height: reader.u32()?,
                    });
                }
                Self::DesktopUpdate {
                    sequence,
                    timestamp_millis,
                    desktop_width,
                    desktop_height,
                    encoding,
                    regions,
                    data: reader.bytes()?,
                }
            }
            17 => {
                let sequence = reader.u64()?;
                let visible = reader.u8()? != 0;
                let x = reader.i32()?;
                let y = reader.i32()?;
                let shape = if reader.u8()? != 0 {
                    Some(CursorShape {
                        kind: reader.u8()?,
                        width: reader.u32()?,
                        height: reader.u32()?,
                        hotspot_x: reader.u32()?,
                        hotspot_y: reader.u32()?,
                        pitch: reader.u32()?,
                        data: reader.bytes()?,
                    })
                } else {
                    None
                };
                Self::Cursor {
                    sequence,
                    visible,
                    x,
                    y,
                    shape,
                }
            }
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
    let flags = u16::from(value.desktop)
        | (u16::from(value.terminal) << 1)
        | (u16::from(value.command_execution) << 2)
        | (u16::from(value.file_transfer) << 3)
        | (u16::from(value.clipboard) << 4)
        | (u16::from(value.ssh_compatibility) << 5)
        | (u16::from(value.control_input) << 6)
        | (u16::from(value.hybrid_video) << 7)
        | (u16::from(value.desktop_optimizations) << 8)
        | (u16::from(value.h264_nal_datagrams) << 9);
    put_u16(out, flags);
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
        let flags = self.u16()?;
        Ok(Capabilities {
            desktop: flags & 1 != 0,
            control_input: flags & 64 != 0,
            terminal: flags & 2 != 0,
            command_execution: flags & 4 != 0,
            file_transfer: flags & 8 != 0,
            clipboard: flags & 16 != 0,
            ssh_compatibility: flags & 32 != 0,
            hybrid_video: flags & 128 != 0,
            desktop_optimizations: flags & 256 != 0,
            h264_nal_datagrams: flags & 512 != 0,
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
            major: PROTOCOL_VERSION.major,
            minor: 99
        }));
        assert!(!PROTOCOL_VERSION.compatible_with(ProtocolVersion {
            major: PROTOCOL_VERSION.major + 1,
            minor: 0
        }));
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
    fn keyframe_acknowledgements_round_trip() {
        let message = WireMessage::KeyframeAck { sequence: 101 };
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
                hybrid_video: true,
                desktop_optimizations: true,
                h264_nal_datagrams: true,
                ..Capabilities::default()
            },
        };
        assert_eq!(
            WireMessage::decode(&message.encode().unwrap()).unwrap(),
            message
        );
    }

    #[test]
    fn desktop_update_and_cursor_round_trip() {
        let update = WireMessage::DesktopUpdate {
            sequence: 8,
            timestamp_millis: 50,
            desktop_width: 1920,
            desktop_height: 1080,
            encoding: 1,
            regions: vec![DesktopRect {
                x: 10,
                y: 20,
                width: 30,
                height: 40,
            }],
            data: vec![1, 2, 3],
        };
        assert_eq!(
            WireMessage::decode(&update.encode().unwrap()).unwrap(),
            update
        );

        let cursor = WireMessage::Cursor {
            sequence: 9,
            visible: true,
            x: 100,
            y: 200,
            shape: Some(CursorShape {
                kind: 2,
                width: 2,
                height: 2,
                hotspot_x: 1,
                hotspot_y: 1,
                pitch: 8,
                data: vec![0xff; 16],
            }),
        };
        assert_eq!(
            WireMessage::decode(&cursor.encode().unwrap()).unwrap(),
            cursor
        );
    }

    #[test]
    fn keyboard_and_wheel_input_round_trip() {
        for event in [
            InputEvent::Key {
                key: KeyboardKey::RightControl,
                pressed: true,
            },
            InputEvent::Key {
                key: KeyboardKey::NumpadEnter,
                pressed: false,
            },
            InputEvent::Wheel {
                delta_x: -120,
                delta_y: 240,
            },
        ] {
            let message = WireMessage::Input(event);
            assert_eq!(
                WireMessage::decode(&message.encode().unwrap()).unwrap(),
                message
            );
        }
    }

    #[test]
    fn rejects_unknown_keyboard_codes() {
        let mut encoded = WireMessage::Input(InputEvent::Key {
            key: KeyboardKey::A,
            pressed: true,
        })
        .encode()
        .unwrap();
        encoded[2..4].copy_from_slice(&u16::MAX.to_be_bytes());
        assert!(WireMessage::decode(&encoded).is_err());
    }
}
