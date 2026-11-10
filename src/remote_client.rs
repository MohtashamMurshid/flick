//! Desktop side of remote libraries: talks to paired servers on its own
//! runtime thread, caches listings and posters, and hands the player a URL.
//!
//! [`RemoteState`] is the one field the `Flick` view holds for this feature.
//! The view calls [`RemoteState::drain`] on its tick and `cx.notify()`s when
//! it returns true, and drops the images from
//! [`RemoteState::take_dropped_images`] with `window.drop_image`.
//!
//! - Pairing strings (`flick::remote::protocol::Pairing`) become
//!   `crate::store::remote::RemoteServer` entries with a fresh random id.
//! - `GET /v1/library` runs off the UI thread; the last good listing is kept
//!   in memory and in `<data dir>/remote/<server id>.json`, so the section
//!   renders before the first reply and while a server is offline.
//! - Posters arrive as PNG bytes and become `gpui::RenderImage`s.
//! - Subtitles download to temporary files for `sub-add`; the OpenSubtitles
//!   hash comes from the server, which can read the file.
//! - The player source is the base URL plus `protocol::paths::playback`, with
//!   `protocol::bearer(token)` as the `Authorization` header.
//!
//! The token is never logged or put in a URL. `FLICK_REMOTE_TOKEN`
//! overrides every saved token for development and is never saved.
//!
//! Downloads on another machine (`docs/REMOTE_DOWNLOADS_PLAN.md`):
//! - [`RemoteState::destinations`] lists this Mac and every paired server
//!   with what its listing allows, for the Save to pickers.
//! - [`RemoteState::poll_transfers`] asks servers that accept downloads for
//!   `GET /v1/transfers` and `GET /v1/fetches`, once a second while
//!   Downloads is open and once a minute otherwise;
//!   [`RemoteState::machine_transfers`] returns the last rows of each.
//! - [`RemoteState::command`] and its wrappers send resolve, start, pause,
//!   remove, link, browse, and share commands; answers arrive in
//!   [`Ready::Command`]. A resolve can wait minutes for peers, so
//!   [`RemoteState::cancel_resolve`] drops its request, which closes the
//!   connection and ends the lookup on the server.

use crate::store::{Settings, remote::RemoteServer};
use flick::remote::protocol::{
    self, BrowseListing, Capability, ErrorBody, FetchPhase, FetchRow, Library, Pairing, Progress,
    Quality, ShareResult, TorrentInfo, TransferPhase, TransferRow,
};
use std::{
    cell::{Cell, OnceCell, RefCell},
    collections::{HashMap, HashSet, VecDeque},
    fmt,
    path::{Path, PathBuf},
    sync::{Arc, OnceLock, mpsc},
    thread,
    time::{Duration, Instant},
};

/// Overrides every saved pairing token, like `FLICK_OPENSUBTITLES_KEY`.
pub const TOKEN_ENV: &str = "FLICK_REMOTE_TOKEN";

/// Listings, hashes, posters, and progress uploads give up after this long.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// Embedded subtitles are extracted on demand, which can take longer.
const SUBTITLE_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_LISTING_BYTES: usize = 64 * 1024 * 1024;
const MAX_POSTER_BYTES: usize = 8 * 1024 * 1024;
const MAX_SUBTITLE_BYTES: usize = 16 * 1024 * 1024;
/// An HLS playlist: about 30 bytes per 4-second segment.
const MAX_PLAYLIST_BYTES: usize = 4 * 1024 * 1024;
/// Decoded posters kept in memory.
const POSTER_CACHE_CAPACITY: usize = 128;
/// Poster downloads in flight at once.
const POSTER_CONCURRENCY: usize = 4;
/// Older poster requests are dropped beyond this; they are asked for again
/// if they are still on screen.
const POSTER_QUEUE_LIMIT: usize = 64;

/// How a paired server is doing, from its last reply.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Status {
    /// A request is in flight, or none has been made yet.
    #[default]
    Connecting,
    Online,
    /// Did not answer: asleep, off the tailnet, or the server is not running.
    Offline,
    /// 401 or 403: the server's token changed or this one was mistyped.
    WrongToken,
    /// Answered, but not with a Flick listing.
    NotFlick,
    /// The listing's protocol major is newer than [`protocol::VERSION`].
    TooNew(u32),
    Failed(String),
}

impl Status {
    pub fn label(&self) -> String {
        match self {
            Status::Connecting => "Connecting…".into(),
            Status::Online => "Online".into(),
            Status::Offline => "Offline".into(),
            Status::WrongToken => "Wrong token. Pair again".into(),
            Status::NotFlick => "Not a Flick server".into(),
            Status::TooNew(version) => {
                format!("Server is newer than this Flick (protocol {version})")
            }
            Status::Failed(message) => message.clone(),
        }
    }

    pub fn is_online(&self) -> bool {
        *self == Status::Online
    }
}

/// A paired server as the library and settings render it.
#[derive(Clone, Debug)]
pub struct ServerView {
    pub id: String,
    pub name: String,
    /// `host:port`, with IPv6 hosts in brackets.
    pub address: String,
    pub status: Status,
    /// The last good listing, possibly from the disk cache.
    pub listing: Option<Arc<Library>>,
    /// The server's default tier.
    pub quality: Quality,
}

impl ServerView {
    /// One line for a settings row or an offline row: status and video count.
    pub fn summary(&self) -> String {
        let count = self.listing.as_ref().map(|listing| listing.videos.len());
        let videos = |count: usize| {
            if count == 1 {
                "1 video".to_owned()
            } else {
                format!("{count} videos")
            }
        };
        match (&self.status, count) {
            (Status::Online, Some(count)) => format!("Online · {}", videos(count)),
            (status, Some(count)) if count > 0 => {
                format!("{} · showing {} saved", status.label(), videos(count))
            }
            (status, _) => status.label(),
        }
    }

    /// The tiers this server offers, for a quality picker. Before the first
    /// listing every tier is offered.
    pub fn qualities(&self) -> Vec<Quality> {
        offered_qualities(self.listing.as_deref())
    }
}

/// What the player needs to load a remote video. The player's
/// `RemoteSource` has the same fields.
#[derive(Clone, PartialEq)]
pub struct RemotePlayback {
    pub server_id: String,
    pub server_name: String,
    pub video_id: String,
    pub title: String,
    /// Seconds, or 0 when unknown.
    pub duration: f64,
    pub quality: Quality,
    /// `http://host:port`.
    pub base_url: String,
    pub token: String,
}

impl RemotePlayback {
    /// The URL libmpv loads: the file for Original, an HLS playlist otherwise.
    pub fn url(&self) -> String {
        format!(
            "{}{}",
            self.base_url,
            protocol::paths::playback(&self.video_id, self.quality)
        )
    }

    /// The `Authorization` header value for `http-header-fields`.
    pub fn authorization(&self) -> String {
        protocol::bearer(&self.token)
    }
}

impl fmt::Debug for RemotePlayback {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RemotePlayback")
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

/// A subtitle on the server: a file next to the video, or a stream inside it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SubtitleRef {
    Sidecar(u32),
    Embedded(u32),
}

/// A finished subtitle download or hash request, from [`RemoteState::take_ready`].
#[derive(Clone, Debug)]
pub enum Ready {
    Subtitle {
        server_id: String,
        video_id: String,
        which: SubtitleRef,
        /// A temporary file for `sub-add`, removed when Flick closes.
        result: Result<PathBuf, String>,
    },
    Hash {
        server_id: String,
        video_id: String,
        /// The OpenSubtitles movie hash, 16 hex digits.
        result: Result<String, String>,
    },
    /// The answer to [`RemoteState::check_playback`].
    Playback {
        /// The number `check_playback` returned.
        check: u64,
        server_id: String,
        video_id: String,
        quality: Quality,
        /// Why the server will not play it, in words for the user.
        result: Result<(), String>,
    },
    /// The answer to a [`RemoteState::command`]. Errors are the server's
    /// message, or a sentence for a server that did not answer.
    Command {
        server_id: String,
        command: Command,
        result: Result<Outcome, String>,
    },
}

/// Text fields of the Remote libraries settings rows. They are GPUI entities,
/// so the settings page creates them when a row needs one.
#[derive(Default)]
pub struct SettingsFields {
    /// The pairing string being pasted, while the Add row is open.
    pub pairing: Option<gpui::Entity<crate::input::TextInput>>,
    /// Why the last pairing string was refused.
    pub pairing_error: Option<String>,
    /// The server being renamed and its name field.
    pub renaming: Option<(String, gpui::Entity<crate::input::TextInput>)>,
}

/// Everything the view keeps for remote libraries.
///
/// Mutable state sits behind a `RefCell` so render code holding `&Flick`
/// can read listings and request posters.
#[derive(Default)]
pub struct RemoteState {
    pub fields: SettingsFields,
    worker: OnceCell<Worker>,
    /// Set by [`Self::shutdown`]: no worker starts after it.
    shut_down: Cell<bool>,
    /// The last number [`Self::check_playback`] handed out.
    checks: Cell<u64>,
    inner: RefCell<Inner>,
    /// The last Save to choice per sheet: a server id, or None for this Mac.
    choices: RefCell<HashMap<Sheet, Option<String>>>,
    /// This Mac's download folder and its free space, read at most every
    /// [`FREE_SPACE_TTL`].
    local: RefCell<LocalDisk>,
}

#[derive(Default)]
struct LocalDisk {
    folder: Option<PathBuf>,
    free: Option<u64>,
    read: Option<Instant>,
}

impl RemoteState {
    /// A state that keeps its cache in `dir` instead of the Flick data dir.
    #[cfg_attr(not(test), allow(dead_code))] // Tests use a temporary folder.
    pub fn with_data_dir(dir: impl Into<PathBuf>) -> Self {
        let state = Self::default();
        state.inner.borrow_mut().data_dir = Some(dir.into());
        state
    }

    /// Applies finished requests. True when something visible changed.
    pub fn drain(&mut self) -> bool {
        let Some(worker) = self.worker.get() else {
            return false;
        };
        let inner = self.inner.get_mut();
        let mut changed = false;
        while let Ok(reply) = worker.replies.try_recv() {
            inner.apply(reply);
            changed = true;
        }
        if changed {
            inner.posters.pump(worker);
            // A download that finished on a server joins its listing.
            let cache = cache_dir(inner.data_dir());
            for id in std::mem::take(&mut inner.relist) {
                let request = inner
                    .servers
                    .get_mut(&id)
                    .and_then(|remote| remote.begin_refresh(&id, &cache));
                if let Some(request) = request
                    && let Err(request) = worker.send(request)
                {
                    inner.apply((*request).unavailable());
                }
            }
        }
        changed
    }

    /// Paired servers with their status and last listing. The first call
    /// loads the disk cache, and each server is refreshed once per session
    /// without being asked.
    pub fn servers(&self, settings: &Settings) -> Vec<ServerView> {
        let mut inner = self.inner.borrow_mut();
        inner.load_cache_once();
        let cache = cache_dir(inner.data_dir());
        let mut views = Vec::with_capacity(settings.remote.servers.len());
        for server in &settings.remote.servers {
            let remote = inner.sync(server);
            if !remote.requested {
                let request = remote.begin_refresh(&server.id, &cache);
                self.send(&mut inner, request);
            }
            let remote = &inner.servers[&server.id];
            views.push(ServerView {
                id: server.id.clone(),
                name: server.name.clone(),
                address: address(&server.host, server.port),
                status: if remote.refreshing {
                    Status::Connecting
                } else {
                    remote.status.clone()
                },
                listing: remote
                    .listing
                    .as_ref()
                    .map(|listing| listing.library.clone()),
                quality: server.quality,
            });
        }
        views
    }

    /// Adds the server in a pairing string, or updates the token of the one
    /// already paired at that address, and refreshes it. Returns its id.
    pub fn add_pairing(&mut self, settings: &mut Settings, text: &str) -> Result<String, String> {
        let pairing = Pairing::parse(text).map_err(|error| error.to_string())?;
        let id = upsert_pairing(&mut settings.remote.servers, &pairing, new_server_id);
        self.refresh_server(settings, &id);
        Ok(id)
    }

    /// Forgets a server, its listing, its cache file, and its progress.
    pub fn remove_server(&mut self, settings: &mut Settings, id: &str) {
        settings.remote.servers.retain(|server| server.id != id);
        settings.remote.progress.retain(|entry| entry.server != id);
        let inner = self.inner.get_mut();
        inner.servers.remove(id);
        inner.machines.remove(id);
        inner.resolves.remove(id);
        let dropped = inner.posters.remove_server(id);
        inner.dropped.extend(dropped);
        if let Some(fields) = &self.fields.renaming
            && fields.0 == id
        {
            self.fields.renaming = None;
        }
        let dir = cache_dir(inner.data_dir());
        if let Err(error) = remove_cache(&dir, id) {
            tracing::debug!(%error, "Could not remove a remote listing cache");
        }
    }

    pub fn set_quality(&mut self, settings: &mut Settings, id: &str, quality: Quality) {
        if let Some(server) = find_mut(settings, id) {
            server.quality = quality;
        }
    }

    /// Renames a server; an empty name goes back to the host's first label.
    pub fn rename(&mut self, settings: &mut Settings, id: &str, name: &str) {
        if let Some(server) = find_mut(settings, id) {
            let name = name.trim();
            server.name = if name.is_empty() {
                default_name(&server.host)
            } else {
                name.to_owned()
            };
        }
    }

    /// Refreshes every paired server.
    pub fn refresh(&mut self, settings: &Settings) {
        for server in &settings.remote.servers {
            self.refresh_server(settings, &server.id);
        }
    }

    pub fn refresh_server(&mut self, settings: &Settings, id: &str) {
        let Some(server) = settings.remote.servers.iter().find(|s| s.id == id) else {
            return;
        };
        let mut inner = self.inner.borrow_mut();
        inner.load_cache_once();
        inner.posters.failed.retain(|(server, _)| server != id);
        let cache = cache_dir(inner.data_dir());
        let request = inner.sync(server).begin_refresh(&server.id, &cache);
        self.send(&mut inner, request);
    }

    /// The decoded poster, or None while it downloads, when the listing
    /// says there is none, or when it failed this session.
    pub fn poster(&self, server_id: &str, video_id: &str) -> Option<Arc<gpui::RenderImage>> {
        let mut inner = self.inner.borrow_mut();
        let inner = &mut *inner;
        if let Some(image) = inner.posters.get(server_id, video_id) {
            return Some(image);
        }
        let remote = inner.servers.get(server_id)?;
        let endpoint = remote.endpoint.clone()?;
        let video = remote.listing.as_ref()?.video(video_id)?;
        if !video.poster {
            return None;
        }
        let key = (server_id.to_owned(), video_id.to_owned());
        if inner.posters.pending.contains(&key) || inner.posters.failed.contains(&key) {
            return None;
        }
        inner.posters.enqueue(key, endpoint);
        if let Some(worker) = self.worker() {
            inner.posters.pump(worker);
        }
        None
    }

    /// Images evicted from the poster cache or belonging to removed servers.
    /// Pass each to `window.drop_image` so GPUI frees its texture.
    pub fn take_dropped_images(&mut self) -> Vec<Arc<gpui::RenderImage>> {
        std::mem::take(&mut self.inner.get_mut().dropped)
    }

    /// What the player needs to play a video at the server's default tier,
    /// or Original when the server cannot transcode.
    pub fn playback(
        &self,
        settings: &Settings,
        server_id: &str,
        video_id: &str,
    ) -> Option<RemotePlayback> {
        let server = settings.remote.servers.iter().find(|s| s.id == server_id)?;
        let inner = self.inner.borrow();
        let listing = inner.servers.get(server_id)?.listing.as_ref()?;
        let video = listing.video(video_id)?;
        Some(RemotePlayback {
            server_id: server.id.clone(),
            server_name: server.name.clone(),
            video_id: video.id.clone(),
            title: if video.title.trim().is_empty() {
                video.file_name.clone()
            } else {
                video.title.clone()
            },
            duration: video.duration,
            // Transcode tiers of an unfinished file are not offered.
            quality: if video.complete {
                playable_quality(server.quality, &listing.library)
            } else {
                Quality::Original
            },
            base_url: base_url(server),
            token: effective_token(&server.token),
        })
    }

    /// Downloads a subtitle to a temporary file; the path arrives in
    /// [`Ready::Subtitle`] from [`Self::take_ready`] after a [`Self::drain`].
    pub fn fetch_subtitle(&mut self, server_id: &str, video_id: &str, which: SubtitleRef) {
        let inner = self.inner.get_mut();
        let endpoint = inner
            .servers
            .get(server_id)
            .and_then(|remote| remote.endpoint.clone());
        let extension = match which {
            SubtitleRef::Embedded(_) => "vtt".to_owned(),
            SubtitleRef::Sidecar(index) => inner
                .servers
                .get(server_id)
                .and_then(|remote| remote.listing.as_ref()?.video(video_id))
                .and_then(|video| video.subtitles.iter().find(|s| s.index == index))
                .map_or_else(
                    || "srt".to_owned(),
                    |file| subtitle_extension(&file.file_name),
                ),
        };
        let Some(endpoint) = endpoint else {
            inner.ready.push(Ready::Subtitle {
                server_id: server_id.to_owned(),
                video_id: video_id.to_owned(),
                which,
                result: Err("This server is not paired".into()),
            });
            return;
        };
        let target = subtitle_dir(inner.data_dir()).join(format!(
            "{}-{}-{}.{extension}",
            safe_name(server_id),
            safe_name(video_id),
            match which {
                SubtitleRef::Sidecar(index) => format!("s{index}"),
                SubtitleRef::Embedded(track) => format!("e{track}"),
            }
        ));
        let request = Request::Subtitle {
            endpoint,
            server_id: server_id.to_owned(),
            video_id: video_id.to_owned(),
            which,
            target,
        };
        self.send(&mut self.inner.borrow_mut(), Some(request));
    }

