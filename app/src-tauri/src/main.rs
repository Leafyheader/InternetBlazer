//! InternetBlazer desktop app: a Tauri shell around the `blazer-core` engine.
//!
//! The UI calls the commands below and listens for engine events, which are forwarded as
//! `download-event`. When the engine drops events because the UI fell behind, `downloads-resync`
//! tells the UI to reload the whole list. Downloads from the browser extension arrive through
//! [`bridge`].

// Hides the console window in release builds on Windows.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod bridge;

use std::path::PathBuf;

use anyhow::Context;
use blazer_core::{
    Config, DownloadId, DownloadRecord, Engine, NewDownload, Queue, QueueId, Settings,
};
use bridge::{Bridge, Incoming};
use serde::Deserialize;
use tauri::{AppHandle, Emitter, Manager, RunEvent, State};
use tauri_plugin_opener::OpenerExt;
use tokio::sync::broadcast::error::RecvError;

/// Commands report errors to the UI as plain messages.
type CmdResult<T> = Result<T, String>;

fn message(e: impl std::fmt::Display) -> String {
    e.to_string()
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AddRequest {
    url: String,
    dir: PathBuf,
    file_name: Option<String>,
    connections: Option<usize>,
    queue: Option<QueueId>,
    start: bool,
    /// Passed on from the browser.
    #[serde(default)]
    headers: Vec<(String, String)>,
}

#[tauri::command]
async fn list_downloads(engine: State<'_, Engine>) -> CmdResult<Vec<DownloadRecord>> {
    engine.list().map_err(message)
}

#[tauri::command]
async fn get_download(engine: State<'_, Engine>, id: DownloadId) -> CmdResult<DownloadRecord> {
    engine.get(id).map_err(message)
}

#[tauri::command]
fn default_dir() -> Option<PathBuf> {
    dirs::download_dir()
}

#[tauri::command]
async fn add_download(engine: State<'_, Engine>, request: AddRequest) -> CmdResult<DownloadId> {
    let mut new = NewDownload::new(request.url.trim(), request.dir);
    new.file_name = request.file_name.filter(|name| !name.trim().is_empty());
    new.connections = request.connections;
    new.queue = request.queue;
    new.start = request.start;
    new.headers = request.headers;
    engine.add(new).await.map_err(message)
}

// Commands that start downloads must be async: the engine spawns tasks on the tokio runtime,
// which sync commands (run on the main thread) are not part of.
#[tauri::command]
async fn start_download(engine: State<'_, Engine>, id: DownloadId) -> CmdResult<()> {
    engine.start(id).map_err(message)
}

#[tauri::command]
async fn pause_download(engine: State<'_, Engine>, id: DownloadId) -> CmdResult<()> {
    engine.pause(id);
    Ok(())
}

#[tauri::command]
async fn restart_download(engine: State<'_, Engine>, id: DownloadId) -> CmdResult<()> {
    engine.restart(id).await.map_err(message)
}

#[tauri::command]
async fn remove_download(
    engine: State<'_, Engine>,
    id: DownloadId,
    delete_files: bool,
) -> CmdResult<()> {
    engine.remove(id, delete_files).await.map_err(message)
}

#[tauri::command]
async fn set_download_queue(
    engine: State<'_, Engine>,
    id: DownloadId,
    queue: QueueId,
) -> CmdResult<()> {
    engine.set_download_queue(id, queue).map_err(message)
}

#[tauri::command]
async fn set_download_speed_limit(
    engine: State<'_, Engine>,
    id: DownloadId,
    limit: Option<u64>,
) -> CmdResult<()> {
    engine.set_download_speed_limit(id, limit).map_err(message)
}

#[tauri::command]
async fn list_queues(engine: State<'_, Engine>) -> CmdResult<Vec<Queue>> {
    engine.queues().map_err(message)
}

#[tauri::command]
async fn add_queue(engine: State<'_, Engine>, queue: Queue) -> CmdResult<Queue> {
    engine.add_queue(queue).map_err(message)
}

#[tauri::command]
async fn update_queue(engine: State<'_, Engine>, queue: Queue) -> CmdResult<()> {
    engine.update_queue(queue).map_err(message)
}

#[tauri::command]
async fn remove_queue(engine: State<'_, Engine>, id: QueueId) -> CmdResult<()> {
    engine.remove_queue(id).map_err(message)
}

#[tauri::command]
async fn start_queue(engine: State<'_, Engine>, id: QueueId) -> CmdResult<()> {
    engine.start_queue(id).map_err(message)
}

#[tauri::command]
async fn stop_queue(engine: State<'_, Engine>, id: QueueId) -> CmdResult<()> {
    engine.stop_queue(id).map_err(message)
}

#[tauri::command]
async fn get_settings(engine: State<'_, Engine>) -> CmdResult<Settings> {
    engine.settings().map_err(message)
}

#[tauri::command]
async fn set_speed_limit(engine: State<'_, Engine>, limit: Option<u64>) -> CmdResult<()> {
    engine.set_speed_limit(limit).map_err(message)
}

/// Downloads from the browser that wait for the user to confirm them.
#[tauri::command]
fn take_browser_downloads(bridge: State<'_, Bridge>) -> Vec<Incoming> {
    bridge.take()
}

/// Opens a downloaded file with its default application. Only paths of known downloads can be
/// opened, so the UI never needs a general file-opening permission.
#[tauri::command]
async fn open_file(app: AppHandle, engine: State<'_, Engine>, id: DownloadId) -> CmdResult<()> {
    let path = engine.get(id).map_err(message)?.path();
    app.opener().open_path(path.to_string_lossy(), None::<&str>).map_err(message)
}

/// Shows the file in the system file manager, or its folder while it is still downloading.
#[tauri::command]
async fn show_in_folder(
    app: AppHandle,
    engine: State<'_, Engine>,
    id: DownloadId,
) -> CmdResult<()> {
    let record = engine.get(id).map_err(message)?;
    let path = [record.path(), record.part_path()].into_iter().find(|p| p.exists());
    match path {
        Some(path) => app.opener().reveal_item_in_dir(path).map_err(message),
        None => app.opener().open_path(record.dir.to_string_lossy(), None::<&str>).map_err(message),
    }
}

/// Shares its state with the `blazer` CLI. redb allows one process at a time, so the CLI cannot
/// run while the app is open.
fn db_path() -> anyhow::Result<PathBuf> {
    Ok(dirs::data_local_dir()
        .context("no local data directory")?
        .join("InternetBlazer")
        .join("state.redb"))
}

fn forward_events(app: AppHandle, engine: &Engine) {
    let mut events = engine.subscribe();
    tauri::async_runtime::spawn(async move {
        loop {
            match events.recv().await {
                Ok(event) => {
                    let _ = app.emit("download-event", event);
                }
                Err(RecvError::Lagged(_)) => {
                    let _ = app.emit("downloads-resync", ());
                }
                Err(RecvError::Closed) => break,
            }
        }
    });
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    // Already running: show that window instead. The second copy could not open the store anyway.
    if blazer_host::send(&blazer_host::Request::Show).is_ok_and(|r| r.ok) {
        return;
    }

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            let db = db_path()?;
            let engine = Engine::open(&db, Config::default())
                .with_context(|| format!("opening {}", db.display()))?;
            forward_events(app.handle().clone(), &engine);
            tauri::async_runtime::spawn(engine.clone().run_scheduler());
            app.manage(engine);
            app.manage(Bridge::default());
            bridge::serve(app.handle().clone());
            bridge::register_host();
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            list_downloads,
            get_download,
            default_dir,
            add_download,
            start_download,
            pause_download,
            restart_download,
            remove_download,
            set_download_queue,
            set_download_speed_limit,
            list_queues,
            add_queue,
            update_queue,
            remove_queue,
            start_queue,
            stop_queue,
            get_settings,
            set_speed_limit,
            take_browser_downloads,
            open_file,
            show_in_folder,
        ])
        .build(tauri::generate_context!())
        .expect("failed to start InternetBlazer")
        .run(|app, event| {
            // Lets running downloads checkpoint their progress before the process ends.
            if let RunEvent::Exit = event
                && let Some(engine) = app.try_state::<Engine>()
            {
                let engine = engine.inner().clone();
                tauri::async_runtime::block_on(engine.shutdown());
            }
        });
}
