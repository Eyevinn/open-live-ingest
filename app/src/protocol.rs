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

    /// The text of the handler the page registers for `event`, from the start of
    /// its line (the element it is on) up to the `});` that closes it.
    fn handler(js: &str, event: &str) -> String {
        let at = js
            .find(&format!("addEventListener(\"{event}\""))
            .unwrap_or_else(|| panic!("app.js handles {event}"));
        let start = js[..at].rfind('\n').map_or(0, |n| n + 1);
        let end = start + js[start..].find("\n  });").expect("the handler ends");
        js[start..end].to_string()
    }

    /// The body of `function name`, up to the `}` that closes it at the left margin.
    fn function(js: &str, name: &str) -> String {
        let start = js
            .find(&format!("function {name}("))
            .unwrap_or_else(|| panic!("app.js has {name}"));
        let end = start + js[start..].find("\n}\n").expect("the function ends");
        js[start..end].to_string()
    }

    /// Every element the script looks up by a literal id is in the page.
    #[test]
    fn every_id_the_script_uses_is_in_the_page() {
        let js = std::str::from_utf8(APP_JS).unwrap();
        let html = std::str::from_utf8(INDEX_HTML).unwrap();
        let used: Vec<&str> = js
            .match_indices("byId(\"")
            .map(|(at, open)| {
                let rest = &js[at + open.len()..];
                &rest[..rest.find('"').unwrap()]
            })
            .collect();
        assert!(used.contains(&"go-passphrase"), "{used:?}");
        for id in used {
            assert!(
                html.contains(&format!("id=\"{id}\"")),
                "no #{id} in index.html"
            );
        }
    }

    /// The library says `passphrase` before `uplink` (OL-66): each has its own line,
    /// so the relay line does not overwrite the passphrase one. Go live clears both.
    #[test]
    fn the_relay_and_passphrase_lines_do_not_overwrite_each_other() {
        let js = std::str::from_utf8(APP_JS).unwrap();
        let uplink = handler(js, "oli:uplink");
        let passphrase = handler(js, "oli:passphrase");
        assert!(uplink.contains("byId(\"go-uplink\")"), "{uplink}");
        assert!(!uplink.contains("go-passphrase"), "{uplink}");
        assert!(
            passphrase.contains("byId(\"go-passphrase\")"),
            "{passphrase}"
        );
        assert!(!passphrase.contains("go-uplink"), "{passphrase}");
        let go_live = function(js, "goLive");
        assert!(
            go_live.contains("\"go-uplink\", \"go-passphrase\""),
            "{go_live}"
        );
    }

    /// OL-70d: the pictures and levels arrive through the IPC like everything else;
    /// the page opens no connection of its own (the CSP's `connect-src 'none'` would
    /// refuse one, and a picture is a `data:` URL the core made).
    #[test]
    fn the_script_talks_only_through_the_ipc() {
        let js = std::str::from_utf8(APP_JS).unwrap();
        for api in [
            "fetch",
            "WebSocket",
            "XMLHttpRequest",
            "EventSource",
            "sendBeacon",
        ] {
            assert!(!js.contains(api), "app.js mentions {api}");
        }
        assert!(js.contains("window.ipc.postMessage("));
    }

    /// The preview runs while step 2 is shown: the page says so when step 1 is done or
    /// the session ends, and when the window is hidden or seen again; never when going
    /// live, which hands the cameras over itself (spec, sections 4.1 and 5.2).
    #[test]
    fn the_preview_follows_step_2_and_the_windows_visibility() {
        let js = std::str::from_utf8(APP_JS).unwrap();
        let visibility = handler(js, "visibilitychange");
        assert!(visibility.contains("syncPreview()"), "{visibility}");
        let wanted = function(js, "previewWanted");
        for condition in [
            "whereReady",
            "!live",
            "document.visibilityState === \"visible\"",
        ] {
            assert!(wanted.contains(condition), "{wanted}");
        }
        let sync = function(js, "syncPreview");
        assert!(sync.contains("call(\"preview\", { on })"), "{sync}");
        assert!(function(js, "renderWhere").contains("syncPreview()"));
        let set_live = function(js, "setLive");
        assert!(set_live.contains("syncPreview()"), "{set_live}");
        assert!(set_live.contains("previewOn = false"), "{set_live}");
    }

    /// A `preview` event whose picture did not change carries no image: the page keeps
    /// the last one. Levels and row notices are drawn by the row's `device`.
    #[test]
    fn the_page_keeps_the_last_picture_and_draws_by_device() {
        let js = std::str::from_utf8(APP_JS).unwrap();
        let preview = handler(js, "oli:preview");
        assert!(preview.contains("old.image"), "{preview}");
        assert!(preview.contains("previewBoxes(data.device)"), "{preview}");
        let level = handler(js, "oli:level");
        assert!(level.contains("level.device"), "{level}");
        let notice = handler(js, "oli:notice");
        assert!(notice.contains("rowNotice(notice)"), "{notice}");
        // The words come from the core's model.
        assert!(js.contains("previewText().no_microphone"));
        assert!(js.contains("previewText().camera_off"));
        assert!(js.contains("previewText().camera_off_states.includes(row.state)"));
    }

    /// Step 2's banner "Stop it" (the preview is off while another gateway is live)
    /// only stops that gateway: unlike Go live's own refusal, it must not go live. The
    /// preview's buttons take their actions from `PREVIEW_ACTIONS`, which has no
    /// `retry` (Go live's action) and its own `stop_it`.
    #[test]
    fn the_preview_banners_stop_it_does_not_go_live() {
        let js = std::str::from_utf8(APP_JS).unwrap();
        let render = function(js, "renderNotices");
        assert!(
            render.contains("byId(\"what-notices\"), previewNotices, PREVIEW_ACTIONS"),
            "{render}"
        );
        let start = js
            .find("const PREVIEW_ACTIONS = {")
            .expect("app.js has PREVIEW_ACTIONS");
        let end = start + js[start..].find("\n};\n").expect("PREVIEW_ACTIONS ends");
        let actions = &js[start..end];
        assert!(
            actions.contains("stop_it: { label: \"Stop it\", run: () => stopOtherForPreview() }"),
            "{actions}"
        );
        assert!(actions.contains("retry: null"), "{actions}");
        assert!(!actions.contains("goLive"), "{actions}");
        let stop = function(js, "stopOtherForPreview");
        assert!(stop.contains("call(\"stop_other\")"), "{stop}");
        assert!(stop.contains("syncPreview()"), "{stop}");
        assert!(!stop.contains("goLive"), "{stop}");
        assert!(!stop.contains("stopOther()"), "{stop}");
        for name in ["drawPicture", "drawRowNotices"] {
            let body = function(js, name);
            assert!(body.contains("PREVIEW_ACTIONS"), "{body}");
        }
    }

    /// A row's bars go empty when its picture stops, its camera goes off or it fails,
    /// and when no `level` event has come for a while: never frozen on a last level.
    #[test]
    fn the_level_bars_do_not_keep_a_stale_level() {
        let js = std::str::from_utf8(APP_JS).unwrap();
        let start = js
            .find("const NO_LEVEL_STATES")
            .expect("app.js has NO_LEVEL_STATES");
        let states = &js[start..start + js[start..].find('\n').unwrap()];
        for state in ["\"stopped\"", "\"camera_off\"", "\"failed\""] {
            assert!(states.contains(state), "{states}");
        }
        let preview = handler(js, "oli:preview");
        assert!(
            preview.contains("NO_LEVEL_STATES.has(data.state) && levels.delete(data.device)"),
            "{preview}"
        );
        let level = handler(js, "oli:level");
        assert!(level.contains("levelsAt = Date.now()"), "{level}");
        assert!(js.contains("Date.now() - levelsAt > LEVEL_STALE_MS"));
    }

    /// Advanced asks Open Live for the relays again whenever it opens, by its own
    /// summary or by the Advanced action of a failure (an unknown relay): the list
    /// from when the window opened may be gone.
    #[test]
    fn opening_advanced_reloads_the_relays() {
        let js = std::str::from_utf8(APP_JS).unwrap();
        let toggle = handler(js, "toggle");
        assert!(toggle.contains("byId(\"advanced\")"), "{toggle}");
        assert!(toggle.contains("loadRelays()"), "{toggle}");
        // Already open: setting `open` fires no toggle, so the action asks itself.
        let open = function(js, "openAdvanced");
        assert!(open.contains("loadRelays()"), "{open}");
        assert!(js.contains("advanced: { label: \"Advanced\", run: () => openAdvanced() }"));
    }
}
