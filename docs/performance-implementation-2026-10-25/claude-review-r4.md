# Independent review, round 4 — focused verification of the round-3 corrections (Claude, 2026-10-25)

Reviewer: Claude Opus 5.5, the read-only review sub-agent the user authorized. This is a new, focused round. It checks the fixes for R3-M1, R3-M2 and R3-L1, and the two related wording changes, against the frozen source. It is not a new whole-app audit: paths verified in earlier rounds were not re-read unless a round-4 change touched them. I used no subagents.

Inputs I read:
- [claude-review-r3.md](claude-review-r3.md) and [review-responses-r3.md](review-responses-r3.md), in its current version, which reports the 242/86 rerun and the GUI check. Earlier reviews and responses served as context only.
- The handoffs `.flick-test/performance-implementation/stop-r3.md` and `error-owner-r3.md`, and their logs.
- [report.md](report.md), [summary.html](summary.html) and `docs/VALIDATION.md` as of 16:05 UTC, plus the new evidence: `r4-*.png`, `r4-broken-delete.json` and `final-checks-r4.json`.

Ground rules:
- No application source, test, Cargo file or `docs/VALIDATION.md` was edited, and nothing was committed. This file is the only file I added to the repository.
- No cargo command used the repository manifest or `target/`. My harnesses, source copies, fixtures and targets are under `/tmp/claude-review-r4/`.
- No network, transfer peers, trackers or DHT were used.
- The native libmpv checks and the transfer-module tests ran as separate processes.

## Verdict

**R3-M1, R3-M2 and R3-L1 are fixed. There are no Blocking, High or Medium findings.**
- Each fix is correct and fits the surrounding code.
- Each has a regression test, and that test fails on the round-3 source.
- I confirmed every fix by running it myself (§5).

| Correction | Status | Executed evidence (this round) |
| --- | --- | --- |
| **R3-M1**: Delete after a failed load timed out | **Fixed** | My round-3 reproduction now acknowledges the first stop in **2.5–3.5 ms** (3 of 3 trials); before the fix it took 10.0 s and failed. Replacing a file and stopping at once: **40/40 Ok, and no descriptor open at acknowledgement**. |
| **R3-M2**: release of a transfer in Error locked its files forever | **Fixed, in the way R3 recommended** | **15/15** torrent tests pass on a hash-verified private copy, including the new real-librqbit Error regression. |
| **R3-L1**: wrong "file kept" message on Windows | **Fixed** | Windows cross-target strict Clippy passes on the final file. The recorded pass ran before the last edit (R4-L2). |
| R2-L6 follow-up: the open-time verification message | **Fixed** (nit N1) | Static review. |

New findings. None requires a source change before PR and merge:

| ID | Severity | Summary |
| --- | --- | --- |
| **R4-L1** | Low. UX only; the app fails closed. Needs documenting. | Suppose the cause of a transfer's Error persists. A full disk, the case that motivated R3-M2, is the example. Resume re-verifies the files, goes live, and can fall back into Error before Delete runs. In-app Delete and Remove · Keep Files then stay refused. The report's limits do not mention this. |
| **R4-L2** | Low. Validation record. | The recorded Windows cross-target Clippy pass ran before the final edit of `trash_windows.rs`. My rerun on the final file passes. The record should say which run covers the final file. |
| N1–N4 | Nits | Wording; see §3. |

**Recommendation:** make the three documentation updates (R4-L1, R4-L2, round-4 status) and proceed. They need no new review round.

If main instead changes the refusal text or the Resume behavior for R4-L1, that is a source change. It then needs the usual refreeze and focused check.

## 1. Scope and method

**Frozen source.** All 49 files in `review-r4-source-hashes.json` matched at the start (16:01:40 UTC), again before writing (16:27:41) and at the end (§5). During the review, main staged files for the PR. No file covered by the manifest changed.

**Baselines.** All eight production baselines named in the brief hash-match `review-r3-source-hashes.json`.

Between the round-3 and round-4 manifests, exactly these eight production files changed:
- `src/playback.rs`
- `src/playback_actor.rs`
- `src/torrent.rs`
- `src/library_ui.rs`
- `src/library_files_ui.rs`
- `src/ui.rs`
- `src/library_files.rs`
- `src/library_files/trash_windows.rs`

The round-4 manifest also covers five test and harness files for the first time. `Cargo.toml` and `Cargo.lock` are unchanged since round 3.

