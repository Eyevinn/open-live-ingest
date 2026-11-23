//! Test support: a fake Open Source Cloud and a fake Open Live on localhost, a fake
//! browser for the sign-in, and scratch folders.
//!
//! The fake Open Source Cloud and the browser are the desktop app's (`app/src/testing.rs`),
//! which the phone's core is tested against as the spec asks (docs/specs/ios-app.md,
//! section 3); the fake Open Live adds sources and productions, as `tests/session.rs`
//! has them, behind the service token the fake Open Source Cloud hands out. The
//! servers are plain HTTP/1.1 on std threads, one request per connection.

use open_live_ingest::osc::Endpoints;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The code the fake browser brings back from the consent page.
pub const CODE: &str = "the-code";
/// An access token for workspace [`WORKSPACE`]: `{"tenantId":"venue-ws"}`. Also
/// accepted as a personal access token.
pub const ACCESS: &str = "h.eyJ0ZW5hbnRJZCI6InZlbnVlLXdzIn0.s";
pub const WORKSPACE: &str = "venue-ws";
pub const REFRESH: &str = "refresh-secret";
/// What a refresh returns in place of the refresh token it was given.
pub const RENEWED: &str = "refresh-renewed";
pub const REGISTERED_ID: &str = "registered-id";
pub const SERVICE_TOKEN: &str = "service-token";

/// What the fake browser does on the consent page: the query it comes back with.
pub const SIGNED_IN: &str = "code=the-code";
/// Never comes back.
pub const NEVER: &str = "";

// ---------------------------------------------------------------------------
// HTTP
// ---------------------------------------------------------------------------

/// One request as a handler sees it.
pub struct Request {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// The body as `application/x-www-form-urlencoded` pairs.
    pub fn form(&self) -> Vec<(String, String)> {
        query_pairs(&String::from_utf8_lossy(&self.body))
    }
}

pub type Handler = Arc<dyn Fn(&Request) -> (u16, Value) + Send + Sync>;

/// Serves `handler` on a free port of 127.0.0.1; returns `http://127.0.0.1:<port>`.
pub fn serve(handler: Handler) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let handler = handler.clone();
            std::thread::spawn(move || {
                let _ = answer(stream, &handler);
            });
        }
    });
    format!("http://{addr}")
}

fn answer(mut stream: TcpStream, handler: &Handler) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let path = parts.next().unwrap_or_default().to_string();
    let mut headers = Vec::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 || line == "\r\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_string(), value.trim().to_string()));
        }
    }
    let length = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0usize);
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body)?;
    let request = Request {
        method,
        path,
        headers,
        body,
    };
    let (status, value) = handler(&request);
    let body = value.to_string();
    write!(
        stream,
        "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )?;
    stream.flush()
}

/// `a=1&b=x%20y` as pairs, percent- and plus-decoded.
pub fn query_pairs(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|pair| {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            (decode(k), decode(v))
        })
        .collect()
}

