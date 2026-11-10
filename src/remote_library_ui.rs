//! Remote libraries in the view: a Home section per paired server, remote
//! entries in Continue Watching, remote subtitles and the OpenSubtitles
//! hash in the player, and the "Share my library on Tailscale" switch.
//!
//! Listings, posters, and downloads come from `crate::remote_client`; the
//! in-app server is `crate::remote_serve`.

use super::{
    chrome_ui::{notice, row, section_title, thumbnail},
    library_ui::{GridStep, card_grid, columns},
    *,
};
use crate::remote_client::{
    Command, MachineTransfers, Outcome, Ready, ServerView, Sheet, Status, SubtitleRef,
};
use crate::remote_discovery::{DiscoveryEvent, FoundFlick};
use crate::remote_keep::{KeepEvent, KeepProgress, KeepRequest};
use crate::store::remote::{KeptCopy, QueuedCommand, RemoteServer, WakeTarget};
use flick::library::format_bytes;
use flick::remote::protocol::{Library, Progress, TransferPhase, TransferRow, Video};
use gpui::{AnyElement, FontWeight};
use std::path::Path;

/// Cards per group on Home before "Show All", in rows.
const COLLAPSED_ROWS: usize = 1;

/// View state for remote libraries that is not a setting.
#[derive(Default)]
pub(super) struct RemoteUiState {
    /// Card ids per grid, for arrow keys; rebuilt as grids render.
    grids: RefCell<HashMap<String, Rc<[ElementId]>>>,
    /// Servers whose Home section shows every video.
    expanded: HashSet<String>,
    /// Why a remote video did not open, shown on Home.
    pub(super) notice: Option<String>,
    /// Subtitle downloads for the playing remote video.
    subtitles: HashMap<SubtitleRef, SubtitleLoad>,
    /// OpenSubtitles hashes by (server, video); None when the server could
    /// not compute one.
    hashes: HashMap<(String, String), Option<String>>,
    /// A subtitle search waiting for its hash: server, video, generation.
    awaiting_hash: Option<(String, String, u64)>,
    /// When the pairing string was last copied, for the button's label.
    pub(super) copied: Option<Instant>,
    /// A transcode tier waiting for the server's answer before it loads.
    pub(super) checking: Option<PendingPlayback>,
    /// Save to pickers, sheets for other machines, and remote deletions.
    pub(super) save_to: SaveToUi,
    /// Finding Flicks, pairing by code, and Wake results.
    pub(super) discovery: DiscoveryUi,
    /// A remote card's open context menu.
    pub(super) menu: Option<RemoteCardMenu>,
    /// Copies to this computer that failed, until dismissed or retried.
    pub(super) keep_failed: Vec<KeepFailure>,
}

/// Downloads to another machine: the sheets' state while they are open.
#[derive(Default)]
pub(super) struct SaveToUi {
    /// What the torrent sheet is resolving, so choosing another machine
    /// resolves it there.
    pub(super) torrent_input: Option<flick::torrent::TorrentInput>,
    /// The machine the torrent sheet resolves on; None for this Mac.
    pub(super) torrent_machine: Option<String>,
    /// The torrent sheet for another machine.
    pub(super) remote_torrent: Option<RemoteTorrent>,
    /// The paste-link sheet.
    pub(super) link: Option<LinkSheet>,
    /// The Share sheet.
    pub(super) share: Option<ShareSheet>,
    /// A remote "Remove and Delete Files…" waiting for confirmation.
    pub(super) delete: Option<RemoteDelete>,
    /// The machine whose share link is shown, when it is not this Mac.
    pub(super) shared_on: Option<String>,
    /// Remote rows with a Pause, Resume, or Remove · Keep Files in flight,
    /// by (server id, transfer id), with what the button says meanwhile. A
    /// second click waits for the answer.
    pub(super) busy: HashMap<(String, String), &'static str>,
}

/// A torrent resolved on another machine.
pub(super) struct RemoteTorrent {
    pub(super) server_id: String,
    /// From `remote_client::wire_input`.
    pub(super) input: String,
    /// None while the server finds peers.
    pub(super) info: Option<flick::remote::protocol::TorrentInfo>,
    pub(super) selected: HashSet<usize>,
    /// Download Selected was pressed and the server has not answered.
    pub(super) starting: bool,
    /// The server did not answer the resolve: the question offering to
    /// queue the download with every file.
    pub(super) asleep: Option<String>,
}

impl RemoteTorrent {
    /// Its resolve is still on the server.
    fn resolving(&self) -> bool {
        self.info.is_none() && self.asleep.is_none()
    }
}

/// A pasted video page link waiting for Save to.
pub(super) struct LinkSheet {
    pub(super) link: flick::fetch::WebLink,
    pub(super) url: String,
    /// None for this Mac.
    pub(super) machine: Option<String>,
}

/// Share, with Seed from.
pub(super) struct ShareSheet {
    /// None for this Mac.
    pub(super) machine: Option<String>,
    /// The virtual folder being browsed on a server; empty lists its roots.
    pub(super) dir: String,
    pub(super) listing: Option<flick::remote::protocol::BrowseListing>,
    pub(super) error: Option<String>,
    /// The folder being hashed on the server.
    pub(super) sharing: Option<String>,
}

/// "Remove and Delete Files…" on another machine.
pub(super) struct RemoteDelete {
    pub(super) server_id: String,
    pub(super) id: String,
    /// Exactly the paths shown, echoed to the server.
    pub(super) confirm: Vec<String>,
    /// The server's message when it refused an older list.
    pub(super) changed: Option<String>,
    /// Sent and not answered yet.
    pub(super) sending: bool,
}

/// What Remove · Keep Files says while the server answers.
const REMOVING: &str = "Removing…";

/// Marks a remote row's Pause, Resume, or Remove as in flight. False when
/// one already is, so the click does nothing.
pub(super) fn start_row_command(
    busy: &mut HashMap<(String, String), &'static str>,
    server_id: &str,
    id: &str,
    label: &'static str,
) -> bool {
    use std::collections::hash_map::Entry;
    match busy.entry((server_id.to_owned(), id.to_owned())) {
        Entry::Occupied(_) => false,
        Entry::Vacant(entry) => {
            entry.insert(label);
            true
        }
    }
}

/// Whether a remote row offers "Remove and Delete Files…". A share is the
/// owner's own files, which the server never deletes: Keep Files only.
pub(super) fn offers_delete(row: &TransferRow) -> bool {
    !row.share
}

/// What a deletion's answer leaves: the confirmation to show again, only
/// when the server's list changed, and a refusal to show once. Any other
/// answer closes the confirmation, so a refusal is never asked again.
pub(super) fn delete_answered(
    server_id: String,
    id: String,
    result: Result<Outcome, String>,
) -> (Option<RemoteDelete>, Option<String>) {
    match result {
        Ok(Outcome::ListChanged { message, current }) => (
            Some(RemoteDelete {
                server_id,
                id,
                confirm: current,
                changed: Some(sentence(&message)),
                sending: false,
            }),
            None,
        ),
        Ok(_) => (None, None),
        Err(message) => (None, Some(message)),
    }
}

/// `TorrentInput` is not `Clone`; the torrent sheet keeps a copy to resolve
/// again on another machine.
pub(super) fn copy_input(input: &flick::torrent::TorrentInput) -> flick::torrent::TorrentInput {
    use flick::torrent::TorrentInput;
    match input {
        TorrentInput::Magnet(text) => TorrentInput::Magnet(text.clone()),
        TorrentInput::File(path) => TorrentInput::File(path.clone()),
        TorrentInput::Bytes(bytes) => TorrentInput::Bytes(bytes.clone()),
        TorrentInput::Url(url) => TorrentInput::Url(url.clone()),
    }
}

/// The virtual path of an entry inside a browsed folder.
pub(super) fn browse_child(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_owned()
    } else {
        format!("{}/{name}", dir.trim_end_matches('/'))
    }
}

/// The folder above a virtual path; the roots above a root.
pub(super) fn browse_parent(dir: &str) -> String {
    dir.trim_end_matches('/')
        .rsplit_once('/')
        .map(|(parent, _)| parent.to_owned())
        .unwrap_or_default()
}

/// The files the torrent sheet selects first: the videos, or everything
/// when there are none.
pub(super) fn default_selection(
    files: &[flick::remote::protocol::TorrentFileInfo],
) -> HashSet<usize> {
    let videos: HashSet<usize> = files
        .iter()
        .filter(|file| flick::library::is_video(Path::new(&file.path)))
        .map(|file| file.index)
        .collect();
    if videos.is_empty() {
        files.iter().map(|file| file.index).collect()
    } else {
        videos
    }
}

/// A remote load waiting for [`Ready::Playback`].
pub(super) struct PendingPlayback {
    /// From `RemoteState::check_playback`.
    pub check: u64,
    /// `open_generation` when it was asked; any later open cancels it.
    pub generation: u64,
    pub remote: RemoteSource,
    pub resume: f64,
}

impl RemoteUiState {
    /// Forgets per-video player state when another video (or tier) loads;
    /// libmpv drops added subtitle files with the old file.
    pub(super) fn reset_player(&mut self) {
        self.subtitles.clear();
        self.awaiting_hash = None;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SubtitleLoad {
    Loading,
    Loaded,
    Failed,
}

/// Videos of one server shown together: films, or one show's episodes.
#[derive(Debug, PartialEq)]
pub(super) struct Group<'a> {
    /// None when it is the only group of plain videos.
    pub title: Option<String>,
    pub key: String,
    pub videos: Vec<&'a Video>,
}

/// Whether a remote video matches the search text (already lowercase).
pub(super) fn matches_query(video: &Video, query: &str) -> bool {
    query.is_empty()
        || video.title.to_lowercase().contains(query)
        || video.file_name.to_lowercase().contains(query)
        || video
            .episode
            .as_ref()
            .is_some_and(|episode| episode.show.to_lowercase().contains(query))
}

/// Groups a listing like the local Shows page: plain videos by title, then
/// each show's episodes by season and number. Only matching videos appear,
/// and empty groups are left out.
pub(super) fn group_videos<'a>(library: &'a Library, query: &str) -> Vec<Group<'a>> {
    let mut films = Vec::new();
    let mut shows: Vec<(String, String, Vec<&Video>)> = Vec::new();
    for video in library.videos.iter().filter(|v| matches_query(v, query)) {
        match &video.episode {
            Some(episode) if !episode.show.trim().is_empty() => {
                let key = episode.show.trim().to_lowercase();
                match shows.iter_mut().find(|(k, ..)| *k == key) {
                    Some((.., episodes)) => episodes.push(video),
                    None => shows.push((key, episode.show.trim().to_owned(), vec![video])),
                }
            }
            _ => films.push(video),
        }
    }
    films.sort_by_cached_key(|v| display_title(v).to_lowercase());
    shows.sort_by(|a, b| a.0.cmp(&b.0));
    let mut groups = Vec::new();
    if !films.is_empty() {
        groups.push(Group {
            title: (!shows.is_empty()).then(|| "Videos".to_owned()),
            key: "videos".into(),
            videos: films,
        });
    }
    for (key, show, mut episodes) in shows {
        episodes.sort_by_key(|v| {
            let episode = v.episode.as_ref();
            (
                episode.map_or(0, |e| e.season),
                episode.map_or(0, |e| e.number),
                display_title(v).to_lowercase(),
            )
        });
        groups.push(Group {
            title: Some(show),
            key: format!("show:{key}"),
            videos: episodes,
        });
    }
    groups
}

fn display_title(video: &Video) -> &str {
    if video.title.trim().is_empty() {
        &video.file_name
    } else {
        &video.title
    }
}

/// A card's progress bar fraction and detail line, as local cards show them.
pub(super) fn card_detail(
    video: &Video,
    resume: f64,
    progress: Option<&crate::store::remote::RemoteProgress>,
) -> (Option<f32>, String) {
    let duration = if video.duration > 0. {
        video.duration
    } else {
        progress.map_or(0., |p| p.duration)
    };
    let episode = video
        .episode
        .as_ref()
        .filter(|e| e.number > 0)
        .map(|e| format!("S{} E{} · ", e.season, e.number))
        .unwrap_or_default();
    if !video.complete {
        return (None, format!("{episode}Still downloading on the server"));
    }
    if resume > 0. && duration > 0. {
        return (
            Some((resume / duration).clamp(0., 1.) as f32),
            format!("{episode}{} left", format_time(duration - resume)),
        );
    }
    let state = if progress.is_some_and(|p| p.finished) {
        "Watched"
    } else {
        "Not started"
    };
    if duration > 0. {
        (
            None,
            format!("{episode}{state} · {}", format_time(duration)),
        )
    } else {
        (None, format!("{episode}{state}"))
    }
}

/// Whether a server is known not to answer, so opening a video would fail.
fn unreachable(status: &Status) -> bool {
    !status.is_online() && *status != Status::Connecting
}

/// A message that ends like a sentence, for joining after another one.
fn sentence(message: &str) -> String {
    let message = message.trim();
    if message.ends_with(['.', '!', '?', '…']) {
        message.to_owned()
    } else {
        format!("{message}.")
    }
}

fn card_id(kind: &str, server: &str, video: &str) -> ElementId {
    ElementId::from(("remote-card", gpui::hash(&(kind, server, video))))
}

/// One remote card's contents.
struct Card<'a> {
    kind: &'static str,
    server: &'a str,
    video: &'a str,
    title: &'a str,
    detail: String,
    progress: Option<f32>,
    badge: Option<&'static str>,
}

/// Shown under Add Server and on onboarding step one while a code is typed.
pub(super) const CODE_HINT: &str = "Ask the owner for a code: open one from its Settings → Remote Libraries, or run flick-serve pair-code on that machine.";

/// The discovery list after a pass that found nothing.
pub(super) const NO_FLICKS: &str = "No Flicks found. Tailscale must be running on this computer, and the other Flick must be sharing its library on Tailscale.";

/// Wake's tooltip and the note beside its result.
const WAKE_NOTE: &str = "Wake works only from the same network as the sleeping machine.";

/// Finding Flicks and pairing by code, for Add Server and onboarding step
/// one. Only one of them is on screen at a time, so they share this.
#[derive(Default)]
pub(super) struct DiscoveryUi {
    /// The last pass's answer; None before the first one ends.
    pub(super) found: Option<Vec<FoundFlick>>,
    /// The found Flick whose code is being typed.
    pub(super) code: Option<CodeEntry>,
    /// The last wake result per server id.
    pub(super) woke: HashMap<String, Result<(), String>>,
}

/// The six-digit code field for one found Flick.
pub(super) struct CodeEntry {
    /// The address discovery reached it on.
    pub(super) host: String,
    pub(super) port: u16,
    pub(super) name: String,
    pub(super) field: Entity<TextInput>,
    /// Sent and not answered yet.
    pub(super) sending: bool,
    /// The server's message when it refused the code.
    pub(super) error: Option<String>,
}

/// A found Flick as the list shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct FoundRow {
    pub(super) name: String,
    /// The MagicDNS host, shown under the name.
    pub(super) host: String,
    /// The address pairing goes to.
    pub(super) address: String,
    pub(super) port: u16,
    /// Already paired at this address; no Pair button.
    pub(super) paired: bool,
    /// The server has a code on screen now.
    pub(super) code_open: bool,
}

