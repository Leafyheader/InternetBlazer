//! Finding the programs media downloads use, yt-dlp, deno and ffmpeg, and installing yt-dlp and
//! deno.
//!
//! Each is looked for in the engine's tools folder first, then on `PATH`. yt-dlp is installed by
//! the engine since it needs updating whenever sites change, and deno on request: yt-dlp runs
//! YouTube's JavaScript with it, without which some formats are missing. ffmpeg is large and
//! usually comes from the system's package manager.
//!
//! A program that comes as a folder is unpacked into a folder named after its checksum, and
//! `<name>.current` names the one in use. A new version goes into a new folder, since Windows
//! won't let a running program's folder be replaced, and old folders are deleted once unused.

use std::{
    io::{self, Cursor},
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
const DENO: &str = if cfg!(windows) { "deno.exe" } else { "deno" };
const FFMPEG: &str = if cfg!(windows) { "ffmpeg.exe" } else { "ffmpeg" };

/// The folder build of yt-dlp for this system, if there is one. It starts in under a second,
/// where the single-file build first unpacks itself for a few seconds, every time it runs.
const YTDLP_FOLDER_ASSET: Option<&str> = if cfg!(all(windows, target_arch = "aarch64")) {
    Some("yt-dlp_win_arm64.zip")
} else if cfg!(all(windows, target_arch = "x86_64")) {
    Some("yt-dlp_win.zip")
} else {
    None
};

/// The single-file build for this system among yt-dlp's release files. These run without
/// Python.
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

/// Deno's build for this system. Each comes with `<asset>.sha256sum`.
const DENO_ASSET: Option<&str> = if cfg!(all(windows, target_arch = "x86_64")) {
    Some("deno-x86_64-pc-windows-msvc.zip")
} else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
    Some("deno-aarch64-apple-darwin.zip")
} else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
    Some("deno-x86_64-apple-darwin.zip")
} else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
    Some("deno-x86_64-unknown-linux-gnu.zip")
} else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
    Some("deno-aarch64-unknown-linux-gnu.zip")
} else {
    None
};
const DENO_RELEASE: &str = "https://github.com/denoland/deno/releases/latest/download/";

/// yt-dlp, and whether it is the copy in `dir` that [`install_ytdlp`] manages.
pub(crate) fn ytdlp(dir: Option<&Path>) -> Option<(PathBuf, bool)> {
    match current(dir, "yt-dlp", YTDLP).or_else(|| in_dir(dir, YTDLP)) {
        Some(path) => Some((path, true)),
        None => on_path(YTDLP).map(|path| (path, false)),
    }
}

/// Whether the managed yt-dlp is the slow-starting single file, where a folder build exists.
pub(crate) fn ytdlp_is_single_file(dir: Option<&Path>) -> bool {
    YTDLP_FOLDER_ASSET.is_some()
        && current(dir, "yt-dlp", YTDLP).is_none()
        && in_dir(dir, YTDLP).is_some()
}

/// deno, and whether it is the copy in `dir` that [`install_deno`] manages.
pub(crate) fn deno(dir: Option<&Path>) -> Option<(PathBuf, bool)> {
    match current(dir, "deno", DENO) {
        Some(path) => Some((path, true)),
        None => on_path(DENO).map(|path| (path, false)),
    }
}

/// Starts a yt-dlp command, with deno for YouTube's JavaScript when there is one.
pub(crate) fn ytdlp_command(ytdlp: &Path, dir: Option<&Path>) -> Command {
    let mut cmd = Command::new(ytdlp);
    cmd.env("PYTHONUTF8", "1");
    if let Some((deno, _)) = deno(dir) {
        let mut runtime = std::ffi::OsString::from("deno:");
        runtime.push(deno);
        cmd.arg("--js-runtimes").arg(runtime);
    }
    cmd
}

