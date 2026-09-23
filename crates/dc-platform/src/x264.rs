//! Optional native libx264 encoder with explicit low-latency rate controls.

use dc_common::{log, DcError, LogLevel, Result};
use dc_media::{
    h264_access_unit_is_keyframe, EncodeOutcome, EncodedVideoPacket, EncoderCapabilities,
    PixelFormat, VideoCodec, VideoEncoder, VideoFrame,
};
use std::ffi::{c_char, c_int, c_void, CStr};
use std::ptr;

const ERROR_BUFFER_LEN: usize = 512;

unsafe extern "C" {
    fn dc_x264_encoder_new(
        width: u32,
        height: u32,
        bitrate_bps: u32,
        fps: u32,
        max_slice_len: u32,
        error: *mut c_char,
        error_len: usize,
    ) -> *mut c_void;
    fn dc_x264_encoder_force_idr(encoder: *mut c_void);
    fn dc_x264_encoder_encode(
        encoder: *mut c_void,
        bgra: *const u8,
        stride: c_int,
        pts: i64,
        output: *mut *const u8,
        output_len: *mut usize,
        keyframe: *mut c_int,
        error: *mut c_char,
        error_len: usize,
    ) -> c_int;
    fn dc_x264_encoder_close(encoder: *mut c_void);
}

pub struct X264Encoder {
    encoder: *mut c_void,
    width: u32,
    height: u32,
    max_slice_len: u32,
    first_output: bool,
}

impl X264Encoder {
    pub fn new(
        width: u32,
        height: u32,
        target_bitrate: u32,
        frames_per_second: u32,
        max_slice_len: u32,
    ) -> Result<Self> {
        if width == 0 || height == 0 || width % 2 != 0 || height % 2 != 0 {
            return Err(DcError::InvalidInput(
                "x264 dimensions must be non-zero and even".into(),
            ));
        }
        if target_bitrate == 0 || frames_per_second == 0 || max_slice_len < 256 {
            return Err(DcError::InvalidInput(
                "x264 bitrate, frame rate and slice budget must be valid".into(),
            ));
        }
        let mut error = [0_i8; ERROR_BUFFER_LEN];
        let encoder = unsafe {
            dc_x264_encoder_new(
                width,
                height,
                target_bitrate,
                frames_per_second,
                max_slice_len,
                error.as_mut_ptr(),
                error.len(),
            )
        };
        if encoder.is_null() {
            return Err(DcError::Codec(ffi_error(
                &error,
                "failed to initialize libx264",
            )));
        }
        let bitrate_kbps = target_bitrate.div_ceil(1_000).max(1);
        log(
            LogLevel::Info,
            "dc-platform::x264",
            &format!(
                "configured preset=veryfast tune=zerolatency,fastdecode profile=high bitrate={} vbv_maxrate_kbps={} vbv_bufsize_kbits={} slice_max_size={} bframes=0 rc_lookahead=0",
                target_bitrate,
                bitrate_kbps,
                bitrate_kbps.div_ceil(2),
                max_slice_len
            ),
        );
        Ok(Self {
            encoder,
            width,
            height,
            max_slice_len,
            first_output: true,
        })
    }
}

impl VideoEncoder for X264Encoder {
    fn codec(&self) -> VideoCodec {
        VideoCodec::H264
    }

    fn capabilities(&self) -> EncoderCapabilities {
        EncoderCapabilities {
            backend: "x264",
            codec: VideoCodec::H264,
            hardware_accelerated: false,
            zero_copy_input: false,
            low_latency: true,
            supports_force_keyframe: true,
        }
    }

    fn force_keyframe(&mut self) -> Result<()> {
        unsafe { dc_x264_encoder_force_idr(self.encoder) };
        Ok(())
    }

    fn encode(&mut self, frame: VideoFrame) -> Result<EncodeOutcome> {
        let layout = frame.layout();
        if layout.size().width() != self.width || layout.size().height() != self.height {
            return Err(DcError::InvalidInput(
                "x264 received a frame with changing dimensions".into(),
            ));
        }
        if layout.pixel_format() != PixelFormat::Bgra32 {
            return Err(DcError::Unsupported(
                "x264 currently accepts BGRA32 frames only".into(),
            ));
        }
        let stride = c_int::try_from(layout.stride())
            .map_err(|_| DcError::InvalidInput("x264 BGRA stride exceeds c_int".into()))?;
        let pts = i64::try_from(frame.sequence())
            .map_err(|_| DcError::InvalidInput("frame sequence exceeds x264 PTS".into()))?;
        let mut output = ptr::null();
        let mut output_len = 0_usize;
        let mut keyframe = 0;
        let mut error = [0_i8; ERROR_BUFFER_LEN];
        let result = unsafe {
            dc_x264_encoder_encode(
                self.encoder,
                frame.data().as_ptr(),
                stride,
                pts,
                &mut output,
                &mut output_len,
                &mut keyframe,
                error.as_mut_ptr(),
                error.len(),
            )
        };
        if result != 0 {
            return Err(DcError::Codec(ffi_error(
                &error,
                "libx264 failed to encode a frame",
            )));
        }
        if output.is_null() || output_len == 0 {
            return Ok(EncodeOutcome::Skipped);
        }
        let data = unsafe { std::slice::from_raw_parts(output, output_len).to_vec() };
        let nals = dc_media::split_h264_nal_units(&data)?;
        let max_nal_bytes = nals.iter().map(|nal| nal.len()).max().unwrap_or(0);
        if self.first_output || max_nal_bytes > self.max_slice_len as usize {
            log(
                if max_nal_bytes > self.max_slice_len as usize {
                    LogLevel::Warn
                } else {
                    LogLevel::Info
                },
                "dc-platform::x264",
                &format!(
                    "encoded access_unit_bytes={} nal_count={} max_nal_bytes={} slice_budget={}",
                    data.len(),
                    nals.len(),
                    max_nal_bytes,
                    self.max_slice_len
                ),
            );
        }
        self.first_output = false;
        let keyframe = keyframe != 0 || h264_access_unit_is_keyframe(&data)?;
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

impl Drop for X264Encoder {
    fn drop(&mut self) {
        if !self.encoder.is_null() {
            unsafe { dc_x264_encoder_close(self.encoder) };
            self.encoder = ptr::null_mut();
        }
    }
}

fn ffi_error(buffer: &[c_char], fallback: &str) -> String {
    if buffer.first().copied().unwrap_or_default() == 0 {
        return fallback.into();
    }
    unsafe { CStr::from_ptr(buffer.as_ptr()) }
        .to_string_lossy()
        .into_owned()
}
