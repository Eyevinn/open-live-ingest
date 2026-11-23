import Foundation

/// The encoder settings of the core's `media` start event (docs/specs/ios-app.md, section 8.2):
/// `video: {width, height, fps, bitrate_kbps, keyframe_interval}`.
struct VideoSettings: Equatable, Decodable {
    var width: Int
    var height: Int
    var fps: Double
    var bitrateKbps: Int
    /// Frames from one keyframe to the next.
    var keyframeInterval: Int

    enum CodingKeys: String, CodingKey {
        case width, height, fps
        case bitrateKbps = "bitrate_kbps"
        case keyframeInterval = "keyframe_interval"
    }

    /// Section 11's phone defaults: 1280x720 at 30 fps, H.264 High CBR 4000 kbps, a keyframe every
    /// 30 frames. Provisional until OL-64-0 (#136) has posted its results (section 14 Q13); a change
    /// then is a settings default, not a design change.
    static let defaults = VideoSettings(width: 1280, height: 720, fps: 30, bitrateKbps: 4000, keyframeInterval: 30)
}

/// `audio: {sample_rate, channels, bitrate_kbps}` of the same event. AAC-LC.
struct AudioSettings: Equatable, Decodable {
    var sampleRate: Int
    var channels: Int
    var bitrateKbps: Int

    enum CodingKeys: String, CodingKey {
        case sampleRate = "sample_rate"
        case channels
        case bitrateKbps = "bitrate_kbps"
    }

    /// Section 11: AAC-LC 48 kHz stereo 128 kbps.
    static let defaults = AudioSettings(sampleRate: 48_000, channels: 2, bitrateKbps: 128)
}

/// `uplink: {host, port, latency_ms, passphrase?, pbkeylen?}` of the same event.
struct UplinkSettings: Equatable, Decodable {
    var host: String
    var port: UInt16
    var latencyMs: Int32
    var passphrase: String?
    var pbkeylen: Int32?

    enum CodingKeys: String, CodingKey {
        case host, port, passphrase, pbkeylen
        case latencyMs = "latency_ms"
    }
}

/// The `media` event with `action: "start"`: open capture, encoders and the SRT caller.
struct MediaStart: Equatable, Decodable {
    var input: String
    var name: String?
    /// The lens, an `AVCaptureDevice.uniqueID`; none means the test pattern.
    var device: String?
    /// The microphone, an `AVAudioSessionPortDescription.uid`; none means the default input.
    var microphone: String?
    var testPattern: Bool?
    var uplink: UplinkSettings
    var video: VideoSettings
    var audio: AudioSettings

    enum CodingKeys: String, CodingKey {
        case input, name, device, microphone, uplink, video, audio
        case testPattern = "test_pattern"
    }

    /// Whether to send the test pattern instead of a camera.
    var usesTestPattern: Bool { testPattern == true || device == nil }

    /// The SRT caller's options: the uplink plus the stream's CBR rate, video and audio.
    var senderOptions: SRTSenderOptions {
        SRTSenderOptions(
            host: uplink.host, port: uplink.port, latencyMs: uplink.latencyMs, passphrase: uplink.passphrase,
            pbkeylen: uplink.pbkeylen, bitrateBitsPerSecond: Int64(video.bitrateKbps + audio.bitrateKbps) * 1000)
    }
}

extension MediaStart: CustomStringConvertible, CustomDebugStringConvertible {
    /// Never shows the passphrase.
    var description: String {
        "media start \(input): \(senderOptions), \(video.width)x\(video.height)@\(video.fps) "
            + "\(video.bitrateKbps) kbps gop \(video.keyframeInterval), AAC \(audio.sampleRate) Hz "
            + "\(audio.channels) ch \(audio.bitrateKbps) kbps"
    }

    var debugDescription: String { description }
}