/// The program in the folder that `<tool>.current` in `dir` names.
fn current(dir: Option<&Path>, tool: &str, program: &str) -> Option<PathBuf> {
    let dir = dir?;
    let folder = std::fs::read_to_string(dir.join(format!("{tool}.current"))).ok()?;
    let folder = folder.trim();
    // Only a folder right inside `dir`.
    if folder.is_empty() || folder.contains(['/', '\\']) || folder.starts_with('.') {
        return None;
    }
    Some(dir.join(folder).join(program)).filter(|path| path.is_file())
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
    if let Some(asset) = YTDLP_FOLDER_ASSET {
        let expected = expected_hash(&String::from_utf8_lossy(&sums), asset).ok_or_else(|| {
            Error::Invalid(format!("the yt-dlp release does not list a checksum for {asset}"))
        })?;
        let zip = fetch(client, &format!("{RELEASE}{asset}")).await?;
        let path = unpack_checked(dir, "yt-dlp", YTDLP, zip, &expected).await?;
        // The single file it replaces, if this is the first folder build.
        for old in [YTDLP.to_string(), format!("{YTDLP}.old")] {
            let _ = std::fs::remove_file(dir.join(old));
        }
        return Ok(path);
    }
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

/// Downloads the latest deno into `dir`, checked against the checksum published with it.
pub(crate) async fn install_deno(client: &reqwest::Client, dir: &Path) -> Result<PathBuf> {
    let asset = DENO_ASSET
        .ok_or_else(|| Error::Invalid("deno has no build for this system to install".into()))?;
    let sums = fetch(client, &format!("{DENO_RELEASE}{asset}.sha256sum")).await?;
    let expected = any_hash(&String::from_utf8_lossy(&sums)).ok_or_else(|| {
        Error::Invalid(format!("could not read the checksum published for {asset}"))
    })?;
    let zip = fetch(client, &format!("{DENO_RELEASE}{asset}")).await?;
    unpack_checked(dir, "deno", DENO, zip, &expected).await
}

/// Checks `zip` against `expected` and unpacks it into a new folder for `tool` in `dir`, which
/// becomes the current one. Returns the path of `program` in it.
async fn unpack_checked(
    dir: &Path,
    tool: &str,
    program: &str,
    zip: Vec<u8>,
    expected: &str,
) -> Result<PathBuf> {
    let actual = hex(&Sha256::digest(&zip));
    if actual != expected {
        return Err(Error::Invalid(format!(
            "the downloaded {tool} does not match its published checksum; try again later"
        )));
    }
    let (dir, tool, program) = (dir.to_path_buf(), tool.to_string(), program.to_string());
    tokio::task::spawn_blocking(move || unpack(&dir, &tool, &program, &zip, &actual[..12]))
        .await
        .map_err(Error::internal)?
}

fn unpack(dir: &Path, tool: &str, program: &str, zip: &[u8], id: &str) -> Result<PathBuf> {
    let name = format!("{tool}-{id}");
    let folder = dir.join(&name);
    let path = folder.join(program);
    // The same version, unpacked before.
    if !path.is_file() {
        let partial = dir.join(format!(".{name}.partial"));
        let _ = std::fs::remove_dir_all(&partial);
        std::fs::create_dir_all(&partial)?;
        let mut archive = zip::ZipArchive::new(Cursor::new(zip))
            .map_err(|e| Error::Invalid(format!("the {tool} download is not a valid zip: {e}")))?;
        for i in 0..archive.len() {
            let mut entry = archive.by_index(i).map_err(|e| Error::Invalid(e.to_string()))?;
            // Never a path outside the folder.
            let Some(relative) = entry.enclosed_name() else { continue };
            let target = partial.join(relative);
            if entry.is_dir() {
                std::fs::create_dir_all(&target)?;
                continue;
            }
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            io::copy(&mut entry, &mut std::fs::File::create(&target)?)?;
            #[cfg(unix)]
            if let Some(mode) = entry.unix_mode() {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&target, std::fs::Permissions::from_mode(mode & 0o777))?;
            }
        }
        if !partial.join(program).is_file() {
            return Err(Error::Invalid(format!("the {tool} download has no {program} in it")));
        }
        let _ = std::fs::remove_dir_all(&folder);
        std::fs::rename(&partial, &folder)?;
    }
    let pointer = dir.join(format!("{tool}.current"));
    let new_pointer = dir.join(format!("{tool}.current.new"));
    std::fs::write(&new_pointer, &name)?;
    std::fs::rename(&new_pointer, &pointer)?;
    remove_old_folders(dir, tool, &name);
    Ok(path)
}

