# Independent review, round 7: two test-only platform-fixture candidates (Claude, 2026-10-25)

Reviewer: Claude Opus 5.5. The user explicitly authorized this review sub-agent. Round 7 is new and independent of round 6, which is still reviewing the combined production changes at `8bec302`.

Scope:
- Only the two private, test-only candidates for the current Windows and macOS CI failures.
- This is not a whole-app audit. It does not repeat R6's scope and implies nothing about R6's verdict.
- I used no subagents. `claude-review-r6.md` appeared in the working tree during this round; I did not read it.

Inputs:
- `.flick-test/performance-implementation/windows-files.patch` and `windows-files.md`, with the private candidate `/tmp/flick-pr5-platform-files-werdps52/candidate/library_files.rs`.
- `.flick-test/performance-implementation/windows-services.patch` and `windows-services.md`, with the private candidate `/tmp/flick-pr5-windows-services-8bec302-d8a5i3hn/candidate/services.rs`.
- Native CI evidence: `/tmp/flick-pr5-windows-8bec302-clean.log` and `/tmp/flick-pr5-macos-8bec302-clean.log`.
- The manifests `review-r6-source-hashes.json` and `review-r7-candidate-hashes.json`.
- [report.md](report.md) at HEAD. For limit wording only, I also read the root's uncommitted `report.md` addition and `review-responses-r7.md`.
- Earlier reviews and responses served as context only.

Ground rules:
- I edited no application, test, Cargo or docs file. I applied nothing to the shared tree and committed nothing. This file is the only file I added.
- No cargo command used the repository manifest or `target/`.
  - My copies, fixtures, targets and logs are under `/tmp/flick-r7-review-ov6Tv0WE/`, called `$R7` below.
  - I only read the authors' private directories.
- Every cargo command ran `--offline --locked`.
- The services tests use `TransferConfig::local` (`src/torrent.rs:378-389`). It sets a loopback listener and `public_discovery: false`, which disables DHT, trackers, UPnP and announce (`src/torrent.rs:1646-1672`).
- Those tests used generated files and explicit loopback peers only. No playback ran, and nothing mixed playback with transfers.
- **I executed nothing on Windows or macOS.** The Windows evidence below is a Linux cross-target type check and lint, plus static analysis.

## Verdict

**Both candidates are approved as written:**
- `library_files.rs` candidate `2ac854f890593165b7b758522d6a637c2383ab17bd6b2064abbcb35976d8264d`;
- `services.rs` candidate `e82c85ff7b2675f0eaad2ceecf5fc52ee32b419bd95ff917f5ff5d10b9b616a4`.

**There are no Blocking, High or Medium findings.**
- **Test-only.** In both files the first differing byte lies inside `#[cfg(test)] mod tests`, which runs to EOF. The production prefixes are byte-identical to `8bec302`.
- **Assertions preserved.** No assertion is removed, weakened, skipped, cfg-gated, or given a longer timeout. One candidate adds a precondition assertion. The services fixture adds alias coverage on Linux.
- **Diagnoses confirmed.** Each diagnosis matches the native panic sites exactly, and the production code and the pinned `notify 8.2.0` source.
- **Re-run by me.** I re-ran the private checks on my own copies, including on the repository-pinned Rust 1.99.0 (§6).

| Candidate change | Status | Evidence from this round |
| --- | --- | --- |
| Windows replacement fixture (`media_replacement_…`) | Correct. The creation-time identity limit remains a documented limit, not a fix. | Windows-GNU type check and strict Clippy pass; Linux tests pass [exec]. Windows runtime: not run. |
| Windows timestamp handle (`timestamp_or_length_…`) | Correct | Same as above. |
| macOS canonical fixture root (six watcher tests) | Correct. Diagnosis confirmed in pinned notify. | 37/37 Linux tests. The 22 `fixture()` tests also pass, 22/22, with an aliased `TMPDIR` [exec]. macOS: not run. |
| Services: three tests plus the alias fixture | Correct | Candidate 19/19. Negative control: exactly the 3 tests fail, with the native messages. Baseline 3/3 [exec, on 1.99.0 and on 1.99.1]. |