    /// Asks the server for a video's OpenSubtitles hash; the answer arrives
    /// in [`Ready::Hash`].
    pub fn fetch_hash(&mut self, server_id: &str, video_id: &str) {
        let inner = self.inner.get_mut();
        let Some(endpoint) = inner
            .servers
            .get(server_id)
            .and_then(|remote| remote.endpoint.clone())
        else {
            inner.ready.push(Ready::Hash {
                server_id: server_id.to_owned(),
                video_id: video_id.to_owned(),
                result: Err("This server is not paired".into()),
            });
            return;
        };
        let request = Request::Hash {
            endpoint,
            server_id: server_id.to_owned(),
            video_id: video_id.to_owned(),
        };
        self.send(&mut self.inner.borrow_mut(), Some(request));
    }

    /// Asks the server whether it will play a video at a tier, by fetching
    /// what libmpv would load first: the HLS playlist for a transcode, or
    /// the file's headers for Original. libmpv reports a refused HTTP load
    /// only as a generic failure, so the server's reason (still downloading,
    /// no FFmpeg, unknown length) is shown from this answer instead. The
    /// answer arrives in [`Ready::Playback`] with the returned number.
    pub fn check_playback(&mut self, server_id: &str, video_id: &str, quality: Quality) -> u64 {
        let check = self.checks.get().wrapping_add(1);
        self.checks.set(check);
        let inner = self.inner.get_mut();
        let Some(endpoint) = inner
            .servers
            .get(server_id)
            .and_then(|remote| remote.endpoint.clone())
        else {
            inner.ready.push(Ready::Playback {
                check,
                server_id: server_id.to_owned(),
                video_id: video_id.to_owned(),
                quality,
                result: Err("This server is not paired".into()),
            });
            return check;
        };
        let request = Request::Playback {
            endpoint,
            check,
            server_id: server_id.to_owned(),
            video_id: video_id.to_owned(),
            quality,
        };
        self.send(&mut self.inner.borrow_mut(), Some(request));
        check
    }

    /// Finished subtitle, hash, and playback checks since the last call.
    pub fn take_ready(&mut self) -> Vec<Ready> {
        std::mem::take(&mut self.inner.get_mut().ready)
    }

    /// Sends playback progress to the server. Failures are ignored.
    pub fn put_progress(
        &mut self,
        settings: &Settings,
        server_id: &str,
        video_id: &str,
        progress: Progress,
    ) {
        let Some(server) = settings.remote.servers.iter().find(|s| s.id == server_id) else {
            return;
        };
        let request = Request::Progress {
            endpoint: Endpoint::of(server),
            video_id: video_id.to_owned(),
            progress,
        };
        self.send(&mut self.inner.borrow_mut(), Some(request));
    }

    /// Stops the worker without waiting for requests in flight and removes
    /// temporary subtitle files. Also runs on drop. Requests made afterwards
    /// fail at once instead of starting a new worker.
    pub fn shutdown(&mut self) {
        self.shut_down.set(true);
        // Dropping the request sender ends the worker's loop.
        if self.worker.take().is_some() {
            let dir = subtitle_dir(self.inner.get_mut().data_dir());
            if let Err(error) = std::fs::remove_dir_all(&dir)
                && error.kind() != std::io::ErrorKind::NotFound
            {
                tracing::debug!(%error, "Could not remove remote subtitle files");
            }
        }
    }

    /// The worker, started on first use. None after [`Self::shutdown`].
    fn worker(&self) -> Option<&Worker> {
        if self.shut_down.get() {
            return None;
        }
        Some(self.worker.get_or_init(Worker::start))
    }

    fn send(&self, inner: &mut Inner, request: Option<Request>) {
        let Some(request) = request else {
            return;
        };
        let refused = match self.worker() {
            Some(worker) => worker.send(request).err().map(|request| *request),
            None => Some(request),
        };
        if let Some(request) = refused {
            inner.apply(request.unavailable());
        }
    }
}

impl Drop for RemoteState {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn find_mut<'a>(settings: &'a mut Settings, id: &str) -> Option<&'a mut RemoteServer> {
    settings.remote.servers.iter_mut().find(|s| s.id == id)
}

#[derive(Default)]
struct Inner {
    data_dir: Option<PathBuf>,
    cache_loaded: bool,
    servers: HashMap<String, Remote>,
    posters: Posters,
    ready: Vec<Ready>,
    dropped: Vec<Arc<gpui::RenderImage>>,
    /// Transfers and link downloads on each server, by server id.
    machines: HashMap<String, Machine>,
    /// Servers whose listing is asked for again on the next drain, because
    /// a download there finished.
    relist: HashSet<String>,
    /// The resolve in flight on each server. Dropping its sender drops the
    /// request, which closes the connection.
    resolves: HashMap<String, tokio::sync::oneshot::Sender<()>>,
}

impl Inner {
    fn data_dir(&mut self) -> &Path {
        self.data_dir.get_or_insert_with(default_data_dir)
    }

    fn load_cache_once(&mut self) {
        if std::mem::replace(&mut self.cache_loaded, true) {
            return;
        }
        let dir = cache_dir(self.data_dir());
        for (id, library) in load_caches(&dir) {
            let remote = self.servers.entry(id).or_default();
            if remote.listing.is_none() {
                remote.listing = Some(Listing::new(library));
            }
        }
    }

    /// The runtime state for a saved server, with its address and token current.
    fn sync(&mut self, server: &RemoteServer) -> &mut Remote {
        let remote = self.servers.entry(server.id.clone()).or_default();
        let endpoint = Endpoint::of(server);
        if remote.endpoint.as_ref() != Some(&endpoint) {
            remote.endpoint = Some(endpoint);
        }
        remote
    }

    fn apply(&mut self, reply: Reply) {
        match reply {
            Reply::Library {
                server_id,
                generation,
                result,
            } => {
                let Some(remote) = self.servers.get_mut(&server_id) else {
                    return;
                };
                if remote.generation != generation {
                    return;
                }
                remote.refreshing = false;
                match result {
                    Ok(library) => {
                        remote.status = Status::Online;
                        remote.listing = Some(Listing::new(library));
                    }
                    Err(status) => remote.status = status,
                }
            }
            Reply::Poster {
                server_id,
                video_id,
                result,
            } => {
                let key = (server_id, video_id);
                self.posters.in_flight = self.posters.in_flight.saturating_sub(1);
                self.posters.pending.remove(&key);
                // A poster for a server removed meanwhile is discarded.
                if !self.servers.contains_key(&key.0) {
                    return;
                }
                match result {
                    Ok(image) => {
                        let image =
                            Arc::new(gpui::RenderImage::new(vec![image::Frame::new(image)]));
                        if let Some(evicted) = self.posters.insert(key, image) {
                            self.dropped.push(evicted);
                        }
                    }
                    Err(error) => {
                        tracing::debug!(%error, "Could not load a remote poster");
                        self.posters.failed.insert(key);
                    }
                }
            }
            Reply::Ready(ready) => self.ready.push(ready),
            Reply::Transfers { server_id, result } => {
                let machine = self.machines.entry(server_id.clone()).or_default();
                machine.polling_transfers = false;
                match result {
                    Ok(rows) => {
                        if machine.transfers_loaded && transfers_finished(&machine.transfers, &rows)
                        {
                            self.relist.insert(server_id);
                        }
                        machine.transfers = rows;
                        machine.transfers_loaded = true;
                        machine.transfers_problem = None;
                    }
                    Err(message) => machine.transfers_problem = Some(message),
                }
            }
            Reply::Fetches { server_id, result } => {
                let machine = self.machines.entry(server_id.clone()).or_default();
                machine.polling_fetches = false;
                match result {
                    Ok(rows) => {
                        if machine.fetches_loaded && fetches_finished(&machine.fetches, &rows) {
                            self.relist.insert(server_id);
                        }
                        machine.fetches = rows;
                        machine.fetches_loaded = true;
                        machine.fetches_problem = None;
                    }
                    Err(message) => machine.fetches_problem = Some(message),
                }
            }
            Reply::Command {
                server_id,
                command,
                result,
            } => {
                // Whatever the answer, the rows may have changed.
                if !matches!(command, Command::Browse { .. } | Command::Resolve { .. }) {
                    let machine = self.machines.entry(server_id.clone()).or_default();
                    machine.last_transfers = None;
                    machine.last_fetches = None;
                }
                self.ready.push(Ready::Command {
                    server_id,
                    command,
                    result,
                });
            }
            Reply::None => {}
        }
    }
}

/// Runtime state of one paired server.
#[derive(Default)]
struct Remote {
    endpoint: Option<Endpoint>,
    listing: Option<Listing>,
    status: Status,
    /// Replies from older refreshes are ignored.
    generation: u64,
    refreshing: bool,
    /// A refresh was made this session.
    requested: bool,
}

impl Remote {
    fn begin_refresh(&mut self, server_id: &str, cache: &Path) -> Option<Request> {
        let endpoint = self.endpoint.clone()?;
        self.requested = true;
        self.refreshing = true;
        self.generation += 1;
        Some(Request::Library {
            endpoint,
            server_id: server_id.to_owned(),
            generation: self.generation,
            cache: cache.to_owned(),
        })
    }
}

/// A listing and an index from video id to position.
struct Listing {
    library: Arc<Library>,
    index: HashMap<String, usize>,
}

impl Listing {
    fn new(library: Library) -> Self {
        let index = library
            .videos
            .iter()
            .enumerate()
            .map(|(position, video)| (video.id.clone(), position))
            .collect();
        Self {
            library: Arc::new(library),
            index,
        }
    }

    fn video(&self, id: &str) -> Option<&protocol::Video> {
        self.library.videos.get(*self.index.get(id)?)
    }
}

type PosterKey = (String, String);

/// Decoded posters with least-recently-used eviction, and the download queue.
#[derive(Default)]
struct Posters {
    /// By server id, then video id, with the clock value of the last use.
    images: HashMap<String, HashMap<String, (Arc<gpui::RenderImage>, u64)>>,
    count: usize,
    clock: u64,
    queue: VecDeque<(PosterKey, Endpoint)>,
    pending: HashSet<PosterKey>,
    failed: HashSet<PosterKey>,
    in_flight: usize,
}

impl Posters {
    fn get(&mut self, server_id: &str, video_id: &str) -> Option<Arc<gpui::RenderImage>> {
        let (image, used) = self.images.get_mut(server_id)?.get_mut(video_id)?;
        self.clock += 1;
        *used = self.clock;
        Some(image.clone())
    }

    /// Inserts a poster and returns the one evicted to make room, if any.
    fn insert(
        &mut self,
        (server_id, video_id): PosterKey,
        image: Arc<gpui::RenderImage>,
    ) -> Option<Arc<gpui::RenderImage>> {
        self.clock += 1;
        let entry = (image, self.clock);
        if let Some(old) = self
            .images
            .get_mut(&server_id)
            .and_then(|videos| videos.get_mut(&video_id))
        {
            return Some(std::mem::replace(old, entry).0);
        }
        let mut evicted = None;
        if self.count >= POSTER_CACHE_CAPACITY {
            let oldest = self
                .images
                .iter()
                .flat_map(|(server, videos)| {
                    videos
                        .iter()
                        .map(move |(video, (_, used))| (*used, server, video))
                })
                .min()
                .map(|(_, server, video)| (server.clone(), video.clone()));
            if let Some((server, video)) = oldest
                && let Some(videos) = self.images.get_mut(&server)
            {
                evicted = videos.remove(&video).map(|e| e.0);
                if videos.is_empty() {
                    self.images.remove(&server);
                }
                self.count -= 1;
            }
        }
        self.images
            .entry(server_id)
            .or_default()
            .insert(video_id, entry);
        self.count += 1;
        evicted
    }

    fn enqueue(&mut self, key: PosterKey, endpoint: Endpoint) {
        self.pending.insert(key.clone());
        self.queue.push_back((key, endpoint));
        while self.queue.len() > POSTER_QUEUE_LIMIT {
            if let Some((key, _)) = self.queue.pop_front() {
                self.pending.remove(&key);
            }
        }
    }

    /// Starts queued downloads, newest first, up to the concurrency limit.
    fn pump(&mut self, worker: &Worker) {
        while self.in_flight < POSTER_CONCURRENCY {
            let Some(((server_id, video_id), endpoint)) = self.queue.pop_back() else {
                break;
            };
            let request = Request::Poster {
                endpoint,
                server_id: server_id.clone(),
                video_id: video_id.clone(),
            };
            if worker.send(request).is_err() {
                self.pending.remove(&(server_id, video_id));
                continue;
            }
            self.in_flight += 1;
        }
    }

    fn remove_server(&mut self, id: &str) -> Vec<Arc<gpui::RenderImage>> {
        self.queue.retain(|((server, _), _)| server != id);
        self.pending.retain(|(server, _)| server != id);
        self.failed.retain(|(server, _)| server != id);
        let removed: Vec<_> = self
            .images
            .remove(id)
            .into_iter()
            .flat_map(|videos| videos.into_values().map(|e| e.0))
            .collect();
        self.count -= removed.len();
        removed
    }
}

/// Where requests for a server go. Debug output never shows the token.
#[derive(Clone, PartialEq, Eq)]
struct Endpoint {
    base: String,
    token: String,
}

impl Endpoint {
    fn of(server: &RemoteServer) -> Self {
        Self {
            base: base_url(server),
            token: effective_token(&server.token),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }
}

impl fmt::Debug for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Endpoint")
            .field("base", &self.base)
            .finish_non_exhaustive()
    }
}

enum Request {
    Library {
        endpoint: Endpoint,
        server_id: String,
        generation: u64,
        /// Where a good listing is saved.
        cache: PathBuf,
    },
    Poster {
        endpoint: Endpoint,
        server_id: String,
        video_id: String,
    },
    Subtitle {
        endpoint: Endpoint,
        server_id: String,
        video_id: String,
        which: SubtitleRef,
        target: PathBuf,
    },
    Hash {
        endpoint: Endpoint,
        server_id: String,
        video_id: String,
    },
    Progress {
        endpoint: Endpoint,
        video_id: String,
        progress: Progress,
    },
    Playback {
        endpoint: Endpoint,
        check: u64,
        server_id: String,
        video_id: String,
        quality: Quality,
    },
    /// `GET /v1/transfers`, or `GET /v1/fetches` when `fetches` is set.
    Poll {
        endpoint: Endpoint,
        server_id: String,
        fetches: bool,
    },
    Command {
        endpoint: Endpoint,
        server_id: String,
        command: Command,
        /// Ends the request when it fires or its sender is dropped; the
        /// answer is then discarded.
        cancel: Option<tokio::sync::oneshot::Receiver<()>>,
    },
}

impl Request {
    /// The reply when the worker is not running.
    fn unavailable(self) -> Reply {
        let error = "Remote libraries are unavailable. Restart Flick to try again".to_owned();
        match self {
            Request::Library {
                server_id,
                generation,
                ..
            } => Reply::Library {
                server_id,
                generation,
                result: Err(Status::Failed(error)),
            },
            Request::Poster {
                server_id,
                video_id,
                ..
            } => Reply::Poster {
                server_id,
                video_id,
                result: Err(error),
            },
            Request::Subtitle {
                server_id,
                video_id,
                which,
                ..
            } => Reply::Ready(Ready::Subtitle {
                server_id,
                video_id,
                which,
                result: Err(error),
            }),
            Request::Hash {
                server_id,
                video_id,
                ..
            } => Reply::Ready(Ready::Hash {
                server_id,
                video_id,
                result: Err(error),
            }),
            Request::Progress { .. } => Reply::None,
            Request::Playback {
                check,
                server_id,
                video_id,
                quality,
                ..
            } => Reply::Ready(Ready::Playback {
                check,
                server_id,
                video_id,
                quality,
                result: Err(error),
            }),
            Request::Poll {
                server_id,
                fetches: false,
                ..
            } => Reply::Transfers {
                server_id,
                result: Err(error),
            },
            Request::Poll {
                server_id,
                fetches: true,
                ..
            } => Reply::Fetches {
                server_id,
                result: Err(error),
            },
            Request::Command {
                server_id, command, ..
            } => Reply::Command {
                server_id,
                command,
                result: Err(error),
            },
        }
    }
}

enum Reply {
    Library {
        server_id: String,
        generation: u64,
        result: Result<Library, Status>,
    },
    Poster {
        server_id: String,
        video_id: String,
        /// BGRA pixels, as GPUI expects.
        result: Result<image::RgbaImage, String>,
    },
    Ready(Ready),
    Transfers {
        server_id: String,
        result: Result<Vec<TransferRow>, String>,
    },
    Fetches {
        server_id: String,
        result: Result<Vec<FetchRow>, String>,
    },
    Command {
        server_id: String,
        command: Command,
        result: Result<Outcome, String>,
    },
    None,
}

