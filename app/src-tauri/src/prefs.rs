//! Settings of the desktop app itself, as opposed to the engine's, which the CLI shares. Kept as
//! JSON next to the engine's store.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::categories::Category;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Prefs {
    /// Where new downloads go. `None` uses the system's Downloads folder.
    pub download_dir: Option<PathBuf>,
    /// New downloads go to the folder used last instead of `download_dir`.
    pub remember_last_dir: bool,
    pub last_dir: Option<PathBuf>,
    /// New downloads go to a folder for their file type, such as Downloads\Video. This wins over
    /// `remember_last_dir`.
    pub sort_by_type: bool,
    /// Folders the user picked for some file types. The others use a subfolder of the download
    /// folder named after the type.
    pub category_dirs: BTreeMap<Category, PathBuf>,
    /// Browser downloads wait in the add dialog. Otherwise they start right away.
    pub confirm_browser_downloads: bool,
    /// Closing the window hides it, and the app keeps running in the tray.
    pub close_to_tray: bool,
    /// Desktop notifications when downloads finish or fail while the window is in the background.
    pub notify: bool,
    /// Looks for a newer version each time the app starts.
    pub check_updates: bool,
    /// Whether the user has been told that closing the window hides it.
    pub tray_notice_shown: bool,
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            download_dir: None,
            remember_last_dir: true,
            last_dir: None,
            sort_by_type: true,
            category_dirs: BTreeMap::new(),
            confirm_browser_downloads: true,
            close_to_tray: true,
            notify: true,
            check_updates: true,
            tray_notice_shown: false,
        }
    }
}

impl Prefs {
    /// The folder a new download of this type goes to unless the user picks another.
    pub fn target_dir(&self, category: Category) -> Option<PathBuf> {
        if self.sort_by_type {
            return self.category_dir(category);
        }
        let last = self.last_dir.as_ref().filter(|_| self.remember_last_dir);
        last.cloned().or_else(|| self.base_dir())
    }

    /// Where files of this type go when downloads are sorted by type.
    pub fn category_dir(&self, category: Category) -> Option<PathBuf> {
        match self.category_dirs.get(&category) {
            Some(dir) => Some(dir.clone()),
            None => self.base_dir().map(|dir| dir.join(category.folder())),
        }
    }

    fn base_dir(&self) -> Option<PathBuf> {
        self.download_dir.clone().or_else(dirs::download_dir)
    }
}

pub struct PrefsStore {
    path: PathBuf,
    prefs: Mutex<Prefs>,
}

impl PrefsStore {
    /// Starts from the defaults when the file is missing or unreadable, so a damaged file never
    /// keeps the app from starting.
    pub fn open(path: PathBuf) -> Self {
        let prefs = match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
                tracing::warn!("ignoring unreadable {}: {e}", path.display());
                Prefs::default()
            }),
            Err(_) => Prefs::default(),
        };
        Self { path, prefs: Mutex::new(prefs) }
    }

    pub fn get(&self) -> Prefs {
        self.prefs.lock().clone()
    }

    pub fn update(&self, change: impl FnOnce(&mut Prefs)) -> anyhow::Result<()> {
        let mut prefs = self.prefs.lock();
        let mut updated = prefs.clone();
        change(&mut updated);
        save(&self.path, &updated)?;
        *prefs = updated;
        Ok(())
    }
}

/// Written beside the file and renamed over it, so a crash leaves the old or the new settings.
fn save(path: &Path, prefs: &Prefs) -> anyhow::Result<()> {
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_vec_pretty(prefs)?)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_the_last_folder_only_when_asked() {
        let mut prefs = Prefs {
            download_dir: Some("D:/Downloads".into()),
            last_dir: Some("E:/Films".into()),
            sort_by_type: false,
            ..Prefs::default()
        };
        assert_eq!(prefs.target_dir(Category::Video), Some("E:/Films".into()));
        prefs.remember_last_dir = false;
        assert_eq!(prefs.target_dir(Category::Video), Some("D:/Downloads".into()));
    }

    #[test]
    fn sorts_into_type_folders_unless_one_was_picked() {
        let mut prefs = Prefs {
            download_dir: Some("D:/Downloads".into()),
            last_dir: Some("E:/Films".into()),
            ..Prefs::default()
        };
        assert_eq!(
            prefs.target_dir(Category::Music),
            Some(Path::new("D:/Downloads").join("Music"))
        );
        prefs.category_dirs.insert(Category::Video, "E:/Films".into());
        assert_eq!(prefs.target_dir(Category::Video), Some("E:/Films".into()));
    }

    #[test]
    fn a_damaged_file_falls_back_to_the_defaults() {
        let dir = std::env::temp_dir().join(format!("blazer-prefs-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("app.json");
        fs::write(&path, b"{ not json").unwrap();

        let store = PrefsStore::open(path.clone());
        assert!(store.get().close_to_tray);
        store.update(|p| p.notify = false).unwrap();
        assert!(!PrefsStore::open(path).get().notify, "saved over the damaged file");
        fs::remove_dir_all(dir).unwrap();
    }
}
