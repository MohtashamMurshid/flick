//! The HTTP server: listing, media with ranges, posters, subtitles, hash,
//! and progress. Transcode routes come from [`crate::remote::hls`].
//!
//! Owned by the server agent. The types below are the contract the
//! `flick-serve` binary and the desktop's in-app switch build against.

use anyhow::{Result, bail};
use std::{net::SocketAddr, path::PathBuf};

/// Where the served videos come from.
#[derive(Clone, Debug)]
pub enum Sources {
    /// The desktop app's `settings.json`, read-only.
    SettingsFile(PathBuf),
    /// Folders scanned for video files.
    Folders(Vec<PathBuf>),
}

#[derive(Clone, Debug)]
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
}

/// A running server. Dropping it does not stop it; call [`Server::shutdown`].
pub struct Server {
    addr: SocketAddr,
}

impl Server {
    pub async fn start(config: ServerConfig) -> Result<Server> {
        if let Some(reason) = crate::remote::bind::refusal(config.bind.ip()) {
            bail!(reason);
        }
        bail!("The remote library server is not implemented yet")
    }

    /// The bound address, with the real port when 0 was requested.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub async fn shutdown(self) {}
}
