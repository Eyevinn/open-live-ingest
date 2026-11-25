import AVFoundation
import XCTest
@testable import OpenLiveIngest

/// The level bar's numbers and the no-sound warning (docs/specs/ios-app.md, section 5; the
/// desktop's thresholds, desktop-app.md section 5.2): dBFS of the test pattern's tone and of
/// digital silence, and the silence detector's 5 s below -60 dBFS, cleared at -54 dBFS.
final class LevelMeterTests: XCTestCase {
    private func pattern() throws -> TestPatternSource {
        try TestPatternSource(video: .defaults, audio: .defaults)
    }

    /// A buffer of `frames` samples of `format`, every sample `value`.
    private func constant(_ format: AVAudioFormat, frames: Int, value: Float) throws -> AVAudioPCMBuffer {
        let buffer = try XCTUnwrap(AVAudioPCMBuffer(pcmFormat: format, frameCapacity: AVAudioFrameCount(frames)))
        buffer.frameLength = AVAudioFrameCount(frames)
        let data = try XCTUnwrap(buffer.floatChannelData)
        for ch in 0..<Int(format.channelCount) {
            for i in 0..<frames { data[ch][i] = value }
        }
        return buffer
    }

    func testTheTestPatternToneIsMinus20PeakAndMinus23RMS() throws {
        // One second of the 1 kHz sine at -20 dBFS: amplitude 0.1, RMS 0.1 / sqrt(2).
        let buffer = try XCTUnwrap(try pattern().audioBuffer(startSample: 0, frameCount: 48_000))
        let level = try XCTUnwrap(AudioLevel.measure(buffer))
        XCTAssertEqual(level.peak, -20, accuracy: 0.05)
        XCTAssertEqual(level.rms, -23.01, accuracy: 0.05)
    }

    func testAShortBufferOfTheToneReadsTheSame() throws {
        // What one frame of the pattern at 25 fps hands the encoder (and the monitor).
        let source = try pattern()
        let first = source.audioSamples(beforeFrame: 7)
        let count = Int(source.audioSamples(beforeFrame: 8) - first)
        let buffer = try XCTUnwrap(source.audioBuffer(startSample: first, frameCount: count))
        let level = try XCTUnwrap(AudioLevel.measure(buffer))
        XCTAssertEqual(level.peak, -20, accuracy: 0.1)
        XCTAssertEqual(level.rms, -23, accuracy: 0.2)
    }

    func testDigitalSilenceIsTheFloor() throws {
        let format = try XCTUnwrap(
            AVAudioFormat(commonFormat: .pcmFormatFloat32, sampleRate: 48_000, channels: 2, interleaved: false))
        let level = try XCTUnwrap(AudioLevel.measure(try constant(format, frames: 1024, value: 0)))
        XCTAssertEqual(level, AudioLevel(peak: AudioLevel.floor, rms: AudioLevel.floor))
        XCTAssertTrue(level.peak < SilenceDetector.threshold)
    }

    func testFullScaleIsZeroAndTheLouderChannelCounts() throws {
        let format = try XCTUnwrap(
            AVAudioFormat(commonFormat: .pcmFormatFloat32, sampleRate: 48_000, channels: 2, interleaved: false))
        let buffer = try constant(format, frames: 480, value: 0)
        let data = try XCTUnwrap(buffer.floatChannelData)
        for i in 0..<480 { data[1][i] = i % 2 == 0 ? 1 : -1 }
        let level = try XCTUnwrap(AudioLevel.measure(buffer))
        XCTAssertEqual(level.peak, 0, accuracy: 0.001)
        XCTAssertEqual(level.rms, 0, accuracy: 0.001)
    }

    func testInterleavedInt16AsCaptureHandsItOver() throws {
        let format = try XCTUnwrap(
            AVAudioFormat(commonFormat: .pcmFormatInt16, sampleRate: 48_000, channels: 2, interleaved: true))
        let frames = 960
        let buffer = try XCTUnwrap(AVAudioPCMBuffer(pcmFormat: format, frameCapacity: AVAudioFrameCount(frames)))
        buffer.frameLength = AVAudioFrameCount(frames)
        let data = try XCTUnwrap(buffer.int16ChannelData)
        // Left: a 1 kHz sine at -20 dBFS; right: silence.
        for i in 0..<frames {
            data[0][2 * i] = Int16((3276.8 * sin(2 * Double.pi * 1000 * Double(i) / 48_000)).rounded())
            data[0][2 * i + 1] = 0
        }
        let level = try XCTUnwrap(AudioLevel.measure(buffer))
        XCTAssertEqual(level.peak, -20, accuracy: 0.05)
        XCTAssertEqual(level.rms, -23.01, accuracy: 0.05)
    }

    func testDBFSClamps() {
        XCTAssertEqual(AudioLevel.dbfs(0), AudioLevel.floor)
        XCTAssertEqual(AudioLevel.dbfs(1e-9), AudioLevel.floor)
        XCTAssertEqual(AudioLevel.dbfs(2), 0)
        XCTAssertEqual(AudioLevel.dbfs(0.5), -6.02, accuracy: 0.01)
        XCTAssertEqual(AudioLevel.dbfs(.nan), AudioLevel.floor)
    }

