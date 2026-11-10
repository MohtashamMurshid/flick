//! Commands a paired Flick may send to this server: start and manage
//! torrents, link downloads, and shares, and read an unfinished download.
//! Design: `docs/REMOTE_DOWNLOADS_PLAN.md`.
//!
//! The HTTP routes talk to a [`TransferBackend`], which is a channel. Two
//! things answer on it:
//! - the headless `flick-serve`, through [`serve_backend`], directly over
//!   `crate::torrent::TransferSession` and `crate::fetch`;
//! - the desktop app, by translating to its `Services` worker (in
//!   `src/remote_serve.rs`).
//!
//! The helpers between the two ([`parse_input`], [`TransferRows`],
//! [`confirm_matches`], [`ShareRoots`], [`storage`], [`FetchTable`]) are
//! public so both backends answer in exactly the same shape.
//!
//! Every write route is refused with 403 unless the owner turned on
//! "Let paired Flicks download to this Mac"; the backend's capabilities say
//! what is allowed and the listing advertises them. With no backend at all,
//! every route here answers 403 as well.

use super::{
    ids::{self, Salt},
    protocol::{
        BrowseEntry, BrowseListing, Capability, ErrorBody, FetchPhase, FetchRow, RemoveTransfer,
        ResolveRequest, ShareResult, StartFetch, StartShare, StartTransfer, Started, Storage,
        TorrentFileInfo, TorrentInfo, TransferFileRow, TransferPhase, TransferRow,
    },
};
use crate::{
    fetch::{FetchSource, FetchStage, FetchUpdate, WebLink},
    torrent::{ResolvedTorrent, TorrentInput, TransferSession, TransferSnapshot, TransferState},
};
use axum::{
    Json, Router,
    body::Bytes,
    extract::{DefaultBodyLimit, Path as UrlPath, Query, State, rejection::BytesRejection},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{delete, get, post},
};
use serde::Deserialize;
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    net::SocketAddr,
    path::{Component, Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::{Semaphore, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

/// What a route asks the backend to do. Paths in `Browse` and `Share` are
/// virtual: `<root name>/<relative path>` under an allowed share root, never
/// a filesystem path. `Remove.confirm` echoes `TransferFileRow::path` values.
#[derive(Clone, Debug, PartialEq)]
pub enum TransferCommand {
    Resolve {
        input: String,
    },
    Start {
        input: String,
        files: Vec<usize>,
    },
    List,
    Pause {
        id: String,
    },
    Resume {
        id: String,
    },
    PauseAll,
    ResumeAll,
    Remove {
        id: String,
        delete_files: bool,
        confirm: Vec<String>,
    },
    FetchStart {
        url: String,
    },
    FetchList,
    FetchCancel {
        id: u64,
    },
    Browse {
        dir: String,
    },
    Share {
        dir: String,
    },
    Storage,
    /// A verified-pieces stream of an unfinished torrent file.
    OpenStream {
        id: String,
        file_index: usize,
    },
}

pub enum TransferReply {
    Resolved(TorrentInfo),
    Started(Started),
    Transfers(Vec<TransferRow>),
    Done,
    Fetches(Vec<FetchRow>),
    FetchStarted { id: u64 },
    Browse(BrowseListing),
    Shared(ShareResult),
    Storage(Storage),
    Stream(crate::torrent::VerifiedStream),
}

/// A refusal the route turns into an HTTP status and an `ErrorBody`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransferError {
    pub status: u16,
    pub message: String,
}

impl TransferError {
    pub fn new(status: u16, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    /// The owner has not allowed this; 403.
    pub fn not_allowed(what: &str) -> Self {
        Self::new(
            403,
            format!(
                "This Flick does not accept {what} from other Flicks. Its owner can allow it in Appearance → Remote Libraries, or start flick-serve with --allow-downloads."
            ),
        )
    }

    /// A transfer id the server does not have; 404.
    pub fn unknown_transfer() -> Self {
        Self::new(
            404,
            "This server has no transfer with this id. Refresh Downloads.",
        )
    }

    /// A browse or share path outside the allowed roots, or missing; 404.
    /// The message never names a real path.
    pub fn unknown_folder() -> Self {
        Self::new(404, "No such folder on this server.")
    }

    /// Too many resolves or link downloads at once; 429.
    pub fn busy(what: &str) -> Self {
        Self::new(
            429,
            format!("This server is already {what}. Try again when one finishes."),
        )
    }
}

impl std::fmt::Display for TransferError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.message, self.status)
    }
}

impl std::error::Error for TransferError {}

pub struct TransferRequest {
    pub command: TransferCommand,
    pub reply: oneshot::Sender<Result<TransferReply, TransferError>>,
}

/// The server's handle to whatever runs transfers. Cheap to clone.
#[derive(Clone)]
pub struct TransferBackend {
    capabilities: Vec<Capability>,
    sender: mpsc::Sender<TransferRequest>,
}

impl TransferBackend {
    pub fn new(capabilities: Vec<Capability>, sender: mpsc::Sender<TransferRequest>) -> Self {
        Self {
            capabilities,
            sender,
        }
    }

    pub fn capabilities(&self) -> &[Capability] {
        &self.capabilities
    }

    pub fn allows(&self, capability: Capability) -> bool {
        self.capabilities.contains(&capability)
    }

