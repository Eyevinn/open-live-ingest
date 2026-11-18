//! Where the webview may go: only the app's own origin.
//!
//! The navigation handler refuses every other URL, and `window.open` is refused
//! outright, so the page can never show remote content. Links to the outside go
//! through the `open_url` command, which takes a name rather than a URL
//! (docs/specs/desktop-app.md, section 9.4).

/// The origin wry serves the [`crate::protocol::SCHEME`] custom protocol under. wry maps
/// a custom scheme to `http://<scheme>.localhost` on Windows (WebView2 does not load
/// custom schemes), and keeps it as is on macOS.
#[cfg(windows)]
pub const ORIGIN: &str = "http://oli.localhost";
#[cfg(not(windows))]
pub const ORIGIN: &str = "oli://localhost";

/// The page the window opens on.
pub fn start_url() -> String {
    format!("{ORIGIN}/")
}

/// Whether the webview may navigate to `url`.
pub fn is_allowed(url: &str) -> bool {
    is_within(ORIGIN, url)
}

/// `url` is `origin` itself, or a path, query or fragment under it. A prefix match on
/// the bare origin is not enough: `oli://localhost.example.com/` and
/// `oli://localhost@example.com/` start with the same characters and are elsewhere.
fn is_within(origin: &str, url: &str) -> bool {
    match url.strip_prefix(origin) {
        Some(rest) => rest.is_empty() || rest.starts_with(['/', '?', '#']),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_start_url_is_allowed() {
        assert!(is_allowed(&start_url()));
        assert!(start_url().starts_with(ORIGIN));
    }

    #[test]
    fn the_apps_own_pages_are_allowed() {
        for origin in ["oli://localhost", "http://oli.localhost"] {
            for url in ["", "/", "/index.html", "/app.js?v=1", "#section", "?x=1"] {
                let url = format!("{origin}{url}");
                assert!(is_within(origin, &url), "{url}");
            }
        }
    }

    #[test]
    fn everything_else_is_refused() {
        for origin in ["oli://localhost", "http://oli.localhost"] {
            for url in [
                "https://example.com/",
                "http://example.com/",
                "about:blank",
                "data:text/html,<p>x</p>",
                "javascript:alert(1)",
                "file:///etc/passwd",
                "blob:oli://localhost/1",
                "",
                "oli://localhost.example.com/",
                "oli://localhost@example.com/",
                "oli://localhost:8080/",
                "oli://localhostx/",
                "oli://localhost\\@example.com/",
                "http://oli.localhost.example.com/",
                "http://oli.localhost@example.com/",
                "http://oli.localhost:8080/",
                "https://oli.localhost/",
                "OLI://LOCALHOST/",
                " oli://localhost/",
            ] {
                assert!(!is_within(origin, url), "{origin}: {url}");
            }
        }
    }

    /// Each platform allows its own form of the origin and not the other's.
    #[test]
    fn only_this_platforms_origin_is_allowed() {
        let (mine, other) = if cfg!(windows) {
            ("http://oli.localhost/", "oli://localhost/")
        } else {
            ("oli://localhost/", "http://oli.localhost/")
        };
        assert!(is_allowed(mine));
        assert!(!is_allowed(other));
    }
}
