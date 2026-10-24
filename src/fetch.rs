//! Downloads from web links, separate from GPUI, playback, and the torrent
//! session. Video pages such as YouTube or X posts use an installed yt-dlp
//! process. Direct files and Internet Archive items download over HTTPS.
//!
//! Run jobs on a background Tokio runtime. A job reports typed updates. When
//! it fails or its cancellation token fires, it removes the partial files it
//! created before returning. Finished files are never overwritten.

pub mod archive;
mod http;
pub mod ytdlp;

use anyhow::{Context, Result, ensure};
use serde::Serialize;
use std::path::{Path, PathBuf};
use tokio_util::sync::CancellationToken;

/// What a link download fetches.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub enum FetchSource {
    /// A pasted link. An HTTPS address ending in a video file name is first
    /// tried as a direct download. Other pages, and addresses that answer with
    /// a web page, go to yt-dlp, which understands sites such as YouTube or X.
    Page(String),
    /// Files over HTTPS, optionally inside a new subfolder of the destination.
    Files {
        folder: Option<String>,
        files: Vec<RemoteFile>,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RemoteFile {
    pub url: String,
    /// Suggested name. Only its last component is used, made safe.
    pub name: String,
    pub size: Option<u64>,
    /// Hex SHA-1 published by the source, checked after downloading.
    pub sha1: Option<String>,
}

/// A pasted http(s) link, by how Flick downloads it.
#[derive(Clone, Debug, PartialEq)]
pub enum WebLink {
    /// An Internet Archive item page. List its files before downloading.
    Archive(String),
    /// Any other link. See [`FetchSource::Page`].
    Page(String),
}

impl WebLink {
    /// The download for this link, except Archive items, which need a listing.
    pub fn source(&self) -> Option<FetchSource> {
        match self {
            Self::Archive(_) => None,
            Self::Page(url) => Some(FetchSource::Page(url.clone())),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub enum FetchUpdate {
    /// The video or item title, once known.
    Title(String),
    Progress(FetchProgress),
    /// A finished file in its final location. A job can save several.
    Saved(PathBuf),
    /// The job saved everything it was asked for.
    Done,
    Failed(String),
    /// Stopped on request. Partial files were removed.
    Cancelled,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize)]
pub struct FetchProgress {
    pub stage: FetchStage,
    /// One-based file position and count. Zero when unknown.
    pub file_index: usize,
    pub file_count: usize,
    pub downloaded_bytes: u64,
    pub total_bytes: Option<u64>,
    pub bytes_per_second: Option<u64>,
    pub eta_seconds: Option<u64>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub enum FetchStage {
    #[default]
    Starting,
    Downloading,
    /// yt-dlp is merging or moving files.
    Processing,
    /// Checking a finished file against its published checksum.
    Verifying,
}

/// Returned when a job stops because its cancellation token fired.
#[derive(Debug)]
pub struct Cancelled;
impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Download cancelled")
    }
}
impl std::error::Error for Cancelled {}

/// Downloads `source` into `folder`, returning the saved files.
pub async fn fetch(
    source: FetchSource,
    folder: &Path,
    cancel: &CancellationToken,
    mut report: impl FnMut(FetchUpdate) + Send,
) -> Result<Vec<PathBuf>> {
    match source {
        FetchSource::Page(url) => {
            let url = page_url(&url)?;
            let direct = direct_file(&url);
            let client = http::client()?;
            fetch_page(
                &client,
                direct,
                ytdlp::Tools::discover,
                &url,
                folder,
                cancel,
                &mut report,
            )
            .await
        }
        FetchSource::Files {
            folder: name,
            files,
        } => {
            for file in &files {
                let url = reqwest::Url::parse(&file.url).context("Invalid download address")?;
                ensure!(url.scheme() == "https", "Direct downloads require HTTPS");
            }
            let folder = match name {
                Some(name) => {
                    folder.join(safe_file_name(&name).context("This download has no usable name")?)
                }
                None => folder.to_owned(),
            };
            http::download_all(&http::client()?, &files, &folder, cancel, &mut report).await
        }
    }
}

async fn fetch_page(
    client: &reqwest::Client,
    direct: Option<RemoteFile>,
    tools: impl FnOnce() -> Result<ytdlp::Tools>,
    url: &reqwest::Url,
    folder: &Path,
    cancel: &CancellationToken,
    report: &mut (impl FnMut(FetchUpdate) + Send),
) -> Result<Vec<PathBuf>> {
    if let Some(file) = direct {
        report(FetchUpdate::Title(file.name.clone()));
        match http::download_all(client, std::slice::from_ref(&file), folder, cancel, report).await
        {
            Err(error) if error.is::<http::WebPage>() => {}
            result => return result,
        }
    }
    ytdlp::download(&tools()?, url, folder, cancel, report).await
}

/// An HTTPS address whose last path segment names a video file.
fn direct_file(url: &reqwest::Url) -> Option<RemoteFile> {
    if url.scheme() != "https" {
        return None;
    }
    let name = percent_decode(url.path_segments()?.rfind(|s| !s.is_empty())?);
    crate::library::is_video(Path::new(&name)).then(|| RemoteFile {
        url: url.to_string(),
        name,
        size: None,
        sha1: None,
    })
}

/// Classifies text that starts with `http://` or `https://`. Other text,
/// including magnet links and search terms, returns `None`.
pub fn web_link(text: &str) -> Option<Result<WebLink>> {
    let text = text.trim();
    let scheme = text.split_once("://")?.0;
    if !(scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https")) {
        return None;
    }
    Some(page_url(text).map(classify))
}

fn classify(url: reqwest::Url) -> WebLink {
    let segments = url
        .path_segments()
        .map(|s| s.filter(|s| !s.is_empty()).collect::<Vec<_>>())
        .unwrap_or_default();
    if matches!(url.host_str(), Some("archive.org" | "www.archive.org"))
        && url.scheme() == "https"
        && let ["details", id, ..] | ["download", id] = segments.as_slice()
    {
        return WebLink::Archive(percent_decode(id));
    }
    WebLink::Page(url.to_string())
}

/// A validated http(s) address with a host and no embedded credentials.
pub fn page_url(text: &str) -> Result<reqwest::Url> {
    let url = reqwest::Url::parse(text.trim())
        .context("Enter a full web address, starting with https://")?;
    ensure!(
        matches!(url.scheme(), "http" | "https"),
        "Only http and https links can be downloaded"
    );
    ensure!(
        url.host_str().is_some_and(|host| !host.is_empty()),
        "This link has no website address"
    );
    ensure!(
        url.username().is_empty() && url.password().is_none(),
        "Remove the user name and password from this link"
    );
    Ok(url)
}

/// A name confined to one folder: the last path component, without
/// separators, control or reserved characters, leading dots, or Windows
/// device names, and at most 180 bytes.
pub fn safe_file_name(name: &str) -> Option<String> {
    let base = name.rsplit(['/', '\\']).next().unwrap_or_default();
    let cleaned = base
        .chars()
        .map(|c| {
            if c.is_control() || r#"<>:"/\|?*"#.contains(c) {
                '_'
            } else {
                c
            }
        })
        .collect::<String>();
    let cleaned = cleaned
        .trim()
        .trim_start_matches('.')
        .trim_end_matches(['.', ' '])
        .trim();
    if cleaned.is_empty() {
        return None;
    }
    let device = cleaned
        .split('.')
        .next()
        .unwrap_or_default()
        .trim_end()
        .to_ascii_uppercase();
    let reserved = matches!(device.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (device.len() == 4
            && (device.starts_with("COM") || device.starts_with("LPT"))
            && device.as_bytes()[3].is_ascii_digit());
    let name = if reserved {
        format!("_{cleaned}")
    } else {
        cleaned.to_owned()
    };
    Some(truncate_name(&name, 180))
}

/// Splits `name` into a stem and a short extension that includes its dot.
fn split_extension(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(dot) if dot > 0 && name.len() - dot <= 16 => name.split_at(dot),
        _ => (name, ""),
    }
}

fn truncate_name(name: &str, max: usize) -> String {
    if name.len() <= max {
        return name.to_owned();
    }
    let (stem, extension) = split_extension(name);
    let mut end = max.saturating_sub(extension.len());
    while !stem.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{extension}", stem[..end].trim_end())
}

/// `name`, then `name (1)`, `name (2)`, … keeping the extension.
fn numbered(name: &str, n: usize) -> String {
    if n == 0 {
        return name.to_owned();
    }
    let (stem, extension) = split_extension(name);
    format!("{stem} ({n}){extension}")
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        if bytes[i] == b'%'
            && let (Some(high), Some(low)) = (
                bytes.get(i + 1).copied().and_then(hex),
                bytes.get(i + 2).copied().and_then(hex),
            )
        {
            out.push((high * 16 + low) as u8);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link(text: &str) -> WebLink {
        web_link(text).unwrap().unwrap()
    }

    #[test]
    fn web_links_route_archive_items_and_pages() {
        for page in [
            "https://www.youtube.com/watch?v=aqz-KE-bpKQ",
            "https://x.com/someone/status/1234567890",
            " HTTPS://vimeo.com/1084537 ",
            "http://example.com/clip.mp4",
            "https://archive.org/search?query=film",
            "https://archive.org/download/item/Film.mp4",
        ] {
            assert!(matches!(link(page), WebLink::Page(_)), "{page}");
        }
        assert_eq!(
            link("https://archive.org/details/night_of_the_living_dead"),
            WebLink::Archive("night_of_the_living_dead".into())
        );
        assert_eq!(
            link("https://archive.org/download/some%20item/"),
            WebLink::Archive("some item".into())
        );
        assert!(link("https://archive.org/details/x").source().is_none());
        assert_eq!(
            link("https://x.com/a/status/1").source(),
            Some(FetchSource::Page("https://x.com/a/status/1".into()))
        );
        for other in [
            "magnet:?xt=urn:btih:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "big buck bunny",
            "ftp://example.com/file.mp4",
            "file:///etc/passwd",
            "-x",
        ] {
            assert!(web_link(other).is_none(), "{other}");
        }
        for invalid in ["https://", "https://user:secret@example.com/video"] {
            assert!(web_link(invalid).unwrap().is_err(), "{invalid}");
        }
    }

    #[test]
    fn only_https_addresses_ending_in_a_video_name_are_tried_directly() {
        let direct = |text: &str| direct_file(&page_url(text).unwrap()).map(|f| f.name);
        assert_eq!(
            direct("https://archive.org/download/item/folder/Film%20One.mp4").as_deref(),
            Some("Film One.mp4")
        );
        assert_eq!(
            direct("https://cdn.example.com/media/clip.WEBM/?token=1").as_deref(),
            Some("clip.WEBM")
        );
        for other in [
            "http://example.com/clip.mp4",
            "https://www.youtube.com/watch?v=x",
            "https://example.com/",
        ] {
            assert_eq!(direct(other), None, "{other}");
        }
    }

    #[tokio::test]
    async fn direct_video_links_download_without_yt_dlp() -> Result<()> {
        use http::tests::{Behavior, content, serve};
        let body = content(10_000);
        let (origin, _, server) = serve(
            body.clone(),
            Behavior {
                content_type: Some("video/mp4"),
                ..Default::default()
            },
        )
        .await;
        let url = page_url(&format!("{origin}/media/clip.mp4"))?;
        let folder = tempfile::tempdir()?;
        let direct = RemoteFile {
            url: url.to_string(),
            name: "clip.mp4".into(),
            size: None,
            sha1: None,
        };
        let client = reqwest::Client::builder().no_proxy().build()?;
        let saved = fetch_page(
            &client,
            Some(direct),
            || anyhow::bail!("yt-dlp must not run"),
            &url,
            folder.path(),
            &CancellationToken::new(),
            &mut |_| {},
        )
        .await?;
        server.abort();
        assert_eq!(std::fs::read(&saved[0])?, *body);
        Ok(())
    }

    /// A page whose address ends like a video file, such as a wiki file page.
    #[cfg(unix)]
    #[tokio::test]
    async fn web_pages_at_video_addresses_fall_back_to_yt_dlp() -> Result<()> {
        use http::tests::{Behavior, serve};
        let (origin, _, server) = serve(
            std::sync::Arc::new(b"<html>a page</html>".to_vec()),
            Behavior {
                content_type: Some("text/html; charset=UTF-8"),
                ..Default::default()
            },
        )
        .await;
        let url = page_url(&format!("{origin}/wiki/File:clip.webm"))?;
        let root = tempfile::tempdir()?;
        let folder = root.path().join("downloads");
        let program = ytdlp::tests::process::stand_in(
            root.path(),
            r#"printf 'video' > "$home/clip [x].webm"
printf 'flick-file "%s"\n' "$home/clip [x].webm""#,
        );
        let client = reqwest::Client::builder().no_proxy().build()?;
        let direct = RemoteFile {
            url: url.to_string(),
            name: "File:clip.webm".into(),
            size: None,
            sha1: None,
        };
        let saved = fetch_page(
            &client,
            Some(direct),
            || {
                Ok(ytdlp::Tools {
                    ytdlp: program,
                    ffmpeg: None,
                })
            },
            &url,
            &folder,
            &CancellationToken::new(),
            &mut |_| {},
        )
        .await?;
        server.abort();
        assert_eq!(saved, vec![folder.canonicalize()?.join("clip [x].webm")]);
        let mut names = std::fs::read_dir(&folder)?
            .map(|e| e.map(|e| e.file_name()))
            .collect::<std::io::Result<Vec<_>>>()?;
        names.sort();
        assert_eq!(names, ["clip [x].webm"]);
        Ok(())
    }

    #[test]
    fn page_urls_must_be_http_with_a_host() {
        assert!(page_url("https://example.com/watch").is_ok());
        for invalid in [
            "javascript:alert(1)",
            "file:///etc/passwd",
            "ftp://example.com/x",
            "--exec=rm",
            "-x",
            "",
        ] {
            assert!(page_url(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn safe_file_names_stay_inside_one_folder() {
        for (raw, expected) in [
            ("../../etc/passwd", Some("passwd")),
            ("folder/sub\\clip.mp4", Some("clip.mp4")),
            ("CON.mp4", Some("_CON.mp4")),
            ("lpt1", Some("_lpt1")),
            ("COM10.mp4", Some("COM10.mp4")),
            ("  ..hidden.mp4. ", Some("hidden.mp4")),
            ("a<b>:c\"d|e?f*.mkv", Some("a_b__c_d_e_f_.mkv")),
            ("tab\there.mp4", Some("tab_here.mp4")),
            ("...", None),
            ("dir/", None),
            ("", None),
        ] {
            assert_eq!(safe_file_name(raw).as_deref(), expected, "{raw}");
        }
        let long = format!("{}.mp4", "é".repeat(200));
        let short = safe_file_name(&long).unwrap();
        assert!(short.len() <= 180 && short.ends_with(".mp4"));
        assert!(short.trim_end_matches(".mp4").chars().all(|c| c == 'é'));
    }

    #[test]
    fn numbered_names_keep_extensions() {
        assert_eq!(numbered("clip.mp4", 0), "clip.mp4");
        assert_eq!(numbered("clip.mp4", 2), "clip (2).mp4");
        assert_eq!(numbered("README", 1), "README (1)");
        assert_eq!(numbered("archive.tar.gz", 1), "archive.tar (1).gz");
    }
}
