use dc_common::{DcError, Result};
use dc_media::{FrameSink, PixelFormat, VideoFrame};
#[cfg(target_os = "macos")]
use dc_platform::window_view_geometry;
use dc_platform::{local_screen_size, InputEventSource};
use dc_protocol::{CursorShape, DesktopRect, InputEvent, KeyboardKey};
use minifb::{Key, KeyRepeat, MouseButton, MouseMode, ScaleMode, Window, WindowOptions};

const MAX_INITIAL_WIDTH: usize = 1_280;
const MAX_INITIAL_HEIGHT: usize = 720;
const WINDOW_DECORATION_HEIGHT: usize = 80;
const WINDOWS_WHEEL_DELTA: f32 = 120.0;
const MAX_WHEEL_DELTA_PER_EVENT: f32 = WINDOWS_WHEEL_DELTA * 100.0;

pub struct PreviewWindowSink {
    base_title: String,
    window: Option<Window>,
    desktop_pixels: Vec<u32>,
    pixels: Vec<u32>,
    frame_size: Option<(usize, usize)>,
    last_pointer: Option<(i32, i32, u8)>,
    scroll_remainder: (f32, f32),
    remote_cursor: RemoteCursor,
}

#[derive(Default)]
struct RemoteCursor {
    visible: bool,
    x: i32,
    y: i32,
    shape: Option<CursorShape>,
}

