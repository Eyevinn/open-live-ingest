#!/usr/bin/env python3
"""Unit tests of the uplink SRT stats check in e2e-app.py (#112), of the stand-in
Open Live the no-secret late-listener check sends to, and of the armed-session checks
(#130: no flow until a production holding the source is active, the flow within 5 s
once it is, the row states, the SRT stats check after reactivation), and of the
preview and CPU checks (OL-70e: the picture and level events, the preview flow gone
after `preview {on: false}` and after Go live, Strom's CPU windows and the taps'
budget). No app and no network beyond loopback:

    python3 -I .github/scripts/test_e2e_app.py
"""

import base64
import contextlib
import importlib.util
import io
import json
import os
import unittest
import urllib.error
import urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
SPEC = importlib.util.spec_from_file_location("e2e_app", os.path.join(HERE, "e2e-app.py"))
e2e = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(e2e)
# Never write these tests' PASS and WARNING lines into the real job summary.
e2e.SUMMARY = os.devnull


def sample(at, sent, dropped, connected=True):
    return {"at": at, "connected": connected, "packets_sent": sent, "packets_sent_dropped": dropped, "rtt_ms": 5.0}


def stats(connections):
    """A `GET /api/flows/<id>/srt-stats` body in Strom 0.8.2's shape."""
    return {"flow_id": "f-1", "stats": {"connections": connections}}


class UplinkSample(unittest.TestCase):
    def test_the_sending_elements_first_peer_is_read(self):
        body = stats({
            "monitor:srtsrc": {"role": "source", "mode": "listener", "connected": True,
                               "callers": [{"packets_received": 9}]},
            "uplink:srtsink": {"role": "sink", "mode": "caller", "connected": True,
                               "callers": [{"packets_sent": 6082, "packets_sent_dropped": 6090, "rtt_ms": 5.5},
                                           {"packets_sent": 1}]},
        })
        got = e2e.uplink_sample(body)
        self.assertEqual(got["packets_sent"], 6082)
        self.assertEqual(got["packets_sent_dropped"], 6090)
        self.assertTrue(got["connected"])

    def test_no_peer_yet_is_no_sample(self):
        body = stats({"uplink:srtsink": {"role": "sink", "mode": "caller", "connected": False, "callers": []}})
        self.assertIsNone(e2e.uplink_sample(body))
        self.assertIsNone(e2e.uplink_sample(stats({})))
        self.assertIsNone(e2e.uplink_sample(None))

    def test_connected_false_and_a_missing_drop_count_are_read_as_such(self):
        body = stats({"uplink:srtsink": {"role": "sink", "connected": False, "callers": [{"packets_sent": 10}]}})
        got = e2e.uplink_sample(body)
        self.assertFalse(got["connected"])
        self.assertEqual(got["packets_sent_dropped"], 0)


class JudgeUplink(unittest.TestCase):
    def test_a_clean_uplink_passes(self):
        self.assertIsNone(e2e.judge_uplink([sample(0, 1000, 0), sample(60, 37000, 12)]))

    def test_exactly_one_percent_passes(self):
        self.assertIsNone(e2e.judge_uplink([sample(0, 0, 0), sample(60, 10000, 100)]))

    def test_more_than_one_percent_fails_with_the_numbers(self):
        problem = e2e.judge_uplink([sample(0, 0, 0), sample(60, 10000, 101)])
        self.assertIsNotNone(problem)
        for text in ("packets_sent grew by 10000", "packets_sent_dropped by 101", "1.01 %", "more than 1 %", "+60 s"):
            self.assertIn(text, problem)

    def test_the_drops_110_saw_fail(self):
        # #111: +6,082 sent and +6,090 dropped over 10 s, scaled to the 60 s window.
        problem = e2e.judge_uplink([sample(0, 1200, 1100), sample(60, 1200 + 36492, 1100 + 36540)])
        self.assertIn("100.13 % dropped", problem)

    def test_connected_false_fails_even_with_no_drops(self):
        problem = e2e.judge_uplink([sample(0, 0, 0), sample(60, 10000, 0, connected=False)])
        self.assertIn("connected=false", problem)

    def test_a_bad_middle_interval_fails_though_the_whole_window_is_under_the_limit(self):
        samples = [sample(0, 0, 0), sample(60, 10000, 0), sample(120, 10100, 50), sample(180, 1000000, 50)]
        problem = e2e.judge_uplink(samples)
        self.assertIn("between +60 s and +120 s", problem)

    def test_a_reconnect_fails(self):
        problem = e2e.judge_uplink([sample(0, 50000, 10), sample(60, 3000, 0)])
        self.assertIn("reconnected", problem)

    def test_nothing_sent_fails(self):
        problem = e2e.judge_uplink([sample(0, 500, 0), sample(60, 500, 0)])
        self.assertIn("sent no packets", problem)

    def test_one_sample_is_not_enough(self):
        self.assertIn("at least 2", e2e.judge_uplink([sample(0, 500, 0)]))

    def test_a_missing_packets_sent_fails(self):
        problem = e2e.judge_uplink([sample(0, None, 0), sample(60, 100, 0)])
        self.assertIn("no packets_sent", problem)


class UplinkProblems(unittest.TestCase):
    """The dropped share is kept apart from the fatal problems (E2E_UPLINK_DROP_WARN_ONLY)."""

    def test_drops_alone_are_only_the_dropped_problem(self):
        fatal, dropped = e2e.uplink_problems([sample(0, 0, 0), sample(60, 10000, 101)])
        self.assertIsNone(fatal)
        self.assertIn("1.01 % dropped", dropped)

    def test_a_reconnect_after_a_drop_is_still_fatal(self):
        samples = [sample(0, 0, 0), sample(60, 10000, 5000), sample(120, 300, 0)]
        fatal, dropped = e2e.uplink_problems(samples)
        self.assertIn("reconnected", fatal)
        self.assertIn("packets_sent_dropped by 5000", dropped)

    def test_connected_false_and_nothing_sent_are_fatal(self):
        self.assertIn("connected=false", e2e.uplink_problems([sample(0, 0, 0), sample(60, 10000, 9000, connected=False)])[0])
        self.assertIn("sent no packets", e2e.uplink_problems([sample(0, 500, 0), sample(60, 500, 0)])[0])

    def test_a_clean_uplink_has_no_problem(self):
        self.assertEqual(e2e.uplink_problems([sample(0, 0, 0), sample(60, 10000, 100)]), (None, None))


