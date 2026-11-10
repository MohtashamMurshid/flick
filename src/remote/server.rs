//! The HTTP server: listing, media with ranges, posters, subtitles, hash,
//! and progress. Transcode routes come from [`crate::remote::hls`].
//!
//! The types below are the contract the `flick-serve` binary and the
//! desktop's in-app switch build against.
//!
//! Every route, including unknown paths, sits behind the bearer-token layer.
//! Every error is a JSON [`ErrorBody`]. Media is streamed from disk through
//! a bounded buffer; nothing reads a whole video into memory.

pub use crate::remote::library::Sources;

use crate::remote::{
    auth,
    hls::{HlsState, ResolvedVideo, Resolver},
    ids::{self, Salt},
    library::{self, Entry},
    protocol::{self, ErrorBody, Library, Progress, SubtitleFile, Video},
    transcode::{self, Encoder, MediaInfo},
};
use anyhow::{Context, Result, bail};
use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::{Path as UrlPath, Request, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
};
use futures_util::StreamExt;
use std::{
    collections::HashMap,
    io::SeekFrom,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, RwLock},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncReadExt, AsyncSeekExt},
    sync::watch,
};
use tokio_util::io::ReaderStream;

/// Read buffer per media response; bounds memory per stream.
const STREAM_BUFFER: usize = 64 * 1024;
/// An unknown id triggers a rebuild at most this often.
const REBUILD_ON_MISS_INTERVAL: Duration = Duration::from_secs(1);
/// How long [`Server::shutdown`] waits for connections to close.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);
/// Progress older than the saved value by more than this is not applied.
const PROGRESS_CONFLICT_WINDOW: u64 = 60;
const PROGRESS_FILE: &str = "progress.json";
const LOCK_FILE: &str = "serve.lock";
/// How long a busy state folder lock is retried.
const LOCK_WAIT: Duration = Duration::from_secs(2);
/// Transcode job folders live here, inside the state folder.
const TRANSCODE_DIR: &str = "transcode";
/// ffprobe runs at once while the library is being inspected.
const INSPECT_CONCURRENCY: usize = 2;

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
    hls: Arc<HlsState>,
    stop: watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
    /// Held while serving so two servers cannot share a state directory.
    _lock: std::fs::File,
}

impl Server {
    pub async fn start(config: ServerConfig) -> Result<Server> {
        if let Some(reason) = crate::remote::bind::refusal(config.bind.ip()) {
            bail!(reason);
        }
        anyhow::ensure!(
            auth::is_valid_token(&config.token),
            "The pairing token must be {} lowercase hex characters",
            protocol::TOKEN_BYTES * 2
        );
        std::fs::create_dir_all(&config.state_dir)
            .with_context(|| format!("Create {}", config.state_dir.display()))?;
        let lock = lock_state_dir(&config.state_dir).await?;
        let salt = Salt::load_or_create(&config.state_dir)?;
        let (stop, stopped) = watch::channel(false);
        // Quality tiers below Original need FFmpeg; without it the server
        // still serves Original and says so in the listing.
        let encoder = transcode::probe(config.ffmpeg.as_deref()).await;
        let media = Arc::new(MediaCache::new(encoder.clone()));
        let index: Arc<RwLock<Arc<Index>>> = Arc::new(RwLock::new(Arc::new(Index::default())));
        let transcode_dir = config.state_dir.join(TRANSCODE_DIR);
        std::fs::create_dir_all(&transcode_dir)
            .with_context(|| format!("Create {}", transcode_dir.display()))?;
        let resolver: Resolver = {
            let index = index.clone();
            let media = media.clone();
            Arc::new(move |id: &str| {
                let entry = index
                    .read()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .by_id
                    .get(id)?
                    .clone();
                if !entry.complete {
                    return None;
                }
                // A known video stays resolvable without FFmpeg or when
                // ffprobe fails; the transcoder then explains why it cannot
                // proceed instead of answering as if the video did not exist.
                let info = media.resolve_blocking(&entry.path).unwrap_or_default();
                Some(ResolvedVideo {
                    path: entry.path.clone(),
                    info,
                })
            })
        };
        let hls = Arc::new(HlsState::new(
            encoder,
            config.max_transcodes,
            transcode_dir,
            resolver,
        ));
        let state = Arc::new(AppState {
            token: config.token,
            name: config.name,
            sources: config.sources,
            poster_cache: config.poster_cache,
            state_dir: config.state_dir,
            salt,
            index,
            media,
            hls: hls.clone(),
            rebuild: tokio::sync::Mutex::new(()),
            progress: tokio::sync::Mutex::new(()),
            stopped,
        });
        // A source that cannot be read at all is a configuration mistake;
        // say so now rather than serving an empty library.
        let entries = library::load(&state.sources, state.poster_cache.as_deref()).await?;
        state.install(entries);

        let listener = tokio::net::TcpListener::bind(config.bind)
            .await
            .with_context(|| format!("Listen on {}", config.bind))?;
        let addr = listener.local_addr()?;
        let app = router(state.clone());
        let mut stopped = state.stopped.clone();
        let task = tokio::spawn(async move {
            let serve = axum::serve(listener, app).with_graceful_shutdown(async move {
                let _ = stopped.wait_for(|stop| *stop).await;
            });
            if let Err(error) = serve.await {
                tracing::warn!("Remote library server stopped: {error}");
            }
        });
        tracing::info!(%addr, videos = state.current().order.len(), "Remote library server listening");
        Ok(Server {
            addr,
            hls,
            stop,
            task,
            _lock: lock,
        })
    }

