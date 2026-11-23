//! The dispatcher against fake Open Source Cloud and Open Live servers, driven the
//! way Swift drives it: JSON in, JSON out through `deliver`.

use super::*;
use crate::testing::{self, FakeOsc, OpenLiveState, Scratch};
use open_live_ingest::keychain::MemoryKeychain;
use std::sync::mpsc::{channel, Receiver};
use std::sync::OnceLock;
use std::time::Instant;

/// Swift's side: every message the core delivered, in order.
struct Swift {
    rx: Receiver<String>,
    seen: Vec<Value>,
}

impl Swift {
    /// The first message that `wanted` picks, within `within`; the ones it passes are
    /// kept in `seen`.
    /// Messages already seen count, oldest first: a reply can overtake an event.
    fn wait(&mut self, what: &str, within: Duration, wanted: impl Fn(&Value) -> bool) -> Value {
        if let Some(at) = self.seen.iter().position(&wanted) {
            return self.seen.remove(at);
        }
        let deadline = Instant::now() + within;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let raw = self
                .rx
                .recv_timeout(left)
                .unwrap_or_else(|_| panic!("no {what} within {within:?}; seen {:#?}", self.seen));
            let message: Value = serde_json::from_str(&raw).expect("JSON");
            if wanted(&message) {
                return message;
            }
            self.seen.push(message);
        }
    }

    fn reply(&mut self, id: u32) -> Value {
        self.wait(&format!("reply {id}"), Duration::from_secs(20), |m| {
            m["id"] == json!(id)
        })
    }

    fn event(&mut self, name: &str, within: Duration, also: impl Fn(&Value) -> bool) -> Value {
        self.wait(&format!("event {name}"), within, |m| {
            m["event"] == name && also(&m["data"])
        })
    }
}

fn core_over(
    scratch: &Scratch,
    endpoints: osc::Endpoints,
    browser: Option<Opener>,
) -> (Core, Swift) {
    // Before anything logs, so that no callsite is cached as having no subscriber.
    captured_log();
    let (tx, rx) = channel();
    let tx = Mutex::new(tx);
    let deliver: Deliver = Arc::new(move |json| {
        let _ = lock(&tx).send(json);
    });
    let core = Core::new(
        Options {
            settings_file: scratch.settings(),
            keychain: Arc::new(MemoryKeychain::new()),
            endpoints,
            sign_in_wait: Duration::from_secs(10),
            browser,
        },
        deliver,
    )
    .unwrap();
    (
        core,
        Swift {
            rx,
            seen: Vec::new(),
        },
    )
}

/// Endpoints nothing answers at.
fn nowhere() -> osc::Endpoints {
    osc::Endpoints {
        connect: "http://127.0.0.1:9".into(),
        catalog: "http://127.0.0.1:9/mysubscriptions".into(),
        token_exchange: "http://127.0.0.1:9/servicetoken".into(),
    }
}

fn send(core: &Core, swift: &mut Swift, id: u32, cmd: &str, args: Value) -> Value {
    core.send(&json!({"id": id, "cmd": cmd, "args": args}).to_string());
    swift.reply(id)
}

/// Everything logged in this test binary: every test's core starts after it is set.
fn captured_log() -> &'static testing::Captured {
    static LOG: OnceLock<testing::Captured> = OnceLock::new();
    LOG.get_or_init(|| {
        let captured = testing::Captured::default();
        let subscriber = tracing_subscriber::fmt()
            .with_env_filter("debug")
            .with_ansi(false)
            .with_writer(captured.clone())
            .finish();
        tracing::subscriber::set_global_default(subscriber).expect("the first subscriber");
        captured
    })
}

