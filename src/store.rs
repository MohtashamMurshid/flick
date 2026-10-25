use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

use crate::model::{file_title, non_negative_finite};

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct Settings {
    pub volume: f64,
    pub muted: bool,
    pub speed: f64,
    pub recent: Vec<RecentFile>,
    pub library: Vec<LibraryVideo>,
    pub appearance: crate::theme::Appearance,
    pub shortcuts: crate::shortcuts::Bindings,
    pub library_view: LibraryView,
    /// A personal OpenSubtitles API key, stored with other settings.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub opensubtitles_key: String,
    /// Comma-separated ISO 639-1 codes for subtitle searches.
    pub subtitle_languages: String,
    pub transfers: TransferPreferences,
    pub torrent_history: Vec<TorrentResume>,
}

/// None keeps the first-run choice visible; it behaves as off until chosen.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct TransferPreferences {
    pub seed_completed: Option<bool>,
    /// Zero means unlimited. Values are bytes per second.
    pub download_bps: u32,
    pub upload_bps: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct TorrentIdentity {
    pub infohash: String,
    pub file_index: usize,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TorrentResume {
    pub identity: TorrentIdentity,
    pub path: PathBuf,
    pub position: f64,
    pub duration: f64,
    #[serde(default)]
    pub finished: bool,
}

/// How All Videos is sorted and laid out.
#[derive(Clone, Copy, Debug, Default, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct LibraryView {
    pub sort: Sort,
    pub list: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Sort {
    /// Newest additions first.
    #[default]
    Added,
    Title,
    /// Most recently watched first, then videos never watched.
    Watched,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            volume: 80.0,
            muted: false,
            speed: 1.0,
            recent: Vec::new(),
            library: Vec::new(),
            appearance: crate::theme::Appearance::default(),
            shortcuts: crate::shortcuts::Bindings::default(),
            library_view: LibraryView::default(),
            opensubtitles_key: String::new(),
            subtitle_languages: "en".into(),
            transfers: TransferPreferences::default(),
            torrent_history: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct RecentFile {
    pub path: PathBuf,
    pub title: String,
    pub position: f64,
    pub duration: f64,
    /// Removed from Continue Watching until the video is played again. The
    /// saved position is kept, so opening it still resumes.
    pub dismissed: bool,
    /// Played to the end or marked watched. Opening a video alone does not
    /// set it, and watching again keeps it until marked unwatched.
    pub finished: bool,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct LibraryVideo {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub torrent: Option<TorrentIdentity>,
    pub path: PathBuf,
    pub title: String,
    /// A cached poster frame, made by the thumbnail worker.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub poster: Option<PathBuf>,
    /// Seconds, or 0 until the video has been read.
    #[serde(skip_serializing_if = "is_zero")]
    pub duration: f64,
    /// Display size after aspect correction, or 0 until known.
    #[serde(skip_serializing_if = "is_zero_u32")]
    pub width: u32,
    #[serde(skip_serializing_if = "is_zero_u32")]
    pub height: u32,
}

fn is_zero(value: &f64) -> bool {
    *value == 0.0
}

fn is_zero_u32(value: &u32) -> bool {
    *value == 0
}

impl LibraryVideo {
    fn new(path: PathBuf) -> Self {
        Self {
            title: file_title(&path),
            path,
            ..Default::default()
        }
    }

    /// A short resolution label such as `4K` or `1080p`.
    pub fn resolution(&self) -> Option<&'static str> {
        let (w, h) = (self.width.max(self.height), self.width.min(self.height));
        Some(match (w, h) {
            (0, _) | (_, 0) => return None,
            (w, h) if w >= 7680 || h >= 4320 => "8K",
            (w, h) if w >= 3840 || h >= 2160 => "4K",
            (w, h) if w >= 2560 || h >= 1440 => "1440p",
            (w, h) if w >= 1920 || h >= 1080 => "1080p",
            (w, h) if w >= 1280 || h >= 720 => "720p",
            _ => "SD",
        })
    }
}

impl Settings {
    /// The OpenSubtitles key from `FLICK_OPENSUBTITLES_KEY`, or settings.
    pub fn subtitle_key(&self) -> String {
        std::env::var("FLICK_OPENSUBTITLES_KEY")
            .ok()
            .filter(|key| !key.trim().is_empty())
            .unwrap_or_else(|| self.opensubtitles_key.trim().to_owned())
    }

    pub fn add_video(&mut self, path: &Path) {
        if !path.as_os_str().is_empty() && !self.library.iter().any(|v| v.path == path) {
            self.library.push(LibraryVideo::new(path.to_owned()));
        }
    }

    pub fn add_videos(&mut self, paths: impl IntoIterator<Item = PathBuf>) {
        let mut known = self
            .library
            .iter()
            .map(|video| video.path.clone())
            .collect::<std::collections::HashSet<_>>();
        for path in paths {
            if !path.as_os_str().is_empty() && known.insert(path.clone()) {
                self.library.push(LibraryVideo::new(path));
            }
        }
    }
    /// Exact paths are deduplicated without accessing or canonicalizing media files.
    pub fn remember(&mut self, path: &Path, position: f64, duration: f64) {
        if path.as_os_str().is_empty() {
            return;
        }
        self.add_video(path);
        let finished = self.watched(path);
        self.recent.retain(|file| file.path != path);
        let mut file = RecentFile {
            path: path.to_owned(),
            title: file_title(path),
            position,
            duration,
            dismissed: false,
            finished,
        };
        file.sanitize();
        file.finished |= reached_end(file.position, file.duration);
        if let Some(identity) = self
            .library
            .iter()
            .find(|v| v.path == path)
            .and_then(|v| v.torrent.clone())
        {
            self.torrent_history.retain(|h| h.identity != identity);
            self.torrent_history.insert(
                0,
                TorrentResume {
                    identity,
                    path: path.to_owned(),
                    position: file.position,
                    duration: file.duration,
                    finished: file.finished,
                },
            );
        }
        self.recent.insert(0, file);
    }

    /// Bind verified media without discarding path-based history or old paths.
    pub fn bind_torrent(&mut self, path: &Path, infohash: &str, file_index: usize) {
        if infohash.len() != 40 || !infohash.bytes().all(|b| b.is_ascii_hexdigit()) {
            return;
        }
        self.add_video(path);
        let identity = TorrentIdentity {
            infohash: infohash.to_ascii_lowercase(),
            file_index,
        };
        if let Some(video) = self.library.iter_mut().find(|v| v.path == path) {
            video.torrent = Some(identity.clone());
        }
        if !self.torrent_history.iter().any(|h| h.identity == identity)
            && let Some(file) = self.recent.iter().find(|f| f.path == path)
        {
            self.torrent_history.push(TorrentResume {
                identity,
                path: path.to_owned(),
                position: file.position,
                duration: file.duration,
                finished: file.finished,
            });
        }
    }

    /// Videos with a saved position that are not dismissed, newest first.
    pub fn continue_watching(&self) -> impl Iterator<Item = &RecentFile> {
        self.recent
            .iter()
            .filter(|file| !file.dismissed && self.resume_position(&file.path) > 0.0)
    }

    pub fn dismiss_from_continue(&mut self, path: &Path) {
        if let Some(file) = self.recent.iter_mut().find(|file| file.path == path) {
            file.dismissed = true;
        }
    }

    /// Records the video as finished, keeping its place in history.
    pub fn mark_watched(&mut self, path: &Path) {
        let (index, duration) = self
            .recent
            .iter()
            .position(|file| file.path == path)
            .map_or((None, 0.0), |index| {
                (Some(index), self.recent[index].duration)
            });
        let file = RecentFile {
            path: path.to_owned(),
            title: file_title(path),
            position: duration,
            duration,
            dismissed: false,
            finished: true,
        };
        match index {
            Some(index) => self.recent[index] = file,
            None => self.recent.insert(0, file),
        }
        self.add_video(path);
        if let Some(identity) = self
            .library
            .iter()
            .find(|v| v.path == path)
            .and_then(|v| v.torrent.clone())
        {
            self.torrent_history.retain(|h| h.identity != identity);
            self.torrent_history.insert(
                0,
                TorrentResume {
                    identity,
                    path: path.to_owned(),
                    position: duration,
                    duration,
                    finished: true,
                },
            );
        }
    }

    /// Forgets the video's history and saved position. It stays in the library.
    pub fn mark_unwatched(&mut self, path: &Path) {
        self.recent.retain(|file| file.path != path);
        if let Some(identity) = self
            .library
            .iter()
            .find(|v| v.path == path)
            .and_then(|v| v.torrent.clone())
        {
            self.torrent_history.retain(|h| h.identity != identity);
            // Keep a zero-position identity record so older path history cannot
            // resurrect resume for a different copy of the same torrent file.
            self.torrent_history.insert(
                0,
                TorrentResume {
                    identity,
                    path: path.to_owned(),
                    position: 0.0,
                    duration: 0.0,
                    finished: false,
                },
            );
        }
    }

    /// Whether the video was played to the end or marked watched. Torrent
    /// media follows its torrent's record, like the resume position.
    pub fn watched(&self, path: &Path) -> bool {
        if let Some(identity) = self
            .library
            .iter()
            .find(|v| v.path == path)
            .and_then(|v| v.torrent.as_ref())
            && let Some(saved) = self
                .torrent_history
                .iter()
                .find(|h| &h.identity == identity)
        {
            return saved.finished;
        }
        self.recent
            .iter()
            .any(|file| file.path == path && file.finished)
    }

    /// Library videos in the chosen order. Ties keep the order of addition.
    pub fn sorted_library(&self, sort: Sort) -> Vec<&LibraryVideo> {
        let mut videos = self.library.iter().rev().collect::<Vec<_>>();
        match sort {
            Sort::Added => {}
            Sort::Title => videos.sort_by_cached_key(|video| video.title.to_lowercase()),
            Sort::Watched => {
                let order = self
                    .recent
                    .iter()
                    .enumerate()
                    .map(|(index, file)| (file.path.as_path(), index))
                    .collect::<std::collections::HashMap<_, _>>();
                videos.sort_by_key(|video| {
                    order
                        .get(video.path.as_path())
                        .copied()
                        .unwrap_or(usize::MAX)
                });
            }
        }
        videos
    }

    pub fn resume_position(&self, path: &Path) -> f64 {
        if let Some(identity) = self
            .library
            .iter()
            .find(|v| v.path == path)
            .and_then(|v| v.torrent.as_ref())
            && let Some(saved) = self
                .torrent_history
                .iter()
                .find(|h| &h.identity == identity)
        {
            return eligible_resume(saved.position, saved.duration);
        }
        self.recent
            .iter()
            .find(|file| file.path == path)
            .filter(|file| {
                file.position.is_finite()
                    && file.duration.is_finite()
                    && file.position > 5.0
                    && file.duration - file.position >= 10.0
            })
            .map_or(0.0, |file| file.position)
    }

    fn sanitize(&mut self) {
        let mut identities = std::collections::HashSet::new();
        self.torrent_history.retain_mut(|saved| {
            saved.identity.infohash = saved.identity.infohash.to_ascii_lowercase();
            saved.duration = non_negative_finite(saved.duration);
            saved.position = non_negative_finite(saved.position).min(saved.duration);
            saved.identity.infohash.len() == 40
                && saved
                    .identity
                    .infohash
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit())
                && identities.insert((saved.identity.infohash.clone(), saved.identity.file_index))
        });
        self.shortcuts.sanitize();
        self.appearance.sanitize();
        self.volume = finite_or(self.volume, 80.0).clamp(0.0, 100.0);
        self.speed = finite_or(self.speed, 1.0).clamp(0.25, 4.0);
        let mut seen = std::collections::HashSet::new();
        self.recent.retain_mut(|file| {
            file.sanitize();
            !file.path.as_os_str().is_empty() && seen.insert(file.path.clone())
        });
        let mut seen_library = std::collections::HashSet::new();
        self.library.retain_mut(|video| {
            if video.title.trim().is_empty() {
                video.title = file_title(&video.path);
            }
            !video.path.as_os_str().is_empty() && seen_library.insert(video.path.clone())
        });
        for file in &self.recent {
            if seen_library.insert(file.path.clone()) {
                self.library.push(LibraryVideo::new(file.path.clone()));
            }
        }
        for video in &mut self.library {
            video.duration = non_negative_finite(video.duration);
            if let Some(identity) = &mut video.torrent {
                identity.infohash = identity.infohash.to_ascii_lowercase();
                if identity.infohash.len() != 40
                    || !identity.infohash.bytes().all(|b| b.is_ascii_hexdigit())
                {
                    video.torrent = None;
                }
            }
        }
    }
}

impl RecentFile {
    fn sanitize(&mut self) {
        self.duration = non_negative_finite(self.duration);
        self.position = non_negative_finite(self.position);
        if self.duration > 0.0 {
            self.position = self.position.min(self.duration);
        }
        if self.title.trim().is_empty() {
            self.title = file_title(&self.path);
        }
    }
}

/// Within the last 10 seconds, where resuming stops, after playing at least
/// half, so closing a short clip right after opening it does not count.
fn reached_end(position: f64, duration: f64) -> bool {
    duration > 0.0 && duration - position < 10.0 && position >= duration / 2.0
}

fn eligible_resume(position: f64, duration: f64) -> f64 {
    if position.is_finite() && duration.is_finite() && position > 5.0 && duration - position >= 10.0
    {
        position
    } else {
        0.0
    }
}

fn finite_or(value: f64, fallback: f64) -> f64 {
    if value.is_finite() { value } else { fallback }
}

pub struct Store {
    pub data: Settings,
    path: Option<PathBuf>,
}

impl Store {
    pub fn load() -> Self {
        if let Some(path) = std::env::var_os("FLICK_SETTINGS_PATH") {
            return Self::load_from(PathBuf::from(path));
        }
        match ProjectDirs::from("app", "Flick", "Flick") {
            Some(dirs) => Self::load_from(dirs.config_dir().join("settings.json")),
            None => {
                tracing::warn!("No application configuration directory; settings stay in memory");
                Self {
                    data: Settings::default(),
                    path: None,
                }
            }
        }
    }

    /// A failed read never modifies the source file and falls back to defaults.
    pub fn load_from(path: impl AsRef<Path>) -> Self {
        let path = path.as_ref().to_owned();
        let mut data = match fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<Settings>(&bytes) {
                Ok(data) => data,
                Err(error) => {
                    tracing::warn!(path = %path.display(), %error, "Invalid settings; using defaults");
                    Settings::default()
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Settings::default(),
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "Cannot read settings; using defaults");
                Settings::default()
            }
        };
        data.sanitize();
        Self {
            data,
            path: Some(path),
        }
    }

    pub fn save(&self) -> Result<()> {
        let path = self
            .path
            .as_ref()
            .context("No settings directory is available")?;
        self.save_to(path)
    }

    /// Write and sync a sibling temporary file, then replace the destination.
    /// Never delete the previous settings before the replacement succeeds.
    pub fn save_to(&self, path: impl AsRef<Path>) -> Result<()> {
        let path = path.as_ref();
        let mut data = self.data.clone();
        data.sanitize();
        let mut bytes = serde_json::to_vec_pretty(&data).context("Cannot serialize settings")?;
        bytes.push(b'\n');

        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent)
            .with_context(|| format!("Cannot create settings directory {}", parent.display()))?;
        let (temporary, mut file) = TemporaryFile::create(parent)?;
        file.write_all(&bytes)
            .context("Cannot write temporary settings")?;
        file.sync_all().context("Cannot sync temporary settings")?;
        drop(file);
        fs::rename(&temporary.path, path)
            .with_context(|| format!("Cannot replace settings at {}", path.display()))?;
        Ok(())
    }

    pub fn remember(&mut self, path: &Path, position: f64, duration: f64) {
        self.data.remember(path, position, duration);
    }

    pub fn resume_position(&self, path: &Path) -> f64 {
        self.data.resume_position(path)
    }
}

