//! File preparation, positional writes, and the disk space and renaming checks around them.

use std::{
    fs::{self, File, OpenOptions},
    io,
    path::Path,
};

/// Opens the `.part` file. A fresh download truncates it and reserves the full size up front.
pub(crate) fn open_part(path: &Path, total: Option<u64>, resume: bool) -> io::Result<File> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let file =
        OpenOptions::new().read(true).write(true).create(true).truncate(!resume).open(path)?;
    if !resume {
        mark_sparse(&file);
        if let Some(total) = total {
            file.set_len(total)?;
        }
    }
    Ok(file)
}

/// Without this, NTFS zero-fills everything before the first write far into the file, which
/// stalls segments near the end of a large download.
#[cfg(windows)]
fn mark_sparse(file: &File) {
    use std::os::windows::io::AsRawHandle;

    use windows_sys::Win32::System::{IO::DeviceIoControl, Ioctl::FSCTL_SET_SPARSE};

    let mut returned = 0u32;
    // SAFETY: the handle is owned by `file` and stays open for the call; no buffers are passed.
    let ok = unsafe {
        DeviceIoControl(
            file.as_raw_handle(),
            FSCTL_SET_SPARSE,
            std::ptr::null(),
            0,
            std::ptr::null_mut(),
            0,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        // FAT32 and exFAT have no sparse files; the download still works, just slower to start.
        tracing::debug!("could not mark file as sparse: {}", io::Error::last_os_error());
    }
}

/// Unix file systems create sparse files from `set_len` already.
#[cfg(not(windows))]
fn mark_sparse(_file: &File) {}

#[cfg(unix)]
pub(crate) fn write_at(file: &File, buf: &[u8], offset: u64) -> io::Result<()> {
    use std::os::unix::fs::FileExt;
    file.write_all_at(buf, offset)
}

#[cfg(windows)]
pub(crate) fn write_at(file: &File, mut buf: &[u8], mut offset: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        match file.seek_write(buf, offset)? {
            0 => return Err(io::ErrorKind::WriteZero.into()),
            n => {
                buf = &buf[n..];
                offset += n as u64;
            }
        }
    }
    Ok(())
}

/// Bytes free for this user on the disk that holds `path`, which need not exist yet.
pub(crate) fn available_space(path: &Path) -> io::Result<u64> {
    let existing = path.ancestors().find(|p| p.exists()).unwrap_or(path);
    free_bytes(existing)
}

/// Bytes the file takes on disk, which for a sparse file is less than its length. 0 when it
/// does not exist.
pub(crate) fn allocated_size(path: &Path) -> u64 {
    allocated(path).unwrap_or(0)
}

/// Renames `from` to `to` unless `to` exists, which fails with
/// [`io::ErrorKind::AlreadyExists`]. Plain `fs::rename` would replace it.
pub(crate) fn rename_new(from: &Path, to: &Path) -> io::Result<()> {
    match rename_no_replace(from, to) {
        Err(e) if e.kind() == io::ErrorKind::Unsupported => {
            // Not atomic, but the file system offers nothing better.
            if to.exists() {
                return Err(io::ErrorKind::AlreadyExists.into());
            }
            fs::rename(from, to)
        }
        result => result,
    }
}

/// Moves `from` to `to`, also to another disk, never over an existing file.
pub(crate) fn move_new(from: &Path, to: &Path) -> io::Result<()> {
    match rename_new(from, to) {
        Err(e) if e.kind() == io::ErrorKind::CrossesDevices => {
            let mut source = File::open(from)?;
            let mut target = OpenOptions::new().write(true).create_new(true).open(to)?;
            let copied = io::copy(&mut source, &mut target).and_then(|_| target.sync_all());
            drop(target);
            if let Err(e) = copied {
                let _ = fs::remove_file(to);
                return Err(e);
            }
            drop(source);
            fs::remove_file(from)
        }
        result => result,
    }
}

#[cfg(windows)]
fn wide(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}

#[cfg(windows)]
fn free_bytes(path: &Path) -> io::Result<u64> {
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

    let mut free = 0u64;
    let path = wide(path);
    // SAFETY: `path` is NUL-terminated and outlives the call; the null pointers are optional
    // outputs.
    let ok = unsafe {
        GetDiskFreeSpaceExW(path.as_ptr(), &mut free, std::ptr::null_mut(), std::ptr::null_mut())
    };
    if ok == 0 { Err(io::Error::last_os_error()) } else { Ok(free) }
}

#[cfg(windows)]
fn allocated(path: &Path) -> io::Result<u64> {
    use windows_sys::Win32::Storage::FileSystem::{GetCompressedFileSizeW, INVALID_FILE_SIZE};

    let mut high = 0u32;
    let path = wide(path);
    // SAFETY: `path` is NUL-terminated and outlives the call.
    let low = unsafe { GetCompressedFileSizeW(path.as_ptr(), &mut high) };
    if low == INVALID_FILE_SIZE {
        // The low half can legitimately be all ones; only the error code tells them apart.
        let e = io::Error::last_os_error();
        if e.raw_os_error() != Some(0) {
            return Err(e);
        }
    }
    Ok((u64::from(high) << 32) | u64::from(low))
}

