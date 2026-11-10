//! "Keep on this Mac": copy a remote video to this computer for travel, show
//! the copy in the library, and remove it later. Owned by the
//! desktop-modules agent; the library view wires the menu items.
//!
//! The copy is streamed from `GET /v1/media/{id}` with the bearer header
//! into `<download folder>/From <server name>/<file name>` through a part
//! file named by server id and video id, with progress, cancel, and resume
//! by range on retry. Beside the part file a small JSON file records the
//! size and the server's `ETag`/`Last-Modified`; a resume sends `If-Range`
//! and starts over when either changed. The final name is chosen only at
//! the rename. On completion the integration agent adds the path to the
//! library and records a `store::remote::KeptCopy`.

use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, VecDeque},
    path::{Path, PathBuf},
    sync::{Arc, mpsc},
    thread,
    time::{Duration, Instant},
};
use tokio::io::AsyncWriteExt;
use tokio_util::sync::CancellationToken;

/// One copy in progress.
#[derive(Clone, Debug, PartialEq)]
pub struct KeepProgress {
    pub server_id: String,
    pub video_id: String,
    pub downloaded: u64,
    pub total: Option<u64>,
    pub bytes_per_second: u64,
}

/// What the view learns from `drain`.
#[derive(Clone, Debug, PartialEq)]
pub enum KeepEvent {
    Progress(KeepProgress),
    Done {
        server_id: String,
        video_id: String,
        path: PathBuf,
    },
    Failed {
        server_id: String,
        video_id: String,
        message: String,
    },
    Cancelled {
        server_id: String,
        video_id: String,
    },
}

/// What to fetch and where.
#[derive(Clone, PartialEq)]
pub struct KeepRequest {
    pub server_id: String,
    pub server_name: String,
    pub video_id: String,
    pub file_name: String,
    pub url: String,
    /// `Authorization: Bearer …`; never logged.
    pub authorization: String,
    pub download_dir: PathBuf,
}

/// The authorization header is left out, so a logged request never reveals
/// the token.
impl std::fmt::Debug for KeepRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeepRequest")
            .field("server_id", &self.server_id)
            .field("server_name", &self.server_name)
            .field("video_id", &self.video_id)
            .field("file_name", &self.file_name)
            .field("url", &self.url)
            .field("authorization", &"<redacted>")
            .field("download_dir", &self.download_dir)
            .finish()
    }
}

/// Copies running at once; more wait their turn.
const IN_FLIGHT: usize = 2;
/// How often a running copy reports progress.
const PROGRESS_EVERY: Duration = Duration::from_millis(250);
/// The window the transfer rate is measured over.
const RATE_WINDOW: Duration = Duration::from_secs(3);
/// Connecting, and silence mid-copy, longer than this fails the copy.
const STALL_TIMEOUT: Duration = Duration::from_secs(30);
/// How long `shutdown` waits for copies to clean up.
const SHUTDOWN_WAIT: Duration = Duration::from_secs(3);

type Key = (String, String);

enum Request {
    Keep(Box<KeepRequest>, CancellationToken),
}

struct Worker {
    requests: tokio::sync::mpsc::UnboundedSender<Request>,
    events: mpsc::Receiver<KeepEvent>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Worker {
    fn start() -> Self {
        let (requests, mut receiver) = tokio::sync::mpsc::unbounded_channel::<Request>();
        let (sender, events) = mpsc::channel();
        let spawned = thread::Builder::new()
            .name("flick-keep".into())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        tracing::warn!(%error, "Keeping copies could not start");
                        return;
                    }
                };
                runtime.block_on(async move {
                    let http = match http_client() {
                        Ok(http) => http,
                        Err(error) => {
                            tracing::warn!(%error, "Keeping copies could not start");
                            return;
                        }
                    };
                    let slots = Arc::new(tokio::sync::Semaphore::new(IN_FLIGHT));
                    let mut running = tokio::task::JoinSet::new();
                    while let Some(Request::Keep(request, cancel)) = receiver.recv().await {
                        let (http, slots, sender) = (http.clone(), slots.clone(), sender.clone());
                        running.spawn(async move {
                            let event = run(&http, &slots, *request, cancel, &sender).await;
                            let _ = sender.send(event);
                        });
                        // Reap finished copies so the set does not grow.
                        while running.try_join_next().is_some() {}
                    }
                    // The state shut down and cancelled everything: let each
                    // copy remove its part file before the runtime stops.
                    let _ = tokio::time::timeout(SHUTDOWN_WAIT, async {
                        while running.join_next().await.is_some() {}
                    })
                    .await;
                });
                runtime.shutdown_timeout(Duration::from_millis(200));
            });
        let thread = match spawned {
            Ok(thread) => Some(thread),
            Err(error) => {
                tracing::warn!(%error, "Keeping copies could not start");
                None
            }
        };
        Self {
            requests,
            events,
            thread,
        }
    }
}

fn http_client() -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        // A system proxy cannot reach tailnet addresses and must not see the token.
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(STALL_TIMEOUT)
        .read_timeout(STALL_TIMEOUT)
        .user_agent(concat!("Flick v", env!("CARGO_PKG_VERSION")))
        .build()
}

/// Owns a worker thread; the view holds one and drains it on tick.
#[derive(Default)]
pub struct KeepState {
    worker: Option<Worker>,
    /// Copies asked for and not yet finished, queued or running.
    active: HashMap<Key, (CancellationToken, KeepProgress)>,
    /// Ask order, so `in_progress` is stable.
    order: Vec<Key>,
}

