#!/usr/bin/env bash
# Build Flick.app with libmpv and its dylib closure bundled, sign it, optionally
# notarize it, and wrap it in a DMG. See docs/PACKAGING.md.
#
# Written for macOS's /bin/bash 3.2: no associative arrays, and no expansion of
# arrays that may be empty. Nothing is downloaded; libmpv must already be
# installed (for example with `brew install mpv`).
set -euo pipefail

script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)
repo_root=$(cd "$script_dir/.." && pwd -P)
packaging_dir="$repo_root/packaging/macos"
libmpv_names='libmpv.2.dylib libmpv.1.dylib libmpv.dylib'

usage() {
  cat <<'USAGE'
Usage: scripts/package-macos.sh [options]
       scripts/package-macos.sh --verify PATH/TO/Flick.app

Builds the release binary, assembles Flick.app with libmpv and every non-system
dylib it needs in Contents/Frameworks, signs everything with the hardened
runtime, optionally notarizes and staples, and creates a compressed DMG.

Options:
  --prefix DIR        Prefix holding lib/libmpv.2.dylib, such as a Homebrew
                      prefix. Default: `brew --prefix`, else /opt/homebrew or
                      /usr/local if libmpv is there. Env: FLICK_MPV_PREFIX.
  --libmpv FILE       Use this libmpv dylib instead of PREFIX/lib.
                      Env: FLICK_MPV_DYLIB.
  --output-dir DIR    Where Flick.app and the DMG go. Default: target/macos.
  --min-macos X.Y     LSMinimumSystemVersion. Default and lower bound: the
                      highest minimum found in the bundled binaries.
  --entitlements FILE Override the entitlements plist (see docs/PACKAGING.md).
  --skip-build        Package the existing release binary instead of running
                      `cargo build --locked --release`.
  --no-dmg            Stop after the signed (and notarized) app.
  --force             Replace an existing Flick.app or DMG in the output dir.
  --dry-run           Validate settings and print the plan and Info.plist
                      without building or touching the output directory.
                      Works without macOS tools.
  --verify APP        Only check an existing bundle: dylib closure, no build
                      prefix references, signatures, and hardened runtime.
  -h, --help          Show this help.

Environment:
  FLICK_CODESIGN_IDENTITY   Signing identity, such as
                            "Developer ID Application: Name (TEAMID)".
                            Unset: ad-hoc signing (-), which cannot be notarized.
  FLICK_CODESIGN_KEYCHAIN   Keychain holding that identity (optional).
  FLICK_BUNDLE_ID           CFBundleIdentifier. Default: app.Flick.Flick,
                            matching the app's settings directory.
  FLICK_BUILD_NUMBER        CFBundleVersion. Default: the Cargo.toml version.
  Notarization runs when exactly one of these credential sets is complete:
  FLICK_NOTARY_KEYCHAIN_PROFILE                 (xcrun notarytool store-credentials)
  FLICK_NOTARY_APPLE_ID, FLICK_NOTARY_TEAM_ID, FLICK_NOTARY_PASSWORD
                                                (app-specific password)
  FLICK_NOTARY_KEY, FLICK_NOTARY_KEY_ID[, FLICK_NOTARY_ISSUER]
                                                (App Store Connect API key)
USAGE
}

log() { printf '==> %s\n' "$*"; }
note() { printf '    %s\n' "$*"; }
die() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}
usage_error() {
  printf 'error: %s\n\n' "$*" >&2
  usage >&2
  exit 2
}

work=''
stage=''
cleanup() {
  if [[ -n $stage && -d $stage ]]; then rm -rf "$stage"; fi
  if [[ -n $work && -d $work ]]; then rm -rf "$work"; fi
}
trap cleanup EXIT

warn() {
  printf 'warning: %s\n' "$*" >&2
  if [[ -n $work ]]; then printf '%s\n' "$*" >> "$work/warnings"; fi
}

# ---------------------------------------------------------------- arguments

dry_run=false
force=false
skip_build=false
make_dmg=true
verify_app=''
prefix=${FLICK_MPV_PREFIX:-}
libmpv=${FLICK_MPV_DYLIB:-}
output_dir=''
min_macos_request=''
entitlements=''

need_value() {
  if [[ $# -lt 2 || -z $2 ]]; then usage_error "$1 needs a value"; fi
}

while [[ $# -gt 0 ]]; do
  case $1 in
    --dry-run) dry_run=true ;;
    --force) force=true ;;
    --skip-build) skip_build=true ;;
    --no-dmg) make_dmg=false ;;
    --prefix) need_value "$@"; prefix=$2; shift ;;
    --prefix=*) prefix=${1#*=} ;;
    --libmpv) need_value "$@"; libmpv=$2; shift ;;
    --libmpv=*) libmpv=${1#*=} ;;
    --output-dir) need_value "$@"; output_dir=$2; shift ;;
    --output-dir=*) output_dir=${1#*=} ;;
    --min-macos) need_value "$@"; min_macos_request=$2; shift ;;
    --min-macos=*) min_macos_request=${1#*=} ;;
    --entitlements) need_value "$@"; entitlements=$2; shift ;;
    --entitlements=*) entitlements=${1#*=} ;;
    --verify) need_value "$@"; verify_app=$2; shift ;;
    --verify=*) verify_app=${1#*=} ;;
    -h|--help) usage; exit 0 ;;
    *) usage_error "unknown argument: $1" ;;
  esac
  shift
done

work=$(mktemp -d "${TMPDIR:-/tmp}/flick-package.XXXXXX")
: > "$work/warnings"

# ---------------------------------------------------------------- helpers

