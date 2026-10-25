//! Cancellable v1 metainfo hashing. All filesystem work and SHA-1 run outside
//! Tokio workers; dropping the caller cancels between bounded read chunks.
use super::{utf8_path, validate_relative_path};
use anyhow::{Context, Result, ensure};
use librqbit::{TorrentMetaV1File, TorrentMetaV1Info};
use serde::{Serialize, Serializer};
use sha1::{Digest, Sha1};
use std::{
    fs::File,
    io::Read,
    path::{Path, PathBuf},
};
use tokio_util::sync::CancellationToken;

const PIECE_LENGTH: usize = 2 * 1024 * 1024;
const READ_CHUNK: usize = 64 * 1024;

#[derive(Default)]
pub(super) struct ByteString(Vec<u8>);

impl AsRef<[u8]> for ByteString {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

impl From<Vec<u8>> for ByteString {
    fn from(value: Vec<u8>) -> Self {
        Self(value)
    }
}

impl Serialize for ByteString {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_bytes(&self.0)
    }
}

pub(super) struct Created {
    pub info: TorrentMetaV1Info<ByteString>,
    pub output_folder: PathBuf,
}

pub(super) async fn create(source: PathBuf, cancel: CancellationToken) -> Result<Created> {
    let guard = cancel.clone().drop_guard();
    let result = tokio::task::spawn_blocking(move || create_blocking(&source, &cancel))
        .await
        .context("Shared file hash worker stopped")?;
    if result.is_ok() {
        guard.disarm();
    }
    result
}

fn check_cancel(cancel: &CancellationToken) -> Result<()> {
    ensure!(!cancel.is_cancelled(), "Sharing cancelled");
    Ok(())
}

fn collect_files(
    path: &Path,
    cancel: &CancellationToken,
    files: &mut Vec<(PathBuf, u64)>,
) -> Result<()> {
    check_cancel(cancel)?;
    utf8_path(path)?;
    let metadata = std::fs::symlink_metadata(path).context("Read shared path")?;
    ensure!(
        !metadata.is_symlink(),
        "Choose original files instead of symbolic links"
    );
    if metadata.is_dir() {
        // Stable file order also fixes piece boundaries across directory walks.
        let mut children = Vec::new();
        for entry in std::fs::read_dir(path).context("Read shared directory")? {
            check_cancel(cancel)?;
            children.push(entry?.path());
        }
        children.sort();
        for child in children {
            collect_files(&child, cancel, files)?;
        }
    } else {
        ensure!(
            metadata.is_file(),
            "Only regular files and folders can be shared"
        );
        files.push((path.to_owned(), metadata.len()));
    }
    Ok(())
}

