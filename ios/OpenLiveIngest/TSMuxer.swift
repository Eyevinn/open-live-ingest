import Foundation

/// A single-program MPEG-TS muxer for H.264 and AAC-LC (docs/specs/ios-app.md, section 11;
/// ADR-002 section 3), what `builtin.mpegtssrt_input` reads on the cloud Strom.
///
/// - The PIDs are fixed: PMT `0x1000`, video `0x0100` (stream type 0x1B, H.264 Annex B, also the
///   PCR PID), audio `0x0101` (stream type 0x0F, AAC-LC in ADTS); program number 1.
/// - Nothing is written before the first IDR. Every access unit starts with an access unit
///   delimiter, and an IDR carries the SPS and PPS in front of it.
/// - PAT and PMT go out before every IDR and whenever `psiInterval` of stream time has passed,
///   which keeps them under 100 ms apart. A PCR goes out with every video PES and, as a
///   payload-free packet on the video PID, whenever `pcrInterval` has passed at an audio PES,
///   which keeps PCRs under 40 ms apart.
/// - Output is whole 188-byte packets in `chunkSize` (1316-byte, 7-packet) chunks, the SRT payload
///   size, handed to `onChunk`; `flush()` hands over what is left.
///
/// Timestamps are 90 kHz ticks from the stream's start. The PCR is the latest timestamp handed in;
/// PTS and DTS are written `delay` later, so a decoder has that much time to receive a frame.
/// Not thread safe: one caller at a time.
final class TSMuxer {
    static let packetSize = 188
    static let packetsPerChunk = 7
    static let chunkSize = packetSize * packetsPerChunk
    static let clockRate: UInt64 = 90_000

    static let patPID: UInt16 = 0x0000
    static let pmtPID: UInt16 = 0x1000
    static let videoPID: UInt16 = 0x0100
    static let audioPID: UInt16 = 0x0101
    static let programNumber: UInt16 = 1
    static let transportStreamID: UInt16 = 1
    static let videoStreamType: UInt8 = 0x1B
    static let audioStreamType: UInt8 = 0x0F

    /// Stream time between PCRs before an audio PES adds one (with audio every 21.3 ms, the gap
    /// stays under 15 + 21.3 ms).
    static let pcrInterval: UInt64 = 15 * 90
    /// Stream time between PAT/PMT pairs (with a PES at least every 33.4 ms at 30 fps, the gap
    /// stays under 50 + 33.4 ms).
    static let psiInterval: UInt64 = 50 * 90
    /// PTS and DTS are written this far after the PCR.
    static let defaultDelay: UInt64 = 300 * 90

    private static let ptsMask: UInt64 = (1 << 33) - 1

    /// Called with each 1316-byte chunk (and the shorter rest at `flush()`).
    var onChunk: ((Data) -> Void)?
    let delay: UInt64

    private var pending = Data()
    private var continuity: [UInt16: UInt8] = [:]
    private var clock: UInt64 = 0
    private var lastPCR: UInt64?
    private var lastPSI: UInt64?
    private var started = false

    init(delay: UInt64 = TSMuxer.defaultDelay) {
        self.delay = delay
    }

    /// Whether the first IDR has been written (everything before it is dropped).
    var hasStarted: Bool { started }

    /// One H.264 access unit: `nalUnits` without start codes (from VideoToolbox's AVCC output,
    /// with its length prefixes removed), `sps` and `pps` from the format description.
    func writeVideo(nalUnits: [Data], sps: Data?, pps: Data?, pts: UInt64, dts: UInt64? = nil, isKeyframe: Bool) {
        let idr = isKeyframe || nalUnits.contains { H264.nalType($0) == H264.NALType.idr }
        if !started {
            guard idr, sps != nil, pps != nil else { return }
            started = true
        }
        let decode = dts ?? pts
        advanceClock(to: decode)
        if idr || needsPSI() { writePSI() }

        var es = Data(H264.accessUnitDelimiter)
        let types = Set(nalUnits.map(H264.nalType))
        if idr {
            if !types.contains(H264.NALType.sps), let sps { es.append(contentsOf: H264.startCode); es.append(sps) }
            if !types.contains(H264.NALType.pps), let pps { es.append(contentsOf: H264.startCode); es.append(pps) }
        }
        for nal in nalUnits where H264.nalType(nal) != H264.NALType.accessUnitDelimiter {
            es.append(contentsOf: H264.startCode)
            es.append(nal)
        }
        let p = (pts &+ delay) & Self.ptsMask
        let d = dts.map { ($0 &+ delay) & Self.ptsMask }
        let pes = Self.pesPacket(streamID: 0xE0, payload: es, pts: p, dts: d == p ? nil : d, bounded: false)
        lastPCR = clock
        writePayload(pid: Self.videoPID, pes, pcr: clock, randomAccess: idr)
    }

