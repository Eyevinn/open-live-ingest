import AVFoundation
import CoreImage
import CoreVideo
import SwiftUI

/// What the level bar and the live picture show (docs/specs/ios-app.md, section 5): set by a
/// `MediaMonitor`, on the main thread.
final class MonitorDisplay: ObservableObject {
    /// Whether a source feeds the monitor.
    @Published fileprivate(set) var running = false
    /// The last 250 ms window's level; nil before the first or when the window had no sound.
    @Published fileprivate(set) var level: AudioLevel?
    @Published fileprivate(set) var silent = false
    /// The microphone's name for the warning (the audio route's port name).
    @Published fileprivate(set) var microphone = ""
    /// A scaled-down picture of what is sent, about twice a second (step 3 only).
    @Published fileprivate(set) var picture: CGImage?
}

/// A tap on pictures and sound that only looks (docs/specs/ios-app.md, section 5): the level of
/// each 250 ms window (`AudioLevel`), the silence warning (`SilenceDetector`), and, when asked
/// for, a picture about twice a second scaled down to 320 pixels wide. Fed by step 2's preview
/// session, and by `MediaSession` with the same pixel and PCM buffers it hands the encoders, so
/// step 3 shows what goes out.
///
/// The taps never block or copy: `video` and `audio` check a flag under a lock and hand the
/// buffer (retained, read only) to the monitor's own queue, where the level is measured and the
/// picture scaled; a picture is skipped while the last one is still being made. Runs between
/// `start` and `stop` only, so it stops with its source (the background pause stops both).
final class MediaMonitor {
    /// The level window and the rate of the level bar (the desktop's 4 Hz).
    static let window: TimeInterval = 0.25
    static let pictureInterval: TimeInterval = 0.5
    static let pictureWidth: CGFloat = 320

    private let display: MonitorDisplay
    private let pictures: Bool
    private let queue = DispatchQueue(label: "com.eyevinn.open-live-ingest.monitor")
    private lazy var context = CIContext(options: [.cacheIntermediates: false])

    // Under `lock`: what the taps look at.
    private let lock = NSLock()
    private var running = false
    private var generation = 0
    private var rendering = false
    private var nextPicture: UInt64 = 0

    // On `queue`.
    private var timer: DispatchSourceTimer?
    private var pending: AudioLevel?
    private var detector = SilenceDetector(start: 0)

    /// Shows on `display`; `pictures` makes the scaled-down pictures too.
    init(display: MonitorDisplay, pictures: Bool) {
        self.display = display
        self.pictures = pictures
    }

    deinit {
        timer?.cancel()
    }

    /// The monitor's clock: seconds since boot.
    static func now() -> TimeInterval {
        ProcessInfo.processInfo.systemUptime
    }

    /// Starts (again) with `microphone` named in the warning; the silence clock starts now.
    func start(microphone: String) {
        lock.lock()
        running = true
        generation += 1
        rendering = false
        nextPicture = 0
        lock.unlock()
        let display = self.display
        queue.async { [self] in
            timer?.cancel()
            pending = nil
            detector = SilenceDetector(start: MediaMonitor.now())
            let ticker = DispatchSource.makeTimerSource(queue: queue)
            ticker.schedule(deadline: .now() + MediaMonitor.window, repeating: MediaMonitor.window, leeway: .milliseconds(20))
            ticker.setEventHandler { [weak self] in self?.closeWindow(at: MediaMonitor.now()) }
            timer = ticker
            ticker.resume()
            DispatchQueue.main.async {
                display.running = true
                display.level = nil
                display.silent = false
                display.microphone = microphone
                display.picture = nil
            }
        }
    }

    /// Stops and clears the display.
    func stop() {
        lock.lock()
        running = false
        generation += 1
        lock.unlock()
        let display = self.display
        queue.async { [self] in
            timer?.cancel()
            timer = nil
            pending = nil
            DispatchQueue.main.async {
                display.running = false
                display.level = nil
                display.silent = false
                display.picture = nil
            }
        }
    }

    private var isRunning: Bool {
        lock.lock()
        defer { lock.unlock() }
        return running
    }

    /// A PCM buffer as it goes to the encoder (or the preview's): measured on the monitor's queue.
    func audio(_ buffer: AVAudioPCMBuffer) {
        guard isRunning else { return }
        queue.async { [weak self] in
            guard let self, self.timer != nil, let level = AudioLevel.measure(buffer) else { return }
            self.pending = self.pending.map { $0.loudest(level) } ?? level
        }
    }