impl KeepState {
    /// Starts a copy, or queues it behind the two already running. Does
    /// nothing when this video from this server is already being copied.
    pub fn keep(&mut self, request: KeepRequest) {
        let key = (request.server_id.clone(), request.video_id.clone());
        if self.active.contains_key(&key) {
            return;
        }
        let cancel = CancellationToken::new();
        let progress = KeepProgress {
            server_id: request.server_id.clone(),
            video_id: request.video_id.clone(),
            downloaded: 0,
            total: None,
            bytes_per_second: 0,
        };
        let worker = self.worker.get_or_insert_with(Worker::start);
        if worker
            .requests
            .send(Request::Keep(Box::new(request), cancel.clone()))
            .is_err()
        {
            tracing::warn!("Keeping copies is unavailable");
            return;
        }
        self.active.insert(key.clone(), (cancel, progress));
        self.order.push(key);
    }

    /// Stops a copy and removes its part file; `Cancelled` follows.
    pub fn cancel(&mut self, server_id: &str, video_id: &str) {
        if let Some((cancel, _)) = self
            .active
            .get(&(server_id.to_owned(), video_id.to_owned()))
        {
            cancel.cancel();
        }
    }

    /// Copies queued or running, in the order they were asked for, as of
    /// the last `drain`.
    pub fn in_progress(&self) -> Vec<KeepProgress> {
        self.order
            .iter()
            .filter_map(|key| self.active.get(key))
            .map(|(_, progress)| progress.clone())
            .collect()
    }

    pub fn drain(&mut self) -> Vec<KeepEvent> {
        let Some(worker) = &self.worker else {
            return Vec::new();
        };
        let events: Vec<_> = worker.events.try_iter().collect();
        for event in &events {
            match event {
                KeepEvent::Progress(progress) => {
                    let key = (progress.server_id.clone(), progress.video_id.clone());
                    if let Some((_, current)) = self.active.get_mut(&key) {
                        *current = progress.clone();
                    }
                }
                KeepEvent::Done {
                    server_id,
                    video_id,
                    ..
                }
                | KeepEvent::Failed {
                    server_id,
                    video_id,
                    ..
                }
                | KeepEvent::Cancelled {
                    server_id,
                    video_id,
                } => {
                    let key = (server_id.clone(), video_id.clone());
                    self.active.remove(&key);
                    self.order.retain(|k| *k != key);
                }
            }
        }
        events
    }

    /// Cancels every copy, waits briefly for their part files to be
    /// removed, and stops the worker.
    pub fn shutdown(&mut self) {
        for (cancel, _) in self.active.values() {
            cancel.cancel();
        }
        self.active.clear();
        self.order.clear();
        if let Some(mut worker) = self.worker.take() {
            drop(worker.requests);
            if let Some(thread) = worker.thread.take() {
                let _ = thread.join();
            }
        }
    }
}

impl Drop for KeepState {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Why a copy stopped early.
enum Stop {
    Cancelled,
    Failed(String),
}

/// Waits for a slot, copies, and says how it ended.
async fn run(
    http: &reqwest::Client,
    slots: &tokio::sync::Semaphore,
    request: KeepRequest,
    cancel: CancellationToken,
    events: &mpsc::Sender<KeepEvent>,
) -> KeepEvent {
    let server_id = request.server_id.clone();
    let video_id = request.video_id.clone();
    let result = tokio::select! {
        _ = cancel.cancelled() => Err(Stop::Cancelled),
        permit = slots.acquire() => match permit {
            Ok(_permit) => copy(http, &request, &cancel, events).await,
            Err(_) => Err(Stop::Failed("Keeping copies stopped".into())),
        },
    };
    match result {
        Ok(path) => KeepEvent::Done {
            server_id,
            video_id,
            path,
        },
        Err(Stop::Cancelled) => {
            // Only this copy's own part file and its notes.
            let paths = destination(&request);
            let _ = tokio::fs::remove_file(&paths.part).await;
            let _ = tokio::fs::remove_file(&paths.info).await;
            KeepEvent::Cancelled {
                server_id,
                video_id,
            }
        }
        Err(Stop::Failed(message)) => KeepEvent::Failed {
            server_id,
            video_id,
            message,
        },
    }
}

/// Where one copy lives while it downloads.
#[derive(Debug, PartialEq)]
struct Paths {
    /// `<download dir>/From <server>/`.
    folder: PathBuf,
    /// `.<server id>-<video id>.part` inside the folder: two videos with the
    /// same file name, or two servers with the same name, never share one.
    part: PathBuf,
    /// The part file's [`PartInfo`], beside it as `….part.json`.
    info: PathBuf,
}

fn destination(request: &KeepRequest) -> Paths {
    let folder = request.download_dir.join(format!(
        "From {}",
        safe_component(&request.server_name, "Flick")
    ));
    let stem = format!(
        ".{}-{}",
        id_component(&request.server_id),
        id_component(&request.video_id)
    );
    Paths {
        part: folder.join(format!("{stem}.part")),
        info: folder.join(format!("{stem}.part.json")),
        folder,
    }
}

/// An id as it appears in a part file's name: kept when it is short and
/// only letters, digits, and underscores (so the `-` between the two ids is
/// unambiguous), else a stable hash of it.
fn id_component(id: &str) -> String {
    if !id.is_empty()
        && id.len() <= 64
        && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
    {
        return id.to_owned();
    }
    use std::hash::{Hash, Hasher};
    let mut hasher = std::hash::DefaultHasher::new();
    id.hash(&mut hasher);
    format!("h{:016x}", hasher.finish())
}

/// What the server said about the whole file when the part file was
/// started, so a resume continues the same file.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
struct PartInfo {
    total: Option<u64>,
    etag: Option<String>,
    last_modified: Option<String>,
}

impl PartInfo {
    fn of(response: &reqwest::Response, total: Option<u64>) -> Self {
        Self {
            total,
            etag: header(response, reqwest::header::ETAG),
            last_modified: header(response, reqwest::header::LAST_MODIFIED),
        }
    }

