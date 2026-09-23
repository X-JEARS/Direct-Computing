//! Windows Media Foundation H.264 MFT backend.
//!
//! The backend intentionally accepts CPU BGRA frames for now. This gives the
//! application a vendor-neutral Windows encoder path while keeping the future
//! D3D11 zero-copy and NVENC/AMF/QSV implementations behind the same trait.

use dc_common::{log, DcError, LogLevel, Result};
use dc_media::{
    h264_access_unit_is_keyframe, EncodeOutcome, EncodedVideoPacket, EncoderCapabilities,
    FrameLayout, PixelFormat, VideoCodec, VideoEncoder, VideoFrame,
};
use std::mem::ManuallyDrop;
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use windows::core::{Interface, GUID, PWSTR};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Variant::{VARIANT, VARIANT_0, VARIANT_0_0, VARIANT_0_0_0, VT_UI4};

const HNS_PER_SECOND: u64 = 10_000_000;

pub struct WindowsMediaFoundationH264Encoder {
    transform: IMFTransform,
    width: u32,
    height: u32,
    frames_per_second: u32,
    first_output: bool,
    hardware_accelerated: bool,
    asynchronous: bool,
    events: Option<IMFMediaEventGenerator>,
    supports_force_keyframe: bool,
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
        let (asynchronous, supports_force_keyframe) =
            configure_transform(&transform, width, height, bitrate, frames_per_second)?;
        let events = if asynchronous {
            Some(
                transform
                    .cast::<IMFMediaEventGenerator>()
                    .map_err(|error| {
                        mf_stage_error("query async Media Foundation events", error)
                    })?,
            )
        } else {
            None
        };
        Ok(Self {
            transform,
            width,
            height,
            frames_per_second,
            first_output: true,
            hardware_accelerated,
            asynchronous,
            events,
            supports_force_keyframe,
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
            supports_force_keyframe: self.supports_force_keyframe,
        }
    }

    fn force_keyframe(&mut self) -> Result<()> {
        let codec_api = self.transform.cast::<ICodecAPI>().map_err(|_| {
            DcError::Unsupported("Media Foundation encoder has no ICodecAPI".into())
        })?;
        set_codec_u32(&codec_api, &CODECAPI_AVEncVideoForceKeyFrame, 1).map_err(|error| {
            DcError::Unsupported(format!(
                "Media Foundation encoder rejected force-keyframe control: {error}"
            ))
        })
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
        .map_err(|error| mf_stage_error("create Media Foundation input buffer", error))?;
        copy_into_buffer(&buffer, &nv12).map_err(|error| {
            DcError::Platform(format!(
                "Media Foundation fill input buffer failed: {error}"
            ))
        })?;
        let sample = unsafe { MFCreateSample() }
            .map_err(|error| mf_stage_error("create Media Foundation input sample", error))?;
        unsafe {
            sample
                .AddBuffer(&buffer)
                .map_err(|error| mf_stage_error("attach Media Foundation input buffer", error))?;
            sample
                .SetSampleTime(duration_to_hns(frame.timestamp())?)
                .map_err(|error| mf_stage_error("set Media Foundation sample time", error))?;
            sample
                .SetSampleDuration(frame_duration_hns(self.frames_per_second)?)
                .map_err(|error| mf_stage_error("set Media Foundation sample duration", error))?;
            self.transform
                .ProcessInput(0, &sample, 0)
                .map_err(|error| mf_stage_error("submit Media Foundation input", error))?;
        }

        if self.asynchronous {
            return self.process_async_output(frame);
        }

        self.process_output(frame)
    }
}

