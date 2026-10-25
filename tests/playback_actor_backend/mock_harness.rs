//! Deterministic owner-thread/blocked-native checks of the actual actor source.
#[path = "../../src/playback.rs"]
pub mod native_types;
#[path = "../../src/playback_actor.rs"]
pub mod playback_actor;
pub mod services {
    pub const STREAM_PROTOCOL: &str = "flicktorrent";
}

pub mod playback {
    pub use super::native_types::{
        OperationId, OperationResult, PlaybackSnapshot, PlayerEvent, StreamOpener,
    };
    use anyhow::{Result, bail};
    use std::{
        cell::{Cell, RefCell},
        marker::PhantomData,
        rc::Rc,
        sync::{Arc, Condvar, Mutex, OnceLock},
        thread::{self, ThreadId},
        time::{Duration, Instant},
    };

    #[derive(Default)]
    pub struct Gates {
        pub startup: bool,
        pub startup_error: bool,
        pub completions: bool,
        pub seekable: bool,
        pub seek_updates: bool,
        pub position_notifications: bool,
        pub seek_error: bool,
        pub silent_seek_failure: bool,
        pub restart: bool,
        pub seek_restart_delay: Duration,
        pub hold_restarts: bool,
        pub advancing: bool,
        pub native_position: f64,
        pub native_paused: bool,
        pub frame_rate: Option<f64>,
        pub early_stop_idle: bool,
        pub position: Option<f64>,
        pub natural_end: bool,
        pub loads: usize,
        pub releases: usize,
        pub destruction: bool,
        pub owner: Option<ThreadId>,
        pub destroyed: bool,
        pub records: Vec<String>,
    }
    #[derive(Default)]
    pub struct Control {
        pub gates: Mutex<Gates>,
        pub wake: Condvar,
    }
    impl Control {
        pub fn change(&self, f: impl FnOnce(&mut Gates)) {
            f(&mut self.gates.lock().unwrap());
            self.wake.notify_all();
        }
        fn record(&self, record: String) {
            self.change(|gates| gates.records.push(record));
        }
    }
    static NEXT: OnceLock<Mutex<Option<Arc<Control>>>> = OnceLock::new();
    pub fn configure(control: Arc<Control>) {
        *NEXT.get_or_init(Default::default).lock().unwrap() = Some(control);
    }
    #[derive(Debug)]
    pub struct RenderedFrames;
    enum Change {
        Property(String, String),
        Seek(f64),
        Command(String),
    }
    pub struct Player {
        control: Arc<Control>,
        snapshot: RefCell<PlaybackSnapshot>,
        generation: Cell<u64>,
        events: RefCell<Vec<PlayerEvent>>,
        operation: RefCell<Option<(u64, Change)>>,
        next: Cell<u64>,
        stopping: Cell<bool>,
        idle: Cell<bool>,
        playlist_empty: Cell<bool>,
        native_position: Cell<f64>,
        restarts: Cell<u64>,
        started_files: Cell<u64>,
        ended_files: Cell<u64>,
        seek_update: Cell<Option<f64>>,
        restart_at: Cell<Option<Instant>>,
        position_at: Cell<Instant>,
        _owner: PhantomData<Rc<()>>,
    }
    impl Player {
        pub fn new() -> Result<Self> {
            let control = NEXT
                .get()
                .unwrap()
                .lock()
                .unwrap()
                .take()
                .expect("configured fake native player");
            let mut gates = control.gates.lock().unwrap();
            gates.owner = Some(thread::current().id());
            while !gates.startup {
                gates = control.wake.wait(gates).unwrap();
            }
            let error = gates.startup_error;
            drop(gates);
            if error {
                bail!("injected startup error");
            }
            Ok(Self {
                control,
                snapshot: Default::default(),
                generation: Cell::new(0),
                events: Default::default(),
                operation: Default::default(),
                next: Cell::new(0),
                stopping: Cell::new(false),
                idle: Cell::new(true),
                playlist_empty: Cell::new(true),
                native_position: Cell::new(0.),
                restarts: Cell::new(0),
                started_files: Cell::new(0),
                ended_files: Cell::new(0),
                seek_update: Cell::new(None),
                restart_at: Cell::new(None),
                position_at: Cell::new(Instant::now()),
                _owner: PhantomData,
            })
        }
        pub fn register_protocol(&self, _: &str, _: Arc<StreamOpener>) -> Result<()> {
            Ok(())
        }
        pub fn render_in_background(&mut self) -> Result<RenderedFrames> {
            Ok(RenderedFrames)
        }
        pub fn frame_generation(&self) -> u64 {
            self.generation.get()
        }
        pub fn set_render_target(&self, _: u32, _: u32) {}
        pub fn load(&self, source: &str) -> Result<()> {
            self.control.record(format!("load:{source}"));
            let mut gates = self.control.gates.lock().unwrap();
            while gates.loads == 0 {
                gates = self.control.wake.wait(gates).unwrap();
            }
            gates.loads -= 1;
            drop(gates);
            self.generation.set(self.generation.get() + 1);
            self.idle.set(false);
            self.playlist_empty.set(false);
            self.stopping.set(false);
            self.started_files.set(0);
            self.ended_files.set(0);
            if source == "queued" {
                // Accepted loadfile with a playlist entry, before START_FILE.
                return Ok(());
            }
            self.started_files.set(1);
            if source == "broken" {
                // Native load errors end the file but retain its playlist entry.
                self.idle.set(true);
                *self.snapshot.borrow_mut() = PlaybackSnapshot::default();
                self.events.borrow_mut().extend([
                    PlayerEvent::Error("injected load error".into()),
                    PlayerEvent::EndFile,
                ]);
                return Ok(());
            }
            let mut snapshot = self.snapshot.borrow_mut();
            snapshot.duration = match source {
                "old" => 1.,
                "seek" => 12.,
                "live" => 0.,
                _ => 2.,
            };
            snapshot.position = 0.;
            self.native_position.set(0.);
            self.seek_update.set(None);
            self.restart_at.set(None);
            self.position_at.set(Instant::now());
            snapshot.paused = false;
            self.events.borrow_mut().push(PlayerEvent::FileLoaded);
            Ok(())
        }
        fn enqueue(&self, change: Change, description: String) -> Result<u64> {
            assert!(
                self.operation.borrow().is_none(),
                "actor crossed a native operation barrier"
            );
            self.control.record(description);
            let id = self.next.get() + 1;
            self.next.set(id);
            *self.operation.borrow_mut() = Some((id, change));
            Ok(id)
        }
        pub fn command_async(&self, args: &[&str]) -> Result<u64> {
            self.enqueue(
                Change::Command(args.join(" ")),
                format!("command:{}", args.join(" ")),
            )
        }
        pub fn set_property_async(&self, name: &str, value: &str) -> Result<u64> {
            self.enqueue(
                Change::Property(name.into(), value.into()),
                format!("property:{name}:{value}"),
            )
        }
        pub fn seek_async(&self, seconds: f64, exact: bool) -> Result<u64> {
            self.enqueue(Change::Seek(seconds), format!("seek:{seconds}:{exact}"))
        }
        pub fn operations_pending(&self) -> bool {
            self.operation.borrow().is_some()
        }
        pub fn command(&self, args: &[&str]) -> Result<()> {
            assert_eq!(args, ["stop"]);
            assert!(!self.operations_pending());
            self.control.record("stop".into());
            self.playlist_empty.set(true);
            if self.files_ended() {
                self.idle.set(true);
                return Ok(());
            }
            self.stopping.set(true);
            if self.control.gates.lock().unwrap().early_stop_idle {
                self.idle.set(true);
            }
            Ok(())
        }
        pub fn poll_events(&self) -> Vec<PlayerEvent> {
            if self.stopping.get() {
                let mut gates = self.control.gates.lock().unwrap();
                if gates.releases > 0 {
                    gates.releases -= 1;
                    self.stopping.set(false);
                    self.idle.set(true);
                    self.events.borrow_mut().push(PlayerEvent::EndFile);
                }
            }
            // Advance independently of event delivery, including while the
            // actor has not yet observed the delayed restart.
            let now = Instant::now();
            let elapsed = now.duration_since(self.position_at.replace(now));
            if self.control.gates.lock().unwrap().advancing {
                let snapshot = self.snapshot.borrow();
                if !snapshot.paused {
                    self.native_position
                        .set(self.native_position.get() + elapsed.as_secs_f64() * snapshot.speed);
                }
            }
            if self.restart_at.get().is_some_and(|at| now >= at)
                && !self.control.gates.lock().unwrap().hold_restarts
            {
                self.restart_at.set(None);
                self.restarts.set(self.restarts.get() + 1);
                self.control.record("delayed-restart-observed".into());
            }
            let (can_complete, natural_end) = {
                let mut gates = self.control.gates.lock().unwrap();
                let end = gates.natural_end;
                gates.natural_end = false;
                (gates.completions, end)
            };
            if natural_end {
                self.events.borrow_mut().push(PlayerEvent::EndFile);
                self.snapshot.borrow_mut().duration = 0.;
                self.snapshot.borrow_mut().position = 0.;
                self.idle.set(true);
            }
            if can_complete && let Some((id, change)) = self.operation.borrow_mut().take() {
                let mut snapshot = self.snapshot.borrow_mut();
                let mut result = OperationResult::Completed;
                match change {
                    Change::Property(name, value) if name == "pause" => {
                        snapshot.paused = value == "yes"
                    }
                    Change::Property(name, value) if name == "volume" => {
                        snapshot.volume = value.parse().unwrap()
                    }
                    Change::Property(name, value) if name == "speed" => {
                        snapshot.speed = value.parse().unwrap()
                    }
                    Change::Property(name, _) if name == "unsupported" => {
                        result = OperationResult::Failed("property not found".into())
                    }
                    Change::Property(_, _) => {}
                    Change::Seek(seconds) => {
                        let mut gates = self.control.gates.lock().unwrap();
                        if gates.seek_error {
                            gates.seek_error = false;
                            result = OperationResult::Failed("injected seek error".into());
                        } else if !gates.silent_seek_failure {
                            self.seek_update.set(Some(seconds));
                        }
                        if gates.seekable {
                            gates.records.push(format!("seek-reply:{seconds}"));
                        }
                    }
                    Change::Command(command) if command == "frame-step" => {
                        self.native_position.set(self.native_position.get() + 0.1);
                    }
                    Change::Command(command) if command == "add chapter 1" => {
                        self.native_position.set(2.);
                    }
                    Change::Command(command) if command == "fail" => {
                        result = OperationResult::Failed("injected operation error".into())
                    }
                    Change::Command(command) if command == "cycle pause" => {
                        snapshot.paused = !snapshot.paused
                    }
                    Change::Command(_) => {}
                }
                self.events
                    .borrow_mut()
                    .push(PlayerEvent::OperationCompleted { id, result });
            }
            {
                let mut gates = self.control.gates.lock().unwrap();
                if gates.restart {
                    gates.restart = false;
                    self.restarts.set(self.restarts.get() + 1);
                }
                if let Some(position) = gates.position.take() {
                    self.native_position.set(position);
                }
                if gates.seek_updates
                    && let Some(seconds) = self.seek_update.take()
                {
                    // Model native seek completion separately from both its
                    // command reply and the delayed property notification.
                    if seconds != self.native_position.get() {
                        if gates.seek_restart_delay.is_zero() {
                            self.restarts.set(self.restarts.get() + 1);
                        } else {
                            self.restart_at
                                .set(Some(Instant::now() + gates.seek_restart_delay));
                        }
                    }
                    let position = gates
                        .frame_rate
                        .map_or(seconds, |fps| (seconds * fps).ceil() / fps);
                    self.native_position.set(position);
                    self.position_at.set(Instant::now());
                    if gates.seekable {
                        gates.records.push(format!("seek-restart:{seconds}"));
                    }
                }
                if gates.position_notifications {
                    self.snapshot.borrow_mut().position = self.native_position.get();
                }
                gates.native_position = self.native_position.get();
                gates.native_paused = self.snapshot.borrow().paused;
            }
            let events: Vec<_> = self.events.borrow_mut().drain(..).collect();
            for event in &events {
                if matches!(event, PlayerEvent::EndFile) {
                    self.ended_files.set(self.ended_files.get() + 1);
                }
            }
            events
        }
        pub fn files_ended(&self) -> bool {
            self.started_files.get() == self.ended_files.get()
        }
        pub fn playback_restarts(&self) -> u64 {
            self.restarts.get()
        }
        pub fn refresh_position(&self) -> Option<f64> {
            let position = self.native_position.get();
            self.snapshot.borrow_mut().position = position;
            self.control.record(format!("position-read:{position}"));
            Some(position)
        }
        pub fn snapshot(&self) -> PlaybackSnapshot {
            self.cached_snapshot()
        }
        pub fn cached_snapshot(&self) -> PlaybackSnapshot {
            self.snapshot.borrow().clone()
        }
        pub fn hardware_decoder(&self) -> Option<String> {
            None
        }
        pub fn dropped_frames(&self) -> Option<u64> {
            Some(0)
        }
        pub fn diagnostic_property(&self, name: &str) -> Option<String> {
            match name {
                "estimated-vf-fps" => self
                    .control
                    .gates
                    .lock()
                    .unwrap()
                    .frame_rate
                    .map(|fps| fps.to_string()),
                "pause" => Some(
                    if self.snapshot.borrow().paused {
                        "yes"
                    } else {
                        "no"
                    }
                    .into(),
                ),
                "speed" => Some(self.snapshot.borrow().speed.to_string()),
                "seekable" => Some(
                    if self.control.gates.lock().unwrap().seekable {
                        "yes"
                    } else {
                        "no"
                    }
                    .into(),
                ),
                "seeking" => Some(
                    if self.seek_update.get().is_some() {
                        "yes"
                    } else {
                        "no"
                    }
                    .into(),
                ),
                "idle-active" => Some(if self.idle.get() { "yes" } else { "no" }.into()),
                "playlist-count" => Some(if self.playlist_empty.get() { "0" } else { "1" }.into()),
                _ => None,
            }
        }
    }
    impl Drop for Player {
        fn drop(&mut self) {
            let mut gates = self.control.gates.lock().unwrap();
            while !gates.destruction {
                gates = self.control.wake.wait(gates).unwrap();
            }
            assert_eq!(gates.owner, Some(thread::current().id()));
            gates.destroyed = true;
        }
    }
}
fn main() {}

