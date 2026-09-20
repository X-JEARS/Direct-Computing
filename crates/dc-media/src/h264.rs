use crate::{
    EncodeOutcome, EncodedVideoPacket, FrameLayout, PixelFormat, VideoCodec, VideoDecoder,
    VideoEncoder, VideoFrame,
};
use dc_common::{DcError, Result};
use openh264::decoder::Decoder;
use openh264::encoder::{BitRate, Encoder, EncoderConfig, FrameRate, FrameType, UsageType};
use openh264::formats::{BgraSliceU8, RgbSliceU8, YUVBuffer, YUVSource};
use openh264::{OpenH264API, Timestamp};

pub struct OpenH264Encoder {
    encoder: Encoder,
}

impl OpenH264Encoder {
    pub fn new(target_bitrate: u32, frames_per_second: f32) -> Result<Self> {
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
        let config = EncoderConfig::new()
            .bitrate(BitRate::from_bps(target_bitrate))
            .max_frame_rate(FrameRate::from_hz(frames_per_second))
            .usage_type(UsageType::ScreenContentRealTime)
            .skip_frames(true)
            .adaptive_quantization(false)
            .background_detection(false);
        let encoder = Encoder::with_api_config(OpenH264API::from_source(), config)
            .map_err(|error| DcError::Codec(error.to_string()))?;
        Ok(Self { encoder })
    }
}

impl VideoEncoder for OpenH264Encoder {
    fn codec(&self) -> VideoCodec {
        VideoCodec::H264
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
}
