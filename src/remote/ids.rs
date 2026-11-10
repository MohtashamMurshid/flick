//! Opaque video identifiers: SHA-1 of a per-server random salt and the
//! canonical path, as 40 lowercase hex characters. The server keeps the
//! id-to-path map in memory; a client never sends a path.
//!
//! The salt lives in `<state_dir>/salt` as 32 hex characters, so ids stay
//! the same across restarts and differ between servers.

use anyhow::{Context, Result};
use sha1::{Digest, Sha1};
use std::path::Path;

const SALT_FILE: &str = "salt";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Salt(pub [u8; 16]);

impl Salt {
    /// Reads the saved salt, or makes and saves a new one.
    pub fn load_or_create(state_dir: &Path) -> Result<Salt> {
        let path = state_dir.join(SALT_FILE);
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                let bytes = decode_hex(text.trim())
                    .filter(|bytes| bytes.len() == 16)
                    .with_context(|| format!("{} is not a valid salt", path.display()))?;
                let mut salt = [0; 16];
                salt.copy_from_slice(&bytes);
                Ok(Salt(salt))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let mut salt = [0; 16];
                getrandom::fill(&mut salt)
                    .map_err(|error| anyhow::anyhow!("Read the system random source: {error}"))?;
                crate::remote::auth::write_private(&path, encode_hex(&salt).as_bytes())?;
                Ok(Salt(salt))
            }
            Err(error) => Err(error).with_context(|| format!("Read {}", path.display())),
        }
    }
}

/// The id of a video, from its canonical path.
pub fn video_id(salt: &Salt, canonical_path: &Path) -> String {
    let mut hasher = Sha1::new();
    hasher.update(salt.0);
    hasher.update(path_bytes(canonical_path).as_ref());
    encode_hex(&hasher.finalize())
}

/// Whether a value from a URL could be an id: 40 lowercase hex characters.
pub fn is_valid_id(id: &str) -> bool {
    id.len() == 40 && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

#[cfg(unix)]
fn path_bytes(path: &Path) -> std::borrow::Cow<'_, [u8]> {
    use std::os::unix::ffi::OsStrExt;
    std::borrow::Cow::Borrowed(path.as_os_str().as_bytes())
}

#[cfg(not(unix))]
fn path_bytes(path: &Path) -> std::borrow::Cow<'_, [u8]> {
    match path.to_string_lossy() {
        std::borrow::Cow::Borrowed(text) => std::borrow::Cow::Borrowed(text.as_bytes()),
        std::borrow::Cow::Owned(text) => std::borrow::Cow::Owned(text.into_bytes()),
    }
}

pub(crate) fn encode_hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut text, byte| {
        let _ = write!(text, "{byte:02x}");
        text
    })
}

fn decode_hex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) || !text.is_ascii() {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_stable_per_salt_and_differ_between_salts() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let first = Salt::load_or_create(dir.path())?;
        let again = Salt::load_or_create(dir.path())?;
        assert_eq!(first, again);
        let path = Path::new("/Volumes/Media/Film.mkv");
        let id = video_id(&first, path);
        assert!(is_valid_id(&id), "{id}");
        assert_eq!(id, video_id(&again, path));
        assert_ne!(id, video_id(&first, Path::new("/Volumes/Media/Other.mkv")));
        let other = tempfile::tempdir()?;
        assert_ne!(id, video_id(&Salt::load_or_create(other.path())?, path));
        Ok(())
    }

    #[test]
    fn a_damaged_salt_is_an_error_not_a_new_identity() -> Result<()> {
        let dir = tempfile::tempdir()?;
        std::fs::write(dir.path().join(SALT_FILE), "not hex")?;
        assert!(Salt::load_or_create(dir.path()).is_err());
        Ok(())
    }

    #[test]
    fn id_syntax_is_checked() {
        assert!(is_valid_id(&"a".repeat(40)));
        assert!(!is_valid_id(&"A".repeat(40)));
        assert!(!is_valid_id(&"a".repeat(39)));
        assert!(!is_valid_id(&format!("{}g", "a".repeat(39))));
        assert!(!is_valid_id("../../etc/passwd"));
        assert_eq!(encode_hex(&[0, 15, 255]), "000fff");
        assert_eq!(decode_hex("000fff"), Some(vec![0, 15, 255]));
        assert_eq!(decode_hex("0"), None);
    }
}