#[cfg(test)]
mod worker_checks {
    use super::{playback::*, playback_actor::*};
    use anyhow::{Result, bail, ensure};
    use std::{
        sync::Arc,
        thread,
        time::{Duration, Instant},
    };
    use tokio::sync::oneshot;
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn wait(check: impl Fn() -> bool) -> Result<()> {
        let until = Instant::now() + Duration::from_secs(5);
        while !check() {
            ensure!(Instant::now() < until, "fake worker timed out");
            thread::sleep(Duration::from_millis(2));
        }
        Ok(())
    }
    fn receive<T>(mut reply: oneshot::Receiver<T>) -> Result<T> {
        let until = Instant::now() + Duration::from_secs(5);
        loop {
            match reply.try_recv() {
                Ok(value) => return Ok(value),
                Err(oneshot::error::TryRecvError::Closed) => bail!("reply canceled"),
                Err(oneshot::error::TryRecvError::Empty) => {
                    ensure!(Instant::now() < until, "reply timed out")
                }
            };
            thread::sleep(Duration::from_millis(2));
        }
    }
    fn configured() -> Arc<Control> {
        let control = Arc::new(Control::default());
        control.change(|g| {
            g.startup = true;
            g.completions = true;
            g.seek_updates = true;
            g.position_notifications = true;
            g.loads = 100;
            g.destruction = true;
        });
        configure(control.clone());
        control
    }
    fn start() -> Result<(PlayerProxy, FramesReady)> {
        PlayerProxy::start(Arc::new(|_| Err(std::io::Error::other("fake backend"))))
    }
    fn has(control: &Control, record: &str) -> bool {
        control
            .gates
            .lock()
            .unwrap()
            .records
            .iter()
            .any(|r| r == record)
    }