/// The rows of the discovery list: each answering Flick with its name, and
/// whether it is already paired (by address or MagicDNS name).
pub(super) fn found_rows(found: &[FoundFlick], servers: &[RemoteServer]) -> Vec<FoundRow> {
    let port = flick::remote::protocol::DEFAULT_PORT;
    found
        .iter()
        .map(|flick| {
            let address = flick.peer.ip.to_string();
            let paired = servers.iter().any(|server| {
                server.port == port
                    && (server.host.eq_ignore_ascii_case(&address)
                        || server.host.eq_ignore_ascii_case(&flick.peer.host)
                        || server
                            .host
                            .split('.')
                            .next()
                            .is_some_and(|label| label.eq_ignore_ascii_case(&flick.peer.host)))
            });
            let name = flick.hello.server_name.trim();
            FoundRow {
                name: if name.is_empty() {
                    flick.peer.host.clone()
                } else {
                    name.to_owned()
                },
                host: flick.peer.host.clone(),
                address,
                port,
                paired,
                code_open: flick.hello.pairing_open,
            }
        })
        .collect()
}

/// What the discovery list says above its rows, if anything.
pub(super) fn discovery_note(
    discovering: bool,
    found: Option<&[FoundFlick]>,
) -> Option<&'static str> {
    match (discovering, found) {
        (true, _) => Some("Looking for Flicks on your Tailscale network…"),
        (false, Some([])) => Some(NO_FLICKS),
        _ => None,
    }
}

/// Six digits, ignoring the spaces and dashes a person may type.
pub(super) fn code_ready(text: &str) -> bool {
    let digits = text
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .collect::<Vec<_>>();
    digits.len() == 6 && digits.iter().all(char::is_ascii_digit)
}

/// Where Wake sends its packet: only for a server that does not answer and
/// told us its hardware addresses while it was online.
pub(super) fn wake_target<'a>(server: &'a RemoteServer, status: &Status) -> Option<&'a WakeTarget> {
    server
        .wake
        .as_ref()
        .filter(|wake| *status == Status::Offline && !wake.mac.is_empty())
}

/// The line under an offline server after Wake was pressed.
pub(super) fn wake_message(result: &Result<(), String>) -> String {
    match result {
        Ok(()) => "Wake packet sent; give it a minute.".to_owned(),
        Err(message) => format!("Could not wake it: {} {WAKE_NOTE}", sentence(message)),
    }
}

/// "Keep on this Mac", named like the Save to picker names this machine.
pub(super) fn keep_label() -> &'static str {
    if cfg!(target_os = "macos") {
        "Keep on this Mac"
    } else if cfg!(windows) {
        "Keep on this PC"
    } else {
        "Keep on this computer"
    }
}

/// The Downloads section of copies kept here.
pub(super) fn kept_section_title() -> &'static str {
    if cfg!(target_os = "macos") {
        "Copies to this Mac"
    } else if cfg!(windows) {
        "Copies to this PC"
    } else {
        "Copies to this computer"
    }
}

/// The mark on a remote card whose video is kept here.
pub(super) fn kept_here_badge() -> &'static str {
    if cfg!(target_os = "macos") {
        "On this Mac"
    } else if cfg!(windows) {
        "On this PC"
    } else {
        "On this computer"
    }
}

/// The copy of this remote video kept here, if any.
pub(super) fn kept_copy<'a>(
    kept: &'a [KeptCopy],
    server: &str,
    video: &str,
) -> Option<&'a KeptCopy> {
    kept.iter()
        .find(|copy| copy.server == server && copy.video == video)
}

/// The mark on a local card that is a kept copy: "Kept from Mac mini".
pub(super) fn kept_from(
    kept: &[KeptCopy],
    servers: &[RemoteServer],
    path: &Path,
) -> Option<String> {
    let copy = kept.iter().find(|copy| copy.path == path)?;
    let name = servers
        .iter()
        .find(|server| server.id == copy.server)
        .map_or("another Flick", |server| server.name.as_str());
    Some(format!("Kept from {name}"))
}

/// Drops kept copies whose file has left the library, by Delete File… or
/// Remove from Library. True when one was dropped.
pub(super) fn prune_kept(kept: &mut Vec<KeptCopy>, in_library: impl Fn(&Path) -> bool) -> bool {
    let before = kept.len();
    kept.retain(|copy| in_library(&copy.path));
    kept.len() != before
}

/// Where a remote video stands with this computer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum KeepStatus {
    NotKept,
    Copying,
    Kept(PathBuf),
}

/// One item of a remote card's menu.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum RemoteMenuItem {
    Play,
    Keep,
    CancelCopy,
    ShowCopy(PathBuf),
    RemoveCopy(PathBuf),
}

impl RemoteMenuItem {
    pub(super) fn icon(&self) -> &'static str {
        match self {
            Self::Play => "play",
            Self::Keep => "download",
            Self::CancelCopy | Self::RemoveCopy(_) => "x",
            Self::ShowCopy(_) => "folder-open",
        }
    }

    pub(super) fn label(&self) -> &'static str {
        match self {
            Self::Play => "Play",
            Self::Keep => keep_label(),
            Self::CancelCopy => "Cancel Copy",
            Self::ShowCopy(_) => "Show Local Copy in Folder",
            Self::RemoveCopy(_) => "Remove Local Copy…",
        }
    }
}

/// A remote card's menu. Keep needs the server to answer and the whole
/// file on it; a kept video offers its copy instead.
pub(super) fn remote_menu_items(status: &KeepStatus, can_copy: bool) -> Vec<RemoteMenuItem> {
    let mut items = vec![RemoteMenuItem::Play];
    match status {
        KeepStatus::NotKept if can_copy => items.push(RemoteMenuItem::Keep),
        KeepStatus::NotKept => {}
        KeepStatus::Copying => items.push(RemoteMenuItem::CancelCopy),
        KeepStatus::Kept(path) => {
            items.push(RemoteMenuItem::ShowCopy(path.clone()));
            items.push(RemoteMenuItem::RemoveCopy(path.clone()));
        }
    }
    items
}

/// What "Keep on this Mac" fetches: the original file with the bearer
/// header, into the app's download folder.
pub(super) fn keep_request(
    playback: &crate::remote_client::RemotePlayback,
    file_name: &str,
    download_dir: &Path,
) -> KeepRequest {
    KeepRequest {
        server_id: playback.server_id.clone(),
        server_name: playback.server_name.clone(),
        video_id: playback.video_id.clone(),
        file_name: file_name.to_owned(),
        url: format!(
            "{}{}",
            playback.base_url,
            flick::remote::protocol::paths::media(&playback.video_id)
        ),
        authorization: flick::remote::protocol::bearer(&playback.token),
        download_dir: download_dir.to_owned(),
    }
}

/// A copy row's detail: bytes so far, size, and speed.
pub(super) fn keep_detail(progress: &KeepProgress, server_name: &str) -> String {
    if progress.downloaded == 0 && progress.bytes_per_second == 0 {
        return format!("From {server_name} · waiting to start");
    }
    let mut text = format!("From {server_name} · {}", format_bytes(progress.downloaded));
    if let Some(total) = progress.total.filter(|total| *total > 0) {
        let percent = (progress.downloaded as f64 / total as f64 * 100.).clamp(0., 100.);
        text = format!(
            "From {server_name} · {} of {} ({percent:.0}%)",
            format_bytes(progress.downloaded),
            format_bytes(total)
        );
    }
    if progress.bytes_per_second > 0 {
        text.push_str(&format!(" · {}/s", format_bytes(progress.bytes_per_second)));
    }
    text
}

/// A queued command's title: the link, or the torrent's name when a magnet
/// carries one.
pub(super) fn queued_title(command: &QueuedCommand) -> String {
    let input = command.input.trim();
    if !command.link
        && let Some(query) = input.strip_prefix("magnet:?")
    {
        let name = query
            .split('&')
            .find_map(|pair| pair.strip_prefix("dn="))
            .map(|name| percent_decode(&name.replace('+', " ")))
            .filter(|name| !name.trim().is_empty());
        return name.unwrap_or_else(|| "Magnet link".to_owned());
    }
    input.to_owned()
}

/// A queued command's detail line under its server.
pub(super) fn queued_detail(command: &QueuedCommand, server_name: &str) -> String {
    let what = if command.link {
        "Link download".to_owned()
    } else if command.files.is_empty() {
        "Download".to_owned()
    } else if command.files.len() == 1 {
        "Download · 1 file".to_owned()
    } else {
        format!("Download · {} files", command.files.len())
    };
    format!("{what} · Waiting for {server_name} to answer")
}

/// The queued commands for one server, in the order they were made.
pub(super) fn queued_for<'a>(queued: &'a [QueuedCommand], server: &str) -> Vec<&'a QueuedCommand> {
    queued
        .iter()
        .filter(|command| command.server == server)
        .collect()
}

