import Foundation

/// The fakes' control address (mobile/examples/fake_servers.rs), for the UI tests and the unit
/// tests alike: over plain HTTP/1.1 on a socket of its own (App Transport Security
/// covers URLSession, not this), one request per connection as the fakes serve them. IPv4 only:
/// the fakes listen on 127.0.0.1.
struct FakeControl {
    let host: String
    let port: UInt16

    struct Failed: Error, CustomStringConvertible {
        let description: String
    }

    init(base: String) throws {
        guard let url = URL(string: base), let host = url.host, let port = url.port, let port16 = UInt16(exactly: port)
        else { throw Failed(description: "not an http://host:port address: \(base)") }
        self.host = host
        self.port = port16
    }

    /// `method path`, the JSON object it answers with.
    func call(_ method: String, _ path: String) throws -> [String: Any] {
        let fd = socket(AF_INET, SOCK_STREAM, 0)
        guard fd >= 0 else { throw Failed(description: "no socket: errno \(errno)") }
        defer { close(fd) }
        var timeout = timeval(tv_sec: 10, tv_usec: 0)
        _ = setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &timeout, socklen_t(MemoryLayout<timeval>.size))
        _ = setsockopt(fd, SOL_SOCKET, SO_SNDTIMEO, &timeout, socklen_t(MemoryLayout<timeval>.size))
        var address = sockaddr_in()
        address.sin_len = UInt8(MemoryLayout<sockaddr_in>.size)
        address.sin_family = sa_family_t(AF_INET)
        address.sin_port = port.bigEndian
        address.sin_addr.s_addr = inet_addr(host)
        let connected = withUnsafePointer(to: &address) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                connect(fd, $0, socklen_t(MemoryLayout<sockaddr_in>.size))
            }
        }
        guard connected == 0 else { throw Failed(description: "\(method) \(path): cannot connect, errno \(errno)") }

        let request = Array(
            "\(method) \(path) HTTP/1.1\r\nHost: \(host):\(port)\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".utf8)
        var offset = 0
        while offset < request.count {
            let n = request[offset...].withUnsafeBytes { write(fd, $0.baseAddress, $0.count) }
            guard n > 0 else { throw Failed(description: "\(method) \(path): write failed, errno \(errno)") }
            offset += n
        }
        var received: [UInt8] = []
        var buffer = [UInt8](repeating: 0, count: 65_536)
        while true {
            let n = buffer.withUnsafeMutableBytes { read(fd, $0.baseAddress, $0.count) }
            if n <= 0 { break }
            received.append(contentsOf: buffer[0..<n])
        }

        let text = String(decoding: received, as: UTF8.self)
        guard text.hasPrefix("HTTP/1.1 200"), let split = text.range(of: "\r\n\r\n") else {
            throw Failed(description: "\(method) \(path): \(text)")
        }
        let body = Data(text[split.upperBound...].utf8)
        guard let object = try JSONSerialization.jsonObject(with: body) as? [String: Any] else {
            throw Failed(description: "\(method) \(path): not a JSON object: \(text)")
        }
        return object
    }
}
