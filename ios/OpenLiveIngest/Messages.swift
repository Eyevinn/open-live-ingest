import Foundation

// The messages between Swift and the Rust core (docs/specs/ios-app.md, section 8.2), as Codable
// types. The envelope is the desktop app's:
//   Swift -> core:  {"id": <u32>, "cmd": "<name>", "args": {...}}
//   core -> Swift:  {"id": <u32>, "ok": <value>} or {"id": <u32>, "error": {"message", "action"}}
//                   {"event": "<name>", "data": {...}}
// No type here prints a secret: commands describe themselves by name only, and the `media` start
// event by its input and action.

// MARK: - Any JSON

/// A JSON value, for the parts of a message whose type depends on the command or event.
enum JSONValue: Codable, Equatable {
    case null
    case bool(Bool)
    case number(Double)
    case string(String)
    case array([JSONValue])
    case object([String: JSONValue])

    init(from decoder: Decoder) throws {
        let container = try decoder.singleValueContainer()
        if container.decodeNil() {
            self = .null
        } else if let value = try? container.decode(Bool.self) {
            self = .bool(value)
        } else if let value = try? container.decode(Double.self) {
            self = .number(value)
        } else if let value = try? container.decode(String.self) {
            self = .string(value)
        } else if let value = try? container.decode([JSONValue].self) {
            self = .array(value)
        } else {
            self = .object(try container.decode([String: JSONValue].self))
        }
    }

    func encode(to encoder: Encoder) throws {
        var container = encoder.singleValueContainer()
        switch self {
        case .null: try container.encodeNil()
        case .bool(let value): try container.encode(value)
        case .number(let value): try container.encode(value)
        case .string(let value): try container.encode(value)
        case .array(let value): try container.encode(value)
        case .object(let value): try container.encode(value)
        }
    }

    subscript(key: String) -> JSONValue? {
        if case .object(let object) = self { return object[key] }
        return nil
    }

    var string: String? {
        if case .string(let value) = self { return value }
        return nil
    }

    /// This value as `T`.
    func decode<T: Decodable>(_ type: T.Type) throws -> T {
        try JSONDecoder().decode(T.self, from: JSONEncoder().encode(self))
    }
}

// MARK: - Commands

/// The network the phone is on, as the `phone` command and the `status` event name it.
enum NetworkKind: String, Codable, Equatable {
    case wifi, cellular, wired, other
    case noNetwork = "none"

    /// For the status line.
    var label: String {
        switch self {
        case .wifi: return "Wi-Fi"
        case .cellular: return "Cellular"
        case .wired: return "Wired"
        case .other: return "Other"
        case .noNetwork: return "No network"
        }
    }
}

/// `media_report`'s arguments: what the media of one input is doing, with libsrt's counters.
struct MediaReport: Encodable, Equatable {
    enum State: String, Encodable {
        case starting, running, failed, stopped
    }

    var input: String
    var state: State
    /// Why it failed: Swift's own words, never a credential.
    var reason: String?
    var stats: SRTSenderStats?
}

/// `phone`'s arguments (section 9).
struct PhoneReport: Encodable, Equatable {
    var foreground: Bool
    var interrupted: Bool
    var network: NetworkKind
}

/// The Advanced values `set_advanced` takes. Each one left nil is left as it is; an empty
/// passphrase and a `pbkeylen` of 0 clear it.
struct AdvancedValues: Encodable, Equatable {
    var name: String?
    var latencyMs: Int?
    var passphrase: String?
    var pbkeylen: Int?
    var bitrateKbps: Int?
    /// `WxH`.
    var resolution: String?
    /// `N/D`.
    var framerate: String?
    var logLevel: String?

    enum CodingKeys: String, CodingKey {
        case name, passphrase, pbkeylen, resolution, framerate
        case latencyMs = "latency_ms"
        case bitrateKbps = "bitrate_kbps"
        case logLevel = "log_level"
    }
}

extension AdvancedValues: CustomStringConvertible, CustomDebugStringConvertible {
    var description: String { "AdvancedValues(<redacted>)" }
    var debugDescription: String { description }
}

