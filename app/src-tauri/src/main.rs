//! InternetBlazer desktop app: a Tauri shell around the `blazer-core` engine.
//!
//! The UI calls the commands below and listens for engine events, which are forwarded as
//! `download-event`. When the engine drops events because the UI fell behind, `downloads-resync`
//! tells the UI to reload the whole list. Downloads from the browser extension arrive through
//! [`bridge`].

// Hides the console window in release builds on Windows.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod bridge;
mod categories;
mod finish;
mod icons;
mod prefs;
mod tray;

use std::path::PathBuf;

use anyhow::Context;
use futures_util::StreamExt;
use blazer_core::{
    CheckState, Config, DownloadId, DownloadRecord, Engine, Event, LinkInfo, MediaFormat, MediaInfo,
    MediaTools, NewDownload, Queue, QueueId, Settings, Status, Tool,
};
use bridge::{Bridge, Incoming};
use categories::{Category, CategoryInfo};
use finish::{Finish, FinishAction};
use prefs::{Prefs, PrefsStore};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, RunEvent, State, WindowEvent, ipc::Response};
use tauri_plugin_autostart::ManagerExt as _;
use tauri_plugin_notification::NotificationExt;
use tauri_plugin_opener::OpenerExt;
use tauri_plugin_updater::UpdaterExt;
use tokio::sync::broadcast::error::RecvError;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

/// Passed by the login item, which starts the app in the tray.
const HIDDEN_ARG: &str = "--hidden";

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
    /// The video on the page to download, rather than the page.
    #[serde(default)]
    media: Option<MediaFormat>,
    /// The SHA-256 the file should have, checked once it finishes.
    #[serde(default)]
    sha256: Option<String>,
}

#[tauri::command]
async fn list_downloads(engine: State<'_, Engine>) -> CmdResult<Vec<DownloadRecord>> {
    engine.list().map_err(message)
}

#[tauri::command]
async fn get_download(engine: State<'_, Engine>, id: DownloadId) -> CmdResult<DownloadRecord> {
    engine.get(id).map_err(message)
}

/// The folder the add dialog suggests for a file, going by its name.
#[tauri::command]
fn default_dir(prefs: State<'_, PrefsStore>, file_name: &str, media: bool) -> Option<PathBuf> {
    prefs.get().target_dir(Category::of(file_name, media))
}

/// The file types downloads are sorted by, so the UI sorts them the same way.
#[tauri::command]
fn file_categories() -> Vec<CategoryInfo> {
    categories::all()
}

/// Where downloads go when the user hasn't picked a folder.
#[tauri::command]
fn system_download_dir() -> Option<PathBuf> {
    dirs::download_dir()
}

/// Size and name of what a link points to, for the add dialog to show before downloading.
#[tauri::command]
async fn inspect_link(
    engine: State<'_, Engine>,
    url: String,
    headers: Vec<(String, String)>,
) -> CmdResult<LinkInfo> {
    engine.inspect(url.trim(), &headers).await.map_err(message)
}

/// The videos on a web page and the qualities they come in.
#[tauri::command]
async fn inspect_media(
    engine: State<'_, Engine>,
    url: String,
    headers: Vec<(String, String)>,
) -> CmdResult<MediaInfo> {
    engine.inspect_media(url.trim(), &headers).await.map_err(message)
}

#[tauri::command]
async fn media_tools(engine: State<'_, Engine>) -> CmdResult<MediaTools> {
    Ok(engine.media_tools().await)
}

/// Installs or updates yt-dlp.
#[tauri::command]
async fn install_ytdlp(engine: State<'_, Engine>) -> CmdResult<Tool> {
    engine.install_ytdlp().await.map_err(message)
}

/// Installs or updates deno, which yt-dlp needs for every YouTube format.
#[tauri::command]
async fn install_deno(engine: State<'_, Engine>) -> CmdResult<Tool> {
    engine.install_deno().await.map_err(message)
}

/// `None` when the disk does not say, such as some network drives.
#[tauri::command]
async fn free_space(dir: PathBuf) -> Option<u64> {
    blazer_core::free_space(&dir).ok()
}

