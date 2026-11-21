//! Golden output: what `devices`, `status`, `up --test` and `down` print, byte for
//! byte, run as the real binary against a fake Strom and a fake Open Live on
//! localhost. No GStreamer, no devices, no network beyond loopback.
//!
//! These pin the operator-facing text, so the code behind it can move (the library
//! and CLI split of #71) without the terminal output changing. When a change to the
//! output is intended, change the expected text here in the same commit.

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const BIN: &str = env!("CARGO_BIN_EXE_open-live-ingest");

// ---------------------------------------------------------------------------
// A minimal HTTP/1.1 server: one thread per connection, one request per
// connection (`connection: close`), routed by a closure.
// ---------------------------------------------------------------------------

struct Request {
    method: String,
    path: String,
    body: Vec<u8>,
}

impl Request {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }
}

type Handler = Arc<dyn Fn(&Request) -> (u16, Value) + Send + Sync>;

/// Serves `handler` on a free loopback port until the test process exits. Returns
/// the base URL.
fn serve(handler: Handler) -> String {
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
    let mut length = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 || line == "\r\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                length = value.trim().parse().unwrap_or(0);
            }
        }
    }
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body)?;
    let (status, value) = handler(&Request { method, path, body });
    let body = if value.is_null() {
        String::new()
    } else {
        value.to_string()
    };
    write!(
        stream,
        "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )?;
    stream.flush()
}

// ---------------------------------------------------------------------------
// Fake Strom: device discovery and the flow API, flows kept in memory.
// ---------------------------------------------------------------------------

#[derive(Default)]
struct StromState {
    video: Vec<Value>,
    audio: Vec<Value>,
    flows: Vec<Value>,
    /// While set, every request waits: a gateway asking is still starting.
    stalled: bool,
    /// Requests that have waited because of `stalled`.
    stalled_requests: usize,
}

fn device(id: &str, name: &str, audio: bool) -> Value {
    json!({
        "id": id,
        "name": name,
        "device_class": if audio { "Audio/Source" } else { "Video/Source" },
        "category": if audio { "audiosource" } else { "videosource" },
        "provider": "fake",
        "properties": {},
        "first_seen_secs_ago": 0,
        "last_seen_secs_ago": 0,
    })
}