    /// One raw AAC-LC frame (1024 samples, no ADTS header); the muxer frames it as ADTS.
    func writeAudio(aacFrame: Data, pts: UInt64, sampleRate: Int, channels: Int) {
        guard started, let header = ADTS.header(payloadLength: aacFrame.count, sampleRate: sampleRate, channels: channels)
        else { return }
        advanceClock(to: pts)
        if needsPSI() { writePSI() }
        if lastPCR.map({ clock &- $0 >= Self.pcrInterval }) ?? true { writePCROnly() }
        var es = Data(header)
        es.append(aacFrame)
        let pes = Self.pesPacket(streamID: 0xC0, payload: es, pts: (pts &+ delay) & Self.ptsMask, dts: nil, bounded: true)
        writePayload(pid: Self.audioPID, pes, pcr: nil, randomAccess: false)
    }

    /// Hands over the packets not yet in a full chunk.
    func flush() {
        guard !pending.isEmpty else { return }
        let rest = pending
        pending = Data()
        onChunk?(rest)
    }

    // MARK: - Clock and tables

    private func advanceClock(to ts: UInt64) {
        if ts > clock { clock = ts }
    }

    private func needsPSI() -> Bool {
        lastPSI.map { clock &- $0 >= Self.psiInterval } ?? true
    }

    private func writePSI() {
        lastPSI = clock
        writeSection(pid: Self.patPID, Self.patSection())
        writeSection(pid: Self.pmtPID, Self.pmtSection())
    }

    static func patSection() -> [UInt8] {
        var s: [UInt8] = [
            0x00,  // table_id: program_association_section
            0, 0,  // section_length, below
            UInt8(transportStreamID >> 8), UInt8(transportStreamID & 0xFF),
            0xC1,  // version 0, current_next_indicator 1
            0x00, 0x00,  // section_number, last_section_number
            UInt8(programNumber >> 8), UInt8(programNumber & 0xFF),
            0xE0 | UInt8(pmtPID >> 8), UInt8(pmtPID & 0xFF),
        ]
        return finishSection(&s)
    }

    static func pmtSection() -> [UInt8] {
        var s: [UInt8] = [
            0x02,  // table_id: TS_program_map_section
            0, 0,
            UInt8(programNumber >> 8), UInt8(programNumber & 0xFF),
            0xC1,
            0x00, 0x00,
            0xE0 | UInt8(videoPID >> 8), UInt8(videoPID & 0xFF),  // PCR_PID
            0xF0, 0x00,  // program_info_length 0
        ]
        for (type, pid) in [(videoStreamType, videoPID), (audioStreamType, audioPID)] {
            s += [type, 0xE0 | UInt8(pid >> 8), UInt8(pid & 0xFF), 0xF0, 0x00]
        }
        return finishSection(&s)
    }

    /// Fills in section_length and appends the CRC.
    private static func finishSection(_ s: inout [UInt8]) -> [UInt8] {
        let length = s.count - 3 + 4
        s[1] = 0xB0 | UInt8(length >> 8)
        s[2] = UInt8(length & 0xFF)
        let crc = CRC32MPEG2.checksum(s)
        s += [UInt8(crc >> 24), UInt8((crc >> 16) & 0xFF), UInt8((crc >> 8) & 0xFF), UInt8(crc & 0xFF)]
        return s
    }

    // MARK: - Packets

    private func nextContinuity(_ pid: UInt16) -> UInt8 {
        let cc = continuity[pid] ?? 0
        continuity[pid] = (cc + 1) & 0x0F
        return cc
    }

    private func writeSection(pid: UInt16, _ section: [UInt8]) {
        var p: [UInt8] = [0x47, 0x40 | UInt8(pid >> 8), UInt8(pid & 0xFF), 0x10 | nextContinuity(pid), 0x00]
        p += section
        p += [UInt8](repeating: 0xFF, count: Self.packetSize - p.count)
        emit(p)
    }

