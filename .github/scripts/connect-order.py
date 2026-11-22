#!/usr/bin/env python3
"""The connect-order measurement against an Open Live instance (OL-64-0, #136;
docs/specs/ios-app.md section 14 Q13 and Q8). Run by the `connect-order` workflow
(workflow_dispatch only, outside the release path) on macos-latest:

    python3 -I .github/scripts/connect-order.py

Does a caller that dials before the production holding its source is active stall the
way Eyevinn/strom#1277 does, and what do drops, RTT and send rate look like at the
phone defaults (spec section 11)? Two runs, each with its own SRT listener source:

  A, production first: the production is activated, and once Open Live says it is
     `active` the caller connects.
  B, caller first: the caller dials, CO_CALLER_LEAD seconds (default 30, never under
     10) go by with nothing listening, then the production is activated.

The sender is ffmpeg (test pattern and a tone: 1280x720 at 30 fps, H.264 High, CBR
4000 kbps, a keyframe every 30 frames, no B-frames, AAC-LC 48 kHz stereo 128 kbps,
MPEG-TS) piped into srt-sender (.github/scripts/srt-sender.c, libsrt with the phone's
socket options: caller, latency 200, payload size 1316, SRTO_INPUTBW at the CBR rate,
SRTO_OHEADBW 25 %), which prints libsrt's sender stats once a second.

In each run the sender stats are read twice, CO_SECONDS apart (default 60, never
under 60): 1 s after the connect and CO_SECONDS later. Each reading gives bytes and
packets sent, packets dropped (too late to send), the drop share
packets_sent_dropped / packets_sent, RTT and send rate; the share over the interval
between the readings is given too. The receiving side is read from Open Live:
`GET /api/v1/productions/<id>` of an active production carries `inputResolutions`,
the cloud Strom's vision mixer's negotiated resolution per input (Open Live's
routes/productions.ts), which stays null for an input that decodes nothing. It is
polled every 2 s from the activation to the end of the run.

A run counts as stalled when, over the interval, more than STALL_DROP_SHARE of what
was sent was dropped (strom#1277's signature: drops as fast as sends at a steady
RTT), or when the input was never seen decoding while Open Live reported it. The raw
per-second stats are kept for the reader to judge.

Open Live calls (spec section 6): the OSC catalog (`mysubscriptions`, the
eyevinn-open-live `apiUrl`) and a service token from OSC_ACCESS_TOKEN, the instance
named OPEN_LIVE_INSTANCE (default openlivedev) from the instance list,
`GET /api/v1/server-info` (stromHost, srtPorts), `POST /api/v1/sources` with
`srt://:0?mode=listener` (Open Live picks the port from the ones it holds),
`POST /api/v1/productions` and `/sources` (the source on video_in_0),
`POST .../activate` and `.../deactivate`, `GET /api/v1/productions/<id>`, and
`DELETE` of both at the end. Whatever happens, the exit path deactivates and deletes
the run's productions and sources, and sources and productions named
`oli-connect-order-` left by an earlier run that was killed are removed first.

Writes, under CO_OUT (default $RUNNER_TEMP/connect-order): `results.json` (everything,
the per-second stats included), `report.md` (also appended to the step summary) and
`comment.md` (a draft of the comment for #133). Exits 1 when a run could not be
completed; a stall or drops above 1 % are results, not failures.

Python standard library only. The OSC token, the service token, the instance address
and the cloud Strom's host are masked in the log as soon as they are known; no
response body is printed. The pure parts are unit tested:
`python3 -I .github/scripts/test_connect_order.py`.
"""

import json
import os
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.request

CATALOG = "https://catalog.svc.prod.osaas.io/mysubscriptions"
TOKEN_EXCHANGE = "https://token.svc.prod.osaas.io/servicetoken"
SERVICE_ID = "eyevinn-open-live"
PREFIX = "oli-connect-order-"
MIXER_INPUT = "video_in_0"
MIXER_INDEX = 0
LATENCY_MS = 200
VIDEO_KBPS = 4000
AUDIO_KBPS = 128
# What SRTO_INPUTBW is set to: the CBR rate, in bytes per second (spec section 11).
INPUT_BW = (VIDEO_KBPS + AUDIO_KBPS) * 1000 // 8
MIN_SECONDS = 60
MIN_LEAD = 10
DROP_LIMIT = 0.01
STALL_DROP_SHARE = 0.5
ACTIVATE_TIMEOUT = 300
DEACTIVATE_TIMEOUT = 180
# How long the caller has to connect once the production is active.
CONNECT_TIMEOUT = 120
POLL_EVERY = 2
RUNS = {
    "A": "production first",
    "B": "caller first",
}

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


