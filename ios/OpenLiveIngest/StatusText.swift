import Foundation

/// The words of step 3 (docs/specs/ios-app.md, section 5; desktop spec, section 5.1), from the
/// core's events. Pure, so every case is tested without a session.
enum StatusText {
    /// The state line: `Armed`, `Waiting for production`, `Connecting`, `Live`,
    /// `Failed: <reason>`, `Stopping`. Armed with productions holding the source, none of them
    /// live, is Waiting for production.
    static func stateLine(_ row: InputRow?, productions: [Holder]?, live: Bool) -> String {
        guard let row else { return live ? "Starting" : "Not live" }
        switch row.state {
        case "registering": return "Registering"
        case "armed": return (productions?.isEmpty == false) ? "Waiting for production" : "Armed"
        case "connecting": return "Connecting"
        case "live": return "Live"
        case "failed":
            let reason = row.message?.trimmingCharacters(in: .whitespacesAndNewlines) ?? ""
            return reason.isEmpty ? "Failed" : "Failed: \(reason)"
        case "stopping": return "Stopping"
        case "stopped": return "Stopped"
        default: return row.state.capitalized
        }
    }

    /// `Production: Morning Show (active), slot video_in_1`; several separated by `;`.
    static func productionLine(_ productions: [Holder]?) -> String {
        guard let productions else { return "Production: unknown" }
        if productions.isEmpty { return "Production: none holds this source" }
        let parts = productions.map { holder in
            "\(holder.name) (\(holder.state)), \(holder.slots.count == 1 ? "slot" : "slots") "
                + holder.slots.joined(separator: ", ")
        }
        return "Production: " + parts.joined(separator: "; ")
    }

    /// `Link: connected, 0.2 % dropped, RTT 31 ms`, or `Link: not sending`.
    static func linkLine(_ link: LinkStatus?) -> String {
        guard let link else { return "Link: not sending" }
        var text = "Link: "
        switch link.state {
        case "not_sending": text += "not sending"
        case "connecting": text += "connecting"
        case "connected": text += "connected"
        default: text += link.state
        }
        if let dropped = link.droppedPct {
            text += String(format: ", %.1f %% dropped", dropped)
        }
        if let rtt = link.rttMs {
            text += ", RTT \(Int(rtt.rounded())) ms"
        }
        return text
    }

    /// `Network: Wi-Fi, 4.1 Mbit/s`.
    static func networkLine(_ network: NetworkKind?, bitrateKbps: Int?) -> String {
        var text = "Network: " + (network?.label ?? "unknown")
        if let kbps = bitrateKbps, kbps > 0 {
            text += String(format: ", %.1f Mbit/s", Double(kbps) / 1000)
        }
        return text
    }
}