/// Deletes the earlier versions' folders. One still running stays until the next update.
fn remove_old_folders(dir: &Path, tool: &str, keep: &str) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let prefix = format!("{tool}-");
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let old = name.strip_prefix(&prefix).is_some_and(|id| id.len() == 12)
            || name.starts_with(&format!(".{prefix}"));
        if old
            && name != keep
            && entry.path().is_dir()
            && let Err(e) = std::fs::remove_dir_all(entry.path())
        {
            tracing::debug!("could not delete the old {name}: {e}");
        }
    }
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

/// The first SHA-256 in a checksum file, whatever its layout: deno's Windows ones are
/// PowerShell's `Get-FileHash` output.
fn any_hash(text: &str) -> Option<String> {
    text.split(|c: char| !c.is_ascii_hexdigit())
        .find(|word| word.len() == 64)
        .map(str::to_ascii_lowercase)
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
    fn reads_checksums_in_any_layout() {
        let hash = "CBA84B26E0DF5AE7F74FC66B86A95B866872A86292EBD84FA1E959927EDD93BF";
        let powershell =
            format!("Algorithm : SHA256\nHash      : {hash}\nPath      : C:\\a\\deno.zip\n");
        assert_eq!(any_hash(&powershell), Some(hash.to_ascii_lowercase()));
        assert_eq!(any_hash(&format!("{hash}  deno.zip")), Some(hash.to_ascii_lowercase()));
        assert_eq!(any_hash("no hash here"), None);
    }

    #[test]
    fn unpacks_into_a_new_current_folder() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default();
        zip.start_file("tool.exe", options).unwrap();
        zip.write_all(b"v1").unwrap();
        zip.start_file("_internal/lib.dll", options).unwrap();
        zip.start_file("../escape.txt", options).unwrap();
        let bytes = zip.finish().unwrap().into_inner();

        let path = unpack(dir.path(), "tool", "tool.exe", &bytes, "aaaaaaaaaaaa").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"v1");
        assert_eq!(current(Some(dir.path()), "tool", "tool.exe"), Some(path));
        assert!(dir.path().join("tool-aaaaaaaaaaaa/_internal/lib.dll").is_file());
        assert!(!dir.path().parent().unwrap().join("escape.txt").exists());

        // A newer version replaces the pointer and the old folder.
        let path = unpack(dir.path(), "tool", "tool.exe", &bytes, "bbbbbbbbbbbb").unwrap();
        assert_eq!(current(Some(dir.path()), "tool", "tool.exe"), Some(path));
        assert!(!dir.path().join("tool-aaaaaaaaaaaa").exists());
    }

    /// Installs yt-dlp and deno from their releases into a temporary folder and runs them. Run
    /// with `cargo test -- --ignored`.
    #[tokio::test]
    #[ignore = "needs the network and downloads about 60 MB"]
    async fn installs_ytdlp_and_deno() {
        let dir = tempfile::tempdir().unwrap();
        let client = reqwest::Client::new();
        let ytdlp = install_ytdlp(&client, dir.path()).await.unwrap();
        assert_eq!(self::ytdlp(Some(dir.path())).unwrap(), (ytdlp.clone(), true));
        assert!(!ytdlp_is_single_file(Some(dir.path())));
        let deno = install_deno(&client, dir.path()).await.unwrap();
        println!("{} / {}", version(&ytdlp).await.unwrap(), version(&deno).await.unwrap());
    }

    #[test]
    fn hashes_in_lowercase_hex() {
        assert_eq!(hex(&Sha256::digest(b"abc"))[..8], *"ba7816bf");
    }
}
