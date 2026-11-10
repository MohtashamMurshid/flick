//! "Share my library on Tailscale": the remote library server from
//! `flick::remote::server`, run inside the desktop app instead of as
//! `flick-serve`. It runs on its own Tokio runtime thread, like `Services`,
//! and makes the same choices as `flick-serve --library`: the live
//! `settings.json` as the source, `<data dir>/remote` as the state folder,
//! the thumbnail cache for posters, and this machine's Tailscale IPv4
//! address on `protocol::DEFAULT_PORT`.
//!
//! The server re-reads `settings.json` for every listing, so videos added
//! while it runs appear once the desktop's background save lands. Before it
//! starts, the worker waits for saves queued until then, so the first
//! listing already sees the library the switch was turned on with.
//!
//! "Let paired Flicks download to this Mac" adds a [`TransferBackend`] when
//! the server starts with it on. Its requests become `Services` queries on
//! the window's own transfer worker, so remote-started downloads are the
//! same transfers the Downloads page shows. The owner's switch, download
//! folder, and share roots are read for every request from a [`Gate`] the
//! settings page updates; turning the switch off answers 403 without a
//! restart, and turning it on restarts a server that started without it.

use crate::services::{
    FetchListing, FetchState, QueryError, QueryHandle, QueryReply, QueryRequest, TransferListing,
};
use flick::remote::{
    auth, bind,
    ids::{self, Salt},
    protocol::{
        self, BrowseEntry, BrowseListing, Capability, FetchPhase, FetchRow, Pairing, ShareResult,
        Started, Storage, TorrentFileInfo, TorrentInfo, TransferFileRow, TransferPhase,
        TransferRow,
    },
    server::{Server, ServerConfig, Sources},
    transfers::{TransferBackend, TransferCommand, TransferError, TransferReply, TransferRequest},
};
use flick::torrent::{TorrentInput, TransferState};
use std::{
    fmt,
    net::{IpAddr, SocketAddr},
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex, mpsc},
    thread,
};

/// Shown when `tailscale ip -4` gives no Tailscale address.
pub const TAILSCALE_MISSING: &str = if cfg!(target_os = "macos") {
    "Tailscale is not running on this Mac"
} else {
    "Tailscale is not running on this computer"
};

/// What the settings row shows. `Debug` redacts the pairing token.
#[derive(Clone, Default, PartialEq)]
pub enum ServeStatus {
    #[default]
    Off,
    Starting,
    Running {
        listen: SocketAddr,
        /// `flick-remote://host:port/#token`, for the other Flick to paste.
        pairing: String,
    },
    Failed(String),
}

impl fmt::Debug for ServeStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ServeStatus::Off => f.write_str("Off"),
            ServeStatus::Starting => f.write_str("Starting"),
            ServeStatus::Running { listen, .. } => f
                .debug_struct("Running")
                .field("listen", listen)
                .field("pairing", &"<redacted>")
                .finish(),
            ServeStatus::Failed(message) => f.debug_tuple("Failed").field(message).finish(),
        }
    }
}

/// Runs before the server reads its sources, on the server thread.
pub type BeforeStart = Box<dyn FnOnce() -> anyhow::Result<()> + Send>;

pub struct ServeOptions {
    /// The desktop's live `settings.json`.
    pub settings_path: PathBuf,
    /// Token, id salt, and transcode temporaries.
    pub state_dir: PathBuf,
    pub poster_cache: Option<PathBuf>,
    /// Empty uses the computer's name.
    pub name: String,
    /// Replaces the saved token first, revoking every paired Flick.
    pub new_token: bool,
    /// None binds to this machine's Tailscale IPv4 address on
    /// [`protocol::DEFAULT_PORT`]. Tests pass loopback.
    pub bind: Option<SocketAddr>,
    /// FFmpeg for the quality tiers. None searches `FLICK_FFMPEG`, `PATH`,
    /// and the usual install folders. Tests pass a path that does not exist,
    /// so they never run this machine's FFmpeg.
    pub ffmpeg: Option<PathBuf>,
    /// For example, waiting for queued settings saves to reach the disk.
    pub before_start: Option<BeforeStart>,
}

/// The owner's choices for paired Flicks' downloads, from
/// `store::remote::RemoteSettings`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TransferSettings {
    /// "Let paired Flicks download to this Mac".
    pub allow: bool,
    /// None uses the app's download folder.
    pub download_dir: Option<PathBuf>,
    pub share_roots: Vec<PathBuf>,
}

impl TransferSettings {
    pub fn from_remote(remote: &crate::store::remote::RemoteSettings) -> Self {
        Self {
            allow: remote.allow_downloads,
            download_dir: remote.download_dir.clone(),
            share_roots: remote.share_roots.clone(),
        }
    }
}

/// Shared by the window and the server thread.
#[derive(Default)]
struct Gate {
    /// None until the settings page sets it; the server then reads
    /// `settings.json` once.
    settings: Option<TransferSettings>,
    /// Whether the server of this generation answers transfer commands.
    attached: Option<(u64, bool)>,
}

/// What a restart needs from the last start, without its one-shot parts.
#[derive(Clone)]
struct RestartOptions {
    settings_path: PathBuf,
    state_dir: PathBuf,
    poster_cache: Option<PathBuf>,
    name: String,
    bind: Option<SocketAddr>,
    ffmpeg: Option<PathBuf>,
}

/// The in-app server's state. One per window; `start` replaces a running
/// server, `stop` and `shutdown` never block the UI thread.
pub struct RemoteServe {
    status: ServeStatus,
    /// Events from older starts are ignored.
    generation: u64,
    running: Option<Running>,
    /// A stopped server's thread, joined by the next start's thread or at quit.
    stopping: Option<thread::JoinHandle<()>>,
    events_tx: mpsc::Sender<(u64, ServeStatus)>,
    events: mpsc::Receiver<(u64, ServeStatus)>,
    gate: Arc<Mutex<Gate>>,
    /// The transfer worker; None uses the window's (`services::query_host`).
    host: Option<QueryHandle>,
    restart: Option<RestartOptions>,
    /// Tests receive each started server's backend here.
    tap: Option<mpsc::Sender<TransferBackend>>,
}

struct Running {
    stop: tokio::sync::oneshot::Sender<()>,
    thread: thread::JoinHandle<()>,
}

impl Default for RemoteServe {
    fn default() -> Self {
        let (events_tx, events) = mpsc::channel();
        Self {
            status: ServeStatus::Off,
            generation: 0,
            running: None,
            stopping: None,
            events_tx,
            events,
            gate: Default::default(),
            host: None,
            restart: None,
            tap: None,
        }
    }
}

impl RemoteServe {
    pub fn status(&self) -> &ServeStatus {
        &self.status
    }

    /// Applies the owner's download settings from the next request on.
    /// Turning downloads on restarts a server that started without them,
    /// with the same token, so paired Flicks see the new capabilities.
    pub fn set_transfer_settings(&mut self, settings: TransferSettings) {
        let restart = {
            let mut gate = self.gate.lock().unwrap_or_else(|e| e.into_inner());
            let restart = settings.allow && gate.attached == Some((self.generation, false));
            gate.settings = Some(settings);
            restart
        };
        if restart
            && self.running.is_some()
            && let Some(options) = self.restart.clone()
        {
            tracing::info!("Restarting the in-app server to accept downloads");
            self.start(ServeOptions {
                settings_path: options.settings_path,
                state_dir: options.state_dir,
                poster_cache: options.poster_cache,
                name: options.name,
                new_token: false,
                bind: options.bind,
                ffmpeg: options.ffmpeg,
                before_start: None,
            });
        }
    }