New findings. None requires a change to either candidate:

| ID | Severity | Summary |
| --- | --- | --- |
| **R7-L1** | Low. Disclosure; native CI is the real test. | On Windows, the canonical fixture root is a verbatim `\\?\C:\…` path. All 21 Windows-run `fixture()` tests will use it; 19 currently pass on the raw spelling, 10 of them watcher/Missing tests. This matches production, because `canonical_video` emits verbatim paths. `windows-files.md` does not mention it. It also newly exercises `GetDriveTypeW("\\?\C:\")` in `local_mount`. |
| **R7-L2** | Low. Precision of the documented limit. | The raw-alias watcher limit applies only to macOS (FSEvents). It has a current producer that is neither "legacy" nor "arbitrary": HTTPS direct and archive.org downloads keep the configured download folder's spelling. The refocus sweep bounds it. `windows-files.md` says download completions are canonicalized; that holds only for yt-dlp and torrents. |
| **R7-L3** | Low. Validation record. | Several assertions will run on native platforms for the first time. The Windows replacement test passes because its fixture sets a distinct creation time. The record should not imply that Windows replacement detection was demonstrated. |
| N1–N3 | Nits | §4. |

**Recommendation.** Apply exactly these bytes after R6 returns, as planned. The root's gates remain:
- the integrated Linux test and lint run;
- the hash comparison against `review-r7-candidate-hashes.json`;
- the native Windows and macOS CI reruns.

Make the R7-L1 to R7-L3 documentation updates and correct the N3 label. These are documentation changes and need no new review round.

**If CI fails in a previously passing `fixture()` test on Windows,** treat it as a production verbatim-path defect, because production imports are verbatim. Do not revert the fixture to raw paths.

**Re-verification is needed if either candidate changes.** It is also needed if an R6 change alters the production prefix of `library_files.rs` or `services.rs`.

## 1. Scope, freeze and method

**Freeze.**
- HEAD is `8bec302566bafebf85a8c15b6f993719e6b2256c`.
- All 50 entries in `review-r6-source-hashes.json` matched the working tree at the start (before 17:39 UTC), again at 17:46:48, and at the end (§6).
- During the round, others changed `docs/VALIDATION.md` (staged), `report.md`, `summary.html` and `review-responses-r6.md`. They also added `claude-review-r6.md` and `review-responses-r7.md`. None of these is a frozen source.

**Candidate manifest.**
- `review-r7-candidate-hashes.json` has the same 50 keys as R6 and differs in exactly two entries:
  - `src/library_files.rs`: `1d9a860c…` → `2ac854f8…`;
  - `src/services.rs`: `804e8ff8…` → `e82c85ff…`.
- With those two entries mapped to the private candidates, all 50 entries match.

**Patches.**
- Their SHA-256s match the handoffs: `9faceece…` for files and `4903ea55…` for services.
- `git apply --check --whitespace=error` passes for both against the shared tree. The check writes nothing.
- `--numstat` reports 25/2 and 27/6.
- Applied in a private directory to the blobs from `git show 8bec302:`, each patch reproduces its candidate byte for byte.
- The diff embedded in `windows-services.md` is identical to the patch file.

**Test-only boundary.**
- The first differing byte is at line 1658 of `library_files.rs` and line 1251 of `services.rs`. Both lie inside `#[cfg(test)] mod tests`, which starts at lines 1652 and 1247 and closes at EOF.
- The production prefixes are byte-identical:
  - `library_files.rs`: 57,947 bytes, SHA-256 `63b487acb35ad50c689d7c42c390f88591adfd38711664bca9ea2c5e207efaac`;
  - `services.rs`: 46,817 bytes, SHA-256 `c4f6563bc52572867b8af30c4ef59a17bd9b28f40c316dda87497c26c7b3eb59`.
- The `#[cfg(test)] async fn run` at `services.rs:531` lies in the prefix and is unchanged.

**Native evidence.**
- Both CI logs hash-match the handoffs.
- Every panic site maps to an assertion the candidates change.
- Windows:
  - `library_files.rs:1966`, the first `replaced: true`;
  - `library_files.rs:1994`, `set_times(...).unwrap()`, failing with OS error 5;
  - `services.rs:1302`, `:2034` and `:2250`.
