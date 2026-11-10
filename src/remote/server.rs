//! The HTTP server: listing, media with ranges, posters, subtitles, hash,
//! and progress. Transcode routes come from [`crate::remote::hls`]; the
//! command routes for downloads, link downloads, and shares come from
//! [`crate::remote::transfers`].
//!
//! The types below are the contract the `flick-serve` binary and the
//! desktop's in-app switch build against.
//!
//! Every route, including unknown paths, sits behind the bearer-token layer,
//! except `GET /v1/hello` and `POST /v1/pair`, which are rate-limited per
//! client address instead. Every error is a JSON [`ErrorBody`]. Media is
//! streamed from disk through a bounded buffer; nothing reads a whole video
//! into memory.

pub use crate::remote::library::Sources;
pub use crate::remote::pairing::PairingCodes;

use crate::remote::{
    auth, cache, hardware,
    hls::{HlsState, ResolvedVideo, Resolver},
    ids::{self, Salt},
    library::{self, Entry},
    pairing::{CodeCheck, RateLimiter},
    protocol::{
        self, Capability, ErrorBody, Hello, Library, PairReply, PairRequest, Pairing, Progress,
        SubtitleFile, TransferRow, Video,
    },
    transcode::{self, Encoder, MediaInfo},
    transfers::{self, TransferBackend, TransferCommand, TransferReply},
};
use anyhow::{Context, Result, bail};
use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::{ConnectInfo, Path as UrlPath, Request, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use futures_util::StreamExt;
use std::{
    collections::HashMap,
    io::SeekFrom,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock, RwLock},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncReadExt, AsyncSeekExt},
    sync::watch,
};
use tokio_util::io::ReaderStream;

/// Read buffer per media response; bounds memory per stream.
const STREAM_BUFFER: usize = 64 * 1024;
/// An unknown id triggers a rebuild at most this often.
const REBUILD_ON_MISS_INTERVAL: Duration = Duration::from_secs(1);
/// How long [`Server::shutdown`] waits for connections to close.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);
/// How long [`Server::shutdown`] lets responses finish before it closes
/// connections still writing, such as one to a client that stopped reading.
const SHUTDOWN_STALLED: Duration = Duration::from_secs(1);
/// Progress older than the saved value by more than this is not applied.
const PROGRESS_CONFLICT_WINDOW: u64 = 60;
const PROGRESS_FILE: &str = "progress.json";
const LOCK_FILE: &str = "serve.lock";
/// How long a busy state folder lock is retried.
const LOCK_WAIT: Duration = Duration::from_secs(2);
/// Transcode job folders live here, inside the state folder.
const TRANSCODE_DIR: &str = "transcode";
/// ffprobe runs at once while the library is being inspected.
const INSPECT_CONCURRENCY: usize = 2;
/// Embedded subtitle extractions (FFmpeg runs) at once.
const EXTRACT_CONCURRENCY: usize = 2;
/// Extracted WebVTT is kept here, inside the state folder.
const SUBTITLE_CACHE_DIR: &str = "subtitles";
/// Sidecar subtitle files larger than this are not sent. Text subtitles are
/// far smaller; a VobSub `.sub` can be hundreds of megabytes.
const MAX_SIDECAR_BYTES: u64 = 16 * 1024 * 1024;
/// A progress `updated` time further ahead of the server's clock than this
/// is taken as now plus this, so a client with a fast clock cannot pin it.
const PROGRESS_CLOCK_SKEW: u64 = 60;
/// How long a read of an unfinished download waits for verified pieces
/// before the media route answers 504, so the player can retry.
const STREAM_WAIT: Duration = Duration::from_secs(30);
/// How long the listing waits for the transfer backend's rows and storage.
const BACKEND_WAIT: Duration = Duration::from_secs(5);
/// Unfinished downloads streamed at once. Each holds a blocking-pool thread
/// while a read waits for pieces; more answer 503 with `Retry-After`.
const MAX_INCOMPLETE_STREAMS: usize = 8;
/// Seconds a client waits before asking for an unfinished stream again.
const INCOMPLETE_STREAM_RETRY: &str = "2";
/// Requests per minute each client address may make to each route that
/// needs no token (`/v1/hello`, `/v1/pair`); more answer 429.
pub const UNAUTHENTICATED_PER_MINUTE: u32 = 30;
/// Written while the server runs: `{"listen": "ip:port", "pid": n}`, for
/// `flick-serve status` and `flick-serve pair-code`. Removed on shutdown.
pub const LISTEN_FILE: &str = "listen";
/// `POST` with the token from this machine only: opens a pairing code and
/// answers `{"pair_code": "123456", "expires_in": seconds}`. Not part of the
/// protocol; `flick-serve pair-code` uses it.
pub const LOCAL_PAIR_CODE_PATH: &str = "/v1/local/pair-code";
/// `POST /v1/pair` while no code is open.
pub const NO_CODE_MESSAGE: &str = "No pairing code is open on that Flick. Open one from its Settings → Remote Libraries, or run `flick-serve pair-code` on that machine.";

#[derive(Clone)]
pub struct ServerConfig {
    /// Must satisfy [`crate::remote::bind::is_allowed`]; port 0 picks a free port.
    pub bind: SocketAddr,
    /// Lowercase hex pairing token, `protocol::TOKEN_BYTES` long.
    pub token: String,
    /// Shown in the client's library as the section name.
    pub name: String,
    pub sources: Sources,
    /// The desktop's poster cache, if any, so listings can offer posters.
    pub poster_cache: Option<PathBuf>,
    /// Where the id salt and transcode temporaries live.
    pub state_dir: PathBuf,
    /// Explicit FFmpeg path; None searches PATH and common locations.
    pub ffmpeg: Option<PathBuf>,
    /// Transcode jobs allowed at once; more get 503.
    pub max_transcodes: usize,
    /// Commands from paired Flicks; None serves read-only.
    pub transfers: Option<crate::remote::transfers::TransferBackend>,
    /// Pairing codes the owner can open; None keeps `POST /v1/pair` closed
    /// (it answers 404) and `hello` reports `pairing_open: false`.
    pub pairing: Option<PairingCodes>,
    /// List this machine's MAC and LAN IPv4 addresses in the listing's
    /// `hardware`, for Wake-on-LAN. Normally true.
    pub advertise_hardware: bool,
}

/// The token is left out, so a logged configuration never reveals it.
impl std::fmt::Debug for ServerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerConfig")
            .field("bind", &self.bind)
            .field("token", &"<redacted>")
            .field("name", &self.name)
            .field("sources", &self.sources)
            .field("poster_cache", &self.poster_cache)
            .field("state_dir", &self.state_dir)
            .field("ffmpeg", &self.ffmpeg)
            .field("max_transcodes", &self.max_transcodes)
            .field(
                "transfers",
                &self.transfers.as_ref().map(TransferBackend::capabilities),
            )
            .field("pairing", &self.pairing)
            .field("advertise_hardware", &self.advertise_hardware)
            .finish()
    }
}

/// A running server. Dropping it does not stop it; call [`Server::shutdown`].
pub struct Server {
    addr: SocketAddr,
    hls: Arc<HlsState>,
    pairing: Option<PairingCodes>,
    /// [`LISTEN_FILE`] in the state folder, removed on shutdown.
    listen_file: PathBuf,
    /// Cancelled to make every open connection's reads and writes fail.
    close_connections: tokio_util::sync::CancellationToken,
    stop: watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
    /// Held while serving so two servers cannot share a state directory.
    _lock: std::fs::File,
}

