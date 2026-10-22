//! Registers the host with browsers, for the current user only.
//!
//! Browsers find a native messaging host through a JSON manifest. On Windows the manifest can
//! live anywhere and a registry key points to it; elsewhere it goes in a folder of each browser's
//! profile directory.

use std::{
    fs, io,
    path::{Path, PathBuf},
};

use serde_json::json;

pub const HOST_NAME: &str = "org.internetblazer.host";

/// Extensions allowed to talk to the host. The first is the unpacked extension in `extension/`,
/// whose ID is fixed by the `key` in its manifest.
pub const CHROME_EXTENSION_IDS: &[&str] = &["bdghohjegkeehdacpafalkbieochhlee"];

/// The `browser_specific_settings.gecko.id` of the extension.
pub const FIREFOX_EXTENSION_ID: &str = "internetblazer@internetblazer.org";

const DESCRIPTION: &str = "InternetBlazer download manager";

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Chromium,
    Firefox,
}

fn manifest(kind: Kind, host_exe: &Path) -> String {
    let (key, allowed): (_, Vec<String>) = match kind {
        Kind::Chromium => (
            "allowed_origins",
            CHROME_EXTENSION_IDS.iter().map(|id| format!("chrome-extension://{id}/")).collect(),
        ),
        Kind::Firefox => ("allowed_extensions", vec![FIREFOX_EXTENSION_ID.to_string()]),
    };
    let mut manifest = json!({
        "name": HOST_NAME,
        "description": DESCRIPTION,
        "path": host_exe,
        "type": "stdio",
    });
    manifest[key] = json!(allowed);
    serde_json::to_string_pretty(&manifest).expect("a JSON value serializes")
}

/// Registers `host_exe` with every supported browser. Returns the browsers it registered with.
pub fn install(host_exe: &Path) -> io::Result<Vec<&'static str>> {
    platform::install(host_exe)
}

/// Removes the registrations made by [`install`].
pub fn uninstall() -> io::Result<()> {
    platform::uninstall()
}

#[cfg(windows)]
mod platform {
    use std::{ffi::OsStr, os::windows::ffi::OsStrExt, ptr};

    use windows_sys::Win32::{
        Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS},
        System::Registry::{
            HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ, RegCloseKey,
            RegCreateKeyExW, RegDeleteKeyW, RegSetValueExW,
        },
    };

    use super::*;

    /// Registry roots under `HKCU\Software` that each browser reads. Opera and Vivaldi read
    /// Chrome's key.
    const BROWSERS: &[(&str, &str, Kind)] = &[
        ("Chrome", r"Google\Chrome", Kind::Chromium),
        ("Edge", r"Microsoft\Edge", Kind::Chromium),
        ("Brave", r"BraveSoftware\Brave-Browser", Kind::Chromium),
        ("Chromium", "Chromium", Kind::Chromium),
        ("Firefox", "Mozilla", Kind::Firefox),
    ];

    fn wide(s: impl AsRef<OsStr>) -> Vec<u16> {
        s.as_ref().encode_wide().chain([0]).collect()
    }

    fn key_path(root: &str) -> String {
        format!(r"Software\{root}\NativeMessagingHosts\{HOST_NAME}")
    }

    fn check(code: u32) -> io::Result<()> {
        if code == ERROR_SUCCESS { Ok(()) } else { Err(io::Error::from_raw_os_error(code as i32)) }
    }

    fn set_default_value(key_path: &str, value: &Path) -> io::Result<()> {
        let value = wide(value);
        let mut key: HKEY = ptr::null_mut();
        // SAFETY: all pointers refer to live, NUL-terminated buffers or valid out-params.
        unsafe {
            check(RegCreateKeyExW(
                HKEY_CURRENT_USER,
                wide(key_path).as_ptr(),
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
                ptr::null(),
                0,
                REG_SZ,
                value.as_ptr().cast(),
                (value.len() * 2) as u32,
            ));
            RegCloseKey(key);
            result
        }
    }

    fn manifest_dir() -> io::Result<PathBuf> {
        let dir = dirs::data_local_dir()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no local data directory"))?;
        Ok(dir.join("InternetBlazer").join("native-messaging"))
    }

    fn manifest_path(dir: &Path, kind: Kind) -> PathBuf {
        dir.join(if kind == Kind::Chromium { "chromium.json" } else { "firefox.json" })
    }

    pub fn install(host_exe: &Path) -> io::Result<Vec<&'static str>> {
        let dir = manifest_dir()?;
        fs::create_dir_all(&dir)?;
        for kind in [Kind::Chromium, Kind::Firefox] {
            fs::write(manifest_path(&dir, kind), manifest(kind, host_exe))?;
        }
        // Registering with a browser that is not installed is harmless, and covers installing it
        // later.
        for &(_, root, kind) in BROWSERS {
            set_default_value(&key_path(root), &manifest_path(&dir, kind))?;
        }
        Ok(BROWSERS.iter().map(|&(name, ..)| name).collect())
    }

    pub fn uninstall() -> io::Result<()> {
        for &(_, root, _) in BROWSERS {
            // SAFETY: the key path is a live, NUL-terminated buffer.
            let code = unsafe { RegDeleteKeyW(HKEY_CURRENT_USER, wide(key_path(root)).as_ptr()) };
            if code != ERROR_FILE_NOT_FOUND {
                check(code)?;
            }
        }
        match fs::remove_dir_all(manifest_dir()?) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }
}

