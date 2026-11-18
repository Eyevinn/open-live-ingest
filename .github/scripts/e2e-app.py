#!/usr/bin/env python3
"""Agent-run end to end of the desktop app against an Open Live instance (OL-59i, #100;
docs/specs/desktop-app.md section 14, #62 AC2).

Drives the app executable a release artifact carries (the DMG's
Contents/MacOS/open-live-ingest-app) with `--ipc-stdio`: the app core with no window
and no webview, fed the same messages the page posts (`app/ui/app.js`), answering with
the same replies and events (`app/src/stdio.rs`). The release workflow's `e2e-macos`
job runs it on the DMG it has just built and checked:

    python3 -I .github/scripts/e2e-app.py "/Volumes/.../Open Live Ingest.app/Contents/MacOS/open-live-ingest-app"

In order, each logged and written to the step summary:
  1. `state`, and `devices`: the app lists devices through the Strom the bundle
     carries.
  2. Where to send, as the page does it with a pasted token: `osc_paste_token` with
     OSC_ACCESS_TOKEN, then `choose_instance` with the instance named
     OPEN_LIVE_INSTANCE (default openlivedev) from the workspace the reply lists.
     Then What to send: `set_inputs` with the test pattern ticked.
  3. `go_live`: the source `<gateway name> — Test pattern` appears in Open Live, and
     Strom runs a flow of that name.
  4. `stop`: the page's `stopped` event, with no error and nothing left behind; the
     source is gone from Open Live and the flow from Strom. When nothing answers at
     Strom's address any more, the app must have said it stopped the Strom it started
     (a `strom` event with state `stopped`); a Strom that died by itself fails.
  5. `go_live` again: no input fails (an Open Live 409 on a source left behind would
     fail it) and the source appears again.
  6. Quitting while live, as the close button does: `quit` asks (`quit_requested`),
     then `quit` with `confirmed: true` (the question's Stop and quit) stops and ends
     the process; the source is gone from Open Live and the flow from Strom.

Before step 2 the settings file the app reads is written with a gateway name unique
to the run (the source prefix, so cleanup finds exactly this run's sources) and the
SRT port range the instance does not publish, as the Advanced section would. The
app's own settings file is used (HOME is not moved), because the pasted token goes to
the login keychain, which lives under HOME.

Without OSC_ACCESS_TOKEN, step 1 still runs, and so does a `go_live` that has to be
refused (no Open Live address yet) and a `quit` that ends the process. The run then
fails naming the secret when E2E_REQUIRE_SECRET is `true` (a run by hand); otherwise
(a pull request run) it ends with a warning naming the secret and exits 0.

Whatever happens, the exit path closes the app (stdin closed: its bounded teardown)
and deletes any source still carrying the run's gateway name. Sources named
`oli-app-e2e-` left by an earlier run that was killed are removed before step 3.

Secrets and the bundled Strom (the reason is #83's, e2e-audio-device.sh): the app is
started with OSC_ACCESS_TOKEN and GH_TOKEN removed from its environment. The app
starts the bundled Strom with its own environment, and Strom would seed its own
Open Source Cloud access from OSC_ACCESS_TOKEN while its API is unauthenticated. The
app itself does not need the variable: the token reaches it over IPC
(`osc_paste_token`), as a pasted token does from the page. Strom also listens on all
interfaces; the workflow step loads a pf rule that drops traffic to its port (8080)
on every interface but loopback before this script runs, as #83 does with iptables.
Nothing GitHub documents promises that a hosted macOS runner cannot be reached from
outside, so the rule is loaded rather than assumed unneeded; the step fails if pf
does not take it.

Python standard library only. The token and the service token are masked as soon as
they are known. Every `url` value in the app's lines (the instance addresses the
workspace lists, the address chosen), and its host without the scheme, is masked
before the line is printed; loopback addresses (Strom's) are not. No response body
that can carry a secret is printed. Replies and events cannot carry a secret (the
core never serialises one), so the app's lines are logged.
"""

import json
import os
import queue
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.request

TOKEN_EXCHANGE = "https://token.svc.prod.osaas.io/servicetoken"
SERVICE_ID = "eyevinn-open-live"
TEST_PATTERN = "test"
TEST_PATTERN_NAME = "Test pattern"
PREFIX = "oli-app-e2e-"
# `[strom] url` when the settings do not set it (src/config.rs).
STROM_URL = "http://127.0.0.1:8080"
# Never handed to the app, so never to the Strom it starts (#83).
WITHHELD_ENV = ("OSC_ACCESS_TOKEN", "GH_TOKEN")

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


def mask_urls(value):
    """Masks every non-loopback `url` value in an app line, and its host without the
    scheme, so that the line can be printed."""
    if isinstance(value, dict):
        for key, item in value.items():
            if key == "url" and isinstance(item, str) and item:
                bare = item.split("://", 1)[-1].rstrip("/")
                host = bare.split("/", 1)[0]
                if host.rsplit(":", 1)[0] not in LOOPBACK_HOSTS:
                    mask(item)
                    mask(bare)
                    mask(host)
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


