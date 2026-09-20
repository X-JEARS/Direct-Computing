//! Cross-platform interfaces implemented by OS-specific adapters.

pub trait ScreenCapturer {}
pub trait InputInjector {}
pub trait ClipboardProvider {}
pub trait TerminalBackend {}
pub trait PermissionManager {}
pub trait SystemService {}
