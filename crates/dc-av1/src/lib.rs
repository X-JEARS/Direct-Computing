//! Safe Direct Computing adapters around the bundled libaom C API.

use dc_common::{DcError, Result};
use dc_media::{
    DecoderCapabilities, EncodeOutcome, EncodedVideoPacket, EncoderCapabilities, FrameLayout,
    PixelFormat, VideoCodec, VideoDecoder, VideoEncoder, VideoFrame,
};
use shiguredo_aom::{
    AomRational, ContentType, Decoder, DecoderConfig, EncodeOptions, Encoder, EncoderConfig,
    ImageData, ImageFormat, KeyframeMode, RateControlMode, Usage,
};

pub struct LibaomAv1Encoder {
    encoder: Encoder,
    width: u32,
    height: u32,
    force_keyframe: bool,
}

impl LibaomAv1Encoder {
    pub fn new(width: u32, height: u32, target_bitrate: u32, fps: u32) -> Result<Self> {
        validate_dimensions(width, height)?;
        if target_bitrate == 0 || fps == 0 {
            return Err(DcError::InvalidInput(
                "AV1 bitrate and frame rate must be non-zero".into(),
            ));
        }

        let mut config = EncoderConfig::new(width, height, ImageFormat::I420);
        config.g_usage = Usage::Realtime;
        config.g_threads = std::thread::available_parallelism()
            .map(|count| count.get().min(8) as u32)
            .ok();
        config.g_profile = 0;
        config.g_timebase = AomRational {
            num: 1,
            den: fps as i32,
        };
        config.g_error_resilient = true;
        config.g_lag_in_frames = Some(0);
        config.rc_dropframe_thresh = Some(0);
        config.rc_end_usage = RateControlMode::Cbr;
        config.rc_target_bitrate = target_bitrate.div_ceil(1_000).max(1);
        config.rc_min_quantizer = 4;
        config.rc_max_quantizer = 56;
        config.rc_buf_sz = Some(500);
        config.rc_buf_initial_sz = Some(100);
        config.rc_buf_optimal_sz = Some(250);
        config.kf_mode = Some(KeyframeMode::Auto);
        config.kf_min_dist = Some(0);
        config.kf_max_dist = Some(fps.saturating_mul(5));
        config.cpu_used = Some(8);
        config.row_mt = Some(true);
        config.tune_content = Some(ContentType::Screen);

        Ok(Self {
            encoder: Encoder::new(config)
                .map_err(|error| DcError::Codec(format!("initialize AV1 encoder: {error}")))?,
            width,
            height,
            force_keyframe: false,
        })
    }
}

impl VideoEncoder for LibaomAv1Encoder {
    fn codec(&self) -> VideoCodec {
        VideoCodec::Av1
    }

    fn capabilities(&self) -> EncoderCapabilities {
        EncoderCapabilities {
            backend: "libaom-av1",
            codec: VideoCodec::Av1,
            hardware_accelerated: false,
            zero_copy_input: false,
            low_latency: true,
            supports_force_keyframe: true,
        }
    }

    fn force_keyframe(&mut self) -> Result<()> {
        self.force_keyframe = true;
        Ok(())
    }

    fn encode(&mut self, frame: VideoFrame) -> Result<EncodeOutcome> {
        let layout = frame.layout();
        let size = layout.size();
        if size.width() != self.width || size.height() != self.height {
            return Err(DcError::InvalidInput(
                "AV1 encoder received a frame with changing dimensions".into(),
            ));
        }

        let i420 = frame_to_i420(&frame)?;
        let y_len = self.width as usize * self.height as usize;
        let chroma_len = y_len / 4;
        let image = ImageData::I420 {
            y: &i420[..y_len],
            u: &i420[y_len..y_len + chroma_len],
            v: &i420[y_len + chroma_len..],
        };
        self.encoder
            .encode(
                &image,
                &EncodeOptions {
                    force_keyframe: self.force_keyframe,
                },
            )
            .map_err(|error| DcError::Codec(format!("encode AV1 frame: {error}")))?;
        self.force_keyframe = false;
        let mut data = Vec::new();
        let mut keyframe = false;
        while let Some(encoded) = self.encoder.next_frame() {
            data.extend_from_slice(
                encoded
                    .data()
                    .map_err(|error| DcError::Codec(format!("read AV1 frame: {error}")))?,
            );
            keyframe |= encoded.is_keyframe();
        }
        if data.is_empty() {
            return Ok(EncodeOutcome::Skipped);
        }
        Ok(EncodeOutcome::Packet(EncodedVideoPacket::new(
            VideoCodec::Av1,
            frame.sequence(),
            frame.timestamp(),
            keyframe,
            layout,
            data,
        )?))
    }
}