    /// Sends one command and waits for its answer. A backend that has gone
    /// away answers 503. Dropping the returned future drops the reply
    /// channel, which a backend may watch to cancel slow work.
    pub async fn call(&self, command: TransferCommand) -> Result<TransferReply, TransferError> {
        let (reply, answer) = oneshot::channel();
        self.sender
            .send(TransferRequest { command, reply })
            .await
            .map_err(|_| TransferError::new(503, "Transfers are not running on this Flick."))?;
        answer
            .await
            .map_err(|_| TransferError::new(503, "Transfers stopped before answering."))?
    }
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

/// Bodies that carry a `.torrent` file as base64: 16 MiB of metadata is
/// about 21.4 MiB of base64, plus the JSON around it.
const TORRENT_BODY_LIMIT: usize = 24 * 1024 * 1024;
/// Every other command body.
const SMALL_BODY_LIMIT: usize = 64 * 1024;

/// The `/v1/transfers`, `/v1/fetches`, `/v1/browse`, and `/v1/shares`
/// routes. The server merges it beside its own routes, inside the token
/// layer. Each route checks the backend's capability before anything else.
pub fn router(backend: Option<TransferBackend>) -> Router {
    Router::new()
        .route(
            "/v1/transfers",
            get(list_transfers)
                .post(start_transfer)
                .layer(DefaultBodyLimit::max(TORRENT_BODY_LIMIT)),
        )
        .route(
            "/v1/transfers/resolve",
            post(resolve).layer(DefaultBodyLimit::max(TORRENT_BODY_LIMIT)),
        )
        .route("/v1/transfers/pause-all", post(pause_all))
        .route("/v1/transfers/resume-all", post(resume_all))
        .route(
            "/v1/transfers/{id}",
            delete(remove_transfer).layer(DefaultBodyLimit::max(SMALL_BODY_LIMIT)),
        )
        .route("/v1/transfers/{id}/pause", post(pause))
        .route("/v1/transfers/{id}/resume", post(resume))
        .route(
            "/v1/fetches",
            get(list_fetches)
                .post(start_fetch)
                .layer(DefaultBodyLimit::max(SMALL_BODY_LIMIT)),
        )
        .route("/v1/fetches/{id}", delete(cancel_fetch))
        .route("/v1/browse", get(browse))
        .route(
            "/v1/shares",
            post(share).layer(DefaultBodyLimit::max(SMALL_BODY_LIMIT)),
        )
        .with_state(backend)
}

type Backend = State<Option<TransferBackend>>;

/// The JSON error response for a refusal.
pub fn error_response(error: &TransferError) -> Response {
    (
        StatusCode::from_u16(error.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        Json(ErrorBody {
            message: error.message.clone(),
        }),
    )
        .into_response()
}

fn what(capability: Capability) -> &'static str {
    match capability {
        Capability::Downloads => "downloads",
        Capability::Links => "link downloads",
        Capability::Shares => "shares",
        Capability::StreamIncomplete => "streaming unfinished downloads",
    }
}

/// The backend when it allows `capability`; otherwise the 403 response.
#[allow(clippy::result_large_err)]
fn gate(
    backend: &Option<TransferBackend>,
    capability: Capability,
) -> Result<&TransferBackend, Response> {
    backend
        .as_ref()
        .filter(|backend| backend.allows(capability))
        .ok_or_else(|| error_response(&TransferError::not_allowed(what(capability))))
}

#[allow(clippy::result_large_err)]
fn json_body<T: for<'de> Deserialize<'de> + Default>(
    body: Result<Bytes, BytesRejection>,
    empty_is_default: bool,
) -> Result<T, Response> {
    let body = body.map_err(|rejection| {
        let status = rejection.status();
        let message = if status == StatusCode::PAYLOAD_TOO_LARGE {
            "The request is larger than this server accepts".to_owned()
        } else {
            "The request body could not be read".to_owned()
        };
        error_response(&TransferError::new(status.as_u16(), message))
    })?;
    if empty_is_default && body.iter().all(u8::is_ascii_whitespace) {
        return Ok(T::default());
    }
    serde_json::from_slice(&body).map_err(|problem| {
        error_response(&TransferError::new(
            400,
            format!("The request is not valid JSON: {problem}"),
        ))
    })
}

/// Runs a command and maps the answer with `ok`; an unexpected reply is 500.
async fn run(
    backend: &TransferBackend,
    command: TransferCommand,
    ok: impl FnOnce(TransferReply) -> Option<Response>,
) -> Response {
    match backend.call(command).await {
        Ok(reply) => ok(reply).unwrap_or_else(|| {
            error_response(&TransferError::new(
                500,
                "The transfer backend gave an unexpected answer",
            ))
        }),
        Err(error) => error_response(&error),
    }
}

fn done(reply: TransferReply) -> Option<Response> {
    matches!(reply, TransferReply::Done).then(|| StatusCode::NO_CONTENT.into_response())
}

/// Whether a value from a URL or body is a torrent id: 40 lowercase hex.
pub fn is_infohash(id: &str) -> bool {
    ids::is_valid_id(id)
}

async fn resolve(State(backend): Backend, body: Result<Bytes, BytesRejection>) -> Response {
    let backend = match gate(&backend, Capability::Downloads) {
        Ok(backend) => backend,
        Err(refused) => return refused,
    };
    let request: ResolveRequest = match json_body(body, false) {
        Ok(request) => request,
        Err(refused) => return refused,
    };
    if let Err(error) = parse_input(&request.input) {
        return error_response(&error);
    }
    run(
        backend,
        TransferCommand::Resolve {
            input: request.input,
        },
        |reply| match reply {
            TransferReply::Resolved(info) => Some(Json(info).into_response()),
            _ => None,
        },
    )
    .await
}

async fn start_transfer(State(backend): Backend, body: Result<Bytes, BytesRejection>) -> Response {
    let backend = match gate(&backend, Capability::Downloads) {
        Ok(backend) => backend,
        Err(refused) => return refused,
    };
    let request: StartTransfer = match json_body(body, false) {
        Ok(request) => request,
        Err(refused) => return refused,
    };
    if let Err(error) = parse_input(&request.input) {
        return error_response(&error);
    }
    run(
        backend,
        TransferCommand::Start {
            input: request.input,
            files: request.files,
        },
        |reply| match reply {
            TransferReply::Started(started) => {
                Some((StatusCode::CREATED, Json(started)).into_response())
            }
            _ => None,
        },
    )
    .await
}

async fn list_transfers(State(backend): Backend) -> Response {
    let backend = match gate(&backend, Capability::Downloads) {
        Ok(backend) => backend,
        Err(refused) => return refused,
    };
    run(backend, TransferCommand::List, |reply| match reply {
        TransferReply::Transfers(rows) => Some(Json(rows).into_response()),
        _ => None,
    })
    .await
}

async fn pause(State(backend): Backend, UrlPath(id): UrlPath<String>) -> Response {
    let backend = match gate(&backend, Capability::Downloads) {
        Ok(backend) => backend,
        Err(refused) => return refused,
    };
    if !is_infohash(&id) {
        return error_response(&TransferError::unknown_transfer());
    }
    run(backend, TransferCommand::Pause { id }, done).await
}

async fn resume(State(backend): Backend, UrlPath(id): UrlPath<String>) -> Response {
    let backend = match gate(&backend, Capability::Downloads) {
        Ok(backend) => backend,
        Err(refused) => return refused,
    };
    if !is_infohash(&id) {
        return error_response(&TransferError::unknown_transfer());
    }
    run(backend, TransferCommand::Resume { id }, done).await
}

async fn pause_all(State(backend): Backend) -> Response {
    let backend = match gate(&backend, Capability::Downloads) {
        Ok(backend) => backend,
        Err(refused) => return refused,
    };
    run(backend, TransferCommand::PauseAll, done).await
}

async fn resume_all(State(backend): Backend) -> Response {
    let backend = match gate(&backend, Capability::Downloads) {
        Ok(backend) => backend,
        Err(refused) => return refused,
    };
    run(backend, TransferCommand::ResumeAll, done).await
}

/// A 409 for a deletion whose `confirm` list does not match carries the
/// current list as `files`, beside `message`, so the client can ask again
/// without another request. Other errors are a plain `ErrorBody`.
async fn remove_transfer(
    State(backend): Backend,
    UrlPath(id): UrlPath<String>,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    let backend = match gate(&backend, Capability::Downloads) {
        Ok(backend) => backend,
        Err(refused) => return refused,
    };
    let request: RemoveTransfer = match json_body(body, true) {
        Ok(request) => request,
        Err(refused) => return refused,
    };
    if !is_infohash(&id) {
        return error_response(&TransferError::unknown_transfer());
    }
    let command = TransferCommand::Remove {
        id: id.clone(),
        delete_files: request.delete_files,
        confirm: request.confirm,
    };
    match backend.call(command).await {
        Ok(TransferReply::Done) => StatusCode::NO_CONTENT.into_response(),
        Ok(_) => error_response(&TransferError::new(
            500,
            "The transfer backend gave an unexpected answer",
        )),
        Err(error) if error.status == 409 && request.delete_files => {
            let files = match backend.call(TransferCommand::List).await {
                Ok(TransferReply::Transfers(rows)) => rows
                    .into_iter()
                    .find(|row| row.id == id)
                    .map(|row| selected_paths(&row))
                    .unwrap_or_default(),
                _ => Vec::new(),
            };
            (
                StatusCode::CONFLICT,
                Json(serde_json::json!({ "message": error.message, "files": files })),
            )
                .into_response()
        }
        Err(error) => error_response(&error),
    }
}

async fn start_fetch(State(backend): Backend, body: Result<Bytes, BytesRejection>) -> Response {
    let backend = match gate(&backend, Capability::Links) {
        Ok(backend) => backend,
        Err(refused) => return refused,
    };
    let request: StartFetch = match json_body(body, false) {
        Ok(request) => request,
        Err(refused) => return refused,
    };
    if let Err(error) = fetch_source(&request.url) {
        return error_response(&error);
    }
    run(
        backend,
        TransferCommand::FetchStart { url: request.url },
        |reply| match reply {
            TransferReply::FetchStarted { id } => {
                Some((StatusCode::CREATED, Json(serde_json::json!({ "id": id }))).into_response())
            }
            _ => None,
        },
    )
    .await
}

async fn list_fetches(State(backend): Backend) -> Response {
    let backend = match gate(&backend, Capability::Links) {
        Ok(backend) => backend,
        Err(refused) => return refused,
    };
    run(backend, TransferCommand::FetchList, |reply| match reply {
        TransferReply::Fetches(rows) => Some(Json(rows).into_response()),
        _ => None,
    })
    .await
}

async fn cancel_fetch(State(backend): Backend, UrlPath(id): UrlPath<String>) -> Response {
    let backend = match gate(&backend, Capability::Links) {
        Ok(backend) => backend,
        Err(refused) => return refused,
    };
    let Some(id) = parse_fetch_id(&id) else {
        return error_response(&TransferError::new(
            404,
            "This server has no link download with this id. Refresh Downloads.",
        ));
    };
    run(backend, TransferCommand::FetchCancel { id }, done).await
}

/// Plain decimal without signs or leading zeros, as the server writes ids.
fn parse_fetch_id(text: &str) -> Option<u64> {
    (!text.is_empty()
        && text.len() <= 20
        && text.bytes().all(|b| b.is_ascii_digit())
        && (text == "0" || !text.starts_with('0')))
    .then(|| text.parse().ok())
    .flatten()
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct BrowseQuery {
    dir: String,
}

async fn browse(
    State(backend): Backend,
    query: Result<Query<BrowseQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    let backend = match gate(&backend, Capability::Shares) {
        Ok(backend) => backend,
        Err(refused) => return refused,
    };
    let Ok(Query(query)) = query else {
        return error_response(&TransferError::new(400, "The folder is not valid"));
    };
    if virtual_parts(&query.dir).is_none() {
        return error_response(&TransferError::unknown_folder());
    }
    run(
        backend,
        TransferCommand::Browse { dir: query.dir },
        |reply| match reply {
            TransferReply::Browse(listing) => Some(Json(listing).into_response()),
            _ => None,
        },
    )
    .await
}

async fn share(State(backend): Backend, body: Result<Bytes, BytesRejection>) -> Response {
    let backend = match gate(&backend, Capability::Shares) {
        Ok(backend) => backend,
        Err(refused) => return refused,
    };
    let request: StartShare = match json_body(body, false) {
        Ok(request) => request,
        Err(refused) => return refused,
    };
    if !virtual_parts(&request.dir).is_some_and(|parts| !parts.is_empty()) {
        return error_response(&TransferError::unknown_folder());
    }
    run(
        backend,
        TransferCommand::Share { dir: request.dir },
        |reply| match reply {
            TransferReply::Shared(result) => {
                Some((StatusCode::CREATED, Json(result)).into_response())
            }
            _ => None,
        },
    )
    .await
}

// ---------------------------------------------------------------------------
// Helpers shared by both backends
// ---------------------------------------------------------------------------

/// Longest accepted `input`: a 16 MiB `.torrent` as base64, with its prefix.
const MAX_INPUT_BYTES: usize = 16 * 1024 * 1024 / 3 * 4 + 64;

/// What `ResolveRequest::input` / `StartTransfer::input` name: a magnet
/// link, a 40-hex infohash, an HTTPS `.torrent` URL, or `base64:` and the
/// bytes of a `.torrent` file. Anything else, including a local file path,
/// is refused with 400.
pub fn parse_input(input: &str) -> Result<TorrentInput, TransferError> {
    let input = input.trim();
    let invalid = |message: &str| TransferError::new(400, message);
    if input.len() > MAX_INPUT_BYTES {
        return Err(invalid("This torrent is larger than 16 MiB"));
    }
    if let Some(encoded) = input.strip_prefix("base64:") {
        let bytes = decode_base64(encoded)
            .ok_or_else(|| invalid("The torrent file is not valid base64"))?;
        if bytes.len() > 16 * 1024 * 1024 {
            return Err(invalid("This torrent is larger than 16 MiB"));
        }
        librqbit::torrent_from_bytes(&bytes)
            .map_err(|_| invalid("This is not a valid .torrent file"))?;
        return Ok(TorrentInput::Bytes(bytes));
    }
    if input.len() >= 8 && input[..8].eq_ignore_ascii_case("https://") {
        let url =
            reqwest::Url::parse(input).map_err(|_| invalid("The torrent link is not valid"))?;
        if url.username() != "" || url.password().is_some() || url.host_str().is_none() {
            return Err(invalid("The torrent link is not valid"));
        }
        return Ok(TorrentInput::Url(input.to_owned()));
    }
    let hex = input.len() == 40 && input.bytes().all(|b| b.is_ascii_hexdigit());
    if hex || input.len() >= 7 && input[..7].eq_ignore_ascii_case("magnet:") {
        let magnet = librqbit::Magnet::parse(input)
            .map_err(|_| invalid("The magnet link or infohash is not valid"))?;
        if magnet.as_id20().is_none() {
            return Err(invalid(
                "Only BitTorrent v1 or hybrid magnet links are supported",
            ));
        }
        return Ok(TorrentInput::Magnet(input.to_owned()));
    }
    Err(invalid(
        "Paste a magnet link, an infohash, an https:// link to a .torrent file, or the file itself",
    ))
}

/// Standard base64 with optional padding and no line breaks.
fn decode_base64(text: &str) -> Option<Vec<u8>> {
    let text = text.trim().trim_end_matches('=');
    let value = |byte: u8| -> Option<u32> {
        Some(match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        } as u32)
    };
    if text.len() % 4 == 1 {
        return None;
    }
    let mut out = Vec::with_capacity(text.len() / 4 * 3 + 2);
    for chunk in text.as_bytes().chunks(4) {
        let mut group = 0u32;
        for (i, byte) in chunk.iter().enumerate() {
            group |= value(*byte)? << (18 - 6 * i);
        }
        let bytes = group.to_be_bytes();
        out.extend_from_slice(&bytes[1..chunk.len()]);
    }
    Some(out)
}

fn torrent_info(resolved: &ResolvedTorrent) -> TorrentInfo {
    TorrentInfo {
        infohash: resolved.infohash.clone(),
        name: resolved.name.clone(),
        files: resolved
            .files
            .iter()
            .map(|file| TorrentFileInfo {
                index: file.index,
                path: slash_path(&file.relative_path),
                size: file.length,
            })
            .collect(),
    }
}

/// A torrent-relative path with `/` between components on every platform.
fn slash_path(path: &Path) -> String {
    path.components()
        .map(|part| part.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

/// The selected files' paths, which a deletion must echo.
pub fn selected_paths(row: &TransferRow) -> Vec<String> {
    row.files
        .iter()
        .filter(|file| file.selected)
        .map(|file| file.path.clone())
        .collect()
}

/// Whether `confirm` lists exactly the row's selected files, each once, in
/// any order.
pub fn confirm_matches(row: &TransferRow, confirm: &[String]) -> bool {
    let mut expected = selected_paths(row);
    let mut given = confirm.to_vec();
    expected.sort();
    given.sort();
    expected == given
}

fn phase(state: TransferState) -> TransferPhase {
    match state {
        TransferState::Checking => TransferPhase::Checking,
        TransferState::Downloading => TransferPhase::Downloading,
        TransferState::Paused => TransferPhase::Paused,
        TransferState::Seeding => TransferPhase::Seeding,
        TransferState::Error => TransferPhase::Error,
    }
}

/// Turns adapter snapshots into [`TransferRow`]s. Torrent-relative file
/// paths come from each torrent's metadata and are kept per infohash.
///
/// `video_id` is set for a selected video file whose absolute path lies
/// under `video_root` (the download folder whose files the listing serves),
/// computed with the server's id salt from the canonical path the adapter
/// reports, so it equals the listing id before and after completion.
pub struct TransferRows {
    salt: Salt,
    video_root: Option<PathBuf>,
    relative: Mutex<HashMap<String, Arc<Vec<String>>>>,
}

impl TransferRows {
    pub fn new(salt: Salt, video_root: Option<PathBuf>) -> Self {
        Self {
            salt,
            video_root: video_root.map(|root| root.canonicalize().unwrap_or(root)),
            relative: Mutex::new(HashMap::new()),
        }
    }

    pub fn rows(&self, session: &TransferSession) -> anyhow::Result<Vec<TransferRow>> {
        let snapshots = session.snapshots()?;
        let mut cache = self
            .relative
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        cache.retain(|id, _| snapshots.iter().any(|s| &s.infohash == id));
        Ok(snapshots
            .iter()
            .map(|snapshot| {
                let relative = cache
                    .entry(snapshot.infohash.clone())
                    .or_insert_with(|| Arc::new(relative_paths(session, &snapshot.infohash)))
                    .clone();
                self.row(snapshot, &relative)
            })
            .collect())
    }

    /// One row; `relative` holds torrent-relative paths by file index.
    pub fn row(&self, snapshot: &TransferSnapshot, relative: &[String]) -> TransferRow {
        TransferRow {
            id: snapshot.infohash.clone(),
            name: snapshot.name.clone(),
            phase: phase(snapshot.state),
            error: snapshot.error.clone(),
            verified_bytes: snapshot.verified_bytes,
            total_bytes: snapshot.total_bytes,
            checked_bytes: snapshot.checked_bytes,
            checking_total_bytes: snapshot.checking_total_bytes,
            uploaded_bytes: snapshot.uploaded_bytes,
            peers: snapshot.peers,
            download_bps: snapshot.download_bytes_per_second,
            upload_bps: snapshot.upload_bytes_per_second,
            // Filled in by the deletion-safety fix: shares and on-disk files.
            share: false,
            files: snapshot
                .files
                .iter()
                .map(|file| TransferFileRow {
                    index: file.index,
                    path: relative.get(file.index).cloned().unwrap_or_else(|| {
                        file.path
                            .file_name()
                            .map(|name| name.to_string_lossy().into_owned())
                            .unwrap_or_default()
                    }),
                    size: file.length,
                    verified_bytes: file.verified_bytes,
                    selected: file.selected,
                    will_delete: file.selected,
                    video_id: self.video_id(&file.path, file.selected),
                })
                .collect(),
        }
    }

    fn video_id(&self, path: &Path, selected: bool) -> Option<String> {
        let root = self.video_root.as_ref()?;
        (selected && path.is_absolute() && path.starts_with(root) && crate::library::is_video(path))
            .then(|| ids::video_id(&self.salt, path))
    }

    /// The salt ids are made with.
    pub fn salt(&self) -> &Salt {
        &self.salt
    }
}

/// Torrent-relative paths by file index, from the torrent's metadata.
fn relative_paths(session: &TransferSession, infohash: &str) -> Vec<String> {
    let Ok(bytes) = session.torrent_bytes(infohash) else {
        return Vec::new();
    };
    let Ok(parsed) = librqbit::torrent_from_bytes(&bytes) else {
        return Vec::new();
    };
    let Ok(info) = parsed.info.data.validate() else {
        return Vec::new();
    };
    info.iter_file_details()
        .map(|file| {
            file.filename
                .iter_components()
                .collect::<Vec<_>>()
                .join("/")
        })
        .collect()
}

/// Free space and the folder's name, never its path.
pub fn storage(download_dir: &Path) -> Storage {
    Storage {
        free_bytes: fs2::available_space(download_dir).unwrap_or(0),
        download_folder_name: download_dir
            .canonicalize()
            .unwrap_or_else(|_| download_dir.to_owned())
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Downloads".to_owned()),
    }
}

/// Splits a virtual `<root name>/<relative path>` into its parts. None for
/// anything that looks like a filesystem path: a leading separator, a
/// backslash, a drive letter, `.` or `..`, an empty part, or a NUL.
fn virtual_parts(dir: &str) -> Option<Vec<&str>> {
    if dir.is_empty() {
        return Some(Vec::new());
    }
    if dir.len() > 4096
        || dir.starts_with('/')
        || dir.starts_with('~')
        || dir.contains(['\\', '\0'])
    {
        return None;
    }
    let dir = dir.strip_suffix('/').unwrap_or(dir);
    let parts: Vec<&str> = dir.split('/').collect();
    let first = parts.first()?;
    if first.contains(':') {
        return None;
    }
    parts
        .iter()
        .all(|part| !part.is_empty() && *part != "." && *part != "..")
        .then_some(parts)
}

/// The folders paired Flicks may browse and share from, each known by its
/// folder name. Symbolic links are resolved before every containment check,
/// and anything outside a root is answered as missing.
#[derive(Clone, Debug, Default)]
pub struct ShareRoots {
    roots: Vec<(String, PathBuf)>,
}

impl ShareRoots {
    /// Roots that cannot be resolved are skipped with a warning. Two roots
    /// with the same folder name get ` (2)`, ` (3)`, … after the name.
    pub fn new(roots: &[PathBuf]) -> Self {
        let mut named: Vec<(String, PathBuf)> = Vec::new();
        for root in roots {
            let canonical = match root.canonicalize() {
                Ok(path) if path.is_dir() => path,
                _ => {
                    tracing::warn!(root = %root.display(), "Skipping a share root that is not a folder");
                    continue;
                }
            };
            if named.iter().any(|(_, existing)| *existing == canonical) {
                continue;
            }
            let base = canonical
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| "Shared".to_owned());
            let mut name = base.clone();
            let mut n = 2;
            while named.iter().any(|(existing, _)| *existing == name) {
                name = format!("{base} ({n})");
                n += 1;
            }
            named.push((name, canonical));
        }
        Self { roots: named }
    }

    pub fn is_empty(&self) -> bool {
        self.roots.is_empty()
    }

    /// The real, canonical path for a virtual one, or 404.
    pub fn resolve(&self, dir: &str) -> Result<PathBuf, TransferError> {
        let parts = virtual_parts(dir).ok_or_else(TransferError::unknown_folder)?;
        let (first, rest) = parts
            .split_first()
            .ok_or_else(TransferError::unknown_folder)?;
        let (_, root) = self
            .roots
            .iter()
            .find(|(name, _)| name == first)
            .ok_or_else(TransferError::unknown_folder)?;
        let mut path = root.clone();
        for part in rest {
            let part = Path::new(part);
            if !matches!(part.components().next(), Some(Component::Normal(_)))
                || part.components().count() != 1
            {
                return Err(TransferError::unknown_folder());
            }
            path.push(part);
        }
        let canonical = path
            .canonicalize()
            .map_err(|_| TransferError::unknown_folder())?;
        if !canonical.starts_with(root) {
            return Err(TransferError::unknown_folder());
        }
        Ok(canonical)
    }

    /// Folders and video files in a virtual folder; the roots for `""`.
    pub fn browse(&self, dir: &str) -> Result<BrowseListing, TransferError> {
        if dir.is_empty() {
            return Ok(BrowseListing {
                dir: String::new(),
                entries: self
                    .roots
                    .iter()
                    .map(|(name, _)| BrowseEntry {
                        name: name.clone(),
                        dir: true,
                        size: 0,
                    })
                    .collect(),
            });
        }
        let folder = self.resolve(dir)?;
        let root = self
            .roots
            .iter()
            .map(|(_, root)| root)
            .find(|root| folder.starts_with(root))
            .ok_or_else(TransferError::unknown_folder)?;
        let read = std::fs::read_dir(&folder).map_err(|_| TransferError::unknown_folder())?;
        let mut entries = Vec::new();
        for item in read.flatten() {
            let name = item.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            // Follow a link only to see where it lands; list it only when
            // that is inside the same root.
            let Ok(target) = item.path().canonicalize() else {
                continue;
            };
            if !target.starts_with(root) {
                continue;
            }
            let Ok(metadata) = std::fs::metadata(&target) else {
                continue;
            };
            if metadata.is_dir() {
                entries.push(BrowseEntry {
                    name,
                    dir: true,
                    size: 0,
                });
            } else if metadata.is_file() && crate::library::is_video(&target) {
                entries.push(BrowseEntry {
                    name,
                    dir: false,
                    size: metadata.len(),
                });
            }
        }
        entries.sort_by(|a, b| {
            b.dir
                .cmp(&a.dir)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
                .then_with(|| a.name.cmp(&b.name))
        });
        Ok(BrowseListing {
            dir: dir.strip_suffix('/').unwrap_or(dir).to_owned(),
            entries,
        })
    }
}

/// The link download for a pasted URL, as the desktop's paste-link sheet
/// starts it. Internet Archive items go to yt-dlp as a page.
fn fetch_source(url: &str) -> Result<FetchSource, TransferError> {
    match crate::fetch::web_link(url) {
        None => Err(TransferError::new(
            400,
            "Paste a web link that starts with https://",
        )),
        Some(Err(error)) => Err(TransferError::new(400, format!("{error:#}"))),
        Some(Ok(WebLink::Archive(_))) => Ok(FetchSource::Page(url.trim().to_owned())),
        Some(Ok(link)) => link
            .source()
            .ok_or_else(|| TransferError::new(400, "This link cannot be downloaded")),
    }
}

/// Link downloads in the shape `GET /v1/fetches` lists them, fed by
/// `fetch::FetchUpdate`s. Finished rows are kept (newest [`FETCH_HISTORY`])
/// until cancelled, which for a finished row means cleared.
#[derive(Default)]
pub struct FetchTable {
    rows: Mutex<BTreeMap<u64, (FetchRow, CancellationToken)>>,
    next: AtomicU64,
}

/// Finished link downloads kept in the list.
pub const FETCH_HISTORY: usize = 50;

impl FetchTable {
    /// Adds a row and returns its id and cancellation token.
    pub fn insert(&self, url: &str) -> (u64, CancellationToken) {
        let id = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        let cancel = CancellationToken::new();
        let mut rows = self
            .rows
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        rows.insert(
            id,
            (
                FetchRow {
                    id,
                    url: url.to_owned(),
                    ..FetchRow::default()
                },
                cancel.clone(),
            ),
        );
        let finished: Vec<u64> = rows
            .iter()
            .filter(|(_, (row, _))| Self::finished(row.phase))
            .map(|(id, _)| *id)
            .collect();
        for old in finished
            .iter()
            .take(finished.len().saturating_sub(FETCH_HISTORY))
        {
            rows.remove(old);
        }
        (id, cancel)
    }

    fn finished(phase: FetchPhase) -> bool {
        matches!(
            phase,
            FetchPhase::Done | FetchPhase::Failed | FetchPhase::Cancelled
        )
    }

    /// Jobs still running.
    pub fn running(&self) -> usize {
        self.rows
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .values()
            .filter(|(row, _)| !Self::finished(row.phase))
            .count()
    }

    /// Applies one update. `saved_id` turns a saved path into a library id.
    pub fn update(
        &self,
        id: u64,
        update: &FetchUpdate,
        saved_id: impl Fn(&Path) -> Option<String>,
    ) {
        let mut rows = self
            .rows
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let Some((row, _)) = rows.get_mut(&id) else {
            return;
        };
        if Self::finished(row.phase) {
            return;
        }
        match update {
            FetchUpdate::Title(title) => row.title = title.clone(),
            FetchUpdate::Progress(progress) => {
                row.phase = match progress.stage {
                    FetchStage::Starting => FetchPhase::Resolving,
                    FetchStage::Downloading => FetchPhase::Downloading,
                    FetchStage::Processing | FetchStage::Verifying => FetchPhase::Merging,
                };
                row.downloaded_bytes = progress.downloaded_bytes;
                row.total_bytes = progress.total_bytes;
                row.bytes_per_second = progress.bytes_per_second;
            }
            FetchUpdate::Saved(path) => {
                if let Some(video) = saved_id(path) {
                    row.saved.push(video);
                }
            }
            FetchUpdate::Done => {
                row.phase = FetchPhase::Done;
                row.bytes_per_second = None;
            }
            FetchUpdate::Failed(message) => {
                row.phase = FetchPhase::Failed;
                row.error = Some(message.clone());
                row.bytes_per_second = None;
            }
            FetchUpdate::Cancelled => {
                row.phase = FetchPhase::Cancelled;
                row.bytes_per_second = None;
            }
        }
    }

    pub fn list(&self) -> Vec<FetchRow> {
        self.rows
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .values()
            .map(|(row, _)| row.clone())
            .collect()
    }

    /// Cancels a running job, or clears a finished one. False when unknown.
    pub fn cancel(&self, id: u64) -> bool {
        let mut rows = self
            .rows
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        match rows.get(&id) {
            None => false,
            Some((row, _)) if Self::finished(row.phase) => {
                rows.remove(&id);
                true
            }
            Some((_, cancel)) => {
                cancel.cancel();
                true
            }
        }
    }

    /// Cancels every running job.
    pub fn cancel_all(&self) {
        for (_, cancel) in self
            .rows
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .values()
        {
            cancel.cancel();
        }
    }
}

// ---------------------------------------------------------------------------
// The headless backend
// ---------------------------------------------------------------------------

/// How long a resolve may wait for peers to supply metadata.
pub const RESOLVE_TIMEOUT: Duration = Duration::from_secs(5 * 60);
/// Resolves (and starts that need one) running at once.
pub const MAX_RESOLVES: usize = 3;
/// Link downloads running at once.
pub const MAX_FETCHES: usize = 3;
/// Resolved torrents kept so a start right after a resolve does not ask the
/// peers again.
const RESOLVED_CACHE: usize = 16;
const RESOLVED_TTL: Duration = Duration::from_secs(10 * 60);
/// How often completed downloads are written to the download list.
const RECORD_INTERVAL: Duration = Duration::from_secs(5);
/// How long stopping waits for link downloads to remove partial files.
const FETCH_STOP_GRACE: Duration = Duration::from_secs(5);

/// The headless server's transfer settings. See [`serve_backend`].
#[derive(Clone, Debug)]
pub struct BackendOptions {
    /// The session's download folder. Torrents go to `<dir>/<infohash>`,
    /// link downloads directly into it; the listing serves what is here.
    pub download_dir: PathBuf,
    /// Folders paired Flicks may browse and share from. Empty: no shares.
    pub share_roots: Vec<PathBuf>,
    /// Explicit peers for every resolve and resume. Tests only; empty when
    /// public discovery finds peers.
    pub initial_peers: Vec<SocketAddr>,
    /// The server's id salt (`Salt::load_or_create(state_dir)`), so rows and
    /// fetches name videos by their listing ids.
    pub salt: Salt,
    /// The server's `remote-downloads.json`, kept when the server serves a
    /// fixed list (`--library`) rather than scanning the download folder.
    pub downloads_list: Option<PathBuf>,
    /// Answer link downloads (yt-dlp and HTTPS). Set only when yt-dlp was found.
    pub links: bool,
    /// Bound on each resolve; [`RESOLVE_TIMEOUT`] by default.
    pub resolve_timeout: Duration,
    /// Stops the backend: running link downloads are cancelled.
    pub stop: CancellationToken,
}

impl BackendOptions {
    pub fn new(download_dir: PathBuf, salt: Salt) -> Self {
        Self {
            download_dir,
            share_roots: Vec::new(),
            initial_peers: Vec::new(),
            salt,
            downloads_list: None,
            links: false,
            resolve_timeout: RESOLVE_TIMEOUT,
            stop: CancellationToken::new(),
        }
    }

    /// What the server should advertise for these options.
    pub fn capabilities(&self) -> Vec<Capability> {
        let mut capabilities = vec![Capability::Downloads, Capability::StreamIncomplete];
        if self.links {
            capabilities.push(Capability::Links);
        }
        if !ShareRoots::new(&self.share_roots).is_empty() {
            capabilities.push(Capability::Shares);
        }
        capabilities
    }
}

/// Answers every [`TransferCommand`] over `session` until `receiver` closes
/// or `options.stop` fires. Each request runs on its own task, so a slow
/// resolve never holds up a listing. A resolve, a stream opening, a browse,
/// or a share whose HTTP request goes away is cancelled.
pub async fn serve_backend(
    session: Arc<TransferSession>,
    options: BackendOptions,
    mut receiver: mpsc::Receiver<TransferRequest>,
) {
    let stop = options.stop.clone();
    let headless = Arc::new(Headless::new(session, options));
    let mut tasks = tokio::task::JoinSet::new();
    let mut record = tokio::time::interval(RECORD_INTERVAL);
    record.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            request = receiver.recv() => match request {
                Some(request) => {
                    let headless = headless.clone();
                    tasks.spawn(async move { headless.answer(request).await });
                }
                None => break,
            },
            () = stop.cancelled() => break,
            _ = record.tick() => headless.record_completed(),
            Some(_) = tasks.join_next(), if !tasks.is_empty() => {}
        }
    }
    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
    headless.stop_fetches().await;
    headless.record_completed();
}