    fn remains_at(proxy: &PlayerProxy, position: f64) -> Result<()> {
        let until = Instant::now() + Duration::from_millis(250);
        while Instant::now() < until {
            assert_eq!(proxy.snapshot().position, position);
            thread::sleep(Duration::from_millis(2));
        }
        Ok(())
    }

    #[test]
    fn exact_seek_fences_command_reply_and_delayed_properties_but_accepts_other_updates()
    -> Result<()> {
        let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
        let control = configured();
        control.change(|g| {
            g.seekable = true;
            g.seek_updates = false;
            g.position_notifications = false;
        });
        let (proxy, frames) = start()?;
        assert!(receive(frames)?.is_ok());
        proxy.load("seek")?;
        wait(|| proxy.snapshot().duration == 12.)?;
        proxy.set_property("pause", "yes")?;
        proxy.seek(8.25, true)?;
        wait(|| has(&control, "seek-reply:8.25"))?;
        // This barrier and the sampling interval occur AFTER COMMAND_REPLY.
        proxy.set_property("volume", "37")?;
        wait(|| proxy.snapshot().volume == 37.)?;
        assert!(proxy.snapshot().paused);
        remains_at(&proxy, 8.25)?;
        // An untagged restart from an earlier scrub is not confirmation.
        control.change(|g| g.restart = true);
        remains_at(&proxy, 8.25)?;
        control.change(|g| g.seek_updates = true);
        wait(|| has(&control, "position-read:8.25"))?;
        remains_at(&proxy, 8.25)?;
        assert_eq!(proxy.snapshot().volume, 37.);

        // A no-op exact seek may have no new restart or property event.
        proxy.seek(8.25, true)?;
        proxy.set_property("volume", "38")?;
        wait(|| proxy.snapshot().volume == 38.)?;
        remains_at(&proxy, 8.25)?;
        control.change(|g| {
            g.position_notifications = true;
            g.position = Some(8.5);
        });
        // A subsequent native position must publish, proving the no-op fence
        // actually retired rather than merely keeping the same target pinned.
        wait(|| proxy.snapshot().position == 8.5)?;

        control.change(|g| g.seek_updates = false);
        proxy.seek(6., true)?;
        wait(|| has(&control, "seek-reply:6"))?;
        for n in 0..10_000 {
            proxy.seek((n % 1000) as f64 / 100., false)?;
        }
        proxy.seek(7.25, true)?;
        wait(|| has(&control, "seek-reply:7.25"))?;
        control.change(|g| g.restart = true);
        remains_at(&proxy, 7.25)?;
        control.change(|g| g.seek_updates = true);
        wait(|| has(&control, "position-read:7.25"))?;
        remains_at(&proxy, 7.25)?;
        Ok(())
    }

