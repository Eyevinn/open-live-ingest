#!/usr/bin/env bash
# Builds a self-contained "Open Live Ingest.app" for Apple silicon: the desktop app
# (open-live-ingest-app, the bundle's main executable), the gateway CLI next to it for
# SSH use, the Strom release they are pinned to, and the Homebrew GStreamer that Strom
# links against, with every library reference rewritten to point inside the bundle.
# Runs on macOS only. From the repository root:
#
#   cargo build --release --target aarch64-apple-darwin
#   cargo build --release --target aarch64-apple-darwin -p open-live-ingest-app
#   .github/scripts/build-macos-app.sh target/aarch64-apple-darwin/release/open-live-ingest \
#       target/aarch64-apple-darwin/release/open-live-ingest-app dist
#
# Needs, on the build Mac: Homebrew with `brew install gstreamer` (the formula carries
# the full plugin set Strom's installer brings: applemedia/avfvideosrc, x264 and
# vtenc, AAC, srt), plus curl, jq and the Xcode command line tools (otool,
# install_name_tool, codesign). The Strom version is the strom-types tag in
# Cargo.toml; it is never written here. The asset is checked against the SHA-256
# digest GitHub records for it (Strom publishes no checksum file of its own).
#
# Layout, which src/local_strom.rs expects for a bundled Strom (BUNDLE_PLUGIN_DIR and
# BUNDLE_PLUGIN_SCANNER, relative to Contents):
#
#   Open Live Ingest.app/Contents/
#     Info.plist                       CFBundleExecutable open-live-ingest-app
#     MacOS/open-live-ingest-app       the desktop app, what Finder opens
#     MacOS/open-live-ingest           the CLI, for SSH and a PATH symlink
#     MacOS/strom
#     Frameworks/                      GStreamer, GLib and the rest of the closure (files only)
#     PlugIns/gstreamer/               GStreamer plugins
#     Helpers/gst-plugin-scanner
#     Resources/AppIcon.icns           from app/icons/AppIcon.icns (CFBundleIconFile)
#
# The app and the CLI both find the bundled Strom as "a strom next to the running
# executable", so either one runs the same gateway with the same GStreamer.
#
# Each kind of code sits where Apple's bundle layout puts it, because codesign only
# accepts nested code in those places. Frameworks, PlugIns, Helpers and MacOS are
# nested-code locations, and codesign takes any directory in them whose name has a
# dot (such as "gstreamer-1.0") for a nested bundle and rejects the app when it is
# not one. Hence "gstreamer", and the --check below fails on such a directory.
#
# Every non-system reference becomes @rpath/<name>. The rpath is
# @executable_path/../Frameworks for MacOS/* and Helpers/*, @loader_path for
# Frameworks/* and @loader_path/../../Frameworks for the plugins. Rpaths the Homebrew
# builds carried are removed. The last step fails if `otool -L` (or an LC_RPATH) of
# any Mach-O in the bundle still names /opt/homebrew or /usr/local, if one is not
# signed with the hardened runtime, if strom, open-live-ingest or open-live-ingest-app
# lacks the camera or microphone entitlement, if strom lacks
# com.apple.security.cs.allow-jit or any other executable or gst-plugin-scanner carries
# it, or if `codesign --verify --deep --strict` rejects the bundle.
#
# Signing, inside out and never with --deep: the libraries and plugins, then
# gst-plugin-scanner, strom and the CLI (open-live-ingest, a nested executable), then
# the bundle, which signs the main executable (open-live-ingest-app) and seals
# everything else. Every Mach-O gets the hardened runtime (--options runtime), and the
# three executables that run as the app, strom, open-live-ingest and
# open-live-ingest-app, get the entitlements in open-live-ingest.entitlements next to
# this script (camera and microphone; the file says why each one is there). strom alone
# also gets com.apple.security.cs.allow-jit, which the script adds to a copy of that
# file, so GStreamer's ORC can compile its inner loops at run time (#113). No other
# entitlement: the app's webview (WKWebView) runs its JIT in Apple's WebContent process,
# not in the app. By default the signature is ad hoc, which Apple silicon requires to
# load the code at all: the bundle runs on the build Mac and exercises the hardened
# runtime, but it cannot be notarized. An ad hoc signature has no Team ID, and macOS 27
# refuses to map the bundled libraries into an ad hoc signed process under library
# validation (#66), so an ad hoc build adds
# com.apple.security.cs.disable-library-validation to strom, open-live-ingest,
# open-live-ingest-app and gst-plugin-scanner; --check requires it there on an ad hoc
# bundle and refuses it on any other. Set MACOS_SIGN_IDENTITY (a "Developer ID
# Application: ..." identity, or its SHA-1 hash, in a keychain the caller has unlocked)
# to sign with it instead, with a secure timestamp, ready for notarization, and no
# hardened-runtime exception but strom's allow-jit; MACOS_SIGN_KEYCHAIN names that
# keychain when it is not in the search list. MACOS_ENTITLEMENTS overrides the
# entitlements file (strom still gets allow-jit on top of it). Signing the DMG,
# notarizing and stapling is left to the caller.
#
# Other environment knobs:
#   BUNDLE_ID             CFBundleIdentifier (default: com.eyevinn.open-live-ingest)
#   MACOS_MIN_VERSION     LSMinimumSystemVersion (default: this Mac's major version,
#                         since Homebrew bottles target the macOS they were built for)
#   GST_PLUGIN_EXCLUDE    space-separated plugin file names to leave out
#                         (default: libgstpython.dylib, which needs a Python install)
#   STROM_ASSET           path to an already downloaded strom-<tag>-macos-aarch64;
#                         it is still checked against the release digest
#
# Check an existing bundle only (no build):
#
#   .github/scripts/build-macos-app.sh --check "dist/Open Live Ingest.app"

