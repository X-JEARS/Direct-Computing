use std::fmt;

/// The common error type used at crate boundaries.
#[derive(Debug)]
pub enum DcError {
    InvalidInput(String),
    Unsupported(String),
    Io(std::io::Error),
}

impl fmt::Display for DcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidInput(message) => write!(f, "invalid input: {message}"),
            Self::Unsupported(message) => write!(f, "unsupported: {message}"),
            Self::Io(error) => write!(f, "I/O error: {error}"),
        }
    }
}

impl std::error::Error for DcError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::InvalidInput(_) | Self::Unsupported(_) => None,
        }
    }
}

impl From<std::io::Error> for DcError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

pub type Result<T> = std::result::Result<T, DcError>;
