//! Discovering size, range support and validators before downloading.

use reqwest::{
    Client, RequestBuilder, StatusCode,
    header::{self, HeaderMap},
};

use crate::{
    error::{Error, Result},
    filename,
};

#[derive(Debug, Clone)]
pub(crate) struct ProbeInfo {
    pub total: Option<u64>,
    pub resumable: bool,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub file_name: Option<String>,
}

impl ProbeInfo {
    /// Compares validators against a previous probe. A validator that either side lacks is
    /// skipped, since some servers send them inconsistently.
    pub fn matches(&self, etag: Option<&str>, last_modified: Option<&str>) -> bool {
        match (etag, self.etag.as_deref()) {
            (Some(old), Some(new)) => old == new,
            _ => match (last_modified, self.last_modified.as_deref()) {
                (Some(old), Some(new)) => old == new,
                _ => true,
            },
        }
    }
}

pub(crate) fn request(client: &Client, url: &str, headers: &[(String, String)]) -> RequestBuilder {
    headers.iter().fold(client.get(url), |req, (name, value)| req.header(name, value))
}

/// Requests the first byte instead of sending `HEAD`, which many servers handle badly.
pub(crate) async fn probe(
    client: &Client,
    url: &str,
    headers: &[(String, String)],
) -> Result<ProbeInfo> {
    let resp = request(client, url, headers).header(header::RANGE, "bytes=0-0").send().await?;
    let h = resp.headers();
    let (total, resumable) = match resp.status() {
        StatusCode::PARTIAL_CONTENT => {
            let total = content_range(h).and_then(|r| r.total);
            (total, total.is_some())
        }
        // An empty file cannot satisfy `bytes=0-0`.
        StatusCode::RANGE_NOT_SATISFIABLE if content_range(h).and_then(|r| r.total) == Some(0) => {
            (Some(0), false)
        }
        status if status.is_success() => (resp.content_length(), false),
        status => return Err(Error::from_response(status, h)),
    };
    let file_name = header_str(h, header::CONTENT_DISPOSITION)
        .and_then(|cd| filename::from_content_disposition(&cd))
        .or_else(|| filename::from_url(resp.url()));
    Ok(ProbeInfo {
        total,
        resumable,
        etag: header_str(h, header::ETAG),
        last_modified: header_str(h, header::LAST_MODIFIED),
        file_name,
    })
}

/// Picks the validator for `If-Range`, which only accepts a strong ETag or a date.
pub(crate) fn if_range(etag: Option<&str>, last_modified: Option<&str>) -> Option<String> {
    etag.filter(|e| !e.starts_with("W/")).or(last_modified).map(str::to_string)
}

fn header_str(headers: &HeaderMap, name: header::HeaderName) -> Option<String> {
    headers.get(name)?.to_str().ok().map(str::to_string)
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ContentRange {
    /// `None` for the `bytes */total` form.
    pub start: Option<u64>,
    pub total: Option<u64>,
}

pub(crate) fn content_range(headers: &HeaderMap) -> Option<ContentRange> {
    parse_content_range(headers.get(header::CONTENT_RANGE)?.to_str().ok()?)
}

fn parse_content_range(value: &str) -> Option<ContentRange> {
    let rest = value.trim().strip_prefix("bytes")?.trim_start();
    let (range, total) = rest.split_once('/')?;
    let total = total.trim().parse().ok();
    let start = match range.trim() {
        "*" => None,
        range => Some(range.split_once('-')?.0.trim().parse().ok()?),
    };
    Some(ContentRange { start, total })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_content_range() {
        let parse = parse_content_range;
        assert_eq!(
            parse("bytes 0-0/1234"),
            Some(ContentRange { start: Some(0), total: Some(1234) })
        );
        assert_eq!(parse("bytes 100-199/*"), Some(ContentRange { start: Some(100), total: None }));
        assert_eq!(parse("bytes */0"), Some(ContentRange { start: None, total: Some(0) }));
        assert_eq!(parse("items 0-1/2"), None);
    }

    #[test]
    fn if_range_skips_weak_etags() {
        assert_eq!(if_range(Some("\"abc\""), None).as_deref(), Some("\"abc\""));
        assert_eq!(if_range(Some("W/\"abc\""), Some("date")).as_deref(), Some("date"));
        assert_eq!(if_range(Some("W/\"abc\""), None), None);
    }
}
