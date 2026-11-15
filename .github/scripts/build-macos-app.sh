#!/usr/bin/env bash
# Builds a self-contained "Open Live Ingest.app" for Apple silicon: the gateway, the
# Strom release it is pinned to, and the Homebrew GStreamer that Strom links against,
# with every library reference rewritten to point inside the bundle. Runs on macOS
# only. From the repository root:
#
#   cargo build --release --target aarch64-apple-darwin
#   .github/scripts/build-macos-app.sh target/aarch64-apple-darwin/release/open-live-ingest dist
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
#     Info.plist
#     MacOS/open-live-ingest, MacOS/strom
#     Frameworks/                                  GStreamer, GLib and the rest of the closure
#     Frameworks/gstreamer-1.0/                    GStreamer plugins
#     Frameworks/gstreamer-1.0/gst-plugin-scanner
#     Resources/
#
# Every non-system reference becomes @rpath/<name>. The rpath is
# @executable_path/../Frameworks for MacOS/*, @loader_path for Frameworks/* and
# @loader_path/.. for the plugins and the scanner. Rpaths the Homebrew builds carried
# are removed. The last step fails if `otool -L` (or an LC_RPATH) of any Mach-O in
# the bundle still names /opt/homebrew or /usr/local.
#
# Signing. Every rewritten Mach-O is signed ad hoc, which Apple silicon requires to
# load it at all, so an unsigned run gives a bundle that works on the build Mac.
# Set MACOS_SIGN_IDENTITY (a "Developer ID Application: ..." identity in a keychain
# the caller has unlocked) to sign everything with it instead, with the hardened
# runtime and a timestamp, ready for notarization. MACOS_ENTITLEMENTS overrides the
# entitlements file used for the two executables (the default allows camera and
# microphone access). Notarizing and stapling is left to the caller.
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
PLUGIN_DIR_REL="Frameworks/gstreamer-1.0"
FORBIDDEN_RE='/opt/homebrew|/usr/local'

die() { echo "error: $*" >&2; exit 1; }
say() { echo "==> $*" >&2; }

is_macho() { file -b "$1" | grep -q 'Mach-O'; }

# Every Mach-O file under a directory, one per line.
macho_files() {
    local f
    find "$1" -type f | while IFS= read -r f; do
        if is_macho "$f"; then printf '%s\n' "$f"; fi
    done
}

# The libraries a Mach-O loads, without its own install name.
deps_of() {
    local own
    own="$(otool -D "$1" | sed -n '2p')"
    otool -L "$1" | tail -n +2 | awk '{print $1}' | while IFS= read -r d; do
        if [[ "$d" != "$own" ]]; then printf '%s\n' "$d"; fi
    done
}

rpaths_of() {
    otool -l "$1" | awk '/cmd LC_RPATH/ {r = 1} r && $1 == "path" {print $2; r = 0}'
}

