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
//!
//! The backend's capabilities are live ([`TransferBackend::live`]): `[]`
//! while the switch is off, so the listing stops advertising at once;
//! `shares` only while an allowed share folder exists; `links` only while
//! yt-dlp is found. They are worked out again whenever the settings change.
//! Both backends answer through the same helpers in
//! `flick::remote::transfers`, so they refuse and list alike.
//!
//! Each start makes one [`PairingCodes`] and hands the server a clone, so
//! "Show Pairing Code" opens the code `POST /v1/pair` accepts (and
//! `flick-serve pair-code` with the same state folder works too). Stopping
//! closes it at once, before the server thread has finished.
//!
//! On macOS, while the server runs and "Keep this Mac awake while sharing"
//! is on, the server thread holds `caffeinate -i -w <this pid>`: an
//! idle-sleep assertion that ends with the server, and with this process
//! even if it crashes. The switch applies at once through a watch channel;
//! until the settings page sets it, each start reads the saved choice.

use crate::services::{
    FetchListing, FetchState, QueryError, QueryHandle, QueryReply, QueryRequest, TransferListing,
};
use flick::remote::{
    auth, bind,
    ids::{self, Salt},
    pairing::CODE_LIFETIME,
    protocol::{
        self, Capability, FetchPhase, FetchRow, Pairing, ShareResult, Started, TransferFileRow,
        TransferRow,
    },
    server::{PairingCodes, Server, ServerConfig, Sources},
    transfers::{
        self, ShareRoots, TransferBackend, TransferCommand, TransferError, TransferReply,
        TransferRequest,
    },
};
use std::{
    fmt,
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};

/// Free space where downloads go; the settings page shows it too.
pub use flick::remote::transfers::storage;

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
    /// The running backend's capabilities, published here.
    capabilities: Option<Arc<tokio::sync::watch::Sender<Vec<Capability>>>>,
    /// Wakes the server thread to work the capabilities out again.
    changed: Arc<tokio::sync::Notify>,
    /// The name the server of this generation answers `hello` with.
    name: Option<(u64, String)>,
    /// The `caffeinate` holding the Mac awake for the server of this
    /// generation.
    caffeinate: Option<(u64, u32)>,
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
    /// The running server's codes; a clone is in its `ServerConfig`.
    pairing: Option<PairingCodes>,
    /// The code "Show Pairing Code" opened, and when.
    shown_code: Option<(String, Instant)>,
    /// When the code was last copied, for the button's label.
    code_copied: Option<Instant>,
    /// "Keep this Mac awake while sharing"; None until the settings page
    /// sets it, so each start reads the saved choice.
    keep_awake: Arc<tokio::sync::watch::Sender<Option<bool>>>,
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
            pairing: None,
            shown_code: None,
            code_copied: None,
            keep_awake: Arc::new(tokio::sync::watch::channel(None).0),
        }
    }
}

impl RemoteServe {
    pub fn status(&self) -> &ServeStatus {
        &self.status
    }

    /// Opens a new six-digit pairing code on the running server, replacing
    /// any open one. Err while the server is not running.
    pub fn open_pairing_code(&mut self) -> Result<String, String> {
        let codes = match (&self.status, &self.pairing) {
            (ServeStatus::Running { .. }, Some(codes)) => codes,
            _ => return Err("Turn on Share my library on Tailscale first".into()),
        };
        let code = codes.open();
        self.shown_code = Some((code.clone(), Instant::now()));
        self.code_copied = None;
        Ok(code)
    }

    /// The code "Show Pairing Code" opened and how long it stays valid;
    /// None once it was used, expired, five wrong tries closed it, the
    /// server stopped, or `flick-serve pair-code` replaced it.
    pub fn pairing_code(&self) -> Option<(String, Duration)> {
        if !matches!(self.status, ServeStatus::Running { .. }) {
            return None;
        }
        let (code, opened) = self.shown_code.as_ref()?;
        let left = self.pairing.as_ref()?.remaining()?;
        // A code opened through the server's local route has more time
        // left than ours could, and is not the one we know.
        let ours = CODE_LIFETIME.saturating_sub(opened.elapsed()) + Duration::from_secs(2);
        (left <= ours).then(|| (code.clone(), left))
    }

    /// Notes that the shown code was copied, for the Copy button's label.
    pub fn mark_code_copied(&mut self) {
        self.code_copied = Some(Instant::now());
    }

    /// Whether the shown code was copied in the last two seconds.
    pub fn code_copied(&self) -> bool {
        self.code_copied
            .is_some_and(|at| at.elapsed() < Duration::from_secs(2))
    }

    /// The name other Flicks find the running server by.
    pub fn server_name(&self) -> Option<String> {
        if !matches!(self.status, ServeStatus::Running { .. }) {
            return None;
        }
        let gate = self.gate.lock().unwrap_or_else(|e| e.into_inner());
        gate.name
            .as_ref()
            .filter(|(generation, _)| *generation == self.generation)
            .map(|(_, name)| name.clone())
    }

    /// "Keep this Mac awake while sharing": applies at once to a running
    /// server and to every later start. Does nothing outside macOS.
    pub fn set_keep_awake(&mut self, on: bool) {
        self.keep_awake.send_replace(Some(on));
    }

