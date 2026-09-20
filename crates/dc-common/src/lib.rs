//! Cross-platform primitives shared by all Direct Computing components.

pub mod error;
pub mod logging;

pub use error::{DcError, Result};
pub use logging::{init_logging, log, LogLevel};
