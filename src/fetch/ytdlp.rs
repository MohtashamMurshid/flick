//! yt-dlp run as a separate process with an argument list, never a shell.
//!
//! Intermediate files go to a private folder inside the destination, so a
//! cancelled or failed download removes them by removing that folder.

use super::{Cancelled, FetchProgress, FetchStage, FetchUpdate};
use anyhow::{Context, Result, bail, ensure};
use std::{
    collections::VecDeque,
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
    process::{ExitStatus, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, BufReader},
    process::{Child, Command},
    sync::mpsc,
};
use tokio_util::sync::CancellationToken;

const PROGRESS: &str = "download:flick-progress %(progress.status)s %(progress.downloaded_bytes)s %(progress.total_bytes)s %(progress.total_bytes_estimate)s %(progress.speed)s %(progress.eta)s";
const POSTPROCESS: &str = "postprocess:flick-postprocess %(progress.status)s";
const TITLE: &str = "before_dl:flick-title %(title)j";
const FILE: &str = "after_move:flick-file %(filepath)j";
const OUTPUT: &str = "%(title).150B [%(id)s].%(ext)s";
/// Best quality, merging separate video and audio streams with FFmpeg.
const MERGED_FORMAT: &str = "bv*+ba/b";
/// Without FFmpeg, the best single file that has both video and audio.
const SINGLE_FORMAT: &str = "b";
const INSTALL: &str = "https://github.com/yt-dlp/yt-dlp#installation";

/// The programs used for page downloads.
#[derive(Clone, Debug)]
pub struct Tools {
    pub ytdlp: PathBuf,
    /// When present, yt-dlp may merge separate video and audio streams.
    pub ffmpeg: Option<PathBuf>,
}

impl Tools {
    /// Finds yt-dlp from `FLICK_YTDLP`, then `PATH`, then common install
    /// folders that desktop launches may leave off `PATH`. FFmpeg is looked
    /// up the same way, and beside yt-dlp.
    pub fn discover() -> Result<Self> {
        let path = std::env::var_os("PATH");
        let extra = extra_dirs();
        let ytdlp = find_program(
            std::env::var_os("FLICK_YTDLP"),
            "FLICK_YTDLP",
            path.as_deref(),
            &extra,
            "yt-dlp",
        )?;
        let mut beside = Vec::new();
        beside.extend(ytdlp.parent().map(Path::to_owned));
        beside.extend(extra);
        let ffmpeg = find_program(None, "", path.as_deref(), &beside, "ffmpeg").ok();
        Ok(Self { ytdlp, ffmpeg })
    }
}

/// An explicit override must name a file. Otherwise search `path`, then
/// `extra`, for an executable called `name` (plus `.exe` on Windows).
pub(crate) fn find_program(
    explicit: Option<OsString>,
    variable: &str,
    path: Option<&OsStr>,
    extra: &[PathBuf],
    name: &str,
) -> Result<PathBuf> {
    if let Some(explicit) = explicit.filter(|v| !v.is_empty()) {
        let explicit = PathBuf::from(explicit);
        ensure!(
            explicit.is_file(),
            "{variable} is set to {}, which is not a file.",
            explicit.display()
        );
        return Ok(explicit);
    }
    let file = format!("{name}{}", std::env::consts::EXE_SUFFIX);
    path.into_iter()
        .flat_map(std::env::split_paths)
        .chain(extra.iter().cloned())
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(&file))
        .find(|candidate| executable(candidate))
        .with_context(|| {
            if name == "yt-dlp" {
                format!(
                    "Downloading from this site needs yt-dlp. Install it ({INSTALL}) and put it on PATH, or set FLICK_YTDLP to its full path."
                )
            } else {
                format!("{name} was not found")
            }
        })
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

