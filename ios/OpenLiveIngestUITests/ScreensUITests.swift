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
        let osc = try environment("OLI_FAKE_OSC_URL")
        let control = try FakeControl(base: try environment("OLI_FAKE_CONTROL_URL"))
        _ = try control.call("POST", "/reset")

        let app = XCUIApplication()
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

        // Stop: the source is kept, marked inactive, for the next Go live.
        XCTAssertEqual(go.label, "Stop")
        go.tap()
        wait(go, "label == %@", "Go live", within: 60)
        shot("6 stopped")
        let after = try control.call("GET", "/state")
        let kept = try XCTUnwrap(after["sources"] as? [[String: Any]])
        XCTAssertEqual(kept.count, 1, "\(after)")
        XCTAssertEqual(kept.first?["id"] as? String, sources.first?["id"] as? String, "not the same source")
        XCTAssertEqual(kept.first?["status"] as? String, "inactive", "\(after)")
        XCTAssertEqual(after["deleted"] as? Int, 0, "the source was deleted: \(after)")
    }
}

/// The fakes' control address, over plain HTTP/1.1 on a socket of its own (App Transport Security
/// covers URLSession, not this), one request per connection as the fakes serve them. IPv4 only:
/// the fakes listen on 127.0.0.1.
struct FakeControl {
    let host: String
    let port: UInt16

    struct Failed: Error, CustomStringConvertible {
        let description: String
    }

    init(base: String) throws {
        guard let url = URL(string: base), let host = url.host, let port = url.port, let port16 = UInt16(exactly: port)
        else { throw Failed(description: "not an http://host:port address: \(base)") }
        self.host = host
        self.port = port16
    }

    /// `method path`, the JSON object it answers with.
    func call(_ method: String, _ path: String) throws -> [String: Any] {
        let fd = socket(AF_INET, SOCK_STREAM, 0)
        guard fd >= 0 else { throw Failed(description: "no socket: errno \(errno)") }
        defer { close(fd) }
        var timeout = timeval(tv_sec: 10, tv_usec: 0)
        _ = setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &timeout, socklen_t(MemoryLayout<timeval>.size))
        _ = setsockopt(fd, SOL_SOCKET, SO_SNDTIMEO, &timeout, socklen_t(MemoryLayout<timeval>.size))
        var address = sockaddr_in()
        address.sin_len = UInt8(MemoryLayout<sockaddr_in>.size)
        address.sin_family = sa_family_t(AF_INET)
        address.sin_port = port.bigEndian
        address.sin_addr.s_addr = inet_addr(host)
        let connected = withUnsafePointer(to: &address) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                connect(fd, $0, socklen_t(MemoryLayout<sockaddr_in>.size))
            }
        }
        guard connected == 0 else { throw Failed(description: "\(method) \(path): cannot connect, errno \(errno)") }

        let request = Array(
            "\(method) \(path) HTTP/1.1\r\nHost: \(host):\(port)\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".utf8)
        var offset = 0
        while offset < request.count {
            let n = request[offset...].withUnsafeBytes { write(fd, $0.baseAddress, $0.count) }
            guard n > 0 else { throw Failed(description: "\(method) \(path): write failed, errno \(errno)") }
            offset += n
        }
        var received: [UInt8] = []
        var buffer = [UInt8](repeating: 0, count: 65_536)
        while true {
            let n = buffer.withUnsafeMutableBytes { read(fd, $0.baseAddress, $0.count) }
            if n <= 0 { break }
            received.append(contentsOf: buffer[0..<n])
        }

        let text = String(decoding: received, as: UTF8.self)
        guard text.hasPrefix("HTTP/1.1 200"), let split = text.range(of: "\r\n\r\n") else {
            throw Failed(description: "\(method) \(path): \(text)")
        }
        let body = Data(text[split.upperBound...].utf8)
        guard let object = try JSONSerialization.jsonObject(with: body) as? [String: Any] else {
            throw Failed(description: "\(method) \(path): not a JSON object: \(text)")
        }
        return object
    }
}
