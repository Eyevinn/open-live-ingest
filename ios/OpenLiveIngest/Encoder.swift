import AVFoundation
import CoreMedia
import VideoToolbox

enum EncoderError: Error, Equatable {
    case videoSession(OSStatus)
    case videoProperty(name: String, status: OSStatus)
    case audioFormat
    case audioConverter
}

/// One encoded H.264 access unit: NAL units without start codes or length prefixes, and the
/// session's current SPS and PPS for the muxer to put in front of an IDR.
struct EncodedVideoFrame {
    let nalUnits: [Data]
    let sps: Data?
    let pps: Data?
    let pts: CMTime
    /// Invalid when it equals `pts` (no frame reordering).
    let dts: CMTime
    let isKeyframe: Bool
    var byteCount: Int { nalUnits.reduce(0) { $0 + $1.count } }
}

/// VideoToolbox H.264 High at the settings of the `media` start event (docs/specs/ios-app.md,
/// sections 8.2 and 11): CBR, a keyframe every `keyframeInterval` frames, no B-frames.
///
/// Rate control is `kVTCompressionPropertyKey_ConstantBitRate` where the encoder offers it, and
/// `AverageBitRate` with `DataRateLimits` over 1.0 s otherwise (the two are alternatives: the
/// constant-bitrate key does not combine with the other two). `rateControl` says which one the
/// session took. Every `keyframeInterval`-th frame handed in is forced to a keyframe, so the
/// cadence counts frames even when the encoder would choose its own.
///
/// `encode` is called from one thread at a time; `output` runs on VideoToolbox's thread.
final class VideoEncoder {
    enum RateControl: String {
        case constantBitRate = "ConstantBitRate"
        case averageWithDataRateLimits = "AverageBitRate+DataRateLimits"
    }

    let settings: VideoSettings
    let rateControl: RateControl
    /// Called with each encoded frame, on VideoToolbox's thread.
    var output: ((EncodedVideoFrame) -> Void)?
    /// Called with a failed frame's status, on VideoToolbox's thread.
    var onError: ((OSStatus) -> Void)?

    private let session: VTCompressionSession
    private var frameCount = 0

    init(settings: VideoSettings) throws {
        self.settings = settings
        var created: VTCompressionSession?
        let status = VTCompressionSessionCreate(
            allocator: kCFAllocatorDefault, width: Int32(settings.width), height: Int32(settings.height),
            codecType: kCMVideoCodecType_H264, encoderSpecification: nil, imageBufferAttributes: nil,
            compressedDataAllocator: nil, outputCallback: nil, refcon: nil, compressionSessionOut: &created)
        guard status == noErr, let session = created else { throw EncoderError.videoSession(status) }
        self.session = session

        let gop = max(1, settings.keyframeInterval)
        let bitsPerSecond = settings.bitrateKbps * 1000
        do {
            // Best effort: a hint for the encoder.
            _ = VTSessionSetProperty(session, key: kVTCompressionPropertyKey_RealTime, value: kCFBooleanTrue)
            _ = VTSessionSetProperty(
                session, key: kVTCompressionPropertyKey_ExpectedFrameRate, value: NSNumber(value: settings.fps))

            try Self.set(session, kVTCompressionPropertyKey_ProfileLevel, "ProfileLevel",
                         kVTProfileLevel_H264_High_AutoLevel)
            try Self.set(session, kVTCompressionPropertyKey_AllowFrameReordering, "AllowFrameReordering",
                         kCFBooleanFalse)
            try Self.set(session, kVTCompressionPropertyKey_MaxKeyFrameInterval, "MaxKeyFrameInterval",
                         NSNumber(value: gop))
            try Self.set(session, kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration, "MaxKeyFrameIntervalDuration",
                         NSNumber(value: Double(gop) / max(settings.fps, 1)))

            if Self.offers(session, kVTCompressionPropertyKey_ConstantBitRate),
                VTSessionSetProperty(session, key: kVTCompressionPropertyKey_ConstantBitRate,
                                     value: NSNumber(value: bitsPerSecond)) == noErr
            {
                rateControl = .constantBitRate
            } else {
                try Self.set(session, kVTCompressionPropertyKey_AverageBitRate, "AverageBitRate",
                             NSNumber(value: bitsPerSecond))
                let limits = [NSNumber(value: bitsPerSecond / 8), NSNumber(value: 1.0)] as CFArray
                try Self.set(session, kVTCompressionPropertyKey_DataRateLimits, "DataRateLimits", limits)
                rateControl = .averageWithDataRateLimits
            }
            let prepared = VTCompressionSessionPrepareToEncodeFrames(session)
            guard prepared == noErr else { throw EncoderError.videoSession(prepared) }
        } catch {
            VTCompressionSessionInvalidate(session)
            throw error
        }
    }

    deinit {
        VTCompressionSessionInvalidate(session)
    }

