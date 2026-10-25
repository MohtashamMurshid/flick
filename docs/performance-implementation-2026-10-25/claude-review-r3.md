# Independent review, round 3 — focused final verification (Claude, 2026-10-25)

Reviewer: Claude Opus 5.5, the read-only review sub-agent the user authorized for this round. As the brief asked, I did this round myself, with no subagents. It checks the round-2 fixes against the frozen code. It is not a new whole-app audit.

Inputs I read:
- [claude-review.md](claude-review.md) and [claude-review-r2.md](claude-review-r2.md);
- [review-responses.md](review-responses.md) and [review-responses-r2.md](review-responses-r2.md), including its final 241/85 update;
- [report.md](report.md) and `docs/VALIDATION.md`. Both were revised during this review, and I checked them again at the end. No source file changed;
- the four handoffs in `.flick-test/performance-implementation/`: `http-review`, `seek-review`, `services-final-review` and `files-final-review`.

Ground rules:
- No application source was edited and nothing was committed. `docs/VALIDATION.md` was not modified. This file is the only file added to the repository.
- No cargo command ran against the repository manifest or `target/`. My harnesses, copies and target directories are under `/tmp/claude-review-r3/`.
- No network, transfer session, peers, trackers or DHT were used. The only native checks were libmpv playback checks, kept separate from transfers.

## Verdict

**No Blocking or High findings remain.** Linux delivery can be described truthfully, but only if the limits in §4 are stated, including the new findings below.

Verified as fixed in the frozen code (§3):
- refusal to release a transfer while it is Checking, in the UI and in the backend;
- a fresh `Progress` after every readiness epoch and after every accepted download;
- Linux Trash on directories that are not mode 0700;
- bounded HTTP restarts, and umask-respecting file modes;
- separate urgent and bulk lanes for Missing;
- batched removal;
- cancellable Share hashing;
- posters marked dirty when transfer state becomes known;
- a recycle-only Windows backend (not run on Windows);
- the low-fps seek fence;
- the quitting guard.

Round 3 found three new problems:

| ID | Severity | Summary |
| --- | --- | --- |
| **R3-M1** | Medium-Low. **Regression from round 2.** | After a video fails to load, Delete on that video waits 10 s and fails with "Playback did not acknowledge file release; your file was kept". A second attempt works. Round 2's actor acknowledged the same stop in 5 ms. **Reproduced against the frozen actor with native libmpv.** |
| **R3-M2** | Medium-Low. Narrower trigger than R2-B1, but the effect is permanent. | R2-B1 is fixed for **Checking**, not for **Error**. Delete, or Remove · Keep Files, on a transfer in Error state still quarantines every file, including complete ones, and then releases the transfer, after which nothing can clear the marks. The new message "Re-add its torrent and select this file to verify" points to a remedy the app refuses. |
| R3-L1 | Low. Windows, not run. | If the recycle-only veto is ever bypassed, the error says "Your file and library record were kept" for an item that Windows reports as permanently deleted. |

What to do:
- **R3-M1** is a small fix, and I recommend making it because it is a regression. Under the brief, any material fix starts a new round.
- **R3-M2:** fix it, or list it as a known limit.
- **R3-L1:** correct the wording when the Windows backend is first validated on Windows.

Response claims that do not hold as written:

| Claim | What the code does |
| --- | --- |
| R2-B1: "Normal verified Progress then clears any existing exact markers" | Only while the owner is still live and not in Error. Once an owner in Error has been released, nothing clears its marks (R3-M2). |
| L6: "open error explains re-add/select to verify" | Adding the torrent again into the same folder is refused, because `<folder>/<infohash>` already exists (`torrent.rs:768`). The UI always passes the configured download folder (`library_ui.rs:3503-3507`). |
| R2-M10: "require fresh accepted end + idle/empty, unless already idle/empty BEFORE stop" | After a failed load, libmpv is idle but `playlist-count` is 1, and END_FILE was sent before the stop. No fresh END_FILE ever arrives, so the stop times out (R3-M1). |

## 1. Scope and method

**Frozen source.** All 44 files in `review-r3-source-hashes.json` matched at the start and at the end of this review. The manifest covers `src/`, `Cargo.toml` and `Cargo.lock`. It does not cover `tests/`, which includes the actor harness and `tests/transfers.rs`.

**Diffs.** I diffed each file against its exact round-2 final state. The hash prefixes match round 2 §1:

| File | Round-2 baseline |
| --- | --- |
| `services.rs` | `a8e326c9c85c` |
| `torrent.rs` | `4dfa0a228342` |
| `playback.rs` | `de1dbf5474d4` |
| `playback_actor.rs` | `b71aa4e8fa23` |
| `fetch/http.rs` | `06944c5b634c` |
| `library_files.rs` | `f74031fedb2d` |
| `library_files_ui.rs` | `220fa04a054c` |
| `library_ui.rs` | `bebb76618d23` |
| `store.rs` | `0cd4a5212ed7` |
| `fetch_ui.rs` | `6264fb5e2421` |
| `shutdown.rs` | `1714d3397a29` |

- `ui.rs` was diffed against round 2's 14:37 copy, so its diff also includes the 14:39 `open_generation` change.
- `torrent/share.rs` and `library_files/trash_windows.rs` are new files and were read in full.
- `Cargo.lock` only adds `windows 0.61.3` and `windows-core 0.61.2` to Flick's dependencies. No other version changed.

**Evidence labels:**
- **[exec]**: run in this round (§5). None of these is a desktop build or interactive playback.
- **[static]**: traced in the code, with file:line references.
- **[dep]**: checked against the pinned `librqbit-9.0.1` sources.
- **[doc]**: checked against vendor documentation.

## 2. Findings

### R3-M1 (Medium-Low, regression). Delete of a video that failed to load waits 10 s and fails — [exec, native; static]

**Mechanism**
1. Stop-for-delete now acknowledges only after a fresh accepted END_FILE, with the player idle and the playlist empty (`playback_actor.rs:694-712`). The one exception is a player that was already idle with an empty playlist before the stop (`:862-867`).
2. After a failed load, libmpv 0.37 has already sent END_FILE with reason ERROR, and it is idle, but it keeps the entry: `playlist-count` = 1. So the "already idle" exception does not apply.
3. `stop` then clears the playlist without sending another END_FILE. The accepted counter (`playback.rs:1152`) never advances, so the stop waits the full `STOP_TIMEOUT` of 10 s (`playback_actor.rs:19`) and returns Err (`:709`).
4. The UI keeps `self.path` after a playback error: `ui.rs:1221-1223` only sets the error. So library Delete calls `stop_for_delete` for that path (`library_files_ui.rs:528-532`).

**Trigger**
1. Open a broken or unsupported video. Flick shows "Playback failed: unrecognized file format".
2. Go Back, and choose Delete… → Move to Trash on that video.
3. The dialog shows Working for 10 s, then reports that playback did not release the file and that the file was kept.
4. A second Delete succeeds, because the first attempt cleared `self.path`.

The file is never at risk: the stop fails closed. But deleting a broken file is a common cleanup action, and it regressed from round 2.

**Executed** (§5, checks 3 and 4):

| Actor source | First `stop_for_delete` after the load error | Second | Control: a loaded file |
| --- | --- | --- | --- |
| Frozen round 3 | `Err(…your file was kept)` after **10.005 s** and **10.003 s** (two runs) | Ok in 5 ms | Ok in 0.20–0.40 s |
| Round-2 final | **Ok in 5.1 ms** | Ok in 5.1 ms | Ok in 0.20–0.40 s |

**Related gap** [static, rare]: the same wait happens if the stop arrives after `loadfile` but before mpv sends START_FILE for the new entry.

**Fix**
- Count accepted START_FILE events for the requested entry as well as END_FILE events.
- After the synchronous `stop` returns and events have been polled, acknowledge when started == ended, `idle-active` is yes, and the playlist is empty.

This keeps R2-M10's protection, because a file that started must end. It also acknowledges a file that already ended, or never started. Add the broken-file case to the native harness.

### R3-M2 (Medium-Low). Releasing a transfer in Error state still locks out its complete files permanently — [static; dep; not executed]

**Mechanism**
1. Both release paths still quarantine **every** file when the owner is in Error:
   - library Delete, `library_files_ui.rs:612-625`;
   - Remove · Keep Files, `library_ui.rs:511-530`.

   Only Checking is refused (`library_files_ui.rs:596-605`, `library_ui.rs:496-507`).
2. The backend refusal covers only `Initializing` (`torrent.rs:1064-1067`). So `release_for_deletion` goes ahead for an owner in Error, and the files are kept but the transfer is gone.
3. After that release, nothing can clear the marks:
   - `Progress` clears marks only for live owners outside Checking and Error (`library_ui.rs:728-744`).
   - Adding the torrent again into the same folder fails at `create_dir(<folder>/<infohash>)` (`torrent.rs:768`). Round 2's harness observed exactly this EEXIST.
   - Open and import keep marks by design.
