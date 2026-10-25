# Independent review, round 2 — performance and deletion changes (Claude, 2026-10-25)

Reviewer: Claude Opus 5.5, the read-only review sub-agent the user authorized for this round. Six read-only Claude subagents helped, one per area: B1, H1/H4, H2, H3, seek/quit and HTTP.

This is a new, independent round, not a continuation of round 1 ([claude-review.md](claude-review.md)). I checked [review-responses.md](review-responses.md), [report.md](report.md) and `docs/VALIDATION.md` against the code, and re-read every finding below in the current source myself before including it.

Ground rules:
- No application source was edited and nothing was committed. This file is the only file added to the repository.
- `docs/VALIDATION.md` was not modified. §7 lists the checks for main to copy there.
- No cargo command ran against the repository manifest or `target/`. Harnesses, copies and target directories are all under `/tmp/claude-review-r2*` and `/tmp/claude-r2-actor-*`.
- No public network, trackers or DHT were used:
  - Transfer checks used `TransferConfig::local` (127.0.0.1, public discovery off) with generated fixtures.
  - HTTP checks used a loopback server written for this review.
  - The Trash check used an isolated `HOME`/`XDG_DATA_HOME`.
  - No real media or user Trash was touched.

## Verdict

**Changes requested.**

Several fixes are real and well built:
- typed transfer availability, with a visible Retry and Cancel;
- canonical saved-ownership roots that keep valid roots when another record is unreadable;
- a separate proof lane for startup observations;
- the per-frame poster working set (H2 is fixed);
- a durable save barrier awaited before any owner release, now on both release paths;
- `renameat2(RENAME_NOREPLACE)` Linux Trash with no copy or unlink fallback;
- writer liveness;
- in-order `DownloadAccepted` delivery.

The integration still has defects that undo the B1 and H3 goals in ordinary flows:

| ID | Severity | Summary |
| --- | --- | --- |
| **R2-B1** | **Blocking** | Delete, or Remove · Keep Files, on a transfer that is still *Checking* permanently quarantines **every** file of that torrent, including complete videos the user could already play. The lockout survives restarts and nothing in the app clears it. `fastresume: false` makes every saved torrent go through Checking after each launch. **Reproduced (harness).** |
| **R2-H1** | High | After a second transfer startup, `Progress` is deduplicated against the previous session's snapshot, and the UI waits for it forever. Triggers: a lazy engine start for a magnet, .torrent or search result, or Retry. For the rest of the session, Flick-downloaded videos (those with a torrent identity) cannot be opened, deleted or given posters, and the notice has no Retry button. **Reproduced (harness).** |
| **R2-H2** | High | Linux Delete fails on every FAT/exFAT/NTFS removable drive, and whenever the home Trash is not mode 0700, because every Trash directory must pass `mode & 0o077 == 0`. The failure is safe (the file is kept), but the feature does not work. **Home-Trash case reproduced (harness).** |
| R2-M1 … R2-M10 | Medium | See §3. They include: an HTTP restart loop that never ends (**a regression from HEAD, reproduced**); a transfer that vanishes from the UI after Download is clicked again; on Windows, an unplugged drive that blocks all local files; Missing still waiting 10–20 s behind Unavailable entries (**reproduced**); quit blocked during Share hashing; the seek fence freezing the shown time for 10 s on video under 20 fps (**reproduced, native**); and the Delete stop acknowledgement arriving before libmpv closes the file (**reproduced, native**). |

**What to fix:**
- **Must fix:** R2-B1, R2-H1 and R2-H2.
- **Fix, or record as explicit limits, before claiming the goals are delivered:** R2-M1 (regression), R2-M2, R2-M3, R2-M7 and R2-M9.
- **Can follow later:** the remaining items.

**Response claims that do not hold as written:**

| Claim | What the code does |
| --- | --- |
| "Ready alone never implies empty ownership" | True in the UI, but the service does not always send the `Progress` that must follow (R2-H1). |
| "10k … Missing in 1.654 s" | Holds only with a 20 ms drain and no Unavailable entries. At the real 500 ms tick, with offline entries, it takes 10–20 s (R2-M4). |
| "Linux native Trash … private owned directories" | Over-strict: it rejects normal FAT/exFAT/NTFS and umask-created Trash directories (R2-H2). |
| "Drops stale prior snapshot until current verification" | Nothing guarantees a fresh snapshot ever arrives (R2-M2). |
| "Eventual recovery" for seeks | The only recovery is a 10 s timeout (R2-M9). |
| Stop acknowledges "only once mpv has unloaded its file" (`playback_actor.rs:370`) | It acknowledges before END_FILE (R2-M10). |
| "No native cancellation deadline invented" | The quit hang in R2-M6 is in Rust and librqbit code, not in a native call. |

## 1. Scope, sources and method

