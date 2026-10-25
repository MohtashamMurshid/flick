# Independent review, round 8: quiet `Load` after terminal startup failure, Windows parent fixture, docs (Claude, 2026-10-25)

Reviewer: Claude Opus 5.5. The user explicitly authorized this review sub-agent. Round 8 is new and covers only the delta after `629446b0d455297060ebea16872b9dfffec08829`.

Scope:
- `src/services.rs`: the terminal-failure `Command::Load` branch and the extended regression `invalid_saved_ownership_stays_unknown_and_startup_failure_replies`.
- `src/library_files.rs`: the 19 test-only lines in `replaced_parent_or_mount_keeps_missing_file_unavailable`.
- Docs: the `VALIDATION.md` table/revision labels and number/unit spacing. Because the root committed `e16c4e7` during this round, I also read its other docs: `report.md:75`, `summary.html`, `ci-629446b.json` and `final-checks-r8.json`.
- This is not a whole-app audit. It reopens no R6/R7 finding or accepted limit: R6-L1, R6-L2, Windows creation-time collisions, macOS raw alias/FSEvents, uncertain ownership, corrupt settings and platform interactive checks.

Ground rules:
- I edited no application, test, Cargo or docs file except this one. I committed nothing, used no subagents, and did not poll or watch the PR.
- No cargo command used the repository manifest or `target/`. My copies, targets and logs are under `/tmp/claude-r8-review-Ex15Dfag/`, called `$R8` below.
  - I copied cached targets from my R7 directory and from the worker cache. I only read the authors' private directories.
  - Each private package has a unique name: `claude-r8-load-review-private`, `claude-r8-parent-fixture-review` and `claude-r8-parent-fixture-mutant`.
  - Every cargo command ran `--offline --locked`, on the pinned **1.99.0**.
- The services test uses `TransferConfig::local`: a `127.0.0.1:0` listener and `public_discovery: false` (`src/torrent.rs:377-389`). That flag disables DHT, trackers and UPnP (`src/torrent.rs:1646-1671`). This corrupt-state test fails before any engine or listener starts. All fixtures are generated locally; no playback ran.
- **I executed nothing on Windows or macOS.** This machine has no mingw linker or Wine. The Windows evidence is a cross-target type check and lint, a negative control, and static analysis.
- The root committed `e16c4e7` at 18:17:18Z, about a minute after my start check. The source bytes did not change (§1).
- I read the root's untracked draft `review-responses-r8.md` only after finishing my analysis.

## Verdict

**Approved as frozen.** There are no Blocking, High or Medium findings.
- `src/services.rs` `b45f954782b276409576489b0374cf1a1237d8bc958cf64607e12180ded0883e`;
- `src/library_files.rs` `8a5a5a508d7e2fdfcc4b7d675dbb4a55cb8e10b05e502cf11103c43cf88e595b`, equal to the worker's private candidate.

What I confirmed:
- **Services production change.** It is exactly one 6-line hunk. Only `Command::Load` received while `startup_error` is set changes: it no longer emits `Event::Error`. Retry, Checking, explicit admission, deletion/ownership replies, shutdown draining and non-terminal `Load` are byte-for-byte unchanged (§2.1).
- **Regression test.** It is red on the old production code and green on the new. It is deterministic, with no ordering sleep, and it discriminates over-broad suppression (§2.2).
- **Parent fixture.** It is test-only: the production prefix is byte-identical. It is correct by static analysis and its Windows code is really type-checked. The new identity assertion is deterministic on Unix (§3).
- **Docs.** Every reformatted measurement keeps its numeric value; the revision labels are accurate (§4).

| ID | Severity | Summary | Action |
| --- | --- | --- | --- |
| **R8-L1** | Low. Precision of the stated rationale. | The fix is correct, but the "actionable Retry message" it is said to preserve is not what the initial failure displays. `services.rs:725-726` sends `Unavailable`, then the raw `Event::Error`. The UI handles them in order, so the raw text immediately replaces "Restore folder access and choose Retry downloads." The recorded screenshot shows the raw text. What R8 actually preserves is the *current* notice, for example the actionable pending-open guidance. The Retry button was never affected. | Docs/reply wording only; no freeze needed (§2.3). |
| **R8-I1** | Info. UX trade-off, acceptable. | After the notice is dismissed or cleared by playback, a Downloads visit no longer re-surfaces any reason. The page shows **Retry downloads** above "No transfers yet". Before R8, the raw reason reappeared on each visit. This has no safety or ownership effect. | None for R8. Optional later UI work (§2.4). |
| N1–N4 | Nits | Docs only. | §4. |

