use crate::InputInjector;
use ::windows::core::Interface;
use ::windows::Win32::Foundation::{HMODULE, RECT};
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
    DXGI_OUTDUPL_MOVE_RECT, DXGI_OUTDUPL_POINTER_SHAPE_INFO, DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR,
    DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR, DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME,
};
use ::windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYBD_EVENT_FLAGS,
    KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, MOUSEEVENTF_ABSOLUTE,
    MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN,
    MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP,
    MOUSEEVENTF_WHEEL, MOUSEINPUT, VIRTUAL_KEY, VK_F13, VK_F14, VK_F15, VK_PAUSE,
};
use ::windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN};
use dc_common::{DcError, Result};
use dc_media::{
    CursorShape, CursorShapeKind, CursorUpdate, DamageRect, FrameLayout, FrameMetadata, FrameSize,
    FrameSource, PixelFormat, VideoFrame,
};
use dc_protocol::{InputEvent, KeyboardKey};
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
                Err(DcError::Timeout(format!(
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
        let metadata = capture_metadata(&self.duplication, &frame_info, source_desc)?;

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
        VideoFrame::new(sequence, self.started_at.elapsed(), layout, data)?.with_metadata(metadata)
    }
}

fn capture_metadata(
    duplication: &IDXGIOutputDuplication,
    frame_info: &DXGI_OUTDUPL_FRAME_INFO,
    source_desc: D3D11_TEXTURE2D_DESC,
) -> Result<FrameMetadata> {
    let mut damage = Vec::new();
    if frame_info.TotalMetadataBufferSize > 0 {
        let dirty_capacity =
            frame_info.TotalMetadataBufferSize as usize / std::mem::size_of::<RECT>() + 1;
        let mut dirty = vec![RECT::default(); dirty_capacity];
        let mut dirty_bytes = 0_u32;
        unsafe {
            duplication.GetFrameDirtyRects(
                u32::try_from(dirty.len() * std::mem::size_of::<RECT>())
                    .map_err(|_| DcError::Platform("dirty rectangle buffer is too large".into()))?,
                dirty.as_mut_ptr(),
                &mut dirty_bytes,
            )
        }
        .map_err(|error| windows_error("read desktop dirty rectangles", error))?;
        dirty.truncate(dirty_bytes as usize / std::mem::size_of::<RECT>());
        for rect in dirty {
            if let Some(region) = damage_rect(rect, source_desc.Width, source_desc.Height) {
                damage.push(region);
            }
        }

        let move_capacity = frame_info.TotalMetadataBufferSize as usize
            / std::mem::size_of::<DXGI_OUTDUPL_MOVE_RECT>()
            + 1;
        let mut moves = vec![DXGI_OUTDUPL_MOVE_RECT::default(); move_capacity];
        let mut move_bytes = 0_u32;
        unsafe {
            duplication.GetFrameMoveRects(
                u32::try_from(moves.len() * std::mem::size_of::<DXGI_OUTDUPL_MOVE_RECT>())
                    .map_err(|_| DcError::Platform("move rectangle buffer is too large".into()))?,
                moves.as_mut_ptr(),
                &mut move_bytes,
            )
        }
        .map_err(|error| windows_error("read desktop move rectangles", error))?;
        moves.truncate(move_bytes as usize / std::mem::size_of::<DXGI_OUTDUPL_MOVE_RECT>());
        for movement in moves {
            let destination = movement.DestinationRect;
            if let Some(region) = damage_rect(destination, source_desc.Width, source_desc.Height) {
                let source = RECT {
                    left: movement.SourcePoint.x,
                    top: movement.SourcePoint.y,
                    right: movement.SourcePoint.x + region.width as i32,
                    bottom: movement.SourcePoint.y + region.height as i32,
                };
                if let Some(source) = damage_rect(source, source_desc.Width, source_desc.Height) {
                    damage.push(source);
                }
                damage.push(region);
            }
        }
    }

    let cursor = if frame_info.LastMouseUpdateTime != 0 || frame_info.PointerShapeBufferSize > 0 {
        let shape = if frame_info.PointerShapeBufferSize > 0 {
            Some(capture_cursor_shape(
                duplication,
                frame_info.PointerShapeBufferSize,
            )?)
        } else {
            None
        };
        Some(CursorUpdate {
            visible: frame_info.PointerPosition.Visible.as_bool(),
            x: frame_info.PointerPosition.Position.x,
            y: frame_info.PointerPosition.Position.y,
            shape,
        })
    } else {
        None
    };

    Ok(FrameMetadata {
        damage: Some(damage),
        cursor,
    })
}

fn damage_rect(rect: RECT, width: u32, height: u32) -> Option<DamageRect> {
    let left = rect.left.clamp(0, width as i32) as u32;
    let top = rect.top.clamp(0, height as i32) as u32;
    let right = rect.right.clamp(0, width as i32) as u32;
    let bottom = rect.bottom.clamp(0, height as i32) as u32;
    (right > left && bottom > top).then_some(DamageRect {
        x: left,
        y: top,
        width: right - left,
        height: bottom - top,
    })
}

fn capture_cursor_shape(
    duplication: &IDXGIOutputDuplication,
    buffer_size: u32,
) -> Result<CursorShape> {
    let mut data = vec![0_u8; buffer_size as usize];
    let mut required = 0_u32;
    let mut info = DXGI_OUTDUPL_POINTER_SHAPE_INFO::default();
    unsafe {
        duplication.GetFramePointerShape(
            buffer_size,
            data.as_mut_ptr().cast(),
            &mut required,
            &mut info,
        )
    }
    .map_err(|error| windows_error("read desktop pointer shape", error))?;
    data.truncate(required as usize);
    let kind = if info.Type == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME.0 as u32 {
        CursorShapeKind::Monochrome
    } else if info.Type == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR.0 as u32 {
        CursorShapeKind::Color
    } else if info.Type == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR.0 as u32 {
        CursorShapeKind::MaskedColor
    } else {
        return Err(DcError::Unsupported(format!(
            "DXGI pointer shape type {} is not supported",
            info.Type
        )));
    };
    let height = if kind == CursorShapeKind::Monochrome {
        info.Height / 2
    } else {
        info.Height
    };
    Ok(CursorShape {
        kind,
        width: info.Width,
        height,
        hotspot_x: info.HotSpot.x.max(0) as u32,
        hotspot_y: info.HotSpot.y.max(0) as u32,
        pitch: info.Pitch,
        data,
    })
}

/// Windows desktop input adapter backed by User32 `SendInput`.
/// Pointer coordinates use physical desktop pixels; mouse button bits are
/// left=1, right=2, middle=4.
pub struct WindowsInputInjector {
    buttons: u8,
    pressed_keys: Vec<KeyboardKey>,
}

impl WindowsInputInjector {
    pub fn new() -> Self {
        Self {
            buttons: 0,
            pressed_keys: Vec::new(),
        }
    }

    fn send(input: &INPUT) -> Result<()> {
        let sent = unsafe {
            SendInput(
                std::slice::from_ref(input),
                std::mem::size_of::<INPUT>() as i32,
            )
        };
        if sent != 1 {
            return Err(DcError::Platform(
                "SendInput did not inject the event".into(),
            ));
        }
        Ok(())
    }
}

impl Default for WindowsInputInjector {
    fn default() -> Self {
        Self::new()
    }
}

impl InputInjector for WindowsInputInjector {
    fn inject(&mut self, event: &InputEvent) -> Result<()> {
        match event {
            InputEvent::Pointer { x, y, buttons } => {
                if buttons & !0x07 != 0 {
                    return Err(DcError::InvalidInput(
                        "pointer contains unsupported button bits".into(),
                    ));
                }
                if *x < 0 || *y < 0 {
                    return Err(DcError::InvalidInput(
                        "pointer coordinates must be non-negative".into(),
                    ));
                }
                let screen_width = unsafe { GetSystemMetrics(SM_CXSCREEN) };
                let screen_height = unsafe { GetSystemMetrics(SM_CYSCREEN) };
                if screen_width <= 0
                    || screen_height <= 0
                    || *x >= screen_width
                    || *y >= screen_height
                {
                    return Err(DcError::InvalidInput(
                        "pointer coordinates are outside the primary desktop".into(),
                    ));
                }
                let normalized_x =
                    ((*x as i64 * 65_535) / i64::from((screen_width - 1).max(1))) as i32;
                let normalized_y =
                    ((*y as i64 * 65_535) / i64::from((screen_height - 1).max(1))) as i32;
                let pointer = INPUT {
                    r#type: INPUT_MOUSE,
                    Anonymous: INPUT_0 {
                        mi: MOUSEINPUT {
                            dx: normalized_x,
                            dy: normalized_y,
                            mouseData: 0,
                            dwFlags: MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE,
                            time: 0,
                            dwExtraInfo: 0,
                        },
                    },
                };
                Self::send(&pointer)?;
                let changed = self.buttons ^ *buttons;
                for (mask, down, up) in [
                    (1, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP),
                    (2, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP),
                    (4, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP),
                ] {
                    if changed & mask != 0 {
                        let button = INPUT {
                            r#type: INPUT_MOUSE,
                            Anonymous: INPUT_0 {
                                mi: MOUSEINPUT {
                                    dx: 0,
                                    dy: 0,
                                    mouseData: 0,
                                    dwFlags: if *buttons & mask != 0 { down } else { up },
                                    time: 0,
                                    dwExtraInfo: 0,
                                },
                            },
                        };
                        Self::send(&button)?;
                    }
                }
                self.buttons = *buttons & 0x07;
                Ok(())
            }
            InputEvent::Wheel { delta_x, delta_y } => {
                if delta_x.unsigned_abs() > 12_000 || delta_y.unsigned_abs() > 12_000 {
                    return Err(DcError::InvalidInput(
                        "wheel delta exceeds the per-event limit".into(),
                    ));
                }
                for (delta, flags) in [
                    (*delta_x, MOUSEEVENTF_HWHEEL),
                    (*delta_y, MOUSEEVENTF_WHEEL),
                ] {
                    if delta == 0 {
                        continue;
                    }
                    Self::send(&INPUT {
                        r#type: INPUT_MOUSE,
                        Anonymous: INPUT_0 {
                            mi: MOUSEINPUT {
                                dx: 0,
                                dy: 0,
                                mouseData: delta as u32,
                                dwFlags: flags,
                                time: 0,
                                dwExtraInfo: 0,
                            },
                        },
                    })?;
                }
                Ok(())
            }
            InputEvent::Key { key, pressed } => {
                let input = INPUT {
                    r#type: INPUT_KEYBOARD,
                    Anonymous: INPUT_0 {
                        ki: windows_key_input(*key, *pressed),
                    },
                };
                Self::send(&input)?;
                if *pressed {
                    if !self.pressed_keys.contains(key) {
                        self.pressed_keys.push(*key);
                    }
                } else {
                    self.pressed_keys.retain(|pressed_key| pressed_key != key);
                }
                Ok(())
            }
        }
    }
}

impl Drop for WindowsInputInjector {
    fn drop(&mut self) {
        for key in self.pressed_keys.drain(..).rev() {
            let _ = Self::send(&INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: windows_key_input(key, false),
                },
            });
        }
        for (mask, up) in [
            (1, MOUSEEVENTF_LEFTUP),
            (2, MOUSEEVENTF_RIGHTUP),
            (4, MOUSEEVENTF_MIDDLEUP),
        ] {
            if self.buttons & mask != 0 {
                let _ = Self::send(&INPUT {
                    r#type: INPUT_MOUSE,
                    Anonymous: INPUT_0 {
                        mi: MOUSEINPUT {
                            dx: 0,
                            dy: 0,
                            mouseData: 0,
                            dwFlags: up,
                            time: 0,
                            dwExtraInfo: 0,
                        },
                    },
                });
            }
        }
    }
}

