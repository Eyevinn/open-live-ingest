import XCTest
@testable import OpenLiveIngest

/// A stand-in for UIKit's idle timer and background tasks, and a clock the test moves.
final class FakePhoneSystem: PhoneSystem {
    var idleTimerDisabled = false
    /// Every task begun, in order; `open` those not ended yet.
    private(set) var begun: [Int] = []
    private(set) var open: Set<Int> = []
    private(set) var expiration: [Int: () -> Void] = [:]
    /// The deadlines asked for, not fired yet.
    private(set) var deadlines: [(seconds: TimeInterval, body: () -> Void)] = []

    func beginBackgroundTask(expired: @escaping () -> Void) -> Int? {
        let id = begun.count + 1
        begun.append(id)
        open.insert(id)
        expiration[id] = expired
        return id
    }

    func endBackgroundTask(_ task: Int) {
        XCTAssertTrue(open.contains(task), "task \(task) ended twice, or never begun")
        open.remove(task)
    }

    func after(_ seconds: TimeInterval, _ body: @escaping () -> Void) {
        deadlines.append((seconds, body))
    }

    /// The oldest deadline passes.
    func fireDeadline() {
        guard !deadlines.isEmpty else { return XCTFail("no deadline") }
        deadlines.removeFirst().body()
    }
}

/// The phone's lifecycle (docs/specs/ios-app.md, section 9; section 14 Q5 and Q6) driven with
/// synthetic notifications: the exact `phone` commands, the reconnects, the idle timer, the
/// background task and the thermal notice; then through the app model with a fake media; then
/// against the Rust core and the fake Open Live, where the source must read `inactive` within 4 s
/// of the app going to the background.
final class PhoneTests: XCTestCase {
    private var system = FakePhoneSystem()
    private var sent: [PhoneReport] = []
    /// What was open when each `phone` was sent: it must go inside the background task.
    private var openWhenSent: [Int] = []
    private var reconnects = 0
    private var notices: [Notice?] = []

    override func setUp() {
        super.setUp()
        system = FakePhoneSystem()
        sent = []
        openWhenSent = []
        reconnects = 0
        notices = []
    }

    private func makePhone() -> Phone {
        Phone(
            system: system,
            send: { [unowned self] report in
                self.sent.append(report)
                self.openWhenSent.append(self.system.open.count)
            },
            reconnect: { [unowned self] in self.reconnects += 1 },
            notice: { [unowned self] notice in self.notices.append(notice) })
    }

    private func report(_ foreground: Bool, _ interrupted: Bool, _ network: NetworkKind) -> PhoneReport {
        PhoneReport(foreground: foreground, interrupted: interrupted, network: network)
    }

    func testTheIdleTimerIsOffWhileLiveAndBackAtStop() {
        let phone = makePhone()
        XCTAssertFalse(system.idleTimerDisabled)
        phone.liveChanged(true)
        XCTAssertTrue(system.idleTimerDisabled)
        phone.enteredBackground()
        phone.enteredForeground()
        XCTAssertTrue(system.idleTimerDisabled, "still live")
        phone.liveChanged(false)
        XCTAssertFalse(system.idleTimerDisabled)
    }

    func testBackgroundAndForegroundWhileLive() {
        let phone = makePhone()
        phone.pathChanged(.wifi)
        phone.liveChanged(true)
        XCTAssertEqual(sent, [report(true, false, .wifi)])

        phone.enteredBackground()
        XCTAssertEqual(sent, [report(true, false, .wifi), report(false, false, .wifi)])
        XCTAssertEqual(system.begun.count, 1, "a background task for the teardown")
        XCTAssertEqual(openWhenSent.last, 1, "phone {foreground:false} was sent outside the background task")
        XCTAssertEqual(system.deadlines.map { $0.seconds }, [4], "bounded at 4 s")
        XCTAssertTrue(phone.held)

        // The core armed the input again: its source is marked inactive, the task is done.
        phone.uplinkHeld()
        XCTAssertTrue(system.open.isEmpty)
        // The deadline passing later ends nothing more.
        system.fireDeadline()
        XCTAssertTrue(system.open.isEmpty)

        phone.enteredForeground()
        XCTAssertEqual(sent.last, report(true, false, .wifi))
        XCTAssertEqual(sent.count, 3)
        XCTAssertFalse(phone.held)
        XCTAssertEqual(reconnects, 0)
        XCTAssertTrue(system.idleTimerDisabled)
    }