def listener_port(address):
    """The port of a hostless listener address, as src/openlive.rs reads it:
    `srt://:47100?mode=listener` gives 47100; anything else, or port 0, None."""
    rest = str(address or "").strip()
    if not rest.startswith("srt://:"):
        return None
    rest = rest[len("srt://:"):]
    digits = ""
    for c in rest:
        if not c.isdigit():
            break
        digits += c
    after = rest[len(digits):]
    if not digits or not (after == "" or after.startswith("?")):
        return None
    port = int(digits)
    return port if 0 < port < 65536 else None


def find_instance(instances, name):
    """The `url` of the instance named `name` in the OSC instance list."""
    for item in instances if isinstance(instances, list) else []:
        if isinstance(item, dict) and item.get("name") == name and item.get("url"):
            return str(item["url"])
    return None


def decoding(resolutions, index=MIXER_INDEX):
    """(width, height) the mixer negotiated for the input, or None."""
    if not isinstance(resolutions, list) or index >= len(resolutions):
        return None
    value = resolutions[index]
    if isinstance(value, dict) and value.get("width") and value.get("height"):
        return (int(value["width"]), int(value["height"]))
    return None


def share(part, whole):
    return (part / whole) if whole else None


def reading(stats):
    """One sender stats reading, in the issue's terms."""
    sent = stats.get("packets_sent", 0)
    dropped = stats.get("packets_sent_dropped", 0)
    return {
        "t": stats["t"],
        "connects": stats.get("connects"),
        "connected": stats.get("connected"),
        "bytes_sent": stats.get("bytes_sent"),
        "packets_sent": sent,
        "packets_sent_dropped": dropped,
        "drop_share": share(dropped, sent),
        "packets_sent_lost": stats.get("packets_sent_lost"),
        "packets_retransmitted": stats.get("packets_retransmitted"),
        "acks_received": stats.get("acks_received"),
        "rtt_ms": stats.get("rtt_ms"),
        "send_rate_mbps": stats.get("send_rate_mbps"),
    }


def pick_readings(stats, connected_at, seconds):
    """The two readings: the first stats line at least 1 s after the connect, and the
    first at least `seconds` after that one. Either may be None."""
    first = next((s for s in stats if s["t"] >= connected_at + 1000), None)
    if first is None:
        return None, None
    second = next((s for s in stats if s["t"] >= first["t"] + seconds * 1000), None)
    return first, second


def interval(first, second):
    """What happened between two readings of the same connection, or None when the
    link was redialled in between (libsrt's counters are per connection)."""
    if not first or not second or first.get("connects") != second.get("connects"):
        return None
    sent = second["packets_sent"] - first["packets_sent"]
    dropped = second["packets_sent_dropped"] - first["packets_sent_dropped"]
    seconds = (second["t"] - first["t"]) / 1000
    nbytes = (second.get("bytes_sent") or 0) - (first.get("bytes_sent") or 0)
    return {
        "seconds": seconds,
        "packets_sent": sent,
        "packets_sent_dropped": dropped,
        "drop_share": share(dropped, sent),
        "acks_received": (second.get("acks_received") or 0) - (first.get("acks_received") or 0),
        "send_rate_mbps": (nbytes * 8 / seconds / 1e6) if seconds > 0 else None,
    }


def decode_summary(polls, active_at, connected_at, end_at):
    """What the receiving side showed. `polls` are (t ms, status, resolution or None,
    whether Open Live reported `inputResolutions` at all)."""
    reported = [p for p in polls if p[3]]
    seen = [p for p in polls if p[2] is not None]
    first = seen[0] if seen else None
    after_connect = [p for p in reported if connected_at is not None and connected_at <= p[0] <= end_at]
    decoding_after = [p for p in after_connect if p[2] is not None]
    # The longest gap with nothing decoded, after the first frame was seen.
    longest_gap = 0.0
    if first:
        last_seen = first[0]
        for t, _status, res, rep in polls:
            if not rep:
                continue
            if res is not None:
                last_seen = t
            else:
                longest_gap = max(longest_gap, (t - last_seen) / 1000)
    return {
        "polls": len(polls),
        "polls_reporting": len(reported),
        "reported": bool(reported),
        "first_seen_after_active_s": None if not first or active_at is None else round((first[0] - active_at) / 1000, 1),
        "first_seen_after_connect_s": None if not first or connected_at is None else round((first[0] - connected_at) / 1000, 1),
        "resolution": f"{first[2][0]}x{first[2][1]}" if first else None,
        "decoding_share_after_connect": share(len(decoding_after), len(after_connect)),
        "longest_gap_after_first_s": round(longest_gap, 1),
    }


