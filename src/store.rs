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

#[path = "store/index.rs"]
mod index;
#[path = "store/persistence.rs"]
mod persistence;
#[path = "store/torrent_integrity.rs"]
mod torrent_integrity;
pub use persistence::{SaveBarrier, SaveStatus};
pub use torrent_integrity::UnverifiedTorrentFile;

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
    /// Durable removal markers. Automatic history/transfer updates respect these.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub removed_videos: Vec<RemovedVideo>,
    /// Unverified files retain their safety gate after transfer ownership ends.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub unverified_torrent_files: Vec<UnverifiedTorrentFile>,
    #[serde(skip)]
    cache: index::Cache,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RemovedVideo {
    pub path: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub torrent: Option<TorrentIdentity>,
    /// False after an explicit identity restore; keep the binding for this path.
    #[serde(default = "default_true")]
    pub block_torrent: bool,
}

fn default_true() -> bool {
    true
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

#[derive(Clone, Debug, PartialEq, Eq, Hash, Deserialize, Serialize)]
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
            removed_videos: Vec::new(),
            unverified_torrent_files: Vec::new(),
            cache: index::Cache::default(),
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
    /// Learned only after positive filesystem presence; does not affect indexes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location: Option<flick::library::FileLocation>,
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
    pub fn remember_location(
        &mut self,
        path: &Path,
        location: flick::library::FileLocation,
    ) -> bool {
        let Some(index) = self.index().library.get(path).copied() else {
            return false;
        };
        if self.library[index].location.as_ref() == Some(&location) {
            return false;
        }
        self.library[index].location = Some(location);
        true
    }

    /// The OpenSubtitles key from `FLICK_OPENSUBTITLES_KEY`, or settings.
    pub fn subtitle_key(&self) -> String {
        std::env::var("FLICK_OPENSUBTITLES_KEY")
            .ok()
            .filter(|key| !key.trim().is_empty())
            .unwrap_or_else(|| self.opensubtitles_key.trim().to_owned())
    }

    pub fn add_video(&mut self, path: &Path) {
        if !path.as_os_str().is_empty()
            && !self.is_removed(path)
            && !self.index().library.contains_key(path)
        {
            self.library.push(LibraryVideo::new(path.to_owned()));
            self.invalidate();
        }
    }

    pub fn add_videos(&mut self, paths: impl IntoIterator<Item = PathBuf>) {
        let mut known = self
            .index()
            .library
            .keys()
            .cloned()
            .collect::<std::collections::HashSet<_>>();
        let paths = paths
            .into_iter()
            .filter(|path| {
                !path.as_os_str().is_empty() && !self.is_removed(path) && known.insert(path.clone())
            })
            .collect::<Vec<_>>();
        if !paths.is_empty() {
            self.library
                .extend(paths.into_iter().map(LibraryVideo::new));
            self.invalidate();
        }
    }
    /// Exact paths are deduplicated without accessing or canonicalizing media files.
    pub fn remember(&mut self, path: &Path, position: f64, duration: f64) {
        if path.as_os_str().is_empty() || self.is_removed(path) {
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
        self.invalidate_history();
    }

    /// Bind verified media without discarding path-based history or old paths.
    pub fn bind_torrent(&mut self, path: &Path, infohash: &str, file_index: usize) {
        if infohash.len() != 40 || !infohash.bytes().all(|b| b.is_ascii_hexdigit()) {
            return;
        }
        if path.as_os_str().is_empty()
            || self.is_removed(path)
            || self.is_torrent_removed(infohash, file_index)
        {
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
        self.invalidate_history();
    }

    /// Videos with a saved position that are not dismissed, newest first.
    pub fn continue_watching(&self) -> impl Iterator<Item = &RecentFile> {
        let index = self.index();
        self.recent
            .iter()
            .filter(move |file| !file.dismissed && index.resume(self, &file.path) > 0.0)
    }

    pub fn dismiss_from_continue(&mut self, path: &Path) {
        if let Some(file) = self.recent.iter_mut().find(|file| file.path == path) {
            file.dismissed = true;
            self.invalidate_history();
        }
    }

    /// Records the video as finished, keeping its place in history.
    pub fn mark_watched(&mut self, path: &Path) {
        if path.as_os_str().is_empty() || self.is_removed(path) {
            return;
        }
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
        self.invalidate_history();
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
        self.invalidate_history();
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
        self.invalidate_history();
    }

    /// Whether the video was played to the end or marked watched. Torrent
    /// media follows its torrent's record, like the resume position.
    pub fn watched(&self, path: &Path) -> bool {
        let index = self.index();
        if self.is_removed(path) {
            return false;
        }
        if let Some(saved) = index
            .library
            .get(path)
            .and_then(|i| self.library[*i].torrent.as_ref())
            .and_then(|identity| index.torrent.get(identity))
            .map(|i| &self.torrent_history[*i])
        {
            return saved.finished;
        }
        index.finished.contains(path)
    }

    /// Library videos in the chosen order. Ties keep legacy reverse-addition order.
    pub fn sorted_library(&self, sort: Sort) -> Vec<&LibraryVideo> {
        self.sorted_indices(sort)
            .iter()
            .map(|i| &self.library[*i])
            .collect()
    }

    pub fn resume_position(&self, path: &Path) -> f64 {
        self.index().resume(self, path)
    }

    pub fn is_removed(&self, path: &Path) -> bool {
        self.index().is_removed(self, path)
    }

    pub fn is_torrent_removed(&self, infohash: &str, file_index: usize) -> bool {
        self.index().removed_torrents.contains(&TorrentIdentity {
            infohash: infohash.to_ascii_lowercase(),
            file_index,
        })
    }

    /// Remove all known copies of a torrent identity coherently, or one local path.
    /// Markers survive saves/restarts and block late history and completion updates.
    /// This never accesses or deletes a media file.
    #[cfg_attr(not(test), allow(dead_code))] // Single-path compatibility API used by diagnostics.
    pub fn remove_video(&mut self, path: &Path) -> bool {
        self.remove_videos(std::iter::once(path))
    }

    /// Remove a filesystem batch with one membership/index invalidation.
    pub fn remove_videos<'a>(&mut self, paths: impl IntoIterator<Item = &'a Path>) -> bool {
        use std::collections::{HashMap, HashSet};
        let requested: HashSet<PathBuf> = paths
            .into_iter()
            .filter(|p| !p.as_os_str().is_empty())
            .map(Path::to_owned)
            .collect();
        if requested.is_empty() {
            return false;
        }
        let mut history = HashMap::new();
        for entry in &self.torrent_history {
            history.entry(&entry.path).or_insert(&entry.identity);
        }
        let mut removed = HashMap::new();
        for entry in &self.removed_videos {
            removed.entry(&entry.path).or_insert(entry.torrent.as_ref());
        }
        let mut affected = HashMap::new();
        let mut identities = HashSet::new();
        for path in &requested {
            let identity = self
                .index()
                .library
                .get(path)
                .and_then(|index| self.library[*index].torrent.as_ref())
                .or_else(|| history.get(path).copied())
                .or_else(|| removed.get(path).copied().flatten())
                .cloned();
            if let Some(identity) = &identity {
                identities.insert(identity.clone());
            }
            affected.insert(path.clone(), identity);
        }
        for video in &self.library {
            if video
                .torrent
                .as_ref()
                .is_some_and(|identity| identities.contains(identity))
            {
                affected.insert(video.path.clone(), video.torrent.clone());
            }
        }
        for saved in &self.torrent_history {
            if identities.contains(&saved.identity) {
                affected.insert(saved.path.clone(), Some(saved.identity.clone()));
            }
        }
        let changed = affected.keys().any(|path| !self.is_removed(path))
            || self
                .library
                .iter()
                .any(|video| affected.contains_key(&video.path))
            || self
                .recent
                .iter()
                .any(|video| affected.contains_key(&video.path));
        self.library
            .retain(|video| !affected.contains_key(&video.path));
        self.recent
            .retain(|video| !affected.contains_key(&video.path));
        self.torrent_history.retain(|saved| {
            !affected.contains_key(&saved.path) && !identities.contains(&saved.identity)
        });
        for marker in &mut self.removed_videos {
            if let Some(Some(identity)) = affected.remove(&marker.path) {
                marker.torrent = Some(identity);
                marker.block_torrent = true;
            }
        }
        self.removed_videos
            .extend(affected.into_iter().map(|(path, torrent)| RemovedVideo {
                path,
                torrent,
                block_torrent: true,
            }));
        self.invalidate();
        changed
    }

    /// Explicit open/import only. Automatic additions must use `add_video` instead.
    /// Restoring a known removed path also restores its torrent identity, while other
    /// removed paths keep their individual path markers.
    pub fn restore_video(&mut self, path: &Path) {
        if path.as_os_str().is_empty() {
            return;
        }
        let identity = self
            .removed_videos
            .iter()
            .find(|v| v.path == path)
            .and_then(|v| v.torrent.clone());
        self.removed_videos.retain(|v| v.path != path);
        if let Some(identity) = &identity {
            for marker in &mut self.removed_videos {
                if marker.torrent.as_ref() == Some(identity) {
                    marker.block_torrent = false;
                }
            }
        }
        self.invalidate();
        self.add_video(path);
        if let Some(identity) = identity {
            self.bind_torrent(path, &identity.infohash, identity.file_index);
        }
    }

    pub fn restore_videos(&mut self, paths: impl IntoIterator<Item = PathBuf>) {
        let paths = paths.into_iter().collect::<Vec<_>>();
        let selected = paths.iter().collect::<std::collections::HashSet<_>>();
        let restored = self
            .removed_videos
            .iter()
            .filter(|m| selected.contains(&m.path))
            .filter_map(|m| m.torrent.clone().map(|identity| (m.path.clone(), identity)))
            .collect::<Vec<_>>();
        let identities = restored
            .iter()
            .map(|(_, id)| id.clone())
            .collect::<std::collections::HashSet<_>>();
        self.removed_videos.retain(|m| !selected.contains(&m.path));
        for marker in &mut self.removed_videos {
            if marker
                .torrent
                .as_ref()
                .is_some_and(|id| identities.contains(id))
            {
                marker.block_torrent = false;
            }
        }
        self.invalidate();
        self.add_videos(paths);
        for (path, identity) in restored {
            self.bind_torrent(&path, &identity.infohash, identity.file_index);
        }
    }

    /// Explicitly open a different copy of a removed torrent file.
    pub fn restore_torrent_video(&mut self, path: &Path, infohash: &str, file_index: usize) {
        if path.as_os_str().is_empty()
            || infohash.len() != 40
            || !infohash.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return;
        }
        let identity = TorrentIdentity {
            infohash: infohash.to_ascii_lowercase(),
            file_index,
        };
        for marker in &mut self.removed_videos {
            if marker.torrent.as_ref() == Some(&identity) {
                marker.block_torrent = false;
            }
        }
        self.invalidate();
        self.restore_video(path);
        self.bind_torrent(path, infohash, file_index);
    }

    fn sanitize(&mut self) {
        self.invalidate();
        self.sanitize_torrent_quarantine();
        let valid_identity = |identity: &mut TorrentIdentity| {
            identity.infohash = identity.infohash.to_ascii_lowercase();
            identity.infohash.len() == 40
                && identity.infohash.bytes().all(|b| b.is_ascii_hexdigit())
        };
        let mut removed_paths = std::collections::HashSet::new();
        self.removed_videos.retain_mut(|marker| {
            if marker
                .torrent
                .as_mut()
                .is_some_and(|id| !valid_identity(id))
            {
                marker.torrent = None;
            }
            !marker.path.as_os_str().is_empty() && removed_paths.insert(marker.path.clone())
        });
        let removed_torrents = self
            .removed_videos
            .iter()
            .filter(|m| m.block_torrent)
            .filter_map(|m| m.torrent.clone())
            .collect::<std::collections::HashSet<_>>();
        for video in &mut self.library {
            if video.torrent.as_mut().is_some_and(|id| !valid_identity(id)) {
                video.torrent = None;
            }
            if video
                .torrent
                .as_ref()
                .is_some_and(|id| removed_torrents.contains(id))
            {
                removed_paths.insert(video.path.clone());
            }
        }
        for saved in &mut self.torrent_history {
            saved.identity.infohash = saved.identity.infohash.to_ascii_lowercase();
            if removed_torrents.contains(&saved.identity) {
                removed_paths.insert(saved.path.clone());
            }
        }
        // Persist newly discovered removed copies too, before dropping their binding.
        let mut marker_paths = self
            .removed_videos
            .iter()
            .map(|m| m.path.clone())
            .collect::<std::collections::HashSet<_>>();
        for video in &self.library {
            if removed_paths.contains(&video.path) && marker_paths.insert(video.path.clone()) {
                self.removed_videos.push(RemovedVideo {
                    path: video.path.clone(),
                    torrent: video.torrent.clone(),
                    block_torrent: true,
                });
            }
        }
        for saved in &self.torrent_history {
            if removed_paths.contains(&saved.path) && marker_paths.insert(saved.path.clone()) {
                self.removed_videos.push(RemovedVideo {
                    path: saved.path.clone(),
                    torrent: Some(saved.identity.clone()),
                    block_torrent: true,
                });
            }
        }
        let mut identities = std::collections::HashSet::new();
        self.torrent_history.retain_mut(|saved| {
            saved.identity.infohash = saved.identity.infohash.to_ascii_lowercase();
            saved.duration = non_negative_finite(saved.duration);
            saved.position = non_negative_finite(saved.position).min(saved.duration);
            saved.finished |= reached_end(saved.position, saved.duration);
            saved.identity.infohash.len() == 40
                && saved
                    .identity
                    .infohash
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit())
                && !removed_paths.contains(&saved.path)
                && !removed_torrents.contains(&saved.identity)
                && identities.insert((saved.identity.infohash.clone(), saved.identity.file_index))
        });
        self.shortcuts.sanitize();
        self.appearance.sanitize();
        self.volume = finite_or(self.volume, 80.0).clamp(0.0, 100.0);
        self.speed = finite_or(self.speed, 1.0).clamp(0.25, 4.0);
        let mut seen = std::collections::HashSet::new();
        self.recent.retain_mut(|file| {
            file.sanitize();
            !file.path.as_os_str().is_empty()
                && !removed_paths.contains(&file.path)
                && seen.insert(file.path.clone())
        });
        let mut seen_library = std::collections::HashSet::new();
        self.library.retain_mut(|video| {
            if video.title.trim().is_empty() {
                video.title = file_title(&video.path);
            }
            !video.path.as_os_str().is_empty()
                && !removed_paths.contains(&video.path)
                && seen_library.insert(video.path.clone())
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
        self.invalidate();
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
        // Also covers history saved before the flag existed.
        self.finished |= reached_end(self.position, self.duration);
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
    writer: persistence::Writer,
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
                    writer: persistence::Writer::new(None),
                }
            }
        }
    }

    /// A failed read never modifies the source file and falls back to defaults.
    pub fn load_from(path: impl AsRef<Path>) -> Self {
        let path = path.as_ref().to_owned();
        let mut initial = None;
        let mut data = match fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<Settings>(&bytes) {
                Ok(data) => {
                    initial = Some((path.clone(), bytes));
                    data
                }
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
            writer: persistence::Writer::new(initial),
        }
    }

    #[cfg_attr(not(test), allow(dead_code))] // Synchronous validation/diagnostic API.
    pub fn save(&self) -> Result<()> {
        let path = self
            .path
            .as_ref()
            .context("No settings directory is available")?;
        self.save_to(path)
    }

    /// Synchronous compatibility API, ordered with background saves.
    #[cfg_attr(not(test), allow(dead_code))] // Synchronous validation/diagnostic API.
    pub fn save_to(&self, path: impl AsRef<Path>) -> Result<()> {
        self.writer
            .save(path.as_ref().to_owned(), self.data.clone())
    }

    /// Clone an in-memory snapshot only; all serialization and I/O is background work.
    /// A newer queued snapshot replaces older pending snapshots for this store.
    pub fn queue_save(&self) -> Result<u64> {
        let path = self
            .path
            .as_ref()
            .context("No settings directory is available")?;
        self.writer.queue(path.clone(), self.data.clone())
    }

    /// Wait for saves queued before this call and surface a durable-write failure.
    #[cfg_attr(not(test), allow(dead_code))] // Synchronous validation/diagnostic API.
    pub fn flush(&self) -> Result<()> {
        self.writer.flush()
    }

    /// Capture the admitted generation; waiting performs no work on the UI thread.
    pub fn save_barrier(&self) -> SaveBarrier {
        self.writer.barrier()
    }

    /// Drain pending saves and join the worker. Future saves return an error.
    pub fn shutdown(&self) -> Result<()> {
        self.writer.shutdown()
    }

    pub fn take_save_error(&self) -> Option<String> {
        self.writer.take_error()
    }

    #[cfg_attr(not(test), allow(dead_code))] // Synchronous validation/diagnostic API.
    pub fn save_status(&self) -> SaveStatus {
        self.writer.status()
    }

    pub fn remember(&mut self, path: &Path, position: f64, duration: f64) {
        self.data.remember(path, position, duration);
    }

    pub fn resume_position(&self, path: &Path) -> f64 {
        self.data.resume_position(path)
    }
}

