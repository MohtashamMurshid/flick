//! FFmpeg discovery, the quality ladder, and transcode jobs.
//!
//! FFmpeg always runs as a separate process with an argument list, never a
//! shell, and with `kill_on_drop` so a dropped request or a stopped server
//! never leaves it running. Tests drive a generated stand-in `ffmpeg`
//! script, as the yt-dlp tests do; real FFmpeg is only used by an
//! `#[ignore]` test in `tests/remote_hls.rs`.

use super::protocol::{EmbeddedSubtitle, HLS_SEGMENT_SECONDS, Quality};
use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::{
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::process::Command;

/// H.264 encoders in order of preference: hardware first, then x264.
pub const ENCODER_PREFERENCE: [&str; 4] =
    ["h264_videotoolbox", "h264_nvenc", "h264_qsv", "libx264"];

/// Overrides the FFmpeg executable, like `FLICK_YTDLP` does for yt-dlp.
pub const FFMPEG_VARIABLE: &str = "FLICK_FFMPEG";

const PROBE_TIMEOUT: Duration = Duration::from_secs(15);
const INSPECT_TIMEOUT: Duration = Duration::from_secs(60);
const EXTRACT_TIMEOUT: Duration = Duration::from_secs(300);
/// WebVTT output larger than this is not a subtitle track.
const MAX_WEBVTT_BYTES: usize = 64 * 1024 * 1024;
/// Output kept from `ffmpeg -encoders` and ffprobe, which are far smaller.
const MAX_QUIET_BYTES: usize = 16 * 1024 * 1024;
/// The end of a program's error output kept for its error message.
const STDERR_TAIL_BYTES: usize = 16 * 1024;

/// The FFmpeg installation the server transcodes with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Encoder {
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
    /// One of [`ENCODER_PREFERENCE`].
    pub video_encoder: String,
}

/// What the server needs to know about a source before transcoding it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MediaInfo {
    /// Seconds, or 0 when unknown. A transcode needs it for its playlist.
    pub duration: f64,
    /// Of the first video stream that is not cover art, before rotation.
    pub width: u32,
    pub height: u32,
    /// Average frame rate, or 0 when unknown. Sets the key-frame interval.
    pub frame_rate: f64,
    /// PQ or HLG transfer, or BT.2020 primaries: tone-mapped to SDR.
    pub hdr: bool,
    /// ffprobe's `color_transfer` for the video stream, empty when unknown.
    pub color_transfer: String,
    /// Subtitle streams in file order; `track` is the index among them.
    pub subtitles: Vec<EmbeddedSubtitle>,
}

/// Finds ffmpeg and ffprobe and the best H.264 encoder that works.
///
/// The search order is `explicit`, then `FLICK_FFMPEG`, then `PATH`, then
/// `~/.local/bin`, `/opt/homebrew/bin`, and `/usr/local/bin`, which apps
/// opened from a desktop may not have on `PATH`. ffprobe is looked for
/// beside ffmpeg first. An explicit path or variable that names nothing
/// usable returns None rather than silently using another FFmpeg.
pub async fn probe(explicit: Option<&Path>) -> Option<Encoder> {
    let variable = std::env::var_os(FFMPEG_VARIABLE).filter(|v| !v.is_empty());
    let chosen = explicit.map(Path::to_owned).or(variable.map(PathBuf::from));
    let path = std::env::var_os("PATH");
    let Some((ffmpeg, ffprobe)) = locate(chosen.as_deref(), path.as_deref(), &extra_dirs()) else {
        tracing::info!("FFmpeg was not found; only Original quality is offered");
        return None;
    };
    let listing = match run_quiet(
        &ffmpeg,
        ["-hide_banner", "-encoders"].map(OsString::from),
        PROBE_TIMEOUT,
    )
    .await
    {
        Ok(output) => output,
        Err(error) => {
            tracing::warn!(%error, ffmpeg = %ffmpeg.display(), "FFmpeg did not list its encoders");
            return None;
        }
    };
    for candidate in listed_encoders(&listing) {
        // Hardware encoders are listed by builds that support them even on
        // machines without the hardware, so try one tiny encode first.
        if candidate == "libx264" || encodes(&ffmpeg, candidate).await {
            tracing::info!(encoder = candidate, ffmpeg = %ffmpeg.display(), "Transcoding is available");
            return Some(Encoder {
                ffmpeg,
                ffprobe,
                video_encoder: candidate.to_owned(),
            });
        }
        tracing::info!(
            encoder = candidate,
            "Encoder is listed but does not work here"
        );
    }
    tracing::info!(ffmpeg = %ffmpeg.display(), "FFmpeg has no usable H.264 encoder");
    None
}

/// The preferred encoders that an `ffmpeg -encoders` listing contains, best
/// first.
pub fn listed_encoders(listing: &str) -> Vec<&'static str> {
    let names: Vec<&str> = listing
        .lines()
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            let flags = words.next()?;
            let name = words.next()?;
            (flags.len() == 6 && flags.starts_with('V')).then_some(name)
        })
        .collect();
    ENCODER_PREFERENCE
        .into_iter()
        .filter(|wanted| names.contains(wanted))
        .collect()
}

