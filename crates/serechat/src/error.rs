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
    /// A streamed response failed after it started (`response.failed` or an
    /// `error` event). The server's codes: `server_error` and
    /// `rate_limit_exceeded` are worth retrying, `context_length_exceeded`
    /// means the input must shrink, `invalid_request_error` is final.
    Response {
        /// Machine-readable error code, when the server sent one.
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
    /// The machine-readable API error code, if any.
    #[must_use]
    pub fn code(&self) -> Option<&str> {
        match self {
            Self::Api { code, .. } | Self::Response { code, .. } => code.as_deref(),
            _ => None,
        }
    }

    /// Whether trying the same request again later may succeed: network
    /// failures, dropped connections, rate limits and server errors.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        use std::io::ErrorKind;
        match self {
            Self::Transport(_) => true,
            Self::Io(e) => matches!(
                e.kind(),
                ErrorKind::ConnectionReset
                    | ErrorKind::ConnectionAborted
                    | ErrorKind::ConnectionRefused
                    | ErrorKind::BrokenPipe
                    | ErrorKind::TimedOut
                    | ErrorKind::UnexpectedEof
                    | ErrorKind::NetworkDown
                    | ErrorKind::NetworkUnreachable
                    | ErrorKind::HostUnreachable
            ),
            Self::Api { status, .. } => *status == 429 || *status >= 500,
            Self::Response { code, .. } => matches!(code.as_deref(), Some("server_error" | "rate_limit_exceeded") | None),
            Self::Decode(_) | Self::Config { .. } | Self::NoHomeDir => false,
        }
    }

    /// Whether the request was too long for the model's context window.
    #[must_use]
    pub fn is_context_overflow(&self) -> bool {
        self.code() == Some("context_length_exceeded")
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport(e) => write!(f, "network error: {e}"),
            Self::Api { status, message, .. } => write!(f, "{message} (HTTP {status})"),
            Self::Response { message, .. } => f.write_str(message),
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
            Self::Api { .. } | Self::Response { .. } | Self::Config { .. } | Self::NoHomeDir => None,
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

#[cfg(test)]
mod tests {
    use super::Error;

    #[test]
    fn classifies_failures() {
        let response = |code: &str| Error::Response { code: Some(code.into()), message: String::new() };
        let api = |status| Error::Api { status, code: None, message: String::new() };
        assert!(response("server_error").is_retryable() && response("rate_limit_exceeded").is_retryable());
        assert!(!response("invalid_request_error").is_retryable());
        assert!(response("context_length_exceeded").is_context_overflow() && !response("context_length_exceeded").is_retryable());
        assert!(api(503).is_retryable() && api(429).is_retryable() && !api(402).is_retryable() && !api(401).is_retryable());
        assert!(Error::Io(std::io::ErrorKind::ConnectionReset.into()).is_retryable());
        assert!(!Error::Io(std::io::Error::other("attachment missing")).is_retryable());
    }
}
