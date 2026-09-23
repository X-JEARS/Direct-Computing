use crate::{
    DecoderCapabilities, EncodeOutcome, EncodedVideoPacket, EncoderCapabilities, FrameLayout,
    PixelFormat, VideoCodec, VideoDecoder, VideoEncoder, VideoFrame,
};
use dc_common::{DcError, Result};
use openh264::decoder::Decoder;
use openh264::encoder::{
    BitRate, Complexity, Encoder, EncoderConfig, FrameRate, FrameType, QpRange, RateControlMode,
    UsageType,
};
use openh264::formats::{BgraSliceU8, RgbSliceU8, YUVBuffer, YUVSource};
use openh264::{OpenH264API, Timestamp};

pub struct OpenH264Encoder {
    encoder: Encoder,
}

impl OpenH264Encoder {
    pub fn new(target_bitrate: u32, frames_per_second: f32) -> Result<Self> {
        Self::new_with_qp(target_bitrate, frames_per_second, None)
    }

    /// Create a screen encoder with an optional explicit quantizer range.
    /// Media Foundation's average bitrate is not a hard per-frame quality
    /// control, so narrow-band profiles use this path to force visibly lower
    /// quality and smaller IDR frames.
    pub fn new_with_qp(
        target_bitrate: u32,
        frames_per_second: f32,
        qp_range: Option<QpRange>,
    ) -> Result<Self> {
        if target_bitrate == 0 {
            return Err(DcError::InvalidInput(
                "H.264 target bitrate must be non-zero".into(),
            ));
        }
        if !frames_per_second.is_finite() || frames_per_second <= 0.0 {
            return Err(DcError::InvalidInput(
                "H.264 frame rate must be finite and greater than zero".into(),
            ));
        }
        let mut config = EncoderConfig::new()
            .bitrate(BitRate::from_bps(target_bitrate))
            .max_frame_rate(FrameRate::from_hz(frames_per_second))
            .usage_type(UsageType::ScreenContentRealTime)
            // The target bitrate is a network budget, not just a quality hint.
            // Bitrate mode prevents large quality-mode bursts and low
            // complexity keeps the software fallback from monopolising the
            // capture thread at full desktop resolution.
            .rate_control_mode(RateControlMode::Bitrate)
            .complexity(Complexity::Low)
            .skip_frames(true)
            .adaptive_quantization(false)
            .background_detection(false);
        if let Some(qp_range) = qp_range {
            config = config.qp(qp_range);
        }
        let encoder = Encoder::with_api_config(OpenH264API::from_source(), config)
            .map_err(|error| DcError::Codec(error.to_string()))?;
        Ok(Self { encoder })
    }

    pub fn new_low_quality(
        target_bitrate: u32,
        frames_per_second: f32,
        minimum_qp: u8,
    ) -> Result<Self> {
        Self::new_with_qp(
            target_bitrate,
            frames_per_second,
            Some(QpRange::new(minimum_qp.min(51), 51)),
        )
    }

    /// Create the software fallback with a hard maximum NAL/slice size.
    ///
    /// OpenH264 applies this limit inside the encoder, unlike an application
    /// level UDP fragment cap which can only reject an already encoded frame.
    /// Keeping the limit slightly below the negotiated datagram payload leaves
    /// room for the media and datagram headers.
    pub fn new_network(
        target_bitrate: u32,
        frames_per_second: f32,
        minimum_qp: u8,
        max_slice_len: u32,
    ) -> Result<Self> {
        if max_slice_len < 256 {
            return Err(DcError::InvalidInput(
                "H.264 maximum slice length must be at least 256 bytes".into(),
            ));
        }
        if target_bitrate == 0 {
            return Err(DcError::InvalidInput(
                "H.264 target bitrate must be non-zero".into(),
            ));
        }
        if !frames_per_second.is_finite() || frames_per_second <= 0.0 {
            return Err(DcError::InvalidInput(
                "H.264 frame rate must be finite and greater than zero".into(),
            ));
        }
        let qp_range = QpRange::new(minimum_qp.min(51), 51);
        let config = EncoderConfig::new()
            .bitrate(BitRate::from_bps(target_bitrate))
            .max_frame_rate(FrameRate::from_hz(frames_per_second))
            .usage_type(UsageType::ScreenContentRealTime)
            .rate_control_mode(RateControlMode::Bitrate)
            .complexity(Complexity::Low)
            .skip_frames(true)
            .adaptive_quantization(false)
            .background_detection(false)
            .qp(qp_range)
            .max_slice_len(max_slice_len);
        let encoder = Encoder::with_api_config(OpenH264API::from_source(), config)
            .map_err(|error| DcError::Codec(error.to_string()))?;
        Ok(Self { encoder })
    }
}

