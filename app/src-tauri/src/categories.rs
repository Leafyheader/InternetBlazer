//! File types that downloads are sorted by, each with a folder of its own.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Category {
    Video,
    Music,
    Compressed,
    Documents,
    Programs,
    Other,
}

impl Category {
    pub const ALL: [Category; 6] = [
        Category::Video,
        Category::Music,
        Category::Compressed,
        Category::Documents,
        Category::Programs,
        Category::Other,
    ];

    /// The subfolder of the download folder that files of this type go to.
    pub fn folder(self) -> &'static str {
        match self {
            Category::Video => "Video",
            Category::Music => "Music",
            Category::Compressed => "Compressed",
            Category::Documents => "Documents",
            Category::Programs => "Programs",
            Category::Other => "Other",
        }
    }

    /// Lower-case extensions, without the dot.
    pub fn extensions(self) -> &'static [&'static str] {
        match self {
            Category::Video => &[
                "mp4", "mkv", "webm", "avi", "mov", "wmv", "flv", "m4v", "mpg", "mpeg", "3gp", "ts",
            ],
            Category::Music => &["mp3", "flac", "wav", "ogg", "m4a", "aac", "opus", "wma", "aiff"],
            Category::Compressed => {
                &["zip", "rar", "7z", "tar", "gz", "tgz", "xz", "bz2", "zst", "cab", "lz", "lzma"]
            }
            Category::Documents => &[
                "pdf", "doc", "docx", "xls", "xlsx", "ppt", "pptx", "odt", "ods", "odp", "rtf",
                "txt", "csv", "epub", "mobi",
            ],
            Category::Programs => &[
                "exe", "msi", "msix", "appx", "dmg", "pkg", "deb", "rpm", "appimage", "apk", "iso",
            ],
            Category::Other => &[],
        }
    }

    /// The type of a file, going by its name. A video download counts as video whatever its name.
    pub fn of(file_name: &str, media: bool) -> Category {
        if media {
            return Category::Video;
        }
        let Some((_, ext)) = file_name.rsplit_once('.') else { return Category::Other };
        let ext = ext.to_ascii_lowercase();
        Category::ALL
            .into_iter()
            .find(|c| c.extensions().contains(&ext.as_str()))
            .unwrap_or(Category::Other)
    }
}

/// What the UI needs to sort downloads the same way.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CategoryInfo {
    pub id: Category,
    pub name: &'static str,
    pub extensions: &'static [&'static str],
}

pub fn all() -> Vec<CategoryInfo> {
    Category::ALL
        .into_iter()
        .map(|c| CategoryInfo { id: c, name: c.folder(), extensions: c.extensions() })
        .collect()
}

/// The last part of a link's path, which is often the file's name.
pub fn name_in_url(url: &str) -> Option<&str> {
    let path = url.split(['?', '#']).next()?;
    let path = path.split_once("://").map_or(path, |(_, rest)| rest);
    let (_, last) = path.split_once('/')?;
    last.rsplit('/').next().filter(|name| !name.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sorts_by_extension_ignoring_case() {
        assert_eq!(Category::of("Film.MKV", false), Category::Video);
        assert_eq!(Category::of("setup.exe", false), Category::Programs);
        assert_eq!(Category::of("archive.tar.gz", false), Category::Compressed);
        assert_eq!(Category::of("README", false), Category::Other);
        assert_eq!(Category::of("photo.png", false), Category::Other);
        assert_eq!(Category::of("index.html", true), Category::Video);
    }

    #[test]
    fn finds_the_name_in_a_link() {
        assert_eq!(name_in_url("https://a.com/files/x.zip?dl=1"), Some("x.zip"));
        assert_eq!(name_in_url("https://a.com/"), None);
        assert_eq!(name_in_url("https://a.com"), None);
    }
}
