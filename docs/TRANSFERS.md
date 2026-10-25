# Transfer development diagnostic

The native transfer adapter and `flick-transfer` diagnostic are implemented. Desktop library search, torrent file selection, Downloads controls, completed-video import, and Sharing controls are implemented. This tool exercises the same adapter as those controls; it is not a remote API or an installed background service.

The adapter pins librqbit 9.0.1, uses separate transfer state with an exclusive lock, resolves file metadata before download, supports selected files and pause/resume, and exposes completed files only after piece verification. New downloads get a directory named by infohash under the chosen destination. Existing unrelated destinations are refused. Removing a transfer and deleting its files are separate backend operations.

## Desktop transfer controls

Downloads shows verified download progress, download/upload rates, uploaded bytes reported by the engine, peers, and a separate **Checking files…** percentage during file verification. Uploaded totals are the native live-state counter, not a lifetime accounting ledger. Initial share creation shows **Checking files…** while creating piece hashes; librqbit exposes no percentage for that initial operation. Restarted transfers recheck their files before Play becomes available.

Each torrent, whether shared or downloaded, has **Export .torrent…**, which uses the native save picker and writes the stored metainfo bytes without re-encoding. Export refuses to overwrite managed torrent content. **Remove · Keep Files** unregisters a transfer. **Remove and Delete Files…** first lists every metainfo file by its destination path and requires **Delete Listed Files**. The adapter deletes only those files, retains unrelated files and directories, and rejects symlinks/reparse points or replaced paths. Shared torrents delete their original shared files if this action is confirmed. On Unix, deletion uses directory descriptors and `unlinkat` without following parent symlinks. Removal is not recursive; empty folders remain.

**Pause All / Resume All** controls torrent transfers, including explicit shares. Link Downloads have their own Cancel controls and remain independent. Resume All honors the completed-download seeding preference.

The welcome page and Downloads ask whether completed downloads should keep sharing. Until a choice is saved, completed downloads pause after verification. Change the choice in **Appearance → Transfers**. Turning sharing off pauses completed downloads in the background (checked every 250 ms); explicit **Share Files…** torrents keep sharing until paused or removed. Turning it on allows sharing; use Resume on transfers that are already paused. This policy applies to persisted downloads after reopening Flick. Explicit-share identities are stored separately with transfer state. All transfers and shares stop when Flick closes.

The same Transfers group provides download and upload sliders, in 256 KiB/s steps up to 32 MiB/s; zero means unlimited. Both limits are saved in settings, used at session startup, and applied live through librqbit's session limits. They apply to torrents, not yt-dlp or HTTPS downloads. Buttons, switches, and sliders use the existing focusable controls for all three interface styles; sliders support arrow keys, Home, and End.

Verified torrent videos are bound to `(infohash, file index)` in viewing settings as well as their local path. Re-downloading that identity into a different destination restores the saved position. Bindings and saved torrent positions are additive: existing path history remains intact, and local-only videos continue to resume by path. Flick cannot identify an arbitrary manually moved file until it is associated with the torrent again.

## Backend diagnostic

The diagnostic requires no GPUI, Metal, display, audio device, or libmpv:

```sh
cargo run --locked --no-default-features --bin flick-transfer -- --help
cargo test --locked --no-default-features --all-targets
```

The repository pins Rust 1.99.0 so ordinary `cargo` commands use the same compiler as validation and CI, even when the machine's global `stable` alias is older.

## Try a local transfer

First generate a video outside the repository:

```sh
bash scripts/make-test-video.sh /tmp/flick-transfer-video.mp4 --with-subtitles
```

In one terminal, seed it. The explicit listener is a BitTorrent TCP socket. The output is a JSON object with the magnet, infohash, and listener. The command refuses to overwrite an existing `.torrent` file. Keep it running to supply the video:

```sh
cargo run --locked --no-default-features --bin flick-transfer -- \
  --state-dir /tmp/flick-seed-state \
  --download-dir /tmp/flick-seed-downloads \
  --listen 127.0.0.1:45831 \
  share /tmp/flick-transfer-video.mp4 \
  --torrent /tmp/flick-transfer-video.torrent
```

In another terminal, inspect the files without starting a download:

```sh
cargo run --locked --no-default-features --bin flick-transfer -- \
  --state-dir /tmp/flick-receiver-state \
  --download-dir /tmp/flick-receiver-downloads \
  inspect /tmp/flick-transfer-video.torrent
```

Download and verify file index 0, with a 60-second deadline:

```sh
cargo run --locked --no-default-features --bin flick-transfer -- \
  --state-dir /tmp/flick-receiver-state \
  --download-dir /tmp/flick-receiver-downloads \
  download /tmp/flick-transfer-video.torrent \
  --file 0 --peer 127.0.0.1:45831 --timeout-seconds 60
```

The successful result lists the absolute downloaded file path. Open that path in Flick using the existing Open action or `cargo run --locked -- /absolute/path/to/video.mp4`.

Inspect and download also accept an HTTPS torrent URL, a magnet, or a 40-character v1 infohash. Resolving a magnet needs a reachable peer with its metadata. Repeat `--file INDEX` to select several files; omit it to select all. Repeat `--peer ADDRESS` for multiple explicit peers. When resuming a local-only transfer, supply the peer again because the pinned engine does not persist initial peer addresses.

The diagnostic defaults to local explicit peers, with DHT, trackers, multicast discovery, and automatic port mapping disabled. `--public` explicitly enables DHT, fallback trackers, and UPnP port mapping; its listener defaults to `0.0.0.0` instead of loopback. For sharing with another machine, choose a reachable listener address with `--listen`; loopback is accessible only from this machine. Peer discovery and network reachability are separate requirements. Ordinary BitTorrent sharing is not private cloud storage.

