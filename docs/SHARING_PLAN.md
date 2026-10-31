# Playback, downloads, and sharing

Direction agreed on 2026-10-24. This plan describes the intended product; unchecked items in [PLAN.md](PLAN.md) are not implemented promises.

## User experience

Opening Flick shows Library. It contains Continue watching, All videos, History, and Downloads. Existing history migrates into the library; added videos remain available even if never watched. A user can add multiple files or a folder. Picking a library item opens the player and resumes an eligible saved position. Returning to Library pauses playback and saves its position.

A search field filters local titles as the user types. Enter or Search online asks discovery providers for downloadable videos. A result opens torrent metadata/file selection, the user chooses files and a destination, and the completed verified video joins the library. Search is part of the primary release. Individual source outages are shown separately from an empty result set. Stale searches are cancelled and cannot replace newer results.

The player gets a timeline and playback controls only after media loads. Downloads and Sharing remain accessible alongside viewing.

The first transfer release accepts a magnet link or `.torrent`, resolves its file list, lets the user choose files and a destination, and tracks the download. A completed, verified video gets Play. Any other file can be saved and revealed in its folder.

Share lets the user choose a file or folder, hashes its contents, seeds those existing files, and offers Copy magnet and Export torrent. Show upload/seeding status and Stop sharing. Creating a magnet does not host a cloud copy: a reachable peer must stay online to supply the content. Seeding after completion and after window close must be visible choices.

## Architecture

- `playback`: libmpv lifecycle, video rendering, tracks, timing, seeking, and sound.
- `torrent`: one librqbit session with explicit storage, stable torrent/file identifiers, validated file selection, progress, seeding, and lifecycle management. It must not depend on GPUI, application history, or libmpv.
- `fetch`: link downloads, separate from the torrent session and GPUI: yt-dlp as an external process for video pages, and HTTPS files for direct links and Internet Archive items.
- `ui`: GPUI views and input. The transfer service runs on a background Tokio runtime; the UI consumes typed state/events and never blocks on hashing, metadata, filesystem scanning, or a network wait.
- `store`: desktop viewing settings and resume history. Transfer persistence uses a separate directory and is locked against multiple owners.

Use an application-facing adapter rather than spreading librqbit types through the UI. Pin the dependency and test its behavior before treating it as established support. Keep local playback operational independently of the transfer engine.

The first development CLI is a diagnostic over this same adapter, not a daemon, a general agent protocol, or a second torrent implementation. It supports explicit local files/destinations and bounded download waits. Sharing remains active only while that diagnostic process is running.

## Why this integration

[Torlink](https://github.com/baairon/torlink) is MIT-licensed TypeScript built on Node and WebTorrent. Its current headless API lacks resolved file paths and selected-file control; `seed` and `serve` each start their own queue runtime. Its HTTP file server reads ordinary disk files without checking verified torrent-piece availability. Shipping it unchanged would still require integration work plus another runtime and platform-specific native dependencies.

Use [librqbit](https://github.com/ikatson/rqbit) for native Rust transfers. Reuse or port Torlink code only where it provides a concrete benefit, including the search adapters, and preserve its attribution when copying code. The native search adapters are ported selectively from Torlink, with source notices in `THIRD_PARTY_NOTICES.md`.

## Compatibility gate

Run tests against generated content with DHT, trackers, and local multicast discovery disabled. Bind the test seed to a loopback TCP listener and give its address explicitly to the receiver. Tests must fail within a fixed deadline and release sockets/files on failure as well as success.

Verify:

- Create valid metadata for a single file and a directory, including file sizes exactly divisible by the torrent piece length. [librqbit issue 698](https://github.com/ikatson/rqbit/issues/698) reports an extra empty piece in this case. Do not ship invalid metadata or silently alter content to evade it.
- Resolve file indices and safe absolute destinations. Reject unsafe paths and invalid/empty file selections before starting a transfer.
- Transfer known bytes between independent sessions, pause, stop/restart the receiver, resume, and compare the completed bytes.
- Treat successful command acceptance and an existing/preallocated file as distinct from verified completion.
- Separate transfer storage from GUI settings, prevent concurrent writers to the same state, and stop sessions cleanly.
- Avoid overwriting unrelated content when adding a new download. Resuming or seeding an existing file requires an intentional existing-data path.

## Playback quality

Render at the physical video bounds without a fixed resolution, axis, or pixel-budget cap. Source decoding stays with libmpv, which maintains aspect ratio and letterboxing. Check integer arithmetic, native API representability, and allocation errors; report a failure rather than silently reducing resolution. The CPU transfer path may consume more resources at large display sizes. Efficient native GPU presentation remains a separate milestone.

Native GPU presentation, hardware decode, HDR, battery use, audible synchronization, and cross-platform playback need their own implementation and physical/native checks. Keep the existing software path as a measured fallback.

## Watching during downloads

Use librqbit's verified stream reads, either through a loopback-only authenticated range server or a libmpv stream callback. The desktop now uses the libmpv stream callback (`flicktorrent://`); see [transfers](TRANSFERS.md). A local partial file is never a playback source just because its full-size directory entry exists.

Handle buffering, startup/container metadata, seeks to unavailable pieces, client disconnects, cancellation, and downloads that cannot keep up. Do not expose a remote control or file server by default.

## Release evidence

Keep [validation](VALIDATION.md) current with commands, versions, platform, fixture details, and outcomes. A dependency's advertised support, successful compilation, and interactive acceptance are separate evidence. Ship the notices required by the actual dependency builds; resolve libmpv/FFmpeg packaging before distributing installers.