    /// The bound address, with the real port when 0 was requested.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Stops accepting connections, ends media streams in progress, and
    /// waits briefly for connections to close.
    pub async fn shutdown(self) {
        let _ = self.stop.send(true);
        self.hls.shutdown().await;
        let mut task = self.task;
        if tokio::time::timeout(SHUTDOWN_GRACE, &mut task)
            .await
            .is_err()
        {
            task.abort();
            let _ = task.await;
        }
    }
}

/// Takes the state folder's lock. A lock just released by a server in this
/// process can stay held for a moment while a child process is being
/// spawned elsewhere (it shares the descriptor until it execs), so a busy
/// lock is retried briefly before it is reported.
async fn lock_state_dir(state_dir: &Path) -> Result<std::fs::File> {
    use fs2::FileExt;
    let path = state_dir.join(LOCK_FILE);
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| format!("Open {}", path.display()))?;
    let deadline = Instant::now() + LOCK_WAIT;
    loop {
        match file.try_lock_exclusive() {
            Ok(()) => return Ok(file),
            Err(_) if Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "Another Flick server is already using {}",
                        state_dir.display()
                    )
                });
            }
        }
    }
}

struct AppState {
    token: String,
    name: String,
    sources: Sources,
    poster_cache: Option<PathBuf>,
    state_dir: PathBuf,
    salt: Salt,
    /// Shared with the transcoder's resolver, which runs outside this state.
    index: Arc<RwLock<Arc<Index>>>,
    /// What ffprobe knows about each file; fills the listing and the transcoder.
    media: Arc<MediaCache>,
    hls: Arc<HlsState>,
    /// Serializes rebuilds so concurrent listings do not scan twice at once.
    rebuild: tokio::sync::Mutex<()>,
    /// Serializes progress file updates.
    progress: tokio::sync::Mutex<()>,
    stopped: watch::Receiver<bool>,
}

#[derive(Default)]
struct Index {
    by_id: HashMap<String, Arc<Entry>>,
    order: Vec<String>,
    built: Option<Instant>,
}

impl AppState {
    fn current(&self) -> Arc<Index> {
        self.index
            .read()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
    }

    fn install(&self, entries: Vec<Entry>) {
        let mut index = Index {
            built: Some(Instant::now()),
            ..Index::default()
        };
        for entry in entries {
            let id = ids::video_id(&self.salt, &entry.path);
            if index.by_id.insert(id.clone(), Arc::new(entry)).is_none() {
                index.order.push(id);
            }
        }
        *self
            .index
            .write()
            .unwrap_or_else(|poison| poison.into_inner()) = Arc::new(index);
        self.media.prefetch(self.current(), self.stopped.clone());
    }

    /// Rebuilds the index from the sources. A failed read keeps the last
    /// good index, so a settings file caught mid-write does not empty the
    /// library.
    async fn refresh(&self) -> Arc<Index> {
        let _guard = self.rebuild.lock().await;
        match library::load(&self.sources, self.poster_cache.as_deref()).await {
            Ok(entries) => self.install(entries),
            Err(error) => tracing::warn!("Keeping the previous library: {error:#}"),
        }
        self.current()
    }

