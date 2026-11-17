#!/usr/bin/env bash
# End-to-end check that a device source with a paired audio device carries sound to
# Open Live (OL-61b, #70; #64 AC6), with virtual devices so it runs on a hosted
# Linux runner. The e2e-audio-device workflow runs it; from the repository root:
#
#   cargo build
#   OSC_ACCESS_TOKEN=... .github/scripts/e2e-audio-device.sh target/debug/open-live-ingest
#
# What it does, in order:
#   1. A virtual camera: the v4l2loopback kernel module, fed a test pattern by
#      gst-launch. Without the module there is no device source to test, so the run
#      fails and names the module; it does not fall back to the test pattern, which
#      `up --test` already covers and which is not a device.
#   2. A virtual microphone: a PulseAudio null sink fed a tone, remapped into a plain
#      capture source (a monitor source would also do, but a real source is what a
#      USB microphone looks like to Strom's discovery).
#   3. The pinned Strom (the strom-types tag in Cargo.toml, checked against the
#      SHA-256 digest GitHub records for the asset), started here so its discovery
#      has seen both devices before the gateway asks. The gateway adopts it.
#   4. `up --all --devices CAMERA --audio CAMERA=MICROPHONE` against the Open Live
#      instance OPEN_LIVE_INSTANCE (default openlivedev) in the token's OSC workspace,
#      with a gateway name unique to the run.
#   5. Checks, each logged and written to the step summary:
#      - the source is registered in Open Live under the gateway's name;
#      - the flow Strom runs for it captures `audio_video` from the microphone and
#        declares one audio track on the uplink;
#      - audio buffers reach the uplink's audio input (A0): a Strom buffer probe on
#        the uplink block counts them;
#      - what Open Live itself says about the source's audio
#        (GET /api/v1/sources/{id}/audio-streams). Open Live answers
#        `discovery: unavailable` until it can probe a source's streams; that is
#        reported as the missing Open Live capability, not as a failure here.
#   6. Ctrl-C equivalent (SIGTERM) to `up`, then a check that the source is gone from
#      Open Live. Whatever happens, the exit trap stops `up`, runs `down`, and deletes
#      any source still carrying the run's gateway name.
#
# Needs: sudo (apt-get, modprobe), curl, jq, gh (GH_TOKEN, to read the Strom
# release), and OSC_ACCESS_TOKEN, an Open Source Cloud personal access token for the
# workspace that owns the instance. The token, the service token and the instance
# address are masked in the log; nothing here prints a response body that can carry
# them.

set -euo pipefail

gateway_bin="$(realpath "${1:?usage: $0 <path to open-live-ingest>}")"
root="$(cd "$(dirname "$0")/../.." && pwd)"
work="${RUNNER_TEMP:-$(mktemp -d)}/oli-e2e"
mkdir -p "$work"

instance="${OPEN_LIVE_INSTANCE:-openlivedev}"
# openlivedev publishes no SRT port range (its port reservation is off), so the
# settings name one. Nothing listens on the cloud side until a production is
# activated, which this run does not do; the uplink keeps retrying, as it does at a
# venue before the show.
port_range="${E2E_PORT_RANGE:-47290-47299}"
strom_port="${E2E_STROM_PORT:-18080}"
strom_url="http://127.0.0.1:$strom_port"
gateway_name="oli-e2e-${GITHUB_RUN_ID:-local}-${GITHUB_RUN_ATTEMPT:-$$}"
camera_name="OLI E2E Camera"
camera_nr=42
camera_dev="/dev/video$camera_nr"
mic_name="OLI-E2E-Microphone"
summary="${GITHUB_STEP_SUMMARY:-/dev/null}"

say() { printf '==> %s\n' "$*"; }
mask() { if [[ -n "${GITHUB_ACTIONS:-}" ]]; then echo "::add-mask::$1"; fi; }
record() { printf '%s\n' "$*" >> "$summary"; }
pass() { say "PASS: $*"; record "- PASS: $*"; }
note() { say "NOTE: $*"; record "- NOTE: $*"; }
fail() {
    echo "::error title=e2e audio device::$*"
    record "- FAIL: $*"
    exit 1
}