def verdict(gap, decode, broken):
    """(stalled, reasons, over_drop_limit)."""
    reasons = []
    if gap is None:
        reasons.append("the link was redialled or a reading is missing, so there is no interval")
    elif gap["drop_share"] is None:
        reasons.append("nothing was sent between the readings")
    elif gap["drop_share"] > STALL_DROP_SHARE:
        reasons.append(f"{gap['drop_share']:.1%} of the packets sent between the readings were dropped")
    if decode["reported"] and decode["resolution"] is None:
        reasons.append("Open Live never showed the input decoding")
    if broken:
        reasons.append(f"the link broke {broken} time(s)")
    stalled = bool(reasons)
    over = gap is not None and gap["drop_share"] is not None and gap["drop_share"] > DROP_LIMIT
    return stalled, reasons, over


def pct(value):
    return "n/a" if value is None else f"{value:.3%}"


def num(value, fmt="{:,}"):
    return "n/a" if value is None else fmt.format(value)


def run_table(result):
    rows = []
    for label, r in (("1st", result.get("reading_1")), ("2nd", result.get("reading_2"))):
        if not r:
            rows.append(f"| {result['run']} | {label} | n/a | n/a | n/a | n/a | n/a | n/a |")
            continue
        rows.append(
            f"| {result['run']} | {label} | {num(r['bytes_sent'])} | {num(r['packets_sent'])} | "
            f"{num(r['packets_sent_dropped'])} | {pct(r['drop_share'])} | {num(r['rtt_ms'], '{:.2f}')} | "
            f"{num(r['send_rate_mbps'], '{:.2f}')} |"
        )
    gap = result.get("interval")
    if gap:
        rows.append(
            f"| {result['run']} | interval ({gap['seconds']:.0f} s) | | {num(gap['packets_sent'])} | "
            f"{num(gap['packets_sent_dropped'])} | {pct(gap['drop_share'])} | | "
            f"{num(gap['send_rate_mbps'], '{:.2f}')} (avg) |"
        )
    return rows


def report(results, meta):
    out = [
        f"## Connect-order measurement (OL-64-0) against `{meta['instance']}`",
        "",
        f"Run: {meta.get('run_url', 'local')}. Sender: ffmpeg {meta.get('ffmpeg', '?')} into srt-sender "
        f"(libsrt {meta.get('libsrt', '?')}) on {meta.get('runner', '?')}. Phone defaults (spec §11): "
        f"1280x720 30 fps, H.264 High CBR {VIDEO_KBPS} kbps, keyframe every 30 frames, no B-frames, "
        f"AAC-LC 48 kHz stereo {AUDIO_KBPS} kbps, MPEG-TS; SRT caller, latency {LATENCY_MS} ms, payload 1316, "
        f"SRTO_INPUTBW {INPUT_BW} B/s, SRTO_OHEADBW 25 %. Readings {meta['seconds']} s apart.",
        "",
        "| run | reading | bytes sent | packets sent | dropped | drop % | RTT ms | send rate Mbps |",
        "|---|---|---|---|---|---|---|---|",
    ]
    for r in results:
        out.extend(run_table(r))
    out.append("")
    for r in results:
        d = r.get("decode") or {}
        out.append(f"**Run {r['run']} ({RUNS[r['run']]})**")
        if r.get("error"):
            out.append(f"- Did not complete: {r['error']}")
        out.append(f"- Activation request to `active`: {num(r.get('activation_s'), '{:.1f}')} s.")
        if r["run"] == "B":
            out.append(
                f"- The caller dialled {num(r.get('lead_s'), '{:.0f}')} s before the activation request; "
                f"connected before it: {'yes' if r.get('connected_before_activation') else 'no'}."
            )
        out.append(
            f"- Connected {num(r.get('connect_after_active_s'), '{:.1f}')} s after the production was `active` "
            f"(negative: before Open Live said so); connects {num(r.get('connects'))}, link breaks {num(r.get('broken'))}."
        )
        if d.get("reported"):
            out.append(
                f"- Receiving side (`inputResolutions[{MIXER_INDEX}]`): first decoded "
                f"{num(d.get('first_seen_after_active_s'))} s after `active` / "
                f"{num(d.get('first_seen_after_connect_s'))} s after the connect, at {d.get('resolution') or 'n/a'}; "
                f"decoding in {pct(d.get('decoding_share_after_connect'))} of the looks after the connect; "
                f"longest gap after the first frame {num(d.get('longest_gap_after_first_s'))} s "
                f"({d.get('polls_reporting')} of {d.get('polls')} looks carried the field)."
            )
        else:
            out.append(f"- Receiving side: Open Live did not report `inputResolutions` ({d.get('polls', 0)} looks).")
        stalled = r.get("stalled")
        out.append(
            f"- Stalled: **{'yes' if stalled else 'no'}**"
            + (f" ({'; '.join(r.get('reasons', []))})" if r.get("reasons") else "")
            + f". Drops above 1 % over the interval: **{'yes' if r.get('over_drop_limit') else 'no'}**."
        )
        out.append("")
    return "\n".join(out)


