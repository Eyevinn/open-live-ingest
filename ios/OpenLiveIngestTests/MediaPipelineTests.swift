import AVFoundation
import XCTest
@testable import OpenLiveIngest

/// The test pattern through VideoToolbox, AudioToolbox and TSMuxer on the simulator, at section
/// 11's defaults (docs/specs/ios-app.md). The 10 s recording and the same stream received over SRT
/// are written to `OLI_RECORDING_DIR` (ios/run-tests.sh hands it over as
/// `TEST_RUNNER_OLI_RECORDING_DIR`), where the script checks them with ffprobe and
/// ios/check-stream.py: the codecs, profile, size and rate, stream types and PIDs, the keyframe
/// cadence, no B-frames, and the peak-to-average bitrate.
final class MediaPipelineTests: XCTestCase {
    static let seconds = 10
    /// Forwards what it receives to `deliveredUDPPort`; the listener is ios/run-tests.sh's.
    static let encodedPort: UInt16 = 4205
    static let deliveredUDPPort: UInt16 = 4305

    struct NoPicture: Error {
        let frame: Int
    }

    struct Recording {
        let chunks: [Data]
        let stream: Data
        let counts: MediaPipeline.Counts
        let rateControl: VideoEncoder.RateControl
    }

    /// Encoded once, faster than real time, with the pattern's own timestamps.
    static let recording: Result<Recording, Error> = Result {
        let video = VideoSettings.defaults
        let audio = AudioSettings.defaults
        let pattern = try TestPatternSource(video: video, audio: audio)
        var chunks: [Data] = []
        let pipeline = try MediaPipeline(video: video, audio: audio) { chunks.append($0) }
        pipeline.synchronousVideo = true
        let frames = MediaPipelineTests.seconds * Int(video.fps)
        for frame in 0..<frames {
            guard let picture = pattern.pixelBuffer(frame: frame) else { throw NoPicture(frame: frame) }
            pipeline.appendVideo(picture, time: CMTime(value: CMTimeValue(frame), timescale: CMTimeScale(video.fps)))
            let first = pattern.audioSamples(beforeFrame: frame)
            let next = pattern.audioSamples(beforeFrame: frame + 1)
            if let sound = pattern.audioBuffer(startSample: first, frameCount: Int(next - first)) {
                pipeline.appendAudio(sound, time: CMTime(value: first, timescale: CMTimeScale(audio.sampleRate)))
            }
        }
        pipeline.finish()
        let stream = chunks.reduce(into: Data()) { $0.append($1) }
        return Recording(chunks: chunks, stream: stream, counts: pipeline.statistics, rateControl: pipeline.rateControl)
    }

    /// Where the files go: `OLI_RECORDING_DIR` when set, and the test host's temporary directory.
    private func save(_ data: Data, as name: String) throws {
        var directories = [URL(fileURLWithPath: NSTemporaryDirectory())]
        if let dir = ProcessInfo.processInfo.environment["OLI_RECORDING_DIR"], !dir.isEmpty {
            directories.insert(URL(fileURLWithPath: dir), at: 0)
        }
        for dir in directories {
            try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
            try data.write(to: dir.appendingPathComponent(name))
            print("wrote \(data.count) bytes to \(dir.appendingPathComponent(name).path)")
        }
        let attachment = XCTAttachment(data: data, uniformTypeIdentifier: "public.mpeg-2-transport-stream")
        attachment.name = name
        attachment.lifetime = .keepAlways
        add(attachment)
    }

    func testTheDefaultsAreSection11s() {
        XCTAssertEqual(
            VideoSettings.defaults,
            VideoSettings(width: 1280, height: 720, fps: 30, bitrateKbps: 4000, keyframeInterval: 30))
        XCTAssertEqual(AudioSettings.defaults, AudioSettings(sampleRate: 48_000, channels: 2, bitrateKbps: 128))
    }

    func testTheMediaStartEventDecodes() throws {
        let json = """
            {"input": "in-1", "name": "Venue - Phone", "action": "start", "device": null,
             "microphone": null, "test_pattern": true,
             "uplink": {"host": "cloud.example.com", "port": 10501, "latency_ms": 200,
                        "passphrase": "0123456789abcdef", "pbkeylen": 16},
             "video": {"width": 1280, "height": 720, "fps": 30.0, "bitrate_kbps": 4000, "keyframe_interval": 30},
             "audio": {"sample_rate": 48000, "channels": 2, "bitrate_kbps": 128}}
            """
        let start = try JSONDecoder().decode(MediaStart.self, from: Data(json.utf8))
        XCTAssertEqual(start.video, .defaults)
        XCTAssertEqual(start.audio, .defaults)
        XCTAssertTrue(start.usesTestPattern)
        XCTAssertEqual(start.senderOptions.bitrateBitsPerSecond, 4_128_000)
        XCTAssertEqual(start.senderOptions.pbkeylen, 16)
        XCTAssertFalse("\(start)".contains("0123456789abcdef"), "no passphrase in the description")
        XCTAssertFalse(String(reflecting: start).contains("0123456789abcdef"))
    }