    /// Starts a server, replacing one already running.
    pub fn start(&mut self, options: ServeOptions) {
        self.stop();
        self.generation += 1;
        self.status = ServeStatus::Starting;
        let generation = self.generation;
        self.restart = Some(RestartOptions {
            settings_path: options.settings_path.clone(),
            state_dir: options.state_dir.clone(),
            poster_cache: options.poster_cache.clone(),
            name: options.name.clone(),
            bind: options.bind,
            ffmpeg: options.ffmpeg.clone(),
        });
        let transfers = self
            .host
            .clone()
            .or_else(crate::services::query_host)
            .map(|host| TransferSetup {
                host,
                gate: self.gate.clone(),
                generation,
                tap: self.tap.clone(),
            });
        let events = self.events_tx.clone();
        let previous = self.stopping.take();
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let spawned = thread::Builder::new()
            .name("flick-serve".into())
            .spawn(move || {
                // The previous server must release the port and state lock.
                if let Some(previous) = previous {
                    let _ = previous.join();
                }
                let runtime = match tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .thread_name("flick-serve-worker")
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        let _ = events.send((
                            generation,
                            ServeStatus::Failed(format!("Could not start sharing: {error}")),
                        ));
                        return;
                    }
                };
                runtime.block_on(serve(options, transfers, generation, events, stopped));
                runtime.shutdown_timeout(std::time::Duration::from_secs(2));
            });
        match spawned {
            Ok(thread) => self.running = Some(Running { stop, thread }),
            Err(error) => {
                self.status = ServeStatus::Failed(format!("Could not start sharing: {error}"))
            }
        }
    }

    /// Shows why sharing could not start, without starting.
    pub fn fail(&mut self, message: String) {
        self.stop();
        self.status = ServeStatus::Failed(message);
    }

    /// Stops the server without waiting for it.
    pub fn stop(&mut self) {
        self.generation += 1;
        self.status = ServeStatus::Off;
        if let Some(running) = self.running.take() {
            let _ = running.stop.send(());
            if let Some(older) = self.stopping.replace(running.thread) {
                // Two stops without a start in between: the older one is
                // already ending; join it at quit.
                crate::services::register_shutdown_worker(older);
            }
        }
    }

    /// Stops the server and leaves its thread for `services::finish_shutdown`.
    pub fn shutdown(&mut self) {
        self.stop();
        if let Some(thread) = self.stopping.take() {
            crate::services::register_shutdown_worker(thread);
        }
    }

    /// Applies status changes from the server thread. True when the status
    /// changed.
    pub fn drain(&mut self) -> bool {
        let mut changed = false;
        while let Ok((generation, status)) = self.events.try_recv() {
            if generation == self.generation && self.status != status {
                if matches!(status, ServeStatus::Failed(_))
                    && let Some(running) = self.running.take()
                {
                    // The thread is ending on its own.
                    self.stopping = Some(running.thread);
                }
                self.status = status;
                changed = true;
            }
        }
        changed
    }
}

impl Drop for RemoteServe {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// What the server thread needs to answer transfer commands.
struct TransferSetup {
    host: QueryHandle,
    gate: Arc<Mutex<Gate>>,
    generation: u64,
    tap: Option<mpsc::Sender<TransferBackend>>,
}

async fn serve(
    options: ServeOptions,
    transfers: Option<TransferSetup>,
    generation: u64,
    events: mpsc::Sender<(u64, ServeStatus)>,
    stopped: tokio::sync::oneshot::Receiver<()>,
) {
    let send = |status| {
        let _ = events.send((generation, status));
    };
    if let Some(before) = options.before_start {
        // Not fatal: the server re-reads the file for every listing.
        match tokio::task::spawn_blocking(before).await {
            Ok(Err(error)) => tracing::warn!(%error, "Settings were not saved before sharing"),
            Err(error) => tracing::warn!(%error, "Settings were not saved before sharing"),
            Ok(Ok(())) => {}
        }
    }
    let token = match auth::load_or_create_token(&options.state_dir, options.new_token) {
        Ok(token) => token,
        Err(error) => return send(ServeStatus::Failed(format!("{error:#}"))),
    };
    let bind = match options.bind {
        Some(bind) => bind,
        None => match bind::tailscale_ipv4().await {
            Ok(ip) => SocketAddr::new(IpAddr::V4(ip), protocol::DEFAULT_PORT),
            Err(error) => {
                tracing::info!(%error, "No Tailscale address for the in-app server");
                return send(ServeStatus::Failed(TAILSCALE_MISSING.into()));
            }
        },
    };
    let name = match options.name.trim() {
        "" => tokio::task::spawn_blocking(computer_name)
            .await
            .unwrap_or_else(|_| "Flick".into()),
        name => name.to_owned(),
    };
    let (backend, requests) = match &transfers {
        Some(setup) => attach_transfers(setup, &options.settings_path).await,
        None => (None, None),
    };
    let state_dir = options.state_dir.clone();
    let config = ServerConfig {
        bind,
        token: token.clone(),
        name,
        sources: Sources::SettingsFile(options.settings_path),
        poster_cache: options.poster_cache.filter(|dir| dir.is_dir()),
        state_dir: options.state_dir,
        ffmpeg: options.ffmpeg,
        max_transcodes: 2,
        transfers: backend.clone(),
    };
    let server = match Server::start(config).await {
        Ok(server) => server,
        Err(error) => return send(ServeStatus::Failed(format!("{error:#}"))),
    };
    let answering = match (transfers, backend, requests) {
        (Some(setup), Some(backend), Some(requests)) => {
            // The server made the salt; the same one names its videos.
            match Salt::load_or_create(&state_dir) {
                Ok(salt) => {
                    if let Some(tap) = &setup.tap {
                        let _ = tap.send(backend.clone());
                    }
                    let answerer = Answerer {
                        host: setup.host,
                        gate: setup.gate,
                        salt,
                        links: backend.allows(Capability::Links),
                    };
                    Some(tokio::spawn(answer_transfers(requests, answerer)))
                }
                Err(error) => {
                    // Requests then answer 503 as the channel closes.
                    tracing::warn!("Transfers from other Flicks are off: {error:#}");
                    None
                }
            }
        }
        _ => None,
    };
    let listen = server.addr();
    send(ServeStatus::Running {
        listen,
        pairing: Pairing {
            host: listen.ip().to_string(),
            port: listen.port(),
            token,
        }
        .to_string(),
    });
    // Resolves on stop, or when the view is gone and the sender dropped.
    let _ = stopped.await;
    tracing::info!("Stopping the in-app remote library server");
    server.shutdown().await;
    if let Some(answering) = answering {
        answering.abort();
        let _ = answering.await;
    }
}

/// Decides, once per start, whether this server answers transfer commands,
/// and makes its backend when it does.
async fn attach_transfers(
    setup: &TransferSetup,
    settings_path: &Path,
) -> (
    Option<TransferBackend>,
    Option<tokio::sync::mpsc::Receiver<TransferRequest>>,
) {
    let path = settings_path.to_owned();
    let saved = tokio::task::spawn_blocking(move || saved_transfer_settings(&path))
        .await
        .unwrap_or_default();
    let attach = {
        let mut gate = setup.gate.lock().unwrap_or_else(|e| e.into_inner());
        let attach = gate.settings.get_or_insert(saved).allow;
        gate.attached = Some((setup.generation, attach));
        attach
    };
    if !attach {
        return (None, None);
    }
    let links = tokio::task::spawn_blocking(|| flick::fetch::ytdlp::Tools::discover().is_ok())
        .await
        .unwrap_or(false);
    let (sender, requests) = tokio::sync::mpsc::channel(32);
    (
        Some(TransferBackend::new(capabilities(links), sender)),
        Some(requests),
    )
}

/// What this Flick offers paired Flicks while downloads are allowed.
fn capabilities(links: bool) -> Vec<Capability> {
    let mut capabilities = vec![
        Capability::Downloads,
        Capability::Shares,
        Capability::StreamIncomplete,
    ];
    if links {
        capabilities.push(Capability::Links);
    }
    capabilities
}

/// The owner's settings as last saved; defaults when unreadable.
fn saved_transfer_settings(settings_path: &Path) -> TransferSettings {
    #[derive(Default, serde::Deserialize)]
    #[serde(default)]
    struct Saved {
        remote: crate::store::remote::RemoteSettings,
    }
    std::fs::read_to_string(settings_path)
        .ok()
        .and_then(|text| serde_json::from_str::<Saved>(&text).ok())
        .map(|saved| TransferSettings::from_remote(&saved.remote))
        .unwrap_or_default()
}

/// Answers the server's transfer commands until the server goes away.
async fn answer_transfers(
    mut requests: tokio::sync::mpsc::Receiver<TransferRequest>,
    answerer: Answerer,
) {
    while let Some(TransferRequest { command, mut reply }) = requests.recv().await {
        let answerer = answerer.clone();
        tokio::spawn(async move {
            // A route that went away drops its question, which stops a
            // resolution waiting for peers on the worker.
            tokio::select! {
                result = answerer.answer(command) => {
                    let _ = reply.send(result);
                }
                () = reply.closed() => {}
            }
        });
    }
}

#[derive(Clone)]
struct Answerer {
    host: QueryHandle,
    gate: Arc<Mutex<Gate>>,
    salt: Salt,
    links: bool,
}

impl Answerer {
    fn settings(&self) -> TransferSettings {
        self.gate
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .settings
            .clone()
            .unwrap_or_default()
    }

