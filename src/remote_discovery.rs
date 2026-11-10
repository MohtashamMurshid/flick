//! Finding Flicks on the tailnet, pairing with a six-digit code, and waking
//! a machine that is asleep. Owned by the desktop-modules agent; wired into
//! the settings, onboarding, and library views by the integration agents.
//!
//! Everything here is plain networking with no GPUI: the Tailscale CLI for
//! peers, `GET /v1/hello` to recognise a Flick, `POST /v1/pair` to turn a
//! code into a pairing string, and a UDP magic packet to wake a machine.

#![allow(dead_code)]

use anyhow::Result;
use flick::remote::protocol::{Hello, Pairing};
use std::net::IpAddr;

/// A machine on the tailnet, from `tailscale status --json`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Peer {
    /// MagicDNS host name without the suffix, e.g. `mohtashams-mac-mini`.
    pub host: String,
    pub ip: IpAddr,
    pub online: bool,
    /// What Tailscale reports, e.g. `macOS`.
    pub os: String,
}

/// A peer that answered `/v1/hello` on the default port.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FoundFlick {
    pub peer: Peer,
    pub hello: Hello,
}

/// Peers of this tailnet, through the Tailscale CLI (searched on `PATH`,
/// then the Mac App Store app's bundled CLI, then `/usr/local/bin`).
pub fn tailscale_peers() -> Result<Vec<Peer>> {
    anyhow::bail!("not implemented")
}

/// Asks one address whether a Flick is listening; None when it is not.
pub async fn hello(base_url: &str) -> Option<Hello> {
    let _ = base_url;
    None
}

/// Probes every online peer on the default port, a few at a time, within a
/// bounded time.
pub async fn discover(peers: Vec<Peer>) -> Vec<FoundFlick> {
    let _ = peers;
    Vec::new()
}

/// Exchanges a code the owner showed for the full pairing string.
pub async fn pair_with_code(base_url: &str, code: &str) -> Result<Pairing> {
    let _ = (base_url, code);
    anyhow::bail!("not implemented")
}

/// Sends a Wake-on-LAN magic packet for each MAC to the LAN broadcast and
/// to each LAN address. Only works from the same network.
pub fn wake(mac: &[String], lan_ips: &[String]) -> Result<()> {
    let _ = (mac, lan_ips);
    anyhow::bail!("not implemented")
}
