# Independent review, round 6: combined PR follow-up delta (Claude, 2026-10-25)

Reviewer: Claude Opus 5.5, the read-only review sub-agent the user authorized. This focused round covers only the combined delta against `6e2dc2b3b787f65b5a002d2fb0f87c67c4fa91b5`, which is the round-4 source plus docs. It is not a new whole-app audit. Round 5 was interrupted, and I did not reconstruct it. I used no subagents.

Ground rules:
- I edited no application, test or Cargo file, and no `docs/VALIDATION.md`.
- I made no commit and did not poll or watch the PR.
- This file is the only file I added to the repository.
- Cargo commands never used the repository manifest or `target/`. Private copies, targets and logs are under `/tmp/flick-claude-r6-review/` (written `$B` below).
- Transfer checks used generated fixtures, discovery disabled, and only the explicit loopback peers in the existing integration suite. No trackers or DHT were used.
- No playback ran. Transfer and playback checks were kept separate.

## Verdict

**There are no Blocking, High or Medium findings, and the R6 delta is approved for PR and merge.** All six changes are correct and fit the existing design.

One condition applies to R6-L1. Before merge, either document it as a known limit (docs only) or fix it in a follow-up that gets its own focused review. No source change is required for this approval.

| Change | Status | Strongest evidence |
| --- | --- | --- |
| 5. Sidecar identity preflight (`torrent.rs`) | **Correct and effective.** One new Low availability regression (R6-L1). | [exec] I removed only the preflight call to get a baseline. On that baseline, the overwrite reproduced: `session.json` changed from `4660dcdc…` to `ef2fdbce…`, absence confirmation of the original owner succeeded, Retry started, and `orphan-a.mp4` was in no live file list. On the frozen candidate, the JSON stayed byte-identical and the owner stayed saved. |
| 3. Multi-torrent import (`library_ui.rs`) | **Correct** for the synchronous paths. Low UX gap (R6-L2). | [static] One Resolve per batch. Busy, quitting and rejected admissions are all counted. |
| 2. Delete without worker (`library_files_ui.rs`) | **Correct** | [static] The visible error comes before reconciliation and generation lookup. |
| 4. Palette Quit (`palette_ui.rs`) | **Correct** | [dep] GPUI 0.2.2 `App::defer` signature checked. [reported] Root's GUI check: exit 0, no panic. |
| 1. Test portability (three tests) | **Correct.** No skips. Nit N1 (Windows margin). | [exec] Mutation test: both fixture sizes still detect a hasher that ignores cancellation. |
| 6. Audit reproducer and evidence JSON | **Correct** | [exec] All 36 exported files equal the `71587ec` blobs. The generated manifest type-checks with `--locked --offline` under the archived toolchain. Exactly 9 JSON values changed, each by prefix only. |

New findings. None requires a source change before merge:

| ID | Severity | Summary |
| --- | --- | --- |
| **R6-L1** | Low. Availability regression; fails closed. | The new 16 MiB sidecar limit is stricter than what Flick admits. A magnet or share whose metainfo exceeds 16 MiB still downloads, but every later startup refuses **all** transfers. Retry and the documented recovery cannot help. Before R6, such a session restored normally. |
| **R6-L2** | Low. UX only. | The "N torrent files were not started" notice is replaced when the started file's resolution fails asynchronously. Two cases: transfers are unavailable, or the first `.torrent` is invalid. Failures of this kind already replaced validation errors before R6. |
| N1 | Nit | On Windows, the 64 MiB cancellation fixture still detects a hasher that ignores cancellation, but only because debug-build hashing is slow (3.18 s against a 2 s deadline here). On Linux and macOS, detection is guaranteed by the fixture size. |
| N2 | Nit | An empty or unreadable sidecar used to take librqbit's magnet fallback; it now fails closed. In local sessions the baseline also failed closed (1.03 s). The change matters only with public discovery. |
| N3 | Nit. Docs. | `report.md` and `summary.html` state 244/88 passing without naming a platform. Root's `review-responses-r7.md` reports that Windows and macOS desktop jobs failed on `8bec302`. Qualify those counts as Linux results until desktop CI on those platforms passes. |