def comment(results, meta):
    """A draft of the #133 comment; the agent that dispatched the run checks and posts
    it."""
    by = {r["run"]: r for r in results}
    b = by.get("B")
    a = by.get("A")
    lines = [
        "<!-- ol-64-0 connect-order results -->",
        report(results, meta),
        "### Answers (#136 acceptance)",
        "",
    ]
    if b is not None and not b.get("error"):
        lines.append(f"- **Does caller-first stall?** {'Yes' if b.get('stalled') else 'No'}, from run B above.")
        if b.get("stalled"):
            sha = meta.get("sha", "main")
            base = f"https://github.com/Eyevinn/open-live-ingest/blob/{sha}"
            lines.append(
                "- The arm-until-live gate (#130) is what keeps the phone out of this: the uplink of an armed "
                f"session runs only while `gate::Verdict::send` ([`src/gate.rs`]({base}/src/gate.rs)) is true, "
                "that is while a production holding the source is `active` (not `activating`), and "
                f"`run::up`'s armed loop ([`src/run.rs`]({base}/src/run.rs), `gate::Watch`) starts the uplink only "
                "then, so the phone, which drives the same session (spec §4), never dials before that."
            )
    else:
        lines.append("- **Does caller-first stall?** Not answered: run B did not complete.")
    over = [r["run"] for r in results if r.get("over_drop_limit")]
    if over:
        lines.append(
            f"- Drops above 1 % at the defaults in run(s) {', '.join(over)}: input to OL-64d's defaults (#140) "
            "and to the adaptive-bitrate follow-up decision (spec §14 Q8)."
        )
    else:
        lines.append("- No run dropped more than 1 % at the defaults over its interval.")
    if a is not None and a.get("error"):
        lines.append(f"- Run A did not complete: {a['error']}")
    lines.append(f"- Cleanup: {meta.get('cleanup', 'unknown')}")
    lines.append("")
    lines.append("Raw per-second sender stats: the `connect-order` artifact of the run.")
    return "\n".join(lines)


# --- Open Live -------------------------------------------------------------------------


