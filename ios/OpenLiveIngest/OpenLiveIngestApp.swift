import SwiftUI

/// The app's entry point. The screens come with OL-64e (docs/specs/ios-app.md, section 5).
@main
struct OpenLiveIngestApp: App {
    var body: some Scene {
        WindowGroup {
            Text("Open Live Ingest")
                .accessibilityIdentifier("title")
        }
    }
}
