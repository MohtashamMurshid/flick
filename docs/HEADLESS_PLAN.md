# Flick headless agent implementation plan

**Status: deferred; not implemented.** The [playback and sharing plan](SHARING_PLAN.md) supersedes this as the next implementation milestone. The existing GPUI desktop player remains the primary frontend. Small inspection/frame diagnostics may be developed independently; the general protocol below is not part of the first transfer release. The current `--smoke-test VIDEO` command is a playback diagnostic, not the headless product described here.

## Outcome

An agent can open local media, inspect its state and tracks, control playback, attach subtitles, wait for a condition, and export a frame through a terminal process. It can do this on Windows, macOS, and Linux without a window, display server, GPU, or audio device. Every operation produces a machine-readable result or an actionable error.

The first interface is a local CLI with a persistent NDJSON session over standard input/output. No HTTP service, remote control server, or MCP server is required for this milestone.

## Architecture and build boundary

Extract the playback orchestration currently mixed into `src/ui.rs` into shared modules:

- **Core:** pure Rust command types, validated configuration, playback state transitions, media-generation IDs, wait conditions, and typed errors. No GPUI, native handles, file dialogs, application history, or window lifecycle.
- **Playback adapter:** the existing dynamically loaded libmpv boundary, with explicit initialization options and typed state/events. Own its native handles on one worker thread; construct the non-`Send` player on that thread. Both frontends use this adapter through the core runtime.
- **Desktop frontend:** GPUI controls, image presentation, keyboard shortcuts, file dialogs, and the existing local preferences/history. It translates input into shared commands and presents shared state.
- **Headless frontend:** CLI parsing, stdio protocol, JSON serialization, cancellation/deadlines, and artifact encoding. It owns its session state and never initializes GPUI.

Keep native operations separate from pure state transitions so unit tests can exercise the core with a fake adapter. Headless frame export must use native decoded pixels rather than a screenshot of the desktop frontend. Convert the adapter's BGRA bytes to the encoder's expected channel order; include a red/blue fixture to catch swapped channels.

Make GPUI and desktop-only dependencies optional under a `desktop` Cargo feature. Keep desktop as the default build. Introduce a separate `flick-headless` binary under a `headless` feature; the desktop binary requires `desktop`. Proposed builds:

```sh
cargo build --release --bin flick
cargo build --release --no-default-features --features headless --bin flick-headless
cargo tree --no-default-features --features headless
```

Acceptance requires the headless dependency graph to exclude GPUI and windowing dependencies, and the produced binary to have no GPUI linkage or initialization path. Use the same locked dependencies for both frontends.

## Runtime defaults

- Software decoding and software frame rendering; hardware decoding disabled. No Vulkan, Metal, OpenGL, or window creation is needed in CPU mode.
- Timed null audio output. Playback time advances normally without opening an audio device. Audible output may be an explicit later option; it is never a headless prerequisite.
- No dialogs, desktop activation, persisted GUI preferences, recent-file updates, or resume restoration. Headless state is session-local. Existing desktop settings/history files must remain byte-for-byte unchanged during headless tests.
- Explicit local input/output paths. Resolve relative paths against the process working directory and report the resolved paths. Return a typed error for a path the native adapter cannot represent; do not silently change it.
- `open` defaults to paused so inspection and frame work do not race normal playback. `play` is explicit. The desktop frontend retains its own autoplay behavior.
- The native playback dependency is still required. Missing or incompatible libmpv must produce a structured diagnostic and a nonzero one-shot exit code, without attempting a GUI fallback.

Replace reliance on `FLICK_AUDIO` for these defaults with explicit adapter options. Environment overrides may remain for diagnostics, but must not accidentally turn an ordinary headless invocation into an audio-device or GPU requirement.

## Proposed CLI

These commands are examples of the planned interface; they do not work yet:

```sh
# Inspect a local file, then release the decoder and exit.
flick-headless inspect ./movie.mkv --json --timeout-ms 10000

# Export a frame after seeking, with documented timing tolerance.
flick-headless frame ./movie.mkv --at 12.5 --tolerance-ms 100 \
  --output ./artifacts/frame-12.5.png --timeout-ms 10000 --json

# Persistent control through stdin/stdout; one JSON object per line.
flick-headless --stdio --protocol-version 1
```

One-shot commands use the same core commands and result schemas as stdio sessions. Successful one-shot output is one JSON object on stdout. All diagnostics go to stderr. Frame files are explicit artifacts; image bytes are never mixed into stdout. Fail rather than overwrite an existing artifact unless the caller supplies `--overwrite` or the equivalent protocol parameter.

## Stdio protocol v1

UTF-8 NDJSON: one complete JSON object per line, flushed after writing. Publish versioned request, response, event, and error JSON Schemas with examples. Every message includes `v: 1` and a `type`. Every request has a caller-supplied string `id`; its terminal response echoes that ID. Events have no request ID and carry an increasing `seq` and the current `media_id` when applicable.

Reject unsupported versions, unknown commands, invalid field types, nonfinite numbers, and duplicate in-flight IDs with stable error codes. Malformed JSON receives an error with `id: null` when no valid ID can be recovered. No native log lines, progress bars, startup banners, or panic text may appear on stdout; configure Rust tracing and native diagnostics accordingly.

| Command | Parameters and completion contract |
| --- | --- |
| `open` | Local `path`, optional `autoplay` (default false). Respond after the matching file-loaded event and initial state are available, or loading fails/times out. Assign a new `media_id`; discard prior media's stale events. |
| `status` | Return state, `media_id`, path, position/duration in seconds, pause/EOF flags, volume, mute, speed, and active track IDs. Unavailable values are explicit `null`. |
| `play` / `pause` | Set an explicit state rather than toggling. Return the observed state. At EOF, document and test whether `play` restarts at zero; choose the desktop's restart behavior for consistency. |
| `seek` | Absolute seconds or a relative delta, with a finite `timeout_ms` and `tolerance_ms`. Validate/clamp against known bounds, report the effective target, and wait for an observable result. Command acceptance alone is not seek completion. |
| `configure` | Whitelisted `volume` (0–100), `muted`, and `speed` (0.25–4). Validate the complete update before applying it; report applied state and any native failure. No arbitrary libmpv command passthrough. |
| `list_tracks` | Return stable native IDs scoped to this `media_id`, type, language, title, and selected state. |
| `select_track` | `kind` (`audio` or `subtitle`) and an ID returned for this media; `null` disables that kind. Reject stale/missing IDs. |
| `subtitle` | Load an explicit local subtitle `path`, with `select` default true. Respond with the resulting subtitle track information or a structured load error. |
| `frame` | Optional target seconds, timing tolerance, output path, and overwrite flag. Pause, seek if requested, wait for a fresh render, and write a PNG. Leave the session paused at the resulting position and report that state. |
| `wait` | A condition such as `loaded`, `paused`, `position_at_least`, or `eof`; optional expected `media_id`; finite `timeout_ms`. Complete when satisfied, cancelled, superseded by new media, or timed out. |
| `cancel` | `request_id` of an outstanding cancellable operation. Acknowledge cancellation separately; the target receives exactly one terminal response. Already completed operations are not undone. |
| `close` | Cancel media-specific waits/jobs, unload the current file, release frame buffers, and return the session to idle. Keep the process available for another `open`. |
| `shutdown` | Stop playback, cancel outstanding requests, release native resources, flush the final response, and exit. EOF on stdin requests the same cleanup without requiring another input line. |

Emit `ready`, `opened`, `state_changed`, `tracks_changed`, `eof`, `artifact_written`, and `error` events. Include capabilities in `ready` so an agent can discover protocol version, software rendering, artifact formats, and supported timing guarantees. Position telemetry is coalesced to a bounded rate; a caller can always request `status`. State and error events must remain observable while a `wait` request is outstanding.

Example round trip, shown without asynchronous state events for readability:

```json
{"v":1,"type":"event","seq":1,"event":"ready","data":{"protocol_versions":[1],"rendering":"software","audio":"null","frame_timing":"observed_position"}}
{"v":1,"type":"request","id":"open-1","command":"open","params":{"path":"/media/movie.mkv","autoplay":false,"timeout_ms":10000}}
{"v":1,"type":"response","id":"open-1","ok":true,"result":{"media_id":1,"state":"paused","position":0.0,"duration":120.0}}
{"v":1,"type":"request","id":"frame-1","command":"frame","params":{"media_id":1,"at":12.5,"tolerance_ms":100,"output":"/artifacts/frame.png","timeout_ms":10000}}
{"v":1,"type":"response","id":"frame-1","ok":true,"result":{"media_id":1,"state":"paused","artifact":{"path":"/artifacts/frame.png","format":"png","width":640,"height":360,"requested_position":12.5,"observed_position":12.48,"actual_frame_pts":null,"timing_basis":"observed_position","tolerance_ms":100,"subtitles":"composited"}}}
{"v":1,"type":"request","id":"wait-1","command":"wait","params":{"condition":"eof","media_id":1,"timeout_ms":1000}}
{"v":1,"type":"response","id":"wait-1","ok":false,"error":{"code":"TIMEOUT","message":"EOF was not reached within 1000 ms","retryable":true,"details":{"state":"paused","media_id":1}}}
{"v":1,"type":"request","id":"quit-1","command":"shutdown","params":{}}
{"v":1,"type":"response","id":"quit-1","ok":true,"result":{"state":"closed"}}
```

Requests and responses share a transport in this transcript: the agent writes requests to stdin and reads responses/events from stdout. Publish separate input and expected-output fixture files to remove that ambiguity in executable examples.

## Bounded work, errors, and artifacts

Default operation timeout: 10 seconds; permit explicit values up to 60 seconds for the initial version. `wait` must register a nonblocking condition, not occupy the command reader or prevent `status`, `cancel`, `close`, or `shutdown`. Bound input line length, pending requests, frame dimensions, and queued artifacts; start with 1 MiB lines, 32 outstanding requests, one active frame export, and a documented pixel limit. Return `BUSY` or `INVALID_ARGUMENT` when limits are reached. Coalesce telemetry under backpressure without silently dropping terminal responses.

Cancellation checks run between native operations and while waiting for events. A cancelled frame job removes its own incomplete temporary artifact; it never removes a pre-existing destination. Open/seek operations already accepted by the engine can have effects even when the caller cancels waiting; return the latest state so the agent can inspect or close the media. A native call itself may be uninterruptible: enforce a runtime watchdog that terminates the headless process with a distinct `ENGINE_UNRESPONSIVE` failure if it cannot regain control within its deadline. The invoking agent should also impose a process timeout. Do not claim cooperative cancellation interrupts a blocked foreign-library call.

Use structured errors with `code`, readable `message`, `retryable`, and relevant `details`. Define at least `INVALID_REQUEST`, `UNSUPPORTED_VERSION`, `UNKNOWN_COMMAND`, `INVALID_ARGUMENT`, `NO_MEDIA`, `FILE_NOT_FOUND`, `DEPENDENCY_UNAVAILABLE`, `DECODE_FAILED`, `TRACK_NOT_FOUND`, `MEDIA_REPLACED`, `TIMESTAMP_UNAVAILABLE`, `OUTPUT_EXISTS`, `OUTPUT_FAILED`, `TIMEOUT`, `CANCELLED`, `BUSY`, and `ENGINE_UNRESPONSIVE`.

Document one-shot exit codes: 0 success, 2 invalid invocation/protocol input, 3 media failure, 4 missing/incompatible native dependency, 5 timeout or unresponsive engine, and 6 artifact write failure. In persistent sessions, ordinary request errors do not terminate the session; a clean `shutdown` exits 0. Protocol-fatal failures and native hangs exit nonzero. Include the native error and operation in `details` when available, without replacing the stable public code.