/// The encoder [`probe`] would choose from a listing if every listed
/// encoder worked.
pub fn choose_encoder(listing: &str) -> Option<&'static str> {
    listed_encoders(listing).first().copied()
}

async fn encodes(ffmpeg: &Path, encoder: &str) -> bool {
    let args = [
        "-nostdin",
        "-hide_banner",
        "-loglevel",
        "error",
        "-f",
        "lavfi",
        "-i",
        "color=c=black:s=320x240:r=30:d=0.2",
        "-frames:v",
        "3",
        "-pix_fmt",
        if encoder == "h264_qsv" {
            "nv12"
        } else {
            "yuv420p"
        },
        "-c:v",
        encoder,
        "-b:v",
        "500k",
        "-f",
        "null",
        "-",
    ]
    .map(OsString::from);
    run_quiet(ffmpeg, args, PROBE_TIMEOUT).await.is_ok()
}

/// Finds `ffmpeg` and `ffprobe`. `explicit` may name the ffmpeg executable
/// or the folder that holds it.
fn locate(
    explicit: Option<&Path>,
    path: Option<&OsStr>,
    extra: &[PathBuf],
) -> Option<(PathBuf, PathBuf)> {
    let ffmpeg_name = program_name("ffmpeg");
    let ffprobe_name = program_name("ffprobe");
    let ffmpeg = match explicit {
        Some(explicit) if explicit.is_dir() => {
            Some(explicit.join(&ffmpeg_name)).filter(|p| executable(p))
        }
        Some(explicit) => Some(explicit.to_owned()).filter(|p| executable(p)),
        None => search(path, extra, &ffmpeg_name),
    };
    let Some(ffmpeg) = ffmpeg else {
        if let Some(explicit) = explicit {
            tracing::warn!(path = %explicit.display(), "The configured FFmpeg is not an executable file");
        }
        return None;
    };
    let ffprobe = ffmpeg
        .parent()
        .map(|dir| dir.join(&ffprobe_name))
        .filter(|p| executable(p))
        .or_else(|| search(path, extra, &ffprobe_name))?;
    Some((ffmpeg, ffprobe))
}

fn program_name(name: &str) -> String {
    format!("{name}{}", std::env::consts::EXE_SUFFIX)
}

fn search(path: Option<&OsStr>, extra: &[PathBuf], file: &str) -> Option<PathBuf> {
    path.into_iter()
        .flat_map(std::env::split_paths)
        .chain(extra.iter().cloned())
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(file))
        .find(|candidate| executable(candidate))
}

fn executable(path: &Path) -> bool {
    let Ok(metadata) = path.metadata() else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        metadata.is_file()
    }
}

fn extra_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(base) = directories::BaseDirs::new()
        && !cfg!(windows)
    {
        dirs.push(base.home_dir().join(".local").join("bin"));
    }
    if cfg!(unix) {
        dirs.extend(["/opt/homebrew/bin", "/usr/local/bin"].map(PathBuf::from));
    }
    dirs
}

/// A command with no window, no stdin, and killed if its future is dropped.
pub(crate) fn command(program: &Path) -> Command {
    let mut command = Command::new(program);
    command.stdin(Stdio::null()).kill_on_drop(true);
    #[cfg(windows)]
    command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    command
}

/// Runs a program to completion and returns its stdout, or an error with
/// the end of its stderr.
async fn run_quiet(
    program: &Path,
    args: impl IntoIterator<Item = OsString>,
    limit: Duration,
) -> Result<String> {
    let output = run(program, args, limit, MAX_QUIET_BYTES).await?;
    String::from_utf8(output).context("The output was not UTF-8")
}

/// Runs a program to completion and returns its stdout. Stdout past
/// `max_stdout` bytes stops the program with an error, and only the end of
/// stderr is kept, so memory stays bounded whatever the program writes.
async fn run(
    program: &Path,
    args: impl IntoIterator<Item = OsString>,
    limit: Duration,
    max_stdout: usize,
) -> Result<Vec<u8>> {
    use tokio::io::AsyncReadExt;
    let mut command = command(program);
    command
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .with_context(|| format!("Could not start {}", program.display()))?;
    let stdout = child.stdout.take().context("stdout")?;
    let mut stderr = child.stderr.take().context("stderr")?;
    let finished = tokio::time::timeout(limit, async {
        let mut output = Vec::new();
        let mut tail = Vec::new();
        let read_stdout = async {
            let read = stdout
                .take(max_stdout as u64 + 1)
                .read_to_end(&mut output)
                .await;
            if output.len() > max_stdout {
                // Stop it now rather than wait for it to notice the closed
                // pipe while stderr is still being drained.
                let _ = child.start_kill();
            }
            read
        };
        // Drained to the end, so a chatty program never blocks on it.
        let read_stderr = async {
            let mut chunk = [0u8; 4096];
            loop {
                let read = stderr.read(&mut chunk).await?;
                if read == 0 {
                    return Ok::<_, std::io::Error>(());
                }
                tail.extend_from_slice(&chunk[..read]);
                if tail.len() > STDERR_TAIL_BYTES {
                    tail.drain(..tail.len() - STDERR_TAIL_BYTES);
                }
            }
        };
        let (read, _) = tokio::join!(read_stdout, read_stderr);
        read.with_context(|| format!("Read the output of {}", program.display()))?;
        if output.len() > max_stdout {
            bail!("{} wrote more than {max_stdout} bytes", program.display());
        }
        let status = child.wait().await?;
        Ok((status, output, tail))
    })
    .await
    .with_context(|| format!("{} did not finish in time", program.display()))?;
    let (status, output, tail) = finished?;
    if !status.success() {
        bail!(
            "{} failed ({}): {}",
            program.display(),
            status,
            stderr_tail(&tail)
        );
    }
    Ok(output)
}