class SrtSettings(unittest.TestCase):
    def setUp(self):
        self.saved = {k: os.environ.pop(k, None) for k in ENV_KEYS}

    def tearDown(self):
        for key, value in self.saved.items():
            os.environ.pop(key, None)
            if value is not None:
                os.environ[key] = value

    def test_defaults(self):
        self.assertEqual(e2e.srt_settings(), (2, 60, 35))

    def test_the_issues_floor_holds(self):
        os.environ.update({"E2E_SRT_SAMPLES": "1", "E2E_SRT_INTERVAL": "5", "E2E_LATE_SECONDS": "3"})
        self.assertEqual(e2e.srt_settings(), (2, 60, 30))
        os.environ.update({"E2E_SRT_SAMPLES": "3", "E2E_SRT_INTERVAL": "90", "E2E_LATE_SECONDS": "45"})
        self.assertEqual(e2e.srt_settings(), (3, 90, 45))

    def test_a_value_that_is_not_a_number_fails_naming_it(self):
        os.environ["E2E_SRT_INTERVAL"] = "1m"
        with self.assertRaises(e2e.Failed) as caught:
            e2e.srt_settings()
        self.assertIn("E2E_SRT_INTERVAL must be a whole number, not '1m'", str(caught.exception))

    def test_the_drop_switch_is_off_unless_true(self):
        self.assertFalse(e2e.drops_warn_only())
        os.environ["E2E_UPLINK_DROP_WARN_ONLY"] = "false"
        self.assertFalse(e2e.drops_warn_only())
        os.environ["E2E_UPLINK_DROP_WARN_ONLY"] = "true"
        self.assertTrue(e2e.drops_warn_only())
        os.environ["E2E_UPLINK_DROP_WARN_ONLY"] = "yes"
        with self.assertRaises(e2e.Failed):
            e2e.drops_warn_only()

    def test_the_part(self):
        self.assertEqual(e2e.part(), "all")
        for name in ("late-listener", "openlivedev"):
            os.environ["E2E_PART"] = name
            self.assertEqual(e2e.part(), name)
        os.environ["E2E_PART"] = "late"
        with self.assertRaises(e2e.Failed):
            e2e.part()


ENV_KEYS = ("E2E_SRT_SAMPLES", "E2E_SRT_INTERVAL", "E2E_LATE_SECONDS", "E2E_UPLINK_DROP_WARN_ONLY", "E2E_PART")


class FakeStrom(e2e.Strom):
    """`watch_uplink` against canned samples, with no waiting."""

    def __init__(self, samples):
        super().__init__("http://127.0.0.1:9")
        self.samples = list(samples)

    def uplink(self, flow_id):
        return self.samples.pop(0)


class WatchUplink(unittest.TestCase):
    def setUp(self):
        self.sleep, self.wait = e2e.time.sleep, e2e.wait_until
        e2e.time.sleep = lambda s: None
        e2e.wait_until = lambda moment: None
        self.saved = {k: os.environ.pop(k, None) for k in ENV_KEYS}

    def tearDown(self):
        e2e.time.sleep, e2e.wait_until = self.sleep, self.wait
        for key, value in self.saved.items():
            os.environ.pop(key, None)
            if value is not None:
                os.environ[key] = value

    def watch(self, strom):
        """watch_uplink with its output captured, so that no `::warning::` reaches the
        job log as a real annotation; returns the output."""
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            strom.watch_uplink("f-1", "step")
        return out.getvalue()

    def peer(self, sent, dropped, connected=True):
        return {"connected": connected, "packets_sent": sent, "packets_sent_dropped": dropped, "rtt_ms": 5.0}

    def test_it_waits_for_a_connected_peer_then_fails_on_drops(self):
        strom = FakeStrom([None, self.peer(0, 0, connected=False), self.peer(100, 0), self.peer(10100, 5000)])
        with self.assertRaises(e2e.Failed) as caught:
            strom.watch_uplink("f-1", "Late listener")
        self.assertIn("Late listener: the uplink dropped too much", str(caught.exception))
        self.assertIn("packets_sent_dropped by 5000", str(caught.exception))

    def test_a_peer_that_goes_away_fails(self):
        strom = FakeStrom([self.peer(100, 0), None])
        with self.assertRaises(e2e.Failed) as caught:
            strom.watch_uplink("f-1", "step")
        self.assertIn("reports no peer any more", str(caught.exception))

    def test_a_clean_uplink_passes(self):
        out = self.watch(FakeStrom([self.peer(100, 0), self.peer(20100, 3)]))
        self.assertIn("PASS: step: the uplink stayed connected and dropped 3 of 20000", out)

    def test_it_waits_for_the_first_peer_with_counters(self):
        # Release run 45466786038: the first poll came before srtsink had peer counters.
        out = self.watch(FakeStrom([self.peer(None, 0), self.peer(100, 0), self.peer(20100, 3)]))
        self.assertIn("PASS: step: the uplink stayed connected and dropped 3 of 20000", out)

    def test_a_connected_peer_that_never_reports_counters_fails(self):
        strom = FakeStrom([])
        strom.uplink = lambda flow_id: self.peer(None, 0)
        clock = iter(range(0, 10 ** 6, 2))
        monotonic, e2e.time.monotonic = e2e.time.monotonic, lambda: next(clock)
        try:
            with self.assertRaises(e2e.Failed) as caught:
                self.watch(strom)
        finally:
            e2e.time.monotonic = monotonic
        message = str(caught.exception)
        self.assertIn(f"shows no connected peer with SRT counters (packets_sent) within {e2e.CONNECT_TIMEOUT} s", message)
        self.assertIn("'packets_sent': None", message)

    # E2E_UPLINK_DROP_WARN_ONLY (until #111 is fixed): only the dropped share is a warning.

    def drops(self):
        return FakeStrom([self.peer(1200, 1100), self.peer(1200 + 36492, 1100 + 36540)])

    def test_drops_fail_when_the_switch_is_off(self):
        for value in (None, "false"):
            if value is not None:
                os.environ["E2E_UPLINK_DROP_WARN_ONLY"] = value
            with self.assertRaises(e2e.Failed) as caught:
                self.watch(self.drops())
            self.assertIn("step: the uplink dropped too much", str(caught.exception))
            self.assertIn("100.13 % dropped", str(caught.exception))

    def test_drops_only_warn_with_the_numbers_when_the_switch_is_on(self):
        os.environ["E2E_UPLINK_DROP_WARN_ONLY"] = "true"
        out = self.watch(self.drops())
        self.assertIn("::warning title=e2e app::step: the uplink dropped too much", out)
        for text in ("packets_sent grew by 36492", "packets_sent_dropped by 36540", "100.13 % dropped", "#111"):
            self.assertIn(text, out)
        self.assertNotIn("PASS", out)

    def test_connected_false_nothing_sent_and_a_reconnect_stay_fatal_when_the_switch_is_on(self):
        os.environ["E2E_UPLINK_DROP_WARN_ONLY"] = "true"
        cases = {
            "connected=false": [self.peer(100, 0), self.peer(20100, 0, connected=False)],
            "sent no packets": [self.peer(100, 0), self.peer(100, 0)],
            "reconnected": [self.peer(50000, 40000), self.peer(3000, 2900)],
        }
        for text, samples in cases.items():
            with self.assertRaises(e2e.Failed) as caught:
                self.watch(FakeStrom(samples))
            self.assertIn(text, str(caught.exception))

    def test_a_peer_that_never_connects_stays_fatal_when_the_switch_is_on(self):
        os.environ["E2E_UPLINK_DROP_WARN_ONLY"] = "true"
        e2e_timeout, e2e.CONNECT_TIMEOUT = e2e.CONNECT_TIMEOUT, 0
        try:
            with self.assertRaises(e2e.Failed) as caught:
                self.watch(FakeStrom([]))
        finally:
            e2e.CONNECT_TIMEOUT = e2e_timeout
        self.assertIn("shows no connected peer", str(caught.exception))