    async fn ask(&self, request: QueryRequest) -> Result<QueryReply, TransferError> {
        self.host
            .query(request)
            .await
            .map_err(|_| TransferError::new(503, "Downloads stopped before answering."))?
            .map_err(transfer_error)
    }

    async fn answer(&self, command: TransferCommand) -> Result<TransferReply, TransferError> {
        let settings = self.settings();
        if !settings.allow {
            return Err(TransferError::not_allowed("downloads"));
        }
        let folder = settings
            .download_dir
            .clone()
            .unwrap_or_else(|| self.host.download_dir.clone());
        let reply = match command {
            TransferCommand::Resolve { input } => {
                let (input, key) = parse_input(&input)?;
                match self.ask(QueryRequest::Resolve { input, key }).await? {
                    QueryReply::Resolved(torrent) => TransferReply::Resolved(TorrentInfo {
                        infohash: torrent.infohash.clone(),
                        name: torrent.name.clone(),
                        files: torrent
                            .files
                            .iter()
                            .map(|file| TorrentFileInfo {
                                index: file.index,
                                path: slash_path(&file.relative_path),
                                size: file.length,
                            })
                            .collect(),
                    }),
                    _ => return Err(unexpected()),
                }
            }
            TransferCommand::Start { input, files } => {
                let (input, key) = parse_input(&input)?;
                let request = QueryRequest::Start {
                    input,
                    key,
                    files,
                    folder,
                };
                match self.ask(request).await? {
                    QueryReply::Started(id) => TransferReply::Started(Started { id }),
                    _ => return Err(unexpected()),
                }
            }
            TransferCommand::List => match self.ask(QueryRequest::List).await? {
                QueryReply::Transfers(listings) => TransferReply::Transfers(
                    listings
                        .iter()
                        .map(|listing| transfer_row(listing, &self.salt))
                        .collect(),
                ),
                _ => return Err(unexpected()),
            },
            TransferCommand::Pause { id } => self.done(QueryRequest::Pause(infohash(&id)?)).await?,
            TransferCommand::Resume { id } => {
                self.done(QueryRequest::Resume(infohash(&id)?)).await?
            }
            TransferCommand::PauseAll => self.done(QueryRequest::PauseAll).await?,
            TransferCommand::ResumeAll => self.done(QueryRequest::ResumeAll).await?,
            TransferCommand::Remove {
                id,
                delete_files,
                confirm,
            } => {
                let request = QueryRequest::Remove {
                    id: infohash(&id)?,
                    delete_files,
                    confirm,
                };
                self.done(request).await?
            }
            TransferCommand::FetchStart { url } => {
                if !self.links {
                    return Err(TransferError::new(
                        403,
                        "This Flick cannot download links: yt-dlp is not installed on it.",
                    ));
                }
                match self.ask(QueryRequest::FetchStart { url, folder }).await? {
                    QueryReply::FetchStarted(id) => TransferReply::FetchStarted { id },
                    _ => return Err(unexpected()),
                }
            }
            TransferCommand::FetchList => match self.ask(QueryRequest::FetchList).await? {
                QueryReply::Fetches(fetches) => TransferReply::Fetches(
                    fetches
                        .iter()
                        .map(|fetch| fetch_row(fetch, &self.salt))
                        .collect(),
                ),
                _ => return Err(unexpected()),
            },
            TransferCommand::FetchCancel { id } => self.done(QueryRequest::FetchCancel(id)).await?,
            TransferCommand::Browse { dir } => {
                let roots = settings.share_roots;
                TransferReply::Browse(blocking(move || browse(&roots, &dir)).await?)
            }
            TransferCommand::Share { dir } => {
                let roots = settings.share_roots;
                let path =
                    blocking(move || resolve_virtual(&roots, &dir).ok_or_else(outside_roots))
                        .await?;
                match self.ask(QueryRequest::Share(path)).await? {
                    QueryReply::Shared(shared) => TransferReply::Shared(ShareResult {
                        infohash: shared.infohash,
                        magnet: shared.magnet,
                    }),
                    _ => return Err(unexpected()),
                }
            }
            TransferCommand::Storage => {
                TransferReply::Storage(blocking(move || Ok(storage(&folder))).await?)
            }
            TransferCommand::OpenStream { id, file_index } => {
                let request = QueryRequest::OpenStream {
                    id: infohash(&id)?,
                    file_index,
                };
                match self.ask(request).await? {
                    QueryReply::Stream(stream) => TransferReply::Stream(stream),
                    _ => return Err(unexpected()),
                }
            }
        };
        Ok(reply)
    }

    async fn done(&self, request: QueryRequest) -> Result<TransferReply, TransferError> {
        match self.ask(request).await? {
            QueryReply::Done => Ok(TransferReply::Done),
            _ => Err(unexpected()),
        }
    }
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, TransferError> + Send + 'static,
) -> Result<T, TransferError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|_| TransferError::new(500, "The request stopped unexpectedly."))?
}

fn unexpected() -> TransferError {
    TransferError::new(500, "Downloads gave an unexpected answer.")
}

