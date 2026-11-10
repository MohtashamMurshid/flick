//! What the server serves: the desktop app's saved library, read-only,
//! folders scanned for videos, or a list of files the server downloaded.
//!
//! The server rebuilds this list on every `GET /v1/library` (and when a
//! request names an id it does not know yet, at most once a second). That is
//! a JSON parse or a directory walk, cheap next to the client's own work,
//! and it means a video added on the desktop appears without a restart.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

/// Where the served videos come from.
#[derive(Clone, Debug)]
pub enum Sources {
    /// The desktop app's `settings.json`, read-only.
    SettingsFile(PathBuf),
    /// Folders scanned for video files.
    Folders(Vec<PathBuf>),
    /// A JSON array of video paths, kept by the server itself (the headless
    /// server's `remote-downloads.json`). A missing file is an empty list.
    PathList(PathBuf),
    /// Several sources, in order; a path listed twice is served once.
    Several(Vec<Sources>),
}

/// The torrent file behind an entry, so an unfinished one can be streamed.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TorrentFile {
    /// 40 lowercase hex characters.
    pub infohash: String,
    pub index: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    /// Canonical.
    pub path: PathBuf,
    pub title: String,
    /// A poster image that exists on disk.
    pub poster: Option<PathBuf>,
    pub duration: f64,
    pub width: u32,
    pub height: u32,
    /// False for a torrent file that has not been verified yet.
    pub complete: bool,
    /// Verified fraction while incomplete; 1 when complete.
    pub progress: f64,
    /// The torrent file this entry is, when known.
    pub torrent: Option<TorrentFile>,
    /// The file's full size when the transfer knows it and the file may not
    /// be on disk yet; otherwise the size is read from disk.
    pub size: Option<u64>,
}

impl Entry {
    /// A video that is a complete file on disk.
    pub fn file(path: PathBuf, title: String) -> Self {
        Self {
            path,
            title,
            poster: None,
            duration: 0.0,
            width: 0,
            height: 0,
            complete: true,
            progress: 1.0,
            torrent: None,
            size: None,
        }
    }

    /// Whether the entry's path names a file the server may read: a complete
    /// file, or a partial one on disk. An unfinished torrent file that only
    /// the transfer knows has a relative, display-only path.
    pub fn on_disk(&self) -> bool {
        self.path.is_absolute()
    }
}

/// The videos to serve, in a stable order, without duplicates or missing
/// files. `poster_cache` is the desktop's thumbnail folder, used to find a
/// poster when the source does not name one.
pub async fn load(sources: &Sources, poster_cache: Option<&Path>) -> Result<Vec<Entry>> {
    let sources = sources.clone();
    let poster_cache = poster_cache.map(Path::to_owned);
    tokio::task::spawn_blocking(move || load_blocking(&sources, poster_cache.as_deref()))
        .await
        .context("The library scan stopped")?
}

pub fn load_blocking(sources: &Sources, poster_cache: Option<&Path>) -> Result<Vec<Entry>> {
    let mut entries = from_sources(sources)?;
    let mut seen = HashSet::new();
    entries.retain(|entry| seen.insert(entry.path.clone()));
    for entry in &mut entries {
        entry.poster = entry
            .poster
            .take()
            .filter(|poster| poster.is_file())
            .or_else(|| {
                poster_cache
                    .and_then(|cache| poster_path(cache, &entry.path))
                    .filter(|poster| poster.is_file())
            });
    }
    Ok(entries)
}

fn from_sources(sources: &Sources) -> Result<Vec<Entry>> {
    Ok(match sources {
        Sources::SettingsFile(path) => from_settings(path)?,
        Sources::Folders(folders) => from_folders(folders)?,
        Sources::PathList(path) => from_path_list(path)?,
        Sources::Several(sources) => {
            let mut entries = Vec::new();
            for source in sources {
                entries.extend(from_sources(source)?);
            }
            entries
        }
    })
}

