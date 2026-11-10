//! HLS delivery of transcodes: playlists, the segment job manager, and the
//! `/v1/hls` routes.
//!
//! The server mounts [`router`] after its own routes have been given their
//! state, and applies authentication as a layer over the merged router, so
//! this module does not check tokens itself:
//!
//! ```ignore
//! let hls = Arc::new(HlsState::new(encoder, config.max_transcodes, temp_dir, resolve));
//! let app = Router::new()
//!     .route("/v1/library", get(listing))
//!     // ...
//!     .with_state(state.clone())
//!     .merge(crate::remote::hls::router(hls.clone()));
//! // On shutdown:
//! hls.shutdown().await;
//! ```
//!
//! Routes: `GET /v1/hls/{id}/{quality}/index.m3u8`, `/init.mp4`, `/{n}.m4s`.
//!
//! One job exists per (video, tier). A job owns a folder of finished
//! segments and at most one FFmpeg run at a time. A segment request serves
//! the segment if it is there, waits if the run will reach it within
//! [`HlsLimits::lookahead`] segments, and otherwise restarts the run at that
//! segment. A request starts at most one run: if another request replaces
//! its run, as when two viewers watch the same video at the same tier at
//! different points, it answers 409 instead of taking the run back, so two
//! requests never restart FFmpeg in turn. Runs write into their own folder;
//! finished segments are moved into the job's folder, so segments from
//! earlier runs stay available.
//! A run that gets [`HlsLimits::max_ahead`] segments ahead of the last
//! request stops, and a later request near the end of what was made starts
//! the next run there. Segments behind the last request are removed once
//! the job holds more than [`HlsLimits::max_job_bytes`]. A job without a
//! request for [`HlsLimits::idle`] is stopped and its folder removed.
//!
//! At most `max_jobs` jobs exist. When a new one is needed at the cap, a job
//! nobody is waiting on is closed to make room if it is another tier of the
//! same video (the viewer switched tiers) or had no request for
//! [`HlsLimits::reclaim_after`]; the least recently used such job goes.
//! Otherwise the request gets 503, so viewers who are watching never take
//! turns closing each other's transcodes.

