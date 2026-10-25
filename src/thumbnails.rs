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
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
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

pub struct Thumbnails {
    requests: Option<mpsc::Sender<PathBuf>>,
    results: mpsc::Receiver<Poster>,
    worker: Option<thread::JoinHandle<()>>,
    /// Set on drop so the worker skips files still queued.
    stop: Arc<AtomicBool>,
}

impl Thumbnails {
    /// Starts the worker. Without a cache directory, requests fail with a
    /// message rather than writing elsewhere.
    pub fn start(cache: Option<PathBuf>) -> Self {
        let (requests, queue) = mpsc::channel::<PathBuf>();
        let (sender, results) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let worker = thread::Builder::new()
            .name("flick-thumbnails".into())
            .spawn(move || {
                let mut player = None;
                for video in queue {
                    if stopped.load(Ordering::Relaxed) {
                        break;
                    }
                    let result = cache
                        .as_deref()
                        .context("No cache directory is available for thumbnails")
                        .and_then(|cache| poster(&mut player, &video, cache))
                        .map_err(|error| format!("{error:#}"));
                    if sender.send(Poster { video, result }).is_err() {
                        break;
                    }
                }
            })
            .ok();
        Self {
            requests: worker.is_some().then_some(requests),
            results,
            worker,
            stop,
        }
    }

    pub fn request(&self, video: PathBuf) {
        if let Some(requests) = &self.requests {
            let _ = requests.send(video);
        }
    }

    pub fn drain(&self) -> impl Iterator<Item = Poster> + '_ {
        self.results.try_iter()
    }
}

impl Drop for Thumbnails {
    fn drop(&mut self) {
        // A closed queue still yields what was sent, so the flag makes the
        // worker stop after its current file instead of finishing the queue.
        self.stop.store(true, Ordering::Relaxed);
        self.requests.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// Preview width for timeline hovers.
const PREVIEW_WIDTH: u32 = 192;

pub struct PreviewRequest {
    pub video: PathBuf,
    pub time: f64,
    /// Echoed in the result so stale previews can be ignored.
    pub generation: u64,
}

pub struct Preview {
    pub generation: u64,
    pub frame: crate::playback::VideoFrame,
}

/// Frames for timeline hover previews from a second silent player, which
/// keeps the hovered video open and seeks to the nearest keyframe. Only the
/// newest pending request is decoded.
pub struct Previews {
    requests: Option<mpsc::Sender<PreviewRequest>>,
    results: mpsc::Receiver<Preview>,
    worker: Option<thread::JoinHandle<()>>,
}

impl Previews {
    pub fn start() -> Self {
        let (requests, queue) = mpsc::channel::<PreviewRequest>();
        let (sender, results) = mpsc::channel();
        let worker = thread::Builder::new()
            .name("flick-previews".into())
            .spawn(move || {
                let mut player = None;
                let mut open: Option<PathBuf> = None;
                while let Ok(mut request) = queue.recv() {
                    // Skip to the newest hover position.
                    while let Ok(newer) = queue.try_recv() {
                        request = newer;
                    }
                    match preview(&mut player, &mut open, &request) {
                        Ok(frame) => {
                            let preview = Preview {
                                generation: request.generation,
                                frame,
                            };
                            if sender.send(preview).is_err() {
                                break;
                            }
                        }
                        Err(error) => {
                            open = None;
                            tracing::debug!(%error, "No timeline preview");
                        }
                    }
                }
            })
            .ok();
        Self {
            requests: worker.is_some().then_some(requests),
            results,
            worker,
        }
    }

    pub fn request(&self, request: PreviewRequest) {
        if let Some(requests) = &self.requests {
            let _ = requests.send(request);
        }
    }

    /// The newest finished preview, if any.
    pub fn latest(&self) -> Option<Preview> {
        self.results.try_iter().last()
    }
}

impl Drop for Previews {
    fn drop(&mut self) {
        self.requests.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn preview(
    player: &mut Option<Player>,
    open: &mut Option<PathBuf>,
    request: &PreviewRequest,
) -> Result<crate::playback::VideoFrame> {
    let player = match player {
        Some(player) => player,
        None => player.insert(Player::silent()?),
    };
    let time = format!("{:.3}", request.time.max(0.));
    let deadline = Instant::now() + POSTER_DEADLINE;
    if open.as_deref() == Some(request.video.as_path()) {
        player.command(&["seek", &time, "absolute+keyframes"])?;
    } else {
        player.set_property("start", &time)?;
        player.load(
            request
                .video
                .to_str()
                .context("Video path must be valid UTF-8")?,
        )?;
        player.set_property("pause", "yes")?;
        let mut loaded = false;
        while !loaded {
            for event in player.poll_events() {
                match event {
                    PlayerEvent::FileLoaded => loaded = true,
                    PlayerEvent::Error(error) => bail!(error),
                    PlayerEvent::EndFile => {}
                }
            }
            ensure!(Instant::now() < deadline, "Timed out opening the video");
            thread::sleep(Duration::from_millis(5));
        }
        *open = Some(request.video.clone());
    }
    loop {
        player.poll_events();
        if let Some((width, height)) = player.video_size() {
            let (w, h) = preview_size(width, height);
            if let Some(frame) = player.render(w, h)? {
                return Ok(frame);
            }
        }
        ensure!(Instant::now() < deadline, "Timed out decoding a preview");
        thread::sleep(Duration::from_millis(5));
    }
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
) -> Result<(PathBuf, MediaDetails)> {
    let target = poster_path(cache, video)?;
    let player = match player {
        Some(player) => player,
        None => player.insert(Player::silent()?),
    };
    let mut frame = None;
    let mut details = None;
    // Openings and fades are often black; look later if the first try is.
    for start in ["20%", "50%"] {
        let (pixels, found) = capture(player, video, start)?;
        let dark = mean_brightness(&pixels.pixels) < DARK_FRAME;
        frame = Some(pixels);
        details = Some(found);
        if !dark {
            break;
        }
    }
    let _ = player.command(&["stop"]);
    let (frame, details) = (frame.context("No frame")?, details.context("No details")?);
    save_png(&target, frame)?;
    Ok((target, details))
}

fn capture(
    player: &Player,
    video: &Path,
    start: &str,
) -> Result<(crate::playback::VideoFrame, MediaDetails)> {
    player.set_property("start", start)?;
    player.load(video.to_str().context("Video path must be valid UTF-8")?)?;
    player.set_property("pause", "yes")?;
    let deadline = Instant::now() + POSTER_DEADLINE;
    let mut loaded_at = None;
    loop {
        for event in player.poll_events() {
            match event {
                PlayerEvent::FileLoaded => loaded_at = Some(Instant::now()),
                PlayerEvent::Error(error) => bail!(error),
                PlayerEvent::EndFile if loaded_at.is_none() => {
                    bail!("The video ended before it loaded")
                }
                PlayerEvent::EndFile => {}
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
