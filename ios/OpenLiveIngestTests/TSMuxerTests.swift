import XCTest
@testable import OpenLiveIngest

/// TSMuxer against docs/specs/ios-app.md, section 11: one program, H.264 as 0x1B with SPS/PPS
/// before every IDR, AAC-LC as 0x0F in ADTS, PCR at least every 40 ms, PAT/PMT at least every
/// 100 ms, 188-byte packets with correct continuity counters, 1316-byte chunks.
final class TSMuxerTests: XCTestCase {
    static let fps = 30
    static let keyframeInterval = 30
    static let sampleRate = 48_000
    static let sps = Data([0x67, 0x64, 0x00, 0x1F, 0xAC, 0xD9, 0x40, 0x50])
    static let pps = Data([0x68, 0xEB, 0xE3, 0xCB, 0x22, 0xC0])

    /// `seconds` of 30 fps video (a keyframe every 30 frames, slices of `frameSize` bytes, the
    /// IDRs without SPS/PPS as VideoToolbox writes them) interleaved with 48 kHz AAC frames.
    private func mux(
        seconds: Double, frameSize: Int = 3_000, keyframeSize: Int = 20_000, startWithDelta: Bool = false,
        muxer: TSMuxer = TSMuxer()
    ) -> (chunks: [Data], stream: Data) {
        var chunks: [Data] = []
        muxer.onChunk = { chunks.append($0) }
        let frames = Int(seconds * Double(Self.fps))
        let audioFrames = Int(seconds * Double(Self.sampleRate) / 1024)
        var a = 0
        for f in 0..<frames {
            let pts = UInt64(f) * 90_000 / UInt64(Self.fps)
            while a < audioFrames, UInt64(a) * 1024 * 90_000 / UInt64(Self.sampleRate) <= pts {
                let apts = UInt64(a) * 1024 * 90_000 / UInt64(Self.sampleRate)
                muxer.writeAudio(aacFrame: Data(repeating: UInt8(a & 0x7F), count: 380), pts: apts,
                                 sampleRate: Self.sampleRate, channels: 2)
                a += 1
            }
            let key = (f + (startWithDelta ? 1 : 0)) % Self.keyframeInterval == 0
            let slice = Data([key ? 0x65 : 0x41] + [UInt8](repeating: UInt8(f & 0x7F) | 0x01, count: key ? keyframeSize : frameSize))
            muxer.writeVideo(nalUnits: [slice], sps: Self.sps, pps: Self.pps, pts: pts, isKeyframe: key)
        }
        muxer.flush()
        return (chunks, chunks.reduce(into: Data()) { $0.append($1) })
    }

    // MARK: - Packets and chunks

    func testPacketsAre188BytesInChunksOf1316() throws {
        let (chunks, stream) = mux(seconds: 2)
        XCTAssertGreaterThan(chunks.count, 10)
        for chunk in chunks.dropLast() { XCTAssertEqual(chunk.count, 1316) }
        let last = try XCTUnwrap(chunks.last)
        XCTAssertTrue(last.count > 0 && last.count <= 1316 && last.count % 188 == 0, "last chunk \(last.count)")
        XCTAssertEqual(stream.count % 188, 0)
        let packets = TSPacket.parse(stream)
        XCTAssertEqual(packets.count, stream.count / 188)
        XCTAssertTrue(packets.allSatisfy { $0.syncByte == 0x47 })
        XCTAssertEqual(Set(packets.map(\.pid)), [0x0000, 0x1000, 0x0100, 0x0101])
    }

    func testContinuityCountersIncrementPerPID() {
        let packets = TSPacket.parse(mux(seconds: 3).stream)
        var last: [UInt16: UInt8] = [:]
        var payloadPackets = 0
        for p in packets {
            if let previous = last[p.pid] {
                let want = p.hasPayload ? (previous + 1) & 0x0F : previous
                XCTAssertEqual(p.continuity, want, "PID \(p.pid) after \(previous)")
            } else {
                XCTAssertTrue(p.hasPayload)
                XCTAssertEqual(p.continuity, 0, "PID \(p.pid) starts at 0")
            }
            last[p.pid] = p.continuity
            if p.hasPayload { payloadPackets += 1 }
        }
        XCTAssertGreaterThan(payloadPackets, 16 * 4, "the counters wrapped")
    }