    async fn lookup(&self, id: &str) -> Option<Arc<Entry>> {
        if !ids::is_valid_id(id) {
            return None;
        }
        let index = self.current();
        if let Some(entry) = index.by_id.get(id) {
            return Some(entry.clone());
        }
        let stale = index
            .built
            .is_none_or(|built| built.elapsed() >= REBUILD_ON_MISS_INTERVAL);
        if !stale {
            return None;
        }
        self.refresh().await.by_id.get(id).cloned()
    }

    /// Resolves when the server is shutting down.
    fn stopping(&self) -> impl Future<Output = ()> + Send + 'static {
        let mut stopped = self.stopped.clone();
        async move {
            let _ = stopped.wait_for(|stop| *stop).await;
        }
    }
}

fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/v1/library", get(listing))
        .route("/v1/media/{id}", get(media).head(media))
        .route("/v1/poster/{id}", get(poster))
        .route("/v1/subtitles/{id}", get(subtitles))
        .route("/v1/subtitles/{id}/{index}", get(subtitle))
        .route(
            "/v1/subtitles/{id}/embedded/{track}",
            get(embedded_subtitle),
        )
        .route("/v1/hash/{id}", get(hash))
        .route("/v1/progress/{id}", get(get_progress).put(put_progress))
        .with_state(state.clone())
        .merge(crate::remote::hls::router(state.hls.clone()))
        .fallback(no_route)
        .method_not_allowed_fallback(method_not_allowed)
        .layer(middleware::from_fn_with_state(state, require_token))
}

async fn require_token(
    State(state): State<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Response {
    let presented = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(auth::bearer_token);
    match presented {
        Some(token) if auth::token_matches(&state.token, token) => next.run(request).await,
        _ => {
            let mut response = error(
                StatusCode::UNAUTHORIZED,
                "This Flick is not paired with the server. Pair again with the server's pairing string.",
            );
            response
                .headers_mut()
                .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
            response
        }
    }
}

fn error(status: StatusCode, message: impl Into<String>) -> Response {
    (
        status,
        Json(ErrorBody {
            message: message.into(),
        }),
    )
        .into_response()
}

fn unknown_video() -> Response {
    error(
        StatusCode::NOT_FOUND,
        "The server has no video with this id. Refresh the library.",
    )
}

fn incomplete_video() -> Response {
    error(
        StatusCode::CONFLICT,
        "This video is still downloading on the server and has not been verified yet.",
    )
}

async fn no_route() -> Response {
    error(StatusCode::NOT_FOUND, "Not found")
}

async fn method_not_allowed() -> Response {
    error(StatusCode::METHOD_NOT_ALLOWED, "Method not allowed")
}

async fn listing(State(state): State<Arc<AppState>>) -> Response {
    let index = state.refresh().await;
    let media = state.media.clone();
    let videos = tokio::task::spawn_blocking(move || {
        index
            .order
            .iter()
            .filter_map(|id| Some(describe(id, index.by_id.get(id)?, &media)))
            .collect::<Vec<_>>()
    })
    .await;
    match videos {
        Ok(videos) => Json(Library {
            protocol: protocol::VERSION,
            server_name: state.name.clone(),
            qualities: state.hls.qualities(),
            videos,
        })
        .into_response(),
        Err(_) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "The library could not be listed",
        ),
    }
}

/// The listing entry for a video. What the source knows (the desktop's
/// saved duration and size) wins; ffprobe's view fills the gaps once the
/// file has been inspected.
fn describe(id: &str, entry: &Entry, media: &MediaCache) -> Video {
    let info = media.get(&entry.path);
    let or_probed = |saved: f64, probed: fn(&MediaInfo) -> f64| {
        if saved > 0.0 {
            saved
        } else {
            info.as_ref().map_or(0.0, probed)
        }
    };
    let or_probed_u32 = |saved: u32, probed: fn(&MediaInfo) -> u32| {
        if saved > 0 {
            saved
        } else {
            info.as_ref().map_or(0, probed)
        }
    };
    Video {
        id: id.to_owned(),
        title: entry.title.clone(),
        file_name: library::file_title(&entry.path),
        size: std::fs::metadata(&entry.path).map_or(0, |m| m.len()),
        duration: or_probed(entry.duration, |info| info.duration),
        width: or_probed_u32(entry.width, |info| info.width),
        height: or_probed_u32(entry.height, |info| info.height),
        mime: library::video_mime(&entry.path).to_owned(),
        episode: crate::library::parse_episode(&entry.path).map(|episode| protocol::Episode {
            show: episode.show,
            season: episode.season,
            number: episode.episode,
        }),
        complete: entry.complete,
        poster: entry.poster.is_some(),
        subtitles: subtitle_files(&entry.path),
        embedded_subtitles: info.map(|info| info.subtitles).unwrap_or_default(),
    }
}

