//! Desktop capture, remote input, and adaptive stream policy.

use dc_common::{DcError, Result};
use dc_media::{EncodedVideoPacket, FrameLayout, PixelFormat, VideoCodec};
use dc_protocol::{InputEvent, WireMessage};
use std::time::Duration;

pub use dc_media::FrameSize;

pub fn packet_to_message(packet: &EncodedVideoPacket) -> Result<WireMessage> {
    let layout = packet.source_layout();
    let codec = match packet.codec() {
        VideoCodec::Raw => 0,
        VideoCodec::H264 => 1,
    };
    let pixel_format = match layout.pixel_format() {
        PixelFormat::Rgb24 => 0,
        PixelFormat::Bgra32 => 1,
        PixelFormat::I420 => 2,
    };
    Ok(WireMessage::Video {
        codec,
        sequence: packet.sequence(),
        timestamp_millis: u64::try_from(packet.timestamp().as_millis())
            .map_err(|_| DcError::InvalidInput("frame timestamp exceeds protocol range".into()))?,
        keyframe: packet.is_keyframe(),
        width: layout.size().width(),
        height: layout.size().height(),
        pixel_format,
        stride: u32::try_from(layout.stride())
            .map_err(|_| DcError::InvalidInput("frame stride exceeds protocol range".into()))?,
        data: packet.data().to_vec(),
    })
}

pub fn message_to_packet(message: WireMessage) -> Result<EncodedVideoPacket> {
    let WireMessage::Video {
        codec,
        sequence,
        timestamp_millis,
        keyframe,
        width,
        height,
        pixel_format,
        stride,
        data,
    } = message
    else {
        return Err(DcError::InvalidInput("expected video message".into()));
    };
    packet_from_parts(
        codec,
        sequence,
        timestamp_millis,
        keyframe,
        width,
        height,
        pixel_format,
        stride,
        data,
    )
}

/// Decode a video message without cloning the message envelope. The encoded
/// payload is copied once into the owned packet required by the decoder.
pub fn message_to_packet_ref(message: &WireMessage) -> Result<EncodedVideoPacket> {
    let WireMessage::Video {
        codec,
        sequence,
        timestamp_millis,
        keyframe,
        width,
        height,
        pixel_format,
        stride,
        data,
    } = message
    else {
        return Err(DcError::InvalidInput("expected video message".into()));
    };
    packet_from_parts(
        *codec,
        *sequence,
        *timestamp_millis,
        *keyframe,
        *width,
        *height,
        *pixel_format,
        *stride,
        data.clone(),
    )
}

