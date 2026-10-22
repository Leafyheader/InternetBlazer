//! Bridge between the InternetBlazer browser extension and the desktop app.
//!
//! The browser starts the `blazer-host` binary over native messaging. It forwards requests to
//! the running app through a local socket that only the current user can reach: a named pipe on
//! Windows, a Unix socket elsewhere. Each connection carries one request and one response, both
//! a single line of JSON.
//!
//! The app serves the socket; this crate holds the protocol, the client and the code that
//! registers the host with browsers.

mod install;

use std::{
    io::{self, BufRead, BufReader, Write},
    path::PathBuf,
};

use serde::{Deserialize, Serialize};

pub use install::{CHROME_EXTENSION_IDS, FIREFOX_EXTENSION_ID, HOST_NAME, install, uninstall};

/// Longest request line the app accepts. Cookies are the only large part.
pub const MAX_REQUEST: u64 = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Request {
    /// Brings the app window to the front.
    Show,
    Download(BrowserDownload),
}

/// A download the browser handed over, with what the server needs to recognize the browser's
/// session.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct BrowserDownload {
    pub url: String,
    pub file_name: Option<String>,
    pub referrer: Option<String>,
    /// A `Cookie` header value.
    pub cookies: Option<String>,
    pub user_agent: Option<String>,
}

impl BrowserDownload {
    /// The request headers that make the server treat the engine like the browser.
    pub fn headers(&self) -> Vec<(String, String)> {
        [("Cookie", &self.cookies), ("Referer", &self.referrer), ("User-Agent", &self.user_agent)]
            .into_iter()
            .filter_map(|(name, value)| {
                let value = value.as_deref()?.trim();
                (!value.is_empty()).then(|| (name.to_string(), value.to_string()))
            })
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Response {
    pub fn ok() -> Self {
        Self { ok: true, error: None }
    }

    pub fn error(message: impl Into<String>) -> Self {
        Self { ok: false, error: Some(message.into()) }
    }
}

/// Where the app listens: a pipe name on Windows, a socket path elsewhere. Unique per user.
pub fn endpoint() -> PathBuf {
    #[cfg(windows)]
    {
        let user: String = std::env::var("USERNAME")
            .unwrap_or_default()
            .chars()
            .filter(char::is_ascii_alphanumeric)
            .collect();
        PathBuf::from(format!(r"\\.\pipe\internetblazer-{user}"))
    }
    #[cfg(unix)]
    {
        // The runtime dir is private to the user. The fallback is made private by the app.
        dirs::runtime_dir()
            .or_else(|| dirs::data_local_dir().map(|d| d.join("InternetBlazer")))
            .unwrap_or_else(std::env::temp_dir)
            .join("internetblazer.sock")
    }
}

/// Whether the app is running and listening.
pub fn is_running() -> bool {
    connect().is_ok()
}

/// Sends one request to the running app. Fails with [`io::ErrorKind::NotFound`] or
/// [`io::ErrorKind::ConnectionRefused`] when the app is not running.
pub fn send(request: &Request) -> io::Result<Response> {
    let stream = connect()?;
    let mut line = serde_json::to_string(request)?;
    line.push('\n');
    (&stream).write_all(line.as_bytes())?;
    (&stream).flush()?;

    let mut reply = String::new();
    BufReader::new(&stream).read_line(&mut reply)?;
    if reply.is_empty() {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "the app closed the connection"));
    }
    Ok(serde_json::from_str(&reply)?)
}

#[cfg(windows)]
fn connect() -> io::Result<std::fs::File> {
    use std::time::{Duration, Instant};

    const ERROR_PIPE_BUSY: i32 = 231;
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match std::fs::OpenOptions::new().read(true).write(true).open(endpoint()) {
            // Another client holds the only free instance; the app makes a new one right away.
            Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) && Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            result => return result,
        }
    }
}

#[cfg(unix)]
fn connect() -> io::Result<std::os::unix::net::UnixStream> {
    let stream = std::os::unix::net::UnixStream::connect(endpoint())?;
    stream.set_read_timeout(Some(std::time::Duration::from_secs(30)))?;
    Ok(stream)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headers_skip_empty_values() {
        let download = BrowserDownload {
            url: "https://example.com/a.zip".into(),
            referrer: Some("https://example.com/".into()),
            cookies: Some("  ".into()),
            ..Default::default()
        };
        assert_eq!(download.headers(), [("Referer".into(), "https://example.com/".into())]);
    }

    #[test]
    fn request_json_shape() {
        let json = r#"{"type":"download","url":"https://x/y","fileName":"y","userAgent":"UA"}"#;
        let request: Request = serde_json::from_str(json).unwrap();
        assert_eq!(
            request,
            Request::Download(BrowserDownload {
                url: "https://x/y".into(),
                file_name: Some("y".into()),
                user_agent: Some("UA".into()),
                ..Default::default()
            })
        );
        assert_eq!(serde_json::to_string(&Response::ok()).unwrap(), r#"{"ok":true}"#);
    }
}
