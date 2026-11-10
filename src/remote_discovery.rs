//! Finding Flicks on the tailnet, pairing with a six-digit code, and waking
//! a machine that is asleep. Owned by the desktop-modules agent; wired into
//! the settings, onboarding, and library views by the integration agents.
//!
//! Everything here is plain networking with no GPUI: the Tailscale CLI for
//! peers, `GET /v1/hello` to recognise a Flick, `POST /v1/pair` to turn a
//! code into a pairing string, and a UDP magic packet to wake a machine.

use anyhow::{Context, Result, bail};
use flick::remote::protocol::{self, ErrorBody, Hello, PairReply, PairRequest, Pairing};
use serde::Deserialize;
use std::{
    collections::HashMap,
    io::Read,
    net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket},
    process::{Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

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

/// Where the Tailscale command line is looked for, in order (the same list
/// as `flick::remote::bind`).
const TAILSCALE_COMMANDS: [&str; 3] = [
    "tailscale",
    "/Applications/Tailscale.app/Contents/MacOS/Tailscale",
    "/usr/local/bin/tailscale",
];

/// How long `tailscale status --json` may take.
const STATUS_TIMEOUT: Duration = Duration::from_secs(10);
/// How long one `/v1/hello` may take.
const HELLO_TIMEOUT: Duration = Duration::from_secs(3);
/// How long a pairing request may take.
const PAIR_TIMEOUT: Duration = Duration::from_secs(10);
/// Peers probed at once during discovery.
const PROBES_IN_FLIGHT: usize = 8;
/// Upper bound on one discovery pass.
const DISCOVERY_BOUND: Duration = Duration::from_secs(10);
/// Wake-on-LAN's conventional "discard" port.
const WAKE_PORT: u16 = 9;
/// The most a hello or pairing answer may be; both are a few hundred bytes.
const MAX_BODY: usize = 64 * 1024;
/// The most hardware addresses, and LAN addresses, a wake uses.
pub const MAX_WAKE_ADDRESSES: usize = 8;

/// Peers of this tailnet, through the Tailscale CLI (searched on `PATH`,
/// then the Mac App Store app's bundled CLI, then `/usr/local/bin`).
pub fn tailscale_peers() -> Result<Vec<Peer>> {
    let mut problems = Vec::new();
    for command in TAILSCALE_COMMANDS {
        match run_with_timeout(command, &["status", "--json"], STATUS_TIMEOUT) {
            Ok(stdout) => match parse_status(&stdout) {
                Ok(peers) => return Ok(peers),
                Err(error) => problems.push(format!("{command}: {error:#}")),
            },
            Err(error) => problems.push(format!("{command}: {error:#}")),
        }
    }
    bail!(
        "Could not ask Tailscale for the machines on this tailnet. Start Tailscale and try again. ({})",
        problems.join("; ")
    )
}

/// Runs a command with no input and returns its standard output, killing it
/// after `timeout`.
fn run_with_timeout(program: &str, args: &[&str], timeout: Duration) -> Result<Vec<u8>> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut stdout = child.stdout.take().context("no standard output")?;
    let mut stderr = child.stderr.take().context("no standard error")?;
    let out = thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stdout.read_to_end(&mut bytes);
        bytes
    });
    let err = thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stderr.read_to_end(&mut bytes);
        bytes
    });
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if started.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            bail!("timed out");
        }
        thread::sleep(Duration::from_millis(20));
    };
    let stdout = out.join().unwrap_or_default();
    let stderr = err.join().unwrap_or_default();
    if !status.success() {
        let stderr = String::from_utf8_lossy(&stderr);
        bail!("{}", stderr.trim());
    }
    Ok(stdout)
}

#[derive(Deserialize)]
struct Status {
    #[serde(rename = "Self")]
    this: Option<Node>,
    #[serde(rename = "Peer", default)]
    peers: Option<HashMap<String, Node>>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Node {
    #[serde(rename = "HostName")]
    host_name: String,
    #[serde(rename = "DNSName")]
    dns_name: String,
    #[serde(rename = "TailscaleIPs")]
    ips: Option<Vec<String>>,
    #[serde(rename = "Online")]
    online: bool,
    #[serde(rename = "OS")]
    os: String,
    /// Shared into this tailnet from another one; never probed.
    #[serde(rename = "ShareeNode")]
    sharee: bool,
}

/// The peers in `tailscale status --json` output, without this machine,
/// sorted by host name. A peer without an address is skipped, and so is a
/// machine shared in from another tailnet (`ShareeNode`): it is not the
/// owner's, so discovery does not knock on it.
fn parse_status(json: &[u8]) -> Result<Vec<Peer>> {
    let status: Status =
        serde_json::from_slice(json).context("Tailscale's status is not the expected JSON")?;
    let own: Vec<IpAddr> = status.this.as_ref().map(node_ips).unwrap_or_default();
    let mut peers: Vec<Peer> = status
        .peers
        .unwrap_or_default()
        .into_values()
        .filter(|node| !node.sharee)
        .filter_map(|node| {
            let ips = node_ips(&node);
            if ips.iter().any(|ip| own.contains(ip)) {
                return None;
            }
            // The first address, preferring IPv4 as Tailscale lists it first.
            let ip = ips
                .iter()
                .copied()
                .find(IpAddr::is_ipv4)
                .or_else(|| ips.first().copied())?;
            Some(Peer {
                host: host_name(&node),
                ip,
                online: node.online,
                os: node.os,
            })
        })
        .collect();
    peers.sort_by(|a, b| a.host.cmp(&b.host).then(a.ip.cmp(&b.ip)));
    Ok(peers)
}

fn node_ips(node: &Node) -> Vec<IpAddr> {
    node.ips
        .iter()
        .flatten()
        .filter_map(|ip| ip.parse().ok())
        .collect()
}

/// The MagicDNS label (`mohtashams-mac-mini` from
/// `mohtashams-mac-mini.tailnet.ts.net.`); `HostName` is the machine's
/// display name, used only when there is no DNS name.
fn host_name(node: &Node) -> String {
    node.dns_name
        .split('.')
        .next()
        .map(str::trim)
        .filter(|label| !label.is_empty())
        .unwrap_or(node.host_name.trim())
        .to_owned()
}

fn http_client(timeout: Duration) -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        // A system proxy cannot reach tailnet addresses.
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(timeout)
        .timeout(timeout)
        .user_agent(concat!("Flick v", env!("CARGO_PKG_VERSION")))
        .build()
}

