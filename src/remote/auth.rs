//! Pairing tokens. Every request carries `Authorization: Bearer <token>`.

/// Constant-time comparison of a presented token against the expected one.
/// Length differences still return false, but every byte is visited so
/// timing does not reveal how many leading bytes matched.
pub fn token_matches(expected: &str, presented: &str) -> bool {
    let expected = expected.as_bytes();
    let presented = presented.as_bytes();
    let mut diff = (expected.len() ^ presented.len()) as u8;
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

// TODO(server agent): `pub fn generate_token() -> String` of
// `protocol::TOKEN_BYTES` random bytes as lowercase hex, from the operating
// system's random source (add `getrandom` or `rand` to Cargo.toml).

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
    }

    #[test]
    fn bearer_header_is_parsed_case_insensitively() {
        assert_eq!(bearer_token("Bearer abc"), Some("abc"));
        assert_eq!(bearer_token("bearer  abc "), Some("abc"));
        assert_eq!(bearer_token("Basic abc"), None);
        assert_eq!(bearer_token("Bearer "), None);
        assert_eq!(bearer_token("abc"), None);
    }
}
