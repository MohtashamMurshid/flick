# Validation and platform support

Flick targets Windows, macOS, and Linux. A CI configuration is not evidence of a passing native build, and a successful build is not evidence of working playback. Record actual results below as checks run.

## Current evidence

| Platform | Native compilation and tests | Desktop playback | Native GPU video path |
| --- | --- | --- | --- |
| Linux | Native debug build, formatting, strict Clippy, 244 ordinary tests and expanded native backend tests pass on 2026-10-25 | Xvfb/Openbox session exercised for the review backlog and in all three interface styles; null audio | Deferred; hardware decoding unverified without a GPU |
| Windows | Workflow configured; not run here | Pending native machine | Deferred |
| macOS | Debug compilation, formatting, strict Clippy, 23 tests, and an opt-in native resolution test pass on 2026-10-25 | Library/search/resume and public Sintel playback observed; CoreAudio output active | Deferred |

The presentation adapter transfers decoded BGRA frames through CPU memory into GPUI images. It sizes frames to physical video bounds without a fixed resolution, axis, or pixel-budget cap. Checked native dimensions and allocation failures produce explicit errors. GPU rendering of those images does not establish hardware video decoding or zero-copy presentation. Treat this as a functional prototype. Hardware decoding, efficient GPU surface sharing, HDR correctness, battery use, and claims of being faster than VLC remain unverified.

### Performance and deletion implementation on 2026-10-25

The approved audit was implemented using delegated Codex workers for state/persistence, library UI, native playback/thumbnails, filesystem reconciliation/deletion, services/torrents, playback actor validation, and HTTP resume correctness. The application keeps libmpv on its owning worker, keeps transfer verification separate from playback, retains native physical render dimensions, and uses OS Trash after confirmation. Implementation evidence and reproducible library measurements are in [the implementation report](performance-implementation-2026-10-25/report.md).

Executed checks so far (final review/reruns are recorded below):

- **Integrated ordinary tests:** `cargo test --locked --all-targets` passed twice after integration: **51 library + 118 desktop unit + 9 generated-local transfer integration = 178 passing; 13 opt-ins ignored**. The final recorded pass took 2.89 s / 4.83 s / 17.55 s for those suites. These are backend/unit tests, including desktop-binary units, not interactive playback. Subsequent HTTP/actor refinements require the final rerun below.
- **Desktop compilation:** `cargo build --locked --bin flick` passed in the debug profile, including the real deletion and service release hooks. Multiple earlier checks failed on missing hooks/event match arms during concurrent implementation; those were resolved. One intermediate test build exposed a main-owned path borrow typo from a lint cleanup; fixed before the integrated pass. No release desktop build is claimed here.
- **Strict lint:** `cargo clippy --locked --all-targets -- -D warnings` passed after the first integration. Earlier lint identified unused synchronous diagnostic APIs, UI borrows/conditionals, actor unwrap and test-initializer issues; corrected. A later `--no-default-features --all-targets` run was blocked by concurrent HTTP `needless_return`; an intermediate no-default test run also failed a cancellation assertion during that HTTP rewrite. Final results supersede these intermediate attempts below.
- **State/persistence:** actual-source standalone harness `/tmp/flick-state-checks-be5xewgx` passed **23** state tests; `cargo test --offline --lib library::tests -- --test-threads=1` passed **5**. Covers indexed semantics at 100/1k/5k/10k, identity tombstones/restores, ordered/coalesced durable saves, atomic replacement, stalled writer, latest-write wins, shutdown/flush/error paths. Integrated state tests are included in the desktop unit suite.
- **Library UI:** exact-source pure harness `/tmp/flick-library-ui-focused-check` passed **11** tests, including virtual ranges through 10k, filtering/cache invalidation, stable focus IDs, arrow/Tab targets, scroll restoration, bounded poster LRU and generated import validation. It extracts unchanged app-independent code and imports actual state/model/theme/shortcuts plus pinned GPUI. It does not prove GUI focus or image presentation.
- **Native frame/thumbnail backend:** actual-source harness `/tmp/flick-playback-checks-3elf9lk1` passed **20** ordinary tests and strict Clippy. A generated 12-second 1280×720 H.264/AAC fixture with subtitles was made with `scripts/make-test-video.sh`. With `FLICK_AUDIO=null FLICK_TEST_VIDEO=/tmp/flick-playback-implementation-20261025.mp4`, **11 native opt-ins passed** (143.06 s); after refinements the ordered-controls test and three real thumbnail/preview tests passed again. Covers bounded atomic latest-frame delivery/invalidation/drop, native operation order/errors/paused exact seek, protocol streams, replacement/subtitles/EOF, generation/idle teardown, preview cancellation/reopen, disk quotas/ownership and eviction notifications. Direct/aligned BGRA bytes matched at 1080p, nonaligned rows and 4K; exact 10000×2 and 7680×4320 output dimensions passed using the generated 720p source. This does not establish decoding an 8K source, a native GPU surface, hardware acceleration, audible playback or desktop scanout.
- **Filesystem backend:** actual-source harness `/tmp/flick-files-checks-jfky6ciu` passed **22** tests (3.38 s). Includes real notify delivery after unlink, stable readable parent/two probes, absent/remounted/network/offline roots, actual permission denial, replacement/cache changes, bounded mailboxes/backpressure, per-path incarnation and stale-result guards, asynchronous prepare/focus reconciliation, and isolated XDG Trash preserving a sibling fixture. Focused Clippy passed with harness-only `-A dead_code`; no owned-source suppression. Temporary UI API checks passed; they were not a desktop build/playback result.
- **Transfer/service backend:** final worker checks passed **9 services + 6 torrent metadata + 9 integration = 24** tests. Commands: actual-source service harness `/tmp/flick-services-checks-qfsibg68` with `--no-default-features --bin flick-services-checks services:: -- --test-threads=1`; `cargo test --offline --no-default-features --lib torrent:: -- --test-threads=1`; `cargo test --offline --no-default-features --test transfers -- --test-threads=1`. A separate exact-source torrent harness repeated **6 + 9** to avoid concurrently unfinished HTTP edits. Uses generated fixtures and explicit loopback peers, no trackers/DHT. Exercises startup roots/partial restore rejection, queue bounds/typed failure replies, delayed readiness with search/fetch/cancel independent, release keeping every file, progress/completion gates, nonblocking shutdown/HTTP cleanup and state-lock release despite retained player Arc. No interactive transfer/playback combination was used in this performance pass.
- **HTTP/discovery initial backend checks:** `cargo test --locked --lib fetch::http::tests -- --test-threads=1` passed **10** and `cargo test --locked --lib catalog::tests -- --test-threads=1` passed **4**; strict library Clippy passed. Includes a 16 GiB sparse partial file opened without scanning its bytes, cancellable hashing and bounded expiring successful search results. Persistent resume identity refinements are covered by final checks below.
- **Release source-helper measurement:** `python3 docs/performance-implementation-2026-10-25/measure-library.py` passed with three invocations and 100/1k/5k/20k generated records. It builds actual indexed Store APIs at release opt-level 3 with bin LTO disabled; it does not build the release desktop. At 1k, metadata batch medians were **0.852–1.829 ms**, sort/filter **0.326–0.677 ms**, Continue Watching **0.315–0.726 ms**, changed-settings queue work **0.718–0.819 ms**. At 5k metadata was **4.53–7.58 ms**; at 20k **29.2–45.3 ms**. See raw JSON and the audit's separately retained baseline. Synchronous unchanged saves now deduplicate; do not compare their timings to old durable changed writes. Queue timing excludes background serialization/fsync.
- **Actual desktop sessions:** task-owned Xvfb/Openbox, Mesa Lavapipe, debug app, GNOME dark, null audio, generated local 12-second fixture, isolated settings/data/downloads and `FLICK_LOCAL_TRANSFERS=1`. An initial launch failed with `NoSupportedDeviceFound` because the test script pointed at a nonexistent `lvp_icd.x86_64.json`; corrected to installed `lvp_icd.json`. Successful sessions showed actual moving colored frames. Preserved pre-change debug binary and implemented binary used the same 1100×740 window and 1k hard-linked local videos/history. XTest Back-click to X11 sidebar pixel transition (roughly 2 ms polling) measured **15.79 / 8.73 / 20.74 s before** for All Videos and **341 / 259 / 349 ms after**. A separate baseline Home return took 7.12 s and is excluded from that All Videos comparison. CPU contention was uncontrolled; this is an X11 software-display diagnostic, not physical display scanout, typical user latency, release acceptance or a whole-app 10× guarantee. After-only GPUI callback spans corroborate tree/render latency; old binary lacks those spans.
- **Interactive deletion:** external unlink of the selected generated file removed its entry and history, persisted a tombstone, and updated count 1000→999 within **1.24 s**. Visible Remove removed another entry and kept its disk file (999→998). Visible Delete opened a confirmation identifying the selected file; Move to Trash removed only that file/entry in about **347 ms**, preserving a neighboring file (998→997). Restart with the resulting settings retained **997 entries and three tombstones**, with no resurrection. The filesystem unit's real Trash test uses isolated XDG state; the first interactive selected fixture used system Trash. Only generated task fixtures were deleted. No real user media was touched.
- **Interactive PiP/idle:** PiP displayed moving frames while All Videos remained visible. During a five-second trace window, the small video surface rendered **150 times and the root/library rendered zero times**. After returning and pausing in the library, a ten-second interval had **zero root renders, unchanged settings mtime and 0.25 process CPU seconds**. This is a short diagnostic, not ten-minute idle/power acceptance. Captures and raw measurements are linked from the report.

Additional executed checks:

