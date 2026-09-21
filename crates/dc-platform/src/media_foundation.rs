//! Windows Media Foundation H.264 MFT backend.
//!
//! The backend intentionally accepts CPU BGRA frames for now. This gives the
//! application a vendor-neutral Windows encoder path while keeping the future
//! D3D11 zero-copy and NVENC/AMF/QSV implementations behind the same trait.

use dc_common::{DcError, Result};
use dc_media::{
    EncodeOutcome, EncodedVideoPacket, EncoderCapabilities, FrameLayout, PixelFormat, VideoCodec,
    VideoEncoder, VideoFrame,
};
use std::mem::ManuallyDrop;
use std::sync::OnceLock;
use windows::core::GUID;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::CoTaskMemFree;

const HNS_PER_SECOND: u64 = 10_000_000;

pub struct WindowsMediaFoundationH264Encoder {
    transform: IMFTransform,
    width: u32,
    height: u32,
    frames_per_second: u32,
    first_output: bool,
    hardware_accelerated: bool,
}

impl WindowsMediaFoundationH264Encoder {
    pub fn new(width: u32, height: u32, bitrate: u32, frames_per_second: u32) -> Result<Self> {
        if width == 0 || height == 0 || width % 2 != 0 || height % 2 != 0 {
            return Err(DcError::InvalidInput(
                "Media Foundation H.264 dimensions must be non-zero and even".into(),
            ));
        }
        if bitrate == 0 || frames_per_second == 0 {
            return Err(DcError::InvalidInput(
                "Media Foundation H.264 bitrate and frame rate must be non-zero".into(),
            ));
        }
        ensure_media_foundation()?;
        let (transform, hardware_accelerated) = activate_encoder()?;
        configure_transform(&transform, width, height, bitrate, frames_per_second)?;
        Ok(Self {
            transform,
            width,
            height,
            frames_per_second,
            first_output: true,
            hardware_accelerated,
        })
    }

    pub fn backend_name(&self) -> &'static str {
        "windows-media-foundation-h264"
    }
}

impl VideoEncoder for WindowsMediaFoundationH264Encoder {
    fn codec(&self) -> VideoCodec {
        VideoCodec::H264
    }

    fn capabilities(&self) -> EncoderCapabilities {
        EncoderCapabilities {
            backend: self.backend_name(),
            codec: VideoCodec::H264,
            hardware_accelerated: self.hardware_accelerated,
            zero_copy_input: false,
            low_latency: true,
            supports_force_keyframe: false,
        }
    }

    fn encode(&mut self, frame: VideoFrame) -> Result<EncodeOutcome> {
        if frame.layout().size().width() != self.width
            || frame.layout().size().height() != self.height
        {
            return Err(DcError::InvalidInput(
                "Media Foundation encoder received a frame with changing dimensions".into(),
            ));
        }
        if frame.layout().pixel_format() != PixelFormat::Bgra32 {
            return Err(DcError::Unsupported(
                "Media Foundation encoder currently accepts BGRA32 frames only".into(),
            ));
        }

        let nv12 = bgra_to_nv12(&frame, self.width, self.height)?;
        let buffer = unsafe {
            MFCreateMemoryBuffer(u32::try_from(nv12.len()).map_err(|_| {
                DcError::InvalidInput("NV12 frame exceeds Media Foundation buffer size".into())
            })?)
        }
        .map_err(mf_error)?;
        copy_into_buffer(&buffer, &nv12)?;
        let sample = unsafe { MFCreateSample() }.map_err(mf_error)?;
        unsafe {
            sample.AddBuffer(&buffer).map_err(mf_error)?;
            sample
                .SetSampleTime(duration_to_hns(frame.timestamp())?)
                .map_err(mf_error)?;
            sample
                .SetSampleDuration(frame_duration_hns(self.frames_per_second)?)
                .map_err(mf_error)?;
            self.transform
                .ProcessInput(0, &sample, 0)
                .map_err(mf_error)?;
        }

        let output_info = unsafe { self.transform.GetOutputStreamInfo(0) }.map_err(mf_error)?;
        let output_size = output_info
            .cbSize
            .max(self.width.saturating_mul(self.height).saturating_mul(4));
        let output_buffer = unsafe { MFCreateMemoryBuffer(output_size) }.map_err(mf_error)?;
        let output_sample = unsafe { MFCreateSample() }.map_err(mf_error)?;
        unsafe {
            output_sample.AddBuffer(&output_buffer).map_err(mf_error)?;
        }
        let mut output = [MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: 0,
            pSample: ManuallyDrop::new(Some(output_sample)),
            dwStatus: 0,
            pEvents: ManuallyDrop::new(None),
        }];
        let mut status = 0;
        let process_result = unsafe { self.transform.ProcessOutput(0, &mut output, &mut status) };
        if let Err(error) = process_result {
            if error.code() == MF_E_TRANSFORM_NEED_MORE_INPUT {
                return Ok(EncodeOutcome::Skipped);
            }
            return Err(mf_error(error));
        }

