#[path = "../../src/benchmark.rs"]
pub mod benchmark;
#[path = "../../src/playback.rs"]
pub mod playback;
#[path = "../../src/playback_actor.rs"]
pub mod playback_actor;
pub mod services {
    pub const STREAM_PROTOCOL: &str = "flicktorrent";
}
fn main() {}

#[cfg(test)]
mod actor_native {
    use super::{playback::*, playback_actor::*};
    use anyhow::{Context, Result, bail, ensure};
    use std::{
        fs::File,
        io::{Read, Seek, SeekFrom},
        path::PathBuf,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        thread,
        time::{Duration, Instant},
    };
    use tokio::sync::oneshot;

    fn fixture() -> Result<PathBuf> {
        let path = PathBuf::from(
            std::env::var_os("FLICK_TEST_VIDEO")
                .context("Set FLICK_TEST_VIDEO to a generated fixture")?,
        );
        ensure!(path.is_file(), "Missing fixture: {}", path.display());
        Ok(path)
    }

    fn receive<T>(mut receiver: oneshot::Receiver<T>) -> Result<T> {
        let until = Instant::now() + Duration::from_secs(20);
        loop {
            match receiver.try_recv() {
                Ok(value) => return Ok(value),
                Err(oneshot::error::TryRecvError::Closed) => bail!("Reply canceled"),
                Err(oneshot::error::TryRecvError::Empty) => {
                    ensure!(Instant::now() < until, "Reply timed out")
                }
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn start() -> Result<(PlayerProxy, RenderedFrames)> {
        let opener: Arc<StreamOpener> =
            Arc::new(|_| Err(std::io::Error::other("No stream in this test")));
        let (proxy, ready) = PlayerProxy::start(opener)?;
        let frames = receive(ready)?.map_err(anyhow::Error::msg)?;
        Ok((proxy, frames))
    }

    fn wait(
        proxy: &PlayerProxy,
        predicate: impl Fn(&PlaybackSnapshot) -> bool,
    ) -> Result<PlaybackSnapshot> {
        let until = Instant::now() + Duration::from_secs(20);
        loop {
            for event in proxy.poll_events() {
                if let PlayerEvent::Error(error) = event {
                    bail!(error);
                }
            }
            let snapshot = proxy.snapshot();
            if predicate(&snapshot) {
                return Ok(snapshot);
            }
            ensure!(Instant::now() < until, "Snapshot timed out: {snapshot:?}");
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn frame(proxy: &PlayerProxy, frames: &mut RenderedFrames) -> Result<VideoFrame> {
        let until = Instant::now() + Duration::from_secs(20);
        loop {
            if let Ok((generation, frame)) = frames.try_recv()
                && generation == proxy.frame_generation()
            {
                return frame.map_err(anyhow::Error::msg);
            }
            ensure!(Instant::now() < until, "Frame timed out");
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn native_failure(proxy: &PlayerProxy) -> Result<String> {
        let until = Instant::now() + Duration::from_secs(10);
        loop {
            for event in proxy.poll_events() {
                if let PlayerEvent::Error(error) = event {
                    return Ok(error);
                }
            }
            ensure!(
                Instant::now() < until,
                "Native failure/completion marker was lost"
            );
            thread::sleep(Duration::from_millis(2));
        }
    }

    fn native_barrier(proxy: &PlayerProxy) -> Result<()> {
        // This required property's asynchronous failure proves that all earlier
        // native commands received their replies. An optimistic snapshot alone
        // cannot satisfy the barrier.
        proxy.set_property("flick-seek-completion-marker", "yes")?;
        let error = native_failure(proxy)?;
        ensure!(
            error.to_lowercase().contains("property not found"),
            "{error}"
        );
        Ok(())
    }

    fn exact_frame(
        proxy: &PlayerProxy,
        frames: &mut RenderedFrames,
        before: &VideoFrame,
        target: f64,
    ) -> Result<VideoFrame> {
        while frames.try_recv().is_ok() {}
        proxy.seek(target, true)?;
        native_barrier(proxy)?;
        let until = Instant::now() + Duration::from_secs(10);
        let mut changed = None;
        let mut arrived = None;
        loop {
            for event in proxy.poll_events() {
                if let PlayerEvent::Error(error) = event {
                    bail!(error);
                }
            }
            let snapshot = proxy.snapshot();
            ensure!(snapshot.paused, "Exact seek unpaused playback");
            ensure!(
                (snapshot.position - target).abs() < 0.08,
                "Position regressed after native command completion: target={target}, {snapshot:?}"
            );
            if let Ok((generation, frame)) = frames.try_recv()
                && generation == proxy.frame_generation()
            {
                let frame = frame.map_err(anyhow::Error::msg)?;
                if frame.pixels != before.pixels {
                    arrived.get_or_insert_with(Instant::now);
                    changed = Some(frame);
                }
            }
            // Sample throughout the interval AFTER frame arrival as well as
            // after COMMAND_REPLY; the admission cache cannot pass early.
            if arrived.is_some_and(|at| at.elapsed() >= Duration::from_millis(500)) {
                return Ok(changed.expect("changed native frame"));
            }
            ensure!(
                Instant::now() < until,
                "Exact seek produced no changed frame at {target}"
            );
            thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    #[ignore = "native libmpv, FLICK_AUDIO=null and generated FLICK_TEST_VIDEO"]
    fn native_actor_paused_scrubbing_finishes_exact_and_updates_frame() -> Result<()> {
        let (proxy, mut frames) = start()?;
        // A native seek failure before loading must not poison later seeks.
        proxy.seek(4., true)?;
        ensure!(
            !native_failure(&proxy)?.is_empty(),
            "Missing native seek error"
        );
        proxy.load(fixture()?.to_str().context("UTF-8 fixture")?)?;
        proxy.set_render_target(320, 180);
        wait(&proxy, |snapshot| {
            snapshot.duration > 10. && !snapshot.tracks.is_empty()
        })?;
        proxy.set_property("pause", "yes")?;
        native_barrier(&proxy)?;
        let initial = frame(&proxy, &mut frames)?;
        let before = exact_frame(&proxy, &mut frames, &initial, 1.)?;

        // Hold a paused keyframe drag for six seconds, then release exactly.
        // The proxy may report the keyframe during the hold; the pointer thumb
        // belongs to the UI and is deliberately outside this backend test.
        while frames.try_recv().is_ok() {}
        proxy.seek(6., false)?;
        native_barrier(&proxy)?;
        let held = frame(&proxy, &mut frames)?;
        let hold = Instant::now();
        while hold.elapsed() < Duration::from_secs(6) {
            let snapshot = proxy.snapshot();
            ensure!(snapshot.paused, "Paused drag unpaused playback");
            for event in proxy.poll_events() {
                if let PlayerEvent::Error(error) = event {
                    bail!(error);
                }
            }
            thread::sleep(Duration::from_millis(2));
        }
        let released = exact_frame(&proxy, &mut frames, &held, 6.25)?;
        assert_ne!(before.pixels, released.pixels);

        for n in 0..10_000 {
            proxy.seek((n % 1000) as f64 / 100., false)?;
        }
        let after = exact_frame(&proxy, &mut frames, &released, 8.25)?;
        assert_eq!((after.width, after.height), (320, 180));
        assert_ne!(before.pixels, after.pixels);
        let mut previous = after;
        for trial in 0..30 {
            let target = [1., 8.25, 2.75, 6.4, 10.1][trial % 5];
            previous = exact_frame(&proxy, &mut frames, &previous, target)?;
        }
        // Repeating the final position must also settle without getting stuck
        // when mpv need not emit a different time-pos or frame.
        proxy.seek(10.1, true)?;
        native_barrier(&proxy)?;
        let until = Instant::now() + Duration::from_millis(500);
        while Instant::now() < until {
            let snapshot = proxy.snapshot();
            ensure!(
                snapshot.paused && (snapshot.position - 10.1).abs() < 0.08,
                "{snapshot:?}"
            );
            thread::sleep(Duration::from_millis(2));
        }
        // No new position is admitted here: advancing beyond the exact target
        // proves that the fence retired and native observations publish again.
        proxy.set_property("pause", "no")?;
        native_barrier(&proxy)?;
        wait(&proxy, |snapshot| snapshot.position > 10.3)?;
        let started = Instant::now();
        drop(proxy);
        assert!(started.elapsed() < Duration::from_millis(100));
        Ok(())
    }

    #[test]
    #[ignore = "native libmpv, FLICK_AUDIO=null and generated FLICK_TEST_VIDEO"]
    fn native_actor_playing_exact_seeks_keep_timeline_and_frames_advancing() -> Result<()> {
        playing_seeks(fixture()?.to_str().context("UTF-8 fixture")?)
    }

    #[test]
    #[ignore = "native libmpv, null audio and generated 10fps FLICK_TEST_LOW_FPS"]
    fn native_actor_low_fps_playing_exact_seeks_keep_advancing() -> Result<()> {
        playing_seeks(&std::env::var("FLICK_TEST_LOW_FPS").context("generated 10fps fixture")?)
    }

    fn playing_seeks(source: &str) -> Result<()> {
        let (proxy, mut frames) = start()?;
        proxy.load(source)?;
        proxy.set_render_target(320, 180);
        wait(&proxy, |snapshot| snapshot.duration > 10.)?;
        for (speed, target) in [(1., 3.013), (4., 7.013), (1., 2.013), (4., 6.013)] {
            proxy.set_property("speed", &speed.to_string())?;
            proxy.set_property("pause", "no")?;
            native_barrier(&proxy)?;
            let before = frame(&proxy, &mut frames)?;
            while frames.try_recv().is_ok() {}
            let started = Instant::now();
            proxy.seek(target, true)?;
            native_barrier(&proxy)?;
            // A command reply precedes decoder restart. Require progress
            // within 5s (well before the 10s recovery path), then sample the
            // running timeline and rendered frames for another 650ms.
            let until = Instant::now() + Duration::from_secs(5);
            let mut advancing_at = None;
            let mut previous = target;
            let mut changes = 0;
            let mut last_pixels = before.pixels;
            let mut progress_at = Instant::now();
            loop {
                ensure!(
                    Instant::now() < until,
                    "playing seek did not begin native progress"
                );
                for event in proxy.poll_events() {
                    if let PlayerEvent::Error(error) = event {
                        bail!(error);
                    }
                }
                let snapshot = proxy.snapshot();
                ensure!(!snapshot.paused && !snapshot.eof, "{snapshot:?}");
                ensure!(
                    snapshot.position >= target - 0.08
                        && snapshot.position
                            <= target + started.elapsed().as_secs_f64() * speed + 0.2,
                    "playing seek escaped bounded trajectory: target={target}, speed={speed}, {snapshot:?}"
                );
                ensure!(
                    snapshot.position >= previous - 0.08,
                    "playing position regressed: previous={previous}, {snapshot:?}"
                );
                if snapshot.position > previous + 0.005 {
                    progress_at = Instant::now();
                }
                if snapshot.position > target + 0.05 * speed {
                    advancing_at.get_or_insert_with(Instant::now);
                }
                if advancing_at.is_some() {
                    ensure!(
                        progress_at.elapsed() < Duration::from_millis(400),
                        "playing timeline pinned after native progress: {snapshot:?}"
                    );
                }
                previous = snapshot.position;
                if let Ok((generation, frame)) = frames.try_recv()
                    && generation == proxy.frame_generation()
                {
                    let pixels = frame.map_err(anyhow::Error::msg)?.pixels;
                    if pixels != last_pixels {
                        changes += 1;
                        last_pixels = pixels;
                    }
                }
                if advancing_at.is_some_and(|at| at.elapsed() >= Duration::from_millis(650)) {
                    break;
                }
                thread::sleep(Duration::from_millis(2));
            }
            ensure!(
                previous > target + 0.1 * speed,
                "playing seek never published continued progress: {previous}"
            );
            ensure!(
                changes >= 3,
                "playing seek produced only {changes} changed frames"
            );
        }
        receive(proxy.stop_for_delete())?.map_err(anyhow::Error::msg)?;
        Ok(())
    }

    #[test]
    #[ignore = "native libmpv, null audio and generated 10fps FLICK_TEST_LOW_FPS"]
    fn native_actor_low_fps_paused_seeks_settle_and_frame_steps_publish() -> Result<()> {
        let source = std::env::var("FLICK_TEST_LOW_FPS").context("generated 10fps fixture")?;
        let (proxy, mut frames) = start()?;
        proxy.load(&source)?;
        proxy.set_render_target(160, 90);
        wait(&proxy, |snapshot| snapshot.duration > 10.)?;
        proxy.set_property("pause", "yes")?;
        native_barrier(&proxy)?;
        for target in [3.013, 1.013, 6.013, 2.013] {
            let before = frame(&proxy, &mut frames)?;
            proxy.seek(target, true)?;
            native_barrier(&proxy)?;
            let landing = (target * 10.0_f64).ceil() / 10.;
            let until = Instant::now() + Duration::from_secs(3);
            while (proxy.snapshot().position - landing).abs() > 0.01 {
                ensure!(
                    Instant::now() < until,
                    "low-fps seek pinned: {:?}",
                    proxy.snapshot()
                );
                ensure!(
                    proxy.snapshot().position >= target - 0.05,
                    "low-fps seek exposed pre-seek state: {:?}",
                    proxy.snapshot()
                );
                thread::sleep(Duration::from_millis(2));
            }
            ensure!(proxy.snapshot().paused);
            let after = frame(&proxy, &mut frames)?;
            ensure!(
                after.pixels != before.pixels,
                "low-fps seek did not change the frame"
            );
            proxy.command(&["frame-step"])?;
            native_barrier(&proxy)?;
            wait(&proxy, |snapshot| snapshot.position >= landing + 0.09)?;
        }
        receive(proxy.stop_for_delete())?.map_err(anyhow::Error::msg)?;
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "native libmpv, FLICK_AUDIO=null and generated FLICK_TEST_VIDEO"]
    fn native_actor_stop_acknowledges_closed_local_descriptor() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let source = directory.path().join("unique-stop-fixture.mp4");
        std::fs::copy(fixture()?, &source)?;
        let source = source.canonicalize()?;
        let open_descriptors = || -> Result<usize> {
            Ok(std::fs::read_dir("/proc/self/fd")?
                .filter_map(|entry| std::fs::read_link(entry.ok()?.path()).ok())
                .filter(|path| path == &source)
                .count())
        };
        let (proxy, _frames) = start()?;
        for trial in 0..20 {
            proxy.load(source.to_str().context("UTF-8 fixture")?)?;
            wait(&proxy, |snapshot| snapshot.duration > 10.)?;
            ensure!(
                open_descriptors()? > 0,
                "fixture never opened in trial {trial}"
            );
            receive(proxy.stop_for_delete())?.map_err(anyhow::Error::msg)?;
            ensure!(
                open_descriptors()? == 0,
                "stop acknowledged while native descriptor was open in trial {trial}"
            );
        }
        Ok(())
    }

    #[test]
    #[ignore = "native libmpv, FLICK_AUDIO=null and generated FLICK_TEST_VIDEO"]
    fn native_actor_failed_load_acknowledges_first_stop_without_another_end() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let broken = directory.path().join("generated-broken.mp4");
        std::fs::write(&broken, vec![0xa5; 200_000])?;
        let (proxy, _frames) = start()?;
        for trial in 0..3 {
            // Replace a started file so stale starts/ends cannot contaminate
            // the new requested entry's lifecycle balance.
            proxy.load(fixture()?.to_str().context("UTF-8 fixture")?)?;
            wait(&proxy, |snapshot| snapshot.duration > 10.)?;
            proxy.load(broken.to_str().context("UTF-8 broken fixture")?)?;
            let error = native_failure(&proxy)?;
            ensure!(error.contains("Playback failed"), "{error}");
            let began = Instant::now();
            receive(proxy.stop_for_delete())?.map_err(anyhow::Error::msg)?;
            let elapsed = began.elapsed();
            ensure!(
                elapsed < Duration::from_secs(2),
                "first stop after failed load took {elapsed:?} in trial {trial}"
            );
            println!("failed-load trial {trial}: first stop acknowledged in {elapsed:?}");
            #[cfg(target_os = "linux")]
            ensure!(
                !std::fs::read_dir("/proc/self/fd")?
                    .filter_map(|entry| std::fs::read_link(entry.ok()?.path()).ok())
                    .any(|path| path == broken),
                "broken-file descriptor remained open after stop"
            );
            receive(proxy.stop_for_delete())?.map_err(anyhow::Error::msg)?;
        }
        Ok(())
    }

    #[test]
    #[ignore = "native libmpv, FLICK_AUDIO=null and generated FLICK_TEST_VIDEO"]
    fn native_actor_stop_during_protocol_open_waits_for_stream_close() -> Result<()> {
        let source = fixture()?;
        let gate = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
        let opening = Arc::new(AtomicUsize::new(0));
        let closed = Arc::new(AtomicUsize::new(0));
        let opener: Arc<StreamOpener> = {
            let gate = gate.clone();
            let opening = opening.clone();
            let closed = closed.clone();
            Arc::new(move |_| {
                opening.fetch_add(1, Ordering::SeqCst);
                let mut released = gate.0.lock().unwrap();
                while !*released {
                    released = gate.1.wait(released).unwrap();
                }
                let file = File::open(&source)?;
                let size = file.metadata()?.len();
                Ok(Box::new(TrackedStream {
                    file,
                    size,
                    closed: closed.clone(),
                }))
            })
        };
        let (proxy, ready) = PlayerProxy::start(opener)?;
        let _frames = receive(ready)?.map_err(anyhow::Error::msg)?;
        proxy.load("flicktorrent://held-generated-local-fixture/0")?;
        let until = Instant::now() + Duration::from_secs(5);
        while opening.load(Ordering::SeqCst) == 0 {
            ensure!(Instant::now() < until, "protocol open did not begin");
            thread::sleep(Duration::from_millis(2));
        }
        let mut reply = proxy.stop_for_delete();
        thread::sleep(Duration::from_millis(150));
        let pending = matches!(reply.try_recv(), Err(oneshot::error::TryRecvError::Empty));
        *gate.0.lock().unwrap() = true;
        gate.1.notify_all();
        ensure!(
            pending,
            "stop acknowledged while protocol opener was still blocked"
        );
        receive(reply)?.map_err(anyhow::Error::msg)?;
        ensure!(opening.load(Ordering::SeqCst) == 1);
        ensure!(
            closed.load(Ordering::SeqCst) == 1,
            "stop acknowledged before the generated local stream closed"
        );
        Ok(())
    }

    #[test]
    #[ignore = "native libmpv, FLICK_AUDIO=null and generated FLICK_TEST_VIDEO"]
    fn native_actor_replacement_filters_events_snapshot_frames_and_errors() -> Result<()> {
        let (proxy, mut frames) = start()?;
        let fixture = fixture()?;
        let source = fixture.to_str().context("UTF-8 fixture")?;
        proxy.load(source)?;
        proxy.set_render_target(320, 180);
        wait(&proxy, |snapshot| snapshot.duration > 10.)?;
        let old_generation = proxy.frame_generation();
        frame(&proxy, &mut frames)?;
        // A property prevents the failed load being coalesced away. Its late
        // events/errors must not leak into the accepted replacement.
        proxy.load("/tmp/flick-actor-file-that-does-not-exist.mp4")?;
        proxy.set_property("pause", "yes")?;
        proxy.load(source)?;
        proxy.set_property("pause", "yes")?;
        assert_eq!(proxy.frame_generation(), u64::MAX);
        assert_eq!(proxy.snapshot().duration, 0.);
        assert!(proxy.snapshot().tracks.is_empty());
        proxy.set_render_target(323, 183);
        wait(&proxy, |snapshot| {
            snapshot.duration > 10. && snapshot.paused && !snapshot.tracks.is_empty()
        })?;
        assert!(proxy.frame_generation() > old_generation);
        let newest = frame(&proxy, &mut frames)?;
        assert_eq!((newest.width, newest.height), (323, 183));
        proxy.command(&["not-a-real-flick-native-command"])?;
        let until = Instant::now() + Duration::from_secs(10);
        loop {
            if proxy
                .poll_events()
                .into_iter()
                .any(|event| matches!(event, PlayerEvent::Error(_)))
            {
                break;
            }
            ensure!(Instant::now() < until, "Native operation failure was lost");
            thread::sleep(Duration::from_millis(5));
        }
        proxy.set_property("volume", "37")?;
        wait(&proxy, |snapshot| (snapshot.volume - 37.).abs() < 0.1)?;
        receive(proxy.stop_for_delete())?.map_err(anyhow::Error::msg)?;
        Ok(())
    }

    struct TrackedStream {
        file: File,
        size: u64,
        closed: Arc<AtomicUsize>,
    }
    impl MediaStream for TrackedStream {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            self.file.read(buffer)
        }
        fn seek(&mut self, offset: u64) -> std::io::Result<u64> {
            self.file.seek(SeekFrom::Start(offset))
        }
        fn size(&self) -> Option<u64> {
            Some(self.size)
        }
        fn canceller(&self) -> Arc<dyn Fn() + Send + Sync> {
            Arc::new(|| {})
        }
    }
    impl Drop for TrackedStream {
        fn drop(&mut self) {
            self.closed.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    #[ignore = "native libmpv, FLICK_AUDIO=null and generated FLICK_TEST_VIDEO"]
    fn native_actor_stop_acknowledges_stream_release_and_can_reopen() -> Result<()> {
        let path = fixture()?;
        let opened = Arc::new(AtomicUsize::new(0));
        let closed = Arc::new(AtomicUsize::new(0));
        let opener: Arc<StreamOpener> = {
            let opened = opened.clone();
            let closed = closed.clone();
            Arc::new(move |_| {
                let file = File::open(&path)?;
                let size = file.metadata()?.len();
                opened.fetch_add(1, Ordering::SeqCst);
                Ok(Box::new(TrackedStream {
                    file,
                    size,
                    closed: closed.clone(),
                }))
            })
        };
        let (proxy, ready) = PlayerProxy::start(opener)?;
        let mut frames = receive(ready)?.map_err(anyhow::Error::msg)?;
        for _ in 0..2 {
            proxy.load("flicktorrent://generated-local-fixture/0")?;
            proxy.set_render_target(320, 180);
            wait(&proxy, |snapshot| snapshot.duration > 10.)?;
            frame(&proxy, &mut frames)?;
            let reply = proxy.stop_for_delete();
            assert_eq!(proxy.frame_generation(), u64::MAX);
            assert_eq!(proxy.snapshot().duration, 0.);
            receive(reply)?.map_err(anyhow::Error::msg)?;
            assert!(opened.load(Ordering::SeqCst) > 0);
            assert_eq!(opened.load(Ordering::SeqCst), closed.load(Ordering::SeqCst));
            thread::sleep(Duration::from_millis(100));
            assert_eq!(proxy.frame_generation(), u64::MAX);
            assert_eq!(proxy.snapshot().duration, 0.);
        }
        Ok(())
    }
}

#[cfg(test)]
mod actor_regressions {
    use super::{playback::*, playback_actor::*};
    use anyhow::{Context, Result, bail, ensure};
    use std::{
        sync::Arc,
        thread,
        time::{Duration, Instant},
    };

    fn ready<T>(mut receiver: tokio::sync::oneshot::Receiver<T>) -> Result<T> {
        let until = Instant::now() + Duration::from_secs(15);
        loop {
            match receiver.try_recv() {
                Ok(value) => return Ok(value),
                Err(tokio::sync::oneshot::error::TryRecvError::Closed) => bail!("reply canceled"),
                Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {
                    ensure!(Instant::now() < until, "reply timed out")
                }
            }
            thread::sleep(Duration::from_millis(2));
        }
    }
    fn start() -> Result<(PlayerProxy, RenderedFrames)> {
        let (proxy, frames) = PlayerProxy::start(Arc::new(|_| {
            Err(std::io::Error::other("local fixtures only"))
        }))?;
        Ok((proxy, ready(frames)?.map_err(anyhow::Error::msg)?))
    }

    #[test]
    #[ignore = "native libmpv and FLICK_AUDIO=null"]
    fn native_actor_optional_property_errors_are_quiet_but_required_errors_remain() -> Result<()> {
        let (proxy, _frames) = start()?;
        proxy.set_optional_property("sub-border-style", "3")?;
        proxy.set_optional_property("flick-actor-unsupported-property", "yes")?;
        proxy.set_property("flick-actor-unsupported-property", "yes")?;
        proxy.command(&["not-a-real-actor-native-marker"])?;
        let mut errors = Vec::new();
        let until = Instant::now() + Duration::from_secs(10);
        loop {
            let mut marker = false;
            for event in proxy.poll_events() {
                if let PlayerEvent::Error(error) = event {
                    if !error.to_lowercase().contains("property not found") {
                        marker = true;
                    }
                    errors.push(error);
                }
            }
            if marker {
                break;
            }
            ensure!(
                Instant::now() < until,
                "required native failure was lost: {errors:?}"
            );
            thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(errors.len(), 2, "optional failure reached UI: {errors:?}");
        assert!(errors[0].to_lowercase().contains("property not found"));
        Ok(())
    }

    #[test]
    #[ignore = "native libmpv, FLICK_AUDIO=null and generated FLICK_TEST_VIDEO"]
    fn native_actor_eof_retains_final_media_and_explicit_stop_clears_it() -> Result<()> {
        let (proxy, _frames) = start()?;
        let source = std::env::var("FLICK_TEST_VIDEO").context("generated FLICK_TEST_VIDEO")?;
        proxy.load(&source)?;
        let until = Instant::now() + Duration::from_secs(15);
        while proxy.snapshot().duration <= 0. {
            for event in proxy.poll_events() {
                if let PlayerEvent::Error(error) = event {
                    bail!(error);
                }
            }
            ensure!(Instant::now() < until, "load timed out");
            thread::sleep(Duration::from_millis(5));
        }
        let duration = proxy.snapshot().duration;
        proxy.seek(duration - 0.3, true)?;
        proxy.set_property("pause", "no")?;
        while !proxy.snapshot().eof {
            for event in proxy.poll_events() {
                if let PlayerEvent::Error(error) = event {
                    bail!(error);
                }
            }
            ensure!(
                Instant::now() < until,
                "EOF timed out: {:?}",
                proxy.snapshot()
            );
            thread::sleep(Duration::from_millis(10));
        }
        thread::sleep(Duration::from_millis(250));
        let finished = proxy.snapshot();
        assert!(finished.eof);
        assert!((finished.duration - duration).abs() < 0.01);
        assert!(
            finished.position >= duration - 0.1,
            "final position lost: {finished:?}"
        );
        assert!(!finished.tracks.is_empty());
        ready(proxy.stop_for_delete())?.map_err(anyhow::Error::msg)?;
        thread::sleep(Duration::from_millis(100));
        let stopped = proxy.snapshot();
        assert!(!stopped.eof);
        assert_eq!((stopped.position, stopped.duration), (0., 0.));
        assert!(stopped.tracks.is_empty());
        assert_eq!(proxy.frame_generation(), u64::MAX);
        Ok(())
    }
}
