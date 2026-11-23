import XCTest
@testable import OpenLiveIngest

/// Every command of docs/specs/ios-app.md, section 8.2, encoded as the core reads it, and every
/// reply and event decoded as the core sends it (mobile/src/core.rs).
final class CoreMessagesTests: XCTestCase {
    /// `text` as a JSON value, to compare without caring for key order.
    private func json(_ text: String, file: StaticString = #filePath, line: UInt = #line) throws -> JSONValue {
        try JSONDecoder().decode(JSONValue.self, from: Data(text.utf8))
    }

    private func assertEncodes(
        _ command: CoreCommand, id: UInt32 = 7, as expected: String, file: StaticString = #filePath, line: UInt = #line
    ) throws {
        XCTAssertEqual(try json(command.json(id: id)), try json(expected), command.name, file: file, line: line)
    }

    func testEveryCommandEncodesInTheEnvelope() throws {
        try assertEncodes(.state, as: #"{"id": 7, "cmd": "state"}"#)
        try assertEncodes(.oscSignIn, as: #"{"id": 7, "cmd": "osc_sign_in"}"#)
        try assertEncodes(.oscSignInCancel, as: #"{"id": 7, "cmd": "osc_sign_in_cancel"}"#)
        try assertEncodes(
            .oscPasteToken(token: "pat-1"), as: #"{"id": 7, "cmd": "osc_paste_token", "args": {"token": "pat-1"}}"#)
        try assertEncodes(
            .chooseInstance(url: "https://a.example"),
            as: #"{"id": 7, "cmd": "choose_instance", "args": {"url": "https://a.example"}}"#)
        try assertEncodes(
            .selfHosted(url: "https://ol.example", apiKey: "key"),
            as: #"{"id": 7, "cmd": "self_hosted", "args": {"url": "https://ol.example", "api_key": "key"}}"#)
        try assertEncodes(
            .selfHosted(url: "https://ol.example", apiKey: nil),
            as: #"{"id": 7, "cmd": "self_hosted", "args": {"url": "https://ol.example"}}"#)
        try assertEncodes(
            .setInput(device: "cam-1", audio: "mic-1", label: "Back camera"),
            as: #"{"id": 7, "cmd": "set_input", "args": {"device": "cam-1", "audio": "mic-1", "label": "Back camera"}}"#)
        try assertEncodes(
            .setInput(device: "cam-1", audio: nil, label: nil),
            as: #"{"id": 7, "cmd": "set_input", "args": {"device": "cam-1"}}"#)
        try assertEncodes(
            .setAdvanced(
                AdvancedValues(
                    name: "Venue", latencyMs: 300, passphrase: "pass-phrase-1", pbkeylen: 32, bitrateKbps: 3000,
                    resolution: "1920x1080", framerate: "25/1", logLevel: "debug")),
            as: #"""
                {"id": 7, "cmd": "set_advanced", "args": {"name": "Venue", "latency_ms": 300,
                 "passphrase": "pass-phrase-1", "pbkeylen": 32, "bitrate_kbps": 3000,
                 "resolution": "1920x1080", "framerate": "25/1", "log_level": "debug"}}
                """#)
        try assertEncodes(
            .setAdvanced(AdvancedValues(latencyMs: 120)),
            as: #"{"id": 7, "cmd": "set_advanced", "args": {"latency_ms": 120}}"#)
        try assertEncodes(.goLive, as: #"{"id": 7, "cmd": "go_live"}"#)
        try assertEncodes(.stop, as: #"{"id": 7, "cmd": "stop"}"#)
        try assertEncodes(
            .mediaReport(
                MediaReport(
                    input: "in-1", state: .running, reason: nil,
                    stats: SRTSenderStats(
                        bytesSent: 1316, packetsSent: 10, packetsSentDropped: 1, rttMs: 31.5, sendRateMbps: 4.1))),
            as: #"""
                {"id": 7, "cmd": "media_report", "args": {"input": "in-1", "state": "running",
                 "stats": {"bytes_sent": 1316, "packets_sent": 10, "packets_sent_dropped": 1,
                           "rtt_ms": 31.5, "send_rate_mbps": 4.1}}}
                """#)
        try assertEncodes(
            .mediaReport(MediaReport(input: "in-1", state: .failed, reason: "no camera", stats: nil)),
            as: #"{"id": 7, "cmd": "media_report", "args": {"input": "in-1", "state": "failed", "reason": "no camera"}}"#)
        try assertEncodes(
            .mediaReport(MediaReport(input: "in-1", state: .starting)),
            as: #"{"id": 7, "cmd": "media_report", "args": {"input": "in-1", "state": "starting"}}"#)
        try assertEncodes(
            .mediaReport(MediaReport(input: "in-1", state: .stopped)),
            as: #"{"id": 7, "cmd": "media_report", "args": {"input": "in-1", "state": "stopped"}}"#)
        for (network, word) in [
            (NetworkKind.wifi, "wifi"), (.cellular, "cellular"), (.wired, "wired"), (.other, "other"), (.noNetwork, "none"),
        ] {
            try assertEncodes(
                .phone(PhoneReport(foreground: true, interrupted: false, network: network)),
                as: #"{"id": 7, "cmd": "phone", "args": {"foreground": true, "interrupted": false, "network": "\#(word)"}}"#)
        }
        try assertEncodes(.forgetSignIn, as: #"{"id": 7, "cmd": "forget_sign_in"}"#)
    }

    func testCommandsDescribeThemselvesWithoutTheirSecrets() {
        let commands: [CoreCommand] = [
            .oscPasteToken(token: "secret-token"),
            .selfHosted(url: "https://ol.example", apiKey: "secret-key"),
            .setAdvanced(AdvancedValues(passphrase: "secret-passphrase")),
        ]
        for command in commands {
            for text in [String(describing: command), String(reflecting: command), "\(command)"] {
                XCTAssertFalse(text.contains("secret"), text)
            }
        }
    }

    func testRepliesDecode() throws {
        let ok = try CoreMessage.parse(#"{"id": 3, "ok": null}"#)
        XCTAssertEqual(ok.id, 3)
        XCTAssertNil(ok.error)
        XCTAssertNil(ok.event)

        let error = try CoreMessage.parse(#"{"id": 4, "error": {"message": "Press Stop first.", "action": null}}"#)
        XCTAssertEqual(error.error, CoreError(message: "Press Stop first.", action: nil))

        let unreadable = try CoreMessage.parse(#"{"id": null, "error": {"message": "x", "action": null}}"#)
        XCTAssertNil(unreadable.id)

        let model = try XCTUnwrap(
            CoreMessage.parse(Self.viewModel).ok
        ).decode(ViewModel.self)
        XCTAssertEqual(model.version, "0.6.0")
        XCTAssertEqual(model.whereToSend?.summary, "Sending to Venue (venue-ws)")
        XCTAssertEqual(model.whereToSend?.instances, [Instance(name: "Venue", url: "http://127.0.0.1:1")])
        XCTAssertEqual(model.input?.studioName, "iPhone 3F2A — Back camera")
        XCTAssertEqual(model.advanced?.latencyMs, 200)
        XCTAssertEqual(model.advanced?.passphraseSet, true)
        XCTAssertEqual(model.network, .wifi)

        let signedIn = try XCTUnwrap(
            CoreMessage.parse(
                #"""
                {"id": 1, "ok": {"workspace": "venue-ws", "instances": [{"name": "A", "url": "https://a"},
                 {"name": "B", "url": "https://b"}], "where": {"mode": "osc", "ready": false, "url": null,
                 "instance": null, "workspace": "venue-ws", "instances": [], "signed_in": true,
                 "api_key_set": false, "summary": null}}}
                """#
            ).ok
        ).decode(SignedIn.self)
        XCTAssertEqual(signedIn.workspace, "venue-ws")
        XCTAssertEqual(signedIn.instances.map(\.name), ["A", "B"])
        XCTAssertEqual(signedIn.whereToSend?.ready, false)

        let problems = try XCTUnwrap(
            CoreMessage.parse(#"{"id": 2, "ok": {"problems": [{"setting": "uplink.latency_ms", "message": "too low"}]}}"#).ok
        )
        XCTAssertThrowsError(try problems.decode(ViewModel.self))
        XCTAssertEqual(
            try problems.decode(ProblemsOnly.self).problems, [Problem(setting: "uplink.latency_ms", message: "too low")])
    }

    static let viewModel = #"""
        {"id": 1, "ok": {"version": "0.6.0", "live": false, "network": "wifi",
         "where": {"mode": "osc", "ready": true, "url": "http://127.0.0.1:1", "instance": "Venue",
                   "workspace": "venue-ws", "instances": [{"name": "Venue", "url": "http://127.0.0.1:1"}],
                   "signed_in": true, "api_key_set": false, "summary": "Sending to Venue (venue-ws)"},
         "input": {"device": "cam-1", "audio": null, "label": "Back camera", "studio_name": "iPhone 3F2A — Back camera"},
         "advanced": {"name": "iPhone 3F2A", "latency_ms": 200, "passphrase_set": true, "pbkeylen": null,
                      "bitrate_kbps": 4000, "resolution": "1280x720", "framerate": "30/1", "log_level": "info",
                      "gateway_id": null, "gateway_token_set": false},
         "problems": []}}
        """#

    private func event(_ text: String) throws -> CoreEvent {
        let message = try CoreMessage.parse(text)
        return try CoreEvent.decode(name: XCTUnwrap(message.event), data: message.data ?? .null)
    }

    func testEveryEventDecodes() throws {
        XCTAssertEqual(
            try event(
                #"""
                {"event": "where", "data": {"mode": "self_hosted", "ready": true, "url": "https://ol.example",
                 "instance": "ol.example", "workspace": null, "instances": [], "signed_in": false,
                 "api_key_set": true, "summary": "Sending to ol.example"}}
                """#),
            .whereToSend(
                WhereView(
                    mode: "self_hosted", ready: true, url: "https://ol.example", instance: "ol.example", workspace: nil,
                    instances: [], signedIn: false, apiKeySet: true, summary: "Sending to ol.example")))
        XCTAssertEqual(
            try event(#"{"event": "osc_sign_in_page", "data": {"url": "https://app.osaas.io/api/connect/authorize?x=1"}}"#),
            .oscSignInPage(URL(string: "https://app.osaas.io/api/connect/authorize?x=1")!))
        XCTAssertEqual(try event(#"{"event": "osc_signed_in", "data": {}}"#), .oscSignedIn)
        XCTAssertEqual(
            try event(#"{"event": "input", "data": {"input": "V — Cam", "state": "armed", "message": "Ready."}}"#),
            .input(InputRow(input: "V — Cam", state: "armed", message: "Ready.")))
        XCTAssertEqual(
            try event(#"{"event": "input", "data": {"input": "V — Cam", "state": "live", "message": null}}"#),
            .input(InputRow(input: "V — Cam", state: "live", message: nil)))
        XCTAssertEqual(
            try event(
                #"""
                {"event": "summary", "data": {"inputs": [{"input": "V — Cam", "port": 47250,
                 "uplink": "srt://127.0.0.1:47250?mode=caller&latency=200", "registered": "srt://:47250?mode=listener"}]}}
                """#),
            .summary(
                Summary(inputs: [
                    SummaryRow(
                        input: "V — Cam", port: 47250, uplink: "srt://127.0.0.1:47250?mode=caller&latency=200",
                        registered: "srt://:47250?mode=listener")
                ])))
        XCTAssertEqual(
            try event(#"{"event": "stats", "data": {"inputs": [{"input": "V — Cam", "bitrate_kbps": 4100, "rtt_ms": 31.0, "dropped": 2}]}}"#),
            .stats(Stats(inputs: [StatsRow(input: "V — Cam", bitrateKbps: 4100, rttMs: 31, dropped: 2)])))
        XCTAssertEqual(
            try event(#"{"event": "stats", "data": {"inputs": [{"input": "V — Cam", "bitrate_kbps": null, "rtt_ms": null, "dropped": null}]}}"#),
            .stats(Stats(inputs: [StatsRow(input: "V — Cam", bitrateKbps: nil, rttMs: nil, dropped: nil)])))
        XCTAssertEqual(
            try event(
                #"""
                {"event": "status", "data": {"network": "cellular", "inputs": [{"input": "V — Cam",
                 "productions": [{"name": "Morning Show", "state": "active", "slots": ["video_in_1"]}],
                 "link": {"state": "connected", "dropped_pct": 0.2, "rtt_ms": 31.0}}]}}
                """#),
            .status(
                Status(
                    inputs: [
                        StatusRow(
                            input: "V — Cam",
                            productions: [Holder(name: "Morning Show", state: "active", slots: ["video_in_1"])],
                            link: LinkStatus(state: "connected", droppedPct: 0.2, rttMs: 31))
                    ], network: .cellular)))
        XCTAssertEqual(
            try event(
                #"""
                {"event": "status", "data": {"network": "other", "inputs": [{"input": "V — Cam", "productions": null,
                 "link": {"state": "not_sending", "dropped_pct": null, "rtt_ms": null}}]}}
                """#),
            .status(
                Status(
                    inputs: [
                        StatusRow(
                            input: "V — Cam", productions: nil,
                            link: LinkStatus(state: "not_sending", droppedPct: nil, rttMs: nil))
                    ], network: .other)))
        XCTAssertEqual(
            try event(#"{"event": "notice", "data": {"kind": "sign_in_ends", "at": 1700000000, "message": "Your sign-in ends."}}"#),
            .notice(Notice(kind: "sign_in_ends", message: "Your sign-in ends.", input: nil)))
        XCTAssertEqual(
            try event(
                #"{"event": "notice", "data": {"kind": "slot_added_while_live", "input": "V — Cam", "productions": ["P"], "message": "Reactivate."}}"#),
            .notice(Notice(kind: "slot_added_while_live", message: "Reactivate.", input: "V — Cam")))
        XCTAssertEqual(
            try event(#"{"event": "stopping", "data": {"done": 0, "total": 1}}"#), .stopping(StoppingProgress(done: 0, total: 1)))
        XCTAssertEqual(
            try event(#"{"event": "stopped", "data": {"left_behind": [], "error": null}}"#),
            .stopped(Stopped(leftBehind: [], error: nil)))
        XCTAssertEqual(
            try event(#"{"event": "stopped", "data": {"left_behind": ["V — Cam"], "error": {"message": "Open Live refused", "action": null}}}"#),
            .stopped(Stopped(leftBehind: ["V — Cam"], error: CoreError(message: "Open Live refused", action: nil))))
        XCTAssertEqual(
            try event(#"{"event": "fatal", "data": {"message": "Something went wrong. Share logs for details."}}"#),
            .fatal(message: "Something went wrong. Share logs for details."))
        XCTAssertEqual(try event(#"{"event": "media", "data": {"input": "in-1", "action": "stop"}}"#), .media(.stop(input: "in-1")))
        XCTAssertEqual(
            try event(#"{"event": "media", "data": {"input": "in-1", "action": "restart"}}"#), .media(.restart(input: "in-1")))
        XCTAssertEqual(try event(#"{"event": "something_new", "data": {}}"#), .unknown("something_new"))
        XCTAssertThrowsError(try event(#"{"event": "media", "data": {"input": "in-1", "action": "dance"}}"#))
    }

    /// The `media` start event exactly as mobile/src/core.rs builds it.
    static func mediaStart(passphrase: String?, device: String? = "cam-1") -> String {
        let pass = passphrase.map { #", "passphrase": "\#($0)", "pbkeylen": 32"# } ?? ""
        let device = device.map { #""\#($0)""# } ?? "null"
        return #"""
            {"event": "media", "data": {"input": "in-1", "name": "iPhone 3F2A — Back camera", "action": "start",
             "device": \#(device), "microphone": "mic-1", "test_pattern": false,
             "uplink": {"host": "127.0.0.1", "port": 47250, "latency_ms": 200\#(pass)},
             "video": {"width": 1280, "height": 720, "fps": 30.0, "bitrate_kbps": 4000, "keyframe_interval": 30},
             "audio": {"sample_rate": 48000, "channels": 2, "bitrate_kbps": 128}}}
            """#
    }

    func testTheMediaStartEventDecodes() throws {
        let decoded = try event(Self.mediaStart(passphrase: "known-passphrase-4711"))
        guard case .media(.start(let start)) = decoded else { return XCTFail("\(decoded)") }
        XCTAssertEqual(start.input, "in-1")
        XCTAssertEqual(start.device, "cam-1")
        XCTAssertEqual(start.microphone, "mic-1")
        XCTAssertFalse(start.usesTestPattern)
        XCTAssertEqual(
            start.uplink,
            UplinkSettings(host: "127.0.0.1", port: 47250, latencyMs: 200, passphrase: "known-passphrase-4711", pbkeylen: 32))
        XCTAssertEqual(start.video, VideoSettings(width: 1280, height: 720, fps: 30, bitrateKbps: 4000, keyframeInterval: 30))
        XCTAssertEqual(start.audio, AudioSettings(sampleRate: 48_000, channels: 2, bitrateKbps: 128))
        XCTAssertEqual(String(describing: decoded), "media start in-1")

        let plain = try event(Self.mediaStart(passphrase: nil, device: nil))
        guard case .media(.start(let pattern)) = plain else { return XCTFail("\(plain)") }
        XCTAssertNil(pattern.uplink.passphrase)
        XCTAssertTrue(pattern.usesTestPattern)
    }
}
