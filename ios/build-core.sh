#!/bin/bash
# Builds the Rust core (mobile/, docs/specs/ios-app.md, section 8.1) as the static library the app
# links: the Xcode project's "Build the Rust core" phase runs it before compiling the Swift. Debug
# for a Debug build (only a debug core reads the UI tests' OLI_TEST_OSC_URL), release for Release;
# aarch64-apple-ios for a device, aarch64-apple-ios-sim for the simulator. The library lands in
# target/<triple>/<debug|release>/libopen_live_ingest_mobile.a, where OLI_RUST_LIB points.
#
# cargo runs with a clean environment: Xcode's SDKROOT and friends point at the iOS SDK, which
# would break the host build scripts of the dependencies. Needs cargo (rustup) with the two iOS
# targets: rustup target add aarch64-apple-ios aarch64-apple-ios-sim
set -euo pipefail

: "${SRCROOT:?run from Xcode}" "${PLATFORM_NAME:?run from Xcode}" "${CONFIGURATION:?run from Xcode}"
root="$(cd "$SRCROOT/.." && pwd)"
case "$PLATFORM_NAME" in
  iphoneos) triple=aarch64-apple-ios ;;
  iphonesimulator) triple=aarch64-apple-ios-sim ;;
  *) echo "error: build-core.sh: no Rust target for $PLATFORM_NAME"; exit 1 ;;
esac
profile=()
[[ "$CONFIGURATION" == Release ]] && profile=(--release)

path="$PATH:$HOME/.cargo/bin"
cargo="$(PATH="$path" command -v cargo || true)"
[[ -n "$cargo" ]] || { echo "error: build-core.sh: cargo not found (install Rust with rustup)"; exit 1; }

keep=(HOME="$HOME" PATH="$path" IPHONEOS_DEPLOYMENT_TARGET="${IPHONEOS_DEPLOYMENT_TARGET:-17.0}")
for name in DEVELOPER_DIR CARGO_HOME RUSTUP_HOME RUSTUP_TOOLCHAIN CARGO_INCREMENTAL CARGO_TERM_COLOR \
  RUSTC_WRAPPER SCCACHE_DIR TMPDIR; do
  if [[ -n "${!name:-}" ]]; then keep+=("$name=${!name}"); fi
done

echo "building the Rust core for $triple (${CONFIGURATION})"
cd "$root"
env -i "${keep[@]}" "$cargo" build -p open-live-ingest-mobile --lib --locked --target "$triple" \
  ${profile[@]+"${profile[@]}"}
