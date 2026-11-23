#!/usr/bin/env python3
"""The agent-run end to end of the iPhone app on the iOS Simulator against an Open Live
instance (OL-64h, #144; docs/specs/ios-app.md sections 15 and 16). Run by the `e2e-ios`
workflow on macos-latest, after it has built the app and its tests for a simulator:

    python3 -I .github/scripts/e2e-ios.py simulator    # prints the UDID of the newest iPhone simulator
    python3 -I .github/scripts/e2e-ios.py run          # the end to end

`run` is the host half. The app half is the XCUITest
ios/OpenLiveIngestUITests/OpenLiveDevE2EUITests.swift, which drives the app's screens
the way a person does, with the test pattern (the simulator has no camera). This script
starts a small control server on 127.0.0.1 that the test calls for everything that is
not the app (the simulator shares the host's loopback), runs the test with
`xcodebuild test-without-building`, and writes the report. In the order of #144:

  1. Sign in: the test opens Advanced and pastes a personal access token into the token
     field. The token never reaches the test: this script puts OSC_ACCESS_TOKEN on the
     simulator's pasteboard (`xcrun simctl pbcopy`, the token on stdin) just before the
     paste and empties the pasteboard right after. Then the instance named
     OPEN_LIVE_INSTANCE (default openlivedev) is picked, and the test pattern, with a
     label unique to the run (`oli-ios-e2e-<run id>-<attempt>`), so the source is
     `<venue> — <label>`.
  2. Go live with no production: the app says Armed, the source is registered in Open
     Live, and nothing is sent for ARMED_HOLD seconds: the link line says
     `Link: not sending`, it carries no SRT counters and the source is not `active` (AC2).
  3. The control server creates a production named `<label> — production` holding the
     source on video_in_0 and activates it through the Open Live API. The app goes Live
     and the source `active`. The SRT sender's counters (the link line's accessibility
     value: packets sent, dropped, bytes sent) are sampled every half second from the
     first connected reading for E2E_SRT_INTERVAL seconds (default 60, never under 60):
     the dropped share between the first and the last reading has to be under 1 %, the
     counters must not go back (a reconnect) and packets must be sent (AC1 on the
     simulator). Meanwhile the cloud side is read: `GET /api/v1/productions/<id>`
     `inputResolutions` (the cloud Strom's mixer has negotiated a picture for the input,
     as connect-order.py reads it) and the production's audio meters (`METER_DATA` for a
     `ch<n>` element above SILENCE_DB on the watch-only controller socket
     `/ws/productions/<id>/controller?mode=watch`). No picture fails; meters that are
     there but silent fail; no channel meter at all is a warning that names it (the
     instance may run without metering). The peak-to-average bitrate of what was sent
     is noted from the bytes counter (spec section 16).
  4. Deactivate: the app is back to waiting (Armed, or Waiting for production while the
     inactive production still holds the source), `Link: not sending` (AC2).
  5. Stop: the source is kept, `inactive`, with its id. Go live again: no input fails
     (an Open Live 409 would), the same source id is reused, still one source (AC3).
  6. The production is activated again and the app goes Live. The app goes to the
     background: the source has to be `inactive` within BACKGROUND_LIMIT seconds (the
     phone's own bound is 4 s, section 9; the time is noted). Back in the foreground,
     with the production still `active`, the app is Live again on a new SRT connection
     (its counters started again), with the same source, and the cloud decodes again.
  7. Stop, then this script deactivates and deletes the production and deletes the
     source, whatever happened before. Sources and productions carrying
     `oli-ios-e2e-` left by an earlier run that was killed are removed first.

The test posts each check it passed to the control server, which writes them with its
own to `report.md` (also appended to the step summary), `results.json` and
`comment.md` (a draft of the comment for #133) under E2E_OUT (default
$RUNNER_TEMP/e2e-ios). Exits 1 when the test or a check failed, or cleanup did not.

Without OSC_ACCESS_TOKEN nothing is created anywhere. The test is still run once with
no control server, where it skips itself, which proves the test bundle and the
`-only-testing` name; then the run fails naming the secret when E2E_REQUIRE_SECRET is
`true` (a run by hand), or warns ("missing capability") and exits 0 otherwise (a pull
request run).

Needs IOS_UDID (a booted or bootable simulator, from `simulator`) and E2E_DERIVED (the
DerivedData folder `xcodebuild build-for-testing` wrote). Python standard library only.
OSC_ACCESS_TOKEN, the service token, the instance address and the cloud Strom's host
are masked in the log as soon as they are known; no response body is printed; xcodebuild
is started with OSC_ACCESS_TOKEN, GH_TOKEN and GITHUB_TOKEN removed from its
environment. The pure parts are unit tested: `python3 -I .github/scripts/test_e2e_ios.py`.
"""

import base64
import hashlib
import json
import os
import re
import socket
import ssl
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

