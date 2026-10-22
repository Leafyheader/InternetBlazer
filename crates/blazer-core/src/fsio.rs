//! File preparation and positional writes.

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