/// Removes the first queued command equal to `command`. True when found.
pub(super) fn cancel_queued(queued: &mut Vec<QueuedCommand>, command: &QueuedCommand) -> bool {
    match queued.iter().position(|entry| entry == command) {
        Some(index) => {
            queued.remove(index);
            true
        }
        None => false,
    }
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && let Some(byte) = text
                .get(index + 1..index + 3)
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
        {
            out.push(byte);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A remote card's open context menu.
pub(super) struct RemoteCardMenu {
    pub(super) server: String,
    pub(super) video: String,
    pub(super) position: gpui::Point<Pixels>,
}

/// A copy that failed, kept in Downloads with its message until dismissed.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct KeepFailure {
    pub(super) server: String,
    pub(super) video: String,
    pub(super) message: String,
}

impl Flick {
    /// Discovery, pairing-by-code, and wake results, drained on tick.
    pub(crate) fn handle_discovery_events(
        &mut self,
        events: Vec<DiscoveryEvent>,
        cx: &mut Context<Self>,
    ) {
        if events.is_empty() {
            return;
        }
        for event in events {
            match event {
                DiscoveryEvent::Found(found) => self.remote_ui.discovery.found = Some(found),
                DiscoveryEvent::Paired { host, port, result } => {
                    // An answer after the field was closed pairs nothing.
                    let waiting = self.remote_ui.discovery.code.as_ref().is_some_and(|entry| {
                        entry.sending && entry.host == host && entry.port == port
                    });
                    if !waiting {
                        continue;
                    }
                    // The pairing was checked to name `host:port`; a code
                    // never replaces the token of a server already paired.
                    let added = result.and_then(|pairing| {
                        self.remote.add_code_pairing(&mut self.store.data, &pairing)
                    });
                    match added {
                        Ok(id) => {
                            // Closing the fields drops the code from the UI.
                            self.remote_ui.discovery.code = None;
                            self.remote.fields.pairing = None;
                            self.remote.fields.pairing_error = None;
                            if self.view == View::Onboarding {
                                self.onboarding_paired(id);
                            }
                            self.persist();
                        }
                        Err(message) => {
                            if let Some(entry) = self.remote_ui.discovery.code.as_mut() {
                                entry.sending = false;
                                entry.error = Some(sentence(&message));
                            }
                        }
                    }
                }
                DiscoveryEvent::Woke { server_id, result } => {
                    self.remote_ui.discovery.woke.insert(server_id, result);
                }
            }
        }
        cx.notify();
    }

    /// Progress and completion of copies kept on this computer, drained on
    /// tick. Also drops kept copies whose file has left the library.
    pub(crate) fn handle_keep_events(&mut self, events: Vec<KeepEvent>, cx: &mut Context<Self>) {
        let data = &mut self.store.data;
        if !data.remote.kept.is_empty() {
            let mut kept = std::mem::take(&mut data.remote.kept);
            let pruned = prune_kept(&mut kept, |path| data.video(path).is_some());
            data.remote.kept = kept;
            if pruned {
                self.persist();
                cx.notify();
            }
        }
        if events.is_empty() {
            return;
        }
        for event in events {
            match event {
                KeepEvent::Progress(_) => {}
                KeepEvent::Done {
                    server_id,
                    video_id,
                    path,
                } => self.kept_copy_done(server_id, video_id, path, cx),
                KeepEvent::Failed {
                    server_id,
                    video_id,
                    message,
                } => {
                    let failures = &mut self.remote_ui.keep_failed;
                    failures.retain(|f| !(f.server == server_id && f.video == video_id));
                    failures.push(KeepFailure {
                        server: server_id,
                        video: video_id,
                        message: sentence(&message),
                    });
                }
                KeepEvent::Cancelled {
                    server_id,
                    video_id,
                } => self
                    .remote_ui
                    .keep_failed
                    .retain(|f| !(f.server == server_id && f.video == video_id)),
            }
        }
        cx.notify();
    }

    /// A finished copy joins the library like an imported file and is
    /// recorded as kept from its server.
    fn kept_copy_done(
        &mut self,
        server: String,
        video: String,
        path: PathBuf,
        cx: &mut Context<Self>,
    ) {
        let path = flick::library::canonical_video(&path).unwrap_or(path);
        self.remote_ui
            .keep_failed
            .retain(|f| !(f.server == server && f.video == video));
        self.library_files_restored([&path]);
        self.store.data.restore_videos([path.clone()]);
        let kept = &mut self.store.data.remote.kept;
        kept.retain(|copy| !(copy.server == server && copy.video == video));
        kept.push(KeptCopy {
            server,
            video,
            path,
        });
        self.library_changed(cx);
        self.persist();
    }

    /// Where this remote video stands with this computer.
    pub(super) fn keep_status(&self, server: &str, video: &str) -> KeepStatus {
        if let Some(copy) = kept_copy(&self.store.data.remote.kept, server, video) {
            return KeepStatus::Kept(copy.path.clone());
        }
        if self
            .keep
            .in_progress()
            .iter()
            .any(|p| p.server_id == server && p.video_id == video)
        {
            return KeepStatus::Copying;
        }
        KeepStatus::NotKept
    }

    /// "Keep on this Mac": copies the original file into the download folder.
    pub(super) fn keep_remote_video(&mut self, server: &str, video: &str, cx: &mut Context<Self>) {
        let Some(playback) = self.remote.playback(&self.store.data, server, video) else {
            self.remote_ui.notice = Some("That library no longer lists this video.".into());
            cx.notify();
            return;
        };
        let file_name = self
            .remote_server(server)
            .and_then(|view| {
                view.listing
                    .as_ref()
                    .and_then(|listing| listing.videos.iter().find(|v| v.id == video))
                    .map(|v| v.file_name.clone())
            })
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(|| playback.title.clone());
        self.remote_ui
            .keep_failed
            .retain(|f| !(f.server == server && f.video == video));
        self.keep
            .keep(keep_request(&playback, &file_name, &self.download_folder));
        cx.notify();
    }

    /// Opens a remote card's context menu at a window position.
    fn open_remote_card_menu(
        &mut self,
        server: String,
        video: String,
        position: gpui::Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.card_menu = None;
        self.remote_ui.menu = Some(RemoteCardMenu {
            server,
            video,
            position,
        });
        // Focus the first item so Escape reaches the menu.
        self.control_handle(&ElementId::from(("card-menu", 0usize)), cx)
            .focus(window);
        cx.notify();
    }

    /// The open remote card's menu items, each with its action.
    pub(super) fn remote_card_menu_items(
        &self,
    ) -> Option<(gpui::Point<Pixels>, Vec<super::library_ui::MenuItem>)> {
        let menu = self.remote_ui.menu.as_ref()?;
        let server = self.remote_server(&menu.server);
        let complete = server
            .as_ref()
            .and_then(|s| s.listing.as_ref())
            .and_then(|listing| listing.videos.iter().find(|v| v.id == menu.video))
            .is_some_and(|video| video.complete);
        let online = server.as_ref().is_some_and(|s| !unreachable(&s.status));
        let status = self.keep_status(&menu.server, &menu.video);
        let items = remote_menu_items(&status, online && complete)
            .into_iter()
            .map(|item| {
                let (server, video) = (menu.server.clone(), menu.video.clone());
                let (icon, label) = (item.icon(), SharedString::from(item.label()));
                let action: Rc<super::library_ui::MenuAction> = match item {
                    RemoteMenuItem::Play => {
                        Rc::new(move |this, w, cx| this.open_remote_video(&server, &video, w, cx))
                    }
                    RemoteMenuItem::Keep => {
                        Rc::new(move |this, _, cx| this.keep_remote_video(&server, &video, cx))
                    }
                    RemoteMenuItem::CancelCopy => {
                        Rc::new(move |this, _, _| this.keep.cancel(&server, &video))
                    }
                    RemoteMenuItem::ShowCopy(path) => {
                        Rc::new(move |_, _, cx| cx.reveal_path(&path))
                    }
                    RemoteMenuItem::RemoveCopy(path) => {
                        Rc::new(move |this, w, cx| this.request_delete_video(path.clone(), w, cx))
                    }
                };
                (icon, label, action)
            })
            .collect();
        Some((menu.position, items))
    }

    /// Wake for an offline server that told us its hardware addresses.
    pub(super) fn wake_button(
        &self,
        id: impl Into<ElementId>,
        server: &RemoteServer,
        status: &Status,
        cx: &Context<Self>,
    ) -> Option<Stateful<Div>> {
        let wake = wake_target(server, status)?.clone();
        let server_id = server.id.clone();
        Some(
            self.button(id, Some("sun"), "Wake", false, cx, move |this, _, cx| {
                this.remote_ui.discovery.woke.remove(&server_id);
                this.discovery
                    .wake(&server_id, wake.mac.clone(), wake.lan_ips.clone());
                cx.notify();
            }),
        )
    }

    /// The paired server's settings, for Wake.
    pub(super) fn saved_server(&self, id: &str) -> Option<&RemoteServer> {
        self.store
            .data
            .remote
            .servers
            .iter()
            .find(|server| server.id == id)
    }

    /// The line after Wake was pressed for this server, while it is offline.
    pub(super) fn wake_line(&self, server_id: &str, status: &Status) -> Option<String> {
        if status.is_online() {
            return None;
        }
        self.remote_ui
            .discovery
            .woke
            .get(server_id)
            .map(wake_message)
    }

    /// Starts a discovery pass for Add Server or onboarding.
    pub(super) fn start_discovery(&mut self, cx: &mut Context<Self>) {
        self.discovery.discover();
        cx.notify();
    }

    /// Opens the code field for a found Flick and focuses it.
    pub(super) fn open_code_field(
        &mut self,
        row: &FoundRow,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let field = cx.new(|cx| TextInput::with_placeholder(cx, "Six-digit code"));
        cx.subscribe(&field, |this, _, event, cx| {
            match event {
                InputEvent::Changed => {
                    if let Some(entry) = this.remote_ui.discovery.code.as_mut() {
                        entry.error = None;
                    }
                }
                InputEvent::Submit => this.submit_code(cx),
            }
            cx.notify();
        })
        .detach();
        field.focus_handle(cx).focus(window);
        self.remote_ui.discovery.code = Some(CodeEntry {
            host: row.address.clone(),
            port: row.port,
            name: row.name.clone(),
            field,
            sending: false,
            error: None,
        });
        cx.notify();
    }

    /// Sends the typed code to the found Flick.
    pub(super) fn submit_code(&mut self, cx: &mut Context<Self>) {
        let Some(entry) = self.remote_ui.discovery.code.as_mut() else {
            return;
        };
        if entry.sending {
            return;
        }
        let code = entry.field.read(cx).value().to_owned();
        if !code_ready(&code) {
            entry.error = Some("A pairing code is six digits.".into());
            cx.notify();
            return;
        }
        entry.sending = true;
        entry.error = None;
        let (host, port) = (entry.host.clone(), entry.port);
        self.discovery.pair_by_code(&host, port, &code);
        cx.notify();
    }

    /// "Find on my Tailscale network": a spinner while looking, each found
    /// Flick with Pair, the code field under the one being paired, and a
    /// note when nothing answered.
    pub(super) fn discovery_list(&self, cx: &Context<Self>) -> Div {
        let t = cx.theme().clone();
        let discovering = self.discovery.discovering();
        let found = self.remote_ui.discovery.found.as_deref();
        let rows = found_rows(found.unwrap_or_default(), &self.store.data.remote.servers);
        let mut list = v_flex().gap_1().child(
            h_flex()
                .gap_2()
                .items_center()
                .child(
                    div()
                        .flex_1()
                        .font_weight(FontWeight::MEDIUM)
                        .child("Find on my Tailscale network"),
                )
                .child(if discovering {
                    h_flex()
                        .gap_1p5()
                        .text_size(t.font(12.))
                        .text_color(t.muted)
                        .child(spinner(&t))
                        .child("Searching…")
                        .into_any_element()
                } else {
                    self.button(
                        "remote-discover",
                        Some("rotate-cw"),
                        if found.is_some() {
                            "Search Again"
                        } else {
                            "Search"
                        },
                        false,
                        cx,
                        |this, _, cx| this.start_discovery(cx),
                    )
                    .into_any_element()
                }),
        );
        if let Some(note) = discovery_note(discovering, found) {
            list = list.child(div().text_size(t.font(12.)).text_color(t.muted).child(note));
        }
        let entry = self.remote_ui.discovery.code.as_ref();
        for (index, row) in rows.into_iter().enumerate() {
            let pairing_this = entry.is_some_and(|e| e.host == row.address && e.port == row.port);
            let detail = if row.code_open && !row.paired {
                format!("{} · {} · showing a pairing code", row.host, row.address)
            } else {
                format!("{} · {}", row.host, row.address)
            };
            let trailing = if row.paired {
                div()
                    .text_size(t.font(12.))
                    .text_color(t.muted)
                    .child("Paired")
                    .into_any_element()
            } else if pairing_this {
                div().into_any_element()
            } else {
                let found = row.clone();
                self.button(
                    ("remote-found-pair", index),
                    Some("plus"),
                    "Pair",
                    false,
                    cx,
                    move |this, window, cx| this.open_code_field(&found, window, cx),
                )
                .into_any_element()
            };
            list = list.child(row_line(&t, &row.name, detail, trailing));
            if pairing_this && let Some(entry) = entry {
                list = list.child(self.code_entry(entry, cx));
            }
        }
        list
    }

    fn code_entry(&self, entry: &CodeEntry, cx: &Context<Self>) -> Div {
        let t = cx.theme().clone();
        v_flex()
            .gap_1()
            .pl_3()
            .child(
                h_flex()
                    .gap_2()
                    .child(div().w(px(140.)).child(self.text_field(&entry.field, cx)))
                    .child(self.button(
                        "remote-code-pair",
                        Some("plus"),
                        if entry.sending { "Pairing…" } else { "Pair" },
                        true,
                        cx,
                        |this, _, cx| this.submit_code(cx),
                    ))
                    .child(self.icon_button(
                        "remote-code-cancel",
                        "x",
                        "Cancel",
                        cx,
                        |this, _, cx| {
                            this.remote_ui.discovery.code = None;
                            cx.notify();
                        },
                    )),
            )
            .child(
                div()
                    .text_size(t.font(12.))
                    .text_color(t.muted)
                    .child(format!("Pairing with {}. {CODE_HINT}", entry.name)),
            )
            .when_some(entry.error.clone(), |d, error| {
                d.child(
                    div()
                        .text_size(t.font(12.))
                        .text_color(t.danger)
                        .child(error),
                )
            })
    }

    /// Downloads: copies to this computer in progress and those that failed.
    pub(super) fn kept_rows(&self, cx: &Context<Self>) -> Option<Div> {
        let progress = self.keep.in_progress();
        let failed = &self.remote_ui.keep_failed;
        if progress.is_empty() && failed.is_empty() {
            return None;
        }
        let t = cx.theme().clone();
        let mut rows = Vec::new();
        for (index, copy) in progress.iter().enumerate() {
            let name = self.machine_name(&copy.server_id);
            let (server, video) = (copy.server_id.clone(), copy.video_id.clone());
            let fraction = copy
                .total
                .filter(|total| *total > 0)
                .map(|total| (copy.downloaded as f32 / total as f32).clamp(0., 1.));
            rows.push(
                v_flex()
                    .child(row(
                        &t,
                        Some("download"),
                        self.remote_video_title(&copy.server_id, &copy.video_id),
                        Some(keep_detail(copy, &name).into()),
                        self.button(
                            ("keep-cancel", index),
                            None,
                            "Cancel",
                            false,
                            cx,
                            move |this, _, cx| {
                                this.keep.cancel(&server, &video);
                                cx.notify();
                            },
                        ),
                    ))
                    .when_some(fraction, |d, fraction| {
                        d.child(
                            div().px_3().pb_2().child(
                                div().h(px(4.)).rounded_full().bg(t.control).child(
                                    div()
                                        .h_full()
                                        .w(relative(fraction))
                                        .rounded_full()
                                        .bg(t.accent),
                                ),
                            ),
                        )
                    })
                    .into_any_element(),
            );
        }
        for (index, failure) in failed.iter().enumerate() {
            let name = self.machine_name(&failure.server);
            let (server, video) = (failure.server.clone(), failure.video.clone());
            let retry = (server.clone(), video.clone());
            rows.push(
                row(
                    &t,
                    Some("info"),
                    self.remote_video_title(&failure.server, &failure.video),
                    Some(format!("From {name} · {}", failure.message).into()),
                    h_flex()
                        .gap_2()
                        .child(self.button(
                            ("keep-retry", index),
                            Some("rotate-cw"),
                            "Retry",
                            false,
                            cx,
                            move |this, _, cx| this.keep_remote_video(&retry.0, &retry.1, cx),
                        ))
                        .child(self.icon_button(
                            ("keep-dismiss", index),
                            "x",
                            "Dismiss",
                            cx,
                            move |this, _, cx| {
                                this.remote_ui
                                    .keep_failed
                                    .retain(|f| !(f.server == server && f.video == video));
                                cx.notify();
                            },
                        )),
                )
                .into_any_element(),
            );
        }
        Some(
            v_flex()
                .mt_4()
                .child(section_title(&t, kept_section_title()))
                .child(super::chrome_ui::group(&t, rows)),
        )
    }

    /// A remote video's title from its server's listing.
    fn remote_video_title(&self, server: &str, video: &str) -> String {
        self.remote_server(server)
            .and_then(|view| {
                view.listing.as_ref().and_then(|listing| {
                    listing
                        .videos
                        .iter()
                        .find(|v| v.id == video)
                        .map(|v| display_title(v).to_owned())
                })
            })
            .unwrap_or_else(|| "A video".to_owned())
    }

    /// Downloads rows for commands waiting for this server to answer.
    fn queued_rows(&self, server_id: &str, name: &str, cx: &Context<Self>) -> Vec<AnyElement> {
        let t = cx.theme().clone();
        queued_for(&self.store.data.remote.queued, server_id)
            .into_iter()
            .map(|command| {
                let key = gpui::hash(&(
                    &command.server,
                    &command.input,
                    command.created,
                    command.link,
                ));
                let cancel = command.clone();
                row(
                    &t,
                    Some(if command.link { "download" } else { "magnet" }),
                    queued_title(command),
                    Some(queued_detail(command, name).into()),
                    self.button(
                        SharedString::from(format!("queued-cancel-{key}")),
                        None,
                        "Cancel",
                        false,
                        cx,
                        move |this, _, cx| {
                            if cancel_queued(&mut this.store.data.remote.queued, &cancel) {
                                this.persist();
                            }
                            cx.notify();
                        },
                    ),
                )
                .into_any_element()
            })
            .collect()
    }
}

/// A turning arrow while discovery runs; still with reduced motion.
fn spinner(t: &Theme) -> AnyElement {
    use gpui::{Animation, AnimationExt, Transformation, percentage};
    let arrow = icon("rotate-cw", 14., t.muted);
    if t.reduce_motion {
        return arrow.into_any_element();
    }
    arrow
        .with_animation(
            "discovery-spinner",
            Animation::new(Duration::from_secs(1)).repeat(),
            |arrow, delta| arrow.with_transformation(Transformation::rotate(percentage(delta))),
        )
        .into_any_element()
}

/// A small label in a poster's top corner; place it with `.left` or `.right`.
pub(super) fn corner_badge(t: &Theme, text: impl Into<SharedString>) -> Div {
    div()
        .absolute()
        .top(px(6.))
        .px_1p5()
        .rounded(px(4.))
        .bg(rgba(0x000000b3))
        .text_color(rgb(0xffffff))
        .text_size(t.font(10.5))
        .font_weight(FontWeight::SEMIBOLD)
        .child(text.into())
}

/// A list line with a title, a muted detail, and a trailing control.
fn row_line(t: &Theme, title: &str, detail: String, trailing: AnyElement) -> Div {
    h_flex()
        .gap_3()
        .py_1()
        .child(
            v_flex()
                .flex_1()
                .min_w(px(0.))
                .child(
                    div()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .child(title.to_owned()),
                )
                .child(
                    div()
                        .text_size(t.font(12.))
                        .text_color(t.muted)
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .child(detail),
                ),
        )
        .child(trailing)
}

impl Flick {
    /// The paired server with this id, with its status and listing.
    pub(super) fn remote_server(&self, id: &str) -> Option<ServerView> {
        self.remote
            .servers(&self.store.data)
            .into_iter()
            .find(|server| server.id == id)
    }

    /// Opens a remote video from a card. A server known to be offline gets
    /// a refresh and a note on Home instead of a playback error.
    pub(super) fn open_remote_video(
        &mut self,
        server_id: &str,
        video_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(server) = self.remote_server(server_id) else {
            self.remote_ui.notice = Some("That library is no longer paired.".into());
            cx.notify();
            return;
        };
        if unreachable(&server.status) {
            self.remote.refresh_server(&self.store.data, server_id);
            self.remote_ui.notice = Some(format!("{}: {}", server.name, server.status.label()));
            cx.notify();
            return;
        }
        let Some(playback) = self.remote.playback(&self.store.data, server_id, video_id) else {
            self.remote.refresh_server(&self.store.data, server_id);
            self.remote_ui.notice = Some(format!("{} no longer lists this video.", server.name));
            cx.notify();
            return;
        };
        self.remote_ui.notice = None;
        let (url, authorization) = (playback.url(), playback.authorization());
        let source = RemoteSource {
            server_id: playback.server_id,
            server_name: playback.server_name,
            video_id: playback.video_id,
            title: playback.title,
            duration: playback.duration,
            quality: playback.quality,
            base_url: playback.base_url,
            token: playback.token,
        };
        // Both sides build the URL and header from the shared protocol.
        debug_assert_eq!(source.url(), url);
        debug_assert_eq!(source.header(), format!("Authorization: {authorization}"));
        self.open_remote(source, window, cx);
    }

    /// Home's note about a remote video that did not open.
    pub(super) fn remote_notice(&self, cx: &Context<Self>) -> Option<Div> {
        let text = self.remote_ui.notice.clone()?;
        let t = cx.theme();
        Some(notice(t, false, text).mb_4().child(self.icon_button(
            "remote-notice-dismiss",
            "x",
            "Dismiss",
            cx,
            |this, _, cx| {
                this.remote_ui.notice = None;
                cx.notify();
            },
        )))
    }

    /// Continue Watching cards for remote videos, newest first.
    pub(super) fn remote_continue_cards(
        &self,
        query: &str,
        limit: usize,
        (cols, width): (usize, f32),
        cx: &Context<Self>,
    ) -> Vec<AnyElement> {
        let servers = self.remote.servers(&self.store.data);
        let entries = self
            .store
            .data
            .remote
            .progress
            .iter()
            .filter(|entry| !entry.finished)
            .filter(|entry| {
                self.store
                    .data
                    .remote_resume_position(&entry.server, &entry.video)
                    > 0.
            })
            .filter(|entry| query.is_empty() || entry.title.to_lowercase().contains(query))
            .filter_map(|entry| {
                let server = servers.iter().find(|s| s.id == entry.server)?;
                Some((entry, server))
            })
            .take(limit)
            .collect::<Vec<_>>();
        let grid = "continue-remote".to_owned();
        let ids = entries
            .iter()
            .map(|(entry, _)| card_id("continue", &entry.server, &entry.video))
            .collect::<Rc<[_]>>();
        self.remote_ui.grids.borrow_mut().insert(grid.clone(), ids);
        let count = entries.len();
        entries
            .into_iter()
            .enumerate()
            .map(|(index, (entry, server))| {
                let resume = self
                    .store
                    .data
                    .remote_resume_position(&entry.server, &entry.video);
                let video = server
                    .listing
                    .as_ref()
                    .and_then(|listing| listing.videos.iter().find(|v| v.id == entry.video));
                let duration = video
                    .map(|v| v.duration)
                    .filter(|d| *d > 0.)
                    .unwrap_or(entry.duration);
                let state = if unreachable(&server.status) {
                    server.status.label()
                } else {
                    format!("{} left", format_time(duration - resume))
                };
                self.remote_card(
                    Card {
                        kind: "continue",
                        server: &entry.server,
                        video: &entry.video,
                        title: &entry.title,
                        detail: format!("{} · {state}", server.name),
                        progress: (duration > 0.).then(|| (resume / duration).clamp(0., 1.) as f32),
                        badge: video
                            .and_then(|v| crate::store::resolution_label(v.width, v.height)),
                    },
                    (&grid, index),
                    (cols, count),
                    width,
                    cx,
                )
            })
            .collect()
    }

    /// One Home section per paired server: its name and status, then its
    /// videos grouped into plain videos and shows. A server with nothing to
    /// show is one offline row with Retry.
    pub(super) fn remote_sections(&self, width: f32, query: &str, cx: &Context<Self>) -> Vec<Div> {
        let t = cx.theme().clone();
        let gap = 16.;
        let (cols, card) = columns(width, 180., gap);
        let mut sections = Vec::new();
        for server in self.remote.servers(&self.store.data) {
            let groups = server
                .listing
                .as_deref()
                .map(|listing| group_videos(listing, query))
                .unwrap_or_default();
            if groups.is_empty() && !query.is_empty() {
                continue;
            }
            let expanded = self.remote_ui.expanded.contains(&server.id);
            let truncated = groups
                .iter()
                .any(|group| group.videos.len() > cols * COLLAPSED_ROWS);
            let mut header = h_flex()
                .gap_3()
                .items_center()
                .child(section_title(&t, server.name.clone()))
                .child(
                    div()
                        .mb_2()
                        .text_size(t.font(12.))
                        .text_color(t.muted)
                        .child(server.summary()),
                )
                .child(div().flex_1());
            let wake = self.saved_server(&server.id).and_then(|saved| {
                self.wake_button(
                    SharedString::from(format!("remote-wake:{}", server.id)),
                    saved,
                    &server.status,
                    cx,
                )
            });
            let woke = self.wake_line(&server.id, &server.status);
            let id = server.id.clone();
            if unreachable(&server.status) && server.listing.is_some() {
                if let Some(wake) = wake {
                    header = header.child(wake.mb_2());
                }
                header = header.child(
                    self.button(
                        SharedString::from(format!("remote-retry:{id}")),
                        Some("rotate-cw"),
                        "Retry",
                        false,
                        cx,
                        move |this, _, cx| {
                            this.remote.refresh_server(&this.store.data, &id);
                            cx.notify();
                        },
                    )
                    .mb_2(),
                );
            }
            if truncated {
                let id = server.id.clone();
                header = header.child(
                    self.focusable(
                        div()
                            .id(SharedString::from(format!("remote-all:{id}")))
                            .relative()
                            .mb_2()
                            .text_size(t.font(12.))
                            .text_color(t.accent_text)
                            .cursor_pointer()
                            .child(if expanded { "Show Less" } else { "Show All" })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if !this.remote_ui.expanded.remove(&id) {
                                    this.remote_ui.expanded.insert(id.clone());
                                }
                                cx.notify();
                            })),
                        &SharedString::from(format!("remote-all:{}", server.id)).into(),
                        None,
                        t.radius,
                        (2., 0.),
                        cx,
                    ),
                );
            }
            let mut section = v_flex().w_full().mt(px(28.)).child(header);
            if let Some(woke) = &woke {
                section = section.child(
                    div()
                        .px_1()
                        .mb_2()
                        .text_size(t.font(12.))
                        .text_color(t.muted)
                        .child(woke.clone()),
                );
            }
            if server.listing.is_none() {
                let id = server.id.clone();
                let wake = self.saved_server(&server.id).and_then(|saved| {
                    self.wake_button(
                        SharedString::from(format!("remote-wake-offline:{}", server.id)),
                        saved,
                        &server.status,
                        cx,
                    )
                });
                section = section.child(
                    notice(&t, false, server.status.label())
                        .children(wake)
                        .child(self.button(
                            SharedString::from(format!("remote-retry:{id}")),
                            Some("rotate-cw"),
                            "Retry",
                            false,
                            cx,
                            move |this, _, cx| {
                                this.remote.refresh_server(&this.store.data, &id);
                                cx.notify();
                            },
                        )),
                );
                sections.push(section);
                continue;
            }
            if groups.is_empty() {
                section = section.child(
                    div()
                        .text_size(t.font(12.))
                        .text_color(t.muted)
                        .child("No videos in this library yet."),
                );
            }
            for group in groups {
                let shown = if expanded {
                    group.videos.len()
                } else {
                    group.videos.len().min(cols * COLLAPSED_ROWS)
                };
                let grid = format!("{}/{}", server.id, group.key);
                let videos = &group.videos[..shown];
                let ids = videos
                    .iter()
                    .map(|video| card_id("library", &server.id, &video.id))
                    .collect::<Rc<[_]>>();
                self.remote_ui.grids.borrow_mut().insert(grid.clone(), ids);
                let cards = videos
                    .iter()
                    .enumerate()
                    .map(|(index, video)| {
                        let progress = self.store.data.remote_progress(&server.id, &video.id);
                        let resume = self
                            .store
                            .data
                            .remote_resume_position(&server.id, &video.id);
                        let (progress, detail) = card_detail(video, resume, progress);
                        self.remote_card(
                            Card {
                                kind: "library",
                                server: &server.id,
                                video: &video.id,
                                title: display_title(video),
                                detail,
                                progress,
                                badge: crate::store::resolution_label(video.width, video.height),
                            },
                            (&grid, index),
                            (cols, shown),
                            card,
                            cx,
                        )
                    })
                    .collect();
                if let Some(title) = &group.title {
                    section = section.child(
                        div()
                            .px_1()
                            .mb_2()
                            .text_size(t.font(12.))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(t.muted)
                            .child(format!("{title} · {}", group.videos.len())),
                    );
                }
                section = section.child(card_grid(cards, cols, gap).mb_4());
            }
            sections.push(section);
        }
        sections
    }

    fn remote_card(
        &self,
        card: Card,
        (grid, index): (&str, usize),
        (cols, count): (usize, usize),
        width: f32,
        cx: &Context<Self>,
    ) -> AnyElement {
        let t = cx.theme();
        let height = (width * 9. / 16.).round();
        let id = card_id(card.kind, card.server, card.video);
        let group_name = SharedString::from(format!(
            "remote-{}",
            gpui::hash(&(card.kind, card.server, card.video))
        ));
        let (server, video) = (card.server.to_owned(), card.video.to_owned());
        let kept = match self.keep_status(card.server, card.video) {
            KeepStatus::Kept(_) => Some(kept_here_badge()),
            KeepStatus::Copying => Some("Copying…"),
            KeepStatus::NotKept => None,
        };
        let focused_card = self
            .keyboard_focused(&id, cx)
            .then(|| self.focused_card.clone());
        let poster = match self.remote.poster(card.server, card.video) {
            Some(image) => div()
                .flex_none()
                .w(px(width))
                .h(px(height))
                .rounded(px(t.card_radius))
                .overflow_hidden()
                .bg(rgb(0x000000))
                .child(img(image).size_full().object_fit(ObjectFit::Cover)),
            None => thumbnail(card.title, width, height, t.card_radius),
        };
        let el = v_flex()
            .id(id.clone())
            .group(group_name.clone())
            .relative()
            .w(px(width))
            .gap_1()
            .cursor_pointer()
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.open_remote_video(&server, &video, window, cx)
            }))
            .child(
                poster
                    .relative()
                    .border_1()
                    .border_color(t.border)
                    .when_some(kept.filter(|_| width >= 120.), |d, kept| {
                        d.child(corner_badge(t, kept).left(px(6.)))
                    })
                    .when_some(card.badge.filter(|_| width >= 120.), |d, badge| {
                        d.child(
                            div()
                                .absolute()
                                .top(px(6.))
                                .right(px(6.))
                                .px_1p5()
                                .rounded(px(4.))
                                .bg(rgba(0x000000b3))
                                .text_color(rgb(0xffffff))
                                .text_size(t.font(10.5))
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(badge),
                        )
                    })
                    .child(
                        div()
                            .absolute()
                            .inset_0()
                            .flex()
                            .items_center()
                            .justify_center()
                            .bg(rgba(0x00000059))
                            .opacity(0.)
                            .group_hover(group_name, |s| s.opacity(1.))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .size(px(44.))
                                    .rounded_full()
                                    .bg(rgba(0xffffff33))
                                    .child(icon("play", 22., rgb(0xffffff))),
                            ),
                    )
                    .when_some(card.progress, |d, progress| {
                        d.child(
                            div()
                                .absolute()
                                .left(px(8.))
                                .right(px(8.))
                                .bottom(px(8.))
                                .h(px(4.))
                                .rounded_full()
                                .bg(rgba(0xffffff4d))
                                .child(
                                    div()
                                        .h_full()
                                        .w(relative(progress))
                                        .rounded_full()
                                        .bg(t.accent),
                                ),
                        )
                    }),
            )
            .child(
                div()
                    .mt_1()
                    .font_weight(FontWeight::MEDIUM)
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .child(card.title.to_owned()),
            )
            .child(
                div()
                    .text_size(t.font(12.))
                    .text_color(t.muted)
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .child(card.detail),
            )
            .when_some(focused_card, |d, cell| {
                d.child(
                    canvas(move |bounds, _, _| cell.set(bounds), |_, _, _, _| {})
                        .absolute()
                        .size_full(),
                )
            });
        let el = self.with_remote_card_menu(el, card.server, card.video, cx);
        let el = self.with_remote_grid_keys(el, (grid.to_owned(), index), (cols, count), cx);
        self.focusable(el, &id, None, t.card_radius, (0., 0.), cx)
            .into_any_element()
    }

    /// Right-click, Shift+F10, or the Menu key open a remote card's menu.
    fn with_remote_card_menu(
        &self,
        el: Stateful<Div>,
        server: &str,
        video: &str,
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        let (server, video) = (server.to_owned(), video.to_owned());
        let (key_server, key_video) = (server.clone(), video.clone());
        el.on_mouse_down(
            MouseButton::Right,
            cx.listener(move |this, e: &gpui::MouseDownEvent, window, cx| {
                cx.stop_propagation();
                this.keyboard_nav = false;
                this.open_remote_card_menu(server.clone(), video.clone(), e.position, window, cx);
            }),
        )
        .on_key_down(cx.listener(move |this, e: &KeyDownEvent, window, cx| {
            let key = crate::shortcuts::normalize(&e.keystroke);
            if key == "shift-f10" || key == "menu" {
                cx.stop_propagation();
                let bounds = this.focused_card.get();
                let position = gpui::point(bounds.left() + px(12.), bounds.top() + px(24.));
                this.open_remote_card_menu(
                    key_server.clone(),
                    key_video.clone(),
                    position,
                    window,
                    cx,
                );
            }
        }))
    }

    /// Arrow keys move between cards of one remote grid, as in local grids.
    /// Every remote card is rendered, so Tab follows the normal order.
    fn with_remote_grid_keys(
        &self,
        el: Stateful<Div>,
        (grid, index): (String, usize),
        (cols, count): (usize, usize),
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        el.on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
            let modifiers = event.keystroke.modifiers;
            if modifiers.control || modifiers.platform || modifiers.alt || modifiers.shift {
                return;
            }
            let Some(step) = GridStep::from_key(&event.keystroke.key) else {
                return;
            };
            cx.stop_propagation();
            this.keyboard_nav = true;
            match step.target(index, cols, count) {
                Some(target) => {
                    let id = this
                        .remote_ui
                        .grids
                        .borrow()
                        .get(&grid)
                        .and_then(|ids| ids.get(target).cloned());
                    if let Some(id) = id {
                        this.control_handle(&id, cx).focus(window);
                    }
                }
                None if matches!(step, GridStep::Left | GridStep::Up) => window.focus_prev(),
                None => window.focus_next(),
            }
            cx.notify();
        }))
    }

    /// Subtitles the server has for the playing remote video: files next to
    /// it, and, in a transcode tier (which carries no subtitle streams),
    /// text streams from inside it. Original quality already has those.
    pub(super) fn remote_subtitle_choices(
        &self,
    ) -> Vec<(SubtitleRef, String, Option<SubtitleLoad>)> {
        let Some(remote) = self.remote_source() else {
            return Vec::new();
        };
        let Some(server) = self.remote_server(&remote.server_id) else {
            return Vec::new();
        };
        let Some(video) = server
            .listing
            .as_ref()
            .and_then(|listing| listing.videos.iter().find(|v| v.id == remote.video_id))
        else {
            return Vec::new();
        };
        let mut choices = Vec::new();
        for file in &video.subtitles {
            let label = match &file.language {
                Some(language) if !language.is_empty() => {
                    format!("{} ({language})", file.file_name)
                }
                _ => file.file_name.clone(),
            };
            choices.push((SubtitleRef::Sidecar(file.index), label));
        }
        if remote.quality.is_transcode() {
            for stream in video.embedded_subtitles.iter().filter(|s| s.text) {
                let label = stream
                    .title
                    .clone()
                    .filter(|t| !t.is_empty())
                    .or_else(|| stream.language.clone().filter(|l| !l.is_empty()))
                    .unwrap_or_else(|| format!("Track {}", stream.track));
                choices.push((
                    SubtitleRef::Embedded(stream.track),
                    format!("{label} (from the file)"),
                ));
            }
        }
        choices
            .into_iter()
            .map(|(which, label)| {
                let state = self.remote_ui.subtitles.get(&which).copied();
                (which, label, state)
            })
            .collect()
    }

    /// Downloads a server subtitle; it is added to the player when it arrives.
    pub(super) fn load_remote_subtitle(&mut self, which: SubtitleRef, cx: &mut Context<Self>) {
        let Some(remote) = self.remote_source() else {
            return;
        };
        if matches!(
            self.remote_ui.subtitles.get(&which),
            Some(SubtitleLoad::Loading | SubtitleLoad::Loaded)
        ) {
            return;
        }
        let (server, video) = (remote.server_id.clone(), remote.video_id.clone());
        self.remote_ui
            .subtitles
            .insert(which, SubtitleLoad::Loading);
        self.remote.fetch_subtitle(&server, &video, which);
        cx.notify();
    }

    /// The OpenSubtitles hash of the playing remote video, if the server
    /// already sent it. None asks the server and the search runs once the
    /// answer arrives in [`Self::apply_remote_ready`].
    pub(super) fn remote_search_hash(&mut self, generation: u64) -> Option<Option<String>> {
        let remote = self.remote_source()?;
        let key = (remote.server_id.clone(), remote.video_id.clone());
        if let Some(hash) = self.remote_ui.hashes.get(&key) {
            return Some(hash.clone());
        }
        self.remote.fetch_hash(&key.0, &key.1);
        self.remote_ui.awaiting_hash = Some((key.0, key.1, generation));
        None
    }

    /// Finished subtitle downloads and hash requests from the remote client.
    pub(super) fn apply_remote_ready(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for ready in self.remote.take_ready() {
            let current = self
                .remote_source()
                .map(|r| (r.server_id.clone(), r.video_id.clone()));
            match ready {
                Ready::Subtitle {
                    server_id,
                    video_id,
                    which,
                    result,
                } => {
                    if current.as_ref() != Some(&(server_id, video_id)) {
                        continue;
                    }
                    let added = result.and_then(|path| {
                        let path = path.to_str().ok_or("unsupported file name")?.to_owned();
                        let player = self.player.as_ref().ok_or("playback is unavailable")?;
                        player
                            .command(&["sub-add", &path, "select"])
                            .map_err(|error| error.to_string())
                    });
                    match added {
                        Ok(()) => {
                            self.remote_ui.subtitles.insert(which, SubtitleLoad::Loaded);
                            self.show_status("Subtitles added");
                        }
                        Err(error) => {
                            self.remote_ui.subtitles.insert(which, SubtitleLoad::Failed);
                            self.show_status(format!("Could not load subtitles: {error}"));
                        }
                    }
                    cx.notify();
                }
                Ready::Hash {
                    server_id,
                    video_id,
                    result,
                } => {
                    let hash = result
                        .inspect_err(|error| tracing::debug!(%error, "No remote video hash"))
                        .ok();
                    let key = (server_id, video_id);
                    self.remote_ui.hashes.insert(key.clone(), hash.clone());
                    let Some((server, video, generation)) = self.remote_ui.awaiting_hash.take()
                    else {
                        continue;
                    };
                    if (server, video) == key && generation == self.subtitle_search.generation {
                        self.send_subtitle_search(None, hash, cx);
                    }
                }
                Ready::Command {
                    server_id,
                    command,
                    result,
                } => self.remote_command_done(server_id, command, result, cx),
                Ready::Playback {
                    check,
                    server_id,
                    video_id,
                    quality,
                    result,
                } => {
                    let Some(pending) = self.remote_ui.checking.take_if(|p| {
                        p.check == check
                            && p.remote.server_id == server_id
                            && p.remote.video_id == video_id
                            && p.remote.quality == quality
                    }) else {
                        continue;
                    };
                    if self.quitting || pending.generation != self.open_generation {
                        continue;
                    }
                    match result {
                        Ok(()) => self.load_remote(pending.remote, pending.resume, window, cx),
                        Err(message) => {
                            // An automatic drop that was refused: the
                            // watcher stays on the tier still playing.
                            if let Some(playing) = self.remote_source().map(|r| r.quality)
                                && let Some(auto) = &mut self.auto_quality
                            {
                                auto.refused(playing, std::time::Instant::now());
                            }
                            self.status = None;
                            self.error = Some(format!(
                                "Could not play this video at {}. {}",
                                pending.remote.quality.label(),
                                sentence(&message)
                            ));
                            cx.notify();
                        }
                    }
                }
            }
        }
    }

    /// Sends this video's progress to its server, matching what was just
    /// saved locally.
    pub(super) fn upload_remote_progress(&mut self, server: &str, video: &str) {
        // The client's worker is already stopped; sending would restart it.
        if self.quitting {
            return;
        }
        let Some(entry) = self.store.data.remote_progress(server, video) else {
            return;
        };
        let progress = Progress {
            position: entry.position,
            duration: entry.duration,
            finished: entry.finished,
            updated: entry.updated,
            revision: 0,
        };
        self.remote
            .put_progress(&self.store.data, server, video, progress);
    }

    /// Turns the in-app server on, or restarts it with a new token.
    pub(super) fn start_serving(&mut self, new_token: bool) {
        // Queue the current library first; the server thread waits for it.
        self.persist();
        let Some(settings_path) = self.store.path().map(PathBuf::from) else {
            self.serve
                .fail("Flick has no settings file to share on this system".into());
            return;
        };
        let barrier = self.store.save_barrier();
        self.serve.start(crate::remote_serve::ServeOptions {
            settings_path,
            state_dir: crate::remote_client::default_data_dir().join("remote"),
            poster_cache: thumbnail_cache(),
            name: self.store.data.remote.serve_name.clone(),
            new_token,
            bind: None,
            ffmpeg: None,
            before_start: Some(Box::new(move || barrier.wait())),
        });
    }

    pub(super) fn set_serving(&mut self, on: bool, cx: &mut Context<Self>) {
        self.store.data.remote.serve = on;
        if on {
            self.start_serving(false);
        } else {
            self.serve.stop();
            self.persist();
        }
        cx.notify();
    }

    /// Settings rows for "Share my library on Tailscale".
    pub(super) fn serve_rows(&self, cx: &Context<Self>) -> (Vec<AnyElement>, Option<String>) {
        use crate::remote_serve::ServeStatus;
        let t = cx.theme().clone();
        let on = self.store.data.remote.serve;
        let status = self.serve.status().clone();
        let detail = match (&status, on) {
            (ServeStatus::Running { listen, .. }, _) => format!("Sharing on {listen}"),
            (ServeStatus::Starting, _) => "Starting…".to_owned(),
            (ServeStatus::Failed(_), _) => "Not sharing".to_owned(),
            (ServeStatus::Off, true) => "Starting…".to_owned(),
            (ServeStatus::Off, false) => {
                "Other Flicks on your tailnet can play this library at full quality".to_owned()
            }
        };
        let mut rows = vec![
            row(
                &t,
                Some("share-2"),
                "Share my library on Tailscale",
                Some(detail.into()),
                self.switch("remote-serve", on, cx, move |this, _, cx| {
                    this.set_serving(!on, cx)
                }),
            )
            .into_any_element(),
        ];
        let error = match &status {
            ServeStatus::Failed(message) => Some(message.clone()),
            _ => None,
        };
        if let ServeStatus::Running { pairing, listen } = status {
            let shown = match pairing.split_once("/#") {
                Some((address, token)) => {
                    format!("{address}/#{}…", &token[..token.len().min(8)])
                }
                None => pairing.clone(),
            };
            let copied = self
                .remote_ui
                .copied
                .is_some_and(|at| at.elapsed() < Duration::from_secs(2));
            let actions = h_flex()
                .gap_2()
                .child(self.button(
                    "remote-serve-copy",
                    Some("copy"),
                    if copied { "Copied" } else { "Copy" },
                    true,
                    cx,
                    move |this, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(pairing.clone()));
                        this.remote_ui.copied = Some(Instant::now());
                        cx.notify();
                    },
                ))
                .child(self.button(
                    "remote-serve-new-token",
                    Some("rotate-ccw"),
                    "New Token",
                    false,
                    cx,
                    |this, _, cx| {
                        this.start_serving(true);
                        cx.notify();
                    },
                ));
            // The pairing code row follows: `settings_ui::serve_extra_rows`.
            rows.push(
                row(
                    &t,
                    None,
                    "Pairing string",
                    Some(
                        format!(
                            "{shown} · Listening on {listen}. Paste it into Remote Libraries on the other Flick. New Token unpairs every Flick using this one."
                        )
                        .into(),
                    ),
                    actions,
                )
                .into_any_element(),
            );
        }
        (rows, error)
    }
}

