import AVFoundation
import CoreMedia

enum CaptureError: Error, Equatable {
    case noCamera
    case cameraInput(String)
    case microphoneInput(String)
    case audioSession(String)
}

/// The picked lens and microphone (docs/specs/ios-app.md, section 5): an `AVCaptureSession` with
/// the camera chosen by `AVCaptureDevice.uniqueID` and the audio route by
/// `AVAudioSessionPortDescription.uid`, handing out pictures and sound with their capture times.
///
/// The picture is always 16:9 landscape (section 5, Orientation): the video connection's
/// `videoRotationAngle` follows the phone between the two landscape angles, from
/// `AVCaptureDevice.RotationCoordinator`'s horizon-level angle, and stays on the last landscape one
/// while the phone is upright, so the encoder is never handed a portrait (letterboxed) picture.
final class Capture: NSObject, AVCaptureVideoDataOutputSampleBufferDelegate, AVCaptureAudioDataOutputSampleBufferDelegate {
    /// Whether this device has any camera (the simulator has none).
    static var hasCamera: Bool {
        !AVCaptureDevice.DiscoverySession(
            deviceTypes: lensTypes, mediaType: .video, position: .unspecified
        ).devices.isEmpty
    }

    static let lensTypes: [AVCaptureDevice.DeviceType] = [
        .builtInWideAngleCamera, .builtInUltraWideCamera, .builtInTelephotoCamera,
    ]

    let cameraID: String?
    let microphoneUID: String?
    let video: VideoSettings

    private let session = AVCaptureSession()
    private let videoOutput = AVCaptureVideoDataOutput()
    private let audioOutput = AVCaptureAudioDataOutput()
    private let videoQueue = DispatchQueue(label: "com.eyevinn.open-live-ingest.capture.video")
    private let audioQueue = DispatchQueue(label: "com.eyevinn.open-live-ingest.capture.audio")
    /// `startRunning` and `stopRunning` block; they run here.
    private let sessionQueue = DispatchQueue(label: "com.eyevinn.open-live-ingest.capture.session")
    private var rotation: AVCaptureDevice.RotationCoordinator?
    private var rotationObservation: NSKeyValueObservation?
    private var onVideo: ((CVPixelBuffer, CMTime) -> Void)?
    private var onAudio: ((AVAudioPCMBuffer, CMTime) -> Void)?

    init(cameraID: String?, microphoneUID: String?, video: VideoSettings) {
        self.cameraID = cameraID
        self.microphoneUID = microphoneUID
        self.video = video
        super.init()
    }

    /// Configures the session and starts it in the background; pictures and sound arrive on queues
    /// of their own.
    func start(
        video onVideo: @escaping (CVPixelBuffer, CMTime) -> Void,
        audio onAudio: @escaping (AVAudioPCMBuffer, CMTime) -> Void
    ) throws {
        self.onVideo = onVideo
        self.onAudio = onAudio
        let camera = try pickCamera()
        try Self.routeMicrophone(microphoneUID)
        try configure(camera)
        let session = self.session
        sessionQueue.async { session.startRunning() }
    }

    func stop() {
        rotationObservation = nil
        rotation = nil
        let session = self.session
        sessionQueue.sync {
            if session.isRunning { session.stopRunning() }
        }
        videoOutput.setSampleBufferDelegate(nil, queue: nil)
        audioOutput.setSampleBufferDelegate(nil, queue: nil)
        onVideo = nil
        onAudio = nil
    }

    private func configure(_ camera: AVCaptureDevice) throws {
        session.beginConfiguration()
        defer { session.commitConfiguration() }
        // The app's audio session (routeMicrophone) decides the input; capture must not reset it.
        session.automaticallyConfiguresApplicationAudioSession = false
        let preset = Self.preset(width: video.width, height: video.height)
        if let preset, session.canSetSessionPreset(preset) { session.sessionPreset = preset }

        do {
            let input = try AVCaptureDeviceInput(device: camera)
            guard session.canAddInput(input) else { throw CaptureError.cameraInput("cannot add \(camera.localizedName)") }
            session.addInput(input)
        } catch let error as CaptureError {
            throw error
        } catch {
            throw CaptureError.cameraInput(error.localizedDescription)
        }
        setFrameRate(camera)

        if let microphone = AVCaptureDevice.default(for: .audio) {
            do {
                let input = try AVCaptureDeviceInput(device: microphone)
                if session.canAddInput(input) { session.addInput(input) }
            } catch {
                throw CaptureError.microphoneInput(error.localizedDescription)
            }
        }

        videoOutput.videoSettings = [
            kCVPixelBufferPixelFormatTypeKey as String: kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange
        ]
        videoOutput.alwaysDiscardsLateVideoFrames = true
        videoOutput.setSampleBufferDelegate(self, queue: videoQueue)
        if session.canAddOutput(videoOutput) { session.addOutput(videoOutput) }
        audioOutput.setSampleBufferDelegate(self, queue: audioQueue)
        if session.canAddOutput(audioOutput) { session.addOutput(audioOutput) }

        if let connection = videoOutput.connection(with: .video) {
            let coordinator = AVCaptureDevice.RotationCoordinator(device: camera, previewLayer: nil)
            rotation = coordinator
            Self.applyLandscape(coordinator.videoRotationAngleForHorizonLevelCapture, to: connection)
            rotationObservation = coordinator.observe(\.videoRotationAngleForHorizonLevelCapture) { coordinator, _ in
                Self.applyLandscape(coordinator.videoRotationAngleForHorizonLevelCapture, to: connection)
            }
        }
    }