- **Base:** HEAD `71587ecac24801d0d1eb3d67fe6dec9451034931` plus the uncommitted tree.
- **Snapshots:** the review started at 14:23:51 UTC. Checksums were taken at the start, at 14:37 (with a full copy in `/tmp/claude-review-r2/snap-1437/`), and at 14:54 (`/tmp/claude-review-r2/snapshot-final.sha256`).
- **The tree still changed during the review:**
  - `src/ui.rs` at 14:27 (small change).
  - `src/library_ui.rs` at 14:36: `send(Remove…)` now quarantines incomplete files and waits on a durable barrier before releasing a transfer. This closes a gap I had found.
  - `src/ui.rs` at 14:39: `prepare_quit` now bumps `open_generation`.
- **Line numbers refer to the 14:54 state.** Short hashes:

  | File | Hash |
  | --- | --- |
  | `services.rs` | `a8e326c9c85c` |
  | `ui.rs` | `bba9a68baac0` |
  | `library_ui.rs` | `bebb76618d23` |
  | `torrent.rs` | `4dfa0a228342` |
  | `library_files.rs` | `f74031fedb2d` |
  | `library_files_ui.rs` | `220fa04a054c` |
  | `playback_actor.rs` | `b71aa4e8fa23` |
  | `fetch/http.rs` | `06944c5b634c` |

  Anything edited after 14:54 UTC has not been reviewed.
- **Evidence labels:**
  - **[exec]**: a backend harness run in this round (§7). None of these is a desktop build or interactive playback.
  - **[static]**: traced through the code, with file:line references.
  - **[dep]**: checked against pinned dependency sources (`gpui-0.2.2`, `librqbit-9.0.1`, `trash-5.2.9`).

## 2. Blocking and High findings

### R2-B1 (Blocking). Releasing a transfer while it is Checking permanently locks out all of its files — [exec by subagent; static]

**Mechanism**
1. Both release paths mark **every** file of the owning transfer when its state is `Checking` or `Error`:
   - library Delete, `library_files_ui.rs:557-576`;
   - Downloads "Remove · Keep Files", `library_ui.rs:503-520`.
2. The marks are made durable, and only then is the release sent.
3. `release_for_deletion` (`torrent.rs:1068-1075`) calls `remove()`, which waits up to 120 s for initialization and then calls `session.delete` straight away (`torrent.rs:1096-1117`).
4. The 700 ms progress publisher never observes the finished, Live state. So the `Progress` handler (`library_ui.rs:716-741`) never clears a mark, and after the release no snapshot or `TorrentCompleted` can arrive for that infohash.
5. Nothing else can clear the marks:
   - Import and open never clear them (`torrent_integrity.rs:27-53`, `ui.rs:806-815`).
   - Downloading again into the same folder is refused, because `<folder>/<infohash>` already exists (`torrent.rs:772`).
   - Share creates a different infohash, so the identity check never matches (`torrent_integrity.rs:63-81`).
6. `fastresume: false` (`torrent.rs:1679`, unchanged from HEAD) puts every saved torrent in Checking after each launch, for seconds to minutes depending on its size.

**Trigger**
1. Keep a completed, multi-file season torrent in Downloads. With seeding off it is paused, but it still rechecks at launch.
2. Launch Flick.
3. While Downloads still shows "Checking files…", choose Delete… → Move to Trash on episode 1.
4. When the check finishes, the dialog reports "Moved to Trash".
5. Every other episode now fails with "This file is not fully verified. Resume its download before opening it.". This persists across restarts, and there is no download left to resume.

Remove · Keep Files during Checking has the same effect. For a shared folder, it locks out the user's own original files.

**Executed by a subagent** (`/tmp/claude-review-r2-h3`, loopback only, generated 64 KiB + 768 MiB fixture, no peers):
- After a restart the transfer was Checking, and the UI rule, copied verbatim, marked 2 of 2 files.
- `release_for_deletion` returned Ok after 1.97 s. The 700 ms samples were Checking, Checking, then absent.
- Afterwards there were zero snapshots and both files were byte-identical on disk.
- `completed_file` returned "Unknown torrent", and downloading into the same folder again failed with EEXIST.

**Fix (either)**
- Refuse a release while any owner is Checking ("Wait until file checking finishes"). The Keep Files message already says this, but the guard only tests the global `!transfers_known`.
- Or have `release_for_deletion`/`remove` return the per-file verified set read **after** `wait_until_initialized` and **before** `session.delete`, and clear the marks for selected, fully verified files from that result before the Trash step. This is sound because nothing writes to the files after the release.

**Test to add:** a Delete started during Checking must leave complete siblings playable after a restart.

### R2-H1 (High). After a second transfer startup, `Progress` is deduplicated and the UI can wait forever — [exec; static]

This leaves round-1 B1 only partly closed. B1 asked to "never leave the UI waiting on an event that will not come".

**Mechanism**
- `ProgressPublisher` is created once per services loop (`services.rs:592`).
  - `empty()` (`services.rs:992-997`) and `publish()` (`services.rs:1052-1058`) send `Progress` only when the snapshots differ from `last`.
- Every `start_session` call sends `Checking` first (`services.rs:478`). This includes:
  - a lazy start triggered by a pending Resolve, Share or PauseAll (`services.rs:693-699`);
  - Retry (`services.rs:783-789`).
- The UI's handling of these events:
  - `Checking` sets `transfers_known = false` and drops the roots (`library_ui.rs:692-696`).
  - `Ready` does nothing (`:697`).
  - Only `Progress` sets `transfers_known` back to true (`:737`).