    func testTheDeadlineEndsTheTaskWhenTheCoreDoesNot() {
        let phone = makePhone()
        phone.liveChanged(true)
        phone.enteredBackground()
        XCTAssertEqual(system.open, [1])
        system.fireDeadline()
        XCTAssertTrue(system.open.isEmpty, "the 4 s deadline did not end the task")

        // A second background: the first task's expiration handler, late, leaves the new one.
        phone.enteredForeground()
        phone.enteredBackground()
        XCTAssertEqual(system.open, [2])
        system.expiration[1]?()
        XCTAssertEqual(system.open, [2])
        system.expiration[2]?()
        XCTAssertTrue(system.open.isEmpty, "iOS's expiration did not end the task")
    }

    func testNoBackgroundTaskWhenNotLive() {
        let phone = makePhone()
        phone.pathChanged(.cellular)
        phone.enteredBackground()
        XCTAssertEqual(sent, [report(true, false, .cellular), report(false, false, .cellular)])
        XCTAssertTrue(system.begun.isEmpty)
        // Stop while in the background task ends it.
        phone.enteredForeground()
        phone.liveChanged(true)
        phone.enteredBackground()
        XCTAssertEqual(system.open.count, 1)
        phone.liveChanged(false)
        XCTAssertTrue(system.open.isEmpty)
    }

    func testInterruptionsAreHandledAsTheBackground() {
        let phone = makePhone()
        phone.pathChanged(.wifi)
        phone.liveChanged(true)

        // A call: the camera and the audio session are both taken; one phone {interrupted:true}.
        phone.interruptionBegan(.capture)
        phone.interruptionBegan(.audio)
        XCTAssertEqual(sent, [report(true, false, .wifi), report(true, true, .wifi)])
        XCTAssertEqual(system.begun.count, 1)
        XCTAssertEqual(openWhenSent.last, 1)

        // Interrupted until both have ended.
        phone.interruptionEnded(.capture)
        XCTAssertEqual(sent.count, 2)
        XCTAssertTrue(phone.held)
        phone.interruptionEnded(.audio)
        XCTAssertEqual(sent.last, report(true, false, .wifi))
        XCTAssertEqual(sent.count, 3)
        XCTAssertTrue(system.open.isEmpty)

        // An end without a beginning sends nothing.
        phone.interruptionEnded(.audio)
        XCTAssertEqual(sent.count, 3)

        // Interrupted in the background: one task.
        phone.enteredBackground()
        phone.interruptionBegan(.capture)
        XCTAssertEqual(Array(sent.suffix(2)), [report(false, false, .wifi), report(false, true, .wifi)])
        XCTAssertEqual(system.begun.count, 2)
        // A late end changes nothing while in the background.
        phone.interruptionEnded(.capture)
        XCTAssertEqual(sent.last, report(false, false, .wifi))
        XCTAssertTrue(phone.held)
        phone.enteredForeground()
        XCTAssertEqual(sent.last, report(true, false, .wifi))
        XCTAssertFalse(phone.held)
    }

    /// On a device, the background interrupts the capture session too, and the core stopping the
    /// uplink releases that session: no `interruptionEnded` ever comes. The foreground ends it.
    func testTheForegroundEndsAnInterruptionWhoseEndNeverCame() {
        let phone = makePhone()
        phone.pathChanged(.wifi)
        phone.liveChanged(true)

        phone.enteredBackground()
        phone.interruptionBegan(.capture)
        phone.interruptionBegan(.audio)
        XCTAssertEqual(Array(sent.suffix(2)), [report(false, false, .wifi), report(false, true, .wifi)])
        // The core stopped the uplink; its capture session is gone.
        phone.uplinkHeld()
        XCTAssertTrue(system.open.isEmpty)

        phone.enteredForeground()
        XCTAssertEqual(sent.last, report(true, false, .wifi))
        XCTAssertFalse(phone.held)
        // Becoming active right after has nothing left to end.
        phone.becameActive()
        XCTAssertEqual(sent.count, 4)
        XCTAssertTrue(system.open.isEmpty)
        XCTAssertEqual(system.begun.count, 1)
    }