CATALOG = "https://catalog.svc.prod.osaas.io/mysubscriptions"
TOKEN_EXCHANGE = "https://token.svc.prod.osaas.io/servicetoken"
SERVICE_ID = "eyevinn-open-live"
PREFIX = "oli-ios-e2e-"
MIXER_INPUT = "video_in_0"
MIXER_INDEX = 0
PROJECT = "ios/OpenLiveIngest.xcodeproj"
SCHEME = "OpenLiveIngest"
TEST_ID = "OpenLiveIngestUITests/OpenLiveDevE2EUITests"
TEST_CASE = "OpenLiveDevE2EUITests"
DROP_LIMIT = 0.01
MIN_SECONDS = 60
# How long Go live has to stay armed and send nothing before the production exists.
ARMED_HOLD = 10
# The phone marks the source inactive within 4 s of going to the background (section 9);
# against a real Open Live the look that sees it adds up to a poll and a round trip.
BACKGROUND_LIMIT = 10
PHONE_BACKGROUND_BOUND = 4
ACTIVATE_TIMEOUT = 300
DEACTIVATE_TIMEOUT = 180
POLL_EVERY = 1.0
# A meter peak above this (dBFS) is sound; the test pattern's tone is at -20 dBFS.
SILENCE_DB = -60.0
TEST_TIMEOUT = 45 * 60
WITHHELD_ENV = ("OSC_ACCESS_TOKEN", "GH_TOKEN", "GITHUB_TOKEN")
WS_GUID = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"

SUMMARY = os.environ.get("GITHUB_STEP_SUMMARY") or os.devnull
ON_ACTIONS = bool(os.environ.get("GITHUB_ACTIONS"))


class Failed(Exception):
    pass


def fail(text):
    raise Failed(text)


def say(text):
    print(f"==> {text}", flush=True)


MASKED = set()


def mask(value):
    if ON_ACTIONS and value and value not in MASKED:
        MASKED.add(value)
        print(f"::add-mask::{value}", flush=True)


def mask_url(url):
    bare = url.split("://", 1)[-1].rstrip("/")
    host = bare.split("/", 1)[0]
    for item in (url, url.rstrip("/"), bare, host, host.rsplit(":", 1)[0]):
        mask(item)


def http(method, url, headers=None, body=None, timeout=30):
    """(status, parsed JSON or None). Never raises for an HTTP status, never prints a
    body."""
    data = None if body is None else json.dumps(body).encode()
    request = urllib.request.Request(url, data=data, method=method, headers=headers or {})
    if data is not None:
        request.add_header("content-type", "application/json")
    try:
        with urllib.request.urlopen(request, timeout=timeout) as response:
            raw = response.read()
            status = response.status
    except urllib.error.HTTPError as err:
        return err.code, None
    except (urllib.error.URLError, TimeoutError, OSError) as err:
        return 0, {"_transport_error": type(err).__name__}
    try:
        return status, json.loads(raw) if raw else None
    except ValueError:
        return status, None


# --- pure parts (unit tested) ----------------------------------------------------------


def pick_simulator(listing):
    """(udid, description) of the newest available iPhone simulator in `xcrun simctl list
    devices available --json`, as ios/run-tests.sh picks it; None when there is none."""
    best = None
    for runtime, devices in (listing.get("devices") or {}).items():
        m = re.search(r"iOS-(\d+)-(\d+)", runtime)
        if not m:
            continue
        version = (int(m[1]), int(m[2]))
        for d in devices:
            if d.get("isAvailable") and str(d.get("name", "")).startswith("iPhone"):
                if best is None or version > best[0]:
                    best = (version, d["udid"], d["name"])
    if best is None:
        return None
    return best[1], f"iOS {best[0][0]}.{best[0][1]}, {best[2]}"


def find_instance(instances, name):
    """The `url` of the instance named `name` in the OSC instance list."""
    for item in instances if isinstance(instances, list) else []:
        if isinstance(item, dict) and item.get("name") == name and item.get("url"):
            return str(item["url"])
    return None


def listener_port(address):
    """The port of a hostless listener address (`srt://:47100?mode=listener`), or None."""
    m = re.fullmatch(r"srt://:(\d+)(\?.*)?", str(address or "").strip())
    if not m:
        return None
    port = int(m[1])
    return port if 0 < port < 65536 else None


def ours(sources, label):
    """The sources of this run: named `<venue> — <label>`."""
    out = []
    for s in sources if isinstance(sources, list) else []:
        if isinstance(s, dict) and str(s.get("name", "")).endswith(f" — {label}"):
            out.append({
                "id": s.get("_id") or s.get("id"),
                "name": s.get("name"),
                "status": s.get("status"),
                "port": listener_port(s.get("address")),
            })
    return out


def stale(name, label):
    """Whether a source or production name belongs to an earlier run of this test."""
    name = str(name or "")
    mine = name.endswith(f" — {label}") or name.startswith(f"{label} — ")
    return not mine and (name.startswith(PREFIX) or f" — {PREFIX}" in name)


def decoding(resolutions, index=MIXER_INDEX):
    """(width, height) the cloud mixer negotiated for the input, or None."""
    if not isinstance(resolutions, list) or index >= len(resolutions):
        return None
    value = resolutions[index]
    if isinstance(value, dict) and value.get("width") and value.get("height"):
        return (int(value["width"]), int(value["height"]))
    return None


def share(part, whole):
    return (part / whole) if whole else None