impl WindowsMediaFoundationH264Encoder {
    fn process_async_output(&mut self, frame: VideoFrame) -> Result<EncodeOutcome> {
        let events = self.events.as_ref().ok_or_else(|| {
            DcError::Platform("async Media Foundation encoder has no event generator".into())
        })?;
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if Instant::now() >= deadline {
                return Err(DcError::Platform(
                    "timed out waiting for Media Foundation encoder output".into(),
                ));
            }
            let event = match unsafe { events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                Ok(event) => event,
                Err(error) if error.code() == MF_E_NO_EVENTS_AVAILABLE => {
                    std::thread::sleep(Duration::from_millis(1));
                    continue;
                }
                Err(error) => {
                    return Err(mf_stage_error("read Media Foundation encoder event", error))
                }
            };
            let event_type = unsafe { event.GetType() }
                .map_err(|error| mf_stage_error("read Media Foundation event type", error))?;
            if event_type == METransformHaveOutput.0 as u32 {
                return self.process_output(frame);
            }
            if event_type == MEError.0 as u32 {
                let status = unsafe { event.GetStatus() }
                    .map_err(|error| mf_stage_error("read Media Foundation error event", error))?;
                return Err(mf_stage_error(
                    "Media Foundation encoder error event",
                    status.into(),
                ));
            }
        }
    }

    fn process_output(&mut self, frame: VideoFrame) -> Result<EncodeOutcome> {
        let output_info = unsafe { self.transform.GetOutputStreamInfo(0) }
            .map_err(|error| mf_stage_error("get Media Foundation output stream info", error))?;
        let provides_samples =
            output_info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0;
        let output_sample = if provides_samples {
            None
        } else {
            let output_size = output_info
                .cbSize
                .max(self.width.saturating_mul(self.height).saturating_mul(2));
            let output_buffer = unsafe { MFCreateMemoryBuffer(output_size) }
                .map_err(|error| mf_stage_error("create Media Foundation output buffer", error))?;
            let output_sample = unsafe { MFCreateSample() }
                .map_err(|error| mf_stage_error("create Media Foundation output sample", error))?;
            unsafe {
                output_sample.AddBuffer(&output_buffer).map_err(|error| {
                    mf_stage_error("attach Media Foundation output buffer", error)
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
        // MFTs may replace either field, including on an error path. Both
        // fields are ManuallyDrop in the Windows projection, so take them
        // explicitly on every path to release returned COM objects.
        let returned_events = unsafe { ManuallyDrop::take(&mut output[0].pEvents) };
        drop(returned_events);
        let returned_sample = unsafe { ManuallyDrop::take(&mut output[0].pSample) };
        if let Err(error) = process_result {
            drop(returned_sample);
            if error.code() == MF_E_TRANSFORM_NEED_MORE_INPUT {
                return Ok(EncodeOutcome::Skipped);
            }
            return Err(mf_stage_error("produce Media Foundation output", error));
        }

        let sample = returned_sample
            .ok_or_else(|| DcError::Codec("Media Foundation produced no output sample".into()))?;
        let encoded_buffer = unsafe { sample.ConvertToContiguousBuffer() }
            .map_err(|error| mf_stage_error("read Media Foundation encoded buffer", error))?;
        let mut pointer = std::ptr::null_mut();
        let mut current_length = 0;
        unsafe {
            encoded_buffer
                .Lock(&mut pointer, None, Some(&mut current_length))
                .map_err(|error| mf_stage_error("lock Media Foundation encoded buffer", error))?;
        }
        if pointer.is_null() {
            unsafe {
                encoded_buffer.Unlock().map_err(|error| {
                    mf_stage_error("unlock Media Foundation encoded buffer", error)
                })?
            };
            return Err(DcError::Codec(
                "Media Foundation returned a null output buffer".into(),
            ));
        }
        let data = unsafe { std::slice::from_raw_parts(pointer, current_length as usize).to_vec() };
        unsafe {
            encoded_buffer
                .Unlock()
                .map_err(|error| mf_stage_error("unlock Media Foundation encoded buffer", error))?
        };
        if data.is_empty() {
            return Ok(EncodeOutcome::Skipped);
        }
        let keyframe = self.first_output || h264_access_unit_is_keyframe(&data)?;
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
    let friendly_name = activation_friendly_name(&activation)
        .unwrap_or_else(|| "unknown Media Foundation encoder".into());
    log(
        LogLevel::Info,
        "dc-platform::media-foundation",
        &format!("selected MFT encoder name={friendly_name:?} hardware={hardware_accelerated}"),
    );
    let transform = unsafe { activation.ActivateObject::<IMFTransform>() }.map_err(mf_error)?;
    Ok((transform, hardware_accelerated))
}

fn activation_friendly_name(activation: &IMFActivate) -> Option<String> {
    let mut pointer = PWSTR::null();
    let mut length = 0_u32;
    if unsafe {
        activation.GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut pointer, &mut length)
    }
    .is_err()
        || pointer.is_null()
    {
        return None;
    }
    let name =
        unsafe { String::from_utf16_lossy(std::slice::from_raw_parts(pointer.0, length as usize)) };
    unsafe { CoTaskMemFree(Some(pointer.0.cast())) };
    Some(name)
}

fn configure_transform(
    transform: &IMFTransform,
    width: u32,
    height: u32,
    bitrate: u32,
    frames_per_second: u32,
) -> Result<(bool, bool)> {
    let attributes = unsafe { transform.GetAttributes() }
        .map_err(|error| mf_stage_error("get Media Foundation transform attributes", error))?;
    let asynchronous = unsafe { attributes.GetUINT32(&MF_TRANSFORM_ASYNC) }
        .map(|value| value != 0)
        .unwrap_or(false);
    unsafe {
        attributes
            .SetUINT32(&MF_LOW_LATENCY, 1)
            .map_err(|error| mf_stage_error("set Media Foundation low-latency mode", error))?;
        if asynchronous {
            attributes
                .SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)
                .map_err(|error| {
                    mf_stage_error("unlock async Media Foundation transform", error)
                })?;
        }
    }
    // IMFTransform exposes codec-specific controls through ICodecAPI.  These
    // controls are optional across GPU vendors, so apply them best-effort and
    // keep the media-type bitrate as the portable fallback.
    let force_keyframe_before_types =
        configure_codec_api(transform, bitrate, frames_per_second, "before-media-types");
    let input = input_media_type(width, height, frames_per_second)?;
    let output = output_media_type(width, height, bitrate, frames_per_second)?;
    let force_keyframe_after_types;
    unsafe {
        // Some hardware encoders derive their accepted input types from the
        // selected H.264 profile. Configure the output before its dependent
        // NV12 input so those MFTs do not report MF_E_TRANSFORM_TYPE_NOT_SET.
        transform
            .SetOutputType(0, &output, 0)
            .map_err(|error| mf_stage_error("set Media Foundation output type", error))?;
        transform
            .SetInputType(0, &input, 0)
            .map_err(|error| mf_stage_error("set Media Foundation input type", error))?;
        // A number of vendor MFTs only expose ICodecAPI after their media
        // types have been selected. Repeat the best-effort controls here so
        // those encoders receive the same low-bandwidth policy.
        force_keyframe_after_types =
            configure_codec_api(transform, bitrate, frames_per_second, "after-media-types");
        transform
            .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
            .map_err(|error| mf_stage_error("begin Media Foundation streaming", error))?;
        transform
            .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
            .map_err(|error| mf_stage_error("start Media Foundation stream", error))?;
    }
    Ok((
        asynchronous,
        force_keyframe_before_types || force_keyframe_after_types,
    ))
}

fn configure_codec_api(
    transform: &IMFTransform,
    bitrate: u32,
    frames_per_second: u32,
    phase: &str,
) -> bool {
    let Ok(codec_api) = transform.cast::<ICodecAPI>() else {
        log(
            LogLevel::Warn,
            "dc-platform::media-foundation",
            &format!("MFT has no ICodecAPI phase={phase}"),
        );
        return false;
    };
    // Keep the MFT on a short-delay, bounded VBV policy. These properties are
    // optional per encoder, so record every accepted/rejected control instead
    // of silently assuming that a successful MFT activation means CBR works.
    let controls = [
        (
            "rate-control=cbr",
            &CODECAPI_AVEncCommonRateControlMode,
            eAVEncCommonRateControlMode_CBR.0 as u32,
        ),
        ("mean-bitrate", &CODECAPI_AVEncCommonMeanBitRate, bitrate),
        ("max-bitrate", &CODECAPI_AVEncCommonMaxBitRate, bitrate),
        // One second of coded data, expressed in bytes. Encoders that honor
        // this VBV setting can
        // no longer amortize a very large desktop frame over an unbounded
        // interval while claiming to meet the average bitrate.
        (
            "buffer-size",
            &CODECAPI_AVEncCommonBufferSize,
            bitrate.div_ceil(8),
        ),
        ("low-latency", &CODECAPI_AVEncCommonLowLatency, 1),
        ("real-time", &CODECAPI_AVEncCommonRealTime, 1),
        ("allow-frame-drops", &CODECAPI_AVEncCommonAllowFrameDrops, 1),
        ("b-frames", &CODECAPI_AVEncMPVDefaultBPictureCount, 0),
        (
            "display-remoting",
            &CODECAPI_AVScenarioInfo,
            eAVScenarioInfo_DisplayRemoting.0 as u32,
        ),
        ("maximum-qp", &CODECAPI_AVEncVideoMaxQP, 51),
        (
            "keyframe-distance",
            &CODECAPI_AVEncVideoMaxKeyframeDistance,
            frames_per_second.saturating_mul(5),
        ),
    ];
    let mut accepted = Vec::new();
    let mut rejected = Vec::new();
    for (name, key, value) in controls {
        match set_codec_u32(&codec_api, key, value) {
            Ok(()) => accepted.push(format!("{name}:{value}")),
            Err(error) => rejected.push(format!("{name}:{}", error.code())),
        }
    }
    let supports_force_keyframe =
        unsafe { codec_api.IsSupported(&CODECAPI_AVEncVideoForceKeyFrame) }.is_ok();
    log(
        LogLevel::Info,
        "dc-platform::media-foundation",
        &format!(
            "MFT codec controls phase={phase} accepted=[{}] rejected=[{}] force_keyframe={supports_force_keyframe}",
            accepted.join(","),
            rejected.join(",")
        ),
    );
    supports_force_keyframe
}

fn set_codec_u32(codec_api: &ICodecAPI, key: &GUID, value: u32) -> windows::core::Result<()> {
    let variant = VARIANT {
        Anonymous: VARIANT_0 {
            Anonymous: ManuallyDrop::new(VARIANT_0_0 {
                vt: VT_UI4,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: VARIANT_0_0_0 { uintVal: value },
            }),
        },
    };
    unsafe { codec_api.SetValue(key, &variant) }
}

fn video_media_type(
    subtype: GUID,
    width: u32,
    height: u32,
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
            .SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, 1_u64 << 32 | 1)
            .map_err(mf_error)?;
        media_type
            .SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
            .map_err(mf_error)?;
    }
    Ok(media_type)
}

