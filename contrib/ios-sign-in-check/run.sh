#!/bin/bash
# The simulator sign-in check (docs/specs/ios-app.md, sections 6 and 14 Q4; #138), as the
# CI job `ios-sign-in` runs it on macos-latest. macOS with Xcode only. From the repo root:
#
#   contrib/ios-sign-in-check/run.sh <result-dir>
#
# 1. builds the Rust core for the simulator, debug (only a debug build reads
#    OLI_TEST_OSC_URL), as the static library the harness links;
# 2. starts fake_osc.py on 127.0.0.1:8765;
# 3. runs the harness's UI test on the newest iPhone simulator: Sign in, allow the system
#    alert, the sheet goes to the fake's authorize page, which 302s to the core's
#    127.0.0.1 listener; osc_signed_in, cancel(), and nothing left on screen.
# The .xcresult (with the screenshots) and the logs go to <result-dir>.
set -euo pipefail

out="${1:?usage: run.sh <result-dir>}"
mkdir -p "$out"
here="contrib/ios-sign-in-check"
[[ -f "$here/fake_osc.py" ]] || { echo "run from the repository root"; exit 1; }

echo "== building the core for aarch64-apple-ios-sim (debug)"
export IPHONEOS_DEPLOYMENT_TARGET=16.0
cargo rustc -p open-live-ingest-mobile --target aarch64-apple-ios-sim --locked --color never \
  --crate-type staticlib -- --print native-static-libs 2>&1 | tee "$out/rustc.log"
libs="$(sed -n 's/^.*native-static-libs: //p' "$out/rustc.log" | tail -n 1)"
[[ -n "$libs" ]] || { echo "rustc printed no native-static-libs"; exit 1; }
lib="target/aarch64-apple-ios-sim/debug/libopen_live_ingest_mobile.a"
[[ -f "$lib" ]] || { echo "no $lib"; exit 1; }
echo "linking $lib with: $libs"

echo "== starting the fake Open Source Cloud"
python3 "$here/fake_osc.py" 8765 >"$out/fake-osc.log" 2>&1 &
fake=$!
trap 'kill "$fake" 2>/dev/null || true; echo "== fake Open Source Cloud log"; cat "$out/fake-osc.log"' EXIT
ready=false
for _ in $(seq 1 30); do
  if curl -fsS -m 2 http://127.0.0.1:8765/health >/dev/null 2>&1; then ready=true; break; fi
  sleep 1
done
$ready || { echo "the fake did not answer"; exit 1; }

echo "== picking a simulator"
udid="$(xcrun simctl list devices available --json | python3 -c '
import json, re, sys
best = None
for runtime, devices in json.load(sys.stdin)["devices"].items():
    m = re.search(r"iOS-(\d+)-(\d+)", runtime)
    if not m:
        continue
    version = (int(m[1]), int(m[2]))
    for d in devices:
        if d.get("isAvailable") and d["name"].startswith("iPhone"):
            if best is None or version > best[0]:
                best = (version, d["udid"], d["name"])
if best is None:
    sys.exit("no available iPhone simulator")
print("iOS %d.%d, %s" % (best[0] + (best[2],)), file=sys.stderr)
print(best[1])
')"

echo "== the sign-in check"
xcodebuild test \
  -project "$here/SignInCheck.xcodeproj" \
  -scheme SignInCheck \
  -destination "platform=iOS Simulator,id=$udid" \
  -resultBundlePath "$out/SignInCheck.xcresult" \
  OLI_RUST_NATIVE_LIBS="$libs" 2>&1 | tee "$out/xcodebuild.log"
grep -h "sign-in-check:" "$out/xcodebuild.log" || true