/// ffprobe results per file, checked against size and modification time,
/// so the listing and the transcoder describe the same bytes. Without
/// FFmpeg nothing is inspected and the listing shows what the source saved.
struct MediaCache {
    encoder: Option<Encoder>,
    entries: Mutex<HashMap<PathBuf, (Stamp, MediaInfo)>>,
    inspecting: tokio::sync::Semaphore,
}

type Stamp = (u64, Option<SystemTime>);

impl MediaCache {
    fn new(encoder: Option<Encoder>) -> Self {
        Self {
            encoder,
            entries: Mutex::new(HashMap::new()),
            inspecting: tokio::sync::Semaphore::new(INSPECT_CONCURRENCY),
        }
    }

    fn stamp(path: &Path) -> Option<Stamp> {
        let metadata = std::fs::metadata(path).ok()?;
        Some((metadata.len(), metadata.modified().ok()))
    }

    /// What is known about a file right now, if it has not changed since.
    fn get(&self, path: &Path) -> Option<MediaInfo> {
        let stamp = Self::stamp(path)?;
        let entries = self
            .entries
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        entries
            .get(path)
            .filter(|(saved, _)| *saved == stamp)
            .map(|(_, info)| info.clone())
    }

    /// The cached result, or one ffprobe run. None without FFmpeg, or when
    /// the file cannot be read or inspected.
    async fn inspect(&self, path: &Path) -> Option<MediaInfo> {
        if let Some(info) = self.get(path) {
            return Some(info);
        }
        let encoder = self.encoder.as_ref()?;
        let _permit = self.inspecting.acquire().await.ok()?;
        if let Some(info) = self.get(path) {
            return Some(info);
        }
        let stamp = Self::stamp(path)?;
        match transcode::inspect(encoder, path).await {
            Ok(info) => {
                self.entries
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .insert(path.to_owned(), (stamp, info.clone()));
                Some(info)
            }
            Err(error) => {
                tracing::warn!(video = %path.display(), "Could not inspect the video: {error:#}");
                None
            }
        }
    }

    /// [`Self::inspect`] for the transcoder's synchronous resolver. On a
    /// multi-threaded runtime an uninspected file is probed in place; on a
    /// single-threaded one only the cache answers.
    fn resolve_blocking(&self, path: &Path) -> Option<MediaInfo> {
        if let Some(info) = self.get(path) {
            return Some(info);
        }
        let handle = tokio::runtime::Handle::try_current().ok()?;
        if handle.runtime_flavor() != tokio::runtime::RuntimeFlavor::MultiThread {
            return None;
        }
        tokio::task::block_in_place(|| handle.block_on(self.inspect(path)))
    }

    /// Inspects every listed file in the background, a few at a time, so
    /// durations, sizes, and embedded subtitles reach the listing and a
    /// transcode can start without waiting for ffprobe.
    fn prefetch(self: &Arc<Self>, index: Arc<Index>, stopped: watch::Receiver<bool>) {
        if self.encoder.is_none() {
            return;
        }
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let cache = self.clone();
        handle.spawn(async move {
            for id in &index.order {
                if *stopped.borrow() {
                    break;
                }
                let Some(entry) = index.by_id.get(id) else {
                    continue;
                };
                if cache.get(&entry.path).is_none() {
                    cache.inspect(&entry.path).await;
                }
            }
        });
    }
}

fn subtitle_files(video: &Path) -> Vec<SubtitleFile> {
    library::sidecar_subtitles(video)
        .into_iter()
        .enumerate()
        .map(|(index, sidecar)| SubtitleFile {
            index: index as u32,
            file_name: sidecar.file_name,
            language: sidecar.language,
        })
        .collect()
}

/// A single byte range, inclusive at both ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ByteRange {
    pub start: u64,
    pub end: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub enum RangeRequest {
    /// No range, or a unit other than bytes: send the whole file.
    Full,
    Partial(ByteRange),
    /// Several ranges, malformed, or outside the file: 416.
    Unsatisfiable,
}

