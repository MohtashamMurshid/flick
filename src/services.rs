//! Background search and one shared native transfer session for the desktop.
use flick::subtitles::SubtitleResult;
use flick::{
    catalog::{self, SearchResults},
    fetch::{self, FetchSource, FetchUpdate, archive::ArchiveItem},
    torrent::{
        ResolvedTorrent, SharedTorrent, TorrentInput, TransferConfig, TransferSession,
        TransferSnapshot, TransferState, confirm_saved_owner_absent, confirm_saved_transfers_empty,
        inspect_saved_transfer_ownership,
    },
};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::PathBuf,
    sync::{Arc, Mutex, mpsc},
    thread,
    time::Duration,
};
use tokio::{
    sync::{mpsc as channel, oneshot},
    task::JoinSet,
    time::Instant,
};
use tokio_util::sync::CancellationToken;

pub enum Command {
    /// Download a web page's video with yt-dlp, or files over HTTPS. These
    /// run apart from the torrent session.
    Fetch {
        id: u64,
        source: FetchSource,
        folder: PathBuf,
    },
    CancelFetch(u64),
    /// List an Internet Archive item's video files.
    ResolveArchive(String),
    /// Search OpenSubtitles for a video, by title and file hash.
    SubtitleSearch {
        generation: u64,
        key: String,
        query: String,
        video: Option<PathBuf>,
        languages: String,
    },
    /// Download a subtitle next to its video, or into `fallback` when that
    /// folder cannot be written.
    SubtitleDownload {
        key: String,
        result: SubtitleResult,
        folder: PathBuf,
        fallback: PathBuf,
        stem: String,
    },
    Search {
        generation: u64,
        query: String,
    },
    Resolve(TorrentInput),
    CancelResolve,
    Download {
        torrent: ResolvedTorrent,
        files: Vec<usize>,
        folder: PathBuf,
    },
    Pause(String),
    Resume(String),
    Remove {
        id: String,
        delete_files: bool,
    },
    ReleaseForDeletion {
        id: String,
        reply: DeletionReply,
    },
    Export {
        id: String,
        path: PathBuf,
    },
    PauseAll,
    ResumeAll,
    ConfigureTransfers {
        seed_completed: bool,
        download_bps: u32,
        upload_bps: u32,
    },
    Share(PathBuf),
    Load,
    /// Explicit retry after a terminal startup failure. Load only asks for readiness.
    RetryTransfers,
    CancelSearch,
    Shutdown,
}
pub enum Event {
    Fetch {
        id: u64,
        update: FetchUpdate,
    },
    ArchiveResolved {
        identifier: String,
        result: Result<ArchiveItem, String>,
    },
    Subtitles {
        generation: u64,
        result: Result<Vec<SubtitleResult>, String>,
    },
    SubtitleSaved(Result<PathBuf, String>),
    Search {
        generation: u64,
        result: Result<SearchResults, String>,
        done: bool,
    },
    Resolved(Result<ResolvedTorrent, String>),
    ResolveCancelled,
    ShareChecking(PathBuf),
    Shared(Result<SharedTorrent, String>),
    /// Explicit Download admission, never automatic startup/completion. Paths
    /// come from the accepted engine handle, which may predate this request.
    DownloadAccepted {
        infohash: String,
        selected: Vec<(PathBuf, usize)>,
    },
    TorrentCompleted {
        infohash: String,
        file_index: usize,
        path: PathBuf,
    },
    Progress(Vec<TransferSnapshot>),
    /// Complete canonical ownership boundaries, available before engine startup.
    TransferRoots(Vec<PathBuf>),
    TransferAvailability(TransferAvailability),
    Completed(PathBuf),
    Error(String),
}

/// Startup state is separate from live ownership snapshots. In particular, an
/// unavailable engine never implies an empty set of transfers. Only complete
/// roots can exclude ownership for canonical paths outside every boundary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TransferAvailability {
    Checking,
    /// Startup succeeded; Progress still supplies live ownership/verification.
    Ready,
    Unavailable {
        message: String,
        roots: Vec<PathBuf>,
        roots_complete: bool,
    },
}

/// A reply also fails if its queued command or running task is dropped. A
/// deletion worker must never interpret a closed channel as released ownership.
pub struct DeletionReply(Option<oneshot::Sender<Result<(), String>>>);

impl DeletionReply {
    fn finish(mut self, result: Result<(), String>) {
        if let Some(reply) = self.0.take() {
            let _ = reply.send(result);
        }
    }
}

impl Drop for DeletionReply {
    fn drop(&mut self) {
        if let Some(reply) = self.0.take() {
            let _ = reply.send(Err(
                "Downloads are unavailable or shutting down; the transfer was not released.".into(),
            ));
        }
    }
}

static SHUTDOWN_WORKERS: ShutdownWorkers = ShutdownWorkers::new();

struct ShutdownWorkers {
    workers: Mutex<Vec<thread::JoinHandle<()>>>,
    joining: Mutex<()>,
}

impl ShutdownWorkers {
    const fn new() -> Self {
        Self {
            workers: Mutex::new(Vec::new()),
            joining: Mutex::new(()),
        }
    }

    fn register(&self, worker: thread::JoinHandle<()>) {
        self.workers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(worker);
    }

    fn finish(&self) {
        // A native quit may race the ordinary asynchronous finalizer. It must
        // wait for that finalizer's joins, even after it drained the registry.
        let _joining = self.joining.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            let workers =
                std::mem::take(&mut *self.workers.lock().unwrap_or_else(|e| e.into_inner()));
            if workers.is_empty() {
                return;
            }
            // Never hold the registration lock across a join: begin_shutdown
            // also runs on window release and must remain nonblocking.
            for worker in workers {
                let _ = worker.join();
            }
        }
    }
}

/// Wait for registered cleanup. Ordinary quit calls this off the UI thread;
/// native termination also uses it as a final barrier after window teardown.
pub fn finish_shutdown() {
    SHUTDOWN_WORKERS.finish();
}

pub struct Services {
    sender: channel::UnboundedSender<Command>,
    events: mpsc::Receiver<Event>,
    worker: Option<thread::JoinHandle<()>>,
    shutdown: CancellationToken,
    pub download_dir: PathBuf,
    /// Opens torrent files for playback while they download.
    pub streams: Arc<StreamSource>,
}

/// Lets the player read torrent files from its own threads while they
/// download, through the transfer runtime. Only verified pieces are read.
#[derive(Default)]
pub struct StreamSource {
    runtime: std::sync::OnceLock<tokio::runtime::Handle>,
    session: std::sync::Mutex<Option<Arc<TransferSession>>>,
}

impl StreamSource {
    /// Opens a `flicktorrent://<infohash>/<file index>` URI. Blocks while
    /// the torrent initializes; call it from a player thread.
    pub fn open(&self, uri: &str) -> anyhow::Result<flick::torrent::BlockingStream> {
        use anyhow::Context as _;
        let (infohash, index) = parse_stream_uri(uri).context("Not a Flick torrent stream")?;
        let runtime = self.runtime.get().context("Downloads are not running")?;
        let session = self
            .session
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .context("Downloads are not running")?;
        let stream = runtime.block_on(async {
            tokio::time::timeout(
                Duration::from_secs(60),
                session.open_stream(&infohash, index),
            )
            .await
            .context("The torrent did not start in time")?
        })?;
        Ok(stream.into_blocking(runtime))
    }
}

/// The URI the player loads for a torrent file that is still downloading.
pub fn stream_uri(infohash: &str, index: usize) -> String {
    format!("{STREAM_PROTOCOL}://{infohash}/{index}")
}

pub const STREAM_PROTOCOL: &str = "flicktorrent";

fn parse_stream_uri(uri: &str) -> Option<(String, usize)> {
    let rest = uri.strip_prefix(STREAM_PROTOCOL)?.strip_prefix("://")?;
    let (infohash, index) = rest.split_once('/')?;
    let valid = infohash.len() == 40 && infohash.bytes().all(|b| b.is_ascii_hexdigit());
    valid.then(|| Some((infohash.to_ascii_lowercase(), index.parse().ok()?)))?
}

/// A torrent stream as the player's stream interface.
pub struct TorrentMediaStream(pub flick::torrent::BlockingStream);

impl crate::playback::MediaStream for TorrentMediaStream {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        self.0.read(buffer)
    }

    fn seek(&mut self, offset: u64) -> std::io::Result<u64> {
        self.0.seek(offset)
    }

    fn size(&self) -> Option<u64> {
        Some(self.0.len())
    }

    fn canceller(&self) -> Arc<dyn Fn() + Send + Sync> {
        let cancel = self.0.canceller();
        Arc::new(move || cancel.cancel())
    }
}

