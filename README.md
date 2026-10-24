# Flick

A desktop video player built in Rust with GPUI and libmpv, now adding native torrent downloads and file sharing. Start with the [implementation plan](PLAN.md), [playback and sharing design](docs/SHARING_PLAN.md), and [validation record](docs/VALIDATION.md).

[View the empty window](docs/screenshots/flick-empty.png) or [playback with subtitles](docs/screenshots/flick-player.png).

Flick draws its interface in a macOS, Windows 11, or GNOME style and matches the running system by default. Each style has its own window chrome, sidebar, controls, typography, and player: a floating HUD on macOS, a transport bar on Windows, and a rounded OSD on GNOME. Choose the style, a light or dark scheme, and an accent color in **Appearance**. Icons come from the free Hugeicons set. The native style uses the platform UI font; other styles use the closest installed font or the bundled Inter. See [asset sources and licenses](assets/README.md).

Flick opens to your library, with Continue watching, All videos, History, and Downloads. Add videos or a folder, filter local titles, or search online for videos to download. Choose torrent files and a destination; completed verified videos join the library. Share a file or folder and copy its magnet link. The [development diagnostic](docs/TRANSFERS.md) uses the same native transfer backend. Watching during downloads follows later. The broad [headless agent proposal](docs/HEADLESS_PLAN.md) is deferred. See [PLAN.md](PLAN.md) for implemented versus planned work.

This first prototype provides video inside the application window, play and pause, seeking, volume and mute, playback speed, fullscreen, audio/subtitle selection with audio delay, external subtitle loading, recent files, and resume positions. Playback controls appear after media loads. libmpv uses the platform's hardware decoder when available (`hwdec=auto-copy`; set `FLICK_HWDEC=no` to compare against software decoding) and renders into the frame GPUI uploads. The software frame-transfer path follows the display's physical video bounds with no fixed resolution or pixel-budget cap. Native dimension/allocator failures are reported instead of silently lowering resolution. Efficient native GPU video presentation is a separate milestone; this version makes no claim of being faster than VLC.

## Run locally

