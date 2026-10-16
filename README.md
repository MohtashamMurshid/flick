# Flick

A local-file desktop video player built in Rust with GPUI and libmpv. Start with the [implementation plan](PLAN.md) and [validation record](docs/VALIDATION.md).

[View the empty window](docs/screenshots/flick-empty.png) or [playback with subtitles](docs/screenshots/flick-player.png).

The interface uses bundled Inter typography, Lucide icons, centered playback controls, and a black/white/gray theme. See [asset sources and licenses](assets/README.md).

The next milestone is [fully headless agent control](docs/HEADLESS_PLAN.md): an independent CLI and NDJSON session interface sharing the playback core, with no GPUI, display server, GPU, or audio-device requirement. It is planned work; the existing smoke-test command is a diagnostic.

This first prototype provides video inside the application window, play and pause, seeking, volume and mute, playback speed, fullscreen, audio/subtitle selection, external subtitle loading, recent files, and resume positions. It uses a portable software frame-transfer path capped at a 1280×720 presentation surface. Efficient native GPU video presentation is a separate milestone; this version makes no claim of being faster than VLC.

## Run locally

Install current stable [Rust](https://rust-lang.org/tools/install/) and the platform's desktop build prerequisites described in [validation](docs/VALIDATION.md#native-build-prerequisites). Install a compatible libmpv shared library for playback:

- Ubuntu 24.04: `sudo apt-get install libmpv2`.
- macOS: `brew install mpv`; full Xcode is required to build GPUI's Metal shaders.
- Windows: install an mpv shared-library build matching the app's architecture. Put its DLL and dependencies beside `flick.exe`, or set `FLICK_MPV_LIBRARY` to the DLL's absolute path.

From the repository root:

```sh
cargo run --locked
cargo run --locked -- "/path/to/video.mp4"
```

If Rust was just installed and `cargo` is not on the current shell's path, open a new terminal or run `. "$HOME/.cargo/env"` on macOS/Linux first. On this development machine, `~/.cargo/bin/cargo` also works directly.

Choose **Open**, drop a video into the window, or pass a local path on the command line. The Linux file picker uses the desktop portal; drag and drop and command-line paths remain available when no portal service is running. The captions icon opens **Audio & subtitles** to select tracks or load a subtitle file. The history icon opens **Recent files** to restore prior viewing sessions. Icon controls have descriptive tooltips.

## Keyboard controls

| Key | Action |
| --- | --- |
| Space | Play or pause |
| Left / Right | Seek backward or forward 10 seconds |
| Up / Down | Raise or lower volume |
| M | Mute or unmute |
| F | Toggle fullscreen |
| Escape | Exit fullscreen or close the menu |
| Ctrl+O / Cmd+O | Open a video |
| Ctrl+L / Cmd+L | Load a subtitle file |
| ? | Show keyboard shortcuts |

Click or drag the timeline to seek or the volume slider to adjust sound. Double-click the video to toggle fullscreen.

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

GPUI 0.2.2 is pinned. Its transitive `xattr` dependency still uses the `ENOATTR` alias removed in libc 0.2.190, so this application temporarily pins libc 0.2.189. Revisit this compatibility pin when updating GPUI.

## Platform status

Windows, macOS, and Linux have native CI jobs configured. Only results actually executed are recorded in [validation](docs/VALIDATION.md); configuration alone does not establish support. Native installers and bundled media libraries remain release work.

## Code organization

- `src/ui.rs`: GPUI window, video image lifecycle, controls, menus, and input.
- `src/playback.rs`: dynamically loaded libmpv boundary and software presentation.
- `src/store.rs`: local preferences, recent files, and resume positions.
- `src/model.rs`: time and seek helpers.
- `src/smoke.rs`: real media playback check without a desktop window.

Preferences are saved to `settings.json` in the operating system's per-user application configuration directory. Media files are read in place.