/// `http://host:port`, with IPv6 hosts in brackets.
fn base_url(host: &str, port: u16) -> String {
    format!("http://{}", crate::remote_client::address(host, port))
}

/// Asks one address whether a Flick is listening; None when it is not.
pub async fn hello(base_url: &str) -> Option<Hello> {
    let http = http_client(HELLO_TIMEOUT).ok()?;
    let url = format!(
        "{}{}",
        base_url.trim_end_matches('/'),
        protocol::paths::hello()
    );
    let response = tokio::time::timeout(HELLO_TIMEOUT, http.get(url).send())
        .await
        .ok()?
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    let body = tokio::time::timeout(HELLO_TIMEOUT, read_bounded(response))
        .await
        .ok()?
        .ok()?;
    let hello: Hello = serde_json::from_slice(&body).ok()?;
    // Every field has a default, so any JSON object would parse; a Flick
    // always states its protocol.
    (hello.protocol > 0).then_some(hello)
}

/// Why a body could not be read.
#[derive(Debug)]
enum BodyError {
    TooLarge,
    Closed,
}

/// The body, refusing one over [`MAX_BODY`] without reading it all.
async fn read_bounded(mut response: reqwest::Response) -> Result<Vec<u8>, BodyError> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_BODY as u64)
    {
        return Err(BodyError::TooLarge);
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| BodyError::Closed)? {
        if body.len() + chunk.len() > MAX_BODY {
            return Err(BodyError::TooLarge);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// Probes every online peer on the default port, a few at a time, within a
/// bounded time.
pub async fn discover(peers: Vec<Peer>) -> Vec<FoundFlick> {
    let targets = peers
        .into_iter()
        .filter(|peer| peer.online)
        .map(|peer| {
            let url = base_url(&peer.ip.to_string(), protocol::DEFAULT_PORT);
            (peer, url)
        })
        .collect();
    probe(targets, PROBES_IN_FLIGHT, DISCOVERY_BOUND).await
}

/// Asks each `(peer, base URL)` for `/v1/hello`, at most `in_flight` at
/// once, and returns whatever answered within `bound`, sorted by host.
async fn probe(targets: Vec<(Peer, String)>, in_flight: usize, bound: Duration) -> Vec<FoundFlick> {
    let deadline = tokio::time::Instant::now() + bound;
    let mut waiting = targets.into_iter();
    let mut running = tokio::task::JoinSet::new();
    let mut found = Vec::new();
    let mut start_next = |running: &mut tokio::task::JoinSet<_>| {
        if let Some((peer, url)) = waiting.next() {
            running.spawn(async move { hello(&url).await.map(|hello| FoundFlick { peer, hello }) });
        }
    };
    for _ in 0..in_flight.max(1) {
        start_next(&mut running);
    }
    loop {
        match tokio::time::timeout_at(deadline, running.join_next()).await {
            Ok(Some(result)) => {
                if let Ok(Some(flick)) = result {
                    found.push(flick);
                }
                start_next(&mut running);
            }
            Ok(None) => break,
            Err(_) => {
                running.abort_all();
                break;
            }
        }
    }
    found.sort_by(|a, b| {
        a.peer
            .host
            .cmp(&b.peer.host)
            .then(a.peer.ip.cmp(&b.peer.ip))
    });
    found
}

/// Exchanges a code the owner showed for the full pairing string. The
/// answer must name the machine the code was sent to: the same host, or a
/// host resolving to the same address, and the same port.
pub async fn pair_with_code(host: &str, port: u16, code: &str) -> Result<Pairing> {
    let code = normalize_code(code)?;
    let http = http_client(PAIR_TIMEOUT)?;
    let url = format!("{}{}", base_url(host, port), protocol::paths::pair());
    let response = http
        .post(url)
        .json(&PairRequest { code })
        .send()
        .await
        .map_err(|_| anyhow::anyhow!("No Flick answered at that address"))?;
    let status = response.status().as_u16();
    let body = read_bounded(response).await.map_err(|error| match error {
        BodyError::TooLarge => anyhow::anyhow!("The server's answer is not a Flick pairing"),
        BodyError::Closed => anyhow::anyhow!("The connection closed before the server answered"),
    })?;
    if !(200..300).contains(&status) {
        bail!("{}", pair_error(status, &body));
    }
    let reply: PairReply = serde_json::from_slice(&body)
        .map_err(|_| anyhow::anyhow!("The server's answer is not a Flick pairing"))?;
    let pairing = Pairing::parse(&reply.pairing)
        .map_err(|_| anyhow::anyhow!("The server sent a pairing string this Flick cannot read"))?;
    if !names_the_same_machine(host, port, &pairing).await {
        bail!(
            "The Flick at {} answered with a pairing for another address. Paste its pairing string instead",
            crate::remote_client::address(host, port)
        );
    }
    Ok(pairing)
}

/// Whether a pairing names `host:port`: the same port, and the same host
/// or one resolving to an address `host` resolves to.
async fn names_the_same_machine(host: &str, port: u16, pairing: &Pairing) -> bool {
    if pairing.port != port {
        return false;
    }
    let (expected, named) = (bare_host(host), bare_host(&pairing.host));
    if expected.eq_ignore_ascii_case(named) {
        return true;
    }
    let lookup = async {
        let (expected, named) = tokio::join!(resolve(expected, port), resolve(named, port));
        named.iter().any(|ip| expected.contains(ip))
    };
    tokio::time::timeout(PAIR_TIMEOUT, lookup)
        .await
        .unwrap_or(false)
}

/// A host without the brackets around an IPv6 address.
fn bare_host(host: &str) -> &str {
    let host = host.trim();
    host.strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host)
}

/// The addresses a host names; nothing when it does not resolve.
async fn resolve(host: &str, port: u16) -> Vec<IpAddr> {
    if let Ok(ip) = host.parse::<IpAddr>() {
        return vec![ip];
    }
    match tokio::net::lookup_host((host, port)).await {
        Ok(addresses) => addresses.map(|address| address.ip()).collect(),
        Err(_) => Vec::new(),
    }
}

/// Six digits, ignoring spaces and dashes the person may type.
fn normalize_code(code: &str) -> Result<String> {
    let digits: String = code
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .collect();
    if digits.len() != 6 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        bail!("A pairing code is six digits");
    }
    Ok(digits)
}