/// The request thread: its own Tokio runtime, started on first use.
struct Worker {
    requests: tokio::sync::mpsc::UnboundedSender<Request>,
    replies: mpsc::Receiver<Reply>,
}

impl Worker {
    fn start() -> Self {
        let (requests, mut receiver) = tokio::sync::mpsc::unbounded_channel::<Request>();
        let (sender, replies) = mpsc::channel();
        let spawned = thread::Builder::new()
            .name("flick-remote".into())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        tracing::warn!(%error, "Remote libraries could not start");
                        return;
                    }
                };
                runtime.block_on(async move {
                    let http = match http_client() {
                        Ok(http) => http,
                        Err(error) => {
                            tracing::warn!(%error, "Remote libraries could not start");
                            return;
                        }
                    };
                    while let Some(request) = receiver.recv().await {
                        let http = http.clone();
                        let sender = sender.clone();
                        tokio::spawn(async move {
                            let reply = handle(&http, request).await;
                            let _ = sender.send(reply);
                        });
                    }
                });
                runtime.shutdown_timeout(Duration::from_millis(500));
            });
        if let Err(error) = spawned {
            tracing::warn!(%error, "Remote libraries could not start");
        }
        Self { requests, replies }
    }

    /// Hands the request back when the worker has stopped.
    fn send(&self, request: Request) -> Result<(), Box<Request>> {
        self.requests
            .send(request)
            .map_err(|error| Box::new(error.0))
    }
}

fn http_client() -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        // A system proxy cannot reach tailnet addresses and must not see the token.
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(REQUEST_TIMEOUT)
        .user_agent(concat!("Flick v", env!("CARGO_PKG_VERSION")))
        .build()
}

async fn handle(http: &reqwest::Client, request: Request) -> Reply {
    match request {
        Request::Library {
            endpoint,
            server_id,
            generation,
            cache,
        } => {
            let result = match get(
                http,
                &endpoint,
                &protocol::paths::library(),
                MAX_LISTING_BYTES,
                REQUEST_TIMEOUT,
            )
            .await
            {
                Ok(body) => parse_listing(&body),
                Err(status) => Err(status),
            };
            if let Ok(library) = &result {
                let library = library.clone();
                let id = server_id.clone();
                let saved =
                    tokio::task::spawn_blocking(move || save_cache(&cache, &id, &library)).await;
                if let Ok(Err(error)) = saved {
                    tracing::debug!(%error, "Could not cache a remote listing");
                }
            }
            Reply::Library {
                server_id,
                generation,
                result,
            }
        }
        Request::Poster {
            endpoint,
            server_id,
            video_id,
        } => {
            let result = match get(
                http,
                &endpoint,
                &protocol::paths::poster(&video_id),
                MAX_POSTER_BYTES,
                REQUEST_TIMEOUT,
            )
            .await
            {
                Ok(body) => tokio::task::spawn_blocking(move || decode_poster(&body))
                    .await
                    .unwrap_or_else(|_| Err("The poster could not be decoded".into())),
                Err(status) => Err(request_error(&status)),
            };
            Reply::Poster {
                server_id,
                video_id,
                result,
            }
        }
        Request::Subtitle {
            endpoint,
            server_id,
            video_id,
            which,
            target,
        } => {
            let path = match which {
                SubtitleRef::Sidecar(index) => protocol::paths::subtitle(&video_id, index),
                SubtitleRef::Embedded(track) => {
                    protocol::paths::embedded_subtitle(&video_id, track)
                }
            };
            let result =
                match get(http, &endpoint, &path, MAX_SUBTITLE_BYTES, SUBTITLE_TIMEOUT).await {
                    Ok(body) => tokio::task::spawn_blocking(move || {
                        write_atomically(&target, &body)
                            .map(|_| target)
                            .map_err(|_| "Could not save the subtitle".to_owned())
                    })
                    .await
                    .unwrap_or_else(|_| Err("Could not save the subtitle".into())),
                    Err(status) => Err(request_error(&status)),
                };
            Reply::Ready(Ready::Subtitle {
                server_id,
                video_id,
                which,
                result,
            })
        }
        Request::Hash {
            endpoint,
            server_id,
            video_id,
        } => {
            let result = match get(
                http,
                &endpoint,
                &protocol::paths::hash(&video_id),
                4096,
                REQUEST_TIMEOUT,
            )
            .await
            {
                Ok(body) => parse_hash(&body).ok_or_else(|| "The server sent no hash".to_owned()),
                Err(status) => Err(request_error(&status)),
            };
            Reply::Ready(Ready::Hash {
                server_id,
                video_id,
                result,
            })
        }
        Request::Progress {
            endpoint,
            video_id,
            progress,
        } => {
            let sent = http
                .put(endpoint.url(&protocol::paths::progress(&video_id)))
                .header(
                    reqwest::header::AUTHORIZATION,
                    protocol::bearer(&endpoint.token),
                )
                .timeout(REQUEST_TIMEOUT)
                .json(&progress)
                .send()
                .await;
            match sent {
                Ok(response) if !response.status().is_success() => {
                    tracing::debug!(status = %response.status(), "Remote progress was refused");
                }
                Ok(_) => {}
                Err(_) => tracing::debug!("Could not send remote progress"),
            }
            Reply::None
        }
        Request::Playback {
            endpoint,
            check,
            server_id,
            video_id,
            quality,
        } => {
            let path = protocol::paths::playback(&video_id, quality);
            let result = if quality.is_transcode() {
                get(http, &endpoint, &path, MAX_PLAYLIST_BYTES, REQUEST_TIMEOUT)
                    .await
                    .map(drop)
            } else {
                head(http, &endpoint, &path).await
            };
            Reply::Ready(Ready::Playback {
                check,
                server_id,
                video_id,
                quality,
                result: result.map_err(|status| playback_error(&status)),
            })
        }
        Request::Poll {
            endpoint,
            server_id,
            fetches,
        } => {
            let path = if fetches {
                protocol::paths::fetches()
            } else {
                protocol::paths::transfers()
            };
            let body = call(http, &endpoint, Call::get(path, MAX_TRANSFERS_BYTES))
                .await
                .map_err(|failure| failure.message());
            if fetches {
                Reply::Fetches {
                    server_id,
                    result: body.and_then(|body| parse_rows(&body)),
                }
            } else {
                Reply::Transfers {
                    server_id,
                    result: body.and_then(|body| parse_rows(&body)),
                }
            }
        }
        Request::Command {
            endpoint,
            server_id,
            command,
            cancel,
        } => {
            let run = run_command(http, &endpoint, &command);
            let result = match cancel {
                // Dropping `run` drops the request and closes its connection.
                Some(cancel) => tokio::select! {
                    result = run => result,
                    _ = cancel => return Reply::None,
                },
                None => run.await,
            };
            Reply::Command {
                server_id,
                command,
                result,
            }
        }
    }
}

/// A HEAD with the bearer header; the file itself is never downloaded.
async fn head(http: &reqwest::Client, endpoint: &Endpoint, path: &str) -> Result<(), Status> {
    let response = http
        .head(endpoint.url(path))
        .header(
            reqwest::header::AUTHORIZATION,
            protocol::bearer(&endpoint.token),
        )
        .timeout(REQUEST_TIMEOUT)
        .send()
        .await
        .map_err(|_| Status::Offline)?;
    let code = response.status().as_u16();
    if (200..300).contains(&code) {
        Ok(())
    } else {
        // A HEAD answer has no body, so the generic message is used.
        Err(status_for_http(code, &[]))
    }
}

/// A GET with the bearer header and a size cap. Non-2xx answers become a
/// [`Status`] from [`status_for_http`].
async fn get(
    http: &reqwest::Client,
    endpoint: &Endpoint,
    path: &str,
    limit: usize,
    timeout: Duration,
) -> Result<Vec<u8>, Status> {
    let mut response = http
        .get(endpoint.url(path))
        .header(
            reqwest::header::AUTHORIZATION,
            protocol::bearer(&endpoint.token),
        )
        .timeout(timeout)
        .send()
        .await
        // Refused, unreachable, unresolved, and timed out all mean no answer.
        .map_err(|_| Status::Offline)?;
    let code = response.status().as_u16();
    let too_large = || Status::Failed("The server's reply is too large".into());
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(if (200..300).contains(&code) {
            too_large()
        } else {
            status_for_http(code, &[])
        });
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| Status::Offline)? {
        if body.len() + chunk.len() > limit {
            return Err(too_large());
        }
        body.extend_from_slice(&chunk);
    }
    if !(200..300).contains(&code) {
        return Err(status_for_http(code, &body));
    }
    Ok(body)
}

/// A 503 without a message: too many streams or transcodes are open.
const BUSY: &str = "The server is busy; try again in a moment";

/// The status a non-success HTTP answer means. For any 4xx or 5xx other
/// than 401, 403, and 404, the server's message is used when it sent one.
pub fn status_for_http(code: u16, body: &[u8]) -> Status {
    let message = || {
        serde_json::from_slice::<ErrorBody>(body)
            .ok()
            .map(|body| short(body.message.trim()))
            .filter(|message| !message.is_empty())
    };
    match code {
        200..=299 => Status::Online,
        401 | 403 => Status::WrongToken,
        404 => Status::NotFlick,
        400..=599 => Status::Failed(message().unwrap_or_else(|| match code {
            409 => "Still downloading on the server".into(),
            503 => BUSY.into(),
            500..=599 => format!("Server error ({code})"),
            _ => format!("Unexpected reply ({code})"),
        })),
        _ => Status::Failed(format!("Unexpected reply ({code})")),
    }
}

/// Why a video will not play, from a failed [`RemoteState::check_playback`].
fn playback_error(status: &Status) -> String {
    match status {
        Status::Offline => "The server did not answer".into(),
        Status::NotFlick => "The server no longer has this video".into(),
        status => status.label(),
    }
}

/// An error line for a poster, subtitle, or hash request.
fn request_error(status: &Status) -> String {
    match status {
        Status::NotFlick => "Not found on the server".into(),
        status => status.label(),
    }
}

fn short(text: &str) -> String {
    const LIMIT: usize = 120;
    if text.chars().count() <= LIMIT {
        text.to_owned()
    } else {
        let mut cut: String = text.chars().take(LIMIT - 1).collect();
        cut.push('…');
        cut
    }
}

/// A listing body, refused when it is not a Flick listing or its protocol
/// major is newer than this build understands.
pub fn parse_listing(body: &[u8]) -> Result<Library, Status> {
    let library: Library = serde_json::from_slice(body).map_err(|_| Status::NotFlick)?;
    check_protocol(library)
}

fn check_protocol(library: Library) -> Result<Library, Status> {
    match library.protocol {
        0 => Err(Status::NotFlick),
        version if version > protocol::VERSION => Err(Status::TooNew(version)),
        _ => Ok(library),
    }
}

/// The hash from `GET /v1/hash/{id}`: `{"hash": "…"}`, a JSON string, or
/// plain text, as 16 hex digits.
pub fn parse_hash(body: &[u8]) -> Option<String> {
    let value = match serde_json::from_slice::<serde_json::Value>(body) {
        Ok(serde_json::Value::Object(object)) => object.get("hash")?.as_str()?.to_owned(),
        Ok(serde_json::Value::String(text)) => text,
        _ => String::from_utf8_lossy(body).into_owned(),
    };
    let value = value.trim().to_ascii_lowercase();
    (value.len() == 16 && value.bytes().all(|b| b.is_ascii_hexdigit())).then_some(value)
}

/// PNG bytes as BGRA pixels, the order GPUI's images use.
fn decode_poster(bytes: &[u8]) -> Result<image::RgbaImage, String> {
    let mut image = image::load_from_memory_with_format(bytes, image::ImageFormat::Png)
        .map_err(|_| "The poster is not a PNG image".to_owned())?
        .into_rgba8();
    for pixel in image.as_chunks_mut::<4>().0 {
        pixel.swap(0, 2);
    }
    Ok(image)
}

/// Adds a paired server or, when one already uses the same host and port,
/// replaces its token. Returns the server's id.
pub fn upsert_pairing(
    servers: &mut Vec<RemoteServer>,
    pairing: &Pairing,
    new_id: impl FnOnce() -> String,
) -> String {
    if let Some(server) = servers
        .iter_mut()
        .find(|s| s.port == pairing.port && s.host.eq_ignore_ascii_case(&pairing.host))
    {
        server.token = pairing.token.clone();
        return server.id.clone();
    }
    let server = server_from_pairing(pairing, new_id());
    let id = server.id.clone();
    servers.push(server);
    id
}

pub fn server_from_pairing(pairing: &Pairing, id: String) -> RemoteServer {
    RemoteServer {
        id,
        name: default_name(&pairing.host),
        host: pairing.host.clone(),
        port: pairing.port,
        token: pairing.token.clone(),
        quality: Quality::default(),
    }
}

/// The host's first DNS label, such as `mohtashams-mac-mini` for a MagicDNS
/// name; an IP address is kept whole.
pub fn default_name(host: &str) -> String {
    if host.parse::<std::net::IpAddr>().is_ok() {
        return host.to_owned();
    }
    host.split('.')
        .next()
        .filter(|label| !label.is_empty())
        .unwrap_or(host)
        .to_owned()
}

/// A random local id: 32 hex digits from the operating system's hash seed,
/// the time, and a counter, without a random-number dependency.
pub fn new_server_id() -> String {
    use std::hash::{BuildHasher, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    let count = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut id = String::with_capacity(32);
    for half in 0..2u8 {
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        hasher.write_u128(now);
        hasher.write_u64(count);
        hasher.write_u8(half);
        id.push_str(&format!("{:016x}", hasher.finish()));
    }
    id
}

/// `http://host:port`, through the shared pairing type so it cannot drift.
pub fn base_url(server: &RemoteServer) -> String {
    Pairing {
        host: server.host.clone(),
        port: server.port,
        token: String::new(),
    }
    .base_url()
}

/// `host:port`, with IPv6 hosts in brackets.
pub fn address(host: &str, port: u16) -> String {
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

/// `FLICK_REMOTE_TOKEN` when set, else the saved token.
fn effective_token(saved: &str) -> String {
    static OVERRIDE: OnceLock<Option<String>> = OnceLock::new();
    choose_token(
        saved,
        OVERRIDE
            .get_or_init(|| std::env::var(TOKEN_ENV).ok())
            .as_deref(),
    )
}

pub fn choose_token(saved: &str, env: Option<&str>) -> String {
    env.map(str::trim)
        .filter(|token| !token.is_empty())
        .unwrap_or(saved)
        .to_owned()
}

/// The tiers to offer: the listing's, or all of them before a listing.
pub fn offered_qualities(listing: Option<&Library>) -> Vec<Quality> {
    match listing {
        Some(library) if !library.qualities.is_empty() => Quality::ALL
            .into_iter()
            .filter(|quality| library.qualities.contains(quality))
            .collect(),
        Some(_) => vec![Quality::Original],
        None => Quality::ALL.to_vec(),
    }
}

/// The next tier after `current` among `offered`, wrapping around.
pub fn next_quality(current: Quality, offered: &[Quality]) -> Quality {
    let Some(position) = offered.iter().position(|q| *q == current) else {
        return offered.first().copied().unwrap_or_default();
    };
    offered[(position + 1) % offered.len()]
}

/// The server's default tier when it offers it, else Original.
pub fn playable_quality(preferred: Quality, listing: &Library) -> Quality {
    if offered_qualities(Some(listing)).contains(&preferred) {
        preferred
    } else {
        Quality::Original
    }
}

/// The Flick data dir: `FLICK_DATA_DIR`, else the platform's local data dir.
pub fn default_data_dir() -> PathBuf {
    std::env::var_os("FLICK_DATA_DIR")
        .map(PathBuf::from)
        .or_else(|| {
            directories::ProjectDirs::from("app", "Flick", "Flick")
                .map(|d| d.data_local_dir().to_owned())
        })
        .unwrap_or_else(|| std::env::temp_dir().join("flick"))
}

pub fn cache_dir(data_dir: &Path) -> PathBuf {
    // Not `remote`: that folder is the in-app server's state, with its
    // token, salt, and progress file.
    data_dir.join("remote-listings")
}

fn subtitle_dir(data_dir: &Path) -> PathBuf {
    cache_dir(data_dir).join("subtitles")
}

/// Server ids become file names, so only plain ids are cached.
fn cache_path(dir: &Path, id: &str) -> Option<PathBuf> {
    (!id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'))
    .then(|| dir.join(format!("{id}.json")))
}

pub fn save_cache(dir: &Path, id: &str, library: &Library) -> std::io::Result<()> {
    let path = cache_path(dir, id).ok_or_else(|| std::io::Error::other("unsafe server id"))?;
    let json = serde_json::to_vec(library).map_err(std::io::Error::other)?;
    write_atomically(&path, &json)
}

/// Cached listings by server id. Unreadable files and listings from a newer
/// protocol are skipped.
pub fn load_caches(dir: &Path) -> Vec<(String, Library)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension()? != "json" {
                return None;
            }
            let id = path.file_stem()?.to_str()?.to_owned();
            cache_path(dir, &id)?;
            let library = serde_json::from_slice(&std::fs::read(&path).ok()?).ok()?;
            Some((id, check_protocol(library).ok()?))
        })
        .collect()
}

