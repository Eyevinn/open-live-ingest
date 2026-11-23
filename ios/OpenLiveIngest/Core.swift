import Foundation
import OliCore

/// The envelope's two halves on the Swift side (docs/specs/ios-app.md, section 8.2): commands go
/// out with an id, and each reply is matched back to its command; events go to `onEvent`. Used on
/// the main thread only. `transport` hands one JSON command on: to the Rust core (`RustCore`), or to
/// a test.
///
/// Only command and event names are logged, never a body: a command can carry a token, an API key
/// or a passphrase, and the `media` start event carries the uplink passphrase.
final class CoreChannel {
    typealias Reply = Result<JSONValue, CoreError>

    /// Every event the core sends, on the main thread, in order.
    var onEvent: ((CoreEvent) -> Void)?

    private let transport: (String) -> Void
    private var nextID: UInt32 = 1
    private var waiting: [UInt32: (Reply) -> Void] = [:]

    init(transport: @escaping (String) -> Void) {
        self.transport = transport
    }

    /// Sends `command`; `reply` is called with the core's answer.
    func send(_ command: CoreCommand, reply: ((Reply) -> Void)? = nil) {
        let id = nextID
        nextID = nextID == UInt32.max ? 1 : nextID + 1
        let json: String
        do {
            json = try command.json(id: id)
        } catch {
            AppLog.error("could not encode the \(command.name) command")
            reply?(.failure(CoreError(message: "The app could not send that. Restart the app.", action: nil)))
            return
        }
        if let reply { waiting[id] = reply }
        switch command {
        case .mediaReport, .phone: AppLog.debug("command \(command.name)")
        default: AppLog.info("command \(command.name)")
        }
        transport(json)
    }

    /// Sends `command` and waits for the reply.
    func call(_ command: CoreCommand) async -> Reply {
        await withCheckedContinuation { continuation in
            send(command) { continuation.resume(returning: $0) }
        }
    }

    /// One message from the core: a reply to a command waiting for it, or an event.
    func receive(_ text: String) {
        let message: CoreMessage
        do {
            message = try CoreMessage.parse(text)
        } catch {
            AppLog.error("the core sent a message that could not be read")
            return
        }
        if let name = message.event {
            let event: CoreEvent
            do {
                event = try CoreEvent.decode(name: name, data: message.data ?? .null)
            } catch {
                AppLog.error("the core's \(name) event could not be read")
                return
            }
            switch event {
            case .stats, .status: AppLog.debug("event \(event)")
            default: AppLog.info("event \(event)")
            }
            onEvent?(event)
            return
        }
        let reply: Reply = message.error.map { .failure($0) } ?? .success(message.ok ?? .null)
        if let id = message.id, let callback = waiting.removeValue(forKey: id) {
            callback(reply)
        } else if case .failure(let error) = reply {
            AppLog.warning("the core refused a message: \(error.message)")
        }
    }
}

/// The Rust core through its C ABI (mobile/include/oli.h; docs/specs/ios-app.md, section 8.1).
///
/// Deliveries arrive on a core thread with a string valid only during the call: it is copied
/// there and handed to the main thread (`DispatchQueue.main.async`, never `sync`, as oli.h asks).
/// `oli_core_free` in `deinit` stops what is live within 4 s; no delivery follows it.
final class RustCore {
    let channel: CoreChannel
    private var handle: OpaquePointer?
    /// What the C callback's context points at. Retained here for as long as the core runs.
    private let box: Box

    private final class Box {
        weak var channel: CoreChannel?
    }

    /// Starts the core: settings under `supportDirectory/open-live-ingest/`, its log in
    /// `logDirectory/app.log`. Nil when it cannot start.
    init?(supportDirectory: URL, logDirectory: URL) {
        let box = Box()
        let ctx = Unmanaged.passUnretained(box).toOpaque()
        let made: OpaquePointer? = supportDirectory.path.withCString { support in
            logDirectory.path.withCString { logs in
                oli_core_new(support, logs, { ctx, json in
                    guard let ctx, let json else { return }
                    let text = String(cString: json)
                    let box = Unmanaged<RustCore.Box>.fromOpaque(ctx).takeUnretainedValue()
                    DispatchQueue.main.async { box.channel?.receive(text) }
                }, ctx)
            }
        }
        guard let made else {
            AppLog.error("the core did not start")
            return nil
        }
        self.box = box
        self.handle = made
        channel = CoreChannel { json in
            json.withCString { oli_core_send(made, $0) }
        }
        box.channel = channel
    }

    deinit {
        if let handle { oli_core_free(handle) }
    }
}