impl Server {
    pub async fn start(config: ServerConfig) -> Result<Server> {
        if let Some(reason) = crate::remote::bind::refusal(config.bind.ip()) {
            bail!(reason);
        }
        anyhow::ensure!(
            auth::is_valid_token(&config.token),
            "The pairing token must be {} lowercase hex characters",
            protocol::TOKEN_BYTES * 2
        );
        std::fs::create_dir_all(&config.state_dir)
            .with_context(|| format!("Create {}", config.state_dir.display()))?;
        let lock = lock_state_dir(&config.state_dir).await?;
        let salt = Salt::load_or_create(&config.state_dir)?;
        let (stop, stopped) = watch::channel(false);
        // Quality tiers below Original need FFmpeg; without it the server
        // still serves Original and says so in the listing.
        let encoder = transcode::probe(config.ffmpeg.as_deref()).await;
        let media = Arc::new(MediaCache::new(encoder.clone()));
        let index: Arc<RwLock<Arc<Index>>> = Arc::new(RwLock::new(Arc::new(Index::default())));
        let transcode_dir = config.state_dir.join(TRANSCODE_DIR);
        std::fs::create_dir_all(&transcode_dir)
            .with_context(|| format!("Create {}", transcode_dir.display()))?;
        sweep_subtitle_cache(&config.state_dir.join(SUBTITLE_CACHE_DIR));
        let resolver: Resolver = {
            let index = index.clone();
            let media = media.clone();
            Arc::new(move |id: &str| {
                let entry = index
                    .read()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .by_id
                    .get(id)?
                    .clone();
                // A known video stays resolvable while it downloads, without
                // FFmpeg, or when ffprobe fails; the transcoder then explains
                // why it cannot proceed instead of answering as if the video
                // did not exist. A download is not inspected.
                let info = if entry.complete {
                    media.resolve_blocking(&entry.path).unwrap_or_default()
                } else {
                    MediaInfo::default()
                };
                Some(ResolvedVideo {
                    path: entry.path.clone(),
                    info,
                    complete: entry.complete,
                })
            })
        };
        let hls = Arc::new(HlsState::new(
            encoder,
            config.max_transcodes,
            transcode_dir.clone(),
            resolver,
        ));
        // Job folders from an earlier run are gone now; anything else an
        // earlier run left is capped like the subtitle cache.
        if let Err(problem) = cache::prune(&transcode_dir, cache::CACHE_LIMIT_BYTES, |_| false) {
            tracing::warn!(dir = %transcode_dir.display(), "Could not prune the transcode folder: {problem}");
        }
        let state = Arc::new(AppState {
            token: config.token,
            name: config.name,
            sources: config.sources,
            poster_cache: config.poster_cache,
            state_dir: config.state_dir,
            salt,
            index,
            media,
            hls: hls.clone(),
            extracting: tokio::sync::Semaphore::new(EXTRACT_CONCURRENCY),
            streaming: Arc::new(tokio::sync::Semaphore::new(MAX_INCOMPLETE_STREAMS)),
            rebuild: tokio::sync::Mutex::new(()),
            progress: tokio::sync::Mutex::new(()),
            transfers: config.transfers,
            pairing: config.pairing.clone(),
            advertise_hardware: config.advertise_hardware,
            limiter: RateLimiter::new(UNAUTHENTICATED_PER_MINUTE, Duration::from_secs(60)),
            listen: OnceLock::new(),
            stopped,
        });
        // A source that cannot be read at all is a configuration mistake;
        // say so now rather than serving an empty library.
        let rows = state.transfer_rows().await.ok().flatten();
        let entries = library::load(&state.sources, state.poster_cache.as_deref()).await?;
        state.install(entries, rows.as_deref());

        let listener = tokio::net::TcpListener::bind(config.bind)
            .await
            .with_context(|| format!("Listen on {}", config.bind))?;
        let addr = listener.local_addr()?;
        let _ = state.listen.set(addr);
        let close_connections = tokio_util::sync::CancellationToken::new();
        let listener = ClosingListener {
            inner: listener,
            close: close_connections.clone(),
        };
        let app = router(state.clone()).into_make_service_with_connect_info::<Client>();
        let mut stopped = state.stopped.clone();
        let task = tokio::spawn(async move {
            let serve = axum::serve(listener, app).with_graceful_shutdown(async move {
                let _ = stopped.wait_for(|stop| *stop).await;
            });
            if let Err(error) = serve.await {
                tracing::warn!("Remote library server stopped: {error}");
            }
        });
        tracing::info!(%addr, videos = state.current().order.len(), "Remote library server listening");
        let listen_file = state.state_dir.join(LISTEN_FILE);
        let listen = serde_json::json!({ "listen": addr.to_string(), "pid": std::process::id() });
        if let Err(problem) = std::fs::write(&listen_file, listen.to_string()) {
            tracing::warn!("Could not record the listening address: {problem:#}");
        }
        Ok(Server {
            addr,
            hls,
            pairing: config.pairing,
            listen_file,
            close_connections,
            stop,
            task,
            _lock: lock,
        })
    }

    /// The bound address, with the real port when 0 was requested.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// The pairing codes `POST /v1/pair` accepts, as given in
    /// [`ServerConfig::pairing`]; `open()` on it shows the owner a code.
    pub fn pairing(&self) -> Option<&PairingCodes> {
        self.pairing.as_ref()
    }

    /// Stops accepting connections, ends media streams in progress, and
    /// waits briefly for connections to close.
    pub async fn shutdown(self) {
        let _ = std::fs::remove_file(&self.listen_file);
        if let Some(pairing) = &self.pairing {
            pairing.close();
        }
        let _ = self.stop.send(true);
        self.hls.shutdown().await;
        let mut task = self.task;
        // Media streams end as soon as they are polled; a response whose
        // client stopped reading is never polled, so its connection is
        // closed after a moment instead.
        if tokio::time::timeout(SHUTDOWN_STALLED, &mut task)
            .await
            .is_ok()
        {
            return;
        }
        self.close_connections.cancel();
        if tokio::time::timeout(SHUTDOWN_GRACE, &mut task)
            .await
            .is_err()
        {
            task.abort();
            let _ = task.await;
        }
    }
}

/// The client's address, as the router's connect info.
#[derive(Clone, Copy, Debug)]
struct Client(SocketAddr);

impl axum::extract::connect_info::Connected<axum::serve::IncomingStream<'_, ClosingListener>>
    for Client
{
    fn connect_info(stream: axum::serve::IncomingStream<'_, ClosingListener>) -> Self {
        Client(*stream.remote_addr())
    }
}

/// Accepts TCP connections whose reads and writes fail once `close` is
/// cancelled, so shutdown can end a connection that hyper would otherwise
/// keep waiting on.
struct ClosingListener {
    inner: tokio::net::TcpListener,
    close: tokio_util::sync::CancellationToken,
}

impl axum::serve::Listener for ClosingListener {
    type Io = ClosingStream;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (ClosingStream, SocketAddr) {
        let (inner, addr) = axum::serve::Listener::accept(&mut self.inner).await;
        let closed = Box::pin(self.close.clone().cancelled_owned());
        (ClosingStream { inner, closed }, addr)
    }

    fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.inner.local_addr()
    }
}

struct ClosingStream {
    inner: tokio::net::TcpStream,
    closed: std::pin::Pin<Box<tokio_util::sync::WaitForCancellationFutureOwned>>,
}

impl ClosingStream {
    /// An error once the connection is to close. Polling registers the
    /// waker, so a read or write waiting on the socket wakes up for it.
    fn check(&mut self, cx: &mut std::task::Context<'_>) -> std::io::Result<()> {
        if self.closed.as_mut().poll(cx).is_ready() {
            Err(std::io::ErrorKind::ConnectionAborted.into())
        } else {
            Ok(())
        }
    }
}

impl tokio::io::AsyncRead for ClosingStream {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        if let Err(error) = self.check(cx) {
            return std::task::Poll::Ready(Err(error));
        }
        std::pin::Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl tokio::io::AsyncWrite for ClosingStream {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        if let Err(error) = self.check(cx) {
            return std::task::Poll::Ready(Err(error));
        }
        std::pin::Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_write_vectored(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> std::task::Poll<std::io::Result<usize>> {
        if let Err(error) = self.check(cx) {
            return std::task::Poll::Ready(Err(error));
        }
        std::pin::Pin::new(&mut self.inner).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        if let Err(error) = self.check(cx) {
            return std::task::Poll::Ready(Err(error));
        }
        std::pin::Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// Takes the state folder's lock. A lock just released by a server in this
/// process can stay held for a moment while a child process is being
/// spawned elsewhere (it shares the descriptor until it execs), so a busy
/// lock is retried briefly before it is reported.
async fn lock_state_dir(state_dir: &Path) -> Result<std::fs::File> {
    use fs2::FileExt;
    let path = state_dir.join(LOCK_FILE);
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| format!("Open {}", path.display()))?;
    let deadline = Instant::now() + LOCK_WAIT;
    loop {
        match file.try_lock_exclusive() {
            Ok(()) => return Ok(file),
            Err(_) if Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "Another Flick server is already using {}",
                        state_dir.display()
                    )
                });
            }
        }
    }
}

