//! Internet Archive items: list video files from archive.org's metadata API,
//! then download them over HTTPS from the item's `download/` path.

use super::{FetchSource, RemoteFile};
use crate::library::is_video;
use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;
use std::{collections::BTreeMap, path::Path, time::Duration};

const MAX_METADATA_BYTES: usize = 32 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ArchiveItem {
    pub identifier: String,
    pub title: String,
    /// Playable video files, sorted by name.
    pub files: Vec<ArchiveFile>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ArchiveFile {
    /// Path within the item, which may include folders.
    pub name: String,
    /// archive.org's format label, such as "h.264" or "MPEG2".
    pub format: String,
    pub size: Option<u64>,
    pub sha1: Option<String>,
    /// Uploaded with the item, rather than converted by archive.org.
    pub original: bool,
    /// Chosen by default: one copy of each video, preferring the original.
    pub preferred: bool,
}

impl ArchiveItem {
    /// HTTPS downloads for the files at `chosen` indices, in one folder named
    /// after the item.
    pub fn source(&self, chosen: impl IntoIterator<Item = usize>) -> Result<FetchSource> {
        let mut files = Vec::new();
        for index in chosen {
            let file = self.files.get(index).context("Unknown archive file")?;
            files.push(RemoteFile {
                url: download_url(&self.identifier, &file.name)?.into(),
                name: file.name.clone(),
                size: file.size,
                sha1: file.sha1.clone(),
            });
        }
        ensure!(!files.is_empty(), "Select at least one file to download");
        Ok(FetchSource::Files {
            folder: Some(if self.title.trim().is_empty() {
                self.identifier.clone()
            } else {
                self.title.clone()
            }),
            files,
        })
    }

    pub fn preferred(&self) -> impl Iterator<Item = usize> + '_ {
        self.files
            .iter()
            .enumerate()
            .filter(|(_, f)| f.preferred)
            .map(|(i, _)| i)
    }
}

/// Lists an item's video files from `https://archive.org/metadata/{id}`.
pub async fn resolve(identifier: &str) -> Result<ArchiveItem> {
    let client = reqwest::Client::builder()
        .https_only(true)
        .timeout(Duration::from_secs(20))
        .user_agent(concat!("Flick/", env!("CARGO_PKG_VERSION")))
        .build()?;
    resolve_from(&client, metadata_url(identifier)?, identifier).await
}

pub(crate) async fn resolve_from(
    client: &reqwest::Client,
    url: reqwest::Url,
    identifier: &str,
) -> Result<ArchiveItem> {
    let mut response = client
        .get(url)
        .send()
        .await
        .context("archive.org could not be reached")?
        .error_for_status()?;
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        ensure!(
            bytes.len() + chunk.len() <= MAX_METADATA_BYTES,
            "The item's file list is too large"
        );
        bytes.extend_from_slice(&chunk);
    }
    let value = serde_json::from_slice(&bytes).context("Invalid archive.org response")?;
    parse_metadata(identifier, value)
}

fn checked(identifier: &str) -> Result<&str> {
    let identifier = identifier.trim();
    ensure!(
        !identifier.is_empty()
            && identifier.len() <= 256
            && !identifier.contains(['/', '\\'])
            && identifier != "."
            && identifier != "..",
        "Invalid Internet Archive identifier"
    );
    Ok(identifier)
}

pub fn metadata_url(identifier: &str) -> Result<reqwest::Url> {
    let mut url = reqwest::Url::parse("https://archive.org/")?;
    url.path_segments_mut()
        .map_err(|_| anyhow::anyhow!("Invalid archive address"))?
        .extend(["metadata", checked(identifier)?]);
    Ok(url)
}

/// `https://archive.org/download/{id}/{name}`, encoding each path segment.
pub fn download_url(identifier: &str, name: &str) -> Result<reqwest::Url> {
    let segments = name.split('/').collect::<Vec<_>>();
    ensure!(
        segments
            .iter()
            .all(|s| !s.is_empty() && *s != "." && *s != ".."),
        "Unsafe archive file name: {name}"
    );
    let mut url = reqwest::Url::parse("https://archive.org/")?;
    url.path_segments_mut()
        .map_err(|_| anyhow::anyhow!("Invalid archive address"))?
        .extend(["download", checked(identifier)?])
        .extend(segments);
    Ok(url)
}

