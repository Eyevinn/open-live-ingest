import XCTest
@testable import OpenLiveIngest

/// SRTSender against `srt-live-transmit` listeners on this Mac, which ios/run-tests.sh starts
/// before the tests (the simulator shares the Mac's network). The ports are the script's; keep
/// them in step. The listeners' passphrase is random per run, handed over as
/// `TEST_RUNNER_OLI_SRT_TEST_PASSPHRASE` (xcodebuild passes it on without the prefix).
final class SRTSenderTests: XCTestCase {
    private var passphrase = ""
    /// Forwards what it receives to `deliveredUDPPort`.
    static let deliveryPort: UInt16 = 4201
    static let deliveredUDPPort: UInt16 = 4301
    static let passphrasePort: UInt16 = 4202
    static let statsPort: UInt16 = 4203
    static let reconnectPort: UInt16 = 4204

    override func setUpWithError() throws {
        passphrase = try XCTUnwrap(
            ProcessInfo.processInfo.environment["OLI_SRT_TEST_PASSPHRASE"],
            "no listener passphrase: run the tests with ios/run-tests.sh")
    }

    private func options(port: UInt16, passphrase: String? = nil) -> SRTSenderOptions {
        SRTSenderOptions(
            host: "127.0.0.1", port: port, latencyMs: 200, passphrase: passphrase ?? self.passphrase, pbkeylen: 16,
            bitrateBitsPerSecond: 4_128_000)
    }

    /// `count` payload-sized chunks, each with its own byte pattern.
    private func chunks(_ count: Int, seed: Int = 0) -> [Data] {
        (0..<count).map { i in
            Data((0..<SRTSender.payloadSize).map { UInt8(truncatingIfNeeded: (seed + i) &* 31 &+ $0) })
        }
    }

    private func waitFor(_ what: String, timeout: TimeInterval, _ condition: () -> Bool) {
        let until = Date().addingTimeInterval(timeout)
        while !condition() && Date() < until {
            Thread.sleep(forTimeInterval: 0.05)
        }
        XCTAssertTrue(condition(), "\(what) within \(timeout) s")
    }

    func testDeliversBytesToAListener() throws {
        let receiver = try UDPReceiver(port: Self.deliveredUDPPort)
        let sender = SRTSender(options: options(port: Self.deliveryPort))
        defer { sender.stop() }
        try sender.connect()
        XCTAssertEqual(sender.state, .connected)

        var sent = Data()
        for chunk in chunks(100) {
            try sender.send(chunk)
            sent.append(chunk)
        }
        let received = receiver.receive(count: sent.count, timeout: 15)
        XCTAssertEqual(received.count, sent.count, "bytes the listener delivered")
        XCTAssertTrue(received == sent, "the listener delivered the bytes sent, in order")
    }

    func testAWrongPassphraseIsRejected() {
        let sender = SRTSender(options: options(port: Self.passphrasePort, passphrase: passphrase + "-but-wrong"))
        defer { sender.stop() }
        XCTAssertThrowsError(try sender.connect()) { error in
            XCTAssertTrue((error as? SRTSenderError)?.isWrongPassphrase == true, "rejected for the passphrase: \(error)")
        }
        XCTAssertNotEqual(sender.state, .connected)
        XCTAssertNil(sender.stats())
        XCTAssertThrowsError(try sender.send(Data(count: SRTSender.payloadSize))) { error in
            XCTAssertEqual(error as? SRTSenderError, .notConnected)
        }

        // The same listener takes the right one, so the rejection was the passphrase's.
        let right = SRTSender(options: options(port: Self.passphrasePort))
        defer { right.stop() }
        XCTAssertNoThrow(try right.connect())
    }

    func testStatsIncreaseWhileSending() throws {
        let sender = SRTSender(options: options(port: Self.statsPort))
        defer { sender.stop() }
        try sender.connect()

        for chunk in chunks(50) { try sender.send(chunk) }
        var first: SRTSenderStats?
        waitFor("the first 50 packets counted", timeout: 5) {
            first = sender.stats()
            return (first?.packetsSent ?? 0) >= 50
        }
        let before = try XCTUnwrap(first)
        XCTAssertGreaterThanOrEqual(before.bytesSent, UInt64(50 * SRTSender.payloadSize))

        for chunk in chunks(50, seed: 50) { try sender.send(chunk) }
        var second: SRTSenderStats?
        waitFor("the next 50 packets counted", timeout: 5) {
            second = sender.stats()
            return (second?.packetsSent ?? 0) >= before.packetsSent + 50
        }
        let after = try XCTUnwrap(second)
        XCTAssertGreaterThan(after.bytesSent, before.bytesSent)
        XCTAssertGreaterThan(after.packetsSent, before.packetsSent)
        XCTAssertGreaterThanOrEqual(after.rttMs, 0)
    }

