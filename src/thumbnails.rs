//! Poster frames and media details for library videos.
//!
//! A background thread owns its own silent libmpv player and the same
//! software renderer as playback. It opens each video paused at 20% of its
//! length, saves a small PNG in the cache directory, and reports the
//! duration and display size it saw. Requests are handled one at a time;
//! nothing here touches the playing video.

use crate::playback::{Player, PlayerEvent};
use anyhow::{Context, Result, bail, ensure};
use std::{
    collections::{HashSet, VecDeque},
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant, UNIX_EPOCH},
};

/// Poster width; height follows the video's aspect ratio.
const POSTER_WIDTH: u32 = 400;
/// Give up on a file that does not load and decode in this time.
const POSTER_DEADLINE: Duration = Duration::from_secs(10);
/// Frames darker than this mean value are retried later in the video.
const DARK_FRAME: f64 = 12.;

#[derive(Clone, Debug, PartialEq)]
pub struct MediaDetails {
    pub duration: f64,
    pub width: u32,
    pub height: u32,
    pub codec: Option<String>,
}

pub struct Poster {
    pub video: PathBuf,
    pub result: Result<(PathBuf, MediaDetails), String>,
}

const MAX_POSTER_REQUESTS: usize = 64;
const MAX_POSTER_RESULTS: usize = 32;
const DECODER_IDLE: Duration = Duration::from_secs(5);
/// Disk quota is enforced by the poster worker, never by the UI.
const CACHE_QUOTA_BYTES: u64 = 256 * 1024 * 1024;
const CACHE_QUOTA_FILES: usize = 2048;

/// Cache artifacts removed by background maintenance. Clear matching poster
/// references/assets when consumed. A rescan is needed only if >256 removals
/// accumulated before the UI drained this bounded notification.
#[derive(Default)]
pub struct CacheInvalidations {
    pub paths: Vec<PathBuf>,
    pub rescan_required: bool,
}

impl CacheInvalidations {
    fn removed(&mut self, path: PathBuf) {
        if self.paths.len() < 256 {
            self.paths.push(path);
        } else {
            self.rescan_required = true;
        }
    }

    fn merge(&mut self, other: Self) {
        self.rescan_required |= other.rescan_required;
        for path in other.paths {
            self.removed(path);
        }
    }
}

#[derive(Default)]
struct PosterState {
    queue: VecDeque<(PathBuf, Arc<AtomicBool>)>,
    active: Option<(PathBuf, Arc<AtomicBool>)>,
    results: VecDeque<Poster>,
    stop: bool,
    close: bool,
    retain: Option<HashSet<PathBuf>>,
    invalidations: CacheInvalidations,
}

#[derive(Default)]
struct PosterShared {
    state: Mutex<PosterState>,
    wake: Condvar,
}

pub struct Thumbnails {
    shared: Arc<PosterShared>,
    worker: Option<thread::JoinHandle<()>>,
}

impl Thumbnails {
    /// Starts an independent silent decoder with bounded admission/results.
    /// Without a cache directory, requests return an error instead of writing
    /// elsewhere. Drain results regularly and retry requests not admitted.
    pub fn start(cache: Option<PathBuf>) -> Self {
        let shared = Arc::new(PosterShared::default());
        let state = shared.clone();
        let worker = thread::Builder::new()
            .name("flick-thumbnails".into())
            .spawn(move || poster_worker(state, cache))
            .ok();
        if worker.is_none() {
            shared.state.lock().unwrap_or_else(|e| e.into_inner()).stop = true;
        }
        Self { shared, worker }
    }

    #[cfg_attr(not(test), allow(dead_code))] // Compatibility for backend callers.
    pub fn request(&self, video: PathBuf) {
        let _ = self.try_request(video);
    }

    /// True means admitted/already pending. False means retry on a future tick.
    pub fn try_request(&self, video: PathBuf) -> bool {
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.stop {
            return false;
        }
        if state.queue.iter().any(|(path, _)| path == &video)
            || state
                .active
                .as_ref()
                .is_some_and(|(path, cancel)| path == &video && !cancel.load(Ordering::Acquire))
            || state.results.iter().any(|poster| poster.video == video)
        {
            return true;
        }
        if state.queue.len() >= MAX_POSTER_REQUESTS {
            return false;
        }
        state
            .queue
            .push_back((video, Arc::new(AtomicBool::new(false))));
        self.shared.wake.notify_one();
        true
    }