set -euo pipefail

APP_NAME="Open Live Ingest.app"
STROM_REPO="Eyevinn/strom"
FRAMEWORKS_REL="Frameworks"
# These two must match BUNDLE_PLUGIN_DIR and BUNDLE_PLUGIN_SCANNER in src/local_strom.rs
# (a unit test there reads them from this file).
PLUGIN_DIR_REL="PlugIns/gstreamer"
SCANNER_REL="Helpers/gst-plugin-scanner"
# The nested-code locations of a bundle (see the layout above).
NESTED_CODE_DIRS="Frameworks PlugIns Helpers MacOS"
FORBIDDEN_RE='/opt/homebrew|/usr/local'
# The bundle's main executable (CFBundleExecutable), the CLI next to it, and Strom.
APP_EXEC="open-live-ingest-app"
CLI_EXEC="open-live-ingest"
EXECUTABLES="MacOS/$APP_EXEC MacOS/$CLI_EXEC MacOS/strom"
# The icon, committed (app/icons/make-icon.py draws it), and its name in the bundle.
ICON_NAME="AppIcon"
ICON_SRC_REL="app/icons/$ICON_NAME.icns"
# Granted to the executables of an ad hoc build only (see the header).
LIBRARY_VALIDATION_KEY="com.apple.security.cs.disable-library-validation"
# Granted to strom alone, ad hoc or Developer ID (see the header and #113).
JIT_KEY="com.apple.security.cs.allow-jit"
JIT_EXEC="MacOS/strom"

die() { echo "error: $*" >&2; exit 1; }
say() { echo "==> $*" >&2; }

# Every Mach-O file under a directory, one per line. Fails (rather than skipping the
# file) when `find` or `file` fails, so the AC1 check below cannot pass by not looking.
macho_files() {
    local f kind list
    list="$(find "$1" -type f)" || { echo "error: cannot list the files in $1" >&2; return 1; }
    [[ -n "$list" ]] || return 0
    while IFS= read -r f; do
        kind="$(file -b "$f")" || { echo "error: file failed on $f" >&2; return 1; }
        case "$kind" in
            '' | 'cannot open'* | 'ERROR'*) echo "error: file cannot read $f ($kind)" >&2; return 1 ;;
            *Mach-O*) printf '%s\n' "$f" ;;
        esac
    done <<<"$list"
}

# Whether the entitlements XML in $1 (`codesign -d --entitlements - --xml`) grants the
# key $2 (set to <true/>). With $3 = any, whether it names the key at all, whatever
# the value.
has_entitlement() {
    local flat
    flat="$(tr -d ' \t\n' <<<"$1")"
    if [[ "${3:-}" == any ]]; then
        grep -qF "<key>$2</key>" <<<"$flat"
    else
        grep -qF "<key>$2</key><true/>" <<<"$flat"
    fi
}