    func testTheLandscapeAngle() {
        XCTAssertEqual(Capture.landscapeAngle(forHorizonLevel: 0, current: 180), 0)
        XCTAssertEqual(Capture.landscapeAngle(forHorizonLevel: 180, current: 0), 180)
        XCTAssertEqual(Capture.landscapeAngle(forHorizonLevel: 90, current: 180), 180, "upright keeps the last")
        XCTAssertEqual(Capture.landscapeAngle(forHorizonLevel: 270, current: 0), 0)
        XCTAssertEqual(Capture.landscapeAngle(forHorizonLevel: -180, current: 0), 180)
    }

    func testTenSecondsOfTheTestPattern() throws {
        let recording = try Self.recording.get()
        let frames = Self.seconds * Int(VideoSettings.defaults.fps)
        print("rate control: \(recording.rateControl.rawValue); \(recording.counts)")
        XCTAssertEqual(recording.counts.videoFrames, frames)
        XCTAssertEqual(recording.counts.keyframes, frames / VideoSettings.defaults.keyframeInterval)
        XCTAssertEqual(recording.counts.droppedVideo, 0)
        XCTAssertGreaterThan(recording.counts.audioFrames, Self.seconds * 48_000 / 1024 - 10)
        for chunk in recording.chunks.dropLast() { XCTAssertEqual(chunk.count, TSMuxer.chunkSize) }
        XCTAssertEqual(recording.stream.count % TSMuxer.packetSize, 0)

        // The muxer's side of it; ffprobe checks the codecs.
        let packets = TSPacket.parse(recording.stream)
        let video = TSPacket.pes(packets, pid: TSMuxer.videoPID)
        XCTAssertEqual(video.count, frames)
        for (n, pes) in video.enumerated() {
            let types = H264.nalUnits(annexB: pes.payload).map(H264.nalType)
            if n % VideoSettings.defaults.keyframeInterval == 0 {
                XCTAssertEqual(Array(types.prefix(3)), [9, 7, 8], "frame \(n): AUD, SPS, PPS")
                XCTAssertTrue(types.contains(H264.NALType.idr), "frame \(n) is an IDR")
            } else {
                XCTAssertFalse(types.contains(H264.NALType.idr), "frame \(n) is not an IDR")
            }
        }
        XCTAssertGreaterThan(TSPacket.pes(packets, pid: TSMuxer.audioPID).count, 400)

        let kbps = Double(recording.counts.videoBytes) * 8 / Double(Self.seconds) / 1000
        print(String(format: "video elementary stream: %.0f kbps over %d s", kbps, Self.seconds))
        try save(recording.stream, as: "test-pattern.ts")
    }

    /// OL-64c's sender carries the encoded stream, paced at real time, to the srt-live-transmit
    /// listener; what it forwards over UDP is the stream, byte for byte, and goes to ffprobe too.
    func testTheEncodedStreamOverSRT() throws {
        let passphrase = try XCTUnwrap(
            ProcessInfo.processInfo.environment["OLI_SRT_TEST_PASSPHRASE"],
            "no listener passphrase: run the tests with ios/run-tests.sh")
        let recording = try Self.recording.get()
        let receiver = try UDPReceiver(port: Self.deliveredUDPPort)
        let sender = SRTSender(options: SRTSenderOptions(
            host: "127.0.0.1", port: Self.encodedPort, latencyMs: 200, passphrase: passphrase, pbkeylen: 16,
            bitrateBitsPerSecond: Int64(VideoSettings.defaults.bitrateKbps + AudioSettings.defaults.bitrateKbps) * 1000,
            connectTimeoutMs: 10_000))
        defer { sender.stop() }
        try sender.connect()

        var received = Data()
        let done = expectation(description: "received")
        DispatchQueue.global().async {
            received = receiver.receive(count: recording.stream.count, timeout: Double(Self.seconds) + 20)
            done.fulfill()
        }
        let start = Date()
        let interval = Double(Self.seconds) / Double(recording.chunks.count)
        for (n, chunk) in recording.chunks.enumerated() {
            let due = start.addingTimeInterval(Double(n) * interval)
            let wait = due.timeIntervalSinceNow
            if wait > 0 { Thread.sleep(forTimeInterval: wait) }
            try sender.send(chunk)
        }
        wait(for: [done], timeout: Double(Self.seconds) + 30)
        XCTAssertEqual(received.count, recording.stream.count, "bytes the listener delivered")
        XCTAssertTrue(received == recording.stream, "the listener delivered the stream sent, in order")
        try save(received, as: "received-over-srt.ts")
    }
}