impl PreviewWindowSink {
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            base_title: title.into(),
            window: None,
            desktop_pixels: Vec::new(),
            pixels: Vec::new(),
            frame_size: None,
            last_pointer: None,
            scroll_remainder: (0.0, 0.0),
            remote_cursor: RemoteCursor::default(),
        }
    }

    /// Name of the native presentation backend used by the preview window.
    ///
    /// minifb's macOS backend uploads the frame into a Metal texture and
    /// presents it through a CAMetalLayer. Keeping this information beside the
    /// sink makes the Viewer startup diagnostics explicit instead of implying
    /// that VideoToolbox hardware decode alone covers presentation.
    pub const fn render_backend() -> &'static str {
        #[cfg(target_os = "macos")]
        {
            "metal"
        }
        #[cfg(not(target_os = "macos"))]
        {
            "minifb"
        }
    }

    pub fn is_open(&self) -> bool {
        self.window.as_ref().is_none_or(Window::is_open)
    }

    pub fn pump_events(&mut self) {
        if let Some(window) = &mut self.window {
            window.update();
            #[cfg(target_os = "macos")]
            let _ = window_view_geometry(window.get_window_handle() as usize);
        }
    }

    /// Drain local keyboard and pointer events observed by the preview window.
    /// Coordinates are reported in decoded-frame pixels, independent of the
    /// window's current scaling.
    pub fn drain_input_events(&mut self) -> Vec<InputEvent> {
        let Some(window) = &mut self.window else {
            return Vec::new();
        };
        let mut events = Vec::new();
        #[cfg(target_os = "macos")]
        let macos_geometry = window_view_geometry(window.get_window_handle() as usize);
        if let Some((mouse_x, mouse_y)) = window.get_unscaled_mouse_pos(MouseMode::Clamp) {
            let (window_width, window_height, mouse_x, mouse_y) = {
                #[cfg(target_os = "macos")]
                if let Some((width, height, x, y)) = macos_geometry {
                    (width, height, x, y)
                } else {
                    let (width, height) = window.get_size();
                    (width, height, mouse_x, mouse_y)
                }
                #[cfg(not(target_os = "macos"))]
                {
                    let (width, height) = window.get_size();
                    (width, height, mouse_x, mouse_y)
                }
            };
            let (frame_width, frame_height) = self
                .frame_size
                .unwrap_or((window_width.max(1), window_height.max(1)));
            // minifb reports the pointer in the window's client area.  With
            // AspectRatioStretch the image is aspect-fitted and letterboxed,
            // so first remove the black-bar offsets before scaling to frame
            // pixels.
            let (x, y) = map_pointer_to_frame(
                mouse_x,
                mouse_y,
                window_width,
                window_height,
                frame_width,
                frame_height,
            );
            let buttons = u8::from(window.get_mouse_down(MouseButton::Left))
                | (u8::from(window.get_mouse_down(MouseButton::Right)) << 1)
                | (u8::from(window.get_mouse_down(MouseButton::Middle)) << 2);
            let pointer = (x, y, buttons);
            if self.last_pointer != Some(pointer) {
                events.push(InputEvent::Pointer {
                    x: pointer.0,
                    y: pointer.1,
                    buttons,
                });
                self.last_pointer = Some(pointer);
            }
        }
        if let Some((horizontal, vertical)) = window.get_scroll_wheel() {
            self.scroll_remainder.0 += horizontal * WINDOWS_WHEEL_DELTA;
            self.scroll_remainder.1 += vertical * WINDOWS_WHEEL_DELTA;
        }
        let delta_x = take_wheel_delta(&mut self.scroll_remainder.0);
        let delta_y = take_wheel_delta(&mut self.scroll_remainder.1);
        if delta_x != 0 || delta_y != 0 {
            events.push(InputEvent::Wheel { delta_x, delta_y });
        }
        for key in window.get_keys_pressed(KeyRepeat::Yes) {
            if let Some(key) = map_key(key) {
                events.push(InputEvent::Key { key, pressed: true });
            }
        }
        for key in window.get_keys_released() {
            if let Some(key) = map_key(key) {
                events.push(InputEvent::Key {
                    key,
                    pressed: false,
                });
            }
        }
        events
    }

    pub fn set_status(&mut self, status: &str) {
        if let Some(window) = &mut self.window {
            window.set_title(&format!("{} | {status}", self.base_title));
        }
    }

    /// Create a visible placeholder surface before the first decoded frame.
    /// This keeps the Viewer window discoverable while the first keyframe is
    /// being reassembled over the lossy media lane.
    pub fn show_placeholder(&mut self, width: usize, height: usize) -> Result<()> {
        self.ensure_window(width, height)?;
        self.frame_size = Some((width, height));
        self.desktop_pixels.resize(width.saturating_mul(height), 0);
        self.pixels.resize(width.saturating_mul(height), 0);
        self.window
            .as_mut()
            .ok_or_else(|| DcError::Platform("preview window was not created".into()))?
            .update_with_buffer(&self.pixels, width, height)
            .map_err(|error| DcError::Platform(format!("show preview window: {error}")))
    }

    pub fn apply_bgra_regions(
        &mut self,
        desktop_width: u32,
        desktop_height: u32,
        regions: &[DesktopRect],
        data: &[u8],
    ) -> Result<()> {
        let width = desktop_width as usize;
        let height = desktop_height as usize;
        if self.frame_size != Some((width, height))
            || self.desktop_pixels.len() != width.saturating_mul(height)
        {
            return Err(DcError::Codec(
                "desktop update arrived before a matching full frame".into(),
            ));
        }
        let mut offset = 0_usize;
        for region in regions {
            let right = region.x.checked_add(region.width);
            let bottom = region.y.checked_add(region.height);
            if region.width == 0
                || region.height == 0
                || right.is_none_or(|value| value > desktop_width)
                || bottom.is_none_or(|value| value > desktop_height)
            {
                return Err(DcError::Codec(
                    "desktop update region is outside the frame".into(),
                ));
            }
            for row in 0..region.height as usize {
                let row_bytes = region.width as usize * 4;
                let source = data
                    .get(offset..offset + row_bytes)
                    .ok_or_else(|| DcError::Codec("desktop update payload is truncated".into()))?;
                let destination_start = (region.y as usize + row) * width + region.x as usize;
                let destination = &mut self.desktop_pixels
                    [destination_start..destination_start + region.width as usize];
                for (pixel, bgra) in destination.iter_mut().zip(source.chunks_exact(4)) {
                    *pixel =
                        (u32::from(bgra[2]) << 16) | (u32::from(bgra[1]) << 8) | u32::from(bgra[0]);
                }
                offset += row_bytes;
            }
        }
        if offset != data.len() {
            return Err(DcError::Codec(
                "desktop update payload has trailing bytes".into(),
            ));
        }
        self.render_composited()
    }

    pub fn update_remote_cursor(
        &mut self,
        visible: bool,
        x: i32,
        y: i32,
        shape: Option<CursorShape>,
    ) -> Result<()> {
        self.remote_cursor.visible = visible;
        self.remote_cursor.x = x;
        self.remote_cursor.y = y;
        if let Some(shape) = shape {
            validate_cursor_shape(&shape)?;
            self.remote_cursor.shape = Some(shape);
        }
        self.render_composited()
    }

    fn render_composited(&mut self) -> Result<()> {
        let Some((width, height)) = self.frame_size else {
            return Ok(());
        };
        self.pixels.clone_from(&self.desktop_pixels);
        draw_cursor(&mut self.pixels, width, height, &self.remote_cursor);
        if let Some(window) = &mut self.window {
            window
                .update_with_buffer(&self.pixels, width, height)
                .map_err(|error| DcError::Platform(format!("update preview window: {error}")))?;
        }
        Ok(())
    }

    fn ensure_window(&mut self, width: usize, height: usize) -> Result<&mut Window> {
        if self.window.is_none() {
            let (window_width, window_height) = initial_window_size(width, height);
            let mut window = Window::new(
                &self.base_title,
                window_width,
                window_height,
                WindowOptions {
                    resize: true,
                    scale_mode: ScaleMode::AspectRatioStretch,
                    ..WindowOptions::default()
                },
            )
            .map_err(|error| DcError::Platform(format!("create preview window: {error}")))?;
            window.set_target_fps(0);
            self.window = Some(window);
        }
        self.window
            .as_mut()
            .ok_or_else(|| DcError::Platform("preview window was not created".into()))
    }
}

