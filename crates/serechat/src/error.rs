//! The single error type used across the crate.

use std::fmt;

/// Convenience alias for results produced by this crate.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Everything that can go wrong while talking to SereChat or touching the
/// local configuration.
#[derive(Debug)]
pub enum Error {
    /// The request never produced an HTTP response: DNS, TCP, TLS or a
    /// protocol-level failure.
    Transport(Box<ureq::Error>),
    /// The server answered with a non-success status code.
    Api {
        /// HTTP status code.
        status: u16,
        /// Machine-readable error code (`error.code`), when the server sent one.
        code: Option<String>,
        /// Human-readable description suitable for display.
        message: String,
    },
    /// A response body could not be decoded into the expected shape.
    Decode(serde_json::Error),
    /// A filesystem operation failed.
    Io(std::io::Error),
    /// The configuration file is malformed.
    Config {
        /// One-based line number of the offending line.
        line: usize,
        /// What is wrong with it.
        message: &'static str,
    },
    /// The user's home directory could not be determined.
    NoHomeDir,
}

impl Error {
    /// Returns `true` when the server rejected our credentials, meaning the
    /// user has to sign in again.
    #[must_use]
    pub fn is_unauthorized(&self) -> bool {
        matches!(self, Self::Api { status: 401, .. })
    }

    /// The machine-readable API error code, if any.
    #[must_use]
    pub fn code(&self) -> Option<&str> {
        match self {
            Self::Api { code, .. } => code.as_deref(),
            _ => None,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport(e) => write!(f, "network error: {e}"),
            Self::Api { status, message, .. } => write!(f, "{message} (HTTP {status})"),
            Self::Decode(e) => write!(f, "malformed JSON: {e}"),
            Self::Io(e) => write!(f, "I/O error: {e}"),
            Self::Config { line, message } => write!(f, "config line {line}: {message}"),
            Self::NoHomeDir => f.write_str("could not determine the home directory"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Transport(e) => Some(e.as_ref()),
            Self::Decode(e) => Some(e),
            Self::Io(e) => Some(e),
            Self::Api { .. } | Self::Config { .. } | Self::NoHomeDir => None,
        }
    }
}

impl From<ureq::Error> for Error {
    fn from(e: ureq::Error) -> Self {
        // Body read failures surface as `ureq::Error::Io`; keep them as I/O.
        match e {
            ureq::Error::Io(io) => Self::Io(io),
            other => Self::Transport(Box::new(other)),
        }
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Self::Decode(e)
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}