absolute_path() {
  case $1 in
    /*) printf '%s\n' "$1" ;;
    *) printf '%s/%s\n' "$PWD" "$1" ;;
  esac
}

# Follow symlinks to the real file. macOS before 13 has no realpath(1).
real_path() {
  local path=$1 target hops=0
  while [[ -L $path ]]; do
    hops=$((hops + 1))
    if [[ $hops -gt 64 ]]; then
      printf 'error: symlink loop at %s\n' "$1" >&2
      return 1
    fi
    target=$(readlink "$path")
    case $target in
      /*) path=$target ;;
      *) path=$(dirname "$path")/$target ;;
    esac
  done
  printf '%s/%s\n' "$(cd "$(dirname "$path")" && pwd -P)" "$(basename "$path")"
}

# True when $1 >= $2 for dotted numeric versions.
version_at_least() {
  awk -v a="$1" -v b="$2" 'BEGIN {
    n = split(a, x, "."); m = split(b, y, "."); k = n > m ? n : m
    for (i = 1; i <= k; i++) {
      p = (i <= n) ? x[i] + 0 : 0; q = (i <= m) ? y[i] + 0 : 0
      if (p > q) exit 0
      if (p < q) exit 1
    }
    exit 0
  }'
}

is_system_path() {
  case $1 in
    /usr/lib/*|/System/*) return 0 ;;
    *) return 1 ;;
  esac
}

is_macho() {
  case "$(file -b "$1" 2>/dev/null)" in
    *Mach-O*) return 0 ;;
    *) return 1 ;;
  esac
}

# Print one load-command field per line. $2: awk regex of load commands; $3: field.
# Readers in these pipelines consume all input: an early exit would make the
# writer fail with SIGPIPE, which pipefail turns into a silent abort.
macho_field() {
  otool -l "$1" | awk -v want="$2" -v field="$3" '
    $1 == "cmd" { cmd = $2; next }
    $1 == field && cmd ~ want {
      line = $0
      sub("^[[:space:]]*" field "[[:space:]]+", "", line)
      sub("[[:space:]]+[(]offset [0-9]+[)][[:space:]]*$", "", line)
      if (!seen[line]++) print line
    }'
}
macho_deps() { macho_field "$1" '^LC_(LOAD_DYLIB|LOAD_WEAK_DYLIB|REEXPORT_DYLIB|LOAD_UPWARD_DYLIB|LAZY_LOAD_DYLIB)$' name; }
macho_rpaths() { macho_field "$1" '^LC_RPATH$' path; }
macho_id() { macho_field "$1" '^LC_ID_DYLIB$' name | awk 'NR == 1'; }
macho_min_macos() {
  otool -l "$1" | awk '
    $1 == "cmd" { cmd = $2; next }
    !found && cmd == "LC_BUILD_VERSION" && $1 == "minos" { print $2; found = 1 }
    !found && cmd == "LC_VERSION_MIN_MACOSX" && $1 == "version" { print $2; found = 1 }'
}

# Look up column $3 of the first row of TSV file $1 whose first column is $2.
tsv_get() {
  awk -F '\t' -v key="$2" -v column="$3" '$1 == key { print $column; exit }' "$1"
}

lowercase() { printf '%s' "$1" | tr '[:upper:]' '[:lower:]'; }

# ---------------------------------------------------------------- verification

# Check a bundle's dylib closure. Prints problems and returns non-zero if any.
# Callers use `|| die`, which disables set -e here, so check each command.
verify_closure() {
  local app=$1 frameworks="$1/Contents/Frameworks" failures=0 file dep name rpath found_mpv=false
  local deps="$work/verify-deps" rpaths="$work/verify-rpaths" libmpv_name
  [[ -d $app/Contents/MacOS ]] || { printf 'not an app bundle: %s\n' "$app" >&2; return 1; }
  for libmpv_name in $libmpv_names; do
    if [[ -f $frameworks/$libmpv_name ]]; then found_mpv=true; fi
  done
  if ! $found_mpv; then
    printf 'missing: Contents/Frameworks has none of: %s\n' "$libmpv_names"
    failures=$((failures + 1))
  fi
  for file in "$app"/Contents/MacOS/* "$frameworks"/*; do
    [[ -f $file ]] || continue
    is_macho "$file" || continue
    if ! macho_deps "$file" > "$deps" || ! macho_rpaths "$file" > "$rpaths"; then
      printf '%s: otool failed\n' "${file#"$app"/}"
      failures=$((failures + 1))
      continue
    fi
    while IFS= read -r dep; do
      case $dep in
        /usr/lib/*|/System/*) ;;
        @loader_path/*)
          name=${dep#@loader_path/}
          if [[ $name == */* || ! -f $(dirname "$file")/$name ]]; then
            printf '%s: unresolved %s\n' "${file#"$app"/}" "$dep"
            failures=$((failures + 1))
          fi
          ;;
        @executable_path/../Frameworks/*)
          name=${dep#@executable_path/../Frameworks/}
          if [[ $name == */* || ! -f $frameworks/$name ]]; then
            printf '%s: unresolved %s\n' "${file#"$app"/}" "$dep"
            failures=$((failures + 1))
          fi
          ;;
        @rpath/libswift*.dylib)
          # Swift runtime libraries live in the OS; allowed with an OS rpath.
          if ! grep -qx '/usr/lib/swift' "$rpaths"; then
            printf '%s: %s without a /usr/lib/swift rpath\n' "${file#"$app"/}" "$dep"
            failures=$((failures + 1))
          fi
          ;;
        *)
          printf '%s: references %s outside the bundle and the OS\n' "${file#"$app"/}" "$dep"
          failures=$((failures + 1))
          ;;
      esac
    done < "$deps"
    while IFS= read -r rpath; do
      if ! is_system_path "$rpath/"; then
        printf '%s: rpath %s points outside the OS\n' "${file#"$app"/}" "$rpath"
        failures=$((failures + 1))
      fi
    done < "$rpaths"
  done
  # The explicit check the release process asks for: no build-prefix paths.
  if otool -L "$app"/Contents/MacOS/* "$frameworks"/* 2>/dev/null \
    | grep -E '^[[:space:]]+(/opt/homebrew|/usr/local)/' > "$work/verify-prefix"; then
    printf 'otool -L still lists Homebrew paths:\n'
    sed 's/^/  /' "$work/verify-prefix"
    failures=$((failures + 1))
  fi
  [[ $failures -eq 0 ]]
}

verify_signatures() {
  local app=$1 file details flags failures=0
  codesign --verify --deep --strict --verbose=2 "$app" || return 1
  for file in "$app"/Contents/MacOS/* "$app"/Contents/Frameworks/*; do
    [[ -f $file ]] || continue
    is_macho "$file" || continue
    details=$(codesign --display --verbose=2 "$file" 2>&1) || {
      printf '%s: unsigned\n' "${file#"$app"/}"
      failures=$((failures + 1))
      continue
    }
    flags=$(printf '%s\n' "$details" | awk '
      !found && match($0, /flags=0x[0-9a-fA-F]+[(][^)]*[)]/) {
        value = substr($0, RSTART, RLENGTH); sub(/^[^(]*[(]/, "", value); sub(/[)]$/, "", value)
        print value; found = 1
      }')
    case ",$flags," in
      *,runtime,*) ;;
      *)
        printf '%s: hardened runtime is not enabled\n' "${file#"$app"/}"
        failures=$((failures + 1))
        ;;
    esac
  done
  [[ $failures -eq 0 ]]
}

if [[ -n $verify_app ]]; then
  [[ $(uname -s) == Darwin ]] || die "--verify needs macOS (otool and codesign)"
  verify_app=$(absolute_path "$verify_app")
  [[ -d $verify_app ]] || die "no such app: $verify_app"
  log "Checking the dylib closure of $verify_app"
  verify_closure "$verify_app" || die "the bundle still depends on files outside itself and the OS"
  note "Every non-system dependency resolves inside Contents/Frameworks."
  log "Checking signatures and the hardened runtime"
  verify_signatures "$verify_app" || die "signature check failed"
  codesign --display --verbose=2 "$verify_app" 2>&1 | grep -E '^(Identifier|Authority|TeamIdentifier|Signature|CodeDirectory)' | sed 's/^/    /' || true
  printf '\nBundle checks passed: %s\n' "$verify_app"
  exit 0
fi

# ---------------------------------------------------------------- settings

version=$(awk '
  /^\[/ { in_package = ($0 == "[package]"); next }
  in_package && /^version[[:space:]]*=/ {
    sub(/^version[[:space:]]*=[[:space:]]*"/, ""); sub(/".*$/, ""); print; exit
  }' "$repo_root/Cargo.toml")
[[ $version =~ ^[0-9]+\.[0-9]+\.[0-9]+ ]] || die "could not read [package] version from Cargo.toml"
short_version=${version%%[-+]*}
[[ $short_version =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "Cargo.toml version $version has no numeric x.y.z part"
if [[ $short_version != "$version" ]]; then
  warn "CFBundleShortVersionString uses $short_version; macOS allows only numbers, not $version"
fi
build_number=${FLICK_BUILD_NUMBER:-$short_version}
[[ $build_number =~ ^[0-9]+(\.[0-9]+){0,2}$ ]] || die "FLICK_BUILD_NUMBER must be one to three dot-separated integers, not '$build_number'"
bundle_id=${FLICK_BUNDLE_ID:-app.Flick.Flick}
[[ $bundle_id =~ ^[A-Za-z0-9-]+(\.[A-Za-z0-9-]+)+$ ]] || die "FLICK_BUNDLE_ID '$bundle_id' is not a reverse-DNS identifier"
if [[ -n $min_macos_request && ! $min_macos_request =~ ^[0-9]+(\.[0-9]+){0,2}$ ]]; then
  die "--min-macos must look like 12.0, not '$min_macos_request'"
fi

identity=${FLICK_CODESIGN_IDENTITY:--}
adhoc=false
if [[ $identity == - ]]; then adhoc=true; fi
keychain=${FLICK_CODESIGN_KEYCHAIN:-}

# Notarization credentials: exactly one complete set, or none.
notary_mode=none
notary_args=()
sets=0
if [[ -n ${FLICK_NOTARY_KEYCHAIN_PROFILE:-} ]]; then
  sets=$((sets + 1)); notary_mode="keychain-profile"
  notary_args=(--keychain-profile "$FLICK_NOTARY_KEYCHAIN_PROFILE")
fi
if [[ -n ${FLICK_NOTARY_APPLE_ID:-}${FLICK_NOTARY_TEAM_ID:-}${FLICK_NOTARY_PASSWORD:-} ]]; then
  [[ -n ${FLICK_NOTARY_APPLE_ID:-} && -n ${FLICK_NOTARY_TEAM_ID:-} && -n ${FLICK_NOTARY_PASSWORD:-} ]] \
    || die "Apple ID notarization needs FLICK_NOTARY_APPLE_ID, FLICK_NOTARY_TEAM_ID, and FLICK_NOTARY_PASSWORD"
  sets=$((sets + 1)); notary_mode="apple-id"
  notary_args=(--apple-id "$FLICK_NOTARY_APPLE_ID" --team-id "$FLICK_NOTARY_TEAM_ID" --password "$FLICK_NOTARY_PASSWORD")
fi
if [[ -n ${FLICK_NOTARY_KEY:-}${FLICK_NOTARY_KEY_ID:-}${FLICK_NOTARY_ISSUER:-} ]]; then
  [[ -n ${FLICK_NOTARY_KEY:-} && -n ${FLICK_NOTARY_KEY_ID:-} ]] \
    || die "API key notarization needs FLICK_NOTARY_KEY (a .p8 path) and FLICK_NOTARY_KEY_ID"
  sets=$((sets + 1)); notary_mode="api-key"
  notary_args=(--key "$FLICK_NOTARY_KEY" --key-id "$FLICK_NOTARY_KEY_ID")
  if [[ -n ${FLICK_NOTARY_ISSUER:-} ]]; then notary_args+=(--issuer "$FLICK_NOTARY_ISSUER"); fi
fi
[[ $sets -le 1 ]] || die "set only one notarization credential set (keychain profile, Apple ID, or API key)"
if [[ $notary_mode != none ]] && $adhoc; then
  die "notarization needs a Developer ID Application identity; set FLICK_CODESIGN_IDENTITY"
fi

if [[ -z $entitlements ]]; then
  if $adhoc; then
    entitlements=$packaging_dir/Flick-adhoc.entitlements
  else
    entitlements=$packaging_dir/Flick.entitlements
  fi
fi
entitlements=$(absolute_path "$entitlements")
[[ -f $entitlements ]] || die "entitlements file not found: $entitlements"
icon_source=$packaging_dir/Flick-icon-1024.png
[[ -f $icon_source ]] || die "icon source not found: $icon_source"
for notice in "$repo_root/LICENSE" "$repo_root/THIRD_PARTY_NOTICES.md" "$repo_root/assets/fonts/OFL.txt" "$repo_root/assets/icons/LICENSE"; do
  [[ -f $notice ]] || die "notice file not found: $notice"
done

if [[ -n ${CARGO_BUILD_TARGET:-} ]]; then
  die "unset CARGO_BUILD_TARGET; this script packages the host architecture from target/release"
fi
target_dir=$(absolute_path "${CARGO_TARGET_DIR:-$repo_root/target}")
binary=$target_dir/release/flick
output_dir=$(absolute_path "${output_dir:-$repo_root/target/macos}")
arch=$(uname -m)
app_path=$output_dir/Flick.app
dmg_path=$output_dir/Flick-$version-$arch.dmg

existing=''
if [[ -e $app_path ]]; then existing="$existing $app_path"; fi
if $make_dmg && [[ -e $dmg_path ]]; then existing="$existing $dmg_path"; fi
if [[ -n $existing ]] && ! $force && ! $dry_run; then
  die "refusing to overwrite:$existing (pass --force to replace)"
fi

write_info_plist() {
  local min_macos=$1
  cat <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleDevelopmentRegion</key>
	<string>en</string>
	<key>CFBundleDisplayName</key>
	<string>Flick</string>
	<key>CFBundleExecutable</key>
	<string>flick</string>
	<key>CFBundleIconFile</key>
	<string>Flick</string>
	<key>CFBundleIdentifier</key>
	<string>$bundle_id</string>
	<key>CFBundleInfoDictionaryVersion</key>
	<string>6.0</string>
	<key>CFBundleName</key>
	<string>Flick</string>
	<key>CFBundlePackageType</key>
	<string>APPL</string>
	<key>CFBundleShortVersionString</key>
	<string>$short_version</string>
	<key>CFBundleVersion</key>
	<string>$build_number</string>
	<key>LSApplicationCategoryType</key>
	<string>public.app-category.entertainment</string>
	<key>LSMinimumSystemVersion</key>
	<string>$min_macos</string>
	<key>NSHighResolutionCapable</key>
	<true/>
	<key>NSPrincipalClass</key>
	<string>NSApplication</string>
	<key>NSSupportsAutomaticGraphicsSwitching</key>
	<true/>
	<key>NSDesktopFolderUsageDescription</key>
	<string>Flick plays and shares the videos you choose.</string>
	<key>NSDocumentsFolderUsageDescription</key>
	<string>Flick plays and shares the videos you choose.</string>
	<key>NSDownloadsFolderUsageDescription</key>
	<string>Flick saves downloads to your Downloads folder and plays them.</string>
	<key>NSRemovableVolumesUsageDescription</key>
	<string>Flick plays and shares the videos you choose.</string>
	<key>NSNetworkVolumesUsageDescription</key>
	<string>Flick plays and shares the videos you choose.</string>
	<key>NSLocalNetworkUsageDescription</key>
	<string>Flick connects to peers and forwards its sharing port on your local network.</string>
	<key>CFBundleDocumentTypes</key>
	<array>
		<dict>
			<key>CFBundleTypeName</key>
			<string>Video</string>
			<key>CFBundleTypeRole</key>
			<string>Viewer</string>
			<key>LSHandlerRank</key>
			<string>Alternate</string>
			<key>LSItemContentTypes</key>
			<array>
				<string>public.movie</string>
				<string>public.mpeg-4</string>
				<string>com.apple.quicktime-movie</string>
				<string>com.apple.m4v-video</string>
				<string>public.avi</string>
				<string>public.mpeg</string>
				<string>public.mpeg-2-transport-stream</string>
				<string>public.avchd-mpeg-2-transport-stream</string>
				<string>com.microsoft.windows-media-wmv</string>
				<string>public.3gpp</string>
			</array>
		</dict>
		<dict>
			<key>CFBundleTypeName</key>
			<string>Video</string>
			<key>CFBundleTypeRole</key>
			<string>Viewer</string>
			<key>LSHandlerRank</key>
			<string>Alternate</string>
			<key>CFBundleTypeExtensions</key>
			<array>
				<string>mkv</string>
				<string>webm</string>
				<string>flv</string>
				<string>ogv</string>
				<string>vob</string>
				<string>m2ts</string>
				<string>mts</string>
				<string>ts</string>
			</array>
		</dict>
		<dict>
			<key>CFBundleTypeName</key>
			<string>BitTorrent file</string>
			<key>CFBundleTypeRole</key>
			<string>Viewer</string>
			<key>LSHandlerRank</key>
			<string>Alternate</string>
			<key>CFBundleTypeExtensions</key>
			<array>
				<string>torrent</string>
			</array>
			<key>CFBundleTypeMIMETypes</key>
			<array>
				<string>application/x-bittorrent</string>
			</array>
		</dict>
	</array>
</dict>
</plist>
PLIST
}

identity_label() {
  if $adhoc; then printf 'ad-hoc (-); runs only where Gatekeeper is bypassed, cannot be notarized'; else printf '%s' "$identity"; fi
}

# ---------------------------------------------------------------- dry run

if $dry_run; then
  if [[ -z $prefix ]]; then
    prefix_text='detected at packaging time: brew --prefix, else /opt/homebrew or /usr/local'
  else
    prefix_text=$prefix
  fi
  cat <<PLAN
Dry run: nothing is built, copied, signed, or written to $output_dir.

Settings
  version          $version (CFBundleShortVersionString $short_version, CFBundleVersion $build_number)
  bundle id        $bundle_id
  architecture     $arch (host; Homebrew libraries are single-architecture)
  libmpv           ${libmpv:-PREFIX/lib/ one of: $libmpv_names}
  prefix           $prefix_text
  signing          $(identity_label)
  entitlements     $entitlements
  notarization     $notary_mode
  app              $app_path$( [[ -e $app_path ]] && printf ' (exists; --force replaces it)')
  dmg              $( $make_dmg && printf '%s' "$dmg_path" || printf 'skipped (--no-dmg)')$( $make_dmg && [[ -e $dmg_path ]] && printf ' (exists; --force replaces it)')

Plan
  1. $( $skip_build && printf 'Use the existing %s' "$binary" || printf 'cargo build --locked --release (binary: %s)' "$binary")
  2. Stage Flick.app in a hidden directory inside the output directory.
  3. Copy the binary to Contents/MacOS/flick and libmpv to Contents/Frameworks.
  4. Walk LC_LOAD_DYLIB, LC_LOAD_WEAK_DYLIB, LC_REEXPORT_DYLIB, and LC_RPATH with
     otool -l, starting from libmpv and the executable, and copy every non-system
     dylib (anything outside /usr/lib and /System) into Contents/Frameworks.
  5. install_name_tool: set each dylib id to @rpath/NAME, point every bundled
     dependency at @loader_path/NAME (@executable_path/../Frameworks/NAME from the
     executable), and delete rpaths outside the OS.
  6. Check that every binary's architectures include $arch, and set
     LSMinimumSystemVersion to the highest minimum macOS in the bundle$( [[ -n $min_macos_request ]] && printf ' (requested: %s)' "$min_macos_request").
  7. Verify the closure: no dependency outside the bundle or the OS, no rpath
     outside the OS, and no /opt/homebrew or /usr/local path in otool -L.
  8. Write Info.plist (below), PkgInfo, and Flick.icns (sips and iconutil from
     $icon_source).
  9. Copy Flick's license and notices, and each bundled library's license files
     from its Homebrew keg, into Contents/Resources/Licenses, with MANIFEST.tsv
     recording formula, version, declared license, and source URL.
 10. xattr -cr, then codesign --options runtime each dylib, then the app with the
     entitlements; codesign --verify --deep --strict.
 11. $( [[ $notary_mode != none ]] && printf 'Notarize the app (xcrun notarytool submit --wait, %s), staple and validate it.' "$notary_mode" || printf 'Skip notarization (no credentials set).')
 12. $( $make_dmg && printf 'hdiutil create -format UDZO with Flick.app and an /Applications link%s' "$( [[ $notary_mode != none ]] && printf '; sign, notarize, and staple the DMG')" || printf 'Skip the DMG.')
 13. Move the results into place and print a summary.

Tools
PLAN
  for tool in uname file otool install_name_tool codesign lipo plutil sips iconutil xattr hdiutil ditto xcrun cargo; do
    if command -v "$tool" > /dev/null 2>&1; then
      printf '  %-18s found\n' "$tool"
    else
      printf '  %-18s missing (needed on macOS)\n' "$tool"
    fi
  done
  printf '\n--- Info.plist (LSMinimumSystemVersion shown as %s; computed when packaging) ---\n' "${min_macos_request:-11.0}"
  write_info_plist "${min_macos_request:-11.0}"
  printf -- '--- end Info.plist ---\n'
  exit 0
fi

# ---------------------------------------------------------------- preflight

[[ $(uname -s) == Darwin ]] || die "packaging needs macOS; use --dry-run elsewhere"
missing=''
for tool in file otool install_name_tool codesign lipo plutil sips iconutil xattr; do
  command -v "$tool" > /dev/null 2>&1 || missing="$missing $tool"
done
if $make_dmg; then command -v hdiutil > /dev/null 2>&1 || missing="$missing hdiutil"; fi
if [[ $notary_mode != none ]]; then
  for tool in xcrun ditto; do command -v "$tool" > /dev/null 2>&1 || missing="$missing $tool"; done
fi
if ! $skip_build; then command -v cargo > /dev/null 2>&1 || missing="$missing cargo"; fi
[[ -z $missing ]] || die "missing tools:$missing (install Xcode or the Command Line Tools)"
if ! $adhoc; then
  if [[ -n $keychain ]]; then
    found_identity=$(security find-identity -v -p codesigning "$keychain" || true)
  else
    found_identity=$(security find-identity -v -p codesigning || true)
  fi
  case $found_identity in
    *"$identity"*) ;;
    *) die "signing identity not found in the keychain: $identity" ;;
  esac
fi

if [[ -z $prefix ]]; then
  if command -v brew > /dev/null 2>&1; then
    prefix=$(HOMEBREW_NO_AUTO_UPDATE=1 brew --prefix)
  else
    for candidate in /opt/homebrew /usr/local; do
      if [[ -e $candidate/lib/libmpv.2.dylib ]]; then prefix=$candidate; break; fi
    done
  fi
fi
if [[ -z $libmpv ]]; then
  [[ -n $prefix ]] || die "no libmpv found. Install it (brew install mpv) or pass --prefix or --libmpv. This script does not download it."
  for name in $libmpv_names; do
    if [[ -e $prefix/lib/$name ]]; then libmpv=$prefix/lib/$name; break; fi
  done
  [[ -n $libmpv ]] || die "no libmpv in $prefix/lib. Install it (brew install mpv) or pass --libmpv. This script does not download it."
fi
libmpv=$(absolute_path "$libmpv")
[[ -f $libmpv ]] || die "libmpv not found: $libmpv"
libmpv_real=$(real_path "$libmpv")
is_macho "$libmpv_real" || die "not a Mach-O library: $libmpv_real"
if [[ -n $prefix ]]; then prefix=$(cd "$prefix" && pwd -P); fi
cellar=''
if [[ -n $prefix && -d $prefix/Cellar ]]; then cellar=$prefix/Cellar; fi

log "Packaging Flick $version for $arch"
note "libmpv: $libmpv -> $libmpv_real"
note "signing: $(identity_label)"

# ---------------------------------------------------------------- build

if $skip_build; then
  log "Using the existing release binary"
else
  log "Building the release binary"
  cargo build --locked --release --manifest-path "$repo_root/Cargo.toml"
fi
[[ -f $binary ]] || die "release binary not found: $binary"
is_macho "$binary" || die "not a Mach-O executable: $binary"
binary_archs=$(lipo -archs "$binary")
case " $binary_archs " in
  *" $arch "*) ;;
  *) die "$binary is built for '$binary_archs', not the host architecture $arch" ;;
esac

# ---------------------------------------------------------------- assemble

mkdir -p "$output_dir"
stage=$(mktemp -d "$output_dir/.flick-package.XXXXXX")
app=$stage/Flick.app
contents=$app/Contents
frameworks=$contents/Frameworks
resources=$contents/Resources
mkdir -p "$contents/MacOS" "$frameworks" "$resources"
cp "$binary" "$contents/MacOS/flick"
chmod 755 "$contents/MacOS/flick"

log "Collecting the dylib closure"
libs=$work/libs.tsv     # real path -> bundled file name
edges=$work/edges.tsv   # referencing real path -> install name -> referenced real path
: > "$libs"
: > "$edges"

# Search order for @rpath: the referencing library's rpaths, then libmpv's,
# because dyld also consults the rpaths of the images that loaded it.
expand_rpaths() {
  local file=$1 dir rpath
  dir=$(dirname "$file")
  macho_rpaths "$file" > "$work/rpaths-raw" || return 1
  while IFS= read -r rpath; do
    case $rpath in
      @loader_path) printf '%s\n' "$dir" ;;
      @loader_path/*) printf '%s/%s\n' "$dir" "${rpath#@loader_path/}" ;;
      @executable_path*) ;; # unknown for a library loaded with dlopen
      *) printf '%s\n' "$rpath" ;;
    esac
  done < "$work/rpaths-raw"
}
expand_rpaths "$libmpv_real" > "$work/libmpv-rpaths"

# Print the real path of a dependency, or "system".
resolve_dependency() {
  local from=$1 dep=$2 dir rest rpath candidate='' system_rpath=false
  dir=$(dirname "$from")
  case $dep in
    /usr/lib/*|/System/*) printf 'system\n'; return 0 ;;
    @loader_path/*) candidate=$dir/${dep#@loader_path/} ;;
    @rpath/*)
      rest=${dep#@rpath/}
      { expand_rpaths "$from"; cat "$work/libmpv-rpaths"; } > "$work/search-rpaths"
      while IFS= read -r rpath; do
        if is_system_path "$rpath/"; then
          system_rpath=true
        elif [[ -e $rpath/$rest ]]; then
          candidate=$rpath/$rest
          break
        fi
      done < "$work/search-rpaths"
      if [[ -z $candidate ]]; then
        if $system_rpath; then printf 'system\n'; return 0; fi
        if [[ -n $prefix && -e $prefix/lib/$rest ]]; then
          candidate=$prefix/lib/$rest
          warn "$from: resolved $dep through $prefix/lib, not an rpath"
        else
          printf 'error: %s: cannot resolve %s through its rpaths\n' "$from" "$dep" >&2
          return 1
        fi
      fi
      ;;
    @executable_path/*)
      printf 'error: %s: %s depends on the loading executable; not supported\n' "$from" "$dep" >&2
      return 1
      ;;
    /*) candidate=$dep ;;
    *)
      printf 'error: %s: relative install name %s\n' "$from" "$dep" >&2
      return 1
      ;;
  esac
  if [[ ! -e $candidate ]]; then
    printf 'error: %s needs %s, which is missing (%s)\n' "$from" "$dep" "$candidate" >&2
    return 1
  fi
  real_path "$candidate"
}

add_library() {
  local real=$1 id name other
  if [[ -n $(tsv_get "$libs" "$real" 2) ]]; then return 0; fi
  is_macho "$real" || die "not a Mach-O library: $real"
  id=$(macho_id "$real")
  name=$(basename "${id:-$real}")
  case $name in
    *.dylib|*.so) ;;
    *) name=$name.dylib ;; # framework binaries such as Python
  esac
  other=$(awk -F '\t' -v name="$name" '$2 == name { print $1; exit }' "$libs")
  if [[ -n $other ]]; then
    die "two different libraries would both be Contents/Frameworks/$name: $other and $real"
  fi
  printf '%s\t%s\n' "$real" "$name" >> "$libs"
}

collect_dependencies() {
  local from=$1 dep target
  macho_deps "$from" > "$work/deps-current" || die "otool failed on $from"
  while IFS= read -r dep; do
    target=$(resolve_dependency "$from" "$dep") || die "could not resolve the dylib closure"
    [[ $target == system ]] && continue
    case $target in
      /usr/lib/*|/System/*) continue ;;
    esac
    add_library "$target"
    printf '%s\t%s\t%s\n' "$from" "$dep" "$target" >> "$edges"
  done < "$work/deps-current"
}

executable_real=$(real_path "$binary")
add_library "$libmpv_real"
collect_dependencies "$executable_real"
tab=$(printf '\t')
index=1
while :; do
  row=$(sed -n "${index}p" "$libs")
  [[ -n $row ]] || break
  collect_dependencies "${row%%"$tab"*}"
  index=$((index + 1))
done
library_count=$(wc -l < "$libs" | tr -d ' ')
note "$library_count dylibs, including libmpv"

log "Copying and relinking"
while IFS=$'\t' read -r real name; do
  cp "$real" "$frameworks/$name"
  chmod 644 "$frameworks/$name"
done < "$libs"

relink() {
  local file=$1 from=$2 prefix_ref=$3 own_name=$4 origin dep target name rpath attempts changes=0
  local -a args
  args=(-id "@rpath/${own_name:-unused}")
  if [[ -n $own_name ]]; then changes=1; fi
  while IFS=$'\t' read -r origin dep target; do
    [[ $origin == "$from" ]] || continue
    name=$(tsv_get "$libs" "$target" 2)
    args+=(-change "$dep" "$prefix_ref$name")
    changes=$((changes + 1))
  done < "$edges"
  # The executable has no install name; drop the placeholder -id pair.
  if [[ -z $own_name ]]; then args=("${args[@]:2}"); fi
  if [[ $changes -gt 0 ]]; then
    install_name_tool "${args[@]}" "$file" 2> "$work/install-name-tool.log" || {
      cat "$work/install-name-tool.log" >&2
      die "install_name_tool failed on $file"
    }
    # Re-signing comes later; only report unexpected messages.
    grep -v 'invalidate the code signature' "$work/install-name-tool.log" >&2 || true
  fi
  macho_rpaths "$file" > "$work/relink-rpaths" || die "otool failed on $file"
  while IFS= read -r rpath; do
    is_system_path "$rpath/" && continue
    attempts=0
    while macho_rpaths "$file" > "$work/relink-current" && grep -qxF -- "$rpath" "$work/relink-current"; do
      attempts=$((attempts + 1))
      [[ $attempts -le 8 ]] || die "could not delete rpath $rpath from $file"
      install_name_tool -delete_rpath "$rpath" "$file" 2> "$work/install-name-tool.log" || {
        cat "$work/install-name-tool.log" >&2
        die "could not delete rpath $rpath from $file"
      }
    done
  done < "$work/relink-rpaths"
}

while IFS=$'\t' read -r real name; do
  relink "$frameworks/$name" "$real" '@loader_path/' "$name"
done < "$libs"
relink "$contents/MacOS/flick" "$executable_real" '@executable_path/../Frameworks/' ''

log "Checking architectures and minimum macOS"
min_macos=0
min_source=''
for file in "$contents/MacOS/flick" "$frameworks"/*; do
  archs=$(lipo -archs "$file")
  case " $archs " in
    *" $arch "*) ;;
    *) die "$(basename "$file") is built for '$archs', not $arch" ;;
  esac
  file_min=$(macho_min_macos "$file") || die "otool failed on $file"
  if [[ -n $file_min ]] && ! version_at_least "$min_macos" "$file_min"; then
    min_macos=$file_min
    min_source=$(basename "$file")
  fi
done
[[ $min_macos != 0 ]] || die "no minimum macOS version found in the bundled binaries"
if [[ -n $min_macos_request ]]; then
  version_at_least "$min_macos_request" "$min_macos" \
    || die "--min-macos $min_macos_request is lower than $min_macos, which $min_source requires"
  min_macos=$min_macos_request
fi
note "LSMinimumSystemVersion $min_macos (highest requirement: $min_source)"

log "Verifying the closure"
verify_closure "$app" || die "the bundle still depends on files outside itself and the OS"
note "Every non-system dependency resolves inside Contents/Frameworks."
grep -laF -- "${prefix:-/opt/homebrew}/" "$frameworks"/* > "$work/embedded-prefix" 2>/dev/null || true
if [[ -s $work/embedded-prefix ]]; then
  warn "$(wc -l < "$work/embedded-prefix" | tr -d ' ') bundled libraries contain ${prefix:-/opt/homebrew} as a string (data or plugin paths such as fontconfig configuration); those features look outside the app at run time: $(sed 's#.*/##' "$work/embedded-prefix" | tr '\n' ' ')"