fn take_wheel_delta(remainder: &mut f32) -> i32 {
    if !remainder.is_finite() {
        *remainder = 0.0;
        return 0;
    }
    let delta = remainder
        .trunc()
        .clamp(-MAX_WHEEL_DELTA_PER_EVENT, MAX_WHEEL_DELTA_PER_EVENT);
    *remainder -= delta;
    delta as i32
}

fn map_key(key: Key) -> Option<KeyboardKey> {
    Some(match key {
        Key::Key0 => KeyboardKey::Digit0,
        Key::Key1 => KeyboardKey::Digit1,
        Key::Key2 => KeyboardKey::Digit2,
        Key::Key3 => KeyboardKey::Digit3,
        Key::Key4 => KeyboardKey::Digit4,
        Key::Key5 => KeyboardKey::Digit5,
        Key::Key6 => KeyboardKey::Digit6,
        Key::Key7 => KeyboardKey::Digit7,
        Key::Key8 => KeyboardKey::Digit8,
        Key::Key9 => KeyboardKey::Digit9,
        Key::A => KeyboardKey::A,
        Key::B => KeyboardKey::B,
        Key::C => KeyboardKey::C,
        Key::D => KeyboardKey::D,
        Key::E => KeyboardKey::E,
        Key::F => KeyboardKey::F,
        Key::G => KeyboardKey::G,
        Key::H => KeyboardKey::H,
        Key::I => KeyboardKey::I,
        Key::J => KeyboardKey::J,
        Key::K => KeyboardKey::K,
        Key::L => KeyboardKey::L,
        Key::M => KeyboardKey::M,
        Key::N => KeyboardKey::N,
        Key::O => KeyboardKey::O,
        Key::P => KeyboardKey::P,
        Key::Q => KeyboardKey::Q,
        Key::R => KeyboardKey::R,
        Key::S => KeyboardKey::S,
        Key::T => KeyboardKey::T,
        Key::U => KeyboardKey::U,
        Key::V => KeyboardKey::V,
        Key::W => KeyboardKey::W,
        Key::X => KeyboardKey::X,
        Key::Y => KeyboardKey::Y,
        Key::Z => KeyboardKey::Z,
        Key::F1 => KeyboardKey::F1,
        Key::F2 => KeyboardKey::F2,
        Key::F3 => KeyboardKey::F3,
        Key::F4 => KeyboardKey::F4,
        Key::F5 => KeyboardKey::F5,
        Key::F6 => KeyboardKey::F6,
        Key::F7 => KeyboardKey::F7,
        Key::F8 => KeyboardKey::F8,
        Key::F9 => KeyboardKey::F9,
        Key::F10 => KeyboardKey::F10,
        Key::F11 => KeyboardKey::F11,
        Key::F12 => KeyboardKey::F12,
        Key::F13 => KeyboardKey::F13,
        Key::F14 => KeyboardKey::F14,
        Key::F15 => KeyboardKey::F15,
        Key::Down => KeyboardKey::ArrowDown,
        Key::Left => KeyboardKey::ArrowLeft,
        Key::Right => KeyboardKey::ArrowRight,
        Key::Up => KeyboardKey::ArrowUp,
        Key::Apostrophe => KeyboardKey::Apostrophe,
        Key::Backquote => KeyboardKey::Backquote,
        Key::Backslash => KeyboardKey::Backslash,
        Key::Comma => KeyboardKey::Comma,
        Key::Equal => KeyboardKey::Equal,
        Key::LeftBracket => KeyboardKey::LeftBracket,
        Key::Minus => KeyboardKey::Minus,
        Key::Period => KeyboardKey::Period,
        Key::RightBracket => KeyboardKey::RightBracket,
        Key::Semicolon => KeyboardKey::Semicolon,
        Key::Slash => KeyboardKey::Slash,
        Key::Backspace => KeyboardKey::Backspace,
        Key::Delete => KeyboardKey::Delete,
        Key::End => KeyboardKey::End,
        Key::Enter => KeyboardKey::Enter,
        Key::Escape => KeyboardKey::Escape,
        Key::Home => KeyboardKey::Home,
        Key::Insert => KeyboardKey::Insert,
        Key::Menu => KeyboardKey::ContextMenu,
        Key::PageDown => KeyboardKey::PageDown,
        Key::PageUp => KeyboardKey::PageUp,
        Key::Pause => KeyboardKey::Pause,
        Key::Space => KeyboardKey::Space,
        Key::Tab => KeyboardKey::Tab,
        Key::NumLock => KeyboardKey::NumLock,
        Key::CapsLock => KeyboardKey::CapsLock,
        Key::ScrollLock => KeyboardKey::ScrollLock,
        Key::LeftShift => KeyboardKey::LeftShift,
        Key::RightShift => KeyboardKey::RightShift,
        Key::LeftCtrl => KeyboardKey::LeftControl,
        Key::RightCtrl => KeyboardKey::RightControl,
        Key::NumPad0 => KeyboardKey::Numpad0,
        Key::NumPad1 => KeyboardKey::Numpad1,
        Key::NumPad2 => KeyboardKey::Numpad2,
        Key::NumPad3 => KeyboardKey::Numpad3,
        Key::NumPad4 => KeyboardKey::Numpad4,
        Key::NumPad5 => KeyboardKey::Numpad5,
        Key::NumPad6 => KeyboardKey::Numpad6,
        Key::NumPad7 => KeyboardKey::Numpad7,
        Key::NumPad8 => KeyboardKey::Numpad8,
        Key::NumPad9 => KeyboardKey::Numpad9,
        Key::NumPadDot => KeyboardKey::NumpadDecimal,
        Key::NumPadSlash => KeyboardKey::NumpadDivide,
        Key::NumPadAsterisk => KeyboardKey::NumpadMultiply,
        Key::NumPadMinus => KeyboardKey::NumpadSubtract,
        Key::NumPadPlus => KeyboardKey::NumpadAdd,
        Key::NumPadEnter => KeyboardKey::NumpadEnter,
        Key::LeftAlt => KeyboardKey::LeftAlt,
        Key::RightAlt => KeyboardKey::RightAlt,
        Key::LeftSuper => KeyboardKey::LeftSuper,
        Key::RightSuper => KeyboardKey::RightSuper,
        Key::Unknown | Key::Count => return None,
    })
}

