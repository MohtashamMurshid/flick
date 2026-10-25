use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use super::{Settings, TorrentIdentity};

/// An exact destination that must not be read directly until it is verified.
/// Removal/restore controls membership; this independent gate controls bytes.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct UnverifiedTorrentFile {
    pub path: PathBuf,
    pub identity: TorrentIdentity,
}

fn identity(infohash: &str, file_index: usize) -> Option<TorrentIdentity> {
    (infohash.len() == 40 && infohash.bytes().all(|b| b.is_ascii_hexdigit())).then(|| {
        TorrentIdentity {
            infohash: infohash.to_ascii_lowercase(),
            file_index,
        }
    })
}

impl Settings {
    /// Capture before releasing an owner, including every unverified sibling.
    /// Open/import/restore never remove these markers.
    pub fn quarantine_torrent_file(
        &mut self,
        path: &Path,
        infohash: &str,
        file_index: usize,
    ) -> bool {
        let Some(identity) = identity(infohash, file_index) else {
            return false;
        };
        if path.as_os_str().is_empty()
            || self
                .unverified_torrent_files
                .iter()
                .any(|file| file.path == path && file.identity == identity)
        {
            return false;
        }
        self.unverified_torrent_files.push(UnverifiedTorrentFile {
            path: path.to_owned(),
            identity,
        });
        self.invalidate_metadata();
        true
    }

    pub fn unverified_torrent_file(&self, path: &Path) -> Option<&UnverifiedTorrentFile> {
        self.unverified_torrent_files
            .iter()
            .find(|file| file.path == path)
    }

    /// Caller must have CURRENT selected/full verification evidence, outside
    /// Checking/Error. A verified alternate copy does not verify this path.
    pub fn clear_verified_torrent_file(
        &mut self,
        path: &Path,
        infohash: &str,
        file_index: usize,
    ) -> bool {
        let Some(identity) = identity(infohash, file_index) else {
            return false;
        };
        let before = self.unverified_torrent_files.len();
        self.unverified_torrent_files
            .retain(|file| file.path != path || file.identity != identity);
        let changed = self.unverified_torrent_files.len() != before;
        if changed {
            self.invalidate_metadata();
        }
        changed
    }

    /// Explicit engine-accepted Download only. Lift membership removal without
    /// adding partial media or clearing its independent verification quarantine.
    pub fn allow_torrent_restore(
        &mut self,
        path: &Path,
        infohash: &str,
        file_index: usize,
    ) -> bool {
        let Some(identity) = identity(infohash, file_index) else {
            return false;
        };
        if path.as_os_str().is_empty() {
            return false;
        }
        let before = self.removed_videos.len();
        self.removed_videos.retain(|marker| marker.path != path);
        let mut changed = before != self.removed_videos.len();
        for marker in &mut self.removed_videos {
            if marker.torrent.as_ref() == Some(&identity) && marker.block_torrent {
                marker.block_torrent = false;
                changed = true;
            }
        }
        if changed {
            self.invalidate();
        }
        changed
    }

    pub(super) fn sanitize_torrent_quarantine(&mut self) {
        let mut seen = HashSet::new();
        self.unverified_torrent_files.retain_mut(|file| {
            file.identity.infohash = file.identity.infohash.to_ascii_lowercase();
            !file.path.as_os_str().is_empty()
                && identity(&file.identity.infohash, file.identity.file_index).is_some()
                && seen.insert((file.path.clone(), file.identity.clone()))
        });
    }
}

#[cfg(test)]
mod tests {
    use crate::store::Store;

    const HASH: &str = "0123456789abcdef0123456789abcdef01234567";

