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