struct AppState {
    token: String,
    name: String,
    sources: Sources,
    poster_cache: Option<PathBuf>,
    state_dir: PathBuf,
    salt: Salt,
    /// Shared with the transcoder's resolver, which runs outside this state.
    index: Arc<RwLock<Arc<Index>>>,
    /// What ffprobe knows about each file; fills the listing and the transcoder.
    media: Arc<MediaCache>,
    hls: Arc<HlsState>,
    /// Bounds embedded subtitle extractions running at once.
    extracting: tokio::sync::Semaphore,
    /// Bounds unfinished downloads streamed at once; a permit lives as long
    /// as its response body.
    streaming: Arc<tokio::sync::Semaphore>,
    /// Serializes rebuilds so concurrent listings do not scan twice at once.
    rebuild: tokio::sync::Mutex<()>,
    /// Serializes progress file updates.
    progress: tokio::sync::Mutex<()>,
    /// Commands from paired Flicks, and the state of unfinished downloads.
    transfers: Option<TransferBackend>,
    pairing: Option<PairingCodes>,
    advertise_hardware: bool,
    /// Bounds requests to the routes that need no token.
    limiter: RateLimiter,
    /// The bound address, set once the listener exists.
    listen: OnceLock<SocketAddr>,
    stopped: watch::Receiver<bool>,
}

#[derive(Default)]
struct Index {
    by_id: HashMap<String, Arc<Entry>>,
    order: Vec<String>,
    built: Option<Instant>,
}

impl AppState {
    fn current(&self) -> Arc<Index> {
        self.index
            .read()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
    }

    /// Replaces the index with `entries`, adjusted by the transfer
    /// backend's `rows` when there are any (see [`overlay_transfers`]).
    fn install(&self, entries: Vec<Entry>, rows: Option<&[TransferRow]>) {
        let mut index = Index {
            built: Some(Instant::now()),
            ..Index::default()
        };
        for entry in entries {
            let id = ids::video_id(&self.salt, &entry.path);
            if index.by_id.insert(id.clone(), Arc::new(entry)).is_none() {
                index.order.push(id);
            }
        }
        if let Some(rows) = rows {
            overlay_transfers(&mut index, rows, self.allows(Capability::StreamIncomplete));
        }
        *self
            .index
            .write()
            .unwrap_or_else(|poison| poison.into_inner()) = Arc::new(index);
        self.media.prefetch(self.current(), self.stopped.clone());
    }

    /// Rebuilds the index from the sources. A failed read keeps the last
    /// good index, so a settings file caught mid-write does not empty the
    /// library.
    ///
    /// With a transfer backend that runs downloads, its rows decide which
    /// files are still downloading. When it does not answer, the previous
    /// index is kept, so a partial file is never served as if it were
    /// complete. A backend that is switched off (no `downloads` capability,
    /// or a 403) has no rows: the sources alone decide, and the in-app
    /// server's source marks unverified torrent files itself.
    async fn refresh(&self) -> Arc<Index> {
        let _guard = self.rebuild.lock().await;
        let Ok(rows) = self.transfer_rows().await else {
            tracing::warn!("Keeping the previous library: the transfer backend did not answer");
            return self.current();
        };
        match library::load(&self.sources, self.poster_cache.as_deref()).await {
            Ok(entries) => self.install(entries, rows.as_deref()),
            Err(error) => tracing::warn!("Keeping the previous library: {error:#}"),
        }
        self.current()
    }

    fn allows(&self, capability: Capability) -> bool {
        self.transfers
            .as_ref()
            .is_some_and(|backend| backend.allows(capability))
    }

    /// The backend's transfers when it runs downloads; `Ok(None)` without
    /// one or when it is switched off (403), `Err` when it does not answer
    /// in time or fails.
    async fn transfer_rows(&self) -> Result<Option<Vec<TransferRow>>, ()> {
        let Some(backend) = self
            .transfers
            .as_ref()
            .filter(|backend| backend.allows(Capability::Downloads))
        else {
            return Ok(None);
        };
        match tokio::time::timeout(BACKEND_WAIT, backend.call(TransferCommand::List)).await {
            Ok(Ok(TransferReply::Transfers(rows))) => Ok(Some(rows)),
            Ok(Err(error)) if error.status == 403 => Ok(None),
            Ok(Err(error)) => {
                tracing::warn!("The transfer backend refused its list: {}", error.message);
                Err(())
            }
            Ok(Ok(_)) | Err(_) => Err(()),
        }
    }

    /// Free space in the download folder, when the backend accepts downloads.
    async fn storage(&self) -> Option<protocol::Storage> {
        let backend = self
            .transfers
            .as_ref()
            .filter(|backend| backend.allows(Capability::Downloads))?;
        match tokio::time::timeout(BACKEND_WAIT, backend.call(TransferCommand::Storage)).await {
            Ok(Ok(TransferReply::Storage(storage))) => Some(storage),
            _ => None,
        }
    }

    async fn lookup(&self, id: &str) -> Option<Arc<Entry>> {
        if !ids::is_valid_id(id) {
            return None;
        }
        let index = self.current();
        if let Some(entry) = index.by_id.get(id) {
            return Some(entry.clone());
        }
        let stale = index
            .built
            .is_none_or(|built| built.elapsed() >= REBUILD_ON_MISS_INTERVAL);
        if !stale {
            return None;
        }
        self.refresh().await.by_id.get(id).cloned()
    }

    /// Resolves when the server is shutting down.
    fn stopping(&self) -> impl Future<Output = ()> + Send + 'static {
        let mut stopped = self.stopped.clone();
        async move {
            let _ = stopped.wait_for(|stop| *stop).await;
        }
    }
}

/// Marks the selected files of transfers that are not fully verified as
/// incomplete, with their verified fraction and torrent identity. A file is
/// matched by its listing id (`TransferFileRow::video_id`) or, for the
/// desktop's library, by the torrent identity the library saved. With
/// `stream_incomplete`, a downloading video that is not in the sources yet
/// (not on disk, or in a library that lists only finished files) is added
/// with a display-only relative path, playable only through the transfer.
fn overlay_transfers(index: &mut Index, rows: &[TransferRow], stream_incomplete: bool) {
    let by_torrent: HashMap<library::TorrentFile, String> = index
        .by_id
        .iter()
        .filter_map(|(id, entry)| Some((entry.torrent.clone()?, id.clone())))
        .collect();
    for row in rows {
        let verifying = matches!(
            row.phase,
            protocol::TransferPhase::Checking | protocol::TransferPhase::Error
        );
        for file in row.files.iter().filter(|file| file.selected) {
            let done = !verifying && file.verified_bytes >= file.size;
            let progress = if file.size == 0 {
                if done { 1.0 } else { 0.0 }
            } else {
                (file.verified_bytes as f64 / file.size as f64).clamp(0.0, 1.0)
            };
            let torrent = library::TorrentFile {
                infohash: row.id.clone(),
                index: file.index,
            };
            let found = file
                .video_id
                .as_ref()
                .filter(|id| index.by_id.contains_key(*id))
                .cloned()
                .or_else(|| by_torrent.get(&torrent).cloned());
            match found {
                Some(id) => {
                    let Some(entry) = index.by_id.get_mut(&id) else {
                        continue;
                    };
                    let entry = Arc::make_mut(entry);
                    entry.torrent = Some(torrent);
                    if done {
                        if !entry.complete {
                            entry.progress = progress;
                        }
                    } else {
                        entry.complete = false;
                        entry.progress = progress.min(0.999_999);
                        entry.size = Some(file.size);
                    }
                }
                None if !done && stream_incomplete => {
                    let Some(id) = file.video_id.as_ref().filter(|id| ids::is_valid_id(id)) else {
                        continue;
                    };
                    let path = PathBuf::from(&file.path);
                    if path.is_absolute()
                        || path
                            .components()
                            .any(|part| !matches!(part, std::path::Component::Normal(_)))
                        || !crate::library::is_video(&path)
                    {
                        continue;
                    }
                    let mut entry = Entry::file(path.clone(), library::file_title(&path));
                    entry.complete = false;
                    entry.progress = progress.min(0.999_999);
                    entry.torrent = Some(torrent);
                    entry.size = Some(file.size);
                    if index.by_id.insert(id.clone(), Arc::new(entry)).is_none() {
                        index.order.push(id.clone());
                    }
                }
                None => {}
            }
        }
    }
}

