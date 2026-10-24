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
    net::SocketAddr,
    num::NonZeroU32,
    path::{Component, Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::io::AsyncReadExt;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

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
    /// Explicit opt-in to DHT and trackers. Multicast/port mapping stay off.
    pub public_discovery: bool,
    pub download_limit_bytes_per_second: Option<NonZeroU32>,
}

impl TransferConfig {
    pub fn local(download_dir: PathBuf, state_dir: PathBuf) -> Self {
        Self {
            download_dir,
            state_dir,
            listen_addr: ([127, 0, 0, 1], 0).into(),
            public_discovery: false,
            download_limit_bytes_per_second: None,
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
        let cancel = CancellationToken::new();
        let session = Session::new_with_opts(
            download_dir.clone(),
            SessionOptions {
                dht: config.public_discovery.then_some(DhtSessionConfig {
                    persistence: None,
                    ..Default::default()
                }),
                disable_trackers: !config.public_discovery,
                disable_local_service_discovery: true,
                persistence: Some(SessionPersistenceConfig::Json {
                    folder: Some(state_dir),
                }),
                fastresume: false, // Always verify on-disk bytes when reopening.
                listen: Some(ListenerOptions {
                    mode: ListenerMode::TcpOnly,
                    listen_addr: config.listen_addr,
                    ipv4_only: config.listen_addr.is_ipv4(),
                    enable_upnp_port_forwarding: false,
                    ..Default::default()
                }),
                cancellation_token: Some(cancel.clone()),
                client_name_and_version: Some(concat!("Flick/", env!("CARGO_PKG_VERSION")).into()),
                ratelimits: librqbit::limits::LimitsConfig {
                    download_bps: config.download_limit_bytes_per_second,
                    upload_bps: None,
                },
                ..Default::default()
            },
        )
        .await;
        match session {
            Ok(session) => Ok(Self {
                session,
                download_dir,
                cancel,
                _state_lock: lock,
            }),
            Err(error) => {
                cancel.cancel();
                Err(error).context("Start transfer session")
            }
        }
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
        .context("Torrent metadata resolution timed out")??;
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
    pub async fn remove(&self, infohash: &str, delete_files: bool) -> Result<()> {
        self.session
            .delete(
                TorrentIdOrHash::Hash(infohash.parse().context("Invalid infohash")?),
                delete_files,
            )
            .await
    }
    pub async fn wait_for_completion(&self, infohash: &str, deadline: Duration) -> Result<()> {
        timeout(deadline, self.handle(infohash)?.wait_until_completed())
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
        self.session.stop().await;
    }

    pub async fn shutdown(&self) {
        self.session.stop().await;
    }

    fn handle(&self, infohash: &str) -> Result<Arc<ManagedTorrent>> {
        self.session
            .get(TorrentIdOrHash::Hash(
                infohash.parse().context("Invalid infohash")?,
            ))
            .context("Unknown torrent")
    }
}

impl Drop for TransferSession {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
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