    pub fn drain(&self) -> impl Iterator<Item = Poster> + '_ {
        let results: Vec<_> = self
            .shared
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .results
            .drain(..)
            .collect();
        self.shared.wake.notify_one();
        results.into_iter()
    }

    pub fn take_cache_invalidations(&self) -> CacheInvalidations {
        std::mem::take(
            &mut self
                .shared
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .invalidations,
        )
    }

    /// Cancels one source, including a running decode and unpublished result.
    pub fn cancel(&self, video: &Path) {
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        state.queue.retain(|(path, _)| path != video);
        state.results.retain(|poster| poster.video != video);
        if let Some((path, cancel)) = &state.active
            && path == video
        {
            cancel.store(true, Ordering::Release);
            state.close = true;
        }
        self.shared.wake.notify_one();
    }

    /// Cancels queued AND active requests and releases undrained results.
    pub fn cancel_pending(&self) {
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        state.queue.clear();
        state.results.clear();
        if let Some((_, cancel)) = &state.active {
            cancel.store(true, Ordering::Release);
        }
        state.close = true;
        self.shared.wake.notify_one();
    }

    pub fn idle_close(&self) {
        self.cancel_pending();
    }

    /// Optional library reconciliation: the worker removes obsolete posters
    /// using the existing path/size/mtime key. No stat or directory scan here.
    pub fn retain_sources(&self, videos: impl IntoIterator<Item = PathBuf>) {
        self.shared
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain = Some(videos.into_iter().collect());
        self.shared.wake.notify_one();
    }

    /// Cooperative stop, with no UI-thread join against native decode/I/O.
    /// The thread retains its own Player and destroys it on its owner thread.
    pub fn shutdown(&mut self) {
        self.cancel_pending();
        self.shared
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .stop = true;
        self.shared.wake.notify_all();
        self.worker.take(); // detach; no native handle is freed on this thread
    }
}

impl Drop for Thumbnails {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn poster_worker(shared: Arc<PosterShared>, cache: Option<PathBuf>) {
    let mut player = None;
    let mut since_cleanup = 0usize;
    if let Some(cache) = &cache {
        maintain_cache(&shared, cache, None);
    }
    loop {
        let (video, cancel) = {
            let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if state.stop {
                    return;
                }
                if state.close {
                    state.close = false;
                    drop(state);
                    player = None;
                    state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
                    continue;
                }
                if let Some(retain) = state.retain.take() {
                    drop(state);
                    if let Some(cache) = &cache {
                        maintain_cache(&shared, cache, Some(&retain));
                    }
                    state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
                    continue;
                }
                if state.results.len() < MAX_POSTER_RESULTS
                    && let Some(request) = state.queue.pop_front()
                {
                    state.active = Some(request.clone());
                    break request;
                }
                let (next, timeout) = shared
                    .wake
                    .wait_timeout(state, DECODER_IDLE)
                    .unwrap_or_else(|e| e.into_inner());
                state = next;
                if timeout.timed_out() {
                    // Native destruction may wait; never hold the admission
                    // mutex while dropping the owner-thread decoder.
                    drop(state);
                    player = None;
                    state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
                }
            }
        };
        let result = cache
            .as_deref()
            .context("No cache directory is available for thumbnails")
            .and_then(|cache| poster(&mut player, &video, cache, &cancel))
            .map_err(|error| format!("{error:#}"));
        let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
        state.active = None;
        if !state.stop && !cancel.load(Ordering::Acquire) {
            state.results.push_back(Poster { video, result });
        }
        let cancelled = cancel.load(Ordering::Acquire);
        drop(state);
        since_cleanup += 1;
        if since_cleanup >= 32 {
            if let Some(cache) = &cache {
                maintain_cache(&shared, cache, None);
            }
            since_cleanup = 0;
        }
        if cancelled {
            player = None;
        }
    }
}

/// Preview width for timeline hovers. Playback presentation is unchanged.
const PREVIEW_WIDTH: u32 = 192;

pub struct PreviewRequest {
    pub video: PathBuf,
    pub time: f64,
    pub generation: u64,
}

pub struct Preview {
    pub generation: u64,
    pub frame: crate::playback::VideoFrame,
}

#[derive(Default)]
struct PreviewState {
    request: Option<(PreviewRequest, Arc<AtomicBool>)>,
    active: Option<Arc<AtomicBool>>,
    result: Option<Preview>,
    stop: bool,
    close: bool,
}

#[derive(Default)]
struct PreviewShared {
    state: Mutex<PreviewState>,
    wake: Condvar,
}

/// Independent silent decoder: one newest request and one newest result.
/// Superseded work checks cancellation while loading and decoding.
pub struct Previews {
    shared: Arc<PreviewShared>,
    worker: Option<thread::JoinHandle<()>>,
}

