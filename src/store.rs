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
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            volume: 80.0,
            muted: false,
            speed: 1.0,
            recent: Vec::new(),
            library: Vec::new(),
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
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct LibraryVideo {
    pub path: PathBuf,
    pub title: String,
}

impl Settings {
    pub fn add_video(&mut self, path: &Path) {
        if !path.as_os_str().is_empty() && !self.library.iter().any(|v| v.path == path) {
            self.library.push(LibraryVideo {
                path: path.to_owned(),
                title: file_title(path),
            });
        }
    }
    /// Exact paths are deduplicated without accessing or canonicalizing media files.
    pub fn remember(&mut self, path: &Path, position: f64, duration: f64) {
        if path.as_os_str().is_empty() {
            return;
        }
        self.add_video(path);
        self.recent.retain(|file| file.path != path);
        let mut file = RecentFile {
            path: path.to_owned(),
            title: file_title(path),
            position,
            duration,
        };
        file.sanitize();
        self.recent.insert(0, file);
    }

    pub fn resume_position(&self, path: &Path) -> f64 {
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
        for path in self
            .recent
            .iter()
            .map(|v| v.path.clone())
            .collect::<Vec<_>>()
        {
            self.add_video(&path);
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
            },
            RecentFile {
                path: "video.mkv".into(),
                title: "duplicate".into(),
                position: 25.0,
                duration: 100.0,
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
