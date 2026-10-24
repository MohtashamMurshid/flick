//! OpenSubtitles search and download through its REST API (v1).
//!
//! The API needs a personal key from opensubtitles.com, which the user adds
//! in Appearance or with `FLICK_OPENSUBTITLES_KEY`. Searches send the
//! video's title and its OpenSubtitles hash, which identifies exact
//! releases. Nothing here touches playback; the desktop loads a saved file
//! with mpv's `sub-add`.

use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    time::Duration,
};

pub const API: &str = "https://api.opensubtitles.com/api/v1";

/// Larger responses are refused; subtitle files are small.
const MAX_RESPONSE: usize = 8 * 1024 * 1024;
/// The OpenSubtitles hash reads this many bytes from each end of a file.
const HASH_CHUNK: u64 = 64 * 1024;

#[derive(Clone, Debug, PartialEq)]
pub struct SubtitleResult {
    pub file_id: u64,
    pub file_name: String,
    /// ISO 639-1 code, such as `en`.
    pub language: String,
    pub release: String,
    pub downloads: u64,
    /// Matched by file hash, so it was made for this exact release.
    pub hash_match: bool,
    pub hearing_impaired: bool,
}

pub struct Client {
    http: reqwest::Client,
    base: reqwest::Url,
    key: String,
}

impl Client {
    pub fn new(key: &str) -> Result<Self> {
        let http = reqwest::Client::builder()
            .https_only(true)
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(30))
            // OpenSubtitles asks clients to identify themselves.
            .user_agent(concat!("Flick v", env!("CARGO_PKG_VERSION")))
            .build()
            .context("Could not prepare subtitle searches")?;
        Self::with_base(http, API, key)
    }

    fn with_base(http: reqwest::Client, base: &str, key: &str) -> Result<Self> {
        let key = key.trim();
        ensure!(
            !key.is_empty(),
            "Add an OpenSubtitles API key in Appearance to search for subtitles"
        );
        let mut base = reqwest::Url::parse(base).context("Invalid OpenSubtitles address")?;
        if !base.path().ends_with('/') {
            base.set_path(&format!("{}/", base.path()));
        }
        Ok(Self {
            http,
            base,
            key: key.to_owned(),
        })
    }

    /// Subtitles for `query` and, when known, the video's hash, in the
    /// given comma-separated languages. Hash matches come first.
    pub async fn search(
        &self,
        query: &str,
        hash: Option<&str>,
        languages: &str,
    ) -> Result<Vec<SubtitleResult>> {
        let mut url = self.base.join("subtitles")?;
        {
            // The API caches better with sorted, lowercase parameters.
            let mut pairs = url.query_pairs_mut();
            let languages = normalize_languages(languages);
            if !languages.is_empty() {
                pairs.append_pair("languages", &languages);
            }
            if let Some(hash) = hash {
                pairs.append_pair("moviehash", hash);
            }
            pairs.append_pair("order_by", "download_count");
            let query = query.trim().to_lowercase();
            if !query.is_empty() {
                pairs.append_pair("query", &query);
            }
        }
        let response = self
            .http
            .get(url)
            .header("Api-Key", &self.key)
            .header("Accept", "application/json")
            .send()
            .await
            .context("Could not reach OpenSubtitles")?;
        let mut results = parse_results(&json_body(response).await?)?;
        results.sort_by_key(|r| (!r.hash_match, std::cmp::Reverse(r.downloads)));
        Ok(results)
    }

    /// Downloads a subtitle into `folder`, named after the video and
    /// language, without overwriting an existing file.
    pub async fn download(
        &self,
        result: &SubtitleResult,
        folder: &Path,
        video_stem: &str,
    ) -> Result<PathBuf> {
        let response = self
            .http
            .post(self.base.join("download")?)
            .header("Api-Key", &self.key)
            .header("Accept", "application/json")
            .json(&json!({ "file_id": result.file_id }))
            .send()
            .await
            .context("Could not reach OpenSubtitles")?;
        let body = json_body(response).await?;
        let link = body["link"]
            .as_str()
            .context("OpenSubtitles did not return a download link")?;
        let link = reqwest::Url::parse(link).context("OpenSubtitles returned an invalid link")?;
        let extension = Path::new(body["file_name"].as_str().unwrap_or(&result.file_name))
            .extension()
            .and_then(|e| e.to_str())
            .filter(|e| {
                matches!(
                    e.to_ascii_lowercase().as_str(),
                    "srt" | "ass" | "ssa" | "vtt" | "sub"
                )
            })
            .unwrap_or("srt")
            .to_ascii_lowercase();
        let bytes = limited_body(
            self.http
                .get(link)
                .send()
                .await
                .context("Could not download the subtitle")?,
        )
        .await?;
        ensure!(!bytes.is_empty(), "The subtitle file was empty");
        let language =
            crate::fetch::safe_file_name(&result.language).unwrap_or_else(|| "sub".into());
        let stem = crate::fetch::safe_file_name(video_stem).unwrap_or_else(|| "subtitles".into());
        write_new(folder, &stem, &language, &extension, &bytes)
    }
}

