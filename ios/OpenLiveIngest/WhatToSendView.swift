import AVFoundation
import SwiftUI

/// Step 2, "What to send" (docs/specs/ios-app.md, section 5): the lens, the microphone with its
/// level bar, and the label, over a live preview of the lens. The preview and the level run only
/// while this screen is shown and the app is in the foreground. The Studio name is
/// `<gateway.name> — <label>`; only the label is editable here (the venue name is under Advanced).
/// Saved with `set_input`.
struct WhatToSendView: View {
    @ObservedObject var model: AppModel
    let next: () -> Void

    @State private var lenses: [Lens] = []
    @State private var microphones: [Microphone] = []
    @State private var lensID = ""
    @State private var microphoneID = ""
    @State private var label = ""
    @State private var preview = PreviewSession()
    @State private var onScreen = false
    @Environment(\.scenePhase) private var scenePhase

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
                    PreviewLevel(display: preview.display)

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
        .onAppear {
            onScreen = true
            load()
        }
        .onDisappear {
            onScreen = false
            preview.stop()
        }
        .onChange(of: lensID) { _, _ in showPreview() }
        .onChange(of: microphoneID) { _, _ in showPreview() }
        .onChange(of: scenePhase) { _, phase in
            // Nothing runs in the background: the camera and the level stop, and come back with
            // the app if this screen is still the one shown.
            if phase == .background {
                preview.stop()
            } else if phase == .active, onScreen {
                showPreview()
            }
        }
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
        guard onScreen, let lens, lens.id != Devices.testPatternID else {
            preview.show(nil, microphone: nil, sound: false)
            return
        }
        let preview = self.preview
        let microphone = microphoneID.isEmpty ? nil : microphoneID
        switch AVCaptureDevice.authorizationStatus(for: .video) {
        case .authorized:
            PreviewSession.microphoneAllowed { sound in preview.show(lens, microphone: microphone, sound: sound) }
        case .notDetermined:
            AVCaptureDevice.requestAccess(for: .video) { granted in
                DispatchQueue.main.async {
                    guard granted else { return }
                    PreviewSession.microphoneAllowed { sound in
                        preview.show(lens, microphone: microphone, sound: sound)
                    }
                }
            }
        default:
            model.problem = "Open Live Ingest may not use the camera. Allow it in Settings, Privacy & Security, Camera."
        }
    }
}

/// Step 2's level bar for the picked microphone, while the preview measures it.
private struct PreviewLevel: View {
    @ObservedObject var display: MonitorDisplay

    var body: some View {
        if display.running {
            LevelBar(display: display)
        }
    }
}