    /// A call (or Siri) while in the foreground: the camera and the audio are interrupted, the core
    /// stops the uplink and its capture session with it, so neither end may come. The app becoming
    /// active again after the call ends them; the next interruption holds the uplink again.
    func testBecomingActiveAgainEndsTheInterruptionsOfACall() {
        let phone = makePhone()
        phone.pathChanged(.cellular)
        phone.liveChanged(true)

        phone.interruptionBegan(.capture)
        phone.interruptionBegan(.audio)
        XCTAssertEqual(sent, [report(true, false, .cellular), report(true, true, .cellular)])
        XCTAssertEqual(system.open.count, 1)
        // The media stopped: the input is armed again.
        phone.uplinkHeld()
        XCTAssertTrue(system.open.isEmpty)
        XCTAssertTrue(phone.held)

        phone.becameActive()
        XCTAssertEqual(sent.last, report(true, false, .cellular))
        XCTAssertEqual(sent.count, 3)
        XCTAssertFalse(phone.held)
        XCTAssertTrue(system.open.isEmpty)
        XCTAssertTrue(system.idleTimerDisabled)

        // Becoming active with nothing interrupted sends nothing.
        phone.becameActive()
        XCTAssertEqual(sent.count, 3)

        // The camera still taken: the new uplink's session is interrupted again, and held again.
        phone.interruptionBegan(.capture)
        XCTAssertEqual(sent.last, report(true, true, .cellular))
        XCTAssertEqual(system.begun.count, 2)
        XCTAssertTrue(phone.held)
    }

    func testAChangeOfTheSatisfiedInterfaceReconnects() {
        let phone = makePhone()
        phone.liveChanged(true)
        phone.pathChanged(.wifi)
        XCTAssertEqual(reconnects, 0, "the first path is no change")
        phone.pathChanged(.wifi)
        XCTAssertEqual(sent.count, 1, "the same path again sends nothing")

        phone.pathChanged(.cellular)
        XCTAssertEqual(reconnects, 1, "Wi-Fi to cellular")
        XCTAssertEqual(sent.last, report(true, false, .cellular))

        // No network: keep trying, tear nothing down.
        phone.pathChanged(.noNetwork)
        XCTAssertEqual(reconnects, 1)
        XCTAssertEqual(sent.last, report(true, false, .noNetwork))
        XCTAssertTrue(system.begun.isEmpty)
        XCTAssertTrue(system.idleTimerDisabled)

        // The same interface back: the socket may still be good; libsrt's own retries cover it.
        phone.pathChanged(.cellular)
        XCTAssertEqual(reconnects, 1)
        // Through no network to another interface: reconnect.
        phone.pathChanged(.noNetwork)
        phone.pathChanged(.wifi)
        XCTAssertEqual(reconnects, 2)
        phone.pathChanged(.wired)
        XCTAssertEqual(reconnects, 3)

        XCTAssertEqual(
            sent.map(\.network), [.wifi, .cellular, .noNetwork, .cellular, .noNetwork, .wifi, .wired])

        // Not live: noted, no media to reconnect.
        phone.liveChanged(false)
        phone.pathChanged(.cellular)
        XCTAssertEqual(reconnects, 3)
        XCTAssertEqual(sent.last, report(true, false, .cellular))
    }

    func testTheThermalNotice() {
        let phone = makePhone()
        phone.thermalStateChanged(.nominal)
        phone.thermalStateChanged(.fair)
        XCTAssertEqual(notices, [])
        phone.thermalStateChanged(.serious)
        XCTAssertEqual(notices, [Phone.hotNotice])
        XCTAssertEqual(notices.last??.message, "The phone is hot; the picture may drop frames.")
        phone.thermalStateChanged(.critical)
        XCTAssertEqual(notices.count, 1, "still hot: no second notice")
        phone.thermalStateChanged(.fair)
        XCTAssertEqual(notices, [Phone.hotNotice, nil], "cooled down: the notice goes")
        XCTAssertTrue(sent.isEmpty, "heat is not a phone command")
        XCTAssertEqual(reconnects, 0)
    }

    // MARK: - Through the app model

    private var json: [String] = []
    private var made: [FakeMedia] = []

    @MainActor
    private func makeModel() -> (AppModel, CoreChannel) {
        let channel = CoreChannel { [weak self] text in self?.json.append(text) }
        let model = AppModel(
            channel: channel,
            media: { [weak self] start in
                let media = FakeMedia(start)
                self?.made.append(media)
                return media
            }, system: system)
        return (model, channel)
    }

    private func commands() throws -> [JSONValue] {
        try json.map { try JSONDecoder().decode(JSONValue.self, from: Data($0.utf8)) }
    }