    // MARK: - PAT and PMT

    func testPATAndPMTDescribeOneProgramWithTheDocumentedPIDs() throws {
        let packets = TSPacket.parse(mux(seconds: 1).stream)
        let pat = try XCTUnwrap(packets.first { $0.pid == 0 }?.section)
        XCTAssertEqual(pat[0], 0x00)
        XCTAssertTrue(CRC32MPEG2.checksum(pat) == 0, "PAT CRC")
        let programs = stride(from: 8, to: pat.count - 4, by: 4).map { (i: Int) -> (number: UInt16, pid: UInt16) in
            (number: be16(pat[i], pat[i + 1]), pid: be16(pat[i + 2] & 0x1F, pat[i + 3]))
        }
        XCTAssertEqual(programs.count, 1, "one program")
        XCTAssertEqual(programs.first?.number, 1)
        XCTAssertEqual(programs.first?.pid, TSMuxer.pmtPID)

        let pmt = try XCTUnwrap(packets.first { $0.pid == TSMuxer.pmtPID }?.section)
        XCTAssertEqual(pmt[0], 0x02)
        XCTAssertTrue(CRC32MPEG2.checksum(pmt) == 0, "PMT CRC")
        XCTAssertEqual(be16(pmt[8] & 0x1F, pmt[9]), TSMuxer.videoPID, "PCR PID")
        let infoLength = Int(be16(pmt[10] & 0x0F, pmt[11]))
        var streams: [(type: UInt8, pid: UInt16)] = []
        var i = 12 + infoLength
        while i + 5 <= pmt.count - 4 {
            streams.append((pmt[i], be16(pmt[i + 1] & 0x1F, pmt[i + 2])))
            i += 5 + Int(be16(pmt[i + 3] & 0x0F, pmt[i + 4]))
        }
        XCTAssertEqual(streams.map(\.type), [0x1B, 0x0F])
        XCTAssertEqual(streams.map(\.pid), [0x0100, 0x0101])
    }

    func testPATAndPMTAtLeastEvery100ms() throws {
        let packets = TSPacket.parse(mux(seconds: 10).stream)
        // Stream time at each PAT: the latest PCR before it.
        var pcr: UInt64?
        var patTimes: [UInt64] = []
        for (n, p) in packets.enumerated() {
            if let value = p.pcr { pcr = value }
            if p.pid == 0 {
                patTimes.append(pcr ?? 0)
                XCTAssertEqual(packets[n + 1].pid, TSMuxer.pmtPID, "the PMT follows the PAT")
            }
        }
        XCTAssertGreaterThan(patTimes.count, 100)
        let gaps = zip(patTimes.dropFirst(), patTimes).map { $0 - $1 }
        let worst = try XCTUnwrap(gaps.max())
        XCTAssertLessThanOrEqual(worst, 100 * 90, "largest PAT/PMT gap \(Double(worst) / 90) ms")
    }

    // MARK: - PCR

    func testPCROnTheVideoPIDAtLeastEvery40ms() throws {
        let packets = TSPacket.parse(mux(seconds: 10).stream)
        let pcrs = packets.compactMap { p in p.pcr.map { (pid: p.pid, value: $0) } }
        XCTAssertGreaterThan(pcrs.count, 300)
        XCTAssertTrue(pcrs.allSatisfy { $0.pid == TSMuxer.videoPID }, "PCR only on the video PID")
        let gaps = zip(pcrs.dropFirst(), pcrs).map { Int64($0.value) - Int64($1.value) }
        XCTAssertTrue(gaps.allSatisfy { $0 >= 0 }, "the PCR never goes back")
        let worst = try XCTUnwrap(gaps.max())
        XCTAssertLessThanOrEqual(worst, 40 * 90, "largest PCR gap \(Double(worst) / 90) ms")
    }