/// Every command Swift sends the core.
enum CoreCommand: Equatable {
    case state
    case oscSignIn
    case oscSignInCancel
    case oscPasteToken(token: String)
    case chooseInstance(url: String)
    case selfHosted(url: String, apiKey: String?)
    case setInput(device: String, audio: String?, label: String?)
    case setAdvanced(AdvancedValues)
    case goLive
    case stop
    case mediaReport(MediaReport)
    case phone(PhoneReport)
    case forgetSignIn

    /// The command's name, the one thing about it that is logged.
    var name: String {
        switch self {
        case .state: return "state"
        case .oscSignIn: return "osc_sign_in"
        case .oscSignInCancel: return "osc_sign_in_cancel"
        case .oscPasteToken: return "osc_paste_token"
        case .chooseInstance: return "choose_instance"
        case .selfHosted: return "self_hosted"
        case .setInput: return "set_input"
        case .setAdvanced: return "set_advanced"
        case .goLive: return "go_live"
        case .stop: return "stop"
        case .mediaReport: return "media_report"
        case .phone: return "phone"
        case .forgetSignIn: return "forget_sign_in"
        }
    }

    /// The command in its envelope, as the core reads it.
    func json(id: UInt32) throws -> String {
        let data = try JSONEncoder().encode(CommandEnvelope(id: id, command: self))
        return String(decoding: data, as: UTF8.self)
    }
}

extension CoreCommand: CustomStringConvertible, CustomDebugStringConvertible {
    /// The name only: a token, an API key or a passphrase may be in the arguments.
    var description: String { name }
    var debugDescription: String { name }
}

/// `{"id", "cmd", "args"}`; `args` is left out for a command that takes none.
struct CommandEnvelope: Encodable {
    let id: UInt32
    let command: CoreCommand

    private enum Keys: String, CodingKey {
        case id, cmd, args
    }

    private struct Token: Encodable {
        let token: String
    }

    private struct URLArg: Encodable {
        let url: String
    }

    private struct SelfHosted: Encodable {
        let url: String
        let apiKey: String?
        enum CodingKeys: String, CodingKey {
            case url
            case apiKey = "api_key"
        }
    }

    private struct SetInput: Encodable {
        let device: String
        let audio: String?
        let label: String?
    }

    func encode(to encoder: Encoder) throws {
        var container = encoder.container(keyedBy: Keys.self)
        try container.encode(id, forKey: .id)
        try container.encode(command.name, forKey: .cmd)
        switch command {
        case .state, .oscSignIn, .oscSignInCancel, .goLive, .stop, .forgetSignIn:
            break
        case .oscPasteToken(let token):
            try container.encode(Token(token: token), forKey: .args)
        case .chooseInstance(let url):
            try container.encode(URLArg(url: url), forKey: .args)
        case .selfHosted(let url, let apiKey):
            try container.encode(SelfHosted(url: url, apiKey: apiKey), forKey: .args)
        case .setInput(let device, let audio, let label):
            try container.encode(SetInput(device: device, audio: audio, label: label), forKey: .args)
        case .setAdvanced(let values):
            try container.encode(values, forKey: .args)
        case .mediaReport(let report):
            try container.encode(report, forKey: .args)
        case .phone(let report):
            try container.encode(report, forKey: .args)
        }
    }
}

// MARK: - Replies

/// An error reply's body: one line for the person, and an action id (none yet).
struct CoreError: Error, Decodable, Equatable {
    var message: String
    var action: String?
}

/// One message from the core: a reply (`id` with `ok` or `error`) or an event.
struct CoreMessage: Decodable {
    var id: UInt32?
    var ok: JSONValue?
    var error: CoreError?
    var event: String?
    var data: JSONValue?

    static func parse(_ text: String) throws -> CoreMessage {
        try JSONDecoder().decode(CoreMessage.self, from: Data(text.utf8))
    }
}

/// An Open Live instance in the signed-in workspace.
struct Instance: Codable, Equatable, Hashable {
    var name: String
    var url: String
}

/// One thing wrong with the settings.
struct Problem: Decodable, Equatable, Hashable {
    var setting: String?
    var message: String
}

/// Step 1, "Where to send", as the view model and the `where` event carry it. No secret.
struct WhereView: Decodable, Equatable {
    var mode: String
    var ready: Bool
    var url: String?
    var instance: String?
    var workspace: String?
    var instances: [Instance]
    var signedIn: Bool
    var apiKeySet: Bool
    /// "Sending to <instance> (<workspace>)" once ready.
    var summary: String?

