//! Direct HTTPS downloads. Each file streams into a hidden, locked `.part`
//! file that resumes with a Range request after interruptions, is checked
//! against its published size and SHA-1, then gets a free name in the
//! destination. Existing files are never overwritten.

use super::{Cancelled, FetchProgress, FetchStage, FetchUpdate, RemoteFile, numbered};
use anyhow::{Context, Result, bail, ensure};
use fs2::FileExt;
use reqwest::{StatusCode, header};
use sha1::{Digest, Sha1};
use std::{
    io::{Read, SeekFrom},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use tokio::io::{AsyncSeekExt, AsyncWriteExt, BufWriter};
use tokio_util::sync::CancellationToken;

/// The address answered with a web page rather than a file.
#[derive(Debug)]
pub(crate) struct WebPage;
impl std::fmt::Display for WebPage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("This address opens a web page, not a video file")
    }
}
impl std::error::Error for WebPage {}

/// Consecutive failed attempts without progress before giving up.
const ATTEMPTS: u32 = 5;
const REPORT_INTERVAL: Duration = Duration::from_millis(250);

pub(crate) fn client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .https_only(true)
        .connect_timeout(Duration::from_secs(15))
        .read_timeout(Duration::from_secs(30))
        .user_agent(concat!("Flick/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("Could not prepare HTTPS downloads")
}

/// Downloads each file in order into `folder`.
pub(crate) async fn download_all(
    client: &reqwest::Client,
    files: &[RemoteFile],
    folder: &Path,
    cancel: &CancellationToken,
    report: &mut (impl FnMut(FetchUpdate) + Send),
) -> Result<Vec<PathBuf>> {
    ensure!(!files.is_empty(), "Choose at least one file to download");
    tokio::fs::create_dir_all(folder)
        .await
        .with_context(|| format!("Could not create {}", folder.display()))?;
    let mut saved = Vec::new();
    for (index, file) in files.iter().enumerate() {
        let path = download_file(client, file, folder, cancel, &mut |mut progress| {
            progress.file_index = index + 1;
            progress.file_count = files.len();
            report(FetchUpdate::Progress(progress));
        })
        .await
        .with_context(|| format!("Could not download {}", file.name))?;
        report(FetchUpdate::Saved(path.clone()));
        saved.push(path);
    }
    Ok(saved)
}

pub(crate) async fn download_file(
    client: &reqwest::Client,
    file: &RemoteFile,
    folder: &Path,
    cancel: &CancellationToken,
    progress: &mut (impl FnMut(FetchProgress) + Send),
) -> Result<PathBuf> {
    let name = super::safe_file_name(&file.name).context("This file has no usable name")?;
    let sha1 = file
        .sha1
        .as_deref()
        .map(str::to_ascii_lowercase)
        .filter(|h| h.len() == 40 && h.bytes().all(|b| b.is_ascii_hexdigit()));
    progress(FetchProgress::default());
    if let (Some(size), Some(sha1)) = (file.size, &sha1)
        && let Some(path) = existing_copy(folder, &name, size, sha1).await?
    {
        return Ok(path);
    }
    let path = part_path(folder, &name, &file.url);
    let mut part = Part::open(&path, sha1.is_some()).await?;
    let result = transfer(client, file, &mut part, cancel, progress).await;
    let result = match result {
        Ok(total) => part.finish(total, sha1.as_deref(), progress).await,
        Err(error) => Err(error),
    };
    drop(part);
    match result {
        Ok(()) => place(&path, folder, &name),
        Err(error) => {
            let _ = std::fs::remove_file(&path);
            Err(error)
        }
    }
}

/// A hidden part file, unique to the source URL, so resuming never mixes
/// bytes from different sources.
pub(crate) fn part_path(folder: &Path, name: &str, url: &str) -> PathBuf {
    let fingerprint = url.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    });
    folder.join(format!(".{name}.{fingerprint:016x}.flick.part"))
}

