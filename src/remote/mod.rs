//! Remote library over Tailscale: one Flick serves its library, another plays
//! from it. Design and acceptance criteria are in `docs/REMOTE_PLAN.md`.
//!
//! Nothing here depends on GPUI or libmpv, so it builds and tests with
//! `--no-default-features`, like `torrent` and `fetch`.
//!
//! Module ownership during the initial implementation:
//! - `protocol`, `bind`, `auth::token_matches` are the shared contract and are
//!   already final; change them only with a matching client change.
//! - `ids`, `auth`, `library`, `server` are the server core.
//! - `transcode`, `hls` are the quality tiers.
//! - `pairing` holds six-digit pairing codes and the rate limit of the
//!   routes without a token; `hardware` reads this machine's Wake-on-LAN
//!   addresses; `cache` caps what the state folder keeps.

pub mod auth;
pub mod bind;
pub mod cache;
pub mod hardware;
pub mod hls;
pub mod ids;
pub mod library;
pub mod pairing;
pub mod protocol;
pub mod server;
pub mod transcode;
pub mod transfers;