    #[test]
    fn playing_exact_seek_releases_after_delayed_restart_with_advancing_position() -> Result<()> {
        let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
        let control = configured();
        control.change(|g| {
            g.seekable = true;
            g.advancing = true;
            g.seek_restart_delay = Duration::from_millis(250);
        });
        let (proxy, frames) = start()?;
        assert!(receive(frames)?.is_ok());
        proxy.load("seek")?;
        wait(|| proxy.snapshot().duration == 12.)?;
        for (trial, speed) in [1., 4.].into_iter().enumerate() {
            proxy.set_property("speed", &speed.to_string())?;
            proxy.set_property("volume", &(40 + trial).to_string())?;
            wait(|| proxy.snapshot().volume == (40 + trial) as f64)?;
            control.change(|g| g.records.clear());
            let target = if trial == 0 { 3. } else { 8. };
            let started = Instant::now();
            proxy.seek(target, true)?;
            wait(|| has(&control, &format!("seek-reply:{target}")))?;
            // At the first restart observation the native clock is already
            // >250ms * speed beyond target. It never agrees within 50ms.
            wait(|| has(&control, "delayed-restart-observed"))?;
            ensure!(started.elapsed() >= Duration::from_millis(250));
            let deadline = Instant::now() + Duration::from_millis(300);
            while proxy.snapshot().position <= target + 0.1 * speed {
                ensure!(
                    Instant::now() < deadline,
                    "playing seek pinned after delayed restart: speed={speed}, {:?}",
                    proxy.snapshot()
                );
                thread::sleep(Duration::from_millis(2));
            }
            let first = proxy.snapshot().position;
            thread::sleep(Duration::from_millis(100));
            ensure!(
                proxy.snapshot().position > first + 0.05 * speed,
                "native advancement stopped after fence retirement"
            );
        }
        Ok(())
    }