fn decode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = bytes
            .get(i + 1..i + 3)
            .and_then(|h| std::str::from_utf8(h).ok())
            .and_then(|h| u8::from_str_radix(h, 16).ok());
        match (bytes[i], hex) {
            (b'+', _) => out.push(b' '),
            (b'%', Some(b)) => {
                out.push(b);
                i += 2;
            }
            (b, _) => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

// ---------------------------------------------------------------------------
// Open Source Cloud
// ---------------------------------------------------------------------------

#[derive(Default)]
struct OscState {
    instances: Vec<(String, String)>,
    refuse_registration: bool,
    refuse_refresh: bool,
    registered_names: Vec<String>,
    token_client_ids: Vec<String>,
}

/// A fake Open Source Cloud: client registration and the token endpoint under
/// `/api/connect`, the catalog, the service-token exchange, and the instance list of
/// workspace [`WORKSPACE`], which take only [`ACCESS`].
pub struct FakeOsc {
    base: String,
    state: Arc<Mutex<OscState>>,
}

impl FakeOsc {
    pub fn start(instances: &[(&str, &str)]) -> Self {
        let state = Arc::new(Mutex::new(OscState::default()));
        let shared = state.clone();
        let base = Arc::new(OnceLock::<String>::new());
        let base_in_handler = base.clone();
        let url = serve(Arc::new(move |req| {
            osc_answer(req, &shared, base_in_handler.get().unwrap())
        }));
        base.set(url.clone()).unwrap();
        let osc = FakeOsc { base: url, state };
        osc.set_instances(instances);
        osc
    }

    pub fn endpoints(&self) -> Endpoints {
        Endpoints {
            connect: self.base.clone(),
            catalog: format!("{}/mysubscriptions", self.base),
            token_exchange: format!("{}/servicetoken", self.base),
        }
    }

    pub fn set_instances(&self, instances: &[(&str, &str)]) {
        self.state.lock().unwrap().instances = instances
            .iter()
            .map(|(n, u)| (n.to_string(), u.to_string()))
            .collect();
    }
}

fn osc_answer(req: &Request, state: &Mutex<OscState>, base: &str) -> (u16, Value) {
    let mut state = state.lock().unwrap();
    let bearer = |name: &str, token: &str| req.header(name) == Some(&format!("Bearer {token}"));
    let path = req.path.split('?').next().unwrap_or_default();
    match (req.method.as_str(), path) {
        ("POST", "/api/connect/register") => {
            let body: Value = serde_json::from_slice(&req.body).unwrap_or_default();
            if let Some(name) = body["client_name"].as_str() {
                state.registered_names.push(name.to_string());
            }
            if state.refuse_registration {
                (500, json!({}))
            } else {
                (201, json!({"client_id": REGISTERED_ID}))
            }
        }
        ("POST", "/api/connect/token") => {
            let form = req.form();
            let get = |key: &str| form.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str());
            if let Some(id) = get("client_id") {
                state.token_client_ids.push(id.to_string());
            }
            let tokens = |refresh: &str| {
                json!({"access_token": ACCESS, "refresh_token": refresh,
                       "token_type": "Bearer", "expires_in": 3600})
            };
            match get("grant_type") {
                Some("authorization_code") if get("code") == Some(CODE) => (200, tokens(REFRESH)),
                Some("refresh_token")
                    if !state.refuse_refresh
                        && matches!(get("refresh_token"), Some(REFRESH | RENEWED)) =>
                {
                    (200, tokens(RENEWED))
                }
                _ => (400, json!({"error": "invalid_grant"})),
            }
        }
        ("GET", "/mysubscriptions") if bearer("x-pat-jwt", ACCESS) => (
            200,
            json!([
                {"serviceId": "another-service", "apiUrl": format!("{base}/nope")},
                {"serviceId": open_live_ingest::openlive::OPEN_LIVE_SERVICE_ID,
                 "apiUrl": format!("{base}/instances")},
            ]),
        ),
        ("POST", "/servicetoken") if bearer("x-pat-jwt", ACCESS) => {
            (200, json!({"token": SERVICE_TOKEN, "expiry": now() + 3600}))
        }
        ("GET", "/instances") if bearer("x-jwt", SERVICE_TOKEN) => (
            200,
            Value::Array(
                state
                    .instances
                    .iter()
                    .map(|(name, url)| json!({"name": name, "url": url, "StromAccessToken": "s"}))
                    .collect(),
            ),
        ),
        ("GET" | "POST", "/mysubscriptions" | "/servicetoken" | "/instances") => (401, json!({})),
        _ => (404, json!({})),
    }
}

/// A browser on the consent page: it reads the redirect and the state from the
/// authorize address it is opened at, and comes back to the app's loopback listener
/// with `outcome` ([`SIGNED_IN`], [`DECLINED`]), or never ([`NEVER`]).
pub fn browser(outcome: &'static str) -> crate::core::Opener {
    Arc::new(move |url: &str| {
        let query = url.split_once('?').map_or("", |(_, q)| q);
        let params = query_pairs(query);
        let get = |key: &str| {
            params
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.clone())
                .unwrap_or_default()
        };
        assert!(url.contains("/api/connect/authorize?"), "{url}");
        let (redirect, state) = (get("redirect_uri"), get("state"));
        if outcome.is_empty() {
            return Ok(());
        }
        std::thread::spawn(move || {
            let rest = redirect.trim_start_matches("http://");
            let (authority, path) = rest.split_once('/').unwrap();
            let mut stream = TcpStream::connect(authority).unwrap();
            write!(
                stream,
                "GET /{path}?{outcome}&state={state} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n\r\n"
            )
            .unwrap();
            let mut response = String::new();
            let _ = stream.read_to_string(&mut response);
        });
        Ok(())
    })
}

// ---------------------------------------------------------------------------
// Open Live
// ---------------------------------------------------------------------------

/// The ports a fake Open Live publishes; each new source gets the lowest one free.
pub const PUBLISHED_PORTS: [u16; 3] = [47250, 47251, 47252];

