import Foundation
import XCTest

/// The three screens on the simulator against the fake Open Source Cloud and Open Live of
/// mobile/src/testing.rs, run as a host process (mobile/examples/fake_servers.rs) by
/// ios/run-tests.sh, which hands the addresses over as `OLI_FAKE_OSC_URL` and
/// `OLI_FAKE_CONTROL_URL` (TEST_RUNNER_ variables). The app, a debug build, is pointed at the fake
/// with the test-only launch argument `-OLITestOSCURL`; an SRT listener of the script's stands in
/// for the cloud Strom on 127.0.0.1. The simulator has no camera, so the test pattern is sent.
///
/// Personal access token paste, instance pick, then Go live: Armed, the fake production goes
/// active, Live with the status lines, Stop, and the source is kept on the fake Open Live.
final class ScreensUITests: XCTestCase {
    /// `testing::ACCESS`, which the fake Open Source Cloud takes as a personal access token.
    static let token = "h.eyJ0ZW5hbnRJZCI6InZlbnVlLXdzIn0.s"

    private func environment(_ name: String) throws -> String {
        let value = ProcessInfo.processInfo.environment[name] ?? ""
        guard !value.isEmpty else {
            throw FakeControl.Failed(
                description: "\(name) is not set: run the UI tests with ios/run-tests.sh, which starts the fake servers")
        }
        return value
    }

    private func shot(_ name: String) {
        let attachment = XCTAttachment(screenshot: XCUIScreen.main.screenshot())
        attachment.name = name
        attachment.lifetime = .keepAlways
        add(attachment)
    }

    /// Waits until `element`'s label satisfies `predicate` (a format over `label`).
    @discardableResult
    private func wait(
        _ element: XCUIElement, _ format: String, _ argument: String, within seconds: TimeInterval,
        file: StaticString = #filePath, line: UInt = #line
    ) -> Bool {
        let predicate = NSPredicate(format: "exists == true AND \(format)", argument)
        let met =
            XCTWaiter().wait(for: [XCTNSPredicateExpectation(predicate: predicate, object: element)], timeout: seconds)
            == .completed
        if !met {
            shot("timed out: \(format) \(argument)")
            let label = element.exists ? element.label : "<no element>"
            XCTFail("\(element) never had \(format) \(argument) within \(seconds) s; it says: \(label)", file: file, line: line)
        }
        return met
    }

    func testPasteTokenPickInstanceGoLiveAndStopKeepingTheSource() throws {
        continueAfterFailure = false
        let control = try FakeControl(base: try environment("OLI_FAKE_CONTROL_URL"))
        let app = XCUIApplication()
        let sourceID = try goLiveUntilActive(app, control)

        // Stop: the source is kept, marked inactive, for the next Go live.
        let go = app.buttons["goLive"]
        XCTAssertEqual(go.label, "Stop")
        go.tap()
        wait(go, "label == %@", "Go live", within: 60)
        shot("6 stopped")
        let after = try control.call("GET", "/state")
        let kept = try XCTUnwrap(after["sources"] as? [[String: Any]])
        XCTAssertEqual(kept.count, 1, "\(after)")
        XCTAssertEqual(kept.first?["id"] as? String, sourceID, "not the same source")
        XCTAssertEqual(kept.first?["status"] as? String, "inactive", "\(after)")
        XCTAssertEqual(after["deleted"] as? Int, 0, "the source was deleted: \(after)")
    }

    /// OL-64f (docs/specs/ios-app.md, section 9): live, the app goes to the background (the
    /// screen locked, another app); within 4 s the fake Open Live has the source `inactive`, and
    /// the session stays armed with no background mode. Back in the foreground with the fake
    /// production still active, the uplink starts again and the state line says Live, with the
    /// same source and no new session.
    func testBackgroundWhileLiveMarksTheSourceInactiveAndTheForegroundResumes() throws {
        continueAfterFailure = false
        let control = try FakeControl(base: try environment("OLI_FAKE_CONTROL_URL"))
        let app = XCUIApplication()
        let sourceID = try goLiveUntilActive(app, control)
        let created = try control.call("GET", "/state")["created"] as? Int

        let backgrounded = Date()
        XCUIDevice.shared.press(.home)
        var status: String?
        repeat {
            let state = try control.call("GET", "/state")
            status = ((state["sources"] as? [[String: Any]])?.first)?["status"] as? String
            if status == "inactive" { break }
            Thread.sleep(forTimeInterval: 0.1)
        } while Date().timeIntervalSince(backgrounded) < 4
        let took = Date().timeIntervalSince(backgrounded)
        XCTAssertEqual(status, "inactive", "the source still read \(status ?? "nothing") 4 s after the app went to the background")
        XCTAssertLessThan(took, 4)

        app.activate()
        let state = app.staticTexts["stateLine"]
        wait(state, "label == %@", "Live", within: 60)
        wait(app.staticTexts["linkLine"], "label BEGINSWITH %@", "Link: connected", within: 60)
        shot("7 live again after the background")
        let go = app.buttons["goLive"]
        XCTAssertEqual(go.label, "Stop", "the session ended")
        let after = try control.call("GET", "/state")
        let sources = try XCTUnwrap(after["sources"] as? [[String: Any]])
        XCTAssertEqual(sources.count, 1, "\(after)")
        XCTAssertEqual(sources.first?["id"] as? String, sourceID, "not the same source")
        XCTAssertEqual(after["created"] as? Int, created, "a new source was registered: \(after)")
        XCTAssertEqual(after["deleted"] as? Int, 0, "\(after)")

        go.tap()
        wait(go, "label == %@", "Go live", within: 60)
    }

