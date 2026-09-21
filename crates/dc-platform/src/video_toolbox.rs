//! macOS VideoToolbox H.264 decoder.
//!
//! The decoder accepts Annex-B H.264 packets, converts them to the AVCC sample
//! layout expected by VideoToolbox, and copies the decoded CVPixelBuffer into
//! the existing `VideoFrame` model. BGRA buffers are preserved in their native
//! packed layout so the macOS Metal-backed window can upload them without a
//! full-frame RGB channel-swizzle pass.

// Clippy 1.98 reports the separate framework link attributes below as
// duplicates even though each names a different Apple framework.
#![allow(clippy::duplicated_attributes)]

use dc_common::{DcError, Result};
use dc_media::{
    DecoderCapabilities, EncodedVideoPacket, FrameLayout, PixelFormat, VideoCodec, VideoDecoder,
    VideoFrame,
};
use std::ffi::c_void;
use std::ptr;

type OSStatus = i32;
type CFAllocatorRef = *const c_void;
type CFStringRef = *const c_void;
type CFDictionaryRef = *const c_void;
type CFNumberRef = *const c_void;
type CMBlockBufferRef = *mut c_void;
type CMSampleBufferRef = *mut c_void;
type CMFormatDescriptionRef = *mut c_void;
type CVImageBufferRef = *mut c_void;
type VTDecompressionSessionRef = *mut c_void;

const NO_ERR: OSStatus = 0;
const KCV_PIXEL_FORMAT_TYPE_32_BGRA: u32 = 0x4247_5241;
const KCV_PIXEL_FORMAT_TYPE_420_V: u32 = 0x3432_3076;
const KCV_PIXEL_FORMAT_TYPE_420_F: u32 = 0x3432_3066;

