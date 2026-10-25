//! Native transfers, independent of GPUI, playback, and viewing history.
//!
//! Call asynchronous operations from a background Tokio runtime. One session
//! owns a locked state directory. `completed_file` gates playback on verification.

use anyhow::{Context, Result, bail, ensure};
use fs2::FileExt;
use librqbit::{
    AddTorrent, AddTorrentOptions, AddTorrentResponse, CreateTorrentOptions, DhtSessionConfig,
    ListenerMode, ListenerOptions, ManagedTorrent, Session, SessionOptions,
    SessionPersistenceConfig, TorrentStatsState, api::TorrentIdOrHash,
    spawn_utils::BlockingSpawner,
};
use serde::Serialize;
use std::{
    collections::HashSet,
    fs::{File, OpenOptions},
    hash::{BuildHasher, Hasher},
    net::{SocketAddr, TcpListener, UdpSocket},
    num::NonZeroU32,
    path::{Component, Path, PathBuf},
    sync::{
        Arc, RwLock,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::io::AsyncReadExt;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

// Torlink src/sources/magnet.ts, revision df059311d0898b7245dd842153c1ff7c892275ee.
// MIT; see THIRD_PARTY_NOTICES.md. Shared by search magnets and public sessions.
pub const PUBLIC_TRACKERS: &[&str] = &[
    "udp://tracker.opentrackr.org:1337/announce",
    "udp://open.demonii.com:1337/announce",
    "udp://tracker.openbittorrent.com:6969/announce",
    "udp://tracker.torrent.eu.org:451/announce",
    "udp://exodus.desync.com:6969/announce",
    "udp://open.stealth.si:80/announce",
    "udp://tracker.dler.org:6969/announce",
    "http://tracker.opentrackr.org:1337/announce",
    "http://tracker.openbittorrent.com:80/announce",
    "http://tracker.dler.org:6969/announce",
    "https://tracker.tamersunion.org:443/announce",
];

const MAX_METADATA_BYTES: u64 = 16 * 1024 * 1024;
const EMPTY_SHA1: [u8; 20] = [
    0xda, 0x39, 0xa3, 0xee, 0x5e, 0x6b, 0x4b, 0x0d, 0x32, 0x55, 0xbf, 0xef, 0x95, 0x60, 0x18, 0x90,
    0xaf, 0xd8, 0x07, 0x09,
];

#[derive(Clone, Debug)]
pub struct TransferConfig {
    pub download_dir: PathBuf,
    pub state_dir: PathBuf,
    pub listen_addr: SocketAddr,
    /// Explicit opt-in to DHT, fallback trackers, and UPnP port mapping.
    pub public_discovery: bool,
    /// Listener and outgoing transports. Local sessions default to TCP.
    pub listener_mode: ListenerMode,
    pub download_limit_bytes_per_second: Option<NonZeroU32>,
    pub upload_limit_bytes_per_second: Option<NonZeroU32>,
    /// Explicit shares keep sharing regardless of the completed-download policy.
    pub seed_completed_downloads: bool,
}

impl TransferConfig {
    pub fn local(download_dir: PathBuf, state_dir: PathBuf) -> Self {
        Self {
            download_dir,
            state_dir,
            listen_addr: ([127, 0, 0, 1], 0).into(),
            public_discovery: false,
            listener_mode: ListenerMode::TcpOnly,
            download_limit_bytes_per_second: None,
            upload_limit_bytes_per_second: None,
            seed_completed_downloads: true,
        }
    }
}

pub enum TorrentInput {
    Magnet(String),
    File(PathBuf),
    Bytes(Vec<u8>),
    Url(String),
}

impl TorrentInput {
    async fn into_add(self) -> Result<AddTorrent<'static>> {
        Ok(match self {
            Self::Url(url) => {
                let url = reqwest::Url::parse(&url).context("Invalid torrent URL")?;
                ensure!(url.scheme() == "https", "Torrent downloads require HTTPS");
                let client = reqwest::Client::builder()
                    .timeout(Duration::from_secs(15))
                    .https_only(true)
                    .build()?;
                let mut response = client.get(url).send().await?.error_for_status()?;
                let mut bytes = Vec::new();
                while let Some(chunk) = response.chunk().await? {
                    ensure!(
                        bytes.len() as u64 + chunk.len() as u64 <= MAX_METADATA_BYTES,
                        "Torrent metadata exceeds 16 MiB"
                    );
                    bytes.extend_from_slice(&chunk);
                }
                AddTorrent::from_bytes(bytes)
            }
            Self::Magnet(value) => {
                let magnet =
                    librqbit::Magnet::parse(value.trim()).context("Invalid magnet or infohash")?;
                ensure!(
                    magnet.as_id20().is_some(),
                    "Only BitTorrent v1/hybrid magnets are supported"
                );
                AddTorrent::from_url(value.trim().to_owned())
            }
            Self::File(path) => {
                ensure!(
                    tokio::fs::metadata(&path)
                        .await
                        .context("Read torrent file metadata")?
                        .is_file(),
                    "Choose a regular torrent file"
                );
                let file = tokio::fs::File::open(&path)
                    .await
                    .context("Open torrent file")?;
                let metadata = file
                    .metadata()
                    .await
                    .context("Read torrent file metadata")?;
                ensure!(metadata.is_file(), "Choose a regular torrent file");
                let size = metadata.len();
                ensure!(
                    size <= MAX_METADATA_BYTES,
                    "Torrent metadata exceeds 16 MiB"
                );
                let mut bytes = Vec::new();
                file.take(MAX_METADATA_BYTES + 1)
                    .read_to_end(&mut bytes)
                    .await
                    .context("Read torrent file")?;
                ensure!(
                    bytes.len() as u64 <= MAX_METADATA_BYTES,
                    "Torrent metadata exceeds 16 MiB"
                );
                AddTorrent::from_bytes(bytes)
            }
            Self::Bytes(bytes) => {
                ensure!(
                    bytes.len() as u64 <= MAX_METADATA_BYTES,
                    "Torrent metadata exceeds 16 MiB"
                );
                AddTorrent::from_bytes(bytes)
            }
        })
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct TorrentFile {
    pub index: usize,
    pub relative_path: PathBuf,
    pub length: u64,
}

/// Metadata resolution does not create media files or register a download.
#[derive(Clone, Debug, Serialize)]
pub struct ResolvedTorrent {
    pub infohash: String,
    pub name: String,
    pub files: Vec<TorrentFile>,
    #[serde(skip)]
    bytes: Vec<u8>,
    #[serde(skip)]
    peers: Vec<SocketAddr>,
}

impl ResolvedTorrent {
    pub fn torrent_bytes(&self) -> &[u8] {
        &self.bytes
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct FileProgress {
    pub index: usize,
    pub path: PathBuf,
    pub length: u64,
    pub verified_bytes: u64,
    pub selected: bool,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TransferState {
    Checking,
    Downloading,
    Paused,
    Seeding,
    Error,
}

#[derive(Clone, Debug, Serialize)]
pub struct TransferSnapshot {
    pub infohash: String,
    pub name: String,
    pub state: TransferState,
    pub verified_bytes: u64,
    pub total_bytes: u64,
    /// Bytes examined during verification, distinct from verified download bytes.
    pub checked_bytes: u64,
    pub checking_total_bytes: u64,
    pub uploaded_bytes: u64,
    pub peers: u32,
    pub download_bytes_per_second: u64,
    pub upload_bytes_per_second: u64,
    pub error: Option<String>,
    pub files: Vec<FileProgress>,
}

#[derive(Clone, Debug, Serialize)]
pub struct SharedTorrent {
    pub infohash: String,
    pub magnet: String,
    #[serde(skip)]
    bytes: Vec<u8>,
}

impl SharedTorrent {
    pub fn torrent_bytes(&self) -> &[u8] {
        &self.bytes
    }
}

pub struct TransferSession {
    session: Arc<Session>,
    download_dir: PathBuf,
    cancel: CancellationToken,
    seed_completed: Arc<AtomicBool>,
    explicit_shares: Arc<RwLock<HashSet<String>>>,
    state_dir: PathBuf,
    _state_lock: File,
}

impl TransferSession {
    pub async fn start(config: TransferConfig) -> Result<Self> {
        tokio::fs::create_dir_all(&config.download_dir)
            .await
            .context("Create download directory")?;
        tokio::fs::create_dir_all(&config.state_dir)
            .await
            .context("Create transfer state directory")?;
        let download_dir = tokio::fs::canonicalize(&config.download_dir).await?;
        let state_dir = tokio::fs::canonicalize(&config.state_dir).await?;
        utf8_path(&download_dir)?;
        utf8_path(&state_dir)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(state_dir.join("session.lock"))
            .context("Open transfer state lock")?;
        lock.try_lock_exclusive()
            .context("Another Flick transfer session owns this state directory")?;
        let explicit_shares = Arc::new(RwLock::new(
            match std::fs::read(state_dir.join("explicit-shares.json")) {
                Ok(bytes) => serde_json::from_slice::<HashSet<String>>(&bytes)
                    .context("Read explicit share identities")?,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => HashSet::new(),
                Err(e) => return Err(e).context("Read explicit share identities"),
            },
        ));
        let mut listen_addr = select_listen_addr(&config, &state_dir)?;
        // A second process can bind between our probe and librqbit's bind.
        // Retry only unusable ports, without masking unrelated startup errors.
        for attempt in 0..16 {
            let cancel = CancellationToken::new();
            let session = Session::new_with_opts(
                download_dir.clone(),
                session_options(&config, &state_dir, listen_addr, cancel.clone())?,
            )
            .await;
            match session {
                Ok(session) => {
                    if config.public_discovery
                        && let Err(error) = persist_listen_port(&state_dir, listen_addr.port())
                    {
                        session.stop().await;
                        return Err(error);
                    }
                    let seed_completed = Arc::new(AtomicBool::new(config.seed_completed_downloads));
                    let policy_session = session.clone();
                    let policy = seed_completed.clone();
                    let policy_cancel = cancel.clone();
                    let policy_shares = explicit_shares.clone();
                    tokio::spawn(async move {
                        loop {
                            tokio::select! {
                                _ = policy_cancel.cancelled() => break,
                                _ = tokio::time::sleep(Duration::from_millis(250)) => {
                                    if !policy.load(Ordering::Relaxed) {
                                        pause_completed_downloads(&policy_session, &policy_shares).await;
                                    }
                                }
                            }
                        }
                    });
                    return Ok(Self {
                        session,
                        download_dir,
                        cancel,
                        seed_completed,
                        explicit_shares,
                        state_dir,
                        _state_lock: lock,
                    });
                }
                Err(error) => {
                    cancel.cancel();
                    let address_unusable = error.chain().any(|cause| {
                        cause
                            .downcast_ref::<std::io::Error>()
                            .is_some_and(port_unusable)
                    });
                    if !config.public_discovery || !address_unusable || attempt == 15 {
                        return Err(error).context("Start transfer session");
                    }
                    listen_addr = available_public_addr(config.listen_addr, config.listener_mode)?;
                }
            }
        }
        unreachable!("bounded startup loop returns on its last attempt")
    }

    pub fn listen_addr(&self) -> Option<SocketAddr> {
        self.session.listen_addr()
    }

    pub async fn resolve(
        &self,
        input: TorrentInput,
        peers: Vec<SocketAddr>,
        deadline: Duration,
    ) -> Result<ResolvedTorrent> {
        let add = input.into_add().await?;
        let result = timeout(
            deadline,
            self.session.add_torrent(
                add,
                Some(AddTorrentOptions {
                    list_only: true,
                    initial_peers: Some(peers.clone()),
                    ..Default::default()
                }),
            ),
        )
        .await
        .context("No peers supplied torrent details before the deadline. Check your connection, try a torrent with active seeders, or open a .torrent file and retry.")??;
        match result {
            AddTorrentResponse::ListOnly(list) => {
                let files = list
                    .info
                    .iter_file_details()
                    .enumerate()
                    .map(|(index, file)| {
                        let relative_path = file.filename.to_pathbuf();
                        validate_relative_path(&relative_path)?;
                        Ok(TorrentFile {
                            index,
                            relative_path,
                            length: file.len,
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
                Ok(ResolvedTorrent {
                    infohash: list.info_hash.as_string(),
                    name: list.info.name_or_else(|| "Unnamed torrent").into_owned(),
                    files,
                    bytes: list.torrent_bytes.to_vec(),
                    peers: list.seen_peers.into_iter().chain(peers).collect(),
                })
            }
            AddTorrentResponse::Added(_, _) | AddTorrentResponse::AlreadyManaged(_, _) => {
                bail!("Metadata-only resolution unexpectedly registered a torrent")
            }
        }
    }

    /// New downloads get an exclusive infohash directory. Never adopt unrelated files.
    pub async fn download(
        &self,
        resolved: &ResolvedTorrent,
        selection: Option<Vec<usize>>,
    ) -> Result<String> {
        self.download_to(resolved, selection, &self.download_dir)
            .await
    }

    pub async fn download_to(
        &self,
        resolved: &ResolvedTorrent,
        selection: Option<Vec<usize>>,
        folder: &Path,
    ) -> Result<String> {
        ensure!(
            librqbit::torrent_from_bytes(&resolved.bytes)?
                .info_hash
                .as_string()
                == resolved.infohash,
            "Torrent identity does not match metadata"
        );
        let selection = validate_selection(selection, resolved.files.len())?;
        if let Ok(existing) = self.handle(&resolved.infohash) {
            ensure!(
                existing.only_files() == selection,
                "Torrent already exists with another file selection"
            );
            return Ok(resolved.infohash.clone());
        }
        tokio::fs::create_dir_all(folder)
            .await
            .context("Create download folder")?;
        let destination = tokio::fs::canonicalize(folder)
            .await?
            .join(&resolved.infohash);
        tokio::fs::create_dir(&destination).await.context("Download destination already exists or cannot be created; use the original transfer state to resume")?;
        let result = self
            .session
            .add_torrent(
                AddTorrent::from_bytes(resolved.bytes.clone()),
                Some(AddTorrentOptions {
                    output_folder: Some(utf8_path(&destination)?.to_owned()),
                    only_files: selection,
                    initial_peers: Some(resolved.peers.clone()),
                    overwrite: false,
                    ..Default::default()
                }),
            )
            .await;
        match result {
            Ok(_) => Ok(resolved.infohash.clone()),
            Err(error) => {
                // Remove only an empty reservation. Never remove partial media.
                let _ = tokio::fs::remove_dir(&destination).await;
                Err(error).context("Start download")
            }
        }
    }

    /// Hash and seed existing files. No metadata files are written into the source.
    pub async fn share(&self, source: &Path, trackers: Vec<String>) -> Result<SharedTorrent> {
        let source = source.to_owned();
        let source = tokio::task::spawn_blocking(move || {
            validate_share_source(&source)?;
            std::fs::canonicalize(source).context("Resolve shared path")
        })
        .await??;
        let created = librqbit::create_torrent(
            &source,
            CreateTorrentOptions {
                trackers: trackers.clone(),
                ..Default::default()
            },
            &BlockingSpawner::new(1),
        )
        .await
        .context("Hash shared files")?;
        let output_folder = created.output_folder.clone();
        let mut info = created.meta.info.data;
        let total = match &info.files {
            Some(files) => files.iter().try_fold(0u64, |sum, file| {
                sum.checked_add(file.length)
                    .context("Shared content is too large")
            })?,
            None => info.length.context("Missing shared file length")?,
        };
        ensure!(total > 0, "Cannot share an empty file or folder");
        let expected_hash_bytes = total.div_ceil(u64::from(info.piece_length)) as usize * 20;
        // librqbit 9.0.1 issue 698: only strip the specific extra empty-input
        // hash. Re-encode the info dictionary so both metadata and magnet use
        // the corrected infohash. Never alter any shared content.
        if total.is_multiple_of(u64::from(info.piece_length))
            && info.pieces.as_ref().len() == expected_hash_bytes + 20
            && info.pieces.as_ref()[expected_hash_bytes..] == EMPTY_SHA1
        {
            info.pieces = info.pieces.as_ref()[..expected_hash_bytes].to_vec().into();
        }
        ensure!(
            info.pieces.as_ref().len() == expected_hash_bytes,
            "Torrent creation returned invalid piece hashes; sharing was not started"
        );
        #[derive(Serialize)]
        struct Envelope<T> {
            #[serde(rename = "announce-list")]
            announce_list: Vec<Vec<String>>,
            info: T,
        }
        let mut bytes = Vec::new();
        librqbit_bencode::bencode_serialize_to_writer(
            &Envelope {
                announce_list: vec![trackers],
                info,
            },
            &mut bytes,
        )?;
        let parsed = librqbit::torrent_from_bytes(&bytes)?;
        let infohash = parsed.info_hash.as_string();
        let magnet = librqbit::Magnet::from_id20(
            parsed.info_hash,
            parsed
                .iter_announce()
                .map(|t| String::from_utf8_lossy(t.as_ref()).into_owned())
                .collect(),
            None,
        )
        .to_string();
        let already_managed = self.handle(&infohash).is_ok();
        ensure!(
            !already_managed,
            "Torrent is already managed by this session"
        );
        let handle = self
            .session
            .add_torrent(
                AddTorrent::from_bytes(bytes.clone()),
                Some(AddTorrentOptions {
                    output_folder: Some(utf8_path(&output_folder)?.to_owned()),
                    overwrite: true,
                    paused: true, // Verify existing files before announcing or downloading.
                    ..Default::default()
                }),
            )
            .await?
            .into_handle()
            .context("Missing shared torrent")?;
        let verification = timeout(Duration::from_secs(120), handle.wait_until_initialized()).await;
        if !matches!(verification, Ok(Ok(()))) || !handle.stats().finished {
            self.session
                .delete(TorrentIdOrHash::Hash(parsed.info_hash), false)
                .await?;
            bail!("Shared files did not verify; sharing was not started");
        }
        self.set_explicit_share(&infohash, true)?;
        self.session.unpause(&handle).await?;
        Ok(SharedTorrent {
            infohash,
            magnet,
            bytes,
        })
    }

    pub fn snapshots(&self) -> Result<Vec<TransferSnapshot>> {
        let handles = self
            .session
            .with_torrents(|items| items.map(|(_, h)| h.clone()).collect::<Vec<_>>());
        handles
            .iter()
            .map(|handle| self.snapshot_handle(handle))
            .collect()
    }

    pub fn snapshot(&self, infohash: &str) -> Result<TransferSnapshot> {
        self.snapshot_handle(&self.handle(infohash)?)
    }

    fn snapshot_handle(&self, handle: &Arc<ManagedTorrent>) -> Result<TransferSnapshot> {
        let stats = handle.stats();
        let state = match stats.state {
            TorrentStatsState::Initializing { .. } => TransferState::Checking,
            TorrentStatsState::Paused => TransferState::Paused,
            TorrentStatsState::Error => TransferState::Error,
            TorrentStatsState::Live if stats.finished => TransferState::Seeding,
            TorrentStatsState::Live => TransferState::Downloading,
        };
        let selected = handle.only_files();
        let files: Vec<FileProgress> = handle
            .with_metadata(|metadata| {
                metadata
                    .file_infos
                    .iter()
                    .enumerate()
                    .map(|(index, file)| FileProgress {
                        index,
                        path: handle.output_folder().join(&file.relative_filename),
                        length: file.len,
                        verified_bytes: stats.file_progress.get(index).copied().unwrap_or(0),
                        selected: selected
                            .as_ref()
                            .is_none_or(|indices| indices.contains(&index)),
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(TransferSnapshot {
            infohash: handle.info_hash().as_string(),
            name: handle.name().unwrap_or_else(|| "Unnamed torrent".into()),
            state,
            verified_bytes: files
                .iter()
                .filter(|f| f.selected)
                .map(|f| f.verified_bytes)
                .sum(),
            total_bytes: files.iter().filter(|f| f.selected).map(|f| f.length).sum(),
            checked_bytes: if state == TransferState::Checking {
                stats.progress_bytes
            } else {
                0
            },
            checking_total_bytes: stats.total_bytes,
            uploaded_bytes: stats.uploaded_bytes,
            peers: stats
                .live
                .as_ref()
                .map_or(0, |s| s.snapshot.peer_stats.live),
            download_bytes_per_second: stats
                .live
                .as_ref()
                .map_or(0, |s| s.download_speed.as_bytes()),
            upload_bytes_per_second: stats.live.as_ref().map_or(0, |s| s.upload_speed.as_bytes()),
            error: stats.error,
            files,
        })
    }

    pub async fn pause(&self, infohash: &str) -> Result<()> {
        let handle = self.handle(infohash)?;
        if handle.is_paused() {
            return Ok(());
        }
        self.session.pause(&handle).await
    }

    /// Explicit peers are supplied again after restart in local-only mode.
    /// The pinned engine does not persist its initial peer list.
    pub async fn resume(&self, infohash: &str, peers: Vec<SocketAddr>) -> Result<()> {
        let handle = self.handle(infohash)?;
        timeout(Duration::from_secs(120), handle.wait_until_initialized())
            .await
            .context("File verification timed out")??;
        if peers.is_empty() {
            if handle.live().is_some() {
                return Ok(());
            }
            return self.session.unpause(&handle).await;
        }
        // Re-register verified existing data with the explicit peer list. This
        // avoids exposing the library's private peer-injection implementation.
        let bytes = handle.with_metadata(|m| m.torrent_bytes.clone())?;
        let output_folder = utf8_path(handle.output_folder())?.to_owned();
        let only_files = handle.only_files();
        self.pause(infohash).await?;
        self.session
            .delete(TorrentIdOrHash::Hash(handle.info_hash()), false)
            .await?;
        self.session
            .add_torrent(
                AddTorrent::from_bytes(bytes),
                Some(AddTorrentOptions {
                    output_folder: Some(output_folder),
                    only_files,
                    overwrite: true,
                    initial_peers: Some(peers),
                    ..Default::default()
                }),
            )
            .await?;
        Ok(())
    }
    /// Only regular metainfo-listed files inside the torrent's output folder
    /// are eligible. Never recursively remove a directory or follow a symlink.
    pub async fn remove(&self, infohash: &str, delete_files: bool) -> Result<()> {
        let handle = self.handle(infohash)?;
        if matches!(handle.stats().state, TorrentStatsState::Initializing { .. }) {
            timeout(Duration::from_secs(120), handle.wait_until_initialized())
                .await
                .context("File verification timed out")??;
        }
        let root = handle.output_folder().to_owned();
        let paths = handle.with_metadata(|m| {
            m.file_infos
                .iter()
                .map(|f| f.relative_filename.clone())
                .collect::<Vec<_>>()
        })?;
        if delete_files {
            for path in &paths {
                validate_delete_path(&root, path)?;
            }
        }
        self.session
            .delete(TorrentIdOrHash::Hash(handle.info_hash()), false)
            .await?;
        self.set_explicit_share(infohash, false)?;
        if delete_files {
            for path in paths {
                validate_delete_path(&root, &path)?;
                match delete_owned_file(&root, &path) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => {
                        return Err(e).context("Transfer removed, but a file could not be deleted");
                    }
                }
            }
        }
        Ok(())
    }

    pub fn torrent_bytes(&self, infohash: &str) -> Result<Vec<u8>> {
        self.handle(infohash)?
            .with_metadata(|m| m.torrent_bytes.to_vec())
    }

    /// The native save picker handles overwrite confirmation. No re-encoding.
    pub async fn export_torrent(&self, infohash: &str, path: &Path) -> Result<()> {
        let target = if path.exists() {
            std::fs::canonicalize(path)?
        } else {
            path.to_owned()
        };
        ensure!(
            !self
                .snapshots()?
                .iter()
                .flat_map(|s| &s.files)
                .any(|f| f.path == target),
            "Cannot export metadata over torrent content"
        );
        tokio::fs::write(path, self.torrent_bytes(infohash)?)
            .await
            .context("Export torrent")
    }

    pub fn set_limits(&self, download: Option<NonZeroU32>, upload: Option<NonZeroU32>) {
        self.session.ratelimits.set_download_bps(download);
        self.session.ratelimits.set_upload_bps(upload);
    }

    pub fn limits(&self) -> librqbit::limits::LimitsConfig {
        self.session.ratelimits.get_config()
    }

    pub async fn set_seed_completed_downloads(&self, enabled: bool) {
        self.seed_completed.store(enabled, Ordering::Relaxed);
        if !enabled {
            pause_completed_downloads(&self.session, &self.explicit_shares).await;
        }
    }

    pub async fn pause_all(&self) -> Result<()> {
        for item in self
            .snapshots()?
            .into_iter()
            .filter(|item| item.state != TransferState::Error)
        {
            let handle = self.handle(&item.infohash)?;
            timeout(Duration::from_secs(120), handle.wait_until_initialized())
                .await
                .context("File verification timed out")??;
            self.pause(&item.infohash).await?;
        }
        Ok(())
    }

    pub async fn resume_all(&self, peers: Vec<SocketAddr>) -> Result<()> {
        for item in self
            .snapshots()?
            .into_iter()
            .filter(|item| item.state != TransferState::Error)
        {
            let handle = self.handle(&item.infohash)?;
            if !self.seed_completed.load(Ordering::Relaxed)
                && is_completed_download(&handle, &self.explicit_shares)
            {
                continue;
            }
            self.resume(&item.infohash, peers.clone()).await?;
        }
        Ok(())
    }
    pub async fn wait_for_completion(&self, infohash: &str, deadline: Duration) -> Result<()> {
        let handle = self.handle(infohash)?;
        // The engine's wait helper requires Live state. A completion policy may
        // already have paused a verified download before this waiter begins.
        timeout(deadline, async {
            loop {
                let stats = handle.stats();
                ensure!(
                    !matches!(stats.state, TorrentStatsState::Error),
                    "Transfer failed: {}",
                    stats.error.unwrap_or_default()
                );
                if stats.finished && !matches!(stats.state, TorrentStatsState::Initializing { .. })
                {
                    return Ok(());
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .context("Download timed out")?
    }

    /// Existence/preallocation is never sufficient to expose a playback path.
    pub fn completed_file(&self, infohash: &str, index: usize) -> Result<PathBuf> {
        let snapshot = self.snapshot(infohash)?;
        ensure!(
            !matches!(
                snapshot.state,
                TransferState::Checking | TransferState::Error
            ),
            "File verification is not complete"
        );
        let file = snapshot
            .files
            .get(index)
            .context("Unknown torrent file index")?;
        ensure!(
            file.selected && file.verified_bytes == file.length,
            "Selected file is not fully verified"
        );
        ensure!(file.path.is_file(), "Completed file is missing from disk");
        Ok(file.path.clone())
    }

    pub async fn stop(self) {
        self.cancel.cancel();
        self.session.stop().await;
    }

    pub async fn shutdown(&self) {
        self.cancel.cancel();
        self.session.stop().await;
    }

    /// Reads one selected file while it downloads. Reads wait for pieces
    /// that are downloaded and verified, so sparse or unverified regions on
    /// disk are never returned, and the engine fetches pieces near the read
    /// position first.
    pub async fn open_stream(&self, infohash: &str, index: usize) -> Result<VerifiedStream> {
        let handle = self.handle(infohash)?;
        timeout(Duration::from_secs(60), handle.wait_until_initialized())
            .await
            .context("File verification timed out")??;
        let snapshot = self.snapshot(infohash)?;
        let file = snapshot
            .files
            .get(index)
            .context("Unknown torrent file index")?;
        ensure!(
            file.selected,
            "Only files chosen for download can be played"
        );
        let inner = handle
            .stream(index)
            .await
            .context("Open the torrent stream")?;
        let len = inner.len();
        Ok(VerifiedStream {
            inner: Box::new(inner),
            len,
        })
    }

    fn set_explicit_share(&self, infohash: &str, explicit: bool) -> Result<()> {
        let mut shares = self
            .explicit_shares
            .write()
            .expect("share registry poisoned");
        if explicit {
            shares.insert(infohash.to_owned());
        } else {
            shares.remove(infohash);
        }
        let bytes = serde_json::to_vec(&*shares)?;
        let temporary = self.state_dir.join("explicit-shares.tmp");
        std::fs::write(&temporary, bytes).context("Write share identities")?;
        std::fs::rename(temporary, self.state_dir.join("explicit-shares.json"))
            .context("Save share identities")
    }

    fn handle(&self, infohash: &str) -> Result<Arc<ManagedTorrent>> {
        self.session
            .get(TorrentIdOrHash::Hash(
                infohash.parse().context("Invalid infohash")?,
            ))
            .context("Unknown torrent")
    }
}

/// librqbit's file stream, which the pinned release returns without
/// exporting its type.
trait ReadSeek: tokio::io::AsyncRead + tokio::io::AsyncSeek + Send + Unpin {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncSeek + Send + Unpin> ReadSeek for T {}

/// One torrent file read as it downloads; see [`TransferSession::open_stream`].
pub struct VerifiedStream {
    inner: Box<dyn ReadSeek>,
    len: u64,
}

impl VerifiedStream {
    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// A blocking reader for players that read on their own threads. A task
    /// on `runtime` performs each read, so callers never block the runtime.
    pub fn into_blocking(self, runtime: &tokio::runtime::Handle) -> BlockingStream {
        let (requests, receiver) = tokio::sync::mpsc::unbounded_channel();
        let cancel = CancellationToken::new();
        runtime.spawn(serve_stream(self.inner, receiver, cancel.clone()));
        BlockingStream {
            requests,
            len: self.len,
            cancel,
        }
    }
}

enum StreamRequest {
    Read(usize, std::sync::mpsc::Sender<std::io::Result<Vec<u8>>>),
    Seek(u64, std::sync::mpsc::Sender<std::io::Result<u64>>),
}

async fn serve_stream(
    mut stream: Box<dyn ReadSeek>,
    mut requests: tokio::sync::mpsc::UnboundedReceiver<StreamRequest>,
    cancel: CancellationToken,
) {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};
    let cancelled = || std::io::Error::new(std::io::ErrorKind::Interrupted, "Stream cancelled");
    loop {
        let request = tokio::select! {
            request = requests.recv() => request,
            () = cancel.cancelled() => None,
        };
        match request {
            None => break,
            Some(StreamRequest::Read(len, reply)) => {
                let mut buffer = vec![0; len];
                let result: std::io::Result<Vec<u8>> = tokio::select! {
                    read = stream.read(&mut buffer) => read.map(|read: usize| {
                        buffer.truncate(read);
                        buffer
                    }),
                    () = cancel.cancelled() => Err(cancelled()),
                };
                let _ = reply.send(result);
            }
            Some(StreamRequest::Seek(offset, reply)) => {
                let result: std::io::Result<u64> = tokio::select! {
                    at = stream.seek(std::io::SeekFrom::Start(offset)) => at,
                    () = cancel.cancelled() => Err(cancelled()),
                };
                let _ = reply.send(result);
            }
        }
    }
}

/// A blocking view of a [`VerifiedStream`]. Dropping it, or cancelling
/// through [`BlockingStream::canceller`], ends pending and later reads.
pub struct BlockingStream {
    requests: tokio::sync::mpsc::UnboundedSender<StreamRequest>,
    len: u64,
    cancel: CancellationToken,
}

impl BlockingStream {
    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Reads up to `buffer.len()` bytes, waiting for verified pieces.
    /// Returns 0 at the end of the file.
    pub fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let (reply, response) = std::sync::mpsc::channel();
        self.request(StreamRequest::Read(buffer.len(), reply))?;
        let bytes = Self::wait(response)??;
        buffer[..bytes.len()].copy_from_slice(&bytes);
        Ok(bytes.len())
    }

    pub fn seek(&mut self, offset: u64) -> std::io::Result<u64> {
        let (reply, response) = std::sync::mpsc::channel();
        self.request(StreamRequest::Seek(offset, reply))?;
        Self::wait(response)?
    }

    pub fn canceller(&self) -> CancellationToken {
        self.cancel.clone()
    }

    fn request(&self, request: StreamRequest) -> std::io::Result<()> {
        if self.cancel.is_cancelled() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "Stream cancelled",
            ));
        }
        self.requests
            .send(request)
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "Stream closed"))
    }

    fn wait<T>(response: std::sync::mpsc::Receiver<T>) -> std::io::Result<T> {
        response
            .recv()
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "Stream closed"))
    }
}

impl Drop for BlockingStream {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

impl Drop for TransferSession {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

// Adapter downloads reserve an exclusive infohash-named destination; explicit
// shares use their original output folder. This also recognizes persisted
// downloads created before the completion preference existed.
fn is_completed_download(handle: &ManagedTorrent, shares: &RwLock<HashSet<String>>) -> bool {
    if shares
        .read()
        .expect("share registry poisoned")
        .contains(&handle.info_hash().as_string())
    {
        return false;
    }
    handle
        .output_folder()
        .file_name()
        .is_some_and(|name| name == handle.info_hash().as_string().as_str())
        && handle.stats().finished
}

async fn pause_completed_downloads(session: &Session, shares: &RwLock<HashSet<String>>) {
    let handles = session.with_torrents(|items| items.map(|(_, h)| h.clone()).collect::<Vec<_>>());
    for handle in handles {
        if handle.live().is_some()
            && is_completed_download(&handle, shares)
            && let Err(error) = session.pause(&handle).await
        {
            tracing::warn!(%error, "Could not stop completed download sharing");
        }
    }
}

#[cfg(unix)]
fn delete_owned_file(root: &Path, relative: &Path) -> std::io::Result<()> {
    use std::{
        ffi::CString,
        os::{
            fd::{AsRawFd, FromRawFd, OwnedFd},
            unix::ffi::OsStrExt,
        },
    };
    // Open each directory without following symlinks. unlinkat stays relative
    // to these descriptors even if another process renames a parent directory.
    let mut directory: OwnedFd = File::open("/")?.into();
    let parent = root.join(relative.parent().unwrap_or(Path::new("")));
    for component in parent.components() {
        let Component::Normal(name) = component else {
            continue;
        };
        let name = CString::new(name.as_bytes())?;
        let fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        directory = unsafe { OwnedFd::from_raw_fd(fd) };
    }
    let name = CString::new(
        relative
            .file_name()
            .expect("validated relative path")
            .as_bytes(),
    )?;
    // unlinkat never follows a final symlink and cannot delete a directory.
    let result = unsafe { libc::unlinkat(directory.as_raw_fd(), name.as_ptr(), 0) };
    if result < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(unix))]
fn delete_owned_file(root: &Path, relative: &Path) -> std::io::Result<()> {
    std::fs::remove_file(root.join(relative))
}

fn validate_delete_path(root: &Path, relative: &Path) -> Result<()> {
    validate_relative_path(relative)?;
    ensure!(
        std::fs::canonicalize(root)? == root,
        "Destination changed or contains a symbolic link"
    );
    let mut path = root.to_owned();
    for component in relative.components() {
        path.push(component);
        match std::fs::symlink_metadata(&path) {
            Ok(meta) => {
                #[cfg(windows)]
                {
                    use std::os::windows::fs::MetadataExt;
                    ensure!(
                        meta.file_attributes() & 0x400 == 0,
                        "Refusing deletion through a reparse point"
                    );
                }
                ensure!(
                    !meta.is_symlink(),
                    "Refusing deletion through a symbolic link"
                );
                ensure!(
                    if path == root.join(relative) {
                        meta.is_file()
                    } else {
                        meta.is_dir()
                    },
                    "Torrent path was replaced"
                );
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => break,
            Err(e) => return Err(e).context("Inspect torrent deletion path"),
        }
    }
    Ok(())
}

fn utf8_path(path: &Path) -> Result<&str> {
    path.to_str().context("Transfer path is not valid UTF-8")
}

fn validate_relative_path(path: &Path) -> Result<()> {
    ensure!(
        !path.as_os_str().is_empty()
            && path.components().all(|p| matches!(p, Component::Normal(_))),
        "Unsafe torrent file path"
    );
    Ok(())
}

fn validate_selection(selection: Option<Vec<usize>>, count: usize) -> Result<Option<Vec<usize>>> {
    if let Some(indices) = &selection {
        ensure!(!indices.is_empty(), "Select at least one file");
        let mut seen = HashSet::new();
        ensure!(
            indices.iter().all(|i| *i < count && seen.insert(*i)),
            "Invalid or duplicate file index"
        );
    }
    Ok(selection)
}

fn validate_share_source(path: &Path) -> Result<()> {
    utf8_path(path)?;
    let metadata = std::fs::symlink_metadata(path).context("Read shared path")?;
    ensure!(
        !metadata.is_symlink(),
        "Choose original files instead of symbolic links"
    );
    if metadata.is_dir() {
        for entry in std::fs::read_dir(path).context("Read shared directory")? {
            validate_share_source(&entry?.path())?;
        }
    } else {
        ensure!(
            metadata.is_file(),
            "Only regular files and folders can be shared"
        );
    }
    Ok(())
}

fn session_options(
    config: &TransferConfig,
    state_dir: &Path,
    listen_addr: SocketAddr,
    cancel: CancellationToken,
) -> Result<SessionOptions> {
    Ok(SessionOptions {
        dht: config.public_discovery.then(|| DhtSessionConfig {
            persistence: Some(librqbit::dht::DhtPersistenceConfig {
                config_filename: Some(state_dir.join("dht.json")),
                ..Default::default()
            }),
            ..Default::default()
        }),
        trackers: if config.public_discovery {
            PUBLIC_TRACKERS
                .iter()
                .map(|value| reqwest::Url::parse(value))
                .collect::<Result<_, _>>()?
        } else {
            HashSet::new()
        },
        disable_trackers: !config.public_discovery,
        disable_local_service_discovery: true,
        persistence: Some(SessionPersistenceConfig::Json {
            folder: Some(state_dir.to_owned()),
        }),
        fastresume: false, // Always verify on-disk bytes when reopening.
        listen: Some(ListenerOptions {
            mode: config.listener_mode,
            listen_addr,
            ipv4_only: listen_addr.is_ipv4(),
            enable_upnp_port_forwarding: config.public_discovery,
            announce_port: config.public_discovery.then_some(listen_addr.port()),
            ..Default::default()
        }),
        ipv4_only: listen_addr.is_ipv4(),
        cancellation_token: Some(cancel),
        client_name_and_version: Some(concat!("Flick/", env!("CARGO_PKG_VERSION")).into()),
        ratelimits: librqbit::limits::LimitsConfig {
            download_bps: config.download_limit_bytes_per_second,
            upload_bps: config.upload_limit_bytes_per_second,
        },
        ..Default::default()
    })
}

fn persist_listen_port(state_dir: &Path, port: u16) -> Result<()> {
    let temporary = state_dir.join("listen-port.tmp");
    std::fs::write(&temporary, format!("{port}\n")).context("Write transfer listen port")?;
    std::fs::rename(temporary, state_dir.join("listen-port")).context("Save transfer listen port")
}

fn select_listen_addr(config: &TransferConfig, state_dir: &Path) -> Result<SocketAddr> {
    if !config.public_discovery {
        return Ok(config.listen_addr);
    }
    let mut addr = config.listen_addr;
    if addr.port() == 0 {
        match std::fs::read_to_string(state_dir.join("listen-port")) {
            Ok(value) => addr.set_port(
                value
                    .trim()
                    .parse::<u16>()
                    .ok()
                    .filter(|p| *p != 0)
                    .unwrap_or(0),
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("Read transfer listen port"),
        }
    }
    if addr.port() != 0 && port_available(addr, config.listener_mode)? {
        return Ok(addr);
    }
    available_public_addr(addr, config.listener_mode)
}

/// Whether a bind failed because of the port, so another port may work.
/// Windows refuses ports in its excluded ranges, which Hyper-V and WinNAT
/// reserve inside 49152–65535, with an access error rather than "in use".
/// The TCP and UDP ranges differ, so a port can pass one probe and fail the
/// other.
fn port_unusable(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::AddrInUse | std::io::ErrorKind::PermissionDenied
    )
}

fn port_available(addr: SocketAddr, mode: ListenerMode) -> Result<bool> {
    let _tcp = match TcpListener::bind(addr) {
        Ok(socket) => socket,
        Err(error) if port_unusable(&error) => return Ok(false),
        Err(error) => return Err(error).context("Probe transfer TCP listen address"),
    };
    if mode.utp_enabled() {
        match UdpSocket::bind(addr) {
            Ok(_) => {}
            Err(error) if port_unusable(&error) => return Ok(false),
            Err(error) => return Err(error).context("Probe transfer UDP listen address"),
        }
    }
    Ok(true)
}

fn available_public_addr(mut addr: SocketAddr, mode: ListenerMode) -> Result<SocketAddr> {
    // RandomState is seeded from the OS, without adding a random-number dependency.
    let first = std::collections::hash_map::RandomState::new()
        .build_hasher()
        .finish();
    for offset in 0..16384 {
        addr.set_port(49152 + (first.wrapping_add(offset) % 16384) as u16);
        if port_available(addr, mode)? {
            return Ok(addr);
        }
    }
    bail!("No available transfer listen port in 49152–65535")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_trackers_are_valid_unique_supported_urls() {
        let mut seen = HashSet::new();
        assert!(!PUBLIC_TRACKERS.is_empty());
        for value in PUBLIC_TRACKERS {
            let url = reqwest::Url::parse(value).unwrap();
            assert!(matches!(url.scheme(), "udp" | "http" | "https"));
            assert!(url.host_str().is_some());
            assert!(seen.insert(url));
        }
    }

    #[test]
    fn local_config_disables_all_discovery_and_mapping() -> Result<()> {
        let root = tempfile::tempdir()?;
        let config =
            TransferConfig::local(root.path().join("downloads"), root.path().join("state"));
        let addr = select_listen_addr(&config, root.path())?;
        assert!(addr.ip().is_loopback());
        assert_eq!(addr.port(), 0);
        let opts = session_options(&config, root.path(), addr, CancellationToken::new())?;
        assert!(opts.dht.is_none());
        assert!(opts.trackers.is_empty());
        assert!(opts.disable_trackers);
        assert!(opts.disable_local_service_discovery);
        let listen = opts.listen.unwrap();
        assert!(!listen.enable_upnp_port_forwarding);
        assert!(listen.announce_port.is_none());
        assert!(!root.path().join("listen-port").exists());
        Ok(())
    }

    #[test]
    fn public_port_is_persisted_reused_and_replaced_when_busy() -> Result<()> {
        let root = tempfile::tempdir()?;
        let mut config =
            TransferConfig::local(root.path().join("downloads"), root.path().to_owned());
        config.public_discovery = true;
        let first = select_listen_addr(&config, root.path())?;
        assert!((49152..=65535).contains(&first.port()));
        persist_listen_port(root.path(), first.port())?;
        assert_eq!(select_listen_addr(&config, root.path())?, first);
        let busy = TcpListener::bind(first)?;
        let replacement = select_listen_addr(&config, root.path())?;
        assert_ne!(replacement.port(), first.port());
        persist_listen_port(root.path(), replacement.port())?;
        assert_eq!(select_listen_addr(&config, root.path())?, replacement);
        drop(busy);
        config.listener_mode = ListenerMode::TcpAndUtp;
        config.listen_addr.set_port(first.port());
        let busy_udp = UdpSocket::bind(first)?;
        assert_ne!(select_listen_addr(&config, root.path())?, first);
        drop(busy_udp);
        std::fs::write(root.path().join("listen-port"), "invalid")?;
        config.listen_addr.set_port(0);
        assert_ne!(select_listen_addr(&config, root.path())?.port(), 0);
        config.listen_addr.set_port(first.port());
        assert_eq!(select_listen_addr(&config, root.path())?, first);
        let opts = session_options(&config, root.path(), first, CancellationToken::new())?;
        assert_eq!(opts.trackers.len(), PUBLIC_TRACKERS.len());
        let persistence = opts.dht.unwrap().persistence.unwrap();
        assert_eq!(
            persistence.config_filename,
            Some(root.path().join("dht.json"))
        );
        let listen = opts.listen.unwrap();
        assert!(listen.enable_upnp_port_forwarding);
        assert_eq!(listen.announce_port, Some(first.port()));
        Ok(())
    }

    #[test]
    fn refused_ports_are_replaced_like_busy_ones() -> Result<()> {
        // Unprivileged Linux processes cannot bind port 1, which stands in for
        // a port inside a Windows excluded range. Elsewhere the bind succeeds
        // and there is nothing to check.
        let refused: SocketAddr = ([127, 0, 0, 1], 1).into();
        match TcpListener::bind(refused) {
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {}
            _ => return Ok(()),
        }
        assert!(!port_available(refused, ListenerMode::TcpAndUtp)?);
        let root = tempfile::tempdir()?;
        let mut config =
            TransferConfig::local(root.path().join("downloads"), root.path().to_owned());
        config.public_discovery = true;
        config.listener_mode = ListenerMode::TcpAndUtp;
        config.listen_addr = refused;
        let replacement = select_listen_addr(&config, root.path())?;
        assert!((49152..=65535).contains(&replacement.port()));
        Ok(())
    }
}
