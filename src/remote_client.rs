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

use crate::store::{Settings, remote::RemoteServer};
use flick::remote::protocol::{self, ErrorBody, Library, Pairing, Progress, Quality};
use std::{
    cell::{Cell, OnceCell, RefCell},
    collections::{HashMap, HashSet, VecDeque},
    fmt,
    path::{Path, PathBuf},
    sync::{Arc, OnceLock, mpsc},
    thread,
    time::Duration,
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
                let request = remote.begin_refresh(server, &cache);
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
        let request = inner.sync(server).begin_refresh(server, &cache);
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
            quality: playable_quality(server.quality, &listing.library),
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
    fn begin_refresh(&mut self, server: &RemoteServer, cache: &Path) -> Option<Request> {
        let endpoint = self.endpoint.clone()?;
        self.requested = true;
        self.refreshing = true;
        self.generation += 1;
        Some(Request::Library {
            endpoint,
            server_id: server.id.clone(),
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
            503 => "The server is busy. Try again shortly".into(),
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
        assert_eq!(
            status_for_http(503, b""),
            Status::Failed("The server is busy. Try again shortly".into())
        );
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
        assert_eq!(
            check("v2", Quality::Data480),
            Err("The server is busy. Try again shortly".into())
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
        }));
        assert!(!state.servers(&settings)[0].status.is_online());

        // A state shut down before any request never starts one.
        let mut fresh = RemoteState::with_data_dir(data.path());
        fresh.shutdown();
        fresh.servers(&settings);
        assert!(fresh.worker.get().is_none());
    }
}