fn map_pointer_to_frame(
    mouse_x: f32,
    mouse_y: f32,
    window_width: usize,
    window_height: usize,
    frame_width: usize,
    frame_height: usize,
) -> (i32, i32) {
    let frame_aspect = frame_width as f32 / frame_height.max(1) as f32;
    let window_aspect = window_width.max(1) as f32 / window_height.max(1) as f32;
    let (display_width, display_height, offset_x, offset_y) = if frame_aspect > window_aspect {
        let display_width = window_width.max(1) as f32;
        let display_height = display_width / frame_aspect;
        (
            display_width,
            display_height,
            0.0,
            (window_height.max(1) as f32 - display_height) / 2.0,
        )
    } else {
        let display_height = window_height.max(1) as f32;
        let display_width = display_height * frame_aspect;
        (
            display_width,
            display_height,
            (window_width.max(1) as f32 - display_width) / 2.0,
            0.0,
        )
    };
    let image_x = (mouse_x - offset_x).clamp(0.0, display_width);
    let image_y = (mouse_y - offset_y).clamp(0.0, display_height);
    let x = (image_x * frame_width as f32 / display_width)
        .floor()
        .clamp(0.0, frame_width.saturating_sub(1) as f32) as i32;
    let y = (image_y * frame_height as f32 / display_height)
        .floor()
        .clamp(0.0, frame_height.saturating_sub(1) as f32) as i32;
    (x, y)
}

