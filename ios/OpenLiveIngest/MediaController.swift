import Foundation

/// One input's running media as the controller drives it: `MediaSession` on the phone, a fake in
/// tests.
protocol MediaRunning: AnyObject {
    /// `change` is called, on any thread, whenever the uplink's state changes; nil stops that.
    func observe(_ change: (() -> Void)?)
    /// Opens the source and starts calling.
    func run() throws
    func stop()
    /// Whether the SRT caller is connected, calling, or stopped, with its counters.
    var uplink: (state: SRTSender.State, stats: SRTSenderStats?) { get }
}

extension MediaSession: MediaRunning {
    func observe(_ change: (() -> Void)?) {
        if let change {
            sender.onStateChange = { _ in change() }
        } else {
            sender.onStateChange = nil
        }
    }

    var uplink: (state: SRTSender.State, stats: SRTSenderStats?) {
        (sender.state, sender.stats())
    }
}

/// Carries out the core's `media` events (docs/specs/ios-app.md, sections 8.2 and 8.3): `start`
/// opens capture (or the test pattern), the encoders, the muxer and the SRT caller with the given
/// `uplink`, `video` and `audio`; `stop` tears them down; `restart` does both. It answers with
/// `media_report` on each state change and every 2 s while running, with libsrt's counters. The
/// core decides when and where; this decides how.
///
/// Logs only the input and the action of a `media` event, never its payload: `uplink.passphrase`
/// is in it. Used on the main thread.
final class MediaController {
    /// Between two reports while running (section 8.2).
    static let reportInterval: TimeInterval = 2

    /// The lens id that means the test pattern: what the simulator, which has no camera, offers.
    static let testPatternDevice = Devices.testPatternID

    typealias Factory = (MediaStart) throws -> MediaRunning

    private let report: (MediaReport) -> Void
    private let factory: Factory
    private var running: [String: (start: MediaStart, media: MediaRunning, last: MediaReport.State?)] = [:]
    private var timer: Timer?

    /// `report` sends a `media_report`; `factory` makes an input's media (a `MediaSession` unless a
    /// test says otherwise).
    init(report: @escaping (MediaReport) -> Void, factory: Factory? = nil) {
        self.report = report
        self.factory = factory ?? { try MediaSession($0) }
    }

    /// The inputs whose media runs.
    var inputs: [String] { Array(running.keys) }

    func handle(_ command: MediaCommand) {
        AppLog.info(command.description)
        switch command {
        case .start(let start):
            begin(start)
        case .stop(let input):
            end(input)
        case .restart(let input):
            guard let start = running[input]?.start else {
                AppLog.warning("media restart \(input): not running")
                return
            }
            end(input)
            begin(start)
        }
    }

    /// Stops everything (the session ended, or the app went wrong).
    func stopAll() {
        for input in inputs { end(input) }
    }

    /// The start as the media takes it: the test pattern when asked for, and for the simulator's
    /// "Test pattern" lens.
    static func effective(_ start: MediaStart) -> MediaStart {
        var start = start
        if start.device == testPatternDevice {
            start.device = nil
            start.testPattern = true
        }
        return start
    }

    private func begin(_ start: MediaStart) {
        let input = start.input
        if running[input] != nil { end(input) }
        do {
            let media = try factory(Self.effective(start))
            media.observe { [weak self] in
                DispatchQueue.main.async { self?.changed(input) }
            }
            running[input] = (start, media, nil)
            send(input, .starting)
            try media.run()
        } catch {
            let reason = Self.reason(error)
            AppLog.error("media start \(input) failed: \(reason)")
            if let media = running.removeValue(forKey: input)?.media { media.stop() }
            report(MediaReport(input: input, state: .failed, reason: reason, stats: nil))
            return
        }
        changed(input)
        startTimer()
    }

    private func end(_ input: String) {
        guard let entry = running.removeValue(forKey: input) else { return }
        entry.media.observe(nil)
        entry.media.stop()
        report(MediaReport(input: input, state: .stopped, reason: nil, stats: nil))
        if running.isEmpty {
            timer?.invalidate()
            timer = nil
        }
    }

    /// A report now if the state changed.
    private func changed(_ input: String) {
        guard let entry = running[input] else { return }
        let state = Self.state(entry.media.uplink.state)
        if state != entry.last { send(input, state) }
    }

    private func send(_ input: String, _ state: MediaReport.State) {
        guard let entry = running[input] else { return }
        let stats = state == .running ? entry.media.uplink.stats : nil
        running[input]?.last = state
        report(MediaReport(input: input, state: state, reason: nil, stats: stats))
    }

    private func startTimer() {
        guard timer == nil else { return }
        let timer = Timer(timeInterval: Self.reportInterval, repeats: true) { [weak self] _ in
            self?.tick()
        }
        RunLoop.main.add(timer, forMode: .common)
        self.timer = timer
    }

    /// Every 2 s: each input's state, with its counters while running.
    func tick() {
        for input in inputs {
            guard let entry = running[input] else { continue }
            send(input, Self.state(entry.media.uplink.state))
        }
    }

    /// The SRT caller's state as `media_report` names it: connected is running; calling, or
    /// waiting to call again, is starting.
    static func state(_ sender: SRTSender.State) -> MediaReport.State {
        switch sender {
        case .connected: return .running
        case .connecting, .waiting: return .starting
        case .stopped: return .starting
        }
    }

    /// Swift's own words for a failure; no credential is in any of them.
    static func reason(_ error: Error) -> String {
        switch error {
        case CaptureError.noCamera: return "no camera on this phone"
        case CaptureError.cameraInput(let why): return "the camera could not be opened: \(why)"
        case CaptureError.microphoneInput(let why): return "the microphone could not be opened: \(why)"
        case CaptureError.audioSession(let why): return "the sound could not be set up: \(why)"
        default: return "the media could not start (\(type(of: error)))"
        }
    }
}