fn router(state: Arc<AppState>) -> Router {
    // Enough to recognise this Flick and pair with a code; nothing else is
    // reachable without the token.
    let open = Router::new()
        .route("/v1/hello", get(hello))
        .route("/v1/pair", post(pair))
        .method_not_allowed_fallback(method_not_allowed)
        .layer(middleware::from_fn_with_state(
            state.clone(),
            limit_unauthenticated,
        ))
        .with_state(state.clone());
    Router::new()
        .route("/v1/library", get(listing))
        .route("/v1/media/{id}", get(media).head(media))
        .route("/v1/poster/{id}", get(poster))
        .route("/v1/subtitles/{id}", get(subtitles))
        .route("/v1/subtitles/{id}/{index}", get(subtitle))
        .route(
            "/v1/subtitles/{id}/embedded/{track}",
            get(embedded_subtitle),
        )
        .route("/v1/hash/{id}", get(hash))
        .route("/v1/progress/{id}", get(get_progress).put(put_progress))
        .route(LOCAL_PAIR_CODE_PATH, post(open_pair_code))
        .with_state(state.clone())
        .merge(crate::remote::hls::router(state.hls.clone()))
        .merge(transfers::router(state.transfers.clone()))
        .fallback(no_route)
        .method_not_allowed_fallback(method_not_allowed)
        .layer(middleware::from_fn_with_state(state, require_token))
        .merge(open)
}

/// The client's address, from the connection.
fn peer(request: &Request) -> Option<SocketAddr> {
    request
        .extensions()
        .get::<ConnectInfo<Client>>()
        .map(|ConnectInfo(Client(addr))| *addr)
}