**Diffs.** Saved in `/tmp/claude-review-r4/diffs/`. The ten unified diffs total 637 lines. Of these:
- 187 are added lines for the new test in `torrent.rs`;
- 177 are the two diffs of the actor harness and the mock.

**Evidence labels:**
- **[exec]**: run by me in this round;
- **[static]**: traced in the code, with file:line references;
- **[dep]**: checked in the pinned `librqbit-9.0.1` source;
- **[reported]**: run by main or a worker. I inspected the logs or evidence but did not re-run it.

## 2. The corrections

### R3-M1. A stop after a failed load: Fixed [exec, native; static]

**What changed**
- **Counting.** `Player` counts START_FILE (`playback.rs:1158-1161`) and END_FILE (`:1167-1170`) only for the requested playlist entry.
  - An END_FILE is now also ignored while no entry has been requested. That state exists only before the first load.
- **Reset.** `load()` resets both counts and the "drained" flag right after it identifies the new entry (`:855-863`).
  - No events are polled between `loadfile` and that reset. So the new entry's START_FILE cannot be consumed before the reset.
  - Events for the replaced entry are filtered out by their entry ID.
- **Drained flag.** `poll_events()` marks the queue drained only when it reaches MPV_EVENT_NONE (`:1139`, `:1149-1152`).
  - If a poll hits its 256-event budget first, the decision waits for a later poll.
  - `files_ended()` also requires that the event queue never overflowed (`:978-982`).
- **Actor.** The actor no longer acknowledges a stop before polling; the pre-stop idle fast path is gone.
  - `Work::Stop` (`playback_actor.rs:852-872`) only creates the `PendingStop`.
  - The decision (`:693-710`) runs on a later loop iteration. It comes after `poll_events` (`:607`), which began after the synchronous `stop` returned.
  - Jobs are taken at most one per iteration, and none while a stop is pending (`:715`). So no load can run between the stop and its decision.

**Assessment**
- **R2-M10's protection is intact.** A file that started still needs its own END_FILE: idle with an empty playlist never acknowledges a stop by itself. The 20-trial descriptor test and the blocked-opener test still pass. [reported: `native.log`, 11/11]
- **Failed and never-started loads.**
  - A failed load has already sent both START_FILE and END_FILE, so its counts balance.
  - An entry that never started has sent neither, so its counts also balance.
  - Either way, the stop acknowledges.
- **Counts cannot underflow.** An END_FILE is counted only for the requested entry, and that entry's START_FILE comes first in the same queue.
- **Stale-event filters.** The filters for START/END, FILE_LOADED and PLAYBACK_RESTART from a replaced entry behave as before.
- **Queue overflow.** After MPV_EVENT_QUEUE_OVERFLOW, every later stop fails closed after 10 s until the player is recreated. This is intended, and I found no realistic way to trigger it.

**Executed [exec]**

| Check | Result |
| --- | --- |
| My round-3 reproduction, run against the frozen round-4 sources, 3 trials (round 3 ran 1) | First stop after "unrecognized file format": Ok in **2.54, 3.49 and 2.59 ms**, and the broken file's descriptor was closed. The second stop was Ok in about 2.5 ms. A loaded-file control was Ok in 202 ms. The round-3 source returned Err after 10.005 s. |
| New: load A (1280×720 H.264/AAC) and wait until it is loaded; then queue `load(B)` and `stop_for_delete` back to back; 20 trials per case | Same path: **20/20 Ok** (min/median/max 4.3 / 199.5 / 403.8 ms). Different path: **20/20 Ok** (194.8 / 399.5 / 402.9 ms). The replaced file's descriptor was open at acknowledgement in **0/40** trials; the new file's in **0/20**. |
| New: ctypes libmpv probe, with no Flick code; 7 cases × 15 trials | I evaluated the round-4 rule at each point: queue drained, counts balanced for the requested entry, `idle-active=yes`, `playlist-count=0`. **69 of 105** acknowledgements had zero starts: when a stop follows a replacement, the never-started case is real and common. The replaced file's descriptor was open at that moment in **0/105** trials. |

**Notes; no action needed**
- **I1.** By design, the balance covers only the requested entry. A replaced entry's close is not awaited.
  - This matches the UI, which stops playback only for its current path (`library_files_ui.rs:528-532`).
  - I observed no early acknowledgement in 145 trials.
  - The only exposed case is reloading the same path and then deleting it before the old entry's close finishes, which takes milliseconds. The UI cannot act that fast.
