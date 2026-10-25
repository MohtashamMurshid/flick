# Performance and deletion implementation

The library now uses indexed lookups and virtualized views, Back queues persistence and native controls in the background, and PiP frames update a small video entity independently. External file deletion removes the stale library entry; visible **Remove** keeps the file, while **Delete… → Move to Trash** confirms and moves only the selected file. Path/torrent removal markers prevent automatic history/completion callbacks from restoring removed entries.

## Observed results

| Diagnostic | Before | Implemented |
| --- | --- | --- |
| Back → All Videos, 1,000 videos/history, debug desktop | 8.73 / 15.79 / 20.74 s | 298 / 317 / 481 ms after review fixes |
| Metadata lookup batch, 1,000 records, release helper | 553–697 ms | 0.85–1.83 ms |
| Sort + Unwatched, 1,000 records, release helper | 280–355 ms | 0.33–0.68 ms |
| Continue Watching, 1,000 records, release helper | 206–241 ms | 0.31–0.73 ms |
| Back → All Videos, 5,000 videos, debug desktop | Not measured | 411 ms, one trial |
| PiP over library, five-second trace | Root invalidated every frame in audited source | 150 video surface renders; zero root renders |
| Paused library, ten-second trace | Periodic saves/scans in audited source | Zero root renders; unchanged settings mtime |

The matched-size desktop comparison exceeds the 10× ambition in this diagnostic: median 15.79 s → 0.317 s (about 50×). An earlier implementation measured 259/341/349 ms, with a 341 ms median (about 46×); those original trials are retained separately. A final smoke check on the frozen review source measured 310 ms in one trial.

These are **debug builds on an Ubuntu VM using Xvfb/Openbox/Lavapipe and null audio**, with uncontrolled CPU contention. X11 pixel detection is not physical display scanout. Three trials do not establish typical device latency or a whole-application 10× guarantee. The helper timings exclude GPUI, image upload and display; they use real production Settings APIs and generated records. Native video decoding/render throughput has no comparable speedup claim.

Raw data: [original desktop comparison](desktop-measurements.json), [desktop trials after review fixes](final-back-measurements.json), [final smoke](final-smoke-back.json), [5k desktop](five-thousand-measurement.json), [library helpers](measurements.json), [PiP](pip-measurement.json), [idle](idle-measurement.json). Helper reproduction: `python3 docs/performance-implementation-2026-10-25/measure-library.py`. The original audit and its frozen measurements remain in [the audit folder](../performance-audit-2026-10-25/report.html).

## Changes and design choices

| Area | Implementation |
| --- | --- |
| Back, settings and periodic saves | Cached player snapshot; ordered background writer coalesces pending settings, deduplicates unchanged bytes, retains atomic replace/fsync and a shutdown durability barrier. Save only advancing active playback periodically. Cancel next-episode countdown and preview work on Back; immediately idle the main render target. |
| Library and Shows | Indexed path/history/torrent/removal lookup; cached normalized titles/sorts/show grouping/adjacent episodes and query results. All Videos, History, Shows and episodes create visible virtual rows. Persistent scrolling and stable focus IDs retain navigation. |
| Posters | Decoded image/task cache keeps a 96-entry warm budget and pins the complete visible working set across frames, with explicit texture eviction; bounded extraction/results, visible-first scheduling, extraction suspended during active playback. Disk quota 256 MiB/2,048 generated PNGs, safe owned-name cleanup and bounded invalidation notifications. |
| Native playback | Native `Player` remains `!Send` and is constructed/used/destroyed on its worker. UI proxy admits bounded ordered controls and reads cached observations. Latest-frame mailbox has capacity one. Track/chapter JSON is cached; controls are asynchronous. Drag seeks use keyframes; release sends the exact seek. Optional unsupported subtitle styling logs without interrupting playback. |
| PiP | Video surface entity receives frame notifications; the library/root receives state notifications. Existing physical rendering dimensions, color bytes and subtitle composition are preserved. |
| Startup and verification | Restore transfers concurrently with search/HTTPS commands. Publish validated canonical saved ownership roots early so unrelated local files can open; claimed/uncertain files still wait for current verified state. Full rechecks and verified-piece streaming remain. Reject incomplete restore ownership rather than falsely publishing a safe empty snapshot. |
| Transfers and idle | Stable progress comparisons suppress unchanged events; idle backoff; immediate operation snapshots; cached immutable metadata and selected-file sets; linear completion membership. Completion policy sleeps when inactive and retains its active cadence. |
| HTTP/discovery | Partial files without checksums use metadata length; checksummed prefix hashing checks cancellation. Reuse clients and bound/expire successful search results. Persist bounded partial-object validators; first resumed request uses If-Range; untrusted/changed checksum-free parts restart. Owned sidecar/partial cleanup preserves foreign/replaced paths. Non-Unix checksum-free persisted resume restarts conservatively. |
| Files deleted outside Flick | Background startup/focus/watch reconciliation. Only stable readable local-parent evidence proves a missing file. Offline, inaccessible, uncertain network/removable roots remain unavailable. No UI-thread stat or per-tick filesystem sweep. |
| Remove/Delete races | Per-path incarnations, filesystem identities, revision guards, removal markers and canceled preview/poster work reject stale observations/results. Explicit Open/import restores deliberately. Delete waits for player unload and transfer ownership release while keeping every sibling file, then moves the selected file to OS Trash. Failures remain visible; no permanent-delete fallback. |
| Shutdown | Signal/cancel native and file work without joining UI against decode. Ordinary quit awaits transfer cleanup off the UI thread; native OS quit joins after GPUI window teardown in its first future poll. Pause is queued immediately. The settings writer drains durably. Native I/O can still stall its owner; no hard native termination deadline is claimed. |

