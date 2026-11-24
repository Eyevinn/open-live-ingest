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
#      with a gateway name unique to the run. The uplink goes through Open Live's SRT
#      relay (`uplink.mode = "relay"`, #159): openlivedev is relay-only and publishes
#      no SRT ports, so an instance that reports no relay fails `up` with the reason
#      instead of quietly sending to a port nothing serves.
#   5. Checks, each logged and written to the step summary:
#      - the source is registered in Open Live under the gateway's name, through the
#        relay, with a dial-in address (`status --json`: `.source.relay`/`.dial_in`);
#      - the flow Strom runs for it captures `audio_video` from the microphone and
#        declares one audio track on the uplink;
#      - audio buffers reach the uplink's audio input (A0): a Strom buffer probe on
#        the uplink block counts them;
#      - Open Live itself confirms the audio track: the script creates a production
#        holding the source (on video_in_0) and activates it, so the cloud Strom dials
#        the relay and decodes the feed, then polls
#        GET /api/v1/sources/{id}/audio-streams until it answers `discovery:
#        available` (open-live#632) and asserts at least one stream. Any other answer
#        once the production is active (`no-live-flow`, `no-signal`, ...) is a FAIL.
#   6. The production is deactivated and deleted, then Ctrl-C equivalent (SIGTERM) to
#      `up`, then a check that the source is gone from Open Live. Whatever happens,
#      the exit trap deactivates and deletes the run's production, stops `up`, runs
#      `down`, and deletes any source still carrying the run's gateway name.
#
# Needs: sudo (apt-get, modprobe), curl, jq, gh (GH_TOKEN, to read the Strom
# release), and OSC_ACCESS_TOKEN, an Open Source Cloud personal access token for the
# workspace that owns the instance. The token, the service token and the instance
# address are masked in the log; nothing here prints a response body that can carry
# them. Strom gets neither token: it would seed its own OSC access from
# OSC_ACCESS_TOKEN, and its API is unauthenticated. It always listens on all
# interfaces, so on CI the script also drops outside traffic to its port.
#
# Without OSC_ACCESS_TOKEN, steps 1-3 still run. The run then fails naming the
# secret when E2E_REQUIRE_SECRET is `true` (the workflow sets that for a run by
# hand); otherwise (a pull request run) it ends with a warning naming the secret and
# exits 0, so the missing secret does not fail unrelated pull requests.
#
# Before step 4 it removes any production and source left on the instance by an
# earlier run that was killed before its exit trap ran (names starting `oli-e2e-`).
# Workflow runs on one instance are serialized by the workflow's concurrency group,
# so none of those is live. A local run (`oli-e2e-local-<pid>`) is outside that
# group: a workflow run, or another local run, started while it is going deletes its
# production and source, and it then fails. Do not run it by hand against an
# instance while the workflow runs there.

set -euo pipefail

gateway_bin="$(realpath "${1:?usage: $0 <path to open-live-ingest>}")"
root="$(cd "$(dirname "$0")/../.." && pwd)"
work="${RUNNER_TEMP:-$(mktemp -d)}/oli-e2e"
mkdir -p "$work"

instance="${OPEN_LIVE_INSTANCE:-openlivedev}"
# The uplink dials Open Live's SRT relay with the stream id Open Live gives the
# source; with no passphrase in the settings, `up` generates one and keeps it in the
# run's settings file. The cloud Strom only dials the relay once a production holding
# the source is active; step 5 activates one, so Open Live can see the audio track
# itself. The relay is E2E_RELAY when set, otherwise picked from Open Live's
# server-info below: `default` when it reports one, else the first.
relay_name="${E2E_RELAY:-}"
# How long the production may take to become active, to stop, and how long Open Live
# may take, once it is active, to report the source's audio streams.
activate_timeout="${E2E_ACTIVATE_TIMEOUT:-300}"
deactivate_timeout="${E2E_DEACTIVATE_TIMEOUT:-180}"
discovery_timeout="${E2E_DISCOVERY_TIMEOUT:-120}"
mixer_input="video_in_0"
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
production_id=""