/// Parses a `Range` header against a file of `size` bytes. Only one range
/// is served; several ranges are refused rather than answered as multipart.
pub fn parse_range(value: &str, size: u64) -> RangeRequest {
    let value = value.trim();
    let Some(spec) = value
        .get(..6)
        .filter(|unit| unit.eq_ignore_ascii_case("bytes="))
        .map(|_| &value[6..])
    else {
        return RangeRequest::Full;
    };
    if spec.contains(',') {
        return RangeRequest::Unsatisfiable;
    }
    let Some((first, last)) = spec.split_once('-') else {
        return RangeRequest::Unsatisfiable;
    };
    let (first, last) = (first.trim(), last.trim());
    let number = |text: &str| {
        (!text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()))
            .then(|| text.parse::<u64>().ok())
            .flatten()
    };
    let range = if first.is_empty() {
        match number(last) {
            Some(suffix) if suffix > 0 && size > 0 => Some(ByteRange {
                start: size.saturating_sub(suffix),
                end: size - 1,
            }),
            _ => None,
        }
    } else {
        match (
            number(first),
            last.is_empty().then_some(u64::MAX).or_else(|| number(last)),
        ) {
            (Some(start), Some(end)) if start <= end && start < size => Some(ByteRange {
                start,
                end: end.min(size - 1),
            }),
            _ => None,
        }
    };
    range.map_or(RangeRequest::Unsatisfiable, RangeRequest::Partial)
}

fn etag(metadata: &std::fs::Metadata) -> String {
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |time| time.as_nanos());
    format!("\"{:x}-{modified:x}\"", metadata.len())
}

async fn media(
    State(state): State<Arc<AppState>>,
    UrlPath(id): UrlPath<String>,
    method: Method,
    headers: HeaderMap,
) -> Response {
    let Some(entry) = state.lookup(&id).await else {
        return unknown_video();
    };
    if !entry.complete {
        return incomplete_video();
    }
    let Ok(mut file) = tokio::fs::File::open(&entry.path).await else {
        return error(
            StatusCode::NOT_FOUND,
            "The video's file is no longer on the server",
        );
    };
    let Ok(metadata) = file.metadata().await else {
        return error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Read the video's details",
        );
    };
    let size = metadata.len();
    let tag = etag(&metadata);
    // If-Range: serve the range only when the client's copy is still this
    // exact file. Dates and weak tags never match, so they get the whole file.
    let range_applies = headers
        .get(header::IF_RANGE)
        .is_none_or(|value| value.to_str().is_ok_and(|value| value.trim() == tag));
    let request = match headers.get(header::RANGE) {
        Some(value) if range_applies => match value.to_str() {
            Ok(value) => parse_range(value, size),
            Err(_) => RangeRequest::Unsatisfiable,
        },
        _ => RangeRequest::Full,
    };
    let mut response = Response::builder()
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::ETAG, &tag)
        .header(header::CACHE_CONTROL, "private, no-transform");
    let range = match request {
        RangeRequest::Unsatisfiable => {
            let mut refused = error(
                StatusCode::RANGE_NOT_SATISFIABLE,
                "The server sends one byte range inside the file",
            );
            let headers = refused.headers_mut();
            headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
            if let Ok(value) = HeaderValue::from_str(&format!("bytes */{size}")) {
                headers.insert(header::CONTENT_RANGE, value);
            }
            return refused;
        }
        RangeRequest::Full => {
            response = response.status(StatusCode::OK);
            ByteRange {
                start: 0,
                end: size.saturating_sub(1),
            }
        }
        RangeRequest::Partial(range) => {
            response = response.status(StatusCode::PARTIAL_CONTENT).header(
                header::CONTENT_RANGE,
                format!("bytes {}-{}/{size}", range.start, range.end),
            );
            range
        }
    };
    let length = if size == 0 {
        0
    } else {
        range.end - range.start + 1
    };
    response = response
        .header(header::CONTENT_TYPE, library::video_mime(&entry.path))
        .header(header::CONTENT_LENGTH, length);
    let body = if method == Method::HEAD || length == 0 {
        Body::empty()
    } else {
        if range.start > 0 && file.seek(SeekFrom::Start(range.start)).await.is_err() {
            return error(StatusCode::INTERNAL_SERVER_ERROR, "Read the video");
        }
        let stream = ReaderStream::with_capacity(file.take(length), STREAM_BUFFER)
            .take_until(state.stopping());
        Body::from_stream(stream)
    };
    response
        .body(body)
        .unwrap_or_else(|_| error(StatusCode::INTERNAL_SERVER_ERROR, "Build the response"))
}