fn fake_strom(state: Arc<Mutex<StromState>>) -> String {
    serve(Arc::new(move |req: &Request| {
        if state.lock().unwrap().stalled {
            state.lock().unwrap().stalled_requests += 1;
            while state.lock().unwrap().stalled {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        let mut s = state.lock().unwrap();
        let path = req.path.as_str();
        let flow_route = path.strip_prefix("/api/flows/");
        match (req.method.as_str(), path, flow_route) {
            ("GET", "/api/discovery/devices?category=video_source", _) => {
                (200, Value::Array(s.video.clone()))
            }
            ("GET", "/api/discovery/devices?category=audio_source", _) => {
                (200, Value::Array(s.audio.clone()))
            }
            ("GET", "/api/flows", _) => (200, json!({ "flows": s.flows })),
            ("POST", "/api/flows", _) => {
                let flow = req.json();
                s.flows.retain(|f| f["id"] != flow["id"]);
                s.flows.push(flow.clone());
                (201, json!({ "flow": flow }))
            }
            (method, _, Some(rest)) => {
                let (id, action) = rest.split_once('/').unwrap_or((rest, ""));
                let Some(at) = s.flows.iter().position(|f| f["id"] == id) else {
                    return (404, json!({ "error": "no such flow" }));
                };
                match (method, action) {
                    ("GET", "") => (200, json!({ "flow": s.flows[at] })),
                    ("POST", "start") => {
                        s.flows[at]["running"] = json!(true);
                        (200, json!({ "flow": s.flows[at] }))
                    }
                    ("POST", "stop") => {
                        s.flows[at]["running"] = json!(false);
                        (200, json!({ "flow": s.flows[at] }))
                    }
                    ("DELETE", "") => {
                        s.flows.remove(at);
                        (204, Value::Null)
                    }
                    // Nothing receives the uplink: no SRT peer yet.
                    ("GET", "srt-stats") => (404, json!({ "error": "no stats" })),
                    _ => (404, Value::Null),
                }
            }
            // An older Strom without the version route; only ever logged.
            _ => (404, Value::Null),
        }
    }))
}

// ---------------------------------------------------------------------------
// Fake Open Live: server info with a published port list, and sources.
// ---------------------------------------------------------------------------

const PUBLISHED_PORTS: [u16; 3] = [47110, 47111, 47112];

#[derive(Default)]
struct OpenLiveState {
    sources: Vec<Value>,
    next_id: u32,
}

/// Open Live answers addresses with the passphrase masked.
fn masked(address: &str) -> String {
    match address.find("passphrase=") {
        Some(at) => {
            let start = at + "passphrase=".len();
            let end = address[start..]
                .find('&')
                .map(|i| start + i)
                .unwrap_or(address.len());
            format!("{}***{}", &address[..start], &address[end..])
        }
        None => address.to_string(),
    }
}

fn port_of(address: &str) -> Option<u16> {
    let rest = address.strip_prefix("srt://:")?;
    rest.split(['?', '&']).next()?.parse().ok()
}

/// Port 0 means "assign one": the lowest published port no other source holds.
fn assign(state: &OpenLiveState, address: &str, except: Option<&str>) -> String {
    if port_of(address) != Some(0) {
        return masked(address);
    }
    let held: Vec<u16> = state
        .sources
        .iter()
        .filter(|s| Some(s["id"].as_str().unwrap_or_default()) != except)
        .filter_map(|s| port_of(s["address"].as_str().unwrap_or_default()))
        .collect();
    let port = PUBLISHED_PORTS
        .iter()
        .find(|p| !held.contains(p))
        .expect("a free published port");
    masked(&address.replacen("srt://:0", &format!("srt://:{port}"), 1))
}

fn fake_open_live(state: Arc<Mutex<OpenLiveState>>) -> String {
    serve(Arc::new(move |req: &Request| {
        let mut s = state.lock().unwrap();
        let id = req
            .path
            .strip_prefix("/api/v1/sources/")
            .map(str::to_string);
        match (req.method.as_str(), req.path.as_str(), id) {
            ("GET", "/api/v1/server-info", _) => (
                200,
                json!({
                    "stromHost": "cloud.example.com",
                    "srtPorts": PUBLISHED_PORTS,
                    "srtPortState": "reserved",
                }),
            ),
            ("GET", "/api/v1/sources", _) => (200, Value::Array(s.sources.clone())),
            ("POST", "/api/v1/sources", _) => {
                let payload = req.json();
                s.next_id += 1;
                let address = assign(&s, payload["address"].as_str().unwrap_or_default(), None);
                let source = json!({
                    "id": format!("src-{}", s.next_id),
                    "name": payload["name"],
                    "address": address,
                    "streamType": payload["streamType"],
                    "status": payload["status"],
                    "latency": payload["latency"],
                });
                s.sources.push(source.clone());
                (201, source)
            }
            (method, _, Some(id)) => {
                let Some(at) = s.sources.iter().position(|x| x["id"] == id.as_str()) else {
                    return (404, json!({ "error": "no such source" }));
                };
                match method {
                    "PATCH" => {
                        let payload = req.json();
                        let address = assign(
                            &s,
                            payload["address"].as_str().unwrap_or_default(),
                            Some(&id),
                        );
                        let source = &mut s.sources[at];
                        source["address"] = json!(address);
                        source["status"] = payload["status"].clone();
                        source["latency"] = payload["latency"].clone();
                        (200, source.clone())
                    }
                    "DELETE" => {
                        s.sources.remove(at);
                        (204, Value::Null)
                    }
                    _ => (404, Value::Null),
                }
            }
            _ => (404, Value::Null),
        }
    }))
}

// ---------------------------------------------------------------------------
// The binary, run with a scratch settings file and user directory.
// ---------------------------------------------------------------------------

/// A made-up SRT key, TOML-quoted, so the tests show it never reaches the output.
const SRT_KEY_TOML: &str = r#""golden-key-0123""#;

/// A scratch directory holding the settings file and serving as the user directory
/// (pidfiles, OSC login), removed again when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str, strom: &str, open_live: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("oli-golden-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("gateway.toml"),
            format!(
                r#"[gateway]
id = "gw-golden"
name = "Venue"

[strom]
url = "{strom}"
manage = false

[open_live]
url = "{open_live}"
auth_mode = "direct"
register = true

[uplink]
mode = "caller"
passphrase = {srt_key}
"#,
                srt_key = SRT_KEY_TOML,
            ),
        )
        .unwrap();
        Scratch(dir)
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(BIN);
        cmd.arg("--config")
            .arg(self.0.join("gateway.toml"))
            .args(args)
            // Logs go to stdout too; they carry timestamps, and they are not what
            // these tests pin.
            .env("RUST_LOG", "off")
            .env("NO_COLOR", "1")
            .env("HOME", &self.0)
            .env("XDG_CONFIG_HOME", &self.0)
            .env("APPDATA", &self.0)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for var in [
            "OLI_CONFIG",
            "OLI_LOG_LEVEL",
            "OLI_OPEN_LIVE_URL",
            "OLI_OPEN_LIVE_API_KEY",
            "OLI_OPEN_LIVE_AUTH_MODE",
            "OLI_OPEN_LIVE_GATEWAY_ID",
            "OLI_OPEN_LIVE_GATEWAY_TOKEN",
            "OLI_STROM_URL",
            "OLI_STROM_API_KEY",
            "OSC_ACCESS_TOKEN",
        ] {
            cmd.env_remove(var);
        }
        cmd
    }

    /// Runs a command that exits by itself.
    fn run(&self, args: &[&str]) -> Output {
        let out = self.command(args).output().expect("running the binary");
        assert!(
            out.status.success(),
            "{args:?} failed: {}\nstdout:\n{}\nstderr:\n{}",
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        out
    }

    fn stdout(&self, args: &[&str]) -> String {
        String::from_utf8(self.run(args).stdout).expect("UTF-8 output")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A running `up`, its stdout read line by line on a thread.
struct Running {
    child: Child,
    lines: mpsc::Receiver<String>,
    seen: String,
}

impl Running {
    fn start(scratch: &Scratch, args: &[&str]) -> Self {
        let mut child = scratch.command(args).spawn().expect("starting up");
        let stdout = child.stdout.take().unwrap();
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        if tx.send(line).is_err() {
                            break;
                        }
                    }
                }
            }
        });
        Running {
            child,
            lines,
            seen: String::new(),
        }
    }

    /// Reads until a line containing `needle`, and returns everything read so far.
    fn until(&mut self, needle: &str) -> &str {
        while !self.seen.lines().any(|l| l.contains(needle)) {
            match self.lines.recv_timeout(Duration::from_secs(60)) {
                Ok(line) => self.seen.push_str(&line),
                Err(_) => {
                    let _ = self.child.kill();
                    let mut stderr = String::new();
                    if let Some(mut e) = self.child.stderr.take() {
                        let _ = e.read_to_string(&mut stderr);
                    }
                    panic!(
                        "no line containing {needle:?}; stdout so far:\n{}\nstderr:\n{stderr}",
                        self.seen
                    );
                }
            }
        }
        &self.seen
    }

    /// Ends it the hard way, as a power cut or `kill -9` would: no teardown.
    fn kill(mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    /// Ctrl-C or SIGTERM, then everything it printed, and whether it exited cleanly.
    #[cfg(unix)]
    fn terminate(mut self) -> (String, bool) {
        let pid = self.child.id() as libc::pid_t;
        // SAFETY: a plain kill(2) of our own child, which has not been waited for.
        assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
        let exited = self.child.wait().expect("waiting for up");
        while let Ok(line) = self.lines.recv_timeout(Duration::from_secs(10)) {
            self.seen.push_str(&line);
        }
        (self.seen, exited.success())
    }
}