class OpenLive:
    def __init__(self, pat):
        self.pat = pat
        self.token = None
        self.expires = 0.0
        self.url = None
        self.sources_made = []
        self.productions_made = []

    def bearer(self):
        if self.token is None or time.time() > self.expires - 120:
            status, body = http(
                "POST",
                TOKEN_EXCHANGE,
                {"x-pat-jwt": f"Bearer {self.pat}", "accept": "application/json"},
                {"serviceId": SERVICE_ID},
            )
            if status != 200 or not isinstance(body, dict) or not body.get("token"):
                fail(f"the OSC token service refused OSC_ACCESS_TOKEN (HTTP {status})")
            self.token = body["token"]
            mask(self.token)
            expiry = body.get("expiry")
            try:
                expiry = float(expiry)
                # Seconds or milliseconds since the epoch.
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
            fail("the workspace of OSC_ACCESS_TOKEN has no Open Live subscription")
        self.bearer()
        status, instances = http("GET", api, {"x-jwt": f"Bearer {self.token}"})
        if status != 200:
            fail(f"listing the Open Live instances failed with HTTP {status}")
        url = find_instance(instances, name)
        if not url:
            fail(f"the workspace has no Open Live instance named '{name}'")
        mask_url(url)
        self.url = url.rstrip("/")

    def call(self, method, path, body=None, ok=(200, 201, 202, 204), timeout=30):
        status, data = http(method, f"{self.url}{path}", self.bearer(), body, timeout=timeout)
        if status not in ok:
            fail(f"{method} {path.split('?')[0]} failed with HTTP {status}")
        return data

    def server_info(self):
        info = self.call("GET", "/api/v1/server-info") or {}
        host = info.get("stromHost")
        if not host:
            fail("Open Live reported no stromHost in server-info")
        mask(host)
        return {"stromHost": host, "srtPorts": info.get("srtPorts"), "srtPortState": info.get("srtPortState")}

    def create_source(self, name):
        body = {
            "name": name,
            "address": "srt://:0?mode=listener",
            "streamType": "srt",
            "status": "active",
            "latency": LATENCY_MS,
            "liveCamera": True,
        }
        made = self.call("POST", "/api/v1/sources", body)
        source_id = (made or {}).get("_id") or (made or {}).get("id")
        if not source_id:
            fail("Open Live created a source but returned no id")
        self.sources_made.append(source_id)
        port = listener_port((made or {}).get("address"))
        if port is None:
            fail("Open Live gave the source no listener port")
        return source_id, port

    def create_production(self, name, source_id):
        made = self.call("POST", "/api/v1/productions", {"name": name})
        production_id = (made or {}).get("_id") or (made or {}).get("id")
        if not production_id:
            fail("Open Live created a production but returned no id")
        self.productions_made.append(production_id)
        self.call("POST", f"/api/v1/productions/{production_id}/sources", {"sourceId": source_id, "mixerInput": MIXER_INPUT})
        return production_id

    def production(self, production_id):
        status, body = http("GET", f"{self.url}/api/v1/productions/{production_id}", self.bearer())
        return body if status == 200 and isinstance(body, dict) else None

    def activate(self, production_id):
        self.call("POST", f"/api/v1/productions/{production_id}/activate", {}, timeout=ACTIVATE_TIMEOUT)
        deadline = time.monotonic() + ACTIVATE_TIMEOUT
        state = None
        while time.monotonic() < deadline:
            p = self.production(production_id)
            state = p.get("status") if p else state
            if state == "active":
                return
            if state not in (None, "activating", "inactive"):
                fail(f"the production did not activate: its status went to '{state}'")
            time.sleep(1)
        fail(f"the production was still '{state}' after {ACTIVATE_TIMEOUT} s")

    def remove_production(self, production_id):
        http("POST", f"{self.url}/api/v1/productions/{production_id}/deactivate", self.bearer(), {}, timeout=DEACTIVATE_TIMEOUT)
        deadline = time.monotonic() + DEACTIVATE_TIMEOUT
        while time.monotonic() < deadline:
            p = self.production(production_id)
            if p is None or p.get("status") in ("inactive", "ended"):
                break
            time.sleep(2)
        status, _ = http("DELETE", f"{self.url}/api/v1/productions/{production_id}", self.bearer())
        gone = status in (200, 202, 204, 404)
        if gone and production_id in self.productions_made:
            self.productions_made.remove(production_id)
        return gone

    def remove_source(self, source_id):
        status, _ = http("DELETE", f"{self.url}/api/v1/sources/{source_id}", self.bearer())
        gone = status in (200, 202, 204, 404)
        if gone and source_id in self.sources_made:
            self.sources_made.remove(source_id)
        return gone

    def sweep(self):
        """Removes what an earlier, killed run left: productions first, as they hold
        the sources."""
        left = 0
        status, productions = http("GET", f"{self.url}/api/v1/productions", self.bearer())
        for p in productions if status == 200 and isinstance(productions, list) else []:
            if str(p.get("name", "")).startswith(PREFIX):
                left += 1
                self.remove_production(p.get("_id") or p.get("id"))
        status, sources = http("GET", f"{self.url}/api/v1/sources", self.bearer())
        for s in sources if status == 200 and isinstance(sources, list) else []:
            if str(s.get("name", "")).startswith(PREFIX):
                left += 1
                self.remove_source(s.get("_id") or s.get("id"))
        return left

    def cleanup(self):
        problems = []
        for production_id in list(self.productions_made):
            if not self.remove_production(production_id):
                problems.append(f"production {production_id}")
        for source_id in list(self.sources_made):
            if not self.remove_source(source_id):
                problems.append(f"source {source_id}")
        return problems


