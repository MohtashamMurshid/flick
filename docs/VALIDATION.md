# Validation and platform support

Flick targets Windows, macOS, and Linux. A CI configuration is not evidence of a passing native build, and a successful build is not evidence of working playback. Record actual results below as checks run.

## Current evidence

| Platform | Native compilation and tests | Desktop playback | Native GPU video path |
| --- | --- | --- | --- |
| Linux | Native debug build, formatting, strict Clippy, 9 unit tests and 3 real-libmpv tests pass | Xvfb/Openbox session exercised; null audio | Deferred |
| Windows | Workflow configured; not run here | Pending native machine | Deferred |
| macOS | Workflow configured; not run here | Pending native machine | Deferred |

The initial presentation adapter transfers decoded BGRA frames through CPU memory into GPUI images. GPU rendering of those images does not establish hardware video decoding or zero-copy presentation. Treat this as a functional prototype. Hardware decoding, efficient GPU surface sharing, HDR correctness, battery use, and claims of being faster than VLC remain unverified.

### Linux verification on 2026-10-16

Environment: Ubuntu 24.04 x86_64, Rust 1.99.1, GPUI 0.2.2, libmpv 0.37.0 (`libmpv.so.2`), Xvfb with Openbox, and Mesa's Lavapipe Vulkan driver. Audio output used libmpv's timed null device; these results do not verify speakers, headphones, or audible synchronization.

Passed checks:

- `cargo build --locked`, `cargo fmt --all -- --check`, and `cargo clippy --locked --all-targets -- -D warnings`.
- `cargo test --locked --all-targets`: 9 passed, 3 native tests deliberately ignored by default.
- `FLICK_AUDIO=null FLICK_TEST_VIDEO=/tmp/flick-platform-validation.mp4 cargo test --locked playback::tests -- --ignored --nocapture`: all 3 native tests passed. They cover real pixel content, pause, seeking, volume, speed, tracks, missing-file errors, repeated file replacement and player teardown, SRT selection, EOF/replay, and paused-seek image changes.
- CLI smoke checks passed for synthetic H.264/AAC MP4 and a multi-track MKV. Seeking is verified while paused within a bounded interval, followed by new rendered frames, so normal playback cannot falsely validate a failed seek.
- Native desktop interactions confirmed CLI opening, moving and correctly colored video, play/pause, arrow seeking, mouse timeline seeking, mute and volume, speed changes, fullscreen and exit, window resizing, recents, two audio tracks and an embedded subtitle track, subtitle display, and clean close with exit code zero.
- A 36-second synthetic clip restored its saved position on reopening. Preferences also survived process restart.
- The empty state and missing-portal file-picker fallback were inspected. This session has no desktop portal, so successful picker selection and native drag and drop remain unverified.

Actual application captures: [empty state](screenshots/flick-empty.png) and [video with subtitles](screenshots/flick-player.png). These are screenshots of the running GPUI application, using locally generated media.

The monochrome UI revision was checked in the same native Linux environment at 1100×740, the 760×500 minimum size, and 1280×900 fullscreen. Bundled Inter and Lucide SVG assets render correctly. The title and transport remain centered, controls fit at the minimum size, and video sizing uses the measured content bounds. Clickable volume, paused timeline seeking and its picture update, recent-file opening, subtitle selection/display, and fullscreen entry/exit were exercised. Both revised desktop sessions closed with exit code zero. The revised build, formatting, strict Clippy, and nine unit tests pass; the three opt-in native tests were not rerun for the visual changes. Captures use a grayscale version of the generated fixture to show the neutral interface; normal media keeps its original colors.

Two integration fixes are covered by this validation: native playback notifications are correlated with the requested playlist entry during rapid replacement, and render callback notifications survive libmpv's presentation timeout so paused seeking updates the picture. The native library mapping remains resident for the process lifetime while individual renderers and playback cores are released. GPUI's X11 close callback defers application quit to the next event-loop turn to avoid reentering its platform borrow.

Release builds, Windows/macOS execution, audible output, successful portal selection, native drops, sustained memory/power measurements, and hardware video acceleration remain pending. An upstream `proc-macro-error2` dependency emits a future Rust compatibility notice; the current stable build and application lint checks pass.

## Native build prerequisites

Use current stable Rust with `rustfmt` and `clippy`. The application pins GPUI 0.2.2 and commits `Cargo.lock`; use `--locked` for repeatable dependency resolution. libmpv is loaded at runtime, so ordinary compilation and unit tests do not require libmpv headers or a linker import library.

### Linux

On Ubuntu/Debian, install the desktop build dependencies:

```sh
sudo apt-get update
sudo apt-get install --no-install-recommends -y \
  build-essential pkg-config clang cmake \
  libfontconfig-dev libwayland-dev libx11-xcb-dev \
  libxkbcommon-x11-dev libssl-dev libvulkan1
```

This enables GPUI's default X11 and Wayland backends. A desktop session, usable Vulkan driver, fonts, an audio device, and a compatible libmpv shared library are additional runtime requirements. On Ubuntu 24.04, `sudo apt-get install libmpv2 ffmpeg` supplies the media runtime and fixture generator; package names may differ on other distributions. The full Zed development package list also includes editor dependencies that this player does not need.

Xvfb can provide a display for a smoke test, but does not by itself supply GPU support. A virtual machine may require Mesa's software Vulkan driver (`mesa-vulkan-drivers`) and installed fonts (`fonts-dejavu-core`). A software-rendered smoke test cannot validate real GPU performance or audible output.

### macOS

Install full Xcode and its macOS components; command line tools alone may omit the Metal compiler. Select the installation and verify the tools:

```sh
sudo xcode-select --switch /Applications/Xcode.app/Contents/Developer
xcrun --find metal
xcrun --find metallib
```