impl Previews {
    pub fn start() -> Self {
        let shared = Arc::new(PreviewShared::default());
        let state = shared.clone();
        let worker = thread::Builder::new()
            .name("flick-previews".into())
            .spawn(move || preview_worker(state))
            .ok();
        if worker.is_none() {
            shared.state.lock().unwrap_or_else(|e| e.into_inner()).stop = true;
        }
        Self { shared, worker }
    }

    pub fn request(&self, request: PreviewRequest) {
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.stop {
            return;
        }
        if let Some(cancel) = &state.active {
            cancel.store(true, Ordering::Release);
        }
        if let Some((_, cancel)) = &state.request {
            cancel.store(true, Ordering::Release);
        }
        state.result = None;
        state.request = Some((request, Arc::new(AtomicBool::new(false))));
        self.shared.wake.notify_one();
    }

    pub fn latest(&self) -> Option<Preview> {
        self.shared
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .result
            .take()
    }

    pub fn cancel(&self) {
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(cancel) = &state.active {
            cancel.store(true, Ordering::Release);
        }
        state.request = None;
        state.result = None;
        self.shared.wake.notify_one();
    }

    pub fn idle_close(&self) {
        self.cancel();
        self.shared
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .close = true;
        self.shared.wake.notify_one();
    }

    pub fn shutdown(&mut self) {
        self.cancel();
        self.shared
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .stop = true;
        self.shared.wake.notify_all();
        self.worker.take();
    }
}

impl Drop for Previews {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn preview_worker(shared: Arc<PreviewShared>) {
    let mut player = None;
    let mut open = None;
    loop {
        let (request, cancel) = {
            let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if state.stop {
                    return;
                }
                if state.close {
                    state.close = false;
                    drop(state);
                    player = None;
                    open = None;
                    state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
                    continue;
                }
                if let Some(request) = state.request.take() {
                    state.active = Some(request.1.clone());
                    break request;
                }
                let (next, timeout) = shared
                    .wake
                    .wait_timeout(state, DECODER_IDLE)
                    .unwrap_or_else(|e| e.into_inner());
                state = next;
                if timeout.timed_out() {
                    drop(state);
                    player = None;
                    open = None;
                    state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
                }
            }
        };
        let result = preview(&mut player, &mut open, &request, &cancel);
        let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
        state.active = None;
        let cancelled = cancel.load(Ordering::Acquire);
        if !state.stop && !cancelled {
            match result {
                Ok(frame) => {
                    state.result = Some(Preview {
                        generation: request.generation,
                        frame,
                    })
                }
                Err(error) => {
                    open = None;
                    tracing::debug!(%error, "No timeline preview");
                }
            }
        }
        drop(state);
        // A fully loaded source can service the next hover without native
        // teardown/reinitialization on every mouse move. Pending seeks remain
        // ordered through the async queue. An interrupted load has no trusted
        // open source and is destroyed on this worker before reuse.
        if cancelled && open.is_none() {
            player = None;
        }
    }
}

fn check_cancel(cancel: &AtomicBool) -> Result<()> {
    ensure!(
        !cancel.load(Ordering::Acquire),
        "Thumbnail request cancelled"
    );
    Ok(())
}

fn preview(
    player: &mut Option<Player>,
    open: &mut Option<PathBuf>,
    request: &PreviewRequest,
    cancel: &AtomicBool,
) -> Result<crate::playback::VideoFrame> {
    check_cancel(cancel)?;
    let player = match player {
        Some(player) => player,
        None => player.insert(Player::silent()?),
    };
    check_cancel(cancel)?;
    let time = format!("{:.3}", request.time.max(0.));
    let deadline = Instant::now() + POSTER_DEADLINE;
    if open.as_deref() == Some(request.video.as_path()) {
        player.seek_async(request.time.max(0.), false)?;
        // Invalidate the previous render notification; wait for native seek
        // completion before accepting any newly decoded image.
        while player.operations_pending() {
            check_cancel(cancel)?;
            check_events(player)?;
            ensure!(Instant::now() < deadline, "Timed out seeking preview");
            thread::sleep(Duration::from_millis(5));
        }
    } else {
        *open = None;
        player.set_property("start", &time)?;
        check_cancel(cancel)?;
        player.load(
            request
                .video
                .to_str()
                .context("Video path must be valid UTF-8")?,
        )?;
        player.set_property("pause", "yes")?;
        let mut loaded = false;
        while !loaded {
            check_cancel(cancel)?;
            for event in player.poll_events() {
                match event {
                    PlayerEvent::FileLoaded => loaded = true,
                    PlayerEvent::Error(error) => bail!(error),
                    PlayerEvent::OperationCompleted {
                        result: crate::playback::OperationResult::Failed(error),
                        ..
                    } => bail!(error),
                    _ => {}
                }
            }
            ensure!(Instant::now() < deadline, "Timed out opening the video");
            thread::sleep(Duration::from_millis(5));
        }
        *open = Some(request.video.clone());
    }
    loop {
        check_cancel(cancel)?;
        check_events(player)?;
        if let Some((width, height)) = player.video_size() {
            let (w, h) = preview_size(width, height);
            if let Some(frame) = player.render(w, h)? {
                check_cancel(cancel)?;
                return Ok(frame);
            }
        }
        ensure!(Instant::now() < deadline, "Timed out decoding a preview");
        thread::sleep(Duration::from_millis(5));
    }
}

