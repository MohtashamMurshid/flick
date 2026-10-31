# Contributing to Flick

Bug reports, fixes, and features are welcome. Open an issue first for anything large, so we can agree on the approach before you spend time on it.

By contributing, you agree that your contributions are licensed under the [MIT License](LICENSE).

## Set up

Follow [Getting started](README.md#getting-started) to install Rust, the platform's build prerequisites, and libmpv. The repository pins Rust 1.99.0 in `rust-toolchain.toml`; rustup selects it automatically. Building on macOS needs full Xcode for GPUI's Metal shaders. [Validation](docs/VALIDATION.md#native-build-prerequisites) lists the native build prerequisites for each platform.

## Ground rules

- **Keep playback and torrent transfers separate.** The transfer code builds and tests without GPUI or libmpv (`--no-default-features`), and CI checks that no desktop dependency leaks into that build.
- **Transfer tests use generated local fixtures and explicit local peers**, never public trackers or DHT.
- **Link-download tests use a generated stand-in for yt-dlp and loopback servers**, never the Internet.
- **Record what you actually ran** in [docs/VALIDATION.md](docs/VALIDATION.md). Keep three things distinct: a passing backend test, a desktop build, and actual interactive playback. CI configuration on its own is not evidence that something works.

## Checks

Run these before opening a pull request. CI runs them on Linux, Windows, and macOS.

```sh
cargo fmt --all -- --check
cargo check --locked --all-targets
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
```

Transfers without the desktop, as CI runs them:

```sh
cargo clippy --locked --no-default-features --all-targets -- -D warnings
cargo test --locked --no-default-features --all-targets
```

### Real playback

Generate a synthetic clip with FFmpeg and exercise the real playback boundary:

```sh
bash scripts/make-test-video.sh /tmp/flick-test.mp4 --with-subtitles
FLICK_AUDIO=null cargo run --locked -- --smoke-test /tmp/flick-test.mp4
FLICK_AUDIO=null FLICK_TEST_VIDEO=/tmp/flick-test.mp4 \
  cargo test --locked playback::tests::real_playback -- --ignored --nocapture
```

`FLICK_AUDIO=null` is for automated tests without an audio device; omit it for normal audible playback. The CLI smoke test does not open a desktop window. Its first-frame timing describes a software presentation submission, not a measured display scanout time.

### Link downloads

Try link downloads without the desktop. `get` prints one JSON update per line, and Ctrl+C cancels:

```sh
cargo run --locked --no-default-features --bin flick-fetch -- tools
cargo run --locked --no-default-features --bin flick-fetch -- archive Nabisco_2
cargo run --locked --no-default-features --bin flick-fetch -- get "https://archive.org/details/Nabisco_2" --dest /tmp/flick-links
```

An opt-in test drives the installed yt-dlp against a loopback server only:

```sh
cargo test --locked --no-default-features --lib installed_ytdlp -- --ignored
```

### Torrent transfers

`flick-transfer` exercises the native transfer backend without GPUI or libmpv. See [the transfer diagnostic](docs/TRANSFERS.md).

### Benchmark

Run a sustained release benchmark with a generated local 4K/30 fps fixture (FFmpeg with libx264 required):

```sh
FLICK_AUDIO=null bash scripts/benchmark.sh                 # 300 seconds at 3840x2160
FLICK_AUDIO=null bash scripts/benchmark.sh 60 --size 1920x1080
FLICK_AUDIO=null FLICK_HWDEC=no cargo run --locked --release --bin flick -- \
  --benchmark /path/to/local-video.mp4 --seconds 300 --size 3840x2160 --interval-ms 16
```

The script removes its temporary 12-second H.264 clip on exit. Stdout is one JSON report; diagnostics go to stderr. The CLI loops the file through the real software renderer without a GPUI window. It reports CPU, RSS, mpv drop counters, source and decoder metadata, and render-call percentiles after the first frame. Frame counts describe software submissions; GPUI upload, display timing, and audible playback are outside this measurement. See the [performance gate](docs/VALIDATION.md#performance-gate) for units, counter limitations, and recorded results.

## Pinned dependencies

GPUI 0.2.2 is pinned. Its transitive `xattr` dependency still uses the `ENOATTR` alias removed in libc 0.2.190, so Flick temporarily pins libc 0.2.189. Revisit this pin when updating GPUI.

## Code layout

- `src/ui.rs`: GPUI window, video image lifecycle, controls, menus, and input.
- `src/playback.rs`: dynamically loaded libmpv boundary and software presentation.
- `src/presentation.rs`: physical display sizing without fixed resolution limits.
- `src/library_ui.rs`: library, history, search results, torrent selection, and Downloads views.
- `src/palette_ui.rs`: the ⌘K / Ctrl+K command center.
- `src/store.rs`: local preferences, recent files, and resume positions.
- `src/model.rs`: time and seek helpers.
- `src/smoke.rs`: real media playback check without a desktop window.
- `src/torrent.rs`: native transfer adapter, metadata, sharing, selected-file downloads, verification, and session persistence.
- `src/services.rs`: desktop background worker and transfer events.
- `src/catalog.rs`: independent native discovery providers with progressive results.
- `src/fetch.rs`: link downloads without GPUI, separate from the torrent session: yt-dlp processes, HTTPS files, and Internet Archive file lists.
- `src/fetch_ui.rs`: Link Downloads on the Downloads page and the Internet Archive file chooser.
- `src/input.rs`: text fields with Unicode/IME and clipboard support, including selectable read-only values.
- `src/shortcuts.rs`: rebindable player shortcuts.
- `src/subtitles.rs`: OpenSubtitles search and download.
- `src/thumbnails.rs`: poster frames, media details, and timeline previews from background players.
- `src/theme.rs`: interface styles, WCAG contrast adjustment, and appearance settings.
- `src/bin/flick-transfer.rs`: development transfer diagnostic without GPUI or libmpv.
- `src/bin/flick-fetch.rs`: development diagnostic for link downloads.

The [implementation plan](docs/PLAN.md) tracks what is built and what is planned.
