# Remote library over Tailscale

**Status: proposed on 2026-11-10; not implemented.** This plan adds a second machine to Flick: one Flick keeps the library and the files, another Flick on the same tailnet browses that library and plays from it at source quality. It does not change local playback, transfers, or link downloads.

## Outcome

A Mac mini (or any PC) runs Flick with the storage. A MacBook on the same Tailscale network opens Flick, sees the Mac mini's library beside its own, picks a video, and watches it. Playback is the original file streamed as is: the same tracks, subtitles, resolution, and bit rate as opening it locally. Nothing is copied to the MacBook. Seeking, resume, episodes, and the player controls work as they do for local files.

Nothing is reachable from the Internet. The server binds only to the machine's Tailscale address and refuses to start on any other address. Every request also carries a pairing token, because a tailnet can contain nodes shared in from other people. In this tailnet, for example, `tailscale status` lists a node owned by another login.

## Why stream the file, not a transcode

The client is a full libmpv player, so it can decode anything the server can. Sending the source bytes keeps quality exact, keeps every audio and subtitle track, keeps HDR metadata, needs no FFmpeg on the server, and costs the server nothing but disk reads. Tailscale between two Apple Silicon machines on the same LAN establishes a direct WireGuard path whose throughput comfortably exceeds a 4K remux. **Original** is therefore the default quality.

Away from home the link is the home connection's upload speed, and sometimes a Tailscale relay. That is where the lower quality tiers below come in. They are an explicit choice in the player, never a silent downgrade.

| Content | Typical bit rate | Fits a direct tailnet link |
| --- | --- | --- |
| 1080p web rip | 5–15 Mbit/s | Yes |
| 4K streaming-grade | 15–30 Mbit/s | Yes |
| 4K HEVC Blu-ray remux | 50–100 Mbit/s | Yes on LAN; check with `tailscale ping` that the path is direct, not relayed |

## Quality tiers for links away from home

The server offers a ladder. Original is the file itself. The others are live transcodes made by FFmpeg on the server, H.264 video and AAC stereo audio, chosen because every client decodes them in hardware.

| Tier | Video | Target bit rate | When |
| --- | --- | --- | --- |
| Original | the file | the file's own | Home LAN, or any direct path with headroom |
| 1080p High | 1920×1080 H.264 | 12 Mbit/s | Fast fibre upload |
| 1080p | 1920×1080 H.264 | 6 Mbit/s | Ordinary home upload |
| 720p | 1280×720 H.264 | 3 Mbit/s | Hotel Wi-Fi, relayed paths |
| 480p | 854×480 H.264 | 1.5 Mbit/s | Cellular |

The listing says which tiers the server can offer: none when FFmpeg is missing, all when an encoder is found. The server probes `ffmpeg -encoders` once and prefers hardware: `h264_videotoolbox` on macOS, `h264_nvenc` or `h264_qsv` on other systems, `libx264` otherwise. HDR sources are tone-mapped to SDR when transcoded; Original keeps HDR.

Transcodes are delivered as HLS, which libmpv plays natively: `GET /v1/hls/{id}/{tier}/index.m3u8` returns a complete VOD playlist with fixed 4-second segments computed from the duration, plus `init.mp4` and `{n}.m4s`. When a segment is requested that has not been made, the server starts, or repositions, one FFmpeg job for that client at that segment's start time, with key frames forced on segment boundaries so every segment is independently decodable. The job stays ahead of the playhead and stops after a minute without requests. Segments live in a temporary directory per job, capped by size, and are removed when the job ends. At most two transcode jobs run at once; a third request gets 503 with a message the client shows. Seeking in a transcode starts a new job at the target, which costs a second or two before video resumes; the client shows its buffering state during that time.

Text subtitle tracks inside the file are extracted on demand as WebVTT, `GET /v1/subtitles/{id}/embedded/{track}`, so subtitles work at every tier. Bitmap subtitles (PGS, DVD) are listed as unavailable in transcode tiers. Transcodes use the default audio track; choosing another audio track in a transcode is a later item.

The client has the tier in the player's settings menu, with the server's default under Remote libraries. Switching tiers reloads at the current position. An automatic mode is a later item: start at the chosen tier, and when libmpv reports it paused for cache repeatedly in the first minute, offer the next tier down.

## Architecture

Three pieces, all in this repository:

- **`remote` module** in the shared library (`src/lib.rs`), with no GPUI or libmpv dependency. It holds the wire types, the stable video identifiers, the token check, the Tailscale address check, and the HTTP server. It builds and tests with `--no-default-features`, like `torrent` and `fetch`.
- **`flick-serve` binary** (`src/bin/flick-serve.rs`): a headless server for a Mac mini or a Linux box with no display. It serves the library saved by the desktop app, or folders given on the command line.
- **Desktop changes**: a Remote libraries page in Appearance to add a server; the library shows the remote videos; the player loads them over HTTP. Later, a switch in Appearance runs the same server inside the desktop app so no separate process is needed.