/// Reads a path list written by [`save_path_list`]. Files that are gone or
/// are not videos are skipped.
fn from_path_list(path: &Path) -> Result<Vec<Entry>> {
    Ok(read_path_list(path)?
        .into_iter()
        .filter(|path| crate::library::is_video(path))
        .filter_map(|path| path.canonicalize().ok().filter(|p| p.is_file()))
        .map(|path| Entry::file(path.clone(), file_title(&path)))
        .collect())
}

/// The paths in a path list file; a missing file is an empty list.
pub fn read_path_list(path: &Path) -> Result<Vec<PathBuf>> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .with_context(|| format!("Parse the download list in {}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error).with_context(|| format!("Read {}", path.display())),
    }
}

/// Replaces a path list file atomically.
pub fn save_path_list(path: &Path, paths: &[PathBuf]) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(paths)?;
    crate::remote::auth::write_private(path, &bytes)
}

/// The subset of the desktop's `settings.json` the server reads.
#[derive(Default, Deserialize)]
#[serde(default)]
struct SavedSettings {
    library: Vec<SavedVideo>,
    unverified_torrent_files: Vec<SavedUnverified>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct SavedVideo {
    path: PathBuf,
    title: String,
    poster: Option<PathBuf>,
    duration: f64,
    width: u32,
    height: u32,
    torrent: Option<SavedTorrent>,
}

#[derive(Clone, Default, Deserialize, PartialEq, Eq, Hash)]
#[serde(default)]
struct SavedTorrent {
    infohash: String,
    file_index: usize,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct SavedUnverified {
    path: PathBuf,
    identity: Option<SavedTorrent>,
}

fn from_settings(path: &Path) -> Result<Vec<Entry>> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("Read the Flick library in {}", path.display()))?;
    let settings: SavedSettings = serde_json::from_str(&text)
        .with_context(|| format!("Parse the Flick library in {}", path.display()))?;
    let unverified_paths: HashSet<PathBuf> = settings
        .unverified_torrent_files
        .iter()
        .flat_map(|file| [file.path.clone(), canonical_or_same(&file.path)])
        .collect();
    let unverified_identities: HashSet<(String, usize)> = settings
        .unverified_torrent_files
        .iter()
        .filter_map(|file| file.identity.as_ref())
        .map(|id| (id.infohash.to_ascii_lowercase(), id.file_index))
        .collect();
    Ok(settings
        .library
        .into_iter()
        .filter_map(|video| {
            let canonical = video.path.canonicalize().ok().filter(|p| p.is_file())?;
            let unverified = unverified_paths.contains(&video.path)
                || unverified_paths.contains(&canonical)
                || video.torrent.as_ref().is_some_and(|torrent| {
                    unverified_identities
                        .contains(&(torrent.infohash.to_ascii_lowercase(), torrent.file_index))
                });
            Some(Entry {
                title: if video.title.trim().is_empty() {
                    file_title(&canonical)
                } else {
                    video.title
                },
                path: canonical,
                poster: video.poster,
                duration: finite(video.duration),
                width: video.width,
                height: video.height,
                complete: !unverified,
                progress: if unverified { 0.0 } else { 1.0 },
                torrent: video
                    .torrent
                    .filter(|torrent| {
                        crate::remote::ids::is_valid_id(&torrent.infohash.to_ascii_lowercase())
                    })
                    .map(|torrent| TorrentFile {
                        infohash: torrent.infohash.to_ascii_lowercase(),
                        index: torrent.file_index,
                    }),
                size: None,
            })
        })
        .collect())
}

fn from_folders(folders: &[PathBuf]) -> Result<Vec<Entry>> {
    let mut entries = Vec::new();
    for folder in folders {
        for path in crate::library::scan_folder(folder)? {
            entries.push(Entry::file(path.clone(), file_title(&path)));
        }
    }
    Ok(entries)
}

fn canonical_or_same(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_owned())
}

fn finite(value: f64) -> f64 {
    if value.is_finite() && value > 0.0 {
        value
    } else {
        0.0
    }
}

/// The desktop's title for a file: its name.
pub fn file_title(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "Untitled video".to_owned())
}

