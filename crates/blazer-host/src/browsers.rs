//! The browsers on this computer: which of them InternetBlazer captures downloads from, which the
//! extension has talked to the app from, and putting the extension into them.
//!
//! The extension goes in the way browsers allow for programs installed beside them. Chromium
//! browsers read a registry key naming an extension and where it updates from; on their next start
//! they add it switched off and ask the user to turn it on. Only an extension from the browser's
//! own store is allowed this way, so it can't work until the extension is published there. For the
//! others the app opens the store page in that browser, where the user adds it.
//!
//! The settings live in files beside the host's manifests, so the host can read them while the app
//! isn't running.

use std::{
    fs, io,
    path::{Path, PathBuf},
};

use serde::Serialize;

/// The extension's ID in the Chrome Web Store, once it is published there.
pub const CHROME_WEB_STORE_ID: Option<&str> = None;
/// The extension's ID in Edge Add-ons, once it is published there.
pub const EDGE_ADD_ONS_ID: Option<&str> = None;
/// The extension's page on addons.mozilla.org, once it is published there.
pub const FIREFOX_ADD_ON_PAGE: Option<&str> = None;

/// Where the browser's extension comes from.
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Store {
    ChromeWebStore,
    EdgeAddOns,
    FirefoxAddOns,
}

impl Store {
    pub fn name(self) -> &'static str {
        match self {
            Store::ChromeWebStore => "Chrome Web Store",
            Store::EdgeAddOns => "Edge Add-ons",
            Store::FirefoxAddOns => "Firefox Add-ons",
        }
    }

    /// The extension's ID there, for the stores browsers install from by ID.
    fn id(self) -> Option<&'static str> {
        match self {
            Store::ChromeWebStore => CHROME_WEB_STORE_ID,
            Store::EdgeAddOns => EDGE_ADD_ONS_ID,
            Store::FirefoxAddOns => None,
        }
    }

    /// Where a browser told about the extension by the registry fetches it and its updates.
    fn update_url(self) -> Option<&'static str> {
        match self {
            Store::ChromeWebStore => Some("https://clients2.google.com/service/update2/crx"),
            Store::EdgeAddOns => Some("https://edge.microsoft.com/extensionwebstorebase/v1/crx"),
            Store::FirefoxAddOns => None,
        }
    }

    /// The extension's page in the store.
    fn page(self) -> Option<String> {
        match self {
            Store::ChromeWebStore => CHROME_WEB_STORE_ID
                .map(|id| format!("https://chromewebstore.google.com/detail/{id}")),
            Store::EdgeAddOns => EDGE_ADD_ONS_ID
                .map(|id| format!("https://microsoftedge.microsoft.com/addons/detail/{id}")),
            Store::FirefoxAddOns => FIREFOX_ADD_ON_PAGE.map(str::to_string),
        }
    }

    fn published(self) -> bool {
        self.page().is_some()
    }
}

/// How the extension gets into a browser.
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Debug, Clone, Copy)]
enum Install {
    /// A key under `HKCU\Software\<root>\Extensions` names the extension; the browser asks the
    /// user to turn it on at its next start.
    Registry { root: &'static str },
    /// The store page opens in the browser, run as this program.
    Page { program: &'static str },
}

#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Debug, Clone, Copy)]
enum Base {
    /// The local app data folder.
    Local,
    /// The roaming app data folder.
    Roaming,
}

/// A browser the extension works in.
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Debug)]
pub struct Browser {
    pub id: &'static str,
    pub name: &'static str,
    /// The file names of its program, in lowercase.
    programs: &'static [&'static str],
    /// Its profile folder, there once it has been used.
    profile: (Base, &'static str),
    store: Store,
    install: Install,
}

