#!/usr/bin/env bash
# Install (or remove) the flick-serve launchd agent for the current user.
# See docs/REMOTE.md, "Start at login with launchd".
set -euo pipefail

label='com.flick.serve'
script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_dir="$(cd "$script_dir/.." && pwd)"
template="$repo_dir/packaging/macos/$label.plist"
agents_dir="$HOME/Library/LaunchAgents"
target="$agents_dir/$label.plist"
log_dir="$HOME/Library/Logs/Flick"
log_file="$log_dir/flick-serve.log"
bin_dir="$HOME/.local/bin"

usage() {
  cat <<'EOF'
Usage: scripts/install-flick-serve.sh [options] [-- flick-serve arguments...]
       scripts/install-flick-serve.sh --uninstall

Installs ~/Library/LaunchAgents/com.flick.serve.plist so flick-serve starts
at login and restarts 30 seconds after it stops, then waits for it to answer
`flick-serve status`. Without arguments after --, the agent serves the
desktop library (--library).

The binary is copied to ~/.local/bin/flick-serve first, so rebuilding or
deleting this checkout does not change the running server; run the installer
again to update it.

Options:
  --binary PATH     flick-serve to install (default: target/release/flick-serve
                    in this repository if built, else flick-serve on PATH)
  --no-copy         run the binary where it is instead of copying it
  --no-caffeinate   do not wrap the server in caffeinate -is, which keeps the
                    Mac from idle sleep while the server runs
  --print           print the generated plist and exit without installing
  --uninstall       stop the agent and remove its plist (the copied binary,
                    state folder, and log stay)
  -h, --help        show this help

flick-serve arguments: --library or --folder PATH (repeatable), and any of
--allow-downloads, --download-dir DIR, --share-root DIR, --links,
--download-limit, --upload-limit, --no-seeding, --name, --port, --bind,
--state-dir, --ffmpeg, --max-transcodes, --no-wake-info. Not --pair-code
(stdout is discarded; run `flick-serve pair-code` instead), --print-pairing,
or --new-token (it would revoke every paired Flick at each restart).

Examples:
  scripts/install-flick-serve.sh
  scripts/install-flick-serve.sh -- --folder /Volumes/Media/Films --folder /Volumes/Media/Shows
  scripts/install-flick-serve.sh -- --folder "$HOME/Movies" --allow-downloads \
    --download-dir "$HOME/Movies/Downloads" --share-root "$HOME/Movies" --links
EOF
}

binary=''
copy=true
caffeinate=true
print_only=false
uninstall=false
serve_args=()
while (($#)); do
  case "$1" in
    --binary)
      [[ $# -ge 2 ]] || { usage >&2; exit 2; }
      binary="$2"
      shift 2
      ;;
    --no-copy) copy=false; shift ;;
    --no-caffeinate) caffeinate=false; shift ;;
    --print) print_only=true; shift ;;
    --uninstall) uninstall=true; shift ;;
    -h|--help) usage; exit 0 ;;
    --) shift; serve_args=("$@"); break ;;
    *) printf 'Unknown option: %s\n' "$1" >&2; usage >&2; exit 2 ;;
  esac
done

if [[ "$(uname -s)" != Darwin ]]; then
  printf '%s\n' 'The launchd agent is for macOS. On Linux, run flick-serve from a systemd user unit.' >&2
  exit 1
fi

domain="gui/$(id -u)"

if $uninstall; then
  if launchctl print "$domain/$label" >/dev/null 2>&1; then
    launchctl bootout "$domain/$label" 2>/dev/null || true
    # bootout returns before the process is gone; wait for it to exit.
    for _ in $(seq 1 50); do
      launchctl print "$domain/$label" >/dev/null 2>&1 || break
      sleep 0.2
    done
    printf 'Stopped %s\n' "$label"
  fi
  rm -f "$target"
  printf 'Removed %s\n' "$target"
  exit 0
fi

case "${serve_args[0]-}" in
  status|pair-code|help|-h|--help)
    printf 'flick-serve %s is a one-off command, not a server to install.\n' "${serve_args[0]}" >&2
    exit 2
    ;;
