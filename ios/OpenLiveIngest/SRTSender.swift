import Foundation
import SRT

/// What the SRT caller is handed: the core's `media.start` uplink (docs/specs/ios-app.md, section
/// 8.2) and the stream's CBR rate. The passphrase never appears in `description`.
struct SRTSenderOptions: Equatable {
    var host: String
    var port: UInt16
    /// `SRTO_STREAMID`, set only when present and not empty: through Open Live's SRT relay, what
    /// pairs this caller with the cloud Strom there. Up to 512 bytes (libsrt's `MAX_SID_LENGTH`).
    /// Every call of a sender, a reconnect too, sets the same one.
    var streamId: String?
    /// `SRTO_LATENCY`, which libsrt also applies as `SRTO_PEERLATENCY`.
    var latencyMs: Int32 = 200
    /// `SRTO_PASSPHRASE`, set only when present (10 to 79 characters).
    var passphrase: String?
    /// `SRTO_PBKEYLEN` (16, 24 or 32), set only when present.
    var pbkeylen: Int32?
    /// The stream's CBR rate, video plus audio, in bits per second: `SRTO_INPUTBW` (bytes per
    /// second) with `SRTO_MAXBW` 0 and `SRTO_OHEADBW` 25 %. Zero leaves the input rate to libsrt's
    /// estimate.
    var bitrateBitsPerSecond: Int64
    /// `SRTO_CONNTIMEO`, set only when present; libsrt's default (3 s for a caller) otherwise.
    var connectTimeoutMs: Int32?
}

extension SRTSenderOptions: CustomStringConvertible, CustomDebugStringConvertible {
    var description: String {
        var uri = "srt://\(host):\(port)?"
        if let streamId, !streamId.isEmpty { uri += "streamid=\(streamId)&" }
        uri += "mode=caller&latency=\(latencyMs)"
        if passphrase != nil { uri += "&passphrase=***" }
        if let pbkeylen { uri += "&pbkeylen=\(pbkeylen)" }
        return uri
    }

    var debugDescription: String { description }
}

/// libsrt's cumulative counters for one connection (`srt_bstats`), under the `media_report` stats
/// names (docs/specs/ios-app.md, section 8.2). A new connection starts them again from zero.
struct SRTSenderStats: Equatable, Encodable {
    /// `byteSentTotal`
    var bytesSent: UInt64
    /// `pktSentTotal`
    var packetsSent: UInt64
    /// `pktSndDropTotal`
    var packetsSentDropped: UInt64
    /// `msRTT`
    var rttMs: Double
    /// `mbpsSendRate`
    var sendRateMbps: Double

    enum CodingKeys: String, CodingKey {
        case bytesSent = "bytes_sent"
        case packetsSent = "packets_sent"
        case packetsSentDropped = "packets_sent_dropped"
        case rttMs = "rtt_ms"
        case sendRateMbps = "send_rate_mbps"
    }
}

extension SRTSenderStats {
    fileprivate init(_ perf: SRT_TRACEBSTATS) {
        bytesSent = perf.byteSentTotal
        packetsSent = UInt64(clamping: perf.pktSentTotal)
        packetsSentDropped = UInt64(clamping: perf.pktSndDropTotal)
        rttMs = perf.msRTT
        sendRateMbps = perf.mbpsSendRate
    }
}

enum SRTSenderError: Error, Equatable {
    case resolve(host: String, message: String)
    case socket(message: String)
    case option(name: String, message: String)
    /// The call failed. `rejectReason` is libsrt's `SRT_REJECT_REASON` (`srt_getrejectreason`).
    case connect(message: String, rejectReason: Int)
    case notConnected
    case send(message: String)

    /// The listener refused the passphrase (`SRT_REJ_BADSECRET`).
    var isWrongPassphrase: Bool {
        if case .connect(_, let reason) = self { return reason == Int(SRT_REJ_BADSECRET.rawValue) }
        return false
    }
}

/// MPEG-TS over SRT in caller mode to `host:port`, with libsrt (docs/specs/ios-app.md, sections 9
/// and 11; ADR-002 section 3); through Open Live's SRT relay, to the relay with the stream id.
///
/// `connect()` makes one call. `start()` keeps calling until connected: after a failed call (which
/// takes up to libsrt's connect timeout) it tries again every 2 s, and it starts again the same way
/// when sending finds the connection broken. `reconnect()` closes the socket and calls again, on
/// whatever network path is current; the counters of `stats()` restart from zero with the new
/// connection. `stop()` closes the socket and ends the retries. Every method may be called from
/// any thread.
final class SRTSender {
    enum State: Equatable {
        case stopped
        case connecting
        case connected
        /// The last call failed; the next one is `retryInterval` away.
        case waiting
    }

    /// Between a failed call and the next (docs/specs/ios-app.md, section 9).
    static let retryInterval: TimeInterval = 2
    /// Bytes per `srt_send`: seven 188-byte TS packets (`SRT_LIVE_DEF_PLSIZE`), also `SRTO_PAYLOADSIZE`.
    static let payloadSize = Int(SRT_LIVE_DEF_PLSIZE)
    /// `SRTO_OHEADBW`, percent over the input rate.
    static let overheadPercent: Int32 = 25

