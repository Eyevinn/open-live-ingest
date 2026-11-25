import AVFoundation

/// A sound level in dBFS (docs/specs/ios-app.md, section 5, Level and picture): the highest
/// channel's sample peak and RMS, clamped to -90…0 as the desktop app's (desktop-app.md, section
/// 5.2). Digital silence is -90.
struct AudioLevel: Equatable {
    static let floor: Float = -90

    var peak: Float
    var rms: Float

    /// An amplitude (1 is full scale) in dBFS, clamped to -90…0.
    static func dbfs(_ amplitude: Float) -> Float {
        guard amplitude > 0, amplitude.isFinite else { return floor }
        return min(0, max(floor, 20 * log10(amplitude)))
    }

    /// The louder of the two, channel by channel as the desktop's window: the higher peak and the
    /// higher RMS.
    func loudest(_ other: AudioLevel) -> AudioLevel {
        AudioLevel(peak: max(peak, other.peak), rms: max(rms, other.rms))
    }

    /// The highest channel peak and the highest channel RMS of `buffer`: Float32, Int16 or Int32
    /// samples, interleaved or not. Nil for an empty buffer or another sample format.
    static func measure(_ buffer: AVAudioPCMBuffer) -> AudioLevel? {
        let frames = Int(buffer.frameLength)
        let channels = Int(buffer.format.channelCount)
        guard frames > 0, channels > 0 else { return nil }
        let step = buffer.stride
        let interleaved = buffer.format.isInterleaved
        // Sample `i` of channel `ch`: one pointer per channel, or every channel behind the first.
        let sample: (Int, Int) -> Float
        if let data = buffer.floatChannelData {
            sample = { ch, i in data[interleaved ? 0 : ch][i * step + (interleaved ? ch : 0)] }
        } else if let data = buffer.int16ChannelData {
            sample = { ch, i in Float(data[interleaved ? 0 : ch][i * step + (interleaved ? ch : 0)]) / 32_768 }
        } else if let data = buffer.int32ChannelData {
            sample = { ch, i in Float(data[interleaved ? 0 : ch][i * step + (interleaved ? ch : 0)]) / 2_147_483_648 }
        } else {
            return nil
        }
        var peak: Float = 0
        var meanSquare: Float = 0
        for ch in 0..<channels {
            var channelPeak: Float = 0
            var sum: Float = 0
            for i in 0..<frames {
                let value = sample(ch, i)
                channelPeak = max(channelPeak, abs(value))
                sum += value * value
            }
            peak = max(peak, channelPeak)
            meanSquare = max(meanSquare, sum / Float(frames))
        }
        return AudioLevel(peak: dbfs(peak), rms: dbfs(meanSquare.squareRoot()))
    }
}

/// Whether a microphone gives no sound, as the desktop app's `SilenceDetector` (desktop-app.md,
/// section 5.2): a pure function of 250 ms windows and a clock. Silent when every window's highest
/// peak is below -60 dBFS for 5 s, or no window had any sound at all for 5 s (a microphone that
/// delivers nothing); it clears at the first window whose peak reaches -54 dBFS (6 dB of
/// hysteresis, so a signal hovering at the threshold does not flicker). The clock starts when the
/// source runs.
struct SilenceDetector {
    /// Below this a window is quiet.
    static let threshold: Float = -60
    /// At or above this a window ends the warning.
    static let clearsAt: Float = -54
    /// Quiet for this long is silent.
    static let after: TimeInterval = 5

    private(set) var silent = false
    /// The last window with sound (at or above the threshold), or the start.
    private var lastSound: TimeInterval

    init(start: TimeInterval) {
        lastSound = start
    }

    /// A window's highest peak, or nil when no sound arrived in it, at `time` (seconds, the same
    /// clock as `start`). Whether the microphone is silent now.
    @discardableResult
    mutating func update(peak: Float?, at time: TimeInterval) -> Bool {
        if let peak {
            if peak >= Self.clearsAt {
                silent = false
                lastSound = time
            } else if peak >= Self.threshold, !silent {
                lastSound = time
            }
        }
        if !silent, time - lastSound >= Self.after {
            silent = true
        }
        return silent
    }
}
