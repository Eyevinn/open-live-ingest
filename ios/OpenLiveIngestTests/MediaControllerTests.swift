import XCTest
@testable import OpenLiveIngest

/// A stand-in for an input's media: records what it was asked, and reports the uplink state the
/// test sets.
final class FakeMedia: MediaRunning {
    let start: MediaStart
    var ran = false
    var stopped = false
    var state: SRTSender.State = .connecting
    var stats: SRTSenderStats?
    private var change: (() -> Void)?

    init(_ start: MediaStart) {
        self.start = start
    }

    func observe(_ change: (() -> Void)?) { self.change = change }
    func run() throws { ran = true }
    func stop() { stopped = true }
    var uplink: (state: SRTSender.State, stats: SRTSenderStats?) { (state, stats) }

    func set(_ state: SRTSender.State, stats: SRTSenderStats? = nil) {
        self.state = state
        self.stats = stats
        change?()
    }
}

/// The media controller and the app model over a fake core: the `media` events are carried out
/// and answered with `media_report`, and the passphrase of `media` start reaches the media but no
/// log line (docs/specs/ios-app.md, section 8.2).
final class MediaControllerTests: XCTestCase {
    private var sent: [String] = []
    private var made: [FakeMedia] = []
    private var lines: [String] = []
    private var observer: UUID?

    override func setUp() {
        super.setUp()
        sent = []
        made = []
        lines = []
        observer = AppLog.observe { [weak self] level, line in
            DispatchQueue.main.async { self?.lines.append("\(level.rawValue): \(line)") }
        }
    }

    override func tearDown() {
        if let observer { AppLog.removeObserver(observer) }
        super.tearDown()
    }

    @MainActor
    private func makeModel() -> (AppModel, CoreChannel) {
        let channel = CoreChannel { [weak self] json in self?.sent.append(json) }
        let model = AppModel(channel: channel, media: { [weak self] start in
            let media = FakeMedia(start)
            self?.made.append(media)
            return media
        })
        return (model, channel)
    }

    /// The `media_report`s sent so far, as JSON values.
    private func reports() throws -> [JSONValue] {
        try sent.map { try JSONDecoder().decode(JSONValue.self, from: Data($0.utf8)) }
            .filter { $0["cmd"]?.string == "media_report" }
            .compactMap { $0["args"] }
    }

    /// Lets the main queue run what was handed to it.
    @MainActor
    private func settle() {
        RunLoop.main.run(until: Date().addingTimeInterval(0.2))
    }