/// The last non-empty line of FFmpeg's error output.
pub(crate) fn stderr_tail(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let line = text
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("no error output");
    line.chars().take(500).collect()
}

/// FFmpeg reads `file:` URLs literally, so a file name with a colon or a
/// leading dash is never taken for a protocol or an option.
fn file_url(path: &Path) -> OsString {
    let mut url = OsString::from("file:");
    url.push(path.as_os_str());
    url
}

/// Reads duration, size, frame rate, HDR, and subtitle streams with ffprobe.
pub async fn inspect(encoder: &Encoder, path: &Path) -> Result<MediaInfo> {
    let mut args: Vec<OsString> = [
        "-v",
        "error",
        "-print_format",
        "json",
        "-show_format",
        "-show_streams",
    ]
    .map(OsString::from)
    .into();
    args.push(file_url(path));
    let json = run_quiet(&encoder.ffprobe, args, INSPECT_TIMEOUT)
        .await
        .with_context(|| format!("ffprobe could not read {}", path.display()))?;
    parse_probe(&json)
}

/// Parses `ffprobe -print_format json -show_format -show_streams`.
pub fn parse_probe(json: &str) -> Result<MediaInfo> {
    let value: Value = serde_json::from_str(json).context("ffprobe returned invalid JSON")?;
    let streams = value
        .get("streams")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let text = |v: &Value, key: &str| v.get(key).and_then(Value::as_str).map(str::to_owned);
    let number = |v: &Value, key: &str| {
        v.get(key)
            .and_then(|n| n.as_str().and_then(|s| s.parse().ok()).or(n.as_f64()))
            .filter(|n: &f64| n.is_finite() && *n > 0.0)
    };
    let video = streams.iter().find(|s| {
        text(s, "codec_type").as_deref() == Some("video")
            && s.pointer("/disposition/attached_pic")
                .and_then(Value::as_i64)
                != Some(1)
    });
    let mut info = MediaInfo::default();
    if let Some(video) = video {
        let size = |key| {
            video
                .get(key)
                .and_then(Value::as_u64)
                .and_then(|n| u32::try_from(n).ok())
                .unwrap_or(0)
        };
        info.width = size("width");
        info.height = size("height");
        info.frame_rate = ["avg_frame_rate", "r_frame_rate"]
            .into_iter()
            .find_map(|key| text(video, key).and_then(|rate| parse_rate(&rate)))
            .unwrap_or(0.0);
        info.color_transfer = text(video, "color_transfer").unwrap_or_default();
        let primaries = text(video, "color_primaries").unwrap_or_default();
        info.hdr = matches!(
            info.color_transfer.as_str(),
            "smpte2084" | "arib-std-b67" | "arithmetic-hlg"
        ) || primaries == "bt2020";
    }
    info.duration = value
        .get("format")
        .and_then(|format| number(format, "duration"))
        .or_else(|| video.and_then(|v| number(v, "duration")))
        .unwrap_or(0.0);
    info.subtitles = streams
        .iter()
        .filter(|s| text(s, "codec_type").as_deref() == Some("subtitle"))
        .zip(0..)
        .map(|(stream, track)| {
            let tag = |key| {
                stream
                    .pointer(&format!("/tags/{key}"))
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|t| !t.is_empty() && *t != "und")
                    .map(str::to_owned)
            };
            EmbeddedSubtitle {
                track,
                language: tag("language"),
                title: tag("title"),
                text: text(stream, "codec_name").is_some_and(|codec| is_text_subtitle(&codec)),
            }
        })
        .collect();
    Ok(info)
}

/// Text subtitle codecs FFmpeg converts to WebVTT. Bitmap ones
/// (`hdmv_pgs_subtitle`, `dvd_subtitle`, `dvb_subtitle`) and anything
/// unknown cannot be shown in a transcode.
pub fn is_text_subtitle(codec: &str) -> bool {
    matches!(
        codec,
        "subrip" | "srt" | "ass" | "ssa" | "webvtt" | "mov_text"
    )
}

fn parse_rate(rate: &str) -> Option<f64> {
    let (num, den) = rate.split_once('/').unwrap_or((rate, "1"));
    let rate = num.trim().parse::<f64>().ok()? / den.trim().parse::<f64>().ok()?;
    (rate.is_finite() && rate > 0.0 && rate < 1000.0).then_some(rate)
}

/// The width of a 16:9 box `height` tall, rounded to an even number.
fn box_width(height: u32) -> u32 {
    (height * 16).div_ceil(9).div_ceil(2) * 2
}

