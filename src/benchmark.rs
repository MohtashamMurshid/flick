//! Sustained software-render benchmark. No GPUI window or transfer session.
use crate::playback::{Player, PlayerEvent};
use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;
use std::{
    ffi::OsString,
    path::PathBuf,
    thread,
    time::{Duration, Instant},
};

#[derive(Debug, PartialEq)]
pub struct Options {
    video: PathBuf,
    seconds: u64,
    width: u32,
    height: u32,
    interval_ms: u64,
}

impl Options {
    pub fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Self> {
        let mut args = args.into_iter();
        let video = args
            .next()
            .context("Supply a local video after --benchmark")?;
        ensure!(
            !video.to_string_lossy().starts_with("--"),
            "Supply a video path before benchmark options"
        );
        let mut options = Self {
            video: video.into(),
            seconds: 300,
            width: 3840,
            height: 2160,
            interval_ms: 16,
        };
        let mut seen = Vec::new();
        while let Some(key) = args.next() {
            let key = key.to_str().context("Benchmark option must be UTF-8")?;
            ensure!(
                !seen.contains(&key.to_owned()),
                "Repeated benchmark option: {key}"
            );
            seen.push(key.to_owned());
            let value = args
                .next()
                .with_context(|| format!("Missing value for {key}"))?;
            let value = value
                .to_str()
                .context("Benchmark option value must be UTF-8")?;
            match key {
                "--seconds" => options.seconds = value.parse().context("Invalid --seconds")?,
                "--interval-ms" => {
                    options.interval_ms = value.parse().context("Invalid --interval-ms")?
                }
                "--size" => {
                    let (width, height) =
                        value.split_once('x').context("Use --size WIDTHxHEIGHT")?;
                    options.width = width.parse().context("Invalid target width")?;
                    options.height = height.parse().context("Invalid target height")?;
                }
                _ => bail!("Unknown benchmark option: {key}"),
            }
        }
        ensure!(options.seconds > 0, "--seconds must be positive");
        ensure!(options.interval_ms > 0, "--interval-ms must be positive");
        ensure!(
            options.width > 0
                && options.height > 0
                && i32::try_from(options.width).is_ok()
                && i32::try_from(options.height).is_ok(),
            "Target dimensions must be positive and representable by libmpv"
        );
        ensure!(
            (options.width as usize)
                .checked_mul(options.height as usize)
                .and_then(|pixels| pixels.checked_mul(4))
                .is_some(),
            "Target byte size overflows"
        );
        ensure!(
            Instant::now()
                .checked_add(Duration::from_secs(options.seconds))
                .is_some()
                && Instant::now()
                    .checked_add(Duration::from_millis(options.interval_ms))
                    .is_some(),
            "Benchmark duration or interval is too large"
        );
        Ok(options)
    }
}

#[derive(Debug, Default, Serialize)]
struct Percentiles {
    count: usize,
    p50: Option<f64>,
    p95: Option<f64>,
    p99: Option<f64>,
    max: Option<f64>,
}

/// Nearest-rank percentiles, including no-frame render calls when requested.
fn percentiles(mut values: Vec<f64>) -> Percentiles {
    values.sort_unstable_by(f64::total_cmp);
    let rank = |percent: usize| {
        (!values.is_empty()).then(|| values[(values.len() * percent).div_ceil(100) - 1])
    };
    Percentiles {
        count: values.len(),
        p50: rank(50),
        p95: rank(95),
        p99: rank(99),
        max: values.last().copied(),
    }
}

#[derive(Debug, Default, Serialize)]
struct Counter {
    initial: Option<u64>,
    last: Option<u64>,
    observed_increase: Option<u64>,
    observed_resets: u64,
    unavailable_samples: u64,
}

impl Counter {
    fn new(value: Option<u64>) -> Self {
        Self {
            initial: value,
            last: value,
            observed_increase: value.map(|_| 0),
            ..Self::default()
        }
    }

    fn sample(&mut self, value: Option<u64>) {
        if let Some(value) = value {
            let increment = self.last.map_or(0, |last| {
                if value < last {
                    self.observed_resets += 1;
                    value
                } else {
                    value - last
                }
            });
            *self.observed_increase.get_or_insert(0) += increment;
            self.last = Some(value);
        } else {
            self.unavailable_samples += 1;
        }
    }
}