- **Actor backend:** retained actual-source harness `tests/playback_actor_backend/Cargo.toml` passed **8 actor units + 7 deterministic worker tests + 5 native actor tests + 1 native benchmark smoke**. Native command: `FLICK_AUDIO=null FLICK_TEST_VIDEO=/tmp/flick-playback-implementation-20261025.mp4 CARGO_TARGET_DIR=/tmp/flick-actor-target cargo test --offline --locked --manifest-path tests/playback_actor_backend/Cargo.toml --bin flick-actor-checks actor_ -- --ignored --test-threads=1 --nocapture`. Five native checks took 5.78 s. Tests cover 10k coalesced paused scrubs/final exact 8.25 s position/pixels, stale load/errors/generations, protocol stream release before deletion acknowledgments, EOF preservation/stop reset, optional property compatibility versus required failures. Both real and mock harnesses pass strict Clippy; native smoke submitted 59 320×180 software frames over two seconds. These are backend checks, not desktop playback or release throughput. Native teardown remains cooperative with no hard deadline for a stalled call.
- **HTTP final focused checks:** `cargo test --offline --no-default-features --lib fetch::http::tests -- --nocapture` passed **21**; `cargo clippy --offline --no-default-features --lib --tests -- -D warnings` passed. Tests include persisted first-request If-Range, changed same-size objects and complete leftovers, eligible Last-Modified, validators/resource mismatch and unexpected 206/416, foreign/malformed metadata bounds, symlink/hard-link/replacement ownership, per-part lock, cancellation/checksum cleanup and sparse metadata-only opening. Non-Unix checksum-free persisted partials restart conservatively. Unix identity checks are not an adversarial filesystem security boundary or a full power-loss guarantee.
- **5k interactive library:** All Videos rendered, scrolled to additional virtual rows, switched to List and showed keyboard focus. Back to this 5k grid measured **411 ms** in one diagnostic X11-pixel trial, with the after-only GPUI callback at 389 ms. This is an after-only scale check, not a matched 5k baseline comparison or exhaustive keyboard navigation acceptance.
- **Dependency isolation:** `cargo tree --locked --no-default-features -e normal --prefix none`, checked for absence of GPUI/image/notify/trash, passed.

**Pre-review integrated check sequence:** all seven checks passed: root `cargo fmt --all -- --check`; retained actor-harness formatting; `cargo clippy --locked --all-targets -- -D warnings`; `cargo test --locked --all-targets` (**62 library + 119 desktop units + 9 generated-local transfer integration = 190 passed**, 13 ignored); the same strict Clippy with `--no-default-features`; `cargo test --locked --no-default-features --all-targets` (**62 + 9 = 71 passed**, one ignored); `cargo build --locked --bin flick` (debug desktop). Raw command/exit results: [final checks](performance-implementation-2026-10-25/final-checks.json). The existing upstream `proc-macro-error2` future-compatibility notice remains. No successful native macOS/Windows execution is inferred from these Linux checks.

Further interactive validation with the final built actor and the 5k List: Escape canceled Delete and kept the selected file; reopening Delete and pressing Tab then Enter confirmed Trash. The previously opened/paused file was released, moved to isolated XDG Trash, removed from the library (5000→4999), and its sibling remained. Completion/persistence was observed in about 930 ms. An automated scrub attempt ran before the first loaded frame and is **not** counted as interactive scrubbing acceptance; exact scrubbing is covered by the native actor test. That loading capture exposed a layout regression: the retained empty video surface occupied the placeholder's flex row and pushed its label right. The surface was put in an absolute wrapper; targeted strict lint/build and visual recheck are recorded below.

Claude independent design/code review round 1 returned changes requested; [the full review](performance-implementation-2026-10-25/claude-review.md) is preserved. Subsequent reviews, fixes and their evidence are recorded below.

### Performance review fixes and final checks on 2026-10-25

Claude round 1 reviewed design and code and requested B1 (startup failure), H1 (filesystem startup backlog), H2 (visible poster thrash), H3 (unverified torrent siblings), H4 (Trash backends), and smaller lifecycle/correctness fixes. Responses are in [review-responses.md](performance-implementation-2026-10-25/review-responses.md). Unknown transfer ownership still cannot justify raw reads. Missing parent folders remain visibly Unavailable; Remove is available. HTTP reconnects without trustworthy validators restart conservatively. Main-player chrome still renders per frame; the measured PiP library isolation remains the narrower claim.

Executed after follow-up integration:

- **Integrated tests:** `cargo test --offline --all-targets -- --test-threads=1` passed **66 library + 147 desktop-binary units + 9 local transfer integration = 222 tests**, 13 opt-ins ignored (5.72 / 30.39 / 33.37 s suite execution). The final UI guard refinements (disable raw previews of active streams, reject direct reads of unselected/incomplete owners, discard old roots on Checking) were followed by `cargo test --offline --bin flick -- --test-threads=1`: **147 passed**, 12 ignored, 29.84 s. An earlier integrated run compiled before the worker updated two shutdown fixtures and failed those two deadline assumptions; the passing reruns supersede it. The tests now explicitly start local Share rather than relying on readiness-only Load to create a session.
- **Without desktop:** `cargo test --offline --no-default-features --all-targets -- --test-threads=1`: **66 + 9 = 75 passed**, one ignored (7.44 / 33.68 s). Strict `cargo clippy --offline --all-targets -- -D warnings` and the same command with `--no-default-features` both passed. Root and retained actor harness `cargo fmt --all -- --check` passed. `cargo build --offline --bin flick` passed on final sources (debug). `cargo tree --offline --no-default-features` contains no GPUI/image/notify/trash; `git diff --check` passed. An intermediate Clippy compile caught a concurrently edited service HashSet tuple-reference type, corrected before final checks. [Commands and exit results](performance-implementation-2026-10-25/review-final-checks.json).
- **Durable quarantine/state:** `CARGO_TARGET_DIR="$PWD/target" cargo test --offline --manifest-path /tmp/flick-state-checks-be5xewgx/Cargo.toml --bin flick-state-checks store:: -- --test-threads=1`. **27 focused tests** passed, including positive-location roundtrip, quarantine across reload/Open/import/removal, exact verified clearing, tombstones versus accepted admission and generated multi-file sibling protection. Strict focused Clippy passed. Final integrated Store/Writer tests additionally cover a background-owned durability fence waiting for actual disk completion and waking after worker exit.
- **Filesystem:** actual-source harness `/tmp/flick-files-review-checks`, real core dependency (no FileLocation shim), passed **34** tests, then strict Clippy with harness-only `-A dead_code`, formatting and whitespace checks. Command: `CARGO_TARGET_DIR="$PWD/target" cargo test --offline --manifest-path /tmp/flick-files-review-checks/Cargo.toml library_files::tests -- --test-threads=1 --nocapture`. A generated 10k startup delivered Missing in **1.654 s with all 9,999 initial positive proofs undrained**. Includes proof/delta coalescing, metadata-only versus inode replacement, injected watcher recovery/focus throttling, real permission denial, stable missing proofs, isolated native Linux Trash, no-overwrite/raw-path metadata, and actual /tmp→/dev/shm cross-device refusal preserving the source. Mac NSFileManager selection was checked against pinned API only; no macOS/Windows native execution. Missing-parent fallback intentionally remains Unavailable.
- **Startup/services:** final private-target actual-source service harness command `CARGO_TARGET_DIR=/tmp/flick-services-checks-qfsibg68/target cargo test --offline --manifest-path /tmp/flick-services-checks-qfsibg68/Cargo.toml --no-default-features --bin flick-services-checks services:: -- --test-threads=1` passed **16**, 18.28 s. Same harness `--lib torrent::` passed **10**, 0.02 s. Strict Clippy with `--lib --bin flick-services-checks --tests -- -D warnings`, formatting and whitespace passed. Covers malformed saved state, deleted output conservative boundaries, held locks/Retry, unknown state never sending false empty ownership, valid absent release without starting an engine, selected admission ordering/re-download and finalizer/HTTP cleanup. All generated local fixtures; transfers explicit loopback, no public trackers/DHT.
- **Seek backend:** retained `tests/playback_actor_backend/Cargo.toml` in `/tmp/flick-actor-target` passed **28 mock tests**, **22 ordinary native-module units**, and **5 actual libmpv actor opt-ins** (33.65 s). Native command: `FLICK_AUDIO=null FLICK_TEST_VIDEO=/tmp/flick-playback-implementation-20261025.mp4 CARGO_TARGET_DIR=/tmp/flick-actor-target cargo test --manifest-path tests/playback_actor_backend/Cargo.toml --bin flick-actor-checks native_actor_ -- --ignored --test-threads=1 --nocapture`. Actual paused drag held six seconds, exact release, 10k scrub flood, **30 repeated exact seeks** sampled every 2ms after native command reply and for at least500ms after a changed native frame, no-op retirement/unpause, failure recovery, generation/EOF/stop/custom-stream release. New deterministic fence tests include silent rejection recovery, live/nonseekable bypass and delayed properties. An initial new test assertion was corrected to wait for native state instead of optimistic admission. These are backend playback checks with generated local media/null audio, separate from transfers and GPUI interaction.
- **Shutdown:** ordinary quit retains a coalesced async finalizer before cx.quit; OS quit uses a final synchronous first-poll barrier after GPUI windows.clear/flush_effects. Service finalization uses separate registration/join locks and serializes overlapping finalizers. Eleven focused checks passed before the later 16-test service suite; actual debug build and strict lint passed. Native termination can exceed GPUI's 100ms future budget deliberately after UI teardown. prepare_quit now queues pause/idle immediately. No hard native-call or cleanup deadline, macOS execution, or app.run-return assumption is claimed.
- **Search/layout:**12 focused library tests and18 native GPUI mock-layout cases passed for bounded notices/search and independent virtual grid scrolling, keyboard paging/HomeEnd, full link/chooser scrolling and pending-open Cancel. These are layout/backend evidence, not actual online results received. No public search/transfer was run.
- **Fresh GUI measurements:** final review-integrated debug desktop at1k gave **298 / 317 / 481ms Back→All Videos**, [raw samples](performance-implementation-2026-10-25/final-back-measurements.json). Median baseline15.79s / final0.317s ≈50× for this diagnostic; the earlier after-trials were259/341/349ms. Ubuntu Xvfb/Openbox/Lavapipe, null audio, uncontrolled CPU contention, X11 pixel detection; no physical-scanout or whole-app10× claim.
- **Fresh GUI deletion/startup:** at relaunch with three generated records and persisted positive parent/volume proof, a known missing file was pruned and tombstoned; a missing entry with deliberately mismatched saved volume remained visibly **Unavailable**. Final Linux confirmed Trash moved only the remaining generated local file into isolated XDG Trash in about41ms at filesystem polling; the library then updated and retained its removal marker. Screenshots: [startup proof](performance-implementation-2026-10-25/startup-proof.png), [confirmation](performance-implementation-2026-10-25/final-delete-confirm.png), [result](performance-implementation-2026-10-25/final-delete-result.png). Native Trash move timing is not a paint latency.
- **Fresh GUI transfer startup:** syntactically valid but malformed saved records with complete canonical boundary evidence displayed an unavailable message/Retry and still played a generated local video outside those roots. Invalid JSON (unknown coverage) displayed Retry, a waiting notice and Cancel on open. After repairing this isolated fixture to an empty known state, Retry opened the pending local video. No torrent transfer or external request was involved. [Unavailable](performance-implementation-2026-10-25/download-startup-unavailable.png), [safe local playback](performance-implementation-2026-10-25/safe-local-playback-after-startup-failure.png), [unknown-state feedback](performance-implementation-2026-10-25/unknown-startup-pending.png). Normal Ctrl+Q exited every completed supervisor with status0.