/// Downloads on another machine: Save to, the sheets that start them, and
/// their sections in Downloads.
impl Flick {
    /// A paired server's name, or a placeholder after it was removed.
    fn machine_name(&self, server_id: &str) -> String {
        self.store
            .data
            .remote
            .servers
            .iter()
            .find(|server| server.id == server_id)
            .map_or_else(|| "The server".to_owned(), |server| server.name.clone())
    }

    /// Whether a Save to picker is worth showing for a sheet.
    pub(super) fn offers_save_to(&self, sheet: Sheet) -> bool {
        crate::remote_client::offers_remote(&self.remote.destinations(&self.store.data), sheet)
    }

    /// Opens Downloads, pausing playback like a local download does.
    fn show_downloads_for_remote(&mut self, cx: &mut Context<Self>) {
        if self.view == View::Player {
            self.persist();
            self.property("pause", "yes", cx);
        }
        self.view = View::Library;
        self.page = Page::Downloads;
    }

    /// Resolves a torrent on a machine: this Mac through the services
    /// worker, or a paired server, whose file list fills the sheet.
    pub(super) fn resolve_on(
        &mut self,
        input: flick::torrent::TorrentInput,
        machine: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if let Some(old) = self.remote_ui.save_to.remote_torrent.take()
            && old.resolving()
        {
            self.remote.cancel_resolve(&self.store.data, &old.server_id);
        }
        self.remote_ui.save_to.torrent_input = Some(copy_input(&input));
        self.remote_ui.save_to.torrent_machine = machine.clone();
        let Some(server_id) = machine else {
            self.resolve_locally(input, cx);
            return;
        };
        if self.resolving {
            self.send(ServiceCommand::CancelResolve, cx);
            self.resolving = false;
        }
        self.pending_torrent = None;
        self.error = None;
        let wire = match crate::remote_client::wire_input(&input) {
            Ok(wire) => wire,
            Err(error) => {
                self.error = Some(error);
                cx.notify();
                return;
            }
        };
        self.show_downloads_for_remote(cx);
        self.remote_ui.save_to.remote_torrent = Some(RemoteTorrent {
            server_id: server_id.clone(),
            input: wire.clone(),
            info: None,
            selected: HashSet::new(),
            starting: false,
            asleep: None,
        });
        self.remote.resolve(&self.store.data, &server_id, wire);
        cx.notify();
    }

