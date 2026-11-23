import AVFoundation
import SwiftUI

/// The picked lens's live picture behind step 2's controls (docs/specs/ios-app.md, section 5): an
/// `AVCaptureVideoPreviewLayer` over a capture session of its own, which runs only while step 2
/// is on screen. The picture is always shown as it is sent, 16:9 landscape: full screen with the
/// phone turned, and in portrait the landscape crop in a 16:9 box with a "Turn the phone" hint.
final class PreviewSession {
    let session = AVCaptureSession()
    private let queue = DispatchQueue(label: "com.eyevinn.open-live-ingest.preview")
    private var lensID: String?

    /// Shows `lens`; nothing for the test pattern or a lens that is not there.
    func show(_ lens: Lens?) {
        guard lens?.id != lensID else { return }
        lensID = lens?.id
        let session = self.session
        let device = lens.flatMap { AVCaptureDevice(uniqueID: $0.id) }
        queue.async {
            session.beginConfiguration()
            for input in session.inputs { session.removeInput(input) }
            if let device, let input = try? AVCaptureDeviceInput(device: device), session.canAddInput(input) {
                session.addInput(input)
            }
            session.commitConfiguration()
            if device != nil, !session.isRunning { session.startRunning() }
            if device == nil, session.isRunning { session.stopRunning() }
        }
    }

    func stop() {
        lensID = nil
        let session = self.session
        queue.async {
            if session.isRunning { session.stopRunning() }
            for input in session.inputs { session.removeInput(input) }
        }
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
