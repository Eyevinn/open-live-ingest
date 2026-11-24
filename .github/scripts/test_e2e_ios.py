#!/usr/bin/env python3
"""Unit tests of the pure parts of e2e-ios.py (OL-64h, #144): the simulator pick, the
run's sources and stale names, the relay uplink line against Open Live's source, the SRT counters over the window, the peak-to-average
bitrate, the cloud side's picture and sound, the WebSocket frames, the control server's
answers and the report. No network beyond loopback, no simulator:

    python3 -I .github/scripts/test_e2e_ios.py
"""

import importlib.util
import json
import os
import unittest
import urllib.parse
import urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
SPEC = importlib.util.spec_from_file_location("e2e_ios", os.path.join(HERE, "e2e-ios.py"))
e2e = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(e2e)

LABEL = "oli-ios-e2e-100-1"


STREAM_ID = "AbCdEfGhIjKlMnOpQrStUv"
DIAL_IN = f"srt://relay.example.com:6000?streamid={STREAM_ID}"
UPLINK = f"Uplink: via relay default to {DIAL_IN}&passphrase=***"
# An Open Live source through the relay, as src/openlive.rs's tests have it.
RELAYED = {
    "_id": "s1", "name": f"iPhone-ab12 — {LABEL}", "status": "active",
    "address": f"srt+relay://{STREAM_ID}?latency=200&passphrase=s3cret-s3cret&pbkeylen=16",
    "connect": {"uri": DIAL_IN, "host": "relay.example.com", "port": 6000, "mode": "caller",
                "source": "srt-relay", "streamid": STREAM_ID, "relay": "default"},
}


def sample(t, sent, dropped=0, nbytes=None):
    return {"t": t, "sent": sent, "dropped": dropped, "bytes": sent * 1316 if nbytes is None else nbytes}