- macOS:
  - `library_files.rs:1691`, the `wait_for` deadline, in five tests;
  - `library_files.rs:1762`, the starvation assertion;
  - the same three `services.rs` lines.
- `git diff dde771c 8bec302` touches neither the failing tests nor `services.rs`. All five Windows failures therefore predate `8bec302`.

**Evidence labels:**
- **[exec]**: run by me in this round;
- **[static]**: traced in the code, with file:line references;
- **[dep]**: checked in the pinned dependency source in the local cargo registry;
- **[reported]**: run by the author. I inspected the logs but did not re-run it.

## 2. Candidate 1: `windows-files.patch`

### 2.1 Windows replacement identity [static; exec cross-lint]

**Diagnosis.** It holds.
- On non-Unix targets, `Identity` is only `metadata.created()` (`src/library_files.rs:379-418`).
- `inspect` sets `replaced` only when `old.identity != stamp.identity` (`:1115-1124`).
- Two causes can keep the creation time the same:
  - a rename-over replacement can inherit the old creation time through NTFS tunneling;
  - two files created within one clock tick can share it.
- CI shows `media: true, replaced: false`. The log does not say which cause applied; the fix covers both.

**Fix.**
- After the rename, the test opens the replacement for writing. Through `std::os::windows::fs::FileTimesExt` it sets creation time to the old baseline plus 60 s with `File::set_times`.
- A `FileTimes` leaves unset fields unchanged (std documents this), so only the creation time changes.
- Because it is set after the rename, the value is final whatever tunneling did.
- `identity.created.unwrap()` fails loudly on a filesystem without creation times.

**Coverage.** Every existing assertion is unchanged:
- both `replaced: true` checks, including the repeat against the unacknowledged baseline;
- the Unix permission-only `Available` check.

The new `assert_ne!` precondition runs on every platform. A fixture that failed to produce a distinct identity now fails there, with a clear message. On Unix the precondition holds deterministically: the replacement is a separate inode, created while the original still exists.

