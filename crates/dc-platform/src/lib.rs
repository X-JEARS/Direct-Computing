//! Cross-platform interfaces implemented by OS-specific adapters.

pub use dc_media::FrameSource as ScreenCapturer;

pub trait InputInjector {}
pub trait ClipboardProvider {}
pub trait TerminalBackend {}
pub trait PermissionManager {}
pub trait SystemService {}
