import AVFoundation
import Network
import UIKit

/// `PhoneSystem` over UIKit: the idle timer and background tasks of `UIApplication.shared`. Called
/// on the main thread only, as `Phone` is.
final class UIKitPhoneSystem: PhoneSystem {
    var idleTimerDisabled: Bool {
        get { MainActor.assumeIsolated { UIApplication.shared.isIdleTimerDisabled } }
        set { MainActor.assumeIsolated { UIApplication.shared.isIdleTimerDisabled = newValue } }
    }

    func beginBackgroundTask(expired: @escaping () -> Void) -> Int? {
        let id = MainActor.assumeIsolated {
            UIApplication.shared.beginBackgroundTask(withName: "Pause the uplink", expirationHandler: expired)
        }
        return id == .invalid ? nil : id.rawValue
    }

    func endBackgroundTask(_ task: Int) {
        MainActor.assumeIsolated {
            UIApplication.shared.endBackgroundTask(UIBackgroundTaskIdentifier(rawValue: task))
        }
    }

    func after(_ seconds: TimeInterval, _ body: @escaping () -> Void) {
        DispatchQueue.main.asyncAfter(deadline: .now() + seconds, execute: body)
    }
}

/// The notifications of docs/specs/ios-app.md, section 9, turned into `Phone`'s calls on the main
/// thread: the app going to the background and back, capture and audio-session interruptions, the
/// network path (`NWPathMonitor`) and the thermal state. The app's only; nothing here decides.
final class PhoneObserver {
    private let phone: Phone
    private var tokens: [NSObjectProtocol] = []
    private let monitor = NetworkMonitor()

    init(phone: Phone) {
        self.phone = phone
    }

    deinit {
        for token in tokens { NotificationCenter.default.removeObserver(token) }
    }

    func start() {
        guard tokens.isEmpty else { return }
        let center = NotificationCenter.default
        func on(_ name: Notification.Name, _ body: @escaping (Notification) -> Void) {
            self.tokens.append(center.addObserver(forName: name, object: nil, queue: .main, using: body))
        }
        on(UIApplication.didEnterBackgroundNotification) { [phone] _ in phone.enteredBackground() }
        on(UIApplication.willEnterForegroundNotification) { [phone] _ in phone.enteredForeground() }
        // Any capture session's: Capture's own is private to it.
        on(AVCaptureSession.wasInterruptedNotification) { [phone] _ in phone.interruptionBegan(.capture) }
        on(AVCaptureSession.interruptionEndedNotification) { [phone] _ in phone.interruptionEnded(.capture) }
        on(AVAudioSession.interruptionNotification) { [phone] note in
            guard let raw = note.userInfo?[AVAudioSessionInterruptionTypeKey] as? UInt,
                let type = AVAudioSession.InterruptionType(rawValue: raw)
            else { return }
            switch type {
            case .began: phone.interruptionBegan(.audio)
            case .ended: phone.interruptionEnded(.audio)
            @unknown default: break
            }
        }
        on(ProcessInfo.thermalStateDidChangeNotification) { [phone] _ in
            phone.thermalStateChanged(ProcessInfo.processInfo.thermalState)
        }
        phone.thermalStateChanged(ProcessInfo.processInfo.thermalState)
        monitor.start { [phone] kind in phone.pathChanged(kind) }
    }
}

/// The network the phone is on, from `NWPathMonitor`: for the status line, and a change of the
/// satisfied interface reconnects the uplink (section 9).
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