    /// `If-Range`: the strong `ETag` when there is one, else the date.
    fn if_range(&self) -> Option<&str> {
        self.etag
            .as_deref()
            .filter(|tag| !tag.starts_with("W/"))
            .or(self.last_modified.as_deref())
    }

    /// Whether a ranged answer continues the same file: the same size and
    /// the same validators.
    fn continues(&self, response: &reqwest::Response, total: Option<u64>) -> bool {
        self.total == total
            && self.etag == header(response, reqwest::header::ETAG)
            && self.last_modified == header(response, reqwest::header::LAST_MODIFIED)
    }
}

fn header(response: &reqwest::Response, name: reqwest::header::HeaderName) -> Option<String> {
    response
        .headers()
        .get(name)?
        .to_str()
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

/// The notes beside a part file; None when missing or unreadable.
async fn read_info(path: &Path) -> Option<PartInfo> {
    let bytes = tokio::fs::read(path).await.ok()?;
    serde_json::from_slice(&bytes).ok()
}

async fn remove_if_present(path: &Path) -> std::io::Result<()> {
    match tokio::fs::remove_file(path).await {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

/// The last component of the server's file name, made safe for a folder.
fn file_name(request: &KeepRequest) -> String {
    let last = request
        .file_name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default();
    safe_component(last, &request.video_id)
}

/// A single path component: no separators, control characters, or leading
/// dots; `fallback` when nothing is left.
fn safe_component(name: &str, fallback: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '\0' => '-',
            c if c.is_control() => ' ',
            c => c,
        })
        .collect();
    let cleaned = cleaned.trim().trim_start_matches('.').trim().to_owned();
    if cleaned.is_empty() {
        let fallback: String = fallback
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
            .collect();
        if fallback.is_empty() {
            "video".into()
        } else {
            fallback
        }
    } else {
        // Keep names within common file system limits (255 bytes), leaving
        // room for " (2)": at most 200 bytes, cut on a character boundary.
        truncate_bytes(&cleaned, 200).trim_end().to_owned()
    }
}

/// The longest prefix of `text` of at most `limit` bytes that ends on a
/// character boundary.
fn truncate_bytes(text: &str, limit: usize) -> &str {
    if text.len() <= limit {
        return text;
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// `name`, or `name (2).ext`, `name (3).ext`, … when taken.
fn unused_path(folder: &Path, name: &str) -> PathBuf {
    let first = folder.join(name);
    if !first.exists() {
        return first;
    }
    let (stem, extension) = match name.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() => (stem, format!(".{extension}")),
        _ => (name, String::new()),
    };
    (2..)
        .map(|n| folder.join(format!("{stem} ({n}){extension}")))
        .find(|path| !path.exists())
        .unwrap_or(first)
}

/// Measures bytes per second over a short sliding window.
struct Rate {
    samples: VecDeque<(Instant, u64)>,
}

impl Rate {
    fn new(now: Instant, bytes: u64) -> Self {
        Self {
            samples: VecDeque::from([(now, bytes)]),
        }
    }

    fn sample(&mut self, now: Instant, bytes: u64) -> u64 {
        self.samples.push_back((now, bytes));
        while self.samples.len() > 2
            && self
                .samples
                .get(1)
                .is_some_and(|(at, _)| now.duration_since(*at) >= RATE_WINDOW)
        {
            self.samples.pop_front();
        }
        let (start, from) = self.samples.front().copied().unwrap_or((now, bytes));
        let seconds = now.duration_since(start).as_secs_f64();
        if seconds <= 0.0 {
            0
        } else {
            (bytes.saturating_sub(from) as f64 / seconds) as u64
        }
    }
}

/// Streams the video into the part file, resuming from what is there, and
/// renames it when complete.
async fn copy(
    http: &reqwest::Client,
    request: &KeepRequest,
    cancel: &CancellationToken,
    events: &mpsc::Sender<KeepEvent>,
) -> Result<PathBuf, Stop> {
    let name = file_name(request);
    let server = request.server_name.trim();
    let server = if server.is_empty() {
        "The server"
    } else {
        server
    };
    let write_error = |error: std::io::Error| {
        Stop::Failed(format!("Could not save {name}: {}", io_reason(&error)))
    };
    let paths = destination(request);
    tokio::fs::create_dir_all(&paths.folder)
        .await
        .map_err(write_error)?;

    let mut restarted = false;
    let (mut response, mut offset, total) = loop {
        let size = match tokio::fs::metadata(&paths.part).await {
            Ok(meta) if meta.is_file() => meta.len(),
            _ => 0,
        };
        // A part file without its notes cannot be checked: start over.
        let saved = match size {
            0 => None,
            _ => read_info(&paths.info).await,
        };
        let offset = if saved.is_some() { size } else { 0 };
        let mut get = http
            .get(&request.url)
            .header(reqwest::header::AUTHORIZATION, &request.authorization);
        if let Some(saved) = &saved {
            get = get.header(reqwest::header::RANGE, format!("bytes={offset}-"));
            if let Some(validator) = saved.if_range() {
                get = get.header(reqwest::header::IF_RANGE, validator);
            }
        }
        let response = tokio::select! {
            _ = cancel.cancelled() => return Err(Stop::Cancelled),
            response = get.send() => response.map_err(|_| {
                Stop::Failed(format!("{server} did not answer. Check that it is awake, then try again"))
            })?,
        };
        let status = response.status().as_u16();
        match (status, &saved) {
            (206, Some(saved)) => match content_range(&response) {
                Some((start, total)) if start == offset && saved.continues(&response, total) => {
                    break (response, offset, total);
                }
                // Not the range asked for, or the file changed on the
                // server: start over rather than splice.
                _ if !restarted => {}
                _ => {
                    return Err(Stop::Failed(format!(
                        "{server} sent part of {name} this Flick did not ask for"
                    )));
                }
            },
            // A whole file: the first copy, or the server's file changed
            // since the part file was started (`If-Range` did not match).
            (200, _) => {
                let total = response.content_length();
                let info = serde_json::to_vec(&PartInfo::of(&response, total))
                    .map_err(|error| write_error(std::io::Error::other(error)))?;
                tokio::fs::write(&paths.info, info)
                    .await
                    .map_err(write_error)?;
                break (response, 0, total);
            }
            // The part file is longer than the video now is.
            (416, Some(_)) if !restarted => {}
            _ => return Err(Stop::Failed(refusal(status, server, &name))),
        }
        restarted = true;
        remove_if_present(&paths.part).await.map_err(write_error)?;
        remove_if_present(&paths.info).await.map_err(write_error)?;
    };

    let file = if offset > 0 {
        tokio::fs::OpenOptions::new()
            .append(true)
            .open(&paths.part)
            .await
    } else {
        tokio::fs::File::create(&paths.part).await
    }
    .map_err(write_error)?;
    let mut file = tokio::io::BufWriter::with_capacity(256 * 1024, file);

    let started = Instant::now();
    let mut rate = Rate::new(started, offset);
    let mut last_report = started;
    let report = |downloaded: u64, bytes_per_second: u64| {
        let _ = events.send(KeepEvent::Progress(KeepProgress {
            server_id: request.server_id.clone(),
            video_id: request.video_id.clone(),
            downloaded,
            total,
            bytes_per_second,
        }));
    };
    report(offset, 0);
    loop {
        let chunk = tokio::select! {
            _ = cancel.cancelled() => {
                drop(file);
                return Err(Stop::Cancelled);
            }
            chunk = response.chunk() => chunk,
        };
        let chunk = match chunk {
            Ok(Some(chunk)) => chunk,
            Ok(None) => break,
            Err(_) => {
                let _ = file.flush().await;
                return Err(Stop::Failed(format!(
                    "The connection to {server} was lost while copying {name}. Try again to continue where it stopped"
                )));
            }
        };
        file.write_all(&chunk).await.map_err(write_error)?;
        offset += chunk.len() as u64;
        let now = Instant::now();
        if now.duration_since(last_report) >= PROGRESS_EVERY {
            last_report = now;
            report(offset, rate.sample(now, offset));
        }
    }
    file.flush().await.map_err(write_error)?;
    file.get_ref().sync_all().await.map_err(write_error)?;
    drop(file);
    if let Some(total) = total
        && offset != total
    {
        return Err(Stop::Failed(format!(
            "{server} stopped sending {name} before the end. Try again to continue where it stopped"
        )));
    }
    // The final name is chosen only now, so a name taken while the copy ran
    // is not overwritten.
    let path = unused_path(&paths.folder, &name);
    tokio::fs::rename(&paths.part, &path)
        .await
        .map_err(write_error)?;
    let _ = tokio::fs::remove_file(&paths.info).await;
    Ok(path)
}

/// `Content-Range: bytes start-end/total` as `(start, total)`; total is None
/// for `*`.
fn content_range(response: &reqwest::Response) -> Option<(u64, Option<u64>)> {
    let value = response
        .headers()
        .get(reqwest::header::CONTENT_RANGE)?
        .to_str()
        .ok()?;
    let (range, total) = value.strip_prefix("bytes ")?.split_once('/')?;
    let (start, end) = range.split_once('-')?;
    let start: u64 = start.trim().parse().ok()?;
    let end: u64 = end.trim().parse().ok()?;
    if end < start {
        return None;
    }
    let total = match total.trim() {
        "*" => None,
        total => Some(total.parse().ok()?),
    };
    Some((start, total))
}

/// A sentence for a status the copy cannot continue from.
fn refusal(status: u16, server: &str, name: &str) -> String {
    match status {
        401 => {
            format!("{server} no longer accepts this Flick's pairing. Pair again, then keep {name}")
        }
        403 => format!("{server} does not allow copying {name}"),
        404 => format!("{server} no longer has {name}"),
        409 => format!("{name} is still downloading on {server}"),
        503 => format!("{server} is busy. Try again in a minute"),
        500..=599 => format!("{server} could not send {name} (error {status})"),
        _ => format!("{server} gave an unexpected answer for {name} ({status})"),
    }
}

/// An I/O error without the path it concerns.
fn io_reason(error: &std::io::Error) -> String {
    match error.kind() {
        std::io::ErrorKind::StorageFull => "the disk is full".into(),
        std::io::ErrorKind::PermissionDenied => "the download folder is not writable".into(),
        std::io::ErrorKind::ReadOnlyFilesystem => "the disk is read-only".into(),
        kind => kind.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{BufRead, BufReader, Write},
        net::TcpListener,
        sync::{
            Mutex,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
    };

    const AUTH: &str = "Bearer 0123456789abcdef";

    fn fixture() -> Arc<Vec<u8>> {
        fixture_from(0x1234_5678, 3 * 1024 * 1024)
    }

    /// `length` non-repeating bytes, different for each seed, so a
    /// misplaced range or a spliced file shows.
    fn fixture_from(seed: u32, length: usize) -> Arc<Vec<u8>> {
        let mut state = seed;
        Arc::new(
            (0..length)
                .map(|_| {
                    state ^= state << 13;
                    state ^= state >> 17;
                    state ^= state << 5;
                    state as u8
                })
                .collect(),
        )
    }

    #[derive(Default)]
    struct Behaviour {
        /// Close the connection after this many body bytes, once.
        cut_after: Mutex<Option<usize>>,
        /// Send slowly, so a test can cancel mid-copy.
        slow: AtomicBool,
        /// Range headers received, in order (None when absent).
        ranges: Mutex<Vec<Option<String>>>,
        /// `(path, If-Range)` received, in order.
        if_ranges: Mutex<Vec<(String, Option<String>)>>,
        requests: AtomicUsize,
        /// Bodies by request path; others get the server's default body.
        bodies: Mutex<std::collections::HashMap<String, Arc<Vec<u8>>>>,
        /// Send no `ETag`.
        no_etag: AtomicBool,
        /// Serve the range even when `If-Range` does not match, as a
        /// careless server might.
        ignore_if_range: AtomicBool,
    }

    /// A strong tag that changes with the body.
    fn tag_of(body: &[u8]) -> String {
        let sum = body.iter().fold(0u64, |sum, b| {
            sum.wrapping_mul(31).wrapping_add(u64::from(*b))
        });
        format!("\"{:x}-{sum:x}\"", body.len())
    }

    /// Serves `body` (or a path's own in [`Behaviour::bodies`]) with range,
    /// `ETag`, and `If-Range` support, 401 without [`AUTH`].
    fn server(body: Arc<Vec<u8>>, behaviour: Arc<Behaviour>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let (body, behaviour) = (body.clone(), behaviour.clone());
                thread::spawn(move || {
                    behaviour.requests.fetch_add(1, Ordering::SeqCst);
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let mut line = String::new();
                    let _ = reader.read_line(&mut line);
                    let path = line.split_whitespace().nth(1).unwrap_or("").to_owned();
                    let body = behaviour
                        .bodies
                        .lock()
                        .unwrap()
                        .get(&path)
                        .cloned()
                        .unwrap_or(body);
                    let tag = tag_of(&body);
                    let mut authorized = false;
                    let mut range = None;
                    let mut if_range = None;
                    loop {
                        let mut header = String::new();
                        if reader.read_line(&mut header).is_err() || header.trim().is_empty() {
                            break;
                        }
                        if let Some((name, value)) = header.split_once(':') {
                            if name.eq_ignore_ascii_case("authorization") {
                                authorized = value.trim() == AUTH;
                            } else if name.eq_ignore_ascii_case("range") {
                                range = Some(value.trim().to_owned());
                            } else if name.eq_ignore_ascii_case("if-range") {
                                if_range = Some(value.trim().to_owned());
                            }
                        }
                    }
                    behaviour.ranges.lock().unwrap().push(range.clone());
                    behaviour
                        .if_ranges
                        .lock()
                        .unwrap()
                        .push((path.clone(), if_range.clone()));
                    let etag = if behaviour.no_etag.load(Ordering::SeqCst) {
                        String::new()
                    } else {
                        format!("ETag: {tag}\r\n")
                    };
                    let range_applies = behaviour.ignore_if_range.load(Ordering::SeqCst)
                        || if_range.as_ref().is_none_or(|value| *value == tag);
                    let range = range.filter(|_| range_applies);
                    if !authorized {
                        let reply = r#"{"message":"no"}"#;
                        let _ = write!(
                            stream,
                            "HTTP/1.1 401 Unauthorized\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                            reply.len()
                        );
                        return;
                    }
                    let start = range
                        .as_deref()
                        .and_then(|r| r.strip_prefix("bytes="))
                        .and_then(|r| r.strip_suffix('-'))
                        .and_then(|r| r.parse::<usize>().ok());
                    let start = match start {
                        Some(start) if start >= body.len() => {
                            let _ = write!(
                                stream,
                                "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */{}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                                body.len()
                            );
                            return;
                        }
                        Some(start) => {
                            let _ = write!(
                                stream,
                                "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {start}-{}/{}\r\nContent-Length: {}\r\n{etag}Accept-Ranges: bytes\r\nConnection: close\r\n\r\n",
                                body.len() - 1,
                                body.len(),
                                body.len() - start
                            );
                            start
                        }
                        None => {
                            let _ = write!(
                                stream,
                                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n{etag}Accept-Ranges: bytes\r\nConnection: close\r\n\r\n",
                                body.len()
                            );
                            0
                        }
                    };
                    let rest = &body[start..];
                    let cut = behaviour.cut_after.lock().unwrap().take();
                    let rest = match cut {
                        Some(cut) => &rest[..cut.min(rest.len())],
                        None => rest,
                    };
                    for chunk in rest.chunks(64 * 1024) {
                        if stream.write_all(chunk).is_err() {
                            return;
                        }
                        if behaviour.slow.load(Ordering::SeqCst) {
                            thread::sleep(Duration::from_millis(50));
                        }
                    }
                    let _ = stream.flush();
                    // Dropping the stream closes it, cut short or not.
                });
            }
        });
        format!("http://127.0.0.1:{port}/v1/media/v1")
    }

    fn request(url: &str, dir: &Path, video_id: &str) -> KeepRequest {
        KeepRequest {
            server_id: "s1".into(),
            server_name: "Mac mini".into(),
            video_id: video_id.into(),
            file_name: "Holiday.mkv".into(),
            url: url.into(),
            authorization: AUTH.into(),
            download_dir: dir.to_owned(),
        }
    }

    /// Drains until a finishing event for `video_id` arrives, collecting
    /// every event on the way.
    fn finish(state: &mut KeepState, video_id: &str, seen: &mut Vec<KeepEvent>) -> KeepEvent {
        let started = Instant::now();
        loop {
            assert!(started.elapsed() < Duration::from_secs(20), "timed out");
            for event in state.drain() {
                seen.push(event.clone());
                match &event {
                    KeepEvent::Done { video_id: v, .. }
                    | KeepEvent::Failed { video_id: v, .. }
                    | KeepEvent::Cancelled { video_id: v, .. }
                        if v == video_id =>
                    {
                        return event;
                    }
                    _ => {}
                }
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn folder(dir: &Path) -> PathBuf {
        dir.join("From Mac mini")
    }

    /// The part file of server `s1`'s `video`.
    fn part(dir: &Path, video: &str) -> PathBuf {
        folder(dir).join(format!(".s1-{video}.part"))
    }

    #[test]
    fn copies_the_whole_file() {
        let dir = tempfile::tempdir().unwrap();
        let body = fixture();
        let url = server(body.clone(), Arc::default());
        let mut state = KeepState::default();
        state.keep(request(&url, dir.path(), "v1"));
        assert_eq!(state.in_progress().len(), 1);
        let mut seen = Vec::new();
        let event = finish(&mut state, "v1", &mut seen);
        let path = folder(dir.path()).join("Holiday.mkv");
        assert_eq!(
            event,
            KeepEvent::Done {
                server_id: "s1".into(),
                video_id: "v1".into(),
                path: path.clone(),
            }
        );
        assert_eq!(std::fs::read(&path).unwrap(), *body);
        assert!(!part(dir.path(), "v1").exists());
        assert!(!part(dir.path(), "v1").with_extension("part.json").exists());
        assert!(state.in_progress().is_empty());
        // The first report carries the size.
        assert!(seen.iter().any(|event| matches!(
            event,
            KeepEvent::Progress(KeepProgress { total: Some(t), .. }) if *t == body.len() as u64
        )));

        // A second copy does not overwrite the first.
        state.keep(request(&url, dir.path(), "v1"));
        match finish(&mut state, "v1", &mut seen) {
            KeepEvent::Done { path, .. } => {
                assert_eq!(path, folder(dir.path()).join("Holiday (2).mkv"));
                assert_eq!(std::fs::read(&path).unwrap(), *body);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn resumes_an_interrupted_copy_with_a_range() {
        let dir = tempfile::tempdir().unwrap();
        let body = fixture();
        let behaviour = Arc::new(Behaviour::default());
        *behaviour.cut_after.lock().unwrap() = Some(1_000_000);
        let url = server(body.clone(), behaviour.clone());
        let mut state = KeepState::default();
        state.keep(request(&url, dir.path(), "v1"));
        let mut seen = Vec::new();
        match finish(&mut state, "v1", &mut seen) {
            KeepEvent::Failed { message, .. } => {
                assert!(message.contains("Holiday.mkv"), "{message}");
                assert!(
                    !message.contains(&*dir.path().to_string_lossy()),
                    "{message}"
                );
                assert!(!message.contains("0123456789abcdef"), "{message}");
            }
            other => panic!("unexpected {other:?}"),
        }
        let part = part(dir.path(), "v1");
        assert_eq!(std::fs::read(&part).unwrap(), body[..1_000_000]);

        state.keep(request(&url, dir.path(), "v1"));
        match finish(&mut state, "v1", &mut seen) {
            KeepEvent::Done { path, .. } => assert_eq!(std::fs::read(path).unwrap(), *body),
            other => panic!("unexpected {other:?}"),
        }
        assert!(!part.exists());
        assert_eq!(
            *behaviour.ranges.lock().unwrap(),
            vec![None, Some("bytes=1000000-".to_owned())]
        );
        // The resume asked for the same file by its tag.
        let if_ranges = behaviour.if_ranges.lock().unwrap();
        assert_eq!(if_ranges[0].1, None);
        assert_eq!(if_ranges[1].1, Some(tag_of(&body)));
    }

    #[test]
    fn videos_with_the_same_name_never_share_a_part_file() {
        let dir = tempfile::tempdir().unwrap();
        let first = fixture_from(1, 2 * 1024 * 1024);
        let second = fixture_from(2, 2 * 1024 * 1024 + 17);
        let behaviour = Arc::new(Behaviour::default());
        behaviour
            .bodies
            .lock()
            .unwrap()
            .insert("/v1/media/v2".into(), second.clone());
        *behaviour.cut_after.lock().unwrap() = Some(700_000);
        let url = server(first.clone(), behaviour.clone());
        let second_url = url.replace("/v1/media/v1", "/v1/media/v2");
        let mut state = KeepState::default();
        let mut seen = Vec::new();

        // The first is cut off, then the second, with the same name, is kept.
        state.keep(request(&url, dir.path(), "v1"));
        assert!(matches!(
            finish(&mut state, "v1", &mut seen),
            KeepEvent::Failed { .. }
        ));
        state.keep(request(&second_url, dir.path(), "v2"));
        let second_path = match finish(&mut state, "v2", &mut seen) {
            KeepEvent::Done { path, .. } => path,
            other => panic!("unexpected {other:?}"),
        };
        assert_eq!(second_path, folder(dir.path()).join("Holiday.mkv"));
        assert_eq!(std::fs::read(&second_path).unwrap(), *second);
        // The first's part file was left alone.
        assert_eq!(
            std::fs::read(part(dir.path(), "v1")).unwrap(),
            first[..700_000]
        );

        // The first resumes into its own name.
        state.keep(request(&url, dir.path(), "v1"));
        let first_path = match finish(&mut state, "v1", &mut seen) {
            KeepEvent::Done { path, .. } => path,
            other => panic!("unexpected {other:?}"),
        };
        assert_eq!(first_path, folder(dir.path()).join("Holiday (2).mkv"));
        assert_eq!(std::fs::read(&first_path).unwrap(), *first);
        assert_eq!(std::fs::read(&second_path).unwrap(), *second);
        assert_eq!(
            behaviour.ranges.lock().unwrap().last().cloned().flatten(),
            Some("bytes=700000-".to_owned())
        );
    }

    /// Interrupts a copy of `before`, changes the server's file to `after`,
    /// keeps again, and returns the copy and the Range headers seen.
    fn keep_across_a_change(
        before: Arc<Vec<u8>>,
        after: Arc<Vec<u8>>,
        behaviour: Arc<Behaviour>,
    ) -> (Vec<u8>, Vec<Option<String>>) {
        let dir = tempfile::tempdir().unwrap();
        *behaviour.cut_after.lock().unwrap() = Some(500_000);
        let url = server(before, behaviour.clone());
        let mut state = KeepState::default();
        let mut seen = Vec::new();
        state.keep(request(&url, dir.path(), "v1"));
        assert!(matches!(
            finish(&mut state, "v1", &mut seen),
            KeepEvent::Failed { .. }
        ));
        behaviour
            .bodies
            .lock()
            .unwrap()
            .insert("/v1/media/v1".into(), after);
        state.keep(request(&url, dir.path(), "v1"));
        let path = match finish(&mut state, "v1", &mut seen) {
            KeepEvent::Done { path, .. } => path,
            other => panic!("unexpected {other:?}"),
        };
        assert!(!part(dir.path(), "v1").exists());
        let ranges = behaviour.ranges.lock().unwrap().clone();
        (std::fs::read(path).unwrap(), ranges)
    }

    #[test]
    fn a_changed_source_restarts_from_zero() {
        let before = fixture_from(3, 1_500_000);
        let after = fixture_from(4, 1_500_000);

        // The server sees If-Range no longer matches and sends it all.
        let (copy, ranges) = keep_across_a_change(before.clone(), after.clone(), Arc::default());
        assert_eq!(copy, *after);
        assert_eq!(ranges, vec![None, Some("bytes=500000-".to_owned())]);

        // A server that sends the range anyway is caught by its new tag.
        let careless = Arc::new(Behaviour::default());
        careless.ignore_if_range.store(true, Ordering::SeqCst);
        let (copy, ranges) = keep_across_a_change(before.clone(), after.clone(), careless);
        assert_eq!(copy, *after);
        assert_eq!(ranges, vec![None, Some("bytes=500000-".to_owned()), None]);

        // Without tags, a different size gives it away.
        let untagged = Arc::new(Behaviour::default());
        untagged.no_etag.store(true, Ordering::SeqCst);
        let longer = fixture_from(5, 1_600_000);
        let (copy, ranges) = keep_across_a_change(before, longer.clone(), untagged);
        assert_eq!(copy, *longer);
        assert_eq!(ranges, vec![None, Some("bytes=500000-".to_owned()), None]);
    }

    #[test]
    fn cancel_removes_only_its_own_part_file() {
        let dir = tempfile::tempdir().unwrap();
        let behaviour = Arc::new(Behaviour::default());
        *behaviour.cut_after.lock().unwrap() = Some(400_000);
        let url = server(fixture(), behaviour.clone());
        let mut state = KeepState::default();
        let mut seen = Vec::new();
        state.keep(request(&url, dir.path(), "v1"));
        assert!(matches!(
            finish(&mut state, "v1", &mut seen),
            KeepEvent::Failed { .. }
        ));
        behaviour.slow.store(true, Ordering::SeqCst);
        state.keep(request(&url, dir.path(), "v2"));
        let started = Instant::now();
        while !part(dir.path(), "v2").exists() {
            assert!(started.elapsed() < Duration::from_secs(10), "timed out");
            state.drain();
            thread::sleep(Duration::from_millis(10));
        }
        state.cancel("s1", "v2");
        assert!(matches!(
            finish(&mut state, "v2", &mut seen),
            KeepEvent::Cancelled { .. }
        ));
        assert!(!part(dir.path(), "v2").exists());
        assert_eq!(
            std::fs::read(part(dir.path(), "v1")).unwrap(),
            fixture()[..400_000]
        );
    }

    #[test]
    fn cancel_removes_the_part_file() {
        let dir = tempfile::tempdir().unwrap();
        let behaviour = Arc::new(Behaviour::default());
        behaviour.slow.store(true, Ordering::SeqCst);
        let url = server(fixture(), behaviour);
        let mut state = KeepState::default();
        state.keep(request(&url, dir.path(), "v1"));
        let part = part(dir.path(), "v1");
        let started = Instant::now();
        while !(state
            .in_progress()
            .first()
            .is_some_and(|p| p.downloaded > 0)
            || part.exists())
        {
            assert!(started.elapsed() < Duration::from_secs(10), "timed out");
            state.drain();
            thread::sleep(Duration::from_millis(10));
        }
        state.cancel("s1", "v1");
        let mut seen = Vec::new();
        assert_eq!(
            finish(&mut state, "v1", &mut seen),
            KeepEvent::Cancelled {
                server_id: "s1".into(),
                video_id: "v1".into(),
            }
        );
        assert!(!part.exists());
        assert!(!folder(dir.path()).join("Holiday.mkv").exists());
    }

    #[test]
    fn a_refused_token_fails_with_a_message() {
        let dir = tempfile::tempdir().unwrap();
        let url = server(fixture(), Arc::default());
        let mut state = KeepState::default();
        let mut wrong = request(&url, dir.path(), "v1");
        wrong.authorization = "Bearer wrong".into();
        state.keep(wrong);
        let mut seen = Vec::new();
        assert_eq!(
            finish(&mut state, "v1", &mut seen),
            KeepEvent::Failed {
                server_id: "s1".into(),
                video_id: "v1".into(),
                message:
                    "Mac mini no longer accepts this Flick's pairing. Pair again, then keep Holiday.mkv"
                        .into(),
            }
        );
        assert!(!part(dir.path(), "v1").exists());
    }

    #[test]
    fn one_copy_per_video_and_two_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let behaviour = Arc::new(Behaviour::default());
        behaviour.slow.store(true, Ordering::SeqCst);
        let url = server(fixture(), behaviour.clone());
        let mut state = KeepState::default();
        for video in ["a", "b", "c"] {
            let mut keep = request(&url, dir.path(), video);
            keep.file_name = format!("{video}.mkv");
            state.keep(keep.clone());
            // Asking again while it runs changes nothing.
            state.keep(keep);
        }
        assert_eq!(state.in_progress().len(), 3);
        thread::sleep(Duration::from_millis(500));
        state.drain();
        assert_eq!(behaviour.requests.load(Ordering::SeqCst), 2);
        // Shutting down cancels all three and removes their part files.
        state.shutdown();
        assert!(state.in_progress().is_empty());
        for video in ["a", "b", "c"] {
            assert!(!part(dir.path(), video).exists());
        }
    }

    #[test]
    fn names_are_made_safe() {
        assert_eq!(safe_component("../../etc", "v"), "-..-etc");
        assert_eq!(safe_component("  .hidden ", "v"), "hidden");
        assert_eq!(safe_component("a:b\nc", "v"), "a-b c");
        assert_eq!(safe_component("", "v/1"), "v1");
        assert_eq!(safe_component("...", ""), "video");
        let mut keep = request("http://x", Path::new("/tmp"), "id1");
        keep.file_name = "Shows/S01/Episode 1.mkv".into();
        assert_eq!(file_name(&keep), "Episode 1.mkv");
        keep.file_name = "C:\\Videos\\Film.mp4".into();
        assert_eq!(file_name(&keep), "Film.mp4");
        keep.file_name = "..".into();
        assert_eq!(file_name(&keep), "id1");
        keep.server_name = "Mac/mini".into();
        let paths = destination(&keep);
        assert_eq!(paths.folder, Path::new("/tmp/From Mac-mini"));
        assert_eq!(paths.part, Path::new("/tmp/From Mac-mini/.s1-id1.part"));
        assert_eq!(
            paths.info,
            Path::new("/tmp/From Mac-mini/.s1-id1.part.json")
        );
        // Ids with other characters are hashed, so `-` stays a separator.
        keep.server_id = "a-b".into();
        keep.video_id = "c".into();
        let one = destination(&keep).part;
        keep.server_id = "a".into();
        keep.video_id = "b-c".into();
        assert_ne!(one, destination(&keep).part);
        assert!(!one.to_string_lossy().contains("a-b"), "{one:?}");
    }

    #[test]
    fn long_names_are_cut_by_bytes_on_a_character_boundary() {
        let cjk = "映画".repeat(60); // 360 bytes, 3 per character
        let cut = safe_component(&cjk, "v");
        assert_eq!(cut.len(), 198);
        assert!(cjk.starts_with(&cut));
        let emoji = format!("a{}", "🎬".repeat(60)); // 1 + 240 bytes
        let cut = safe_component(&emoji, "v");
        assert_eq!(cut.len(), 197);
        assert!(emoji.starts_with(&cut));
        let ascii = "x".repeat(300);
        assert_eq!(safe_component(&ascii, "v").len(), 200);
        assert_eq!(safe_component("short.mkv", "v"), "short.mkv");
    }

    #[test]
    fn the_rate_follows_a_short_window() {
        let start = Instant::now();
        let mut rate = Rate::new(start, 0);
        assert_eq!(rate.sample(start + Duration::from_secs(1), 1_000), 1_000);
        assert_eq!(rate.sample(start + Duration::from_secs(2), 2_000), 1_000);
        // After a fast stretch the older slow samples fall out of the window.
        rate.sample(start + Duration::from_secs(5), 32_000);
        let now = rate.sample(start + Duration::from_secs(6), 42_000);
        assert!(now >= 10_000, "{now}");
    }

    #[test]
    fn debug_output_hides_the_token() {
        let keep = request("http://x", Path::new("/tmp"), "v1");
        let shown = format!("{keep:?}");
        assert!(!shown.contains("0123456789abcdef"), "{shown}");
        assert!(shown.contains("<redacted>"));
    }
}