struct TemporaryFile {
    path: PathBuf,
}

impl TemporaryFile {
    fn create(parent: &Path) -> Result<(Self, File)> {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        for _ in 0..16 {
            let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let path = parent.join(format!(
                ".flick-settings-{}-{timestamp}-{sequence}.tmp",
                std::process::id()
            ));
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(file) => return Ok((Self { path }, file)),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error).context("Cannot create temporary settings"),
            }
        }
        anyhow::bail!("Cannot allocate a unique temporary settings file")
    }
}

impl Drop for TemporaryFile {
    fn drop(&mut self) {
        // After a successful rename the old temporary path no longer exists.
        let _ = fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn torrent_resume_follows_identity_across_paths_without_losing_local_history() {
        let directory = tempfile::tempdir().unwrap();
        let settings_path = directory.path().join("settings.json");
        let mut store = Store::load_from(&settings_path);
        let hash = "0123456789abcdef0123456789abcdef01234567";
        let original = Path::new("old/download/movie.mkv");
        let moved = Path::new("new/download/movie.mkv");
        // Bind an already-watched local path without destructive migration.
        store.remember(original, 32.0, 200.0);
        store.data.bind_torrent(original, hash, 2);
        store.remember(Path::new("local.mp4"), 24.0, 100.0);
        store
            .data
            .bind_torrent(moved, &hash.to_ascii_uppercase(), 2);
        assert_eq!(store.resume_position(moved), 32.0);
        store.remember(moved, 75.0, 200.0);
        store.save().unwrap();
        let mut loaded = Store::load_from(&settings_path);
        assert_eq!(loaded.resume_position(moved), 75.0);
        assert_eq!(loaded.resume_position(original), 75.0);
        assert_eq!(loaded.resume_position(Path::new("local.mp4")), 24.0);
        assert!(
            loaded
                .data
                .recent
                .iter()
                .any(|f| f.path == original && f.position == 32.0)
        );
        loaded.data.bind_torrent(Path::new("another.mkv"), hash, 3);
        loaded.data.bind_torrent(
            Path::new("other-hash.mkv"),
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            2,
        );
        assert_eq!(loaded.resume_position(Path::new("another.mkv")), 0.0);
        assert_eq!(loaded.resume_position(Path::new("other-hash.mkv")), 0.0);
        // Watched state follows the torrent too, whichever path played it.
        assert!(!loaded.data.watched(original));
        loaded.data.mark_watched(moved);
        assert_eq!(loaded.resume_position(original), 0.0);
        assert!(loaded.data.watched(original));
        loaded.data.remember(moved, 60.0, 200.0);
        loaded.data.mark_unwatched(moved);
        assert_eq!(loaded.resume_position(original), 0.0);
        assert!(!loaded.data.watched(original));
        loaded.save().unwrap();
        assert_eq!(
            Store::load_from(settings_path).resume_position(original),
            0.0
        );
    }

    #[test]
    fn transfer_preferences_default_and_roundtrip_with_legacy_settings() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.json");
        fs::write(
            &path,
            br#"{"volume":42,"recent":[{"path":"old.mp4","position":30,"duration":100}]}"#,
        )
        .unwrap();
        let mut store = Store::load_from(&path);
        assert_eq!(store.data.transfers.seed_completed, None);
        assert_eq!(store.data.transfers.download_bps, 0);
        assert_eq!(store.data.transfers.upload_bps, 0);
        store.data.transfers.seed_completed = Some(false);
        store.data.transfers.download_bps = 1_048_576;
        store.data.transfers.upload_bps = 262_144;
        store.save().unwrap();
        let loaded = Store::load_from(path);
        assert_eq!(loaded.data.transfers.seed_completed, Some(false));
        assert_eq!(loaded.data.transfers.download_bps, 1_048_576);
        assert_eq!(loaded.data.transfers.upload_bps, 262_144);
        assert_eq!(loaded.resume_position(Path::new("old.mp4")), 30.0);
        assert_eq!(loaded.data.volume, 42.0);
    }