**Merge gates (unchanged root plan).** All checks must pass on the final head, including **both** Windows desktop jobs (push and PR). The new Windows directory open and `SetFileTime` have not executed anywhere yet; native CI will be their first run.

## 1. Freeze and method

- Manifest `review-r8-source-hashes.json` has 51 entries and SHA-256 `2faf6fa716e9bef93d4763a0f8e17cf6741349651649293a2259b788d99b0c5f`. That value was the same at start and end.
- **Start, 18:16:08Z.** 51/51 working-tree inputs matched, with HEAD at `629446b` plus uncommitted edits.
- **End, 18:30:10Z.** 51/51 matched in both the working tree and `git show e16c4e7:<path>`.
- So the reviewed source is the frozen source, and it is exactly what `e16c4e7` commits.
- Production-prefix check, scripted against `git show`:
  - `library_files.rs` is byte-identical before `#[cfg(test)] mod tests`, which starts at line 1652.
  - In `services.rs`, the only production difference from `629446b` is the 6-line hunk at 792-799.

## 2. `services.rs`: quiet `Load` after terminal startup failure

### 2.1 Behavior delta [static]

- Commands reach the changed branch through pass-through helpers: `fetches.handle` and `subtitles` both end in `command => return Some(command)`.
- Branches that precede it are unaffected:
  - `CancelSearch`, `Search`, `CancelResolve` and `ConfigureTransfers` (`services.rs:759-784`);
  - **`RetryTransfers` (785-791)**: it clears `startup_error` and calls `start_session(…, true, …)`, which emits `Checking` (478).
- **Changed (792-799).** With `startup_error` set:
  - `Load` now hits `continue` with no event;
  - every other command still goes to `reject_transfer`: `Resolved(Err)`, `Shared(Err)`, `ReleaseForDeletion` → `reply.finish(Err)`, or `Event::Error` (869-883).
- **Unchanged:**
  - **Admission.** Auto-start needs `startup_error.is_none()` (675), so terminal `Load` never queued anything before or after the change.
  - **Ownership.** No `TransferRoots` or `Progress` is emitted. The UI keeps `transfers_known = false`, and deletion replies stay `Err`, which fails closed.
  - **Shutdown draining** (817-830).
  - **Non-terminal `Load`** (804-809).
  - **A `Load` pending at the moment of failure** (727-729) is still rejected with an `Event::Error`. That error carries the same text as line 726 just sent, so nothing visible changes.
- **Only producer.** `ui.rs:1386` (`show_page(Page::Downloads)`) is the only production sender of `Load`. It is fire-and-forget and sets no waiting state. The `resolving` flag is cleared by `Resolved`, `ResolveCancelled` or a failed send, not only by `Error`. Dropping the reply therefore leaves nothing stuck.

### 2.2 Regression test [static; exec]

The first drain consumes `Checking`, `Unavailable` and the initial raw `Event::Error`. All of them are sent before the first release reply in every interleaving: biased `select!`, startup arm first. The second phase then sends 3× `Load`, `Resolve(Bytes([]))` and a `ReleaseForDeletion` fence.

Why the fence is sufficient:
- A single loop handles commands strictly FIFO.
- Every event is a synchronous `std::mpsc` send made before the reply's `oneshot` send.
- So `drain()` sees all preceding events.
- The 3 s timeout only guards against a hang; it does not order anything.

The second drain rejects any `Error`, `TransferAvailability`, `TransferRoots` or `Progress`. It also requires `Resolved(Err)` containing "Parse saved transfer metadata", which is the same `startup_error` text. That message is platform-independent; the first half already asserts it on all CI platforms.

Executed, in a private harness (§5):
- **Baseline red.** The baseline is HEAD `services.rs` with only the production hunk reverted; its diff against `629446b` is exactly the test hunk. Result: `FAILED`, "Error: Load repeated the terminal startup state", exit 101.
- **Candidate green** on the exact frozen bytes: 1 passed.
- **Determinism.** Re-running the compiled binaries: candidate 25/25 pass; baseline 10/10 fail with the same message.
- **Discrimination.** A mutant that also suppresses `Resolve(_)` fails with "Explicit metadata request was not rejected".
- **Lint.** Candidate strict Clippy (`--tests -D warnings`) passes.