struct Headless {
    session: Arc<TransferSession>,
    download_dir: PathBuf,
    share_roots: ShareRoots,
    initial_peers: Vec<SocketAddr>,
    downloads_list: Option<PathBuf>,
    links: bool,
    resolve_timeout: Duration,
    rows: TransferRows,
    resolving: Arc<Semaphore>,
    resolved: Mutex<Vec<(Instant, String, ResolvedTorrent)>>,
    fetches: Arc<FetchTable>,
    fetch_jobs: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    /// Completed video paths written to `downloads_list`.
    recorded: Mutex<Option<BTreeSet<PathBuf>>>,
    /// Serializes starts so two requests cannot race on one destination.
    starting: tokio::sync::Mutex<()>,
}

impl Headless {
    fn new(session: Arc<TransferSession>, options: BackendOptions) -> Self {
        let download_dir = options
            .download_dir
            .canonicalize()
            .unwrap_or(options.download_dir);
        Self {
            rows: TransferRows::new(options.salt, Some(download_dir.clone())),
            session,
            share_roots: ShareRoots::new(&options.share_roots),
            initial_peers: options.initial_peers,
            downloads_list: options.downloads_list,
            links: options.links,
            resolve_timeout: options.resolve_timeout,
            resolving: Arc::new(Semaphore::new(MAX_RESOLVES)),
            resolved: Mutex::new(Vec::new()),
            fetches: Arc::new(FetchTable::default()),
            fetch_jobs: Mutex::new(Vec::new()),
            recorded: Mutex::new(None),
            starting: tokio::sync::Mutex::new(()),
            download_dir,
        }
    }