/// The settings name both fakes by URL, and the port changes every run.
fn normalise(text: &str, strom: &str, open_live: &str) -> String {
    text.replace(strom, "http://STROM")
        .replace(open_live, "http://OPEN-LIVE")
}

/// On failure, both texts in full: a diff of whitespace in a table is otherwise hard
/// to read from `assert_eq!`'s escaped strings.
#[track_caller]
fn assert_golden(actual: &str, expected: &str) {
    if actual != expected {
        panic!("output changed\n--- expected ---\n{expected}--- actual ---\n{actual}--- end ---");
    }
}

// ---------------------------------------------------------------------------
// The goldens.
// ---------------------------------------------------------------------------

// The note column of a device with no note is padding: the line ends in spaces.
const DEVICES: &str = concat!(
    "DEVICE                         ID                       NOTE\n",
    "FaceTime HD Camera             dev-cam                  \n",
    "\
OBS Virtual Camera             dev-obs                  virtual, skipped unless --all

`up` starts all of these except the virtual ones; `up --devices \"FaceTime\"` picks by name or id.

AUDIO SOURCE                   ID
USB Microphone                 dev-mic

No audio is sent unless paired: `up --audio \"FaceTime=USB Microphone\"` sends one with a camera.
"
);

const DEVICES_JSON: &str = r#"[
  {
    "name": "FaceTime HD Camera",
    "id": "dev-cam",
    "kind": "video",
    "virtual": false
  },
  {
    "name": "OBS Virtual Camera",
    "id": "dev-obs",
    "kind": "video",
    "virtual": true
  },
  {
    "name": "USB Microphone",
    "id": "dev-mic",
    "kind": "audio",
    "virtual": false
  }
]
"#;