    /// Save to changed in the torrent sheet: resolve again on that machine.
    fn choose_torrent_machine(&mut self, machine: Option<String>, cx: &mut Context<Self>) {
        self.remote
            .remember_destination(Sheet::Torrent, machine.clone());
        if machine == self.remote_ui.save_to.torrent_machine {
            return;
        }
        match self
            .remote_ui
            .save_to
            .torrent_input
            .as_ref()
            .map(copy_input)
        {
            Some(input) => self.resolve_on(input, machine, cx),
            None => self.remote_ui.save_to.torrent_machine = machine,
        }
        cx.notify();
    }

    /// Closes the torrent sheet for another machine, cancelling its resolve.
    fn cancel_remote_torrent(&mut self, cx: &mut Context<Self>) {
        if let Some(sheet) = self.remote_ui.save_to.remote_torrent.take()
            && sheet.resolving()
        {
            self.remote
                .cancel_resolve(&self.store.data, &sheet.server_id);
        }
        self.remote_ui.save_to.torrent_input = None;
        cx.notify();
    }

    /// Queues the torrent with every file for the machine that did not
    /// answer its resolve, and closes the sheet.
    fn queue_remote_torrent(&mut self, cx: &mut Context<Self>) {
        let save_to = &mut self.remote_ui.save_to;
        let Some(sheet) = save_to
            .remote_torrent
            .take_if(|sheet| sheet.asleep.is_some())
        else {
            return;
        };
        save_to.torrent_input = None;
        let message = self.remote.queue_download(&sheet.server_id, sheet.input);
        self.show_status_for(message, std::time::Duration::from_secs(6));
        cx.notify();
    }

    fn start_remote_torrent(&mut self, cx: &mut Context<Self>) {
        let Some(sheet) = self.remote_ui.save_to.remote_torrent.as_mut() else {
            return;
        };
        if sheet.starting || sheet.info.is_none() {
            return;
        }
        if sheet.selected.is_empty() {
            self.error = Some("Select at least one file to download.".into());
            cx.notify();
            return;
        }
        let mut files = sheet.selected.iter().copied().collect::<Vec<_>>();
        files.sort_unstable();
        sheet.starting = true;
        let (server_id, input) = (sheet.server_id.clone(), sheet.input.clone());
        self.remote
            .start(&self.store.data, &server_id, input, files);
        cx.notify();
    }

    /// Opens the paste-link sheet for a video page when some server takes
    /// links. Returns false when it downloads on this Mac as before.
    pub(super) fn open_link_sheet(
        &mut self,
        link: &flick::fetch::WebLink,
        cx: &mut Context<Self>,
    ) -> bool {
        let flick::fetch::WebLink::Page(url) = link else {
            return false;
        };
        if !self.offers_save_to(Sheet::Link) {
            return false;
        }
        let machine = self.remote.destination(&self.store.data, Sheet::Link);
        self.show_downloads_for_remote(cx);
        self.error = None;
        self.remote_ui.save_to.link = Some(LinkSheet {
            link: link.clone(),
            url: url.clone(),
            machine,
        });
        cx.notify();
        true
    }

    fn confirm_link(&mut self, cx: &mut Context<Self>) {
        let Some(sheet) = self.remote_ui.save_to.link.take() else {
            return;
        };
        match sheet.machine {
            None => self.download_link_here(sheet.link, cx),
            Some(server_id) => {
                self.remote
                    .fetch_start(&self.store.data, &server_id, &sheet.url);
                cx.notify();
            }
        }
    }

    /// Share: the Seed from sheet when a server shares folders, else the
    /// file picker as before.
    pub(super) fn open_share(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.offers_save_to(Sheet::Share) {
            self.pick_library(true, true, window, cx);
            return;
        }
        let machine = self.remote.destination(&self.store.data, Sheet::Share);
        self.show_downloads_for_remote(cx);
        self.remote_ui.save_to.share = Some(ShareSheet {
            machine: None,
            dir: String::new(),
            listing: None,
            error: None,
            sharing: None,
        });
        self.choose_share_machine(machine, cx);
    }

    fn choose_share_machine(&mut self, machine: Option<String>, cx: &mut Context<Self>) {
        self.remote
            .remember_destination(Sheet::Share, machine.clone());
        let Some(sheet) = self.remote_ui.save_to.share.as_mut() else {
            return;
        };
        sheet.machine = machine.clone();
        sheet.error = None;
        if let Some(server_id) = machine {
            self.browse_remote(&server_id, String::new(), cx);
        }
        cx.notify();
    }

    fn browse_remote(&mut self, server_id: &str, dir: String, cx: &mut Context<Self>) {
        let Some(sheet) = self.remote_ui.save_to.share.as_mut() else {
            return;
        };
        sheet.dir = dir.clone();
        sheet.listing = None;
        sheet.error = None;
        self.remote.browse(&self.store.data, server_id, &dir);
        cx.notify();
    }

    fn share_remote(&mut self, server_id: &str, dir: String, cx: &mut Context<Self>) {
        let Some(sheet) = self.remote_ui.save_to.share.as_mut() else {
            return;
        };
        if sheet.sharing.is_some() {
            return;
        }
        sheet.sharing = Some(dir.clone());
        sheet.error = None;
        self.remote.share(&self.store.data, server_id, &dir);
        cx.notify();
    }