    async fn answer(self: Arc<Self>, request: TransferRequest) {
        let TransferRequest { command, mut reply } = request;
        // Work that only reads, or that is safe to abandon, stops when the
        // client goes away. Starts and removals always finish.
        let abandonable = matches!(
            command,
            TransferCommand::Resolve { .. }
                | TransferCommand::OpenStream { .. }
                | TransferCommand::Browse { .. }
                | TransferCommand::Share { .. }
        );
        let work = self.handle(command);
        let result = if abandonable {
            tokio::select! {
                result = work => result,
                () = reply.closed() => return,
            }
        } else {
            work.await
        };
        let _ = reply.send(result);
    }

    async fn handle(&self, command: TransferCommand) -> Result<TransferReply, TransferError> {
        match command {
            TransferCommand::Resolve { input } => {
                let resolved = self.resolve(&input).await?;
                Ok(TransferReply::Resolved(torrent_info(&resolved)))
            }
            TransferCommand::Start { input, files } => self.start(&input, files).await,
            TransferCommand::List => {
                let rows = self.list()?;
                self.record_completed();
                Ok(TransferReply::Transfers(rows))
            }
            TransferCommand::Pause { id } => {
                self.known(&id)?;
                self.session
                    .pause(&id)
                    .await
                    .map_err(|e| refused(409, "Could not pause this transfer", e))?;
                Ok(TransferReply::Done)
            }
            TransferCommand::Resume { id } => {
                self.known(&id)?;
                self.session
                    .resume(&id, self.initial_peers.clone())
                    .await
                    .map_err(|e| refused(409, "Could not resume this transfer", e))?;
                Ok(TransferReply::Done)
            }
            TransferCommand::PauseAll => {
                self.session
                    .pause_all()
                    .await
                    .map_err(|e| refused(409, "Could not pause every transfer", e))?;
                Ok(TransferReply::Done)
            }
            TransferCommand::ResumeAll => {
                self.session
                    .resume_all(self.initial_peers.clone())
                    .await
                    .map_err(|e| refused(409, "Could not resume every transfer", e))?;
                Ok(TransferReply::Done)
            }
            TransferCommand::Remove {
                id,
                delete_files,
                confirm,
            } => self.remove(&id, delete_files, &confirm).await,
            TransferCommand::FetchStart { url } => self.fetch_start(&url),
            TransferCommand::FetchList => {
                self.links_allowed()?;
                Ok(TransferReply::Fetches(self.fetches.list()))
            }
            TransferCommand::FetchCancel { id } => {
                self.links_allowed()?;
                if self.fetches.cancel(id) {
                    Ok(TransferReply::Done)
                } else {
                    Err(TransferError::new(
                        404,
                        "This server has no link download with this id. Refresh Downloads.",
                    ))
                }
            }
            TransferCommand::Browse { dir } => {
                self.shares_allowed()?;
                let roots = self.share_roots.clone();
                tokio::task::spawn_blocking(move || roots.browse(&dir))
                    .await
                    .map_err(|_| TransferError::new(500, "Listing the folder stopped"))?
                    .map(TransferReply::Browse)
            }
            TransferCommand::Share { dir } => {
                self.shares_allowed()?;
                let roots = self.share_roots.clone();
                let source = tokio::task::spawn_blocking(move || roots.resolve(&dir))
                    .await
                    .map_err(|_| TransferError::new(500, "Finding the folder stopped"))??;
                let shared = self
                    .session
                    .share(&source, Vec::new())
                    .await
                    .map_err(|e| refused(409, "Could not share this folder", e))?;
                Ok(TransferReply::Shared(ShareResult {
                    infohash: shared.infohash,
                    magnet: shared.magnet,
                }))
            }
            TransferCommand::Storage => {
                let dir = self.download_dir.clone();
                tokio::task::spawn_blocking(move || storage(&dir))
                    .await
                    .map(TransferReply::Storage)
                    .map_err(|_| TransferError::new(500, "Reading free space stopped"))
            }
            TransferCommand::OpenStream { id, file_index } => {
                self.known(&id)?;
                self.session
                    .open_stream(&id, file_index)
                    .await
                    .map(TransferReply::Stream)
                    .map_err(|e| refused(409, "Could not open this file while it downloads", e))
            }
        }
    }