    // MARK: - Silence

    /// Feeds 250 ms windows of `peak` from `from` (exclusive) to `to` (inclusive); the last answer.
    @discardableResult
    private func feed(_ detector: inout SilenceDetector, _ peak: Float?, from: TimeInterval, to: TimeInterval) -> Bool {
        var silent = detector.silent
        var time = from + 0.25
        while time <= to + 1e-9 {
            silent = detector.update(peak: peak, at: time)
            time += 0.25
        }
        return silent
    }

    func testSilenceWarnsAfterFiveSecondsBelowMinus60() {
        var detector = SilenceDetector(start: 100)
        XCTAssertFalse(feed(&detector, AudioLevel.floor, from: 100, to: 104.75))
        XCTAssertTrue(feed(&detector, AudioLevel.floor, from: 104.75, to: 105))
    }

    func testTheToneNeverWarns() {
        var detector = SilenceDetector(start: 0)
        XCTAssertFalse(feed(&detector, -20, from: 0, to: 60))
    }

    func testAQuietRoomAboveMinus60DoesNotWarn() {
        var detector = SilenceDetector(start: 0)
        XCTAssertFalse(feed(&detector, -58, from: 0, to: 30))
    }

    func testNoSoundAtAllWarnsAfterFiveSeconds() {
        var detector = SilenceDetector(start: 0)
        XCTAssertFalse(feed(&detector, -20, from: 0, to: 2))
        XCTAssertFalse(feed(&detector, nil, from: 2, to: 6.75))
        XCTAssertTrue(feed(&detector, nil, from: 6.75, to: 7))
    }

    func testAPauseInSpeechDoesNotWarn() {
        var detector = SilenceDetector(start: 0)
        XCTAssertFalse(feed(&detector, -30, from: 0, to: 3))
        XCTAssertFalse(feed(&detector, -80, from: 3, to: 7.5))
        XCTAssertFalse(feed(&detector, -30, from: 7.5, to: 8))
        XCTAssertFalse(feed(&detector, -80, from: 8, to: 12.75))
    }

    func testTheWarningClearsAtMinus54NotBelow() {
        var detector = SilenceDetector(start: 0)
        XCTAssertTrue(feed(&detector, AudioLevel.floor, from: 0, to: 5))
        // Hovering between the two thresholds keeps the warning (6 dB of hysteresis).
        XCTAssertTrue(feed(&detector, -57, from: 5, to: 20))
        XCTAssertFalse(detector.update(peak: -54, at: 20.25))
        // Quiet again: five more seconds before it warns again.
        XCTAssertFalse(feed(&detector, -70, from: 20.25, to: 25))
        XCTAssertTrue(feed(&detector, -70, from: 25, to: 25.25))
    }

    func testWithinTenSecondsOfAMutedMicrophone() {
        // #173 AC3: covered or muted, the warning shows within 10 s (5 s plus a 250 ms window).
        var detector = SilenceDetector(start: 0)
        XCTAssertFalse(feed(&detector, -25, from: 0, to: 10))
        var warnedAt: TimeInterval?
        var time = 10.0
        while time < 20, warnedAt == nil {
            time += 0.25
            if detector.update(peak: AudioLevel.floor, at: time) { warnedAt = time }
        }
        XCTAssertNotNil(warnedAt)
        XCTAssertLessThanOrEqual((warnedAt ?? 99) - 10, 10)
    }

    // MARK: - The monitor

    /// The monitor fed the pattern's tone: a steady level near -20 dBFS and no warning, on the
    /// display, as step 3 shows it while live with the test pattern.
    func testTheMonitorShowsTheToneSteadyAndNoWarning() throws {
        let source = try pattern()
        let display = MonitorDisplay()
        let monitor = MediaMonitor(display: display, pictures: true)
        monitor.start(microphone: "Test pattern")
        var frame = 0
        var peaks: [Float] = []
        let end = Date().addingTimeInterval(2)
        while Date() < end {
            let first = source.audioSamples(beforeFrame: frame)
            let next = source.audioSamples(beforeFrame: frame + 1)
            if let sound = source.audioBuffer(startSample: first, frameCount: Int(next - first)) { monitor.audio(sound) }
            if let picture = source.pixelBuffer(frame: frame) { monitor.video(picture) }
            frame += 1
            RunLoop.main.run(until: Date().addingTimeInterval(1 / source.video.fps))
            if let peak = display.level?.peak { peaks.append(peak) }
        }
        XCTAssertTrue(display.running)
        XCTAssertFalse(display.silent)
        XCTAssertFalse(peaks.isEmpty, "no level reached the display")
        for peak in peaks { XCTAssertEqual(peak, -20, accuracy: 0.5) }
        XCTAssertNotNil(display.picture, "no picture reached the display")
        XCTAssertEqual(Double(display.picture?.width ?? 0), Double(MediaMonitor.pictureWidth), accuracy: 1)

        monitor.stop()
        RunLoop.main.run(until: Date().addingTimeInterval(0.3))
        XCTAssertFalse(display.running)
        XCTAssertNil(display.level)
        XCTAssertNil(display.picture)
    }
}