    /// Plays a file of a download on another machine; at Original while it
    /// is unfinished.
    pub(super) fn watch_remote(
        &mut self,
        server_id: &str,
        video_id: &str,
        title: &str,
        unfinished: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(playback) =
            self.remote
                .transfer_playback(&self.store.data, server_id, video_id, title)
        else {
            self.error = Some("That library is no longer paired.".into());
            cx.notify();
            return;
        };
        let source = RemoteSource {
            server_id: playback.server_id,
            server_name: playback.server_name,
            video_id: playback.video_id,
            title: playback.title,
            duration: playback.duration,
            quality: if unfinished {
                Quality::Original
            } else {
                playback.quality
            },
            base_url: playback.base_url,
            token: playback.token,
        };
        self.open_remote(source, window, cx);
    }

    /// Pause All or Resume All on every machine with transfers.
    pub(super) fn pause_remote_machines(&mut self, resume: bool) {
        for machine in self.remote.machine_transfers(&self.store.data) {
            if machine.transfers.is_empty() {
                continue;
            }
            if resume {
                self.remote.resume_all(&self.store.data, &machine.server_id);
            } else {
                self.remote.pause_all(&self.store.data, &machine.server_id);
            }
        }
    }

    /// Applies a server's answer to a command.
    pub(super) fn remote_command_done(
        &mut self,
        server_id: String,
        command: Command,
        result: Result<Outcome, String>,
        cx: &mut Context<Self>,
    ) {
        let name = self.machine_name(&server_id);
        let save_to = &mut self.remote_ui.save_to;
        match (command, result) {
            (Command::Resolve { input }, result) => {
                let Some(sheet) = save_to.remote_torrent.as_mut().filter(|sheet| {
                    sheet.server_id == server_id && sheet.input == input && sheet.info.is_none()
                }) else {
                    return;
                };
                match result {
                    Ok(Outcome::Resolved(info)) => {
                        sheet.selected = default_selection(&info.files);
                        sheet.info = Some(info);
                    }
                    Ok(Outcome::Unanswered { question }) => sheet.asleep = Some(question),
                    Ok(_) => {}
                    Err(message) => {
                        save_to.remote_torrent = None;
                        self.error = Some(format!("{name}: {}", sentence(&message)));
                    }
                }
            }
            (Command::Start { input, .. }, result) => {
                let open = save_to
                    .remote_torrent
                    .as_ref()
                    .is_some_and(|sheet| sheet.server_id == server_id && sheet.input == input);
                let queued = match &result {
                    Ok(Outcome::Queued { message }) => Some(message.clone()),
                    _ => None,
                };
                match result {
                    Ok(_) if open => {
                        save_to.remote_torrent = None;
                        save_to.torrent_input = None;
                    }
                    Ok(_) => {}
                    Err(message) => {
                        if open && let Some(sheet) = save_to.remote_torrent.as_mut() {
                            sheet.starting = false;
                        }
                        self.error = Some(format!("{name}: {}", sentence(&message)));
                    }
                }
                // The machine is asleep: say so, and Downloads shows the
                // waiting row until it answers.
                if let Some(message) = queued {
                    self.show_status_for(message, std::time::Duration::from_secs(6));
                }
            }
            (
                Command::Remove {
                    id,
                    delete_files: true,
                    ..
                },
                result,
            ) => {
                let pending = save_to
                    .delete
                    .as_ref()
                    .is_some_and(|delete| delete.server_id == server_id && delete.id == id);
                // A share's 403, or a 409 without a list, is said once.
                let (again, refused) = delete_answered(server_id, id, result);
                if pending {
                    save_to.delete = again;
                }
                if let Some(message) = refused {
                    self.error = Some(format!("{name}: {}", sentence(&message)));
                }
            }
            (
                Command::Pause { id } | Command::Resume { id } | Command::Remove { id, .. },
                result,
            ) => {
                save_to.busy.remove(&(server_id, id));
                if let Err(message) = result {
                    self.error = Some(format!("{name}: {}", sentence(&message)));
                }
            }
            (Command::FetchStart { .. }, result) => match result {
                Ok(Outcome::Queued { message }) => {
                    self.show_status_for(message, std::time::Duration::from_secs(6));
                }
                Ok(_) => {}
                Err(message) => self.error = Some(format!("{name}: {}", sentence(&message))),
            },
            (Command::Browse { dir }, result) => {
                let Some(sheet) = save_to.share.as_mut().filter(|sheet| {
                    sheet.machine.as_deref() == Some(server_id.as_str()) && sheet.dir == dir
                }) else {
                    return;
                };
                match result {
                    Ok(Outcome::Browsed(listing)) => sheet.listing = Some(listing),
                    Ok(_) => {}
                    Err(message) => sheet.error = Some(sentence(&message)),
                }
            }
            (Command::Share { dir }, result) => {
                let sheet = save_to
                    .share
                    .as_mut()
                    .filter(|sheet| sheet.sharing.as_deref() == Some(dir.as_str()));
                match (result, sheet) {
                    (Ok(Outcome::Shared(shared)), _) => {
                        save_to.share = None;
                        save_to.shared_on = Some(name);
                        self.magnet_field
                            .update(cx, |field, cx| field.set_value(shared.magnet.clone(), cx));
                        self.shared_magnet = Some(shared.magnet);
                    }
                    (Ok(_), _) => {}
                    (Err(message), Some(sheet)) => {
                        sheet.sharing = None;
                        sheet.error = Some(sentence(&message));
                    }
                    (Err(message), None) => {
                        self.error = Some(format!("{name}: {}", sentence(&message)));
                    }
                }
            }
            // Cancelling a resolve that already ended is not news.
            (Command::CancelResolve, _) => {}
            (_, Err(message)) => self.error = Some(format!("{name}: {}", sentence(&message))),
            (_, Ok(_)) => {}
        }
        cx.notify();
    }