class LocalOpenLive(unittest.TestCase):
    def setUp(self):
        self.ol = e2e.LocalOpenLive(47123)

    def tearDown(self):
        self.ol.close()

    def call(self, method, path, body=None):
        data = None if body is None else json.dumps(body).encode()
        request = urllib.request.Request(self.ol.url + path, data=data, method=method)
        if data is not None:
            request.add_header("content-type", "application/json")
        try:
            with urllib.request.urlopen(request, timeout=5) as response:
                raw = response.read()
                return response.status, json.loads(raw) if raw else None
        except urllib.error.HTTPError as err:
            return err.code, None

    def test_server_info_publishes_loopback_and_the_one_port(self):
        self.assertEqual(
            self.call("GET", "/api/v1/server-info"),
            (200, {"stromHost": "127.0.0.1", "srtPorts": [47123], "srtPortState": "reserved"}),
        )

    def test_sources_are_created_with_the_port_patched_and_deleted(self):
        payload = {"name": "gw — Test pattern", "address": "srt://:0?mode=listener", "streamType": "srt",
                   "status": "inactive", "latency": 200, "liveCamera": True}
        status, created = self.call("POST", "/api/v1/sources", payload)
        self.assertEqual(status, 201)
        self.assertEqual(created["address"], "srt://:47123?mode=listener")
        status, patched = self.call("PATCH", f"/api/v1/sources/{created['id']}",
                                    dict(payload, address="srt://:47123?mode=listener", status="active"))
        self.assertEqual((status, patched["status"]), (200, "active"))
        self.assertEqual([s["name"] for s in self.call("GET", "/api/v1/sources")[1]], ["gw — Test pattern"])
        self.assertEqual(self.ol.ours("gw"), [patched])
        self.assertEqual(self.call("DELETE", f"/api/v1/sources/{created['id']}")[0], 204)
        self.assertEqual(self.call("DELETE", f"/api/v1/sources/{created['id']}")[0], 404)
        self.assertEqual(self.call("PATCH", "/api/v1/sources/nope", payload)[0], 404)
        self.assertEqual(self.call("GET", "/api/v1/sources"), (200, []))

    def test_productions_are_listed_as_open_live_returns_them(self):
        self.assertEqual(self.call("GET", "/api/v1/productions"), (200, []))
        self.ol.put_production("p-1", "gw — production", "src-1", "inactive")
        self.ol.put_production("p-1", "gw — production", "src-1", "active")
        self.assertEqual(
            self.call("GET", "/api/v1/productions"),
            (200, [{"_id": "p-1", "name": "gw — production", "status": "active",
                    "sources": [{"sourceId": "src-1", "mixerInput": "video_in_0"}]}]),
        )

    def test_unknown_routes_are_404(self):
        self.assertEqual(self.call("GET", "/api/v1/productions/p-1")[0], 404)
        self.assertEqual(self.call("GET", "/api/v1/nothing")[0], 404)


class ScriptedStrom(e2e.Strom):
    """`flows` answers from a script, one entry per call (the last repeats); a clock
    that moves only when the script sleeps."""

    def __init__(self, script):
        super().__init__("http://127.0.0.1:9")
        self.script = list(script)

    def flows(self):
        return self.script.pop(0) if len(self.script) > 1 else self.script[0]


class Clock:
    def __init__(self):
        self.now = 0.0

    def monotonic(self):
        return self.now

    def sleep(self, seconds):
        self.now += max(seconds, 0.01)


class StromFlows(unittest.TestCase):
    """#130: an armed session runs no flow, and the flow comes and goes with the
    production."""

    def setUp(self):
        self.clock = Clock()
        self.saved = (e2e.time.monotonic, e2e.time.sleep)
        e2e.time.monotonic, e2e.time.sleep = self.clock.monotonic, self.clock.sleep

    def tearDown(self):
        e2e.time.monotonic, e2e.time.sleep = self.saved

    FLOW = {"id": "f-1", "name": "gw — Test pattern"}

    def test_no_flow_passes_while_strom_runs_none_of_that_name(self):
        ScriptedStrom([[], [{"id": "f-9", "name": "other"}]]).no_flow("gw — Test pattern", 7, "armed")

    def test_a_flow_while_armed_fails(self):
        with self.assertRaises(e2e.Failed) as caught:
            ScriptedStrom([[], [], [self.FLOW]]).no_flow("gw — Test pattern", 7, "armed")
        self.assertIn("armed: Strom runs flow f-1", str(caught.exception))

    def test_no_flow_with_strom_never_answering_proves_nothing(self):
        with self.assertRaises(e2e.Failed) as caught:
            ScriptedStrom([None]).no_flow("gw — Test pattern", 3, "armed")
        self.assertIn("proves nothing", str(caught.exception))

    def test_a_single_look_is_taken_with_no_time(self):
        ScriptedStrom([[]]).no_flow("gw — Test pattern", 0, "activating")
        with self.assertRaises(e2e.Failed):
            ScriptedStrom([[self.FLOW]]).no_flow("gw — Test pattern", 0, "activating")

    def test_the_flow_has_to_start_within_the_timeout(self):
        strom = ScriptedStrom([[], None, [], [self.FLOW]])
        self.assertEqual(strom.wait_for_flow("gw — Test pattern", 5, "active", every=0.25), self.FLOW)
        self.assertLessEqual(self.clock.now, 5)
        with self.assertRaises(e2e.Failed) as caught:
            ScriptedStrom([[]]).wait_for_flow("gw — Test pattern", 5, "active", every=0.25)
        self.assertIn("active: Strom at http://127.0.0.1:9 runs no flow named 'gw — Test pattern' after 5 s", str(caught.exception))

    def test_the_flow_has_to_go_and_strom_has_to_answer(self):
        ScriptedStrom([[self.FLOW], [self.FLOW], []]).wait_flow_gone("gw — Test pattern", 15, "inactive")
        for script in ([[self.FLOW]], [None]):
            with self.assertRaises(e2e.Failed) as caught:
                ScriptedStrom(script).wait_flow_gone("gw — Test pattern", 15, "inactive")
            self.assertIn("inactive: Strom at http://127.0.0.1:9 still runs a flow", str(caught.exception))