/// Selects playable videos from a metadata response. Files derived from the
/// same original form one group; each group's default is its original when
/// that is a video, otherwise its most widely playable derivative.
pub fn parse_metadata(identifier: &str, value: serde_json::Value) -> Result<ArchiveItem> {
    let object = value.as_object().context("Invalid archive.org response")?;
    if object.is_empty() {
        bail!("This Internet Archive item does not exist");
    }
    if object.get("is_dark").and_then(|v| v.as_bool()) == Some(true) {
        bail!("This Internet Archive item is not available");
    }
    let entries = object
        .get("files")
        .and_then(|v| v.as_array())
        .context("This Internet Archive item has no file list")?;
    let text = |value: &serde_json::Value, key: &str| {
        value.get(key).and_then(|v| match v {
            serde_json::Value::String(s) => Some(s.trim().to_owned()),
            serde_json::Value::Number(n) => Some(n.to_string()),
            _ => None,
        })
    };
    let mut files = Vec::new();
    let mut groups = BTreeMap::<String, Vec<usize>>::new();
    for entry in entries {
        let Some(name) = text(entry, "name").filter(|n| !n.is_empty()) else {
            continue;
        };
        let source = text(entry, "source").unwrap_or_default();
        if !matches!(source.as_str(), "original" | "derivative")
            || text(entry, "private").as_deref() == Some("true")
            || !is_video(Path::new(&name))
            || download_url(identifier, &name).is_err()
        {
            continue;
        }
        let original = source == "original";
        let group = if original {
            name.clone()
        } else {
            text(entry, "original").unwrap_or_else(|| name.clone())
        };
        groups
            .entry(stem(&group).to_owned())
            .or_default()
            .push(files.len());
        files.push(ArchiveFile {
            format: text(entry, "format").unwrap_or_default(),
            size: text(entry, "size").and_then(|s| s.parse().ok()),
            sha1: text(entry, "sha1").filter(|h| h.len() == 40),
            original,
            preferred: false,
            name,
        });
    }
    for members in groups.values() {
        if let Some(best) = members.iter().copied().min_by_key(|&i| {
            let file = &files[i];
            (
                !file.original,
                container_rank(&file.name),
                std::cmp::Reverse(file.size.unwrap_or(0)),
            )
        }) {
            files[best].preferred = true;
        }
    }
    ensure!(
        !files.is_empty(),
        "This Internet Archive item has no downloadable video files"
    );
    files.sort_by(|a, b| a.name.cmp(&b.name));
    let title = object
        .get("metadata")
        .and_then(|m| m.get("title"))
        .and_then(|t| match t {
            serde_json::Value::String(s) => Some(s.clone()),
            serde_json::Value::Array(values) => values.first()?.as_str().map(str::to_owned),
            _ => None,
        })
        .map(|t| t.trim().to_owned())
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| identifier.to_owned());
    Ok(ArchiveItem {
        identifier: identifier.to_owned(),
        title,
        files,
    })
}

/// Lower is more widely playable.
fn container_rank(name: &str) -> u8 {
    let extension = Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    match extension.as_str() {
        "mp4" | "m4v" => 0,
        "webm" | "mkv" | "mov" => 1,
        "ogv" => 2,
        _ => 3,
    }
}