#[repr(C)]
#[derive(Clone, Copy)]
struct CMTime {
    value: i64,
    timescale: i32,
    flags: u32,
    epoch: i64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CMSampleTimingInfo {
    duration: CMTime,
    presentation_time_stamp: CMTime,
    decode_time_stamp: CMTime,
}

#[repr(C)]
struct VTDecompressionOutputCallbackRecord {
    callback: Option<
        extern "C" fn(
            decompression_output_ref_con: *mut c_void,
            source_frame_ref_con: *mut c_void,
            status: OSStatus,
            info_flags: u32,
            image_buffer: CVImageBufferRef,
            presentation_time_stamp: CMTime,
            presentation_duration: CMTime,
        ),
    >,
    ref_con: *mut c_void,
}

#[cfg_attr(
    target_vendor = "apple",
    link(name = "VideoToolbox", kind = "framework")
)]
#[cfg_attr(target_vendor = "apple", link(name = "CoreMedia", kind = "framework"))]
#[cfg_attr(target_vendor = "apple", link(name = "CoreVideo", kind = "framework"))]
#[cfg_attr(
    target_vendor = "apple",
    link(name = "CoreFoundation", kind = "framework")
)]
extern "C" {
    fn CMVideoFormatDescriptionCreateFromH264ParameterSets(
        allocator: CFAllocatorRef,
        parameter_set_count: usize,
        parameter_set_pointers: *const *const u8,
        parameter_set_sizes: *const usize,
        nal_unit_header_length: i32,
        format_description_out: *mut CMFormatDescriptionRef,
    ) -> OSStatus;
    fn CMBlockBufferCreateWithMemoryBlock(
        allocator: CFAllocatorRef,
        memory_block: *mut c_void,
        block_length: usize,
        block_allocator: CFAllocatorRef,
        custom_block_source: *const c_void,
        offset_to_data: usize,
        data_length: usize,
        flags: u32,
        block_buffer_out: *mut CMBlockBufferRef,
    ) -> OSStatus;
    fn CMBlockBufferReplaceDataBytes(
        source_bytes: *const c_void,
        destination_buffer: CMBlockBufferRef,
        offset_into_destination: usize,
        data_length: usize,
    ) -> OSStatus;
    fn CMSampleBufferCreateReady(
        allocator: CFAllocatorRef,
        data_buffer: CMBlockBufferRef,
        format_description: CMFormatDescriptionRef,
        num_samples: isize,
        num_sample_timing_entries: isize,
        sample_timing_array: *const CMSampleTimingInfo,
        num_sample_size_entries: isize,
        sample_size_array: *const usize,
        sample_buffer_out: *mut CMSampleBufferRef,
    ) -> OSStatus;
    fn CFRelease(value: *const c_void);
    fn CFNumberCreate(
        allocator: CFAllocatorRef,
        number_type: isize,
        value_ptr: *const c_void,
    ) -> CFNumberRef;
    fn CFDictionaryCreate(
        allocator: CFAllocatorRef,
        keys: *const *const c_void,
        values: *const *const c_void,
        count: isize,
        key_callbacks: *const c_void,
        value_callbacks: *const c_void,
    ) -> CFDictionaryRef;

    static kCVPixelBufferPixelFormatTypeKey: CFStringRef;

    fn VTDecompressionSessionCreate(
        allocator: CFAllocatorRef,
        format_description: CMFormatDescriptionRef,
        decoder_specification: *const c_void,
        image_buffer_attributes: *const c_void,
        output_callback: *const VTDecompressionOutputCallbackRecord,
        decompression_session_out: *mut VTDecompressionSessionRef,
    ) -> OSStatus;
    fn VTDecompressionSessionInvalidate(session: VTDecompressionSessionRef);
    fn VTDecompressionSessionDecodeFrame(
        session: VTDecompressionSessionRef,
        sample_buffer: CMSampleBufferRef,
        decode_flags: u32,
        source_frame_ref_con: *mut c_void,
        info_flags_out: *mut u32,
    ) -> OSStatus;
    fn VTDecompressionSessionWaitForAsynchronousFrames(
        session: VTDecompressionSessionRef,
    ) -> OSStatus;

    fn CVPixelBufferLockBaseAddress(pixel_buffer: CVImageBufferRef, flags: u64) -> OSStatus;
    fn CVPixelBufferUnlockBaseAddress(pixel_buffer: CVImageBufferRef, flags: u64) -> OSStatus;
    fn CVPixelBufferGetPixelFormatType(pixel_buffer: CVImageBufferRef) -> u32;
    fn CVPixelBufferGetWidth(pixel_buffer: CVImageBufferRef) -> usize;
    fn CVPixelBufferGetHeight(pixel_buffer: CVImageBufferRef) -> usize;
    fn CVPixelBufferGetBytesPerRow(pixel_buffer: CVImageBufferRef) -> usize;
    fn CVPixelBufferGetBaseAddress(pixel_buffer: CVImageBufferRef) -> *mut c_void;
    fn CVPixelBufferGetPlaneCount(pixel_buffer: CVImageBufferRef) -> usize;
    fn CVPixelBufferGetBaseAddressOfPlane(
        pixel_buffer: CVImageBufferRef,
        plane_index: usize,
    ) -> *mut c_void;
    fn CVPixelBufferGetBytesPerRowOfPlane(
        pixel_buffer: CVImageBufferRef,
        plane_index: usize,
    ) -> usize;
}

pub struct VideoToolboxH264Decoder {
    session: VTDecompressionSessionRef,
    format_description: CMFormatDescriptionRef,
    sps: Vec<u8>,
    pps: Vec<u8>,
}

struct DecodeContext {
    sequence: u64,
    timestamp: std::time::Duration,
    source_layout: FrameLayout,
    frame: Option<VideoFrame>,
    error: Option<String>,
}

impl VideoToolboxH264Decoder {
    pub fn new() -> Result<Self> {
        Ok(Self {
            session: ptr::null_mut(),
            format_description: ptr::null_mut(),
            sps: Vec::new(),
            pps: Vec::new(),
        })
    }

