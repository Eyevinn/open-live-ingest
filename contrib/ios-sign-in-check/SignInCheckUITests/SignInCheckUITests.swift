// Drives the sign-in check (SignInCheck.swift) the way a person would: tap Sign in,
// allow the system's "wants to use ... to Sign In" alert, then read what happened.
// fake_osc.py must be listening on 127.0.0.1:8765 (the CI job `ios-sign-in` starts it).

import XCTest

final class SignInCheckUITests: XCTestCase {
    private let fakeOsc = "http://127.0.0.1:8765"

    private func shot(_ name: String) {
        let attachment = XCTAttachment(screenshot: XCUIScreen.main.screenshot())
        attachment.name = name
        attachment.lifetime = .keepAlways
        add(attachment)
    }

    private func wait(_ element: XCUIElement, label: String, within seconds: TimeInterval) -> Bool {
        let met = NSPredicate(format: "label == %@", label)
        return XCTWaiter().wait(
            for: [XCTNSPredicateExpectation(predicate: met, object: element)], timeout: seconds
        ) == .completed
    }

    // Waits for sign-in until a deadline, polling about once a second. A single
    // XCTNSPredicateExpectation is not enough on a slow runner: each label read can take
    // 10+ s there, so a 60 s expectation checks the predicate only a few times and can
    // time out with the event already in. `osc_signed_in` in the comma-separated `events`
    // label counts as evidence too: SignInCheck.swift sets `events` and `signedIn = "yes"`
    // in the same main-queue block. After the deadline it reads once more so a slow
    // snapshot can't lose a result that is already there.
    private func waitForSignIn(_ app: XCUIApplication, within seconds: TimeInterval) -> Bool {
        let signedIn = app.staticTexts["signedIn"]
        let events = app.staticTexts["events"]
        func arrived() -> Bool {
            signedIn.label == "yes" || events.label.split(separator: ",").contains("osc_signed_in")
        }
        let deadline = Date().addingTimeInterval(seconds)
        while Date() < deadline {
            if arrived() { return true }
            RunLoop.current.run(until: Date().addingTimeInterval(1))
        }
        return arrived()
    }

    func testLoopbackSignInCompletesInWebAuthenticationSession() throws {
        continueAfterFailure = false
        let app = XCUIApplication()
        app.launchEnvironment["OLI_TEST_OSC_URL"] = fakeOsc
        app.launch()
        app.buttons["signIn"].tap()

        // The consent alert is SpringBoard's on recent iOS, the app's on older ones.
        let springboard = XCUIApplication(bundleIdentifier: "com.apple.springboard")
        let signedIn = app.staticTexts["signedIn"]
        var allowed = false
        let deadline = Date().addingTimeInterval(120)
        while Date() < deadline, !allowed, signedIn.label != "yes" {
            for button in [springboard.buttons["Continue"], app.alerts.buttons["Continue"]]
            where button.exists {
                shot("1 consent alert")
                button.tap()
                allowed = true
                break
            }
            if !allowed { RunLoop.current.run(until: Date().addingTimeInterval(0.5)) }
        }
        print("sign-in-check: consent alert shown and allowed: \(allowed)")
        XCTAssertEqual(app.staticTexts["page"].label, "started", "the session did not start")

        let arrived = waitForSignIn(app, within: 180)
        shot("2 after osc_signed_in")
        print("sign-in-check: events: \(app.staticTexts["events"].label)")
        XCTAssertTrue(arrived, "osc_signed_in never arrived: the sheet did not reach the loopback listener")

        let completion = app.staticTexts["completion"]
        let done = XCTWaiter().wait(
            for: [XCTNSPredicateExpectation(predicate: NSPredicate(format: "label != '-'"), object: completion)],
            timeout: 10
        ) == .completed
        XCTAssertTrue(done, "no completion outcome recorded")
        print("sign-in-check: completion after cancel(): \(completion.label)")
        XCTAssertTrue(
            ["canceledLogin", "not called"].contains(completion.label),
            "cancel() ended the session with \(completion.label)"
        )

        // The sheet is gone and nothing in its place: no error page, the app is in front.
        RunLoop.current.run(until: Date().addingTimeInterval(2))
        shot("3 after cancel")
        XCTAssertTrue(app.buttons["signIn"].isHittable, "something still covers the app after cancel()")
        XCTAssertFalse(app.webViews.firstMatch.exists, "a web view is still shown after cancel()")
        // The harness's own labels have identifiers; an error page's text has none.
        let errorText = NSPredicate(
            format: "identifier == '' AND (label CONTAINS[c] 'cannot' OR label CONTAINS[c] 'can’t' OR label CONTAINS[c] 'error')"
        )
        XCTAssertFalse(app.staticTexts.matching(errorText).firstMatch.exists, "an error is shown in the app")
        XCTAssertFalse(springboard.staticTexts.matching(errorText).firstMatch.exists, "an error is shown over the app")

        let reply = wait(app.staticTexts["reply"], label: "ok", within: 90)
        print("sign-in-check: osc_sign_in reply: \(app.staticTexts["reply"].label)")
        XCTAssertTrue(reply, "the osc_sign_in reply was not ok")
        print("sign-in-check: FINDING yes: the loopback redirect completes in ASWebAuthenticationSession")
    }
}
