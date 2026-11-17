//! The non-interactive setup calls of `open_live_ingest::api` (#73), driven with no
//! terminal against a fake Strom and a fake Open Live on localhost, and the settings
//! they write read back by the real CLI binary (AC4 of #63).

use open_live_ingest::api::{self, SaveError};
use open_live_ingest::config::{AuthMode, Config, InputSettings, UplinkMode};
use open_live_ingest::gateway::Events;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

const BIN: &str = env!("CARGO_BIN_EXE_open-live-ingest");

// ---------------------------------------------------------------------------
// A minimal HTTP/1.1 server: one request per connection, routed by a closure on
// method and path.
// ---------------------------------------------------------------------------

type Handler = Arc<dyn Fn(&str, &str) -> (u16, Value) + Send + Sync>;

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
    let (status, value) = handler(&method, &path);
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

fn device(id: &str, name: &str) -> Value {
    json!({
        "id": id,
        "name": name,
        "device_class": "Video/Source",
        "category": "videosource",
        "provider": "fake",
        "properties": {},
        "first_seen_secs_ago": 0,
        "last_seen_secs_ago": 0,
    })
}

/// A Strom that sees two cameras, one of them virtual, and one microphone. With
/// `key_required` it refuses everything, as a Strom with STROM_API_KEY set does.
fn fake_strom(key_required: bool) -> String {
    serve(Arc::new(move |method, path| {
        if key_required {
            return (401, json!({ "error": "unauthorized" }));
        }
        match (method, path) {
            ("GET", "/api/discovery/devices?category=video_source") => (
                200,
                json!([
                    device("dev-obs", "OBS Virtual Camera"),
                    device("dev-ft", "FaceTime HD Camera"),
                ]),
            ),
            ("GET", "/api/discovery/devices?category=audio_source") => {
                (200, json!([device("dev-mic", "USB Microphone")]))
            }
            ("GET", "/api/flows") => (200, json!({ "flows": [] })),
            _ => (404, Value::Null),
        }
    }))
}

/// An Open Live that wants `Bearer secret`, approximated: with `refuse` set it
/// answers 401 as for a wrong key. `ports` is what server-info publishes.
fn fake_open_live(refuse: bool, ports: Option<Vec<u16>>) -> String {
    serve(Arc::new(move |method, path| {
        if refuse {
            return (401, json!({ "error": "unauthorized" }));
        }
        match (method, path) {
            ("GET", "/api/v1/sources") => (200, json!([])),
            ("GET", "/api/v1/server-info") => match &ports {
                Some(ports) => (
                    200,
                    json!({
                        "stromHost": "cloud.example.com",
                        "srtPorts": ports,
                        "srtPortState": "reserved",
                    }),
                ),
                None => (
                    200,
                    json!({ "stromHost": "cloud.example.com", "srtPortState": "disabled" }),
                ),
            },
            _ => (404, Value::Null),
        }
    }))
}