Frame export does **not** promise an exact source frame merely because libmpv accepted an `absolute+exact` seek. Wait for seek completion and a newly rendered image, then report requested position, observed playback position, tolerance, dimensions, subtitle composition, and output path. Current playback position is not proof of the rendered frame's presentation timestamp. Keep `actual_frame_pts` null unless the adapter can reliably obtain and correlate that timestamp; expose the timing basis in capabilities/results. A caller requesting verified frame-PTS precision must receive `TIMESTAMP_UNAVAILABLE` if that guarantee is unavailable. Use variable-frame-rate and sparse-keyframe fixtures to test tolerance failures rather than silently labeling the nearest frame exact.

Write PNGs through a same-directory temporary artifact, flush and close it, then commit the destination according to the overwrite policy. The artifact's existence and dimensions must be independently verifiable. Do not announce success before encoding/writing completes.

## Delivery sequence and acceptance

### 1. Extract the shared core

- Move playback orchestration/state out of GPUI views; make adapter initialization explicit.
- Keep history/dialogs/window code in the desktop frontend; preserve current viewing behavior.
- Add optional desktop/headless features and separate binary targets.
- Add pure tests for legal commands, media replacement, EOF/replay, typed failures, and deadline/cancellation transitions.

**Acceptance:** existing desktop checks remain green; the headless build graph excludes GPUI/windowing; a fake-adapter core test runs without initializing native playback or desktop services.

### 2. Deliver one-shot headless inspection and frame export

- Implement `inspect` and `frame` using CPU rendering and null audio.
- Implement validated options, structured results/errors, timing metadata, artifact writes, and exit codes.
- Make the current diagnostic use explicit adapter options or retain it as a clearly separate diagnostic.

**Acceptance:** inspect MP4/MKV fixtures and export a correct PNG with all display environment variables unset, no Xvfb, GPU initialization, or audio device. Nonexistent/corrupt media, missing libmpv, timeout, existing output, and unwritable output return their documented failures. No headless operation changes GUI settings/history. Strict timing requests fail honestly when frame PTS is unavailable.

### 3. Deliver persistent stdio sessions

- Implement v1 schemas, parser/writer, request IDs, events, command dispatch, bounded waits, cancellation, and watchdog behavior.
- Cover the complete command table, including track selection, subtitle loading, close/reopen, and shutdown.
- Keep stdout valid NDJSON and route application/native logs to stderr. Test partial input reads and multiple lines arriving together.

**Acceptance:** an external process drives one session through open → inspect tracks → play → pause → seek → configure → subtitle → frame → close → reopen → shutdown. Every request receives one correlated terminal response. A paused `wait(eof)` times out while concurrent `status` and `cancel` remain responsive. Reopening cannot attach stale events/artifacts to the new media. EOF on stdin and explicit shutdown exit within a documented bounded deadline and release media files.

### 4. Add native CI and agent documentation

- Build and test headless on native Windows, macOS, and Linux runners with real libmpv. Run without Xvfb or any desktop session; unset `DISPLAY`, `WAYLAND_DISPLAY`, and related Linux session variables. Audit dependencies/linkage and run software/null-audio tests without requesting graphics or audio services.
- Generate deterministic short media fixtures with known audio/subtitle tracks, a red/blue color check, sparse keyframes, and variable frame rate. Validate frame contents and dimensions as well as JSON.
- Add process-level protocol fixtures, schema validation, deadlines, cancellation/backpressure cases, malformed input, Unicode/spaced paths, write failures, EOF/replay, and repeated open/close coverage.
- Publish copyable shell and small agent-client examples, the protocol/schema version policy, error/exit-code reference, and a native validation matrix. Record actual runtime/dependency versions and elapsed startup/seek/export timings.

**Acceptance:** the same protocol fixtures and CLI examples pass on all three native OS jobs without a display server. Automated checks prove stdout JSON hygiene, bounded failure behavior, useful diagnostics, correct artifacts, and unchanged GUI history. List any unverified native behavior explicitly; this milestone does not establish a speed advantage over another player.