fn input_media_type(width: u32, height: u32, frames_per_second: u32) -> Result<IMFMediaType> {
    let media_type = video_media_type(MFVideoFormat_NV12, width, height, frames_per_second)?;
    let sample_size = width
        .checked_mul(height)
        .and_then(|pixels| pixels.checked_mul(3))
        .map(|bytes| bytes / 2)
        .ok_or_else(|| DcError::InvalidInput("NV12 frame size overflow".into()))?;
    unsafe {
        media_type
            .SetUINT32(&MF_MT_FIXED_SIZE_SAMPLES, 1)
            .map_err(mf_error)?;
        media_type
            .SetUINT32(&MF_MT_ALL_SAMPLES_INDEPENDENT, 1)
            .map_err(mf_error)?;
        media_type
            .SetUINT32(&MF_MT_SAMPLE_SIZE, sample_size)
            .map_err(mf_error)?;
        media_type
            .SetUINT32(&MF_MT_DEFAULT_STRIDE, width)
            .map_err(mf_error)?;
    }
    Ok(media_type)
}

fn output_media_type(
    width: u32,
    height: u32,
    bitrate: u32,
    frames_per_second: u32,
) -> Result<IMFMediaType> {
    let media_type = video_media_type(MFVideoFormat_H264, width, height, frames_per_second)?;
    unsafe {
        media_type
            .SetUINT32(&MF_MT_AVG_BITRATE, bitrate)
            .map_err(mf_error)?;
        media_type
            .SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_High.0 as u32)
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

fn mf_stage_error(stage: &str, error: windows::core::Error) -> DcError {
    DcError::Platform(format!(
        "Media Foundation {stage} failed ({:#010x}): {}",
        error.code().0 as u32,
        error.message()
    ))
}