use super::protocol::{ErrorBody, HLS_SEGMENT_SECONDS, Quality};
use super::transcode::{Encoder, MediaInfo, command, job_args, stderr_tail};
use axum::{
    Json, Router,
    extract::{Path as UrlPath, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use std::{
    collections::{BTreeMap, HashMap},
    fmt::Write as _,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc, Mutex, MutexGuard, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{sync::watch, task::JoinHandle, time::timeout_at};
use tokio_util::sync::CancellationToken;

/// Folder names this module creates under the temporary folder.
const JOB_PREFIX: &str = "flick-hls-job-";
/// A request whose FFmpeg run another request replaced.
pub const CONFLICT_MESSAGE: &str = "Another viewer is watching this video at this quality at another point. Choose another quality or Original.";
/// Any route for an unverified torrent download; the media route says the same.
pub const INCOMPLETE_MESSAGE: &str =
    "This video is still downloading on the server and has not been verified yet.";

/// A source the server knows.
#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedVideo {
    pub path: PathBuf,
    pub info: MediaInfo,
    /// False while the file is an unverified torrent download; every HLS
    /// route then answers 409, as the media route does.
    pub complete: bool,
}

/// Looks up a video id; None answers 404.
pub type Resolver = Arc<dyn Fn(&str) -> Option<ResolvedVideo> + Send + Sync>;

/// Timing and size limits of the job manager. The defaults suit real use;
/// tests shorten them.
#[derive(Clone, Debug)]
pub struct HlsLimits {
    /// A job without requests for this long is stopped and removed.
    pub idle: Duration,
    /// How long a segment request waits for FFmpeg before answering 504.
    pub segment_wait: Duration,
    /// Finished segments a job keeps on disk before removing old ones.
    pub max_job_bytes: u64,
    /// A request this many segments past what the run has made waits for
    /// it instead of restarting the run.
    pub lookahead: u32,
    /// A run this many segments ahead of the last request stops.
    pub max_ahead: u32,
    /// A request within this many segments of the end of what was made
    /// starts the next run there. Less than `max_ahead`.
    pub resume_ahead: u32,
    /// How often a run checks its folder for finished segments.
    pub poll: Duration,
    /// At the job cap, a job of another video with no request for this
    /// long is closed to make room for a new one.
    pub reclaim_after: Duration,
}

impl Default for HlsLimits {
    fn default() -> Self {
        Self {
            idle: Duration::from_secs(60),
            segment_wait: Duration::from_secs(30),
            max_job_bytes: 1024 * 1024 * 1024,
            lookahead: 4,
            max_ahead: 45,
            resume_ahead: 20,
            poll: Duration::from_millis(100),
            reclaim_after: Duration::from_secs(10),
        }
    }
}

type JobKey = (String, Quality);

/// The transcode side of the server. Share it as `Arc<HlsState>`.
pub struct HlsState {
    encoder: Option<Encoder>,
    max_jobs: usize,
    temp_dir: PathBuf,
    resolve: Resolver,
    limits: HlsLimits,
    jobs: Mutex<HashMap<JobKey, Arc<Job>>>,
    /// Jobs closed to make room, still stopping FFmpeg and removing files.
    closing: Mutex<Vec<JoinHandle<()>>>,
    next_job: AtomicU64,
    reaper: AtomicBool,
}

impl HlsState {
    /// `encoder` is None when FFmpeg was not found; every route then answers
    /// 503. At most `max_jobs` (video, tier) jobs exist at once; see the
    /// module documentation for when one is closed to make room. Job folders
    /// are made inside `temp_dir`; leftovers from an earlier process that
    /// stopped without cleaning up are removed here.
    pub fn new(
        encoder: Option<Encoder>,
        max_jobs: usize,
        temp_dir: PathBuf,
        resolve: Resolver,
    ) -> Self {
        if let Ok(entries) = std::fs::read_dir(&temp_dir) {
            for entry in entries.flatten() {
                if entry.file_name().to_string_lossy().starts_with(JOB_PREFIX) {
                    let _ = std::fs::remove_dir_all(entry.path());
                }
            }
        }
        Self {
            encoder,
            max_jobs,
            temp_dir,
            resolve,
            limits: HlsLimits::default(),
            jobs: Mutex::new(HashMap::new()),
            closing: Mutex::new(Vec::new()),
            next_job: AtomicU64::new(0),
            reaper: AtomicBool::new(false),
        }
    }

    pub fn with_limits(mut self, limits: HlsLimits) -> Self {
        self.limits = limits;
        self
    }

    pub fn encoder(&self) -> Option<&Encoder> {
        self.encoder.as_ref()
    }

    /// The tiers to list: Original, plus every transcode when FFmpeg works.
    pub fn qualities(&self) -> Vec<Quality> {
        if self.encoder.is_some() {
            Quality::ALL.to_vec()
        } else {
            vec![Quality::Original]
        }
    }

    /// Number of (video, tier) jobs that exist now.
    pub fn job_count(&self) -> usize {
        lock(&self.jobs).len()
    }

    /// Stops every FFmpeg run, waits for each to exit, and removes the job
    /// folders. Requests made afterwards start new jobs.
    pub async fn shutdown(&self) {
        let jobs: Vec<_> = lock(&self.jobs).drain().map(|(_, job)| job).collect();
        for job in jobs {
            job.close().await;
        }
        let closing = std::mem::take(&mut *lock(&self.closing));
        for task in closing {
            let _ = task.await;
        }
    }

    /// The job for (`id`, `quality`), made if needed, with the caller
    /// counted as waiting on it. Counting happens under the jobs lock, so a
    /// job being handed out is never chosen to make room for another.
    fn job(
        self: &Arc<Self>,
        id: &str,
        quality: Quality,
        video: ResolvedVideo,
    ) -> Result<Waiting, Refusal> {
        let mut jobs = lock(&self.jobs);
        if let Some(job) = jobs.get(&(id.to_owned(), quality)) {
            return Ok(Waiting::new(job.clone()));
        }
        if jobs.len() >= self.max_jobs {
            let Some(victim) = self.reclaimable(&jobs, id) else {
                return Err(Refusal(
                    StatusCode::SERVICE_UNAVAILABLE,
                    format!(
                        "This server is already making {} transcode{} at once. Stop another stream, wait a few seconds, or choose Original quality.",
                        self.max_jobs,
                        if self.max_jobs == 1 { "" } else { "s" }
                    ),
                ));
            };
            let job = jobs.remove(&victim).expect("chosen from the map");
            tracing::debug!(dir = %job.root.display(), "Closing an unused transcode to make room");
            job.stop_runs();
            let mut closing = lock(&self.closing);
            closing.retain(|task| !task.is_finished());
            closing.push(tokio::spawn(async move { job.close().await }));
        }
        let root = self.temp_dir.join(format!(
            "{JOB_PREFIX}{}-{}",
            std::process::id(),
            self.next_job.fetch_add(1, Ordering::Relaxed)
        ));
        if let Err(e) = std::fs::create_dir_all(&root) {
            tracing::warn!(error = %e, dir = %root.display(), "Could not create a transcode folder");
            return Err(Refusal(
                StatusCode::INTERNAL_SERVER_ERROR,
                "The server could not create a folder for this transcode.".into(),
            ));
        }
        let (changed, _) = watch::channel(0);
        let job = Arc::new(Job {
            quality,
            count: segment_count(video.info.duration),
            video,
            root,
            limits: self.limits.clone(),
            encoder: self.encoder.clone().expect("checked before creating jobs"),
            changed,
            state: Mutex::new(JobState::new()),
        });
        jobs.insert((id.to_owned(), quality), job.clone());
        let waiting = Waiting::new(job);
        drop(jobs);
        if !self.reaper.swap(true, Ordering::AcqRel) {
            tokio::spawn(reap(Arc::downgrade(self), self.limits.idle));
        }
        Ok(waiting)
    }

    /// The job to close to make room for one of video `id`: nobody waiting
    /// on it, and either another tier of `id` or unused for
    /// [`HlsLimits::reclaim_after`]. The least recently used one wins.
    fn reclaimable(&self, jobs: &HashMap<JobKey, Arc<Job>>, id: &str) -> Option<JobKey> {
        jobs.iter()
            .filter_map(|(key, job)| {
                let state = lock(&job.state);
                let unused = state.last_used.elapsed() >= self.limits.reclaim_after;
                (state.waiters == 0 && (key.0 == id || unused))
                    .then(|| (state.last_used, key.clone()))
            })
            .min_by_key(|(last_used, _)| *last_used)
            .map(|(_, key)| key)
    }
}

impl Drop for HlsState {
    /// Without [`HlsState::shutdown`], still stop every run. Each run kills
    /// FFmpeg and removes its folders on the runtime; if the runtime is gone
    /// too, `kill_on_drop` stops FFmpeg and the next start removes folders.
    fn drop(&mut self) {
        let jobs: Vec<_> = lock(&self.jobs).drain().map(|(_, job)| job).collect();
        if jobs.is_empty() {
            return;
        }
        for job in &jobs {
            job.stop_runs();
        }
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                for job in jobs {
                    job.close().await;
                }
            });
        }
    }
}