#[cfg(windows)]
fn rename_no_replace(from: &Path, to: &Path) -> io::Result<()> {
    use windows_sys::Win32::Storage::FileSystem::{MOVEFILE_WRITE_THROUGH, MoveFileExW};

    let (from, to) = (wide(from), wide(to));
    // SAFETY: both paths are NUL-terminated and outlive the call. Without
    // MOVEFILE_REPLACE_EXISTING it fails when `to` exists.
    let ok = unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), MOVEFILE_WRITE_THROUGH) };
    if ok == 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}

#[cfg(unix)]
fn c_path(path: &Path) -> io::Result<std::ffi::CString> {
    use std::os::unix::ffi::OsStrExt;
    std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"))
}

#[cfg(unix)]
fn free_bytes(path: &Path) -> io::Result<u64> {
    let path = c_path(path)?;
    // SAFETY: all zeroes is a valid statvfs, which the call overwrites.
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `path` is NUL-terminated and `stat` is a valid output buffer.
    if unsafe { libc::statvfs(path.as_ptr(), &mut stat) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // The field types differ between platforms.
    #[allow(clippy::unnecessary_cast)]
    Ok(stat.f_bavail as u64 * stat.f_frsize as u64)
}

#[cfg(unix)]
fn allocated(path: &Path) -> io::Result<u64> {
    use std::os::unix::fs::MetadataExt;
    // Counted in 512-byte units, whatever the file system's block size.
    Ok(fs::metadata(path)?.blocks() * 512)
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn rename_no_replace(from: &Path, to: &Path) -> io::Result<()> {
    let (from, to) = (c_path(from)?, c_path(to)?);
    // SAFETY: both paths are NUL-terminated and outlive the call.
    let rc = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            from.as_ptr(),
            libc::AT_FDCWD,
            to.as_ptr(),
            libc::RENAME_NOREPLACE as _,
        )
    };
    unix_rename_result(rc)
}

#[cfg(target_vendor = "apple")]
fn rename_no_replace(from: &Path, to: &Path) -> io::Result<()> {
    let (from, to) = (c_path(from)?, c_path(to)?);
    // SAFETY: both paths are NUL-terminated and outlive the call.
    let rc = unsafe { libc::renamex_np(from.as_ptr(), to.as_ptr(), libc::RENAME_EXCL) };
    unix_rename_result(rc)
}

#[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
fn unix_rename_result(rc: libc::c_int) -> io::Result<()> {
    if rc == 0 {
        return Ok(());
    }
    let e = io::Error::last_os_error();
    match e.raw_os_error() {
        // The kernel or the file system cannot refuse to replace.
        Some(libc::EINVAL | libc::ENOSYS | libc::ENOTSUP) => Err(io::ErrorKind::Unsupported.into()),
        _ => Err(e),
    }
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "android", target_vendor = "apple"))))]
fn rename_no_replace(_from: &Path, _to: &Path) -> io::Result<()> {
    Err(io::ErrorKind::Unsupported.into())
}

/// Sizes for messages, written the way the app shows them.
pub(crate) fn format_bytes(n: u64) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rename_new_never_replaces() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b, c) = (dir.path().join("a"), dir.path().join("b"), dir.path().join("c"));
        fs::write(&a, "a").unwrap();
        fs::write(&b, "b").unwrap();
        assert_eq!(rename_new(&a, &b).unwrap_err().kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read_to_string(&b).unwrap(), "b");
        rename_new(&a, &c).unwrap();
        assert!(!a.exists());
        assert_eq!(fs::read_to_string(&c).unwrap(), "a");
    }

    #[test]
    fn measures_space() {
        let dir = tempfile::tempdir().unwrap();
        assert!(available_space(&dir.path().join("not/yet/created")).unwrap() > 0);
        assert_eq!(allocated_size(&dir.path().join("missing")), 0);

        let sparse = dir.path().join("sparse");
        let len = 64 * 1024 * 1024;
        let file = open_part(&sparse, Some(len), false).unwrap();
        write_at(&file, &[1; 4096], 0).unwrap();
        file.sync_all().unwrap();
        drop(file);
        let allocated = allocated_size(&sparse);
        assert!(allocated >= 4096, "{allocated}");
        // Sparse, so the unwritten part takes no space.
        assert!(allocated < len / 2, "{allocated}");
    }

    #[test]
    fn formats_bytes() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(1536), "1.5 KB");
        assert_eq!(format_bytes(3 * 1024 * 1024 * 1024), "3.0 GB");
    }
}
