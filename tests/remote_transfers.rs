//! Downloads on another machine: the server's command routes, the headless
//! transfer backend, and streaming an unfinished download. Everything runs
//! on 127.0.0.1 with generated fixtures: a seed `TransferSession` shares a
//! generated file and the server's session gets it as an explicit peer.
//! No trackers, no DHT, no Internet.

use anyhow::{Context, Result, ensure};
use flick::{
    remote::{
        auth,
        ids::Salt,
        protocol::{
            self, BrowseListing, Capability, ErrorBody, FetchPhase, FetchRow, Library, ShareResult,
            Started, TorrentInfo, TransferPhase, TransferRow, Video, paths,
        },
        server::{Server, ServerConfig, Sources},
        transfers::{
            self, BackendOptions, TransferBackend, TransferCommand, TransferError, TransferReply,
        },
    },
    torrent::{SharedTorrent, TorrentInput, TransferConfig, TransferSession},
};
use reqwest::{Method, StatusCode, header};
use serde_json::json;
use std::{
    net::SocketAddr,
    num::NonZeroU32,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};
use tempfile::TempDir;
use tokio::time::{sleep, timeout};
use tokio_util::sync::CancellationToken;

/// Bytes that differ at every offset in a 251-byte cycle, so a misplaced
/// range is visible.
fn content(len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| ((i.wrapping_mul(31) ^ (i >> 9)) % 251) as u8)
        .collect()
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let mut group = [0u8; 3];
        group[..chunk.len()].copy_from_slice(chunk);
        let n = u32::from_be_bytes([0, group[0], group[1], group[2]]);
        for i in 0..chunk.len() + 1 {
            out.push(ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char);
        }
        for _ in chunk.len()..3 {
            out.push('=');
        }
    }
    out
}

/// A loopback session sharing `source`.
struct Seed {
    session: TransferSession,
    shared: SharedTorrent,
    peer: SocketAddr,
}

impl Seed {
    async fn start(root: &Path, source: &Path, upload_limit: Option<NonZeroU32>) -> Result<Self> {
        let mut config =
            TransferConfig::local(root.join("seed-downloads"), root.join("seed-state"));
        config.upload_limit_bytes_per_second = upload_limit;
        let session = TransferSession::start(config).await?;
        let shared = session.share(source, vec![]).await?;
        let peer = session.listen_addr().context("Missing loopback listener")?;
        ensure!(peer.ip().is_loopback(), "The seed listens beyond loopback");
        Ok(Self {
            session,
            shared,
            peer,
        })
    }
}

#[derive(Default)]
struct HeadlessOptions {
    peers: Vec<SocketAddr>,
    download_limit: Option<NonZeroU32>,
    share_roots: Vec<PathBuf>,
    /// Serve a settings file plus `remote-downloads.json` instead of
    /// scanning the download folder.
    library: bool,
}

/// The server with a headless backend over its own loopback session.
struct Headless {
    server: Option<Server>,
    session: Arc<TransferSession>,
    stop: CancellationToken,
    task: tokio::task::JoinHandle<()>,
    base: String,
    token: String,
    client: reqwest::Client,
    download_dir: PathBuf,
    state_dir: PathBuf,
}

impl Headless {
    async fn start(root: &Path, options: HeadlessOptions) -> Result<Self> {
        let state_dir = root.join("server-state");
        let download_dir = root.join("server-downloads");
        let token = auth::load_or_create_token(&state_dir, false)?;
        let mut config = TransferConfig::local(download_dir.clone(), state_dir.join("transfers"));
        config.download_limit_bytes_per_second = options.download_limit;
        let session = Arc::new(TransferSession::start(config).await?);
        let mut backend_options =
            BackendOptions::new(download_dir.clone(), Salt::load_or_create(&state_dir)?);
        backend_options.initial_peers = options.peers;
        backend_options.share_roots = options.share_roots;
        let downloads_list = state_dir.join("remote-downloads.json");
        backend_options.downloads_list = options.library.then(|| downloads_list.clone());
        let capabilities = backend_options.capabilities();
        let stop = backend_options.stop.clone();
        let (sender, receiver) = tokio::sync::mpsc::channel(16);
        let task = tokio::spawn(transfers::serve_backend(
            session.clone(),
            backend_options,
            receiver,
        ));
        let sources = if options.library {
            let settings = root.join("settings.json");
            std::fs::write(&settings, json!({ "library": [] }).to_string())?;
            Sources::Several(vec![
                Sources::SettingsFile(settings),
                Sources::PathList(downloads_list),
            ])
        } else {
            Sources::Folders(vec![download_dir.clone()])
        };
        let server = Server::start(ServerConfig {
            bind: "127.0.0.1:0".parse()?,
            token: token.clone(),
            name: "Test Mini".into(),
            sources,
            poster_cache: None,
            state_dir: state_dir.clone(),
            ffmpeg: Some(state_dir.join("no-ffmpeg")),
            max_transcodes: 1,
            transfers: Some(TransferBackend::new(capabilities, sender)),
            pairing: None,
            advertise_hardware: true,
        })
        .await?;
        Ok(Self {
            base: format!("http://{}", server.addr()),
            server: Some(server),
            session,
            stop,
            task,
            token,
            client: client()?,
            download_dir: download_dir.canonicalize()?,
            state_dir,
        })
    }

    fn request(&self, method: Method, path: &str) -> reqwest::RequestBuilder {
        request(&self.client, &self.base, &self.token, method, path)
    }

    async fn library(&self) -> Result<Library> {
        let response = self.request(Method::GET, &paths::library()).send().await?;
        ensure!(response.status() == StatusCode::OK, "{}", response.status());
        Ok(response.json().await?)
    }

    async fn transfers(&self) -> Result<Vec<TransferRow>> {
        let response = self
            .request(Method::GET, &paths::transfers())
            .send()
            .await?;
        ensure!(response.status() == StatusCode::OK, "{}", response.status());
        Ok(response.json().await?)
    }

    async fn start_transfer(&self, input: &str, files: &[usize]) -> Result<String> {
        let response = self
            .request(Method::POST, &paths::transfers())
            .json(&json!({ "input": input, "files": files }))
            .send()
            .await?;
        ensure!(
            response.status() == StatusCode::CREATED,
            "{}: {}",
            response.status(),
            error_message(response).await?
        );
        Ok(response.json::<Started>().await?.id)
    }