**Large display / 5k idle:** final recorded actual GUI trial used a3840×2160 client at1×,5,000 distinct media and poster paths,180 visible posters (162 complete/18 clipped). After60s settling,60.001s had **zero root renders, zero video-surface renders, zero poster opens/accesses**, identical before/after pixels; CPU2.15% of one core, RSS577,536→576,592KiB. Ctrl+Q exited0 and private display:93 was cleaned up. [Raw final data](performance-implementation-2026-10-25/large-display-idle.json), [screenshot](performance-implementation-2026-10-25/large-display-after.png). This tests the pinned viewport working set beyond96 entries, not playback/physical display. Three60s observations are retained; the first measurement had unverified shutdown (supervisor143), and the second passed normal quit on an earlier binary; both remain explicitly labeled. The authoritative final running SHA-256 is `f1f4dcc529588c128537635052e104b281605d288e34d6d19fc9818c8921eab9`; later Remove/quit-only changes did not alter the poster cache. No source changes/builds by this GUI worker. IPv4/IPv6 sockets blocked in its app test environment; local X11 only.

**Claude's executed round-1 checks:** source-only review plus actual-source scratch harnesses, not a desktop build/playback acceptance. `/tmp/claude-review-files-FYn164`, `CARGO_TARGET_DIR=/tmp/claude-review-target cargo test --offline review_ -- --test-threads=1 --nocapture`: before FileLocation2 passed (unknown missing pruned; old global revision rule accepted0/dropped21 over6s); after FileLocation2 passed (unknown missing nowUnavailable); after per-file epochs3 passed (Missing accepted1.0s while another path changed). Later harness uses a one-type FileLocation shim. `/tmp/claude-review-actor-hF8Orq`, seven ignored `review_paused_drag` runs against system libmpv0.37/nullaudio/generated fixture: keyframe seek6.0s landed0.0s7/7, pre-seek position briefly returned1/7 drags and1/21 exact seeks, exact release reachedtarget7/7. `ffprobe -skip_frame nokey` found fixturekeyframes0 and8.333s. These observations drove the seek fence. No public network/trackers/DHT or repo Cargo builds by Claude.

### Round 2 follow-ups and frozen final checks on 2026-10-25

[Claude round 2](performance-implementation-2026-10-25/claude-review-r2.md) requested another correction pass. [The response matrix](performance-implementation-2026-10-25/review-responses-r2.md) maps every Blocking/High/Medium finding to a fix or explicit limit. In particular, UI **and backend** now refuse removal while Checking, before new quarantine marks can trap complete siblings; an owner is retained until it can report verified progress. The source was then frozen and hashed for a focused Claude round 3. Source changes since that hash: none as of this check.

Final integrated execution on this frozen source:

- `cargo test --offline --all-targets -- --test-threads=1`: **76 core + 156 desktop-binary units + 9 local transfer integration = 241 passed**, 13 opt-ins ignored. Suite durations88.50 /33.76 /36.82s. This includes new standard v1 SHA-1 construction/cancellation, readiness epoch/admission snapshots, Checking refusal, native Trash compatibility/priority lanes, batched membership removal and retained regression suites. Tests are backend/unit evidence, not GUI playback.
- `cargo test --offline --locked --no-default-features --all-targets -- --test-threads=1`: **76+9=85 passed**, one ignored,88.49 /34.22s. Strict Clippy passed with all targets both with desktop and with `--offline --locked --no-default-features`. Root and retained actor-harness formatting passed. `cargo build --offline --bin flick` passed (debug). `git diff --check` and final dependency isolation passed. [Command/exit record](performance-implementation-2026-10-25/final-checks-r3.json). The upstream proc-macro-error2 future-compatibility notice remains.
- **Native seek/stop backend:** retained and private actual-source harnesses passed **36 deterministic mocks**, **24 nonnative backend units**, and **10 libmpv actor tests** (71.46s native execution), plus strict Clippy and formatting. Command: `FLICK_AUDIO=null FLICK_TEST_VIDEO=/tmp/flick-playback-implementation-20261025.mp4 FLICK_TEST_LOW_FPS=/tmp/flick-seek-review-20261025/low-fps-10.mp4 CARGO_NET_OFFLINE=true CARGO_TARGET_DIR=/tmp/flick-actor-target cargo test --manifest-path /tmp/flick-seek-review-20261025/candidate/tests/playback_actor_backend/Cargo.toml --bin flick-actor-checks native_actor_ -- --ignored --test-threads=1 --nocapture`; retained mocks via `--locked --manifest-path tests/playback_actor_backend/Cargo.toml --bin flick-actor-mock-checks`. Fixtures generated locally at30fps and10fps. Tests include advancing1×/4× seeks, speed/pause changes while waiting, off-frame10fps seek/frame-step/chapter commands, **20 descriptor-close trials**, and a deliberately blocked protocol opener: stop remains pending until accepted END_FILE and actual stream closure. No transfer/network session. Earlier candidate tests deliberately reproduced frozen-source delayed-restart and changed-control failures; initial native deadline was corrected to wait for native progress instead of command reply. Native tests are not GPUI/physical scanout or audible playback.
- **Share/readiness/removal backend:** actual-source private `/tmp/flick-services-final-review` passed **76 core,19 service,9 transfer** tests, plus final strengthened identical-snapshot regression. Commands used `CARGO_TARGET_DIR=/tmp/flick-services-checks-qfsibg68/target cargo test --offline --manifest-path /tmp/flick-services-final-review/Cargo.toml --no-default-features --lib --bin flick-services-checks -- --test-threads=1`, then `--bin flick-services-checks --test transfers`, then the filtered `readiness_and_acceptance_republish_unchanged_snapshots_with_expected_completions`. Includes repeated empty Retry and lazy resolve restoring ownership knowledge, identical accepted snapshots resent with intended completion dedup, deterministic Initializing refusal retaining owner/bytes, standard2MiB cross-file piece hashes/trailing empty files/symlink refusal, and a **64GiB sparse** source paused after a real64KiB read: abort/session shutdown cancels the worker within2s, admits no engine and leaves source/metadata untouched. The helper checks cancellation between <=64KiB reads/walking, holds no engine reference, and cannot forcibly interrupt a kernel call. A new multifile Share sorts entries deterministically and may generate a different infohash from the old unsorted walk; existing saved metainfo is unchanged. Generated local fixtures and explicit loopback peers only; no public discovery.
- **Filesystem priority/Trash backend:** **37** real-source tests passed,10.32s, using `/tmp/flick-files-review-checks` with the actual core (no FileLocation shim); strict focused Clippy used harness-only `-A dead_code`, and owned formatting/whitespace passed. At a real500ms drain, **10k offline entries + one Missing** produced Missing in **1.0288s** and recovery in **0.5070s**; every latest bulk state was eventually delivered. Urgent and bulk mailboxes each cap256, proofs remain separate. Native isolated home Trash at0755/0775 now works without chmod, preserving mode0640 source bytes; ownership, symlink, sticky-directory, no-overwrite and real /tmp→/dev/shm refusal tests pass. No actual FAT/exFAT/NTFS mount was available.
- **Windows recycle backend binding check:** exact optional Windows desktop dependencies windows0.61.3 and windows-core0.61.2; new actual `src/library_files/trash_windows.rs` imported by `/tmp/flick-windows-trash-check`. `cargo clippy --offline --manifest-path /tmp/flick-windows-trash-check/Cargo.toml --target x86_64-pc-windows-gnu --tests -- -D warnings -A dead_code` passed after installing the Rust target. All **three Windows API-contract tests compile but were not run**. Operation mask0x183614 requires recycle, excludes WANTNUKEWARNING; the pre-delete sink vetoes nonrecycle operations and post-delete requires success plus Recycle Bin destination. Python checked exact pinned constants (recycle transfer bit0x80). This is cross-target compile/lint, **not a Windows executable/runtime/desktop/Recycle Bin test**. macOS remains NSFileManager, not executed here.
- **HTTP backend:** private `/tmp/flick-http-review-eo_bs2b3` passed **27** tests (22.34s) and strict Clippy/format/patch checks; the integrated `cargo test --offline --lib fetch::http::tests -- --test-threads=1` repeated **27 passed** (78.69s under concurrent work). Includes bounded repeated-prefix/discard retries, eight useful range recoveries, checksum-safe changing mirrors, checksum-free URL-scoped validator refusal without looping/splicing, cancellation/locks/foreign metadata and16GiB metadata-only resume. Negative control on frozen source exceeded25s as expected; patched repeated-prefix case ends after five requests. Permission child checks at umasks0022/0027 produced0644/0640 final files, with sidecars0600; frozen control remained0600 and failed. No external HTTP; loopback-only generated fixtures, no torrent traffic.
- **Store/UI follow-up:** **30** focused integrated Store tests passed (3.66s), including10k library/1k batch removal with one invalidation and persistent tombstones, alongside prior identity, async writer/barrier and quarantine checks. A missing UI pending-queue field was caught during the first intermediate compile and fixed; final suites pass. Strict lint caught the now-diagnostic single-path compatibility API as unused; it retains an explained non-test allowance. New UI behavior dirties poster candidates on Roots/Ready/known transition, displays Needs verification, uses per-path HTTP removal generations, moves silent deferred opens to a visible waiting library, and freezes controls/open/render target while Closing.
- **Final GUI smoke:** same Ubuntu/Xvfb/Openbox/Lavapipe/nullaudio setup and generated1k library: Back **309.9ms**, retained as one smoke sample, not a new median comparison. Normal Ctrl+Q exit0. A separate one-file GUI fixture began with an externally held local state lock and no poster: poster remained absent while unavailable. Releasing that test lock and clicking Retry generated/persisted a poster in **1.31s**, without scrolling. [Waiting](performance-implementation-2026-10-25/poster-waiting.png), [recovered](performance-implementation-2026-10-25/poster-recovered.png), [raw observation](performance-implementation-2026-10-25/poster-recovery.json). No video was played during the held-lock/poster test, no transfer was started, and all state/cache/Trash paths were isolated.
- **Report checks:** every local Markdown link in the implementation report and this validation log resolves. All 44 files in the final-review SHA-256 manifest still match. T3 `html_preview` explicitly returned an unavailable sandbox error: “This host blocks the sandbox T3's browser runs in (AppArmor on Ubuntu 23.10+).” The installed Chrome headless shell rendered the standalone summary at 728 px and 390 px instead; collapsed and expanded layouts have no horizontal overflow, and the desktop capture was visually inspected. [Layout results](performance-implementation-2026-10-25/report-layout-checks.json). These are report checks, not application browser or playback tests.

