//! Desktop settings for remote libraries: paired servers and playback
//! progress on their videos. Saved inside `settings.json` under `remote`.
//!
//! Owned by the desktop client agent. Remote videos never enter
//! `Settings::library`, because the file watcher checks every library path
//! on disk; they live in memory in `crate::remote_client`.

use flick::remote::protocol::Quality;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct RemoteSettings {
    pub servers: Vec<RemoteServer>,
    pub progress: Vec<RemoteProgress>,
}

impl RemoteSettings {
    pub fn is_empty(&self) -> bool {
        self.servers.is_empty() && self.progress.is_empty()
    }
}

/// A paired server. `id` is random and local; it keys progress entries so
/// renaming or re-pairing a server keeps its positions.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct RemoteServer {
    pub id: String,
    pub name: String,
    pub host: String,
    pub port: u16,
    /// The pairing token. Never logged. `FLICK_REMOTE_TOKEN` overrides it
    /// for development and is never written here.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub token: String,
    /// Default tier for this server; the player can change it per video.
    pub quality: Quality,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct RemoteProgress {
    pub server: String,
    pub video: String,
    pub title: String,
    pub position: f64,
    pub duration: f64,
    pub finished: bool,
    /// Seconds since the Unix epoch.
    pub updated: u64,
}
