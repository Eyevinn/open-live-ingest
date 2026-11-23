#!/bin/bash
# The iOS app's tests on an iOS Simulator, as the CI job `ios` runs them on macos-latest
# (docs/specs/ios-app.md, section 12). macOS with Xcode, after ios/Vendor/build-libsrt.sh, with
# Homebrew's srt (`srt-live-transmit`) and ffmpeg (`ffprobe`) on the PATH, and Rust with the
# aarch64-apple-ios and aarch64-apple-ios-sim targets (the app's "Build the Rust core" phase runs
# ios/build-core.sh). From the repo root:
#
#   ios/run-tests.sh <result-dir>
#
# 1. builds the app for a device, unsigned, which links SRT.xcframework's device slice and the
#    release core;
# 2. builds the app and its tests for the newest iPhone simulator (build-for-testing), with the
#    debug core; both builds take the workspace version (Cargo.toml) as CFBundleShortVersionString;
#    the device build's Info.plist is checked for an empty UIBackgroundModes (section 9);
# 3. starts the srt-live-transmit listeners SRTSenderTests.swift and MediaPipelineTests.swift call
#    on 127.0.0.1, and for the UI tests (ScreensUITests.swift) the fake Open Source Cloud and Open
#    Live (mobile/examples/fake_servers.rs) and a listener standing in for the cloud Strom;
# 4. runs the unit and UI tests (test-without-building);
# 5. checks the 10 s test-pattern recording and the same stream received over SRT, which
#    MediaPipelineTests.swift writes, with ffprobe and ios/check-stream.py (section 11's stream
#    shape, and the peak-to-average bitrate of section 16).
# The .xcresult, the logs, the recordings and the report (stream-checks.md) go to <result-dir>.
set -euo pipefail