    /// Encodes one frame; the result comes to `output`.
    func encode(_ pixelBuffer: CVPixelBuffer, pts: CMTime, duration: CMTime = .invalid) {
        let forceKeyframe = frameCount % max(1, settings.keyframeInterval) == 0
        frameCount += 1
        let properties: CFDictionary? =
            forceKeyframe ? [kVTEncodeFrameOptionKey_ForceKeyFrame as String: true] as CFDictionary : nil
        let status = VTCompressionSessionEncodeFrame(
            session, imageBuffer: pixelBuffer, presentationTimeStamp: pts, duration: duration,
            frameProperties: properties, infoFlagsOut: nil
        ) { [weak self] status, _, sampleBuffer in
            guard let self else { return }
            guard status == noErr, let sampleBuffer else {
                if status != noErr { self.onError?(status) }
                return
            }
            if let frame = Self.frame(from: sampleBuffer) { self.output?(frame) }
        }
        if status != noErr { onError?(status) }
    }

    /// Waits for the frames handed in up to `pts` (all of them for `.invalid`) to come out.
    func complete(until pts: CMTime = .invalid) {
        VTCompressionSessionCompleteFrames(session, untilPresentationTimeStamp: pts)
    }

    // MARK: -

    private static func set(_ session: VTCompressionSession, _ key: CFString, _ name: String, _ value: CFTypeRef) throws {
        let status = VTSessionSetProperty(session, key: key, value: value)
        guard status == noErr else { throw EncoderError.videoProperty(name: name, status: status) }
    }

    private static func offers(_ session: VTCompressionSession, _ key: CFString) -> Bool {
        var supported: CFDictionary?
        guard VTSessionCopySupportedPropertyDictionary(session, supportedPropertyDictionaryOut: &supported) == noErr,
            let dictionary = supported as NSDictionary?
        else { return false }
        return dictionary[key as String] != nil
    }

    private static func frame(from sampleBuffer: CMSampleBuffer) -> EncodedVideoFrame? {
        guard CMSampleBufferDataIsReady(sampleBuffer),
            let block = CMSampleBufferGetDataBuffer(sampleBuffer),
            let format = CMSampleBufferGetFormatDescription(sampleBuffer)
        else { return nil }
        let length = CMBlockBufferGetDataLength(block)
        var bytes = Data(count: length)
        let copied = bytes.withUnsafeMutableBytes { raw -> OSStatus in
            guard let base = raw.baseAddress else { return kCMBlockBufferBadPointerParameterErr }
            return CMBlockBufferCopyDataBytes(block, atOffset: 0, dataLength: length, destination: base)
        }
        guard copied == kCMBlockBufferNoErr else { return nil }

        var keyframe = true
        if let attachments = CMSampleBufferGetSampleAttachmentsArray(sampleBuffer, createIfNecessary: false)
            as? [NSDictionary], let first = attachments.first
        {
            keyframe = !((first[kCMSampleAttachmentKey_NotSync] as? NSNumber)?.boolValue ?? false)
        }

        var count = 0
        var headerLength: Int32 = 4
        _ = CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
            format, parameterSetIndex: 0, parameterSetPointerOut: nil, parameterSetSizeOut: nil,
            parameterSetCountOut: &count, nalUnitHeaderLengthOut: &headerLength)
        var sps: Data?
        var pps: Data?
        for i in 0..<count {
            var pointer: UnsafePointer<UInt8>?
            var size = 0
            guard CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
                format, parameterSetIndex: i, parameterSetPointerOut: &pointer, parameterSetSizeOut: &size,
                parameterSetCountOut: nil, nalUnitHeaderLengthOut: nil) == noErr,
                let pointer
            else { continue }
            let set = Data(bytes: pointer, count: size)
            switch H264.nalType(set) {
            case H264.NALType.sps where sps == nil: sps = set
            case H264.NALType.pps where pps == nil: pps = set
            default: break
            }
        }

        let pts = CMSampleBufferGetPresentationTimeStamp(sampleBuffer)
        let dts = CMSampleBufferGetDecodeTimeStamp(sampleBuffer)
        return EncodedVideoFrame(
            nalUnits: H264.nalUnits(avcc: bytes, lengthSize: Int(headerLength)), sps: sps, pps: pps, pts: pts,
            dts: dts, isKeyframe: keyframe)
    }
}

/// AudioToolbox AAC-LC (through `AVAudioConverter`) at the settings of the `media` start event:
/// 48 kHz stereo 128 kbps by default. Takes any PCM; other rates or a mono input are converted
/// first (mono to both channels). Each AAC frame comes out raw (no ADTS header; the muxer adds it)
/// with its presentation time: the first sample's time plus 1024 samples a frame, less the
/// encoder's priming, re-based when the input jumps by more than 100 ms.
///
/// Not thread safe: `encode` from one thread at a time; `output` runs on that thread.
final class AudioEncoder {
    /// The encoder's priming (`AudioConverterPrimeInfo.leadingFrames` of Apple's AAC encoder):
    /// the first frame's audio starts this many samples before the first sample handed in.
    static let primingSamples: Int64 = 2112
    static let samplesPerFrame: Int64 = 1024

