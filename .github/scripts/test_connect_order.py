#!/usr/bin/env python3
"""Unit tests of the pure parts of connect-order.py (OL-64-0, #136): the server-info
`srt` block, the relay choice, the generated passphrase, the relay source address, the
dial-in from `connect` and the refusals, the instance lookup, the two stats readings and the interval between them, the
receiving side's summary, the stall verdict and the report. No network:

    python3 -I .github/scripts/test_connect_order.py
"""

import importlib.util
import os
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
SPEC = importlib.util.spec_from_file_location("connect_order", os.path.join(HERE, "connect-order.py"))
co = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(co)


def stats(t, sent, dropped, connects=1, rtt=6.0, rate=4.3, nbytes=None, acks=0):
    return {
        "t": t, "event": "stats", "connects": connects, "connected": True,
        "packets_sent": sent, "packets_sent_dropped": dropped,
        "bytes_sent": sent * 1316 if nbytes is None else nbytes,
        "rtt_ms": rtt, "send_rate_mbps": rate, "acks_received": acks,
    }


# The server-info shape of open-live main (src/routes/status.ts) on an instance that
# takes SRT through the relay only.
SERVER_INFO = {
    "stromHost": "strom.example.com",
    "srtPorts": None,
    "srtPortState": "disabled",
    "srt": {
        "state": "ok",
        "modes": {"listener": True, "caller": True, "rendezvous": True},
        "relays": [{"name": "default", "public": "relay.example.com:9000"}],
        "inboundReachable": None,
        "ports": None,
    },
}

# A created relay source (open-live src/routes/sources.ts toApi, src/lib/srt-connect.ts).
MADE = {
    "id": "src-1",
    "address": "srt+relay://AbCdEfGhIjKlMnOpQrStUv?latency=200&passphrase=***&pbkeylen=16",
    "connect": {
        "uri": "srt://relay.example.com:9000?streamid=AbCdEfGhIjKlMnOpQrStUv",
        "host": "relay.example.com", "port": 9000, "mode": "caller", "source": "srt-relay",
        "streamid": "AbCdEfGhIjKlMnOpQrStUv", "relay": "default",
    },
}


class Relay(unittest.TestCase):
    def test_the_srt_block_is_read(self):
        srt = co.srt_info(SERVER_INFO)
        self.assertEqual(srt["state"], "ok")
        self.assertEqual(srt["relays"], [{"name": "default", "public": "relay.example.com:9000"}])
        self.assertTrue(srt["modes"]["caller"])

    def test_no_srt_block_is_an_older_open_live(self):
        self.assertIsNone(co.srt_info({"stromHost": "h", "srtPorts": [47100]}))
        with self.assertRaisesRegex(co.Failed, "too old"):
            co.pick_relay(None)

    def test_no_relay_fails_and_says_so(self):
        srt = co.srt_info({"srt": {"state": "ok", "relays": []}})
        with self.assertRaisesRegex(co.Failed, r"no SRT relay \(srt.state 'ok', relays \[\]\)"):
            co.pick_relay(srt)

    def test_malformed_relays_are_skipped(self):
        srt = co.srt_info({"srt": {"state": "ok", "relays": [{"public": "x:1"}, "y", {"name": "", "public": "z:2"}]}})
        self.assertEqual(srt["relays"], [])

    def test_the_default_relay_is_picked_else_the_first(self):
        self.assertEqual(co.pick_relay(co.srt_info(SERVER_INFO)), "default")
        two = {"srt": {"state": "ok", "relays": [{"name": "eu-north", "public": "a:1"}, {"name": "us", "public": "b:2"}]}}
        self.assertEqual(co.pick_relay(co.srt_info(two)), "eu-north")

    def test_a_generated_passphrase_meets_the_length_rules_and_is_random(self):
        a, b = co.generate_passphrase(), co.generate_passphrase()
        self.assertNotEqual(a, b)
        for p in (a, b):
            self.assertTrue(10 <= len(p.encode()) <= 79)
            self.assertTrue(p.isalnum() and p.isascii())
        with self.assertRaises(ValueError):
            co.generate_passphrase(9)
        with self.assertRaises(ValueError):
            co.generate_passphrase(80)

    def test_the_relay_address_asks_for_an_id(self):
        self.assertEqual(
            co.relay_address("default", "p" * 32),
            "srt+relay://auto?latency=200&passphrase=" + "p" * 32 + "&pbkeylen=16",
        )
        self.assertTrue(co.relay_address("eu-north", "p" * 32).startswith("srt+relay://auto@eu-north?latency=200&"))

    def test_the_caller_dials_connect(self):
        dial = co.relay_dial(MADE)
        self.assertEqual(dial["host"], "relay.example.com")
        self.assertEqual(dial["port"], 9000)
        self.assertEqual(dial["streamid"], "AbCdEfGhIjKlMnOpQrStUv")
        self.assertEqual(dial["relay"], "default")
        self.assertTrue(dial["uri"].startswith("srt://relay.example.com:9000?streamid="))

    def test_a_null_uri_reports_the_reason(self):
        made = {"id": "src-1", "connect": {"uri": None, "host": None, "port": None, "mode": "caller",
                                           "source": "srt-relay", "streamid": "x" * 22, "relay": "default",
                                           "reason": "srt-relay-unreachable"}}
        with self.assertRaisesRegex(co.Failed, "connect.reason srt-relay-unreachable"):
            co.relay_dial(made)
        with self.assertRaisesRegex(co.Failed, "no `connect`"):
            co.relay_dial({"id": "src-1"})

    def test_a_422_names_the_code_and_the_relays(self):
        body = {"error": "...", "statusCode": 422, "code": "srt-relay-unknown", "relays": ["eu-north", "us"]}
        self.assertEqual(co.refusal(422, body), "POST /api/v1/sources failed with HTTP 422: code srt-relay-unknown, relays [eu-north, us]")
        body = {"error": "...", "statusCode": 422, "code": "srt-relay-not-configured", "relays": []}
        self.assertIn("code srt-relay-not-configured, relays []", co.refusal(422, body))
        self.assertEqual(co.refusal(400, {"error": "bad"}), "POST /api/v1/sources failed with HTTP 400")
        self.assertEqual(co.refusal(422, None), "POST /api/v1/sources failed with HTTP 422")