    func testPTSIsTheInputPlusTheDelayAndNeverBeforeThePCR() throws {
        let muxer = TSMuxer(delay: 9_000)
        let packets = TSPacket.parse(mux(seconds: 2, muxer: muxer).stream)
        let video = TSPacket.pes(packets, pid: TSMuxer.videoPID)
        XCTAssertEqual(video.count, 60)
        for (n, pes) in video.enumerated() {
            XCTAssertEqual(pes.streamID, 0xE0)
            XCTAssertEqual(pes.pts, UInt64(n) * 3_000 + 9_000)
        }
        var pcr: UInt64 = 0
        for p in packets {
            if let value = p.pcr { pcr = value }
            if p.payloadUnitStart, p.pid != 0, p.pid != TSMuxer.pmtPID, let pes = PES(p.payload) {
                XCTAssertGreaterThanOrEqual(pes.pts ?? 0, pcr, "PTS at or after the PCR")
            }
        }
    }

    // MARK: - H.264

    func testSPSAndPPSBeforeEveryIDR() throws {
        let packets = TSPacket.parse(mux(seconds: 4).stream)
        let video = TSPacket.pes(packets, pid: TSMuxer.videoPID)
        XCTAssertEqual(video.count, 120)
        var idrs = 0
        for (n, pes) in video.enumerated() {
            let types = H264.nalUnits(annexB: pes.payload).map(H264.nalType)
            XCTAssertEqual(types.first, H264.NALType.accessUnitDelimiter, "frame \(n) starts with an AUD")
            if types.contains(H264.NALType.idr) {
                idrs += 1
                XCTAssertEqual(types, [9, 7, 8, 5], "frame \(n): AUD, SPS, PPS, IDR")
                XCTAssertEqual(H264.nalUnits(annexB: pes.payload)[1], Self.sps)
                XCTAssertEqual(H264.nalUnits(annexB: pes.payload)[2], Self.pps)
            } else {
                XCTAssertEqual(types, [9, 1], "frame \(n)")
            }
        }
        XCTAssertEqual(idrs, 4)
    }

    func testAnIDRCarryingItsParameterSetsIsNotGivenThemTwice() {
        var out = Data()
        let muxer = TSMuxer()
        muxer.onChunk = { out.append($0) }
        muxer.writeVideo(nalUnits: [Self.sps, Self.pps, Data([0x65, 0x88, 0x84])], sps: Self.sps, pps: Self.pps,
                         pts: 0, isKeyframe: true)
        muxer.flush()
        let pes = TSPacket.pes(TSPacket.parse(out), pid: TSMuxer.videoPID)
        XCTAssertEqual(pes.count, 1)
        XCTAssertEqual(H264.nalUnits(annexB: pes[0].payload).map(H264.nalType), [9, 7, 8, 5])
    }

    func testTheKeyframeIsARandomAccessPointAfterPATAndPMT() {
        let packets = TSPacket.parse(mux(seconds: 3).stream)
        var keyframes = 0
        for (n, p) in packets.enumerated() where p.pid == TSMuxer.videoPID && p.payloadUnitStart {
            let isIDR = PES(p.payload).map { H264.nalUnits(annexB: $0.payload).contains { H264.nalType($0) == 5 } } ?? false
            XCTAssertEqual(p.randomAccess, isIDR, "random_access_indicator on packet \(n)")
            if isIDR {
                keyframes += 1
                XCTAssertEqual(packets[n - 2].pid, 0, "PAT before the IDR")
                XCTAssertEqual(packets[n - 1].pid, TSMuxer.pmtPID, "PMT before the IDR")
            }
        }
        XCTAssertEqual(keyframes, 3)
    }

