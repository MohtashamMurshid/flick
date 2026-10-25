#!/usr/bin/env bash
# Local-only sustained playback benchmark. Fixture and build messages go to stderr;
# stdout contains only Flick's final JSON. No downloaded media is used.
set -euo pipefail

usage() {
  printf '%s\n' 'Usage: scripts/benchmark.sh [seconds] [--size WIDTHxHEIGHT]'
  printf '%s\n' 'Defaults: 300 seconds, 3840x2160, 30 fps H.264, 16 ms render tick.'
  printf '%s\n' 'Generates a 12-second clip in a private temporary directory and removes it on exit.'
  printf '%s\n' 'FLICK_AUDIO defaults to null; FLICK_HWDEC and CARGO_TARGET_DIR are respected.'
}

seconds=300
size=3840x2160
seen_seconds=false
seen_size=false
while [[ $# -gt 0 ]]; do
  case "$1" in
    --help|-h) usage; exit 0 ;;
    --size)
      if "$seen_size" || [[ $# -lt 2 ]]; then usage >&2; exit 2; fi
      size="$2"
      seen_size=true
      shift 2
      ;;
    --*) printf 'Unknown option: %s\n' "$1" >&2; usage >&2; exit 2 ;;
    *)
      if "$seen_seconds"; then usage >&2; exit 2; fi
      seconds="$1"
      seen_seconds=true
      shift
      ;;
  esac
done
if [[ ! "$seconds" =~ ^[1-9][0-9]*$ ]] || [[ ! "$size" =~ ^[1-9][0-9]*x[1-9][0-9]*$ ]]; then
  printf '%s\n' 'Seconds and WIDTHxHEIGHT must be positive integers.' >&2
  exit 2
fi
width="${size%x*}"
height="${size#*x}"
if (( width % 2 != 0 || height % 2 != 0 )); then
  printf '%s\n' 'The H.264 yuv420p fixture requires even width and height.' >&2
  exit 2
fi
if ! command -v ffmpeg >/dev/null 2>&1; then
  printf '%s\n' 'FFmpeg with the libx264 encoder is required.' >&2
  exit 1
fi
if ! command -v cargo >/dev/null 2>&1; then
  printf '%s\n' 'Cargo and the repository-pinned Rust toolchain are required.' >&2
  exit 1
fi

repository="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repository"
fixture_dir="$(mktemp -d "${TMPDIR:-/tmp}/flick-benchmark.XXXXXX")"
trap 'rm -rf -- "$fixture_dir"' EXIT
fixture="$fixture_dir/testsrc2-${size}-30fps.mp4"
if [[ -e "$fixture" ]]; then
  printf '%s\n' 'Fixture already exists; refusing to overwrite it.' >&2
  exit 1
fi
printf 'Generating local %s fixture (12 seconds, H.264, 30 fps)…\n' "$size" >&2
ffmpeg -hide_banner -loglevel warning -nostdin -n \
  -f lavfi -i "testsrc2=size=${size}:rate=30:duration=12" \
  -an -c:v libx264 -preset veryfast -crf 23 -threads 4 -pix_fmt yuv420p \
  -color_primaries bt709 -color_trc bt709 -colorspace bt709 \
  -movflags +faststart -f mp4 "$fixture" >&2

# cargo run respects custom target directories, and runs the release desktop
# binary without opening a window. Build/encode CPU is outside the measurement.
FLICK_AUDIO="${FLICK_AUDIO:-null}" cargo run --locked --release --bin flick -- \
  --benchmark "$fixture" --seconds "$seconds" --size "$size" --interval-ms 16