pub fn remove_cache(dir: &Path, id: &str) -> std::io::Result<()> {
    let Some(path) = cache_path(dir, id) else {
        return Ok(());
    };
    match std::fs::remove_file(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

/// Writes through a temporary file so a partial write never replaces a good one.
fn write_atomically(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("no parent folder"))?;
    std::fs::create_dir_all(parent)?;
    let mut temporary = path.as_os_str().to_owned();
    temporary.push(".tmp");
    let temporary = PathBuf::from(temporary);
    std::fs::write(&temporary, bytes)?;
    std::fs::rename(&temporary, path)
}

/// A file-name-safe form of an id.
fn safe_name(id: &str) -> String {
    id.chars()
        .take(64)
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// A sidecar's extension for the temporary file, so mpv picks the parser.
fn subtitle_extension(file_name: &str) -> String {
    Path::new(file_name)
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .filter(|e| !e.is_empty() && e.len() <= 5 && e.bytes().all(|b| b.is_ascii_alphanumeric()))
        .unwrap_or_else(|| "srt".into())
}

// Downloads on paired servers: Save to destinations, polled rows, commands.

/// Downloads polls each server this often while it is open, and otherwise.
const POLL_OPEN: Duration = Duration::from_secs(1);
const POLL_IDLE: Duration = Duration::from_secs(60);
/// The server waits up to five minutes for peers when resolving.
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(5 * 60 + 30);
/// Starting, pausing, removing, and link downloads.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(60);
/// A share hashes the whole folder before it answers.
const SHARE_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const MAX_TRANSFERS_BYTES: usize = 16 * 1024 * 1024;
const MAX_COMMAND_BYTES: usize = 8 * 1024 * 1024;
/// A `.torrent` file sent to a server to resolve.
const MAX_TORRENT_FILE_BYTES: u64 = 16 * 1024 * 1024;
/// How long this Mac's free space is reused before it is read again.
const FREE_SPACE_TTL: Duration = Duration::from_secs(10);

/// A place a download starts. Each remembers its own Save to choice.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Sheet {
    /// The torrent file-selection sheet.
    Torrent,
    /// The paste-link sheet.
    Link,
    /// The Share sheet, as Seed from.
    Share,
}

/// A machine a download can go to: this Mac, or a paired server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Destination {
    /// The paired server's id; None for this Mac.
    pub id: Option<String>,
    pub name: String,
    /// This Mac's download folder, or the server's advertised storage.
    pub free_bytes: Option<u64>,
    /// This Mac, or a server whose last listing request answered.
    pub online: bool,
    pub allows_downloads: bool,
    pub allows_links: bool,
    pub allows_shares: bool,
    /// The server's status from its last reply; Online for this Mac.
    pub status: Status,
}

impl Destination {
    pub fn is_local(&self) -> bool {
        self.id.is_none()
    }

    pub fn allows(&self, sheet: Sheet) -> bool {
        match sheet {
            Sheet::Torrent => self.allows_downloads,
            Sheet::Link => self.allows_links,
            Sheet::Share => self.allows_shares,
        }
    }

    /// Whether the picker lets this be chosen for `sheet`.
    pub fn selectable(&self, sheet: Sheet) -> bool {
        self.online && self.allows(sheet)
    }

    /// The line under its segment: free space, or why it cannot be chosen.
    pub fn detail(&self, sheet: Sheet) -> String {
        if !self.online {
            return match &self.status {
                Status::Connecting => "Connecting…".into(),
                Status::WrongToken => "Pair again".into(),
                Status::Offline => "Offline".into(),
                _ => "Unavailable".into(),
            };
        }
        if !self.allows(sheet) {
            return match sheet {
                Sheet::Torrent => "Not accepting downloads",
                Sheet::Link => "Not accepting links",
                Sheet::Share => "Not sharing folders",
            }
            .into();
        }
        self.free_bytes
            .map(|free| format!("{} free", flick::library::format_bytes(free)))
            .unwrap_or_default()
    }
}

/// What the picker calls this machine.
pub fn this_machine_name() -> &'static str {
    if cfg!(target_os = "macos") {
        "This Mac"
    } else if cfg!(windows) {
        "This PC"
    } else {
        "This Computer"
    }
}

/// A paired server as [`destinations_from`] needs it.
pub struct ServerFacts<'a> {
    pub id: &'a str,
    pub name: &'a str,
    pub listing: Option<&'a Library>,
    /// From the last reply, not "Connecting" during a refresh.
    pub status: &'a Status,
}

/// This Mac first, then every paired server, with what each allows from
/// its last listing (possibly the cached one).
pub fn destinations_from(local_free: Option<u64>, servers: &[ServerFacts<'_>]) -> Vec<Destination> {
    let mut destinations = vec![Destination {
        id: None,
        name: this_machine_name().into(),
        free_bytes: local_free,
        online: true,
        allows_downloads: true,
        allows_links: true,
        allows_shares: true,
        status: Status::Online,
    }];
    for server in servers {
        let allows = |capability| {
            server
                .listing
                .is_some_and(|listing| listing.capabilities.contains(&capability))
        };
        destinations.push(Destination {
            id: Some(server.id.to_owned()),
            name: server.name.to_owned(),
            free_bytes: server
                .listing
                .and_then(|listing| listing.storage.as_ref())
                .map(|storage| storage.free_bytes),
            online: server.status.is_online(),
            allows_downloads: allows(Capability::Downloads),
            allows_links: allows(Capability::Links),
            allows_shares: allows(Capability::Shares),
            status: server.status.clone(),
        });
    }
    destinations
}

/// The Save to choice for a sheet: its last choice, else the default from
/// onboarding (`settings.remote.default_destination`), else this Mac, each
/// only while it can be chosen.
pub fn choose_destination(
    destinations: &[Destination],
    sheet: Sheet,
    remembered: Option<&Option<String>>,
    default: &str,
) -> Option<String> {
    let usable = |id: &Option<String>| {
        destinations
            .iter()
            .any(|destination| &destination.id == id && destination.selectable(sheet))
    };
    if let Some(remembered) = remembered
        && usable(remembered)
    {
        return remembered.clone();
    }
    let default = (!default.is_empty()).then(|| default.to_owned());
    if default.is_some() && usable(&default) {
        return default;
    }
    None
}

/// Whether a picker is worth showing for a sheet: some server allows it.
pub fn offers_remote(destinations: &[Destination], sheet: Sheet) -> bool {
    destinations
        .iter()
        .any(|destination| !destination.is_local() && destination.allows(sheet))
}

/// A command for a paired server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    /// `input` is what [`wire_input`] makes.
    Resolve {
        input: String,
    },
    CancelResolve,
    Start {
        input: String,
        files: Vec<usize>,
    },
    Pause {
        id: String,
    },
    Resume {
        id: String,
    },
    PauseAll,
    ResumeAll,
    /// `confirm` comes from [`confirm_list`] when deleting files.
    Remove {
        id: String,
        delete_files: bool,
        confirm: Vec<String>,
    },
    FetchStart {
        url: String,
    },
    FetchCancel {
        id: u64,
    },
    Browse {
        dir: String,
    },
    Share {
        dir: String,
    },
}

/// A command's successful answer.
#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    Resolved(TorrentInfo),
    Started {
        id: String,
    },
    Done,
    /// The server refused a deletion because its file list is not the one
    /// confirmed; nothing was deleted. `current` is what it would delete now.
    ListChanged {
        message: String,
        current: Vec<String>,
    },
    FetchStarted {
        id: u64,
    },
    Browsed(BrowseListing),
    Shared(ShareResult),
}

/// One server's section in Downloads.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MachineTransfers {
    pub server_id: String,
    pub name: String,
    pub transfers: Vec<TransferRow>,
    pub fetches: Vec<FetchRow>,
    /// Every list this server offers has answered at least once.
    pub loaded: bool,
    /// One line when the last poll failed; the rows are the last known ones.
    pub problem: Option<String>,
    pub allows_links: bool,
    /// The server plays a torrent file before it has finished.
    pub stream_incomplete: bool,
}

/// Polled state of one server's transfers and link downloads.
#[derive(Default)]
struct Machine {
    transfers: Vec<TransferRow>,
    fetches: Vec<FetchRow>,
    transfers_loaded: bool,
    fetches_loaded: bool,
    transfers_problem: Option<String>,
    fetches_problem: Option<String>,
    polling_transfers: bool,
    polling_fetches: bool,
    /// When each poll last started; None asks at once.
    last_transfers: Option<Instant>,
    last_fetches: Option<Instant>,
}

/// Whether a list last asked for at `last` is due again.
pub fn poll_due(last: Option<Instant>, now: Instant, downloads_open: bool) -> bool {
    let interval = if downloads_open { POLL_OPEN } else { POLL_IDLE };
    last.is_none_or(|last| now.saturating_duration_since(last) >= interval)
}

/// Tiers the player's picker offers for a remote video: Original only
/// unless the video is known to be finished (`complete` from
/// [`RemoteState::video_complete`]), else the server's tiers, or every tier
/// when it has none.
pub fn picker_qualities(complete: Option<bool>, server: Option<Vec<Quality>>) -> Vec<Quality> {
    if complete != Some(true) {
        return vec![Quality::Original];
    }
    server.unwrap_or_else(|| Quality::ALL.to_vec())
}

/// Whether a remote open asks the server first ([`RemoteState::check_playback`])
/// instead of loading at once: a transcode tier, or a file not known to be
/// finished, so the server's reason (an unfinished file it cannot stream,
/// or 503 when too many streams are open) is shown instead of libmpv's
/// generic failure.
pub fn check_before_loading(quality: Quality, complete: Option<bool>) -> bool {
    quality.is_transcode() || complete != Some(true)
}

/// The paths "Remove and Delete Files" would delete, in the server's order:
/// what the confirmation lists and a deletion echoes in
/// `RemoveTransfer::confirm`. Those whose `will_delete` is set; a server
/// that sets none (one older than `will_delete`) means the selected files.
pub fn confirm_list(row: &TransferRow) -> Vec<String> {
    let marked = row.files.iter().any(|file| file.will_delete);
    row.files
        .iter()
        .filter(|file| {
            if marked {
                file.will_delete
            } else {
                file.selected
            }
        })
        .map(|file| file.path.clone())
        .collect()
}

fn file_complete(file: &protocol::TransferFileRow) -> bool {
    file.verified_bytes >= file.size
}

/// Whether a selected file finished, or a transfer started seeding, since
/// the previous poll: the server's listing has a new video.
pub fn transfers_finished(old: &[TransferRow], new: &[TransferRow]) -> bool {
    new.iter().any(|row| {
        let before = old.iter().find(|old| old.id == row.id);
        let file_done =
            row.files
                .iter()
                .filter(|file| file.selected && file_complete(file))
                .any(|file| {
                    !before.is_some_and(|before| {
                        before.files.iter().any(|old| {
                            old.index == file.index && old.selected && file_complete(old)
                        })
                    })
                });
        let seeding = row.phase == TransferPhase::Seeding
            && before.is_some_and(|before| {
                matches!(
                    before.phase,
                    TransferPhase::Downloading | TransferPhase::Checking
                )
            });
        file_done || seeding
    })
}

/// Whether a link download finished or saved another file since the
/// previous poll.
pub fn fetches_finished(old: &[FetchRow], new: &[FetchRow]) -> bool {
    new.iter().any(|row| {
        let before = old.iter().find(|old| old.id == row.id);
        let done = row.phase == FetchPhase::Done
            && !before.is_some_and(|before| before.phase == FetchPhase::Done);
        let saved = row.saved.len() > before.map_or(0, |before| before.saved.len());
        done || saved
    })
}

/// What a command's error answer means, in words for the user: the
/// server's message when it sent one (403 names the owner's switch), else
/// a sentence for the status. 401 means the token changed.
pub fn command_error(code: u16, body: &[u8]) -> String {
    if code == 401 {
        return Status::WrongToken.label();
    }
    server_message(body).unwrap_or_else(|| match code {
        403 => "This Flick's owner has not allowed that from other Flicks".into(),
        404 => "The server no longer has this. Refresh and try again".into(),
        409 => "The server already has this".into(),
        504 => "The server took too long to answer. Try again".into(),
        _ => status_for_http(code, body).label(),
    })
}

/// The `message` of an error body, when it has one.
fn server_message(body: &[u8]) -> Option<String> {
    serde_json::from_slice::<ErrorBody>(body)
        .ok()
        .map(|body| short(body.message.trim()))
        .filter(|message| !message.is_empty())
}

/// What a server is sent for a torrent: a magnet link or URL as it is, a
/// `.torrent` file as `base64:` and its bytes.
pub fn wire_input(input: &flick::torrent::TorrentInput) -> Result<String, String> {
    use flick::torrent::TorrentInput;
    match input {
        TorrentInput::Magnet(text) | TorrentInput::Url(text) => Ok(text.trim().to_owned()),
        TorrentInput::Bytes(bytes) => Ok(format!("base64:{}", base64(bytes))),
        TorrentInput::File(path) => {
            let too_large = || "This .torrent file is too large to send".to_owned();
            let size = std::fs::metadata(path)
                .map_err(|_| "Could not read this .torrent file".to_owned())?
                .len();
            if size > MAX_TORRENT_FILE_BYTES {
                return Err(too_large());
            }
            let bytes =
                std::fs::read(path).map_err(|_| "Could not read this .torrent file".to_owned())?;
            Ok(format!("base64:{}", base64(&bytes)))
        }
    }
}

/// Standard base64 with padding.
pub fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut text = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let value = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        for position in 0..4 {
            if position <= chunk.len() {
                text.push(ALPHABET[(value >> (18 - 6 * position) & 63) as usize] as char);
            } else {
                text.push('=');
            }
        }
    }
    text
}

/// A list body: a JSON array, or an object holding one array.
fn parse_rows<T: serde::de::DeserializeOwned>(body: &[u8]) -> Result<Vec<T>, String> {
    let unreadable = || "The server sent an unreadable list".to_owned();
    let value: serde_json::Value = serde_json::from_slice(body).map_err(|_| unreadable())?;
    let list = match value {
        serde_json::Value::Array(_) => value,
        serde_json::Value::Object(object) => object
            .into_iter()
            .map(|(_, value)| value)
            .find(serde_json::Value::is_array)
            .ok_or_else(unreadable)?,
        _ => return Err(unreadable()),
    };
    serde_json::from_value(list).map_err(|_| unreadable())
}

/// The id in `POST /v1/fetches`' answer: `{"id": 7}`, `{"id": "7"}`, or `7`.
fn parse_fetch_id(body: &[u8]) -> Option<u64> {
    let value: serde_json::Value = serde_json::from_slice(body).ok()?;
    let id = match &value {
        serde_json::Value::Object(object) => object.get("id")?,
        value => value,
    };
    id.as_u64().or_else(|| id.as_str()?.trim().parse().ok())
}

/// The file list a 409 to a deletion carries, under `confirm`, `files`,
/// `current`, or `paths`, as strings or objects with a `path`. Objects
/// count when `will_delete` is true, or, without it, unless `selected` is
/// false. None when the answer has no list: not a changed list, so the
/// user is not asked again.
pub fn parse_current_list(body: &[u8]) -> Option<Vec<String>> {
    let value: serde_json::Value = serde_json::from_slice(body).ok()?;
    let object = value.as_object()?;
    let list = ["confirm", "current", "files", "paths"]
        .iter()
        .find_map(|key| object.get(*key)?.as_array())?;
    list.iter()
        .map(|entry| match entry {
            serde_json::Value::String(path) => Some(path.clone()),
            serde_json::Value::Object(file) => {
                let flag = |key| file.get(key).and_then(serde_json::Value::as_bool);
                if flag("will_delete").or(flag("selected")) == Some(false) {
                    return Some(String::new());
                }
                file.get("path")?.as_str().map(str::to_owned)
            }
            _ => None,
        })
        .collect::<Option<Vec<_>>>()
        .map(|paths| paths.into_iter().filter(|p| !p.is_empty()).collect())
}

/// A failed request: no answer, too large, or an HTTP error with its body.
enum Failure {
    Offline,
    TooLarge,
    Http { code: u16, body: Vec<u8> },
}

impl Failure {
    fn message(&self) -> String {
        match self {
            Failure::Offline => "The server did not answer".into(),
            Failure::TooLarge => "The server's reply is too large".into(),
            Failure::Http { code, body } => command_error(*code, body),
        }
    }
}

/// One request for [`call`].
struct Call<'a> {
    method: reqwest::Method,
    path: String,
    query: &'a [(&'a str, &'a str)],
    body: Option<serde_json::Value>,
    limit: usize,
    timeout: Duration,
}

impl Call<'_> {
    fn get(path: String, limit: usize) -> Self {
        Call {
            method: reqwest::Method::GET,
            path,
            query: &[],
            body: None,
            limit,
            timeout: REQUEST_TIMEOUT,
        }
    }
}