**What the test now proves on Windows.**
- It proves the production rule: a different identity gives `replaced: true`.
- It does not prove that a real tunneled rename-over is detected. That remains the documented creation-time limit (`report.md:65` at HEAD, plus the root's new sentence).

**The production effect of the limit:**
- A tunneled replacement yields `media: true, replaced: false`.
- Posters and dimensions are cleared, but playback is not released and the generation is kept (`src/library_files_ui.rs:291-335`).
- Delete stays protected. The trash ticket compares the full stamp, including length and modified time (`src/library_files.rs:1204-1208`).

### 2.2 Windows timestamp handle [static; exec cross-lint]

- **Cause.** `File::open` opens read-only. `SetFileTime` needs `FILE_WRITE_ATTRIBUTES`, hence OS error 5.
- **Fix.** `OpenOptions::new().write(true)` grants that access without create or truncate.
- **Unix.** `futimens` already worked there. Opening with `O_WRONLY` changes no timestamp. This test calls `inspect` directly rather than the watcher.
- **Assertions.** Both `replaced: false` checks are unchanged.
  - On Windows they never ran before, because the panic came earlier.
  - They should hold: identity is creation time. Neither a change to the modified time nor std's in-place truncating `fs::write` of the same file touches creation time.
  - They have not run natively (R7-L3).

### 2.3 macOS canonical fixture root [static; dep; exec]

**Diagnosis, confirmed in source:**
- **notify.** In `notify-8.2.0`, `FsEventWatcher::append_path` canonicalizes the registered path for its own filter (`src/fsevent.rs:388-392`). It forwards the native FSEvents callback path unchanged (`:526-574`). On macOS that path is the resolved `/private/var/…` form.
- **Flick.** It keys its index by the tracked spelling (`EventIndex::add`, `src/library_files.rs:193-203`). It registers watches under that spelling (`:672-693`) and dispatches by exact key (`collect_event`, `:642-646`). A `/var/…` record therefore never matches its own event.
- **The six failing tests.** Each removes the file after the watch is installed. Each then waits 5–8 s for an observation, below the 30 s failed-watch fallback. In that window, only native event dispatch can deliver it.
- **`failed_native_watcher_is_reinstalled_on_focus`.** It calls `reconcile_focus()` before the removal, so its focus sweep runs while the file still exists.

**Fix.**
- `tempdir_in(std::env::temp_dir().canonicalize())` gives the media, sibling and poster paths the resolved spelling from the start.
- That is the spelling production stores for picker, drop, folder and Open With imports (`src/library.rs:39-62`, `src/library_ui.rs:410,464`, `src/ui.rs:766`).
- The patch adds no timeout, rescan, synthetic Missing or skip, and changes no assertion.
- Mount evidence is unaffected: macOS `local_mount` calls `statfs` on the parent and returns the same `f_fsid` for either spelling (`:1598-1609`).

**Linux checks.** `/tmp` is already canonical, so a plain Linux run says nothing about macOS. To exercise raw ≠ canonical at the fixture, I also ran all 22 `fixture()` tests with `TMPDIR` set to a symlink: 22/22 pass [exec]. Linux inotify builds event paths from the registered root (`notify-8.2.0/src/inotify.rs:217-218`), so this cannot reproduce the macOS failure. Only macOS CI can confirm the fix.

**Windows side effect.** See R7-L1.

### 2.4 Statements of limits [static]

See R7-L2 and N3. Otherwise `windows-files.md` is accurate and does not overstate its evidence:
- its Windows check is a GNU type check, not MSVC;
- it claims no Windows, Wine or macOS runtime.

## 3. Candidate 2: `windows-services.patch` [static; exec]

**Production never compares raw fixture paths here.**
- `DownloadAcceptance.selected` comes from `session.selected_file_paths(&id)`, which joins onto the engine's output folder (`src/services.rs:903-911`).
- The completion tombstones, `ProgressPublisher.completed`, are keyed by snapshot paths (`:1021-1048`).
- The engine canonicalizes share sources (`src/torrent/share.rs:100`) and download roots (`src/torrent.rs:600-601`, `:798`) with std `canonicalize`.

The failures are therefore on the test side, as diagnosed:
- on Windows, `canonicalize()` always yields verbatim `\\?\` paths, so a raw path never matches;
- on macOS, `/var` resolves to `/private/var`.

**`readiness_and_acceptance_…`.**
- The simulated acceptance now uses `source.canonicalize()?`, which is the engine's identity for a single-file share.
- Every count assertion is unchanged:
  - readiness: one snapshot, no completion;
  - acceptance: one snapshot and exactly one completion.
- The change also adds an implicit check that the engine identity is the canonical path. N1 gives a closer mirror.

**`deleted_output_…`.**
- The canonical output is captured after `seed.stop()` and before `remove_dir_all`, so nothing canonicalizes a missing path.
- The changed assertion follows from the unchanged `roots == [parent.canonicalize()?]` above it, as the original did in raw space.
- The substantive checks are all unchanged:
  - root equality;
  - independent local media stays outside the root;
  - the boundary arrives before Progress;
  - zero verified bytes;
  - no completion;
  - `roots_complete` on the Unavailable branch.

**`accepted_download_…`.**
- The expected prefix comes from the test's own config, `original_folder.canonicalize()?.join(&infohash)`, not from the event, so the check is not tautological.
- It still rejects the `ignored-new-folder` path requested in the second round.
- Unchanged:
  - membership before completion;
  - duplicate-acceptance rejection;
  - post-acceptance Progress;
  - no re-arming after a rejected command.

**Alias fixture.**
- On Unix it adds an ancestor symlink (`parent/alias → parent/actual`) and guards it with `assert_ne!(raw, canonical)`. Linux therefore exercises the same mismatch. The shared files themselves stay regular.
- On Windows, `canonicalize()` always returns a verbatim path, so the mismatch exists without symlink privileges.
- `_fixture` lives to the end of each test. `root` is dropped first, then `parent`.

**Negative control [exec].**
- I diffed the three copies. `alias-baseline/services.rs` is exactly the baseline plus the fixture; the candidate is exactly that plus the three expectation changes.
- On Linux, the alias baseline fails exactly these three tests, at `:1320`, `:2052` and `:2268`. These are the native lines shifted by the 18-line fixture.
- The messages match the native ones word for word, including `left: 0` / `right: 1`.

## 4. Findings

### R7-L1 (Low; disclosure, exposed only natively). Windows `fixture()` tests move to verbatim paths

**What changes.**
- On Windows, `std::env::temp_dir().canonicalize()` returns a verbatim `\\?\C:\…\Temp` path.
- Every Windows-run `fixture()` test therefore switches from the raw `%TEMP%` spelling to the verbatim one. That is 21 tests.
- 19 of them pass on Windows today (CI log lines 614-678). 10 of these use the native watcher and Missing proofs. The other two tests are the ones this patch fixes.

**This is the right direction.** Production picker and folder imports store verbatim paths on Windows (`canonical_video`), so the tests become more faithful.

**It newly exercises:**
- `local_mount`: `components().take(2)` yields `\\?\C:\`, which is passed to `GetDriveTypeW` (`src/library_files.rs:1610-1620`);
- notify's ReadDirectoryChangesW backend with a verbatim directory. It builds event paths as `registered_dir.join(name)` (`notify-8.2.0/src/windows.rs:167-220`, `:394-398`), so events still match;
- `readable_absence` and the parent identity on verbatim paths;
- `trash_windows::recycle`, which already strips the verbatim prefix for the shell (`src/library_files/trash_windows.rs:51`, `:92`).

**Risk.** I expect `GetDriveTypeW` to accept `\\?\C:\`, because it goes through the same NT path conversion that serves volume-GUID paths. This was not executed.

**Requests:**
- State this in `windows-files.md` or in the VALIDATION entry.
- If native CI shows new failures in these 19 tests, triage them as production verbatim-path defects. Do not revert the fixture to raw spellings for Windows; that would hide a defect in the path production actually uses.

### R7-L2 (Low; precision of the documented limit). The raw-alias watcher limit: platform, producers, bound

The root's planned sentence is true: "Arbitrary or legacy raw-alias library paths can miss native watcher events…; ordinary picker/folder imports canonicalize their paths." Three refinements would make it precise.

1. **Platform.**
   - The limit applies to macOS: FSEvents reports resolved paths.
   - inotify and notify's ReadDirectoryChangesW backend build event paths from the registered spelling (`inotify.rs:217-218`, `windows.rs:394-398`), so raw keys still match on Linux and Windows.
2. **Producers.**
   - **A current one.** HTTPS direct and archive.org downloads (`FetchSource::Files`, `src/fetch.rs:153-167`) save to `folder.join(name)` (`src/fetch/http.rs:926-944`). That path is restored as-is (`src/fetch_ui.rs:189-195`).
   - **The folder is never canonicalized.** It is the configured download folder: the default (`src/services.rs:307-314`), a picker choice (`src/library_ui.rs:1010`), a typed value, or `FLICK_DOWNLOAD_DIR`.
   - **Default case.** The default `~/Downloads/Flick` is normally canonical on macOS. The issue therefore affects only symlinked or aliased folders.
   - **The handoff's claim.** `windows-files.md` says "Download completion paths are also canonicalized". That holds only for yt-dlp (`src/fetch/ytdlp.rs:396`) and torrents.
   - **Others.** Legacy records, and the store's raw `add_video` and `add_videos` callers.
3. **Bound.**
   - When the window goes from inactive to active, `poll_library_files` calls `reconcile_focus()` (`src/library_files_ui.rs:174-179`). The worker then runs a full sweep, at most once per 5 s (`src/library_files.rs:27`, `:857-860`). A restart also re-inspects every record.
   - What is lost is live detection while Flick stays focused or in the background. The common case is still covered: delete in Finder, then return to Flick.

`report.md:65` at HEAD names "path aliases on case-insensitive mounts", but not symlink or firmlink aliases. The new sentence fills that gap; please fold in (1)–(3).

### R7-L3 (Low; validation record). Assertions native CI will run for the first time

These assertions will run natively for the first time:

| Platform | Test | Assertions |
| --- | --- | --- |
| Windows | `timestamp_or_length_…` | Both `replaced: false` checks. |
| Windows | `media_replacement_…` | Passes only because the fixture sets a distinct creation time. |
| Windows and macOS | `deleted_output_…` | Everything after the formerly failing line: Progress or Unavailable, zero verified bytes. |
| Windows and macOS | `accepted_download_…` | Everything after the formerly failing line: completion membership, the second round, no re-arming after a rejected command. |

Linux runs the services assertions with aliases, and they pass. They have not run natively.

**Request.** When recording the native CI rerun, list these as first native executions. Describe the Windows replacement test as using an "identity-distinct fixture", not as Windows replacement detection.

### Nits

- **N1.** In `readiness_and_acceptance_…`, `selected` could come from `session.selected_file_paths(&shared.infohash)?`, filtered to videos, to mirror `transfer_command` exactly. The current `canonicalize()` form is acceptable, and it also pins the engine identity to std canonicalization. No change is required.
- **N2.** The `fixture()` comment mentions only macOS `/var` vs `/private/var`. It could also note Windows verbatim canonicalization (R7-L1). This is optional.
- **N3.** `windows-files.md` line 14 lists "Unchanged production prefix SHA-256: `1d9a860c…`". That is the hash of the whole baseline file. The actual prefix hashes are in §1: `63b487ac…` for `library_files.rs` and `c4f6563b…` for `services.rs`. The byte-identity claim is true; only the label or value is wrong. This matters because the root compares hashes.

## 5. Are the handoffs truthful?

| Claim | Status |
| --- | --- |
| Test-only; production prefixes byte-identical | True [exec]. The prefix hash in `windows-files.md` is mislabeled (N3). |
| Patch, candidate and baseline hashes; the patches apply and round-trip | True [exec] |
| 37 baseline and 37 candidate Linux filesystem tests pass | True [exec, on 1.99.0] |
| Windows-GNU check and strict Clippy; Linux strict Clippy; rustfmt | True [exec] |
| The two private alias diagnostics pass | [reported]; I did not re-run them. I confirmed the claims they support statically in notify and `library_files.rs`. |
| Services: baseline 3 pass; the fixture-only baseline fails exactly 3, matching native; candidate 19 pass; strict Clippy; rustfmt | True [exec, on 1.99.0 and on 1.99.1] |
| The Windows creation-time limit is not presented as fixed | True. R7-L3 asks that the VALIDATION wording keep it so. |
| "Download completion paths are also canonicalized" | Only partly true (R7-L2). |
| No claim of Windows or macOS execution, a desktop build, or playback | True |
| Diagnoses: identity collision, read-only handle, canonical FSEvents paths, raw vs engine-canonical paths | Consistent with the native panic sites, the production code and pinned notify [static; dep]. Confirmed by my negative control [exec]. |

## 6. Checks executed in round 7 (for `docs/VALIDATION.md`)

**What these checks are not.**
- They are Linux backend tests plus Windows cross-target type checks and lint.
- None is a native Windows or macOS execution, an MSVC build, a desktop build, a GUI session or playback.
- No cargo command used the repository manifest or `target/`.
- None used the network, public peers, trackers or DHT. The services tests used `TransferConfig::local` with explicit loopback peers and generated files.

**Executed by me**

1. **Freeze**
   - **Command:** `python3 -I $R7/scripts/verify_freeze.py <repo>`
   - **Result:**
     - The R6 manifest matched 50/50 at the start (before 17:39 UTC), at 17:46:48 and at the end (§1). The end check's time is under "Final verification" below.
     - R6 and R7 differ in exactly the two expected entries.
     - With those two entries mapped to the private candidates, the R7 manifest matches 50/50.
     - The patch and CI-log hashes match. HEAD is `8bec302`.
2. **Patches**
   - **Commands:** `git apply --check --whitespace=error` for both patches, against the shared tree with no write; then `git apply --numstat`.
   - **Result:** both checks pass; the counts are 25/2 and 27/6.
   - **Round trip:** in `$R7/roundtrip`, applied to the blobs from `git show 8bec302:`. `cmp` shows both results equal to the candidates.
3. **Production prefix:** the first differing byte lies inside `#[cfg(test)] mod tests` in both files. Prefix hashes are in §1.
4. **Input copies**
   - **Setup:**
     - `$R7/files-{baseline,candidate}` and `$R7/services/{baseline,alias-baseline,candidate}` were copied from the authors' private harnesses.
     - Every source hash was verified.
     - `library.rs` and `trash_windows.rs` are byte-identical to the repository.
     - The services harness compiles the real `src/lib.rs`, read-only.
5. **Services, pinned toolchain (rustc 1.99.0, b940084d7)**
   - **Command:** `CARGO_TARGET_DIR=$R7/target-services-1990 CARGO_NET_OFFLINE=true FLICK_LOCAL_TRANSFERS=1 cargo +1.99.0 test --offline --locked --manifest-path $R7/services/Cargo.toml --bin <bin> …`
   - **Results:**
     - candidate, `services::`: 19 passed, 0 failed, in 20.53 s;
     - alias baseline, three test filters: 0 passed and 3 failed, as expected (exit code 101). The messages are identical to the native ones;
     - baseline, the same three filters: 3 passed;
     - `cargo +1.99.0 clippy … --bin windows-services-candidate --tests -- -D warnings`: exit code 0.
   - **Log:** `$R7/services-checks-1990.log`.
6. **Services, stable rustc 1.99.1**
   - This was my first run. rustup resolved the toolchain from the private directory, which has no toolchain file.
   - **Results:** identical: 19 passed (21.27 s), 3 failed as expected, 3 passed, Clippy exit code 0.
   - **Log:** `$R7/services-checks.log`.
   - I kept this run as corroboration. Check 5 is the pinned-toolchain record.
7. **Files tests, rustc 1.99.0**
   - **Command:** `CARGO_TARGET_DIR=$R7/target-files CARGO_NET_OFFLINE=true cargo test --offline --locked --manifest-path $R7/files-candidate/Cargo.toml --lib library_files::tests:: -- --test-threads=1`
   - **Result:** 37 passed, in 10.29 s.
   - **Baseline:** the same command against the baseline copy, in `$R7/target-files-baseline`: 37 passed, in 10.37 s.
8. **Files tests with an aliased `TMPDIR`**
   - **Setup:** `TMPDIR=$R7/tmp-alias`, a symlink to `$R7/tmp-actual`.
   - **Run:** the 22 `fixture()` tests, with `--exact`.
   - **Result:** 22 passed, in 8.42 s. The temporary directory was left empty.
9. **Files lint and type check**
   - **Linux Clippy:** `cargo clippy --offline --locked … --lib --tests -- -D warnings -A dead_code`: exit code 0.
   - **Windows type check:** `cargo check --offline --locked … --target x86_64-pc-windows-gnu --tests`: exit code 0. The only warning is the harness's unused `retain`.
   - **Windows Clippy:** `cargo clippy … --target x86_64-pc-windows-gnu --lib --tests -- -D warnings -A dead_code`: exit code 0.
   - **Log:** `$R7/files-checks.log`.
10. **Formatting:** `rustfmt --edition 2024 --check` on both candidates: exit code 0.

**Inspected, not re-run**

11. The author's private alias-diagnostics log and Windows `FileTimesExt` probe. The native CI logs, after hash verification.

**Static**

12. The production paths cited in §§2–4; the pinned `notify-8.2.0` FSEvents, inotify and Windows backends [dep]; `git diff dde771c 8bec302`.

**Not run by me**

13. Any Windows or macOS execution, an MSVC build, a desktop build, playback, and the integrated Linux suite with both patches applied. The last is the root's gate.

**Final verification, at 17:54:37 UTC, after this file was written:**
- HEAD is `8bec302`.
- The R6 manifest matches 50/50. R6 and R7 differ only in `src/library_files.rs` and `src/services.rs`.
- With those two entries mapped to the private candidates, the R7 manifest matches 50/50. Both candidates still hash to `2ac854f8…` and `e82c85ff…`.
- The patch and CI-log hashes are unchanged.
- In `git status`, `claude-review-r7.md` is my only addition. The other entries listed in §1 are concurrent changes by others to non-frozen docs.