/// The FFmpeg arguments for one job that starts at `start_segment` and
/// writes `init.mp4` and `{n}.m4s`, numbered from `start_segment`, to
/// `out_dir`. Every segment is independently decodable and carries its
/// absolute time, so segments from different jobs of the same source and
/// tier are interchangeable.
///
/// [`Quality::Original`] is not a transcode; it is treated as 1080p High.
pub fn job_args(
    encoder: &Encoder,
    source: &Path,
    info: &MediaInfo,
    quality: Quality,
    start_segment: u32,
    out_dir: &Path,
) -> Vec<OsString> {
    let quality = if quality.is_transcode() {
        quality
    } else {
        Quality::High1080
    };
    let height = quality.height().unwrap_or(1080);
    let video_kbps = quality.video_kbps().unwrap_or(12_000);
    let audio_kbps = quality.audio_kbps().unwrap_or(192);
    let segment = HLS_SEGMENT_SECONDS;
    let start_seconds = u64::from(start_segment) * u64::from(segment);
    let encoder_name = encoder.video_encoder.as_str();
    // Key frames every segment: the interval in frames, for encoders that
    // ignore -force_key_frames between their own key frames.
    let gop = if info.frame_rate > 0.0 {
        ((info.frame_rate * f64::from(segment)).round() as u32).max(1)
    } else {
        60 * segment
    };
    let pixel_format = if encoder_name == "h264_qsv" {
        "nv12"
    } else {
        "yuv420p"
    };

    // Fit inside the tier's 16:9 box, never upscale, keep the aspect ratio,
    // and keep both dimensions even as 4:2:0 H.264 requires.
    let mut filters = format!(
        "scale=w=min(iw\\,{width}):h=min(ih\\,{height}):force_original_aspect_ratio=decrease:force_divisible_by=2",
        width = box_width(height)
    );
    if info.hdr {
        // Tone-map to SDR BT.709 with zimg: linearize the BT.2020 source
        // (PQ, HLG, or an SDR BT.2020 curve), convert primaries in float,
        // compress highlights with Hable, then encode as BT.709.
        let transfer = match info.color_transfer.as_str() {
            "smpte2084" => "smpte2084",
            "arib-std-b67" | "arithmetic-hlg" => "arib-std-b67",
            _ => "bt709",
        };
        filters.push_str(&format!(
            ",zscale=tin={transfer}:min=bt2020nc:pin=bt2020:rin=tv:t=linear:npl=100,format=gbrpf32le,zscale=p=bt709,tonemap=tonemap=hable:desat=0,zscale=t=bt709:m=bt709:r=tv"
        ));
    }
    // 8-bit 4:2:0, which every hardware H.264 decoder accepts.
    filters.push_str(&format!(",format={pixel_format}"));

    let mut args: Vec<OsString> = Vec::new();
    let mut push = |values: &[&str]| args.extend(values.iter().map(OsString::from));
    // No interactive keys, no banner, only errors on stderr.
    push(&["-nostdin", "-hide_banner", "-loglevel", "error"]);
    // Seek before -i: fast input seeking, and output timestamps restart at 0
    // at the start of the segment.
    push(&["-ss", &start_seconds.to_string()]);
    args.push("-i".into());
    args.push(file_url(source));
    let mut push = |values: &[&str]| args.extend(values.iter().map(OsString::from));
    // The first video stream that is not cover art, and the default audio
    // track if there is one. No subtitles, data, chapters, or metadata.
    push(&["-map", "0:V:0", "-map", "0:a:0?"]);
    push(&["-map_metadata", "-1", "-map_chapters", "-1"]);
    push(&["-vf", &filters]);
    push(&["-c:v", encoder_name, "-profile:v", "high"]);
    match encoder_name {
        // A fast x264 preset keeps 1080p real time on ordinary CPUs; no
        // scene-cut key frames, so segments stay exactly on the boundaries.
        "libx264" => push(&["-preset", "veryfast", "-sc_threshold", "0"]),
        "h264_nvenc" => push(&["-preset", "p4", "-rc", "vbr"]),
        "h264_qsv" => push(&["-preset", "faster"]),
        _ => {}
    }
    // Target bit rate with a peak cap and a two-second buffer, so a segment
    // never needs much more than the tier's bandwidth.
    let rate = format!("{video_kbps}k");
    push(&[
        "-b:v",
        &rate,
        "-maxrate",
        &rate,
        "-bufsize",
        &format!("{}k", video_kbps * 2),
    ]);
    // A key frame exactly on every segment boundary, and none in between.
    let gop = gop.to_string();
    push(&["-g", &gop, "-keyint_min", &gop]);
    push(&[
        "-force_key_frames",
        &format!("expr:gte(t,n_forced*{segment})"),
    ]);
    if info.hdr {
        // Tag the tone-mapped output as SDR BT.709.
        push(&[
            "-color_primaries",
            "bt709",
            "-color_trc",
            "bt709",
            "-colorspace",
            "bt709",
        ]);
    }
    // AAC stereo, which every client decodes.
    push(&["-c:a", "aac", "-ac", "2", "-b:a", &format!("{audio_kbps}k")]);
    // Shift timestamps back to the source timeline, and write each
    // fragment's absolute decode time (frag_discont) rather than one
    // relative to this job, so a seek-started job's segments line up.
    push(&["-output_ts_offset", &start_seconds.to_string()]);
    push(&["-f", "hls", "-hls_segment_type", "fmp4"]);
    push(&["-hls_segment_options", "movflags=+frag_discont"]);
    push(&["-hls_time", &segment.to_string()]);
    // temp_file: segments are written under a temporary name and renamed
    // when complete, so a segment that exists is complete.
    push(&["-hls_flags", "temp_file+independent_segments"]);
    push(&["-hls_fmp4_init_filename", "init.mp4"]);
    push(&["-hls_list_size", "0", "-hls_playlist_type", "vod"]);
    push(&["-start_number", &start_segment.to_string()]);
    // %d in the folder name would be read as the segment number.
    let mut pattern = OsString::from(out_dir.to_string_lossy().replace('%', "%%"));
    pattern.push(std::path::MAIN_SEPARATOR_STR);
    pattern.push("%d.m4s");
    args.push("-hls_segment_filename".into());
    args.push(pattern);
    // FFmpeg's own playlist; the server serves one computed from the
    // duration instead.
    args.push(out_dir.join("ffmpeg.m3u8").into_os_string());
    args
}