    let settings: AudioSettings
    /// What `encode` takes without converting: Float32, non-interleaved, at the settings.
    let pcmFormat: AVAudioFormat
    /// Called with each raw AAC frame and its time.
    var output: ((Data, CMTime) -> Void)?

    private let aacFormat: AVAudioFormat
    private let converter: AVAudioConverter
    private var resampler: AVAudioConverter?
    private var queued: [AVAudioPCMBuffer] = []
    private var baseSample: Int64?
    private var samplesIn: Int64 = 0
    private var framesOut: Int64 = 0

    init(settings: AudioSettings) throws {
        self.settings = settings
        guard let pcm = AVAudioFormat(
            commonFormat: .pcmFormatFloat32, sampleRate: Double(settings.sampleRate),
            channels: AVAudioChannelCount(settings.channels), interleaved: false)
        else { throw EncoderError.audioFormat }
        var description = AudioStreamBasicDescription(
            mSampleRate: Double(settings.sampleRate), mFormatID: kAudioFormatMPEG4AAC,
            mFormatFlags: UInt32(MPEG4ObjectID.AAC_LC.rawValue), mBytesPerPacket: 0,
            mFramesPerPacket: UInt32(Self.samplesPerFrame), mBytesPerFrame: 0,
            mChannelsPerFrame: UInt32(settings.channels), mBitsPerChannel: 0, mReserved: 0)
        guard let aac = AVAudioFormat(streamDescription: &description) else { throw EncoderError.audioFormat }
        guard let converter = AVAudioConverter(from: pcm, to: aac) else { throw EncoderError.audioConverter }
        converter.bitRateStrategy = AVAudioBitRateStrategy_Constant
        converter.bitRate = settings.bitrateKbps * 1000
        pcmFormat = pcm
        aacFormat = aac
        self.converter = converter
    }

    /// Encodes `buffer`, whose first sample was captured at `time`.
    func encode(_ buffer: AVAudioPCMBuffer, time: CMTime) {
        guard let pcm = canonical(buffer), pcm.frameLength > 0 else { return }
        let rate = Int64(settings.sampleRate)
        let sample = CMTimeConvertScale(time, timescale: Int32(rate), method: .roundHalfAwayFromZero).value
        if let base = baseSample {
            let drift = sample - (base + samplesIn)
            if abs(drift) > rate / 10 { baseSample = base + drift }
        } else {
            baseSample = sample
        }
        samplesIn += Int64(pcm.frameLength)
        queued.append(pcm)
        drain()
    }

    private func drain() {
        let maximumPacketSize = max(converter.maximumOutputPacketSize, 1536)
        while true {
            let out = AVAudioCompressedBuffer(format: aacFormat, packetCapacity: 8, maximumPacketSize: maximumPacketSize)
            var error: NSError?
            let status = converter.convert(to: out, error: &error) { [weak self] _, inputStatus in
                guard let self, !self.queued.isEmpty else {
                    inputStatus.pointee = .noDataNow
                    return nil
                }
                inputStatus.pointee = .haveData
                return self.queued.removeFirst()
            }
            emit(out)
            if status != .haveData || out.packetCount == 0 { return }
        }
    }

    private func emit(_ out: AVAudioCompressedBuffer) {
        guard let descriptions = out.packetDescriptions, let base = baseSample else { return }
        for i in 0..<Int(out.packetCount) {
            let d = descriptions[i]
            let data = Data(bytes: out.data.advanced(by: Int(d.mStartOffset)), count: Int(d.mDataByteSize))
            let start = base + framesOut * Self.samplesPerFrame - Self.primingSamples
            framesOut += 1
            output?(data, CMTime(value: start, timescale: Int32(settings.sampleRate)))
        }
    }

    /// `buffer` in `pcmFormat`, converted when it is not.
    private func canonical(_ buffer: AVAudioPCMBuffer) -> AVAudioPCMBuffer? {
        if buffer.format == pcmFormat { return buffer }
        if resampler?.inputFormat != buffer.format {
            resampler = AVAudioConverter(from: buffer.format, to: pcmFormat)
            if buffer.format.channelCount == 1 && pcmFormat.channelCount > 1 {
                resampler?.channelMap = Array(repeating: 0, count: Int(pcmFormat.channelCount))
            }
        }
        guard let resampler else { return nil }
        let ratio = pcmFormat.sampleRate / buffer.format.sampleRate
        let capacity = AVAudioFrameCount((Double(buffer.frameLength) * ratio).rounded(.up)) + 64
        guard let out = AVAudioPCMBuffer(pcmFormat: pcmFormat, frameCapacity: capacity) else { return nil }
        var handed = false
        var error: NSError?
        let status = resampler.convert(to: out, error: &error) { _, inputStatus in
            if handed {
                inputStatus.pointee = .noDataNow
                return nil
            }
            handed = true
            inputStatus.pointee = .haveData
            return buffer
        }
        return status == .error ? nil : out
    }
}