pub struct LibaomAv1Decoder {
    decoder: Decoder,
}

impl LibaomAv1Decoder {
    pub fn new() -> Result<Self> {
        let config = DecoderConfig {
            threads: std::thread::available_parallelism()
                .map(|count| count.get().min(8) as u32)
                .ok(),
            w: None,
            h: None,
            allow_lowbitdepth: Some(true),
        };
        Ok(Self {
            decoder: Decoder::new(config)
                .map_err(|error| DcError::Codec(format!("initialize AV1 decoder: {error}")))?,
        })
    }
}

impl VideoDecoder for LibaomAv1Decoder {
    fn codec(&self) -> VideoCodec {
        VideoCodec::Av1
    }

    fn capabilities(&self) -> DecoderCapabilities {
        DecoderCapabilities {
            backend: "libaom-av1",
            codec: VideoCodec::Av1,
            hardware_accelerated: false,
            zero_copy_output: false,
            low_latency: true,
        }
    }

    fn decode(&mut self, packet: EncodedVideoPacket) -> Result<VideoFrame> {
        if packet.codec() != VideoCodec::Av1 {
            return Err(DcError::InvalidInput(format!(
                "AV1 decoder received {:?} data",
                packet.codec()
            )));
        }
        self.decoder
            .decode(packet.data())
            .map_err(|error| DcError::Codec(format!("decode AV1 frame: {error}")))?;
        let expected = packet.source_layout().size();
        let data = {
            let image = self
                .decoder
                .next_frame()
                .ok_or_else(|| DcError::Codec("libaom produced no decoded AV1 frame".into()))?;
            let format = image
                .format()
                .map_err(|error| DcError::Codec(error.to_string()))?;
            if format != ImageFormat::I420 || image.is_high_depth() {
                return Err(DcError::Unsupported(format!(
                    "libaom produced unsupported AV1 image format {format:?}"
                )));
            }
            if image.width() != expected.width() as usize
                || image.height() != expected.height() as usize
            {
                return Err(DcError::Codec(format!(
                    "decoded AV1 dimensions {}x{} differ from packet {}x{}",
                    image.width(),
                    image.height(),
                    expected.width(),
                    expected.height()
                )));
            }
            i420_image_to_bgra(&image)?
        };
        if self.decoder.next_frame().is_some() {
            return Err(DcError::Codec(
                "libaom produced multiple pictures for one AV1 packet".into(),
            ));
        }
        let layout = FrameLayout::packed(expected, PixelFormat::Bgra32)?;
        VideoFrame::new(packet.sequence(), packet.timestamp(), layout, data)
    }
}

fn validate_dimensions(width: u32, height: u32) -> Result<()> {
    if width == 0 || height == 0 || width % 2 != 0 || height % 2 != 0 {
        return Err(DcError::InvalidInput(
            "AV1 frame dimensions must be non-zero and even".into(),
        ));
    }
    Ok(())
}

