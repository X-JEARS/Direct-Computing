//! Cross-platform interfaces implemented by OS-specific adapters.

use dc_common::Result;
pub use dc_media::FrameSource as ScreenCapturer;
use dc_protocol::InputEvent;

#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
pub use windows::WindowsDesktopCapturer;
#[cfg(target_os = "windows")]
mod media_foundation;
#[cfg(target_os = "windows")]
pub use media_foundation::WindowsMediaFoundationH264Encoder;
#[cfg(target_os = "windows")]
mod media_foundation_decoder;
#[cfg(target_os = "windows")]
pub use media_foundation_decoder::WindowsMediaFoundationH264Decoder;

/// Sink for authenticated remote input events.
pub trait InputInjector {
    fn inject(&mut self, event: &InputEvent) -> Result<()>;
}

/// Source of local viewer input events. Implementations return events observed
/// since the previous call.
pub trait InputEventSource {
    fn drain_input_events(&mut self) -> Vec<InputEvent>;
}
pub trait ClipboardProvider {}
pub trait TerminalBackend {}
pub trait PermissionManager {}
pub trait SystemService {}

#[cfg(target_os = "windows")]
pub use windows::WindowsInputInjector;

#[cfg(target_os = "macos")]
mod video_toolbox;
#[cfg(target_os = "macos")]
pub use video_toolbox::VideoToolboxH264Decoder;

/// Read the actual minifb MTKView geometry and mouse position after a Cocoa
/// fullscreen transition.
#[cfg(target_os = "macos")]
pub use macos_window::window_view_geometry;

#[cfg(target_os = "macos")]
mod macos_window {
    use std::ffi::{c_char, c_void, CString};

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct Point {
        x: f64,
        y: f64,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct Size {
        width: f64,
        height: f64,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct Rect {
        origin: Point,
        size: Size,
    }

    #[link(name = "Cocoa", kind = "framework")]
    extern "C" {}

    #[link(name = "objc")]
    unsafe extern "C" {
        fn objc_msgSend();
        fn sel_registerName(name: *const c_char) -> *mut c_void;
    }

    type Id = *mut c_void;

    unsafe fn selector(name: &str) -> *mut c_void {
        let name = CString::new(name).expect("Objective-C selector contains no NUL");
        sel_registerName(name.as_ptr())
    }

    unsafe fn message_id(receiver: Id, name: &str) -> Id {
        let send: unsafe extern "C" fn(Id, *mut c_void) -> Id =
            std::mem::transmute(objc_msgSend as *const ());
        send(receiver, selector(name))
    }

    unsafe fn message_rect(receiver: Id, name: &str) -> Rect {
        let send: unsafe extern "C" fn(Id, *mut c_void) -> Rect =
            std::mem::transmute(objc_msgSend as *const ());
        send(receiver, selector(name))
    }

    unsafe fn message_indexed_id(receiver: Id, name: &str, index: usize) -> Id {
        let send: unsafe extern "C" fn(Id, *mut c_void, usize) -> Id =
            std::mem::transmute(objc_msgSend as *const ());
        send(receiver, selector(name), index)
    }

    unsafe fn message_count(receiver: Id, name: &str) -> usize {
        let send: unsafe extern "C" fn(Id, *mut c_void) -> usize =
            std::mem::transmute(objc_msgSend as *const ());
        send(receiver, selector(name))
    }

    unsafe fn message_point(receiver: Id, name: &str) -> Point {
        let send: unsafe extern "C" fn(Id, *mut c_void) -> Point =
            std::mem::transmute(objc_msgSend as *const ());
        send(receiver, selector(name))
    }

    unsafe fn message_set_frame(receiver: Id, frame: Rect) {
        let send: unsafe extern "C" fn(Id, *mut c_void, Rect) =
            std::mem::transmute(objc_msgSend as *const ());
        send(receiver, selector("setFrame:"), frame);
    }

    unsafe fn message_convert_point(receiver: Id, point: Point, from_view: Id) -> Point {
        let send: unsafe extern "C" fn(Id, *mut c_void, Point, Id) -> Point =
            std::mem::transmute(objc_msgSend as *const ());
        send(
            receiver,
            selector("convertPoint:fromView:"),
            point,
            from_view,
        )
    }

    pub fn window_view_geometry(window_handle: usize) -> Option<(usize, usize, f32, f32)> {
        // SAFETY: minifb documents this handle as an NSWindow. All messages
        // below are sent to objects owned by that window and use Cocoa ABI
        // compatible layouts for NSPoint/NSRect.
        unsafe {
            let window = window_handle as Id;
            if window.is_null() {
                return None;
            }
            let content = message_id(window, "contentView");
            if content.is_null() {
                return None;
            }
            // OSXWindow wraps the actual MTKView in a child content view.
            // Resizing that wrapper (instead of the MTKView) makes the
            // title-bar area visible as a permanent white strip.
            let subviews = message_id(content, "subviews");
            let view = if !subviews.is_null() && message_count(subviews, "count") != 0 {
                message_indexed_id(subviews, "objectAtIndex:", 0)
            } else {
                content
            };
            if view.is_null() {
                return None;
            }
            let parent = message_id(view, "superview");
            if !parent.is_null() {
                // NSWindow's contentLayoutRect excludes the title-bar/toolbar
                // area. The minifb wrapper otherwise reports the full window
                // frame, which vertically centers the image through the title
                // bar instead of centering it in the usable content region.
                let parent_bounds = message_rect(parent, "bounds");
                let layout = message_rect(window, "contentLayoutRect");
                let frame = if layout.size.width > 0.0 && layout.size.height > 0.0 {
                    layout
                } else {
                    parent_bounds
                };
                message_set_frame(view, frame);
            }
            let bounds = message_rect(view, "bounds");
            let window_point = message_point(window, "mouseLocationOutsideOfEventStream");
            let point = message_convert_point(view, window_point, std::ptr::null_mut());
            Some((
                bounds.size.width.max(1.0).round() as usize,
                bounds.size.height.max(1.0).round() as usize,
                point.x as f32,
                (bounds.size.height - point.y) as f32,
            ))
        }
    }
}
