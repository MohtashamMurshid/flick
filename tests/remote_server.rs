//! The remote library server on loopback, with generated fixtures in
//! temporary folders. Nothing here reaches beyond 127.0.0.1.

use anyhow::{Context, Result, ensure};
use flick::remote::{
    auth,
    protocol::{self, ErrorBody, Library, Pairing, Progress, Quality, SubtitleFile, paths},
    server::{Server, ServerConfig, Sources},
};
use reqwest::{StatusCode, header};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};
use tempfile::TempDir;
use tokio::time::timeout;

const DEADLINE: Duration = Duration::from_secs(30);
const VIDEO_SIZE: usize = 4 * 1024 * 1024 + 123;

/// Bytes that differ at every offset in a 251-byte cycle, so a misplaced
/// range is visible.
fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

struct Fixture {
    _root: TempDir,
    media: PathBuf,
    state: PathBuf,
    token: String,
}

impl Fixture {
    fn new() -> Result<Self> {
        let root = tempfile::tempdir()?;
        let media = root.path().join("media");
        let show = media.join("The Show").join("Season 1");
        std::fs::create_dir_all(&show)?;
        std::fs::write(media.join("Film.mp4"), pattern(VIDEO_SIZE))?;
        std::fs::write(
            media.join("Film.en.srt"),
            b"1\n00:00:01,000 --> 00:00:02,000\nHi\n",
        )?;
        std::fs::write(
            media.join("Film.srt"),
            b"1\n00:00:01,000 --> 00:00:02,000\nHey\n",
        )?;
        std::fs::write(media.join("notes.txt"), b"not a video")?;
        std::fs::write(show.join("The.Show.S01E02.mkv"), pattern(2048))?;
        let state = root.path().join("state");
        let token = auth::load_or_create_token(&state, false)?;
        Ok(Self {
            media,
            state,
            token,
            _root: root,
        })
    }

    fn config(&self, sources: Sources) -> ServerConfig {
        ServerConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            token: self.token.clone(),
            name: "Test Mini".into(),
            sources,
            poster_cache: None,
            state_dir: self.state.clone(),
            // A path that is not FFmpeg keeps the test off the real one.
            ffmpeg: Some(self.state.join("no-ffmpeg")),
            max_transcodes: 2,
        }
    }

    async fn start(&self) -> Result<Running> {
        self.start_with(Sources::Folders(vec![self.media.clone()]))
            .await
    }

    async fn start_with(&self, sources: Sources) -> Result<Running> {
        let server = Server::start(self.config(sources)).await?;
        Ok(Running {
            base: format!("http://{}", server.addr()),
            token: self.token.clone(),
            client: reqwest::Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(20))
                .build()?,
            server,
        })
    }
}

struct Running {
    server: Server,
    base: String,
    token: String,
    client: reqwest::Client,
}

impl Running {
    fn get(&self, path: &str) -> reqwest::RequestBuilder {
        self.request(reqwest::Method::GET, path)
    }

    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        self.client
            .request(method, format!("{}{path}", self.base))
            .header(header::AUTHORIZATION, protocol::bearer(&self.token))
    }

    async fn library(&self) -> Result<Library> {
        let response = self.get(&paths::library()).send().await?;
        ensure!(response.status() == StatusCode::OK, "{}", response.status());
        Ok(response.json().await?)
    }

    async fn film_id(&self) -> Result<String> {
        Ok(self
            .library()
            .await?
            .videos
            .into_iter()
            .find(|v| v.file_name == "Film.mp4")
            .context("Film.mp4 is listed")?
            .id)
    }
}

async fn error_message(response: reqwest::Response) -> Result<String> {
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    ensure!(
        content_type.starts_with("application/json"),
        "error is JSON: {content_type}"
    );
    Ok(response.json::<ErrorBody>().await?.message)
}

