import AVFoundation
import SwiftUI

/// The picked lens's live picture behind step 2's controls, and the picked microphone's level
/// (docs/specs/ios-app.md, section 5): an `AVCaptureVideoPreviewLayer` over a capture session of
/// its own, with the microphone's PCM into a `MediaMonitor` for the level bar and the no-sound
/// warning. It runs only while step 2 is on screen and the app is in the foreground. The picture
/// is always shown as it is sent, 16:9 landscape: full screen with the phone turned, and in
/// portrait the landscape crop in a 16:9 box with a "Turn the phone" hint.
final class PreviewSession: NSObject, AVCaptureAudioDataOutputSampleBufferDelegate {
    let session = AVCaptureSession()
    /// The level bar's and the warning's.
    let display: MonitorDisplay
    private let monitor: MediaMonitor
    private let audioOutput = AVCaptureAudioDataOutput()
    private let queue = DispatchQueue(label: "com.eyevinn.open-live-ingest.preview")
    private let audioQueue = DispatchQueue(label: "com.eyevinn.open-live-ingest.preview.audio")
    /// What is shown: the lens, the microphone and whether with sound.
    private var shown: String?

    override init() {
        let display = MonitorDisplay()
        self.display = display
        monitor = MediaMonitor(display: display, pictures: false)
        super.init()
        audioOutput.setSampleBufferDelegate(self, queue: audioQueue)
    }

    /// Shows `lens`, with the level of `microphoneUID` (the default route when nil) when `sound`;
    /// nothing for the test pattern or a lens that is not there.
    func show(_ lens: Lens?, microphone microphoneUID: String?, sound: Bool) {
        let key = lens.map { "\($0.id)|\(microphoneUID ?? "")|\(sound)" }
        guard key != shown else { return }
        shown = key
        let device = lens.flatMap { AVCaptureDevice(uniqueID: $0.id) }
        let withSound: Bool
        if device != nil, sound {
            do {
                try Capture.routeMicrophone(microphoneUID)
                withSound = true
            } catch {
                AppLog.warning("preview: the microphone could not be routed, no level shown")
                withSound = false
            }
        } else {
            withSound = false
        }
        let session = self.session
        let audioOutput = self.audioOutput
        queue.async {
            session.beginConfiguration()
            // The app's audio session (Capture.routeMicrophone) decides the input.
            session.automaticallyConfiguresApplicationAudioSession = false
            for input in session.inputs { session.removeInput(input) }
            if let device, let input = try? AVCaptureDeviceInput(device: device), session.canAddInput(input) {
                session.addInput(input)
            }
            let hasOutput = session.outputs.contains { $0 === audioOutput }
            if withSound, let microphone = AVCaptureDevice.default(for: .audio),
                let input = try? AVCaptureDeviceInput(device: microphone), session.canAddInput(input)
            {
                session.addInput(input)
                if !hasOutput, session.canAddOutput(audioOutput) { session.addOutput(audioOutput) }
            } else if hasOutput {
                session.removeOutput(audioOutput)
            }
            session.commitConfiguration()
            if device != nil, !session.isRunning { session.startRunning() }
            if device == nil, session.isRunning { session.stopRunning() }
        }
        if withSound {
            monitor.start(microphone: Capture.microphoneName())
        } else {
            monitor.stop()
        }
    }

    /// `body` on the main thread with whether the microphone may be used, asking first if it was
    /// never asked. Without it step 2 shows the picture and no level bar.
    static func microphoneAllowed(_ body: @escaping (Bool) -> Void) {
        switch AVCaptureDevice.authorizationStatus(for: .audio) {
        case .authorized:
            body(true)
        case .notDetermined:
            AVCaptureDevice.requestAccess(for: .audio) { granted in
                DispatchQueue.main.async { body(granted) }
            }
        default:
            body(false)
        }
    }

    func stop() {
        shown = nil
        monitor.stop()
        let session = self.session
        queue.async {
            if session.isRunning { session.stopRunning() }
            for input in session.inputs { session.removeInput(input) }
        }
    }

    func captureOutput(_ output: AVCaptureOutput, didOutput sampleBuffer: CMSampleBuffer, from connection: AVCaptureConnection) {
        if let pcm = Capture.pcmBuffer(sampleBuffer) { monitor.audio(pcm) }
    }
}

/// The preview layer in a view, turned to landscape: `angle` is the connection's
/// `videoRotationAngle`, 0 or 180.
struct CameraPreview: UIViewRepresentable {
    let session: AVCaptureSession
    let angle: CGFloat

    final class PreviewView: UIView {
        override class var layerClass: AnyClass { AVCaptureVideoPreviewLayer.self }
        var preview: AVCaptureVideoPreviewLayer { layer as! AVCaptureVideoPreviewLayer }
    }

    func makeUIView(context: Context) -> PreviewView {
        let view = PreviewView()
        view.backgroundColor = .black
        view.preview.session = session
        view.preview.videoGravity = .resizeAspectFill
        return view
    }

    func updateUIView(_ view: PreviewView, context: Context) {
        if let connection = view.preview.connection, connection.isVideoRotationAngleSupported(angle) {
            connection.videoRotationAngle = angle
        }
    }
}

/// The preview as step 2 shows it: full screen in landscape, the 16:9 crop and the hint in
/// portrait, or what is sent instead when there is no camera.
struct PreviewBackdrop: View {
    let session: AVCaptureSession
    let lens: Lens?

    var body: some View {
        GeometryReader { geometry in
            let portrait = geometry.size.height > geometry.size.width
            ZStack {
                Color.black
                if lens == nil || lens?.id == Devices.testPatternID {
                    Text("No camera here: the test pattern is sent.")
                        .foregroundStyle(.white.opacity(0.8))
                        .accessibilityIdentifier("previewTestPattern")
                } else if portrait {
                    VStack(spacing: 12) {
                        CameraPreview(session: session, angle: 0)
                            .frame(width: geometry.size.width, height: geometry.size.width * 9 / 16)
                            .clipped()
                        Label("Turn the phone: the picture is sent in landscape.", systemImage: "rotate.right")
                            .foregroundStyle(.white)
                            .accessibilityIdentifier("turnThePhone")
                    }
                } else {
                    CameraPreview(session: session, angle: Self.landscapeAngle())
                }
            }
        }
        .ignoresSafeArea()
    }

    /// The rotation that shows the picture upright with the phone turned: the interface's
    /// landscape side.
    @MainActor static func landscapeAngle() -> CGFloat {
        let scene = UIApplication.shared.connectedScenes.compactMap { $0 as? UIWindowScene }.first
        return scene?.interfaceOrientation == .landscapeLeft ? 180 : 0
    }
}
