//! Windows Media Foundation H.264 decoder.
//!
//! The decoder prefers a hardware MFT and asks it for NV12 output.  The
//! decoded surface is copied once into the cross-platform BGRA frame model;
//! keeping the MFT behind the VideoDecoder trait leaves the UI independent of
//! Windows-specific media types.

use dc_common::{DcError, Result};
use dc_media::{
    DecoderCapabilities, EncodedVideoPacket, FrameLayout, FrameSize, PixelFormat, VideoCodec,
    VideoDecoder, VideoFrame,
};
use std::mem::ManuallyDrop;
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use windows::core::{Interface, GUID};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::CoTaskMemFree;

pub struct WindowsMediaFoundationH264Decoder {
    transform: IMFTransform,
    width: u32,
    height: u32,
    hardware_accelerated: bool,
    asynchronous: bool,
    events: Option<IMFMediaEventGenerator>,
}

impl WindowsMediaFoundationH264Decoder {
    pub fn new(width: u32, height: u32) -> Result<Self> {
        if width == 0 || height == 0 || width % 2 != 0 || height % 2 != 0 {
            return Err(DcError::InvalidInput(
                "Media Foundation H.264 dimensions must be non-zero and even".into(),
            ));
        }
        ensure_media_foundation()?;
        let (transform, hardware_accelerated) = activate_decoder()?;
        let asynchronous = configure_decoder(&transform, width, height)?;
        let events = if asynchronous {
            Some(
                transform
                    .cast::<IMFMediaEventGenerator>()
                    .map_err(|error| {
                        mf_stage_error("query async Media Foundation decoder events", error)
                    })?,
            )
        } else {
            None
        };
        Ok(Self {
            transform,
            width,
            height,
            hardware_accelerated,
            asynchronous,
            events,
        })
    }

    pub const fn backend_name(&self) -> &'static str {
        "windows-media-foundation-h264-decoder"
    }

    fn process_output(&mut self, packet: &EncodedVideoPacket) -> Result<VideoFrame> {
        let output_info = unsafe { self.transform.GetOutputStreamInfo(0) }
            .map_err(|error| mf_stage_error("get Media Foundation decoder output info", error))?;
        let provides_samples =
            output_info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0;
        let output_sample = if provides_samples {
            None
        } else {
            let output_size = self
                .width
                .checked_mul(self.height)
                .and_then(|pixels| pixels.checked_mul(3))
                .map(|bytes| bytes / 2)
                .ok_or_else(|| DcError::InvalidInput("NV12 decoder output size overflow".into()))?;
            let output_buffer = unsafe { MFCreateMemoryBuffer(output_size) }.map_err(|error| {
                mf_stage_error("create Media Foundation decoder output buffer", error)
            })?;
            let output_sample = unsafe { MFCreateSample() }.map_err(|error| {
                mf_stage_error("create Media Foundation decoder output sample", error)
            })?;
            unsafe {
                output_sample.AddBuffer(&output_buffer).map_err(|error| {
                    mf_stage_error("attach Media Foundation decoder output buffer", error)
                })?;
            }
            Some(output_sample)
        };
        let mut output = [MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: 0,
            pSample: ManuallyDrop::new(output_sample),
            dwStatus: 0,
            pEvents: ManuallyDrop::new(None),
        }];
        let mut status = 0;
        let process_result = unsafe { self.transform.ProcessOutput(0, &mut output, &mut status) };
        let returned_events = unsafe { ManuallyDrop::take(&mut output[0].pEvents) };
        drop(returned_events);
        let returned_sample = unsafe { ManuallyDrop::take(&mut output[0].pSample) };
        if let Err(error) = process_result {
            drop(returned_sample);
            if error.code() == MF_E_TRANSFORM_NEED_MORE_INPUT {
                return Err(DcError::Codec(
                    "Media Foundation decoder produced no frame for H.264 input".into(),
                ));
            }
            return Err(mf_stage_error(
                "produce Media Foundation decoder output",
                error,
            ));
        }
        let sample = returned_sample
            .ok_or_else(|| DcError::Codec("Media Foundation decoder produced no sample".into()))?;
        let buffer = unsafe { sample.ConvertToContiguousBuffer() }
            .map_err(|error| mf_stage_error("read Media Foundation decoded buffer", error))?;
        let mut pointer = std::ptr::null_mut();
        let mut current_length = 0;
        unsafe {
            buffer
                .Lock(&mut pointer, None, Some(&mut current_length))
                .map_err(|error| mf_stage_error("lock Media Foundation decoded buffer", error))?;
        }
        if pointer.is_null() {
            unsafe {
                buffer.Unlock().map_err(|error| {
                    mf_stage_error("unlock Media Foundation decoded buffer", error)
                })?
            };
            return Err(DcError::Codec(
                "Media Foundation returned a null decoded buffer".into(),
            ));
        }
        let bytes = unsafe { std::slice::from_raw_parts(pointer, current_length as usize) };
        let frame = nv12_to_frame(packet, self.width, self.height, bytes);
        let unlock_result = unsafe { buffer.Unlock() };
        unlock_result
            .map_err(|error| mf_stage_error("unlock Media Foundation decoded buffer", error))?;
        frame
    }
}

