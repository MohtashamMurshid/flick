# Platform fixture follow-up

The Windows desktop job on `8bec302` failed five tests; the macOS desktop job failed nine. Core and transfer tests without desktop passed on Linux, Windows and macOS. The platform failures prompted two private test-only candidates, reviewed separately from the unchanged round 6 application freeze.

| Failure | Candidate response |
|---|---|
| Three service tests compare raw fixture paths with canonical engine identities | Simulated acceptance uses the canonical source; the output path is canonicalized before deletion; expected download roots are independently canonicalized. A Unix ancestor-symlink fixture reproduces all three failures on Linux and keeps those alias cases in the existing tests. All event-ordering, repeat-admission, completion membership and byte-verification assertions remain. |
| Six macOS worker tests wait for an aliased path while native events use canonical paths | Construct the shared fixture beneath the canonical temporary directory, matching picker/folder import paths. No forced rescan, relaxed timeout, synthetic Missing or skipped test is added. |
| Windows replacement fixture shares the old creation-time identity | After rename, explicitly set a distinct creation time and assert actual identity differs before checking both existing replacement observations. This makes the fixture deterministic; it does not fix the known creation-time identity collision limitation. |
| Windows timestamp-only test uses a read-only handle | Open the fixture with write access before setting its timestamp. Both time/length updates must still report `replaced: false`. |

Private candidate checks: 37 filesystem tests, two alias diagnostics, Linux/Windows GNU cross-target lint and Windows type checking passed. Service tests: ordinary baseline three pass; alias baseline reproduces exactly three assertion failures; candidate suite 19 pass, strict lint and formatting pass. These are Linux backend and cross-target checks, not native Windows/macOS execution, a desktop build or playback.

The patches preserve application code byte-for-byte. The expected candidate manifest is `review-r7-candidate-hashes.json`; at review start the shared source still matches round 6. Both patches were applied exactly after both reviews finished. All 50 expected candidate hashes match; production prefixes remain byte-identical. Integrated Linux checks and native CI reruns are recorded separately.

Existing production limits must remain explicit: Windows creation-time identity can collide or be retained by file tunneling. On macOS only, raw-alias records can miss native FSEvents because indexing uses exact paths. Direct HTTPS/archive.org downloads can still produce these records; yt-dlp, torrents and picker/folder imports canonicalize paths. Focus reconciliation clears stale entries, with sweeps limited to once per five seconds. Correcting the fixtures does not claim those cases are solved.

Round 7 approval covers only the two private test candidates. Round 6 independently reviews the combined production changes; its result remains required before merge.

Round 6 approves the application delta and round 7 independently approves both exact test candidates, with no Blocking, High or Medium findings. R7-L1: canonical Windows fixtures now exercise verbatim `\\?\C:\…` paths across 21 tests; any new failure must be treated as a production path issue, not hidden by reverting the fixture. R7-L3: the replacement test explicitly sets a distinct creation time; it cannot establish that Windows always distinguishes replaced files. Several later assertions are newly reached by native CI. Actual production-prefix SHA-256 values are `63b487acb35ad50c689d7c42c390f88591adfd38711664bca9ea2c5e207efaac` (filesystem) and `c4f6563bc52572867b8af30c4ef59a17bd9b28f40c316dda87497c26c7b3eb59` (services); the private filesystem handoff mislabeled the whole-file hash as its prefix.
