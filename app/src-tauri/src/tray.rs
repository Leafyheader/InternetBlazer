//! The tray icon, which keeps the app reachable while its window is hidden, and shows what is
//! downloading in its tooltip.

use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use blazer_core::{DownloadId, Engine, Event, Status};
use tauri::{
    AppHandle, Manager,
    menu::{Menu, MenuItem, PredefinedMenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
};

use crate::bridge::show_window;

const ID: &str = "main";
const NAME: &str = "InternetBlazer";
/// Keeps the tooltip from being rewritten for every progress report.
const TOOLTIP_INTERVAL: Duration = Duration::from_secs(1);

pub fn create(app: &AppHandle) -> tauri::Result<()> {
    let open = MenuItem::with_id(app, "open", "Open InternetBlazer", true, None::<&str>)?;
    let pause = MenuItem::with_id(app, "pause-all", "Pause all downloads", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let separator = PredefinedMenuItem::separator(app)?;
    let menu = Menu::with_items(app, &[&open, &pause, &separator, &quit])?;
    let mut tray = TrayIconBuilder::with_id(ID)
        .tooltip(NAME)
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "open" => show_window(app),
            "pause-all" => pause_all(app),
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                show_window(tray.app_handle());
            }
        });
    if let Some(icon) = app.default_window_icon() {
        tray = tray.icon(icon.clone());
    }
    tray.build(app)?;
    Ok(())
}

fn pause_all(app: &AppHandle) {
    let engine = app.state::<Engine>().inner().clone();
    tauri::async_runtime::spawn(async move {
        let records = match engine.list() {
            Ok(records) => records,
            Err(e) => return tracing::error!("pausing all downloads: {e}"),
        };
        for record in records {
            if matches!(record.status, Status::Downloading | Status::Queued | Status::Retrying) {
                engine.pause(record.id);
            }
        }
    });
}

/// Follows engine events to keep the tooltip's count and speed current.
#[derive(Default)]
pub struct Activity {
    /// Speed of each running download, in bytes per second.
    speeds: HashMap<DownloadId, u64>,
    shown: Option<Shown>,
}

struct Shown {
    at: Instant,
    count: usize,
    text: String,
}

impl Activity {
    pub fn observe(&mut self, app: &AppHandle, event: &Event) {
        match event {
            Event::Progress(p) => {
                self.speeds.insert(p.id, p.speed);
            }
            Event::StatusChanged { id, status, .. } if *status != Status::Downloading => {
                self.speeds.remove(id);
            }
            Event::Removed { id } => {
                self.speeds.remove(id);
            }
            _ => return,
        }
        let count = self.speeds.len();
        let text = match count {
            0 => NAME.to_string(),
            n => format!(
                "{NAME}
{n} downloading · {}/s",
                bytes(self.speeds.values().sum())
            ),
        };
        // A change in what runs shows at once; a change in speed waits a little.
        let due = self.shown.as_ref().is_none_or(|shown| {
            shown.count != count || (shown.text != text && shown.at.elapsed() >= TOOLTIP_INTERVAL)
        });
        if due && let Some(tray) = app.tray_by_id(ID) {
            let _ = tray.set_tooltip(Some(&text));
            self.shown = Some(Shown { at: Instant::now(), count, text });
        }
    }
}

fn bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    match unit {
        0 => format!("{n} B"),
        _ => format!("{value:.1} {}", UNITS[unit]),
    }
}
