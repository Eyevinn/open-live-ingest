// The simulator sign-in check of OL-64b (docs/specs/ios-app.md, sections 6 and 14 Q4):
// a throwaway harness, not the app (OL-64e builds that under ios/).
//
// It links the Rust core (mobile/, a debug build pointed at fake_osc.py through
// OLI_TEST_OSC_URL), sends `osc_sign_in`, presents the `osc_sign_in_page` URL in an
// ASWebAuthenticationSession (prefersEphemeralWebBrowserSession = false, as the app
// will), and cancels the session the moment `osc_signed_in` arrives, as section 6 says
// the app does. What happened is shown in labels the UI test reads:
//   page        "started" once the session is presented
//   signedIn    "yes" once the core says the loopback callback arrived
//   cancelled   "yes" once cancel() was called
//   completion  the session's completion: "canceledLogin", another error, "callback"
//               (it completed on its own), or "not called" 2 s after cancel()
//   reply       the osc_sign_in reply: "ok" or "error: <message>"
//   events      the event names, in order

import AuthenticationServices
import UIKit

@main
final class AppDelegate: UIResponder, UIApplicationDelegate {
    var window: UIWindow?

    func application(
        _ application: UIApplication,
        didFinishLaunchingWithOptions launchOptions: [UIApplication.LaunchOptionsKey: Any]? = nil
    ) -> Bool {
        let window = UIWindow(frame: UIScreen.main.bounds)
        window.rootViewController = SignInCheckViewController()
        window.makeKeyAndVisible()
        self.window = window
        return true
    }
}

final class SignInCheckViewController: UIViewController, ASWebAuthenticationPresentationContextProviding {
    private var core: OpaquePointer?
    private var session: ASWebAuthenticationSession?
    private var labels: [String: UILabel] = [:]
    private var events: [String] = []

    override func viewDidLoad() {
        super.viewDidLoad()
        view.backgroundColor = .systemBackground
        let stack = UIStackView()
        stack.axis = .vertical
        stack.spacing = 8
        stack.translatesAutoresizingMaskIntoConstraints = false
        view.addSubview(stack)
        NSLayoutConstraint.activate([
            stack.leadingAnchor.constraint(equalTo: view.safeAreaLayoutGuide.leadingAnchor, constant: 16),
            stack.trailingAnchor.constraint(equalTo: view.safeAreaLayoutGuide.trailingAnchor, constant: -16),
            stack.topAnchor.constraint(equalTo: view.safeAreaLayoutGuide.topAnchor, constant: 16),
        ])
        let button = UIButton(type: .system)
        button.setTitle("Sign in", for: .normal)
        button.accessibilityIdentifier = "signIn"
        button.addTarget(self, action: #selector(signIn), for: .touchUpInside)
        stack.addArrangedSubview(button)
        for key in ["page", "signedIn", "cancelled", "completion", "reply", "events"] {
            let label = UILabel()
            label.numberOfLines = 0
            label.accessibilityIdentifier = key
            label.text = "-"
            stack.addArrangedSubview(label)
            labels[key] = label
        }
        startCore()
    }

    private func set(_ key: String, _ text: String) {
        labels[key]?.text = text
        NSLog("sign-in-check: %@ = %@", key, text)
    }

    private func startCore() {
        let files = FileManager.default
        let support = files.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
        let logs = files.urls(for: .cachesDirectory, in: .userDomainMask)[0].appendingPathComponent("logs")
        try? files.createDirectory(
            at: support.appendingPathComponent("open-live-ingest"), withIntermediateDirectories: true)
        try? files.createDirectory(at: logs, withIntermediateDirectories: true)
        let ctx = Unmanaged.passUnretained(self).toOpaque()
        core = oli_core_new(support.path, logs.path, { ctx, json in
            guard let ctx, let json else { return }
            let text = String(cString: json)
            let me = Unmanaged<SignInCheckViewController>.fromOpaque(ctx).takeUnretainedValue()
            DispatchQueue.main.async { me.received(text) }
        }, ctx)
        set("reply", core == nil ? "error: the core did not start" : "-")
    }

    @objc private func signIn() {
        guard let core else { return }
        oli_core_send(core, #"{"id": 1, "cmd": "osc_sign_in"}"#)
    }

    private func received(_ text: String) {
        guard let data = text.data(using: .utf8),
              let message = try? JSONSerialization.jsonObject(with: data) as? [String: Any]
        else { return }
        if let event = message["event"] as? String {
            events.append(event)
            set("events", events.joined(separator: ","))
            let body = message["data"] as? [String: Any]
            switch event {
            case "osc_sign_in_page":
                if let address = body?["url"] as? String, let url = URL(string: address) {
                    present(url)
                }
            case "osc_signed_in":
                set("signedIn", "yes")
                session?.cancel()
                set("cancelled", "yes")
                DispatchQueue.main.asyncAfter(deadline: .now() + 2) { [weak self] in
                    if self?.labels["completion"]?.text == "-" {
                        self?.set("completion", "not called")
                    }
                }
            default:
                break
            }
        } else if (message["id"] as? Int) == 1 {
            if message["ok"] != nil {
                set("reply", "ok")
            } else {
                let error = message["error"] as? [String: Any]
                set("reply", "error: \(error?["message"] as? String ?? "?")")
            }
        }
    }

    private func present(_ url: URL) {
        let session = ASWebAuthenticationSession(
            url: url, callbackURLScheme: "com.eyevinn.open-live-ingest.ios"
        ) { [weak self] callback, error in
            DispatchQueue.main.async { self?.completed(callback, error) }
        }
        session.prefersEphemeralWebBrowserSession = false
        session.presentationContextProvider = self
        self.session = session
        set("page", session.start() ? "started" : "not started")
    }

    private func completed(_ callback: URL?, _ error: Error?) {
        if let error = error as? ASWebAuthenticationSessionError {
            set("completion", error.code == .canceledLogin ? "canceledLogin" : "error \(error.code.rawValue)")
        } else if let error = error as NSError? {
            set("completion", "error \(error.domain) \(error.code)")
        } else {
            set("completion", callback == nil ? "no callback" : "callback")
        }
    }

    func presentationAnchor(for session: ASWebAuthenticationSession) -> ASPresentationAnchor {
        view.window ?? ASPresentationAnchor()
    }
}
