//! HLS delivery of transcodes: playlists, the segment job manager, and the
//! `/v1/hls` routes.
//!
//! Owned by the transcode agent. The server agent mounts [`router`] after
//! its own routes have been given their state:
//!
//! ```ignore
//! let app = Router::new()
//!     .route("/v1/library", get(listing))
//!     // ...
//!     .with_state(state.clone())
//!     .merge(crate::remote::hls::router(hls_state));
//! ```
//!
//! Authentication is applied by the server agent as a layer over the merged
//! router, so this module does not check tokens itself.
//!
//! Intended API:
//!
//! ```ignore
//! pub struct HlsState { encoder: Option<Encoder>, max_jobs: usize, temp_dir: PathBuf,
//!                       resolve: Arc<dyn Fn(&str) -> Option<ResolvedVideo> + Send + Sync> }
//! pub fn playlist(duration: f64, quality: Quality) -> String; // VOD, HLS_SEGMENT_SECONDS segments
//! pub fn router(state: Arc<HlsState>) -> axum::Router;
//! ```
//!
//! Routes: `GET /v1/hls/{id}/{quality}/index.m3u8`, `/init.mp4`, `/{n}.m4s`.
//! A segment that does not exist yet starts or repositions the job for this
//! (id, quality), waits for the file to be complete, then serves it. Jobs
//! stop after 60 s without a request, their temporary directory is removed,
//! and at most `max_jobs` run at once; beyond that, 503 with an
//! `ErrorBody` the client can show.

/// Placeholder until the transcode agent lands the real router.
pub fn router() -> axum::Router {
    axum::Router::new()
}
