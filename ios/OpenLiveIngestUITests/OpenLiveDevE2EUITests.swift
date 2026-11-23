import Foundation
import XCTest

/// The agent-run end to end against a real Open Live (OL-64h, #144; docs/specs/ios-app.md,
/// sections 15 and 16): the app half. The host half, .github/scripts/e2e-ios.py, runs a control
/// server on 127.0.0.1 and hands its address over as `OLI_E2E_CONTROL_URL` (a TEST_RUNNER_
/// variable); this test calls it for everything that is not the app: the run's settings, the
/// token on the simulator's pasteboard, the source and the production as Open Live has them,
/// activating and deactivating the production, and the cloud side of each measurement. It skips
/// itself without that address, so ios/run-tests.sh and the CI `ios` job never run it.
///
/// The token never reaches this process: the control server puts it on the simulator's
/// pasteboard, the test pastes it into the token field (a secure field, so the screenshots show
/// dots) and has the pasteboard emptied at once. Everything typed here is not secret.
///
/// Each check that passes is posted to the control server for the report; a failure stops the
/// test, and the run fails on the test's result.
final class OpenLiveDevE2EUITests: XCTestCase {
    private var control: FakeControl!
    private var app: XCUIApplication!

    private func shot(_ name: String) {
        let attachment = XCTAttachment(screenshot: XCUIScreen.main.screenshot())
        attachment.name = name
        attachment.lifetime = .keepAlways
        add(attachment)
    }

    private static func encode(_ text: String) -> String {
        var allowed = CharacterSet.alphanumerics
        allowed.insert(charactersIn: "-._~")
        return text.addingPercentEncoding(withAllowedCharacters: allowed) ?? ""
    }

    @discardableResult
    private func call(_ method: String, _ path: String, _ query: [String: String] = [:]) throws -> [String: Any] {
        let items = query.keys.sorted().map { "\($0)=\(Self.encode(query[$0] ?? ""))" }
        return try control.call(method, items.isEmpty ? path : path + "?" + items.joined(separator: "&"))
    }

    /// Posts a passed check for the report, or fails the test with it.
    private func verify(
        _ ok: Bool, _ id: String, _ detail: String, file: StaticString = #filePath, line: UInt = #line
    ) throws {
        try call("POST", "/check", ["id": id, "ok": ok ? "1" : "0", "detail": detail])
        if !ok { shot("failed: \(id)") }
        XCTAssertTrue(ok, "\(id): \(detail)", file: file, line: line)
    }

    /// Waits until `element`'s label satisfies `format` (over `label`), failing the test if not.
    private func wait(
        _ element: XCUIElement, _ format: String, _ argument: String, within seconds: TimeInterval,
        file: StaticString = #filePath, line: UInt = #line
    ) {
        let predicate = NSPredicate(format: "exists == true AND \(format)", argument)
        let met =
            XCTWaiter().wait(for: [XCTNSPredicateExpectation(predicate: predicate, object: element)], timeout: seconds)
            == .completed
        if !met {
            shot("timed out: \(format) \(argument)")
            let label = element.exists ? element.label : "<no element>"
            XCTFail("\(element) never had \(format) \(argument) within \(seconds) s; it says: \(label)", file: file, line: line)
        }
    }

    /// Polls `body` every `every` seconds until it returns a value or `seconds` pass.
    private func poll<T>(_ seconds: TimeInterval, every: TimeInterval = 1, _ body: () throws -> T?) rethrows -> T? {
        let deadline = Date().addingTimeInterval(seconds)
        repeat {
            if let value = try body() { return value }
            Thread.sleep(forTimeInterval: every)
        } while Date() < deadline
        return try body()
    }

    private struct Source {
        let id: String
        let status: String
    }

    /// The run's sources as the control server last saw them in Open Live.
    private func sources() throws -> [Source] {
        let list = try call("GET", "/source")["sources"] as? [[String: Any]] ?? []
        return list.compactMap { item in
            guard let id = item["id"] as? String else { return nil }
            return Source(id: id, status: item["status"] as? String ?? "")
        }
    }

    private func production() throws -> [String: Any] { try call("GET", "/production") }

