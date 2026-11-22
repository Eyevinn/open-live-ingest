#!/usr/bin/env python3
"""Agent-run end to end of the desktop app against an Open Live instance (OL-59i, #100;
docs/specs/desktop-app.md section 14, #62 AC2).

Drives the app executable a release artifact carries (the DMG's
Contents/MacOS/open-live-ingest-app, or the open-live-ingest-app.exe the Windows
installer puts in %ProgramFiles%\\Open Live Ingest) with `--ipc-stdio`: the app core
with no window and no webview, fed the same messages the page posts (`app/ui/app.js`),
answering with the same replies and events (`app/src/stdio.rs`). The release
workflow's `e2e-macos` job runs it on the DMG it has just built and checked, and its
`e2e-windows` job (#145) on the installer `windows-app` has just built and smoke
tested, after a silent install:

    python3 -I .github/scripts/e2e-app.py "/Volumes/.../Open Live Ingest.app/Contents/MacOS/open-live-ingest-app"
    python -I .github/scripts/e2e-app.py "C:\\Program Files\\Open Live Ingest\\open-live-ingest-app.exe"

In order, each logged and written to the step summary:
  1. `state`, and `devices`: the app lists devices through the Strom the bundle
     carries (on Windows, the one the installer's Strom MSI put in place).
  2. Where to send, as the page does it with a pasted token: `osc_paste_token` with
     OSC_ACCESS_TOKEN, then `choose_instance` with the instance named
     OPEN_LIVE_INSTANCE (default openlivedev) from the workspace the reply lists.
     Then What to send: `set_inputs` with the test pattern ticked.
  3. `go_live`: the source `<gateway name> — Test pattern` appears in Open Live, and
     the session arms (#130): its row is Armed and Strom runs no flow of that name,
     because no production holding the source is live. Then the late receiver
     (#112): a production named `<gateway name> — production` is created with the
     source on `video_in_0`; the row waits for it and still nothing is sent. At least
     E2E_LATE_SECONDS (default 35, never under 30) after Go live it is activated, so
     the cloud Strom's listener comes up only now, as in #110; while it is
     `activating` Strom runs no flow of the source, and once Open Live says it is
     `active` the flow has to run within 5 s. The uplink's SRT stats are checked
     (below), then the production is deactivated: the flow goes within 15 s and the
     source is kept. The production stays, inactive, for step 5.
  4. `stop`: the page's `stopped` event, with no error and nothing left behind; the
     flow is gone from Strom, and the source is kept in Open Live with its id (#115),
     the only one of the run's gateway name. When nothing answers at
     Strom's address any more, the app must have said it stopped the Strom it started
     (a `strom` event with state `stopped`); a Strom that died by itself fails.
  5. `go_live` again: no input fails (an Open Live 409 would fail it) and the kept
     source is reused with the same id, so a production slot holding it stays valid.
     The row waits for the production, which still holds the source, and no flow
     runs; the production is activated again and the flow runs within 5 s of it
     being `active`, without restarting the app; the new connect gets the same
     uplink SRT stats check as the first one (#130 AC2).
  6. Quitting while live, as the close button does: `quit` asks (`quit_requested`),
     then `quit` with `confirmed: true` (the question's Stop and quit) stops and ends
     the process; the flow is gone from Strom and the source is kept, with the same id.

Before step 2 the settings file the app reads is written with a gateway name unique
to the run and the platform (the source prefix, so cleanup finds exactly this run's
sources, and the macOS and Windows jobs of one run never share one) and the SRT port
range the instance does not publish, as the Advanced section would. The app's own
settings file is used (HOME or APPDATA is not moved), because the pasted token goes
to the login keychain (macOS) or the Credential Manager (Windows) of the user the
runner runs as.

Before step 2, with or without OSC_ACCESS_TOKEN, a `go_live` has to be refused (no
Open Live address yet), and then the late-listener check runs with no secret (#112):
  - `self_hosted` points the app at a stand-in Open Live on loopback (LocalOpenLive:
    the four source routes, server-info publishing Strom host 127.0.0.1 and one free
    SRT port, and `GET /api/v1/productions` listing the productions the run puts
    there), and `set_inputs` ticks the test pattern.
  - `go_live`: the source appears there and the session arms (#130): the stand-in
    lists no production, so the row is Armed and Strom runs no flow of the source.
    The stand-in then lists a production holding the source on `video_in_0`,
    `inactive` (the row waits for it) and then `activating`; still no flow. Then it
    reports it `active`, and the flow has to run within 5 s; the uplink calls a port
    where nothing listens yet.
  - At least E2E_LATE_SECONDS after the flow started, a local SRT listener starts on
    that port (`srt-live-transmit`, from Homebrew's `srt`, or SRT_LIVE_TRANSMIT), and
    the uplink's SRT stats are checked (below): the late listener has to connect.
  - The stand-in reports the production `inactive` again: the flow goes within 15 s,
    the source is kept and the row waits for the production again.
  - `stop`: stopped with nothing left behind, the flow gone and the source kept with
    its id (#115).

E2E_PART picks the part of the run: `all` (the default, a run by hand), or, as the
release workflow splits it, `late-listener` (the `e2e-macos-late-listener` job, no
secret, fork pull requests too: the refused `go_live`, the late-listener check, then
a `quit` with nothing live ends the app) and `openlivedev` (the `e2e-macos` and
`e2e-windows` jobs: everything but the late-listener check).

The SRT stats check reads the uplink's `GET /api/flows/<flow id>/srt-stats` from the
bundled Strom, as `src/strom.rs` does: the sending element's (`role` sink) first peer.
The first sample is the first one that shows a connected peer; then
E2E_SRT_SAMPLES - 1 more (default 2 in all, never fewer), E2E_SRT_INTERVAL seconds
apart (default 60, never under 60). Every sample is logged. It fails when any sample
says `connected` false, or when, between consecutive samples or between the first
and the last, `packets_sent_dropped` grows by more than 1 % of the growth in
`packets_sent` (or nothing is sent, or the counters go back, which is a reconnect).
The failure names the numbers. `connected` alone proves nothing: srtsink can keep a
stale entry that reports it (src/strom.rs, `uplink_peer`), so packets_sent must grow
too. While E2E_UPLINK_DROP_WARN_ONLY is `true`, a dropped share above 1 % is a
`::warning::` with the numbers instead of a failure, until #111 (the sender drops
after a late connect) is fixed; `connected` false, nothing sent and a reconnect still
fail. #111's fix removes the switch.

Without OSC_ACCESS_TOKEN, a `quit` with nothing live then has to end the process. The
run fails naming the secret when E2E_REQUIRE_SECRET is `true` (a run by hand);
otherwise (a pull request run) it ends with a warning naming the secret and exits 0.

Whatever happens, the exit path closes the app (stdin closed: its bounded teardown)
and deletes any source and production still carrying the run's gateway name (the
source the app keeps included), and stops the local SRT listener. Sources and
productions named `oli-app-e2e-` left by an earlier run that was killed are removed
before step 3.

The check itself is unit tested, with no app and no network beyond loopback:
`python3 -I .github/scripts/test_e2e_app.py`.

Secrets and the bundled Strom (the reason is #83's, e2e-audio-device.sh): the app is
started with OSC_ACCESS_TOKEN and GH_TOKEN removed from its environment. The app
starts the bundled Strom with its own environment, and Strom would seed its own
Open Source Cloud access from OSC_ACCESS_TOKEN while its API is unauthenticated. The
app itself does not need the variable: the token reaches it over IPC
(`osc_paste_token`), as a pasted token does from the page. Strom also listens on all
interfaces; the workflow step loads a pf rule (macOS) or Windows Firewall rules
(Windows) that drop traffic to its port (8080) from anything but loopback before this
script runs, as #83 does with iptables. Nothing GitHub documents promises that a
hosted runner cannot be reached from outside, so the rule is loaded rather than
assumed unneeded; the step fails if the firewall does not take it.

Python standard library only. On Windows stdout and stderr are switched to UTF-8 (the
source names carry an em dash, device names anything), since a pipe there defaults to
the ANSI code page. The token and the service token are masked as soon as
they are known. Every `url` value in the app's lines (the instance addresses the
workspace lists, the address chosen), and its host without the scheme, is masked
before the line is printed; loopback addresses (Strom's) are not. No response body
that can carry a secret is printed. Replies and events cannot carry a secret (the
core never serialises one), so the app's lines are logged.
"""