    #[test]
    fn missing_settings_use_defaults_and_roundtrip_replaces_existing_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("nested/settings.json");
        let mut store = Store::load_from(&path);
        assert_eq!(store.data.volume, 80.0);
        assert_eq!(store.data.speed, 1.0);
        assert!(!store.data.muted);
        store.remember(Path::new("video.mkv"), 24.0, 120.0);
        store.save().unwrap();
        store.data.volume = 42.0;
        store.data.muted = true;
        store.data.speed = 1.5;
        store.save().unwrap();

        let loaded = Store::load_from(&path);
        assert_eq!(loaded.data.volume, 42.0);
        assert!(loaded.data.muted);
        assert_eq!(loaded.data.speed, 1.5);
        assert_eq!(loaded.resume_position(Path::new("video.mkv")), 24.0);
        assert_eq!(loaded.data.recent[0].title, "video.mkv");
    }

    #[test]
    fn corrupt_settings_load_defaults_without_overwriting_the_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.json");
        fs::write(&path, b"{broken json").unwrap();
        let store = Store::load_from(&path);
        assert_eq!(store.data.volume, 80.0);
        assert!(store.data.recent.is_empty());
        assert_eq!(fs::read(&path).unwrap(), b"{broken json");
    }

    #[test]
    fn history_and_library_keep_older_videos_and_deduplicate() {
        let mut settings = Settings::default();
        for index in 0..25 {
            settings.remember(Path::new(&format!("{index}.mkv")), 20.0, 120.0);
        }
        settings.remember(Path::new("10.mkv"), 45.0, 120.0);
        assert_eq!(settings.recent.len(), 25);
        assert_eq!(settings.library.len(), 25);
        assert_eq!(settings.recent[0].path, Path::new("10.mkv"));
        assert_eq!(settings.recent[1].path, Path::new("24.mkv"));
        assert_eq!(settings.resume_position(Path::new("10.mkv")), 45.0);
        assert_eq!(
            settings
                .recent
                .iter()
                .filter(|file| file.path == Path::new("10.mkv"))
                .count(),
            1
        );
        assert!(
            settings
                .recent
                .iter()
                .any(|file| file.path == Path::new("0.mkv"))
        );
    }

    #[test]
    fn batch_import_and_history_migration_preserve_order_and_existing_titles() {
        let mut settings = Settings::default();
        settings.library.push(LibraryVideo {
            path: "existing.mp4".into(),
            title: "Existing title".into(),
            ..Default::default()
        });
        settings.add_videos([
            "existing.mp4".into(),
            "new.mp4".into(),
            "new.mp4".into(),
            PathBuf::new(),
        ]);
        settings.recent.push(RecentFile {
            path: "older.mp4".into(),
            ..Default::default()
        });
        settings.sanitize();
        settings.sanitize();
        assert_eq!(settings.library.len(), 3);
        assert_eq!(settings.library[0].title, "Existing title");
        assert_eq!(settings.library[1].path, Path::new("new.mp4"));
        assert_eq!(settings.library[2].path, Path::new("older.mp4"));
    }

    #[test]
    fn old_settings_migrate_history_into_library_and_keep_unwatched_videos() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.json");
        fs::write(
            &path,
            br#"{"recent":[{"path":"old.mp4","position":42,"duration":120}]}"#,
        )
        .unwrap();
        let mut store = Store::load_from(&path);
        assert_eq!(store.data.library[0].path, Path::new("old.mp4"));
        store.data.add_video(Path::new("unwatched.mkv"));
        store.save().unwrap();
        let loaded = Store::load_from(path);
        assert_eq!(loaded.data.library.len(), 2);
        assert_eq!(loaded.data.recent.len(), 1);
        assert_eq!(loaded.resume_position(Path::new("old.mp4")), 42.);
    }

    #[test]
    fn continue_watching_dismissal_and_watched_marks() {
        let mut settings = Settings::default();
        settings.remember(Path::new("a.mkv"), 30.0, 120.0);
        settings.remember(Path::new("b.mkv"), 40.0, 120.0);
        settings.add_video(Path::new("c.mkv"));
        let continuing = |s: &Settings| {
            s.continue_watching()
                .map(|f| f.path.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            continuing(&settings),
            [PathBuf::from("b.mkv"), "a.mkv".into()]
        );
        settings.dismiss_from_continue(Path::new("b.mkv"));
        assert_eq!(continuing(&settings), [PathBuf::from("a.mkv")]);
        // Dismissal keeps the resume position, and playing again restores it.
        assert_eq!(settings.resume_position(Path::new("b.mkv")), 40.0);
        settings.remember(Path::new("b.mkv"), 41.0, 120.0);
        assert_eq!(continuing(&settings)[0], Path::new("b.mkv"));

        // Opening or sampling a video does not count as watching it.
        assert!(!settings.watched(Path::new("b.mkv")));
        settings.remember(Path::new("short.mkv"), 0.0, 8.0);
        assert!(!settings.watched(Path::new("short.mkv")));
        // Reaching the end does, and watching again keeps it.
        settings.remember(Path::new("b.mkv"), 115.0, 120.0);
        assert!(settings.watched(Path::new("b.mkv")));
        settings.remember(Path::new("b.mkv"), 30.0, 120.0);
        assert!(settings.watched(Path::new("b.mkv")));

        settings.mark_watched(Path::new("a.mkv"));
        assert_eq!(settings.resume_position(Path::new("a.mkv")), 0.0);
        assert!(settings.watched(Path::new("a.mkv")));
        settings.mark_watched(Path::new("c.mkv"));
        assert!(settings.watched(Path::new("c.mkv")));
        settings.mark_unwatched(Path::new("c.mkv"));
        assert!(!settings.watched(Path::new("c.mkv")));
        assert!(
            settings
                .library
                .iter()
                .any(|v| v.path == Path::new("c.mkv"))
        );
    }

    #[test]
    fn resolution_labels_cover_landscape_and_portrait() {
        let video = |width, height| LibraryVideo {
            width,
            height,
            ..Default::default()
        };
        assert_eq!(video(3840, 2160).resolution(), Some("4K"));
        assert_eq!(video(1920, 800).resolution(), Some("1080p"));
        assert_eq!(video(1080, 1920).resolution(), Some("1080p"));
        assert_eq!(video(1024, 436).resolution(), Some("SD"));
        assert_eq!(video(0, 0).resolution(), None);
        // Unknown details are not written to settings.
        let json = serde_json::to_string(&video(0, 0)).unwrap();
        assert!(!json.contains("poster") && !json.contains("duration"));
    }

    #[test]
    fn library_sorts_by_addition_title_and_last_watched() {
        let mut settings = Settings::default();
        for name in ["b.mkv", "C.mkv", "a.mkv"] {
            settings.add_video(Path::new(name));
        }
        settings.remember(Path::new("C.mkv"), 1.0, 10.0);
        settings.remember(Path::new("b.mkv"), 1.0, 10.0);
        let order = |sort| {
            settings
                .sorted_library(sort)
                .into_iter()
                .map(|v| v.title.as_str())
                .collect::<Vec<_>>()
        };
        assert_eq!(order(Sort::Added), ["a.mkv", "C.mkv", "b.mkv"]);
        assert_eq!(order(Sort::Title), ["a.mkv", "b.mkv", "C.mkv"]);
        assert_eq!(order(Sort::Watched), ["b.mkv", "C.mkv", "a.mkv"]);
    }

    #[test]
    fn resume_ignores_the_start_unknown_duration_and_near_end() {
        let mut settings = Settings::default();
        let path = Path::new("video.mkv");
        for (position, duration, expected) in [
            (5.0, 100.0, 0.0),
            (5.1, 100.0, 5.1),
            (90.0, 100.0, 90.0),
            (90.1, 100.0, 0.0),
            (100.0, 100.0, 0.0),
            (25.0, 0.0, 0.0),
            (f64::NAN, 100.0, 0.0),
        ] {
            settings.remember(path, position, duration);
            assert_eq!(settings.resume_position(path), expected);
        }
    }

    #[test]
    fn save_and_load_sanitize_settings_and_history() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.json");
        let mut store = Store::load_from(&path);
        store.data.volume = f64::NAN;
        store.data.speed = f64::INFINITY;
        store.data.recent = vec![
            RecentFile {
                path: "video.mkv".into(),
                title: String::new(),
                position: 200.0,
                duration: 100.0,
                ..Default::default()
            },
            RecentFile {
                path: "video.mkv".into(),
                title: "duplicate".into(),
                position: 25.0,
                duration: 100.0,
                ..Default::default()
            },
            RecentFile::default(),
        ];
        store.save().unwrap();
        let loaded = Store::load_from(&path);
        assert_eq!(loaded.data.volume, 80.0);
        assert_eq!(loaded.data.speed, 1.0);
        assert_eq!(loaded.data.recent.len(), 1);
        assert_eq!(loaded.data.recent[0].position, 100.0);
        assert_eq!(loaded.data.recent[0].title, "video.mkv");

        fs::write(&path, br#"{"volume":200,"speed":-2}"#).unwrap();
        let loaded = Store::load_from(&path);
        assert_eq!(loaded.data.volume, 100.0);
        assert_eq!(loaded.data.speed, 0.25);
        assert!(!loaded.data.muted);
    }

    #[test]
    fn failed_replacement_preserves_the_destination_and_cleans_temporary_files() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("settings.json");
        fs::create_dir(&destination).unwrap();
        let marker = destination.join("keep");
        fs::write(&marker, "prior data").unwrap();
        let store = Store::load_from(directory.path().join("missing.json"));
        assert!(store.save_to(&destination).is_err());
        assert_eq!(fs::read_to_string(marker).unwrap(), "prior data");
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
    }
}
