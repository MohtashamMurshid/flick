//! UI-safe player control. The native !Send player is constructed, used and
//! destroyed on one owned worker; only owned messages cross that boundary.

use crate::playback::{
    OperationId, OperationResult, PlaybackSnapshot, Player, PlayerEvent, RenderedFrames,
    StreamOpener,
};
use anyhow::{Result, anyhow, ensure};
use std::{
    collections::VecDeque,
    sync::{Arc, Condvar, Mutex, MutexGuard},
    thread,
    time::{Duration, Instant},
};
use tokio::sync::oneshot;

const COMMAND_LIMIT: usize = 64;
const NO_GENERATION: u64 = u64::MAX;
const STOP_TIMEOUT: Duration = Duration::from_secs(10);
// A recovery deadline, not a delay or evidence that a seek succeeded. Some
// demuxers silently reject a seek without producing PLAYBACK_RESTART.
const SEEK_RECOVERY_TIMEOUT: Duration = Duration::from_secs(10);
const SEEK_POSITION_TOLERANCE: f64 = 0.05;
type StopReply = oneshot::Sender<std::result::Result<(), String>>;
pub type FramesReady = oneshot::Receiver<std::result::Result<RenderedFrames, String>>;

enum Work {
    Load(String),
    Command(Vec<String>),
    Property(String, String, bool),
    Seek(f64, bool),
    Stop(StopReply),
}

struct Job {
    request: u64,
    work: Work,
}

#[derive(Default)]
struct Queue {
    work: VecDeque<Job>,
    target: (u32, u32),
    stopped: bool,
}

impl Queue {
    fn push(&mut self, job: Job) -> Result<()> {
        ensure!(!self.stopped, "Playback is no longer running");
        let replace = self.work.back().is_some_and(|old| {
            match (&old.work, &job.work) {
                // Only adjacent loads may supersede one another. Never remove
                // a load across a property, command, seek or stop barrier.
                (Work::Load(_), Work::Load(_)) => true,
                (Work::Seek(..), Work::Seek(..)) => old.request == job.request,
                (
                    Work::Property(old_name, _, old_optional),
                    Work::Property(new_name, _, new_optional),
                ) => {
                    old.request == job.request
                        && old_name == new_name
                        && old_optional == new_optional
                }
                _ => false,
            }
        });
        // Reserve one extra slot for a deletion barrier, even under saturation.
        let limit = COMMAND_LIMIT + usize::from(matches!(job.work, Work::Stop(_)));
        ensure!(
            replace || self.work.len() < limit,
            "Playback is busy; try again shortly"
        );
        if replace {
            self.work.pop_back();
        }
        self.work.push_back(job);
        Ok(())
    }

    fn close(&mut self, reason: &str) {
        self.stopped = true;
        self.target = (0, 0);
        for job in self.work.drain(..) {
            if let Work::Stop(reply) = job.work {
                let _ = reply.send(Err(reason.to_owned()));
            }
        }
    }
}

#[derive(Default)]
struct Facts {
    hardware_decoder: Option<String>,
    dropped_frames: Option<u64>,
}

struct Event {
    // Startup/worker failures have no file generation and survive replacement.
    request: Option<u64>,
    event: PlayerEvent,
}

struct State {
    queue: Queue,
    snapshot: PlaybackSnapshot,
    facts: Facts,
    events: VecDeque<Event>,
    requested: u64,
    accepted_generation: u64,
    revision: u64,
}

impl Default for State {
    fn default() -> Self {
        Self {
            queue: Queue::default(),
            snapshot: PlaybackSnapshot::default(),
            facts: Facts::default(),
            events: VecDeque::new(),
            requested: 0,
            accepted_generation: NO_GENERATION,
            revision: 0,
        }
    }
}

impl State {
    fn reset_media(&mut self) {
        let (volume, muted, speed) = (
            self.snapshot.volume,
            self.snapshot.muted,
            self.snapshot.speed,
        );
        self.snapshot = PlaybackSnapshot {
            volume,
            muted,
            speed,
            ..PlaybackSnapshot::default()
        };
        self.facts = Facts::default();
        self.accepted_generation = NO_GENERATION;
    }

    fn transition(&mut self, work: Work) -> Result<()> {
        let request = self
            .requested
            .checked_add(1)
            .ok_or_else(|| anyhow!("Playback request limit reached"))?;
        self.queue.push(Job { request, work })?;
        self.requested = request;
        self.revision = self.revision.wrapping_add(1);
        self.queue.target = (0, 0);
        self.reset_media();
        self.events.retain(|event| event.request.is_none());
        Ok(())
    }