/// The desktop thumbnail worker's poster file for a video, keyed by path,
/// size, and modification time. Mirrors `thumbnails::poster_path`, which
/// lives in the desktop build.
fn poster_path(cache: &Path, video: &Path) -> Option<PathBuf> {
    let metadata = std::fs::metadata(video).ok()?;
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |time| time.as_nanos());
    let key = format!(
        "{}\u{0}{}\u{0}{modified}",
        video.to_string_lossy(),
        metadata.len()
    );
    let hash = key.bytes().fold(0xcbf29ce484222325u64, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
    });
    Some(cache.join(format!("{hash:016x}.png")))
}

/// Subtitle files next to a video: same stem, optionally followed by a
/// `.language` part, with a text subtitle extension. Sorted by file name;
/// the index in this list is the index in URLs.
pub fn sidecar_subtitles(video: &Path) -> Vec<Sidecar> {
    // A relative path is a display name for a file not on disk yet; looking
    // beside it would read the server's working folder.
    if !video.is_absolute() {
        return Vec::new();
    }
    let (Some(dir), Some(stem)) = (video.parent(), video.file_stem().and_then(|s| s.to_str()))
    else {
        return Vec::new();
    };
    let Ok(read) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut found: Vec<Sidecar> = read
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
        .filter_map(|entry| {
            let path = entry.path();
            let extension = path.extension()?.to_str()?.to_ascii_lowercase();
            if !SUBTITLE_EXTENSIONS.contains(&extension.as_str()) {
                return None;
            }
            let sub_stem = path.file_stem()?.to_str()?;
            let language = if sub_stem == stem {
                None
            } else {
                let suffix = sub_stem.strip_prefix(stem)?.strip_prefix('.')?;
                Some(language_from_suffix(suffix))
            };
            Some(Sidecar {
                file_name: path.file_name()?.to_string_lossy().into_owned(),
                language: language.flatten(),
                extension,
                path,
            })
        })
        .collect();
    found.sort_by(|a, b| a.file_name.cmp(&b.file_name));
    found
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sidecar {
    pub path: PathBuf,
    pub file_name: String,
    pub extension: String,
    pub language: Option<String>,
}

pub const SUBTITLE_EXTENSIONS: [&str; 5] = ["srt", "ass", "ssa", "vtt", "sub"];

/// `Movie.en.srt` → `en`, `Movie.pt-BR.forced.srt` → `pt-br`. The language
/// is the last dotted part that looks like a language tag; descriptive parts
/// such as `forced` or `sdh` are skipped.
fn language_from_suffix(suffix: &str) -> Option<String> {
    suffix
        .split('.')
        .rev()
        .find(|part| {
            let (primary, region) = part.split_once(['-', '_']).unwrap_or((part, ""));
            (2..=3).contains(&primary.len())
                && primary.bytes().all(|b| b.is_ascii_alphabetic())
                && !matches!(primary.to_ascii_lowercase().as_str(), "sdh" | "cc")
                && region.len() <= 4
                && region.bytes().all(|b| b.is_ascii_alphanumeric())
        })
        .map(|part| part.replace('_', "-").to_ascii_lowercase())
}

/// The `Content-Type` of a video file by extension.
pub fn video_mime(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("mp4" | "m4v") => "video/mp4",
        Some("mkv") => "video/x-matroska",
        Some("webm") => "video/webm",
        Some("mov") => "video/quicktime",
        Some("avi") => "video/x-msvideo",
        Some("ts" | "m2ts" | "mts") => "video/mp2t",
        Some("mpeg" | "mpg" | "vob") => "video/mpeg",
        Some("wmv") => "video/x-ms-wmv",
        Some("flv") => "video/x-flv",
        Some("ogv") => "video/ogg",
        Some("3gp") => "video/3gpp",
        _ => "application/octet-stream",
    }
}