# The libraries a Mach-O loads, without its own install name.
deps_of() {
    local own
    own="$(otool -D "$1" | sed -n '2p')"
    otool -L "$1" | tail -n +2 | awk '{print $1}' | while IFS= read -r d; do
        if [[ "$d" != "$own" ]]; then printf '%s\n' "$d"; fi
    done
}

# The LC_RPATH entries of a Mach-O, one per line. A path may contain spaces, so take
# everything between "path " and the trailing " (offset N)".
rpaths_of() {
    otool -l "$1" | awk '
        /cmd LC_RPATH/ { r = 1 }
        r && $1 == "path" {
            sub(/^[ \t]*path /, ""); sub(/ \(offset [0-9]+\)$/, ""); print; r = 0
        }'
}

# AC1: fail on any Mach-O whose load commands still name a build-host prefix, and on
# a bundle codesign does not accept. This is the gate the release build relies on, so
# it fails closed: a missing tool, an otool that fails, a file that cannot be read or
# a bundle without its executables or plugins is an error, never a pass.
check_bundle() {
    local app="${1%/}" f d tool req key bad=0 checked=0 list load_cmds rpaths hits dotted sig ents adhoc
    local plist main_exec icon
    [[ -d "$app/Contents" ]] || die "$app is not an app bundle"
    for tool in file otool find awk sed grep codesign plutil; do
        command -v "$tool" >/dev/null 2>&1 || die "$tool not found (the check needs it)"
    done
    # codesign treats a dotted directory in a nested-code location as a bundle.
    dotted=""
    for d in $NESTED_CODE_DIRS; do
        [[ -d "$app/Contents/$d" ]] || continue
        dotted+="$(find "$app/Contents/$d" -mindepth 1 -type d -name '*.*')"$'\n' \
            || die "cannot list the directories in Contents/$d"
    done
    dotted="$(grep -v '^$' <<<"$dotted" || true)"
    [[ -z "$dotted" ]] || die "codesign would take these directories for nested bundles: $dotted"
    say "Checking every Mach-O in $app for $FORBIDDEN_RE"
    list="$(macho_files "$app")" || die "cannot list the Mach-O files in $app"
    for req in $EXECUTABLES "$SCANNER_REL"; do
        grep -qxF "$app/Contents/$req" <<<"$list" || die "Contents/$req is missing or is not a Mach-O"
    done
    plist="$app/Contents/Info.plist"
    main_exec="$(plutil -extract CFBundleExecutable raw -o - "$plist")" \
        || die "cannot read CFBundleExecutable from Contents/Info.plist"
    [[ "$main_exec" == "$APP_EXEC" ]] \
        || die "CFBundleExecutable is '$main_exec', not $APP_EXEC"
    icon="$(plutil -extract CFBundleIconFile raw -o - "$plist")" \
        || die "cannot read CFBundleIconFile from Contents/Info.plist"
    [[ -s "$app/Contents/Resources/${icon%.icns}.icns" ]] \
        || die "CFBundleIconFile names $icon, but Contents/Resources has no ${icon%.icns}.icns"
    say "OK: CFBundleExecutable is $APP_EXEC, the CLI and strom are next to it, the icon is there"
    grep -qF "$app/Contents/$PLUGIN_DIR_REL/" <<<"$list" || die "Contents/$PLUGIN_DIR_REL holds no plugins"
    while IFS= read -r f; do
        load_cmds="$(otool -L "$f")" || die "otool -L failed on ${f#"$app"/}"
        # Line 1 of `otool -L` is the file's own path, which may sit anywhere. Every
        # Mach-O here links at least libSystem, so no reference at all means otool
        # did not really read the file.
        [[ "$(printf '%s\n' "$load_cmds" | wc -l)" -ge 2 ]] \
            || die "otool -L shows no libraries for ${f#"$app"/}: $load_cmds"
        rpaths="$(rpaths_of "$f")" || die "otool -l failed on ${f#"$app"/}"
        hits="$( {
            printf '%s\n' "$load_cmds" | tail -n +2
            if [[ -n "$rpaths" ]]; then printf '%s\n' "$rpaths" | sed 's/^/LC_RPATH /'; fi
        } | grep -E "$FORBIDDEN_RE" || true)"
        if [[ -n "$hits" ]]; then
            echo "${f#"$app"/}:" >&2
            printf '%s\n' "$hits" | sed 's/^/    /' >&2
            bad=1
        fi
        checked=$((checked + 1))
    done <<<"$list"
    [[ $bad -eq 0 ]] || die "the bundle still references build-host libraries"
    say "OK: none of the $checked Mach-O files in the bundle references /opt/homebrew or /usr/local"
    # Notarization requires the hardened runtime on every executable and library.
    while IFS= read -r f; do
        sig="$(codesign -d --verbose=2 "$f" 2>&1)" || die "${f#"$app"/} is not signed: $sig"
        grep -qE '^CodeDirectory .*flags=0x[0-9a-f]+\([^)]*runtime' <<<"$sig" \
            || { echo "${f#"$app"/}: no hardened runtime" >&2; bad=1; }
    done <<<"$list"
    [[ $bad -eq 0 ]] || die "Mach-O files in the bundle are signed without the hardened runtime"
    say "OK: all $checked Mach-O files are signed with the hardened runtime"
    for req in $EXECUTABLES; do
        ents="$(codesign -d --entitlements - --xml "$app/Contents/$req" 2>/dev/null)" \
            || die "cannot read the entitlements of Contents/$req"
        for key in com.apple.security.device.camera com.apple.security.device.audio-input; do
            has_entitlement "$ents" "$key" || die "Contents/$req lacks the entitlement $key"
        done
    done
    say "OK: $APP_EXEC, $CLI_EXEC and strom carry the camera and microphone entitlements"
    for req in $EXECUTABLES "$SCANNER_REL"; do
        sig="$(codesign -d --verbose=2 "$app/Contents/$req" 2>&1)" \
            || die "Contents/$req is not signed: $sig"
        ents="$(codesign -d --entitlements - --xml "$app/Contents/$req" 2>/dev/null)" \
            || die "cannot read the entitlements of Contents/$req"
        adhoc=false
        if grep -qx 'Signature=adhoc' <<<"$sig"; then adhoc=true; fi
        if has_entitlement "$ents" "$LIBRARY_VALIDATION_KEY"; then
            $adhoc || die "Contents/$req is not signed ad hoc but carries $LIBRARY_VALIDATION_KEY"
        elif $adhoc; then
            die "Contents/$req is signed ad hoc but lacks $LIBRARY_VALIDATION_KEY"
        fi
        # ORC's JIT (#113): strom, which runs the GStreamer pipelines, and nothing else.
        if [[ "$req" == "$JIT_EXEC" ]]; then
            has_entitlement "$ents" "$JIT_KEY" || die "Contents/$req lacks $JIT_KEY"
        elif has_entitlement "$ents" "$JIT_KEY" any; then
            die "Contents/$req carries $JIT_KEY, which only $JIT_EXEC may have"
        fi
    done
    say "OK: $LIBRARY_VALIDATION_KEY is on the executables if and only if they are signed ad hoc"
    say "OK: $JIT_KEY is on $JIT_EXEC and on no other executable or gst-plugin-scanner"
    codesign --verify --deep --strict --verbose=2 "$app" || die "codesign does not accept $app"
    say "OK: codesign --verify --strict accepts the bundle"
}