    @MainActor
    private func settle() {
        RunLoop.main.run(until: Date().addingTimeInterval(0.2))
    }

    /// A path change while sending reconnects the SRT caller only: the media (capture, encoders)
    /// is neither stopped nor made again, and the session is not restarted.
    @MainActor
    func testAPathChangeWhileSendingReconnectsWithoutRestarting() throws {
        json = []
        made = []
        let (model, channel) = makeModel()
        model.phone.pathChanged(.wifi)
        model.goLiveOrStop()
        XCTAssertTrue(system.idleTimerDisabled)
        channel.receive(CoreMessagesTests.mediaStart(passphrase: nil))
        settle()
        let media = try XCTUnwrap(made.first)
        media.set(.connected, stats: SRTSenderStats(bytesSent: 1316, packetsSent: 1, packetsSentDropped: 0, rttMs: 20, sendRateMbps: 4))
        settle()

        model.phone.pathChanged(.cellular)
        settle()
        XCTAssertEqual(media.reconnects, 1)
        XCTAssertFalse(media.stopped, "the media was stopped")
        XCTAssertEqual(made.count, 1, "the media was made again")
        let names = try commands().compactMap { $0["cmd"]?.string }
        XCTAssertEqual(names.filter { $0 == "go_live" }.count, 1, "the session was restarted: \(names)")
        XCTAssertFalse(names.contains("stop"))
        let phones = try commands().filter { $0["cmd"]?.string == "phone" }.compactMap { $0["args"] }
        XCTAssertEqual(phones.last?["network"]?.string, "cellular")
        XCTAssertEqual(phones.last?["foreground"], .bool(true))

        // The new connection: counted again from zero, as the core expects.
        media.set(.connecting)
        settle()
        media.set(.connected, stats: SRTSenderStats(bytesSent: 0, packetsSent: 0, packetsSentDropped: 0, rttMs: 20, sendRateMbps: 0))
        settle()
        let reports = try commands().filter { $0["cmd"]?.string == "media_report" }.compactMap { $0["args"] }
        XCTAssertEqual(reports.map { $0["state"]?.string }, ["starting", "running", "starting", "running"])
        XCTAssertEqual(reports.last?["stats"]?["bytes_sent"], .number(0))

        // Heat: the notice, on the Go live screen's banner.
        model.phone.thermalStateChanged(.serious)
        XCTAssertEqual(model.notices.map(\.message), [Phone.hotMessage])
        model.phone.thermalStateChanged(.nominal)
        XCTAssertEqual(model.notices, [])

        channel.receive(#"{"event": "stopped", "data": {"left_behind": [], "error": null}}"#)
        settle()
        XCTAssertFalse(model.live)
        XCTAssertFalse(system.idleTimerDisabled, "the idle timer stayed off after Stop")
    }

    /// The core arming the input again ends the background task.
    @MainActor
    func testTheCoreArmingAgainEndsTheBackgroundTask() throws {
        json = []
        made = []
        let (model, channel) = makeModel()
        model.phone.pathChanged(.wifi)
        model.goLiveOrStop()
        channel.receive(#"{"event": "input", "data": {"input": "V — Cam", "state": "live", "message": null}}"#)
        model.phone.enteredBackground()
        XCTAssertEqual(system.open.count, 1)
        let phones = try commands().filter { $0["cmd"]?.string == "phone" }.compactMap { $0["args"] }
        XCTAssertEqual(phones.last?["foreground"], .bool(false))
        XCTAssertEqual(phones.last?["interrupted"], .bool(false))
        channel.receive(#"{"event": "input", "data": {"input": "V — Cam", "state": "armed", "message": null}}"#)
        XCTAssertTrue(system.open.isEmpty)
        _ = model
    }

    // MARK: - Against the Rust core and the fake Open Live

    /// `testing::ACCESS`, which the fake Open Source Cloud takes as a personal access token.
    private static let token = "h.eyJ0ZW5hbnRJZCI6InZlbnVlLXdzIn0.s"

    /// Runs the main run loop until `done` (checked every 0.1 s) or `seconds` pass.
    @MainActor
    @discardableResult
    private func until(_ seconds: TimeInterval, _ what: String, _ done: () throws -> Bool) rethrows -> Bool {
        let deadline = Date().addingTimeInterval(seconds)
        while Date() < deadline {
            if try done() { return true }
            RunLoop.main.run(until: Date().addingTimeInterval(0.1))
        }
        if try done() { return true }
        XCTFail("not within \(seconds) s: \(what)")
        return false
    }

    /// The Go live of the UI test with the Rust core over the fake Open Live, the media a fake
    /// whose counters move. Live; to the background, where the source must read `inactive` on
    /// the fake within 4 s, armed, the session kept; back to the foreground, the production still
    /// active: a new media start and Live again; Stop.
    @MainActor
    func testBackgroundWhileLiveMarksTheSourceInactiveOnTheFakeOpenLiveWithin4s() throws {
        let env = ProcessInfo.processInfo.environment
        guard let osc = env["OLI_FAKE_OSC_URL"], !osc.isEmpty,
            let controlURL = env["OLI_FAKE_CONTROL_URL"], !controlURL.isEmpty
        else {
            throw XCTSkip("no fake servers: run with ios/run-tests.sh, which starts them")
        }
        continueAfterFailure = false
        let control = try FakeControl(base: controlURL)
        _ = try control.call("POST", "/reset")
        func source() throws -> [String: Any]? {
            (try control.call("GET", "/state")["sources"] as? [[String: Any]])?.first
        }

        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("oli-phone-\(UUID().uuidString)")
        defer { try? FileManager.default.removeItem(at: dir) }
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        setenv("OLI_TEST_OSC_URL", osc, 1)
        let core = try XCTUnwrap(RustCore(supportDirectory: dir, logDirectory: dir))
        unsetenv("OLI_TEST_OSC_URL")
        var made: [FakeMedia] = []
        let model = AppModel(
            channel: core.channel, core: core,
            media: { start in
                let media = FakeMedia(start)
                made.append(media)
                return media
            }, system: system)
        // Counters that move, as a delivering uplink's do.
        var packets: UInt64 = 0
        func deliver() {
            guard let media = made.last, !media.stopped else { return }
            packets += 40
            media.set(
                .connected,
                stats: SRTSenderStats(
                    bytesSent: packets * 1316, packetsSent: packets, packetsSentDropped: 0, rttMs: 5, sendRateMbps: 4))
        }

        model.phone.pathChanged(.wifi)
        model.pasteToken(Self.token)
        until(30, "the workspace's instances") { model.whereToSend?.instances.count == 2 }
        let morning = try XCTUnwrap(model.whereToSend?.instances.first { $0.name == "Morning Studio" })
        model.chooseInstance(morning)
        until(30, "an instance chosen") { model.whereToSend?.ready == true }
        var saved = false
        model.setInput(lens: Devices.testPattern, microphone: nil, label: "Test pattern") { saved = true }
        until(30, "the input saved") { saved }

        model.goLiveOrStop()
        until(60, "armed") { model.row?.state == "armed" }
        XCTAssertTrue(system.idleTimerDisabled)
        _ = try control.call("POST", "/production/active")
        until(30, "a media start") { !made.isEmpty }
        try until(60, "the source active on the fake Open Live") {
            deliver()
            return try source()?["status"] as? String == "active"
        }
        until(10, "Live") { model.row?.state == "live" }
        let id = try XCTUnwrap(source()?["id"] as? String)

        // To the background.
        let backgrounded = Date()
        model.phone.enteredBackground()
        XCTAssertEqual(system.begun.count, 1, "no background task")
        try until(4, "the source inactive on the fake Open Live") {
            try source()?["status"] as? String == "inactive"
        }
        XCTAssertLessThan(Date().timeIntervalSince(backgrounded), 4)
        until(4, "the uplink stopped") { made.first?.stopped == true }
        until(4, "armed again") { model.row?.state == "armed" }
        XCTAssertTrue(system.open.isEmpty, "the background task was not ended")
        XCTAssertTrue(model.live, "the session ended")

        // Back, the production still active: the uplink starts again, and the input is live.
        model.phone.enteredForeground()
        until(30, "a media start again") { made.count == 2 }
        try until(60, "Live again") {
            deliver()
            return model.row?.state == "live"
        }
        let after = try control.call("GET", "/state")
        XCTAssertEqual(after["deleted"] as? Int, 0, "\(after)")
        XCTAssertEqual(try source()?["id"] as? String, id, "not the same source")

        model.goLiveOrStop()
        until(60, "stopped") { !model.live }
        XCTAssertFalse(system.idleTimerDisabled)
    }
}