    fn send(&mut self, work: Work) -> Result<()> {
        self.queue.push(Job {
            request: self.requested,
            work,
        })?;
        // Admission and the optimistic UI cache update share one lock. The
        // worker cannot publish between them or overwrite a newer admission.
        match &self.queue.work.back().expect("admitted job").work {
            Work::Command(args) if args == &["cycle", "pause"] => {
                self.snapshot.paused = !self.snapshot.paused;
            }
            Work::Property(name, value, _) => match name.as_str() {
                "pause" if matches!(value.as_str(), "yes" | "no") => {
                    self.snapshot.paused = value == "yes"
                }
                "mute" if matches!(value.as_str(), "yes" | "no") => {
                    self.snapshot.muted = value == "yes"
                }
                "volume" | "speed" => {
                    if let Ok(value) = value.parse::<f64>()
                        && value.is_finite()
                    {
                        if name == "volume" {
                            self.snapshot.volume = value;
                        } else {
                            self.snapshot.speed = value;
                        }
                    }
                }
                _ => {}
            },
            Work::Seek(seconds, _) => self.snapshot.position = *seconds,
            _ => {}
        }
        self.revision = self.revision.wrapping_add(1);
        Ok(())
    }

    fn event(&mut self, request: Option<u64>, event: PlayerEvent) {
        if request.is_some_and(|request| request != self.requested) {
            return;
        }
        if self.events.len() >= COMMAND_LIMIT {
            self.events.pop_front();
        }
        self.events.push_back(Event { request, event });
    }

    fn publish(&mut self, request: u64, revision: u64, snapshot: PlaybackSnapshot) {
        if !self.queue.stopped
            && request == self.requested
            && revision == self.revision
            && self.queue.work.is_empty()
        {
            self.snapshot = snapshot;
        }
    }
}

#[derive(Default)]
struct Shared {
    state: Mutex<State>,
    wake: Condvar,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|error| error.into_inner())
    }

    fn finish(&self, error: Option<String>) {
        let mut state = self.lock();
        let reason = error.as_deref().unwrap_or("Playback is no longer running");
        state.queue.close(reason);
        state.reset_media();
        if let Some(error) = error {
            state.event(None, PlayerEvent::Error(error));
        }
        self.wake.notify_all();
    }
}

pub struct PlayerProxy {
    shared: Arc<Shared>,
}

impl PlayerProxy {
    pub fn start(opener: Arc<StreamOpener>) -> Result<(Self, FramesReady)> {
        let shared = Arc::new(Shared::default());
        let (ready, frames) = oneshot::channel();
        let worker_shared = shared.clone();
        thread::Builder::new()
            .name("flick-player-control".into())
            .spawn(move || {
                // Unwinding still destroys the native player on this worker and
                // closes queued deletion acknowledgements with an explicit error.
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    run(worker_shared.clone(), opener, ready)
                }));
                let error = match result {
                    Ok(Ok(())) => None,
                    Ok(Err(error)) => Some(format!("Playback is unavailable. {error:#}")),
                    Err(_) => Some("Playback worker failed unexpectedly".into()),
                };
                worker_shared.finish(error);
            })?;
        Ok((Self { shared }, frames))
    }

    fn send(&self, work: Work) -> Result<()> {
        self.shared.lock().send(work)?;
        self.shared.wake.notify_one();
        Ok(())
    }

    pub fn load(&self, source: &str) -> Result<()> {
        ensure!(
            !source.is_empty() && !source.contains('\0'),
            "Invalid video source"
        );
        self.shared
            .lock()
            .transition(Work::Load(source.to_owned()))?;
        self.shared.wake.notify_one();
        Ok(())
    }

    pub fn command(&self, args: &[&str]) -> Result<()> {
        ensure!(
            !args.is_empty() && !args.iter().any(|value| value.contains('\0')),
            "Invalid playback command"
        );
        // absolute-percent and relative seeks must keep their native units.
        // Bare absolute seeks are exact by default in mpv, too.
        if args.len() == 3 && args[0] == "seek" {
            let flags: Vec<_> = args[2].split('+').collect();
            if flags.contains(&"absolute")
                && flags
                    .iter()
                    .all(|flag| matches!(*flag, "absolute" | "exact" | "keyframes"))
            {
                let seconds = args[1].parse::<f64>()?;
                ensure!(seconds.is_finite(), "Invalid seek position");
                // Negative native absolute positions count from the end.
                // Keep those and conflicting flags for mpv to interpret.
                if seconds >= 0. && !(flags.contains(&"exact") && flags.contains(&"keyframes")) {
                    return self.seek(seconds, !flags.contains(&"keyframes"));
                }
            }
        }
        self.send(Work::Command(
            args.iter().map(|value| (*value).to_owned()).collect(),
        ))
    }

    pub fn set_property(&self, name: &str, value: &str) -> Result<()> {
        self.property(name, value, false)
    }

    /// Best-effort styling/version compatibility. Native failures are logged
    /// with the property name; required user controls still use set_property.
    pub fn set_optional_property(&self, name: &str, value: &str) -> Result<()> {
        self.property(name, value, true)
    }

    fn property(&self, name: &str, value: &str, optional: bool) -> Result<()> {
        ensure!(
            !name.is_empty() && !name.contains('\0') && !value.contains('\0'),
            "Invalid playback property"
        );
        self.send(Work::Property(name.to_owned(), value.to_owned(), optional))
    }

    pub fn seek(&self, seconds: f64, exact: bool) -> Result<()> {
        ensure!(seconds.is_finite(), "Invalid seek position");
        self.send(Work::Seek(seconds.max(0.), exact))
    }

    pub fn snapshot(&self) -> PlaybackSnapshot {
        self.shared.lock().snapshot.clone()
    }

    pub fn poll_events(&self) -> Vec<PlayerEvent> {
        self.shared
            .lock()
            .events
            .drain(..)
            .map(|event| event.event)
            .collect()
    }

    pub fn frame_generation(&self) -> u64 {
        self.shared.lock().accepted_generation
    }

    pub fn set_render_target(&self, width: u32, height: u32) {
        let mut state = self.shared.lock();
        if !state.queue.stopped && state.queue.target != (width, height) {
            state.queue.target = (width, height);
            state.revision = state.revision.wrapping_add(1);
            self.shared.wake.notify_one();
        }
    }

    pub fn hardware_decoder(&self) -> Option<String> {
        self.shared.lock().facts.hardware_decoder.clone()
    }

    pub fn dropped_frames(&self) -> Option<u64> {
        self.shared.lock().facts.dropped_frames
    }

    /// An ordered barrier: acknowledge only once mpv has unloaded its file.
    /// Waiting and native property reads happen on the worker, never the UI.
    pub fn stop_for_delete(&self) -> oneshot::Receiver<std::result::Result<(), String>> {
        let (reply, receiver) = oneshot::channel();
        let mut state = self.shared.lock();
        // Keep the sender here until admission succeeds so overload/closure
        // reports an explicit error, rather than silently canceling a receiver.
        let request = state.requested.checked_add(1);
        let error = if state.queue.stopped {
            Some("Playback is no longer running")
        } else if state.queue.work.len() > COMMAND_LIMIT {
            Some("Playback is busy; try again shortly")
        } else if request.is_none() {
            Some("Playback request limit reached")
        } else {
            None
        };
        if let Some(error) = error {
            let _ = reply.send(Err(error.into()));
        } else {
            // Do not clear commands or other stop replies. A stop cannot cross
            // preceding controls, nor can a later load cross this barrier.
            state
                .transition(Work::Stop(reply))
                .expect("checked stop admission");
            self.shared.wake.notify_one();
        }
        receiver
    }
}