class FakeApp(e2e.App):
    """An App over canned lines, with no process."""

    def __init__(self, lines):
        self.lines = e2e.queue.Queue()
        for line in lines:
            self.lines.put(line)
        self.seen = []

    def _take(self, timeout):
        try:
            message = self.lines.get_nowait()
        except e2e.queue.Empty:
            return "timeout"
        if message is not None:
            self.seen.append(message)
        return message


def row(state, name="gw — Test pattern"):
    return {"event": "input", "data": {"input": name, "state": state}}


class Rows(unittest.TestCase):
    def test_the_last_state_of_the_row_counts(self):
        app = FakeApp([row("registering"), row("armed"), row("connecting", name="other"), {"event": "summary"}])
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            self.assertEqual(app.wait_row("gw — Test pattern", ("armed",), 0, "Go live", timeout=1), "armed")
        self.assertIsNone(app.row_state("gw — Test pattern", len(app.seen)))

    def test_a_row_that_never_gets_there_fails_naming_its_state(self):
        app = FakeApp([row("registering"), row("connecting")])
        out = io.StringIO()
        with contextlib.redirect_stdout(out), self.assertRaises(e2e.Failed) as caught:
            app.wait_row("gw — Test pattern", e2e.ARMED, 0, "Go live", timeout=0.2)
        self.assertIn("Go live: the row of 'gw — Test pattern' is 'connecting', not armed or waiting_for_production", str(caught.exception))


class EventsSaidBeforeTheReply(unittest.TestCase):
    """`quit_requested` comes before the reply to `quit`: `ok()` reads it into `seen`
    while it waits for the reply, and the wait for the event must still find it (#145)."""

    def quit_app(self, before_reply):
        app = FakeApp(before_reply + [{"id": 1, "ok": None}])
        app.send = lambda cmd, args=None: 1
        return app

    def test_an_event_read_while_waiting_for_the_reply_is_found(self):
        app = self.quit_app([{"event": "quit_requested", "data": {"inputs_live": 1}}])
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            mark = len(app.seen)
            app.ok("quit", {"confirmed": False})
            asked = app.event_since("quit_requested", mark, 0.2)
        self.assertEqual(asked, {"inputs_live": 1})

    def test_waiting_only_for_a_new_event_misses_it(self):
        app = self.quit_app([{"event": "quit_requested", "data": {"inputs_live": 1}}])
        out = io.StringIO()
        with contextlib.redirect_stdout(out), self.assertRaises(e2e.Failed):
            app.ok("quit", {"confirmed": False})
            app.event("quit_requested", 0.2)

    def test_an_event_from_before_the_mark_does_not_count(self):
        app = FakeApp([{"event": "stopped", "data": {"error": None}}, {"id": 1, "ok": None}])
        app.send = lambda cmd, args=None: 1
        out = io.StringIO()
        with contextlib.redirect_stdout(out), self.assertRaises(e2e.Failed):
            app.drain()
            mark = len(app.seen)
            app.ok("stop")
            app.event_since("stopped", mark, 0.2)

    def test_an_event_after_the_reply_is_still_waited_for(self):
        app = FakeApp([{"id": 1, "ok": None}, {"event": "stopped", "data": {"left_behind": []}}])
        app.send = lambda cmd, args=None: 1
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            mark = len(app.seen)
            app.ok("stop")
            self.assertEqual(app.event_since("stopped", mark, 0.2), {"left_behind": []})


class FakeOpenLive:
    """Open Live's activate, with no network: `while_activating` runs once, as for a
    production that is still activating at the first look."""

    def __init__(self):
        self.activated = []

    def activate(self, production_id, timeout, while_activating=lambda: None):
        while_activating()
        self.activated.append(production_id)


class ReactivatedStrom(ScriptedStrom):
    """Flows from a script, and the uplink SRT stats from canned samples."""

    def __init__(self, script, samples):
        super().__init__(script)
        self.samples = list(samples)
        self.watched = []

    def uplink(self, flow_id):
        self.watched.append(flow_id)
        return self.samples.pop(0)


class Reactivated(unittest.TestCase):
    """#130 AC2: the reconnect after reactivation gets the same SRT stats check as
    the first connect."""

    FLOW = {"id": "f-2", "name": "gw — Test pattern"}

    def setUp(self):
        self.clock = Clock()
        self.saved = (e2e.time.monotonic, e2e.time.sleep, e2e.wait_until)
        e2e.time.monotonic, e2e.time.sleep = self.clock.monotonic, self.clock.sleep
        e2e.wait_until = lambda moment: None
        self.env = {k: os.environ.pop(k, None) for k in ENV_KEYS}

    def tearDown(self):
        e2e.time.monotonic, e2e.time.sleep, e2e.wait_until = self.saved
        for key, value in self.env.items():
            os.environ.pop(key, None)
            if value is not None:
                os.environ[key] = value

    def check(self, samples):
        strom = ReactivatedStrom([[], [], [self.FLOW]], samples)
        open_live = FakeOpenLive()
        app = FakeApp([row("waiting_for_production"), row("connecting")])
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            flow = e2e.reactivated_check(app, open_live, strom, "p-1", "gw — Test pattern", 0)
        return flow, strom, open_live, out.getvalue()

    def peer(self, sent, dropped, connected=True):
        return {"connected": connected, "packets_sent": sent, "packets_sent_dropped": dropped, "rtt_ms": 5.0}

    def test_a_clean_reconnect_is_watched_on_the_new_flow(self):
        flow, strom, open_live, out = self.check([self.peer(100, 0), self.peer(20100, 3)])
        self.assertEqual(flow, self.FLOW)
        self.assertEqual(open_live.activated, ["p-1"])
        self.assertEqual(set(strom.watched), {"f-2"})
        self.assertIn("PASS: Go live again: production reactivated: Strom runs flow f-2", out)
        self.assertIn("PASS: Go live again: production reactivated: the uplink stayed connected and dropped 3 of 20000", out)

    def test_drops_after_the_reconnect_fail(self):
        with self.assertRaises(e2e.Failed) as caught:
            self.check([self.peer(100, 0), self.peer(10100, 5000)])
        self.assertIn("Go live again: production reactivated: the uplink dropped too much", str(caught.exception))

    def test_a_reconnect_that_never_gets_a_peer_fails(self):
        timeout, e2e.CONNECT_TIMEOUT = e2e.CONNECT_TIMEOUT, 0
        try:
            with self.assertRaises(e2e.Failed) as caught:
                self.check([])
        finally:
            e2e.CONNECT_TIMEOUT = timeout
        self.assertIn("Go live again: production reactivated: the uplink of flow f-2 shows no connected peer", str(caught.exception))

    def test_a_flow_while_reactivating_fails(self):
        strom = ReactivatedStrom([[self.FLOW]], [])
        with contextlib.redirect_stdout(io.StringIO()), self.assertRaises(e2e.Failed) as caught:
            e2e.reactivated_check(FakeApp([]), FakeOpenLive(), strom, "p-1", "gw — Test pattern", 0)
        self.assertIn("Go live again: production reactivated (activating): Strom runs flow f-2", str(caught.exception))
        self.assertEqual(strom.watched, [])