#[tauri::command]
async fn add_download(
    engine: State<'_, Engine>,
    prefs: State<'_, PrefsStore>,
    request: AddRequest,
) -> CmdResult<DownloadId> {
    let dir = request.dir.clone();
    let mut new = NewDownload::new(request.url.trim(), request.dir);
    new.file_name = request.file_name.filter(|name| !name.trim().is_empty());
    new.connections = request.connections;
    new.queue = request.queue;
    new.start = request.start;
    new.headers = request.headers;
    new.media = request.media;
    new.sha256 = request.sha256.filter(|hash| !hash.trim().is_empty());
    let id = engine.add(new).await.map_err(message)?;
    if let Err(e) = prefs.update(|p| p.last_dir = Some(dir)) {
        tracing::warn!("saving the last folder used: {e:#}");
    }
    Ok(id)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ImportRequest {
    urls: Vec<String>,
    /// One folder for every link, or none to put each file in the folder for its type.
    dir: Option<PathBuf>,
    queue: QueueId,
    start: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ImportFailure {
    url: String,
    error: String,
}

/// How many links of a list are checked with their servers at once.
const IMPORT_AT_ONCE: usize = 6;

/// Adds a list of links, a few at a time, and returns the ones that couldn't be added.
#[tauri::command]
async fn import_links(
    engine: State<'_, Engine>,
    prefs: State<'_, PrefsStore>,
    request: ImportRequest,
) -> CmdResult<Vec<ImportFailure>> {
    let engine = engine.inner().clone();
    let current = prefs.get();
    let ImportRequest { urls, dir, queue, start } = request;
    let failures = futures_util::stream::iter(urls)
        .map(|url| {
            let (engine, current, dir) = (&engine, &current, dir.clone());
            async move {
                let error = import_link(engine, current, &url, dir, queue, start).await.err()?;
                Some(ImportFailure { url, error })
            }
        })
        .buffered(IMPORT_AT_ONCE)
        .filter_map(std::future::ready)
        .collect()
        .await;
    if let Some(dir) = dir
        && let Err(e) = prefs.update(|p| p.last_dir = Some(dir))
    {
        tracing::warn!("saving the last folder used: {e:#}");
    }
    Ok(failures)
}

async fn import_link(
    engine: &Engine,
    prefs: &Prefs,
    url: &str,
    dir: Option<PathBuf>,
    queue: QueueId,
    start: bool,
) -> CmdResult<DownloadId> {
    let url = url.trim();
    let dir = match dir {
        Some(dir) => dir,
        None => {
            // The name at the end of the link decides the folder. A link without one, such as
            // download.php?id=7, asks the server.
            let path = url.split(['?', '#']).next().unwrap_or(url);
            let mut name = path.rsplit('/').next().unwrap_or("").to_owned();
            if !name.contains('.')
                && let Ok(info) = engine.inspect(url, &[]).await
            {
                name = info.file_name.unwrap_or(name);
            }
            prefs
                .category_dir(Category::of(&name, false))
                .ok_or("there's no Downloads folder to save in; pick a folder")?
        }
    };
    let mut new = NewDownload::new(url, dir);
    new.queue = Some(queue);
    new.start = start;
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
async fn set_download_url(engine: State<'_, Engine>, id: DownloadId, url: String) -> CmdResult<()> {
    engine.set_url(id, &url).await.map_err(message)
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

/// Sets the SHA-256 the file should have, from text holding one, or forgets it with `None`.
#[tauri::command]
async fn set_download_sha256(
    engine: State<'_, Engine>,
    id: DownloadId,
    sha256: Option<String>,
) -> CmdResult<()> {
    engine.set_sha256(id, sha256.as_deref()).map_err(message)
}

#[tauri::command]
async fn check_download_sha256(engine: State<'_, Engine>, id: DownloadId) -> CmdResult<()> {
    engine.check_sha256(id).map_err(message)
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

/// The engine's settings, with those left unset filled in from its configuration.
/// What each queue does when it finishes, for the queues that do something.
#[tauri::command]
fn finish_actions(finish: State<'_, Finish>) -> std::collections::HashMap<QueueId, FinishAction> {
    finish.actions()
}

/// Sets what the queue does when it next finishes, or clears it with `None`.
#[tauri::command]
fn set_finish_action(
    app: AppHandle,
    finish: State<'_, Finish>,
    queue: QueueId,
    action: Option<FinishAction>,
) {
    finish.set(&app, queue, action);
}

#[tauri::command]
fn cancel_finish(app: AppHandle, finish: State<'_, Finish>) {
    finish.cancel(&app);
}

#[tauri::command]
fn finish_now(app: AppHandle, finish: State<'_, Finish>) {
    finish.now(&app);
}

#[tauri::command]
async fn get_settings(engine: State<'_, Engine>) -> CmdResult<Settings> {
    let settings = engine.settings().map_err(message)?;
    let config = engine.config();
    Ok(Settings {
        connections: settings.connections.or(Some(config.connections)),
        auto_retries: settings.auto_retries.or(Some(config.auto_retries)),
        ..settings
    })
}

#[tauri::command]
async fn set_settings(engine: State<'_, Engine>, settings: Settings) -> CmdResult<()> {
    engine.set_settings(settings).map_err(message)
}

#[tauri::command]
fn get_prefs(prefs: State<'_, PrefsStore>) -> Prefs {
    prefs.get()
}

/// Leaves alone what the app keeps track of itself.
#[tauri::command]
fn set_prefs(prefs: State<'_, PrefsStore>, new: Prefs) -> CmdResult<()> {
    prefs
        .update(|p| {
            *p =
                Prefs { last_dir: p.last_dir.take(), tray_notice_shown: p.tray_notice_shown, ..new }
        })
        .map_err(|e| format!("{e:#}"))
}

#[tauri::command]
fn get_launch_at_login(app: AppHandle) -> CmdResult<bool> {
    app.autolaunch().is_enabled().map_err(message)
}

#[tauri::command]
fn set_launch_at_login(app: AppHandle, enabled: bool) -> CmdResult<()> {
    let autolaunch = app.autolaunch();
    match enabled {
        true => autolaunch.enable(),
        false => autolaunch.disable(),
    }
    .map_err(message)
}

#[tauri::command]
fn open_log_folder(app: AppHandle) -> CmdResult<()> {
    let dir = log_dir().map_err(message)?;
    app.opener().open_path(dir.to_string_lossy(), None::<&str>).map_err(message)
}

const PROJECT_URL: &str = "https://github.com/Leafyheader/InternetBlazer";

/// Opens one of the project's pages in the browser. Only these pages, so the window can't
/// open arbitrary addresses.
#[tauri::command]
fn open_project_page(app: AppHandle, page: &str) -> CmdResult<()> {
    let path = match page {
        "home" => "",
        "releases" => "/releases",
        "issues" => "/issues",
        "license" => "#license",
        _ => return Err(format!("Unknown page: {page}")),
    };
    app.opener().open_url(format!("{PROJECT_URL}{path}"), None::<&str>).map_err(message)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct UpdateInfo {
    version: String,
    notes: Option<String>,
}

/// Asks GitHub for a newer release.
#[tauri::command]
async fn check_update(app: AppHandle) -> CmdResult<Option<UpdateInfo>> {
    let update = app.updater().map_err(message)?.check().await.map_err(message)?;
    Ok(update.map(|u| UpdateInfo { version: u.version, notes: u.body }))
}

/// Downloads the newer release and installs it, which ends the app and starts the new one.
#[tauri::command]
async fn install_update(app: AppHandle, engine: State<'_, Engine>) -> CmdResult<()> {
    let update = app.updater().map_err(message)?.check().await.map_err(message)?;
    let update = update.ok_or("InternetBlazer is up to date.")?;
    let bytes = update.download(|_, _| {}, || {}).await.map_err(message)?;
    // The installer ends the process without the exit event, so downloads checkpoint now and
    // carry on in the new version.
    engine.inner().clone().shutdown().await;
    update.install(bytes).map_err(message)?;
    app.restart()
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

/// Shows the system's "Open with" dialog for a finished download. Windows only; the UI offers it
/// nowhere else.
#[tauri::command]
async fn open_with(engine: State<'_, Engine>, id: DownloadId) -> CmdResult<()> {
    let path = engine.get(id).map_err(message)?.path();
    if !path.is_file() {
        return Err(format!("the file is no longer at {}", path.display()));
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // rundll32 passes the rest of the line on as the path, so it must not be quoted.
        std::process::Command::new("rundll32.exe")
            .raw_arg(format!("shell32.dll,OpenAs_RunDLL {}", path.display()))
            .spawn()
            .map(drop)
            .map_err(message)
    }
    #[cfg(not(windows))]
    Err("“Open with” is only available on Windows".into())
}

/// Moves or renames a finished download's file, or a paused one's partial data.
#[tauri::command]
async fn move_download(
    engine: State<'_, Engine>,
    id: DownloadId,
    dir: PathBuf,
    file_name: String,
) -> CmdResult<()> {
    engine.move_to(id, dir, &file_name).await.map_err(message)
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

/// The system icon for the download's file type, or for the file itself once it has finished,
/// as PNG bytes.
#[tauri::command]
async fn file_icon(
    app: AppHandle,
    engine: State<'_, Engine>,
    id: DownloadId,
    size: u16,
) -> CmdResult<Response> {
    let record = engine.get(id).map_err(message)?;
    let png = icons::png(&app, &record, size.clamp(16, 256)).await.map_err(|e| format!("{e:#}"))?;
    Ok(Response::new(png))
}

fn data_dir() -> anyhow::Result<PathBuf> {
    Ok(dirs::data_local_dir().context("no local data directory")?.join("InternetBlazer"))
}

/// Shares its state with the `blazer` CLI. redb allows one process at a time, so the CLI cannot
/// run while the app is open.
fn db_path() -> anyhow::Result<PathBuf> {
    Ok(data_dir()?.join("state.redb"))
}

/// Holds the yt-dlp the app installs, shared with the CLI.
fn tools_dir() -> anyhow::Result<PathBuf> {
    Ok(data_dir()?.join("tools"))
}

fn log_dir() -> anyhow::Result<PathBuf> {
    Ok(data_dir()?.join("logs"))
}

/// Logs to the console and to a file a day, keeping a week of them. `RUST_LOG` overrides the
/// level.
fn init_logging() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    // Written directly rather than from a background thread: the event loop ends the process
    // without running destructors, which would lose the last lines of a buffered writer.
    let file = log_dir().and_then(|dir| {
        Ok(tracing_appender::rolling::Builder::new()
            .rotation(tracing_appender::rolling::Rotation::DAILY)
            .filename_prefix("internetblazer")
            .filename_suffix("log")
            .max_log_files(7)
            .build(dir)?)
    });
    let file = file.inspect_err(|e| eprintln!("logging to a file: {e:#}")).ok();
    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer())
        .with(file.map(|file| tracing_subscriber::fmt::layer().with_ansi(false).with_writer(file)))
        .init();
    // Panics would otherwise only reach the console, which release builds don't have.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        tracing::error!("{info}");
        default_hook(info);
    }));
}

/// Shows a desktop notification, unless the user turned them off or is looking at the app.
pub(crate) fn notify(app: &AppHandle, title: &str, body: String) {
    let focused = app.get_webview_window("main").is_some_and(|w| w.is_focused().unwrap_or(false));
    if focused || !app.state::<PrefsStore>().get().notify {
        return;
    }
    if let Err(e) = app.notification().builder().title(title).body(body).show() {
        tracing::warn!("showing a notification: {e}");
    }
}

fn notify_status(
    app: &AppHandle,
    engine: &Engine,
    id: DownloadId,
    status: Status,
    error: Option<&str>,
) {
    if !matches!(status, Status::Completed | Status::Failed) {
        return;
    }
    let Ok(record) = engine.get(id) else { return };
    match status {
        Status::Completed => notify(app, "Download complete", record.file_name),
        _ => {
            let error = error.unwrap_or("unknown error");
            notify(app, "Download failed", format!("{}: {error}", record.file_name));
        }
    }
}

/// Tells the user when a finished file turned out not to be the one its SHA-256 belongs to.
fn notify_check(app: &AppHandle, engine: &Engine, id: DownloadId, state: &CheckState) {
    let Ok(record) = engine.get(id) else { return };
    match state {
        CheckState::Mismatched { .. } => notify(
            app,
            "SHA-256 doesn't match",
            format!("{}: it's damaged, or not the file the hash belongs to", record.file_name),
        ),
        CheckState::Failed { error } => {
            notify(app, "Couldn't check the SHA-256", format!("{}: {error}", record.file_name));
        }
        CheckState::Waiting | CheckState::Checking | CheckState::Matched => {}
    }
}

fn forward_events(app: AppHandle, engine: &Engine) {
    let mut events = engine.subscribe();
    let engine = engine.clone();
    let mut activity = tray::Activity::default();
    tauri::async_runtime::spawn(async move {
        loop {
            match events.recv().await {
                Ok(event) => {
                    activity.observe(&app, &event);
                    app.state::<Finish>().observe(&app, &engine, &event);
                    if let Event::StatusChanged { id, status, error } = &event {
                        notify_status(&app, &engine, *id, *status, error.as_deref());
                    }
                    if let Event::ChecksumChanged { id, checksum: Some(checksum) } = &event {
                        notify_check(&app, &engine, *id, &checksum.state);
                    }
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

/// Hides the window instead of closing it when the app should stay in the tray. Tells the user
/// the first time, so the app doesn't seem to have quit while downloads carry on.
fn close_to_tray(window: &tauri::Window) -> bool {
    let prefs = window.state::<PrefsStore>();
    let current = prefs.get();
    if !current.close_to_tray {
        return false;
    }
    let _ = window.hide();
    if !current.tray_notice_shown {
        let shown = window
            .notification()
            .builder()
            .title("InternetBlazer is still running")
            .body("Downloads carry on in the background. To quit, right-click the tray icon and choose Quit.")
            .show();
        if let Err(e) = shown {
            tracing::warn!("showing the tray notice: {e}");
        }
        if let Err(e) = prefs.update(|p| p.tray_notice_shown = true) {
            tracing::warn!("saving that the tray notice was shown: {e:#}");
        }
    }
    true
}

fn main() {
    init_logging();

    // Already running: show that window instead. The second copy could not open the store anyway.
    if blazer_host::send(&blazer_host::Request::Show).is_ok_and(|r| r.ok) {
        return;
    }

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_autostart::Builder::new().arg(HIDDEN_ARG).build())
        .setup(|app| {
            let db = db_path()?;
            app.manage(PrefsStore::open(data_dir()?.join("app.json")));
            app.manage(Finish::default());
            let config = Config { tools_dir: Some(tools_dir()?), ..Config::default() };
            let engine =
                Engine::open(&db, config).with_context(|| format!("opening {}", db.display()))?;
            forward_events(app.handle().clone(), &engine);
            tauri::async_runtime::spawn(engine.clone().run_scheduler());
            // Picks up where the app left off when it quit or crashed.
            let resume = engine.clone();
            tauri::async_runtime::spawn(async move {
                if let Err(e) = resume.resume_interrupted() {
                    tracing::error!("resuming interrupted downloads: {e}");
                }
                // Earlier versions installed the yt-dlp that takes seconds to start.
                if resume.ytdlp_is_slow()
                    && let Err(e) = resume.install_ytdlp().await
                {
                    tracing::warn!("replacing yt-dlp with its quicker build: {e}");
                }
            });
            app.manage(engine);
            app.manage(Bridge::default());
            bridge::serve(app.handle().clone());
            bridge::register_host();
            tray::create(app.handle())?;
            // The window starts hidden, so that starting at login doesn't flash it.
            if !std::env::args().any(|arg| arg == HIDDEN_ARG) {
                bridge::show_window(app.handle());
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event
                && window.label() == "main"
                && close_to_tray(window)
            {
                api.prevent_close();
            }
        })
        .invoke_handler(tauri::generate_handler![
            list_downloads,
            get_download,
            default_dir,
            file_categories,
            system_download_dir,
            inspect_link,
            inspect_media,
            media_tools,
            install_ytdlp,
            install_deno,
            free_space,
            add_download,
            import_links,
            start_download,
            pause_download,
            restart_download,
            set_download_url,
            remove_download,
            set_download_queue,
            set_download_speed_limit,
            set_download_sha256,
            check_download_sha256,
            list_queues,
            add_queue,
            update_queue,
            remove_queue,
            start_queue,
            stop_queue,
            finish_actions,
            set_finish_action,
            cancel_finish,
            finish_now,
            get_settings,
            set_settings,
            set_speed_limit,
            get_prefs,
            set_prefs,
            get_launch_at_login,
            set_launch_at_login,
            open_log_folder,
            open_project_page,
            check_update,
            install_update,
            take_browser_downloads,
            open_file,
            open_with,
            move_download,
            show_in_folder,
            file_icon,
        ])
        .build(tauri::generate_context!())
        .expect("failed to start InternetBlazer")
        .run(|app, event| match event {
            // Lets running downloads checkpoint their progress before the process ends.
            RunEvent::Exit => {
                if let Some(engine) = app.try_state::<Engine>() {
                    let engine = engine.inner().clone();
                    tauri::async_runtime::block_on(engine.shutdown());
                }
            }
            // Clicking the dock icon of a hidden app.
            #[cfg(target_os = "macos")]
            RunEvent::Reopen { .. } => bridge::show_window(app),
            _ => {}
        });
}
