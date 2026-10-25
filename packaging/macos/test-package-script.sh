#!/usr/bin/env bash
# Assertions run small scripts with bash -c and positional arguments.
# shellcheck disable=SC2016,SC2012
# Exercise scripts/package-macos.sh on any OS with stand-ins for the macOS tools
# (otool, install_name_tool, codesign, lipo, plutil, sips, iconutil, xattr,
# hdiutil, ditto, brew) and a fake Homebrew prefix whose "Mach-O" files are text.
#
# This checks the script's own logic: the dependency walk, @rpath and
# @loader_path resolution, relinking, rpath removal, the minimum macOS, license
# collection, signing order, the DMG step, --verify, and --force. It proves
# nothing about real Mach-O files, Apple's tools, signatures, or Gatekeeper;
# only a run on macOS does. Requires python3.
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd -P)
root=$(mktemp -d "${TMPDIR:-/tmp}/flick-package-test.XXXXXX")
trap '[[ -n ${KEEP:-} ]] || rm -rf "$root"' EXIT
stubs=$root/bin
hb=$root/hb
mkdir -p "$stubs" "$hb"

failures=0
pass() { printf 'ok   %s\n' "$*"; }
fail() { printf 'FAIL %s\n' "$*"; failures=$((failures + 1)); }
check() { # description, command...
  local description=$1
  shift
  if "$@"; then pass "$description"; else fail "$description"; fi
}

# ---------------------------------------------------------------- stand-ins

stub() { cat > "$stubs/$1"; chmod +x "$stubs/$1"; }

stub uname <<'EOF'
#!/usr/bin/env bash
case ${1:-} in
  -s) echo Darwin ;;
  -m) echo arm64 ;;
  *) exec /usr/bin/uname "$@" ;;
esac
EOF

stub file <<'EOF'
#!/usr/bin/env bash
[[ $1 == -b ]] && shift
if [[ -f $1 ]] && head -n 1 "$1" | grep -q '^FAKE-MACHO'; then
  echo 'Mach-O 64-bit dynamically linked shared library arm64'
else
  echo data
fi
EOF

# Fake Mach-O: "FAKE-MACHO" then lines "id X", "dep X", "weak X", "rpath X",
# "minos X", "arch X", "signed FLAGS", and free text.
stub otool <<'EOF'
#!/usr/bin/env bash
mode=$1
shift
for file in "$@"; do
  if ! head -n 1 "$file" | grep -q '^FAKE-MACHO'; then
    echo "$file: is not an object file"
    continue
  fi
  echo "$file:"
  n=0
  while read -r kind value; do
    case $mode:$kind in
      -l:id)
        printf 'Load command %d\n          cmd LC_ID_DYLIB\n      cmdsize 56\n         name %s (offset 24)\n   time stamp 1 Thu Jan  1 00:00:01 1970\n      current version 1.0.0\ncompatibility version 1.0.0\n' $n "$value" ;;
      -l:dep|-l:weak)
        cmd=LC_LOAD_DYLIB; [[ $kind == weak ]] && cmd=LC_LOAD_WEAK_DYLIB
        printf 'Load command %d\n          cmd %s\n      cmdsize 56\n         name %s (offset 24)\n   time stamp 2 Thu Jan  1 00:00:02 1970\n      current version 1.0.0\ncompatibility version 1.0.0\n' $n "$cmd" "$value" ;;
      -l:rpath)
        printf 'Load command %d\n          cmd LC_RPATH\n      cmdsize 32\n         path %s (offset 12)\n' $n "$value" ;;
      -l:minos)
        printf 'Load command %d\n      cmd LC_BUILD_VERSION\n  cmdsize 32\n platform 1\n    minos %s\n      sdk 15.0\n   ntools 1\n     tool 3\n  version 1115.7.3\n' $n "$value" ;;
      -L:id|-L:dep|-L:weak)
        printf '\t%s (compatibility version 1.0.0, current version 1.0.0)\n' "$value" ;;
    esac
    n=$((n + 1))
  done < <(tail -n +2 "$file")
done
EOF