/// Install folders that apps opened from a desktop may not have on `PATH`.
fn extra_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if cfg!(target_os = "macos") {
        dirs.extend(["/opt/homebrew/bin", "/usr/local/bin", "/opt/local/bin"].map(PathBuf::from));
    } else if cfg!(unix) {
        dirs.extend(["/usr/local/bin", "/usr/bin", "/snap/bin"].map(PathBuf::from));
    }
    if let Some(base) = directories::BaseDirs::new() {
        if cfg!(windows) {
            dirs.push(
                base.data_local_dir()
                    .join("Microsoft")
                    .join("WinGet")
                    .join("Links"),
            );
        } else {
            dirs.push(base.home_dir().join(".local").join("bin"));
        }
    }
    dirs
}

/// The yt-dlp argument list. Options end with `--`, so the URL can never be
/// read as an option.
pub(crate) fn arguments(
    url: &reqwest::Url,
    home: &Path,
    temp: &Path,
    ffmpeg: Option<&Path>,
) -> Vec<OsString> {
    let mut args = [
        "--ignore-config",
        "--no-playlist",
        "--newline",
        // `--print` implies `--quiet`, which would hide progress.
        "--progress",
        "--no-simulate",
        "--no-overwrites",
        "--no-mtime",
        "--progress-template",
        PROGRESS,
        "--progress-template",
        POSTPROCESS,
        "--print",
        TITLE,
        "--print",
        FILE,
        "--output",
        OUTPUT,
    ]
    .map(OsString::from)
    .to_vec();
    for (kind, path) in [("home:", home), ("temp:", temp)] {
        let mut value = OsString::from(kind);
        value.push(path);
        args.extend(["--paths".into(), value]);
    }
    match ffmpeg {
        Some(ffmpeg) => args.extend([
            "--ffmpeg-location".into(),
            ffmpeg.into(),
            "--format".into(),
            MERGED_FORMAT.into(),
        ]),
        None => args.extend(["--format".into(), SINGLE_FORMAT.into()]),
    }
    args.extend(["--".into(), url.as_str().into()]);
    args
}

#[derive(Debug, PartialEq)]
pub(crate) enum Line {
    Title(String),
    Progress(FetchProgress),
    Processing,
    File(PathBuf),
    Other,
}

/// Reads one line produced by the templates in [`arguments`].
pub(crate) fn parse_line(line: &str) -> Line {
    let line = line.trim_end_matches(['\r', '\n']);
    if let Some(rest) = line.strip_prefix("flick-progress ") {
        let fields = rest.split(' ').collect::<Vec<_>>();
        let [status, done, total, estimate, speed, eta] = fields.as_slice() else {
            return Line::Other;
        };
        let number = |value: &str| {
            value
                .parse::<f64>()
                .ok()
                .filter(|v| v.is_finite() && *v >= 0.)
                .map(|v| v.round() as u64)
        };
        let Some(downloaded_bytes) = number(done) else {
            return Line::Other;
        };
        return Line::Progress(FetchProgress {
            stage: if *status == "finished" {
                FetchStage::Processing
            } else {
                FetchStage::Downloading
            },
            downloaded_bytes,
            total_bytes: number(total).or_else(|| number(estimate)),
            bytes_per_second: number(speed),
            eta_seconds: number(eta),
            ..FetchProgress::default()
        });
    }
    if line.starts_with("flick-postprocess ") {
        return Line::Processing;
    }
    if let Some(rest) = line.strip_prefix("flick-title ") {
        let title = serde_json::from_str::<String>(rest).unwrap_or_else(|_| rest.to_owned());
        let title = title.trim();
        return if title.is_empty() || title == "NA" {
            Line::Other
        } else {
            Line::Title(title.to_owned())
        };
    }
    if let Some(rest) = line.strip_prefix("flick-file ")
        && let Ok(path) = serde_json::from_str::<String>(rest)
        && !path.is_empty()
    {
        return Line::File(path.into());
    }
    Line::Other
}

