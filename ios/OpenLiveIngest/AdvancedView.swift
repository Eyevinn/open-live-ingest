import SwiftUI

/// Advanced (docs/specs/ios-app.md, section 5): the venue name, the uplink's latency, passphrase
/// and key length, the video's bitrate, resolution and frame rate, and the log level, saved with
/// `set_advanced` (its `problems[]` shown, nothing saved then); Forget sign-in; Share logs; About.
/// The values are the library's settings; their defaults are section 11's, provisional until
/// OL-64-0 (#136) has measured them, so nothing here fixes them.
struct AdvancedView: View {
    @ObservedObject var model: AppModel

    @State private var name = ""
    @State private var latency = ""
    @State private var passphrase = ""
    @State private var clearPassphrase = false
    @State private var pbkeylen = 0
    @State private var bitrate = ""
    @State private var resolution = ""
    @State private var framerate = ""
    @State private var logLevel = "info"
    @State private var showAbout = false

    static let keyLengths = [0, 16, 24, 32]
    static let logLevels = ["error", "warn", "info", "debug", "trace"]

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Group {
                field("Venue name", text: $name, id: "venueName")
                field("Uplink latency (ms)", text: $latency, id: "latency", numbers: true)
                SecureField(
                    model.view?.advanced?.passphraseSet == true ? "Passphrase (set; blank keeps it)" : "Passphrase (none)",
                    text: $passphrase
                )
                .textInputAutocapitalization(.never)
                .autocorrectionDisabled()
                .textFieldStyle(.roundedBorder)
                .accessibilityIdentifier("passphrase")
                if model.view?.advanced?.passphraseSet == true {
                    Toggle("Remove the passphrase", isOn: $clearPassphrase)
                }
                Picker("Key length", selection: $pbkeylen) {
                    ForEach(Self.keyLengths, id: \.self) { length in
                        Text(length == 0 ? "Default" : "\(length * 8) bit").tag(length)
                    }
                }
                field("Video bitrate (kbit/s)", text: $bitrate, id: "bitrate", numbers: true)
                field("Resolution (WxH)", text: $resolution, id: "resolution")
                field("Frame rate (N/D)", text: $framerate, id: "framerate")
                Picker("Log level", selection: $logLevel) {
                    ForEach(Self.logLevels, id: \.self) { Text($0).tag($0) }
                }
            }

            ForEach(model.advancedProblems, id: \.self) { problem in
                Text(problem.message).foregroundStyle(.red).font(.footnote)
            }
            ForEach(model.advancedWarnings, id: \.self) { warning in
                Text(warning.message).foregroundStyle(.orange).font(.footnote)
            }

            Button("Save") { save() }
                .buttonStyle(.bordered)
                .disabled(model.live || model.busy)
                .accessibilityIdentifier("saveAdvanced")

            Divider()
            if model.whereToSend?.signedIn == true {
                Button("Forget sign-in", role: .destructive) { model.forgetSignIn() }
                    .disabled(model.live)
                    .accessibilityIdentifier("forgetSignIn")
            }
            if let logFile = model.logFile {
                ShareLink(item: logFile) {
                    Label("Share logs", systemImage: "square.and.arrow.up")
                }
                .accessibilityIdentifier("shareLogs")
            }
            Button("About") { showAbout = true }
                .accessibilityIdentifier("about")
        }
        .onAppear(perform: load)
        .onChange(of: model.view) { _, _ in load() }
        .sheet(isPresented: $showAbout) {
            AboutView(version: model.view?.version)
        }
    }

    private func field(_ title: String, text: Binding<String>, id: String, numbers: Bool = false) -> some View {
        VStack(alignment: .leading, spacing: 2) {
            Text(title).font(.caption).foregroundStyle(.secondary)
            TextField(title, text: text)
                .keyboardType(numbers ? .numberPad : .default)
                .textInputAutocapitalization(.never)
                .autocorrectionDisabled()
                .textFieldStyle(.roundedBorder)
                .accessibilityIdentifier(id)
        }
    }

    private func load() {
        guard let saved = model.view?.advanced else { return }
        name = saved.name
        latency = String(saved.latencyMs)
        pbkeylen = saved.pbkeylen ?? 0
        bitrate = String(saved.bitrateKbps)
        resolution = saved.resolution ?? ""
        framerate = saved.framerate ?? ""
        logLevel = saved.logLevel
        passphrase = ""
        clearPassphrase = false
    }

    private func save() {
        var values = AdvancedValues()
        values.name = name
        values.latencyMs = Int(latency.trimmingCharacters(in: .whitespaces))
        if !passphrase.isEmpty {
            values.passphrase = passphrase
        } else if clearPassphrase {
            values.passphrase = ""
        }
        values.pbkeylen = pbkeylen
        values.bitrateKbps = Int(bitrate.trimmingCharacters(in: .whitespaces))
        values.resolution = resolution
        values.framerate = framerate
        values.logLevel = logLevel
        model.setAdvanced(values)
        passphrase = ""
    }
}

/// About: the version, and the third-party code the app ships with its licence and source.
struct AboutView: View {
    let version: String?
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        NavigationStack {
            List {
                Section("Open Live Ingest") {
                    Text("Version \(version ?? Self.bundleVersion)")
                    Link("Source code", destination: URL(string: "https://github.com/Eyevinn/open-live-ingest")!)
                }
                ForEach(ThirdPartyLicenses.all) { component in
                    Section(component.name) {
                        Text("\(component.version), \(component.license)")
                        Text(component.note).font(.footnote)
                        Link("Licence", destination: component.licenseText)
                        Link("Source", destination: component.source)
                    }
                }
            }
            .navigationTitle("About")
            .toolbar {
                ToolbarItem(placement: .confirmationAction) {
                    Button("Done") { dismiss() }
                }
            }
        }
    }

    static var bundleVersion: String {
        Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String ?? "?"
    }
}