# AC1: fail on any Mach-O whose load commands still name a build-host prefix.
check_bundle() {
    local app="$1" f bad=0 hits
    [[ -d "$app/Contents" ]] || die "$app is not an app bundle"
    say "Checking every Mach-O in $app for $FORBIDDEN_RE"
    while IFS= read -r f; do
        # Line 1 of `otool -L` is the file's own path, which may sit anywhere.
        hits="$( { otool -L "$f" | tail -n +2; rpaths_of "$f" | sed 's/^/LC_RPATH /'; } \
            | grep -E "$FORBIDDEN_RE" || true)"
        if [[ -n "$hits" ]]; then
            echo "${f#"$app"/}:" >&2
            printf '%s\n' "$hits" | sed 's/^/    /' >&2
            bad=1
        fi
    done < <(macho_files "$app")
    [[ $bad -eq 0 ]] || die "the bundle still references build-host libraries"
    say "OK: no Mach-O in the bundle references /opt/homebrew or /usr/local"
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

# Resolve a dependency of the build-host file `origin` (as it sat before copying) to
# a file on this machine. Prints nothing for a system library.
resolve_dep() {
    local dep="$1" origin="$2" origin_dir rp cand rest
    origin_dir="$(dirname "$origin")"
    case "$dep" in
        /usr/lib/* | /System/*) return 0 ;;
        @loader_path/* | @executable_path/*)
            cand="$origin_dir/${dep#*/}"
            [[ -e "$cand" ]] && { printf '%s\n' "$cand"; return 0; }
            ;;
        @rpath/*)
            rest="${dep#@rpath/}"
            while IFS= read -r rp; do
                rp="${rp/#@loader_path/$origin_dir}"
                rp="${rp/#@executable_path/$origin_dir}"
                cand="$rp/$rest"
                [[ -e "$cand" ]] && { printf '%s\n' "$cand"; return 0; }
            done < <(rpaths_of "$origin")
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

if [[ $# -ne 2 ]]; then
    echo "usage: $0 <aarch64-apple-darwin open-live-ingest binary> <output dir>" >&2
    echo "       $0 --check <path/to/$APP_NAME>" >&2
    exit 2
fi
ingest_bin="$1"
out_dir="$2"

[[ "$(uname -s)" == "Darwin" ]] || die "this script runs on macOS only"
for tool in brew curl jq shasum otool install_name_tool codesign lipo file plutil; do
    command -v "$tool" >/dev/null 2>&1 || die "$tool not found"
done
[[ -f "$ingest_bin" ]] || die "$ingest_bin not found"
lipo -archs "$ingest_bin" | grep -qw arm64 || die "$ingest_bin is not an arm64 Mach-O"

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd -P)"

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
mkdir -p "$contents/MacOS" "$contents/$PLUGIN_DIR_REL" "$contents/Resources"
say "Assembling $app"

install -m 0755 "$ingest_bin" "$contents/MacOS/open-live-ingest"
install -m 0755 "$strom_bin" "$contents/MacOS/strom"

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
    <string>open-live-ingest</string>
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

printf '%s\t%s\n' "$contents/MacOS/open-live-ingest" "$(real_path "$ingest_bin")" >>"$origins"
printf '%s\t%s\n' "$contents/MacOS/strom" "$strom_bin" >>"$origins"

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
cp -L "$gst_scanner" "$contents/$PLUGIN_DIR_REL/gst-plugin-scanner"
printf '%s\t%s\n' "$contents/$PLUGIN_DIR_REL/gst-plugin-scanner" "$(real_path "$gst_scanner")" >>"$origins"
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
say "Bundled $(($(wc -l <"$origins") - plugin_count - 3)) libraries into $FRAMEWORKS_REL"

# --- Rewrite install names and rpaths ---------------------------------------------------
while IFS= read -r entry; do
    file="${entry%%$'\t'*}"
    case "$file" in
        "$contents/MacOS/"*) rpath="@executable_path/../$FRAMEWORKS_REL" ;;
        "$contents/$PLUGIN_DIR_REL/"*) rpath="@loader_path/.." ;;
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

# Rewriting breaks the signatures; Apple silicon refuses to load unsigned code.
while IFS= read -r entry; do
    codesign --force --sign - "${entry%%$'\t'*}" </dev/null
done <"$origins"

# --- AC1 ------------------------------------------------------------------------------
check_bundle "$app"

# --- Optional signing ---------------------------------------------------------------------
if [[ -n "${MACOS_SIGN_IDENTITY:-}" ]]; then
    entitlements="${MACOS_ENTITLEMENTS:-$work/entitlements.plist}"
    if [[ -z "${MACOS_ENTITLEMENTS:-}" ]]; then
        cat >"$entitlements" <<'EOF'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>com.apple.security.device.camera</key>
    <true/>
    <key>com.apple.security.device.audio-input</key>
    <true/>
</dict>
</plist>
EOF
    fi
    say "Signing with $MACOS_SIGN_IDENTITY (hardened runtime)"
    sign() { codesign --force --timestamp --options runtime --sign "$MACOS_SIGN_IDENTITY" "$@"; }
    # Inside out: libraries and plugins, then the executables, then the bundle.
    while IFS= read -r entry; do
        file="${entry%%$'\t'*}"
        case "$file" in "$contents/MacOS/"*) continue ;; esac
        sign "$file" </dev/null
    done <"$origins"
    sign --entitlements "$entitlements" "$contents/MacOS/strom"
    sign --entitlements "$entitlements" "$contents/MacOS/open-live-ingest"
    sign --entitlements "$entitlements" "$app"
    codesign --verify --deep --strict "$app"
else
    say "MACOS_SIGN_IDENTITY not set: the bundle is signed ad hoc only"
fi

say "Built $app (Strom $strom_tag)"
printf '%s\n' "$app"