fn check_events(player: &Player) -> Result<()> {
    for event in player.poll_events() {
        match event {
            PlayerEvent::Error(error) => bail!(error),
            PlayerEvent::OperationCompleted {
                result: crate::playback::OperationResult::Failed(error),
                ..
            } => bail!(error),
            _ => {}
        }
    }
    Ok(())
}

fn preview_size(width: u32, height: u32) -> (u32, u32) {
    let height = (f64::from(PREVIEW_WIDTH) * f64::from(height) / f64::from(width.max(1)))
        .round()
        .clamp(1., f64::from(PREVIEW_WIDTH));
    (PREVIEW_WIDTH, height as u32)
}

/// The cache file for a video, keyed by path, size, and modification time
/// so a replaced file gets a new poster.
pub fn poster_path(cache: &Path, video: &Path) -> Result<PathBuf> {
    let metadata = std::fs::metadata(video).context("Read video file details")?;
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |time| time.as_nanos());
    let key = format!(
        "{}\u{0}{}\u{0}{modified}",
        video.to_string_lossy(),
        metadata.len()
    );
    Ok(cache.join(format!("{:016x}.png", fnv1a(key.as_bytes()))))
}

/// FNV-1a, stable across Rust releases unlike the standard library hasher.
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
    })
}

fn poster(
    player: &mut Option<Player>,
    video: &Path,
    cache: &Path,
    cancel: &AtomicBool,
) -> Result<(PathBuf, MediaDetails)> {
    check_cancel(cancel)?;
    let target = poster_path(cache, video)?;
    let player = match player {
        Some(player) => player,
        None => player.insert(Player::silent()?),
    };
    let mut frame = None;
    let mut details = None;
    // Openings and fades are often black; look later if the first try is.
    for start in ["20%", "50%"] {
        check_cancel(cancel)?;
        let (pixels, found) = capture(player, video, start, cancel)?;
        let dark = mean_brightness(&pixels.pixels) < DARK_FRAME;
        frame = Some(pixels);
        details = Some(found);
        if !dark {
            break;
        }
    }
    let _ = player.command(&["stop"]);
    let (frame, details) = (frame.context("No frame")?, details.context("No details")?);
    check_cancel(cancel)?;
    save_png(&target, frame)?;
    check_cancel(cancel)?;
    Ok((target, details))
}

fn capture(
    player: &Player,
    video: &Path,
    start: &str,
    cancel: &AtomicBool,
) -> Result<(crate::playback::VideoFrame, MediaDetails)> {
    check_cancel(cancel)?;
    player.set_property("start", start)?;
    player.load(video.to_str().context("Video path must be valid UTF-8")?)?;
    player.set_property("pause", "yes")?;
    let deadline = Instant::now() + POSTER_DEADLINE;
    let mut loaded_at = None;
    loop {
        check_cancel(cancel)?;
        for event in player.poll_events() {
            match event {
                PlayerEvent::FileLoaded => loaded_at = Some(Instant::now()),
                PlayerEvent::Error(error) => bail!(error),
                PlayerEvent::EndFile if loaded_at.is_none() => {
                    bail!("The video ended before it loaded")
                }
                PlayerEvent::EndFile | PlayerEvent::OperationCompleted { .. } => {}
            }
        }
        if let Some(loaded_at) = loaded_at
            && let Some((width, height)) = player.video_size()
        {
            let size = poster_size(width, height);
            // A render at a new size can precede the first decoded frame
            // and come back black. Later frames arrive through libmpv's
            // update callback; accept black only once decoding had time.
            if let Some(frame) = player.render(size.0, size.1)?
                && (mean_brightness(&frame.pixels) > 1.
                    || loaded_at.elapsed() > Duration::from_millis(1500))
            {
                let snapshot = player.snapshot();
                return Ok((
                    frame,
                    MediaDetails {
                        duration: snapshot.duration,
                        width,
                        height,
                        codec: player.video_codec(),
                    },
                ));
            }
        }
        ensure!(Instant::now() < deadline, "Timed out reading a video frame");
        thread::sleep(Duration::from_millis(10));
    }
}

