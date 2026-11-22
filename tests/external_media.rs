//! OL-64a: a session whose media the host does (`ExternalMedia`, the phone) writes no
//! pidfile and asks no Strom, and does not start one. Its own test binary, so no
//! other session in the process writes the pidfile meanwhile: the check would mean
//! nothing next to the Strom sessions of `tests/session.rs`.

use open_live_ingest::config::{self, Config, InputSettings, UplinkMode};
use open_live_ingest::gateway::{Event, Events, InputState};
use open_live_ingest::media::{ExternalMedia, MediaRequest};
use open_live_ingest::run::{self, Sending, Sources, Stop};
use open_live_ingest::session::Inputs;
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::oneshot;

#[tokio::test]
async fn a_session_through_external_media_writes_no_pidfile_and_asks_no_strom() {
    let dir = std::env::temp_dir().join(format!("oli-external-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    for var in ["HOME", "XDG_CONFIG_HOME", "APPDATA"] {
        std::env::set_var(var, &dir);
    }
    // Where the settings say Strom is: nothing may connect.
    let strom = TcpListener::bind("127.0.0.1:0").unwrap();
    strom.set_nonblocking(true).unwrap();

    let mut cfg = Config::default();
    cfg.gateway.id = Some("gw-external-pidfile".to_string());
    cfg.gateway.name = "External test".to_string();
    cfg.strom.url = format!("http://{}", strom.local_addr().unwrap());
    cfg.open_live.register = false;
    cfg.uplink.mode = UplinkMode::Listener;
    cfg.uplink.host = Some("127.0.0.1".to_string());
    cfg.uplink.public_host = Some("127.0.0.1".to_string());
    cfg.uplink.port_range = Some("47410-47419".to_string());
    cfg.inputs = vec![InputSettings {
        device: "com.apple.avfoundation.avcapturedevice.built-in_video:0".to_string(),
        audio: None,
        label: Some("iPhone".to_string()),
    }];
    config::validate(&cfg).unwrap();

    let (media, mut requests) = ExternalMedia::new();
    let host = media.clone();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let events = {
        let seen = seen.clone();
        Events::new(move |e| seen.lock().unwrap().push(e))
    };
    let (stop_tx, stop_rx) = oneshot::channel::<Stop>();
    let session = tokio::spawn(async move {
        let stop = async move { stop_rx.await.unwrap_or(Stop::Interrupt) };
        run::up(
            &media,
            cfg,
            None,
            Inputs::default(),
            (Sources::Remove, Sending::Always),
            &events,
            stop,
        )
        .await
    });

    let started = tokio::time::timeout(Duration::from_secs(30), requests.recv())
        .await
        .expect("no start within 30 s")
        .expect("the session ended");
    let MediaRequest::Start { input, .. } = started else {
        panic!("not a start: {started:?}");
    };
    assert_eq!(input.name, "External test — iPhone");
    assert!(input
        .endpoint
        .venue_uri()
        .starts_with("srt://:47410?mode=listener"));
    host.report(&input.id, true, None);
    // Live: the input is up and polled, and still no pidfile.
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(
        seen.lock()
            .unwrap()
            .iter()
            .any(|e| matches!(e, Event::Summary { inputs } if inputs.len() == 1)),
        "{:?}",
        seen.lock().unwrap()
    );
    assert!(!config::pidfile().exists(), "a pidfile was written");
    assert!(!config::strom_pidfile().exists(), "a Strom was started");
    assert_eq!(run::running_gateway(), None);

    stop_tx.send(Stop::Interrupt).unwrap();
    tokio::time::timeout(Duration::from_secs(30), session)
        .await
        .expect("the session did not stop")
        .unwrap()
        .expect("the session ended on an error");
    let stop_request = requests.recv().await.expect("no stop");
    assert!(
        matches!(&stop_request, MediaRequest::Stop { input_id } if *input_id == input.id),
        "{stop_request:?}"
    );
    let seen = seen.lock().unwrap();
    assert!(
        seen.contains(&Event::FlowRemoved {
            input: "External test — iPhone".to_string()
        }),
        "{seen:?}"
    );
    assert!(
        !seen.iter().any(|e| matches!(
            e,
            Event::Input {
                state: InputState::Failed { .. },
                ..
            }
        )),
        "{seen:?}"
    );
    assert!(!config::pidfile().exists(), "a pidfile was written");
    match strom.accept() {
        Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {}
        other => panic!("something asked the Strom: {other:?}"),
    }
}
