# Final readiness and fixture follow-up

Round 8 covers only changes after `629446b`. Earlier application changes and platform fixtures are approved by rounds 6 and 7. The new frozen manifest includes 51 source/Cargo/test/reproducer/toolchain inputs.

| Finding | Response |
|---|---|
| CodeRabbit 4371830586: platform evidence scope | Label the table as evidence by revision, mark the failed `8bec302` snapshots, and distinguish earlier pre-PR macOS playback/CoreAudio observations from final PR validation. Preserve historical evidence without presenting it as current acceptance. |
| CodeRabbit 4371830594: measurement spacing | Separate prose numbers and units; retain every measurement and meaning. |
| CodeRabbit review 5644560711: Load after failed startup | Ignore only readiness `Load` after terminal failure, whose availability was already reported. Explicit metadata/deletion commands still reject. Extend the existing corrupt-state test with three Loads and a FIFO reply fence: baseline fails, fix passes, and explicit Resolve still produces its error. No ownership/Retry/startup path changes. |
| Duplicate Windows CI parent-identity failure | Retain canonical Windows fixtures; set the recreated directory's actual creation time distinctly after creation and assert identity differs before both original Unavailable guards. This is a deterministic fixture for the documented creation-time identity limit, not a claim to fix that production limit. |

The parent fixture matches the private candidate exactly and preserves the production prefix. It passes Linux focused testing and Linux/Windows-GNU lint/type checks. Windows directory setter execution remains for native CI.

Integrated Linux checks pass 244 tests, strict Clippy, formatting and debug build. Exact checks are in `final-checks-r8.json`. Round 8 approves the exact frozen source without Blocking, High or Medium findings. All 15 check entries pass on application revision `e16c4e7`, including duplicate workflow jobs; the durable record is `ci-e16c4e7.json`.

R8-L1 wording correction: Load preserves the current notice by suppressing subsequent raw errors after terminal failure. It does not alter initial error ordering (`Unavailable`, then raw error) or the availability-driven Retry button. After dismissal, the reason may be absent until Retry; this optional UI improvement remains deferred. R8-N1/N2 clarify the earlier Windows failure was its first replaced-parent Unavailable assertion, not the newly added identity assertion, and link the committed CI record. Final table/summary entries now include the passing application revision. Spacing edits also clarify `1k` as `1,000 entries` and supervisor `143` as its status. No source was changed for these wording corrections.
