//! `blazer-host`: the native messaging host of the InternetBlazer browser extension.
//!
//! Browsers start it and exchange messages over stdin and stdout, each a JSON value preceded by
//! its length as a native-endian `u32`. It forwards downloads to the app, starting the app first
//! when it is not running.
//!
//! Run `blazer-host install` once to register it with the installed browsers, and
//! `blazer-host uninstall` to remove it again.

use std::{
    io::{self, Read, Write},
    path::PathBuf,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use blazer_host::{BrowserDownload, Request, Response};
use serde::Deserialize;
use serde_json::{Value, json};

/// Browsers refuse messages from the host above 1 MiB, and send at most 64 MiB to it.
const MAX_MESSAGE: u32 = 64 * 1024 * 1024;

/// How long a freshly started app may take to start listening.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum Message {
    /// Reports whether the host is installed and the app is running, without starting it.
    Ping,
    Download(BrowserDownload),
    Inspect(BrowserDownload),
    /// Like `Inspect`, for a video that started playing, so the qualities are ready when the user
    /// asks. Never starts the app.
    Prefetch(BrowserDownload),
}

fn main() {
    let command = std::env::args().nth(1);
    let result = match command.as_deref() {
        Some("install") => install(),
        Some("uninstall") => blazer_host::uninstall().map(|()| println!("Unregistered.")),
        Some("-h" | "--help") => {
            println!("usage: blazer-host [install | uninstall]");
            Ok(())
        }
        // Browsers pass the extension's origin or the manifest path.
        _ => serve(),
    };
    if let Err(e) = result {
        eprintln!("blazer-host: {e}");
        std::process::exit(1);
    }
}

fn install() -> io::Result<()> {
    let exe = std::env::current_exe()?;
    let browsers = blazer_host::install(&exe)?;
    if browsers.is_empty() {
        println!("No supported browser found.");
    } else {
        println!("Registered {} for {}.", exe.display(), browsers.join(", "));
    }
    Ok(())
}

fn serve() -> io::Result<()> {
    let mut stdin = io::stdin().lock();
    let mut stdout = io::stdout().lock();
    while let Some(message) = read_message(&mut stdin)? {
        let reply = match serde_json::from_slice::<Message>(&message) {
            Ok(Message::Ping) => json!({
                "ok": true,
                "version": env!("CARGO_PKG_VERSION"),
                "appRunning": blazer_host::is_running(),
            }),
            Ok(Message::Download(download)) => {
                allow_focus();
                json!(forward(Request::Download(download)))
            }
            Ok(Message::Inspect(page)) => json!(forward(Request::Inspect(page))),
            Ok(Message::Prefetch(page)) => match blazer_host::send(&Request::Inspect(page)) {
                Ok(response) => json!(response),
                Err(_) => json!({ "ok": false, "appRunning": false }),
            },
            Err(e) => json!(Response::error(format!("bad message: {e}"))),
        };
        write_message(&mut stdout, &reply)?;
    }
    Ok(())
}

fn read_message(input: &mut impl Read) -> io::Result<Option<Vec<u8>>> {
    let mut len = [0; 4];
    match input.read_exact(&mut len) {
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        result => result?,
    }
    let len = u32::from_ne_bytes(len);
    if len > MAX_MESSAGE {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "message too large"));
    }
    let mut message = vec![0; len as usize];
    input.read_exact(&mut message)?;
    Ok(Some(message))
}

fn write_message(output: &mut impl Write, value: &Value) -> io::Result<()> {
    let message = serde_json::to_vec(value)?;
    output.write_all(&(message.len() as u32).to_ne_bytes())?;
    output.write_all(&message)?;
    output.flush()
}

/// Lets the app bring its window to the front. Windows allows that only to processes the user
/// is interacting with, such as the browser and the host it started, and those they pass it on to.
fn allow_focus() {
    #[cfg(windows)]
    // SAFETY: takes no pointers.
    unsafe {
        use windows_sys::Win32::UI::WindowsAndMessaging::{ASFW_ANY, AllowSetForegroundWindow};
        AllowSetForegroundWindow(ASFW_ANY);
    }
}

/// Sends the request to the app, starting the app if it is not running.
fn forward(request: Request) -> Response {
    match blazer_host::send(&request) {
        Ok(response) => return response,
        Err(e)
            if !matches!(e.kind(), io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused) =>
        {
            return Response::error(format!("could not reach InternetBlazer: {e}"));
        }
        Err(_) => {}
    }

    if let Err(e) = launch_app() {
        return Response::error(format!("InternetBlazer is not running and could not start: {e}"));
    }
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    loop {
        thread::sleep(Duration::from_millis(250));
        match blazer_host::send(&request) {
            Ok(response) => return response,
            Err(e) if Instant::now() >= deadline => {
                return Response::error(format!("InternetBlazer did not start in time: {e}"));
            }
            Err(_) => {}
        }
    }
}

/// The app is installed next to the host.
fn app_path() -> io::Result<PathBuf> {
    let name = if cfg!(windows) { "internetblazer.exe" } else { "internetblazer" };
    Ok(std::env::current_exe()?.with_file_name(name))
}

fn launch_app() -> io::Result<()> {
    let mut command = Command::new(app_path()?);
    // The app must not inherit the pipes to the browser, or the browser would wait for it.
    command.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        use windows_sys::Win32::System::Threading::{
            CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP,
        };

        // Browsers run the host in a job that ends all its processes when the host exits. Leave
        // the job where the browser allows it.
        command.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_BREAKAWAY_FROM_JOB);
        if command.spawn().is_ok() {
            return Ok(());
        }
        command.creation_flags(CREATE_NEW_PROCESS_GROUP);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Out of the host's process group, so the browser ending the host does not end the app.
        command.process_group(0);
    }
    command.spawn().map(drop)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_round_trip() {
        let mut buf = Vec::new();
        write_message(&mut buf, &json!({"type": "ping"})).unwrap();
        let mut input = buf.as_slice();
        let message = read_message(&mut input).unwrap().unwrap();
        assert!(matches!(serde_json::from_slice(&message).unwrap(), Message::Ping));
        assert_eq!(read_message(&mut input).unwrap(), None);
    }
}