impl Services {
    pub fn start(preferences: &crate::store::TransferPreferences) -> Self {
        let base = std::env::var_os("FLICK_DATA_DIR")
            .map(PathBuf::from)
            .or_else(|| {
                directories::ProjectDirs::from("app", "Flick", "Flick")
                    .map(|d| d.data_local_dir().to_owned())
            })
            .unwrap_or_else(|| std::env::temp_dir().join("flick"));
        let download_dir = std::env::var_os("FLICK_DOWNLOAD_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                directories::UserDirs::new()
                    .and_then(|d| d.download_dir().map(PathBuf::from))
                    .unwrap_or_else(|| base.clone())
                    .join("Flick")
            });
        let config = TransferConfig {
            download_dir: download_dir.clone(),
            state_dir: base.join("transfers"),
            listen_addr: (
                if std::env::var_os("FLICK_LOCAL_TRANSFERS").is_some() {
                    [127, 0, 0, 1]
                } else {
                    [0, 0, 0, 0]
                },
                0,
            )
                .into(),
            public_discovery: std::env::var_os("FLICK_LOCAL_TRANSFERS").is_none(),
            listener_mode: if std::env::var_os("FLICK_LOCAL_TRANSFERS").is_some() {
                librqbit::ListenerMode::TcpOnly
            } else {
                librqbit::ListenerMode::TcpAndUtp
            },
            // FLICK_DOWNLOAD_LIMIT, in bytes per second, overrides the saved
            // limit to check playback while downloading from a fast local seed.
            download_limit_bytes_per_second: std::env::var("FLICK_DOWNLOAD_LIMIT")
                .ok()
                .and_then(|limit| limit.parse().ok())
                .or(std::num::NonZeroU32::new(preferences.download_bps)),
            upload_limit_bytes_per_second: std::num::NonZeroU32::new(preferences.upload_bps),
            seed_completed_downloads: preferences.seed_completed.unwrap_or(false),
        };
        let peers = std::env::var("FLICK_INITIAL_PEERS")
            .unwrap_or_default()
            .split(',')
            .filter_map(|v| v.parse().ok())
            .collect::<Vec<std::net::SocketAddr>>();
        Self::start_config(config, peers)
    }

    fn start_config(config: TransferConfig, peers: Vec<std::net::SocketAddr>) -> Self {
        let download_dir = config.download_dir.clone();
        let (sender, receiver) = channel::unbounded_channel();
        let (events_tx, events) = mpsc::channel();
        let streams = Arc::new(StreamSource::default());
        let worker_streams = streams.clone();
        let shutdown = CancellationToken::new();
        let worker_shutdown = shutdown.clone();
        let worker = thread::Builder::new()
            .name("flick-transfers".into())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        let failure =
                            StartupFailure::from(format!("Downloads could not start: {error}"));
                        let _ = events_tx.send(failure.event());
                        let _ = events_tx.send(Event::Error(failure.message));
                        return;
                    }
                };
                runtime.block_on(run_with_shutdown(
                    config,
                    peers,
                    receiver,
                    events_tx,
                    worker_streams,
                    worker_shutdown,
                ));
                runtime.shutdown_timeout(Duration::from_secs(2));
            })
            .expect("create Flick background worker");
        Self {
            sender,
            events,
            worker: Some(worker),
            shutdown,
            download_dir,
            streams,
        }
    }
    pub fn send(&self, command: Command) -> Result<(), String> {
        if self.shutdown.is_cancelled() {
            return Err("Downloads are shutting down.".into());
        }
        self.sender
            .send(command)
            .map_err(|_| "Downloads are unavailable. Restart Flick to try again.".into())
    }
    pub fn drain(&self) -> impl Iterator<Item = Event> + '_ {
        self.events.try_iter()
    }

    pub fn release_for_deletion(&self, id: String) -> oneshot::Receiver<Result<(), String>> {
        let (reply, receiver) = oneshot::channel();
        let _ = self.send(Command::ReleaseForDeletion {
            id,
            reply: DeletionReply(Some(reply)),
        });
        receiver
    }

    /// Signal immediately without joining a worker on the UI thread.
    pub fn begin_shutdown(&mut self) {
        let _ = self.sender.send(Command::Shutdown);
        self.shutdown.cancel();
        if let Some(worker) = self.worker.take() {
            SHUTDOWN_WORKERS.register(worker);
        }
    }
}
impl Drop for Services {
    fn drop(&mut self) {
        self.shutdown.cancel();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

const MAX_PENDING_TRANSFERS: usize = 32;
const ACTIVE_POLL: Duration = Duration::from_millis(700);
const MAX_IDLE_POLL: Duration = Duration::from_secs(10);
type StartupResult = Result<Option<Arc<TransferSession>>, StartupFailure>;

#[derive(Debug)]
struct StartupFailure {
    message: String,
    roots: Vec<PathBuf>,
    roots_complete: bool,
}

impl From<String> for StartupFailure {
    fn from(message: String) -> Self {
        Self {
            message,
            roots: Vec::new(),
            roots_complete: false,
        }
    }
}

impl From<&str> for StartupFailure {
    fn from(message: &str) -> Self {
        Self::from(message.to_owned())
    }
}

impl StartupFailure {
    fn event(&self) -> Event {
        Event::TransferAvailability(TransferAvailability::Unavailable {
            message: self.message.clone(),
            roots: self.roots.clone(),
            roots_complete: self.roots_complete,
        })
    }
}

fn start_session(
    config: TransferConfig,
    saved_only: bool,
    events: mpsc::Sender<Event>,
    shutdown: CancellationToken,
) -> tokio::task::JoinHandle<StartupResult> {
    let _ = events.send(Event::TransferAvailability(TransferAvailability::Checking));
    tokio::spawn(async move {
        let mut roots = Vec::new();
        let mut roots_complete = false;
        let start = async {
            // Inspect on fresh admission too, so its later failure retains the
            // same exclusion evidence as automatic startup.
            let evidence = inspect_saved_transfer_ownership(&config.state_dir).await;
            roots = evidence.roots.clone();
            roots_complete = evidence.roots_complete;
            if saved_only
                && evidence.error.is_none()
                && !evidence.has_saved_transfers()
                && let Err(error) = confirm_saved_transfers_empty(&config.state_dir).await
            {
                roots_complete = false;
                return Err(format!(
                    "Downloads are unavailable: could not confirm empty saved ownership: {error:#}"
                ));
            }
            if roots_complete {
                events
                    .send(Event::TransferRoots(roots.clone()))
                    .map_err(|_| "The download interface closed.".to_string())?;
            }
            if let Some(error) = &evidence.error {
                return Err(format!(
                    "Downloads are unavailable: could not establish saved transfer ownership: {error}"
                ));
            }
            if saved_only && !evidence.has_saved_transfers() {
                return Ok(None);
            }
            TransferSession::start_cancellable(config, shutdown.child_token())
                .await
                .map(|session| Some(Arc::new(session)))
                .map_err(|e| format!("Downloads are unavailable: {e:#}"))
        };
        let result = tokio::select! {
            biased;
            _ = shutdown.cancelled() => Err("Downloads shut down during startup.".into()),
            result = tokio::time::timeout(Duration::from_secs(120), start) => {
                result.unwrap_or_else(|_| Err("Downloads are unavailable: startup timed out.".into()))
            }
        };
        result.map_err(|message| StartupFailure {
            message,
            roots,
            roots_complete,
        })
    })
}

#[cfg(test)]
async fn run(
    config: TransferConfig,
    initial_peers: Vec<std::net::SocketAddr>,
    commands: channel::UnboundedReceiver<Command>,
    events: mpsc::Sender<Event>,
    streams: Arc<StreamSource>,
) {
    run_with_shutdown(
        config,
        initial_peers,
        commands,
        events,
        streams,
        CancellationToken::new(),
    )
    .await;
}

async fn run_with_shutdown(
    config: TransferConfig,
    initial_peers: Vec<std::net::SocketAddr>,
    commands: channel::UnboundedReceiver<Command>,
    events: mpsc::Sender<Event>,
    streams: Arc<StreamSource>,
    shutdown: CancellationToken,
) {
    let startup = start_session(config.clone(), true, events.clone(), shutdown.clone());
    run_loop(
        config,
        initial_peers,
        commands,
        events,
        streams,
        shutdown,
        startup,
    )
    .await;
}

async fn run_loop(
    mut config: TransferConfig,
    initial_peers: Vec<std::net::SocketAddr>,
    mut commands: channel::UnboundedReceiver<Command>,
    events: mpsc::Sender<Event>,
    streams: Arc<StreamSource>,
    shutdown: CancellationToken,
    startup: tokio::task::JoinHandle<StartupResult>,
) {
    let _ = streams.runtime.set(tokio::runtime::Handle::current());
    let mut session: Option<Arc<TransferSession>> = None;
    let mut startup = Some(startup);
    let mut startup_error: Option<String> = None;
    let mut pending = VecDeque::new();
    // Mutations run in order, apart from searches/metadata resolution. In
    // particular, a prior resume cannot re-register a transfer after release.
    let mut transfer_job: Option<tokio::task::JoinHandle<Option<DownloadAcceptance>>> = None;
    let changed = Arc::new(tokio::sync::Notify::new());
    let mut tasks = JoinSet::new();
    let mut search_task: Option<tokio::task::AbortHandle> = None;
    let mut resolve_task: Option<tokio::task::JoinHandle<()>> = None;
    let mut progress = ProgressPublisher::default();
    let mut next_poll: Option<Instant> = None;
    let mut poll_delay = ACTIVE_POLL;
    let mut dirty = false;
    let mut configure = false;
    let mut fetches = Fetches::default();
    loop {
        if shutdown.is_cancelled() {
            break;
        }
        if let Some(ready) = &session {
            // Resolution stays responsive even behind a long verification or
            // share operation. Only the most recent waiting request survives.
            if let Some(position) = pending
                .iter()
                .position(|c| matches!(c, Command::Resolve(_)))
            {
                let Command::Resolve(input) = pending.remove(position).unwrap() else {
                    unreachable!()
                };
                if let Some(task) = resolve_task.take() {
                    task.abort();
                    let _ = task.await;
                }
                let (session, peers, events) =
                    (ready.clone(), initial_peers.clone(), events.clone());
                resolve_task = Some(tokio::spawn(async move {
                    let result = session
                        .resolve(input, peers, Duration::from_secs(300))
                        .await
                        .map_err(|e| format!("Could not read torrent files: {e:#}"));
                    let _ = events.send(Event::Resolved(result));
                }));
            }
            if configure {
                ready.set_limits(
                    config.download_limit_bytes_per_second,
                    config.upload_limit_bytes_per_second,
                );
                ready.request_seed_completed_downloads(config.seed_completed_downloads);
                configure = false;
                dirty = true;
            }
            if transfer_job.is_none() {
                let command = pending.pop_front();
                if let Some(command) = command {
                    transfer_job = Some(tokio::spawn(transfer_command(
                        command,
                        ready.clone(),
                        initial_peers.clone(),
                        events.clone(),
                        changed.clone(),
                    )));
                    dirty = true;
                }
            }
            if dirty {
                match progress.publish(ready, &events, transfer_job.is_none()) {
                    Ok((changed, active)) => {
                        poll_delay = if active || changed || transfer_job.is_some() {
                            ACTIVE_POLL
                        } else {
                            (poll_delay * 2).min(MAX_IDLE_POLL)
                        };
                        next_poll = if progress.last.as_ref().is_some_and(Vec::is_empty)
                            && transfer_job.is_none()
                        {
                            None
                        } else {
                            Some(Instant::now() + poll_delay)
                        };
                    }
                    Err(error) => {
                        let message = format!("Could not update downloads: {error:#}");
                        if progress.error.as_ref() != Some(&message) {
                            let _ = events.send(Event::Error(message.clone()));
                            progress.error = Some(message);
                        }
                        next_poll = Some(Instant::now() + MAX_IDLE_POLL);
                    }
                }
                dirty = false;
            }
        } else if startup.is_none() && startup_error.is_none() {
            // A release is not an admission request. Confirm absence under the
            // disk lock without initializing an engine, listener or discovery.
            if transfer_job.is_none() {
                if matches!(pending.front(), Some(Command::ReleaseForDeletion { .. })) {
                    let Some(Command::ReleaseForDeletion { id, reply }) = pending.pop_front()
                    else {
                        unreachable!()
                    };
                    let state_dir = config.state_dir.clone();
                    transfer_job = Some(tokio::spawn(async move {
                        reply.finish(
                            confirm_saved_owner_absent(&state_dir, &id)
                                .await
                                .map_err(|e| format!("Could not confirm transfer release: {e:#}")),
                        );
                        None
                    }));
                } else if !pending.is_empty() {
                    startup = Some(start_session(
                        config.clone(),
                        false,
                        events.clone(),
                        shutdown.clone(),
                    ));
                }
            }
        }
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => break,
            result = async { startup.as_mut().unwrap().await }, if startup.is_some() => {
                startup = None;
                match result.unwrap_or_else(|e| Err(StartupFailure::from(format!("Downloads are unavailable: startup stopped: {e}")))) {
                    Ok(Some(ready)) => {
                        *streams.session.lock().unwrap_or_else(|e| e.into_inner()) = Some(ready.clone());
                        session = Some(ready);
                        progress.begin_ready_epoch();
                        let _ = events.send(Event::TransferAvailability(TransferAvailability::Ready));
                        // Apply preferences changed while startup was running.
                        configure = true;
                        dirty = true;
                    }
                    Ok(None) => {
                        progress.begin_ready_epoch();
                        let _ = events.send(Event::TransferAvailability(TransferAvailability::Ready));
                        progress.empty(&events);
                        pending.retain(|command| !matches!(command, Command::Load));
                    }
                    Err(error) => {
                        let _ = events.send(error.event());
                        let _ = events.send(Event::Error(error.message.clone()));
                        while let Some(command) = pending.pop_front() {
                            reject_transfer(command, &error.message, &events);
                        }
                        startup_error = Some(error.message);
                    }
                }
            }
            result = async { transfer_job.as_mut().unwrap().await }, if transfer_job.is_some() => {
                transfer_job = None;
                match result {
                    Ok(Some(accepted)) => {
                        // Re-arm exactly these files, including a completed
                        // transfer explicitly downloaded again after removal.
                        progress.accept_download(&accepted);
                    }
                    Ok(None) => {}
                    Err(error) => {
                        let _ = events.send(Event::Error(format!("Transfer operation stopped: {error}")));
                    }
                }
                dirty = true;
            }
            _ = changed.notified() => { dirty = true; }
            _ = tokio::time::sleep_until(next_poll.unwrap_or_else(Instant::now)), if next_poll.is_some() => {
                next_poll = None;
                dirty = true;
            }
            command = commands.recv() => {
                let Some(command) = command else { break; };
                if matches!(command, Command::Shutdown) { break; }
                let Some(command) = fetches.handle(command, &events) else { continue; };
                let Some(command) = subtitles(command, &events) else { continue; };
                if matches!(command, Command::CancelSearch) {
                    if let Some(task) = search_task.take() { task.abort(); }
                    continue;
                }
                if let Command::Search { generation, query } = command {
                    if let Some(task) = search_task.take() { task.abort(); }
                    let events = events.clone();
                    search_task = Some(tasks.spawn(async move {
                        let result = catalog::search(&query, |partial| { let _ = events.send(Event::Search {generation, result:Ok(partial), done:false}); }).await.map_err(|e| e.to_string());
                        let _ = events.send(Event::Search { generation, result, done:true });
                    }));
                    continue;
                }
                if matches!(command, Command::CancelResolve) {
                    pending.retain(|c| !matches!(c, Command::Resolve(_)));
                    if let Some(task) = resolve_task.take() { task.abort(); let _ = task.await; }
                    let _ = events.send(Event::ResolveCancelled);
                    continue;
                }
                if let Command::ConfigureTransfers { seed_completed, download_bps, upload_bps } = command {
                    config.seed_completed_downloads = seed_completed;
                    config.download_limit_bytes_per_second = std::num::NonZeroU32::new(download_bps);
                    config.upload_limit_bytes_per_second = std::num::NonZeroU32::new(upload_bps);
                    configure = true;
                    continue;
                }
                if matches!(command, Command::RetryTransfers) {
                    if session.is_none() && startup.is_none() {
                        startup_error = None;
                        startup = Some(start_session(config.clone(), true, events.clone(), shutdown.clone()));
                    }
                    continue;
                }
                if let Some(error) = &startup_error {
                    reject_transfer(command, error, &events);
                    continue;
                }
                if matches!(command, Command::Resolve(_)) {
                    pending.retain(|c| !matches!(c, Command::Resolve(_)));
                    if let Some(task) = resolve_task.take() { task.abort(); let _ = task.await; }
                }
                // Load is a readiness request, never a second startup.
                if matches!(command, Command::Load) {
                    if session.is_some() { dirty = true; continue; }
                    if startup.is_none() { progress.empty(&events); continue; }
                    if pending.iter().any(|c| matches!(c, Command::Load)) { continue; }
                }
                if pending.len() >= MAX_PENDING_TRANSFERS {
                    reject_transfer(command, "Too many pending transfer commands. Try again when downloads are ready.", &events);
                } else { pending.push_back(command); }
            }
            _ = tasks.join_next(), if !tasks.is_empty() => {}
        }
    }
    shutdown.cancel();
    // Stop admission before draining, including commands not yet pulled from
    // the public channel. Every deletion waiter receives a concrete failure.
    commands.close();
    while let Ok(command) = commands.try_recv() {
        pending.push_back(command);
    }
    while let Some(command) = pending.pop_front() {
        reject_transfer(
            command,
            "Downloads shut down before the command completed.",
            &events,
        );
    }
    if let Some(task) = startup {
        // Cancellation is selected in the startup task and reaches partial
        // engine initialization; await it so its state-directory lock is gone.
        match task.await {
            Ok(Ok(Some(ready))) => {
                if session.is_none() {
                    session = Some(ready);
                }
            }
            Ok(Err(_)) | Ok(Ok(None)) => {}
            Err(error) => {
                let _ = events.send(Event::Error(format!(
                    "Transfer startup cleanup failed: {error}"
                )));
            }
        }
    }
    if let Some(task) = resolve_task {
        task.abort();
        let _ = task.await;
    }
    if let Some(task) = transfer_job {
        task.abort();
        let _ = task.await;
    }
    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
    fetches.shutdown(&events).await;
    streams
        .session
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take();
    if let Some(session) = session {
        session.shutdown().await;
    }
}

fn reject_transfer(command: Command, error: &str, events: &mpsc::Sender<Event>) {
    match command {
        Command::ReleaseForDeletion { reply, .. } => reply.finish(Err(error.into())),
        Command::Resolve(_) => {
            let _ = events.send(Event::Resolved(Err(error.into())));
        }
        Command::Share(_) => {
            let _ = events.send(Event::Shared(Err(error.into())));
        }
        Command::Shutdown => {}
        _ => {
            let _ = events.send(Event::Error(error.into()));
        }
    }
}

struct DownloadAcceptance {
    infohash: String,
    selected: Vec<(PathBuf, usize)>,
}

async fn transfer_command(
    command: Command,
    session: Arc<TransferSession>,
    peers: Vec<std::net::SocketAddr>,
    events: mpsc::Sender<Event>,
    changed: Arc<tokio::sync::Notify>,
) -> Option<DownloadAcceptance> {
    let mut accepted = None;
    let result: anyhow::Result<()> = match command {
        Command::Download {
            torrent,
            files,
            folder,
        } => match session.download_to(&torrent, Some(files), &folder).await {
            Ok(id) => {
                // Capture actual engine destinations rather than the requested
                // folder: accepting an existing transfer keeps its original root.
                match session.selected_file_paths(&id) {
                    Ok(files) => {
                        let admission = DownloadAcceptance {
                            infohash: id.clone(),
                            selected: files
                                .into_iter()
                                .filter(|(path, _)| flick::library::is_video(path))
                                .collect(),
                        };
                        // Do not wait for a potentially long verification in
                        // resume: this explicit admission must survive a quit
                        // while the accepted transfer is still checking.
                        let _ = events.send(Event::DownloadAccepted {
                            infohash: admission.infohash.clone(),
                            selected: admission.selected.clone(),
                        });
                        accepted = Some(admission);
                    }
                    Err(error) => {
                        let _ = events.send(Event::Error(format!(
                            "Could not identify accepted download files: {error:#}"
                        )));
                    }
                }
                changed.notify_one();
                session.resume(&id, peers).await
            }
            Err(error) => Err(error),
        },
        Command::Pause(id) => session.pause(&id).await,
        Command::Resume(id) => session.resume(&id, peers).await,
        Command::Remove { id, delete_files } => session.remove(&id, delete_files).await,
        Command::ReleaseForDeletion { id, reply } => {
            let result = session
                .release_for_deletion(&id)
                .await
                .map_err(|e| format!("Could not release the transfer: {e:#}"));
            reply.finish(result);
            Ok(())
        }
        Command::Export { id, path } => session.export_torrent(&id, &path).await,
        Command::PauseAll => session.pause_all().await,
        Command::ResumeAll => session.resume_all(peers).await,
        Command::ConfigureTransfers {
            seed_completed,
            download_bps,
            upload_bps,
        } => {
            session.set_limits(
                std::num::NonZeroU32::new(download_bps),
                std::num::NonZeroU32::new(upload_bps),
            );
            session.set_seed_completed_downloads(seed_completed).await;
            Ok(())
        }
        Command::Share(path) => {
            let _ = events.send(Event::ShareChecking(path.clone()));
            let result = session
                .share(&path, vec![])
                .await
                .map_err(|e| format!("Could not share these files: {e:#}"));
            let _ = events.send(Event::Shared(result));
            Ok(())
        }
        _ => Ok(()),
    };
    if let Err(error) = result {
        let _ = events.send(Event::Error(format!("Transfer failed: {error:#}")));
    }
    accepted
}

#[derive(Default)]
struct ProgressPublisher {
    last: Option<Vec<TransferSnapshot>>,
    completed: HashSet<(String, usize, PathBuf)>,
    error: Option<String>,
}

impl ProgressPublisher {
    fn begin_ready_epoch(&mut self) {
        // Checking invalidates the UI's ownership snapshot. Even identical
        // engine state must be sent again after successful startup. Completion
        // identities survive, so readiness alone never replays completion.
        self.last = None;
        self.error = None;
    }