class Pure(unittest.TestCase):
    def test_the_instance_is_found_by_name(self):
        instances = [{"name": "other", "url": "https://a.example.com"}, {"name": "openlivedev", "url": "https://b.example.com"}]
        self.assertEqual(co.find_instance(instances, "openlivedev"), "https://b.example.com")
        self.assertIsNone(co.find_instance(instances, "missing"))
        self.assertIsNone(co.find_instance({"not": "a list"}, "openlivedev"))

    def test_decoding_reads_the_inputs_resolution(self):
        self.assertEqual(co.decoding([{"width": 1280, "height": 720}]), (1280, 720))
        self.assertIsNone(co.decoding([None]))
        self.assertIsNone(co.decoding([]))
        self.assertIsNone(co.decoding(None))

    def test_the_readings_are_1_s_after_the_connect_and_the_interval_apart(self):
        lines = [stats(1000 + i * 1000, i * 400, 0) for i in range(0, 70)]
        first, second = co.pick_readings(lines, 1500, 60)
        self.assertEqual(first["t"], 3000)
        self.assertEqual(second["t"], 63000)
        self.assertEqual(co.pick_readings(lines[:30], 1500, 60), (first, None))
        self.assertEqual(co.pick_readings([], 1500, 60), (None, None))

    def test_the_interval_gives_the_drop_share_and_the_rate(self):
        a = co.reading(stats(0, 1000, 10))
        b = co.reading(stats(60000, 25000, 250))
        gap = co.interval(a, b)
        self.assertEqual(gap["packets_sent"], 24000)
        self.assertEqual(gap["packets_sent_dropped"], 240)
        self.assertAlmostEqual(gap["drop_share"], 0.01)
        self.assertAlmostEqual(gap["send_rate_mbps"], 24000 * 1316 * 8 / 60 / 1e6)
        self.assertAlmostEqual(a["drop_share"], 0.01)

    def test_a_redial_between_the_readings_gives_no_interval(self):
        a = co.reading(stats(0, 1000, 0, connects=1))
        b = co.reading(stats(60000, 500, 0, connects=2))
        self.assertIsNone(co.interval(a, b))

    def test_the_decode_summary_times_the_first_frame(self):
        res = (1280, 720)
        polls = [(0, "activating", None, False), (2000, "active", None, True), (4000, "active", res, True),
                 (6000, "active", None, True), (8000, "active", None, True), (10000, "active", res, True)]
        d = co.decode_summary(polls, active_at=2000, connected_at=3000, end_at=10000)
        self.assertTrue(d["reported"])
        self.assertEqual(d["first_seen_after_active_s"], 2.0)
        self.assertEqual(d["first_seen_after_connect_s"], 1.0)
        self.assertEqual(d["resolution"], "1280x720")
        self.assertAlmostEqual(d["decoding_share_after_connect"], 2 / 4)
        self.assertEqual(d["longest_gap_after_first_s"], 4.0)

    def test_a_clean_run_is_no_stall(self):
        gap = {"drop_share": 0.0}
        d = {"reported": True, "resolution": "1280x720"}
        self.assertEqual(co.verdict(gap, d, 0), (False, [], False))

    def test_strom_1277s_signature_is_a_stall(self):
        gap = {"drop_share": 0.98}
        d = {"reported": True, "resolution": None}
        stalled, reasons, over = co.verdict(gap, d, 0)
        self.assertTrue(stalled)
        self.assertTrue(over)
        self.assertEqual(len(reasons), 2)

    def test_drops_over_1_percent_are_flagged_without_a_stall(self):
        stalled, _, over = co.verdict({"drop_share": 0.02}, {"reported": True, "resolution": "1280x720"}, 0)
        self.assertFalse(stalled)
        self.assertTrue(over)

    def test_no_resolution_field_is_not_a_stall_by_itself(self):
        stalled, _, _ = co.verdict({"drop_share": 0.0}, {"reported": False, "resolution": None}, 0)
        self.assertFalse(stalled)