fn frame_to_i420(frame: &VideoFrame) -> Result<Vec<u8>> {
    let layout = frame.layout();
    let size = layout.size();
    validate_dimensions(size.width(), size.height())?;
    let width = size.width() as usize;
    let height = size.height() as usize;
    let chroma_width = width / 2;
    let y_len = width
        .checked_mul(height)
        .ok_or_else(|| DcError::InvalidInput("AV1 frame size overflow".into()))?;
    let chroma_len = chroma_width
        .checked_mul(height / 2)
        .ok_or_else(|| DcError::InvalidInput("AV1 chroma size overflow".into()))?;
    let mut output = vec![0_u8; y_len + chroma_len * 2];
    let (y_plane, chroma) = output.split_at_mut(y_len);
    let (u_plane, v_plane) = chroma.split_at_mut(chroma_len);
    let bytes_per_pixel = layout
        .pixel_format()
        .packed_bytes_per_pixel()
        .ok_or_else(|| {
            DcError::Unsupported("AV1 encoder requires packed RGB24 or BGRA32 input".into())
        })?;

    for block_y in (0..height).step_by(2) {
        for block_x in (0..width).step_by(2) {
            let mut u_sum = 0_i32;
            let mut v_sum = 0_i32;
            for offset_y in 0..2 {
                for offset_x in 0..2 {
                    let x = block_x + offset_x;
                    let y = block_y + offset_y;
                    let start = y * layout.stride() + x * bytes_per_pixel;
                    let pixel = &frame.data()[start..start + bytes_per_pixel];
                    let (r, g, b) = match layout.pixel_format() {
                        PixelFormat::Bgra32 => (pixel[2], pixel[1], pixel[0]),
                        PixelFormat::Rgb24 => (pixel[0], pixel[1], pixel[2]),
                        PixelFormat::I420 => unreachable!(),
                    };
                    let r = i32::from(r);
                    let g = i32::from(g);
                    let b = i32::from(b);
                    y_plane[y * width + x] =
                        clamp_u8(((66 * r + 129 * g + 25 * b + 128) >> 8) + 16);
                    u_sum += ((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128;
                    v_sum += ((112 * r - 94 * g - 18 * b + 128) >> 8) + 128;
                }
            }
            let chroma_index = (block_y / 2) * chroma_width + block_x / 2;
            u_plane[chroma_index] = clamp_u8((u_sum + 2) / 4);
            v_plane[chroma_index] = clamp_u8((v_sum + 2) / 4);
        }
    }
    Ok(output)
}

fn i420_image_to_bgra(image: &shiguredo_aom::DecodedFrame<'_>) -> Result<Vec<u8>> {
    let width = image.width();
    let height = image.height();
    validate_dimensions(width as u32, height as u32)?;
    let y_plane = image
        .y_plane()
        .map_err(|error| DcError::Codec(error.to_string()))?;
    let u_plane = image
        .u_plane()
        .map_err(|error| DcError::Codec(error.to_string()))?;
    let v_plane = image
        .v_plane()
        .map_err(|error| DcError::Codec(error.to_string()))?;
    let y_stride = image
        .y_stride()
        .map_err(|error| DcError::Codec(error.to_string()))?;
    let u_stride = image
        .u_stride()
        .map_err(|error| DcError::Codec(error.to_string()))?;
    let v_stride = image
        .v_stride()
        .map_err(|error| DcError::Codec(error.to_string()))?;
    let output_len = width
        .checked_mul(height)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| DcError::InvalidInput("decoded AV1 frame size overflow".into()))?;
    let mut output = vec![0_u8; output_len];
    for y in 0..height {
        let y_row = &y_plane[y * y_stride..y * y_stride + width];
        let u_row = &u_plane[(y / 2) * u_stride..(y / 2) * u_stride + width / 2];
        let v_row = &v_plane[(y / 2) * v_stride..(y / 2) * v_stride + width / 2];
        for x in 0..width {
            let c = (i32::from(y_row[x]) - 16).max(0);
            let d = i32::from(u_row[x / 2]) - 128;
            let e = i32::from(v_row[x / 2]) - 128;
            let offset = (y * width + x) * 4;
            output[offset] = clamp_u8((298 * c + 516 * d + 128) >> 8);
            output[offset + 1] = clamp_u8((298 * c - 100 * d - 208 * e + 128) >> 8);
            output[offset + 2] = clamp_u8((298 * c + 409 * e + 128) >> 8);
            output[offset + 3] = 255;
        }
    }
    Ok(output)
}

fn clamp_u8(value: i32) -> u8 {
    value.clamp(0, 255) as u8
}

#[cfg(test)]
mod tests {
    use super::*;
    use dc_media::{FrameSize, VideoDecoder, VideoEncoder};
    use std::time::Duration;

    fn frame(sequence: u64, changed: bool) -> VideoFrame {
        let size = FrameSize::new(32, 24).unwrap();
        let layout = FrameLayout::packed(size, PixelFormat::Bgra32).unwrap();
        let mut data = vec![0_u8; layout.data_len()];
        for pixel in data.chunks_exact_mut(4) {
            pixel.copy_from_slice(&[32, 96, 192, 255]);
        }
        if changed {
            data[0..4].copy_from_slice(&[220, 30, 10, 255]);
        }
        VideoFrame::new(sequence, Duration::from_millis(sequence * 33), layout, data).unwrap()
    }

    #[test]
    fn encodes_inter_frames_and_decodes_full_canvas() {
        let mut encoder = LibaomAv1Encoder::new(32, 24, 300_000, 30).unwrap();
        let mut decoder = LibaomAv1Decoder::new().unwrap();
        let first = match encoder.encode(frame(0, false)).unwrap() {
            EncodeOutcome::Packet(packet) => packet,
            EncodeOutcome::Skipped => panic!("first frame was skipped"),
        };
        assert!(first.is_keyframe());
        let decoded = decoder.decode(first).unwrap();
        assert_eq!(decoded.layout().size(), FrameSize::new(32, 24).unwrap());
        assert_eq!(decoded.layout().pixel_format(), PixelFormat::Bgra32);

        let second = match encoder.encode(frame(1, true)).unwrap() {
            EncodeOutcome::Packet(packet) => packet,
            EncodeOutcome::Skipped => panic!("second frame was skipped"),
        };
        assert!(!second.is_keyframe());
        let decoded = decoder.decode(second).unwrap();
        assert_eq!(decoded.sequence(), 1);
        assert_eq!(decoded.data().len(), 32 * 24 * 4);
    }
}