struct Part {
    file: BufWriter<tokio::fs::File>,
    len: u64,
    hasher: Option<Sha1>,
}

impl Part {
    /// Opens and locks the part file, keeping existing bytes to resume.
    async fn open(path: &Path, hash: bool) -> Result<Self> {
        let path = path.to_owned();
        let (file, len, hasher) = tokio::task::spawn_blocking(move || -> Result<_> {
            let mut file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&path)
                .with_context(|| format!("Could not write {}", path.display()))?;
            if file.try_lock_exclusive().is_err() {
                bail!("This file is already downloading");
            }
            // Earlier bytes count toward the checksum when resuming.
            let mut hasher = hash.then(Sha1::new);
            let mut len = 0;
            let mut buffer = vec![0; 256 * 1024];
            loop {
                let read = file.read(&mut buffer)?;
                if read == 0 {
                    break;
                }
                if let Some(hasher) = &mut hasher {
                    hasher.update(&buffer[..read]);
                }
                len += read as u64;
            }
            Ok((file, len, hasher))
        })
        .await
        .context("Could not open the partial download")??;
        let mut file = tokio::fs::File::from_std(file);
        file.seek(SeekFrom::Start(len)).await?;
        Ok(Self {
            file: BufWriter::with_capacity(256 * 1024, file),
            len,
            hasher,
        })
    }

    async fn write(&mut self, bytes: &[u8]) -> Result<()> {
        self.file
            .write_all(bytes)
            .await
            .context("Could not write the download")?;
        if let Some(hasher) = &mut self.hasher {
            hasher.update(bytes);
        }
        self.len += bytes.len() as u64;
        Ok(())
    }

    /// Discards existing bytes when the server sends the whole file.
    async fn restart(&mut self) -> Result<()> {
        self.file.flush().await?;
        self.file.get_mut().set_len(0).await?;
        self.file.get_mut().seek(SeekFrom::Start(0)).await?;
        self.len = 0;
        if self.hasher.is_some() {
            self.hasher = Some(Sha1::new());
        }
        Ok(())
    }

    async fn finish(
        &mut self,
        total: Option<u64>,
        sha1: Option<&str>,
        progress: &mut (impl FnMut(FetchProgress) + Send),
    ) -> Result<()> {
        self.file.flush().await?;
        self.file.get_mut().sync_all().await?;
        if let Some(total) = total {
            ensure!(
                self.len == total,
                "The download is {} bytes; {total} were expected",
                self.len
            );
        }
        if let (Some(expected), Some(hasher)) = (sha1, self.hasher.take()) {
            progress(FetchProgress {
                stage: FetchStage::Verifying,
                downloaded_bytes: self.len,
                total_bytes: Some(self.len),
                ..FetchProgress::default()
            });
            let actual = hasher
                .finalize()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>();
            ensure!(
                actual == expected,
                "The downloaded file does not match its published checksum"
            );
        }
        Ok(())
    }
}