    fn known(&self, id: &str) -> Result<(), TransferError> {
        if is_infohash(id) && self.session.snapshot(id).is_ok() {
            Ok(())
        } else {
            Err(TransferError::unknown_transfer())
        }
    }

    fn links_allowed(&self) -> Result<(), TransferError> {
        if self.links {
            Ok(())
        } else {
            Err(TransferError::not_allowed(what(Capability::Links)))
        }
    }

    fn shares_allowed(&self) -> Result<(), TransferError> {
        if self.share_roots.is_empty() {
            Err(TransferError::not_allowed(what(Capability::Shares)))
        } else {
            Ok(())
        }
    }

    fn list(&self) -> Result<Vec<TransferRow>, TransferError> {
        self.rows
            .rows(&self.session)
            .map_err(|e| refused(503, "Transfers are not available", e))
    }

    /// A cached resolution of this input or infohash, or a new one bounded
    /// by the resolve timeout and the concurrency limit.
    async fn resolve(&self, input: &str) -> Result<ResolvedTorrent, TransferError> {
        let input = input.trim();
        let parsed = parse_input(input)?;
        let key = cache_key(&parsed);
        if let Some(resolved) = self.cached(&key) {
            return Ok(resolved);
        }
        let _permit = self
            .resolving
            .clone()
            .try_acquire_owned()
            .map_err(|_| TransferError::busy("looking up several torrents"))?;
        let resolved = self
            .session
            .resolve(parsed, self.initial_peers.clone(), self.resolve_timeout)
            .await
            .map_err(|error| {
                if error
                    .chain()
                    .any(|cause| cause.is::<tokio::time::error::Elapsed>())
                {
                    TransferError::new(
                        504,
                        "No peers supplied this torrent's details in time. Try a torrent with active seeders, or send the .torrent file.",
                    )
                } else {
                    refused(502, "Could not get this torrent's details", error)
                }
            })?;
        let mut cache = self
            .resolved
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        cache.retain(|(at, _, _)| at.elapsed() < RESOLVED_TTL);
        cache.push((Instant::now(), key, resolved.clone()));
        let excess = cache.len().saturating_sub(RESOLVED_CACHE);
        cache.drain(..excess);
        Ok(resolved)
    }