    func testNothingIsWrittenBeforeTheFirstIDR() throws {
        let (_, stream) = mux(seconds: 2, startWithDelta: true)
        let packets = TSPacket.parse(stream)
        XCTAssertEqual(packets.first?.pid, 0, "the stream starts with the PAT")
        let video = TSPacket.pes(packets, pid: TSMuxer.videoPID)
        let first = try XCTUnwrap(video.first)
        XCTAssertTrue(H264.nalUnits(annexB: first.payload).contains { H264.nalType($0) == 5 })
        XCTAssertEqual(video.count, 31, "frames 29 to 59")
        let audio = TSPacket.pes(packets, pid: TSMuxer.audioPID)
        XCTAssertGreaterThanOrEqual(try XCTUnwrap(audio.first?.pts), first.pts ?? 0, "no audio before the video")
    }

    func testAFrameOver64KiBGetsAnUnboundedPES() {
        let packets = TSPacket.parse(mux(seconds: 1, keyframeSize: 100_000).stream)
        let video = TSPacket.pes(packets, pid: TSMuxer.videoPID)
        XCTAssertEqual(video.first?.declaredLength, 0)
        XCTAssertGreaterThan(video.first?.payload.count ?? 0, 100_000)
    }

    func testNALUnitsFromAVCC() {
        let avcc = Data([0, 0, 0, 3, 0x09, 0xF0, 0xAA, 0, 0, 0, 2, 0x65, 0x88])
        XCTAssertEqual(H264.nalUnits(avcc: avcc), [Data([0x09, 0xF0, 0xAA]), Data([0x65, 0x88])])
        XCTAssertEqual(H264.nalUnits(avcc: avcc.prefix(12)), [Data([0x09, 0xF0, 0xAA])], "truncated unit dropped")
        XCTAssertEqual(H264.nalUnits(avcc: Data([0, 2, 0x41, 0x9A]), lengthSize: 2), [Data([0x41, 0x9A])])
    }

    // MARK: - AAC

    func testADTSHeaderForAACLC48kHzStereo() throws {
        let header = try XCTUnwrap(ADTS.header(payloadLength: 371, sampleRate: 48_000, channels: 2))
        // syncword FFF, MPEG-4, layer 0, no CRC; profile LC (1), index 3 (48 kHz), channels 2,
        // frame length 378, buffer fullness 0x7FF, one raw data block.
        XCTAssertEqual(header, [0xFF, 0xF1, 0x4C, 0x80, 0x2F, 0x5F, 0xFC])
        XCTAssertNil(ADTS.header(payloadLength: 100, sampleRate: 47_000, channels: 2))
        XCTAssertNil(ADTS.header(payloadLength: 100, sampleRate: 48_000, channels: 0))
        XCTAssertNil(ADTS.header(payloadLength: 8_190, sampleRate: 48_000, channels: 2))
    }

    func testEveryAudioPESIsOneADTSFrame() throws {
        let packets = TSPacket.parse(mux(seconds: 2).stream)
        let audio = TSPacket.pes(packets, pid: TSMuxer.audioPID)
        XCTAssertGreaterThan(audio.count, 80)
        var previous: UInt64?
        for pes in audio {
            XCTAssertEqual(pes.streamID, 0xC0)
            XCTAssertEqual(pes.declaredLength, pes.payload.count + 3 + 5, "PES_packet_length")
            let b = [UInt8](pes.payload)
            XCTAssertEqual(b[0], 0xFF)
            XCTAssertEqual(b[1] & 0xF6, 0xF0, "sync, MPEG-4, layer 0")
            XCTAssertEqual(b[1] & 0x01, 1, "no CRC")
            XCTAssertEqual(b[2] >> 6, 1, "AAC LC")
            XCTAssertEqual((b[2] >> 2) & 0x0F, 3, "48 kHz")
            XCTAssertEqual((b[2] & 0x01) << 2 | b[3] >> 6, 2, "stereo")
            let length: Int = (Int(b[3] & 0x03) << 11) + (Int(b[4]) << 3) + (Int(b[5]) >> 5)
            XCTAssertEqual(length, b.count, "aac_frame_length is the whole frame")
            XCTAssertEqual(b.count, 7 + 380)
            let pts = try XCTUnwrap(pes.pts)
            if let previous { XCTAssertEqual(pts - previous, 1920, "1024 samples at 48 kHz") }
            previous = pts
        }
    }