### 2.3 R8-L1: what is actually preserved [static; evidence]

- On terminal startup failure, `services.rs:725-726` (unchanged since `dde771c`) sends `error.event()` (`Unavailable`), then `Event::Error(error.message)`.
- `poll_services` processes events in order:
  - the `Unavailable` arm sets "Downloads unavailable: {message}. Restore folder access and choose Retry downloads." (`library_ui.rs:751-760`);
  - the `Error` arm immediately replaces that text with the raw message (`library_ui.rs:848-851`).
- The recorded GUI evidence `download-startup-unavailable.png` (`dde771c`) shows exactly the raw text, with no "Restore folder access…" suffix.
- Before R8, a Downloads visit re-sent that same raw text, so nothing changed visually unless the notice had been dismissed or replaced.
- What R8 really fixes: a visit no longer overwrites the **current** notice, notably the pending-open guidance "…Restore access to the download folder, then choose Retry downloads." (`ui.rs:810-815`).
- The **Retry downloads** button depends only on `transfer_availability` (`library_ui.rs:1519-1546`). `Event::Error` never touched it, before or after R8.

Requested actions, wording only (no new freeze):
- `report.md:75` says "preserving the existing actionable Retry message". Suggested replacement: "Downloads visits after a terminal transfer-startup failure no longer re-emit the raw startup error over the current notice; the initial failure notice and the Retry downloads button are unchanged."
- Suggested CodeRabbit reply: "Fixed in `e16c4e7`. After a terminal startup failure, readiness `Load` is ignored, so Downloads visits no longer replace the current notice with the raw startup error. Explicit commands are still rejected, and Retry/Checking/ownership are unchanged. The initial failure still emits `Unavailable` followed by the raw error (pre-existing)."
- The code comments at `services.rs:793-794` and `1887-1888` have the same imprecision. Changing them would need a new freeze; reword only if a later code change touches this branch.
- **Optional product follow-up**, separate freeze plus tests, not needed for merge: make the formatted guidance the visible text by dropping or reordering the redundant `Event::Error` at line 726. Note R6's point that "Restore folder access" misfits corrupt metadata, so the raw cause may be the better text there.

### 2.4 R8-I1: reason after dismissal [static]

- `self.error` is cleared by Dismiss (`ui.rs:1489-1492`) and by entering the player (`ui.rs:875`).
- After that, a Downloads visit now shows only **Retry downloads** above "No transfers yet" (`library_ui.rs:3105-3111`). That empty state is pre-existing, and before R8 the raw reason above it softened it.
- **Safety unaffected.** Ownership stays unknown, deletes still fail closed, and Retry still works and re-reports any failure.
- **Optional later UI change:** show the `Unavailable` reason from `transfer_availability` next to Retry, and avoid "No transfers yet" while `!transfers_known`. Not for R8.

## 3. `library_files.rs`: Windows parent replacement fixture [static; exec cross-lint]

**Diagnosis matches the native evidence:**
- In `/tmp/flick-pr5-windows-629446b-clean.log:787-797`, the PR-run desktop job panicked at `src\library_files.rs:1882:9`. At `629446b` that is the **first** replaced-parent `Unavailable` assertion.
- Result: 151 passed, 1 failed, 12 ignored. `ci-629446b.json` shows the other Windows desktop job succeeded.
- `inspect` (`library_files.rs:1046-1056`) treats the parent as stable when `Identity` (Windows: `created`) is equal. Equal identity plus a present mount leads to the Missing-settle path (1091-1103), which returns `None` rather than `Unavailable`. That is consistent with a creation-time collision between the renamed and recreated directories.