    /// From a fresh launch to Live: paste the token, pick the instance, the test pattern, Go live
    /// (Armed), the fake production active, Live, and the source marked active on the fake Open
    /// Live. The source's id.
    private func goLiveUntilActive(_ app: XCUIApplication, _ control: FakeControl) throws -> String {
        let osc = try environment("OLI_FAKE_OSC_URL")
        _ = try control.call("POST", "/reset")
        app.launchArguments = ["-OLITestOSCURL", osc, "-OLIReset"]
        app.launch()

        // Step 1: Advanced, paste a personal access token.
        XCTAssertTrue(app.buttons["signIn"].waitForExistence(timeout: 30), "no Where to send screen")
        app.buttons["whereAdvanced"].tap()
        let field = app.secureTextFields["patField"]
        XCTAssertTrue(field.waitForExistence(timeout: 10))
        field.tap()
        field.typeText(Self.token + "\n")

        // Two instances in the workspace: one is picked.
        let morning = app.buttons["instance Morning Studio"]
        XCTAssertTrue(morning.waitForExistence(timeout: 30), "the workspace's instances were not listed")
        XCTAssertTrue(app.buttons["instance Evening Studio"].exists)
        shot("1 instances")
        morning.tap()
        wait(app.staticTexts["whereSummary"], "label == %@", "Sending to Morning Studio (venue-ws)", within: 30)
        shot("2 where to send")
        app.buttons["toWhat"].tap()

        // Step 2: no camera on the simulator, so the test pattern.
        XCTAssertTrue(app.staticTexts["previewTestPattern"].waitForExistence(timeout: 10), "no test pattern offered")
        let next = app.buttons["toGoLive"]
        XCTAssertTrue(next.waitForExistence(timeout: 10))
        shot("3 what to send")
        next.tap()

        // Step 3: Go live arms and sends nothing.
        let go = app.buttons["goLive"]
        XCTAssertTrue(go.waitForExistence(timeout: 15))
        XCTAssertEqual(go.label, "Go live")
        go.tap()
        let state = app.staticTexts["stateLine"]
        wait(state, "label == %@", "Armed", within: 60)
        shot("4 armed")
        let armed = try control.call("GET", "/state")
        let sources = try XCTUnwrap(armed["sources"] as? [[String: Any]])
        XCTAssertEqual(sources.count, 1, "the source was not registered: \(armed)")

        // The production holding the source goes active: the uplink starts, and the input is live.
        _ = try control.call("POST", "/production/active")
        wait(state, "label == %@", "Live", within: 120)
        wait(
            app.staticTexts["productionLine"], "label == %@", "Production: Morning Show (active), slot video_in_1",
            within: 30)
        wait(app.staticTexts["linkLine"], "label BEGINSWITH %@", "Link: connected", within: 60)
        XCTAssertTrue(app.staticTexts["networkLine"].label.hasPrefix("Network: "))
        shot("5 live")
        // Marked active once delivering.
        var live: [String: Any] = [:]
        let deadline = Date().addingTimeInterval(30)
        repeat {
            live = try control.call("GET", "/state")
            if ((live["sources"] as? [[String: Any]])?.first)?["status"] as? String == "active" { break }
            Thread.sleep(forTimeInterval: 1)
        } while Date() < deadline
        XCTAssertEqual(((live["sources"] as? [[String: Any]])?.first)?["status"] as? String, "active", "\(live)")
        return try XCTUnwrap(sources.first?["id"] as? String)
    }
}
