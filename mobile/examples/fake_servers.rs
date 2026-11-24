//! The fake Open Source Cloud and Open Live of `src/testing.rs` as a host process, for
//! the iOS app's UI tests on the simulator (docs/specs/ios-app.md, section 12; #141).
//! `ios/run-tests.sh` starts it and hands the addresses to the tests:
//!
//! ```text
//! cargo run -p open-live-ingest-mobile --features fake-servers --example fake_servers -- <urls.json>
//! ```
//!
//! It writes `{"osc": ..., "open_live": ..., "control": ...}` to `<urls.json>` (and
//! stdout) once all three answer, then serves until it is killed.
//!
//! - **osc**: the fake Open Source Cloud. A debug build of the app signs in there when
//!   pointed at it (`OLI_TEST_OSC_URL`); it takes `testing::ACCESS` as a personal access
//!   token and lists two instances, "Morning Studio" (the fake Open Live) and
//!   "Evening Studio" (nothing behind it), so one has to be picked.
//! - **open_live**: the fake Open Live behind the service token, publishing
//!   `127.0.0.1` as the cloud Strom and `testing::PUBLISHED_PORTS`, where the test run
//!   puts an SRT listener.
//! - **control**, for the test: `GET /state` (sources, created, deleted, the PATCHed
//!   statuses, productions), `POST /production/<status>` (one production, "Morning
//!   Show", holding every source in that status: `inactive`, `active`, ...) and
//!   `POST /reset` (no sources, no productions). `GET /health` answers once it is up.
//!
//! Plain HTTP on 127.0.0.1 only; no secret but the fakes' own test tokens.

use open_live_ingest_mobile::testing::{self, FakeOsc, OpenLiveState};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

fn main() {
    let out = std::env::args().nth(1);
    let state = Arc::new(Mutex::new(OpenLiveState::default()));
    let open_live = testing::fake_open_live_at(testing::SERVICE_TOKEN, state.clone(), "127.0.0.1");
    let osc = FakeOsc::start(&[
        ("Morning Studio", &open_live),
        ("Evening Studio", "https://evening.example.com"),
    ]);
    let control = testing::serve(Arc::new(move |req| control(req, &state)));
    let urls = json!({
        "osc": osc.endpoints().connect,
        "open_live": open_live,
        "control": control,
    });
    let text = urls.to_string();
    if let Some(path) = out {
        std::fs::write(&path, &text).unwrap_or_else(|err| panic!("writing {path}: {err}"));
    }
    println!("{text}");
    loop {
        std::thread::park();
    }
}

fn control(req: &testing::Request, state: &Mutex<OpenLiveState>) -> (u16, Value) {
    let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
    let path = req.path.split('?').next().unwrap_or_default();
    match (req.method.as_str(), path) {
        ("GET", "/health") => (200, json!({"ok": true})),
        ("GET", "/state") => (200, snapshot(&s)),
        ("POST", "/reset") => {
            *s = OpenLiveState::default();
            (200, snapshot(&s))
        }
        ("POST", _) if path.starts_with("/production/") => {
            let status = &path["/production/".len()..];
            let ids: Vec<String> = s
                .sources
                .iter()
                .filter_map(|x| x["id"].as_str().map(str::to_string))
                .collect();
            let ids: Vec<&str> = ids.iter().map(String::as_str).collect();
            s.productions = vec![testing::production("prod-1", "Morning Show", status, &ids)];
            (200, snapshot(&s))
        }
        _ => (404, json!({})),
    }
}

fn snapshot(s: &OpenLiveState) -> Value {
    json!({
        "sources": s.sources,
        "created": s.created,
        "deleted": s.deleted,
        "patched": s.patched,
        "productions": s.productions,
    })
}