/// The server's message for a refused code, else a sentence for the status.
fn pair_error(status: u16, body: &[u8]) -> String {
    let message = serde_json::from_slice::<ErrorBody>(body)
        .ok()
        .map(|body| body.message.trim().chars().take(160).collect::<String>())
        .filter(|message| !message.is_empty());
    message.unwrap_or_else(|| match status {
        400 | 401 | 403 => "That code was not accepted. Check it on the other Flick".into(),
        404 => "No pairing code is open on that Flick.".into(),
        429 => "Too many attempts. Wait a minute, then try again".into(),
        500..=599 => format!("Server error ({status})"),
        _ => format!("Unexpected reply ({status})"),
    })
}

/// Sends a Wake-on-LAN magic packet for each MAC to the LAN broadcast and
/// to each LAN address. Only works from the same network. Whatever the
/// server listed is filtered first ([`wake_macs`], [`wake_ips`]): a packet
/// never goes to a public, broadcast, or multicast address it named.
pub fn wake(mac: &[String], lan_ips: &[String]) -> Result<()> {
    let macs = parse_macs(mac)?;
    let mut targets = vec![SocketAddr::from((Ipv4Addr::BROADCAST, WAKE_PORT))];
    targets.extend(
        wake_ips(lan_ips)
            .into_iter()
            .map(|ip| SocketAddr::from((ip, WAKE_PORT))),
    );
    send_magic(&macs, &targets)
}

/// The usable hardware addresses, skipping malformed ones; an error only
/// when none is left.
fn parse_macs(macs: &[String]) -> Result<Vec<[u8; 6]>> {
    if macs.is_empty() {
        bail!("This machine has not shared a hardware address to wake it with");
    }
    let parsed: Vec<[u8; 6]> = wake_macs(macs)
        .iter()
        .filter_map(|mac| parse_mac(mac))
        .collect();
    if parsed.is_empty() {
        bail!("{:?} is not a hardware address", macs[0]);
    }
    Ok(parsed)
}

/// The hardware addresses worth waking: well formed, one device's own (not
/// all zeros, not group addresses), without repeats, at most
/// [`MAX_WAKE_ADDRESSES`].
pub fn wake_macs(macs: &[String]) -> Vec<String> {
    let mut kept: Vec<String> = Vec::new();
    for mac in macs {
        let Some(bytes) = parse_mac(mac) else {
            continue;
        };
        if bytes == [0; 6] || bytes[0] & 1 == 1 {
            continue;
        }
        let mac = mac.trim().to_owned();
        if !kept.iter().any(|seen| parse_mac(seen) == Some(bytes)) {
            kept.push(mac);
        }
        if kept.len() == MAX_WAKE_ADDRESSES {
            break;
        }
    }
    kept
}

/// The LAN addresses a wake packet may go to: private IPv4 (10/8,
/// 172.16/12, 192.168/16) only, never a broadcast or network address (`.255`
/// or `.0`), multicast, or public address, without repeats, at most
/// [`MAX_WAKE_ADDRESSES`].
pub fn wake_ips(ips: &[String]) -> Vec<Ipv4Addr> {
    let mut kept = Vec::new();
    for ip in ips {
        let Ok(ip) = ip.trim().parse::<Ipv4Addr>() else {
            continue;
        };
        let last = ip.octets()[3];
        if !ip.is_private()
            || ip.is_broadcast()
            || ip.is_multicast()
            || last == 255
            || last == 0
            || kept.contains(&ip)
        {
            continue;
        }
        kept.push(ip);
        if kept.len() == MAX_WAKE_ADDRESSES {
            break;
        }
    }
    kept
}