    /// libsrt's version, `major.minor.patch`.
    static var libraryVersion: String {
        let v = srt_getversion()
        return "\(v >> 16).\((v >> 8) & 0xff).\(v & 0xff)"
    }

    private static let startup: Void = {
        _ = srt_startup()
    }()

    let options: SRTSenderOptions
    /// Called on a background thread on every change of `state`.
    var onStateChange: ((State) -> Void)?

    private let lock = NSLock()
    private let queue = DispatchQueue(label: "com.eyevinn.open-live-ingest.srt-sender")
    private var srtSocket = SRT_INVALID_SOCK
    private var currentState = State.stopped
    private var retrying = false
    /// Bumped by every `start`, `reconnect` and `stop`, so an older retry loop ends.
    private var generation = 0

    init(options: SRTSenderOptions) {
        _ = Self.startup
        self.options = options
    }

    deinit {
        if srtSocket != SRT_INVALID_SOCK { srt_close(srtSocket) }
    }

    var state: State { locked { currentState } }

    /// One call with a new socket, replacing the current one. Blocks for up to libsrt's connect
    /// timeout. Throws `SRTSenderError.connect` when nobody answers or the listener rejects it.
    func connect() throws {
        let gen = locked { generation }
        try call(generation: gen)
    }

    /// Calls until connected, in the background, retrying every 2 s.
    func start() {
        let gen: Int = locked {
            generation += 1
            retrying = true
            return generation
        }
        queue.async { [weak self] in self?.retryLoop(generation: gen) }
    }

    /// Closes the socket and calls again (a network path change, OL-64f). The counters restart
    /// from zero.
    func reconnect() {
        closeSocket(newState: .connecting)
        start()
    }

    /// Closes the socket; no more calls.
    func stop() {
        let changed: Bool = locked {
            generation += 1
            retrying = false
            return closeLocked(newState: .stopped)
        }
        if changed { onStateChange?(.stopped) }
    }

    /// Sends `data` in `payloadSize` messages. Throws `notConnected` when there is no connection
    /// (the bytes are dropped); a broken connection is closed, and calling starts again if
    /// `start()` or `reconnect()` is in effect.
    @discardableResult
    func send(_ data: Data) throws -> Int {
        let sock: SRTSOCKET = locked { currentState == .connected ? srtSocket : SRT_INVALID_SOCK }
        guard sock != SRT_INVALID_SOCK else { throw SRTSenderError.notConnected }
        var failure: String?
        let sent = data.withUnsafeBytes { (raw: UnsafeRawBufferPointer) -> Int in
            guard let base = raw.baseAddress?.assumingMemoryBound(to: CChar.self) else { return 0 }
            var offset = 0
            while offset < raw.count {
                let n = min(Self.payloadSize, raw.count - offset)
                if srt_send(sock, base + offset, Int32(n)) == SRT_ERROR {
                    failure = Self.lastError()
                    break
                }
                offset += n
            }
            return offset
        }
        if let failure {
            connectionLost(sock)
            throw SRTSenderError.send(message: failure)
        }
        return sent
    }

    /// This connection's counters, or nil without one.
    func stats() -> SRTSenderStats? {
        let sock: SRTSOCKET = locked { currentState == .connected ? srtSocket : SRT_INVALID_SOCK }
        guard sock != SRT_INVALID_SOCK else { return nil }
        var perf = SRT_TRACEBSTATS()
        guard srt_bstats(sock, &perf, 0) != SRT_ERROR else { return nil }
        return SRTSenderStats(perf)
    }

    // MARK: - Calling

    private func retryLoop(generation gen: Int) {
        while isCurrent(gen) {
            do {
                try call(generation: gen)
                return
            } catch {
                guard isCurrent(gen) else { return }
                setState(.waiting, generation: gen)
                let until = Date().addingTimeInterval(Self.retryInterval)
                while Date() < until {
                    guard isCurrent(gen) else { return }
                    Thread.sleep(forTimeInterval: 0.1)
                }
            }
        }
    }

    private func call(generation gen: Int) throws {
        let sock = srt_create_socket()
        guard sock != SRT_INVALID_SOCK else { throw SRTSenderError.socket(message: Self.lastError()) }
        // The new socket becomes current before the blocking call, so stop() and reconnect()
        // can close it and end the call.
        let replaced: Bool = locked {
            guard gen == generation else { return false }
            if srtSocket != SRT_INVALID_SOCK { srt_close(srtSocket) }
            srtSocket = sock
            currentState = .connecting
            return true
        }
        guard replaced else {
            srt_close(sock)
            throw SRTSenderError.notConnected
        }
        onStateChange?(.connecting)
        do {
            try configure(sock)
            try connect(sock)
        } catch {
            let changed: Bool = locked {
                guard srtSocket == sock else { return false }
                srtSocket = SRT_INVALID_SOCK
                currentState = .stopped
                return true
            }
            srt_close(sock)
            if changed { onStateChange?(.stopped) }
            throw error
        }
        let connected: Bool = locked {
            guard srtSocket == sock else { return false }
            currentState = .connected
            return true
        }
        guard connected else { throw SRTSenderError.notConnected }
        onStateChange?(.connected)
    }