    /// A resolution made in the last few minutes for the same key.
    fn cached(&self, key: &str) -> Option<ResolvedTorrent> {
        self.resolved
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .iter()
            .rev()
            .filter(|(at, _, _)| at.elapsed() < RESOLVED_TTL)
            .find(|(_, cached, resolved)| cached == key || resolved.infohash == key)
            .map(|(_, _, resolved)| resolved.clone())
    }

    async fn start(&self, input: &str, files: Vec<usize>) -> Result<TransferReply, TransferError> {
        let resolved = self.resolve(input).await?;
        let _starting = self.starting.lock().await;
        if self.session.snapshot(&resolved.infohash).is_ok() {
            return Err(TransferError::new(
                409,
                "This torrent is already on this server. See it in Downloads.",
            ));
        }
        let selection = if files.is_empty() {
            None
        } else {
            let mut seen = BTreeSet::new();
            if !files
                .iter()
                .all(|index| *index < resolved.files.len() && seen.insert(*index))
            {
                return Err(TransferError::new(
                    400,
                    "A chosen file is not in this torrent",
                ));
            }
            Some(files)
        };
        let id = self
            .session
            .download(&resolved, selection)
            .await
            .map_err(|e| refused(409, "Could not start this download", e))?;
        Ok(TransferReply::Started(Started { id }))
    }