impl FrameSink for PreviewWindowSink {
    fn present(&mut self, frame: VideoFrame) -> Result<()> {
        let layout = frame.layout();
        let width = layout.size().width() as usize;
        let height = layout.size().height() as usize;
        // The placeholder is only a bootstrap surface. Recreate it at the
        // first decoded frame's native size so a fitting remote desktop is
        // shown 1:1 instead of remaining a 640x360 scaled window.
        if self.frame_size == Some((640, 360)) && (width, height) != (640, 360) {
            self.window = None;
            self.frame_size = None;
        }
        self.ensure_window(width, height)?;
        self.frame_size = Some((width, height));
        convert_to_minifb(&frame, &mut self.desktop_pixels)?;
        self.render_composited()
    }
}

fn validate_cursor_shape(shape: &CursorShape) -> Result<()> {
    if shape.width == 0
        || shape.height == 0
        || shape.hotspot_x >= shape.width
        || shape.hotspot_y >= shape.height
    {
        return Err(DcError::Codec("invalid cursor shape dimensions".into()));
    }
    let minimum_pitch = match shape.kind {
        1 => shape.width.div_ceil(8),
        2 | 4 => shape
            .width
            .checked_mul(4)
            .ok_or_else(|| DcError::Codec("cursor pitch overflowed".into()))?,
        _ => return Err(DcError::Codec("unknown cursor shape kind".into())),
    };
    if shape.pitch < minimum_pitch {
        return Err(DcError::Codec("cursor shape pitch is too small".into()));
    }
    let rows = if shape.kind == 1 {
        shape.height.saturating_mul(2)
    } else {
        shape.height
    };
    let required = shape
        .pitch
        .checked_mul(rows)
        .and_then(|value| usize::try_from(value).ok())
        .ok_or_else(|| DcError::Codec("cursor shape size overflowed".into()))?;
    if shape.data.len() != required {
        return Err(DcError::Codec(
            "cursor shape payload has the wrong size".into(),
        ));
    }
    Ok(())
}

fn draw_cursor(output: &mut [u32], width: usize, height: usize, cursor: &RemoteCursor) {
    let Some(shape) = cursor.shape.as_ref().filter(|_| cursor.visible) else {
        return;
    };
    let origin_x = cursor.x - shape.hotspot_x as i32;
    let origin_y = cursor.y - shape.hotspot_y as i32;
    for cursor_y in 0..shape.height as usize {
        let y = origin_y + cursor_y as i32;
        if y < 0 || y >= height as i32 {
            continue;
        }
        for cursor_x in 0..shape.width as usize {
            let x = origin_x + cursor_x as i32;
            if x < 0 || x >= width as i32 {
                continue;
            }
            let destination = &mut output[y as usize * width + x as usize];
            match shape.kind {
                1 => draw_monochrome_cursor_pixel(destination, shape, cursor_x, cursor_y),
                2 => blend_color_cursor_pixel(destination, shape, cursor_x, cursor_y, false),
                4 => blend_color_cursor_pixel(destination, shape, cursor_x, cursor_y, true),
                _ => {}
            }
        }
    }
}

fn draw_monochrome_cursor_pixel(pixel: &mut u32, shape: &CursorShape, x: usize, y: usize) {
    let pitch = shape.pitch as usize;
    let byte = x / 8;
    let bit = 7 - x % 8;
    let and_set = shape.data[y * pitch + byte] & (1 << bit) != 0;
    let xor_offset = shape.height as usize * pitch;
    let xor_set = shape.data[xor_offset + y * pitch + byte] & (1 << bit) != 0;
    let and_mask = if and_set { 0x00ff_ffff } else { 0 };
    let xor_mask = if xor_set { 0x00ff_ffff } else { 0 };
    *pixel = (*pixel & and_mask) ^ xor_mask;
}