/// The browsers on Windows. Brave and Vivaldi read Chrome's key, as the Chromium code they share
/// does, so the extension installed for Chrome is offered to them as well.
#[cfg(windows)]
const BROWSERS: &[Browser] = &[
    Browser {
        id: "chrome",
        name: "Google Chrome",
        programs: &["chrome.exe"],
        profile: (Base::Local, r"Google\Chrome\User Data"),
        store: Store::ChromeWebStore,
        install: Install::Registry { root: r"Google\Chrome" },
    },
    Browser {
        id: "edge",
        name: "Microsoft Edge",
        programs: &["msedge.exe"],
        profile: (Base::Local, r"Microsoft\Edge\User Data"),
        store: Store::EdgeAddOns,
        install: Install::Registry { root: r"Microsoft\Edge" },
    },
    Browser {
        id: "brave",
        name: "Brave",
        programs: &["brave.exe"],
        profile: (Base::Local, r"BraveSoftware\Brave-Browser\User Data"),
        store: Store::ChromeWebStore,
        install: Install::Registry { root: r"Google\Chrome" },
    },
    Browser {
        id: "vivaldi",
        name: "Vivaldi",
        programs: &["vivaldi.exe"],
        profile: (Base::Local, r"Vivaldi\User Data"),
        store: Store::ChromeWebStore,
        install: Install::Registry { root: r"Google\Chrome" },
    },
    Browser {
        id: "opera",
        name: "Opera",
        programs: &["opera.exe"],
        profile: (Base::Roaming, r"Opera Software\Opera Stable"),
        store: Store::ChromeWebStore,
        install: Install::Page { program: "opera.exe" },
    },
    Browser {
        id: "firefox",
        name: "Mozilla Firefox",
        programs: &["firefox.exe"],
        profile: (Base::Roaming, r"Mozilla\Firefox\Profiles"),
        store: Store::FirefoxAddOns,
        install: Install::Page { program: "firefox.exe" },
    },
];

#[cfg(not(windows))]
const BROWSERS: &[Browser] = &[];

impl Browser {
    fn is_installed(&self) -> bool {
        let base = match self.profile.0 {
            Base::Local => dirs::data_local_dir(),
            Base::Roaming => dirs::data_dir(),
        };
        base.is_some_and(|dir| dir.join(self.profile.1).is_dir())
    }

    /// Whether the app can put the extension into this browser yet.
    fn can_install(&self) -> bool {
        self.store.published()
    }
}

/// The browser with this ID.
pub fn browser(id: &str) -> Option<&'static Browser> {
    BROWSERS.iter().find(|b| b.id == id)
}

/// The browser that runs a program with this file name.
pub fn by_program(file_name: &str) -> Option<&'static Browser> {
    let file_name = file_name.to_ascii_lowercase();
    BROWSERS.iter().find(|b| b.programs.contains(&file_name.as_str()))
}

/// A browser as the app's settings show it.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowserState {
    pub id: &'static str,
    pub name: &'static str,
    /// Downloads in it go to the app.
    pub capture: bool,
    /// The extension has talked to the app from it.
    pub connected: bool,
    /// The app can put the extension into it, which needs the extension in its store.
    pub can_install: bool,
    pub store: &'static str,
    /// Installing writes a key the browser reads at its next start, rather than opening a page.
    pub on_restart: bool,
}

/// The browsers installed on this computer.
pub fn installed() -> Vec<BrowserState> {
    let off = no_capture();
    BROWSERS
        .iter()
        .filter(|b| b.is_installed())
        .map(|b| BrowserState {
            id: b.id,
            name: b.name,
            capture: !off.iter().any(|id| id == b.id),
            connected: seen_path(b.id).is_ok_and(|p| p.exists()),
            can_install: b.can_install(),
            store: b.store.name(),
            on_restart: matches!(b.install, Install::Registry { .. }),
        })
        .collect()
}

fn data_dir() -> io::Result<PathBuf> {
    let dir = dirs::data_local_dir()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no local data directory"))?;
    Ok(dir.join("InternetBlazer").join("native-messaging"))
}

fn no_capture_path() -> io::Result<PathBuf> {
    Ok(data_dir()?.join("no-capture.json"))
}

