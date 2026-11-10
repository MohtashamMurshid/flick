//! Headless remote library server. See `docs/REMOTE.md` for running it on a
//! Mac mini, `docs/REMOTE_PLAN.md` for the design, and
//! `docs/REMOTE_DOWNLOADS_PLAN.md` for accepting downloads from other Flicks.
//!
//! Stdout carries one JSON line, `{"pairing": "...", "listen": "..."}`,
//! at startup or for `--print-pairing`. Logs go to stderr and never contain
//! the token.

use anyhow::{Context, Result, bail};
use clap::Parser;
use flick::{
    remote::{
        auth, bind,
        ids::Salt,
        protocol::Pairing,
        server::{Server, ServerConfig, Sources},
        transfers::{self, BackendOptions, TransferBackend},
    },
    torrent::{TransferConfig, TransferSession},
};
use std::{
    net::{IpAddr, SocketAddr},
    num::NonZeroU32,
    path::PathBuf,
    sync::Arc,
};

/// Inside the state folder: the transfer session's state.
const TRANSFER_STATE_DIR: &str = "transfers";
/// Inside the state folder: completed downloads served with `--library`.
const DOWNLOADS_LIST: &str = "remote-downloads.json";

#[derive(Parser)]
#[command(
    about = "Serve a Flick library to other Flicks on this Tailscale network",
    long_about = "Serves the desktop app's library, or folders, over HTTP on this machine's Tailscale address only. Pair another Flick with the string printed at startup or by --print-pairing."
)]
struct Args {
    /// Serve the videos in the desktop app's settings.json.
    #[arg(long, conflicts_with = "folders")]
    library: bool,
    /// Serve the videos in this folder; repeat for several.
    #[arg(long = "folder", value_name = "PATH")]
    folders: Vec<PathBuf>,
    /// Address to listen on. Defaults to this machine's Tailscale IPv4 address;
    /// only Tailscale addresses and 127.0.0.1 are accepted.
    #[arg(long, value_name = "IP:PORT")]
    bind: Option<SocketAddr>,
    /// Port when --bind is not given.
    #[arg(long, default_value_t = flick::remote::protocol::DEFAULT_PORT)]
    port: u16,
    /// Name shown in the other Flick's library. Defaults to this computer's name.
    #[arg(long)]
    name: Option<String>,
    /// Where the token, id salt, and transcode temporaries are kept.
    /// Defaults to the Flick data directory, in `remote`.
    #[arg(long, value_name = "DIR")]
    state_dir: Option<PathBuf>,
    /// Full path to ffmpeg, used for quality tiers below Original.
    #[arg(long, value_name = "PATH")]
    ffmpeg: Option<PathBuf>,
    /// Transcode jobs allowed at once.
    #[arg(long, default_value_t = 2)]
    max_transcodes: usize,
    /// Print the pairing string for the saved (or a fresh) token and exit.
    #[arg(long)]
    print_pairing: bool,
    /// Replace the saved token with a new one, revoking every paired Flick.
    #[arg(long)]
    new_token: bool,
    /// Let paired Flicks start, pause, and remove torrents here, and play
    /// them while they download. Off unless given.
    #[arg(long)]
    allow_downloads: bool,
    /// Where downloads go. Defaults to `Downloads` in the Flick data directory.
    #[arg(long, value_name = "DIR", requires = "allow_downloads")]
    download_dir: Option<PathBuf>,
    /// A folder paired Flicks may browse and share (seed) from; repeat for
    /// several. Nothing outside these folders is shown.
    #[arg(long = "share-root", value_name = "DIR", requires = "allow_downloads")]
    share_roots: Vec<PathBuf>,
    /// Let paired Flicks start link downloads (yt-dlp and HTTPS). Offered
    /// only when yt-dlp is found.
    #[arg(long, requires = "allow_downloads")]
    links: bool,
    /// Keep transfers off the Internet: no DHT, trackers, or port mapping;
    /// listen on 127.0.0.1 only. For tests with explicit local peers.
    #[arg(long, requires = "allow_downloads")]
    local_transfers: bool,
    /// An explicit peer for every torrent, for tests with --local-transfers.
    #[arg(
        long = "peer",
        value_name = "IP:PORT",
        requires = "allow_downloads",
        hide = true
    )]
    peers: Vec<SocketAddr>,
    /// Download speed limit in KiB/s for transfers started here.
    #[arg(long, value_name = "KIB_PER_SECOND", requires = "allow_downloads")]
    download_limit: Option<u32>,
    /// Upload speed limit in KiB/s.
    #[arg(long, value_name = "KIB_PER_SECOND", requires = "allow_downloads")]
    upload_limit: Option<u32>,
    /// Stop sharing a download once it completes, instead of seeding it.
    #[arg(long, requires = "allow_downloads")]
    no_seeding: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "flick=info".into()),
        )
        .init();

    let state_dir = match args.state_dir.clone() {
        Some(dir) => dir,
        None => default_state_dir()?,
    };
    let bind = match args.bind {
        Some(bind) => bind,
        None => SocketAddr::new(IpAddr::V4(bind::tailscale_ipv4().await?), args.port),
    };
    if let Some(reason) = bind::refusal(bind.ip()) {
        bail!(reason);
    }
    // Only after the address is acceptable, so a refused start leaves no
    // state folder or token behind.
    let token = auth::load_or_create_token(&state_dir, args.new_token)?;

    if args.print_pairing {
        if bind.port() == 0 {
            bail!(
                "--print-pairing needs a fixed port; port 0 is chosen when the server starts, and its startup line shows the real one"
            );
        }
        print_pairing(bind, &token)?;
        return Ok(());
    }

    if !args.library && args.folders.is_empty() {
        bail!("Choose what to serve: --library, or one or more --folder PATH");
    }
    let downloads = if args.allow_downloads {
        Some(start_downloads(&args, &state_dir).await?)
    } else {
        None
    };
    let sources = match (&downloads, args.library) {
        (None, true) => Sources::SettingsFile(settings_path()?),
        (None, false) => Sources::Folders(args.folders.clone()),
        // The desktop's settings file stays read-only; what this server
        // downloads is listed beside it.
        (Some(_), true) => Sources::Several(vec![
            Sources::SettingsFile(settings_path()?),
            Sources::PathList(state_dir.join(DOWNLOADS_LIST)),
        ]),
        (Some(downloads), false) => {
            let mut folders = args.folders.clone();
            folders.push(downloads.download_dir.clone());
            Sources::Folders(folders)
        }
    };
    let started = Server::start(ServerConfig {
        bind,
        token: token.clone(),
        name: args.name.clone().unwrap_or_else(computer_name),
        sources,
        poster_cache: poster_cache().filter(|dir| dir.is_dir()),
        state_dir,
        ffmpeg: args.ffmpeg.clone(),
        max_transcodes: args.max_transcodes,
        transfers: downloads
            .as_ref()
            .map(|downloads| downloads.backend.clone()),
    })
    .await;
    let server = match started {
        Ok(server) => server,
        Err(error) => {
            if let Some(downloads) = downloads {
                downloads.stop().await;
            }
            return Err(error);
        }
    };
    print_pairing(server.addr(), &token)?;

    wait_for_stop().await?;
    tracing::info!("Stopping the remote library server");
    server.shutdown().await;
    if let Some(downloads) = downloads {
        downloads.stop().await;
    }
    Ok(())
}

