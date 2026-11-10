//! Headless remote library server. See `docs/REMOTE_PLAN.md`.
//!
//! Owned by the server agent. This stub fixes the command line so the
//! launchd agent and documentation can be written against it.

use anyhow::{Result, bail};
use clap::Parser;
use std::{net::SocketAddr, path::PathBuf};

#[derive(Parser)]
#[command(
    about = "Serve a Flick library to other Flicks on this Tailscale network",
    long_about = "Serves the desktop app's library, or folders, over HTTP on this machine's Tailscale address only. Pair another Flick with the string printed at startup or by --print-pairing."
)]
struct Args {
    /// Serve the videos in the desktop app's settings.json.
    #[arg(long, conflicts_with = "folder")]
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
    let _ = (
        args.library,
        &args.folders,
        args.bind,
        args.port,
        &args.name,
        &args.state_dir,
        &args.ffmpeg,
        args.max_transcodes,
        args.print_pairing,
        args.new_token,
    );
    bail!("flick-serve is not implemented yet; see docs/REMOTE_PLAN.md")
}