    /// The `caffeinate` process holding this Mac awake, if any.
    pub fn keeping_awake(&self) -> Option<u32> {
        self.gate
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .caffeinate
            .filter(|(generation, _)| *generation == self.generation)
            .map(|(_, pid)| pid)
    }

    /// Applies the owner's download settings from the next request on.
    /// Turning downloads on restarts a server that started without them,
    /// with the same token, so paired Flicks see the new capabilities.
    pub fn set_transfer_settings(&mut self, settings: TransferSettings) {
        let restart = {
            let mut gate = self.gate.lock().unwrap_or_else(|e| e.into_inner());
            let restart = settings.allow && gate.attached == Some((self.generation, false));
            if !settings.allow
                && let Some(capabilities) = &gate.capabilities
            {
                // Off takes effect now; the listing stops advertising.
                capabilities.send_replace(Vec::new());
            }
            gate.settings = Some(settings);
            gate.changed.notify_one();
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
        let codes = PairingCodes::new();
        self.pairing = Some(codes.clone());
        self.shown_code = None;
        let extras = Extras {
            pairing: codes,
            keep_awake: self.keep_awake.subscribe(),
            gate: self.gate.clone(),
        };
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
                runtime.block_on(serve(
                    options, transfers, extras, generation, events, stopped,
                ));
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
        self.close_pairing();
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
                if matches!(status, ServeStatus::Failed(_)) {
                    self.close_pairing();
                    if let Some(running) = self.running.take() {
                        // The thread is ending on its own.
                        self.stopping = Some(running.thread);
                    }
                }
                self.status = status;
                changed = true;
            }
        }
        changed
    }

    /// Closes the code at once; the server thread may take a moment more.
    fn close_pairing(&mut self) {
        if let Some(codes) = self.pairing.take() {
            codes.close();
        }
        self.shown_code = None;
        self.code_copied = None;
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

/// What every start passes the server thread besides its options.
struct Extras {
    pairing: PairingCodes,
    keep_awake: tokio::sync::watch::Receiver<Option<bool>>,
    gate: Arc<Mutex<Gate>>,
}

async fn serve(
    options: ServeOptions,
    transfers: Option<TransferSetup>,
    extras: Extras,
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
    let (backend, requests, capabilities) = match &transfers {
        Some(setup) => attach_transfers(setup, &options.settings_path).await,
        None => (None, None, None),
    };
    let state_dir = options.state_dir.clone();
    let settings_path = options.settings_path.clone();
    extras.gate.lock().unwrap_or_else(|e| e.into_inner()).name = Some((generation, name.clone()));
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
        pairing: Some(extras.pairing),
        advertise_hardware: true,
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
                    let keeping = capabilities.map(|capabilities| {
                        tokio::spawn(keep_capabilities(setup.gate.clone(), capabilities))
                    });
                    let answerer = Answerer {
                        host: setup.host,
                        gate: setup.gate,
                        salt,
                        backend: backend.clone(),
                    };
                    Some((tokio::spawn(answer_transfers(requests, answerer)), keeping))
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
    keep_awake_until(
        extras.keep_awake,
        &extras.gate,
        generation,
        &settings_path,
        stopped,
    )
    .await;
    tracing::info!("Stopping the in-app remote library server");
    server.shutdown().await;
    if let Some((answering, keeping)) = answering {
        answering.abort();
        let _ = answering.await;
        if let Some(keeping) = keeping {
            keeping.abort();
            let _ = keeping.await;
        }
    }
}

/// Holds `caffeinate` while the owner wants it, until `stopped` resolves.
async fn keep_awake_until(
    mut wanted: tokio::sync::watch::Receiver<Option<bool>>,
    gate: &Mutex<Gate>,
    generation: u64,
    settings_path: &Path,
    mut stopped: tokio::sync::oneshot::Receiver<()>,
) {
    let saved = {
        let path = settings_path.to_owned();
        tokio::task::spawn_blocking(move || saved_keep_awake(&path))
            .await
            .unwrap_or(true)
    };
    let mut held: Option<tokio::process::Child> = None;
    loop {
        let on = wanted.borrow_and_update().unwrap_or(saved);
        match (on, held.is_some()) {
            (true, false) => held = caffeinate(),
            (false, true) => release(&mut held).await,
            _ => {}
        }
        let pid = held.as_ref().and_then(tokio::process::Child::id);
        gate.lock().unwrap_or_else(|e| e.into_inner()).caffeinate =
            pid.map(|pid| (generation, pid));
        tokio::select! {
            _ = &mut stopped => break,
            changed = wanted.changed() => {
                if changed.is_err() {
                    // The view is gone; its stop follows.
                    let _ = (&mut stopped).await;
                    break;
                }
            }
        }
    }
    release(&mut held).await;
    let mut gate = gate.lock().unwrap_or_else(|e| e.into_inner());
    if gate
        .caffeinate
        .is_some_and(|(held_by, _)| held_by == generation)
    {
        gate.caffeinate = None;
    }
}

/// Starts `caffeinate -i -w <this pid>` on macOS; None elsewhere or when it
/// cannot start, which only costs the assertion.
fn caffeinate() -> Option<tokio::process::Child> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let spawned = tokio::process::Command::new("/usr/bin/caffeinate")
        .arg("-i")
        .arg("-w")
        .arg(std::process::id().to_string())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn();
    match spawned {
        Ok(child) => {
            tracing::info!("Keeping this Mac awake while sharing");
            Some(child)
        }
        Err(error) => {
            tracing::warn!(%error, "Could not keep this Mac awake while sharing");
            None
        }
    }
}