import json
import os
import queue
import re
import shutil
import socket
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

TOKEN_EXCHANGE = "https://token.svc.prod.osaas.io/servicetoken"
SERVICE_ID = "eyevinn-open-live"
TEST_PATTERN = "test"
TEST_PATTERN_NAME = "Test pattern"
PREFIX = "oli-app-e2e-"
# `[strom] url` when the settings do not set it (src/config.rs).
STROM_URL = "http://127.0.0.1:8080"
# In the gateway name, so the macOS and Windows jobs of one run have sources of their own.
PLATFORM_TAG = {"darwin": "mac", "win32": "win"}.get(sys.platform, sys.platform)
# Never handed to the app, so never to the Strom it starts (#83).
WITHHELD_ENV = ("OSC_ACCESS_TOKEN", "GH_TOKEN")
# The uplink fails the run when it drops more than this share of what it sends (#112).
DROPPED_LIMIT = 0.01
# How late the receiver comes up after Go live, at least: #110 hit it after 30 s.
MIN_LATE_SECONDS = 30
# How far apart the SRT stats samples are, at least, and how many, at least.
MIN_SRT_INTERVAL = 60
MIN_SRT_SAMPLES = 2
# How long the uplink has, once its receiver is up, to show a connected peer.
CONNECT_TIMEOUT = 120
# The mixer input the openlivedev production puts the source on.
MIXER_INPUT = "video_in_0"
# What E2E_PART may ask for (`part`).
PARTS = ("all", "late-listener", "openlivedev")
# Once a production holding the source is `active`, its uplink flow runs within this
# many seconds (#130 AC1).
FLOW_START_TIMEOUT = 5
# Once none is live any more, the flow is gone within this many seconds (#130 AC2:
# the session looks at the productions every 2 s).
FLOW_GONE_TIMEOUT = 15
# How long an armed session is watched for a flow it must not start: three of its
# looks at the productions, 2 s apart.
ARMED_HOLD = 7
# How long a row has to show what it waits for: the app reads the productions for the
# rows every 4 s (app/src/status.rs).
ROW_TIMEOUT = 20
# The row states of an armed input (app/src/go_live.rs, RowState).
ARMED = ("armed", "waiting_for_production")

SUMMARY = os.environ.get("GITHUB_STEP_SUMMARY") or os.devnull
ON_ACTIONS = bool(os.environ.get("GITHUB_ACTIONS"))


def say(text):
    print(f"==> {text}", flush=True)


def record(text):
    with open(SUMMARY, "a", encoding="utf-8") as f:
        f.write(text + "\n")


MASKED = set()


def mask(value):
    if ON_ACTIONS and value and value not in MASKED:
        MASKED.add(value)
        print(f"::add-mask::{value}", flush=True)


LOOPBACK_HOSTS = ("127.0.0.1", "localhost", "[::1]")


# Keys whose string value is a URL. `uplink` and `registered` are the `summary` event's
# caller uplink to the cloud Strom and the address registered with Open Live.
URL_KEYS = ("url", "uplink", "registered")


def mask_urls(value):
    """Masks every non-loopback URL value (see URL_KEYS) in an app line, its host and
    its hostname (without scheme or query), so that the line can be printed."""
    if isinstance(value, dict):
        for key, item in value.items():
            if key in URL_KEYS and isinstance(item, str) and item:
                bare = item.split("://", 1)[-1].rstrip("/")
                host = bare.split("/", 1)[0].split("?", 1)[0]
                hostname = host.rsplit(":", 1)[0]
                if hostname and hostname not in LOOPBACK_HOSTS:
                    mask(item)
                    mask(bare)
                    mask(host)
                    mask(hostname)
            else:
                mask_urls(item)
    elif isinstance(value, list):
        for item in value:
            mask_urls(item)


def passed(text):
    say(f"PASS: {text}")
    record(f"- PASS: {text}")


def note(text):
    say(f"NOTE: {text}")
    record(f"- NOTE: {text}")


def warn(text):
    print(f"::warning title=e2e app::{text}", flush=True)
    record(f"- WARNING: {text}")


class Failed(Exception):
    pass


def fail(text):
    raise Failed(text)


# --- the app, over stdin and stdout ---------------------------------------------------


