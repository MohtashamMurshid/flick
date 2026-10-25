//! Native transfers, independent of GPUI, playback, and viewing history.
//!
//! Call asynchronous operations from a background Tokio runtime. One session
//! owns a locked state directory. `completed_file` gates playback on verification.

use anyhow::{Context, Result, bail, ensure};
use fs2::FileExt;
use librqbit::{
    AddTorrent, AddTorrentOptions, AddTorrentResponse, DhtSessionConfig, ListenerMode,
    ListenerOptions, ManagedTorrent, Session, SessionOptions, SessionPersistenceConfig,
    TorrentStatsState, api::TorrentIdOrHash,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    fs::{File, OpenOptions},
    hash::{BuildHasher, Hasher},
    net::{SocketAddr, TcpListener, UdpSocket},
    num::NonZeroU32,
    path::{Component, Path, PathBuf},
    sync::{
        Arc, Mutex, RwLock, Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::io::AsyncReadExt;
use tokio::sync::Notify;
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
mod share;

/// Canonical root evidence survives malformed records and startup failures.
/// `roots_complete` permits ruling out ownership only outside every root; it
/// never proves that a file within a root is verified or directly playable.
#[derive(Debug, Default)]
pub struct SavedTransferOwnership {
    pub roots: Vec<PathBuf>,
    pub roots_complete: bool,
    pub error: Option<String>,
    saved: Option<SavedOwnership>,
}

impl SavedTransferOwnership {
    pub fn has_saved_transfers(&self) -> bool {
        self.saved
            .as_ref()
            .is_some_and(|saved| !saved.infohashes.is_empty())
    }
}

#[derive(Debug)]
struct SavedOwnership {
    infohashes: HashSet<String>,
}

#[derive(Deserialize)]
struct SavedSession {
    torrents: HashMap<usize, SavedTorrent>,
}

#[derive(Deserialize)]
struct SavedTorrent {
    info_hash: String,
    #[serde(rename = "trackers")]
    _trackers: HashSet<String>,
    output_folder: PathBuf,
    #[serde(rename = "only_files")]
    _only_files: Option<Vec<usize>>,
    #[serde(rename = "is_paused")]
    _is_paused: bool,
}

fn validate_saved_folder(folder: &Path) -> Result<()> {
    ensure!(
        folder.is_absolute()
            && !folder
                .components()
                .any(|c| matches!(c, Component::ParentDir))
            && !folder.as_os_str().as_encoded_bytes().contains(&0),
        "Saved transfer folder must be a valid absolute path"
    );
    Ok(())
}

async fn read_saved_metadata(state_dir: &Path) -> Result<Option<Vec<u8>>> {
    read_saved_metadata_file(&state_dir.join("session.json")).await
}

async fn read_saved_metadata_file(path: &Path) -> Result<Option<Vec<u8>>> {
    let metadata = match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("Inspect saved transfer ownership"),
    };
    ensure!(
        metadata.is_file(),
        "Saved transfer metadata is not a regular file"
    );
    ensure!(
        metadata.len() <= MAX_METADATA_BYTES,
        "Saved transfer metadata exceeds 16 MiB"
    );
    let file = tokio::fs::File::open(path)
        .await
        .context("Open saved transfer metadata")?;
    ensure!(
        file.metadata().await?.is_file(),
        "Saved transfer metadata is not a regular file"
    );
    let mut bytes = Vec::new();
    file.take(MAX_METADATA_BYTES + 1)
        .read_to_end(&mut bytes)
        .await
        .context("Read saved transfer metadata")?;
    ensure!(
        bytes.len() as u64 <= MAX_METADATA_BYTES,
        "Saved transfer metadata exceeds 16 MiB"
    );
    Ok(Some(bytes))
}

/// Validate sidecar identities before librqbit can replace a saved record by ID.
/// Missing sidecars keep the engine's existing magnet-restoration behavior.
async fn validate_saved_metainfo(state_dir: &Path, saved: &SavedOwnership) -> Result<()> {
    for expected in &saved.infohashes {
        let path = state_dir.join(format!("{expected}.torrent"));
        let Some(bytes) = read_saved_metadata_file(&path)
            .await
            .with_context(|| format!("Read saved torrent metadata for {expected}"))?
        else {
            continue;
        };
        let actual = tokio::task::spawn_blocking(move || -> Result<_> {
            let metainfo =
                librqbit::torrent_from_bytes(&bytes).context("Parse saved torrent metadata")?;
            metainfo.info.data.validate()?;
            Ok(metainfo.info_hash.as_string())
        })
        .await
        .context("Check saved torrent metadata")??;
        ensure!(
            actual == *expected,
            "Saved torrent metadata infohash does not match saved transfer {expected}"
        );
    }
    Ok(())
}

/// Read before starting librqbit, whose restoration may wait for peers. Missing
/// output folders cannot contain openable files and do not hide other roots.
/// Unreadable/malformed metadata remains unknown, preserving any independently
/// valid roots from a syntactically valid JSON document.
pub async fn inspect_saved_transfer_ownership(state_dir: &Path) -> SavedTransferOwnership {
    let mut evidence = SavedTransferOwnership::default();
    let bytes = match read_saved_metadata(state_dir).await {
        Ok(Some(bytes)) => bytes,
        Ok(None) => {
            evidence.roots_complete = true;
            return evidence;
        }
        Err(error) => {
            evidence.error = Some(format!("{error:#}"));
            return evidence;
        }
    };
    let parsed = tokio::task::spawn_blocking(move || -> Result<_> {
        let document: serde_json::Value =
            serde_json::from_slice(&bytes).context("Parse saved transfer metadata")?;
        let torrents = document
            .get("torrents")
            .and_then(serde_json::Value::as_object)
            .context("Saved transfer metadata has no torrent records")?;
        // Extract each root separately: a malformed sibling record must not
        // erase sound canonical evidence from the rest of the document.
        let folders = torrents
            .values()
            .map(|record| {
                let folder: PathBuf = serde_json::from_value(
                    record
                        .get("output_folder")
                        .cloned()
                        .context("Missing saved transfer folder")?,
                )
                .context("Invalid saved transfer folder")?;
                validate_saved_folder(&folder)?;
                Ok(folder)
            })
            .collect::<Vec<Result<PathBuf>>>();
        let saved = (|| -> Result<SavedOwnership> {
            // Keep the required fields/types aligned with librqbit 9.0.1's
            // SerializedSessionDatabase/SerializedTorrent.
            let saved: SavedSession =
                serde_json::from_value(document).context("Parse saved transfer records")?;
            let mut infohashes = HashSet::new();
            for torrent in saved.torrents.into_values() {
                let _ = TorrentIdOrHash::Hash(
                    torrent
                        .info_hash
                        .parse()
                        .context("Invalid saved infohash")?,
                );
                validate_saved_folder(&torrent.output_folder)?;
                infohashes.insert(torrent.info_hash.to_ascii_lowercase());
            }
            Ok(SavedOwnership { infohashes })
        })();
        Ok((folders, saved))
    })
    .await;
    let (folders, saved) = match parsed {
        Ok(Ok(parsed)) => parsed,
        Ok(Err(error)) => {
            evidence.error = Some(format!("{error:#}"));
            return evidence;
        }
        Err(error) => {
            evidence.error = Some(format!("Read saved transfer ownership: {error}"));
            return evidence;
        }
    };
    match saved {
        Ok(saved) => evidence.saved = Some(saved),
        Err(error) => evidence.error = Some(format!("{error:#}")),
    }
    evidence.roots_complete = true;
    for folder in folders {
        let resolved = async {
            let folder = folder?;
            let mut candidate = folder.as_path();
            let root = loop {
                match tokio::fs::canonicalize(candidate).await {
                    Ok(root) => break root,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        // A dangling symlink can point outside its parent; its
                        // parent's canonical path cannot bound that output.
                        match tokio::fs::symlink_metadata(candidate).await {
                            Ok(metadata) => ensure!(
                                !metadata.is_symlink(),
                                "Saved transfer folder has an unavailable symlink target"
                            ),
                            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                            Err(error) => {
                                return Err(error).context("Inspect missing saved transfer folder");
                            }
                        }
                        // Keep a conservative canonical ancestor: restoration
                        // may recreate this output before its first snapshot.
                        candidate = candidate
                            .parent()
                            .context("Saved transfer folder has no available ancestor")?;
                    }
                    Err(error) => return Err(error).context("Resolve saved transfer folder"),
                }
            };
            ensure!(
                tokio::fs::metadata(&root).await?.is_dir(),
                "Saved transfer folder is not a directory"
            );
            Ok::<_, anyhow::Error>(root)
        }
        .await;
        match resolved {
            Ok(root) => evidence.roots.push(root),
            Err(error) => {
                evidence.roots_complete = false;
                evidence.error.get_or_insert_with(|| format!("{error:#}"));
            }
        }
    }
    evidence.roots.sort();
    evidence.roots.dedup();
    evidence
}