    fn accept_download(&mut self, accepted: &DownloadAcceptance) {
        // Acceptance also invalidates the UI snapshot for the selected files.
        self.last = None;
        let selected = accepted
            .selected
            .iter()
            .map(|(path, index)| (path, index))
            .collect::<HashSet<_>>();
        self.completed.retain(|(hash, index, path)| {
            hash != &accepted.infohash || !selected.contains(&(path, index))
        });
    }

    fn empty(&mut self, events: &mpsc::Sender<Event>) {
        if self.last.as_ref().is_none_or(|s| !s.is_empty()) {
            let _ = events.send(Event::Progress(Vec::new()));
            self.last = Some(Vec::new());
        }
    }

    fn publish(
        &mut self,
        session: &TransferSession,
        events: &mpsc::Sender<Event>,
        publish_completions: bool,
    ) -> anyhow::Result<(bool, bool)> {
        let snapshots = session.snapshots()?;
        self.error = None;
        // One linear membership index replaces a snapshot/file scan for every
        // prior completion. Include the destination when an identity is reused.
        let live = snapshots
            .iter()
            .flat_map(|s| {
                s.files
                    .iter()
                    .map(|f| (s.infohash.clone(), f.index, f.path.clone()))
            })
            .collect::<HashSet<_>>();
        self.completed.retain(|key| live.contains(key));
        for snapshot in snapshots.iter().filter(|_| publish_completions) {
            if matches!(
                snapshot.state,
                TransferState::Checking | TransferState::Error
            ) {
                continue;
            }
            for file in &snapshot.files {
                if !file.selected
                    || file.verified_bytes != file.length
                    || !flick::library::is_video(&file.path)
                {
                    continue;
                }
                let key = (snapshot.infohash.clone(), file.index, file.path.clone());
                if !self.completed.contains(&key)
                    && let Ok(path) = session.completed_file(&snapshot.infohash, file.index)
                {
                    self.completed.insert(key);
                    let _ = events.send(Event::TorrentCompleted {
                        infohash: snapshot.infohash.clone(),
                        file_index: file.index,
                        path,
                    });
                }
            }
        }
        let active = snapshots.iter().any(|s| {
            matches!(
                s.state,
                TransferState::Checking | TransferState::Downloading
            ) || s.download_bytes_per_second > 0
                || s.upload_bytes_per_second > 0
        });
        let changed = self.last.as_ref() != Some(&snapshots);
        if changed {
            events
                .send(Event::Progress(snapshots.clone()))
                .map_err(|_| anyhow::anyhow!("Download interface closed"))?;
            self.last = Some(snapshots);
        }
        Ok((changed, active))
    }
}

/// Starts OpenSubtitles searches and downloads, which need no torrent
/// session. Returns any other command.
fn subtitles(command: Command, events: &mpsc::Sender<Event>) -> Option<Command> {
    let events = events.clone();
    match command {
        Command::SubtitleSearch {
            generation,
            key,
            query,
            video,
            languages,
        } => {
            tokio::spawn(async move {
                let result = async {
                    let client = flick::subtitles::Client::new(&key)?;
                    // The hash reads 128 KiB; an unreadable file just
                    // searches by title.
                    let hash = match video {
                        Some(video) => tokio::task::spawn_blocking(move || {
                            flick::subtitles::movie_hash(&video).ok()
                        })
                        .await
                        .ok()
                        .flatten(),
                        None => None,
                    };
                    client.search(&query, hash.as_deref(), &languages).await
                }
                .await
                .map_err(|e| format!("{e:#}"));
                let _ = events.send(Event::Subtitles { generation, result });
            });
        }
        Command::SubtitleDownload {
            key,
            result,
            folder,
            fallback,
            stem,
        } => {
            tokio::spawn(async move {
                let saved = async {
                    let client = flick::subtitles::Client::new(&key)?;
                    let subtitle = client.download(&result).await?;
                    // Videos on read-only media get the subtitle in the
                    // fallback folder, without downloading it again.
                    match subtitle.save(&folder, &stem) {
                        Err(error)
                            if error.chain().any(|cause| {
                                cause.downcast_ref::<std::io::Error>().is_some_and(|e| {
                                    matches!(
                                        e.kind(),
                                        std::io::ErrorKind::PermissionDenied
                                            | std::io::ErrorKind::ReadOnlyFilesystem
                                    )
                                })
                            }) =>
                        {
                            subtitle.save(&fallback, &stem)
                        }
                        saved => saved,
                    }
                }
                .await
                .map_err(|e| format!("Could not download the subtitle: {e:#}"));
                let _ = events.send(Event::SubtitleSaved(saved));
            });
        }
        command => return Some(command),
    }
    None
}

/// Link downloads, keyed by the identifier the UI chose.
#[derive(Default)]
struct Fetches {
    jobs: HashMap<u64, (CancellationToken, tokio::task::JoinHandle<()>)>,
    retiring: Vec<tokio::task::JoinHandle<()>>,
}

impl Fetches {
    /// Starts or cancels link downloads. Returns any other command.
    fn handle(&mut self, command: Command, events: &mpsc::Sender<Event>) -> Option<Command> {
        self.jobs.retain(|_, (_, job)| !job.is_finished());
        self.retiring.retain(|job| !job.is_finished());
        match command {
            Command::Fetch { id, source, folder } => {
                let cancel = CancellationToken::new();
                let job = tokio::spawn(fetch_job(
                    id,
                    source,
                    folder,
                    cancel.clone(),
                    events.clone(),
                ));
                if let Some((previous, job)) = self.jobs.insert(id, (cancel, job)) {
                    previous.cancel();
                    // Keep replaced jobs until their partial-file cleanup has
                    // finished too; dropping their handles detached that work.
                    self.retiring.push(job);
                }
            }
            Command::CancelFetch(id) => {
                if let Some((cancel, _)) = self.jobs.get(&id) {
                    cancel.cancel();
                }
            }
            Command::ResolveArchive(identifier) => {
                let events = events.clone();
                tokio::spawn(async move {
                    let result = fetch::archive::resolve(&identifier)
                        .await
                        .map_err(|e| format!("Could not list this Internet Archive item: {e:#}"));
                    let _ = events.send(Event::ArchiveResolved { identifier, result });
                });
            }
            command => return Some(command),
        }
        None
    }

