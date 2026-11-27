//! The preview's model with a fake clock, and its controller against a fake Strom.

use super::*;
use crate::go_live::{GoLive, HostFuture};
use crate::testing::{self, Captured, FakeStrom, STROM_RESTARTED};
use open_live_ingest::api::{LeftRunning, Released, StromHold};
use open_live_ingest::gateway::Events;
use open_live_ingest::keychain::MemoryKeychain;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver};

const MS: Duration = Duration::from_millis(1);

fn cam(mic: bool) -> Target {
    Target {
        device: "dev-cam".to_string(),
        name: "Venue — Cam".to_string(),
        source: Source::Device {
            device_id: "dev-cam".to_string(),
            audio_device: mic.then(|| "dev-mic".to_string()),
            resolution: None,
            framerate: None,
        },
        camera: "Cam".to_string(),
        camera_device: Some("Cam".to_string()),
        microphone: mic.then(|| "Mic".to_string()),
    }
}

fn test_pattern() -> Target {
    Target {
        device: "test".to_string(),
        name: "Venue — Test pattern".to_string(),
        source: Source::Test {
            resolution: "1920x1080".to_string(),
            framerate: "25/1".to_string(),
        },
        camera: "Test pattern".to_string(),
        camera_device: None,
        microphone: None,
    }
}

fn json_of(events: &[Event]) -> Vec<Value> {
    events
        .iter()
        .map(|e| serde_json::to_value(e).unwrap())
        .collect()
}

fn named<'a>(events: &'a [Value], name: &str) -> Vec<&'a Value> {
    events.iter().filter(|e| e["event"] == name).collect()
}

/// A preview that runs, with `targets` started at `t`: their generations by device.
fn running(targets: Vec<Target>, t: Instant) -> (Preview, BTreeMap<String, u64>) {
    let mut preview = Preview::default();
    preview.set_wanted(true);
    preview.set_capable(Some(true));
    let (plan, _) = preview.plan("gw", targets, t);
    let mut generations = BTreeMap::new();
    for start in plan.start {
        preview.started(&start.device, start.generation, &Ok(true), t);
        generations.insert(start.device, start.generation);
    }
    (preview, generations)
}

fn reading(flow_id: FlowId, peak: &[Option<f64>]) -> MeterReading {
    MeterReading {
        flow_id,
        element_id: flow::BLOCK_SOUND.to_string(),
        rms_db: peak.iter().map(|p| p.map(|p| p - 3.0)).collect(),
        peak_db: peak.to_vec(),
    }
}

fn preview_id(device: &str) -> FlowId {
    flow::preview_flow_id("gw", &flow::device_input_id(device))
}

fn jpeg(fill: u8) -> Vec<u8> {
    let mut bytes = JPEG_MARKER.to_vec();
    bytes.extend(std::iter::repeat_n(fill, 64));
    bytes
}

// ---------------------------------------------------------------------------
// Silence and levels
// ---------------------------------------------------------------------------

/// #172 AC3: below -60 dBFS for 5 s is silent; -57 does not clear it, -54 does.
#[test]
fn silence_is_raised_after_5_s_below_minus_60_and_cleared_at_minus_54() {
    let t = Instant::now();
    let mut silence = SilenceDetector::new(t);
    for k in 1..20 {
        assert_eq!(
            silence.window(Some(-70.0), t + LEVEL_WINDOW * k),
            None,
            "{k}"
        );
    }
    assert_eq!(
        silence.window(Some(-70.0), t + LEVEL_WINDOW * 20),
        Some(true)
    );
    assert!(silence.is_silent());
    assert_eq!(silence.window(Some(-57.0), t + LEVEL_WINDOW * 21), None);
    assert!(silence.is_silent(), "cleared below -54");
    assert_eq!(
        silence.window(Some(-54.0), t + LEVEL_WINDOW * 22),
        Some(false)
    );
    assert!(!silence.is_silent());
}

/// Sound in between restarts the 5 s; a level between -60 and -54 is sound.
#[test]
fn a_sound_in_between_restarts_the_five_seconds() {
    let t = Instant::now();
    let mut silence = SilenceDetector::new(t);
    for k in 1..=16 {
        assert_eq!(silence.window(Some(-80.0), t + LEVEL_WINDOW * k), None);
    }
    assert_eq!(silence.window(Some(-58.0), t + LEVEL_WINDOW * 17), None);
    for k in 18..37 {
        assert_eq!(
            silence.window(Some(-80.0), t + LEVEL_WINDOW * k),
            None,
            "{k}"
        );
    }
    assert_eq!(
        silence.window(Some(-80.0), t + LEVEL_WINDOW * 37),
        Some(true)
    );
}

/// A microphone that delivers no reading at all for 5 s while the flow runs.
#[test]
fn a_microphone_that_delivers_nothing_is_silent_after_5_s() {
    let t = Instant::now();
    let mut silence = SilenceDetector::new(t);
    assert_eq!(silence.window(None, t + Duration::from_millis(4750)), None);
    assert_eq!(silence.window(None, t + Duration::from_secs(5)), Some(true));
}

/// The level event carries per channel the highest peak and RMS of the window,
/// -inf (`null`) and anything below as -90, for every row with sound in one event.
#[test]
fn levels_are_the_highest_of_each_window_clamped_in_one_event() {
    let t = Instant::now();
    let (mut preview, _) = running(vec![cam(true), test_pattern()], t);
    let test_id = flow::preview_flow_id("gw", flow::TEST_INPUT_ID);
    preview.reading(&reading(preview_id("dev-cam"), &[Some(-20.0), None]));
    preview.reading(&reading(
        preview_id("dev-cam"),
        &[Some(-30.0), Some(-120.0)],
    ));
    preview.reading(&reading(test_id, &[Some(-18.0), Some(-18.0)]));
    // Another flow's reading is not a row's.
    preview.reading(&reading(preview_id("dev-other"), &[Some(0.0)]));
    let events = json_of(&preview.window(t + LEVEL_WINDOW));
    assert_eq!(
        events,
        vec![json!({"event": "level", "data": {"levels": [
            {"device": "dev-cam", "peak_db": [-20.0, -90.0], "rms_db": [-23.0, -90.0]},
            {"device": "test", "peak_db": [-18.0, -18.0], "rms_db": [-21.0, -21.0]},
        ]}})]
    );
    // The next window starts empty.
    assert!(preview.window(t + LEVEL_WINDOW * 2).is_empty());
}

/// A row without a microphone has no level and never warns; nor does the test
/// pattern, whose tone has a level.
#[test]
fn a_row_without_a_microphone_and_the_test_pattern_never_warn() {
    let t = Instant::now();
    let (mut preview, _) = running(vec![cam(false), test_pattern()], t);
    preview.reading(&reading(preview_id("dev-cam"), &[Some(-10.0)]));
    let mut seen = Vec::new();
    for k in 1..=40 {
        seen.extend(json_of(&preview.window(t + LEVEL_WINDOW * k)));
    }
    assert!(named(&seen, "notice").is_empty(), "{seen:?}");
    let levels = named(&seen, "level");
    assert!(
        levels.iter().all(|l| l["data"]["levels"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["device"] != "dev-cam")),
        "{levels:?}"
    );
}

/// Through the model: `no_sound` for the row's microphone at 5 s, cleared at
/// -54 dBFS; the detector runs while the window is hidden, the level event does not.
#[test]
fn no_sound_is_a_row_notice_that_clears_itself_and_runs_while_hidden() {
    let t = Instant::now();
    let (mut preview, _) = running(vec![cam(true)], t);
    preview.set_visible(false);
    let mut seen = Vec::new();
    for k in 1..=20 {
        preview.reading(&reading(preview_id("dev-cam"), &[Some(-75.0)]));
        seen.extend(json_of(&preview.window(t + LEVEL_WINDOW * k)));
    }
    assert!(named(&seen, "level").is_empty(), "hidden: {seen:?}");
    assert_eq!(
        named(&seen, "notice"),
        vec![&json!({"event": "notice", "data": {
            "kind": "no_sound",
            "message": "No sound from Mic. Check that it is on and not muted.",
            "action": null,
            "device": "dev-cam",
        }})]
    );
    preview.set_visible(true);
    preview.reading(&reading(preview_id("dev-cam"), &[Some(-40.0)]));
    let events = json_of(&preview.window(t + LEVEL_WINDOW * 21));
    assert_eq!(events[0]["event"], "level");
    assert_eq!(
        events[1],
        json!({"event": "notice", "data": {"clear": "no_sound", "device": "dev-cam"}})
    );
}

// ---------------------------------------------------------------------------
// Pictures
// ---------------------------------------------------------------------------