/// The transfer session and its backend task for `--allow-downloads`.
struct Downloads {
    session: Arc<TransferSession>,
    backend: TransferBackend,
    download_dir: PathBuf,
    stop: tokio_util::sync::CancellationToken,
    task: tokio::task::JoinHandle<()>,
}

impl Downloads {
    /// Cancels link downloads, then stops the transfer session.
    async fn stop(self) {
        self.stop.cancel();
        let _ = self.task.await;
        self.session.shutdown().await;
    }
}

async fn start_downloads(args: &Args, state_dir: &std::path::Path) -> Result<Downloads> {
    let download_dir = match &args.download_dir {
        Some(dir) => dir.clone(),
        None => default_data_dir()?.join("Downloads"),
    };
    std::fs::create_dir_all(&download_dir)
        .with_context(|| format!("Create {}", download_dir.display()))?;
    let download_dir = download_dir
        .canonicalize()
        .with_context(|| format!("Find {}", download_dir.display()))?;
    let local = args.local_transfers;
    let kib = |limit: Option<u32>| limit.and_then(|kib| NonZeroU32::new(kib.saturating_mul(1024)));
    let session = TransferSession::start(TransferConfig {
        download_dir: download_dir.clone(),
        state_dir: state_dir.join(TRANSFER_STATE_DIR),
        listen_addr: (if local { [127, 0, 0, 1] } else { [0, 0, 0, 0] }, 0).into(),
        // This is the owner's own machine, so it finds peers the way the
        // desktop does unless told to stay local.
        public_discovery: !local,
        listener_mode: if local {
            librqbit::ListenerMode::TcpOnly
        } else {
            librqbit::ListenerMode::TcpAndUtp
        },
        download_limit_bytes_per_second: kib(args.download_limit),
        upload_limit_bytes_per_second: kib(args.upload_limit),
        seed_completed_downloads: !args.no_seeding,
    })
    .await
    .context("Start transfers")?;
    let session = Arc::new(session);
    let links = args.links
        && match flick::fetch::ytdlp::Tools::discover() {
            Ok(_) => true,
            Err(error) => {
                tracing::warn!("Link downloads are off: {error:#}");
                false
            }
        };
    let mut options = BackendOptions::new(download_dir.clone(), Salt::load_or_create(state_dir)?);
    options.share_roots = args.share_roots.clone();
    options.initial_peers = args.peers.clone();
    options.links = links;
    options.downloads_list = args.library.then(|| state_dir.join(DOWNLOADS_LIST));
    let capabilities = options.capabilities();
    tracing::info!(?capabilities, "Accepting downloads from paired Flicks");
    let stop = options.stop.clone();
    let (sender, receiver) = tokio::sync::mpsc::channel(64);
    let task = tokio::spawn(transfers::serve_backend(session.clone(), options, receiver));
    Ok(Downloads {
        session,
        backend: TransferBackend::new(capabilities, sender),
        download_dir,
        stop,
        task,
    })
}

