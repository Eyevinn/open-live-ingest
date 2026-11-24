//! The exported symbols called the way Swift calls them (docs/specs/ios-app.md,
//! section 8.1): through raw pointers and a C callback, with null and non-UTF-8
//! arguments, a panic inside a command, and a free while live that has to return
//! within its bound even when Open Live stops answering.

use open_live_ingest_mobile::{oli_core_free, oli_core_new, oli_core_send, oli_string_free};
use serde_json::{json, Value};
use std::ffi::{c_char, c_void, CStr, CString};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// What Swift's side of one core has been handed, by the callback.
#[derive(Default)]
struct Inbox {
    messages: Mutex<Vec<Value>>,
    arrived: Condvar,
}

impl Inbox {
    /// The first message `wanted` picks, within `within`.
    fn wait(&self, what: &str, within: Duration, wanted: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + within;
        let mut messages = self.messages.lock().unwrap();
        loop {
            if let Some(found) = messages.iter().find(|m| wanted(m)) {
                return found.clone();
            }
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(
                !left.is_zero(),
                "no {what} within {within:?}: {messages:#?}"
            );
            messages = self.arrived.wait_timeout(messages, left).unwrap().0;
        }
    }

    fn len(&self) -> usize {
        self.messages.lock().unwrap().len()
    }
}

/// The callback, as Swift's would be: copies the string, valid only during the call.
extern "C" fn deliver(ctx: *mut c_void, json: *const c_char) {
    // SAFETY: ctx is the Inbox each test keeps alive past oli_core_free, and json is
    // a NUL-terminated string valid during this call, by the ABI's contract.
    let (inbox, json) = unsafe { (&*(ctx as *const Inbox), CStr::from_ptr(json)) };
    let message = serde_json::from_str(json.to_str().unwrap()).unwrap();
    inbox.messages.lock().unwrap().push(message);
    inbox.arrived.notify_all();
}

struct Phone {
    dir: PathBuf,
    inbox: Box<Inbox>,
}

impl Phone {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("oli-abi-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Phone {
            dir,
            inbox: Box::default(),
        }
    }

    fn settings(&self) -> PathBuf {
        self.dir.join("open-live-ingest").join("gateway.toml")
    }

    fn ctx(&self) -> *mut c_void {
        &*self.inbox as *const Inbox as *mut c_void
    }

    fn start(&self) -> *mut open_live_ingest_mobile::OliCore {
        let dir = c(self.dir.to_str().unwrap());
        let logs = c(self.dir.join("Logs").to_str().unwrap());
        // SAFETY: valid strings for the call; the inbox outlives the core.
        let core = unsafe { oli_core_new(dir.as_ptr(), logs.as_ptr(), Some(deliver), self.ctx()) };
        assert!(!core.is_null());
        core
    }
}