# --- the sender ------------------------------------------------------------------------

# Never handed to ffmpeg or srt-sender.
WITHHELD_ENV = ("OSC_ACCESS_TOKEN", "GH_TOKEN", "GITHUB_TOKEN")


def child_env():
    return {k: v for k, v in os.environ.items() if k not in WITHHELD_ENV}


def ffmpeg_args(ffmpeg):
    return [
        ffmpeg, "-hide_banner", "-nostats", "-loglevel", "warning",
        "-re", "-f", "lavfi", "-i", "testsrc2=size=1280x720:rate=30",
        "-re", "-f", "lavfi", "-i", "sine=frequency=1000:sample_rate=48000",
        "-map", "0:v", "-map", "1:a",
        "-c:v", "libx264", "-preset", "veryfast", "-tune", "zerolatency",
        "-profile:v", "high", "-pix_fmt", "yuv420p",
        "-b:v", f"{VIDEO_KBPS}k", "-minrate", f"{VIDEO_KBPS}k", "-maxrate", f"{VIDEO_KBPS}k",
        "-bufsize", f"{VIDEO_KBPS}k", "-x264-params", "nal-hrd=cbr:force-cfr=1",
        "-g", "30", "-keyint_min", "30", "-sc_threshold", "0", "-bf", "0",
        "-c:a", "aac", "-profile:a", "aac_low", "-b:a", f"{AUDIO_KBPS}k", "-ar", "48000", "-ac", "2",
        "-f", "mpegts", "-mpegts_flags", "+resend_headers", "-flush_packets", "1", "pipe:1",
    ]


class Sender:
    def __init__(self, host, port, work, run):
        self.lines = []
        self.lock = threading.Lock()
        self.connected = threading.Event()
        self.ffmpeg_log = open(os.path.join(work, f"ffmpeg-{run}.log"), "w")
        self.sender_log = open(os.path.join(work, f"srt-sender-{run}.log"), "w")
        ffmpeg = os.environ.get("CO_FFMPEG", "ffmpeg")
        sender = os.environ.get("CO_SENDER", os.path.join(work, "srt-sender"))
        self.encoder = subprocess.Popen(ffmpeg_args(ffmpeg), stdout=subprocess.PIPE, stderr=self.ffmpeg_log, stdin=subprocess.DEVNULL, env=child_env())
        self.sender = subprocess.Popen(
            [sender, host, str(port), str(LATENCY_MS), str(INPUT_BW)],
            stdin=self.encoder.stdout, stdout=subprocess.PIPE, stderr=self.sender_log, text=True, env=child_env(),
        )
        self.encoder.stdout.close()
        self.started = time.time() * 1000
        threading.Thread(target=self._read, daemon=True).start()

    def _read(self):
        for line in self.sender.stdout:
            try:
                item = json.loads(line)
            except ValueError:
                continue
            with self.lock:
                self.lines.append(item)
            if item.get("event") == "connected":
                say(f"caller connected (connect {item.get('connects')})")
                self.connected.set()
            elif item.get("event") == "broken":
                say(f"the link broke (connect {item.get('connects')})")

    def snapshot(self):
        with self.lock:
            return list(self.lines)

    def first_connect(self):
        return next((i["t"] for i in self.snapshot() if i.get("event") == "connected"), None)

    def check_alive(self):
        if self.encoder.poll() is not None:
            fail(f"ffmpeg exited with {self.encoder.returncode} (see the ffmpeg log in the artifact)")
        if self.sender.poll() is not None:
            fail(f"srt-sender exited with {self.sender.returncode} (see its log in the artifact)")

    def stop(self):
        for proc in (self.encoder, self.sender):
            if proc.poll() is None:
                proc.terminate()
        for proc in (self.encoder, self.sender):
            try:
                proc.wait(timeout=10)
            except subprocess.TimeoutExpired:
                proc.kill()
        self.ffmpeg_log.close()
        self.sender_log.close()