/// `en, PT-br,fr` → `en,fr,pt-br`.
fn normalize_languages(languages: &str) -> String {
    let mut codes = languages
        .split(',')
        .map(|code| code.trim().to_lowercase())
        .filter(|code| {
            !code.is_empty() && code.chars().all(|c| c.is_ascii_alphabetic() || c == '-')
        })
        .collect::<Vec<_>>();
    codes.sort();
    codes.dedup();
    codes.join(",")
}

async fn limited_body(response: reqwest::Response) -> Result<Vec<u8>> {
    let status = response.status();
    if let Some(length) = response.content_length() {
        ensure!(
            length as usize <= MAX_RESPONSE,
            "The response was too large"
        );
    }
    let bytes = response
        .bytes()
        .await
        .context("Could not read the response")?;
    ensure!(bytes.len() <= MAX_RESPONSE, "The response was too large");
    if !status.is_success() {
        let message = serde_json::from_slice::<Value>(&bytes)
            .ok()
            .and_then(|v| v["message"].as_str().map(str::to_owned));
        bail!(match (status.as_u16(), message) {
            (401 | 403, _) => "OpenSubtitles rejected the API key. Check it in Appearance.".into(),
            (_, Some(message)) => format!("OpenSubtitles: {message}"),
            (code, None) => format!("OpenSubtitles answered with HTTP {code}"),
        });
    }
    Ok(bytes.to_vec())
}

async fn json_body(response: reqwest::Response) -> Result<Value> {
    serde_json::from_slice(&limited_body(response).await?)
        .context("OpenSubtitles returned an unexpected response")
}

/// Results from a `/subtitles` response; each file is its own result.
pub fn parse_results(body: &Value) -> Result<Vec<SubtitleResult>> {
    let data = body["data"]
        .as_array()
        .context("OpenSubtitles returned an unexpected response")?;
    let mut results = Vec::new();
    for item in data {
        let attributes = &item["attributes"];
        for file in attributes["files"].as_array().into_iter().flatten() {
            let Some(file_id) = file["file_id"].as_u64() else {
                continue;
            };
            results.push(SubtitleResult {
                file_id,
                file_name: file["file_name"].as_str().unwrap_or_default().to_owned(),
                language: attributes["language"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
                release: attributes["release"]
                    .as_str()
                    .filter(|r| !r.is_empty())
                    .or_else(|| attributes["feature_details"]["title"].as_str())
                    .unwrap_or("Subtitles")
                    .to_owned(),
                downloads: attributes["download_count"].as_u64().unwrap_or_default(),
                hash_match: attributes["moviehash_match"].as_bool().unwrap_or(false),
                hearing_impaired: attributes["hearing_impaired"].as_bool().unwrap_or(false),
            });
        }
    }
    Ok(results)
}

/// Writes `stem.language.extension`, or `stem.language (2).extension` and
/// so on, never replacing an existing file.
fn write_new(
    folder: &Path,
    stem: &str,
    language: &str,
    extension: &str,
    bytes: &[u8],
) -> Result<PathBuf> {
    use std::io::Write;
    std::fs::create_dir_all(folder)
        .with_context(|| format!("Could not create {}", folder.display()))?;
    for n in 1..1000 {
        let name = if n == 1 {
            format!("{stem}.{language}.{extension}")
        } else {
            format!("{stem}.{language} ({n}).{extension}")
        };
        let path = folder.join(name);
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut file) => {
                file.write_all(bytes)
                    .context("Could not save the subtitle")?;
                return Ok(path);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error).context("Could not save the subtitle"),
        }
    }
    bail!("Too many subtitle files with this name")
}

/// The OpenSubtitles hash: the file size plus the little-endian 64-bit
/// words of its first and last 64 KiB, wrapping, as 16 hex digits.
pub fn movie_hash(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path).context("Open the video")?;
    let size = file.metadata()?.len();
    ensure!(size >= HASH_CHUNK, "The video is too small to identify");
    let mut hash = size;
    let mut add = |file: &mut std::fs::File, from: u64| -> Result<()> {
        file.seek(SeekFrom::Start(from))?;
        let mut chunk = vec![0; HASH_CHUNK as usize];
        file.read_exact(&mut chunk)?;
        for word in chunk.as_chunks::<8>().0 {
            hash = hash.wrapping_add(u64::from_le_bytes(*word));
        }
        Ok(())
    };
    add(&mut file, 0)?;
    add(&mut file, size - HASH_CHUNK)?;
    Ok(format!("{hash:016x}"))
}