    #[test]
    fn playing_seek_speed_change_before_delayed_restart_keeps_advancing() -> Result<()> {
        let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
        let control = configured();
        control.change(|g| {
            g.seekable = true;
            g.advancing = true;
            g.seek_restart_delay = Duration::from_millis(250);
        });
        let (proxy, frames) = start()?;
        assert!(receive(frames)?.is_ok());
        proxy.load("seek")?;
        wait(|| proxy.snapshot().duration == 12.)?;
        proxy.seek(3., true)?;
        wait(|| has(&control, "seek-reply:3"))?;
        proxy.set_property("speed", "4")?;
        wait(|| proxy.snapshot().speed == 4. && has(&control, "property:speed:4"))?;
        wait(|| has(&control, "delayed-restart-observed"))?;
        let deadline = Instant::now() + Duration::from_millis(300);
        while proxy.snapshot().position <= 3.5 {
            ensure!(
                Instant::now() < deadline,
                "1x-to-4x change left seek pinned: {:?}",
                proxy.snapshot()
            );
            thread::sleep(Duration::from_millis(2));
        }
        let position = proxy.snapshot().position;
        thread::sleep(Duration::from_millis(100));
        ensure!(
            proxy.snapshot().position > position + 0.2,
            "4x playback stopped publishing after speed-change settlement"
        );
        Ok(())
    }

    #[test]
    fn playing_seek_then_pause_preserves_native_progress_before_delayed_restart() -> Result<()> {
        let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
        let control = configured();
        control.change(|g| {
            g.seekable = true;
            g.advancing = true;
            g.seek_restart_delay = Duration::from_millis(250);
            g.hold_restarts = true;
        });
        let (proxy, frames) = start()?;
        assert!(receive(frames)?.is_ok());
        proxy.load("seek")?;
        wait(|| proxy.snapshot().duration == 12.)?;
        proxy.seek(3., true)?;
        wait(|| has(&control, "seek-reply:3"))?;
        wait(|| control.gates.lock().unwrap().native_position > 3.15)?;
        proxy.set_property("pause", "yes")?;
        wait(|| control.gates.lock().unwrap().native_paused)?;
        let native_position = control.gates.lock().unwrap().native_position;
        ensure!(native_position > 3.15);
        ensure!(!has(&control, "delayed-restart-observed"));
        // A native pause reply alone cannot retire the seek. The stale cache
        // stays protected until the held restart is actually observed.
        remains_at(&proxy, 3.)?;
        control.change(|g| g.hold_restarts = false);
        wait(|| has(&control, "delayed-restart-observed"))?;
        let deadline = Instant::now() + Duration::from_millis(300);
        while (proxy.snapshot().position - native_position).abs() > 0.001 {
            ensure!(
                Instant::now() < deadline,
                "pause after native advance left seek pinned: {:?}",
                proxy.snapshot()
            );
            thread::sleep(Duration::from_millis(2));
        }
        ensure!(proxy.snapshot().paused);
        remains_at(&proxy, native_position)?;
        Ok(())
    }

    #[test]
    fn playing_earlier_restart_cannot_release_a_reachable_preseek_position() -> Result<()> {
        let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
        let control = configured();
        control.change(|g| g.seekable = true);
        let (proxy, frames) = start()?;
        assert!(receive(frames)?.is_ok());
        proxy.load("seek")?;
        wait(|| proxy.snapshot().duration == 12.)?;
        control.change(|g| g.position = Some(8.4));
        wait(|| proxy.snapshot().position == 8.4)?;
        control.change(|g| g.silent_seek_failure = true);
        proxy.seek(8., true)?;
        wait(|| has(&control, "seek-reply:8"))?;
        thread::sleep(Duration::from_millis(250));
        // The old 8.4 position is within the new target's elapsed trajectory
        // after another 250ms, but it also belongs to the pre-seek trajectory.
        control.change(|g| g.restart = true);
        remains_at(&proxy, 8.)?;
        remains_at(&proxy, 8.)?;
        Ok(())
    }

    #[test]
    fn low_fps_paused_seek_settles_on_next_frame_and_later_commands_publish() -> Result<()> {
        let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
        let control = configured();
        control.change(|g| {
            g.seekable = true;
            g.frame_rate = Some(10.);
        });
        let (proxy, frames) = start()?;
        assert!(receive(frames)?.is_ok());
        proxy.load("seek")?;
        wait(|| proxy.snapshot().duration == 12.)?;
        proxy.set_property("pause", "yes")?;
        proxy.seek(3.013, true)?;
        wait(|| (proxy.snapshot().position - 3.1).abs() < 0.001)?;
        ensure!(proxy.snapshot().paused);
        control.change(|g| g.silent_seek_failure = true);
        proxy.seek(8.013, true)?;
        wait(|| has(&control, "seek-reply:8.013"))?;
        remains_at(&proxy, 8.013)?;
        proxy.command(&["frame-step"])?;
        wait(|| (proxy.snapshot().position - 3.2).abs() < 0.001)?;
        proxy.seek(9.013, true)?;
        wait(|| has(&control, "seek-reply:9.013"))?;
        remains_at(&proxy, 9.013)?;
        proxy.command(&["add", "chapter", "1"])?;
        wait(|| proxy.snapshot().position == 2.)?;
        Ok(())
    }

