use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl fmt::Display for LogLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = match self {
            Self::Error => "ERROR",
            Self::Warn => "WARN",
            Self::Info => "INFO",
            Self::Debug => "DEBUG",
            Self::Trace => "TRACE",
        };
        f.write_str(value)
    }
}

/// Initializes the process-wide logging hook.
///
/// The first implementation deliberately uses only the standard library. It provides a
/// stable logging seam without committing the workspace to a particular logging backend.
pub fn init_logging() {
    // Kept as an explicit entry point so applications have one consistent initialization call.
}

pub fn log(level: LogLevel, target: &str, message: &str) {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default();
    eprintln!("{timestamp} {level} {target}: {message}");
}