/// [`UNAUTHENTICATED_PER_MINUTE`] per client address and route; more answer
/// 429 with `Retry-After`.
async fn limit_unauthenticated(
    State(state): State<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Response {
    let route = if request.uri().path() == "/v1/pair" {
        "pair"
    } else {
        "hello"
    };
    let ip = peer(&request).map_or(std::net::IpAddr::from([0, 0, 0, 0]), |addr| addr.ip());
    match state.limiter.check(ip, route) {
        Ok(()) => next.run(request).await,
        Err(wait) => {
            let mut response = error(
                StatusCode::TOO_MANY_REQUESTS,
                "Too many requests from this address. Wait a minute and try again.",
            );
            if let Ok(value) = HeaderValue::from_str(&wait.as_secs().max(1).to_string()) {
                response.headers_mut().insert(header::RETRY_AFTER, value);
            }
            response
        }
    }
}

/// `GET /v1/hello`: the protocol, the name, and whether a code is open.
async fn hello(State(state): State<Arc<AppState>>) -> Response {
    Json(Hello {
        protocol: protocol::VERSION,
        server_name: state.name.clone(),
        pairing_open: state.pairing.as_ref().is_some_and(PairingCodes::is_open),
    })
    .into_response()
}

/// `POST /v1/pair` `{"code"}`: the pairing string when the code matches.
/// 404 while no code is open, 403 for a wrong code (the fifth closes it).
async fn pair(State(state): State<Arc<AppState>>, request: Request) -> Response {
    let from = peer(&request);
    let Some(codes) = state.pairing.as_ref().filter(|codes| codes.is_open()) else {
        return error(StatusCode::NOT_FOUND, NO_CODE_MESSAGE);
    };
    let Ok(body) = axum::body::to_bytes(request.into_body(), 4096).await else {
        return error(StatusCode::BAD_REQUEST, "The pairing request is too large");
    };
    let Ok(PairRequest { code }) = serde_json::from_slice::<PairRequest>(&body) else {
        return error(
            StatusCode::BAD_REQUEST,
            "Send the code as JSON: {\"code\": \"123456\"}",
        );
    };
    match codes.check(&code) {
        CodeCheck::Matched => {
            let Some(listen) = state.listen.get() else {
                return error(StatusCode::SERVICE_UNAVAILABLE, "The server is starting");
            };
            tracing::info!(peer = ?from.map(|addr| addr.ip()), "Paired a Flick with a pairing code");
            let pairing = Pairing {
                host: listen.ip().to_string(),
                port: listen.port(),
                token: state.token.clone(),
            };
            Json(PairReply {
                pairing: pairing.to_string(),
            })
            .into_response()
        }
        CodeCheck::Wrong { attempts_left } => error(
            StatusCode::FORBIDDEN,
            format!(
                "That code is not the one shown on the server. {attempts_left} attempt{} left before the code is closed.",
                if attempts_left == 1 { "" } else { "s" }
            ),
        ),
        CodeCheck::Exhausted => error(
            StatusCode::FORBIDDEN,
            "That code is not the one shown on the server, and that was the last attempt. The code is closed; open a new one on the server.",
        ),
        CodeCheck::NotOpen => error(StatusCode::NOT_FOUND, NO_CODE_MESSAGE),
    }
}

/// [`LOCAL_PAIR_CODE_PATH`]: opens a code for `flick-serve pair-code`. Only
/// from this machine (loopback, or this server's own address), with the
/// token like every other route.
async fn open_pair_code(State(state): State<Arc<AppState>>, request: Request) -> Response {
    let local = peer(&request).is_some_and(|addr| {
        addr.ip().is_loopback() || state.listen.get().is_some_and(|own| own.ip() == addr.ip())
    });
    if !local {
        return error(
            StatusCode::FORBIDDEN,
            "Pairing codes can only be opened on the server itself.",
        );
    }
    let Some(codes) = &state.pairing else {
        return error(
            StatusCode::NOT_FOUND,
            "This server does not accept pairing codes.",
        );
    };
    let code = codes.open();
    let expires_in = codes.remaining().map_or(0, |left| left.as_secs());
    Json(serde_json::json!({ "pair_code": code, "expires_in": expires_in })).into_response()
}

async fn require_token(
    State(state): State<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Response {
    let presented = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(auth::bearer_token);
    match presented {
        Some(token) if auth::token_matches(&state.token, token) => next.run(request).await,
        _ => {
            let mut response = error(
                StatusCode::UNAUTHORIZED,
                "This Flick is not paired with the server. Pair again with the server's pairing string.",
            );
            response
                .headers_mut()
                .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
            response
        }
    }
}

fn error(status: StatusCode, message: impl Into<String>) -> Response {
    (
        status,
        Json(ErrorBody {
            message: message.into(),
        }),
    )
        .into_response()
}

fn unknown_video() -> Response {
    error(
        StatusCode::NOT_FOUND,
        "The server has no video with this id. Refresh the library.",
    )
}

fn incomplete_video() -> Response {
    error(StatusCode::CONFLICT, crate::remote::hls::INCOMPLETE_MESSAGE)
}

async fn no_route() -> Response {
    error(StatusCode::NOT_FOUND, "Not found")
}

async fn method_not_allowed() -> Response {
    error(StatusCode::METHOD_NOT_ALLOWED, "Method not allowed")
}

async fn listing(State(state): State<Arc<AppState>>) -> Response {
    let index = state.refresh().await;
    let storage = state.storage().await;
    let media = state.media.clone();
    let mut saved = {
        let _guard = state.progress.lock().await;
        read_progress(&progress_path(&state)).await
    };
    let advertise_hardware = state.advertise_hardware;
    let described = tokio::task::spawn_blocking(move || {
        let videos = index
            .order
            .iter()
            .filter_map(|id| Some(describe(id, index.by_id.get(id)?, &media, saved.remove(id))))
            .collect::<Vec<_>>();
        let hardware = advertise_hardware
            .then(hardware::local)
            .filter(|found| !found.mac.is_empty() || !found.lan_ips.is_empty());
        (videos, hardware)
    })
    .await;
    match described {
        Ok((videos, hardware)) => Json(Library {
            protocol: protocol::VERSION,
            server_name: state.name.clone(),
            hardware,
            qualities: state.hls.qualities(),
            capabilities: state
                .transfers
                .as_ref()
                .map(TransferBackend::capabilities)
                .unwrap_or_default(),
            storage,
            videos,
        })
        .into_response(),
        Err(_) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "The library could not be listed",
        ),
    }
}

/// The listing entry for a video. What the source knows (the desktop's
/// saved duration and size) wins; ffprobe's view fills the gaps once the
/// file has been inspected. `resume` is the progress saved here, if any.
fn describe(id: &str, entry: &Entry, media: &MediaCache, resume: Option<Progress>) -> Video {
    // A file still downloading is not inspected: ffprobe would read
    // unverified bytes, and a display-only path is not on disk at all.
    let info = if entry.complete && entry.on_disk() {
        media.get(&entry.path)
    } else {
        None
    };
    let or_probed = |saved: f64, probed: fn(&MediaInfo) -> f64| {
        if saved > 0.0 {
            saved
        } else {
            info.as_ref().map_or(0.0, probed)
        }
    };
    let or_probed_u32 = |saved: u32, probed: fn(&MediaInfo) -> u32| {
        if saved > 0 {
            saved
        } else {
            info.as_ref().map_or(0, probed)
        }
    };
    Video {
        id: id.to_owned(),
        title: entry.title.clone(),
        file_name: library::file_title(&entry.path),
        size: entry.size.unwrap_or_else(|| {
            if entry.on_disk() {
                std::fs::metadata(&entry.path).map_or(0, |m| m.len())
            } else {
                0
            }
        }),
        duration: or_probed(entry.duration, |info| info.duration),
        width: or_probed_u32(entry.width, |info| info.width),
        height: or_probed_u32(entry.height, |info| info.height),
        mime: library::video_mime(&entry.path).to_owned(),
        episode: crate::library::parse_episode(&entry.path).map(|episode| protocol::Episode {
            show: episode.show,
            season: episode.season,
            number: episode.episode,
        }),
        complete: entry.complete,
        progress: if entry.complete { 1.0 } else { entry.progress },
        poster: entry.poster.is_some(),
        subtitles: subtitle_files(&entry.path),
        audio_tracks: info
            .as_ref()
            .map(|info| info.audio.clone())
            .unwrap_or_default(),
        embedded_subtitles: info.map(|info| info.subtitles).unwrap_or_default(),
        resume,
    }
}

/// ffprobe results per file, checked against size and modification time,
/// so the listing and the transcoder describe the same bytes. Without
/// FFmpeg nothing is inspected and the listing shows what the source saved.
struct MediaCache {
    encoder: Option<Encoder>,
    entries: Mutex<HashMap<PathBuf, (Stamp, MediaInfo)>>,
    inspecting: tokio::sync::Semaphore,
}

type Stamp = (u64, Option<SystemTime>);

impl MediaCache {
    fn new(encoder: Option<Encoder>) -> Self {
        Self {
            encoder,
            entries: Mutex::new(HashMap::new()),
            inspecting: tokio::sync::Semaphore::new(INSPECT_CONCURRENCY),
        }
    }

    fn stamp(path: &Path) -> Option<Stamp> {
        let metadata = std::fs::metadata(path).ok()?;
        Some((metadata.len(), metadata.modified().ok()))
    }

    /// What is known about a file right now, if it has not changed since.
    fn get(&self, path: &Path) -> Option<MediaInfo> {
        let stamp = Self::stamp(path)?;
        let entries = self
            .entries
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        entries
            .get(path)
            .filter(|(saved, _)| *saved == stamp)
            .map(|(_, info)| info.clone())
    }

    /// The cached result, or one ffprobe run. None without FFmpeg, or when
    /// the file cannot be read or inspected.
    async fn inspect(&self, path: &Path) -> Option<MediaInfo> {
        if let Some(info) = self.get(path) {
            return Some(info);
        }
        let encoder = self.encoder.as_ref()?;
        let _permit = self.inspecting.acquire().await.ok()?;
        if let Some(info) = self.get(path) {
            return Some(info);
        }
        let stamp = Self::stamp(path)?;
        match transcode::inspect(encoder, path).await {
            Ok(info) => {
                self.entries
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .insert(path.to_owned(), (stamp, info.clone()));
                Some(info)
            }
            Err(error) => {
                tracing::warn!(video = %path.display(), "Could not inspect the video: {error:#}");
                None
            }
        }
    }

    /// [`Self::inspect`] for the transcoder's synchronous resolver. On a
    /// multi-threaded runtime an uninspected file is probed in place; on a
    /// single-threaded one only the cache answers.
    fn resolve_blocking(&self, path: &Path) -> Option<MediaInfo> {
        if let Some(info) = self.get(path) {
            return Some(info);
        }
        let handle = tokio::runtime::Handle::try_current().ok()?;
        if handle.runtime_flavor() != tokio::runtime::RuntimeFlavor::MultiThread {
            return None;
        }
        tokio::task::block_in_place(|| handle.block_on(self.inspect(path)))
    }

    /// Inspects every listed file in the background, a few at a time, so
    /// durations, sizes, and embedded subtitles reach the listing and a
    /// transcode can start without waiting for ffprobe.
    fn prefetch(self: &Arc<Self>, index: Arc<Index>, stopped: watch::Receiver<bool>) {
        if self.encoder.is_none() {
            return;
        }
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let cache = self.clone();
        handle.spawn(async move {
            for id in &index.order {
                if *stopped.borrow() {
                    break;
                }
                let Some(entry) = index.by_id.get(id) else {
                    continue;
                };
                if !entry.complete || !entry.on_disk() {
                    continue;
                }
                if cache.get(&entry.path).is_none() {
                    cache.inspect(&entry.path).await;
                }
            }
        });
    }
}

fn subtitle_files(video: &Path) -> Vec<SubtitleFile> {
    library::sidecar_subtitles(video)
        .into_iter()
        .enumerate()
        .map(|(index, sidecar)| SubtitleFile {
            index: index as u32,
            file_name: sidecar.file_name,
            language: sidecar.language,
        })
        .collect()
}

/// A single byte range, inclusive at both ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ByteRange {
    pub start: u64,
    pub end: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub enum RangeRequest {
    /// No range, or a unit other than bytes: send the whole file.
    Full,
    Partial(ByteRange),
    /// Several ranges, malformed, or outside the file: 416.
    Unsatisfiable,
}

/// Parses a `Range` header against a file of `size` bytes. Only one range
/// is served; several ranges are refused rather than answered as multipart.
pub fn parse_range(value: &str, size: u64) -> RangeRequest {
    let value = value.trim();
    let Some(spec) = value
        .get(..6)
        .filter(|unit| unit.eq_ignore_ascii_case("bytes="))
        .map(|_| &value[6..])
    else {
        return RangeRequest::Full;
    };
    if spec.contains(',') {
        return RangeRequest::Unsatisfiable;
    }
    let Some((first, last)) = spec.split_once('-') else {
        return RangeRequest::Unsatisfiable;
    };
    let (first, last) = (first.trim(), last.trim());
    let number = |text: &str| {
        (!text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()))
            .then(|| text.parse::<u64>().ok())
            .flatten()
    };
    let range = if first.is_empty() {
        match number(last) {
            Some(suffix) if suffix > 0 && size > 0 => Some(ByteRange {
                start: size.saturating_sub(suffix),
                end: size - 1,
            }),
            _ => None,
        }
    } else {
        match (
            number(first),
            last.is_empty().then_some(u64::MAX).or_else(|| number(last)),
        ) {
            (Some(start), Some(end)) if start <= end && start < size => Some(ByteRange {
                start,
                end: end.min(size - 1),
            }),
            _ => None,
        }
    };
    range.map_or(RangeRequest::Unsatisfiable, RangeRequest::Partial)
}

fn etag(metadata: &std::fs::Metadata) -> String {
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |time| time.as_nanos());
    format!("\"{:x}-{modified:x}\"", metadata.len())
}

