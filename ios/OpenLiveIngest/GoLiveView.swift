import SwiftUI

/// Step 3, "Go live / Stop" (docs/specs/ios-app.md, section 5): one button; the state line; the
/// status lines (the production holding the source and its slot, the link, the network and the
/// bitrate); a banner for session-wide notices; and Advanced, collapsed.
struct GoLiveView: View {
    @ObservedObject var model: AppModel
    @State private var showAdvanced = false

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 14) {
                ForEach(model.notices) { notice in
                    NoticeBanner(notice: notice) { model.dismiss(notice) }
                }

                if let name = model.view?.input?.studioName {
                    Text(name)
                        .font(.headline)
                        .accessibilityIdentifier("studioName")
                }

                Button {
                    model.goLiveOrStop()
                } label: {
                    Text(model.live ? "Stop" : "Go live")
                        .frame(maxWidth: .infinity)
                        .padding(.vertical, 6)
                }
                .buttonStyle(.borderedProminent)
                .tint(model.live ? .red : .accentColor)
                .disabled(model.view?.input == nil && !model.live)
                .accessibilityIdentifier("goLive")

                Text(StatusText.stateLine(model.row, productions: model.status?.productions, live: model.live))
                    .font(.title3.bold())
                    .accessibilityIdentifier("stateLine")
                if let message = rowMessage {
                    Text(message)
                        .font(.footnote)
                        .accessibilityIdentifier("stateMessage")
                }

                if model.live {
                    VStack(alignment: .leading, spacing: 4) {
                        Text(StatusText.productionLine(model.status?.productions))
                            .accessibilityIdentifier("productionLine")
                        Text(StatusText.linkLine(model.status?.link))
                            .accessibilityValue(StatusText.linkCounters(model.status?.link))
                            .accessibilityIdentifier("linkLine")
                        Text(StatusText.networkLine(model.network, bitrateKbps: model.bitrateKbps))
                            .accessibilityIdentifier("networkLine")
                    }
                    .font(.callout.monospacedDigit())
                }
                if let stopping = model.stopping {
                    Text("Stopping: \(stopping.done) of \(stopping.total)")
                        .font(.footnote)
                }
                if let problem = model.goProblem {
                    Text(problem)
                        .foregroundStyle(.red)
                        .accessibilityIdentifier("goProblem")
                }

                Divider()
                Button(showAdvanced ? "Hide Advanced" : "Advanced") { showAdvanced.toggle() }
                    .accessibilityIdentifier("advanced")
                if showAdvanced {
                    AdvancedView(model: model)
                }
            }
            .padding()
        }
        .navigationTitle("Go live")
    }

    /// The row's own words, unless the state line already says them (Failed: <reason>).
    private var rowMessage: String? {
        guard let row = model.row, row.state != "failed" else { return nil }
        return row.message
    }
}

/// One session-wide notice: Open Live unreachable, the sign-in ending, a slot added while live.
struct NoticeBanner: View {
    let notice: Notice
    let dismiss: () -> Void

    var body: some View {
        HStack(alignment: .top) {
            Image(systemName: "exclamationmark.triangle.fill")
                .foregroundStyle(.yellow)
            Text(notice.message)
                .font(.footnote)
                .accessibilityIdentifier("notice")
            Spacer()
            Button(action: dismiss) {
                Image(systemName: "xmark")
            }
            .accessibilityLabel("Dismiss")
        }
        .padding(10)
        .background(.yellow.opacity(0.15), in: RoundedRectangle(cornerRadius: 8))
    }
}