class App:
    """The app executable started with --ipc-stdio: page messages in, replies and
    events out, one JSON value per line."""

    def __init__(self, exe, stderr_log):
        # OSC_ACCESS_TOKEN and GH_TOKEN stay out of the app, and so out of the Strom it
        # starts: see the header (#83).
        env = {k: v for k, v in os.environ.items() if k not in WITHHELD_ENV}
        self.proc = subprocess.Popen(
            [exe, "--ipc-stdio"],
            env=env,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=stderr_log,
            text=True,
            encoding="utf-8",
            bufsize=1,
        )
        self.lines = queue.Queue()
        self.seen = []
        self.next_id = 1
        threading.Thread(target=self._read, daemon=True).start()

    def _read(self):
        for line in self.proc.stdout:
            line = line.strip()
            if not line:
                continue
            try:
                self.lines.put(json.loads(line))
            except ValueError:
                self.lines.put({"unreadable": line})
        self.lines.put(None)

    def _take(self, timeout):
        """The next line from the app; None once its stdout has closed."""
        try:
            message = self.lines.get(timeout=timeout)
        except queue.Empty:
            return "timeout"
        if message is None:
            # The end stays the end for whoever reads next.
            self.lines.put(None)
        else:
            self.seen.append(message)
            # Before anything prints it: the instance address is not for the log.
            mask_urls(message)
            print(f"    app: {json.dumps(message, ensure_ascii=False)}", flush=True)
        return message

    def send(self, cmd, args=None):
        message_id = self.next_id
        self.next_id += 1
        message = {"id": message_id, "cmd": cmd}
        if args is not None:
            message["args"] = args
        shown = dict(message)
        if cmd == "osc_paste_token":
            shown["args"] = {"token": "***"}
        print(f"    page: {json.dumps(shown)}", flush=True)
        try:
            self.proc.stdin.write(json.dumps(message) + "\n")
            self.proc.stdin.flush()
        except (BrokenPipeError, OSError):
            fail(f"the app was gone before `{cmd}` could be sent")
        return message_id

    def wait(self, wanted, what, timeout):
        """The first line from now on that `wanted` accepts."""
        deadline = time.monotonic() + timeout
        while True:
            left = deadline - time.monotonic()
            if left <= 0:
                fail(f"no {what} from the app within {timeout:.0f} s")
            message = self._take(left)
            if message == "timeout":
                fail(f"no {what} from the app within {timeout:.0f} s")
            if message is None:
                fail(f"the app ended (exit {self.proc.wait()}) before {what}")
            if wanted(message):
                return message

    def call(self, cmd, args=None, timeout=60):
        """Sends a command and returns its reply, `ok` or `error`."""
        message_id = self.send(cmd, args)
        return self.wait(lambda m: m.get("id") == message_id, f"reply to `{cmd}`", timeout)

    def ok(self, cmd, args=None, timeout=60):
        reply = self.call(cmd, args, timeout)
        if "error" in reply:
            fail(f"`{cmd}` was refused: {reply['error'].get('message')}")
        return reply.get("ok")

    def event(self, name, timeout, where=lambda data: True):
        return self.wait(
            lambda m: m.get("event") == name and where(m.get("data") or {}),
            f"`{name}` event",
            timeout,
        )["data"]

    def ended(self, timeout):
        """Waits for the process to end, reading what it still says. Its exit code."""
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if self._take(max(0.1, deadline - time.monotonic())) is None:
                break
        try:
            return self.proc.wait(timeout=max(0.1, deadline - time.monotonic()))
        except subprocess.TimeoutExpired:
            return None

    def close(self, timeout=30):
        """Closes stdin (the app's bounded teardown when live) and waits; kills it last."""
        if self.proc.poll() is not None:
            return
        try:
            self.proc.stdin.close()
        except OSError:
            pass
        if self.ended(timeout) is None:
            self.proc.kill()
            self.proc.wait()

    def drain(self):
        """Reads what the app has said so far, without waiting."""
        while self._take(0.01) not in ("timeout", None):
            pass

    def strom_stopped(self, since, timeout):
        """Whether the app has said, since line `since`, that it stopped the Strom it
        started (a `strom` event with state `stopped`), waiting up to `timeout` for it."""

        def said():
            return any(
                m.get("event") == "strom" and (m.get("data") or {}).get("state") == "stopped"
                for m in self.seen[since:]
            )

        self.drain()
        deadline = time.monotonic() + timeout
        while not said() and time.monotonic() < deadline:
            if self._take(max(0.1, deadline - time.monotonic())) in ("timeout", None):
                break
        return said()

    def failed_rows(self, since):
        self.drain()
        return [
            m["data"]
            for m in self.seen[since:]
            if m.get("event") == "input" and (m.get("data") or {}).get("state") == "failed"
        ]

    def row_state(self, input_name, since):
        """The state the last `input` event since line `since` gave the row of
        `input_name`, or None when there was none."""
        state = None
        for m in self.seen[since:]:
            data = m.get("data") or {}
            if m.get("event") == "input" and data.get("input") == input_name:
                state = data.get("state")
        return state

    def wait_row(self, input_name, states, since, step, timeout=ROW_TIMEOUT):
        """Waits until the row of `input_name`, as told since line `since`, is in one
        of `states`; returns that state."""
        self.drain()
        deadline = time.monotonic() + timeout
        while True:
            state = self.row_state(input_name, since)
            if state in states:
                return state
            left = deadline - time.monotonic()
            if left <= 0:
                fail(f"{step}: the row of '{input_name}' is '{state}', not {' or '.join(states)}, after {timeout} s")
            if self._take(left) is None:
                fail(f"{step}: the app ended (exit {self.proc.wait()}) with the row of '{input_name}' '{state}'")


# --- HTTP ------------------------------------------------------------------------------


def http(method, url, headers=None, body=None, timeout=30):
    """(status, parsed JSON or None). Never raises for an HTTP status."""
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
    try:
        return status, json.loads(raw) if raw else None
    except ValueError:
        return status, None


class SourceList:
    """What the run asks of an Open Live's sources; `sources` and `delete` are the
    subclass's."""

    def ours(self, gateway_name):
        prefix = f"{gateway_name} — "
        return [s for s in self.sources() if str(s.get("name", "")).startswith(prefix)]

    def wait_for_source(self, name, timeout):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            found = [s for s in self.sources() if s.get("name") == name]
            if found:
                return found[0]
            time.sleep(2)
        fail(f"no source named '{name}' appeared in Open Live within {timeout} s")


class OpenLive(SourceList):
    def __init__(self, url, pat):
        self.url = url.rstrip("/")
        self.pat = pat
        self.token = None
        self.expires = 0.0
        self.productions = []

    def _bearer(self):
        if self.token is None or time.time() > self.expires - 60:
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
            self.expires = float(body.get("expiry") or time.time() + 600)
        return {"Authorization": f"Bearer {self.token}"}

    def sources(self):
        status, body = http("GET", f"{self.url}/api/v1/sources", self._bearer())
        if status != 200 or not isinstance(body, list):
            fail(f"GET /api/v1/sources failed with HTTP {status}")
        return body

    def delete(self, source_id):
        status, _ = http("DELETE", f"{self.url}/api/v1/sources/{source_id}", self._bearer())
        return status in (200, 202, 204, 404)

    # Productions: only what the late-receiver check needs (#112).

    def all_productions(self):
        status, body = http("GET", f"{self.url}/api/v1/productions", self._bearer())
        if status != 200 or not isinstance(body, list):
            fail(f"GET /api/v1/productions failed with HTTP {status}")
        return body

    def production(self, production_id):
        status, body = http("GET", f"{self.url}/api/v1/productions/{production_id}", self._bearer())
        if status != 200 or not isinstance(body, dict):
            fail(f"GET /api/v1/productions/{production_id} failed with HTTP {status}")
        return body

    def create_production(self, name, source_id):
        """A production with the source on MIXER_INPUT, inactive; its id. Remembered,
        so the run's exit path removes it whatever happens."""
        status, body = http("POST", f"{self.url}/api/v1/productions", self._bearer(), {"name": name})
        if status not in (200, 201) or not isinstance(body, dict) or not body.get("_id"):
            fail(f"creating production '{name}' failed with HTTP {status}")
        production_id = body["_id"]
        self.productions.append(production_id)
        status, _ = http(
            "POST",
            f"{self.url}/api/v1/productions/{production_id}/sources",
            self._bearer(),
            {"sourceId": source_id, "mixerInput": MIXER_INPUT},
        )
        if status not in (200, 201):
            fail(f"assigning source {source_id} to production {production_id} failed with HTTP {status}")
        return production_id

    def activate(self, production_id, timeout, while_activating=lambda: None):
        """Activates the production and waits until Open Live says it is active;
        `while_activating` runs at every look that still finds it activating."""
        status, _ = http("POST", f"{self.url}/api/v1/productions/{production_id}/activate", self._bearer(), {})
        if status != 200:
            fail(f"activating production {production_id} failed with HTTP {status}")
        deadline = time.monotonic() + timeout
        state = "activating"
        while time.monotonic() < deadline:
            state = self.production(production_id).get("status")
            if state == "active":
                return
            if state != "activating":
                fail(f"production {production_id} did not activate: its status went to '{state}'")
            while_activating()
            time.sleep(1)
        fail(f"production {production_id} was still '{state}' after {timeout} s")

    def deactivate(self, production_id, timeout):
        """Deactivates the production and waits until Open Live says it is inactive."""
        status, _ = http("POST", f"{self.url}/api/v1/productions/{production_id}/deactivate", self._bearer(), {}, timeout=180)
        if status not in (200, 202, 204):
            fail(f"deactivating production {production_id} failed with HTTP {status}")
        deadline = time.monotonic() + timeout
        state = None
        while time.monotonic() < deadline:
            state = self.production(production_id).get("status")
            if state == "inactive":
                return
            time.sleep(2)
        fail(f"production {production_id} was still '{state}' {timeout} s after its deactivation")

    def remove_production(self, production_id):
        """Deactivates and deletes; True when it is gone. Deactivating an inactive
        production is harmless, and Open Live joins a deactivation already running."""
        url = f"{self.url}/api/v1/productions/{production_id}"
        http("POST", f"{url}/deactivate", self._bearer(), {}, timeout=180)
        status, _ = http("DELETE", url, self._bearer())
        gone = status in (200, 202, 204, 404)
        if gone and production_id in self.productions:
            self.productions.remove(production_id)
        return gone


