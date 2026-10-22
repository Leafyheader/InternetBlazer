use std::{fmt::Display, time::Duration};

use reqwest::{
    StatusCode,
    header::{self, HeaderMap},
};

use crate::types::{DownloadId, QueueId};

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid URL: {0}")]
    InvalidUrl(String),
    #[error("network error: {}", with_causes(.0))]
    Network(#[from] reqwest::Error),
    #[error("server responded with HTTP {0}")]
    Status(u16),
    /// HTTP 429 or 503: the server wants fewer connections or requests.
    #[error("server is limiting connections (HTTP {status})")]
    Throttled { status: u16, retry_after: Option<Duration> },
    #[error("server ignored the byte-range request")]
    RangeIgnored,
    #[error("the file on the server changed since the download started")]
    RemoteChanged,
    #[error("connection closed before the segment was complete")]
    Truncated,
    #[error("download stopped before it was complete")]
    Incomplete,
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("database error: {0}")]
    Store(String),
    #[error("download {0} not found")]
    NotFound(DownloadId),
    #[error("queue {0} not found")]
    QueueNotFound(QueueId),
    #[error("{0}")]
    Invalid(String),
    #[error("internal error: {0}")]
    Internal(String),
}

impl Error {
    /// Whether retrying the same request may succeed.
    pub fn is_retryable(&self) -> bool {
        match self {
            Error::Network(e) => !(e.is_builder() || e.is_redirect()),
            Error::Status(code) => *code >= 500 || *code == 408,
            Error::Throttled { .. } | Error::Truncated => true,
            _ => false,
        }
    }

    pub(crate) fn from_response(status: StatusCode, headers: &HeaderMap) -> Self {
        match status {
            StatusCode::TOO_MANY_REQUESTS | StatusCode::SERVICE_UNAVAILABLE => {
                // Only the delay-seconds form; HTTP dates are rare here.
                let retry_after = headers
                    .get(header::RETRY_AFTER)
                    .and_then(|v| v.to_str().ok()?.trim().parse().ok())
                    .map(Duration::from_secs);
                Error::Throttled { status: status.as_u16(), retry_after }
            }
            status => Error::Status(status.as_u16()),
        }
    }

    pub(crate) fn internal(e: impl Display) -> Self {
        Error::Internal(e.to_string())
    }
}

/// reqwest's own message rarely says what went wrong; the cause chain does.
fn with_causes(e: &dyn std::error::Error) -> String {
    let mut message = e.to_string();
    let mut source = e.source();
    while let Some(cause) = source {
        message.push_str(": ");
        message.push_str(&cause.to_string());
        source = cause.source();
    }
    message
}
