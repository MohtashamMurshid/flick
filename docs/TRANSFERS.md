# Transfer development diagnostic

The native transfer adapter and `flick-transfer` diagnostic are implemented. Desktop library search, torrent file selection, Downloads controls, completed-video import, and Sharing controls are implemented. This tool exercises the same adapter as those controls; it is not a remote API or an installed background service.

The adapter pins librqbit 9.0.1, uses separate transfer state with an exclusive lock, resolves file metadata before download, supports selected files and pause/resume, and exposes completed files only after piece verification. New downloads get a directory named by infohash under the chosen destination. Existing unrelated destinations are refused. Removing a transfer and deleting its files are separate backend operations.

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

The diagnostic defaults to local explicit peers, with DHT, trackers, multicast discovery, and automatic port mapping disabled. `--public` explicitly enables DHT/trackers. For sharing with another machine, choose a reachable listener address with `--listen`; loopback is accessible only from this machine. Peer discovery and network reachability are separate requirements. Ordinary BitTorrent sharing is not private cloud storage.

Press Ctrl+C to stop sharing successfully. Interrupting inspect or download before completion exits with an error instead of reporting success without a result. A successful download command stops its session after printing the verified result; it does not continue seeding in the background. Inspect/download deadlines accept 1–86400 seconds and cover command work after session startup; session shutdown adds about a second. Logs/errors go to stderr, and command results go to stdout as JSON. Transfer state stays separate from viewing settings/history.

## Compatibility notes

- BitTorrent v1 and hybrid magnets with a v1 infohash are supported; v2-only support is not established.
- The pinned engine can append an extra empty piece when creating torrents whose total size is exactly divisible by the piece length. The adapter removes only that known invalid trailing hash and re-encodes the info dictionary before calculating/exporting the magnet. The regression independently checks piece content and the infohash with SHA-1. Other invalid creation output is rejected before starting a seed.
- Shared content is verified in place before it is announced. Metadata is exported only to an explicitly chosen diagnostic output, rather than written into the shared folder automatically. Symbolic links and special files are refused.
- Snapshots expose checking/downloading/paused/seeding/error state, selected files, verified progress, peers, and transfer speeds. `completed_file` rejects incomplete or unselected files. A partial file's existence is not a playback guarantee.
- Streaming incomplete downloads, further provider maintenance, installer packaging, and cross-platform GUI acceptance remain in [the plan](../PLAN.md).

## Isolated desktop checks

The desktop worker starts its native transfer session on a transfer action or when opening Downloads. It uses public discovery for normal user downloads. Development checks can set `FLICK_SETTINGS_PATH`, `FLICK_DATA_DIR`, and `FLICK_DOWNLOAD_DIR` to task-owned paths. `FLICK_LOCAL_TRANSFERS=1` disables public DHT/trackers and binds loopback; `FLICK_INITIAL_PEERS=127.0.0.1:PORT` supplies an explicit test seed. These environment settings are development controls, not extra setup required by the user flow.