/// The IDs of the browsers the user turned capturing off for.
fn no_capture() -> Vec<String> {
    no_capture_path()
        .and_then(fs::read)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// Whether downloads in the browser with this ID go to the app. On unless the user turned it off.
pub fn captures(id: &str) -> bool {
    !no_capture().iter().any(|off| off == id)
}

/// Turns capturing downloads in a browser on or off.
pub fn set_capture(id: &str, on: bool) -> io::Result<()> {
    let mut off = no_capture();
    off.retain(|other| other != id);
    if !on {
        off.push(id.to_string());
    }
    let path = no_capture_path()?;
    fs::create_dir_all(path.parent().expect("the path is in a folder"))?;
    write_replacing(&path, &serde_json::to_vec(&off)?)
}

/// Written beside the file and renamed over it, so a reader sees the old or the new list.
fn write_replacing(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path)
}

fn seen_path(id: &str) -> io::Result<PathBuf> {
    Ok(data_dir()?.join(format!("seen-{id}")))
}

/// Notes that the extension talked to the app from this browser.
pub fn mark_seen(id: &str) {
    if let Ok(path) = seen_path(id)
        && !path.exists()
    {
        let _ = fs::create_dir_all(path.parent().expect("the path is in a folder"));
        let _ = fs::write(path, b"");
    }
}

/// What installing the extension did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Installed {
    /// The browser asks the user to turn the extension on at its next start.
    OnRestart,
    /// The store page opened in the browser, where the user adds it.
    PageOpened,
}

/// Puts the extension into the browser with this ID, the way the browser allows.
pub fn install_extension(id: &str) -> io::Result<Installed> {
    let browser = browser(id)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("no browser {id}")))?;
    let unpublished = || {
        io::Error::new(
            io::ErrorKind::Unsupported,
            format!("the extension isn't in the {} yet", browser.store.name()),
        )
    };
    match browser.install {
        Install::Registry { root } => {
            let (Some(id), Some(url)) = (browser.store.id(), browser.store.update_url()) else {
                return Err(unpublished());
            };
            platform::register_extension(root, id, url)?;
            Ok(Installed::OnRestart)
        }
        Install::Page { program } => {
            let page = browser.store.page().ok_or_else(unpublished)?;
            platform::open_in(program, &page)?;
            Ok(Installed::PageOpened)
        }
    }
}

/// Removes what [`install_extension`] wrote, which has the browsers remove the extension too.
pub(crate) fn uninstall_extensions() -> io::Result<()> {
    for browser in BROWSERS {
        if let (Install::Registry { root }, Some(id)) = (browser.install, browser.store.id()) {
            platform::unregister_extension(root, id)?;
        }
    }
    Ok(())
}

/// The browser that started this process: the host's parent, or on Windows, where Chrome starts
/// the host through `cmd.exe`, a process or two further up.
pub fn calling_browser() -> Option<&'static Browser> {
    platform::ancestors().iter().take(4).find_map(|name| by_program(name))
}

#[cfg(windows)]
mod platform {
    use std::{collections::HashMap, ffi::OsStr, os::windows::ffi::OsStrExt, ptr};

    use windows_sys::Win32::{
        Foundation::{CloseHandle, ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, INVALID_HANDLE_VALUE},
        System::{
            Diagnostics::ToolHelp::{
                CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
                TH32CS_SNAPPROCESS,
            },
            Registry::{
                HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ,
                RegCloseKey, RegCreateKeyExW, RegDeleteKeyW, RegSetValueExW,
            },
            Threading::GetCurrentProcessId,
        },
    };

    use super::*;

    fn wide(s: impl AsRef<OsStr>) -> Vec<u16> {
        s.as_ref().encode_wide().chain([0]).collect()
    }

    fn check(code: u32) -> io::Result<()> {
        if code == ERROR_SUCCESS { Ok(()) } else { Err(io::Error::from_raw_os_error(code as i32)) }
    }

    fn key_path(root: &str, id: &str) -> String {
        format!(r"Software\{root}\Extensions\{id}")
    }

