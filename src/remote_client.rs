//! Desktop side of remote libraries: talks to paired servers on the
//! services runtime, caches listings and posters, and hands the player a URL.
//!
//! Owned by the desktop client agent. `RemoteState` is the one field the
//! `Flick` view holds for this feature; add what you need inside it so other
//! agents' edits to `src/ui.rs` do not conflict.
//!
//! Intended responsibilities:
//! - Parse pairing strings (`flick::remote::protocol::Pairing`) into
//!   `crate::store::remote::RemoteServer` entries with a fresh random id.
//! - Fetch `GET /v1/library` per server off the UI thread, publish typed
//!   results through a channel the view drains (see how `Services` and
//!   `Thumbnails` publish), and keep an on-disk cache of the last listing in
//!   the Flick data directory so the section renders before the first reply.
//! - Fetch poster bytes and decode them into `gpui::RenderImage`s.
//! - Fetch sidecar and embedded subtitles as temporary files for `sub-add`.
//! - Fetch the OpenSubtitles hash from the server.
//! - Build the player source: base URL plus
//!   `flick::remote::protocol::paths::playback(id, quality)`, and the
//!   `http-header-fields` value `flick::remote::protocol::bearer(token)`.

#[derive(Default)]
pub struct RemoteState {}