    pub const fn backend_name(&self) -> &'static str {
        "apple-videotoolbox-h264"
    }

    fn update_format_description(&mut self, nals: &[Vec<u8>]) -> Result<()> {
        let mut changed = false;
        for nal in nals {
            if nal.is_empty() {
                continue;
            }
            match nal[0] & 0x1f {
                7 if self.sps != *nal => {
                    self.sps = nal.clone();
                    changed = true;
                }
                8 if self.pps != *nal => {
                    self.pps = nal.clone();
                    changed = true;
                }
                _ => {}
            }
        }
        if self.sps.is_empty() || self.pps.is_empty() {
            return Ok(());
        }
        if !changed && !self.format_description.is_null() {
            return Ok(());
        }
        if !self.session.is_null() {
            unsafe {
                VTDecompressionSessionWaitForAsynchronousFrames(self.session);
                VTDecompressionSessionInvalidate(self.session);
            }
            self.session = ptr::null_mut();
        }
        if !self.format_description.is_null() {
            unsafe { CFRelease(self.format_description.cast()) };
            self.format_description = ptr::null_mut();
        }
        let pointers = [self.sps.as_ptr(), self.pps.as_ptr()];
        let sizes = [self.sps.len(), self.pps.len()];
        let mut description = ptr::null_mut();
        let status = unsafe {
            CMVideoFormatDescriptionCreateFromH264ParameterSets(
                ptr::null(),
                pointers.len(),
                pointers.as_ptr(),
                sizes.as_ptr(),
                4,
                &mut description,
            )
        };
        if status != NO_ERR {
            return Err(os_error("create H.264 format description", status));
        }
        self.format_description = description;
        self.recreate_session()
    }

    fn recreate_session(&mut self) -> Result<()> {
        if self.format_description.is_null() {
            return Ok(());
        }
        if !self.session.is_null() {
            unsafe {
                VTDecompressionSessionWaitForAsynchronousFrames(self.session);
                VTDecompressionSessionInvalidate(self.session);
            }
            self.session = ptr::null_mut();
        }
        let callback = VTDecompressionOutputCallbackRecord {
            callback: Some(output_callback),
            ref_con: ptr::null_mut(),
        };
        // Ask VideoToolbox for a packed BGRA surface.  This is the format
        // consumed by the macOS Metal presentation path, and avoids forcing
        // the decoder callback through a CPU NV12 colour conversion.
        let pixel_format = KCV_PIXEL_FORMAT_TYPE_32_BGRA;
        let pixel_format_number = unsafe {
            CFNumberCreate(
                ptr::null(),
                3, // kCFNumberSInt32Type
                (&pixel_format as *const u32).cast(),
            )
        };
        if pixel_format_number.is_null() {
            return Err(DcError::Platform(
                "create VideoToolbox pixel-format attribute failed".into(),
            ));
        }
        let keys = [unsafe { kCVPixelBufferPixelFormatTypeKey }];
        let values = [pixel_format_number];
        let image_buffer_attributes = unsafe {
            CFDictionaryCreate(
                ptr::null(),
                keys.as_ptr().cast(),
                values.as_ptr().cast(),
                1,
                ptr::null(),
                ptr::null(),
            )
        };
        unsafe { CFRelease(pixel_format_number) };
        if image_buffer_attributes.is_null() {
            return Err(DcError::Platform(
                "create VideoToolbox image-buffer attributes failed".into(),
            ));
        }
        let mut session = ptr::null_mut();
        let status = unsafe {
            VTDecompressionSessionCreate(
                ptr::null(),
                self.format_description,
                ptr::null(),
                image_buffer_attributes,
                &callback,
                &mut session,
            )
        };
        unsafe { CFRelease(image_buffer_attributes) };
        if status != NO_ERR {
            return Err(os_error(
                "create VideoToolbox decompression session",
                status,
            ));
        }
        self.session = session;
        Ok(())
    }
}

impl VideoDecoder for VideoToolboxH264Decoder {
    fn codec(&self) -> VideoCodec {
        VideoCodec::H264
    }

    fn capabilities(&self) -> DecoderCapabilities {
        DecoderCapabilities {
            backend: self.backend_name(),
            codec: VideoCodec::H264,
            hardware_accelerated: true,
            zero_copy_output: false,
            low_latency: true,
        }
    }