def interval(samples, seconds, limit=DROP_LIMIT):
    """The SRT sender's counters over the window: `samples` are dicts with t (s), sent,
    dropped and bytes (or None), oldest first. The first reading is the first that has
    sent anything, the second the first at least `seconds` after it. A dict with the
    two readings, the share dropped between them and the problems (empty when it
    passes)."""
    problems = []
    rows = [s for s in samples if s.get("sent") is not None and s.get("dropped") is not None]
    first = next((s for s in rows if s["sent"] > 0), None)
    out = {"first": first, "second": None, "seconds": None, "packets_sent": None,
           "packets_sent_dropped": None, "drop_share": None, "problems": problems}
    if first is None:
        problems.append("the link line never showed packets sent")
        return out
    after = [s for s in rows if s["t"] >= first["t"]]
    for a, b in zip(after, after[1:]):
        if b["sent"] < a["sent"] or b["dropped"] < a["dropped"]:
            problems.append(f"the counters went back at {b['t'] - first['t']:.1f} s (the link reconnected)")
            break
    second = next((s for s in after if s["t"] >= first["t"] + seconds), None)
    out["second"] = second
    if second is None:
        span = after[-1]["t"] - first["t"]
        problems.append(f"no second reading {seconds} s after the first (the samples span {span:.1f} s)")
        return out
    sent = second["sent"] - first["sent"]
    dropped = second["dropped"] - first["dropped"]
    out.update(seconds=second["t"] - first["t"], packets_sent=sent, packets_sent_dropped=dropped,
               drop_share=share(dropped, sent))
    if sent <= 0:
        problems.append("no packets were sent between the readings")
    elif dropped / sent >= limit:
        problems.append(
            f"{dropped} of {sent} packets ({dropped / sent:.3%}) were dropped between the readings, "
            f"{limit:.0%} or more")
    return out


def peak_to_average(samples):
    """The bitrate of what was sent, from the bytes counter: (average kbps, peak kbps,
    ratio), or (None, None, None). Only the samples where the counter moved count, so
    each window is one of the sender's reports (every 2 s), as the link line saw it."""
    moved = []
    for s in samples:
        b = s.get("bytes")
        if b is None:
            continue
        if not moved or b != moved[-1][1]:
            moved.append((s["t"], b))
    rates = []
    for (t0, b0), (t1, b1) in zip(moved, moved[1:]):
        if b1 < b0:
            return None, None, None
        if t1 - t0 >= 0.5:
            rates.append((b1 - b0) * 8 / (t1 - t0) / 1000)
    if len(moved) < 3 or not rates:
        return None, None, None
    duration = moved[-1][0] - moved[0][0]
    if duration <= 0:
        return None, None, None
    average = (moved[-1][1] - moved[0][1]) * 8 / duration / 1000
    if average <= 0:
        return None, None, None
    peak = max(rates)
    return average, peak, peak / average


def meter_peak(value):
    """The loudest of a meter's peak value(s), in dB, or None."""
    if isinstance(value, (int, float)) and not isinstance(value, bool):
        return float(value)
    if isinstance(value, list):
        numbers = [meter_peak(v) for v in value]
        numbers = [n for n in numbers if n is not None]
        return max(numbers) if numbers else None
    return None


def audio_verdict(meters, silence=SILENCE_DB):
    """`meters` are (t, element id, loudest peak) of the channel meters seen. ('sound'
    | 'silent' | 'unreported', detail)."""
    channels = [m for m in meters if str(m[1]).startswith("ch") and m[2] is not None]
    if not channels:
        return "unreported", "no channel meter arrived"
    loudest = max(m[2] for m in channels)
    loud = [m for m in channels if m[2] > silence]
    names = sorted({m[1] for m in channels})
    detail = f"{len(channels)} meter readings on {', '.join(names)}, loudest {loudest:.1f} dB"
    return ("sound" if loud else "silent"), detail


def decode_summary(polls, start, end):
    """`polls` are (t, status, resolution or None). The share of the looks in [start,
    end] that saw a picture, and the resolution."""
    window = [p for p in polls if start <= p[0] <= end]
    seen = [p for p in window if p[2] is not None]
    return {
        "looks": len(window),
        "decoding": len(seen),
        "decoding_share": share(len(seen), len(window)),
        "resolution": f"{seen[0][2][0]}x{seen[0][2][1]}" if seen else None,
    }


def query(path):
    """(path, {key: value}) of a control request's target."""
    parsed = urllib.parse.urlsplit(path)
    return parsed.path, {k: v[-1] for k, v in urllib.parse.parse_qs(parsed.query, keep_blank_values=True).items()}


def ws_frame(opcode, payload, mask_key=None):
    """A client WebSocket frame (RFC 6455: a client masks every frame)."""
    mask_key = mask_key if mask_key is not None else os.urandom(4)
    head = bytes([0x80 | opcode])
    n = len(payload)
    if n < 126:
        head += bytes([0x80 | n])
    elif n < 65536:
        head += bytes([0x80 | 126]) + n.to_bytes(2, "big")
    else:
        head += bytes([0x80 | 127]) + n.to_bytes(8, "big")
    return head + mask_key + bytes(b ^ mask_key[i % 4] for i, b in enumerate(payload))


def ws_parse(buffer):
    """(fin, opcode, payload, rest) of the first whole frame in `buffer`, or None while it
    is incomplete."""
    if len(buffer) < 2:
        return None
    fin = bool(buffer[0] & 0x80)
    opcode = buffer[0] & 0x0F
    masked = bool(buffer[1] & 0x80)
    n = buffer[1] & 0x7F
    at = 2
    if n == 126:
        if len(buffer) < 4:
            return None
        n = int.from_bytes(buffer[2:4], "big")
        at = 4
    elif n == 127:
        if len(buffer) < 10:
            return None
        n = int.from_bytes(buffer[2:10], "big")
        at = 10
    key = b""
    if masked:
        if len(buffer) < at + 4:
            return None
        key = buffer[at:at + 4]
        at += 4
    if len(buffer) < at + n:
        return None
    payload = buffer[at:at + n]
    if masked:
        payload = bytes(b ^ key[i % 4] for i, b in enumerate(payload))
    return fin, opcode, payload, buffer[at + n:]