async fn media(
    State(state): State<Arc<AppState>>,
    UrlPath(id): UrlPath<String>,
    method: Method,
    headers: HeaderMap,
) -> Response {
    let Some(entry) = state.lookup(&id).await else {
        return unknown_video();
    };
    if !entry.complete {
        return match (&entry.torrent, &state.transfers) {
            (Some(torrent), Some(backend)) if backend.allows(Capability::StreamIncomplete) => {
                stream_incomplete(&state, backend, &entry, torrent, method, &headers).await
            }
            _ => incomplete_video(),
        };
    }
    let Ok(mut file) = tokio::fs::File::open(&entry.path).await else {
        return error(
            StatusCode::NOT_FOUND,
            "The video's file is no longer on the server",
        );
    };
    let Ok(metadata) = file.metadata().await else {
        return error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Read the video's details",
        );
    };
    let size = metadata.len();
    let tag = etag(&metadata);
    // If-Range: serve the range only when the client's copy is still this
    // exact file. Dates and weak tags never match, so they get the whole file.
    let range_applies = headers
        .get(header::IF_RANGE)
        .is_none_or(|value| value.to_str().is_ok_and(|value| value.trim() == tag));
    let request = match headers.get(header::RANGE) {
        Some(value) if range_applies => match value.to_str() {
            Ok(value) => parse_range(value, size),
            Err(_) => RangeRequest::Unsatisfiable,
        },
        _ => RangeRequest::Full,
    };
    let mut response = Response::builder()
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::ETAG, &tag)
        .header(header::CACHE_CONTROL, "private, no-transform");
    let range = match request {
        RangeRequest::Unsatisfiable => {
            let mut refused = error(
                StatusCode::RANGE_NOT_SATISFIABLE,
                "The server sends one byte range inside the file",
            );
            let headers = refused.headers_mut();
            headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
            if let Ok(value) = HeaderValue::from_str(&format!("bytes */{size}")) {
                headers.insert(header::CONTENT_RANGE, value);
            }
            return refused;
        }
        RangeRequest::Full => {
            response = response.status(StatusCode::OK);
            ByteRange {
                start: 0,
                end: size.saturating_sub(1),
            }
        }
        RangeRequest::Partial(range) => {
            response = response.status(StatusCode::PARTIAL_CONTENT).header(
                header::CONTENT_RANGE,
                format!("bytes {}-{}/{size}", range.start, range.end),
            );
            range
        }
    };
    let length = if size == 0 {
        0
    } else {
        range.end - range.start + 1
    };
    response = response
        .header(header::CONTENT_TYPE, library::video_mime(&entry.path))
        .header(header::CONTENT_LENGTH, length);
    let body = if method == Method::HEAD || length == 0 {
        Body::empty()
    } else {
        if range.start > 0 && file.seek(SeekFrom::Start(range.start)).await.is_err() {
            return error(StatusCode::INTERNAL_SERVER_ERROR, "Read the video");
        }
        let stream = ReaderStream::with_capacity(file.take(length), STREAM_BUFFER)
            .take_until(state.stopping());
        Body::from_stream(stream)
    };
    response
        .body(body)
        .unwrap_or_else(|_| error(StatusCode::INTERNAL_SERVER_ERROR, "Build the response"))
}

fn timed_out_waiting() -> Response {
    error(
        StatusCode::GATEWAY_TIMEOUT,
        "The server has not downloaded this part of the video yet. Try again shortly.",
    )
}

/// A range of a torrent file that is still downloading, read through the
/// backend's verified-pieces stream: only verified bytes are sent. Opening
/// the stream, seeking, and the first read must finish within
/// [`STREAM_WAIT`], or the answer is 504 so the player can retry; a later
/// read that waits that long ends the response. The response has the full
/// file's `Content-Length` for its range and no `ETag`: verified bytes never
/// change. A client that goes away, or the server stopping, cancels the
/// waiting read and releases the stream.
///
/// At most [`MAX_INCOMPLETE_STREAMS`] run at once, so waiting reads cannot
/// take every blocking thread; another answers 503 with `Retry-After`.
async fn stream_incomplete(
    state: &AppState,
    backend: &TransferBackend,
    entry: &Entry,
    torrent: &library::TorrentFile,
    method: Method,
    headers: &HeaderMap,
) -> Response {
    let Ok(permit) = state.streaming.clone().try_acquire_owned() else {
        let mut busy = error(
            StatusCode::SERVICE_UNAVAILABLE,
            "The server is already streaming several unfinished downloads. Try again shortly.",
        );
        busy.headers_mut().insert(
            header::RETRY_AFTER,
            HeaderValue::from_static(INCOMPLETE_STREAM_RETRY),
        );
        return busy;
    };
    let opened = tokio::time::timeout(
        STREAM_WAIT,
        backend.call(TransferCommand::OpenStream {
            id: torrent.infohash.clone(),
            file_index: torrent.index,
        }),
    )
    .await;
    let stream = match opened {
        Err(_) => return timed_out_waiting(),
        Ok(Err(refusal)) => return transfers::error_response(&refusal),
        Ok(Ok(TransferReply::Stream(stream))) => stream,
        Ok(Ok(_)) => {
            return error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "The transfer backend gave an unexpected answer",
            );
        }
    };
    let size = stream.len();
    let request = match headers.get(header::RANGE) {
        Some(value) => match value.to_str() {
            Ok(value) => parse_range(value, size),
            Err(_) => RangeRequest::Unsatisfiable,
        },
        None => RangeRequest::Full,
    };
    let mut response = Response::builder()
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CACHE_CONTROL, "private, no-transform");
    let range = match request {
        RangeRequest::Unsatisfiable => {
            let mut refused = error(
                StatusCode::RANGE_NOT_SATISFIABLE,
                "The server sends one byte range inside the file",
            );
            let headers = refused.headers_mut();
            headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
            if let Ok(value) = HeaderValue::from_str(&format!("bytes */{size}")) {
                headers.insert(header::CONTENT_RANGE, value);
            }
            return refused;
        }
        RangeRequest::Full => {
            response = response.status(StatusCode::OK);
            ByteRange {
                start: 0,
                end: size.saturating_sub(1),
            }
        }
        RangeRequest::Partial(range) => {
            response = response.status(StatusCode::PARTIAL_CONTENT).header(
                header::CONTENT_RANGE,
                format!("bytes {}-{}/{size}", range.start, range.end),
            );
            range
        }
    };
    let length = if size == 0 {
        0
    } else {
        range.end - range.start + 1
    };
    response = response
        .header(header::CONTENT_TYPE, library::video_mime(&entry.path))
        .header(header::CONTENT_LENGTH, length);
    if method == Method::HEAD || length == 0 {
        return response
            .body(Body::empty())
            .unwrap_or_else(|_| error(StatusCode::INTERNAL_SERVER_ERROR, "Build the response"));
    }
    let reader = stream.into_blocking(&tokio::runtime::Handle::current());
    let cancel = reader.canceller();
    // Cancels a waiting read whenever this function or the body is dropped.
    let guard = cancel.clone().drop_guard();
    let start = range.start;
    let first_len = length.min(STREAM_BUFFER as u64) as usize;
    let first = tokio::time::timeout(
        STREAM_WAIT,
        tokio::task::spawn_blocking(move || {
            let mut reader = reader;
            if reader.seek(start)? != start {
                return Err(std::io::Error::other("The seek landed elsewhere"));
            }
            let mut buffer = vec![0; first_len];
            let read = reader.read(&mut buffer)?;
            if read == 0 {
                return Err(std::io::ErrorKind::UnexpectedEof.into());
            }
            buffer.truncate(read);
            Ok((reader, buffer))
        }),
    )
    .await;
    let (reader, first) = match first {
        Err(_) => {
            cancel.cancel();
            return timed_out_waiting();
        }
        Ok(Ok(Ok(read))) => read,
        Ok(Ok(Err(problem))) => {
            return error(
                StatusCode::BAD_GATEWAY,
                format!("The download could not be read: {problem}"),
            );
        }
        Ok(Err(_)) => return error(StatusCode::INTERNAL_SERVER_ERROR, "Read the download"),
    };
    let remaining = length - first.len() as u64;
    // The permit and the cancel guard live as long as the body.
    let guard = (guard, permit);
    let rest = futures_util::stream::unfold(
        (Some(reader), remaining, guard, cancel),
        |(reader, remaining, guard, cancel)| async move {
            let mut reader = reader?;
            if remaining == 0 {
                return None;
            }
            let want = remaining.min(STREAM_BUFFER as u64) as usize;
            let read = tokio::time::timeout(
                STREAM_WAIT,
                tokio::task::spawn_blocking(move || {
                    let mut buffer = vec![0; want];
                    let read = reader.read(&mut buffer);
                    (reader, read, buffer)
                }),
            )
            .await;
            match read {
                Ok(Ok((reader, Ok(read), mut buffer))) if read > 0 => {
                    buffer.truncate(read);
                    let left = remaining - read as u64;
                    Some((Ok(Bytes::from(buffer)), (Some(reader), left, guard, cancel)))
                }
                Ok(Ok((_, Ok(_), _))) => Some((
                    Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof)),
                    (None, 0, guard, cancel),
                )),
                Ok(Ok((_, Err(problem), _))) => Some((Err(problem), (None, 0, guard, cancel))),
                Ok(Err(_)) => Some((
                    Err(std::io::Error::other("The read stopped")),
                    (None, 0, guard, cancel),
                )),
                Err(_) => {
                    cancel.cancel();
                    Some((
                        Err(std::io::Error::from(std::io::ErrorKind::TimedOut)),
                        (None, 0, guard, cancel),
                    ))
                }
            }
        },
    );
    let body = futures_util::stream::once(std::future::ready(Ok::<_, std::io::Error>(
        Bytes::from(first),
    )))
    .chain(rest)
    .take_until(state.stopping());
    response
        .body(Body::from_stream(body))
        .unwrap_or_else(|_| error(StatusCode::INTERNAL_SERVER_ERROR, "Build the response"))
}

