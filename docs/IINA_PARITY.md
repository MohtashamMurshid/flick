# IINA feature parity

[IINA](https://iina.io) is the macOS video player most Flick users will compare it with. Both play through mpv, so most of IINA's player features are mpv properties that Flick can reach too; the differences are in the interface and in platform integration. This page compares the two as of IINA 1.5.1 (November 2026), records what this branch added, and orders what is left.

Flick runs on macOS, Windows, and Linux, and IINA only on macOS, so some IINA features have a platform equivalent here rather than a copy. Flick also has things IINA does not: the library with Continue Watching and Shows, torrents with Watch Now, link downloads, the remote library over Tailscale, and the command center.

**Status:** *Have* means Flick already had it. *Added* means this branch added it. *Partial* means a smaller version exists. *Missing* is a gap worth closing. *Later* is out of scope for now, with the reason.

## Playback and navigation

| IINA | Flick | Notes |
| --- | --- | --- |
| Play, pause, seek, frame step | Have | |
| Resume where you left off | Have | Also across machines for remote libraries. |
| Speed slider, 0.25–16× | Partial | Speed button cycles 0.5–2×; J and L shuttle up to 4×. |
| Chapters: markers, next and previous, sidebar list | Partial | Markers and Page Up/Down; no chapter list to pick from. |
| A–B loop | Have | |
| Repeat the file | Added | Shift+L, or the command center. |
| Jump to a time (⌘J) | Added | Type `12:34` or `1:02:03` in the command center. |
| Scroll to seek and change volume | Added | Vertical scrolling changes the volume, horizontal seeks. |
| Configurable seek step; exact or keyframe seeking | Missing | Arrow keys always seek 10 s, exactly. |
| Playlist: auto-add the folder, reorder, shuffle, repeat all | Missing | Shows have Up Next; other folders have nothing queued. |
| Playback history window | Have | The History page. |
| Open and stream a URL, with yt-dlp | Partial | Flick downloads links instead of streaming them. Remote libraries stream. |
| Several player windows | Later | Flick is one window with picture in picture. |

## Video

| IINA | Flick | Notes |
| --- | --- | --- |
| Hardware decoding | Have | `hwdec=auto-copy`. |
| Timeline thumbnail preview | Have | |
| Aspect ratio override | Added | Default, 16:9, 4:3, 16:10, 2.35:1, 1:1. |
| Rotate | Added | A quarter turn at a time, as an FFmpeg filter; see the rotation finding below. |
| Flip and mirror | Added | |
| Deinterlace | Added | |
| Equalizer: brightness, contrast, saturation, gamma, hue | Added | FFmpeg `eq` and `hue` filters, because the software renderer has no equalizer. |
| Crop, including drawing the crop box | Missing | mpv 0.38's `video-crop`, or an FFmpeg crop filter. |
| Custom video filters | Missing | |
| Screenshots | Added | S saves a PNG with subtitles to Pictures/Flick. No clipboard copy or format choice. |
| HDR (EDR) | Later | Needs native GPU presentation, [plan](PLAN.md) item 13. |
| Live Text | Later | macOS Vision on a paused frame. |

## Audio

| IINA | Flick | Notes |
| --- | --- | --- |
| Track selection and audio delay | Have | |
| Load an external audio file | Added | Audio & subtitles › Load Audio…, or the command center. |
| Output device selection | Missing | mpv `audio-device` and `audio-device-list`. |
| Volume above 100% | Missing | mpv `volume-max`. |
| Equalizer with saved presets; audio filters | Missing | |
| ReplayGain, gapless audio, Spatial Audio | Missing | |
| Music mode: audio files, cover art, mini player | Later | Flick's library holds videos only. |

## Subtitles

| IINA | Flick | Notes |
| --- | --- | --- |
| Track selection, loading a file | Have | |
| Online search | Have | OpenSubtitles, with a personal API key. |
| Load matching subtitles automatically | Added | `Film.en.srt` beside `Film.mkv`, and files in a `Subs` or `Subtitles` folder. |
| Subtitle delay | Added | Z and X, in 100 ms steps, and in the Audio & subtitles menu. |
| A second subtitle track | Missing | mpv `secondary-sid`. |
| Size, color, background, position | Have | Settings › Subtitles. |
| Font, outline and shadow, ASS style override, text encoding | Missing | |

## Interface and system integration

| IINA | Flick | Notes |
| --- | --- | --- |
| Fullscreen; double-click for fullscreen | Have | |
| Picture in picture | Have | A floating Flick window, not the system's picture in picture. |
| Keep the display awake while playing | Added | macOS and Windows. Linux is still missing. |
| Media keys and Now Playing in Control Center | Missing | Play/pause keys, AirPods, the lock screen. |
| Menu bar with Playback, Video, Audio, and Subtitles menus | Missing | Flick's macOS menu has only Quit. |
| Rebindable keys | Have | No mpv `input.conf` import or key presets. |
| Inspector: codecs, bitrates, cache, dropped frames | Missing | Flick already reads the codec, decoder, and dropped frames for its logs. |
| Trackpad gestures beyond scrolling: pinch to resize | Missing | |
| Window: float on top, resize to the video's size, snap | Missing | |
| Adjustable controls and sidebars | Partial | Three platform styles, no layout editing. |
| Plugins, custom mpv options, log viewer | Later | Flick runs mpv with `config=no` and its own settings. |
| `iina://` links and browser extension | Later | |
| Translations | Missing | English only. |
| Touch Bar | Later | Apple no longer makes it. |

## Added on this branch

All of this is per video: opening another video turns off repeat and resets the subtitle delay and the Video panel. Reopening the same video, or a quality switch on a remote video, keeps them.

- **Video panel.** A new sliders button beside Audio & subtitles opens it: aspect ratio, a 90° rotate, flip, mirror, deinterlace, and the five color settings in steps of 10, with Reset All. `src/video_adjust.rs` turns them into mpv properties and one `vf` filter chain.
- **Subtitle delay**, mirroring the audio delay: Z and X shortcuts, a row in Audio & subtitles, and a reset.
- **Repeat this video** with Shift+L or the command center. A repeating video never ends, so Up Next does not start.
- **Screenshots** with S. libmpv writes the frame on screen at the video's own size, with subtitles, as `Title at 12.34.png` in Pictures/Flick, numbering a name that is taken.
- **Scrolling on the video** changes the volume (up and down) or seeks (left and right), one percent or second per 5 px, and five per mouse-wheel notch.
- **Go to a time** from the command center.
- **Load Audio…** adds an external audio file as a track.
- **Matching subtitles load automatically** (`sub-auto=fuzzy` and `sub-file-paths`). Posters and timeline previews still skip subtitles.
- **The display stays awake** while a video plays, in the player or picture in picture, and may sleep once it pauses or ends. See below.

## Findings while comparing

**The display could sleep during playback.** mpv's own windows hold off display sleep (`stop-screensaver`), but `vo_libmpv` has no screensaver handling, so Flick has to do it itself. Nothing did, so a film played without touching the mouse ran into the energy-saver timeout. `src/awake.rs` now holds an IOKit `PreventUserIdleDisplaySleep` assertion on macOS and sets `ES_DISPLAY_REQUIRED` on Windows while a video plays. Linux needs a D-Bus screensaver inhibitor that stays open, which needs a D-Bus dependency.

**Rotation does nothing on screen.** `vo_libmpv` declares `VO_CAP_ROTATE90`, so mpv never inserts its own rotation filter, but the software renderer Flick uses (`libmpv_sw.c`) has no rotation code. Flick's Rotate is therefore an FFmpeg `transpose` filter. The same gap probably shows portrait phone videos, which carry rotation metadata, on their side and squeezed into a portrait-shaped area. That is from reading mpv's source, not from playing such a file; check it with a rotated clip before fixing it. The container's rotation is not reliably in `video-dec-params`, so the fix should read the video track's `demux-rotation` from `track-list`, set `video-rotate=no`, and add the matching `transpose` itself.

**The equalizer properties do nothing either.** `brightness`, `contrast`, and the others are applied by mpv's GPU output, not by the software renderer, so the Video panel uses FFmpeg filters. Native GPU presentation ([plan](PLAN.md) item 13) would make both the properties and mpv's own rotation work, and would remove the filters' CPU cost.

## What to add next

In order of value for the effort:

1. **Media keys and Now Playing.** `MPRemoteCommandCenter` and `MPNowPlayingInfoCenter` on macOS, System Media Transport Controls on Windows, and MPRIS on Linux. This is the largest everyday gap with IINA. It needs Objective-C block bindings on macOS and a Mac with Xcode to test.
2. **Rotation metadata for phone videos**, once a rotated clip confirms the finding above.
3. **A queue.** Play the folder's next video after this one, as Shows already do for episodes, with a queue panel, shuffle, and repeat all.
4. **The macOS menu bar**: Playback, Video, Audio, and Subtitles menus with the player's commands and their shortcuts, through GPUI actions.
5. **An inspector panel** with the codec, resolution, frame rate, bitrate, decoder, dropped frames, and cache.
6. **Audio output and loudness**: an output device picker, volume up to 150%, and an equalizer with presets through FFmpeg's `superequalizer`.
7. **A second subtitle track**, and subtitle font and outline settings.
8. **Finer speed and seeking**: a speed menu from 0.25× to 4×, and a configurable seek step.
9. **Crop.**
10. **Linux display inhibition** through the desktop portal or `org.freedesktop.ScreenSaver`.
11. **Window sizing**: float on top, and resize to half, normal, or double the video's size.

HDR, plugins, Live Text, music mode, and the Touch Bar stay out of scope: HDR needs native GPU presentation first, and the others are outside a video library player's core.