    @MainActor
    func testAKnownPassphraseInMediaStartReachesTheMediaAndNoLog() throws {
        let secret = "known-passphrase-4711"
        let (model, channel) = makeModel()
        // The whole path from the core's text: parse, decode, the model, the controller.
        channel.receive(CoreMessagesTests.mediaStart(passphrase: secret))
        settle()
        XCTAssertEqual(made.count, 1)
        let media = try XCTUnwrap(made.first)
        XCTAssertEqual(media.start.uplink.passphrase, secret, "the passphrase did not reach the media")
        XCTAssertTrue(media.ran)

        media.set(.connected, stats: SRTSenderStats(bytesSent: 1316, packetsSent: 1, packetsSentDropped: 0, rttMs: 30, sendRateMbps: 4))
        settle()
        channel.receive(#"{"event": "media", "data": {"input": "in-1", "action": "restart"}}"#)
        channel.receive(#"{"event": "media", "data": {"input": "in-1", "action": "stop"}}"#)
        // A start that cannot be read at all is not quoted either.
        channel.receive(#"{"event": "media", "data": {"input": "in-1", "action": "start", "uplink": {"passphrase": "\#(secret)"}}}"#)
        settle()
        XCTAssertEqual(made.count, 2, "the restart made the media again")
        XCTAssertTrue(made.allSatisfy(\.stopped))

        XCTAssertTrue(lines.contains { $0.contains("media start in-1") }, "nothing was logged: \(lines)")
        for line in lines {
            XCTAssertFalse(line.contains(secret), "the passphrase was logged: \(line)")
        }
        for json in sent {
            XCTAssertFalse(json.contains(secret), "the passphrase went back to the core: \(json)")
        }
        for start in made.map(\.start) {
            XCTAssertFalse(String(describing: start).contains(secret))
            XCTAssertFalse(String(reflecting: start).contains(secret))
        }
        _ = model
    }

    @MainActor
    func testReportsOnEachChangeAndWhileRunning() throws {
        let (model, channel) = makeModel()
        channel.receive(CoreMessagesTests.mediaStart(passphrase: nil))
        settle()
        let media = try XCTUnwrap(made.first)
        XCTAssertEqual(try reports().map { $0["state"]?.string }, ["starting"])

        let stats = SRTSenderStats(bytesSent: 2632, packetsSent: 2, packetsSentDropped: 0, rttMs: 12, sendRateMbps: 4.1)
        media.set(.connected, stats: stats)
        settle()
        var all = try reports()
        XCTAssertEqual(all.map { $0["state"]?.string }, ["starting", "running"])
        XCTAssertEqual(all.last?["stats"]?["bytes_sent"], .number(2632))

        // Every 2 s while running, the counters again, whether or not the state changed.
        model.mediaTick()
        all = try reports()
        XCTAssertEqual(all.map { $0["state"]?.string }, ["starting", "running", "running"])

        media.set(.waiting)
        settle()
        XCTAssertEqual(try reports().last?["state"]?.string, "starting")
        XCTAssertNil(try reports().last?["stats"])

        channel.receive(#"{"event": "stopped", "data": {"left_behind": [], "error": null}}"#)
        settle()
        XCTAssertTrue(media.stopped)
        XCTAssertEqual(try reports().last?["state"]?.string, "stopped")
    }

    @MainActor
    func testAStartThatFailsIsReportedWithItsReason() throws {
        let channel = CoreChannel { [weak self] json in self?.sent.append(json) }
        let model = AppModel(channel: channel, media: { _ in throw CaptureError.noCamera })
        channel.receive(CoreMessagesTests.mediaStart(passphrase: nil))
        settle()
        let last = try XCTUnwrap(reports().last)
        XCTAssertEqual(last["state"]?.string, "failed")
        XCTAssertEqual(last["reason"]?.string, "no camera on this phone")
        _ = model
    }

    /// The simulator has no camera: it offers the test pattern, and the media sends it.
    @MainActor
    func testTheTestPatternIsSentOnTheSimulator() throws {
        #if targetEnvironment(simulator)
            XCTAssertEqual(Devices.lenses(), [Devices.testPattern])
            XCTAssertFalse(Capture.hasCamera)
        #endif
        let message = try CoreMessage.parse(CoreMessagesTests.mediaStart(passphrase: nil, device: Devices.testPatternID))
        let event = try CoreEvent.decode(name: "media", data: XCTUnwrap(message.data))
        guard case .media(.start(let start)) = event else { return XCTFail("\(event)") }
        XCTAssertFalse(start.usesTestPattern, "as the core sends it")
        let effective = MediaController.effective(start)
        XCTAssertTrue(effective.usesTestPattern)
        XCTAssertNil(effective.device)
        XCTAssertEqual(effective.uplink, start.uplink)
    }

    @MainActor
    func testTheStateAndStatusLinesFollowTheEvents() throws {
        let (model, channel) = makeModel()
        XCTAssertEqual(StatusText.stateLine(model.row, productions: model.status?.productions, live: model.live), "Not live")
        channel.receive(#"{"event": "input", "data": {"input": "V — Cam", "state": "armed", "message": "Ready."}}"#)
        channel.receive(
            #"""
            {"event": "status", "data": {"network": "wifi", "inputs": [{"input": "V — Cam",
             "productions": [{"name": "Morning Show", "state": "inactive", "slots": ["video_in_1"]}],
             "link": {"state": "not_sending", "dropped_pct": null, "rtt_ms": null}}]}}
            """#)
        XCTAssertEqual(
            StatusText.stateLine(model.row, productions: model.status?.productions, live: true), "Waiting for production")
        XCTAssertEqual(StatusText.productionLine(model.status?.productions), "Production: Morning Show (inactive), slot video_in_1")
        XCTAssertEqual(StatusText.linkLine(model.status?.link), "Link: not sending")
        XCTAssertEqual(model.network, .wifi)
        channel.receive(#"{"event": "notice", "data": {"kind": "warning", "message": "Open Live is not answering."}}"#)
        channel.receive(#"{"event": "notice", "data": {"kind": "warning", "message": "Open Live is not answering."}}"#)
        XCTAssertEqual(model.notices.map(\.message), ["Open Live is not answering."])
    }
}