stub install_name_tool <<'EOF'
#!/usr/bin/env python3
import sys
args = sys.argv[1:]
path = args.pop()
lines = open(path).read().split('\n')
i = 0
while i < len(args):
    flag = args[i]
    if flag == '-id':
        lines = [('id ' + args[i + 1]) if l.startswith('id ') else l for l in lines]
        i += 2
    elif flag == '-change':
        old, new = args[i + 1], args[i + 2]
        lines = [(l.split(' ', 1)[0] + ' ' + new) if l in ('dep ' + old, 'weak ' + old) else l for l in lines]
        i += 3
    elif flag == '-delete_rpath':
        target = 'rpath ' + args[i + 1]
        if target not in lines:
            sys.exit('install_name_tool: no LC_RPATH load command with path: ' + args[i + 1])
        lines.remove(target)
        i += 2
    else:
        sys.exit('install_name_tool stand-in: unknown flag ' + flag)
lines = [l for l in lines if not l.startswith('signed ')]
open(path, 'w').write('\n'.join(lines))
print('warning: changes being made to the file will invalidate the code signature in: ' + path, file=sys.stderr)
EOF

stub codesign <<'EOF'
#!/usr/bin/env bash
echo "codesign $*" >> "$FLICK_TEST_LOG"
target=${!#}
main_of() { if [[ -d $1 ]]; then echo "$1/Contents/MacOS/flick"; else echo "$1"; fi; }
case " $* " in
  *" --verify "*)
    for file in "$target"/Contents/MacOS/* "$target"/Contents/Frameworks/*; do
      if head -n 1 "$file" | grep -q '^FAKE-MACHO' && ! grep -q '^signed ' "$file"; then
        echo "$file: code object is not signed at all" >&2; exit 1
      fi
    done
    echo "$target: valid on disk" >&2 ;;
  *" --display "*)
    file=$(main_of "$target")
    flags=$(sed -n 's/^signed //p' "$file" | tail -n 1)
    [[ -n $flags ]] || { echo "$file: code object is not signed at all" >&2; exit 1; }
    echo "Executable=$file" >&2
    echo "CodeDirectory v=20500 size=1 flags=0x10002($flags) hashes=1+2 location=embedded" >&2 ;;
  *" --sign "*)
    flags=adhoc; [[ " $* " == *" --sign - "* ]] || flags=''
    [[ " $* " == *" --options runtime "* ]] && flags=${flags:+$flags,}runtime
    file=$(main_of "$target")
    if head -n 1 "$file" | grep -q '^FAKE-MACHO'; then
      grep -v '^signed ' "$file" > "$file.tmp" || true
      mv "$file.tmp" "$file"; echo "signed $flags" >> "$file"
    fi ;;
esac
EOF

stub lipo <<'EOF'
#!/usr/bin/env bash
[[ $1 == -archs ]] || exit 1
sed -n 's/^arch //p' "$2" | paste -s -d ' ' -
EOF

stub plutil <<'EOF'
#!/usr/bin/env bash
[[ $1 == -lint ]] && exec python3 -I -c 'import plistlib, sys; plistlib.load(open(sys.argv[1], "rb")); print(sys.argv[1] + ": OK")' "$2"
exit 1
EOF

stub sips <<'EOF'
#!/usr/bin/env bash
# sips -z H W input --out output
cp "$4" "$6"
EOF

stub iconutil <<'EOF'
#!/usr/bin/env bash
# iconutil --convert icns --output FILE DIR
[[ $(ls "$5" | wc -l) -eq 10 ]] || { echo "iconutil: expected 10 images" >&2; exit 1; }
echo icns > "$4"
EOF

stub xattr <<'EOF'
#!/usr/bin/env bash
exit 0
EOF

stub ditto <<'EOF'
#!/usr/bin/env bash
cp -R "$1" "$2"
EOF

stub hdiutil <<'EOF'
#!/usr/bin/env bash
echo "hdiutil $*" >> "$FLICK_TEST_LOG"
case $1 in
  create)
    srcfolder='' out=${!#}
    while [[ $# -gt 0 ]]; do [[ $1 == -srcfolder ]] && srcfolder=$2; shift; done
    (cd "$srcfolder" && find . | sort) > "$out" ;;
  verify) exit 0 ;;
esac
EOF

stub brew <<EOF
#!/usr/bin/env bash
[[ \$1 == --prefix ]] && echo "$hb"
EOF

# ---------------------------------------------------------------- fake prefix

macho() { # path, lines...
  local path=$1
  shift
  mkdir -p "$(dirname "$path")"
  printf 'FAKE-MACHO\narch arm64\n' > "$path"
  printf '%s\n' "$@" >> "$path"
}
keg() { # formula version
  mkdir -p "$hb/Cellar/$1/$2/.brew"
  ln -sfn "../Cellar/$1/$2" "$hb/opt/$1"
}
mkdir -p "$hb/opt" "$hb/lib"

keg mpv 0.40.0
macho "$hb/Cellar/mpv/0.40.0/lib/libmpv.2.dylib" \
  "id $hb/opt/mpv/lib/libmpv.2.dylib" "minos 14.0" \
  "dep /usr/lib/libSystem.B.dylib" \
  "dep /System/Library/Frameworks/CoreAudio.framework/Versions/A/CoreAudio" \
  "dep @rpath/libswiftCore.dylib" \
  "dep $hb/opt/ffmpeg/lib/libavcodec.61.dylib" \
  "dep @rpath/libavutil.59.dylib" \
  "dep $hb/opt/luajit/lib/libluajit-5.1.2.dylib" \
  "dep $hb/opt/python@3.13/Frameworks/Python.framework/Versions/3.13/Python" \
  "weak $hb/opt/fontconfig/lib/libfontconfig.1.dylib" \
  "rpath /usr/lib/swift" "rpath $hb/lib" "rpath $hb/lib"
ln -s ../Cellar/mpv/0.40.0/lib/libmpv.2.dylib "$hb/lib/libmpv.2.dylib"
printf '  license all_of: [\n    "GPL-2.0-or-later",\n    "LGPL-2.1-or-later",\n  ]\n  url "https://github.com/mpv-player/mpv/archive/refs/tags/v0.40.0.tar.gz"\n' \
  > "$hb/Cellar/mpv/0.40.0/.brew/mpv.rb"
echo 'GPL text' > "$hb/Cellar/mpv/0.40.0/LICENSE.GPL"
echo 'LGPL text' > "$hb/Cellar/mpv/0.40.0/LICENSE.LGPL"
echo 'Copyright' > "$hb/Cellar/mpv/0.40.0/Copyright"
echo 'readme' > "$hb/Cellar/mpv/0.40.0/README.md"

keg ffmpeg 7.1.1
macho "$hb/Cellar/ffmpeg/7.1.1/lib/libavcodec.61.19.101.dylib" \
  "id $hb/opt/ffmpeg/lib/libavcodec.61.dylib" "minos 14.0" \
  "dep @rpath/libavutil.59.dylib" "dep /usr/lib/libSystem.B.dylib" "rpath @loader_path/../lib"
ln -s libavcodec.61.19.101.dylib "$hb/Cellar/ffmpeg/7.1.1/lib/libavcodec.61.dylib"
macho "$hb/Cellar/ffmpeg/7.1.1/lib/libavutil.59.39.100.dylib" \
  "id $hb/opt/ffmpeg/lib/libavutil.59.dylib" "minos 14.0" "dep /usr/lib/libSystem.B.dylib"
ln -s libavutil.59.39.100.dylib "$hb/Cellar/ffmpeg/7.1.1/lib/libavutil.59.dylib"
for name in libavcodec.61.dylib libavutil.59.dylib; do ln -s "../opt/ffmpeg/lib/$name" "$hb/lib/$name"; done
printf '  license "GPL-2.0-or-later"\n  url "https://ffmpeg.org/releases/ffmpeg-7.1.1.tar.xz"\n' > "$hb/Cellar/ffmpeg/7.1.1/.brew/ffmpeg.rb"
echo 'GPLv2' > "$hb/Cellar/ffmpeg/7.1.1/COPYING.GPLv2"
echo 'LGPLv2.1' > "$hb/Cellar/ffmpeg/7.1.1/COPYING.LGPLv2.1"
echo 'license md' > "$hb/Cellar/ffmpeg/7.1.1/LICENSE.md"

keg luajit 2.1.1736781742
macho "$hb/Cellar/luajit/2.1.1736781742/lib/libluajit-5.1.2.dylib" \
  "id $hb/opt/luajit/lib/libluajit-5.1.2.dylib" "minos 13.0" "dep /usr/lib/libSystem.B.dylib"
printf '  license "MIT"\n  url "https://github.com/LuaJIT/LuaJIT.git"\n' > "$hb/Cellar/luajit/2.1.1736781742/.brew/luajit.rb"
mkdir -p "$hb/Cellar/luajit/2.1.1736781742/share/doc/luajit"
echo 'MIT' > "$hb/Cellar/luajit/2.1.1736781742/share/doc/luajit/COPYRIGHT"

keg python@3.13 3.13.3
macho "$hb/Cellar/python@3.13/3.13.3/Frameworks/Python.framework/Versions/3.13/Python" \
  "id $hb/opt/python@3.13/Frameworks/Python.framework/Versions/3.13/Python" "minos 14.0" \
  "dep $hb/opt/gettext/lib/libintl.8.dylib" "dep /usr/lib/libSystem.B.dylib"
printf '  license "Python-2.0"\n' > "$hb/Cellar/python@3.13/3.13.3/.brew/python@3.13.rb"
echo 'PSF' > "$hb/Cellar/python@3.13/3.13.3/LICENSE"

# No license file in the keg and no .rb: falls back to the SBOM, then warns.
keg gettext 0.24
rmdir "$hb/Cellar/gettext/0.24/.brew"
macho "$hb/Cellar/gettext/0.24/lib/libintl.8.dylib" \
  "id $hb/opt/gettext/lib/libintl.8.dylib" "minos 14.0" "dep /usr/lib/libSystem.B.dylib"
printf '{\n  "packages": [\n    {\n      "licenseDeclared": "GPL-3.0-or-later"\n    }\n  ]\n}\n' > "$hb/Cellar/gettext/0.24/sbom.spdx.json"

keg fontconfig 2.16.0
macho "$hb/Cellar/fontconfig/2.16.0/lib/libfontconfig.1.dylib" \
  "id $hb/opt/fontconfig/lib/libfontconfig.1.dylib" "minos 14.0" \
  "dep /usr/lib/libSystem.B.dylib" "config $hb/etc/fonts/fonts.conf"
printf '  license "MIT"\n' > "$hb/Cellar/fontconfig/2.16.0/.brew/fontconfig.rb"
echo 'MIT' > "$hb/Cellar/fontconfig/2.16.0/COPYING"

target=$root/target
macho "$target/release/flick" "minos 11.0" \
  "dep /usr/lib/libSystem.B.dylib" "dep /System/Library/Frameworks/Metal.framework/Versions/A/Metal"

export FLICK_TEST_LOG=$root/tool.log
export CARGO_TARGET_DIR=$target
export PATH="$stubs:$PATH"
unset FLICK_CODESIGN_IDENTITY FLICK_NOTARY_KEYCHAIN_PROFILE FLICK_NOTARY_APPLE_ID FLICK_NOTARY_TEAM_ID \
  FLICK_NOTARY_PASSWORD FLICK_NOTARY_KEY FLICK_NOTARY_KEY_ID FLICK_NOTARY_ISSUER FLICK_MPV_PREFIX FLICK_MPV_DYLIB
script=$repo_root/scripts/package-macos.sh
out=$root/out

# ---------------------------------------------------------------- package

if bash "$script" --skip-build --output-dir "$out" > "$root/run.log" 2>&1; then
  pass "packages with the fake prefix detected through brew --prefix"
else
  fail "packaging failed"; cat "$root/run.log"; exit 1
fi
app=$out/Flick.app
fw=$app/Contents/Frameworks

expected="Python.dylib libavcodec.61.dylib libavutil.59.dylib libfontconfig.1.dylib libintl.8.dylib libluajit-5.1.2.dylib libmpv.2.dylib"
check "bundles exactly the non-system closure, named by install id" \
  test "$(cd "$fw" && ls | sort | tr '\n' ' ')" = "$expected "
check "libmpv keeps the name the loader searches for" test -f "$fw/libmpv.2.dylib"
check "libmpv's dependencies point at @loader_path" \
  grep -qx "dep @loader_path/libavcodec.61.dylib" "$fw/libmpv.2.dylib"
check "@rpath dependencies are resolved through the library's own rpaths" \
  grep -qx "dep @loader_path/libavutil.59.dylib" "$fw/libavcodec.61.dylib"
check "framework binaries are bundled as dylibs" \
  grep -qx "dep @loader_path/Python.dylib" "$fw/libmpv.2.dylib"
check "weak dependencies are bundled and relinked" \
  grep -qx "weak @loader_path/libfontconfig.1.dylib" "$fw/libmpv.2.dylib"
check "transitive dependencies of framework binaries are bundled" \
  grep -qx "dep @loader_path/libintl.8.dylib" "$fw/Python.dylib"
check "install ids become @rpath/NAME" grep -qx "id @rpath/libmpv.2.dylib" "$fw/libmpv.2.dylib"
check "Swift runtime stays in the OS with its rpath" \
  bash -c 'grep -qx "dep @rpath/libswiftCore.dylib" "$1" && grep -qx "rpath /usr/lib/swift" "$1"' _ "$fw/libmpv.2.dylib"
check "build-prefix rpaths, including duplicates, are deleted" \
  bash -c '! grep -q "^rpath $2" "$1" && ! grep -q "^rpath @loader_path" "$3"' _ "$fw/libmpv.2.dylib" "$hb" "$fw/libavcodec.61.dylib"
check "no bundled binary references the fake prefix" bash -c '! grep -rq "^\(dep\|weak\|id\|rpath\) $1" "$2"' _ "$hb" "$app"
check "the executable keeps only system dependencies" \
  bash -c '! grep -q "^dep @" "$1"' _ "$app/Contents/MacOS/flick"
check "Homebrew originals are untouched" grep -qx "dep $hb/opt/ffmpeg/lib/libavcodec.61.dylib" "$hb/Cellar/mpv/0.40.0/lib/libmpv.2.dylib"
check "LSMinimumSystemVersion is the highest minos in the bundle" \
  python3 -I -c 'import plistlib, sys; assert plistlib.load(open(sys.argv[1], "rb"))["LSMinimumSystemVersion"] == "14.0"' "$app/Contents/Info.plist"
check "PkgInfo and icon are written" bash -c 'test "$(cat "$1/PkgInfo")" = "APPL????" && test -f "$1/Resources/Flick.icns"' _ "$app/Contents"
check "every binary is signed with the hardened runtime" \
  bash -c 'for f in "$1"/Contents/MacOS/flick "$1"/Contents/Frameworks/*; do grep -qx "signed adhoc,runtime" "$f" || exit 1; done' _ "$app"
check "dylibs are signed before the app, and the app with the ad-hoc entitlements" \
  bash -c 'app=$(grep -n -- "--entitlements" "$1" | head -n 1); last=$(grep -n -- "--sign .*/Frameworks/" "$1" | tail -n 1 | cut -d: -f1)
    [[ -n $app && -n $last && $last -lt ${app%%:*} && $app == *Flick-adhoc.entitlements*Flick.app ]]' _ "$FLICK_TEST_LOG"