/// Streams the remote file into `part`, resuming after dropped connections.
/// Returns the expected size when the server or source knows it.
async fn transfer(
    client: &reqwest::Client,
    file: &RemoteFile,
    part: &mut Part,
    cancel: &CancellationToken,
    progress: &mut (impl FnMut(FetchProgress) + Send),
) -> Result<Option<u64>> {
    let mut total = file.size;
    let mut failures = 0;
    let mut validator: Option<header::HeaderValue> = None;
    let mut meter = Meter::new(part.len);
    'attempt: loop {
        let offset = part.len;
        if offset > 0 && total == Some(offset) {
            return Ok(total);
        }
        let mut request = client.get(&file.url);
        if offset > 0 {
            request = request.header(header::RANGE, format!("bytes={offset}-"));
            if let Some(validator) = &validator {
                request = request.header(header::IF_RANGE, validator.clone());
            }
        }
        let response = tokio::select! {
            _ = cancel.cancelled() => return Err(Cancelled.into()),
            response = request.send() => response,
        };
        let response = match response {
            Ok(response) => response,
            Err(error) => {
                failures += 1;
                if failures >= ATTEMPTS {
                    return Err(error).context("The server could not be reached");
                }
                backoff(failures, cancel).await?;
                continue;
            }
        };
        let status = response.status();
        match status {
            StatusCode::PARTIAL_CONTENT if offset > 0 => match content_range(response.headers()) {
                Some((start, size)) if start == offset => total = size.or(total),
                _ => {
                    part.restart().await?;
                    continue;
                }
            },
            StatusCode::OK => {
                if offset > 0 {
                    part.restart().await?;
                }
                total = response.content_length().or(total);
            }
            StatusCode::RANGE_NOT_SATISFIABLE if offset > 0 => {
                if content_range(response.headers()).and_then(|(_, size)| size) == Some(offset) {
                    return Ok(Some(offset));
                }
                part.restart().await?;
                continue;
            }
            status
                if status.is_server_error()
                    || matches!(
                        status,
                        StatusCode::TOO_MANY_REQUESTS | StatusCode::REQUEST_TIMEOUT
                    ) =>
            {
                failures += 1;
                if failures >= ATTEMPTS {
                    bail!("The server is not responding ({status})");
                }
                backoff(failures, cancel).await?;
                continue;
            }
            status => bail!("The server refused the download ({status})"),
        }
        let kind = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_ascii_lowercase();
        if kind.starts_with("text/html") || kind.starts_with("application/xhtml") {
            return Err(WebPage.into());
        }
        if validator.is_none() {
            validator = response
                .headers()
                .get(header::ETAG)
                .filter(|v| !v.as_bytes().starts_with(b"W/"))
                .or_else(|| response.headers().get(header::LAST_MODIFIED))
                .cloned();
        }
        let mut response = response;
        let started = part.len;
        loop {
            let chunk = tokio::select! {
                _ = cancel.cancelled() => return Err(Cancelled.into()),
                chunk = response.chunk() => chunk,
            };
            match chunk {
                Ok(Some(bytes)) => {
                    part.write(&bytes).await?;
                    if let Some(total) = total {
                        ensure!(part.len <= total, "The server sent more data than expected");
                    }
                    meter.update(part.len, total, progress);
                }
                Ok(None) if total.is_none_or(|total| part.len == total) => {
                    meter.report(part.len, total, progress);
                    return Ok(total);
                }
                // The connection closed early, or failed mid-file.
                Ok(None) | Err(_) => {
                    part.file.flush().await?;
                    if part.len > started {
                        failures = 0;
                    }
                    failures += 1;
                    if failures >= ATTEMPTS {
                        bail!("The connection kept dropping");
                    }
                    backoff(failures, cancel).await?;
                    continue 'attempt;
                }
            }
        }
    }
}

async fn backoff(failures: u32, cancel: &CancellationToken) -> Result<()> {
    let delay = Duration::from_millis(500 << failures.min(5));
    tokio::select! {
        _ = cancel.cancelled() => Err(Cancelled.into()),
        _ = tokio::time::sleep(delay) => Ok(()),
    }
}

/// Parses `bytes start-end/size` or `bytes */size`.
fn content_range(headers: &header::HeaderMap) -> Option<(u64, Option<u64>)> {
    let value = headers.get(header::CONTENT_RANGE)?.to_str().ok()?;
    let (range, size) = value.strip_prefix("bytes ")?.split_once('/')?;
    let size = size.trim().parse().ok();
    let start = match range.trim() {
        "*" => 0,
        range => range.split_once('-')?.0.trim().parse().ok()?,
    };
    Some((start, size))
}