    #[test]
    fn stop_early_idle_waits_for_end_file_but_already_idle_stops_acknowledge() -> Result<()> {
        let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
        let control = configured();
        control.change(|g| g.early_stop_idle = true);
        let (proxy, frames) = start()?;
        assert!(receive(frames)?.is_ok());
        assert!(receive(proxy.stop_for_delete())?.is_ok());
        proxy.load("seek")?;
        wait(|| proxy.snapshot().duration == 12.)?;
        let mut reply = proxy.stop_for_delete();
        wait(|| {
            control
                .gates
                .lock()
                .unwrap()
                .records
                .iter()
                .filter(|r| *r == "stop")
                .count()
                == 2
        })?;
        thread::sleep(Duration::from_millis(150));
        ensure!(
            matches!(reply.try_recv(), Err(oneshot::error::TryRecvError::Empty)),
            "early native idle acknowledged before END_FILE"
        );
        control.change(|g| g.releases = 1);
        assert!(receive(reply)?.is_ok());
        assert!(receive(proxy.stop_for_delete())?.is_ok());
        Ok(())
    }

    #[test]
    fn failed_replacement_load_already_ended_acknowledges_first_stop() -> Result<()> {
        let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
        let control = configured();
        let (proxy, frames) = start()?;
        assert!(receive(frames)?.is_ok());
        proxy.load("seek")?;
        wait(|| proxy.snapshot().duration == 12.)?;
        // The replaced entry's END_FILE is not accepted for the new entry.
        // Counting its start across generations would leave an unmatched start.
        proxy.load("broken")?;
        wait(|| {
            proxy.poll_events().iter().any(
                |event| matches!(event, PlayerEvent::Error(error) if error == "injected load error"),
            )
        })?;
        assert!(receive(proxy.stop_for_delete())?.is_ok());
        assert!(has(&control, "stop"));
        assert!(receive(proxy.stop_for_delete())?.is_ok());
        Ok(())
    }

    #[test]
    fn stop_behind_blocked_load_waits_then_acknowledges_without_a_start_or_end() -> Result<()> {
        let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
        let control = configured();
        control.change(|g| g.loads = 0);
        let (proxy, frames) = start()?;
        assert!(receive(frames)?.is_ok());
        proxy.load("queued")?;
        wait(|| has(&control, "load:queued"))?;
        let mut reply = proxy.stop_for_delete();
        thread::sleep(Duration::from_millis(150));
        ensure!(
            matches!(reply.try_recv(), Err(oneshot::error::TryRecvError::Empty)),
            "stop acknowledged while load was blocked before START_FILE"
        );
        ensure!(!has(&control, "stop"), "stop crossed the blocked load");
        control.change(|g| g.loads = 1);
        assert!(receive(reply)?.is_ok());
        assert!(has(&control, "stop"));
        Ok(())
    }

    #[test]
    fn nonseekable_and_unknown_duration_sources_do_not_pin_unconfirmed_targets() -> Result<()> {
        let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
        let control = configured();
        control.change(|g| g.silent_seek_failure = true);
        let (proxy, frames) = start()?;
        assert!(receive(frames)?.is_ok());
        for source in ["seek", "live"] {
            control.change(|g| g.seekable = source == "live");
            proxy.load(source)?;
            // A property barrier also lets FILE_LOADED and cached native
            // observations settle even when the duration is unknown (zero).
            proxy.set_property("volume", "36")?;
            wait(|| proxy.snapshot().volume == 36.)?;
            proxy.seek(8.25, true)?;
            proxy.set_property("volume", "37")?;
            wait(|| {
                let snapshot = proxy.snapshot();
                snapshot.volume == 37. && snapshot.position == 0.
            })?;
            assert_eq!(proxy.snapshot().position, 0.);
        }
        Ok(())
    }

    #[test]
    fn seek_failure_replacement_and_stop_retire_fences_and_allow_recovery() -> Result<()> {
        let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
        let control = configured();
        control.change(|g| g.seekable = true);
        let (proxy, frames) = start()?;
        assert!(receive(frames)?.is_ok());
        proxy.load("seek")?;
        wait(|| proxy.snapshot().duration == 12.)?;
        proxy.seek(1., true)?;
        wait(|| has(&control, "position-read:1"))?;
        control.change(|g| g.seek_error = true);
        proxy.seek(8.25, true)?;
        let mut error = false;
        wait(|| {
            // Keep event collection outside the predicate's immutable capture.
            has(&control, "seek-reply:8.25") && proxy.snapshot().position == 1.
        })?;
        for event in proxy.poll_events() {
            error |=
                matches!(event, PlayerEvent::Error(message) if message == "injected seek error");
        }
        assert!(error, "seek failure did not reach the caller");
        proxy.seek(4.5, true)?;
        wait(|| has(&control, "position-read:4.5"))?;
        remains_at(&proxy, 4.5)?;
        control.change(|g| g.seek_updates = false);
        proxy.seek(9., true)?;
        wait(|| has(&control, "seek-reply:9"))?;
        proxy.load("latest")?;
        wait(|| proxy.snapshot().duration == 2.)?;
        assert_eq!(proxy.snapshot().position, 0.);
        proxy.load("seek")?;
        wait(|| proxy.snapshot().duration == 12.)?;
        proxy.seek(9.5, true)?;
        wait(|| has(&control, "seek-reply:9.5"))?;
        control.change(|g| g.releases = 1);
        assert!(receive(proxy.stop_for_delete())?.is_ok());
        assert_eq!(proxy.snapshot().position, 0.);
        assert_eq!(proxy.snapshot().duration, 0.);
        assert_eq!(proxy.frame_generation(), u64::MAX);
        Ok(())
    }