pids=()
up_pid=""
open_live_url=""
sat=""

cleanup() {
    local status=$?
    set +e
    if [[ -n "$up_pid" ]] && kill -0 "$up_pid" 2>/dev/null; then
        kill -TERM "$up_pid"
        for _ in $(seq 60); do kill -0 "$up_pid" 2>/dev/null || break; sleep 1; done
    fi
    if [[ -f "$work/gateway.toml" ]]; then
        "$gateway_bin" --config "$work/gateway.toml" down >/dev/null 2>&1
    fi
    # Last resort, so a failed run never leaves a source behind on the instance.
    if [[ -n "$open_live_url" && -n "$sat" ]]; then
        local id
        for id in $(our_source_ids 2>/dev/null); do
            say "removing leftover source $id"
            curl -fsS -o /dev/null -X DELETE -H "Authorization: Bearer $sat" \
                "$open_live_url/api/v1/sources/$id"
        done
    fi
    for pid in "${pids[@]}"; do kill "$pid" 2>/dev/null; done
    if [[ -f "$work/strom/strom.log" && $status -ne 0 ]]; then
        echo "::group::Strom log (tail)"
        tail -n 100 "$work/strom/strom.log"
        echo "::endgroup::"
    fi
    if [[ -f "$work/up.log" && $status -ne 0 ]]; then
        echo "::group::open-live-ingest up log"
        cat "$work/up.log"
        echo "::endgroup::"
    fi
    exit "$status"
}
trap cleanup EXIT

# --- credentials and instance ------------------------------------------------------

[[ -n "${OSC_ACCESS_TOKEN:-}" ]] \
    || fail "missing capability: the repository secret OSC_ACCESS_TOKEN (an Open Source Cloud personal access token for the workspace that owns the Open Live instance '$instance') is not set"
mask "$OSC_ACCESS_TOKEN"

service_token() {
    curl -fsS -X POST https://token.svc.prod.osaas.io/servicetoken \
        -H "x-pat-jwt: Bearer $OSC_ACCESS_TOKEN" -H 'content-type: application/json' \
        -d '{"serviceId":"eyevinn-open-live"}' | jq -r '.token // empty'
}

say "Resolving the Open Live instance '$instance' in the token's workspace"
sat="$(service_token)" || true
[[ -n "$sat" ]] || fail "missing capability: OSC_ACCESS_TOKEN was refused by the OSC token service (expired, or not a personal access token)"
mask "$sat"
api_url="$(curl -fsS -H "x-pat-jwt: Bearer $OSC_ACCESS_TOKEN" https://catalog.svc.prod.osaas.io/mysubscriptions \
    | jq -r '.[] | select(.serviceId == "eyevinn-open-live") | .apiUrl' | head -n 1)"
[[ -n "$api_url" ]] || fail "missing capability: the OSC_ACCESS_TOKEN workspace has no Open Live subscription"
open_live_url="$(curl -fsS -H "x-jwt: Bearer $sat" "$api_url" \
    | jq -r --arg n "$instance" '.[] | select(.name == $n) | .url' | head -n 1)"
[[ -n "$open_live_url" ]] || fail "missing capability: the OSC_ACCESS_TOKEN workspace has no Open Live instance named '$instance'"
open_live_url="${open_live_url%/}"
mask "$open_live_url"
mask "${open_live_url#https://}"

ol_get() { curl -fsS -H "Authorization: Bearer $sat" "$open_live_url$1"; }
our_source_ids() {
    ol_get /api/v1/sources | jq -r --arg p "$gateway_name — " '.[] | select(.name | startswith($p)) | .id'
}

# --- system packages ----------------------------------------------------------------

say "Installing GStreamer, PulseAudio and the v4l2loopback module"
sudo apt-get update -q
sudo DEBIAN_FRONTEND=noninteractive apt-get install -y -q \
    gstreamer1.0-tools gstreamer1.0-plugins-base gstreamer1.0-plugins-good \
    gstreamer1.0-plugins-bad gstreamer1.0-plugins-ugly gstreamer1.0-libav \
    gstreamer1.0-nice gstreamer1.0-gl gstreamer1.0-pulseaudio libcairo2 \
    pulseaudio pulseaudio-utils v4l-utils