    #[test]
    fn quarantine_survives_reload_open_import_restore_and_removal() -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        let settings_path = root.path().join("settings.json");
        let video = root.path().join("partial.mp4");
        std::fs::write(&video, vec![0; 4096])?;
        let mut store = Store::load_from(&settings_path);
        store.data.bind_torrent(&video, HASH, 1);
        assert!(
            store
                .data
                .quarantine_torrent_file(&video, &HASH.to_uppercase(), 1)
        );
        assert!(!store.data.quarantine_torrent_file(&video, HASH, 1));
        store.data.remove_video(&video);
        store.data.restore_video(&video);
        store.data.remove_video(&video);
        store.data.restore_videos([video.clone()]);
        store.data.remove_video(&video);
        store.data.restore_torrent_video(&video, HASH, 1);
        store.data.remove_video(&video);
        store.data.allow_torrent_restore(&video, HASH, 1);
        store.data.remember(&video, 20.0, 100.0);
        store.save()?;
        let mut loaded = Store::load_from(&settings_path);
        assert_eq!(loaded.data.unverified_torrent_files.len(), 1);
        assert_eq!(
            loaded
                .data
                .unverified_torrent_file(&video)
                .unwrap()
                .identity
                .infohash,
            HASH
        );
        assert!(!loaded.data.clear_verified_torrent_file(&video, HASH, 2));
        assert!(!loaded.data.clear_verified_torrent_file(
            &root.path().join("verified-other-copy.mp4"),
            HASH,
            1
        ));
        assert!(loaded.data.unverified_torrent_file(&video).is_some());
        assert!(loaded.data.clear_verified_torrent_file(&video, HASH, 1));
        loaded.save()?;
        assert!(
            Store::load_from(settings_path)
                .data
                .unverified_torrent_file(&video)
                .is_none()
        );
        Ok(())
    }

    #[test]
    fn accepted_download_lifts_only_selected_tombstones_without_adding_partial_cards()
    -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        let settings_path = root.path().join("settings.json");
        let old = root.path().join("old.mp4");
        let selected = root.path().join("new.mp4");
        let sibling = root.path().join("sibling.mp4");
        let mut store = Store::load_from(&settings_path);
        store.data.bind_torrent(&old, HASH, 1);
        store.data.bind_torrent(&sibling, HASH, 2);
        store.data.remove_video(&old);
        store.data.remove_video(&sibling);
        // Automatic completion at either old or unknown destinations stays removed.
        for _ in 0..3 {
            store.data.bind_torrent(&old, HASH, 1);
            store.data.bind_torrent(&selected, HASH, 1);
        }
        assert!(store.data.library.is_empty());
        store.data.quarantine_torrent_file(&selected, HASH, 1);
        assert!(store.data.allow_torrent_restore(&selected, HASH, 1));
        assert!(store.data.library.is_empty());
        assert!(store.data.is_removed(&old));
        assert!(store.data.is_torrent_removed(HASH, 2));
        assert!(store.data.unverified_torrent_file(&selected).is_some());
        store.save()?;
        let mut loaded = Store::load_from(settings_path);
        assert!(loaded.data.library.is_empty());
        assert!(!loaded.data.is_torrent_removed(HASH, 1));
        assert!(loaded.data.is_removed(&old));
        loaded.data.clear_verified_torrent_file(&selected, HASH, 1);
        loaded.data.bind_torrent(&selected, HASH, 1);
        assert_eq!(loaded.data.library.len(), 1);
        loaded.data.bind_torrent(&sibling, HASH, 2);
        assert_eq!(loaded.data.library.len(), 1);
        loaded.data.remove_video(&selected);
        loaded.data.bind_torrent(&selected, HASH, 1);
        loaded
            .data
            .bind_torrent(&root.path().join("late.mp4"), HASH, 1);
        assert!(
            loaded.data.library.is_empty(),
            "Later automatic completion overrode removal"
        );
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn releasing_local_transfer_keeps_partial_siblings_quarantined_until_verified()
    -> anyhow::Result<()> {
        use flick::torrent::{TorrentInput, TransferConfig, TransferSession};
        use std::time::Duration;
        let root = tempfile::tempdir()?;
        let config = |name: &str| {
            TransferConfig::local(
                root.path().join(format!("{name}-downloads")),
                root.path().join(format!("{name}-state")),
            )
        };
        let folder = root.path().join("fixture");
        std::fs::create_dir(&folder)?;
        std::fs::write(folder.join("chosen.mp4"), vec![41; 65_537])?;
        std::fs::write(folder.join("sibling.mp4"), vec![42; 65_537])?;
        let seed = TransferSession::start(config("seed")).await?;
        let shared = seed.share(&folder, vec![]).await?;
        let receiver_config = config("receiver");
        let receiver = TransferSession::start(receiver_config.clone()).await?;
        // Local metainfo only; no peer is supplied until the re-download below.
        let resolved = receiver
            .resolve(
                TorrentInput::Bytes(shared.torrent_bytes().to_vec()),
                vec![],
                Duration::from_secs(3),
            )
            .await?;
        let id = receiver.download(&resolved, None).await?;
        receiver.resume(&id, vec![]).await?;
        let snapshot = receiver.snapshot(&id)?;
        assert_eq!(snapshot.files.len(), 2);
        let mut store = Store::load_from(root.path().join("settings.json"));
        for file in &snapshot.files {
            assert!(file.verified_bytes < file.length);
            store
                .data
                .quarantine_torrent_file(&file.path, &id, file.index);
        }
        let sibling = snapshot
            .files
            .iter()
            .find(|file| file.path.ends_with("sibling.mp4"))
            .unwrap();
        // A real partial file outlives ownership and explicit local Open/import.
        std::fs::write(&sibling.path, vec![0; 4096])?;
        store.save()?;
        receiver.release_for_deletion(&id).await?;
        receiver.release_for_deletion(&id).await?;
        assert!(receiver.snapshots()?.is_empty());
        assert!(sibling.path.is_file());
        let mut loaded = Store::load_from(root.path().join("settings.json"));
        loaded.data.restore_video(&sibling.path);
        assert!(loaded.data.unverified_torrent_file(&sibling.path).is_some());
        assert!(receiver.completed_file(&id, sibling.index).is_err());
        // The engine refuses to adopt leftover destinations. Remove only this
        // generated fixture reservation before downloading with an explicit peer.
        std::fs::remove_dir_all(receiver_config.download_dir.join(&id))?;
        receiver.download(&resolved, None).await?;
        receiver
            .resume(&id, vec![seed.listen_addr().unwrap()])
            .await?;
        receiver
            .wait_for_completion(&id, Duration::from_secs(20))
            .await?;
        let verified = receiver.completed_file(&id, sibling.index)?;
        assert_eq!(verified, sibling.path);
        assert!(
            loaded
                .data
                .clear_verified_torrent_file(&verified, &id, sibling.index)
        );
        assert!(loaded.data.unverified_torrent_file(&verified).is_none());
        assert_eq!(std::fs::read(&verified)?, vec![42; 65_537]);
        receiver.stop().await;
        seed.stop().await;
        Ok(())
    }
}
