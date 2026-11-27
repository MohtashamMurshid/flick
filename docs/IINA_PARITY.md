# IINA feature parity

[IINA](https://iina.io) is the macOS video player most Flick users will compare it with. Both play through mpv, so most of IINA's player features are mpv properties that Flick can reach too; the differences are in the interface and in platform integration. This page compares the two as of IINA 1.5.1 (November 2026) and records how Flick covers each feature.

Flick runs on macOS, Windows, and Linux, and IINA only on macOS, so some IINA features have a platform equivalent here rather than a copy. Flick also has things IINA does not: the library with Continue Watching and Shows, torrents with Watch Now, link downloads, the remote library over Tailscale, and the command center.

**Status:** *Have* means Flick has it. *Partial* means a smaller version exists, with what is missing in the notes. *Not planned* gives the reason it is out of reach or out of scope.

## Playback and navigation

| IINA | Flick | Notes |
| --- | --- | --- |
| Play, pause, seek, frame step | Have | |
| Resume where you left off | Have | Also across machines for remote libraries. |
| Speed from 0.25× to 16× | Have | A speed menu, and faster, slower, and normal-speed keys. |
| Chapters: markers, next and previous, a list to pick from | Have | The queue panel lists chapters with their times. |
| A–B loop | Have | |
| Repeat the file | Have | Shift+L, the Playback menu, or the command center. |
| Jump to a time (⌘J) | Have | Type `12:34` or `1:02:03` in the command center, or Playback › Go to Time. |
| Scroll to seek and change volume | Have | Vertical scrolling changes the volume, horizontal seeks. |
| Seek step; exact or keyframe seeking | Have | Settings › Playback: 5, 10, 15, or 30 seconds, and Exact seeking. |
| Playlist: auto-add the folder, reorder, shuffle, repeat all | Have | The queue: a show's episodes or the rest of the folder, Play Next and Add to Queue from the library, move up and down, remove, shuffle, and repeat all. |
| Playback history window | Have | The History page. |
| Open and stream a URL, with yt-dlp | Have | Paste a link in the command center and choose Play without downloading, or open `flick://open?url=…`. |
| `iina://` links and the browser extension | Partial | `flick://open?url=` plays a web link, so a bookmarklet works; there is no browser extension. |
| Several player windows | Not planned | Flick is one window with picture in picture; its library, transfers, and player share that window's state. |

## Video

| IINA | Flick | Notes |
| --- | --- | --- |
| Hardware decoding | Have | `hwdec=auto-copy`. |
| Timeline thumbnail preview | Have | |
| Aspect ratio override | Have | Default, 16:9, 4:3, 16:10, 2.35:1, 1:1. |
| Crop | Partial | Presets to 16:9, 4:3, 2.35:1, 1:1, and 9:16; no crop box to draw. |
| Rotate, flip, and mirror | Have | Rotation metadata, such as a phone video's, is turned upright too; see the findings below. |
| Deinterlace | Have | |
| Equalizer: brightness, contrast, saturation, gamma, hue | Have | FFmpeg `eq` and `hue` filters, because the software renderer has no equalizer. |
| Custom video filters | Have | Settings › Advanced › Video filters, in mpv's syntax. |
| Screenshots: to a file or the clipboard, with or without subtitles, PNG or JPEG | Have | S saves, Shift+S copies; format, subtitles, and folder in Settings › Screenshots. |
| Live Text | Have | Shift+T copies the text macOS's Vision framework finds in the frame, subtitles included. macOS only. |
| Media information (the inspector) | Have | I opens codecs, size, frame rate, bitrates, decoder, dropped frames, cache, and A/V sync. |
| HDR (EDR) | Not planned yet | Needs native GPU presentation, [plan](PLAN.md) item 13; the software renderer outputs 8-bit RGB. |

## Audio

| IINA | Flick | Notes |
| --- | --- | --- |
| Track selection and audio delay | Have | |
| Load an external audio file | Have | Audio & subtitles › Load Audio…, or the command center. |
| Output device selection | Have | Audio & subtitles lists the outputs. |
| Volume above 100% | Have | Settings › Audio › Maximum volume, up to 200%. |
| Equalizer with presets | Have | Ten bands from 31 Hz to 16 kHz in Audio & subtitles › Equalizer, with seven presets. The equalizer is kept between videos. Custom curves are kept as one setting, not as named presets. |
| Audio filters | Have | Settings › Advanced › Audio filters. |
| ReplayGain | Have | Settings › Audio. |
| Spatial Audio | Have | Settings › Audio on macOS plays through mpv's AVFoundation output where this libmpv has it. |
| Gapless audio | Not planned | It joins tracks of an album; Flick plays videos one file at a time. |
| Music mode: audio files, cover art, mini player | Partial | Audio files open in the player, and picture in picture is the small player; the library lists videos only, with no album view. |

## Subtitles

| IINA | Flick | Notes |
| --- | --- | --- |
| Track selection, loading a file | Have | |
| Online search | Have | OpenSubtitles, with a personal API key. |
| Load matching subtitles automatically | Have | `Film.en.srt` beside `Film.mkv`, and files in a `Subs` or `Subtitles` folder. |
| Subtitle delay | Have | Z and X, in 100 ms steps, and in Audio & subtitles. |
| A second subtitle track | Have | Audio & subtitles › Second subtitles. |
| Size, color, background, position | Have | Settings › Subtitles. |
| Font, outline, shadow, ASS style override, text encoding | Have | Settings › Subtitles. |

## Interface and system integration

| IINA | Flick | Notes |
| --- | --- | --- |
| Fullscreen; double-click for fullscreen | Have | |
| Picture in picture | Have | A floating Flick window, not the system's picture in picture. |
| Keep the display awake while playing | Have | macOS (IOKit), Windows (thread execution state), and Linux (`org.freedesktop.ScreenSaver`). |
| Media keys and Now Playing | Have | MediaPlayer's Now Playing on macOS, the System Media Transport Controls on Windows, and MPRIS on Linux. |
| Menu bar with Playback, Video, Audio, and Subtitles menus | Have | macOS. Menu items show no key equivalents, because Flick's player keys are rebindable and handled by the player. |
| Float on top, and float while playing | Have | macOS and Windows; Linux window managers offer no portable way to ask. |
| Half, actual, and double size; fit to screen | Have | The Video menu. |
| Rebindable keys, with mpv's and VLC's keymaps | Have | Keyboard Shortcuts › Keys like: Flick, mpv and IINA, or VLC. No `input.conf` import. |
| Adjustable controls | Have | Settings › Playback › Player controls: floating, bar, or compact, in any interface style. |
| Log viewer | Partial | Settings › Advanced › Show Log opens `flick.log` and libmpv's `mpv.log`; there is no in-app viewer. |
| Custom mpv options | Have | Settings › Advanced. |
| Trackpad gestures beyond scrolling: pinch to resize | Not planned | GPUI 0.2.2 reports no pinch or magnify events. |
| Plugins | Not planned | A plugin system is a scripting runtime and a stable API to keep; custom mpv options and filters cover the common uses. |
| Translations | Not planned yet | Every string in Flick is English, written in code; translating needs a string catalog first. |
| Touch Bar | Not planned | Apple no longer makes Macs with one. |

## What changed in this work

Before this work Flick had the core player, the library, and picture in picture. It added, in order of the tables above: repeat, going to a typed time, scrolling for volume and seeking, the seek step and exact seeking, the queue with chapters, streaming links, `flick://` links, the Video panel (aspect ratio, crop presets, rotation, flip, mirror, deinterlace, color), custom filters, screenshots to a file or the clipboard, Live Text, media information, external audio, output devices, a volume ceiling, the audio equalizer, ReplayGain, Spatial Audio, matching subtitles, subtitle delay, a second subtitle track, subtitle font and styling, keeping the display awake, media keys and Now Playing, the macOS menu bar, float on top, window sizes, keymaps, the controls layout, and log files.

Repeat, the subtitle delay, the second subtitle track, and the Video panel last until another video opens; reopening the same video, or a quality switch on a remote video, keeps them. The equalizer and everything in Settings stay.

## Findings while comparing

**Videos with rotation metadata crashed Flick.** `vo_libmpv` declares `VO_CAP_ROTATE90`, so mpv never inserts its own rotation filter and sizes the picture as if turned, but the software renderer Flick uses (`libmpv_sw.c`) does not rotate. On the first frame of a phone video, libmpv aborted the whole process: `Assertion failed: (x1 <= img->w && y1 <= img->h), function mp_image_crop`. The poster worker hit the same abort when such a video joined the library. Every player now runs with `video-rotate=no`, and Flick turns the picture with a `transpose` filter from the video track's `demux-rotation`, together with the user's own turns. A clip tagged with `ffmpeg -display_rotation:v:0 90` now plays upright as 720×1280.

**The display could sleep during playback.** mpv's own windows hold off display sleep (`stop-screensaver`), but `vo_libmpv` has no screensaver handling, so Flick has to do it itself, and nothing did.

**The equalizer properties do nothing in Flick.** `brightness`, `contrast`, and the others are applied by mpv's GPU output, not by the software renderer, so the Video panel uses FFmpeg filters. Native GPU presentation ([plan](PLAN.md) item 13) would make both the properties and mpv's own rotation work, and would remove the filters' CPU cost.