    /// Cancels running downloads and waits briefly for their cleanup.
    async fn shutdown(&mut self, events: &mpsc::Sender<Event>) {
        for (cancel, _) in self.jobs.values() {
            cancel.cancel();
        }
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        for (id, (_, mut job)) in self.jobs.drain() {
            if tokio::time::timeout_at(deadline, &mut job).await.is_err() {
                let _ = events.send(Event::Error(format!(
                    "Download {id} cleanup timed out; a partial file may remain."
                )));
                job.abort();
                let _ = job.await;
            }
        }
        for mut job in self.retiring.drain(..) {
            if tokio::time::timeout_at(deadline, &mut job).await.is_err() {
                let _ = events.send(Event::Error(
                    "Replaced download cleanup timed out; a partial file may remain.".into(),
                ));
                job.abort();
                let _ = job.await;
            }
        }
    }
}

async fn fetch_job(
    id: u64,
    source: FetchSource,
    folder: PathBuf,
    cancel: CancellationToken,
    events: mpsc::Sender<Event>,
) {
    let result = fetch::fetch(source, &folder, &cancel, |update| {
        if let FetchUpdate::Saved(path) = &update
            && flick::library::is_video(path)
        {
            let _ = events.send(Event::Completed(path.clone()));
        }
        let _ = events.send(Event::Fetch { id, update });
    })
    .await;
    let update = match result {
        Ok(_) => FetchUpdate::Done,
        Err(_) if cancel.is_cancelled() => FetchUpdate::Cancelled,
        Err(error) => FetchUpdate::Failed(format!("{error:#}")),
    };
    let _ = events.send(Event::Fetch { id, update });
}

#[cfg(test)]
mod tests {
    use super::*;