4. The new open error, "Re-add its torrent and select this file to verify…" (`ui.rs:827`), leads the user into that refusal. The "Needs verification" badge (`library_ui.rs:1819-1829`) is accurate, but there is no way to clear it.

**When a transfer is in Error:** librqbit sets Error on a fatal disk write failure, for example a full disk (`librqbit-9.0.1/src/torrent_state/live/mod.rs:1869-1878`). It also sets Error when the initial check fails (`torrent_state/mod.rs:395-406`).

**Trigger:** a season download stops with a disk-full error. The user deletes a watched episode to free space. Every other episode, including the complete ones, is then permanently unplayable in Flick, unless the user renames or moves the files outside the app and imports them again.

**Workaround:** Resume in Downloads re-verifies the transfer, and Delete is then safe. The Delete dialog does not tell the user this.

**Fix (either)**
- Treat Error like Checking on both release paths: "Resume this transfer so its files can be verified, then retry." Remove · Delete Files stays available.
- Or let adding the torrent again adopt an existing `<folder>/<infohash>` through a paused verification, the way Share already admits existing files. That is the only safe way to clear a mark. If neither is done, rewrite `ui.rs:827` so it does not promise that remedy.

### R3-L1 (Low; Windows, not run). Wrong message if Windows permanently deletes despite the veto — [static; doc]

- In `trash_windows.rs:151-169`, a null Recycle Bin item with a successful result becomes `CANNOT_RECYCLE` ("The operation was refused…").
- `library_files.rs:1211` then adds "Your file and library record were kept."
- Microsoft documents a null `psiNewlyCreated` as meaning that the item was fully deleted. So this branch can only be reached after a permanent delete, and both statements would be false.
- The primary guard is the `PreDeleteItem` veto when `TSF_DELETE_RECYCLE_IF_POSSIBLE` is missing (`trash_windows.rs:140-150`). Electron's `DeleteFileProgressSink` uses the same technique (PR #3337), so the veto is plausible.
- **Fix,** when Windows is validated: say that the file was permanently deleted, and let the record go through Missing.

## 3. Round-2 items in the frozen source

| Item | Status | Evidence |
| --- | --- | --- |
| R2-B1 | **Fixed for Checking.** Residual: R3-M2 for Error. | The UI refuses before marking or saving (`library_files_ui.rs:596-605`; `library_ui.rs:496-507`). The backend refuses `Initializing` at once, with no wait (`torrent.rs:1064-1067`). The owner, the lock and the bytes are retained. Test: `removing_initializing_owner_is_immediate_retriable_and_preserves_verified_removal`. |
| R2-H1 | **Fixed** | `begin_ready_epoch` runs on both `Ok(Some)` and `Ok(None)` (`services.rs:712, 719, 983-989`). It resets only `last` and `error`, so completion identities survive. The failure path sends no empty `Progress` (`:723-731`). |
| R2-H2 | **Fixed.** A FAT/exFAT mount was still not exercised. | The home Trash and per-user Trash need only an owned, same-device, non-symlink directory (`library_files.rs:1344-1386`). The shared `.Trash` keeps the sticky-bit check and requires a root or self owner (`:1323-1341`). Errors from every candidate are reported, and the rename, no-copy and no-unlink paths are unchanged. |
| R2-M1 + L1 | **Fixed** | Restarts are bounded (`http.rs:624-629`). The failure count resets only past the high-water mark (`:693, 832-835`). Mirrors are assembled only when a checksum is published (`:738-744`). Part files are created 0666 filtered by umask, and sidecars stay 0600 (`:490`, `:341-349`). |
| R2-M2 | **Fixed** | `accept_download` clears `last` (`services.rs:991-993`). |
| R2-M3 | **Not fixed (stated limit)** | See §4. |
| R2-M4 | **Fixed** (backend measurement only) | Separate mailboxes and lanes, each capped at 256, with the urgent lane drained first (`library_files.rs:179, 300-313, 490-577`). A path moving between lanes is moved, never dropped. |
| R2-M5 | **Fixed** | One `remove_videos` call and one invalidation per batch (`store.rs:469-547`), and one `retain_sources` per batch. Marker identities and history are kept exactly as in the single-path version. |
| R2-M6 | **Fixed** (cancellation is cooperative) | All directory walking, file reads and SHA-1 run on one blocking worker that holds no engine (`torrent/share.rs:44-215`). Cancellation is checked around every read of at most 64 KiB. Drop guards sit in `create` and `share` (`torrent.rs:800-801`), and cancellation is checked again before admission (`:837`). Shutdown aborts the job, and `session.stop()` is bounded: it pauses initial checks, then sleeps 1 s (dep). The piece layout matches librqbit's fixed 2 MiB choice (`create_torrent_file.rs:50-53`), with a partial final piece and no empty piece. Sorting the walk changes the infohash of new multi-file shares, as stated. |
| R2-M7 | **Fixed** | `posters_dirty` is set on `Ready`, on `TransferRoots`, and when transfers become known (`library_ui.rs:705-707, 724-725, 748-750`). |
| R2-M8 | **Implemented, not run** | Single-threaded COM, with `CoUninitialize` only after a successful init and after every COM object has dropped. A per-item sink. `FOF_NO_UI \| FOFX_RECYCLEONDELETE \| FOFX_EARLYFAILURE \| …` without `WANTNUKEWARNING`. A `PreDeleteItem` veto, a `PostDeleteItem` result and destination check, and an abort or missing callback treated as an error. Residual: R3-L1. |
| R2-M9 | **Fixed** | Tolerance of one frame interval from `estimated-vf-fps`, bounded to 50 ms–1 s (`playback_actor.rs:806-819`). The playing trajectory is bounded, and positions the pre-seek clock could still reach are excluded (`:444-471`). Commands and properties that move the position retire the fence (`:517-538, 751-753, 775-777`). Stated residual: a delayed observation that overlaps the pre-seek trajectory still relies on the 10 s recovery. |
| R2-M10 | **Fixed** for loaded files and stream opens. Regression: R3-M1. | `playback_actor.rs:694-712, 860-879`; `playback.rs:1146-1152`. I confirmed the loaded-file and end-of-file paths natively (§5). |
| L3 | **Fixed** | Per-path generations are compared with the job's admission epoch (`fetch_ui.rs:190-191`, `library_files_ui.rs:481-484`). |
| L4 | **Fixed** | A deferred open returns to the Library (`ui.rs:805-808`), which pauses playback unless PiP is open. |
| L5 | **Fixed** | `library_ui.rs:1487-1497` |
| L6 | **Badge fixed; message not truthful** | See R3-M2. |
| L16 | **Fixed** | Guards in `ui.rs:299, 600, 743, 1126, 1322, 1335, 1404, 1719, 1988` and `library_ui.rs:493`. PiP controls go through the guarded `command`. The "Closing…" render replaces every other handler. `shutdown.rs` notifies the root. |