/// Downloads the video at `url` into `folder`.
pub(crate) async fn download(
    tools: &Tools,
    url: &reqwest::Url,
    folder: &Path,
    cancel: &CancellationToken,
    report: &mut (impl FnMut(FetchUpdate) + Send),
) -> Result<Vec<PathBuf>> {
    tokio::fs::create_dir_all(folder)
        .await
        .with_context(|| format!("Could not create {}", folder.display()))?;
    let folder = folder
        .canonicalize()
        .with_context(|| format!("Could not open {}", folder.display()))?;
    let temp = private_dir(&folder).await?;
    let result = run(tools, url, &folder, &temp, cancel, report).await;
    remove_dir(&temp).await;
    result
}

async fn run(
    tools: &Tools,
    url: &reqwest::Url,
    folder: &Path,
    temp: &Path,
    cancel: &CancellationToken,
    report: &mut (impl FnMut(FetchUpdate) + Send),
) -> Result<Vec<PathBuf>> {
    let mut command = Command::new(&tools.ytdlp);
    command
        .args(arguments(url, folder, temp, tools.ffmpeg.as_deref()))
        .current_dir(folder)
        .env("PYTHONIOENCODING", "utf-8")
        .env("PYTHONUNBUFFERED", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    // Own process group, so cancelling also stops FFmpeg children.
    #[cfg(unix)]
    command.process_group(0);
    #[cfg(windows)]
    command.creation_flags(CREATE_NO_WINDOW);
    if cancel.is_cancelled() {
        return Err(Cancelled.into());
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("Could not start {}", tools.ytdlp.display()))?;
    let result = pump(&mut child, folder, tools.ffmpeg.is_none(), cancel, report).await;
    if result.is_err() {
        stop(&mut child).await;
    }
    result
}

async fn pump(
    child: &mut Child,
    folder: &Path,
    without_ffmpeg: bool,
    cancel: &CancellationToken,
    report: &mut (impl FnMut(FetchUpdate) + Send),
) -> Result<Vec<PathBuf>> {
    let (lines, mut output) = mpsc::channel(64);
    if let Some(stdout) = child.stdout.take() {
        tokio::spawn(forward(stdout, false, lines.clone()));
    }
    if let Some(stderr) = child.stderr.take() {
        tokio::spawn(forward(stderr, true, lines));
    }
    let mut saved = Vec::new();
    let mut errors = VecDeque::new();
    let mut last = FetchProgress::default();
    let mut reported = Instant::now() - Duration::from_secs(1);
    report(FetchUpdate::Progress(last));
    loop {
        let (from_stderr, line) = tokio::select! {
            _ = cancel.cancelled() => return Err(Cancelled.into()),
            line = output.recv() => match line { Some(line) => line, None => break },
        };
        match parse_line(&line) {
            Line::Title(title) => report(FetchUpdate::Title(title)),
            Line::Progress(progress) => {
                let changed = progress.stage != last.stage;
                last = progress;
                if changed || reported.elapsed() >= Duration::from_millis(250) {
                    reported = Instant::now();
                    report(FetchUpdate::Progress(progress));
                }
            }
            Line::Processing => {
                if last.stage != FetchStage::Processing {
                    last.stage = FetchStage::Processing;
                    last.bytes_per_second = None;
                    last.eta_seconds = None;
                    report(FetchUpdate::Progress(last));
                }
            }
            Line::File(path) => {
                let path = confined(folder, &path)?;
                if !saved.contains(&path) {
                    report(FetchUpdate::Saved(path.clone()));
                    saved.push(path);
                }
            }
            Line::Other if from_stderr && !line.trim().is_empty() => {
                if errors.len() == 12 {
                    errors.pop_front();
                }
                errors.push_back(line.chars().take(600).collect::<String>());
            }
            Line::Other => {}
        }
    }
    let status = tokio::select! {
        _ = cancel.cancelled() => return Err(Cancelled.into()),
        status = child.wait() => status.context("yt-dlp stopped unexpectedly")?,
    };
    if !status.success() {
        bail!(failure(errors.make_contiguous(), status, without_ffmpeg));
    }
    ensure!(!saved.is_empty(), "yt-dlp finished without saving a video");
    Ok(saved)
}

async fn forward(
    stream: impl AsyncRead + Unpin,
    from_stderr: bool,
    lines: mpsc::Sender<(bool, String)>,
) {
    let mut reader = BufReader::new(stream);
    let mut buffer = Vec::new();
    loop {
        buffer.clear();
        match reader.read_until(b'\n', &mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                let line = String::from_utf8_lossy(&buffer).trim_end().to_owned();
                if lines.send((from_stderr, line)).await.is_err() {
                    break;
                }
            }
        }
    }
}