def ws_accept(key):
    return base64.b64encode(hashlib.sha1((key + WS_GUID).encode()).digest()).decode()


def report(meta, checks, phases, cleanup):
    out = [
        f"## iPhone app end to end on the iOS Simulator (OL-64h) against `{meta['instance']}`",
        "",
        f"Run: {meta.get('run_url', 'local')}. Simulator: {meta.get('simulator', '?')}; "
        f"{meta.get('xcode', '?')}; commit {meta.get('sha', '?')[:12]}. Test pattern, "
        f"label `{meta.get('label', '?')}`. Readings {meta.get('seconds', '?')} s apart.",
        "",
        "| | Check | Detail |",
        "|---|---|---|",
    ]
    for c in checks:
        out.append(f"| {'pass' if c['ok'] else 'FAIL'} | {c['id']} | {c.get('detail', '')} |")
    out.append("")
    for name, p in phases.items():
        gap = p.get("interval") or {}
        avg, peak, ratio = p.get("bitrate") or (None, None, None)
        d = p.get("decode") or {}
        out.append(f"**{name}**")
        if gap.get("drop_share") is not None:
            out.append(
                f"- SRT sender over {gap['seconds']:.0f} s: {gap['packets_sent']:,} packets sent, "
                f"{gap['packets_sent_dropped']:,} dropped (**{gap['drop_share']:.3%}**).")
        for problem in gap.get("problems") or []:
            out.append(f"- Problem: {problem}")
        if ratio:
            out.append(
                f"- Peak-to-average bitrate sent (spec §16), over the sender's 2 s reports: **{ratio:.2f}** "
                f"(peak {peak:.0f} kbps, average {avg:.0f} kbps).")
        if d:
            out.append(
                f"- Cloud picture (`inputResolutions[{MIXER_INDEX}]`): {d.get('resolution') or 'none'}, "
                f"in {d.get('decoding', 0)} of {d.get('looks', 0)} looks.")
        if p.get("audio"):
            out.append(f"- Cloud sound (channel meters): {p['audio'][0]} ({p['audio'][1]}).")
        out.append("")
    out.append(f"Cleanup: {cleanup}")
    return "\n".join(out)


def comment(meta, checks, phases, cleanup, passed):
    lines = [
        "<!-- ol-64h ios simulator e2e results -->",
        report(meta, checks, phases, cleanup),
        "",
        f"Result: **{'all checks passed' if passed else 'FAILED'}**. The `.xcresult` with the screenshots of "
        "each step is in the `e2e-ios` artifact of the run.",
        "",
        "Not covered on a simulator (a real camera, cellular, Wi-Fi to cellular): the human check in "
        "#133's closing comment (spec §15).",
    ]
    return "\n".join(lines)


# --- Open Live -------------------------------------------------------------------------


class OpenLive:
    def __init__(self, pat):
        self.pat = pat
        self.token = None
        self.expires = 0.0
        self.url = None
        self.lock = threading.Lock()

    def bearer(self):
        with self.lock:
            if self.token is None or time.time() > self.expires - 120:
                status, body = http(
                    "POST", TOKEN_EXCHANGE,
                    {"x-pat-jwt": f"Bearer {self.pat}", "accept": "application/json"},
                    {"serviceId": SERVICE_ID},
                )
                if status != 200 or not isinstance(body, dict) or not body.get("token"):
                    fail(f"the OSC token service refused OSC_ACCESS_TOKEN (HTTP {status})")
                self.token = body["token"]
                mask(self.token)
                try:
                    expiry = float(body.get("expiry"))
                    self.expires = expiry / 1000 if expiry > 1e11 else expiry
                except (TypeError, ValueError):
                    self.expires = time.time() + 600
            return {"Authorization": f"Bearer {self.token}"}

    def resolve(self, name):
        status, subs = http("GET", CATALOG, {"x-pat-jwt": f"Bearer {self.pat}"})
        if status != 200 or not isinstance(subs, list):
            fail(f"listing the OSC subscriptions failed with HTTP {status}")
        api = next((s.get("apiUrl") for s in subs if isinstance(s, dict) and s.get("serviceId") == SERVICE_ID), None)
        if not api:
            fail("missing capability: the workspace of OSC_ACCESS_TOKEN has no Open Live subscription")
        self.bearer()
        status, instances = http("GET", api, {"x-jwt": f"Bearer {self.token}"})
        if status != 200:
            fail(f"listing the Open Live instances failed with HTTP {status}")
        url = find_instance(instances, name)
        if not url:
            fail(f"missing capability: the workspace has no Open Live instance named '{name}'")
        mask_url(url)
        self.url = url.rstrip("/")

    def get(self, path, timeout=20):
        status, body = http("GET", f"{self.url}{path}", self.bearer(), timeout=timeout)
        return status, body

    def call(self, method, path, body=None, ok=(200, 201, 202, 204), timeout=30):
        status, data = http(method, f"{self.url}{path}", self.bearer(), body, timeout=timeout)
        if status not in ok:
            fail(f"{method} {path.split('?')[0]} failed with HTTP {status}")
        return data

    def sources(self):
        status, body = self.get("/api/v1/sources")
        if status != 200 or not isinstance(body, list):
            fail(f"GET /api/v1/sources failed with HTTP {status}")
        return body

    def productions(self):
        status, body = self.get("/api/v1/productions")
        if status != 200 or not isinstance(body, list):
            fail(f"GET /api/v1/productions failed with HTTP {status}")
        return body

    def production(self, production_id):
        status, body = self.get(f"/api/v1/productions/{production_id}")
        return body if status == 200 and isinstance(body, dict) else None

    def create_production(self, name, source_id):
        made = self.call("POST", "/api/v1/productions", {"name": name})
        production_id = (made or {}).get("_id") or (made or {}).get("id")
        if not production_id:
            fail("Open Live created a production but returned no id")
        self.call("POST", f"/api/v1/productions/{production_id}/sources",
                  {"sourceId": source_id, "mixerInput": MIXER_INPUT})
        return production_id

    def deactivate(self, production_id):
        http("POST", f"{self.url}/api/v1/productions/{production_id}/deactivate", self.bearer(), {},
             timeout=DEACTIVATE_TIMEOUT)
        deadline = time.monotonic() + DEACTIVATE_TIMEOUT
        while time.monotonic() < deadline:
            p = self.production(production_id)
            if p is None or p.get("status") in ("inactive", "ended"):
                return True
            time.sleep(2)
        return False

    def remove_production(self, production_id):
        self.deactivate(production_id)
        status, _ = http("DELETE", f"{self.url}/api/v1/productions/{production_id}", self.bearer())
        return status in (200, 202, 204, 404)

    def remove_source(self, source_id):
        status, _ = http("DELETE", f"{self.url}/api/v1/sources/{source_id}", self.bearer())
        return status in (200, 202, 204, 404)

    def sweep(self, label):
        left = 0
        for p in self.productions():
            if stale(p.get("name"), label):
                left += 1
                self.remove_production(p.get("_id") or p.get("id"))
        for s in self.sources():
            if stale(s.get("name"), label):
                left += 1
                self.remove_source(s.get("_id") or s.get("id"))
        return left