Install [Rust through rustup](https://rust-lang.org/tools/install/), which selects the repository's pinned 1.99.0 toolchain, and the platform's desktop build prerequisites described in [validation](docs/VALIDATION.md#native-build-prerequisites). Install a compatible libmpv shared library for playback:

- Ubuntu 24.04: `sudo apt-get install libmpv2`.
- macOS: `brew install mpv`; full Xcode is required to build GPUI's Metal shaders.
- Windows: install an mpv shared-library build matching the app's architecture. Put its DLL and dependencies beside `flick.exe`, or set `FLICK_MPV_LIBRARY` to the DLL's absolute path.

From the repository root:

```sh
cargo run --locked
cargo run --locked -- "/path/to/video.mp4"
```

If Rust was just installed and `cargo` is not on the current shell's path, open a new terminal or run `. "$HOME/.cargo/env"` on macOS/Linux first. On this development machine, `~/.cargo/bin/cargo` also works directly.

A new library opens to a welcome page with **Open Video…**, **Add Folder…**, and **Paste Magnet Link**. Afterward, Flick opens to **Home**, with Continue Watching and Recently Added; the sidebar also has All Videos, History, Downloads, and Appearance. Use **Add Videos…** or **Add Folder…** to build the library, or drop files/folders into it. Click a video to play or continue it. The back arrow returns to the library and pauses playback. Passing a local path on the command line opens that video directly. Type in the search field to filter local videos; press Enter for download results, or paste a magnet link. Search uses TPB, Internet Archive, and YTS independently and reports unavailable sources. The Linux file picker uses the desktop portal; drag and drop and command-line paths remain available when no portal service is running. The captions icon opens **Audio & subtitles** to select tracks or load a subtitle file. While a video plays, its controls hide after 2.5 seconds without pointer movement, and subtitles move above the controls whenever they show. Turn hiding off in **Appearance**. Icon controls have descriptive tooltips.

## Keyboard controls

Player keys can be changed on the **Keyboard Shortcuts** page, which also lists them. Press `?` to show them in the player or open the page from the library.

| Key | Action |
| --- | --- |
| Space | Play or pause |
| Left / Right | Seek backward or forward 10 seconds |
| Up / Down | Raise or lower volume |
| M | Mute or unmute |
| F | Toggle fullscreen |
| , / . | Previous or next frame |
| J / K / L | Slower or back 10 seconds / pause at normal speed / play, then faster |
| 0–9 | Jump to 0–90% of the video |
| Page Up / Page Down | Previous or next chapter |
| B | Set the A–B loop start, then its end, then clear it |
| [ / ] | Move audio 50 ms earlier or later |
| Escape | Exit fullscreen or close a menu |
| Ctrl+O / Cmd+O | Add videos in the library or open a video in the player |
| Ctrl+K / Cmd+K | Go to the library and focus search |
| Ctrl+, / Cmd+, | Open Appearance |
| Ctrl+L / Cmd+L | Load a subtitle file |
| ? | Show keyboard shortcuts |

Tab and Shift+Tab move between controls, with a focus ring and the control's tooltip. Enter or Space activates the focused control. Arrow keys move between video cards and list rows, and Shift+F10 or the Menu key opens a video's menu: Play, Show in Folder, Mark as Watched or Unwatched, and Remove from Continue Watching. Right-click opens the same menu.

Click or drag the timeline to seek or the volume slider to adjust sound. Double-click the video to toggle fullscreen. Chapters and an A–B loop are marked on the timeline.

**Appearance** also sets text size, contrast, button labels, and subtitle size, color, background, and position. Auto contrast follows the system's increased-contrast setting on macOS, Windows, and GNOME. Colors meet WCAG AA contrast in every style, scheme, and accent.

## Development checks

```sh
cargo fmt --all -- --check
cargo check --locked --all-targets
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
```

Generate a synthetic clip using FFmpeg and exercise the real playback boundary:

```sh
bash scripts/make-test-video.sh /tmp/flick-test.mp4 --with-subtitles
FLICK_AUDIO=null cargo run --locked -- --smoke-test /tmp/flick-test.mp4
FLICK_AUDIO=null FLICK_TEST_VIDEO=/tmp/flick-test.mp4 \
  cargo test --locked playback::tests::real_playback -- --ignored --nocapture
```

`FLICK_AUDIO=null` is for automated tests without an audio device. Omit it for normal audible playback. The CLI smoke test does not open a desktop window. Its first-frame timing describes a software presentation submission, not a measured display scanout time.

Run a sustained release benchmark with a generated local 4K/30 fps fixture (FFmpeg with libx264 required):

```sh
FLICK_AUDIO=null bash scripts/benchmark.sh                 # 300 seconds at 3840x2160
FLICK_AUDIO=null bash scripts/benchmark.sh 60 --size 1920x1080
FLICK_AUDIO=null FLICK_HWDEC=no cargo run --locked --release --bin flick -- \
  --benchmark /path/to/local-video.mp4 --seconds 300 --size 3840x2160 --interval-ms 16
```

The script removes its temporary 12-second H.264 clip on exit. Stdout is one JSON report; diagnostics go to stderr. The CLI loops the file through the real software renderer without a GPUI window. It reports CPU, RSS, mpv drop counters, source/decoder metadata, and render-call percentiles after the first frame. Frame counts describe software submissions; GPUI upload, display timing, and audible playback are outside this measurement. See the [performance gate](docs/VALIDATION.md#performance-gate) for units, counter limitations, and recorded results.

GPUI 0.2.2 is pinned. Its transitive `xattr` dependency still uses the `ENOATTR` alias removed in libc 0.2.190, so this application temporarily pins libc 0.2.189. Revisit this compatibility pin when updating GPUI.

## Platform status

Windows, macOS, and Linux have native CI jobs configured. Only results actually executed are recorded in [validation](docs/VALIDATION.md); configuration alone does not establish support. Native installers and bundled media libraries remain release work.

## Code organization

- `src/ui.rs`: GPUI window, video image lifecycle, controls, menus, and input.
- `src/playback.rs`: dynamically loaded libmpv boundary and software presentation.
- `src/store.rs`: local preferences, recent files, and resume positions.
- `src/model.rs`: time and seek helpers.
- `src/smoke.rs`: real media playback check without a desktop window.
- `src/torrent.rs`: native transfer adapter, metadata, sharing, selected-file downloads, verification, and session persistence.
- `src/bin/flick-transfer.rs`: development transfer diagnostic without GPUI or libmpv.
- `src/presentation.rs`: physical display sizing without fixed resolution limits.
- `src/library_ui.rs`: library, history, search results, torrent selection, and Downloads views.
- `src/catalog.rs`: independent native discovery providers with progressive results.
- `src/services.rs`: desktop background worker and transfer events.
- `src/input.rs`: text fields with Unicode/IME and clipboard support, including selectable read-only values.
- `src/shortcuts.rs`: rebindable player shortcuts.
- `src/theme.rs`: interface styles, WCAG contrast adjustment, and appearance settings.

Preferences are saved to `settings.json` in the operating system's per-user application configuration directory. Media files are read in place.
