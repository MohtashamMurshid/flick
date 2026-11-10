//! What the player is playing: a file on this machine, or a video on a paired
//! remote library streamed over HTTP.

use flick::remote::protocol::{self, AudioTrack, Quality};
use std::{
    fmt,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, PartialEq)]
pub enum Source {
    /// A local file, or a torrent file still downloading at this path.
    Local(PathBuf),
    Remote(RemoteSource),
}

impl Source {
    pub fn local_path(&self) -> Option<&Path> {
        match self {
            Source::Local(path) => Some(path),
            Source::Remote(_) => None,
        }
    }

    /// The title shown in the title bar, the window title, and Now Playing.
    pub fn title(&self) -> String {
        match self {
            Source::Local(path) => crate::model::file_title(path),
            Source::Remote(remote) if remote.title.trim().is_empty() => "Untitled video".into(),
            Source::Remote(remote) => remote.title.clone(),
        }
    }
}

/// A video on a paired server, at one quality tier.
#[derive(Clone, PartialEq)]
pub struct RemoteSource {
    /// `store::remote::RemoteServer::id`; keys saved progress.
    pub server_id: String,
    pub server_name: String,
    pub video_id: String,
    pub title: String,
    /// Seconds from the listing, or 0 when unknown.
    pub duration: f64,
    pub quality: Quality,
    /// `http://host:port`, without a trailing slash.
    pub base_url: String,
    /// Sent as `Authorization: Bearer …`. Never logged or put in a URL.
    pub token: String,
}

impl RemoteSource {
    /// What libmpv loads. The token is never part of it.
    pub fn url(&self) -> String {
        self.url_with_audio(None)
    }

    /// What libmpv loads with a chosen audio stream (an
    /// [`AudioTrack::track`]). Only transcode tiers carry it: at Original
    /// the file's own tracks are all there and libmpv switches among them.
    ///
    /// The choice lives beside the source in the player rather than in a
    /// field, so `RemoteSource` literals elsewhere stay as they are.
    pub fn url_with_audio(&self, audio: Option<u32>) -> String {
        let path = match audio {
            Some(track) if self.quality.is_transcode() => {
                protocol::paths::hls_playlist_audio(&self.video_id, self.quality, track)
            }
            _ => protocol::paths::playback(&self.video_id, self.quality),
        };
        format!("{}{path}", self.base_url.trim_end_matches('/'))
    }

    /// The libmpv `http-header-fields` value for this server.
    pub fn header(&self) -> String {
        format!("Authorization: {}", protocol::bearer(&self.token))
    }
}

/// Whether the player's audio menu lists the server's audio streams
/// instead of libmpv's tracks: a transcode carries only the one stream it
/// was asked for, so the others must be requested from the server.
pub fn server_audio_menu(quality: Quality, tracks: &[AudioTrack]) -> bool {
    quality.is_transcode() && !tracks.is_empty()
}

/// The stream a transcode is playing: the one asked for, else the first,
/// which the server maps when no `?audio=` is given.
pub fn current_audio(chosen: Option<u32>, tracks: &[AudioTrack]) -> Option<u32> {
    chosen
        .filter(|track| tracks.iter().any(|t| t.track == *track))
        .or_else(|| tracks.first().map(|t| t.track))
}

/// A menu label for a server audio stream: title, language, channels.
pub fn audio_track_label(track: &AudioTrack) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut push = |text: Option<&str>| {
        if let Some(text) = text.map(str::trim).filter(|text| !text.is_empty())
            && !parts.iter().any(|part| part.eq_ignore_ascii_case(text))
        {
            parts.push(text.to_owned());
        }
    };
    push(track.title.as_deref());
    push(track.language.as_deref());
    let channels = match track.channels {
        0 => None,
        1 => Some("Mono".to_owned()),
        2 => Some("Stereo".to_owned()),
        6 => Some("5.1".to_owned()),
        8 => Some("7.1".to_owned()),
        n => Some(format!("{n} channels")),
    };
    push(channels.as_deref());
    if parts.is_empty() {
        format!("Track {}", track.track + 1)
    } else {
        parts.join(" · ")
    }
}

/// Keeps the token out of logs and panic messages.
impl fmt::Debug for RemoteSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RemoteSource")
            .field("server_id", &self.server_id)
            .field("server_name", &self.server_name)
            .field("video_id", &self.video_id)
            .field("title", &self.title)
            .field("duration", &self.duration)
            .field("quality", &self.quality)
            .field("base_url", &self.base_url)
            .field("token", &"<redacted>")
            .finish()
    }
}

/// libmpv's HTTP request headers. Set to the bearer header just before a
/// remote load and kept for that session, because libmpv reads it again for
/// every HLS segment and reconnect; the next load replaces or clears it, so no later load
/// can carry the token.
pub const HTTP_HEADERS: &str = "http-header-fields";