GPUI 0.2.2 builds Metal shaders and uses libclang to generate platform bindings. GitHub's macOS runners supply Xcode; the workflow checks the selected installation. Install a libmpv build matching the machine's architecture for playback. FFmpeg is needed only to generate fixtures. A real desktop session is needed to exercise the player.

### Windows

Use stable Rust's MSVC target, Visual Studio C++ Build Tools, and the Windows SDK. GPUI 0.2.2 uses the SDK's **FXC** shader compiler for release builds, rather than DXC. If it is not discoverable, set `GPUI_FXC_PATH` to the full path of the SDK's `x64\fxc.exe`. The workflow locates an installed SDK explicitly; the dependency's built-in fallback hardcodes one SDK version.

Playback additionally needs a compatible x64/ARM64 libmpv DLL and its dependencies that match the application's architecture. Set `FLICK_MPV_LIBRARY` to its absolute path if the library is outside the platform's normal search paths; the same override works for `.so` and `.dylib` files. Build jobs do not download or distribute third-party DLL bundles. Verify their source, version, and distribution terms before packaging Flick.

## Automated checks

Run from the repository root:

```sh
cargo fmt --all -- --check
cargo check --locked --all-targets
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
cargo build --locked --release
```

The native workflow runs these checks separately on `ubuntu-latest`, `windows-latest`, and `macos-latest`. Tests run without launching the graphical application. The ignored libmpv integration test requires the media runtime and is not part of the default test pass. After generating the fixture below, run it on Linux/macOS with:

```sh
FLICK_AUDIO=null FLICK_TEST_VIDEO=/tmp/flick-test.mp4 \
  cargo test --locked playback::tests::real_playback -- --ignored --nocapture
```

Run all native regressions, including lifecycle/subtitle/EOF and paused seek image-content checks, by replacing the test filter with `playback::tests`.

The null audio device allows playback timing to progress without sound hardware. This test checks actual nonblack frames, playback progress, seeking, and media commands; it does not open a GPUI window or verify audible output.

## Reproducible local media

With FFmpeg's `libx264` and AAC encoders available:

```sh
bash scripts/make-test-video.sh /tmp/flick-test.mp4 --with-subtitles
cargo run --locked -- /tmp/flick-test.mp4
```

On macOS/Linux the script generates 12 seconds of 1280×720, 30 fps SDR BT.709 H.264 video, a quiet 440 Hz AAC audio track, and optional external SRT captions. It uses synthetic FFmpeg sources and refuses to overwrite existing output files. Generated media belongs outside the repository. Windows developers can generate the fixture in Git Bash or copy their own locally generated fixture from another machine.

Confirm the file's streams with:

```sh
ffprobe -v error -show_entries stream=codec_name,width,height,pix_fmt,color_transfer,color_primaries -of json /tmp/flick-test.mp4
```

To test MKV without acquiring another clip, remux the generated streams:

```sh
ffmpeg -nostdin -n -i /tmp/flick-test.mp4 -map 0 -c copy /tmp/flick-test.mkv
```

The basic fixture contains one audio track and no embedded subtitles. Create separate multi-track MP4/MKV fixtures before claiming track-switching coverage. Also test missing, malformed, unsupported, Unicode-named, and space-containing paths.

## Desktop acceptance pass

Perform this pass on each OS and record the OS version, architecture, GPUI version, libmpv version, GPU/driver, display backend, clip details, and outcome:

1. Open a clip through the file picker, command-line path, and drag and drop.
2. Confirm a moving, correctly colored picture and audible, synchronized audio. Resize repeatedly and verify letterboxing.
3. Pause and resume, seek forward and backward, change volume and mute, and change speed. Confirm controls and keyboard shortcuts match.
4. Toggle fullscreen and return to the normal window. Test the close button and clean process shutdown.
5. Load the generated SRT and verify captions. Use separate multi-track fixtures to change audio/subtitle tracks and disable captions.
6. Open another clip while one is playing. Try a missing file and then recover by opening a valid file.
7. Close and reopen the player. Confirm recents and an eligible resume position persist. Verify clips near their end do not reopen at a useless position.
8. Pause and measure resource use; play and resize for several minutes while watching memory. Retired video images must leave GPUI's sprite atlas.

For a headless Linux smoke test, record exactly which interactions and frame observations were automated. It cannot substitute for native Windows/macOS tests or for visual/audio assessment on physical hardware.

## Performance gate

Compare release builds of Flick, VLC, and mpv on the same machine, file, display resolution, output scaling, and decoder settings. Record cold and warm runs separately. Measure launch to first displayed frame, file open to first displayed frame, seek to stable playback, dropped frames, process memory, CPU use, and paused CPU use. Measure power consumption when supported by the machine. Include multiple runs and the range of results.

Do not report FPS from a screen refresh callback as decoded FPS. Do not compare a CPU fallback on one player against hardware decoding on another without identifying the difference. No performance results have been recorded yet.

## Upstream references

- [Published GPUI 0.2.2 package and feature dependencies](https://docs.rs/crate/gpui/0.2.2/source/Cargo.toml)
- [Published GPUI 0.2.2 native shader build requirements](https://docs.rs/crate/gpui/0.2.2/source/build.rs)
- [Published GPUI 0.2.2 macOS setup instructions](https://docs.rs/crate/gpui/0.2.2/source/README.md)
- [Zed Linux dependency installer at the package's recorded revision](https://github.com/zed-industries/zed/blob/69e2130295c2649963eb639fc70b4f2ee8ea1624/script/linux)
- [GPUI 0.2.2 BGRA image contract](https://docs.rs/crate/gpui/0.2.2/source/src/assets.rs)
- [libmpv rendering API](https://github.com/mpv-player/mpv/blob/master/include/mpv/render.h)
