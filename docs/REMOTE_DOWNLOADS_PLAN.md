# Download to another machine, and onboarding

**Status: proposed and built on 2026-11-10; delivered at `d8cc148` as ticked below. Loopback tests, the desktop typecheck and unit tests, and `curl` against a real Tailscale address pass; nothing has been used from a second machine yet, which is this plan's acceptance test. See [validation](VALIDATION.md#download-to-another-machine-and-onboarding-on-2026-11-10).** Builds on the [remote library](REMOTE_PLAN.md), which is merged and was played across machines the same day. That feature is read-only: a paired Flick can list and play what the server has. This plan lets a paired Flick also *start* things on the server: a torrent, a link download, or a share. It adds a first-launch onboarding that sets all of this up in four screens.

## Outcome

Sitting at the MacBook, the user pastes a magnet link or a video page URL, chooses **Save to: Mac mini**, and the download runs on the Mac mini with the Mac mini's disk and connection. Downloads shows it under a "Mac mini" heading with the same progress, rates, peers, and controls as a local transfer. The user presses Watch Now and the video streams from the Mac mini while it is still downloading, with the same verified-pieces guarantee local playback has. When it finishes, it is simply a video in the Mac mini's library, visible from every paired Flick. Sharing works the same way: choose a folder on the Mac mini and it seeds from there, where it stays online.

A first launch asks four questions and ends with everything paired: where the videos live, whether this Flick shares its library, where downloads go, and the existing seeding choice. Nothing is turned on by accident; the server's side of this (accepting downloads from other Flicks) is a switch that is off until the owner turns it on.

## User experience

**Save to.** Every place a download starts gets the same control, a segmented picker with one segment per machine that accepts downloads: *This Mac* and each paired server that allows it. It appears in the torrent file-selection sheet, the paste-link sheet, the Share sheet (as *Seed from*), and the command center (*Download to Mac mini…*). Each segment shows the machine's free space. The default comes from onboarding and is remembered; the picker remembers the last choice per sheet. A machine that is offline is shown greyed with "Offline" and cannot be chosen.

**Downloads.** One section per machine, *This Mac* first, then each server by name. Remote rows show verified progress, download and upload rates, peers, and the Checking files percentage the local rows show, refreshed every second while the view is open. Pause, Resume, Remove · Keep Files, and Remove and Delete Files… work as they do locally; deletion lists every file the server will delete and needs the same explicit confirmation. Watch Now opens the remote stream. A server that stops answering keeps its last known rows and shows one status line; nothing is removed from view because a machine is asleep.

**Player.** A remote video that is still downloading plays like any other; there is no extra indicator in the player. A seek into pieces the server does not have yet shows the existing Buffering state and waits; it does not fail.

**Share.** *Seed from Mac mini* opens a folder browser limited to the folders the Mac mini's owner allowed. The magnet link is the server's; the share keeps going after the MacBook closes.

**Onboarding.** Four screens on first launch, also reachable from Settings as *Set up Flick again*:

1. *Where are your videos?* On this Mac, or on another machine on my Tailscale network (paste its pairing string; the connection is tested before Next is enabled).
2. *Share this library?* The in-app server switch with its pairing string and Copy. Off by default.
3. *Where should downloads go?* The Save to picker, listing this Mac and every paired machine that accepts downloads; with the seeding choice that the welcome page asks today.
4. *Done.* A summary of what was set, with each line linking to its settings page.

**Server side.** Settings → Remote Libraries gets a second group, *Let paired Flicks download to this Mac*, off by default, with the downloads folder and the list of folders other Flicks may share from. A headless `flick-serve` gets the same as flags.

## Architecture

The server grows from read-only to a small, explicit command surface. Everything new sits behind the existing bearer token and, in addition, behind the owner's switch: with the switch off every write route answers 403 with a message that names the switch. The listing advertises what the server allows, so a client never offers a machine it cannot use.

### Wire protocol additions

Listing gains `capabilities` (any of `downloads`, `links`, `shares`, `stream-incomplete`), `storage` (`free_bytes`, `download_folder_name`), and in-progress torrent videos with `complete: false` plus `progress` (0 to 1).

| Request | Response |
| --- | --- |
| `POST /v1/transfers/resolve` `{input}` with a magnet, HTTPS `.torrent` URL, infohash, or base64 `.torrent` bytes | The file list, as the desktop's file-selection sheet shows it. Bounded by the same five-minute peer wait. As built there is no `DELETE`: the server cancels a lookup when the client closes the request. |
| `POST /v1/transfers` `{input, files: [index]}` | `{id}`; the server picks the folder, never the client. 409 when the transfer already exists. |
| `GET /v1/transfers` | Snapshots of every transfer the server owns, in the adapter's shape: state, selected files, verified progress, rates, peers, checking percentage. |
| `POST /v1/transfers/{id}/pause`, `/resume`, `/pause-all`, `/resume-all` | 204. |
| `DELETE /v1/transfers/{id}` `{delete_files, confirm: [paths]}` | With `delete_files`, the server deletes only the files whose paths the client echoed from the snapshot, exactly as the desktop's Delete Listed Files does; a mismatch is 409 with the current list. |
| `POST /v1/fetches` `{url}`, `GET /v1/fetches`, `DELETE /v1/fetches/{id}` | Link downloads with yt-dlp or HTTPS on the server; updates in the shape `fetch::FetchUpdate` already has. |
| `GET /v1/browse?dir=` | Folders and video files under an allowed share root; anything outside answers 404, never the real path. |
| `POST /v1/shares` `{dir}` | Seeds that folder; returns the magnet and infohash. As built the body has no `name`. |
| `GET /v1/media/{id}` for `complete: false` | Range requests served through `TransferSession::open_stream`, which returns only verified pieces and steers the picker toward the read position. A read that has to wait blocks up to 30 s, then answers 504 so the player can retry, instead of today's 409. |