/// The first picture is sent with the image; the same one again is not sent, a
/// changed one is. Polled every 250 ms until the first picture, then every second.
#[test]
fn the_first_picture_is_shown_then_only_a_changed_one() {
    let t = Instant::now();
    let (mut preview, g) = running(vec![cam(false)], t);
    let gen = g["dev-cam"];
    assert_eq!(
        preview.due(t),
        vec![("dev-cam".to_string(), preview_id("dev-cam"), gen)]
    );
    assert!(preview.due(t).is_empty(), "asked twice at once");
    let none = preview.pictured(
        "dev-cam",
        gen,
        Ok(Thumbnail::NoFrame {
            capture_failed: false,
        }),
        t,
    );
    assert!(none.is_empty());
    assert!(preview.due(t + 200 * MS).is_empty());
    assert_eq!(preview.due(t + FIRST_PICTURE_EVERY).len(), 1);

    let shown =
        json_of(&preview.pictured("dev-cam", gen, Ok(Thumbnail::Jpeg(jpeg(1))), t + 300 * MS));
    assert_eq!(shown.len(), 1);
    assert_eq!(shown[0]["data"]["state"], "showing");
    assert_eq!(shown[0]["data"]["device"], "dev-cam");
    let image = shown[0]["data"]["image"].as_str().unwrap();
    assert_eq!(image, data_url(&jpeg(1)));
    assert!(image.starts_with("data:image/jpeg;base64,/9j/"));

    assert!(
        preview.due(t + 1200 * MS).is_empty(),
        "polled faster than once a second"
    );
    assert_eq!(preview.due(t + 1300 * MS).len(), 1);
    let same = preview.pictured("dev-cam", gen, Ok(Thumbnail::Jpeg(jpeg(1))), t + 1300 * MS);
    assert!(same.is_empty(), "an unchanged picture was sent again");
    preview.due(t + 2300 * MS);
    let changed =
        json_of(&preview.pictured("dev-cam", gen, Ok(Thumbnail::Jpeg(jpeg(2))), t + 2300 * MS));
    assert_eq!(changed[0]["data"]["image"], json!(data_url(&jpeg(2))));
}

/// No fresh picture for 5 s: `no_picture`, with the camera's line, whether one had
/// come or not. Strom's 500 (the capture failed) counts as no picture.
#[test]
fn no_picture_after_5_s_without_a_fresh_frame() {
    let t = Instant::now();
    let (mut preview, g) = running(vec![cam(false)], t);
    let gen = g["dev-cam"];
    let no_frame = || {
        Ok(Thumbnail::NoFrame {
            capture_failed: true,
        })
    };
    assert!(preview
        .pictured("dev-cam", gen, no_frame(), t + 4900 * MS)
        .is_empty());
    let events = json_of(&preview.pictured("dev-cam", gen, no_frame(), t + 5000 * MS));
    assert_eq!(
        events,
        vec![json!({"event": "preview", "data": {
            "device": "dev-cam", "state": "no_picture", "image": null,
            "message": "No picture from Cam.", "action": null,
        }})]
    );
    // Said once.
    assert!(preview
        .pictured("dev-cam", gen, no_frame(), t + 6000 * MS)
        .is_empty());

    // A picture again, then 5 s of 504s.
    let shown = json_of(&preview.pictured(
        "dev-cam",
        gen,
        Ok(Thumbnail::Jpeg(jpeg(3))),
        t + 7 * Duration::from_secs(1),
    ));
    assert_eq!(shown[0]["data"]["state"], "showing");
    let no_frame = || {
        Ok(Thumbnail::NoFrame {
            capture_failed: false,
        })
    };
    assert!(preview
        .pictured("dev-cam", gen, no_frame(), t + Duration::from_millis(11900))
        .is_empty());
    let events =
        json_of(&preview.pictured("dev-cam", gen, no_frame(), t + Duration::from_secs(12)));
    assert_eq!(events[0]["data"]["state"], "no_picture");
}

/// A body that is not a JPEG, or is larger than 256 KiB, never reaches the page.
#[test]
fn a_picture_that_is_not_a_jpeg_or_too_large_is_dropped() {
    let t = Instant::now();
    let (mut preview, g) = running(vec![cam(false)], t);
    let gen = g["dev-cam"];
    let png = b"\x89PNG\r\n\x1a\n".to_vec();
    assert!(preview
        .pictured("dev-cam", gen, Ok(Thumbnail::Jpeg(png)), t)
        .is_empty());
    let mut huge = jpeg(0);
    huge.resize(MAX_PICTURE + 1, 0);
    assert!(preview
        .pictured("dev-cam", gen, Ok(Thumbnail::Jpeg(huge)), t)
        .is_empty());
    let mut largest = jpeg(0);
    largest.resize(MAX_PICTURE, 0);
    assert_eq!(
        preview
            .pictured("dev-cam", gen, Ok(Thumbnail::Jpeg(largest)), t)
            .len(),
        1
    );
}

/// Strom not answering changes nothing on the row; an answer for an earlier flow
/// of the row is dropped.
#[test]
fn no_answer_or_a_late_answer_changes_nothing() {
    let t = Instant::now();
    let (mut preview, g) = running(vec![cam(false)], t);
    let gen = g["dev-cam"];
    let late = preview.pictured("dev-cam", gen + 100, Ok(Thumbnail::Jpeg(jpeg(1))), t);
    assert!(late.is_empty());
    let none = preview.pictured(
        "dev-cam",
        gen,
        Err(anyhow::anyhow!("connection refused")),
        t + Duration::from_secs(9),
    );
    assert!(none.is_empty());
}

/// A preview flow gone (a thumbnail 404, and Strom does not list the flow either)
/// while it should run is `failed`, and created again at the next reconcile.
#[test]
fn a_preview_flow_gone_is_failed_and_created_again_at_the_next_reconcile() {
    let t = Instant::now();
    let (mut preview, g) = running(vec![cam(false)], t);
    let gone = confirm_gone(false);
    assert_eq!(gone, Thumbnail::Gone);
    let events = json_of(&preview.pictured("dev-cam", g["dev-cam"], Ok(gone), t));
    assert_eq!(events[0]["data"]["state"], "failed");
    let (plan, events) = preview.plan("gw", vec![cam(false)], t + RECONCILE_EVERY);
    assert!(plan.remove.is_empty(), "nothing to remove: it is gone");
    assert_eq!(plan.start.len(), 1);
    assert_eq!(json_of(&events)[0]["data"]["state"], "starting");
}

/// #190: a thumbnail 404 for a flow Strom still lists is no fresh frame, not gone:
/// no `failed`, nothing removed or created again, and `no_picture` once 5 s pass
/// without a picture, as for 504s.
#[test]
fn a_thumbnail_404_for_a_flow_strom_still_has_is_no_picture_not_a_restart() {
    let t = Instant::now();
    let (mut preview, g) = running(vec![cam(false)], t);
    let gen = g["dev-cam"];
    let mut at = t;
    while at < t + NO_PICTURE_AFTER {
        assert_eq!(preview.due(at).len(), 1, "polled at {:?}", at - t);
        let events = preview.pictured("dev-cam", gen, Ok(confirm_gone(true)), at);
        assert!(events.is_empty(), "{:?}", json_of(&events));
        if (at - t)
            .as_millis()
            .is_multiple_of(RECONCILE_EVERY.as_millis())
        {
            let (plan, events) = preview.plan("gw", vec![cam(false)], at);
            assert!(plan.remove.is_empty() && plan.start.is_empty(), "restarted");
            assert!(events.is_empty());
        }
        at += FIRST_PICTURE_EVERY;
    }
    preview.due(at);
    let events = json_of(&preview.pictured("dev-cam", gen, Ok(confirm_gone(true)), at));
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["data"]["state"], "no_picture");
    assert_eq!(events[0]["data"]["message"], "No picture from Cam.");
    // Still not created again, and further 404s say nothing more.
    let (plan, _) = preview.plan("gw", vec![cam(false)], at + RECONCILE_EVERY);
    assert!(plan.remove.is_empty() && plan.start.is_empty());
    preview.due(at + Duration::from_secs(1));
    assert!(preview
        .pictured(
            "dev-cam",
            gen,
            Ok(confirm_gone(true)),
            at + Duration::from_secs(1)
        )
        .is_empty());
}

/// #190: a row whose flow went away and is then unticked has its flow removed (Strom
/// may still run it), and is `stopped` once Strom answers 404. Go live hands such a
/// flow over the same way.
#[test]
fn unticking_a_row_whose_flow_went_away_removes_its_flow() {
    let t = Instant::now();
    let (mut preview, g) = running(vec![cam(false)], t);
    preview.pictured("dev-cam", g["dev-cam"], Ok(confirm_gone(false)), t);
    let (plan, events) = preview.plan("gw", vec![], t + RECONCILE_EVERY);
    assert!(events.is_empty(), "stopped before the flow is gone");
    assert!(plan.start.is_empty());
    assert_eq!(plan.remove.len(), 1);
    let removal = &plan.remove[0];
    assert_eq!(removal.flow, preview_id("dev-cam"));
    assert_eq!(removal.announce, Some("stopped"));
    let events = json_of(&preview.removed(removal));
    assert_eq!(events[0]["data"]["state"], "stopped");

    let (mut preview, g) = running(vec![cam(false)], t);
    preview.pictured("dev-cam", g["dev-cam"], Ok(confirm_gone(false)), t);
    let (removals, events) = preview.hand_over();
    assert!(events.is_empty());
    assert_eq!(removals.len(), 1);
    assert_eq!(removals[0].announce, Some("camera_off"));
}