# --- the production's audio meters -----------------------------------------------------


class MeterWatch(threading.Thread):
    """The watch-only controller socket of a production: every channel meter it relays.
    A watcher never writes (Open Live's src/ws/controller.ts)."""

    def __init__(self, url, production_id, headers):
        super().__init__(daemon=True)
        self.url = url
        self.production_id = production_id
        self.headers = headers
        self.meters = []
        self.frames = 0
        self.error = None
        self.stopping = threading.Event()

    def stop(self):
        self.stopping.set()

    def connect(self):
        parts = urllib.parse.urlsplit(self.url)
        host = parts.hostname
        port = parts.port or 443
        path = f"/ws/productions/{self.production_id}/controller?mode=watch"
        raw = socket.create_connection((host, port), timeout=15)
        sock = ssl.create_default_context().wrap_socket(raw, server_hostname=host)
        key = base64.b64encode(os.urandom(16)).decode()
        lines = [
            f"GET {path} HTTP/1.1", f"Host: {host}", "Upgrade: websocket", "Connection: Upgrade",
            f"Sec-WebSocket-Key: {key}", "Sec-WebSocket-Version: 13",
        ] + [f"{k}: {v}" for k, v in self.headers.items()]
        sock.sendall(("\r\n".join(lines) + "\r\n\r\n").encode())
        data = b""
        while b"\r\n\r\n" not in data:
            chunk = sock.recv(4096)
            if not chunk:
                fail("the meter socket closed during the handshake")
            data += chunk
        head, rest = data.split(b"\r\n\r\n", 1)
        status_line = head.split(b"\r\n", 1)[0].decode(errors="replace")
        if " 101 " not in f"{status_line} ":
            fail(f"the meter socket was refused: {status_line.split(' ', 2)[1] if ' ' in status_line else '?'}")
        if ws_accept(key).encode() not in head:
            fail("the meter socket answered with a wrong Sec-WebSocket-Accept")
        return sock, rest

    def run(self):
        try:
            sock, buffer = self.connect()
        except (Failed, OSError, ssl.SSLError) as err:
            self.error = str(err) if isinstance(err, Failed) else type(err).__name__
            return
        sock.settimeout(1.0)
        message = b""
        try:
            while not self.stopping.is_set():
                frame = ws_parse(buffer)
                if frame is None:
                    try:
                        chunk = sock.recv(65536)
                    except socket.timeout:
                        continue
                    if not chunk:
                        break
                    buffer += chunk
                    continue
                fin, opcode, payload, buffer = frame
                if opcode == 0x8:
                    break
                if opcode == 0x9:
                    sock.sendall(ws_frame(0xA, payload))
                    continue
                if opcode in (0x1, 0x0):
                    message += payload
                    if fin:
                        self.take(message)
                        message = b""
            try:
                sock.sendall(ws_frame(0x8, b"\x03\xe8"))
            except OSError:
                pass
        except (OSError, ssl.SSLError) as err:
            self.error = type(err).__name__
        finally:
            sock.close()

    def take(self, raw):
        self.frames += 1
        try:
            m = json.loads(raw)
        except ValueError:
            return
        if isinstance(m, dict) and m.get("type") == "METER_DATA":
            element = str(m.get("elementId", ""))
            if element.startswith("ch"):
                self.meters.append((time.monotonic(), element, meter_peak(m.get("peak"))))


# --- the control server ----------------------------------------------------------------