class LocalOpenLive(SourceList):
    """A stand-in for a self-hosted Open Live on loopback, for the no-secret late
    listener check (#112): `GET /api/v1/server-info` publishes Strom host 127.0.0.1
    and the one SRT port `srt_port`, and `/api/v1/sources` lists, creates, patches and
    deletes sources the way the gateway uses them. A source registered with port 0
    (`srt://:0?mode=listener`, "assign me one") is given `srt_port`.
    `GET /api/v1/productions` lists the productions the run puts there with
    `put_production`, in the shape Open Live returns them (`_id`, `name`, `status`,
    `sources` of `sourceId` and `mixerInput`), so the run decides when a production
    holding the source goes live (#130). No key."""

    def __init__(self, srt_port):
        self.srt_port = srt_port
        self.lock = threading.Lock()
        self.stored = {}
        self.next_id = 1
        self.stored_productions = {}
        owner = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *args):
                pass

            def _reply(self, status, body=None):
                raw = b"" if body is None else json.dumps(body).encode()
                self.send_response(status)
                self.send_header("content-type", "application/json")
                self.send_header("content-length", str(len(raw)))
                self.end_headers()
                self.wfile.write(raw)

            def _body(self):
                length = int(self.headers.get("content-length") or 0)
                try:
                    return json.loads(self.rfile.read(length) or b"null")
                except ValueError:
                    return None

            def do_GET(self):
                if self.path == "/api/v1/server-info":
                    self._reply(200, owner.server_info())
                elif self.path == "/api/v1/sources":
                    self._reply(200, owner.sources())
                elif self.path == "/api/v1/productions":
                    self._reply(200, owner.productions())
                else:
                    self._reply(404, {"error": "not found"})

            def do_POST(self):
                body = self._body()
                if self.path != "/api/v1/sources" or not isinstance(body, dict):
                    self._reply(404 if self.path != "/api/v1/sources" else 400, {"error": "no"})
                    return
                self._reply(201, owner.create(body))

            def do_PATCH(self):
                body = self._body()
                source_id = self.path.rsplit("/", 1)[-1]
                if not self.path.startswith("/api/v1/sources/") or not isinstance(body, dict):
                    self._reply(400, {"error": "no"})
                    return
                patched = owner.patch(source_id, body)
                self._reply(200 if patched else 404, patched or {"error": "not found"})

            def do_DELETE(self):
                if not self.path.startswith("/api/v1/sources/"):
                    self._reply(404, {"error": "not found"})
                    return
                self._reply(204 if owner.delete(self.path.rsplit("/", 1)[-1]) else 404)

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.url = f"http://127.0.0.1:{self.server.server_address[1]}"
        threading.Thread(target=self.server.serve_forever, daemon=True).start()

    def server_info(self):
        return {"stromHost": "127.0.0.1", "srtPorts": [self.srt_port], "srtPortState": "reserved"}

    def _with_port(self, source):
        source["address"] = re.sub(r"^srt://:0(?=\?|$)", f"srt://:{self.srt_port}", str(source.get("address", "")))
        return source

    def sources(self):
        with self.lock:
            return [dict(s) for s in self.stored.values()]

    def create(self, body):
        with self.lock:
            source = self._with_port(dict(body, id=f"src-{self.next_id}"))
            self.next_id += 1
            self.stored[source["id"]] = source
            return dict(source)

    def patch(self, source_id, body):
        with self.lock:
            if source_id not in self.stored:
                return None
            source = self._with_port(dict(self.stored[source_id], **body, id=source_id))
            self.stored[source_id] = source
            return dict(source)

    def delete(self, source_id):
        with self.lock:
            return self.stored.pop(source_id, None) is not None

    def productions(self):
        with self.lock:
            return [json.loads(json.dumps(p)) for p in self.stored_productions.values()]

    def put_production(self, production_id, name, source_id, status):
        """Lists a production holding `source_id` on MIXER_INPUT, in `status`
        (`inactive`, `activating`, `active`), replacing one of the same id."""
        with self.lock:
            self.stored_productions[production_id] = {
                "_id": production_id,
                "name": name,
                "status": status,
                "sources": [{"sourceId": source_id, "mixerInput": MIXER_INPUT}],
            }

    def close(self):
        self.server.shutdown()
        self.server.server_close()


def free_udp_port():
    """A UDP port nothing on loopback uses right now: the stand-in Open Live publishes
    it, and the late listener binds it."""
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


class LateListener:
    """A local SRT listener (srt-live-transmit, from Homebrew's `srt`), started late on
    purpose: the receiver the uplink calls comes up only after Go live (#112)."""

    def __init__(self, port, log_path):
        self.exe = os.environ.get("SRT_LIVE_TRANSMIT") or shutil.which("srt-live-transmit")
        if not self.exe:
            fail("missing tool: srt-live-transmit is not on PATH (brew install srt), and SRT_LIVE_TRANSMIT is not set")
        self.port = port
        self.log_path = log_path
        self.proc = None

    def start(self):
        self.log = open(self.log_path, "w", encoding="utf-8")
        self.proc = subprocess.Popen(
            [self.exe, f"srt://127.0.0.1:{self.port}?mode=listener", "file://con"],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=self.log,
        )
        time.sleep(1)
        self.check_running("just after it started")

    def check_running(self, when):
        if self.proc is not None and self.proc.poll() is not None:
            self.log.flush()
            with open(self.log_path, encoding="utf-8", errors="replace") as f:
                tail = f.read()[-2000:]
            fail(f"the local SRT listener on 127.0.0.1:{self.port} exited ({self.proc.returncode}) {when}: {tail.strip()}")

    def stop(self):
        if self.proc is not None and self.proc.poll() is None:
            self.proc.terminate()
            try:
                self.proc.wait(timeout=10)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                self.proc.wait()
        if self.proc is not None:
            self.log.close()


# --- the uplink's SRT stats (#112) -----------------------------------------------------


def uplink_sample(body):
    """From a `GET /api/flows/<id>/srt-stats` body, the uplink's state: the sending
    element's (`role` sink) `connected`, and its first peer's `packets_sent` and
    `packets_sent_dropped`, as `uplink_peer` in src/strom.rs picks it. None when the
    flow reports no peer yet."""
    # `connected` may be stale (src/strom.rs, uplink_peer): srtsink keeps reporting it
    # while retrying a vanished peer, so never rely on it alone; judge_uplink also needs
    # packets_sent to grow.
    connections = ((body or {}).get("stats") or {}).get("connections") or {}
    for connection in connections.values():
        if not isinstance(connection, dict) or connection.get("role") != "sink":
            continue
        callers = connection.get("callers") or []
        if not callers:
            return None
        peer = callers[0]
        return {
            "connected": connection.get("connected") is True,
            "packets_sent": peer.get("packets_sent"),
            "packets_sent_dropped": peer.get("packets_sent_dropped") or 0,
            "rtt_ms": peer.get("rtt_ms"),
        }
    return None


