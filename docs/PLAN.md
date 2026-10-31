# Flick implementation plan

Flick is a desktop video player with built-in torrent downloads and file sharing. Open to the library, with continue watching, history, other added videos, and search for videos to download and watch. Keep downloads and sharing alongside this flow. Use Rust, GPUI, and libmpv for the application, with a separate Rust transfer backend based on a pinned librqbit release.

The detailed design, acceptance criteria, and delivery order are in [the playback and sharing plan](SHARING_PLAN.md). This replaces the general agent/headless interface as the next product milestone. [The older headless proposal](HEADLESS_PLAN.md) is deferred; small inspection and frame-export diagnostics may be added independently later.

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
- [x] Provide a small [development CLI](TRANSFERS.md) to exercise the adapter before adding GUI controls.
- [x] Prove local sharing → download → pause → restart → resume → byte verification using two clients and generated files.
- [x] Cover the reported torrent-creation boundary bug and isolate transfer persistence from viewing settings.

Acceptance met on macOS on 2026-10-25: backend tests and desktop compilation pass; library and loaded window layouts were observed, and a real-video CLI transfer and null-audio playback diagnostic passed. Native libmpv also rendered 7680×4320 and a 10000-pixel-wide target. See [validation](VALIDATION.md). Removing the size cap alone does not establish an efficient GPU path.

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
- [x] Add delete-files confirmation, torrent export, hashing feedback, upload-rate display, and a first-run seeding preference to the desktop UI. Initial share hashing has a status message; verification uses the engine’s checked-byte progress.
- [x] Key viewing history/resume by infohash and file index for verified torrent media, alongside local paths. Re-downloaded identities retain resume across destinations without deleting existing history.
- [ ] Complete the native share → download → Play acceptance pass through the GUI controls on each supported OS.

Acceptance: one native app can share a generated video, another instance can download it, and the completed file opens in Flick. Local playback remains usable if transfers fail. The local backend round trip and background completion events pass; the entire GUI round trip remains unverified because native click automation failed during this session.

### 3. Finish playback quality and native distribution

- [ ] Implement and measure efficient native GPU presentation, starting with macOS, then Windows and Linux.
- [ ] Verify native-resolution video, sound/synchronization, resize, subtitles, paused seeking, fullscreen, and sustained resource use on each OS.
- [ ] Package native dependencies and ship the applicable notices/licenses. macOS packaging is drafted but unverified (item 25); Windows and Linux are not started.
- [ ] Run the configured transfer regression jobs on native Windows, macOS, and Linux CI and record their results.

Acceptance: publish actual native results and release measurements. No performance advantage over VLC or mpv is claimed without comparative measurements.

### 4. Watch during downloads

- [x] Read only verified pieces using librqbit's stream API, with bounded buffering and cancellation.
- [x] Add buffering/recovery states and seek prioritization, including container metadata at the end of a file.
- [x] Preserve resume identity when moving from a torrent stream to a completed local file. History uses the file's final local path from the start.
- [ ] Test throttled peers, absent pieces, rapid seeks, file replacement, and subtitle behavior. Throttled downloads, a seek to missing pieces, and cancelling a read blocked on a paused download are covered; rapid seeks, file replacement, and subtitles are not.

Acceptance: playback never reads sparse/unavailable disk regions as if they contained verified media.

### 5. Maintain discovery providers

- [x] Port TPB/API Bay and YTS adapters with Torlink attribution, and add Internet Archive discovery.
- [x] Keep providers independent from transfers and playback, with cancellation, progressive results, and explicit failure states.
- [ ] Recheck provider compatibility over time. TPB/API Bay and Internet Archive responded during validation; YTS timed out and was reported unavailable.

### 6. Review backlog from 2026-10-24

Reported after using the macOS build. Items are numbered as reported so commits and validation notes can refer to them. Some items repeat earlier unchecked work, which the backlog now tracks.

Bugs:

- [x] 1. Back from fullscreen exits fullscreen. `Control::Library` toggles fullscreen unconditionally. The library already renders in fullscreen, and Escape still exits.
- [x] 2. Double-click goes to fullscreen. GPUI 0.2.2 has no selectable static text, so double-clicks fall through to the video surface. Opening a library card with a double-click delivers the second click to the new video surface. Ignore video double-clicks for 500 ms after entering the player. Make read-only values, such as magnet links and file paths, selectable read-only fields.

Accessibility:

- [ ] 3. Screen readers see an empty window: GPUI 0.2.2 exposes no accessibility tree, and AccessKit is absent. This is a framework gap. Upgrade GPUI when it adds AccessKit, or contribute support.
- [x] 4. Keyboard navigation: make buttons and cards focusable, define a Tab order with a visible focus ring, move between cards with the arrow keys, and open them with Enter. Library keys other than Ctrl/Cmd shortcuts are currently dropped.
- [x] 5. Show tooltips on keyboard focus as well as hover. Add text labels, or a setting that always shows them. The setting covers toolbar and player title-bar buttons; the transport stays icon-only, with focus tooltips.
- [x] 6. Pad the hit areas of small icon buttons, including 28 px macOS icon buttons, toward Apple's 44 pt guideline while keeping their visual size. Hit areas reach 44 px vertically; widths are unchanged because neighbors are 4 px apart and GPUI 0.2.2 collapses rows with negative horizontal margins.
- [x] 7. Add text scaling, a reduce-motion switch for fades and control slides, and high contrast. Follow OS settings where GPUI exposes them. Text size (85–150%), high contrast, and Reduce motion are in Appearance. Player controls now fade and slide, and the status message fades in; Reduce motion shows them at once. Auto contrast and motion follow the OS settings, which Flick reads itself because GPUI exposes none.
- [x] 8. Adjustable subtitles: size, color, background, and position, using mpv's `sub-scale`, `sub-color`, `sub-back-color`, and `sub-pos`.
- [x] 9. Check grey-on-black text and both color schemes' accents against WCAG AA. Several failed; themes now adjust lightness to pass, and a test covers every style, scheme, and accent.
- [x] 10. Add a shortcuts page in Settings and make shortcuts rebindable; `?` is the only discovery path today.

Performance:

- [x] 11. Hardware decoding: use `hwdec=auto-copy`, which selects VideoToolbox, D3D11, or VAAPI, instead of `hwdec=no`. Only the software fallback was observed so far: the Linux validation VM has no GPU.
- [x] 12. Stop copying each frame twice. mpv renders into a 64-byte-aligned buffer, then every row is repacked into a new `Vec`. libmpv now renders directly into the frame GPUI receives.
- [ ] 13. Native GPU presentation (stage 3): render through mpv's GPU API into a texture that GPUI draws directly. This is the real fix for 4K. GPUI 0.2.2 draws only CPU images, except macOS `CVPixelBuffer` surfaces. The likely macOS route is mpv's OpenGL render API drawing into an IOSurface-backed texture, shown with GPUI's `surface` element; it needs a Mac to build and test. Linux and Windows need GPUI support for external textures. Until then, the render thread and direct rendering (items 12 and 14) keep scaling off the interface thread.
- [x] 14. Use a separate render thread with callback-driven redraws instead of a fixed 16 ms UI poll, which fits neither 24 fps content nor 120 Hz displays and stalls the UI on a slow decode. A render thread owns the libmpv render context and is woken by its update callback; the UI presents frames as they arrive and polls playback state every 50 ms. The responsiveness gain has not been measured on an idle machine.
- [x] 15. Benchmark harness: CPU, dropped frames, and memory over a 5-minute 4K loop. Speed claims and regression checks depend on it.

Features:

- [x] 16. Real thumbnails: capture and cache a poster frame using the software renderer, recording duration and resolution at the same time. Cards currently use a hashed gradient. A background thread with its own silent player saves PNG posters to the cache, retrying later in the video if the frame is nearly black; cards show resolution badges and durations.
- [x] 17. Group series by `S01E02`-style names and folders, and advance to the next episode automatically. A Shows page lists each show's episodes by season; Shift+N and Shift+P change episodes, and an 8-second Up next countdown can be turned off in Appearance.
- [x] 18. Library: sort options, a grid/list toggle, Mark as watched, and Remove from Continue watching.
- [x] 19. Player controls: chapter markers, frame step (`.` and `,`), J/K/L shuttle, number keys for 10–90%, an A–B loop, and audio delay. The software renderer cannot play in reverse, so J slows a shuttle and otherwise goes back 10 seconds.
- [x] 20. Timeline hover previews, after item 16. A second silent player seeks to the nearest keyframe and shows the frame, time, and chapter above the pointer.
- [x] 21. Picture-in-picture, or an always-on-top mini window. P or the player button opens a small pop-up window that keeps playing while the library is used. It floats on macOS; on X11 staying above depends on the window manager (Openbox let the main window cover it), and Wayland has no always-on-top request.
- [x] 22. OpenSubtitles search from the Audio & subtitles menu. Find Online searches by title and OpenSubtitles file hash, lists exact matches first, and downloads and loads a choice beside the video. It needs a personal API key, entered in Appearance or set with `FLICK_OPENSUBTITLES_KEY`. Only the rejected-key path has been checked against the live API.
- [x] 23. Watch while downloading (stage 4): stream verified pieces from librqbit and prioritize pieces near the playhead. Watch Now plays a downloading video through a `flicktorrent://` libmpv stream; library cards for partly downloaded files use it too, and posters wait for completion.
- [x] 24. Transfer gaps from stage 2: delete-files confirmation, upload rate, torrent export, seeding preference, resume keyed by infohash, live bandwidth limits, global pause, and hashing feedback. The complete GUI acceptance pass on each OS remains the separate unchecked stage-2 item.
- [x] 26. Download from YouTube, X, and other sites supported by [yt-dlp](https://github.com/yt-dlp/yt-dlp) using a pasted page URL. Run an installed yt-dlp as a separate process, separate from playback and the torrent session, with progress, cancellation, and completed videos joining the library. Tests use a generated local stand-in for yt-dlp and no network access. Done in `src/fetch.rs` and `src/fetch_ui.rs`; direct HTTPS video links skip yt-dlp. X was not tried with a real post; see [validation](VALIDATION.md#link-downloads-on-2026-10-24).

Transfer reliability, which should close most of the gap with other clients:

- [x] 27. Add Torlink's tracker list to search magnets in `catalog.rs`, and to `SessionOptions.trackers`, which librqbit adds to every torrent as a fallback.
- [x] 28. Keep the DHT routing table across launches, in the transfer state directory, instead of `persistence: None`.
- [x] 29. Turn on UPnP port forwarding, with a fixed listen port so the mapping stays stable.
- [x] 30. Allow five minutes for metadata resolution, show "Finding peers…", offer Cancel, and explain how to recover after a timeout.
- [x] 31. Download Internet Archive results over HTTPS from archive.org's `download/` URLs, using the URL-download backend from item 26, instead of the Archive torrent. Check whether a newer librqbit supports web seeds. Results now list the item's files from the metadata API and download over HTTPS with SHA-1 checks. librqbit 9.0.1 is still the newest release and has no web seed (`url-list`) support.
- [x] 32. Measure `TcpAndUtp` against `TcpOnly` with fresh public Sintel sessions; keep TCP+uTP for public sessions after eight recorded trials. Local sessions retain TCP. See [validation](VALIDATION.md#public-discovery-and-listener-comparison-on-2026-10-24).

Automated transfer tests stay local, with generated fixtures and explicit peers. Public trackers, DHT, and UPnP are checked only in recorded manual runs.

Distribution:

- [ ] 25. Bundle libmpv, sign and notarize, and ship a DMG. The app currently requires `brew install mpv`, so it can't be handed to anyone. `scripts/package-macos.sh` and the `macos-package` CI job now do this; see [Packaging for macOS](PACKAGING.md). They were written on Linux, where only syntax checks, ShellCheck, a dry run, and a run with stand-in macOS tools passed. The bundle, signatures, notarization, DMG, and playback from the bundle are unverified until the job or a Mac runs them. Distribution also waits on a Developer ID, notarization credentials, and the license questions in that document: Homebrew's libmpv and FFmpeg are GPL, and corresponding source is not yet provided. Flick's own source is under the MIT License. Finder's Open With launches Flick without opening the file until `src/main.rs` handles `on_open_urls`.

### 7. Follow-ups from 2026-10-25

- [x] 33. Quit with ⌘Q on macOS and Ctrl+Q elsewhere, with Quit Flick in the macOS app menu. Settings and the playback position are saved first.
- [x] 34. A double-click inside the title bar's search field or on its buttons no longer zooms or fills the screen; only empty title-bar space does.
- [x] 35. ⌘K or Ctrl+K opens a centered command center over any view: library videos, online search and results, link downloads, and commands, chosen with the arrow keys. The library search field shows the shortcut.
- [x] 36. Pasting a YouTube, X, other web, or magnet link into the command center or the search field offers its download straight away, naming the site.
- [x] 37. Cards say Watched only for finished videos, using the same flag as Mark as Watched; older history that reached the end counts as finished.

Watch folders, remote control, accounts, cloud storage, casting, and a general NDJSON agent interface are outside the first release. Search belongs in the library workflow.

## Existing prototype

The local-file prototype already includes video, sound controls, seeking, fullscreen, speed, tracks/subtitles, recents, and resume. Prior Linux checks and screenshots remain documented in [validation](VALIDATION.md). Windows/macOS behavior is verified only when native checks actually run.
