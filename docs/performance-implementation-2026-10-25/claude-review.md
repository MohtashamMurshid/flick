# Independent review — performance implementation and deletion (Claude, 2026-10-25)

Reviewer: Claude Opus 5.5, acting as the read-only review sub-agent the user authorized. No application source was edited, nothing was committed, no work was delegated, and no public transfers, trackers or DHT were used. This file is the only file I added to the repository. My scratch harnesses live outside the repository, under `/tmp/claude-review-*`.

## Verdict

**Changes requested.** Do not yet claim the performance or deletion goals as delivered.

- **1 blocking bug is still open (B1).** A common transfer-state failure silently disables local playback and posters for the whole session. One trigger is the user deleting a download folder outside Flick, which is the very scenario this work targets.
- **4 high-severity items are still open:**
  - H1 (rest of it): pruning missing files at startup takes minutes on large libraries.
  - H2: the 96-entry decoded poster cache thrashes forever on large 1× displays.
  - H3: Delete releases a whole multi-file torrent, which exposes unverified sibling bytes.
  - H4: the Trash backends behave badly on macOS and Linux (macOS behavior not verified on a Mac).
- Several medium and low items remain (§5–6).
- **The main agent fixed or started fixing eight of my findings while I was reviewing (13:40–14:04 UTC).** I re-checked each against current source, and re-ran two of them, then recorded what remains in §4.

The design direction is sound:
- the `!Send` player stays on its own worker;
- the frame mailbox holds at most one frame and is memory-safe;
- settings are written by one ordered writer;
- library lists are virtualized;
- removal markers (tombstones) are durable;
- Trash never falls back to permanent deletion.

The 46× Back-to-All-Videos result is real, but only for the path that was measured: a 1k library in a debug build under Xvfb. It is not a whole-app speedup (§7).

## 1. What was reviewed, and how

- **Base:** HEAD `71587ecac24801d0d1eb3d67fe6dec9451034931`. I read the full diff and every new untracked source file: `src/library_files.rs`, `src/library_files_ui.rs`, `src/playback_actor.rs`, `src/store/{index,persistence}.rs`, `src/video_frame_ui.rs`, `src/shutdown.rs` (added during the review) and `tests/playback_actor_backend/*`.
- **The tree kept changing during the review**, from about 13:20 to 14:04 UTC. Checksum snapshots showed concurrent edits to `ui.rs`, `services.rs`, `torrent.rs`, `library_files*.rs`, `library_ui.rs`, `store.rs`, `library.rs` and `main.rs`. **Every finding listed as open was re-checked against source at 14:04 UTC**, and line numbers refer to that state. Anything edited after 14:04 has not been reviewed.
- **Evidence labels:**
  - **[static]**: source reasoning with file:line evidence.
  - **[executed]**: my own runs, listed in §9.
  - **[GPUI]**: checked against the pinned `gpui-0.2.2` sources in the Cargo registry.
  - **[dep]**: checked against other pinned dependency sources.

## 2. Blocking (still open at 14:04 UTC)

### B1. A failed transfer startup silently blocks local playback and posters for the session — [static]

**Evidence**
- `torrent.rs:155-164`: `read_saved_ownership` uses `canonicalize(&folder)?` on every saved output folder. One missing folder, an offline drive or a non-directory fails the whole read.
- `torrent.rs:~446-455`: a partial restore is rejected and the session is stopped.
- `services.rs:429-431`: an ownership error returns `Err` before `TransferRoots` is sent.
- `services.rs:622-628`: a startup `Err` sends only `Event::Error` and sets `startup_error`. No `Progress` is ever sent.
- `services.rs:1528-1550`: a test asserts that no `TransferRoots` or `Progress` arrives in this state.
- `library_ui.rs:581`: the only place `transfers_known` becomes true is on `Progress`.
- `ui.rs:760-783`: `can_open_before_transfers` needs `transfers_known`, or known `transfer_roots` that the path is outside. Otherwise `pending_open = Some(path)` is set with no UI feedback.
- `ui.rs:1071-1073`: the pending open is retried only when that predicate turns true, which it never does.
- `ui.rs:990-994`: posters return early while `!transfers_known`.
- **HEAD behaved differently.** At `services.rs:304-320`, HEAD sent `Progress(Vec::new())` after a failed start, so local files still opened.

**Scenario**
1. Download a torrent with Flick. Its output folder is `<download dir>/<infohash>`.
2. Quit. Delete that folder in Finder or Files, or unplug the drive holding a download.
3. Relaunch.
4. The banner says "Downloads are unavailable: could not establish saved transfer ownership…". Clicking any library video does nothing, and no posters are generated.