**Fixture correctness:**
- `entry.parent` is set at the first `check`, because the file exists (1067). Both Windows `unwrap`s are therefore sound on NTFS.
- The block sets `created = recorded_parent.created + 60 s` *after* `create_dir`. So neither tunneling nor timestamp granularity matters, and the value necessarily differs from the recorded identity, which is exactly what `inspect` compares.
- `access_mode(0x100)` overrides std's read/write access, giving `OPEN_EXISTING`. `FILE_FLAG_BACKUP_SEMANTICS` is required for directory handles. `SetFileTime` needs `FILE_WRITE_ATTRIBUTES`, and std's default share mode is read/write/delete.
- In the pinned `windows-0.61.3` source, `FILE_FLAG_BACKUP_SEMANTICS` = 33554432 = `0x02000000` (`Storage/FileSystem/mod.rs:4176`) and `FILE_WRITE_ATTRIBUTES` = 256 = `0x100` (`:4657`).
- The handle is a temporary, dropped at the end of the statement, so it cannot block `remove_dir_all(old)` or the tempdir cleanup.
- If `SetFileTime` were refused on CI, the test would panic loudly; it cannot pass silently.

**The new `assert_ne!` runs on every platform:**
- It compares the same two values that `inspect` compares, so it precisely states the precondition of the first guard.
- On Unix it is deterministic: the renamed old directory still exists on the same filesystem, so the new directory cannot reuse its `(dev, inode)`.
- The second guard (mount mismatch, with parent identity deliberately reset to equal) is unchanged.

**Nothing weakened.** No assertion was removed or weakened. There is no skip, sleep, mocked identity, production code, dependency or flag. The pattern mirrors the R7-approved media-file fixture at `library_files.rs:1987-2005`, which uses `write(true)` because it opens a file.

**Executed (§5):**
- **Focused Linux test:** 1 passed.
- **Windows-GNU:** tests type-check and strict Clippy (`-A dead_code`) pass. Linux strict Clippy passes. rustfmt passes.
- **Negative control:** replacing `0x100` with `"not-a-u32"` gives Windows-GNU `E0308` at line 1889 (exit 101), while the Linux check passes. This proves the `cfg(windows)` block is actually compiled by the cross check.

## 4. Docs [static; exec scripted comparison]

**Measurement spacing.** I compared the five reformatted lines (Search/layout, Fresh GUI measurements, deletion/startup, transfer startup, Large display) old vs new by script.
- All numeric values are equal.
- After removing whitespace, the only differences are "1k" → "1,000 entries" and "supervisor143" → "supervisor status 143". Both are value-preserving clarifications.

**Table.**
- "Historical … `8bec302`" for Windows (five failures) matches the earlier record.
- macOS "nine test failures" matches `/tmp/flick-pr5-macos-8bec302-clean.log`: 141 passed, 9 failed, 12 ignored.
- Earlier macOS Sintel/CoreAudio observations are now explicitly pre-PR, and final-PR interactive playback is marked unverified. The CI-vs-playback distinction is kept.

**Other records.**
- The committed `ci-629446b.json` is JSON-equal to `/tmp/flick-pr5-ci-629446b.json`.
- `final-checks-r8.json` matches `/tmp/flick-pr5-r8-check-0..3.log`: 79 + 156 + 9 = 244 passed, 0 failed, 13 ignored, and both R8 tests `ok`. Clippy is clean, the fmt log is empty, and the build is up to date.
- The `summary.html` status sentence is accurate for now.

Nits, all docs-only with no freeze needed:
- **N1.** VALIDATION says the `629446b` PR job failed "at the new-parent identity guard". That test had no identity guard at `629446b`; the failure was its first replaced-parent `Unavailable` assertion (`src\library_files.rs:1882:9`). Reword so it does not imply that R8's new `assert_ne!` failed.
- **N2.** VALIDATION cites `/tmp/flick-pr5-ci-629446b.json`. Cite the committed, durable `performance-implementation-2026-10-25/ci-629446b.json` instead; it has the same content and the job URLs. The raw failing log remains `/tmp`-only.
- **N3.** After final-head CI, add its result to the Windows/macOS rows, or as a "final head" line. Otherwise the first thing readers see is the historical `8bec302` failures. Also update `summary.html`'s "under review" sentence.
- **N4.** "Measurement spacing changes preserve every numeric value" is true. Two edits are wording clarifications rather than pure spacing (above); optionally say so.

## 5. Checks executed in round 8 (for `docs/VALIDATION.md`)

`$R8` = `/tmp/claude-r8-review-Ex15Dfag`. Every cargo command used `cargo +1.99.0 … --offline --locked` with a private `CARGO_TARGET_DIR`. Only each private copy's root-package name in `Cargo.lock` was renamed; dependency entries were unchanged.