- **I2.** Suppose `load()` succeeds at `loadfile` but cannot read `playlist/0/id` (`:855-858`).
  - The counts keep the previous entry's balanced values. A stop could then acknowledge while the unidentified new entry is still loading. Round 3 would have timed out instead.
  - The read follows a synchronous `loadfile … replace`, and I found no way to make it fail.
  - Optional hardening: on that error path, mark the lifecycle as untrusted, as overflow already does.

### R3-M2. Releasing a transfer in Error state: Fixed, in the way R3 recommended [exec; static; dep]

**Library Delete**
- It refuses when any captured owner is in Error (`library_files_ui.rs:606-615`).
- The refusal comes after the existing Checking refusal. It also comes before the quarantine loop (`:616-637`), `queue_save`, the save barrier, the playback release (`:652`) and the backend release.
- So a refused Delete does not interrupt playback.

**Remove · Keep Files and Remove · Delete Files**
- Keep Files refuses an owner in Error (`library_ui.rs:509-520`), before its quarantine loop (`:521-542`) and before the save barrier.
- Delete Files stays available. The backend permits it with `delete_files || !Error` (`torrent.rs:1073-1076`).

**Backend**
- `remove(id, false)` refuses an owner in Error. It does so before reading metadata, deleting the transfer from the session, or changing its explicit-share mark.
- `release_for_deletion` therefore inherits the refusal.
- An absent owner still succeeds (`:1039-1046`), and an owner in Initializing is still refused first.

**Recovery is reachable**
1. Rows in Error now show **Resume** (`library_ui.rs:3147`, `:3229-3236`). It sends `Command::Resume` (`services.rs:933`), which calls `resume()`.
2. For an owner in Error, `resume()` calls `Session::unpause` before it waits (`torrent.rs:998-1000`).
   - librqbit's `start()` then replaces Error with a new Initializing state, synchronously, with `previously_errored=true` [dep: `torrent_state/mod.rs:430-445`].
   - That flag clears the fast-resume data [dep: `initializing.rs:198-204`]. Flick also sets `fastresume: false` (`torrent.rs:1633`).
   - As a result, `wait_until_initialized` no longer returns the stale error at once [dep: `mod.rs:567-588`], and every retained byte is re-hashed.
3. During that re-check the owner is Checking, so Delete gets the existing "wait" refusal.
4. Afterwards, the normal path applies. It quarantines only siblings that are unverified, unselected or incomplete.

**Stale UI snapshots fail closed in both directions**
- If the UI still shows a live owner that is now in Error, Delete marks only unverified siblings. The backend refusal then keeps the owner, and the marks clear after Resume.
- If the UI still shows Error for an owner that has recovered, Delete refuses until the next `Progress` arrives.

**Test hygiene**
- The failing storage decorator lives only in `mod tests` (`torrent.rs:1730` onward). Production code has no test hooks.
- The test uses `TransferConfig::local`, generated files and no peers.
  - The configuration listens on 127.0.0.1, with DHT, trackers, local discovery and UPnP all off (`torrent.rs:348-359`, `:1612-1641`).

**Executed [exec]**
- On a private copy whose 44 source and Cargo files hash-match the round-4 manifest, **15/15** `torrent::tests` pass.
- These include `error_owner_refuses_retained_file_release_and_recovers_after_resume` and the Checking regression.

**[reported]**
- By the worker: the negative control against the round-3 source, and the private Store/quarantine-marks extension.
- By main: the 242/86 integrated runs.

**Residual:** R4-L1, below.

### R3-L1. The Windows "file kept" wording: Fixed [static; exec cross-lint]

**All platforms** (`library_files.rs:1210-1211`)
- The wrapper no longer says "Your file and library record were kept".
- The new prefix, "Could not confirm that this file was moved to Trash.", is true on every platform.
- Linux keeps its definite explanation, "no file was copied or permanently deleted" (`:1224-1226`).

**Windows** (`trash_windows.rs`)
- If `PostDeleteItem` succeeds but returns no Recycle Bin item, the error now says the file was permanently deleted (`:34-35`, `:166`).
- The veto, abort and missing-callback paths keep `CANNOT_RECYCLE`, now worded as unconfirmed, with an instruction to check both the original folder and the Recycle Bin (`:33`, `:85`, `:148`).
- The operation flags and the veto are unchanged.

