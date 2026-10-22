//! Telling a file's type from its first bytes, for files the server sends without an extension.

use std::{fs::File, io::Read, path::Path};

/// Enough to reach the `CD001` mark of an ISO image, the furthest signature below.
pub(crate) const HEAD_LEN: usize = 0x8006;

/// Reads the start of a file, or less when it is shorter.
pub(crate) fn head(path: &Path) -> std::io::Result<Vec<u8>> {
    let mut head = Vec::with_capacity(HEAD_LEN);
    File::open(path)?.take(HEAD_LEN as u64).read_to_end(&mut head)?;
    Ok(head)
}

/// The usual extension for a file that starts with `head`, without the dot. Only signatures
/// that are hard to hit by chance count; anything else is left without a guess.
pub(crate) fn extension(head: &[u8]) -> Option<&'static str> {
    let at = |offset: usize, magic: &[u8]| head.get(offset..offset + magic.len()) == Some(magic);
    let ext = match () {
        // Video and audio.
        () if at(4, b"ftyp") => match head.get(8..12)? {
            b"qt  " => "mov",
            b"M4A " | b"M4B " => "m4a",
            b"heic" | b"heix" | b"mif1" => "heic",
            b"avif" => "avif",
            brand if brand.starts_with(b"3g") => "3gp",
            _ => "mp4",
        },
        () if at(0, b"\x1a\x45\xdf\xa3") => {
            // The document type is near the start of the header.
            let header = &head[..head.len().min(64)];
            match header.windows(4).any(|w| w == b"webm") {
                true => "webm",
                false => "mkv",
            }
        }
        () if at(0, b"RIFF") && at(8, b"AVI ") => "avi",
        () if at(0, b"RIFF") && at(8, b"WAVE") => "wav",
        () if at(0, b"RIFF") && at(8, b"WEBP") => "webp",
        () if at(0, b"FLV\x01") => "flv",
        () if at(0, b"OggS") => "ogg",
        () if at(0, b"fLaC") => "flac",
        () if at(0, b"ID3") => "mp3",
        () if at(0, b"\x30\x26\xb2\x75\x8e\x66\xcf\x11") => "wmv",
        // MPEG transport streams repeat a sync byte every 188 bytes.
        () if head.len() >= 188 * 3 && (0..3).all(|i| head[i * 188] == 0x47) => "ts",
        // Images and documents.
        () if at(0, b"\x89PNG\r\n\x1a\n") => "png",
        () if at(0, b"\xff\xd8\xff") => "jpg",
        () if at(0, b"GIF87a") || at(0, b"GIF89a") => "gif",
        () if at(0, b"%PDF-") => "pdf",
        // Archives and disk images.
        () if at(0, b"PK\x03\x04") => "zip",
        () if at(0, b"Rar!\x1a\x07") => "rar",
        () if at(0, b"7z\xbc\xaf\x27\x1c") => "7z",
        () if at(0, b"\x1f\x8b") => "gz",
        () if at(0, b"\xfd7zXZ\x00") => "xz",
        () if at(0, b"BZh") => "bz2",
        () if at(0, b"\x28\xb5\x2f\xfd") => "zst",
        () if at(0, b"MSCF") => "cab",
        () if at(257, b"ustar") => "tar",
        () if at(0x8001, b"CD001") => "iso",
        // Programs.
        () if at(0, b"MZ") => "exe",
        () => return None,
    };
    Some(ext)
}

/// Whether `name` already ends in an extension.
pub(crate) fn has_extension(name: &str) -> bool {
    Path::new(name).extension().is_some_and(|ext| !ext.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_common_formats() {
        assert_eq!(extension(b"\0\0\0\x20ftypisom\0\0\x02\0"), Some("mp4"));
        assert_eq!(extension(b"\0\0\0\x14ftypqt  \0\0\0\0"), Some("mov"));
        assert_eq!(extension(b"FLV\x01\x05\0\0\0\x09"), Some("flv"));
        assert_eq!(extension(b"\x1a\x45\xdf\xa3\x9f\x42\x82\x84webm"), Some("webm"));
        assert_eq!(extension(b"\x1a\x45\xdf\xa3\xa3\x42\x82\x88matroska"), Some("mkv"));
        assert_eq!(extension(b"RIFF\0\0\0\0WAVEfmt "), Some("wav"));
        assert_eq!(extension(b"%PDF-1.7"), Some("pdf"));
        assert_eq!(extension(b"PK\x03\x04\x14\0"), Some("zip"));
        assert_eq!(extension(b"MZ\x90\0"), Some("exe"));

        let mut tar = vec![0; 512];
        tar[257..262].copy_from_slice(b"ustar");
        assert_eq!(extension(&tar), Some("tar"));
        let mut iso = vec![0; HEAD_LEN];
        iso[0x8001..0x8006].copy_from_slice(b"CD001");
        assert_eq!(extension(&iso), Some("iso"));
    }

    #[test]
    fn makes_no_guess_without_a_signature() {
        assert_eq!(extension(b""), None);
        assert_eq!(extension(b"hello, world"), None);
        assert_eq!(extension(b"\0\0\0\x20ftyp"), None, "too short to read the brand");
    }

    #[test]
    fn finds_extensions() {
        assert!(has_extension("movie.mp4"));
        assert!(!has_extension("sample"));
        assert!(!has_extension(".hidden"));
        assert!(!has_extension("trailing."));
    }
}
