use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

pub fn is_video(path: &Path) -> bool {
    path.extension().and_then(|v| v.to_str()).is_some_and(|v| {
        matches!(
            v.to_ascii_lowercase().as_str(),
            "mp4"
                | "mkv"
                | "mov"
                | "m4v"
                | "avi"
                | "webm"
                | "mpeg"
                | "mpg"
                | "ts"
                | "m2ts"
                | "mts"
                | "wmv"
                | "flv"
                | "ogv"
                | "3gp"
                | "vob"
        )
    })
}

/// Explicitly chosen folders only. Skip symlinks to avoid recursion cycles.
pub fn scan_folder(root: &Path) -> Result<Vec<PathBuf>> {
    let mut pending = vec![root.to_owned()];
    let mut videos = Vec::new();
    while let Some(folder) = pending.pop() {
        for entry in
            std::fs::read_dir(&folder).with_context(|| format!("Read {}", folder.display()))?
        {
            let entry = entry?;
            let kind = entry.file_type()?;
            if kind.is_dir() {
                pending.push(entry.path());
            } else if kind.is_file() && is_video(&entry.path()) {
                videos.push(entry.path().canonicalize()?);
            }
        }
    }
    videos.sort();
    Ok(videos)
}

pub fn format_bytes(bytes: u64) -> String {
    let value = bytes as f64;
    if bytes >= 1024 * 1024 * 1024 {
        format!("{:.1} GB", value / (1024_f64.powi(3)))
    } else if bytes >= 1024 * 1024 {
        format!("{:.1} MB", value / (1024_f64.powi(2)))
    } else {
        format!("{:.0} KB", value / 1024.)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn explicit_folder_scan_finds_nested_videos_but_not_other_files() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("sub")).unwrap();
        for path in ["film.MKV", "sub/other.mp4", "sub/notes.txt"] {
            std::fs::write(root.path().join(path), []).unwrap();
        }
        let videos = scan_folder(root.path()).unwrap();
        assert_eq!(videos.len(), 2);
        assert!(videos.iter().all(|p| p.is_absolute() && is_video(p)));
    }
}