    enum CodingKeys: String, CodingKey {
        case mode, ready, url, instance, workspace, instances, summary
        case signedIn = "signed_in"
        case apiKeySet = "api_key_set"
    }
}

/// Step 2, the one input.
struct InputView: Decodable, Equatable {
    var device: String
    var audio: String?
    var label: String?
    /// `<gateway.name> — <label>`, the source's name in Studio.
    var studioName: String

    enum CodingKeys: String, CodingKey {
        case device, audio, label
        case studioName = "studio_name"
    }
}

/// The Advanced values as saved. Whether a passphrase is set, never the passphrase.
struct AdvancedSettings: Decodable, Equatable {
    var name: String
    var latencyMs: Int
    var passphraseSet: Bool
    var pbkeylen: Int?
    var bitrateKbps: Int
    var resolution: String?
    var framerate: String?
    var logLevel: String
    var gatewayId: String?
    var gatewayTokenSet: Bool

    enum CodingKeys: String, CodingKey {
        case name, pbkeylen, resolution, framerate
        case latencyMs = "latency_ms"
        case passphraseSet = "passphrase_set"
        case bitrateKbps = "bitrate_kbps"
        case logLevel = "log_level"
        case gatewayId = "gateway_id"
        case gatewayTokenSet = "gateway_token_set"
    }
}

/// What `state`, `choose_instance`, `self_hosted`, `set_input`, `set_advanced` and
/// `forget_sign_in` answer with.
struct ViewModel: Decodable, Equatable {
    var version: String
    var live: Bool
    var network: NetworkKind?
    var whereToSend: WhereView?
    var input: InputView?
    var advanced: AdvancedSettings?
    var problems: [Problem]?
    /// The settings file could not be read.
    var problem: String?
    /// From `set_advanced`: saved, with something to know (a venue name already in use).
    var warnings: [Problem]?

    enum CodingKeys: String, CodingKey {
        case version, live, network, input, advanced, problems, problem, warnings
        case whereToSend = "where"
    }
}

/// What `osc_sign_in` and `osc_paste_token` answer with.
struct SignedIn: Decodable, Equatable {
    var workspace: String?
    var instances: [Instance]
    var whereToSend: WhereView?

    enum CodingKeys: String, CodingKey {
        case workspace, instances
        case whereToSend = "where"
    }
}

/// `set_advanced`'s answer when nothing was saved.
struct ProblemsOnly: Decodable, Equatable {
    var problems: [Problem]
}

// MARK: - Events

/// An input's state (the desktop's per-input rows).
struct InputRow: Decodable, Equatable {
    var input: String
    /// `registering`, `armed`, `connecting`, `live`, `failed`, `stopping`, `stopped`.
    var state: String
    var message: String?
}

struct SummaryRow: Decodable, Equatable {
    var input: String
    var port: Int
    var uplink: String
    var registered: String
}

struct Summary: Decodable, Equatable {
    var inputs: [SummaryRow]
}

struct StatsRow: Decodable, Equatable {
    var input: String
    var bitrateKbps: Int?
    var rttMs: Double?
    var dropped: Int?

    enum CodingKeys: String, CodingKey {
        case input, dropped
        case bitrateKbps = "bitrate_kbps"
        case rttMs = "rtt_ms"
    }
}

struct Stats: Decodable, Equatable {
    var inputs: [StatsRow]
}

/// A production holding the input's source.
struct Holder: Decodable, Equatable {
    var name: String
    var state: String
    /// The mixer inputs, `video_in_<n>`.
    var slots: [String]
}

struct LinkStatus: Decodable, Equatable {
    /// `not_sending`, `connecting`, `connected`.
    var state: String
    var droppedPct: Double?
    var rttMs: Double?
    /// The SRT connection's cumulative counters while connected (they start again from zero
    /// on a reconnect).
    var packetsSent: UInt64? = nil
    var packetsSentDropped: UInt64? = nil
    var bytesSent: UInt64? = nil

    enum CodingKeys: String, CodingKey {
        case state
        case droppedPct = "dropped_pct"
        case rttMs = "rtt_ms"
        case packetsSent = "packets_sent"
        case packetsSentDropped = "packets_sent_dropped"
        case bytesSent = "bytes_sent"
    }
}