/// Stops jobs that had no request for `idle`.
async fn reap(state: Weak<HlsState>, idle: Duration) {
    let tick = (idle / 4).clamp(Duration::from_millis(20), Duration::from_secs(1));
    loop {
        tokio::time::sleep(tick).await;
        let Some(state) = state.upgrade() else {
            return;
        };
        let stale: Vec<Arc<Job>> = {
            let mut jobs = lock(&state.jobs);
            let keys: Vec<JobKey> = jobs
                .iter()
                .filter(|(_, job)| job.idle_for() >= idle)
                .map(|(key, _)| key.clone())
                .collect();
            keys.iter().filter_map(|key| jobs.remove(key)).collect()
        };
        drop(state);
        for job in stale {
            tracing::debug!(dir = %job.root.display(), "Stopping an idle transcode");
            job.close().await;
        }
    }
}

struct Job {
    quality: Quality,
    video: ResolvedVideo,
    /// Segments in the playlist.
    count: u32,
    /// Holds `init.mp4`, finished `{n}.m4s`, and one `run-{gen}` folder per
    /// FFmpeg run.
    root: PathBuf,
    limits: HlsLimits,
    encoder: Encoder,
    /// Bumped whenever segments, the init file, or a run's outcome change.
    changed: watch::Sender<u64>,
    state: Mutex<JobState>,
}

struct JobState {
    /// Finished segments in `root`, with their sizes.
    segments: BTreeMap<u32, u64>,
    bytes: u64,
    init: bool,
    run: Option<Run>,
    generation: u64,
    /// Every run task that has not finished, so closing can wait for them.
    tasks: Vec<(CancellationToken, JoinHandle<()>)>,
    /// The last segment requested.
    playhead: u32,
    last_used: Instant,
    /// Requests waiting now; a job with waiters is never idle.
    waiters: usize,
    /// A run reached the end of the source here; later segments do not exist.
    end: Option<u32>,
    closed: bool,
}