class Report(unittest.TestCase):
    def result(self, run, stalled, over=False):
        r1 = co.reading(stats(0, 400, 0))
        r2 = co.reading(stats(60000, 24000, 23000 if stalled else 0))
        return {
            "run": run, "order": co.RUNS[run], "activation_s": 12.0, "connect_after_active_s": 1.0,
            "lead_s": 30.0, "connected_before_activation": False, "connects": 1, "broken": 0,
            "reading_1": r1, "reading_2": r2, "interval": co.interval(r1, r2),
            "decode": {"reported": True, "resolution": None if stalled else "1280x720", "polls": 30, "polls_reporting": 28},
            "stalled": stalled, "reasons": ["x"] if stalled else [], "over_drop_limit": over or stalled,
        }

    def test_the_comment_names_the_relay_path_and_first_decoded_at(self):
        meta = {"instance": "openlivedev", "seconds": 60, "sha": "abc123", "cleanup": "done", "relay": "default"}
        r = self.result("A", False)
        r["decode"].update({"first_seen_after_active_s": 2.0, "first_seen_after_connect_s": 1.0})
        text = co.comment([r], meta)
        self.assertIn("through Open Live's SRT relay `default`", text)
        self.assertIn("`srt+relay://auto`", text)
        self.assertIn("first decoded at 1280x720", text)
        self.assertNotIn("relay.example.com", text)

    def test_the_comment_cites_the_gate_only_when_caller_first_stalls(self):
        meta = {"instance": "openlivedev", "seconds": 60, "sha": "abc123", "cleanup": "done"}
        clean = co.comment([self.result("A", False), self.result("B", False)], meta)
        self.assertIn("**Does caller-first stall?** No", clean)
        self.assertNotIn("gate::Verdict::send", clean)
        self.assertIn("No run dropped more than 1 %", clean)
        stalled = co.comment([self.result("A", False), self.result("B", True)], meta)
        self.assertIn("**Does caller-first stall?** Yes", stalled)
        self.assertIn("gate::Verdict::send", stalled)
        self.assertIn("blob/abc123/src/gate.rs", stalled)
        self.assertIn("run(s) B", stalled)

    def test_a_failed_run_is_reported(self):
        meta = {"instance": "openlivedev", "seconds": 60, "cleanup": "done"}
        text = co.comment([{"run": "B", "order": "caller first", "error": "the caller did not connect within 120 s"}], meta)
        self.assertIn("Not answered", text)
        self.assertIn("did not connect", text)


if __name__ == "__main__":
    unittest.main()