Press Ctrl+C to stop sharing successfully. Interrupting inspect or download before completion exits with an error instead of reporting success without a result. A successful download command stops its session after printing the verified result; it does not continue seeding in the background. Inspect/download deadlines accept 1–86400 seconds and cover command work after session startup; session shutdown adds about a second. Logs/errors go to stderr, and command results go to stdout as JSON. Transfer state stays separate from viewing settings/history.

## Compatibility notes

- BitTorrent v1 and hybrid magnets with a v1 infohash are supported; v2-only support is not established.
- The pinned engine can append an extra empty piece when creating torrents whose total size is exactly divisible by the piece length. The adapter removes only that known invalid trailing hash and re-encodes the info dictionary before calculating/exporting the magnet. The regression independently checks piece content and the infohash with SHA-1. Other invalid creation output is rejected before starting a seed.
- Shared content is verified in place before it is announced. Metadata is exported only to an explicitly chosen diagnostic output, rather than written into the shared folder automatically. Symbolic links and special files are refused.
- Snapshots expose checking/downloading/paused/seeding/error state, selected files, verified progress, peers, and transfer speeds. `completed_file` rejects incomplete or unselected files. A partial file's existence is not a playback guarantee.
- Further provider maintenance, installer packaging, and cross-platform GUI acceptance remain in [the plan](../PLAN.md).

## Isolated desktop checks

The desktop worker starts its native transfer session on a transfer action or when opening Downloads. It uses public discovery for normal user downloads. Development checks can set `FLICK_SETTINGS_PATH`, `FLICK_DATA_DIR`, and `FLICK_DOWNLOAD_DIR` to task-owned paths. `FLICK_LOCAL_TRANSFERS=1` disables public DHT/trackers and binds loopback; `FLICK_INITIAL_PEERS=127.0.0.1:PORT` supplies an explicit test seed. These environment settings are development controls, not extra setup required by the user flow.

## Public discovery and listener comparison

Public sessions add the same Torlink tracker list used in search magnets as fallback trackers for every torrent. Local sessions ignore trackers, including those embedded in input magnets or metadata. Multicast discovery stays disabled.

The DHT routing table and peer store are saved to `dht.json` in the transfer state directory using librqbit's periodic persistence (every 60 seconds). The BitTorrent listen port is saved separately in `listen-port`. Public sessions choose an available port in 49152–65535 once, reuse it on later launches, and replace it if it is occupied. An explicit nonzero `--listen` port takes precedence; an occupied port falls back to an available port. The actual selected port is announced to peers and used for UPnP. The pinned engine requests TCP mappings only. UPnP still depends on the router and network; enabling it does not prove an external port mapping succeeded. Local sessions keep their ephemeral loopback listener and create neither public state file.

The desktop allows five minutes to find peers with torrent metadata. Its “Finding peers…” notice has a Cancel button that stops the pending resolution. Cancellation leaves existing downloads running. A timeout suggests checking connectivity, choosing active seeders, or opening a `.torrent` file.

The diagnostic accepts `--transport tcp-only` or `--transport tcp-and-utp`. Public sessions default to TCP plus uTP after the [recorded Sintel comparison](VALIDATION.md#2026-10-24-public-discovery-and-listener-comparison); local sessions default to TCP. In librqbit 9.0.1 the uTP listener socket also enables outgoing uTP; TCP remains enabled. To measure a legal public torrent, use fresh directories for each run and pass its magnet to `benchmark`:

```sh
cargo run --locked --no-default-features --bin flick-transfer -- \
  --state-dir /tmp/flick-public-tcp-1/state \
  --download-dir /tmp/flick-public-tcp-1/downloads \
  --public --transport tcp-only \
  benchmark 'magnet:?xt=urn:btih:08ada5a7a6183aae1e09d831df6748d566095a10' \
  --window-seconds 120
```

This is the Sintel torrent from `https://webtorrent.io/torrents/sintel.torrent`. The benchmark emits JSON lines every 250 ms, including metadata resolution time, first verified progress, live peer counts, and completion or progress at the end of the window. Timing starts after session startup and includes metadata resolution. If metadata never arrives, the resolve deadline ends the run with an error; no download has started. No public swarm is contacted by automated tests. Remove the downloaded media after manual measurements.

## Web seeds

As checked on 2026-10-24, the [upstream release list](https://github.com/ikatson/rqbit/releases) and GitHub latest-release API still identify v9.0.1 as the latest release. No newer upstream release with BEP 19 support was found. The [web-seed request](https://github.com/ikatson/rqbit/issues/500) remains open; it links a separate fork that implements HTTP sources. The pinned 9.0.1 source has no `url-list` download path. Flick keeps that dependency pinned; direct Archive HTTPS downloads belong to the separate URL-download backend.

## Watching while downloading

The desktop registers a `flicktorrent://<infohash>/<file index>` libmpv protocol. Reads go through librqbit's file stream, which returns only downloaded and verified pieces and steers the piece picker toward the read position. A task on the transfer runtime performs each read, and libmpv's cancel callback ends a read waiting for pieces. Partly downloaded files are never opened from disk: library cards for them use the stream, and posters wait until the file is complete.

`FLICK_DOWNLOAD_LIMIT` sets a download cap in bytes per second for the desktop session. It exists to check streaming against a fast local seed, for example with `FLICK_LOCAL_TRANSFERS=1 FLICK_INITIAL_PEERS=127.0.0.1:<port> FLICK_DOWNLOAD_LIMIT=300000`.
