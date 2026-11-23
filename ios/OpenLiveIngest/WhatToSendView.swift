import AVFoundation
import SwiftUI

/// Step 2, "What to send" (docs/specs/ios-app.md, section 5): the lens, the microphone and the
/// label, over a live preview of the lens. The Studio name is `<gateway.name> — <label>`; only the
/// label is editable here (the venue name is under Advanced). Saved with `set_input`.
struct WhatToSendView: View {
    @ObservedObject var model: AppModel
    let next: () -> Void

    @State private var lenses: [Lens] = []
    @State private var microphones: [Microphone] = []
    @State private var lensID = ""
    @State private var microphoneID = ""
    @State private var label = ""
    @State private var preview = PreviewSession()

    private var lens: Lens? { lenses.first { $0.id == lensID } }

    var body: some View {
        ZStack {
            PreviewBackdrop(session: preview.session, lens: lens)
            ScrollView {
                VStack(alignment: .leading, spacing: 14) {
                    Picker("Lens", selection: $lensID) {
                        ForEach(lenses) { Text($0.name).tag($0.id) }
                    }
                    .pickerStyle(.menu)
                    .accessibilityIdentifier("lensPicker")

                    Picker("Microphone", selection: $microphoneID) {
                        Text("Default microphone").tag("")
                        ForEach(microphones) { Text($0.name).tag($0.id) }
                    }
                    .pickerStyle(.menu)
                    .accessibilityIdentifier("microphonePicker")

                    TextField("Label", text: $label)
                        .textInputAutocapitalization(.words)
                        .textFieldStyle(.roundedBorder)
                        .accessibilityIdentifier("labelField")
                    Text("In Studio: \(studioName)")
                        .font(.footnote)
                        .accessibilityIdentifier("studioName")

                    if let problem = model.problem {
                        Text(problem)
                            .foregroundStyle(.red)
                            .accessibilityIdentifier("problem")
                    }

                    Button("Next") {
                        guard let lens else { return }
                        let microphone = microphones.first { $0.id == microphoneID }
                        model.setInput(lens: lens, microphone: microphone, label: label) {
                            preview.stop()
                            next()
                        }
                    }
                    .buttonStyle(.borderedProminent)
                    .disabled(lens == nil || label.trimmingCharacters(in: .whitespaces).isEmpty || model.busy)
                    .accessibilityIdentifier("toGoLive")
                }
                .padding()
                .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 12))
                .padding()
            }
        }
        .navigationTitle("What to send")
        .disabled(model.live)
        .onAppear(perform: load)
        .onDisappear { preview.stop() }
        .onChange(of: lensID) { _, _ in showPreview() }
    }

    private var studioName: String {
        let venue = model.view?.advanced?.name ?? ""
        let shown = label.trimmingCharacters(in: .whitespaces)
        return venue.isEmpty ? shown : "\(venue) — \(shown)"
    }

    private func load() {
        lenses = Devices.lenses()
        microphones = Devices.microphones()
        let saved = model.view?.input
        let pick = lenses.first { $0.id == saved?.device } ?? lenses.first { $0.name.hasPrefix("Back wide") }
            ?? lenses.first
        lensID = pick?.id ?? ""
        microphoneID = microphones.first { $0.id == saved?.audio }?.id ?? ""
        if let savedLabel = saved?.label, !savedLabel.isEmpty, pick?.id == saved?.device {
            label = savedLabel
        } else if let pick {
            label = Devices.defaultLabel(for: pick)
        }
        showPreview()
    }

    private func showPreview() {
        guard let lens, lens.id != Devices.testPatternID else {
            preview.show(nil)
            return
        }
        let preview = self.preview
        switch AVCaptureDevice.authorizationStatus(for: .video) {
        case .authorized:
            preview.show(lens)
        case .notDetermined:
            AVCaptureDevice.requestAccess(for: .video) { granted in
                DispatchQueue.main.async { if granted { preview.show(lens) } }
            }
        default:
            model.problem = "Open Live Ingest may not use the camera. Allow it in Settings, Privacy & Security, Camera."
        }
    }
}