#[test]
fn devices_and_devices_json_print_as_before() {
    let strom_state = Arc::new(Mutex::new(StromState {
        video: vec![
            device("dev-obs", "OBS Virtual Camera", false),
            device("dev-cam", "FaceTime HD Camera", false),
        ],
        audio: vec![device("dev-mic", "USB Microphone", true)],
        ..Default::default()
    }));
    let strom = fake_strom(strom_state);
    let open_live = fake_open_live(Arc::default());
    let scratch = Scratch::new("devices", &strom, &open_live);

    assert_golden(&scratch.stdout(&["devices"]), DEVICES);
    assert_golden(&scratch.stdout(&["devices", "--json"]), DEVICES_JSON);
}

const UP_START: &str = "\
Bringing up 1 input(s) to cloud.example.com, SRT ports 47110-47112 from Open Live, about 6 Mbps in total.

INPUT                                PORT   ADDRESSES
Venue — Test pattern                 47110  sends to:   srt://cloud.example.com:47110?mode=caller&latency=200&passphrase=***
                                            registered: srt://:47110?mode=listener&passphrase=***

Assign these in Studio and activate the production; they read as inactive until
something is receiving them. Ctrl-C stops and removes them.

  Venue — Test pattern — waiting for a receiver
";

const UP_RESTART: &str = "\
Bringing up 1 input(s) to cloud.example.com, SRT ports 47110-47112 from Open Live, about 6 Mbps in total.
  removed Strom flow \"Venue — Test pattern\", left over from an earlier run

INPUT                                PORT   ADDRESSES
Venue — Test pattern                 47110  sends to:   srt://cloud.example.com:47110?mode=caller&latency=200&passphrase=***
                                            registered: srt://:47110?mode=listener&passphrase=***

Assign these in Studio and activate the production; they read as inactive until
something is receiving them. Ctrl-C stops and removes them.

  Venue — Test pattern — waiting for a receiver