def describe(sample):
    return (
        f"+{sample['at']:.0f} s: connected={str(sample['connected']).lower()}, "
        f"packets_sent={sample['packets_sent']}, packets_sent_dropped={sample['packets_sent_dropped']}, "
        f"rtt_ms={sample.get('rtt_ms')}"
    )


def uplink_problems(samples, limit=DROPPED_LIMIT):
    """(fatal, dropped) for `samples` (in order, each with `at`, seconds since the
    first), each None or what went wrong, with the numbers. `fatal`: too few samples,
    a sample says `connected` false or has no packets_sent, or, between consecutive
    samples or between the first and the last, nothing is sent or a counter goes back
    (the link reconnected). `dropped`: the first such span in which
    `packets_sent_dropped` grows by more than `limit` of the growth in `packets_sent`.
    Kept apart so that E2E_UPLINK_DROP_WARN_ONLY can turn only `dropped` into a
    warning (#111)."""
    if len(samples) < MIN_SRT_SAMPLES:
        return f"only {len(samples)} SRT stats sample(s); at least {MIN_SRT_SAMPLES} are needed", None
    for sample in samples:
        if not sample["connected"]:
            return f"the uplink reports connected=false at {describe(sample)}", None
        if sample["packets_sent"] is None:
            return f"the uplink reports no packets_sent at {describe(sample)}", None
    pairs = list(zip(samples, samples[1:]))
    if len(samples) > 2:
        pairs.append((samples[0], samples[-1]))
    too_many = None
    for a, b in pairs:
        sent = b["packets_sent"] - a["packets_sent"]
        dropped = b["packets_sent_dropped"] - a["packets_sent_dropped"]
        span = f"between +{a['at']:.0f} s and +{b['at']:.0f} s ({b['at'] - a['at']:.0f} s)"
        if sent < 0 or dropped < 0:
            return (
                f"the uplink's counters went back {span}: packets_sent {a['packets_sent']} -> {b['packets_sent']}, "
                f"packets_sent_dropped {a['packets_sent_dropped']} -> {b['packets_sent_dropped']}; the link reconnected"
            ), too_many
        if sent == 0:
            return f"the uplink sent no packets {span} (packets_sent stayed at {b['packets_sent']})", too_many
        if dropped > sent * limit and too_many is None:
            too_many = (
                f"the uplink dropped too much {span}: packets_sent grew by {sent} and packets_sent_dropped by "
                f"{dropped}, {100 * dropped / sent:.2f} % dropped, more than {100 * limit:g} %"
            )
    return None, too_many


def judge_uplink(samples, limit=DROPPED_LIMIT):
    """None when the uplink held up over `samples`; otherwise what went wrong, with the
    numbers (`uplink_problems`, a fatal problem first)."""
    fatal, dropped = uplink_problems(samples, limit)
    return fatal or dropped


def env_int(name, default, floor):
    """The whole number in env `name` (`default` when unset or empty), never below
    `floor`; fails naming the variable when it is not a whole number."""
    raw = os.environ.get(name) or str(default)
    try:
        value = int(raw)
    except ValueError:
        fail(f"{name} must be a whole number, not {raw!r}")
    return max(floor, value)


def srt_settings():
    """(samples, interval in seconds, late start in seconds) from the environment,
    never below the issue's floor."""
    samples = env_int("E2E_SRT_SAMPLES", MIN_SRT_SAMPLES, MIN_SRT_SAMPLES)
    interval = env_int("E2E_SRT_INTERVAL", MIN_SRT_INTERVAL, MIN_SRT_INTERVAL)
    late = env_int("E2E_LATE_SECONDS", 35, MIN_LATE_SECONDS)
    return samples, interval, late


def drops_warn_only():
    """True when E2E_UPLINK_DROP_WARN_ONLY is `true`: a dropped share above the limit
    is then a warning, not a failure, until #111 (the sender drops after a late
    connect) is fixed. Everything else the SRT stats check finds stays fatal. #111's
    fix removes this switch."""
    raw = os.environ.get("E2E_UPLINK_DROP_WARN_ONLY") or "false"
    if raw not in ("true", "false"):
        fail(f"E2E_UPLINK_DROP_WARN_ONLY must be `true` or `false`, not {raw!r}")
    return raw == "true"


def part():
    """Which part of the run E2E_PART asks for: `all` (the default, a run by hand),
    `late-listener` (only the no-secret late-listener check, the `e2e-macos-late-listener`
    job, which runs on fork pull requests too) or `openlivedev` (everything but it, the
    `e2e-macos` job)."""
    raw = os.environ.get("E2E_PART") or "all"
    if raw not in PARTS:
        fail(f"E2E_PART must be one of {', '.join(PARTS)}, not {raw!r}")
    return raw


def wait_until(moment):
    left = moment - time.monotonic()
    if left > 0:
        time.sleep(left)