### Wire protocol

HTTP/1.1 and JSON. Every request carries `Authorization: Bearer <token>`. The server answers 401 without it and compares tokens in constant time.

| Request | Response |
| --- | --- |
| `GET /v1/library` | Server name, protocol version, and the videos: id, title, file name, size, duration, width, height, episode parse, whether it is complete, whether a poster exists, sidecar subtitle count. |
| `GET /v1/media/{id}` and `HEAD` | The file bytes. Supports single `Range` requests (206, `Content-Range`, `Accept-Ranges: bytes`), rejects multi-range (416), sends `Content-Length`, `Content-Type`, a strong `ETag`, and honors `If-Range`. Streams from disk with a bounded buffer; never reads the file into memory. |
| `GET /v1/poster/{id}` | The cached poster PNG the server's thumbnail worker already makes, or 404. |
| `GET /v1/subtitles/{id}` and `/{id}/{n}` | Sidecar subtitle files next to the video, by index. |
| `GET /v1/hash/{id}` | The OpenSubtitles movie hash, computed on the server because the client cannot read the file. |

Identifiers are opaque: SHA-1 of a per-server random salt plus the canonical path, as 40 hex characters. The server keeps the id-to-path map in memory and rebuilds it from the library. The client never sends a path, so there is no path traversal to defend. An id whose file is an incomplete torrent download returns 409 until the transfer verifies it.

Protocol version is in the listing. A client that sees a newer major version shows a "update Flick on the Mac mini" message instead of guessing.

### Binding to Tailscale only