## Validation

All final build, test and lint checks pass: **242 ordinary tests with desktop enabled; 86 without desktop; both strict Clippy runs; formatting; dependency isolation; and the debug desktop build**. Native backend checks and actual GUI observations are recorded in [VALIDATION.md](../VALIDATION.md). Ordinary backend tests and native generated-file tests are distinguished from the debug desktop build and observed interactive playback. Transfer tests use generated fixtures and explicit loopback peers without trackers or DHT.

Interactive deletion checks started with 1,000 generated files: external unlink → 999, Remove keeping the file → 998, confirmed Trash preserving its sibling → 997. Restart retained 997 entries and all three removal markers. PiP displayed moving frames over the library. The 5k library scrolled to further virtual rows and switched to List with keyboard focus visible.

At 3840×2160 and 1× scaling, 180 visible distinct posters in a 5,000-entry library settled for 60 seconds with zero root redraws or poster reads. CPU averaged 2.15% of one core; RSS changed from 577,536 to 576,592 KiB. This used Xvfb/software rendering with no playback running. [Large-display evidence](large-display-idle.json).

Filesystem backend checks delivered Missing in 1.03 seconds and recovery in 0.51 seconds despite 10,000 offline entries, using actual 500 ms UI drain intervals. A GUI check with an externally held startup lock recovered its missing poster within 1.31 seconds after Retry, without scrolling. [Poster recovery evidence](poster-recovery.json).

A final GUI regression opened a generated unsupported video, observed the load error, then confirmed Delete on the first attempt. The file reached isolated Trash unchanged and the library count reached zero in 85.8 ms; normal Ctrl+Q exited successfully. [Observation](r4-broken-delete.json), [confirmation](r4-delete-confirm.png), [result](r4-delete-result.png).

During validation, unsupported optional subtitle properties initially surfaced as an error banner, and asynchronous control failures could clear UI loaded state. Both were corrected. Actor tests also cover final EOF/history/autoplay state and confirmed stream release before Trash.

Claude rounds 1 and 2 requested changes; [round 1 findings](claude-review.md), [round 1 responses](review-responses.md), [round 2 findings](claude-review-r2.md), and [round 2 responses](review-responses-r2.md) are preserved. [Focused round 3](claude-review-r3.md) found no Blocking or High issues, but identified failed-load deletion and Error-transfer removal edge cases. [Focused round 4](claude-review-r4.md) independently verified those corrections and found **no Blocking, High or Medium findings**. Its remaining low-severity limits are documented below.

## Remaining limits

Windows/macOS execution, audible synchronization, physical GPU/display acceptance, hardware decoding, native GPU surface sharing, sustained 4K throughput, ten-minute idle/power/RSS measurements and multi-GB restart throughput were not established by this pass. GPUI 0.2.2's software presentation architecture remains a separate constraint. Buffer recycling without ownership proof, downscaling output/subtitles and skipping torrent verification were deliberately avoided.

Remaining behavior: missing parent folders/uncertain drives remain visibly Unavailable and can be removed; corrupt ownership blocks potentially unverified disk reads with Retry/Cancel. Deleting a torrent file stops its whole transfer and keeps siblings, with unverified siblings durably blocked unless a live transfer verifies them. After ownership is removed, adopting the existing folder for verification is not implemented. Files not selected for a live download remain blocked too. HTTP reconnects lacking trustworthy validators may restart. Main-player chrome still renders per frame.

Final review fixes include refusal to remove Checking transfers, fresh ownership snapshots per startup or accepted download, priority Missing/recovery over offline reports, batched UI removal, native recycle-only Windows operations, cancellable Share hashing, bounded redirect retries, low-fps/advancing seek confirmation, and balanced accepted START_FILE/END_FILE events before stop acknowledgement. Native Windows behavior remains unexecuted despite clean binding cross-compilation.

Known ownership-availability limitation: an unplugged Windows download volume, or a non-NotFound error on any platform (for example permission denial, a disconnected network mount, stale NFS or a failing disk), can leave ownership coverage uncertain and block unrelated local opens with visible Retry/Cancel. Ordinary Linux missing-path cases already retain a conservative existing ancestor and allow files outside it. Safe exclusion of uncertain roots was not bypassed by trusting a raw saved alias.

Existing corrupt-settings fallback, path aliases on case-insensitive mounts, and Windows creation-time file identity remain follow-ups. A corrupt settings file can lose durable quarantine evidence; byte-safety guarantees depend on that evidence surviving. No claim of perfect correctness across those cases is made.

Error transfers: Delete and Remove · Keep Files require Resume and reverification first. If the cause persists, such as a full disk, the transfer may fail again before Delete runs. Pause during checking, free space outside Flick, or delete the selected file outside Flick; whole-transfer Delete Files also remains available and removes every listed file. A large recheck can report a 120-second timeout while verification continues and may later succeed. These paths retain the owner and fail closed. The old playback-error banner can remain visible after deleting the failed file.
