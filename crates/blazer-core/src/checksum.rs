//! Checking a finished file against the SHA-256 it should have.

use std::{fs::File, io::Read, path::Path};

use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

/// The SHA-256 in `text`, in lowercase hex: the first run of 64 hex digits, so a line as
/// `sha256sum` prints it (`<hash>  <file>`) or `sha256:<hash>` works too.
pub fn parse_sha256(text: &str) -> Option<String> {
    text.split(|c: char| !c.is_ascii_hexdigit())
        .find(|word| word.len() == 64)
        .map(str::to_ascii_lowercase)
}

/// The file's SHA-256 in lowercase hex, or `None` if `cancel` fired first. Blocks while it reads
/// the whole file.
pub(crate) fn hash_file(path: &Path, cancel: &CancellationToken) -> std::io::Result<Option<String>> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0; 1 << 20];
    loop {
        if cancel.is_cancelled() {
            return Ok(None);
        }
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(Some(hasher.finalize().iter().map(|b| format!("{b:02x}")).collect()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ABC: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    #[test]
    fn finds_the_hash_in_pasted_text() {
        assert_eq!(parse_sha256(ABC).as_deref(), Some(ABC));
        assert_eq!(parse_sha256(&format!("  {}\n", ABC.to_uppercase())).as_deref(), Some(ABC));
        assert_eq!(parse_sha256(&format!("{ABC}  setup.exe")).as_deref(), Some(ABC));
        assert_eq!(parse_sha256(&format!("SHA256: {ABC}")).as_deref(), Some(ABC));
        assert_eq!(parse_sha256(&ABC[1..]), None);
        assert_eq!(parse_sha256(&format!("{ABC}0")), None);
        assert_eq!(parse_sha256(""), None);
    }

    #[test]
    fn hashes_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("abc.txt");
        std::fs::write(&path, b"abc").unwrap();
        let hash = hash_file(&path, &CancellationToken::new()).unwrap();
        assert_eq!(hash.as_deref(), Some(ABC));
    }
}
