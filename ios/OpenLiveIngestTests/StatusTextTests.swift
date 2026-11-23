import XCTest
@testable import OpenLiveIngest

/// Step 3's words (docs/specs/ios-app.md, section 5; desktop spec, section 5.1).
final class StatusTextTests: XCTestCase {
    private func row(_ state: String, _ message: String? = nil) -> InputRow {
        InputRow(input: "V — Cam", state: state, message: message)
    }

    private let holding = [Holder(name: "Morning Show", state: "inactive", slots: ["video_in_1"])]

    func testTheStateLine() {
        XCTAssertEqual(StatusText.stateLine(nil, productions: nil, live: false), "Not live")
        XCTAssertEqual(StatusText.stateLine(nil, productions: nil, live: true), "Starting")
        XCTAssertEqual(StatusText.stateLine(row("registering"), productions: nil, live: true), "Registering")
        XCTAssertEqual(StatusText.stateLine(row("armed"), productions: nil, live: true), "Armed")
        XCTAssertEqual(StatusText.stateLine(row("armed"), productions: [], live: true), "Armed")
        XCTAssertEqual(StatusText.stateLine(row("armed"), productions: holding, live: true), "Waiting for production")
        XCTAssertEqual(StatusText.stateLine(row("connecting"), productions: holding, live: true), "Connecting")
        XCTAssertEqual(StatusText.stateLine(row("live"), productions: holding, live: true), "Live")
        XCTAssertEqual(
            StatusText.stateLine(row("failed", "the camera could not be opened"), productions: nil, live: true),
            "Failed: the camera could not be opened")
        XCTAssertEqual(StatusText.stateLine(row("failed"), productions: nil, live: true), "Failed")
        XCTAssertEqual(StatusText.stateLine(row("stopping"), productions: nil, live: true), "Stopping")
    }

    func testTheProductionLine() {
        XCTAssertEqual(StatusText.productionLine(nil), "Production: unknown")
        XCTAssertEqual(StatusText.productionLine([]), "Production: none holds this source")
        XCTAssertEqual(
            StatusText.productionLine([Holder(name: "Morning Show", state: "active", slots: ["video_in_1"])]),
            "Production: Morning Show (active), slot video_in_1")
        XCTAssertEqual(
            StatusText.productionLine([
                Holder(name: "A", state: "active", slots: ["video_in_1", "video_in_3"]),
                Holder(name: "B", state: "ended", slots: ["video_in_2"]),
            ]),
            "Production: A (active), slots video_in_1, video_in_3; B (ended), slot video_in_2")
    }

    func testTheLinkAndNetworkLines() {
        XCTAssertEqual(StatusText.linkLine(nil), "Link: not sending")
        XCTAssertEqual(
            StatusText.linkLine(LinkStatus(state: "not_sending", droppedPct: nil, rttMs: nil)), "Link: not sending")
        XCTAssertEqual(StatusText.linkLine(LinkStatus(state: "connecting", droppedPct: nil, rttMs: nil)), "Link: connecting")
        XCTAssertEqual(
            StatusText.linkLine(LinkStatus(state: "connected", droppedPct: 0.2, rttMs: 31.4)),
            "Link: connected, 0.2 % dropped, RTT 31 ms")
        XCTAssertEqual(StatusText.networkLine(.wifi, bitrateKbps: 4100), "Network: Wi-Fi, 4.1 Mbit/s")
        XCTAssertEqual(StatusText.networkLine(.cellular, bitrateKbps: nil), "Network: Cellular")
        XCTAssertEqual(StatusText.networkLine(nil, bitrateKbps: 0), "Network: unknown")
    }
}