fn blend_color_cursor_pixel(
    pixel: &mut u32,
    shape: &CursorShape,
    x: usize,
    y: usize,
    masked: bool,
) {
    let offset = y * shape.pitch as usize + x * 4;
    let source = &shape.data[offset..offset + 4];
    let source_rgb =
        (u32::from(source[2]) << 16) | (u32::from(source[1]) << 8) | u32::from(source[0]);
    let alpha = u32::from(source[3]);
    if masked {
        *pixel = if alpha == 0 {
            *pixel ^ source_rgb
        } else {
            source_rgb
        };
        return;
    }
    if alpha == 255 {
        *pixel = source_rgb;
    } else if alpha != 0 {
        let inverse = 255 - alpha;
        let red = (((source_rgb >> 16) & 0xff) * alpha + ((*pixel >> 16) & 0xff) * inverse) / 255;
        let green = (((source_rgb >> 8) & 0xff) * alpha + ((*pixel >> 8) & 0xff) * inverse) / 255;
        let blue = ((source_rgb & 0xff) * alpha + (*pixel & 0xff) * inverse) / 255;
        *pixel = (red << 16) | (green << 8) | blue;
    }
}

impl InputEventSource for PreviewWindowSink {
    fn drain_input_events(&mut self) -> Vec<InputEvent> {
        PreviewWindowSink::drain_input_events(self)
    }
}

fn initial_window_size(width: usize, height: usize) -> (usize, usize) {
    let (max_width, max_height) = local_screen_size()
        .map(|(screen_width, screen_height)| {
            (
                screen_width,
                screen_height.saturating_sub(WINDOW_DECORATION_HEIGHT),
            )
        })
        .unwrap_or((MAX_INITIAL_WIDTH, MAX_INITIAL_HEIGHT));
    if width <= max_width && height <= max_height {
        return (width, height);
    }
    let scale = (max_width as f64 / width as f64).min(max_height as f64 / height as f64);
    (
        (width as f64 * scale).round().max(1.0) as usize,
        (height as f64 * scale).round().max(1.0) as usize,
    )
}

