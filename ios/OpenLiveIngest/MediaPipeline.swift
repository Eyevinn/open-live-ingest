import AVFoundation
import CoreMedia

/// Encode and mux: pictures and sound in, MPEG-TS out in 1316-byte chunks (docs/specs/ios-app.md,
/// section 11). VideoToolbox H.264 and AudioToolbox AAC into `TSMuxer`.
///
/// The stream starts at the first picture: its time is the stream's zero, and sound from before
/// it is dropped. `appendVideo` and `appendAudio` may be called from two different threads, each
/// from one at a time; `onChunk` is called with the muxer's lock held, so it should hand the chunk
/// on rather than block.
final class MediaPipeline {
    let video: VideoSettings
    let audio: AudioSettings
    /// When set, `appendVideo` waits for the frame to be encoded (tests encoding faster than real time).
    var synchronousVideo = false

    private let videoEncoder: VideoEncoder
    private let audioEncoder: AudioEncoder
    private let muxer: TSMuxer
    private let lock = NSLock()
    private var origin: CMTime?
    private var counts = Counts()

    struct Counts: Equatable {
        var videoFrames = 0
        var keyframes = 0
        var videoBytes = 0
        var audioFrames = 0
        var droppedVideo = 0
    }

    var rateControl: VideoEncoder.RateControl { videoEncoder.rateControl }
    var statistics: Counts { locked { counts } }

    init(video: VideoSettings, audio: AudioSettings, onChunk: @escaping (Data) -> Void) throws {
        self.video = video
        self.audio = audio
        videoEncoder = try VideoEncoder(settings: video)
        audioEncoder = try AudioEncoder(settings: audio)
        muxer = TSMuxer()
        muxer.onChunk = onChunk
        videoEncoder.output = { [weak self] frame in self?.mux(frame) }
        videoEncoder.onError = { [weak self] _ in
            guard let self else { return }
            self.locked { self.counts.droppedVideo += 1 }
        }
        audioEncoder.output = { [weak self] data, time in self?.mux(aac: data, time: time) }
    }

    func appendVideo(_ pixelBuffer: CVPixelBuffer, time: CMTime) {
        locked {
            if origin == nil { origin = time }
        }
        let duration = CMTime(seconds: 1 / max(video.fps, 1), preferredTimescale: 90_000)
        videoEncoder.encode(pixelBuffer, pts: time, duration: duration)
        if synchronousVideo { videoEncoder.complete(until: time) }
    }

    func appendAudio(_ buffer: AVAudioPCMBuffer, time: CMTime) {
        guard locked({ origin != nil }) else { return }
        audioEncoder.encode(buffer, time: time)
    }

    /// Waits for the encoder and hands over the last, short chunk.
    func finish() {
        videoEncoder.complete()
        locked { muxer.flush() }
    }

    // MARK: -

    private func ticks(_ time: CMTime) -> UInt64? {
        guard let origin, time.isValid else { return nil }
        let value = CMTimeConvertScale(CMTimeSubtract(time, origin), timescale: 90_000, method: .roundHalfAwayFromZero).value
        return value >= 0 ? UInt64(value) : nil
    }

    private func mux(_ frame: EncodedVideoFrame) {
        locked {
            guard let pts = ticks(frame.pts) else { return }
            let dts = frame.dts.isValid ? ticks(frame.dts) : nil
            muxer.writeVideo(nalUnits: frame.nalUnits, sps: frame.sps, pps: frame.pps, pts: pts, dts: dts,
                             isKeyframe: frame.isKeyframe)
            counts.videoFrames += 1
            counts.videoBytes += frame.byteCount
            if frame.isKeyframe { counts.keyframes += 1 }
        }
    }

    private func mux(aac: Data, time: CMTime) {
        locked {
            guard let pts = ticks(time) else { return }
            muxer.writeAudio(aacFrame: aac, pts: pts, sampleRate: audio.sampleRate, channels: audio.channels)
            counts.audioFrames += 1
        }
    }

    private func locked<T>(_ body: () throws -> T) rethrows -> T {
        lock.lock()
        defer { lock.unlock() }
        return try body()
    }
}

/// One input's media on the phone, as the core's `media` start event describes it: the camera and
/// microphone (or the test pattern), encode and mux, and the SRT caller (docs/specs/ios-app.md,
/// section 8.2; ADR-002 section 3). `stop()` ends all of it. Chunks are sent off the muxer's lock
/// on a queue of their own; while the caller is not connected they are dropped.
final class MediaSession {
    let start: MediaStart
    let sender: SRTSender
    private let pipeline: MediaPipeline
    private let sendQueue: DispatchQueue
    private let mediaQueue = DispatchQueue(label: "com.eyevinn.open-live-ingest.media")
    private var pattern: TestPatternSource?
    private var capture: Capture?

    init(_ start: MediaStart) throws {
        self.start = start
        let sender = SRTSender(options: start.senderOptions)
        let sendQueue = DispatchQueue(label: "com.eyevinn.open-live-ingest.media-send")
        self.sendQueue = sendQueue
        pipeline = try MediaPipeline(video: start.video, audio: start.audio) { chunk in
            sendQueue.async { _ = try? sender.send(chunk) }
        }
        self.sender = sender
    }

    /// Opens the source and starts calling. The test pattern when the event asks for it or this
    /// device has no camera.
    func run() throws {
        sender.start()
        let pipeline = self.pipeline
        if start.usesTestPattern || !Capture.hasCamera {
            let pattern = try TestPatternSource(video: start.video, audio: start.audio)
            pattern.start(
                queue: mediaQueue, video: { pipeline.appendVideo($0, time: $1) },
                audio: { pipeline.appendAudio($0, time: $1) })
            self.pattern = pattern
        } else {
            let capture = Capture(cameraID: start.device, microphoneUID: start.microphone, video: start.video)
            try capture.start(video: { pipeline.appendVideo($0, time: $1) }, audio: { pipeline.appendAudio($0, time: $1) })
            self.capture = capture
        }
    }

    func stop() {
        pattern?.stop()
        pattern = nil
        capture?.stop()
        capture = nil
        sender.stop()
    }
}
