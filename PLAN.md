# Flick MVP implementation plan

Build an independent Rust desktop video player with a polished GPUI interface. Target Windows, macOS, and Linux from the first implementation. The first release plays local files reliably; performance improvements must be demonstrated with measurements rather than inferred from Rust or GPU rendering.

## Product scope

The MVP opens local videos through a file picker, drag and drop, or a command-line path. It provides play and pause, seeking, volume and mute, fullscreen, playback speed, audio and subtitle track selection, external SRT loading, recent files, and resume positions. Start the supported test matrix with SDR H.264 video and AAC audio in MP4 and MKV containers.

Use one quiet black, white, and gray window: bundled Inter typography, Lucide icon controls with tooltips, a centered empty state and transport, a large video area, and a readable timeline. Follow shadcn-style spacing, borders, and hierarchy. Keep errors actionable. Settings and history stay local.

Streaming, casting, disc menus, accounts, AI features, a full media library, thumbnail scrubbing, and advanced HDR behavior are later work.

## Proposed implementation

- Rust and Cargo for application code and builds.
- GPUI for windowing, layout, input, controls, and presentation. Pin dependencies and commit the application lockfile.
- libmpv through a small Rust boundary for media playback, timing, audio, subtitle rendering, and seeking. This is independent of VLC and reuses existing native media libraries.
- A presentation adapter between playback and GPUI. First establish a portable working path, then validate native GPU integration on each OS. A CPU frame-transfer fallback is acceptable for the first functional prototype, but does not satisfy the GPU performance milestone.
- Serde and small local files for preferences and recent playback positions. Use each OS's application data directory.
- Structured diagnostics and timings for playback problems and performance work.
- Native CI jobs for Windows, macOS, and Linux. A successful build is distinct from an interactive playback or hardware-decoding test.

GPUI's current external video surface support differs across platforms. Investigate this before promising a common GPU texture path. Keep playback and presentation code separate so improving the frame path does not require rewriting the interface.

## Implementation milestones

### 1 Establish the project and playback boundary

- [x] Create the Cargo project, source modules, and local development instructions.
- [x] Load libmpv and report missing native dependencies clearly.
- [x] Open a generated local test clip with video and advancing audio timing using a null audio device. Audible synchronization remains a physical-device check.
- [x] Establish commands and events for playback state, errors, seeking, and track selection.
- [x] Add focused tests for state transitions and the playback boundary.

Acceptance: a reproducible local playback test and a clear record of which operating systems were actually exercised.

### 2 Build the usable GPUI player

- [x] Add the empty state, file picker, drag and drop, and command-line opening. The picker and drop handlers are wired; successful portal selection and native drag and drop still need desktop acceptance checks.
- [x] Display video inside the app with correctly scaled letterboxing.
- [x] Add the timeline, keyboard controls, volume, speed, and fullscreen.
- [x] Add audio and subtitle selection and external subtitle loading.
- [x] Persist recent files, preferences, and useful resume positions.
- [x] Handle end of file, replacement files, decode errors, and shutdown safely.
- [x] Refine the interface with neutral colors, bundled typography, SVG controls, and balanced layout at normal and minimum window sizes.

Acceptance: the primary local-file viewing flow works end to end without relying on a separate player window.

### 3 Verify all platforms and measure performance

- [x] Run format, lint, and relevant test checks on Linux.
- [x] Add native build checks and documented installation steps for each OS.
- [ ] Exercise playback, resize, fullscreen, seeking, track switching, and pause on physical or virtual desktop sessions for each OS.
- [ ] Verify GPU presentation and hardware decoding per platform; record any CPU fallback.
- [ ] Compare cold and warm opening, first displayed frame, seek recovery, dropped frames, memory, and paused CPU usage with VLC and mpv on the same hardware and output settings.
- [ ] Prepare distributable packages after native dependencies and their distribution terms are settled.

Acceptance: publish the validation matrix and actual measurements. Do not label the app faster than VLC until comparative results support that claim.

### 4 Add a fully headless agent frontend

The next implementation milestone is [Flick headless agent implementation plan](docs/HEADLESS_PLAN.md). Preserve the desktop player and share the playback core with a separate headless frontend that does not initialize or depend on GPUI.

- [ ] Extract shared playback commands, state, and events from the desktop view.
- [ ] Build a headless binary without windowing dependencies, a display server, GPU access, or an audio device.
- [ ] Add one-shot inspection and frame export with machine-readable results.
- [ ] Add a versioned NDJSON stdio interface for agents, including control, state inspection, tracks, subtitles, bounded waits, cancellation, and shutdown.
- [ ] Validate real headless sessions and frame artifacts on all three operating systems without Xvfb.

This is planned work. The current `--smoke-test` diagnostic is not a complete agent interface.

## Work allocation

The main agent owns the plan, Cargo integration, GPUI interface, final assembly, and verification. Parallel agents may own the playback module, persistence and state logic, and build or review work in separate files. Define interfaces before parallel implementation; avoid concurrent edits to Cargo metadata or the same source file.

## Initial environment and validation limits

The repository starts empty. The available development machine runs Linux and has FFmpeg and a virtual X server. Rust and libmpv are not initially on the command path. Install the required development tools, generate a synthetic test clip, and verify the Linux build and playback locally. Windows and macOS remain unverified until their native jobs or test machines run successfully.

## References

- [GPUI documentation](https://github.com/zed-industries/zed/blob/main/crates/gpui/README.md)
- [GPUI video surface implementation](https://github.com/zed-industries/zed/blob/main/crates/gpui/src/elements/surface.rs)
- [libmpv rendering interface](https://github.com/mpv-player/mpv/blob/master/include/mpv/render.h)
- [libmpv client interface](https://github.com/mpv-player/mpv/blob/master/include/mpv/client.h)

## Progress

- Plan created before implementation; first working prototype implemented on 2026-10-16.
- Linux build, formatting, and strict Clippy pass. Nine unit tests and three opt-in real-libmpv integration tests pass.
- Desktop checks under Xvfb/Openbox verify MP4 and MKV playback, keyboard and mouse seeking, paused seek image updates, volume and mute, playback speed, track selection, subtitles, fullscreen, resizing, recents, resume across reopening, and clean window close.
- The monochrome UI revision uses Inter and Lucide assets, three equal columns for centered playback and title alignment, measured video bounds, and a draggable volume slider. The next agent-facing headless milestone is specified in [the headless implementation plan](docs/HEADLESS_PLAN.md).
- The headless session has no file-picker portal: its actionable fallback message is verified. Successful file selection, native file drops, audible output, Windows/macOS runs, signed installers, and GPU performance checks remain open.
- [Build and validation details](docs/VALIDATION.md), [empty window](docs/screenshots/flick-empty.png), and [playing video with subtitles](docs/screenshots/flick-player.png).