impl Drop for Phone {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn c(s: &str) -> CString {
    CString::new(s).unwrap()
}

fn send(core: *mut open_live_ingest_mobile::OliCore, message: Value) {
    let message = c(&message.to_string());
    // SAFETY: a live core and a valid string for the call.
    unsafe { oli_core_send(core, message.as_ptr()) };
}

#[test]
fn a_core_is_not_made_from_null_or_non_utf8_arguments() {
    let phone = Phone::new("new-args");
    let dir = c(phone.dir.to_str().unwrap());
    let not_utf8 = CString::new(vec![0x66, 0xff, 0xfe]).unwrap();
    // SAFETY: null, or valid strings for the call.
    unsafe {
        assert!(oli_core_new(std::ptr::null(), dir.as_ptr(), Some(deliver), phone.ctx()).is_null());
        assert!(oli_core_new(dir.as_ptr(), std::ptr::null(), Some(deliver), phone.ctx()).is_null());
        assert!(
            oli_core_new(not_utf8.as_ptr(), dir.as_ptr(), Some(deliver), phone.ctx()).is_null()
        );
        assert!(oli_core_new(dir.as_ptr(), dir.as_ptr(), None, phone.ctx()).is_null());
        // Null is ignored by the others.
        oli_core_send(std::ptr::null_mut(), dir.as_ptr());
        oli_core_free(std::ptr::null_mut());
        oli_string_free(std::ptr::null_mut());
    }
    assert_eq!(phone.inbox.len(), 0);
}

#[test]
fn null_and_non_utf8_messages_get_an_error_reply_without_an_id() {
    let phone = Phone::new("send-args");
    let core = phone.start();
    let not_utf8 = CString::new(vec![b'{', 0xc3, 0x28, b'}']).unwrap();
    // SAFETY: a live core; null, or a valid string, for the call.
    unsafe {
        oli_core_send(core, std::ptr::null());
        oli_core_send(core, not_utf8.as_ptr());
    }
    let within = Duration::from_secs(5);
    phone.inbox.wait("the null reply", within, |m| {
        m["id"].is_null() && m["error"]["message"].as_str().unwrap().contains("null")
    });
    phone.inbox.wait("the non-UTF-8 reply", within, |m| {
        m["id"].is_null() && m["error"]["message"].as_str().unwrap().contains("UTF-8")
    });
    send(core, json!({"id": 1, "cmd": "state"}));
    let state = phone.inbox.wait("the state", within, |m| m["id"] == 1);
    assert_eq!(state["ok"]["live"], false);
    // SAFETY: from oli_core_new, freed once.
    unsafe { oli_core_free(core) };
}

#[test]
fn a_panic_in_a_command_is_the_fatal_event_and_the_core_still_answers() {
    let phone = Phone::new("panic");
    let core = phone.start();
    send(
        core,
        json!({"id": 1, "cmd": "debug_panic", "args": {"in_task": false}}),
    );
    let within = Duration::from_secs(5);
    let fatal = phone.inbox.wait("fatal", within, |m| m["event"] == "fatal");
    assert!(fatal["data"]["message"].is_string());
    send(core, json!({"id": 2, "cmd": "state"}));
    phone.inbox.wait("the state", within, |m| m["id"] == 2);
    // SAFETY: from oli_core_new, freed once.
    unsafe { oli_core_free(core) };
}

/// Free while armed: the session's teardown (the source kept, marked inactive) is
/// waited for, at most four seconds, then the core is freed and delivers nothing
/// more. Here Open Live stops answering first, so only the bound ends it.
#[test]
fn free_while_live_returns_within_its_bound() {
    let phone = Phone::new("free-live");
    let hang = Arc::new(AtomicBool::new(false));
    let open_live = fake_open_live(hang.clone());
    write_settings(&phone.settings(), &open_live);
    let core = phone.start();
    send(core, json!({"id": 1, "cmd": "go_live"}));
    let within = Duration::from_secs(30);
    assert_eq!(
        phone.inbox.wait("the reply", within, |m| m["id"] == 1),
        json!({"id": 1, "ok": null})
    );
    phone.inbox.wait("armed", within, |m| {
        m["event"] == "input" && m["data"]["state"] == "armed"
    });

    hang.store(true, Ordering::SeqCst);
    let started = Instant::now();
    // SAFETY: from oli_core_new, freed once.
    unsafe { oli_core_free(core) };
    let took = started.elapsed();
    assert!(took < Duration::from_secs(6), "freeing took {took:?}");
    let after = phone.inbox.len();
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(
        phone.inbox.len(),
        after,
        "delivered after oli_core_free returned"
    );
}

fn write_settings(path: &Path, open_live: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
        path,
        format!(
            "[gateway]\nid = \"abi-phone\"\nname = \"ABI phone\"\n\n\
             [open_live]\nurl = \"{open_live}\"\n\n\
             [video]\ncodec = \"h264\"\nencoder_preference = \"hardware\"\nrate_control = \"cbr\"\n\n\
             [[input]]\ndevice = \"cam:0\"\nlabel = \"Back\"\n"
        ),
    )
    .unwrap();
}

/// A self-hosted Open Live with no key: server info, sources, no productions. With
/// `hang` set it reads each request and never answers.
fn fake_open_live(hang: Arc<AtomicBool>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let sources = Arc::new(Mutex::new(Vec::<Value>::new()));
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let (hang, sources) = (hang.clone(), sources.clone());
            std::thread::spawn(move || {
                let _ = answer(stream, &hang, &sources);
            });
        }
    });
    url
}

fn answer(
    mut stream: TcpStream,
    hang: &AtomicBool,
    sources: &Mutex<Vec<Value>>,
) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let (method, path) = (
        parts.next().unwrap_or_default().to_string(),
        parts.next().unwrap_or_default().to_string(),
    );
    let mut length = 0;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header)? == 0 || header == "\r\n" {
            break;
        }
        if let Some((name, value)) = header.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                length = value.trim().parse().unwrap_or(0);
            }
        }
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body)?;
    if hang.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_secs(60));
        return Ok(());
    }
    let request: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let mut sources = sources.lock().unwrap();
    let (status, value) = match (method.as_str(), path.as_str()) {
        ("GET", "/api/v1/server-info") => (
            200,
            json!({"stromHost": "cloud.example.com", "srtPorts": [47260], "srtPortState": "reserved"}),
        ),
        ("GET", "/api/v1/sources") => (200, Value::Array(sources.clone())),
        ("GET", "/api/v1/productions") => (200, json!([])),
        ("POST", "/api/v1/sources") => {
            let source = json!({
                "id": "src-1",
                "name": request["name"],
                "address": request["address"].as_str().unwrap_or_default().replacen("srt://:0", "srt://:47260", 1),
                "streamType": "srt",
                "status": request["status"],
                "latency": request["latency"],
            });
            sources.push(source.clone());
            (201, source)
        }
        ("PATCH", _) => (200, sources.first().cloned().unwrap_or_default()),
        _ => (404, json!({})),
    };
    let body = value.to_string();
    write!(
        stream,
        "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
}