impl VideoDecoder for WindowsMediaFoundationH264Decoder {
    fn codec(&self) -> VideoCodec {
        VideoCodec::H264
    }

    fn capabilities(&self) -> DecoderCapabilities {
        DecoderCapabilities {
            backend: self.backend_name(),
            codec: VideoCodec::H264,
            hardware_accelerated: self.hardware_accelerated,
            zero_copy_output: false,
            low_latency: true,
        }
    }

    fn decode(&mut self, packet: EncodedVideoPacket) -> Result<VideoFrame> {
        if packet.codec() != VideoCodec::H264 {
            return Err(DcError::InvalidInput(
                "Media Foundation decoder received non-H.264 data".into(),
            ));
        }
        if packet.source_layout().size().width() != self.width
            || packet.source_layout().size().height() != self.height
        {
            return Err(DcError::InvalidInput(
                "Media Foundation decoder received changing dimensions".into(),
            ));
        }
        let sample_data = annex_b_sample(packet.data())?;
        let buffer = unsafe {
            MFCreateMemoryBuffer(
                u32::try_from(sample_data.len())
                    .map_err(|_| DcError::InvalidInput("H.264 sample is too large".into()))?,
            )
        }
        .map_err(|error| mf_stage_error("create Media Foundation decoder input buffer", error))?;
        copy_into_buffer(&buffer, &sample_data)?;
        let sample = unsafe { MFCreateSample() }.map_err(|error| {
            mf_stage_error("create Media Foundation decoder input sample", error)
        })?;
        unsafe {
            sample.AddBuffer(&buffer).map_err(|error| {
                mf_stage_error("attach Media Foundation decoder input buffer", error)
            })?;
            sample
                .SetSampleTime(duration_to_hns(packet.timestamp())?)
                .map_err(|error| {
                    mf_stage_error("set Media Foundation decoder sample time", error)
                })?;
            self.transform
                .ProcessInput(0, &sample, 0)
                .map_err(|error| mf_stage_error("submit Media Foundation decoder input", error))?;
        }
        if self.asynchronous {
            let events = self.events.as_ref().ok_or_else(|| {
                DcError::Platform("async Media Foundation decoder has no event generator".into())
            })?;
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                if Instant::now() >= deadline {
                    return Err(DcError::Platform(
                        "timed out waiting for Media Foundation decoder output".into(),
                    ));
                }
                match unsafe { events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                    Ok(event) => {
                        let event_type = unsafe { event.GetType() }.map_err(|error| {
                            mf_stage_error("read Media Foundation decoder event type", error)
                        })?;
                        if event_type == METransformHaveOutput.0 as u32 {
                            return self.process_output(&packet);
                        }
                        if event_type == MEError.0 as u32 {
                            let status = unsafe { event.GetStatus() }.map_err(|error| {
                                mf_stage_error("read Media Foundation decoder error", error)
                            })?;
                            return Err(mf_stage_error(
                                "Media Foundation decoder error event",
                                status.into(),
                            ));
                        }
                    }
                    Err(error) if error.code() == MF_E_NO_EVENTS_AVAILABLE => {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    Err(error) => {
                        return Err(mf_stage_error("read Media Foundation decoder event", error))
                    }
                }
            }
        }
        self.process_output(&packet)
    }
}

fn activate_decoder() -> Result<(IMFTransform, bool)> {
    let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0;
    let hardware_result = unsafe {
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_DECODER,
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
    let hardware = hardware_result.is_ok() && count != 0;
    if !hardware {
        if !activates.is_null() {
            unsafe { CoTaskMemFree(Some(activates.cast())) };
        }
        activates = std::ptr::null_mut();
        count = 0;
        unsafe {
            MFTEnumEx(
                MFT_CATEGORY_VIDEO_DECODER,
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
            "Windows has no Media Foundation H.264 decoder".into(),
        ));
    }
    let activation = unsafe { (*activates).clone() }.ok_or_else(|| {
        DcError::Platform("Media Foundation returned an empty decoder activation".into())
    })?;
    unsafe { CoTaskMemFree(Some(activates.cast())) };
    let transform = unsafe { activation.ActivateObject::<IMFTransform>() }.map_err(mf_error)?;
    Ok((transform, hardware))
}

fn configure_decoder(transform: &IMFTransform, width: u32, height: u32) -> Result<bool> {
    let attributes = unsafe { transform.GetAttributes() }
        .map_err(|error| mf_stage_error("get Media Foundation decoder attributes", error))?;
    let asynchronous = unsafe { attributes.GetUINT32(&MF_TRANSFORM_ASYNC) }
        .map(|value| value != 0)
        .unwrap_or(false);
    unsafe {
        attributes.SetUINT32(&MF_LOW_LATENCY, 1).map_err(|error| {
            mf_stage_error("set Media Foundation decoder low-latency mode", error)
        })?;
        if asynchronous {
            attributes
                .SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)
                .map_err(|error| mf_stage_error("unlock async Media Foundation decoder", error))?;
        }
    }
    let input = video_media_type(MFVideoFormat_H264, width, height)?;
    let output = video_media_type(MFVideoFormat_NV12, width, height)?;
    unsafe {
        transform
            .SetInputType(0, &input, 0)
            .map_err(|error| mf_stage_error("set Media Foundation decoder input type", error))?;
        transform
            .SetOutputType(0, &output, 0)
            .map_err(|error| mf_stage_error("set Media Foundation decoder output type", error))?;
        transform
            .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
            .map_err(|error| mf_stage_error("begin Media Foundation decoder streaming", error))?;
        transform
            .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
            .map_err(|error| mf_stage_error("start Media Foundation decoder stream", error))?;
    }
    Ok(asynchronous)
}