Other triggers:
- a corrupt `session.json`;
- a saved torrent that librqbit skips on restore;
- a second Flick instance that holds `session.lock`;
- startup taking longer than 120 s.

When roots were already published (lock held, restore failure, timeout), files *inside* the roots are blocked forever and files outside them still open. When ownership cannot be read at all, everything is blocked.

**Fix**
- Never leave the UI waiting on an event that will not come. Send an explicit "ownership unknown" state that carries any roots that could be parsed.
- A saved root that fails with `NotFound` cannot contain openable files. Skip it rather than failing all roots.
- If the engine fails after roots were published, keep the roots and allow files outside them.
- For files inside a root while ownership is unknown, show a visible "Downloads unavailable — this file may be incomplete" message with Retry, and optionally Play anyway. Never defer silently.
- Always show the `pending_open` state, for example "Waiting for downloads to check this file…", with a way to cancel.
- Add a UI-level test: startup fails, then a local open must either open or show a message.

## 3. High (still open)

### H1. Pruning missing files at startup is slow on large libraries, and every revision resyncs the whole model — [static; the starvation core is fixed, see §4]

**Evidence**
- `library_files.rs:674-681`: the first sweep publishes an `Available` observation for every entry, because `entry.last` is `None`.
- `library_files.rs:215`: `drain()` hands the UI at most 32 observations per call, and the library view polls every 500 ms (`ui.rs` `tick`). That caps throughput at about 64 observations per second.
- A `Missing` for a file deleted while Flick was closed only appears after its second check (400 ms later). It then joins the back of the queue (`ready`, then `results` capped at 256), behind every pending `Available`.
- `library_files_ui.rs:142-144`: every change to the global `Settings::revision()` still triggers a full O(n) `library_files_changed`. That clones a `TrackedFile` per entry, and the worker rebuilds its whole index and watch set.

**Effect (estimated from these limits)**

| Library size | Time until a file deleted while Flick was closed disappears |
| --- | --- |
| 1k | about 15 s |
| 5k | about 75 s |
| 10k | about 2.5 min |

The full O(n) resync also costs UI time on every poster result, every `remember()` and every metadata change.

**Fix**
- Do not publish the initial `Available` observation. Publish only transitions: Missing, Unavailable, Metadata, and Available that clears a previously Unavailable state.
- Drain observations by a time budget rather than a fixed 32, and notify the UI when results are ready instead of waiting for the next 500 ms tick.
- Resync only when a path's inputs change (membership, poster, incomplete, location). The per-path epochs added at 14:02 already compute exactly this, so send deltas.

### H2. The 96-entry decoded poster cache thrashes forever when a frame needs more than 96 posters — [static, GPUI]

**Evidence**
- `library_ui.rs:186`: `POSTER_CACHE_CAPACITY = 96`.
- `library_ui.rs:209-221`: `touch` evicts the front entry on every miss once the cache is full.
- `library_ui.rs:250-290`: `load` drops the evicted texture and the evicted entry's decode task.
- GPUI calls `ImageCache::load` for every visible `img` on every frame.
- When a decode finishes, `cx.on_next_frame(... notify(view))` re-renders the root.
- With N > 96 posters in view, each frame evicts entries that the same frame still needs. In-flight decodes are cancelled, and each completion triggers another render with more misses. The result is a loop of decode → notify → re-render → evict that runs as long as the grid is on screen, and those posters never settle.

**When this happens:** `columns(width, 180., 16.)` (`library_ui.rs:3359`) puts about 17–18 columns × 9–10 rendered rows, roughly 150–180 posters, on a 3840×2160 display at 1× scaling. An ultrawide 3440×1440 at 1× gives roughly 96–112. Both are estimates; the exact count depends on the sidebar and chrome widths. Typical 2560×1440 at 1× gives about 66–77 and is fine. All Videos grid and the Shows grid are affected; List view (88 px rows) is not.

**Fix**
- Evict only entries that were not touched during the current frame (keep a frame epoch).
- Size the capacity to at least about 1.5× the last frame's visible count, with a ceiling such as 512. A 400×225 BGRA poster is about 0.36 MB.
- Add a test where the number visible exceeds capacity, and measure idle CPU with a 4K@1× window.

### H3. Delete releases the whole multi-file torrent, which stops sibling downloads and exposes their unverified bytes — [static]