**UI**
- When Trash fails, the UI keeps the library record (`library_files_ui.rs:722-726`). A file that was in fact permanently deleted then goes through Missing, as round 3 suggested.

Windows runtime behavior has still not been executed.

### R2-L6 follow-up. The open-time verification message: Fixed (nit N1)

`ui.rs:827` no longer promises that re-adding the torrent will work. It suggests Resume only "If its transfer is still in Downloads". That advice:
- is accurate for an owner in Error, because Resume now recovers it;
- promises nothing for an owner that has already been released.

## 3. New findings

### R4-L1 (Low; UX, fails closed). Resume, then retry, can loop when the cause of the Error persists [static; dep]

**Mechanism**
1. Delete and Remove · Keep Files refuse an owner in Error. They tell the user to Resume the transfer, then retry.
2. Resume (`torrent.rs:993-1015`) re-verifies the files and then goes **Live**: `Session::unpause` starts the transfer unpaused, and Flick's `resume()` returns once it is live.
3. If the cause persists, the first failed block write ends the transfer again. A full disk, the round-3 trigger, is the obvious case. librqbit's fatal-error receiver calls `stop_with_error`, so the owner returns to Error [dep: `torrent_state/mod.rs:650-672`, `:322-333`].
4. Delete is refused again. A user who wants to delete a watched episode to free space cannot do it inside Flick.

**Trigger**
1. A season download fails with a full disk while peers are connected.
2. The user chooses Delete on an episode. It is refused.
3. The user clicks Resume. The row shows "Checking files…", which takes minutes for a large season.
4. The transfer goes Live, then returns to Error within seconds.
5. Delete is refused again.

**Workarounds that exist today**
- Click Pause while the transfer is still checking. Pause queues behind the Resume job (`services.rs:635-646`) and runs right after initialization, normally before any data arrives. Delete then works.
- Free space outside Flick.
- Delete the file outside Flick. The library then shows it as Missing.
- Use Remove · Delete Files, which deletes every file in the transfer.

Neither the refusal message nor the report mentions any of these.

**Secondary issue**
- `resume()` still waits at most 120 s for initialization (`:1001-1003`).
- A large transfer in Error can therefore report "Transfer failed: File verification timed out" while its re-check continues and later succeeds.
- The single transfer job slot is held for that whole time.
- This behavior already existed for transfers in Initializing. Rows in Error can now reach it too.

**Fix**
- **Before merge (documentation only):** add this to report.md's "Remaining behavior" and to VALIDATION's limits. Suggested wording: "Delete and Remove · Keep Files are refused for a transfer in Error, and Resume re-verifies it first. If the cause persists, for example a full disk, the transfer can fail again before Delete runs. Pause it as soon as checking finishes, free space, or delete the file outside Flick."
- **Optional, later:** either of these:
  - a verify-only path for a transfer in Error, which re-checks the files and leaves the transfer Paused;
  - refusal text that names the Pause step.

### R4-L2 (Low; precision of the validation record) [exec]

**The claim**
- VALIDATION's round-3 corrections say that cross-target Clippy for `/tmp/flick-windows-trash-check` passed "after the changes".
- `final-checks-r4.json` lists that check with exit code 0.

**What the artifacts show**
- That harness includes the repository's `trash_windows.rs` by absolute `#[path]`.
- Its newest artifact is from **15:46:55**.
- `trash_windows.rs` was last written at **15:50:16**, which matches the recorded rustfmt layout correction.
- No Windows-target artifact anywhere under `/tmp` is newer than that file.
- So the recorded pass did not lint the final bytes.

**My rerun:** I ran the same command in a private copy, with the pinned 1.99.0 toolchain. It re-checked the crate and passed with exit code 0 (§5, check 7).

**Fix:** record my rerun as the pass that covers the final file, or re-run the check in place. The 15:50 change looks like layout only, so this is not a behavior risk.

### Nits

- **N1.** The message at `ui.rs:827` also appears for files of a live transfer that were never selected for download, or are only partly downloaded (`incomplete_paths`, `ui.rs:915-937`).
  - For those files, Resume and re-checking never make them playable.
  - A transfer's file selection cannot be changed afterwards (`torrent.rs:756-759`).
  - Suggest adding: "Files that were not selected for download cannot be played."
- **N2.** Two places promise verification that cannot happen once the owner has been released:
  - `report.md:59`: "unverified siblings durably quarantined until reverified";
  - `summary.html`: "Partial torrent siblings stay protected until verified".

  After release, the app has no way to verify those files again, because adding the torrent again into the same folder is refused (`torrent.rs:768`). Suggest "…stay blocked unless a live transfer verifies them".