fn video_media_type(subtype: GUID, width: u32, height: u32) -> Result<IMFMediaType> {
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
            .SetUINT64(&MF_MT_FRAME_RATE, 30_u64 << 32 | 1)
            .map_err(mf_error)?;
        media_type
            .SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, 1_u64 << 32 | 1)
            .map_err(mf_error)?;
        media_type
            .SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
            .map_err(mf_error)?;
    }
    Ok(media_type)
}

fn annex_b_sample(data: &[u8]) -> Result<Vec<u8>> {
    let nals = dc_media::split_h264_nal_units(data)?;
    let mut output = Vec::with_capacity(data.len() + nals.len() * 4);
    for nal in nals {
        output.extend_from_slice(&[0, 0, 0, 1]);
        output.extend_from_slice(nal);
    }
    Ok(output)
}

fn copy_into_buffer(buffer: &IMFMediaBuffer, bytes: &[u8]) -> Result<()> {
    let mut pointer = std::ptr::null_mut();
    unsafe { buffer.Lock(&mut pointer, None, None).map_err(mf_error)? };
    if pointer.is_null() {
        unsafe { buffer.Unlock().map_err(mf_error)? };
        return Err(DcError::Codec(
            "Media Foundation returned a null decoder input buffer".into(),
        ));
    }
    unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), pointer, bytes.len()) };
    unsafe {
        buffer.Unlock().map_err(mf_error)?;
        buffer
            .SetCurrentLength(
                u32::try_from(bytes.len())
                    .map_err(|_| DcError::InvalidInput("decoder input is too large".into()))?,
            )
            .map_err(mf_error)?;
    }
    Ok(())
}

fn nv12_to_frame(
    packet: &EncodedVideoPacket,
    width: u32,
    height: u32,
    bytes: &[u8],
) -> Result<VideoFrame> {
    let width = width as usize;
    let height = height as usize;
    let y_len = width
        .checked_mul(height)
        .ok_or_else(|| DcError::InvalidInput("NV12 luma size overflow".into()))?;
    let required = y_len
        .checked_add(y_len / 2)
        .ok_or_else(|| DcError::InvalidInput("NV12 frame size overflow".into()))?;
    if bytes.len() < required {
        return Err(DcError::Codec(
            "Media Foundation returned a truncated NV12 frame".into(),
        ));
    }
    let size = FrameSize::new(width as u32, height as u32)?;
    let layout = FrameLayout::packed(size, PixelFormat::Bgra32)?;
    let mut output = vec![0_u8; layout.data_len()];
    let uv = &bytes[y_len..required];
    for y in 0..height {
        for x in 0..width {
            let y_value = i32::from(bytes[y * width + x]) - 16;
            let uv_index = (y / 2) * width + (x / 2) * 2;
            let u = i32::from(uv[uv_index]) - 128;
            let v = i32::from(uv[uv_index + 1]) - 128;
            let c = 298 * y_value;
            let r = ((c + 409 * v + 128) / 256).clamp(0, 255) as u8;
            let g = ((c - 100 * u - 208 * v + 128) / 256).clamp(0, 255) as u8;
            let b = ((c + 516 * u + 128) / 256).clamp(0, 255) as u8;
            let offset = (y * width + x) * 4;
            output[offset..offset + 4].copy_from_slice(&[b, g, r, 255]);
        }
    }
    VideoFrame::new(packet.sequence(), packet.timestamp(), layout, output)
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

fn duration_to_hns(duration: Duration) -> Result<i64> {
    i64::try_from(duration.as_nanos() / 100)
        .map_err(|_| DcError::InvalidInput("timestamp exceeds Media Foundation range".into()))
}

fn mf_error(error: windows::core::Error) -> DcError {
    DcError::Platform(format!(
        "Media Foundation operation failed ({:#010x}): {}",
        error.code().0 as u32,
        error.message()
    ))
}

fn mf_stage_error(stage: &str, error: windows::core::Error) -> DcError {
    DcError::Platform(format!(
        "Media Foundation {stage} failed ({:#010x}): {}",
        error.code().0 as u32,
        error.message()
    ))
}