#[test]
fn every_command_of_the_spec_parses() {
    let cases = [
        json!({"id": 1, "cmd": "state"}),
        json!({"id": 1, "cmd": "state", "args": null}),
        json!({"id": 1, "cmd": "osc_sign_in"}),
        json!({"id": 1, "cmd": "osc_sign_in_cancel"}),
        json!({"id": 1, "cmd": "osc_paste_token", "args": {"token": "t"}}),
        json!({"id": 1, "cmd": "choose_instance", "args": {"url": "https://x.osaas.io"}}),
        json!({"id": 1, "cmd": "self_hosted", "args": {"url": "https://x", "api_key": "k"}}),
        json!({"id": 1, "cmd": "set_input", "args": {"device": "d", "audio": "a", "label": "l"}}),
        json!({"id": 1, "cmd": "set_input", "args": {"device": "d"}}),
        json!({"id": 1, "cmd": "set_advanced", "args": {"name": "n", "latency_ms": 300,
            "passphrase": "p", "pbkeylen": 16, "bitrate_kbps": 3000, "resolution": "1920x1080",
            "framerate": "25/1", "log_level": "debug", "gateway_id": "g", "gateway_token": "t"}}),
        json!({"id": 1, "cmd": "go_live"}),
        json!({"id": 1, "cmd": "stop"}),
        json!({"id": 1, "cmd": "media_report", "args": {"input": "i", "state": "running",
            "stats": {"bytes_sent": 1, "packets_sent": 1, "packets_sent_dropped": 0,
                      "rtt_ms": 3.5, "send_rate_mbps": 4.1}}}),
        json!({"id": 1, "cmd": "media_report", "args": {"input": "i", "state": "failed", "reason": "r"}}),
        json!({"id": 1, "cmd": "phone", "args": {"foreground": false, "interrupted": false, "network": "cellular"}}),
        json!({"id": 1, "cmd": "forget_sign_in"}),
    ];
    for case in cases {
        let parsed = parse(&case.to_string());
        assert!(parsed.is_ok(), "{case}");
        assert_eq!(parsed.unwrap().1.name(), case["cmd"].as_str().unwrap());
    }
}