- **N3.** On a Windows veto, the full message says "Could not confirm …" twice ("Could not confirm … Windows could not confirm recycling …"). It is repetitive but true.
- **N4.** In `r4-delete-result.png`, the "Playback failed" banner for the deleted file still shows above the empty library. This is cosmetic and was already the behavior before this round.

## 4. Are the report, summary, responses and VALIDATION truthful?

**Yes, with the edits below.**

| Claim | Assessment |
| --- | --- |
| 242/86 tests, both strict Clippy modes, debug build, fmt, dependency isolation | [reported] Consistent with `final-checks-r4.json`. I did not re-run these, because the repository target belongs to main. |
| GUI: the first Delete after a failed load succeeds, in 85.8 ms | Consistent. The screenshots show the error, the confirmation and the empty library, and the JSON matches. |
| "balanced accepted START_FILE/END_FILE events before stop acknowledgement" (`report.md:61`) | True (§2). |
| 38 mock tests, 11 native tests, and the red run at 11.02 s | [reported] The logs match: `mock.log` shows 38 passed; `native.log` shows 11 passed, with 198.9 / 203.7 / 204.0 ms first stops; `red.log` shows 0 passed, 1 failed. |
| 15 torrent tests; the round-3 source fails the new test | I re-ran the 15 tests: all passed. The negative control is [reported]. |
| Windows cross-target lint "passed after the changes" | Not true as recorded (R4-L2). It is true now, through my rerun. |
| "Those follow-ups now pass … focused round 4 is in progress" (report, summary, responses) | Replace with this verdict. |
| Remaining limits | Add R4-L1. Apply the N1 and N2 wording. |

**Accepted limits.** The limits the brief lists are all still stated explicitly:
- **Uncertain ownership (R2-M3) is described as cross-platform.** It covers an unplugged Windows volume and non-NotFound errors on any platform. Ordinary Linux NotFound cases fall back to a conservative existing ancestor (`report.md:63`; VALIDATION line 89).
- **No hard cancellation of native or kernel I/O.** `report.md:37` and VALIDATION line 89 state this. The specific case of a hung mount, which stays Checking with only Cancel available, is implied rather than spelled out. Adding it is optional.
- **Platforms and hardware:** no native macOS or Windows execution, and no physical GPU, audio or sustained 4K checks.
- **Performance claim:** there is no whole-app 10× claim. The roughly 50× result is labeled as one Back diagnostic: a 1k library, a debug build, under Xvfb.
- **Rendering:** the main player root still renders every frame.
- **Folders, settings and file identity:**
  - Missing parent folders stay Unavailable.
  - A corrupt settings file can lose quarantine evidence.
  - Path aliases and Windows creation-time identity remain follow-ups.

The Blocking and High items fixed in earlier rounds stay fixed. Nothing in the round-4 diffs touches those paths beyond what §2 assesses.

## 5. Checks executed in round 4 (for `docs/VALIDATION.md`)

**What these checks are not.**
- All are backend or native-library checks. None is a desktop build, a GUI session or interactive playback.
- None ran cargo against the repository manifest or `target/`.
- None used the network, public peers, trackers or DHT.
- All media were generated locally.

**Executed by me**

1. **Frozen-source manifest**
   - **Command:** `python3 -I /tmp/claude-review-r4/scripts/verify_hashes.py <repo> docs/performance-implementation-2026-10-25/review-r4-source-hashes.json`
   - **Result:** 49/49 matched at 16:01:40 UTC, at 16:27:41, and at the end of the review (16:31:12).
2. **Baselines**
   - **Command:** `python3 -I /tmp/claude-review-r4/scripts/check_baselines.py …`
   - **Result:** all eight production baselines match `review-r3-source-hashes.json`. The round-3 → round-4 change set is as listed in §1. The diffs are in `/tmp/claude-review-r4/diffs/`.
3. **Generated fixtures,** in `/tmp/claude-review-r4/fixtures/`:
   - ffmpeg testsrc2, 320×180, 25 fps, 10 s, H.264 (`good10.mp4`, `other10.mp4`);
   - 1280×720, 30 fps, 12 s, H.264 with a 440 Hz AAC tone (`a720.mp4`, copied as `b720.mp4`);
   - 200,000 bytes from `/dev/urandom` (`broken.mp4`).