    /// A picture as it goes to the encoder: every `pictureInterval`, unless the last one is still
    /// being scaled, on the monitor's queue.
    func video(_ pixelBuffer: CVPixelBuffer) {
        guard pictures else { return }
        let now = DispatchTime.now().uptimeNanoseconds
        lock.lock()
        let take = running && !rendering && now >= nextPicture
        if take {
            rendering = true
            nextPicture = now + UInt64(MediaMonitor.pictureInterval * 1_000_000_000)
        }
        let generation = self.generation
        lock.unlock()
        guard take else { return }
        queue.async { [weak self] in
            guard let self else { return }
            let image = self.scaled(pixelBuffer)
            self.lock.lock()
            self.rendering = false
            let current = self.running && self.generation == generation
            self.lock.unlock()
            guard current, let image else { return }
            let display = self.display
            DispatchQueue.main.async { display.picture = image }
        }
    }

    /// Every 250 ms: the window's level to the display and the silence detector.
    private func closeWindow(at time: TimeInterval) {
        let level = pending
        pending = nil
        let silent = detector.update(peak: level?.peak, at: time)
        let display = self.display
        DispatchQueue.main.async {
            guard display.running else { return }
            display.level = level
            if display.silent != silent { display.silent = silent }
        }
    }

    private func scaled(_ pixelBuffer: CVPixelBuffer) -> CGImage? {
        let image = CIImage(cvPixelBuffer: pixelBuffer)
        let width = image.extent.width
        guard width > 0 else { return nil }
        let scale = min(1, MediaMonitor.pictureWidth / width)
        let small = image.transformed(by: CGAffineTransform(scaleX: scale, y: scale))
        return context.createCGImage(small, from: small.extent.integral)
    }
}

/// The level bar: the peak from -60 to 0 dBFS, the RMS as a darker inner bar (as the desktop's,
/// desktop-app.md section 5.2), and "No sound from <microphone>." while the microphone is silent.
/// Its accessibility value is the level in dBFS ("-20.0 dBFS peak, -23.0 dBFS RMS").
struct LevelBar: View {
    @ObservedObject var display: MonitorDisplay

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            GeometryReader { geometry in
                let width = geometry.size.width
                ZStack(alignment: .leading) {
                    Capsule().fill(Color.black.opacity(0.35))
                    Capsule()
                        .fill(Self.color(display.level?.peak))
                        .frame(width: width * Self.fraction(display.level?.peak))
                    Capsule()
                        .fill(Color.black.opacity(0.3))
                        .frame(width: width * Self.fraction(display.level?.rms), height: 4)
                }
            }
            .frame(height: 8)
            .accessibilityElement()
            .accessibilityLabel("Sound level")
            .accessibilityValue(Self.value(display.level))
            .accessibilityIdentifier("levelBar")
            if display.silent {
                HStack(spacing: 4) {
                    Image(systemName: "speaker.slash.fill")
                    Text("No sound from \(display.microphone).")
                        .accessibilityIdentifier("noSoundWarning")
                }
                .font(.footnote)
                .foregroundStyle(.orange)
            }
        }
    }

    /// Where `db` sits on the bar's -60…0 dBFS.
    static func fraction(_ db: Float?) -> CGFloat {
        guard let db else { return 0 }
        return CGFloat(min(1, max(0, (db + 60) / 60)))
    }

    static func color(_ peak: Float?) -> Color {
        guard let peak else { return .green }
        if peak >= -3 { return .red }
        if peak >= -9 { return .yellow }
        return .green
    }

    /// The accessibility value: the peak first, so a test reads it as the first number.
    static func value(_ level: AudioLevel?) -> String {
        guard let level else { return "no sound level" }
        return String(format: "%.1f dBFS peak, %.1f dBFS RMS", level.peak, level.rms)
    }
}

/// Step 3 while the uplink runs: the picture being sent, small, and the level bar under it.
struct LiveMonitorView: View {
    @ObservedObject var display: MonitorDisplay

    static let width: CGFloat = 192

    var body: some View {
        if display.running {
            VStack(alignment: .leading, spacing: 6) {
                ZStack {
                    Color.black
                    if let picture = display.picture {
                        Image(decorative: picture, scale: 1)
                            .resizable()
                            .aspectRatio(contentMode: .fit)
                    }
                }
                .frame(width: Self.width, height: Self.width * 9 / 16)
                .clipShape(RoundedRectangle(cornerRadius: 6))
                .accessibilityElement()
                .accessibilityLabel("Picture being sent")
                .accessibilityValue(display.picture == nil ? "waiting" : "showing")
                .accessibilityIdentifier("livePreview")
                LevelBar(display: display)
                    .frame(width: Self.width)
            }
        }
    }
}
