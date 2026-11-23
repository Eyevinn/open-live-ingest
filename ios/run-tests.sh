#!/bin/bash
# The iOS app's tests on an iOS Simulator, as the CI job `ios` runs them on macos-latest
# (docs/specs/ios-app.md, section 12). macOS with Xcode, after ios/Vendor/build-libsrt.sh, with
# Homebrew's srt (`srt-live-transmit`) on the PATH. From the repo root:
#
#   ios/run-tests.sh <result-dir>
#
# 1. builds the app for a device, unsigned, which links SRT.xcframework's device slice;
# 2. builds the app and its tests for the newest iPhone simulator (build-for-testing);
# 3. starts the srt-live-transmit listeners SRTSenderTests.swift calls on 127.0.0.1;
# 4. runs the unit and UI tests (test-without-building).
# The .xcresult and the logs go to <result-dir>.
set -euo pipefail

out="${1:?usage: ios/run-tests.sh <result-dir>}"
mkdir -p "$out"
out="$(cd "$out" && pwd)"
project="ios/OpenLiveIngest.xcodeproj"
[[ -d "$project" ]] || { echo "run from the repository root"; exit 1; }
[[ -d ios/Vendor/SRT.xcframework ]] || { echo "no ios/Vendor/SRT.xcframework: run ios/Vendor/build-libsrt.sh first"; exit 1; }
command -v srt-live-transmit >/dev/null || { echo "srt-live-transmit not found (brew install srt)"; exit 1; }
derived="$out/DerivedData"

# run_logged <log> <command...>: the command's output to the terminal and <log>, its status kept.
run_logged() {
  local log="$1" rc
  shift
  set +e
  "$@" 2>&1 | tee "$log"
  rc="${PIPESTATUS[0]}"
  set -e
  [[ "$rc" == 0 ]] || echo "failed ($rc): see $log"
  return "$rc"
}

echo "== building for a device, unsigned (the device slice)"
run_logged "$out/build-device.log" xcodebuild build -quiet \
  -project "$project" -scheme OpenLiveIngest -configuration Release \
  -destination "generic/platform=iOS" -derivedDataPath "$derived" \
  CODE_SIGNING_ALLOWED=NO

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
destination="platform=iOS Simulator,id=$udid"

echo "== build-for-testing"
run_logged "$out/build-for-testing.log" xcodebuild build-for-testing -quiet \
  -project "$project" -scheme OpenLiveIngest -configuration Debug \
  -destination "$destination" -derivedDataPath "$derived"

# The listeners of SRTSenderTests.swift, on its ports, with a passphrase made for this run and
# handed to the tests (xcodebuild passes TEST_RUNNER_<name> to them as <name>).
pass="$(openssl rand -hex 16)"
export TEST_RUNNER_OLI_SRT_TEST_PASSPHRASE="$pass"
pids=()
listen() {
  local name="$1" port="$2" target="$3"
  srt-live-transmit -v "srt://:$port?mode=listener&passphrase=$pass&pbkeylen=16" "$target" \
    >/dev/null 2>"$out/srt-$name.log" &
  pids+=("$!")
}
cleanup() {
  for pid in "${pids[@]}"; do kill "$pid" 2>/dev/null || true; done
  for log in "$out"/srt-*.log; do
    [[ -f "$log" ]] || continue
    echo "== $(basename "$log") (tail)"
    tail -n 20 "$log"
  done
}
trap cleanup EXIT

echo "== starting the srt-live-transmit listeners"
listen delivery 4201 "udp://127.0.0.1:4301"
listen passphrase 4202 "file://con"
listen stats 4203 "file://con"
listen reconnect 4204 "file://con"
sleep 2
for pid in "${pids[@]}"; do
  kill -0 "$pid" 2>/dev/null || { echo "a listener exited at start"; exit 1; }
done

echo "== test-without-building"
run_logged "$out/test.log" xcodebuild test-without-building \
  -project "$project" -scheme OpenLiveIngest \
  -destination "$destination" -derivedDataPath "$derived" \
  -parallel-testing-enabled NO \
  -resultBundlePath "$out/OpenLiveIngest.xcresult"
