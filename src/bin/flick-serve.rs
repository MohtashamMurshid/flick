//! Headless remote library server. See `docs/REMOTE.md` for running it on a
//! Mac mini and `docs/REMOTE_PLAN.md` for the design.
//!
//! Stdout carries one JSON line, `{"pairing": "...", "listen": "..."}`,
//! at startup or for `--print-pairing`. Logs go to stderr and never contain
//! the token.

use anyhow::{Context, Result, bail};
use clap::Parser;
use flick::remote::{
    auth, bind,
    protocol::Pairing,
    server::{Server, ServerConfig, Sources},
};
use std::{
    net::{IpAddr, SocketAddr},
    path::PathBuf,
};

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

    let state_dir = match args.state_dir {
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

    let sources = if args.library {
        Sources::SettingsFile(settings_path()?)
    } else if !args.folders.is_empty() {
        Sources::Folders(args.folders)
    } else {
        bail!("Choose what to serve: --library, or one or more --folder PATH");
    };
    let server = Server::start(ServerConfig {
        bind,
        token: token.clone(),
        name: args.name.unwrap_or_else(computer_name),
        sources,
        poster_cache: poster_cache().filter(|dir| dir.is_dir()),
        state_dir,
        ffmpeg: args.ffmpeg,
        max_transcodes: args.max_transcodes,
        transfers: None,
    })
    .await?;
    print_pairing(server.addr(), &token)?;

    wait_for_stop().await?;
    tracing::info!("Stopping the remote library server");
    server.shutdown().await;
    Ok(())
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

/// The Flick data directory, as the desktop's services use it, plus `remote`.
fn default_state_dir() -> Result<PathBuf> {
    std::env::var_os("FLICK_DATA_DIR")
        .map(PathBuf::from)
        .or_else(|| project_dirs().map(|dirs| dirs.data_local_dir().to_owned()))
        .map(|base| base.join("remote"))
        .context("No data directory for Flick on this system; pass --state-dir")
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