struct StatusRow: Decodable, Equatable {
    var input: String
    /// Nil when not known yet; empty when no production holds the source.
    var productions: [Holder]?
    var link: LinkStatus
}

/// The status lines' parts.
struct Status: Decodable, Equatable {
    var inputs: [StatusRow]
    var network: NetworkKind
}

/// A session-wide notice for the banner.
struct Notice: Decodable, Equatable, Identifiable {
    /// `warning`, `connect_warning`, `sign_in_ends`, `slot_added_while_live`.
    var kind: String
    var message: String
    var input: String?

    var id: String { kind + "|" + (input ?? "") + "|" + message }
}

struct StoppingProgress: Decodable, Equatable {
    var done: Int
    var total: Int
}

struct Stopped: Decodable, Equatable {
    var leftBehind: [String]
    var error: CoreError?

    enum CodingKeys: String, CodingKey {
        case error
        case leftBehind = "left_behind"
    }
}

/// What the core asks of the media.
enum MediaCommand: Equatable {
    case start(MediaStart)
    case stop(input: String)
    case restart(input: String)

    var input: String {
        switch self {
        case .start(let start): return start.input
        case .stop(let input), .restart(let input): return input
        }
    }

    var action: String {
        switch self {
        case .start: return "start"
        case .stop: return "stop"
        case .restart: return "restart"
        }
    }
}

extension MediaCommand: CustomStringConvertible, CustomDebugStringConvertible {
    /// The input and the action, never the payload.
    var description: String { "media \(action) \(input)" }
    var debugDescription: String { description }
}

/// Every event the core sends.
enum CoreEvent: Equatable {
    case whereToSend(WhereView)
    case oscSignInPage(URL)
    case oscSignedIn
    case input(InputRow)
    case summary(Summary)
    case stats(Stats)
    case status(Status)
    case notice(Notice)
    case stopping(StoppingProgress)
    case stopped(Stopped)
    case media(MediaCommand)
    case fatal(message: String)
    /// An event this app does not know; ignored.
    case unknown(String)

    struct BadEvent: Error, Equatable {
        let name: String
    }

    private struct Page: Decodable {
        let url: String
    }

    private struct Fatal: Decodable {
        let message: String
    }

    private struct InputOnly: Decodable {
        let input: String
    }

    /// The event `name` with `data`.
    static func decode(name: String, data: JSONValue) throws -> CoreEvent {
        switch name {
        case "where": return .whereToSend(try data.decode(WhereView.self))
        case "osc_sign_in_page":
            guard let url = URL(string: try data.decode(Page.self).url) else { throw BadEvent(name: name) }
            return .oscSignInPage(url)
        case "osc_signed_in": return .oscSignedIn
        case "input": return .input(try data.decode(InputRow.self))
        case "summary": return .summary(try data.decode(Summary.self))
        case "stats": return .stats(try data.decode(Stats.self))
        case "status": return .status(try data.decode(Status.self))
        case "notice": return .notice(try data.decode(Notice.self))
        case "stopping": return .stopping(try data.decode(StoppingProgress.self))
        case "stopped": return .stopped(try data.decode(Stopped.self))
        case "fatal": return .fatal(message: try data.decode(Fatal.self).message)
        case "media":
            switch data["action"]?.string {
            case "start": return .media(.start(try data.decode(MediaStart.self)))
            case "stop": return .media(.stop(input: try data.decode(InputOnly.self).input))
            case "restart": return .media(.restart(input: try data.decode(InputOnly.self).input))
            default: throw BadEvent(name: name)
            }
        default: return .unknown(name)
        }
    }

    var name: String {
        switch self {
        case .whereToSend: return "where"
        case .oscSignInPage: return "osc_sign_in_page"
        case .oscSignedIn: return "osc_signed_in"
        case .input: return "input"
        case .summary: return "summary"
        case .stats: return "stats"
        case .status: return "status"
        case .notice: return "notice"
        case .stopping: return "stopping"
        case .stopped: return "stopped"
        case .media: return "media"
        case .fatal: return "fatal"
        case .unknown(let name): return name
        }
    }
}

extension CoreEvent: CustomStringConvertible, CustomDebugStringConvertible {
    /// The name, and for `media` the input and the action: never a payload.
    var description: String {
        if case .media(let media) = self { return media.description }
        return name
    }

    var debugDescription: String { description }
}