/// A request with the bearer header, an optional JSON body, and a size cap.
async fn call(
    http: &reqwest::Client,
    endpoint: &Endpoint,
    Call {
        method,
        path,
        query,
        body,
        limit,
        timeout,
    }: Call<'_>,
) -> Result<Vec<u8>, Failure> {
    let mut url = reqwest::Url::parse(&endpoint.url(&path)).map_err(|_| Failure::Offline)?;
    if !query.is_empty() {
        url.query_pairs_mut().extend_pairs(query);
    }
    let mut request = http
        .request(method, url)
        .header(
            reqwest::header::AUTHORIZATION,
            protocol::bearer(&endpoint.token),
        )
        .timeout(timeout);
    if let Some(body) = body {
        request = request.json(&body);
    }
    let mut response = request.send().await.map_err(|_| Failure::Offline)?;
    let code = response.status().as_u16();
    let success = (200..300).contains(&code);
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(if success {
            Failure::TooLarge
        } else {
            Failure::Http {
                code,
                body: Vec::new(),
            }
        });
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| Failure::Offline)? {
        if bytes.len() + chunk.len() > limit {
            return Err(Failure::TooLarge);
        }
        bytes.extend_from_slice(&chunk);
    }
    if success {
        Ok(bytes)
    } else {
        Err(Failure::Http { code, body: bytes })
    }
}

async fn run_command(
    http: &reqwest::Client,
    endpoint: &Endpoint,
    command: &Command,
) -> Result<Outcome, String> {
    use protocol::paths;
    use reqwest::Method;
    let json = |value: &dyn erased::Json| Some(value.json());
    let (method, path, query, body, timeout) = match command {
        Command::Resolve { input } => (
            Method::POST,
            paths::transfers_resolve(),
            Vec::new(),
            json(&protocol::ResolveRequest {
                input: input.clone(),
            }),
            RESOLVE_TIMEOUT,
        ),
        Command::CancelResolve => (
            Method::DELETE,
            paths::transfers_resolve(),
            Vec::new(),
            None,
            REQUEST_TIMEOUT,
        ),
        Command::Start { input, files } => (
            Method::POST,
            paths::transfers(),
            Vec::new(),
            json(&protocol::StartTransfer {
                input: input.clone(),
                files: files.clone(),
            }),
            COMMAND_TIMEOUT,
        ),
        Command::Pause { id } => (
            Method::POST,
            paths::transfer_pause(id),
            Vec::new(),
            None,
            COMMAND_TIMEOUT,
        ),
        Command::Resume { id } => (
            Method::POST,
            paths::transfer_resume(id),
            Vec::new(),
            None,
            COMMAND_TIMEOUT,
        ),
        Command::PauseAll => (
            Method::POST,
            paths::transfers_pause_all(),
            Vec::new(),
            None,
            COMMAND_TIMEOUT,
        ),
        Command::ResumeAll => (
            Method::POST,
            paths::transfers_resume_all(),
            Vec::new(),
            None,
            COMMAND_TIMEOUT,
        ),
        Command::Remove {
            id,
            delete_files,
            confirm,
        } => (
            Method::DELETE,
            paths::transfer(id),
            Vec::new(),
            json(&protocol::RemoveTransfer {
                delete_files: *delete_files,
                confirm: if *delete_files {
                    confirm.clone()
                } else {
                    Vec::new()
                },
            }),
            COMMAND_TIMEOUT,
        ),
        Command::FetchStart { url } => (
            Method::POST,
            paths::fetches(),
            Vec::new(),
            json(&protocol::StartFetch { url: url.clone() }),
            COMMAND_TIMEOUT,
        ),
        Command::FetchCancel { id } => (
            Method::DELETE,
            paths::fetch(*id),
            Vec::new(),
            None,
            COMMAND_TIMEOUT,
        ),
        Command::Browse { dir } => (
            Method::GET,
            paths::browse(),
            vec![("dir", dir.as_str())],
            None,
            REQUEST_TIMEOUT,
        ),
        Command::Share { dir } => (
            Method::POST,
            paths::shares(),
            Vec::new(),
            json(&protocol::StartShare { dir: dir.clone() }),
            SHARE_TIMEOUT,
        ),
    };
    let answer = call(
        http,
        endpoint,
        Call {
            method,
            path,
            query: &query,
            body,
            limit: MAX_COMMAND_BYTES,
            timeout,
        },
    )
    .await;
    let unreadable = |what: &str| format!("The server sent an unreadable {what}");
    match (command, answer) {
        (
            Command::Remove {
                delete_files: true,
                ..
            },
            Err(Failure::Http { code: 409, body }),
        ) => match parse_current_list(&body) {
            // The list changed: ask again with the server's.
            Some(current) => Ok(Outcome::ListChanged {
                message: command_error(409, &body),
                current,
            }),
            // Any other refusal is said once and not asked again.
            None => Err(server_message(&body)
                .unwrap_or_else(|| "The server deleted nothing. Refresh and try again".into())),
        },
        // Keep Files on a transfer that is already gone did what was asked.
        (
            Command::Remove {
                delete_files: false,
                ..
            },
            Err(Failure::Http { code: 404, .. }),
        ) => Ok(Outcome::Done),
        (_, Err(failure)) => Err(failure.message()),
        (Command::Resolve { .. }, Ok(body)) => serde_json::from_slice(&body)
            .map(Outcome::Resolved)
            .map_err(|_| unreadable("file list")),
        // It may have started: say so rather than offering to start it again.
        (Command::Start { .. }, Ok(body)) => serde_json::from_slice::<protocol::Started>(&body)
            .ok()
            .map(|started| started.id.trim().to_owned())
            .filter(|id| !id.is_empty())
            .map(|id| Outcome::Started { id })
            .ok_or_else(|| {
                "The server sent an unreadable answer. Check its downloads before starting this again"
                    .to_owned()
            }),
        (Command::FetchStart { .. }, Ok(body)) => parse_fetch_id(&body)
            .map(|id| Outcome::FetchStarted { id })
            .ok_or_else(|| unreadable("answer")),
        (Command::Browse { .. }, Ok(body)) => serde_json::from_slice(&body)
            .map(Outcome::Browsed)
            .map_err(|_| unreadable("folder list")),
        (Command::Share { .. }, Ok(body)) => serde_json::from_slice(&body)
            .map(Outcome::Shared)
            .map_err(|_| unreadable("share link")),
        (_, Ok(_)) => Ok(Outcome::Done),
    }
}

/// Request bodies as JSON values, so one table can build every command.
mod erased {
    pub trait Json {
        fn json(&self) -> serde_json::Value;
    }

    impl<T: serde::Serialize> Json for T {
        fn json(&self) -> serde_json::Value {
            serde_json::to_value(self).unwrap_or(serde_json::Value::Null)
        }
    }
}

impl RemoteState {
    /// This Mac's download folder, for its free space in [`Self::destinations`].
    pub fn set_local_folder(&self, folder: &Path) {
        let mut local = self.local.borrow_mut();
        if local.folder.as_deref() != Some(folder) {
            local.folder = Some(folder.to_owned());
            local.read = None;
        }
    }

    fn local_free(&self) -> Option<u64> {
        let mut local = self.local.borrow_mut();
        let folder = local.folder.clone()?;
        if local
            .read
            .is_none_or(|read| read.elapsed() >= FREE_SPACE_TTL)
        {
            local.free = folder
                .ancestors()
                .find(|path| path.exists())
                .and_then(|path| fs2::available_space(path).ok());
            local.read = Some(Instant::now());
        }
        local.free
    }

    /// This Mac first, then every paired server with what its listing
    /// allows. Offline servers keep their last known capabilities but are
    /// not [`Destination::selectable`].
    pub fn destinations(&self, settings: &Settings) -> Vec<Destination> {
        let local_free = self.local_free();
        let mut inner = self.inner.borrow_mut();
        inner.load_cache_once();
        let cache = cache_dir(inner.data_dir());
        for server in &settings.remote.servers {
            let remote = inner.sync(server);
            if !remote.requested {
                let request = remote.begin_refresh(&server.id, &cache);
                self.send(&mut inner, request);
            }
        }
        let facts = settings
            .remote
            .servers
            .iter()
            .filter_map(|server| {
                let remote = inner.servers.get(&server.id)?;
                Some(ServerFacts {
                    id: &server.id,
                    name: &server.name,
                    listing: remote.listing.as_ref().map(|l| &*l.library),
                    status: &remote.status,
                })
            })
            .collect::<Vec<_>>();
        destinations_from(local_free, &facts)
    }

    /// The Save to choice a sheet opens with: a server id, or None for
    /// this Mac.
    pub fn destination(&self, settings: &Settings, sheet: Sheet) -> Option<String> {
        let destinations = self.destinations(settings);
        choose_destination(
            &destinations,
            sheet,
            self.choices.borrow().get(&sheet),
            &settings.remote.default_destination,
        )
    }

    /// Remembers a sheet's Save to choice for this session.
    pub fn remember_destination(&self, sheet: Sheet, id: Option<String>) {
        self.choices.borrow_mut().insert(sheet, id);
    }

    /// Asks servers that accept downloads for their transfers and link
    /// downloads when due: every second while Downloads is open, every
    /// minute otherwise. Call it on the view's tick.
    pub fn poll_transfers(&self, settings: &Settings, downloads_open: bool) {
        let now = Instant::now();
        let mut inner = self.inner.borrow_mut();
        for server in &settings.remote.servers {
            let remote = inner.sync(server);
            let Some(endpoint) = remote.endpoint.clone() else {
                continue;
            };
            let capabilities = remote
                .listing
                .as_ref()
                .map(|listing| listing.library.capabilities.clone())
                .unwrap_or_default();
            let mut requests = Vec::new();
            let machine = inner.machines.entry(server.id.clone()).or_default();
            if capabilities.contains(&Capability::Downloads)
                && !machine.polling_transfers
                && poll_due(machine.last_transfers, now, downloads_open)
            {
                machine.polling_transfers = true;
                machine.last_transfers = Some(now);
                requests.push(false);
            }
            if capabilities.contains(&Capability::Links)
                && !machine.polling_fetches
                && poll_due(machine.last_fetches, now, downloads_open)
            {
                machine.polling_fetches = true;
                machine.last_fetches = Some(now);
                requests.push(true);
            }
            for fetches in requests {
                let request = Request::Poll {
                    endpoint: endpoint.clone(),
                    server_id: server.id.clone(),
                    fetches,
                };
                self.send(&mut inner, Some(request));
            }
        }
    }

    /// One section per server that accepts downloads or links, or still
    /// has rows from before, in pairing order.
    pub fn machine_transfers(&self, settings: &Settings) -> Vec<MachineTransfers> {
        let inner = self.inner.borrow();
        settings
            .remote
            .servers
            .iter()
            .filter_map(|server| {
                let capabilities = inner
                    .servers
                    .get(&server.id)
                    .and_then(|remote| remote.listing.as_ref())
                    .map(|listing| listing.library.capabilities.as_slice())
                    .unwrap_or_default();
                let machine = inner.machines.get(&server.id);
                let downloads = capabilities.contains(&Capability::Downloads);
                let links = capabilities.contains(&Capability::Links);
                let has_rows =
                    machine.is_some_and(|m| !m.transfers.is_empty() || !m.fetches.is_empty());
                if !downloads && !links && !has_rows {
                    return None;
                }
                let machine =
                    machine.map_or_else(MachineTransfers::default, |machine| MachineTransfers {
                        transfers: machine.transfers.clone(),
                        fetches: machine.fetches.clone(),
                        loaded: (!downloads || machine.transfers_loaded)
                            && (!links || machine.fetches_loaded),
                        problem: machine
                            .transfers_problem
                            .clone()
                            .or_else(|| machine.fetches_problem.clone()),
                        ..Default::default()
                    });
                Some(MachineTransfers {
                    server_id: server.id.clone(),
                    name: server.name.clone(),
                    allows_links: links,
                    stream_incomplete: capabilities.contains(&Capability::StreamIncomplete),
                    ..machine
                })
            })
            .collect()
    }

    /// What the player needs for a file of a remote download, at Original
    /// while it is unfinished. Works before the listing has the video.
    pub fn transfer_playback(
        &self,
        settings: &Settings,
        server_id: &str,
        video_id: &str,
        title: &str,
    ) -> Option<RemotePlayback> {
        if let Some(playback) = self.playback(settings, server_id, video_id) {
            return Some(playback);
        }
        let server = settings.remote.servers.iter().find(|s| s.id == server_id)?;
        Some(RemotePlayback {
            server_id: server.id.clone(),
            server_name: server.name.clone(),
            video_id: video_id.to_owned(),
            title: title.to_owned(),
            duration: 0.,
            quality: Quality::Original,
            base_url: base_url(server),
            token: effective_token(&server.token),
        })
    }

    /// Whether a remote video has finished downloading: None before the
    /// server's first listing, false while it downloads or when the
    /// listing does not have it yet (a download started from Downloads).
    pub fn video_complete(&self, server_id: &str, video_id: &str) -> Option<bool> {
        let inner = self.inner.borrow();
        let listing = inner.servers.get(server_id)?.listing.as_ref()?;
        Some(listing.video(video_id).is_some_and(|video| video.complete))
    }

    /// Sends a command to a paired server; the answer arrives in
    /// [`Ready::Command`] after a [`Self::drain`]. Rows are asked for again
    /// after every answer.
    pub fn command(&mut self, settings: &Settings, server_id: &str, command: Command) {
        let Some(server) = settings.remote.servers.iter().find(|s| s.id == server_id) else {
            self.inner.get_mut().ready.push(Ready::Command {
                server_id: server_id.to_owned(),
                command,
                result: Err("This server is not paired".into()),
            });
            return;
        };
        let mut inner = self.inner.borrow_mut();
        let cancel = match &command {
            // A newer resolve on the same server replaces, and so cancels,
            // the older one.
            Command::Resolve { .. } => {
                let (sender, receiver) = tokio::sync::oneshot::channel();
                inner.resolves.insert(server_id.to_owned(), sender);
                Some(receiver)
            }
            Command::CancelResolve => {
                inner.resolves.remove(server_id);
                None
            }
            _ => None,
        };
        let request = Request::Command {
            endpoint: Endpoint::of(server),
            server_id: server_id.to_owned(),
            command,
            cancel,
        };
        self.send(&mut inner, Some(request));
    }

    /// Asks a server for a torrent's file list. `input` is from [`wire_input`].
    /// A later resolve on the same server cancels this one.
    pub fn resolve(&mut self, settings: &Settings, server_id: &str, input: String) {
        self.command(settings, server_id, Command::Resolve { input });
    }

    /// Ends the resolve in flight on a server: its request is dropped,
    /// which closes the connection and so ends the lookup on the server,
    /// and its answer is discarded. `DELETE /v1/transfers/resolve` is also
    /// sent for servers that track lookups apart from the connection; the
    /// cancellation does not depend on it.
    pub fn cancel_resolve(&mut self, settings: &Settings, server_id: &str) {
        self.command(settings, server_id, Command::CancelResolve);
    }

    /// Starts a torrent on a server with the chosen file indices.
    pub fn start(
        &mut self,
        settings: &Settings,
        server_id: &str,
        input: String,
        files: Vec<usize>,
    ) {
        self.command(settings, server_id, Command::Start { input, files });
    }

    pub fn pause(&mut self, settings: &Settings, server_id: &str, id: &str) {
        self.command(settings, server_id, Command::Pause { id: id.into() });
    }

    pub fn resume(&mut self, settings: &Settings, server_id: &str, id: &str) {
        self.command(settings, server_id, Command::Resume { id: id.into() });
    }

    pub fn pause_all(&mut self, settings: &Settings, server_id: &str) {
        self.command(settings, server_id, Command::PauseAll);
    }

    pub fn resume_all(&mut self, settings: &Settings, server_id: &str) {
        self.command(settings, server_id, Command::ResumeAll);
    }

    /// Removes a transfer. With `delete_files`, `confirm` must be the
    /// [`confirm_list`] the user saw; a changed list answers
    /// [`Outcome::ListChanged`] and nothing is deleted.
    pub fn remove(
        &mut self,
        settings: &Settings,
        server_id: &str,
        id: &str,
        delete_files: bool,
        confirm: Vec<String>,
    ) {
        self.command(
            settings,
            server_id,
            Command::Remove {
                id: id.into(),
                delete_files,
                confirm,
            },
        );
    }

    pub fn fetch_start(&mut self, settings: &Settings, server_id: &str, url: &str) {
        self.command(
            settings,
            server_id,
            Command::FetchStart {
                url: url.trim().into(),
            },
        );
    }

    pub fn fetch_cancel(&mut self, settings: &Settings, server_id: &str, id: u64) {
        self.command(settings, server_id, Command::FetchCancel { id });
    }

    /// Lists a folder under the server's allowed share roots; empty lists
    /// the roots.
    pub fn browse(&mut self, settings: &Settings, server_id: &str, dir: &str) {
        self.command(settings, server_id, Command::Browse { dir: dir.into() });
    }