impl VideoEncoder for OpenH264Encoder {
    fn codec(&self) -> VideoCodec {
        VideoCodec::H264
    }

    fn capabilities(&self) -> EncoderCapabilities {
        EncoderCapabilities {
            backend: "openh264",
            codec: VideoCodec::H264,
            hardware_accelerated: false,
            zero_copy_input: false,
            low_latency: true,
            supports_force_keyframe: false,
        }
    }

    fn force_keyframe(&mut self) -> Result<()> {
        self.encoder.force_intra_frame();
        Ok(())
    }

    fn encode(&mut self, frame: VideoFrame) -> Result<EncodeOutcome> {
        let layout = frame.layout();
        let size = layout.size();
        if size.width() % 2 != 0 || size.height() % 2 != 0 {
            return Err(DcError::InvalidInput(
                "H.264 frame dimensions must be even".into(),
            ));
        }

        let dimensions = (size.width() as usize, size.height() as usize);
        let packed = tightly_pack(&frame)?;
        let yuv = match layout.pixel_format() {
            PixelFormat::Bgra32 => {
                YUVBuffer::from_bgra8_source(BgraSliceU8::new(&packed, dimensions))
            }
            PixelFormat::Rgb24 => YUVBuffer::from_rgb8_source(RgbSliceU8::new(&packed, dimensions)),
            PixelFormat::I420 => {
                return Err(DcError::Unsupported(
                    "I420 frame layout is not implemented yet".into(),
                ));
            }
        };
        let timestamp_millis = u64::try_from(frame.timestamp().as_millis()).map_err(|_| {
            DcError::InvalidInput("frame timestamp exceeds u64 milliseconds".into())
        })?;
        let bitstream = self
            .encoder
            .encode_at(&yuv, Timestamp::from_millis(timestamp_millis))
            .map_err(|error| DcError::Codec(error.to_string()))?;
        let frame_type = bitstream.frame_type();
        let keyframe = matches!(frame_type, FrameType::IDR | FrameType::I);
        let data = bitstream.to_vec();
        if frame_type == FrameType::Skip || data.is_empty() {
            return Ok(EncodeOutcome::Skipped);
        }
        Ok(EncodeOutcome::Packet(EncodedVideoPacket::new(
            VideoCodec::H264,
            frame.sequence(),
            frame.timestamp(),
            keyframe,
            layout,
            data,
        )?))
    }
}

pub struct OpenH264Decoder {
    decoder: Decoder,
}

impl OpenH264Decoder {
    pub fn new() -> Result<Self> {
        let decoder = Decoder::new().map_err(|error| DcError::Codec(error.to_string()))?;
        Ok(Self { decoder })
    }
}

impl VideoDecoder for OpenH264Decoder {
    fn codec(&self) -> VideoCodec {
        VideoCodec::H264
    }

    fn capabilities(&self) -> DecoderCapabilities {
        DecoderCapabilities {
            backend: "openh264",
            codec: VideoCodec::H264,
            hardware_accelerated: false,
            zero_copy_output: false,
            low_latency: true,
        }
    }

    fn decode(&mut self, packet: EncodedVideoPacket) -> Result<VideoFrame> {
        if packet.codec() != VideoCodec::H264 {
            return Err(DcError::InvalidInput(format!(
                "H.264 decoder received {:?} data",
                packet.codec()
            )));
        }
        let sequence = packet.sequence();
        let timestamp = packet.timestamp();
        let decoded = self
            .decoder
            .decode(packet.data())
            .map_err(|error| DcError::Codec(error.to_string()))?
            .ok_or_else(|| DcError::Codec("decoder produced no frame".into()))?;
        let (width, height) = decoded.dimensions();
        let size = crate::FrameSize::new(
            u32::try_from(width).map_err(|_| DcError::Codec("decoded width exceeds u32".into()))?,
            u32::try_from(height)
                .map_err(|_| DcError::Codec("decoded height exceeds u32".into()))?,
        )?;
        let layout = FrameLayout::packed(size, PixelFormat::Rgb24)?;
        let mut data = vec![0; layout.data_len()];
        decoded.write_rgb8(&mut data);
        VideoFrame::new(sequence, timestamp, layout, data)
    }
}