fn stem(name: &str) -> &str {
    match name.rfind('.') {
        Some(dot) if dot > 0 && !name[dot..].contains('/') => &name[..dot],
        _ => name,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fetch::http::tests::{Behavior, serve};
    use std::sync::Arc;

    fn fixture() -> serde_json::Value {
        let sha1 = "a".repeat(40);
        serde_json::json!({
            "created": 1,
            "metadata": {"identifier": "film-1950", "title": ["Film (1950)", "Alt"], "mediatype": "movies"},
            "files": [
                {"name": "Film.mpeg", "source": "original", "format": "MPEG2", "size": "1000000", "sha1": sha1},
                {"name": "Film.mp4", "source": "derivative", "format": "h.264", "original": "Film.mpeg", "size": "300000"},
                {"name": "Film.ogv", "source": "derivative", "format": "Ogg Video", "original": "Film.mpeg", "size": "200000"},
                {"name": "Extras/Trailer one.avi", "source": "original", "format": "Cinepack", "size": 5000},
                {"name": "Extras/Trailer one.mp4", "source": "derivative", "format": "h.264", "original": "Extras/Trailer one.avi"},
                {"name": "Scan.iso", "source": "original", "format": "ISO Image"},
                {"name": "Scan.ogv", "source": "derivative", "format": "Ogg Video", "original": "Scan.iso", "size": "10"},
                {"name": "Scan.mp4", "source": "derivative", "format": "MPEG4", "original": "Scan.iso", "size": "20"},
                {"name": "Cover.jpg", "source": "original", "format": "JPEG"},
                {"name": "film-1950_archive.torrent", "source": "metadata", "format": "Archive BitTorrent"},
                {"name": "Locked.mp4", "source": "original", "format": "h.264", "private": "true"},
                {"name": "../escape.mp4", "source": "original", "format": "h.264"}
            ]
        })
    }

    #[test]
    fn metadata_selects_one_playable_copy_per_video_preferring_originals() {
        let item = parse_metadata("film-1950", fixture()).unwrap();
        assert_eq!(item.title, "Film (1950)");
        let listed = item
            .files
            .iter()
            .map(|f| (f.name.as_str(), f.preferred))
            .collect::<Vec<_>>();
        assert_eq!(
            listed,
            [
                ("Extras/Trailer one.avi", true),
                ("Extras/Trailer one.mp4", false),
                ("Film.mp4", false),
                ("Film.mpeg", true),
                ("Film.ogv", false),
                ("Scan.mp4", true),
                ("Scan.ogv", false),
            ]
        );
        let film = item.files.iter().find(|f| f.name == "Film.mpeg").unwrap();
        assert_eq!(film.size, Some(1_000_000));
        assert_eq!(film.sha1, Some("a".repeat(40)));
        assert!(film.original);
        assert_eq!(item.files[0].size, Some(5000));

        let FetchSource::Files { folder, files } = item.source(item.preferred()).unwrap() else {
            panic!("expected HTTPS files");
        };
        assert_eq!(folder.as_deref(), Some("Film (1950)"));
        assert_eq!(
            files.iter().map(|f| f.url.as_str()).collect::<Vec<_>>(),
            [
                "https://archive.org/download/film-1950/Extras/Trailer%20one.avi",
                "https://archive.org/download/film-1950/Film.mpeg",
                "https://archive.org/download/film-1950/Scan.mp4",
            ]
        );
        assert_eq!(files[1].sha1, Some("a".repeat(40)));
        assert!(item.source([]).is_err());
        assert!(item.source([99]).is_err());
    }

    #[test]
    fn missing_dark_or_videoless_items_are_explained() {
        let error = |value| parse_metadata("x", value).unwrap_err().to_string();
        assert!(error(serde_json::json!({})).contains("does not exist"));
        assert!(error(serde_json::json!({"is_dark": true})).contains("not available"));
        assert!(
            error(serde_json::json!({"files": [{"name": "a.pdf", "source": "original"}]}))
                .contains("no downloadable video")
        );
        assert!(error(serde_json::json!([])).contains("Invalid"));
        let untitled = parse_metadata(
            "plain",
            serde_json::json!({"files": [{"name": "a.webm", "source": "original"}]}),
        )
        .unwrap();
        assert_eq!(untitled.title, "plain");
        assert!(untitled.files[0].preferred);
    }

    #[test]
    fn archive_urls_keep_identifiers_and_names_in_their_segments() {
        let url = metadata_url("sample?x=1").unwrap();
        assert_eq!(url.host_str(), Some("archive.org"));
        assert!(url.query().is_none());
        assert_eq!(url.path(), "/metadata/sample%3Fx=1");
        assert_eq!(
            download_url("id", "a b/c#d.mp4").unwrap().as_str(),
            "https://archive.org/download/id/a%20b/c%23d.mp4"
        );
        for invalid in ["", "..", "a/b"] {
            assert!(metadata_url(invalid).is_err(), "{invalid}");
        }
        assert!(download_url("id", "../x.mp4").is_err());
        assert!(download_url("id", "a//x.mp4").is_err());
    }

    #[tokio::test]
    async fn resolving_reads_the_metadata_response() -> Result<()> {
        let body = Arc::new(fixture().to_string().into_bytes());
        let (origin, requests, server) = serve(body, Behavior::default()).await;
        let client = reqwest::Client::builder().no_proxy().build()?;
        let item = resolve_from(
            &client,
            reqwest::Url::parse(&format!("{origin}/metadata/film-1950"))?,
            "film-1950",
        )
        .await?;
        server.abort();
        assert_eq!(item.files.len(), 7);
        assert!(requests.lock().unwrap()[0].starts_with("GET /metadata/film-1950 "));
        Ok(())
    }
}