## 4. Do the stated limits prevent truthful Linux delivery?

**No, if the following is stated plainly.**

1. **R2-M3 on Linux.**
   - **What already works:** the usual removable-drive case. udisks removes the mount point, so the walk reaches the nearest existing ancestor (`torrent.rs:220-241`), and only files under that ancestor wait.
   - **When M3 applies:** only when a saved download folder returns an error other than NotFound:
     - permission denied;
     - ENOTCONN from a disconnected sshfs or rclone FUSE mount;
     - ESTALE or EIO from NFS or a failing disk.

     In those cases every local video waits until Retry succeeds. It fails closed and stays visible, with Retry and Cancel.
   - **A hung hard mount** keeps the state at Checking, with Cancel but no Retry. This is part of the stated kernel-I/O limit.
   - **Wording:** `VALIDATION.md` states this correctly: "stale nonNotFound I/O … gate[s] unrelated local opens". `report.md:61`, however, calls it a "Known **Windows** limitation", which implies Linux is unaffected. For a truthful Linux claim, label that part cross-platform and give the Linux examples above.
2. **Windows.** M3 covers B1's own trigger, an unplugged drive letter. So B1 must not be claimed as fixed on Windows, quite apart from the lack of any Windows or macOS execution. The Windows Trash backend has only been cross-compiled and linted.
3. **R3-M1 and R3-M2.** If they are not fixed, list them as known limits.
4. **The other stated limits are acceptable as written:**
   - whole-folder Delete stays Unavailable;
   - forced cancellation of native or kernel I/O;
   - the main player root still renders every frame;
   - aliases on case-insensitive mounts.

   The fail-open quarantine on a corrupt `settings.json` (R2-L7) is the only stated exception to byte safety, so keep it listed.
5. **Report updates.** `report.md` was revised at 15:27, during this review. It now gives 241/85 and the backend lane measurement (Missing in 1.03 s at the real 500 ms drain, with 10,000 offline entries), and `VALIDATION.md` keeps 222/75 as history. Both are consistent. What remains:
   - relabel the M3 limit (item 1);
   - add R3-M1 and R3-M2, if they are not fixed;
   - replace "Focused round 3 is in progress" with this verdict.

   `report.md:59` lists "END_FILE before stop acknowledgement" among the fixes. That is accurate for loaded files, but R3-M1 is its exception.
