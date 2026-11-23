import SwiftUI

/// The app's entry point (docs/specs/ios-app.md, section 5): one navigation stack over the
/// desktop's three steps, Where to send, What to send, and Go live / Stop.
@main
struct OpenLiveIngestApp: App {
    @StateObject private var model: AppModel =
        OpenLiveIngestApp.isUnitTestHost ? AppModel(channel: nil) : AppModel.app()

    /// The unit tests run inside the app; it then starts no core of its own.
    private static var isUnitTestHost: Bool {
        ProcessInfo.processInfo.environment["XCTestConfigurationFilePath"] != nil
    }

    var body: some Scene {
        WindowGroup {
            ContentView(model: model)
        }
    }
}

/// The three steps, and what covers them when the core went wrong.
struct ContentView: View {
    @ObservedObject var model: AppModel
    @State private var path: [Step] = []

    enum Step: Hashable {
        case what, go
    }

    var body: some View {
        NavigationStack(path: $path) {
            WhereToSendView(model: model) { path.append(.what) }
                .navigationDestination(for: Step.self) { step in
                    switch step {
                    case .what: WhatToSendView(model: model) { path.append(.go) }
                    case .go: GoLiveView(model: model)
                    }
                }
        }
        .overlay {
            if let fatal = model.fatal {
                FatalView(message: fatal, logFile: model.logFile)
            }
        }
    }
}

/// "Something went wrong", with Share logs: the core caught a panic, or could not start.
struct FatalView: View {
    let message: String
    let logFile: URL?

    var body: some View {
        VStack(spacing: 16) {
            Text("Something went wrong")
                .font(.title2.bold())
                .accessibilityIdentifier("fatalTitle")
            Text(message)
                .multilineTextAlignment(.center)
            if let logFile {
                ShareLink(item: logFile) {
                    Label("Share logs", systemImage: "square.and.arrow.up")
                }
            }
        }
        .padding(24)
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .background(.regularMaterial)
    }
}