class ProductionStatusPolls(unittest.TestCase):
    """#145: Open Live reports `ended`, not `inactive`, once an active production is
    deactivated; `deactivate` takes either as done, and activating again from `ended`
    goes through `activating` to `active`. Open Live's HTTP is scripted, no network."""

    def setUp(self):
        self.clock = Clock()
        self.saved = (e2e.time.monotonic, e2e.time.sleep, e2e.http)
        e2e.time.monotonic, e2e.time.sleep = self.clock.monotonic, self.clock.sleep
        self.calls = []
        self.statuses = []
        e2e.http = self.http
        self.open_live = e2e.OpenLive("https://open-live.example", "pat")
        self.open_live._bearer = lambda: {}

    def tearDown(self):
        e2e.time.monotonic, e2e.time.sleep, e2e.http = self.saved

    def http(self, method, url, headers=None, body=None, timeout=30):
        self.calls.append((method, url.rsplit("/", 1)[-1]))
        if method == "POST":
            return 200, {}
        status = self.statuses.pop(0) if len(self.statuses) > 1 else self.statuses[0]
        return 200, {"_id": "p-1", "status": status}

    def test_deactivate_takes_ended_as_done(self):
        self.statuses = ["active", "ended"]
        self.open_live.deactivate("p-1", 180)
        self.assertEqual(self.calls, [("POST", "deactivate"), ("GET", "p-1"), ("GET", "p-1")])

    def test_deactivate_still_takes_inactive_as_done(self):
        self.statuses = ["inactive"]
        self.open_live.deactivate("p-1", 180)
        self.assertEqual(self.calls, [("POST", "deactivate"), ("GET", "p-1")])

    def test_deactivate_that_never_gets_idle_fails_naming_the_status(self):
        self.statuses = ["active"]
        with self.assertRaises(e2e.Failed) as caught:
            self.open_live.deactivate("p-1", 180)
        self.assertIn("production p-1 was still 'active' 180 s after its deactivation", str(caught.exception))

    def test_activating_again_from_ended_reaches_active(self):
        self.statuses = ["ended", "active"]
        self.open_live.deactivate("p-1", 180)
        self.statuses = ["activating", "active"]
        looks = []
        self.open_live.activate("p-1", 300, lambda: looks.append(self.clock.now))
        self.assertEqual(len(looks), 1)
        self.assertEqual(self.calls[-3:], [("POST", "activate"), ("GET", "p-1"), ("GET", "p-1")])


# --- the preview (OL-70e) ---------------------------------------------------------------


JPEG = b"\xff\xd8\xff\xe0" + b"\x00" * 60


def image(raw=JPEG):
    return "data:image/jpeg;base64," + base64.b64encode(raw).decode()


def picture(state="showing", raw=JPEG, device="test"):
    return {"event": "preview", "data": {"device": device, "state": state,
                                         "image": image(raw) if state == "showing" else None,
                                         "message": None, "action": None}}


def level(peaks, device="test"):
    return {"event": "level", "data": {"levels": [{"device": device, "peak_db": peaks, "rms_db": peaks}]}}


class PreviewEvents(unittest.TestCase):
    def test_a_jpeg_data_url_is_a_picture(self):
        self.assertEqual(e2e.jpeg_bytes(image()), JPEG)

    def test_anything_else_is_not(self):
        self.assertIsNone(e2e.jpeg_bytes(image(b"\x89PNG\r\n")))
        self.assertIsNone(e2e.jpeg_bytes("data:image/png;base64," + base64.b64encode(JPEG).decode()))
        self.assertIsNone(e2e.jpeg_bytes("data:image/jpeg;base64,!!not base64!!"))
        self.assertIsNone(e2e.jpeg_bytes(None))

    def test_a_showing_jpeg_of_the_device_is_what_the_check_waits_for(self):
        where = e2e.shows_picture("test")
        self.assertTrue(where(picture()["data"]))
        self.assertFalse(where(picture(state="starting")["data"]))
        self.assertFalse(where(picture(device="cam")["data"]))
        self.assertFalse(where(picture(raw=b"GIF89a")["data"]))

    def test_the_loudest_channel_of_the_device_counts(self):
        data = {"levels": [{"device": "cam", "peak_db": [-1.0]}, {"device": "test", "peak_db": [-70.0, -2.5, None]}]}
        self.assertEqual(e2e.loudest(data, "test"), -2.5)
        self.assertIsNone(e2e.loudest({"levels": []}, "test"))
        self.assertTrue(e2e.has_sound("test")(data))

    def test_silence_at_or_below_minus_60_is_no_sound(self):
        self.assertFalse(e2e.has_sound("test")(level([-60.0, -80.0])["data"]))
        self.assertTrue(e2e.has_sound("test")(level([-59.9])["data"]))
        self.assertFalse(e2e.has_sound("test")(level([-3.0], device="cam")["data"]))

    def test_no_picture_is_logged(self):
        shown = e2e.printable(picture())
        self.assertNotIn("base64", json.dumps(shown))
        self.assertEqual(shown["data"]["image"], f"<{len(image())} characters>")
        self.assertEqual(picture()["data"]["image"], image(), "the line itself is kept as it came")
        self.assertEqual(e2e.printable(level([-3.0])), level([-3.0]))

    def test_level_lines_are_logged_once_every_ten_seconds(self):
        clock = Clock()
        saved = e2e.time.monotonic
        e2e.time.monotonic = clock.monotonic
        try:
            app = object.__new__(e2e.App)
            app.level_logged_at = None
            logged = []
            for _ in range(50):
                logged.append(app._logs(level([-3.0])))
                clock.now += 0.25
            self.assertEqual(logged.count(True), 2)
            self.assertTrue(app._logs({"event": "input"}))
        finally:
            e2e.time.monotonic = saved


