//! What to do when a queue finishes: shut down, sleep or hibernate the computer, or quit the app.
//!
//! The choice is for one run of a queue: it is used once and then cleared. It isn't saved, so a
//! restarted app never turns the computer off by itself. A 30-second countdown, which the user
//! can cancel, comes first; it runs here rather than in the window, whose timers slow down while
//! it's hidden.

use std::{collections::HashMap, time::Duration};

use blazer_core::{Engine, Event, QueueId, Status};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager};

use crate::{bridge::show_window, notify};

const COUNTDOWN: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FinishAction {
    Shutdown,
    Sleep,
    Hibernate,
    Quit,
}

impl FinishAction {
    /// For the system actions: "The computer …".
    fn describe(self) -> &'static str {
        match self {
            Self::Shutdown => "shuts down",
            Self::Sleep => "goes to sleep",
            Self::Hibernate => "hibernates",
            Self::Quit => "quits",
        }
    }
}

/// A countdown to an action, as the window shows it.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Countdown {
    queue: String,
    action: FinishAction,
    seconds: u64,
}

#[derive(Default)]
pub struct Finish(Mutex<State>);

#[derive(Default)]
struct State {
    /// The action of each queue that has one.
    actions: HashMap<QueueId, FinishAction>,
    /// The countdown running now, by its number, and what it ends in.
    pending: Option<(u64, FinishAction)>,
    started: u64,
}

impl Finish {
    pub fn actions(&self) -> HashMap<QueueId, FinishAction> {
        self.0.lock().actions.clone()
    }

    pub fn set(&self, app: &AppHandle, queue: QueueId, action: Option<FinishAction>) {
        let actions = {
            let mut state = self.0.lock();
            match action {
                Some(action) => state.actions.insert(queue, action),
                None => state.actions.remove(&queue),
            };
            state.actions.clone()
        };
        let _ = app.emit("finish-actions", actions);
    }

    /// Stops the countdown, if one runs.
    pub fn cancel(&self, app: &AppHandle) {
        if self.0.lock().pending.take().is_some() {
            let _ = app.emit("finish-countdown", None::<Countdown>);
        }
    }

    /// Ends the countdown early by doing its action now.
    pub fn now(&self, app: &AppHandle) {
        let pending = self.0.lock().pending.take();
        if let Some((_, action)) = pending {
            let _ = app.emit("finish-countdown", None::<Countdown>);
            run(app.clone(), action);
        }
    }

    /// Starts the countdown once the last download of a queue with an action completes or fails.
    /// A queue the user stops doesn't count as finished.
    pub fn observe(&self, app: &AppHandle, engine: &Engine, event: &Event) {
        let Event::StatusChanged { id, status: Status::Completed | Status::Failed, .. } = event
        else {
            return;
        };
        if self.0.lock().actions.is_empty() {
            return;
        }
        let Ok(record) = engine.get(*id) else { return };
        let queue = record.queue;
        let Ok(records) = engine.list() else { return };
        let busy = records.iter().any(|r| {
            r.queue == queue
                && matches!(r.status, Status::Downloading | Status::Queued | Status::Retrying)
        });
        if busy {
            return;
        }
        let (number, action, actions) = {
            let mut state = self.0.lock();
            let Some(action) = state.actions.remove(&queue) else { return };
            state.started += 1;
            let number = state.started;
            state.pending = Some((number, action));
            (number, action, state.actions.clone())
        };
        let name = engine.queue(queue).map(|q| q.name).unwrap_or_else(|_| "The queue".into());
        tracing::info!(%queue, ?action, "queue finished; counting down");
        let _ = app.emit("finish-actions", actions);
        let _ = app.emit(
            "finish-countdown",
            Some(Countdown { queue: name.clone(), action, seconds: COUNTDOWN.as_secs() }),
        );
        show_window(app);
        let who = match action {
            FinishAction::Quit => "InternetBlazer",
            _ => "The computer",
        };
        notify(
            app,
            &format!("{name} is done"),
            format!("{who} {} in {} seconds unless you cancel.", action.describe(), COUNTDOWN.as_secs()),
        );
        let app = app.clone();
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(COUNTDOWN).await;
            let finish = app.state::<Finish>();
            let due = {
                let mut state = finish.0.lock();
                match state.pending {
                    Some((n, _)) if n == number => state.pending.take(),
                    _ => None,
                }
            };
            if due.is_some() {
                let _ = app.emit("finish-countdown", None::<Countdown>);
                run(app.clone(), action);
            }
        });
    }
}

fn run(app: AppHandle, action: FinishAction) {
    tracing::info!(?action, "doing the queue's finish action");
    tauri::async_runtime::spawn(async move {
        if action == FinishAction::Quit {
            return app.exit(0);
        }
        if action == FinishAction::Shutdown {
            // Other queues may still be downloading; their progress is saved first, as when the
            // app quits. The app then has no engine, so it quits whether or not the system does.
            let engine = app.state::<Engine>().inner().clone();
            engine.shutdown().await;
        }
        // Sleep returns only once the computer wakes up.
        let result = tauri::async_runtime::spawn_blocking(move || power(action))
            .await
            .unwrap_or_else(|e| Err(std::io::Error::other(e.to_string())));
        if let Err(e) = &result {
            tracing::error!(?action, "finish action failed: {e}");
            notify(&app, "The queue's finish action failed", e.to_string());
        }
        if action == FinishAction::Shutdown {
            app.exit(0);
        }
    });
}

/// Asks the system to shut down, sleep or hibernate.
fn power(action: FinishAction) -> std::io::Result<()> {
    let (program, args): (&str, &[&str]) = match action {
        #[cfg(windows)]
        FinishAction::Shutdown => ("shutdown", &["/s", "/t", "0"]),
        #[cfg(windows)]
        FinishAction::Hibernate => ("shutdown", &["/h"]),
        // rundll32's SetSuspendState hibernates instead when hibernation is on; .NET's doesn't.
        #[cfg(windows)]
        FinishAction::Sleep => (
            "powershell",
            &[
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "Add-Type -AssemblyName System.Windows.Forms; \
                 [void][System.Windows.Forms.Application]::SetSuspendState('Suspend', $false, $false)",
            ],
        ),
        #[cfg(target_os = "macos")]
        FinishAction::Shutdown => ("osascript", &["-e", "tell application \"System Events\" to shut down"]),
        #[cfg(target_os = "macos")]
        FinishAction::Sleep => ("pmset", &["sleepnow"]),
        #[cfg(target_os = "macos")]
        FinishAction::Hibernate => {
            return Err(std::io::Error::other("macOS has no hibernate command"));
        }
        #[cfg(all(unix, not(target_os = "macos")))]
        FinishAction::Shutdown => ("systemctl", &["poweroff"]),
        #[cfg(all(unix, not(target_os = "macos")))]
        FinishAction::Sleep => ("systemctl", &["suspend"]),
        #[cfg(all(unix, not(target_os = "macos")))]
        FinishAction::Hibernate => ("systemctl", &["hibernate"]),
        FinishAction::Quit => unreachable!("quitting doesn't go through the system"),
    };
    let mut cmd = std::process::Command::new(program);
    cmd.args(args);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let output = cmd.output()?;
    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let detail = [stderr.trim(), stdout.trim()].into_iter().find(|s| !s.is_empty());
        Err(std::io::Error::other(detail.unwrap_or("the system refused").to_string()))
    }
}