enum WindowsKeyMapping {
    Scan { code: u16, extended: bool },
    Virtual(VIRTUAL_KEY),
}

fn windows_key_input(key: KeyboardKey, pressed: bool) -> KEYBDINPUT {
    let mut flags = if pressed {
        KEYBD_EVENT_FLAGS(0)
    } else {
        KEYEVENTF_KEYUP
    };
    let (virtual_key, scan_code) = match windows_key_mapping(key) {
        WindowsKeyMapping::Scan { code, extended } => {
            flags |= KEYEVENTF_SCANCODE;
            if extended {
                flags |= KEYEVENTF_EXTENDEDKEY;
            }
            (VIRTUAL_KEY(0), code)
        }
        WindowsKeyMapping::Virtual(virtual_key) => (virtual_key, 0),
    };
    KEYBDINPUT {
        wVk: virtual_key,
        wScan: scan_code,
        dwFlags: flags,
        time: 0,
        dwExtraInfo: 0,
    }
}

fn windows_key_mapping(key: KeyboardKey) -> WindowsKeyMapping {
    use KeyboardKey::*;
    let (code, extended) = match key {
        Digit0 => (0x0b, false),
        Digit1 => (0x02, false),
        Digit2 => (0x03, false),
        Digit3 => (0x04, false),
        Digit4 => (0x05, false),
        Digit5 => (0x06, false),
        Digit6 => (0x07, false),
        Digit7 => (0x08, false),
        Digit8 => (0x09, false),
        Digit9 => (0x0a, false),
        A => (0x1e, false),
        B => (0x30, false),
        C => (0x2e, false),
        D => (0x20, false),
        E => (0x12, false),
        F => (0x21, false),
        G => (0x22, false),
        H => (0x23, false),
        I => (0x17, false),
        J => (0x24, false),
        K => (0x25, false),
        L => (0x26, false),
        M => (0x32, false),
        N => (0x31, false),
        O => (0x18, false),
        P => (0x19, false),
        Q => (0x10, false),
        R => (0x13, false),
        S => (0x1f, false),
        T => (0x14, false),
        U => (0x16, false),
        V => (0x2f, false),
        W => (0x11, false),
        X => (0x2d, false),
        Y => (0x15, false),
        Z => (0x2c, false),
        F1 => (0x3b, false),
        F2 => (0x3c, false),
        F3 => (0x3d, false),
        F4 => (0x3e, false),
        F5 => (0x3f, false),
        F6 => (0x40, false),
        F7 => (0x41, false),
        F8 => (0x42, false),
        F9 => (0x43, false),
        F10 => (0x44, false),
        F11 => (0x57, false),
        F12 => (0x58, false),
        F13 => return WindowsKeyMapping::Virtual(VK_F13),
        F14 => return WindowsKeyMapping::Virtual(VK_F14),
        F15 => return WindowsKeyMapping::Virtual(VK_F15),
        ArrowDown => (0x50, true),
        ArrowLeft => (0x4b, true),
        ArrowRight => (0x4d, true),
        ArrowUp => (0x48, true),
        Apostrophe => (0x28, false),
        Backquote => (0x29, false),
        Backslash => (0x2b, false),
        Comma => (0x33, false),
        Equal => (0x0d, false),
        LeftBracket => (0x1a, false),
        Minus => (0x0c, false),
        Period => (0x34, false),
        RightBracket => (0x1b, false),
        Semicolon => (0x27, false),
        Slash => (0x35, false),
        Backspace => (0x0e, false),
        Delete => (0x53, true),
        End => (0x4f, true),
        Enter => (0x1c, false),
        Escape => (0x01, false),
        Home => (0x47, true),
        Insert => (0x52, true),
        ContextMenu => (0x5d, true),
        PageDown => (0x51, true),
        PageUp => (0x49, true),
        Pause => return WindowsKeyMapping::Virtual(VK_PAUSE),
        Space => (0x39, false),
        Tab => (0x0f, false),
        NumLock => (0x45, true),
        CapsLock => (0x3a, false),
        ScrollLock => (0x46, false),
        LeftShift => (0x2a, false),
        RightShift => (0x36, false),
        LeftControl => (0x1d, false),
        RightControl => (0x1d, true),
        Numpad0 => (0x52, false),
        Numpad1 => (0x4f, false),
        Numpad2 => (0x50, false),
        Numpad3 => (0x51, false),
        Numpad4 => (0x4b, false),
        Numpad5 => (0x4c, false),
        Numpad6 => (0x4d, false),
        Numpad7 => (0x47, false),
        Numpad8 => (0x48, false),
        Numpad9 => (0x49, false),
        NumpadDecimal => (0x53, false),
        NumpadDivide => (0x35, true),
        NumpadMultiply => (0x37, false),
        NumpadSubtract => (0x4a, false),
        NumpadAdd => (0x4e, false),
        NumpadEnter => (0x1c, true),
        LeftAlt => (0x38, false),
        RightAlt => (0x38, true),
        LeftSuper => (0x5b, true),
        RightSuper => (0x5c, true),
    };
    WindowsKeyMapping::Scan { code, extended }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_physical_keys_to_windows_scan_codes() {
        let letter = windows_key_input(KeyboardKey::A, true);
        assert_eq!(letter.wVk, VIRTUAL_KEY(0));
        assert_eq!(letter.wScan, 0x1e);
        assert_eq!(letter.dwFlags, KEYEVENTF_SCANCODE);

        let right_control = windows_key_input(KeyboardKey::RightControl, true);
        assert_eq!(right_control.wScan, 0x1d);
        assert_eq!(
            right_control.dwFlags,
            KEYEVENTF_SCANCODE | KEYEVENTF_EXTENDEDKEY
        );

        let numpad_enter = windows_key_input(KeyboardKey::NumpadEnter, false);
        assert_eq!(numpad_enter.wScan, 0x1c);
        assert_eq!(
            numpad_enter.dwFlags,
            KEYEVENTF_SCANCODE | KEYEVENTF_EXTENDEDKEY | KEYEVENTF_KEYUP
        );

        let main_enter = windows_key_input(KeyboardKey::Enter, true);
        assert_eq!(main_enter.wScan, 0x1c);
        assert_eq!(main_enter.dwFlags, KEYEVENTF_SCANCODE);
    }

    #[test]
    fn maps_keys_without_set_one_codes_to_virtual_keys() {
        let f13 = windows_key_input(KeyboardKey::F13, true);
        assert_eq!(f13.wVk, VK_F13);
        assert_eq!(f13.wScan, 0);
        assert_eq!(f13.dwFlags, KEYBD_EVENT_FLAGS(0));
    }
}