    fn decode(&mut self, packet: EncodedVideoPacket) -> Result<VideoFrame> {
        if packet.codec() != VideoCodec::H264 {
            return Err(DcError::InvalidInput(
                "VideoToolbox received non-H.264 data".into(),
            ));
        }
        let nals: Vec<Vec<u8>> = dc_media::split_h264_nal_units(packet.data())?
            .into_iter()
            .map(<[u8]>::to_vec)
            .collect();
        self.update_format_description(&nals)?;
        if self.session.is_null() {
            return Err(DcError::Codec(
                "VideoToolbox is waiting for H.264 SPS/PPS".into(),
            ));
        }
        let avcc = annex_b_to_avcc(&nals)?;
        let block_buffer = create_block_buffer(&avcc)?;
        let sample_buffer =
            create_sample_buffer(block_buffer, self.format_description, packet.timestamp())?;
        let mut context = DecodeContext {
            sequence: packet.sequence(),
            timestamp: packet.timestamp(),
            source_layout: packet.source_layout(),
            frame: None,
            error: None,
        };
        let mut info_flags = 0;
        let status = unsafe {
            VTDecompressionSessionDecodeFrame(
                self.session,
                sample_buffer,
                0,
                (&mut context as *mut DecodeContext).cast(),
                &mut info_flags,
            )
        };
        unsafe {
            CFRelease(sample_buffer.cast());
            CFRelease(block_buffer.cast());
        }
        if status != NO_ERR {
            return Err(os_error("decode H.264 frame with VideoToolbox", status));
        }
        let status = unsafe { VTDecompressionSessionWaitForAsynchronousFrames(self.session) };
        if status != NO_ERR {
            return Err(os_error("wait for VideoToolbox decoder", status));
        }
        if let Some(error) = context.error {
            return Err(DcError::Codec(error));
        }
        context
            .frame
            .ok_or_else(|| DcError::Codec("VideoToolbox produced no decoded frame".into()))
    }
}

impl Drop for VideoToolboxH264Decoder {
    fn drop(&mut self) {
        if !self.session.is_null() {
            unsafe {
                VTDecompressionSessionWaitForAsynchronousFrames(self.session);
                VTDecompressionSessionInvalidate(self.session);
            }
        }
        if !self.format_description.is_null() {
            unsafe { CFRelease(self.format_description.cast()) };
        }
    }
}

extern "C" fn output_callback(
    ref_con: *mut c_void,
    source_frame_ref_con: *mut c_void,
    status: OSStatus,
    _info_flags: u32,
    image_buffer: CVImageBufferRef,
    _presentation_time_stamp: CMTime,
    _presentation_duration: CMTime,
) {
    if source_frame_ref_con.is_null() {
        return;
    }
    let context = unsafe { &mut *(source_frame_ref_con.cast::<DecodeContext>()) };
    if status != NO_ERR {
        context.error = Some(format!("VideoToolbox callback failed with status {status}"));
        return;
    }
    let _ = ref_con;
    if image_buffer.is_null() {
        context.error = Some("VideoToolbox callback returned no image buffer".into());
        return;
    }
    match copy_pixel_buffer(
        image_buffer,
        context.sequence,
        context.timestamp,
        context.source_layout,
    ) {
        Ok(frame) => context.frame = Some(frame),
        Err(error) => context.error = Some(error.to_string()),
    }
}