    async fn wait_complete(&self, id: &str, deadline: Duration) -> Result<TransferRow> {
        timeout(deadline, async {
            loop {
                let row = self
                    .transfers()
                    .await?
                    .into_iter()
                    .find(|row| row.id == id)
                    .context("The transfer is listed")?;
                ensure!(row.phase != TransferPhase::Error, "{:?}", row.error);
                if row.total_bytes > 0
                    && row.verified_bytes == row.total_bytes
                    && row.phase != TransferPhase::Checking
                {
                    return Ok(row);
                }
                sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .context("The download did not complete in time")?
    }

    /// Stops the server, the backend, and the session, bounded so a stream
    /// that was not released shows up as a failure rather than a hang.
    async fn stop(mut self) -> Result<()> {
        timeout(Duration::from_secs(20), async {
            if let Some(server) = self.server.take() {
                server.shutdown().await;
            }
            self.stop.cancel();
            (&mut self.task).await?;
            self.session.shutdown().await;
            Ok::<_, anyhow::Error>(())
        })
        .await
        .context("Stopping the server, backend, and session hung")?
    }
}

fn client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(60))
        .build()?)
}

fn request(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    method: Method,
    path: &str,
) -> reqwest::RequestBuilder {
    client
        .request(method, format!("{base}{path}"))
        .header(header::AUTHORIZATION, protocol::bearer(token))
}

async fn error_message(response: reqwest::Response) -> Result<String> {
    let text = response.text().await?;
    Ok(serde_json::from_str::<ErrorBody>(&text)
        .map(|body| body.message)
        .unwrap_or(text))
}

/// Percent-encodes a query value.
fn encode_query(value: &str) -> String {
    value
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn video<'a>(library: &'a Library, id: &str) -> Option<&'a Video> {
    library.videos.iter().find(|video| video.id == id)
}

/// A backend that answers listings and storage and refuses everything else
/// with 418, so a test can tell a route-level refusal from one that reached
/// the backend.
fn stub_backend(capabilities: Vec<Capability>) -> TransferBackend {
    let (sender, mut receiver) = tokio::sync::mpsc::channel::<transfers::TransferRequest>(8);
    tokio::spawn(async move {
        while let Some(request) = receiver.recv().await {
            let reply = match request.command {
                TransferCommand::List => Ok(TransferReply::Transfers(Vec::new())),
                TransferCommand::Storage => Ok(TransferReply::Storage(protocol::Storage {
                    free_bytes: 42,
                    download_folder_name: "Stub".into(),
                })),
                _ => Err(TransferError::new(418, "reached the stub")),
            };
            let _ = request.reply.send(reply);
        }
    });
    TransferBackend::new(capabilities, sender)
}