impl JobState {
    fn new() -> Self {
        Self {
            segments: BTreeMap::new(),
            bytes: 0,
            init: false,
            run: None,
            generation: 0,
            tasks: Vec::new(),
            playhead: 0,
            last_used: Instant::now(),
            waiters: 0,
            end: None,
            closed: false,
        }
    }

    fn active_run(&self) -> Option<&Run> {
        self.run.as_ref().filter(|run| run.outcome.is_none())
    }
}

struct Run {
    generation: u64,
    start: u32,
    /// The next segment this run will finish.
    next: u32,
    cancel: CancellationToken,
    outcome: Option<Outcome>,
}

#[derive(Clone, Debug, PartialEq)]
enum Outcome {
    /// FFmpeg reached the end of the source.
    Finished,
    /// Stopped on purpose: replaced, too far ahead, or closing.
    Stopped,
    Failed(String),
}

/// A job with one request counted as waiting on it while this lives, even
/// if the request's future is dropped because the client went away.
struct Waiting(Arc<Job>);

impl Waiting {
    fn new(job: Arc<Job>) -> Self {
        lock(&job.state).waiters += 1;
        Self(job)
    }
}

impl Drop for Waiting {
    fn drop(&mut self) {
        let mut state = lock(&self.0.state);
        state.waiters -= 1;
        state.last_used = Instant::now();
    }
}

enum Next {
    Serve(PathBuf),
    Wait(watch::Receiver<u64>),
    Fail(Response),
}

impl Job {
    fn idle_for(&self) -> Duration {
        let state = lock(&self.state);
        if state.waiters > 0 {
            Duration::ZERO
        } else {
            state.last_used.elapsed()
        }
    }

    fn bump(&self) {
        self.changed.send_modify(|v| *v = v.wrapping_add(1));
    }

    /// Cancels every run without waiting.
    fn stop_runs(&self) {
        let mut state = lock(&self.state);
        state.closed = true;
        for (cancel, _) in &state.tasks {
            cancel.cancel();
        }
        if let Some(run) = state.run.as_mut().filter(|r| r.outcome.is_none()) {
            run.outcome = Some(Outcome::Stopped);
        }
        drop(state);
        self.bump();
    }

    /// Stops every run, waits for FFmpeg to exit, and removes the folder.
    async fn close(&self) {
        self.stop_runs();
        let tasks = std::mem::take(&mut lock(&self.state).tasks);
        for (_, task) in tasks {
            let _ = tokio::time::timeout(Duration::from_secs(10), task).await;
        }
        remove_dir(&self.root).await;
    }

    /// Starts a run at `start`, replacing the current one.
    fn start_run(self: &Arc<Self>, state: &mut JobState, start: u32) -> Result<(), Refusal> {
        if state.closed {
            return Err(Refusal(
                StatusCode::SERVICE_UNAVAILABLE,
                "This transcode was stopped. Try again.".into(),
            ));
        }
        if let Some(old) = state.run.as_mut() {
            old.cancel.cancel();
            if old.outcome.is_none() {
                old.outcome = Some(Outcome::Stopped);
            }
        }
        state.tasks.retain(|(_, task)| !task.is_finished());
        state.generation += 1;
        let generation = state.generation;
        let dir = self.root.join(format!("run-{generation}"));
        let spawned = std::fs::create_dir_all(&dir)
            .and_then(|()| std::fs::File::create(dir.join("ffmpeg.log")))
            .and_then(|log| {
                command(&self.encoder.ffmpeg)
                    .args(job_args(
                        &self.encoder,
                        &self.video.path,
                        &self.video.info,
                        self.quality,
                        start,
                        &dir,
                    ))
                    .stdout(Stdio::null())
                    .stderr(log)
                    .spawn()
            });
        let child = match spawned {
            Ok(child) => child,
            Err(e) => {
                tracing::warn!(error = %e, ffmpeg = %self.encoder.ffmpeg.display(), "Could not start FFmpeg");
                state.run = None;
                return Err(Refusal(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("The server could not start FFmpeg: {e}"),
                ));
            }
        };
        tracing::debug!(start, dir = %dir.display(), "Started a transcode run");
        let cancel = CancellationToken::new();
        state.run = Some(Run {
            generation,
            start,
            next: start,
            cancel: cancel.clone(),
            outcome: None,
        });
        let task = tokio::spawn(drive(
            self.clone(),
            generation,
            start,
            dir,
            child,
            cancel.clone(),
        ));
        state.tasks.push((cancel, task));
        self.bump();
        Ok(())
    }