class Strom:
    def __init__(self, url):
        self.url = url.rstrip("/")

    def flows(self):
        """The flows Strom runs, or None when nothing answers at its address."""
        try:
            status, body = http("GET", f"{self.url}/api/flows", timeout=5)
        except (urllib.error.URLError, OSError):
            return None
        if status != 200 or not isinstance(body, dict):
            return None
        return body.get("flows") or []

    def flow_named(self, name):
        """The flow named `name`, None when there is none, or "unreachable" when
        nothing answers at Strom's address."""
        flows = self.flows()
        if flows is None:
            return "unreachable"
        return next((f for f in flows if f.get("name") == name), None)

    def wait_for_flow(self, name, timeout, step=None, every=2):
        deadline = time.monotonic() + timeout
        while True:
            flow = self.flow_named(name)
            if isinstance(flow, dict):
                return flow
            if time.monotonic() >= deadline:
                break
            time.sleep(min(every, max(0.0, deadline - time.monotonic())))
        fail(f"{step + ': ' if step else ''}Strom at {self.url} runs no flow named '{name}' after {timeout} s")

    def no_flow(self, name, seconds, step):
        """For `seconds`, Strom runs no flow named `name` (an armed session sends
        nothing, #130). Fails when one shows up, or when nothing answers at Strom's
        address the whole time, which would prove nothing."""
        deadline = time.monotonic() + seconds
        answered = False
        while True:
            flow = self.flow_named(name)
            if isinstance(flow, dict):
                fail(f"{step}: Strom runs flow {flow.get('id')} for '{name}' while no production holding the source is live")
            answered |= flow is None
            if time.monotonic() >= deadline:
                break
            time.sleep(min(0.5, max(0.0, deadline - time.monotonic())))
        if not answered:
            fail(f"{step}: nothing answered at {self.url} for {seconds} s, so the absence of a flow proves nothing")

    def wait_flow_gone(self, name, timeout, step):
        """Waits until Strom, still answering, runs no flow named `name`."""
        deadline = time.monotonic() + timeout
        while True:
            if self.flow_named(name) is None:
                return
            if time.monotonic() >= deadline:
                break
            time.sleep(min(0.5, max(0.0, deadline - time.monotonic())))
        fail(f"{step}: Strom at {self.url} still runs a flow named '{name}' {timeout} s after no production holding the source is live")

    def uplink(self, flow_id):
        """The uplink's state (`uplink_sample`), or None when the flow reports no peer
        yet. Fails when Strom does not answer."""
        try:
            status, body = http("GET", f"{self.url}/api/flows/{flow_id}/srt-stats", timeout=10)
        except (urllib.error.URLError, OSError) as err:
            fail(f"GET srt-stats of flow {flow_id} from Strom at {self.url} failed: {err}")
        if status == 404:
            return None
        if status != 200:
            fail(f"GET srt-stats of flow {flow_id} failed with HTTP {status}")
        return uplink_sample(body)

    def watch_uplink(self, flow_id, step, still_up=lambda when: None):
        """The SRT stats check (#112): waits up to CONNECT_TIMEOUT for a connected peer,
        then takes the rest of the samples E2E_SRT_INTERVAL apart, logs each and judges
        them. `still_up` checks the receiver is still there before each sample."""
        samples, interval, _ = srt_settings()
        deadline = time.monotonic() + CONNECT_TIMEOUT
        first = None
        while time.monotonic() < deadline:
            first = self.uplink(flow_id)
            if first is not None and first["connected"]:
                break
            time.sleep(2)
        if first is None or not first["connected"]:
            fail(f"{step}: the uplink of flow {flow_id} shows no connected peer {CONNECT_TIMEOUT} s after its receiver came up (last: {first})")
        start = time.monotonic()
        first["at"] = 0.0
        taken = [first]
        say(f"{step}: uplink SRT stats {describe(first)}")
        for n in range(1, samples):
            wait_until(start + n * interval)
            still_up(f"before sample {n + 1}")
            sample = self.uplink(flow_id)
            if sample is None:
                fail(f"{step}: the uplink of flow {flow_id} reports no peer any more at +{time.monotonic() - start:.0f} s")
            sample["at"] = time.monotonic() - start
            taken.append(sample)
            say(f"{step}: uplink SRT stats {describe(sample)}")
        fatal, dropped = uplink_problems(taken)
        if fatal:
            fail(f"{step}: {fatal}")
        a, b = taken[0], taken[-1]
        if dropped:
            if not drops_warn_only():
                fail(f"{step}: {dropped}")
            warn(
                f"{step}: {dropped}. Not failed: E2E_UPLINK_DROP_WARN_ONLY is set until #111 (sender drops after a "
                f"late connect) is fixed. The uplink stayed connected and kept sending ({len(taken)} samples over {b['at']:.0f} s)"
            )
            return
        passed(
            f"{step}: the uplink stayed connected and dropped {b['packets_sent_dropped'] - a['packets_sent_dropped']} "
            f"of {b['packets_sent'] - a['packets_sent']} packets sent over {b['at']:.0f} s "
            f"({len(taken)} samples; at most {100 * DROPPED_LIMIT:g} % allowed)"
        )

    def check_gone(self, flow_id, step, app, since):
        """The flow is gone from Strom. When nothing answers at Strom's address, the app
        must have said since line `since` that it stopped the Strom it started: a Strom
        that died by itself takes its flows with it too, and must not pass."""
        flows = self.flows()
        if flows is None:
            if not app.strom_stopped(since, 15):
                fail(f"{step}: nothing answers at {self.url}, and the app did not say it stopped the Strom it started")
            passed(f"{step}: the flow is gone with the Strom the app started, which it says it stopped (nothing answers at {self.url})")
            return
        if any(f.get("id") == flow_id for f in flows):
            fail(f"{step}: Strom still has flow {flow_id}")
        passed(f"{step}: Strom at {self.url} no longer has flow {flow_id}")


# --- settings --------------------------------------------------------------------------


def settings_file():
    if sys.platform == "darwin":
        base = os.path.join(os.environ["HOME"], "Library", "Application Support")
    elif sys.platform == "win32":
        base = os.environ["APPDATA"]
    else:
        base = os.environ.get("XDG_CONFIG_HOME") or os.path.join(os.environ["HOME"], ".config")
    return os.path.join(base, "open-live-ingest", "gateway.toml")


def write_settings(path, gateway_name, port_range):
    """What the Advanced section would hold: the source prefix and the SRT port range
    the instance does not publish. Where to send and What to send fill in the rest."""
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w", encoding="utf-8") as f:
        f.write(
            f'[gateway]\nname = "{gateway_name}"\n\n'
            f'[uplink]\nport_range = "{port_range}"\n\n'
            "[video]\nbitrate_kbps = 1000\n"
        )
    os.chmod(path, 0o600)


# --- the run ---------------------------------------------------------------------------


def list_devices(app):
    state = app.ok("state")
    passed(f"the app core answers `state` (version {state.get('version')}, settings {state.get('settings_file')})")
    what = app.ok("devices", {"show_all": False}, timeout=180)
    rows = what.get("video") or []
    if not any(r.get("test_pattern") for r in rows):
        fail("the What to send rows have no test pattern row")
    cameras = [r.get("name") for r in rows if not r.get("test_pattern")]
    passed(f"`devices` lists the test pattern and {len(cameras)} camera(s) through the bundled Strom")


def tick_the_test_pattern(app):
    what = app.ok("set_inputs", {"inputs": [{"device": TEST_PATTERN, "audio": None, "label": TEST_PATTERN_NAME}]})
    ticked = [r.get("name") for r in what.get("video") or [] if r.get("ticked")]
    passed(f"What to send: `set_inputs` saved the test pattern (ticked: {ticked}, prefix '{what.get('name_prefix')}')")


def go_live(app, open_live, strom, source_name, step, waits_for=ARMED):
    """Go live; returns the source and the app line the session began at. The session
    arms (#130): the row comes to one of `waits_for` and Strom runs no flow of the
    source over ARMED_HOLD, since no production holding it is live."""
    since = len(app.seen)
    reply = app.call("go_live", timeout=120)
    if "error" in reply:
        fail(f"{step}: `go_live` was refused: {reply['error'].get('message')}")
    source = open_live.wait_for_source(source_name, 120)
    if app.failed_rows(since):
        fail(f"{step}: an input failed: {app.failed_rows(since)}")
    passed(f"{step}: source '{source_name}' ({source.get('id')}) is in Open Live (streamType {source.get('streamType')}, status {source.get('status')})")
    state = app.wait_row(source_name, waits_for, since, step)
    strom.no_flow(source_name, ARMED_HOLD, step)
    passed(f"{step}: the session is armed: the row is '{state}' and Strom runs no flow for it over {ARMED_HOLD} s")
    return source, since


def sends_once_live(strom, source_name, step, live_at):
    """Once a production holding the source is `active` (at `live_at`), Strom runs
    the source's flow within FLOW_START_TIMEOUT (#130 AC1). The flow, and when it was
    seen."""
    flow = strom.wait_for_flow(source_name, FLOW_START_TIMEOUT, step, every=0.25)
    seen_at = time.monotonic()
    passed(f"{step}: Strom runs flow {flow.get('id')} {seen_at - live_at:.1f} s after the production is active (at most {FLOW_START_TIMEOUT} s)")
    return flow, seen_at


def stops_once_not_live(strom, open_live, gateway_name, source, source_name, step):
    """Once no production holding the source is live, the flow goes within
    FLOW_GONE_TIMEOUT and the source is kept (#130 AC2)."""
    gone_from = time.monotonic()
    strom.wait_flow_gone(source_name, FLOW_GONE_TIMEOUT, step)
    kept = [s.get("id") for s in open_live.ours(gateway_name)]
    if kept != [source.get("id")]:
        fail(f"{step}: expected source {source.get('id')} kept as the only one of '{gateway_name}', found {kept}")
    passed(f"{step}: the flow is gone from Strom {time.monotonic() - gone_from:.1f} s later and source {source.get('id')} is kept")