#[cfg(unix)]
mod platform {
    use super::*;

    /// The folder of host manifests for each browser, relative to the home directory. Its
    /// parent is the browser's profile directory.
    #[cfg(target_os = "macos")]
    const BROWSERS: &[(&str, &str, Kind)] = &[
        (
            "Chrome",
            "Library/Application Support/Google/Chrome/NativeMessagingHosts",
            Kind::Chromium,
        ),
        ("Edge", "Library/Application Support/Microsoft Edge/NativeMessagingHosts", Kind::Chromium),
        (
            "Brave",
            "Library/Application Support/BraveSoftware/Brave-Browser/NativeMessagingHosts",
            Kind::Chromium,
        ),
        ("Chromium", "Library/Application Support/Chromium/NativeMessagingHosts", Kind::Chromium),
        ("Firefox", "Library/Application Support/Mozilla/NativeMessagingHosts", Kind::Firefox),
    ];

    #[cfg(not(target_os = "macos"))]
    const BROWSERS: &[(&str, &str, Kind)] = &[
        ("Chrome", ".config/google-chrome/NativeMessagingHosts", Kind::Chromium),
        ("Edge", ".config/microsoft-edge/NativeMessagingHosts", Kind::Chromium),
        ("Brave", ".config/BraveSoftware/Brave-Browser/NativeMessagingHosts", Kind::Chromium),
        ("Chromium", ".config/chromium/NativeMessagingHosts", Kind::Chromium),
        ("Firefox", ".mozilla/native-messaging-hosts", Kind::Firefox),
    ];

    fn manifest_paths() -> io::Result<Vec<(&'static str, PathBuf, Kind)>> {
        let home = dirs::home_dir()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no home directory"))?;
        Ok(BROWSERS
            .iter()
            .map(|&(name, folder, kind)| {
                (name, home.join(folder).join(format!("{HOST_NAME}.json")), kind)
            })
            .collect())
    }

    pub fn install(host_exe: &Path) -> io::Result<Vec<&'static str>> {
        let mut registered = Vec::new();
        for (name, path, kind) in manifest_paths()? {
            let folder = path.parent().expect("the manifest is in a folder");
            // Only browsers that are installed: their profile directory exists.
            if folder.parent().is_some_and(Path::is_dir) {
                fs::create_dir_all(folder)?;
                fs::write(&path, manifest(kind, host_exe))?;
                registered.push(name);
            }
        }
        Ok(registered)
    }

    pub fn uninstall() -> io::Result<()> {
        for (_, path, _) in manifest_paths()? {
            match fs::remove_file(path) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
                _ => {}
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifests_name_the_extensions() {
        let chromium: serde_json::Value =
            serde_json::from_str(&manifest(Kind::Chromium, Path::new("/opt/blazer-host"))).unwrap();
        assert_eq!(chromium["path"], "/opt/blazer-host");
        assert_eq!(
            chromium["allowed_origins"][0],
            format!("chrome-extension://{}/", CHROME_EXTENSION_IDS[0])
        );
        let firefox: serde_json::Value =
            serde_json::from_str(&manifest(Kind::Firefox, Path::new("/opt/blazer-host"))).unwrap();
        assert_eq!(firefox["allowed_extensions"][0], FIREFOX_EXTENSION_ID);
        assert!(firefox.get("allowed_origins").is_none());
    }
}