class Session:
    """What the control server knows and does: the production, the looks at Open Live,
    the samples and checks the test posts."""

    def __init__(self, ol, label, udid, pat, seconds, instance):
        self.ol = ol
        self.label = label
        self.udid = udid
        self.pat = pat
        self.seconds = seconds
        self.instance = instance
        self.production_id = None
        self.production_status = None
        self.production_error = None
        self.busy = None  # "activating" / "deactivating" while a thread works
        self.sources = []
        self.sources_at = None
        self.polls = []  # (t, status, resolution)
        self.checks = []
        self.phases = {}
        self.samples = {}
        self.windows = {}
        self.watch = None
        self.lock = threading.Lock()
        self.stopping = threading.Event()
        self.poller = threading.Thread(target=self.poll, daemon=True)

    def poll(self):
        while not self.stopping.is_set():
            try:
                found = ours(self.ol.sources(), self.label)
                with self.lock:
                    self.sources, self.sources_at = found, time.monotonic()
                if self.production_id:
                    p = self.ol.production(self.production_id)
                    if p is not None:
                        res = decoding(p.get("inputResolutions"))
                        with self.lock:
                            self.production_status = p.get("status")
                            self.polls.append((time.monotonic(), p.get("status"), res))
            except Failed as err:
                say(f"a look at Open Live failed (retried): {err}")
            self.stopping.wait(POLL_EVERY)

    def pasteboard(self, text):
        subprocess.run(["xcrun", "simctl", "pbcopy", self.udid], input=text.encode(), check=True,
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)

    def activate(self, source_id):
        try:
            if not self.production_id:
                self.production_id = self.ol.create_production(f"{self.label} — production", source_id)
                say(f"production {self.production_id} created, holding source {source_id} on {MIXER_INPUT}")
            started = time.monotonic()
            self.ol.call("POST", f"/api/v1/productions/{self.production_id}/activate", {}, timeout=ACTIVATE_TIMEOUT)
            while time.monotonic() - started < ACTIVATE_TIMEOUT:
                p = self.ol.production(self.production_id)
                state = p.get("status") if p else None
                if state == "active":
                    say(f"production active after {time.monotonic() - started:.1f} s")
                    self.start_meters()
                    return
                if state not in (None, "activating", "inactive"):
                    fail(f"the production did not activate: its status went to '{state}'")
                time.sleep(1)
            fail(f"the production was not active after {ACTIVATE_TIMEOUT} s")
        except Failed as err:
            self.production_error = str(err)
            say(str(err))
        finally:
            self.busy = None

    def deactivate(self):
        try:
            self.stop_meters()
            if self.production_id and not self.ol.deactivate(self.production_id):
                self.production_error = f"the production was not inactive {DEACTIVATE_TIMEOUT} s after deactivate"
        except Failed as err:
            self.production_error = str(err)
        finally:
            self.busy = None

    def start_meters(self):
        self.stop_meters()
        self.watch = MeterWatch(self.ol.url, self.production_id, self.ol.bearer())
        self.watch.start()

    def stop_meters(self):
        if self.watch:
            self.watch.stop()
            self.watch.join(5)

    def measure_stop(self, phase):
        with self.lock:
            start = self.windows.get(phase, time.monotonic())
            end = time.monotonic()
            samples = list(self.samples.get(phase, []))
            polls = list(self.polls)
        gap = interval(samples, self.seconds)
        result = {
            "samples": len(samples),
            "interval": gap,
            "bitrate": peak_to_average(samples),
            "decode": decode_summary(polls, start, end),
        }
        problems = list(gap["problems"])
        if not result["decode"]["resolution"]:
            problems.append("the cloud Strom never showed a picture for the input")
        watch = self.watch
        if watch is not None:
            meters = [m for m in watch.meters if start <= m[0] <= end]
            verdict, detail = audio_verdict(meters)
            if watch.error:
                detail += f"; the meter socket: {watch.error}"
            result["audio"] = (verdict, detail)
            if verdict == "silent":
                problems.append(f"the cloud side shows no sound: {detail}")
            elif verdict == "unreported":
                print(f"::warning title=e2e ios::sound not checked: the instance relayed no channel meter ({detail})",
                      flush=True)
        result["problems"] = problems
        result["ok"] = not problems
        self.phases[phase] = result
        return result

    def handle(self, method, path, args):
        if method == "GET" and path == "/config":
            return {"instance": self.instance, "label": self.label, "seconds": self.seconds,
                    "armed_hold": ARMED_HOLD, "background_limit": BACKGROUND_LIMIT,
                    "phone_background_bound": PHONE_BACKGROUND_BOUND}
        if method == "POST" and path == "/pasteboard/token":
            self.pasteboard(self.pat)
            return {"ok": True}
        if method == "POST" and path == "/pasteboard/clear":
            self.pasteboard("")
            return {"ok": True}
        if method == "GET" and path == "/source":
            with self.lock:
                age = None if self.sources_at is None else time.monotonic() - self.sources_at
                return {"sources": self.sources, "age": age}
        if method == "GET" and path == "/production":
            with self.lock:
                last = self.polls[-1] if self.polls else None
            return {"id": self.production_id, "status": self.production_status, "busy": self.busy,
                    "error": self.production_error,
                    "resolution": f"{last[2][0]}x{last[2][1]}" if last and last[2] else None}
        if method == "POST" and path == "/production/activate":
            if self.busy:
                fail(f"the production is {self.busy}")
            source = args.get("source") or ""
            if not source:
                fail("activate needs ?source=<id>")
            self.busy, self.production_error = "activating", None
            threading.Thread(target=self.activate, args=(source,), daemon=True).start()
            return {"ok": True}
        if method == "POST" and path == "/production/deactivate":
            if self.busy:
                fail(f"the production is {self.busy}")
            self.busy, self.production_error = "deactivating", None
            threading.Thread(target=self.deactivate, daemon=True).start()
            return {"ok": True}
        if method == "POST" and path == "/measure/start":
            phase = args.get("phase") or "measure"
            with self.lock:
                self.windows[phase] = time.monotonic()
                self.samples[phase] = []
            return {"ok": True, "seconds": self.seconds}
        if method == "POST" and path == "/sample":
            phase = args.get("phase") or "measure"

            def number(key):
                value = args.get(key, "")
                return int(value) if value.isdigit() else None

            with self.lock:
                self.samples.setdefault(phase, []).append({
                    "t": time.monotonic(), "sent": number("sent"), "dropped": number("dropped"),
                    "bytes": number("bytes")})
            return {"ok": True}
        if method == "POST" and path == "/measure/stop":
            result = self.measure_stop(args.get("phase") or "measure")
            gap = result["interval"]
            return {"ok": result["ok"], "problems": result["problems"],
                    "drop_share": gap.get("drop_share"), "packets_sent": gap.get("packets_sent"),
                    "resolution": result["decode"]["resolution"],
                    "audio": (result.get("audio") or ("unreported", ""))[0]}
        if method == "POST" and path == "/check":
            check = {"id": args.get("id", "?"), "ok": args.get("ok") == "1", "detail": args.get("detail", "")}
            with self.lock:
                self.checks.append(check)
            say(f"{'PASS' if check['ok'] else 'FAIL'}: {check['id']}: {check['detail']}")
            return {"ok": True}
        fail(f"no such control request: {method} {path}")