    /// Save to: one segment per machine with its free space. Machines that
    /// are offline or do not allow this are greyed and cannot be chosen.
    pub(super) fn save_to_picker(
        &self,
        sheet: Sheet,
        selected: Option<&str>,
        cx: &Context<Self>,
        on_select: impl Fn(&mut Self, Option<String>, &mut Window, &mut Context<Self>) + 'static,
    ) -> Div {
        let t = cx.theme().clone();
        let on_select = Rc::new(on_select);
        let name = match sheet {
            Sheet::Torrent => "save-to-torrent",
            Sheet::Link => "save-to-link",
            Sheet::Share => "save-to-share",
        };
        let mut segments = h_flex()
            .flex_wrap()
            .gap(px(2.))
            .p(px(2.))
            .rounded(px(7.))
            .bg(t.control);
        for (index, destination) in self
            .remote
            .destinations(&self.store.data)
            .into_iter()
            .enumerate()
        {
            let on = destination.id.as_deref() == selected;
            let enabled = destination.selectable(sheet);
            let detail = destination.detail(sheet);
            let id = ElementId::from((name, index));
            let mut item = div()
                .id(id.clone())
                .relative()
                .flex()
                .flex_col()
                .items_center()
                .min_w(px(96.))
                .px_3()
                .py_1()
                .rounded(px(5.))
                .child(
                    div()
                        .text_size(t.font(12.))
                        .whitespace_nowrap()
                        .when(on, |d| d.font_weight(FontWeight::SEMIBOLD))
                        .child(destination.name.clone()),
                )
                .child(
                    div()
                        .text_size(t.font(11.))
                        .text_color(t.muted)
                        .whitespace_nowrap()
                        .child(if detail.is_empty() {
                            " ".into()
                        } else {
                            detail
                        }),
                )
                .when(on, |d| d.bg(t.surface).shadow_xs());
            if enabled {
                let on_select = on_select.clone();
                let choice = destination.id.clone();
                item = item
                    .cursor_pointer()
                    .when(!on, |d| d.hover(|s| s.bg(t.control_hover)))
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener(|this, _, _, _| this.control_pressed = true),
                    )
                    .on_click(cx.listener(move |this, _: &gpui::ClickEvent, window, cx| {
                        cx.stop_propagation();
                        on_select(this, choice.clone(), window, cx);
                    }));
                segments = segments.child(self.focusable(item, &id, None, 5., (0., 0.), cx));
            } else {
                segments = segments.child(item.opacity(0.45));
            }
        }
        v_flex()
            .gap_1p5()
            .px_3()
            .py_2()
            .child(div().child(if sheet == Sheet::Share {
                "Seed from"
            } else {
                "Save to"
            }))
            .child(segments)
    }

    /// The torrent sheet's Save to row, when another machine takes downloads.
    pub(super) fn torrent_save_to(&self, cx: &Context<Self>) -> Option<AnyElement> {
        if !self.offers_save_to(Sheet::Torrent) {
            return None;
        }
        Some(
            self.save_to_picker(
                Sheet::Torrent,
                self.remote_ui.save_to.torrent_machine.as_deref(),
                cx,
                |this, machine, _, cx| this.choose_torrent_machine(machine, cx),
            )
            .into_any_element(),
        )
    }

    /// The torrent sheet for a file list from another machine.
    pub(super) fn remote_torrent_sheet(&self, cx: &Context<Self>) -> Option<Div> {
        let sheet = self.remote_ui.save_to.remote_torrent.as_ref()?;
        let t = cx.theme().clone();
        let name = self.machine_name(&sheet.server_id);
        let mut rows = Vec::new();
        rows.extend(self.torrent_save_to(cx));
        if let Some(question) = &sheet.asleep {
            rows.push(row(&t, Some("magnet"), question.clone(), None, div()).into_any_element());
            return Some(
                v_flex()
                    .child(section_title(&t, "New Download"))
                    .child(super::chrome_ui::group(&t, rows))
                    .child(
                        h_flex()
                            .mt_3()
                            .gap_2()
                            .justify_end()
                            .child(self.button(
                                "remote-queue-cancel",
                                None,
                                "Cancel",
                                false,
                                cx,
                                |this, _, cx| this.cancel_remote_torrent(cx),
                            ))
                            .child(self.button(
                                "remote-queue-confirm",
                                Some("download"),
                                "Queue",
                                true,
                                cx,
                                |this, _, cx| this.queue_remote_torrent(cx),
                            )),
                    ),
            );
        }
        let Some(info) = &sheet.info else {
            rows.push(
                row(
                    &t,
                    Some("magnet"),
                    format!("Finding peers on {name}…"),
                    Some("The file list appears when the server has the torrent's details".into()),
                    self.button(
                        "remote-resolve-cancel",
                        None,
                        "Cancel",
                        false,
                        cx,
                        |this, _, cx| this.cancel_remote_torrent(cx),
                    ),
                )
                .into_any_element(),
            );
            return Some(
                v_flex()
                    .child(section_title(&t, "New Download"))
                    .child(super::chrome_ui::group(&t, rows)),
            );
        };
        let total = info.files.iter().map(|file| file.size).sum::<u64>();
        rows.push(
            row(
                &t,
                Some("magnet"),
                info.name.clone(),
                Some(
                    format!(
                        "{} files · {} · downloads on {name}",
                        info.files.len(),
                        format_bytes(total)
                    )
                    .into(),
                ),
                div(),
            )
            .into_any_element(),
        );
        for file in &info.files {
            let index = file.index;
            let id = ElementId::from(("remote-torrent-file", index));
            let el = h_flex()
                .id(id.clone())
                .relative()
                .gap_3()
                .px_3()
                .py_2()
                .cursor_pointer()
                .hover(|s| s.bg(t.selection))
                .child(super::library_ui::checkbox(
                    &t,
                    sheet.selected.contains(&index),
                ))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .child(file.path.clone()),
                )
                .child(div().text_color(t.muted).child(format_bytes(file.size)))
                .on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(sheet) = this.remote_ui.save_to.remote_torrent.as_mut()
                        && !sheet.selected.remove(&index)
                    {
                        sheet.selected.insert(index);
                    }
                    cx.notify();
                }));
            rows.push(
                self.focusable(
                    el,
                    &id,
                    None,
                    t.radius,
                    (
                        -super::library_ui::RING_INSET,
                        -super::library_ui::RING_INSET,
                    ),
                    cx,
                )
                .into_any_element(),
            );
        }
        Some(
            v_flex()
                .child(section_title(&t, "New Download"))
                .child(super::chrome_ui::group(&t, rows))
                .child(
                    h_flex()
                        .mt_3()
                        .gap_2()
                        .justify_end()
                        .child(self.button(
                            "remote-download-cancel",
                            None,
                            "Cancel",
                            false,
                            cx,
                            |this, _, cx| this.cancel_remote_torrent(cx),
                        ))
                        .child(self.button(
                            "remote-download-confirm",
                            Some("download"),
                            if sheet.starting {
                                "Starting…"
                            } else {
                                "Download Selected"
                            },
                            true,
                            cx,
                            |this, _, cx| this.start_remote_torrent(cx),
                        )),
                ),
        )
    }

    /// The paste-link sheet: the link and where it downloads.
    pub(super) fn link_sheet(&self, cx: &Context<Self>) -> Option<Div> {
        let sheet = self.remote_ui.save_to.link.as_ref()?;
        let t = cx.theme().clone();
        let rows = vec![
            row(
                &t,
                Some("download"),
                format!("Download video from {}", sheet.link.site()),
                Some(sheet.url.clone().into()),
                div(),
            )
            .into_any_element(),
            self.save_to_picker(
                Sheet::Link,
                sheet.machine.as_deref(),
                cx,
                |this, machine, _, cx| {
                    this.remote
                        .remember_destination(Sheet::Link, machine.clone());
                    if let Some(sheet) = this.remote_ui.save_to.link.as_mut() {
                        sheet.machine = machine;
                    }
                    cx.notify();
                },
            )
            .into_any_element(),
        ];
        Some(
            v_flex()
                .child(section_title(&t, "New Link Download"))
                .child(super::chrome_ui::group(&t, rows))
                .child(
                    h_flex()
                        .mt_3()
                        .mb_6()
                        .gap_2()
                        .justify_end()
                        .child(self.button(
                            "link-sheet-cancel",
                            None,
                            "Cancel",
                            false,
                            cx,
                            |this, _, cx| {
                                this.remote_ui.save_to.link = None;
                                cx.notify();
                            },
                        ))
                        .child(self.button(
                            "link-sheet-confirm",
                            Some("download"),
                            "Download",
                            true,
                            cx,
                            |this, _, cx| this.confirm_link(cx),
                        )),
                ),
        )
    }

    /// Share with Seed from: this Mac's file picker, or a folder browser
    /// over what the server's owner allowed.
    pub(super) fn share_sheet(&self, cx: &Context<Self>) -> Option<Div> {
        let sheet = self.remote_ui.save_to.share.as_ref()?;
        let t = cx.theme().clone();
        let mut rows = vec![
            self.save_to_picker(
                Sheet::Share,
                sheet.machine.as_deref(),
                cx,
                |this, machine, _, cx| this.choose_share_machine(machine, cx),
            )
            .into_any_element(),
        ];
        match &sheet.machine {
            None => rows.push(
                row(
                    &t,
                    Some("share-2"),
                    "Files or a folder on this Mac",
                    Some("Keep Flick open while others download".into()),
                    self.button(
                        "share-sheet-choose",
                        Some("folder-open"),
                        "Choose…",
                        false,
                        cx,
                        |this, window, cx| {
                            this.remote_ui.save_to.share = None;
                            this.pick_library(true, true, window, cx);
                        },
                    ),
                )
                .into_any_element(),
            ),
            Some(server_id) => {
                let name = self.machine_name(server_id);
                let dir = sheet.dir.clone();
                let up = (!dir.is_empty()).then(|| {
                    let server = server_id.clone();
                    let parent = browse_parent(&dir);
                    self.button(
                        "share-browse-up",
                        None,
                        "Up",
                        false,
                        cx,
                        move |this, _, cx| this.browse_remote(&server, parent.clone(), cx),
                    )
                });
                let here = (!dir.is_empty() && sheet.sharing.is_none()).then(|| {
                    let server = server_id.clone();
                    let dir = dir.clone();
                    self.button(
                        "share-browse-here",
                        Some("share-2"),
                        "Share This Folder",
                        true,
                        cx,
                        move |this, _, cx| this.share_remote(&server, dir.clone(), cx),
                    )
                });
                rows.push(
                    row(
                        &t,
                        Some("folder-open"),
                        if dir.is_empty() {
                            format!("Shared folders on {name}")
                        } else {
                            dir.clone()
                        },
                        Some(
                            match &sheet.sharing {
                                Some(path) => {
                                    format!("Creating piece hashes for {path} on {name}…")
                                }
                                None => format!(
                                    "Seeds from {name} and keeps going after this Mac closes"
                                ),
                            }
                            .into(),
                        ),
                        h_flex().gap_2().children(up).children(here),
                    )
                    .into_any_element(),
                );
                if let Some(error) = &sheet.error {
                    rows.push(
                        div()
                            .px_3()
                            .py_2()
                            .text_size(t.font(12.))
                            .text_color(t.danger)
                            .child(error.clone())
                            .into_any_element(),
                    );
                }
                match &sheet.listing {
                    None if sheet.error.is_none() => rows.push(
                        div()
                            .px_3()
                            .py_2()
                            .text_color(t.muted)
                            .child("Reading folders…")
                            .into_any_element(),
                    ),
                    None => {}
                    Some(listing) if listing.entries.is_empty() => rows.push(
                        div()
                            .px_3()
                            .py_2()
                            .text_color(t.muted)
                            .child(if dir.is_empty() {
                                format!("{name}'s owner has not allowed any folders yet.")
                            } else {
                                "This folder is empty.".to_owned()
                            })
                            .into_any_element(),
                    ),
                    Some(listing) => {
                        for (index, entry) in listing.entries.iter().enumerate() {
                            let path = browse_child(&dir, &entry.name);
                            let id = ElementId::from(("share-entry", index));
                            let server = server_id.clone();
                            let open = path.clone();
                            let mut line = h_flex()
                                .id(id.clone())
                                .relative()
                                .gap_3()
                                .px_3()
                                .py_2()
                                .child(icon(
                                    if entry.dir { "folder-open" } else { "film" },
                                    16.,
                                    t.muted,
                                ))
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w(px(0.))
                                        .overflow_hidden()
                                        .whitespace_nowrap()
                                        .child(entry.name.clone()),
                                );
                            if !entry.dir {
                                line = line.child(
                                    div().text_color(t.muted).child(format_bytes(entry.size)),
                                );
                            }
                            if entry.dir {
                                line = line.cursor_pointer().hover(|s| s.bg(t.selection)).on_click(
                                    cx.listener(move |this, _, _, cx| {
                                        this.browse_remote(&server, open.clone(), cx)
                                    }),
                                );
                            }
                            if sheet.sharing.is_none() {
                                let server = server_id.clone();
                                line = line.child(self.button(
                                    ElementId::from(("share-entry-share", index)),
                                    None,
                                    "Share",
                                    false,
                                    cx,
                                    move |this, _, cx| this.share_remote(&server, path.clone(), cx),
                                ));
                            }
                            rows.push(
                                self.focusable(
                                    line,
                                    &id,
                                    None,
                                    t.radius,
                                    (
                                        -super::library_ui::RING_INSET,
                                        -super::library_ui::RING_INSET,
                                    ),
                                    cx,
                                )
                                .into_any_element(),
                            );
                        }
                    }
                }
            }
        }
        Some(
            v_flex()
                .child(section_title(&t, "Share Files"))
                .child(super::chrome_ui::group(&t, rows))
                .child(h_flex().mt_3().mb_6().justify_end().child(self.button(
                    "share-sheet-cancel",
                    None,
                    "Close",
                    false,
                    cx,
                    |this, _, cx| {
                        this.remote_ui.save_to.share = None;
                        cx.notify();
                    },
                ))),
        )
    }

    /// One section per server in Downloads, after this Mac's.
    pub(super) fn remote_download_sections(
        &self,
        machines: &[MachineTransfers],
        cx: &Context<Self>,
    ) -> Vec<Div> {
        let t = cx.theme().clone();
        let statuses = self
            .remote
            .servers(&self.store.data)
            .into_iter()
            .map(|server| (server.id, server.status))
            .collect::<HashMap<_, _>>();
        // Commands waiting for a machine that has no section of its own.
        let mut waiting = Vec::new();
        for command in &self.store.data.remote.queued {
            if !machines.iter().any(|m| m.server_id == command.server)
                && !waiting.contains(&command.server)
                && self.saved_server(&command.server).is_some()
            {
                waiting.push(command.server.clone());
            }
        }
        let extra = waiting.into_iter().map(|server_id| MachineTransfers {
            name: self.machine_name(&server_id),
            server_id,
            loaded: true,
            ..Default::default()
        });
        let machines = machines.iter().cloned().chain(extra).collect::<Vec<_>>();
        machines
            .iter()
            .enumerate()
            .map(|(index, machine)| {
                let mut section = v_flex()
                    .mt_6()
                    .child(section_title(&t, machine.name.clone()));
                let status = statuses
                    .get(&machine.server_id)
                    .cloned()
                    .unwrap_or_default();
                let wake = self.saved_server(&machine.server_id).and_then(|server| {
                    self.wake_button(("downloads-wake", index), server, &status, cx)
                });
                let woke = self.wake_line(&machine.server_id, &status);
                if let Some(problem) = &machine.problem {
                    section = section.child(
                        h_flex()
                            .mb_2()
                            .gap_2()
                            .text_size(t.font(12.))
                            .text_color(t.muted)
                            .child(icon("info", 14., t.muted))
                            .child(div().flex_1().child(
                                if machine.transfers.is_empty() && machine.fetches.is_empty() {
                                    sentence(problem)
                                } else {
                                    format!(
                                        "{} Showing the last known transfers.",
                                        sentence(problem)
                                    )
                                },
                            ))
                            .children(wake),
                    );
                } else if let Some(wake) = wake {
                    section = section.child(
                        h_flex()
                            .mb_2()
                            .gap_2()
                            .text_size(t.font(12.))
                            .text_color(t.muted)
                            .child(icon("info", 14., t.muted))
                            .child(div().flex_1().child(status.label()))
                            .child(wake),
                    );
                }
                if let Some(woke) = woke {
                    section = section.child(
                        div()
                            .mb_2()
                            .text_size(t.font(12.))
                            .text_color(t.muted)
                            .child(woke),
                    );
                }
                let queued = self.queued_rows(&machine.server_id, &machine.name, cx);
                let has_queued = !queued.is_empty();
                if has_queued {
                    section =
                        section.child(div().mb_2().child(super::chrome_ui::group(&t, queued)));
                }
                if !machine.transfers.is_empty() {
                    let rows = machine
                        .transfers
                        .iter()
                        .map(|row| self.remote_transfer_row(machine, row, cx))
                        .collect();
                    section = section.child(super::chrome_ui::group(&t, rows));
                }
                if !machine.fetches.is_empty() {
                    let rows = machine
                        .fetches
                        .iter()
                        .map(|row| self.remote_fetch_row(machine, row, cx))
                        .collect();
                    section = section.child(div().mt_2().child(super::chrome_ui::group(&t, rows)));
                }
                if machine.transfers.is_empty()
                    && machine.fetches.is_empty()
                    && machine.problem.is_none()
                    && !has_queued
                {
                    section =
                        section.child(div().text_size(t.font(12.)).text_color(t.muted).child(
                            if machine.loaded {
                                format!("No transfers on {}.", machine.name)
                            } else {
                                "Asking for transfers…".to_owned()
                            },
                        ));
                }
                section
            })
            .collect()
    }

    fn remote_transfer_row(
        &self,
        machine: &MachineTransfers,
        transfer: &TransferRow,
        cx: &Context<Self>,
    ) -> AnyElement {
        let server = machine.server_id.clone();
        let id = transfer.id.clone();
        let key = format!("{}-{id}", gpui::hash(&server));
        let paused = matches!(transfer.phase, TransferPhase::Paused | TransferPhase::Error);
        let video = |file: &&flick::remote::protocol::TransferFileRow| {
            file.selected
                && file.video_id.is_some()
                && flick::library::is_video(Path::new(&file.path))
        };
        let playable = transfer.files.iter().filter(video).find(|file| {
            file.verified_bytes >= file.size
                && !matches!(
                    transfer.phase,
                    TransferPhase::Checking | TransferPhase::Error
                )
        });
        let streamable = transfer.files.iter().filter(video).find(|file| {
            playable.is_none()
                && machine.stream_incomplete
                && file.verified_bytes < file.size
                && transfer.phase == TransferPhase::Downloading
        });
        let mut actions = h_flex().flex_wrap().gap_2();
        for (file, label, unfinished) in
            [(playable, "Play", false), (streamable, "Watch Now", true)]
        {
            let Some(file) = file else { continue };
            let (server, video) = (server.clone(), file.video_id.clone().unwrap_or_default());
            let title = Path::new(&file.path)
                .file_name()
                .map_or_else(|| file.path.clone(), |n| n.to_string_lossy().into_owned());
            actions = actions.child(self.button(
                SharedString::from(format!("remote-play-{key}")),
                Some("play"),
                label,
                false,
                cx,
                move |this, window, cx| {
                    this.watch_remote(&server, &video, &title, unfinished, window, cx)
                },
            ));
        }
        let busy = self
            .remote_ui
            .save_to
            .busy
            .get(&(server.clone(), id.clone()))
            .copied();
        let removing = busy == Some(REMOVING);
        let (toggle_server, toggle_id) = (server.clone(), id.clone());
        let (remove_server, remove_id) = (server.clone(), id.clone());
        actions = actions
            .child(self.button(
                SharedString::from(format!("remote-pause-{key}")),
                Some(if paused { "play" } else { "pause" }),
                match busy {
                    Some(label) if !removing => label,
                    _ if paused => "Resume",
                    _ => "Pause",
                },
                false,
                cx,
                move |this, _, cx| {
                    let label = if paused { "Resuming…" } else { "Pausing…" };
                    let busy = &mut this.remote_ui.save_to.busy;
                    if !start_row_command(busy, &toggle_server, &toggle_id, label) {
                        return;
                    }
                    if paused {
                        this.remote
                            .resume(&this.store.data, &toggle_server, &toggle_id);
                    } else {
                        this.remote
                            .pause(&this.store.data, &toggle_server, &toggle_id);
                    }
                    cx.notify();
                },
            ))
            .child(self.button(
                SharedString::from(format!("remote-remove-{key}")),
                None,
                if removing {
                    REMOVING
                } else {
                    "Remove · Keep Files"
                },
                false,
                cx,
                move |this, _, cx| {
                    let busy = &mut this.remote_ui.save_to.busy;
                    if !start_row_command(busy, &remove_server, &remove_id, REMOVING) {
                        return;
                    }
                    this.remote.remove(
                        &this.store.data,
                        &remove_server,
                        &remove_id,
                        false,
                        Vec::new(),
                    );
                    cx.notify();
                },
            ));
        if offers_delete(transfer) {
            let (delete_server, delete_id, delete_list) = (
                server.clone(),
                id.clone(),
                crate::remote_client::confirm_list(transfer),
            );
            actions = actions.child(self.button(
                SharedString::from(format!("remote-delete-{key}")),
                None,
                "Remove and Delete Files…",
                false,
                cx,
                move |this, _, cx| {
                    this.remote_ui.save_to.delete = Some(RemoteDelete {
                        server_id: delete_server.clone(),
                        id: delete_id.clone(),
                        confirm: delete_list.clone(),
                        changed: None,
                        sending: false,
                    });
                    cx.notify();
                },
            ));
        }
        let confirmation = self
            .remote_ui
            .save_to
            .delete
            .as_ref()
            .filter(|delete| delete.server_id == server && delete.id == id)
            .map(|delete| {
                self.delete_confirmation(
                    &key,
                    delete.confirm.iter().cloned(),
                    delete.changed.clone(),
                    delete.sending,
                    cx,
                    |this, _, cx| {
                        this.remote_ui.save_to.delete = None;
                        cx.notify();
                    },
                    |this, _, cx| {
                        let Some(delete) = this.remote_ui.save_to.delete.as_mut() else {
                            return;
                        };
                        if delete.sending {
                            return;
                        }
                        delete.sending = true;
                        let (server, id, confirm) = (
                            delete.server_id.clone(),
                            delete.id.clone(),
                            delete.confirm.clone(),
                        );
                        this.remote
                            .remove(&this.store.data, &server, &id, true, confirm);
                        cx.notify();
                    },
                )
            });
        self.transfer_entry(
            &super::library_ui::RowFigures::remote(transfer),
            actions,
            confirmation,
            cx,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flick::remote::protocol::Episode;

    #[test]
    fn server_messages_end_as_sentences() {
        assert_eq!(
            sentence("Still downloading on the server"),
            "Still downloading on the server."
        );
        assert_eq!(sentence(" Try again. "), "Try again.");
        assert_eq!(sentence("Busy…"), "Busy…");
    }

    fn video(id: &str, title: &str, episode: Option<(&str, u32, u32)>) -> Video {
        Video {
            id: id.into(),
            title: title.into(),
            file_name: format!("{title}.mkv"),
            complete: true,
            episode: episode.map(|(show, season, number)| Episode {
                show: show.into(),
                season,
                number,
            }),
            ..Default::default()
        }
    }

    fn library(videos: Vec<Video>) -> Library {
        Library {
            videos,
            ..Default::default()
        }
    }

    #[test]
    fn plain_videos_come_first_then_shows_with_episodes_in_order() {
        let listing = library(vec![
            video("1", "Zeta", None),
            video("2", "Pilot", Some(("Night Shift", 1, 1))),
            video("3", "Finale", Some(("Night Shift", 2, 3))),
            video("4", "Alpha", None),
            video("5", "Two", Some(("Night Shift", 1, 2))),
            video("6", "Start", Some(("Bakers", 1, 1))),
        ]);
        let groups = group_videos(&listing, "");
        let summary: Vec<_> = groups
            .iter()
            .map(|g| {
                (
                    g.title.clone(),
                    g.videos.iter().map(|v| v.id.as_str()).collect::<Vec<_>>(),
                )
            })
            .collect();
        assert_eq!(
            summary,
            [
                (Some("Videos".into()), vec!["4", "1"]),
                (Some("Bakers".into()), vec!["6"]),
                (Some("Night Shift".into()), vec!["2", "5", "3"]),
            ]
        );
    }

    #[test]
    fn a_library_of_plain_videos_has_one_untitled_group() {
        let listing = library(vec![video("1", "B", None), video("2", "A", None)]);
        let groups = group_videos(&listing, "");
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].title, None);
        assert!(group_videos(&library(Vec::new()), "").is_empty());
    }

    #[test]
    fn search_matches_titles_file_names_and_show_names() {
        let listing = library(vec![
            video("1", "Arrival", None),
            video("2", "Pilot", Some(("Night Shift", 1, 1))),
            video("3", "Heat", None),
        ]);
        let ids = |query: &str| {
            group_videos(&listing, query)
                .into_iter()
                .flat_map(|g| g.videos)
                .map(|v| v.id.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(ids("arr"), ["1"]);
        assert_eq!(ids("night"), ["2"]);
        assert_eq!(ids("heat.mkv"), ["3"]);
        assert!(ids("nothing").is_empty());
    }

    #[test]
    fn card_details_follow_progress_and_completion() {
        let mut film = video("1", "Film", None);
        film.duration = 3600.;
        assert_eq!(
            card_detail(&film, 0., None),
            (None, "Not started · 1:00:00".into())
        );
        assert_eq!(
            card_detail(&film, 900., None),
            (Some(0.25), "45:00 left".into())
        );
        let watched = crate::store::remote::RemoteProgress {
            finished: true,
            ..Default::default()
        };
        assert_eq!(
            card_detail(&film, 0., Some(&watched)),
            (None, "Watched · 1:00:00".into())
        );
        let mut episode = video("2", "Pilot", Some(("Show", 1, 2)));
        assert_eq!(card_detail(&episode, 0., None).1, "S1 E2 · Not started");
        episode.complete = false;
        assert_eq!(
            card_detail(&episode, 0., None).1,
            "S1 E2 · Still downloading on the server"
        );
    }

    #[test]
    fn only_known_failures_block_opening() {
        assert!(!unreachable(&Status::Online));
        assert!(!unreachable(&Status::Connecting));
        assert!(unreachable(&Status::Offline));
        assert!(unreachable(&Status::WrongToken));
        assert!(unreachable(&Status::Failed("x".into())));
    }

    #[test]
    fn share_browser_paths_stay_virtual() {
        assert_eq!(browse_child("", "Media"), "Media");
        assert_eq!(browse_child("Media", "Films"), "Media/Films");
        assert_eq!(browse_child("Media/", "Films"), "Media/Films");
        assert_eq!(browse_parent("Media/Films/2024"), "Media/Films");
        assert_eq!(browse_parent("Media"), "");
        assert_eq!(browse_parent(""), "");
    }

    #[test]
    fn remote_torrent_sheet_selects_videos_first() {
        use flick::remote::protocol::TorrentFileInfo;
        let file = |index: usize, path: &str| TorrentFileInfo {
            index,
            path: path.into(),
            size: 1,
        };
        let files = vec![
            file(0, "a/E01.mkv"),
            file(1, "a/notes.txt"),
            file(2, "a/E02.mp4"),
        ];
        assert_eq!(default_selection(&files), HashSet::from([0, 2]));
        assert_eq!(
            default_selection(&[file(4, "readme.txt"), file(5, "cover.jpg")]),
            HashSet::from([4, 5])
        );
        let magnet = flick::torrent::TorrentInput::Magnet("magnet:?x".into());
        assert!(matches!(
            copy_input(&magnet),
            flick::torrent::TorrentInput::Magnet(text) if text == "magnet:?x"
        ));
    }

    #[test]
    fn shares_offer_keep_files_only() {
        let row = TransferRow::default();
        assert!(offers_delete(&row));
        assert!(!offers_delete(&TransferRow { share: true, ..row }));
    }

    #[test]
    fn row_commands_wait_for_their_answer() {
        let mut busy = HashMap::new();
        assert!(start_row_command(&mut busy, "mini", "a", REMOVING));
        assert!(!start_row_command(&mut busy, "mini", "a", "Pausing…"));
        assert_eq!(busy[&("mini".to_owned(), "a".to_owned())], REMOVING);
        // Other rows and other servers are independent.
        assert!(start_row_command(&mut busy, "mini", "b", "Pausing…"));
        assert!(start_row_command(&mut busy, "book", "a", "Pausing…"));
    }

    #[test]
    fn deletions_ask_again_only_for_a_changed_list() {
        let changed = Ok(Outcome::ListChanged {
            message: "The files changed since you looked".into(),
            current: vec!["Show/E01.mkv".into(), "Show/notes.txt".into()],
        });
        let (again, refused) = delete_answered("mini".into(), "a".into(), changed);
        let again = again.expect("asked again");
        assert_eq!(again.confirm, ["Show/E01.mkv", "Show/notes.txt"]);
        assert_eq!(
            again.changed.as_deref(),
            Some("The files changed since you looked.")
        );
        assert!(!again.sending && refused.is_none());
        let (again, refused) = delete_answered("mini".into(), "a".into(), Ok(Outcome::Done));
        assert!(again.is_none() && refused.is_none());
        // A share's 403 or a 409 without a list closes it and is said once.
        let shared = Err("This is a share; remove it with Keep Files".to_owned());
        let (again, refused) = delete_answered("mini".into(), "a".into(), shared);
        assert!(again.is_none());
        assert_eq!(
            refused.as_deref(),
            Some("This is a share; remove it with Keep Files")
        );
    }

    #[test]
    fn remote_rows_use_the_local_row_figures() {
        use super::super::library_ui::{RowFigures, RowState};
        use flick::remote::protocol::{TransferPhase, TransferRow};
        let row = TransferRow {
            id: "a".into(),
            name: "Show".into(),
            phase: TransferPhase::Checking,
            verified_bytes: 5,
            total_bytes: 10,
            checked_bytes: 3,
            checking_total_bytes: 9,
            uploaded_bytes: 2,
            peers: 4,
            download_bps: 7,
            upload_bps: 8,
            error: Some("x".into()),
            share: false,
            files: Vec::new(),
        };
        let figures = RowFigures::remote(&row);
        assert_eq!(figures.state, RowState::Checking);
        assert_eq!(
            (
                figures.checked,
                figures.checking_total,
                figures.peers,
                figures.upload_bps
            ),
            (3, 9, 4, 8)
        );
        for (phase, state) in [
            (TransferPhase::Downloading, RowState::Downloading),
            (TransferPhase::Paused, RowState::Paused),
            (TransferPhase::Seeding, RowState::Seeding),
            (TransferPhase::Error, RowState::Error),
        ] {
            let row = TransferRow {
                phase,
                ..row.clone()
            };
            assert_eq!(RowFigures::remote(&row).state, state);
        }
    }

    fn found(host: &str, ip: &str, name: &str, open: bool) -> FoundFlick {
        FoundFlick {
            peer: crate::remote_discovery::Peer {
                host: host.into(),
                ip: ip.parse().unwrap(),
                online: true,
                os: "macOS".into(),
            },
            hello: flick::remote::protocol::Hello {
                protocol: 1,
                server_name: name.into(),
                pairing_open: open,
            },
        }
    }

    fn paired(id: &str, host: &str, port: u16) -> RemoteServer {
        RemoteServer {
            id: id.into(),
            name: "Mac mini".into(),
            host: host.into(),
            port,
            ..Default::default()
        }
    }

    #[test]
    fn found_flicks_show_their_name_and_whether_they_are_paired() {
        let port = flick::remote::protocol::DEFAULT_PORT;
        let list = [
            found("mohtashams-mac-mini", "100.98.200.69", "Mac mini", true),
            found("studio", "100.64.0.2", "  ", false),
            found("laptop", "100.64.0.3", "Laptop", true),
        ];
        let servers = [
            paired("a", "mohtashams-mac-mini.tailnet.ts.net", port),
            paired("b", "100.64.0.2", port),
            // Another port is another server.
            paired("c", "100.64.0.3", 9000),
        ];
        let rows = found_rows(&list, &servers);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].name, "Mac mini");
        assert_eq!(rows[0].host, "mohtashams-mac-mini");
        assert_eq!(rows[0].address, "100.98.200.69");
        assert_eq!(rows[0].port, port);
        assert!(rows[0].paired, "paired by MagicDNS name");
        assert_eq!(
            rows[1].name, "studio",
            "an empty name falls back to the host"
        );
        assert!(rows[1].paired, "paired by address");
        assert!(!rows[2].paired);
        assert!(rows[2].code_open);
        assert!(found_rows(&list, &[]).iter().all(|row| !row.paired));
    }

    #[test]
    fn the_discovery_list_says_when_it_searches_or_finds_nothing() {
        assert!(discovery_note(true, None).unwrap().starts_with("Looking"));
        assert!(
            discovery_note(true, Some(&[]))
                .unwrap()
                .starts_with("Looking")
        );
        assert_eq!(discovery_note(false, None), None);
        let none = discovery_note(false, Some(&[])).unwrap();
        assert_eq!(none, NO_FLICKS);
        assert!(none.contains("Tailscale must be running") && none.contains("sharing"));
        let one = [found("studio", "100.64.0.2", "Studio", false)];
        assert_eq!(discovery_note(false, Some(&one)), None);
        assert!(CODE_HINT.contains("Settings → Remote Libraries"));
        assert!(CODE_HINT.contains("flick-serve pair-code on that machine"));
    }

    #[test]
    fn pairing_codes_are_six_digits() {
        assert!(code_ready("123456"));
        assert!(code_ready(" 123-456 "));
        assert!(code_ready("123 456"));
        assert!(!code_ready("12345"));
        assert!(!code_ready("1234567"));
        assert!(!code_ready("12a456"));
        assert!(!code_ready(""));
    }

    #[test]
    fn wake_is_offered_only_for_an_offline_server_with_hardware() {
        let target = WakeTarget {
            mac: vec!["aa:bb:cc:dd:ee:ff".into()],
            lan_ips: vec!["192.168.1.20".into()],
        };
        let mut server = paired("a", "100.98.200.69", 47117);
        assert_eq!(wake_target(&server, &Status::Offline), None);
        server.wake = Some(target.clone());
        assert_eq!(wake_target(&server, &Status::Offline), Some(&target));
        for status in [
            Status::Online,
            Status::Connecting,
            Status::WrongToken,
            Status::NotFlick,
            Status::Failed("x".into()),
        ] {
            assert_eq!(wake_target(&server, &status), None, "{status:?}");
        }
        server.wake = Some(WakeTarget::default());
        assert_eq!(wake_target(&server, &Status::Offline), None);
        assert_eq!(wake_message(&Ok(())), "Wake packet sent; give it a minute.");
        let failed = wake_message(&Err("There was nowhere to send the wake packet".into()));
        assert!(
            failed.starts_with("Could not wake it: There was nowhere to send the wake packet.")
        );
        assert!(failed.contains("same network"));
    }

    #[test]
    fn remote_card_menus_follow_the_kept_copy() {
        use RemoteMenuItem::*;
        let path = PathBuf::from("/Movies/From Mac mini/film.mkv");
        assert_eq!(remote_menu_items(&KeepStatus::NotKept, true), [Play, Keep]);
        assert_eq!(remote_menu_items(&KeepStatus::NotKept, false), [Play]);
        assert_eq!(
            remote_menu_items(&KeepStatus::Copying, true),
            [Play, CancelCopy]
        );
        assert_eq!(
            remote_menu_items(&KeepStatus::Kept(path.clone()), false),
            [Play, ShowCopy(path.clone()), RemoveCopy(path)]
        );
        assert_eq!(Keep.label(), keep_label());
        if cfg!(target_os = "macos") {
            assert_eq!(keep_label(), "Keep on this Mac");
            assert_eq!(kept_section_title(), "Copies to this Mac");
        }
        assert_eq!(RemoveCopy(PathBuf::new()).label(), "Remove Local Copy…");
    }

    #[test]
    fn kept_copies_mark_cards_and_leave_with_their_file() {
        let path = PathBuf::from("/Movies/From Mac mini/film.mkv");
        let mut kept = vec![
            KeptCopy {
                server: "a".into(),
                video: "v1".into(),
                path: path.clone(),
            },
            KeptCopy {
                server: "gone".into(),
                video: "v2".into(),
                path: PathBuf::from("/Movies/other.mkv"),
            },
        ];
        let servers = [paired("a", "100.98.200.69", 47117)];
        assert_eq!(kept_copy(&kept, "a", "v1").map(|c| &c.path), Some(&path));
        assert_eq!(kept_copy(&kept, "a", "v2"), None);
        assert_eq!(
            kept_from(&kept, &servers, &path).as_deref(),
            Some("Kept from Mac mini")
        );
        assert_eq!(
            kept_from(&kept, &servers, Path::new("/Movies/other.mkv")).as_deref(),
            Some("Kept from another Flick")
        );
        assert_eq!(
            kept_from(&kept, &servers, Path::new("/elsewhere.mkv")),
            None
        );
        assert!(!prune_kept(&mut kept, |_| true));
        assert!(prune_kept(&mut kept, |p| p == path));
        assert_eq!(kept.len(), 1);
    }

    #[test]
    fn keep_requests_fetch_the_original_with_the_bearer_header() {
        let playback = crate::remote_client::RemotePlayback {
            server_id: "a".into(),
            server_name: "Mac mini".into(),
            video_id: "abc123".into(),
            title: "Film".into(),
            duration: 60.,
            quality: Quality::Low720,
            base_url: "http://100.98.200.69:47117".into(),
            token: "secret".into(),
        };
        let request = keep_request(&playback, "film.mkv", Path::new("/Downloads"));
        assert_eq!(request.url, "http://100.98.200.69:47117/v1/media/abc123");
        assert_eq!(request.authorization, "Bearer secret");
        assert_eq!(request.file_name, "film.mkv");
        assert_eq!(request.download_dir, PathBuf::from("/Downloads"));
        assert_eq!(
            (request.server_id.as_str(), request.video_id.as_str()),
            ("a", "abc123")
        );
        assert_eq!(request.server_name, "Mac mini");
        assert!(!format!("{request:?}").contains("secret"));
    }

    #[test]
    fn copy_rows_show_bytes_and_speed() {
        let mut progress = KeepProgress {
            server_id: "a".into(),
            video_id: "v".into(),
            downloaded: 0,
            total: None,
            bytes_per_second: 0,
        };
        assert_eq!(
            keep_detail(&progress, "Mac mini"),
            "From Mac mini · waiting to start"
        );
        progress.downloaded = 512 * 1024 * 1024;
        assert_eq!(
            keep_detail(&progress, "Mac mini"),
            "From Mac mini · 512.0 MB"
        );
        progress.total = Some(1024 * 1024 * 1024);
        progress.bytes_per_second = 10 * 1024 * 1024;
        assert_eq!(
            keep_detail(&progress, "Mac mini"),
            "From Mac mini · 512.0 MB of 1.0 GB (50%) · 10.0 MB/s"
        );
    }

    #[test]
    fn queued_commands_wait_under_their_server() {
        let magnet = QueuedCommand {
            server: "a".into(),
            input: "magnet:?xt=urn:btih:abcdef&dn=Big%20Buck+Bunny&tr=x".into(),
            files: vec![0, 2],
            link: false,
            created: 1,
        };
        let bare = QueuedCommand {
            input: "magnet:?xt=urn:btih:abcdef".into(),
            files: Vec::new(),
            created: 2,
            ..magnet.clone()
        };
        let link = QueuedCommand {
            server: "b".into(),
            input: " https://example.com/watch?v=1 ".into(),
            files: Vec::new(),
            link: true,
            created: 3,
        };
        assert_eq!(queued_title(&magnet), "Big Buck Bunny");
        assert_eq!(queued_title(&bare), "Magnet link");
        assert_eq!(queued_title(&link), "https://example.com/watch?v=1");
        assert_eq!(
            queued_detail(&magnet, "Mac mini"),
            "Download · 2 files · Waiting for Mac mini to answer"
        );
        assert_eq!(
            queued_detail(&bare, "Mac mini"),
            "Download · Waiting for Mac mini to answer"
        );
        assert_eq!(
            queued_detail(&link, "Studio"),
            "Link download · Waiting for Studio to answer"
        );
        let mut queued = vec![magnet.clone(), link.clone(), bare.clone()];
        assert_eq!(queued_for(&queued, "a"), [&magnet, &bare]);
        assert_eq!(queued_for(&queued, "b"), [&link]);
        assert!(cancel_queued(&mut queued, &magnet));
        assert!(!cancel_queued(&mut queued, &magnet));
        assert_eq!(queued, [link, bare]);
    }
}