/// Converts subtitle stream `track` (the index among subtitle streams) to
/// WebVTT.
pub async fn extract_webvtt(encoder: &Encoder, source: &Path, track: u32) -> Result<Vec<u8>> {
    let mut args: Vec<OsString> = ["-nostdin", "-hide_banner", "-loglevel", "error", "-i"]
        .map(OsString::from)
        .into();
    args.push(file_url(source));
    args.extend(
        [
            "-map",
            &format!("0:s:{track}"),
            "-c:s",
            "webvtt",
            "-f",
            "webvtt",
            "-",
        ]
        .map(OsString::from),
    );
    // Read through a bound rather than buffered whole, so a runaway
    // conversion stops at the limit instead of filling memory.
    run(&encoder.ffmpeg, args, EXTRACT_TIMEOUT, MAX_WEBVTT_BYTES)
        .await
        .with_context(|| format!("Could not extract subtitle track {track}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encoder(name: &str) -> Encoder {
        Encoder {
            ffmpeg: "/bin/ffmpeg".into(),
            ffprobe: "/bin/ffprobe".into(),
            video_encoder: name.into(),
        }
    }

    fn strings(args: &[OsString]) -> Vec<String> {
        args.iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    fn value_after<'a>(args: &'a [String], flag: &str) -> &'a str {
        let at = args
            .iter()
            .position(|a| a == flag)
            .unwrap_or_else(|| panic!("{flag} missing from {args:?}"));
        &args[at + 1]
    }

    const LISTING: &str = "Encoders:
 V..... = Video
 A..... = Audio
 ------
 V....D libx264              libx264 H.264 / AVC / MPEG-4 AVC / MPEG-4 part 10 (codec h264)
 V....D libx264rgb           libx264 H.264 RGB (codec h264)
 V....D h264_videotoolbox    VideoToolbox H.264 Encoder (codec h264)
 V....D h264_nvenc           NVIDIA NVENC H.264 encoder (codec h264)
 A....D aac                  AAC (Advanced Audio Coding)
 S..... webvtt               WebVTT subtitle
";

    /// A program that writes without end is stopped at the limit instead of
    /// filling memory, and its error output is kept only to the end.
    #[cfg(unix)]
    #[tokio::test]
    async fn output_is_read_through_a_bound() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("endless");
        std::fs::write(
            &script,
            "#!/bin/sh\ncase \"$1\" in\n  out) exec yes ;;\n  err) yes >&2 & sleep 0.3; kill $!; echo 'last line' >&2; exit 3 ;;\n  ok) printf 'small' ;;\nesac\n",
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let limit = Duration::from_secs(20);
        let started = std::time::Instant::now();
        let error = run(&script, [OsString::from("out")], limit, 1024)
            .await
            .unwrap_err();
        assert!(
            format!("{error:#}").contains("wrote more than 1024 bytes"),
            "{error:#}"
        );
        assert!(started.elapsed() < Duration::from_secs(10));
        let error = run(&script, [OsString::from("err")], limit, 1024)
            .await
            .unwrap_err();
        assert!(format!("{error:#}").ends_with("last line"), "{error:#}");
        let output = run(&script, [OsString::from("ok")], limit, 1024)
            .await
            .unwrap();
        assert_eq!(output, b"small");
    }

    #[test]
    fn encoders_are_chosen_hardware_first() {
        assert_eq!(
            listed_encoders(LISTING),
            ["h264_videotoolbox", "h264_nvenc", "libx264"]
        );
        assert_eq!(choose_encoder(LISTING), Some("h264_videotoolbox"));
        let linux = LISTING.replace("h264_videotoolbox", "hevc_videotoolbox");
        assert_eq!(choose_encoder(&linux), Some("h264_nvenc"));
        let qsv = " V....D h264_qsv  QSV\n V....D libx264 x264\n";
        assert_eq!(choose_encoder(qsv), Some("h264_qsv"));
        assert_eq!(choose_encoder(" V....D libx264 x264\n"), Some("libx264"));
        // Audio-only or names in the legend do not count.
        assert_eq!(choose_encoder(" A....D aac AAC\n V..... = Video\n"), None);
        assert_eq!(choose_encoder(""), None);
    }

    #[test]
    fn job_args_follow_each_tier() {
        let info = MediaInfo {
            duration: 600.0,
            width: 3840,
            height: 2160,
            frame_rate: 24000.0 / 1001.0,
            ..Default::default()
        };
        let expected = [
            (
                Quality::High1080,
                "1920",
                "1080",
                "12000k",
                "24000k",
                "192k",
            ),
            (
                Quality::Standard1080,
                "1920",
                "1080",
                "6000k",
                "12000k",
                "192k",
            ),
            (Quality::Low720, "1280", "720", "3000k", "6000k", "128k"),
            (Quality::Data480, "854", "480", "1500k", "3000k", "128k"),
        ];
        for (quality, width, height, rate, buffer, audio) in expected {
            let args = strings(&job_args(
                &encoder("h264_videotoolbox"),
                Path::new("/media/film.mkv"),
                &info,
                quality,
                7,
                Path::new("/tmp/job"),
            ));
            let filters = value_after(&args, "-vf");
            assert!(
                filters.starts_with(&format!(
                    "scale=w=min(iw\\,{width}):h=min(ih\\,{height}):force_original_aspect_ratio=decrease:force_divisible_by=2"
                )),
                "{filters}"
            );
            assert!(filters.ends_with(",format=yuv420p"), "{filters}");
            assert!(!filters.contains("tonemap"), "SDR is not tone-mapped");
            assert!(!args.iter().any(|a| a == "-color_trc"));
            assert_eq!(value_after(&args, "-b:v"), rate);
            assert_eq!(value_after(&args, "-maxrate"), rate);
            assert_eq!(value_after(&args, "-bufsize"), buffer);
            assert_eq!(value_after(&args, "-b:a"), audio);
            assert_eq!(value_after(&args, "-ac"), "2");
            assert_eq!(value_after(&args, "-c:a"), "aac");
            assert_eq!(value_after(&args, "-c:v"), "h264_videotoolbox");
            assert_eq!(value_after(&args, "-ss"), "28");
            assert_eq!(value_after(&args, "-output_ts_offset"), "28");
            assert_eq!(value_after(&args, "-start_number"), "7");
            assert_eq!(value_after(&args, "-g"), "96");
            assert_eq!(value_after(&args, "-keyint_min"), "96");
            assert_eq!(
                value_after(&args, "-force_key_frames"),
                "expr:gte(t,n_forced*4)"
            );
            assert_eq!(value_after(&args, "-hls_time"), "4");
            assert_eq!(value_after(&args, "-hls_segment_type"), "fmp4");
            assert_eq!(
                value_after(&args, "-hls_flags"),
                "temp_file+independent_segments"
            );
            assert_eq!(value_after(&args, "-hls_fmp4_init_filename"), "init.mp4");
            assert_eq!(value_after(&args, "-hls_playlist_type"), "vod");
            assert_eq!(
                value_after(&args, "-hls_segment_options"),
                "movflags=+frag_discont"
            );
            assert_eq!(
                value_after(&args, "-hls_segment_filename"),
                format!("/tmp/job{}%d.m4s", std::path::MAIN_SEPARATOR)
            );
            assert_eq!(value_after(&args, "-i"), "file:/media/film.mkv");
            assert_eq!(value_after(&args, "-f"), "hls");
            assert_eq!(
                &args[..4],
                ["-nostdin", "-hide_banner", "-loglevel", "error"]
            );
            // -ss is an input option, so it must come before -i.
            let seek = args.iter().position(|a| a == "-ss").unwrap();
            let input = args.iter().position(|a| a == "-i").unwrap();
            assert!(seek < input);
            let maps: Vec<_> = args
                .windows(2)
                .filter(|w| w[0] == "-map")
                .map(|w| w[1].as_str())
                .collect();
            assert_eq!(maps, ["0:V:0", "0:a:0?"]);
            assert!(args.last().unwrap().ends_with("ffmpeg.m3u8"));
        }
    }

    #[test]
    fn hdr_sources_are_tone_mapped_and_tagged_sdr() {
        let pq = MediaInfo {
            hdr: true,
            color_transfer: "smpte2084".into(),
            frame_rate: 0.0,
            ..Default::default()
        };
        let args = strings(&job_args(
            &encoder("libx264"),
            Path::new("/m/hdr.mkv"),
            &pq,
            Quality::Low720,
            0,
            Path::new("/tmp/job"),
        ));
        let filters = value_after(&args, "-vf");
        assert!(
            filters.contains("zscale=tin=smpte2084:min=bt2020nc:pin=bt2020"),
            "{filters}"
        );
        assert!(filters.contains("tonemap=tonemap=hable"), "{filters}");
        assert!(
            filters.ends_with("zscale=t=bt709:m=bt709:r=tv,format=yuv420p"),
            "{filters}"
        );
        // Scaling happens before the float conversion, which is costly at 4K.
        assert!(filters.find("scale=w").unwrap() < filters.find("zscale").unwrap());
        assert_eq!(value_after(&args, "-color_trc"), "bt709");
        assert_eq!(value_after(&args, "-colorspace"), "bt709");
        assert_eq!(value_after(&args, "-ss"), "0");
        assert_eq!(value_after(&args, "-start_number"), "0");
        // Unknown frame rate: a 4 s interval at 60 fps as the upper bound.
        assert_eq!(value_after(&args, "-g"), "240");
        assert_eq!(value_after(&args, "-sc_threshold"), "0");
        assert_eq!(value_after(&args, "-preset"), "veryfast");

        let hlg = MediaInfo {
            hdr: true,
            color_transfer: "arib-std-b67".into(),
            ..Default::default()
        };
        let args = strings(&job_args(
            &encoder("h264_qsv"),
            Path::new("/m/hlg.mkv"),
            &hlg,
            Quality::Data480,
            3,
            Path::new("/tmp/50%"),
        ));
        assert!(value_after(&args, "-vf").contains("tin=arib-std-b67"));
        assert!(value_after(&args, "-vf").ends_with(",format=nv12"));
        assert_eq!(
            value_after(&args, "-hls_segment_filename"),
            format!("/tmp/50%%{}%d.m4s", std::path::MAIN_SEPARATOR)
        );
    }

    #[test]
    fn probe_output_gives_duration_size_hdr_and_subtitles() {
        let json = r#"{
          "streams": [
            {"index":0,"codec_name":"mjpeg","codec_type":"video","width":600,"height":600,
             "disposition":{"attached_pic":1}},
            {"index":1,"codec_name":"hevc","codec_type":"video","width":3840,"height":2160,
             "avg_frame_rate":"24000/1001","color_transfer":"smpte2084","color_primaries":"bt2020",
             "disposition":{"attached_pic":0}},
            {"index":2,"codec_name":"eac3","codec_type":"audio"},
            {"index":3,"codec_name":"subrip","codec_type":"subtitle","tags":{"language":"eng","title":"English"}},
            {"index":4,"codec_name":"hdmv_pgs_subtitle","codec_type":"subtitle","tags":{"language":"fre"}},
            {"index":5,"codec_name":"ass","codec_type":"subtitle","tags":{"language":"und"}}
          ],
          "format": {"duration":"5423.125000"}
        }"#;
        let info = parse_probe(json).unwrap();
        assert_eq!((info.width, info.height), (3840, 2160));
        assert!((info.frame_rate - 23.976).abs() < 0.001);
        assert!((info.duration - 5423.125).abs() < 1e-9);
        assert!(info.hdr);
        assert_eq!(info.color_transfer, "smpte2084");
        assert_eq!(
            info.subtitles,
            [
                EmbeddedSubtitle {
                    track: 0,
                    language: Some("eng".into()),
                    title: Some("English".into()),
                    text: true
                },
                EmbeddedSubtitle {
                    track: 1,
                    language: Some("fre".into()),
                    title: None,
                    text: false
                },
                EmbeddedSubtitle {
                    track: 2,
                    language: None,
                    title: None,
                    text: true
                },
            ]
        );

        let sdr = r#"{"streams":[{"codec_type":"video","width":1280,"height":720,
            "r_frame_rate":"30/1","color_transfer":"bt709","color_primaries":"bt709","duration":"12.5"}],
            "format":{}}"#;
        let info = parse_probe(sdr).unwrap();
        assert!(!info.hdr);
        assert_eq!(info.frame_rate, 30.0);
        assert_eq!(info.duration, 12.5, "falls back to the stream duration");
        let hlg = r#"{"streams":[{"codec_type":"video","color_transfer":"arib-std-b67"}]}"#;
        assert!(parse_probe(hlg).unwrap().hdr);
        assert_eq!(parse_probe("{}").unwrap(), MediaInfo::default());
        assert!(parse_probe("not json").is_err());
    }

    #[test]
    fn text_and_bitmap_subtitle_codecs() {
        for codec in ["subrip", "ass", "ssa", "webvtt", "mov_text"] {
            assert!(is_text_subtitle(codec), "{codec}");
        }
        for codec in [
            "hdmv_pgs_subtitle",
            "dvd_subtitle",
            "dvb_subtitle",
            "eia_608",
        ] {
            assert!(!is_text_subtitle(codec), "{codec}");
        }
    }

    #[test]
    fn box_widths_match_the_tier_table() {
        assert_eq!(box_width(1080), 1920);
        assert_eq!(box_width(720), 1280);
        assert_eq!(box_width(480), 854);
    }

    #[cfg(unix)]
    #[test]
    fn location_uses_explicit_then_path_then_extra_folders() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let make = |dir: &Path, name: &str, mode: u32| {
            std::fs::create_dir_all(dir).unwrap();
            let path = dir.join(name);
            std::fs::write(&path, "").unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
            path
        };
        let (a, b, c) = (
            root.path().join("a"),
            root.path().join("b"),
            root.path().join("c"),
        );
        make(&a, "ffmpeg", 0o644); // not executable, skipped
        let b_ffmpeg = make(&b, "ffmpeg", 0o755);
        let c_ffprobe = make(&c, "ffprobe", 0o755);
        let path = std::env::join_paths([&a, &b]).unwrap();
        // ffprobe is not beside ffmpeg, so it is searched for.
        assert_eq!(
            locate(None, Some(&path), std::slice::from_ref(&c)),
            Some((b_ffmpeg.clone(), c_ffprobe.clone()))
        );
        let b_ffprobe = make(&b, "ffprobe", 0o755);
        assert_eq!(
            locate(None, Some(&path), std::slice::from_ref(&c)),
            Some((b_ffmpeg.clone(), b_ffprobe.clone()))
        );
        // Extra folders come after PATH.
        let c_ffmpeg = make(&c, "ffmpeg", 0o755);
        assert_eq!(
            locate(None, None, std::slice::from_ref(&c)),
            Some((c_ffmpeg.clone(), c_ffprobe.clone()))
        );
        // An explicit file or folder wins; one that is not usable gives None.
        assert_eq!(
            locate(Some(&c_ffmpeg), Some(&path), &[]),
            Some((c_ffmpeg.clone(), c_ffprobe.clone()))
        );
        assert_eq!(
            locate(Some(&c), Some(&path), &[]),
            Some((c_ffmpeg, c_ffprobe))
        );
        assert_eq!(locate(Some(&a.join("ffmpeg")), Some(&path), &[]), None);
        assert_eq!(
            locate(Some(&root.path().join("none")), Some(&path), &[]),
            None
        );
        // Relative PATH entries are ignored.
        assert_eq!(locate(None, Some(OsStr::new("relative")), &[]), None);
    }

    #[test]
    fn stderr_tail_keeps_the_last_line() {
        assert_eq!(
            stderr_tail(b"first\nNo such filter: 'zscale'\n\n"),
            "No such filter: 'zscale'"
        );
        assert_eq!(stderr_tail(b""), "no error output");
    }

    #[cfg(unix)]
    mod process {
        use super::*;
        use std::os::unix::fs::PermissionsExt;

        fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
            let path = dir.join(name);
            std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            path
        }

        #[tokio::test]
        async fn probe_skips_hardware_encoders_that_fail() {
            let root = tempfile::tempdir().unwrap();
            // Lists VideoToolbox and NVENC, but only x264 can encode here.
            let ffmpeg = script(
                root.path(),
                "ffmpeg",
                &format!(
                    r#"case "$*" in
  *-encoders*) printf '%s' '{LISTING}' ;;
  *h264_videotoolbox*|*h264_nvenc*) echo 'Device not found' >&2; exit 1 ;;
  *) exit 0 ;;