class ScriptedApp(FakeApp):
    """A FakeApp that answers each command with the next canned reply, the events
    listed before it first, and records what was sent."""

    def __init__(self, answers):
        super().__init__([])
        self.answers = list(answers)
        self.sent = []
        self.next_id = 1

    def send(self, cmd, args=None):
        message_id = self.next_id
        self.next_id += 1
        self.sent.append((cmd, args))
        events, ok = self.answers.pop(0)
        for line in events + [{"id": message_id, "ok": ok}]:
            self.lines.put(line)
        return message_id


class FakeCpu:
    def __init__(self, figures=()):
        self.figures = list(figures)
        self.measured = []
        self.taps = None

    def measure(self, what):
        self.measured.append(what)
        return self.figures.pop(0) if self.figures else 1.0


PREVIEW = {"id": "p-1", "name": "gw — Test pattern (preview)"}
UPLINK = {"id": "f-1", "name": "gw — Test pattern"}
TICKED = {"video": [{"name": "Test pattern", "ticked": True}], "name_prefix": "gw — "}


class PreviewCheck(unittest.TestCase):
    """Step 2: `preview {on}` gives a JPEG and a level, the preview flow runs; off
    removes it; the CPU is measured with and without it."""

    def setUp(self):
        self.clock = Clock()
        self.saved = (e2e.time.monotonic, e2e.time.sleep)
        e2e.time.monotonic, e2e.time.sleep = self.clock.monotonic, self.clock.sleep

    def tearDown(self):
        e2e.time.monotonic, e2e.time.sleep = self.saved

    def check(self, on_events, flows):
        app = ScriptedApp([([], TICKED), (on_events, {"available": True}), ([picture("stopped")], {"available": True})])
        cpu = FakeCpu()
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            e2e.preview_check(app, ScriptedStrom(flows), "gw — Test pattern", cpu)
        return app, cpu, out.getvalue()

    def test_picture_level_flow_then_gone(self):
        app, cpu, out = self.check([picture("starting"), level([-3.0]), picture()], [[PREVIEW], [PREVIEW], []])
        self.assertEqual([c for c, _ in app.sent], ["set_inputs", "preview", "preview"])
        self.assertEqual([a for _, a in app.sent][1:], [{"on": True}, {"on": False}])
        self.assertEqual(cpu.measured, ["Step 2, preview flow running (picture polled)", "Step 2, preview off (held Strom, no flow)"])
        self.assertIn(f"a JPEG of {len(JPEG)} bytes", out)
        self.assertIn("a level of -3.0 dBFS", out)
        self.assertIn("the preview flow is gone from Strom's GET /api/flows", out)
        self.assertNotIn("base64", out)

    def test_no_picture_fails(self):
        with self.assertRaises(e2e.Failed) as caught:
            self.check([level([-3.0]), picture("starting")], [[PREVIEW], []])
        self.assertIn("no `preview` event from the app within", str(caught.exception))

    def test_a_silent_level_fails(self):
        with self.assertRaises(e2e.Failed) as caught:
            self.check([picture(), level([-80.0])], [[PREVIEW], []])
        self.assertIn("no `level` event from the app within", str(caught.exception))

    def test_a_preview_flow_left_after_off_fails(self):
        with self.assertRaises(e2e.Failed) as caught:
            self.check([picture(), level([-3.0])], [[PREVIEW]])
        self.assertIn("Preview (step 2): preview off: Strom at http://127.0.0.1:9 still runs a flow named "
                      "'gw — Test pattern (preview)' 15 s after `preview {on: false}`", str(caught.exception))

    def test_the_preview_not_available_fails(self):
        app = ScriptedApp([([], TICKED), ([], {"available": False})])
        with contextlib.redirect_stdout(io.StringIO()), self.assertRaises(e2e.Failed) as caught:
            e2e.preview_check(app, ScriptedStrom([[]]), "gw — Test pattern", FakeCpu())
        self.assertIn("the preview is not available", str(caught.exception))


class PreviewAtGoLive(unittest.TestCase):
    """Go live removes the preview flow before the uplink starts, and the picture and
    level while live come from the uplink flow."""

    def setUp(self):
        self.clock = Clock()
        self.saved = (e2e.time.monotonic, e2e.time.sleep)
        e2e.time.monotonic, e2e.time.sleep = self.clock.monotonic, self.clock.sleep

    def tearDown(self):
        e2e.time.monotonic, e2e.time.sleep = self.saved

    def test_armed_with_no_preview_flow_passes(self):
        ScriptedStrom([[], [{"id": "x", "name": "other (preview)"}]]).no_flow(
            "gw — Test pattern", 7, "Go live", preview="gw — Test pattern (preview)")

    def test_a_preview_flow_left_after_go_live_fails(self):
        with self.assertRaises(e2e.Failed) as caught:
            ScriptedStrom([[], [PREVIEW]]).no_flow("gw — Test pattern", 7, "Go live", preview="gw — Test pattern (preview)")
        self.assertIn("Go live: Strom still runs the preview flow p-1", str(caught.exception))

    def test_a_preview_flow_next_to_the_uplink_fails(self):
        with contextlib.redirect_stdout(io.StringIO()):
            flow, _ = e2e.sends_once_live(ScriptedStrom([[UPLINK]]), "gw — Test pattern", "active", 0.0)
        self.assertEqual(flow, UPLINK)
        with contextlib.redirect_stdout(io.StringIO()), self.assertRaises(e2e.Failed) as caught:
            e2e.sends_once_live(ScriptedStrom([[UPLINK, PREVIEW]]), "gw — Test pattern", "active", 0.0)
        self.assertIn("active: Strom runs the preview flow p-1 next to the uplink f-1", str(caught.exception))

    def test_picture_and_level_from_the_uplink(self):
        app = FakeApp([picture("camera_off"), row("connecting"), picture("starting"), picture(), level([-2.0])])
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            e2e.live_preview_check(app, ScriptedStrom([[UPLINK]]), "gw — Test pattern", 0, "Late receiver")
        self.assertIn("a level of -2.0 dBFS from the uplink flow", out.getvalue())

    def test_no_picture_while_live_fails(self):
        app = FakeApp([picture("starting"), level([-2.0])])
        with contextlib.redirect_stdout(io.StringIO()), self.assertRaises(e2e.Failed):
            e2e.live_preview_check(app, ScriptedStrom([[UPLINK]]), "gw — Test pattern", 0, "Late receiver")

    def test_a_preview_flow_while_live_fails(self):
        app = FakeApp([picture(), level([-2.0])])
        with contextlib.redirect_stdout(io.StringIO()), self.assertRaises(e2e.Failed) as caught:
            e2e.live_preview_check(app, ScriptedStrom([[UPLINK, PREVIEW]]), "gw — Test pattern", 0, "Late receiver")
        self.assertIn("Strom runs the preview flow p-1 while live", str(caught.exception))


