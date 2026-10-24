# Validation and platform support

Flick targets Windows, macOS, and Linux. A CI configuration is not evidence of a passing native build, and a successful build is not evidence of working playback. Record actual results below as checks run.

## Current evidence

| Platform | Native compilation and tests | Desktop playback | Native GPU video path |
| --- | --- | --- | --- |
| Linux | Native debug build, formatting, strict Clippy, all ordinary tests, and all five real-libmpv tests pass on 2026-10-24 | Xvfb/Openbox session exercised for the review backlog and in all three interface styles; null audio | Deferred; hardware decoding unverified without a GPU |
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

- **Internet Archive over HTTPS.** [`Nabisco_2`](https://archive.org/details/Nabisco_2) is a public-domain Prelinger item. Its listing preselected the two originals: an MPEG-2 file and a HiRes MPEG-4. The three archive.org derivatives were listed but not selected. The first `get` was killed externally after saving and verifying the first file, leaving an empty part file for the second. The cause was not determined; the shell running it was terminated, and Flick logged no error. Running `get` again reused the verified first file without downloading it, completed the second, and removed the part file. Both SHA-1s matched archive.org's metadata (`d723d45d…` and `58a234df…`), and FFmpeg decoded both as about 10.5 seconds of video with audio.
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

`review-backlog` at `0874128` was then merged into this branch. That work added keyboard access, text scaling, Shows, and the transfer-reliability changes. Link rows and the Archive chooser now use scaled text, and Archive file rows join the Tab order like torrent file rows.

On the merged tree, these checks pass: formatting, both strict Clippy runs, and the dependency isolation check. `cargo test --locked --all-targets` passes 78 tests: 39 library, 35 desktop, and 4 transfer integration. Six are ignored: five libmpv tests and the yt-dlp test. `--no-default-features` passes 43, with one ignored. The opt-in yt-dlp loopback test also passes.

A short GUI check of the merged build used loopback media and one archive.org listing, and showed the following:

- A link download completed.
- Tab reached **Paste Link**, whose tooltip read "Paste Link", then Share Files…, then the row's Play and remove buttons. The remove button's tooltip read "Remove from list · keep files".
- Enter on the remove button removed the row and kept the file.
- Tab moved a focus ring through the Archive chooser's file rows, and Space toggled the focused file.

Not verified:

- **macOS and Windows.** In particular, on Windows the yt-dlp console window is suppressed with `CREATE_NO_WINDOW`, and cancellation ends the process tree with `taskkill /T`. Neither was compiled or run in this session; Windows CI has not run them.
- **Downloads on other sites.** X downloads, playlist addresses, and sites that require signing in were not tried.
- **Edge cases.** A full disk and resuming an HTTPS download after a real crash, beyond the one external interruption above, are untested.

All downloaded media, generated clips, and the downloaded yt-dlp release were deleted after these checks.
### Review backlog: playback, keyboard, and accessibility on 2026-10-24

This covers backlog items 1, 2, 4–12, 18, and 19 in [PLAN.md](../PLAN.md). Environment: the same Ubuntu 24.04.5 x86_64 VM (four vCPUs, no GPU), Rust 1.99.0, GPUI 0.2.2, and libmpv 0.37.0. Interactive checks used Xvfb with Openbox and Mesa Lavapipe, null audio, isolated settings, and generated fixtures: the 12-second 1280×720 H.264/AAC clip with SRT captions from `scripts/make-test-video.sh`, and a 6-second 3840×2160 `testsrc2` clip.

Automated checks:

- `cargo fmt --all -- --check`, `cargo clippy --locked --all-targets -- -D warnings`, and `cargo clippy --locked --no-default-features --all-targets -- -D warnings` pass.
- `cargo test --locked --all-targets` passes. New desktop tests cover shortcut binding, conflicts, persistence, and keystroke normalization; WCAG contrast for every style, scheme, accent, and contrast mode; appearance clamping and subtitle properties; grid arrow navigation; A–B loop steps; chapter parsing; Continue Watching dismissal, watched marks, and sorting; and double-click word selection in read-only fields.
- All five opt-in libmpv tests pass with `FLICK_AUDIO=null FLICK_TEST_VIDEO=<720p clip>`: real playback, paused-seek image change, lifecycle/subtitles/EOF, the uncapped 10000×2 and 7680×4320 targets, and the new check that direct and aligned render targets produce byte-identical frames.
- `FLICK_AUDIO=null cargo run --locked -- --smoke-test <720p clip>` passes: six frames, first software presentation submission at 373 ms, and a paused seek to 6.07 s.

Rendering into the frame GPUI receives (item 12) was measured with the identical-pixel test in a release build. libmpv rendered a paused 4K frame 20 times per size; numbers are milliseconds per frame, including scaling, so they bound the copy's share rather than isolating it. Other builds were running on the VM, so treat the runs as rough comparisons:

| Target size | Aligned buffer + copy, run 1 | Direct, run 1 | Aligned, run 2 | Direct, run 2 |
| --- | --- | --- | --- | --- |
| 1920×1080 | 32.25 | 24.22 | 37.11 | 32.90 |
| 1917×1080 (rows not a 64-byte multiple) | — | — | 50.34 | 32.56 |
| 3840×2160 | 74.61 | 59.34 | 86.27 | 81.78 |

The direct target was faster at every size, although glibc's large allocations are only 16-byte aligned, so it is now the default whenever the frame meets BGR0's four-byte alignment. The format changed from mpv's undocumented `bgra` to the documented `bgr0`, with alpha still forced opaque.

`hwdec=auto-copy` (item 11) was exercised only without a GPU. libmpv probed VDPAU, found no driver, and decoded in software; `hwdec-current` was empty, and Flick now logs and reports it. Hardware decoding on VideoToolbox, D3D11, or VAAPI remains unverified.

Interactive observations under Xvfb:

- Back from fullscreen kept the library fullscreen (1280×860, no title bar), and Escape returned the window to 1100×740 (item 1).
- Double-clicking a library card opened the video and left the window at 1100×740 (item 2). The magnet and download-folder fields render as read-only fields; selecting and copying their text was not exercised.
- Tab moved through the sidebar, toolbar, search field, See All, and cards with a visible focus ring. A focused toolbar button showed its tooltip below it. Right arrow moved between cards. Shift+F10 opened the card menu with Play focused, and Tab, Tab, Enter chose Mark as Unwatched, which reset the card to "Not started" and cleared its saved history (items 4, 5, and 18). The GNOME header showed the same ring and focus tooltip.
- In the macOS style, a 28 px toolbar button now has a 28×44 hit area. An earlier attempt to widen hit areas collapsed the toolbar, because GPUI 0.2.2 gives rows with negative horizontal margins no width; widths are unchanged (item 6).
- Number keys sought to 10% and 20%. After the loop fix described below, `b`, a pause, then `b` showed "Loop 0:02–0:03" and an accent span on the timeline, and `]` showed "Audio delay +50 ms" (item 19). Frame step, J/K/L, and chapter keys were not exercised interactively; the fixtures have no chapters.
- On the Keyboard Shortcuts page, Change followed by `n` rebound Mute; settings stored `{"mute": "n"}`, and the row offered Restore (item 10).
- Text size 130% enlarged text live. High contrast brightened secondary text and borders. Auto contrast followed GNOME's `org.gnome.desktop.a11y.interface high-contrast` setting through an isolated keyfile GSettings backend, on and off (item 7).
- Yellow subtitles with the Box background and a raised position rendered as a translucent box behind each line. libmpv 0.37 has no `sub-border-style`, which Flick skips; `sub-back-color` with a shadow offset produced the box (item 8).
- Finding peers… and its Cancel button were exercised with a stalled loopback peer and local-only transfers. Cancel cleared the notice, and no late result appeared 12 seconds later. Without a peer, local-only resolution fails immediately, which is expected.

Fixed during this pass: the A–B key used mpv's three-state `ab-loop` cycle, which kept stale state and could store the end before the start; it now sets an ordered start and end itself. Earlier, a capture taken after the 12-second clip reached its end showed the player paused at the seek target. That is `keep-open` pausing at the end, not a bug.

Not verified: macOS and Windows, including macOS's `increaseContrast` and Windows' HighContrast flags read through `defaults` and `reg`; screen readers (GPUI 0.2.2 exposes no accessibility tree); audible output; and hardware decoding.

### Benchmark harness on 2026-10-24

Backlog item 15 uses the real `Player` software renderer in the desktop binary without opening GPUI or starting transfers. Environment: Ubuntu 24.04.5 x86_64, Linux 7.0.0-1011-gcp, four vCPUs, 15 GiB RAM, no GPU; pinned Rust 1.99.0 (`b940084d7`), GPUI 0.2.2, libmpv 0.37.0 at `/lib/x86_64-linux-gnu/libmpv.so.2`, FFmpeg 6.1.1. Standard release profile: thin LTO, one codegen unit, stripped debuginfo. Other work can run concurrently on this VM; load snapshots below label these as shared-VM observations rather than controlled performance comparisons.

The fixtures are generated locally with no audio: 12-second H.264 `testsrc2`, yuv420p, 30 fps, 360 frames. `ffprobe` confirmed both dimensions, frame rate/count, codec, and duration after encoding completed. An earlier probe during encoding reported `moov atom not found`; probing the completed file passed. Encoding and compilation finish before the timed runs. The benchmark starts after its first software submission, loops with `loop-file=inf`, polls native events, and calls `Player::render` on 16 ms deadlines at the source resolution. Metrics and their limits are defined in the [performance gate](#performance-gate).

Commands used to prepare the release binary and fixtures (`benchmark_dir` was `/tmp/flick-benchmark-validation.ZoKckK`):

```sh
cargo build --locked --release --bin flick
benchmark_dir=$(mktemp -d /tmp/flick-benchmark-validation.XXXXXX)
ffmpeg -hide_banner -loglevel warning -nostdin -n \
  -f lavfi -i 'testsrc2=size=3840x2160:rate=30:duration=12' \
  -an -c:v libx264 -preset veryfast -crf 23 -threads 4 -pix_fmt yuv420p \
  -color_primaries bt709 -color_trc bt709 -colorspace bt709 \
  -movflags +faststart -f mp4 "$benchmark_dir/4k.mp4"
ffmpeg -hide_banner -loglevel warning -nostdin -n \
  -f lavfi -i 'testsrc2=size=1920x1080:rate=30:duration=12' \
  -an -c:v libx264 -preset veryfast -crf 23 -threads 4 -pix_fmt yuv420p \
  -color_primaries bt709 -color_trc bt709 -colorspace bt709 \
  -movflags +faststart -f mp4 "$benchmark_dir/1080p.mp4"
```

An initial timed attempt was stopped before completion after a sanity report exposed startup-only decoder reporting (`vulkan-copy` in an early sanity report despite no physical GPU). The harness was corrected to retain both initial and final decoder status, then rebuilt and rechecked before the recorded full run. That incomplete attempt is not a performance baseline. A subsequent 4K attempt received SIGTERM before completion while another desktop instance was restarted on the shared VM. To avoid process-name collisions, recorded measurements used a copy of the release executable named `measure`; `cmp` and SHA-256 confirmed identical bytes (`42d6cd91c84923eb3856953da9d23c517baf8c386b8cf343635f35eb44cd6e85`). Neither interrupted attempt produced a final JSON report or counts as a full run.

Native measurements used these commands (`measure` is the byte-identical release executable), with `uptime` immediately before and after each process (UTC times and complete load lines follow):

```sh
cp -- target/release/flick "$benchmark_dir/measure"
cmp -- target/release/flick "$benchmark_dir/measure"

env -u FLICK_HWDEC FLICK_AUDIO=null FLICK_MPV_LIBRARY=/lib/x86_64-linux-gnu/libmpv.so.2 \
  "$benchmark_dir/measure" --benchmark "$benchmark_dir/4k.mp4" \
  --seconds 300 --size 3840x2160 --interval-ms 16
env -u FLICK_HWDEC FLICK_AUDIO=null FLICK_MPV_LIBRARY=/lib/x86_64-linux-gnu/libmpv.so.2 \
  "$benchmark_dir/measure" --benchmark "$benchmark_dir/1080p.mp4" \
  --seconds 60 --size 1920x1080 --interval-ms 16
FLICK_AUDIO=null FLICK_HWDEC=no FLICK_MPV_LIBRARY=/lib/x86_64-linux-gnu/libmpv.so.2 \
  "$benchmark_dir/measure" --benchmark "$benchmark_dir/4k.mp4" \
  --seconds 60 --size 3840x2160 --interval-ms 16
```

All three processes exited 0. Each reported `hardware_decoder_initial: null` and `hardware_decoder: null`: decoding ended in software with both default `auto-copy` and explicit `no`. CPU and memory figures are observations under varying contention, not a controlled decoder comparison. The default mode did not establish hardware acceleration on this VM.

| Run | Wall seconds | Software submissions | Nominal source frames | CPU % (100% = one CPU) | Process peak RSS MiB | Average RSS MiB | Frame-producing render p95 ms | Observed VO drops |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 4K, default auto-copy, 300 s | 300.150 | 1965 | 9004.50 | 150.70 | 421.74 | 419.03 | 160.06 | 4226 |
| 1080p, default auto-copy, 60 s | 60.000 | 1659 | 1800.01 | 84.90 | 209.61 | 206.93 | 32.76 | 116 |
| 4K, hwdec=no, 60 s | 60.134 | 406 | 1804.02 | 150.60 | 343.33 | 336.64 | 141.52 | 874 |

Decoder drop-counter increases were zero in all three runs. The nominal source-frame estimate is not an independent loss measurement. VO counters reset at loop boundaries; observed increases can undercount unsampled drops, as described below. Linux `/proc` RSS and `getrusage` peak RSS are independently sampled kernel readings and can differ slightly.

UTC process times and raw `uptime` snapshots:

4K, default auto-copy, 300 s: 2026-10-24T21:18:24.496103+00:00 → 2026-10-24T21:23:26.315653+00:00.

```text
before: 21:18:24 up 12 days, 10:53,  0 user,  load average: 10.73, 11.70, 10.87
after:  21:23:26 up 12 days, 10:58,  0 user,  load average: 12.43, 12.85, 11.68
```

1080p, default auto-copy, 60 s: 2026-10-24T21:23:26.334058+00:00 → 2026-10-24T21:24:27.201214+00:00.

```text
before: 21:23:26 up 12 days, 10:58,  0 user,  load average: 12.43, 12.85, 11.68
after:  21:24:27 up 12 days, 10:59,  0 user,  load average: 9.62, 11.92, 11.44
```

4K, hwdec=no, 60 s: 2026-10-24T21:24:27.210218+00:00 → 2026-10-24T21:25:28.078114+00:00.

```text
before: 21:24:27 up 12 days, 10:59,  0 user,  load average: 9.62, 11.92, 11.44
after:  21:25:28 up 12 days, 11:00,  0 user,  load average: 12.63, 12.40, 11.64
```

Raw stdout JSON, unchanged (one document per completed measurement):

4K, default auto-copy, 300 s:

```json
{"schema_version":1,"presentation":"headless-software-bgr0-submissions","build_profile":"release","os":"linux","arch":"x86_64","video":"/tmp/flick-benchmark-validation.ZoKckK/4k.mp4","requested_seconds":300,"elapsed_seconds":300.149863567,"startup_seconds":1.302696756,"interval_ms":16,"target_size":[3840,2160],"frames_presented":1965,"expected_frames":9004.495907010001,"skipped_tick_deadlines":16623,"source":{"width":3840,"height":2160,"fps":30.0,"fps_property":"container-fps","codec":"H.264 / AVC / MPEG-4 AVC / MPEG-4 part 10"},"hardware_decoder":null,"hardware_decoder_initial":null,"flick_hwdec":null,"effective_hwdec":"auto-copy","mpv_version":"mpv 0.37.0","flick_audio":"null","frame_drop_count":{"initial":0,"last":37,"observed_increase":4226,"observed_resets":17,"unavailable_samples":0},"decoder_frame_drop_count":{"initial":0,"last":0,"observed_increase":0,"observed_resets":0,"unavailable_samples":0},"render_call_ms":{"count":2137,"p50":93.59176,"p95":156.876228,"p99":347.332086,"max":636.003359},"frame_render_ms":{"count":1965,"p50":95.563071,"p95":160.058036,"p99":353.126377,"max":636.003359},"cpu":{"user_seconds":449.444793,"system_seconds":2.8861239999999997,"average_percent":150.7016900239336},"memory":{"process_peak_rss_bytes":442228736,"sampled_peak_rss_bytes":442523648,"time_weighted_average_rss_bytes":439389474.10235876,"samples":276,"covered_seconds":300.149863567}}
```

1080p, default auto-copy, 60 s:

```json
{"schema_version":1,"presentation":"headless-software-bgr0-submissions","build_profile":"release","os":"linux","arch":"x86_64","video":"/tmp/flick-benchmark-validation.ZoKckK/1080p.mp4","requested_seconds":60,"elapsed_seconds":60.000252412,"startup_seconds":0.602556541,"interval_ms":16,"target_size":[1920,1080],"frames_presented":1659,"expected_frames":1800.00757236,"skipped_tick_deadlines":1381,"source":{"width":1920,"height":1080,"fps":30.0,"fps_property":"container-fps","codec":"H.264 / AVC / MPEG-4 AVC / MPEG-4 part 10"},"hardware_decoder":null,"hardware_decoder_initial":null,"flick_hwdec":null,"effective_hwdec":"auto-copy","mpv_version":"mpv 0.37.0","flick_audio":"null","frame_drop_count":{"initial":0,"last":0,"observed_increase":116,"observed_resets":4,"unavailable_samples":0},"decoder_frame_drop_count":{"initial":0,"last":0,"observed_increase":0,"observed_resets":0,"unavailable_samples":0},"render_call_ms":{"count":2369,"p50":13.250145,"p95":30.550722,"p99":42.981763,"max":272.099101},"frame_render_ms":{"count":1659,"p50":16.229157,"p95":32.763816,"p99":53.194814,"max":272.099101},"cpu":{"user_seconds":50.433640999999994,"system_seconds":0.508305,"average_percent":84.90288615821164},"memory":{"process_peak_rss_bytes":219795456,"sampled_peak_rss_bytes":220164096,"time_weighted_average_rss_bytes":216987031.34934205,"samples":60,"covered_seconds":60.000252412}}
```

4K, hwdec=no, 60 s:

```json
{"schema_version":1,"presentation":"headless-software-bgr0-submissions","build_profile":"release","os":"linux","arch":"x86_64","video":"/tmp/flick-benchmark-validation.ZoKckK/4k.mp4","requested_seconds":60,"elapsed_seconds":60.133878383,"startup_seconds":0.549841166,"interval_ms":16,"target_size":[3840,2160],"frames_presented":406,"expected_frames":1804.01635149,"skipped_tick_deadlines":3326,"source":{"width":3840,"height":2160,"fps":30.0,"fps_property":"container-fps","codec":"H.264 / AVC / MPEG-4 AVC / MPEG-4 part 10"},"hardware_decoder":null,"hardware_decoder_initial":null,"flick_hwdec":"no","effective_hwdec":"no","mpv_version":"mpv 0.37.0","flick_audio":"null","frame_drop_count":{"initial":0,"last":140,"observed_increase":874,"observed_resets":3,"unavailable_samples":0},"decoder_frame_drop_count":{"initial":0,"last":0,"observed_increase":0,"observed_resets":0,"unavailable_samples":0},"render_call_ms":{"count":433,"p50":96.260296,"p95":141.069883,"p99":164.80947,"max":216.291837},"frame_render_ms":{"count":406,"p50":97.785993,"p95":141.51701,"p99":164.80947,"max":216.291837},"cpu":{"user_seconds":90.04964799999999,"system_seconds":0.5118699999999999,"average_percent":150.5998289736156},"memory":{"process_peak_rss_bytes":360009728,"sampled_peak_rss_bytes":360173568,"time_weighted_average_rss_bytes":352992668.19098395,"samples":57,"covered_seconds":60.133878383}}
```

Checks on the final implementation:

- `cargo fmt --all -- --check`: passed.
- `cargo clippy --locked --all-targets -- -D warnings`: passed.
- `cargo clippy --locked --no-default-features --all-targets -- -D warnings`: passed.
- `cargo test --locked --all-targets`: passed, 53 tests total (10 library, 39 desktop binary, 4 local transfer integration); 6 opt-in native tests ignored. The desktop tests include 5 new pure benchmark tests covering arguments, JSON shape, percentiles/CPU math, counter resets/RSS integration, and deadline skipping.
- `cargo test --locked --no-default-features --all-targets`: passed, 14 tests (10 library, 4 local transfer integration). No desktop or libmpv dependency is initialized by this suite.
- `cargo build --locked --release --bin flick`: passed. The initial build took 29m58s; the final relink took 5m18s. Cargo emitted an existing future-compatibility notice for `proc-macro-error2` 2.0.1; exit status was zero. No dependency was added or updated.
- `FLICK_AUDIO=null FLICK_MPV_LIBRARY=/lib/x86_64-linux-gnu/libmpv.so.2 FLICK_TEST_VIDEO="$benchmark_dir/1080p.mp4" cargo test --locked --bin flick benchmark::tests::native_benchmark -- --ignored --nocapture`: passed, one real-libmpv test. Final corrected debug test: 53 software submissions in 2.000326136 s at 320×180; initial and final hardware decoder both `null`.
- `bash -n scripts/benchmark.sh` and `bash scripts/benchmark.sh --help`: passed. Six script argument-error cases (unknown flag, zero seconds, odd target width, missing size, duplicate size, extra positional argument) returned exit 2 with empty stdout. The release CLI help and four invalid benchmark argument cases also passed without opening GPUI; invalid requests returned nonzero with empty stdout.
- A two-second release sanity run at 320×180 preceded the recorded measurements. Each report was parsed as a single JSON document and checked for release profile, elapsed duration, frame/call count consistency, source size/fps/codec, populated mpv version/drop counters, and positive Linux CPU/RSS readings.

End-to-end fixture script check:

```sh
env -u FLICK_HWDEC FLICK_AUDIO=null FLICK_MPV_LIBRARY=/lib/x86_64-linux-gnu/libmpv.so.2 \
  bash scripts/benchmark.sh 2
```

Passed: generated a fresh local 4K fixture, ran the release binary for 2.063616042 s, produced one parseable stdout JSON document (16 software submissions), and removed its temporary fixture/directory. Diagnostics stayed on stderr. Raw stdout:

```json
{"schema_version":1,"presentation":"headless-software-bgr0-submissions","build_profile":"release","os":"linux","arch":"x86_64","video":"/tmp/flick-benchmark.xJ83UR/testsrc2-3840x2160-30fps.mp4","requested_seconds":2,"elapsed_seconds":2.063616042,"startup_seconds":1.1011311990000001,"interval_ms":16,"target_size":[3840,2160],"frames_presented":16,"expected_frames":61.90848126,"skipped_tick_deadlines":110,"source":{"width":3840,"height":2160,"fps":30.0,"fps_property":"container-fps","codec":"H.264 / AVC / MPEG-4 AVC / MPEG-4 part 10"},"hardware_decoder":null,"hardware_decoder_initial":null,"flick_hwdec":null,"effective_hwdec":"auto-copy","mpv_version":"mpv 0.37.0","flick_audio":"null","frame_drop_count":{"initial":1,"last":44,"observed_increase":43,"observed_resets":0,"unavailable_samples":0},"decoder_frame_drop_count":{"initial":0,"last":0,"observed_increase":0,"observed_resets":0,"unavailable_samples":0},"render_call_ms":{"count":18,"p50":76.844495,"p95":145.241529,"p99":145.241529,"max":145.241529},"frame_render_ms":{"count":16,"p50":77.32964100000001,"p95":145.241529,"p99":145.241529,"max":145.241529},"cpu":{"user_seconds":3.9602260000000005,"system_seconds":0.13266299999999998,"average_percent":198.3357813032547},"memory":{"process_peak_rss_bytes":430698496,"sampled_peak_rss_bytes":431112192,"time_weighted_average_rss_bytes":359625981.11761004,"samples":4,"covered_seconds":2.063616042}}
```

The separately prepared manual fixture videos and copied `measure` executable were removed after the completed runs; raw JSON and runner logs remained in the task-owned temporary directory during this session.

These are headless native playback measurements and desktop compilation/unit-test evidence. No interactive playback, display scanout, GPUI upload, audible sound, A/V synchronization, native GPU path, or VLC/mpv comparison was tested in this pass. The synthetic fixtures exercise software conversion and allocation and do not represent every codec or real-world file. Generated media is not committed.

### Public discovery and listener comparison on 2026-10-24

Environment: Ubuntu 24.04.5, x86_64, Linux 7.0.0-1011-gcp, four vCPUs (Intel Xeon at 2.20 GHz), 15 GiB RAM. Rust 1.99.0 (`b940084d7`), pinned librqbit 9.0.1, unoptimized development binary with debug symbols disabled. Other Rust compilation was running on this VM during the measurements; these are short public-swarm observations, not a controlled hardware benchmark or a general speed claim.

The tracker list was fetched from Torlink's [`src/sources/magnet.ts`](https://github.com/baairon/torlink/blob/df059311d0898b7245dd842153c1ff7c892275ee/src/sources/magnet.ts), revision `df059311d0898b7245dd842153c1ff7c892275ee`. Its 11 entries are shared by search magnets and public sessions. Public sessions enable DHT persistence at `state/dht.json`, saved listen-port selection, and UPnP; automated tests keep public discovery and port mapping off. The engine dumps DHT state every 60 seconds. These completed swarm trials were shorter than that interval, so they do not establish an observed DHT save/reload. The pinned UPnP implementation requests TCP mappings only. No successful router UPnP mapping was observed or claimed.

#### Manual public transfers (item 32)

Only the authorized legal [Sintel torrent](https://webtorrent.io/torrents/sintel.torrent) was used. Its v1 infohash is `08ada5a7a6183aae1e09d831df6748d566095a10`. Each run had new state and download directories, public discovery enabled, no rate limit, and a 120-second maximum window. The modes alternated TCP, TCP+uTP, TCP, TCP+uTP for each input type. A separate UDP tracker connect probe to `tracker.opentrackr.org:1337` returned a valid transaction-matching 16-byte response in 0.181 seconds.

The exact diagnostic command was the following, with `ROOT`, `MODE`, and `INPUT` taking the values below (no `cargo run` startup time is included):

```sh
target/debug/flick-transfer --state-dir "$ROOT/$MODE-$RUN/state" \
  --download-dir "$ROOT/$MODE-$RUN/downloads" --public --transport "$MODE" \
  benchmark "$INPUT" --window-seconds 120
```

For the HTTPS input, `ROOT=/tmp/flick-public-comparison-cxp8oizq` and `INPUT=https://webtorrent.io/torrents/sintel.torrent`. For the magnet input, `ROOT=/tmp/flick-magnet-comparison-emm3tk3r` and `INPUT=magnet:?xt=urn:btih:08ada5a7a6183aae1e09d831df6748d566095a10`. `MODE` was `tcp-only` or `tcp-and-utp`, and `RUN` was `1` or `2`. The magnet hash was independently extracted from the fetched torrent's raw info dictionary with Python's SHA-1 implementation.

Raw elapsed seconds below start after session initialization. HTTPS metadata time includes fetching and parsing the `.torrent`; magnet metadata time requires peer metadata exchange. First verified bytes, completion, and live peers are sampled every 250 ms, so timestamps have that observation granularity. Every run exited successfully after verifying all **129,302,391 bytes**. Peak counts refer to live connections, not all discovered addresses. Live peers were zero in every completion snapshot after the engine closed download connections.

| Input | Mode | Run | Started UTC | Metadata (s) | First verified (s) | Complete (s) | Peak live peers |
| --- | --- | --- | --- | --- | --- | --- | --- |
| HTTPS | tcp-only | 1 | 2026-10-24T19:50:38.327080+00:00 | 1.310116839 | 3.020489838 | 15.830994119 | 40 |
| HTTPS | tcp-and-utp | 1 | 2026-10-24T19:50:57.729253+00:00 | 1.166109137 | 3.069033728 | 15.925069505 | 40 |
| HTTPS | tcp-only | 2 | 2026-10-24T19:51:14.870466+00:00 | 1.295604750 | 2.650823181 | 16.308498002 | 39 |
| HTTPS | tcp-and-utp | 2 | 2026-10-24T19:51:32.395585+00:00 | 1.178779639 | 3.351984547 | 14.645966774 | 40 |
| Magnet | tcp-only | 1 | 2026-10-24T19:52:08.513763+00:00 | 1.038129572 | 2.285118184 | 18.061256893 | 39 |
| Magnet | tcp-and-utp | 1 | 2026-10-24T19:52:27.795379+00:00 | 1.303690219 | 2.692355322 | 13.791851801 | 31 |
| Magnet | tcp-only | 2 | 2026-10-24T19:52:46.275931+00:00 | 0.999187247 | 3.043522763 | 17.812040816 | 39 |
| Magnet | tcp-and-utp | 2 | 2026-10-24T19:53:05.468146+00:00 | 1.061685472 | 2.338047551 | 14.677042284 | 31 |

Mean completion time was 16.069746061 seconds for TCP versus 15.285518140 for TCP+uTP with HTTPS metadata, and 17.936648855 versus 14.234447043 with magnet metadata. TCP+uTP was faster in the magnet trials and comparable in the HTTPS trials. Public desktop and diagnostic sessions now default to `TcpAndUtp`; local sessions retain TCP. In 9.0.1 the same uTP listener socket enables outgoing uTP while outgoing TCP stays enabled; no separate outgoing-uTP option exists. Mixed-mode logs confirmed TCP and UDP listeners on the same port. These results do not isolate how many bytes traveled over each protocol.

The uTP engine warned that its receive buffer was 8,388,608 bytes rather than the requested 167,772,160 bytes; `net.core.rmem_max` and `net.core.wmem_max` were both 4,194,304. No kernel settings were changed. All eight download directories, including downloaded media, were deleted after the processes stopped. Raw JSON samples and engine logs remained in the two task-owned `/tmp` roots during this session; no downloaded media is committed.

#### Checks and scope

- `cargo build --locked --no-default-features --bin flick-transfer`: passed (4m 52s). This builds the backend diagnostic, not the desktop.
- `cargo test --locked --no-default-features --all-targets`: passed, 10 unit tests and four local transfer tests. Generated local fixtures and explicit loopback peers covered sharing, selected-file download, pause/restart/resume, byte verification, invalid inputs, actionable metadata timeout, and no public state files in local mode.
- `cargo fmt --all -- --check`: passed.
- `cargo clippy --locked --all-targets -- -D warnings`: passed (7m 39s). Cargo also printed a nonfatal future-incompatibility notice for dependency `proc-macro-error2` 2.0.1.
- Final `cargo clippy --locked --no-default-features --all-targets -- -D warnings`: passed after the correction below.
- `cargo test --locked --all-targets`: passed, 32 tests (10 shared-library, 18 desktop, four local-transfer tests); four native libmpv playback tests remained ignored. The desktop cancellation test passed against a generated fixture and a stalled explicit loopback peer.
- Final `cargo test --locked --no-default-features --all-targets`: passed again, 14 tests (10 unit, four local-transfer tests).
- `cargo build --locked`: passed; the Linux desktop application and diagnostic linked successfully. Cargo elapsed times included waiting on the same worktree's build lock. A linked desktop binary is not evidence of interactive playback.
- Initial `cargo clippy --locked --no-default-features --all-targets -- -D warnings` found `collapsible_if` in the new startup code. The conditional was corrected before final checks.
- An exploratory `cargo check --locked --all-targets` was interrupted during dependency compilation (exit 130) to build the diagnostic first. It is not a passing desktop check.

The desktop resolve notice now says “Finding peers…” and offers Cancel; the worker waits up to five minutes and acknowledges cancellation after aborting the old task. Automated cancellation coverage uses a generated fixture and a stalled explicit loopback peer. This session has not exercised that button interactively or run desktop playback.

#### Web-seed research (item 31, research only)

The [upstream release list](https://github.com/ikatson/rqbit/releases) and GitHub latest-release API returned v9.0.1 (published 2026-08-20); no newer upstream release was found. [Issue 500](https://github.com/ikatson/rqbit/issues/500) remains open for BEP 19 web seeds. Its discussion links a separate fork implementing HTTP sources, not a newer upstream release. The pinned source has no `url-list` downloader. The dependency was not changed. The Sintel metadata's web-seed URL was present but these measurements used BitTorrent peers; the separate URL-download backend owns direct Archive HTTPS downloads.

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

The repeatable software-path baseline is `FLICK_AUDIO=null bash scripts/benchmark.sh`: it generates a local 12-second 3840×2160/30 fps H.264 `testsrc2` fixture, builds/runs the release desktop binary with `--locked`, loops for 300 seconds, and removes the media afterward. Use `bash scripts/benchmark.sh 60 --size 1920x1080` for a shorter comparison, or `target/release/flick --benchmark VIDEO --seconds 300 --size 3840x2160 --interval-ms 16` for an existing local file. Set `FLICK_HWDEC=no` for a software-decoder comparison; the default is `auto-copy`. Keep fixture encoding, target size, decoder, interval, release settings, and machine load fixed when checking regressions. Repeat runs on an idle machine before setting numeric regression thresholds.

The measurement begins after the first software frame; startup is reported separately. Hardware-decoder status is captured both at that first submission and at the end, because initial auto-probing can report a candidate before falling back. The main `hardware_decoder` field is the final status. A deadline-based render tick defaults to 16 ms, skipping overdue ticks without catch-up bursts. `frames_presented` counts `Player::render` results containing a frame, not distinct decoded frames or visible display scanouts. `expected_frames` is source fps × measured wall time (an estimate for variable-rate sources); its difference from submissions is not an mpv dropped-frame counter. `render_call_ms` includes calls returning no frame, while `frame_render_ms` covers only frame-producing calls; both use nearest-rank p50/p95/p99/max. Rendering may overrun the requested duration by one call.

JSON CPU user/system seconds are process `getrusage` deltas during measurement, including libmpv worker threads; 100% average CPU means one occupied logical CPU, so values may exceed 100%. Linux RSS comes from `/proc/self/status` at roughly one-second intervals and the endpoints: the average is a sample-and-hold time-weighted estimate, and its covered time is reported. Sampling retains the current output frame, but does not model GPUI retaining/uploading it. Process peak RSS comes from `getrusage` and includes startup; sampled peak covers the measured window. RSS is reported in bytes. Other Unix platforms retain CPU and peak RSS but report sampled/average RSS as unavailable; Windows resource metrics are unavailable. Unavailable values are JSON `null`, never invented zeroes. The build profile label follows the normal debug-assertions setting (standard Cargo debug/release builds).

Drop counters include the initial and last mpv values, observed positive increases, observed decreases/resets, and unavailable-sample counts. Seeking/looping can reset mpv counters; per-tick accumulation preserves observed increases across detected resets but can miss drops at an unsampled boundary or a reset that catches up before the next sample. Treat these as observed lower bounds, not exact end-to-end frame loss. Property semantics follow the [mpv 0.37 manual](https://github.com/mpv-player/mpv/blob/v0.37.0/DOCS/man/input.rst).

This harness measures decode, software scaling/composition, output allocation, and polling overhead. GPUI upload/composition, interactive responsiveness, display scanout, audio synchronization, paused CPU, seek latency, native GPU presentation, and power still require separate native measurements. The sustained Linux baseline is recorded above; no VLC/mpv comparison was measured. Do not report FPS from a screen refresh callback as decoded FPS, or compare a CPU fallback against hardware decoding without identifying the difference.

## Upstream references

- [Published GPUI 0.2.2 package and feature dependencies](https://docs.rs/crate/gpui/0.2.2/source/Cargo.toml)
- [Published GPUI 0.2.2 native shader build requirements](https://docs.rs/crate/gpui/0.2.2/source/build.rs)
- [Published GPUI 0.2.2 macOS setup instructions](https://docs.rs/crate/gpui/0.2.2/source/README.md)
- [Zed Linux dependency installer at the package's recorded revision](https://github.com/zed-industries/zed/blob/69e2130295c2649963eb639fc70b4f2ee8ea1624/script/linux)
- [GPUI 0.2.2 BGRA image contract](https://docs.rs/crate/gpui/0.2.2/source/src/assets.rs)
- [libmpv rendering API](https://github.com/mpv-player/mpv/blob/master/include/mpv/render.h)