    // MARK: - Devices

    private func pickCamera() throws -> AVCaptureDevice {
        if let cameraID, let device = AVCaptureDevice(uniqueID: cameraID) { return device }
        let devices = AVCaptureDevice.DiscoverySession(
            deviceTypes: Self.lensTypes, mediaType: .video, position: .unspecified
        ).devices
        guard let device = devices.first(where: { $0.position == .back }) ?? devices.first else {
            throw CaptureError.noCamera
        }
        return device
    }

    /// The app's audio session set up for recording, its input the microphone with
    /// `AVAudioSessionPortDescription.uid` `microphoneUID` (the default route when nil or not
    /// there). Capture and step 2's preview (`PreviewSession`) both take the picked microphone so.
    static func routeMicrophone(_ microphoneUID: String?) throws {
        let audioSession = AVAudioSession.sharedInstance()
        do {
            try audioSession.setCategory(.playAndRecord, mode: .videoRecording, options: [.allowBluetoothHFP, .mixWithOthers])
            try audioSession.setPreferredSampleRate(48_000)
            try audioSession.setActive(true)
            if let microphoneUID,
                let port = audioSession.availableInputs?.first(where: { $0.uid == microphoneUID })
            {
                try audioSession.setPreferredInput(port)
            }
        } catch {
            throw CaptureError.audioSession(error.localizedDescription)
        }
    }

    /// The name of the microphone the audio session records from now (its port name), for the
    /// no-sound warning.
    static func microphoneName() -> String {
        AVAudioSession.sharedInstance().currentRoute.inputs.first?.portName ?? "the microphone"
    }

    /// The camera's frame rate, when its active format offers it.
    private func setFrameRate(_ camera: AVCaptureDevice) {
        guard video.fps > 0 else { return }
        let ranges = camera.activeFormat.videoSupportedFrameRateRanges
        guard ranges.contains(where: { $0.minFrameRate <= video.fps && video.fps <= $0.maxFrameRate }) else { return }
        let duration = CMTime(value: 1000, timescale: CMTimeScale((video.fps * 1000).rounded()))
        do {
            try camera.lockForConfiguration()
            camera.activeVideoMinFrameDuration = duration
            camera.activeVideoMaxFrameDuration = duration
            camera.unlockForConfiguration()
        } catch {
            // The camera keeps its own rate; the encoder still stamps what it gets.
        }
    }

    static func preset(width: Int, height: Int) -> AVCaptureSession.Preset? {
        switch (width, height) {
        case (3840, 2160): return .hd4K3840x2160
        case (1920, 1080): return .hd1920x1080
        case (1280, 720): return .hd1280x720
        case (960, 540): return .iFrame960x540
        default: return nil
        }
    }

    /// The landscape rotation for a horizon-level angle: 0 and 180 are landscape and are taken;
    /// upright angles (90, 270) keep the connection on its last landscape angle.
    static func landscapeAngle(forHorizonLevel angle: CGFloat, current: CGFloat) -> CGFloat {
        let normalized = (angle.truncatingRemainder(dividingBy: 360) + 360).truncatingRemainder(dividingBy: 360)
        switch normalized {
        case 0, 180: return normalized
        default: return current == 180 ? 180 : 0
        }
    }

    private static func applyLandscape(_ angle: CGFloat, to connection: AVCaptureConnection) {
        let target = landscapeAngle(forHorizonLevel: angle, current: connection.videoRotationAngle)
        if connection.videoRotationAngle != target, connection.isVideoRotationAngleSupported(target) {
            connection.videoRotationAngle = target
        }
    }

    // MARK: - Sample buffers

    func captureOutput(_ output: AVCaptureOutput, didOutput sampleBuffer: CMSampleBuffer, from connection: AVCaptureConnection) {
        let time = CMSampleBufferGetPresentationTimeStamp(sampleBuffer)
        if output === videoOutput {
            guard let pixelBuffer = CMSampleBufferGetImageBuffer(sampleBuffer) else { return }
            onVideo?(pixelBuffer, time)
        } else if output === audioOutput, let pcm = Self.pcmBuffer(sampleBuffer) {
            onAudio?(pcm, time)
        }
    }

    /// The PCM of a capture sample buffer, copied out of it.
    static func pcmBuffer(_ sampleBuffer: CMSampleBuffer) -> AVAudioPCMBuffer? {
        guard let description = CMSampleBufferGetFormatDescription(sampleBuffer) else { return nil }
        let format = AVAudioFormat(cmAudioFormatDescription: description)
        let frames = CMSampleBufferGetNumSamples(sampleBuffer)
        guard frames > 0, let buffer = AVAudioPCMBuffer(pcmFormat: format, frameCapacity: AVAudioFrameCount(frames))
        else { return nil }
        buffer.frameLength = AVAudioFrameCount(frames)
        let status = CMSampleBufferCopyPCMDataIntoAudioBufferList(
            sampleBuffer, at: 0, frameCount: Int32(frames), into: buffer.mutableAudioBufferList)
        return status == noErr ? buffer : nil
    }
}