fn print_pairing(listen: SocketAddr, token: &str) -> Result<()> {
    use std::io::Write;
    let pairing = Pairing {
        host: listen.ip().to_string(),
        port: listen.port(),
        token: token.to_owned(),
    };
    let line = serde_json::json!({
        "pairing": pairing.to_string(),
        "listen": listen.to_string(),
    });
    let mut stdout = std::io::stdout().lock();
    writeln!(stdout, "{line}")?;
    stdout.flush()?;
    Ok(())
}

/// Ctrl+C, or SIGTERM from launchd or systemd.
async fn wait_for_stop() -> Result<()> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut terminate = signal(SignalKind::terminate()).context("Listen for SIGTERM")?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result.context("Listen for Ctrl+C"),
            _ = terminate.recv() => Ok(()),
        }
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await.context("Listen for Ctrl+C")
    }
}

fn project_dirs() -> Option<directories::ProjectDirs> {
    directories::ProjectDirs::from("app", "Flick", "Flick")
}

/// The Flick data directory, as the desktop's services use it.
fn default_data_dir() -> Result<PathBuf> {
    std::env::var_os("FLICK_DATA_DIR")
        .map(PathBuf::from)
        .or_else(|| project_dirs().map(|dirs| dirs.data_local_dir().to_owned()))
        .context("No data directory for Flick on this system; pass --state-dir")
}

/// The Flick data directory plus `remote`.
fn default_state_dir() -> Result<PathBuf> {
    default_data_dir().map(|base| base.join("remote"))
}

/// The desktop's settings file, found the way `Store::load` finds it.
fn settings_path() -> Result<PathBuf> {
    std::env::var_os("FLICK_SETTINGS_PATH")
        .map(PathBuf::from)
        .or_else(|| project_dirs().map(|dirs| dirs.config_dir().join("settings.json")))
        .context("No configuration directory for Flick on this system; set FLICK_SETTINGS_PATH")
}

/// The desktop's poster cache, found the way the desktop finds it.
fn poster_cache() -> Option<PathBuf> {
    std::env::var_os("FLICK_DATA_DIR")
        .map(PathBuf::from)
        .or_else(|| project_dirs().map(|dirs| dirs.cache_dir().to_owned()))
        .map(|base| base.join("thumbnails"))
}

/// The computer's friendly name on macOS, else the host name.
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