cleanup() {
    local status=$?
    set +e
    # The production first: it holds the source, and while it is active the cloud
    # Strom keeps dialling the relay.
    if [[ -n "$production_id" ]]; then
        say "removing the run's production $production_id"
        remove_production "$production_id" || say "could not remove production $production_id"
    fi
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
# Strom v0.8.2 binds 0.0.0.0 with no option to change it and its API is
# unauthenticated: on CI, accept only loopback traffic to its port.
if [[ -n "${GITHUB_ACTIONS:-}" ]]; then
    for ipt in iptables ip6tables; do
        sudo "$ipt" -I INPUT -p tcp --dport "$strom_port" ! -i lo -j DROP \
            || say "could not add the $ipt rule for port $strom_port"
    done
fi
# Neither token goes to Strom: it seeds its default OSC access from OSC_ACCESS_TOKEN.
env -u OSC_ACCESS_TOKEN -u GH_TOKEN "$work/strom-bin" --headless --port "$strom_port" --data-dir "$work/strom" \
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

# --- credentials and instance (after the local devices, so a run without the
# secret still proves the runner side) ------------------------------------------------------

if [[ -z "${OSC_ACCESS_TOKEN:-}" ]]; then
    missing="missing capability: the repository secret OSC_ACCESS_TOKEN (an Open Source Cloud personal access token for the workspace that owns the Open Live instance '$instance') is not set"
    [[ "${E2E_REQUIRE_SECRET:-true}" != "true" ]] || fail "$missing"
    echo "::warning title=e2e audio device::$missing; the device checks above passed, up --audio against '$instance' did not run"
    record "- SKIPPED: $missing. The device checks above passed; up --audio against '$instance' did not run."
    say "SKIPPED: $missing"
    exit 0
fi
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
ol_post() {
    curl -fsS -X POST -H "Authorization: Bearer $sat" -H 'content-type: application/json' \
        --max-time "${3:-30}" -d "$2" "$open_live_url$1"
}
our_source_ids() {
    ol_get /api/v1/sources | jq -r --arg p "$gateway_name — " '.[] | select(.name | startswith($p)) | .id'
}
# The production's status, or nothing when Open Live does not answer with one (gone,
# or a transient error). Never fails, so a poll under `set -e` keeps polling.
production_status() {
    { ol_get "/api/v1/productions/$1" | jq -r '.status // empty'; } 2>/dev/null || true
}
# Deactivates (harmless when it is not active), waits until Open Live calls it idle
# (`inactive`, or `ended` after it was active), then deletes it. True when it is gone.
remove_production() {
    local id="$1" state=""
    ol_post "/api/v1/productions/$id/deactivate" '{}' "$deactivate_timeout" >/dev/null 2>&1 || true
    for _ in $(seq $((deactivate_timeout / 2))); do
        state="$(production_status "$id")"
        [[ "$state" == "inactive" || "$state" == "ended" || -z "$state" ]] && break
        sleep 2
    done
    local code
    code="$(curl -sS -o /dev/null -w '%{http_code}' -X DELETE -H "Authorization: Bearer $sat" \
        "$open_live_url/api/v1/productions/$id")" || return 1
    [[ "$code" =~ ^(200|202|204|404)$ ]] || return 1
    if [[ "$id" == "$production_id" ]]; then production_id=""; fi
}

# Productions first: they hold the sources.
stale_productions="$(ol_get /api/v1/productions \
    | jq -r '.[] | select((.name // "") | startswith("oli-e2e-")) | (._id // .id)')" \
    || fail "GET /api/v1/productions on '$instance' failed"
for id in $stale_productions; do
    say "removing stale production $id from an earlier e2e run"
    remove_production "$id" || say "could not remove stale production $id"
done
[[ -z "$stale_productions" ]] || note "removed $(wc -w <<<"$stale_productions") stale oli-e2e- production(s) left by an earlier run"

stale_ids="$(ol_get /api/v1/sources \
    | jq -r --arg p "$gateway_name — " '.[] | select((.name | startswith("oli-e2e-")) and (.name | startswith($p) | not)) | .id')"
removed=0
for id in $stale_ids; do
    say "removing stale source $id from an earlier e2e run"
    if curl -fsS -o /dev/null -X DELETE -H "Authorization: Bearer $sat" \
        "$open_live_url/api/v1/sources/$id"; then
        removed=$((removed + 1))
    else
        say "could not remove stale source $id"
    fi
done
[[ -z "$stale_ids" ]] || note "removed $removed of $(wc -w <<<"$stale_ids") stale oli-e2e- source(s) left by an earlier run"

srt_info="$(ol_get /api/v1/server-info | jq -c '.srt // null')" \
    || fail "GET /api/v1/server-info on '$instance' failed"
relays="$(jq -r '(.relays // [])[] | .name' <<<"$srt_info")"
[[ -n "$relays" ]] || fail "missing capability: Open Live '$instance' reports no SRT relay (srt.state '$(jq -r '.state // "absent"' <<<"$srt_info")'), and it publishes no SRT ports, so a device source has nowhere to send"
if [[ -z "$relay_name" ]]; then
    if grep -qx default <<<"$relays"; then relay_name=default; else relay_name="$(head -n 1 <<<"$relays")"; fi
fi
grep -qxF "$relay_name" <<<"$relays" \
    || fail "Open Live '$instance' has no SRT relay named '$relay_name'; it has: $(tr '\n' ' ' <<<"$relays")"
pass "Open Live '$instance' reports SRT relay(s) $(tr '\n' ' ' <<<"$relays")(state $(jq -r '.state // "unknown"' <<<"$srt_info")); sending through '$relay_name'"

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
mode = "relay"
relay = "$relay_name"
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

# Through the relay: the gateway's own view of the source names the relay and has a
# dial-in address (srt://<relay>?streamid=<id>). This check prints only whether they
# are there. The up log dumped at the end does print the dial-in address: the relay's
# public host:port and the stream id, which is no secret (docs/DESIGN.md), with the
# passphrase masked by the gateway.
via_relay=""
for _ in $(seq 30); do
    via_relay="$("$gateway_bin" --config "$work/gateway.toml" status --json 2>/dev/null \
        | jq -r --arg n "$source_name" '.inputs[] | select(.name == $n) | .source
            | select(. != null and (.relay // "") != "") | "\(.relay) \(if (.dial_in // "") != "" then "dial-in" else "no-dial-in: \(.dial_in_error // "no reason")" end)"' \
        | head -n 1)" || true
    [[ "$via_relay" == *" dial-in" ]] && break
    sleep 2
done
[[ -n "$via_relay" ]] || fail "the source '$source_name' is not registered through an SRT relay (status --json reports no source.relay)"
[[ "$via_relay" == *" dial-in" ]] || fail "the source '$source_name' is on relay '${via_relay%% *}' but Open Live gave no dial-in address (${via_relay#* })"
pass "the source goes through Open Live's SRT relay '${via_relay%% *}', which gave it a dial-in address"

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

# Open Live's own view of the audio track. It only discovers a source's streams while
# a production holding it is active (open-live#632): the cloud Strom then dials the
# relay, decodes the feed, and Open Live reads the audio pads it negotiated.
production_name="$gateway_name — production"
made="$(ol_post /api/v1/productions "$(jq -nc --arg n "$production_name" '{name: $n}')")" \
    || fail "creating production '$production_name' on '$instance' failed"
production_id="$(jq -r '._id // .id // empty' <<<"$made")"
[[ -n "$production_id" ]] || fail "Open Live created production '$production_name' but returned no id"
ol_post "/api/v1/productions/$production_id/sources" \
    "$(jq -nc --arg s "$source_id" --arg m "$mixer_input" '{sourceId: $s, mixerInput: $m}')" >/dev/null \
    || fail "assigning the source to production $production_id ($mixer_input) failed"
say "Activating production $production_id (up to $activate_timeout s)"
ol_post "/api/v1/productions/$production_id/activate" '{}' "$activate_timeout" >/dev/null \
    || fail "activating production $production_id failed"
state="activating"
for _ in $(seq "$activate_timeout"); do
    state="$(production_status "$production_id")"
    [[ "$state" == "active" ]] && break
    [[ "$state" == "activating" || "$state" == "inactive" || -z "$state" ]] \
        || fail "production $production_id did not activate: its status went to '$state'"
    sleep 1
done
[[ "$state" == "active" ]] || fail "production $production_id was still '$state' after $activate_timeout s"
pass "production '$production_name' ($production_id) holding the source on $mixer_input is active"

streams='{}'
discovery="absent"
for _ in $(seq $((discovery_timeout / 3))); do
    streams="$(ol_get "/api/v1/sources/$source_id/audio-streams" 2>/dev/null || echo '{}')"
    discovery="$(jq -r '.discovery // "absent"' <<<"$streams")"
    [[ "$discovery" == "available" ]] && break
    sleep 3
done
echo "::group::Open Live audio-streams for the source"
jq . <<<"$streams"
echo "::endgroup::"
[[ "$discovery" == "available" ]] \
    || fail "Open Live does not confirm the source's audio track: with production $production_id active, GET /api/v1/sources/{id}/audio-streams still answers discovery '$discovery', reason '$(jq -r '.reason // "none"' <<<"$streams")' after $discovery_timeout s"
count="$(jq '(.streams // []) | length' <<<"$streams")"
(( count >= 1 )) || fail "Open Live reports discovery 'available' but no audio stream for the source"
pass "Open Live reports $count audio stream(s) for the source: $(jq -c '[.streams[] | {index, channels}]' <<<"$streams")"

# --- 6. teardown -----------------------------------------------------------------------------

say "Deactivating and deleting production $production_id"
remove_production "$production_id" || fail "production $production_id could not be deactivated and deleted"
pass "production deactivated and deleted"

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