fi

# ---------------------------------------------------------------- metadata

log "Writing Info.plist, PkgInfo, and the icon"
write_info_plist "$min_macos" > "$contents/Info.plist"
plutil -lint "$contents/Info.plist" > /dev/null
printf 'APPL????' > "$contents/PkgInfo"
iconset=$work/Flick.iconset
mkdir -p "$iconset"
for size in 16 32 128 256 512; do
  sips -z "$size" "$size" "$icon_source" --out "$iconset/icon_${size}x${size}.png" > /dev/null
  sips -z $((size * 2)) $((size * 2)) "$icon_source" --out "$iconset/icon_${size}x${size}@2x.png" > /dev/null
done
iconutil --convert icns --output "$resources/Flick.icns" "$iconset"

# ---------------------------------------------------------------- licenses

log "Copying license texts"
licenses=$resources/Licenses
libraries_dir=$licenses/bundled-libraries
mkdir -p "$libraries_dir"
cp "$repo_root/LICENSE" "$licenses/Flick-LICENSE.txt"
cp "$repo_root/THIRD_PARTY_NOTICES.md" "$licenses/Flick-THIRD_PARTY_NOTICES.md"
cp "$repo_root/assets/fonts/OFL.txt" "$licenses/Inter-OFL.txt"
cp "$repo_root/assets/icons/LICENSE" "$licenses/Hugeicons-LICENSE.txt"
manifest=$libraries_dir/MANIFEST.tsv
printf 'library\tformula\tversion\tdeclared_license\tsource_url\tlicense_files\n' > "$manifest"
kegs=$work/kegs.tsv # keg -> formula, version, license, url, files
: > "$kegs"

