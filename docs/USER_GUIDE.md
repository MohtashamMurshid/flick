# Flick user guide

This guide covers everything Flick does, screen by screen. For installation, see the [README](../README.md#getting-started).

## First launch

A new Flick, with an empty library and no paired server, opens to setup: four screens, each with Back, Skip, and Next. Skip leaves that screen's settings as they were when you reached it. Each choice is saved as you make it, through the same controls as Settings.

1. **Where are your videos?** *On this Mac* (*On this computer* on Windows and Linux), or *On another machine on my Tailscale network*. Choosing another machine starts a search of your tailnet and lists the Flicks that answer, by name; choose **Pair** beside one and type the six-digit code it shows (see [Finding Flicks and pairing by code](#finding-flicks-and-pairing-by-code)). Or paste the pairing string that Flick shows in **Settings → Remote Libraries** there, or that `flick-serve --print-pairing` prints, and choose **Test Connection**. Next is enabled once that server answers. Going on with *On this Mac* unpairs a server that was only tested here.
2. **Share this library?** The **Share my library on Tailscale** switch, off by default, with the pairing string, Copy, New Token, and **Show Pairing Code** once sharing starts. See [Remote libraries](#remote-libraries).
3. **Where should downloads go?** This Mac and every paired machine. A machine that is offline, or does not accept downloads, cannot be chosen; the others show their download folder and free space. Below it, **Keep sharing completed downloads?** asks the seeding question with *Keep sharing completed downloads* or *Stop sharing*. Next needs both answers.
4. **You're set.** One line each for Videos, Share this library, Downloads go to, and After downloading, with **Change** to go back to that screen. **Open Library** finishes setup and shows Home.

To run it again, open **Settings**, find **Setup**, and choose **Set Up Again…**. It starts at the first screen, with your first paired server already chosen. If you quit before the last screen, the next launch starts in setup again only when the library is empty and no server is paired.

A Flick that already had a library or a paired server before setup existed opens to its library as before.

## The library

Flick opens to your library, with Continue Watching, All Videos, Shows, History, and Downloads. An empty library shows a welcome page with **Open Video…**, **Add Folder…**, and **Paste Link**. Afterward, Flick opens to **Home**, with Continue Watching and Recently Added.

Use **Add Videos…** or **Add Folder…** to build the library, or drop files and folders into it. Media files are read in place. Click a video to play or continue it. The back arrow returns to the library and pauses playback. Passing a local path on the command line opens that video directly.

**Shows** groups episodes named like `Show S01E02`, `Show 1x02`, or `Show/Season 1/02`, and the next episode starts after a short countdown.

Type in the search field to filter local videos. Press Enter for download results, or paste a magnet link or web address to see a download suggestion and press Enter to start it. Search queries TPB, Internet Archive, and YTS independently and reports sources that are unavailable.

The Linux file picker uses the desktop portal. Drag and drop and command-line paths still work when no portal service is running.

## Playback

Flick plays video inside its window with libmpv: play and pause, seeking, volume and mute, playback speed, frame stepping, chapters, an A–B loop, fullscreen, picture in picture, audio and subtitle track selection with audio delay, external subtitle loading, recent files, and resume positions. Playback controls appear after media loads. While a video plays, its controls hide after 2.5 seconds without pointer movement, and subtitles move above the controls whenever they show. Turn hiding off in **Settings → Playback**.

libmpv uses the platform's hardware decoder when available (`hwdec=auto-copy`). Set `FLICK_HWDEC=no` to compare against software decoding. libmpv renders into the frame GPUI uploads. This frame transfer follows the display's physical video bounds with no fixed resolution or pixel-budget cap, and native dimension or allocation failures are reported instead of silently lowering resolution. Efficient native GPU video presentation is a separate milestone; this version makes no claim of being faster than VLC.

The captions icon opens **Audio & subtitles** to select tracks, adjust audio delay, load a subtitle file, or find subtitles on OpenSubtitles.com. Finding subtitles needs a free personal API key from OpenSubtitles, entered in **Settings → Subtitles** or set with `FLICK_OPENSUBTITLES_KEY`. A key entered in Settings is stored in `settings.json`; the environment variable overrides it and is never stored.

## Torrents and Watch Now

Choose torrent files and a destination; completed, verified videos join the library.

**Watch Now** plays a video while it downloads. It reads only verified pieces and fetches the pieces near the playhead first. Seeking into missing pieces shows Buffering until they arrive.

**Downloads** shows torrent download and upload rates, verification progress, and Pause All / Resume All. Export exact metainfo with **Export .torrent…**. Removing a transfer keeps its files unless you choose **Remove and Delete Files…** and confirm the listed paths.

Setup's third screen asks whether completed downloads keep sharing; until you answer, the welcome page and Downloads ask too, and completed downloads pause. Change this, and the live torrent bandwidth limits, in **Settings → Transfers**. Explicit shares keep sharing regardless of that preference, and all transfers stop when Flick closes. Verified torrent videos save their resume position by infohash and file index, so downloading the same video again into another folder keeps your place.

To share a file or folder, choose it and copy its magnet link. See [transfer controls](TRANSFERS.md#desktop-transfer-controls) for details, and the [transfer diagnostic](TRANSFERS.md), which uses the same native transfer backend without the desktop.

Torrent peers can see your IP address. Download and share only content you have the right to.

## Downloading from web links

Paste an `http` or `https` address into the command center or the search field. Flick names the site (YouTube, X, Vimeo, Internet Archive, or the host) and offers the download, which Enter starts. You can also copy the address and choose **Paste Link** on the Downloads page. Downloads appear under **Link Downloads** with progress, speed, Cancel, errors, Try Again, and Play. Completed videos join the library.

Files are saved to the current download folder: `FLICK_DOWNLOAD_DIR` if set, otherwise `Downloads/Flick` in your home folder. Download only videos you have the right to save.

- **Video pages** such as YouTube or X posts need [yt-dlp](https://github.com/yt-dlp/yt-dlp#installation), an optional program that Flick runs separately. Keep it current; sites change often, and outdated releases fail. Flick uses `FLICK_YTDLP` if it is set to the program's full path, then `yt-dlp` (`yt-dlp.exe` on Windows) on `PATH`, then common install folders that apps opened from the desktop may not have on `PATH`: `/opt/homebrew/bin`, `/usr/local/bin`, and `/opt/local/bin` on macOS; `/usr/local/bin`, `/usr/bin`, `/snap/bin`, and `~/.local/bin` on Linux; and `%LOCALAPPDATA%\Microsoft\WinGet\Links` on Windows. Install it with `brew install yt-dlp`, `winget install yt-dlp.yt-dlp`, `pipx install yt-dlp`, or the official standalone release. Distribution packages are often too old. A whole-playlist address downloads every entry.
- **FFmpeg** is optional. Flick looks for it on `PATH`, beside yt-dlp, and in the same install folders, and passes its location to yt-dlp. With FFmpeg, yt-dlp merges the best video and audio; without it, Flick asks for the best single file that has both.
- **Direct HTTPS video links**, whose address ends in a video file name, download without yt-dlp. If the address turns out to be a web page, yt-dlp handles it instead.
- **Internet Archive** search results and `archive.org/details/…` links list the item's video files first, preselecting one copy of each video and preferring the uploaded original. Downloads go over HTTPS from archive.org into a folder named after the item and are checked against the published size and SHA-1.

Flick never overwrites an existing file: a different file with the same name gets a numbered name, and an identical Archive file is reused. Partial files use dot-prefixed names until a download finishes. Cancelling, a failure, or closing Flick removes them. An HTTPS download resumes after a dropped connection, and continues from a leftover partial file when the same download starts again.

## Remote libraries

A remote library lets one computer keep the videos and another watch them. A Mac mini with the storage runs `flick-serve`; Flick on a MacBook on the same [Tailscale](https://tailscale.com) network pairs with it and streams the original files. Nothing is copied to the MacBook, and nothing is reachable from the Internet or the LAN: the server listens only on its Tailscale address and every request needs the pairing token. [Serving a Flick library](REMOTE.md) explains how to set up the server.

Each paired server has its own section in the library, and its videos play like local ones, with Continue Watching. It has been played from a MacBook at Original and through a transcode tier; see [validation](VALIDATION.md#remote-playback-from-the-macbook-pro-on-2026-11-10).

To pair by pasting, run `flick-serve --print-pairing` on the server and copy the `flick-remote://…` string. In Flick, open **Settings**, find **Remote Libraries**, choose **Add Server…**, paste the string, and choose **Add**. Each paired server has a row with its address and status: Online with its number of videos, Offline, or Wrong token. The row can rename the server, refresh it, change its default quality, turn [automatic quality](#automatic-quality) off, and remove it. The token is saved in `settings.json`; treat the pairing string like a password.

The features below (finding Flicks, pairing codes, automatic quality, Continue Watching across machines, Keep on this Mac, Wake, audio tracks in transcodes, queued downloads, and notifications) are covered by unit and loopback tests, and the server side was checked with `curl` on a real Tailscale address. None of them has been used from the app's own screens or from a second machine yet; see [validation](VALIDATION.md#remote-library-polish-on-2026-11-11).

### Finding Flicks and pairing by code

**Add Server…** and the first setup screen search your tailnet for Flicks. They ask Tailscale for the machines on it and list each one that answers on Flick's port, by its name, with its Tailscale name and address. A Flick already paired says **Paired**; one whose owner has a code open says "showing a pairing code". **Search Again** repeats the search. If nothing answers, the list says that Tailscale must be running on this computer and the other Flick must be sharing its library on Tailscale.

Choose **Pair** beside a Flick, type the six-digit code shown on it, and choose **Pair** again. Spaces and dashes in the code are ignored. On success the server is added like a pasted pairing string. A wrong code shows the server's answer, with how many attempts are left; after five wrong codes, or ten minutes, the code closes and the owner must show a new one.

To show a code on the machine that shares, turn on **Share my library on Tailscale** in **Settings → Remote Libraries** and choose **Show Pairing Code**. The code appears in large digits with its time left, **Copy**, and **New Code**, which replaces it. With `flick-serve`, run `flick-serve pair-code` while it runs, or start it with `--pair-code`; see [pairing](REMOTE.md#pairing).

Pasting the full pairing string still works, and is the way to pair a server on another port or one that discovery does not find.

### Quality

Original sends the file as it is: every audio and subtitle track, HDR, and the full bit rate. Use it at home, or on any direct path with room to spare. The lower tiers are live H.264 transcodes made on the server: 1080p High (12 Mbit/s) for fast fibre upload, 1080p (6 Mbit/s) for ordinary home upload, 720p (3 Mbit/s) for hotel Wi-Fi and relayed paths, and 480p (1.5 Mbit/s) for cellular. A server offers them only when it has FFmpeg. **Quality**, at the top of the player's Audio & subtitles menu, switches tiers for a remote video and reloads at the current position.

Before relying on it:

- **Home upload speed is the limit away from home.** A 4K remux at original quality needs more than most home connections upload.
- **Direct or relayed.** `tailscale ping <server>` shows `via <address>` for a direct path and `via DERP(…)` for a relay. Relayed paths are slower and vary; expect 720p there.
- **FFmpeg on the server** is needed for any tier but Original.
- **Transcodes are lossy.** They tone-map HDR to SDR, carry one audio track at a time, and cannot show bitmap subtitles such as PGS. Original keeps all of these.
- **Keep the server awake.** The Mac mini must stay awake, logged in, and connected to Tailscale. The launchd agent from `scripts/install-flick-serve.sh` runs the server inside `caffeinate`, and the desktop app's own server does the same while **Keep this Mac awake while sharing** is on; see [Wake](#wake) and [keeping the Mac mini awake](REMOTE.md#keep-the-mac-mini-awake-and-reachable).

#### Automatic quality

While a remote video plays on a server that offers transcode tiers, Flick watches for buffering. Two stalls within the first minute on a tier, or one stall longer than five seconds at any time, drop it one tier and reload at the current position, with a message on the video such as "Dropped to 720p after buffering twice." A short stall right after a seek is not counted. It never drops below the lowest tier the server offers, and never raises the tier again.

Choosing a tier yourself in **Quality** turns automatic quality off until that video is closed. To turn it off for a server for good, switch off **Automatic quality** under that server in **Settings → Remote Libraries**. It is on by default, and does nothing for a server without FFmpeg or for a download that has not finished, since those play only at Original.

#### Audio tracks in transcodes

A transcode carries one audio track. When the server has read the file, the player's audio menu lists the file's audio tracks by language, title, and channels (for example "eng · 5.1"), and choosing one reloads the tier at the current position with that track. Without a choice, a transcode uses the file's first audio track. At Original, the menu lists the file's own tracks, as for a local video.

### Continue Watching across machines

Each Flick saves its position in a remote video to the server as it plays, and the server's listing carries the last position any Flick saved there. When a listing arrives, Flick keeps whichever position is newer, its own or the server's. Stop watching on one Mac, and any other Flick paired with the same server resumes from there. A video finished on one machine counts as finished on the others.

### Keep on this Mac

A remote video's menu (right-click its card, or Shift+F10) has **Keep on this Mac** (**Keep on this PC** on Windows, **Keep on this computer** on Linux). It copies the original file to this computer, for watching without the server, such as when travelling. It is offered only while the server answers and the video has finished downloading there.

- The copy goes into Flick's download folder, in a folder named `From <server name>`. Downloads shows it under **Copies to this Mac**, with its progress, speed, and **Cancel**. A copy that fails stays there with its reason, **Retry**, and **Dismiss**; Retry continues from the bytes already copied.
- When the copy finishes, it joins the library as a local video marked "Kept from Mac mini", and the server's card is marked **On this Mac**. It plays from this computer.
- While copying, the server card's menu has **Cancel Copy**. Once kept, it has **Show Local Copy in Folder** and **Remove Local Copy…**, which asks before moving the copy to the Trash, like Delete File. The file on the server is never touched.

### Wake

A server that told this Flick its hardware addresses while it was online, and has stopped answering, gets a **Wake** button: on its row in **Settings → Remote Libraries**, beside its section title in the library, and in its Downloads section. Wake sends a Wake-on-LAN packet and says "Wake packet sent; give it a minute." Then the row updates when the server answers again.

- It works only from the same local network as the sleeping machine. Over Tailscale from elsewhere, the packet cannot reach it.
- The machine must allow waking for network access, and a Mac on Wi-Fi may not wake from a packet. Ethernet is the reliable case.
- A server started with `flick-serve --no-wake-info` does not share its addresses, so no Wake button appears for it.

To avoid needing Wake, keep the sharing machine awake. In the desktop app, **Keep this Mac awake while sharing** in **Settings → Remote Libraries** (macOS only, on by default) stops idle sleep while the server runs; the display can still sleep. The launchd agent for `flick-serve` does the same with `caffeinate`.

### Notifications

Flick shows a system notification when a torrent or link download it is following on another machine finishes ("Sintel finished downloading on Mac mini"), when a kept copy finishes ("… is now on this Mac"), and when queued downloads are sent to a machine that woke up. Flick must be running to notice. Notifications use the system's own tool, `osascript` on macOS and `notify-send` on Linux; Windows shows none yet. They never include paths or tokens.

## Downloading to another machine

A paired machine that allows it can run downloads for you. From the MacBook, start a torrent or a link download on the Mac mini: it uses the Mac mini's disk and connection, and the finished video joins the Mac mini's library, where every paired Flick sees it. The design is in [the remote downloads plan](REMOTE_DOWNLOADS_PLAN.md).

This has been checked with loopback tests and with `curl` against a real Tailscale address. It has not yet been used from a second machine; see [validation](VALIDATION.md#download-to-another-machine-and-onboarding-on-2026-11-10).

**On the machine that keeps the files.** Nothing is accepted until its owner allows it:

- In Flick, open **Settings**, find **Remote Libraries**, turn on **Share my library on Tailscale**, then turn on **Let paired Flicks download to this Mac** in the group below it. Choose the **Downloads folder** (the default is Flick's download folder) and, to let others seed from this machine, add folders under **Folders others may seed from**. Turning the switch off refuses new commands at once.
- With `flick-serve`, pass `--allow-downloads`, and `--download-dir`, `--share-root`, and `--links` as needed. See [accepting downloads](REMOTE.md#accepting-downloads-from-other-flicks).

The owner's own preferences govern what it downloads for others: keep sharing completed downloads, bandwidth limits, and public peer discovery. Another Flick cannot change them, and never chooses a folder; the server saves into its download folder. Link downloads need yt-dlp on that machine.

**Save to.** When a paired machine accepts that kind of download, a **Save to** row appears in the torrent file-selection sheet and the paste-link sheet, and **Seed from** in the Share sheet. It has one segment for this Mac and one for each paired machine, each showing its free space. A machine that cannot be chosen is dimmed and says why: Offline, Connecting…, Pair again, Not accepting downloads, Not accepting links, or Not sharing folders. A sheet opens with the machine chosen last time in this session, else the default from setup, else this Mac. A torrent sent to another machine shows "Finding peers on Mac mini…" with Cancel until the server has the file list.

The command center lists **Download to Mac mini…** for each machine that accepts downloads, and, after you paste a link, **Download to Mac mini** beside the usual download.

**Downloads by machine.** Downloads shows this Mac's section first, then one section per paired machine that accepts downloads or links, by its name. A remote torrent row shows verified progress, download and upload rates, peers, and Checking files, with **Pause** or **Resume**, **Remove · Keep Files**, and **Remove and Delete Files…**. Deleting lists every file the server will delete and needs your confirmation; if the list changed on the server in the meantime, nothing is deleted and the new list is shown. Link downloads on that machine have **Cancel download**, and **Play** when done. **Pause All** and **Resume All** cover every machine. Rows refresh every second while Downloads is open and every minute otherwise. A machine that stops answering keeps its last rows with a line saying so; nothing disappears because a machine is asleep.

**Watch Now before it finishes.** While a remote torrent downloads, its row offers **Watch Now** when the server allows streaming unfinished downloads. The video streams from the server at Original quality, from verified pieces only. A seek into pieces the server does not have yet shows Buffering; the server waits up to 30 seconds for a piece before the player asks again. Lower quality tiers become available once the download completes. When it completes, the row offers **Play**, and the video appears in that machine's library section without a restart.

**Seed from.** Choosing another machine in the Share sheet browses only the folders its owner allowed. **Share This Folder** seeds from there; the magnet link is the server's, and sharing keeps going after this Mac closes.

**Queued for a sleeping machine.** If a torrent or link download sent to another machine gets no answer, because it went to sleep or lost its connection in the meantime, Flick keeps it and says "Mac mini is not answering; queued until it does." Downloads shows it in that machine's section as "Download · Waiting for Mac mini to answer", with **Cancel**. The queue is saved, so it survives quitting Flick. When the machine answers again, Flick sends its queued downloads in order and shows one notification for the batch. A download the machine refuses then is dropped from the queue. Pause, Resume, Remove, and the other commands on existing transfers are never queued.

Limits:

- Only paired machines on your tailnet, with the same pairing token as the library. Nothing is reachable from the Internet or the LAN.
- A download cannot move between machines once started. A machine already shown as offline cannot be chosen under Save to; only a download it did not answer is queued.
- A server looks up at most three torrents and runs at most three link downloads at once; more are refused until one finishes. A lookup waits at most five minutes for peers.
- Unfinished downloads play at Original only; the transcode tiers need the finished file.

## Command center

Press ⌘K on macOS or Ctrl+K elsewhere, from the library or the player. One large field finds library videos and plays them, searches online and lists the results, offers to download a pasted YouTube, X, web, or magnet link, and runs commands such as Open Video, Add Folder, Downloads, Open Settings, Fullscreen, Picture in Picture, and Quit. Arrow keys choose a row, Enter runs it, and Escape or a click outside closes it. The library's search field shows the shortcut; clicking it opens the command center too.

## Keyboard and mouse

Player keys can be changed on the **Keyboard Shortcuts** page, which also lists them. Press `?` to show them in the player, or open the page from the library. Tab, Escape, Enter, number keys, and Ctrl or ⌘ with O, K, comma, L, or Q keep their fixed jobs and can't be reassigned.

| Key | Action |
| --- | --- |
| Space | Play or pause |
| Left / Right | Seek backward or forward 10 seconds |
| Up / Down | Raise or lower volume |
| M | Mute or unmute |
| F | Toggle fullscreen |
| P | Picture in picture |
| , / . | Previous or next frame |
| J / K / L | Slower or back 10 seconds / pause at normal speed / play, then faster |
| 0–9 | Jump to 0–90% of the video |
| Page Up / Page Down | Previous or next chapter |
| B | Set the A–B loop start, then its end, then clear it |
| [ / ] | Move audio 50 ms earlier or later |
| Shift+N / Shift+P | Next or previous episode |
| Escape | Exit fullscreen or close a menu |
| Ctrl+O / Cmd+O | Add videos in the library or open a video in the player |
| Ctrl+K / Cmd+K | Open the command center |
| Ctrl+F / Cmd+F | Focus the library search field |
| Ctrl+, / Cmd+, | Open Settings |
| Ctrl+L / Cmd+L | Load a subtitle file |
| Ctrl+Q / Cmd+Q | Quit Flick |
| ? | Show keyboard shortcuts |

Tab and Shift+Tab move between controls, with a focus ring and the control's tooltip. Enter or Space activates the focused control. Arrow keys move between video cards and list rows, and Shift+F10 or the Menu key opens a video's menu: Play, Show in Folder, Mark as Watched or Unwatched, and Remove from Continue Watching. Right-click opens the same menu. Icon controls have descriptive tooltips.

Click or drag the timeline to seek, or the volume slider to adjust sound. Hover the timeline to preview that moment. Double-click the video to toggle fullscreen. Chapters and an A–B loop are marked on the timeline.

## Appearance

Flick draws its interface in a macOS, Windows 11, or GNOME style and matches the running system by default. Each style has its own window chrome, sidebar, controls, typography, and player: a floating HUD on macOS, a transport bar on Windows, and a rounded OSD on GNOME. Choose the style, a light or dark scheme, and an accent color in the **Appearance** group of **Settings**. The native style uses the platform UI font; other styles use the closest installed font or the bundled Inter. Icons come from the free Hugeicons set. See [asset sources and licenses](../assets/README.md).

**Settings** also sets text size, contrast, and button labels under **Accessibility**, and subtitle size, color, background, and position under **Subtitles**. Auto contrast follows the system's increased-contrast setting on macOS, Windows, and GNOME. Colors meet WCAG AA contrast in every style, scheme, and accent.

## Settings and environment variables

Preferences are saved to `settings.json` in the operating system's per-user application configuration directory. If the file can't be read or parsed, Flick starts with default settings.

| Variable | Effect |
| --- | --- |
| `FLICK_SETTINGS_PATH` | Full path to a settings file to use instead of `settings.json` in the configuration directory. |
| `FLICK_DATA_DIR` | Base folder for torrent state (in `transfers`), remote library listings and subtitles (in `remote`), and cached posters (in `thumbnails`). By default, torrent state and remote library files go in the platform's data directory and posters in its cache directory. `flick-serve` also keeps its token in `remote` here unless given `--state-dir`. |
| `FLICK_DOWNLOAD_DIR` | Folder for link downloads. Default: `Downloads/Flick` in your home folder. |
| `FLICK_YTDLP` | Full path to yt-dlp, checked before `PATH`. |
| `FLICK_OPENSUBTITLES_KEY` | OpenSubtitles API key. It overrides a key entered in Settings and is never written to `settings.json`. |
| `FLICK_MPV_LIBRARY` | Full path to the libmpv shared library, used instead of searching the usual locations. |
| `FLICK_HWDEC` | Set to `no` to turn off hardware decoding. |
| `FLICK_FFMPEG` | Full path to the FFmpeg that the remote library server uses for quality tiers below Original, checked before `PATH`. |
| `FLICK_REMOTE_TOKEN` | Pairing token for remote libraries. It overrides the token of every paired server and is never written to `settings.json`. |


These variables exist for development and testing:

| Variable | Effect |
| --- | --- |
| `FLICK_AUDIO` | Set to `null` to play without an audio device. |
| `FLICK_LOCAL_TRANSFERS` | Set to any value to keep torrent transfers on this computer: Flick listens on `127.0.0.1` over TCP only and turns off public discovery. |
| `FLICK_INITIAL_PEERS` | Comma-separated `ip:port` peers to add at startup. Entries that don't parse are skipped. |
| `FLICK_DOWNLOAD_LIMIT` | Torrent download limit in bytes per second, overriding the saved limit. Zero and values that aren't whole numbers are ignored. |
