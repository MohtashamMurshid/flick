# Validation and platform support

Flick targets Windows, macOS, and Linux. A CI configuration is not evidence of a passing native build, and a successful build is not evidence of working playback. Record actual results below as checks run.

## Current evidence

| Platform | Native compilation and tests | Desktop playback | Native GPU video path |
| --- | --- | --- | --- |
| Linux | Native debug build, formatting, strict Clippy, and 26 ordinary tests pass on 2026-10-24; 3 real-libmpv tests passed earlier | Xvfb/Openbox session exercised in all three interface styles; null audio | Deferred |
| Windows | Workflow configured; not run here | Pending native machine | Deferred |
| macOS | Debug compilation, formatting, strict Clippy, 23 tests, and an opt-in native resolution test pass on 2026-10-25 | Library/search/resume and public Sintel playback observed; CoreAudio output active | Deferred |

The presentation adapter transfers decoded BGRA frames through CPU memory into GPUI images. It sizes frames to physical video bounds without a fixed resolution, axis, or pixel-budget cap. Checked native dimensions and allocation failures produce explicit errors. GPU rendering of those images does not establish hardware video decoding or zero-copy presentation. Treat this as a functional prototype. Hardware decoding, efficient GPU surface sharing, HDR correctness, battery use, and claims of being faster than VLC remain unverified.

### Link downloads on 2026-10-24

Backlog items 26 and 31 add downloads from pasted web links. `src/fetch.rs` has no GPUI dependency and is separate from the torrent session. It runs yt-dlp as a separate process for video pages, downloads direct HTTPS files, and lists Internet Archive items from archive.org's metadata API. `src/services.rs` runs these jobs on the existing background runtime without starting the torrent session, and `src/fetch_ui.rs` shows them on the Downloads page. Internet Archive search results now download over HTTPS instead of through the Archive's torrent. librqbit 9.0.1 is still the newest crates.io release, and its metainfo parser has no web seed (`url-list`) support.

Environment: Ubuntu 24.04.5 x86_64, Rust 1.99.0, system yt-dlp 2024.04.09 at `/usr/bin/yt-dlp`, FFmpeg 6.1.1, libmpv 0.37.0, and Xvfb with Openbox and Mesa Lavapipe, using null audio.

Automated checks, all passing:

- `cargo fmt --all -- --check`, `cargo clippy --locked --all-targets -- -D warnings`, and `cargo clippy --locked --no-default-features --all-targets -- -D warnings`. The upstream `proc-macro-error2` future-compatibility notice remains.
- `cargo test --locked --all-targets` passes 55 tests: 34 library, 18 desktop, and 3 transfer integration tests. Five tests are ignored by default: the four libmpv tests and one opt-in yt-dlp test. `cargo test --locked --no-default-features --all-targets` passes 37, with the same yt-dlp test ignored.
- The `--no-default-features` normal dependency tree still contains no GPUI, Blade graphics, Cocoa, Metal, or winit.
- The 28 new tests use no Internet access. A generated `/bin/sh` stand-in for yt-dlp reports progress, a title, and a saved file; fails with exit code 1; exits without saving; reports a file outside the destination; or sleeps after starting a background child. The tests check the argument list, with `--` immediately before the URL, and the removal of partial files after failure. Cancellation must stop the stand-in's whole process group: a mutation that disabled the process group made the test fail. Further tests cover a missing program, discovery through `FLICK_YTDLP`, `PATH`, and extra folders, progress and print parsing, rejected schemes and credentials, and safe file names. Plain-HTTP loopback servers cover resuming after a dropped connection with `Range` and `If-Range`, resuming or restarting an existing part file, checksum failures, never overwriting an unrelated file, reusing an identical copy, cancellation, HTTP refusals, refusing a concurrent download of the same file, and HTML answers. A page at a video-like address falls back to yt-dlp. Archive tests parse a metadata fixture and select files from it. The services test checks that link downloads run and cancel while the torrent session cannot start. The stand-in tests are Unix-only.
- `cargo test --locked --no-default-features --lib installed_ytdlp -- --ignored` passes with the system yt-dlp and FFmpeg, using only a loopback server. A throttled download was cancelled, which left no files or temporary folder, and a second download saved byte-identical content.

The HTTPS client itself, with TLS and `https_only`, is covered only by the manual checks below. No automated test uses a TLS server.