/// A poster [`POSTER_WIDTH`] wide with the video's aspect ratio. Very tall
/// videos are capped at a square.
fn poster_size(width: u32, height: u32) -> (u32, u32) {
    let height = (f64::from(POSTER_WIDTH) * f64::from(height) / f64::from(width.max(1)))
        .round()
        .clamp(1., f64::from(POSTER_WIDTH));
    (POSTER_WIDTH, height as u32)
}

fn mean_brightness(bgra: &[u8]) -> f64 {
    let pixels = bgra.as_chunks::<4>().0;
    if pixels.is_empty() {
        return 0.;
    }
    let total = pixels
        .iter()
        .map(|p| u64::from(p[0]) + u64::from(p[1]) + u64::from(p[2]))
        .sum::<u64>();
    total as f64 / (pixels.len() as f64 * 3.)
}

/// Only our original hashed names are cache-owned. Never recurse, follow
/// symlinks, or unlink unrelated images/files even inside the cache directory.
fn owned_cache_name(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let hash = name
        .strip_suffix(".png")
        .or_else(|| name.strip_suffix(".png.tmp"));
    hash.is_some_and(|hash| hash.len() == 16 && hash.bytes().all(|b| b.is_ascii_hexdigit()))
}

fn clean_cache(
    cache: &Path,
    retain: Option<&HashSet<PathBuf>>,
    protect: Option<&Path>,
) -> Result<CacheInvalidations> {
    let metadata = match std::fs::symlink_metadata(cache) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(CacheInvalidations::default());
        }
        Err(error) => return Err(error.into()),
    };
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "Cache must be a real directory"
    );
    let keep = retain.and_then(|sources| {
        sources
            .iter()
            .map(|video| poster_path(cache, video))
            .collect::<Result<HashSet<_>>>()
            .ok()
    });
    let mut invalidations = CacheInvalidations::default();
    let mut files = Vec::new();
    let mut bytes = 0u64;
    for entry in std::fs::read_dir(cache)? {
        let entry = entry?;
        let path = entry.path();
        if !owned_cache_name(&path) || !entry.file_type()?.is_file() {
            continue;
        }
        let metadata = std::fs::symlink_metadata(&path)?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            continue;
        }
        if path.extension().is_some_and(|extension| extension == "tmp") {
            // A different app instance may still be writing this temporary.
            if metadata
                .modified()
                .ok()
                .and_then(|time| time.elapsed().ok())
                .is_some_and(|age| age > Duration::from_secs(3600))
                && std::fs::remove_file(&path).is_ok()
            {
                invalidations.removed(path.clone());
            }
            continue;
        }
        let obsolete = path.extension().is_some_and(|extension| extension == "png")
            && keep.as_ref().is_some_and(|keep| !keep.contains(&path));
        if Some(path.as_path()) != protect && obsolete {
            if std::fs::remove_file(&path).is_ok() {
                invalidations.removed(path.clone());
            }
            continue;
        }
        bytes = bytes.saturating_add(metadata.len());
        files.push((
            metadata.modified().unwrap_or(UNIX_EPOCH),
            path,
            metadata.len(),
        ));
    }
    files.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    let mut count = files.len();
    for (_, path, length) in files {
        if bytes <= CACHE_QUOTA_BYTES && count <= CACHE_QUOTA_FILES {
            break;
        }
        if Some(path.as_path()) == protect {
            continue;
        }
        if std::fs::remove_file(&path).is_ok() {
            invalidations.removed(path);
            bytes = bytes.saturating_sub(length);
            count -= 1;
        }
    }
    Ok(invalidations)
}

fn maintain_cache(shared: &PosterShared, cache: &Path, retain: Option<&HashSet<PathBuf>>) {
    match clean_cache(cache, retain, None) {
        Ok(invalidations) => shared
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .invalidations
            .merge(invalidations),
        Err(error) => tracing::debug!(%error, "Could not maintain thumbnail cache"),
    }
}