    #[test]
    fn silently_rejected_exact_seek_recovers_to_native_position_without_a_restart() -> Result<()> {
        let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
        let control = configured();
        control.change(|g| {
            g.seekable = true;
            g.silent_seek_failure = true;
        });
        let (proxy, frames) = start()?;
        assert!(receive(frames)?.is_ok());
        proxy.load("seek")?;
        wait(|| proxy.snapshot().duration == 12.)?;
        proxy.seek(8.25, true)?;
        wait(|| has(&control, "seek-reply:8.25"))?;
        proxy.set_property("volume", "37")?;
        wait(|| proxy.snapshot().volume == 37.)?;
        remains_at(&proxy, 8.25)?;
        let until = Instant::now() + Duration::from_secs(12);
        while proxy.snapshot().position != 0. {
            ensure!(
                Instant::now() < until,
                "silent native rejection left a permanent fence"
            );
            thread::sleep(Duration::from_millis(10));
        }
        assert!(has(&control, "position-read:0"));
        control.change(|g| g.silent_seek_failure = false);
        proxy.seek(4.5, true)?;
        wait(|| has(&control, "position-read:4.5"))?;
        remains_at(&proxy, 4.5)?;
        Ok(())
    }

    #[test]
    fn optional_native_failure_is_quiet_and_required_property_failure_is_reported() -> Result<()> {
        let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
        let control = configured();
        let (proxy, frames) = start()?;
        assert!(receive(frames)?.is_ok());
        proxy.set_optional_property("unsupported", "yes")?;
        proxy.set_property("unsupported", "yes")?;
        proxy.set_property("volume", "37")?;
        wait(|| has(&control, "property:volume:37"))?;
        let errors: Vec<_> = proxy
            .poll_events()
            .into_iter()
            .filter_map(|event| {
                if let PlayerEvent::Error(error) = event {
                    Some(error)
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(errors, ["property not found"]);
        assert_eq!(
            control.gates.lock().unwrap().records,
            [
                "property:unsupported:yes",
                "property:unsupported:yes",
                "property:volume:37"
            ]
        );
        Ok(())
    }

    #[test]
    fn natural_end_keeps_final_snapshot_after_native_unload_but_stop_clears_it() -> Result<()> {
        let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
        let control = configured();
        let (proxy, frames) = start()?;
        assert!(receive(frames)?.is_ok());
        proxy.load("latest")?;
        wait(|| proxy.snapshot().duration == 2.)?;
        control.change(|g| g.natural_end = true);
        wait(|| proxy.snapshot().eof)?;
        assert_eq!(proxy.snapshot().duration, 2.);
        assert_eq!(proxy.snapshot().position, 2.);
        assert!(proxy.snapshot().paused);
        thread::sleep(Duration::from_millis(150));
        assert!(proxy.snapshot().eof);
        assert_eq!(proxy.snapshot().duration, 2.);
        control.change(|g| g.releases = 1);
        assert!(receive(proxy.stop_for_delete())?.is_ok());
        assert!(!proxy.snapshot().eof);
        assert_eq!(proxy.snapshot().duration, 0.);
        assert_eq!(proxy.snapshot().position, 0.);
        Ok(())
    }

    #[test]
    fn startup_is_async_failure_closes_queued_stop_and_future_commands() -> Result<()> {
        let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
        let control = configured();
        control.change(|g| {
            g.startup = false;
            g.startup_error = true;
        });
        let started = Instant::now();
        let (proxy, frames) = start()?;
        assert!(started.elapsed() < Duration::from_millis(100));
        proxy.load("latest")?;
        let stopped = proxy.stop_for_delete();
        control.change(|g| g.startup = true);
        assert!(
            receive(frames)?
                .unwrap_err()
                .contains("injected startup error")
        );
        assert!(receive(stopped)?.is_err());
        assert!(proxy.load("again").is_err());
        assert!(
            proxy
                .poll_events()
                .iter()
                .any(|e| matches!(e, PlayerEvent::Error(_)))
        );
        Ok(())
    }

    #[test]
    fn blocked_load_cannot_publish_over_latest_request_or_cross_control_barriers() -> Result<()> {
        let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
        let control = configured();
        control.change(|g| g.loads = 0);
        let (proxy, frames) = start()?;
        assert!(receive(frames)?.is_ok());
        proxy.load("old")?;
        wait(|| has(&control, "load:old"))?;
        proxy.load("middle")?;
        proxy.load("latest")?;
        proxy.set_property("pause", "yes")?;
        for n in 0..10_000 {
            proxy.seek(n as f64, false)?;
        }
        proxy.seek(7.25, true)?;
        control.change(|g| g.loads = 1);
        wait(|| has(&control, "load:latest"))?;
        assert!(!has(&control, "load:middle"));
        assert_eq!(proxy.frame_generation(), u64::MAX);
        assert_eq!(proxy.snapshot().duration, 0.);
        assert!(proxy.snapshot().paused);
        assert_eq!(proxy.snapshot().position, 7.25);
        assert!(proxy.poll_events().is_empty());
        control.change(|g| g.loads = 1);
        wait(|| proxy.snapshot().duration == 2.)?;
        assert_eq!(proxy.frame_generation(), 2);
        assert!(proxy.snapshot().paused);
        assert_eq!(proxy.snapshot().position, 7.25);
        assert_eq!(
            control.gates.lock().unwrap().records,
            [
                "load:old",
                "load:latest",
                "property:pause:yes",
                "seek:7.25:true"
            ]
        );
        let events = proxy.poll_events();
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], PlayerEvent::FileLoaded));
        drop(proxy);
        wait(|| control.gates.lock().unwrap().destroyed)?;
        Ok(())
    }