# --- Strom's CPU (OL-70e) ---------------------------------------------------------------


class CpuFigures(unittest.TestCase):
    def test_ps_time_values(self):
        self.assertAlmostEqual(e2e.cpu_seconds("0:01.23"), 1.23)
        self.assertAlmostEqual(e2e.cpu_seconds(" 12:03.50\n"), 723.5)
        self.assertAlmostEqual(e2e.cpu_seconds("1:00:00.00"), 3600.0)
        self.assertAlmostEqual(e2e.cpu_seconds("00:00:07"), 7.0)
        self.assertAlmostEqual(e2e.cpu_seconds("2-00:00:01"), 172801.0)

    def test_share_is_percent_of_one_core(self):
        self.assertAlmostEqual(e2e.share((10.0, 5.0), (70.0, 8.0)), 5.0)
        self.assertAlmostEqual(e2e.share((0.0, 0.0), (60.0, 90.0)), 150.0)
        with self.assertRaises(e2e.Failed):
            e2e.share((5.0, 1.0), (5.0, 2.0))

    def test_the_taps_budget_is_per_input(self):
        self.assertEqual(e2e.taps_verdict(12.0, 9.0), (3.0, None))
        self.assertEqual(e2e.taps_verdict(14.0, 9.0)[1], None, "exactly 5 % passes")
        cost, over = e2e.taps_verdict(16.0, 9.0)
        self.assertEqual(cost, 7.0)
        for text in ("7.00 % of one core per input", "16.00 % polled minus 9.00 % not polled", "over the 5 % budget"):
            self.assertIn(text, over)
        self.assertEqual(e2e.taps_verdict(16.0, 9.0, inputs=2), (3.5, None))


class CpuWindows(unittest.TestCase):
    """Cpu.measure over a scripted sampler and clock: the figure is the CPU seconds
    between both ends over the wall time, the pid is read from strom.pid."""

    def setUp(self):
        self.clock = Clock()
        self.saved = (e2e.time.monotonic, e2e.time.sleep, e2e.wait_until, e2e.SUMMARY)
        e2e.time.monotonic, e2e.time.sleep = self.clock.monotonic, self.clock.sleep
        e2e.wait_until = lambda moment: setattr(self.clock, "now", max(self.clock.now, moment))
        self.env = os.environ.pop("E2E_CPU_SECONDS", None)
        self.dir = __import__("tempfile").mkdtemp()
        self.pidfile = os.path.join(self.dir, "strom.pid")
        with open(self.pidfile, "w", encoding="utf-8") as f:
            f.write("4242\n")

    def tearDown(self):
        e2e.time.monotonic, e2e.time.sleep, e2e.wait_until, e2e.SUMMARY = self.saved
        if self.env is not None:
            os.environ["E2E_CPU_SECONDS"] = self.env
        __import__("shutil").rmtree(self.dir)

    def cpu(self, sample, read_along=False):
        cpu = e2e.Cpu(self.pidfile)
        cpu.sample = sample
        cpu.read_along = read_along
        return cpu

    def test_a_window_of_60_s_with_the_cpu_seconds_at_both_ends(self):
        pids = []

        def sample(pid):
            pids.append(pid)
            return self.clock.now * 0.03, None  # 3 % of one core

        cpu = self.cpu(sample)
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            percent = cpu.measure("Live, uplink with the picture polled")
        self.assertAlmostEqual(percent, 3.0)
        self.assertEqual(set(pids), {4242})
        self.assertEqual(len(pids), 2, "without reading along, only at both ends (PowerShell)")
        self.assertGreaterEqual(cpu.figures[0]["wall"], 60)
        self.assertIn("CPU (hosted runner): Live, uplink with the picture polled: 3.00 % of one core", out.getvalue())

    def test_ps_is_read_along_the_way_and_its_mean_logged(self):
        calls = []

        def sample(pid):
            calls.append(pid)
            return self.clock.now * 0.05, 4.0

        cpu = self.cpu(sample, read_along=True)
        with contextlib.redirect_stdout(io.StringIO()) as out:
            percent = cpu.measure("Step 2, preview flow running (picture polled)")
        self.assertAlmostEqual(percent, 5.0)
        self.assertGreater(len(calls), 10)
        self.assertIn("mean of `ps -o %cpu` 4.00 %", out.getvalue())

    def test_the_window_is_never_under_60_s(self):
        os.environ["E2E_CPU_SECONDS"] = "5"
        try:
            self.assertEqual(self.cpu(None).seconds, 60)
            os.environ["E2E_CPU_SECONDS"] = "90"
            self.assertEqual(self.cpu(None).seconds, 90)
        finally:
            del os.environ["E2E_CPU_SECONDS"]

    def test_no_strom_pid_fails(self):
        os.remove(self.pidfile)
        with self.assertRaises(e2e.Failed) as caught:
            self.cpu(lambda pid: (0.0, None)).measure("Live")
        self.assertIn("no Strom pid to measure the CPU of", str(caught.exception))

    def test_a_strom_that_changed_during_the_window_fails(self):
        def sample(pid):
            with open(self.pidfile, "w", encoding="utf-8") as f:
                f.write("5151")
            return self.clock.now, None

        with self.assertRaises(e2e.Failed) as caught:
            self.cpu(sample).measure("Live")
        self.assertIn("Strom's pid changed from 4242", str(caught.exception))

    def test_the_report_is_labelled_as_hosted_runner_figures(self):
        cpu = self.cpu(lambda pid: (self.clock.now * 0.02, None))
        summary = os.path.join(self.dir, "summary.md")
        e2e.SUMMARY = summary
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            cpu.measure("Live, uplink with the picture polled")
            cpu.taps = 0.5
            cpu.report()
        text = out.getvalue()
        self.assertIn("Strom CPU figures for #172 (GitHub-hosted runner figures, not real hardware)", text)
        self.assertIn("Polling the picture while live (polled minus not polled), per input: 0.50 % of one core (budget 5 %)", text)
        with open(summary, encoding="utf-8") as f:
            self.assertIn("| Live, uplink with the picture polled | 2.00 |", f.read())

    def test_no_figures_no_report(self):
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            self.cpu(None).report()
        self.assertEqual(out.getvalue(), "")


