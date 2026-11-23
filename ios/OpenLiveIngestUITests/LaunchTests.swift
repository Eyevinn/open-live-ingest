import XCTest

final class LaunchTests: XCTestCase {
    /// The app opens on step 1, Where to send: signed out, or on its summary.
    func testTheAppLaunches() {
        let app = XCUIApplication()
        app.launchArguments = ["-OLIReset"]
        app.launch()
        XCTAssertTrue(app.buttons["signIn"].waitForExistence(timeout: 30))
        XCTAssertTrue(app.navigationBars["Where to send"].exists)
    }
}
