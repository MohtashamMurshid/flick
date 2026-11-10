//! Commands a paired Flick may send to this server: start and manage
//! torrents, link downloads, and shares, and read an unfinished download.
//! Design: `docs/REMOTE_DOWNLOADS_PLAN.md`.
//!
//! The HTTP routes (owned by the server-routes agent) talk to a
//! [`TransferBackend`], which is a channel. Two things answer on it:
//! - the headless `flick-serve`, directly over `crate::torrent::TransferSession`
//!   and `crate::fetch` (owned by the server-routes agent, in this module);
//! - the desktop app, by translating to its `Services` worker (owned by the
//!   desktop-server agent, in `src/remote_serve.rs`).
//!
//! Every write route is refused with 403 unless the owner turned on
//! "Let paired Flicks download to this Mac"; the backend's capabilities say
//! what is allowed and the listing advertises them.

use super::protocol::{
    BrowseListing, Capability, FetchRow, ShareResult, Started, Storage, TorrentInfo, TransferRow,
};
use tokio::sync::{mpsc, oneshot};

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
                "This Flick does not accept {what} from other Flicks. Its owner can allow it in Appearance → Remote Libraries."
            ),
        )
    }
}

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
    /// away answers 503.
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

/// The `/v1/transfers`, `/v1/fetches`, `/v1/browse`, and `/v1/shares`
/// routes. The server mounts it after its own routes like the HLS router;
/// authentication is layered by the server. Placeholder until the
/// server-routes agent lands it.
pub fn router(backend: Option<TransferBackend>) -> axum::Router {
    let _ = backend;
    axum::Router::new()
}