#[derive(Default)]
struct Usage {
    user: Option<f64>,
    system: Option<f64>,
    peak_rss: Option<u64>,
}

#[cfg(unix)]
fn usage() -> Usage {
    let mut result = std::mem::MaybeUninit::<libc::rusage>::uninit();
    // SAFETY: getrusage writes a complete rusage on success to valid storage.
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, result.as_mut_ptr()) } != 0 {
        return Usage::default();
    }
    // SAFETY: the successful call above initialized the output.
    let result = unsafe { result.assume_init() };
    let seconds = |time: libc::timeval| time.tv_sec as f64 + time.tv_usec as f64 / 1_000_000.;
    let rss_unit = if cfg!(target_os = "macos") { 1 } else { 1024 };
    Usage {
        user: Some(seconds(result.ru_utime)),
        system: Some(seconds(result.ru_stime)),
        peak_rss: u64::try_from(result.ru_maxrss)
            .ok()
            .and_then(|rss| rss.checked_mul(rss_unit)),
    }
}

#[cfg(not(unix))]
fn usage() -> Usage {
    Usage::default()
}

fn resident_bytes() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let status = std::fs::read_to_string("/proc/self/status").ok()?;
        let kb = status
            .lines()
            .find_map(|line| line.strip_prefix("VmRSS:"))?
            .split_whitespace()
            .next()?
            .parse::<u64>()
            .ok()?;
        kb.checked_mul(1024)
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

#[derive(Debug, Default, Serialize)]
struct Memory {
    /// Peak since process start (getrusage), including player startup.
    process_peak_rss_bytes: Option<u64>,
    sampled_peak_rss_bytes: Option<u64>,
    time_weighted_average_rss_bytes: Option<f64>,
    samples: u64,
    covered_seconds: f64,
    #[serde(skip)]
    previous: Option<(f64, u64)>,
    #[serde(skip)]
    byte_seconds: f64,
}

impl Memory {
    /// Sample-and-hold integral. A missing reading leaves that interval uncovered.
    fn sample(&mut self, seconds: f64, bytes: Option<u64>) {
        if let Some((last_time, last_bytes)) = self.previous {
            let elapsed = (seconds - last_time).max(0.);
            self.byte_seconds += elapsed * last_bytes as f64;
            self.covered_seconds += elapsed;
        }
        self.previous = bytes.map(|bytes| (seconds, bytes));
        if let Some(bytes) = bytes {
            self.samples += 1;
            self.sampled_peak_rss_bytes = Some(self.sampled_peak_rss_bytes.unwrap_or(0).max(bytes));
        }
        self.time_weighted_average_rss_bytes =
            (self.covered_seconds > 0.).then(|| self.byte_seconds / self.covered_seconds);
    }
}

#[derive(Default, Serialize)]
struct Source {
    width: Option<u64>,
    height: Option<u64>,
    fps: Option<f64>,
    fps_property: Option<String>,
    codec: Option<String>,
}

fn positive_number(player: &Player, name: &str) -> Option<f64> {
    player
        .diagnostic_property(name)?
        .parse::<f64>()
        .ok()
        .filter(|n| n.is_finite() && *n > 0.)
}

fn source(player: &Player) -> Source {
    let params = player
        .diagnostic_property("video-params")
        .and_then(|json| serde_json::from_str::<serde_json::Value>(&json).ok())
        .unwrap_or_default();
    let fps = ["container-fps", "estimated-vf-fps"]
        .into_iter()
        .find_map(|name| positive_number(player, name).map(|fps| (fps, name.to_owned())));
    Source {
        width: params["w"].as_u64(),
        height: params["h"].as_u64(),
        fps: fps.as_ref().map(|(fps, _)| *fps),
        fps_property: fps.map(|(_, name)| name),
        codec: player.diagnostic_property("video-codec"),
    }
}

#[derive(Default, Serialize)]
struct Cpu {
    user_seconds: Option<f64>,
    system_seconds: Option<f64>,
    /// One fully occupied logical CPU is 100%; multithreaded decode may exceed it.
    average_percent: Option<f64>,
}