esac
state_dir=''
expect_state_dir=false
for argument in "${serve_args[@]+"${serve_args[@]}"}"; do
  if $expect_state_dir; then
    state_dir="$argument"
    expect_state_dir=false
    continue
  fi
  case "$argument" in
    --pair-code)
      printf '%s\n' 'Leave out --pair-code: the agent discards stdout. Open a code on the running server with: flick-serve pair-code' >&2
      exit 2
      ;;
    --print-pairing|--new-token)
      printf '%s cannot run as a launchd agent. Run flick-serve %s once from a terminal instead.\n' "$argument" "$argument" >&2
      exit 2
      ;;
    --state-dir) expect_state_dir=true ;;
    --state-dir=*) state_dir="${argument#--state-dir=}" ;;
  esac
done

if [[ -z "$binary" ]]; then
  if [[ -x "$repo_dir/target/release/flick-serve" ]]; then
    binary="$repo_dir/target/release/flick-serve"
  elif command -v flick-serve >/dev/null 2>&1; then
    binary="$(command -v flick-serve)"
  else
    binary="$repo_dir/target/release/flick-serve"
  fi
fi
if [[ ! -x "$binary" ]]; then
  printf 'flick-serve not found at %s.\nBuild it first: cargo build --locked --release --no-default-features --bin flick-serve\n' "$binary" >&2
  exit 1
fi
binary="$(cd "$(dirname "$binary")" && pwd)/$(basename "$binary")"
program="$binary"
if $copy; then
  program="$bin_dir/flick-serve"
fi
if ((${#serve_args[@]} == 0)); then
  serve_args=(--library)
fi

xml_escape() {
  local value="$1"
  value="${value//&/&amp;}"
  value="${value//</&lt;}"
  value="${value//>/&gt;}"
  printf '%s' "$value"
}

arguments_file="$(mktemp)"
trap 'rm -f "$arguments_file"' EXIT
{
  if $caffeinate; then
    printf '    <string>/usr/bin/caffeinate</string>\n    <string>-is</string>\n'
  fi
  printf '    <string>%s</string>\n' "$(xml_escape "$program")"
  for argument in "${serve_args[@]}"; do
    printf '    <string>%s</string>\n' "$(xml_escape "$argument")"
  done
} >"$arguments_file"

home_escaped="$(xml_escape "$HOME")"
home_escaped="${home_escaped//\\/\\\\}"
home_escaped="${home_escaped//|/\\|}"
plist="$(sed -e "/<!-- PROGRAM_ARGUMENTS -->/{
r $arguments_file
d
}" -e "s|__HOME__|${home_escaped//&/\\&}|g" "$template")"

if $print_only; then
  printf '%s\n' "$plist"
  exit 0
fi

mkdir -p "$agents_dir" "$log_dir"
printf '%s\n' "$plist" >"$target.tmp"
plutil -lint "$target.tmp" >/dev/null
launchctl bootout "$domain/$label" 2>/dev/null || true
for _ in $(seq 1 50); do
  launchctl print "$domain/$label" >/dev/null 2>&1 || break
  sleep 0.2
done
if $copy && [[ "$binary" != "$program" ]]; then
  mkdir -p "$bin_dir"
  # A new file renamed over the old one, never a running binary rewritten.
  cp "$binary" "$program.tmp"
  chmod 0755 "$program.tmp"
  mv -f "$program.tmp" "$program"
  printf 'Copied %s to %s\n' "$binary" "$program"
fi
mv "$target.tmp" "$target"
launchctl bootstrap "$domain" "$target"
printf 'Installed %s and started %s.\nLogs: %s\n' "$target" "$label" "$log_file"

status_args=(status)
state_hint=''
if [[ -n "$state_dir" ]]; then
  status_args+=(--state-dir "$state_dir")
  state_hint=" --state-dir '$state_dir'"
fi
for _ in $(seq 1 30); do
  if status="$("$program" "${status_args[@]}" 2>/dev/null)"; then
    printf 'Running: %s\n' "$status"
    printf 'Pairing code: %s pair-code%s\nPairing string: %s --print-pairing%s (and the same --port or --bind)\n' \
      "$program" "$state_hint" "$program" "$state_hint"
    exit 0
  fi
  sleep 1
done
printf 'The agent is installed but flick-serve did not answer within 30 seconds; launchd retries every 30 seconds.\nLast log lines (%s):\n' "$log_file" >&2
tail -n 5 "$log_file" >&2 2>/dev/null || true
exit 1