#[allow(clippy::too_many_arguments)]
fn packet_from_parts(
    codec: u8,
    sequence: u64,
    timestamp_millis: u64,
    keyframe: bool,
    width: u32,
    height: u32,
    pixel_format: u8,
    stride: u32,
    data: Vec<u8>,
) -> Result<EncodedVideoPacket> {
    let codec = match codec {
        0 => VideoCodec::Raw,
        1 => VideoCodec::H264,
        _ => return Err(DcError::Codec("unknown video codec".into())),
    };
    let pixel_format = match pixel_format {
        0 => PixelFormat::Rgb24,
        1 => PixelFormat::Bgra32,
        2 => PixelFormat::I420,
        _ => return Err(DcError::Codec("unknown pixel format".into())),
    };
    let size = FrameSize::new(width, height)?;
    let layout = FrameLayout::with_stride(
        size,
        pixel_format,
        usize::try_from(stride)
            .map_err(|_| DcError::InvalidInput("frame stride is invalid".into()))?,
    )?;
    EncodedVideoPacket::new(
        codec,
        sequence,
        Duration::from_millis(timestamp_millis),
        keyframe,
        layout,
        data,
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RateSample {
    pub rtt: Duration,
    pub loss_percent: u8,
    pub queue_depth: u16,
    pub encode_time: Duration,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RateSettings {
    pub bitrate: u32,
    pub frames_per_second: u16,
}

#[derive(Clone, Copy, Debug)]
pub struct RateController {
    settings: RateSettings,
    minimum: RateSettings,
    maximum: RateSettings,
}

impl RateController {
    pub const fn new(initial: RateSettings, minimum: RateSettings, maximum: RateSettings) -> Self {
        Self {
            settings: initial,
            minimum,
            maximum,
        }
    }
    pub const fn settings(self) -> RateSettings {
        self.settings
    }
    pub fn update(&mut self, sample: RateSample) -> RateSettings {
        let congested = sample.loss_percent >= 5
            || sample.rtt >= Duration::from_millis(150)
            || sample.queue_depth >= 3;
        let overloaded = sample.encode_time >= Duration::from_millis(45);
        if congested || overloaded {
            self.settings.bitrate = (self.settings.bitrate * 80 / 100).max(self.minimum.bitrate);
            self.settings.frames_per_second =
                (self.settings.frames_per_second * 80 / 100).max(self.minimum.frames_per_second);
        } else {
            self.settings.bitrate = (self.settings.bitrate * 105 / 100).min(self.maximum.bitrate);
            self.settings.frames_per_second =
                (self.settings.frames_per_second + 1).min(self.maximum.frames_per_second);
        }
        self.settings
    }
}

pub fn validate_input(event: &InputEvent, width: u32, height: u32) -> Result<()> {
    match event {
        InputEvent::Pointer { buttons, .. } if buttons & !0x07 != 0 => Err(DcError::InvalidInput(
            "pointer contains unsupported button bits".into(),
        )),
        InputEvent::Pointer { x, y, .. }
            if *x < 0 || *y < 0 || *x >= width as i32 || *y >= height as i32 =>
        {
            Err(DcError::InvalidInput(
                "pointer position is outside the desktop".into(),
            ))
        }
        InputEvent::Wheel { delta_x, delta_y }
            if delta_x.unsigned_abs() > 12_000 || delta_y.unsigned_abs() > 12_000 =>
        {
            Err(DcError::InvalidInput(
                "wheel delta exceeds the per-event limit".into(),
            ))
        }
        InputEvent::Pointer { .. } | InputEvent::Wheel { .. } | InputEvent::Key { .. } => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dc_media::{EncodedVideoPacket, VideoCodec};

    #[test]
    fn packet_message_round_trip_preserves_metadata() {
        let size = FrameSize::new(2, 2).unwrap();
        let layout = FrameLayout::packed(size, PixelFormat::Bgra32).unwrap();
        let packet = EncodedVideoPacket::new(
            VideoCodec::H264,
            4,
            Duration::from_millis(8),
            true,
            layout,
            vec![1, 2],
        )
        .unwrap();
        assert_eq!(
            message_to_packet(packet_to_message(&packet).unwrap()).unwrap(),
            packet
        );
    }

    #[test]
    fn rate_controller_reduces_settings_when_network_is_congested() {
        let mut controller = RateController::new(
            RateSettings {
                bitrate: 1_000,
                frames_per_second: 30,
            },
            RateSettings {
                bitrate: 400,
                frames_per_second: 5,
            },
            RateSettings {
                bitrate: 2_000,
                frames_per_second: 60,
            },
        );
        let result = controller.update(RateSample {
            rtt: Duration::from_millis(200),
            loss_percent: 0,
            queue_depth: 0,
            encode_time: Duration::ZERO,
        });
        assert!(result.bitrate < 1_000 && result.frames_per_second < 30);
    }

    #[test]
    fn validates_pointer_buttons_and_wheel_bounds() {
        assert!(validate_input(
            &InputEvent::Wheel {
                delta_x: 120,
                delta_y: -240,
            },
            100,
            100,
        )
        .is_ok());
        assert!(validate_input(
            &InputEvent::Wheel {
                delta_x: 0,
                delta_y: 12_001,
            },
            100,
            100,
        )
        .is_err());
        assert!(validate_input(
            &InputEvent::Pointer {
                x: 1,
                y: 1,
                buttons: 0x80,
            },
            100,
            100,
        )
        .is_err());
    }
}

mod region;
mod video_datagram;

pub use region::{
    changed_area, decode_region_payload, encode_region_update, DirtyRegionDetector,
    REGION_ENCODING_PACK_BITS, REGION_ENCODING_RAW,
};
pub use video_datagram::{
    classify_video_datagram, packetize_h264_nal_message, packetize_video_message,
};
pub use video_datagram::{
    H264NalDatagramReassembler, VideoDatagramKind, VideoDatagramReassembler, VideoDatagramStats,
};