    func testReconnectRestartsTheCountersFromZero() throws {
        let sender = SRTSender(options: options(port: Self.reconnectPort))
        defer { sender.stop() }
        try sender.connect()
        for chunk in chunks(50) { try sender.send(chunk) }
        waitFor("packets counted before the reconnect", timeout: 5) { (sender.stats()?.packetsSent ?? 0) >= 50 }

        sender.reconnect()
        // The listener takes a new caller once it has seen the old one go; the sender retries
        // every 2 s until then.
        waitFor("connected again", timeout: 30) { sender.state == .connected }
        let fresh = try XCTUnwrap(sender.stats())
        XCTAssertEqual(fresh.bytesSent, 0)
        XCTAssertEqual(fresh.packetsSent, 0)
        XCTAssertEqual(fresh.packetsSentDropped, 0)

        for chunk in chunks(10) { try sender.send(chunk) }
        waitFor("the new connection counting", timeout: 5) { (sender.stats()?.packetsSent ?? 0) >= 10 }
    }

    func testLinksTheRecordedLibsrt() {
        XCTAssertEqual(SRTSender.libraryVersion, ThirdPartyLicenses.libsrt.version)
    }

    func testStatsUseTheMediaReportNames() throws {
        let stats = SRTSenderStats(bytesSent: 1, packetsSent: 2, packetsSentDropped: 3, rttMs: 4.5, sendRateMbps: 6.5)
        let json = try XCTUnwrap(
            JSONSerialization.jsonObject(with: JSONEncoder().encode(stats)) as? [String: Double])
        XCTAssertEqual(
            json,
            ["bytes_sent": 1, "packets_sent": 2, "packets_sent_dropped": 3, "rtt_ms": 4.5, "send_rate_mbps": 6.5])
    }

    func testTheOptionsNeverShowThePassphrase() {
        let opts = options(port: Self.deliveryPort)
        XCTAssertEqual("\(opts)", "srt://127.0.0.1:4201?mode=caller&latency=200&passphrase=***&pbkeylen=16")
        XCTAssertFalse(String(reflecting: opts).contains(passphrase))
    }
}

/// A UDP socket on 127.0.0.1 collecting what the delivery listener forwards.
private final class UDPReceiver {
    private let fd: Int32

    init(port: UInt16) throws {
        fd = socket(AF_INET, SOCK_DGRAM, IPPROTO_UDP)
        guard fd >= 0 else { throw POSIXError(POSIXErrorCode(rawValue: errno) ?? .EIO) }
        var yes: Int32 = 1
        setsockopt(fd, SOL_SOCKET, SO_REUSEADDR, &yes, socklen_t(MemoryLayout<Int32>.size))
        var size: Int32 = 2 << 20
        setsockopt(fd, SOL_SOCKET, SO_RCVBUF, &size, socklen_t(MemoryLayout<Int32>.size))
        var timeout = timeval(tv_sec: 0, tv_usec: 200_000)
        setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &timeout, socklen_t(MemoryLayout<timeval>.size))
        var addr = sockaddr_in()
        addr.sin_len = UInt8(MemoryLayout<sockaddr_in>.size)
        addr.sin_family = sa_family_t(AF_INET)
        addr.sin_port = port.bigEndian
        addr.sin_addr.s_addr = inet_addr("127.0.0.1")
        let rc = withUnsafePointer(to: &addr) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                bind(fd, $0, socklen_t(MemoryLayout<sockaddr_in>.size))
            }
        }
        guard rc == 0 else {
            let err = errno
            close(fd)
            throw POSIXError(POSIXErrorCode(rawValue: err) ?? .EIO)
        }
    }

    deinit { close(fd) }

    func receive(count: Int, timeout: TimeInterval) -> Data {
        var out = Data()
        var buffer = [UInt8](repeating: 0, count: 65_536)
        let until = Date().addingTimeInterval(timeout)
        while out.count < count && Date() < until {
            let n = recv(fd, &buffer, buffer.count, 0)
            if n > 0 { out.append(contentsOf: buffer[0..<n]) }
        }
        return out
    }
}