async fn stub_server(root: &Path, transfers: Option<TransferBackend>) -> Result<(Server, String)> {
    let state_dir = root.join("state");
    let media = root.join("media");
    std::fs::create_dir_all(&media)?;
    let token = auth::load_or_create_token(&state_dir, false)?;
    let server = Server::start(ServerConfig {
        bind: "127.0.0.1:0".parse()?,
        token: token.clone(),
        name: "Stub".into(),
        sources: Sources::Folders(vec![media]),
        poster_cache: None,
        state_dir: state_dir.clone(),
        ffmpeg: Some(state_dir.join("no-ffmpeg")),
        max_transcodes: 1,
        transfers,
        pairing: None,
        advertise_hardware: true,
    })
    .await?;
    Ok((server, token))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn routes_refuse_without_a_backend_or_its_capability() -> Result<()> {
    timeout(Duration::from_secs(30), async {
        let root = TempDir::new()?;
        let client = client()?;
        let hash = "0123456789abcdef0123456789abcdef01234567";
        let every_route = [
            (
                Method::POST,
                paths::transfers_resolve(),
                json!({"input": hash}),
            ),
            (Method::POST, paths::transfers(), json!({"input": hash})),
            (Method::DELETE, paths::transfers_resolve(), json!(null)),
            (Method::GET, paths::transfers(), json!(null)),
            (Method::POST, paths::transfer_pause(hash), json!(null)),
            (Method::POST, paths::transfer_resume(hash), json!(null)),
            (Method::POST, paths::transfers_pause_all(), json!(null)),
            (Method::POST, paths::transfers_resume_all(), json!(null)),
            (Method::DELETE, paths::transfer(hash), json!({})),
            (
                Method::POST,
                paths::fetches(),
                json!({"url": "https://example.com/v"}),
            ),
            (Method::GET, paths::fetches(), json!(null)),
            (Method::DELETE, paths::fetch(1), json!(null)),
            (
                Method::GET,
                format!("{}?dir=", paths::browse()),
                json!(null),
            ),
            (Method::POST, paths::shares(), json!({"dir": "Movies"})),
        ];

        // No backend at all: every route is refused with the switch's message.
        let (server, token) = stub_server(&root.path().join("none"), None).await?;
        let base = format!("http://{}", server.addr());
        for (method, path, body) in &every_route {
            let mut builder = request(&client, &base, &token, method.clone(), path);
            if !body.is_null() {
                builder = builder.json(body);
            }
            let response = builder.send().await?;
            ensure!(
                response.status() == StatusCode::FORBIDDEN,
                "{method} {path}: {}",
                response.status()
            );
            let message = error_message(response).await?;
            ensure!(message.contains("does not accept"), "{message}");
        }
        let library: Library = request(&client, &base, &token, Method::GET, &paths::library())
            .send()
            .await?
            .json()
            .await?;
        ensure!(library.capabilities.is_empty() && library.storage.is_none());
        // The token still comes first.
        let response = client
            .get(format!("{base}{}", paths::transfers()))
            .send()
            .await?;
        ensure!(response.status() == StatusCode::UNAUTHORIZED);
        server.shutdown().await;

        // Downloads only: torrents reach the backend; links and shares do not.
        let (server, token) = stub_server(
            &root.path().join("downloads"),
            Some(stub_backend(vec![Capability::Downloads])),
        )
        .await?;
        let base = format!("http://{}", server.addr());
        let get = |path: String| request(&client, &base, &token, Method::GET, &path).send();
        ensure!(get(paths::transfers()).await?.status() == StatusCode::OK);
        for (path, what) in [
            (paths::fetches(), "link downloads"),
            (format!("{}?dir=", paths::browse()), "shares"),
        ] {
            let response = get(path.clone()).await?;
            ensure!(response.status() == StatusCode::FORBIDDEN, "{path}");
            let message = error_message(response).await?;
            ensure!(message.contains(what), "{message}");
        }
        let library: Library = get(paths::library()).await?.json().await?;
        ensure!(library.capabilities == [Capability::Downloads]);
        ensure!(
            library.storage
                == Some(protocol::Storage {
                    free_bytes: 42,
                    download_folder_name: "Stub".into()
                })
        );
        server.shutdown().await;

        // Everything allowed: the route still validates before the backend.
        let (server, token) = stub_server(
            &root.path().join("all"),
            Some(stub_backend(vec![
                Capability::Downloads,
                Capability::Links,
                Capability::Shares,
                Capability::StreamIncomplete,
            ])),
        )
        .await?;
        let base = format!("http://{}", server.addr());
        let send = |method: Method, path: String, body: Option<serde_json::Value>| {
            let builder = request(&client, &base, &token, method, &path);
            match body {
                Some(body) => builder.json(&body),
                None => builder,
            }
            .send()
        };
        for (method, path, body, status) in [
            (Method::POST, paths::transfer_pause("ABC"), None, 404),
            (
                Method::POST,
                paths::transfer_pause(&hash.to_ascii_uppercase()),
                None,
                404,
            ),
            (Method::DELETE, paths::transfer("../x"), None, 404),
            (Method::DELETE, "/v1/fetches/abc".into(), None, 404),
            (Method::DELETE, "/v1/fetches/01".into(), None, 404),
            // Invalid input is 422, as the in-app server answers.
            (
                Method::POST,
                paths::transfers_resolve(),
                Some(json!({"input": "/etc/passwd"})),
                422,
            ),
            (
                Method::POST,
                paths::transfers_resolve(),
                Some(json!({"input": "base64:not base64"})),
                422,
            ),
            (
                Method::POST,
                paths::transfers(),
                Some(json!({"input": "http://example.com/a.torrent"})),
                422,
            ),
            (
                Method::POST,
                paths::fetches(),
                Some(json!({"url": "file:///etc/passwd"})),
                422,
            ),
            // Cancelling a resolve: the client drops its POST; this is 204.
            (Method::DELETE, paths::transfers_resolve(), None, 204),
            (
                Method::GET,
                format!("{}?dir=%2Fetc", paths::browse()),
                None,
                404,
            ),
            (
                Method::GET,
                format!("{}?dir=Movies%2F..%2F..", paths::browse()),
                None,
                404,
            ),
            (
                Method::GET,
                format!("{}?dir=C%3A%5CUsers", paths::browse()),
                None,
                404,
            ),
            (Method::POST, paths::shares(), Some(json!({"dir": ""})), 404),
            (
                Method::POST,
                paths::shares(),
                Some(json!({"dir": "/Users"})),
                404,
            ),
            // These reach the stub.
            (
                Method::POST,
                paths::transfers_resolve(),
                Some(json!({"input": hash})),
                418,
            ),
            (
                Method::GET,
                format!("{}?dir=Movies", paths::browse()),
                None,
                418,
            ),
            (Method::GET, paths::transfer(hash), None, 405),
        ] {
            let response = send(method.clone(), path.clone(), body).await?;
            ensure!(
                response.status().as_u16() == status,
                "{method} {path}: {} instead of {status}",
                response.status()
            );
            ensure!(
                status == 204
                    || response
                        .headers()
                        .get(header::CONTENT_TYPE)
                        .is_some_and(|value| value == "application/json"),
                "{method} {path}: not JSON"
            );
        }
        // Invalid JSON, and a body over the limit.
        let response = request(
            &client,
            &base,
            &token,
            Method::POST,
            &paths::transfers_resolve(),
        )
        .header(header::CONTENT_TYPE, "application/json")
        .body("{")
        .send()
        .await?;
        ensure!(response.status() == StatusCode::BAD_REQUEST);
        let response = request(&client, &base, &token, Method::POST, &paths::shares())
            .body(vec![b' '; 128 * 1024])
            .send()
            .await?;
        ensure!(
            response.status() == StatusCode::PAYLOAD_TOO_LARGE,
            "{}",
            response.status()
        );
        server.shutdown().await;
        Ok(())
    })
    .await
    .context("The refusal test timed out")?
}

/// A backend switched off, as the in-app one is when its owner turns the
/// switch off: it refuses with 403 first, then publishes no capabilities.
/// The listing keeps refreshing from its sources and stops advertising.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_switched_off_backend_stops_advertising_and_the_listing_refreshes() -> Result<()> {
    timeout(Duration::from_secs(30), async {
        let root = TempDir::new()?;
        let (publish, capabilities) =
            tokio::sync::watch::channel(vec![Capability::Downloads, Capability::StreamIncomplete]);
        let refusing = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (sender, mut receiver) = tokio::sync::mpsc::channel::<transfers::TransferRequest>(8);
        let refuse = refusing.clone();
        tokio::spawn(async move {
            while let Some(request) = receiver.recv().await {
                let reply = if refuse.load(std::sync::atomic::Ordering::SeqCst) {
                    Err(TransferError::not_allowed("downloads"))
                } else {
                    match request.command {
                        TransferCommand::List => Ok(TransferReply::Transfers(Vec::new())),
                        TransferCommand::Storage => Ok(TransferReply::Storage(protocol::Storage {
                            free_bytes: 42,
                            download_folder_name: "Stub".into(),
                        })),
                        _ => Err(TransferError::new(418, "reached the stub")),
                    }
                };
                let _ = request.reply.send(reply);
            }
        });
        let backend = TransferBackend::live(capabilities, sender);
        let (server, token) = stub_server(root.path(), Some(backend)).await?;
        let base = format!("http://{}", server.addr());
        let client = client()?;
        let library = || async {
            let response = request(&client, &base, &token, Method::GET, &paths::library())
                .send()
                .await?;
            ensure!(response.status() == StatusCode::OK, "{}", response.status());
            Ok::<Library, anyhow::Error>(response.json().await?)
        };
        let media = root.path().join("media");

        let listed = library().await?;
        ensure!(listed.capabilities == [Capability::Downloads, Capability::StreamIncomplete]);
        ensure!(listed.storage.is_some() && listed.videos.is_empty());

        // The backend refuses before its capabilities change: no overlay,
        // and the sources are read again rather than frozen.
        refusing.store(true, std::sync::atomic::Ordering::SeqCst);
        std::fs::write(media.join("First.mp4"), b"first")?;
        let listed = library().await?;
        ensure!(listed.videos.len() == 1, "{:?}", listed.videos);
        ensure!(listed.videos[0].complete);

        // Capabilities published as nothing: the listing advertises
        // nothing and keeps refreshing; routes refuse with 403.
        publish.send_replace(Vec::new());
        std::fs::write(media.join("Second.mp4"), b"second")?;
        let listed = library().await?;
        ensure!(listed.capabilities.is_empty(), "{:?}", listed.capabilities);
        ensure!(listed.storage.is_none());
        ensure!(listed.videos.len() == 2, "{:?}", listed.videos);
        let refused = request(&client, &base, &token, Method::GET, &paths::transfers())
            .send()
            .await?;
        ensure!(refused.status() == StatusCode::FORBIDDEN);

        // On again.
        refusing.store(false, std::sync::atomic::Ordering::SeqCst);
        publish.send_replace(vec![Capability::Downloads]);
        let listed = library().await?;
        ensure!(listed.capabilities == [Capability::Downloads]);
        ensure!(listed.storage.is_some());
        let listing = request(&client, &base, &token, Method::GET, &paths::transfers())
            .send()
            .await?;
        ensure!(listing.status() == StatusCode::OK);
        server.shutdown().await;
        Ok(())
    })
    .await
    .context("The switch test timed out")?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_download_resolves_progresses_pauses_completes_and_removes() -> Result<()> {
    timeout(Duration::from_secs(120), async {
        let root = TempDir::new()?;
        let source = root.path().join("Shared videos");
        std::fs::create_dir(&source)?;
        let bytes = content(8 * 1024 * 1024 + 4099);
        std::fs::write(source.join("clip.mp4"), &bytes)?;
        std::fs::write(source.join("notes.txt"), b"not selected")?;
        let seed = Seed::start(root.path(), &source, None).await?;
        let headless = Headless::start(
            root.path(),
            HeadlessOptions {
                peers: vec![seed.peer],
                // Slow enough to see progress and pause before completion.
                download_limit: NonZeroU32::new(1024 * 1024),
                ..Default::default()
            },
        )
        .await?;
        let result: Result<()> = async {
            let library = headless.library().await?;
            for capability in [Capability::Downloads, Capability::StreamIncomplete] {
                ensure!(library.capabilities.contains(&capability), "{capability:?}");
            }
            ensure!(!library.capabilities.contains(&Capability::Links));
            ensure!(!library.capabilities.contains(&Capability::Shares));
            let storage = library.storage.context("Storage is listed")?;
            ensure!(storage.download_folder_name == "server-downloads");
            ensure!(storage.free_bytes > 0);

            // Resolve from the .torrent bytes, then over the metadata
            // exchange with the explicit peer.
            let by_bytes: TorrentInfo = headless
                .request(Method::POST, &paths::transfers_resolve())
                .json(&json!({
                    "input": format!("base64:{}", base64(seed.shared.torrent_bytes()))
                }))
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            ensure!(by_bytes.infohash == seed.shared.infohash);
            let mut names: Vec<_> = by_bytes.files.iter().map(|f| f.path.as_str()).collect();
            names.sort();
            ensure!(names == ["clip.mp4", "notes.txt"], "{names:?}");
            let by_magnet: TorrentInfo = headless
                .request(Method::POST, &paths::transfers_resolve())
                .json(&json!({ "input": seed.shared.magnet }))
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            ensure!(by_magnet == by_bytes);
            let clip = by_magnet
                .files
                .iter()
                .find(|f| f.path == "clip.mp4")
                .context("clip.mp4 is in the torrent")?;
            ensure!(clip.size == bytes.len() as u64);

            let id = headless
                .start_transfer(&seed.shared.magnet, &[clip.index])
                .await?;
            ensure!(id == seed.shared.infohash);
            let again = headless
                .request(Method::POST, &paths::transfers())
                .json(&json!({ "input": seed.shared.magnet, "files": [clip.index] }))
                .send()
                .await?;
            ensure!(again.status() == StatusCode::CONFLICT, "{}", again.status());
            // Only a deletion's mismatch carries a list.
            let body: serde_json::Value = again.json().await?;
            ensure!(body.get("files").is_none(), "{body}");

            // Progress rises; the listing shows the unfinished video under
            // the id the transfer row names.
            let mut seen = Vec::new();
            let mut listed_unfinished = false;
            let video_id = timeout(Duration::from_secs(30), async {
                loop {
                    let row = headless
                        .transfers()
                        .await?
                        .into_iter()
                        .find(|row| row.id == id)
                        .context("The transfer is listed")?;
                    let file = row
                        .files
                        .iter()
                        .find(|f| f.index == clip.index)
                        .context("The clip row")?;
                    ensure!(file.selected && file.path == "clip.mp4");
                    ensure!(
                        row.files
                            .iter()
                            .all(|f| f.index == clip.index || !f.selected),
                        "Only the clip is selected"
                    );
                    let video_id = file.video_id.clone().context("The clip has a video id")?;
                    if row.verified_bytes > 0 && row.verified_bytes < row.total_bytes {
                        if seen.last() != Some(&row.verified_bytes) {
                            seen.push(row.verified_bytes);
                        }
                        if !listed_unfinished {
                            let library = headless.library().await?;
                            if let Some(video) = video(&library, &video_id)
                                && !video.complete
                            {
                                ensure!(video.progress > 0.0 && video.progress < 1.0);
                                ensure!(video.size == bytes.len() as u64);
                                ensure!(video.file_name == "clip.mp4");
                                listed_unfinished = true;
                            }
                        }
                    }
                    if seen.len() >= 2 && listed_unfinished {
                        return Ok::<_, anyhow::Error>(video_id);
                    }
                    ensure!(
                        row.verified_bytes < row.total_bytes,
                        "Finished before progress was seen: {seen:?}"
                    );
                    sleep(Duration::from_millis(50)).await;
                }
            })
            .await
            .context("Progress did not rise")??;
            ensure!(seen.windows(2).all(|w| w[0] < w[1]), "{seen:?}");

            // Pause and resume.
            let paused = headless
                .request(Method::POST, &paths::transfer_pause(&id))
                .send()
                .await?;
            ensure!(
                paused.status() == StatusCode::NO_CONTENT,
                "{}",
                paused.status()
            );
            let row = headless.transfers().await?.remove(0);
            ensure!(row.phase == TransferPhase::Paused, "{:?}", row.phase);
            let resumed = headless
                .request(Method::POST, &paths::transfer_resume(&id))
                .send()
                .await?;
            ensure!(
                resumed.status() == StatusCode::NO_CONTENT,
                "{}",
                resumed.status()
            );
            let unknown = headless
                .request(
                    Method::POST,
                    &paths::transfer_pause("ffffffffffffffffffffffffffffffffffffffff"),
                )
                .send()
                .await?;
            ensure!(unknown.status() == StatusCode::NOT_FOUND);

            // Completion: the video joins the listing under the same id.
            let row = headless.wait_complete(&id, Duration::from_secs(40)).await?;
            ensure!(
                row.files
                    .iter()
                    .any(|f| f.video_id.as_deref() == Some(&video_id))
            );
            let library = headless.library().await?;
            let listed = video(&library, &video_id).context("The finished video is listed")?;
            ensure!(listed.complete && listed.progress == 1.0, "{listed:?}");
            let media = headless
                .request(Method::GET, &paths::media(&video_id))
                .send()
                .await?;
            ensure!(media.status() == StatusCode::OK);
            ensure!(media.bytes().await? == bytes, "Served bytes differ");

            // Removal. clip.mp4 does not end on a piece boundary, so its
            // last piece also wrote the start of the unselected notes.txt,
            // which the engine deletes too: the row says so.
            let folder = headless.download_dir.join(&id);
            let clip_path = folder.join("clip.mp4");
            let notes_path = folder.join("notes.txt");
            ensure!(notes_path.is_file(), "The boundary piece wrote notes.txt");
            ensure!(!row.share);
            let notes = row
                .files
                .iter()
                .find(|f| f.path == "notes.txt")
                .context("The notes row")?;
            ensure!(!notes.selected && notes.will_delete, "{notes:?}");
            ensure!(
                transfers::deletion_paths(&row) == ["clip.mp4", "notes.txt"]
                    || transfers::deletion_paths(&row) == ["notes.txt", "clip.mp4"],
                "{:?}",
                transfers::deletion_paths(&row)
            );
            let keep = folder.join("keep.txt");
            std::fs::write(&keep, b"not the torrent's")?;
            // The selected file alone is a wrong list: nothing is deleted,
            // and the 409 names the right one.
            let wrong = headless
                .request(Method::DELETE, &paths::transfer(&id))
                .json(&json!({ "delete_files": true, "confirm": ["clip.mp4"] }))
                .send()
                .await?;
            ensure!(wrong.status() == StatusCode::CONFLICT, "{}", wrong.status());
            let body: serde_json::Value = wrong.json().await?;
            let mut files: Vec<String> = serde_json::from_value(body["files"].clone())?;
            files.sort();
            ensure!(files == ["clip.mp4", "notes.txt"], "{body}");
            ensure!(body["message"].as_str().is_some_and(|m| !m.is_empty()));
            ensure!(clip_path.is_file(), "A wrong list deleted the clip");
            ensure!(notes_path.is_file(), "A wrong list deleted notes.txt");
            ensure!(headless.transfers().await?.len() == 1);

            let right = headless
                .request(Method::DELETE, &paths::transfer(&id))
                .json(&json!({ "delete_files": true, "confirm": files }))
                .send()
                .await?;
            ensure!(
                right.status() == StatusCode::NO_CONTENT,
                "{}",
                right.status()
            );
            ensure!(!clip_path.exists(), "The clip was not deleted");
            ensure!(!notes_path.exists(), "notes.txt was not deleted");
            ensure!(std::fs::read(&keep)? == b"not the torrent's");
            ensure!(headless.transfers().await?.is_empty());
            ensure!(video(&headless.library().await?, &video_id).is_none());
            let gone = headless
                .request(Method::DELETE, &paths::transfer(&id))
                .send()
                .await?;
            ensure!(gone.status() == StatusCode::NOT_FOUND);
            Ok(())
        }
        .await;
        headless.stop().await?;
        seed.session.stop().await;
        result
    })
    .await
    .context("The remote download test timed out")?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn library_mode_lists_completed_downloads_from_its_own_list() -> Result<()> {
    timeout(Duration::from_secs(60), async {
        let root = TempDir::new()?;
        let source = root.path().join("Film.mkv");
        let bytes = content(300_001);
        std::fs::write(&source, &bytes)?;
        let seed = Seed::start(root.path(), &source, None).await?;
        let headless = Headless::start(
            root.path(),
            HeadlessOptions {
                peers: vec![seed.peer],
                library: true,
                ..Default::default()
            },
        )
        .await?;
        let result: Result<()> = async {
            let id = headless.start_transfer(&seed.shared.magnet, &[]).await?;
            let row = headless.wait_complete(&id, Duration::from_secs(30)).await?;
            let video_id = row.files[0].video_id.clone().context("A video id")?;
            let library = headless.library().await?;
            let listed = video(&library, &video_id).context("The download is listed")?;
            ensure!(listed.complete && listed.size == bytes.len() as u64);
            let saved: Vec<PathBuf> = serde_json::from_slice(&std::fs::read(
                headless.state_dir.join("remote-downloads.json"),
            )?)?;
            ensure!(
                saved == [headless.download_dir.join(&id).join("Film.mkv")],
                "{saved:?}"
            );

            // A transfer outside the download folder (the owner's own,
            // added to the session directly): a paired Flick may remove it
            // with Keep Files, never delete its files.
            let kept = headless
                .request(Method::DELETE, &paths::transfer(&id))
                .json(&json!({ "delete_files": false }))
                .send()
                .await?;
            ensure!(kept.status() == StatusCode::NO_CONTENT, "{}", kept.status());
            let resolved = headless
                .session
                .resolve(
                    TorrentInput::Bytes(seed.shared.torrent_bytes().to_vec()),
                    vec![],
                    Duration::from_secs(10),
                )
                .await?;
            let elsewhere = root.path().join("owner elsewhere");
            headless
                .session
                .download_to(&resolved, None, &elsewhere)
                .await?;
            headless.session.resume(&id, vec![seed.peer]).await?;
            let row = headless.wait_complete(&id, Duration::from_secs(30)).await?;
            let copy = elsewhere.canonicalize()?.join(&id).join("Film.mkv");
            ensure!(copy.is_file());
            let refused = headless
                .request(Method::DELETE, &paths::transfer(&id))
                .json(&json!({
                    "delete_files": true,
                    "confirm": transfers::deletion_paths(&row),
                }))
                .send()
                .await?;
            ensure!(
                refused.status() == StatusCode::FORBIDDEN,
                "{}",
                refused.status()
            );
            let body: serde_json::Value = refused.json().await?;
            ensure!(body.get("files").is_none(), "{body}");
            ensure!(
                body["message"]
                    .as_str()
                    .is_some_and(|m| m.contains("Keep Files")),
                "{body}"
            );
            ensure!(copy.is_file(), "The owner's file was deleted");
            Ok(())
        }
        .await;
        headless.stop().await?;
        seed.session.stop().await;
        result
    })
    .await
    .context("The library mode test timed out")?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn browse_and_share_stay_inside_allowed_roots() -> Result<()> {
    timeout(Duration::from_secs(60), async {
        let root = TempDir::new()?;
        let movies = root.path().join("Movies");
        let sub = movies.join("Holiday 2025");
        std::fs::create_dir_all(&sub)?;
        let clip = content(700_001);
        std::fs::write(sub.join("day one.mp4"), &clip)?;
        std::fs::write(movies.join("Film.mkv"), b"film")?;
        std::fs::write(movies.join("notes.txt"), b"not a video")?;
        std::fs::write(movies.join(".hidden.mp4"), b"hidden")?;
        let outside = root.path().join("Private");
        std::fs::create_dir(&outside)?;
        std::fs::write(outside.join("secret.mp4"), b"secret")?;
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&outside, movies.join("escape"))?;
            std::os::unix::fs::symlink(outside.join("secret.mp4"), movies.join("secret.mp4"))?;
            std::os::unix::fs::symlink(&sub, movies.join("inside link"))?;
        }
        let headless = Headless::start(
            root.path(),
            HeadlessOptions {
                share_roots: vec![movies.clone()],
                ..Default::default()
            },
        )
        .await?;
        let result: Result<()> = async {
            ensure!(
                headless
                    .library()
                    .await?
                    .capabilities
                    .contains(&Capability::Shares)
            );
            let browse = |dir: &str| {
                headless
                    .request(
                        Method::GET,
                        &format!("{}?dir={}", paths::browse(), encode_query(dir)),
                    )
                    .send()
            };
            let roots: BrowseListing = browse("").await?.error_for_status()?.json().await?;
            ensure!(roots.entries.len() == 1 && roots.entries[0].name == "Movies");
            let listing: BrowseListing = browse("Movies").await?.error_for_status()?.json().await?;
            let names: Vec<_> = listing
                .entries
                .iter()
                .map(|e| (e.name.as_str(), e.dir))
                .collect();
            let mut expected = vec![("Holiday 2025", true), ("Film.mkv", false)];
            if cfg!(unix) {
                expected.insert(1, ("inside link", true));
            }
            ensure!(names == expected, "{names:?}");
            ensure!(listing.entries.iter().all(|e| e.dir || e.size == 4));
            let inner: BrowseListing = browse("Movies/Holiday 2025/")
                .await?
                .error_for_status()?
                .json()
                .await?;
            ensure!(inner.dir == "Movies/Holiday 2025");
            ensure!(inner.entries.len() == 1 && inner.entries[0].size == clip.len() as u64);

            let outside_text = outside.display().to_string();
            for dir in [
                "Movies/..",
                "Movies/../Private",
                "/",
                outside_text.as_str(),
                "Movies/escape",
                "Movies/missing",
                "Private",
                "Movies/Film.mkv/x",
            ] {
                let response = browse(dir).await?;
                ensure!(
                    response.status() == StatusCode::NOT_FOUND,
                    "{dir}: {}",
                    response.status()
                );
                let message = error_message(response).await?;
                ensure!(!message.contains(&outside_text), "{message}");
                ensure!(!message.contains(&*movies.to_string_lossy()), "{message}");
            }
            let response = headless
                .request(Method::POST, &paths::shares())
                .json(&json!({ "dir": "Movies/escape" }))
                .send()
                .await?;
            ensure!(
                response.status() == StatusCode::NOT_FOUND,
                "{}",
                response.status()
            );

            // Share a folder under the root; another session fetches its
            // details from the server's session over loopback.
            let response = headless
                .request(Method::POST, &paths::shares())
                .json(&json!({ "dir": "Movies/Holiday 2025" }))
                .send()
                .await?;
            ensure!(
                response.status() == StatusCode::CREATED,
                "{}",
                response.status()
            );
            let shared: ShareResult = response.json().await?;
            ensure!(shared.magnet.contains(&shared.infohash));
            ensure!(
                headless
                    .transfers()
                    .await?
                    .iter()
                    .any(|row| row.id == shared.infohash && row.phase == TransferPhase::Seeding)
            );
            let peer = headless
                .session
                .listen_addr()
                .context("The server's session listens")?;
            let other = TransferSession::start(TransferConfig::local(
                root.path().join("other-downloads"),
                root.path().join("other-state"),
            ))
            .await?;
            let resolved = other
                .resolve(
                    TorrentInput::Magnet(shared.magnet.clone()),
                    vec![peer],
                    Duration::from_secs(15),
                )
                .await;
            other.stop().await;
            let resolved = resolved?;
            ensure!(resolved.infohash == shared.infohash);
            ensure!(resolved.files.len() == 1 && resolved.files[0].length == clip.len() as u64);
            // The source is left as it was.
            ensure!(std::fs::read(sub.join("day one.mp4"))? == clip);

            // A share's files are the owner's originals: the row says so,
            // and deleting them is refused whatever the confirmation; Keep
            // Files unregisters it and leaves them.
            let row = headless
                .transfers()
                .await?
                .into_iter()
                .find(|row| row.id == shared.infohash)
                .context("The share is listed")?;
            ensure!(row.share);
            ensure!(row.files.iter().all(|file| !file.will_delete), "{row:?}");
            for confirm in [json!([]), json!(["day one.mp4"])] {
                let refused = headless
                    .request(Method::DELETE, &paths::transfer(&shared.infohash))
                    .json(&json!({ "delete_files": true, "confirm": confirm }))
                    .send()
                    .await?;
                ensure!(
                    refused.status() == StatusCode::FORBIDDEN,
                    "{}",
                    refused.status()
                );
                let message = error_message(refused).await?;
                ensure!(message.contains("shared folder"), "{message}");
                ensure!(std::fs::read(sub.join("day one.mp4"))? == clip);
            }
            let kept = headless
                .request(Method::DELETE, &paths::transfer(&shared.infohash))
                .json(&json!({ "delete_files": false }))
                .send()
                .await?;
            ensure!(kept.status() == StatusCode::NO_CONTENT, "{}", kept.status());
            ensure!(headless.transfers().await?.is_empty());
            ensure!(std::fs::read(sub.join("day one.mp4"))? == clip);
            Ok(())
        }
        .await;
        headless.stop().await?;
        result
    })
    .await
    .context("The browse and share test timed out")?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unfinished_download_streams_verified_ranges_and_times_out_ahead() -> Result<()> {
    timeout(Duration::from_secs(150), async {
        let root = TempDir::new()?;
        let source = root.path().join("movie.mp4");
        let bytes = content(16 * 1024 * 1024 + 4099);
        std::fs::write(&source, &bytes)?;
        let size = bytes.len() as u64;
        // A slow seed, so every check below happens before completion.
        let seed = Seed::start(root.path(), &source, NonZeroU32::new(512 * 1024)).await?;
        let headless = Headless::start(
            root.path(),
            HeadlessOptions {
                peers: vec![seed.peer],
                ..Default::default()
            },
        )
        .await?;
        let result: Result<()> = async {
            let id = headless.start_transfer(&seed.shared.magnet, &[]).await?;
            ensure!(id == seed.shared.infohash);
            let video_id = timeout(Duration::from_secs(20), async {
                loop {
                    if let Some(id) = headless
                        .transfers()
                        .await?
                        .first()
                        .and_then(|row| row.files.first())
                        .and_then(|file| file.video_id.clone())
                    {
                        return Ok::<_, anyhow::Error>(id);
                    }
                    sleep(Duration::from_millis(50)).await;
                }
            })
            .await??;
            let library = headless.library().await?;
            let listed = video(&library, &video_id).context("The unfinished video is listed")?;
            ensure!(!listed.complete && listed.progress < 1.0, "{listed:?}");
            ensure!(listed.size == size);

            let head = headless
                .request(Method::HEAD, &paths::media(&video_id))
                .send()
                .await?;
            ensure!(head.status() == StatusCode::OK, "{}", head.status());
            let length = head
                .headers()
                .get(header::CONTENT_LENGTH)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<u64>().ok());
            ensure!(length == Some(size), "{length:?}");
            ensure!(head.headers().get(header::ETAG).is_none());

            // A range near the start arrives with the right bytes.
            let start = 1000u64;
            let end = start + 65_535;
            let response = headless
                .request(Method::GET, &paths::media(&video_id))
                .header(header::RANGE, format!("bytes={start}-{end}"))
                .send()
                .await?;
            ensure!(
                response.status() == StatusCode::PARTIAL_CONTENT,
                "{}: {}",
                response.status(),
                error_message(response).await?
            );
            ensure!(response.content_length() == Some(65_536));
            ensure!(
                response
                    .headers()
                    .get(header::CONTENT_RANGE)
                    .is_some_and(|value| *value == *format!("bytes {start}-{end}/{size}"))
            );
            ensure!(
                response.bytes().await? == bytes[start as usize..=end as usize],
                "Streamed bytes differ"
            );
            let row = headless.transfers().await?.remove(0);
            ensure!(
                row.verified_bytes < row.total_bytes,
                "The download finished before the checks"
            );

            // With the seed stopped, a range far ahead cannot arrive: the
            // server answers 504 after its bounded wait.
            seed.session.pause(&seed.shared.infohash).await?;
            sleep(Duration::from_millis(500)).await;
            let far = size - 65_536;
            let asked = Instant::now();
            let response = headless
                .request(Method::GET, &paths::media(&video_id))
                .header(header::RANGE, format!("bytes={far}-"))
                .send()
                .await?;
            let waited = asked.elapsed();
            ensure!(
                response.status() == StatusCode::GATEWAY_TIMEOUT,
                "{} after {waited:?}",
                response.status()
            );
            ensure!(
                waited >= Duration::from_secs(25) && waited < Duration::from_secs(45),
                "{waited:?}"
            );
            ensure!(!error_message(response).await?.is_empty());

            // Eight reads waiting for pieces at once are the limit; the
            // ninth is refused at once with Retry-After, not queued on the
            // blocking pool.
            let mut waiting = Vec::new();
            for _ in 0..8 {
                waiting.push(tokio::spawn(
                    headless
                        .request(Method::GET, &paths::media(&video_id))
                        .header(header::RANGE, format!("bytes={far}-"))
                        .send(),
                ));
            }
            sleep(Duration::from_secs(1)).await;
            let asked = Instant::now();
            let refused = headless
                .request(Method::GET, &paths::media(&video_id))
                .header(header::RANGE, "bytes=0-1023")
                .send()
                .await?;
            ensure!(
                refused.status() == StatusCode::SERVICE_UNAVAILABLE,
                "{}",
                refused.status()
            );
            ensure!(asked.elapsed() < Duration::from_secs(5));
            ensure!(
                refused
                    .headers()
                    .get(header::RETRY_AFTER)
                    .is_some_and(|value| value == "2")
            );
            ensure!(!error_message(refused).await?.is_empty());
            // Clients that go away give their places back.
            for request in &waiting {
                request.abort();
            }
            for request in waiting {
                let _ = request.await;
            }
            timeout(Duration::from_secs(10), async {
                loop {
                    let response = headless
                        .request(Method::GET, &paths::media(&video_id))
                        .header(header::RANGE, "bytes=0-1023")
                        .send()
                        .await?;
                    if response.status() == StatusCode::PARTIAL_CONTENT {
                        ensure!(response.bytes().await? == bytes[..1024]);
                        return Ok::<_, anyhow::Error>(());
                    }
                    ensure!(response.status() == StatusCode::SERVICE_UNAVAILABLE);
                    sleep(Duration::from_millis(100)).await;
                }
            })
            .await
            .context("Abandoned streams kept their places")??;

            // A client that goes away while a read waits releases the
            // stream; stopping below would hang otherwise.
            let abandoned = headless
                .request(Method::GET, &paths::media(&video_id))
                .header(header::RANGE, format!("bytes={far}-"))
                .send();
            let abandoned = tokio::spawn(abandoned);
            sleep(Duration::from_secs(1)).await;
            abandoned.abort();
            let _ = abandoned.await;
            Ok(())
        }
        .await;
        let stopped = Instant::now();
        headless.stop().await?;
        ensure!(
            stopped.elapsed() < Duration::from_secs(15),
            "Stopping took {:?}",
            stopped.elapsed()
        );
        seed.session.stop().await;
        result
    })
    .await
    .context("The streaming test timed out")?
}

/// `flick-serve` with its download flags, as a separate process: link
/// downloads through a generated yt-dlp stand-in, shares, and the listing.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn flick_serve_flags_accept_downloads_links_and_shares() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    use tokio::io::AsyncBufReadExt;

    timeout(Duration::from_secs(60), async {
        let root = TempDir::new()?;
        let media = root.path().join("media");
        let downloads = root.path().join("dl");
        let shares = root.path().join("Share Root");
        std::fs::create_dir_all(&media)?;
        std::fs::create_dir_all(shares.join("Folder"))?;
        let stand_in = root.path().join("yt-dlp");
        std::fs::write(
            &stand_in,
            r#"#!/bin/sh
home=''
for arg in "$@"; do
  case "$arg" in
    home:*) home="${arg#home:}" ;;
  esac
done
printf 'flick-title "Clip"\n'
printf 'stand-in video' > "$home/Clip [abc].mp4"
printf 'flick-file "%s"\n' "$home/Clip [abc].mp4"
"#,
        )?;
        std::fs::set_permissions(&stand_in, std::fs::Permissions::from_mode(0o755))?;
        let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_flick-serve"))
            .arg("--folder")
            .arg(&media)
            .args(["--bind", "127.0.0.1:0", "--name", "Headless"])
            .arg("--state-dir")
            .arg(root.path().join("state"))
            .arg("--ffmpeg")
            .arg(root.path().join("no-ffmpeg"))
            .args(["--allow-downloads", "--links", "--local-transfers"])
            .arg("--download-dir")
            .arg(&downloads)
            .arg("--share-root")
            .arg(&shares)
            .env("FLICK_YTDLP", &stand_in)
            .env("FLICK_DATA_DIR", root.path().join("data"))
            .env("RUST_LOG", "off")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()?;
        let stdout = child.stdout.take().context("stdout")?;
        let mut lines = tokio::io::BufReader::new(stdout).lines();
        let line = timeout(Duration::from_secs(30), lines.next_line())
            .await
            .context("No startup line")??
            .context("flick-serve exited")?;
        let startup: serde_json::Value = serde_json::from_str(&line)?;
        let pairing = protocol::Pairing::parse(startup["pairing"].as_str().context("pairing")?)?;
        let base = pairing.base_url();
        let client = client()?;
        let send =
            |method: Method, path: String| request(&client, &base, &pairing.token, method, &path);
        let result: Result<()> = async {
            let library: Library = send(Method::GET, paths::library())
                .send()
                .await?
                .json()
                .await?;
            for capability in [
                Capability::Downloads,
                Capability::Links,
                Capability::Shares,
                Capability::StreamIncomplete,
            ] {
                ensure!(library.capabilities.contains(&capability), "{capability:?}");
            }
            ensure!(
                library
                    .storage
                    .as_ref()
                    .is_some_and(|s| s.download_folder_name == "dl")
            );

            let roots: BrowseListing = send(Method::GET, format!("{}?dir=", paths::browse()))
                .send()
                .await?
                .json()
                .await?;
            ensure!(roots.entries.len() == 1 && roots.entries[0].name == "Share Root");

            // The URL is never fetched: the stand-in answers for yt-dlp.
            let response = send(Method::POST, paths::fetches())
                .json(&json!({ "url": "http://127.0.0.1:9/watch?v=abc" }))
                .send()
                .await?;
            ensure!(
                response.status() == StatusCode::CREATED,
                "{}",
                response.status()
            );
            let id = response.json::<serde_json::Value>().await?["id"]
                .as_u64()
                .context("A fetch id")?;
            let row = timeout(Duration::from_secs(20), async {
                loop {
                    let rows: Vec<FetchRow> = send(Method::GET, paths::fetches())
                        .send()
                        .await?
                        .json()
                        .await?;
                    let row = rows.into_iter().find(|r| r.id == id).context("Listed")?;
                    match row.phase {
                        FetchPhase::Done => return Ok::<_, anyhow::Error>(row),
                        FetchPhase::Failed | FetchPhase::Cancelled => {
                            anyhow::bail!("{:?}: {:?}", row.phase, row.error)
                        }
                        _ => sleep(Duration::from_millis(50)).await,
                    }
                }
            })
            .await??;
            ensure!(row.title == "Clip" && row.saved.len() == 1, "{row:?}");
            let library: Library = send(Method::GET, paths::library())
                .send()
                .await?
                .json()
                .await?;
            let listed = video(&library, &row.saved[0]).context("The link download is listed")?;
            ensure!(listed.file_name == "Clip [abc].mp4" && listed.complete);
            ensure!(downloads.join("Clip [abc].mp4").is_file());
            // A finished fetch is cleared by cancelling it.
            let cleared = send(Method::DELETE, paths::fetch(id)).send().await?;
            ensure!(cleared.status() == StatusCode::NO_CONTENT);
            let missing = send(Method::DELETE, paths::fetch(id)).send().await?;
            ensure!(missing.status() == StatusCode::NOT_FOUND);
            Ok(())
        }
        .await;
        let pid = child.id().context("pid")? as libc::pid_t;
        // SIGTERM, as launchd sends; the server stops on its own.
        unsafe {
            libc::kill(pid, libc::SIGTERM);
        }
        let status = timeout(Duration::from_secs(20), child.wait())
            .await
            .context("flick-serve did not stop on SIGTERM")??;
        result?;
        ensure!(status.success(), "{status}");
        Ok(())
    })
    .await
    .context("The flick-serve test timed out")?
}