    /// The SRT sender's counters on the link line (its accessibility value): packets sent,
    /// dropped and bytes sent, or nil while it carries none.
    private func counters() -> (sent: UInt64, dropped: UInt64, bytes: UInt64?)? {
        let line = app.staticTexts["linkLine"]
        guard line.exists, let value = line.value as? String, !value.isEmpty else { return nil }
        let numbers = value.split(whereSeparator: { !$0.isNumber }).compactMap { UInt64($0) }
        guard numbers.count >= 2 else { return nil }
        return (numbers[0], numbers[1], numbers.count >= 3 ? numbers[2] : nil)
    }

    /// Activates the production holding `source` (creating it the first time) and waits until
    /// Open Live has it `active`.
    private func activate(_ source: String) throws {
        try call("POST", "/production/activate", ["source": source])
        let state = try poll(330, every: 2) { () throws -> String? in
            let p = try production()
            if let error = p["error"] as? String { return "error: \(error)" }
            return (p["busy"] as? String) == nil && (p["status"] as? String) == "active" ? "active" : nil
        }
        try verify(state == "active", "production activated", state ?? "not active within 330 s")
    }

    private func deactivate() throws {
        try call("POST", "/production/deactivate")
        let state = try poll(200, every: 2) { () throws -> String? in
            let p = try production()
            if let error = p["error"] as? String { return "error: \(error)" }
            let status = p["status"] as? String
            return (p["busy"] as? String) == nil && (status == "inactive" || status == "ended") ? status : nil
        }
        try verify(state == "inactive" || state == "ended", "production deactivated", state ?? "still active after 200 s")
    }

    /// Samples the link line's counters every half second for the window (plus a margin) and
    /// has the control server judge them with the cloud side's looks of the same time.
    private func measure(_ phase: String, seconds: Int) throws -> [String: Any] {
        try call("POST", "/measure/start", ["phase": phase])
        let end = Date().addingTimeInterval(TimeInterval(seconds + 8))
        while Date() < end {
            if let c = counters() {
                try call(
                    "POST", "/sample",
                    ["phase": phase, "sent": String(c.sent), "dropped": String(c.dropped), "bytes": c.bytes.map { String($0) } ?? ""])
            }
            Thread.sleep(forTimeInterval: 0.5)
        }
        return try call("POST", "/measure/stop", ["phase": phase])
    }

    /// Pastes what is on the pasteboard into `field` with the edit menu, as a person does.
    private func paste(into field: XCUIElement) {
        field.tap()
        let item = app.menuItems["Paste"]
        if !item.waitForExistence(timeout: 3) {
            field.tap()
        }
        if !item.waitForExistence(timeout: 3) {
            field.press(forDuration: 1.2)
        }
        XCTAssertTrue(item.waitForExistence(timeout: 5), "no Paste in the edit menu of the token field")
        item.tap()
    }

