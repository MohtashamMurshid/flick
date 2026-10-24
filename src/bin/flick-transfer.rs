//! Development diagnostic for the shared transfer adapter. No GUI or media runtime.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use flick::torrent::{TorrentInput, TransferConfig, TransferSession};
use serde::Serialize;
use std::{
    io::{self, Write},
    net::SocketAddr,
    path::PathBuf,
    time::Duration,
};

#[derive(Parser)]
#[command(about = "Flick transfer diagnostic: inspect, download, and share local files")]
struct Args {
    /// Separate transfer state directory; locked against concurrent sessions.
    #[arg(long)]
    state_dir: PathBuf,
    /// Downloads are stored beneath this directory, grouped by infohash.
    #[arg(long)]
    download_dir: PathBuf,
    /// Enable public DHT/tracker discovery. Default is explicit peers only.
    #[arg(long)]
    public: bool,
    /// Incoming BitTorrent listener, not an HTTP API.
    #[arg(long, default_value = "127.0.0.1:0")]
    listen: SocketAddr,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Resolve a local .torrent, magnet link, or v1 infohash and list its files.
    Inspect {
        input: String,
        #[arg(long)]
        peer: Vec<SocketAddr>,
        #[arg(long, default_value_t = 60, value_parser = clap::value_parser!(u64).range(1..=86400))]
        timeout_seconds: u64,
    },
    /// Download selected files, wait for verification, then stop the session.
    Download {
        input: String,
        /// File index from inspect. Repeat to select several; omit for all.
        #[arg(long)]
        file: Vec<usize>,
        #[arg(long)]
        peer: Vec<SocketAddr>,
        #[arg(long, default_value_t = 300, value_parser = clap::value_parser!(u64).range(1..=86400))]
        timeout_seconds: u64,
    },
    /// Seed existing files until Ctrl+C. Export metadata to a new file.
    Share {
        path: PathBuf,
        #[arg(long)]
        torrent: PathBuf,
        #[arg(long)]
        tracker: Vec<String>,
    },
    /// Show restored transfer state, then stop. This is not a daemon.
    Status,
}

#[tokio::main(worker_threads = 2)]
async fn main() -> Result<()> {
    let args = Args::parse();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "error".into()),
        )
        .with_writer(io::stderr)
        .init();
    let session = TransferSession::start(TransferConfig {
        download_dir: args.download_dir,
        state_dir: args.state_dir,
        listen_addr: args.listen,
        public_discovery: args.public,
        download_limit_bytes_per_second: None,
    })
    .await?;
    let command_timeout = match &args.command {
        Command::Inspect {
            timeout_seconds, ..
        }
        | Command::Download {
            timeout_seconds, ..
        } => Some(Duration::from_secs(*timeout_seconds)),
        _ => None,
    };
    let result = tokio::select! {
        result = async {
            match command_timeout {
                Some(limit) => tokio::time::timeout(limit, run(&session, args.command)).await.context("Transfer command timed out")?,
                None => run(&session, args.command).await,
            }
        } => result,
        signal = tokio::signal::ctrl_c() => signal.context("Listen for Ctrl+C").and_then(|()| {
            anyhow::ensure!(command_timeout.is_none(), "Interrupted before the command finished");
            Ok(())
        }),
    };
    session.stop().await;
    result
}

async fn run(session: &TransferSession, command: Command) -> Result<()> {
    match command {
        Command::Inspect {
            input,
            peer,
            timeout_seconds,
        } => {
            json(
                &session
                    .resolve(
                        torrent_input(input),
                        peer,
                        Duration::from_secs(timeout_seconds),
                    )
                    .await?,
            )?;
        }
        Command::Download {
            input,
            file,
            peer,
            timeout_seconds,
        } => {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_seconds);
            let resolved = session
                .resolve(
                    torrent_input(input),
                    peer.clone(),
                    deadline.saturating_duration_since(tokio::time::Instant::now()),
                )
                .await?;
            let id = session
                .download(&resolved, (!file.is_empty()).then_some(file))
                .await?;
            session.resume(&id, peer).await?;
            session
                .wait_for_completion(
                    &id,
                    deadline.saturating_duration_since(tokio::time::Instant::now()),
                )
                .await?;
            json(&session.snapshot(&id)?)?;
        }
        Command::Share {
            path,
            torrent,
            tracker,
        } => {
            // Refuse an existing output before hashing or starting a seed.
            anyhow::ensure!(!torrent.try_exists()?, "Torrent output already exists");
            let shared = session.share(&path, tracker).await?;
            let mut output = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&torrent)
                .context("Create torrent output")?;
            output.write_all(shared.torrent_bytes())?;
            output.sync_all()?;
            #[derive(Serialize)]
            struct Output<'a> {
                #[serde(flatten)]
                shared: &'a flick::torrent::SharedTorrent,
                listen_addr: Option<SocketAddr>,
                torrent: PathBuf,
            }
            json(&Output {
                shared: &shared,
                listen_addr: session.listen_addr(),
                torrent,
            })?;
            eprintln!("Sharing while this process is running. Press Ctrl+C to stop.");
            tokio::signal::ctrl_c().await?;
        }
        Command::Status => json(&session.snapshots()?)?,
    }
    Ok(())
}

fn torrent_input(value: String) -> TorrentInput {
    if value.starts_with("https://") {
        TorrentInput::Url(value)
    } else if value.starts_with("magnet:")
        || (value.len() == 40 && value.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        TorrentInput::Magnet(value)
    } else {
        TorrentInput::File(value.into())
    }
}

fn json(value: &impl Serialize) -> Result<()> {
    let mut stdout = io::stdout().lock();
    serde_json::to_writer(&mut stdout, value)?;
    writeln!(stdout)?;
    stdout.flush()?;
    Ok(())
}
