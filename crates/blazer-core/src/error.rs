use std::{
    fmt::Display,
    io,
    time::{Duration, SystemTime},
};

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
    #[error("{}", network_message(.0))]
    Network(#[from] reqwest::Error),
    #[error("{}", status_message(*.0))]
    Status(u16),
    /// HTTP 429 or 503: the server wants fewer connections or requests.
    #[error("server is limiting connections (HTTP {status})")]
    Throttled { status: u16, retry_after: Option<Duration> },
    #[error("server ignored the byte-range request")]
    RangeIgnored,
    /// A `206 Partial Content` response that does not say which bytes it holds.
    #[error("server sent part of the file without saying which part")]
    MalformedRange,
    /// A `206 Partial Content` response with more bytes than the range it says it holds, so
    /// the sizes the server gives cannot be trusted either.
    #[error("server sent more data than the part of the file it said it was sending")]
    RangeOverflow,
    #[error("the file on the server changed since the download started")]
    RemoteChanged,
    #[error("connection closed before the segment was complete")]
    Truncated,
    #[error("download stopped before it was complete")]
    Incomplete,
    #[error("the downloaded file has {actual} bytes instead of {expected}")]
    WrongSize { expected: u64, actual: u64 },
    #[error(
        "not enough disk space: {} more is needed, but only {} is free",
        crate::fsio::format_bytes(*.needed),
        crate::fsio::format_bytes(*.available)
    )]
    NoSpace { needed: u64, available: u64 },
    #[error("{}", io_message(.0))]
    Io(#[from] io::Error),
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
            Error::Throttled { .. } | Error::Truncated | Error::Incomplete => true,
            _ => false,
        }
    }

    /// How long the server asked to wait before the next request.
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Error::Throttled { retry_after, .. } => *retry_after,
            _ => None,
        }
    }

    pub(crate) fn from_response(status: StatusCode, headers: &HeaderMap) -> Self {
        match status {
            StatusCode::TOO_MANY_REQUESTS | StatusCode::SERVICE_UNAVAILABLE => {
                let retry_after = headers
                    .get(header::RETRY_AFTER)
                    .and_then(|v| parse_retry_after(v.to_str().ok()?, SystemTime::now()));
                Error::Throttled { status: status.as_u16(), retry_after }
            }
            status => Error::Status(status.as_u16()),
        }
    }

    pub(crate) fn internal(e: impl Display) -> Self {
        Error::Internal(e.to_string())
    }
}

/// `Retry-After` is either a number of seconds or an HTTP date.
fn parse_retry_after(value: &str, now: SystemTime) -> Option<Duration> {
    let value = value.trim();
    if let Ok(secs) = value.parse() {
        return Some(Duration::from_secs(secs));
    }
    let at = httpdate::parse_http_date(value).ok()?;
    Some(at.duration_since(now).unwrap_or(Duration::ZERO))
}

/// Says what the status means for the download, since the bare number rarely helps.
fn status_message(code: u16) -> String {
    let meaning = match code {
        401 => "the server wants you to sign in",
        403 => {
            "access denied: the link may have expired, or the site needs you to be signed in.              Start the download again from the browser, or give it a new address"
        }
        404 => "the file was not found on the server",
        407 => "the proxy wants you to sign in",
        410 => "the file is no longer available on the server",
        416 => "the server rejected the requested part of the file",
        500..=599 => "the server had a problem",
        _ => return format!("server responded with HTTP {code}"),
    };
    format!("{meaning} (HTTP {code})")
}

fn network_message(e: &reqwest::Error) -> String {
    let what = if e.is_timeout() {
        "the server stopped responding"
    } else if e.is_connect() {
        "could not connect to the server"
    } else {
        "network error"
    };
    format!("{what}: {}", with_causes(e))
}

fn io_message(e: &io::Error) -> String {
    match e.kind() {
        io::ErrorKind::StorageFull => "the disk is full: free up some space, then resume".into(),
        io::ErrorKind::PermissionDenied => format!("no permission to write the file: {e}"),
        _ => format!("I/O error: {e}"),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_retry_after() {
        let now = httpdate::parse_http_date("Sun, 06 Nov 1994 08:49:37 GMT").unwrap();
        assert_eq!(parse_retry_after(" 120 ", now), Some(Duration::from_secs(120)));
        assert_eq!(
            parse_retry_after("Sun, 06 Nov 1994 08:50:07 GMT", now),
            Some(Duration::from_secs(30))
        );
        // A date in the past means no wait.
        assert_eq!(parse_retry_after("Sun, 06 Nov 1994 08:00:00 GMT", now), Some(Duration::ZERO));
        assert_eq!(parse_retry_after("soon", now), None);
    }

    #[test]
    fn explains_common_statuses() {
        assert!(Error::Status(403).to_string().contains("expired"));
        assert_eq!(Error::Status(418).to_string(), "server responded with HTTP 418");
    }
}