        let sample = unsafe { ManuallyDrop::take(&mut output[0].pSample) }
            .ok_or_else(|| DcError::Codec("Media Foundation produced no output sample".into()))?;
        let encoded_buffer = unsafe { sample.ConvertToContiguousBuffer() }.map_err(mf_error)?;
        let mut pointer = std::ptr::null_mut();
        let mut current_length = 0;
        unsafe {
            encoded_buffer
                .Lock(&mut pointer, None, Some(&mut current_length))
                .map_err(mf_error)?;
        }
        if pointer.is_null() {
            unsafe { encoded_buffer.Unlock().map_err(mf_error)? };
            return Err(DcError::Codec(
                "Media Foundation returned a null output buffer".into(),
            ));
        }
        let data = unsafe { std::slice::from_raw_parts(pointer, current_length as usize).to_vec() };
        unsafe { encoded_buffer.Unlock().map_err(mf_error)? };
        if data.is_empty() {
            return Ok(EncodeOutcome::Skipped);
        }
        let keyframe = self.first_output;
        self.first_output = false;
        let layout = FrameLayout::packed(frame.layout().size(), PixelFormat::Bgra32)?;
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

impl Drop for WindowsMediaFoundationH264Encoder {
    fn drop(&mut self) {
        let _ = unsafe {
            self.transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0)
        };
    }
}

fn ensure_media_foundation() -> Result<()> {
    static STARTED: OnceLock<std::result::Result<(), String>> = OnceLock::new();
    match STARTED
        .get_or_init(|| unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL).map_err(|e| e.to_string()) })
    {
        Ok(()) => Ok(()),
        Err(error) => Err(DcError::Platform(format!(
            "Media Foundation startup failed: {error}"
        ))),
    }
}

fn activate_encoder() -> Result<(IMFTransform, bool)> {
    let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0;
    let hardware_result = unsafe {
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            MFT_ENUM_FLAG_HARDWARE,
            None,
            Some(&MFT_REGISTER_TYPE_INFO {
                guidMajorType: MFMediaType_Video,
                guidSubtype: MFVideoFormat_H264,
            }),
            &mut activates,
            &mut count,
        )
    };
    let hardware_accelerated = hardware_result.is_ok() && count != 0;
    if !hardware_accelerated {
        if !activates.is_null() {
            unsafe { CoTaskMemFree(Some(activates.cast())) };
        }
        activates = std::ptr::null_mut();
        count = 0;
        unsafe {
            MFTEnumEx(
                MFT_CATEGORY_VIDEO_ENCODER,
                MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_LOCALMFT,
                None,
                Some(&MFT_REGISTER_TYPE_INFO {
                    guidMajorType: MFMediaType_Video,
                    guidSubtype: MFVideoFormat_H264,
                }),
                &mut activates,
                &mut count,
            )
        }
        .map_err(mf_error)?;
    }
    if count == 0 || activates.is_null() {
        return Err(DcError::Unsupported(
            "Windows has no Media Foundation H.264 encoder".into(),
        ));
    }
    let activation = unsafe { (*activates).clone() }.ok_or_else(|| {
        DcError::Platform("Media Foundation returned an empty encoder activation".into())
    })?;
    unsafe { CoTaskMemFree(Some(activates.cast())) };
    let transform = unsafe { activation.ActivateObject::<IMFTransform>() }.map_err(mf_error)?;
    Ok((transform, hardware_accelerated))
}

fn configure_transform(
    transform: &IMFTransform,
    width: u32,
    height: u32,
    bitrate: u32,
    frames_per_second: u32,
) -> Result<()> {
    let attributes = unsafe { transform.GetAttributes() }.map_err(mf_error)?;
    unsafe {
        attributes.SetUINT32(&MF_LOW_LATENCY, 1).map_err(mf_error)?;
    }
    let input = media_type(
        MFVideoFormat_NV12,
        width,
        height,
        bitrate,
        frames_per_second,
    )?;
    let output = media_type(
        MFVideoFormat_H264,
        width,
        height,
        bitrate,
        frames_per_second,
    )?;
    unsafe {
        transform.SetInputType(0, &input, 0).map_err(mf_error)?;
        transform.SetOutputType(0, &output, 0).map_err(mf_error)?;
        transform
            .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
            .map_err(mf_error)?;
        transform
            .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
            .map_err(mf_error)?;
    }
    Ok(())
}