impl Drop for PlayerProxy {
    fn drop(&mut self) {
        self.shared.finish(None);
        // No join: native shutdown and the renderer join stay on their owner.
    }
}

struct PendingStop {
    reply: StopReply,
    started: Instant,
}

struct ActiveOperation {
    id: OperationId,
    request: u64,
    optional_property: Option<String>,
}

struct PlayingSeek {
    position: f64,
    max_speed: f64,
}

struct SeekFence {
    id: OperationId,
    restarts: u64,
    target: f64,
    no_op: bool,
    frame_tolerance: f64,
    playing: Option<PlayingSeek>,
    started: Instant,
}

impl SeekFence {
    fn observe_speed(&mut self, player: &Player) {
        if let Some(playing) = &mut self.playing
            && let Some(speed) = native_speed(player)
        {
            playing.max_speed = playing.max_speed.max(speed);
        }
    }

    fn position_agrees(&self, position: f64, elapsed: Duration, restarted: bool) -> bool {
        if position >= self.target - SEEK_POSITION_TOLERANCE
            && position <= self.target + self.frame_tolerance
        {
            return true;
        }
        let Some(playing) = &self.playing else {
            return false;
        };
        if !restarted {
            return false;
        }
        // A running clock can pass the exact target before we observe its
        // restart. Bound that progress by time since submission at the highest
        // native rate observed while pending. A later speed change or pause
        // cannot erase progress already made. Seeks submitted while paused
        // retain strict target agreement because they have no playing clock.
        let advance = elapsed.as_secs_f64() * playing.max_speed;
        let reachable = |start| {
            position >= start - SEEK_POSITION_TOLERANCE
                && position <= start + advance + self.frame_tolerance
        };
        // The pre-seek clock can also keep running, or an older untagged
        // restart can arrive. An overlapping trajectory is ambiguous; retain
        // protection rather than accepting that old position as confirmation.
        reachable(self.target) && !reachable(playing.position)
    }

    fn pending(&self, player: &Player) -> bool {
        let elapsed = self.started.elapsed();
        let expired = elapsed >= SEEK_RECOVERY_TIMEOUT;
        if expired || self.no_op || player.playback_restarts() != self.restarts {
            // A restart is untagged and can belong to an earlier scrub. Read
            // current native scalars; never treat the optimistic cache, a
            // command reply, or an unrelated restart as seek completion.
            let position = player.refresh_position();
            if expired {
                return false;
            }
            if let Some(position) = position {
                // The no-op exception only permits strict target agreement;
                // accepting progress still requires a newer native restart.
                if self.position_agrees(
                    position,
                    elapsed,
                    player.playback_restarts() != self.restarts,
                ) && player.diagnostic_property("seeking").as_deref() == Some("no")
                {
                    return false;
                }
            }
        }
        true
    }
}

