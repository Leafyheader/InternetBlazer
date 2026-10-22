//! File icons from the operating system, so downloads show the same icon as in the file manager.
//!
//! The platform APIs only describe files that exist. A finished download is asked about directly,
//! which also picks up icons embedded in the file itself, such as a program's. Anything else is
//! described by an empty stand-in file with the same extension.

use std::{fs, io, path::PathBuf};

use anyhow::Context;
use blazer_core::DownloadRecord;
use tauri::{AppHandle, Manager};

/// The icon as PNG bytes, `size` pixels square.
pub async fn png(app: &AppHandle, record: &DownloadRecord, size: u16) -> anyhow::Result<Vec<u8>> {
    let path = record.path();
    let path = if path.is_file() { path } else { stand_in(app, &record.file_name)? };

    // GTK only works on the main thread on Linux; elsewhere it costs little.
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.run_on_main_thread(move || {
        let _ = tx.send(file_icon_provider::get_file_icon(&path, size));
    })?;
    let icon = rx.await?.map_err(|e| anyhow::anyhow!("{e}"))?;
    encode(&icon)
}

/// An empty file named like `file_name` in the app cache, created on first use.
fn stand_in(app: &AppHandle, file_name: &str) -> anyhow::Result<PathBuf> {
    let dir = app.path().app_cache_dir()?.join("icons");
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let ext: String = file_name
        .rsplit_once('.')
        .map(|(_, ext)| ext)
        .filter(|ext| ext.len() <= 16 && ext.chars().all(|c| c.is_ascii_alphanumeric()))
        .unwrap_or_default()
        .to_ascii_lowercase();
    let path = if ext.is_empty() { dir.join("file") } else { dir.join(format!("file.{ext}")) };
    match fs::OpenOptions::new().write(true).create_new(true).open(&path) {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e).with_context(|| format!("creating {}", path.display())),
    }
    Ok(path)
}

fn encode(icon: &file_icon_provider::Icon) -> anyhow::Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(&mut out, icon.width, icon.height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.write_header()?.write_image_data(&icon.pixels)?;
    Ok(out)
}
