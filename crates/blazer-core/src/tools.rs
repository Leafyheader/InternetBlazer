//! Finding the programs media downloads use, yt-dlp and ffmpeg, and installing yt-dlp.
//!
//! Each is looked for in the engine's tools folder first, then on `PATH`. Only yt-dlp is
//! installed by the engine: it needs updating whenever sites change, while ffmpeg is large and
//! usually comes from the system's package manager.

use std::{
    io,
    path::{Path, PathBuf},
    process::Stdio,
};

use sha2::{Digest, Sha256};
use tokio::process::Command;

use crate::{
    child::Tree,
    error::{Error, Result},
    probe,
};

const YTDLP: &str = if cfg!(windows) { "yt-dlp.exe" } else { "yt-dlp" };
const FFMPEG: &str = if cfg!(windows) { "ffmpeg.exe" } else { "ffmpeg" };

/// The build for this system among yt-dlp's release files. These run without Python.
const ASSET: &str = if cfg!(all(windows, target_arch = "aarch64")) {
    "yt-dlp_arm64.exe"
} else if cfg!(windows) {
    "yt-dlp.exe"
} else if cfg!(target_os = "macos") {
    "yt-dlp_macos"
} else if cfg!(target_arch = "aarch64") {
    "yt-dlp_linux_aarch64"
} else {
    "yt-dlp_linux"
};
const RELEASE: &str = "https://github.com/yt-dlp/yt-dlp/releases/latest/download/";
/// Lists the SHA-256 of every file in the release.
const CHECKSUMS: &str = "SHA2-256SUMS";

/// yt-dlp, and whether it is the copy in `dir` that [`install_ytdlp`] manages.
pub(crate) fn ytdlp(dir: Option<&Path>) -> Option<(PathBuf, bool)> {
    match in_dir(dir, YTDLP) {
        Some(path) => Some((path, true)),
        None => on_path(YTDLP).map(|path| (path, false)),
    }
}

pub(crate) fn ffmpeg(dir: Option<&Path>) -> Option<PathBuf> {
    in_dir(dir, FFMPEG).or_else(|| on_path(FFMPEG))
}

fn in_dir(dir: Option<&Path>, name: &str) -> Option<PathBuf> {
    dir.map(|dir| dir.join(name)).filter(|path| path.is_file())
}

fn on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    // Apps opened from the macOS Finder get a short `PATH` without Homebrew's folders.
    let extra: &[&str] =
        if cfg!(target_os = "macos") { &["/opt/homebrew/bin", "/usr/local/bin"] } else { &[] };
    std::env::split_paths(&path)
        .chain(extra.iter().map(PathBuf::from))
        .chain(saved_path())
        .map(|dir| dir.join(name))
        .find(|path| path.is_file())
}

/// The `PATH` as Windows saved it, for programs started from now on. A program installed after
/// the app started, as with `winget install ffmpeg`, is only on this one.
#[cfg(windows)]
fn saved_path() -> Vec<PathBuf> {
    use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};

    let user = registry_string(HKEY_CURRENT_USER, "Environment", "Path");
    let system = registry_string(
        HKEY_LOCAL_MACHINE,
        r"SYSTEM\CurrentControlSet\Control\Session Manager\Environment",
        "Path",
    );
    [system, user]
        .into_iter()
        .flatten()
        .flat_map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .collect()
}

#[cfg(not(windows))]
fn saved_path() -> Vec<PathBuf> {
    Vec::new()
}

/// A text value from the registry, with `%VARIABLES%` filled in.
#[cfg(windows)]
fn registry_string(
    root: windows_sys::Win32::System::Registry::HKEY,
    key: &str,
    value: &str,
) -> Option<std::ffi::OsString> {
    use std::os::windows::ffi::{OsStrExt, OsStringExt};

    use windows_sys::Win32::System::Registry::{RRF_RT_REG_EXPAND_SZ, RRF_RT_REG_SZ, RegGetValueW};

    let wide = |s: &str| std::ffi::OsStr::new(s).encode_wide().chain(Some(0)).collect::<Vec<u16>>();
    let (key, value) = (wide(key), wide(value));
    let flags = RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ;
    let mut len = 0u32;
    // SAFETY: the names are NUL-terminated; the first call only asks for the size in bytes.
    let status = unsafe {
        RegGetValueW(
            root,
            key.as_ptr(),
            value.as_ptr(),
            flags,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut len,
        )
    };
    if status != 0 || len == 0 {
        return None;
    }
    let mut buf = vec![0u16; (len as usize).div_ceil(2)];
    // SAFETY: `buf` holds `len` bytes, which the call writes at most.
    let status = unsafe {
        RegGetValueW(
            root,
            key.as_ptr(),
            value.as_ptr(),
            flags,
            std::ptr::null_mut(),
            buf.as_mut_ptr().cast(),
            &mut len,
        )
    };
    if status != 0 {
        return None;
    }
    let chars = (len as usize / 2).min(buf.len());
    let text = &buf[..chars];
    let end = text.iter().position(|&c| c == 0).unwrap_or(text.len());
    Some(std::ffi::OsString::from_wide(&text[..end]))
}