    /// A packet on the video PID with only an adaptation field carrying the PCR. Its continuity
    /// counter repeats the last one (it carries no payload).
    private func writePCROnly() {
        lastPCR = clock
        let pid = Self.videoPID
        let cc = ((continuity[pid] ?? 0) &+ 15) & 0x0F
        var p: [UInt8] = [0x47, UInt8(pid >> 8), UInt8(pid & 0xFF), 0x20 | cc, 183, 0x10]
        p += Self.pcrBytes(clock)
        p += [UInt8](repeating: 0xFF, count: Self.packetSize - p.count)
        emit(p)
    }

    private func writePayload(pid: UInt16, _ pes: Data, pcr: UInt64?, randomAccess: Bool) {
        var offset = pes.startIndex
        var first = true
        while offset < pes.endIndex {
            var adaptation: [UInt8] = []  // after the length byte
            if first && (pcr != nil || randomAccess) {
                var flags: UInt8 = 0
                if randomAccess { flags |= 0x40 }
                if pcr != nil { flags |= 0x10 }
                adaptation.append(flags)
                if let pcr { adaptation += Self.pcrBytes(pcr) }
            }
            var hasAdaptation = !adaptation.isEmpty
            let space = 184 - (hasAdaptation ? 1 + adaptation.count : 0)
            let n = min(space, pes.endIndex - offset)
            var stuffing = space - n
            if stuffing > 0 && !hasAdaptation {
                hasAdaptation = true
                stuffing -= 1  // the length byte
                if stuffing > 0 {
                    adaptation.append(0x00)
                    stuffing -= 1
                }
            }
            adaptation += [UInt8](repeating: 0xFF, count: stuffing)

            var p: [UInt8] = [
                0x47,
                (first ? 0x40 : 0x00) | UInt8(pid >> 8),
                UInt8(pid & 0xFF),
                (hasAdaptation ? 0x30 : 0x10) | nextContinuity(pid),
            ]
            p.reserveCapacity(Self.packetSize)
            if hasAdaptation {
                p.append(UInt8(adaptation.count))
                p += adaptation
            }
            p += pes[offset..<(offset + n)]
            emit(p)
            offset += n
            first = false
        }
    }

    private func emit(_ packet: [UInt8]) {
        precondition(packet.count == Self.packetSize, "a TS packet of \(packet.count) bytes")
        pending.append(contentsOf: packet)
        while pending.count >= Self.chunkSize {
            let chunk = pending.prefix(Self.chunkSize)
            pending = Data(pending.dropFirst(Self.chunkSize))
            onChunk?(Data(chunk))
        }
    }

    // MARK: - PES

    static func pesPacket(streamID: UInt8, payload: Data, pts: UInt64, dts: UInt64?, bounded: Bool) -> Data {
        let headerData = dts == nil ? 5 : 10
        var h: [UInt8] = [0x00, 0x00, 0x01, streamID, 0, 0, 0x84, dts == nil ? 0x80 : 0xC0, UInt8(headerData)]
        h += timestamp(pts, prefix: dts == nil ? 0x2 : 0x3)
        if let dts { h += timestamp(dts, prefix: 0x1) }
        // PES_packet_length counts from after the length field; zero (unbounded) is allowed for
        // video in a transport stream, and needed when a frame exceeds 65535 bytes.
        let length = 3 + headerData + payload.count
        if bounded && length <= 0xFFFF {
            h[4] = UInt8(length >> 8)
            h[5] = UInt8(length & 0xFF)
        }
        var out = Data(h)
        out.append(payload)
        return out
    }

    static func timestamp(_ ts: UInt64, prefix: UInt8) -> [UInt8] {
        let t = ts & ptsMask
        return [
            (prefix << 4) | UInt8((t >> 29) & 0x0E) | 0x01,
            UInt8((t >> 22) & 0xFF),
            UInt8((t >> 14) & 0xFE) | 0x01,
            UInt8((t >> 7) & 0xFF),
            UInt8((t << 1) & 0xFE) | 0x01,
        ]
    }

    /// program_clock_reference: the 33-bit base in 90 kHz, six reserved bits, a zero extension.
    static func pcrBytes(_ pcr: UInt64) -> [UInt8] {
        let base = pcr & ptsMask
        return [
            UInt8((base >> 25) & 0xFF),
            UInt8((base >> 17) & 0xFF),
            UInt8((base >> 9) & 0xFF),
            UInt8((base >> 1) & 0xFF),
            UInt8((base & 0x01) << 7) | 0x7E,
            0x00,
        ]
    }
}

