# Packaging Flick for macOS

`scripts/package-macos.sh` builds `Flick.app` with libmpv and every non-system library it needs inside the bundle. It then signs the app with the hardened runtime, can notarize and staple it, and wraps it in a compressed DMG. The result should run on a Mac without `brew install mpv`.

**Status: unverified on macOS.** The script, entitlements, and CI job were written on Linux. There they passed only syntax checks, ShellCheck, a dry run, and a run against stand-ins for the macOS tools (see [validation](VALIDATION.md#macos-packaging-script-on-2026-10-24)). A real bundle, its signatures, notarization, the DMG, and playback from the bundle are unproven until the `macos-package` CI job or a Mac runs them. Backlog item 25 stays open until then.

## Requirements

- macOS with full Xcode, as for any [native build](VALIDATION.md#macos), and the pinned Rust toolchain.
- libmpv for the build machine's architecture. Homebrew's is the default: `brew install mpv`. The script copies from an existing installation and never downloads anything.
- For distribution: a Developer ID Application certificate and notarization credentials. Without them the app is signed ad hoc, which suits CI and local testing only.

## Usage

```sh
# Ad-hoc signed app and DMG in target/macos/
scripts/package-macos.sh

# Check settings and print the plan and Info.plist; works on any OS
scripts/package-macos.sh --dry-run

# Developer ID signing, notarization through a stored keychain profile
xcrun notarytool store-credentials flick-notary \
  --apple-id you@example.com --team-id TEAMID1234
FLICK_CODESIGN_IDENTITY="Developer ID Application: Your Name (TEAMID1234)" \
FLICK_NOTARY_KEYCHAIN_PROFILE=flick-notary \
  scripts/package-macos.sh

# Re-check an existing bundle
scripts/package-macos.sh --verify target/macos/Flick.app
```

Outputs go to `target/macos/Flick.app` and `target/macos/Flick-VERSION-ARCH.dmg`. The script refuses to replace either unless `--force` is given. It assembles everything in a hidden staging directory and moves the results into place only after every step succeeds.

| Option | Purpose |
| --- | --- |
| `--prefix DIR` | Prefix whose `lib/` holds libmpv. Default: `brew --prefix`, then `/opt/homebrew` or `/usr/local`. Env: `FLICK_MPV_PREFIX`. |
| `--libmpv FILE` | A specific libmpv dylib. Env: `FLICK_MPV_DYLIB`. |
| `--output-dir DIR` | Output directory. Default: `target/macos`. |
| `--min-macos X.Y` | `LSMinimumSystemVersion`; cannot be lower than the bundled binaries require. |
| `--entitlements FILE` | Replace the entitlements plist. |
| `--skip-build` | Package the existing `target/release/flick`. |
| `--no-dmg` | Stop after the app. |
| `--force` | Replace existing outputs. |
| `--dry-run` | Print the plan and Info.plist without building or writing outputs. |
| `--verify APP` | Only check an existing bundle's dylib closure and signatures. |

| Environment | Purpose |
| --- | --- |
| `FLICK_CODESIGN_IDENTITY` | Signing identity. Unset means ad hoc (`-`). |
| `FLICK_CODESIGN_KEYCHAIN` | Keychain holding that identity, if not in the search list. |
| `FLICK_BUNDLE_ID` | `CFBundleIdentifier`. Default `app.Flick.Flick`, matching the settings directory that `directories::ProjectDirs::from("app", "Flick", "Flick")` uses. |
| `FLICK_BUILD_NUMBER` | `CFBundleVersion`. Default: the `Cargo.toml` version. |
| `FLICK_NOTARY_KEYCHAIN_PROFILE` | Notarize with a `notarytool store-credentials` profile. |
| `FLICK_NOTARY_APPLE_ID`, `FLICK_NOTARY_TEAM_ID`, `FLICK_NOTARY_PASSWORD` | Notarize with an Apple ID and app-specific password. The password is passed on `notarytool`'s command line, so prefer a keychain profile on shared machines. |
| `FLICK_NOTARY_KEY`, `FLICK_NOTARY_KEY_ID`, `FLICK_NOTARY_ISSUER` | Notarize with an App Store Connect API key (`.p8` path). The issuer is omitted for individual keys. |

Exactly one complete notarization set, or none, must be present. Partial or conflicting sets are errors, and so is notarization with an ad-hoc identity.

## What the script does

1. Runs `cargo build --locked --release` unless `--skip-build` is given.
2. Copies `target/release/flick` to `Contents/MacOS/flick`.
3. Starting from libmpv and the executable, reads `LC_LOAD_DYLIB`, `LC_LOAD_WEAK_DYLIB`, `LC_REEXPORT_DYLIB`, and `LC_RPATH` with `otool -l`. It resolves absolute, `@rpath/`, and `@loader_path/` install names and copies every library outside `/usr/lib` and `/System` into `Contents/Frameworks`. Each file is named after its install name, such as `libavcodec.61.dylib` and `libmpv.2.dylib`. Framework binaries, such as Python's from VapourSynth, get a `.dylib` suffix. The copies are flat: two different libraries with the same name stop the script.
4. Rewrites install names with `install_name_tool`. Each library's id becomes `@rpath/NAME`, and each dependency on a bundled library becomes `@loader_path/NAME`, so no rpath is needed. Rpaths outside the OS are deleted. The executable links nothing outside the OS today; if it ever does, those references become `@executable_path/../Frameworks/NAME`. `dylibbundler` is not used.
5. Checks that every binary includes the host architecture. It sets `LSMinimumSystemVersion` to the highest `LC_BUILD_VERSION` minimum found in the bundle.
6. Fails unless every dependency resolves inside the bundle or the OS, no rpath points elsewhere, and `otool -L` lists no `/opt/homebrew` or `/usr/local` path.
7. Writes `Info.plist`, `PkgInfo`, and `Flick.icns`, built with `sips` and `iconutil` from `packaging/macos/Flick-icon-1024.png`.
8. Copies license texts into `Contents/Resources/Licenses` (see [licenses](#licenses-and-what-the-bundle-ships)).
9. Clears extended attributes and signs each dylib with `codesign --options runtime`, then the app with the entitlements. It then runs `codesign --verify --deep --strict` and checks that every binary has the hardened runtime flag. Developer ID signatures get a secure timestamp, which contacts Apple's timestamp server.
10. With credentials, zips the app with `ditto`, submits it with `xcrun notarytool submit --wait`, staples the app, and checks it with `spctl`. If Apple rejects it, the script prints `notarytool log`.
11. Creates a UDZO DMG with `hdiutil`, containing `Flick.app` and an `/Applications` link. `hdiutil` sometimes reports "Resource busy" on CI, so it retries twice. With Developer ID it signs the DMG, and with credentials it notarizes and staples it too. The app is stapled as well, so it passes Gatekeeper offline after being copied out.
12. Prints a summary with sizes, minimum macOS, library count, signing, notarization, the license manifest, and warnings.

### How Flick finds the bundled libmpv

`library_candidates()` in `src/playback.rs` tries, in order: the bare names `libmpv.2.dylib`, `libmpv.1.dylib`, and `libmpv.dylib`; the same names beside the executable; `../Frameworks/` from the executable; and finally `/opt/homebrew/lib` and `/usr/local/lib`. `FLICK_MPV_LIBRARY` replaces the whole list.

Apple documents that the hardened runtime makes dyld ignore `DYLD_*` variables. dyld is also expected to restrict a bare-name `dlopen` to the fallback directory `/usr/lib` in such a process. If so, the bundled copy is found before the Homebrew paths. That restriction comes from dyld's handling of restricted processes, not from Apple's documentation, and it has not been observed for Flick. On an Intel Mac with Homebrew mpv in `/usr/local/lib`, a process without the hardened runtime could load that copy first. With a Developer ID signature, library validation would reject Homebrew's ad-hoc-signed copy, and the loader would move on. `src/playback.rs` was not changed. Listing `../Frameworks` first on macOS would remove the ambiguity; that is a small follow-up. CI proves the bundled copy loads by hiding Homebrew's libraries before running the smoke test.

### Minimum macOS and architecture

Homebrew bottles are built for one macOS release. Their `LC_BUILD_VERSION` minimum is usually that release, so a DMG built from Homebrew on macOS 15 requires macOS 15 or later. The script reports which library sets the minimum. Supporting older systems needs libmpv and its dependencies built with an older `MACOSX_DEPLOYMENT_TARGET`.

The bundle has one architecture, the build machine's, because Homebrew installs one. A universal app would need both architectures of every library merged with `lipo`; the script does not do this.

### Paths that still point outside the bundle

Some libraries embed their build prefix as data, not as a link: for example fontconfig's configuration directory, Vulkan driver manifests (MoltenVK), VapourSynth's plugin directory, Python's standard library, and libbluray's Java support. `otool` cannot see these. The script warns and lists every bundled library that contains the prefix string. On a Mac without Homebrew, those features find nothing. Flick renders with libmpv's software API and does not use Vulkan or VapourSynth. Whether subtitle fonts are unaffected depends on libass using CoreText. That has not been checked on a Mac without Homebrew.

## Signing and entitlements

Every binary is signed with the hardened runtime, which notarization requires. The entitlements live in `packaging/macos`:

| Entitlement | `Flick.entitlements` (Developer ID) | `Flick-adhoc.entitlements` (ad hoc) |
| --- | --- | --- |
| `com.apple.security.cs.allow-unsigned-executable-memory` | yes | yes |
| `com.apple.security.cs.allow-jit` | yes | yes |
| `com.apple.security.cs.disable-library-validation` | no | yes |

- **Executable memory.** Homebrew builds mpv with `-Dlua=luajit`. One developer shipping libmpv on the Mac App Store [reported](https://dev.to/yamayu-dev/shipping-a-libmpv-based-video-player-on-the-mac-app-store-1f2l) that LuaJIT allocates executable memory during `mpv_initialize()`, even with scripts disabled. With only `allow-jit`, AMFI killed the process. Their tested configuration that worked had both `allow-jit` and `allow-unsigned-executable-memory`; Flick uses the same pair. Whether `allow-jit` alone can be dropped is untested. A libmpv built with `-Dlua=disabled` should need neither.
- **Library validation stays on for Developer ID builds.** Every bundled dylib is re-signed with the app's identity and Team ID, so the hardened runtime accepts them without `disable-library-validation`. Validation also blocks foreign dylibs, including an ad-hoc-signed Homebrew libmpv.
- **Ad-hoc builds disable library validation.** Ad-hoc signatures carry no Team ID, and library validation rejects such libraries with "mapped file has no Team ID and is not a platform binary". Without the exception, the CI smoke test could not load the bundled libmpv. Ad-hoc apps cannot be notarized and are blocked by Gatekeeper when downloaded, so this exception never reaches users through a notarized release.
- No sandbox, network, or file entitlements are needed outside the App Sandbox.

`Info.plist` adds purpose strings for the Desktop, Documents, Downloads, removable and network volumes, and the local network. macOS shows them when Flick first reads those folders or contacts local peers and the router for UPnP.

## Document types

`Info.plist` declares Flick as an alternate viewer for every video type that `src/library.rs` accepts and for `.torrent` files. Types with system identifiers use `LSItemContentTypes`. MKV, WebM, FLV, OGV, VOB, and the MPEG transport streams also have an extension list, because some macOS versions lack identifiers for them. The rank is `Alternate`, so Flick appears under Open With without taking over existing defaults.

Flick does not yet handle macOS's open-documents event. GPUI 0.2.2 delivers it through `App::on_open_urls`, which `src/main.rs` does not register. Choosing Flick in Open With therefore launches it without opening the file. Paths given on the command line work. Handling this requires a change in `src/main.rs`.

## Licenses and what the bundle ships

This is a record of facts and open questions, not legal advice.

The bundle redistributes Homebrew's binaries unmodified, apart from install names and signatures. The formulae on 2026-10-24 declare:

- **mpv 0.41.0:** `GPL-2.0-or-later AND LGPL-2.1-or-later`. It is built with mpv's default GPL configuration and LuaJIT.
- **FFmpeg 9.0.2:** `GPL-3.0-or-later`. It is configured with `--enable-gpl --enable-version3` and links x264, x265, and OpenSSL, among others.
- The rest of the closure is a mix of licenses, including MIT LuaJIT, LGPL VapourSynth, and Python's PSF license. It varies with each Homebrew update. The script records what was actually bundled.

For each bundled library, the script records its Homebrew keg: the formula, version, the license that formula declares, and its source URL. That comes from the keg's `.brew/FORMULA.rb`, or from its `sbom.spdx.json` when the formula file is absent. The record goes in `Contents/Resources/Licenses/bundled-libraries/MANIFEST.tsv`. The keg's license files are copied into `bundled-libraries/FORMULA-VERSION/`, along with its SBOM. These are files named `COPYING*`, `COPYRIGHT*`, `LICENSE*`, `LICENCE*`, `NOTICE*`, `PATENTS*`, or `UNLICENSE*`, found at the keg root, `share/doc/*/`, or `share/licenses/*/`. It warns about kegs without license files and about libraries not from a keg, and it names the GPL formulae in its summary. `Contents/Resources/Licenses` also holds Flick's `THIRD_PARTY_NOTICES.md`, the Inter font's OFL, and the Hugeicons MIT license, which covers the play glyph in the app icon.

Declared licenses are not verified against each library's build configuration. A missing license file in a keg means the text must be found and added by hand.

### Open questions before public distribution

1. **Flick's own license.** The repository has no `LICENSE` file, and `Cargo.toml` declares none. Flick calls libmpv in-process through `dlopen`. With Homebrew's GPL libmpv and GPLv3 FFmpeg, the FSF's view is that the shipped combination must be distributable under GPLv3-compatible terms. Choose Flick's license with that in mind, or follow item 4.
2. **Corresponding source.** GPL and LGPL require offering the exact source of the shipped binaries, including Homebrew's patches and build flags. That means a source archive beside each release or a written offer. The manifest records source URLs, but the script does not archive sources, since it never downloads anything. Nothing provides them yet.
3. **Rust dependencies.** The executable statically links hundreds of crates under MIT, Apache-2.0, and other licenses. Their notices are not collected yet. A tool such as `cargo-about` could generate them; it is not installed or run here.
4. **An LGPL-only build.** mpv with `-Dgpl=false -Dlua=disabled`, plus an FFmpeg built without `--enable-gpl`, x264, or x265, would avoid the GPL question. It would also drop both executable-memory entitlements. It means building those libraries instead of using Homebrew's. Pass the result with `--prefix` or `--libmpv`.
5. **Codec patents.** The bundled FFmpeg includes H.264 and HEVC encoders and decoders. Patent licensing for distribution in some countries is a separate question from copyright licensing.
6. **LGPL relinking.** LGPL lets users replace the library. A user who swaps a dylib in a signed bundle breaks its signature and library validation. Whether that matters for the LGPL parts has not been assessed.

## Continuous integration

The `macos-package` job in `.github/workflows/ci.yml` runs on `macos-latest`:

1. Installs mpv, FFmpeg, and ShellCheck with Homebrew. It lints the scripts, checks them with macOS's `/bin/bash` 3.2, and runs a dry run.
2. Packages ad hoc with `/bin/bash`. It runs `--verify`, `codesign --verify --deep --strict`, and an `otool -L` check for Homebrew paths, then prints the entitlements, Info.plist, Frameworks, and license manifest.
3. When the secrets below are set, imports the certificate into a temporary keychain and packages again with the Developer ID, notarizing when the notary secrets exist too.
4. Generates a fixture with `scripts/make-test-video.sh`. It then moves Homebrew's `Cellar` and `opt` directories aside, so any leftover Homebrew reference fails to load.
5. With an empty environment, so no `FLICK_MPV_LIBRARY` and no `DYLD_*`, and with `FLICK_AUDIO=null`, runs `flick --smoke-test` from the ad-hoc app and from the mounted DMG. It also runs from the Developer ID app when one was built.
6. Restores Homebrew and deletes the keychain, then uploads the DMG and license manifest as `flick-macos-adhoc`, and the signed DMG as `flick-macos-signed` when built.

| Secret | Contents |
| --- | --- |
| `MACOS_CERTIFICATE_P12` | Base64 of the exported Developer ID Application certificate and private key (`.p12`). |
| `MACOS_CERTIFICATE_PASSWORD` | Password of that `.p12`. |
| `MACOS_CODESIGN_IDENTITY` | For example `Developer ID Application: Your Name (TEAMID1234)`. |
| `MACOS_NOTARY_APPLE_ID`, `MACOS_NOTARY_TEAM_ID`, `MACOS_NOTARY_PASSWORD` | Apple ID, team, and app-specific password for notarization. |

Without secrets, including on pull requests from forks, only the ad-hoc path runs. The job adds a release build and a full Homebrew mpv installation to each push, which uses a considerable share of macOS runner minutes.

## Checking the script without a Mac

```sh
bash -n scripts/package-macos.sh
shellcheck scripts/package-macos.sh packaging/macos/test-package-script.sh
scripts/package-macos.sh --dry-run
bash packaging/macos/test-package-script.sh
```

`packaging/macos/test-package-script.sh` runs the real script against a fake Homebrew prefix whose "Mach-O" files are text, with stand-ins for `otool`, `install_name_tool`, `codesign`, `lipo`, `plutil`, `sips`, `iconutil`, `xattr`, `hdiutil`, `ditto`, and `brew`. It checks the script's own logic. It covers the closure walk, `@rpath` and `@loader_path` resolution, framework and weak dependencies, relinking, rpath removal, the minimum macOS, license collection, signing order, the DMG step, `--verify`, `--force`, and failure handling. It says nothing about real Mach-O files, Apple's tools, signatures, or Gatekeeper.

## Files

- `scripts/package-macos.sh`: the packaging script.
- `packaging/macos/Flick.entitlements`, `packaging/macos/Flick-adhoc.entitlements`: hardened-runtime entitlements, explained above.
- `packaging/macos/Flick-icon.svg`: the app icon, drawn for Flick around the Hugeicons play glyph from `assets/icons/play.svg`. `Flick-icon-1024.png` is rendered from it with `rsvg-convert -w 1024 -h 1024` (librsvg 2.58.0). Re-render after editing the SVG.
- `packaging/macos/test-package-script.sh`: the stand-in test described above.
