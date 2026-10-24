//! Development diagnostic for link downloads: video pages through yt-dlp and
//! files over HTTPS, using the same adapter as the desktop. No GUI or libmpv.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use flick::fetch::{self, FetchUpdate, WebLink, archive, ytdlp::Tools};
use serde::Serialize;
use std::{io::Write, path::PathBuf};
use tokio_util::sync::CancellationToken;

#[derive(Parser)]
#[command(about = "Flick link-download diagnostic: video pages via yt-dlp, files over HTTPS")]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Show the yt-dlp and FFmpeg that page downloads would use.
    Tools,
    /// List an Internet Archive item's video files and default selection.
    Archive { identifier: String },
    /// Download a link into a folder, printing one JSON update per line.
    /// Archive item pages download their default files. Ctrl+C cancels and
    /// removes partial files.
    Get {
        link: String,
        #[arg(long)]
        dest: PathBuf,
    },
}

#[tokio::main(worker_threads = 2)]
async fn main() -> Result<()> {
    let args = Args::parse();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "error".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    match args.command {
        Command::Tools => {
            let tools = Tools::discover()?;
            print(&serde_json::json!({"yt_dlp": tools.ytdlp, "ffmpeg": tools.ffmpeg}))
        }
        Command::Archive { identifier } => print(&archive::resolve(&identifier).await?),
        Command::Get { link, dest } => {
            let link = fetch::web_link(&link).context("Enter an http or https link")??;
            let source = match link {
                WebLink::Archive(identifier) => {
                    let item = archive::resolve(&identifier).await?;
                    item.source(item.preferred())?
                }
                link => link.source().context("Unsupported link")?,
            };
            let cancel = CancellationToken::new();
            let interrupt = cancel.clone();
            tokio::spawn(async move {
                if tokio::signal::ctrl_c().await.is_ok() {
                    interrupt.cancel();
                }
            });
            let mut output = Ok(());
            let result = fetch::fetch(source, &dest, &cancel, |update| {
                if output.is_ok() {
                    output = print(&update);
                }
            })
            .await;
            output?;
            match result {
                Ok(_) => print(&FetchUpdate::Done),
                Err(_) if cancel.is_cancelled() => {
                    print(&FetchUpdate::Cancelled)?;
                    std::process::exit(130);
                }
                Err(error) => Err(error),
            }
        }
    }
}

fn print(value: &impl Serialize) -> Result<()> {
    let mut stdout = std::io::stdout().lock();
    serde_json::to_writer(&mut stdout, value)?;
    writeln!(stdout)?;
    stdout.flush()?;
    Ok(())
}
