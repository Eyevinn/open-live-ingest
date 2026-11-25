import Foundation
import SwiftUI

/// What the screens show and do (docs/specs/ios-app.md, section 5), over the Rust core: the core
/// decides when, where and what is registered; the media controller carries out its `media`
/// events (section 8.3). One per app, on the main thread.
@MainActor
final class AppModel: ObservableObject {
    // Step 1
    @Published private(set) var whereToSend: WhereView?
    @Published private(set) var signingIn = false
    // Steps 2 and 3
    @Published private(set) var view: ViewModel?
    @Published private(set) var live = false {
        didSet { phone?.liveChanged(live) }
    }
    @Published private(set) var row: InputRow?
    @Published private(set) var status: StatusRow?
    /// The SRT relay the uplink goes through while it runs; nil without one.
    @Published private(set) var relay: RelayRoute?
    /// The uplink line without the relay host and the stream id (`-OLIRedactUplink`, a debug
    /// build's agent-run end to end only, whose screenshots are published).
    private(set) var redactUplink = false
    @Published private(set) var network: NetworkKind?
    @Published private(set) var bitrateKbps: Int?
    @Published private(set) var stopping: StoppingProgress?
    @Published private(set) var notices: [Notice] = []
    /// The picture being sent and its level while the uplink runs (step 3, section 5).
    let liveMonitor = MonitorDisplay()
    // Advanced
    @Published private(set) var advancedProblems: [Problem] = []
    @Published private(set) var advancedWarnings: [Problem] = []
    // Anything that went wrong with the last request, for the screen it came from.
    @Published var problem: String?
    @Published var goProblem: String?
    /// A panic in the core: "Something went wrong", with Share logs.
    @Published private(set) var fatal: String?
    @Published private(set) var busy = false

    /// The core's log, for Share logs.
    let logFile: URL?

    private let channel: CoreChannel?
    private let core: RustCore?
    private var media: MediaController!
    /// The phone's lifecycle (section 9): background, interruptions, network, heat.
    private(set) var phone: Phone!
    /// The app's notifications into `phone`; none in the tests.
    private var observer: PhoneObserver?
    private let sheet = SignInSheet()

    /// Over `channel`, with `media` making the inputs' media (a `MediaSession` unless a test says
    /// otherwise) and `system` the idle timer and background tasks (none unless the app or a test
    /// says otherwise).
    init(
        channel: CoreChannel?, core: RustCore? = nil, logFile: URL? = nil, media: MediaController.Factory? = nil,
        system: PhoneSystem? = nil
    ) {
        self.channel = channel
        self.core = core
        self.logFile = logFile
        self.media = MediaController(
            report: { [weak channel] report in channel?.send(.mediaReport(report)) }, factory: media,
            monitor: MediaMonitor(display: liveMonitor, pictures: true))
        phone = Phone(
            system: system ?? NoPhoneSystem(),
            send: { [weak channel] report in channel?.send(.phone(report)) },
            reconnect: { [weak self] in self?.media.reconnectAll() },
            notice: { [weak self] notice in self?.showHot(notice) })
        channel?.onEvent = { [weak self] event in self?.handle(event) }
        if channel == nil {
            fatal = "The app could not start. Restart the phone, or Share logs."
        }
    }

    /// The app's own: the Rust core over the app's Application Support and Logs folders. A debug
    /// build takes two launch arguments, for the UI tests only: `-OLITestOSCURL <url>` points the
    /// core at a fake Open Source Cloud (`OLI_TEST_OSC_URL`), and `-OLIReset` starts from no
    /// settings. `-OLIRedactUplink` keeps the relay host and the stream id off the screen.
    static func app() -> AppModel {
        let files = FileManager.default
        let support = files.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
        let logs = files.urls(for: .libraryDirectory, in: .userDomainMask)[0].appendingPathComponent("Logs")
        #if DEBUG
            let arguments = ProcessInfo.processInfo.arguments
            if let at = arguments.firstIndex(of: "-OLITestOSCURL"), at + 1 < arguments.count {
                setenv("OLI_TEST_OSC_URL", arguments[at + 1], 1)
            }
            if arguments.contains("-OLIReset") {
                try? files.removeItem(at: support.appendingPathComponent("open-live-ingest"))
            }
        #endif
        var settings = support.appendingPathComponent("open-live-ingest")
        try? files.createDirectory(at: settings, withIntermediateDirectories: true)
        try? files.createDirectory(at: logs, withIntermediateDirectories: true)
        // The settings can hold a passphrase: not in the iCloud backup (section 7).
        var exclude = URLResourceValues()
        exclude.isExcludedFromBackup = true
        try? settings.setResourceValues(exclude)
        let core = RustCore(supportDirectory: support, logDirectory: logs)
        let model = AppModel(
            channel: core?.channel, core: core, logFile: logs.appendingPathComponent("app.log"),
            system: UIKitPhoneSystem())
        let observer = PhoneObserver(phone: model.phone)
        observer.start()
        model.observer = observer
        #if DEBUG
            model.redactUplink = ProcessInfo.processInfo.arguments.contains("-OLIRedactUplink")
        #endif
        model.refresh()
        return model
    }

    // MARK: - Requests

    /// The view model, at launch and after a session.
    func refresh() {
        channel?.send(.state) { [weak self] reply in
            if case .success(let value) = reply { self?.apply(value) }
        }
    }

