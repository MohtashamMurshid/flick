//! Wire types shared by the server and the desktop client. Both sides build
//! URLs through [`paths`] so they cannot drift apart.

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::fmt;

/// Major protocol version, sent in every listing. A client that sees a newer
/// major asks for the server to be updated instead of guessing.
pub const VERSION: u32 = 1;

/// Default listening port. Outside the ephemeral range and not assigned by IANA.
pub const DEFAULT_PORT: u16 = 47117;

/// Scheme of the pairing string: `flick-remote://host:port/#token`.
pub const PAIRING_SCHEME: &str = "flick-remote";

/// Token length in bytes before hex encoding.
pub const TOKEN_BYTES: usize = 32;

/// The listing the server returns for `GET /v1/library`.
#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct Library {
    pub protocol: u32,
    pub server_name: String,
    /// Tiers this server can deliver. Always contains [`Quality::Original`];
    /// the others appear only when FFmpeg and an H.264 encoder were found.
    pub qualities: Vec<Quality>,
    pub videos: Vec<Video>,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct Video {
    /// Opaque, stable for this server; never a path.
    pub id: String,
    pub title: String,
    pub file_name: String,
    pub size: u64,
    /// Seconds, or 0 when unknown.
    pub duration: f64,
    pub width: u32,
    pub height: u32,
    pub mime: String,
    pub episode: Option<Episode>,
    /// False while a torrent download is still verifying this file; the
    /// media route answers 409 until it is complete.
    pub complete: bool,
    /// Whether `GET /v1/poster/{id}` has an image.
    pub poster: bool,
    /// Subtitle files next to the video, by index.
    pub subtitles: Vec<SubtitleFile>,
    /// Subtitle streams inside the file, extracted as WebVTT on demand.
    pub embedded_subtitles: Vec<EmbeddedSubtitle>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct Episode {
    pub show: String,
    pub season: u32,
    pub number: u32,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct SubtitleFile {
    pub index: u32,
    pub file_name: String,
    pub language: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct EmbeddedSubtitle {
    pub track: u32,
    pub language: Option<String>,
    pub title: Option<String>,
    /// Text subtitles can be served at every tier; bitmap ones cannot.
    pub text: bool,
}

/// A quality tier. Original is the file itself; the others are live H.264
/// transcodes delivered as HLS.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Deserialize, Serialize)]
pub enum Quality {
    #[default]
    #[serde(rename = "original")]
    Original,
    #[serde(rename = "1080p-high")]
    High1080,
    #[serde(rename = "1080p")]
    Standard1080,
    #[serde(rename = "720p")]
    Low720,
    #[serde(rename = "480p")]
    Data480,
}

impl Quality {
    pub const ALL: [Quality; 5] = [
        Quality::Original,
        Quality::High1080,
        Quality::Standard1080,
        Quality::Low720,
        Quality::Data480,
    ];

    /// The identifier used in URLs and JSON.
    pub fn as_str(self) -> &'static str {
        match self {
            Quality::Original => "original",
            Quality::High1080 => "1080p-high",
            Quality::Standard1080 => "1080p",
            Quality::Low720 => "720p",
            Quality::Data480 => "480p",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|q| q.as_str() == value)
    }

    pub fn label(self) -> &'static str {
        match self {
            Quality::Original => "Original",
            Quality::High1080 => "1080p High",
            Quality::Standard1080 => "1080p",
            Quality::Low720 => "720p",
            Quality::Data480 => "480p",
        }
    }

    /// Output height for transcodes; None keeps the source.
    pub fn height(self) -> Option<u32> {
        match self {
            Quality::Original => None,
            Quality::High1080 | Quality::Standard1080 => Some(1080),
            Quality::Low720 => Some(720),
            Quality::Data480 => Some(480),
        }
    }

    /// Target video bit rate in kbit/s; None for Original.
    pub fn video_kbps(self) -> Option<u32> {
        match self {
            Quality::Original => None,
            Quality::High1080 => Some(12_000),
            Quality::Standard1080 => Some(6_000),
            Quality::Low720 => Some(3_000),
            Quality::Data480 => Some(1_500),
        }
    }

    /// AAC stereo bit rate in kbit/s for transcodes.
    pub fn audio_kbps(self) -> Option<u32> {
        match self {
            Quality::Original => None,
            Quality::High1080 | Quality::Standard1080 => Some(192),
            Quality::Low720 | Quality::Data480 => Some(128),
        }
    }

    pub fn is_transcode(self) -> bool {
        self != Quality::Original
    }
}

impl fmt::Display for Quality {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Segment length for HLS transcodes, in seconds. Key frames are forced on
/// these boundaries so any segment can be made on its own.
pub const HLS_SEGMENT_SECONDS: u32 = 4;

/// Playback progress, `PUT /v1/progress/{id}` and inside listings later.
#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct Progress {
    pub position: f64,
    pub duration: f64,
    pub finished: bool,
    /// Seconds since the Unix epoch when this was saved.
    pub updated: u64,
}

/// JSON body of every error response.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct ErrorBody {
    pub message: String,
}