/// Writes the BGRA frame as an RGBA PNG through a temporary file, so a
/// partly written poster never replaces a good one.
fn save_png(target: &Path, frame: crate::playback::VideoFrame) -> Result<()> {
    let mut pixels = frame.pixels;
    for pixel in pixels.as_chunks_mut::<4>().0 {
        pixel.swap(0, 2);
    }
    let image = image::RgbaImage::from_raw(frame.width, frame.height, pixels)
        .context("Video frame has an unexpected size")?;
    let parent = target.parent().context("Poster path has no folder")?;
    std::fs::create_dir_all(parent).context("Create thumbnail cache")?;
    let temporary = target.with_extension("png.tmp");
    image
        .save_with_format(&temporary, image::ImageFormat::Png)
        .context("Write poster")?;
    std::fs::rename(&temporary, target).context("Save poster")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn poster_sizes_follow_aspect_ratio() {
        assert_eq!(poster_size(1920, 1080), (400, 225));
        assert_eq!(poster_size(1024, 436), (400, 170));
        assert_eq!(poster_size(1080, 1920), (400, 400));
        assert_eq!(poster_size(0, 0), (400, 1));
    }

    #[test]
    fn poster_paths_change_when_a_file_changes() {
        let root = tempfile::tempdir().unwrap();
        let video = root.path().join("clip.mp4");
        std::fs::write(&video, b"one").unwrap();
        let first = poster_path(root.path(), &video).unwrap();
        assert_eq!(first, poster_path(root.path(), &video).unwrap());
        assert_eq!(first.parent(), Some(root.path()));
        std::fs::write(&video, b"longer").unwrap();
        assert_ne!(first, poster_path(root.path(), &video).unwrap());
        assert!(poster_path(root.path(), &root.path().join("missing.mp4")).is_err());
        assert_eq!(fnv1a(b""), 0xcbf29ce484222325);
    }

    #[test]
    fn dark_frames_are_detected() {
        assert!(mean_brightness(&[0, 0, 0, 255, 8, 8, 8, 255]) < DARK_FRAME);
        assert!(mean_brightness(&[200, 100, 50, 255]) > DARK_FRAME);
        assert_eq!(mean_brightness(&[]), 0.);
    }

    #[test]
    fn poster_admission_is_bounded_deduplicated_and_cancellable() {
        let thumbnails = Thumbnails {
            shared: Arc::new(PosterShared::default()),
            worker: None,
        };
        for index in 0..MAX_POSTER_REQUESTS {
            assert!(thumbnails.try_request(PathBuf::from(format!("video-{index}"))));
        }
        assert!(thumbnails.try_request(PathBuf::from("video-0")));
        assert!(!thumbnails.try_request(PathBuf::from("overflow")));
        thumbnails.cancel(Path::new("video-0"));
        assert!(thumbnails.try_request(PathBuf::from("overflow")));
        thumbnails.cancel_pending();
        assert!(thumbnails.shared.state.lock().unwrap().queue.is_empty());
    }

    #[test]
    fn poster_results_apply_backpressure_and_shutdown_discards_queued_work() -> Result<()> {
        // No cache means this exercises worker scheduling/errors without
        // initializing libmpv or touching media/network sources.
        let mut thumbnails = Thumbnails::start(None);
        for index in 0..MAX_POSTER_REQUESTS {
            assert!(thumbnails.try_request(PathBuf::from(format!("video-{index}"))));
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let state = thumbnails.shared.state.lock().unwrap();
            if state.results.len() == MAX_POSTER_RESULTS {
                assert!(state.queue.len() <= MAX_POSTER_REQUESTS);
                break;
            }
            drop(state);
            ensure!(Instant::now() < deadline, "Poster results did not fill");
            thread::sleep(Duration::from_millis(2));
        }
        thread::sleep(Duration::from_millis(20));
        assert_eq!(
            thumbnails.shared.state.lock().unwrap().results.len(),
            MAX_POSTER_RESULTS
        );
        assert!(thumbnails.drain().all(|poster| poster.result.is_err()));
        thumbnails.shutdown();
        assert!(thumbnails.shared.state.lock().unwrap().queue.is_empty());
        Ok(())
    }

    #[test]
    fn preview_slots_keep_latest_cancel_active_and_never_join_slow_work() {
        let shared = Arc::new(PreviewShared::default());
        let cancel = Arc::new(AtomicBool::new(false));
        shared.state.lock().unwrap().active = Some(cancel.clone());
        let (release, wait) = std::sync::mpsc::channel();
        let (finished, done) = std::sync::mpsc::channel();
        let worker = thread::spawn(move || {
            let _ = wait.recv();
            finished.send(()).unwrap();
        });
        let previews = Previews {
            shared: shared.clone(),
            worker: Some(worker),
        };
        for generation in 0..10_000 {
            previews.request(PreviewRequest {
                video: "generated-local.mp4".into(),
                time: 1.,
                generation,
            });
        }
        assert!(cancel.load(Ordering::Acquire));
        assert_eq!(
            shared
                .state
                .lock()
                .unwrap()
                .request
                .as_ref()
                .unwrap()
                .0
                .generation,
            9999
        );
        let started = Instant::now();
        drop(previews);
        assert!(
            started.elapsed() < Duration::from_millis(100),
            "Drop joined slow native work"
        );
        let state = shared.state.lock().unwrap();
        assert!(state.stop && state.request.is_none() && state.result.is_none());
        drop(state);
        release.send(()).unwrap();
        done.recv_timeout(Duration::from_secs(1)).unwrap();
    }

    #[test]
    fn cancellation_is_checked_before_opening_a_decoder() {
        let cancel = AtomicBool::new(true);
        let mut player = None;
        let mut open = None;
        let request = PreviewRequest {
            video: "does-not-exist.mp4".into(),
            time: 1.,
            generation: 1,
        };
        assert!(preview(&mut player, &mut open, &request, &cancel).is_err());
        assert!(player.is_none());
    }

    #[test]
    fn cache_cleanup_enforces_quota_without_touching_foreign_or_linked_files() -> Result<()> {
        let root = tempfile::tempdir()?;
        let cache = root.path().join("cache");
        std::fs::create_dir(&cache)?;
        let foreign = cache.join("family.png");
        std::fs::write(&foreign, b"keep")?;
        let old = cache.join("1111111111111111.png");
        let newer = cache.join("2222222222222222.png");
        std::fs::File::create(&old)?.set_len(CACHE_QUOTA_BYTES)?;
        thread::sleep(Duration::from_millis(2));
        std::fs::File::create(&newer)?.set_len(1024)?;
        let fresh_temporary = cache.join("5555555555555555.png.tmp");
        std::fs::write(&fresh_temporary, b"active writer")?;
        let outside = root.path().join("outside.png");
        std::fs::write(&outside, b"outside")?;
        #[cfg(unix)]
        let linked = {
            let path = cache.join("3333333333333333.png");
            std::os::unix::fs::symlink(&outside, &path)?;
            path
        };
        let directory = cache.join("4444444444444444.png");
        std::fs::create_dir(&directory)?;
        clean_cache(&cache, None, Some(&newer))?;
        assert!(!old.exists());
        assert!(newer.exists());
        assert!(
            foreign.exists() && outside.exists() && directory.exists() && fresh_temporary.exists()
        );
        #[cfg(unix)]
        assert!(std::fs::symlink_metadata(linked)?.file_type().is_symlink());
        Ok(())
    }

    #[test]
    fn cache_file_count_quota_and_stale_temporaries_are_cleaned() -> Result<()> {
        let cache = tempfile::tempdir()?;
        for index in 0..=CACHE_QUOTA_FILES {
            std::fs::write(cache.path().join(format!("{index:016x}.png")), b"")?;
        }
        let temporary = cache.path().join("ffffffffffffffff.png.tmp");
        let file = std::fs::File::create(&temporary)?;
        file.set_times(std::fs::FileTimes::new().set_modified(UNIX_EPOCH))?;
        let invalidations = clean_cache(cache.path(), None, None)?;
        assert!(invalidations.paths.contains(&temporary));
        assert_eq!(invalidations.paths.len(), 2);
        assert!(!invalidations.rescan_required);
        assert!(!temporary.exists());
        let count = std::fs::read_dir(cache.path())?
            .collect::<std::io::Result<Vec<_>>>()?
            .len();
        assert_eq!(count, CACHE_QUOTA_FILES);
        Ok(())
    }

    #[test]
    fn cache_removal_notifications_are_bounded_and_report_rescan_overflow() {
        let thumbnails = Thumbnails {
            shared: Arc::new(PosterShared::default()),
            worker: None,
        };
        let mut invalidations = CacheInvalidations::default();
        for index in 0..300 {
            invalidations.removed(PathBuf::from(format!("{index:016x}.png")));
        }
        thumbnails
            .shared
            .state
            .lock()
            .unwrap()
            .invalidations
            .merge(invalidations);
        let invalidations = thumbnails.take_cache_invalidations();
        assert_eq!(invalidations.paths.len(), 256);
        assert!(invalidations.rescan_required);
        let empty = thumbnails.take_cache_invalidations();
        assert!(empty.paths.is_empty() && !empty.rescan_required);
    }

    #[test]
    fn source_reconciliation_purges_obsolete_keys_but_preserves_offline_sources() -> Result<()> {
        let root = tempfile::tempdir()?;
        let video = root.path().join("generated.mp4");
        std::fs::write(&video, b"one")?;
        let cache = root.path().join("cache");
        std::fs::create_dir(&cache)?;
        let first = poster_path(&cache, &video)?;
        std::fs::write(&first, b"old")?;
        std::fs::write(&video, b"new version")?;
        let second = poster_path(&cache, &video)?;
        std::fs::write(&second, b"new")?;
        let sources = HashSet::from([video.clone()]);
        clean_cache(&cache, Some(&sources), None)?;
        assert!(!first.exists() && second.exists());
        std::fs::remove_file(&video)?;
        clean_cache(&cache, Some(&sources), None)?;
        assert!(
            second.exists(),
            "Unavailable source must not erase its cached poster"
        );
        clean_cache(&cache, Some(&HashSet::new()), None)?;
        assert!(!second.exists());
        Ok(())
    }

    #[test]
    #[ignore = "requires native libmpv and generated FLICK_TEST_VIDEO"]
    fn real_superseded_previews_cancel_and_idle_close_reopens() -> Result<()> {
        let video = PathBuf::from(std::env::var("FLICK_TEST_VIDEO")?);
        let previews = Previews::start();
        for generation in 0..1000 {
            previews.request(PreviewRequest {
                video: video.clone(),
                time: 2.,
                generation,
            });
        }
        previews.request(PreviewRequest {
            video: video.clone(),
            time: 8.,
            generation: 1000,
        });
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if let Some(preview) = previews.latest() {
                assert_eq!(preview.generation, 1000);
                break;
            }
            ensure!(Instant::now() < deadline, "Newest preview did not arrive");
            thread::sleep(Duration::from_millis(5));
        }
        previews.idle_close();
        assert!(previews.latest().is_none());
        previews.request(PreviewRequest {
            video,
            time: 2.,
            generation: 1001,
        });
        loop {
            if let Some(preview) = previews.latest() {
                assert_eq!(preview.generation, 1001);
                break;
            }
            ensure!(
                Instant::now() < deadline,
                "Preview did not reopen after idle-close"
            );
            thread::sleep(Duration::from_millis(5));
        }
        previews.cancel();
        assert!(previews.latest().is_none());
        Ok(())
    }

    /// Previews follow the newest request and differ between positions.
    #[test]
    #[ignore = "requires native libmpv and a moving-pattern FLICK_TEST_VIDEO"]
    fn real_previews_follow_the_newest_request() -> Result<()> {
        let video = PathBuf::from(std::env::var("FLICK_TEST_VIDEO")?);
        let previews = Previews::start();
        let mut frames = Vec::new();
        for (generation, time) in [(1, 2.), (2, 8.)] {
            previews.request(PreviewRequest {
                video: video.clone(),
                time,
                generation,
            });
            let deadline = Instant::now() + Duration::from_secs(15);
            let preview = loop {
                if let Some(preview) = previews.latest()
                    && preview.generation == generation
                {
                    break preview;
                }
                ensure!(Instant::now() < deadline, "Preview timed out");
                thread::sleep(Duration::from_millis(10));
            };
            assert_eq!(preview.frame.width, PREVIEW_WIDTH);
            frames.push(preview.frame.pixels);
        }
        assert!(
            frames[0] != frames[1],
            "Previews at 2 s and 8 s were identical"
        );
        Ok(())
    }

    /// Requires native libmpv and a generated FLICK_TEST_VIDEO.
    #[test]
    #[ignore = "requires native libmpv and FLICK_TEST_VIDEO"]
    fn real_posters_are_written_with_details() -> Result<()> {
        let video = PathBuf::from(std::env::var("FLICK_TEST_VIDEO")?);
        let cache = tempfile::tempdir()?;
        let thumbnails = Thumbnails::start(Some(cache.path().to_owned()));
        thumbnails.request(video.clone());
        thumbnails.request(cache.path().join("missing.mp4"));
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut results = Vec::new();
        while results.len() < 2 {
            results.extend(thumbnails.drain());
            ensure!(Instant::now() < deadline, "Thumbnails timed out");
            thread::sleep(Duration::from_millis(20));
        }
        let (poster, details) = results[0]
            .result
            .as_ref()
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        assert_eq!(results[0].video, video);
        let image = image::open(poster)?;
        assert_eq!(image.width(), POSTER_WIDTH);
        assert!(details.duration > 0. && details.width > 0 && details.height > 0);
        assert!(mean_brightness(image.to_rgba8().as_raw()) > DARK_FRAME);
        assert!(results[1].result.is_err());
        println!("{details:?} -> {}", poster.display());
        Ok(())
    }
}