fn cpu_summary(before: &Usage, after: &Usage, elapsed: f64) -> Cpu {
    let user = before.user.zip(after.user).map(|(a, b)| (b - a).max(0.));
    let system = before
        .system
        .zip(after.system)
        .map(|(a, b)| (b - a).max(0.));
    Cpu {
        user_seconds: user,
        system_seconds: system,
        average_percent: user
            .zip(system)
            .filter(|_| elapsed > 0.)
            .map(|(user, system)| 100. * (user + system) / elapsed),
    }
}

#[derive(Default, Serialize)]
struct Report {
    schema_version: u32,
    presentation: &'static str,
    build_profile: &'static str,
    os: &'static str,
    arch: &'static str,
    video: PathBuf,
    requested_seconds: u64,
    elapsed_seconds: f64,
    startup_seconds: f64,
    interval_ms: u64,
    target_size: [u32; 2],
    frames_presented: u64,
    expected_frames: Option<f64>,
    skipped_tick_deadlines: u64,
    source: Source,
    hardware_decoder: Option<String>,
    hardware_decoder_initial: Option<String>,
    flick_hwdec: Option<String>,
    effective_hwdec: String,
    mpv_version: Option<String>,
    flick_audio: Option<String>,
    frame_drop_count: Counter,
    decoder_frame_drop_count: Counter,
    render_call_ms: Percentiles,
    frame_render_ms: Percentiles,
    cpu: Cpu,
    memory: Memory,
}

fn poll(player: &Player) -> Result<()> {
    for event in player.poll_events() {
        match event {
            PlayerEvent::Error(error) => bail!(error),
            PlayerEvent::EndFile => bail!("Video ended despite loop-file=inf"),
            PlayerEvent::FileLoaded => {}
        }
    }
    Ok(())
}

/// Keep a fixed phase and skip missed ticks instead of rendering in bursts.
fn next_tick_delay(late: Duration, interval: Duration) -> (Duration, u64) {
    let interval_ns = interval.as_nanos();
    let remaining_ns = interval_ns - late.as_nanos() % interval_ns;
    let delay = Duration::new(
        (remaining_ns / 1_000_000_000) as u64,
        (remaining_ns % 1_000_000_000) as u32,
    );
    (delay, (late.as_nanos() / interval_ns + 1) as u64)
}

fn drop_count(player: &Player, name: &str) -> Option<u64> {
    player.diagnostic_property(name)?.parse().ok()
}