    /// Answer for a request whose run, `started`, is gone and which may not
    /// start another: 409 when another request replaced it.
    fn lost(state: &JobState, started: u64) -> Response {
        if state.generation != started {
            error(StatusCode::CONFLICT, CONFLICT_MESSAGE.into())
        } else {
            error(
                StatusCode::SERVICE_UNAVAILABLE,
                "The transcode stopped before reaching this point. Try again.".into(),
            )
        }
    }

    /// Decides what a request for segment `n` does now. `waited` is the run
    /// the request last waited on, `started` the run it started, if any.
    fn next_step(
        self: &Arc<Self>,
        n: u32,
        waited: &mut Option<u64>,
        started: &mut Option<u64>,
    ) -> Next {
        let mut state = lock(&self.state);
        state.last_used = Instant::now();
        state.playhead = n;
        if state.segments.contains_key(&n) {
            self.resume_ahead(&mut state, n);
            return Next::Serve(self.root.join(format!("{n}.m4s")));
        }
        if n >= self.count || state.end.is_some_and(|end| n >= end) {
            return Next::Fail(not_found());
        }
        if let Some(run) = state.run.as_ref()
            && Some(run.generation) == *waited
            && let Some(Outcome::Failed(message)) = &run.outcome
        {
            return Next::Fail(error(
                StatusCode::BAD_GATEWAY,
                format!("FFmpeg could not transcode this video: {message}"),
            ));
        }
        let covered = state.active_run().is_some_and(|run| {
            run.start <= n && run.next <= n && n <= run.next.saturating_add(self.limits.lookahead)
        });
        if !covered {
            // At most one run per request: a request whose run was replaced
            // does not take it back, or two requests would restart FFmpeg
            // in turn for as long as they wait.
            if let Some(mine) = *started {
                return Next::Fail(Self::lost(&state, mine));
            }
            if let Err(refusal) = self.start_run(&mut state, n) {
                return Next::Fail(refusal.into_response());
            }
            *started = Some(state.generation);
        }
        *waited = state.run.as_ref().map(|run| run.generation);
        Next::Wait(self.changed.subscribe())
    }

    /// After serving `n`: if nothing is running and the segments made so
    /// far end soon after `n`, start the next run where they end.
    fn resume_ahead(self: &Arc<Self>, state: &mut JobState, n: u32) {
        if state.active_run().is_some() {
            return;
        }
        let mut frontier = n + 1;
        while state.segments.contains_key(&frontier) {
            frontier += 1;
        }
        let end = state.end.unwrap_or(self.count).min(self.count);
        if frontier < end && frontier - n <= self.limits.resume_ahead {
            let _ = self.start_run(state, frontier);
        }
    }

    /// Answers a request for segment `n`. The caller holds a [`Waiting`].
    async fn segment(self: &Arc<Self>, n: u32) -> Response {
        let deadline = tokio::time::Instant::now() + self.limits.segment_wait;
        let mut waited = None;
        let mut started = None;
        loop {
            match self.next_step(n, &mut waited, &mut started) {
                Next::Serve(path) => match tokio::fs::read(&path).await {
                    Ok(bytes) => return file(bytes, "video/iso.segment"),
                    // Removed between the check and the read: decide again.
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        let mut state = lock(&self.state);
                        if let Some(size) = state.segments.remove(&n) {
                            state.bytes -= size;
                        }
                    }
                    Err(e) => return read_failed(&path, e),
                },
                Next::Fail(response) => return response,
                Next::Wait(mut changed) => {
                    if timeout_at(deadline, changed.changed()).await.is_err() {
                        return error(
                            StatusCode::GATEWAY_TIMEOUT,
                            "The transcode did not reach this point in time. The server may be too slow for this quality.".into(),
                        );
                    }
                }
            }
        }
    }

    /// Answers a request for `init.mp4`, which any run makes. The caller
    /// holds a [`Waiting`].
    async fn init(self: &Arc<Self>) -> Response {
        let deadline = tokio::time::Instant::now() + self.limits.segment_wait;
        let mut waited = None;
        let mut started = None;
        loop {
            let changed = {
                let mut state = lock(&self.state);
                state.last_used = Instant::now();
                if state.init {
                    None
                } else {
                    if let Some(run) = state.run.as_ref()
                        && Some(run.generation) == waited
                        && let Some(Outcome::Failed(message)) = &run.outcome
                    {
                        return error(
                            StatusCode::BAD_GATEWAY,
                            format!("FFmpeg could not transcode this video: {message}"),
                        );
                    }
                    if state.active_run().is_none() {
                        // Like a segment request, start at most one run.
                        if let Some(mine) = started {
                            return Self::lost(&state, mine);
                        }
                        let start = state.playhead.min(self.count.saturating_sub(1));
                        if let Err(refusal) = self.start_run(&mut state, start) {
                            return refusal.into_response();
                        }
                        started = Some(state.generation);
                    }
                    waited = state.run.as_ref().map(|run| run.generation);
                    Some(self.changed.subscribe())
                }
            };
            let Some(mut changed) = changed else {
                let path = self.root.join("init.mp4");
                return match tokio::fs::read(&path).await {
                    Ok(bytes) => file(bytes, "video/mp4"),
                    Err(e) => read_failed(&path, e),
                };
            };
            if timeout_at(deadline, changed.changed()).await.is_err() {
                return error(
                    StatusCode::GATEWAY_TIMEOUT,
                    "The transcode did not start in time.".into(),
                );
            }
        }
    }
}

