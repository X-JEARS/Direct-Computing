use ::windows::core::Interface;
use ::windows::Win32::Foundation::HMODULE;
use ::windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use ::windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_CPU_ACCESS_READ,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_SDK_VERSION,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use ::windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_MODE_ROTATION_IDENTITY, DXGI_MODE_ROTATION_UNSPECIFIED,
};
use ::windows::Win32::Graphics::Dxgi::{
    IDXGIAdapter, IDXGIDevice, IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource,
    DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO,
};
use dc_common::{DcError, Result};
use dc_media::{FrameLayout, FrameSize, FrameSource, PixelFormat, VideoFrame};
use std::time::Instant;

pub struct WindowsDesktopCapturer {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    duplication: IDXGIOutputDuplication,
    staging: Option<(D3D11_TEXTURE2D_DESC, ID3D11Texture2D)>,
    output_index: u32,
    output_name: String,
    timeout_ms: u32,
    sequence: u64,
    started_at: Instant,
}

impl WindowsDesktopCapturer {
    pub fn new(output_index: u32, timeout_ms: u32) -> Result<Self> {
        let (device, context) = create_device()?;
        let dxgi_device: IDXGIDevice = device
            .cast()
            .map_err(|error| windows_error("cast D3D11 device to DXGI device", error))?;
        // The COM objects originate from the device and remain alive through owned interface refs.
        let adapter = unsafe { dxgi_device.GetAdapter() }
            .map_err(|error| windows_error("get DXGI adapter", error))?;
        let output = unsafe { adapter.EnumOutputs(output_index) }
            .map_err(|error| windows_error("enumerate DXGI output", error))?;
        let output_desc = unsafe { output.GetDesc() }
            .map_err(|error| windows_error("read DXGI output description", error))?;
        if !output_desc.AttachedToDesktop.as_bool() {
            return Err(DcError::Platform(format!(
                "DXGI output {output_index} is not attached to the desktop"
            )));
        }
        if output_desc.Rotation != DXGI_MODE_ROTATION_IDENTITY
            && output_desc.Rotation != DXGI_MODE_ROTATION_UNSPECIFIED
        {
            return Err(DcError::Unsupported(format!(
                "rotated DXGI output {output_index} is not supported yet"
            )));
        }
        let output_name = utf16_name(&output_desc.DeviceName);
        let output1: IDXGIOutput1 = output
            .cast()
            .map_err(|error| windows_error("cast DXGI output", error))?;
        let duplication = unsafe { output1.DuplicateOutput(&device) }
            .map_err(|error| windows_error("start desktop duplication", error))?;

        Ok(Self {
            device,
            context,
            duplication,
            staging: None,
            output_index,
            output_name,
            timeout_ms,
            sequence: 0,
            started_at: Instant::now(),
        })
    }

    pub const fn output_index(&self) -> u32 {
        self.output_index
    }

    pub fn output_name(&self) -> &str {
        &self.output_name
    }
}

impl FrameSource for WindowsDesktopCapturer {
    fn capture(&mut self) -> Result<VideoFrame> {
        let mut frame_info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut desktop_resource: Option<IDXGIResource> = None;
        let acquire_result = unsafe {
            self.duplication.AcquireNextFrame(
                self.timeout_ms,
                &mut frame_info,
                &mut desktop_resource,
            )
        };
        if let Err(error) = acquire_result {
            return if error.code() == DXGI_ERROR_WAIT_TIMEOUT {
                Err(DcError::Platform(format!(
                    "timed out after {} ms waiting for desktop frame",
                    self.timeout_ms
                )))
            } else if error.code() == DXGI_ERROR_ACCESS_LOST {
                Err(DcError::Platform(
                    "desktop duplication access was lost; recreate the capturer".into(),
                ))
            } else {
                Err(windows_error("acquire desktop frame", error))
            };
        }
        let _frame_guard = AcquiredFrame::new(&self.duplication);
        let desktop_resource = desktop_resource.ok_or_else(|| {
            DcError::Platform("DXGI returned a frame without a desktop resource".into())
        })?;
        let texture: ID3D11Texture2D = desktop_resource
            .cast()
            .map_err(|error| windows_error("cast desktop frame to D3D11 texture", error))?;
        let mut source_desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { texture.GetDesc(&mut source_desc) };
        if source_desc.Format != DXGI_FORMAT_B8G8R8A8_UNORM {
            return Err(DcError::Unsupported(format!(
                "DXGI desktop format {:?} is not supported",
                source_desc.Format
            )));
        }

        let staging = staging_texture(&self.device, &mut self.staging, source_desc)?;
        unsafe { self.context.CopyResource(&staging, &texture) };

        let size = FrameSize::new(source_desc.Width, source_desc.Height)?;
        let layout = FrameLayout::packed(size, PixelFormat::Bgra32)?;
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe {
            self.context
                .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
        }
        .map_err(|error| windows_error("map desktop staging texture", error))?;

        let copy_result = copy_mapped_bgra(&mapped, layout);
        unsafe { self.context.Unmap(&staging, 0) };
        let data = copy_result?;
        let sequence = self.sequence;
        self.sequence = self.sequence.wrapping_add(1);
        VideoFrame::new(sequence, self.started_at.elapsed(), layout, data)
    }
}