    /// "Sign in with Open Source Cloud": the core's loopback sign-in, its page in the sheet.
    func signIn() {
        guard let channel, !signingIn else { return }
        signingIn = true
        problem = nil
        channel.send(.oscSignIn) { [weak self] reply in
            guard let self else { return }
            self.signingIn = false
            self.sheet.finish()
            self.signedIn(reply)
        }
    }

    /// Advanced, "Paste a personal access token".
    func pasteToken(_ token: String) {
        request(.oscPasteToken(token: token)) { [weak self] reply in self?.signedIn(reply) }
    }

    func chooseInstance(_ instance: Instance) {
        request(.chooseInstance(url: instance.url)) { [weak self] reply in self?.answered(reply) }
    }

    /// Advanced, "Use a self-hosted Open Live".
    func selfHosted(url: String, apiKey: String) {
        request(.selfHosted(url: url, apiKey: apiKey.isEmpty ? nil : apiKey)) { [weak self] reply in
            self?.answered(reply)
        }
    }

    /// Saves the lens, the microphone and the label; `done` when they are saved.
    func setInput(lens: Lens, microphone: Microphone?, label: String, done: @escaping () -> Void) {
        request(.setInput(device: lens.id, audio: microphone?.id, label: label)) { [weak self] reply in
            if self?.answered(reply) == true { done() }
        }
    }

    func setAdvanced(_ values: AdvancedValues) {
        advancedProblems = []
        advancedWarnings = []
        request(.setAdvanced(values)) { [weak self] reply in
            guard let self else { return }
            switch reply {
            case .success(let value):
                if let model = try? value.decode(ViewModel.self) {
                    self.apply(model)
                    self.advancedWarnings = model.warnings ?? []
                } else if let only = try? value.decode(ProblemsOnly.self) {
                    self.advancedProblems = only.problems
                }
            case .failure(let error):
                self.advancedProblems = [Problem(setting: nil, message: error.message)]
            }
        }
    }

    func forgetSignIn() {
        request(.forgetSignIn) { [weak self] reply in self?.answered(reply) }
    }

    /// The one button: Go live, or Stop.
    func goLiveOrStop() {
        guard let channel else { return }
        goProblem = nil
        if live {
            channel.send(.stop)
            return
        }
        row = nil
        status = nil
        relay = nil
        bitrateKbps = nil
        notices = []
        stopping = nil
        if phone.hot { notices = [Phone.hotNotice] }
        live = true
        channel.send(.goLive) { [weak self] reply in
            guard let self else { return }
            if case .failure(let error) = reply {
                self.live = false
                self.goProblem = error.message
            }
        }
    }

    /// The media's 2 s report, now (the tests' clock).
    func mediaTick() {
        media.tick()
    }

    func dismiss(_ notice: Notice) {
        notices.removeAll { $0.id == notice.id }
    }

    /// The thermal notice (section 9), or none once the phone has cooled down.
    private func showHot(_ notice: Notice?) {
        notices.removeAll { $0.kind == Phone.hotKind }
        if let notice { notices.append(notice) }
    }

    private func request(_ command: CoreCommand, then: @escaping (CoreChannel.Reply) -> Void) {
        guard let channel else { return }
        busy = true
        problem = nil
        channel.send(command) { [weak self] reply in
            self?.busy = false
            then(reply)
        }
    }

    // MARK: - Replies and events

    @discardableResult
    private func answered(_ reply: CoreChannel.Reply) -> Bool {
        switch reply {
        case .success(let value):
            apply(value)
            return true
        case .failure(let error):
            problem = error.message
            return false
        }
    }

    private func signedIn(_ reply: CoreChannel.Reply) {
        switch reply {
        case .success(let value):
            if let signed = try? value.decode(SignedIn.self), let place = signed.whereToSend {
                whereToSend = place
            }
            refresh()
        case .failure(let error):
            problem = error.message
        }
    }

    private func apply(_ value: JSONValue) {
        guard let model = try? value.decode(ViewModel.self) else { return }
        apply(model)
    }

    private func apply(_ model: ViewModel) {
        view = model
        if let place = model.whereToSend { whereToSend = place }
        if let network = model.network { self.network = network }
        if model.problem != nil { problem = model.problem }
    }

    func handle(_ event: CoreEvent) {
        switch event {
        case .whereToSend(let place):
            whereToSend = place
        case .oscSignInPage(let url):
            sheet.present(url) { [weak self] in
                self?.channel?.send(.oscSignInCancel)
            }
        case .oscSignedIn:
            sheet.finish()
        case .input(let row):
            self.row = row
            // Armed again (or failed) after a hold: the core's part of the background is done.
            if row.state == "armed" || row.state == "failed" { phone.uplinkHeld() }
        case .summary:
            break
        case .stats(let stats):
            bitrateKbps = stats.inputs.first?.bitrateKbps
        case .status(let status):
            self.status = status.inputs.first
            network = status.network
        case .notice(let notice):
            if !notices.contains(where: { $0.id == notice.id }) { notices.append(notice) }
        case .stopping(let progress):
            stopping = progress
        case .stopped(let stopped):
            media.stopAll()
            relay = nil
            live = false
            stopping = nil
            if let error = stopped.error { goProblem = error.message }
            refresh()
        case .media(let command):
            media.handle(command)
            relay = media.relay
        case .fatal(let message):
            media.stopAll()
            relay = nil
            fatal = message
        case .unknown:
            break
        }
    }
}