fn create_blocking(source: &Path, cancel: &CancellationToken) -> Result<Created> {
    #[cfg(test)]
    let probe = tests::take_probe(source);
    check_cancel(cancel)?;
    // Inspect before canonicalizing so a chosen symlink is still refused.
    let mut files = Vec::new();
    collect_files(source, cancel, &mut files)?;
    let source = std::fs::canonicalize(source).context("Resolve shared path")?;
    let directory = source.is_dir();
    let output_folder = if directory {
        source.clone()
    } else {
        source
            .parent()
            .context("Shared file has no parent")?
            .to_owned()
    };
    let name = utf8_path(Path::new(
        source.file_name().context("Shared path has no name")?,
    ))?
    .as_bytes()
    .to_vec()
    .into();
    // Recollect under the canonical root so relative paths have one boundary.
    files.clear();
    collect_files(&source, cancel, &mut files)?;
    let total = files.iter().try_fold(0u64, |sum, (_, len)| {
        sum.checked_add(*len).context("Shared content is too large")
    })?;
    ensure!(total > 0, "Cannot share an empty file or folder");
    let mut output_files = Vec::new();
    let mut pieces = Vec::new();
    let mut checksum = Sha1::new();
    let mut filled = 0;
    let mut buffer = vec![0u8; READ_CHUNK];
    for (path, length) in files {
        check_cancel(cancel)?;
        let metadata = std::fs::symlink_metadata(&path).context("Inspect shared file")?;
        ensure!(
            metadata.is_file() && !metadata.is_symlink(),
            "Shared file was replaced"
        );
        let mut options = File::options();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW);
        }
        let mut file = options
            .open(&path)
            .context("Open shared file for reading")?;
        ensure!(
            file.metadata()?.is_file() && file.metadata()?.len() == length,
            "Shared file changed while hashing"
        );
        let mut remaining = length;
        while remaining > 0 {
            check_cancel(cancel)?;
            let limit = (PIECE_LENGTH - filled)
                .min(READ_CHUNK)
                .min(remaining.min(READ_CHUNK as u64) as usize);
            let size = file
                .read(&mut buffer[..limit])
                .context("Read shared file")?;
            ensure!(size > 0, "Shared file changed while hashing");
            check_cancel(cancel)?;
            checksum.update(&buffer[..size]);
            filled += size;
            remaining -= size as u64;
            if filled == PIECE_LENGTH {
                pieces.extend_from_slice(&checksum.finalize_reset());
                filled = 0;
            }
            #[cfg(test)]
            if let Some(probe) = &probe {
                probe.checkpoint(cancel);
            }
        }
        check_cancel(cancel)?;
        ensure!(
            file.read(&mut buffer[..1])? == 0 && file.metadata()?.len() == length,
            "Shared file changed while hashing"
        );
        if directory {
            let relative = path
                .strip_prefix(&source)
                .context("Shared file escaped its folder")?;
            validate_relative_path(relative)?;
            let path = relative
                .components()
                .map(|component| {
                    Ok(utf8_path(Path::new(component.as_os_str()))?
                        .as_bytes()
                        .to_vec()
                        .into())
                })
                .collect::<Result<Vec<_>>>()?;
            output_files.push(TorrentMetaV1File {
                length,
                path,
                attr: None,
                sha1: None,
                symlink_path: None,
            });
        }
    }
    check_cancel(cancel)?;
    if filled > 0 {
        pieces.extend_from_slice(&checksum.finalize());
    }
    Ok(Created {
        info: TorrentMetaV1Info {
            name: Some(name),
            pieces: pieces.into(),
            piece_length: PIECE_LENGTH as u32,
            length: (!directory).then_some(total),
            files: directory.then_some(output_files),
            ..Default::default()
        },
        output_folder,
    })
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use std::{
        collections::HashMap,
        sync::{
            Arc, Mutex, OnceLock,
            atomic::{AtomicBool, Ordering},
        },
    };

    // Path-scoped probes pause only their generated fixture after a real hash
    // chunk. They cannot stall concurrent tests or leak into production builds.
    #[derive(Default)]
    pub(in crate::torrent) struct Probe {
        pub entered: AtomicBool,
        pub exited: AtomicBool,
    }

    fn probes() -> &'static Mutex<HashMap<PathBuf, Arc<Probe>>> {
        static PROBES: OnceLock<Mutex<HashMap<PathBuf, Arc<Probe>>>> = OnceLock::new();
        PROBES.get_or_init(Mutex::default)
    }

    pub(in crate::torrent) fn pause_at_chunk(source: PathBuf) -> Arc<Probe> {
        let probe = Arc::new(Probe::default());
        assert!(
            probes()
                .lock()
                .unwrap()
                .insert(source, probe.clone())
                .is_none()
        );
        probe
    }

    pub(super) fn take_probe(source: &Path) -> Option<WorkerProbe> {
        probes().lock().unwrap().remove(source).map(WorkerProbe)
    }

    pub(super) struct WorkerProbe(Arc<Probe>);

    impl WorkerProbe {
        pub(super) fn checkpoint(&self, cancel: &CancellationToken) {
            self.0.entered.store(true, Ordering::SeqCst);
            while !cancel.is_cancelled() {
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
        }
    }

    impl Drop for WorkerProbe {
        fn drop(&mut self) {
            self.0.exited.store(true, Ordering::SeqCst);
        }
    }
}