/// Settings for a self-hosted Open Live and an existing Strom, nothing else needed.
fn settings(strom: &str, open_live: &str) -> Config {
    let mut cfg = Config::default();
    cfg.gateway.name = "Venue".to_string();
    cfg.strom.url = strom.to_string();
    cfg.strom.manage = false;
    cfg.open_live.url = Some(open_live.to_string());
    cfg.open_live.auth_mode = AuthMode::Direct;
    cfg
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("oli-api-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

// ---------------------------------------------------------------------------

/// AC4: settings written through the API are the file and format the CLI reads.
/// The binary loads and checks the file, and the file parses back to exactly what
/// was saved.
#[test]
fn settings_saved_through_the_api_are_what_the_cli_reads() {
    let dir = scratch("roundtrip");
    let path = dir.join("gateway.toml");

    let mut cfg = settings("http://127.0.0.1:8080", "https://open-live.example.com");
    cfg.open_live.api_key = Some("an-api-key".to_string());
    cfg.uplink.mode = UplinkMode::Listener;
    cfg.uplink.public_host = Some("198.51.100.7".to_string());
    cfg.uplink.port_range = Some("47110-47129".to_string());
    cfg.uplink.passphrase = Some("roundtrip-key-0123".to_string());
    cfg.capture.video_resolution = Some("1280x720".to_string());
    cfg.video.bitrate_kbps = 4000;
    cfg.inputs = vec![InputSettings {
        device: "FaceTime".to_string(),
        audio: Some("USB Microphone".to_string()),
    }];
    api::save_to(&path, &cfg).expect("saves");

    let mut cli = Command::new(BIN);
    cli.arg("--config")
        .arg(&path)
        .arg("check")
        .env("RUST_LOG", "off")
        .env("HOME", &dir)
        .env("XDG_CONFIG_HOME", &dir)
        .env("APPDATA", &dir)
        .stdin(Stdio::null());
    for var in [
        "OLI_CONFIG",
        "OLI_OPEN_LIVE_URL",
        "OLI_OPEN_LIVE_API_KEY",
        "OLI_OPEN_LIVE_AUTH_MODE",
        "OLI_OPEN_LIVE_GATEWAY_ID",
        "OLI_OPEN_LIVE_GATEWAY_TOKEN",
        "OLI_STROM_URL",
        "OLI_STROM_API_KEY",
        "OSC_ACCESS_TOKEN",
    ] {
        cli.env_remove(var);
    }
    let out = cli.output().expect("running the binary");
    assert!(
        out.status.success(),
        "the CLI refused what the API saved: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        format!("Settings at {} are valid.\n", path.display())
    );

    let raw = std::fs::read_to_string(&path).unwrap();
    let read: Config = toml::from_str(&raw).expect("the file parses as the CLI's settings");
    assert_eq!(
        toml::to_string(&read).unwrap(),
        toml::to_string(&cfg).unwrap()
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0, "group and other must have no access");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// #65 AC3: after a sign-in from the app, the secret is in the keychain and the
/// settings file holds only the marker; a later pasted token replaces it the same
/// way, and forgetting removes both.
#[test]
fn a_sign_in_kept_in_the_keychain_leaves_no_token_in_the_settings_file() {
    use open_live_ingest::config::CredentialStore;
    use open_live_ingest::credential::{self, StoredCredential};
    use open_live_ingest::keychain::MemoryKeychain;
    use open_live_ingest::osc::Credential;

    let dir = scratch("keychain");
    let path = dir.join("gateway.toml");
    let keychain = MemoryKeychain::new();
    let mut cfg = settings(
        "http://127.0.0.1:8080",
        "https://venue.eyevinn-open-live.auto.prod-se.osaas.io",
    );
    cfg.open_live.api_key = Some("an-older-pasted-token".to_string());
    cfg.uplink.port_range = Some("47110-47129".to_string());

    let sign_in = Credential {
        access_token: "access-secret".to_string(),
        refresh_token: Some("refresh-secret".to_string()),
        expires_at: Some(1_800_003_600),
        client_id: "registered-id".to_string(),
    };
    api::keep_osc_sign_in(&mut cfg, &keychain, &sign_in).unwrap();
    api::save_to(&path, &cfg).expect("saves");

    let raw = std::fs::read_to_string(&path).unwrap();
    for secret in [
        "access-secret",
        "refresh-secret",
        "an-older-pasted-token",
        "api_key",
    ] {
        assert!(!raw.contains(secret), "{secret} is in the file:\n{raw}");
    }
    assert!(raw.contains("credential = \"keychain\""), "{raw}");
    assert!(raw.contains("auth_mode = \"osc\""), "{raw}");
    assert_eq!(
        credential::load(&keychain).unwrap(),
        Some(StoredCredential::SignIn {
            refresh_token: "refresh-secret".to_string(),
            client_id: "registered-id".to_string(),
        })
    );

    api::keep_osc_token(&mut cfg, &keychain, "  pasted-pat \n").unwrap();
    assert_eq!(
        credential::load(&keychain).unwrap(),
        Some(StoredCredential::Token {
            token: "pasted-pat".to_string(),
            expires_at: None,
        })
    );
    assert_eq!(cfg.open_live.credential, Some(CredentialStore::Keychain));

    api::forget_osc_credential(&mut cfg, &keychain).unwrap();
    assert_eq!(credential::load(&keychain).unwrap(), None);
    assert_eq!(cfg.open_live.credential, None);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Invalid settings never reach the file, and the caller hears every problem.
#[test]
fn invalid_settings_are_refused_with_every_problem_and_nothing_written() {
    let dir = scratch("invalid");
    let path = dir.join("gateway.toml");
    let mut cfg = settings("http://127.0.0.1:8080", "https://open-live.example.com");
    cfg.strom.url = "not-a-url".to_string();
    cfg.capture.video_framerate = Some("25".to_string());
    match api::save_to(&path, &cfg) {
        Err(SaveError::Invalid(problems)) => {
            let settings: Vec<_> = problems.iter().map(|p| p.setting).collect();
            assert_eq!(
                settings,
                [Some("strom.url"), Some("capture.video_framerate")]
            );
        }
        other => panic!("expected the settings to be refused, got {other:?}"),
    }
    assert!(!path.exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn devices_lists_video_and_audio_with_the_virtual_flag() {
    let cfg = settings(&fake_strom(false), "http://127.0.0.1:1");
    let found = api::devices(&cfg, &Events::discard()).await.expect("lists");
    let video: Vec<(&str, &str, bool)> = found
        .video
        .iter()
        .map(|d| (d.name.as_str(), d.id.as_str(), d.virtual_))
        .collect();
    assert_eq!(
        video,
        [
            ("FaceTime HD Camera", "dev-ft", false),
            ("OBS Virtual Camera", "dev-obs", true),
        ]
    );
    let audio: Vec<&str> = found.audio.iter().map(|d| d.id.as_str()).collect();
    assert_eq!(audio, ["dev-mic"]);
    // The same shape `devices --json` prints, per device.
    let json = serde_json::to_value(&found.video[1]).unwrap();
    assert_eq!(
        json,
        json!({ "name": "OBS Virtual Camera", "id": "dev-obs", "kind": "video", "virtual": true })
    );
}

#[tokio::test]
async fn validate_finds_nothing_wrong_with_working_settings() {
    let cfg = settings(
        &fake_strom(false),
        &fake_open_live(false, Some(vec![47110])),
    );
    assert_eq!(api::validate(&cfg).await, []);
}

/// Everything setup would stop at, reported together.
#[tokio::test]
async fn validate_reports_every_problem_setup_would_stop_at() {
    let mut cfg = settings(&fake_strom(true), &fake_open_live(true, None));
    cfg.gateway.name = " ".to_string();
    cfg.capture.video_resolution = Some("1080p".to_string());
    let problems = api::validate(&cfg).await;
    let settings: Vec<_> = problems.iter().map(|p| p.setting).collect();
    assert_eq!(
        settings,
        [
            Some("capture.video_resolution"),
            Some("gateway.name"),
            Some("open_live"),
            Some("strom.api_key"),
        ],
        "{problems:#?}"
    );
    assert!(
        problems[2].message.contains("401"),
        "the reason Open Live gave is kept: {}",
        problems[2]
    );
}

#[tokio::test]
async fn validate_wants_a_port_range_only_when_nobody_publishes_one() {
    let strom = fake_strom(false);
    let cfg = settings(&strom, &fake_open_live(false, None));
    let problems = api::validate(&cfg).await;
    assert_eq!(
        problems.iter().map(|p| p.setting).collect::<Vec<_>>(),
        [Some("uplink.port_range")],
        "{problems:#?}"
    );

    let mut cfg = settings(&strom, &fake_open_live(false, None));
    cfg.uplink.port_range = Some("47110-47129".to_string());
    assert_eq!(api::validate(&cfg).await, []);

    // With registration off, the cloud Strom has to be named, and its ports too.
    let mut cfg = settings(&strom, "http://127.0.0.1:1");
    cfg.open_live.register = false;
    let problems = api::validate(&cfg).await;
    assert_eq!(
        problems.iter().map(|p| p.setting).collect::<Vec<_>>(),
        [Some("uplink.host"), Some("uplink.port_range")],
        "{problems:#?}"
    );
}

/// An unreachable Strom is a problem unless the gateway is set to start its own.
#[tokio::test]
async fn validate_reports_a_strom_that_is_not_there() {
    let closed = {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        format!("http://{}", listener.local_addr().unwrap())
    };
    let mut cfg = settings(&closed, &fake_open_live(false, Some(vec![47110])));
    let problems = api::validate(&cfg).await;
    assert_eq!(
        problems.iter().map(|p| p.setting).collect::<Vec<_>>(),
        [Some("strom.url")],
        "{problems:#?}"
    );
    cfg.strom.manage = true;
    cfg.strom.binary = "/nonexistent/strom".to_string();
    let problems = api::validate(&cfg).await;
    assert_eq!(
        problems.iter().map(|p| p.setting).collect::<Vec<_>>(),
        [Some("strom.binary")],
        "{problems:#?}"
    );
}

#[tokio::test]
async fn check_self_hosted_says_ok_or_what_is_wrong() {
    api::check_self_hosted(&fake_open_live(false, Some(vec![47110])), Some("key"))
        .await
        .expect("a working Open Live checks out");
    api::check_self_hosted(&fake_open_live(false, None), None)
        .await
        .expect("no key is fine for one that needs none");

    let refused = api::check_self_hosted(&fake_open_live(true, None), Some("wrong"))
        .await
        .unwrap_err();
    assert_eq!(refused.setting, Some("open_live"));
    assert!(refused.message.contains("401"), "{refused}");

    let osc = api::check_self_hosted("https://venue.osaas.io", Some("key"))
        .await
        .unwrap_err();
    assert_eq!(osc.setting, Some("open_live.url"));
    let scheme = api::check_self_hosted("open-live.example.com", None)
        .await
        .unwrap_err();
    assert_eq!(scheme.setting, Some("open_live.url"));
}

/// Refused before anything is asked of the platform.
#[tokio::test]
async fn instances_refuses_a_missing_or_expired_credential() {
    assert!(api::instances("  ").await.is_err());
    // A JWT whose `exp` is 1970.
    let expired = "e30.eyJleHAiOjF9.sig";
    let err = api::instances(expired).await.unwrap_err();
    assert!(err.to_string().contains("expired"), "{err:#}");
}