/// The reported file must be inside the destination folder.
fn confined(folder: &Path, path: &Path) -> Result<PathBuf> {
    let resolved = path
        .canonicalize()
        .with_context(|| format!("yt-dlp reported a missing file: {}", path.display()))?;
    ensure!(
        resolved.starts_with(folder) && resolved.is_file(),
        "yt-dlp saved a file outside the download folder: {}",
        path.display()
    );
    Ok(resolved)
}

/// The last yt-dlp error, with a hint for common causes.
fn failure(errors: &[String], status: ExitStatus, without_ffmpeg: bool) -> String {
    let error = errors
        .iter()
        .rev()
        .find_map(|line| line.strip_prefix("ERROR: "))
        .or_else(|| errors.last().map(String::as_str))
        .map(str::trim);
    let Some(error) = error else {
        return format!("yt-dlp stopped ({status}) without an error message.");
    };
    let hint = if error.contains("Unsupported URL") {
        " This site is not supported by yt-dlp."
    } else if without_ffmpeg
        && (error.contains("ffmpeg") || error.contains("Requested format is not available"))
    {
        " Install FFmpeg so yt-dlp can combine separate video and audio."
    } else {
        ""
    };
    format!("yt-dlp: {error}{hint}")
}

static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

/// A new hidden folder for yt-dlp's partial files.
async fn private_dir(folder: &Path) -> Result<PathBuf> {
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or_default();
    for _ in 0..64 {
        let n = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
        let dir = folder.join(format!(
            ".flick-yt-dlp-{}-{:x}",
            std::process::id(),
            seed ^ n.rotate_left(32)
        ));
        match tokio::fs::create_dir(&dir).await {
            Ok(()) => return Ok(dir),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("Could not write to {}", folder.display()));
            }
        }
    }
    bail!(
        "Could not create a temporary folder in {}",
        folder.display()
    )
}

/// Removes a private folder, retrying while stopped processes release files.
async fn remove_dir(dir: &Path) {
    for attempt in 0..10 {
        match tokio::fs::remove_dir_all(dir).await {
            Ok(()) => return,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
            Err(error) if attempt == 9 => {
                tracing::warn!(%error, dir = %dir.display(), "Could not remove partial download");
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(200)).await,
        }
    }
}

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Stops yt-dlp and the processes it started, then waits for it to exit.
async fn stop(child: &mut Child) {
    let Some(pid) = child.id() else {
        return;
    };
    #[cfg(unix)]
    signal_group(pid, libc::SIGTERM);
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .creation_flags(CREATE_NO_WINDOW)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;
    }
    if tokio::time::timeout(Duration::from_secs(3), child.wait())
        .await
        .is_err()
    {
        let _ = child.kill().await;
    }
    // Children that outlived yt-dlp still hold its process group.
    #[cfg(unix)]
    signal_group(pid, libc::SIGKILL);
}