    pub fn register_extension(root: &str, id: &str, update_url: &str) -> io::Result<()> {
        let value = wide(update_url);
        let mut key: HKEY = ptr::null_mut();
        // SAFETY: all pointers refer to live, NUL-terminated buffers or valid out-params.
        unsafe {
            check(RegCreateKeyExW(
                HKEY_CURRENT_USER,
                wide(key_path(root, id)).as_ptr(),
                0,
                ptr::null(),
                REG_OPTION_NON_VOLATILE,
                KEY_SET_VALUE,
                ptr::null(),
                &mut key,
                ptr::null_mut(),
            ))?;
            let result = check(RegSetValueExW(
                key,
                wide("update_url").as_ptr(),
                0,
                REG_SZ,
                value.as_ptr().cast(),
                (value.len() * 2) as u32,
            ));
            RegCloseKey(key);
            result
        }
    }

    pub fn unregister_extension(root: &str, id: &str) -> io::Result<()> {
        // SAFETY: the key path is a live, NUL-terminated buffer.
        let code = unsafe { RegDeleteKeyW(HKEY_CURRENT_USER, wide(key_path(root, id)).as_ptr()) };
        if code == ERROR_FILE_NOT_FOUND { Ok(()) } else { check(code) }
    }

    /// Opens `url` in the browser run as `program`, which Windows finds through its App Paths.
    pub fn open_in(program: &str, url: &str) -> io::Result<()> {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        // `start` looks programs up the way the Run box does; the empty title keeps it from taking
        // the program for one.
        std::process::Command::new("cmd")
            .args(["/d", "/c", "start", "", program, url])
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .map(drop)
    }

    /// The file names of the processes above this one, nearest first.
    pub fn ancestors() -> Vec<String> {
        let mut parents = HashMap::new();
        // SAFETY: the snapshot handle is checked and closed; the entry is sized as the API needs.
        unsafe {
            let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
            if snapshot == INVALID_HANDLE_VALUE {
                return Vec::new();
            }
            let mut entry: PROCESSENTRY32W = std::mem::zeroed();
            entry.dwSize = size_of::<PROCESSENTRY32W>() as u32;
            let mut more = Process32FirstW(snapshot, &mut entry) != 0;
            while more {
                let len = entry.szExeFile.iter().position(|&c| c == 0).unwrap_or(0);
                let name = String::from_utf16_lossy(&entry.szExeFile[..len]);
                parents.insert(entry.th32ProcessID, (entry.th32ParentProcessID, name));
                more = Process32NextW(snapshot, &mut entry) != 0;
            }
            CloseHandle(snapshot);
        }
        // SAFETY: takes no pointers.
        let mut pid = unsafe { GetCurrentProcessId() };
        let mut names = Vec::new();
        // Process IDs are reused, so a loop is possible; the depth bounds it.
        while names.len() < 8 {
            let Some((parent, _)) = parents.get(&pid) else { break };
            let Some((_, name)) = parents.get(parent) else { break };
            names.push(name.clone());
            pid = *parent;
        }
        names
    }
}

#[cfg(not(windows))]
mod platform {
    use super::*;

    pub fn register_extension(_root: &str, _id: &str, _update_url: &str) -> io::Result<()> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "only on Windows"))
    }

    pub fn unregister_extension(_root: &str, _id: &str) -> io::Result<()> {
        Ok(())
    }

    pub fn open_in(_program: &str, _url: &str) -> io::Result<()> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "only on Windows"))
    }

    pub fn ancestors() -> Vec<String> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn finds_browsers_by_their_program() {
        assert_eq!(by_program("MSEDGE.EXE").map(|b| b.id), Some("edge"));
        assert_eq!(by_program("chrome.exe").map(|b| b.id), Some("chrome"));
        assert!(by_program("cmd.exe").is_none());
    }

    #[test]
    fn nothing_installs_before_the_extension_is_published() {
        for browser in BROWSERS {
            assert_eq!(browser.can_install(), browser.store.published());
        }
        if CHROME_WEB_STORE_ID.is_none() {
            assert!(install_extension("chrome").is_err());
        }
    }

    /// Prints what the app's settings would list on this computer.
    #[test]
    #[ignore = "depends on this computer's browsers"]
    fn lists_this_computers_browsers() {
        println!("{:#?}", installed());
        println!("{:?}", platform::ancestors());
    }

    #[test]
    fn sees_its_own_ancestors() {
        // The test runner's parent, at least, is running.
        if cfg!(windows) {
            assert!(!platform::ancestors().is_empty());
        }
    }
}