**Additional checks executed by Claude round2** (backend harnesses only, no repo Cargo/desktop/interactive playback): services `/tmp/claude-review-r2-services`, `CARGO_TARGET_DIR=/tmp/claude-review-r2-target-services cargo test --offline --no-default-features --bin flick-services-checks claude_review_r2 -- --test-threads=1 --nocapture`: expected **0pass/1fail**, event sequence Checking/Roots/Ready/Progress0/Checking/Roots/Ready/Resolved demonstrated missing second Progress. A local64KiB+768MiB transfer at `/tmp/claude-review-r2-h3` reproduced Checking→release→absent snapshots with retained bytes/quarantine. `/tmp/claude-review-r2-files` ran two review tests: Missing behind Unavailable took10.1s with5k/500ms,20.4s with10k/500ms,1.3s at20ms; home Trash775 safely failed before chmod700. `/tmp/claude-review-r2-http` compared real frozen/HEAD HTTP on a loopback512KiB body cut every64KiB: stable targets completed7.1s; changing targets still looped at14s in frozen code while HEAD completed~7s; alternating ETag looped in both; cancellations cleaned folders; old/new modes664/600. `/tmp/claude-r2-actor-exp` used generated24fps60s and10fps40s clips with libmpv0.37/nullaudio:24fps playing0/30,0/30,0/20 froze at1×/2×/4×;10fps6/15 froze >=1.5s (one10.01s), paused frame steps hidden; stop acknowledged before delayed stream opening and left descriptor open10/20 and2/20 batches; raw idle preceded END_FILE10/10; pause stale-state0/40. These failures drove the fixes above. Exact original commands/results remain in [review §7](performance-implementation-2026-10-25/claude-review-r2.md). No public peers/trackers/DHT, user media or user Trash were touched by those harnesses.

**Explicit remaining limits:** R2-M3 is not fixed: an unavailable Windows volume, or a non-NotFound error on any platform (permission denial, a disconnected network mount, stale NFS or a failing disk), can leave ownership coverage uncertain and gate unrelated local opens. Ordinary Linux missing-path cases already retain a conservative existing ancestor and permit paths outside it. Visible Retry/Cancel preserves byte verification; uncertain roots are not excluded by trusting raw saved aliases. Missing parent directories remain visibly Unavailable with Remove. Native Windows/macOS execution, physical GPU/display/audio and sustained 4K throughput are unverified. The main player root still renders per frame. Existing corrupt-settings fallback, case/normalization aliases, Windows creation-time identity and forced native/kernel I/O cancellation remain follow-ups. Corrupt settings can lose quarantine evidence, so byte safety depends on that evidence surviving. There is no claim that every audit item is eliminated or the whole app is 10× faster.

### Focused Claude round 3 on 2026-10-25

[The complete review](performance-implementation-2026-10-25/claude-review-r3.md) found **no Blocking or High issues**, verified the round-2 fixes, and identified two Medium-Low deletion edge cases plus Windows error wording. These are being corrected before PR creation and a new focused review.

Executed by Claude, using generated files and no network/transfers:

- SHA-256 comparison matched all 44 frozen source/dependency files at the beginning and end. The manifest did not cover tests; the next review manifest will include the retained harness.
- A ctypes probe of system libmpv 0.37.0 (`vo=null`, `ao=null`, `idle=yes`, `keep-open=yes`) used a generated 200 KB broken MP4 and generated 160×90/10 fps H.264 clip. `python3 -I /tmp/claude-review-r3/scripts/mpv_stop_probe.py /tmp/claude-review-r3/fixtures/broken.mp4 /tmp/claude-review-r3/fixtures/ok10.mp4` showed that failed load sends END_FILE before stop and retains a playlist entry, whereas a loaded-file stop emits a fresh END_FILE.
- Private actual-source and round-2-baseline actor harnesses were built with `cargo test --offline --locked --manifest-path /tmp/claude-review-r3/actor/Cargo.toml --bin flick-actor-checks --no-run`, with separate `/tmp/claude-review-r3/actor-target-frozen` and baseline target directories. Running `review_r3 --ignored --test-threads=1 --nocapture` with `REVIEW_BROKEN`/`REVIEW_GOOD` and null audio reproduced the regression: first stop after failed load timed out at 10.005/10.003 seconds; a second succeeded in 5 ms. The round-2 actor succeeded initially in 5.1 ms. Loaded-file controls succeeded in 0.20–0.40 seconds. One shared-target run reused the baseline binary; it was discarded and rebuilt in a separate target, with embedded source paths checked.
- No integrated suite, desktop build, GUI session, FAT mount or native Windows/macOS execution ran in that review. Error-transfer release and Windows wording findings were static source/dependency checks. The subsequent fix checks are recorded separately.

### Round 3 corrections on 2026-10-25

- **Native failed-load stop regression:** `/tmp/flick-stop-r3-20261025/checks/Cargo.toml` imports the actual checkout playback files and retained harnesses, with private targets and `--offline --locked`. **38 mocks passed** (16.12 s); **11 native libmpv tests passed** (53.40 s), using `FLICK_AUDIO=null FLICK_HWDEC=no FLICK_TEST_VIDEO=/tmp/flick-playback-implementation-20261025.mp4 FLICK_TEST_LOW_FPS=/tmp/flick-seek-review-20261025/low-fps-10.mp4` and filter `native_actor_ -- --ignored --test-threads=1 --nocapture`. The new failed replacement-load first stop succeeded in 198.9/203.7/204.0 ms. The native suite retained 20 descriptor-close trials, the blocked protocol opener, stream release/reopen, low-fps/advancing seek, EOF and property-error checks. Strict all-target Clippy and owned formatting passed. The new native test against saved R3 sources deliberately failed at 11.02 s (0 passed/1 failed), confirming it reproduces the prior timeout. The reviewer's ctypes probe was rerun read-only and passed. Generated local media only; no transfers/network, desktop build or GUI playback in these worker checks. Exact pre-edit source/harness files and verified hashes are under `/tmp/flick-stop-r3-20261025/r3-baseline/`.
- **Error-owner refusal/recovery:** private `/tmp/flick-error-owner-r3-ifb2wn_a` passed **15 torrent tests**, including a deterministic actual librqbit storage Error. Repeated retained-file removals preserve owner identity, file bytes, explicit-share marks and state lock; removing the fault and calling public Resume rechecks both siblings and permits safe release. A second fixture confirms explicit whole-transfer Delete Files still works in Error. A private actual-Store extension verifies settings bytes/reload/tombstones/quarantine remain unchanged on refusal and clear only with current full verification after Resume. Exact R3 source fails the new regression with “Error owner was released: ()”, as intended. Commands: `CARGO_TARGET_DIR=/tmp/flick-error-owner-r3-ifb2wn_a/target cargo test --offline --manifest-path /tmp/flick-error-owner-r3-ifb2wn_a/Cargo.toml --no-default-features --lib torrent::tests:: -- --test-threads=1`; the `marks/Cargo.toml` and `negative/Cargo.toml` variants use filter `error_owner_refuses_retained_file_release_and_recovers_after_resume`. Owned formatting/whitespace and source checks for guard-before-quarantine ordering pass. These are backend tests, not GPUI or interactive playback. Generated local fixtures, no supplied peers and local-only configuration; no public discovery. Initial fixture compilation and persistence-factory identity issues were corrected before the passing runs. Baselines under `r3-baseline/` match all three original hashes.
- **Windows/Trash wording:** the generic failure no longer promises file preservation. Windows success without a Recycle Bin destination explicitly reports permanent deletion; unconfirmed outcomes request checking the original folder and Recycle Bin. The initial actual-source cross-target strict Clippy pass preceded the final formatting edit. The final bytes are covered by Claude round 4’s pinned-toolchain rerun below (R4-L2). No Windows native operation was run. An initial root formatting check found two layout differences in these edits; both were corrected, and owned-file rustfmt passed.
- **Verification message:** a quarantined file no longer promises that re-adding into an existing torrent folder will succeed; Resume is suggested conditionally when the owner is still in Downloads.

**Final frozen round-4 integration:** `cargo test --offline --locked --all-targets` passed **77 core + 156 desktop-binary units + 9 loopback transfer integration = 242 tests**, 13 opt-ins ignored; normal test concurrency, suite durations 23.94/9.71/18.23 s. `cargo test --offline --locked --no-default-features --all-targets` passed **77 + 9 = 86 tests**, one ignored, 23.80/18.33 s. Both strict all-target Clippy modes, the debug desktop build, root and retained-harness formatting, dependency isolation and whitespace checks pass. [Executed command record](performance-implementation-2026-10-25/final-checks-r4.json). The new review manifest covers all 49 source/dependency/test/harness files, and all still match. The previous 241/85 pass is retained as history above. Backend tests and the desktop build are separate from the GUI observation below.

**Final failed-load Delete GUI regression:** task-owned Ubuntu Xvfb/Openbox/Lavapipe, debug binary, one generated unsupported MP4, isolated settings/data/XDG Trash, null audio, local-only transfer mode with no transfer started. Open showed “Playback failed: unrecognized file format”; Back returned to All Videos; the visible Delete button opened the selected-file confirmation. Its first Move to Trash succeeded: file absent, library 1→0, one isolated Trash file with identical SHA-256, within **85.8 ms** of confirmation to disk/settings observation. This is not click-to-physical-paint timing. [Failure](performance-implementation-2026-10-25/r4-failed-load.png), [confirmation](performance-implementation-2026-10-25/r4-delete-confirm.png), [empty library](performance-implementation-2026-10-25/r4-delete-result.png), [raw result](performance-implementation-2026-10-25/r4-broken-delete.json). Ctrl+Q exited 0 and the owned display/window manager were cleaned up. The first log-based wait found no UI-only error text and expired after eight seconds; the visible error was confirmed in the screenshot instead. This exercised failed loading and deletion, not moving-video/audio acceptance. No user files or user Trash were touched.

