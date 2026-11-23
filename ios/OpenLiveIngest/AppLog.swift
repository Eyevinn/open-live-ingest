import Foundation
import os

/// The Swift side's own log lines (docs/specs/ios-app.md, section 8.2): command and event names,
/// what the media is doing, and failures. Never a message body: the `media` start event carries
/// the uplink passphrase, so only an input's id and the action are ever written about it.
///
/// Lines go to the unified log (Console.app, `log stream`). Tests watch every line with
/// `observe(_:)`, to check that a known secret appears in none of them. The Rust core keeps its own
/// `app.log`, which Share logs hands out.
enum AppLog {
    enum Level: String {
        case debug, info, warning, error
    }

    private static let logger = Logger(subsystem: "com.eyevinn.open-live-ingest.ios", category: "app")
    private static let lock = NSLock()
    private static var observers: [UUID: (Level, String) -> Void] = [:]

    static func debug(_ message: String) { write(.debug, message) }
    static func info(_ message: String) { write(.info, message) }
    static func warning(_ message: String) { write(.warning, message) }
    static func error(_ message: String) { write(.error, message) }

    /// Calls `observer` with every line from now on, on the thread that logs it, until
    /// `removeObserver(_:)`.
    @discardableResult
    static func observe(_ observer: @escaping (Level, String) -> Void) -> UUID {
        let id = UUID()
        lock.lock()
        observers[id] = observer
        lock.unlock()
        return id
    }

    static func removeObserver(_ id: UUID) {
        lock.lock()
        observers[id] = nil
        lock.unlock()
    }

    private static func write(_ level: Level, _ message: String) {
        switch level {
        case .debug: logger.debug("\(message, privacy: .public)")
        case .info: logger.info("\(message, privacy: .public)")
        case .warning: logger.warning("\(message, privacy: .public)")
        case .error: logger.error("\(message, privacy: .public)")
        }
        lock.lock()
        let current = Array(observers.values)
        lock.unlock()
        for observer in current { observer(level, message) }
    }
}