/// The worker's refusal as an HTTP status. A deletion that no longer
/// matches lists the current files, one per line, after the message.
pub fn transfer_error(error: QueryError) -> TransferError {
    match error {
        QueryError::NotFound(message) => TransferError::new(404, message),
        QueryError::Conflict { message, files } if files.is_empty() => {
            TransferError::new(409, message)
        }
        QueryError::Conflict { message, files } => {
            TransferError::new(409, format!("{message}\n{}", files.join("\n")))
        }
        QueryError::Invalid(message) => TransferError::new(422, message),
        QueryError::Busy(message) | QueryError::Unavailable(message) => {
            TransferError::new(503, message)
        }
        QueryError::TimedOut(message) => TransferError::new(504, message),
        QueryError::Failed(message) => TransferError::new(500, message),
    }
}

/// A transfer id from a URL: an infohash, else 404.
fn infohash(id: &str) -> Result<String, TransferError> {
    if id.len() == 40 && id.bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(id.to_ascii_lowercase())
    } else {
        Err(TransferError::new(
            404,
            "No transfer with that id on this Flick.",
        ))
    }
}

/// A resolve or start input, with the key that matches a Start to the
/// Resolve before it: the infohash when the input names one.
fn parse_input(text: &str) -> Result<(TorrentInput, String), TransferError> {
    let text = text.trim();
    let invalid = |message: &str| TransferError::new(422, message);
    if let Some(encoded) = text.strip_prefix("base64:") {
        let bytes = decode_base64(encoded)
            .ok_or_else(|| invalid("The .torrent file is not valid base64."))?;
        let infohash = librqbit::torrent_from_bytes(&bytes)
            .map_err(|_| invalid("That is not a .torrent file."))?
            .info_hash
            .as_string();
        return Ok((TorrentInput::Bytes(bytes), infohash));
    }
    if text.len() == 40 && text.bytes().all(|b| b.is_ascii_hexdigit()) {
        let hash = text.to_ascii_lowercase();
        return Ok((
            TorrentInput::Magnet(format!("magnet:?xt=urn:btih:{hash}")),
            hash,
        ));
    }
    if text.len() > 7 && text[..7].eq_ignore_ascii_case("magnet:") {
        let key = magnet_infohash(text).unwrap_or_else(|| text.to_owned());
        return Ok((TorrentInput::Magnet(text.to_owned()), key));
    }
    if text.len() > 8 && text[..8].eq_ignore_ascii_case("https://") {
        return Ok((TorrentInput::Url(text.to_owned()), text.to_owned()));
    }
    Err(invalid(
        "Send a magnet link, an https link to a .torrent file, an infohash, or base64: and the file.",
    ))
}

/// The hex infohash in a magnet link's `xt=urn:btih:` parameter.
fn magnet_infohash(magnet: &str) -> Option<String> {
    let query = magnet.split_once('?')?.1;
    query.split('&').find_map(|pair| {
        let value = pair.strip_prefix("xt=")?;
        let hash = value
            .get(..9)
            .filter(|prefix| prefix.eq_ignore_ascii_case("urn:btih:"))
            .map(|_| &value[9..])?;
        (hash.len() == 40 && hash.bytes().all(|b| b.is_ascii_hexdigit()))
            .then(|| hash.to_ascii_lowercase())
    })
}

/// Standard base64 with optional padding; whitespace is ignored.
fn decode_base64(text: &str) -> Option<Vec<u8>> {
    let mut bytes = Vec::with_capacity(text.len() * 3 / 4);
    let mut buffer = 0u32;
    let mut bits = 0;
    let mut padding = 0;
    for byte in text.bytes().filter(|b| !b.is_ascii_whitespace()) {
        if byte == b'=' {
            padding += 1;
            continue;
        }
        if padding > 0 {
            return None;
        }
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        };
        buffer = (buffer << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            bytes.push((buffer >> bits) as u8);
            buffer &= (1 << bits) - 1;
        }
    }
    (padding <= 2 && !bytes.is_empty()).then_some(bytes)
}