Transfers are identified by infohash and fetches by the server's own id, so a client holding stale ids gets 404 and refreshes.

### Where it runs

- **In the desktop app**, the in-app server reuses the existing `Services` worker and its `TransferSession`; the new routes are thin adapters over the commands the Downloads view already sends. The owner's seeding preference and bandwidth limits apply to remote-started transfers exactly as to local ones.
- **Headless `flick-serve`** gets a `TransferSession` and the fetch backend when `--allow-downloads` is given, with `--download-dir`, repeatable `--share-root`, and the same public-discovery settings the desktop uses. Its transfer state lives in its state folder. Completed downloads join its listing: in `--folder` mode because the download folder is scanned, in `--library` mode through a `remote-downloads.json` the server keeps beside its state, since the desktop's `settings.json` stays read-only.
- **Client**: a `RemoteTransfers` part of `RemoteState` polls `GET /v1/transfers` and `GET /v1/fetches` once a second while Downloads is open and once a minute otherwise, publishes typed rows, and sends commands through the same worker. The Downloads view renders remote rows with the local row component.

### Streaming an unfinished download

The desktop already plays an unfinished torrent through a libmpv stream callback over `open_stream`. The server wraps the same stream in the media route: each range request maps to a seek and read on a `VerifiedStream`, which blocks until the pieces are verified. The hard cases are the same ones `docs/TRANSFERS.md` lists for local playback: container metadata at the end of the file, seeks into missing pieces, clients that disconnect while a read waits, and downloads slower than the bit rate. A client disconnect cancels the read through the stream's cancel token. Transcode tiers of an unfinished file are not offered; FFmpeg would stall on missing pieces.

### Onboarding

A new `onboarding` view owns the four steps and their state. As built it comes before the welcome page rather than replacing it: an empty library still shows the welcome page after setup. Step one reuses the pairing code from Remote Libraries; step two reuses the in-app server switch; step three writes the default destination and the seeding choice. A `setup_done` flag in settings, set only on the last step, decides whether first launch shows it; *Set up Flick again* clears the flag. Every step can be skipped, and skipping sets nothing.

## Security and consent

- Write routes are off until the owner turns on *Let paired Flicks download to this Mac*. The listing advertises `capabilities` only when they are on, and each route checks again.
- A client never sends a filesystem path to start a download; the server chooses the folder. The browser for shares shows only allowed roots, resolves symlinks before checking containment, and answers 404 for anything outside.
- Deleting files requires the client to echo the exact file list, as the desktop confirms locally.
- Resolve and fetch are rate-limited per server (a few at a time) and bounded in time, because each one opens network connections on the owner's behalf.
- The owner's own transfer preferences (seeding after completion, bandwidth limits, public discovery) govern remote-started transfers. A client cannot change them.
- Nothing here changes the read side: the token, the Tailscale-only bind, and the opaque ids stay as they are.

## Delivery order

### 1. Server command routes, in-app first

- [x] Listing: `capabilities`, `storage`, in-progress videos with `progress`.
- [x] Transfers: resolve, start, list, pause, resume, remove with echoed deletion; adapters over the existing `Services` commands and events.
- [x] The owner's switch and folder settings in `store::remote`; 403 everywhere when off.
- [x] Loopback tests with generated fixtures and explicit local peers, following `tests/transfers.rs`: start a download from a loopback seed through the route, watch its snapshot complete, pause and resume, remove with a wrong and then a right file list. No trackers, no DHT.

### 2. Client: Save to, and Downloads by machine

- [x] Save to picker in the torrent sheet, the link sheet, and the command center; defaults and memory. The last choice per sheet is remembered for the session, not across launches.
- [x] Remote rows in Downloads with the local row component, polling, actions, and the offline status line.
- [x] Completed remote downloads appear in that server's library section without a restart.

### 3. Watch while downloading over the wire

- [x] Media route over `VerifiedStream` with bounded waits and cancel on disconnect.
- [ ] Buffering on seeks into missing pieces, through the existing paused-for-cache state. It relies on the player's existing Buffering state; no player has played an unfinished remote download yet.
- [ ] Tests with a throttled loopback seed: play starts before completion, a seek past the download waits and resumes, a disconnected read releases the stream. `tests/remote_transfers.rs` reads verified ranges before completion, gets 504 for a range a stopped seed cannot supply, and releases the stream on disconnect; a wait that then resumes, and a real player, are not tested.

### 4. Onboarding and the headless server

- [x] The four-step view, `setup_done`, and *Set up Flick again*.
- [x] `flick-serve --allow-downloads --download-dir --share-root`, its transfer state, and `remote-downloads.json`; the launchd agent and `docs/REMOTE.md` updated. The launchd installer already passes options after `--`, so it was not changed; it has not been run with these options.

### 5. Shares and links from the server

- [x] `browse` and `shares` with allowed roots; *Seed from* in the Share sheet.
- [x] Link downloads through `fetches`; yt-dlp discovery on the server with a clear message when it is missing. `flick-serve --links` logs why links are off; the in-app server leaves `links` out of its capabilities, and Save to says Not accepting links.

## Not in scope

Moving an existing download between machines, a queue for machines that are asleep, transcoding an unfinished file, accounts, and exposure outside the tailnet.

## Validation

Each phase records in [VALIDATION.md](VALIDATION.md): loopback backend tests, the desktop typecheck and unit tests, and a session from a second machine over Tailscale that starts a download on the server, watches it in Downloads, and plays it before it completes. The last one is the acceptance test for this plan.