    async fn remove(
        &self,
        id: &str,
        delete_files: bool,
        confirm: &[String],
    ) -> Result<TransferReply, TransferError> {
        self.known(id)?;
        if delete_files {
            let row = self
                .list()?
                .into_iter()
                .find(|row| row.id == id)
                .ok_or_else(TransferError::unknown_transfer)?;
            if !confirm_matches(&row, confirm) {
                return Err(TransferError::new(
                    409,
                    "The files to delete have changed. Check the list and confirm again.",
                ));
            }
        }
        self.session
            .remove(id, delete_files)
            .await
            .map_err(|e| refused(409, "Could not remove this transfer", e))?;
        self.record_completed();
        Ok(TransferReply::Done)
    }

    fn fetch_start(&self, url: &str) -> Result<TransferReply, TransferError> {
        self.links_allowed()?;
        let source = fetch_source(url)?;
        if self.fetches.running() >= MAX_FETCHES {
            return Err(TransferError::busy("running several link downloads"));
        }
        let (id, cancel) = self.fetches.insert(url.trim());
        let table = self.fetches.clone();
        let folder = self.download_dir.clone();
        let salt = *self.rows.salt();
        let root = self.download_dir.clone();
        let list = self.downloads_list.clone();
        let saved_id = move |path: &Path| {
            let path = path.canonicalize().ok()?;
            (crate::library::is_video(&path) && path.starts_with(&root))
                .then(|| ids::video_id(&salt, &path))
        };
        let job = tokio::spawn(async move {
            let saved = Mutex::new(Vec::new());
            let result = crate::fetch::fetch(source, &folder, &cancel, |update| {
                if let FetchUpdate::Saved(path) = &update {
                    saved
                        .lock()
                        .unwrap_or_else(|poison| poison.into_inner())
                        .push(path.clone());
                }
                table.update(id, &update, &saved_id);
            })
            .await;
            let update = match result {
                Ok(_) => FetchUpdate::Done,
                Err(_) if cancel.is_cancelled() => FetchUpdate::Cancelled,
                Err(error) => FetchUpdate::Failed(format!("{error:#}")),
            };
            if let Some(list) = list {
                let saved = saved
                    .into_inner()
                    .unwrap_or_else(|poison| poison.into_inner());
                if let Err(error) = append_downloads(&list, &folder, &saved) {
                    tracing::warn!("Could not record a link download: {error:#}");
                }
            }
            table.update(id, &update, &saved_id);
        });
        let mut jobs = self
            .fetch_jobs
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        jobs.retain(|job| !job.is_finished());
        jobs.push(job);
        Ok(TransferReply::FetchStarted { id })
    }

    async fn stop_fetches(&self) {
        self.fetches.cancel_all();
        let jobs = std::mem::take(
            &mut *self
                .fetch_jobs
                .lock()
                .unwrap_or_else(|poison| poison.into_inner()),
        );
        for mut job in jobs {
            if tokio::time::timeout(FETCH_STOP_GRACE, &mut job)
                .await
                .is_err()
            {
                job.abort();
            }
        }
    }