/// A search phrase from a file name: `The.Movie.2019.1080p.mkv` → `The Movie 2019`.
pub fn query_from_title(title: &str) -> String {
    let stem = Path::new(title)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(title);
    let mut words = Vec::new();
    for word in stem.split(['.', '_', ' ']) {
        let lower = word.to_ascii_lowercase();
        // Stop at release details such as resolutions and sources.
        if [
            "1080p", "2160p", "720p", "480p", "4k", "bluray", "web-dl", "webrip", "hdtv", "x264",
            "x265", "hevc",
        ]
        .contains(&lower.as_str())
        {
            break;
        }
        if !word.is_empty() {
            words.push(word);
        }
    }
    words.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn movie_hash_adds_size_and_both_ends() -> Result<()> {
        let root = tempfile::tempdir()?;
        let path = root.path().join("clip.mkv");
        let mut bytes = vec![0u8; 1024 * 1024];
        bytes[..8].copy_from_slice(&1u64.to_le_bytes());
        let end = bytes.len() - 8;
        bytes[end..].copy_from_slice(&2u64.to_le_bytes());
        std::fs::write(&path, &bytes)?;
        assert_eq!(movie_hash(&path)?, format!("{:016x}", 1024 * 1024 + 3));
        std::fs::write(&path, [0; 100])?;
        assert!(movie_hash(&path).is_err());
        Ok(())
    }

    #[test]
    fn queries_and_languages_are_cleaned() {
        assert_eq!(
            query_from_title("The.Movie.2019.1080p.BluRay.mkv"),
            "The Movie 2019"
        );
        assert_eq!(query_from_title("Night train.mp4"), "Night train");
        assert_eq!(normalize_languages(" en, PT-br,fr,en, x1"), "en,fr,pt-br");
        assert!(Client::with_base(reqwest::Client::new(), API, "  ").is_err());
    }

    #[test]
    fn results_parse_files_and_put_hash_matches_first() -> Result<()> {
        let body = json!({"data": [
            {"attributes": {"language": "en", "release": "Popular", "download_count": 900,
                "files": [{"file_id": 1, "file_name": "a.srt"}]}},
            {"attributes": {"language": "en", "release": "", "download_count": 5, "moviehash_match": true,
                "feature_details": {"title": "Exact"}, "files": [{"file_id": 2, "file_name": "b.srt"}, {"file_name": "no id"}]}}
        ]});
        let results = parse_results(&body)?;
        assert_eq!(results.len(), 2);
        assert_eq!(results[1].release, "Exact");
        assert!(results[1].hash_match);
        assert!(parse_results(&json!({"errors": []})).is_err());
        Ok(())
    }

    /// A loopback stand-in for the API: search, download link, and file.
    async fn api() -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let log = Arc::new(Mutex::new(Vec::new()));
        let requests = log.clone();
        let link = format!("{origin}/file/b.srt");
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let log = log.clone();
                let link = link.clone();
                tokio::spawn(async move {
                    let mut buffer = vec![0; 16 * 1024];
                    let read = stream.read(&mut buffer).await.unwrap_or(0);
                    let request = String::from_utf8_lossy(&buffer[..read]).to_string();
                    log.lock().unwrap().push(request.clone());
                    let line = request.lines().next().unwrap_or_default().to_owned();
                    // Download links are temporary and need no key.
                    let key_ok = request.to_ascii_lowercase().contains("api-key: good-key")
                        || line.starts_with("GET /file/");
                    let (status, body) = if !key_ok {
                        (
                            "401 Unauthorized",
                            json!({"message": "bad key"}).to_string(),
                        )
                    } else if line.starts_with("GET /api/v1/subtitles") {
                        ("200 OK", json!({"data": [{"attributes": {"language": "en", "release": "Exact", "moviehash_match": true,
                            "files": [{"file_id": 7, "file_name": "b.srt"}]}}]}).to_string())
                    } else if line.starts_with("POST /api/v1/download") {
                        (
                            "200 OK",
                            json!({"link": link, "file_name": "b.srt"}).to_string(),
                        )
                    } else if line.starts_with("GET /file/") {
                        ("200 OK", "1\n00:00:00,000 --> 00:00:02,000\nHello\n".into())
                    } else {
                        ("404 Not Found", "{}".into())
                    };
                    let response = format!(
                        "HTTP/1.1 {status}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                });
            }
        });
        (origin, requests)
    }

    #[tokio::test]
    async fn searches_and_downloads_against_a_local_stand_in() -> Result<()> {
        let (origin, requests) = api().await;
        let base = format!("{origin}/api/v1");
        let client = Client::with_base(reqwest::Client::new(), &base, "good-key")?;
        let results = client
            .search("The Movie", Some("0123456789abcdef"), "fr,en")
            .await?;
        assert_eq!(results.len(), 1);
        let search = requests.lock().unwrap()[0].clone();
        assert!(search.contains("languages=en%2Cfr"));
        assert!(search.contains("moviehash=0123456789abcdef"));
        assert!(search.contains("query=the+movie"));
        let root = tempfile::tempdir()?;
        let first = client
            .download(&results[0], root.path(), "../Movie")
            .await?;
        assert_eq!(first, root.path().join("Movie.en.srt"));
        assert!(std::fs::read_to_string(&first)?.contains("Hello"));
        // A second download keeps the first.
        let second = client.download(&results[0], root.path(), "Movie").await?;
        assert_eq!(second, root.path().join("Movie.en (2).srt"));
        let bad = Client::with_base(reqwest::Client::new(), &base, "wrong")?;
        let error = bad.search("x", None, "en").await.unwrap_err().to_string();
        assert!(error.contains("rejected the API key"), "{error}");
        Ok(())
    }
}