/// A flow that does not reach running: the camera's line, and no new flow until the
/// row is ticked again.
#[test]
fn a_camera_that_cannot_be_opened_waits_to_be_ticked_again() {
    let t = Instant::now();
    let mut preview = Preview::default();
    preview.set_wanted(true);
    preview.set_capable(Some(true));
    let (plan, _) = preview.plan("gw", vec![cam(false)], t);
    let start = &plan.start[0];
    let events = json_of(&preview.started(&start.device, start.generation, &Ok(false), t));
    assert_eq!(
        events,
        vec![json!({"event": "preview", "data": {
            "device": "dev-cam", "state": "failed", "image": null,
            "message": "Cam could not be opened. Close other apps that use it, then untick and tick it again.",
            "action": null,
        }})]
    );
    let (plan, _) = preview.plan("gw", vec![cam(false)], t + RECONCILE_EVERY);
    assert!(plan.start.is_empty() && plan.remove.is_empty());
    // Unticked: stopped, nothing to remove. Ticked again: a new flow.
    let (plan, events) = preview.plan("gw", vec![], t + RECONCILE_EVERY * 2);
    assert!(plan.remove.is_empty());
    assert_eq!(json_of(&events)[0]["data"]["state"], "stopped");
    let (plan, _) = preview.plan("gw", vec![cam(false)], t + RECONCILE_EVERY * 3);
    assert_eq!(plan.start.len(), 1);
}

// ---------------------------------------------------------------------------
// Wrong camera
// ---------------------------------------------------------------------------

/// strom#1278: Strom opened another camera than the one picked: `wrong_camera` on
/// the row, naming both; the same camera, or no `device-name`, says nothing.
#[test]
fn wrong_camera_on_a_mismatched_device_name() {
    let t = Instant::now();
    let (mut preview, g) = running(vec![cam(false)], t);
    let gen = g["dev-cam"];
    for opened in [
        Opened::Device("Cam".into()),
        Opened::Unknown,
        Opened::NotRunning,
    ] {
        assert_eq!(
            preview.camera_checked("dev-cam", gen, opened, false),
            (Vec::new(), None)
        );
    }
    let (events, recovery) = preview.camera_checked(
        "dev-cam",
        gen,
        Opened::Device("OBS Virtual Camera".into()),
        false,
    );
    assert_eq!(recovery, None, "not a Strom the app may restart");
    let events = json_of(&events);
    assert_eq!(
        events,
        vec![json!({"event": "notice", "data": {
            "kind": "wrong_camera",
            "message": "Strom opened \"OBS Virtual Camera\" instead of \"Cam\". Untick and tick it again.",
            "action": "show_logs",
            "device": "dev-cam",
        }})]
    );
    // Unticked: the notice goes with the row.
    let (_, events) = preview.plan("gw", vec![], t + RECONCILE_EVERY);
    assert!(json_of(&events).contains(
        &json!({"event": "notice", "data": {"clear": "wrong_camera", "device": "dev-cam"}})
    ));
}

/// The test pattern opens no camera: nothing to check.
#[test]
fn the_test_pattern_has_no_camera_to_check() {
    let t = Instant::now();
    let (mut preview, g) = running(vec![test_pattern()], t);
    let checked = preview.camera_checked("test", g["test"], Opened::Device("Cam".into()), true);
    assert_eq!(checked, (Vec::new(), None));
}

/// #196: on a Strom the app may restart, a preview row's wrong camera asks for a
/// fresh Strom instead of the notice, twice per tick; the third shows the notice.
/// Unticked and ticked again, it may recover again.
#[test]
fn a_preview_row_recovers_twice_per_tick_then_shows_the_notice() {
    let t = Instant::now();
    let mut preview = Preview::default();
    preview.set_wanted(true);
    preview.set_capable(Some(true));
    let other = || Opened::Device("FaceTime HD Camera".into());
    let start = |preview: &mut Preview| {
        let (plan, _) = preview.plan("gw", vec![cam(false)], t);
        let start = &plan.start[0];
        preview.started(&start.device, start.generation, &Ok(true), t);
        start.generation
    };
    for attempt in 1..=MAX_RECOVERIES {
        let gen = start(&mut preview);
        let (events, recovery) = preview.camera_checked("dev-cam", gen, other(), true);
        assert!(events.is_empty(), "{:?}", json_of(&events));
        assert_eq!(recovery, Some(Recovery::Preview { attempt }));
        assert!(preview.recovery_due("dev-cam", gen));
        // The recovery takes the rows out, saying nothing, and starts them again.
        let (removals, events) = preview.restart_previews();
        assert!(events.is_empty());
        assert_eq!(removals[0].announce, None);
        assert!(preview.removed(&removals[0]).is_empty());
    }
    let gen = start(&mut preview);
    let (events, recovery) = preview.camera_checked("dev-cam", gen, other(), true);
    assert_eq!(recovery, None);
    assert_eq!(json_of(&events)[0]["data"]["kind"], "wrong_camera");

    // Unticked, then ticked again: a new tick may recover again.
    preview.plan("gw", vec![], t);
    let gen = start(&mut preview);
    let (_, recovery) = preview.camera_checked("dev-cam", gen, other(), true);
    assert_eq!(recovery, Some(Recovery::Preview { attempt: 1 }));
}

/// A recovery made stale by another row's (its row started again meanwhile) is not
/// made and gives its attempt back; a Strom that could not be replaced leaves the
/// row's next wrong camera to the notice.
#[test]
fn a_stale_recovery_is_given_back_and_a_strom_not_replaced_ends_them() {
    let t = Instant::now();
    let (mut preview, g) = running(vec![cam(false)], t);
    let other = || Opened::Device("Other".into());
    let (_, recovery) = preview.camera_checked("dev-cam", g["dev-cam"], other(), true);
    assert_eq!(recovery, Some(Recovery::Preview { attempt: 1 }));
    assert!(!preview.recovery_due("dev-cam", g["dev-cam"] + 1));
    let (_, recovery) = preview.camera_checked("dev-cam", g["dev-cam"], other(), true);
    assert_eq!(
        recovery,
        Some(Recovery::Preview { attempt: 1 }),
        "the stale attempt was not given back"
    );

    preview.no_more_recoveries("dev-cam");
    let (events, recovery) = preview.camera_checked("dev-cam", g["dev-cam"], other(), true);
    assert_eq!(recovery, None);
    assert_eq!(json_of(&events)[0]["data"]["kind"], "wrong_camera");
}

/// #196: while live, before any input is on air, an uplink's wrong camera asks for
/// the session to be started again (the uplinks' rows go, and are not read until it
/// runs again), at most twice per Go live; then the notice. Once an input is on air
/// it is the notice at once, never a restart.
#[test]
fn while_live_a_wrong_camera_restarts_only_before_on_air() {
    let t = Instant::now();
    let other = || Opened::Device("Other".into());
    let uplink = flow::flow_id("gw", "dev-cam");
    let (mut preview, _) = running(vec![], t);
    preview.hand_over();
    for attempt in 1..=MAX_RECOVERIES {
        let (_, checks) = preview.uplinks(vec![(cam(true), uplink)], t);
        let (events, recovery) = preview.camera_checked("dev-cam", checks[0].2, other(), true);
        assert_eq!(recovery, Some(Recovery::Live { attempt }));
        let events = json_of(&events);
        assert_eq!(events.last().unwrap()["data"]["state"], "camera_off");
        assert!(!events.iter().any(|e| e["data"]["kind"] == "wrong_camera"));
        // The run being stopped still reports its uplinks: not read.
        let (events, checks) = preview.uplinks(vec![(cam(true), uplink)], t);
        assert!(events.is_empty() && checks.is_empty());
        preview.relaunched();
    }
    let (_, checks) = preview.uplinks(vec![(cam(true), uplink)], t);
    let (events, recovery) = preview.camera_checked("dev-cam", checks[0].2, other(), true);
    assert_eq!(recovery, None);
    assert_eq!(
        json_of(&events)[0]["data"]["message"],
        "Strom opened \"Other\" instead of \"Cam\". Stop, then Go live."
    );

    // The next Go live may recover again, until an input is on air.
    preview.live_ended();
    preview.hand_over();
    let (_, checks) = preview.uplinks(vec![(cam(true), uplink)], t);
    preview.set_on_air();
    let (events, recovery) = preview.camera_checked("dev-cam", checks[0].2, other(), true);
    assert_eq!(recovery, None, "restarted after on air");
    assert_eq!(json_of(&events)[0]["data"]["kind"], "wrong_camera");
}

// ---------------------------------------------------------------------------
// When it runs
// ---------------------------------------------------------------------------

/// An older Strom without the blocks: no preview flow, `available` false, and the
/// notice once.
#[test]
fn no_blocks_no_preview_flow_and_one_notice() {
    let t = Instant::now();
    let mut preview = Preview::default();
    preview.set_wanted(true);
    let events = json_of(&preview.set_capable(Some(false)));
    assert_eq!(
        events,
        vec![json!({"event": "notice", "data": {
            "kind": "preview_unavailable",
            "message": "This video engine has no preview. Pictures and sound levels need Strom 0.8.4 or later; going live is not affected.",
            "action": null,
        }})]
    );
    assert!(preview.set_capable(Some(false)).is_empty(), "shown twice");
    let (plan, events) = preview.plan("gw", vec![cam(true)], t);
    assert!(plan.start.is_empty() && events.is_empty());
    assert!(!preview.available());
}

