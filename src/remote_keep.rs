//! "Keep on this Mac": copy a remote video to this computer for travel, show
//! the copy in the library, and remove it later. Owned by the
//! desktop-modules agent; the library view wires the menu items.
//!
//! The copy is streamed from `GET /v1/media/{id}` with the bearer header
//! into `<download folder>/From <server name>/<file name>` through a
//! temporary file, with progress, cancel, and resume by range on retry. On
//! completion the integration agent adds the path to the library and records
//! a `store::remote::KeptCopy`.

#![allow(dead_code)]

use std::path::PathBuf;

/// One copy in progress.
#[derive(Clone, Debug, PartialEq)]
pub struct KeepProgress {
    pub server_id: String,
    pub video_id: String,
    pub downloaded: u64,
    pub total: Option<u64>,
    pub bytes_per_second: u64,
}

/// What the view learns from `drain`.
#[derive(Clone, Debug, PartialEq)]
pub enum KeepEvent {
    Progress(KeepProgress),
    Done {
        server_id: String,
        video_id: String,
        path: PathBuf,
    },
    Failed {
        server_id: String,
        video_id: String,
        message: String,
    },
    Cancelled {
        server_id: String,
        video_id: String,
    },
}

/// What to fetch and where.
#[derive(Clone, Debug, PartialEq)]
pub struct KeepRequest {
    pub server_id: String,
    pub server_name: String,
    pub video_id: String,
    pub file_name: String,
    pub url: String,
    /// `Authorization: Bearer …`; never logged.
    pub authorization: String,
    pub download_dir: PathBuf,
}

/// Owns a worker thread; the view holds one and drains it on tick.
#[derive(Default)]
pub struct KeepState {}

impl KeepState {
    pub fn keep(&mut self, request: KeepRequest) {
        let _ = request;
    }

    pub fn cancel(&mut self, server_id: &str, video_id: &str) {
        let _ = (server_id, video_id);
    }

    pub fn in_progress(&self) -> Vec<KeepProgress> {
        Vec::new()
    }

    pub fn drain(&mut self) -> Vec<KeepEvent> {
        Vec::new()
    }

    pub fn shutdown(&mut self) {}
}
