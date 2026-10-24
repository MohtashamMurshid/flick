# Flick implementation plan

Flick is a desktop video player with built-in torrent downloads and file sharing. Open to the library, with continue watching, history, other added videos, and search for videos to download and watch. Keep downloads and sharing alongside this flow. Use Rust, GPUI, and libmpv for the application, with a separate Rust transfer backend based on a pinned librqbit release.

The detailed design, acceptance criteria, and delivery order are in [the playback and sharing plan](docs/SHARING_PLAN.md). This replaces the general agent/headless interface as the next product milestone. [The older headless proposal](docs/HEADLESS_PLAN.md) is deferred; small inspection and frame-export diagnostics may be added independently later.

## Product contract updated on 2026-10-25

- The PC app opens to Library. The mention of "phone" is treated as opening the app; a mobile frontend is not currently in scope.
- Continue watching opens a video at its saved position. History and unwatched/other added videos remain available in the library.
- Search is a primary library action and connects to downloading and playback. It is part of the first release.
- Playback follows the physical display bounds and source media. There is no application-imposed 720p, 4K, 8192-pixel, or pixel-budget limit. Native API representability and allocation failures are reported explicitly.

## Delivery order

### 1. Establish and verify the foundations

- [x] Document the revised product direction, architecture, and scope.
- [x] Hide the transport until media has loaded.
- [x] Remove fixed presentation resolution limits and size software frames for the actual display, with checked native dimensions and allocations.
- [x] Isolate desktop dependencies so transfer tests run without GPUI, Metal, a display, or libmpv.
- [x] Implement a pinned Rust transfer adapter with explicit storage, file metadata, selected-file downloads, pause/resume, completion checks, sharing, and shutdown.
- [x] Provide a small [development CLI](docs/TRANSFERS.md) to exercise the adapter before adding GUI controls.
- [x] Prove local sharing → download → pause → restart → resume → byte verification using two clients and generated files.
- [x] Cover the reported torrent-creation boundary bug and isolate transfer persistence from viewing settings.

Acceptance met on macOS on 2026-10-25: backend tests and desktop compilation pass; library and loaded window layouts were observed, and a real-video CLI transfer and null-audio playback diagnostic passed. Native libmpv also rendered 7680×4320 and a 10000-pixel-wide target. See [validation](docs/VALIDATION.md). Removing the size cap alone does not establish an efficient GPU path.

### 2. Library, search, downloads, and sharing

- [x] Open to a persistent library with continue watching, history, and other videos.
- [x] Add multiple videos or a folder without starting playback.
- [x] Filter the local library and search online from one search field.
- [x] Keep old history and migrate existing settings into the library without discarding videos.

- [x] Add magnet links and `.torrent` files through paste, a file picker, drag and drop, and command-line input.
- [x] Resolve metadata before selecting files and the destination directory.
- [x] Add a Downloads panel with progress, peers, speed, pause/resume, errors, and removal that keeps files.
- [x] Play a completed, verified video using its resolved local path. Non-video files remain downloadable.
- [x] Share a chosen file or folder in place and copy its magnet.
- [x] Show seeding state, provide pause/resume, and explain that transfers stop when Flick closes.
- [x] Connect one transfer session to GPUI with typed commands/events on a background runtime.
- [ ] Add delete-files confirmation, torrent export, hashing feedback, upload-rate display, and a first-run seeding preference to the desktop UI.
- [ ] Key viewing history/resume by infohash and file index for torrent media, alongside local paths. Backend transfer identities are already persisted.
- [ ] Complete the native share → download → Play acceptance pass through the GUI controls on each supported OS.

Acceptance: one native app can share a generated video, another instance can download it, and the completed file opens in Flick. Local playback remains usable if transfers fail. The local backend round trip and background completion events pass; the entire GUI round trip remains unverified because native click automation failed during this session.

### 3. Finish playback quality and native distribution

- [ ] Implement and measure efficient native GPU presentation, starting with macOS, then Windows and Linux.
- [ ] Verify native-resolution video, sound/synchronization, resize, subtitles, paused seeking, fullscreen, and sustained resource use on each OS.
- [ ] Package native dependencies and ship the applicable notices/licenses.
- [ ] Run the configured transfer regression jobs on native Windows, macOS, and Linux CI and record their results.

Acceptance: publish actual native results and release measurements. No performance advantage over VLC or mpv is claimed without comparative measurements.

### 4. Watch during downloads

- [ ] Read only verified pieces using librqbit's stream API, with bounded buffering and cancellation.
- [ ] Add buffering/recovery states and seek prioritization, including container metadata at the end of a file.
- [ ] Preserve resume identity when moving from a torrent stream to a completed local file.
- [ ] Test throttled peers, absent pieces, rapid seeks, file replacement, and subtitle behavior.

Acceptance: playback never reads sparse/unavailable disk regions as if they contained verified media.

### 5. Maintain discovery providers

- [x] Port TPB/API Bay and YTS adapters with Torlink attribution, and add Internet Archive discovery.
- [x] Keep providers independent from transfers and playback, with cancellation, progressive results, and explicit failure states.
- [ ] Recheck provider compatibility over time. TPB/API Bay and Internet Archive responded during validation; YTS timed out and was reported unavailable.

Watch folders, remote control, accounts, cloud storage, casting, and a general NDJSON agent interface are outside the first release. Search belongs in the library workflow.

## Existing prototype

The local-file prototype already includes video, sound controls, seeking, fullscreen, speed, tracks/subtitles, recents, and resume. Prior Linux checks and screenshots remain documented in [validation](docs/VALIDATION.md). Windows/macOS behavior is verified only when native checks actually run.
