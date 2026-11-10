//! What the server serves: the desktop app's saved library, read-only, or
//! folders scanned for videos.
//!
//! Owned by the server agent. Intended API:
//!
//! ```ignore
//! pub enum Sources { SettingsFile(PathBuf), Folders(Vec<PathBuf>) }
//! pub struct Entry {
//!     pub path: PathBuf,          // canonical
//!     pub title: String,
//!     pub poster: Option<PathBuf>,
//!     pub duration: f64,
//!     pub width: u32,
//!     pub height: u32,
//!     pub complete: bool,         // false for an incomplete torrent file
//! }
//! pub async fn load(sources: &Sources) -> anyhow::Result<Vec<Entry>>;
//! ```
//!
//! `SettingsFile` parses only the `library` array of the desktop
//! `settings.json` (fields `path`, `title`, `poster`, `duration`, `width`,
//! `height`, `torrent`) with a private serde struct here; the desktop
//! `store` module is not available without the desktop feature. A video
//! with a `torrent` identity whose path is not a complete file, or that is
//! listed in `unverified_torrent_files`, is `complete: false`.
//! `Folders` uses `crate::library::scan_folder` and `crate::library::parse_episode`.