/// Another gateway live on this computer: the preview flows go, none is created,
/// and the notice says why until it is gone.
#[test]
fn another_gateway_pauses_the_preview() {
    let t = Instant::now();
    let (mut preview, _) = running(vec![cam(false)], t);
    let events = json_of(&preview.set_other_gateway(Some(4242)));
    assert_eq!(
        events,
        vec![json!({"event": "notice", "data": {
            "kind": "preview_off_other_gateway",
            "message": "The preview is off while another gateway is live on this computer.",
            "action": "stop_it",
        }})]
    );
    assert!(preview.set_other_gateway(Some(4242)).is_empty());
    assert!(!preview.available());
    let (plan, _) = preview.plan("gw", vec![cam(false)], t);
    assert!(plan.start.is_empty());
    assert_eq!(plan.remove.len(), 1);
    assert_eq!(plan.remove[0].flow, preview_id("dev-cam"));
    let stopped = json_of(&preview.removed(&plan.remove[0]));
    assert_eq!(stopped[0]["data"]["state"], "stopped");

    let events = json_of(&preview.set_other_gateway(None));
    assert_eq!(
        events,
        vec![json!({"event": "notice", "data": {"clear": "preview_off_other_gateway"}})]
    );
    let (plan, _) = preview.plan("gw", vec![cam(false)], t);
    assert_eq!(plan.start.len(), 1);
}

/// Minimised: the preview flows go; shown again, they come back.
#[test]
fn a_minimised_window_runs_no_preview_flow() {
    let t = Instant::now();
    let (mut preview, _) = running(vec![cam(false)], t);
    preview.set_visible(false);
    assert!(preview.due(t + Duration::from_secs(9)).is_empty());
    let (plan, _) = preview.plan("gw", vec![cam(false)], t);
    assert_eq!(plan.remove.len(), 1);
    preview.set_visible(true);
    let (plan, _) = preview.plan("gw", vec![cam(false)], t);
    assert_eq!(plan.start.len(), 1);
}

/// Section 5.2's table: a label changed keeps the flow (its id does not depend on
/// it); an audio pick changed removes it and creates it again, saying nothing in
/// between; unticked removes it, `stopped` once it is gone.
#[test]
fn ticks_labels_and_audio_picks_follow_the_table() {
    let t = Instant::now();
    let (mut preview, _) = running(vec![cam(false)], t);

    let mut relabelled = cam(false);
    relabelled.name = "Venue — Stage left".to_string();
    let (plan, events) = preview.plan("gw", vec![relabelled], t);
    assert!(plan.remove.is_empty() && plan.start.is_empty() && events.is_empty());

    let (plan, _) = preview.plan("gw", vec![cam(true)], t);
    assert_eq!(plan.remove.len(), 1);
    assert_eq!(plan.remove[0].announce, None);
    assert_eq!(plan.start.len(), 1);
    assert!(preview.removed(&plan.remove[0]).is_empty());
    let started = &plan.start[0];
    assert!(started
        .flow
        .blocks
        .iter()
        .any(|b| b.id == flow::BLOCK_SOUND));

    let (plan, _) = preview.plan("gw", vec![], t);
    assert_eq!(plan.remove[0].announce, Some("stopped"));
    let events = json_of(&preview.removed(&plan.remove[0]));
    assert_eq!(
        events,
        vec![
            json!({"event": "preview", "data": {"device": "dev-cam", "state": "stopped", "image": null, "message": null, "action": null}})
        ]
    );
}

/// macOS refuses the camera: the camera rows say so and get no flow; the test
/// pattern, which needs none, still runs.
#[test]
fn access_refused_fails_the_rows_that_need_it_only() {
    let t = Instant::now();
    let mut preview = Preview::default();
    preview.set_wanted(true);
    preview.set_capable(Some(true));
    let (mut plan, _) = preview.plan("gw", vec![cam(true), test_pattern()], t);
    let events = json_of(&preview.denied(Media::Camera, &mut plan));
    assert_eq!(plan.start.len(), 1);
    assert_eq!(plan.start[0].device, "test");
    assert_eq!(
        events,
        vec![json!({"event": "preview", "data": {
            "device": "dev-cam", "state": "failed", "image": null,
            "message": "Open Live Ingest is not allowed to use the camera. Allow it in System Settings → Privacy & Security, then tick the camera again.",
            "action": "open_settings",
        }})]
    );
}

/// Go live: every preview flow is handed over (removed, `camera_off` once gone), and
/// none is created while live. After the session (or a refused Go live) the preview
/// stays off until the page sends `preview {on}` again.
#[test]
fn go_live_hands_every_preview_flow_over_and_the_page_asks_again_after() {
    let t = Instant::now();
    let (mut preview, _) = running(vec![cam(true), test_pattern()], t);
    let (removals, _) = preview.hand_over();
    let mut flows: Vec<FlowId> = removals.iter().map(|r| r.flow).collect();
    flows.sort();
    let mut expected = vec![preview_id("dev-cam"), flow::preview_flow_id("gw", "test")];
    expected.sort();
    assert_eq!(flows, expected);
    for removal in &removals {
        let events = json_of(&preview.removed(removal));
        assert_eq!(events[0]["data"]["state"], "camera_off");
    }
    assert!(!preview.available());
    let (plan, _) = preview.plan("gw", vec![cam(true)], t);
    assert!(plan.start.is_empty(), "a preview flow while live");

    preview.live_ended();
    let (plan, _) = preview.plan("gw", vec![cam(true)], t);
    assert!(plan.start.is_empty(), "the page has not asked again");
    preview.set_wanted(true);
    let (plan, _) = preview.plan("gw", vec![cam(true)], t);
    assert_eq!(plan.start.len(), 1);
}

/// While live, each running uplink's taps are read: polled while visible, its
/// camera checked, `camera_off` once it stops. A 404 is the session's to explain.
#[test]
fn while_live_the_uplinks_taps_are_read() {
    let t = Instant::now();
    let (mut preview, _) = running(vec![], t);
    preview.hand_over();
    let uplink = flow::flow_id("gw", "dev-cam");
    let (events, checks) = preview.uplinks(vec![(cam(true), uplink)], t);
    assert_eq!(json_of(&events)[0]["data"]["state"], "starting");
    assert_eq!(checks.len(), 1);
    let gen = checks[0].2;
    // Stats again: nothing new.
    let (events, checks) = preview.uplinks(vec![(cam(true), uplink)], t);
    assert!(events.is_empty() && checks.is_empty());
    assert_eq!(preview.due(t), vec![("dev-cam".to_string(), uplink, gen)]);
    assert!(
        !preview.is_preview("dev-cam"),
        "an uplink's 404 is not confirmed"
    );
    assert!(preview
        .pictured("dev-cam", gen, Ok(Thumbnail::Gone), t)
        .is_empty());
    let wrong = json_of(
        &preview
            .camera_checked("dev-cam", gen, Opened::Device("Other".into()), false)
            .0,
    );
    assert_eq!(
        wrong[0]["data"]["message"],
        "Strom opened \"Other\" instead of \"Cam\". Stop, then Go live."
    );
    preview.reading(&reading(uplink, &[Some(-12.0)]));
    assert_eq!(
        json_of(&preview.window(t + LEVEL_WINDOW))[0]["event"],
        "level"
    );

    let (events, _) = preview.uplinks(vec![], t);
    let events = json_of(&events);
    assert!(events.contains(
        &json!({"event": "notice", "data": {"clear": "wrong_camera", "device": "dev-cam"}})
    ));
    assert_eq!(events.last().unwrap()["data"]["state"], "camera_off");
}

/// Without the blocks the uplinks carry no taps, so there is nothing to read.
#[test]
fn while_live_on_a_strom_without_the_blocks_nothing_is_read() {
    let t = Instant::now();
    let mut preview = Preview::default();
    preview.set_capable(Some(false));
    preview.hand_over();
    let (events, checks) = preview.uplinks(vec![(cam(true), flow::flow_id("gw", "dev-cam"))], t);
    assert!(events.is_empty() && checks.is_empty());
}