async fn release(held: &mut Option<tokio::process::Child>) {
    if let Some(mut child) = held.take() {
        // Kills and reaps it.
        let _ = child.kill().await;
        tracing::info!("This Mac may sleep again");
    }
}

/// "Keep this Mac awake while sharing" as last saved; on when unreadable.
fn saved_keep_awake(settings_path: &Path) -> bool {
    #[derive(Default, serde::Deserialize)]
    #[serde(default)]
    struct Saved {
        remote: crate::store::remote::RemoteSettings,
    }
    std::fs::read_to_string(settings_path)
        .ok()
        .and_then(|text| serde_json::from_str::<Saved>(&text).ok())
        .is_none_or(|saved| saved.remote.keep_awake)
}

type Published = Arc<tokio::sync::watch::Sender<Vec<Capability>>>;

/// Decides, once per start, whether this server answers transfer commands,
/// and makes its backend when it does, with live capabilities.
async fn attach_transfers(
    setup: &TransferSetup,
    settings_path: &Path,
) -> (
    Option<TransferBackend>,
    Option<tokio::sync::mpsc::Receiver<TransferRequest>>,
    Option<Published>,
) {
    let path = settings_path.to_owned();
    let saved = tokio::task::spawn_blocking(move || saved_transfer_settings(&path))
        .await
        .unwrap_or_default();
    let settings = {
        let mut gate = setup.gate.lock().unwrap_or_else(|e| e.into_inner());
        let settings = gate.settings.get_or_insert(saved).clone();
        gate.attached = Some((setup.generation, settings.allow));
        settings
    };
    if !settings.allow {
        return (None, None, None);
    }
    let initial = tokio::task::spawn_blocking(move || capabilities(&settings))
        .await
        .unwrap_or_default();
    let (publish, receiver) = tokio::sync::watch::channel(initial);
    let publish = Arc::new(publish);
    setup
        .gate
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .capabilities = Some(publish.clone());
    // Settings changed while the first answer was worked out.
    setup
        .gate
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .changed
        .notify_one();
    let (sender, requests) = tokio::sync::mpsc::channel(32);
    (
        Some(TransferBackend::live(receiver, sender)),
        Some(requests),
        Some(publish),
    )
}

/// What this Flick offers paired Flicks with these settings: nothing while
/// the switch is off; `shares` only with an allowed folder that exists;
/// `links` only when yt-dlp is found (found, never run).
fn capabilities(settings: &TransferSettings) -> Vec<Capability> {
    if !settings.allow {
        return Vec::new();
    }
    let mut capabilities = vec![Capability::Downloads, Capability::StreamIncomplete];
    if flick::fetch::ytdlp::Tools::discover().is_ok() {
        capabilities.push(Capability::Links);
    }
    if !ShareRoots::quiet(&settings.share_roots).is_empty() {
        capabilities.push(Capability::Shares);
    }
    capabilities
}