/// `aa:bb:cc:dd:ee:ff` or `aa-bb-cc-dd-ee-ff`.
fn parse_mac(mac: &str) -> Option<[u8; 6]> {
    let mac = mac.trim();
    let separator = if mac.contains('-') { '-' } else { ':' };
    let mut bytes = [0u8; 6];
    let mut parts = mac.split(separator);
    for byte in &mut bytes {
        let part = parts.next()?;
        if part.len() != 2 {
            return None;
        }
        *byte = u8::from_str_radix(part, 16).ok()?;
    }
    parts.next().is_none().then_some(bytes)
}

/// Six 0xFF bytes, then the MAC sixteen times: 102 bytes.
fn magic_packet(mac: [u8; 6]) -> [u8; 102] {
    let mut packet = [0xFF; 102];
    for chunk in packet[6..].as_chunks_mut::<6>().0 {
        chunk.copy_from_slice(&mac);
    }
    packet
}

/// Sends every packet to every target; succeeds when at least one send did.
fn send_magic(macs: &[[u8; 6]], targets: &[SocketAddr]) -> Result<()> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).context("Could not open a socket")?;
    socket
        .set_broadcast(true)
        .context("Could not enable broadcast")?;
    let mut sent = 0;
    let mut problem = None;
    for mac in macs {
        let packet = magic_packet(*mac);
        for target in targets {
            match socket.send_to(&packet, target) {
                Ok(_) => sent += 1,
                Err(error) => {
                    tracing::debug!(%target, %error, "Wake packet not sent");
                    problem = Some(error);
                }
            }
        }
    }
    match (sent, problem) {
        (0, Some(error)) => Err(error).context("Could not send the wake packet"),
        (0, None) => bail!("There was nowhere to send the wake packet"),
        _ => Ok(()),
    }
}

/// What the view learns from [`DiscoveryState::drain`].
#[derive(Clone, Debug, PartialEq)]
pub enum DiscoveryEvent {
    /// A discovery pass finished; offline peers and non-Flicks are omitted.
    Found(Vec<FoundFlick>),
    /// A pairing code was accepted (`Ok`, with a pairing checked to name
    /// `host:port`) or refused with the server's message.
    Paired {
        host: String,
        port: u16,
        result: Result<Pairing, String>,
    },
    /// A wake packet was sent, or could not be.
    Woke {
        server_id: String,
        result: Result<(), String>,
    },
}

enum Request {
    Discover,
    Pair {
        host: String,
        port: u16,
        code: String,
    },
    Wake {
        server_id: String,
        mac: Vec<String>,
        lan_ips: Vec<String>,
    },
}

struct Worker {
    requests: tokio::sync::mpsc::UnboundedSender<Request>,
    events: mpsc::Receiver<DiscoveryEvent>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Worker {
    fn start() -> Self {
        let (requests, mut receiver) = tokio::sync::mpsc::unbounded_channel::<Request>();
        let (sender, events) = mpsc::channel();
        let spawned = thread::Builder::new()
            .name("flick-discovery".into())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(1)
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        tracing::warn!(%error, "Discovery could not start");
                        return;
                    }
                };
                runtime.block_on(async move {
                    while let Some(request) = receiver.recv().await {
                        let sender = sender.clone();
                        tokio::spawn(async move {
                            let event = handle(request).await;
                            let _ = sender.send(event);
                        });
                    }
                });
                runtime.shutdown_timeout(Duration::from_millis(500));
            });
        let thread = match spawned {
            Ok(thread) => Some(thread),
            Err(error) => {
                tracing::warn!(%error, "Discovery could not start");
                None
            }
        };
        Self {
            requests,
            events,
            thread,
        }
    }
}

async fn handle(request: Request) -> DiscoveryEvent {
    match request {
        Request::Discover => {
            let peers = match tokio::task::spawn_blocking(tailscale_peers).await {
                Ok(Ok(peers)) => peers,
                Ok(Err(error)) => {
                    tracing::warn!("{error:#}");
                    Vec::new()
                }
                Err(_) => Vec::new(),
            };
            DiscoveryEvent::Found(discover(peers).await)
        }
        Request::Pair { host, port, code } => {
            // Only a pairing that names `host:port` comes back as `Ok`.
            let result = pair_with_code(&host, port, &code)
                .await
                .map_err(|error| error.to_string());
            DiscoveryEvent::Paired { host, port, result }
        }
        Request::Wake {
            server_id,
            mac,
            lan_ips,
        } => {
            let result = tokio::task::spawn_blocking(move || wake(&mac, &lan_ips))
                .await
                .unwrap_or_else(|_| Err(anyhow::anyhow!("The wake packet could not be sent")))
                .map_err(|error| format!("{error:#}"));
            DiscoveryEvent::Woke { server_id, result }
        }
    }
}

/// Owns a worker thread for discovery, pairing by code, and waking; the
/// view holds one and drains it on tick.
#[derive(Default)]
pub struct DiscoveryState {
    worker: Option<Worker>,
    discovering: bool,
}

impl DiscoveryState {
    /// Starts a discovery pass; results arrive as [`DiscoveryEvent::Found`].
    /// Does nothing while a pass is already running.
    pub fn discover(&mut self) {
        if self.discovering {
            return;
        }
        self.discovering = self.send(Request::Discover);
    }

    /// Whether a pass is running, for a spinner.
    pub fn discovering(&self) -> bool {
        self.discovering
    }

    pub fn pair_by_code(&mut self, host: &str, port: u16, code: &str) {
        let request = Request::Pair {
            host: host.to_owned(),
            port,
            code: code.to_owned(),
        };
        if !self.send(request) {
            tracing::warn!("Pairing by code is unavailable");
        }
    }