/// Watches one FFmpeg run: moves finished segments into the job folder,
/// stops the run when it is far enough ahead, records how it ended, and
/// removes its folder.
async fn drive(
    job: Arc<Job>,
    generation: u64,
    start: u32,
    dir: PathBuf,
    mut child: tokio::process::Child,
    cancel: CancellationToken,
) {
    let mut next = start;
    let mut ticker = tokio::time::interval(job.limits.poll);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let outcome = loop {
        tokio::select! {
            () = cancel.cancelled() => break Outcome::Stopped,
            status = child.wait() => {
                // Segments finished just before exit.
                collect(&job, generation, &dir, &mut next).await;
                break match status {
                    Ok(status) if status.success() => Outcome::Finished,
                    Ok(status) => {
                        let log = tokio::fs::read(dir.join("ffmpeg.log")).await.unwrap_or_default();
                        Outcome::Failed(format!("{status}: {}", stderr_tail(&log)))
                    }
                    Err(e) => Outcome::Failed(e.to_string()),
                };
            }
            _ = ticker.tick() => {
                if collect(&job, generation, &dir, &mut next).await {
                    break Outcome::Stopped;
                }
            }
        }
    };
    // Kill before removing the folder FFmpeg writes into.
    let _ = child.start_kill();
    let _ = child.wait().await;
    if let Outcome::Failed(message) = &outcome {
        tracing::warn!(%message, "FFmpeg transcode failed");
    }
    {
        let mut state = lock(&job.state);
        if outcome == Outcome::Finished {
            state.end = Some(state.end.map_or(next, |end| end.min(next)));
        }
        if let Some(run) = state.run.as_mut().filter(|r| r.generation == generation)
            && run.outcome.is_none()
        {
            run.outcome = Some(outcome);
        }
    }
    job.bump();
    remove_dir(&dir).await;
}

/// Moves finished segments from a run folder into the job folder. Returns
/// true when the run is far enough ahead of the playhead to stop.
async fn collect(job: &Job, generation: u64, dir: &Path, next: &mut u32) -> bool {
    let mut changed = false;
    let mut victims = Vec::new();
    let mut stop = false;
    loop {
        let name = format!("{next}.m4s");
        let source = dir.join(&name);
        let Ok(meta) = tokio::fs::metadata(&source).await else {
            break;
        };
        // The init file is written before the first segment is renamed.
        let need_init = !lock(&job.state).init;
        if need_init
            && tokio::fs::rename(dir.join("init.mp4"), job.root.join("init.mp4"))
                .await
                .is_ok()
        {
            lock(&job.state).init = true;
        }
        let have = lock(&job.state).segments.contains_key(next);
        if have {
            let _ = tokio::fs::remove_file(&source).await;
        } else if let Err(e) = tokio::fs::rename(&source, job.root.join(&name)).await {
            tracing::warn!(error = %e, "Could not keep a transcoded segment");
            break;
        }
        let mut state = lock(&job.state);
        if !have {
            state.segments.insert(*next, meta.len());
            state.bytes += meta.len();
        }
        *next += 1;
        if let Some(run) = state.run.as_mut().filter(|r| r.generation == generation) {
            run.next = *next;
        }
        victims.extend(evict(&mut state, &job.limits));
        stop = *next > state.playhead.saturating_add(job.limits.max_ahead);
        changed = true;
        if stop {
            break;
        }
    }
    if changed {
        job.bump();
    }
    for victim in victims {
        let _ = tokio::fs::remove_file(job.root.join(format!("{victim}.m4s"))).await;
    }
    stop
}