    /// Seeds a folder or file from [`Self::browse`] on the server.
    pub fn share(&mut self, settings: &Settings, server_id: &str, dir: &str) {
        self.command(settings, server_id, Command::Share { dir: dir.into() });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flick::remote::protocol::Video;
    use std::{
        io::{BufRead, BufReader, Write},
        net::TcpListener,
        time::Instant,
    };

    const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const OTHER: &str = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";

    fn pairing(host: &str, port: u16, token: &str) -> Pairing {
        Pairing {
            host: host.into(),
            port,
            token: token.into(),
        }
    }

    fn library(videos: usize) -> Library {
        Library {
            protocol: protocol::VERSION,
            server_name: "Mac mini".into(),
            qualities: vec![Quality::Original, Quality::Low720],
            capabilities: Vec::new(),
            storage: None,
            videos: (0..videos)
                .map(|index| Video {
                    id: format!("v{index}"),
                    title: format!("Video {index}"),
                    file_name: format!("video-{index}.mkv"),
                    duration: 60.,
                    poster: true,
                    complete: true,
                    ..Default::default()
                })
                .collect(),
        }
    }

    #[test]
    fn pairing_becomes_a_server_named_after_the_host() {
        let server = server_from_pairing(
            &pairing("mohtashams-mac-mini.taildcfc7a.ts.net", 47117, TOKEN),
            "id".into(),
        );
        assert_eq!(server.id, "id");
        assert_eq!(server.name, "mohtashams-mac-mini");
        assert_eq!(server.host, "mohtashams-mac-mini.taildcfc7a.ts.net");
        assert_eq!(server.port, 47117);
        assert_eq!(server.token, TOKEN);
        assert_eq!(server.quality, Quality::Original);
        assert_eq!(default_name("100.98.200.69"), "100.98.200.69");
        assert_eq!(
            default_name("fd7a:115c:a1e0::6c2e:c846"),
            "fd7a:115c:a1e0::6c2e:c846"
        );
        assert_eq!(default_name("box"), "box");
    }

    #[test]
    fn pairing_the_same_address_again_replaces_the_token() {
        let mut servers = Vec::new();
        let first = upsert_pairing(&mut servers, &pairing("Box.ts.net", 1, TOKEN), || {
            "a".into()
        });
        servers[0].name = "Renamed".into();
        let again = upsert_pairing(&mut servers, &pairing("box.ts.net", 1, OTHER), || {
            panic!("no new id for a known address")
        });
        assert_eq!((first.as_str(), again.as_str()), ("a", "a"));
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].token, OTHER);
        assert_eq!(servers[0].name, "Renamed");
        let other_port = upsert_pairing(&mut servers, &pairing("box.ts.net", 2, TOKEN), || {
            "b".into()
        });
        assert_eq!(other_port, "b");
        assert_eq!(servers.len(), 2);
    }

    #[test]
    fn server_ids_are_unique_hex() {
        let ids: HashSet<String> = (0..100).map(|_| new_server_id()).collect();
        assert_eq!(ids.len(), 100);
        assert!(ids.iter().all(|id| id.len() == 32
            && id.bytes().all(|b| b.is_ascii_hexdigit())
            && cache_path(Path::new("."), id).is_some()));
    }

    #[test]
    fn http_codes_map_to_statuses() {
        assert_eq!(status_for_http(401, b""), Status::WrongToken);
        assert_eq!(status_for_http(403, b""), Status::WrongToken);
        assert_eq!(status_for_http(404, b""), Status::NotFlick);
        assert_eq!(
            status_for_http(500, br#"{"message":"Disk is gone"}"#),
            Status::Failed("Disk is gone".into())
        );
        assert_eq!(
            status_for_http(502, b"<html>"),
            Status::Failed("Server error (502)".into())
        );
        assert_eq!(status_for_http(503, b""), Status::Failed(BUSY.into()));
        assert_eq!(
            status_for_http(418, b""),
            Status::Failed("Unexpected reply (418)".into())
        );
        // Every 4xx and 5xx but 401, 403, and 404 carries the server's message.
        for code in [400, 409, 415, 422, 418, 503, 504] {
            assert_eq!(
                status_for_http(code, br#"{"message":" Bitmap track "}"#),
                Status::Failed("Bitmap track".into()),
                "{code}"
            );
        }
        assert_eq!(
            status_for_http(415, b"<html>"),
            Status::Failed("Unexpected reply (415)".into())
        );
        assert_eq!(
            status_for_http(409, br#"{"message":""}"#),
            Status::Failed("Still downloading on the server".into())
        );
        assert_eq!(
            status_for_http(404, br#"{"message":"gone"}"#),
            Status::NotFlick
        );
        assert_eq!(
            status_for_http(302, br#"{"message":"elsewhere"}"#),
            Status::Failed("Unexpected reply (302)".into())
        );
        assert_eq!(
            Status::TooNew(2).label(),
            "Server is newer than this Flick (protocol 2)"
        );
    }

    #[test]
    fn listings_from_newer_or_unknown_servers_are_refused() {
        let ok = serde_json::to_vec(&library(1)).unwrap();
        assert_eq!(parse_listing(&ok).unwrap(), library(1));
        let newer = Library {
            protocol: protocol::VERSION + 1,
            ..library(0)
        };
        assert_eq!(
            parse_listing(&serde_json::to_vec(&newer).unwrap()),
            Err(Status::TooNew(protocol::VERSION + 1))
        );
        assert_eq!(parse_listing(b"{}"), Err(Status::NotFlick));
        assert_eq!(parse_listing(b"<html>"), Err(Status::NotFlick));
    }

    #[test]
    fn cached_listings_round_trip_and_are_removed() {
        let dir = tempfile::tempdir().unwrap();
        let dir = cache_dir(dir.path());
        save_cache(&dir, "abc", &library(2)).unwrap();
        save_cache(&dir, "def", &library(1)).unwrap();
        assert!(save_cache(&dir, "../escape", &library(1)).is_err());
        std::fs::write(dir.join("broken.json"), b"{").unwrap();
        let mut loaded = load_caches(&dir);
        loaded.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(
            loaded,
            vec![("abc".into(), library(2)), ("def".into(), library(1))]
        );
        remove_cache(&dir, "abc").unwrap();
        remove_cache(&dir, "abc").unwrap();
        assert_eq!(load_caches(&dir).len(), 1);
        assert!(load_caches(&dir.join("missing")).is_empty());
    }

    #[test]
    fn urls_tokens_and_tiers() {
        let server = server_from_pairing(&pairing("fd7a::1", 9, TOKEN), "id".into());
        assert_eq!(base_url(&server), "http://[fd7a::1]:9");
        assert_eq!(address("fd7a::1", 9), "[fd7a::1]:9");
        assert_eq!(address("box", 9), "box:9");
        assert_eq!(choose_token("saved", None), "saved");
        assert_eq!(choose_token("saved", Some(" ")), "saved");
        assert_eq!(choose_token("saved", Some("env")), "env");
        let listing = library(1);
        assert_eq!(
            offered_qualities(Some(&listing)),
            vec![Quality::Original, Quality::Low720]
        );
        assert_eq!(offered_qualities(None), Quality::ALL.to_vec());
        assert_eq!(
            playable_quality(Quality::Data480, &listing),
            Quality::Original
        );
        assert_eq!(playable_quality(Quality::Low720, &listing), Quality::Low720);
        let offered = offered_qualities(Some(&listing));
        assert_eq!(next_quality(Quality::Original, &offered), Quality::Low720);
        assert_eq!(next_quality(Quality::Low720, &offered), Quality::Original);
        assert_eq!(next_quality(Quality::Data480, &offered), Quality::Original);
        let playback = RemotePlayback {
            server_id: "s".into(),
            server_name: "Box".into(),
            video_id: "abc".into(),
            title: "T".into(),
            duration: 1.,
            quality: Quality::Low720,
            base_url: "http://box:1".into(),
            token: TOKEN.into(),
        };
        assert_eq!(playback.url(), "http://box:1/v1/hls/abc/720p/index.m3u8");
        assert_eq!(playback.authorization(), format!("Bearer {TOKEN}"));
        assert!(!format!("{playback:?}").contains(TOKEN));
    }

    #[test]
    fn hashes_and_file_names() {
        assert_eq!(
            parse_hash(br#"{"hash":"8E245D9679D31E12"}"#).as_deref(),
            Some("8e245d9679d31e12")
        );
        assert_eq!(
            parse_hash(br#""8e245d9679d31e12""#).as_deref(),
            Some("8e245d9679d31e12")
        );
        assert_eq!(
            parse_hash(b"8e245d9679d31e12\n").as_deref(),
            Some("8e245d9679d31e12")
        );
        assert_eq!(parse_hash(b"nope"), None);
        assert_eq!(subtitle_extension("Movie.en.ASS"), "ass");
        assert_eq!(subtitle_extension("Movie"), "srt");
        assert_eq!(safe_name("a/b.c"), "a_b_c");
    }

    type Answer = (u16, &'static str, Vec<u8>);

    /// A loopback stand-in for a Flick server: one request per connection.
    fn fake_server(listing: Library, poster: Vec<u8>) -> u16 {
        loopback_server(move |_, path| {
            if path == protocol::paths::library() {
                (
                    200,
                    "application/json",
                    serde_json::to_vec(&listing).unwrap(),
                )
            } else if path == protocol::paths::poster("v0") {
                (200, "image/png", poster.clone())
            } else {
                (404, "application/json", br#"{"message":"gone"}"#.to_vec())
            }
        })
    }

    /// Answers 401 without the test token, else what `answer` gives for the
    /// method and path. HEAD answers carry no body.
    fn loopback_server(answer: impl Fn(&str, &str) -> Answer + Send + 'static) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let mut words = line.split_whitespace();
                let method = words.next().unwrap_or("").to_owned();
                let path = words.next().unwrap_or("").to_owned();
                let mut authorized = false;
                loop {
                    let mut header = String::new();
                    reader.read_line(&mut header).unwrap();
                    if header.trim().is_empty() {
                        break;
                    }
                    if let Some((name, value)) = header.split_once(':')
                        && name.eq_ignore_ascii_case("authorization")
                    {
                        authorized = value.trim() == protocol::bearer(TOKEN);
                    }
                }
                let (code, kind, body) = if !authorized {
                    (401, "application/json", br#"{"message":"no"}"#.to_vec())
                } else {
                    answer(&method, &path)
                };
                let _ = write!(
                    stream,
                    "HTTP/1.1 {code} X\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                if method != "HEAD" {
                    let _ = stream.write_all(&body);
                }
            }
        });
        port
    }

    fn wait_until(state: &mut RemoteState, mut done: impl FnMut(&mut RemoteState) -> bool) {
        let started = Instant::now();
        while !done(state) {
            assert!(started.elapsed() < Duration::from_secs(10), "timed out");
            state.drain();
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn lists_caches_and_fetches_posters_from_a_loopback_server() {
        let data = tempfile::tempdir().unwrap();
        let data_dir = data.path();
        let mut png = Vec::new();
        image::RgbaImage::from_pixel(2, 3, image::Rgba([10, 20, 30, 255]))
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let port = fake_server(library(2), png);
        let mut state = RemoteState::with_data_dir(data_dir);
        let mut settings = Settings::default();

        assert!(
            state
                .add_pairing(&mut settings, "flick-remote://nowhere")
                .is_err()
        );
        let id = state
            .add_pairing(
                &mut settings,
                &format!("flick-remote://127.0.0.1:{port}/#{TOKEN}"),
            )
            .unwrap();
        assert_eq!(settings.remote.servers.len(), 1);
        assert_eq!(settings.remote.servers[0].name, "127.0.0.1");
        wait_until(&mut state, |state| {
            state.servers(&settings)[0].status == Status::Online
        });
        let view = &state.servers(&settings)[0];
        assert_eq!(view.listing.as_deref(), Some(&library(2)));
        assert_eq!(view.summary(), "Online · 2 videos");
        assert!(cache_dir(data_dir).join(format!("{id}.json")).exists());

        let playback = state.playback(&settings, &id, "v1").unwrap();
        assert_eq!(
            playback.url(),
            format!("http://127.0.0.1:{port}/v1/media/v1")
        );
        assert_eq!(playback.title, "Video 1");
        assert!(state.playback(&settings, &id, "missing").is_none());

        assert!(state.poster(&id, "v0").is_none());
        wait_until(&mut state, |state| state.poster(&id, "v0").is_some());
        // v1 has a poster in the listing but the server answers 404.
        assert!(state.poster(&id, "v1").is_none());
        wait_until(&mut state, |state| {
            state.inner.borrow().posters.failed.len() == 1
        });

        // A restarted Flick shows the cached listing before any reply.
        let restarted = RemoteState::with_data_dir(data_dir);
        let cached = restarted.servers(&settings);
        assert_eq!(cached[0].listing.as_deref(), Some(&library(2)));

        // Re-pairing with a wrong token keeps the server and its listing.
        let again = state
            .add_pairing(
                &mut settings,
                &format!("flick-remote://127.0.0.1:{port}/#{OTHER}"),
            )
            .unwrap();
        assert_eq!(again, id);
        wait_until(&mut state, |state| {
            state.servers(&settings)[0].status == Status::WrongToken
        });
        assert!(state.servers(&settings)[0].listing.is_some());

        state.remove_server(&mut settings, &id);
        assert!(settings.remote.servers.is_empty());
        assert!(!cache_dir(data_dir).join(format!("{id}.json")).exists());
        assert_eq!(state.take_dropped_images().len(), 1);
    }

    #[test]
    fn an_unreachable_server_is_offline() {
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let data = tempfile::tempdir().unwrap();
        let mut state = RemoteState::with_data_dir(data.path());
        let mut settings = Settings::default();
        state
            .add_pairing(
                &mut settings,
                &format!("flick-remote://127.0.0.1:{port}/#{TOKEN}"),
            )
            .unwrap();
        wait_until(&mut state, |state| {
            state.servers(&settings)[0].status == Status::Offline
        });
    }

    fn wait_for_ready(state: &mut RemoteState) -> Ready {
        let started = Instant::now();
        loop {
            state.drain();
            if let Some(ready) = state.take_ready().pop() {
                return ready;
            }
            assert!(started.elapsed() < Duration::from_secs(10), "timed out");
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn error_body(message: &str) -> Vec<u8> {
        serde_json::to_vec(&ErrorBody {
            message: message.into(),
        })
        .unwrap()
    }

    #[test]
    fn server_messages_reach_subtitles_and_playback_checks() {
        const BITMAP: &str = "Track 3 is a bitmap track, which cannot be converted to WebVTT";
        const DOWNLOADING: &str = "This video is still downloading on the server";
        const NO_LENGTH: &str = "The server could not read this video's length";
        let port = loopback_server(|method, path| {
            let json = "application/json";
            if path == protocol::paths::library() {
                (200, json, serde_json::to_vec(&library(3)).unwrap())
            } else if path == protocol::paths::embedded_subtitle("v0", 3) {
                (415, json, error_body(BITMAP))
            } else if path == protocol::paths::hls_playlist("v0", Quality::Low720) {
                (200, "application/vnd.apple.mpegurl", b"#EXTM3U\n".to_vec())
            } else if path == protocol::paths::hls_playlist("v1", Quality::Low720) {
                (409, json, error_body(DOWNLOADING))
            } else if path == protocol::paths::hls_playlist("v2", Quality::Low720) {
                (422, json, error_body(NO_LENGTH))
            } else if path == protocol::paths::hls_playlist("v2", Quality::Data480) {
                (503, "text/plain", b"busy".to_vec())
            } else if method == "HEAD" && path == protocol::paths::media("v0") {
                (200, "video/mp4", vec![0; 1024])
            } else if method == "HEAD" && path == protocol::paths::media("v2") {
                // Too many unfinished-file streams open: Watch Now is refused.
                (503, json, error_body("Too many streams are open"))
            } else {
                (404, json, error_body("gone"))
            }
        });
        let data = tempfile::tempdir().unwrap();
        let mut state = RemoteState::with_data_dir(data.path());
        let mut settings = Settings::default();
        let id = state
            .add_pairing(
                &mut settings,
                &format!("flick-remote://127.0.0.1:{port}/#{TOKEN}"),
            )
            .unwrap();
        wait_until(&mut state, |state| {
            state.servers(&settings)[0].status == Status::Online
        });

        state.fetch_subtitle(&id, "v0", SubtitleRef::Embedded(3));
        let Ready::Subtitle { result, .. } = wait_for_ready(&mut state) else {
            panic!("expected a subtitle");
        };
        assert_eq!(result.unwrap_err(), BITMAP);

        let mut check = |video: &str, quality: Quality| {
            let number = state.check_playback(&id, video, quality);
            let Ready::Playback {
                check,
                server_id,
                video_id,
                quality: answered,
                result,
            } = wait_for_ready(&mut state)
            else {
                panic!("expected a playback check");
            };
            assert_eq!(
                (check, server_id.as_str(), video_id.as_str(), answered),
                (number, id.as_str(), video, quality)
            );
            result
        };
        assert_eq!(check("v0", Quality::Low720), Ok(()));
        assert_eq!(check("v0", Quality::Original), Ok(()));
        assert_eq!(check("v1", Quality::Low720), Err(DOWNLOADING.into()));
        assert_eq!(check("v2", Quality::Low720), Err(NO_LENGTH.into()));
        assert_eq!(check("v2", Quality::Data480), Err(BUSY.into()));
        // A HEAD answer has no body, so a busy stream limit says BUSY.
        assert_eq!(
            check("v2", Quality::Original),
            Err("The server is busy; try again in a moment".into())
        );
        assert_eq!(
            check("v1", Quality::Original),
            Err("The server no longer has this video".into())
        );
        assert_eq!(
            check("missing", Quality::Low720),
            Err("The server no longer has this video".into())
        );

        let unpaired = state.check_playback("nobody", "v0", Quality::Low720);
        let Ready::Playback { check, result, .. } = wait_for_ready(&mut state) else {
            panic!("expected a playback check");
        };
        assert_eq!(check, unpaired);
        assert_eq!(result, Err("This server is not paired".into()));
    }

    #[test]
    fn no_worker_starts_after_shutdown() {
        let port = fake_server(library(1), Vec::new());
        let data = tempfile::tempdir().unwrap();
        let mut state = RemoteState::with_data_dir(data.path());
        let mut settings = Settings::default();
        let id = state
            .add_pairing(
                &mut settings,
                &format!("flick-remote://127.0.0.1:{port}/#{TOKEN}"),
            )
            .unwrap();
        wait_until(&mut state, |state| {
            state.servers(&settings)[0].status == Status::Online
        });
        state.shutdown();
        assert!(state.worker.get().is_none());

        // Every kind of request fails at once instead of restarting it.
        state.fetch_hash(&id, "v0");
        state.fetch_subtitle(&id, "v0", SubtitleRef::Sidecar(0));
        state.check_playback(&id, "v0", Quality::Low720);
        state.put_progress(&settings, &id, "v0", Progress::default());
        state.refresh(&settings);
        assert!(state.poster(&id, "v0").is_none());
        assert!(state.worker.get().is_none());
        let ready = state.take_ready();
        assert_eq!(ready.len(), 3);
        assert!(ready.iter().all(|ready| match ready {
            Ready::Subtitle { result, .. } => result.is_err(),
            Ready::Hash { result, .. } => result.is_err(),
            Ready::Playback { result, .. } => result.is_err(),
            Ready::Command { result, .. } => result.is_err(),
        }));
        assert!(!state.servers(&settings)[0].status.is_online());

        // A state shut down before any request never starts one.
        let mut fresh = RemoteState::with_data_dir(data.path());
        fresh.shutdown();
        fresh.servers(&settings);
        assert!(fresh.worker.get().is_none());
    }

    // Downloads on another machine.

    use flick::remote::protocol::{BrowseEntry, Storage, TorrentFileInfo, TransferFileRow};
    use std::{
        io::Read,
        sync::{
            Mutex,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
    };

    fn facts<'a>(id: &'a str, listing: Option<&'a Library>, status: &'a Status) -> ServerFacts<'a> {
        ServerFacts {
            id,
            name: id,
            listing,
            status,
        }
    }

    fn capable(capabilities: &[Capability], free: u64) -> Library {
        Library {
            capabilities: capabilities.to_vec(),
            storage: Some(Storage {
                free_bytes: free,
                download_folder_name: "Downloads".into(),
            }),
            ..library(0)
        }
    }

    #[test]
    fn destinations_list_this_mac_first_with_what_each_server_allows() {
        let all = capable(
            &[Capability::Downloads, Capability::Links, Capability::Shares],
            5_000_000_000,
        );
        let read_only = library(1);
        let (online, offline) = (Status::Online, Status::Offline);
        let destinations = destinations_from(
            Some(42),
            &[
                facts("mini", Some(&all), &online),
                facts("box", Some(&read_only), &online),
                facts("asleep", Some(&all), &offline),
                facts("new", None, &Status::Connecting),
            ],
        );
        let ids: Vec<_> = destinations.iter().map(|d| d.id.as_deref()).collect();
        assert_eq!(
            ids,
            [None, Some("mini"), Some("box"), Some("asleep"), Some("new")]
        );
        let local = &destinations[0];
        assert!(local.is_local() && local.online && local.selectable(Sheet::Share));
        assert_eq!(
            (local.name.as_str(), local.free_bytes),
            (this_machine_name(), Some(42))
        );

        let mini = &destinations[1];
        assert!(mini.allows_downloads && mini.allows_links && mini.allows_shares);
        assert_eq!(mini.free_bytes, Some(5_000_000_000));
        assert_eq!(
            mini.detail(Sheet::Torrent),
            format!("{} free", flick::library::format_bytes(5_000_000_000))
        );
        let read_only = &destinations[2];
        assert!(!read_only.selectable(Sheet::Torrent) && read_only.online);
        assert_eq!(read_only.detail(Sheet::Torrent), "Not accepting downloads");
        assert_eq!(read_only.detail(Sheet::Share), "Not sharing folders");
        // Offline keeps its last known capabilities but cannot be chosen.
        let asleep = &destinations[3];
        assert!(asleep.allows_downloads && !asleep.selectable(Sheet::Torrent));
        assert_eq!(asleep.detail(Sheet::Torrent), "Offline");
        assert_eq!(destinations[4].detail(Sheet::Link), "Connecting…");
        assert!(!destinations[4].allows_links);

        assert!(offers_remote(&destinations, Sheet::Torrent));
        assert!(!offers_remote(&destinations[..1], Sheet::Torrent));
        assert!(!offers_remote(
            &destinations_from(None, &[facts("box", Some(&library(0)), &online)]),
            Sheet::Link
        ));
    }

    #[test]
    fn save_to_defaults_and_remembers_per_sheet_while_choosable() {
        let downloads = capable(&[Capability::Downloads], 1);
        let (online, offline) = (Status::Online, Status::Offline);
        let destinations = destinations_from(
            None,
            &[
                facts("mini", Some(&downloads), &online),
                facts("asleep", Some(&downloads), &offline),
            ],
        );
        let mini = Some("mini".to_owned());
        // No memory: the onboarding default, else this Mac.
        assert_eq!(
            choose_destination(&destinations, Sheet::Torrent, None, "mini"),
            mini
        );
        assert_eq!(
            choose_destination(&destinations, Sheet::Torrent, None, ""),
            None
        );
        // A default that is offline, unknown, or not allowed falls back.
        assert_eq!(
            choose_destination(&destinations, Sheet::Torrent, None, "asleep"),
            None
        );
        assert_eq!(
            choose_destination(&destinations, Sheet::Torrent, None, "gone"),
            None
        );
        assert_eq!(
            choose_destination(&destinations, Sheet::Link, None, "mini"),
            None
        );
        // The last choice wins over the default, this Mac included.
        assert_eq!(
            choose_destination(&destinations, Sheet::Torrent, Some(&None), "mini"),
            None
        );
        assert_eq!(
            choose_destination(&destinations, Sheet::Torrent, Some(&mini), ""),
            mini
        );
        // A remembered machine that went offline gives way to the default.
        assert_eq!(
            choose_destination(
                &destinations,
                Sheet::Torrent,
                Some(&Some("asleep".into())),
                "mini"
            ),
            mini
        );
    }

    fn file_row(
        index: usize,
        path: &str,
        size: u64,
        verified: u64,
        selected: bool,
    ) -> TransferFileRow {
        TransferFileRow {
            index,
            path: path.into(),
            size,
            verified_bytes: verified,
            selected,
            will_delete: selected,
            video_id: selected.then(|| format!("vid{index}")),
        }
    }

    fn transfer(id: &str, phase: TransferPhase, verified: u64) -> TransferRow {
        TransferRow {
            id: id.into(),
            name: "Show".into(),
            phase,
            verified_bytes: verified,
            total_bytes: 200,
            files: vec![
                file_row(0, "Show/E01.mkv", 100, verified.min(100), true),
                file_row(1, "Show/notes.txt", 10, 0, false),
                file_row(2, "Show/E02.mkv", 100, verified.saturating_sub(100), true),
            ],
            ..Default::default()
        }
    }

    #[test]
    fn deletion_confirms_exactly_the_files_the_server_will_delete() {
        assert_eq!(
            confirm_list(&transfer("a", TransferPhase::Downloading, 0)),
            ["Show/E01.mkv", "Show/E02.mkv"]
        );
        assert!(confirm_list(&TransferRow::default()).is_empty());
        // An unselected file with data on disk is deleted too, and listed.
        let mut row = transfer("a", TransferPhase::Paused, 0);
        row.files[1].will_delete = true;
        row.files[2].will_delete = false;
        assert_eq!(confirm_list(&row), ["Show/E01.mkv", "Show/notes.txt"]);
        // A server older than `will_delete` sets none: the selected files.
        for file in &mut row.files {
            file.will_delete = false;
        }
        assert_eq!(confirm_list(&row), ["Show/E01.mkv", "Show/E02.mkv"]);
        assert_eq!(
            parse_current_list(br#"{"message":"changed","files":["a","b"]}"#),
            Some(vec!["a".into(), "b".into()])
        );
        assert_eq!(
            parse_current_list(
                br#"{"message":"x","current":[{"path":"a","selected":true},{"path":"b","selected":false}]}"#
            ),
            Some(vec!["a".into()])
        );
        assert_eq!(
            parse_current_list(
                br#"{"files":[{"path":"a","selected":false,"will_delete":true},{"path":"b","selected":true,"will_delete":false}]}"#
            ),
            Some(vec!["a".into()])
        );
        assert_eq!(parse_current_list(br#"{"message":"no list"}"#), None);
        assert_eq!(parse_current_list(b"<html>"), None);
    }

    #[test]
    fn command_errors_carry_the_servers_message() {
        const OFF: &str = "This Flick does not accept downloads from other Flicks. Its owner can allow it in Settings → Remote Libraries.";
        let body = |message: &str| error_body(message);
        assert_eq!(command_error(403, &body(OFF)), OFF);
        assert_eq!(
            command_error(403, b""),
            "This Flick's owner has not allowed that from other Flicks"
        );
        assert_eq!(command_error(401, &body("no")), Status::WrongToken.label());
        for code in [404, 409, 422, 503, 504] {
            assert_eq!(
                command_error(code, &body("Server says")),
                "Server says",
                "{code}"
            );
        }
        assert_eq!(
            command_error(404, b""),
            "The server no longer has this. Refresh and try again"
        );
        assert_eq!(command_error(409, b""), "The server already has this");
        assert_eq!(command_error(503, b""), BUSY);
        assert_eq!(
            command_error(504, b""),
            "The server took too long to answer. Try again"
        );
        assert_eq!(command_error(422, b"<html>"), "Unexpected reply (422)");
        assert_eq!(command_error(500, b""), "Server error (500)");
        assert_eq!(Failure::Offline.message(), "The server did not answer");
    }

    #[test]
    fn wire_inputs_and_answers_parse() {
        use flick::torrent::TorrentInput;
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64(&[0xff, 0xfe, 0xfd]), "//79");
        assert_eq!(
            wire_input(&TorrentInput::Magnet(" magnet:?xt=urn:btih:abc ".into())).unwrap(),
            "magnet:?xt=urn:btih:abc"
        );
        assert_eq!(
            wire_input(&TorrentInput::Bytes(b"d4:infoe".to_vec())).unwrap(),
            "base64:ZDQ6aW5mb2U="
        );
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.torrent");
        std::fs::write(&file, b"foo").unwrap();
        assert_eq!(
            wire_input(&TorrentInput::File(file)).unwrap(),
            "base64:Zm9v"
        );
        assert!(wire_input(&TorrentInput::File(dir.path().join("missing"))).is_err());

        assert_eq!(parse_fetch_id(br#"{"id":7}"#), Some(7));
        assert_eq!(parse_fetch_id(br#"{"id":"8"}"#), Some(8));
        assert_eq!(parse_fetch_id(b"9"), Some(9));
        assert_eq!(parse_fetch_id(b"{}"), None);
        let rows = serde_json::to_vec(&vec![transfer("a", TransferPhase::Paused, 1)]).unwrap();
        assert_eq!(parse_rows::<TransferRow>(&rows).unwrap()[0].id, "a");
        let wrapped = format!(r#"{{"transfers":{}}}"#, String::from_utf8(rows).unwrap());
        assert_eq!(
            parse_rows::<TransferRow>(wrapped.as_bytes()).unwrap().len(),
            1
        );
        assert!(parse_rows::<TransferRow>(b"{}").is_err());
        assert!(parse_rows::<FetchRow>(b"[1]").is_err());
    }

    #[test]
    fn finished_downloads_and_polling_intervals() {
        use std::slice::from_ref as one;
        let downloading = transfer("a", TransferPhase::Downloading, 50);
        let first_file = transfer("a", TransferPhase::Downloading, 120);
        let seeding = transfer("a", TransferPhase::Seeding, 200);
        assert!(!transfers_finished(one(&downloading), one(&downloading)));
        assert!(transfers_finished(one(&downloading), one(&first_file)));
        assert!(transfers_finished(one(&first_file), one(&seeding)));
        assert!(!transfers_finished(one(&seeding), one(&seeding)));
        // A new transfer that already has a file counts; one without does not.
        assert!(transfers_finished(&[], one(&first_file)));
        assert!(!transfers_finished(&[], one(&downloading)));

        let running = FetchRow {
            id: 1,
            phase: FetchPhase::Downloading,
            ..Default::default()
        };
        let done = FetchRow {
            phase: FetchPhase::Done,
            saved: vec!["v".into()],
            ..running.clone()
        };
        assert!(fetches_finished(one(&running), one(&done)));
        assert!(!fetches_finished(one(&done), one(&done)));
        assert!(!fetches_finished(one(&running), one(&running)));

        let now = Instant::now();
        assert!(poll_due(None, now, false));
        assert!(!poll_due(Some(now), now, true));
        let earlier = now - Duration::from_secs(2);
        assert!(poll_due(Some(earlier), now, true));
        assert!(!poll_due(Some(earlier), now, false));
        assert!(poll_due(Some(now - POLL_IDLE), now, false));
    }

    #[test]
    fn videos_not_known_to_be_finished_offer_original_and_are_checked_first() {
        let server = Some(vec![Quality::Original, Quality::Low720]);
        assert_eq!(
            picker_qualities(Some(true), server.clone()),
            [Quality::Original, Quality::Low720]
        );
        assert_eq!(picker_qualities(Some(true), None), Quality::ALL);
        // Still downloading, or not listed yet (Watch Now before the listing).
        for complete in [Some(false), None] {
            assert_eq!(
                picker_qualities(complete, server.clone()),
                [Quality::Original]
            );
            assert!(check_before_loading(Quality::Original, complete));
        }
        assert!(!check_before_loading(Quality::Original, Some(true)));
        assert!(check_before_loading(Quality::Low720, Some(true)));
    }

    #[test]
    fn unfinished_videos_play_at_original() {
        let mut listing = Library {
            qualities: vec![Quality::Original, Quality::Low720],
            ..library(2)
        };
        listing.videos[1].complete = false;
        let port = fake_server(listing, Vec::new());
        let data = tempfile::tempdir().unwrap();
        let mut state = RemoteState::with_data_dir(data.path());
        let mut settings = Settings::default();
        let id = state
            .add_pairing(
                &mut settings,
                &format!("flick-remote://127.0.0.1:{port}/#{TOKEN}"),
            )
            .unwrap();
        settings.remote.servers[0].quality = Quality::Low720;
        wait_until(&mut state, |state| {
            state.servers(&settings)[0].status == Status::Online
        });
        assert_eq!(
            state.playback(&settings, &id, "v0").unwrap().quality,
            Quality::Low720
        );
        assert_eq!(
            state.playback(&settings, &id, "v1").unwrap().quality,
            Quality::Original
        );
        assert_eq!(state.video_complete(&id, "v0"), Some(true));
        assert_eq!(state.video_complete(&id, "v1"), Some(false));
        assert_eq!(state.video_complete(&id, "new"), Some(false));
        assert_eq!(state.video_complete("other", "v0"), None);
        // A file the listing does not have yet plays at Original.
        let unlisted = state
            .transfer_playback(&settings, &id, "new", "New.mkv")
            .unwrap();
        assert_eq!(
            (unlisted.quality, unlisted.title.as_str(), unlisted.url()),
            (
                Quality::Original,
                "New.mkv",
                format!("http://127.0.0.1:{port}/v1/media/new")
            )
        );
    }

    /// What the fake server with downloads saw and decides.
    #[derive(Default)]
    struct Fake {
        /// Each `GET /v1/transfers` moves the download forward.
        polls: AtomicUsize,
        listings: AtomicUsize,
        /// Drops `/v1/transfers` connections without answering.
        down: AtomicBool,
        started: Mutex<Vec<StartTransferBody>>,
        removed: Mutex<Vec<protocol::RemoveTransfer>>,
    }

    type StartTransferBody = protocol::StartTransfer;

    const OWNER_OFF: &str = "This Flick does not accept downloads from other Flicks. Its owner can allow it in Settings → Remote Libraries.";

    /// A loopback Flick that answers the command routes. `writes` is false
    /// for a server whose owner has not allowed downloads: its listing has
    /// no capabilities and every command answers 403.
    fn transfers_server(fake: Arc<Fake>, writes: bool) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                if reader.read_line(&mut line).is_err() {
                    continue;
                }
                let mut words = line.split_whitespace();
                let method = words.next().unwrap_or("").to_owned();
                let path = words.next().unwrap_or("").to_owned();
                let (mut authorized, mut length) = (false, 0usize);
                loop {
                    let mut header = String::new();
                    reader.read_line(&mut header).unwrap();
                    if header.trim().is_empty() {
                        break;
                    }
                    if let Some((name, value)) = header.split_once(':') {
                        if name.eq_ignore_ascii_case("authorization") {
                            authorized = value.trim() == protocol::bearer(TOKEN);
                        } else if name.eq_ignore_ascii_case("content-length") {
                            length = value.trim().parse().unwrap();
                        }
                    }
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                if path == protocol::paths::transfers() && fake.down.load(Ordering::SeqCst) {
                    continue;
                }
                let (code, reply) = if !authorized {
                    (401, error_body("no"))
                } else {
                    answer(&fake, writes, &method, &path, &body)
                };
                let _ = write!(
                    stream,
                    "HTTP/1.1 {code} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    reply.len()
                );
                let _ = stream.write_all(&reply);
            }
        });
        port
    }

    fn answer(fake: &Fake, writes: bool, method: &str, path: &str, body: &[u8]) -> (u16, Vec<u8>) {
        let json = |value: &dyn erased::Json| serde_json::to_vec(&value.json()).unwrap();
        if path == protocol::paths::library() {
            fake.listings.fetch_add(1, Ordering::SeqCst);
            let listing = if writes {
                capable(
                    &[
                        Capability::Downloads,
                        Capability::Links,
                        Capability::Shares,
                        Capability::StreamIncomplete,
                    ],
                    7_000_000_000,
                )
            } else {
                library(1)
            };
            return (200, json(&listing));
        }
        if !writes {
            return (403, error_body(OWNER_OFF));
        }
        let id = "c".repeat(40);
        match (method, path) {
            ("GET", "/v1/transfers") => {
                let step = fake.polls.fetch_add(1, Ordering::SeqCst) as u64;
                let row = if step >= 2 {
                    transfer(&id, TransferPhase::Seeding, 200)
                } else {
                    transfer(&id, TransferPhase::Downloading, 40 * step)
                };
                (200, json(&vec![row]))
            }
            ("GET", "/v1/fetches") => (200, b"[]".to_vec()),
            ("POST", "/v1/transfers/resolve") => (
                200,
                json(&TorrentInfo {
                    infohash: id,
                    name: "Show".into(),
                    files: vec![
                        TorrentFileInfo {
                            index: 0,
                            path: "Show/E01.mkv".into(),
                            size: 100,
                        },
                        TorrentFileInfo {
                            index: 1,
                            path: "Show/notes.txt".into(),
                            size: 10,
                        },
                    ],
                }),
            ),
            ("POST", "/v1/transfers") => {
                let start: protocol::StartTransfer = serde_json::from_slice(body).unwrap();
                let duplicate = fake.started.lock().unwrap().contains(&start);
                fake.started.lock().unwrap().push(start);
                if duplicate {
                    (409, error_body("This download is already on the Mac mini"))
                } else {
                    (200, json(&protocol::Started { id }))
                }
            }
            ("POST", path) if path.ends_with("/pause") || path.ends_with("/resume") => {
                (204, Vec::new())
            }
            ("DELETE", path) if path == protocol::paths::transfer(&id) => {
                let remove: protocol::RemoveTransfer = serde_json::from_slice(body).unwrap();
                fake.removed.lock().unwrap().push(remove.clone());
                let current = confirm_list(&transfer(&id, TransferPhase::Seeding, 200));
                if remove.delete_files && remove.confirm.is_empty() {
                    // A refusal that is not a changed list carries no list.
                    (409, error_body("Confirm the files to delete"))
                } else if remove.delete_files && remove.confirm != current {
                    let reply = serde_json::json!({
                        "message": "The files changed since you looked",
                        "files": current,
                    });
                    (409, serde_json::to_vec(&reply).unwrap())
                } else {
                    (204, Vec::new())
                }
            }
            ("POST", "/v1/fetches") => (200, br#"{"id":3}"#.to_vec()),
            ("GET", path) if path.starts_with("/v1/browse") => {
                let dir = path
                    .split_once("dir=")
                    .map_or("", |(_, dir)| dir)
                    .to_owned();
                (
                    200,
                    json(&BrowseListing {
                        dir,
                        entries: vec![BrowseEntry {
                            name: "Films".into(),
                            dir: true,
                            size: 0,
                        }],
                    }),
                )
            }
            ("POST", "/v1/shares") => {
                let share: protocol::StartShare = serde_json::from_slice(body).unwrap();
                (
                    200,
                    json(&ShareResult {
                        infohash: "d".repeat(40),
                        magnet: format!("magnet:?dn={}", share.dir),
                    }),
                )
            }
            _ => (404, error_body("gone")),
        }
    }

    fn command_answer(state: &mut RemoteState) -> (String, Command, Result<Outcome, String>) {
        let started = Instant::now();
        loop {
            state.drain();
            if let Some(Ready::Command {
                server_id,
                command,
                result,
            }) = state.take_ready().pop()
            {
                return (server_id, command, result);
            }
            assert!(started.elapsed() < Duration::from_secs(10), "timed out");
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn downloads_on_a_loopback_server_poll_command_and_refresh_its_listing() {
        let fake = Arc::new(Fake::default());
        let closed = Arc::new(Fake::default());
        let port = transfers_server(fake.clone(), true);
        let closed_port = transfers_server(closed.clone(), false);
        let data = tempfile::tempdir().unwrap();
        let mut state = RemoteState::with_data_dir(data.path());
        state.set_local_folder(data.path());
        let mut settings = Settings::default();
        let mini = state
            .add_pairing(
                &mut settings,
                &format!("flick-remote://127.0.0.1:{port}/#{TOKEN}"),
            )
            .unwrap();
        let other = state
            .add_pairing(
                &mut settings,
                &format!("flick-remote://127.0.0.1:{closed_port}/#{TOKEN}"),
            )
            .unwrap();
        wait_until(&mut state, |state| {
            state
                .servers(&settings)
                .iter()
                .all(|s| s.status == Status::Online)
        });

        // Save to: this Mac, the server that allows it, the one that does not.
        let destinations = state.destinations(&settings);
        assert_eq!(destinations.len(), 3);
        assert!(destinations[0].free_bytes.is_some());
        assert!(destinations[1].selectable(Sheet::Torrent));
        assert_eq!(destinations[1].free_bytes, Some(7_000_000_000));
        assert!(!destinations[2].selectable(Sheet::Torrent));
        assert_eq!(state.destination(&settings, Sheet::Torrent), None);
        settings.remote.default_destination = mini.clone();
        assert_eq!(
            state.destination(&settings, Sheet::Torrent),
            Some(mini.clone())
        );
        assert_eq!(
            state.destination(&settings, Sheet::Share),
            Some(mini.clone())
        );
        state.remember_destination(Sheet::Torrent, None);
        assert_eq!(state.destination(&settings, Sheet::Torrent), None);
        assert_eq!(
            state.destination(&settings, Sheet::Link),
            Some(mini.clone())
        );
        settings.remote.default_destination = other.clone();
        assert_eq!(state.destination(&settings, Sheet::Link), None);

        // Rows advance once a second while Downloads is open; reaching
        // Seeding asks for the listing again.
        let listings = fake.listings.load(Ordering::SeqCst);
        let started = Instant::now();
        loop {
            state.poll_transfers(&settings, true);
            state.drain();
            let machines = state.machine_transfers(&settings);
            assert_eq!(machines.len(), 1, "only the server that takes downloads");
            let machine = &machines[0];
            if machine.transfers.first().map(|row| row.phase) == Some(TransferPhase::Seeding)
                && fake.listings.load(Ordering::SeqCst) > listings
            {
                assert!(machine.loaded && machine.problem.is_none());
                assert!(machine.allows_links && machine.stream_incomplete);
                assert_eq!(machine.name, "127.0.0.1");
                break;
            }
            assert!(started.elapsed() < Duration::from_secs(15), "timed out");
            thread::sleep(Duration::from_millis(20));
        }
        assert!(fake.polls.load(Ordering::SeqCst) >= 3);
        // Closed, it asks once a minute: nothing new right away.
        let polls = fake.polls.load(Ordering::SeqCst);
        state.poll_transfers(&settings, false);
        thread::sleep(Duration::from_millis(100));
        state.drain();
        assert_eq!(fake.polls.load(Ordering::SeqCst), polls);

        // Resolve and start on the server.
        state.resolve(&settings, &mini, "magnet:?xt=urn:btih:abc".into());
        let (server, command, result) = command_answer(&mut state);
        assert_eq!(server, mini);
        assert!(matches!(command, Command::Resolve { .. }));
        let Ok(Outcome::Resolved(info)) = result else {
            panic!("expected a file list: {result:?}");
        };
        assert_eq!(info.files.len(), 2);
        state.start(&settings, &mini, "magnet:?xt=urn:btih:abc".into(), vec![0]);
        let (_, _, result) = command_answer(&mut state);
        assert_eq!(result, Ok(Outcome::Started { id: "c".repeat(40) }));
        assert_eq!(
            fake.started.lock().unwrap()[0],
            protocol::StartTransfer {
                input: "magnet:?xt=urn:btih:abc".into(),
                files: vec![0],
            }
        );
        state.start(&settings, &mini, "magnet:?xt=urn:btih:abc".into(), vec![0]);
        let (_, _, result) = command_answer(&mut state);
        assert_eq!(
            result,
            Err("This download is already on the Mac mini".into())
        );

        // Deleting with an outdated list shows the server's list and asks again.
        let id = "c".repeat(40);
        state.remove(&settings, &mini, &id, true, vec!["Show/E01.mkv".into()]);
        let (_, _, result) = command_answer(&mut state);
        let Ok(Outcome::ListChanged { message, current }) = result else {
            panic!("expected the current list: {result:?}");
        };
        assert_eq!(message, "The files changed since you looked");
        assert_eq!(current, ["Show/E01.mkv", "Show/E02.mkv"]);
        // A 409 without a list is said once, not asked again.
        state.remove(&settings, &mini, &id, true, Vec::new());
        assert_eq!(
            command_answer(&mut state).2,
            Err("Confirm the files to delete".into())
        );
        state.remove(&settings, &mini, &id, true, current.clone());
        assert_eq!(command_answer(&mut state).2, Ok(Outcome::Done));
        state.remove(&settings, &mini, &id, false, current);
        assert_eq!(command_answer(&mut state).2, Ok(Outcome::Done));
        let removed = fake.removed.lock().unwrap().clone();
        assert_eq!(removed.len(), 4);
        // Keep Files never sends a list.
        assert!(!removed[3].delete_files && removed[3].confirm.is_empty());

        state.pause(&settings, &mini, &id);
        assert_eq!(command_answer(&mut state).2, Ok(Outcome::Done));
        state.fetch_start(&settings, &mini, " https://example.com/watch ");
        assert_eq!(
            command_answer(&mut state).2,
            Ok(Outcome::FetchStarted { id: 3 })
        );
        state.browse(&settings, &mini, "Media");
        let Ok(Outcome::Browsed(listing)) = command_answer(&mut state).2 else {
            panic!("expected a folder list");
        };
        assert_eq!((listing.dir.as_str(), listing.entries.len()), ("Media", 1));
        state.share(&settings, &mini, "Media/Films");
        let Ok(Outcome::Shared(shared)) = command_answer(&mut state).2 else {
            panic!("expected a share link");
        };
        assert!(shared.magnet.ends_with("Media/Films"));

        // The owner of the other server has not allowed downloads.
        state.pause_all(&settings, &other);
        let (server, command, result) = command_answer(&mut state);
        assert_eq!(
            (server.as_str(), command),
            (other.as_str(), Command::PauseAll)
        );
        assert_eq!(result, Err(OWNER_OFF.into()));
        state.fetch_start(&settings, &other, "https://example.com");
        assert_eq!(command_answer(&mut state).2, Err(OWNER_OFF.into()));
        assert_eq!(closed.polls.load(Ordering::SeqCst), 0);
        state.command(&settings, "nobody", Command::PauseAll);
        assert_eq!(
            command_answer(&mut state).2,
            Err("This server is not paired".into())
        );

        // A server that stops answering keeps its rows with one status line.
        fake.down.store(true, Ordering::SeqCst);
        let started = Instant::now();
        loop {
            state.poll_transfers(&settings, true);
            state.drain();
            let machine = state.machine_transfers(&settings).remove(0);
            if let Some(problem) = machine.problem {
                assert_eq!(problem, "The server did not answer");
                assert_eq!(machine.transfers.len(), 1);
                break;
            }
            assert!(started.elapsed() < Duration::from_secs(20), "timed out");
            thread::sleep(Duration::from_millis(20));
        }

        state.remove_server(&mut settings, &mini);
        assert!(state.machine_transfers(&settings).is_empty());
    }

    #[test]
    fn refusals_end_without_asking_again_and_unreadable_starts_are_errors() {
        const SHARED: &str = "This is a share of the owner's files; remove it with Keep Files";
        let port = loopback_server(|method, path| {
            let json = "application/json";
            match (method, path) {
                ("POST", "/v1/transfers") => (201, json, b"<html>".to_vec()),
                ("DELETE", "/v1/transfers/gone") => (404, json, error_body("gone")),
                ("DELETE", "/v1/transfers/share") => (403, json, error_body(SHARED)),
                ("DELETE", "/v1/transfers/held") => (409, json, b"{}".to_vec()),
                _ => (404, json, error_body("gone")),
            }
        });
        let data = tempfile::tempdir().unwrap();
        let mut state = RemoteState::with_data_dir(data.path());
        let mut settings = Settings::default();
        let id = state
            .add_pairing(
                &mut settings,
                &format!("flick-remote://127.0.0.1:{port}/#{TOKEN}"),
            )
            .unwrap();
        let answer = |state: &mut RemoteState, command: Command| {
            state.command(&settings, &id, command.clone());
            loop {
                let (_, answered, result) = command_answer(state);
                if answered == command {
                    return result;
                }
            }
        };
        let start = Command::Start {
            input: "magnet:?xt=urn:btih:abc".into(),
            files: vec![0],
        };
        let Err(message) = answer(&mut state, start) else {
            panic!("an unreadable start answer is an error");
        };
        assert!(message.starts_with("The server sent an unreadable answer"));
        let remove = |id: &str, delete_files: bool| Command::Remove {
            id: id.into(),
            delete_files,
            confirm: vec!["a".into()],
        };
        // Keep Files on a transfer that is already gone is done.
        assert_eq!(answer(&mut state, remove("gone", false)), Ok(Outcome::Done));
        assert_eq!(answer(&mut state, remove("gone", true)), Err("gone".into()));
        assert_eq!(
            answer(&mut state, remove("share", true)),
            Err(SHARED.into())
        );
        assert_eq!(
            answer(&mut state, remove("held", true)),
            Err("The server deleted nothing. Refresh and try again".into())
        );
    }

    /// Reads one request's head and body; None when the client left first.
    fn read_request(reader: &mut BufReader<std::net::TcpStream>) -> Option<(String, String)> {
        let mut line = String::new();
        reader.read_line(&mut line).ok()?;
        let mut words = line.split_whitespace();
        let method = words.next()?.to_owned();
        let path = words.next()?.to_owned();
        let mut length = 0;
        loop {
            let mut header = String::new();
            reader.read_line(&mut header).ok()?;
            if header.trim().is_empty() {
                break;
            }
            if let Some((name, value)) = header.split_once(':')
                && name.eq_ignore_ascii_case("content-length")
            {
                length = value.trim().parse().ok()?;
            }
        }
        let mut body = vec![0; length];
        reader.read_exact(&mut body).ok()?;
        Some((method, path))
    }

    #[test]
    fn cancelling_a_resolve_closes_its_connection() {
        let (events, seen) = mpsc::channel::<String>();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let events = events.clone();
                thread::spawn(move || {
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let Some((method, path)) = read_request(&mut reader) else {
                        return;
                    };
                    if (method.as_str(), path.as_str()) == ("POST", "/v1/transfers/resolve") {
                        // Hold the lookup open, as a server finding peers does,
                        // until the client closes the connection.
                        let _ = events.send("resolving".into());
                        let mut rest = Vec::new();
                        let _ = reader.read_to_end(&mut rest);
                        let _ = events.send("closed".into());
                        return;
                    }
                    let code = if method == "DELETE" { 204 } else { 404 };
                    let _ = events.send(format!("{method} {path}"));
                    let _ = write!(
                        stream,
                        "HTTP/1.1 {code} X\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    );
                });
            }
        });
        let next = |want: &str| loop {
            let event = seen.recv_timeout(Duration::from_secs(10)).expect(want);
            if event == want {
                return;
            }
        };
        let data = tempfile::tempdir().unwrap();
        let mut state = RemoteState::with_data_dir(data.path());
        let mut settings = Settings::default();
        let id = state
            .add_pairing(
                &mut settings,
                &format!("flick-remote://127.0.0.1:{port}/#{TOKEN}"),
            )
            .unwrap();
        state.resolve(&settings, &id, "magnet:?xt=urn:btih:abc".into());
        next("resolving");
        state.cancel_resolve(&settings, &id);
        next("closed");
        next("DELETE /v1/transfers/resolve");
        assert_eq!(
            command_answer(&mut state).1,
            Command::CancelResolve,
            "only the cancel is answered"
        );

        // A newer resolve on the same server cancels the older one, and
        // removing the server cancels its resolve.
        state.resolve(&settings, &id, "magnet:?xt=urn:btih:abc".into());
        next("resolving");
        state.resolve(&settings, &id, "magnet:?xt=urn:btih:def".into());
        // The old connection closes and the new one opens, in either order.
        let mut pending = vec!["closed", "resolving"];
        while !pending.is_empty() {
            let event = seen.recv_timeout(Duration::from_secs(10)).expect("both");
            pending.retain(|want| *want != event);
        }
        state.remove_server(&mut settings, &id);
        next("closed");
        thread::sleep(Duration::from_millis(100));
        state.drain();
        assert!(
            state.take_ready().is_empty(),
            "cancelled resolves are not answered"
        );
    }
}