# Follow symlinks to the file itself (macOS readlink has no -f before macOS 13).
real_path() {
    local p="$1" dir link
    while [[ -L "$p" ]]; do
        dir="$(cd "$(dirname "$p")" && pwd -P)"
        link="$(readlink "$p")"
        case "$link" in /*) p="$link" ;; *) p="$dir/$link" ;; esac
    done
    dir="$(cd "$(dirname "$p")" && pwd -P)"
    printf '%s/%s\n' "$dir" "$(basename "$p")"
}

# The build-host paths of the bundle's executables (open-live-ingest-app,
# open-live-ingest, strom and gst-plugin-scanner), set once they are known. dyld
# resolves @executable_path, and the rpaths of the image chain, from whichever of them
# loaded a library.
exec_origins=()

# Resolve a dependency of the build-host file `origin` (as it sat before copying) to
# a file on this machine. Prints nothing for a system library.
#   @executable_path/... : the origin's own directory, then each executable's.
#   @rpath/...           : the origin's own LC_RPATHs, then each executable's (dyld
#                          searches the rpaths of the whole chain of loaders; the
#                          libraries in between are not tracked, but Homebrew
#                          bottles use absolute install names, so this rarely matters).
# Anything it still cannot find is an error, never a guess.
resolve_dep() {
    local dep="$1" origin="$2" origin_dir rp cand rest dir loader loader_dir
    origin_dir="$(dirname "$origin")"
    case "$dep" in
        /usr/lib/* | /System/*) return 0 ;;
        @loader_path/*)
            cand="$origin_dir/${dep#*/}"
            [[ -e "$cand" ]] && { printf '%s\n' "$cand"; return 0; }
            ;;
        @executable_path/*)
            for dir in "$origin_dir" "${exec_origins[@]+"${exec_origins[@]}"}"; do
                [[ "$dir" == "$origin_dir" ]] || dir="$(dirname "$dir")"
                cand="$dir/${dep#*/}"
                [[ -e "$cand" ]] && { printf '%s\n' "$cand"; return 0; }
            done
            ;;
        @rpath/*)
            rest="${dep#@rpath/}"
            for loader in "$origin" "${exec_origins[@]+"${exec_origins[@]}"}"; do
                loader_dir="$(dirname "$loader")"
                while IFS= read -r rp; do
                    rp="${rp/#@loader_path/$loader_dir}"
                    rp="${rp/#@executable_path/$loader_dir}"
                    cand="$rp/$rest"
                    [[ -e "$cand" ]] && { printf '%s\n' "$cand"; return 0; }
                done < <(rpaths_of "$loader")
            done
            ;;
        /*)
            [[ -e "$dep" ]] && { printf '%s\n' "$dep"; return 0; }
            ;;
    esac
    die "cannot find $dep, needed by $origin"
}

if [[ "${1:-}" == "--check" ]]; then
    [[ $# -eq 2 ]] || die "usage: $0 --check <path/to/$APP_NAME>"
    check_bundle "$2"
    exit 0
fi

if [[ $# -ne 3 ]]; then
    echo "usage: $0 <aarch64-apple-darwin open-live-ingest binary> <aarch64-apple-darwin open-live-ingest-app binary> <output dir>" >&2
    echo "       $0 --check <path/to/$APP_NAME>" >&2
    exit 2
fi
ingest_bin="$1"
app_bin="$2"
out_dir="$3"

[[ "$(uname -s)" == "Darwin" ]] || die "this script runs on macOS only"
for tool in brew curl jq shasum otool install_name_tool codesign lipo file plutil sips; do
    command -v "$tool" >/dev/null 2>&1 || die "$tool not found"
done
for bin in "$ingest_bin" "$app_bin"; do
    [[ -f "$bin" ]] || die "$bin not found"
    lipo -archs "$bin" | grep -qw arm64 || die "$bin is not an arm64 Mach-O"
done

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd -P)"
icon_src="$root/$ICON_SRC_REL"
[[ -s "$icon_src" ]] || die "no app icon at $icon_src (app/icons/make-icon.py draws it)"
# sips reads .icns; a file it cannot read would leave the app with a blank icon.
sips -g pixelWidth "$icon_src" >/dev/null 2>&1 || die "$icon_src is not an icon macOS can read"

# The same pin Cargo.toml, build.rs and install.sh use: the strom-types tag.
strom_tag="$(sed -n 's/^strom-types = .*tag = "\([^"]*\)".*/\1/p' "$root/Cargo.toml")"
[[ "$strom_tag" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]] \
    || die "cannot read the strom-types tag from Cargo.toml (got '$strom_tag')"
