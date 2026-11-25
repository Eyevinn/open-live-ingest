#!/bin/bash
# Builds SRT.xcframework: libsrt with mbedTLS as its crypto, both from pinned, digest-checked
# source tarballs, for the iPhone (aarch64-apple-ios) and the simulator (aarch64-apple-ios-sim)
# (docs/specs/ios-app.md, sections 3 and 13; ADR-002 section 3). The output is not committed.
# macOS with Xcode and cmake only. From anywhere:
#
#   ios/Vendor/build-libsrt.sh [output-dir]     (default: the directory of this script)
#
# Both tarballs are downloaded and checked before anything is built; a SHA-256 that differs from
# the pin below stops the script with nothing written to the output directory.
#
# SRT.xcframework holds one static library per slice (libsrt and mbedTLS's libmbedcrypto merged),
# libsrt's public headers and a module map, so Swift says `import SRT`.
set -euo pipefail

# libsrt, at the version the cloud Strom's GStreamer links: Strom (Eyevinn/strom, its Dockerfile
# at v0.8.4, the strom-types tag pinned in Cargo.toml) runs on ubuntu:26.04 with the distro's
# gstreamer1.0-plugins-bad 1.28.2, whose srt plugin links libsrt1.5-gnutls 1.5.4-3
# (https://packages.ubuntu.com/resolute/libsrt1.5-gnutls). Move it with that image, on purpose.
SRT_VERSION=1.5.4
SRT_URL="https://github.com/Haivision/srt/archive/refs/tags/v${SRT_VERSION}.tar.gz"
SRT_SHA256=d0a8b600fe1b4eaaf6277530e3cfc8f15b8ce4035f16af4a5eb5d4b123640cdd

# mbedTLS, the 3.6 LTS branch, from the release tarball (it carries the generated sources, so
# no Python is needed). libsrt uses its AES-CTR, AES key wrap and PBKDF2 (libmbedcrypto only).
MBEDTLS_VERSION=3.6.8
MBEDTLS_URL="https://github.com/Mbed-TLS/mbedtls/releases/download/mbedtls-${MBEDTLS_VERSION}/mbedtls-${MBEDTLS_VERSION}.tar.bz2"
MBEDTLS_SHA256=bc54b818e87f49c0b3281dd85d24adaede0e63b4e822e3fbd49c005a38b9595b

# The app's minimum (docs/specs/ios-app.md, section 14 Q10).
IOS_DEPLOYMENT_TARGET=17.0

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
out="${1:-$here}"

for tool in curl shasum cmake xcrun xcodebuild libtool; do
  command -v "$tool" >/dev/null || { echo "build-libsrt.sh: $tool not found (macOS with Xcode and cmake)" >&2; exit 1; }
done

work="$(mktemp -d "${TMPDIR:-/tmp}/build-libsrt.XXXXXX")"
trap 'rm -rf "$work"' EXIT
jobs="$(sysctl -n hw.ncpu 2>/dev/null || echo 4)"

# fetch <url> <file> <sha256>
fetch() {
  local url="$1" file="$2" want="$3" got
  echo "== downloading $url"
  curl -fsSL --retry 3 -o "$file" "$url"
  got="$(shasum -a 256 "$file" | awk '{print $1}')"
  if [[ "$got" != "$want" ]]; then
    echo "build-libsrt.sh: $(basename "$file"): SHA-256 $got does not match the pinned $want" >&2
    exit 1
  fi
  echo "SHA-256 OK: $got"
}

fetch "$SRT_URL" "$work/srt.tar.gz" "$SRT_SHA256"
fetch "$MBEDTLS_URL" "$work/mbedtls.tar.bz2" "$MBEDTLS_SHA256"

