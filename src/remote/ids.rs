//! Opaque video identifiers: SHA-1 of a per-server random salt and the
//! canonical path, as 40 lowercase hex characters. The server keeps the
//! id-to-path map in memory; a client never sends a path.
//!
//! Owned by the server agent. Intended API:
//!
//! ```ignore
//! pub struct Salt(pub [u8; 16]);
//! impl Salt { pub fn load_or_create(state_dir: &Path) -> anyhow::Result<Salt>; }
//! pub fn video_id(salt: &Salt, canonical_path: &Path) -> String;
//! pub fn is_valid_id(id: &str) -> bool; // 40 hex characters
//! ```