fn copy_pixel_buffer(
    pixel_buffer: CVImageBufferRef,
    sequence: u64,
    timestamp: std::time::Duration,
    source_layout: FrameLayout,
) -> Result<VideoFrame> {
    let status = unsafe { CVPixelBufferLockBaseAddress(pixel_buffer, 0) };
    if status != NO_ERR {
        return Err(os_error("lock VideoToolbox pixel buffer", status));
    }
    let result = (|| {
        let width = unsafe { CVPixelBufferGetWidth(pixel_buffer) } as u32;
        let height = unsafe { CVPixelBufferGetHeight(pixel_buffer) } as u32;
        let size = dc_media::FrameSize::new(width, height)?;
        let format = unsafe { CVPixelBufferGetPixelFormatType(pixel_buffer) };
        let pixel_format = match format {
            KCV_PIXEL_FORMAT_TYPE_32_BGRA => PixelFormat::Bgra32,
            KCV_PIXEL_FORMAT_TYPE_420_V | KCV_PIXEL_FORMAT_TYPE_420_F => PixelFormat::Rgb24,
            _ => {
                return Err(DcError::Unsupported(format!(
                    "unsupported VideoToolbox pixel format 0x{format:08x}"
                )))
            }
        };
        let layout = FrameLayout::packed(size, pixel_format)?;
        let mut data = vec![0_u8; layout.data_len()];
        match format {
            KCV_PIXEL_FORMAT_TYPE_32_BGRA => copy_bgra(
                pixel_buffer,
                width as usize,
                height as usize,
                &mut data,
                layout.stride(),
            )?,
            KCV_PIXEL_FORMAT_TYPE_420_V | KCV_PIXEL_FORMAT_TYPE_420_F => {
                copy_nv12(pixel_buffer, width as usize, height as usize, &mut data)?
            }
            _ => unreachable!("pixel format was validated above"),
        }
        let _ = source_layout;
        VideoFrame::new(sequence, timestamp, layout, data)
    })();
    let unlock_status = unsafe { CVPixelBufferUnlockBaseAddress(pixel_buffer, 0) };
    if unlock_status != NO_ERR {
        return Err(os_error("unlock VideoToolbox pixel buffer", unlock_status));
    }
    result
}

fn copy_bgra(
    pixel_buffer: CVImageBufferRef,
    width: usize,
    height: usize,
    output: &mut [u8],
    output_stride: usize,
) -> Result<()> {
    let source_stride = unsafe { CVPixelBufferGetBytesPerRow(pixel_buffer) };
    let source = unsafe { CVPixelBufferGetBaseAddress(pixel_buffer) };
    if source.is_null() || source_stride < width * 4 {
        return Err(DcError::Codec(
            "VideoToolbox returned an invalid BGRA buffer".into(),
        ));
    }
    if output_stride < width * 4 || output.len() < output_stride.saturating_mul(height) {
        return Err(DcError::Codec(
            "VideoToolbox returned an invalid BGRA destination buffer".into(),
        ));
    }
    for y in 0..height {
        let src = unsafe {
            std::slice::from_raw_parts((source as *const u8).add(y * source_stride), width * 4)
        };
        output[y * output_stride..y * output_stride + width * 4].copy_from_slice(src);
    }
    Ok(())
}

fn copy_nv12(
    pixel_buffer: CVImageBufferRef,
    width: usize,
    height: usize,
    output: &mut [u8],
) -> Result<()> {
    if unsafe { CVPixelBufferGetPlaneCount(pixel_buffer) } < 2 {
        return Err(DcError::Codec(
            "VideoToolbox returned an invalid NV12 buffer".into(),
        ));
    }
    let y_base = unsafe { CVPixelBufferGetBaseAddressOfPlane(pixel_buffer, 0) } as *const u8;
    let uv_base = unsafe { CVPixelBufferGetBaseAddressOfPlane(pixel_buffer, 1) } as *const u8;
    let y_stride = unsafe { CVPixelBufferGetBytesPerRowOfPlane(pixel_buffer, 0) };
    let uv_stride = unsafe { CVPixelBufferGetBytesPerRowOfPlane(pixel_buffer, 1) };
    if y_base.is_null() || uv_base.is_null() || y_stride < width || uv_stride < width {
        return Err(DcError::Codec(
            "VideoToolbox returned invalid NV12 planes".into(),
        ));
    }
    for y in 0..height {
        for x in 0..width {
            let y_value = unsafe { *y_base.add(y * y_stride + x) } as f32;
            let uv = unsafe {
                std::slice::from_raw_parts(uv_base.add((y / 2) * uv_stride + (x / 2) * 2), 2)
            };
            let u = f32::from(uv[0]) - 128.0;
            let v = f32::from(uv[1]) - 128.0;
            let (r, g, b) = (
                1.164 * (y_value - 16.0) + 1.596 * v,
                1.164 * (y_value - 16.0) - 0.392 * u - 0.813 * v,
                1.164 * (y_value - 16.0) + 2.017 * u,
            );
            let offset = (y * width + x) * 3;
            output[offset] = r.clamp(0.0, 255.0) as u8;
            output[offset + 1] = g.clamp(0.0, 255.0) as u8;
            output[offset + 2] = b.clamp(0.0, 255.0) as u8;
        }
    }
    Ok(())
}