mkdir -p "$work/src"
tar -xzf "$work/srt.tar.gz" -C "$work/src"
tar -xjf "$work/mbedtls.tar.bz2" -C "$work/src"
srt_src="$work/src/srt-$SRT_VERSION"
mbedtls_src="$work/src/mbedtls-$MBEDTLS_VERSION"
[[ -f "$srt_src/CMakeLists.txt" && -f "$mbedtls_src/CMakeLists.txt" ]] \
  || { echo "build-libsrt.sh: unexpected tarball layout" >&2; exit 1; }

# build_slice <sdk>: mbedTLS, then libsrt against it, merged into $work/<sdk>/libSRT.a.
# CMAKE_POLICY_VERSION_MINIMUM: libsrt's CMakeLists asks for CMake 2.8.12, which CMake 4 refuses.
build_slice() {
  local sdk="$1"
  local prefix="$work/$sdk/prefix"
  local common=(
    -DCMAKE_SYSTEM_NAME=iOS
    -DCMAKE_OSX_SYSROOT="$sdk"
    -DCMAKE_OSX_ARCHITECTURES=arm64
    -DCMAKE_OSX_DEPLOYMENT_TARGET="$IOS_DEPLOYMENT_TARGET"
    -DCMAKE_BUILD_TYPE=Release
    -DCMAKE_INSTALL_PREFIX="$prefix"
    -DCMAKE_INSTALL_LIBDIR=lib
    -DCMAKE_POLICY_VERSION_MINIMUM=3.5
  )

  echo "== mbedTLS $MBEDTLS_VERSION for $sdk"
  cmake -S "$mbedtls_src" -B "$work/$sdk/mbedtls" "${common[@]}" \
    -DENABLE_PROGRAMS=OFF -DENABLE_TESTING=OFF \
    -DUSE_STATIC_MBEDTLS_LIBRARY=ON -DUSE_SHARED_MBEDTLS_LIBRARY=OFF \
    -DMBEDTLS_FATAL_WARNINGS=OFF
  cmake --build "$work/$sdk/mbedtls" --parallel "$jobs"
  cmake --install "$work/$sdk/mbedtls"

  echo "== libsrt $SRT_VERSION for $sdk"
  cmake -S "$srt_src" -B "$work/$sdk/srt" "${common[@]}" \
    -DENABLE_APPS=OFF -DENABLE_SHARED=OFF -DENABLE_STATIC=ON \
    -DENABLE_ENCRYPTION=ON -DUSE_ENCLIB=mbedtls \
    -DSSL_INCLUDE_DIRS="$prefix/include" \
    -DSSL_LIBRARY_DIRS="$prefix/lib" \
    -DSSL_LIBRARIES="$prefix/lib/libmbedcrypto.a" \
    -DENABLE_BONDING=OFF -DENABLE_UNITTESTS=OFF -DENABLE_TESTING=OFF
  cmake --build "$work/$sdk/srt" --parallel "$jobs"
  cmake --install "$work/$sdk/srt"

  libtool -static -o "$work/$sdk/libSRT.a" "$prefix/lib/libsrt.a" "$prefix/lib/libmbedcrypto.a"
}

build_slice iphoneos
build_slice iphonesimulator

# One set of headers for both slices (the same source and version), and the module map.
headers="$work/headers"
mkdir -p "$headers"
cp "$work/iphoneos/prefix/include/srt/"*.h "$headers/"
cat >"$headers/module.modulemap" <<'EOF'
module SRT {
    header "srt.h"
    link "c++"
    export *
}
EOF

mkdir -p "$out"
rm -rf "$out/SRT.xcframework"
xcodebuild -create-xcframework \
  -library "$work/iphoneos/libSRT.a" -headers "$headers" \
  -library "$work/iphonesimulator/libSRT.a" -headers "$headers" \
  -output "$out/SRT.xcframework"
echo "== $out/SRT.xcframework: libsrt $SRT_VERSION, mbedTLS $MBEDTLS_VERSION, iOS $IOS_DEPLOYMENT_TARGET (device, simulator)"