### Final Claude review, round 4, on 2026-10-25

[The focused review](performance-implementation-2026-10-25/claude-review-r4.md) independently verified R3-M1, R3-M2 and R3-L1 and found **no Blocking, High or Medium findings**. It recommended documentation updates only before PR/merge; those are applied. No application/test/dependency bytes changed during or after review: all 49 manifest hashes matched at start, before writing and at 16:31:12 UTC.

Executed by Claude in private `/tmp/claude-review-r4/` manifests/targets, with generated local fixtures and no network/peers/trackers/DHT:

- **Native actor:** `CARGO_NET_OFFLINE=true CARGO_TARGET_DIR=/tmp/claude-review-r4/actor-target cargo test --offline --locked --manifest-path /tmp/claude-review-r4/actor/Cargo.toml --bin flick-actor-checks --no-run`; then the built binary with `FLICK_AUDIO=null FLICK_HWDEC=no REVIEW_BROKEN=…/broken.mp4 REVIEW_GOOD=…/good10.mp4 REVIEW_A=…/a720.mp4 REVIEW_B=…/b720.mp4 REVIEW_TRIALS=20` and `review_r4 --ignored --test-threads=1 --nocapture`. **2 tests passed**, 18.28 s. The failed-load first stop succeeded in 2.54/3.49/2.59 ms with its descriptor closed. Immediate replacement→stop passed **40/40** trials; zero replaced/new descriptors were open at acknowledgement. Logs: `actor-r4.log`.
- **Independent libmpv probe:** `python3 -I /tmp/claude-review-r4/scripts/mpv_replace_probe.py good10.mp4 other10.mp4 broken.mp4 15` used system 0.37.0, null output/audio, idle/keep-open and no config. **7×15 = 105** cases; zero replaced descriptors remained open when the release rule first held; 69 acknowledgements had zero accepted starts, covering the never-started case. Log: `mpv-replace-probe.log`.
- **Transfer backend:** a private copy of all 44 application/dependency files hash-matched the final source. `CARGO_NET_OFFLINE=true CARGO_TARGET_DIR=/tmp/claude-review-r4/torrent-target cargo test --offline --locked --manifest-path /tmp/claude-review-r4/torrent/Cargo.toml --no-default-features --lib torrent::tests:: -- --test-threads=1`: **15 passed**, 8.51 s, local-only configuration and no supplied peers. Log: `torrent-tests.log`.
- **Final-byte Windows cross-target lint (covering R4-L2):** `CARGO_NET_OFFLINE=true CARGO_TARGET_DIR=/tmp/claude-review-r4/wincheck-target cargo +1.99.0 clippy --offline --locked --manifest-path /tmp/claude-review-r4/wincheck/Cargo.toml --target x86_64-pc-windows-gnu --tests -- -D warnings -A dead_code` passed on the final Windows module. The earlier 15:46:55 check preceded its final 15:50:16 formatting edit; this rerun covers those final bytes. A first unpinned attempt used stable without the Windows std target and failed E0463; discarded as setup error. Actor/transfer private runs used rustc 1.99.1 stable; this Windows rerun used the repository-pinned 1.99.0. Cross-lint is not Windows execution.
- **Static review:** all eight changed production files and both harness diffs, Error UI guards before persistence/quarantine, and librqbit 9.0.1 unpause/start/previous-error/wait/fatal-error paths. Inspected worker 38-mock/11-native/negative-control logs and GUI screenshots/JSON, without rerunning those checks.

None of Claude's checks was a desktop build, GUI session or interactive playback. It did not rerun the integrated 242/86 suites, Store-marker extension, FAT mounts or native Windows/macOS behavior. The native playback and transfer checks were separate processes.

**Remaining R4-L1 behavior:** Delete and Remove · Keep Files refuse an Error owner. Resume rechecks retained bytes, then starts live transfer work; if the cause persists (for example a full disk), it may return to Error before Delete can run. Possible workarounds are Pause during checking, freeing space outside Flick, or deleting the chosen file outside Flick. Whole-transfer Delete Files remains available and deletes every listed file. A large recheck can report “File verification timed out” after 120 seconds while checking continues and later succeeds; its transfer-operation slot remains occupied during that wait. This fails closed and was accepted as a documented low-severity UX limit. Existing-folder adoption after release is not implemented; unverified siblings remain blocked unless a live transfer verifies them. Files not selected for a live download remain blocked. The old playback failure banner may remain after the failed file is deleted. No source changes were made for these nits or limits.

### PR #5: CI test-portability follow-up on 2026-10-25

The first CI runs on `6e2dc2b` exposed three test assumptions, after the Linux checks passed:

