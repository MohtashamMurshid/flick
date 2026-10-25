//! Direct HTTPS downloads. Each file streams into a hidden, locked `.part`
//! file that resumes with a Range request after interruptions, is checked
//! against its published size and SHA-1, then gets a free name in the
//! destination. Existing files are never overwritten.

use super::{Cancelled, FetchProgress, FetchStage, FetchUpdate, RemoteFile, numbered};
use anyhow::{Context, Result, bail, ensure};
use fs2::FileExt;
use reqwest::{StatusCode, header};
use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};
use std::{
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::OnceLock,
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

/// Failed attempts without retained progress, or discarded prefixes, before giving up.
const ATTEMPTS: u32 = 5;
const REPORT_INTERVAL: Duration = Duration::from_millis(250);

pub(crate) fn client() -> Result<reqwest::Client> {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    if let Some(client) = CLIENT.get() {
        return Ok(client.clone());
    }
    let client = reqwest::Client::builder()
        .https_only(true)
        .connect_timeout(Duration::from_secs(15))
        .read_timeout(Duration::from_secs(30))
        .user_agent(concat!("Flick/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("Could not prepare HTTPS downloads")?;
    let _ = CLIENT.set(client.clone());
    Ok(CLIENT.get().cloned().unwrap_or(client))
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
        && let Some(path) = existing_copy(folder, &name, size, sha1, cancel).await?
    {
        return Ok(path);
    }
    let path = part_path(folder, &name, &file.url);
    let mut part = Part::open(&path, sha1.is_some(), cancel).await?;
    let result = transfer(client, file, &mut part, cancel, progress).await;
    let result = match result {
        Ok(total) => part.finish(total, sha1.as_deref(), progress).await,
        Err(error) => Err(error),
    };
    match result {
        Ok(()) => {
            part.remove_sidecar();
            ensure!(part.owns_path(), "The partial download was replaced");
            // Keep the exclusive part lock through publication and cleanup.
            let result = place(&path, folder, &name);
            if result.is_err() {
                part.remove_partial();
            }
            result
        }
        Err(error) => {
            part.remove_sidecar();
            part.remove_partial();
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

const MAX_VALIDATOR: usize = 1024;
const MAX_RESOURCE: usize = 1024;
const MAX_IDENTITY: u64 = 8192;
const IDENTITY_FORMAT: &str = "flick-http-part-v1";

/// If-Range requires a strong validator (RFC 9110 sections 8.8.2.2,
/// 13.1.5). A date is eligible only without any ETag, and with an earlier
/// Last-Modified than Date. We conservatively require a full 60 second gap
/// and accept only IMF-fixdate, rather than guessing about clocks or dates.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum Validator {
    Etag(String),
    Modified { value: String, date: String },
}

/// ETags are scoped to a resource. A redirected target changing to another
/// URL with the same ETag must not authorize combining the two resources.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct ResumeIdentity {
    resource: String,
    validator: Validator,
}

impl ResumeIdentity {
    fn from_response(response: &reqwest::Response) -> Option<Self> {
        if response.url().as_str().len() > MAX_RESOURCE {
            return None;
        }
        Some(Self {
            resource: response.url().as_str().to_owned(),
            validator: Validator::from_headers(response.headers())?,
        })
    }

    fn valid(&self) -> bool {
        self.resource.len() <= MAX_RESOURCE
            && reqwest::Url::parse(&self.resource)
                .is_ok_and(|url| matches!(url.scheme(), "https" | "http"))
            && self.validator.valid()
    }

    fn matches(&self, response: &reqwest::Response) -> bool {
        self.resource == response.url().as_str() && self.validator.matches(response.headers())
    }
}

impl Validator {
    fn from_headers(headers: &header::HeaderMap) -> Option<Self> {
        if headers.contains_key(header::ETAG) {
            let value = single_header(headers, header::ETAG)?;
            return strong_etag(value).then(|| Self::Etag(value.to_str().unwrap().to_owned()));
        }
        let value = single_header(headers, header::LAST_MODIFIED)?
            .to_str()
            .ok()?;
        let date = single_header(headers, header::DATE)?.to_str().ok()?;
        (http_date(date)?.checked_sub(http_date(value)?)? >= 60).then(|| Self::Modified {
            value: value.to_owned(),
            date: date.to_owned(),
        })
    }

    fn valid(&self) -> bool {
        match self {
            Self::Etag(value) => {
                header::HeaderValue::from_str(value).is_ok_and(|v| strong_etag(&v))
            }
            Self::Modified { value, date } => http_date(date)
                .zip(http_date(value))
                .and_then(|(date, value)| date.checked_sub(value))
                .is_some_and(|gap| gap >= 60),
        }
    }

    fn value(&self) -> &str {
        match self {
            Self::Etag(value) | Self::Modified { value, .. } => value,
        }
    }

    fn matches(&self, headers: &header::HeaderMap) -> bool {
        let name = match self {
            Self::Etag(_) => header::ETAG,
            Self::Modified { .. } if !headers.contains_key(header::ETAG) => header::LAST_MODIFIED,
            Self::Modified { .. } => return false,
        };
        single_header(headers, name)
            .is_some_and(|value| value.as_bytes() == self.value().as_bytes())
    }
}

fn single_header(
    headers: &header::HeaderMap,
    name: header::HeaderName,
) -> Option<&header::HeaderValue> {
    let mut values = headers.get_all(name).iter();
    let value = values.next()?;
    values.next().is_none().then_some(value)
}

fn strong_etag(value: &header::HeaderValue) -> bool {
    let bytes = value.as_bytes();
    (2..=MAX_VALIDATOR).contains(&bytes.len())
        && bytes.first() == Some(&b'"')
        && bytes.last() == Some(&b'"')
        && bytes[1..bytes.len() - 1]
            .iter()
            .all(|b| *b == 0x21 || (0x23..=0x7e).contains(b))
}

/// Strict, bounded IMF-fixdate parsing for validator eligibility only.
/// Unusual/obsolete HTTP dates simply disable checksum-free resumption.
fn http_date(value: &str) -> Option<u64> {
    if value.len() != 29 || !value.is_ascii() {
        return None;
    }
    let b = value.as_bytes();
    if &b[3..5] != b", "
        || b[7] != b' '
        || b[11] != b' '
        || b[16] != b' '
        || b[19] != b':'
        || b[22] != b':'
        || &b[25..] != b" GMT"
    {
        return None;
    }
    let number = |start, end| {
        let s = &value[start..end];
        s.bytes()
            .all(|b| b.is_ascii_digit())
            .then(|| s.parse::<u64>().ok())
            .flatten()
    };
    let (day, year, hour, minute, second) = (
        number(5, 7)?,
        number(12, 16)?,
        number(17, 19)?,
        number(20, 22)?,
        number(23, 25)?,
    );
    if year < 1601 || hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    let month = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ]
    .iter()
    .position(|m| *m == &value[8..11])?;
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let months = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    if day == 0 || day > months[month] {
        return None;
    }
    let y = year - 1;
    let days = y * 365 + y / 4 - y / 100 + y / 400 + months[..month].iter().sum::<u64>() + day - 1;
    let weekday = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"][(days % 7) as usize];
    (weekday == &value[..3]).then_some(days * 86400 + hour * 3600 + minute * 60 + second)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct FileIdentity {
    device: u64,
    inode: u64,
}

fn file_identity(metadata: &std::fs::Metadata) -> Option<FileIdentity> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Some(FileIdentity {
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        // No portable stable file ID: safely redownload checksum-free parts.
        None
    }
}

fn regular_file(metadata: &std::fs::Metadata) -> bool {
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        metadata.nlink() == 1
    }
    #[cfg(not(unix))]
    true
}

fn regular_options() -> std::fs::OpenOptions {
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(0x00200000); // FILE_FLAG_OPEN_REPARSE_POINT
    }
    options
}

fn same_file(path: &Path, file: &std::fs::File) -> bool {
    let (Ok(path), Ok(open)) = (path.symlink_metadata(), file.metadata()) else {
        return false;
    };
    if !regular_file(&path) || !regular_file(&open) {
        return false;
    }
    match (file_identity(&path), file_identity(&open)) {
        (Some(path), Some(open)) => path == open,
        _ => path
            .created()
            .ok()
            .zip(open.created().ok())
            .is_some_and(|(a, b)| a == b),
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct IdentityRecord {
    format: String,
    part: FileIdentity,
    validator: Option<ResumeIdentity>,
}

/// Only a recognized record bound to this locked part inode is ours to
/// update/remove. Unknown files, symlinks and hard links are left alone.
struct Sidecar {
    path: PathBuf,
    file: std::fs::File,
}

impl Sidecar {
    fn path(part: &Path) -> PathBuf {
        let mut name = part.as_os_str().to_owned();
        name.push(".identity");
        PathBuf::from(name)
    }

    fn load(
        part: &Path,
        identity: Option<&FileIdentity>,
    ) -> (Option<Self>, Option<ResumeIdentity>) {
        let Some(identity) = identity else {
            return (None, None);
        };
        let path = Self::path(part);
        let loaded = (|| -> Option<_> {
            let mut file = regular_options().open(&path).ok()?;
            if !same_file(&path, &file) || file.metadata().ok()?.len() > MAX_IDENTITY {
                return None;
            }
            let mut bytes = Vec::new();
            Read::by_ref(&mut file)
                .take(MAX_IDENTITY + 1)
                .read_to_end(&mut bytes)
                .ok()?;
            if bytes.len() as u64 > MAX_IDENTITY {
                return None;
            }
            let record: IdentityRecord = serde_json::from_slice(&bytes).ok()?;
            if record.format != IDENTITY_FORMAT || &record.part != identity {
                return None;
            }
            let validator = record.validator.filter(ResumeIdentity::valid);
            Some((Self { path, file }, validator))
        })();
        loaded.map_or((None, None), |(sidecar, validator)| {
            (Some(sidecar), validator)
        })
    }

    fn create(part: &Path) -> Option<Self> {
        let path = Self::path(part);
        let file = regular_options().create_new(true).open(&path).ok()?;
        same_file(&path, &file).then_some(Self { path, file })
    }

    fn store(&mut self, part: &FileIdentity, validator: Option<ResumeIdentity>) -> Result<()> {
        ensure!(
            same_file(&self.path, &self.file),
            "The partial download identity was replaced"
        );
        let bytes = serde_json::to_vec(&IdentityRecord {
            format: IDENTITY_FORMAT.into(),
            part: part.clone(),
            validator,
        })?;
        ensure!(
            bytes.len() as u64 <= MAX_IDENTITY,
            "The partial download identity is too large"
        );
        self.file.set_len(0)?;
        self.file.seek(SeekFrom::Start(0))?;
        self.file.write_all(&bytes)?;
        self.file.sync_all()?;
        Ok(())
    }

    fn remove(&mut self) {
        if same_file(&self.path, &self.file) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

struct Part {
    file: BufWriter<tokio::fs::File>,
    guard: std::fs::File,
    len: u64,
    hasher: Option<Sha1>,
    path: PathBuf,
    identity: Option<FileIdentity>,
    sidecar: Option<Sidecar>,
    validator: Option<ResumeIdentity>,
}

impl Part {
    /// Opens and locks the part file, keeping existing bytes to resume.
    async fn open(path: &Path, hash: bool, cancel: &CancellationToken) -> Result<Self> {
        let path = path.to_owned();
        let cancel = cancel.clone();
        let opened_path = path.clone();
        let (file, len, hasher, identity, sidecar, validator, guard) =
            tokio::task::spawn_blocking(move || -> Result<_> {
                let mut options = regular_options();
                // A published hard link retains this inode's permissions. Match
                // ordinary downloads (0666 filtered by umask); sidecars stay 0600.
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt;
                    options.mode(0o666);
                }
                let mut file = options
                    .create(true)
                    .open(&path)
                    .with_context(|| format!("Could not write {}", path.display()))?;
                ensure!(
                    regular_file(&file.metadata()?),
                    "The partial download is not a private regular file"
                );
                if file.try_lock_exclusive().is_err() {
                    bail!("This file is already downloading");
                }
                ensure!(same_file(&path, &file), "The partial download was replaced");
                let identity = file_identity(&file.metadata()?);
                let (mut sidecar, validator) = Sidecar::load(&path, identity.as_ref());
                let prepared = (|| -> Result<_> {
                    if cancel.is_cancelled() {
                        return Err(Cancelled.into());
                    }
                    let len = file.metadata()?.len();
                    // Checksummed resumes must hash the prefix. A plain resume needs
                    // only its length, even for a multi-gigabyte partial file.
                    let mut hasher = hash.then(Sha1::new);
                    if let Some(hasher) = &mut hasher {
                        let mut buffer = vec![0; 256 * 1024];
                        loop {
                            if cancel.is_cancelled() {
                                return Err(Cancelled.into());
                            }
                            let read = file.read(&mut buffer)?;
                            if read == 0 {
                                break;
                            }
                            hasher.update(&buffer[..read]);
                        }
                    }
                    file.seek(SeekFrom::Start(len))?;
                    let guard = file.try_clone()?;
                    Ok((len, hasher, guard))
                })();
                match prepared {
                    Ok((len, hasher, guard)) => {
                        Ok((file, len, hasher, identity, sidecar, validator, guard))
                    }
                    Err(error) => {
                        if let Some(sidecar) = &mut sidecar {
                            sidecar.remove();
                        }
                        if same_file(&path, &file) {
                            let _ = std::fs::remove_file(&path);
                        }
                        Err(error)
                    }
                }
            })
            .await
            .context("Could not open the partial download")??;
        let file = tokio::fs::File::from_std(file);
        Ok(Self {
            file: BufWriter::with_capacity(256 * 1024, file),
            guard,
            len,
            hasher,
            path: opened_path,
            identity,
            sidecar,
            validator,
        })
    }

    async fn set_validator(&mut self, validator: Option<ResumeIdentity>) -> Result<()> {
        let guard = self.guard.try_clone()?;
        let (path, identity, mut sidecar) = (
            self.path.clone(),
            self.identity.clone(),
            self.sidecar.take(),
        );
        let stored = validator.clone();
        let (sidecar, result) = tokio::task::spawn_blocking(move || {
            // A dropped async future must not release the part lock while
            // this blocking sidecar operation is still in flight.
            let _guard = guard;
            let result = (|| -> Result<()> {
                if let Some(identity) = identity {
                    if sidecar.is_none() && stored.is_some() {
                        sidecar = Sidecar::create(&path);
                    }
                    if let Some(sidecar) = &mut sidecar {
                        sidecar.store(&identity, stored)?;
                    }
                }
                Ok(())
            })();
            (sidecar, result)
        })
        .await
        .context("Could not save the partial download identity")?;
        self.sidecar = sidecar;
        result?;
        self.validator = validator;
        Ok(())
    }

    fn owns_path(&self) -> bool {
        // The lock and this handle remain live until after unlink/publication.
        same_file(&self.path, &self.guard)
    }

    fn remove_partial(&self) {
        if self.owns_path() {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    fn remove_sidecar(&mut self) {
        if let Some(sidecar) = &mut self.sidecar {
            sidecar.remove();
        }
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
    async fn restart(&mut self, restarts: &mut u32) -> Result<()> {
        *restarts += 1;
        ensure!(
            *restarts < ATTEMPTS,
            "The server kept restarting the download"
        );
        // Invalidate durably BEFORE truncation/replacement bytes, so a crash
        // cannot associate a new object's prefix with the previous validator.
        self.set_validator(None).await?;
        self.file.flush().await?;
        self.file.get_mut().set_len(0).await?;
        self.file.get_mut().sync_all().await?;
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
    let mut restarts = 0;
    // Receiving the same prefix again after a restart is not new progress.
    let mut high_water = part.len;
    let mut meter = Meter::new(part.len);
    'attempt: loop {
        if cancel.is_cancelled() {
            return Err(Cancelled.into());
        }
        if part.len > 0 && part.hasher.is_none() && part.validator.is_none() {
            // Legacy/unknown prefixes have no identity. Size alone cannot
            // establish that these bytes belong to the current remote object.
            part.restart(&mut restarts).await?;
            total = file.size;
            meter = Meter::new(0);
        }
        let offset = part.len;
        if offset > 0 && total == Some(offset) && part.hasher.is_some() {
            return Ok(total);
        }
        let mut request = client.get(&file.url);
        if offset > 0 {
            request = request.header(header::RANGE, format!("bytes={offset}-"));
            if let Some(validator) = &part.validator {
                request = request.header(header::IF_RANGE, validator.validator.value());
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
            StatusCode::PARTIAL_CONTENT if offset > 0 => {
                // A published checksum checks the whole assembled object, even
                // across rotating mirror URLs or validators. Otherwise check
                // resource identity: a broken server may ignore If-Range.
                if part.hasher.is_none()
                    && part
                        .validator
                        .as_ref()
                        .is_some_and(|v| !v.matches(&response))
                {
                    part.restart(&mut restarts).await?;
                    total = file.size;
                    meter = Meter::new(0);
                    continue;
                }
                match content_range(response.headers()) {
                    Some((start, size)) if start == offset => total = size.or(total),
                    _ => {
                        part.restart(&mut restarts).await?;
                        total = file.size;
                        meter = Meter::new(0);
                        continue;
                    }
                }
            }
            StatusCode::PARTIAL_CONTENT => bail!("The server sent an unexpected partial response"),
            StatusCode::OK => {
                if offset > 0 {
                    part.restart(&mut restarts).await?;
                    meter = Meter::new(0);
                }
                total = response.content_length().or(total);
            }
            StatusCode::RANGE_NOT_SATISFIABLE if offset > 0 => {
                if content_range(response.headers()).and_then(|(_, size)| size) == Some(offset)
                    && (part.hasher.is_some()
                        || part
                            .validator
                            .as_ref()
                            .is_some_and(|v| v.matches(&response)))
                {
                    return Ok(Some(offset));
                }
                part.restart(&mut restarts).await?;
                total = file.size;
                meter = Meter::new(0);
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
        if part.len == 0 {
            part.set_validator(ResumeIdentity::from_response(&response))
                .await?;
        }
        let mut response = response;
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
                    if part.len > high_water && (part.hasher.is_some() || part.validator.is_some())
                    {
                        high_water = part.len;
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
    cancel: &CancellationToken,
) -> Result<Option<PathBuf>> {
    let folder = folder.to_owned();
    let (name, sha1) = (name.to_owned(), sha1.to_owned());
    let cancel = cancel.clone();
    tokio::task::spawn_blocking(move || {
        for n in 0..1000 {
            if cancel.is_cancelled() {
                return Err(Cancelled.into());
            }
            let candidate = folder.join(numbered(&name, n));
            let Ok(metadata) = candidate.symlink_metadata() else {
                return Ok(None);
            };
            if metadata.is_file()
                && metadata.len() == size
                && hash_file(&candidate, &cancel)? == sha1
            {
                return Ok(Some(candidate));
            }
        }
        Ok(None)
    })
    .await
    .context("Could not check existing downloads")?
}

fn hash_file(path: &Path, cancel: &CancellationToken) -> Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha1::new();
    let mut buffer = vec![0; 256 * 1024];
    loop {
        if cancel.is_cancelled() {
            return Err(Cancelled.into());
        }
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
        /// None uses the default strong ETag; Some("") omits validators.
        pub validator_headers: Option<&'static str>,
        pub honor_if_range: bool,
        pub unexpected_partial: bool,
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
        let mut range = head
            .lines()
            .find_map(|l| {
                l.to_ascii_lowercase()
                    .strip_prefix("range: bytes=")
                    .map(str::to_owned)
            })
            .and_then(|r| r.trim_end_matches('-').parse::<usize>().ok());
        let validators = behavior.validator_headers.unwrap_or("ETag: \"v1\"\r\n");
        let if_range = head.lines().find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("if-range").then(|| value.trim())
        });
        let current = validators.lines().find_map(|line| {
            let (name, value) = line.split_once(':')?;
            (name.eq_ignore_ascii_case("etag") || name.eq_ignore_ascii_case("last-modified"))
                .then(|| value.trim())
        });
        if behavior.honor_if_range && if_range.is_some() && if_range != current {
            range = None;
        }
        if behavior.unexpected_partial && range.is_none() {
            range = Some(0);
        }
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
        response.push_str(validators);
        response.push_str("Accept-Ranges: bytes\r\nConnection: close\r\n\r\n");
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

    fn request_header<'a>(request: &'a str, name: &str) -> Option<&'a str> {
        request.lines().find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.eq_ignore_ascii_case(name).then(|| value.trim())
        })
    }

    /// Drop all handles without normal download cleanup, as after a process
    /// interruption. The next call must recover identity solely from disk.
    async fn interrupted_part(
        path: &Path,
        bytes: &[u8],
        validator: Option<Validator>,
        resource: &str,
    ) -> Result<()> {
        let mut part = Part::open(path, false, &CancellationToken::new()).await?;
        part.set_validator(validator.map(|validator| ResumeIdentity {
            resource: resource.into(),
            validator,
        }))
        .await?;
        part.write(bytes).await?;
        part.file.flush().await?;
        part.file.get_mut().sync_all().await?;
        Ok(())
    }

    /// Scripted loopback responses exercise redirects and broken range servers.
    async fn serve_scripted(
        mut reply: impl FnMut(&str, usize) -> (String, Vec<u8>) + Send + 'static,
    ) -> (String, Arc<Mutex<Vec<String>>>, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let log = requests.clone();
        let server = tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let mut head = Vec::new();
                let mut byte = [0; 1];
                while !head.ends_with(b"\r\n\r\n") && head.len() < 16 * 1024 {
                    if stream.read(&mut byte).await.ok() != Some(1) {
                        break;
                    }
                    head.push(byte[0]);
                }
                let head = String::from_utf8_lossy(&head).into_owned();
                let index = {
                    let mut log = log.lock().unwrap();
                    log.push(head.clone());
                    log.len() - 1
                };
                let (response, body) = reply(&head, index);
                if stream.write_all(response.as_bytes()).await.is_ok() {
                    let _ = stream.write_all(&body).await;
                }
                let _ = stream.shutdown().await;
            }
        });
        (origin, requests, server)
    }

    fn range_start(request: &str) -> Option<usize> {
        request_header(request, "range")?
            .strip_prefix("bytes=")?
            .strip_suffix('-')?
            .parse()
            .ok()
    }

    #[tokio::test]
    async fn checksummed_rotating_mirrors_resume_and_verify_the_complete_object() -> Result<()> {
        for validator in ["ETag: \"v1\"\r\n", "ETag: \"rotated\"\r\n", ""] {
            for corrupt in [false, true] {
                let body = content(64 * 1024);
                let fixture = body.clone();
                let (origin, requests, server) = serve_scripted(move |request, index| {
                    if request.starts_with("GET /media/") {
                        return (
                            format!("HTTP/1.1 302 Found\r\nLocation: /mirror/{index}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"),
                            Vec::new(),
                        );
                    }
                    let start = range_start(request).unwrap_or(0);
                    let (status, range, headers) = if start == 0 {
                        ("200 OK", String::new(), "ETag: \"v1\"\r\n")
                    } else {
                        ("206 Partial Content", format!("Content-Range: bytes {start}-{}/{}\r\n", fixture.len() - 1, fixture.len()), validator)
                    };
                    let mut bytes = fixture[start..(start + 16 * 1024).min(fixture.len())].to_vec();
                    if corrupt && start > 0 {
                        bytes.fill(0xff);
                    }
                    (
                        format!("HTTP/1.1 {status}\r\n{range}{headers}Content-Length: {}\r\nConnection: close\r\n\r\n", fixture.len() - start),
                        bytes,
                    )
                }).await;
                let folder = tempfile::tempdir()?;
                let result = tokio::time::timeout(
                    Duration::from_secs(15),
                    download_file(
                        &plain_client(),
                        &remote(&origin, &body, true),
                        folder.path(),
                        &CancellationToken::new(),
                        &mut |_| {},
                    ),
                )
                .await?;
                server.abort();
                if corrupt {
                    assert!(result.unwrap_err().to_string().contains("checksum"));
                    assert!(names(folder.path()).is_empty());
                } else {
                    assert_eq!(std::fs::read(result?)?, *body);
                    assert_eq!(names(folder.path()), ["clip.mp4"]);
                }
                let log = requests.lock().unwrap();
                let mirrors: Vec<_> = log
                    .iter()
                    .filter(|r| r.starts_with("GET /mirror/"))
                    .collect();
                assert_eq!(mirrors.len(), 4, "{log:?}");
                assert_eq!(range_start(mirrors[0]), None);
                for (index, request) in mirrors.iter().enumerate().skip(1) {
                    assert_eq!(range_start(request), Some(index * 16 * 1024));
                    assert_eq!(request_header(request, "if-range"), Some("\"v1\""));
                }
            }
        }
        Ok(())
    }

    #[tokio::test]
    async fn discarded_prefixes_are_bounded_even_when_each_new_prefix_is_larger() -> Result<()> {
        for rejection in ["validator", "content-range", "416", "200"] {
            let body = content(128 * 1024);
            let fixture = body.clone();
            let (origin, requests, server) = serve_scripted(move |request, index| {
                let offset = range_start(request);
                let prefix = (index + 1) * 1024;
                let (status, extra, start) = match (rejection, offset) {
                    ("validator", Some(start)) => ("206 Partial Content", format!("ETag: \"changed\"\r\nContent-Range: bytes {start}-{}/{}\r\n", fixture.len()-1, fixture.len()), start),
                    ("content-range", Some(start)) => ("206 Partial Content", "ETag: \"v1\"\r\nContent-Range: invalid\r\n".into(), start),
                    ("416", Some(_)) => return ("HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into(), Vec::new()),
                    _ => ("200 OK", "ETag: \"v1\"\r\n".into(), 0),
                };
                (format!("HTTP/1.1 {status}\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n", fixture.len()-start), fixture[start..start+prefix].to_vec())
            }).await;
            let folder = tempfile::tempdir()?;
            let result = tokio::time::timeout(
                Duration::from_secs(15),
                download_file(
                    &plain_client(),
                    &remote(&origin, &body, false),
                    folder.path(),
                    &CancellationToken::new(),
                    &mut |_| {},
                ),
            )
            .await;
            server.abort();
            let error = result?.unwrap_err().to_string();
            assert!(error.contains("kept restarting"), "{rejection}: {error}");
            let log = requests.lock().unwrap();
            assert!(log.len() <= 10, "{rejection}: {} requests", log.len());
            assert!(names(folder.path()).is_empty());
        }
        Ok(())
    }

    #[tokio::test]
    async fn repeated_identical_full_prefixes_cannot_reset_the_failure_budget() -> Result<()> {
        let body = content(64 * 1024);
        let fixture = body.clone();
        let (origin, requests, server) = serve_scripted(move |_, _| {
            (format!("HTTP/1.1 200 OK\r\nETag: \"v1\"\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", fixture.len()), fixture[..16 * 1024].to_vec())
        }).await;
        let folder = tempfile::tempdir()?;
        let result = tokio::time::timeout(
            Duration::from_secs(25),
            download_file(
                &plain_client(),
                &remote(&origin, &body, false),
                folder.path(),
                &CancellationToken::new(),
                &mut |_| {},
            ),
        )
        .await;
        server.abort();
        let error = result?.unwrap_err();
        assert!(
            error.to_string().contains("connection kept dropping"),
            "{error}"
        );
        assert_eq!(requests.lock().unwrap().len(), ATTEMPTS as usize);
        assert!(names(folder.path()).is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn checksum_free_rotating_targets_never_splice_or_restart_forever() -> Result<()> {
        let body = content(64 * 1024);
        let fixture = body.clone();
        let (origin, requests, server) = serve_scripted(move |request, index| {
            if request.starts_with("GET /media/") {
                return (
                    format!("HTTP/1.1 302 Found\r\nLocation: /mirror/{index}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"),
                    Vec::new(),
                );
            }
            if let Some(start) = range_start(request) {
                // A different resource can legitimately reuse the same ETag.
                return (
                    format!("HTTP/1.1 206 Partial Content\r\nETag: \"v1\"\r\nContent-Range: bytes {start}-{}/{}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", fixture.len()-1, fixture.len(), fixture.len()-start),
                    vec![0xff; fixture.len()-start],
                );
            }
            (format!("HTTP/1.1 200 OK\r\nETag: \"v1\"\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", fixture.len()), fixture[..16*1024].to_vec())
        }).await;
        let folder = tempfile::tempdir()?;
        let result = tokio::time::timeout(
            Duration::from_secs(25),
            download_file(
                &plain_client(),
                &remote(&origin, &body, false),
                folder.path(),
                &CancellationToken::new(),
                &mut |_| {},
            ),
        )
        .await;
        server.abort();
        let error = result?.unwrap_err();
        assert!(error.to_string().contains("kept dropping"), "{error}");
        let log = requests.lock().unwrap();
        assert_eq!(
            log.iter().filter(|r| r.starts_with("GET /mirror/")).count(),
            9
        );
        assert!(names(folder.path()).is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn retained_range_progress_can_exceed_the_retry_count() -> Result<()> {
        let body = content(128 * 1024);
        let fixture = body.clone();
        let (origin, requests, server) = serve_scripted(move |request, _| {
            let start = range_start(request).unwrap_or(0);
            let (status, extra) = if start == 0 {
                ("200 OK", String::new())
            } else {
                ("206 Partial Content", format!("Content-Range: bytes {start}-{}/{}\r\n", fixture.len()-1, fixture.len()))
            };
            (format!("HTTP/1.1 {status}\r\n{extra}ETag: \"v1\"\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", fixture.len()-start), fixture[start..(start+16*1024).min(fixture.len())].to_vec())
        }).await;
        let folder = tempfile::tempdir()?;
        let result = tokio::time::timeout(
            Duration::from_secs(20),
            download_file(
                &plain_client(),
                &remote(&origin, &body, false),
                folder.path(),
                &CancellationToken::new(),
                &mut |_| {},
            ),
        )
        .await;
        server.abort();
        assert_eq!(std::fs::read(result??)?, *body);
        assert_eq!(requests.lock().unwrap().len(), 8);
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn downloads_follow_creation_umask_and_sidecars_stay_private() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let folder = tempfile::tempdir()?;
        let ordinary = folder.path().join("ordinary");
        std::fs::File::create(&ordinary)?;
        let expected = ordinary.metadata()?.permissions().mode() & 0o777;
        let body = content(16 * 1024);
        let (origin, _, server) = serve(body.clone(), Behavior::default()).await;
        let file = remote(&origin, &body, false);
        let path = part_path(folder.path(), "clip.mp4", &file.url);
        interrupted_part(
            &path,
            &body[..1024],
            Some(Validator::Etag("\"v1\"".into())),
            &file.url,
        )
        .await?;
        assert_eq!(path.metadata()?.permissions().mode() & 0o777, expected);
        assert_eq!(
            Sidecar::path(&path).metadata()?.permissions().mode() & 0o777,
            expected & 0o600
        );
        let result = download_file(
            &plain_client(),
            &file,
            folder.path(),
            &CancellationToken::new(),
            &mut |_| {},
        )
        .await;
        server.abort();
        let saved = result?;
        assert_eq!(saved.metadata()?.permissions().mode() & 0o777, expected);
        assert_eq!(std::fs::read(saved)?, *body);
        assert!(!Sidecar::path(&path).exists());
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn persisted_validator_is_sent_on_the_first_request_after_reopen() -> Result<()> {
        let body = content(100 * 1024);
        for (headers, validator) in [
            ("ETag: \"v1\"\r\n", Validator::Etag("\"v1\"".into())),
            (
                "Last-Modified: Sun, 06 Nov 1994 08:49:37 GMT\r\nDate: Sun, 06 Nov 1994 08:50:37 GMT\r\n",
                Validator::Modified {
                    value: "Sun, 06 Nov 1994 08:49:37 GMT".into(),
                    date: "Sun, 06 Nov 1994 08:50:37 GMT".into(),
                },
            ),
        ] {
            for prefix in [20_000, body.len()] {
                let (origin, requests, server) = serve(
                    body.clone(),
                    Behavior {
                        ranges: true,
                        honor_if_range: true,
                        validator_headers: Some(headers),
                        ..Default::default()
                    },
                )
                .await;
                let folder = tempfile::tempdir()?;
                let mut file = remote(&origin, &body, false);
                file.size = Some(body.len() as u64);
                let path = part_path(folder.path(), "clip.mp4", &file.url);
                interrupted_part(&path, &body[..prefix], Some(validator.clone()), &file.url)
                    .await?;
                assert!(Sidecar::path(&path).metadata()?.len() <= MAX_IDENTITY);
                let saved = download_file(
                    &plain_client(),
                    &file,
                    folder.path(),
                    &CancellationToken::new(),
                    &mut |_| {},
                )
                .await?;
                server.abort();
                assert_eq!(std::fs::read(saved)?, *body);
                assert_eq!(names(folder.path()), ["clip.mp4"]);
                let requests = requests.lock().unwrap();
                assert_eq!(requests.len(), 1);
                assert_eq!(
                    request_header(&requests[0], "range"),
                    Some(format!("bytes={prefix}-").as_str())
                );
                assert_eq!(
                    request_header(&requests[0], "if-range"),
                    Some(validator.value())
                );
            }
        }
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn changed_same_sized_objects_never_splice_or_use_the_completed_part_shortcut()
    -> Result<()> {
        let body = content(100 * 1024);
        for honor_if_range in [false, true] {
            for prefix in [20_000, body.len()] {
                let (origin, requests, server) = serve(
                    body.clone(),
                    Behavior {
                        ranges: true,
                        honor_if_range,
                        validator_headers: Some("ETag: \"v2\"\r\n"),
                        ..Default::default()
                    },
                )
                .await;
                let folder = tempfile::tempdir()?;
                let mut file = remote(&origin, &body, false);
                file.size = Some(body.len() as u64);
                let path = part_path(folder.path(), "clip.mp4", &file.url);
                interrupted_part(
                    &path,
                    &vec![0xff; prefix],
                    Some(Validator::Etag("\"v1\"".into())),
                    &file.url,
                )
                .await?;
                let saved = download_file(
                    &plain_client(),
                    &file,
                    folder.path(),
                    &CancellationToken::new(),
                    &mut |_| {},
                )
                .await?;
                server.abort();
                assert_eq!(std::fs::read(saved)?, *body);
                assert_eq!(names(folder.path()), ["clip.mp4"]);
                let requests = requests.lock().unwrap();
                assert_eq!(requests.len(), if honor_if_range { 1 } else { 2 });
                assert_eq!(request_header(&requests[0], "if-range"), Some("\"v1\""));
                if !honor_if_range {
                    assert!(request_header(&requests[1], "range").is_none());
                    assert!(request_header(&requests[1], "if-range").is_none());
                }
            }
        }
        Ok(())
    }

    #[tokio::test]
    async fn checksum_free_legacy_or_invalid_validator_parts_restart_without_a_range() -> Result<()>
    {
        let body = content(32 * 1024);
        for validator in [
            None,
            Some(Validator::Etag("W/\"v1\"".into())),
            Some(Validator::Etag("not-an-etag".into())),
            Some(Validator::Etag(format!(
                "\"{}\"",
                "x".repeat(MAX_VALIDATOR)
            ))),
            Some(Validator::Modified {
                value: "Sun, 06 Nov 1994 08:49:37 GMT".into(),
                date: "Sun, 06 Nov 1994 08:49:38 GMT".into(),
            }),
        ] {
            for prefix in [1000, body.len()] {
                let (origin, requests, server) = serve(
                    body.clone(),
                    Behavior {
                        ranges: true,
                        ..Default::default()
                    },
                )
                .await;
                let folder = tempfile::tempdir()?;
                let mut file = remote(&origin, &body, false);
                file.size = Some(body.len() as u64);
                let path = part_path(folder.path(), "clip.mp4", &file.url);
                interrupted_part(&path, &vec![0xff; prefix], validator.clone(), &file.url).await?;
                let saved = download_file(
                    &plain_client(),
                    &file,
                    folder.path(),
                    &CancellationToken::new(),
                    &mut |_| {},
                )
                .await?;
                server.abort();
                assert_eq!(std::fs::read(saved)?, *body);
                assert_eq!(names(folder.path()), ["clip.mp4"]);
                let requests = requests.lock().unwrap();
                assert_eq!(requests.len(), 1);
                assert!(request_header(&requests[0], "range").is_none());
                assert!(request_header(&requests[0], "if-range").is_none());
            }
        }
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_missing_response_validator_restarts_and_unsolicited_206_is_rejected() -> Result<()> {
        let body = content(16 * 1024);
        for unexpected_partial in [false, true] {
            let (origin, requests, server) = serve(
                body.clone(),
                Behavior {
                    ranges: true,
                    validator_headers: Some(""),
                    unexpected_partial,
                    ..Default::default()
                },
            )
            .await;
            let folder = tempfile::tempdir()?;
            let file = remote(&origin, &body, false);
            let path = part_path(folder.path(), "clip.mp4", &file.url);
            interrupted_part(
                &path,
                &vec![0xff; 1000],
                Some(Validator::Etag("\"v1\"".into())),
                &file.url,
            )
            .await?;
            let saved = download_file(
                &plain_client(),
                &file,
                folder.path(),
                &CancellationToken::new(),
                &mut |_| {},
            )
            .await;
            server.abort();
            if unexpected_partial {
                assert!(
                    saved
                        .unwrap_err()
                        .to_string()
                        .contains("unexpected partial response")
                );
                assert!(names(folder.path()).is_empty());
            } else {
                assert_eq!(std::fs::read(saved?)?, *body);
                assert_eq!(names(folder.path()), ["clip.mp4"]);
            }
            let requests = requests.lock().unwrap();
            assert_eq!(requests.len(), 2);
            assert_eq!(request_header(&requests[0], "if-range"), Some("\"v1\""));
            assert!(request_header(&requests[1], "range").is_none());
        }
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unknown_oversized_and_foreign_sidecars_are_never_overwritten_or_removed() -> Result<()>
    {
        let body = content(8 * 1024);
        for record in [
            b"unrelated local file".to_vec(),
            vec![b'x'; MAX_IDENTITY as usize + 1],
            serde_json::to_vec(&IdentityRecord {
                format: IDENTITY_FORMAT.into(),
                part: FileIdentity {
                    device: u64::MAX,
                    inode: u64::MAX,
                },
                validator: Some(ResumeIdentity {
                    resource: "http://127.0.0.1/foreign".into(),
                    validator: Validator::Etag("\"v1\"".into()),
                }),
            })?,
        ] {
            let (origin, requests, server) = serve(
                body.clone(),
                Behavior {
                    ranges: true,
                    ..Default::default()
                },
            )
            .await;
            let folder = tempfile::tempdir()?;
            let file = remote(&origin, &body, false);
            let path = part_path(folder.path(), "clip.mp4", &file.url);
            std::fs::write(&path, vec![0xff; 1000])?;
            let sidecar = Sidecar::path(&path);
            std::fs::write(&sidecar, &record)?;
            let saved = download_file(
                &plain_client(),
                &file,
                folder.path(),
                &CancellationToken::new(),
                &mut |_| {},
            )
            .await?;
            server.abort();
            assert_eq!(std::fs::read(saved)?, *body);
            assert_eq!(std::fs::read(sidecar)?, record);
            assert!(!path.exists());
            assert!(request_header(&requests.lock().unwrap()[0], "range").is_none());
        }
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn sidecar_and_part_symlinks_or_hard_links_do_not_touch_their_targets() -> Result<()> {
        use std::os::unix::fs::symlink;
        let body = content(8 * 1024);
        for link_part in [false, true] {
            for hard_link in [false, true] {
                let (origin, requests, server) = serve(body.clone(), Behavior::default()).await;
                let folder = tempfile::tempdir()?;
                let file = remote(&origin, &body, false);
                let path = part_path(folder.path(), "clip.mp4", &file.url);
                let target = folder.path().join("someone-elses-file");
                std::fs::write(&target, b"keep these bytes")?;
                let link = if link_part {
                    path.clone()
                } else {
                    Sidecar::path(&path)
                };
                if hard_link {
                    std::fs::hard_link(&target, &link)?;
                } else {
                    symlink(&target, &link)?;
                }
                let saved = download_file(
                    &plain_client(),
                    &file,
                    folder.path(),
                    &CancellationToken::new(),
                    &mut |_| {},
                )
                .await;
                server.abort();
                assert_eq!(std::fs::read(&target)?, b"keep these bytes");
                assert_eq!(std::fs::read(&link)?, b"keep these bytes");
                if link_part {
                    assert!(saved.is_err());
                    assert!(requests.lock().unwrap().is_empty());
                } else {
                    assert_eq!(std::fs::read(saved?)?, *body);
                }
            }
        }
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn the_part_lock_protects_its_sidecar_until_cleanup() -> Result<()> {
        let folder = tempfile::tempdir()?;
        let path = folder.path().join("locked.part");
        interrupted_part(
            &path,
            b"fixture",
            Some(Validator::Etag("\"v1\"".into())),
            "http://127.0.0.1/fixture",
        )
        .await?;
        let mut held = Part::open(&path, false, &CancellationToken::new()).await?;
        let before = std::fs::read(Sidecar::path(&path))?;
        let error = Part::open(&path, false, &CancellationToken::new())
            .await
            .err()
            .unwrap();
        assert!(error.to_string().contains("already downloading"));
        assert_eq!(std::fs::read(Sidecar::path(&path))?, before);
        held.remove_sidecar();
        held.remove_partial();
        assert!(names(folder.path()).is_empty());
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn the_same_etag_at_a_different_resource_cannot_authorize_a_prefix() -> Result<()> {
        let body = content(16 * 1024);
        let (origin, requests, server) = serve(
            body.clone(),
            Behavior {
                ranges: true,
                ..Default::default()
            },
        )
        .await;
        let folder = tempfile::tempdir()?;
        let file = remote(&origin, &body, false);
        let path = part_path(folder.path(), "clip.mp4", &file.url);
        interrupted_part(
            &path,
            &vec![0xff; 1000],
            Some(Validator::Etag("\"v1\"".into())),
            &format!("{origin}/previous-redirect-target"),
        )
        .await?;
        let saved = download_file(
            &plain_client(),
            &file,
            folder.path(),
            &CancellationToken::new(),
            &mut |_| {},
        )
        .await?;
        server.abort();
        assert_eq!(std::fs::read(saved)?, *body);
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(request_header(&requests[0], "if-range"), Some("\"v1\""));
        assert!(request_header(&requests[1], "range").is_none());
        assert_eq!(names(folder.path()), ["clip.mp4"]);
        Ok(())
    }

    #[tokio::test]
    async fn checksum_failure_removes_a_fresh_partial_and_its_owned_identity() -> Result<()> {
        let body = content(8 * 1024);
        let (origin, _, server) = serve(body.clone(), Behavior::default()).await;
        let folder = tempfile::tempdir()?;
        let mut file = remote(&origin, &body, true);
        file.sha1 = Some("0".repeat(40));
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

    #[cfg(unix)]
    #[tokio::test]
    async fn cleanup_does_not_unlink_replacements_for_owned_paths() -> Result<()> {
        let folder = tempfile::tempdir()?;
        let path = folder.path().join("replaced.part");
        interrupted_part(
            &path,
            b"fixture",
            Some(Validator::Etag("\"v1\"".into())),
            "http://127.0.0.1/fixture",
        )
        .await?;
        let mut part = Part::open(&path, false, &CancellationToken::new()).await?;
        let identity = Sidecar::path(&path);
        std::fs::rename(&identity, folder.path().join("original.identity"))?;
        std::fs::rename(&path, folder.path().join("original.part"))?;
        std::fs::write(&identity, b"unrelated identity")?;
        std::fs::write(&path, b"unrelated partial")?;
        part.remove_sidecar();
        part.remove_partial();
        assert!(!part.owns_path());
        assert_eq!(std::fs::read(identity)?, b"unrelated identity");
        assert_eq!(std::fs::read(path)?, b"unrelated partial");
        Ok(())
    }

    #[test]
    fn only_strong_bounded_and_suitable_validators_are_accepted() {
        for (headers, expected) in [
            (vec![(header::ETAG, "\"v1\"")], true),
            (vec![(header::ETAG, "W/\"v1\"")], false),
            (vec![(header::ETAG, "unquoted")], false),
            (vec![(header::ETAG, "\"a\", \"b\"")], false),
            (
                vec![(header::LAST_MODIFIED, "Sun, 06 Nov 1994 08:49:37 GMT")],
                false,
            ),
            (
                vec![
                    (header::LAST_MODIFIED, "Sun, 06 Nov 1994 08:49:37 GMT"),
                    (header::DATE, "Sun, 06 Nov 1994 08:50:36 GMT"),
                ],
                false,
            ),
            (
                vec![
                    (header::LAST_MODIFIED, "Sun, 06 Nov 1994 08:49:37 GMT"),
                    (header::DATE, "Sun, 06 Nov 1994 08:50:37 GMT"),
                ],
                true,
            ),
            (
                vec![
                    (header::LAST_MODIFIED, "Sun, 06 Nov 1994 08:49:37 GMT"),
                    (header::DATE, "Sun, 06 Nov 1994 08:48:37 GMT"),
                ],
                false,
            ),
            (
                vec![
                    (header::LAST_MODIFIED, "Sun, 06 Nov 1994 08:49:37 GMT"),
                    (header::DATE, "Sun, 06 Nov 1994 08:50:37 GMT"),
                    (header::ETAG, "W/\"v1\""),
                ],
                false,
            ),
            (
                vec![(header::ETAG, "\"v1\""), (header::ETAG, "\"v1\"")],
                false,
            ),
        ] {
            let mut map = header::HeaderMap::new();
            for (name, value) in headers {
                map.append(name, value.parse().unwrap());
            }
            assert_eq!(Validator::from_headers(&map).is_some(), expected, "{map:?}");
        }
        for date in [
            "Mon, 06 Nov 1994 08:49:37 GMT",
            "Sun, 31 Feb 1994 08:49:37 GMT",
            "Sun, 06 Nov 1994 24:49:37 GMT",
            "Sun, 06 Nov 1994 08:49:37 UTC",
        ] {
            assert!(http_date(date).is_none(), "{date}");
        }
        let mut map = header::HeaderMap::new();
        map.insert(
            header::ETAG,
            format!("\"{}\"", "x".repeat(MAX_VALIDATOR))
                .parse()
                .unwrap(),
        );
        assert!(Validator::from_headers(&map).is_none());
    }

    #[tokio::test]
    async fn checksum_free_resume_uses_length_without_reading_a_large_sparse_prefix() -> Result<()>
    {
        let folder = tempfile::tempdir()?;
        let path = folder.path().join("large.part");
        let file = std::fs::File::create(&path)?;
        // set_len can reserve real disk space on Windows. Retain the large
        // sparse-prefix regression elsewhere and verify metadata-only opening
        // on Windows without allocating 16 GiB on its temporary volume.
        #[cfg(windows)]
        let bytes = 64_u64 * 1024 * 1024;
        #[cfg(not(windows))]
        let bytes = 16_u64 * 1024 * 1024 * 1024;
        file.set_len(bytes)?;
        drop(file);
        let part = tokio::time::timeout(
            Duration::from_secs(2),
            Part::open(&path, false, &CancellationToken::new()),
        )
        .await??;
        assert_eq!(part.len, bytes);
        assert!(part.hasher.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn cancelled_prefix_and_existing_copy_checks_stop_before_reading() -> Result<()> {
        let folder = tempfile::tempdir()?;
        let path = folder.path().join("cancel.part");
        std::fs::write(&path, b"generated fixture")?;
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert!(
            Part::open(&path, true, &cancel)
                .await
                .err()
                .unwrap()
                .is::<Cancelled>()
        );
        std::fs::write(&path, b"generated fixture")?;
        assert!(hash_file(&path, &cancel).unwrap_err().is::<Cancelled>());
        assert!(
            existing_copy(
                folder.path(),
                "cancel.part",
                17,
                "0000000000000000000000000000000000000000",
                &cancel
            )
            .await
            .unwrap_err()
            .is::<Cancelled>()
        );
        Ok(())
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
        assert_eq!(names(folder.path()).len(), if cfg!(unix) { 2 } else { 1 });
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