class Poller:
    """Looks at the production every POLL_EVERY seconds: its status and whether the
    mixer has a resolution for the input."""

    def __init__(self, ol, production_id):
        self.ol = ol
        self.production_id = production_id
        self.polls = []
        self.stop_event = threading.Event()
        self.thread = threading.Thread(target=self._loop, daemon=True)
        self.thread.start()

    def _loop(self):
        while not self.stop_event.is_set():
            try:
                p = self.ol.production(self.production_id) or {}
            except Failed:
                p = {}
            res = p.get("inputResolutions")
            self.polls.append((int(time.time() * 1000), p.get("status"), decoding(res), isinstance(res, list)))
            self.stop_event.wait(POLL_EVERY)

    def stop(self):
        self.stop_event.set()
        self.thread.join(timeout=30)
        return list(self.polls)


def wait_for(predicate, timeout, what, alive=lambda: None):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        alive()
        value = predicate()
        if value:
            return value
        time.sleep(0.5)
    fail(f"{what} within {timeout} s")


def measure(ol, cloud, run, name, seconds, lead, work):
    result = {"run": run, "order": RUNS[run]}
    source_id, port = ol.create_source(f"{name}-{run} — Test pattern")
    say(f"run {run}: source {source_id} listens on SRT port {port}")
    production_id = ol.create_production(f"{name}-{run} — production", source_id)
    sender = None
    poller = None
    try:
        if run == "A":
            asked = time.time() * 1000
            poller = Poller(ol, production_id)
            ol.activate(production_id)
            active = time.time() * 1000
            say(f"run A: production active after {(active - asked) / 1000:.1f} s; the caller dials now")
            sender = Sender(cloud, port, work, run)
            connected = wait_for(sender.first_connect, CONNECT_TIMEOUT, "the caller did not connect", sender.check_alive)
        else:
            sender = Sender(cloud, port, work, run)
            say(f"run B: the caller dials; the production is activated in {lead} s")
            time.sleep(lead)
            sender.check_alive()
            early = sender.first_connect()
            result["connected_before_activation"] = early is not None
            result["lead_s"] = (time.time() * 1000 - sender.started) / 1000
            asked = time.time() * 1000
            poller = Poller(ol, production_id)
            ol.activate(production_id)
            active = time.time() * 1000
            say(f"run B: production active after {(active - asked) / 1000:.1f} s")
            connected = wait_for(sender.first_connect, CONNECT_TIMEOUT, "the caller did not connect", sender.check_alive)
        result["activation_s"] = (active - asked) / 1000
        result["connect_after_active_s"] = (connected - active) / 1000

        def stats():
            return [i for i in sender.snapshot() if i.get("event") == "stats"]

        def readings():
            first, second = pick_readings(stats(), connected, seconds)
            return (first, second) if second else None

        say(f"run {run}: reading the sender stats 1 s after the connect and {seconds} s later")
        first, second = wait_for(readings, seconds + 30, "the second stats reading did not arrive", sender.check_alive)
        end = second["t"]
        result["reading_1"] = reading(first)
        result["reading_2"] = reading(second)
        result["interval"] = interval(result["reading_1"], result["reading_2"])
        events = sender.snapshot()
        result["connects"] = max((i.get("connects", 0) for i in events), default=0)
        result["broken"] = sum(1 for i in events if i.get("event") == "broken" and i["t"] <= end)
        polls = poller.stop()
        poller = None
        result["decode"] = decode_summary(polls, active, connected, end)
        result["polls"] = polls
        result["stalled"], result["reasons"], result["over_drop_limit"] = verdict(
            result["interval"], result["decode"], result["broken"]
        )
        for label in ("reading_1", "reading_2"):
            r = result[label]
            say(
                f"run {run} {label}: {r['packets_sent']} sent, {r['packets_sent_dropped']} dropped "
                f"({pct(r['drop_share'])}), RTT {r['rtt_ms']} ms, {r['send_rate_mbps']} Mbps"
            )
    finally:
        if poller is not None:
            result.setdefault("polls", poller.stop())
        if sender is not None:
            sender.stop()
            result["sender_lines"] = sender.snapshot()
        ol.remove_production(production_id)
        ol.remove_source(source_id)
    return result


def env_int(name, default, floor):
    raw = os.environ.get(name, "").strip()
    value = int(raw) if raw else default
    if value < floor:
        fail(f"{name} is {value}; it may not be under {floor}")
    return value