/// Images and levels are IPC payloads: no log line carries one, and of the events
/// only `preview` carries the picture.
#[test]
fn no_event_or_log_line_carries_an_image() {
    let log = Captured::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(log.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::TRACE)
        .finish();
    let image = data_url(&jpeg(7));
    let encoded = image
        .trim_start_matches("data:image/jpeg;base64,")
        .to_string();
    let events = tracing::subscriber::with_default(subscriber, || {
        let t = Instant::now();
        let (mut preview, g) = running(vec![cam(true)], t);
        let gen = g["dev-cam"];
        let mut events = Vec::new();
        events.extend(preview.pictured("dev-cam", gen, Ok(Thumbnail::Jpeg(jpeg(7))), t));
        events.extend(preview.pictured("dev-cam", gen, Ok(Thumbnail::Jpeg(b"junk".to_vec())), t));
        events.extend(preview.pictured(
            "dev-cam",
            gen,
            Ok(Thumbnail::NoFrame {
                capture_failed: false,
            }),
            t + Duration::from_secs(6),
        ));
        events.extend(
            preview
                .camera_checked("dev-cam", gen, Opened::Device("Other".into()), false)
                .0,
        );
        for k in 1..=24 {
            preview.reading(&reading(preview_id("dev-cam"), &[Some(-80.0)]));
            events.extend(preview.window(t + LEVEL_WINDOW * k));
        }
        events.extend(preview.hand_over().1);
        json_of(&events)
    });
    let log = log.text();
    for needle in ["data:image", encoded.as_str(), "/9j/"] {
        assert!(!log.contains(needle), "the log carries an image: {log}");
    }
    for event in &events {
        let text = event.to_string();
        if event["event"] == "preview" && event["data"]["state"] == "showing" {
            assert_eq!(event["data"]["image"], json!(image));
        } else {
            assert!(
                !text.contains("data:image") && !text.contains(&encoded),
                "{text}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// The saved selection
// ---------------------------------------------------------------------------

fn settings_text(strom_url: &str, inputs: &str) -> String {
    format!(
        r#"[gateway]
id = "gw-app"
name = "Venue"

[strom]
url = "{strom_url}"
manage = false

[open_live]
register = false

[uplink]
host = "cloud.example.com"
port_range = "47110-47112"
{inputs}"#
    )
}

const CAMERA_WITH_MIC: &str = "\n[[input]]\ndevice = \"dev-cam\"\naudio = \"dev-mic\"\n";
const TEST_PATTERN: &str = "\n[[input]]\ndevice = \"test\"\nlabel = \"Bars\"\n";

fn devices() -> Devices {
    use open_live_ingest::run::{DeviceKind, DeviceReport};
    let report = |id: &str, name: &str, kind| DeviceReport {
        name: name.to_string(),
        id: id.to_string(),
        kind,
        virtual_: false,
    };
    Devices {
        video: vec![report("dev-cam", "Cam", DeviceKind::Video)],
        audio: vec![report("dev-mic", "Mic", DeviceKind::Audio)],
    }
}

/// The rows are the saved `[[input]]` entries, named as their sources are, with the
/// device names from Strom's list; a saved device not listed now gets no flow.
#[test]
fn targets_follow_the_saved_selection() {
    let text = settings_text(
        "http://127.0.0.1:9",
        &format!("{CAMERA_WITH_MIC}{TEST_PATTERN}\n[[input]]\ndevice = \"dev-gone\"\n"),
    );
    let settings: Config = toml::from_str(&text).unwrap();
    let targets = targets(&settings, Some(&devices()));
    let summary: Vec<_> = targets
        .iter()
        .map(|t| {
            (
                t.device.as_str(),
                t.name.as_str(),
                t.camera_device.as_deref(),
                t.microphone.as_deref(),
            )
        })
        .collect();
    assert_eq!(
        summary,
        [
            ("dev-cam", "Venue — Cam", Some("Cam"), Some("Mic")),
            ("test", "Venue — Bars", None, None),
        ]
    );
    // The preview flow opens the camera with the uplink's capture properties.
    let flow = flow::preview("gw-app", &targets[0].name, &targets[0].source);
    assert_eq!(flow.id, flow::preview_flow_id("gw-app", "dev-cam"));
    assert_eq!(flow.name, "Venue — Cam (preview)");
}

/// OL-70d: the page finds a row's `preview`, `level` and row notices by the row's
/// `device`, and a step 3 row by its Studio name. The What to send rows and the
/// preview's rows agree on both, and on the microphone ("No sound from ..."); the
/// `preview` event carries the picture state the page draws.
#[test]
fn the_page_rows_and_the_preview_rows_agree() {
    let text = settings_text(
        "http://127.0.0.1:9",
        &format!("{CAMERA_WITH_MIC}{TEST_PATTERN}"),
    );
    let settings: Config = toml::from_str(&text).unwrap();
    let listed = devices();
    let page = crate::what_to_send::model(Some(&listed), &settings, false);
    let targets = targets(&settings, Some(&listed));
    for target in &targets {
        let row = page
            .video
            .iter()
            .find(|r| r.device == target.device)
            .unwrap_or_else(|| panic!("no page row for {}", target.device));
        assert!(row.picture, "{}", row.device);
        assert_eq!(row.input, target.name);
        assert_eq!(row.microphone, target.microphone);
        assert_eq!(row.sound, target.has_sound());
    }

    let t = Instant::now();
    let (mut preview, generations) = running(targets, t);
    let gen = generations["dev-cam"];
    let shown = json_of(&preview.pictured("dev-cam", gen, Ok(Thumbnail::Jpeg(jpeg(1))), t));
    let data = &shown[0]["data"];
    assert_eq!(shown[0]["event"], "preview");
    assert_eq!(data["device"], "dev-cam");
    assert_eq!(data["state"], "showing");
    assert!(data["image"]
        .as_str()
        .unwrap()
        .starts_with("data:image/jpeg;base64,"));
    // Go live: every step 2 picture says its camera is off.
    let (removals, events) = preview.hand_over();
    let mut states: Vec<Value> = json_of(&events)
        .iter()
        .map(|e| e["data"]["state"].clone())
        .collect();
    for removal in &removals {
        states.extend(
            json_of(&preview.removed(removal))
                .iter()
                .map(|e| e["data"]["state"].clone()),
        );
    }
    assert!(!states.is_empty());
    for state in states {
        assert_eq!(state, "camera_off");
    }
}

// ---------------------------------------------------------------------------
// The controller against a fake Strom
// ---------------------------------------------------------------------------

/// A machine with no other gateway; macOS access granted unless `denied` says not.
/// The fake Strom is adopted, never started; `managed` has it stand for one the app
/// started, which a release stops ([`FakeStrom::restart`]) unless `left_running`.
#[derive(Default)]
struct FakeHost {
    denied: Mutex<Option<Media>>,
    asked: AtomicUsize,
    managed: bool,
    left_running: bool,
    strom: Option<FakeStrom>,
    /// Holds taken.
    holds: AtomicUsize,
    /// Holds released to replace the Strom.
    releases: AtomicUsize,
    /// Taking a hold fails once this many have been taken.
    holds_fail_after: Option<usize>,
}

impl Host for FakeHost {
    fn other_gateway(&self) -> Option<u32> {
        None
    }
    fn reclaim_strom(&self) {}
    fn media_access(&self, _needs: Needs) -> Result<(), Media> {
        self.asked.fetch_add(1, Ordering::SeqCst);
        match *self.denied.lock().unwrap() {
            Some(media) => Err(media),
            None => Ok(()),
        }
    }
    fn hold_strom(&self, settings: Config) -> HostFuture<anyhow::Result<StromHold>> {
        let taken = self.holds.fetch_add(1, Ordering::SeqCst);
        if self.holds_fail_after.is_some_and(|n| taken >= n) {
            return Box::pin(async { Err(anyhow::anyhow!("Strom did not start")) });
        }
        Box::pin(async move { api::hold_strom(&settings, &Events::new(|_| {})).await })
    }
    fn restartable(&self, _hold: &StromHold) -> bool {
        self.managed
    }
    fn release_strom(&self, hold: StromHold) -> HostFuture<Released> {
        self.releases.fetch_add(1, Ordering::SeqCst);
        drop(hold);
        if self.left_running {
            return Box::pin(async { Released::LeftRunning(LeftRunning::Flows { count: 1 }) });
        }
        if let Some(strom) = &self.strom {
            strom.restart();
        }
        Box::pin(async { Released::Stopped })
    }
}

fn quick() -> Timing {
    Timing {
        gone_poll: Duration::from_millis(10),
        gone_within: Duration::from_secs(2),
        camera_retry: Duration::from_millis(10),
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
}

fn deliverer() -> (Deliver, Receiver<Value>) {
    let (tx, rx) = channel();
    let tx = Mutex::new(tx);
    let deliver: Deliver = Arc::new(move |script: String| {
        let json = script
            .strip_prefix("window.oli.receive(")
            .and_then(|s| s.strip_suffix(')'))
            .unwrap();
        let _ = tx.lock().unwrap().send(serde_json::from_str(json).unwrap());
    });
    (deliver, rx)
}

/// Waits for the first delivered message `matches` accepts.
fn wait_for(rx: &Receiver<Value>, what: &str, matches: impl Fn(&Value) -> bool) -> Value {
    loop {
        let message = rx
            .recv_timeout(Duration::from_secs(10))
            .unwrap_or_else(|_| panic!("never delivered: {what}"));
        if matches(&message) {
            return message;
        }
    }
}

struct Setup {
    strom: FakeStrom,
    _scratch: testing::Scratch,
    path: PathBuf,
    host: Arc<FakeHost>,
    rt: tokio::runtime::Runtime,
}

impl Setup {
    fn new(name: &str, blocks: bool, opened: &'static str, inputs: &str) -> Self {
        Self::with(name, blocks, &[opened], inputs, |_| FakeHost::default())
    }

    /// On a Strom standing for one the app started (strom#1278 recovery), whose
    /// `n`th flow started opens `opened[n]` (the last after that).
    fn managed(name: &str, opened: &[&'static str], left_running: bool) -> Self {
        Self::with(name, true, opened, CAMERA_WITH_MIC, |strom| FakeHost {
            managed: true,
            left_running,
            strom: Some(strom),
            ..FakeHost::default()
        })
    }

    fn with(
        name: &str,
        blocks: bool,
        opened: &[&'static str],
        inputs: &str,
        host: impl FnOnce(FakeStrom) -> FakeHost,
    ) -> Self {
        let strom = FakeStrom::start_opening(blocks, 2, opened);
        let scratch = testing::Scratch::new(name);
        let path = scratch.settings();
        std::fs::write(&path, settings_text(&strom.url, inputs)).unwrap();
        Setup {
            host: Arc::new(host(strom.clone())),
            strom,
            _scratch: scratch,
            path,
            rt: runtime(),
        }
    }

    fn settings(&self) -> Config {
        api::load_from(&self.path).unwrap()
    }

    fn go_live(&self) -> GoLive {
        GoLive::new(
            self.path.clone(),
            Arc::new(MemoryKeychain::new()),
            self.host.clone(),
        )
        .with_preview(|file, held, host| {
            PreviewController::new(file, held, host).with_timing(quick())
        })
    }

    /// The controller of `go`, delivering to the receiver, with Strom's device list.
    fn preview(&self, go: &GoLive) -> (Arc<PreviewController>, Receiver<Value>) {
        let preview = go.preview();
        let (deliver, rx) = deliverer();
        preview.deliver_to(deliver, Some(devices()));
        (preview, rx)
    }

    /// Where in Strom's requests each start of `flow` is.
    fn starts_of(&self, flow: FlowId) -> Vec<usize> {
        let start = format!("POST /api/flows/{flow}/start");
        self.strom
            .calls()
            .iter()
            .enumerate()
            .filter(|(_, c)| **c == start)
            .map(|(i, _)| i)
            .collect()
    }

    /// Where in Strom's requests it was replaced with a fresh one.
    fn restarts(&self) -> Vec<usize> {
        self.strom
            .calls()
            .iter()
            .enumerate()
            .filter(|(_, c)| *c == STROM_RESTARTED)
            .map(|(i, _)| i)
            .collect()
    }

    /// Waits up to 10 s for `flow` to have been started `n` times, and its camera
    /// read after the last.
    fn wait_for_checked_start(&self, flow: FlowId, n: usize) {
        let start = format!("POST /api/flows/{flow}/start");
        let check = format!("GET /api/flows/{flow}/elements/capture:videosrc/properties");
        self.strom
            .wait_for(&format!("start {n} of {flow}, checked"), |calls| {
                let starts: Vec<usize> = calls
                    .iter()
                    .enumerate()
                    .filter(|(_, c)| **c == start)
                    .map(|(i, _)| i)
                    .collect();
                starts.len() >= n && calls[starts[n - 1]..].contains(&check)
            });
    }

    fn calls_for(&self, flow: FlowId) -> Vec<String> {
        let id = flow.to_string();
        self.strom
            .calls()
            .into_iter()
            .filter(|c| c.contains(&id))
            .collect()
    }
}

fn preview_of(device: &str) -> FlowId {
    flow::preview_flow_id("gw-app", &flow::device_input_id(device))
}

/// Ticked: the preview flow is created and started, and its camera checked.
/// Unticked: it is removed, Strom asked until it answers 404, then the row is
/// `stopped`.
#[test]
fn tick_creates_the_flow_and_untick_removes_it_after_404() {
    let setup = Setup::new("preview-tick", true, "Cam", CAMERA_WITH_MIC);
    let go = setup.go_live();
    let (preview, rx) = setup.preview(&go);
    setup.rt.block_on(preview.hold(&setup.settings())).unwrap();
    assert!(preview.capable());
    preview.set_wanted(true);
    setup.rt.block_on(preview.reconcile());

    let id = preview_of("dev-cam");
    assert_eq!(setup.strom.flow_ids(), [id.to_string()]);
    assert_eq!(
        setup.calls_for(id)[..1],
        [format!("POST /api/flows/{id}/start")]
    );
    let flow = setup.strom.flow(&id.to_string()).unwrap();
    assert_eq!(flow["name"], "Venue — Cam (preview)");
    let starting = wait_for(&rx, "starting", |m| m["event"] == "preview");
    assert_eq!(starting["data"]["state"], "starting");
    setup.strom.wait_for("the camera check", |calls| {
        calls
            .iter()
            .any(|c| c.ends_with("/elements/capture:videosrc/properties"))
    });
    assert_eq!(
        setup.host.asked.load(Ordering::SeqCst),
        1,
        "access asked before the first flow"
    );

    std::fs::write(&setup.path, settings_text(&setup.strom.url, "")).unwrap();
    setup.rt.block_on(preview.reconcile());
    assert!(setup.strom.flow_ids().is_empty());
    let calls = setup.calls_for(id);
    let delete = calls.iter().position(|c| c.starts_with("DELETE")).unwrap();
    let gets = calls[delete..]
        .iter()
        .filter(|c| c.starts_with(&format!("GET /api/flows/{id}")))
        .count();
    assert!(gets >= 3, "did not wait for 404: {calls:?}");
    let stopped = wait_for(&rx, "stopped", |m| m["data"]["state"] == "stopped");
    assert_eq!(stopped["data"]["device"], "dev-cam");
}

/// #190: Strom answers 404 for the picture of a flow it runs. The controller asks
/// Strom for the flow, finds it, and neither removes nor creates it again.
#[test]
fn a_picture_404_for_a_running_flow_does_not_restart_it() {
    let setup = Setup::new("preview-404", true, "Cam", CAMERA_WITH_MIC);
    let go = setup.go_live();
    let (preview, _rx) = setup.preview(&go);
    setup.rt.block_on(preview.hold(&setup.settings())).unwrap();
    preview.set_wanted(true);
    setup.rt.block_on(preview.reconcile());
    let id = preview_of("dev-cam");
    setup.strom.pictures_404();

    let strom = StromClient::new(&setup.strom.url, None).unwrap();
    let (listed, unknown, unconfirmed) = setup.rt.block_on(async {
        (
            picture_of(&strom, &id, true).await.unwrap(),
            picture_of(&strom, &preview_of("dev-other"), true)
                .await
                .unwrap(),
            picture_of(&strom, &id, false).await.unwrap(),
        )
    });
    assert_eq!(
        listed,
        Thumbnail::NoFrame {
            capture_failed: false
        }
    );
    assert_eq!(unknown, Thumbnail::Gone);
    assert_eq!(unconfirmed, Thumbnail::Gone);

    let asked = setup
        .calls_for(id)
        .iter()
        .filter(|c| **c == format!("GET /api/flows/{id}"))
        .count();
    setup.rt.block_on(async { preview.poll_pictures() });
    setup
        .strom
        .wait_for("the flow asked for after the 404", |calls| {
            calls
                .iter()
                .filter(|c| **c == format!("GET /api/flows/{id}"))
                .count()
                > asked
        });
    let deadline = Instant::now() + Duration::from_secs(10);
    while preview.model().rows["dev-cam"].polling {
        assert!(Instant::now() < deadline, "the picture never came back");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(preview.model().rows["dev-cam"].picture.polled(), "failed");
    setup.rt.block_on(preview.reconcile());
    let calls = setup.calls_for(id);
    assert!(
        !calls.iter().any(|c| c.starts_with("DELETE")),
        "removed: {calls:?}"
    );
    assert_eq!(
        calls
            .iter()
            .filter(|c| **c == format!("POST /api/flows/{id}/start"))
            .count(),
        1,
        "started again: {calls:?}"
    );
    assert_eq!(setup.strom.flow_ids(), [id.to_string()]);
}

/// Strom opened another camera: the row's `wrong_camera` notice.
#[test]
fn the_controller_raises_wrong_camera_from_strom_s_device_name() {
    let setup = Setup::new("preview-wrong", true, "OBS Virtual Camera", CAMERA_WITH_MIC);
    let go = setup.go_live();
    let (preview, rx) = setup.preview(&go);
    setup.rt.block_on(preview.hold(&setup.settings())).unwrap();
    preview.set_wanted(true);
    setup.rt.block_on(preview.reconcile());
    let notice = wait_for(&rx, "wrong_camera", |m| m["data"]["kind"] == "wrong_camera");
    assert_eq!(notice["data"]["device"], "dev-cam");
    assert_eq!(
        notice["data"]["message"],
        "Strom opened \"OBS Virtual Camera\" instead of \"Cam\". Untick and tick it again."
    );
    // #196: an adopted Strom is never replaced: the notice only.
    assert_eq!(setup.host.releases.load(Ordering::SeqCst), 0);
    assert_eq!(setup.host.holds.load(Ordering::SeqCst), 1);
    assert_eq!(setup.starts_of(preview_of("dev-cam")).len(), 1);
}

/// Waits until `rx` has been quiet for `quiet`, and returns what it delivered.
fn drain(rx: &Receiver<Value>, quiet: Duration) -> Vec<Value> {
    let mut seen = Vec::new();
    while let Ok(message) = rx.recv_timeout(quiet) {
        seen.push(message);
    }
    seen
}

fn wrong_camera_notices(seen: &[Value]) -> Vec<&Value> {
    seen.iter()
        .filter(|m| m["data"]["kind"] == "wrong_camera")
        .collect()
}

/// #196 item 2: on a Strom the app started, a wrong camera in the preview has the
/// preview flow removed, the Strom replaced with a fresh one, and the flow started
/// again; the second start opens the camera picked, so no notice is shown.
#[test]
fn a_wrong_camera_in_the_preview_is_recovered_with_a_fresh_strom() {
    let setup = Setup::managed("preview-recover", &["Other", "Cam"], false);
    let go = setup.go_live();
    let (preview, rx) = setup.preview(&go);
    setup.rt.block_on(preview.hold(&setup.settings())).unwrap();
    preview.set_wanted(true);
    setup.rt.block_on(preview.reconcile());
    let id = preview_of("dev-cam");
    setup.wait_for_checked_start(id, 2);
    let deadline = Instant::now() + Duration::from_secs(10);
    while preview
        .model()
        .rows
        .get("dev-cam")
        .map(|r| r.camera.clone())
        != Some(CameraCheck::Same)
    {
        assert!(
            Instant::now() < deadline,
            "the second start was never checked"
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    let starts = setup.starts_of(id);
    let restarts = setup.restarts();
    assert_eq!(starts.len(), 2);
    assert_eq!(restarts.len(), 1);
    assert!(
        starts[0] < restarts[0] && restarts[0] < starts[1],
        "{:?}",
        setup.strom.calls()
    );
    let delete = setup
        .strom
        .calls()
        .iter()
        .position(|c| *c == format!("DELETE /api/flows/{id}"))
        .unwrap();
    assert!(
        delete < restarts[0],
        "the Strom was replaced before its flow went"
    );
    assert_eq!(setup.host.releases.load(Ordering::SeqCst), 1);
    assert_eq!(setup.host.holds.load(Ordering::SeqCst), 2);
    let seen = drain(&rx, Duration::from_millis(300));
    assert!(wrong_camera_notices(&seen).is_empty(), "{seen:?}");
    assert!(
        !seen.iter().any(|m| m["data"]["state"] == "stopped"),
        "{seen:?}"
    );
    assert_eq!(setup.strom.flow_ids(), [id.to_string()]);
}

/// #196 item 2: the camera still wrong after two fresh Stroms: the notice, and no
/// third.
#[test]
fn a_camera_still_wrong_after_two_recoveries_shows_the_notice() {
    let setup = Setup::managed("preview-recover-twice", &["Other"], false);
    let go = setup.go_live();
    let (preview, rx) = setup.preview(&go);
    setup.rt.block_on(preview.hold(&setup.settings())).unwrap();
    preview.set_wanted(true);
    setup.rt.block_on(preview.reconcile());
    let notice = wait_for(&rx, "wrong_camera", |m| m["data"]["kind"] == "wrong_camera");
    assert_eq!(
        notice["data"]["message"],
        "Strom opened \"Other\" instead of \"Cam\". Untick and tick it again."
    );
    assert_eq!(setup.host.releases.load(Ordering::SeqCst), 2);
    assert_eq!(setup.restarts().len(), 2);
    assert_eq!(setup.starts_of(preview_of("dev-cam")).len(), 3);
}

/// A Strom that cannot be stopped (it runs a flow that is not ours) is held again as
/// it is: no recovery counted, and the flow's next wrong camera is the notice.
#[test]
fn a_strom_left_running_is_no_recovery_and_the_notice_follows() {
    let setup = Setup::managed("preview-left-running", &["Other"], true);
    let go = setup.go_live();
    let (preview, rx) = setup.preview(&go);
    setup.rt.block_on(preview.hold(&setup.settings())).unwrap();
    preview.set_wanted(true);
    setup.rt.block_on(preview.reconcile());
    wait_for(&rx, "wrong_camera", |m| m["data"]["kind"] == "wrong_camera");
    assert_eq!(setup.host.releases.load(Ordering::SeqCst), 1);
    assert!(setup.restarts().is_empty());
    assert_eq!(setup.starts_of(preview_of("dev-cam")).len(), 2);
    assert!(go.held().is_held());
}

/// The handover (section 4.1): Go live removes every preview flow and waits until
/// Strom answers 404 for each before the session starts, then hands the held
/// Strom to the session, whose uplinks carry the taps.
#[test]
fn go_live_removes_every_preview_flow_and_waits_for_404_before_the_session_starts() {
    let setup = Setup::new(
        "preview-handover",
        true,
        "Cam",
        &format!("{CAMERA_WITH_MIC}{TEST_PATTERN}"),
    );
    let go = setup.go_live();
    let (preview, _events) = setup.preview(&go);
    setup.rt.block_on(preview.hold(&setup.settings())).unwrap();
    preview.set_wanted(true);
    setup.rt.block_on(preview.reconcile());
    let previews = [
        preview_of("dev-cam"),
        flow::preview_flow_id("gw-app", "test"),
    ];
    assert_eq!(setup.strom.flow_ids().len(), 2);

    let before = setup.strom.calls().len();
    let (deliver, rx) = deliverer();
    go.start(9, deliver, setup.rt.handle());
    wait_for(&rx, "the reply", |m| m["id"] == 9);
    assert!(!go.held().is_held(), "the hold was not handed over");
    let uplink = flow::flow_id("gw-app", "dev-cam");
    setup.strom.wait_for("the uplink", |calls| {
        calls.contains(&format!("POST /api/flows/{uplink}/start"))
    });

    let calls = setup.strom.calls()[before..].to_vec();
    let session_starts = calls
        .iter()
        .position(|c| c.starts_with("GET /api/discovery/devices"))
        .unwrap_or_else(|| panic!("the session never listed devices: {calls:?}"));
    for id in previews {
        let path = format!("/api/flows/{id}");
        let delete = calls
            .iter()
            .position(|c| *c == format!("DELETE {path}"))
            .unwrap();
        let gets: Vec<usize> = calls
            .iter()
            .enumerate()
            .filter(|(_, c)| **c == format!("GET {path}"))
            .map(|(i, _)| i)
            .collect();
        // The fake answers twice more after the DELETE, then 404: the third GET.
        assert!(gets.len() >= 3, "{calls:?}");
        assert!(delete < gets[0]);
        assert!(
            gets[2] < session_starts,
            "the session started before the 404: {calls:?}"
        );
    }
    assert!(setup
        .strom
        .flow_ids()
        .iter()
        .all(|id| !previews.iter().any(|p| p.to_string() == *id)));
    // The uplink carries the taps.
    let flow = setup.strom.flow(&uplink.to_string()).unwrap();
    let defs: Vec<&str> = flow["blocks"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|b| b["block_definition_id"].as_str())
        .collect();
    assert!(
        defs.contains(&"builtin.thumbnail") && defs.contains(&"builtin.meter"),
        "{defs:?}"
    );

    go.stop();
    setup.rt.block_on(go.until_idle());
}

/// An older Strom without the blocks: no preview flow is created, and the session's
/// uplinks are built without taps (`Taps::Off`).
#[test]
fn no_blocks_no_preview_flow_and_the_uplinks_carry_no_taps() {
    let setup = Setup::new("preview-old-strom", false, "Cam", CAMERA_WITH_MIC);
    let go = setup.go_live();
    let (preview, rx) = setup.preview(&go);
    setup.rt.block_on(preview.hold(&setup.settings())).unwrap();
    assert!(!preview.capable());
    assert!(!preview.available());
    wait_for(&rx, "preview_unavailable", |m| {
        m["data"]["kind"] == "preview_unavailable"
    });
    preview.set_wanted(true);
    setup.rt.block_on(preview.reconcile());
    assert!(!setup.strom.calls().iter().any(|c| c == "POST /api/flows"));
    assert!(setup.strom.flow_ids().is_empty());

    let (deliver, rx) = deliverer();
    go.start(3, deliver, setup.rt.handle());
    wait_for(&rx, "the reply", |m| m["id"] == 3);
    let uplink = flow::flow_id("gw-app", "dev-cam");
    setup.strom.wait_for("the uplink", |calls| {
        calls.contains(&format!("POST /api/flows/{uplink}/start"))
    });
    let flow = setup.strom.flow(&uplink.to_string()).unwrap();
    let text = flow.to_string();
    assert!(
        !text.contains("builtin.thumbnail") && !text.contains("builtin.meter"),
        "{text}"
    );
    go.stop();
    setup.rt.block_on(go.until_idle());
}

/// #196 item 1: on a Strom the app started, Go live replaces the Strom that ran the
/// previews with a fresh one, after the preview flows are gone and before the
/// uplinks start.
#[test]
fn go_live_starts_the_uplinks_on_a_fresh_strom() {
    let setup = Setup::managed("golive-fresh", &["Cam"], false);
    let go = setup.go_live();
    let (preview, _events) = setup.preview(&go);
    setup.rt.block_on(preview.hold(&setup.settings())).unwrap();
    preview.set_wanted(true);
    setup.rt.block_on(preview.reconcile());
    let id = preview_of("dev-cam");
    assert_eq!(setup.strom.flow_ids(), [id.to_string()]);

    let (deliver, rx) = deliverer();
    go.start(5, deliver, setup.rt.handle());
    wait_for(&rx, "the reply", |m| m["id"] == 5);
    let uplink = flow::flow_id("gw-app", "dev-cam");
    setup.strom.wait_for("the uplink", |calls| {
        calls.contains(&format!("POST /api/flows/{uplink}/start"))
    });
    let calls = setup.strom.calls();
    let restarts = setup.restarts();
    assert_eq!(restarts.len(), 1, "{calls:?}");
    let removed = calls
        .iter()
        .position(|c| *c == format!("DELETE /api/flows/{id}"))
        .unwrap();
    let uplink_start = setup.starts_of(uplink)[0];
    assert!(
        removed < restarts[0] && restarts[0] < uplink_start,
        "{calls:?}"
    );
    assert_eq!(setup.host.holds.load(Ordering::SeqCst), 2);

    go.stop();
    setup.rt.block_on(go.until_idle());
}

/// #196 item 2: while live, before any input is on air, a wrong camera on the
/// uplink stops the session and goes live again on a fresh Strom; the second start
/// opens the camera picked, so no notice, and the page never hears it stop.
#[test]
fn a_wrong_camera_before_on_air_goes_live_again_on_a_fresh_strom() {
    let setup = Setup::managed("golive-recover", &["Other", "Cam"], false);
    let go = setup.go_live();
    let (preview, _events) = setup.preview(&go);
    setup.rt.block_on(preview.hold(&setup.settings())).unwrap();

    let (deliver, rx) = deliverer();
    go.start(6, deliver, setup.rt.handle());
    wait_for(&rx, "the reply", |m| m["id"] == 6);
    let uplink = flow::flow_id("gw-app", "dev-cam");
    setup.wait_for_checked_start(uplink, 2);
    let deadline = Instant::now() + Duration::from_secs(10);
    while preview
        .model()
        .rows
        .get("dev-cam")
        .map(|r| r.camera.clone())
        != Some(CameraCheck::Same)
    {
        assert!(
            Instant::now() < deadline,
            "the second uplink was never checked"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(go.is_live());
    // The hold, the fresh Strom at Go live, and the one after the wrong camera.
    assert_eq!(setup.host.holds.load(Ordering::SeqCst), 3);
    let seen = drain(&rx, Duration::from_millis(300));
    assert!(wrong_camera_notices(&seen).is_empty(), "{seen:?}");
    assert!(!seen.iter().any(|m| m["event"] == "stopped"), "{seen:?}");

    go.stop();
    setup.rt.block_on(go.until_idle());
}

/// Go live, the uplink started and checked on a fresh Strom (camera right), so the
/// session's recovery hook is installed: for the tests that queue a `Recover` by hand.
fn live_on_a_fresh_strom(setup: &Setup, go: &GoLive, id: u32) -> Receiver<Value> {
    let (deliver, rx) = deliverer();
    go.start(id, deliver, setup.rt.handle());
    wait_for(&rx, "the reply", |m| m["id"] == id);
    setup.wait_for_checked_start(flow::flow_id("gw-app", "dev-cam"), 1);
    rx
}

/// The `Recover` the preview would queue after a wrong camera on an uplink.
fn queued_recover(preview: &PreviewController) -> Recover {
    lock(&preview.live_recovery)
        .clone()
        .expect("no recovery hook on a Strom the app started")
}

/// An input goes on air (the loop handles its `Input OnAir`) after a wrong camera
/// queued a `Recover` and before the loop handles it: the session keeps running on
/// its Strom, and the uplinks are read again so the wrong camera shows the notice.
#[test]
fn an_input_on_air_before_a_queued_recover_keeps_the_session() {
    let setup = Setup::managed("golive-onair-recover", &["Cam"], false);
    let go = setup.go_live();
    let (preview, _events) = setup.preview(&go);
    setup.rt.block_on(preview.hold(&setup.settings())).unwrap();
    let rx = live_on_a_fresh_strom(&setup, &go, 7);
    let uplink = flow::flow_id("gw-app", "dev-cam");

    // As `camera_checked` leaves the model when it asks for a live recovery.
    preview.model().live_restarting = true;
    preview.on_air();
    queued_recover(&preview)(1);
    let deadline = Instant::now() + Duration::from_secs(10);
    while preview.model().live_restarting {
        assert!(Instant::now() < deadline, "the Recover was never handled");
        std::thread::sleep(Duration::from_millis(10));
    }

    assert!(go.is_live());
    let seen = drain(&rx, Duration::from_millis(300));
    assert!(!seen.iter().any(|m| m["event"] == "stopped"), "{seen:?}");
    // The hold and the fresh Strom at Go live; none after.
    assert_eq!(setup.host.holds.load(Ordering::SeqCst), 2);
    assert_eq!(setup.restarts().len(), 1);
    assert_eq!(setup.starts_of(uplink).len(), 1);
    assert!(setup.strom.flow_ids().contains(&uplink.to_string()));

    go.stop();
    setup.rt.block_on(go.until_idle());
}

/// Stop pressed while the session is being stopped for a fresh Strom: it ends as
/// Stop says, once, and goes idle without being started again.
#[test]
fn stop_during_a_live_recovery_ends_the_session() {
    let setup = Setup::managed("golive-stop-recover", &["Cam"], false);
    let go = setup.go_live();
    let (preview, _events) = setup.preview(&go);
    setup.rt.block_on(preview.hold(&setup.settings())).unwrap();
    let rx = live_on_a_fresh_strom(&setup, &go, 8);
    let uplink = flow::flow_id("gw-app", "dev-cam");

    queued_recover(&preview)(1);
    // The run's teardown has begun: the recovery is under way.
    setup.strom.wait_for("the run's teardown", |calls| {
        calls.contains(&format!("DELETE /api/flows/{uplink}"))
    });
    go.stop();
    setup.rt.block_on(go.until_idle());

    assert!(!go.is_live());
    let stopped = wait_for(&rx, "stopped", |m| m["event"] == "stopped");
    assert!(stopped["data"]["error"].is_null(), "{stopped}");
    let seen = drain(&rx, Duration::from_millis(300));
    assert!(!seen.iter().any(|m| m["event"] == "stopped"), "{seen:?}");
}

/// No fresh Strom after a wrong camera while live (taking the hold fails): the
/// session ends, and the page hears `stopped` with the error.
#[test]
fn no_fresh_strom_after_a_live_recovery_stops_with_the_error() {
    let setup = Setup::with(
        "golive-recover-fails",
        true,
        &["Other"],
        CAMERA_WITH_MIC,
        |strom| {
            FakeHost {
                managed: true,
                strom: Some(strom),
                // The hold, and the fresh Strom at Go live; the recovery's fails.
                holds_fail_after: Some(2),
                ..FakeHost::default()
            }
        },
    );
    let go = setup.go_live();
    let (preview, _events) = setup.preview(&go);
    setup.rt.block_on(preview.hold(&setup.settings())).unwrap();

    let (deliver, rx) = deliverer();
    go.start(10, deliver, setup.rt.handle());
    wait_for(&rx, "the reply", |m| m["id"] == 10);
    let stopped = wait_for(&rx, "stopped", |m| m["event"] == "stopped");
    assert!(stopped["data"]["error"].is_object(), "{stopped}");
    setup.rt.block_on(go.until_idle());
    assert!(!go.is_live());
    assert_eq!(setup.host.holds.load(Ordering::SeqCst), 3);
    assert_eq!(setup.starts_of(flow::flow_id("gw-app", "dev-cam")).len(), 1);
}

/// #196: Go live on an adopted Strom keeps it: no fresh one, the uplinks start on
/// the Strom that ran the previews.
#[test]
fn go_live_on_an_adopted_strom_keeps_it() {
    let setup = Setup::new("golive-adopted", true, "Cam", CAMERA_WITH_MIC);
    let go = setup.go_live();
    let (preview, _events) = setup.preview(&go);
    setup.rt.block_on(preview.hold(&setup.settings())).unwrap();

    let (deliver, rx) = deliverer();
    go.start(11, deliver, setup.rt.handle());
    wait_for(&rx, "the reply", |m| m["id"] == 11);
    let uplink = flow::flow_id("gw-app", "dev-cam");
    setup.strom.wait_for("the uplink", |calls| {
        calls.contains(&format!("POST /api/flows/{uplink}/start"))
    });
    assert_eq!(setup.host.releases.load(Ordering::SeqCst), 0);
    assert_eq!(setup.host.holds.load(Ordering::SeqCst), 1);
    assert!(setup.restarts().is_empty());

    go.stop();
    setup.rt.block_on(go.until_idle());
}

/// Go live failing after the handover (refused by its checks): the preview stays off
/// until the page re-sends `preview {on}`, which runs the flows again on the hold the
/// refused Go live kept.
#[test]
fn a_failed_go_live_after_the_handover_previews_again_when_the_page_asks() {
    let setup = Setup::new("preview-refused", true, "Cam", CAMERA_WITH_MIC);
    let go = setup.go_live();
    let (preview, _events) = setup.preview(&go);
    setup.rt.block_on(preview.hold(&setup.settings())).unwrap();
    preview.set_wanted(true);
    setup.rt.block_on(preview.reconcile());
    let id = preview_of("dev-cam");
    assert_eq!(setup.strom.flow_ids(), [id.to_string()]);

    *setup.host.denied.lock().unwrap() = Some(Media::Camera);
    let (deliver, rx) = deliverer();
    go.start(4, deliver, setup.rt.handle());
    let reply = wait_for(&rx, "the reply", |m| m["id"] == 4);
    assert!(reply["error"].is_object(), "{reply}");
    setup.rt.block_on(go.until_idle());
    assert!(
        setup.strom.flow_ids().is_empty(),
        "the handover left a preview flow"
    );
    assert!(go.held().is_held(), "a refused Go live keeps the hold");
    setup.rt.block_on(preview.reconcile());
    assert!(
        setup.strom.flow_ids().is_empty(),
        "previewing again before the page asked"
    );

    *setup.host.denied.lock().unwrap() = None;
    preview.set_wanted(true);
    setup.rt.block_on(preview.reconcile());
    assert_eq!(setup.strom.flow_ids(), [id.to_string()]);
}

/// Quitting while not live: the preview flows go, then the hold is released.
#[test]
fn shutting_down_removes_the_preview_flows_and_releases_the_hold() {
    let setup = Setup::new("preview-quit", true, "Cam", CAMERA_WITH_MIC);
    let go = setup.go_live();
    let (preview, _events) = setup.preview(&go);
    setup.rt.block_on(preview.hold(&setup.settings())).unwrap();
    preview.set_wanted(true);
    setup.rt.block_on(preview.reconcile());
    assert_eq!(setup.strom.flow_ids().len(), 1);
    setup.rt.block_on(preview.shut_down());
    assert!(setup.strom.flow_ids().is_empty());
    assert!(!go.held().is_held());
}