- The new session's snapshots usually equal the old `last`: resolving registers no torrent, and `Ok(None)` again yields `[]`. So `Progress` never comes. The `Load` sent by the Downloads page is deduplicated as well (`services.rs:799-801`).

**Effect in the UI**
- `can_open_before_transfers` (`ui.rs:772-785`) refuses every library video with a torrent identity.
- Opening one stays pending under "Checking saved downloads before opening this video…" (`library_ui.rs:1495`). That notice has Cancel but no Retry, because availability is `Ready`.
- Posters for those videos are skipped (`ui.rs:1059-1064`).
- Delete is refused with "Retry downloads before deleting this file" (`library_files_ui.rs:544-549`), but no Retry button is shown.
- Up Next and next-episode autoplay into those videos silently do nothing (R2-L4).
- Only a restart recovers.

**Trigger**
1. A library video keeps its torrent identity without a saved transfer. This is the normal result of Remove · Keep Files, and of Delete on a sibling, which releases the whole torrent. No other transfers are saved.
2. Launch Flick.
3. Paste a magnet link, open a .torrent, or click a torrent search result. Then cancel the file sheet, or let the resolve fail.
4. Click the video. It waits forever.

**Second trigger:** a lazy start fails (Unavailable with empty, complete roots). Retry then yields `Ok(None)`, and `empty()` is deduplicated.

**Executed:** `/tmp/claude-review-r2-services`, using the real `services.rs` copied byte-for-byte.
- Event log: `Checking, TransferRoots(0), Ready, Progress(0), Checking, TransferRoots(0), Ready, Resolved(ok=false)`, then nothing after `Load`.
- The modelled `transfers_known` ended `false`.
- The B1 subagent found the same defect independently.

**Fix**
- Set `progress.last = None` whenever a startup completes, both `Ok(Some)` and `Ok(None)` (`services.rs:709-721`).
- Do the same in `accept_download` (this also fixes R2-M2).
- Add a UI-level test: after a second startup, a video with a torrent identity either opens or shows Retry.

### R2-H2 (High). Linux Trash rejects any Trash directory that is not mode 0700 — [exec by subagent for the home Trash; static, from kernel behaviour, for FAT]

**Mechanism**
- `linux_trash_directories` (`library_files.rs:1207-1229`) requires `mode & 0o077 == 0` for the root, `files` and `info` of **every** candidate. That covers the home Trash and `$topdir/.Trash-$uid`, not only the shared `$topdir/.Trash`.
- On vfat, exFAT and ntfs3, permission bits come from mount options. udisks2's defaults set `uid=` but no `dmask`, so directories show as 0755 and `chmod` cannot change them. Flick creates `/media/$USER/X/.Trash-$uid` with mode 0700 and then rejects it.
- A home Trash created by another tool with `create_dir_all` (0755/0775) is rejected the same way. `trash` 5.2.9 itself creates it like that.
- GIO/Nautilus only require a directory owned by the user for these locations.
- The error shown is from whichever candidate was tried last, because `last_error` is overwritten (`library_files.rs:1191-1201`).

**Trigger:** Delete any video on a typical FAT32, exFAT or NTFS USB or external drive. It always fails, leaving an empty `.Trash-$uid` behind.

**Executed by a subagent** (`/tmp/claude-review-r2-files`, isolated `HOME`, `XDG_DATA_HOME` and Trash root):
- With directories at 775/775/775 the call returned `Err(… writable Trash … Permission denied)` and the file was kept.
- After `chmod 0700` it returned Ok, with one file and a `.trashinfo` whose Path is absolute.
- The FAT/exFAT case was not mounted, because that needs root.

**Fix**
- For the home Trash and `.Trash-$uid`, require only a directory that is not a symlink, is owned by the user, and is on the same device.
- Keep the strict sticky-bit and ownership checks for the shared `$topdir/.Trash`.
- Report the most specific error.

## 3. Medium findings

- **R2-M1. HTTP: a restart loop that never ends when the redirect target changes on each request. Regression from HEAD** — [exec by subagent; static re-traced].
  - **Mechanism:** the resume identity includes the final redirected URL (`fetch/http.rs:173-175`). A 206 from a different signed URL fails the match, so `restart()` runs and the loop continues **without counting a failure** (`http.rs:715-728`, and the 416 path at `:750-764`). The fresh 200's first bytes then reset `failures = 0` (`:815-818`).
  - **Trigger:** a 302 to a per-request signed CDN or S3 URL with a stable strong ETag, plus a connection that drops mid-file. The download restarts from zero forever with no error. Bandwidth is wasted; disk use stays bounded.
  - **Loopback results (subagent):** changing redirect with SHA-1: 27 restarts in 14 s, never past 64 KiB (HEAD: done in 7.1 s). Checksum-free: still looping at 14 s (HEAD: 7.0 s). The alternating-ETag variant loops in both trees.
  - **Fix:** count every restart as a failure, or cap restarts per file. Reset `failures` only when `part.len` passes the highest length reached so far. Remove the redirect target from the identity; for SHA-1 parts compare only the validator value (round-1 M8).