**Recommendation:**
1. Record R6-L1 as a known limit in `report.md` → Remaining behavior and in `docs/VALIDATION.md`, or take the follow-up fix in §2.1.
2. Optionally make the R6-L2 and N1–N3 wording updates. They are docs-only.

Any source change for R6-L1 or R6-L2 starts a new focused round.

## 1. Scope and method

**Frozen inputs.** All 50 files in `review-r6-source-hashes.json` matched at four points:
- 17:09:43 UTC (start);
- 17:17:21 UTC;
- 17:30:10 UTC;
- 17:38:32 UTC, the end of the review (§5).

`rust-toolchain.toml` is outside the manifest but unchanged since `6e2dc2b`.

**Root rebased and committed during the review.**
- HEAD is now `8bec302`. Its parent `dde771c` has the same tree as `6e2dc2b` (`a961fd6…`).
- Main is now `596a8aa`, whose tree equals `71587ec` (`463c6a3…`).
- So the reviewed delta is exactly `git diff 6e2dc2b 8bec302`, and its source bytes equal the frozen manifest.

**Round 7.** Root's untracked round-7 test-only candidates (`review-responses-r7.md`, `review-r7-candidate-hashes.json`) appeared during this review. They are outside its scope, and shared source still matched round 6.

**Private copy.**
- `git archive 8bec302` into `$B/candidate`. All 50 hashes matched.
- For the baseline, `$B/probe-guard-off` removes only the three-line preflight call (`torrent.rs:622-624`).
- Each variant used its own seeded target. Cargo's metadata hash for these root packages does not depend on their location, so the variants cannot share a target safely.

**Labels:**
- **[exec]**: run by me;
- **[static]**: traced in code, with `file:line`;
- **[dep]**: checked in the pinned `librqbit-9.0.1`, `librqbit-core-9.0.1` or `gpui-0.2.2` sources;
- **[reported]**: run by root or a worker. I inspected the logs or evidence but did not re-run it.

## 2. The changes

### 2.1 Saved-sidecar identity preflight. Correct [exec, static, dep]

**What it does.**
- `start_inner` takes the advisory lock (`torrent.rs:604-613`) and reads saved ownership.
- Before `Session::new_with_opts`, `validate_saved_metainfo` (`:143-166`) checks every saved infohash that has an available `<hash>.torrent`:
  - It reads the file through the extracted bounded reader (`:108-139`). The file must be a regular file, not a symlink, at most 16 MiB by stat, still regular once opened, and the read is capped at 16 MiB + 1.
  - On a blocking thread, it parses with `librqbit::torrent_from_bytes`, runs `info.data.validate()`, and requires the raw infohash to match the saved one.
- A missing sidecar is skipped. The post-restore completeness guard (`:637-651`) is unchanged.

**The preflight matches the engine exactly [dep]**
- **Path.**
  - librqbit names sidecars with `{info_hash:?}.torrent` (`json.rs:116-118`). `Id`'s `Debug` writes lowercase hex (`hash_id.rs:77-84`).
  - Flick lowercases the saved string, and both use the same canonical `state_dir` as the JSON persistence folder (`torrent.rs:1663-1665`).
  - librqbit's JSON deserializer accepts only 40-hex strings (`hash_id.rs:141-153`), and a base32 string makes engine startup fail. So uppercase input cannot split the two paths.
- **Parser and validation.** `librqbit::torrent_from_bytes` is the re-exported `librqbit_core` parser (`lib.rs:109`). The engine's `TorrentFileBytes` path uses that same parser and the same `validate()` (`session.rs:1132-1157`). So the preflight never rejects bytes the engine would have accepted, except in the cases in R6-L1 and N2.
- **No writes.** The preflight writes nothing, so a rejected startup leaves the JSON, the sidecars and the media untouched.

**Was the vulnerability real, and does the fix close it? [exec]**