# --- 1. virtual camera ----------------------------------------------------------------

load_loopback() {
    sudo modprobe v4l2loopback devices=1 video_nr="$camera_nr" \
        card_label="$camera_name" exclusive_caps=1
}
kernel="$(uname -r)"
if ! load_loopback 2>/dev/null; then
    say "v4l2loopback is not in the running kernel's modules; trying linux-modules-extra-$kernel"
    if ! { sudo DEBIAN_FRONTEND=noninteractive apt-get install -y -q "linux-modules-extra-$kernel" && load_loopback; }; then
        say "trying v4l2loopback-dkms"
        sudo DEBIAN_FRONTEND=noninteractive apt-get install -y -q "linux-headers-$kernel" v4l2loopback-dkms || true
        load_loopback || fail "missing capability: this runner cannot load the v4l2loopback kernel module (kernel $kernel; neither linux-modules-extra-$kernel nor v4l2loopback-dkms gave a loadable module), so there is no virtual camera"
    fi
fi
for _ in $(seq 20); do [[ -e "$camera_dev" ]] && break; sleep 0.5; done
[[ -e "$camera_dev" ]] || fail "v4l2loopback loaded but $camera_dev did not appear"
# The runner user is not in the video group.
sudo chmod 0666 "$camera_dev"
gst-launch-1.0 -q videotestsrc is-live=true pattern=smpte \
    ! video/x-raw,format=YUY2,width=1280,height=720,framerate=25/1 \
    ! v4l2sink device="$camera_dev" > "$work/camera.log" 2>&1 &
pids+=("$!")
pass "virtual camera '$camera_name' at $camera_dev (v4l2loopback, kernel $kernel), fed a test pattern"

# --- 2. virtual microphone -------------------------------------------------------------

export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"
if [[ ! -d "$XDG_RUNTIME_DIR" ]]; then
    sudo mkdir -p "$XDG_RUNTIME_DIR"
    sudo chown "$(id -u):$(id -g)" "$XDG_RUNTIME_DIR"
    chmod 0700 "$XDG_RUNTIME_DIR"
fi
pulseaudio --check 2>/dev/null || pulseaudio --start --exit-idle-time=-1 \
    || fail "missing capability: PulseAudio does not start on this runner, so there is no virtual audio device"
for _ in $(seq 20); do pactl info >/dev/null 2>&1 && break; sleep 0.5; done
pactl load-module module-null-sink sink_name=oli_e2e_sink \
    sink_properties=device.description=OLI-E2E-Sink >/dev/null
pactl load-module module-remap-source master=oli_e2e_sink.monitor source_name=oli_e2e_mic \
    source_properties=device.description="$mic_name" >/dev/null
gst-launch-1.0 -q audiotestsrc is-live=true wave=sine freq=440 \
    ! audio/x-raw,rate=48000,channels=2 \
    ! pulsesink device=oli_e2e_sink > "$work/microphone.log" 2>&1 &
pids+=("$!")
pass "virtual microphone '$mic_name' (PulseAudio null sink remapped to a source), fed a 440 Hz tone"

# --- 3. the pinned Strom -----------------------------------------------------------------