formula_license() {
  awk '
    !found && /^[[:space:]]*license[[:space:](]/ { found = 1 }
    found {
      line = $0; sub(/^[[:space:]]+/, "", line); sub(/[[:space:]]+$/, "", line)
      text = text (text == "" ? "" : " ") line
      depth += gsub(/\[/, "[", line) - gsub(/\]/, "]", line)
      if (depth <= 0 && text !~ /,$/) { sub(/^license[[:space:](]+/, "", text); print text; exit }
    }' "$1"
}

describe_keg() {
  local keg=$1 formula=$2 keg_version=$3 rb license='' url='' files='' file base first target
  rb=$keg/.brew/$formula.rb
  if [[ -f $rb ]]; then
    license=$(formula_license "$rb")
    url=$(awk '/^[[:space:]]*url[[:space:]]+"/ { sub(/^[[:space:]]*url[[:space:]]+"/, ""); sub(/".*/, ""); print; exit }' "$rb")
  fi
  if [[ -z $license && -f $keg/sbom.spdx.json ]]; then
    license=$(awk '
      !found && /"licenseDeclared"[[:space:]]*:/ {
        value = $0; sub(/.*"licenseDeclared"[[:space:]]*:[[:space:]]*"/, "", value); sub(/".*/, "", value)
        print value; found = 1
      }' "$keg/sbom.spdx.json")
  fi
  target=$libraries_dir/$formula-$keg_version
  mkdir -p "$target"
  for file in "$keg"/* "$keg"/share/doc/*/* "$keg"/share/licenses/*/*; do
    [[ -f $file ]] || continue
    base=$(basename "$file")
    first=$(lowercase "${base%%[.-]*}")
    case $first in
      copying|copyright|license|licence|notice|patents|unlicense) ;;
      *) continue ;;
    esac
    if [[ -e $target/$base ]]; then base=$(basename "$(dirname "$file")")-$base; fi
    cp "$file" "$target/$base"
    files="$files${files:+ }$base"
  done
  if [[ -f $keg/sbom.spdx.json ]]; then cp "$keg/sbom.spdx.json" "$target/sbom.spdx.json"; fi
  if [[ -z $files ]]; then
    rmdir "$target" 2>/dev/null || true
    warn "no license file found in the $formula $keg_version keg; add its license text before distributing"
  fi
  if [[ -z $license ]]; then
    warn "no declared license found for $formula $keg_version"
  fi
  printf '%s\t%s\t%s\t%s\t%s\t%s\n' "$keg" "$formula" "$keg_version" "${license:-unknown}" "${url:-unknown}" "${files:-NONE FOUND}" >> "$kegs"
}

while IFS=$'\t' read -r real name; do
  formula=unknown keg_version=unknown keg=''
  if [[ -n $cellar ]]; then
    case $real in
      "$cellar"/*/*/*)
        relative=${real#"$cellar"/}
        formula=${relative%%/*}
        relative=${relative#*/}
        keg_version=${relative%%/*}
        keg=$cellar/$formula/$keg_version
        ;;
    esac
  fi
  if [[ -z $keg ]]; then
    warn "$name ($real) is not from a Homebrew keg; record its license and source by hand"
    printf '%s\tunknown\tunknown\tunknown\tunknown\tNONE FOUND\n' "$name" >> "$manifest"
    continue
  fi
  if [[ -z $(tsv_get "$kegs" "$keg" 2) ]]; then describe_keg "$keg" "$formula" "$keg_version"; fi
  awk -F '\t' -v keg="$keg" -v name="$name" 'BEGIN { OFS = "\t" } $1 == keg { print name, $2, $3, $4, $5, $6; exit }' "$kegs" >> "$manifest"
done < "$libs"

gpl_formulas=$(awk -F '\t' 'NR > 1 && $4 ~ /(^|[^L])GPL/ { print $2 }' "$manifest" | sort -u | tr '\n' ' ')
cat > "$licenses/README.txt" <<README
Flick $version for macOS ($arch)

Flick is released under the MIT License (Flick-LICENSE.txt). Its third-party
notices are in Flick-THIRD_PARTY_NOTICES.md, with the Inter font license
(Inter-OFL.txt) and the Hugeicons license (Hugeicons-LICENSE.txt).

Contents/Frameworks holds libmpv and the libraries it needs, copied unmodified
except for install names and code signatures from $([[ -n $cellar ]] && printf 'the Homebrew installation in %s' "$prefix" || printf '%s' "$(dirname "$libmpv_real")").
bundled-libraries/MANIFEST.tsv lists each library with its Homebrew formula,
version, the license its formula declares, and its source URL. License files
found in each formula's installation are in bundled-libraries/FORMULA-VERSION/.
Declared licenses come from the installed formulae and were not checked against
the build configuration of each library.

Libraries whose formulae declare GPL terms: ${gpl_formulas:-none}

Source code for these libraries is available from the source URLs in the
manifest. See docs/PACKAGING.md in Flick's repository for the release
obligations that remain open.
README

# ---------------------------------------------------------------- sign

log "Signing with the hardened runtime"
sign_args=(--force --sign "$identity" --options runtime)
if $adhoc; then sign_args+=(--timestamp=none); else sign_args+=(--timestamp); fi
if [[ -n $keychain ]]; then sign_args+=(--keychain "$keychain"); fi
xattr -cr "$app"
for file in "$frameworks"/*; do
  codesign "${sign_args[@]}" "$file"
done
codesign "${sign_args[@]}" --entitlements "$entitlements" "$app"
verify_signatures "$app" || die "signature verification failed"
note "codesign --verify --deep --strict passed; every binary has the hardened runtime"

submit_for_notarization() {
  local file=$1 log_file id
  log_file=$work/notary-$(basename "$file").log
  log "Notarizing $(basename "$file") (waits for Apple)"
  if ! xcrun notarytool submit "$file" "${notary_args[@]}" --wait 2>&1 | tee "$log_file"; then
    warn "notarytool exited with an error"
  fi
  if ! grep -q 'status: Accepted' "$log_file"; then
    id=$(awk '$1 == "id:" { print $2; exit }' "$log_file")
    if [[ -n $id ]]; then
      xcrun notarytool log "$id" "${notary_args[@]}" >&2 || true
    fi
    die "notarization of $(basename "$file") was not accepted${id:+ (submission $id)}"
  fi
}
staple() {
  xcrun stapler staple "$1"
  xcrun stapler validate "$1"
}

notarized=no
if [[ $notary_mode != none ]]; then
  ditto -c -k --keepParent "$app" "$work/Flick.zip"
  submit_for_notarization "$work/Flick.zip"
  # A zip cannot hold a ticket; staple the app that was submitted inside it.
  staple "$app"
  spctl --assess --type execute --verbose=2 "$app"
  notarized=app
fi

# ---------------------------------------------------------------- dmg

staged_dmg=''
if $make_dmg; then
  log "Creating the DMG"
  dmg_root=$stage/dmg-root
  mkdir -p "$dmg_root"
  ditto "$app" "$dmg_root/Flick.app"
  ln -s /Applications "$dmg_root/Applications"
  staged_dmg=$stage/$(basename "$dmg_path")
  attempt=1
  # hdiutil occasionally reports "Resource busy" on CI machines; retry.
  until hdiutil create -volname Flick -srcfolder "$dmg_root" -fs HFS+ \
    -format UDZO -imagekey zlib-level=9 "$staged_dmg"; do
    [[ $attempt -lt 3 ]] || die "hdiutil create failed"
    attempt=$((attempt + 1))
    rm -f "$staged_dmg"
    sleep 5
  done
  rm -rf "$dmg_root"
  if ! $adhoc; then
    dmg_sign_args=(--force --sign "$identity" --timestamp)
    if [[ -n $keychain ]]; then dmg_sign_args+=(--keychain "$keychain"); fi
    codesign "${dmg_sign_args[@]}" "$staged_dmg"
  fi
  if [[ $notary_mode != none ]]; then
    submit_for_notarization "$staged_dmg"
    staple "$staged_dmg"
    spctl --assess --type open --context context:primary-signature --verbose=2 "$staged_dmg"
    notarized="app and DMG"
  fi
  hdiutil verify "$staged_dmg" > /dev/null
fi

# ---------------------------------------------------------------- finish

if [[ -e $app_path ]]; then rm -rf "$app_path"; fi
mv "$app" "$app_path"
if [[ -n $staged_dmg ]]; then
  if [[ -e $dmg_path ]]; then rm -f "$dmg_path"; fi
  mv "$staged_dmg" "$dmg_path"
fi

printf '\nSummary\n'
printf '  app              %s (%s)\n' "$app_path" "$(du -sh "$app_path" | cut -f1)"
if [[ -n $staged_dmg ]]; then
  printf '  dmg              %s (%s)\n' "$dmg_path" "$(du -h "$dmg_path" | cut -f1)"
fi
printf '  version          %s (build %s), %s\n' "$short_version" "$build_number" "$arch"
printf '  bundle id        %s\n' "$bundle_id"
printf '  minimum macOS    %s\n' "$min_macos"
printf '  bundled dylibs   %s (from %s)\n' "$library_count" "${prefix:-$(dirname "$libmpv_real")}"
printf '  signing          %s, hardened runtime\n' "$(identity_label)"
printf '  entitlements     %s\n' "${entitlements#"$repo_root"/}"
printf '  notarized        %s\n' "$notarized"
printf '  licenses         %s\n' "$app_path/Contents/Resources/Licenses/bundled-libraries/MANIFEST.tsv"
if [[ -n $gpl_formulas ]]; then
  printf '  GPL components   %s(see docs/PACKAGING.md before distributing)\n' "$gpl_formulas"
fi
if [[ -s $work/warnings ]]; then
  printf '\nWarnings\n'
  sed 's/^/  - /' "$work/warnings"
fi
