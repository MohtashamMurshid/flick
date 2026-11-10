//! Desktop settings for remote libraries: paired servers and playback
//! progress on their videos. Saved inside `settings.json` under `remote`.
//!
//! Owned by the desktop client agent. Remote videos never enter
//! `Settings::library`, because the file watcher checks every library path
//! on disk; they live in memory in `crate::remote_client`.

use flick::remote::protocol::Quality;
use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct RemoteSettings {
    pub servers: Vec<RemoteServer>,
    pub progress: Vec<RemoteProgress>,
    /// "Share my library on Tailscale": run the remote library server inside
    /// this Flick. Off by default; started on launch while on.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub serve: bool,
    /// The name other Flicks show for this library. Empty uses the
    /// computer's name.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub serve_name: String,
    /// "Let paired Flicks download to this Mac". Off by default; every
    /// write route answers 403 while it is off.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub allow_downloads: bool,
    /// Where remote-started downloads go; None uses the app's download folder.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub download_dir: Option<std::path::PathBuf>,
    /// Folders other Flicks may browse and seed from.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub share_roots: Vec<std::path::PathBuf>,
    /// The Save to default: a paired server's id, or empty for this Mac.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub default_destination: String,
    /// Remote videos copied to this computer.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub kept: Vec<KeptCopy>,
    /// Commands waiting for a machine that was asleep.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub queued: Vec<QueuedCommand>,
    /// Set on the last onboarding step; a launch without it starts there.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub setup_done: bool,
    /// "Keep this Mac awake while sharing": hold a `caffeinate -i` power
    /// assertion while the in-app server runs. On by default; macOS only.
    #[serde(skip_serializing_if = "is_true")]
    pub keep_awake: bool,
}

/// Written by hand so `keep_awake` defaults to on, in a missing `remote`
/// and in a `remote` without the field alike.
impl Default for RemoteSettings {
    fn default() -> Self {
        Self {
            servers: Vec::new(),
            progress: Vec::new(),
            serve: false,
            serve_name: String::new(),
            allow_downloads: false,
            download_dir: None,
            share_roots: Vec::new(),
            default_destination: String::new(),
            kept: Vec::new(),
            queued: Vec::new(),
            setup_done: false,
            keep_awake: true,
        }
    }
}

fn is_true(value: &bool) -> bool {
    *value
}

impl RemoteSettings {
    pub fn is_empty(&self) -> bool {
        self.servers.is_empty()
            && self.progress.is_empty()
            && !self.serve
            && self.serve_name.is_empty()
            && !self.allow_downloads
            && self.download_dir.is_none()
            && self.share_roots.is_empty()
            && self.default_destination.is_empty()
            && self.kept.is_empty()
            && self.queued.is_empty()
            && !self.setup_done
            && self.keep_awake
    }
}

/// A paired server. `id` is random and local; it keys progress entries so
/// renaming or re-pairing a server keeps its positions. `Debug` redacts the
/// token.
#[derive(Clone, Default, Deserialize, Serialize)]
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
    /// Drop a tier on repeated buffering early in playback; on by default.
    #[serde(default = "default_true")]
    pub auto_quality: bool,
    /// Learned from the server's listing for Wake-on-LAN when it is offline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wake: Option<WakeTarget>,
}

fn default_true() -> bool {
    true
}

/// Where a Wake-on-LAN packet goes; both lists come from `Hardware`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct WakeTarget {
    pub mac: Vec<String>,
    pub lan_ips: Vec<String>,
}

/// A remote video copied to this computer with "Keep on this Mac".
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct KeptCopy {
    pub server: String,
    pub video: String,
    pub path: std::path::PathBuf,
}

/// A download or link requested while its machine was asleep; sent when the
/// machine answers again.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct QueuedCommand {
    pub server: String,
    /// A magnet, infohash, `.torrent` URL, or a video page link.
    pub input: String,
    /// Torrent file indices; empty means every file. Ignored for links.
    pub files: Vec<usize>,
    pub link: bool,
    /// Seconds since the Unix epoch.
    pub created: u64,
}

/// Keeps the token out of logs, panic messages, and `Settings`' Debug.
impl fmt::Debug for RemoteServer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RemoteServer")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("host", &self.host)
            .field("port", &self.port)
            .field(
                "token",
                &if self.token.is_empty() {
                    ""
                } else {
                    "<redacted>"
                },
            )
            .field("quality", &self.quality)
            .finish()
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn debug_output_redacts_the_token() {
        let server = RemoteServer {
            id: "id".into(),
            name: "Mac mini".into(),
            host: "100.98.200.69".into(),
            port: 47117,
            token: TOKEN.into(),
            quality: Quality::Original,
            auto_quality: true,
            wake: None,
        };
        let text = format!("{server:?}");
        assert!(!text.contains(TOKEN), "{text}");
        assert!(
            text.contains("<redacted>") && text.contains("Mac mini"),
            "{text}"
        );
        let settings = RemoteSettings {
            servers: vec![server],
            ..Default::default()
        };
        let text = format!("{settings:?} {settings:#?}");
        assert!(!text.contains(TOKEN), "{text}");
        let all = crate::store::Settings {
            remote: settings,
            ..Default::default()
        };
        assert!(!format!("{all:?}").contains(TOKEN));
    }

    #[test]
    fn keep_awake_defaults_on_and_is_saved_only_when_off() {
        assert!(RemoteSettings::default().keep_awake);
        let parsed: RemoteSettings = serde_json::from_str(r#"{"serve": true}"#).unwrap();
        assert!(parsed.keep_awake);
        assert!(
            !serde_json::to_string(&parsed)
                .unwrap()
                .contains("keep_awake")
        );
        let off = RemoteSettings {
            keep_awake: false,
            ..Default::default()
        };
        assert!(!off.is_empty());
        let text = serde_json::to_string(&off).unwrap();
        assert!(text.contains(r#""keep_awake":false"#), "{text}");
        assert!(
            !serde_json::from_str::<RemoteSettings>(&text)
                .unwrap()
                .keep_awake
        );
    }
}