fn staging_texture(
    device: &ID3D11Device,
    cache: &mut Option<(D3D11_TEXTURE2D_DESC, ID3D11Texture2D)>,
    mut description: D3D11_TEXTURE2D_DESC,
) -> Result<ID3D11Texture2D> {
    description.Usage = D3D11_USAGE_STAGING;
    description.BindFlags = 0;
    description.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
    description.MiscFlags = 0;
    if let Some((cached_description, texture)) = cache {
        if *cached_description == description {
            return Ok(texture.clone());
        }
    }

    let mut texture = None;
    unsafe { device.CreateTexture2D(&description, None, Some(&mut texture)) }
        .map_err(|error| windows_error("create CPU-readable staging texture", error))?;
    let texture =
        texture.ok_or_else(|| DcError::Platform("D3D11 returned no staging texture".into()))?;
    *cache = Some((description, texture.clone()));
    Ok(texture)
}

fn create_device() -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    let mut device = None;
    let mut context = None;
    unsafe {
        D3D11CreateDevice(
            None::<&IDXGIAdapter>,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
    }
    .map_err(|error| windows_error("create D3D11 device", error))?;
    let device = device.ok_or_else(|| DcError::Platform("D3D11 returned no device".into()))?;
    let context =
        context.ok_or_else(|| DcError::Platform("D3D11 returned no device context".into()))?;
    Ok((device, context))
}

fn copy_mapped_bgra(mapped: &D3D11_MAPPED_SUBRESOURCE, layout: FrameLayout) -> Result<Vec<u8>> {
    let source_stride = mapped.RowPitch as usize;
    let row_bytes = layout.size().width() as usize * 4;
    if mapped.pData.is_null() || source_stride < row_bytes {
        return Err(DcError::Platform(
            "D3D11 returned an invalid mapped desktop texture".into(),
        ));
    }
    let mapped_len = source_stride
        .checked_mul(layout.size().height() as usize)
        .ok_or_else(|| DcError::Platform("mapped desktop buffer size overflowed".into()))?;
    // D3D11 guarantees pData covers RowPitch * Height until the matching Unmap call.
    let source = unsafe { std::slice::from_raw_parts(mapped.pData.cast::<u8>(), mapped_len) };
    let mut data = vec![0; layout.data_len()];
    for (source_row, destination_row) in source
        .chunks_exact(source_stride)
        .zip(data.chunks_exact_mut(layout.stride()))
    {
        destination_row.copy_from_slice(&source_row[..row_bytes]);
    }
    Ok(data)
}

fn utf16_name(value: &[u16]) -> String {
    let end = value
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(value.len());
    String::from_utf16_lossy(&value[..end])
}

fn windows_error(operation: &str, error: ::windows::core::Error) -> DcError {
    DcError::Platform(format!(
        "{operation} failed ({:#010x}): {}",
        error.code().0 as u32,
        error.message()
    ))
}

struct AcquiredFrame<'a> {
    duplication: &'a IDXGIOutputDuplication,
}

impl<'a> AcquiredFrame<'a> {
    const fn new(duplication: &'a IDXGIOutputDuplication) -> Self {
        Self { duplication }
    }
}

impl Drop for AcquiredFrame<'_> {
    fn drop(&mut self) {
        // ReleaseFrame is required exactly once after every successful AcquireNextFrame.
        let _ = unsafe { self.duplication.ReleaseFrame() };
    }
}