fn create_block_buffer(bytes: &[u8]) -> Result<CMBlockBufferRef> {
    let mut block = ptr::null_mut();
    let status = unsafe {
        CMBlockBufferCreateWithMemoryBlock(
            ptr::null(),
            ptr::null_mut(),
            bytes.len(),
            ptr::null(),
            ptr::null(),
            0,
            bytes.len(),
            0,
            &mut block,
        )
    };
    if status != NO_ERR {
        return Err(os_error("create VideoToolbox block buffer", status));
    }
    let status =
        unsafe { CMBlockBufferReplaceDataBytes(bytes.as_ptr().cast(), block, 0, bytes.len()) };
    if status != NO_ERR {
        unsafe { CFRelease(block.cast()) };
        return Err(os_error("copy H.264 data into block buffer", status));
    }
    Ok(block)
}

fn create_sample_buffer(
    block: CMBlockBufferRef,
    format: CMFormatDescriptionRef,
    timestamp: std::time::Duration,
) -> Result<CMSampleBufferRef> {
    let timing = CMSampleTimingInfo {
        duration: CMTime {
            value: 1,
            timescale: 30,
            flags: 1,
            epoch: 0,
        },
        presentation_time_stamp: CMTime {
            value: duration_to_timescale(timestamp, 30),
            timescale: 30,
            flags: 1,
            epoch: 0,
        },
        decode_time_stamp: CMTime {
            value: 0,
            timescale: 30,
            flags: 0,
            epoch: 0,
        },
    };
    let size = unsafe { block_buffer_length(block) };
    let mut sample = ptr::null_mut();
    let status = unsafe {
        CMSampleBufferCreateReady(
            ptr::null(),
            block,
            format,
            1,
            1,
            &timing,
            1,
            &size,
            &mut sample,
        )
    };
    if status != NO_ERR {
        return Err(os_error("create VideoToolbox sample buffer", status));
    }
    Ok(sample)
}

unsafe fn block_buffer_length(block: CMBlockBufferRef) -> usize {
    // CMBlockBufferGetDataLength is intentionally declared locally to keep all
    // CoreMedia FFI in this module.
    unsafe extern "C" {
        fn CMBlockBufferGetDataLength(block_buffer: CMBlockBufferRef) -> usize;
    }
    unsafe { CMBlockBufferGetDataLength(block) }
}

fn annex_b_to_avcc(nals: &[Vec<u8>]) -> Result<Vec<u8>> {
    let total = nals.iter().try_fold(0_usize, |sum, nal| {
        sum.checked_add(4 + nal.len())
            .ok_or_else(|| DcError::InvalidInput("H.264 sample size overflowed".into()))
    })?;
    let mut output = Vec::with_capacity(total);
    for nal in nals {
        let length = u32::try_from(nal.len())
            .map_err(|_| DcError::InvalidInput("H.264 NAL unit is too large".into()))?;
        output.extend_from_slice(&length.to_be_bytes());
        output.extend_from_slice(nal);
    }
    Ok(output)
}

fn duration_to_timescale(duration: std::time::Duration, timescale: i32) -> i64 {
    (duration.as_secs_f64() * f64::from(timescale)).round() as i64
}

fn os_error(operation: &str, status: OSStatus) -> DcError {
    DcError::Platform(format!("{operation} failed with OSStatus {status}"))
}
