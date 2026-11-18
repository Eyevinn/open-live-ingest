//! The `oli` custom protocol: the only place the webview loads anything from.
//!
//! The page is three static files compiled into the binary. Any other path is a 404,
//! and every response, the 404 included, carries the Content-Security-Policy below
//! (docs/specs/desktop-app.md, section 9.4).

/// The custom scheme the page is served on.
pub const SCHEME: &str = "oli";

/// Only the app's own files, no inline script, no network access from the page.
pub const CONTENT_SECURITY_POLICY: &str = "default-src 'self'; script-src 'self'; \
     style-src 'self'; img-src 'self' data:; connect-src 'none'; object-src 'none'; \
     base-uri 'none'; frame-ancestors 'none'";

const INDEX_HTML: &[u8] = include_bytes!("../ui/index.html");
const APP_CSS: &[u8] = include_bytes!("../ui/app.css");
const APP_JS: &[u8] = include_bytes!("../ui/app.js");
const NOT_FOUND: &[u8] = b"Not found";

/// What the protocol handler answers for one path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Asset {
    pub status: u16,
    pub content_type: &'static str,
    pub body: &'static [u8],
}

impl Asset {
    /// The response headers, the same set on every response.
    pub fn headers(&self) -> [(&'static str, &'static str); 4] {
        [
            ("Content-Type", self.content_type),
            ("Content-Security-Policy", CONTENT_SECURITY_POLICY),
            ("X-Content-Type-Options", "nosniff"),
            ("Cache-Control", "no-store"),
        ]
    }
}

/// The file for a request path (the URI's path, without query or fragment).
pub fn serve(path: &str) -> Asset {
    let (content_type, body) = match path {
        "/" | "/index.html" => ("text/html; charset=utf-8", INDEX_HTML),
        "/app.css" => ("text/css; charset=utf-8", APP_CSS),
        "/app.js" => ("text/javascript; charset=utf-8", APP_JS),
        _ => {
            return Asset {
                status: 404,
                content_type: "text/plain; charset=utf-8",
                body: NOT_FOUND,
            }
        }
    };
    Asset {
        status: 200,
        content_type,
        body,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn csp(asset: &Asset) -> &'static str {
        asset
            .headers()
            .iter()
            .find(|(name, _)| *name == "Content-Security-Policy")
            .map(|(_, value)| *value)
            .expect("a Content-Security-Policy header")
    }

    #[test]
    fn the_three_files_are_served_with_their_types() {
        for (path, content_type, body) in [
            ("/", "text/html; charset=utf-8", INDEX_HTML),
            ("/index.html", "text/html; charset=utf-8", INDEX_HTML),
            ("/app.css", "text/css; charset=utf-8", APP_CSS),
            ("/app.js", "text/javascript; charset=utf-8", APP_JS),
        ] {
            let asset = serve(path);
            assert_eq!(asset.status, 200, "{path}");
            assert_eq!(asset.content_type, content_type, "{path}");
            assert_eq!(asset.body, body, "{path}");
            assert!(!asset.body.is_empty(), "{path}");
        }
    }

    #[test]
    fn any_other_path_is_a_404() {
        for path in [
            "",
            "/index.htm",
            "/APP.JS",
            "/app.js/",
            "//app.js",
            "/../Cargo.toml",
            "/ui/app.js",
            "/favicon.ico",
            "/%2e%2e/secret",
        ] {
            let asset = serve(path);
            assert_eq!(asset.status, 404, "{path:?}");
            assert_eq!(asset.body, NOT_FOUND, "{path:?}");
        }
    }

    #[test]
    fn every_response_carries_the_csp_including_the_404() {
        for path in ["/", "/app.css", "/app.js", "/nope"] {
            let asset = serve(path);
            assert_eq!(csp(&asset), CONTENT_SECURITY_POLICY, "{path}");
            assert!(asset
                .headers()
                .contains(&("X-Content-Type-Options", "nosniff")));
        }
    }

    /// The policy is the spec's, directive by directive.
    #[test]
    fn the_csp_allows_only_the_apps_own_files() {
        let directives: Vec<&str> = CONTENT_SECURITY_POLICY.split(';').map(str::trim).collect();
        assert_eq!(
            directives,
            [
                "default-src 'self'",
                "script-src 'self'",
                "style-src 'self'",
                "img-src 'self' data:",
                "connect-src 'none'",
                "object-src 'none'",
                "base-uri 'none'",
                "frame-ancestors 'none'",
            ]
        );
        assert!(!CONTENT_SECURITY_POLICY.contains("unsafe"));
    }

    /// The page loads nothing from anywhere else: no remote URL in the files, and the
    /// script and style it references are the ones served here.
    #[test]
    fn the_page_references_only_its_own_files() {
        for (name, body) in [
            ("index.html", INDEX_HTML),
            ("app.css", APP_CSS),
            ("app.js", APP_JS),
        ] {
            let text = std::str::from_utf8(body).expect("UTF-8");
            for remote in ["http:", "https:", "//cdn", "ws:", "wss:"] {
                assert!(!text.contains(remote), "{name} mentions {remote}");
            }
        }
        let html = std::str::from_utf8(INDEX_HTML).unwrap();
        assert!(html.contains("src=\"app.js\""));
        assert!(html.contains("href=\"app.css\""));
        assert!(
            !html.contains("<script>"),
            "inline script is refused by the CSP"
        );
        assert!(
            !html.contains("style=\""),
            "inline style is refused by the CSP"
        );
    }
}