    // Keep fixture spellings distinct from engine identities on Unix too.
    // The alias is an ancestor, so the shared files themselves stay regular.
    fn path_alias_fixture() -> anyhow::Result<(tempfile::TempDir, tempfile::TempDir)> {
        let parent = tempfile::tempdir()?;
        let directory = parent.path().join("actual");
        std::fs::create_dir(&directory)?;
        #[cfg(unix)]
        let directory = {
            let alias = parent.path().join("alias");
            std::os::unix::fs::symlink(&directory, &alias)?;
            alias
        };
        let root = tempfile::tempdir_in(directory)?;
        #[cfg(unix)]
        assert_ne!(root.path(), root.path().canonicalize()?);
        Ok((parent, root))
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn readiness_and_acceptance_republish_unchanged_snapshots_with_expected_completions()
    -> anyhow::Result<()> {
        let (_fixture, root) = path_alias_fixture()?;
        let source = root.path().join("complete.mp4");
        std::fs::write(&source, vec![41; 65_537])?;
        let config =
            TransferConfig::local(root.path().join("downloads"), root.path().join("state"));
        let session = TransferSession::start(config).await?;
        let shared = session.share(&source, vec![]).await?;
        let (events, output) = mpsc::channel();
        let mut progress = ProgressPublisher::default();
        progress.publish(&session, &events, true)?;
        let initial = output.try_iter().collect::<Vec<_>>();
        assert!(
            initial
                .iter()
                .any(|event| matches!(event, Event::TorrentCompleted { .. }))
        );
        progress.begin_ready_epoch();
        progress.publish(&session, &events, true)?;
        let mut snapshots = 0;
        for event in output.try_iter() {
            assert!(
                !matches!(event, Event::TorrentCompleted { .. }),
                "Readiness replayed a completion"
            );
            if let Event::Progress(live) = event {
                assert_eq!(live.len(), 1);
                assert_eq!(live[0].infohash, shared.infohash);
                snapshots += 1;
            }
        }
        assert_eq!(snapshots, 1);
        // Model the UI dropping its old snapshot on DownloadAccepted. A quiet
        // engine has identical state, but acceptance must still republish it.
        let unchanged = session.snapshots()?;
        progress.accept_download(&DownloadAcceptance {
            infohash: shared.infohash.clone(),
            selected: vec![(source.canonicalize()?, 0)],
        });
        assert_eq!(session.snapshots()?, unchanged);
        progress.publish(&session, &events, true)?;
        let accepted_events = output.try_iter().collect::<Vec<_>>();
        assert_eq!(
            accepted_events
                .iter()
                .filter(|event| matches!(event, Event::Progress(_)))
                .count(),
            1
        );
        assert_eq!(
            accepted_events
                .iter()
                .filter(|event| matches!(event, Event::TorrentCompleted { .. }))
                .count(),
            1
        );
        session.stop().await;
        Ok(())
    }

    // Model the UI fold: Checking invalidates transfers_known; only Progress
    // restores it. Ready alone does not prove live ownership or verification.
    async fn wait_ready_empty_epoch(output: &mpsc::Receiver<Event>) -> anyhow::Result<bool> {
        tokio::time::timeout(Duration::from_secs(5), async {
            let (mut checking, mut ready, mut known, mut resolved) = (false, false, false, false);
            loop {
                match output.try_recv() {
                    Ok(Event::TransferAvailability(TransferAvailability::Checking)) => {
                        checking = true;
                        ready = false;
                        known = false;
                    }
                    Ok(Event::TransferAvailability(TransferAvailability::Ready)) => ready = true,
                    Ok(Event::Resolved(result)) => {
                        assert!(result.is_err());
                        resolved = true;
                    }
                    Ok(Event::Progress(snapshots)) => {
                        assert!(checking && ready, "Ownership preceded successful readiness");
                        assert!(snapshots.is_empty());
                        known = true;
                    }
                    Ok(
                        Event::TransferAvailability(TransferAvailability::Unavailable {
                            message,
                            ..
                        })
                        | Event::Error(message),
                    ) => anyhow::bail!(message),
                    Err(mpsc::TryRecvError::Disconnected) => anyhow::bail!("Services closed"),
                    _ => {}
                }
                if known {
                    return Ok::<_, anyhow::Error>(resolved);
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await?
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn retry_empty_startup_republishes_authoritative_progress_each_epoch()
    -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        let config =
            TransferConfig::local(root.path().join("downloads"), root.path().join("state"));
        let (commands, receiver) = channel::unbounded_channel();
        let (events, output) = mpsc::channel();
        let streams = Arc::<StreamSource>::default();
        let job = tokio::spawn(run(config, vec![], receiver, events, streams.clone()));
        let result = async {
            wait_ready_empty_epoch(&output).await?;
            for _ in 0..2 {
                commands.send(Command::RetryTransfers)?;
                wait_ready_empty_epoch(&output).await?;
                assert!(
                    streams.session.lock().unwrap().is_none(),
                    "Empty retry created an engine"
                );
            }
            Ok::<_, anyhow::Error>(())
        }
        .await;
        commands.send(Command::Shutdown)?;
        tokio::time::timeout(Duration::from_secs(5), job).await??;
        result
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn lazy_invalid_resolve_republishes_authoritative_empty_engine_progress()
    -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        let config =
            TransferConfig::local(root.path().join("downloads"), root.path().join("state"));
        let (commands, receiver) = channel::unbounded_channel();
        let (events, output) = mpsc::channel();
        let streams = Arc::<StreamSource>::default();
        let job = tokio::spawn(run(config, vec![], receiver, events, streams.clone()));
        let result = async {
            wait_ready_empty_epoch(&output).await?;
            commands.send(Command::Resolve(TorrentInput::Bytes(
                b"invalid metadata".to_vec(),
            )))?;
            let resolved_already_seen = wait_ready_empty_epoch(&output).await?;
            assert!(streams.session.lock().unwrap().is_some());
            if !resolved_already_seen {
                let resolved =
                    wait_event(&output, |event| matches!(event, Event::Resolved(_))).await?;
                assert!(matches!(resolved, Event::Resolved(Err(_))));
            }
            commands.send(Command::Load)?;
            tokio::time::sleep(Duration::from_millis(50)).await;
            assert!(
                !output
                    .try_iter()
                    .any(|event| matches!(event, Event::Progress(_))),
                "Unchanged Load repeatedly published empty progress"
            );
            Ok::<_, anyhow::Error>(())
        }
        .await;
        commands.send(Command::Shutdown)?;
        tokio::time::timeout(Duration::from_secs(5), job).await??;
        result
    }

    #[test]
    fn finalizers_wait_for_each_other_and_include_workers_registered_during_a_join() {
        let workers = Arc::new(ShutdownWorkers::new());
        let (release_first, first_gate) = mpsc::channel();
        workers.register(thread::spawn(move || first_gate.recv().unwrap()));
        let (finished, completion) = mpsc::channel();
        let first = {
            let workers = workers.clone();
            let finished = finished.clone();
            thread::spawn(move || {
                workers.finish();
                finished.send(()).unwrap();
            })
        };
        // Observe the real first finalizer after draining, while its join is
        // blocked. A second caller used to return immediately at this point.
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while !workers.workers.lock().unwrap().is_empty() {
            assert!(std::time::Instant::now() < deadline);
            thread::yield_now();
        }
        let (started, starting) = mpsc::channel();
        let second = {
            let workers = workers.clone();
            thread::spawn(move || {
                started.send(()).unwrap();
                workers.finish();
                finished.send(()).unwrap();
            })
        };
        starting.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(completion.recv_timeout(Duration::from_millis(100)).is_err());

        let (release_late, late_gate) = mpsc::channel();
        let late_worker = thread::spawn(move || late_gate.recv().unwrap());
        let registered = std::time::Instant::now();
        workers.register(late_worker);
        assert!(registered.elapsed() < Duration::from_millis(100));
        release_first.send(()).unwrap();
        // Even the first finalizer must join the worker registered while it
        // waited, before a native termination barrier is allowed to return.
        assert!(completion.recv_timeout(Duration::from_millis(100)).is_err());
        release_late.send(()).unwrap();
        completion.recv_timeout(Duration::from_secs(2)).unwrap();
        completion.recv_timeout(Duration::from_secs(2)).unwrap();
        first.join().unwrap();
        second.join().unwrap();
        workers.finish();
    }

    #[test]
    fn stream_uris_round_trip_and_reject_other_text() {
        let hash = "0123456789abcdef0123456789ABCDEF01234567";
        let uri = stream_uri(hash, 3);
        assert_eq!(uri, format!("flicktorrent://{hash}/3"));
        assert_eq!(parse_stream_uri(&uri), Some((hash.to_ascii_lowercase(), 3)));
        assert_eq!(parse_stream_uri("flicktorrent://short/1"), None);
        assert_eq!(parse_stream_uri(&format!("flicktorrent://{hash}/x")), None);
        assert_eq!(parse_stream_uri("file:///tmp/a.mp4"), None);
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelled_resolution_allows_the_next_request() -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        let config =
            TransferConfig::local(root.path().join("downloads"), root.path().join("state"));
        let source = root.path().join("fixture.bin");
        tokio::fs::write(&source, vec![42u8; 65_537]).await?;
        let seed = TransferSession::start(TransferConfig::local(
            root.path().join("seed-downloads"),
            root.path().join("seed-state"),
        ))
        .await?;
        let shared = seed.share(&source, vec![]).await?;
        seed.stop().await;
        let (commands, receiver) = channel::unbounded_channel();
        let (events, output) = mpsc::channel();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let peer = listener.local_addr()?;
        let (connected, connection) = tokio::sync::oneshot::channel();
        let stalled = tokio::spawn(async move {
            let (_socket, _) = listener.accept().await?;
            let _ = connected.send(());
            std::future::pending::<()>().await;
            Ok::<_, std::io::Error>(())
        });
        let job = tokio::spawn(run(config, vec![peer], receiver, events, Arc::default()));
        // An explicit local peer accepts the connection but supplies no metadata.
        commands.send(Command::Resolve(TorrentInput::Magnet(shared.magnet)))?;
        let outcome = tokio::time::timeout(Duration::from_secs(10), async {
            connection.await?;
            commands.send(Command::CancelResolve)?;
            loop {
                match output.try_recv() {
                    Ok(Event::ResolveCancelled) => break,
                    Ok(Event::Resolved(_) | Event::Error(_)) => {
                        anyhow::bail!("Cancelled request published a result")
                    }
                    _ => tokio::time::sleep(Duration::from_millis(10)).await,
                }
            }
            commands.send(Command::Resolve(TorrentInput::Bytes(
                b"bad metadata".to_vec(),
            )))?;
            loop {
                match output.try_recv() {
                    Ok(Event::Resolved(Err(_))) => return Ok::<_, anyhow::Error>(()),
                    Ok(Event::Error(error)) => anyhow::bail!(error),
                    _ => tokio::time::sleep(Duration::from_millis(10)).await,
                }
            }
        })
        .await;
        commands.send(Command::Shutdown)?;
        job.await?;
        stalled.abort();
        outcome??;
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn background_transfers_publish_completed_videos_and_stop_cleanly() -> anyhow::Result<()>
    {
        let root = tempfile::tempdir()?;
        let video = root.path().join("video.mp4");
        tokio::fs::write(&video, vec![42u8; 65_537]).await?;
        let config =
            TransferConfig::local(root.path().join("downloads"), root.path().join("state"));
        let (commands, receiver) = channel::unbounded_channel();
        let (events, output) = mpsc::channel();
        let job = tokio::spawn(run(config, vec![], receiver, events, Arc::default()));
        commands.send(Command::Share(video.clone()))?;
        let outcome = tokio::time::timeout(Duration::from_secs(12), async {
            let mut shared = false;
            let mut completed = false;
            while !shared || !completed {
                match output.try_recv() {
                    Ok(Event::Shared(Ok(_))) => shared = true,
                    Ok(Event::TorrentCompleted { path, .. }) => {
                        assert_eq!(path, video.canonicalize()?);
                        completed = true;
                    }
                    Ok(Event::Error(error)) | Ok(Event::Shared(Err(error))) => anyhow::bail!(error),
                    _ => tokio::time::sleep(Duration::from_millis(10)).await,
                }
            }
            Ok::<_, anyhow::Error>(())
        })
        .await;
        commands.send(Command::Shutdown)?;
        tokio::time::timeout(Duration::from_secs(5), job).await??;
        outcome??;
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn link_downloads_work_and_cancel_when_the_torrent_session_cannot_start()
    -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        // A file where the state directory belongs, so the session fails.
        let blocked = root.path().join("state");
        std::fs::write(&blocked, b"")?;
        let config = TransferConfig::local(root.path().join("downloads"), blocked);
        let (commands, receiver) = channel::unbounded_channel();
        let (events, output) = mpsc::channel();
        let job = tokio::spawn(run(config, vec![], receiver, events, Arc::default()));
        // Accepts connections but never answers, so the download stays open.
        let stalled = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let folder = root.path().join("links");
        commands.send(Command::Load)?;
        commands.send(Command::Fetch {
            id: 1,
            source: FetchSource::Page("ftp://example.com/video".into()),
            folder: folder.clone(),
        })?;
        commands.send(Command::Fetch {
            id: 2,
            source: FetchSource::Files {
                folder: None,
                files: vec![fetch::RemoteFile {
                    url: format!("https://{}/clip.mp4", stalled.local_addr()?),
                    name: "clip.mp4".into(),
                    size: None,
                    sha1: None,
                }],
            },
            folder: folder.clone(),
        })?;
        let outcome = tokio::time::timeout(Duration::from_secs(12), async {
            let (mut unavailable, mut rejected, mut cancelled, mut cancelling) =
                (false, false, false, false);
            while !(unavailable && rejected && cancelled) {
                match output.try_recv() {
                    Ok(Event::Error(error)) => {
                        assert!(error.contains("Downloads are unavailable"), "{error}");
                        unavailable = true;
                    }
                    Ok(Event::Fetch {
                        id: 1,
                        update: FetchUpdate::Failed(error),
                    }) => {
                        assert!(error.contains("http and https"), "{error}");
                        rejected = true;
                    }
                    Ok(Event::Fetch {
                        id: 2,
                        update: FetchUpdate::Progress(_),
                    }) if !cancelling => {
                        commands.send(Command::CancelFetch(2))?;
                        cancelling = true;
                    }
                    Ok(Event::Fetch {
                        id: 2,
                        update: FetchUpdate::Cancelled,
                    }) => cancelled = true,
                    Ok(Event::Fetch {
                        update: FetchUpdate::Failed(error),
                        ..
                    }) => anyhow::bail!(error),
                    Ok(Event::Fetch {
                        update: FetchUpdate::Saved(path),
                        ..
                    }) => anyhow::bail!("Unexpected download: {}", path.display()),
                    _ => tokio::time::sleep(Duration::from_millis(10)).await,
                }
            }
            Ok::<_, anyhow::Error>(())
        })
        .await;
        commands.send(Command::Shutdown)?;
        tokio::time::timeout(Duration::from_secs(5), job).await??;
        outcome??;
        assert_eq!(std::fs::read_dir(&folder)?.count(), 0);
        Ok(())
    }

    async fn wait_event(
        output: &mpsc::Receiver<Event>,
        mut accept: impl FnMut(&Event) -> bool,
    ) -> anyhow::Result<Event> {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                match output.try_recv() {
                    Ok(event) if accept(&event) => return Ok(event),
                    Ok(Event::Error(error)) => anyhow::bail!(error),
                    Err(mpsc::TryRecvError::Disconnected) => {
                        anyhow::bail!("Services closed before the expected event")
                    }
                    _ => tokio::time::sleep(Duration::from_millis(5)).await,
                }
            }
        })
        .await?
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn delayed_startup_keeps_search_fetch_and_resolution_cancellation_responsive()
    -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        let config =
            TransferConfig::local(root.path().join("downloads"), root.path().join("state"));
        let (commands, receiver) = channel::unbounded_channel();
        let (events, output) = mpsc::channel();
        let (entered, starting) = oneshot::channel();
        let (release, gate) = oneshot::channel();
        let shutdown = CancellationToken::new();
        let startup_config = config.clone();
        let startup_cancel = shutdown.child_token();
        // A real session holds the lock while readiness is deliberately held.
        // Any accidental second start would fail visibly against this lock.
        let startup = tokio::spawn(async move {
            let session = TransferSession::start_cancellable(startup_config, startup_cancel)
                .await
                .map_err(|e| StartupFailure::from(e.to_string()))?;
            let _ = entered.send(());
            let _ = gate.await;
            Ok(Some(Arc::new(session)))
        });
        let streams = Arc::new(StreamSource::default());
        let job = tokio::spawn(run_loop(
            config.clone(),
            vec![],
            receiver,
            events,
            streams.clone(),
            shutdown.clone(),
            startup,
        ));
        starting.await?;
        for _ in 0..100 {
            commands.send(Command::Load)?;
        }
        commands.send(Command::Resolve(TorrentInput::Bytes(
            b"cancel before ready".to_vec(),
        )))?;
        commands.send(Command::CancelResolve)?;
        // Fails validation before any search provider/network request.
        commands.send(Command::Search {
            generation: 81,
            query: "x".into(),
        })?;
        commands.send(Command::Fetch {
            id: 91,
            source: FetchSource::Page("ftp://invalid.example/clip".into()),
            folder: root.path().join("links"),
        })?;
        let (mut searched, mut fetched, mut cancelled) = (false, false, false);
        tokio::time::timeout(Duration::from_secs(2), async {
            while !(searched && fetched && cancelled) {
                match output.try_recv() {
                    Ok(Event::Search {
                        generation: 81,
                        result: Err(_),
                        done: true,
                    }) => searched = true,
                    Ok(Event::Fetch {
                        id: 91,
                        update: FetchUpdate::Failed(_),
                    }) => fetched = true,
                    Ok(Event::ResolveCancelled) => cancelled = true,
                    Ok(Event::Error(error)) => anyhow::bail!(error),
                    Ok(Event::Progress(_) | Event::Resolved(_)) => {
                        anyhow::bail!("A waiting/cancelled request published too early")
                    }
                    _ => tokio::time::sleep(Duration::from_millis(5)).await,
                }
            }
            Ok::<_, anyhow::Error>(())
        })
        .await??;
        assert!(streams.session.lock().unwrap().is_none());
        release.send(()).unwrap();
        wait_event(&output, |e| matches!(e, Event::Progress(_))).await?;
        assert!(streams.session.lock().unwrap().is_some());
        // Repeated Load requests publish no repeated empty progress events.
        for _ in 0..20 {
            commands.send(Command::Load)?;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            !output
                .try_iter()
                .any(|e| matches!(e, Event::Progress(_) | Event::Error(_)))
        );
        commands.send(Command::Shutdown)?;
        tokio::time::timeout(Duration::from_secs(5), job).await??;
        use fs2::FileExt;
        let lock = std::fs::File::open(config.state_dir.join("session.lock"))?;
        lock.try_lock_exclusive()?;
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn startup_queue_is_bounded_and_shutdown_replies_to_every_deletion_waiter()
    -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        let config =
            TransferConfig::local(root.path().join("downloads"), root.path().join("state"));
        let (commands, receiver) = channel::unbounded_channel();
        let (events, _output) = mpsc::channel();
        let shutdown = CancellationToken::new();
        let cancel = shutdown.clone();
        let startup = tokio::spawn(async move {
            cancel.cancelled().await;
            Err("cancelled startup".into())
        });
        let job = tokio::spawn(run_loop(
            config,
            vec![],
            receiver,
            events,
            Arc::default(),
            shutdown,
            startup,
        ));
        let mut replies = Vec::new();
        for i in 0..MAX_PENDING_TRANSFERS + 8 {
            let (reply, response) = oneshot::channel();
            replies.push(response);
            commands.send(Command::ReleaseForDeletion {
                id: format!("{i:040x}"),
                reply: DeletionReply(Some(reply)),
            })?;
        }
        // The first overflow reply proves the preceding 32 were admitted.
        let error =
            tokio::time::timeout(Duration::from_secs(2), &mut replies[MAX_PENDING_TRANSFERS])
                .await??
                .unwrap_err();
        assert!(error.contains("Too many pending"), "{error}");
        commands.send(Command::Shutdown)?;
        tokio::time::timeout(Duration::from_secs(3), job).await??;
        for (index, reply) in replies.into_iter().enumerate() {
            if index == MAX_PENDING_TRANSFERS {
                continue;
            } // already received
            assert!(reply.await?.is_err());
        }
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn invalid_saved_ownership_stays_unknown_and_startup_failure_replies()
    -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        let config =
            TransferConfig::local(root.path().join("downloads"), root.path().join("state"));
        std::fs::create_dir_all(&config.state_dir)?;
        std::fs::write(
            config.state_dir.join("session.json"),
            b"not valid saved metadata",
        )?;
        let mut services = Services::start_config(config, vec![]);
        let reply =
            services.release_for_deletion("0123456789abcdef0123456789abcdef01234567".into());
        assert!(
            tokio::time::timeout(Duration::from_secs(3), reply)
                .await??
                .is_err()
        );
        let mut failure = false;
        for event in services.drain() {
            match event {
                Event::TransferAvailability(TransferAvailability::Unavailable {
                    message,
                    roots,
                    roots_complete,
                }) => {
                    assert!(
                        message.contains("Parse saved transfer metadata"),
                        "{message}"
                    );
                    assert!(roots.is_empty());
                    assert!(!roots_complete);
                    failure = true;
                }
                Event::TransferRoots(_) | Event::Progress(_) => {
                    anyhow::bail!("Unknown ownership was published as empty")
                }
                _ => {}
            }
        }
        assert!(
            failure,
            "No terminal availability state after corrupt startup"
        );
        services.begin_shutdown();
        assert!(
            services
                .release_for_deletion("anything".into())
                .await?
                .is_err()
        );
        tokio::task::spawn_blocking(finish_shutdown).await?;
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn held_lock_without_saved_records_never_publishes_empty_ownership() -> anyhow::Result<()>
    {
        use fs2::FileExt;
        let root = tempfile::tempdir()?;
        let config =
            TransferConfig::local(root.path().join("downloads"), root.path().join("state"));
        std::fs::create_dir(&config.state_dir)?;
        let lock = std::fs::File::create(config.state_dir.join("session.lock"))?;
        lock.try_lock_exclusive()?;
        let (commands, receiver) = channel::unbounded_channel();
        let (events, observed) = mpsc::channel();
        let job = tokio::spawn(run(config, vec![], receiver, events, Arc::default()));
        let outcome = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match observed.try_recv() {
                    Ok(Event::TransferAvailability(TransferAvailability::Unavailable {
                        roots_complete,
                        ..
                    })) => {
                        assert!(!roots_complete);
                        break;
                    }
                    Ok(Event::TransferRoots(_) | Event::Progress(_)) => {
                        anyhow::bail!("Held lock falsely proved empty ownership")
                    }
                    Err(mpsc::TryRecvError::Disconnected) => anyhow::bail!("Services closed"),
                    _ => tokio::time::sleep(Duration::from_millis(5)).await,
                }
            }
            Ok::<_, anyhow::Error>(())
        })
        .await;
        commands.send(Command::Shutdown)?;
        tokio::time::timeout(Duration::from_secs(5), job).await??;
        outcome??;
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn held_lock_reports_complete_roots_and_explicit_retry_restores() -> anyhow::Result<()> {
        use fs2::FileExt;
        let root = tempfile::tempdir()?;
        let config =
            TransferConfig::local(root.path().join("downloads"), root.path().join("state"));
        let output = root.path().join("owned");
        std::fs::create_dir(&output)?;
        std::fs::write(output.join("video.mp4"), vec![41; 65_537])?;
        let seed = TransferSession::start(config.clone()).await?;
        seed.share(&output, vec![]).await?;
        seed.stop().await;
        let lock = std::fs::File::open(config.state_dir.join("session.lock"))?;
        lock.try_lock_exclusive()?;
        let (commands, receiver) = channel::unbounded_channel();
        let (events, observed) = mpsc::channel();
        let job = tokio::spawn(run(config, vec![], receiver, events, Arc::default()));
        let outcome = tokio::time::timeout(Duration::from_secs(5), async {
            let mut seen_roots = false;
            loop {
                match observed.try_recv() {
                    Ok(Event::TransferRoots(roots)) => {
                        assert_eq!(roots, vec![output.canonicalize()?]);
                        seen_roots = true;
                    }
                    Ok(Event::TransferAvailability(TransferAvailability::Unavailable {
                        message,
                        roots,
                        roots_complete,
                    })) => {
                        assert!(seen_roots);
                        assert!(roots_complete);
                        assert_eq!(roots, vec![output.canonicalize()?]);
                        assert!(
                            message.contains("Another Flick transfer session"),
                            "{message}"
                        );
                        assert!(
                            output
                                .join("video.mp4")
                                .canonicalize()?
                                .starts_with(&roots[0])
                        );
                        let outside = root.path().join("local.mp4");
                        std::fs::write(&outside, b"independent local fixture")?;
                        assert!(!outside.canonicalize()?.starts_with(&roots[0]));
                        break;
                    }
                    Ok(Event::Progress(_)) => {
                        anyhow::bail!("Held lock published false live ownership")
                    }
                    Err(mpsc::TryRecvError::Disconnected) => anyhow::bail!("Services closed"),
                    _ => tokio::time::sleep(Duration::from_millis(5)).await,
                }
            }
            Ok::<_, anyhow::Error>(())
        })
        .await;
        // Discard the legacy text banner before the retry's expected events.
        observed.try_iter().for_each(drop);
        drop(lock);
        commands.send(Command::RetryTransfers)?;
        let retried = async {
            let mut checking = false;
            loop {
                match wait_event(&observed, |event| {
                    matches!(event, Event::TransferAvailability(_) | Event::Progress(_))
                })
                .await?
                {
                    Event::TransferAvailability(TransferAvailability::Checking) => checking = true,
                    Event::Progress(progress) => {
                        assert!(checking);
                        assert_eq!(progress.len(), 1);
                        break;
                    }
                    Event::TransferAvailability(TransferAvailability::Unavailable {
                        message,
                        ..
                    }) => anyhow::bail!(message),
                    _ => {}
                }
            }
            Ok::<_, anyhow::Error>(())
        }
        .await;
        commands.send(Command::Shutdown)?;
        tokio::time::timeout(Duration::from_secs(5), job).await??;
        outcome??;
        retried?;
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn deleted_output_retains_boundary_before_recreating_unverified_files()
    -> anyhow::Result<()> {
        let (_fixture, root) = path_alias_fixture()?;
        let parent = root.path().join("owned");
        let output = parent.join("removed-output");
        std::fs::create_dir_all(&output)?;
        std::fs::write(output.join("video.mp4"), vec![41; 65_537])?;
        let config =
            TransferConfig::local(root.path().join("downloads"), root.path().join("state"));
        let seed = TransferSession::start(config.clone()).await?;
        seed.share(&output, vec![]).await?;
        seed.stop().await;
        let canonical_output = output.canonicalize()?;
        std::fs::remove_dir_all(&output)?;
        let local = root.path().join("local.mp4");
        std::fs::write(&local, b"independent local fixture")?;
        let (commands, receiver) = channel::unbounded_channel();
        let (events, observed) = mpsc::channel();
        let job = tokio::spawn(run(config, vec![], receiver, events, Arc::default()));
        let outcome = tokio::time::timeout(Duration::from_secs(10), async {
            let mut boundary = false;
            loop {
                match observed.try_recv() {
                    Ok(Event::TransferRoots(roots)) => {
                        assert_eq!(roots, vec![parent.canonicalize()?]);
                        assert!(!local.canonicalize()?.starts_with(&roots[0]));
                        assert!(canonical_output.starts_with(&roots[0]));
                        boundary = true;
                    }
                    Ok(Event::Progress(progress)) => {
                        assert!(boundary, "Restoration preceded missing-root protection");
                        assert_eq!(progress.len(), 1);
                        assert!(
                            progress[0]
                                .files
                                .iter()
                                .all(|file| file.verified_bytes == 0)
                        );
                        break;
                    }
                    Ok(Event::TransferAvailability(TransferAvailability::Unavailable {
                        message,
                        roots,
                        roots_complete,
                    })) => {
                        // An engine that refuses absent output must preserve
                        // the same usable, complete exclusion boundary.
                        assert!(boundary && roots_complete, "{message}");
                        assert_eq!(roots, vec![parent.canonicalize()?]);
                        break;
                    }
                    Ok(Event::TorrentCompleted { .. }) => {
                        anyhow::bail!("Deleted payload was marked verified")
                    }
                    Err(mpsc::TryRecvError::Disconnected) => anyhow::bail!("Services closed"),
                    _ => tokio::time::sleep(Duration::from_millis(5)).await,
                }
            }
            Ok::<_, anyhow::Error>(())
        })
        .await;
        commands.send(Command::Shutdown)?;
        tokio::time::timeout(Duration::from_secs(5), job).await??;
        outcome??;
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn saved_roots_arrive_before_progress_and_release_retains_every_file()
    -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        let config =
            TransferConfig::local(root.path().join("downloads"), root.path().join("state"));
        let folder = root.path().join("shared");
        std::fs::create_dir(&folder)?;
        std::fs::write(folder.join("video.mp4"), vec![41; 65_537])?;
        std::fs::write(folder.join("sibling.mp4"), vec![42; 65_537])?;
        std::fs::write(folder.join("notes.txt"), b"preserve siblings")?;
        let seed = TransferSession::start(config.clone()).await?;
        let shared = seed.share(&folder, vec![]).await?;
        seed.stop().await;
        let mut services = Services::start_config(config.clone(), vec![]);
        let mut roots_seen = false;
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                for event in services.drain() {
                    match event {
                        Event::TransferRoots(roots) => {
                            assert_eq!(roots, vec![folder.canonicalize()?]);
                            roots_seen = true;
                        }
                        Event::Progress(progress) => {
                            assert!(roots_seen, "Snapshots preceded early ownership");
                            assert_eq!(progress.len(), 1);
                            if progress[0]
                                .files
                                .iter()
                                .filter(|f| f.selected)
                                .all(|f| f.verified_bytes == f.length)
                                && !matches!(progress[0].state, TransferState::Checking)
                            {
                                return Ok::<_, anyhow::Error>(());
                            }
                        }
                        Event::Error(error) => anyhow::bail!(error),
                        _ => {}
                    }
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await??;
        services
            .release_for_deletion(shared.infohash.clone())
            .await?
            .map_err(anyhow::Error::msg)?;
        // Library identity can outlive an earlier transfer removal. The same
        // ready session must acknowledge repeated release without touching data.
        services
            .release_for_deletion(shared.infohash)
            .await?
            .map_err(anyhow::Error::msg)?;
        let session = services.streams.session.lock().unwrap().clone().unwrap();
        assert!(
            session.snapshots()?.is_empty(),
            "Acknowledgment preceded release"
        );
        assert_eq!(std::fs::read(folder.join("video.mp4"))?, vec![41; 65_537]);
        assert_eq!(std::fs::read(folder.join("sibling.mp4"))?, vec![42; 65_537]);
        assert_eq!(
            std::fs::read(folder.join("notes.txt"))?,
            b"preserve siblings"
        );
        services.begin_shutdown();
        tokio::task::spawn_blocking(finish_shutdown).await?;
        // An external player/reader Arc must not retain the stopped state's lock.
        use fs2::FileExt;
        let lock = std::fs::File::open(config.state_dir.join("session.lock"))?;
        lock.try_lock_exclusive()?;
        assert!(session.snapshots().is_err());
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn known_empty_session_releases_absent_valid_owner_but_rejects_invalid_id()
    -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        let config =
            TransferConfig::local(root.path().join("downloads"), root.path().join("state"));
        let mut services = Services::start_config(config.clone(), vec![]);
        services
            .release_for_deletion("0123456789abcdef0123456789abcdef01234567".into())
            .await?
            .map_err(anyhow::Error::msg)?;
        assert!(
            services
                .release_for_deletion("invalid".into())
                .await?
                .is_err()
        );
        assert!(
            services.streams.session.lock().unwrap().is_none(),
            "Absent-owner release started an engine"
        );
        assert!(
            !config.download_dir.exists(),
            "Absent-owner release created downloads"
        );
        assert!(
            !config.state_dir.exists(),
            "Absent-owner release created transfer state"
        );
        services.begin_shutdown();
        assert!(
            services
                .release_for_deletion("0123456789abcdef0123456789abcdef01234567".into())
                .await?
                .is_err()
        );
        tokio::task::spawn_blocking(finish_shutdown).await?;
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn accepted_download_precedes_completion_uses_actual_paths_and_rearms_explicit_repeats()
    -> anyhow::Result<()> {
        let (_fixture, root) = path_alias_fixture()?;
        let config = |name: &str| {
            TransferConfig::local(
                root.path().join(format!("{name}-downloads")),
                root.path().join(format!("{name}-state")),
            )
        };
        let source = root.path().join("fixture");
        std::fs::create_dir(&source)?;
        std::fs::write(source.join("chosen.mp4"), vec![41; 65_537])?;
        std::fs::write(source.join("sibling.mp4"), vec![42; 65_537])?;
        std::fs::write(source.join("notes.txt"), b"not a video")?;
        let seed = TransferSession::start(config("seed")).await?;
        let shared = seed.share(&source, vec![]).await?;
        let metadata = TransferSession::start(config("metadata")).await?;
        let resolved = metadata
            .resolve(
                TorrentInput::Bytes(shared.torrent_bytes().to_vec()),
                vec![],
                Duration::from_secs(3),
            )
            .await?;
        metadata.stop().await;
        let selection = resolved
            .files
            .iter()
            .map(|file| file.index)
            .collect::<Vec<_>>();
        let receiver_config = config("receiver");
        let original_folder = receiver_config.download_dir.clone();
        let mut services =
            Services::start_config(receiver_config, vec![seed.listen_addr().unwrap()]);
        let outcome = tokio::time::timeout(Duration::from_secs(30), async {
            for round in 0..2 {
                services
                    .send(Command::Download {
                        torrent: resolved.clone(),
                        files: selection.clone(),
                        // Existing admission must report original engine paths.
                        folder: if round == 0 {
                            original_folder.clone()
                        } else {
                            root.path().join("ignored-new-folder")
                        },
                    })
                    .map_err(anyhow::Error::msg)?;
                let mut accepted = None;
                let mut completed = HashSet::new();
                let mut snapshot_after_acceptance = false;
                while completed.len() < 2 || !snapshot_after_acceptance {
                    for event in services.drain() {
                        match event {
                            Event::DownloadAccepted { infohash, selected } => {
                                assert!(accepted.is_none(), "Duplicate acceptance in one round");
                                assert_eq!(infohash, resolved.infohash);
                                assert_eq!(selected.len(), 2);
                                let original_destination =
                                    original_folder.canonicalize()?.join(&infohash);
                                assert!(selected.iter().all(|(path, _)| {
                                    path.starts_with(&original_destination)
                                        && flick::library::is_video(path)
                                }));
                                accepted = Some(selected.into_iter().collect::<HashSet<_>>());
                            }
                            Event::TorrentCompleted {
                                infohash,
                                file_index,
                                path,
                            } => {
                                assert_eq!(infohash, resolved.infohash);
                                assert!(
                                    accepted.as_ref().is_some_and(
                                        |files| files.contains(&(path.clone(), file_index))
                                    ),
                                    "Completion preceded accepted identity/path event"
                                );
                                completed.insert((path, file_index));
                            }
                            Event::Progress(snapshots) if accepted.is_some() => {
                                snapshot_after_acceptance = snapshots
                                    .iter()
                                    .any(|snapshot| snapshot.infohash == resolved.infohash);
                            }
                            Event::Error(error) => anyhow::bail!(error),
                            _ => {}
                        }
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            }
            // A rejected explicit command must not lift any tombstone.
            services
                .send(Command::Download {
                    torrent: resolved.clone(),
                    files: vec![usize::MAX],
                    folder: original_folder.clone(),
                })
                .map_err(anyhow::Error::msg)?;
            loop {
                for event in services.drain() {
                    match event {
                        Event::DownloadAccepted { .. } => {
                            anyhow::bail!("Rejected download was accepted")
                        }
                        Event::TorrentCompleted { .. } => {
                            anyhow::bail!("Automatic completion was re-armed without acceptance")
                        }
                        Event::Error(_) => return Ok::<_, anyhow::Error>(()),
                        _ => {}
                    }
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await;
        services.begin_shutdown();
        tokio::task::spawn_blocking(finish_shutdown).await?;
        seed.stop().await;
        outcome??;
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn direct_drop_finishes_cleanup_with_a_retained_session() -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        let config =
            TransferConfig::local(root.path().join("downloads"), root.path().join("state"));
        let services = Services::start_config(config.clone(), vec![]);
        let fixture = root.path().join("shutdown-fixture.mp4");
        std::fs::write(&fixture, vec![41; 65_537])?;
        services
            .send(Command::Share(fixture))
            .map_err(anyhow::Error::msg)?;
        let session = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if let Some(session) = services.streams.session.lock().unwrap().clone() {
                    break session;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await?;
        tokio::time::timeout(
            Duration::from_secs(8),
            tokio::task::spawn_blocking(move || drop(services)),
        )
        .await??;
        use fs2::FileExt;
        let lock = std::fs::File::open(config.state_dir.join("session.lock"))?;
        lock.try_lock_exclusive()?;
        assert!(session.snapshots().is_err());
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn begin_shutdown_is_nonblocking_and_finalization_cleans_http_and_session_lock()
    -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        let config =
            TransferConfig::local(root.path().join("downloads"), root.path().join("state"));
        let mut services = Services::start_config(config.clone(), vec![]);
        let fixture = root.path().join("shutdown-fixture.mp4");
        std::fs::write(&fixture, vec![41; 65_537])?;
        services
            .send(Command::Share(fixture))
            .map_err(anyhow::Error::msg)?;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let folder = root.path().join("links");
        services
            .send(Command::Fetch {
                id: 1,
                source: FetchSource::Files {
                    folder: None,
                    files: vec![fetch::RemoteFile {
                        url: format!("https://{}/clip.mp4", listener.local_addr()?),
                        name: "clip.mp4".into(),
                        size: None,
                        sha1: None,
                    }],
                },
                folder: folder.clone(),
            })
            .map_err(anyhow::Error::msg)?;
        let (_socket, _) =
            tokio::time::timeout(Duration::from_secs(3), listener.accept()).await??;
        tokio::time::timeout(Duration::from_secs(3), async {
            while services.streams.session.lock().unwrap().is_none() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await?;
        let started = std::time::Instant::now();
        services.begin_shutdown();
        assert!(started.elapsed() < Duration::from_millis(100));
        assert!(services.worker.is_none());
        tokio::time::timeout(
            Duration::from_secs(8),
            tokio::task::spawn_blocking(finish_shutdown),
        )
        .await??;
        assert_eq!(
            std::fs::read_dir(&folder)?.count(),
            0,
            "HTTP partial cleanup was abandoned"
        );
        use fs2::FileExt;
        let lock = std::fs::File::open(config.state_dir.join("session.lock"))?;
        lock.try_lock_exclusive()?;
        assert!(!services.drain().any(|e| matches!(e, Event::Error(_))));
        Ok(())
    }
}