- **R2-M2. Clicking Download again for a transfer already listed can make it vanish from the UI indefinitely** — [static].
  - **Mechanism:** `download_to` returns the existing transfer when the selection is identical (`torrent.rs:759-764`), and `DownloadAccepted` is still sent (`services.rs:900-917`). The UI then drops the transfer from `self.downloads` (`library_ui.rs:746-747`). An idle Live transfer's snapshot does not change, and `resume` returns early when the transfer is already Live (`torrent.rs:1033-1037`), so the publisher never resends it.
  - **Effects:**
    - The transfer disappears from Downloads.
    - Its unselected and non-video files leave `incomplete_files`, so they can be opened and get posters.
    - Its selected videos cannot stream.
    - A library Delete finds no owner and sends the file to Trash while the transfer still runs.
  - **Fix:** set `last = None` in `accept_download` (`services.rs:981-990`).
- **R2-M3. An unplugged Windows drive, or a non-NotFound I/O error on a saved folder, blocks every local file** — [static; Windows not run].
  - **Absent drive:** for `E:\Flick\<hash>` with E: absent, the ancestor walk reaches `E:\`. There `parent()` returns None, which gives "no available ancestor" (`torrent.rs:241-243`).
  - **I/O error:** ERROR_NOT_READY or a stale mount (EIO/ESTALE) fails at `torrent.rs:245`.
  - **Effect:** either way `roots_complete = false`. The UI drops all roots (`library_ui.rs:703-704`), and every local file on every drive waits; Retry fails the same way. This is round-1 B1's own trigger ("unplug the drive holding a download") on a target platform. It is visible now, but it still blocks everything.
  - **Fix:** use the absent volume root, in canonical `\\?\` form, as a complete boundary. On other errors, use the saved path itself as the boundary: it was canonical when saved and has been validated as absolute with no `..` (`torrent.rs:96-106`).
- **R2-M4. A Missing still waits behind floods of Unavailable observations (H1 residual)** — [exec by subagent].
  - **Mechanism:** Missing, Unavailable, Metadata and recovery share one FIFO (`library_files.rs:863-891`). It holds at most `RESULT_LIMIT = 256` (`:19`) and is drained once per 500 ms library tick (`ui.rs:1294-1300`). The worker never wakes the UI.
  - **Measured** at startup with one file deleted while Flick was closed:

    | Offline entries | Drain interval | Missing accepted after |
    | --- | --- | --- |
    | 5k | 500 ms | 10.1 s |
    | 10k | 500 ms | 20.4 s |
    | 10k | 20 ms | 1.3 s |

  - **Fix:** put Missing and recovery ahead of Unavailable, and drain within a budget or notify the UI.
- **R2-M5. Applying Missing results has no time budget, and each costs O(n)** — [static; magnitude estimated].
  - **Mechanism:** results are applied without a budget (`library_files_ui.rs:232-325`). For each Missing:
    - `remove_library_record` calls `remove_video` and then `invalidate()` (`store.rs:463-523`), and the next result's `video()` rebuilds the whole index;
    - `retain_sources` clones n paths (`library_files_ui.rs:470-471`).
  - **Trigger:** delete 256 or more episodes of a 10k library in a file manager while keeping their folder.
  - **Effect:** roughly a second of UI stall per 256 removals in a release build. Not measured.
  - **Fix:** apply all removals in one pass, with one invalidate and one `retain_sources`.
- **R2-M6. Quit is blocked for minutes during Share hashing** — [static, dep].
  - **Mechanism:** shutdown runs `task.abort(); task.await` on the transfer job (`services.rs:846-849`). librqbit's `create_torrent` hashes in a synchronous loop with no await point (`librqbit-9.0.1/src/create_torrent_file.rs:112-124`), so the abort cannot take effect until hashing finishes.
  - **Effect:**
    - `finish_shutdown` has no deadline.
    - A second ⌘Q is ignored (`shutdown.rs:46-50`).
    - The window stays open and paused.
    - A native macOS termination blocks inside `on_app_quit` (`shutdown.rs:28`).
  - **Fix:** put deadlines on the job join and on `session.shutdown()`, or run the hashing on a blocking thread that can be abandoned.
- **R2-M7. Posters can stay ungenerated after transfer state becomes known** — [static; found by two subagents].
  - **Mechanism:** candidates are recomputed only when `posters_dirty` is set (`ui.rs:1059-1064`), and these events do not set it:
    - `TransferRoots` (`library_ui.rs:713-715`) and `Ready` (`:697`) never do.
    - `Progress` sets `transfers_known` but not the flag.
    - `transfers_changed` sets it only when the incomplete set changes (`ui.rs:973-980`).

    The first tick, at 100 ms (`ui.rs:536`), usually clears the flag while those files are still gated.
  - **Trigger:** relaunch with a saved, complete torrent video that has no poster, and do not scroll. The placeholder stays.
  - **Fix:** set `posters_dirty` when `transfers_known` turns true, and on `TransferRoots`.
- **R2-M8. Windows: a permanent delete the user confirms is then reported as "Moved to Trash"** — [dep; Windows not run].
  - **Mechanism:** on Windows `native_trash` calls `trash::delete` (`library_files.rs:1136-1139`). trash 5.2.9 passes `FOF_NO_UI | FOF_ALLOWUNDO | FOF_WANTNUKEWARNING` (`trash-5.2.9/src/windows.rs:97`). For items the Recycle Bin cannot take (over its size limit, removable FAT drives, some shares), Windows then shows its own "permanently delete?" prompt from the worker thread.
  - **Effect:** accepting that prompt destroys the file, and Flick still reports "Moved to Trash" (`library_files_ui.rs:661`) after promising "You can restore it there" (`:724`).
  - **Fix:** drive `IFileOperation` without `FOF_WANTNUKEWARNING` and treat "cannot recycle" as an error.
- **R2-M9. The seek fence can freeze the shown time for 10 s and hide later position changes** — [exec, native, by subagent; static].
  - **Mechanism:** the fence is released only when the native position is within 0.05 s of the target and `seeking=no`, or after 10 s (`playback_actor.rs:22-23, 427-447`). Until then every snapshot shows the target (`:783-787`).
  - **Why it stalls:** an exact seek lands on the first frame at or after the target. Below about 20 fps that frame can be more than 50 ms away, so only the 10 s timeout releases the fence. `Work::Command` also clears the fence only for `stop` (`:646-650`), so frame-step and chapter commands stay hidden.
  - **Knock-on effects:**
    - ←/→ compute their target from the frozen value.
    - `persist` saves the frozen position.
    - `buffering()` can light up for torrent streams.
  - **Native results (generated 24 and 10 fps fixtures):**

    | Case | Result |
    | --- | --- |
    | 24 fps, while playing | 0/30 froze |
    | 10 fps, while playing | 6/15 froze for ≥1.5 s; one held at 3.013 for 10.01 s, then jumped to 13.0 |
    | 10 fps, paused | 5 frame-steps stayed hidden until the timeout |

    The retained native test covers only paused seeks at 30 fps (`tests/playback_actor_backend/harness.rs:141`).
  - **Fix:** after a new restart with `seeking=no`, release once the position has left the pre-seek value or lies within target + one frame interval (`estimated-vf-fps`) + speed × elapsed. Clear the fence on every `Work::Command`.
- **R2-M10. The stop-for-delete acknowledgement can come before libmpv has closed the file** — [exec, native, by subagent; static]. Medium-Low on Linux/macOS; likely Medium on Windows.
  - **Mechanism:** release is inferred from `idle-active=yes && playlist-count=0` (`playback_actor.rs:591-608`). In libmpv 0.37 both are true as soon as `stop` is processed, before END_FILE (10/10 raw trials).
  - **Native results:**
    - The file descriptor was still open right after Ok in 10/20 runs in one batch and 2/20 in another; it closed 7–9.5 ms later.
    - A protocol stream was opened *after* the acknowledgement.
  - **Impact:** Trash on Windows can hit a sharing violation. It fails visibly and safely, but this is exactly what the barrier is meant to prevent. A torrent owner's release can also start while the stream opener is still active.
  - **Fix:** if the player is already idle before `stop`, acknowledge immediately. Otherwise wait for the stopped entry's END_FILE, then for idle, keeping the 10 s Err timeout.

## 4. Low findings (follow-ups)

- **R2-L1. HTTP downloads are saved with mode 0600.** The part file is created 0600 (`http.rs:341-349`) and published by hard link (`:908-924`). HEAD gave 0664 under umask 0002. Media servers running as other users lose access. [exec by subagent]
- **R2-L2. A torn HTTP sidecar disables resume for that path forever and leaks a hidden `.identity` file.** The sidecar is rewritten in place (`http.rs:450-454`). After a crash mid-write, `load` rejects it and `create_new` keeps failing with EEXIST (`:405-436`). Write to a temporary file and rename it instead.
- **R2-L3. HTTP re-download restore uses a global removal epoch** (`fetch_ui.rs:166, 189-200`). Any unrelated Remove, Delete or automatic Missing during the download (`library_files_ui.rs:275, 432`) keeps an explicitly re-downloaded, tombstoned path hidden. This is conservative.
- **R2-L4. Opens deferred from the Player view are silent.** Up Next (`ui.rs:1263-1269`), next/previous episode and the palette can set `pending_open`, but its notice renders only in library pages (`library_ui.rs:1485-1512`).
- **R2-L5. Retry sets Checking before the send succeeds** (`library_ui.rs:1469-1478`). If the transfer worker is gone, it shows "Checking" forever.
- **R2-L6. Quarantine marks are never removed, and marked cards look normal.**
  - A marked entry is never pruned, because Missing is suppressed for incomplete paths (`library_files_ui.rs:268-272`).
  - A stale mark blocks any later file at the same path.
  - The "Resume its download" text is wrong once the transfer has been released.
  - Drop the mark after a successful Trash, or after a proven Missing with no live owner, and add a badge.
- **R2-L7. The quarantine fails open on a corrupt `settings.json`.** Loading falls back to defaults, and the next save overwrites the file (`store.rs:766-806`). Keep a backup copy of the corrupt file.
- **R2-L8. O(n) UI work remains on hot paths:**
  - `library_files_changed` rebuilds and diffs a `TrackedFile` per entry on every global revision bump (`library_files_ui.rs:71-140`, called at `:170-172`). The revision bumps on each poster result, each `remember()` and each metadata change.
  - The palette scans the whole library on every render (`palette_ui.rs:158-172`), which happens per video frame over the player.
  - Each poster result rebuilds the All Videos query (`ui.rs:1029-1032`).
- **R2-L9. Poster edge cases:**
  - Palette posters are never scheduled over list pages. The list callbacks clear the visible set (`library_ui.rs:85-87, 2431, 2758, 2817, 2969`) after `Flick::render` recorded them.
  - With 96 or more posters visible there is no spare capacity (`library_ui.rs:274`).
- **R2-L10. Linux Trash details:**
  - The identity is checked and then the file is renamed by path (`library_files.rs:1112-1116` vs `:1309-1316`). Only the Trash destination can be affected, so this is recoverable.
  - An EXDEV on a bind mount is not retried with that mount's `.Trash-$uid`.
  - NFS and FUSE without rename2 return EINVAL, and get a generic message.
- **R2-L11. Some Delete waits are unbounded.**
  - A writer stuck in fsync (for example on a dead network home) leaves the dialog Working forever, and Escape is disabled (`library_files_ui.rs:689`).
  - Trash shares the worker that stats every entry, so a dead hard-mounted NFS entry stalls Delete (`library_files.rs:784-807, 953-1081`).
- **R2-L12. A second Flick process misreports or over-blocks.** Linux and Windows have no single-instance hand-off (`main.rs:49-52`), and three problems follow:
  - The lock-held state says "Restore folder access".
  - If the live instance's session has no saved records, `confirm_saved_transfers_empty` fails (`torrent.rs:307-317`) and every local file, including the one double-clicked, is blocked.
  - A held lock *with* records is reported as complete roots, although the live instance can still add more transfers.
- **R2-L13. Marks and live paths are compared byte for byte.** On case-insensitive or normalizing volumes (macOS, Windows), a torrent path that differs only in case or normalization escapes both gates. Plausible; not run.
- **R2-L14. On Windows, file identity is creation time only** (`library_files.rs:395-400`). Creation-time tunnelling weakens both M9's replaced-file detection and the Trash ticket. Plausible; not run.
- **R2-L15. Several libmpv workers are never joined at exit.** This includes `flick-player-control` (`playback_actor.rs:249-263, 401-405`) and the poster and preview workers (`thumbnails.rs:211-221, 397-407`). Their teardown races `exit` on macOS and `main` returning elsewhere. This was round-1 N1's recommendation and has not been done.
- **R2-L16. The window stays live during the asynchronous quit.**
  - The next tick restores the render target (`ui.rs:1234-1244`).
  - Space can unpause.
  - `OPEN_REQUESTS` is still processed (`ui.rs:1112-1119`), and `Player::load` sets `pause=no` (`playback.rs:870-871`).
  - Since 14:39, in-flight opens are cancelled by the `open_generation` bump.
  - Add a quitting flag, and hide the window or show "Quitting…".

## 5. Round-1 items after round 2

| Round-1 item | Response claim | Round-2 status | Evidence and residual |
| --- | --- | --- | --- |
| **B1** startup failure blocks local playback | Typed availability, complete roots, a visible pending state with Retry and Cancel | **Partly fixed** | Verified: the states (`services.rs:138-148`); roots sent before engine start (`services.rs:479-511`); a missing folder maps to its nearest canonical ancestor (`torrent.rs:224-247`); one startup at a time, with in-order events and Retry (`services.rs:783-789`); the notices (`library_ui.rs:1437-1512`); no filesystem work on the UI thread. Residual: **R2-H1**, R2-M3, R2-M7, R2-L4, R2-L5, R2-L12. |
| **H1** startup pruning backlog | Separate proof map, 3 ms budget, deltas | **Partly fixed** | Verified: the proof lane (`library_files.rs:174-176, 287-307, 814-875`), acknowledgement guards (`:643-660`), and deltas sent to the worker (`library_files_ui.rs:123-141`). Residual: R2-M4, R2-M5, R2-L8; the UI still polls every 500 ms. |
| **H2** poster cache thrash | Protect the current and previous frame's working set | **Fixed** | `library_ui.rs:229-296`. The frame boundary is `Flick::render` (`ui.rs:1958`), whose root is an uncached `AnyView`, and Flick has no `.cached()` views, so nothing is evicted within a frame. The test covers only `PosterLru`. If L1 is ever done with `.cached()`, a reused scene could reference evicted textures. |
| **H3** sibling bytes after a release | Durable marks before release; gated reads | **Partly fixed** | Verified for Delete (`library_files_ui.rs:551-634`) and, since 14:36, Keep Files (`library_ui.rs:492-552`). The barrier is awaited off the UI thread. Every open and poster path is gated (`ui.rs:806-815, 900-923, 1059-1064`). Marks survive reload (`torrent_integrity.rs:111-119`). Clearing needs the exact path, infohash and index. Residual: **R2-B1**, R2-M2, R2-L6, R2-L7, R2-L13. |
| **H4** Trash backends | NSFileManager on macOS; native Linux Trash with no copy | **Partly fixed** | macOS: `DeleteMethod::NsFileManager` matches the trash 5.2.9 API (`library_files.rs:1124-1130`); not run on a Mac. Linux: an info file and then `renameat2(NOREPLACE)` only, never a copy or unlink (`:1235-1340`). Residual: **R2-H2**, R2-L10; Windows: R2-M8. |
| Re-download after delete | DownloadAccepted lifts accepted tombstones and marks new destinations | **Mostly fixed** | Ordering verified: completions are held while a job runs, and DownloadAccepted is sent inside the job (`services.rs:649, 913, 1018`). Residual: R2-M2, R2-L3. |
| Delete releasing another live owner, or starting a session | Owners come only from live snapshots; a release never starts a session | **Fixed** | `library_files_ui.rs:551-556`; `services.rs:675-692`. |
| Seek flicker | Restart counter plus position fence | **Partly fixed** | Verified: the restart count is captured before submission, a restart from seek N cannot release seek N+1 (`playback_actor.rs:419-447, 687-716`; `playback.rs:1176-1181`), and the thumb follows the pointer while dragging (`ui.rs:1212-1216`). Residual: R2-M9. |
| M9 metadata change stops playback | `Metadata.replaced` | **Fixed on Unix** | `library_files.rs:1023-1032`, `library_files_ui.rs:310-318`. Windows: R2-L14. |
| N1 quit audio and lifecycle | `prepare_quit` pauses and idles; asynchronous cleanup | **Partly fixed** | Pause and idle are queued at once (`ui.rs:296-306`). Since 14:39 in-flight opens are cancelled. The native barrier runs after GPUI clears its windows (`shutdown.rs:15-66`; gpui `app.rs:698-719`). Residual: R2-M6, R2-L15, R2-L16. |
| L2 writer liveness | WorkerLifetime | **Fixed** | `store/persistence.rs:52-90, 105`. Waits are rechecked under the same mutex, so no wakeup is lost. A failure masked by a later full-snapshot success is harmless. |
| L3 History rows | One placeholder per stale row | **Fixed** | `library_ui.rs:2964-2990` |
| L4 cache bypass | Cache created in `Flick::new` | **Fixed** | `library_ui.rs:63-68`, `ui.rs:427` |
| L5 watcher and focus sweeps | Watcher reinstalled; sweeps throttled | **Fixed** | `library_files.rs:756-764, 594-609` |
| L10 palette lowercasing | Normalized titles | **Correctness fixed** | Still O(n) per render (R2-L8). |
| L7 deferred open feedback | Visible notice | **Fixed in library pages** | R2-L4 covers the Player view. |
| N3 folder deletion | Accepted limit | **Acceptable** | The card shows "Unavailable" (`library_ui.rs:1799-1812`) and keeps Remove. |
| M8 HTTP trade-offs | Accepted limit | **Limit acceptable; implementation is not** | R2-M1. |
| L8, L9, L11 (round 1) | Not claimed | Unchanged | Cancel still focuses the root, Remove has no undo, and L11 is unchanged. |

## 6. Assessment of the stated limits

1. **"Uncertain or corrupt ownership blocks direct reads, with Retry."** Acceptable as a policy for state that is genuinely unreadable or malformed. The implementation applies "uncertain" more widely:
   - after a *healthy* second startup (R2-H1) — **not acceptable**;
   - for an absent Windows volume or a stale mount, where a sound boundary exists (R2-M3);
   - for a second instance, with a misleading message (R2-L12).
2. **"Absent parent → visibly Unavailable, with Remove."** Acceptable, and the marker is verified as visible.
3. **"The main player root still renders every frame; the PiP library is isolated."** Acceptable. Do not fix it with `.cached()` without also changing how posters are protected (§5, H2 row).
4. **"Conservative HTTP restart without a strong validator."** Acceptable as a policy. The implementation also treats the redirect target as identity and does not count restarts, which produces an endless loop (R2-M1, a regression).
5. **"No macOS/Windows execution, and no GPU or speed claim."** Acceptable. Static review still found platform risks that the report should list: R2-M3, R2-M8, R2-M10, R2-L13 and R2-L14.

The performance claims are unchanged from round 1. "46× Back" holds only for the path measured: a 1k library, debug build, Xvfb and Lavapipe. I did not re-measure desktop timings.

## 7. Checks executed in round 2 (for main to copy into `docs/VALIDATION.md`)

Every check below is a **backend harness check**: not a desktop build, and not interactive playback. None ran cargo against the repository manifest or `target/`. All were loopback-only with generated fixtures, and none used trackers or DHT.

1. **Services startup re-publication** (run by me). **Failed as expected, which demonstrates R2-H1.**
   - **Setup:** `/tmp/claude-review-r2-services`. The real `src/services.rs` (sha256 `a8e326c9…f6b555`) is copied byte-for-byte, with a review-only test module appended to the copy, using the shims and manifest of main's services harness.
   - **Build:** dependencies were reused by copying that harness's target into `/tmp/claude-review-r2-target-services`.
   - **Command:** `CARGO_TARGET_DIR=/tmp/claude-review-r2-target-services cargo test --offline --no-default-features --bin flick-services-checks claude_review_r2 -- --test-threads=1 --nocapture`
   - **Result:** 0 passed, 1 failed. Event log: `Checking, TransferRoots(0), Ready, Progress(0), Checking, TransferRoots(0), Ready, Resolved(ok=false)`; final `transfers_known=false`. A local `TransferConfig::local` session was used, resolving invalid bytes, with no peers.
2. **Release during Checking** (subagent). **Demonstrates R2-B1.**
   - **Setup:** `/tmp/claude-review-r2-h3`, using the real `flick::torrent` through the repository's `lib.rs`.
   - **Build:** `CARGO_TARGET_DIR=/tmp/claude-review-r2-target-h3 cargo build --offline --bin flick-h3-checks --no-default-features`, then the binary was run.
   - **Fixture:** a generated 64 KiB + 768 MiB local fixture, `TransferConfig::local`, no peers.
   - **Result:** see R2-B1. The run folder was deleted afterwards.
3. **Filesystem worker** (subagent).
   - **Setup:** `/tmp/claude-review-r2-files`, with the real `src/library_files.rs` via `#[path]` and the round-1 shim.
   - **Build:** `CARGO_TARGET_DIR=/tmp/claude-review-r2-target-files cargo test --offline --no-run`.
   - **Tests run** (each passed, 1 test):
     - `review::review_missing_queues_fifo_behind_unavailable_at_ui_tick` (table in R2-M4);
     - `review::review_home_trash_created_with_umask_mode_refuses_every_delete`, with isolated `REVIEW_TRASH_ROOT`, `XDG_DATA_HOME` and `HOME` under `/tmp` (R2-H2).