/// Network and cache tuning as `(property, remote value, libmpv default)`.
/// Remote loads apply the second column; every local load explicitly
/// restores the third, so tuning never carries over to local playback.
/// The remote values are a starting point to replace with measurements.
pub const NETWORK_TUNING: [(&str, &str, &str); 5] = [
    ("cache", "yes", "auto"),
    ("demuxer-max-bytes", "512MiB", "150MiB"),
    ("demuxer-max-back-bytes", "128MiB", "50MiB"),
    ("demuxer-readahead-secs", "20", "1"),
    ("network-timeout", "30", "60"),
];

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn remote(quality: Quality) -> RemoteSource {
        RemoteSource {
            server_id: "server-1".into(),
            server_name: "Mac mini".into(),
            video_id: "abc".into(),
            title: "A Film".into(),
            duration: 120.,
            quality,
            base_url: "http://100.98.200.69:47117".into(),
            token: TOKEN.into(),
        }
    }

    #[test]
    fn remote_url_follows_the_shared_paths_and_never_holds_the_token() {
        assert_eq!(
            remote(Quality::Original).url(),
            "http://100.98.200.69:47117/v1/media/abc"
        );
        assert_eq!(
            remote(Quality::Low720).url(),
            "http://100.98.200.69:47117/v1/hls/abc/720p/index.m3u8"
        );
        let mut slash = remote(Quality::Original);
        slash.base_url.push('/');
        assert_eq!(slash.url(), "http://100.98.200.69:47117/v1/media/abc");
        for quality in Quality::ALL {
            assert!(!remote(quality).url().contains(TOKEN));
        }
    }

    #[test]
    fn a_chosen_audio_stream_reaches_transcodes_only() {
        assert_eq!(
            remote(Quality::Low720).url_with_audio(Some(2)),
            "http://100.98.200.69:47117/v1/hls/abc/720p/index.m3u8?audio=2"
        );
        assert_eq!(
            remote(Quality::Low720).url_with_audio(None),
            remote(Quality::Low720).url()
        );
        // Original plays the file; libmpv picks among its own tracks.
        assert_eq!(
            remote(Quality::Original).url_with_audio(Some(2)),
            "http://100.98.200.69:47117/v1/media/abc"
        );
        for quality in Quality::ALL {
            assert!(!remote(quality).url_with_audio(Some(1)).contains(TOKEN));
        }
    }

    #[test]
    fn the_audio_menu_offers_server_streams_for_transcodes() {
        let track =
            |track: u32, language: Option<&str>, title: Option<&str>, channels: u32| AudioTrack {
                track,
                language: language.map(Into::into),
                title: title.map(Into::into),
                channels,
            };
        let tracks = [
            track(0, Some("eng"), None, 6),
            track(1, Some("jpn"), Some("Commentary"), 2),
            track(2, None, None, 0),
        ];
        assert!(server_audio_menu(Quality::Low720, &tracks));
        assert!(!server_audio_menu(Quality::Original, &tracks));
        assert!(!server_audio_menu(Quality::Low720, &[]));
        assert_eq!(current_audio(None, &tracks), Some(0));
        assert_eq!(current_audio(Some(1), &tracks), Some(1));
        // A choice the listing no longer has falls back to the default.
        assert_eq!(current_audio(Some(9), &tracks), Some(0));
        assert_eq!(current_audio(None, &[]), None);
        assert_eq!(audio_track_label(&tracks[0]), "eng · 5.1");
        assert_eq!(audio_track_label(&tracks[1]), "Commentary · jpn · Stereo");
        assert_eq!(audio_track_label(&tracks[2]), "Track 3");
        assert_eq!(
            audio_track_label(&track(4, Some("en"), Some("EN"), 3)),
            "EN · 3 channels"
        );
    }

    #[test]
    fn remote_header_is_a_bearer_authorization() {
        assert_eq!(
            remote(Quality::Original).header(),
            format!("Authorization: Bearer {TOKEN}")
        );
    }

    #[test]
    fn debug_output_redacts_the_token() {
        let text = format!("{:?}", Source::Remote(remote(Quality::Original)));
        assert!(!text.contains(TOKEN), "{text}");
        assert!(text.contains("<redacted>"));
    }

    #[test]
    fn titles_paths_and_kinds() {
        let local = Source::Local(PathBuf::from("/films/Night Train.mkv"));
        assert_eq!(local.title(), "Night Train.mkv");
        assert_eq!(
            local.local_path(),
            Some(Path::new("/films/Night Train.mkv"))
        );
        assert!(matches!(local, Source::Local(_)));

        let mut film = remote(Quality::Original);
        let source = Source::Remote(film.clone());
        assert_eq!(source.title(), "A Film");
        assert_eq!(source.local_path(), None);
        assert!(matches!(source, Source::Remote(_)));
        film.title = "  ".into();
        assert_eq!(Source::Remote(film).title(), "Untitled video");
    }

    #[test]
    fn network_tuning_covers_each_property_once() {
        let mut names: Vec<_> = NETWORK_TUNING.iter().map(|(name, ..)| *name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), NETWORK_TUNING.len());
        assert!(!names.contains(&HTTP_HEADERS));
    }
}
