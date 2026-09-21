use dc_common::{DcError, Result};
use dc_media::{FrameSink, PixelFormat, VideoFrame};
use dc_platform::InputEventSource;
use dc_protocol::InputEvent;
use minifb::{Key, KeyRepeat, MouseButton, MouseMode, ScaleMode, Window, WindowOptions};

const MAX_INITIAL_WIDTH: usize = 1_280;
const MAX_INITIAL_HEIGHT: usize = 720;

pub struct PreviewWindowSink {
    base_title: String,
    window: Option<Window>,
    pixels: Vec<u32>,
    frame_size: Option<(usize, usize)>,
    last_pointer: Option<(i32, i32, u8)>,
}

impl PreviewWindowSink {
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            base_title: title.into(),
            window: None,
            pixels: Vec::new(),
            frame_size: None,
            last_pointer: None,
        }
    }

    pub fn is_open(&self) -> bool {
        self.window
            .as_ref()
            .is_none_or(|window| window.is_open() && !window.is_key_down(Key::Escape))
    }

    pub fn pump_events(&mut self) {
        if let Some(window) = &mut self.window {
            window.update();
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
        if let Some((mouse_x, mouse_y)) = window.get_unscaled_mouse_pos(MouseMode::Clamp) {
            let (window_width, window_height) = window.get_size();
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
        for key in window.get_keys_pressed(KeyRepeat::No) {
            events.push(InputEvent::Key {
                code: key as u32,
                pressed: true,
            });
        }
        for key in window.get_keys_released() {
            events.push(InputEvent::Key {
                code: key as u32,
                pressed: false,
            });
        }
        events
    }

    pub fn set_status(&mut self, status: &str) {
        if let Some(window) = &mut self.window {
            window.set_title(&format!("{} | {status}", self.base_title));
        }
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
        self.ensure_window(width, height)?;
        self.frame_size = Some((width, height));
        convert_to_minifb(&frame, &mut self.pixels)?;
        self.window
            .as_mut()
            .ok_or_else(|| DcError::Platform("preview window was not created".into()))?
            .update_with_buffer(&self.pixels, width, height)
            .map_err(|error| DcError::Platform(format!("update preview window: {error}")))
    }
}

impl InputEventSource for PreviewWindowSink {
    fn drain_input_events(&mut self) -> Vec<InputEvent> {
        PreviewWindowSink::drain_input_events(self)
    }
}

fn initial_window_size(width: usize, height: usize) -> (usize, usize) {
    if width <= MAX_INITIAL_WIDTH && height <= MAX_INITIAL_HEIGHT {
        return (width, height);
    }
    let scale =
        (MAX_INITIAL_WIDTH as f64 / width as f64).min(MAX_INITIAL_HEIGHT as f64 / height as f64);
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
        assert_eq!(initial_window_size(3_840, 2_160), (1_280, 720));
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
}