class PausableApp(ScriptedApp):
    """A ScriptedApp that can be suspended: `before` is what the app said before the
    suspension (already on the reader's queue), `after` what it says once resumed;
    `state` answers the `state` command."""

    def __init__(self, before, after=(), state=None):
        super().__init__([([], {"live": True} if state is None else state)])
        for line in before:
            self.lines.put(line)
        self.after = list(after)
        self.states = []

    @contextlib.contextmanager
    def paused(self):
        self.states.append("suspended")
        try:
            yield
        finally:
            self.states.append("resumed")
            for line in self.after:
                self.lines.put(line)


SUSPENDED = "Live, uplink with the picture not polled (app suspended, 15 s after; meter still on)"


class TapsCpu(unittest.TestCase):
    def setUp(self):
        self.clock = Clock()
        self.saved = (e2e.time.monotonic, e2e.time.sleep)
        e2e.time.monotonic, e2e.time.sleep = self.clock.monotonic, self.clock.sleep

    def tearDown(self):
        e2e.time.monotonic, e2e.time.sleep = self.saved

    def check(self, polled, not_polled, before=(row("live"), level([-3.0])), after=(level([-4.0]),), state=None):
        app = PausableApp(list(before), list(after), state)

        class Timed(FakeCpu):
            def measure(inner, what):
                inner.at = getattr(inner, "at", []) + [self.clock.now]
                return FakeCpu.measure(inner, what)

        cpu = Timed([polled, not_polled])
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            cost = e2e.taps_cpu_check(app, cpu, "gw — Test pattern", 0, "Late receiver")
        return app, cpu, cost, out.getvalue()

    def test_within_budget_passes_and_the_app_is_suspended_only_for_the_second_window(self):
        app, cpu, cost, out = self.check(6.0, 4.5)
        self.assertEqual(cpu.measured, ["Live, uplink with the picture polled", SUSPENDED])
        self.assertEqual(app.states, ["suspended", "resumed"])
        self.assertEqual(cost, 1.5)
        self.assertEqual(cpu.taps, 1.5)
        self.assertIn("PASS: Late receiver: polling the picture costs 1.50 % of one core per input", out)
        self.assertIn("PASS: Late receiver: after the app was suspended: the app runs again: a new level of -4.0 dBFS", out)
        self.assertEqual(app.sent, [("state", None)])

    def test_the_not_polled_window_starts_past_the_idle_timeout(self):
        _, cpu, _, _ = self.check(6.0, 4.5)
        self.assertGreaterEqual(e2e.TAP_IDLE_SETTLE, 10 + 5)
        self.assertGreaterEqual(cpu.at[1] - cpu.at[0], e2e.TAP_IDLE_SETTLE)

    def test_over_budget_only_warns_with_the_numbers(self):
        _, cpu, cost, out = self.check(20.0, 4.0)
        self.assertEqual(cost, 16.0)
        self.assertIn("::warning title=e2e app::Late receiver: polling the picture costs 16.00 % of one core per input", out)

    def test_only_what_was_said_before_the_suspension_fails(self):
        # The `live` row and a loud level from before the suspension, nothing after.
        with self.assertRaises(e2e.Failed) as caught:
            self.check(6.0, 4.5, before=(row("live"), level([-3.0])), after=())
        self.assertIn("after the app was suspended: no new level above -60 dBFS within 10 s of the resume", str(caught.exception))

    def test_a_failed_row_after_the_resume_fails(self):
        with self.assertRaises(e2e.Failed) as caught:
            self.check(6.0, 4.5, after=(level([-4.0]), row("failed")))
        self.assertIn("an input failed after the resume", str(caught.exception))

    def test_a_failed_row_from_before_the_suspension_does_not_count(self):
        _, _, cost, _ = self.check(6.0, 4.5, before=(row("failed"), row("live")), after=(level([-4.0]),))
        self.assertEqual(cost, 1.5)

    def test_not_live_after_the_resume_fails(self):
        with self.assertRaises(e2e.Failed) as caught:
            self.check(6.0, 4.5, state={"live": False})
        self.assertIn("`state` after the resume says the app is not live", str(caught.exception))

    def test_a_failure_while_suspended_still_resumes_the_app(self):
        app = PausableApp([])

        class Broken(FakeCpu):
            def measure(self, what):
                if "suspended" in what:
                    e2e.fail("no Strom")
                return 3.0

        with contextlib.redirect_stdout(io.StringIO()), self.assertRaises(e2e.Failed):
            e2e.taps_cpu_check(app, Broken(), "gw — Test pattern", 0, "Late receiver")
        self.assertEqual(app.states, ["suspended", "resumed"])


class RealPaused(unittest.TestCase):
    """The real `App.paused`, with `suspend` faked: it suspends, then resumes whatever
    happens in the block."""

    def setUp(self):
        self.calls = []
        self.saved = e2e.suspend
        e2e.suspend = lambda pid, stop: self.calls.append((pid, stop))
        self.app = FakeApp([])
        self.app.proc = type("Proc", (), {"pid": 4242})()

    def tearDown(self):
        e2e.suspend = self.saved

    def test_it_suspends_then_resumes(self):
        with self.app.paused():
            self.assertEqual(self.calls, [(4242, True)])
        self.assertEqual(self.calls, [(4242, True), (4242, False)])

    def test_it_resumes_after_an_exception(self):
        with self.assertRaises(e2e.Failed):
            with self.app.paused():
                e2e.fail("no Strom")
        self.assertEqual(self.calls, [(4242, True), (4242, False)])

    def test_a_suspend_that_fails_resumes_nothing(self):
        def refuse(pid, stop):
            self.calls.append((pid, stop))
            if stop:
                e2e.fail("could not open the app process")

        e2e.suspend = refuse
        with self.assertRaises(e2e.Failed):
            with self.app.paused():
                self.fail("the block ran with the app not suspended")
        self.assertEqual(self.calls, [(4242, True)])


if __name__ == "__main__":
    unittest.main(verbosity=2)