**Evidence**
- `library_files_ui.rs:470-482`: every owner (any live download that lists the file, plus the library's saved identity) gets `release_for_deletion`.
- `torrent.rs:902-909` calls `remove(id, false)`, which unregisters the entire torrent.
- After that, the UI's notion of "incomplete" comes only from live downloads (`ui.rs:854-873`). Sibling files that were partly downloaded and already in the library, for example after "Watch Now", become directly playable and eligible for posters (`ui.rs:~996-1026`) while containing unverified or sparse data.
- The dialog text (`library_files_ui.rs:599`) warns that "a transfer using this file will stop", but nothing protects the siblings.

**Scenario**
1. A season pack is still downloading, and episode 1 is finished.
2. The user deletes episode 1.
3. The whole pack disappears from Downloads. Episode 3, half downloaded and previously opened with Watch Now, now plays straight from disk with holes.

This is the kind of verified-byte bypass the user asked reviewers to flag.

**Fix**
- If the owner has other selected incomplete files, list them and ask for explicit confirmation. Better, deselect only that one file index and keep the transfer.
- After any release, keep durable "unverified" markers on the released incomplete paths. Refuse direct open and poster generation for them until they are re-verified.

### H4. The Trash backends misbehave on target platforms — [dep, static; platform acceptance, not run on macOS]

**macOS.** `trash` 5.2.9 defaults to `DeleteMethod::Finder`, which runs `osascript 'tell application "Finder" to delete …'` (`trash-5.2.9/src/macos/mod.rs:23-51, 116-128`). The first Delete should trigger an Automation prompt ("Flick wants to control Finder"). The packaged `Info.plist` (`scripts/package-macos.sh:~447-457`) has no `NSAppleEventsUsageDescription`. If the user denies the prompt, every Delete fails until System Settings is changed.
- **Fix:** use `trash::macos::TrashContextExtMacos::set_delete_method(DeleteMethod::NsFileManager)`. It needs no automation and plays no Finder sound. The trade-off is losing Finder's "Put Back".

**Linux.** If the volume's own trash folder is unusable, `trash` falls back to the home trash with a **copy + delete across devices** (`freedesktop.rs:52-64, 583-605`). For a multi-GB video, that is a long copy on the file worker thread (`library_files.rs:928`), which stalls all reconciliation. It can also leave partial copies behind when the disk fills, or duplicate data if the final unlink fails.
- **Fix:** before Trash, check that a trash folder on the same device is usable. Otherwise refuse with a clear message, which is consistent with "no permanent-delete fallback". At the least, warn that a copy will happen.

**Windows.** Trashing a file that a decoder still has open fails visibly. That is already documented as an accepted limitation.

## 4. Fixed, or fixed in part, during the review — residual checks

| Earlier finding | Status (time checked) | Remaining gap |
| --- | --- | --- |
| **Observation starvation:** observations were skipped without acknowledgement whenever the global Settings revision changed, so pruning never happened while posters kept arriving each tick. | **Fixed at about 14:02** by per-path `ObservationGenerations` epochs (`library_files.rs:58-102`, `library_files_ui.rs:~68-110, 163-167`). **[executed]** The real worker under the old rule: 0 `Missing` accepted and 21 dropped over 6 s of per-tick changes. Under the new rule: `Missing` accepted after **1.0 s** while another path's epoch changed every tick. | H1 (startup flood, drain rate, O(n) resync). |
| **macOS shutdown:** `services::finish_shutdown()` after `app.run()` never runs on macOS. `NSApp terminate:` calls `exit()` after `applicationWillTerminate` (gpui `platform/mac/platform.rs:503-523, 1421-1429`). HEAD instead joined `Services` in `Drop`. | **Fixed** by `src/shutdown.rs`. `on_app_quit` joins synchronously inside the first poll of its future, after windows are cleared. Quit goes through `shutdown::request`. | **N1 (Medium-Low):** `request()` (`shutdown.rs:46-66`) leaves the window open and the player **still playing** while services are joined (HTTP cleanup up to about 5 s, plus a 2 s runtime ceiling and an unbounded librqbit stop). There is no timeout, and a second ⌘Q is ignored (`task.is_some()`). Fix: persist, pause and close or hide the windows first, then cap the wait (for example 10 s) before `cx.quit()`. The `flick-player-control` worker is still never joined, so mpv teardown can race process exit. Unverified; a bounded join is recommended. |
| **First-observation prune:** a never-seen entry under an existing empty directory, such as the stub folder left by an unmounted drive, was proven Missing and tombstoned. | **Fixed:** `known_location` (`library_files.rs:809-820`) plus a persisted `FileLocation`. **[executed]** Before the fix this returned `Missing`; after it, `Unavailable`. | None found. |
| **Linux location token:** it embedded per-boot mount IDs and mount flags, so startup pruning would stop working after a reboot. | **Fixed (seen at about 14:04):** the token is now `linux:{maj:min}:{root}:{fstype}:{source}` (`library_files.rs:976-985`). | Not executed. The local filesystem list now includes exFAT, vfat and NTFS; the `known_location` and mount-prefix exclusions keep this safe. |
| **Poster quota churn:** with a 2048-file / 256 MiB quota and every poster-less video re-queued, libraries larger than the quota decoded, evicted and regenerated posters forever. | **Fixed:** `visible_poster_candidates` (`ui.rs:~996-1026, 1796-1808`), and offscreen requests are cancelled. | H2. Idle CPU on a 5k library was not measured. |
| **Scrubbing:** the thumb was drawn from `snapshot.position`, and keyframe drag seeks pulled it back to the keyframe. | **Fixed for the drag-hold case** by pinning to `scrub_target` while scrubbing (`ui.rs:1159-1166`). **[executed]** A paused hold at 6.0 s landed on the 0.0 s keyframe in 7/7 native runs, which the override now hides. | **Medium-Low:** the actor still sometimes publishes the *pre-seek* cached position after the seek reply (`playback_actor.rs:676-710`): seen in 1/7 drags and 1/21 paused exact seeks, for up to ~100 ms. That can flicker the thumb and time after keyboard ±10 s or a click while paused. Fix: hold the optimistic position for a request until `seeking=no` or `PLAYBACK_RESTART` (event 21), or until `time-pos` changes after the reply. |
| **Online search results clipped** on the virtualized Videos page. | **Fixed:** bounded, scrollable search pane (`library_ui.rs:1348-1355, 3281-3291`). | Focus and scroll keys not fully re-reviewed. |
| **Delete fails with "Unknown torrent"** for a torrent-sourced video whose transfer was already removed. | **Fixed:** `release_for_deletion` returns `Ok(())` when the torrent is absent (`torrent.rs:902-909`). | Not executed. **Medium-Low:** the UI adds the library's saved identity as an owner unconditionally (`library_files_ui.rs:478-482`). If the same infohash is active somewhere else, Delete removes that other live transfer. With no session running, a release request starts a full session, with public discovery when enabled, just to reply (`services.rs:599-605`). Release only transfers whose snapshot lists `ticket.path`, and never start a session to release. |
| **Re-download after delete never reappears**, because tombstones block `TorrentCompleted` and `Completed` even for downloads the user explicitly started (`library_ui.rs:~583-603`). | **In progress:** `services.rs` sends `Event::DownloadAccepted` (`services.rs:633-640, 869-878`). No UI arm had been wired at 14:04. | Re-review once the UI clears or bypasses tombstones for user-started downloads. HTTP and yt-dlp `Completed(path)` need the same treatment. |

## 5. Medium

- **N3. Deleting a folder never prunes its videos** — [static]. `absent` requires the parent directory to exist and be readable (`library_files.rs:815-820`). The test `absent_parent_is_unavailable_and_never_pruned` (`library_files.rs:1287`) pins this behavior. Deleting a season folder, which is common, leaves every episode as a normal-looking card that fails to open. Unavailable entries have no visible marker (`library_file_unavailable` is used only for posters).
  - **Fix:** store the identity of an ancestor (or of the volume root) in `FileLocation`. If the nearest existing ancestor matches the saved location and is readable, and it lacks the next path component, the subtree is gone. As a fallback, show a "Missing" badge with a one-click "Remove missing".
- **M8. HTTP resume trade-offs that regress HEAD** — [static]. This follows the HTTP note's "trades bandwidth for correctness", but two points are worth stating plainly:
  - A checksum-free download from a server without a strong validator **restarts from zero on every mid-session reconnect** (`http.rs:684-690`; HEAD resumed with `Range`).
  - A SHA-1-protected download restarts whenever the redirect target URL changes, even though the SHA-1 check already guards integrity (`http.rs:719-731`, `ResumeIdentity::matches`). Mirror rotation at archive.org-style hosts would cause this.
  - **Suggestion:** within a session, allow resume when `Content-Range`'s total matches and the ETag or Last-Modified value is unchanged, even if weak. For SHA-1 parts, compare the validator value only.
- **M9. A metadata-only change to the playing file stops playback** — [static]. A changed length or modification time with the same inode (`library_files.rs:333-335`) triggers `release_library_playback` (`library_files_ui.rs:237-241`). `touch`, tagging or backup tools can stop playback mid-film. Stop only when the inode is replaced.

## 6. Low or optional

- **L1. The main player view still re-renders the whole `Flick` root on every video frame** — [GPUI]. The window root is an uncached `AnyView` (`gpui-0.2.2/src/view.rs:166-181`, `window.rs:1346, 2036`). Notifying `VideoFrameView` (`ui.rs:604`) dirties the window, and `draw_roots` calls `Flick::render`. The child entity only isolates PiP from the main window, which matches the PiP trace. Optional: put the player chrome in its own entity rendered with `.cached(...)`.
- **L2. `Writer::flush` can wait forever** — [static]. If the writer thread died (panicked), `flush` waits on `completed_generation` with no end (`store/persistence.rs:190-209`). Only `shutdown` checks the join result. Add a liveness flag.
- **L3. History rows can misalign** — [static]. The History `uniform_list` uses `filter_map` at row level (`library_ui.rs:~2747-2765`), so it can return fewer elements than the requested range and shift rows and IDs. Emit a placeholder row instead.
- **L4. Posters can bypass the bounded cache** — [static]. `PosterCache` is created lazily in `page_body` (`library_ui.rs:1187`). The palette (`palette_ui.rs:440`) and the player (`player_ui.rs:903`) draw posters through `chrome_ui.rs:94-124`. Before the library has been visited, for example when launched with a video path, those posters go into GPUI's keep-everything asset cache. Create the cache in `Flick::new`.
- **L5. Native watching can be lost for good, and focus sweeps are unbounded** — [static]. After any notify error, `watcher = None` (`library_files.rs:606`) and it is never reinstalled; the only `recommended_watcher` call is at line 464, despite the comment. That leaves 30 s full polling. Every window activation also triggers a full stat sweep (`library_files_ui.rs:145-151`). Reinstall the watcher, and rate-limit focus reconciliation.
- **L7. A deferred open gives no feedback** (`ui.rs:782`). B1 makes this much worse.
- **L8. Focus does not return to the card** — [static]. Cancelling Delete moves focus to the root (`library_files_ui.rs:566, 584`) instead of the originating card, so keyboard users lose their place.
- **L9. Remove has no undo** — [static]. The visible per-card **Remove** (`library_ui.rs:1782-1800`) deletes viewing history permanently with no confirmation or undo (`store.rs:463`). Consider an undo toast.
- **L10. The palette lowercases every title on every render** (`palette_ui.rs:143-167`). This code is unchanged by this pass. Use `normalized_title`.
- **L11. Torrent path matching has one edge case.** `incomplete_torrent_file` dropped its `canonicalize` fallback (`ui.rs:854-865`). It now relies on output folders being canonical at add time (`torrent.rs:~603-605`). That holds unless a path component is later replaced by a symlink or bind mount.

## 7. Design trade-offs and accuracy of claims

- **Accurate:** the actor keeps libmpv on a worker it owns. Queues are bounded and coalesced. Optimistic snapshots are guarded by a revision. Frames are filtered by generation. Removal markers are durable. Trash moves only the selected file, never deletes permanently, and keeps siblings. Settings are coalesced, written atomically and synced, and the writer is drained at quit. No filesystem calls remain on the UI thread (checked by grep across `ui.rs`, `library_ui.rs`, `chrome_ui.rs`, `player_ui.rs`, `pip_ui.rs`, `palette_ui.rs`, `settings_ui.rs`, `fetch_ui.rs` and `library_files_ui.rs`).
- **Scope of the measurements:** "46× Back" is one path: a 1k library in a debug build, detected by X11 pixel changes under Lavapipe. Idle was measured only at 1k, with posters already generated. I could not make these claims from the evidence:
  - a 10× speedup of the whole app;
  - 5k or 10k idle behavior;
  - behavior on large-display grids (H2);
  - startup pruning at scale (H1);
  - main-window frame cost (L1).
- **Frame mailbox (unsafe code), checked in detail.** It is sound:
  - Every non-null pointer stored in the slot belongs to the single `FrameSender`, which is not `Clone`, takes `&mut self`, and is moved into the render thread.
  - Every reader removes a pointer by swap, or by CAS for the producer's own pointer, so each Box has exactly one owner.
  - ABA is impossible because only the producer stores non-null pointers.
  - Both operations on `value` are read-modify-writes. `clear()`'s `fetch_add` on the epoch is therefore visible to the producer's later epoch load whenever its swap reads after `clear()`'s swap, so no stale frame stays in the slot.
  - Close-then-final-swap leaves no message behind, `Notify` registration happens before the check, and `FrameMailbox::drop` frees the last frame.
  - One nit: idle `advance_generation` does an epoch `fetch_add` and a swap on every actor iteration while the target is (0,0). It is harmless.
- **Settings writer:** order and generations are correct. Only an adjacent request without a reply is coalesced. Synchronous saves act as barriers. Dedup is reset after a failure. Windows uses `MOVEFILE_REPLACE_EXISTING|WRITE_THROUGH`, and Unix syncs both the file and its directory. Remaining nit: L2.
- **Delete dialog keyboard handling:** GPUI dispatches bound actions before key listeners (`window.rs:3834-3846` before `finish_dispatch_key_event`), so ⌘Q still works while the dialog is open, and other keys are trapped as intended.

## 8. Suggested additional validation (after fixes)

1. **B1:** for each failure (corrupt `session.json`, deleted output folder, `session.lock` already held), confirm that a local file outside the roots opens immediately and one inside a root shows a visible message.
2. **H1:** with 10k generated entries, delete one file while Flick is closed, relaunch with posters missing, and measure time until the card disappears. Repeat while scrolling and while a file is loaded.
3. **H2:** use a 3840×2160 window at 1× (or override the scale factor) on All Videos with more than 96 visible posters. Measure idle CPU over 60 s and confirm posters settle.
4. **H3:** delete one file from a generated multi-file local torrent that is still downloading. Siblings must not become playable.
5. **H4:** on macOS, Delete with Automation permission denied. On Linux, Delete on a second filesystem without `.Trash-$uid`.
6. Check idle CPU and RSS on a 5k library after its visible posters settle, and measure main-window player-view frame cost (`Root view rendered` per second).

## 9. Checks I executed (for main to copy into `docs/VALIDATION.md`)

All of these are focused backend checks against the actual repository source files, included by path. None of them is a desktop build or interactive playback. I ran no cargo commands against the repository's own manifest or target directory, to avoid contending with concurrent agents.

1. **Filesystem worker harness**, `/tmp/claude-review-files-FYn164`.
   - Setup: the real `src/library_files.rs` plus pinned `notify` 8.2.0, `trash` 5.2.9, `tokio` and `libc` from the repository `Cargo.lock`. Since about 13:56 it also uses a one-type local `flick` shim crate that provides `library::FileLocation`. Generated temporary files only.
   - Command: `CARGO_TARGET_DIR=/tmp/claude-review-target cargo test --offline review_ -- --test-threads=1 --nocapture`.
   - Run at about 13:43, before `FileLocation` existed (2 passed): a never-seen absent file under an existing empty directory was reported `Missing`. Under the old global-revision rule, 0 `Missing` were accepted and 21 dropped over 6 s of per-tick changes, converging only once changes stopped.
   - Run at about 13:56 against current source (2 passed): the same absent file is now `Unavailable`; the old-rule starvation result is unchanged (0 / 21).
   - Run at about 14:03, adding a model of the per-path epoch rule (3 passed): a deleted file's `Missing` was accepted after 1.0 s while another path's epoch changed every tick.
2. **Native actor harness**, `/tmp/claude-review-actor-hF8Orq`.
   - Setup: the real `src/playback.rs` and `src/playback_actor.rs`, the system libmpv 0.37, `FLICK_AUDIO=null`, and the generated fixture `/tmp/flick-playback-implementation-20261025.mp4`. `ffprobe -skip_frame nokey` shows keyframes at 0.000 s and 8.333 s.
   - Command: `FLICK_AUDIO=null FLICK_TEST_VIDEO=… claude_review_actor-df7d3263fe1d3222 review_paused_drag --ignored --nocapture --test-threads=1`, run 7 times.
   - Results: a paused keyframe seek to 6.0 s settled at 0.0 s in 7/7 runs, after about 100–300 ms. The pre-seek position was published briefly in 1/7 drags and 1/21 paused exact seeks. The exact release stayed at the target in 7/7.
3. `ffprobe` listing of the fixture's keyframes, as above.

No public network traffic, trackers or DHT were involved. Only generated temporary files were created or deleted.