fn convert_to_minifb(frame: &VideoFrame, output: &mut Vec<u32>) -> Result<()> {
    let layout = frame.layout();
    let width = layout.size().width() as usize;
    let height = layout.size().height() as usize;
    output.clear();
    output.reserve(width.saturating_mul(height));

    for row in frame.data().chunks_exact(layout.stride()).take(height) {
        match layout.pixel_format() {
            PixelFormat::Rgb24 => {
                for pixel in row[..width * 3].chunks_exact(3) {
                    output.push(
                        (u32::from(pixel[0]) << 16)
                            | (u32::from(pixel[1]) << 8)
                            | u32::from(pixel[2]),
                    );
                }
            }
            PixelFormat::Bgra32 => {
                for pixel in row[..width * 4].chunks_exact(4) {
                    output.push(
                        (u32::from(pixel[2]) << 16)
                            | (u32::from(pixel[1]) << 8)
                            | u32::from(pixel[0]),
                    );
                }
            }
            PixelFormat::I420 => {
                return Err(DcError::Unsupported(
                    "preview window does not support planar I420 frames".into(),
                ));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use dc_media::{FrameLayout, FrameSize};
    use std::time::Duration;

    #[test]
    fn preview_size_preserves_aspect_ratio() {
        assert_eq!(initial_window_size(640, 480), (640, 480));
        let (width, height) = initial_window_size(3_840, 2_160);
        assert!(width <= local_screen_size().map_or(MAX_INITIAL_WIDTH, |size| size.0));
        assert!(
            height
                <= local_screen_size().map_or(MAX_INITIAL_HEIGHT, |size| {
                    size.1.saturating_sub(WINDOW_DECORATION_HEIGHT)
                })
        );
        assert_eq!(width * 2_160, height * 3_840);
    }

    #[test]
    fn converts_bgra_to_rgb_pixels() {
        let size = FrameSize::new(2, 1).unwrap();
        let layout = FrameLayout::packed(size, PixelFormat::Bgra32).unwrap();
        let frame = VideoFrame::new(
            0,
            Duration::ZERO,
            layout,
            vec![0x33, 0x22, 0x11, 0xff, 0xcc, 0xbb, 0xaa, 0xff],
        )
        .unwrap();
        let mut output = Vec::new();
        convert_to_minifb(&frame, &mut output).unwrap();
        assert_eq!(output, vec![0x00112233, 0x00aabbcc]);
    }

    #[test]
    fn maps_scaled_window_pointer_to_frame_pixels() {
        assert_eq!(
            map_pointer_to_frame(320.0, 180.0, 640, 360, 1_280, 720),
            (640, 360)
        );
        assert_eq!(
            map_pointer_to_frame(999.0, 999.0, 640, 360, 1_280, 720),
            (1_279, 719)
        );
    }

    #[test]
    fn ignores_letterbox_bars_when_mapping_pointer() {
        // A 16:9 frame in a 4:3 window has vertical black bars.
        assert_eq!(map_pointer_to_frame(0.0, 0.0, 800, 600, 1_280, 720), (0, 0));
        assert_eq!(
            map_pointer_to_frame(400.0, 300.0, 800, 600, 1_280, 720),
            (640, 360)
        );
        assert_eq!(
            map_pointer_to_frame(800.0, 600.0, 800, 600, 1_280, 720),
            (1_279, 719)
        );

        // A 4:3 frame in a 16:9 window has horizontal black bars.
        assert_eq!(map_pointer_to_frame(0.0, 0.0, 1_280, 720, 800, 600), (0, 0));
        assert_eq!(
            map_pointer_to_frame(640.0, 360.0, 1_280, 720, 800, 600),
            (400, 300)
        );
    }

    #[test]
    fn applies_regions_without_changing_other_pixels() {
        let mut sink = PreviewWindowSink::new("test");
        sink.frame_size = Some((2, 1));
        sink.desktop_pixels = vec![0x00112233, 0x00445566];
        sink.apply_bgra_regions(
            2,
            1,
            &[DesktopRect {
                x: 1,
                y: 0,
                width: 1,
                height: 1,
            }],
            &[0xcc, 0xbb, 0xaa, 0xff],
        )
        .unwrap();
        assert_eq!(sink.desktop_pixels, vec![0x00112233, 0x00aabbcc]);
    }

    #[test]
    fn cursor_is_composited_without_polluting_the_desktop() {
        let mut sink = PreviewWindowSink::new("test");
        sink.frame_size = Some((2, 1));
        sink.desktop_pixels = vec![0, 0];
        sink.update_remote_cursor(
            true,
            0,
            0,
            Some(CursorShape {
                kind: 2,
                width: 1,
                height: 1,
                hotspot_x: 0,
                hotspot_y: 0,
                pitch: 4,
                data: vec![0, 0, 255, 255],
            }),
        )
        .unwrap();
        assert_eq!(sink.pixels, vec![0x00ff0000, 0]);
        assert_eq!(sink.desktop_pixels, vec![0, 0]);

        sink.update_remote_cursor(true, 1, 0, None).unwrap();
        assert_eq!(sink.pixels, vec![0, 0x00ff0000]);
    }

    #[test]
    fn maps_navigation_modifiers_and_numpad_keys() {
        assert_eq!(map_key(Key::A), Some(KeyboardKey::A));
        assert_eq!(map_key(Key::RightCtrl), Some(KeyboardKey::RightControl));
        assert_eq!(map_key(Key::LeftAlt), Some(KeyboardKey::LeftAlt));
        assert_eq!(map_key(Key::Delete), Some(KeyboardKey::Delete));
        assert_eq!(map_key(Key::NumPadEnter), Some(KeyboardKey::NumpadEnter));
        assert_eq!(map_key(Key::Unknown), None);
    }

    #[test]
    fn wheel_conversion_preserves_fractional_motion() {
        let mut remainder = 60.75;
        assert_eq!(take_wheel_delta(&mut remainder), 60);
        assert!((remainder - 0.75).abs() < f32::EPSILON);
        remainder += 59.25;
        assert_eq!(take_wheel_delta(&mut remainder), 60);
        assert_eq!(remainder, 0.0);
    }
}