/// What one Flick copies from the server and pastes into the other:
/// `flick-remote://host:port/#token`. The token is in the fragment so it
/// never reaches a server log if the string is pasted into a browser.
#[derive(Clone, PartialEq, Eq)]
pub struct Pairing {
    pub host: String,
    pub port: u16,
    pub token: String,
}

/// The token is left out, so a logged pairing never reveals it.
impl std::fmt::Debug for Pairing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pairing")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("token", &"<redacted>")
            .finish()
    }
}

impl Pairing {
    pub fn parse(value: &str) -> Result<Self> {
        let value = value.trim();
        let rest = value
            .strip_prefix(PAIRING_SCHEME)
            .and_then(|rest| rest.strip_prefix("://"))
            .with_context(|| format!("A pairing string starts with {PAIRING_SCHEME}://"))?;
        let (authority, token) = rest
            .split_once("/#")
            .context("The pairing string is missing its token")?;
        let token = token.trim();
        ensure!(
            token.len() == TOKEN_BYTES * 2 && token.bytes().all(|b| b.is_ascii_hexdigit()),
            "The pairing token is not valid"
        );
        let (host, port) = if let Some(rest) = authority.strip_prefix('[') {
            let (host, port) = rest
                .split_once("]:")
                .context("An IPv6 pairing address needs a port after the bracket")?;
            (host.to_owned(), port)
        } else {
            let (host, port) = authority
                .rsplit_once(':')
                .context("The pairing address is missing its port")?;
            (host.to_owned(), port)
        };
        ensure!(
            !host.is_empty() && !host.contains(['/', '?', '#', ' ']),
            "The pairing host is not valid"
        );
        let port: u16 = port.parse().context("The pairing port is not a number")?;
        if port == 0 {
            bail!("The pairing port cannot be zero");
        }
        Ok(Self {
            host,
            port,
            token: token.to_ascii_lowercase(),
        })
    }

    /// `http://host:port`, with IPv6 hosts in brackets. Plain HTTP is
    /// correct here: the tailnet encrypts the link.
    pub fn base_url(&self) -> String {
        if self.host.contains(':') {
            format!("http://[{}]:{}", self.host, self.port)
        } else {
            format!("http://{}:{}", self.host, self.port)
        }
    }
}

impl fmt::Display for Pairing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.host.contains(':') {
            write!(
                f,
                "{PAIRING_SCHEME}://[{}]:{}/#{}",
                self.host, self.port, self.token
            )
        } else {
            write!(
                f,
                "{PAIRING_SCHEME}://{}:{}/#{}",
                self.host, self.port, self.token
            )
        }
    }
}

/// The `Authorization` header value for a token.
pub fn bearer(token: &str) -> String {
    format!("Bearer {token}")
}

/// Request paths, relative to the server's base URL.
pub mod paths {
    use super::Quality;

    pub fn library() -> String {
        "/v1/library".into()
    }