async fn poster(State(state): State<Arc<AppState>>, UrlPath(id): UrlPath<String>) -> Response {
    let Some(entry) = state.lookup(&id).await else {
        return unknown_video();
    };
    let Some(path) = &entry.poster else {
        return error(StatusCode::NOT_FOUND, "This video has no poster yet");
    };
    match tokio::fs::read(path).await {
        Ok(bytes) => ([(header::CONTENT_TYPE, "image/png")], bytes).into_response(),
        Err(_) => error(StatusCode::NOT_FOUND, "This video has no poster yet"),
    }
}

async fn subtitles(State(state): State<Arc<AppState>>, UrlPath(id): UrlPath<String>) -> Response {
    let Some(entry) = state.lookup(&id).await else {
        return unknown_video();
    };
    match tokio::task::spawn_blocking(move || subtitle_files(&entry.path)).await {
        Ok(files) => Json(files).into_response(),
        Err(_) => error(StatusCode::INTERNAL_SERVER_ERROR, "List subtitles"),
    }
}

/// A sidecar subtitle file, sent as is. Only a regular file of at most
/// [`MAX_SIDECAR_BYTES`] is sent; a larger one answers 413 with a message.
/// The listing already skips symbolic links; the file is opened without
/// following one (on Unix) and checked again after opening, so a file
/// swapped for a link after the listing is not followed.
async fn subtitle(
    State(state): State<Arc<AppState>>,
    UrlPath((id, index)): UrlPath<(String, String)>,
) -> Response {
    let Some(entry) = state.lookup(&id).await else {
        return unknown_video();
    };
    let Ok(index) = index.parse::<usize>() else {
        return error(StatusCode::NOT_FOUND, "No subtitle file with this index");
    };
    let path = entry.path.clone();
    let sidecars = tokio::task::spawn_blocking(move || library::sidecar_subtitles(&path))
        .await
        .unwrap_or_default();
    let Some(sidecar) = sidecars.into_iter().nth(index) else {
        return error(StatusCode::NOT_FOUND, "No subtitle file with this index");
    };
    let path = sidecar.path.clone();
    match tokio::task::spawn_blocking(move || read_sidecar(&path)).await {
        Ok(Ok(bytes)) => (
            [(
                header::CONTENT_TYPE,
                library::subtitle_mime(&sidecar.extension),
            )],
            bytes,
        )
            .into_response(),
        Ok(Err(SidecarError::TooLarge)) => error(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!(
                "This subtitle file is larger than {} MiB, so the server does not send it",
                MAX_SIDECAR_BYTES / (1024 * 1024)
            ),
        ),
        Ok(Err(SidecarError::Gone)) | Err(_) => {
            error(StatusCode::NOT_FOUND, "The subtitle file is gone")
        }
    }
}

enum SidecarError {
    TooLarge,
    /// Missing, unreadable, or not a regular file.
    Gone,
}

fn read_sidecar(path: &Path) -> Result<Vec<u8>, SidecarError> {
    use std::io::Read;
    let check = |metadata: &std::fs::Metadata| {
        if !metadata.is_file() {
            Err(SidecarError::Gone)
        } else if metadata.len() > MAX_SIDECAR_BYTES {
            Err(SidecarError::TooLarge)
        } else {
            Ok(())
        }
    };
    check(&std::fs::symlink_metadata(path).map_err(|_| SidecarError::Gone)?)?;
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(path).map_err(|_| SidecarError::Gone)?;
    check(&file.metadata().map_err(|_| SidecarError::Gone)?)?;
    // The file may still grow after the check.
    let mut bytes = Vec::new();
    file.take(MAX_SIDECAR_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| SidecarError::Gone)?;
    if bytes.len() as u64 > MAX_SIDECAR_BYTES {
        return Err(SidecarError::TooLarge);
    }
    Ok(bytes)
}

/// A subtitle stream inside the file, extracted as WebVTT by FFmpeg. At most
/// [`EXTRACT_CONCURRENCY`] extractions run at once, and each result is kept
/// in the state folder, keyed by the file's path, size, modification time,
/// and the track, so asking again does not run FFmpeg again.
async fn embedded_subtitle(
    State(state): State<Arc<AppState>>,
    UrlPath((id, track)): UrlPath<(String, String)>,
) -> Response {
    let Some(entry) = state.lookup(&id).await else {
        return unknown_video();
    };
    if !entry.complete {
        return incomplete_video();
    }
    let Ok(track) = track.parse::<u32>() else {
        return error(StatusCode::NOT_FOUND, "No subtitle track with this index");
    };
    let Some(encoder) = state.hls.encoder() else {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "This server has no FFmpeg, so subtitles inside the file cannot be extracted",
        );
    };
    let Some(info) = state.media.inspect(&entry.path).await else {
        return error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "The video could not be inspected",
        );
    };
    match info
        .subtitles
        .iter()
        .find(|subtitle| subtitle.track == track)
    {
        None => return error(StatusCode::NOT_FOUND, "No subtitle track with this index"),
        Some(subtitle) if !subtitle.text => {
            return error(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "This subtitle track is bitmap, which cannot be sent as text",
            );
        }
        Some(_) => {}
    }
    let Some(cached) = subtitle_cache_path(&state.state_dir, &entry.path, track) else {
        return error(
            StatusCode::NOT_FOUND,
            "The video's file is no longer on the server",
        );
    };
    if let Ok(bytes) = tokio::fs::read(&cached).await {
        touch(cached).await;
        return webvtt(bytes);
    }
    let Ok(_permit) = state.extracting.acquire().await else {
        return error(StatusCode::SERVICE_UNAVAILABLE, "The server is stopping");
    };
    // Another request may have extracted it while this one waited.
    if let Ok(bytes) = tokio::fs::read(&cached).await {
        return webvtt(bytes);
    }
    match transcode::extract_webvtt(encoder, &entry.path, track).await {
        Ok(bytes) => {
            match save_atomically(&cached, &bytes).await {
                Ok(()) => {
                    // Keep the cache under its cap, oldest first, without
                    // holding up this answer.
                    let dir = state.state_dir.join(SUBTITLE_CACHE_DIR);
                    tokio::task::spawn_blocking(move || {
                        let fresh = cached.clone();
                        if let Err(problem) = cache::prune(&dir, cache::CACHE_LIMIT_BYTES, |path| {
                            path == fresh || !is_subtitle_cache_file(path)
                        }) {
                            tracing::warn!(dir = %dir.display(), "Could not prune the subtitle cache: {problem}");
                        }
                    });
                }
                Err(problem) => {
                    tracing::warn!(path = %cached.display(), "Could not keep the extracted subtitles: {problem:#}");
                }
            }
            webvtt(bytes)
        }
        Err(problem) => error(StatusCode::BAD_GATEWAY, format!("{problem:#}")),
    }
}