    #[test]
    fn saturated_worker_drop_is_bounded_while_native_destruction_stays_on_owner() -> Result<()> {
        let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
        let control = configured();
        control.change(|g| {
            g.completions = false;
            g.destruction = false;
        });
        let (proxy, frames) = start()?;
        assert!(receive(frames)?.is_ok());
        proxy.set_property("pause", "yes")?;
        wait(|| has(&control, "property:pause:yes"))?;
        for _ in 0..64 {
            proxy.command(&["frame-step"])?;
        }
        assert!(proxy.command(&["frame-step"]).is_err());
        let stopped = proxy.stop_for_delete();
        assert!(receive(proxy.stop_for_delete())?.is_err());
        let started = Instant::now();
        drop(proxy);
        assert!(started.elapsed() < Duration::from_millis(100));
        assert!(receive(stopped)?.is_err());
        assert!(!control.gates.lock().unwrap().destroyed);
        control.change(|g| g.destruction = true);
        wait(|| control.gates.lock().unwrap().destroyed)?;
        assert_ne!(
            control.gates.lock().unwrap().owner,
            Some(thread::current().id())
        );
        Ok(())
    }

    #[test]
    fn native_completion_errors_are_filtered_by_request_and_current_failure_recovers() -> Result<()>
    {
        let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
        let control = configured();
        control.change(|g| g.completions = false);
        let (proxy, frames) = start()?;
        assert!(receive(frames)?.is_ok());
        proxy.command(&["fail"])?;
        wait(|| has(&control, "command:fail"))?;
        proxy.load("latest")?;
        control.change(|g| g.completions = true);
        wait(|| proxy.snapshot().duration == 2.)?;
        assert!(
            proxy
                .poll_events()
                .iter()
                .all(|e| !matches!(e, PlayerEvent::Error(_)))
        );
        proxy.command(&["fail"])?;
        let mut found = false;
        let until = Instant::now() + Duration::from_secs(5);
        while !found {
            found = proxy.poll_events().iter().any(|e| matches!(e, PlayerEvent::Error(message) if message == "injected operation error"));
            ensure!(Instant::now() < until, "operation error lost");
            thread::sleep(Duration::from_millis(2));
        }
        proxy.set_property("volume", "37")?;
        wait(|| has(&control, "property:volume:37"))?;
        wait(|| proxy.snapshot().volume == 37.)?;
        Ok(())
    }

    #[test]
    fn stop_waits_for_unload_and_preserves_prior_commands_later_loads_and_each_reply() -> Result<()>
    {
        let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
        let control = configured();
        let (proxy, frames) = start()?;
        assert!(receive(frames)?.is_ok());
        proxy.load("old")?;
        wait(|| proxy.snapshot().duration == 1.)?;
        proxy.set_property("volume", "37")?;
        let mut first = proxy.stop_for_delete();
        proxy.load("latest")?;
        let mut second = proxy.stop_for_delete();
        wait(|| has(&control, "stop"))?;
        assert_eq!(first.try_recv(), Err(oneshot::error::TryRecvError::Empty));
        assert!(!has(&control, "load:latest"));
        control.change(|g| g.releases = 1);
        assert!(receive(first)?.is_ok());
        wait(|| {
            control
                .gates
                .lock()
                .unwrap()
                .records
                .iter()
                .filter(|r| *r == "stop")
                .count()
                == 2
        })?;
        assert_eq!(second.try_recv(), Err(oneshot::error::TryRecvError::Empty));
        control.change(|g| g.releases = 1);
        assert!(receive(second)?.is_ok());
        assert_eq!(
            control.gates.lock().unwrap().records,
            [
                "load:old",
                "property:volume:37",
                "stop",
                "load:latest",
                "stop"
            ]
        );
        assert_eq!(proxy.snapshot().duration, 0.);
        assert_eq!(proxy.frame_generation(), u64::MAX);
        Ok(())
    }
}