fn slash_path(path: &Path) -> String {
    path.components()
        .map(|part| part.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

/// A transfer as `GET /v1/transfers` lists it. Each selected video gets
/// the id the server's library gives the same file.
pub fn transfer_row(listing: &TransferListing, salt: &Salt) -> TransferRow {
    let snapshot = &listing.snapshot;
    TransferRow {
        id: snapshot.infohash.clone(),
        name: snapshot.name.clone(),
        phase: match snapshot.state {
            TransferState::Checking => TransferPhase::Checking,
            TransferState::Downloading => TransferPhase::Downloading,
            TransferState::Paused => TransferPhase::Paused,
            TransferState::Seeding => TransferPhase::Seeding,
            TransferState::Error => TransferPhase::Error,
        },
        error: snapshot.error.clone(),
        verified_bytes: snapshot.verified_bytes,
        total_bytes: snapshot.total_bytes,
        checked_bytes: snapshot.checked_bytes,
        checking_total_bytes: snapshot.checking_total_bytes,
        uploaded_bytes: snapshot.uploaded_bytes,
        peers: snapshot.peers,
        download_bps: snapshot.download_bytes_per_second,
        upload_bps: snapshot.upload_bytes_per_second,
        // Filled in by the deletion-safety fix: shares and on-disk files.
        share: false,
        files: snapshot
            .files
            .iter()
            .map(|file| TransferFileRow {
                index: file.index,
                path: listing.paths.get(file.index).cloned().unwrap_or_default(),
                size: file.length,
                verified_bytes: file.verified_bytes,
                selected: file.selected,
                will_delete: file.selected,
                video_id: (file.selected && flick::library::is_video(&file.path))
                    .then(|| library_id(salt, &file.path))
                    .flatten(),
            })
            .collect(),
    }
}

/// The server's id for a file, as its listing computes it: from the
/// canonical path. None while the file does not exist yet.
fn library_id(salt: &Salt, path: &Path) -> Option<String> {
    let canonical = path.canonicalize().ok()?;
    Some(ids::video_id(salt, &canonical))
}

fn fetch_row(fetch: &FetchListing, salt: &Salt) -> FetchRow {
    let (phase, error) = match &fetch.state {
        FetchState::Running => (
            match fetch.progress.stage {
                flick::fetch::FetchStage::Starting => FetchPhase::Resolving,
                flick::fetch::FetchStage::Downloading => FetchPhase::Downloading,
                flick::fetch::FetchStage::Processing | flick::fetch::FetchStage::Verifying => {
                    FetchPhase::Merging
                }
            },
            None,
        ),
        FetchState::Done => (FetchPhase::Done, None),
        FetchState::Failed(error) => (FetchPhase::Failed, Some(error.clone())),
        FetchState::Cancelled => (FetchPhase::Cancelled, None),
    };
    FetchRow {
        id: fetch.id,
        url: fetch.url.clone(),
        title: fetch.title.clone(),
        phase,
        error,
        downloaded_bytes: fetch.progress.downloaded_bytes,
        total_bytes: fetch.progress.total_bytes,
        bytes_per_second: fetch.progress.bytes_per_second,
        saved: fetch
            .saved
            .iter()
            .filter(|path| flick::library::is_video(path))
            .filter_map(|path| library_id(salt, path))
            .collect(),
    }
}

fn outside_roots() -> TransferError {
    TransferError::new(404, "No such folder among the shared folders.")
}

/// Share roots by the names paired Flicks see: the folder's name, with a
/// number after repeats. Roots that are gone are left out.
fn named_roots(roots: &[PathBuf]) -> Vec<(String, PathBuf)> {
    let mut named: Vec<(String, PathBuf)> = Vec::new();
    for root in roots {
        let Ok(canonical) = root.canonicalize() else {
            continue;
        };
        if !canonical.is_dir() {
            continue;
        }
        let base = canonical
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Shared".to_owned());
        let mut name = base.clone();
        let mut count = 1;
        while named.iter().any(|(known, _)| *known == name) {
            count += 1;
            name = format!("{base} {count}");
        }
        named.push((name, canonical));
    }
    named
}

/// A virtual `<root name>/<relative path>` as a canonical path inside that
/// root, with symlinks resolved first. None for anything outside a root,
/// for `..`, and for paths that do not exist.
pub fn resolve_virtual(roots: &[PathBuf], dir: &str) -> Option<PathBuf> {
    let mut parts = dir.split('/').filter(|part| !part.is_empty());
    let first = parts.next()?;
    let (_, root) = named_roots(roots)
        .into_iter()
        .find(|(name, _)| name == first)?;
    let mut path = root.clone();
    for part in parts {
        let mut components = Path::new(part).components();
        match (components.next(), components.next()) {
            (Some(Component::Normal(name)), None) if !part.contains('\\') => path.push(name),
            _ => return None,
        }
    }
    let canonical = path.canonicalize().ok()?;
    canonical.starts_with(&root).then_some(canonical)
}

/// Folders and videos under an allowed root; the roots by name for "".
pub fn browse(roots: &[PathBuf], dir: &str) -> Result<BrowseListing, TransferError> {
    let normalized = dir
        .split('/')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("/");
    if normalized.is_empty() {
        return Ok(BrowseListing {
            dir: String::new(),
            entries: named_roots(roots)
                .into_iter()
                .map(|(name, _)| BrowseEntry {
                    name,
                    dir: true,
                    size: 0,
                })
                .collect(),
        });
    }
    let root = named_roots(roots)
        .into_iter()
        .find(|(name, _)| Some(name.as_str()) == normalized.split('/').next())
        .map(|(_, root)| root)
        .ok_or_else(outside_roots)?;
    let path = resolve_virtual(roots, &normalized).ok_or_else(outside_roots)?;
    let reading = std::fs::read_dir(&path).map_err(|_| outside_roots())?;
    let mut entries = Vec::new();
    for entry in reading.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        // Symlinks are followed, and kept only when they stay inside.
        let Ok(target) = entry.path().canonicalize() else {
            continue;
        };
        if !target.starts_with(&root) {
            continue;
        }
        let Ok(metadata) = std::fs::metadata(&target) else {
            continue;
        };
        if metadata.is_dir() {
            entries.push(BrowseEntry {
                name,
                dir: true,
                size: 0,
            });
        } else if metadata.is_file() && flick::library::is_video(Path::new(&name)) {
            entries.push(BrowseEntry {
                name,
                dir: false,
                size: metadata.len(),
            });
        }
    }
    entries.sort_by(|a, b| {
        b.dir
            .cmp(&a.dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    Ok(BrowseListing {
        dir: normalized,
        entries,
    })
}

/// Free space where downloads go, from the nearest folder that exists.
pub fn storage(folder: &Path) -> Storage {
    let free_bytes = folder
        .ancestors()
        .find(|dir| dir.is_dir())
        .and_then(|dir| fs2::available_space(dir).ok())
        .unwrap_or(0);
    Storage {
        free_bytes,
        download_folder_name: folder
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default(),
    }
}

/// The computer's friendly name on macOS, else the host name, as
/// `flick-serve` chooses it.
fn computer_name() -> String {
    let run = |program: &str, args: &[&str]| {
        std::process::Command::new(program)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
            .filter(|name| !name.is_empty())
    };
    let name = if cfg!(target_os = "macos") {
        run("scutil", &["--get", "ComputerName"])
    } else {
        None
    };
    name.or_else(|| run("hostname", &[]))
        .or_else(|| std::env::var("HOSTNAME").ok())
        .or_else(|| std::env::var("COMPUTERNAME").ok())
        .unwrap_or_else(|| "Flick".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        time::{Duration, Instant},
    };

    fn wait_for(serve: &mut RemoteServe, done: impl Fn(&ServeStatus) -> bool) -> ServeStatus {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            serve.drain();
            if done(serve.status()) {
                return serve.status().clone();
            }
            assert!(Instant::now() < deadline, "status: {:?}", serve.status());
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn get(listen: SocketAddr, path: &str, token: &str) -> String {
        let mut stream = std::net::TcpStream::connect(listen).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        write!(
            stream,
            "GET {path} HTTP/1.1\r\nHost: {listen}\r\nAuthorization: Bearer {token}\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        response
    }

    fn options(dir: &std::path::Path, new_token: bool) -> ServeOptions {
        ServeOptions {
            settings_path: dir.join("settings.json"),
            state_dir: dir.join("data").join("remote"),
            poster_cache: None,
            name: "Test Library".into(),
            new_token,
            bind: Some("127.0.0.1:0".parse().unwrap()),
            // Not an executable: transcoding stays off without searching PATH.
            ffmpeg: Some(dir.join("no-ffmpeg")),
            before_start: None,
        }
    }

    #[test]
    fn serves_the_settings_file_until_stopped_and_new_token_revokes() {
        let dir = tempfile::tempdir().unwrap();
        let video = dir.path().join("Clip.mp4");
        std::fs::write(&video, b"not really a video").unwrap();
        std::fs::write(
            dir.path().join("settings.json"),
            serde_json::json!({"library": [{"path": video, "title": "Clip"}]}).to_string(),
        )
        .unwrap();

        let mut serve = RemoteServe::default();
        assert_eq!(serve.status(), &ServeStatus::Off);
        let mut before = options(dir.path(), false);
        let (ran_tx, ran) = mpsc::channel();
        before.before_start = Some(Box::new(move || {
            ran_tx.send(()).unwrap();
            Ok(())
        }));
        serve.start(before);
        assert!(serve.running.is_some());
        let ServeStatus::Running { listen, pairing } =
            wait_for(&mut serve, |s| !matches!(s, ServeStatus::Starting))
        else {
            panic!("did not start: {:?}", serve.status());
        };
        ran.try_recv().expect("before_start runs before serving");
        let debug = format!("{:?}", serve.status());
        assert!(debug.contains("<redacted>"), "{debug}");
        let parsed = Pairing::parse(&pairing).unwrap();
        assert!(!debug.contains(&parsed.token), "{debug}");
        assert!(!debug.contains(&pairing), "{debug}");
        assert_eq!(
            (parsed.host.as_str(), parsed.port),
            ("127.0.0.1", listen.port())
        );
        let listing = get(listen, "/v1/library", &parsed.token);
        assert!(listing.starts_with("HTTP/1.1 200"), "{listing}");
        assert!(listing.contains("\"Test Library\""), "{listing}");
        assert!(listing.contains("\"Clip\""), "{listing}");
        // The test's FFmpeg does not exist, so only Original is offered.
        assert!(listing.contains(r#""qualities":["original"]"#), "{listing}");
        assert!(get(listen, "/v1/library", &"0".repeat(64)).starts_with("HTTP/1.1 401"));

        // A new token restarts the server and revokes the old one.
        serve.start(options(dir.path(), true));
        let ServeStatus::Running {
            listen: second,
            pairing: renewed,
        } = wait_for(&mut serve, |s| !matches!(s, ServeStatus::Starting))
        else {
            panic!("did not restart: {:?}", serve.status());
        };
        let renewed = Pairing::parse(&renewed).unwrap();
        assert_ne!(renewed.token, parsed.token);
        assert!(get(second, "/v1/library", &parsed.token).starts_with("HTTP/1.1 401"));
        assert!(get(second, "/v1/library", &renewed.token).starts_with("HTTP/1.1 200"));

        serve.stop();
        assert_eq!(serve.status(), &ServeStatus::Off);
        assert!(serve.running.is_none());
        // Late events from the stopped server are ignored.
        std::thread::sleep(Duration::from_millis(100));
        assert!(!serve.drain());
        let thread = serve.stopping.take().unwrap();
        thread.join().unwrap();
        assert!(std::net::TcpStream::connect(second).is_err());
    }

    fn snapshot_file(index: usize, path: PathBuf, selected: bool) -> flick::torrent::FileProgress {
        flick::torrent::FileProgress {
            index,
            path,
            length: 100,
            verified_bytes: 40,
            selected,
        }
    }

    #[test]
    fn snapshots_map_to_rows_with_library_ids_for_selected_videos() {
        let dir = tempfile::tempdir().unwrap();
        let salt = Salt::load_or_create(dir.path()).unwrap();
        let folder = dir.path().join("abc");
        std::fs::create_dir(&folder).unwrap();
        let video = folder.join("Film.mp4");
        std::fs::write(&video, b"video").unwrap();
        let listing = TransferListing {
            snapshot: flick::torrent::TransferSnapshot {
                infohash: "a".repeat(40),
                name: "Film".into(),
                state: TransferState::Checking,
                verified_bytes: 40,
                total_bytes: 100,
                checked_bytes: 10,
                checking_total_bytes: 300,
                uploaded_bytes: 7,
                peers: 3,
                download_bytes_per_second: 1000,
                upload_bytes_per_second: 20,
                error: None,
                files: vec![
                    snapshot_file(0, video.clone(), true),
                    snapshot_file(1, folder.join("notes.txt"), true),
                    snapshot_file(2, folder.join("Other.mp4"), false),
                    snapshot_file(3, folder.join("Missing.mkv"), true),
                ],
            },
            paths: vec![
                "Film/Film.mp4".into(),
                "Film/notes.txt".into(),
                "Film/Other.mp4".into(),
                "Film/Missing.mkv".into(),
            ],
        };
        let row = transfer_row(&listing, &salt);
        assert_eq!(row.id, "a".repeat(40));
        assert_eq!(row.phase, TransferPhase::Checking);
        assert_eq!(
            (
                row.verified_bytes,
                row.total_bytes,
                row.checked_bytes,
                row.checking_total_bytes
            ),
            (40, 100, 10, 300)
        );
        assert_eq!((row.uploaded_bytes, row.peers), (7, 3));
        assert_eq!((row.download_bps, row.upload_bps), (1000, 20));
        assert_eq!(row.files.len(), 4);
        assert_eq!(row.files[0].path, "Film/Film.mp4");
        assert_eq!((row.files[0].size, row.files[0].verified_bytes), (100, 40));
        assert_eq!(
            row.files[0].video_id,
            Some(ids::video_id(&salt, &video.canonicalize().unwrap()))
        );
        // Not a video, not selected, and not on disk yet: no library id.
        assert_eq!(row.files[1].video_id, None);
        assert!(!row.files[2].selected);
        assert_eq!(row.files[2].video_id, None);
        assert_eq!(row.files[3].video_id, None);
        for (state, phase) in [
            (TransferState::Downloading, TransferPhase::Downloading),
            (TransferState::Paused, TransferPhase::Paused),
            (TransferState::Seeding, TransferPhase::Seeding),
            (TransferState::Error, TransferPhase::Error),
        ] {
            let mut other = listing.clone();
            other.snapshot.state = state;
            assert_eq!(transfer_row(&other, &salt).phase, phase);
        }
    }

    #[test]
    fn a_confirmation_that_does_not_match_is_409_with_the_current_list() {
        let required = vec!["Show/E01.mkv".to_owned(), "Show/E02.mkv".to_owned()];
        assert!(
            crate::services::check_confirmation(
                &required,
                &["Show/E02.mkv".into(), "Show/E01.mkv".into()]
            )
            .is_ok()
        );
        for wrong in [
            vec![],
            vec!["Show/E01.mkv".to_owned()],
            vec![
                "Show/E01.mkv".to_owned(),
                "Show/E02.mkv".to_owned(),
                "Show/E03.mkv".to_owned(),
            ],
            vec![
                "Show/E01.mkv".to_owned(),
                "Show/E01.mkv".to_owned(),
                "Show/E02.mkv".to_owned(),
            ],
        ] {
            let error = crate::services::check_confirmation(&required, &wrong).unwrap_err();
            let QueryError::Conflict { files, .. } = &error else {
                panic!("{error:?}");
            };
            assert_eq!(files, &required);
            let error = transfer_error(error);
            assert_eq!(error.status, 409);
            let lines = error.message.lines().collect::<Vec<_>>();
            assert!(
                lines[0].contains("nothing was deleted"),
                "{}",
                error.message
            );
            assert_eq!(&lines[1..], ["Show/E01.mkv", "Show/E02.mkv"]);
        }
        assert_eq!(transfer_error(QueryError::NotFound("x".into())).status, 404);
        assert_eq!(transfer_error(QueryError::Invalid("x".into())).status, 422);
        assert_eq!(transfer_error(QueryError::Busy("x".into())).status, 503);
        assert_eq!(
            transfer_error(QueryError::Unavailable("x".into())).status,
            503
        );
        let plain = transfer_error(QueryError::Conflict {
            message: "Still checking.".into(),
            files: vec![],
        });
        assert_eq!(
            (plain.status, plain.message.as_str()),
            (409, "Still checking.")
        );
    }

    #[cfg(unix)]
    #[test]
    fn browsing_stays_inside_share_roots_after_resolving_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("Movies");
        let outside = dir.path().join("Private");
        std::fs::create_dir_all(root.join("Shows/Season 1")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(root.join("Film.mkv"), b"12345").unwrap();
        std::fs::write(root.join("notes.txt"), b"not a video").unwrap();
        std::fs::write(root.join(".hidden.mp4"), b"hidden").unwrap();
        std::fs::write(outside.join("Secret.mp4"), b"secret").unwrap();
        std::os::unix::fs::symlink(&outside, root.join("Escape")).unwrap();
        std::os::unix::fs::symlink(outside.join("Secret.mp4"), root.join("Linked.mp4")).unwrap();
        std::os::unix::fs::symlink(root.join("Shows"), root.join("Series")).unwrap();
        // A second root with the same name gets a number.
        let other = dir.path().join("elsewhere").join("Movies");
        std::fs::create_dir_all(&other).unwrap();
        let roots = vec![root.clone(), other.clone(), dir.path().join("gone")];

        let top = browse(&roots, "").unwrap();
        assert_eq!(top.dir, "");
        let names = top
            .entries
            .iter()
            .map(|e| e.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(names, ["Movies", "Movies 2"]);
        assert!(top.entries.iter().all(|e| e.dir));

        let listing = browse(&roots, "Movies/").unwrap();
        assert_eq!(listing.dir, "Movies");
        let names = listing
            .entries
            .iter()
            .map(|e| (e.name.as_str(), e.dir, e.size))
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            [
                ("Series", true, 0),
                ("Shows", true, 0),
                ("Film.mkv", false, 5)
            ]
        );
        assert_eq!(
            browse(&roots, "Movies/Series").unwrap().entries[0].name,
            "Season 1"
        );

        for refused in [
            "Movies/Escape",
            "Movies/../Private",
            "Movies/Shows/../../Private",
            "Movies/./Shows",
            "Private",
            "Movies/Missing",
            "gone",
        ] {
            assert_eq!(
                browse(&roots, refused).unwrap_err().status,
                404,
                "{refused}"
            );
        }
        assert_eq!(
            resolve_virtual(&roots, "Movies/Film.mkv"),
            Some(root.join("Film.mkv").canonicalize().unwrap())
        );
        assert_eq!(resolve_virtual(&roots, "Movies/Linked.mp4"), None);
        assert_eq!(resolve_virtual(&roots, "Movies/Escape/Secret.mp4"), None);
        assert_eq!(resolve_virtual(&roots, ""), None);
        assert_eq!(
            resolve_virtual(&roots, "Movies 2"),
            Some(other.canonicalize().unwrap())
        );
    }

    #[test]
    fn inputs_parse_with_a_key_that_matches_start_to_resolve() {
        let hash = "0123456789abcdef0123456789ABCDEF01234567";
        let lower = hash.to_ascii_lowercase();
        let (input, key) = parse_input(hash).unwrap();
        assert!(
            matches!(input, TorrentInput::Magnet(m) if m == format!("magnet:?xt=urn:btih:{lower}"))
        );
        assert_eq!(key, lower);
        let magnet = format!("magnet:?dn=Film&xt=urn:btih:{hash}&tr=x");
        let (input, key) = parse_input(&format!("  {magnet} ")).unwrap();
        assert!(matches!(input, TorrentInput::Magnet(m) if m == magnet));
        assert_eq!(key, lower);
        let (input, key) = parse_input("https://example.com/a.torrent").unwrap();
        assert!(matches!(input, TorrentInput::Url(_)));
        assert_eq!(key, "https://example.com/a.torrent");
        for refused in [
            "",
            "http://example.com/a.torrent",
            "/etc/passwd",
            "base64:!!",
            "base64:aGVsbG8=",
        ] {
            assert_eq!(
                parse_input(refused).err().map(|e| e.status),
                Some(422),
                "{refused}"
            );
        }
        assert_eq!(decode_base64("aGVsbG8="), Some(b"hello".to_vec()));
        assert_eq!(decode_base64("aGVs\nbG8"), Some(b"hello".to_vec()));
        assert_eq!(decode_base64("aGV=sbG8"), None);
        assert_eq!(infohash("nope").unwrap_err().status, 404);
        assert_eq!(infohash(hash).unwrap(), lower);
    }

    #[test]
    fn storage_reports_the_nearest_existing_folder() {
        let dir = tempfile::tempdir().unwrap();
        let storage = storage(&dir.path().join("Not Yet").join("Flick"));
        assert_eq!(storage.download_folder_name, "Flick");
        assert!(storage.free_bytes > 0);
    }

    fn transfer_test_runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap()
    }

    fn started_backend(
        serve: &mut RemoteServe,
        taps: &mpsc::Receiver<TransferBackend>,
    ) -> TransferBackend {
        assert!(matches!(
            wait_for(serve, |s| !matches!(s, ServeStatus::Starting)),
            ServeStatus::Running { .. }
        ));
        taps.recv_timeout(Duration::from_secs(10))
            .expect("the server attached transfers")
    }

    #[test]
    fn turning_downloads_on_restarts_a_server_that_started_without_them() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("settings.json"), "{}").unwrap();
        let root = dir.path().join("transfers");
        let services = crate::services::Services::start_config(
            flick::torrent::TransferConfig::local(root.join("downloads"), root.join("state")),
            vec![],
        );
        let (tap, taps) = mpsc::channel();
        let mut serve = RemoteServe::default();
        serve.host = Some(services.query_handle());
        serve.tap = Some(tap);
        serve.start(options(dir.path(), false));
        let ServeStatus::Running { pairing, .. } =
            wait_for(&mut serve, |s| !matches!(s, ServeStatus::Starting))
        else {
            panic!("did not start: {:?}", serve.status());
        };
        assert!(taps.try_recv().is_err(), "off at start: read-only");
        let settings = TransferSettings {
            allow: true,
            download_dir: Some(dir.path().join("Remote Downloads")),
            share_roots: vec![],
        };
        serve.set_transfer_settings(settings.clone());
        let backend = started_backend(&mut serve, &taps);
        let ServeStatus::Running { pairing: again, .. } = serve.status().clone() else {
            unreachable!()
        };
        // Tests bind port 0; the app binds the fixed port.
        assert_eq!(
            Pairing::parse(&again).unwrap().token,
            Pairing::parse(&pairing).unwrap().token,
            "a restart keeps the token"
        );
        assert!(backend.allows(Capability::Downloads));
        let runtime = transfer_test_runtime();
        let answer = runtime.block_on(backend.call(TransferCommand::Storage));
        let Ok(TransferReply::Storage(storage)) = answer else {
            panic!("storage was not answered");
        };
        assert_eq!(storage.download_folder_name, "Remote Downloads");
        // Off answers 403 at once, without a restart.
        serve.set_transfer_settings(TransferSettings {
            allow: false,
            ..settings.clone()
        });
        let refused = runtime.block_on(backend.call(TransferCommand::List));
        assert_eq!(refused.err().map(|e| e.status), Some(403));
        serve.set_transfer_settings(settings);
        assert!(taps.try_recv().is_err(), "on again: same server");
        assert!(matches!(
            runtime.block_on(backend.call(TransferCommand::List)),
            Ok(TransferReply::Transfers(rows)) if rows.is_empty()
        ));
        serve.stop();
        serve.stopping.take().unwrap().join().unwrap();
        drop(services);
    }

    /// The acceptance path of the in-app server against a loopback seed:
    /// resolve, start, progress, completion, deletion with a wrong and then
    /// the right list, storage, browsing, and sharing. No trackers or DHT.
    #[test]
    fn paired_flicks_download_through_the_window_worker_from_a_loopback_seed() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = transfer_test_runtime();
        let seed_source = dir.path().join("seed-source");
        std::fs::create_dir(&seed_source).unwrap();
        let bytes = (0..4 * 1024 * 1024 + 321)
            .map(|i: usize| ((i.wrapping_mul(31) ^ (i >> 9)) % 251) as u8)
            .collect::<Vec<_>>();
        std::fs::write(seed_source.join("Film.mp4"), &bytes).unwrap();
        let seed = runtime
            .block_on(flick::torrent::TransferSession::start(
                flick::torrent::TransferConfig::local(
                    dir.path().join("seed-downloads"),
                    dir.path().join("seed-state"),
                ),
            ))
            .unwrap();
        let shared = runtime
            .block_on(seed.share(&seed_source.join("Film.mp4"), vec![]))
            .unwrap();

        // The window's worker, slowed so progress is seen before completion.
        let mut config = flick::torrent::TransferConfig::local(
            dir.path().join("app-downloads"),
            dir.path().join("app-state"),
        );
        config.download_limit_bytes_per_second = std::num::NonZeroU32::new(2 * 1024 * 1024);
        let mut services =
            crate::services::Services::start_config(config, vec![seed.listen_addr().unwrap()]);

        let share_root = dir.path().join("Shared Videos");
        std::fs::create_dir(&share_root).unwrap();
        std::fs::write(share_root.join("Clip.mp4"), &bytes[..300_000]).unwrap();
        let remote_downloads = dir.path().join("Remote Downloads");
        std::fs::write(
            dir.path().join("settings.json"),
            serde_json::json!({"remote": {
                "allow_downloads": true,
                "download_dir": remote_downloads,
                "share_roots": [share_root],
            }})
            .to_string(),
        )
        .unwrap();
        let (tap, taps) = mpsc::channel();
        let mut serve = RemoteServe::default();
        serve.host = Some(services.query_handle());
        serve.tap = Some(tap);
        serve.start(options(dir.path(), false));
        let backend = started_backend(&mut serve, &taps);
        assert!(backend.allows(Capability::Downloads));
        assert!(backend.allows(Capability::Shares));
        assert!(backend.allows(Capability::StreamIncomplete));
        let salt = Salt::load_or_create(&dir.path().join("data").join("remote")).unwrap();

        let outcome = runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(90), async {
                let call = |command| backend.call(command);
                let Ok(TransferReply::Storage(storage)) = call(TransferCommand::Storage).await
                else {
                    panic!("no storage");
                };
                assert_eq!(storage.download_folder_name, "Remote Downloads");
                assert!(storage.free_bytes > 0);

                let Ok(TransferReply::Resolved(info)) = call(TransferCommand::Resolve {
                    input: shared.magnet.clone(),
                })
                .await
                else {
                    panic!("not resolved");
                };
                assert_eq!(info.infohash, shared.infohash);
                assert_eq!(info.files.len(), 1);
                assert_eq!(
                    (info.files[0].path.as_str(), info.files[0].size),
                    ("Film.mp4", bytes.len() as u64)
                );

                // Start reuses the resolution, by infohash.
                let Ok(TransferReply::Started(started)) = call(TransferCommand::Start {
                    input: shared.infohash.clone(),
                    files: vec![0],
                })
                .await
                else {
                    panic!("not started");
                };
                assert_eq!(started.id, shared.infohash);
                let again = call(TransferCommand::Start {
                    input: shared.magnet.clone(),
                    files: vec![0],
                })
                .await;
                assert_eq!(again.err().map(|e| e.status), Some(409));

                let mut seen_incomplete = false;
                let row = loop {
                    let Ok(TransferReply::Transfers(rows)) = call(TransferCommand::List).await
                    else {
                        panic!("no list");
                    };
                    let row = rows
                        .into_iter()
                        .find(|row| row.id == shared.infohash)
                        .expect("the remote download is listed");
                    if row.verified_bytes < row.total_bytes {
                        seen_incomplete = true;
                    } else if matches!(row.phase, TransferPhase::Seeding | TransferPhase::Paused) {
                        break row;
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                };
                assert!(seen_incomplete, "progress rises from below the total");
                assert_eq!(row.total_bytes, bytes.len() as u64);
                let downloaded = remote_downloads
                    .canonicalize()
                    .unwrap()
                    .join(&shared.infohash)
                    .join("Film.mp4");
                assert_eq!(std::fs::read(&downloaded).unwrap(), bytes);
                assert_eq!(row.files[0].path, "Film.mp4");
                assert_eq!(
                    row.files[0].video_id,
                    Some(ids::video_id(&salt, &downloaded.canonicalize().unwrap()))
                );

                let Ok(TransferReply::Stream(stream)) = call(TransferCommand::OpenStream {
                    id: shared.infohash.clone(),
                    file_index: 0,
                })
                .await
                else {
                    panic!("no stream");
                };
                assert_eq!(stream.len(), bytes.len() as u64);
                drop(stream);
                for missing in [
                    TransferCommand::OpenStream {
                        id: shared.infohash.clone(),
                        file_index: 5,
                    },
                    TransferCommand::Pause { id: "f".repeat(40) },
                    TransferCommand::Resume { id: "../x".into() },
                    TransferCommand::FetchCancel { id: 7 },
                ] {
                    assert_eq!(call(missing).await.err().map(|e| e.status), Some(404));
                }
                assert!(matches!(
                    call(TransferCommand::FetchList).await,
                    Ok(TransferReply::Fetches(rows)) if rows.is_empty()
                ));

                // The wrong list deletes nothing and names the right one.
                let wrong = call(TransferCommand::Remove {
                    id: shared.infohash.clone(),
                    delete_files: true,
                    confirm: vec!["Other.mp4".into()],
                })
                .await
                .err()
                .expect("a wrong list is refused");
                assert_eq!(wrong.status, 409);
                assert_eq!(wrong.message.lines().last(), Some("Film.mp4"));
                assert!(downloaded.is_file());
                assert!(matches!(
                    call(TransferCommand::Remove {
                        id: shared.infohash.clone(),
                        delete_files: true,
                        confirm: vec!["Film.mp4".into()],
                    })
                    .await,
                    Ok(TransferReply::Done)
                ));
                assert!(!downloaded.exists());
                let Ok(TransferReply::Transfers(rows)) = call(TransferCommand::List).await else {
                    panic!("no list");
                };
                assert!(rows.iter().all(|row| row.id != shared.infohash));

                let Ok(TransferReply::Browse(top)) =
                    call(TransferCommand::Browse { dir: String::new() }).await
                else {
                    panic!("no browse");
                };
                assert_eq!(top.entries.len(), 1);
                assert_eq!(top.entries[0].name, "Shared Videos");
                let Ok(TransferReply::Browse(inside)) = call(TransferCommand::Browse {
                    dir: "Shared Videos".into(),
                })
                .await
                else {
                    panic!("no browse");
                };
                assert_eq!(inside.entries.len(), 1);
                assert_eq!(inside.entries[0].name, "Clip.mp4");
                assert_eq!(
                    call(TransferCommand::Browse {
                        dir: "Shared Videos/..".into()
                    })
                    .await
                    .err()
                    .map(|e| e.status),
                    Some(404)
                );
                assert_eq!(
                    call(TransferCommand::Share {
                        dir: "seed-source/Film.mp4".into()
                    })
                    .await
                    .err()
                    .map(|e| e.status),
                    Some(404)
                );
                let Ok(TransferReply::Shared(seeded)) = call(TransferCommand::Share {
                    dir: "Shared Videos/Clip.mp4".into(),
                })
                .await
                else {
                    panic!("not shared");
                };
                assert!(ids::is_valid_id(&seeded.infohash), "{}", seeded.infohash);
                assert!(seeded.magnet.starts_with("magnet:?"));
                let Ok(TransferReply::Transfers(rows)) = call(TransferCommand::List).await else {
                    panic!("no list");
                };
                assert!(rows.iter().any(|row| row.id == seeded.infohash));
            })
            .await
        });
        serve.stop();
        if let Some(thread) = serve.stopping.take() {
            thread.join().unwrap();
        }
        services.begin_shutdown();
        crate::services::finish_shutdown();
        runtime.block_on(seed.stop());
        outcome.expect("the remote download finished in time");
    }

    #[test]
    fn a_refused_address_is_reported_inline() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("settings.json"), "{}").unwrap();
        let mut serve = RemoteServe::default();
        let mut refused = options(dir.path(), false);
        refused.bind = Some("192.168.1.10:0".parse().unwrap());
        serve.start(refused);
        let ServeStatus::Failed(message) =
            wait_for(&mut serve, |s| !matches!(s, ServeStatus::Starting))
        else {
            panic!("a LAN address must be refused");
        };
        assert!(message.contains("not a Tailscale address"), "{message}");
        assert!(serve.running.is_none());
    }
}