fn header_value(response: &reqwest::Response, name: header::HeaderName) -> String {
    response
        .headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn requests_without_the_token_are_refused() -> Result<()> {
    timeout(DEADLINE, async {
        let fixture = Fixture::new()?;
        let running = fixture.start().await?;
        let url = format!("{}{}", running.base, paths::library());
        for (authorization, label) in [
            (None, "missing"),
            (Some(protocol::bearer(&auth::generate_token())), "wrong"),
            (Some(format!("Basic {}", running.token)), "wrong scheme"),
            (Some("Bearer ".to_owned()), "empty"),
        ] {
            let mut request = running.client.get(&url);
            if let Some(value) = authorization {
                request = request.header(header::AUTHORIZATION, value);
            }
            let response = request.send().await?;
            ensure!(
                response.status() == StatusCode::UNAUTHORIZED,
                "{label}: {}",
                response.status()
            );
            ensure!(header_value(&response, header::WWW_AUTHENTICATE) == "Bearer");
            ensure!(!error_message(response).await?.is_empty());
        }
        // Unknown routes and media are behind the token too.
        for path in ["/nowhere", &paths::media(&"a".repeat(40))] {
            let response = running
                .client
                .get(format!("{}{path}", running.base))
                .send()
                .await?;
            ensure!(response.status() == StatusCode::UNAUTHORIZED, "{path}");
        }
        let response = running.get("/nowhere").send().await?;
        ensure!(response.status() == StatusCode::NOT_FOUND);
        error_message(response).await?;
        running.server.shutdown().await;
        Ok(())
    })
    .await?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unknown_ids_are_not_found() -> Result<()> {
    timeout(DEADLINE, async {
        let fixture = Fixture::new()?;
        let running = fixture.start().await?;
        let unknown = "0".repeat(40);
        for path in [
            paths::media(&unknown),
            paths::media("not-an-id"),
            paths::media("..%2F..%2Fetc%2Fpasswd"),
            paths::poster(&unknown),
            paths::subtitles(&unknown),
            paths::subtitle(&unknown, 0),
            paths::hash(&unknown),
            paths::progress(&unknown),
        ] {
            let response = running.get(&path).send().await?;
            ensure!(
                response.status() == StatusCode::NOT_FOUND,
                "{path}: {}",
                response.status()
            );
            error_message(response).await?;
        }
        let response = running
            .request(reqwest::Method::DELETE, &paths::library())
            .send()
            .await?;
        ensure!(response.status() == StatusCode::METHOD_NOT_ALLOWED);
        error_message(response).await?;
        running.server.shutdown().await;
        Ok(())
    })
    .await?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn listing_describes_videos_with_ids_stable_across_restarts() -> Result<()> {
    timeout(DEADLINE, async {
        let fixture = Fixture::new()?;
        let running = fixture.start().await?;
        let library = running.library().await?;
        ensure!(library.protocol == protocol::VERSION);
        ensure!(library.server_name == "Test Mini");
        ensure!(library.qualities == vec![Quality::Original]);
        ensure!(library.videos.len() == 2, "{:?}", library.videos);
        let film = library
            .videos
            .iter()
            .find(|v| v.file_name == "Film.mp4")
            .context("film")?;
        ensure!(film.id.len() == 40 && !film.id.contains('/'));
        ensure!(film.title == "Film.mp4");
        ensure!(film.size == VIDEO_SIZE as u64);
        ensure!(film.mime == "video/mp4");
        ensure!(film.complete && !film.poster && film.episode.is_none());
        ensure!(film.embedded_subtitles.is_empty());
        ensure!(
            film.subtitles
                == vec![
                    SubtitleFile {
                        index: 0,
                        file_name: "Film.en.srt".into(),
                        language: Some("en".into())
                    },
                    SubtitleFile {
                        index: 1,
                        file_name: "Film.srt".into(),
                        language: None
                    },
                ],
            "{:?}",
            film.subtitles
        );
        let episode = library
            .videos
            .iter()
            .find(|v| v.file_name == "The.Show.S01E02.mkv")
            .and_then(|v| v.episode.clone())
            .context("episode")?;
        ensure!((episode.season, episode.number) == (1, 2), "{episode:?}");
        // No listing field carries a path.
        let raw = running.get(&paths::library()).send().await?.text().await?;
        ensure!(!raw.contains(fixture.media.to_string_lossy().as_ref()));

        let mut ids: Vec<_> = library.videos.iter().map(|v| v.id.clone()).collect();
        ids.sort();
        running.server.shutdown().await;

        let again = fixture.start().await?;
        let mut after: Vec<_> = again
            .library()
            .await?
            .videos
            .into_iter()
            .map(|v| v.id)
            .collect();
        after.sort();
        ensure!(ids == after, "ids change across restarts");
        again.server.shutdown().await;

        // A different server (another salt) gives different ids.
        let other = Fixture::new()?;
        let mut config = other.config(Sources::Folders(vec![fixture.media.clone()]));
        config.token = other.token.clone();
        let server = Server::start(config).await?;
        let client = reqwest::Client::builder().no_proxy().build()?;
        let library: Library = client
            .get(format!("http://{}{}", server.addr(), paths::library()))
            .header(header::AUTHORIZATION, protocol::bearer(&other.token))
            .send()
            .await?
            .json()
            .await?;
        ensure!(library.videos.iter().all(|v| !ids.contains(&v.id)));
        server.shutdown().await;
        Ok(())
    })
    .await?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn media_serves_single_ranges() -> Result<()> {
    timeout(DEADLINE, async {
        let fixture = Fixture::new()?;
        let running = fixture.start().await?;
        let id = running.film_id().await?;
        let bytes = pattern(VIDEO_SIZE);
        let size = VIDEO_SIZE as u64;

        let full = running.get(&paths::media(&id)).send().await?;
        ensure!(full.status() == StatusCode::OK);
        ensure!(header_value(&full, header::ACCEPT_RANGES) == "bytes");
        ensure!(header_value(&full, header::CONTENT_TYPE) == "video/mp4");
        ensure!(header_value(&full, header::CONTENT_LENGTH) == size.to_string());
        let etag = header_value(&full, header::ETAG);
        ensure!(
            etag.starts_with('"') && etag.ends_with('"') && etag.len() > 2,
            "{etag}"
        );
        ensure!(full.bytes().await?.as_ref() == bytes.as_slice());

        for (range, start, end) in [
            ("bytes=0-99".to_owned(), 0, 99),
            (
                format!("bytes={}-{}", size - 1, size - 1),
                size - 1,
                size - 1,
            ),
            (format!("bytes={}-", size - 1000), size - 1000, size - 1),
            ("bytes=1000000-".to_owned(), 1_000_000, size - 1),
            ("bytes=-10".to_owned(), size - 10, size - 1),
            (format!("bytes=5-{}", size + 50), 5, size - 1),
        ] {
            let response = running
                .get(&paths::media(&id))
                .header(header::RANGE, &range)
                .send()
                .await?;
            ensure!(
                response.status() == StatusCode::PARTIAL_CONTENT,
                "{range}: {}",
                response.status()
            );
            ensure!(
                header_value(&response, header::CONTENT_RANGE)
                    == format!("bytes {start}-{end}/{size}"),
                "{range}"
            );
            ensure!(
                header_value(&response, header::CONTENT_LENGTH) == (end - start + 1).to_string()
            );
            ensure!(header_value(&response, header::ETAG) == etag);
            let body = response.bytes().await?;
            ensure!(
                body.as_ref() == &bytes[start as usize..=end as usize],
                "{range} body"
            );
        }
        running.server.shutdown().await;
        Ok(())
    })
    .await?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn media_refuses_multi_range_and_out_of_bounds() -> Result<()> {
    timeout(DEADLINE, async {
        let fixture = Fixture::new()?;
        let running = fixture.start().await?;
        let id = running.film_id().await?;
        let size = VIDEO_SIZE as u64;
        for range in [
            "bytes=0-1,5-6".to_owned(),
            format!("bytes={size}-"),
            format!("bytes={}-{}", size + 10, size + 20),
            "bytes=10-5".to_owned(),
            "bytes=-0".to_owned(),
            "bytes=x-y".to_owned(),
        ] {
            let response = running
                .get(&paths::media(&id))
                .header(header::RANGE, &range)
                .send()
                .await?;
            ensure!(
                response.status() == StatusCode::RANGE_NOT_SATISFIABLE,
                "{range}: {}",
                response.status()
            );
            ensure!(header_value(&response, header::CONTENT_RANGE) == format!("bytes */{size}"));
            error_message(response).await?;
        }
        running.server.shutdown().await;
        Ok(())
    })
    .await?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn head_reports_lengths_without_a_body() -> Result<()> {
    timeout(DEADLINE, async {
        let fixture = Fixture::new()?;
        let running = fixture.start().await?;
        let id = running.film_id().await?;
        let size = VIDEO_SIZE as u64;
        let response = running
            .request(reqwest::Method::HEAD, &paths::media(&id))
            .send()
            .await?;
        ensure!(response.status() == StatusCode::OK);
        ensure!(header_value(&response, header::CONTENT_LENGTH) == size.to_string());
        ensure!(header_value(&response, header::ACCEPT_RANGES) == "bytes");
        ensure!(!header_value(&response, header::ETAG).is_empty());
        ensure!(response.bytes().await?.is_empty());

        let response = running
            .request(reqwest::Method::HEAD, &paths::media(&id))
            .header(header::RANGE, "bytes=100-199")
            .send()
            .await?;
        ensure!(response.status() == StatusCode::PARTIAL_CONTENT);
        ensure!(header_value(&response, header::CONTENT_LENGTH) == "100");
        ensure!(header_value(&response, header::CONTENT_RANGE) == format!("bytes 100-199/{size}"));
        ensure!(response.bytes().await?.is_empty());
        running.server.shutdown().await;
        Ok(())
    })
    .await?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn if_range_with_a_stale_etag_sends_the_whole_file() -> Result<()> {
    timeout(DEADLINE, async {
        let fixture = Fixture::new()?;
        let running = fixture.start().await?;
        let id = running.film_id().await?;
        let head = running
            .request(reqwest::Method::HEAD, &paths::media(&id))
            .send()
            .await?;
        let etag = header_value(&head, header::ETAG);

        let fresh = running
            .get(&paths::media(&id))
            .header(header::RANGE, "bytes=10-19")
            .header(header::IF_RANGE, &etag)
            .send()
            .await?;
        ensure!(fresh.status() == StatusCode::PARTIAL_CONTENT);
        ensure!(fresh.bytes().await?.as_ref() == &pattern(VIDEO_SIZE)[10..20]);

        for stale in [
            "\"0-0\"".to_owned(),
            format!("W/{etag}"),
            "Wed, 21 Oct 2015 07:28:00 GMT".to_owned(),
        ] {
            let response = running
                .get(&paths::media(&id))
                .header(header::RANGE, "bytes=10-19")
                .header(header::IF_RANGE, &stale)
                .send()
                .await?;
            ensure!(
                response.status() == StatusCode::OK,
                "{stale}: {}",
                response.status()
            );
            ensure!(header_value(&response, header::CONTENT_RANGE).is_empty());
            ensure!(response.bytes().await?.len() == VIDEO_SIZE);
        }

        // Replacing the file changes the tag.
        std::fs::write(fixture.media.join("Film.mp4"), pattern(VIDEO_SIZE - 1))?;
        let response = running
            .request(reqwest::Method::HEAD, &paths::media(&id))
            .send()
            .await?;
        ensure!(header_value(&response, header::ETAG) != etag);
        running.server.shutdown().await;
        Ok(())
    })
    .await?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unverified_torrent_files_answer_conflict() -> Result<()> {
    timeout(DEADLINE, async {
        let fixture = Fixture::new()?;
        let film = fixture.media.join("Film.mp4");
        let partial = fixture.media.join("Partial.mkv");
        std::fs::write(&partial, pattern(1024))?;
        let infohash = "c".repeat(40);
        let settings = fixture.state.parent().unwrap().join("settings.json");
        std::fs::write(
            &settings,
            serde_json::json!({
                "volume": 0.5,
                "library": [
                    {"path": film, "title": "My Film", "duration": 61.5, "width": 1280, "height": 720},
                    {"path": partial, "title": "Partial", "torrent": {"infohash": infohash, "file_index": 1}},
                ],
                "unverified_torrent_files": [
                    {"path": partial, "identity": {"infohash": infohash, "file_index": 1}}
                ],
            })
            .to_string(),
        )?;
        let running = fixture.start_with(Sources::SettingsFile(settings)).await?;
        let library = running.library().await?;
        ensure!(library.videos.len() == 2);
        let film = &library.videos[0];
        ensure!(film.title == "My Film" && film.duration == 61.5 && (film.width, film.height) == (1280, 720));
        ensure!(film.complete);
        let partial = &library.videos[1];
        ensure!(!partial.complete);
        for method in [reqwest::Method::GET, reqwest::Method::HEAD] {
            let response = running
                .request(method.clone(), &paths::media(&partial.id))
                .send()
                .await?;
            ensure!(response.status() == StatusCode::CONFLICT, "{method}");
        }
        let response = running.get(&paths::media(&partial.id)).send().await?;
        ensure!(error_message(response).await?.contains("downloading"));
        let response = running.get(&paths::hash(&partial.id)).send().await?;
        ensure!(response.status() == StatusCode::CONFLICT);
        let response = running.get(&paths::media(&film.id)).send().await?;
        ensure!(response.status() == StatusCode::OK);
        running.server.shutdown().await;
        Ok(())
    })
    .await?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_readers_get_their_own_ranges() -> Result<()> {
    timeout(DEADLINE, async {
        let fixture = Fixture::new()?;
        let running = fixture.start().await?;
        let id = running.film_id().await?;
        let bytes = pattern(VIDEO_SIZE);
        let ranges = [
            (0usize, 2 * 1024 * 1024 - 1),
            (1_500_001, VIDEO_SIZE - 1),
            (777, 3_000_000),
        ];
        let mut readers = Vec::new();
        for (start, end) in ranges {
            // Separate clients, so each reader has its own connection.
            let client = reqwest::Client::builder().no_proxy().build()?;
            let url = format!("{}{}", running.base, paths::media(&id));
            let token = running.token.clone();
            readers.push(tokio::spawn(async move {
                let response = client
                    .get(url)
                    .header(header::AUTHORIZATION, protocol::bearer(&token))
                    .header(header::RANGE, format!("bytes={start}-{end}"))
                    .send()
                    .await?;
                anyhow::ensure!(response.status() == StatusCode::PARTIAL_CONTENT);
                Ok::<_, anyhow::Error>((start, end, response.bytes().await?))
            }));
        }
        for reader in readers {
            let (start, end, body) = reader.await??;
            ensure!(body.as_ref() == &bytes[start..=end], "{start}-{end}");
        }
        running.server.shutdown().await;
        Ok(())
    })
    .await?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn binding_outside_tailscale_and_loopback_is_refused() -> Result<()> {
    timeout(DEADLINE, async {
        let fixture = Fixture::new()?;
        for bind in [
            "0.0.0.0:0",
            "[::]:0",
            "192.168.1.10:0",
            "10.0.0.5:0",
            "8.8.8.8:0",
        ] {
            let mut config = fixture.config(Sources::Folders(vec![fixture.media.clone()]));
            config.bind = bind.parse()?;
            let Err(error) = Server::start(config).await else {
                anyhow::bail!("{bind} was accepted");
            };
            ensure!(error.to_string().contains("Tailscale"), "{bind}: {error}");
        }
        // A token of the wrong shape is refused too.
        let mut config = fixture.config(Sources::Folders(vec![fixture.media.clone()]));
        config.token = "short".into();
        ensure!(Server::start(config).await.is_err());
        // Two servers cannot share one state folder.
        let running = fixture.start().await?;
        ensure!(fixture.start().await.is_err());
        running.server.shutdown().await;
        Ok(())
    })
    .await?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn progress_round_trips_and_survives_restarts() -> Result<()> {
    timeout(DEADLINE, async {
        let fixture = Fixture::new()?;
        let running = fixture.start().await?;
        let id = running.film_id().await?;
        let response = running.get(&paths::progress(&id)).send().await?;
        ensure!(response.status() == StatusCode::NOT_FOUND);

        let saved = Progress {
            position: 42.5,
            duration: 100.0,
            finished: false,
            updated: 1_800_000_000,
        };
        let response = running
            .request(reqwest::Method::PUT, &paths::progress(&id))
            .json(&saved)
            .send()
            .await?;
        ensure!(response.status() == StatusCode::OK, "{}", response.status());
        ensure!(response.json::<Progress>().await? == saved);
        let got: Progress = running
            .get(&paths::progress(&id))
            .send()
            .await?
            .json()
            .await?;
        ensure!(got == saved);

        // Much older progress from an offline client does not rewind it.
        let older = Progress {
            position: 1.0,
            updated: saved.updated - 3600,
            ..saved.clone()
        };
        let response = running
            .request(reqwest::Method::PUT, &paths::progress(&id))
            .json(&older)
            .send()
            .await?;
        ensure!(response.json::<Progress>().await? == saved);

        // Within the same minute, the client wins.
        let close = Progress {
            position: 40.0,
            updated: saved.updated - 30,
            ..saved.clone()
        };
        let response = running
            .request(reqwest::Method::PUT, &paths::progress(&id))
            .json(&close)
            .send()
            .await?;
        ensure!(response.json::<Progress>().await? == close);

        let response = running
            .request(reqwest::Method::PUT, &paths::progress(&id))
            .body("not json")
            .send()
            .await?;
        ensure!(response.status() == StatusCode::BAD_REQUEST);
        error_message(response).await?;
        running.server.shutdown().await;

        let again = fixture.start().await?;
        let got: Progress = again
            .get(&paths::progress(&id))
            .send()
            .await?
            .json()
            .await?;
        ensure!(got == close);
        again.server.shutdown().await;
        Ok(())
    })
    .await?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn subtitles_posters_and_hash_are_served() -> Result<()> {
    timeout(DEADLINE, async {
        let fixture = Fixture::new()?;
        let cache = fixture.state.parent().unwrap().join("thumbnails");
        std::fs::create_dir_all(&cache)?;
        let poster = cache.join("poster.png");
        std::fs::write(&poster, b"\x89PNG fake")?;
        let settings = fixture.state.parent().unwrap().join("settings.json");
        let film = fixture.media.join("Film.mp4");
        let episode = fixture.media.join("The Show/Season 1/The.Show.S01E02.mkv");
        std::fs::write(
            &settings,
            serde_json::json!({"library": [
                {"path": film, "title": "Film", "poster": poster},
                {"path": episode, "title": "Ep"},
            ]})
            .to_string(),
        )?;
        let mut config = fixture.config(Sources::SettingsFile(settings));
        config.poster_cache = Some(cache);
        let server = Server::start(config).await?;
        let running = Running {
            base: format!("http://{}", server.addr()),
            token: fixture.token.clone(),
            client: reqwest::Client::builder().no_proxy().build()?,
            server,
        };
        let library = running.library().await?;
        let film = &library.videos[0];
        let episode = &library.videos[1];
        ensure!(film.poster && !episode.poster);

        let response = running.get(&paths::poster(&film.id)).send().await?;
        ensure!(response.status() == StatusCode::OK);
        ensure!(header_value(&response, header::CONTENT_TYPE) == "image/png");
        ensure!(response.bytes().await?.as_ref() == b"\x89PNG fake");
        let response = running.get(&paths::poster(&episode.id)).send().await?;
        ensure!(response.status() == StatusCode::NOT_FOUND);

        let list: Vec<SubtitleFile> = running
            .get(&paths::subtitles(&film.id))
            .send()
            .await?
            .json()
            .await?;
        ensure!(list == film.subtitles && list.len() == 2);
        let response = running.get(&paths::subtitle(&film.id, 1)).send().await?;
        ensure!(response.status() == StatusCode::OK);
        ensure!(header_value(&response, header::CONTENT_TYPE) == "application/x-subrip");
        ensure!(response.text().await?.contains("Hey"));
        let response = running.get(&paths::subtitle(&film.id, 2)).send().await?;
        ensure!(response.status() == StatusCode::NOT_FOUND);

        let hash: serde_json::Value = running
            .get(&paths::hash(&film.id))
            .send()
            .await?
            .json()
            .await?;
        let expected = flick::subtitles::movie_hash(&fixture.media.join("Film.mp4"))?;
        ensure!(hash["hash"] == expected.as_str(), "{hash}");
        // Too small for the OpenSubtitles hash.
        let response = running.get(&paths::hash(&episode.id)).send().await?;
        ensure!(response.status() == StatusCode::UNPROCESSABLE_ENTITY);
        error_message(response).await?;
        running.server.shutdown().await;
        Ok(())
    })
    .await?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shutdown_ends_streams_in_progress() -> Result<()> {
    timeout(DEADLINE, async {
        let fixture = Fixture::new()?;
        let running = fixture.start().await?;
        let id = running.film_id().await?;
        let mut response = running.get(&paths::media(&id)).send().await?;
        ensure!(response.status() == StatusCode::OK);
        // Read one chunk, then stop the server with the rest unread.
        ensure!(response.chunk().await?.is_some());
        let started = std::time::Instant::now();
        running.server.shutdown().await;
        ensure!(
            started.elapsed() < Duration::from_secs(4),
            "{:?}",
            started.elapsed()
        );
        let mut received = 0;
        let mut ended_early = false;
        loop {
            match response.chunk().await {
                Ok(Some(chunk)) => received += chunk.len(),
                Ok(None) => break,
                Err(_) => {
                    ended_early = true;
                    break;
                }
            }
        }
        ensure!(
            ended_early || received < VIDEO_SIZE,
            "stream finished after shutdown"
        );
        ensure!(
            reqwest::Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(2))
                .build()?
                .get(format!("{}{}", running.base, paths::library()))
                .send()
                .await
                .is_err(),
            "listener still accepts"
        );
        Ok(())
    })
    .await?
}

fn flick_serve(state: &Path) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_flick-serve"));
    command
        .arg("--state-dir")
        .arg(state)
        .env("RUST_LOG", "warn")
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    command
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn flick_serve_prints_pairing_and_serves() -> Result<()> {
    timeout(DEADLINE, async {
        let fixture = Fixture::new()?;
        let print = |extra: &'static [&'static str]| {
            let mut command = flick_serve(&fixture.state);
            command
                .args(["--bind", "127.0.0.1:47999", "--print-pairing"])
                .args(extra);
            async move {
                let output = command.output().await?;
                ensure!(
                    output.status.success(),
                    "{}",
                    String::from_utf8_lossy(&output.stderr)
                );
                let line: serde_json::Value = serde_json::from_slice(&output.stdout)?;
                ensure!(line["listen"] == "127.0.0.1:47999", "{line}");
                Pairing::parse(line["pairing"].as_str().context("pairing")?)
            }
        };
        let pairing = print(&[]).await?;
        ensure!(pairing.host == "127.0.0.1" && pairing.port == 47999);
        ensure!(pairing.token == fixture.token, "the saved token is reused");
        ensure!(print(&[]).await?.token == fixture.token);
        let renewed = print(&["--new-token"]).await?;
        ensure!(renewed.token != fixture.token);
        ensure!(std::fs::read_to_string(fixture.state.join("token"))?.trim() == renewed.token);

        // Refusals happen before anything is served.
        for bind in ["0.0.0.0:47117", "192.168.1.10:47117"] {
            let output = flick_serve(&fixture.state)
                .args(["--bind", bind, "--folder"])
                .arg(&fixture.media)
                .output()
                .await?;
            ensure!(!output.status.success(), "{bind}");
            ensure!(output.stdout.is_empty(), "{bind}");
        }
        let output = flick_serve(&fixture.state)
            .args(["--bind", "127.0.0.1:0", "--print-pairing"])
            .output()
            .await?;
        ensure!(!output.status.success(), "port 0 has no stable pairing");

        // Start it, read the startup line, and use the pairing.
        let mut child = flick_serve(&fixture.state)
            .args(["--bind", "127.0.0.1:0", "--name", "Mini", "--folder"])
            .arg(&fixture.media)
            .stdout(std::process::Stdio::piped())
            .spawn()?;
        let stdout = child.stdout.take().context("stdout")?;
        let mut lines = tokio::io::AsyncBufReadExt::lines(tokio::io::BufReader::new(stdout));
        let line: serde_json::Value =
            serde_json::from_str(&lines.next_line().await?.context("startup line")?)?;
        let pairing = Pairing::parse(line["pairing"].as_str().context("pairing")?)?;
        ensure!(pairing.token == renewed.token && pairing.port != 0);
        ensure!(line["listen"] == format!("127.0.0.1:{}", pairing.port));
        let library: Library = reqwest::Client::builder()
            .no_proxy()
            .build()?
            .get(format!("{}{}", pairing.base_url(), paths::library()))
            .header(header::AUTHORIZATION, protocol::bearer(&pairing.token))
            .send()
            .await?
            .json()
            .await?;
        ensure!(library.server_name == "Mini" && library.videos.len() == 2);

        #[cfg(unix)]
        {
            // launchd stops agents with SIGTERM; the server exits cleanly.
            let pid = child.id().context("pid")? as libc::pid_t;
            ensure!(unsafe { libc::kill(pid, libc::SIGTERM) } == 0);
            let status = child.wait().await?;
            ensure!(status.success(), "{status}");
        }
        #[cfg(not(unix))]
        child.kill().await?;
        Ok(())
    })
    .await?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn without_ffmpeg_only_original_is_offered() -> Result<()> {
    timeout(DEADLINE, async {
        let fixture = Fixture::new()?;
        let running = fixture.start().await?;
        let library = running.library().await?;
        ensure!(library.qualities == vec![Quality::Original]);
        let id = running.film_id().await?;
        for path in [
            paths::hls_playlist(&id, Quality::Low720),
            paths::hls_init(&id, Quality::Low720),
            paths::hls_segment(&id, Quality::Low720, 0),
            paths::embedded_subtitle(&id, 0),
        ] {
            let response = running.get(&path).send().await?;
            ensure!(
                response.status() == StatusCode::SERVICE_UNAVAILABLE,
                "{path}: {}",
                response.status()
            );
            let message = error_message(response).await?;
            ensure!(message.contains("FFmpeg"), "{path}: {message}");
        }
        // Original is never an HLS tier.
        let response = running
            .get(&paths::hls_playlist(&id, Quality::Original))
            .send()
            .await?;
        ensure!(response.status() == StatusCode::NOT_FOUND);
        running.server.shutdown().await;
        Ok(())
    })
    .await?
}

/// Needs a real FFmpeg and a generated clip:
/// `bash scripts/make-test-video.sh /tmp/flick-test.mp4 --with-subtitles`
/// then `FLICK_TEST_VIDEO=/tmp/flick-test.mp4 cargo test … -- --ignored`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires FFmpeg on PATH or FLICK_FFMPEG, and FLICK_TEST_VIDEO"]
async fn with_ffmpeg_tiers_probed_details_and_embedded_subtitles_are_served() -> Result<()> {
    timeout(Duration::from_secs(120), async {
        let clip =
            PathBuf::from(std::env::var_os("FLICK_TEST_VIDEO").context("Set FLICK_TEST_VIDEO")?);
        let fixture = Fixture::new()?;
        std::fs::copy(&clip, fixture.media.join("Clip.mp4"))?;
        let mut config = fixture.config(Sources::Folders(vec![fixture.media.clone()]));
        config.ffmpeg = std::env::var_os("FLICK_FFMPEG").map(PathBuf::from);
        let server = Server::start(config).await?;
        let running = Running {
            base: format!("http://{}", server.addr()),
            token: fixture.token.clone(),
            client: reqwest::Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(60))
                .build()?,
            server,
        };
        let mut clip_entry = None;
        for _ in 0..50 {
            let library = running.library().await?;
            ensure!(
                library.qualities == Quality::ALL.to_vec(),
                "{:?}",
                library.qualities
            );
            let entry = library
                .videos
                .into_iter()
                .find(|v| v.file_name == "Clip.mp4")
                .context("Clip.mp4 is listed")?;
            if entry.duration > 0.0 && entry.width > 0 {
                clip_entry = Some(entry);
                break;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        let clip_entry = clip_entry.context("ffprobe filled in the duration and size")?;
        let response = running
            .get(&paths::hls_playlist(&clip_entry.id, Quality::Data480))
            .send()
            .await?;
        ensure!(response.status() == StatusCode::OK, "{}", response.status());
        let playlist = response.text().await?;
        ensure!(playlist.starts_with("#EXTM3U") && playlist.contains("#EXT-X-ENDLIST"));
        let response = running
            .get(&paths::hls_segment(&clip_entry.id, Quality::Data480, 0))
            .send()
            .await?;
        ensure!(response.status() == StatusCode::OK, "{}", response.status());
        ensure!(response.bytes().await?.len() > 1000);
        if let Some(track) = clip_entry.embedded_subtitles.iter().find(|s| s.text) {
            let response = running
                .get(&paths::embedded_subtitle(&clip_entry.id, track.track))
                .send()
                .await?;
            ensure!(response.status() == StatusCode::OK, "{}", response.status());
            ensure!(header_value(&response, header::CONTENT_TYPE).starts_with("text/vtt"));
            ensure!(response.text().await?.starts_with("WEBVTT"));
        } else {
            eprintln!("the clip has no text subtitle track; extraction not exercised");
        }
        let response = running
            .get(&paths::embedded_subtitle(&clip_entry.id, 99))
            .send()
            .await?;
        ensure!(response.status() == StatusCode::NOT_FOUND);
        running.server.shutdown().await;
        Ok(())
    })
    .await?
}