lic=$app/Contents/Resources/Licenses
manifest=$lic/bundled-libraries/MANIFEST.tsv
check "Flick's own notices are copied" test -f "$lic/Flick-THIRD_PARTY_NOTICES.md" -a -f "$lic/Inter-OFL.txt" -a -f "$lic/Hugeicons-LICENSE.txt"
check "license files come from each keg, not READMEs" \
  bash -c 'test -f "$1/mpv-0.40.0/LICENSE.GPL" -a -f "$1/mpv-0.40.0/Copyright" -a ! -e "$1/mpv-0.40.0/README.md" -a -f "$1/ffmpeg-7.1.1/COPYING.LGPLv2.1" -a -f "$1/luajit-2.1.1736781742/COPYRIGHT"' _ "$lic/bundled-libraries"
check "multi-line formula licenses are read" grep -q $'^libmpv.2.dylib\tmpv\t0.40.0\tall_of: \\[ "GPL-2.0-or-later", "LGPL-2.1-or-later", \\]\thttps://github.com/mpv-player' "$manifest"
check "the SBOM supplies a license when the formula file is absent" grep -q $'^libintl.8.dylib\tgettext\t0.24\tGPL-3.0-or-later\t' "$manifest"
check "one manifest row per bundled library" test "$(($(wc -l < "$manifest") - 1))" -eq 7
check "missing license files are reported" grep -q 'no license file found in the gettext 0.24 keg' "$root/run.log"
check "embedded prefix strings are reported" grep -q 'libfontconfig.1.dylib' "$root/run.log"
check "GPL components are named in the summary" grep -q 'GPL components   ffmpeg gettext mpv' "$root/run.log"
check "the DMG holds the app and an Applications link" \
  bash -c 'grep -qx "./Flick.app/Contents/Frameworks/libmpv.2.dylib" "$1" && grep -qx "./Applications" "$1"' _ "$out/Flick-0.1.0-arm64.dmg"