class Pure(unittest.TestCase):
    def test_the_newest_iphone_simulator_is_picked(self):
        listing = {"devices": {
            "com.apple.CoreSimulator.SimRuntime.iOS-17-5": [
                {"name": "iPhone 15", "udid": "A", "isAvailable": True}],
            "com.apple.CoreSimulator.SimRuntime.iOS-18-2": [
                {"name": "iPad Air", "udid": "B", "isAvailable": True},
                {"name": "iPhone 16", "udid": "C", "isAvailable": True},
                {"name": "iPhone 16 Pro", "udid": "D", "isAvailable": False}],
            "com.apple.CoreSimulator.SimRuntime.watchOS-11-0": [
                {"name": "iPhone-ish", "udid": "E", "isAvailable": True}],
        }}
        self.assertEqual(e2e.pick_simulator(listing), ("C", "iOS 18.2, iPhone 16"))
        self.assertIsNone(e2e.pick_simulator({"devices": {}}))

    def test_the_instance_and_the_listener_port(self):
        instances = [{"name": "other", "url": "https://a.example.com"}, {"name": "openlivedev", "url": "https://b.example.com"}]
        self.assertEqual(e2e.find_instance(instances, "openlivedev"), "https://b.example.com")
        self.assertIsNone(e2e.find_instance(instances, "missing"))
        self.assertEqual(e2e.listener_port("srt://:47100?mode=listener"), 47100)
        self.assertIsNone(e2e.listener_port("srt://:0?mode=listener"))
        self.assertIsNone(e2e.listener_port("srt://host.example.com:47100"))

    def test_the_runs_sources_and_the_stale_names(self):
        sources = [
            {"_id": "s1", "name": f"iPhone-ab12 — {LABEL}", "status": "inactive", "address": "srt://:47101?mode=listener"},
            {"id": "s2", "name": "iPhone-ab12 — oli-ios-e2e-99-1", "status": "active"},
            {"id": "s3", "name": "Venue — Camera 1", "status": "active"},
            {"id": "s4", "name": "iPhone-ab12 — oli-ios-e2e-100-12", "status": "active"},
        ]
        self.assertEqual(e2e.ours(sources, LABEL),
                         [{"id": "s1", "name": f"iPhone-ab12 — {LABEL}", "status": "inactive", "port": 47101,
                           "relay_address": False, "relay": None, "streamid": None, "dial_in": None}])
        self.assertFalse(e2e.stale(f"iPhone-ab12 — {LABEL}", LABEL))
        self.assertFalse(e2e.stale(f"{LABEL} — production", LABEL))
        self.assertTrue(e2e.stale("iPhone-ab12 — oli-ios-e2e-99-1", LABEL))
        self.assertTrue(e2e.stale("oli-ios-e2e-99-1 — production", LABEL))
        self.assertTrue(e2e.stale("iPhone-ab12 — oli-ios-e2e-100-12", LABEL))
        self.assertFalse(e2e.stale("Venue — Camera 1", LABEL))
        self.assertFalse(e2e.stale("oli-app-e2e-1-1-mac — Test pattern", LABEL))

    def test_a_relay_source_keeps_its_connect_block_and_never_its_address(self):
        found = e2e.ours([RELAYED], LABEL)
        self.assertEqual(found, [{"id": "s1", "name": f"iPhone-ab12 — {LABEL}", "status": "active", "port": None,
                                  "relay_address": True, "relay": "default", "streamid": STREAM_ID,
                                  "dial_in": DIAL_IN}])
        self.assertNotIn("s3cret", json.dumps(found))

    def test_the_uplink_line_is_parsed(self):
        self.assertEqual(e2e.uplink_route(UPLINK), {
            "relay": "default", "dial_in": DIAL_IN, "streamid": STREAM_ID, "encrypted": True})
        self.assertEqual(e2e.uplink_route("Uplink: via relay eu-north"),
                         {"relay": "eu-north", "dial_in": None, "streamid": None, "encrypted": False})
        self.assertEqual(e2e.uplink_route("Uplink: via relay default to srt://relay.example.com:6000")["streamid"], None)
        for line in ("", None, "Link: connected", "Uplink: to srt://relay.example.com:6000?streamid=x"):
            self.assertIsNone(e2e.uplink_route(line))
        self.assertEqual(e2e.short_id(STREAM_ID), "`AbCd…` (22 chars)")
        self.assertEqual(e2e.short_id(None), "none")

    def test_the_relay_verdict_holds_the_line_against_open_live(self):
        sources = e2e.ours([RELAYED], LABEL)
        ok, detail, sid = e2e.relay_verdict(UPLINK, sources)
        self.assertTrue(ok, detail)
        self.assertEqual(sid, STREAM_ID)
        self.assertIn("'Uplink: via relay default', stream id `AbCd…` (22 chars), encrypted", detail)
        # The report shows no host and not the whole stream id.
        self.assertNotIn("relay.example.com", detail)
        self.assertNotIn(STREAM_ID, detail)
        # A reconnect with the same stream id passes; another one fails.
        ok, detail, _ = e2e.relay_verdict(UPLINK, sources, first=STREAM_ID)
        self.assertTrue(ok, detail)
        self.assertIn("the same stream id as at the first connect", detail)
        ok, detail, _ = e2e.relay_verdict(UPLINK, sources, first="ZyXwVuTsRqPoNmLkJiHgFe")
        self.assertFalse(ok)
        self.assertIn("the stream id changed since the first connect (`ZyXw…` (22 chars))", detail)

    def test_anything_short_of_the_relay_fails(self):
        sources = e2e.ours([RELAYED], LABEL)
        ok, detail, sid = e2e.relay_verdict("", sources)
        self.assertEqual((ok, sid), (False, None))
        self.assertIn("no relay uplink line (it says: 'nothing')", detail)
        ok, detail, _ = e2e.relay_verdict("Uplink: via relay default", sources)
        self.assertFalse(ok)
        self.assertIn("carries no stream id", detail)
        ok, detail, _ = e2e.relay_verdict(UPLINK.replace(STREAM_ID, "ZyXwVuTsRqPoNmLkJiHgFe"), sources)
        self.assertFalse(ok)
        self.assertIn("Open Live's stream id for the source is `AbCd…` (22 chars) (connect.streamid)", detail)
        self.assertIn("differs from Open Live's connect.uri", detail)
        ok, detail, _ = e2e.relay_verdict(UPLINK.replace("relay default", "relay eu"), sources)
        self.assertFalse(ok)
        self.assertIn("Open Live reports relay 'default'", detail)
        # A listener source (no relay in Open Live) or no source at all.
        listener = e2e.ours([{"_id": "s1", "name": f"iPhone — {LABEL}", "address": "srt://:47101"}], LABEL)
        ok, detail, _ = e2e.relay_verdict(UPLINK, listener)
        self.assertFalse(ok)
        self.assertIn("not an srt+relay:// one", detail)
        self.assertIn("Open Live reports relay 'None'", detail)
        ok, detail, _ = e2e.relay_verdict(UPLINK, [])
        self.assertFalse(ok)
        self.assertIn("Open Live lists 0 sources of the run, not one", detail)

    def test_the_drop_share_is_taken_between_the_two_readings(self):
        samples = [{"t": 0, "sent": None, "dropped": None, "bytes": None}, sample(1, 0), sample(2, 1000, 5)]
        samples += [sample(2 + i, 1000 + 380 * i, 5 + (i // 20)) for i in range(1, 65)]
        gap = e2e.interval(samples, 60)
        self.assertEqual(gap["first"]["t"], 2)
        self.assertEqual(gap["second"]["t"], 62)
        self.assertEqual(gap["packets_sent"], 380 * 60)
        self.assertEqual(gap["packets_sent_dropped"], 3)
        self.assertEqual(gap["problems"], [])

    def test_too_many_drops_a_reconnect_or_a_short_window_fail(self):
        lossy = [sample(0, 100, 0), sample(30, 5000, 20), sample(61, 10100, 101)]
        gap = e2e.interval(lossy, 60)
        self.assertAlmostEqual(gap["drop_share"], 0.0101, places=4)
        self.assertIn("or more", gap["problems"][0])
        back = [sample(0, 100), sample(30, 5000), sample(31, 10), sample(61, 9000)]
        self.assertIn("went back", e2e.interval(back, 60)["problems"][0])
        short = [sample(0, 100), sample(40, 5000)]
        self.assertIn("no second reading", e2e.interval(short, 60)["problems"][0])
        self.assertIn("never showed", e2e.interval([sample(0, 0)], 60)["problems"][0])
        still = [sample(0, 100), sample(61, 100)]
        self.assertIn("no packets", e2e.interval(still, 60)["problems"][0])

    def test_peak_to_average_counts_only_the_moves_of_the_counter(self):
        # Reports every 2 s at 4000 kbps, one of them a 6000 kbps burst; sampled twice as often.
        samples, total = [], 0
        for i in range(31):
            rate = 6000 if i == 10 else 4000
            if i:
                total += rate * 1000 * 2 // 8
            samples.append({"t": 2 * i, "sent": i, "dropped": 0, "bytes": total})
            samples.append({"t": 2 * i + 1, "sent": i, "dropped": 0, "bytes": total})
        avg, peak, ratio = e2e.peak_to_average(samples)
        self.assertAlmostEqual(peak, 6000)
        self.assertAlmostEqual(avg, (4000 * 29 + 6000) / 30)
        self.assertAlmostEqual(ratio, 6000 / avg)
        self.assertEqual(e2e.peak_to_average([]), (None, None, None))
        self.assertEqual(e2e.peak_to_average([{"t": 0, "bytes": 10}, {"t": 2, "bytes": 5}, {"t": 4, "bytes": 9}]),
                         (None, None, None))

    def test_the_cloud_picture_and_sound(self):
        self.assertEqual(e2e.decoding([{"width": 1280, "height": 720}]), (1280, 720))
        self.assertIsNone(e2e.decoding([None]))
        self.assertIsNone(e2e.decoding(None))
        polls = [(0, "activating", None), (5, "active", None), (7, "active", (1280, 720)), (9, "active", (1280, 720))]
        d = e2e.decode_summary(polls, 4, 10)
        self.assertEqual((d["looks"], d["decoding"], d["resolution"]), (3, 2, "1280x720"))
        self.assertIsNone(e2e.decode_summary(polls, 0, 6)["resolution"])

        self.assertEqual(e2e.meter_peak([-20.5, -21.0]), -20.5)
        self.assertEqual(e2e.meter_peak(-30), -30.0)
        self.assertIsNone(e2e.meter_peak("loud"))
        self.assertEqual(e2e.audio_verdict([(1, "ch1", -20.1), (2, "main", -10.0)])[0], "sound")
        self.assertEqual(e2e.audio_verdict([(1, "ch1", -90.0), (2, "ch1", -120.0)])[0], "silent")
        self.assertEqual(e2e.audio_verdict([(1, "main", -20.0)])[0], "unreported")
        self.assertEqual(e2e.audio_verdict([])[0], "unreported")

    def test_anything_short_of_sound_fails_the_phase(self):
        self.assertIsNone(e2e.sound_check("sound", "ch1 loud", None, 40))
        # No channel meter arrived: sound is unverified, which fails AC1 (not a warning).
        problem = e2e.sound_check(*e2e.audio_verdict([]), None, 40)
        self.assertIn("sound not verified", problem)
        self.assertIn("no channel meter arrived", problem)
        self.assertIn("no sound", e2e.sound_check("silent", "loudest -90.0 dB", None, 40))
        # The meter socket failed (a 401/403 on the upgrade, TLS, a drop): even sound seen
        # before the failure does not pass.
        for error in ("the meter socket was refused: 403", "SSLError", "closed by Open Live"):
            self.assertIn(error, e2e.sound_check("sound", "ch1 loud", error, 40))
        self.assertIn("no frame", e2e.sound_check("unreported", "no channel meter arrived", None, 0))
        self.assertIn("never opened", e2e.sound_check("unreported", "no channel meter arrived", None, None))

    def test_all_checks_passed_only_when_every_phase_heard_sound(self):
        checks = [{"id": "AC2", "ok": True}]
        heard = {"first": {"ok": True, "audio": ("sound", "ch1")}}
        self.assertTrue(e2e.all_passed(0, checks, heard))
        self.assertFalse(e2e.all_passed(1, checks, heard))
        self.assertFalse(e2e.all_passed(0, [{"id": "AC3", "ok": False}], heard))
        self.assertFalse(e2e.all_passed(0, checks, {}))
        self.assertFalse(e2e.all_passed(0, checks, {"first": {"ok": True, "audio": ("unreported", "none")}}))
        self.assertFalse(e2e.all_passed(0, checks, {"first": {"ok": True}}))
        self.assertFalse(e2e.all_passed(0, checks, {"first": {"ok": False, "audio": ("sound", "ch1")}}))

    def test_websocket_frames_round_trip(self):
        for size in (0, 5, 200, 70000):
            payload = bytes(range(256)) * (size // 256) + bytes(range(size % 256))
            frame = e2e.ws_frame(0x1, payload, b"\x01\x02\x03\x04")
            self.assertEqual(frame[1] & 0x80, 0x80, "a client frame is masked")
            fin, opcode, got, rest = e2e.ws_parse(frame + b"\x89")
            self.assertEqual((fin, opcode, got, rest), (True, 0x1, payload, b"\x89"))
            self.assertIsNone(e2e.ws_parse(frame[:-1]))
        # An unmasked server frame.
        self.assertEqual(e2e.ws_parse(b"\x81\x02hi"), (True, 0x1, b"hi", b""))
        # RFC 6455's example key.
        self.assertEqual(e2e.ws_accept("dGhlIHNhbXBsZSBub25jZQ=="), "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=")

    def test_the_query_of_a_control_request(self):
        self.assertEqual(e2e.query("/check?id=AC2%20armed&ok=1&detail=a%26b"),
                         ("/check", {"id": "AC2 armed", "ok": "1", "detail": "a&b"}))
        self.assertEqual(e2e.query("/config"), ("/config", {}))


class FakeOpenLive:
    url = "https://open-live.example.com"

    def sources(self):
        return [{"_id": "s1", "name": f"iPhone — {LABEL}", "status": "inactive", "address": "srt://:47101"}]


class Control(unittest.TestCase):
    """The control server over loopback, as the XCUITest calls it (HTTP/1.1 200 with a JSON
    object, FakeControl.swift), with no Open Live behind it."""

    def setUp(self):
        self.session = e2e.Session(FakeOpenLive(), LABEL, "UDID", "secret-token", 60, "openlivedev")
        self.server = e2e.serve(self.session)
        self.base = f"http://127.0.0.1:{self.server.server_address[1]}"

    def tearDown(self):
        self.server.shutdown()
        self.server.server_close()

    def call(self, method, path):
        request = urllib.request.Request(self.base + path, method=method, data=b"" if method == "POST" else None)
        try:
            with urllib.request.urlopen(request, timeout=5) as response:
                return response.status, response.version, json.loads(response.read())
        except urllib.error.HTTPError as err:
            return err.code, None, json.loads(err.read())

    def test_config_and_answers_are_http_1_1_json(self):
        status, version, body = self.call("GET", "/config")
        self.assertEqual((status, version), (200, 11))
        self.assertEqual(body["label"], LABEL)
        self.assertEqual(body["seconds"], 60)
        self.assertNotIn("secret-token", json.dumps(body))

    def test_samples_and_checks_are_recorded(self):
        self.call("POST", "/measure/start?phase=first")
        for sent in (100, 200):
            self.call("POST", f"/sample?phase=first&sent={sent}&dropped=0&bytes={sent * 1316}")
        self.call("POST", "/sample?phase=first&sent=&dropped=&bytes=")
        self.assertEqual([s["sent"] for s in self.session.samples["first"]], [100, 200, None])
        self.call("POST", "/check?id=AC2%20armed&ok=1&detail=no%20counters")
        self.assertEqual(self.session.checks, [{"id": "AC2 armed", "ok": True, "detail": "no counters"}])
        status, _, body = self.call("POST", "/measure/stop?phase=first")
        self.assertEqual(status, 200)
        self.assertFalse(body["ok"])
        self.assertTrue(any("second reading" in p for p in body["problems"]))
        self.assertTrue(any("picture" in p for p in body["problems"]))
        # No meter socket was ever opened: sound is unverified and fails the phase.
        self.assertTrue(any("sound not verified" in p for p in body["problems"]))
        self.assertEqual(body["audio"], "unverified")

    def test_unreported_sound_or_a_failed_meter_socket_fail_the_phase(self):
        self.call("POST", "/measure/start?phase=first")
        for t, sent in ((0, 100), (61, 23100)):
            self.session.samples["first"].append(sample(self.session.windows["first"] + t * 0.0001, sent))
        self.session.polls.append((self.session.windows["first"], "active", (1280, 720)))
        watch = e2e.MeterWatch("https://open-live.example.com", "p1", {})
        watch.frames = 12  # frames came, but no channel meter among them
        self.session.watch = watch
        self.session.seconds = 0.005
        status, _, body = self.call("POST", "/measure/stop?phase=first")
        self.assertEqual(status, 200)
        self.assertFalse(body["ok"])
        self.assertEqual(body["audio"], "unverified")
        self.assertEqual([p for p in body["problems"] if "sound" in p],
                         ["sound not verified: no channel meter arrived on the watch-only controller socket"])

        watch.meters.append((self.session.windows["first"], "ch1", -20.0))
        _, _, body = self.call("POST", "/measure/stop?phase=first")
        self.assertTrue(body["ok"], body["problems"])
        self.assertEqual(body["audio"], "sound")

        watch.error = "the meter socket was refused: 403"
        _, _, body = self.call("POST", "/measure/stop?phase=first")
        self.assertFalse(body["ok"])
        self.assertTrue(any("meter socket failed (the meter socket was refused: 403)" in p for p in body["problems"]))
        self.assertFalse(e2e.all_passed(0, [], self.session.phases))

    def test_config_says_whether_the_relay_is_required(self):
        _, _, body = self.call("GET", "/config")
        self.assertIs(body["relay"], True)

    def test_the_uplink_line_is_judged_and_its_stream_id_kept(self):
        line = urllib.parse.quote(UPLINK, safe="")
        status, _, body = self.call("POST", f"/uplink?phase=first&line={line}")
        self.assertEqual(status, 200)
        self.assertFalse(body["ok"], "no source of the run seen yet")
        self.assertIsNone(self.session.first_streamid)
        self.session.sources = e2e.ours([RELAYED], LABEL)
        _, _, body = self.call("POST", f"/uplink?phase=first&line={line}")
        self.assertTrue(body["ok"], body["detail"])
        self.assertEqual(self.session.first_streamid, STREAM_ID)
        other = urllib.parse.quote(UPLINK.replace(STREAM_ID, "ZyXwVuTsRqPoNmLkJiHgFe"), safe="")
        self.session.sources[0]["streamid"] = "ZyXwVuTsRqPoNmLkJiHgFe"
        self.session.sources[0]["dial_in"] = DIAL_IN.replace(STREAM_ID, "ZyXwVuTsRqPoNmLkJiHgFe")
        _, _, body = self.call("POST", f"/uplink?phase=foreground&line={other}")
        self.assertFalse(body["ok"])
        self.assertIn("changed since the first connect", body["detail"])
        self.assertEqual(self.session.first_streamid, STREAM_ID)
        _, _, body = self.call("POST", "/uplink?phase=foreground")
        self.assertFalse(body["ok"])
        self.assertIn("no relay uplink line", body["detail"])

    def test_the_foreground_picture_counts_only_looks_after_the_window(self):
        self.session.polls.append((0.0, "active", (1280, 720)))  # from before the background
        status, _, body = self.call("GET", "/picture?phase=foreground")
        self.assertEqual(status, 500)
        self.call("POST", "/measure/start?phase=foreground")
        _, _, body = self.call("GET", "/picture?phase=foreground")
        self.assertIsNone(body["resolution"])
        self.session.polls.append((self.session.windows["foreground"], "active", (1920, 1080)))
        _, _, body = self.call("GET", "/picture?phase=foreground")
        self.assertEqual(body["resolution"], "1920x1080")

    def test_an_unknown_request_or_a_bad_activate_is_a_500_with_the_reason(self):
        status, _, body = self.call("GET", "/nope")
        self.assertEqual(status, 500)
        self.assertIn("no such control request", body["error"])
        status, _, body = self.call("POST", "/production/activate")
        self.assertEqual(status, 500)
        self.assertIn("source", body["error"])


class Report(unittest.TestCase):
    def test_the_report_and_the_comment(self):
        gap = e2e.interval([sample(0, 100, 0), sample(61, 23100, 4)], 60)
        phases = {"first": {"interval": gap, "bitrate": (4100.0, 5300.0, 5300 / 4100),
                            "decode": {"looks": 30, "decoding": 28, "resolution": "1280x720"},
                            "audio": ("sound", "30 meter readings on ch1, loudest -20.0 dB"), "ok": True}}
        checks = [{"id": "AC2 armed", "ok": True, "detail": "nothing sent for 10 s"},
                  {"id": "AC3 reuse", "ok": False, "detail": "a new id"}]
        meta = {"instance": "openlivedev", "label": LABEL, "seconds": 60, "sha": "0123456789abcdef"}
        text = e2e.report(meta, checks, phases, "the test production and source were deactivated and deleted")
        self.assertIn("| pass | AC2 armed |", text)
        self.assertIn("| FAIL | AC3 reuse |", text)
        self.assertIn("23,000 packets sent, 4 dropped (**0.017%**)", text)
        self.assertIn("**1.29**", text)
        self.assertIn("1280x720, in 28 of 30 looks", text)
        draft = e2e.comment(meta, checks, phases, "deleted", False)
        self.assertTrue(draft.startswith("<!-- ol-64h ios simulator e2e results -->"))
        self.assertIn("**FAILED**", draft)


if __name__ == "__main__":
    unittest.main()