async fn poster(State(state): State<Arc<AppState>>, UrlPath(id): UrlPath<String>) -> Response {
    let Some(entry) = state.lookup(&id).await else {
        return unknown_video();
    };
    let Some(path) = &entry.poster else {
        return error(StatusCode::NOT_FOUND, "This video has no poster yet");
    };
    match tokio::fs::read(path).await {
        Ok(bytes) => ([(header::CONTENT_TYPE, "image/png")], bytes).into_response(),
        Err(_) => error(StatusCode::NOT_FOUND, "This video has no poster yet"),
    }
}

async fn subtitles(State(state): State<Arc<AppState>>, UrlPath(id): UrlPath<String>) -> Response {
    let Some(entry) = state.lookup(&id).await else {
        return unknown_video();
    };
    match tokio::task::spawn_blocking(move || subtitle_files(&entry.path)).await {
        Ok(files) => Json(files).into_response(),
        Err(_) => error(StatusCode::INTERNAL_SERVER_ERROR, "List subtitles"),
    }
}

async fn subtitle(
    State(state): State<Arc<AppState>>,
    UrlPath((id, index)): UrlPath<(String, String)>,
) -> Response {
    let Some(entry) = state.lookup(&id).await else {
        return unknown_video();
    };
    let Ok(index) = index.parse::<usize>() else {
        return error(StatusCode::NOT_FOUND, "No subtitle file with this index");
    };
    let path = entry.path.clone();
    let sidecars = tokio::task::spawn_blocking(move || library::sidecar_subtitles(&path))
        .await
        .unwrap_or_default();
    let Some(sidecar) = sidecars.into_iter().nth(index) else {
        return error(StatusCode::NOT_FOUND, "No subtitle file with this index");
    };
    match tokio::fs::read(&sidecar.path).await {
        Ok(bytes) => (
            [(
                header::CONTENT_TYPE,
                library::subtitle_mime(&sidecar.extension),
            )],
            bytes,
        )
            .into_response(),
        Err(_) => error(StatusCode::NOT_FOUND, "The subtitle file is gone"),
    }
}

/// A subtitle stream inside the file, extracted as WebVTT by FFmpeg.
async fn embedded_subtitle(
    State(state): State<Arc<AppState>>,
    UrlPath((id, track)): UrlPath<(String, String)>,
) -> Response {
    let Some(entry) = state.lookup(&id).await else {
        return unknown_video();
    };
    if !entry.complete {
        return incomplete_video();
    }
    let Ok(track) = track.parse::<u32>() else {
        return error(StatusCode::NOT_FOUND, "No subtitle track with this index");
    };
    let Some(encoder) = state.hls.encoder() else {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "This server has no FFmpeg, so subtitles inside the file cannot be extracted",
        );
    };
    let Some(info) = state.media.inspect(&entry.path).await else {
        return error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "The video could not be inspected",
        );
    };
    match info
        .subtitles
        .iter()
        .find(|subtitle| subtitle.track == track)
    {
        None => return error(StatusCode::NOT_FOUND, "No subtitle track with this index"),
        Some(subtitle) if !subtitle.text => {
            return error(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "This subtitle track is bitmap, which cannot be sent as text",
            );
        }
        Some(_) => {}
    }
    match transcode::extract_webvtt(encoder, &entry.path, track).await {
        Ok(bytes) => ([(header::CONTENT_TYPE, "text/vtt")], bytes).into_response(),
        Err(problem) => error(StatusCode::BAD_GATEWAY, format!("{problem:#}")),
    }
}

/// `{"hash": "<16 hex>"}`, the OpenSubtitles movie hash.
async fn hash(State(state): State<Arc<AppState>>, UrlPath(id): UrlPath<String>) -> Response {
    let Some(entry) = state.lookup(&id).await else {
        return unknown_video();
    };
    if !entry.complete {
        return incomplete_video();
    }
    let path = entry.path.clone();
    match tokio::task::spawn_blocking(move || crate::subtitles::movie_hash(&path)).await {
        Ok(Ok(hash)) => Json(serde_json::json!({ "hash": hash })).into_response(),
        Ok(Err(problem)) => error(StatusCode::UNPROCESSABLE_ENTITY, format!("{problem:#}")),
        Err(_) => error(StatusCode::INTERNAL_SERVER_ERROR, "Hash the video"),
    }
}

