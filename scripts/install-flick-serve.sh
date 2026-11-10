#!/usr/bin/env bash
# Install (or remove) the flick-serve launchd agent for the current user.
# See docs/REMOTE.md.
set -euo pipefail

label='com.flick.serve'
script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_dir="$(cd "$script_dir/.." && pwd)"
template="$repo_dir/packaging/macos/$label.plist"
agents_dir="$HOME/Library/LaunchAgents"
target="$agents_dir/$label.plist"
log_dir="$HOME/Library/Logs/Flick"

usage() {
  cat <<'EOF'
Usage: scripts/install-flick-serve.sh [options] [-- flick-serve arguments...]
       scripts/install-flick-serve.sh --uninstall

Installs ~/Library/LaunchAgents/com.flick.serve.plist so flick-serve starts
at login and restarts if it stops. Without arguments after --, the agent
serves the desktop library (--library).

Options:
  --binary PATH     flick-serve to run (default: flick-serve on PATH, else
                    target/release/flick-serve in this repository)
  --no-caffeinate   do not wrap the server in caffeinate -is, which keeps the
                    Mac from idle sleep while the server runs
  --print           print the generated plist and exit without installing
  --uninstall       stop the agent and remove its plist
  -h, --help        show this help

Examples:
  scripts/install-flick-serve.sh
  scripts/install-flick-serve.sh -- --folder /Volumes/Media/Films --folder /Volumes/Media/Shows
EOF
}

binary=''
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
  launchctl bootout "$domain/$label" 2>/dev/null || true
  rm -f "$target"
  printf 'Removed %s\n' "$target"
  exit 0
fi

if [[ -z "$binary" ]]; then
  if command -v flick-serve >/dev/null 2>&1; then
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
  printf '    <string>%s</string>\n' "$(xml_escape "$binary")"
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
mv "$target.tmp" "$target"
launchctl bootout "$domain/$label" 2>/dev/null || true
launchctl bootstrap "$domain" "$target"
printf 'Installed %s and started %s.\nLogs: %s/flick-serve.log\nPairing string: %s --print-pairing\n' \
  "$target" "$label" "$log_dir" "$binary"