    /// Writes completed torrent videos under the download folder to the
    /// download list, and forgets paths whose files are gone. Only for a
    /// server that serves a fixed list.
    fn record_completed(&self) {
        let Some(list) = &self.downloads_list else {
            return;
        };
        let Ok(snapshots) = self.session.snapshots() else {
            return;
        };
        let mut recorded = self
            .recorded
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let current = match recorded.take() {
            Some(current) => current,
            None => match crate::remote::library::read_path_list(list) {
                Ok(paths) => paths.into_iter().collect(),
                Err(error) => {
                    tracing::warn!("Not recording downloads: {error:#}");
                    return;
                }
            },
        };
        let mut next: BTreeSet<PathBuf> = current.iter().filter(|p| p.is_file()).cloned().collect();
        for snapshot in &snapshots {
            if matches!(
                snapshot.state,
                TransferState::Checking | TransferState::Error
            ) {
                continue;
            }
            for file in &snapshot.files {
                if file.selected
                    && file.verified_bytes == file.length
                    && crate::library::is_video(&file.path)
                    && file.path.starts_with(&self.download_dir)
                    && file.path.is_file()
                {
                    next.insert(file.path.clone());
                }
            }
        }
        if next != current {
            let paths: Vec<PathBuf> = next.iter().cloned().collect();
            if let Err(error) = crate::remote::library::save_path_list(list, &paths) {
                tracing::warn!("Could not record completed downloads: {error:#}");
                *recorded = Some(current);
                return;
            }
        }
        *recorded = Some(next);
    }
}

/// What a resolution is cached under: the infohash when the input names
/// one, so a large `.torrent` is never kept as text, else the URL.
fn cache_key(input: &TorrentInput) -> String {
    match input {
        TorrentInput::Magnet(value) => librqbit::Magnet::parse(value)
            .ok()
            .and_then(|magnet| magnet.as_id20())
            .map(|id| id.as_string())
            .unwrap_or_else(|| value.clone()),
        TorrentInput::Bytes(bytes) => librqbit::torrent_from_bytes(bytes)
            .map(|torrent| torrent.info_hash.as_string())
            .unwrap_or_default(),
        TorrentInput::Url(url) => url.clone(),
        TorrentInput::File(path) => path.display().to_string(),
    }
}

/// Adds saved link-download videos under `root` to a download list.
fn append_downloads(list: &Path, root: &Path, saved: &[PathBuf]) -> anyhow::Result<()> {
    let mut paths: BTreeSet<PathBuf> = crate::remote::library::read_path_list(list)?
        .into_iter()
        .collect();
    let before = paths.len();
    for path in saved {
        if let Ok(path) = path.canonicalize()
            && path.starts_with(root)
            && crate::library::is_video(&path)
        {
            paths.insert(path);
        }
    }
    if paths.len() != before {
        crate::remote::library::save_path_list(list, &paths.into_iter().collect::<Vec<_>>())?;
    }
    Ok(())
}

/// An adapter failure as a refusal with `status`. The adapter's messages
/// are written for people and name no paths a client did not send.
fn refused(status: u16, what: &str, error: anyhow::Error) -> TransferError {
    TransferError::new(status, format!("{what}: {error:#}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_round_trips_standard_alphabet() {
        assert_eq!(decode_base64("").unwrap(), b"");
        assert_eq!(decode_base64("Zg==").unwrap(), b"f");
        assert_eq!(decode_base64("Zm8=").unwrap(), b"fo");
        assert_eq!(decode_base64("Zm9v").unwrap(), b"foo");
        assert_eq!(decode_base64("Zm9vYg").unwrap(), b"foob");
        assert_eq!(decode_base64("/+8=").unwrap(), [0xff, 0xef]);
        assert!(decode_base64("Z").is_none());
        assert!(decode_base64("Zm9v YmFy").is_none());
        assert!(decode_base64("Zm9-").is_none());
    }

    #[test]
    fn inputs_are_magnets_hashes_https_or_torrent_bytes_only() {
        let hash = "0123456789abcdef0123456789abcdef01234567";
        assert!(matches!(parse_input(hash), Ok(TorrentInput::Magnet(_))));
        assert!(matches!(
            parse_input(&format!("magnet:?xt=urn:btih:{hash}")),
            Ok(TorrentInput::Magnet(_))
        ));
        assert!(matches!(
            parse_input("https://example.com/a.torrent"),
            Ok(TorrentInput::Url(_))
        ));
        for bad in [
            "",
            "/etc/passwd",
            "~/Downloads/a.torrent",
            "C:\\a.torrent",
            "http://example.com/a.torrent",
            "https://user:pw@example.com/a.torrent",
            "file:///a.torrent",
            "base64:!!!!",
            "base64:Zm9v",
            "magnet:?xt=urn:btih:nothex",
        ] {
            let error = parse_input(bad).err().unwrap_or_else(|| panic!("{bad}"));
            assert_eq!(error.status, 400, "{bad}");
        }
    }

    #[test]
    fn virtual_dirs_never_look_like_paths() {
        assert_eq!(virtual_parts(""), Some(vec![]));
        assert_eq!(virtual_parts("Movies"), Some(vec!["Movies"]));
        assert_eq!(virtual_parts("Movies/a b/"), Some(vec!["Movies", "a b"]));
        for bad in [
            "/Movies",
            "Movies/../x",
            "Movies/./x",
            "Movies//x",
            "..",
            "C:/x",
            "C:",
            "Movies\\x",
            "~/x",
            "a\0b",
        ] {
            assert_eq!(virtual_parts(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn fetch_ids_are_plain_decimals() {
        assert_eq!(parse_fetch_id("1"), Some(1));
        assert_eq!(parse_fetch_id("0"), Some(0));
        for bad in ["", "01", "+1", "-1", "1a", "99999999999999999999999"] {
            assert_eq!(parse_fetch_id(bad), None, "{bad}");
        }
    }

    #[test]
    fn confirm_lists_must_match_selected_files_exactly() {
        let row = TransferRow {
            files: vec![
                TransferFileRow {
                    index: 0,
                    path: "a.mp4".into(),
                    selected: true,
                    ..Default::default()
                },
                TransferFileRow {
                    index: 1,
                    path: "b.txt".into(),
                    selected: false,
                    ..Default::default()
                },
                TransferFileRow {
                    index: 2,
                    path: "dir/c.mkv".into(),
                    selected: true,
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        assert!(confirm_matches(&row, &["dir/c.mkv".into(), "a.mp4".into()]));
        assert!(!confirm_matches(&row, &["a.mp4".into()]));
        assert!(!confirm_matches(
            &row,
            &["a.mp4".into(), "dir/c.mkv".into(), "b.txt".into()]
        ));
        assert!(!confirm_matches(
            &row,
            &["a.mp4".into(), "a.mp4".into(), "dir/c.mkv".into()]
        ));
    }

    #[test]
    fn fetch_rows_follow_updates_and_clear_when_finished() {
        let table = FetchTable::default();
        let (id, cancel) = table.insert("https://example.com/v");
        assert_eq!(table.running(), 1);
        table.update(id, &FetchUpdate::Title("Clip".into()), |_| None);
        table.update(
            id,
            &FetchUpdate::Progress(crate::fetch::FetchProgress {
                stage: FetchStage::Downloading,
                downloaded_bytes: 5,
                total_bytes: Some(10),
                ..Default::default()
            }),
            |_| None,
        );
        table.update(id, &FetchUpdate::Saved("/x/clip.mp4".into()), |_| {
            Some("a".repeat(40))
        });
        let row = &table.list()[0];
        assert_eq!(row.phase, FetchPhase::Downloading);
        assert_eq!((row.title.as_str(), row.downloaded_bytes), ("Clip", 5));
        assert_eq!(row.saved, ["a".repeat(40)]);
        assert!(table.cancel(id));
        assert!(cancel.is_cancelled());
        table.update(id, &FetchUpdate::Cancelled, |_| None);
        assert_eq!(table.running(), 0);
        assert!(table.cancel(id));
        assert!(table.list().is_empty());
        assert!(!table.cancel(id));
    }
}