**Executed by me:**
1. **Freeze.** Scripted SHA-256 of all 51 manifest inputs at 18:16:08Z (working tree) and 18:30:10Z (working tree and `git show e16c4e7:`): 51/51 each time. Manifest SHA-256 unchanged.
2. **Services red/green.** Harness `$R8/services`; target copied from `/tmp/flick-r7-review-ov6Tv0WE/target-services-1990`. The `[lib]` is the repository's frozen `src/lib.rs`, read-only. Each command used `CARGO_TARGET_DIR=$R8/services/target CARGO_NET_OFFLINE=true FLICK_LOCAL_TRANSFERS=1`:
   - `cargo +1.99.0 test --offline --locked --manifest-path $R8/services/Cargo.toml --bin r8-load-baseline -- --exact services::tests::invalid_saved_ownership_stays_unknown_and_startup_failure_replies --test-threads=1` → **FAILED** as expected, "Load repeated the terminal startup state", exit 101, 12.0 s including build.
   - Same with `--bin r8-load-candidate` → **1 passed**, 18 filtered.
   - Repeats of the compiled binaries: candidate **25/25** pass; baseline **10/10** expected failures.
   - Mutant `--bin r8-load-mutant-resolve` (`Load | Resolve(_)` suppressed) → fails "Explicit metadata request was not rejected".
   - `cargo +1.99.0 clippy --offline --locked --manifest-path $R8/services/Cargo.toml --bin r8-load-candidate --tests -- -D warnings` → pass.
   - Log: `$R8/services-checks.log`, `$R8/mutant-resolve.log`.
3. **Parent fixture.** Harness `$R8/files`, built from the worker's manifest. The sources are fresh repository copies, hash-verified against the manifest. Targets were copied from `/tmp/flick-pr5-platform-files-werdps52/{target,target-windows}`.
   - `cargo +1.99.0 test --offline --locked --manifest-path $R8/files/Cargo.toml --lib library_files::tests::replaced_parent_or_mount_keeps_missing_file_unavailable -- --exact --test-threads=1` → **1 passed**, 41 filtered.
   - `cargo +1.99.0 check --offline --locked --manifest-path $R8/files/Cargo.toml --target x86_64-pc-windows-gnu --tests` → pass. The one harness dead-code warning (`retain`) is pre-existing and unrelated.
   - `cargo +1.99.0 clippy --offline --locked --manifest-path $R8/files/Cargo.toml --target x86_64-pc-windows-gnu --lib --tests -- -D warnings -A dead_code` → pass.
   - The same Clippy on Linux → pass.
   - Negative control `$R8/files-mutant` (`access_mode("not-a-u32")`): Windows-GNU check `E0308` at line 1889, exit 101; Linux check exit 0.
   - Log: `$R8/files-checks.log`, `$R8/mutant-wingnu.log`, `$R8/mutant-linux.log`.
4. **Formatting and whitespace.** `rustfmt +1.99.0 --edition 2024 --check` on the private copies of both files → pass. `git diff --check 629446b e16c4e7` → clean.
5. **Docs scripts.** Numeric/whitespace comparison of the five measurement lines; JSON equality of the two `ci-629446b.json` copies; count of macOS `8bec302` failures; production-prefix comparisons (§1).

**Reported. I inspected these logs but did not re-run them:**
- the root's red log `/tmp/flick-pr5-load-baseline-red.log`: 1 failed, 167 filtered, the same message;
- the root's green log `/tmp/flick-pr5-load-services-green.log`: **19 passed**, 149 filtered, 20.63 s;
- the root's integrated checks `/tmp/flick-pr5-r8-check-{0..3}.log` (§4);
- the Windows CI log and metadata for `629446b`. Which run was push and which was PR is as the root reports; I had no network.
- The worker's `windows-parent.md` checks; I independently reproduced its focused-test, check and lint results above.

**Static only:**
- the services control flow and the UI consequences (§2.1, §2.3, §2.4);
- the Windows API semantics and the Unix determinism argument (§3).

**Not performed:** Windows or macOS execution, an MSVC build, a full suite, a desktop build, GUI or playback, and any network, CI polling or PR operation. All fixtures were generated locally; no public trackers, DHT or discovery were used.
