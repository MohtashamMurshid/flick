# Serving a Flick library from a Mac mini

`flick-serve` is a headless server for a machine that keeps the storage, such as a Mac mini. Another Flick on the same [Tailscale](https://tailscale.com) network can browse its library and stream the original files from it. Files are not copied. The design is in [the remote library plan](REMOTE_PLAN.md).

**Status:** the server, this tool, the quality tiers, and the desktop side (pairing in **Settings → Remote Libraries**, a library section per server, playback, subtitles, and the quality picker) are implemented and covered by loopback tests; the server was checked on a real Tailscale address, including from the desktop app's own "Share my library on Tailscale" switch (see [validation](VALIDATION.md#remote-library-integration-and-quality-tiers-on-2026-11-10)), and played from a MacBook (see [validation](VALIDATION.md#remote-playback-from-the-macbook-pro-on-2026-11-10)). Accepting downloads from paired Flicks is implemented, covered by loopback tests, and checked with `curl` on a real Tailscale address; it has not been used from a second machine yet (see [validation](VALIDATION.md#download-to-another-machine-and-onboarding-on-2026-11-10)). You can also check the server with `curl` or stock `mpv`, as shown below.

## What it does, and what it never does

- It listens **only on this machine's Tailscale address**. It refuses `0.0.0.0`, LAN addresses, and public addresses; `127.0.0.1` is allowed for local checks. Nothing is reachable from the Internet or from your LAN.
- Every request needs the **pairing token** (`Authorization: Bearer …`). A tailnet can include machines shared in by other people, so a Tailscale address on its own is not enough. Tokens are compared in constant time.
- By default it serves files read-only: it never writes to your library or your settings, and writes only in its own state folder (see [Files it keeps](#files-it-keeps)). With `--allow-downloads` it also writes downloads into its download folder, deletes downloaded files when a paired Flick confirms the exact list, and keeps `remote-downloads.json` in its state folder; see [Accepting downloads from other Flicks](#accepting-downloads-from-other-flicks). It never writes to the desktop's `settings.json`.
- It speaks plain HTTP. Tailscale's WireGuard link already encrypts the traffic between the two machines.

## Build

You need Rust (the repository pins 1.99.0 in `rust-toolchain.toml`). No Xcode, GPUI, libmpv, or FFmpeg is needed:

```sh
cargo build --locked --release --no-default-features --bin flick-serve
./target/release/flick-serve --help
```

Copy `target/release/flick-serve` somewhere on your `PATH`, such as `/usr/local/bin`, if you like.

## Run it

Start Tailscale first and check the address:

```sh
tailscale ip -4          # for example 100.98.200.69
```

On macOS, with the Mac App Store or standalone Tailscale app, the CLI is `/Applications/Tailscale.app/Contents/MacOS/Tailscale`. `flick-serve` looks for `tailscale` on `PATH`, then that path, then `/usr/local/bin/tailscale`.

Then choose what to serve:

```sh
flick-serve --library                    # the videos in the desktop Flick's library
flick-serve --folder /Volumes/Media/Films --folder /Volumes/Media/Shows
```

`--library` reads the desktop app's `settings.json` read-only, so the desktop app can keep running. It finds the file the way the app does: `FLICK_SETTINGS_PATH` if set, else the app's configuration folder (`~/Library/Application Support/app.Flick.Flick/settings.json` on macOS). Posters come from the desktop's poster cache when it has one. Videos still downloading by torrent are listed but answer `409` until Flick has verified them.

`--folder` scans each folder and its subfolders for video files. Symbolic links, to files or folders, are not followed. Videos from `--folder` have no duration or size in pixels in the listing; those come from the desktop library.

One of `--library` or `--folder` is required.

At startup it prints one line of JSON to stdout and keeps running until Ctrl+C (or SIGTERM):

```json
{"listen":"100.98.200.69:47117","pairing":"flick-remote://100.98.200.69:47117/#3f1c…"}
```

Logs go to stderr. Set `RUST_LOG=flick=debug` for more detail. The token never appears in the logs.

### Options

| Option | Default | Meaning |
| --- | --- | --- |
| `--library` | | Serve the desktop app's library. |
| `--folder PATH` | | Serve the videos in a folder; repeat for several. |
| `--bind IP:PORT` | Tailscale IPv4 address and `--port` | Listen here instead. Only Tailscale addresses (`100.64.0.0/10`, `fd7a:115c:a1e0::/48`) and loopback are accepted. |
| `--port PORT` | `47117` | Port when `--bind` is not given. Keep it fixed so paired Flicks keep working across restarts. |
| `--name NAME` | the computer's name (`scutil --get ComputerName`, else the host name) | The section title in the other Flick's library. |
| `--state-dir DIR` | Flick's data folder, in `remote`: `~/Library/Application Support/app.Flick.Flick/remote` on macOS, or `FLICK_DATA_DIR/remote` if that variable is set | Where the token, id salt, and progress are kept. |
| `--print-pairing` | | Print the pairing line for the saved token (making one if needed) and exit. Needs a fixed port, and Tailscale running unless `--bind` is given. |
| `--new-token` | | Replace the saved token. Every paired Flick must pair again. |
| `--ffmpeg PATH`, `--max-transcodes N` | found on `PATH` (then `~/.local/bin`, `/opt/homebrew/bin`, `/usr/local/bin`), `2` | The FFmpeg used for the quality tiers below Original, and how many transcodes may run at once. Without a working FFmpeg only Original is offered. `FLICK_FFMPEG` also names it. |

The library is re-read on every listing request, so a video added on the desktop appears the next time the other Flick refreshes, without restarting the server.

### Accepting downloads from other Flicks

By default the server is read-only. With `--allow-downloads`, paired Flicks may also start torrents here, follow and control them, and play them while they download. The design is in [the remote downloads plan](REMOTE_DOWNLOADS_PLAN.md).

```sh
flick-serve --folder /Volumes/Media/Films --allow-downloads --download-dir /Volumes/Media/Downloads
flick-serve --library --allow-downloads --share-root /Volumes/Media/Films --links
```

| Option | Default | Meaning |
| --- | --- | --- |
| `--allow-downloads` | off | Accept torrents from paired Flicks: resolve, start, pause, resume, and remove (deleting files only when the client echoes the exact file list). Unfinished downloads are listed and can be played while they download. Every other option below needs this one. |
| `--download-dir DIR` | Flick's data folder, in `Downloads` (`FLICK_DATA_DIR/Downloads` if that variable is set) | Where downloads go; each torrent gets its own `<infohash>` folder, link downloads go directly here. The client never chooses a path. The listing shows only this folder's name and free space. |
| `--share-root DIR` | none | A folder paired Flicks may browse and seed from; repeat for several. Only folders and video files inside these roots are shown. A symbolic link is resolved before the check, and anything outside a root answers `404` without naming a path. |
| `--links` | off | Accept link downloads (yt-dlp and HTTPS). Offered only when yt-dlp is found (`FLICK_YTDLP`, then `PATH`, then the usual install folders); otherwise the server logs why and leaves it off. |
| `--download-limit KIB`, `--upload-limit KIB` | unlimited | Speed limits in KiB/s for transfers on this server. |
| `--no-seeding` | seeding on | Stop sharing a download once it completes. |
| `--local-transfers` | off | Keep transfers off the Internet: no DHT, trackers, or port mapping, and listen on `127.0.0.1` only. For tests with explicit local peers. Without it, the server finds peers the way the desktop app does, since it is your own machine. |

A hidden `--peer IP:PORT`, repeatable, adds an explicit peer to every lookup and resume. It exists for tests with `--local-transfers` and does not appear in `--help`.

`flick-serve` refuses `--download-dir`, `--share-root`, `--links`, and the other options above without `--allow-downloads`. While a feature is off, its routes answer `403` with a message that names the desktop setting, Settings → Remote Libraries; for `flick-serve` the equivalent is the flag.

To run it at login with these options, pass them to the launchd installer after `--`, as in [Start at login with launchd](#start-at-login-with-launchd).

What paired Flicks may do is advertised in the listing's `capabilities` (`downloads`, `links`, `shares`, `stream-incomplete`) and `storage`; every command route checks again and answers `403` for anything not allowed. At most three torrent lookups and three link downloads run at once; more answer `429`. A lookup waits at most five minutes for peers.

Completed downloads join the listing: with `--folder`, the download folder is scanned with the other folders; with `--library`, the server keeps the paths it downloaded in `remote-downloads.json` in its state folder and lists them beside the desktop's library, whose `settings.json` stays read-only. A video still downloading is listed with `complete: false` and its verified `progress`; `GET /v1/media/{id}` serves its ranges from verified pieces only, waits up to 30 seconds for a missing piece, then answers `504` so the player can retry. Quality tiers below Original are not offered for an unfinished file.

The transfer state is kept in `transfers` inside the state folder. Stopping the server cancels running link downloads (their partial files are removed) and stops the transfers; they are restored, after verifying what is on disk, the next time it starts.

## Pairing

The pairing string is `flick-remote://<address>:<port>/#<token>`. The token is in the `#` fragment, so pasting the string into a browser by mistake never sends it to a server.

```sh
flick-serve --print-pairing
```

prints it again at any time, including while the server is running. Give it the same `--state-dir`, and the same `--port` or `--bind`, as the running server, or it prints a different address or makes a new token.

On the other Mac, open Flick's **Settings**, find **Remote Libraries**, choose **Add Server…**, paste the string, and choose **Add**. The row then shows the server's address and "Online · N videos", "Offline", or "Wrong token. Pair again". Pasting a string for an address that is already paired replaces its token.

Treat the string like a password. To revoke every paired Flick, run `flick-serve --new-token --print-pairing` and restart the server.

## Start at login with launchd

`scripts/install-flick-serve.sh` installs a launchd agent for the current user from [packaging/macos/com.flick.serve.plist](../packaging/macos/com.flick.serve.plist):

```sh
cargo build --locked --release --no-default-features --bin flick-serve
scripts/install-flick-serve.sh                                     # serves --library
scripts/install-flick-serve.sh -- --folder /Volumes/Media/Films     # or folders
scripts/install-flick-serve.sh --print                              # show the plist, install nothing
scripts/install-flick-serve.sh --uninstall                          # stop and remove it
```

The agent:

- starts at login and restarts 30 seconds after the server stops, which also covers Tailscale not being connected yet right after login;
- runs `flick-serve` inside `caffeinate -is`, so the Mac does not idle-sleep while serving (pass `--no-caffeinate` to leave sleep alone);
- writes logs to `~/Library/Logs/Flick/flick-serve.log`;
- discards stdout, because the startup line contains the token. Use `flick-serve --print-pairing` to get the pairing string.

Check it with:

```sh
launchctl print gui/$(id -u)/com.flick.serve | head -20
tail -f ~/Library/Logs/Flick/flick-serve.log
```

A launchd agent runs only while you are logged in. For a Mac mini without a display, turn on automatic login (**System Settings → Users & Groups → Automatically log in as**), or keep a session logged in.

If your media is on an external drive, macOS may ask the first time whether `flick-serve` may access removable volumes. Answer in **System Settings → Privacy & Security → Files and Folders**, or grant it Full Disk Access.

## Keep the Mac mini awake and reachable

The server is only reachable while the Mac is awake and Tailscale is connected on both machines.

- `caffeinate -is` in the launchd agent stops idle sleep while the server runs.
- To stop sleep entirely, use **System Settings → Energy → Prevent automatic sleeping when the display is off**, or `sudo pmset -a sleep 0` (display sleep is fine: `sudo pmset -a displaysleep 10`).
- **Start up automatically after a power failure** in the same settings pane brings it back after an outage.
- In the Tailscale app, enable **Launch at login** (or run `tailscaled` as a system daemon), so the tailnet address exists before the server starts.

## Check the link with `tailscale ping`

From the MacBook:

```sh
tailscale ping mohtashams-mac-mini
```

`pong from … via 192.168.1.20:41641` means a **direct** path: on the same LAN that comfortably carries the original file of a 4K remux. `via DERP(…)` means the traffic is **relayed** through a Tailscale server, which is slower and varies; expect it to struggle with high bit rate files. Away from home, the Mac mini's home upload speed is the ceiling either way.

## Check it with curl or mpv

With the pairing line's address and token:

```sh
TOKEN=…            # the part after /# in the pairing string
SERVER=http://100.98.200.69:47117

curl -s -H "Authorization: Bearer $TOKEN" "$SERVER/v1/library" | python3 -m json.tool
curl -sI -H "Authorization: Bearer $TOKEN" "$SERVER/v1/media/<id>"
curl -s -H "Authorization: Bearer $TOKEN" -H 'Range: bytes=0-99' "$SERVER/v1/media/<id>" | xxd | head
mpv --http-header-fields="Authorization: Bearer $TOKEN" "$SERVER/v1/media/<id>"
```

Do not put the token in a URL; only the `Authorization` header is accepted.

## HTTP interface

All paths are under `/v1` and need the bearer token. Every error is JSON, `{"message": "…"}`. The types are in `src/remote/protocol.rs`.

| Request | Answer |
| --- | --- |
| `GET /v1/library` | Protocol version, server name, available qualities (`["original"]` for now), and the videos: opaque id, title, file name, size, duration, size in pixels, MIME type, episode, `complete`, whether a poster exists, sidecar subtitles. |
| `GET`/`HEAD /v1/media/{id}` | The file. One `Range` per request (`206` with `Content-Range`), `Accept-Ranges: bytes`, `Content-Length`, `Content-Type`, a strong `ETag` from size and modification time, and `If-Range`. Several ranges, or a range outside the file, get `416`. Streamed from disk with a 64 KiB buffer. `409` while a torrent file is unverified, except a download this server runs with `stream-incomplete`: its ranges come from verified pieces, with `504` after a 30-second wait. |
| `GET /v1/poster/{id}` | The poster PNG, or `404`. |
| `GET /v1/subtitles/{id}` | Sidecar subtitle files: same name as the video, optionally with a language part (`Film.en.srt`), with extension `.srt`, `.ass`, `.ssa`, `.vtt`, or `.sub`. |
| `GET /v1/subtitles/{id}/{n}` | Subtitle file `n` from that list. |
| `GET /v1/hash/{id}` | `{"hash": "<16 hex digits>"}`, the OpenSubtitles movie hash. `422` for files under 64 KiB, `409` while a torrent file is unverified. |
| `GET`/`PUT /v1/progress/{id}` | Playback position, `{"position", "duration", "finished", "updated"}`. `GET` is `404` when nothing is saved. `PUT` answers with what is stored: an `updated` more than a minute older than the stored value is not applied; within a minute, the newest `PUT` wins. `updated: 0` means now. |

With downloads allowed, the listing also has `capabilities` and `storage` (`free_bytes`, `download_folder_name`), and each video has `progress` from 0 to 1. These routes answer `403` unless the listing advertises the capability in brackets:

| Request | Answer |
| --- | --- |
| `POST /v1/transfers/resolve` `{"input"}` [downloads] | The torrent's infohash, name, and files. `input` is a magnet link, a 40-digit infohash, an `https://` link to a `.torrent` file, or `base64:` and the file's bytes (at most 16 MiB); anything else, such as a path, is `400`. `429` when three lookups already run; `504` when no peer supplied the details within five minutes. |
| `POST /v1/transfers` `{"input", "files": [index]}` [downloads] | `201` `{"id": "<infohash>"}`. An empty `files` selects every file. `409` when this server already has the torrent. |
| `GET /v1/transfers` [downloads] | Every transfer: phase (`checking`, `downloading`, `paused`, `seeding`, `error`), verified and total bytes, checked bytes, uploaded bytes, peers, rates, and its files with their `path`, verified bytes, and library `video_id`. |
| `POST /v1/transfers/{id}/pause`, `/resume`, `/v1/transfers/pause-all`, `/resume-all` [downloads] | `204`. |
| `DELETE /v1/transfers/{id}` `{"delete_files", "confirm": [path]}` [downloads] | `204`. With `delete_files`, `confirm` must list every selected file's `path` exactly; otherwise `409` with the current list in `files`, and nothing is deleted. |
| `POST /v1/fetches` `{"url"}`, `GET /v1/fetches`, `DELETE /v1/fetches/{id}` [links] | Link downloads into the download folder: `201` `{"id"}`, the list with phase, bytes, speed, errors, and saved video ids, and `204` on cancel. `429` when three already run. |
| `GET /v1/browse?dir=` [shares] | Folders and video files. `dir` is empty for the allowed roots by name, else `<root name>/<relative path>`; anything outside a root is `404` "No such folder on this server." |
| `POST /v1/shares` `{"dir"}` [shares] | `201` with the `infohash` and `magnet` of the seeded folder or file. |

A request without the token is `401`, even for a path that does not exist. An unknown id or path is `404`. Ids are SHA-1 of a random per-server salt and the file's canonical path, so they stay the same across restarts and never reveal a path.

## Files it keeps

In the state folder (`--state-dir`):

| File | Contents |
| --- | --- |
| `token` | The pairing token, mode 0600. |
| `salt` | The id salt, mode 0600. Deleting it changes every id; paired Flicks then need a refresh and lose remote progress. |
| `progress.json` | Saved positions by video id. |
| `serve.lock` | Held while a server runs, so two servers cannot share the folder. |
| `transfers/` | With `--allow-downloads`: the transfer session's state, restored at the next start. |
| `remote-downloads.json` | With `--allow-downloads --library`: the completed downloads it lists beside the desktop's library. |

The desktop Flick keeps its cached listings of paired servers (`<server id>.json`) and downloaded subtitles (`subtitles/`) in the same default folder. On a machine that runs both, the two sets of files sit side by side; their names do not overlap.

## Troubleshooting

- **"Could not find this machine's Tailscale address"**: Tailscale is not running or not logged in. Run `tailscale status`. Or pass `--bind` with the address.
- **"… is not a Tailscale address"**: `--bind` must be a `100.x.y.z` Tailscale address (or `127.0.0.1` for a local check).
- **"Another Flick server is already using …"**: a server, often the launchd agent, already runs with this state folder. Stop it with `scripts/install-flick-serve.sh --uninstall`, or use another `--state-dir`.
- **401 on every request**: the token does not match. Copy the pairing string again with `flick-serve --print-pairing`.
- **Playback stutters away from home**: check `tailscale ping`; a relayed path or a slow home upload cannot carry a high bit rate original.