On the baseline (preflight removed), I flipped one byte of the sidecar's filename, turning `orphan-a.mp4` into `orphan-b.mp4`. Then:
1. Startup failed only at the completeness guard.
2. `session.json` now held `ef2fdbce…` instead of `4660dcdc…`, and `confirm_saved_owner_absent(4660dcdc…)` succeeded.
3. Retry started successfully. The only live list was `[chosen.mp4 (selected), orphan-b.mp4 (unselected)]`. The retained `orphan-a.mp4` (zeros, unverified) appeared in no list.

With the frozen preflight:
- The same input fails with "Saved torrent metadata infohash does not match saved transfer 4660dcdc…".
- The JSON stays byte-identical, the owner stays saved, and the orphan's bytes are unchanged.

I did not exercise the downstream UI read gate; the worker's static trace covers it. **The fix closes the overwrite path.** With a sidecar present, librqbit takes its identity from the sidecar's bytes. Without one, it uses the saved hash (`session_persistence/mod.rs:39-50`). That leaves no restore path that changes identity, apart from the accepted window in which an outside process (one that ignores Flick's lock) could replace a sidecar after the check.

**Committed regressions [exec].** On the baseline, both new tests fail (exit 101), each at its error-text assertion (`torrent.rs:2240`, `:2287`), before reaching the byte and JSON assertions. The ownership consequence itself is shown by my probe above and by the worker's external test. No change needed.

**No false positives on genuine sidecars [exec].** The candidate's 9 loopback-only transfer tests pass. They restart sessions whose sidecars librqbit wrote itself, including one wrapped from metadata a magnet fetched from a local peer.

#### R6-L1 (Low): the sidecar limit is stricter than what Flick admits

**Executed [exec].** I built a valid single-file v1 metainfo of 16,859,214 bytes (842,956 pieces, 1 KiB pieces). I passed it as a `ResolvedTorrent`, the same value `resolve()` returns, to the real `TransferSession::download`. Results:
- Admission succeeded, and librqbit wrote a 16,859,214-byte sidecar.
- **Candidate restart:** failed in 1 ms with "Read saved torrent metadata for 69229889…: Saved transfer metadata exceeds 16 MiB".
- **Baseline restart:** succeeded in 101 ms.

**How such metainfo gets in [static, dep]**
- `resolve()` returns `list.torrent_bytes` without a size check (`torrent.rs:712-760`).
- `download_to` and `share` add bytes without one (`:805`, `:874`).
- librqbit's magnet path accepts info dictionaries up to 32 MiB (`peer_info_reader/mod.rs:87`) and stores them with any trackers (`session.rs:1662-1666`).
- Only file, URL and byte inputs apply the 16 MiB cap (`torrent.rs:400-467`).

**Impact**
- After a restart, every transfer shows "Downloads unavailable: … exceeds 16 MiB. Restore folder access and choose Retry downloads." Retry fails the same way.
- Files inside saved roots stay blocked; files outside them still play.
- The documented manual recovery does not apply here: the matching metainfo is itself over the limit. Removing the transfer needs the engine.
- Byte safety is intact. The trigger is rare: it needs more than about 839k piece hashes, or a very large file list.

**Recommended follow-up (a source change, so a new focused round).** Make the bounds consistent in one of two ways:
- enforce `MAX_METADATA_BYTES` when admitting resolved or shared metainfo, so Flick never writes a sidecar its own startup refuses; or
- give the sidecar preflight a limit no stricter than what Flick admits (at least librqbit's 32 MiB info limit plus the envelope).

If the PR merges first, add this to the report's remaining limits.

#### N2: empty and unreadable sidecars [exec, dep]

- **Before R6.** librqbit read an empty or unreadable sidecar as no bytes and fell back to a magnet that keeps the saved identity (`json.rs:282-305`, `mod.rs:39-50`).
- **Now.** The preflight rejects these cases immediately; an empty sidecar fails in 2 ms with "Parse saved torrent metadata: eof".
- I had expected the baseline magnet fallback to hang in a local session. It did not: it failed closed through the completeness guard in 1,032 ms, without rewriting the JSON.
- **So the change matters only with public discovery.** There, the magnet path could previously have recovered metadata from peers. The change is in the safe direction.
- The handoff's "empty … fail closed" is accurate. `review-responses-r6.md` could say "empty, unreadable or malformed".

**Accepted limits rechecked.** These remain truthful in the report and responses:
- An outside process that ignores Flick's lock could replace metadata between the check and the engine's read.
- There is no automatic reset.
- Retry does not repair damaged metadata. The UI's generic "Restore folder access and choose Retry downloads" hint also does not fit this cause; this is the existing corrupt-metadata limitation.
- Ownership lost before this fix cannot be reconstructed.

### 2.2 Multi-torrent import. Correct, with R6-L2 [static]

**Both entry points.** `import_paths` (`library_ui.rs:479`) and multi-file `pick_library` (`:899`) call `resolve_imported_torrents` (`:488-521`).

**Admission rules**
- It resolves only the first torrent, and only when not resolving and not quitting.
- The file counts as started only if `resolving` survives `send()`, which resets it on rejection (`:608-611`).
- A new admission error is taken from `self.error` and kept with the validation errors.

**Messages**
- 3 torrents, idle: "2 torrent files were not started".
- 3 torrents, busy or rejected: "3 … were not started".
- 1 torrent, rejected: "1 torrent file was not started".

**Sequence and side effects**
- Resolve no longer runs before video validation in the picker path, so the count is not overwritten by the later validation result.
- No queue was added.
- Resolve's existing behavior is unchanged: it pauses the player and replaces any pending chooser.

**R6-L2 (Low).** `ServiceEvent::Resolved(Err)` replaces `self.error` (`:722`), which removes the not-started count and any validation errors from the same batch. That happens almost at once in two cases:
- transfers are unavailable, so `reject_transfer` emits `Resolved(Err)` immediately (`services.rs:865-870`);
- the first `.torrent` is unreadable or invalid, so the resolve task fails within milliseconds (`services.rs:604-624`).

When the first torrent resolves successfully, the notice stays next to the chooser. The overwrite pattern is older than R6.

Suggestions:
- **Docs:** the response says files are "counted at admission; a later asynchronous failure of the started file replaces the notice".
- **Optional follow-up:** append, rather than replace, while an import notice is showing.

### 2.3 Delete without a file worker. Correct [static]

`request_delete_video` (`library_files_ui.rs:378-422`) now runs these steps in order:
1. the existing Working-dialog guard;
2. closing the card menu;
3. the library-membership check;
4. **the worker check, before reconciliation or generation lookup**. Without a worker, it sets "File monitoring could not start. Your file was kept; restart Flick and retry." and returns.

Other details:
- Both callers are library menus (`library_ui.rs:1986`, `:2098`). That page renders `self.error` as a dismissible notice, and only explicit actions clear it.
- When the worker exists, the actual generation is still required, the same request is prepared, focus moves, and the dialog enters Checking.
- Cloning the worker clones an `Arc` (`library_files.rs:157-160`).
- No invented generation is used, and no stale-ticket guard changed.

### 2.4 Palette Quit. Correct [dep, reported]

- `Action::Quit => cx.defer(crate::shutdown::request)` (`palette_ui.rs:373`). GPUI 0.2.2 has `App::defer(&mut self, impl FnOnce(&mut App) + 'static)` (`app.rs:1434-1438`). `Context` has only `defer_in` (`context.rs:305`), so `cx.defer` resolves to `App::defer` through `Deref`.
- `shutdown::request` (`shutdown.rs:49-67`) calls `prepare`, which updates the root `Flick`. Calling it directly inside `Flick`'s own update would reenter. Deferring it until after the effect cycle avoids that.
- The old `cx.quit()` (`app.rs:749-751`, `platform.quit()`) did not reenter. It bypassed the asynchronous coordinator in favor of the native-termination barrier.
- The new path coalesces with ⌘Q and the menu (`main.rs:59`).
- [reported] Root's Xvfb GUI check: the process exited 0 in 1.11 s with no panic. `palette-quit-r6.png` shows the "Quit Flick" row. That is a Linux shutdown check, not playback.

### 2.5 Test portability. Correct; nit N1 [exec, static]

**Canonical path (`torrent.rs:2016`).** `completed_file` joins the canonical output folder (`:1261`), so comparing with `source.canonicalize()` keeps exact identity on macOS (`/private/var`) and Windows (`\\?\`).

**Trash source preservation (`library_files.rs:2562`).** The `path.exists()` assertion moved out of the Unix block. It now runs on every platform, after both refusals, and fixes the unused-variable lint on Windows.

**HTTP fixture (`fetch/http.rs:1995-2001`).** On Windows, the 64 MiB file still proves the metadata-only open through `hasher.is_none()`. As its comment says, the 2 s large-prefix timing check runs only on other platforms.

**Share-cancellation fixture (`torrent.rs:2032-2038`)**
- The probe blocks the hasher at its first 64 KiB chunk until cancellation (`share.rs:260-266`). Every assertion is unchanged.
- [exec, mutation] I replaced `check_cancel` with a no-op. The test then **fails with both sizes**: "Cancelled hashing continued in the background".
- A timing variant shows why the 64 MiB case still fails: the non-cancelling debug hasher needs **3,183 ms** after cancellation, against the 2 s deadline.
- **N1:** on Windows, detection therefore rests on debug-build hashing speed (a 1.6× margin on this VM). On Linux and macOS, it is guaranteed by the fixture size.
- No change is needed. "No weakened assertions" is accurate. If Windows CI ever runs optimized tests, a test-only follow-up could count probe checkpoints after cancellation.

### 2.6 Audit reproducer and evidence JSON. Correct [exec]

**Reproducer.** `reproduce.py --prepare-only` exported 36 files.
- **Exports.** The set and every byte equal `git ls-tree`/`git show 71587ec`. The lockfile is identical. The copied harness differs only in its five `#[path]` attributes.
- **Toolchain.** In the exported directory, `rustup show active-toolchain` reports `1.99.0 … overridden by …/audited-source/rust-toolchain.toml`. So running in that directory selects the archived toolchain.
- **Build check.** `cargo metadata --offline --locked` succeeded. `cargo check --offline --locked … --bin flick-perf-audit` passed in 3 min 39 s with a private target. That is a type-check only: no release build, fixture, libmpv or benchmark run.
- **Isolation.** The target is now `folder/target`, and `repo_head` records the audited revision.
- **Safety.** Archive extraction refuses links and path escapes.

**Evidence JSON.** Comparing `6e2dc2b` with `8bec302`, exactly 9 values changed across the three JSON records: `binary`, `before_screenshot` and `after_screenshot`. Each change only replaces the absolute `/home/…` prefix with `<repo>/`. Everything else, including `session.folder`, is unchanged.

**N3.** `report.md` ("passes 244 … and 88") and the `summary.html` tiles name no platform. Those results are Linux runs. Root's R7 note reports Windows and macOS desktop failures on `8bec302`. Add "on Linux" until those jobs pass. The whole-app 10× and native-platform limitations in `report.md` and `summary.html` otherwise remain truthful.

## 3. Executed checks

All Cargo commands ran with `CARGO_NET_OFFLINE=true` and a private `CARGO_TARGET_DIR` under `$B`, from that variant's directory, using pinned Rust 1.99.0.

| # | Kind | Command | Result |
| --- | --- | --- | --- |
| E1 | Backend unit tests | `cd $B/candidate && cargo test --offline --locked --no-default-features --lib -- --test-threads=2` | **79 passed**, 1 ignored, 51.59 s. All **17** torrent tests pass, including both new regressions and the three changed fixtures. Log `logs/candidate-lib.log` |
| E2 | Transfer tests, loopback only | `… cargo test --offline --locked --no-default-features --test transfers -- --test-threads=1` | **9 passed**, 34.65 s, including the restart cases. `logs/candidate-transfers.log` |
| E3 | Backend probes, candidate | `cd $B/probe-candidate && … --lib torrent::tests::r6_probe -- --test-threads=1 --nocapture` | **3 passed**: mismatch refused with byte-identical JSON; metainfo over 16 MiB admitted, then restart refused; empty sidecar refused. `logs/probe-candidate.log` |
| E4 | Backend probes, baseline (guard removed) | the same command in `$B/probe-guard-off` | Overwrite reproduced (§2.1). Oversized metainfo restarts in 101 ms. Empty sidecar fails closed in 1,032 ms; the probe failed only because my expectation of a hang was wrong. `logs/probe-guard-off.log` |
| E5 | Committed regressions, baseline | `cd $B/probe-guard-off && … --lib -- --test-threads=1 torrent::tests::saved_metainfo_mismatch_preserves_ownership_across_retry_and_repair torrent::tests::invalid_saved_metainfo_preserves_ownership_before_engine_start` | **Both fail** as expected, exit 101. `logs/guard-off-committed-regressions.log` |
| E6 | Mutation test | `--lib torrent::tests::share_hash_abort_and_shutdown_cancel_worker_without_engine_admission -- --exact` in `$B/mutant-cancel-64{mib,gib}` and `…-64mib-timing` | With `check_cancel` a no-op, **both sizes fail**. The 64 MiB hasher continues for 3,183 ms. `logs/mutant-*.log` |
| E7 | Build: type-check only | `python3 -I docs/performance-audit-2026-10-25/reproduce.py --prepare-only`, then, in `…/audited-source`, `cargo metadata --offline --locked` and `cargo check --offline --locked --manifest-path /tmp/flick-performance-audit-jhkb6s3l/Cargo.toml --bin flick-perf-audit` | Passed, 3 min 39 s. All 36 exports byte-equal. `logs/audit-reproducer-check.log` |
| E8 | Evidence | `python3 -I $B/json_diff.py <repo> 6e2dc2b 8bec302` | 9 prefix-only changes |
| E9 | Formatting | `rustfmt --edition 2024 --check` on the six changed Rust files; `py_compile` on `reproduce.py` | Pass |
| E10 | Freeze check | `python3 -I $B/verify_hashes.py <repo> review-r6-source-hashes.json` | 50/50 matched at all four checks |

**Reported, not re-run:**
- Root's integrated Linux runs: **244** tests (79 + 156 + 9) and **88** without desktop, both strict Clippy runs, formatting, the debug build, and the GUI palette Quit check. I inspected `/tmp/flick-pr5-combined-*.log`.
- The UI worker's 11 mocked-boundary tests.
- The startup worker's 17 + 3 tests and its baseline run.
- Root's R7 note that CI on `8bec302` passed the core and transfer jobs on Windows and macOS, and failed desktop tests there.

None of my checks was a desktop build, a GUI session, playback, or native Windows or macOS execution.

## 4. Remaining actions

**Before merge (docs only; no new round).**
- Document R6-L1 as a known limit, unless it is fixed separately.
- Optionally:
  - R6-L2 wording in `review-responses-r6.md`;
  - N2: "empty, unreadable or malformed";
  - N3: "on Linux".

**Follow-up (source changes; each needs a focused review).**
- **R6-L1:** align the admission and sidecar limits.
- **R6-L2:** optionally preserve the not-started count when an asynchronous resolve error arrives.
- **N1:** optionally strengthen the Windows cancellation assertion (test only).

**This approval does not cover:**
- the round-7 test-only candidates;
- Windows or macOS desktop CI status;
- interactive playback.

## 5. Final freeze verification

`python3 -I $B/verify_hashes.py <repo> docs/performance-implementation-2026-10-25/review-r6-source-hashes.json` reported **50 entries, 0 bad** at 17:38:32 UTC, the end of this review. HEAD was `8bec302` with no tracked changes; only root's two R7 files were untracked. This file was then added. No frozen application, test or Cargo file changed during the review.