4. **HTTP resume** (subagent).
   - **Setup:** `/tmp/claude-review-r2-http`, with the unmodified `src/fetch/http.rs` via `#[path]` and HEAD's version from `git show`. Built with `CARGO_TARGET_DIR=/tmp/claude-review-r2-target-http --offline`.
   - **Server:** a loopback HTTP/1.1 server serving a 512 KiB body, with every response cut after 64 KiB.
   - **Results:**
     - Stable redirect with SHA-1: both trees completed in 7.1 s.
     - Changing redirect target, with or without SHA-1: the working tree was still looping at 14 s; HEAD completed in about 7 s.
     - Alternating ETag: both trees loop.
     - Cancelled runs left their folders empty.
     - Published file mode was 600 in the working tree and 664 at HEAD.
5. **Native playback actor** (subagent). Linux, system libmpv 0.37, `FLICK_AUDIO=null`.
   - **Setup:** `/tmp/claude-r2-actor-exp` includes the repository's `src/playback.rs` and `src/playback_actor.rs` by absolute path, with the lockfile from `tests/playback_actor_backend`.
   - **Fixtures:** generated with ffmpeg in `/tmp/claude-r2-actor-fixtures/`: `v24.mp4` (24 fps, 60 s) and `v10.mp4` (10 fps, 40 s).
   - **Build:** `CARGO_TARGET_DIR=/tmp/claude-review-r2-target-actor cargo test --offline --locked --no-run`.
   - **Runs:** `FLICK_AUDIO=null <test binary> <test> --ignored --test-threads=1 --nocapture`.
   - **Results:**

     | Case | Result |
     | --- | --- |
     | Exact seeks while playing, 24 fps at 1×/2×/4× | 0/30, 0/30, 0/20 froze |
     | Exact seeks while playing, 10 fps at 1× | 6/15 froze for ≥1.5 s; one held for 10.01 s |
     | Paused frame-steps at 10 fps | Hidden until the timeout |
     | Stop during a stream open | Acknowledged in 3 ms, before the stream opened |
     | File descriptor open after acknowledgement | 10/20 and 2/20 runs |
     | Raw `idle-active` after `stop` | Already yes before END_FILE, 10/10 |
     | Pause toggle publishing stale state | 0/40 |

Static subagents for B1 and H2 executed nothing.

## 8. Suggested validation after fixes

1. **R2-B1:** use a generated multi-file local torrent and restart Flick so that it is Checking. Delete one file during the check. After a restart, every complete sibling must open. Repeat with Remove · Keep Files.
2. **R2-H1:** start with no saved transfers and one library video with a torrent identity. Resolve a magnet, then cancel. The video must open. Add a UI-level test of the event handling.
3. **R2-H2:** Delete on a loop-mounted vfat or exFAT image mounted with udisks defaults, and with a home Trash at mode 0755. Both must move the file to Trash.
4. **R2-M1:** serve a loopback 302 to a URL that changes on each request, and cut connections mid-body. The download must finish, or fail after a bounded number of attempts.
5. **R2-M4:** 10k entries, 5k of them on an unplugged drive, and one local file deleted while Flick was closed. Measure Missing latency at the real 500 ms tick.
6. **R2-M9 and R2-M10:** run native exact seeks while playing a 10 fps fixture, and check that the descriptor is closed when stop-for-delete is acknowledged. Run the Windows Trash scenarios on Windows.