/// Works the capabilities out again whenever the settings page changes the
/// settings, until the server stops. A result is published only while the
/// settings it was worked out from are still current.
async fn keep_capabilities(gate: Arc<Mutex<Gate>>, publish: Published) {
    let changed = gate
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .changed
        .clone();
    loop {
        changed.notified().await;
        let settings = gate
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .settings
            .clone()
            .unwrap_or_default();
        let worked_out = settings.clone();
        let Ok(next) = tokio::task::spawn_blocking(move || capabilities(&worked_out)).await else {
            continue;
        };
        let gate = gate.lock().unwrap_or_else(|e| e.into_inner());
        if gate.settings.as_ref() == Some(&settings) {
            publish.send_if_modified(|current| {
                let modified = *current != next;
                *current = next;
                modified
            });
        }
    }
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
    /// The backend this answers for; its capabilities are live.
    backend: TransferBackend,
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

    fn require(&self, capability: Capability, refusal: &str) -> Result<(), TransferError> {
        if self.backend.allows(capability) {
            Ok(())
        } else {
            Err(TransferError::not_allowed(refusal))
        }
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
                let input = transfers::parse_input(&input)?;
                let key = transfers::input_key(&input);
                match self.ask(QueryRequest::Resolve { input, key }).await? {
                    QueryReply::Resolved(torrent) => {
                        TransferReply::Resolved(transfers::torrent_info(&torrent))
                    }
                    _ => return Err(unexpected()),
                }
            }
            TransferCommand::Start { input, files } => {
                let input = transfers::parse_input(&input)?;
                let key = transfers::input_key(&input);
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
                    folder,
                };
                self.done(request).await?
            }
            TransferCommand::FetchStart { url } => {
                self.require(Capability::Links, "link downloads")?;
                transfers::fetch_source(&url)?;
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
                self.require(Capability::Shares, "shares")?;
                let roots = settings.share_roots;
                TransferReply::Browse(
                    blocking(move || ShareRoots::quiet(&roots).browse(&dir)).await?,
                )
            }
            TransferCommand::Share { dir } => {
                self.require(Capability::Shares, "shares")?;
                let roots = settings.share_roots;
                let path = blocking(move || ShareRoots::quiet(&roots).resolve(&dir)).await?;
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

/// The worker's refusal as an HTTP status, as the headless backend answers
/// the same refusal. Only a deletion whose list did not match carries the
/// current list as `files`; messages never name a path on this machine.
pub fn transfer_error(error: QueryError) -> TransferError {
    let public = |message: String| {
        transfers::public_text(&message).unwrap_or_else(|| {
            tracing::warn!("A paired Flick's request failed: {message}");
            "This Flick could not do that. Its owner can see why in the log.".into()
        })
    };
    match error {
        QueryError::NotFound(message) => TransferError::new(404, public(message)),
        QueryError::Forbidden(message) => TransferError::new(403, message),
        QueryError::Conflict {
            files: Some(files), ..
        } => TransferError::files_changed(files),
        QueryError::Conflict {
            message,
            files: None,
        } => TransferError::new(409, public(message)),
        QueryError::Invalid(message) => TransferError::new(422, public(message)),
        QueryError::Busy(message) | QueryError::Unavailable(message) => {
            TransferError::new(503, public(message))
        }
        QueryError::TimedOut(message) => TransferError::new(504, public(message)),
        QueryError::Failed(message) => TransferError::new(500, public(message)),
    }
}

/// A transfer id from a URL: an infohash, else 404.
fn infohash(id: &str) -> Result<String, TransferError> {
    if transfers::is_infohash(id) {
        Ok(id.to_owned())
    } else {
        Err(TransferError::unknown_transfer())
    }
}

/// A transfer as `GET /v1/transfers` lists it. Each selected video gets
/// the id the server's library gives the same file.
pub fn transfer_row(listing: &TransferListing, salt: &Salt) -> TransferRow {
    let snapshot = &listing.snapshot;
    TransferRow {
        id: snapshot.infohash.clone(),
        name: snapshot.name.clone(),
        phase: transfers::phase(snapshot.state),
        error: snapshot
            .error
            .as_deref()
            .map(transfers::public_transfer_error),
        verified_bytes: snapshot.verified_bytes,
        total_bytes: snapshot.total_bytes,
        checked_bytes: snapshot.checked_bytes,
        checking_total_bytes: snapshot.checking_total_bytes,
        uploaded_bytes: snapshot.uploaded_bytes,
        peers: snapshot.peers,
        download_bps: snapshot.download_bytes_per_second,
        upload_bps: snapshot.upload_bytes_per_second,
        share: listing.share,
        files: snapshot
            .files
            .iter()
            .map(|file| TransferFileRow {
                index: file.index,
                path: listing.paths.get(file.index).cloned().unwrap_or_default(),
                size: file.length,
                verified_bytes: file.verified_bytes,
                selected: file.selected,
                will_delete: transfers::will_delete(file, listing.share),
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
        // Already short and without paths (`services::fetch_job`); checked
        // again here, at the boundary.
        FetchState::Failed(error) => (
            FetchPhase::Failed,
            Some(
                transfers::public_text(error)
                    .unwrap_or_else(|| "The link could not be downloaded on the server.".into()),
            ),
        ),
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
    use flick::remote::protocol::TransferPhase;
    use flick::torrent::TransferState;
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

    fn post(listen: SocketAddr, path: &str, body: &str) -> String {
        let mut stream = std::net::TcpStream::connect(listen).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        write!(
            stream,
            "POST {path} HTTP/1.1\r\nHost: {listen}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        response
    }

    fn pair_with(listen: SocketAddr, code: &str) -> String {
        post(
            listen,
            "/v1/pair",
            &serde_json::json!({ "code": code }).to_string(),
        )
    }

    fn library_settings(dir: &std::path::Path, remote: serde_json::Value) {
        std::fs::write(
            dir.join("settings.json"),
            serde_json::json!({"library": [], "remote": remote}).to_string(),
        )
        .unwrap();
    }

    #[test]
    fn an_opened_code_pairs_through_v1_pair_once() {
        let dir = tempfile::tempdir().unwrap();
        library_settings(dir.path(), serde_json::json!({"keep_awake": false}));
        let mut serve = RemoteServe::default();
        assert!(serve.open_pairing_code().is_err(), "not running yet");
        assert_eq!(serve.pairing_code(), None);
        serve.start(options(dir.path(), false));
        let ServeStatus::Running { listen, pairing } =
            wait_for(&mut serve, |s| !matches!(s, ServeStatus::Starting))
        else {
            panic!("did not start: {:?}", serve.status());
        };
        assert_eq!(serve.server_name().as_deref(), Some("Test Library"));
        assert_eq!(serve.pairing_code(), None);
        let hello = get(listen, "/v1/hello", "");
        assert!(hello.contains(r#""pairing_open":false"#), "{hello}");
        assert!(pair_with(listen, "123456").starts_with("HTTP/1.1 404"));

        let code = serve.open_pairing_code().unwrap();
        assert!(
            code.len() == 6 && code.bytes().all(|b| b.is_ascii_digit()),
            "{code}"
        );
        let (shown, left) = serve.pairing_code().unwrap();
        assert_eq!(shown, code);
        assert!(
            left > Duration::from_secs(9 * 60) && left <= CODE_LIFETIME,
            "{left:?}"
        );
        let hello = get(listen, "/v1/hello", "");
        assert!(hello.contains(r#""pairing_open":true"#), "{hello}");

        let wrong = if code == "000000" { "000001" } else { "000000" };
        let refused = pair_with(listen, wrong);
        assert!(refused.starts_with("HTTP/1.1 403"), "{refused}");
        assert_eq!(
            serve.pairing_code().map(|(code, _)| code),
            Some(code.clone())
        );

        let paired = pair_with(listen, &code);
        assert!(paired.starts_with("HTTP/1.1 200"), "{paired}");
        assert!(
            paired.contains(&pairing),
            "the reply carries the pairing string"
        );
        assert_eq!(serve.pairing_code(), None, "a match closes the code");
        assert!(pair_with(listen, &code).starts_with("HTTP/1.1 404"));

        // A new code replaces the open one.
        let first = serve.open_pairing_code().unwrap();
        let second = serve.open_pairing_code().unwrap();
        assert_eq!(
            serve.pairing_code().map(|(code, _)| code),
            Some(second.clone())
        );
        if first != second {
            assert!(pair_with(listen, &first).starts_with("HTTP/1.1 403"));
        }
        assert!(pair_with(listen, &second).starts_with("HTTP/1.1 200"));

        // The saved choice was off, so nothing held the Mac awake.
        assert_eq!(serve.keeping_awake(), None);
        serve.stop();
    }

    #[test]
    fn stopping_the_server_closes_its_code() {
        let dir = tempfile::tempdir().unwrap();
        library_settings(dir.path(), serde_json::json!({"keep_awake": false}));
        let mut serve = RemoteServe::default();
        serve.start(options(dir.path(), false));
        let ServeStatus::Running { listen, .. } =
            wait_for(&mut serve, |s| !matches!(s, ServeStatus::Starting))
        else {
            panic!("did not start: {:?}", serve.status());
        };
        let code = serve.open_pairing_code().unwrap();
        let codes = serve.pairing.clone().unwrap();
        assert!(codes.is_open());

        serve.stop();
        assert!(!codes.is_open(), "closed at once, before the thread ends");
        assert_eq!(serve.pairing_code(), None);
        assert!(serve.open_pairing_code().is_err());
        serve.stopping.take().unwrap().join().unwrap();
        assert!(std::net::TcpStream::connect(listen).is_err());

        // The next start has its own code, closed until opened.
        serve.start(options(dir.path(), false));
        let ServeStatus::Running { listen, .. } =
            wait_for(&mut serve, |s| !matches!(s, ServeStatus::Starting))
        else {
            panic!("did not restart: {:?}", serve.status());
        };
        assert_eq!(serve.pairing_code(), None);
        assert!(pair_with(listen, &code).starts_with("HTTP/1.1 404"));
        serve.stop();
    }

    /// Whether a process with this id exists (and is not a zombie).
    fn alive(pid: u32) -> bool {
        std::process::Command::new("ps")
            .args(["-o", "stat=,command=", "-p", &pid.to_string()])
            .output()
            .ok()
            .filter(|output| output.status.success())
            .is_some_and(|output| !String::from_utf8_lossy(&output.stdout).starts_with('Z'))
    }

    fn wait_awake(serve: &RemoteServe, held: bool) -> Option<u32> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let pid = serve.keeping_awake();
            if pid.is_some() == held {
                return pid;
            }
            assert!(Instant::now() < deadline, "keeping awake: {pid:?}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn caffeinate_is_held_while_serving_on_macos_and_follows_the_switch() {
        let dir = tempfile::tempdir().unwrap();
        // No saved choice: on by default.
        library_settings(dir.path(), serde_json::json!({}));
        let mut serve = RemoteServe::default();
        serve.start(options(dir.path(), false));
        wait_for(&mut serve, |s| matches!(s, ServeStatus::Running { .. }));
        if !cfg!(target_os = "macos") {
            std::thread::sleep(Duration::from_millis(300));
            assert_eq!(serve.keeping_awake(), None);
            serve.stop();
            return;
        }
        let pid = wait_awake(&serve, true).unwrap();
        let command = std::process::Command::new("ps")
            .args(["-o", "command=", "-p", &pid.to_string()])
            .output()
            .unwrap();
        let command = String::from_utf8_lossy(&command.stdout);
        assert_eq!(
            command.trim(),
            format!("/usr/bin/caffeinate -i -w {}", std::process::id())
        );

        serve.set_keep_awake(false);
        wait_awake(&serve, false);
        assert!(!alive(pid), "switching off ends caffeinate");

        serve.set_keep_awake(true);
        let again = wait_awake(&serve, true).unwrap();
        assert!(alive(again));
        serve.stop();
        assert_eq!(serve.keeping_awake(), None);
        serve.stopping.take().unwrap().join().unwrap();
        assert!(!alive(again), "stopping the server ends caffeinate");
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
            share: false,
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
        // Deletion covers selected files, and unselected ones with data.
        assert!(!row.share);
        let deleting = row.files.iter().map(|f| f.will_delete).collect::<Vec<_>>();
        assert_eq!(deleting, [true, true, false, true]);
        std::fs::write(folder.join("Other.mp4"), b"boundary piece").unwrap();
        let row = transfer_row(&listing, &salt);
        assert!(row.files[2].will_delete && !row.files[2].selected);
        assert_eq!(
            crate::services::deletion_list(&listing),
            flick::remote::transfers::deletion_paths(&row)
        );
        // A share's files are never deleted through the wire.
        let shared = TransferListing {
            share: true,
            ..listing.clone()
        };
        let row = transfer_row(&shared, &salt);
        assert!(row.share);
        assert!(row.files.iter().all(|f| !f.will_delete));
        assert!(crate::services::deletion_list(&shared).is_empty());
        // Errors never carry the server's paths.
        let mut failed = listing.clone();
        failed.snapshot.error = Some(format!("Could not open {}", video.display()));
        let error = transfer_row(&failed, &salt).error.unwrap();
        assert!(!error.contains(&*video.to_string_lossy()), "{error}");
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
            assert_eq!(files.as_ref(), Some(&required));
            let error = transfer_error(error);
            assert_eq!(error.status, 409);
            assert!(
                error.message.contains("nothing was deleted"),
                "{}",
                error.message
            );
            assert_eq!(error.files.as_ref(), Some(&required));
        }
        assert_eq!(transfer_error(QueryError::NotFound("x".into())).status, 404);
        assert_eq!(transfer_error(QueryError::Invalid("x".into())).status, 422);
        assert_eq!(transfer_error(QueryError::Busy("x".into())).status, 503);
        assert_eq!(
            transfer_error(QueryError::Unavailable("x".into())).status,
            503
        );
        let forbidden = transfer_error(QueryError::Forbidden("Keep Files.".into()));
        assert_eq!((forbidden.status, forbidden.files), (403, None));
        // Other conflicts carry no list.
        let plain = transfer_error(QueryError::Conflict {
            message: "Still checking.".into(),
            files: None,
        });
        assert_eq!(
            (plain.status, plain.message.as_str(), plain.files),
            (409, "Still checking.", None)
        );
        // A path in a worker's message never reaches the client.
        let leaky = transfer_error(QueryError::Failed(
            "Could not start the download: Create /Users/owner/Downloads".into(),
        ));
        assert_eq!(leaky.status, 500);
        assert!(!leaky.message.contains("/Users"), "{}", leaky.message);
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

        let shares = ShareRoots::quiet(&roots);
        let browse = |dir: &str| shares.browse(dir);
        let resolve_virtual = |dir: &str| shares.resolve(dir).ok();
        let top = browse("").unwrap();
        assert_eq!(top.dir, "");
        let names = top
            .entries
            .iter()
            .map(|e| e.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(names, ["Movies", "Movies (2)"]);
        assert!(top.entries.iter().all(|e| e.dir));

        let listing = browse("Movies/").unwrap();
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
        assert_eq!(browse("Movies/Series").unwrap().entries[0].name, "Season 1");

        for refused in [
            "Movies/Escape",
            "Movies/../Private",
            "Movies/Shows/../../Private",
            "Movies/./Shows",
            "Private",
            "Movies/Missing",
            "gone",
        ] {
            assert_eq!(browse(refused).unwrap_err().status, 404, "{refused}");
        }
        assert_eq!(
            resolve_virtual("Movies/Film.mkv"),
            Some(root.join("Film.mkv").canonicalize().unwrap())
        );
        assert_eq!(resolve_virtual("Movies/Linked.mp4"), None);
        assert_eq!(resolve_virtual("Movies/Escape/Secret.mp4"), None);
        assert_eq!(resolve_virtual(""), None);
        assert_eq!(
            resolve_virtual("Movies (2)"),
            Some(other.canonicalize().unwrap())
        );
    }

    #[test]
    fn inputs_parse_with_a_key_that_matches_start_to_resolve() {
        let hash = "0123456789abcdef0123456789ABCDEF01234567";
        let lower = hash.to_ascii_lowercase();
        let parse = |text: &str| {
            transfers::parse_input(text).map(|input| {
                let key = transfers::input_key(&input);
                (input, key)
            })
        };
        let (input, key) = parse(hash).unwrap();
        assert!(
            matches!(input, flick::torrent::TorrentInput::Magnet(m) if m == format!("magnet:?xt=urn:btih:{lower}"))
        );
        assert_eq!(key, lower);
        let magnet = format!("magnet:?dn=Film&xt=urn:btih:{hash}&tr=x");
        let (input, key) = parse(&format!("  {magnet} ")).unwrap();
        assert!(matches!(input, flick::torrent::TorrentInput::Magnet(m) if m == magnet));
        assert_eq!(key, lower);
        let (input, key) = parse("https://example.com/a.torrent").unwrap();
        assert!(matches!(input, flick::torrent::TorrentInput::Url(_)));
        assert_eq!(key, "https://example.com/a.torrent");
        // Invalid input is 422, as the headless server answers.
        for refused in [
            "",
            "http://example.com/a.torrent",
            "/etc/passwd",
            "base64:!!",
            "base64:aGVsbG8=",
        ] {
            assert_eq!(
                parse(refused).err().map(|e| e.status),
                Some(422),
                "{refused}"
            );
        }
        assert_eq!(infohash("nope").unwrap_err().status, 404);
        assert_eq!(infohash(hash).unwrap_err().status, 404, "ids are lowercase");
        assert_eq!(infohash(&lower).unwrap(), lower);
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
        let listen = match serve.status() {
            ServeStatus::Running { listen, .. } => *listen,
            status => panic!("{status:?}"),
        };
        let token = Pairing::parse(&again).unwrap().token;
        let listing = || {
            let response = get(listen, "/v1/library", &token);
            assert!(response.starts_with("HTTP/1.1 200"), "{response}");
            let body = response.split_once("\r\n\r\n").unwrap().1;
            serde_json::from_str::<protocol::Library>(body).unwrap()
        };
        let library = listing();
        assert!(library.capabilities.contains(&Capability::Downloads));
        assert!(library.capabilities.contains(&Capability::StreamIncomplete));
        // No share folder: shares are not offered.
        assert!(!library.capabilities.contains(&Capability::Shares));
        assert!(library.storage.is_some());
        assert!(library.videos.is_empty());
        let runtime = transfer_test_runtime();
        let answer = runtime.block_on(backend.call(TransferCommand::Storage));
        let Ok(TransferReply::Storage(storage)) = answer else {
            panic!("storage was not answered");
        };
        assert_eq!(storage.download_folder_name, "Remote Downloads");
        // Off answers 403 at once, without a restart, and stops advertising.
        serve.set_transfer_settings(TransferSettings {
            allow: false,
            ..settings.clone()
        });
        assert!(backend.capabilities().is_empty());
        let refused = runtime.block_on(backend.call(TransferCommand::List));
        assert_eq!(refused.err().map(|e| e.status), Some(403));
        // The listing is not frozen while off: a new video appears.
        let video = dir.path().join("Clip.mp4");
        std::fs::write(&video, b"not really a video").unwrap();
        std::fs::write(
            dir.path().join("settings.json"),
            serde_json::json!({"library": [{"path": video, "title": "Clip"}]}).to_string(),
        )
        .unwrap();
        let library = listing();
        assert!(
            library.capabilities.is_empty(),
            "{:?}",
            library.capabilities
        );
        assert!(library.storage.is_none());
        assert_eq!(library.videos.len(), 1);
        assert_eq!(library.videos[0].title, "Clip");
        // On again, now with a share folder: capabilities come back with
        // shares, on the same server.
        let shared = dir.path().join("Shared");
        std::fs::create_dir(&shared).unwrap();
        serve.set_transfer_settings(TransferSettings {
            share_roots: vec![shared],
            ..settings.clone()
        });
        assert!(taps.try_recv().is_err(), "on again: same server");
        let deadline = Instant::now() + Duration::from_secs(10);
        let library = loop {
            let library = listing();
            if library.capabilities.contains(&Capability::Shares) {
                break library;
            }
            assert!(Instant::now() < deadline, "{:?}", library.capabilities);
            std::thread::sleep(Duration::from_millis(20));
        };
        assert!(library.capabilities.contains(&Capability::Downloads));
        assert!(library.storage.is_some());
        assert!(matches!(
            runtime.block_on(backend.call(TransferCommand::List)),
            Ok(TransferReply::Transfers(rows)) if rows.is_empty()
        ));
        // Removing the share folder takes shares away again.
        serve.set_transfer_settings(settings);
        let deadline = Instant::now() + Duration::from_secs(10);
        while backend.allows(Capability::Shares) {
            assert!(Instant::now() < deadline, "shares are still offered");
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(backend.allows(Capability::Downloads));
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
        // Film.mp4 does not end on a piece boundary, so its last piece
        // also writes the start of the unselected notes.txt.
        std::fs::create_dir(seed_source.join("Film")).unwrap();
        std::fs::write(seed_source.join("Film").join("Film.mp4"), &bytes).unwrap();
        std::fs::write(
            seed_source.join("Film").join("notes.txt"),
            vec![7u8; 300_000],
        )
        .unwrap();
        let seed = runtime
            .block_on(flick::torrent::TransferSession::start(
                flick::torrent::TransferConfig::local(
                    dir.path().join("seed-downloads"),
                    dir.path().join("seed-state"),
                ),
            ))
            .unwrap();
        let shared = runtime
            .block_on(seed.share(&seed_source.join("Film"), vec![]))
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
                assert_eq!(info.files.len(), 2);
                let film = info
                    .files
                    .iter()
                    .find(|file| file.path == "Film.mp4")
                    .expect("Film.mp4 is in the torrent");
                assert_eq!(film.size, bytes.len() as u64);
                let film = film.index;
                let notes = 1 - film;

                // Start reuses the resolution, by infohash.
                let Ok(TransferReply::Started(started)) = call(TransferCommand::Start {
                    input: shared.infohash.clone(),
                    files: vec![film],
                })
                .await
                else {
                    panic!("not started");
                };
                assert_eq!(started.id, shared.infohash);
                let again = call(TransferCommand::Start {
                    input: shared.magnet.clone(),
                    files: vec![film],
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
                let boundary = downloaded.with_file_name("notes.txt");
                assert_eq!(std::fs::read(&downloaded).unwrap(), bytes);
                assert!(!row.share);
                assert_eq!(row.files[film].path, "Film.mp4");
                assert!(row.files[film].selected && row.files[film].will_delete);
                assert_eq!(
                    row.files[film].video_id,
                    Some(ids::video_id(&salt, &downloaded.canonicalize().unwrap()))
                );
                // The boundary piece wrote into the unselected file, so a
                // deletion would delete it too, and says so.
                assert!(boundary.is_file(), "the boundary piece wrote notes.txt");
                assert_eq!(row.files[notes].path, "notes.txt");
                assert!(!row.files[notes].selected && row.files[notes].will_delete);

                let Ok(TransferReply::Stream(stream)) = call(TransferCommand::OpenStream {
                    id: shared.infohash.clone(),
                    file_index: film,
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

                // The selected file alone is the wrong list: it deletes
                // nothing and names the right one.
                let wrong = call(TransferCommand::Remove {
                    id: shared.infohash.clone(),
                    delete_files: true,
                    confirm: vec!["Film.mp4".into()],
                })
                .await
                .err()
                .expect("a wrong list is refused");
                assert_eq!(wrong.status, 409);
                assert_eq!(
                    wrong.files,
                    Some(vec!["Film.mp4".to_owned(), "notes.txt".to_owned()])
                );
                assert!(downloaded.is_file() && boundary.is_file());
                assert!(matches!(
                    call(TransferCommand::Remove {
                        id: shared.infohash.clone(),
                        delete_files: true,
                        confirm: flick::remote::transfers::deletion_paths(&row),
                    })
                    .await,
                    Ok(TransferReply::Done)
                ));
                assert!(!downloaded.exists() && !boundary.exists());
                let Ok(TransferReply::Transfers(rows)) = call(TransferCommand::List).await else {
                    panic!("no list");
                };
                assert!(rows.iter().all(|row| row.id != shared.infohash));

                // The owner's own download elsewhere on disk: a paired
                // Flick may unregister it, never delete its files.
                let resolver =
                    flick::torrent::TransferSession::start(flick::torrent::TransferConfig::local(
                        dir.path().join("resolver-downloads"),
                        dir.path().join("resolver-state"),
                    ))
                    .await
                    .unwrap();
                let resolved = resolver
                    .resolve(
                        flick::torrent::TorrentInput::Bytes(shared.torrent_bytes().to_vec()),
                        vec![],
                        Duration::from_secs(10),
                    )
                    .await
                    .unwrap();
                resolver.stop().await;
                let elsewhere = dir.path().join("Owner Elsewhere");
                services
                    .send(crate::services::Command::Download {
                        torrent: resolved,
                        files: vec![film],
                        folder: elsewhere.clone(),
                    })
                    .unwrap();
                let local = loop {
                    let Ok(TransferReply::Transfers(rows)) = call(TransferCommand::List).await
                    else {
                        panic!("no list");
                    };
                    if let Some(row) = rows.into_iter().find(|row| row.id == shared.infohash)
                        && row.phase != TransferPhase::Checking
                    {
                        break row;
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                };
                assert!(!local.share);
                let refused = call(TransferCommand::Remove {
                    id: shared.infohash.clone(),
                    delete_files: true,
                    confirm: flick::remote::transfers::deletion_paths(&local),
                })
                .await
                .err()
                .expect("the owner's files elsewhere are kept");
                assert_eq!((refused.status, refused.files), (403, None));
                assert!(
                    refused.message.contains("Keep Files"),
                    "{}",
                    refused.message
                );
                assert!(
                    elsewhere
                        .canonicalize()
                        .unwrap()
                        .join(&shared.infohash)
                        .is_dir()
                );
                assert!(matches!(
                    call(TransferCommand::Remove {
                        id: shared.infohash.clone(),
                        delete_files: false,
                        confirm: vec![],
                    })
                    .await,
                    Ok(TransferReply::Done)
                ));

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
                let row = rows
                    .iter()
                    .find(|row| row.id == seeded.infohash)
                    .expect("the share is listed");
                assert!(row.share);
                assert!(row.files.iter().all(|file| !file.will_delete));
                // A share's files are the owner's originals: never deleted
                // through the wire, whatever the confirmation says.
                let original = share_root.join("Clip.mp4");
                for confirm in [vec![], vec!["Clip.mp4".to_owned()]] {
                    let refused = call(TransferCommand::Remove {
                        id: seeded.infohash.clone(),
                        delete_files: true,
                        confirm,
                    })
                    .await
                    .err()
                    .expect("a share's files are kept");
                    assert_eq!((refused.status, refused.files), (403, None));
                    assert!(
                        refused.message.contains("shared folder"),
                        "{}",
                        refused.message
                    );
                    assert!(original.is_file());
                }
                assert!(matches!(
                    call(TransferCommand::Remove {
                        id: seeded.infohash.clone(),
                        delete_files: false,
                        confirm: vec![],
                    })
                    .await,
                    Ok(TransferReply::Done)
                ));
                assert_eq!(std::fs::read(&original).unwrap(), bytes[..300_000]);
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