# A floor, not a pin: Strom releases before 0.8.1 cannot be bundled (OL-57).
IFS=. read -r major minor patch <<<"${strom_tag#v}"
if (( major == 0 && (minor < 8 || (minor == 8 && patch < 1)) )); then
    die "Strom $strom_tag is pinned; the app bundle needs Strom 0.8.1 or later"
fi
app_version="$(sed -n 's/^version = "\([^"]*\)"/\1/p' "$root/Cargo.toml" | head -n 1)"
[[ -n "$app_version" ]] || die "cannot read the package version from Cargo.toml"

gst_prefix="$(brew --prefix gstreamer 2>/dev/null)" || true
[[ -n "$gst_prefix" && -d "$gst_prefix/lib/gstreamer-1.0" ]] \
    || die "no Homebrew GStreamer found; run: brew install gstreamer"
gst_scanner="$gst_prefix/libexec/gstreamer-1.0/gst-plugin-scanner"
[[ -x "$gst_scanner" ]] || die "no gst-plugin-scanner at $gst_scanner"

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

# --- Strom ----------------------------------------------------------------------
asset="strom-$strom_tag-macos-aarch64"
say "Strom $strom_tag ($asset), from the strom-types pin in Cargo.toml"
release_json="$work/release.json"
curl_auth=()
if [[ -n "${GH_TOKEN:-${GITHUB_TOKEN:-}}" ]]; then
    curl_auth=(-H "Authorization: Bearer ${GH_TOKEN:-${GITHUB_TOKEN:-}}")
fi
curl -fsSL "${curl_auth[@]+"${curl_auth[@]}"}" -H "Accept: application/vnd.github+json" \
    "https://api.github.com/repos/$STROM_REPO/releases/tags/$strom_tag" -o "$release_json" \
    || die "cannot read Strom release $strom_tag"
digest="$(jq -r --arg n "$asset" '.assets[] | select(.name == $n) | .digest // empty' "$release_json")"
url="$(jq -r --arg n "$asset" '.assets[] | select(.name == $n) | .browser_download_url' "$release_json")"
[[ -n "$url" ]] || die "Strom $strom_tag has no asset $asset"
[[ "$digest" == sha256:* ]] || die "GitHub records no SHA-256 digest for $asset"

strom_bin="$work/strom"
if [[ -n "${STROM_ASSET:-}" ]]; then
    cp "$STROM_ASSET" "$strom_bin"
else
    curl -fsSL -o "$strom_bin" "$url" || die "cannot download $url"
fi
got="$(shasum -a 256 "$strom_bin" | awk '{print $1}')"
[[ "sha256:$got" == "$digest" ]] || die "$asset: SHA-256 $got does not match the release ($digest)"
lipo -archs "$strom_bin" | grep -qw arm64 || die "$asset is not an arm64 Mach-O"

# --- Bundle skeleton ----------------------------------------------------------------
mkdir -p "$out_dir"
app="$(cd "$out_dir" && pwd -P)/$APP_NAME"
rm -rf "$app"
contents="$app/Contents"
mkdir -p "$contents/MacOS" "$contents/$FRAMEWORKS_REL" "$contents/$PLUGIN_DIR_REL" \
    "$contents/$(dirname "$SCANNER_REL")" "$contents/Resources"
say "Assembling $app"

install -m 0755 "$app_bin" "$contents/MacOS/$APP_EXEC"
install -m 0755 "$ingest_bin" "$contents/MacOS/$CLI_EXEC"
install -m 0755 "$strom_bin" "$contents/MacOS/strom"
install -m 0644 "$icon_src" "$contents/Resources/$ICON_NAME.icns"

min_version="${MACOS_MIN_VERSION:-$(sw_vers -productVersion | cut -d. -f1).0}"
cat >"$contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key>
    <string>Open Live Ingest</string>
    <key>CFBundleDisplayName</key>
    <string>Open Live Ingest</string>
    <key>CFBundleIdentifier</key>
    <string>${BUNDLE_ID:-com.eyevinn.open-live-ingest}</string>
    <key>CFBundleExecutable</key>
    <string>$APP_EXEC</string>
    <key>CFBundleIconFile</key>
    <string>$ICON_NAME</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleInfoDictionaryVersion</key>
    <string>6.0</string>
    <key>CFBundleShortVersionString</key>
    <string>$app_version</string>
    <key>CFBundleVersion</key>
    <string>$app_version</string>
    <key>LSMinimumSystemVersion</key>
    <string>$min_version</string>
    <key>LSApplicationCategoryType</key>
    <string>public.app-category.video</string>
    <key>NSHighResolutionCapable</key>
    <true/>
    <key>NSCameraUsageDescription</key>
    <string>Open Live Ingest sends the cameras you choose to Open Live.</string>
    <key>NSMicrophoneUsageDescription</key>
    <string>Open Live Ingest sends the audio inputs you choose to Open Live.</string>
</dict>
</plist>
EOF
plutil -lint "$contents/Info.plist" >/dev/null

# --- Plugins and the dylib closure ----------------------------------------------------
# origins: "<file in bundle>\t<build-host path it came from>", one line per Mach-O to walk.
# changes: "<file in bundle>\t<reference as written>\t<library name in Frameworks>".
origins="$work/origins.tsv"
changes="$work/changes.tsv"
: >"$origins"
: >"$changes"

{
    printf '%s\t%s\n' "$contents/MacOS/$APP_EXEC" "$(real_path "$app_bin")"
    printf '%s\t%s\n' "$contents/MacOS/$CLI_EXEC" "$(real_path "$ingest_bin")"
    printf '%s\t%s\n' "$contents/MacOS/strom" "$strom_bin"
} >>"$origins"

exclude=" ${GST_PLUGIN_EXCLUDE-libgstpython.dylib} "
plugin_count=0
for p in "$gst_prefix"/lib/gstreamer-1.0/*.dylib "$gst_prefix"/lib/gstreamer-1.0/*.so; do
    [[ -e "$p" ]] || continue
    name="$(basename "$p")"
    if [[ "$exclude" == *" $name "* ]]; then
        say "Leaving out plugin $name (GST_PLUGIN_EXCLUDE)"
        continue
    fi
    cp -L "$p" "$contents/$PLUGIN_DIR_REL/$name"
    printf '%s\t%s\n' "$contents/$PLUGIN_DIR_REL/$name" "$(real_path "$p")" >>"$origins"
    plugin_count=$((plugin_count + 1))
done
[[ $plugin_count -gt 0 ]] || die "no plugins found in $gst_prefix/lib/gstreamer-1.0"
cp -L "$gst_scanner" "$contents/$SCANNER_REL"
printf '%s\t%s\n' "$contents/$SCANNER_REL" "$(real_path "$gst_scanner")" >>"$origins"
exec_origins=("$(real_path "$app_bin")" "$(real_path "$ingest_bin")" "$strom_bin" "$(real_path "$gst_scanner")")
say "Copied $plugin_count GStreamer plugins and gst-plugin-scanner"

# Walk the closure breadth first. origins.tsv grows as libraries are copied in, so
# read it by line number.
line_no=0
while :; do
    line_no=$((line_no + 1))
    entry="$(sed -n "${line_no}p" "$origins")"
    [[ -n "$entry" ]] || break
    file="${entry%%$'\t'*}"
    origin="${entry#*$'\t'}"
    chmod u+w "$file"
    while IFS= read -r dep; do
        src="$(resolve_dep "$dep" "$origin")"
        [[ -n "$src" ]] || continue
        src="$(real_path "$src")"
        name="$(basename "$dep")"
        dest="$contents/$FRAMEWORKS_REL/$name"
        if [[ ! -e "$dest" ]]; then
            cp "$src" "$dest"
            chmod u+w "$dest"
            printf '%s\t%s\n' "$dest" "$src" >>"$origins"
        elif ! cmp -s "$src" "$dest"; then
            die "two different libraries are both called $name (one is $src)"
        fi
        printf '%s\t%s\t%s\n' "$file" "$dep" "$name" >>"$changes"
    done < <(deps_of "$file")
done
say "Bundled $(($(wc -l <"$origins") - plugin_count - 4)) libraries into $FRAMEWORKS_REL"

# --- Rewrite install names and rpaths ---------------------------------------------------
while IFS= read -r entry; do
    file="${entry%%$'\t'*}"
    case "$file" in
        "$contents/MacOS/"* | "$contents/$(dirname "$SCANNER_REL")/"*)
            rpath="@executable_path/../$FRAMEWORKS_REL" ;;
        "$contents/$PLUGIN_DIR_REL/"*) rpath="@loader_path/../../$FRAMEWORKS_REL" ;;
        *) rpath="@loader_path" ;;
    esac
    # One call per file: the shorter @rpath names free the header space the new
    # rpath needs.
    args=()
    if [[ -n "$(otool -D "$file" | sed -n '2p')" ]]; then
        args+=(-id "@rpath/$(basename "$file")")
    fi
    while IFS=$'\t' read -r f old name; do
        if [[ "$f" == "$file" ]]; then args+=(-change "$old" "@rpath/$name"); fi
    done <"$changes"
    keep=false
    while IFS= read -r old_rpath; do
        if [[ "$old_rpath" == "$rpath" ]]; then keep=true; else args+=(-delete_rpath "$old_rpath"); fi
    done < <(rpaths_of "$file" | sort -u)
    $keep || args+=(-add_rpath "$rpath")
    if [[ ${#args[@]} -gt 0 ]]; then
        install_name_tool "${args[@]}" "$file" </dev/null \
            2> >(grep -v 'invalidate the code signature' >&2)
    fi
done <"$origins"

# --- Signing ------------------------------------------------------------------------------
# Rewriting breaks the signatures; Apple silicon refuses to load unsigned code. Inside
# out, no --deep: every nested Mach-O first (libraries and plugins, gst-plugin-scanner,
# strom, the CLI), then the bundle. Signing the bundle signs its main executable
# (open-live-ingest-app) and seals the nested code, which must already be signed, so
# the app binary is never signed on its own. The bundle is signed with the
# entitlements too, since that signature is the main executable's. The CLI is not the
# main executable, but over SSH it is the process TCC sees, so it gets them as well.
entitlements="${MACOS_ENTITLEMENTS:-$root/.github/scripts/open-live-ingest.entitlements}"
[[ -f "$entitlements" ]] || die "no entitlements file at $entitlements"
plutil -lint "$entitlements" >/dev/null || die "$entitlements is not a valid plist"
plistbuddy=/usr/libexec/PlistBuddy
[[ -x "$plistbuddy" ]] || die "$plistbuddy not found"
scanner_sign_args=()
if [[ -n "${MACOS_SIGN_IDENTITY:-}" ]]; then
    say "Signing with the Developer ID identity (hardened runtime, secure timestamp)"
    sign_args=(--timestamp --sign "$MACOS_SIGN_IDENTITY")
    if [[ -n "${MACOS_SIGN_KEYCHAIN:-}" ]]; then sign_args+=(--keychain "$MACOS_SIGN_KEYCHAIN"); fi
else
    say "MACOS_SIGN_IDENTITY not set: signing AD HOC (hardened runtime, no timestamp)."
    say "An ad hoc signed app cannot be notarized, and Gatekeeper rejects it once downloaded."
    sign_args=(--sign -)
    say "Ad hoc: strom, $CLI_EXEC, $APP_EXEC and gst-plugin-scanner get $LIBRARY_VALIDATION_KEY."
    adhoc_entitlements="$work/adhoc.entitlements"
    cp "$entitlements" "$adhoc_entitlements"
    "$plistbuddy" -c "Delete :$LIBRARY_VALIDATION_KEY" "$adhoc_entitlements" >/dev/null 2>&1 || true
    "$plistbuddy" -c "Add :$LIBRARY_VALIDATION_KEY bool true" "$adhoc_entitlements" >/dev/null \
        || die "cannot add $LIBRARY_VALIDATION_KEY to $adhoc_entitlements"
    plutil -convert xml1 "$adhoc_entitlements" || die "$adhoc_entitlements is not a valid plist"
    entitlements="$adhoc_entitlements"
    scanner_entitlements="$work/adhoc-scanner.entitlements"
    cat >"$scanner_entitlements" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>$LIBRARY_VALIDATION_KEY</key>
    <true/>
</dict>
</plist>
EOF
    plutil -lint "$scanner_entitlements" >/dev/null || die "$scanner_entitlements is not a valid plist"
    scanner_sign_args=(--entitlements "$scanner_entitlements")
fi
# strom alone also gets ORC's JIT (#113): GStreamer's ORC compiles the inner loops of
# videoconvert, videoscale, audioconvert and the like to machine code at run time, and
# under the hardened runtime that needs com.apple.security.cs.allow-jit. Without it
# ORC logs "Failed to create write and exec mmap regions" and falls back to slower C.
# The key is added here, not in the file, so the app, the CLI and the scanner (which
# run no pipeline) never get it; --check enforces that.
strom_entitlements="$work/strom.entitlements"
cp "$entitlements" "$strom_entitlements"
"$plistbuddy" -c "Delete :$JIT_KEY" "$strom_entitlements" >/dev/null 2>&1 || true
"$plistbuddy" -c "Add :$JIT_KEY bool true" "$strom_entitlements" >/dev/null \
    || die "cannot add $JIT_KEY to $strom_entitlements"
plutil -convert xml1 "$strom_entitlements" || die "$strom_entitlements is not a valid plist"
sign() { codesign --force --options runtime "${sign_args[@]}" "$@" </dev/null; }
while IFS= read -r entry; do
    file="${entry%%$'\t'*}"
    case "$file" in "$contents/MacOS/"* | "$contents/$SCANNER_REL") continue ;; esac
    sign "$file"
done <"$origins"
sign "${scanner_sign_args[@]+"${scanner_sign_args[@]}"}" "$contents/$SCANNER_REL"
sign --entitlements "$strom_entitlements" "$contents/$JIT_EXEC"
sign --entitlements "$entitlements" "$contents/MacOS/$CLI_EXEC"
sign --entitlements "$entitlements" "$app"

# --- AC1 ------------------------------------------------------------------------------
check_bundle "$app"

say "Built $app (Strom $strom_tag)"
printf '%s\n' "$app"