/// Marks a cached file as just used, so pruning removes the least recently
/// used first.
async fn touch(path: PathBuf) {
    let _ = tokio::task::spawn_blocking(move || {
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .and_then(|file| file.set_modified(SystemTime::now()))
    })
    .await;
}

/// At startup: removes temporary files that an earlier process left in the
/// subtitle cache (this process's own may still be in use), then caps it.
fn sweep_subtitle_cache(dir: &Path) {
    let mine = format!(".{}-", std::process::id());
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') && name.ends_with(".tmp") && !name.starts_with(&mine) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    if let Err(problem) = cache::prune(dir, cache::CACHE_LIMIT_BYTES, |path| {
        !is_subtitle_cache_file(path)
    }) {
        tracing::warn!(dir = %dir.display(), "Could not prune the subtitle cache: {problem}");
    }
}

/// What pruning may remove from the subtitle cache: `<hash>.vtt` entries,
/// and `.<pid>-<n>.tmp` files of other processes. This process's own may be
/// mid-write; anything else is not the server's.
fn is_subtitle_cache_file(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let temporary = name.starts_with('.') && name.ends_with(".tmp");
    name.ends_with(".vtt") || (temporary && !name.starts_with(&format!(".{}-", std::process::id())))
}

fn webvtt(bytes: Vec<u8>) -> Response {
    ([(header::CONTENT_TYPE, "text/vtt")], bytes).into_response()
}

/// Where the WebVTT of `track` in `video` is kept: a hash of the path, size,
/// modification time, and track, so a changed file is extracted again.
/// None when the file cannot be read.
fn subtitle_cache_path(state_dir: &Path, video: &Path, track: u32) -> Option<PathBuf> {
    use sha1::{Digest, Sha1};
    let metadata = std::fs::metadata(video).ok()?;
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |time| time.as_nanos());
    let mut hasher = Sha1::new();
    hasher.update(video.as_os_str().as_encoded_bytes());
    hasher.update([0]);
    hasher.update(metadata.len().to_le_bytes());
    hasher.update(modified.to_le_bytes());
    hasher.update(track.to_le_bytes());
    let name = format!("{}.vtt", ids::encode_hex(&hasher.finalize()));
    Some(state_dir.join(SUBTITLE_CACHE_DIR).join(name))
}

/// Writes through a uniquely named temporary file and renames it, so a
/// reader never sees part of a file and two writers never share one.
async fn save_atomically(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let dir = path.parent().context("A cache file needs a folder")?;
    tokio::fs::create_dir_all(dir)
        .await
        .with_context(|| format!("Create {}", dir.display()))?;
    let temporary = dir.join(format!(
        ".{}-{}.tmp",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    if let Err(problem) = tokio::fs::write(&temporary, bytes).await {
        let _ = tokio::fs::remove_file(&temporary).await;
        return Err(problem).with_context(|| format!("Write {}", temporary.display()));
    }
    if let Err(problem) = tokio::fs::rename(&temporary, path).await {
        let _ = tokio::fs::remove_file(&temporary).await;
        return Err(problem).with_context(|| format!("Save {}", path.display()));
    }
    Ok(())
}

/// `{"hash": "<16 hex>"}`, the OpenSubtitles movie hash.
async fn hash(State(state): State<Arc<AppState>>, UrlPath(id): UrlPath<String>) -> Response {
    let Some(entry) = state.lookup(&id).await else {
        return unknown_video();
    };
    if !entry.complete {
        return incomplete_video();
    }
    let path = entry.path.clone();
    match tokio::task::spawn_blocking(move || crate::subtitles::movie_hash(&path)).await {
        Ok(Ok(hash)) => Json(serde_json::json!({ "hash": hash })).into_response(),
        Ok(Err(problem)) => error(StatusCode::UNPROCESSABLE_ENTITY, format!("{problem:#}")),
        Err(_) => error(StatusCode::INTERNAL_SERVER_ERROR, "Hash the video"),
    }
}

fn progress_path(state: &AppState) -> PathBuf {
    state.state_dir.join(PROGRESS_FILE)
}

async fn read_progress(path: &Path) -> HashMap<String, Progress> {
    match tokio::fs::read(path).await {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|error| {
            tracing::warn!("Ignoring unreadable {}: {error}", path.display());
            HashMap::new()
        }),
        Err(_) => HashMap::new(),
    }
}

async fn get_progress(
    State(state): State<Arc<AppState>>,
    UrlPath(id): UrlPath<String>,
) -> Response {
    if state.lookup(&id).await.is_none() {
        return unknown_video();
    }
    let _guard = state.progress.lock().await;
    match read_progress(&progress_path(&state)).await.remove(&id) {
        Some(progress) => Json(progress).into_response(),
        None => error(StatusCode::NOT_FOUND, "No progress saved for this video"),
    }
}

/// Saves progress and answers with what is now stored. A value older than
/// the stored one by more than a minute is not applied, so a client that
/// was offline does not rewind another client's newer position. A time more
/// than a minute ahead of the server's clock is lowered to that, so a
/// client with a wrong clock cannot pin its value against every later one.
async fn put_progress(
    State(state): State<Arc<AppState>>,
    UrlPath(id): UrlPath<String>,
    body: Bytes,
) -> Response {
    if state.lookup(&id).await.is_none() {
        return unknown_video();
    }
    let mut progress: Progress = match serde_json::from_slice(&body) {
        Ok(progress) => progress,
        Err(problem) => {
            return error(
                StatusCode::BAD_REQUEST,
                format!("The progress is not valid JSON: {problem}"),
            );
        }
    };
    if !(progress.position.is_finite()
        && progress.position >= 0.0
        && progress.duration.is_finite()
        && progress.duration >= 0.0)
    {
        return error(
            StatusCode::BAD_REQUEST,
            "Position and duration must be non-negative seconds",
        );
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |time| time.as_secs());
    if progress.updated == 0 {
        progress.updated = now;
    }
    progress.updated = progress
        .updated
        .min(now.saturating_add(PROGRESS_CLOCK_SKEW));
    let _guard = state.progress.lock().await;
    let path = progress_path(&state);
    let mut saved = read_progress(&path).await;
    if let Some(existing) = saved.get(&id)
        && existing.updated > progress.updated.saturating_add(PROGRESS_CONFLICT_WINDOW)
    {
        return Json(existing.clone()).into_response();
    }
    saved.insert(id, progress.clone());
    let written = serde_json::to_vec_pretty(&saved)
        .map_err(anyhow::Error::from)
        .and_then(|bytes| auth::write_private(&path, &bytes));
    match written {
        Ok(()) => Json(progress).into_response(),
        Err(problem) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Save progress: {problem:#}"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn partial(start: u64, end: u64) -> RangeRequest {
        RangeRequest::Partial(ByteRange { start, end })
    }

    #[test]
    fn ranges_follow_rfc_9110_for_one_range() {
        assert_eq!(parse_range("bytes=0-9", 100), partial(0, 9));
        assert_eq!(parse_range("bytes=90-", 100), partial(90, 99));
        assert_eq!(parse_range("bytes=99-99", 100), partial(99, 99));
        assert_eq!(parse_range("bytes=-10", 100), partial(90, 99));
        assert_eq!(parse_range("bytes=-1000", 100), partial(0, 99));
        assert_eq!(parse_range("bytes=50-1000", 100), partial(50, 99));
        assert_eq!(parse_range("BYTES= 1 - 2 ", 100), partial(1, 2));
        assert_eq!(parse_range("items=0-1", 100), RangeRequest::Full);
        for refused in [
            "bytes=0-1,5-6",
            "bytes=100-",
            "bytes=100-200",
            "bytes=5-4",
            "bytes=-0",
            "bytes=-",
            "bytes=a-b",
            "bytes=+1-2",
            "bytes=0",
        ] {
            assert_eq!(
                parse_range(refused, 100),
                RangeRequest::Unsatisfiable,
                "{refused}"
            );
        }
        assert_eq!(parse_range("bytes=0-", 0), RangeRequest::Unsatisfiable);
        assert_eq!(parse_range("bytes=-1", 0), RangeRequest::Unsatisfiable);
    }
}