#[test]
fn an_unknown_command_or_bad_args_is_an_error_reply() {
    let scratch = Scratch::new("bad-commands");
    let (core, mut swift) = core_over(&scratch, nowhere(), None);
    for (raw, id) in [
        (r#"{"id": 3, "cmd": "launch_rockets"}"#, json!(3)),
        (
            r#"{"id": 4, "cmd": "set_input", "args": {"camera": "x"}}"#,
            json!(4),
        ),
        (
            r#"{"id": 5, "cmd": "phone", "args": {"foreground": "yes"}}"#,
            json!(5),
        ),
        (r#"{"cmd": "state"}"#, Value::Null),
        (r#"{"id": -1, "cmd": "state"}"#, Value::Null),
        ("not json", Value::Null),
    ] {
        core.send(raw);
        let reply = swift.wait("a reply", Duration::from_secs(5), |m| m.get("id").is_some());
        assert_eq!(reply["id"], id, "{raw}");
        assert_eq!(reply["error"]["message"], NOT_UNDERSTOOD, "{raw}");
    }
}

/// First launch (section 7.1): a new gateway id, `iPhone <first 4 hex>` as the name,
/// and the phone's defaults; a later launch keeps them.
#[test]
fn the_first_launch_writes_the_phones_identity_and_defaults() {
    let scratch = Scratch::new("first-launch");
    let (core, mut swift) = core_over(&scratch, nowhere(), None);
    let saved = api::load_from(&scratch.settings()).unwrap();
    let id = saved.gateway.id.clone().expect("a gateway id");
    assert_eq!(id.len(), 36, "{id}");
    assert_eq!(
        saved.gateway.name,
        format!("iPhone {}", id[..4].to_uppercase())
    );
    assert_eq!(saved.video.bitrate_kbps, 4000);
    assert!(config::phone_problems(&saved).is_empty());

    let state = send(&core, &mut swift, 1, "state", Value::Null)["ok"].clone();
    assert_eq!(state["where"]["mode"], "none");
    assert_eq!(state["where"]["ready"], false);
    assert_eq!(state["advanced"]["name"], saved.gateway.name);
    assert_eq!(state["advanced"]["passphrase_set"], false);
    assert_eq!(state["problems"], json!([]));
    assert_eq!(state["input"], Value::Null);
    drop(core);

    let (_again, _) = core_over(&scratch, nowhere(), None);
    assert_eq!(
        api::load_from(&scratch.settings()).unwrap().gateway.id,
        Some(id)
    );
}

/// A camera can be picked before where to send is, and a desktop's settings asking
/// for what the phone cannot send are refused at Go live.
#[test]
fn a_camera_is_kept_before_an_instance_and_desktop_video_settings_are_refused() {
    let scratch = Scratch::new("phone-rules");
    let (core, mut swift) = core_over(&scratch, nowhere(), None);
    let set = send(
        &core,
        &mut swift,
        1,
        "set_input",
        json!({"device": "cam:0", "audio": "mic:0", "label": "Back"}),
    );
    let name = api::load_from(&scratch.settings()).unwrap().gateway.name;
    assert_eq!(set["ok"]["input"]["studio_name"], format!("{name} — Back"));
    let refused = send(
        &core,
        &mut swift,
        2,
        "set_input",
        json!({"device": " ", "label": "x"}),
    );
    assert!(refused["error"]["message"].is_string(), "{refused}");

    let mut settings = api::load_from(&scratch.settings()).unwrap();
    settings.video.codec = "av1".into();
    config::save_unfinished(&scratch.settings(), &settings).unwrap();
    let state = send(&core, &mut swift, 3, "state", Value::Null);
    assert_eq!(
        state["ok"]["problems"][0]["setting"], "video.codec",
        "{state}"
    );
    let go = send(&core, &mut swift, 4, "go_live", Value::Null);
    assert!(
        go["error"]["message"]
            .as_str()
            .unwrap()
            .contains("video.codec"),
        "{go}"
    );
    assert!(!core.live());
}

/// The loopback sign-in (section 6): the page goes to Swift, `osc_signed_in` comes
/// when the browser is back (before the reply, which lists the workspace), and the
/// workspace's only instance is saved.
#[test]
fn signing_in_tells_swift_the_page_then_that_the_browser_is_back() {
    let scratch = Scratch::new("sign-in");
    let osc = FakeOsc::start(&[("Venue", "https://venue.example.osaas.io")]);
    let browser = testing::browser(testing::SIGNED_IN);
    let (core, mut swift) = core_over(&scratch, osc.endpoints(), Some(browser));
    let reply = send(&core, &mut swift, 7, "osc_sign_in", Value::Null);
    assert_eq!(reply["ok"]["workspace"], testing::WORKSPACE, "{reply}");
    assert_eq!(reply["ok"]["where"]["ready"], true);
    let names: Vec<&Value> = swift.seen.iter().map(|m| &m["event"]).collect();
    assert_eq!(names, [&json!("osc_sign_in_page"), &json!("osc_signed_in")]);
    assert!(swift.seen[0]["data"]["url"]
        .as_str()
        .unwrap()
        .contains("/api/connect/authorize?"));
    let saved = api::load_from(&scratch.settings()).unwrap();
    assert_eq!(
        saved.open_live.url.as_deref(),
        Some("https://venue.example.osaas.io")
    );
    assert!(saved.open_live.uses_keychain());
}

/// Closing the sheet ends the sign-in: its command gets an error reply, and another
/// sign-in can start.
#[test]
fn closing_the_sign_in_sheet_ends_the_sign_in() {
    let scratch = Scratch::new("sign-in-cancel");
    let osc = FakeOsc::start(&[]);
    let (core, mut swift) = core_over(
        &scratch,
        osc.endpoints(),
        Some(testing::browser(testing::NEVER)),
    );
    core.send(r#"{"id": 1, "cmd": "osc_sign_in"}"#);
    swift.event("osc_sign_in_page", Duration::from_secs(10), |_| true);
    let again = send(&core, &mut swift, 2, "osc_sign_in", Value::Null);
    assert_eq!(again["error"]["message"], SIGN_IN_WAITING);
    assert_eq!(
        send(&core, &mut swift, 3, "osc_sign_in_cancel", Value::Null),
        json!({"id": 3, "ok": null})
    );
    assert_eq!(swift.reply(1)["error"]["message"], SIGN_IN_CANCELLED);
    core.send(r#"{"id": 4, "cmd": "osc_sign_in"}"#);
    swift.event("osc_sign_in_page", Duration::from_secs(10), |_| true);
}

/// A panic in a task the core runs is the `fatal` event; the core still answers.
#[test]
fn a_panic_in_a_core_task_is_the_fatal_event() {
    let scratch = Scratch::new("panic-task");
    let (core, mut swift) = core_over(&scratch, nowhere(), None);
    assert_eq!(
        send(
            &core,
            &mut swift,
            1,
            "debug_panic",
            json!({"in_task": true})
        ),
        json!({"id": 1, "ok": null})
    );
    let fatal = swift.event("fatal", Duration::from_secs(5), |_| true);
    assert!(fatal["data"]["message"].is_string());
    assert_eq!(
        send(&core, &mut swift, 2, "state", Value::Null)["ok"]["live"],
        false
    );
}

/// Renaming the venue onto a prefix Open Live already has sources under is saved,
/// with a warning: a Stop sweeps that prefix (section 7.1).
#[test]
fn renaming_onto_a_prefix_in_use_warns() {
    let scratch = Scratch::new("rename");
    let state = Arc::new(Mutex::new(OpenLiveState::default()));
    state.lock().unwrap().sources.push(json!({
        "id": "src-other", "name": "Studio Cam — Back", "address": "srt://:47250?mode=listener",
        "streamType": "srt", "status": "inactive", "latency": 200,
    }));
    let open_live = testing::fake_open_live("self-hosted-key", state);
    let (core, mut swift) = core_over(&scratch, nowhere(), None);
    let hosted = send(
        &core,
        &mut swift,
        1,
        "self_hosted",
        json!({"url": open_live, "api_key": "self-hosted-key"}),
    );
    assert_eq!(hosted["ok"]["where"]["mode"], "self_hosted", "{hosted}");
    assert_eq!(hosted["ok"]["where"]["api_key_set"], true);

    let renamed = send(
        &core,
        &mut swift,
        2,
        "set_advanced",
        json!({"name": "Studio Cam"}),
    );
    assert_eq!(
        renamed["ok"]["warnings"][0]["setting"], "gateway.name",
        "{renamed}"
    );
    assert_eq!(renamed["ok"]["advanced"]["name"], "Studio Cam");
    let unique = send(
        &core,
        &mut swift,
        3,
        "set_advanced",
        json!({"name": "Pitch Side"}),
    );
    assert_eq!(unique["ok"]["warnings"], json!([]), "{unique}");
    let bad = send(
        &core,
        &mut swift,
        4,
        "set_advanced",
        json!({"pbkeylen": 20}),
    );
    assert_eq!(
        bad["ok"]["problems"][0]["setting"], "uplink.pbkeylen",
        "{bad}"
    );
    assert_eq!(
        api::load_from(&scratch.settings()).unwrap().uplink.pbkeylen,
        None,
        "a problem saves nothing"
    );
}

/// The whole path, as the issue's acceptance criteria name it: a pasted personal
/// access token, the workspace's instances, an instance, a camera and microphone, Go
/// live (armed), the production going active, the `media` start Swift carries out
/// and reports on, live, the phone going to the background and back, and Stop with
/// the source kept. A known passphrase crosses to Swift in `media` start and reaches
/// no log line.
#[test]
fn paste_token_choose_go_live_send_while_active_and_stop_keeping_the_source() {
    let log = captured_log();
    const PASSPHRASE: &str = "known-passphrase-4711";
    let scratch = Scratch::new("go-live");
    let open_live_state = Arc::new(Mutex::new(OpenLiveState::default()));
    let open_live = testing::fake_open_live(testing::SERVICE_TOKEN, open_live_state.clone());
    let osc = FakeOsc::start(&[("Venue", &open_live), ("Other", "https://other.osaas.io")]);
    let (core, mut swift) = core_over(&scratch, osc.endpoints(), None);
    let within = Duration::from_secs(30);

    let pasted = send(
        &core,
        &mut swift,
        1,
        "osc_paste_token",
        json!({"token": testing::ACCESS}),
    );
    assert_eq!(pasted["ok"]["workspace"], testing::WORKSPACE, "{pasted}");
    assert_eq!(pasted["ok"]["instances"].as_array().unwrap().len(), 2);
    assert_eq!(
        pasted["ok"]["where"]["ready"], false,
        "two instances: one is picked"
    );
    let chosen = send(
        &core,
        &mut swift,
        2,
        "choose_instance",
        json!({"url": open_live}),
    );
    assert_eq!(chosen["ok"]["where"]["ready"], true, "{chosen}");
    assert_eq!(chosen["ok"]["where"]["instance"], "Venue");
    assert_eq!(
        chosen["ok"]["where"]["summary"],
        format!("Sending to Venue ({})", testing::WORKSPACE)
    );
    let input = send(
        &core,
        &mut swift,
        3,
        "set_input",
        json!({"device": "com.apple.avfoundation.avcapturedevice.built-in_video:0",
               "audio": "BuiltInMicrophoneDevice", "label": "Back camera"}),
    );
    let name = input["ok"]["input"]["studio_name"]
        .as_str()
        .unwrap()
        .to_string();
    let advanced = send(
        &core,
        &mut swift,
        4,
        "set_advanced",
        json!({"passphrase": PASSPHRASE, "pbkeylen": 32}),
    );
    assert_eq!(
        advanced["ok"]["advanced"]["passphrase_set"], true,
        "{advanced}"
    );
    assert!(!advanced.to_string().contains(PASSPHRASE));

    // Go live: armed, sending nothing.
    assert_eq!(
        send(&core, &mut swift, 5, "go_live", Value::Null),
        json!({"id": 5, "ok": null})
    );
    assert!(core.live());
    let refused = send(
        &core,
        &mut swift,
        6,
        "choose_instance",
        json!({"url": open_live}),
    );
    assert_eq!(refused["error"]["message"], STOP_FIRST);
    swift.event("input", within, |d| {
        d["input"] == name && d["state"] == "armed"
    });
    swift.event("summary", within, |_| true);
    let source = open_live_state.lock().unwrap().sources[0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let set = |status: &str| {
        open_live_state.lock().unwrap().productions = vec![testing::production(
            "prod-1",
            "Morning Show",
            status,
            &[&source],
        )];
    };

    // The production goes active: Swift is asked to start, with all it needs.
    set("active");
    let start = swift.event("media", within, |d| d["action"] == "start");
    let data = &start["data"];
    let input_id = data["input"].as_str().unwrap().to_string();
    assert_eq!(data["name"], name);
    assert_eq!(
        data["device"],
        "com.apple.avfoundation.avcapturedevice.built-in_video:0"
    );
    assert_eq!(data["microphone"], "BuiltInMicrophoneDevice");
    assert_eq!(data["uplink"]["host"], "cloud.example.com");
    assert_eq!(data["uplink"]["port"], testing::PUBLISHED_PORTS[0]);
    assert_eq!(data["uplink"]["latency_ms"], 200);
    assert_eq!(data["uplink"]["passphrase"], PASSPHRASE);
    assert_eq!(data["uplink"]["pbkeylen"], 32);
    assert_eq!(
        data["video"],
        json!({"width": 1280, "height": 720, "fps": 30.0, "bitrate_kbps": 4000,
               "keyframe_interval": 30})
    );
    assert_eq!(
        data["audio"],
        json!({"sample_rate": 48000, "channels": 2, "bitrate_kbps": 128})
    );

    // Swift reports running, its bytes moving every second, until the input is live.
    let mut bytes = 0u64;
    let mut report = |core: &Core| {
        bytes += 500_000;
        core.send(
            &json!({"id": 100, "cmd": "media_report", "args": {"input": input_id,
                "state": "running", "stats": {"bytes_sent": bytes, "packets_sent": bytes / 1316,
                "packets_sent_dropped": 0, "rtt_ms": 31.0, "send_rate_mbps": 4.0}}})
            .to_string(),
        );
    };
    let deadline = Instant::now() + Duration::from_secs(45);
    loop {
        report(&core);
        let next = swift.rx.recv_timeout(Duration::from_secs(1));
        if let Ok(raw) = next {
            let m: Value = serde_json::from_str(&raw).unwrap();
            if m["event"] == "input" && m["data"]["state"] == "live" {
                break;
            }
            swift.seen.push(m);
        }
        assert!(Instant::now() < deadline, "not live: {:#?}", swift.seen);
    }
    assert!(open_live_state
        .lock()
        .unwrap()
        .patched
        .contains(&"active".to_string()));
    // The status lines name the production holding the source, its state and the slot.
    let status = swift.event("status", within, |d| {
        d["inputs"][0]["productions"][0]["state"] == "active"
    });
    assert_eq!(
        status["data"]["inputs"][0]["productions"],
        json!([{"name": "Morning Show", "state": "active", "slots": ["video_in_1"]}]),
        "{status}"
    );
    assert_eq!(status["data"]["inputs"][0]["input"], name);

    // To the background: the uplink stops, the source is marked inactive, armed.
    core.send(r#"{"id": 7, "cmd": "phone", "args": {"foreground": false, "interrupted": false, "network": "wifi"}}"#);
    swift.event("media", within, |d| {
        d["action"] == "stop" && d["input"] == input_id
    });
    swift.event("input", within, |d| {
        d["input"] == name && d["state"] == "armed"
    });
    let deadline = Instant::now() + within;
    while open_live_state.lock().unwrap().sources[0]["status"] != "inactive" {
        assert!(Instant::now() < deadline, "the source stayed active");
        std::thread::sleep(Duration::from_millis(200));
    }
    // Back: the production is still active, so it sends again.
    core.send(r#"{"id": 8, "cmd": "phone", "args": {"foreground": true, "interrupted": false, "network": "wifi"}}"#);
    swift.event("media", within, |d| {
        d["action"] == "start" && d["input"] == input_id
    });
    report(&core);

    // Stop: the uplink stops, and the source is kept for the next Go live.
    assert_eq!(
        send(&core, &mut swift, 9, "stop", Value::Null),
        json!({"id": 9, "ok": null})
    );
    let stopped = swift.event("stopped", within, |_| true);
    assert_eq!(stopped["data"]["error"], Value::Null, "{stopped}");
    let stopping: Vec<&Value> = swift
        .seen
        .iter()
        .filter(|m| m["event"] == "media" && m["data"]["action"] == "stop")
        .collect();
    assert_eq!(stopping.len(), 1, "Stop's, after the background's");
    {
        let ol = open_live_state.lock().unwrap();
        assert_eq!(ol.deleted, 0, "the source was deleted");
        assert_eq!(ol.sources.len(), 1);
        assert_eq!(ol.sources[0]["id"], source.as_str());
        assert_eq!(ol.sources[0]["status"], "inactive");
    }
    // Idle once the page hears it stopped: settings can change again.
    let mut tries = 0;
    while core.live() && tries < 50 {
        std::thread::sleep(Duration::from_millis(50));
        tries += 1;
    }
    assert!(!core.live());

    let text = log.text();
    assert!(
        text.contains("cmd=\"go_live\""),
        "the log is not captured: {text}"
    );
    assert!(text.contains("event=\"media\""), "{text}");
    for line in text.lines() {
        assert!(
            !line.contains(PASSPHRASE),
            "the passphrase was logged: {line}"
        );
        assert!(
            !line.contains(testing::ACCESS),
            "the token was logged: {line}"
        );
    }
}

#[test]
fn nothing_is_delivered_once_shutdown_returns() {
    let scratch = Scratch::new("closed");
    let (mut core, mut swift) = core_over(&scratch, nowhere(), None);
    send(&core, &mut swift, 1, "state", Value::Null);
    let shared = core.shared.clone();
    core.shutdown(SHUTDOWN_WITHIN);
    // What was delivered before it returned is Swift's already.
    while swift.rx.try_recv().is_ok() {}
    // A task that outlived the runtime's shutdown, delivering late.
    shared.emit("notice", json!({"message": "late"}));
    shared.fatal();
    shared.reply(&Reply::ok(2, Value::Null));
    assert!(
        swift.rx.try_recv().is_err(),
        "a delivery followed the shutdown"
    );
}

#[test]
fn closing_the_gate_waits_for_a_delivery_under_way() {
    let gate = Arc::new(Gate::default());
    let (entered_tx, entered_rx) = channel();
    let returned = Arc::new(AtomicBool::new(false));
    let worker = {
        let (gate, returned) = (gate.clone(), returned.clone());
        std::thread::spawn(move || {
            gate.pass(|| {
                entered_tx.send(()).unwrap();
                std::thread::sleep(Duration::from_millis(300));
                returned.store(true, Ordering::SeqCst);
            })
        })
    };
    entered_rx.recv().unwrap();
    gate.close(true);
    assert!(
        returned.load(Ordering::SeqCst),
        "close returned while Swift's callback was still running"
    );
    let mut ran = false;
    gate.pass(|| ran = true);
    assert!(!ran, "a delivery passed a closed gate");
    worker.join().unwrap();
}