    func testCRC32MPEG2() {
        XCTAssertEqual(CRC32MPEG2.checksum(Array("123456789".utf8)), 0x0376_E6E7)
    }
}

// MARK: - A small TS reader for the tests

func be16(_ high: UInt8, _ low: UInt8) -> UInt16 {
    UInt16(high) << 8 | UInt16(low)
}

struct TSPacket {
    let syncByte: UInt8
    let payloadUnitStart: Bool
    let pid: UInt16
    let continuity: UInt8
    let hasPayload: Bool
    let randomAccess: Bool
    let pcr: UInt64?
    let payload: Data

    init(_ b: ArraySlice<UInt8>) {
        let s = b.startIndex
        syncByte = b[s]
        payloadUnitStart = b[s + 1] & 0x40 != 0
        pid = be16(b[s + 1] & 0x1F, b[s + 2])
        let control = (b[s + 3] >> 4) & 0x03
        continuity = b[s + 3] & 0x0F
        hasPayload = control & 0x01 != 0
        var offset = s + 4
        var ra = false
        var clock: UInt64?
        if control & 0x02 != 0 {
            let length = Int(b[offset])
            if length > 0 {
                let flags = b[offset + 1]
                ra = flags & 0x40 != 0
                if flags & 0x10 != 0 {
                    let p = offset + 2
                    var base: UInt64 = 0
                    for k in 0..<4 { base = base << 8 | UInt64(b[p + k]) }
                    clock = base << 1 | UInt64(b[p + 4] >> 7)
                }
            }
            offset += 1 + length
        }
        randomAccess = ra
        pcr = clock
        payload = hasPayload ? Data(b[offset..<(s + 188)]) : Data()
    }

    /// A PSI section starting in this packet (pointer field skipped).
    var section: [UInt8]? {
        guard payloadUnitStart, let pointer = payload.first else { return nil }
        let bytes = [UInt8](payload.dropFirst(1 + Int(pointer)))
        guard bytes.count >= 3 else { return nil }
        let length = Int(be16(bytes[1] & 0x0F, bytes[2]))
        return Array(bytes.prefix(3 + length))
    }

    static func parse(_ data: Data) -> [TSPacket] {
        let bytes = [UInt8](data)
        return stride(from: 0, to: bytes.count - 187, by: 188).map { TSPacket(bytes[$0..<($0 + 188)]) }
    }

    /// The PES packets of `pid`, reassembled.
    static func pes(_ packets: [TSPacket], pid: UInt16) -> [PES] {
        var out: [PES] = []
        var current: Data?
        for p in packets where p.pid == pid && p.hasPayload {
            if p.payloadUnitStart {
                if let current, let pes = PES(current) { out.append(pes) }
                current = p.payload
            } else {
                current?.append(p.payload)
            }
        }
        if let current, let pes = PES(current) { out.append(pes) }
        return out
    }
}

struct PES {
    let streamID: UInt8
    let declaredLength: Int
    let pts: UInt64?
    let payload: Data

    init?(_ data: Data) {
        let b = [UInt8](data)
        guard b.count >= 9, b[0] == 0, b[1] == 0, b[2] == 1 else { return nil }
        streamID = b[3]
        declaredLength = Int(be16(b[4], b[5]))
        let headerLength = Int(b[8])
        if b[7] & 0x80 != 0 {
            let p = 9
            let high = UInt64(b[p] & 0x0E) << 29
            let middle = UInt64(b[p + 1]) << 22 | UInt64(b[p + 2] & 0xFE) << 14
            let low = UInt64(b[p + 3]) << 7 | UInt64(b[p + 4] >> 1)
            pts = high | middle | low
        } else {
            pts = nil
        }
        payload = Data(b[(9 + headerLength)...])
    }
}
