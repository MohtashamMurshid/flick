//! Size caps for what the server keeps in its state folder: the extracted
//! WebVTT cache and anything an earlier run left in the transcode folder.

use std::{
    io,
    path::{Path, PathBuf},
    time::SystemTime,
};

/// The most the WebVTT cache, and the transcode folder's leftovers, may hold.
pub const CACHE_LIMIT_BYTES: u64 = 256 * 1024 * 1024;

/// What [`prune`] did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Pruned {
    pub removed: usize,
    pub kept_bytes: u64,
}

/// Removes the oldest entries of `dir`, by modification time, until what is
/// left takes at most `max_bytes`. A folder inside counts as one entry of
/// its total size and is removed whole. Symbolic links are left alone and
/// count nothing, and `keep` names entries that are never removed (they
/// still count). A missing folder is empty.
pub fn prune(dir: &Path, max_bytes: u64, keep: impl Fn(&Path) -> bool) -> io::Result<Pruned> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Pruned::default()),
        Err(error) => return Err(error),
    };
    let mut found: Vec<(SystemTime, u64, PathBuf, bool)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(metadata) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        let modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        if metadata.is_file() {
            found.push((modified, metadata.len(), path, false));
        } else if metadata.is_dir() {
            found.push((modified, folder_size(&path), path, true));
        }
    }
    let mut total: u64 = found.iter().map(|(_, size, _, _)| *size).sum();
    found.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.2.cmp(&b.2)));
    let mut removed = 0;
    for (_, size, path, is_dir) in found {
        if total <= max_bytes {
            break;
        }
        if keep(&path) {
            continue;
        }
        let result = if is_dir {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
        match result {
            Ok(()) => {
                total -= size;
                removed += 1;
            }
            // Another prune got there first.
            Err(error) if error.kind() == io::ErrorKind::NotFound => total -= size,
            Err(error) => {
                tracing::warn!(path = %path.display(), "Could not remove a cached file: {error}");
            }
        }
    }
    Ok(Pruned {
        removed,
        kept_bytes: total,
    })
}

/// Bytes of regular files under `dir`, not following symbolic links.
fn folder_size(dir: &Path) -> u64 {
    let mut total = 0;
    let mut pending = vec![dir.to_owned()];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(metadata) = std::fs::symlink_metadata(entry.path()) else {
                continue;
            };
            if metadata.is_file() {
                total += metadata.len();
            } else if metadata.is_dir() {
                pending.push(entry.path());
            }
        }
    }
    total
}
