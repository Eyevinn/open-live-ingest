import SwiftUI

/// Step 1, "Where to send" (docs/specs/ios-app.md, sections 5 and 6): sign in with Open Source
/// Cloud and pick one of the workspace's Open Live instances; under Advanced, a self-hosted Open
/// Live or a pasted personal access token. Saved at once; later launches open on "Sending to
/// <instance> (<workspace>)" with Change.
struct WhereToSendView: View {
    @ObservedObject var model: AppModel
    let next: () -> Void

    @State private var changing = false
    @State private var showAdvanced = false
    @State private var token = ""
    @State private var selfHostedURL = ""
    @State private var selfHostedKey = ""

    private var place: WhereView? { model.whereToSend }
    private var ready: Bool { place?.ready == true }

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 16) {
                if ready && !changing {
                    summary
                } else {
                    chooser
                }
                if let problem = model.problem {
                    Text(problem)
                        .foregroundStyle(.red)
                        .accessibilityIdentifier("problem")
                }
            }
            .padding()
        }
        .navigationTitle("Where to send")
        .disabled(model.live)
    }

    private var summary: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text(place?.summary ?? "Sending to \(place?.instance ?? "Open Live")")
                .font(.headline)
                .accessibilityIdentifier("whereSummary")
            HStack {
                Button("Change") { changing = true }
                    .accessibilityIdentifier("whereChange")
                Spacer()
                Button("Next") { next() }
                    .buttonStyle(.borderedProminent)
                    .accessibilityIdentifier("toWhat")
            }
        }
    }

    private var chooser: some View {
        VStack(alignment: .leading, spacing: 16) {
            Button {
                model.signIn()
            } label: {
                Label(
                    model.signingIn ? "Signing in…" : "Sign in with Open Source Cloud",
                    systemImage: "person.crop.circle.badge.checkmark")
            }
            .buttonStyle(.borderedProminent)
            .disabled(model.signingIn)
            .accessibilityIdentifier("signIn")

            if let place, !place.instances.isEmpty {
                instances(place)
            }

            Button(showAdvanced ? "Hide Advanced" : "Advanced") { showAdvanced.toggle() }
                .accessibilityIdentifier("whereAdvanced")
            if showAdvanced {
                advanced
            }

            if ready {
                Button("Done") { changing = false }
                    .accessibilityIdentifier("whereDone")
            }
        }
    }

    private func instances(_ place: WhereView) -> some View {
        VStack(alignment: .leading, spacing: 8) {
            Text(place.workspace.map { "Open Live in \($0)" } ?? "Open Live instances")
                .font(.headline)
            ForEach(place.instances, id: \.self) { instance in
                Button {
                    model.chooseInstance(instance)
                    changing = false
                } label: {
                    HStack {
                        Text(instance.name)
                        Spacer()
                        if instance.url == place.url {
                            Image(systemName: "checkmark")
                        }
                    }
                }
                .accessibilityIdentifier("instance \(instance.name)")
            }
        }
    }

    private var advanced: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("Paste a personal access token").font(.subheadline.bold())
            HStack {
                SecureField("Personal access token", text: $token)
                    .textInputAutocapitalization(.never)
                    .autocorrectionDisabled()
                    .textFieldStyle(.roundedBorder)
                    .onSubmit(useToken)
                    .accessibilityIdentifier("patField")
                Button("Use", action: useToken)
                    .disabled(token.isEmpty || model.busy)
                    .accessibilityIdentifier("patSubmit")
            }

            Text("Use a self-hosted Open Live").font(.subheadline.bold())
            TextField("https://open-live.example.com", text: $selfHostedURL)
                .keyboardType(.URL)
                .textInputAutocapitalization(.never)
                .autocorrectionDisabled()
                .textFieldStyle(.roundedBorder)
                .accessibilityIdentifier("selfHostedURL")
            SecureField("API key (blank for none)", text: $selfHostedKey)
                .textInputAutocapitalization(.never)
                .autocorrectionDisabled()
                .textFieldStyle(.roundedBorder)
                .accessibilityIdentifier("selfHostedKey")
            Button("Use this Open Live") {
                model.selfHosted(url: selfHostedURL, apiKey: selfHostedKey)
                selfHostedKey = ""
                changing = false
            }
            .disabled(selfHostedURL.isEmpty || model.busy)
            .accessibilityIdentifier("selfHostedSubmit")
        }
    }

    private func useToken() {
        guard !token.isEmpty else { return }
        model.pasteToken(token)
        token = ""
    }
}
