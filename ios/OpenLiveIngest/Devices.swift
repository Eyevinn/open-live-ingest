import AVFoundation

/// A lens the phone offers (docs/specs/ios-app.md, section 5, step 2).
struct Lens: Equatable, Hashable, Identifiable {
    /// `AVCaptureDevice.uniqueID`, or `Devices.testPatternID`.
    let id: String
    let name: String
}

/// A microphone route: `AVAudioSessionPortDescription.uid`.
struct Microphone: Equatable, Hashable, Identifiable {
    let id: String
    let name: String
}

/// What the phone can send: its lenses from `AVCaptureDevice.DiscoverySession` and its
/// microphones from `AVAudioSession.availableInputs`. A device with no camera (the simulator)
/// offers the test pattern instead, which the media sends in place of a camera.
enum Devices {
    static let testPatternID = "test-pattern"
    static let testPattern = Lens(id: testPatternID, name: "Test pattern")

    static func lenses() -> [Lens] {
        let devices = AVCaptureDevice.DiscoverySession(
            deviceTypes: Capture.lensTypes, mediaType: .video, position: .unspecified
        ).devices
        guard !devices.isEmpty else { return [testPattern] }
        return devices.map { Lens(id: $0.uniqueID, name: name(of: $0)) }
    }

    /// "Back wide", "Back ultra wide", "Back telephoto", "Front", as the person knows them.
    static func name(of device: AVCaptureDevice) -> String {
        let side: String
        switch device.position {
        case .back: side = "Back"
        case .front: side = "Front"
        default: side = "External"
        }
        switch device.deviceType {
        case .builtInUltraWideCamera: return "\(side) ultra wide"
        case .builtInTelephotoCamera: return "\(side) telephoto"
        case .builtInWideAngleCamera: return device.position == .front ? side : "\(side) wide"
        default: return device.localizedName
        }
    }

    /// The microphones of the current audio routes. Sets the app's audio session up for
    /// recording first, which lists the inputs; that asks no permission.
    static func microphones() -> [Microphone] {
        let session = AVAudioSession.sharedInstance()
        do {
            try session.setCategory(.playAndRecord, mode: .videoRecording, options: [.allowBluetoothHFP, .mixWithOthers])
        } catch {
            AppLog.warning("the audio session could not be set up to list microphones")
        }
        return (session.availableInputs ?? []).map { Microphone(id: $0.uid, name: $0.portName) }
    }

    /// The label a lens gets until the person changes it.
    static func defaultLabel(for lens: Lens) -> String {
        lens.id == testPatternID ? "Test pattern" : "\(lens.name) camera"
    }
}