fn tightly_pack(frame: &VideoFrame) -> Result<Vec<u8>> {
    let layout = frame.layout();
    let bytes_per_pixel = layout
        .pixel_format()
        .packed_bytes_per_pixel()
        .ok_or_else(|| DcError::Unsupported("planar input is not implemented yet".into()))?;
    let row_bytes = layout.size().width() as usize * bytes_per_pixel;
    let mut packed = Vec::with_capacity(row_bytes * layout.size().height() as usize);
    for row in frame.data().chunks_exact(layout.stride()) {
        packed.extend_from_slice(&row[..row_bytes]);
    }
    Ok(packed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FrameSource, SyntheticFrameSource};

    #[test]
    fn h264_round_trip_preserves_frame_metadata() {
        let size = crate::FrameSize::new(64, 48).unwrap();
        let mut source = SyntheticFrameSource::new(size, 30).unwrap();
        let frame = source.capture().unwrap();
        let source_len = frame.data().len();
        let mut encoder = OpenH264Encoder::new(500_000, 30.0).unwrap();
        let EncodeOutcome::Packet(packet) = encoder.encode(frame).unwrap() else {
            panic!("the first encoded frame must not be skipped");
        };

        assert_eq!(packet.codec(), VideoCodec::H264);
        assert!(packet.is_keyframe());
        assert!(packet.data().len() < source_len);

        let mut decoder = OpenH264Decoder::new().unwrap();
        let decoded = decoder.decode(packet).unwrap();
        assert_eq!(decoded.sequence(), 0);
        assert_eq!(decoded.layout().size(), size);
        assert_eq!(decoded.layout().pixel_format(), PixelFormat::Rgb24);
    }

    #[test]
    fn h264_rejects_odd_dimensions_without_panicking() {
        let size = crate::FrameSize::new(3, 3).unwrap();
        let mut source = SyntheticFrameSource::new(size, 30).unwrap();
        let frame = source.capture().unwrap();
        let mut encoder = OpenH264Encoder::new(500_000, 30.0).unwrap();
        assert!(encoder.encode(frame).is_err());
    }

    #[test]
    fn h264_rejects_invalid_encoder_settings() {
        assert!(OpenH264Encoder::new(0, 30.0).is_err());
        assert!(OpenH264Encoder::new(500_000, 0.0).is_err());
        assert!(OpenH264Encoder::new(500_000, f32::NAN).is_err());
    }

    #[test]
    fn openh264_reports_software_low_latency_capabilities() {
        let encoder = OpenH264Encoder::new(500_000, 30.0).unwrap();
        let capabilities = encoder.capabilities();
        assert_eq!(capabilities.backend, "openh264");
        assert!(!capabilities.hardware_accelerated);
        assert!(!capabilities.zero_copy_input);
        assert!(capabilities.low_latency);
    }

    #[test]
    fn openh264_decoder_reports_software_capabilities() {
        let decoder = OpenH264Decoder::new().unwrap();
        let capabilities = decoder.capabilities();
        assert_eq!(capabilities.backend, "openh264");
        assert_eq!(capabilities.codec, VideoCodec::H264);
        assert!(!capabilities.hardware_accelerated);
        assert!(!capabilities.zero_copy_output);
        assert!(capabilities.low_latency);
    }

    #[test]
    fn openh264_network_encoder_bounds_video_slice_nals() {
        let size = crate::FrameSize::new(640, 360).unwrap();
        let mut source = SyntheticFrameSource::new(size, 30).unwrap();
        let frame = source.capture().unwrap();
        let mut encoder = OpenH264Encoder::new_network(4_000_000, 30.0, 0, 900).unwrap();
        let EncodeOutcome::Packet(packet) = encoder.encode(frame).unwrap() else {
            panic!("the first network frame must not be skipped");
        };
        let video_nals: Vec<_> = crate::split_h264_nal_units(packet.data())
            .unwrap()
            .into_iter()
            .filter(|nal| {
                nal.first()
                    .is_some_and(|header| header & 0x1f == 1 || header & 0x1f == 5)
            })
            .collect();
        assert!(
            video_nals.len() > 1,
            "the frame should be split into multiple slices"
        );
        assert!(video_nals.iter().all(|nal| nal.len() <= 900));
    }
}