fn serialized(mut data: Settings) -> Result<Vec<u8>> {
    data.sanitize();
    let mut bytes = serde_json::to_vec_pretty(&data).context("Cannot serialize settings")?;
    bytes.push(b'\n');
    Ok(bytes)
}

/// Durable replacement: the destination is never removed before a successful swap.
fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    atomic_write_using(path, bytes, replace)
}

fn atomic_write_using(
    path: &Path,
    bytes: &[u8],
    replace_file: impl FnOnce(&Path, &Path) -> std::io::Result<()>,
) -> Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)
        .with_context(|| format!("Cannot create settings directory {}", parent.display()))?;
    let (temporary, mut file) = TemporaryFile::create(parent)?;
    file.write_all(bytes)
        .context("Cannot write temporary settings")?;
    file.sync_all().context("Cannot sync temporary settings")?;
    drop(file);
    replace_file(&temporary.path, path)
        .with_context(|| format!("Cannot replace settings at {}", path.display()))?;
    #[cfg(unix)]
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .with_context(|| format!("Cannot sync settings directory {}", parent.display()))?;
    Ok(())
}

#[cfg(not(windows))]
fn replace(from: &Path, to: &Path) -> std::io::Result<()> {
    fs::rename(from, to)
}

#[cfg(windows)]
fn replace(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn MoveFileExW(from: *const u16, to: *const u16, flags: u32) -> i32;
    }
    let from = from
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let to = to
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    // Replace existing destination and flush the move before reporting success.
    if unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), 0x1 | 0x8) } == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
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
    fn batched_removal_invalidates_once_and_keeps_unrelated_entries() {
        let mut data = Settings::default();
        let paths = (0..10_000)
            .map(|i| PathBuf::from(format!("/generated/video-{i}.mp4")))
            .collect::<Vec<_>>();
        data.restore_videos(paths.clone());
        let revision = data.revision();
        assert!(data.remove_videos(paths.iter().take(1000).map(PathBuf::as_path)));
        assert_eq!(data.revision(), revision.wrapping_add(1));
        assert_eq!(data.library.len(), 9000);
        assert_eq!(data.removed_videos.len(), 1000);
        for path in paths.iter().take(1000) {
            assert!(data.is_removed(path));
        }
        for path in paths.iter().skip(1000) {
            assert!(data.video(path).is_some());
        }
        data.sanitize();
        assert_eq!(data.library.len(), 9000);
    }

    #[test]
    fn positive_file_location_roundtrips_without_changing_query_indexes() {
        let directory = tempfile::tempdir().unwrap();
        let path = Path::new("generated-location.mp4");
        let settings_path = directory.path().join("settings.json");
        let mut store = Store::load_from(&settings_path);
        store.data.restore_video(path);
        let title = store.data.normalized_title(path).unwrap().to_owned();
        let revision = store.data.revision();
        let location = flick::library::FileLocation {
            parent: "unix:1:42".into(),
            volume: "generated mounted volume".into(),
        };
        assert!(store.data.remember_location(path, location.clone()));
        assert!(!store.data.remember_location(path, location.clone()));
        assert_eq!(store.data.revision(), revision);
        assert_eq!(store.data.normalized_title(path), Some(title.as_str()));
        store.queue_save().unwrap();
        store.flush().unwrap();
        let loaded = Store::load_from(&settings_path);
        assert_eq!(
            loaded.data.video(path).unwrap().location.as_ref(),
            Some(&location)
        );
    }

    #[test]
    fn indexed_lookup_preserves_legacy_duplicates_identity_sort_and_direct_invalidation() {
        let mut settings = Settings::default();
        let path = Path::new("Movie.mkv");
        settings.add_video(path);
        settings.recent = vec![
            RecentFile {
                path: path.into(),
                position: 20.0,
                duration: 100.0,
                ..Default::default()
            },
            RecentFile {
                path: path.into(),
                position: 99.0,
                duration: 100.0,
                finished: true,
                ..Default::default()
            },
        ];
        settings.invalidate();
        // Resume chooses the first duplicate; watched historically accepted any.
        assert_eq!(settings.resume_position(path), 20.0);
        assert!(settings.watched(path));
        assert_eq!(settings.normalized_title(path), Some("movie.mkv"));
        let revision = settings.revision();
        settings.library[0].title = "Edited TITLE".into();
        settings.recent[0].position = 40.0;
        settings.recent[0].title = "Recent TITLE".into();
        settings.invalidate();
        assert!(settings.revision() > revision);
        assert_eq!(settings.normalized_title(path), Some("edited title"));
        assert_eq!(settings.normalized_recent_title(path), Some("recent title"));
        assert_eq!(settings.resume_position(path), 40.0);
        // Build each cache, then mutate through all history APIs.
        settings.sorted_library(Sort::Watched);
        settings.mark_unwatched(path);
        assert!(!settings.watched(path));
        assert_eq!(settings.resume_position(path), 0.0);
        settings.mark_watched(path);
        assert!(settings.watched(path));
        settings.remember(path, 30.0, 100.0);
        assert!(settings.watched(path));
        assert_eq!(settings.continue_watching().count(), 1);
        settings.dismiss_from_continue(path);
        assert_eq!(settings.continue_watching().count(), 0);
    }

    #[test]
    fn cached_shows_adjacency_and_ties_preserve_library_semantics() {
        let mut settings = Settings::default();
        settings.add_videos(
            [
                "Show.S01E01.mkv",
                "show.S01E02.720p.mkv",
                "Show.S01E02.1080p.mkv",
                "Show.S01E03.mkv",
            ]
            .map(PathBuf::from),
        );
        let expected =
            flick::library::group_shows(settings.library.iter().map(|v| v.path.as_path()));
        assert_eq!(settings.shows(), expected);
        let address = settings.shows().as_ptr();
        assert_eq!(settings.shows().as_ptr(), address);
        settings.remember(Path::new("Show.S01E01.mkv"), 40.0, 200.0);
        assert_eq!(settings.shows().as_ptr(), address);
        let revision = settings.revision();
        settings.library[0].poster = Some("poster.png".into());
        settings.library[0].duration = 200.0;
        settings.library[0].width = 1920;
        settings.library[0].height = 1080;
        settings.invalidate_metadata();
        assert!(settings.revision() > revision);
        assert_eq!(settings.shows().as_ptr(), address);
        assert_eq!(
            settings
                .video(Path::new("Show.S01E01.mkv"))
                .unwrap()
                .poster
                .as_deref(),
            Some(Path::new("poster.png"))
        );
        for path in ["show.S01E02.720p.mkv", "Show.S01E02.1080p.mkv"] {
            for step in [-1, 1] {
                assert_eq!(
                    settings.adjacent_episode(Path::new(path), step),
                    flick::library::adjacent_episode(
                        settings.library.iter().map(|v| v.path.as_path()),
                        Path::new(path),
                        step
                    )
                );
            }
        }
        settings.library[0].title = "Same".into();
        settings.library[1].title = "same".into();
        settings.invalidate();
        let sorted = settings.sorted_library(Sort::Title);
        let ties = sorted
            .iter()
            .filter(|v| v.title.eq_ignore_ascii_case("same"))
            .map(|v| v.path.clone())
            .collect::<Vec<_>>();
        assert_eq!(
            ties,
            [
                PathBuf::from("show.S01E02.720p.mkv"),
                "Show.S01E01.mkv".into()
            ]
        );
        settings.remove_video(Path::new("Show.S01E03.mkv"));
        assert_eq!(settings.shows()[0].episodes.len(), 3);
        assert_eq!(
            settings.adjacent_episode(Path::new("show.S01E02.720p.mkv"), 1),
            None
        );
    }

    #[test]
    fn durable_removal_blocks_history_sanitize_and_automatic_readds_until_explicit_restore() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.json");
        let video = Path::new("removed.mkv");
        let mut store = Store::load_from(&path);
        store.remember(video, 32.0, 200.0);
        store.data.sorted_library(Sort::Title);
        assert!(store.data.remove_video(video));
        assert!(!store.data.remove_video(video));
        store.remember(video, 60.0, 200.0);
        store.data.add_video(video);
        store.data.add_videos([video.to_owned()]);
        store.data.mark_watched(video);
        assert!(store.data.library.is_empty());
        assert!(store.data.recent.is_empty());
        // Simulate a legacy/stale externally supplied history snapshot.
        store.data.recent.push(RecentFile {
            path: video.into(),
            position: 50.0,
            duration: 200.0,
            ..Default::default()
        });
        store.data.invalidate();
        assert_eq!(store.data.continue_watching().count(), 0);
        store.queue_save().unwrap();
        store.flush().unwrap();
        let mut loaded = Store::load_from(path);
        assert!(loaded.data.is_removed(video));
        assert!(loaded.data.library.is_empty());
        assert!(loaded.data.recent.is_empty());
        loaded.data.restore_video(video);
        loaded.remember(video, 70.0, 200.0);
        assert!(!loaded.data.is_removed(video));
        assert_eq!(loaded.resume_position(video), 70.0);
        loaded.save().unwrap();
        assert_eq!(
            Store::load_from(directory.path().join("settings.json")).resume_position(video),
            70.0
        );
    }

    #[test]
    fn removal_blocks_late_torrent_identity_at_new_paths_but_allows_explicit_restore() {
        let mut settings = Settings::default();
        let hash = "0123456789abcdef0123456789abcdef01234567";
        let first = Path::new("first.mkv");
        let second = Path::new("second.mkv");
        let late = Path::new("late.mkv");
        settings.bind_torrent(first, hash, 2);
        settings.bind_torrent(second, hash, 2);
        settings.remember(first, 40.0, 200.0);
        assert_eq!(settings.resume_position(second), 40.0);
        settings.remove_video(first);
        assert!(settings.library.is_empty());
        assert!(settings.torrent_history.is_empty());
        assert!(settings.is_removed(second));
        settings.bind_torrent(late, &hash.to_uppercase(), 2);
        assert!(settings.video(late).is_none());
        assert!(settings.is_torrent_removed(hash, 2));
        // A stale snapshot can contain an alternate identity path only in history.
        settings.torrent_history.push(TorrentResume {
            identity: TorrentIdentity {
                infohash: hash.to_uppercase(),
                file_index: 2,
            },
            path: "stale-history.mkv".into(),
            position: 40.0,
            duration: 200.0,
            finished: false,
        });
        settings.recent.push(RecentFile {
            path: "stale-history.mkv".into(),
            position: 40.0,
            duration: 200.0,
            ..Default::default()
        });
        settings.invalidate();
        settings.sanitize();
        assert!(settings.is_removed(Path::new("stale-history.mkv")));
        assert!(settings.recent.is_empty());
        assert!(!settings.is_torrent_removed(hash, 3));
        settings.bind_torrent(Path::new("different-index.mkv"), hash, 3);
        assert_eq!(settings.library.len(), 1);
        settings.restore_torrent_video(late, hash, 2);
        assert!(!settings.is_torrent_removed(hash, 2));
        assert!(settings.is_removed(first));
        assert!(settings.is_removed(second));
        settings.remember(late, 80.0, 200.0);
        assert_eq!(settings.resume_position(late), 80.0);
        settings.restore_video(second);
        assert_eq!(settings.resume_position(second), 80.0);
    }

    #[test]
    fn batch_restore_keeps_import_order_and_torrent_identity_across_restart() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.json");
        let hash = "0123456789abcdef0123456789abcdef01234567";
        let mut store = Store::load_from(&path);
        store.data.bind_torrent(Path::new("second.mkv"), hash, 2);
        store.data.bind_torrent(Path::new("third.mkv"), hash, 2);
        store.data.remove_video(Path::new("second.mkv"));
        store
            .data
            .restore_videos(["first.mkv", "second.mkv", "last.mkv"].map(PathBuf::from));
        assert_eq!(
            store
                .data
                .library
                .iter()
                .map(|v| v.path.as_path())
                .collect::<Vec<_>>(),
            [
                Path::new("first.mkv"),
                Path::new("second.mkv"),
                Path::new("last.mkv")
            ]
        );
        store.remember(Path::new("second.mkv"), 50.0, 200.0);
        store.save().unwrap();
        let mut loaded = Store::load_from(path);
        assert!(loaded.data.is_removed(Path::new("third.mkv")));
        assert!(!loaded.data.is_torrent_removed(hash, 2));
        loaded.data.restore_video(Path::new("third.mkv"));
        assert_eq!(loaded.resume_position(Path::new("third.mkv")), 50.0);
    }

    #[test]
    fn cached_lookups_and_derived_views_cover_large_generated_libraries() {
        for count in [100_usize, 1_000, 5_000, 10_000] {
            let mut settings = Settings::default();
            for i in 0..count {
                let path = PathBuf::from(format!("Show{:05}.S01E{:03}.mkv", i / 20, i % 20));
                settings.library.push(LibraryVideo {
                    path: path.clone(),
                    title: format!("Video {i:06}"),
                    ..Default::default()
                });
                settings.recent.push(RecentFile {
                    path,
                    position: if i % 3 == 0 { 99.0 } else { 20.0 },
                    duration: 100.0,
                    finished: i % 3 == 0,
                    ..Default::default()
                });
            }
            settings.invalidate();
            for (i, video) in settings.library.iter().enumerate() {
                assert_eq!(settings.video(&video.path).unwrap().title, video.title);
                assert_eq!(settings.watched(&video.path), i % 3 == 0);
                assert_eq!(
                    settings.resume_position(&video.path),
                    if i % 3 == 0 { 0.0 } else { 20.0 }
                );
            }
            assert_eq!(
                settings.sorted_library(Sort::Title).first().unwrap().title,
                "Video 000000"
            );
            assert_eq!(
                settings.sorted_library(Sort::Watched).first().unwrap().path,
                settings.recent[0].path
            );
            assert_eq!(
                settings.continue_watching().count(),
                count - count.div_ceil(3)
            );
            assert_eq!(settings.shows().len(), count.div_ceil(20));
        }
    }

    #[test]
    fn replacement_failure_preserves_existing_file_and_removes_partial_temp() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.json");
        fs::write(&path, "previous durable settings").unwrap();
        let error = atomic_write_using(&path, b"new settings", |temporary, destination| {
            assert_eq!(fs::read(destination).unwrap(), b"previous durable settings");
            assert_eq!(fs::read(temporary).unwrap(), b"new settings");
            Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "injected replacement refusal",
            ))
        })
        .unwrap_err();
        assert!(format!("{error:#}").contains("injected replacement refusal"));
        assert_eq!(fs::read(path).unwrap(), b"previous durable settings");
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    #[test]
    fn background_writer_flush_dedup_errors_and_shutdown() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.json");
        let mut store = Store::load_from(&path);
        store.data.volume = 33.0;
        let generation = store.queue_save().unwrap();
        store.flush().unwrap();
        assert_eq!(Store::load_from(&path).data.volume, 33.0);
        assert_eq!(store.save_status().completed_generation, generation);
        let writes = store.save_status().writes;
        store.queue_save().unwrap();
        store.flush().unwrap();
        assert_eq!(store.save_status().writes, writes);
        assert_eq!(store.save_status().unchanged, 1);
        store.data.volume = 55.0;
        store.queue_save().unwrap();
        store.shutdown().unwrap();
        assert_eq!(Store::load_from(&path).data.volume, 55.0);
        assert!(
            store
                .queue_save()
                .unwrap_err()
                .to_string()
                .contains("shut down")
        );
        store.shutdown().unwrap();

        let parent = directory.path().join("blocked");
        fs::write(&parent, "keep").unwrap();
        let broken = Store::load_from(parent.join("settings.json"));
        broken.queue_save().unwrap();
        assert!(broken.flush().is_err());
        let error = broken.take_save_error().unwrap();
        assert!(
            error.contains("generation 1") && error.contains("Cannot create settings directory")
        );
        assert!(broken.take_save_error().is_none());
        assert!(broken.flush().is_err());
        // A successful export must not hide a failed settings-path save.
        broken
            .save_to(directory.path().join("export.json"))
            .unwrap();
        assert!(broken.flush().is_err());
        assert_eq!(fs::read(&parent).unwrap(), b"keep");
        fs::remove_file(&parent).unwrap();
        broken.queue_save().unwrap();
        broken.flush().unwrap();
        assert_eq!(
            Store::load_from(parent.join("settings.json")).data.volume,
            80.0
        );
    }

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
        // History saved before the flag existed counts if it reached the end.
        let mut old: Settings = serde_json::from_str(
            r#"{"recent":[{"path":"done.mkv","position":118,"duration":120},
                          {"path":"half.mkv","position":60,"duration":120}]}"#,
        )
        .unwrap();
        old.sanitize();
        assert!(old.watched(Path::new("done.mkv")));
        assert!(!old.watched(Path::new("half.mkv")));
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