    pub fn media(id: &str) -> String {
        format!("/v1/media/{id}")
    }

    pub fn poster(id: &str) -> String {
        format!("/v1/poster/{id}")
    }

    pub fn subtitles(id: &str) -> String {
        format!("/v1/subtitles/{id}")
    }

    pub fn subtitle(id: &str, index: u32) -> String {
        format!("/v1/subtitles/{id}/{index}")
    }

    pub fn embedded_subtitle(id: &str, track: u32) -> String {
        format!("/v1/subtitles/{id}/embedded/{track}")
    }

    pub fn hash(id: &str) -> String {
        format!("/v1/hash/{id}")
    }

    pub fn hls_playlist(id: &str, quality: Quality) -> String {
        format!("/v1/hls/{id}/{quality}/index.m3u8")
    }

    pub fn hls_init(id: &str, quality: Quality) -> String {
        format!("/v1/hls/{id}/{quality}/init.mp4")
    }

    pub fn hls_segment(id: &str, quality: Quality, index: u32) -> String {
        format!("/v1/hls/{id}/{quality}/{index}.m4s")
    }

    pub fn progress(id: &str) -> String {
        format!("/v1/progress/{id}")
    }

    /// What the player loads for a video at a tier.
    pub fn playback(id: &str, quality: Quality) -> String {
        if quality.is_transcode() {
            hls_playlist(id, quality)
        } else {
            media(id)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn pairing_round_trips_for_names_and_ipv6() {
        for host in [
            "mohtashams-mac-mini.taildcfc7a.ts.net",
            "100.98.200.69",
            "fd7a:115c:a1e0::6c2e:c846",
        ] {
            let pairing = Pairing {
                host: host.into(),
                port: DEFAULT_PORT,
                token: TOKEN.into(),
            };
            let text = pairing.to_string();
            assert_eq!(Pairing::parse(&text).unwrap(), pairing, "{text}");
        }
        assert_eq!(
            Pairing::parse(&format!(
                " flick-remote://box:47117/#{} ",
                TOKEN.to_ascii_uppercase()
            ))
            .unwrap()
            .token,
            TOKEN
        );
        assert_eq!(
            Pairing {
                host: "fd7a::1".into(),
                port: 1,
                token: TOKEN.into()
            }
            .base_url(),
            "http://[fd7a::1]:1"
        );
    }

    #[test]
    fn pairing_debug_leaves_out_the_token() {
        let pairing = Pairing {
            host: "box".into(),
            port: DEFAULT_PORT,
            token: TOKEN.into(),
        };
        for text in [format!("{pairing:?}"), format!("{pairing:#?}")] {
            assert!(!text.contains(TOKEN), "{text}");
            assert!(text.contains("box") && text.contains("redacted"), "{text}");
        }
    }

    #[test]
    fn pairing_rejects_bad_scheme_token_host_and_port() {
        for bad in [
            format!("http://box:47117/#{TOKEN}"),
            "flick-remote://box:47117/#abc".to_owned(),
            format!("flick-remote://box/#{TOKEN}"),
            format!("flick-remote://box:0/#{TOKEN}"),
            format!("flick-remote://:47117/#{TOKEN}"),
            format!("flick-remote://box:47117/?t={TOKEN}"),
        ] {
            assert!(Pairing::parse(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn quality_names_are_stable_in_urls_and_json() {
        for quality in Quality::ALL {
            assert_eq!(Quality::parse(quality.as_str()), Some(quality));
            let json = serde_json::to_string(&quality).unwrap();
            assert_eq!(json, format!("\"{}\"", quality.as_str()));
            assert_eq!(serde_json::from_str::<Quality>(&json).unwrap(), quality);
        }
        assert_eq!(Quality::parse("4k"), None);
        assert_eq!(paths::playback("abc", Quality::Original), "/v1/media/abc");
        assert_eq!(
            paths::playback("abc", Quality::Low720),
            "/v1/hls/abc/720p/index.m3u8"
        );
    }
}