";

const STATUS: &str = "\
No gateway process running.
Uplink to cloud.example.com, SRT ports 47110-47112 from Open Live.

INPUT                                FLOW      UPLINK                     OPEN LIVE
Venue — Camera 9                     -         -                          inactive (warning: srt-listener-unsupported)
Venue — Test pattern                 running   no receiver                inactive
";

const STATUS_JSON: &str = r#"{
  "process": {
    "running": false,
    "pid": null
  },
  "uplink": {
    "host": "cloud.example.com",
    "ports": "47110-47112",
    "port_source": "Open Live"
  },
  "uplink_error": null,
  "strom": {
    "url": "http://STROM",
    "reachable": true,
    "error": null
  },
  "open_live": {
    "url": "http://OPEN-LIVE",
    "reachable": true,
    "error": null
  },
  "inputs": [
    {
      "name": "Venue — Camera 9",
      "flow": null,
      "uplink": null,
      "source": {
        "id": "src-old",
        "status": "inactive",
        "warning": "srt-listener-unsupported"
      }
    },
    {
      "name": "Venue — Test pattern",
      "flow": {
        "id": "FLOW-ID",
        "running": true
      },
      "uplink": null,
      "source": {
        "id": "src-1",
        "status": "inactive",
        "warning": null
      }
    }
  ]
}
"#;

const DOWN: &str = concat!(
    "  Venue — Test pattern — flow stopped and removed\n",
    "  Venue — Camera 9 — source removed from Open Live\n",
    "  Venue — Test pattern — source removed from Open Live\n",
);

const DOWN_AGAIN: &str = "Nothing of ours left to remove.\n";

/// Another source of this gateway, left by an earlier run, that Open Live warns
/// about: `status` shows it, and `down` sweeps it by the name prefix.
fn leftover_source() -> Value {
    json!({
        "id": "src-old",
        "name": "Venue — Camera 9",
        "address": "srt://:47199?mode=listener",
        "streamType": "srt",
        "status": "inactive",
        "latency": 200,
        "connect": { "warning": "srt-listener-unsupported" },
    })
}