/// Strict convenience API for callers that require complete saved ownership.
pub async fn saved_transfer_roots(state_dir: &Path) -> Result<Option<Vec<PathBuf>>> {
    let evidence = inspect_saved_transfer_ownership(state_dir).await;
    if let Some(error) = evidence.error {
        bail!(error);
    }
    Ok(evidence.saved.map(|_| evidence.roots))
}

async fn read_saved_ownership(state_dir: &Path) -> Result<Option<SavedOwnership>> {
    let evidence = inspect_saved_transfer_ownership(state_dir).await;
    if let Some(error) = evidence.error {
        bail!(error);
    }
    Ok(evidence.saved)
}

/// Lock an existing state directory without starting an engine or creating a
/// missing directory. The caller retains this lock through its metadata read.
async fn lock_existing_state(state_dir: &Path) -> Result<Option<File>> {
    match tokio::fs::symlink_metadata(state_dir).await {
        Ok(metadata) => ensure!(metadata.is_dir(), "Transfer state is not a directory"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("Inspect transfer state"),
    }
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(state_dir.join("session.lock"))
        .context("Open transfer state lock")?;
    lock.try_lock_exclusive()
        .context("Another Flick transfer session owns this state directory")?;
    Ok(Some(lock))
}

/// Missing/empty metadata proves an empty startup only while no live engine
/// holds the directory. A held lock with no readable records remains unknown.
pub async fn confirm_saved_transfers_empty(state_dir: &Path) -> Result<()> {
    let Some(_lock) = lock_existing_state(state_dir).await? else {
        return Ok(());
    };
    let saved = read_saved_ownership(state_dir).await?;
    ensure!(
        saved.is_none_or(|saved| saved.infohashes.is_empty()),
        "Saved transfers changed during startup. Retry downloads."
    );
    Ok(())
}

/// Confirm an absent owner without starting an engine or any discovery. The
/// state lock prevents racing a live owner's writes; an unreadable session or
/// an owner still saved on disk must be retried through normal startup.
pub async fn confirm_saved_owner_absent(state_dir: &Path, infohash: &str) -> Result<()> {
    let _ = TorrentIdOrHash::Hash(infohash.parse().context("Invalid infohash")?);
    let Some(_lock) = lock_existing_state(state_dir).await? else {
        return Ok(());
    };
    if let Some(saved) = read_saved_ownership(state_dir).await? {
        ensure!(
            !saved.infohashes.contains(&infohash.to_ascii_lowercase()),
            "This transfer is still saved. Retry downloads before deleting its file."
        );
    }
    Ok(())
}

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

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
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

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
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
    policy_changed: Arc<Notify>,
    explicit_shares: Arc<RwLock<HashSet<String>>>,
    state_dir: PathBuf,
    snapshot_metadata: Mutex<HashMap<usize, Arc<SnapshotMetadata>>>,
    state_lock: Mutex<Option<File>>,
}

struct SnapshotMetadata {
    handle: Weak<ManagedTorrent>,
    infohash: String,
    name: String,
    files: Vec<(PathBuf, u64)>,
}

impl TransferSession {
    pub async fn start(config: TransferConfig) -> Result<Self> {
        Self::start_cancellable(config, CancellationToken::new()).await
    }

    /// Cancellation also reaches engine tasks spawned before restoration
    /// returns. Dropping only the startup future would leave those tasks alive.
    pub async fn start_cancellable(
        config: TransferConfig,
        cancel: CancellationToken,
    ) -> Result<Self> {
        let guard = cancel.clone().drop_guard();
        let result = tokio::select! {
            biased;
            _ = cancel.cancelled() => bail!("Transfer startup cancelled"),
            result = Self::start_inner(config, cancel.clone()) => result,
        };
        if result.is_ok() {
            guard.disarm();
        }
        result
    }

