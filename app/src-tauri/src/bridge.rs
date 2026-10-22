//! Serves requests from the browser extension, which `blazer-host` forwards over a local socket.
//!
//! A browser download normally waits in [`Bridge`] until the UI takes it and shows it in the add
//! dialog, so the user picks where it goes; `browser-download` tells the UI that one is waiting.
//! Users can turn that off, and then it starts right away in the usual folder.

use std::path::PathBuf;

use blazer_core::{Engine, NewDownload};
use blazer_host::{BrowserDownload, MAX_REQUEST, Request, Response};
use parking_lot::Mutex;
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};

use crate::prefs::PrefsStore;

/// A download from the browser, as the add dialog fills it in.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Incoming {
    url: String,
    file_name: Option<String>,
    headers: Vec<(String, String)>,
}

#[derive(Default)]
pub struct Bridge {
    pending: Mutex<Vec<Incoming>>,
}

impl Bridge {
    pub fn take(&self) -> Vec<Incoming> {
        std::mem::take(&mut self.pending.lock())
    }
}

/// Brings the main window to the front.
pub fn show_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
    }
}

fn handle(app: &AppHandle, request: Request) -> Response {
    match request {
        Request::Show => show_window(app),
        Request::Download(download) => {
            let scheme = download.url.split_once(':').map(|(s, _)| s.to_ascii_lowercase());
            if !matches!(scheme.as_deref(), Some("http" | "https")) {
                return Response::error("only http and https links can be downloaded");
            }
            let headers = download.headers();
            let BrowserDownload { url, file_name, .. } = download;
            let incoming = Incoming { url, file_name, headers };
            let prefs = app.state::<PrefsStore>().get();
            match prefs.target_dir().filter(|_| !prefs.confirm_browser_downloads) {
                Some(dir) => {
                    tauri::async_runtime::spawn(add_now(app.clone(), incoming, dir));
                }
                None => confirm(app, incoming),
            }
        }
    }
    Response::ok()
}

/// Waits for the user to confirm the download in the add dialog.
fn confirm(app: &AppHandle, incoming: Incoming) {
    app.state::<Bridge>().pending.lock().push(incoming);
    let _ = app.emit("browser-download", ());
    show_window(app);
}

/// Adds the download without asking. One that can't be added goes to the add dialog instead,
/// which shows why.
async fn add_now(app: AppHandle, incoming: Incoming, dir: PathBuf) {
    let mut new = NewDownload::new(&incoming.url, dir);
    new.file_name = incoming.file_name.clone();
    new.headers = incoming.headers.clone();
    let engine = app.state::<Engine>();
    match engine.add(new).await {
        Ok(id) => {
            // The download left the browser without a trace there, so say where it went.
            if let Ok(record) = engine.get(id) {
                crate::notify(&app, "Downloading", record.file_name);
            }
        }
        Err(e) => {
            tracing::warn!("adding a browser download: {e}");
            confirm(&app, incoming);
        }
    }
}

async fn respond(app: AppHandle, stream: impl AsyncRead + AsyncWrite + Unpin) {
    let (read, mut write) = tokio::io::split(stream);
    let mut line = String::new();
    if let Err(e) = BufReader::new(read.take(MAX_REQUEST)).read_line(&mut line).await {
        tracing::debug!("bridge: reading request: {e}");
        return;
    }
    // A client that only checks whether the app runs sends nothing.
    if line.trim().is_empty() {
        return;
    }
    let response = match serde_json::from_str(&line) {
        Ok(request) => handle(&app, request),
        Err(e) => Response::error(format!("bad request: {e}")),
    };
    let mut reply = serde_json::to_string(&response).expect("a response serializes");
    reply.push('\n');
    let _ = write.write_all(reply.as_bytes()).await;
    let _ = write.shutdown().await;
}

/// Listens until the app exits. Logs and gives up when the endpoint is taken.
pub fn serve(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        if let Err(e) = listen(app).await {
            tracing::warn!("browser bridge unavailable: {e}");
        }
    });
}

#[cfg(windows)]
async fn listen(app: AppHandle) -> std::io::Result<()> {
    use tokio::net::windows::named_pipe::ServerOptions;

    let name = blazer_host::endpoint();
    // Fails when another process already owns the name, instead of sharing it.
    let mut server = ServerOptions::new().first_pipe_instance(true).create(&name)?;
    loop {
        server.connect().await?;
        let client = std::mem::replace(&mut server, ServerOptions::new().create(&name)?);
        tauri::async_runtime::spawn(respond(app.clone(), client));
    }
}

#[cfg(unix)]
async fn listen(app: AppHandle) -> std::io::Result<()> {
    use std::{fs, os::unix::fs::PermissionsExt};

    let path = blazer_host::endpoint();
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    // Left behind by an app that crashed: a running one would have answered at startup.
    let _ = fs::remove_file(&path);
    let listener = tokio::net::UnixListener::bind(&path)?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    loop {
        let (client, _) = listener.accept().await?;
        tauri::async_runtime::spawn(respond(app.clone(), client));
    }
}

/// Registers the `blazer-host` installed next to the app with the browsers, so the extension
/// finds this copy of the app.
pub fn register_host() {
    let Ok(exe) = std::env::current_exe() else { return };
    let host: PathBuf =
        exe.with_file_name(if cfg!(windows) { "blazer-host.exe" } else { "blazer-host" });
    if !host.exists() {
        return;
    }
    tauri::async_runtime::spawn_blocking(move || {
        if let Err(e) = blazer_host::install(&host) {
            tracing::warn!("registering {} with browsers: {e}", host.display());
        }
    });
}