fn native_idle(player: &Player) -> bool {
    player.diagnostic_property("idle-active").as_deref() == Some("yes")
        && player.diagnostic_property("playlist-count").as_deref() == Some("0")
}

fn position_property(name: &str) -> bool {
    matches!(
        name,
        "chapter"
            | "time-pos"
            | "playback-time"
            | "percent-pos"
            | "playlist-pos"
            | "playlist-pos-1"
    )
}

fn command_changes_position(args: &[String]) -> bool {
    args.first().is_some_and(|name| {
        matches!(
            name.as_str(),
            "stop"
                | "seek"
                | "revert-seek"
                | "frame-step"
                | "frame-back-step"
                | "playlist-next"
                | "playlist-prev"
                | "playlist-play-index"
                | "loadfile"
                | "loadlist"
        ) || (matches!(name.as_str(), "set" | "add" | "cycle" | "multiply")
            && args
                .get(1)
                .is_some_and(|property| position_property(property)))
    })
}

fn native_speed(player: &Player) -> Option<f64> {
    player
        .diagnostic_property("speed")?
        .parse::<f64>()
        .ok()
        .filter(|speed| speed.is_finite() && *speed > 0.)
}

fn operation_failed(shared: &Shared, request: u64, optional_property: Option<&str>, error: String) {
    if let Some(property) = optional_property {
        tracing::debug!(property, %error, "Optional playback property is unavailable");
    } else {
        shared
            .lock()
            .event(Some(request), PlayerEvent::Error(error));
    }
}

