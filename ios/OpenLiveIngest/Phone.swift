import Foundation

/// What `Phone` asks of the system: UIKit's idle timer and background tasks, and a clock for the
/// 4 s deadline. `PhoneObserver.swift` has the app's (`UIKitPhoneSystem`); the tests have a fake.
protocol PhoneSystem: AnyObject {
    /// `UIApplication.isIdleTimerDisabled`.
    var idleTimerDisabled: Bool { get set }
    /// `UIApplication.beginBackgroundTask`; `expired` is iOS's expiration handler. Nil when iOS
    /// gives none (`UIBackgroundTaskIdentifier.invalid`).
    func beginBackgroundTask(expired: @escaping () -> Void) -> Int?
    func endBackgroundTask(_ task: Int)
    /// `body` on the main thread after `seconds`.
    func after(_ seconds: TimeInterval, _ body: @escaping () -> Void)
}

/// The phone's lifecycle while the app runs (docs/specs/ios-app.md, section 9, and section 14 Q5
/// and Q6), apart from UIKit so it is tested with synthetic notifications:
///
/// - live (armed or sending): the idle timer off, back on at Stop;
/// - background, lock, or an interruption (a call, Siri, another app taking the camera or the
///   audio session): `phone {foreground:false}` or `{interrupted:true}` inside a background task
///   bounded at 4 s, ended when the core has stopped the uplink (`uplinkHeld`) or at the deadline;
///   the core stops the uplink, PATCHes the source `inactive` and stays armed;
/// - foreground, interruption ended: `phone {foreground:true, interrupted:false}`; the core's next
///   look at the productions starts the uplink again. Coming back to the foreground, or becoming
///   active again, ends every interruption still recorded: the held uplink's capture session is
///   released, so it never posts its end, and an audio `.began` is not promised an `.ended`;
/// - network path: `phone {network}`; a change of the satisfied interface (Wi-Fi to cellular, say)
///   reconnects the SRT caller at once, capture and encoders running on (libsrt 1.5 has no
///   connection migration); no network tears nothing down;
/// - thermal state `serious` or `critical`: a notice, cleared when the phone cools down.
///
/// `phone` is sent only when one of its three values changed. Used on the main thread.
final class Phone {
    /// The background task's bound: the CLI's `teardown_within` (section 9).
    static let teardownDeadline: TimeInterval = 4
    /// The notice's kind and words (section 9).
    static let hotKind = "thermal"
    static let hotMessage = "The phone is hot; the picture may drop frames."

    /// Who interrupted: the capture session and the audio session end theirs separately.
    enum Interruption: Hashable {
        case capture, audio
    }

    private let system: PhoneSystem
    private let send: (PhoneReport) -> Void
    private let reconnect: () -> Void
    private let notice: (Notice?) -> Void

    private(set) var live = false
    private(set) var foreground = true
    private var interruptions: Set<Interruption> = []
    /// The path as `NWPathMonitor` last said; nil before it said anything.
    private(set) var network: NetworkKind?
    /// The last interface that carried traffic (never `none`): what a reconnect is measured against.
    private var satisfied: NetworkKind?
    private(set) var hot = false
    private var lastSent: PhoneReport?
    /// The background task while the core stops the uplink, and which one (for its deadline).
    private var task: (id: Int, generation: Int)?
    private var generation = 0

    /// `send` hands a `phone` command to the core; `reconnect` closes every running SRT caller and
    /// calls again; `notice` shows the thermal notice, or clears it with nil.
    init(
        system: PhoneSystem, send: @escaping (PhoneReport) -> Void, reconnect: @escaping () -> Void,
        notice: @escaping (Notice?) -> Void
    ) {
        self.system = system
        self.send = send
        self.reconnect = reconnect
        self.notice = notice
    }

    var interrupted: Bool { !interruptions.isEmpty }
    /// Whether the core holds the uplink back.
    var held: Bool { !foreground || interrupted }
    /// Whether a background task is open.
    var inBackgroundTask: Bool { task != nil }
    /// What `phone` says now.
    var report: PhoneReport {
        PhoneReport(foreground: foreground, interrupted: interrupted, network: network ?? .other)
    }

    // MARK: - Synthetic notifications

    /// Go live (armed) or Stop.
    func liveChanged(_ live: Bool) {
        guard live != self.live else { return }
        self.live = live
        system.idleTimerDisabled = live
        if !live { endTask() }
    }

