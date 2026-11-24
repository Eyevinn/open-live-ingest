import XCTest
@testable import OpenLiveIngest

/// The app's Info.plist against docs/specs/ios-app.md, section 11, and its version against the
/// workspace's (section 3: one version for the phone, the desktop app and the CLI).
final class AppInfoTests: XCTestCase {
    private let info = Bundle.main.infoDictionary ?? [:]

    func testTheInfoPlistMatchesSection11() {
        XCTAssertEqual(Bundle.main.bundleIdentifier, "com.eyevinn.open-live-ingest.ios")
        XCTAssertFalse((info["NSCameraUsageDescription"] as? String ?? "").isEmpty)
        XCTAssertFalse((info["NSMicrophoneUsageDescription"] as? String ?? "").isEmpty)
        XCTAssertNil(info["NSLocalNetworkUsageDescription"], "the loopback listener is not the local network")
        XCTAssertEqual(info["ITSAppUsesNonExemptEncryption"] as? Bool, false)
        XCTAssertEqual(info["UIRequiredDeviceCapabilities"] as? [String], ["arm64"])
        XCTAssertEqual(info["LSApplicationCategoryType"] as? String, "public.app-category.video")
        XCTAssertEqual((info["UIBackgroundModes"] as? [String]) ?? [], [], "no background modes (section 9)")
        XCTAssertEqual(info["MinimumOSVersion"] as? String, "17.0")
    }

    /// `CFBundleShortVersionString` is the workspace version, which the core reports as its own.
    func testTheVersionIsTheWorkspaceVersion() throws {
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("oli-version-\(UUID().uuidString)")
        defer { try? FileManager.default.removeItem(at: dir) }
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        let core = try XCTUnwrap(RustCore(supportDirectory: dir, logDirectory: dir))
        let answered = expectation(description: "the state reply")
        var version: String?
        core.channel.send(.state) { reply in
            if case .success(let value) = reply { version = try? value.decode(ViewModel.self).version }
            answered.fulfill()
        }
        wait(for: [answered], timeout: 20)
        let coreVersion = try XCTUnwrap(version)
        XCTAssertEqual(info["CFBundleShortVersionString"] as? String, coreVersion)
    }
}
