import AuthenticationServices
import UIKit

/// The Open Source Cloud sign-in page in `ASWebAuthenticationSession` (docs/specs/ios-app.md,
/// section 6). The core's loopback listener answers the redirect to `http://127.0.0.1:<port>/callback`
/// while the sheet is up (OL-64b's finding, #149); the session has no callback scheme of its own,
/// so it never completes by itself: `finish()` cancels it once the core says `osc_signed_in`.
/// `prefersEphemeralWebBrowserSession` is false, so an existing OSC browser login is reused.
@MainActor
final class SignInSheet: NSObject, ASWebAuthenticationPresentationContextProviding {
    private var session: ASWebAuthenticationSession?
    /// The sheet on screen; nil once finished by the app.
    private var current: UUID?

    /// Shows `url`. `closed` is called when the person closes the sheet before the sign-in
    /// finished (or it could not be shown), not after `finish()`.
    func present(_ url: URL, closed: @escaping () -> Void) {
        finish()
        let id = UUID()
        let session = ASWebAuthenticationSession(url: url, callbackURLScheme: nil) { [weak self] _, _ in
            DispatchQueue.main.async {
                guard let self, self.current == id else { return }
                self.current = nil
                self.session = nil
                closed()
            }
        }
        session.prefersEphemeralWebBrowserSession = false
        session.presentationContextProvider = self
        self.session = session
        current = id
        if !session.start() {
            current = nil
            self.session = nil
            closed()
        }
    }

    /// Closes the sheet: the sign-in finished, or failed.
    func finish() {
        current = nil
        session?.cancel()
        session = nil
    }

    func presentationAnchor(for session: ASWebAuthenticationSession) -> ASPresentationAnchor {
        let windows = UIApplication.shared.connectedScenes
            .compactMap { $0 as? UIWindowScene }
            .flatMap { $0.windows }
        return windows.first(where: { $0.isKeyWindow }) ?? windows.first ?? ASPresentationAnchor()
    }
}