/// H.264 NAL unit helpers (ITU-T H.264 section 7.3.1 and Annex B).
enum H264 {
    enum NALType {
        static let nonIDR: UInt8 = 1
        static let idr: UInt8 = 5
        static let sei: UInt8 = 6
        static let sps: UInt8 = 7
        static let pps: UInt8 = 8
        static let accessUnitDelimiter: UInt8 = 9
    }

    static let startCode: [UInt8] = [0x00, 0x00, 0x00, 0x01]
    /// An access unit delimiter, primary_pic_type 7 (any slice type).
    static let accessUnitDelimiter: [UInt8] = [0x00, 0x00, 0x00, 0x01, 0x09, 0xF0]

    static func nalType(_ nal: Data) -> UInt8 {
        (nal.first ?? 0) & 0x1F
    }

    /// The NAL units of a length-prefixed (AVCC) buffer, as VideoToolbox writes them. Stops at a
    /// truncated unit.
    static func nalUnits(avcc: Data, lengthSize: Int = 4) -> [Data] {
        var out: [Data] = []
        var i = avcc.startIndex
        while i + lengthSize <= avcc.endIndex {
            var length = 0
            for b in avcc[i..<(i + lengthSize)] { length = (length << 8) | Int(b) }
            i += lengthSize
            guard length > 0, i + length <= avcc.endIndex else { break }
            out.append(Data(avcc[i..<(i + length)]))
            i += length
        }
        return out
    }

    /// The NAL units of an Annex B byte stream (three- or four-byte start codes).
    static func nalUnits(annexB: Data) -> [Data] {
        let bytes = [UInt8](annexB)
        var starts: [(code: Int, payload: Int)] = []
        var i = 0
        while i + 2 < bytes.count {
            if bytes[i] == 0, bytes[i + 1] == 0, bytes[i + 2] == 1 {
                let code = (i > 0 && bytes[i - 1] == 0) ? i - 1 : i
                starts.append((code, i + 3))
                i += 3
            } else {
                i += 1
            }
        }
        return starts.enumerated().map { n, s in
            let end = n + 1 < starts.count ? starts[n + 1].code : bytes.count
            return Data(bytes[s.payload..<max(s.payload, end)])
        }
    }
}

/// ADTS framing for AAC-LC (ISO/IEC 13818-7 section 6.2, ISO/IEC 14496-3 section 1.A.2).
enum ADTS {
    static let headerSize = 7
    static let sampleRates = [96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350]

    /// The seven-byte header (no CRC) for one AAC-LC frame of `payloadLength` bytes, or nil for a
    /// rate ADTS cannot signal, a channel count outside 1 to 7, or a frame over 8191 bytes.
    static func header(payloadLength: Int, sampleRate: Int, channels: Int) -> [UInt8]? {
        guard let rateIndex = sampleRates.firstIndex(of: sampleRate), (1...7).contains(channels) else { return nil }
        let length = headerSize + payloadLength
        guard length <= 0x1FFF else { return nil }
        let profile: UInt8 = 1  // audio object type 2 (AAC LC) minus one
        let channelConfig = UInt8(channels)
        return [
            0xFF,
            0xF1,  // syncword, MPEG-4, layer 0, protection_absent
            (profile << 6) | (UInt8(rateIndex) << 2) | (channelConfig >> 2),
            ((channelConfig & 0x03) << 6) | UInt8((length >> 11) & 0x03),
            UInt8((length >> 3) & 0xFF),
            UInt8((length & 0x07) << 5) | 0x1F,  // buffer fullness 0x7FF (variable)
            0xFC,  // one raw data block
        ]
    }
}

/// CRC-32/MPEG-2 (polynomial 0x04C11DB7, initial 0xFFFFFFFF, no reflection, no final XOR), the
/// CRC of PSI sections.
enum CRC32MPEG2 {
    private static let table: [UInt32] = (0..<256).map { i -> UInt32 in
        var c = UInt32(i) << 24
        for _ in 0..<8 { c = (c & 0x8000_0000) != 0 ? (c << 1) ^ 0x04C1_1DB7 : c << 1 }
        return c
    }

    static func checksum<S: Sequence>(_ bytes: S) -> UInt32 where S.Element == UInt8 {
        var crc: UInt32 = 0xFFFF_FFFF
        for b in bytes { crc = (crc << 8) ^ table[Int(((crc >> 24) ^ UInt32(b)) & 0xFF)] }
        return crc
    }
}