fn progress_path(state: &AppState) -> PathBuf {
    state.state_dir.join(PROGRESS_FILE)
}

async fn read_progress(path: &Path) -> HashMap<String, Progress> {
    match tokio::fs::read(path).await {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|error| {
            tracing::warn!("Ignoring unreadable {}: {error}", path.display());
            HashMap::new()
        }),
        Err(_) => HashMap::new(),
    }
}

async fn get_progress(
    State(state): State<Arc<AppState>>,
    UrlPath(id): UrlPath<String>,
) -> Response {
    if state.lookup(&id).await.is_none() {
        return unknown_video();
    }
    let _guard = state.progress.lock().await;
    match read_progress(&progress_path(&state)).await.remove(&id) {
        Some(progress) => Json(progress).into_response(),
        None => error(StatusCode::NOT_FOUND, "No progress saved for this video"),
    }
}

/// Saves progress and answers with what is now stored. A value older than
/// the stored one by more than a minute is not applied, so a client that
/// was offline does not rewind another client's newer position.
async fn put_progress(
    State(state): State<Arc<AppState>>,
    UrlPath(id): UrlPath<String>,
    body: Bytes,
) -> Response {
    if state.lookup(&id).await.is_none() {
        return unknown_video();
    }
    let mut progress: Progress = match serde_json::from_slice(&body) {
        Ok(progress) => progress,
        Err(problem) => {
            return error(
                StatusCode::BAD_REQUEST,
                format!("The progress is not valid JSON: {problem}"),
            );
        }
    };
    if !(progress.position.is_finite()
        && progress.position >= 0.0
        && progress.duration.is_finite()
        && progress.duration >= 0.0)
    {
        return error(
            StatusCode::BAD_REQUEST,
            "Position and duration must be non-negative seconds",
        );
    }
    if progress.updated == 0 {
        progress.updated = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |time| time.as_secs());
    }
    let _guard = state.progress.lock().await;
    let path = progress_path(&state);
    let mut saved = read_progress(&path).await;
    if let Some(existing) = saved.get(&id)
        && existing.updated > progress.updated.saturating_add(PROGRESS_CONFLICT_WINDOW)
    {
        return Json(existing.clone()).into_response();
    }
    saved.insert(id, progress.clone());
    let written = serde_json::to_vec_pretty(&saved)
        .map_err(anyhow::Error::from)
        .and_then(|bytes| auth::write_private(&path, &bytes));
    match written {
        Ok(()) => Json(progress).into_response(),
        Err(problem) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Save progress: {problem:#}"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn partial(start: u64, end: u64) -> RangeRequest {
        RangeRequest::Partial(ByteRange { start, end })
    }

    #[test]
    fn ranges_follow_rfc_9110_for_one_range() {
        assert_eq!(parse_range("bytes=0-9", 100), partial(0, 9));
        assert_eq!(parse_range("bytes=90-", 100), partial(90, 99));
        assert_eq!(parse_range("bytes=99-99", 100), partial(99, 99));
        assert_eq!(parse_range("bytes=-10", 100), partial(90, 99));
        assert_eq!(parse_range("bytes=-1000", 100), partial(0, 99));
        assert_eq!(parse_range("bytes=50-1000", 100), partial(50, 99));
        assert_eq!(parse_range("BYTES= 1 - 2 ", 100), partial(1, 2));
        assert_eq!(parse_range("items=0-1", 100), RangeRequest::Full);
        for refused in [
            "bytes=0-1,5-6",
            "bytes=100-",
            "bytes=100-200",
            "bytes=5-4",
            "bytes=-0",
            "bytes=-",
            "bytes=a-b",
            "bytes=+1-2",
            "bytes=0",
        ] {
            assert_eq!(
                parse_range(refused, 100),
                RangeRequest::Unsatisfiable,
                "{refused}"
            );
        }
        assert_eq!(parse_range("bytes=0-", 0), RangeRequest::Unsatisfiable);
        assert_eq!(parse_range("bytes=-1", 0), RangeRequest::Unsatisfiable);
    }
}