/// Over the size cap, drop the oldest segments behind the playhead, then
/// the farthest ones well ahead of it.
fn evict(state: &mut JobState, limits: &HlsLimits) -> Vec<u32> {
    let mut victims = Vec::new();
    let keep_until = state.playhead.saturating_add(limits.max_ahead);
    while state.bytes > limits.max_job_bytes {
        let behind = state
            .segments
            .keys()
            .next()
            .copied()
            .filter(|&k| k < state.playhead);
        let ahead = state
            .segments
            .keys()
            .next_back()
            .copied()
            .filter(|&k| k > keep_until);
        let Some(victim) = behind.or(ahead) else {
            break;
        };
        state.bytes -= state.segments.remove(&victim).unwrap_or(0);
        victims.push(victim);
    }
    victims
}

/// Removes a folder, retrying while a stopped process releases files.
async fn remove_dir(dir: &Path) {
    for attempt in 0..10 {
        match tokio::fs::remove_dir_all(dir).await {
            Ok(()) => return,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
            Err(e) if attempt == 9 => {
                tracing::warn!(error = %e, dir = %dir.display(), "Could not remove a transcode folder");
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    }
}

/// Segments in a playlist for `duration` seconds. A remainder shorter than
/// a tenth of a second is folded into the last segment, since FFmpeg may
/// not make a segment for it.
pub fn segment_count(duration: f64) -> u32 {
    if !duration.is_finite() || duration <= 0.0 {
        return 0;
    }
    let segment = f64::from(HLS_SEGMENT_SECONDS);
    let count = ((duration - 0.1) / segment).ceil().max(1.0);
    count.min(f64::from(u32::MAX)) as u32
}

/// A complete VOD playlist of `HLS_SEGMENT_SECONDS` segments, the last one
/// shorter, with relative segment URIs.
pub fn playlist(duration: f64, quality: Quality) -> String {
    let _ = quality; // Every tier uses the same segment timeline.
    let segment = f64::from(HLS_SEGMENT_SECONDS);
    let count = segment_count(duration);
    let mut text = format!(
        "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-TARGETDURATION:{HLS_SEGMENT_SECONDS}\n#EXT-X-MEDIA-SEQUENCE:0\n#EXT-X-PLAYLIST-TYPE:VOD\n#EXT-X-INDEPENDENT-SEGMENTS\n#EXT-X-MAP:URI=\"init.mp4\"\n"
    );
    for n in 0..count {
        let length = if n + 1 == count {
            duration - segment * f64::from(n)
        } else {
            segment
        };
        let _ = write!(text, "#EXTINF:{length:.6},\n{n}.m4s\n");
    }
    text.push_str("#EXT-X-ENDLIST\n");
    text
}

/// The `/v1/hls` routes.
pub fn router(state: Arc<HlsState>) -> Router {
    Router::new()
        .route("/v1/hls/{id}/{quality}/{file}", get(hls_file))
        .with_state(state)
}

async fn hls_file(
    State(state): State<Arc<HlsState>>,
    UrlPath((id, quality, file_name)): UrlPath<(String, String, String)>,
) -> Response {
    let Some(quality) = Quality::parse(&quality).filter(|q| q.is_transcode()) else {
        return not_found();
    };
    enum Want {
        Playlist,
        Init,
        Segment(u32),
    }
    let want = match file_name.as_str() {
        "index.m3u8" => Want::Playlist,
        "init.mp4" => Want::Init,
        other => match other
            .strip_suffix(".m4s")
            .and_then(|n| n.parse::<u32>().ok().filter(|v| v.to_string() == n))
        {
            Some(n) => Want::Segment(n),
            None => return not_found(),
        },
    };
    let Some(video) = (state.resolve)(&id) else {
        return not_found();
    };
    if !video.complete {
        return error(StatusCode::CONFLICT, INCOMPLETE_MESSAGE.into());
    }
    if state.encoder.is_none() {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "This server has no FFmpeg, so only Original quality is available. Install FFmpeg on the server and restart it.".into(),
        );
    }
    if segment_count(video.info.duration) == 0 {
        return error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "The server could not read this video's length, so it cannot transcode it. Choose Original quality.".into(),
        );
    }
    match want {
        Want::Playlist => (
            [
                (header::CONTENT_TYPE, "application/vnd.apple.mpegurl"),
                (header::CACHE_CONTROL, "no-cache"),
            ],
            playlist(video.info.duration, quality),
        )
            .into_response(),
        Want::Segment(n) if n >= segment_count(video.info.duration) => not_found(),
        Want::Segment(n) => match state.job(&id, quality, video) {
            Ok(waiting) => waiting.0.segment(n).await,
            Err(refusal) => refusal.into_response(),
        },
        Want::Init => match state.job(&id, quality, video) {
            Ok(waiting) => waiting.0.init().await,
            Err(refusal) => refusal.into_response(),
        },
    }
}