def serve(session):
    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def answer(self, method):
            path, args = query(self.path)
            try:
                body, code = session.handle(method, path, args), 200
            except Failed as err:
                body, code = {"error": str(err)}, 500
            except Exception as err:  # noqa: BLE001 - the test gets the reason, the run goes on
                body, code = {"error": f"{type(err).__name__}"}, 500
            data = json.dumps(body).encode()
            self.send_response(code)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(data)))
            self.send_header("Connection", "close")
            self.end_headers()
            self.wfile.write(data)

        def do_GET(self):  # noqa: N802
            self.answer("GET")

        def do_POST(self):  # noqa: N802
            length = int(self.headers.get("Content-Length") or 0)
            if length:
                self.rfile.read(length)
            self.answer("POST")

        def log_message(self, *_):
            pass

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server


# --- xcodebuild ------------------------------------------------------------------------


def child_env(extra):
    env = {k: v for k, v in os.environ.items() if k not in WITHHELD_ENV}
    env.update(extra)
    return env


def run_test(udid, derived, out, control_url):
    """xcodebuild test-without-building of the end-to-end test only. (exit code, output)."""
    bundle = os.path.join(out, "e2e.xcresult")
    if os.path.exists(bundle):
        subprocess.run(["rm", "-rf", bundle], check=True)
    cmd = [
        "xcodebuild", "test-without-building", "-project", PROJECT, "-scheme", SCHEME,
        "-destination", f"platform=iOS Simulator,id={udid}", "-derivedDataPath", derived,
        f"-only-testing:{TEST_ID}", "-parallel-testing-enabled", "NO", "-resultBundlePath", bundle,
    ]
    extra = {"TEST_RUNNER_OLI_E2E_CONTROL_URL": control_url} if control_url else {}
    lines = []
    with open(os.path.join(out, "xcodebuild-test.log"), "w", encoding="utf-8") as log:
        proc = subprocess.Popen(cmd, env=child_env(extra), stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                                text=True, errors="replace")
        timer = threading.Timer(TEST_TIMEOUT, proc.kill)
        timer.start()
        try:
            for line in proc.stdout:
                print(line, end="", flush=True)
                log.write(line)
                lines.append(line)
            code = proc.wait()
        finally:
            timer.cancel()
    return code, "".join(lines)


def version_of(cmd):
    try:
        return subprocess.run(cmd, capture_output=True, text=True, check=False).stdout.strip().splitlines()[0]
    except (OSError, IndexError):
        return None


def env_int(name, default, floor):
    raw = os.environ.get(name, "").strip()
    if not raw:
        return default
    if not raw.isdigit():
        fail(f"{name} must be a whole number of seconds (got '{raw}')")
    value = int(raw)
    if value < floor:
        fail(f"{name} may not be under {floor} (got {value})")
    return value


def simulator():
    listing = json.loads(subprocess.run(["xcrun", "simctl", "list", "devices", "available", "--json"],
                                        capture_output=True, text=True, check=True).stdout)
    picked = pick_simulator(listing)
    if picked is None:
        print("no available iPhone simulator", file=sys.stderr)
        return 1
    print(picked[1], file=sys.stderr)
    print(picked[0])
    return 0