/// The `Content-Type` of a subtitle file by extension.
pub fn subtitle_mime(extension: &str) -> &'static str {
    match extension {
        "srt" => "application/x-subrip",
        "vtt" => "text/vtt",
        "ass" | "ssa" => "text/x-ssa",
        _ => "text/plain",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_library_is_read_with_unverified_files_marked() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let done = dir.path().join("Done.mkv");
        let partial = dir.path().join("Partial.mkv");
        let by_identity = dir.path().join("Identity.mkv");
        for file in [&done, &partial, &by_identity] {
            std::fs::write(file, b"video")?;
        }
        let settings = dir.path().join("settings.json");
        std::fs::write(
            &settings,
            serde_json::json!({
                "volume": 1.0,
                "appearance": {"anything": true},
                "library": [
                    {"path": done, "title": "Done", "duration": 12.5, "width": 1920, "height": 1080},
                    {"path": partial, "title": "", "torrent": {"infohash": "a".repeat(40), "file_index": 0}},
                    {"path": by_identity, "title": "Id", "torrent": {"infohash": "B".repeat(40), "file_index": 3}},
                    {"path": dir.path().join("Gone.mkv"), "title": "Gone"},
                    {"path": done, "title": "Duplicate"}
                ],
                "unverified_torrent_files": [
                    {"path": partial, "identity": {"infohash": "a".repeat(40), "file_index": 0}},
                    {"path": dir.path().join("elsewhere.mkv"), "identity": {"infohash": "b".repeat(40), "file_index": 3}}
                ]
            })
            .to_string(),
        )?;
        let entries = load_blocking(&Sources::SettingsFile(settings), None)?;
        let titles: Vec<_> = entries.iter().map(|e| e.title.as_str()).collect();
        assert_eq!(titles, ["Done", "Partial.mkv", "Id"]);
        assert!(entries[0].complete);
        assert_eq!(entries[0].duration, 12.5);
        assert_eq!((entries[0].width, entries[0].height), (1920, 1080));
        assert!(!entries[1].complete);
        assert!(!entries[2].complete);
        assert_eq!(entries[0].path, done.canonicalize()?);
        Ok(())
    }

    #[test]
    fn folders_are_scanned_and_sidecars_found() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let show = dir.path().join("Show");
        std::fs::create_dir(&show)?;
        let video = show.join("Show.S01E02.mkv");
        std::fs::write(&video, b"video")?;
        std::fs::write(show.join("Show.S01E02.srt"), b"1")?;
        std::fs::write(show.join("Show.S01E02.en.srt"), b"1")?;
        std::fs::write(show.join("Show.S01E02.pt-BR.forced.ass"), b"1")?;
        std::fs::write(show.join("Show.S01E02.png"), b"1")?;
        std::fs::write(show.join("Show.S01E021.srt"), b"1")?;
        std::fs::write(show.join("notes.txt"), b"1")?;
        let entries = load_blocking(&Sources::Folders(vec![dir.path().to_owned()]), None)?;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].title, "Show.S01E02.mkv");
        let subs = sidecar_subtitles(&entries[0].path);
        let names: Vec<_> = subs
            .iter()
            .map(|s| (s.file_name.as_str(), s.language.as_deref()))
            .collect();
        assert_eq!(
            names,
            [
                ("Show.S01E02.en.srt", Some("en")),
                ("Show.S01E02.pt-BR.forced.ass", Some("pt-br")),
                ("Show.S01E02.srt", None),
            ]
        );
        Ok(())
    }

    #[test]
    fn posters_come_from_the_cache_when_present() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let video = dir.path().join("Film.mp4");
        std::fs::write(&video, b"video")?;
        let cache = dir.path().join("thumbnails");
        std::fs::create_dir(&cache)?;
        let sources = Sources::Folders(vec![dir.path().to_owned()]);
        assert_eq!(load_blocking(&sources, Some(&cache))?[0].poster, None);
        let poster = poster_path(&cache, &video.canonicalize()?).unwrap();
        std::fs::write(&poster, b"png")?;
        assert_eq!(
            load_blocking(&sources, Some(&cache))?[0].poster,
            Some(poster)
        );
        Ok(())
    }

    #[test]
    fn mime_types_follow_extensions() {
        assert_eq!(video_mime(Path::new("a.MKV")), "video/x-matroska");
        assert_eq!(video_mime(Path::new("a.mp4")), "video/mp4");
        assert_eq!(video_mime(Path::new("a")), "application/octet-stream");
        assert_eq!(subtitle_mime("vtt"), "text/vtt");
    }
}