#[cfg(unix)]
fn signal_group(pid: u32, signal: libc::c_int) {
    if let Ok(group) = libc::pid_t::try_from(pid) {
        // SAFETY: kill has no memory-safety preconditions. A negative PID
        // addresses the process group created for this child.
        unsafe {
            libc::kill(-group, signal);
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn progress_title_and_file_lines_parse() {
        assert_eq!(
            parse_line("flick-progress downloading 3072 4747984 NA 1799316.0016757436 2"),
            Line::Progress(FetchProgress {
                stage: FetchStage::Downloading,
                downloaded_bytes: 3072,
                total_bytes: Some(4_747_984),
                bytes_per_second: Some(1_799_316),
                eta_seconds: Some(2),
                ..Default::default()
            })
        );
        let Line::Progress(estimated) =
            parse_line("flick-progress downloading 1024 NA 2048.5 NA NA\r")
        else {
            panic!("expected progress");
        };
        assert_eq!(estimated.total_bytes, Some(2049));
        assert_eq!(estimated.bytes_per_second, None);
        let Line::Progress(finished) =
            parse_line("flick-progress finished 4747984 4747984 NA 41406136.2 NA")
        else {
            panic!("expected progress");
        };
        assert_eq!(finished.stage, FetchStage::Processing);
        assert_eq!(
            parse_line("flick-postprocess started Merger"),
            Line::Processing
        );
        assert_eq!(
            parse_line(r#"flick-title "Café \"live\" 🎬""#),
            Line::Title("Café \"live\" 🎬".into())
        );
        assert_eq!(parse_line("flick-title NA"), Line::Other);
        assert_eq!(
            parse_line(r#"flick-file "/tmp/out/clip [abc].mp4""#),
            Line::File("/tmp/out/clip [abc].mp4".into())
        );
        for other in [
            "flick-progress downloading 1 2",
            "flick-progress downloading NA NA NA NA NA",
            "flick-progress downloading -5 10 NA NA NA",
            "flick-file not-json",
            "[download] 10% of 1MiB",
            "",
        ] {
            assert_eq!(parse_line(other), Line::Other, "{other}");
        }
    }

    #[test]
    fn arguments_end_options_before_the_url() {
        let url = reqwest::Url::parse("https://example.com/watch?v=-rf").unwrap();
        let home = Path::new("/downloads/Flick");
        let temp = Path::new("/downloads/Flick/.flick-yt-dlp-1");
        let args = arguments(&url, home, temp, None);
        let text = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(text.last().unwrap(), url.as_str());
        assert_eq!(text[text.len() - 2], "--");
        assert_eq!(text.iter().filter(|a| *a == "--").count(), 1);
        for flag in [
            "--ignore-config",
            "--no-playlist",
            "--newline",
            "--progress",
            "--no-overwrites",
        ] {
            assert!(text.iter().any(|a| a == flag), "{flag}");
        }
        assert!(text.contains(&"home:/downloads/Flick".to_owned()));
        assert!(text.contains(&"temp:/downloads/Flick/.flick-yt-dlp-1".to_owned()));
        let format = text.iter().position(|a| a == "--format").unwrap();
        assert_eq!(text[format + 1], SINGLE_FORMAT);
        assert!(!text.iter().any(|a| a == "--ffmpeg-location"));

        let merged = arguments(&url, home, temp, Some(Path::new("/opt/ffmpeg")))
            .into_iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        let location = merged
            .iter()
            .position(|a| a == "--ffmpeg-location")
            .unwrap();
        assert_eq!(merged[location + 1], "/opt/ffmpeg");
        let format = merged.iter().position(|a| a == "--format").unwrap();
        assert_eq!(merged[format + 1], MERGED_FORMAT);
        assert!(location < merged.len() - 2 && format < merged.len() - 2);
    }

    #[test]
    fn failures_keep_the_yt_dlp_error_and_add_hints() {
        #[cfg(unix)]
        let status = {
            use std::os::unix::process::ExitStatusExt;
            ExitStatus::from_raw(1 << 8)
        };
        #[cfg(windows)]
        let status = {
            use std::os::windows::process::ExitStatusExt;
            ExitStatus::from_raw(1)
        };
        let lines = [
            "WARNING: falling back".to_owned(),
            "ERROR: Unsupported URL: https://a.example/".to_owned(),
        ];
        let message = failure(&lines, status, false);
        assert!(message.starts_with("yt-dlp: Unsupported URL"));
        assert!(message.ends_with("not supported by yt-dlp."));
        let missing = ["ERROR: [youtube] x: Requested format is not available".to_owned()];
        assert!(failure(&missing, status, true).contains("Install FFmpeg"));
        assert!(!failure(&missing, status, false).contains("Install FFmpeg"));
        assert!(failure(&[], status, false).contains("without an error message"));
    }

    #[test]
    fn discovery_uses_the_override_then_path_then_extra_folders() {
        let root = tempfile::tempdir().unwrap();
        let (first, second) = (root.path().join("first"), root.path().join("second"));
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        let name = format!("yt-dlp{}", std::env::consts::EXE_SUFFIX);
        let program = second.join(&name);
        std::fs::write(&program, "").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
            // Not executable, so skipped.
            std::fs::write(first.join(&name), "").unwrap();
        }
        let path = std::env::join_paths([&first, &second]).unwrap();
        let find = |explicit: Option<&Path>, path: Option<&OsStr>, extra: &[PathBuf]| {
            find_program(
                explicit.map(Into::into),
                "FLICK_YTDLP",
                path,
                extra,
                "yt-dlp",
            )
        };
        assert_eq!(find(None, Some(&path), &[]).unwrap(), program);
        assert_eq!(
            find(None, None, std::slice::from_ref(&second)).unwrap(),
            program
        );
        assert_eq!(
            find(Some(&program), Some(OsStr::new("")), &[]).unwrap(),
            program
        );
        let missing = find(Some(&root.path().join("nope")), Some(&path), &[]).unwrap_err();
        assert!(missing.to_string().contains("FLICK_YTDLP"));
        let absent = find(None, Some(OsStr::new("")), &[]).unwrap_err();
        assert!(absent.to_string().contains("needs yt-dlp"));
        // Relative PATH entries are ignored.
        assert!(find(None, Some(OsStr::new("relative/bin")), &[]).is_err());
    }

    /// The installed yt-dlp against a loopback server only: cancel a slow
    /// download, then complete one. Run with `-- --ignored`.
    #[tokio::test]
    #[ignore = "requires an installed yt-dlp; uses a loopback server, not the Internet"]
    async fn installed_ytdlp_cancels_and_completes_on_loopback() -> Result<()> {
        use crate::fetch::http::tests::{Behavior, content, serve};
        let tools = Tools::discover()?;
        let body = content(4 * 1024 * 1024);
        let video = Behavior {
            content_type: Some("video/mp4"),
            ..Default::default()
        };
        let root = tempfile::tempdir()?;
        let folder = root.path().join("downloads");

        let (origin, _, slow) = serve(
            body.clone(),
            Behavior {
                delay: Some(Duration::from_millis(50)),
                ..video.clone()
            },
        )
        .await;
        let url = reqwest::Url::parse(&format!("{origin}/media/clip.mp4"))?;
        let cancel = CancellationToken::new();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let job = {
            let (tools, cancel, folder, url) =
                (tools.clone(), cancel.clone(), folder.clone(), url.clone());
            tokio::spawn(async move {
                download(&tools, &url, &folder, &cancel, &mut |u| {
                    if let FetchUpdate::Progress(p) = u
                        && p.downloaded_bytes > 0
                    {
                        let _ = tx.send(p);
                    }
                })
                .await
            })
        };
        let progress = tokio::time::timeout(Duration::from_secs(30), rx.recv())
            .await?
            .context("yt-dlp reported no progress")?;
        assert_eq!(progress.total_bytes, Some(body.len() as u64));
        cancel.cancel();
        let error = tokio::time::timeout(Duration::from_secs(10), job)
            .await??
            .unwrap_err();
        slow.abort();
        assert!(error.is::<Cancelled>(), "{error:#}");
        assert_eq!(std::fs::read_dir(&folder)?.count(), 0);

        let (origin, _, fast) = serve(body.clone(), video).await;
        let url = reqwest::Url::parse(&format!("{origin}/media/clip.mp4"))?;
        let mut titles = Vec::new();
        let saved = download(&tools, &url, &folder, &CancellationToken::new(), &mut |u| {
            if let FetchUpdate::Title(title) = u {
                titles.push(title);
            }
        })
        .await?;
        fast.abort();
        assert_eq!(titles, ["clip"]);
        assert_eq!(saved.len(), 1);
        assert!(saved[0].starts_with(folder.canonicalize()?));
        assert_eq!(std::fs::read(&saved[0])?, *body);
        assert_eq!(std::fs::read_dir(&folder)?.count(), 1);
        Ok(())
    }

    #[cfg(unix)]
    pub(crate) mod process {
        use super::*;
        use std::os::unix::fs::PermissionsExt;

        /// Writes a stand-in for yt-dlp that records its arguments, reads the
        /// `--paths` values, and runs `body` with `$home`, `$temp`, and `$url`.
        pub(crate) fn stand_in(dir: &Path, body: &str) -> PathBuf {
            let script = dir.join("yt-dlp");
            let args = dir.join("args.txt");
            let text = format!(
                r#"#!/bin/sh
: > '{args}'
home=''; temp=''; url=''; after=''
for arg in "$@"; do
  printf '%s\n' "$arg" >> '{args}'
  if [ -n "$after" ]; then url="$arg"; fi
  case "$arg" in
    --) after=1 ;;
    home:*) home="${{arg#home:}}" ;;
    temp:*) temp="${{arg#temp:}}" ;;
  esac
done
{body}
"#,
                args = args.display()
            );
            std::fs::write(&script, text).unwrap();
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
            script
        }

        fn tools(program: PathBuf) -> Tools {
            Tools {
                ytdlp: program,
                ffmpeg: None,
            }
        }

        fn leftovers(folder: &Path) -> Vec<String> {
            std::fs::read_dir(folder)
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .filter(|n| n.starts_with(".flick-yt-dlp"))
                .collect()
        }

        #[tokio::test]
        async fn stand_in_download_reports_progress_and_the_saved_file() -> Result<()> {
            let root = tempfile::tempdir()?;
            let folder = root.path().join("downloads");
            let program = stand_in(
                root.path(),
                r#"echo 'flick-title "Stand-in clip"'
printf 'partial' > "$temp/clip [x1].mp4.part"
echo 'flick-progress downloading 512 1024 NA 2048.0 1'
echo 'flick-progress downloading 1024 1024 NA 2048.0 0'
echo 'flick-progress finished 1024 1024 NA NA NA'
echo 'flick-postprocess started MoveFiles' >&2
mv "$temp/clip [x1].mp4.part" "$home/clip [x1].mp4"
printf 'flick-file "%s"\n' "$home/clip [x1].mp4"
echo 'WARNING: this is only a stand-in' >&2"#,
            );
            let url = reqwest::Url::parse("https://example.com/watch?v=x1")?;
            let mut updates = Vec::new();
            let saved = download(
                &tools(program),
                &url,
                &folder,
                &CancellationToken::new(),
                &mut |u| updates.push(u),
            )
            .await?;
            let expected = folder.canonicalize()?.join("clip [x1].mp4");
            assert_eq!(saved, vec![expected.clone()]);
            assert_eq!(std::fs::read(&expected)?, b"partial");
            assert!(updates.contains(&FetchUpdate::Title("Stand-in clip".into())));
            assert!(updates.contains(&FetchUpdate::Saved(expected)));
            assert!(updates.iter().any(|u| matches!(
                u,
                FetchUpdate::Progress(FetchProgress {
                    stage: FetchStage::Downloading,
                    total_bytes: Some(1024),
                    ..
                })
            )));
            assert!(updates.iter().any(|u| matches!(
                u,
                FetchUpdate::Progress(FetchProgress {
                    stage: FetchStage::Processing,
                    ..
                })
            )));
            assert!(leftovers(&folder).is_empty());
            let args = std::fs::read_to_string(root.path().join("args.txt"))?;
            let args = args.lines().collect::<Vec<_>>();
            assert_eq!(args[args.len() - 2..], ["--", url.as_str()]);
            assert!(args.contains(&"--ignore-config"));
            Ok(())
        }

        #[tokio::test]
        async fn failures_report_the_error_and_remove_partial_files() -> Result<()> {
            let root = tempfile::tempdir()?;
            let folder = root.path().join("downloads");
            let program = stand_in(
                root.path(),
                r#"printf 'partial' > "$temp/clip.mp4.part"
echo 'ERROR: [generic] Unsupported URL: '"$url" >&2
exit 1"#,
            );
            let url = reqwest::Url::parse("https://example.com/page")?;
            let error = download(
                &tools(program),
                &url,
                &folder,
                &CancellationToken::new(),
                &mut |_| {},
            )
            .await
            .unwrap_err()
            .to_string();
            assert!(
                error.contains("Unsupported URL: https://example.com/page"),
                "{error}"
            );
            assert!(error.contains("not supported by yt-dlp"));
            assert!(leftovers(&folder).is_empty());
            assert_eq!(std::fs::read_dir(&folder)?.count(), 0);

            let silent = stand_in(root.path(), "exit 0");
            let error = download(
                &tools(silent),
                &url,
                &folder,
                &CancellationToken::new(),
                &mut |_| {},
            )
            .await
            .unwrap_err();
            assert!(error.to_string().contains("without saving"));

            let outside = root.path().join("outside.mp4");
            std::fs::write(&outside, b"x")?;
            let escaping = stand_in(
                root.path(),
                &format!("echo 'flick-file \"{}\"'", outside.display()),
            );
            let error = download(
                &tools(escaping),
                &url,
                &folder,
                &CancellationToken::new(),
                &mut |_| {},
            )
            .await
            .unwrap_err();
            assert!(error.to_string().contains("outside the download folder"));
            Ok(())
        }

        #[tokio::test]
        async fn a_missing_program_fails_to_start() {
            let root = tempfile::tempdir().unwrap();
            let url = reqwest::Url::parse("https://example.com/page").unwrap();
            let error = download(
                &tools(root.path().join("missing-yt-dlp")),
                &url,
                root.path(),
                &CancellationToken::new(),
                &mut |_| {},
            )
            .await
            .unwrap_err();
            assert!(error.to_string().contains("Could not start"));
            assert!(leftovers(root.path()).is_empty());
        }

        #[tokio::test]
        async fn cancelling_stops_the_process_group_and_removes_partial_files() -> Result<()> {
            let root = tempfile::tempdir()?;
            let folder = root.path().join("downloads");
            let survived = root.path().join("survived");
            let program = stand_in(
                root.path(),
                &format!(
                    r#"printf 'partial' > "$temp/clip.mp4.part"
(sleep 2; touch '{survived}') &
echo 'flick-progress downloading 7 100 NA NA NA'
sleep 30
touch '{survived}'"#,
                    survived = survived.display()
                ),
            );
            let url = reqwest::Url::parse("https://example.com/page")?;
            let cancel = CancellationToken::new();
            let (progress, mut started) = tokio::sync::mpsc::unbounded_channel();
            let job = {
                let cancel = cancel.clone();
                let folder = folder.clone();
                tokio::spawn(async move {
                    download(&tools(program), &url, &folder, &cancel, &mut |u| {
                        if let FetchUpdate::Progress(p) = u
                            && p.downloaded_bytes == 7
                        {
                            let _ = progress.send(());
                        }
                    })
                    .await
                })
            };
            tokio::time::timeout(Duration::from_secs(10), started.recv())
                .await?
                .context("stand-in did not report progress")?;
            assert_eq!(leftovers(&folder).len(), 1);
            let stopped = Instant::now();
            cancel.cancel();
            let error = tokio::time::timeout(Duration::from_secs(10), job)
                .await??
                .unwrap_err();
            assert!(error.is::<Cancelled>());
            assert!(stopped.elapsed() < Duration::from_secs(5));
            assert!(leftovers(&folder).is_empty());
            assert_eq!(std::fs::read_dir(&folder)?.count(), 0);
            tokio::time::sleep(Duration::from_millis(2500)).await;
            assert!(
                !survived.exists(),
                "a stand-in process outlived cancellation"
            );
            Ok(())
        }
    }
}