    func testSignInGoLiveArmedActivateMeasureStopReuseAndBackgroundAgainstOpenLive() throws {
        guard let base = ProcessInfo.processInfo.environment["OLI_E2E_CONTROL_URL"], !base.isEmpty else {
            throw XCTSkip("OLI_E2E_CONTROL_URL is not set: run by .github/scripts/e2e-ios.py against a real Open Live")
        }
        continueAfterFailure = false
        control = try FakeControl(base: base)
        let config = try call("GET", "/config")
        let instance = try XCTUnwrap(config["instance"] as? String)
        let label = try XCTUnwrap(config["label"] as? String)
        let seconds = try XCTUnwrap(config["seconds"] as? Int)
        let armedHold = config["armed_hold"] as? Int ?? 10
        let backgroundLimit = config["background_limit"] as? Double ?? 10
        let phoneBound = config["phone_background_bound"] as? Double ?? 4

        app = XCUIApplication()
        app.launchArguments = ["-OLIReset"]
        app.launch()

        // 1. Sign in with a personal access token, pick the instance.
        XCTAssertTrue(app.buttons["signIn"].waitForExistence(timeout: 30), "no Where to send screen")
        app.buttons["whereAdvanced"].tap()
        let field = app.secureTextFields["patField"]
        XCTAssertTrue(field.waitForExistence(timeout: 10))
        try call("POST", "/pasteboard/token")
        paste(into: field)
        try call("POST", "/pasteboard/clear")
        let use = app.buttons["patSubmit"]
        XCTAssertTrue(use.waitForExistence(timeout: 5))
        let enabled = XCTNSPredicateExpectation(predicate: NSPredicate(format: "isEnabled == true"), object: use)
        XCTAssertEqual(XCTWaiter().wait(for: [enabled], timeout: 10), .completed, "nothing was pasted into the token field")
        use.tap()
        let pick = app.buttons["instance \(instance)"]
        let listed = pick.waitForExistence(timeout: 90)
        if !listed, app.staticTexts["problem"].exists {
            try verify(false, "PAT sign-in", "the app says: \(app.staticTexts["problem"].label)")
        }
        try verify(listed, "PAT sign-in", "the token was taken and the workspace's instances listed, '\(instance)' among them")
        shot("1 instances")
        pick.tap()
        wait(app.staticTexts["whereSummary"], "label BEGINSWITH %@", "Sending to \(instance)", within: 30)
        try verify(true, "instance pick", app.staticTexts["whereSummary"].label)
        app.buttons["toWhat"].tap()

        // What to send: the test pattern (no camera on a simulator), the run's label.
        XCTAssertTrue(app.staticTexts["previewTestPattern"].waitForExistence(timeout: 15), "no test pattern offered")
        let labelField = app.textFields["labelField"]
        XCTAssertTrue(labelField.waitForExistence(timeout: 10))
        // The cursor at the end of what is there, which is deleted, then the label; return hides
        // the keyboard.
        labelField.coordinate(withNormalizedOffset: CGVector(dx: 0.97, dy: 0.5)).tap()
        let old = (labelField.value as? String) ?? ""
        if !old.isEmpty && old != labelField.placeholderValue {
            labelField.typeText(String(repeating: XCUIKeyboardKey.delete.rawValue, count: old.count))
        }
        labelField.typeText(label + "\n")
        wait(app.staticTexts["studioName"], "label ENDSWITH %@", " — \(label)", within: 10)
        shot("2 what to send")
        app.buttons["toGoLive"].tap()

        // 2. Go live with no production: Armed, the source registered, nothing sent (AC2).
        let go = app.buttons["goLive"]
        XCTAssertTrue(go.waitForExistence(timeout: 20))
        XCTAssertEqual(go.label, "Go live")
        go.tap()
        let state = app.staticTexts["stateLine"]
        let link = app.staticTexts["linkLine"]
        wait(state, "label == %@", "Armed", within: 90)
        let registered = try poll(30) { () throws -> Source? in try sources().first }
        let source = try XCTUnwrap(registered, "the source '— \(label)' never appeared in Open Live")
        let start = Date()
        var sentAnything = false
        while Date().timeIntervalSince(start) < TimeInterval(armedHold) {
            if counters() != nil || !link.label.hasPrefix("Link: not sending") || state.label != "Armed" { sentAnything = true }
            if try sources().contains(where: { $0.status == "active" }) { sentAnything = true }
            Thread.sleep(forTimeInterval: 1)
        }
        shot("3 armed")
        let armedSources = try sources()
        try verify(
            !sentAnything && armedSources.count == 1, "AC2: Go live before activation arms and sends nothing",
            "Armed for \(armedHold) s, link '\(link.label)', no SRT counters, source \(source.id) registered and not active")

        // 3. The production holding the source goes active: the uplink starts; AC1's numbers.
        try activate(source.id)
        wait(state, "label == %@", "Live", within: 120)
        wait(link, "label BEGINSWITH %@", "Link: connected", within: 60)
        let active = try poll(30) { () throws -> Bool? in try sources().first?.status == "active" ? true : nil }
        shot("4 live")
        try verify(
            active == true, "AC2: activation starts the uplink",
            "Live, '\(link.label)', \(app.staticTexts["productionLine"].label), source active in Open Live")
        let first = try measure("first connect", seconds: seconds)
        shot("5 measured")
        let share = (first["drop_share"] as? Double).map { String(format: "%.3f %%", $0 * 100) } ?? "n/a"
        try verify(
            first["ok"] as? Bool == true, "AC1 on the simulator: under 1 % dropped over \(seconds) s, cloud decodes",
            "dropped \(share) of \(first["packets_sent"] ?? "?") packets, picture \(first["resolution"] ?? "none"), "
                + "sound \(first["audio"] ?? "?")"
                + ((first["problems"] as? [String]).map { $0.isEmpty ? "" : "; " + $0.joined(separator: "; ") } ?? ""))

        // 4. Deactivate: the uplink stops, back to waiting (AC2).
        try deactivate()
        wait(link, "label BEGINSWITH %@", "Link: not sending", within: 60)
        let waiting = ["Armed", "Waiting for production"].contains(state.label)
        shot("6 deactivated")
        try verify(waiting, "AC2: deactivation stops the uplink", "'\(state.label)', '\(link.label)'")

        // 5. Stop keeps the source, inactive; Go live again reuses it with no 409 (AC3).
        go.tap()
        wait(go, "label == %@", "Go live", within: 60)
        let kept = try poll(20) { () throws -> [Source]? in
            let s = try sources()
            return s.count == 1 && s[0].status == "inactive" ? s : nil
        }
        let afterStop = try sources().map { "\($0.id) \($0.status)" }
        try verify(
            kept?.first?.id == source.id, "AC3: Stop keeps the source",
            "source \(kept?.first?.id ?? "missing") kept, inactive (Open Live lists \(afterStop))")
        go.tap()
        let again = poll(90) { () -> String? in
            let label = state.label
            return label == "Armed" || label == "Waiting for production" || label.hasPrefix("Failed") ? label : nil
        }
        shot("7 live again")
        let now = try sources()
        try verify(
            again != nil && !(again ?? "").hasPrefix("Failed") && now.count == 1 && now.first?.id == source.id,
            "AC3: Go live again reuses the source id with no 409",
            "'\(again ?? state.label)', sources \(now.map { $0.id })")

        // 6. Live again, then to the background: paused, the source inactive; foreground resumes.
        try activate(source.id)
        wait(state, "label == %@", "Live", within: 120)
        wait(link, "label BEGINSWITH %@", "Link: connected", within: 60)
        _ = try poll(30) { () throws -> Bool? in try sources().first?.status == "active" ? true : nil }
        Thread.sleep(forTimeInterval: 6)
        let before = counters()?.sent ?? 0
        shot("8 live before the background")
        let backgrounded = Date()
        XCUIDevice.shared.press(.home)
        let inactive = try poll(backgroundLimit, every: 0.25) { () throws -> Bool? in
            try sources().first?.status == "inactive" ? true : nil
        }
        let took = Date().timeIntervalSince(backgrounded)
        try verify(
            inactive == true, "background while live pauses and marks the source inactive",
            String(format: "inactive in Open Live %.1f s after the app went to the background", took)
                + (took > phoneBound ? String(format: " (over the phone's %.0f s bound, as seen through Open Live)", phoneBound) : ""))
        app.activate()
        wait(state, "label == %@", "Live", within: 120)
        wait(link, "label BEGINSWITH %@", "Link: connected", within: 60)
        // Only looks at Open Live that began from here on count for the cloud picture: the
        // production stayed active in the background, so an earlier look may be stale.
        try call("POST", "/measure/start", ["phase": "foreground"])
        let resumed = try poll(30) { () throws -> Bool? in try sources().first?.status == "active" ? true : nil }
        let after = poll(10) { () -> UInt64? in counters()?.sent }
        let picture = try poll(90, every: 2) { () throws -> String? in
            try call("GET", "/picture", ["phase": "foreground"])["resolution"] as? String
        }
        let still = try sources()
        shot("9 live after the foreground")
        try verify(
            resumed == true && still.count == 1 && still.first?.id == source.id && (after ?? .max) < before
                && picture != nil,
            "foreground resumes the uplink",
            "Live, source \(source.id) active again, a new SRT connection (\(after.map { String($0) } ?? "?") packets "
                + "after resuming, \(before) before the background), cloud picture \(picture ?? "none")")

        // 7. Stop; the control server removes the production and the source after the test.
        go.tap()
        wait(go, "label == %@", "Go live", within: 60)
        shot("10 stopped")
    }
}