pub fn run(options: Options) -> Result<()> {
    ensure!(
        options.video.is_file(),
        "Video does not exist: {}",
        options.video.display()
    );
    let startup = Instant::now();
    let player = Player::new()?;
    player.set_property("loop-file", "inf")?;
    player.load(
        options
            .video
            .to_str()
            .context("Video path must be valid UTF-8")?,
    )?;
    // Warm up through the first software submission; exclude it from summaries.
    loop {
        poll(&player)?;
        if player.render(options.width, options.height)?.is_some() {
            break;
        }
        ensure!(
            startup.elapsed() < Duration::from_secs(30),
            "No initial video frame within 30 seconds"
        );
        thread::sleep(Duration::from_millis(options.interval_ms.min(16)));
    }
    let source = source(&player);
    let mut vo_drops = Counter::new(drop_count(&player, "frame-drop-count"));
    let mut decoder_drops = Counter::new(drop_count(&player, "decoder-frame-drop-count"));
    let hardware_decoder = player.hardware_decoder();
    let mut memory = Memory::default();
    memory.sample(0., resident_bytes());
    let before = usage();
    let started = Instant::now();
    let deadline = started
        .checked_add(Duration::from_secs(options.seconds))
        .context("Duration too large")?;
    let interval = Duration::from_millis(options.interval_ms);
    let mut next_tick = started;
    let mut next_sample = Duration::from_secs(1);
    let mut frames = 0;
    let mut skipped_ticks = 0;
    let mut render_times = Vec::new();
    let mut frame_times = Vec::new();
    while Instant::now() < deadline {
        poll(&player)?;
        let render_started = Instant::now();
        let frame = player.render(options.width, options.height)?;
        let render_ms = render_started.elapsed().as_secs_f64() * 1000.;
        render_times.push(render_ms);
        if frame.is_some() {
            frames += 1;
            frame_times.push(render_ms);
        }
        vo_drops.sample(drop_count(&player, "frame-drop-count"));
        decoder_drops.sample(drop_count(&player, "decoder-frame-drop-count"));
        let elapsed = started.elapsed();
        if elapsed >= next_sample {
            // Keep the current frame alive while sampling its resident allocation.
            memory.sample(elapsed.as_secs_f64(), resident_bytes());
            next_sample = elapsed + Duration::from_secs(1);
        }
        drop(frame);
        next_tick += interval;
        let now = Instant::now();
        if next_tick < now {
            // Skip missed deadlines; never issue a burst of catch-up renders.
            let (delay, missed) = next_tick_delay(now - next_tick, interval);
            skipped_ticks += missed;
            next_tick = now + delay;
        }
        thread::sleep(
            next_tick
                .min(deadline)
                .saturating_duration_since(Instant::now()),
        );
    }
    poll(&player)?;
    vo_drops.sample(drop_count(&player, "frame-drop-count"));
    decoder_drops.sample(drop_count(&player, "decoder-frame-drop-count"));
    let elapsed = started.elapsed().as_secs_f64();
    let after = usage();
    memory.sample(elapsed, resident_bytes());
    memory.process_peak_rss_bytes = after.peak_rss;
    ensure!(frames > 0, "No frames rendered during measurement");
    let report = Report {
        schema_version: 1,
        presentation: "headless-software-bgr0-submissions",
        build_profile: if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        },
        os: std::env::consts::OS,
        arch: std::env::consts::ARCH,
        video: options.video,
        requested_seconds: options.seconds,
        elapsed_seconds: elapsed,
        startup_seconds: (started - startup).as_secs_f64(),
        interval_ms: options.interval_ms,
        target_size: [options.width, options.height],
        frames_presented: frames,
        expected_frames: source.fps.map(|fps| fps * elapsed),
        skipped_tick_deadlines: skipped_ticks,
        source,
        hardware_decoder: player.hardware_decoder(),
        hardware_decoder_initial: hardware_decoder,
        flick_hwdec: std::env::var("FLICK_HWDEC").ok(),
        effective_hwdec: std::env::var("FLICK_HWDEC").unwrap_or_else(|_| "auto-copy".into()),
        mpv_version: player.diagnostic_property("mpv-version"),
        flick_audio: std::env::var("FLICK_AUDIO").ok(),
        frame_drop_count: vo_drops,
        decoder_frame_drop_count: decoder_drops,
        render_call_ms: percentiles(render_times),
        frame_render_ms: percentiles(frame_times),
        cpu: cpu_summary(&before, &after, elapsed),
        memory,
    };
    println!("{}", serde_json::to_string(&report)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Options> {
        Options::parse(args.iter().map(OsString::from))
    }

    #[test]
    fn arguments_defaults_and_overrides() -> Result<()> {
        let defaults = parse(&["a video.mp4"])?;
        assert_eq!(
            (
                defaults.seconds,
                defaults.width,
                defaults.height,
                defaults.interval_ms
            ),
            (300, 3840, 2160, 16)
        );
        assert_eq!(defaults.video, PathBuf::from("a video.mp4"));
        let custom = parse(&[
            "clip.mp4",
            "--seconds",
            "2",
            "--size",
            "1920x1080",
            "--interval-ms",
            "8",
        ])?;
        assert_eq!(
            (
                custom.seconds,
                custom.width,
                custom.height,
                custom.interval_ms
            ),
            (2, 1920, 1080, 8)
        );
        Ok(())
    }

    #[test]
    fn invalid_arguments_fail_without_native_libraries() {
        for args in [
            vec![],
            vec!["--seconds", "2"],
            vec!["v", "--seconds"],
            vec!["v", "--seconds", "0"],
            vec!["v", "--seconds", "nan"],
            vec!["v", "--interval-ms", "0"],
            vec!["v", "--size", "0x1"],
            vec!["v", "--size", "1X2"],
            vec!["v", "--size", "4294967295x2"],
            vec!["v", "--size", "1x2x3"],
            vec!["v", "extra"],
            vec!["v", "--unknown", "2"],
            vec!["v", "--seconds", "1", "--seconds", "2"],
        ] {
            assert!(parse(&args).is_err(), "Accepted {args:?}");
        }
    }

    #[test]
    fn nearest_rank_summary_and_unavailable_metrics() -> Result<()> {
        let values = (1..=100).rev().map(f64::from).collect();
        let summary = percentiles(values);
        assert_eq!(
            (summary.p50, summary.p95, summary.p99, summary.max),
            (Some(50.), Some(95.), Some(99.), Some(100.))
        );
        assert_eq!(percentiles(vec![4.]).p99, Some(4.));
        assert_eq!(percentiles(vec![]).p50, None);
        let cpu = cpu_summary(
            &Usage {
                user: Some(1.),
                system: Some(2.),
                ..Usage::default()
            },
            &Usage {
                user: Some(4.),
                system: Some(3.),
                ..Usage::default()
            },
            2.,
        );
        assert_eq!(cpu.average_percent, Some(200.));
        assert_eq!(
            cpu_summary(&Usage::default(), &Usage::default(), 1.).average_percent,
            None
        );
        let json = serde_json::to_value(Report {
            schema_version: 1,
            elapsed_seconds: 300.,
            target_size: [3840, 2160],
            frames_presented: 8999,
            expected_frames: Some(9000.),
            source: Source {
                width: Some(3840),
                height: Some(2160),
                fps: Some(30.),
                ..Source::default()
            },
            ..Report::default()
        })?;
        assert!(json["cpu"]["user_seconds"].is_null());
        assert!(json["memory"]["time_weighted_average_rss_bytes"].is_null());
        assert!(json["frame_drop_count"]["observed_increase"].is_null());
        assert!(json["render_call_ms"]["p99"].is_null());
        assert!(json["frames_presented"].is_u64());
        assert_eq!(json["schema_version"], 1);
        assert_eq!(json["elapsed_seconds"], 300.);
        assert_eq!(json["expected_frames"], 9000.);
        assert_eq!(json["target_size"], serde_json::json!([3840, 2160]));
        assert_eq!(json["source"]["fps"], 30.);
        assert!(json["memory"].get("previous").is_none());
        assert!(json["memory"].get("byte_seconds").is_none());
        Ok(())
    }

    #[test]
    fn overrun_skips_ticks_without_changing_the_phase() {
        let interval = Duration::from_millis(16);
        assert_eq!(
            next_tick_delay(Duration::from_millis(1), interval),
            (Duration::from_millis(15), 1)
        );
        assert_eq!(
            next_tick_delay(Duration::from_millis(32), interval),
            (interval, 3)
        );
        assert_eq!(
            next_tick_delay(Duration::from_millis(35), interval),
            (Duration::from_millis(13), 3)
        );
        // The interval is stored in milliseconds, which may exceed u64 nanoseconds.
        let huge = Duration::from_secs(20_000_000_000);
        assert_eq!(
            next_tick_delay(Duration::from_secs(1), huge),
            (huge - Duration::from_secs(1), 1)
        );
    }

    #[test]
    fn counter_resets_and_time_weighted_memory() {
        let mut counter = Counter::new(Some(4));
        for value in [Some(7), Some(1), None, Some(3)] {
            counter.sample(value);
        }
        assert_eq!(counter.observed_increase, Some(6));
        assert_eq!(counter.observed_resets, 1);
        assert_eq!(counter.unavailable_samples, 1);
        let mut memory = Memory::default();
        memory.sample(0., Some(100));
        memory.sample(1., Some(200));
        memory.sample(4., Some(50));
        assert_eq!(memory.time_weighted_average_rss_bytes, Some(175.));
        assert_eq!(memory.sampled_peak_rss_bytes, Some(200));
        memory.sample(5., None);
        memory.sample(7., Some(500));
        assert_eq!(memory.covered_seconds, 5.);
        assert_eq!(memory.time_weighted_average_rss_bytes, Some(150.));
    }

    #[test]
    #[ignore = "requires native libmpv and local FLICK_TEST_VIDEO"]
    fn native_benchmark() -> Result<()> {
        run(Options {
            video: std::env::var_os("FLICK_TEST_VIDEO")
                .context("Set FLICK_TEST_VIDEO")?
                .into(),
            seconds: 2,
            width: 320,
            height: 180,
            interval_ms: 16,
        })
    }
}