    /// The options of docs/specs/ios-app.md, section 11. `SRTO_TRANSTYPE` first: it resets the others.
    private func configure(_ sock: SRTSOCKET) throws {
        try set(sock, SRTO_TRANSTYPE, "SRTO_TRANSTYPE", SRTT_LIVE)
        try set(sock, SRTO_LATENCY, "SRTO_LATENCY", options.latencyMs)
        try set(sock, SRTO_PAYLOADSIZE, "SRTO_PAYLOADSIZE", Int32(Self.payloadSize))
        try set(sock, SRTO_MAXBW, "SRTO_MAXBW", Int64(0))
        if options.bitrateBitsPerSecond > 0 {
            try set(sock, SRTO_INPUTBW, "SRTO_INPUTBW", options.bitrateBitsPerSecond / 8)
        }
        try set(sock, SRTO_OHEADBW, "SRTO_OHEADBW", Self.overheadPercent)
        // A string option, its bytes without a terminator, and "pre": only before the call.
        if let streamId = options.streamId, !streamId.isEmpty {
            let bytes = Array(streamId.utf8)
            let rc = bytes.withUnsafeBytes { srt_setsockflag(sock, SRTO_STREAMID, $0.baseAddress, Int32($0.count)) }
            if rc == SRT_ERROR { throw SRTSenderError.option(name: "SRTO_STREAMID", message: Self.lastError()) }
        }
        if let passphrase = options.passphrase {
            let bytes = Array(passphrase.utf8)
            let rc = bytes.withUnsafeBytes { srt_setsockflag(sock, SRTO_PASSPHRASE, $0.baseAddress, Int32($0.count)) }
            if rc == SRT_ERROR { throw SRTSenderError.option(name: "SRTO_PASSPHRASE", message: Self.lastError()) }
        }
        if let pbkeylen = options.pbkeylen {
            try set(sock, SRTO_PBKEYLEN, "SRTO_PBKEYLEN", pbkeylen)
        }
        if let connectTimeoutMs = options.connectTimeoutMs {
            try set(sock, SRTO_CONNTIMEO, "SRTO_CONNTIMEO", connectTimeoutMs)
        }
    }

    private func set<T>(_ sock: SRTSOCKET, _ opt: SRT_SOCKOPT, _ name: String, _ value: T) throws {
        var v = value
        let rc = withUnsafeBytes(of: &v) { srt_setsockflag(sock, opt, $0.baseAddress, Int32($0.count)) }
        if rc == SRT_ERROR { throw SRTSenderError.option(name: name, message: Self.lastError()) }
    }

    private func connect(_ sock: SRTSOCKET) throws {
        var hints = addrinfo()
        hints.ai_family = AF_UNSPEC
        hints.ai_socktype = SOCK_DGRAM
        var result: UnsafeMutablePointer<addrinfo>?
        let rc = getaddrinfo(options.host, String(options.port), &hints, &result)
        guard rc == 0, let first = result else {
            throw SRTSenderError.resolve(host: options.host, message: String(cString: gai_strerror(rc)))
        }
        defer { freeaddrinfo(result) }
        if srt_connect(sock, first.pointee.ai_addr, Int32(first.pointee.ai_addrlen)) == SRT_ERROR {
            throw SRTSenderError.connect(message: Self.lastError(), rejectReason: Int(srt_getrejectreason(sock)))
        }
    }

    // MARK: - State

    private func connectionLost(_ sock: SRTSOCKET) {
        let restart: Bool = locked {
            guard srtSocket == sock else { return false }
            _ = closeLocked(newState: .stopped)
            return retrying
        }
        if restart {
            start()
        } else {
            onStateChange?(.stopped)
        }
    }

    private func closeSocket(newState: State) {
        let changed: Bool = locked { closeLocked(newState: newState) }
        if changed { onStateChange?(newState) }
    }

    /// With the lock held.
    private func closeLocked(newState: State) -> Bool {
        if srtSocket != SRT_INVALID_SOCK {
            srt_close(srtSocket)
            srtSocket = SRT_INVALID_SOCK
        }
        guard currentState != newState else { return false }
        currentState = newState
        return true
    }

    private func setState(_ state: State, generation gen: Int) {
        let changed: Bool = locked {
            guard gen == generation, currentState != state else { return false }
            currentState = state
            return true
        }
        if changed { onStateChange?(state) }
    }

    private func isCurrent(_ gen: Int) -> Bool {
        locked { gen == generation }
    }

    private func locked<T>(_ body: () throws -> T) rethrows -> T {
        lock.lock()
        defer { lock.unlock() }
        return try body()
    }

    private static func lastError() -> String {
        String(cString: srt_getlasterror_str())
    }
}
