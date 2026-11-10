//! Pairing tokens. Every request carries `Authorization: Bearer <token>`.

/// Constant-time comparison of a presented token against the expected one.
/// Length differences still return false, but every byte is visited so
/// timing does not reveal how many leading bytes matched.
pub fn token_matches(expected: &str, presented: &str) -> bool {
    let expected = expected.as_bytes();
    let presented = presented.as_bytes();
    let mut diff = u8::from(expected.len() != presented.len());
    for i in 0..expected.len().max(presented.len()) {
        let a = expected.get(i).copied().unwrap_or(0);
        let b = presented.get(i).copied().unwrap_or(0);
        diff |= a ^ b;
    }
    diff == 0
}

/// The token from an `Authorization` header value, if it is a bearer token.
pub fn bearer_token(header: &str) -> Option<&str> {
    let (scheme, token) = header.trim().split_once(' ')?;
    scheme
        .eq_ignore_ascii_case("bearer")
        .then(|| token.trim())
        .filter(|token| !token.is_empty())
}

use anyhow::{Context, Result};
use std::path::Path;

const TOKEN_FILE: &str = "token";

/// A new pairing token: `protocol::TOKEN_BYTES` bytes from the operating
/// system's random source, as lowercase hex.
pub fn generate_token() -> String {
    let mut bytes = [0u8; crate::remote::protocol::TOKEN_BYTES];
    getrandom::fill(&mut bytes).expect("the operating system's random source is unavailable");
    crate::remote::ids::encode_hex(&bytes)
}

/// Whether a saved value has the shape of a token.
pub fn is_valid_token(token: &str) -> bool {
    token.len() == crate::remote::protocol::TOKEN_BYTES * 2
        && token
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// The token saved in `<state_dir>/token`, made and saved (mode 0600 on
/// Unix) when there is none or `replace` is set. A damaged file is an error
/// rather than a silent revocation of every paired Flick.
pub fn load_or_create_token(state_dir: &Path, replace: bool) -> Result<String> {
    let path = state_dir.join(TOKEN_FILE);
    if !replace {
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                let token = text.trim().to_owned();
                anyhow::ensure!(
                    is_valid_token(&token),
                    "{} is not a valid token; replace it with --new-token",
                    path.display()
                );
                return Ok(token);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).with_context(|| format!("Read {}", path.display())),
        }
    }
    let token = generate_token();
    write_private(&path, token.as_bytes())?;
    Ok(token)
}

/// The token saved in `<state_dir>/token`, without making one. For tools
/// that talk to a running server, such as `flick-serve status`.
pub fn saved_token(state_dir: &Path) -> Result<String> {
    let path = state_dir.join(TOKEN_FILE);
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("Read {}", path.display()))?;
    let token = text.trim().to_owned();
    anyhow::ensure!(
        is_valid_token(&token),
        "{} is not a valid token",
        path.display()
    );
    Ok(token)
}

/// Writes a secret through a temporary file and a rename, readable only by
/// this user on Unix.
pub(crate) fn write_private(path: &Path, contents: &[u8]) -> Result<()> {
    use std::io::Write;
    let dir = path.parent().context("A state file needs a folder")?;
    std::fs::create_dir_all(dir).with_context(|| format!("Create {}", dir.display()))?;
    let temporary = path.with_extension("tmp");
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&temporary)
        .with_context(|| format!("Write {}", temporary.display()))?;
    #[cfg(unix)]
    {
        // An existing temporary keeps its old mode; tighten it explicitly.
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(contents)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&temporary, path).with_context(|| format!("Save {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_compare_exactly() {
        assert!(token_matches("abc", "abc"));
        assert!(!token_matches("abc", "abd"));
        assert!(!token_matches("abc", "ab"));
        assert!(!token_matches("abc", "abcd"));
        assert!(!token_matches("", "a"));
        assert!(token_matches("", ""));
        // Lengths that differ by a multiple of 256 still differ.
        assert!(!token_matches("", &"\0".repeat(256)));
        assert!(!token_matches("a", &format!("a{}", "\0".repeat(256))));
    }

    #[test]
    fn bearer_header_is_parsed_case_insensitively() {
        assert_eq!(bearer_token("Bearer abc"), Some("abc"));
        assert_eq!(bearer_token("bearer  abc "), Some("abc"));
        assert_eq!(bearer_token("Basic abc"), None);
        assert_eq!(bearer_token("Bearer "), None);
        assert_eq!(bearer_token("abc"), None);
    }

    #[test]
    fn generated_tokens_pair_and_differ() {
        let token = generate_token();
        assert!(is_valid_token(&token), "{token}");
        assert_ne!(token, generate_token());
        let pairing = crate::remote::protocol::Pairing {
            host: "100.98.200.69".into(),
            port: crate::remote::protocol::DEFAULT_PORT,
            token: token.clone(),
        };
        assert_eq!(
            crate::remote::protocol::Pairing::parse(&pairing.to_string())
                .unwrap()
                .token,
            token
        );
    }

    #[test]
    fn saved_token_persists_until_replaced() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let first = load_or_create_token(dir.path(), false)?;
        assert_eq!(load_or_create_token(dir.path(), false)?, first);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.path().join(TOKEN_FILE))?
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        assert_eq!(saved_token(dir.path())?, first);
        let second = load_or_create_token(dir.path(), true)?;
        assert_ne!(second, first);
        assert_eq!(load_or_create_token(dir.path(), false)?, second);
        std::fs::write(dir.path().join(TOKEN_FILE), "short")?;
        assert!(load_or_create_token(dir.path(), false).is_err());
        assert!(saved_token(dir.path()).is_err());
        assert!(saved_token(&dir.path().join("none")).is_err());
        Ok(())
    }
}