/// Reuses an earlier download of the same file, matched by size and SHA-1.
async fn existing_copy(
    folder: &Path,
    name: &str,
    size: u64,
    sha1: &str,
) -> Result<Option<PathBuf>> {
    let folder = folder.to_owned();
    let (name, sha1) = (name.to_owned(), sha1.to_owned());
    tokio::task::spawn_blocking(move || {
        for n in 0..1000 {
            let candidate = folder.join(numbered(&name, n));
            let Ok(metadata) = candidate.symlink_metadata() else {
                return Ok(None);
            };
            if metadata.is_file() && metadata.len() == size && hash_file(&candidate)? == sha1 {
                return Ok(Some(candidate));
            }
        }
        Ok(None)
    })
    .await
    .context("Could not check existing downloads")?
}

fn hash_file(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha1::new();
    let mut buffer = vec![0; 256 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// Gives the finished part file the first free name. A hard link fails
/// rather than replacing an existing file; filesystems without hard links
/// fall back to a checked rename.
fn place(part: &Path, folder: &Path, name: &str) -> Result<PathBuf> {
    for n in 0..1000 {
        let candidate = folder.join(numbered(name, n));
        match std::fs::hard_link(part, &candidate) {
            Ok(()) => {
                if let Err(error) = std::fs::remove_file(part) {
                    tracing::warn!(%error, "Could not remove a finished partial download");
                }
                return Ok(candidate);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(_) if candidate.symlink_metadata().is_ok() => {}
            Err(_) => {
                std::fs::rename(part, &candidate)
                    .with_context(|| format!("Could not save {}", candidate.display()))?;
                return Ok(candidate);
            }
        }
    }
    bail!("No free file name for {name}")
}

/// Rate-limited progress with a smoothed speed.
struct Meter {
    at: Instant,
    bytes: u64,
    speed: Option<f64>,
}

impl Meter {
    fn new(bytes: u64) -> Self {
        Self {
            at: Instant::now(),
            bytes,
            speed: None,
        }
    }

    fn update(
        &mut self,
        bytes: u64,
        total: Option<u64>,
        progress: &mut (impl FnMut(FetchProgress) + Send),
    ) {
        if self.at.elapsed() >= REPORT_INTERVAL {
            self.report(bytes, total, progress);
        }
    }

    fn report(
        &mut self,
        bytes: u64,
        total: Option<u64>,
        progress: &mut (impl FnMut(FetchProgress) + Send),
    ) {
        let elapsed = self.at.elapsed().as_secs_f64();
        if elapsed > 0. && bytes >= self.bytes {
            let now = (bytes - self.bytes) as f64 / elapsed;
            self.speed = Some(self.speed.map_or(now, |speed| speed * 0.7 + now * 0.3));
        }
        self.at = Instant::now();
        self.bytes = bytes;
        let speed = self.speed.filter(|s| *s >= 1.);
        progress(FetchProgress {
            stage: FetchStage::Downloading,
            downloaded_bytes: bytes,
            total_bytes: total,
            bytes_per_second: speed.map(|s| s as u64),
            eta_seconds: total
                .zip(speed)
                .map(|(total, speed)| (total.saturating_sub(bytes) as f64 / speed).ceil() as u64),
            ..FetchProgress::default()
        });
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tokio::io::AsyncReadExt;

    /// How the loopback server answers.
    #[derive(Clone, Default)]
    pub(crate) struct Behavior {
        pub ranges: bool,
        /// Close the first response after this many body bytes.
        pub cut_after: Option<usize>,
        pub delay: Option<Duration>,
        pub status: Option<u16>,
        pub content_type: Option<&'static str>,
    }

    /// A plain-HTTP loopback server for one byte string, recording request
    /// headers. Tests pass a client without `https_only`.
    pub(crate) async fn serve(
        body: Arc<Vec<u8>>,
        behavior: Behavior,
    ) -> (String, Arc<Mutex<Vec<String>>>, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let log = requests.clone();
        let server = tokio::spawn(async move {
            let mut served = 0;
            while let Ok((stream, _)) = listener.accept().await {
                served += 1;
                let first = served == 1;
                tokio::spawn(respond(
                    stream,
                    body.clone(),
                    behavior.clone(),
                    first,
                    log.clone(),
                ));
            }
        });
        (origin, requests, server)
    }

    async fn respond(
        mut stream: tokio::net::TcpStream,
        body: Arc<Vec<u8>>,
        behavior: Behavior,
        first: bool,
        log: Arc<Mutex<Vec<String>>>,
    ) {
        let mut head = Vec::new();
        let mut byte = [0; 1];
        while !head.ends_with(b"\r\n\r\n") {
            match stream.read(&mut byte).await {
                Ok(1) => head.push(byte[0]),
                _ => break,
            }
        }
        let head = String::from_utf8_lossy(&head).to_string();
        log.lock().unwrap().push(head.clone());
        let range = head
            .lines()
            .find_map(|l| {
                l.to_ascii_lowercase()
                    .strip_prefix("range: bytes=")
                    .map(str::to_owned)
            })
            .and_then(|r| r.trim_end_matches('-').parse::<usize>().ok());
        let mut response = String::new();
        let start = match (behavior.status, range) {
            (Some(code), _) => {
                response.push_str(&format!("HTTP/1.1 {code} Error\r\nContent-Length: 0\r\n"));
                body.len()
            }
            (None, Some(start)) if behavior.ranges && start >= body.len() => {
                response.push_str(&format!(
                    "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */{}\r\nContent-Length: 0\r\n",
                    body.len()
                ));
                body.len()
            }
            (None, Some(start)) if behavior.ranges => {
                response.push_str(&format!(
                    "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {start}-{}/{}\r\nContent-Length: {}\r\n",
                    body.len() - 1,
                    body.len(),
                    body.len() - start
                ));
                start
            }
            _ => {
                response.push_str(&format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n",
                    body.len()
                ));
                0
            }
        };
        if let Some(content_type) = behavior.content_type {
            response.push_str(&format!("Content-Type: {content_type}\r\n"));
        }
        response.push_str("ETag: \"v1\"\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n");
        if stream.write_all(response.as_bytes()).await.is_err() {
            return;
        }
        let end = match behavior.cut_after {
            Some(cut) if first => (start + cut).min(body.len()),
            _ => body.len(),
        };
        if head.starts_with("HEAD ") {
            return;
        }
        for chunk in body[start..end].chunks(16 * 1024) {
            if let Some(delay) = behavior.delay {
                tokio::time::sleep(delay).await;
            }
            if stream.write_all(chunk).await.is_err() {
                return;
            }
        }
        let _ = stream.shutdown().await;
    }

    pub(crate) fn content(len: usize) -> Arc<Vec<u8>> {
        Arc::new((0..len).map(|i| (i * 7 % 251) as u8).collect())
    }

    pub(crate) fn sha1_hex(bytes: &[u8]) -> String {
        Sha1::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    fn plain_client() -> reqwest::Client {
        reqwest::Client::builder().no_proxy().build().unwrap()
    }

    fn remote(origin: &str, body: &[u8], verified: bool) -> RemoteFile {
        RemoteFile {
            url: format!("{origin}/media/clip.mp4"),
            name: "nested/clip.mp4".into(),
            size: verified.then_some(body.len() as u64),
            sha1: verified.then(|| sha1_hex(body)),
        }
    }

    fn names(folder: &Path) -> Vec<String> {
        let mut names = std::fs::read_dir(folder)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        names.sort();
        names
    }

    #[tokio::test]
    async fn downloads_resume_after_the_connection_drops() -> Result<()> {
        let body = content(300 * 1024);
        let (origin, requests, server) = serve(
            body.clone(),
            Behavior {
                ranges: true,
                cut_after: Some(100 * 1024),
                ..Default::default()
            },
        )
        .await;
        let folder = tempfile::tempdir()?;
        let mut updates = Vec::new();
        let saved = download_all(
            &plain_client(),
            &[remote(&origin, &body, true)],
            folder.path(),
            &CancellationToken::new(),
            &mut |u| updates.push(u),
        )
        .await?;
        server.abort();
        let path = folder.path().join("clip.mp4");
        assert_eq!(saved, vec![path.clone()]);
        assert_eq!(std::fs::read(&path)?, *body);
        assert_eq!(names(folder.path()), ["clip.mp4"]);
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        let resumed = requests[1].to_ascii_lowercase();
        assert!(
            resumed.contains(&format!("range: bytes={}-", 100 * 1024)),
            "{resumed}"
        );
        assert!(resumed.contains("if-range: \"v1\""), "{resumed}");
        assert!(updates.contains(&FetchUpdate::Saved(path)));
        assert!(updates.iter().any(|u| matches!(
            u,
            FetchUpdate::Progress(FetchProgress {
                stage: FetchStage::Verifying,
                file_index: 1,
                file_count: 1,
                ..
            })
        )));
        Ok(())
    }

    #[tokio::test]
    async fn existing_parts_resume_or_restart_when_ranges_are_ignored() -> Result<()> {
        let body = content(120 * 1024);
        for ranges in [true, false] {
            let (origin, requests, server) = serve(
                body.clone(),
                Behavior {
                    ranges,
                    ..Default::default()
                },
            )
            .await;
            let folder = tempfile::tempdir()?;
            let file = remote(&origin, &body, true);
            let part = part_path(folder.path(), "clip.mp4", &file.url);
            // A resumable prefix, or bytes the server's full answer replaces.
            let prefix = if ranges {
                body[..50_000].to_vec()
            } else {
                vec![9; 50_000]
            };
            std::fs::write(&part, prefix)?;
            let path = download_file(
                &plain_client(),
                &file,
                folder.path(),
                &CancellationToken::new(),
                &mut |_| {},
            )
            .await?;
            server.abort();
            assert_eq!(std::fs::read(&path)?, *body);
            assert!(!part.exists());
            let first = requests.lock().unwrap()[0].to_ascii_lowercase();
            assert!(first.contains("range: bytes=50000-"), "{first}");
        }
        Ok(())
    }

    #[tokio::test]
    async fn wrong_resumed_bytes_fail_the_checksum_and_are_removed() -> Result<()> {
        let body = content(64 * 1024);
        let (origin, _, server) = serve(
            body.clone(),
            Behavior {
                ranges: true,
                ..Default::default()
            },
        )
        .await;
        let folder = tempfile::tempdir()?;
        let file = remote(&origin, &body, true);
        let part = part_path(folder.path(), "clip.mp4", &file.url);
        std::fs::write(&part, vec![1; 1000])?;
        let error = download_file(
            &plain_client(),
            &file,
            folder.path(),
            &CancellationToken::new(),
            &mut |_| {},
        )
        .await
        .unwrap_err();
        server.abort();
        assert!(error.to_string().contains("checksum"), "{error}");
        assert!(names(folder.path()).is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn unrelated_files_are_kept_and_identical_copies_reused() -> Result<()> {
        let body = content(40 * 1024);
        let (origin, requests, server) = serve(body.clone(), Behavior::default()).await;
        let folder = tempfile::tempdir()?;
        std::fs::write(folder.path().join("clip.mp4"), b"someone else's video")?;
        let client = plain_client();
        let fetch = |file: RemoteFile| {
            let client = client.clone();
            let folder = folder.path().to_owned();
            async move {
                download_file(
                    &client,
                    &file,
                    &folder,
                    &CancellationToken::new(),
                    &mut |_| {},
                )
                .await
            }
        };
        let first = fetch(remote(&origin, &body, true)).await?;
        assert_eq!(first, folder.path().join("clip (1).mp4"));
        assert_eq!(
            std::fs::read(folder.path().join("clip.mp4"))?,
            b"someone else's video"
        );
        // Same size and checksum: no new request or copy.
        assert_eq!(fetch(remote(&origin, &body, true)).await?, first);
        assert_eq!(requests.lock().unwrap().len(), 1);
        // Without a checksum, a second download gets its own name.
        let unverified = fetch(remote(&origin, &body, false)).await?;
        assert_eq!(unverified, folder.path().join("clip (2).mp4"));
        server.abort();
        assert_eq!(
            names(folder.path()),
            ["clip (1).mp4", "clip (2).mp4", "clip.mp4"]
        );
        Ok(())
    }

    #[tokio::test]
    async fn cancelling_removes_the_partial_file() -> Result<()> {
        let body = content(2 * 1024 * 1024);
        let (origin, _, server) = serve(
            body.clone(),
            Behavior {
                delay: Some(Duration::from_millis(20)),
                ..Default::default()
            },
        )
        .await;
        let folder = tempfile::tempdir()?;
        let cancel = CancellationToken::new();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let job = {
            let (cancel, folder) = (cancel.clone(), folder.path().to_owned());
            let file = remote(&origin, &body, false);
            tokio::spawn(async move {
                download_file(&plain_client(), &file, &folder, &cancel, &mut |p| {
                    if p.downloaded_bytes > 0 {
                        let _ = tx.send(());
                    }
                })
                .await
            })
        };
        tokio::time::timeout(Duration::from_secs(10), rx.recv()).await?;
        assert_eq!(names(folder.path()).len(), 1);
        cancel.cancel();
        let error = tokio::time::timeout(Duration::from_secs(5), job)
            .await??
            .unwrap_err();
        server.abort();
        assert!(error.is::<Cancelled>());
        assert!(names(folder.path()).is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn refusals_and_concurrent_downloads_are_reported() -> Result<()> {
        let body = content(1024);
        let (origin, _, server) = serve(
            body.clone(),
            Behavior {
                status: Some(404),
                ..Default::default()
            },
        )
        .await;
        let folder = tempfile::tempdir()?;
        let file = remote(&origin, &body, false);
        let error = download_file(
            &plain_client(),
            &file,
            folder.path(),
            &CancellationToken::new(),
            &mut |_| {},
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("404"), "{error}");
        assert!(names(folder.path()).is_empty());

        let part = part_path(folder.path(), "clip.mp4", &file.url);
        let held = std::fs::File::create(&part)?;
        held.lock_exclusive()?;
        let error = download_file(
            &plain_client(),
            &file,
            folder.path(),
            &CancellationToken::new(),
            &mut |_| {},
        )
        .await
        .unwrap_err();
        server.abort();
        assert!(error.to_string().contains("already downloading"), "{error}");
        Ok(())
    }

    #[tokio::test]
    async fn web_pages_are_not_saved_as_files() -> Result<()> {
        let body = Arc::new(b"<!doctype html><title>File page</title>".to_vec());
        let (origin, _, server) = serve(
            body.clone(),
            Behavior {
                content_type: Some("text/html; charset=UTF-8"),
                ..Default::default()
            },
        )
        .await;
        let folder = tempfile::tempdir()?;
        let file = remote(&origin, &body, false);
        let error = download_all(
            &plain_client(),
            &[file],
            folder.path(),
            &CancellationToken::new(),
            &mut |_| {},
        )
        .await
        .unwrap_err();
        server.abort();
        assert!(error.is::<WebPage>(), "{error:#}");
        assert!(names(folder.path()).is_empty());
        Ok(())
    }

    #[test]
    fn content_ranges_parse() {
        let mut headers = header::HeaderMap::new();
        for (value, expected) in [
            ("bytes 100-199/200", Some((100, Some(200)))),
            ("bytes */200", Some((0, Some(200)))),
            ("bytes 5-9/*", Some((5, None))),
            ("items 1-2/3", None),
        ] {
            headers.insert(header::CONTENT_RANGE, value.parse().unwrap());
            assert_eq!(content_range(&headers), expected, "{value}");
        }
    }
}
