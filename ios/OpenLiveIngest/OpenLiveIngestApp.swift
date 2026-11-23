import Network
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
    @State private var monitor = NetworkMonitor()

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
        .onAppear {
            monitor.start { network in model.phone(network: network) }
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

/// The network the phone is on, from `NWPathMonitor`, for the status line (section 5, step 3).
/// What the uplink does on a change of path is OL-64f's (section 9).
final class NetworkMonitor {
    private var monitor: NWPathMonitor?
    private let queue = DispatchQueue(label: "com.eyevinn.open-live-ingest.network")
    /// The last kind reported; touched on `queue` only.
    private var last: NetworkKind?

    func start(_ changed: @escaping (NetworkKind) -> Void) {
        guard monitor == nil else { return }
        let monitor = NWPathMonitor()
        monitor.pathUpdateHandler = { [weak self] path in
            guard let self else { return }
            let kind = NetworkMonitor.kind(of: path)
            guard kind != self.last else { return }
            self.last = kind
            DispatchQueue.main.async { changed(kind) }
        }
        monitor.start(queue: queue)
        self.monitor = monitor
    }

    static func kind(of path: NWPath) -> NetworkKind {
        guard path.status == .satisfied else { return .noNetwork }
        if path.usesInterfaceType(.wifi) { return .wifi }
        if path.usesInterfaceType(.cellular) { return .cellular }
        if path.usesInterfaceType(.wiredEthernet) { return .wired }
        return .other
    }
}