esac"#
                ),
            );
            script(root.path(), "ffprobe", "exit 0");
            let found = probe(Some(&ffmpeg)).await.unwrap();
            assert_eq!(found.video_encoder, "libx264");
            assert_eq!(found.ffprobe, root.path().join("ffprobe"));

            // A working VideoToolbox is preferred.
            script(
                root.path(),
                "ffmpeg",
                &format!(r#"case "$*" in *-encoders*) printf '%s' '{LISTING}' ;; esac; exit 0"#),
            );
            let found = probe(Some(root.path())).await.unwrap();
            assert_eq!(found.video_encoder, "h264_videotoolbox");

            // No H.264 encoder at all.
            script(root.path(), "ffmpeg", "echo ' A....D aac AAC'");
            assert_eq!(probe(Some(&ffmpeg)).await, None);
            // A configured path that does not exist.
            assert_eq!(probe(Some(&root.path().join("missing"))).await, None);
        }

        #[tokio::test]
        async fn inspect_and_extract_use_the_programs() {
            let root = tempfile::tempdir().unwrap();
            let args = root.path().join("args.txt");
            let ffprobe = script(
                root.path(),
                "ffprobe",
                &format!(
                    r#"printf '%s\n' "$@" > '{}'
echo '{{"streams":[{{"codec_type":"video","width":640,"height":360}},{{"codec_type":"subtitle","codec_name":"mov_text"}}],"format":{{"duration":"8.0"}}}}'"#,
                    args.display()
                ),
            );
            let ffmpeg = script(
                root.path(),
                "ffmpeg",
                r#"case "$*" in
  *0:s:0*) printf 'WEBVTT\n\n00:00.000 --> 00:01.000\nHello\n' ;;
  *) echo "Stream map '0:s:9' matches no streams." >&2; exit 1 ;;
esac"#,
            );
            let encoder = Encoder {
                ffmpeg,
                ffprobe,
                video_encoder: "libx264".into(),
            };
            let source = root.path().join("-odd: name.mp4");
            let info = inspect(&encoder, &source).await.unwrap();
            assert_eq!((info.width, info.height, info.duration), (640, 360, 8.0));
            assert!(info.subtitles[0].text);
            let recorded = std::fs::read_to_string(&args).unwrap();
            assert_eq!(
                recorded.lines().last().unwrap(),
                format!("file:{}", source.display())
            );
            assert!(recorded.contains("-show_streams"));

            let vtt = extract_webvtt(&encoder, &source, 0).await.unwrap();
            assert!(vtt.starts_with(b"WEBVTT"));
            let error = extract_webvtt(&encoder, &source, 9).await.unwrap_err();
            assert!(
                format!("{error:#}").contains("matches no streams"),
                "{error:#}"
            );
        }
    }
}