    pub fn wake(&mut self, server_id: &str, mac: Vec<String>, lan_ips: Vec<String>) {
        let request = Request::Wake {
            server_id: server_id.to_owned(),
            mac,
            lan_ips,
        };
        if !self.send(request) {
            tracing::warn!("Waking is unavailable");
        }
    }

    pub fn drain(&mut self) -> Vec<DiscoveryEvent> {
        let Some(worker) = &self.worker else {
            return Vec::new();
        };
        let events: Vec<_> = worker.events.try_iter().collect();
        if events
            .iter()
            .any(|event| matches!(event, DiscoveryEvent::Found(_)))
        {
            self.discovering = false;
        }
        events
    }

    /// Stops the worker; requests in flight are abandoned.
    pub fn shutdown(&mut self) {
        self.discovering = false;
        if let Some(mut worker) = self.worker.take() {
            drop(worker.requests);
            if let Some(thread) = worker.thread.take() {
                let _ = thread.join();
            }
        }
    }

    /// Starts the worker on first use; false when it has stopped.
    fn send(&mut self, request: Request) -> bool {
        let worker = self.worker.get_or_insert_with(Worker::start);
        worker.requests.send(request).is_ok()
    }
}

impl Drop for DiscoveryState {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{BufRead, BufReader, Write},
        net::{TcpListener, TcpStream},
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
    };

    const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    /// Shaped like real `tailscale status --json` output, trimmed, with
    /// made-up machines: this one, an online Mac, an offline Linux box, and
    /// a NAS shared in from another tailnet.
    const STATUS: &str = r#"{
      "Version": "1.90.6-t0123456789-g0123456789",
      "BackendState": "Running",
      "TailscaleIPs": ["100.98.200.69", "fd7a:115c:a1e0::6c2e:c846"],
      "Self": {
        "ID": "nSelf",
        "HostName": "Studio Mac mini",
        "DNSName": "studio-mac-mini.example.ts.net.",
        "OS": "macOS",
        "TailscaleIPs": ["100.98.200.69", "fd7a:115c:a1e0::6c2e:c846"],
        "Online": true
      },
      "MagicDNSSuffix": "example.ts.net",
      "Peer": {
        "nodekey:aaaa": {
          "ID": "nA",
          "HostName": "Travel MacBook",
          "DNSName": "travel-macbook.example.ts.net.",
          "OS": "macOS",
          "TailscaleIPs": ["100.109.155.32", "fd7a:115c:a1e0::cb2e:9b21"],
          "Online": true,
          "Relay": "sin"
        },
        "nodekey:eeee": {
          "ID": "nShared",
          "HostName": "friends-nas",
          "DNSName": "friends-nas.other-tailnet.ts.net.",
          "OS": "linux",
          "TailscaleIPs": ["100.77.1.2"],
          "Online": true,
          "ShareeNode": true
        },
        "nodekey:bbbb": {
          "ID": "nB",
          "HostName": "media-box",
          "DNSName": "",
          "OS": "linux",
          "TailscaleIPs": ["fd7a:115c:a1e0::5b2e:1b62", "100.98.27.97"],
          "Online": false
        },
        "nodekey:cccc": {
          "ID": "nSelfAgain",
          "HostName": "Studio Mac mini",
          "DNSName": "studio-mac-mini.example.ts.net.",
          "OS": "macOS",
          "TailscaleIPs": ["100.98.200.69"],
          "Online": true
        },
        "nodekey:dddd": {
          "HostName": "no-address",
          "TailscaleIPs": null,
          "Online": true
        }
      },
      "User": {}
    }"#;

    #[test]
    fn status_json_lists_peers_without_this_machine() {
        let peers = parse_status(STATUS.as_bytes()).unwrap();
        assert_eq!(
            peers,
            vec![
                Peer {
                    host: "media-box".into(),
                    ip: "100.98.27.97".parse().unwrap(),
                    online: false,
                    os: "linux".into(),
                },
                Peer {
                    host: "travel-macbook".into(),
                    ip: "100.109.155.32".parse().unwrap(),
                    online: true,
                    os: "macOS".into(),
                },
            ]
        );
    }

    /// Checks the parser against real output saved by hand:
    /// `tailscale status --json > status.json`, then
    /// `FLICK_TAILSCALE_STATUS=status.json cargo test … -- --ignored saved_status`.
    #[test]
    #[ignore = "needs a saved `tailscale status --json` file"]
    fn parses_a_saved_status_file() {
        let path = std::env::var("FLICK_TAILSCALE_STATUS").expect("FLICK_TAILSCALE_STATUS");
        let json = std::fs::read(path).unwrap();
        let peers = parse_status(&json).unwrap();
        assert!(!peers.is_empty());
        for peer in &peers {
            assert!(!peer.host.is_empty());
            assert!(!peer.host.contains('.'), "{}", peer.host);
            assert!(flick::remote::bind::is_tailscale(peer.ip), "{}", peer.ip);
        }
        let online = peers.iter().filter(|peer| peer.online).count();
        println!("{} peers, {online} online", peers.len());
    }

    #[test]
    fn status_json_without_peers_or_with_garbage() {
        assert_eq!(
            parse_status(br#"{"Self": {"TailscaleIPs": ["100.1.2.3"]}}"#).unwrap(),
            Vec::new()
        );
        assert_eq!(parse_status(br#"{"Peer": null}"#).unwrap(), Vec::new());
        assert!(parse_status(b"not json").is_err());
    }

    /// A one-request-per-connection HTTP server on loopback.
    /// `answer(own port, method, path, body)` gives the status line code and
    /// body.
    fn loopback_server(
        answer: impl Fn(u16, &str, &str, &str) -> (u16, String) + Send + Sync + 'static,
    ) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let answer = Arc::new(answer);
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let answer = answer.clone();
                thread::spawn(move || serve(stream, &*answer));
            }
        });
        format!("http://127.0.0.1:{port}")
    }

    /// `(own port, method, path, body)` to `(status, body)`.
    type Answer = dyn Fn(u16, &str, &str, &str) -> (u16, String);

    fn serve(mut stream: TcpStream, answer: &Answer) {
        let own_port = stream.local_addr().map_or(0, |addr| addr.port());
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut line = String::new();
        if reader.read_line(&mut line).is_err() {
            return;
        }
        let mut words = line.split_whitespace();
        let method = words.next().unwrap_or("").to_owned();
        let path = words.next().unwrap_or("").to_owned();
        let mut length = 0;
        loop {
            let mut header = String::new();
            if reader.read_line(&mut header).is_err() || header.trim().is_empty() {
                break;
            }
            if let Some((name, value)) = header.split_once(':')
                && name.eq_ignore_ascii_case("content-length")
            {
                length = value.trim().parse().unwrap_or(0);
            }
        }
        let mut body = vec![0; length];
        let _ = reader.read_exact(&mut body);
        let (code, reply) = answer(own_port, &method, &path, &String::from_utf8_lossy(&body));
        let _ = write!(
            stream,
            "HTTP/1.1 {code} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
            reply.len()
        );
    }

    /// A fake Flick that pairs with code 123456, naming its own address.
    /// Codes 900001 to 900004 answer with a pairing for another host,
    /// another port, `localhost`, and a host that does not resolve.
    fn fake_flick() -> String {
        loopback_server(|port, method, path, body| match (method, path) {
            ("GET", "/v1/hello") => (
                200,
                r#"{"protocol":1,"server_name":"Mac mini","pairing_open":true}"#.into(),
            ),
            ("POST", "/v1/pair") => {
                let request: PairRequest = serde_json::from_str(body).unwrap();
                let authority = match request.code.as_str() {
                    "123456" => Some(format!("127.0.0.1:{port}")),
                    "900001" => Some(format!("100.98.200.69:{port}")),
                    "900002" => Some(format!("127.0.0.1:{}", port.wrapping_add(1))),
                    "900003" => Some(format!("localhost:{port}")),
                    "900004" => Some(format!("no-such-host.invalid:{port}")),
                    _ => None,
                };
                if let Some(authority) = authority {
                    (
                        200,
                        format!(r#"{{"pairing":"flick-remote://{authority}/#{TOKEN}"}}"#),
                    )
                } else {
                    (
                        403,
                        r#"{"message":"That code is not right. 4 tries left."}"#.into(),
                    )
                }
            }
            _ => (404, r#"{"message":"not found"}"#.into()),
        })
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    /// A loopback port with nothing listening.
    fn closed_port() -> u16 {
        TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    #[test]
    fn hello_recognises_a_flick_and_nothing_else() {
        let rt = runtime();
        let flick = fake_flick();
        assert_eq!(
            rt.block_on(hello(&flick)),
            Some(Hello {
                protocol: 1,
                server_name: "Mac mini".into(),
                pairing_open: true,
            })
        );
        let other = loopback_server(|_, _, _, _| (200, r#"{"hello":"world"}"#.into()));
        assert_eq!(rt.block_on(hello(&other)), None);
        let missing = loopback_server(|_, _, _, _| (404, String::new()));
        assert_eq!(rt.block_on(hello(&missing)), None);
        let nothing = format!("http://127.0.0.1:{}", closed_port());
        assert_eq!(rt.block_on(hello(&nothing)), None);
    }

    fn port_of(base_url: &str) -> u16 {
        base_url.rsplit(':').next().unwrap().parse().unwrap()
    }

    /// Pairs with the loopback server at `base_url` by code.
    fn pair(rt: &tokio::runtime::Runtime, base_url: &str, code: &str) -> Result<Pairing> {
        rt.block_on(pair_with_code("127.0.0.1", port_of(base_url), code))
    }

    #[test]
    fn pairing_with_the_right_code_returns_the_pairing() {
        let rt = runtime();
        let flick = fake_flick();
        let pairing = pair(&rt, &flick, " 123-456 ").unwrap();
        assert_eq!(pairing.host, "127.0.0.1");
        assert_eq!(pairing.port, port_of(&flick));
        assert_eq!(pairing.token, TOKEN);
        // A name for the same address is the same machine.
        let pairing = pair(&rt, &flick, "900003").unwrap();
        assert_eq!(pairing.host, "localhost");
    }

    #[test]
    fn a_pairing_for_another_address_is_refused() {
        let rt = runtime();
        let flick = fake_flick();
        // Another host, another port, and a host that resolves to nothing.
        for code in ["900001", "900002", "900004"] {
            let error = pair(&rt, &flick, code).unwrap_err().to_string();
            assert!(
                error.contains("pairing for another address"),
                "{code}: {error}"
            );
            assert!(!error.contains(TOKEN), "{error}");
        }
    }

    #[test]
    fn the_same_machine_is_checked_by_host_or_address() {
        let rt = runtime();
        let named = |host: &str, port: u16| Pairing {
            host: host.into(),
            port,
            token: TOKEN.into(),
        };
        let check = |host: &str, port: u16, pairing: Pairing| {
            rt.block_on(names_the_same_machine(host, port, &pairing))
        };
        assert!(check("Box.ts.net", 1, named("box.ts.net", 1)));
        assert!(check("::1", 1, named("[::1]", 1)));
        assert!(check("127.0.0.1", 1, named("localhost", 1)));
        assert!(!check("box.ts.net", 1, named("box.ts.net", 2)));
        assert!(!check("127.0.0.1", 1, named("127.0.0.2", 1)));
        assert!(!check("100.98.200.69", 1, named("100.98.200.70", 1)));
    }

    #[test]
    fn pairing_with_a_wrong_code_shows_the_server_message() {
        let rt = runtime();
        let flick = fake_flick();
        let error = pair(&rt, &flick, "654321").unwrap_err();
        assert_eq!(error.to_string(), "That code is not right. 4 tries left.");
        let error = pair(&rt, &flick, "12345").unwrap_err();
        assert_eq!(error.to_string(), "A pairing code is six digits");
        let silent = loopback_server(|_, _, _, _| (429, String::new()));
        let error = pair(&rt, &silent, "123456").unwrap_err();
        assert!(error.to_string().contains("Too many attempts"), "{error}");
    }

    #[test]
    fn pairing_where_no_flick_listens_fails_clearly() {
        let rt = runtime();
        let nothing = format!("http://127.0.0.1:{}", closed_port());
        let error = pair(&rt, &nothing, "123456").unwrap_err();
        assert_eq!(error.to_string(), "No Flick answered at that address");
        let other = loopback_server(|_, _, _, _| (200, r#"{"pairing":"nonsense"}"#.into()));
        let error = pair(&rt, &other, "123456").unwrap_err();
        assert!(error.to_string().contains("cannot read"), "{error}");
    }

    #[test]
    fn oversized_answers_are_not_read() {
        let rt = runtime();
        let huge = format!(
            r#"{{"protocol":1,"server_name":"{}","pairing_open":true}}"#,
            "x".repeat(MAX_BODY)
        );
        let big = loopback_server(move |_, _, _, _| (200, huge.clone()));
        assert_eq!(rt.block_on(hello(&big)), None);
        let error = pair(&rt, &big, "123456").unwrap_err();
        assert!(error.to_string().contains("not a Flick pairing"), "{error}");
        // Just under the cap is still read.
        let name = "x".repeat(MAX_BODY - 100);
        let reply = format!(r#"{{"protocol":1,"server_name":"{name}","pairing_open":true}}"#);
        let fits = loopback_server(move |_, _, _, _| (200, reply.clone()));
        assert_eq!(
            rt.block_on(hello(&fits)).unwrap().server_name.len(),
            name.len()
        );
    }

    #[test]
    fn the_state_pairs_by_code_through_its_worker() {
        let flick = fake_flick();
        let port = port_of(&flick);
        let mut state = DiscoveryState::default();
        state.pair_by_code("127.0.0.1", port, "000000");
        state.pair_by_code("127.0.0.1", port, "123456");
        state.pair_by_code("127.0.0.1", port, "900001");
        let mut events = Vec::new();
        let started = Instant::now();
        while events.len() < 3 {
            assert!(started.elapsed() < Duration::from_secs(10), "timed out");
            events.extend(state.drain());
            thread::sleep(Duration::from_millis(10));
        }
        let results: Vec<_> = events
            .into_iter()
            .map(|event| match event {
                DiscoveryEvent::Paired {
                    host,
                    port: p,
                    result,
                } => {
                    assert_eq!((host.as_str(), p), ("127.0.0.1", port));
                    result.map(|pairing| pairing.token)
                }
                other => panic!("unexpected {other:?}"),
            })
            .collect();
        assert!(results.contains(&Ok(TOKEN.to_owned())));
        assert!(results.contains(&Err("That code is not right. 4 tries left.".to_owned())));
        // A pairing for another host never reaches the view as `Ok`.
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        state.shutdown();
        assert!(state.drain().is_empty());
    }

    #[test]
    fn the_state_reports_a_wake_that_cannot_be_sent() {
        let mut state = DiscoveryState::default();
        state.wake("server-1", vec!["not a mac".into()], Vec::new());
        let started = Instant::now();
        let event = loop {
            assert!(started.elapsed() < Duration::from_secs(10), "timed out");
            if let Some(event) = state.drain().pop() {
                break event;
            }
            thread::sleep(Duration::from_millis(10));
        };
        match event {
            DiscoveryEvent::Woke { server_id, result } => {
                assert_eq!(server_id, "server-1");
                assert!(result.unwrap_err().contains("not a hardware address"));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    fn peer(host: &str) -> Peer {
        Peer {
            host: host.into(),
            ip: "127.0.0.1".parse().unwrap(),
            online: true,
            os: "macOS".into(),
        }
    }

    #[test]
    fn discovery_is_bounded_and_limits_probes_in_flight() {
        // A listener that accepts and never answers, counting connections
        // open at once.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let silent = format!("http://{}", listener.local_addr().unwrap());
        let open = Arc::new(AtomicUsize::new(0));
        let most = Arc::new(AtomicUsize::new(0));
        {
            let (open, most) = (open.clone(), most.clone());
            thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(mut stream) = stream else { continue };
                    let now = open.fetch_add(1, Ordering::SeqCst) + 1;
                    most.fetch_max(now, Ordering::SeqCst);
                    let open = open.clone();
                    thread::spawn(move || {
                        // Holds the connection until the client gives up.
                        let mut buffer = [0; 1024];
                        while matches!(stream.read(&mut buffer), Ok(n) if n > 0) {}
                        open.fetch_sub(1, Ordering::SeqCst);
                    });
                }
            });
        }
        let flick = fake_flick();
        let mut targets = vec![(peer("answers"), flick)];
        targets.extend((0..20).map(|n| (peer(&format!("silent-{n:02}")), silent.clone())));
        let rt = runtime();
        let started = Instant::now();
        let found = rt.block_on(probe(targets, 8, Duration::from_millis(1500)));
        let took = started.elapsed();
        assert!(took < Duration::from_millis(2500), "took {took:?}");
        assert!(took >= Duration::from_millis(1400), "took {took:?}");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].peer.host, "answers");
        assert_eq!(found[0].hello.server_name, "Mac mini");
        let most = most.load(Ordering::SeqCst);
        assert!((1..=8).contains(&most), "{most} probes at once");
    }

    #[test]
    fn discovery_skips_offline_peers() {
        let rt = runtime();
        let mut offline = peer("asleep");
        offline.online = false;
        // Only offline peers: nothing is probed, so it returns at once.
        let started = Instant::now();
        assert!(rt.block_on(discover(vec![offline])).is_empty());
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn mac_addresses_are_validated() {
        assert_eq!(
            parse_mac("aa:bb:cc:dd:ee:ff"),
            Some([0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff])
        );
        assert_eq!(
            parse_mac("01-23-45-67-89-AB"),
            Some([0x01, 0x23, 0x45, 0x67, 0x89, 0xab])
        );
        for bad in [
            "",
            "aa:bb:cc:dd:ee",
            "aa:bb:cc:dd:ee:ff:00",
            "aa:bb:cc:dd:ee:gg",
            "aabb.ccdd.eeff",
            "a:bb:cc:dd:ee:ff",
            "aa:bb-cc:dd:ee:ff",
        ] {
            assert_eq!(parse_mac(bad), None, "{bad:?}");
        }
        // Validation happens before anything is sent.
        assert!(wake(&["aa:bb:cc:dd:ee:zz".into()], &[]).is_err());
        assert!(wake(&[], &["127.0.0.1".into()]).is_err());
        // Malformed ones are skipped when a good one is left.
        assert_eq!(
            parse_macs(&["nonsense".into(), "aa:bb:cc:dd:ee:ff".into()]).unwrap(),
            vec![[0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff]]
        );
    }

    #[test]
    fn wake_uses_only_private_lan_addresses_and_a_few_macs() {
        let ips: Vec<String> = [
            "192.168.1.20",
            "10.0.0.5",
            "172.20.3.4",
            " 192.168.1.20 ",  // repeat
            "8.8.8.8",         // public
            "172.32.0.1",      // just outside 172.16/12
            "100.98.200.69",   // Tailscale, not LAN
            "127.0.0.1",       // loopback
            "169.254.1.1",     // link-local
            "255.255.255.255", // broadcast
            "192.168.1.255",   // a subnet's broadcast
            "10.0.0.0",        // a network address
            "224.0.0.1",       // multicast
            "fe80::1",         // not IPv4
            "nonsense",
        ]
        .map(String::from)
        .to_vec();
        assert_eq!(
            wake_ips(&ips),
            vec![
                Ipv4Addr::new(192, 168, 1, 20),
                Ipv4Addr::new(10, 0, 0, 5),
                Ipv4Addr::new(172, 20, 3, 4),
            ]
        );
        let many: Vec<String> = (1..=20).map(|n| format!("192.168.0.{n}")).collect();
        assert_eq!(wake_ips(&many).len(), MAX_WAKE_ADDRESSES);

        let macs: Vec<String> = [
            "aa:bb:cc:dd:ee:f0",
            "AA-BB-CC-DD-EE-F0", // the same one
            "ff:ff:ff:ff:ff:ff", // broadcast
            "01:00:5e:00:00:01", // multicast
            "00:00:00:00:00:00",
            "zz:bb:cc:dd:ee:ff",
            "02:00:00:00:00:01",
        ]
        .map(String::from)
        .to_vec();
        assert_eq!(
            wake_macs(&macs),
            vec!["aa:bb:cc:dd:ee:f0", "02:00:00:00:00:01"]
        );
        let many: Vec<String> = (0..20).map(|n| format!("02:00:00:00:00:{n:02x}")).collect();
        assert_eq!(wake_macs(&many).len(), MAX_WAKE_ADDRESSES);
    }

    #[test]
    fn magic_packets_reach_a_loopback_listener() {
        let listener = UdpSocket::bind("127.0.0.1:0").unwrap();
        listener
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let target = listener.local_addr().unwrap();
        let macs = parse_macs(&["aa:bb:cc:dd:ee:ff".into(), "02:03:04:05:06:07".into()]).unwrap();
        send_magic(&macs, &[target]).unwrap();
        for mac in [
            [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff],
            [0x02, 0x03, 0x04, 0x05, 0x06, 0x07],
        ] {
            let mut buffer = [0u8; 200];
            let (length, _) = listener.recv_from(&mut buffer).unwrap();
            assert_eq!(length, 102);
            assert_eq!(&buffer[..6], &[0xFF; 6]);
            for repeat in buffer[6..102].as_chunks::<6>().0 {
                assert_eq!(*repeat, mac);
            }
        }
    }
}
