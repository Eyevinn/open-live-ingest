#!/usr/bin/env python3
"""Unit tests of the uplink SRT stats check in e2e-app.py (#112), and of the stand-in
Open Live the no-secret late-listener check sends to. No app and no network beyond
loopback:

    python3 -I .github/scripts/test_e2e_app.py
"""

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

    def test_unknown_routes_are_404(self):
        self.assertEqual(self.call("GET", "/api/v1/productions")[0], 404)


if __name__ == "__main__":
    unittest.main(verbosity=2)