4. **Native actor harness** (`/tmp/claude-review-r4/actor/review.rs`)
   - **Setup:**
     - The harness includes the frozen `src/benchmark.rs`, `src/playback.rs` and `src/playback_actor.rs` by absolute `#[path]`.
     - Its lockfile is identical to `tests/playback_actor_backend/Cargo.lock`.
     - Its private target was copied from my round-3 target.
     - I checked that the built binary contains `files_ended`, so it was compiled from the round-4 sources.
   - **Build:** `CARGO_NET_OFFLINE=true CARGO_TARGET_DIR=/tmp/claude-review-r4/actor-target cargo test --offline --locked --manifest-path /tmp/claude-review-r4/actor/Cargo.toml --bin flick-actor-checks --no-run`
   - **Run:** `FLICK_AUDIO=null FLICK_HWDEC=no REVIEW_BROKEN=…/broken.mp4 REVIEW_GOOD=…/good10.mp4 REVIEW_A=…/a720.mp4 REVIEW_B=…/b720.mp4 REVIEW_TRIALS=20 <test binary> review_r4 --ignored --test-threads=1 --nocapture`
   - **Result:** 2 passed in 18.28 s. The numbers are in §2. Log: `/tmp/claude-review-r4/actor-r4.log`.
5. **libmpv ctypes probe**
   - **Setup:** no Flick code. System libmpv 0.37.0, with `vo=null`, `ao=null`, `idle=yes`, `keep-open=yes` and `config=no`.
   - **Command:** `python3 -I /tmp/claude-review-r4/scripts/mpv_replace_probe.py good10.mp4 other10.mp4 broken.mp4 15`
   - **Result:** 7 cases × 15 trials. When the round-4 condition first held, the replaced file's descriptor was open in 0/105 trials, and 69/105 acknowledgements had zero starts. Log: `/tmp/claude-review-r4/mpv-replace-probe.log`.
6. **Torrent module tests**
   - **Setup:**
     - A private copy of the frozen `src/`, `Cargo.toml` and `Cargo.lock`; all 44 hashes match the manifest.
     - A private target copied from the worker's target.
     - Local-only configuration, generated files and no peers.
   - **Command:** `CARGO_NET_OFFLINE=true CARGO_TARGET_DIR=/tmp/claude-review-r4/torrent-target cargo test --offline --locked --manifest-path /tmp/claude-review-r4/torrent/Cargo.toml --no-default-features --lib torrent::tests:: -- --test-threads=1`
   - **Result:** 15 passed, 0 failed, in 8.51 s. Log: `/tmp/claude-review-r4/torrent-tests.log`.
7. **Windows cross-target strict Clippy** on the final `trash_windows.rs`
   - **Setup:** a private copy of `/tmp/flick-windows-trash-check`.
   - **Command:** `CARGO_NET_OFFLINE=true CARGO_TARGET_DIR=/tmp/claude-review-r4/wincheck-target cargo +1.99.0 clippy --offline --locked --manifest-path /tmp/claude-review-r4/wincheck/Cargo.toml --target x86_64-pc-windows-gnu --tests -- -D warnings -A dead_code`
   - **Result:** exit code 0. The crate was re-checked after the 15:50:16 edit.
   - **Discarded attempt:** a first run without `+1.99.0` used the stable toolchain. Stable has no Windows standard library installed, so the run failed with E0463. That was a tooling problem, and I discarded the run.
8. **Toolchains.** Checks 4 and 6 used rustc 1.99.1 (stable), because my private directories have no `rust-toolchain.toml`. The repository pins 1.99.0. Check 7 used 1.99.0.

**Inspected but not re-run**

9. The evidence below:
   - the worker logs: `mock.log` (38 passed), `native.log` (11 passed) and `red.log` (0 passed, 1 failed, 11.02 s);
   - the GUI screenshots and JSON;
   - `final-checks-r4.json`.

**Static**

10. **Code:**
    - the round-3 → round-4 diffs of all eight production files and of the two harness files, with the call paths traced (§2);
    - librqbit 9.0.1's start, unpause, wait, fatal-error and `previously_errored` paths [dep].

**Not run by me**

11. These checks:
    - the integrated 242/86 suites and the debug desktop build, which main ran;
    - the private Store/quarantine-marks extension and the round-3 negative control, which the worker ran;
    - any GUI session, FAT mount, or Windows or macOS execution.
