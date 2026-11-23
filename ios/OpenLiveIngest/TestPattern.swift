import AVFoundation
import CoreMedia
import CoreVideo

/// A moving test pattern and a tone at the configured resolution and frame rate, for when there
/// is no camera (the simulator) or the test pattern is picked (docs/specs/ios-app.md, section 15,
/// OL-64d; the end-to-end run OL-64h uses it).
///
/// The picture: colour bars in the top two thirds with a white square sweeping across them, and
/// a band of noise at the bottom that changes every frame, so the encoder has to spend its bits
/// as it would on a camera's picture. The sound: a 1 kHz sine at -20 dBFS on every channel.
///
/// `pixelBuffer(frame:)` and `audioBuffer(startSample:frameCount:)` make any frame on demand (the
/// tests encode faster than real time); `start` drives both at the frame rate on the host clock,
/// the clock capture timestamps are on.
final class TestPatternSource {
    let video: VideoSettings
    let audio: AudioSettings
    let pcmFormat: AVAudioFormat

    private let pool: CVPixelBufferPool
    private let width: Int
    private let height: Int
    private let bandTop: Int
    private let luma: [UInt8]
    private let chroma: [UInt8]
    /// Twice the picture's width and twice the band's height, so any window of the band's size fits.
    private let noiseLuma: [UInt8]
    private let noiseChroma: [UInt8]

    private var timer: DispatchSourceTimer?
    private let timerQueue = DispatchQueue(label: "com.eyevinn.open-live-ingest.test-pattern")

    init(video: VideoSettings, audio: AudioSettings) throws {
        self.video = video
        self.audio = audio
        let width = video.width & ~1
        let height = video.height & ~1
        guard width >= 16, height >= 16,
            let format = AVAudioFormat(
                commonFormat: .pcmFormatFloat32, sampleRate: Double(audio.sampleRate),
                channels: AVAudioChannelCount(audio.channels), interleaved: false)
        else { throw EncoderError.audioFormat }

        let attributes: [String: Any] = [
            kCVPixelBufferPixelFormatTypeKey as String: kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
            kCVPixelBufferWidthKey as String: width,
            kCVPixelBufferHeightKey as String: height,
            kCVPixelBufferIOSurfacePropertiesKey as String: [String: Any](),
        ]
        var created: CVPixelBufferPool?
        let status = CVPixelBufferPoolCreate(kCFAllocatorDefault, nil, attributes as CFDictionary, &created)
        guard status == kCVReturnSuccess, let pool = created else { throw EncoderError.videoSession(status) }

        // BT.709 video range: white, yellow, cyan, green, magenta, red, blue, black at 75 %.
        let bars: [(y: UInt8, cb: UInt8, cr: UInt8)] = [
            (180, 128, 128), (168, 44, 136), (145, 147, 44), (134, 63, 52),
            (63, 193, 204), (51, 109, 212), (28, 212, 120), (16, 128, 128),
        ]
        let bandTop = (height * 2 / 3) & ~1
        var y = [UInt8](repeating: 16, count: width * height)
        var c = [UInt8](repeating: 128, count: width * height / 2)
        for row in 0..<bandTop {
            for x in 0..<width {
                y[row * width + x] = bars[x * bars.count / width].y
            }
        }
        for row in 0..<(bandTop / 2) {
            for x in stride(from: 0, to: width, by: 2) {
                let bar = bars[x * bars.count / width]
                c[row * width + x] = bar.cb
                c[row * width + x + 1] = bar.cr
            }
        }

        let bandHeight = height - bandTop
        var random = SplitMix64(seed: 0x6F6C_692D_3634_64)
        let noiseY = (0..<(4 * width * bandHeight)).map { _ in UInt8(16 + random.next() % 220) }
        let noiseC = (0..<(2 * width * bandHeight)).map { _ in UInt8(64 + random.next() % 128) }

        pcmFormat = format
        self.pool = pool
        self.width = width
        self.height = height
        self.bandTop = bandTop
        luma = y
        chroma = c
        noiseLuma = noiseY
        noiseChroma = noiseC
    }

    deinit {
        timer?.cancel()
    }