def version_of(cmd):
    try:
        out = subprocess.run(cmd, capture_output=True, text=True, timeout=30).stdout
        return out.split("\n", 1)[0].strip() or None
    except (OSError, subprocess.SubprocessError):
        return None


def main():
    pat = os.environ.get("OSC_ACCESS_TOKEN", "").strip()
    if not pat:
        print(
            "::error title=connect-order::the repository secret OSC_ACCESS_TOKEN (an Open Source Cloud "
            "personal access token for the workspace that owns the Open Live instance) is not set; "
            "nothing was measured",
            flush=True,
        )
        return 1
    mask(pat)
    instance = os.environ.get("OPEN_LIVE_INSTANCE", "").strip() or "openlivedev"
    runs = [r.strip().upper() for r in os.environ.get("CO_RUNS", "A,B").split(",") if r.strip()]
    work = os.environ.get("CO_OUT") or os.path.join(os.environ.get("RUNNER_TEMP", "/tmp"), "connect-order")
    os.makedirs(work, exist_ok=True)
    meta = {
        "instance": instance,
        "runner": os.environ.get("RUNNER_OS", sys.platform) + " " + os.environ.get("RUNNER_ARCH", ""),
        "sha": os.environ.get("GITHUB_SHA", "main"),
        "ffmpeg": (version_of([os.environ.get("CO_FFMPEG", "ffmpeg"), "-version"]) or "?").replace("ffmpeg version ", "").split(" ")[0],
        "libsrt": os.environ.get("CO_LIBSRT_VERSION", "?"),
    }
    if os.environ.get("GITHUB_RUN_ID"):
        meta["run_url"] = (
            f"{os.environ.get('GITHUB_SERVER_URL', 'https://github.com')}/"
            f"{os.environ.get('GITHUB_REPOSITORY', '')}/actions/runs/{os.environ['GITHUB_RUN_ID']}"
        )
    name = f"{PREFIX}{os.environ.get('GITHUB_RUN_ID', int(time.time()))}-{os.environ.get('GITHUB_RUN_ATTEMPT', '1')}"
    results = []
    code = 0
    ol = OpenLive(pat)
    try:
        bad = [r for r in runs if r not in RUNS]
        if bad or not runs:
            fail(f"CO_RUNS may name A and B only (got {','.join(runs) or 'nothing'})")
        seconds = env_int("CO_SECONDS", MIN_SECONDS, MIN_SECONDS)
        lead = env_int("CO_CALLER_LEAD", 30, MIN_LEAD)
        meta["seconds"] = seconds
        ol.resolve(instance)
        say(f"instance '{instance}' found")
        info = ol.server_info()
        say(f"server-info: srtPortState {info['srtPortState']}, {len(info['srtPorts'] or [])} SRT ports")
        left = ol.sweep()
        if left:
            say(f"removed {left} production(s)/source(s) left by an earlier run")
        for run in runs:
            try:
                results.append(measure(ol, info["stromHost"], run, name, seconds, lead, work))
            except Failed as err:
                say(f"run {run} failed: {err}")
                results.append({"run": run, "order": RUNS[run], "error": str(err)})
                code = 1
    except Failed as err:
        print(f"::error title=connect-order::{err}", flush=True)
        code = 1
    finally:
        problems = ol.cleanup() if ol.url else []
        meta["cleanup"] = (
            "the test productions and sources were deactivated and deleted"
            if not problems else f"could not remove {', '.join(problems)}"
        )
        if problems:
            print(f"::error title=connect-order::{meta['cleanup']}", flush=True)
            code = 1
    meta.setdefault("seconds", MIN_SECONDS)
    with open(os.path.join(work, "results.json"), "w", encoding="utf-8") as f:
        json.dump({"meta": meta, "results": results}, f, indent=2)
    if results:
        text = report(results, meta)
        with open(os.path.join(work, "report.md"), "w", encoding="utf-8") as f:
            f.write(text + "\n")
        with open(os.path.join(work, "comment.md"), "w", encoding="utf-8") as f:
            f.write(comment(results, meta) + "\n")
        with open(SUMMARY, "a", encoding="utf-8") as f:
            f.write(text + "\n")
        print(text, flush=True)
    return code


if __name__ == "__main__":
    sys.exit(main())