check "no staging directories are left behind" test "$(ls -A "$out" | tr '\n' ' ')" = "Flick-0.1.0-arm64.dmg Flick.app "

# ---------------------------------------------------------------- verify, force

check "--verify accepts the bundle" bash -c 'bash "$1" --verify "$2" > /dev/null 2>&1' _ "$script" "$app"
cp -R "$app" "$root/Tampered.app"
echo "dep /opt/homebrew/opt/x264/lib/libx264.164.dylib" >> "$root/Tampered.app/Contents/Frameworks/libavcodec.61.dylib"
check "--verify rejects a Homebrew reference" bash -c '! bash "$1" --verify "$2" > "$3" 2>&1 && grep -q "otool -L still lists Homebrew paths" "$3"' _ "$script" "$root/Tampered.app" "$root/verify.log"
rm -rf "$root/Tampered.app"
cp -R "$app" "$root/Unsigned.app"
# BSD sed has no GNU-style -i, so rewrite the file instead.
unsigned=$root/Unsigned.app/Contents/Frameworks/libintl.8.dylib
grep -v '^signed ' "$unsigned" > "$unsigned.tmp" || true
mv "$unsigned.tmp" "$unsigned"
check "--verify rejects an unsigned dylib" bash -c '! bash "$1" --verify "$2" > /dev/null 2>&1' _ "$script" "$root/Unsigned.app"
check "refuses to overwrite without --force" \
  bash -c '! bash "$1" --skip-build --output-dir "$2" > "$3" 2>&1 && grep -q "refusing to overwrite" "$3"' _ "$script" "$out" "$root/force.log"
check "--force replaces the outputs" \
  bash -c 'touch "$2/Flick.app/stale" && bash "$1" --skip-build --force --output-dir "$2" > /dev/null 2>&1 && ! test -e "$2/Flick.app/stale"' _ "$script" "$out"

# ---------------------------------------------------------------- failures

rm "$hb/Cellar/gettext/0.24/lib/libintl.8.dylib"
check "a missing dependency stops packaging with its name" \
  bash -c '! bash "$1" --skip-build --output-dir "$2" > "$3" 2>&1 && grep -q "libintl.8.dylib, which is missing" "$3" && ! test -e "$2/Flick.app"' _ "$script" "$root/out-missing" "$root/missing.log"
check "--min-macos below the bundle's requirement is refused" \
  bash -c '! bash "$1" --skip-build --no-dmg --min-macos 12.0 --libmpv "$2" --output-dir "$3" > "$4" 2>&1 && grep -q "lower than 14.0" "$4"' \
  _ "$script" "$hb/Cellar/ffmpeg/7.1.1/lib/libavutil.59.dylib" "$root/out-min" "$root/min.log"

printf '\n%d failed\n' "$failures"
[[ $failures -eq 0 ]]