out="${1:?usage: ios/run-tests.sh <result-dir>}"
mkdir -p "$out"
out="$(cd "$out" && pwd)"
project="ios/OpenLiveIngest.xcodeproj"
[[ -d "$project" ]] || { echo "run from the repository root"; exit 1; }
[[ -d ios/Vendor/SRT.xcframework ]] || { echo "no ios/Vendor/SRT.xcframework: run ios/Vendor/build-libsrt.sh first"; exit 1; }
command -v srt-live-transmit >/dev/null || { echo "srt-live-transmit not found (brew install srt)"; exit 1; }
command -v ffprobe >/dev/null || { echo "ffprobe not found (brew install ffmpeg)"; exit 1; }
derived="$out/DerivedData"
command -v cargo >/dev/null || { echo "cargo not found (install Rust with rustup)"; exit 1; }
# One version for the phone, the desktop app and the CLI (docs/specs/ios-app.md, section 3).
version="$(sed -n 's/^version = "\([^"]*\)"$/\1/p' Cargo.toml | head -n 1)"
[[ -n "$version" ]] || { echo "no workspace version in Cargo.toml"; exit 1; }
echo "workspace version $version"

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
  CODE_SIGNING_ALLOWED=NO MARKETING_VERSION="$version"

# No background modes (docs/specs/ios-app.md, section 9): the built app's Info.plist has
# UIBackgroundModes empty or not at all. AppInfoTests.swift checks the simulator build's too.
plist="$derived/Build/Products/Release-iphoneos/OpenLiveIngest.app/Info.plist"
[[ -f "$plist" ]] || { echo "no Info.plist in the device build at $plist"; exit 1; }
python3 -c '
import plistlib, sys
modes = plistlib.load(open(sys.argv[1], "rb")).get("UIBackgroundModes", [])
sys.exit("UIBackgroundModes is not empty in the built app: %r" % (modes,) if modes else 0)
' "$plist"
echo "UIBackgroundModes: none, in the built app's Info.plist"

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
xcrun simctl boot "$udid" 2>/dev/null || true

echo "== build-for-testing"
run_logged "$out/build-for-testing.log" xcodebuild build-for-testing -quiet \
  -project "$project" -scheme OpenLiveIngest -configuration Debug \
  -destination "$destination" -derivedDataPath "$derived" MARKETING_VERSION="$version"

echo "== waiting for the simulator to finish booting"
xcrun simctl bootstatus "$udid" -b

# The listeners of SRTSenderTests.swift and MediaPipelineTests.swift, on their ports, with a passphrase made for this run and
# handed to the tests (xcodebuild passes TEST_RUNNER_<name> to them as <name>).
pass="$(openssl rand -hex 16)"
export TEST_RUNNER_OLI_SRT_TEST_PASSPHRASE="$pass"
recordings="$out/recordings"
mkdir -p "$recordings"
export TEST_RUNNER_OLI_RECORDING_DIR="$recordings"
pids=()
listen() {
  local name="$1" port="$2" target="$3"
  srt-live-transmit "srt://:$port?mode=listener&passphrase=$pass&pbkeylen=16" "$target" \
    >/dev/null 2>"$out/srt-$name.log" &
  pids+=("$!")
}
cleanup() {
  for pid in "${pids[@]}"; do kill "$pid" 2>/dev/null || true; done
  if [[ -f "$out/fake-servers.log" ]]; then
    echo "== fake-servers.log (tail)"
    tail -n 20 "$out/fake-servers.log"
  fi
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
listen encoded 4205 "udp://127.0.0.1:4305"
# The UI tests' cloud Strom: the fake Open Live publishes 127.0.0.1 and these ports, and the
# first source registered gets the first one. No passphrase, as the UI test sets none.
srt-live-transmit "srt://:47250?mode=listener" "file://con" >/dev/null 2>"$out/srt-uplink.log" &
pids+=("$!")
sleep 2
for pid in "${pids[@]}"; do
  kill -0 "$pid" 2>/dev/null || { echo "a listener exited at start"; exit 1; }
done

echo "== starting the fake Open Source Cloud and Open Live (mobile/examples/fake_servers.rs)"
run_logged "$out/fake-servers-build.log" cargo build -p open-live-ingest-mobile --features fake-servers \
  --example fake_servers --locked
urls="$out/fake-servers.json"
rm -f "$urls"
target/debug/examples/fake_servers "$urls" >"$out/fake-servers.log" 2>&1 &
pids+=("$!")
for _ in $(seq 1 30); do
  [[ -s "$urls" ]] && break
  sleep 1
done
[[ -s "$urls" ]] || { echo "the fake servers did not start"; exit 1; }
fake() { python3 -c 'import json, sys; print(json.load(open(sys.argv[1]))[sys.argv[2]])' "$urls" "$1"; }
export TEST_RUNNER_OLI_FAKE_OSC_URL="$(fake osc)"
export TEST_RUNNER_OLI_FAKE_CONTROL_URL="$(fake control)"
curl -fsS -m 5 "$TEST_RUNNER_OLI_FAKE_CONTROL_URL/health" >/dev/null || { echo "the fake servers do not answer"; exit 1; }
echo "fake Open Source Cloud at $TEST_RUNNER_OLI_FAKE_OSC_URL, control at $TEST_RUNNER_OLI_FAKE_CONTROL_URL"

echo "== test-without-building"
tests=0
run_logged "$out/test.log" xcodebuild test-without-building \
  -project "$project" -scheme OpenLiveIngest \
  -destination "$destination" -derivedDataPath "$derived" \
  -parallel-testing-enabled NO \
  -resultBundlePath "$out/OpenLiveIngest.xcresult" || tests=$?

# The recordings: where the tests wrote them, or else the test host's temporary directory. Checked
# even when a test failed, so the report says what the stream looked like.
container="$(xcrun simctl get_app_container "$udid" com.eyevinn.open-live-ingest.ios data 2>/dev/null || true)"
echo "== checking the streams with $(ffprobe -version | head -n 1)"
report="$out/stream-checks.md"
: >"$report"
failed=0
for name in test-pattern.ts received-over-srt.ts; do
  file="$recordings/$name"
  if [[ ! -s "$file" && -n "$container" && -s "$container/tmp/$name" ]]; then
    cp "$container/tmp/$name" "$file"
  fi
  if [[ ! -s "$file" ]]; then
    echo "no $name from the tests"
    failed=1
    continue
  fi
  GITHUB_STEP_SUMMARY="$report" python3 ios/check-stream.py "$file" || failed=1
done
if [[ -n "${GITHUB_STEP_SUMMARY:-}" ]]; then
  { echo "## OL-64d stream checks"; echo; cat "$report"; } >>"$GITHUB_STEP_SUMMARY"
fi
[[ "$failed" == 0 ]] || { echo "the stream checks failed: see $report"; exit 1; }
[[ "$tests" == 0 ]] || { echo "the tests failed ($tests): see $out/test.log"; exit "$tests"; }