- [Windows transfers](https://github.com/MohtashamMurshid/flick/actions/runs/40212331627/job/121226042782) failed the Checking-removal fixture's raw-versus-canonical path equality (`C:\Users\RUNNER~1…` versus the canonical long path) and attempted to reserve a full 64 GiB file, returning Windows disk-full error 112 before the cancellation test began.
- [macOS transfers](https://github.com/MohtashamMurshid/flick/actions/runs/40212342149/job/121226074378) and [macOS desktop units](https://github.com/MohtashamMurshid/flick/actions/runs/40212342149/job/121226074329) failed that same assertion on `/var` versus `/private/var` temporary-directory aliases. macOS compilation and strict lint passed before the assertion failure.
- [Windows desktop lint](https://github.com/MohtashamMurshid/flick/actions/runs/40212342149/job/121226074429) rejected a path variable used only inside the Unix test branch; application compilation had passed.

The corrections are confined to the existing `#[cfg(test)] mod tests` blocks in `torrent.rs` and `library_files.rs`: compare the verified path with the canonical expected file; use a 64 MiB Windows cancellation fixture while retaining 64 GiB elsewhere; assert that the selected regular file remains present on every platform, after the optional Unix symlink case. Both fixture sizes retain work after the deterministic first-64-KiB pause. All cancellation deadlines, worker-exit/owner/byte/lock checks remain, and no tests or checks are skipped. Both production prefixes are byte-identical to the reviewed `6e2dc2b` sources. Exact pre-edit copies are under `/tmp/flick-ci-portability-baseline/`; the new 49-file manifest is `review-r5-source-hashes.json`.

Local executed checks with pinned rustc 1.99.0: `cargo test --offline --locked --no-default-features --lib torrent::tests -- --test-threads=1` passed **15**, 8.48 s; `cargo test --offline --locked --bin flick library_files::tests::trash_refuses_directories_and_symlinks_without_touching_targets` passed **1**; `cargo clippy --offline --locked --all-targets -- -D warnings` and root formatting passed. These are Linux backend/unit/lint checks, not native macOS/Windows rerun results or interactive playback. That initial round 5 review was interrupted when PR feedback expanded the scope; it supplies no approval. The combined round 6 freeze and checks are recorded below.

### PR #5: combined CI and CodeRabbit follow-up on 2026-10-25

Frozen inputs: `performance-implementation-2026-10-25/review-r6-source-hashes.json` (50 source/Cargo/test/reproducer files). Round 5's earlier manifest is historical; its review was interrupted, not approved. The response table is `review-responses-r6.md`. Transfer fixtures are generated locally with discovery disabled or explicit loopback peers; no public trackers/DHT were used by these checks.

- **Integrated backend/unit suites:** pinned Rust 1.99.0, `cargo test --offline --locked --all-targets` passed **79 core + 156 desktop-unit + 9 transfer = 244**, 13 ignored; suite times 23.97 / 9.68 / 18.28 seconds. `cargo test --offline --locked --no-default-features --all-targets` passed **79 + 9 = 88**, one ignored; suite times 23.98 / 18.32 seconds. Logs `/tmp/flick-pr5-combined-tests.log` and `/tmp/flick-pr5-combined-no-default.log`. These are test runs, not a desktop session.
- **Integrated lint/format/build:** `cargo clippy --offline --locked --all-targets -- -D warnings` passed (9.51 s); the no-default-features variant passed (4.24 s). `cargo fmt --all -- --check` and `cargo fmt --manifest-path tests/playback_actor_backend/Cargo.toml -- --check` passed. `cargo build --offline --locked --bin flick` passed (0.78 s). These cover the final combined source with pinned Rust 1.99.0; the build alone makes no GUI or playback claim. Commands/results are retained in `performance-implementation-2026-10-25/final-checks-r6.json`.
- **Saved-sidecar identity regression, private backend:** exact parent-applied `src/torrent.rs` SHA-256 `044e5283c198066a39b201559d3a44a1904db18616a73b06759739a623c2be59`. Candidate **17 unit + 3 external tests passed**; strict Clippy and rustfmt passed. Distinct-package baseline countercheck passed the ordinary partial-restore and malformed-session controls but failed the byte-preservation regression as expected (exit 101). Commands: `CARGO_NET_OFFLINE=true CARGO_TARGET_DIR=/tmp/flick-coderabbit-recovery-20261025/target cargo test --offline --manifest-path /tmp/flick-coderabbit-startup-20261025/candidate/Cargo.toml --lib --test startup_guard -- --nocapture`; same manifest/target with `cargo clippy --offline --lib --tests -- -D warnings`; `rustfmt --edition 2024 --check /tmp/flick-coderabbit-startup-20261025/candidate/src/torrent.rs`. Logs `candidate-tests-isolated.log`, `candidate-clippy.log`, and `baseline-regression-isolated.log` in that private folder. Two failed attempts retain exact saved JSON, bad sidecar and unselected partial bytes; matching repair restores the owner. No playback was performed.
- **UI method checks, private mocks:** `/tmp/flick-coderabbit-ui-r_fdhn51/make-focused-check.py` extracted actual helper/Resolve/Delete/admission methods with mocked GPUI/store/worker/service boundaries. `rustc --edition 2024 --test /tmp/flick-coderabbit-ui-r_fdhn51/focused.rs -o /tmp/flick-coderabbit-ui-r_fdhn51/focused-tests` and `focused-tests --test-threads=1` passed **11**. Candidate rustfmt, patch round-trip and pinned GPUI 0.2.2 deferred-callback signature/effect ordering inspection passed. These are mock-boundary checks, not GPUI runtime or interactive playback.
- **Reproducer and evidence checks:** `python3 docs/performance-audit-2026-10-25/reproduce.py --help`, invalid-run argument cases, syntax parsing and `--prepare-only` passed. All **36** exported audited inputs match `git show 71587ecac24801d0d1eb3d67fe6dec9451034931:path`; manifest/harness paths use those exact archived inputs. A generated empty repository fails clearly when the audited commit is unavailable, without fetching. Preparation builds no binary and runs no benchmark. Three JSON evidence files differ only in nine `<repo>/` path substitutions; all measurements/hashes remain unchanged. Detailed private handoff `.flick-test/performance-implementation/coderabbit-docs.md`.

- **Actual GUI palette Quit:** final debug binary SHA-256 `806fb326dfbfb00c726984d2cfe6daa7fabe5003211f5f818124798887b77190`, task-owned Xvfb/Openbox/Lavapipe with null audio and isolated generated local state. Focused the window, opened Ctrl+K, typed `quit`, inspected the visible Quit Flick row and clicked it. Process exited **0**, no entity-reentry panic, observed within 1.11 s; owned display/supervisor exited. The first setup attempt submitted the default online-search row instead of Quit and did not test shutdown; the corrected screenshot-selected command is the recorded result. Discovery was disabled and no peer connection occurred. Evidence `palette-quit-r6.json` and `palette-quit-r6.png`. This is a GUI shutdown check, not a new interactive playback or native Windows/macOS test.

Recovery UX remains deferred: Retry does not repair corrupt metadata. Dropping `session.json` alone is unsafe for retained partial files. The preflight protects saved identity before engine admission under Flick's advisory lock; an uncooperative external writer replacing metadata afterward remains a TOCTOU limitation. No claim of native Windows/macOS interactive execution follows from these Linux tests or binding checks.

### Whole-application performance audit on 2026-10-25

The user requested a report before application changes, a whole-app performance audit with a 10× ambition, consultation with Claude, and investigation of library entries remaining after files are deleted externally plus a missing Delete button. The [report](performance-audit-2026-10-25/report.html), [findings](performance-audit-2026-10-25/findings.json), [raw measurements](performance-audit-2026-10-25/measurements.json), [benchmark harness](performance-audit-2026-10-25/harness.rs), and [reproducer](performance-audit-2026-10-25/reproduce.py) record the result. No application behavior was changed.

Audited revision: `71587ecac24801d0d1eb3d67fe6dec9451034931`. Environment: Ubuntu 24.04.5 x86_64, four vCPUs, Rust 1.99.0, GPUI 0.2.2, libmpv 0.37.0, FFmpeg 6.1.1. The VM had unrelated CPU activity; the final helper runs started/ended around 9.1–9.2 one-minute load on four CPUs. These are diagnostic measurements under contention, not idle-machine release acceptance.

Executed checks and observations:

- **Ordinary backend/unit/integration tests:** `cargo test --locked --all-targets` passed: 45 library tests, 48 desktop-binary unit tests, and 9 transfer integration tests, for **102 passing and 11 opt-in tests ignored**. Transfer execution used generated local fixtures and explicit local peers without public trackers/DHT. Desktop-binary unit tests do not establish interactive playback.
- **Native playback backend:** generated a local 12-second 1280×720/30 fps H.264/AAC clip using `bash scripts/make-test-video.sh /tmp/flick-performance-audit-8yb855uf/fixture.mp4`. `FLICK_AUDIO=null FLICK_TEST_VIDEO=/tmp/flick-performance-audit-8yb855uf/fixture.mp4 target/debug/deps/flick-1721ea9edcbbdac7 playback::tests::real_playback --ignored --nocapture` passed, one test. This uses the test binary built by the ordinary test pass; it does not open a desktop window.
- **Desktop build:** `cargo build --locked --bin flick` passed in the debug profile. The release builds in this audit built the separate desktop-linked helper, **not a release Flick application**. The upstream `proc-macro-error2` future-compatibility notice remains.
- **Interactive desktop:** ran the existing debug Flick binary in a task-owned Xvfb/Openbox session, with Mesa Lavapipe, GNOME dark style, null audio, isolated settings/data/download directories, and `FLICK_LOCAL_TRANSFERS=1`. A generated video was displayed in the library. Deleting that file from disk left its card visible after navigating to All Videos; right-click showed Play, Show in Folder and Mark as Watched, with no Delete action. Restored the generated hard link, opened playback, observed the colored test-pattern frame, clicked Library/Back to All Videos, then quit with Ctrl+Q; app exit code was zero. No public transfer was started. Captures: [before deletion](performance-audit-2026-10-25/before-deletion.png), [after deletion](performance-audit-2026-10-25/after-deletion.png), [context menu](performance-audit-2026-10-25/context-menu.png), [player](performance-audit-2026-10-25/playing.png), and [Back](performance-audit-2026-10-25/back-to-library.png). This small session does not measure Back-to-paint latency, large-library interaction, PiP CPU, audible output/sync or real GPU performance.
- **Desktop setup failures:** an initial launch supervisor ended before interaction, and a subsequent Xvfb setup reset after its display-fd reader closed, leaving the app unable to initialize X11 (exit 101). Keeping the supervisor/reader alive and using `-noreset` produced the successful session above. Task-owned app/window-manager/display processes were cleaned up.
- **Source-helper performance experiment:** `python3 docs/performance-audit-2026-10-25/reproduce.py --runs 3` completed end to end. It creates a temporary Cargo manifest using the repository's dependencies/lockfile and actual Store/Player modules, builds at release opt-level 3 with bin LTO disabled (`cargo rustc --locked --release ... -- -C lto=off`), generates local media, and runs three independent helper invocations. No desktop window or transfer session is opened by this helper. The first exploratory 100/1k/5k/10k run was intentionally interrupted after emitting 100/1k results because the quadratic work made the larger sweep excessive; those partial results are excluded from the retained final measurements.
- **Deletion persistence experiment:** in each final invocation, a generated file was recorded in library/history and then unlinked. Save/reload retained one library entry despite the file being absent. Clearing only the library and saving also reintroduced one entry from history. These reproduce the model behavior in addition to the interactive stale-card observation.
- **Claude consultation:** two T3-owned read-only reviews with Claude Opus 5.5 completed, first for playback exit and then for all subsystems/deletion. Claude made no edits and ran no builds, tests or playback. The primary agent checked the relevant source/dependency claims and performed the checks above.

- **Report/artifact validation:** T3 `html_preview` and `preview_open` were unavailable: "This host blocks the sandbox T3's browser runs in (AppArmor on Ubuntu 23.10+)." A first full-Chromium CLI attempt timed out after 30 seconds. A dedicated local Chrome headless-shell fallback rendered the self-contained report successfully. At 728 px and 390 px widths, checks passed for both library-size selector states, all 12 findings, and no horizontal overflow; the desktop screenshot was inspected. The final report retained the same checks after theme-variable/number-format adjustments. Python AST parsing, JSON/result completeness, the three deletion-reproduction outputs and `git diff --check` also passed.

The final helper uses 100 and 1,000 generated local-file records, with equally sized history, about two thirds at an eligible resume position, cached poster path records, and no torrent identities. Each operation has an unmeasured warmup. There are nine timed save/filter/show-group rounds and five metadata/continue-watching/indexed rounds per invocation. The metadata model follows the current duration/resume/watched/poster branches using actual Store methods but omits formatting, GPUI tree/text/image work, GPU upload and visible presentation. Its temporary indexed counterpart rebuilds path/history indexes inside each round. Assertions check resume/watched/poster equivalence for every fixture entry; this is not a general production implementation of torrent/history/UI semantics.

Range of the three per-invocation medians:

| Helper operation | 100 records | 1,000 records |
| --- | --- | --- |
| Full settings save, including clone/sanitize/JSON/fsync/rename | 2.70–5.74 ms | 13.07–13.32 ms |
| Sort plus Unwatched filtering | 2.65–7.29 ms | 280.13–355.18 ms |
| Modeled per-card metadata batch, without GPUI/formatting | 4.65–14.84 ms | 552.86–697.14 ms |
| Continue Watching scan | 1.88–2.37 ms | 206.14–241.25 ms |
| Group Shows | 0.056–0.060 ms | 0.519–0.597 ms |
| Indexed metadata prototype including index construction | 0.081–0.095 ms | 0.810–0.989 ms |

The native helper initializes a separate Player, uses its background render thread at a 1920×1080 target, and consumes generated software frames. Across the three runs, player initialization was 153.81–333.72 ms and load-to-first-frame-received was 171.42–456.21 ms; neither is visible display scanout or app-launch latency. Playing snapshot medians were 0.119–0.124 ms; paused snapshot medians were 0.110–0.126 ms. A batch of two pause property updates and two snapshots had medians 0.303–0.347 ms. Native hardware-decoder status was unavailable/none. The current snapshot makes **13** native property reads; the initial Claude review estimated about 14.

The measurements support investigating indexing/virtualization before claiming that fsync alone explains Back. They establish a large isolated lookup opportunity, not a shipped whole-app speedup. The report also covers PiP invalidation, unbounded frame delivery, idle scans/saves, startup gating, scrubbing, poster/preview caches, HTTP resume rereads, transfer snapshot work and shutdown cancellation. Do not disable torrent rechecks/verified-piece reads or reduce default presentation resolution to create a favorable result.

Pending: application implementation; direct Back-to-first-paint spans and actual UI timing at large sizes; sustained CPU/RSS/cache/idle/power measurements; device-specific native GPU/4K performance; download throughput and multi-GB restart profiling; audible synchronization; and macOS/Windows acceptance. No new sustained 4K or external-network performance result is claimed.

### Windows listen-port failure in CI on 2026-10-25

Pull request #3 ran CI twice at `051da49`. In the `pull_request` run, the `Transfers without desktop / windows-latest` job failed `torrent::tests::public_port_is_persisted_reused_and_replaced_when_busy` with "Probe transfer UDP listen address: An attempt was made to access a socket in a way forbidden by its access permissions (os error 10013)". The same job passed in the `push` run. Windows refuses ports in its excluded ranges, which Hyper-V and WinNAT reserve inside 49152–65535, with that access error instead of "address in use". TCP and UDP ranges differ, so a random port could pass the TCP probe and fail the UDP one. The test exposed a real defect: public desktop sessions on Windows could fail to start whenever the saved or random port fell in a reserved range.

The fix treats a refused bind like a busy one, both when probing ports and when librqbit's own bind fails at startup, so Flick moves to another port. The new `torrent::tests::refused_ports_are_replaced_like_busy_ones` uses port 1, which unprivileged Linux processes cannot bind, as the stand-in for a reserved port. The test returns early where that bind succeeds. With the fix reverted, it failed on this VM with "Permission denied (os error 13)"; with the fix, it passed.

On Linux (Ubuntu 24.04.5, Rust 1.99.0): `cargo fmt --check`, strict Clippy with and without default features, and `cargo test` all passed: 44 library tests, 48 desktop tests and 9 local transfer tests, with 11 ignored. No desktop playback was run. After the push, both `Transfers without desktop / windows-latest` runs passed at `1896f19`.

### Quit, title-bar double-click, and the command center on 2026-10-25

Items 33–37 in the plan. Checked on Linux (Ubuntu 24.04.5, Rust 1.99.0, debug build) under Xvfb with Openbox and Lavapipe, `FLICK_AUDIO=null`, and the earlier test library. Not run on macOS or Windows, so the macOS app menu, ⌘ keys, and the macOS title-bar double-click preference are unverified there.

- **Quit:** Ctrl+Q closed Flick about 0.4 seconds after the key press, with no panic in the log, and `settings.json` was rewritten during the quit.
- **Double-click:** in the title bar's search field, it left the window at 1100×740. On empty title-bar space it still maximized the window, to 1600×979.
- **Command center:** Ctrl+K opened it over the library and over a playing video. With nothing typed, it listed Continue Watching and commands, with player commands first during playback. Typing "night" listed two library matches and "Search online". The arrow keys moved the selection, and the footer changed to the selected row's action. Escape, a click outside the panel, and Ctrl+K each closed it. The panel is centered in the window with a fixed height: in a 1100×740 window it spanned the same 500 pixels, centered within 2 pixels, both empty and with results. Typing "harbor" and pressing Enter played `Harbor lights.mp4`. CodeRabbit found that keys typed into the command center over a playing video still reached the player, because its field was missing from the list of text fields. After the fix, typing `m f p 5` over a playing video went into the field. The video kept playing from 0:07 to 0:10 without muting, pausing, seeking, entering fullscreen, or opening picture in picture.
- **Links:** typed into the command center or the search field, `https://youtu.be/…` and `https://www.youtube.com/watch?v=…` were offered as "Download video from YouTube", and `https://x.com/…/status/…` as "Download video from X". The library sections were hidden while a link was typed. Neither link was downloaded, so no YouTube or X request was made. A link to a test clip on a loopback HTTP server, entered with Enter, opened Downloads. The system yt-dlp 2024.04.09 saved it, and `cmp` matched the source bytes.
- **Search field:** it shows "Ctrl+K" on Linux, which is ⌘K on macOS, until it is focused or holds text. The placeholder was shortened to "Search or find videos", which fits beside the hint; the old text was cut off even before the hint was added.

Automated checks: `cargo fmt --check`, strict Clippy with and without default features, and `cargo test` passed (45 library, 48 desktop and 9 local transfer tests; 11 ignored). New assertions cover site names for links, the reserved quit keys, and finished state for history saved before the flag existed.

### Pull request #3 review fixes on 2026-10-25

CodeRabbit reviewed `051da49` and left 12 comments and one nitpick. All were checked against the code and fixed:

- **Episodes:** Next and previous episode skipped to another copy of the same episode, such as a 720p and a 1080p file. They now step past copies, in both directions.
- **Watched state:** opening a video counted as watching it, so sampled episodes showed as watched and an in-progress video offered only "Mark as Unwatched". History and torrent records now carry a `finished` flag. It is set by Mark as Watched, or by stopping within the last 10 seconds after playing at least half. Watching again keeps it, Mark as Unwatched clears it, and torrent media follows its torrent's record, like the resume position. Sorting by last watched now builds its order once instead of searching history for every comparison.
- **Exit with posters queued:** closing Flick waited for the poster worker to finish every queued video. A closed queue still delivers what was sent, so the worker now checks a stop flag and quits after its current file.
- **Shortcuts:** rebinding accepted keys Flick handles before looking up shortcuts, so they never ran: Ctrl or ⌘ with O, K, comma or L, Escape with modifiers, and Tab with Alt. These are refused, and hand-edited settings that use them are unset. A test also checks that no default binding is reserved.
- **Subtitles:** the fallback folder applied only to permission errors, not read-only file systems, and it downloaded the subtitle a second time, using another download from the OpenSubtitles daily quota. `Client::download` now returns the subtitle in memory, and only saving is retried.
- **Transfers:** if the transfer runtime could not be created, local files stayed pending because no empty transfer list was sent. It is sent now.
- **Packaging test:** `packaging/macos/test-package-script.sh` used GNU `sed -i` and a `\n` replacement, which BSD `sed` on macOS handles differently. It now rewrites files with `grep -v` and `mv`, and joins lines with `paste`.
- **Documentation:** removed stale statements that streaming is unimplemented (README, `docs/TRANSFERS.md`, `docs/SHARING_PLAN.md`), limited the HTTPS resume claim to what is tested, described the conditional license and SBOM copies in `THIRD_PARTY_NOTICES.md`, and stopped claiming a Flick license for the picture-in-picture icon, since the project has none yet.

Checks on Linux: `cargo fmt --check`, strict Clippy with and without default features, and `cargo test` passed (44 library, 48 desktop and 9 local transfer tests; 11 ignored). New assertions cover duplicate episode copies, watched state through `remember`, Mark as Watched and Unwatched for local and torrent media, and the reserved keys. ShellCheck reported nothing, and the packaging test passed all 35 checks under GNU tools; it has not run with BSD tools. The exit-time poster fix, the read-only fallback and the runtime failure path have no automated test and were not exercised interactively. No desktop playback was run.

### macOS packaging script on 2026-10-24

Backlog item 25 adds `scripts/package-macos.sh`, hardened-runtime entitlements and an app icon in `packaging/macos/`, the `macos-package` CI job, and [Packaging for macOS](PACKAGING.md). All of it was written and checked on Linux, so **no macOS packaging result exists yet**. A real `Flick.app`, its relinked libraries, signatures, notarization, the DMG, Gatekeeper, and playback from the bundle are unverified. They need the CI job or a Mac. No Rust source changed, so no cargo checks were rerun, and no desktop build or interactive playback was attempted.

Environment: Ubuntu 24.04.5 x86_64, GNU bash 5.2.21, ShellCheck 0.9.0, Python 3.12.3 with PyYAML 6.0.1, and librsvg 2.58.0.

- `bash -n scripts/package-macos.sh` passed, and ShellCheck reported nothing for it or for `packaging/macos/test-package-script.sh`. The test script disables SC2016 and SC2012 for its `bash -c` assertions. Neither check ran under macOS's `/bin/bash` 3.2. The script avoids bash 4 features, and the CI job runs it with `/bin/bash`.
- `scripts/package-macos.sh --dry-run` printed the settings and plan, listed the macOS tools as missing, and created no output directory. Python's `plistlib` parsed the Info.plist it prints: bundle id `app.Flick.Flick`, version 0.1.0, `NSHighResolutionCapable`, and three document types. Both entitlements files parse. The minimum macOS shown is a placeholder; real packaging computes it from the binaries.
- These inputs were each refused with a specific message: an unknown option, a missing option value, a partial Apple ID credential set, two credential sets, notarization with an ad-hoc identity, an invalid build number, bundle id, or `--min-macos`, and a missing entitlements file. An existing `Flick.app` was refused without `--force` and only reported in a dry run. Packaging and `--verify` on Linux were refused.
- `bash packaging/macos/test-package-script.sh` passed 35 checks. It runs the real script against a fake Homebrew prefix of text "Mach-O" files, with stand-ins for `otool`, `install_name_tool`, `codesign`, `lipo`, `plutil`, `sips`, `iconutil`, `xattr`, `hdiutil`, `ditto`, and `brew`. It covered:
  - the closure walk through absolute, `@rpath`, `@loader_path`, weak, and framework dependencies, with Swift's OS rpath kept;
  - relinking to `@loader_path`, removal of duplicate build-prefix rpaths, and untouched originals;
  - the minimum macOS, license and SBOM collection with warnings, and GPL reporting;
  - signing order, DMG contents, `--verify` acceptance and rejection, `--force`, and a missing dependency that leaves no output.

  The stand-ins follow the documented `otool` output format; real Apple tools were not involved. The harness found one real defect, which was fixed: an `awk … exit` or `head -n 1` reader closed its pipe early, and under `pipefail` the writer's SIGPIPE stopped the script without a message.
- PyYAML parsed `.github/workflows/ci.yml`. The existing `transfers` and `validate` jobs are identical to the previous revision. actionlint was unavailable and not run, and **the new job has not run**.
- The documented formula versions, licenses, and build flags for mpv, FFmpeg, LuaJIT, and VapourSynth came from Homebrew's formula API and `homebrew-core` sources on this date. They were not observed in an installed keg.

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
### Watching while downloading on 2026-10-24

This covers backlog item 23 and stage 4 in [PLAN.md](../PLAN.md), on the same Ubuntu VM and Xvfb session.

Automated checks: `cargo test --locked --all-targets` and `--no-default-features` pass, including the new local-peer test `streams_read_verified_bytes_while_downloading`. A 12 MiB generated file is shared over loopback and downloaded at 512 KiB/s. While it downloads, the stream returns exactly the source's first 200,000 bytes. A seek 50,000 bytes before the end then returns the source's final bytes before the download completes, which shows the tail was fetched ahead of the middle. With the download paused, a read blocked in the middle of the file ends when cancelled, and later reads fail. The opt-in `real_custom_protocol_streams_play` test plays the 720p fixture through a registered libmpv protocol, seeks to 8 s, and reports an unknown stream as a load error.

Interactive check: `flick-transfer share` seeded a generated 60-second 1280×720 H.264/AAC file (20.6 MB) on 127.0.0.1:47123. Its MP4 index (`moov`) is at the end of the file, at byte 21,535,740 of 21,601,163. A fresh Flick session used `FLICK_LOCAL_TRANSFERS=1`, `FLICK_INITIAL_PEERS=127.0.0.1:47123`, and `FLICK_DOWNLOAD_LIMIT=300000`. Pasting the magnet in search resolved the file. Download Selected started it, and at 0 KB of 20.6 MB at 285 KB/s the row offered Watch Now. Watch Now showed the first frame after 549 ms. Pressing 9 to seek to 0:54 showed "Buffering…" over the previous frame, and 14 seconds later playback had reached the end at 0:59.97. An earlier run at 400 KB/s seeked to 0:48 and resumed there.

Tried and reverted: enabling mpv's demuxer cache for streams, so mpv would report buffering itself, delayed the first frame from about 0.5 s to 10 s at 300 KB/s. Flick instead shows Buffering when a stream that should be playing has not advanced for a second.

Not verified: public swarms, rapid repeated seeks, subtitles inside streamed files, replacing a file during playback, and macOS or Windows.

### Library, player, and rendering features on 2026-10-24

This covers backlog items 7, 14, 16, 17, 20, 21, and 22 in [PLAN.md](../PLAN.md), on the same Ubuntu VM, Xvfb/Openbox/Lavapipe session, isolated settings, and generated fixtures as the backlog pass above. Two copies of the 12-second clip were arranged as `Harbor Show/Season 1/01 - Arrival.mp4` and `02 - Departure.mp4`, and the 6-second 4K clip was added to the library. Other agents' builds and benchmarks were running, with load averages of 10–14 on four vCPUs, so no timing below is a performance result.

Automated checks on the merged tree: formatting; strict Clippy with and without desktop features; and `cargo test --locked --all-targets` (43 shared-library, 44 desktop, and four local-transfer tests; nine opt-in tests ignored). These opt-in native tests pass with `FLICK_AUDIO=null FLICK_TEST_VIDEO=<720p clip>`:

- `playback::tests::real_render_thread_delivers_tagged_frames`: frames arrive at the target size, a zero target stops rendering, each load advances the generation, a resize re-renders while paused, and dropping the player while it plays joins the render thread.
- `thumbnails::tests::real_posters_are_written_with_details`: a 400-pixel PNG poster that is not black, with duration 12.02 s, 1280×720, and `h264`; a missing file reports an error.
- `thumbnails::tests::real_previews_follow_the_newest_request`: previews at 2 s and 8 s differ.
- The six earlier playback tests also pass after the render-thread refactor, and the smoke test passes.

Interactive observations under Xvfb:

- **Shows (item 17):** the episodes appeared as one show, Harbor Show, listed under Season 1. At the end of episode 1, an "Up next in 7 s" card offered Cancel and Play Now; without input, episode 2 opened. Cancel kept episode 1 open past the countdown. Shift+P returned to episode 1. The first Cancel attempt clicked before the frame after the pointer move rendered; the retry worked.
- **Posters (item 16):** posters for all six videos were generated in the background, at roughly 2–3 seconds each under this load. Cards showed the posters with 4K and 720p badges and durations such as "Not started · 0:06".
- **Timeline previews (item 20):** hovering the timeline showed the frame at 0:07 above the pointer.
- **Picture in picture (item 21):** P opened the video in a separate window while the main window showed "Playing in picture in picture". Back to the library kept it playing: the timecode advanced from 0:05.7 to 0:11.9. Dragging moved it, hovering showed its controls, and its return button closed it and restored the player. Openbox placed it at the bottom left and let the main window cover it, so staying above other windows is unverified on X11.
- **Render thread (item 14):** the 4K clip, paused seeks (the picture updated at 6.03 s and 9.63 s while paused), and picture in picture all worked through the render thread. During the load above, 20 frames were presented in 5.3 s with 72 frames dropped; this is not a measure of the change.
- **Fades (item 7):** player controls hid on schedule and were removed. Burst captures went from shown to hidden without an intermediate frame, so the 180 ms fade itself was not observed at this capture rate.
- **OpenSubtitles (item 22):** Find Online opened with "Harbor lights" filled in. Without a key, Search explained where to add one and offered Open Appearance. With `FLICK_OPENSUBTITLES_KEY` set to a deliberately invalid value, one live search to api.opensubtitles.com showed "OpenSubtitles rejected the API key." A successful live search and download were not attempted, because no key was available.

Fixed during this pass: picture in picture first crashed with "cannot read Flick while it is already being updated", because the new window's first render read the main view during its update. Opening is now deferred to the next turn of the event loop.

Not verified: macOS floating panels and Windows tool windows for picture in picture; Wayland, where no always-on-top request exists; and macOS Reduce motion and Windows animation settings.
### Transfer controls and torrent resume on 2026-10-24

Backlog item 24 and the matching stage-2 implementation boxes are complete. Downloads adds an explicit file-list confirmation for permanent removal, upload activity, exact metainfo export through the native save picker, and Pause All / Resume All. Appearance has a Transfers group with a persisted seeding preference and live download/upload limits. The welcome page and Downloads request the seeding choice until answered; until then, verified completed downloads pause. Explicit shares remain active independently of this preference, including after restart. Torrent media binds to `(infohash, file index)` alongside its path-based history. No playback rendering or benchmark files changed; `src/ui.rs` only gained transfer UI state and the settings argument for service startup.

Checking uses the engine's examined-byte counter and total separately from verified download progress. Initial share creation exposes no percentage in librqbit 9.0.1, so that phase shows “Checking files…” without inventing progress. Uploaded totals are the native live-state counter, not lifetime accounting. Empty folders remain after deletion. Arbitrary manually moved files need a torrent association before identity-based resume can apply.

Environment: Linux x86_64, Rust `1.99.0 (b940084d7 2026-09-28)`, pinned GPUI 0.2.2 and librqbit 9.0.1. Executed checks:

- `cargo check --locked --no-default-features --all-targets`: passed (4m 30s); backend checking, not a desktop build.
- `cargo check --locked --all-targets`: passed (10m 14s); desktop checking, not interactive playback.
- `cargo fmt --all -- --check`: passed. An initial `cargo fmt --all` during editing reported a match-arm syntax error, corrected before the final checks.
- `cargo clippy --locked --all-targets -- -D warnings`: passed (5m 43s including a build-lock wait).
- `cargo clippy --locked --no-default-features --all-targets -- -D warnings`: passed (10.63s).
- `cargo test --locked --all-targets`: passed (6m 26s compile; 39 shared-library, 41 desktop, and eight local-transfer tests passed). Eight opt-in tests were ignored: one installed-yt-dlp test and seven native libmpv/thumbnail tests. This command also linked the desktop app binary used below.
- `cargo test --locked --no-default-features --all-targets`: passed (1m 17s compile; 39 library and eight local-transfer tests passed, one installed-yt-dlp test ignored).

The new integration fixtures cover guarded deletion with unrelated files in the same folder, symlink-parent refusal and preservation of outside content (Unix), exact shared/downloaded metainfo export and file-list/infohash round-trip, refusal to export over managed content, both configured and live bandwidth limits, automatic completion pause, restart verification with seeding disabled, global pause/resume, and persisted explicit-share identity. Store regressions cover path changes, independent file indices and hashes, preservation of old path/local history, watched/unwatched state, and backward-compatible transfer preferences. All automated transfer tests use generated local content, explicit loopback peers where needed, and no public trackers, DHT, or UPnP.

Interactive transfer check: `target/debug/flick` (built by the full test command) ran under task-owned Xvfb `:98`, Openbox, Mesa Lavapipe (`VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.json`), and `FLICK_AUDIO=null`. `FLICK_SETTINGS_PATH`, `FLICK_DATA_DIR`, and `FLICK_DOWNLOAD_DIR` pointed into `/tmp/flick-gap-ui-okun7f8m`; `FLICK_LOCAL_TRANSFERS=1` disabled public discovery. A `flick-transfer` seed shared a generated 262,144-byte non-video fixture, and its explicit loopback listener was passed in `FLICK_INITIAL_PEERS`. Observed:

- Shift+Tab reached **Pause When Complete** with a visible focus ring; Enter saved `seed_completed: false` and removed the first-run choice.
- Entering the fixture's magnet in search resolved its metadata from the local seed. **Download Selected** saved byte-identical content and the row showed **Complete · sharing paused**, the upload fields, and the close-Flick note.
- The Transfers settings switch saved `seed_completed: true`. Shift+Tab reached both bandwidth sliders; Right changed each to 256 KiB/s, confirmed in the settings file. The upload slider's focus ring and tooltip were visible.
- **Resume All** changed the completed transfer to **Sharing**, and **Pause All** paused it again. The transfer row was inspected in all three interface styles on Linux; this does not establish native macOS/Windows behavior.
- **Remove and Delete Files…** listed the fixture's full path and explained permanence. Shift+Tab reached **Delete Listed Files** with a visible ring; Shift+Tab again and Enter activated Cancel and kept the fixture. Reopening and keyboard-confirming deletion removed the downloaded fixture and its row, kept an unrelated sibling with byte-identical contents, and kept the seed's original file.
- The app was closed through its window control. The supervisor stopped its seed, Openbox, and Xvfb by their recorded PIDs. An earlier launch supervisor exited too soon and its processes were terminated before interaction; the subsequent supervised run supplied the observations above. Generated incremental/check-only artifacts from this worktree were removed to stay within available disk space; no other worktree's files were removed.

Screenshots: [first-run keyboard choice](screenshots/flick-transfer-seeding-choice.png), [settings and keyboard slider](screenshots/flick-transfer-settings.png), [file-list confirmation](screenshots/flick-transfer-delete-confirmation.png), and [macOS-style transfer row on Linux](screenshots/flick-transfer-mac-row.png).

Native export save picking, visible hashing progress on a large fixture, interactive torrent-media resume, and actual playback were not exercised. Export and identity resume have automated coverage; this non-video GUI download does not establish the complete share → download → Play acceptance pass. No separate `cargo build` command was run. The existing native libmpv/thumbnail and installed-yt-dlp opt-in tests were not run. The upstream `proc-macro-error2` 2.0.1 future-compatibility notice remains. Final `cargo fmt --all -- --check` and `git diff --check` also passed after the documentation update.


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

GPUI 0.2.2 builds Metal shaders and uses libclang to generate platform bindings. GitHub's macOS runners supply Xcode; the workflow checks the selected installation. Install a libmpv build matching the machine's architecture for playback. FFmpeg is needed only to generate fixtures. A real desktop session is needed to exercise the player. To build a self-contained `Flick.app` and DMG, see [Packaging for macOS](PACKAGING.md); the earlier development `Flick.app` and `Flick Preview.app` bundles in this record were local wrappers around a debug binary, and nothing here shows that they carried libmpv.

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