/// What a fake Open Live holds.
#[derive(Default)]
pub struct OpenLiveState {
    pub sources: Vec<Value>,
    /// Sources ever created.
    pub created: usize,
    /// Sources ever deleted.
    pub deleted: usize,
    /// The `status` of every PATCH, in order.
    pub patched: Vec<String>,
    /// What `GET /api/v1/productions` lists.
    pub productions: Vec<Value>,
}

/// A fake Open Live that wants `Bearer <bearer>`: server info publishing a cloud
/// host and [`PUBLISHED_PORTS`], sources, and productions.
pub fn fake_open_live(bearer: &'static str, state: Arc<Mutex<OpenLiveState>>) -> String {
    fake_open_live_at(bearer, state, "cloud.example.com")
}

/// [`fake_open_live`], publishing `strom_host` as the cloud Strom: `127.0.0.1` when an
/// SRT listener on [`PUBLISHED_PORTS`] stands in for it (the iOS UI tests).
pub fn fake_open_live_at(
    bearer: &'static str,
    state: Arc<Mutex<OpenLiveState>>,
    strom_host: &'static str,
) -> String {
    serve(Arc::new(move |req| {
        if req.header("authorization") != Some(&format!("Bearer {bearer}")) {
            return (401, json!({"error": "unauthorized"}));
        }
        let body: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
        let mut s = state.lock().unwrap();
        let path = req.path.as_str();
        match (
            req.method.as_str(),
            path,
            path.strip_prefix("/api/v1/sources/"),
        ) {
            ("GET", "/api/v1/server-info", _) => (
                200,
                json!({"stromHost": strom_host, "srtPorts": PUBLISHED_PORTS,
                       "srtPortState": "reserved"}),
            ),
            ("GET", "/api/v1/sources", _) => (200, Value::Array(s.sources.clone())),
            ("GET", "/api/v1/productions", _) => (200, Value::Array(s.productions.clone())),
            ("POST", "/api/v1/sources", _) => {
                let held: Vec<String> = s
                    .sources
                    .iter()
                    .map(|x| x["address"].as_str().unwrap_or_default().to_string())
                    .collect();
                let port = PUBLISHED_PORTS
                    .iter()
                    .find(|p| !held.iter().any(|a| a.starts_with(&format!("srt://:{p}"))))
                    .expect("a free published port");
                let address = body["address"].as_str().unwrap_or_default().replacen(
                    "srt://:0",
                    &format!("srt://:{port}"),
                    1,
                );
                s.created += 1;
                let source = json!({
                    "id": format!("src-{}", s.created),
                    "name": body["name"],
                    "address": address,
                    "streamType": body["streamType"],
                    "status": body["status"],
                    "latency": body["latency"],
                });
                s.sources.push(source.clone());
                (201, source)
            }
            ("PATCH", _, Some(id)) => {
                let Some(source) = s.sources.iter_mut().find(|x| x["id"] == id) else {
                    return (404, json!({"error": "Source not found"}));
                };
                for field in ["name", "address", "streamType", "status", "latency"] {
                    if !body[field].is_null() {
                        source[field] = body[field].clone();
                    }
                }
                let source = source.clone();
                s.patched
                    .push(body["status"].as_str().unwrap_or_default().to_string());
                (200, source)
            }
            ("DELETE", _, Some(id)) => {
                s.sources.retain(|x| x["id"] != id);
                s.deleted += 1;
                (200, json!({}))
            }
            _ => (404, json!({})),
        }
    }))
}

/// A production as Open Live lists it, holding `sources`.
pub fn production(id: &str, name: &str, status: &str, sources: &[&str]) -> Value {
    json!({
        "_id": id,
        "name": name,
        "status": status,
        "sources": sources
            .iter()
            .enumerate()
            .map(|(i, s)| json!({"sourceId": s, "mixerInput": format!("video_in_{}", i + 1)}))
            .collect::<Vec<Value>>(),
    })
}

// ---------------------------------------------------------------------------
// Scratch folders and logs
// ---------------------------------------------------------------------------

/// A folder of its own for one test, removed afterwards: the app's Application
/// Support folder, with the settings file in `open-live-ingest/` below it.
pub struct Scratch(PathBuf);

impl Scratch {
    pub fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("oli-mobile-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
    }

    pub fn settings(&self) -> PathBuf {
        self.0.join("open-live-ingest").join("gateway.toml")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A `MakeWriter` over a shared buffer, to read what was logged.
#[derive(Clone, Default)]
pub struct Captured(Arc<Mutex<Vec<u8>>>);

impl Captured {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

impl Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
    type Writer = Captured;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}