def run():
    pat = os.environ.get("OSC_ACCESS_TOKEN", "").strip()
    mask(pat)
    instance = os.environ.get("OPEN_LIVE_INSTANCE", "").strip() or "openlivedev"
    udid = os.environ.get("IOS_UDID", "").strip()
    derived = os.environ.get("E2E_DERIVED", "").strip()
    out = os.environ.get("E2E_OUT") or os.path.join(os.environ.get("RUNNER_TEMP", "/tmp"), "e2e-ios")
    os.makedirs(out, exist_ok=True)
    if not udid or not derived:
        fail("IOS_UDID and E2E_DERIVED must be set (see the workflow e2e-ios.yml)")
    seconds = env_int("E2E_SRT_INTERVAL", MIN_SECONDS, MIN_SECONDS)
    label = f"{PREFIX}{os.environ.get('GITHUB_RUN_ID', int(time.time()))}-{os.environ.get('GITHUB_RUN_ATTEMPT', '1')}"
    meta = {
        "instance": instance, "label": label, "seconds": seconds,
        "sha": os.environ.get("GITHUB_SHA", "local"),
        "simulator": os.environ.get("IOS_SIMULATOR", udid),
        "xcode": (version_of(["xcodebuild", "-version"]) or "Xcode ?"),
    }
    if os.environ.get("GITHUB_RUN_ID"):
        meta["run_url"] = (
            f"{os.environ.get('GITHUB_SERVER_URL', 'https://github.com')}/"
            f"{os.environ.get('GITHUB_REPOSITORY', '')}/actions/runs/{os.environ['GITHUB_RUN_ID']}"
        )

    if not pat:
        say("no OSC_ACCESS_TOKEN: running the end-to-end test once with no control server, where it skips itself")
        code, output = run_test(udid, derived, out, None)
        if code != 0:
            fail(f"the end-to-end test bundle did not run (xcodebuild exit {code}); see xcodebuild-test.log")
        if TEST_CASE not in output:
            fail(f"xcodebuild ran no {TEST_CASE}: the -only-testing name {TEST_ID} matches nothing")
        missing = (
            "missing capability: the repository secret OSC_ACCESS_TOKEN (an Open Source Cloud personal access "
            f"token for the workspace that owns the Open Live instance '{instance}') is not set"
        )
        with open(SUMMARY, "a", encoding="utf-8") as f:
            f.write(f"## iPhone app end to end (OL-64h)\n\n- The test bundle ran on the simulator and the "
                    f"end-to-end test skipped itself.\n- SKIPPED: {missing}. Nothing ran against '{instance}'.\n")
        if os.environ.get("E2E_REQUIRE_SECRET", "true") == "true":
            fail(missing)
        print(f"::warning title=e2e ios::{missing}; the test bundle ran and skipped, nothing ran against '{instance}'",
              flush=True)
        return 0

    ol = OpenLive(pat)
    session = None
    server = None
    code = 1
    cleanup = "nothing was created"
    try:
        ol.resolve(instance)
        say(f"instance '{instance}' found")
        left = ol.sweep(label)
        if left:
            say(f"removed {left} production(s)/source(s) left by an earlier run")
        session = Session(ol, label, udid, pat, seconds, instance)
        session.poller.start()
        server = serve(session)
        control = f"http://127.0.0.1:{server.server_address[1]}"
        say(f"control server at {control}")
        code, _ = run_test(udid, derived, out, control)
        if code != 0:
            print(f"::error title=e2e ios::the end-to-end test failed (xcodebuild exit {code}); "
                  "see the log above and e2e.xcresult in the artifact", flush=True)
    except Failed as err:
        print(f"::error title=e2e ios::{err}", flush=True)
        code = 1
    finally:
        problems = []
        if session is not None:
            session.stopping.set()
            session.stop_meters()
            try:
                session.pasteboard("")
            except (OSError, subprocess.CalledProcessError):
                problems.append("the simulator's pasteboard could not be emptied")
        if server is not None:
            server.shutdown()
        if ol.url:
            try:
                if session is not None and session.production_id:
                    if not ol.remove_production(session.production_id):
                        problems.append(f"production {session.production_id}")
                for s in ours(ol.sources(), label):
                    if not ol.remove_source(s["id"]):
                        problems.append(f"source {s['id']}")
                if any(ours(ol.sources(), label)):
                    problems.append("a source of the run is still listed")
                cleanup = ("the test production and source were deactivated and deleted"
                           if not problems else f"could not remove {', '.join(problems)}")
            except Failed as err:
                cleanup = f"could not finish: {err}"
                problems.append(cleanup)
        if problems:
            print(f"::error title=e2e ios::cleanup: {cleanup}", flush=True)
            code = 1

    checks = session.checks if session else []
    phases = session.phases if session else {}
    if any(not c["ok"] for c in checks) or any(not p["ok"] for p in phases.values()):
        code = 1
    passed = code == 0
    text = report(meta, checks, phases, cleanup)
    with open(os.path.join(out, "results.json"), "w", encoding="utf-8") as f:
        json.dump({"meta": meta, "checks": checks, "phases": phases, "cleanup": cleanup, "passed": passed},
                  f, indent=2, default=str)
    with open(os.path.join(out, "report.md"), "w", encoding="utf-8") as f:
        f.write(text + "\n")
    with open(os.path.join(out, "comment.md"), "w", encoding="utf-8") as f:
        f.write(comment(meta, checks, phases, cleanup, passed) + "\n")
    with open(SUMMARY, "a", encoding="utf-8") as f:
        f.write(text + "\n")
    print(text, flush=True)
    return code


def main():
    what = sys.argv[1] if len(sys.argv) == 2 else ""
    if what == "simulator":
        return simulator()
    if what != "run":
        print(f"usage: {sys.argv[0]} simulator|run", file=sys.stderr)
        return 2
    try:
        return run()
    except Failed as err:
        print(f"::error title=e2e ios::{err}", flush=True)
        return 1


if __name__ == "__main__":
    sys.exit(main())