def stopped_cleanly(data, step):
    if data.get("error"):
        fail(f"{step}: the session stopped with an error: {data['error']}")
    if data.get("left_behind"):
        fail(f"{step}: the app says it left inputs behind: {data['left_behind']}")


def late_listener_check(app, strom, gateway_name, source_name, work):
    """The no-secret late-listener check (#112, #130): Go live to a stand-in Open Live
    on loopback, armed until the stand-in reports a production holding the source
    `active`; a local SRT listener starts at least E2E_LATE_SECONDS after the flow
    did; the uplink's SRT stats are checked; the production goes `inactive` again and
    the flow goes; Stop. See the header."""
    step = "Late listener (no secret)"
    _, _, late = srt_settings()
    srt_port = free_udp_port()
    listener = LateListener(srt_port, os.path.join(work, "srt-listener.log"))
    local = LocalOpenLive(srt_port)
    try:
        where = app.ok("self_hosted", {"url": local.url, "api_key": ""}, timeout=60)
        if not where.get("where", {}).get("ready"):
            fail(f"{step}: Where to send is not done after `self_hosted`: {where.get('where', {}).get('problem')}")
        passed(f"{step}: Where to send is the stand-in Open Live at {local.url} (Strom host 127.0.0.1, SRT port {srt_port})")
        tick_the_test_pattern(app)

        # Go live: no production holds the source, so the row is Armed and nothing runs.
        source, since = go_live(app, local, strom, source_name, step, waits_for=("armed",))

        # A production holds it, inactive, then activating: the row waits, nothing runs.
        production_id = "prod-e2e-1"
        production = f"{gateway_name} — production"
        local.put_production(production_id, production, source.get("id"), "inactive")
        app.wait_row(source_name, ("waiting_for_production",), since, step)
        strom.no_flow(source_name, ARMED_HOLD, f"{step}: production inactive")
        passed(f"{step}: production '{production}' holds the source on {MIXER_INPUT}, inactive: the row waits for it and Strom runs no flow")
        local.put_production(production_id, production, source.get("id"), "activating")
        strom.no_flow(source_name, ARMED_HOLD, f"{step}: production activating")
        passed(f"{step}: production activating: Strom still runs no flow for the source over {ARMED_HOLD} s")

        # Active: the flow runs within 5 s, calling a port where nothing listens yet.
        local.put_production(production_id, production, source.get("id"), "active")
        flow, started_at = sends_once_live(strom, source_name, f"{step}: production active", time.monotonic())
        app.wait_row(source_name, ("connecting", "live"), since, f"{step}: production active")
        say(f"{step}: nothing listens on 127.0.0.1:{srt_port} yet; the listener starts {late} s after the flow did")
        wait_until(started_at + late)
        listener.start()
        passed(f"{step}: the local SRT listener is up on 127.0.0.1:{srt_port}, {time.monotonic() - started_at:.0f} s after the flow started")
        strom.watch_uplink(flow.get("id"), step, listener.check_running)
        listener.check_running("after the last sample")

        # Inactive again: the flow goes, the source stays, the row waits again.
        again_from = len(app.seen)
        local.put_production(production_id, production, source.get("id"), "inactive")
        stops_once_not_live(strom, local, gateway_name, source, source_name, f"{step}: production inactive again")
        app.wait_row(source_name, ("waiting_for_production",), again_from, f"{step}: production inactive again")

        # Stop: the source stays with its id (#115).
        app.ok("stop")
        stopped_cleanly(app.event("stopped", 120), f"{step}: Stop")
        kept = local.ours(gateway_name)
        if [s.get("id") for s in kept] != [source.get("id")]:
            fail(f"{step}: Stop: expected source {source.get('id')} kept as the only one of '{gateway_name}' in the stand-in Open Live, found {[s.get('id') for s in kept]}")
        passed(f"{step}: Stop reports stopped, nothing left behind, and source {source.get('id')} is kept (status {kept[0].get('status')})")
        strom.check_gone(flow.get("id"), f"{step}: Stop", app, since)
    finally:
        listener.stop()
        local.close()


def late_receiver_check(app, open_live, strom, source, source_name, live_at, gateway_name):
    """The openlivedev late-receiver check (#112, #130): the production that takes the
    source is activated at least E2E_LATE_SECONDS after Go live, so the cloud Strom's
    listener comes up late; the session sends nothing while it activates and starts
    the flow once it is active; the uplink's SRT stats are checked; the production is
    deactivated, the flow goes and the source is kept. Returns the production id (it
    stays, inactive, for Go live again) and the flow."""
    step = "Late receiver (openlivedev)"
    _, _, late = srt_settings()
    since = len(app.seen)
    production_id = open_live.create_production(f"{gateway_name} — production", source.get("id"))
    app.wait_row(source_name, ("waiting_for_production",), since, step)
    strom.no_flow(source_name, ARMED_HOLD, f"{step}: production inactive")
    passed(f"{step}: production {production_id} has the source on {MIXER_INPUT}, inactive: the row waits for it and Strom runs no flow; it activates {late} s after Go live")
    wait_until(live_at + late)
    activated_at = time.monotonic()
    open_live.activate(production_id, 300, lambda: strom.no_flow(source_name, 0, f"{step}: production activating"))
    active_at = time.monotonic()
    passed(f"{step}: production {production_id} is active, {active_at - live_at:.0f} s after Go live (activation took {active_at - activated_at:.0f} s, no flow while it activated)")
    flow, _ = sends_once_live(strom, source_name, step, active_at)
    app.wait_row(source_name, ("connecting", "live"), since, step)
    strom.watch_uplink(flow.get("id"), step)
    open_live.deactivate(production_id, 180)
    stops_once_not_live(strom, open_live, gateway_name, source, source_name, f"{step}: production deactivated")
    return production_id, flow


def reactivated_check(app, open_live, strom, production_id, source_name, since):
    """#130 AC2: reactivating the production that still holds the kept source, with
    the app still running, gives a new clean connect. No flow while it activates, the
    flow within FLOW_START_TIMEOUT once it is active, the row connecting or live, and
    the same uplink SRT stats check as the first connect (`watch_uplink`). Returns
    the flow."""
    step = "Go live again: production reactivated"
    open_live.activate(production_id, 300, lambda: strom.no_flow(source_name, 0, f"{step} (activating)"))
    flow, _ = sends_once_live(strom, source_name, step, time.monotonic())
    app.wait_row(source_name, ("connecting", "live"), since, step)
    strom.watch_uplink(flow.get("id"), step)
    return flow