    /// `UIApplication.didEnterBackgroundNotification`: the screen locked or another app came up.
    func enteredBackground() {
        guard foreground else { return }
        let wasHeld = held
        foreground = false
        changed(wasHeld: wasHeld)
    }

    /// `UIApplication.willEnterForegroundNotification`. Ends the interruptions still recorded too
    /// (see `becameActive`).
    func enteredForeground() {
        guard !foreground else { return }
        let wasHeld = held
        foreground = true
        interruptions.removeAll()
        changed(wasHeld: wasHeld)
    }

    /// `UIApplication.didBecomeActiveNotification`: a call, Siri or another app is no longer over
    /// this one, so whatever interrupted it is over. The end may never come otherwise: the core
    /// stopping the uplink releases its `AVCaptureSession`, which then posts no
    /// `interruptionEndedNotification`, and iOS does not promise an audio `.ended` for every
    /// `.began`. If the camera or the audio is still taken, the next uplink's session is
    /// interrupted again and the hold comes back.
    func becameActive() {
        guard foreground, interrupted else { return }
        let wasHeld = held
        interruptions.removeAll()
        changed(wasHeld: wasHeld)
    }

    /// `AVCaptureSession.wasInterruptedNotification` (not for `videoDeviceNotAvailableInBackground`:
    /// `foreground:false` already holds the uplink then), or `AVAudioSession.interruptionNotification`
    /// with `.began`.
    func interruptionBegan(_ source: Interruption) {
        let wasHeld = held
        guard interruptions.insert(source).inserted else { return }
        changed(wasHeld: wasHeld)
    }

    /// `AVCaptureSession.interruptionEndedNotification`, or `.ended`.
    func interruptionEnded(_ source: Interruption) {
        let wasHeld = held
        guard interruptions.remove(source) != nil else { return }
        changed(wasHeld: wasHeld)
    }

    /// `NWPathMonitor`'s path, as a kind.
    func pathChanged(_ kind: NetworkKind) {
        guard kind != network else { return }
        network = kind
        if kind != .noNetwork {
            let before = satisfied
            satisfied = kind
            // A new interface, not the first nor the same one back after no network: the socket
            // is bound to the old local address, so call again now on the new path.
            if live, let before, before != kind { reconnect() }
        }
        sendIfChanged()
    }

    /// `ProcessInfo.thermalStateDidChangeNotification`.
    func thermalStateChanged(_ state: ProcessInfo.ThermalState) {
        let hot = state == .serious || state == .critical
        guard hot != self.hot else { return }
        self.hot = hot
        notice(hot ? Self.hotNotice : nil)
    }

    /// The core stopped the uplink after a hold (the input is armed again, or the session
    /// stopped): the background task is done.
    ///
    /// Held while armed but not sending, the core has nothing to stop and sends no `input` event,
    /// so this is not called: the task then stays open until the 4 s deadline (or iOS's
    /// expiration) ends it. Bounded, and the app does nothing in it.
    func uplinkHeld() {
        endTask()
    }

    static var hotNotice: Notice { Notice(kind: hotKind, message: hotMessage, input: nil) }

    // MARK: -

    private func changed(wasHeld: Bool) {
        // Held from now on while live: the core's teardown runs inside a background task.
        if live, held, !wasHeld { beginTask() }
        if !held { endTask() }
        sendIfChanged()
    }

    private func sendIfChanged() {
        let now = report
        guard now != lastSent else { return }
        lastSent = now
        send(now)
    }

    private func beginTask() {
        endTask()
        generation += 1
        let gen = generation
        let id = system.beginBackgroundTask { [weak self] in self?.endTask(generation: gen) }
        guard let id else { return }
        task = (id, gen)
        system.after(Self.teardownDeadline) { [weak self] in self?.endTask(generation: gen) }
    }

    /// Ends the open task; with `generation`, only if it is still that one.
    private func endTask(generation gen: Int? = nil) {
        guard let task, gen == nil || gen == task.generation else { return }
        self.task = nil
        system.endBackgroundTask(task.id)
    }
}

/// No idle timer and no background tasks: the unit-test host, and a model a test makes without a
/// system of its own.
final class NoPhoneSystem: PhoneSystem {
    var idleTimerDisabled = false
    func beginBackgroundTask(expired: @escaping () -> Void) -> Int? { nil }
    func endBackgroundTask(_ task: Int) {}
    func after(_ seconds: TimeInterval, _ body: @escaping () -> Void) {}
}