/// The version the program reports, such as `2026.10.21` for yt-dlp.
pub(crate) async fn version(path: &Path) -> Result<String> {
    let mut cmd = Command::new(path);
    cmd.arg("--version").stdin(Stdio::null());
    let output = Tree::output(&mut cmd).await?;
    let text = String::from_utf8_lossy(&output.stdout);
    match text.lines().next().map(str::trim) {
        Some(version) if output.status.success() && !version.is_empty() => Ok(version.into()),
        _ => Err(Error::Media {
            message: format!("{} did not report its version", path.display()),
            retryable: false,
        }),
    }
}

/// Downloads the latest yt-dlp into `dir`, replacing the copy there. The file must match the
/// checksum published with the release, since it is a program that will run.
pub(crate) async fn install_ytdlp(client: &reqwest::Client, dir: &Path) -> Result<PathBuf> {
    let sums = fetch(client, &format!("{RELEASE}{CHECKSUMS}")).await?;
    let expected = expected_hash(&String::from_utf8_lossy(&sums), ASSET).ok_or_else(|| {
        Error::Invalid(format!("the yt-dlp release does not list a checksum for {ASSET}"))
    })?;
    let program = fetch(client, &format!("{RELEASE}{ASSET}")).await?;
    let actual = hex(&Sha256::digest(&program));
    if actual != expected {
        return Err(Error::Invalid(
            "the downloaded yt-dlp does not match its published checksum; try again later".into(),
        ));
    }
    let dir = dir.to_path_buf();
    tokio::task::spawn_blocking(move || replace(&dir, &program))
        .await
        .map_err(Error::internal)?
        .map_err(Error::from)
}

/// Puts the new program in place. The old one is moved aside rather than overwritten, which
/// works even while it runs on Windows; the next update deletes it.
fn replace(dir: &Path, program: &[u8]) -> io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let target = dir.join(YTDLP);
    let new = dir.join(format!("{YTDLP}.new"));
    let old = dir.join(format!("{YTDLP}.old"));
    std::fs::write(&new, program)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&new, std::fs::Permissions::from_mode(0o755))?;
    }
    if let Err(e) = std::fs::remove_file(&old)
        && e.kind() != io::ErrorKind::NotFound
    {
        tracing::debug!("could not delete the previous yt-dlp: {e}");
    }
    if target.exists() {
        std::fs::rename(&target, &old)?;
    }
    std::fs::rename(&new, &target)?;
    Ok(target)
}

async fn fetch(client: &reqwest::Client, url: &str) -> Result<Vec<u8>> {
    let resp = probe::request(client, url, &[]).send().await?;
    if !resp.status().is_success() {
        return Err(Error::from_response(resp.status(), resp.headers()));
    }
    Ok(resp.bytes().await?.to_vec())
}

/// Finds `name` in a checksum list of `<hash> <name>` lines, where the name may start with `*`.
fn expected_hash(sums: &str, name: &str) -> Option<String> {
    sums.lines().find_map(|line| {
        let (hash, file) = line.trim().split_once(char::is_whitespace)?;
        (file.trim().trim_start_matches('*') == name).then(|| hash.to_ascii_lowercase())
    })
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_checksum_of_a_file() {
        let sums = "b7af77f6  yt-dlp\n33121D19 *yt-dlp.exe\n1380a9f1  yt-dlp_arm64.exe\n";
        assert_eq!(expected_hash(sums, "yt-dlp.exe").as_deref(), Some("33121d19"));
        assert_eq!(expected_hash(sums, "yt-dlp").as_deref(), Some("b7af77f6"));
        assert_eq!(expected_hash(sums, "yt-dlp_macos"), None);
    }

    #[test]
    fn hashes_in_lowercase_hex() {
        assert_eq!(hex(&Sha256::digest(b"abc"))[..8], *"ba7816bf");
    }
}