    /// Frame `frame` of the pattern, NV12 (video range) from a pool.
    func pixelBuffer(frame: Int) -> CVPixelBuffer? {
        var created: CVPixelBuffer?
        guard CVPixelBufferPoolCreatePixelBuffer(kCFAllocatorDefault, pool, &created) == kCVReturnSuccess,
            let buffer = created
        else { return nil }
        CVPixelBufferLockBaseAddress(buffer, [])
        defer { CVPixelBufferUnlockBaseAddress(buffer, []) }
        guard let yBase = CVPixelBufferGetBaseAddressOfPlane(buffer, 0),
            let cBase = CVPixelBufferGetBaseAddressOfPlane(buffer, 1)
        else { return nil }
        let yStride = CVPixelBufferGetBytesPerRowOfPlane(buffer, 0)
        let cStride = CVPixelBufferGetBytesPerRowOfPlane(buffer, 1)

        // Where this frame's noise window starts: a fresh place every frame.
        var random = SplitMix64(seed: UInt64(frame) &+ 1)
        let bandHeight = height - bandTop
        let nx = Int(random.next() % UInt64(width)) & ~1
        let ny = Int(random.next() % UInt64(bandHeight)) & ~1

        // The square: 1/6 of the height, sweeping left to right over two seconds.
        let side = (height / 6) & ~1
        let period = max(1, Int(video.fps * 2))
        let sx = ((frame % period) * (width - side) / period) & ~1
        let sy = (bandTop / 2 - side / 2) & ~1

        luma.withUnsafeBufferPointer { src in
            noiseLuma.withUnsafeBufferPointer { noise in
                for row in 0..<height {
                    let dst = yBase.advanced(by: row * yStride)
                    if row < bandTop {
                        dst.copyMemory(from: src.baseAddress!.advanced(by: row * width), byteCount: width)
                        if row >= sy && row < sy + side { dst.advanced(by: sx).initializeMemory(as: UInt8.self, repeating: 235, count: side) }
                    } else {
                        let n = (ny + row - bandTop) * 2 * width + nx
                        dst.copyMemory(from: noise.baseAddress!.advanced(by: n), byteCount: width)
                    }
                }
            }
        }
        chroma.withUnsafeBufferPointer { src in
            noiseChroma.withUnsafeBufferPointer { noise in
                for row in 0..<(height / 2) {
                    let dst = cBase.advanced(by: row * cStride)
                    if row < bandTop / 2 {
                        dst.copyMemory(from: src.baseAddress!.advanced(by: row * width), byteCount: width)
                        if row >= sy / 2 && row < (sy + side) / 2 {
                            dst.advanced(by: sx).initializeMemory(as: UInt8.self, repeating: 128, count: side)
                        }
                    } else {
                        let n = (ny / 2 + row - bandTop / 2) * 2 * width + nx
                        dst.copyMemory(from: noise.baseAddress!.advanced(by: n), byteCount: width)
                    }
                }
            }
        }
        return buffer
    }

    /// `frameCount` samples of the tone from sample `startSample` on.
    func audioBuffer(startSample: Int64, frameCount: Int) -> AVAudioPCMBuffer? {
        guard frameCount > 0,
            let buffer = AVAudioPCMBuffer(pcmFormat: pcmFormat, frameCapacity: AVAudioFrameCount(frameCount)),
            let channels = buffer.floatChannelData
        else { return nil }
        buffer.frameLength = AVAudioFrameCount(frameCount)
        let step = 2 * Double.pi * 1000 / Double(audio.sampleRate)
        let amplitude: Float = 0.1  // -20 dBFS
        for i in 0..<frameCount {
            let phase = Double((startSample + Int64(i)) % Int64(audio.sampleRate)) * step
            let value = amplitude * Float(sin(phase))
            for ch in 0..<Int(pcmFormat.channelCount) { channels[ch][i] = value }
        }
        return buffer
    }

    /// The samples from the start up to (not including) frame `frame`.
    func audioSamples(beforeFrame frame: Int) -> Int64 {
        Int64((Double(frame) * Double(audio.sampleRate) / video.fps).rounded())
    }

    /// Hands out a frame and the tone up to the next one every 1/fps seconds on `queue`, stamped on
    /// the host clock, until `stop()`.
    func start(
        queue: DispatchQueue, video onVideo: @escaping (CVPixelBuffer, CMTime) -> Void,
        audio onAudio: @escaping (AVAudioPCMBuffer, CMTime) -> Void
    ) {
        stop()
        let origin = CMClockGetTime(CMClockGetHostTimeClock())
        let rate = Int32(audio.sampleRate)
        var frame = 0
        let timer = DispatchSource.makeTimerSource(queue: timerQueue)
        timer.schedule(deadline: .now(), repeating: 1 / max(video.fps, 1), leeway: .milliseconds(2))
        timer.setEventHandler { [weak self] in
            guard let self else { return }
            let first = self.audioSamples(beforeFrame: frame)
            let next = self.audioSamples(beforeFrame: frame + 1)
            let videoTime = CMTimeAdd(origin, CMTime(seconds: Double(frame) / self.video.fps, preferredTimescale: 90_000))
            let audioTime = CMTimeAdd(origin, CMTime(value: first, timescale: rate))
            let picture = self.pixelBuffer(frame: frame)
            let sound = self.audioBuffer(startSample: first, frameCount: Int(next - first))
            frame += 1
            queue.async {
                if let picture { onVideo(picture, videoTime) }
                if let sound { onAudio(sound, audioTime) }
            }
        }
        self.timer = timer
        timer.resume()
    }

    func stop() {
        timer?.cancel()
        timer = nil
    }
}

/// A small, seedable generator, so the pattern is the same on every run.
struct SplitMix64 {
    private var state: UInt64

    init(seed: UInt64) {
        state = seed
    }

    mutating func next() -> UInt64 {
        state &+= 0x9E37_79B9_7F4A_7C15
        var z = state
        z = (z ^ (z >> 30)) &* 0xBF58_476D_1CE4_E5B9
        z = (z ^ (z >> 27)) &* 0x94D0_49BB_1331_11EB
        return z ^ (z >> 31)
    }
}
