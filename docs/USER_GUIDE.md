# Flick user guide

This guide covers everything Flick does, screen by screen. For installation, see the [README](../README.md#getting-started).

## The library

Flick opens to your library, with Continue Watching, All Videos, Shows, History, and Downloads. A new library opens to a welcome page with **Open Video…**, **Add Folder…**, and **Paste Link**. Afterward, Flick opens to **Home**, with Continue Watching and Recently Added.

Use **Add Videos…** or **Add Folder…** to build the library, or drop files and folders into it. Media files are read in place. Click a video to play or continue it. The back arrow returns to the library and pauses playback. Passing a local path on the command line opens that video directly.

**Shows** groups episodes named like `Show S01E02`, `Show 1x02`, or `Show/Season 1/02`, and the next episode starts after a short countdown.

Type in the search field to filter local videos. Press Enter for download results, or paste a magnet link or web address to see a download suggestion and press Enter to start it. Search queries TPB, Internet Archive, and YTS independently and reports sources that are unavailable.

The Linux file picker uses the desktop portal. Drag and drop and command-line paths still work when no portal service is running.

## Playback

Flick plays video inside its window with libmpv: play and pause, seeking, volume and mute, playback speed, frame stepping, chapters, an A–B loop, fullscreen, picture in picture, audio and subtitle track selection with audio delay, external subtitle loading, recent files, and resume positions. Playback controls appear after media loads. While a video plays, its controls hide after 2.5 seconds without pointer movement, and subtitles move above the controls whenever they show. Turn hiding off in **Appearance**.

libmpv uses the platform's hardware decoder when available (`hwdec=auto-copy`). Set `FLICK_HWDEC=no` to compare against software decoding. libmpv renders into the frame GPUI uploads. This frame transfer follows the display's physical video bounds with no fixed resolution or pixel-budget cap, and native dimension or allocation failures are reported instead of silently lowering resolution. Efficient native GPU video presentation is a separate milestone; this version makes no claim of being faster than VLC.

The captions icon opens **Audio & subtitles** to select tracks, adjust audio delay, load a subtitle file, or find subtitles on OpenSubtitles.com. Finding subtitles needs a free personal API key from OpenSubtitles, entered in **Appearance** or set with `FLICK_OPENSUBTITLES_KEY`. The key is stored in `settings.json`.

## Torrents and Watch Now

Choose torrent files and a destination; completed, verified videos join the library.

**Watch Now** plays a video while it downloads. It reads only verified pieces and fetches the pieces near the playhead first. Seeking into missing pieces shows Buffering until they arrive.

**Downloads** shows torrent download and upload rates, verification progress, and Pause All / Resume All. Export exact metainfo with **Export .torrent…**. Removing a transfer keeps its files unless you choose **Remove and Delete Files…** and confirm the listed paths.

On first run, Flick asks whether completed downloads keep sharing. Change this, and the live torrent bandwidth limits, in **Appearance → Transfers**. Explicit shares keep sharing regardless of that preference, and all transfers stop when Flick closes. Verified torrent videos save their resume position by infohash and file index, so downloading the same video again into another folder keeps your place.

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

## Command center

Press ⌘K on macOS or Ctrl+K elsewhere, from the library or the player. One large field finds library videos and plays them, searches online and lists the results, offers to download a pasted YouTube, X, web, or magnet link, and runs commands such as Open Video, Add Folder, Downloads, Appearance, Fullscreen, Picture in Picture, and Quit. Arrow keys choose a row, Enter runs it, and Escape or a click outside closes it. The library's search field shows the shortcut; clicking it opens the command center too.

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
| Ctrl+, / Cmd+, | Open Appearance |
| Ctrl+L / Cmd+L | Load a subtitle file |
| Ctrl+Q / Cmd+Q | Quit Flick |
| ? | Show keyboard shortcuts |

Tab and Shift+Tab move between controls, with a focus ring and the control's tooltip. Enter or Space activates the focused control. Arrow keys move between video cards and list rows, and Shift+F10 or the Menu key opens a video's menu: Play, Show in Folder, Mark as Watched or Unwatched, and Remove from Continue Watching. Right-click opens the same menu. Icon controls have descriptive tooltips.

Click or drag the timeline to seek, or the volume slider to adjust sound. Hover the timeline to preview that moment. Double-click the video to toggle fullscreen. Chapters and an A–B loop are marked on the timeline.

## Appearance

Flick draws its interface in a macOS, Windows 11, or GNOME style and matches the running system by default. Each style has its own window chrome, sidebar, controls, typography, and player: a floating HUD on macOS, a transport bar on Windows, and a rounded OSD on GNOME. Choose the style, a light or dark scheme, and an accent color in **Appearance**. The native style uses the platform UI font; other styles use the closest installed font or the bundled Inter. Icons come from the free Hugeicons set. See [asset sources and licenses](../assets/README.md).

**Appearance** also sets text size, contrast, button labels, and subtitle size, color, background, and position. Auto contrast follows the system's increased-contrast setting on macOS, Windows, and GNOME. Colors meet WCAG AA contrast in every style, scheme, and accent.

## Settings and environment variables

Preferences are saved to `settings.json` in the operating system's per-user application configuration directory.

| Variable | Effect |
| --- | --- |
| `FLICK_DOWNLOAD_DIR` | Folder for link downloads. Default: `Downloads/Flick` in your home folder. |
| `FLICK_YTDLP` | Full path to yt-dlp, checked before `PATH`. |
| `FLICK_OPENSUBTITLES_KEY` | OpenSubtitles API key, instead of entering it in Appearance. |
| `FLICK_MPV_LIBRARY` | Full path to the libmpv shared library, used instead of searching the usual locations. |
| `FLICK_HWDEC` | Set to `no` to turn off hardware decoding. |
| `FLICK_AUDIO` | Set to `null` for automated tests without an audio device. |