/// `up --test` killed hard leaves its flow and source; `status` reports them, a
/// second `up` clears the leftover flow and keeps the source's port, and `down`
/// removes everything.
#[test]
fn up_status_and_down_print_as_before() {
    let strom_state = Arc::new(Mutex::new(StromState::default()));
    let strom = fake_strom(strom_state.clone());
    let ol_state = Arc::new(Mutex::new(OpenLiveState {
        sources: vec![leftover_source()],
        next_id: 0,
    }));
    let open_live = fake_open_live(ol_state.clone());
    let scratch = Scratch::new("lifecycle", &strom, &open_live);

    let mut up = Running::start(&scratch, &["up", "--test"]);
    assert_golden(up.until("waiting for a receiver"), UP_START);
    up.kill();
    assert_eq!(strom_state.lock().unwrap().flows.len(), 1);
    assert_eq!(ol_state.lock().unwrap().sources.len(), 2);

    assert_golden(&scratch.stdout(&["status"]), STATUS);
    let flow_id = strom_state.lock().unwrap().flows[0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let json = normalise(&scratch.stdout(&["status", "--json"]), &strom, &open_live)
        .replace(&flow_id, "FLOW-ID");
    assert_golden(&json, STATUS_JSON);

    let mut up = Running::start(&scratch, &["up", "--test"]);
    assert_golden(up.until("waiting for a receiver"), UP_RESTART);
    up.kill();

    assert_golden(&scratch.stdout(&["down"]), DOWN);
    assert!(strom_state.lock().unwrap().flows.is_empty());
    assert!(ol_state.lock().unwrap().sources.is_empty());
    assert_golden(&scratch.stdout(&["down"]), DOWN_AGAIN);
}

#[cfg(unix)]
const UP_THEN_STOP: &str = "\
Bringing up 1 input(s) to cloud.example.com, SRT ports 47110-47112 from Open Live, about 6 Mbps in total.

INPUT                                PORT   ADDRESSES
Venue — Test pattern                 47110  sends to:   srt://cloud.example.com:47110?mode=caller&latency=200&passphrase=***
                                            registered: srt://:47110?mode=listener&passphrase=***

Assign these in Studio and activate the production; they read as inactive until
something is receiving them. Ctrl-C stops and removes them.

  Venue — Test pattern — waiting for a receiver

Stopping.
  Venue — Test pattern — flow stopped and removed
  Venue — Test pattern — source removed from Open Live
  Venue — Camera 9 — source removed from Open Live
";

/// SIGTERM stops `up` the way Ctrl-C does: it removes its flow and source, sweeps
/// the gateway's other sources, and exits cleanly. Windows has no SIGTERM to send.
#[cfg(unix)]
#[test]
fn up_stopped_by_a_signal_tears_down_as_before() {
    let strom_state = Arc::new(Mutex::new(StromState::default()));
    let strom = fake_strom(strom_state.clone());
    let ol_state = Arc::new(Mutex::new(OpenLiveState {
        sources: vec![leftover_source()],
        next_id: 0,
    }));
    let open_live = fake_open_live(ol_state.clone());
    let scratch = Scratch::new("stop", &strom, &open_live);

    let mut up = Running::start(&scratch, &["up", "--test"]);
    up.until("waiting for a receiver");
    // The signal handlers are installed as the loop waits for its next tick.
    std::thread::sleep(Duration::from_secs(1));
    let (out, clean) = up.terminate();
    assert_golden(&out, UP_THEN_STOP);
    assert!(clean, "up did not exit cleanly");
    assert!(strom_state.lock().unwrap().flows.is_empty());
    assert!(ol_state.lock().unwrap().sources.is_empty());
}

/// A second `up` on the same settings while the first is live refuses, and touches
/// nothing: no flow, no source, no Strom. The one change to `up`'s output (#95).
#[test]
fn a_second_up_refuses_while_one_is_live() {
    let strom_state = Arc::new(Mutex::new(StromState::default()));
    let strom = fake_strom(strom_state.clone());
    let ol_state = Arc::new(Mutex::new(OpenLiveState::default()));
    let open_live = fake_open_live(ol_state.clone());
    let scratch = Scratch::new("refusal", &strom, &open_live);

    let mut first = Running::start(&scratch, &["up", "--test"]);
    first.until("waiting for a receiver");
    let pid = first.child.id();

    // Bounded: a second `up` that does not refuse would run until stopped.
    let mut second = scratch.command(&["up", "--test"]).spawn().unwrap();
    let started = std::time::Instant::now();
    while second.try_wait().unwrap().is_none() {
        if started.elapsed() > Duration::from_secs(60) {
            let _ = second.kill();
            first.kill();
            panic!("the second up did not refuse");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let second = second.wait_with_output().unwrap();
    let stdout = String::from_utf8(second.stdout).unwrap();
    let stderr = String::from_utf8(second.stderr).unwrap();
    first.kill();

    assert!(!second.status.success(), "the second up started");
    assert_golden(&stdout, "");
    assert_golden(
        &stderr,
        &format!("Error: already running (pid {pid}); run 'open-live-ingest down' first\n"),
    );
    assert_eq!(strom_state.lock().unwrap().flows.len(), 1);
    assert_eq!(ol_state.lock().unwrap().sources.len(), 1);
}

/// #95 C1: an `up` that is still starting (here, held at its first Strom request) is
/// already a running gateway. A second `up` refuses it, as the app's Go live does,
/// whose Strom reclaim therefore never takes a starting gateway's Strom for one left
/// behind.
#[test]
fn a_second_up_refuses_while_one_is_still_starting() {
    let strom_state = Arc::new(Mutex::new(StromState {
        stalled: true,
        ..StromState::default()
    }));
    let strom = fake_strom(strom_state.clone());
    let ol_state = Arc::new(Mutex::new(OpenLiveState::default()));
    let open_live = fake_open_live(ol_state.clone());
    let scratch = Scratch::new("refusal-starting", &strom, &open_live);

    let mut first = Running::start(&scratch, &["up", "--test"]);
    let pid = first.child.id();
    let started = std::time::Instant::now();
    while strom_state.lock().unwrap().stalled_requests == 0 {
        if started.elapsed() > Duration::from_secs(60) {
            first.kill();
            panic!("the first up never asked Strom");
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    let second = scratch.command(&["up", "--test"]).output().unwrap();
    assert!(!second.status.success(), "the second up started");
    assert_golden(&String::from_utf8(second.stdout).unwrap(), "");
    assert_golden(
        &String::from_utf8(second.stderr).unwrap(),
        &format!("Error: already running (pid {pid}); run 'open-live-ingest down' first\n"),
    );

    // The first goes on to go live, alone.
    strom_state.lock().unwrap().stalled = false;
    first.until("waiting for a receiver");
    first.kill();
    assert_eq!(strom_state.lock().unwrap().flows.len(), 1);
    assert_eq!(ol_state.lock().unwrap().sources.len(), 1);
}

// ---------------------------------------------------------------------------
// The saved selection: `[[input]]` entries say what `up` sends.
// ---------------------------------------------------------------------------

impl Scratch {
    /// Adds `text` to the end of the settings file.
    fn append_settings(&self, text: &str) {
        let path = self.0.join("gateway.toml");
        let mut settings = std::fs::read_to_string(&path).unwrap();
        settings.push_str(text);
        std::fs::write(path, settings).unwrap();
    }
}

fn selection_strom() -> Arc<Mutex<StromState>> {
    Arc::new(Mutex::new(StromState {
        video: vec![
            device("dev-obs", "OBS Virtual Camera", false),
            device("dev-cam", "FaceTime HD Camera", false),
        ],
        audio: vec![device("dev-mic", "USB Microphone", true)],
        ..Default::default()
    }))
}

/// The flows' names and whether each carries an audio device, sorted by name.
fn flows_of(state: &Arc<Mutex<StromState>>) -> Vec<(String, bool)> {
    let mut flows: Vec<(String, bool)> = state
        .lock()
        .unwrap()
        .flows
        .iter()
        .map(|f| {
            let audio = f.to_string().contains("dev-mic");
            (f["name"].as_str().unwrap_or_default().to_string(), audio)
        })
        .collect();
    flows.sort();
    flows
}

fn source_names(state: &Arc<Mutex<OpenLiveState>>) -> Vec<String> {
    let mut names: Vec<String> = state
        .lock()
        .unwrap()
        .sources
        .iter()
        .map(|s| s["name"].as_str().unwrap_or_default().to_string())
        .collect();
    names.sort();
    names
}

const UP_EVERY_DEVICE: &str = "\
Bringing up 1 input(s) to cloud.example.com, SRT ports 47110-47112 from Open Live, about 6 Mbps in total.

INPUT                                PORT   ADDRESSES
Venue — FaceTime HD Camera           47110  sends to:   srt://cloud.example.com:47110?mode=caller&latency=200&passphrase=***
                                            registered: srt://:47110?mode=listener&passphrase=***

Assign these in Studio and activate the production; they read as inactive until
something is receiving them. Ctrl-C stops and removes them.

  Venue — FaceTime HD Camera — waiting for a receiver
";

const UP_SAVED_SELECTION: &str = "\
Bringing up 2 input(s) to cloud.example.com, SRT ports 47110-47112 from Open Live, about 12 Mbps in total.

INPUT                                PORT   ADDRESSES
Venue — Test pattern                 47110  sends to:   srt://cloud.example.com:47110?mode=caller&latency=200&passphrase=***
                                            registered: srt://:47110?mode=listener&passphrase=***
Venue — Stage left                   47111  sends to:   srt://cloud.example.com:47111?mode=caller&latency=200&passphrase=***
                                            registered: srt://:47111?mode=listener&passphrase=***

Assign these in Studio and activate the production; they read as inactive until
something is receiving them. Ctrl-C stops and removes them.

  Venue — Test pattern — waiting for a receiver
  Venue — Stage left — waiting for a receiver
";

/// Without `[[input]]`, `up` sends every non-virtual device as before. With it, `up`
/// sends exactly the entries (a virtual device and the test pattern here, not the
/// FaceTime camera), with the paired audio and the label, and skips one that is not
/// connected. `--devices` overrides the entries for a run.
#[test]
fn up_sends_exactly_the_saved_inputs_and_flags_override_them() {
    let strom_state = selection_strom();
    let strom = fake_strom(strom_state.clone());
    let ol_state = Arc::new(Mutex::new(OpenLiveState::default()));
    let open_live = fake_open_live(ol_state.clone());
    let scratch = Scratch::new("selection", &strom, &open_live);

    // No [[input]]: unchanged.
    let mut up = Running::start(&scratch, &["up"]);
    assert_golden(up.until("waiting for a receiver"), UP_EVERY_DEVICE);
    up.kill();
    assert_eq!(
        flows_of(&strom_state),
        [("Venue — FaceTime HD Camera".to_string(), false)]
    );
    scratch.run(&["down"]);
    assert!(strom_state.lock().unwrap().flows.is_empty());

    scratch.append_settings(
        r#"
[[input]]
device = "dev-obs"
audio = "dev-mic"
label = "Stage left"

[[input]]
device = "test"

[[input]]
device = "dev-unplugged"
"#,
    );
    let mut up = Running::start(&scratch, &["up"]);
    let out = up.until("Stage left — waiting for a receiver").to_string();
    up.kill();
    assert_golden(&out, UP_SAVED_SELECTION);
    assert_eq!(
        flows_of(&strom_state),
        [
            ("Venue — Stage left".to_string(), true),
            ("Venue — Test pattern".to_string(), false),
        ]
    );
    assert_eq!(
        source_names(&ol_state),
        ["Venue — Stage left", "Venue — Test pattern"]
    );
    scratch.run(&["down"]);
    assert!(strom_state.lock().unwrap().flows.is_empty());
    assert!(ol_state.lock().unwrap().sources.is_empty());

    // A flag picks for this run, here a device without an entry.
    let mut up = Running::start(&scratch, &["up", "--devices", "FaceTime"]);
    assert_golden(up.until("waiting for a receiver"), UP_EVERY_DEVICE);
    up.kill();
    assert_eq!(
        flows_of(&strom_state),
        [("Venue — FaceTime HD Camera".to_string(), false)]
    );
    scratch.run(&["down"]);

    // Picked by flags (`--all`, as it is virtual), a device with an entry keeps the
    // entry's audio and label.
    let mut up = Running::start(&scratch, &["up", "--all", "--devices", "dev-obs"]);
    up.until("Stage left — waiting for a receiver");
    up.kill();
    assert_eq!(
        flows_of(&strom_state),
        [("Venue — Stage left".to_string(), true)]
    );
    scratch.run(&["down"]);
}

/// `check` refuses one audio source paired with two cameras in `[[input]]`.
#[test]
fn check_refuses_one_audio_source_paired_twice() {
    let strom = fake_strom(selection_strom());
    let open_live = fake_open_live(Arc::default());
    let scratch = Scratch::new("pairing", &strom, &open_live);
    scratch.append_settings(
        r#"
[[input]]
device = "dev-cam"
audio = "dev-mic"

[[input]]
device = "dev-obs"
audio = "dev-mic"
"#,
    );
    let out = scratch.command(&["check"]).output().unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains(r#"audio source "dev-mic" is paired with both "dev-cam" and "dev-obs""#),
        "{stderr}"
    );
}
