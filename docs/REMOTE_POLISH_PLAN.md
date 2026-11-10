# Remote library polish

**Status: implemented on 2026-11-11 and tested on loopback, with the server checked with `curl` on a real Tailscale address at `981d26a` ([validation](VALIDATION.md#remote-library-polish-on-2026-11-11)). Review fixes landed at `eeb7237`. Nothing has been used from a second machine or in the app's own screens yet.** A set of smaller improvements on top of the [remote library](REMOTE_PLAN.md) and [downloads to another machine](REMOTE_DOWNLOADS_PLAN.md), chosen for how much they change daily use. Each is independent; together they remove the last awkward steps from setting up and living with a second machine.

## What changes

**Find Flicks automatically.** Onboarding and Add Server list the machines on the tailnet (from `tailscale status`) that answer `GET /v1/hello` on the default port, by name. Pairing is a six-digit code shown on the server's Settings page or printed by `flick-serve --pair-code`, typed on the client; the server returns the pairing string. A code lives ten minutes, allows five wrong attempts, and the unauthenticated routes are rate-limited. Pasting a full pairing string still works.

**Automatic quality.** For a remote video, two stalls in the first minute, or one stall longer than five seconds, drop one tier and say so in the player. Choosing a tier by hand turns it off for that session; a per-server switch turns it off for good.

**Continue Watching that follows you.** The server already receives positions from every client; its listing now carries the last saved position per video, and the client uses the newer of its own and the server's. Finish on one machine, resume on another.

**Keep on this Mac.** A remote video's card menu offers a copy to this computer, into the download folder under a folder named after the server, with progress in Downloads. The copy appears in the local library with a mark, plays locally, and can be removed from the same menu. Interrupted copies resume by byte range.

**Finish notifications.** A system notification when a download started on another machine completes, or a kept copy finishes.

**Wake the machine.** The server advertises its own MAC and LAN addresses; an offline server's row offers Wake, which sends a Wake-on-LAN packet. Works only from the same network. The in-app server and the launchd agent keep the machine from idle-sleeping while serving.

**Audio tracks in transcodes.** The listing names the audio streams inside a file; a transcode tier can be requested with one of them, and the player's audio menu offers them.

**Queue for machines that are asleep.** A download or link started while its machine does not answer is kept on the client and sent when the machine answers again; Downloads shows it as waiting.

**Housekeeping.** The subtitle cache and transcode temporaries are capped by size; `flick-serve status` reports a running server; the launchd agent is installed and checked on a real Mac mini.

## Not done here

A menu bar app for the headless server (GPUI has no status item support; `flick-serve status` covers the need from a terminal), the GitHub Actions billing lock, and the Ethernet cable.

## Delivery

- [x] Server: hello, pairing codes, hardware addresses, resume in the listing, audio tracks and `?audio=` tiers, pruning, `status`.
- [x] Desktop modules: discovery, keep-copy worker, auto-quality state machine, notifications.
- [x] In-app server: show a pairing code, keep awake while serving; launchd agent installed and verified.
- [x] Views: discovery in onboarding and Add Server, pairing by code, Wake on offline rows, Keep on this Mac in card menus and Downloads, queued rows, the auto-quality switch, audio tracks in the player menu.
- [x] Player and client glue: auto quality on tick with its message, Continue Watching merge, queue replay, notifications on completion.
- [x] Review, fixes, docs, validation. Two reviews (Claude, then a four-round GPT Sol loop ending in approve); every finding fixed with a test; recorded at `eeb7237`.