def run(exe, work):
    instance = os.environ.get("OPEN_LIVE_INSTANCE") or "openlivedev"
    run_id = os.environ.get("GITHUB_RUN_ID", "local")
    attempt = os.environ.get("GITHUB_RUN_ATTEMPT", str(os.getpid()))
    gateway_name = f"{PREFIX}{run_id}-{attempt}-{PLATFORM_TAG}"
    source_name = f"{gateway_name} — {TEST_PATTERN_NAME}"
    port_range = os.environ.get("E2E_PORT_RANGE") or "47300-47309"
    pat = os.environ.get("OSC_ACCESS_TOKEN") or ""
    mask(pat)
    # Bad settings fail here, before anything starts.
    which = part()
    srt_settings()
    drops_warn_only()

    settings = settings_file()
    if os.path.exists(settings):
        fail(f"{settings} exists already; run this on a machine where the app has not been set up")
    write_settings(settings, gateway_name, port_range)
    against = "a stand-in Open Live on loopback (late-listener part)" if which == "late-listener" else f"`{instance}`"
    record(f"### App end to end against {against}: {sys.platform}, `{os.path.basename(exe)} --ipc-stdio`")

    stderr_log = open(os.path.join(work, "app-stderr.log"), "w", encoding="utf-8")
    app = App(exe, stderr_log)
    open_live = None
    # The settings leave [strom] at its default, as a first-time user does.
    strom = Strom(STROM_URL)
    try:
        list_devices(app)

        reply = app.call("go_live", timeout=120)
        if "error" not in reply:
            fail("`go_live` with no Open Live address was not refused")
        passed(f"`go_live` with no Open Live address is refused: {reply['error'].get('message')}")

        # With or without the secret: the late-listener check needs none (#112).
        if which != "openlivedev":
            late_listener_check(app, strom, gateway_name, source_name, work)

        if not pat or which == "late-listener":
            app.ok("quit", {"confirmed": False})
            code = app.ended(30)
            if code != 0:
                fail(f"`quit` with nothing live did not end the app (exit {code})")
            passed("`quit` with nothing live ends the app")
            if which == "late-listener":
                say("e2e app (late-listener part): done")
                return
            missing = (
                "missing capability: the repository secret OSC_ACCESS_TOKEN (an Open Source Cloud "
                f"personal access token for the workspace that owns the Open Live instance '{instance}') is not set"
            )
            if os.environ.get("E2E_REQUIRE_SECRET", "true") == "true":
                fail(missing)
            print(f"::warning title=e2e app::{missing}; the app core checks above passed, Go live against '{instance}' did not run", flush=True)
            record(f"- SKIPPED: {missing}. The app core checks above passed; Go live, Stop and quit while live against '{instance}' did not run.")
            return

        # The reply lists the workspace's instances; `_take` masked their addresses
        # before the reply was printed.
        where = app.ok("osc_paste_token", {"token": pat}, timeout=90)
        instances = where.get("instances") or []
        chosen = [i for i in instances if i.get("name") == instance]
        if not chosen:
            fail(f"missing capability: the OSC_ACCESS_TOKEN workspace has no Open Live instance named '{instance}'")
        url = chosen[0]["url"]
        where = app.ok("choose_instance", {"url": url})
        if not where.get("where", {}).get("ready"):
            fail(f"Where to send is not done after `choose_instance`: {where.get('where', {}).get('problem')}")
        passed(f"Where to send: token pasted, instance '{instance}' chosen ({where['where'].get('summary')})")
        tick_the_test_pattern(app)

        open_live = OpenLive(url, pat)
        for production in open_live.all_productions():
            name = str(production.get("name", ""))
            if name.startswith(PREFIX) and not name.startswith(f"{gateway_name} — "):
                say(f"removing stale production {production.get('_id')} from an earlier run")
                open_live.remove_production(production.get("_id"))
        for source in open_live.sources():
            name = str(source.get("name", ""))
            if name.startswith(PREFIX) and not name.startswith(f"{gateway_name} — "):
                say(f"removing stale source {source.get('id')} from an earlier run")
                open_live.delete(source.get("id"))

        # 3. Go live, armed: no production holds the source yet. Then the late
        # receiver: the production is created and activated only now.
        live_at = time.monotonic()
        source, since = go_live(app, open_live, strom, source_name, "Go live", waits_for=("armed",))
        production_id, flow = late_receiver_check(app, open_live, strom, source, source_name, live_at, gateway_name)

        # 4. Stop: the flow goes, the source stays (#115).
        app.ok("stop")
        data = app.event("stopped", 120)
        stopped_cleanly(data, "Stop")
        kept = [s.get("id") for s in open_live.ours(gateway_name)]
        if kept != [source.get("id")]:
            fail(f"Stop: expected source {source.get('id')} kept as the only one of '{gateway_name}' in Open Live, found {kept}")
        passed(f"Stop: the app reports stopped, nothing left behind, and source {source.get('id')} is kept in Open Live")
        strom.check_gone(flow.get("id"), "Stop", app, since)

        # 5. Go live again: no 409, and the kept source reused with its id. The source
        # is there from the start, so go_live() checks the rows before the session has
        # registered anything; they are checked again once the row is armed, which is
        # after registration. The production still holds the source, inactive: the
        # row waits for it, and activating it again starts the flow (#130 AC2).
        again, since = go_live(app, open_live, strom, source_name, "Go live again (no 409)", waits_for=("waiting_for_production",))
        if app.failed_rows(since):
            fail(f"Go live again (no 409): an input failed: {app.failed_rows(since)}")
        if again.get("id") != source.get("id"):
            fail(f"Go live again: the source id changed from {source.get('id')} to {again.get('id')}")
        passed(f"Go live again: the same source {source.get('id')} is reused, so a production slot holding it stays valid")
        flow = reactivated_check(app, open_live, strom, production_id, source_name, since)

        # 6. Quit while live.
        app.ok("quit", {"confirmed": False})
        asked = app.event("quit_requested", 30)
        if not asked.get("inputs_live"):
            fail(f"Quit while live: the question counts {asked.get('inputs_live')} live inputs")
        passed(f"Quit while live: the app asks first (\"You are live with {asked['inputs_live']} input(s)\")")
        app.ok("quit", {"confirmed": True})
        code = app.ended(120)
        if code is None:
            fail("Quit while live: Stop and quit did not end the app within 120 s")
        if code != 0:
            fail(f"Quit while live: the app exited with {code}")
        stops = [m for m in app.seen if m.get("event") == "stopped"]
        if len(stops) < 2:
            fail("Quit while live: the app ended without a `stopped` event for the second session")
        stopped_cleanly(stops[-1]["data"], "Quit while live")
        kept = [s.get("id") for s in open_live.ours(gateway_name)]
        if kept != [source.get("id")]:
            fail(f"Quit while live: expected source {source.get('id')} kept as the only one of '{gateway_name}' in Open Live, found {kept}")
        passed(f"Quit while live: Stop and quit ended the app and source {source.get('id')} is kept in Open Live")
        strom.check_gone(flow.get("id"), "Quit while live", app, since)
        say("e2e app: all checks passed")
    finally:
        app.close()
        stderr_log.close()
        if open_live is not None:
            for production_id in list(open_live.productions):
                try:
                    say(f"removing leftover production {production_id}")
                    open_live.remove_production(production_id)
                except (Failed, urllib.error.URLError, OSError) as err:
                    say(f"could not remove production {production_id}: {err}")
            try:
                for source in open_live.ours(gateway_name):
                    say(f"removing leftover source {source.get('id')}")
                    open_live.delete(source.get("id"))
            except Failed as err:
                say(f"could not sweep leftover sources: {err}")


def main():
    if sys.platform == "win32":
        # A pipe on Windows is in the ANSI code page otherwise: see the header.
        for stream in (sys.stdout, sys.stderr):
            stream.reconfigure(encoding="utf-8", errors="replace")
    if len(sys.argv) != 2:
        print(f"usage: {sys.argv[0]} <path to open-live-ingest-app>", file=sys.stderr)
        return 2
    exe = os.path.abspath(sys.argv[1])
    work = os.path.join(os.environ.get("RUNNER_TEMP") or os.path.join(os.getcwd(), "target"), "oli-app-e2e")
    os.makedirs(work, exist_ok=True)
    try:
        run(exe, work)
    except Failed as err:
        print(f"::error title=e2e app::{err}", flush=True)
        record(f"- FAIL: {err}")
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