6. **Performance claims.** These are unchanged: about 46–50× on the one measured Back diagnostic (1k library, debug build, Xvfb, Lavapipe). There is no whole-app 10× claim and no macOS or Windows native execution.

## 5. Checks executed in round 3 (for main to copy into `docs/VALIDATION.md`)

All of these are backend checks. None is a desktop build or interactive GUI playback. None ran cargo against the repository manifest or `target/`. No network, transfer session, peers, trackers or DHT were used.

1. **Frozen-source manifest:** `python3 -I` SHA-256 over `review-r3-source-hashes.json`. 44 of 44 files matched, at the start and at the end.
2. **Round-2 baseline diffs:** written to `/tmp/claude-review-r3/diffs/`, using the baselines in §1.
3. **libmpv END_FILE and idle probe**
   - **Script:** `/tmp/claude-review-r3/scripts/mpv_stop_probe.py`, using ctypes. It does not use Flick code.
   - **libmpv:** system 0.37.0 (`libmpv2 0.37.0-1ubuntu4`, client API 2.2), with `vo=null`, `ao=null`, `idle=yes` and `keep-open=yes`, as Flick sets them (`playback.rs:743-744`).
   - **Fixtures (generated):**
     - `broken.mp4`: 200 KB from `/dev/urandom`;
     - `ok10.mp4`: ffmpeg testsrc2, 160×90, 10 fps, 2 s, H.264.
   - **Command:** `python3 -I /tmp/claude-review-r3/scripts/mpv_stop_probe.py /tmp/claude-review-r3/fixtures/broken.mp4 /tmp/claude-review-r3/fixtures/ok10.mp4`
   - **Results:**

     | Case | Before stop | After stop |
     | --- | --- | --- |
     | Broken file | END_FILE reason 4 already sent; `idle-active=yes`, `playlist-count=1` | `playlist-count=0`, **no END_FILE** |
     | Loaded file | — | One END_FILE, reason 2 |
     | End of file with keep-open | — | One END_FILE, reason 2 |
4. **Actor check on the frozen sources versus the round-2 sources**
   - **Harnesses:**
     - `/tmp/claude-review-r3/actor/review.rs` includes the frozen `src/benchmark.rs`, `src/playback.rs` and `src/playback_actor.rs` by absolute `#[path]`;
     - `/tmp/claude-review-r3/actor-base/` includes the round-2 final files from `/tmp/flick-seek-review-20261025/before/src/` instead.
   - **Build:**
     - **Lockfile:** copied from `tests/playback_actor_backend/`.
     - **Frozen build:** `CARGO_NET_OFFLINE=true CARGO_TARGET_DIR=/tmp/claude-review-r3/actor-target-frozen cargo test --offline --locked --manifest-path /tmp/claude-review-r3/actor/Cargo.toml --bin flick-actor-checks --no-run`
     - **Baseline build:** the same command, with `--manifest-path /tmp/claude-review-r3/actor-base/Cargo.toml` and `CARGO_TARGET_DIR=/tmp/claude-review-r3/actor-target`.
   - **Run:** `FLICK_AUDIO=null REVIEW_BROKEN=…/broken.mp4 REVIEW_GOOD=…/ok10.mp4 <test binary> review_r3 --ignored --test-threads=1 --nocapture`
   - **Results:** in the R3-M1 table. Logs are in `/tmp/claude-review-r3/actor-{frozen,baseline}.log`.
   - **Discarded run:** one rebuild attempt in the shared target directory reused the round-2 build instead of compiling the frozen sources. I discarded that run and rebuilt the frozen check in a separate target. I confirmed which sources each binary contains with `strings`.
5. **Not run in this round:**
   - the integrated suites (main reports 241 with desktop features and 85 without);
   - the HTTP, Share, files or Windows suites, for which the handoff commands were reviewed instead;
   - any desktop build, GUI session, FAT mount or Windows/macOS execution.

Sources for R3-L1 and R2-M8:
- Microsoft, [IFileOperationProgressSink::PreDeleteItem](https://learn.microsoft.com/en-us/windows/win32/api/shobjidl_core/nf-shobjidl_core-ifileoperationprogresssink-predeleteitem)
- Microsoft, [IFileOperationProgressSink::PostDeleteItem](https://msdn.microsoft.com/en-us/library/bb775728.aspx)
- Electron, [Fixing MoveItemToTrash behavior in Windows](https://ayakael.net/mirrors/electron/commit/4ff5ce4d6d4b8104c1dfca2b95f15ab847a2a3ed). I relied on its search summary; the page itself did not load.