The server resolves its address in this order: an explicit `--bind` flag, then the `tailscale ip -4` command (searched on `PATH`, then the Mac App Store app's bundled CLI, then `/usr/local/bin`). Before binding it checks that the address lies in Tailscale's ranges, `100.64.0.0/10` or `fd7a:115c:a1e0::/48`. Any other address is refused, with one exception: loopback is allowed so tests and the local smoke check can run without Tailscale. `0.0.0.0` is never accepted. The check is a pure function with unit tests.

The port defaults to a fixed value and is saved with the token, so the client's saved address keeps working across restarts.

On the tailnet, traffic is already encrypted by WireGuard, so the server speaks plain HTTP. HTTPS via `tailscale cert` and MagicDNS is a later option, not a requirement. Running the server on loopback behind `tailscale serve` is an alternative that also gives identity headers for free; it is noted here, not chosen, because it needs a second command outside Flick.

### Pairing

Turning the server on generates a random token and shows a pairing string, `flick-remote://mohtashams-mac-mini.taildcfc7a.ts.net:PORT/#<token>`, with a Copy button. On the client, Appearance → Remote libraries → Add pastes it. The token is stored in the client's `settings.json` beside the OpenSubtitles key and never logged. `FLICK_REMOTE_TOKEN` overrides it for development, the way `FLICK_OPENSUBTITLES_KEY` does.

A later step can replace the token with the caller's tailnet identity from Tailscale's local `whois` API, allowing only the same login. The token stays as the baseline because it is testable on loopback.

### Client playback

The desktop loads `http://<server>/v1/media/<id>` through the existing `Player::load`, which already accepts any string libmpv understands. libmpv's HTTP demuxer does range requests, so seeking is native. Before the load the client sets `http-header-fields` to the bearer header and clears it after the file loads, so the token never appears in a URL. For smooth high bit rate playback the client raises `demuxer-max-bytes`, sets `demuxer-readahead-secs`, and sets `network-timeout`; the values are decided by measurement in validation, not guessed.

Sidecar subtitles load with `sub-add` on their HTTP URL. OpenSubtitles search uses the hash endpoint instead of reading the file. Posters are fetched as PNG bytes and shown with GPUI's image element rather than decoded by the client's thumbnail worker.

### Where the desktop changes

- `src/ui.rs:798` (`open_prepared`) is the seam. Today it takes a `PathBuf`, requires the path to be a file or an incomplete torrent, and builds the source string. It becomes a `Source` enum: `Local(PathBuf)`, `Torrent { infohash, index }`, `Remote { server, id }`. The `self.path` field that resume, history, and the title bar read becomes a `Source` as well. This is the largest piece of client work.
- `LibraryVideo` in `src/store.rs` stays local-only. The file watcher in `src/library_files.rs` checks every library path on disk, so remote videos must not enter `settings.library`. Remote videos live in a per-server in-memory list refreshed when the library opens, with a short cache on disk so the section renders before the first reply. The store saves only the server list and resume positions keyed by `(server id, video id)`.
- `src/library_ui.rs` gets a section per remote server, named after the server, with a badge on cards. Continue Watching includes remote items. A server that does not answer shows one offline row, not an error dialog, and local videos are unaffected.
- `src/settings_ui.rs` gets Remote libraries (add, remove, test connection, forget token) and, in the last phase, "Share my library on Tailscale" with the pairing string.

### Headless server on the Mac mini

```sh
flick-serve --library            # serve the videos in Flick's settings.json
flick-serve --folder /Volumes/Media/Films --folder /Volumes/Media/Shows
flick-serve --bind 127.0.0.1:0 --folder /tmp/fixtures   # tests and local checks
```

`--library` opens the desktop settings file read-only, so the desktop app can keep running. Posters come from the desktop's cache directory when present; the headless server does not generate posters itself in the first version. A launchd agent plist in `packaging/macos` starts it at login. `--print-pairing` prints the pairing string for a fresh token.

## Delivery order

### 1. Server core and headless binary

- [ ] Add the HTTP server dependency and keep it out of the desktop feature gate check in CI. Candidate: `axum` 0.8 on the existing Tokio runtime; a hand-written range handler rather than another middleware crate.
- [ ] `remote::ids`, `remote::auth`, `remote::bind` with unit tests: id stability, constant-time token compare, Tailscale range check that rejects `0.0.0.0`, LAN, and public addresses.
- [ ] `remote::server` with the listing, media, poster, subtitle, and hash routes.
- [ ] `flick-serve` with `--library`, `--folder`, `--bind`, `--port`, `--print-pairing`.
- [ ] Integration tests on loopback with generated fixtures: 401 without token, 404 unknown id, 206 ranges including the last byte and an open-ended range, 416 for multi-range and out-of-bounds, `HEAD` lengths, `If-Range` with a stale ETag, 409 for an incomplete torrent file, and a concurrent-reader test that two clients reading different ranges get the right bytes.
- [ ] `remote::transcode` and `remote::hls`: FFmpeg and encoder discovery, the tier ladder, playlist generation, the segment job manager, WebVTT extraction, and tests driven by a generated stand-in `ffmpeg` script, following the yt-dlp stand-in pattern. Real FFmpeg is exercised only by an ignored test.
- [ ] Manual check from the MacBook with stock mpv: `mpv --http-header-fields='Authorization: Bearer …' http://mohtashams-mac-mini.taildcfc7a.ts.net:PORT/v1/media/<id>`. This proves the transport end to end before any desktop work and is recorded in validation as a real playback check with the observed bit rate and `tailscale ping` result.

### 2. Desktop client

- [ ] `Source` enum through `open_prepared`, resume, history, and the title.
- [ ] Remote libraries settings page and pairing string parsing, with tests for the parser.
- [ ] Library section per server, posters over HTTP, offline row.
- [ ] Playback with the bearer header set and cleared around load; cache tuning by measurement.
- [ ] Sidecar subtitles, embedded WebVTT subtitles, and the OpenSubtitles hash over the wire.
- [ ] Quality tier picker in the player and a per-server default; switching reloads at the current position.
- [ ] An ignored playback test like `playback::tests::real_playback` that starts the server on loopback with `FLICK_TEST_VIDEO` and plays through it.

### 3. In-app server and extras

- [ ] "Share my library on Tailscale" in Appearance, running the server on the services runtime; the switch is off by default and the state survives restarts.
- [ ] launchd agent for `flick-serve`.
- [ ] Resume position sync: `PUT /v1/progress/{id}` so Continue Watching matches on both machines. Client wins on conflict within the same minute; otherwise the newer timestamp wins.
- [ ] Watch while downloading over remote: serve incomplete torrent files through `TransferSession::open_stream` so verified pieces stream and unverified ones block, the same guarantee as local playback.
- [ ] Tailnet identity through Tailscale `whois` as an alternative to the token.
- [ ] Optional HTTPS via `tailscale cert`.

## Constraints to know before relying on it

- **Upload speed at home is the ceiling away from home.** Original quality of a 4K remux needs more than most home connections upload. The lower tiers exist for that case.
- **Direct or relayed.** `tailscale ping <machine>` says whether the path is direct or goes through a Tailscale relay. Relayed paths are slower and vary; expect 720p there.
- **FFmpeg on the server** is required for any tier but Original. On Apple Silicon, VideoToolbox encodes 4K to 1080p in real time without loading the machine. Without a hardware encoder, `libx264` at 1080p needs a modern CPU.
- **Transcodes are lossy** and tone-map HDR to SDR, carry only the default audio track, and cannot show bitmap subtitles.
- **The server machine must stay awake and reachable.** A Mac mini needs sleep disabled or the launchd agent plus `caffeinate`, and Tailscale running on both machines.
- **Two transcodes at once**, then 503. Original streams have no such limit.

## Not in scope

No Internet exposure, no accounts, no casting, no mobile client, no writes to the server's library from the client, and no remote control of the server's player. These remain outside this plan; some may become their own later.

## Validation

Each phase records in [VALIDATION.md](VALIDATION.md), separately: the loopback backend tests, the desktop build, and actual interactive playback from a second machine over Tailscale. The playback entry names both machines, the Tailscale version, whether `tailscale ping` reported a direct or relayed path, the file's bit rate and resolution, and whether seeking and resume worked. A passing loopback test is not evidence that playback over the tailnet works.