fn media_type(
    subtype: GUID,
    width: u32,
    height: u32,
    bitrate: u32,
    frames_per_second: u32,
) -> Result<IMFMediaType> {
    let media_type = unsafe { MFCreateMediaType() }.map_err(mf_error)?;
    unsafe {
        media_type
            .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
            .map_err(mf_error)?;
        media_type
            .SetGUID(&MF_MT_SUBTYPE, &subtype)
            .map_err(mf_error)?;
        media_type
            .SetUINT64(
                &MF_MT_FRAME_SIZE,
                u64::from(width) << 32 | u64::from(height),
            )
            .map_err(mf_error)?;
        media_type
            .SetUINT64(&MF_MT_FRAME_RATE, u64::from(frames_per_second) << 32 | 1)
            .map_err(mf_error)?;
        media_type
            .SetUINT32(&MF_MT_AVG_BITRATE, bitrate)
            .map_err(mf_error)?;
        media_type
            .SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
            .map_err(mf_error)?;
    }
    Ok(media_type)
}

fn copy_into_buffer(buffer: &IMFMediaBuffer, bytes: &[u8]) -> Result<()> {
    let mut pointer = std::ptr::null_mut();
    unsafe {
        buffer.Lock(&mut pointer, None, None).map_err(mf_error)?;
    }
    if pointer.is_null() {
        unsafe { buffer.Unlock().map_err(mf_error)? };
        return Err(DcError::Codec(
            "Media Foundation returned a null input buffer".into(),
        ));
    }
    unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), pointer, bytes.len()) };
    unsafe {
        buffer.Unlock().map_err(mf_error)?;
        buffer
            .SetCurrentLength(u32::try_from(bytes.len()).map_err(|_| {
                DcError::InvalidInput("Media Foundation input buffer is too large".into())
            })?)
            .map_err(mf_error)?;
    }
    Ok(())
}

fn bgra_to_nv12(frame: &VideoFrame, width: u32, height: u32) -> Result<Vec<u8>> {
    let width = width as usize;
    let height = height as usize;
    let y_len = width * height;
    let mut output = vec![0_u8; y_len + y_len / 2];
    for y in 0..height {
        let row = &frame.data()[y * frame.layout().stride()..];
        for x in 0..width {
            let pixel = &row[x * 4..x * 4 + 4];
            output[y * width + x] = rgb_to_y(pixel[2], pixel[1], pixel[0]);
        }
    }
    let uv_start = y_len;
    for y in (0..height).step_by(2) {
        let row = &frame.data()[y * frame.layout().stride()..];
        for x in (0..width).step_by(2) {
            let pixel = &row[x * 4..x * 4 + 4];
            let uv = uv_start + (y / 2) * width + x;
            output[uv] = rgb_to_u(pixel[2], pixel[1], pixel[0]);
            output[uv + 1] = rgb_to_v(pixel[2], pixel[1], pixel[0]);
        }
    }
    Ok(output)
}

fn rgb_to_y(r: u8, g: u8, b: u8) -> u8 {
    ((66 * i32::from(r) + 129 * i32::from(g) + 25 * i32::from(b) + 128) / 256 + 16).clamp(0, 255)
        as u8
}

fn rgb_to_u(r: u8, g: u8, b: u8) -> u8 {
    ((-38 * i32::from(r) - 74 * i32::from(g) + 112 * i32::from(b) + 128) / 256 + 128).clamp(0, 255)
        as u8
}

fn rgb_to_v(r: u8, g: u8, b: u8) -> u8 {
    ((112 * i32::from(r) - 94 * i32::from(g) - 18 * i32::from(b) + 128) / 256 + 128).clamp(0, 255)
        as u8
}

fn duration_to_hns(duration: std::time::Duration) -> Result<i64> {
    let value = duration.as_nanos() / 100;
    i64::try_from(value)
        .map_err(|_| DcError::InvalidInput("frame timestamp exceeds Media Foundation range".into()))
}

fn frame_duration_hns(fps: u32) -> Result<i64> {
    i64::try_from(HNS_PER_SECOND / u64::from(fps))
        .map_err(|_| DcError::InvalidInput("frame duration exceeds Media Foundation range".into()))
}

fn mf_error(error: windows::core::Error) -> DcError {
    DcError::Platform(format!(
        "Media Foundation operation failed ({:#010x}): {}",
        error.code().0 as u32,
        error.message()
    ))
}
