#!/usr/bin/env bash
# Generate a local fixture from FFmpeg sources. No downloaded media is used.
set -euo pipefail

usage() {
  printf '%s\n' 'Usage: scripts/make-test-video.sh [output.mp4] [--with-subtitles]'
  printf '%s\n' 'Default output: ${TMPDIR:-/tmp}/flick-test.mp4. Existing files are never overwritten.'
}

output=''
with_subtitles=false
for argument in "$@"; do
  case "$argument" in
    --help|-h) usage; exit 0 ;;
    --with-subtitles) with_subtitles=true ;;
    --*) printf 'Unknown option: %s\n' "$argument" >&2; usage >&2; exit 2 ;;
    *)
      if [[ -n "$output" ]]; then
        usage >&2
        exit 2
      fi
      output="$argument"
      ;;
  esac
done

output="${output:-${TMPDIR:-/tmp}/flick-test.mp4}"
subtitle_path="${output%.*}.srt"

if ! command -v ffmpeg >/dev/null 2>&1; then
  printf '%s\n' 'FFmpeg is required, including its libx264 and AAC encoders.' >&2
  exit 1
fi
if [[ -e "$output" ]] || { "$with_subtitles" && [[ -e "$subtitle_path" ]]; }; then
  printf '%s\n' 'A requested output file already exists. Choose a new output path.' >&2
  exit 1
fi

mkdir -p "$(dirname "$output")"
ffmpeg -hide_banner -loglevel warning -nostdin -n \
  -f lavfi -i 'testsrc2=size=1280x720:rate=30:duration=12' \
  -f lavfi -i 'sine=frequency=440:sample_rate=48000:duration=12' \
  -map 0:v:0 -map 1:a:0 -t 12 \
  -c:v libx264 -preset fast -crf 23 -pix_fmt yuv420p \
  -color_primaries bt709 -color_trc bt709 -colorspace bt709 \
  -af 'volume=0.15' -c:a aac -b:a 128k \
  -movflags +faststart -f mp4 "$output"

if "$with_subtitles"; then
  cat > "$subtitle_path" <<'SUBTITLES'
1
00:00:01,000 --> 00:00:04,000
Flick: external subtitles are working.

2
00:00:05,000 --> 00:00:08,000
Pause, seek, and change playback speed.

3
00:00:09,000 --> 00:00:11,500
This clip was generated locally with FFmpeg.
SUBTITLES
  printf 'Subtitles: %s\n' "$subtitle_path"
fi
printf 'Video: %s\n' "$output"