Manual Internet checks, run with the `flick-fetch` diagnostic into temporary folders:

- **Internet Archive over HTTPS.** [`Nabisco_2`](https://archive.org/details/Nabisco_2) is a public-domain Prelinger item. Its listing preselected the two originals: an MPEG-2 file and a HiRes MPEG-4. The three archive.org derivatives were listed but not selected. The first `get` was killed externally after saving and verifying the first file, leaving an empty part file for the second. The cause was not confirmed; another session's process cleanup is the likely explanation. Running `get` again reused the verified first file without downloading it, completed the second, and removed the part file. Both SHA-1s matched archive.org's metadata (`d723d45d…` and `58a234df…`), and FFmpeg decoded both as about 10.5 seconds of video with audio.
- **YouTube.** With the system yt-dlp 2024.04.09, YouTube's API answered `HTTP Error 400: Bad Request` ("Precondition check failed"). yt-dlp then reported `[youtube] aqz-KE-bpKQ: Requested format is not available`. Flick showed that error unchanged, and the folder stayed empty. The official yt-dlp 2026.10.21 standalone release was then downloaded into a new temporary folder and selected with `FLICK_YTDLP`. With it, the Blender Foundation's CC BY [Big Buck Bunny 60fps 4K](https://www.youtube.com/watch?v=aqz-KE-bpKQ) downloaded and merged into one 722,512,905-byte WebM: AV1 3840×2160 with Opus audio, 634.6 seconds. A second run was interrupted with SIGINT at 184 MB of 712 MB. It reported `Cancelled`, left the folder empty, and left no yt-dlp or FFmpeg process. YouTube did not block this host.
- **Wikimedia Commons.** The public-domain NASA clip [`NASA Probe Sparrow Motion.webm`](https://commons.wikimedia.org/wiki/File:NASA_Probe_Sparrow_Motion.webm) exposed a bug. Its page address ends in `.webm`, so the first build saved the 97 KB HTML page as a video. Direct downloads now refuse HTML answers, and a pasted link then falls back to yt-dlp. After the fix, the system yt-dlp received `HTTP Error 403: Forbidden` from Wikimedia, and Flick showed that error. yt-dlp 2026.10.21 downloaded the clip: VP9 1280×720, 4.2 seconds.
- **X** was not tried, because no suitable public-domain post was at hand.

Interactive desktop checks, using the debug build under a separate Xvfb display with temporary settings and download folders. Video-page downloads came from a loopback HTTP server that served generated 12-second clips, with one path throttled. The Archive check used the public Internet, and the search also queried TPB and YTS. No torrent content was downloaded. The following were observed:

- The welcome page shows **Paste Link**. The GNOME Downloads header shows the same action as an icon; its label was not inspected.
- Typing a loopback page URL in search and pressing Enter cleared the field and opened Downloads. The **Link Downloads** row showed yt-dlp's title, a progress bar, the size, speed, and time remaining, and the sidebar badge counted the download.
- The cancel button's tooltip read "Cancel download". Cancelling removed the row and the badge. It left nothing in the download folder, including the hidden temporary folder, and no yt-dlp process remained.
- With the address copied to the clipboard from Flick's own search field, **Paste Link** downloaded the clip. Play opened it in the player. About three seconds later, the test pattern's timestamp read 2.33 seconds. The video then appeared under Recently Added.
- A 404 page showed yt-dlp's error in the row. After the file was added to the loopback server, **Try Again** downloaded it. With `FLICK_YTDLP` set to a missing path, the row showed "FLICK_YTDLP is set to …, which is not a file."
- Searching "Nabisco Bumper" returned the Internet Archive item, labelled "Downloads over HTTPS · choose files first". Download opened a file chooser with formats, sizes, the original-file defaults, and the destination. Changing the selection updated the total. Download Selected showed "File 1 of 2", saved both files into a `Nabisco Bumper` folder with matching SHA-1s, and added them to the library. Play showed the film.
- Entering `https://archive.org/details/Nabisco_2` in search and choosing Download Selected downloaded the default originals. The MPEG-2 file was fetched. The identical HiRes MPEG-4 already on disk was reused instead of being copied under a numbered name. The row named the `Nabisco Bumper` folder.
- Closing the window during a throttled yt-dlp download exited Flick within half a second. The hidden temporary folder was removed, and no yt-dlp process remained.

Two problems found during this pass were fixed and rechecked in the GUI. The retry icon showed a "10" seek glyph and is now a **Try Again** button. A multi-file row named the parent folder instead of the item folder.

[Link download in progress](screenshots/flick-gnome-light-link-download.png) and [the Internet Archive file chooser](screenshots/flick-gnome-light-archive-files.png).

Not verified:

- **macOS and Windows.** In particular, on Windows the yt-dlp console window is suppressed with `CREATE_NO_WINDOW`, and cancellation ends the process tree with `taskkill /T`. Neither was compiled or run in this session; Windows CI has not run them.
- **Downloads on other sites.** X downloads, playlist addresses, and sites that require signing in were not tried.
- **Edge cases.** A full disk and resuming an HTTPS download after a real crash, beyond the one external interruption above, are untested.

All downloaded media, generated clips, and the downloaded yt-dlp release were deleted after these checks.

### Native interface styles on 2026-10-24

Flick now draws macOS, Windows 11, and GNOME styles (`src/theme.rs`, `src/chrome_ui.rs`), with an Appearance page, Hugeicons assets, a first-run welcome page, card-based Home and All Videos pages, and style-specific auto-hiding player controls.

On Linux, these checks pass: formatting, strict desktop Clippy, all 26 ordinary desktop tests (including three new appearance tests), strict transfer-only Clippy, and all ten tests with `--no-default-features`. The four opt-in libmpv tests were not rerun.

The debug build was run under Xvfb with Openbox and Mesa Lavapipe, using null audio and a generated 12-second fixture with subtitles. Settings lived in a temporary `HOME`. Each style was captured in dark mode on the first-run, Home, Appearance, and player screens, plus Home in light mode. These interactions were observed:

- A style tile on the Appearance page restyled the window immediately and was saved to `settings.json`.
- The Downloads sidebar item opened the empty Downloads page.
- `?` opened the shortcuts panel.
- Playback controls hid after 2.5 seconds without pointer movement.
- Subtitles moved above visible controls and returned to the bottom when the controls hid.

[Home in the macOS style](screenshots/flick-mac-dark-home.png), [Windows 11](screenshots/flick-windows-dark-home.png), and [GNOME](screenshots/flick-gnome-dark-home.png). Also: [playback in the Windows style](screenshots/flick-windows-dark-player.png), [Appearance in the GNOME style](screenshots/flick-gnome-dark-appearance.png), and [the macOS-style welcome page](screenshots/flick-mac-dark-first.png). These captures crop the Openbox frame. Openbox provided server-side decorations, so Flick did not draw its own window controls.

Not verified:

- **Linux client-side decorations.** A test-only helper advertised a compositor and `_GTK_FRAME_EXTENTS`, but GPUI presented only black frames without a real compositor. Flick's drawn window controls, shadow, rounded frame, and resize edges remain unchecked on GNOME/Wayland.
- **macOS and Windows.** Traffic-light alignment, Windows caption buttons and Snap Layouts, and native platform fonts were not checked on those systems.
- **Transfers.** The share → download → Play GUI flow was not re-exercised for this change.

### Pull request review follow-up on 2026-10-25

The review fixes make interrupted inspect/download commands fail, give each movie-search host a four-second attempt budget, enforce HTTPS through redirects using reqwest's `https_only` mode, and share canonical video-file validation between picker/drop imports. Imports now use batch deduplication; history migration reuses its existing set instead of scanning the whole library for each entry.

After these changes, formatting, all-target checking, strict desktop/backend Clippy, all 23 ordinary tests, and all ten transfer-only tests pass. New regressions use a stalled loopback HTTP connection followed by a working fallback, aliased/missing/unsupported import paths, and batch import/history ordering with retained titles. Separate real CLI processes were interrupted against a stalled loopback TLS endpoint: inspect and download each exited with code 1 and no success JSON. A generated local share emitted its magnet and stopped with code 0 on Ctrl+C. No public discovery or public peers were used for these regressions.

### Pull request verification on 2026-10-25

Before opening the library/downloads/sharing PR, the final macOS checkout passed formatting, `cargo check --locked --all-targets`, strict desktop Clippy, all 20 ordinary tests, strict transfer-only Clippy, and all eight tests with `--no-default-features`. The transfer-only normal dependency graph contains no GPUI, Blade graphics, Cocoa, Metal, or winit. The four opt-in native libmpv tests remain excluded from the ordinary pass; the executed resolution and interactive checks are recorded separately below.

Remote build/test evidence is published by the [Native builds workflow](https://github.com/MohtashamMurshid/flick/actions/workflows/ci.yml). Each pull request runs both the full desktop checks/release build and the transfer-only checks on Windows, macOS, and Linux. A remote build result does not establish interactive playback on that platform.

### Public torrent and audio check on 2026-10-25

The user explicitly requested an Internet download and audio test after the local fixture pass. This manual acceptance check uses public discovery; the automated transfer regressions remain isolated and local as required by the repository instructions.

Downloaded [WebTorrent's public Sintel torrent](https://webtorrent.io/torrents/sintel.torrent) using Flick's transfer diagnostic with `--public`, a fresh state directory, and no explicit/local peers. File index 5, `Sintel.mp4`, completed in about 29 seconds. The final snapshot reported 129,241,752 of 129,241,752 bytes verified, state `seeding`, and 27 peers. The diagnostic then shut down its session. This is evidence of a successful public-swarm transfer, rather than a download from an agent-supplied local seed or an HTTPS copy of the video.

The completed file is 888.064 seconds of 1024×436 H.264 video with 48 kHz, six-channel AAC audio. A five-second audio decode at 60 seconds produced non-silent samples, with overall RMS about -25.86 dBFS. The unmodified film, including credits, remains in the ignored test directory. Sintel is a Blender Foundation film distributed as the public sample linked in the [WebTorrent documentation](https://webtorrent.io/docs).

Opened that completed copy in the current native player as `Flick Public Demo.app`, using normal system audio, unmuted playback, volume 45%, and speed 1.0. The actual fullscreen window showed the film advancing at 2:25. A read-only CoreAudio diagnostic for that player's PID reported `running_output: true` and `running_input: false`. The default device was MacBook Air Speakers. Its system mute was initially on; the Sound settings checkbox was switched off through the native UI for the requested audio test. A follow-up query confirmed the device was unmuted and reported system volume 100%.

The film was left playing for the user. This verifies downloaded content, native presentation, audio decoding, and active output to an unmuted physical device. It does not claim a human listening assessment or measured A/V synchronization. Downloading used the transfer diagnostic, so acceptance of every GUI download action remains separate. No application code changed in this pass.

### Downloaded video demo on 2026-10-25

At the user's request, a generated 12-second 1280×720 H.264/AAC clip was shared and downloaded between independent Flick transfer clients. Both used task-owned state, loopback TCP, explicit peers, and no public discovery. The receiver resolved the magnet, downloaded file index 0, and reported all 5,150,150 bytes verified with state `seeding`. Source and downloaded copies had identical SHA-256 `cb8dbdfed5034ebb557d1bf00786d1e50263dbfebfa83a4fc9662d686e87a331`.

The downloaded copy then opened in a separate native `Flick Demo.app` using the current player build. The actual fullscreen window displayed the moving pattern at 5.5 seconds. Escape returned to windowed mode; Left sought to 2 seconds and the paused image updated. The demo was left open for the user, and the local seed was stopped after the verified transfer. The user's existing download and main preview were left alone. Normal audio output was configured, but audible synchronization was not assessed.

This is a CLI transfer-adapter round trip followed by interactive native playback. It does not establish a complete transfer through all GUI controls or public-swarm connectivity. No implementation changes or additional automated test pass were needed for this check.

### macOS text-layout crash fix on 2026-10-25

Both direct launches and the first preview bundle aborted in GPUI's macOS `MacTextSystemState::layout_line`, at a UTF-8 slice boundary. A bundle opening once did not establish stability. The stack matches the published GPUI 0.2.2 [Unicode ellipsis crash report](https://github.com/zed-industries/zed/issues/49860). Its text layout retains mutable truncation runs across measurement passes, so a later pass can use an earlier byte length against the original text.

All seven Flick uses of `text_ellipsis` now clip single-line labels without rewriting their text runs. This covers library/history titles, online results, download names, metadata filenames/destinations, and the player title. Long names stay intact in stored data. Escape also exits fullscreen from Library and while search has focus.

Executed checks after the change:

- `cargo build --locked --bin flick`, `cargo fmt --all -- --check`, `cargo clippy --locked --all-targets -- -D warnings`, `git diff --check`, and all 20 ordinary tests pass. The existing four opt-in libmpv tests were not rerun for this UI change.
- A native QA bundle rendered six long labels mixing ASCII, Armenian, Chinese, Japanese, and emoji. Clipping remained on one line. Live `Big Buck Bunny` search displayed 48 results and an explicit YTS outage. A long Unicode search value and fullscreen/windowed transitions did not abort. Escape visibly cleared search and returned the window from fullscreen. Closing the QA window exited its process with code zero.
- A direct launch of the fixed binary remained running through the previous startup failure, then was stopped deliberately. The updated normal `Flick Preview.app` used existing settings and normal audio configuration, passed a 60-second process-liveness check, and remained open for the user. Its Library and fullscreen transitions were observed. No new crash report appeared during these checks.
- The older local `Flick.app` development bundle was updated as well, so it no longer contains the crashing build.

These checks supersede the unresolved direct-launch status recorded in the earlier preview-launch paragraph below. They do not establish sustained playback performance or the full GUI transfer acceptance pass.

### Library, discovery, and uncapped presentation on 2026-10-25

This supersedes the earlier presentation-budget and planned-desktop-control status below. The environment is the same native Apple Silicon macOS setup used for the foundations pass.

Executed checks:

- `cargo check --locked --all-targets`, `cargo fmt --all -- --check`, and `cargo clippy --locked --all-targets -- -D warnings` pass. Upstream future-compatibility notices remain for `block` and `proc-macro-error2`.
- `cargo test --locked --all-targets` passes 20 tests. Five library tests cover physical sizing, discovery response validation, safe Archive URLs, and folder import; twelve desktop tests cover model/store behavior, legacy history migration, unwatched videos, Unicode/IME offsets, and background transfer completion/shutdown; three local peer integration tests cover transfers. Four native libmpv tests are ignored by default.
- `cargo test --locked --no-default-features --all-targets` passes eight tests without the desktop. The transfer integration now also verifies a chosen destination outside the default download folder, including persistence across restart.
- `FLICK_AUDIO=null FLICK_TEST_VIDEO=/tmp/flick-sharing-validation-20261025.mp4 cargo test --locked --bin flick playback::tests::real_presentation_has_no_fixed_resolution_cap -- --ignored --nocapture` passes. Real libmpv rendered a 10000×2 target and a 7680×4320 target with exactly the requested dimensions and byte counts. This checks output sizing using a generated 1280×720 source; it does not establish 8K source decoding, display scanout, or sustained 8K performance. The other three native regressions were not rerun in this pass.
- A separate development app bundle used isolated settings, transfer state, and generated videos. The initial library showed Continue watching, watched history, and an unwatched video. Typing `quiet` filtered the local library to `A quiet afternoon.mp4`. Returning to the library paused the player and showed its updated saved position.
- Opening a generated 120-second video with an eligible saved position of 18 seconds resumed playback. The native window showed the moving test pattern and advancing time beyond that saved position. The final library history showed the saved position at 46 seconds. Audio used the null device.
- A live `Big Buck Bunny` search returned 48 results progressively from responding providers. YTS timed out and appeared separately as an unavailable source. No public torrent content was downloaded or shared during these checks.
- The background-worker regression shares generated local bytes named as a video, observes the typed `Shared` and verified `Completed` events, then requests and bounds clean shutdown. Peer integration checks remain loopback-only with public discovery disabled.

Library, search, metadata/file selection, Downloads controls, and sharing controls compile and use the tested native adapter. A complete GUI share → download → Play acceptance pass remains unverified. Native automation returned `Computer Use server error -10005: noWindowsAvailable` for coordinate clicks, and subsequent key actions sometimes produced no visible change. The file picker was inspected, but its final selection and the custom download controls were not confirmed through that automation. This is an acceptance limitation, not a passing GUI transfer result.

Watching incomplete downloads, torrent-based viewing identity, desktop torrent export/delete controls, hashing feedback, upload-rate display, and seeding preferences remain planned. Native GPU presentation, audible synchronization, release packaging, and current Windows/Linux desktop acceptance remain pending. Configured CI jobs have not been run remotely during this session.

User preview launch on 2026-10-25: the latest debug binary was copied into a development `Flick Preview.app` bundle under ignored `.flick-test/` and launched with normal settings and audio configuration. Its Library window was visually confirmed with the existing history migrated into Continue watching and All videos. Direct `cargo run --locked --bin flick` instead reproduced a GPUI macOS text-layout UTF-8 boundary panic in this session. The app bundle opens successfully; the direct-launch crash remains unresolved.

### macOS foundations and transfers on 2026-10-25

Environment: macOS 27.2 build 26B5101f, Apple Silicon, pinned Rust 1.99.0, GPUI 0.2.2, librqbit 9.0.1, Homebrew mpv/libmpv 0.41.0, FFmpeg 9.0.2, and Xcode Metal Toolchain 17F42. The machine's global stable alias was older; the repository and both native CI jobs now explicitly select the validated compiler.

Executed checks:

- `cargo check --locked --all-targets` passes on the native macOS checkout.
- `cargo fmt --all -- --check` and `cargo clippy --locked --all-targets -- -D warnings` pass. The existing upstream future-compatibility notices for `block` and `proc-macro-error2` remain.
- `cargo test --locked --all-targets` passes 14 tests: two presentation-budget tests, nine existing model/store tests, and three transfer integration tests. The three opt-in libmpv regression tests remain ignored by default and were not rerun in this pass.
- `cargo test --locked --no-default-features --all-targets` and the corresponding strict Clippy check pass. The normal dependency graph with desktop disabled contains no GPUI, Blade graphics, Cocoa, Metal, or winit. The transfer binary does not initialize or require libmpv.
- The transfer tests use independent clients with loopback TCP listeners and explicit peers. DHT, trackers, local multicast discovery, and automatic port mapping are disabled. They resolve magnet metadata over the real peer protocol, select one file from a multi-file torrent, verify partial progress, pause, close/restart the receiver, recheck existing data, resume from the explicit seed, compare final bytes, and remove the transfer while retaining its files.
- The exact-piece-boundary regression verifies a 2 MiB file's piece hash and corrected infohash independently with SHA-1. It checks that the magnet references the corrected dictionary and that sharing does not modify source data. The adapter applies a narrow workaround for librqbit issue 698; other invalid creation output is rejected.
- Negative transfer checks cover exclusive state ownership, reopening after shutdown, malformed metadata, missing inputs, invalid magnets, unsafe torrent filenames, empty shares, invalid/empty file selections, incomplete/unselected playback paths, bounded waits, and refusing unrelated existing destination data.
- A separate process-level diagnostic seeded a generated 12-second H.264/AAC video and downloaded it using the printed magnet and explicit loopback peer. Metadata inspection and successful download each produced one JSON document on stdout. All 5,150,150 downloaded bytes matched the source. Ctrl+C stopped the seed with exit code zero.
- `FLICK_AUDIO=null cargo run --locked -- --smoke-test /tmp/flick-sharing-validation-20261025.mp4` passed using the same video: six observed frames, first software presentation submission at 132 ms, duration 12 seconds, and observed position 6.033333 after the paused seek. This is not a display-scanout or audible-sync measurement.
- The native GPUI app was launched in a temporary development bundle under ignored `.flick-test/`. The empty window was visually checked with the transport hidden. A subsequently opened local video displayed the timeline and playback controls. These observations do not constitute a complete macOS desktop acceptance pass. The prior screenshots below are historical Linux captures.

At the end of this earlier pass, the backend and development diagnostic were implemented and desktop Downloads/Sharing views were still planned. The newer library pass above records their implementation and acceptance limits. Independent transfer CI jobs are configured for Windows, macOS, and Linux without desktop build prerequisites, but no remote CI execution is claimed here. The native GPU path, release packages, audible synchronization, and Windows execution remain pending.

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

Use the Rust 1.99.0 toolchain pinned in `rust-toolchain.toml`, including `rustfmt` and `clippy`. The application pins GPUI 0.2.2 and commits `Cargo.lock`; use `--locked` for repeatable dependency resolution. libmpv is loaded at runtime, so ordinary compilation and unit tests do not require libmpv headers or a linker import library. Transfer-only checks use `--no-default-features` and require no desktop prerequisites.

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
