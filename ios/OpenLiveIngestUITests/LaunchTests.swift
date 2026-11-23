import XCTest

final class LaunchTests: XCTestCase {
    func testTheAppLaunches() {
        let app = XCUIApplication()
        app.launch()
        XCTAssertTrue(app.staticTexts["title"].waitForExistence(timeout: 30))
    }
}
