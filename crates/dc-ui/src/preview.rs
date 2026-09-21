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
    last_pointer: Option<(i32, i32, u8)>,
}

impl PreviewWindowSink {
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            base_title: title.into(),
            window: None,
            pixels: Vec::new(),
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
        if let Some((x, y)) = window.get_unscaled_mouse_pos(MouseMode::Clamp) {
            let buttons = u8::from(window.get_mouse_down(MouseButton::Left))
                | (u8::from(window.get_mouse_down(MouseButton::Right)) << 1)
                | (u8::from(window.get_mouse_down(MouseButton::Middle)) << 2);
            let pointer = (x.round() as i32, y.round() as i32, buttons);
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

impl FrameSink for PreviewWindowSink {
    fn present(&mut self, frame: VideoFrame) -> Result<()> {
        let layout = frame.layout();
        let width = layout.size().width() as usize;
        let height = layout.size().height() as usize;
        self.ensure_window(width, height)?;
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
}