fn run(
    shared: Arc<Shared>,
    opener: Arc<StreamOpener>,
    ready: oneshot::Sender<std::result::Result<RenderedFrames, String>>,
) -> Result<()> {
    let mut player = match Player::new() {
        Ok(player) => player,
        Err(error) => {
            let _ = ready.send(Err(format!("{error:#}")));
            return Err(error);
        }
    };
    let protocol_error = player
        .register_protocol(crate::services::STREAM_PROTOCOL, opener)
        .err()
        .map(|error| format!("Watching while downloading is unavailable. {error:#}"));
    if let Some(error) = &protocol_error {
        tracing::warn!(%error);
    }
    let frames = match player.render_in_background() {
        Ok(frames) => frames,
        Err(error) => {
            let _ = ready.send(Err(format!("{error:#}")));
            return Err(error);
        }
    };
    if shared.lock().queue.stopped {
        let _ = ready.send(Err("Playback was closed during startup".into()));
        return Ok(());
    }
    ensure!(
        ready.send(Ok(frames)).is_ok(),
        "Video output closed during startup"
    );
    let mut active_request = 0;
    let mut has_source = false;
    let mut file_loaded = false;
    let mut loaded_snapshot: Option<PlaybackSnapshot> = None;
    let mut final_snapshot: Option<PlaybackSnapshot> = None;
    let mut operation: Option<ActiveOperation> = None;
    let mut seek_fence: Option<SeekFence> = None;
    let mut stopping: Option<PendingStop> = None;
    let mut facts_at = Instant::now();
    loop {
        if shared.lock().queue.stopped {
            if let Some(stop) = stopping.take() {
                let _ = stop
                    .reply
                    .send(Err("Playback closed before file release".into()));
            }
            break;
        }
        let events = player.poll_events();
        if let Some(fence) = &mut seek_fence {
            // Observe confirmed native rate changes even while later queued
            // controls prevent snapshot publication. Reading only at publish
            // could miss a temporary faster rate followed by a pause/slowdown.
            fence.observe_speed(&player);
        }
        let file_failed = events
            .iter()
            .any(|event| matches!(event, PlayerEvent::Error(_)));
        if file_failed {
            seek_fence = None;
        }
        let mut refresh_snapshot = false;
        for event in events {
            match event {
                PlayerEvent::OperationCompleted { id, result } => {
                    if operation
                        .as_ref()
                        .is_some_and(|operation| operation.id == id)
                    {
                        let operation = operation.take().expect("active operation");
                        if let OperationResult::Failed(error) = result {
                            if seek_fence.as_ref().is_some_and(|fence| fence.id == id) {
                                seek_fence = None;
                                player.refresh_position();
                            }
                            operation_failed(
                                &shared,
                                operation.request,
                                operation.optional_property.as_deref(),
                                error,
                            );
                        }
                    }
                }
                PlayerEvent::EndFile => {
                    seek_fence = None;
                    // The native event omits its reason. Actor-owned stops and
                    // replacements already clear file_loaded; a non-error end
                    // of the loaded file is EOF. Preserve the last observations
                    // even if native unload notifications cleared its cache.
                    let natural_end = has_source && file_loaded && !file_failed;
                    has_source = false;
                    file_loaded = false;
                    let observed = player.cached_snapshot();
                    let mut state = shared.lock();
                    if natural_end {
                        let mut snapshot = if observed.duration > 0. {
                            observed
                        } else if active_request == state.requested && state.snapshot.duration > 0.
                        {
                            state.snapshot.clone()
                        } else {
                            loaded_snapshot.clone().unwrap_or_default()
                        };
                        snapshot.position = snapshot.duration.max(snapshot.position);
                        snapshot.eof = true;
                        snapshot.paused = true;
                        final_snapshot = Some(snapshot.clone());
                        if active_request == state.requested && state.queue.work.is_empty() {
                            state.snapshot = snapshot;
                        }
                    } else if active_request == state.requested {
                        final_snapshot = None;
                        state.reset_media();
                    }
                    state.event(Some(active_request), PlayerEvent::EndFile);
                }
                PlayerEvent::FileLoaded => {
                    if has_source {
                        file_loaded = true;
                        refresh_snapshot = true;
                        shared
                            .lock()
                            .event(Some(active_request), PlayerEvent::FileLoaded);
                    }
                }
                event => shared.lock().event(Some(active_request), event),
            }
        }
        if refresh_snapshot {
            // Refresh once at the file boundary. Until FILE_LOADED, the native
            // cache may still contain the previous file's pause/EOF/metadata.
            loaded_snapshot = Some(player.snapshot());
        }
        if stopping.is_some() {
            // Every started file must end, even if idle/empty arrives before
            // descriptor closure. A failed load may already have ended; stop
            // clears its retained playlist entry without a second END_FILE.
            let released = player.files_ended() && native_idle(&player);
            if released
                || stopping
                    .as_ref()
                    .is_some_and(|stop| stop.started.elapsed() >= STOP_TIMEOUT)
            {
                let stop = stopping.take().expect("pending stop");
                let result = if released {
                    Ok(())
                } else {
                    Err("Playback did not acknowledge file release; your file was kept".into())
                };
                let _ = stop.reply.send(result);
            }
        }
        let job = {
            let mut state = shared.lock();
            if !state.queue.stopped && !player.operations_pending() && stopping.is_none() {
                state.queue.work.pop_front()
            } else {
                None
            }
        };
        if let Some(Job { request, work }) = job {
            let result = match work {
                Work::Load(source) => {
                    seek_fence = None;
                    active_request = request;
                    has_source = false;
                    file_loaded = false;
                    loaded_snapshot = None;
                    final_snapshot = None;
                    let result = if let Some(error) = &protocol_error
                        && source.starts_with(&format!("{}://", crate::services::STREAM_PROTOCOL))
                    {
                        Err(anyhow!(error.clone()))
                    } else {
                        player.load(&source)
                    };
                    if result.is_ok() {
                        has_source = true;
                        let mut state = shared.lock();
                        if !state.queue.stopped && state.requested == request {
                            state.accepted_generation = player.frame_generation();
                        }
                        facts_at = Instant::now()
                            .checked_sub(Duration::from_secs(1))
                            .unwrap_or_else(Instant::now);
                    }
                    result
                }
                Work::Command(args) => {
                    if command_changes_position(&args) {
                        seek_fence = None;
                    }
                    if args.first().is_some_and(|name| name == "stop") {
                        has_source = false;
                        file_loaded = false;
                        loaded_snapshot = None;
                        final_snapshot = None;
                        let mut state = shared.lock();
                        if active_request == state.requested {
                            state.reset_media();
                        }
                    }
                    player
                        .command_async(&args.iter().map(String::as_str).collect::<Vec<_>>())
                        .map(|id| {
                            operation = Some(ActiveOperation {
                                id,
                                request,
                                optional_property: None,
                            });
                        })
                }
                Work::Property(name, value, optional) => {
                    if position_property(&name) {
                        seek_fence = None;
                    }
                    let optional_property = optional.then_some(name.as_str());
                    match player.set_property_async(&name, &value) {
                        Ok(id) => {
                            operation = Some(ActiveOperation {
                                id,
                                request,
                                optional_property: optional_property.map(str::to_owned),
                            })
                        }
                        Err(error) => operation_failed(
                            &shared,
                            request,
                            optional_property,
                            format!("{error:#}"),
                        ),
                    }
                    Ok(())
                }
                Work::Seek(seconds, exact) => {
                    // Replace the previous fence even for a keyframe seek. A
                    // new load/stop and native failures also retire it.
                    seek_fence = None;
                    let observed = player.cached_snapshot();
                    let fence = exact
                        && has_source
                        && file_loaded
                        && observed.duration > 0.
                        && player.diagnostic_property("seekable").as_deref() == Some("yes");
                    // Exact seeking lands on the next decoded frame, which
                    // can be >50ms away on low-frame-rate sources. Permit one
                    // native estimated frame, with a 1s upper bound.
                    let frame_tolerance = if fence {
                        player
                            .diagnostic_property("estimated-vf-fps")
                            .and_then(|fps| fps.parse::<f64>().ok())
                            .filter(|fps| fps.is_finite() && *fps > 0.)
                            .map_or(SEEK_POSITION_TOLERANCE, |fps| {
                                (1. / fps).clamp(SEEK_POSITION_TOLERANCE, 1.)
                            })
                    } else {
                        SEEK_POSITION_TOLERANCE
                    };
                    let playing =
                        if fence && player.diagnostic_property("pause").as_deref() == Some("no") {
                            player.refresh_position().zip(native_speed(&player)).map(
                                |(position, speed)| PlayingSeek {
                                    position,
                                    max_speed: speed,
                                },
                            )
                        } else {
                            None
                        };
                    let restarts = player.playback_restarts();
                    let started = Instant::now();
                    player.seek_async(seconds, exact).map(|id| {
                        operation = Some(ActiveOperation {
                            id,
                            request,
                            optional_property: None,
                        });
                        if fence {
                            let target = seconds.min(observed.duration);
                            seek_fence = Some(SeekFence {
                                id,
                                restarts,
                                target,
                                no_op: observed.position >= target - SEEK_POSITION_TOLERANCE
                                    && observed.position <= target + frame_tolerance,
                                frame_tolerance,
                                playing,
                                started,
                            });
                        }
                    })
                }
                Work::Stop(reply) => {
                    seek_fence = None;
                    active_request = request;
                    has_source = false;
                    file_loaded = false;
                    loaded_snapshot = None;
                    final_snapshot = None;
                    player.set_render_target(0, 0);
                    match player.command(&["stop"]) {
                        Ok(()) => {
                            // Poll after the synchronous stop before deciding,
                            // including when the load has not emitted START_FILE.
                            stopping = Some(PendingStop {
                                reply,
                                started: Instant::now(),
                            })
                        }
                        Err(error) => {
                            let _ = reply.send(Err(format!("{error:#}")));
                        }
                    }
                    Ok(())
                }
            };
            if let Err(error) = result {
                shared
                    .lock()
                    .event(Some(request), PlayerEvent::Error(format!("{error:#}")));
            }
        }
        let (request, revision, target, publish) = {
            let state = shared.lock();
            let current = has_source && active_request == state.requested && !state.queue.stopped;
            (
                state.requested,
                state.revision,
                if current { state.queue.target } else { (0, 0) },
                !state.queue.stopped
                    && active_request == state.requested
                    && (!has_source || file_loaded)
                    && state.queue.work.is_empty()
                    && !player.operations_pending()
                    && stopping.is_none(),
            )
        };
        player.set_render_target(target.0, target.1);
        if publish {
            if seek_fence
                .as_ref()
                .is_some_and(|fence| !fence.pending(&player))
            {
                seek_fence = None;
            }
            let mut snapshot = player.cached_snapshot();
            if let Some(final_snapshot) = &final_snapshot {
                snapshot = PlaybackSnapshot {
                    volume: snapshot.volume,
                    muted: snapshot.muted,
                    speed: snapshot.speed,
                    ..final_snapshot.clone()
                };
            } else if !has_source {
                snapshot = PlaybackSnapshot {
                    volume: snapshot.volume,
                    muted: snapshot.muted,
                    speed: snapshot.speed,
                    ..PlaybackSnapshot::default()
                };
            } else if let Some(fence) = &seek_fence {
                // Keep accepting other property observations while time-pos
                // still describes the state before the accepted exact seek.
                snapshot.position = fence.target;
            }
            shared.lock().publish(request, revision, snapshot);
        }
        if facts_at.elapsed() >= Duration::from_secs(1) && has_source && active_request == request {
            let facts = Facts {
                hardware_decoder: player.hardware_decoder(),
                dropped_frames: player.dropped_frames(),
            };
            let mut state = shared.lock();
            if !state.queue.stopped && active_request == state.requested {
                state.facts = facts;
            }
            facts_at = Instant::now();
        }
        let state = shared.lock();
        let pending = player.operations_pending() || stopping.is_some();
        let delay = if pending {
            Duration::from_millis(2)
        } else if has_source && !state.snapshot.paused {
            Duration::from_millis(10)
        } else {
            Duration::from_millis(100)
        };
        if !state.queue.stopped
            && state.revision == revision
            && (state.queue.work.is_empty() || pending)
        {
            let _ = shared
                .wake
                .wait_timeout(state, delay)
                .map_err(|_| anyhow!("Playback queue poisoned"))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(work: Work) -> Job {
        Job { request: 1, work }
    }

    fn trajectory_fence(position: f64, speed: f64) -> SeekFence {
        SeekFence {
            id: 1,
            restarts: 0,
            target: 8.,
            no_op: false,
            frame_tolerance: SEEK_POSITION_TOLERANCE,
            playing: Some(PlayingSeek {
                position,
                max_speed: speed,
            }),
            started: Instant::now(),
        }
    }

    #[test]
    fn playing_seek_trajectory_bounds_progress_at_native_speed() {
        let elapsed = Duration::from_millis(250);
        let fence = trajectory_fence(1., 1.);
        assert!(fence.position_agrees(8.25, elapsed, true));
        assert!(!fence.position_agrees(8.31, elapsed, true));
        assert!(!fence.position_agrees(7.94, elapsed, true));
        assert!(!fence.position_agrees(8.25, elapsed, false));
        let mut fast = trajectory_fence(1., 4.);
        assert!(fast.position_agrees(9., elapsed, true));
        assert!(!fast.position_agrees(9.06, elapsed, true));
        fast.playing = None;
        assert!(!fast.position_agrees(9., elapsed, true));
        assert!(fast.position_agrees(8.02, elapsed, false));
    }

    #[test]
    fn playing_seek_rejects_overlapping_preseek_trajectory_in_both_directions() {
        let elapsed = Duration::from_millis(500);
        let backward = trajectory_fence(8.4, 1.);
        assert!(!backward.position_agrees(8.4, elapsed, true));
        assert!(!backward.position_agrees(8.5, elapsed, true));
        assert!(backward.position_agrees(8.25, elapsed, true));
        let forward = trajectory_fence(7.8, 1.);
        assert!(!forward.position_agrees(8.25, elapsed, true));
        assert!(forward.position_agrees(8.4, elapsed, true));
    }

    #[test]
    fn command_queue_coalesces_scrubbing_but_preserves_barriers() {
        let mut queue = Queue::default();
        for n in 0..10_000 {
            queue.push(job(Work::Seek(n as f64, false))).unwrap();
        }
        assert_eq!(queue.work.len(), 1);
        queue
            .push(job(Work::Property("pause".into(), "yes".into(), false)))
            .unwrap();
        queue.push(job(Work::Seek(9., true))).unwrap();
        assert_eq!(queue.work.len(), 3);
        assert!(matches!(
            queue.work.back().map(|job| &job.work),
            Some(Work::Seek(9., true))
        ));
    }

    #[test]
    fn command_queue_is_bounded_and_reserves_one_stop_slot() {
        let mut queue = Queue::default();
        for _ in 0..COMMAND_LIMIT {
            queue
                .push(job(Work::Command(vec!["frame-step".into()])))
                .unwrap();
        }
        assert!(
            queue
                .push(job(Work::Command(vec!["frame-step".into()])))
                .is_err()
        );
        let (sender, mut receiver) = oneshot::channel();
        queue.push(job(Work::Stop(sender))).unwrap();
        let (sender, _) = oneshot::channel();
        assert!(queue.push(job(Work::Stop(sender))).is_err());
        assert_eq!(queue.work.len(), COMMAND_LIMIT + 1);
        queue.close("test shutdown");
        assert_eq!(receiver.try_recv().unwrap(), Err("test shutdown".into()));
        assert!(queue.push(job(Work::Seek(1., true))).is_err());
    }

    #[test]
    fn latest_adjacent_load_wins_without_crossing_controls_or_stop() {
        let mut state = State::default();
        for n in 0..10_000 {
            state.transition(Work::Load(n.to_string())).unwrap();
        }
        assert_eq!(state.queue.work.len(), 1);
        assert_eq!(state.requested, 10_000);
        state
            .send(Work::Property("pause".into(), "yes".into(), false))
            .unwrap();
        state.transition(Work::Load("next".into())).unwrap();
        let (sender, mut receiver) = oneshot::channel();
        state.transition(Work::Stop(sender)).unwrap();
        state.transition(Work::Load("last".into())).unwrap();
        assert_eq!(state.queue.work.len(), 5);
        assert_eq!(
            receiver.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        );
        assert!(matches!(&state.queue.work[0].work, Work::Load(source) if source == "9999"));
        assert!(matches!(&state.queue.work[1].work, Work::Property(..)));
        assert!(matches!(&state.queue.work[2].work, Work::Load(source) if source == "next"));
        assert!(matches!(&state.queue.work[3].work, Work::Stop(_)));
        assert!(matches!(&state.queue.work[4].work, Work::Load(source) if source == "last"));
    }

    #[test]
    fn replacement_rejects_stale_cache_events_and_native_publications() {
        let mut state = State::default();
        state.transition(Work::Load("old".into())).unwrap();
        state.queue.work.clear();
        let old_request = state.requested;
        let old_revision = state.revision;
        state.accepted_generation = 7;
        state.snapshot.duration = 12.;
        state.snapshot.tracks.push(Default::default());
        state.facts.hardware_decoder = Some("old".into());
        state.event(Some(old_request), PlayerEvent::FileLoaded);
        state.transition(Work::Load("latest".into())).unwrap();
        assert_eq!(state.accepted_generation, NO_GENERATION);
        assert_eq!(state.snapshot.duration, 0.);
        assert!(state.snapshot.tracks.is_empty());
        assert!(state.facts.hardware_decoder.is_none());
        state.event(Some(old_request), PlayerEvent::Error("stale".into()));
        state.event(Some(old_request), PlayerEvent::EndFile);
        state.publish(
            old_request,
            old_revision,
            PlaybackSnapshot {
                duration: 999.,
                ..Default::default()
            },
        );
        assert!(state.events.is_empty());
        assert_eq!(state.snapshot.duration, 0.);
        state.queue.work.clear();
        let revision = state.revision;
        state.send(Work::Seek(5., true)).unwrap();
        state.queue.work.clear();
        state.publish(state.requested, revision, PlaybackSnapshot::default());
        assert_eq!(state.snapshot.position, 5.);
    }

    #[test]
    fn stop_replies_are_ordered_bounded_and_explicit_on_overload_and_drop() {
        let proxy = PlayerProxy {
            shared: Arc::new(Shared::default()),
        };
        proxy.load("one").unwrap();
        proxy.set_property("volume", "37").unwrap();
        let mut first = proxy.stop_for_delete();
        proxy.load("two").unwrap();
        let mut second = proxy.stop_for_delete();
        assert_eq!(proxy.shared.lock().queue.work.len(), 5);
        assert_eq!(first.try_recv(), Err(oneshot::error::TryRecvError::Empty));
        assert_eq!(second.try_recv(), Err(oneshot::error::TryRecvError::Empty));
        assert_eq!(proxy.snapshot().volume, 37.);
        let mut replies: Vec<_> = (0..COMMAND_LIMIT)
            .map(|_| proxy.stop_for_delete())
            .collect();
        assert_eq!(proxy.shared.lock().queue.work.len(), COMMAND_LIMIT + 1);
        assert!(replies.last_mut().unwrap().try_recv().unwrap().is_err());
        drop(proxy);
        assert!(first.try_recv().unwrap().is_err());
        assert!(second.try_recv().unwrap().is_err());
        for reply in &mut replies[..COMMAND_LIMIT - 4] {
            assert!(reply.try_recv().unwrap().is_err());
        }
    }

    #[test]
    fn absolute_seek_defaults_exact_and_preserves_other_native_units() {
        let proxy = PlayerProxy {
            shared: Arc::new(Shared::default()),
        };
        proxy.command(&["seek", "3", "absolute"]).unwrap();
        assert!(matches!(
            &proxy.shared.lock().queue.work[0].work,
            Work::Seek(3., true)
        ));
        proxy.command(&["seek", "4", "absolute+keyframes"]).unwrap();
        assert!(matches!(
            &proxy.shared.lock().queue.work[0].work,
            Work::Seek(4., false)
        ));
        proxy.command(&["seek", "50", "absolute-percent"]).unwrap();
        assert!(
            matches!(&proxy.shared.lock().queue.work[1].work, Work::Command(args) if args[2] == "absolute-percent")
        );
        proxy.command(&["seek", "-1", "absolute"]).unwrap();
        assert!(
            matches!(&proxy.shared.lock().queue.work[2].work, Work::Command(args) if args[1] == "-1")
        );
        assert!(proxy.seek(f64::NAN, true).is_err());
        assert!(proxy.set_property("", "yes").is_err());
    }

    #[test]
    fn event_backlog_is_bounded_and_worker_failure_closes_commands_and_replies() {
        let shared = Arc::new(Shared::default());
        let proxy = PlayerProxy {
            shared: shared.clone(),
        };
        for n in 0..10_000 {
            shared
                .lock()
                .event(Some(0), PlayerEvent::Error(n.to_string()));
        }
        assert_eq!(shared.lock().events.len(), COMMAND_LIMIT);
        let mut stopped = proxy.stop_for_delete();
        shared.finish(Some("startup failed".into()));
        assert_eq!(stopped.try_recv().unwrap(), Err("startup failed".into()));
        assert!(proxy.load("x").is_err());
        assert!(proxy.seek(1., true).is_err());
        assert!(
            matches!(proxy.poll_events().last(), Some(PlayerEvent::Error(error)) if error == "startup failed")
        );
    }

    #[test]
    fn optional_property_failures_are_quiet_without_suppressing_required_controls() {
        let proxy = PlayerProxy {
            shared: Arc::new(Shared::default()),
        };
        proxy
            .set_optional_property("sub-border-style", "3")
            .unwrap();
        proxy.set_property("sub-border-style", "3").unwrap();
        let state = proxy.shared.lock();
        assert_eq!(state.queue.work.len(), 2);
        assert!(matches!(
            &state.queue.work[0].work,
            Work::Property(_, _, true)
        ));
        assert!(matches!(
            &state.queue.work[1].work,
            Work::Property(_, _, false)
        ));
        drop(state);
        operation_failed(
            &proxy.shared,
            0,
            Some("sub-border-style"),
            "property not found".into(),
        );
        assert!(proxy.poll_events().is_empty());
        operation_failed(&proxy.shared, 0, None, "required control failed".into());
        assert!(
            matches!(proxy.poll_events().as_slice(), [PlayerEvent::Error(error)] if error == "required control failed")
        );
    }
}