fn file(bytes: Vec<u8>, content_type: &'static str) -> Response {
    ([(header::CONTENT_TYPE, content_type)], bytes).into_response()
}

fn read_failed(path: &Path, e: std::io::Error) -> Response {
    tracing::warn!(error = %e, path = %path.display(), "Could not read a transcoded file");
    error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "The server could not read the transcoded file.".into(),
    )
}

/// A request the job manager turns down, answered as an [`ErrorBody`].
struct Refusal(StatusCode, String);

impl IntoResponse for Refusal {
    fn into_response(self) -> Response {
        error(self.0, self.1)
    }
}

fn not_found() -> Response {
    error(StatusCode::NOT_FOUND, "Not found.".into())
}

fn error(status: StatusCode, message: String) -> Response {
    (status, Json(ErrorBody { message })).into_response()
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn segments(text: &str) -> Vec<(String, String)> {
        let lines: Vec<&str> = text.lines().collect();
        lines
            .windows(2)
            .filter(|w| w[0].starts_with("#EXTINF:"))
            .map(|w| (w[0].to_owned(), w[1].to_owned()))
            .collect()
    }

    #[test]
    fn playlist_has_the_vod_header_and_a_short_last_segment() {
        let text = playlist(10.5, Quality::Low720);
        assert_eq!(
            text,
            "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:0\n#EXT-X-PLAYLIST-TYPE:VOD\n#EXT-X-INDEPENDENT-SEGMENTS\n#EXT-X-MAP:URI=\"init.mp4\"\n\
             #EXTINF:4.000000,\n0.m4s\n#EXTINF:4.000000,\n1.m4s\n#EXTINF:2.500000,\n2.m4s\n#EXT-X-ENDLIST\n"
        );
    }

    #[test]
    fn playlist_for_multiples_short_and_zero_durations() {
        let exact = segments(&playlist(12.0, Quality::Data480));
        assert_eq!(exact.len(), 3);
        assert!(exact.iter().all(|(inf, _)| inf == "#EXTINF:4.000000,"));
        assert_eq!(exact[2].1, "2.m4s");

        let short = segments(&playlist(1.25, Quality::High1080));
        assert_eq!(short, [("#EXTINF:1.250000,".into(), "0.m4s".into())]);

        // A sliver after the last boundary is folded into the last segment.
        let sliver = segments(&playlist(8.05, Quality::Standard1080));
        assert_eq!(sliver.len(), 2);
        assert_eq!(sliver[1].0, "#EXTINF:4.050000,");

        let long = segments(&playlist(5423.125, Quality::Low720));
        assert_eq!(long.len(), 1356);
        assert_eq!(long[1355], ("#EXTINF:3.125000,".into(), "1355.m4s".into()));

        for empty in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            let text = playlist(empty, Quality::Low720);
            assert!(segments(&text).is_empty());
            assert!(text.ends_with("#EXT-X-ENDLIST\n"));
        }
    }

    #[test]
    fn segment_counts() {
        assert_eq!(segment_count(0.0), 0);
        assert_eq!(segment_count(0.05), 1);
        assert_eq!(segment_count(4.0), 1);
        assert_eq!(segment_count(4.2), 2);
        assert_eq!(segment_count(400.0), 100);
    }

    #[test]
    fn eviction_removes_behind_the_playhead_first() {
        let limits = HlsLimits {
            max_job_bytes: 30,
            max_ahead: 3,
            ..Default::default()
        };
        let mut state = JobState::new();
        for n in [0, 1, 2, 5, 6, 20, 21] {
            state.segments.insert(n, 10);
        }
        state.bytes = 70;
        state.playhead = 5;
        // Behind first (0, 1, 2), then the farthest beyond 5 + 3 (21).
        assert_eq!(evict(&mut state, &limits), [0, 1, 2, 21]);
        assert_eq!(state.bytes, 30);
        assert_eq!(
            state.segments.keys().copied().collect::<Vec<_>>(),
            [5, 6, 20]
        );
        // Only the protected window is left; nothing more can go.
        state.bytes = 100;
        state.segments.remove(&20);
        assert_eq!(evict(&mut state, &limits), Vec::<u32>::new());
    }
}