class OpenLive:
    def __init__(self, url, pat):
        self.url = url.rstrip("/")
        self.pat = pat
        self.token = None
        self.expires = 0.0

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

    def ours(self, gateway_name):
        prefix = f"{gateway_name} — "
        return [s for s in self.sources() if str(s.get("name", "")).startswith(prefix)]

    def delete(self, source_id):
        status, _ = http("DELETE", f"{self.url}/api/v1/sources/{source_id}", self._bearer())
        return status in (200, 202, 204, 404)

    def wait_for_source(self, name, timeout):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            found = [s for s in self.sources() if s.get("name") == name]
            if found:
                return found[0]
            time.sleep(2)
        fail(f"no source named '{name}' appeared in Open Live within {timeout} s")

    def wait_gone(self, gateway_name, timeout):
        deadline = time.monotonic() + timeout
        left = self.ours(gateway_name)
        while left and time.monotonic() < deadline:
            time.sleep(2)
            left = self.ours(gateway_name)
        return left


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

    def wait_for_flow(self, name, timeout):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            for flow in self.flows() or []:
                if flow.get("name") == name:
                    return flow
            time.sleep(2)
        fail(f"Strom at {self.url} runs no flow named '{name}' after {timeout} s")

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


def go_live(app, open_live, strom, source_name, step):
    """Go live; returns the source, the flow and the app line the session began at."""
    since = len(app.seen)
    reply = app.call("go_live", timeout=120)
    if "error" in reply:
        fail(f"{step}: `go_live` was refused: {reply['error'].get('message')}")
    source = open_live.wait_for_source(source_name, 120)
    if app.failed_rows(since):
        fail(f"{step}: an input failed: {app.failed_rows(since)}")
    passed(f"{step}: source '{source_name}' ({source.get('id')}) is in Open Live (streamType {source.get('streamType')}, status {source.get('status')})")
    flow = strom.wait_for_flow(source_name, 60)
    passed(f"{step}: Strom runs flow {flow.get('id')} for it")
    return source, flow, since


def stopped_cleanly(data, step):
    if data.get("error"):
        fail(f"{step}: the session stopped with an error: {data['error']}")
    if data.get("left_behind"):
        fail(f"{step}: the app says it left inputs behind: {data['left_behind']}")


def run(exe, work):
    instance = os.environ.get("OPEN_LIVE_INSTANCE") or "openlivedev"
    run_id = os.environ.get("GITHUB_RUN_ID", "local")
    attempt = os.environ.get("GITHUB_RUN_ATTEMPT", str(os.getpid()))
    gateway_name = f"{PREFIX}{run_id}-{attempt}"
    source_name = f"{gateway_name} — {TEST_PATTERN_NAME}"
    port_range = os.environ.get("E2E_PORT_RANGE") or "47300-47309"
    pat = os.environ.get("OSC_ACCESS_TOKEN") or ""
    mask(pat)

    settings = settings_file()
    if os.path.exists(settings):
        fail(f"{settings} exists already; run this on a machine where the app has not been set up")
    write_settings(settings, gateway_name, port_range)
    record(f"### App end to end against `{instance}`: {sys.platform}, `{os.path.basename(exe)} --ipc-stdio`")

    stderr_log = open(os.path.join(work, "app-stderr.log"), "w", encoding="utf-8")
    app = App(exe, stderr_log)
    open_live = None
    try:
        list_devices(app)

        if not pat:
            reply = app.call("go_live", timeout=120)
            if "error" not in reply:
                fail("`go_live` with no Open Live address was not refused")
            passed(f"`go_live` with no Open Live address is refused: {reply['error'].get('message')}")
            app.ok("quit", {"confirmed": False})
            code = app.ended(30)
            if code != 0:
                fail(f"`quit` with nothing live did not end the app (exit {code})")
            passed("`quit` with nothing live ends the app")
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
        for source in open_live.sources():
            name = str(source.get("name", ""))
            if name.startswith(PREFIX) and not name.startswith(f"{gateway_name} — "):
                say(f"removing stale source {source.get('id')} from an earlier run")
                open_live.delete(source.get("id"))

        # The settings leave [strom] at its default, as a first-time user does.
        strom = Strom(STROM_URL)

        # 3. Go live.
        _, flow, since = go_live(app, open_live, strom, source_name, "Go live")

        # 4. Stop.
        app.ok("stop")
        data = app.event("stopped", 120)
        stopped_cleanly(data, "Stop")
        left = open_live.wait_gone(gateway_name, 30)
        if left:
            fail(f"Stop: {len(left)} source(s) of '{gateway_name}' are still in Open Live")
        passed("Stop: the app reports stopped, nothing left behind, and the source is gone from Open Live")
        strom.check_gone(flow.get("id"), "Stop", app, since)

        # 5. Go live again: no 409. Open Live answers 409 when a source of the same
        # name is still registered, and the app shows that as an `input` row with
        # state `failed`. go_live() checks those rows once, as soon as the source is
        # back. That is enough only because step 4 has already checked that no source
        # of this gateway was left behind (wait_gone); do not loosen step 4 without
        # making this check wait for the rows to settle.
        _, flow, since = go_live(app, open_live, strom, source_name, "Go live again (no 409)")

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
        left = open_live.wait_gone(gateway_name, 30)
        if left:
            fail(f"Quit while live: {len(left)} source(s) of '{gateway_name}' are still in Open Live")
        passed("Quit while live: Stop and quit ended the app and the source is gone from Open Live")
        strom.check_gone(flow.get("id"), "Quit while live", app, since)
        say("e2e app: all checks passed")
    finally:
        app.close()
        stderr_log.close()
        if open_live is not None:
            try:
                for source in open_live.ours(gateway_name):
                    say(f"removing leftover source {source.get('id')}")
                    open_live.delete(source.get("id"))
            except Failed as err:
                say(f"could not sweep leftover sources: {err}")


def main():
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