    async fn start_inner(
        config: TransferConfig,
        startup_cancel: CancellationToken,
    ) -> Result<Self> {
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
        let saved = read_saved_ownership(&state_dir).await?;
        if let Some(saved) = &saved {
            validate_saved_metainfo(&state_dir, saved).await?;
        }
        let mut listen_addr = select_listen_addr(&config, &state_dir)?;
        // A second process can bind between our probe and librqbit's bind.
        // Retry only unusable ports, without masking unrelated startup errors.
        for attempt in 0..16 {
            let cancel = startup_cancel.child_token();
            let session = Session::new_with_opts(
                download_dir.clone(),
                session_options(&config, &state_dir, listen_addr, cancel.clone())?,
            )
            .await;
            match session {
                Ok(session) => {
                    // The engine logs and skips some failed saved torrents.
                    // An incomplete restore must not publish an empty/partial
                    // ownership snapshot and unlock their preallocated files.
                    if let Some(saved) = &saved {
                        let restored = session.with_torrents(|items| {
                            items
                                .map(|(_, h)| h.info_hash().as_string())
                                .collect::<HashSet<_>>()
                        });
                        if !saved.infohashes.is_subset(&restored) {
                            session.stop().await;
                            bail!(
                                "Not all saved transfers could be restored; ownership remains unknown"
                            );
                        }
                    }
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
                    let policy_changed = Arc::new(Notify::new());
                    let policy_notify = policy_changed.clone();
                    tokio::spawn(async move {
                        loop {
                            let active = !policy.load(Ordering::Relaxed)
                                && pause_completed_downloads(&policy_session, &policy_shares).await;
                            tokio::select! {
                                _ = policy_cancel.cancelled() => break,
                                _ = policy_notify.notified() => {},
                                // Preserve completion latency while active; idle
                                // paused/seeding sessions need no policy timer.
                                _ = tokio::time::sleep(Duration::from_millis(250)), if active => {},
                            }
                        }
                    });
                    return Ok(Self {
                        session,
                        download_dir,
                        cancel,
                        seed_completed,
                        policy_changed,
                        explicit_shares,
                        state_dir,
                        snapshot_metadata: Mutex::new(HashMap::new()),
                        state_lock: Mutex::new(Some(lock)),
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
        ensure!(!self.cancel.is_cancelled(), "Transfers have shut down");
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
        ensure!(!self.cancel.is_cancelled(), "Transfers have shut down");
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
            Ok(_) => {
                self.policy_changed.notify_one();
                Ok(resolved.infohash.clone())
            }
            Err(error) => {
                // Remove only an empty reservation. Never remove partial media.
                let _ = tokio::fs::remove_dir(&destination).await;
                Err(error).context("Start download")
            }
        }
    }

    /// Hash and seed existing files. No metadata files are written into the source.
    pub async fn share(&self, source: &Path, trackers: Vec<String>) -> Result<SharedTorrent> {
        ensure!(!self.cancel.is_cancelled(), "Transfers have shut down");
        // Dropping/aborting this future also cancels its blocking hash worker.
        // A worker never owns the engine and cannot admit a cancelled share.
        let cancel = self.cancel.child_token();
        let _cancel_on_drop = cancel.clone().drop_guard();
        let created = share::create(source.to_owned(), cancel.clone())
            .await
            .context("Hash shared files")?;
        let output_folder = created.output_folder;
        let info = created.info;
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
        ensure!(!cancel.is_cancelled(), "Sharing cancelled before admission");
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
        ensure!(!self.cancel.is_cancelled(), "Transfers have shut down");
        let handles = self
            .session
            .with_torrents(|items| items.map(|(_, h)| h.clone()).collect::<Vec<_>>());
        let live = handles
            .iter()
            .map(|h| Arc::as_ptr(h) as usize)
            .collect::<HashSet<_>>();
        self.snapshot_metadata
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|key, _| live.contains(key));
        let mut snapshots = handles
            .iter()
            .map(|handle| self.snapshot_handle(handle))
            .collect::<Result<Vec<_>>>()?;
        snapshots.sort_unstable_by(|a, b| a.infohash.cmp(&b.infohash));
        Ok(snapshots)
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
        let selected = handle
            .only_files()
            .map(|indices| indices.into_iter().collect::<HashSet<_>>());
        // Paths, lengths, names and hashes are immutable for one engine
        // handle. Resume with explicit peers creates another handle, so cache
        // by handle identity (with a Weak guard), never by infohash alone.
        let key = Arc::as_ptr(handle) as usize;
        let mut cache = self
            .snapshot_metadata
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let metadata = cache
            .get(&key)
            .filter(|m| m.handle.ptr_eq(&Arc::downgrade(handle)))
            .cloned();
        let metadata = metadata.unwrap_or_else(|| {
            let files = handle.with_metadata(|m| {
                m.file_infos
                    .iter()
                    .map(|f| (handle.output_folder().join(&f.relative_filename), f.len))
                    .collect()
            });
            let known = files.is_ok();
            let metadata = Arc::new(SnapshotMetadata {
                handle: Arc::downgrade(handle),
                infohash: handle.info_hash().as_string(),
                name: handle.name().unwrap_or_else(|| "Unnamed torrent".into()),
                files: files.unwrap_or_default(),
            });
            if known {
                cache.insert(key, metadata.clone());
            }
            metadata
        });
        drop(cache);
        let files = metadata
            .files
            .iter()
            .enumerate()
            .map(|(index, (path, length))| FileProgress {
                index,
                path: path.clone(),
                length: *length,
                verified_bytes: stats.file_progress.get(index).copied().unwrap_or(0),
                selected: selected
                    .as_ref()
                    .is_none_or(|indices| indices.contains(&index)),
            })
            .collect::<Vec<_>>();
        Ok(TransferSnapshot {
            infohash: metadata.infohash.clone(),
            name: metadata.name.clone(),
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
        let result = self.session.pause(&handle).await;
        self.policy_changed.notify_one();
        result
    }

    /// Explicit peers are supplied again after restart in local-only mode.
    /// The pinned engine does not persist its initial peer list.
    pub async fn resume(&self, infohash: &str, peers: Vec<SocketAddr>) -> Result<()> {
        let handle = self.handle(infohash)?;
        // Error has no pending initialization to wait for. Ask the engine to
        // reopen storage and reverify retained bytes before trusting them again.
        if matches!(handle.stats().state, TorrentStatsState::Error) {
            self.session.unpause(&handle).await?;
        }
        timeout(Duration::from_secs(120), handle.wait_until_initialized())
            .await
            .context("File verification timed out")??;
        if peers.is_empty() {
            if handle.live().is_some() {
                self.policy_changed.notify_one();
                return Ok(());
            }
            let result = self.session.unpause(&handle).await;
            self.policy_changed.notify_one();
            return result;
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
        self.policy_changed.notify_one();
        Ok(())
    }
    /// Release a ready session's owner without deleting any files. A valid
    /// absent identity is already released; stopped/invalid sessions still fail.
    pub async fn release_for_deletion(&self, infohash: &str) -> Result<()> {
        ensure!(!self.cancel.is_cancelled(), "Transfers have shut down");
        let hash = infohash.parse().context("Invalid infohash")?;
        if self.session.get(TorrentIdOrHash::Hash(hash)).is_none() {
            return Ok(());
        }
        self.remove(infohash, false).await
    }

    /// Selected destination paths from engine metadata, including during checking.
    pub fn selected_file_paths(&self, infohash: &str) -> Result<Vec<(PathBuf, usize)>> {
        let handle = self.handle(infohash)?;
        let selected = handle
            .only_files()
            .map(|indices| indices.into_iter().collect::<HashSet<_>>());
        handle.with_metadata(|metadata| {
            metadata
                .file_infos
                .iter()
                .enumerate()
                .filter(|(index, _)| selected.as_ref().is_none_or(|files| files.contains(index)))
                .map(|(index, file)| (handle.output_folder().join(&file.relative_filename), index))
                .collect()
        })
    }

    /// Only regular metainfo-listed files inside the torrent's output folder
    /// are eligible. Never recursively remove a directory or follow a symlink.
    pub async fn remove(&self, infohash: &str, delete_files: bool) -> Result<()> {
        let handle = self.handle(infohash)?;
        ensure!(
            !matches!(handle.stats().state, TorrentStatsState::Initializing { .. }),
            "Files are still being verified. Wait for verification to finish, then try again."
        );
        ensure!(
            delete_files || !matches!(handle.stats().state, TorrentStatsState::Error),
            "Resume this transfer in Downloads so its files can be verified, then try again. Your files and their transfer were kept."
        );
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
        self.policy_changed.notify_one();
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

    /// Updates the policy without waiting for torrent initialization or disk
    /// persistence. The policy task wakes immediately, including when idle.
    pub fn request_seed_completed_downloads(&self, enabled: bool) {
        self.seed_completed.store(enabled, Ordering::Relaxed);
        self.policy_changed.notify_one();
    }

    pub async fn set_seed_completed_downloads(&self, enabled: bool) {
        self.request_seed_completed_downloads(enabled);
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
        // Inspect just this file. Building every file's progress once per
        // completed video made a fully selected multi-file torrent quadratic.
        let handle = self.handle(infohash)?;
        let stats = handle.stats();
        ensure!(
            !matches!(
                stats.state,
                TorrentStatsState::Initializing { .. } | TorrentStatsState::Error
            ),
            "File verification is not complete"
        );
        let (path, length) = handle
            .with_metadata(|m| {
                m.file_infos
                    .get(index)
                    .map(|f| (handle.output_folder().join(&f.relative_filename), f.len))
            })?
            .context("Unknown torrent file index")?;
        ensure!(
            handle
                .only_files()
                .is_none_or(|files| files.contains(&index))
                && stats.file_progress.get(index).copied().unwrap_or(0) == length,
            "Selected file is not fully verified"
        );
        ensure!(path.is_file(), "Completed file is missing from disk");
        Ok(path)
    }

    pub async fn stop(self) {
        self.cancel.cancel();
        self.session.stop().await;
    }

    pub async fn shutdown(&self) {
        self.cancel.cancel();
        self.session.stop().await;
        // Player-side stream opening can retain an Arc after shutdown. A
        // stopped session must release ownership independently of that Arc.
        self.state_lock
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
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
        ensure!(!self.cancel.is_cancelled(), "Transfers have shut down");
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

async fn pause_completed_downloads(session: &Session, shares: &RwLock<HashSet<String>>) -> bool {
    let handles = session.with_torrents(|items| items.map(|(_, h)| h.clone()).collect::<Vec<_>>());
    let mut active = false;
    for handle in handles {
        let stats = handle.stats();
        active |= matches!(stats.state, TorrentStatsState::Initializing { .. })
            || (matches!(stats.state, TorrentStatsState::Live) && !stats.finished);
        if handle.live().is_some()
            && is_completed_download(&handle, shares)
            && let Err(error) = session.pause(&handle).await
        {
            tracing::warn!(%error, "Could not stop completed download sharing");
            active = true; // Retry a failed pause at the normal active cadence.
        }
    }
    active
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

    // Fail after hashing, when initialization hands storage to the paused
    // state. This produces a real engine Error without permissions, disk-full
    // assumptions, network traffic, or changing the generated fixture's bytes.
    #[derive(Clone)]
    struct FailTakeFactory(Arc<AtomicBool>);

    struct FailTakeStorage {
        inner: librqbit::storage::filesystem::FilesystemStorage,
        fail: Arc<AtomicBool>,
    }

    impl librqbit::storage::StorageFactory for FailTakeFactory {
        type Storage = Box<dyn librqbit::storage::TorrentStorage>;

        fn create(
            &self,
            shared: &librqbit::ManagedTorrentShared,
            metadata: &librqbit::TorrentMetadata,
        ) -> Result<Self::Storage> {
            Ok(Box::new(FailTakeStorage {
                inner: librqbit::storage::filesystem::FilesystemStorageFactory::default()
                    .create(shared, metadata)?,
                fail: self.0.clone(),
            }))
        }

        fn is_type_id(&self, type_id: std::any::TypeId) -> bool {
            // This wrapper still stores ordinary files; allow JSON persistence
            // to record their paths as it does for the wrapped factory.
            librqbit::storage::filesystem::FilesystemStorageFactory::default().is_type_id(type_id)
        }

        fn clone_box(&self) -> librqbit::storage::BoxStorageFactory {
            Box::new(self.clone())
        }
    }

    impl librqbit::storage::TorrentStorage for FailTakeStorage {
        fn init(
            &mut self,
            shared: &librqbit::ManagedTorrentShared,
            metadata: &librqbit::TorrentMetadata,
        ) -> Result<()> {
            self.inner.init(shared, metadata)
        }

        fn pread_exact(&self, file: usize, offset: u64, buf: &mut [u8]) -> Result<()> {
            self.inner.pread_exact(file, offset, buf)
        }

        fn pwrite_all(&self, file: usize, offset: u64, buf: &[u8]) -> Result<()> {
            self.inner.pwrite_all(file, offset, buf)
        }

        fn remove_file(&self, file: usize, path: &Path) -> Result<()> {
            self.inner.remove_file(file, path)
        }

        fn remove_directory_if_empty(&self, path: &Path) -> Result<()> {
            self.inner.remove_directory_if_empty(path)
        }

        fn ensure_file_length(&self, file: usize, length: u64) -> Result<()> {
            self.inner.ensure_file_length(file, length)
        }

        fn take(&self) -> Result<Box<dyn librqbit::storage::TorrentStorage>> {
            ensure!(
                !self.fail.load(Ordering::Relaxed),
                "Fixture storage failure"
            );
            self.inner.take()
        }
    }

    #[tokio::test]
    async fn error_owner_refuses_retained_file_release_and_recovers_after_resume() -> Result<()> {
        for delete_in_error in [false, true] {
            let root = tempfile::tempdir()?;
            let config =
                TransferConfig::local(root.path().join("downloads"), root.path().join("state"));
            let source = root.path().join("season");
            std::fs::create_dir(&source)?;
            let files = [
                ("chosen.mp4", vec![41; 65_537]),
                ("sibling.mp4", vec![42; 65_537]),
            ];
            for (name, bytes) in &files {
                std::fs::write(source.join(name), bytes)?;
            }
            let created = share::create(source.clone(), CancellationToken::new()).await?;
            #[derive(Serialize)]
            struct Envelope<T> {
                info: T,
            }
            let mut bytes = Vec::new();
            librqbit_bencode::bencode_serialize_to_writer(
                &Envelope { info: created.info },
                &mut bytes,
            )?;
            let fail = Arc::new(AtomicBool::new(true));
            let session = TransferSession::start(config.clone()).await?;
            let handle = session
                .session
                .add_torrent(
                    AddTorrent::from_bytes(bytes),
                    Some(AddTorrentOptions {
                        output_folder: Some(utf8_path(&created.output_folder)?.to_owned()),
                        storage_factory: Some(Box::new(FailTakeFactory(fail.clone()))),
                        overwrite: true,
                        paused: true,
                        ..Default::default()
                    }),
                )
                .await?
                .into_handle()
                .context("Missing fixture owner")?;
            let id = handle.info_hash().as_string();
            let error = timeout(Duration::from_secs(5), handle.wait_until_initialized())
                .await?
                .expect_err("Fixture did not enter Error");
            assert!(
                error.to_string().contains("Fixture storage failure"),
                "{error:#}"
            );
            session.set_explicit_share(&id, true)?;
            let marks_path = config.state_dir.join("explicit-shares.json");
            let marks = std::fs::read(&marks_path)?;
            for release in [false, true, false, true] {
                let error = timeout(Duration::from_millis(250), async {
                    if release {
                        session.release_for_deletion(&id).await
                    } else {
                        session.remove(&id, false).await
                    }
                })
                .await?
                .expect_err("Error owner was released");
                assert!(
                    error.to_string().contains("Resume this transfer"),
                    "{error:#}"
                );
                assert!(Arc::ptr_eq(&session.handle(&id)?, &handle));
                assert_eq!(session.snapshot(&id)?.state, TransferState::Error);
                assert!(session.completed_file(&id, 0).is_err());
                assert_eq!(std::fs::read(&marks_path)?, marks);
                assert!(session.explicit_shares.read().unwrap().contains(&id));
                for (name, bytes) in &files {
                    assert_eq!(std::fs::read(source.join(name))?, *bytes);
                }
                let lock = File::open(config.state_dir.join("session.lock"))?;
                assert!(
                    lock.try_lock_exclusive().is_err(),
                    "Refusal dropped state lock"
                );
            }
            if delete_in_error {
                // The explicit destructive action remains available in Error.
                session.remove(&id, true).await?;
                for (name, _) in &files {
                    assert!(!source.join(name).exists());
                }
            } else {
                fail.store(false, Ordering::Relaxed);
                timeout(Duration::from_secs(5), session.resume(&id, vec![])).await??;
                let snapshot = session.snapshot(&id)?;
                assert!(!matches!(
                    snapshot.state,
                    TransferState::Checking | TransferState::Error
                ));
                for file in snapshot.files {
                    assert_eq!(file.verified_bytes, file.length);
                    assert_eq!(session.completed_file(&id, file.index)?, file.path);
                }
                session.release_for_deletion(&id).await?;
                session.release_for_deletion(&id).await?;
                for (name, bytes) in &files {
                    assert_eq!(std::fs::read(source.join(name))?, *bytes);
                }
            }
            assert!(session.snapshots()?.is_empty());
            assert!(!session.explicit_shares.read().unwrap().contains(&id));
            session.stop().await;
        }
        Ok(())
    }

    #[tokio::test]
    async fn removing_initializing_owner_is_immediate_retriable_and_preserves_verified_removal()
    -> Result<()> {
        let root = tempfile::tempdir()?;
        let config =
            TransferConfig::local(root.path().join("downloads"), root.path().join("state"));
        let source = root.path().join("kept.mp4");
        let content = vec![41; 65_537];
        std::fs::write(&source, &content)?;
        let created = share::create(source.clone(), CancellationToken::new()).await?;
        #[derive(Serialize)]
        struct Envelope<T> {
            info: T,
        }
        let mut bytes = Vec::new();
        librqbit_bencode::bencode_serialize_to_writer(
            &Envelope { info: created.info },
            &mut bytes,
        )?;
        let session = TransferSession::start(config.clone()).await?;
        let handle = session
            .session
            .add_torrent(
                AddTorrent::from_bytes(bytes),
                Some(AddTorrentOptions {
                    output_folder: Some(utf8_path(&created.output_folder)?.to_owned()),
                    overwrite: true,
                    paused: true,
                    ..Default::default()
                }),
            )
            .await?
            .into_handle()
            .context("Missing fixture owner")?;
        // This single-thread runtime has not yielded since the engine spawned
        // verification. All three refusals must return without waiting for it.
        let id = handle.info_hash().as_string();
        assert!(matches!(
            handle.stats().state,
            TorrentStatsState::Initializing { .. }
        ));
        for delete_files in [false, true] {
            let error = timeout(
                Duration::from_millis(250),
                session.remove(&id, delete_files),
            )
            .await?
            .expect_err("Initializing owner was removed");
            assert!(error.to_string().contains("try again"), "{error:#}");
        }
        let error = timeout(
            Duration::from_millis(250),
            session.release_for_deletion(&id),
        )
        .await?
        .expect_err("Initializing owner was released");
        assert!(error.to_string().contains("try again"), "{error:#}");
        assert_eq!(session.snapshot(&id)?.state, TransferState::Checking);
        assert!(session.completed_file(&id, 0).is_err());
        assert_eq!(std::fs::read(&source)?, content);
        let lock = File::open(config.state_dir.join("session.lock"))?;
        assert!(
            lock.try_lock_exclusive().is_err(),
            "Refusal dropped the state lock"
        );
        timeout(Duration::from_secs(5), handle.wait_until_initialized()).await??;
        assert_eq!(session.completed_file(&id, 0)?, source.canonicalize()?);
        session.release_for_deletion(&id).await?;
        assert!(session.snapshot(&id).is_err());
        assert_eq!(std::fs::read(&source)?, content);
        session.stop().await;
        Ok(())
    }

    #[tokio::test]
    async fn share_hash_abort_and_shutdown_cancel_worker_without_engine_admission() -> Result<()> {
        use std::sync::atomic::Ordering;
        for abort in [true, false] {
            let root = tempfile::tempdir()?;
            let config =
                TransferConfig::local(root.path().join("downloads"), root.path().join("state"));
            let source = root.path().join("large-sparse.mp4");
            // Windows may reserve the full length instead of creating a sparse
            // file. Both sizes leave work after the first 64 KiB cancellation
            // fence, without reserving 64 GiB on the Windows CI disk.
            #[cfg(windows)]
            let length = 64 * 1024 * 1024;
            #[cfg(not(windows))]
            let length = 64 * 1024 * 1024 * 1024;
            let file = File::create(&source)?;
            file.set_len(length)?;
            drop(file);
            let probe = share::tests::pause_at_chunk(source.clone());
            let session = Arc::new(TransferSession::start(config.clone()).await?);
            let sharing = session.clone();
            let fixture = source.clone();
            let job = tokio::spawn(async move { sharing.share(&fixture, vec![]).await });
            timeout(Duration::from_secs(2), async {
                while !probe.entered.load(Ordering::SeqCst) {
                    tokio::time::sleep(Duration::from_millis(2)).await;
                }
            })
            .await
            .context("Hasher never reached its first actual chunk")?;
            assert!(!job.is_finished());
            assert!(session.snapshots()?.is_empty());
            let began = std::time::Instant::now();
            if abort {
                job.abort();
                assert!(
                    timeout(Duration::from_secs(2), job)
                        .await?
                        .unwrap_err()
                        .is_cancelled()
                );
            } else {
                timeout(Duration::from_secs(2), session.shutdown()).await?;
                assert!(timeout(Duration::from_secs(2), job).await??.is_err());
            }
            timeout(Duration::from_secs(2), async {
                while !probe.exited.load(Ordering::SeqCst) {
                    tokio::time::sleep(Duration::from_millis(2)).await;
                }
            })
            .await
            .context("Cancelled hashing continued in the background")?;
            assert!(began.elapsed() < Duration::from_secs(2));
            assert_eq!(std::fs::metadata(&source)?.len(), length);
            assert_eq!(File::open(&source)?.metadata()?.len(), length);
            assert_eq!(
                session.session.with_torrents(|torrents| torrents.count()),
                0
            );
            assert!(!source.with_extension("mp4.torrent").exists());
            session.shutdown().await;
            let lock = File::open(config.state_dir.join("session.lock"))?;
            lock.try_lock_exclusive()?;
        }
        Ok(())
    }

    #[tokio::test]
    async fn share_multifile_hashes_span_boundaries_and_ignore_trailing_empty_files() -> Result<()>
    {
        use sha1::{Digest, Sha1};
        let root = tempfile::tempdir()?;
        let source = root.path().join("fixture");
        std::fs::create_dir(&source)?;
        let first = vec![31; 2 * 1024 * 1024 - 7];
        let second = vec![42; 18];
        std::fs::write(source.join("a.bin"), &first)?;
        std::fs::create_dir(source.join("nested"))?;
        std::fs::write(source.join("nested/b.bin"), &second)?;
        std::fs::write(source.join("z-empty.bin"), [])?;
        let config =
            TransferConfig::local(root.path().join("downloads"), root.path().join("state"));
        let session = TransferSession::start(config).await?;
        let shared = session.share(&source, vec![]).await?;
        let parsed = librqbit::torrent_from_bytes(shared.torrent_bytes())?;
        parsed.info.data.clone().validate()?;
        let concatenated = [first.clone(), second.clone()].concat();
        let expected = concatenated
            .chunks(2 * 1024 * 1024)
            .flat_map(|chunk| Sha1::digest(chunk).to_vec())
            .collect::<Vec<_>>();
        assert_eq!(parsed.info.data.pieces.as_ref(), expected);
        let files = parsed.info.data.files.as_ref().unwrap();
        assert_eq!(
            files.iter().map(|file| file.length).collect::<Vec<_>>(),
            vec![first.len() as u64, second.len() as u64, 0]
        );
        assert_eq!(std::fs::read(source.join("a.bin"))?, first);
        assert_eq!(std::fs::read(source.join("nested/b.bin"))?, second);
        session.stop().await;
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn share_refuses_nested_and_root_symlinks_without_source_writes() -> Result<()> {
        let root = tempfile::tempdir()?;
        let source = root.path().join("source");
        std::fs::create_dir(&source)?;
        let outside = root.path().join("outside.bin");
        std::fs::write(&outside, b"preserved original")?;
        let config =
            TransferConfig::local(root.path().join("downloads"), root.path().join("state"));
        let session = TransferSession::start(config).await?;
        let alias = root.path().join("alias");
        std::os::unix::fs::symlink(&source, &alias)?;
        assert!(session.share(&alias, vec![]).await.is_err());
        std::os::unix::fs::symlink(&outside, source.join("nested.bin"))?;
        assert!(session.share(&source, vec![]).await.is_err());
        assert!(session.snapshots()?.is_empty());
        assert_eq!(std::fs::read(&outside)?, b"preserved original");
        session.stop().await;
        Ok(())
    }

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

    fn saved_record(folder: &Path) -> serde_json::Value {
        serde_json::json!({"info_hash":"0123456789abcdef0123456789abcdef01234567", "trackers":[], "output_folder":folder, "only_files":null, "is_paused":true})
    }

    async fn saved_metainfo_fixture(
        root: &Path,
    ) -> Result<(TransferConfig, String, Vec<u8>, PathBuf)> {
        let config = TransferConfig::local(root.join("downloads"), root.join("state"));
        let source = root.join("source");
        std::fs::create_dir(&source)?;
        std::fs::create_dir(&config.state_dir)?;
        std::fs::write(source.join("chosen.mp4"), vec![1; 32_768])?;
        std::fs::write(source.join("orphan-a.mp4"), vec![2; 32_768])?;
        let created = share::create(source, CancellationToken::new()).await?;
        #[derive(Serialize)]
        struct Envelope<T> {
            info: T,
        }
        let mut bytes = Vec::new();
        librqbit_bencode::bencode_serialize_to_writer(
            &Envelope { info: created.info },
            &mut bytes,
        )?;
        let infohash = librqbit::torrent_from_bytes(&bytes)?.info_hash.as_string();
        let orphan = created.output_folder.join("orphan-a.mp4");
        std::fs::write(&orphan, vec![0; 32_768])?;
        let mut record = saved_record(&created.output_folder);
        record["info_hash"] = serde_json::json!(infohash);
        record["only_files"] = serde_json::json!([0]);
        std::fs::write(
            config.state_dir.join("session.json"),
            serde_json::to_vec_pretty(&serde_json::json!({"torrents":{"0":record}}))?,
        )?;
        std::fs::write(config.state_dir.join(format!("{infohash}.torrent")), &bytes)?;
        Ok((config, infohash, bytes, orphan))
    }

    #[tokio::test]
    async fn saved_metainfo_mismatch_preserves_ownership_across_retry_and_repair() -> Result<()> {
        let root = tempfile::tempdir()?;
        let (config, infohash, bytes, orphan) = saved_metainfo_fixture(root.path()).await?;
        let session_path = config.state_dir.join("session.json");
        let saved = std::fs::read(&session_path)?;
        let sidecar = config.state_dir.join(format!("{infohash}.torrent"));
        let mut mismatched = bytes.clone();
        let offset = mismatched
            .windows(b"orphan-a.mp4".len())
            .position(|window| window == b"orphan-a.mp4")
            .context("Missing fixture filename")?;
        mismatched[offset + b"orphan-".len()] = b'b';
        assert_ne!(
            librqbit::torrent_from_bytes(&mismatched)?
                .info_hash
                .as_string(),
            infohash
        );
        std::fs::write(&sidecar, &mismatched)?;
        for _ in 0..2 {
            let error = TransferSession::start(config.clone())
                .await
                .err()
                .context("Mismatched sidecar restored")?;
            assert!(format!("{error:#}").contains("infohash does not match"));
            assert_eq!(std::fs::read(&session_path)?, saved);
            assert_eq!(std::fs::read(&sidecar)?, mismatched);
            assert_eq!(std::fs::read(&orphan)?, vec![0; 32_768]);
            assert!(!orphan.with_file_name("orphan-b.mp4").exists());
            assert!(
                confirm_saved_owner_absent(&config.state_dir, &infohash)
                    .await
                    .is_err()
            );
        }
        std::fs::write(&sidecar, bytes)?;
        let session = TransferSession::start(config).await?;
        let snapshot = session.snapshot(&infohash)?;
        session.stop().await;
        assert!(
            snapshot
                .files
                .iter()
                .any(|file| file.path == orphan && !file.selected)
        );
        assert_eq!(std::fs::read(orphan)?, vec![0; 32_768]);
        Ok(())
    }

    #[tokio::test]
    async fn invalid_saved_metainfo_preserves_ownership_before_engine_start() -> Result<()> {
        let root = tempfile::tempdir()?;
        let (config, infohash, bytes, _) = saved_metainfo_fixture(root.path()).await?;
        let session_path = config.state_dir.join("session.json");
        let saved = std::fs::read(&session_path)?;
        let sidecar = config.state_dir.join(format!("{infohash}.torrent"));
        for invalid in [b"".as_slice(), b"malformed".as_slice()] {
            std::fs::write(&sidecar, invalid)?;
            for _ in 0..2 {
                assert!(TransferSession::start(config.clone()).await.is_err());
                assert_eq!(std::fs::read(&session_path)?, saved);
                assert_eq!(std::fs::read(&sidecar)?, invalid);
                assert!(
                    confirm_saved_owner_absent(&config.state_dir, &infohash)
                        .await
                        .is_err()
                );
            }
        }
        std::fs::File::create(&sidecar)?.set_len(MAX_METADATA_BYTES + 1)?;
        let error = TransferSession::start(config.clone()).await.err().unwrap();
        assert!(format!("{error:#}").contains("16 MiB"));
        assert_eq!(std::fs::read(&session_path)?, saved);
        std::fs::remove_file(&sidecar)?;
        std::fs::create_dir(&sidecar)?;
        assert!(TransferSession::start(config.clone()).await.is_err());
        assert_eq!(std::fs::read(&session_path)?, saved);
        std::fs::remove_dir(&sidecar)?;
        #[cfg(unix)]
        {
            let target = config.state_dir.join("fixture.torrent");
            std::fs::write(&target, &bytes)?;
            std::os::unix::fs::symlink(&target, &sidecar)?;
            assert!(TransferSession::start(config.clone()).await.is_err());
            assert_eq!(std::fs::read(&session_path)?, saved);
            std::fs::remove_file(&sidecar)?;
        }
        // A genuinely missing sidecar still permits the existing magnet path;
        // the preflight alone never forgets or proves absence of that owner.
        let ownership = read_saved_ownership(&config.state_dir).await?.unwrap();
        validate_saved_metainfo(&config.state_dir, &ownership).await?;
        assert_eq!(std::fs::read(&session_path)?, saved);
        assert!(
            confirm_saved_owner_absent(&config.state_dir, &infohash)
                .await
                .is_err()
        );
        std::fs::write(sidecar, bytes)?;
        Ok(())
    }

    #[tokio::test]
    async fn saved_ownership_is_bounded_strict_and_conservative() -> Result<()> {
        let root = tempfile::tempdir()?;
        let state = root.path().join("state");
        std::fs::create_dir(&state)?;
        assert_eq!(saved_transfer_roots(&state).await?, None);
        let path = state.join("session.json");
        std::fs::write(&path, br#"{"torrents":{}}"#)?;
        assert_eq!(saved_transfer_roots(&state).await?, Some(Vec::new()));
        let folder = root.path().join("output");
        std::fs::create_dir(&folder)?;
        let valid =
            serde_json::json!({"torrents":{"0":saved_record(&folder), "1":saved_record(&folder)}});
        std::fs::write(&path, serde_json::to_vec(&valid)?)?;
        assert_eq!(
            saved_transfer_roots(&state).await?,
            Some(vec![folder.canonicalize()?])
        );
        for invalid in [
            serde_json::json!({}),
            serde_json::json!({"torrents":[]}),
            serde_json::json!({"torrents":{"0":{ "output_folder":folder }}}),
            serde_json::json!({"torrents":{"bad":saved_record(&folder)}}),
            serde_json::json!({"torrents":{"0":saved_record(Path::new("relative"))}}),
            serde_json::json!({"torrents":{"0":saved_record(&folder.join("../output"))}}),
        ] {
            std::fs::write(&path, serde_json::to_vec(&invalid)?)?;
            assert!(
                saved_transfer_roots(&state).await.is_err(),
                "Accepted {invalid}"
            );
        }
        std::fs::write(&path, b"malformed")?;
        assert!(saved_transfer_roots(&state).await.is_err());
        let file = OpenOptions::new().write(true).open(&path)?;
        file.set_len(MAX_METADATA_BYTES + 1)?;
        assert!(
            saved_transfer_roots(&state)
                .await
                .unwrap_err()
                .to_string()
                .contains("16 MiB")
        );
        drop(file);
        std::fs::remove_file(&path)?;
        std::fs::create_dir(&path)?;
        assert!(saved_transfer_roots(&state).await.is_err());
        Ok(())
    }

    #[tokio::test]
    async fn missing_saved_output_uses_canonical_ancestor_without_losing_other_roots() -> Result<()>
    {
        let root = tempfile::tempdir()?;
        let online = root.path().join("online");
        let parent = root.path().join("owned");
        std::fs::create_dir(&online)?;
        std::fs::create_dir(&parent)?;
        let missing = parent.join("removed").join("season");
        let saved =
            serde_json::json!({"torrents":{"0":saved_record(&online), "1":saved_record(&missing)}});
        std::fs::write(
            root.path().join("session.json"),
            serde_json::to_vec(&saved)?,
        )?;
        let evidence = inspect_saved_transfer_ownership(root.path()).await;
        assert!(evidence.error.is_none(), "{:?}", evidence.error);
        assert!(evidence.roots_complete);
        assert!(evidence.has_saved_transfers());
        let mut expected = vec![online.canonicalize()?, parent.canonicalize()?];
        expected.sort();
        assert_eq!(evidence.roots, expected);
        assert!(!missing.exists(), "Ownership inspection recreated output");
        Ok(())
    }

    #[tokio::test]
    async fn malformed_saved_records_retain_independent_canonical_root_evidence() -> Result<()> {
        let root = tempfile::tempdir()?;
        let known = root.path().join("known");
        std::fs::create_dir(&known)?;
        let path = root.path().join("session.json");
        for broken in [
            serde_json::json!({}),
            serde_json::json!({"output_folder":"relative"}),
        ] {
            let saved = serde_json::json!({"torrents":{"0":saved_record(&known), "1":broken}});
            std::fs::write(&path, serde_json::to_vec(&saved)?)?;
            let evidence = inspect_saved_transfer_ownership(root.path()).await;
            assert_eq!(evidence.roots, vec![known.canonicalize()?]);
            assert!(!evidence.roots_complete);
            assert!(evidence.error.is_some());
            assert!(!evidence.has_saved_transfers());
            assert!(saved_transfer_roots(root.path()).await.is_err());
        }
        // Broken non-location fields need not discard complete root coverage.
        let saved = serde_json::json!({"torrents":{"0": {"output_folder":known}}});
        std::fs::write(&path, serde_json::to_vec(&saved)?)?;
        let evidence = inspect_saved_transfer_ownership(root.path()).await;
        assert_eq!(evidence.roots, vec![known.canonicalize()?]);
        assert!(evidence.roots_complete);
        assert!(evidence.error.is_some());
        // One unusable output does not erase valid sibling evidence either.
        let regular = root.path().join("file-instead-of-directory");
        std::fs::write(&regular, b"not a folder")?;
        let saved =
            serde_json::json!({"torrents":{"0":saved_record(&known), "1":saved_record(&regular)}});
        std::fs::write(&path, serde_json::to_vec(&saved)?)?;
        let evidence = inspect_saved_transfer_ownership(root.path()).await;
        assert_eq!(evidence.roots, vec![known.canonicalize()?]);
        assert!(!evidence.roots_complete);
        assert!(evidence.error.is_some());
        Ok(())
    }

    #[tokio::test]
    async fn absent_saved_owner_confirmation_never_starts_or_mutates_a_transfer() -> Result<()> {
        let root = tempfile::tempdir()?;
        let state = root.path().join("state");
        let absent = "1111111111111111111111111111111111111111";
        confirm_saved_owner_absent(&state, absent).await?;
        assert!(!state.exists());
        assert!(confirm_saved_owner_absent(&state, "invalid").await.is_err());
        std::fs::create_dir(&state)?;
        let output = root.path().join("output");
        std::fs::create_dir(&output)?;
        let saved =
            serde_json::to_vec(&serde_json::json!({"torrents":{"0":saved_record(&output)}}))?;
        std::fs::write(state.join("session.json"), &saved)?;
        confirm_saved_owner_absent(&state, absent).await?;
        assert!(
            confirm_saved_owner_absent(&state, "0123456789abcdef0123456789abcdef01234567")
                .await
                .is_err()
        );
        assert_eq!(std::fs::read(state.join("session.json"))?, saved);
        assert_eq!(std::fs::read_dir(&output)?.count(), 0);
        let lock = std::fs::File::open(state.join("session.lock"))?;
        lock.try_lock_exclusive()?;
        assert!(confirm_saved_owner_absent(&state, absent).await.is_err());
        drop(lock);
        std::fs::write(state.join("session.json"), b"corrupt")?;
        assert!(confirm_saved_owner_absent(&state, absent).await.is_err());
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn saved_ownership_canonicalizes_symlink_roots() -> Result<()> {
        let root = tempfile::tempdir()?;
        let folder = root.path().join("actual");
        std::fs::create_dir(&folder)?;
        let alias = root.path().join("alias");
        std::os::unix::fs::symlink(&folder, &alias)?;
        std::fs::write(
            root.path().join("session.json"),
            serde_json::to_vec(&serde_json::json!({"torrents":{"0":saved_record(&alias)}}))?,
        )?;
        assert_eq!(
            saved_transfer_roots(root.path()).await?,
            Some(vec![folder.canonicalize()?])
        );
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dangling_output_symlink_cannot_prove_a_canonical_parent_boundary() -> Result<()> {
        let root = tempfile::tempdir()?;
        let known = root.path().join("known");
        std::fs::create_dir(&known)?;
        let alias = root.path().join("alias");
        std::os::unix::fs::symlink(root.path().join("unavailable-target"), &alias)?;
        let saved = serde_json::json!({"torrents":{"0":saved_record(&known), "1":saved_record(&alias.join("output"))}});
        std::fs::write(
            root.path().join("session.json"),
            serde_json::to_vec(&saved)?,
        )?;
        let evidence = inspect_saved_transfer_ownership(root.path()).await;
        assert!(evidence.error.is_some());
        assert!(!evidence.roots_complete);
        assert_eq!(evidence.roots, vec![known.canonicalize()?]);
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