strom_tag="$(sed -n 's/^strom-types = .*tag = "\([^"]*\)".*/\1/p' "$root/Cargo.toml")"
[[ "$strom_tag" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]] \
    || fail "cannot read the strom-types tag from Cargo.toml (got '$strom_tag')"
asset="strom-$strom_tag-linux-x86_64"
say "Downloading Strom $strom_tag ($asset)"
want="$(gh api "repos/Eyevinn/strom/releases/tags/$strom_tag" \
    --jq ".assets[] | select(.name == \"$asset\") | .digest" | sed 's/^sha256://')"
[[ -n "$want" ]] || fail "Strom $strom_tag has no asset $asset with a recorded digest"
gh release download "$strom_tag" --repo Eyevinn/strom --pattern "$asset" --dir "$work" --clobber
have="$(sha256sum "$work/$asset" | cut -d' ' -f1)"
[[ "$want" == "$have" ]] || fail "checksum mismatch for $asset"
install -m 0755 "$work/$asset" "$work/strom-bin"

mkdir -p "$work/strom"
"$work/strom-bin" --headless --port "$strom_port" --data-dir "$work/strom" \
    > "$work/strom/strom.log" 2>&1 &
pids+=("$!")

strom_get() { curl -fsS "$strom_url$1"; }
say "Waiting for Strom's discovery to list both devices"
seen_camera=""
seen_mic=""
for _ in $(seq 90); do
    seen_camera="$(strom_get '/api/discovery/devices?category=video_source' 2>/dev/null \
        | jq -r --arg n "$camera_name" '.[] | select(.name | contains($n)) | .name' | head -n 1)" || true
    seen_mic="$(strom_get '/api/discovery/devices?category=audio_source' 2>/dev/null \
        | jq -r --arg n "$mic_name" '.[] | select(.name | contains($n)) | .name' | head -n 1)" || true
    [[ -n "$seen_camera" && -n "$seen_mic" ]] && break
    sleep 2
done
echo "::group::Strom discovery"
strom_get '/api/discovery/devices?category=video_source' | jq -r '.[] | "video: \(.id) \(.name)"' || true
strom_get '/api/discovery/devices?category=audio_source' | jq -r '.[] | "audio: \(.id) \(.name)"' || true
echo "::endgroup::"
[[ -n "$seen_camera" ]] || fail "Strom $strom_tag does not list the virtual camera '$camera_name' as a video source"
[[ -n "$seen_mic" ]] || fail "Strom $strom_tag does not list the virtual microphone '$mic_name' as an audio source"
pass "Strom $strom_tag lists '$seen_camera' (video_source) and '$seen_mic' (audio_source)"

# --- 4. up --audio ------------------------------------------------------------------------

# The instance address and the token stay in the environment, never in the file.
cat > "$work/gateway.toml" <<EOF
[gateway]
name = "$gateway_name"

[strom]
url = "$strom_url"
manage = false

[open_live]
auth_mode = "osc"
register = true

[uplink]
mode = "caller"
port_range = "$port_range"
latency_ms = 200

[video]
bitrate_kbps = 1000
EOF
chmod 0600 "$work/gateway.toml"
export OLI_OPEN_LIVE_URL="$open_live_url"
export OSC_ACCESS_TOKEN

"$gateway_bin" --config "$work/gateway.toml" check
"$gateway_bin" --config "$work/gateway.toml" devices

say "Starting: up --all --devices '$camera_name' --audio '$camera_name=$mic_name'"
"$gateway_bin" --config "$work/gateway.toml" up --all \
    --devices "$camera_name" --audio "$camera_name=$mic_name" \
    < /dev/null > "$work/up.log" 2>&1 &
up_pid=$!

source_id=""
for _ in $(seq 60); do
    kill -0 "$up_pid" 2>/dev/null || fail "open-live-ingest up exited before registering the source"
    source_id="$(our_source_ids | head -n 1)" || true
    [[ -n "$source_id" ]] && break
    sleep 2
done
[[ -n "$source_id" ]] || fail "no source named '$gateway_name — …' appeared in Open Live within 120 s"
source_json="$(ol_get "/api/v1/sources/$source_id")"
source_name="$(jq -r .name <<<"$source_json")"
pass "source '$source_name' ($source_id) registered in Open Live '$instance' (streamType $(jq -r .streamType <<<"$source_json"), status $(jq -r .status <<<"$source_json"))"

# --- 5. audio checks ------------------------------------------------------------------------

flow_id=""
for _ in $(seq 30); do
    flow_id="$("$gateway_bin" --config "$work/gateway.toml" status --json 2>/dev/null \
        | jq -r --arg n "$source_name" '.inputs[] | select(.name == $n and .flow.running == true) | .flow.id' \
        | head -n 1)" || true
    [[ -n "$flow_id" ]] && break
    sleep 2
done
[[ -n "$flow_id" ]] || fail "Strom is not running a flow for '$source_name'"

flow_json="$(strom_get "/api/flows/$flow_id" | jq '.flow')"
stream_mode="$(jq -r '.blocks[] | select(.id == "capture") | .properties.stream_mode' <<<"$flow_json")"
audio_tracks="$(jq -r '.blocks[] | select(.id == "uplink") | .properties.num_audio_tracks' <<<"$flow_json")"
audio_link="$(jq -r '.links[] | select(.from == "capture:audio_out" and .to == "uplink:audio_in_0") | .to' <<<"$flow_json")"
echo "::group::Strom flow (blocks and links)"
jq '{blocks: [.blocks[] | {id, block_definition_id, properties: (.properties | with_entries(select(.key != "srt_uri")))}], links}' <<<"$flow_json"
echo "::endgroup::"
[[ "$stream_mode" == "audio_video" ]] || fail "the capture block runs stream_mode '$stream_mode', not audio_video"
[[ "$audio_tracks" == "1" ]] || fail "the uplink declares $audio_tracks audio track(s), not 1"
[[ -n "$audio_link" ]] || fail "the flow has no capture:audio_out -> uplink:audio_in_0 link"
pass "flow $flow_id captures audio_video, links capture:audio_out -> uplink:audio_in_0, and declares 1 audio track"

curl -fsS -o /dev/null -X POST -H 'content-type: application/json' \
    -d '{"element_id":"uplink","sample_interval":1,"timeout_secs":30}' \
    "$strom_url/api/flows/$flow_id/probes" \
    || fail "Strom refused a buffer probe on the uplink block"
sleep 5
probes="$(strom_get "/api/flows/$flow_id/probes")"
echo "::group::Strom buffer probes on the uplink"
jq . <<<"$probes"
echo "::endgroup::"
audio_buffers="$(jq '[.probes[] | select(.pad_name | test("^A0$|audio"; "i")) | .sample_count] | add // 0' <<<"$probes")"
video_buffers="$(jq '[.probes[] | select(.pad_name | test("^V0$|video"; "i")) | .sample_count] | add // 0' <<<"$probes")"
(( audio_buffers > 0 )) || fail "no audio buffers reached the uplink's audio input in 5 s (video: $video_buffers)"
pass "$audio_buffers audio buffers (and $video_buffers video buffers) reached the uplink in 5 s"

streams="$(ol_get "/api/v1/sources/$source_id/audio-streams" || echo '{}')"
discovery="$(jq -r '.discovery // "absent"' <<<"$streams")"
if [[ "$discovery" == "available" ]] || jq -e '(.streams // []) | length > 0' <<<"$streams" >/dev/null; then
    count="$(jq '(.streams // []) | length' <<<"$streams")"
    (( count >= 1 )) || fail "Open Live reports no audio stream for the source"
    pass "Open Live reports $count audio stream(s) for the source"
else
    note "Open Live cannot report the source's audio tracks yet: GET /api/v1/sources/{id}/audio-streams answers discovery '$discovery', reason '$(jq -r '.reason // "none"' <<<"$streams")'. The audio track is confirmed on the gateway side above; Open Live's own confirmation needs its source-stream probe."
fi

# --- 6. teardown -----------------------------------------------------------------------------

say "Stopping up (SIGTERM, as Ctrl-C)"
kill -TERM "$up_pid"
for _ in $(seq 60); do kill -0 "$up_pid" 2>/dev/null || break; sleep 1; done
if kill -0 "$up_pid" 2>/dev/null; then fail "open-live-ingest up did not exit within 60 s of SIGTERM"; fi
wait "$up_pid" || true
up_pid=""
left="$(our_source_ids | wc -l)"
(( left == 0 )) || fail "$left source(s) of '$gateway_name' are still registered in Open Live after up stopped"
pass "up stopped on SIGTERM and removed the source from Open Live"

echo "::group::open-live-ingest up log"
cat "$work/up.log"
echo "::endgroup::"
say "e2e audio device: all checks passed"
