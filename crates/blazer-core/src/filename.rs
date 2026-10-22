//! Choosing a safe, unique local file name.

use std::path::{Path, PathBuf};

use percent_encoding::percent_decode_str;

const MAX_NAME_BYTES: usize = 200;
const RESERVED_WINDOWS_NAMES: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// Extracts the file name from a `Content-Disposition` header, preferring the RFC 5987
/// `filename*` form.
pub(crate) fn from_content_disposition(value: &str) -> Option<String> {
    let mut plain = None;
    for param in split_params(value) {
        let Some((key, val)) = param.split_once('=') else { continue };
        let key = key.trim().to_ascii_lowercase();
        let val = val.trim();
        if key == "filename*" {
            // charset'language'percent-encoded-name
            if let Some(encoded) = val.splitn(3, '\'').nth(2) {
                let name = percent_decode_str(encoded.trim_matches('"')).decode_utf8_lossy();
                if !name.is_empty() {
                    return Some(name.into_owned());
                }
            }
        } else if key == "filename" {
            plain = Some(unquote(val));
        }
    }
    plain.filter(|name| !name.is_empty())
}

/// Splits on `;` outside quoted strings.
fn split_params(value: &str) -> Vec<&str> {
    let mut params = Vec::new();
    let (mut start, mut quoted, mut escaped) = (0, false, false);
    for (i, c) in value.char_indices() {
        match c {
            _ if escaped => escaped = false,
            '\\' if quoted => escaped = true,
            '"' => quoted = !quoted,
            ';' if !quoted => {
                params.push(&value[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    params.push(&value[start..]);
    params
}

fn unquote(value: &str) -> String {
    match value.strip_prefix('"').and_then(|v| v.strip_suffix('"')) {
        Some(inner) => inner.replace("\\\"", "\"").replace("\\\\", "\\"),
        None => value.to_string(),
    }
}

pub(crate) fn from_url(url: &url::Url) -> Option<String> {
    let last = url.path_segments()?.rev().find(|s| !s.is_empty())?;
    Some(percent_decode_str(last).decode_utf8_lossy().into_owned())
}

/// Makes a server-supplied name safe to use as a file name on every platform.
pub(crate) fn sanitize(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or_default();
    let replaced: String = base
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '|' | '?' | '*' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    let mut name = replaced.trim().trim_end_matches(['.', ' ']).to_string();
    if name.is_empty() {
        name = "download".to_string();
    }

    let stem = name.split('.').next().unwrap_or_default().to_ascii_uppercase();
    if RESERVED_WINDOWS_NAMES.contains(&stem.as_str()) {
        name.insert(0, '_');
    }

    if name.len() > MAX_NAME_BYTES {
        let (stem, ext) = split_ext(&name);
        let mut cut = MAX_NAME_BYTES.saturating_sub(ext.len());
        while !stem.is_char_boundary(cut) {
            cut -= 1;
        }
        name = format!("{}{ext}", &stem[..cut]);
    }
    name
}

/// Splits `name` into stem and extension, keeping the dot with the extension.
fn split_ext(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(i) if i > 0 => name.split_at(i),
        _ => (name, ""),
    }
}

/// Returns `name`, or `name (1)`, `name (2)`, … if `taken` reports the path as in use.
pub(crate) fn unique(dir: &Path, name: &str, taken: impl Fn(&Path) -> bool) -> String {
    if !taken(&dir.join(name)) {
        return name.to_string();
    }
    let (stem, ext) = split_ext(name);
    (1..)
        .map(|n| format!("{stem} ({n}){ext}"))
        .find(|candidate| !taken(&dir.join(candidate)))
        .expect("an unused name exists")
}

pub(crate) fn part_path(path: &Path) -> PathBuf {
    let mut part = path.as_os_str().to_owned();
    part.push(".part");
    PathBuf::from(part)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_disposition_forms() {
        let cd = from_content_disposition;
        assert_eq!(cd("attachment; filename=\"report.pdf\"").as_deref(), Some("report.pdf"));
        assert_eq!(cd("attachment; filename=plain.zip").as_deref(), Some("plain.zip"));
        assert_eq!(
            cd("attachment; filename=\"fallback.txt\"; filename*=UTF-8''na%C3%AFve%20file.txt")
                .as_deref(),
            Some("naïve file.txt")
        );
        assert_eq!(cd("attachment; filename=\"a;b.txt\"").as_deref(), Some("a;b.txt"));
        assert_eq!(cd("inline"), None);
    }

    #[test]
    fn url_names_are_decoded() {
        let url = url::Url::parse("https://example.com/dir/My%20File.iso?token=1").unwrap();
        assert_eq!(from_url(&url).as_deref(), Some("My File.iso"));
        assert_eq!(from_url(&url::Url::parse("https://example.com/").unwrap()), None);
    }

    #[test]
    fn sanitize_strips_unsafe_parts() {
        assert_eq!(sanitize("../../etc/passwd"), "passwd");
        assert_eq!(sanitize("C:\\evil\\a:b?.txt"), "a_b_.txt");
        assert_eq!(sanitize("con.txt"), "_con.txt");
        assert_eq!(sanitize("trailing. . "), "trailing");
        assert_eq!(sanitize(""), "download");
        let long = format!("{}.bin", "é".repeat(150));
        let short = sanitize(&long);
        assert!(short.len() <= MAX_NAME_BYTES && short.ends_with(".bin"));
    }

    #[test]
    fn unique_appends_a_counter() {
        let dir = Path::new("d");
        let taken = [dir.join("a.zip"), dir.join("a (1).zip")];
        assert_eq!(unique(dir, "a.zip", |p| taken.iter().any(|t| t == p)), "a (2).zip");
        assert_eq!(unique(dir, "b.zip", |p| taken.iter().any(|t| t == p)), "b.zip");
    }
}
