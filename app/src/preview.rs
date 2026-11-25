//! The preview: a picture and the sound level of each ticked row in the What to send
//! step, and of each running uplink while live (docs/specs/desktop-app.md, sections
//! 4.1, 5.2 and 6; ADR-003).
//!
//! In step 2 the controller runs one short-lived preview flow per ticked row
//! (`flow::preview`) on the Strom the app holds ([`HeldStrom`]), only while the page
//! asks for it (`preview {on}`), the window is visible, the app is not live, that Strom
//! has the thumbnail and meter blocks, and no other gateway is live on this computer.
//! It reconciles on every command that changes one of these and every
//! [`RECONCILE_EVERY`]. Go live hands the camera over ([`PreviewController::hand_over`]):
//! every preview flow is removed and Strom is asked until it answers 404, before the
//! session starts. While live it reads the taps of the running uplinks instead.
//!
//! [`Preview`] is the model, pure apart from the instants it is handed, so the picture
//! states, the levels, the silence detector and the wrong-camera check are tested
//! with a fake clock. [`PreviewController`] runs it against the held Strom.
//!
//! Images and levels are IPC payloads only: nothing here logs one.

use crate::go_live::{HeldStrom, Host};
use crate::ipc::{Deliver, Event};
use crate::media_access::Needs;
use crate::messages::{Failure, Media, Message};
use base64::Engine;
use open_live_ingest::api::{self, Devices};
use open_live_ingest::config::{Config, InputSettings};
use open_live_ingest::devices::name_prefix;
use open_live_ingest::flow::{self, Source};
use open_live_ingest::gateway::{InputState, InputStats};
use open_live_ingest::strom::{Flow, FlowId, PropertyValue, StromClient, Thumbnail};
use open_live_ingest::strom_events::{self, MeterReading};
use serde_json::{json, Value};
use std::collections::hash_map::DefaultHasher;
use std::collections::BTreeMap;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};
use tokio::sync::Notify;
use tokio::task::{JoinHandle, JoinSet};
use tracing::instrument::WithSubscriber;

/// How often the preview flows are compared with what should run.
pub const RECONCILE_EVERY: Duration = Duration::from_secs(2);
/// How often a row's picture is asked for until the first one arrives.
pub const FIRST_PICTURE_EVERY: Duration = Duration::from_millis(250);
/// How often a row's picture is asked for once one has arrived.
pub const PICTURE_EVERY: Duration = Duration::from_secs(1);
/// No fresh picture for this long, while the flow runs: "No picture from ...".
pub const NO_PICTURE_AFTER: Duration = Duration::from_secs(5);
/// The levels sent to the page are the highest of each window this long (4 Hz).
pub const LEVEL_WINDOW: Duration = Duration::from_millis(250);
/// A microphone whose highest channel peak stays below this ...
pub const SILENT_BELOW_DB: f64 = -60.0;
/// ... for this long, or that delivers nothing for this long, is silent.
pub const SILENT_AFTER: Duration = Duration::from_secs(5);
/// A silent microphone is heard again at the first window whose peak reaches this
/// (6 dB above [`SILENT_BELOW_DB`], so a level at the threshold does not flicker).
pub const SOUND_AT_DB: f64 = -54.0;
/// The largest picture handed to the page.
pub const MAX_PICTURE: usize = 256 * 1024;
/// How long quitting waits for the preview flows to go and the hold to be released:
/// the CLI's bound (section 8).
pub const SHUT_DOWN_WITHIN: Duration = Duration::from_secs(4);
/// The lowest level the page is sent.
const FLOOR_DB: f64 = -90.0;
/// The capture's video source in a flow, whose `device-name` says which camera Strom
/// opened (strom#1278).
const CAMERA_ELEMENT: &str = "capture:videosrc";
const DEVICE_NAME: &str = "device-name";
/// The start of every JPEG.
const JPEG_MARKER: [u8; 3] = [0xFF, 0xD8, 0xFF];

/// Window notices (section 10).
pub const PREVIEW_UNAVAILABLE: &str = "preview_unavailable";
pub const PREVIEW_OFF_OTHER_GATEWAY: &str = "preview_off_other_gateway";
/// Row notices (section 10), sent with the row's `device`.
pub const NO_SOUND: &str = "no_sound";
pub const WRONG_CAMERA: &str = "wrong_camera";

/// How long removing a flow waits for Strom to answer 404 for it, and how often it
/// asks. Shorter in tests.
#[derive(Debug, Clone, Copy)]
pub struct Timing {
    pub gone_poll: Duration,
    pub gone_within: Duration,
    /// Before the one retry of the camera check, when the flow is not running yet.
    pub camera_retry: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Timing {
            gone_poll: Duration::from_millis(100),
            gone_within: Duration::from_secs(5),
            camera_retry: Duration::from_secs(1),
        }
    }
}

// ---------------------------------------------------------------------------
// What should run
// ---------------------------------------------------------------------------

/// One ticked row, as the settings and Strom's device list describe it.
#[derive(Debug, Clone)]
pub struct Target {
    /// The `[[input]]` device: the camera's Strom device id, or `test`.
    pub device: String,
    /// `<gateway.name> — <label or device name>`.
    pub name: String,
    pub source: Source,
    /// The camera's name, for the messages: the device's name, or "Test pattern".
    pub camera: String,
    /// The name Strom should report as `device-name` when it opens the camera; None
    /// for the test pattern, which opens none.
    pub camera_device: Option<String>,
    /// The paired microphone's name; None without one, and for the test pattern.
    pub microphone: Option<String>,
}

impl Target {
    /// Whether its flow has a `sound` block: a microphone, or the test pattern's tone.
    fn has_sound(&self) -> bool {
        self.microphone.is_some() || matches!(self.source, Source::Test { .. })
    }

    fn needs(&self) -> Needs {
        Needs {
            camera: matches!(self.source, Source::Device { .. }),
            microphone: self.microphone.is_some(),
        }
    }
}

/// The rows the saved `[[input]]` selection ticks, among the devices Strom listed
/// last (a saved device not listed now is not connected, and gets no flow). Without a
/// list, every entry counts, named by its id.
pub fn targets(settings: &Config, listed: Option<&Devices>) -> Vec<Target> {
    let prefix = name_prefix(&settings.gateway.name);
    let capture = &settings.capture;
    let name_of = |kind: &[open_live_ingest::run::DeviceReport], id: &str| {
        kind.iter().find(|d| d.id == id).map(|d| d.name.clone())
    };
    settings
        .inputs
        .iter()
        .filter_map(|entry: &InputSettings| {
            if entry.is_test_pattern() {
                let camera = crate::what_to_send::TEST_PATTERN_NAME.to_string();
                return Some(Target {
                    device: open_live_ingest::config::TEST_PATTERN_DEVICE.to_string(),
                    name: format!("{prefix}{}", entry.label().unwrap_or(&camera)),
                    source: Source::Test {
                        resolution: capture
                            .video_resolution
                            .clone()
                            .unwrap_or_else(|| "1920x1080".to_string()),
                        framerate: capture
                            .video_framerate
                            .clone()
                            .unwrap_or_else(|| "25/1".to_string()),
                    },
                    camera,
                    camera_device: None,
                    microphone: None,
                });
            }
            let device = entry.device.trim().to_string();
            // Without a list there is no name to check the opened camera against.
            let (camera, camera_device) = match listed {
                Some(listed) => {
                    let name = name_of(&listed.video, &device)?;
                    (name.clone(), Some(name))
                }
                None => (device.clone(), None),
            };
            let audio = entry
                .audio
                .as_deref()
                .map(str::trim)
                .filter(|a| !a.is_empty());
            let microphone = audio.map(|id| {
                listed
                    .and_then(|l| name_of(&l.audio, id))
                    .unwrap_or_else(|| id.to_string())
            });
            Some(Target {
                name: format!("{prefix}{}", entry.label().unwrap_or(&camera)),
                source: Source::Device {
                    device_id: device.clone(),
                    audio_device: audio.map(str::to_string),
                    resolution: capture.video_resolution.clone(),
                    framerate: capture.video_framerate.clone(),
                },
                device,
                camera_device,
                camera,
                microphone,
            })
        })
        .collect()
}

/// A preview flow without its name: two flows with the same fingerprint open the
/// camera the same way, so a changed label needs no new flow.
fn fingerprint(flow: &Flow) -> Value {
    let mut value = serde_json::to_value(flow).expect("a flow is plain JSON");
    if let Some(object) = value.as_object_mut() {
        object.remove("name");
    }
    value
}

// ---------------------------------------------------------------------------
// Levels and silence
// ---------------------------------------------------------------------------

/// Per channel, the highest peak and RMS of the current window, in dBFS.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LevelWindow {
    peak: Vec<f64>,
    rms: Vec<f64>,
    readings: usize,
}

impl LevelWindow {
    /// Adds one `MeterData` reading. `None` (digital silence, `null` in Strom's JSON)
    /// counts as -inf.
    pub fn add(&mut self, peak: &[Option<f64>], rms: &[Option<f64>]) {
        fn merge(into: &mut Vec<f64>, values: &[Option<f64>]) {
            if into.len() < values.len() {
                into.resize(values.len(), f64::NEG_INFINITY);
            }
            for (slot, value) in into.iter_mut().zip(values) {
                *slot = slot.max(value.unwrap_or(f64::NEG_INFINITY));
            }
        }
        merge(&mut self.peak, peak);
        merge(&mut self.rms, rms);
        self.readings += 1;
    }

    /// The highest channel peak of the window; None when no reading arrived.
    pub fn highest_peak(&self) -> Option<f64> {
        (self.readings > 0).then(|| self.peak.iter().copied().fold(f64::NEG_INFINITY, f64::max))
    }

    /// Ends the window: per channel peak and RMS, clamped to -90…0 dBFS, or None when
    /// no reading arrived. The next window starts empty.
    pub fn take(&mut self) -> Option<(Vec<f64>, Vec<f64>)> {
        let window = std::mem::take(self);
        let clamp =
            |v: Vec<f64>| -> Vec<f64> { v.into_iter().map(|d| d.clamp(FLOOR_DB, 0.0)).collect() };
        (window.readings > 0).then(|| (clamp(window.peak), clamp(window.rms)))
    }
}

/// Whether a microphone gives sound (section 5.2): silent once every window's highest
/// channel peak has stayed below [`SILENT_BELOW_DB`], or no reading has come, for
/// [`SILENT_AFTER`]; heard again at the first window whose peak reaches
/// [`SOUND_AT_DB`]. A pure function of the windows and the clock.
#[derive(Debug, Clone, PartialEq)]
pub struct SilenceDetector {
    quiet_since: Option<Instant>,
    silent: bool,
}

impl SilenceDetector {
    /// The clock starts when the flow runs: no reading since counts as quiet.
    pub fn new(running_since: Instant) -> Self {
        SilenceDetector {
            quiet_since: Some(running_since),
            silent: false,
        }
    }

    pub fn is_silent(&self) -> bool {
        self.silent
    }

    /// One window ending at `now`, with its highest channel peak (None: no reading
    /// in it). `Some(true)` when the microphone has just become silent, `Some(false)`
    /// when it has just been heard again.
    pub fn window(&mut self, peak: Option<f64>, now: Instant) -> Option<bool> {
        let peak = peak.unwrap_or(f64::NEG_INFINITY);
        if self.silent {
            if peak >= SOUND_AT_DB {
                self.silent = false;
                self.quiet_since = None;
                return Some(false);
            }
            return None;
        }
        if peak >= SILENT_BELOW_DB {
            self.quiet_since = None;
            return None;
        }
        let since = *self
            .quiet_since
            .get_or_insert_with(|| now.checked_sub(LEVEL_WINDOW).unwrap_or(now));
        if now.saturating_duration_since(since) >= SILENT_AFTER {
            self.silent = true;
            return Some(true);
        }
        None
    }
}

// ---------------------------------------------------------------------------
// The model
// ---------------------------------------------------------------------------

/// Where a row's picture comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// The row's own preview flow, in step 2.
    Preview,
    /// The input's uplink flow, with the taps, while live.
    Uplink,
}

/// A row's picture (section 6).
#[derive(Debug, Clone, PartialEq)]
enum PictureState {
    /// The flow runs; no picture yet.
    Starting { since: Instant },
    /// The latest picture, and when it arrived.
    Showing { at: Instant },
    /// No fresh picture for [`NO_PICTURE_AFTER`].
    NoPicture,
    /// The flow did not come up, or went away. `retry`: created again at the next
    /// reconcile (it went away); otherwise only when the row is ticked again.
    Failed { retry: bool },
}

impl PictureState {
    fn polled(&self) -> bool {
        !matches!(self, PictureState::Failed { .. })
    }
}

/// What the camera check found (strom#1278).
#[derive(Debug, Clone, PartialEq, Eq)]
enum CameraCheck {
    Pending,
    Same,
    Other,
    Unknown,
}

/// What Strom says it opened for a flow's camera.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Opened {
    /// The flow does not run (yet), or has no such element.
    NotRunning,
    /// The element has no `device-name`.
    Unknown,
    Device(String),
}

#[derive(Debug, Clone)]
struct Row {
    target: Target,
    flow: FlowId,
    kind: Kind,
    /// The preview flow without its name; None for an uplink.
    fingerprint: Option<Value>,
    /// Counts the row's starts, so the answer for an earlier flow is dropped.
    generation: u64,
    picture: PictureState,
    /// The last picture sent, by hash: the next is sent only when it differs.
    last_image: Option<u64>,
    next_poll: Instant,
    /// A picture request is under way.
    polling: bool,
    levels: LevelWindow,
    /// With a microphone, once the flow runs.
    silence: Option<SilenceDetector>,
    camera: CameraCheck,
    /// A picture Strom sent that is not a JPEG of a sane size was logged already.
    bad_picture_logged: bool,
}

impl Row {
    fn new(target: Target, flow: FlowId, kind: Kind, generation: u64, now: Instant) -> Self {
        Row {
            target,
            flow,
            kind,
            fingerprint: None,
            generation,
            picture: PictureState::Starting { since: now },
            last_image: None,
            next_poll: now,
            polling: false,
            levels: LevelWindow::default(),
            silence: None,
            camera: CameraCheck::Pending,
            bad_picture_logged: false,
        }
    }

    fn event(&self, state: &str, image: Option<String>, message: Option<Message>) -> Event {
        picture_event(&self.target.device, state, image, message)
    }

    /// The notices this row raised, cleared.
    fn clear_notices(&self) -> Vec<Event> {
        let mut out = Vec::new();
        if self
            .silence
            .as_ref()
            .is_some_and(SilenceDetector::is_silent)
        {
            out.push(clear_row_notice(NO_SOUND, &self.target.device));
        }
        if self.camera == CameraCheck::Other {
            out.push(clear_row_notice(WRONG_CAMERA, &self.target.device));
        }
        out
    }
}

fn picture_event(
    device: &str,
    state: &str,
    image: Option<String>,
    message: Option<Message>,
) -> Event {
    Event::new(
        "preview",
        json!({
            "device": device,
            "state": state,
            "image": image,
            "message": message.as_ref().map(|m| &m.message),
            "action": message.as_ref().and_then(|m| m.action),
        }),
    )
}

fn row_notice(kind: &str, device: &str, message: &Message) -> Event {
    Event::new(
        "notice",
        json!({ "kind": kind, "message": message.message, "action": message.action, "device": device }),
    )
}

fn clear_row_notice(kind: &str, device: &str) -> Event {
    Event::new("notice", json!({ "clear": kind, "device": device }))
}

fn window_notice(kind: &str, message: &Message) -> Event {
    Event::new(
        "notice",
        json!({ "kind": kind, "message": message.message, "action": message.action }),
    )
}

/// A flow to remove, and what its row says once it is gone.
#[derive(Debug, Clone)]
pub struct Removal {
    pub device: String,
    pub flow: FlowId,
    /// `stopped` (unticked, preview off) or nothing (removed to be created again).
    pub announce: Option<&'static str>,
}

/// A preview flow to create and start.
#[derive(Debug, Clone)]
pub struct Start {
    pub device: String,
    pub flow: Flow,
    pub generation: u64,
    needs: Needs,
}

/// What a reconcile has to do on Strom: first the removals, then the starts.
#[derive(Debug, Default)]
pub struct Plan {
    pub remove: Vec<Removal>,
    pub start: Vec<Start>,
}

/// The preview's state (section 6). Pure: every change is handed the instant it
/// happens at, and returns the page events it makes.
#[derive(Debug)]
pub struct Preview {
    /// The page's `preview {on}`.
    wanted: bool,
    /// The window is not minimised.
    visible: bool,
    /// Going live, live or stopping: the session owns the Strom and the cameras.
    live: bool,
    /// The held Strom has the thumbnail and meter blocks; None: no Strom held, or not
    /// asked yet.
    capable: Option<bool>,
    /// Another gateway live on this computer, by pid.
    other_gateway: Option<u32>,
    /// `preview_unavailable` has been shown.
    unavailable_shown: bool,
    rows: BTreeMap<String, Row>,
    generations: u64,
}

impl Default for Preview {
    fn default() -> Self {
        Preview {
            wanted: false,
            visible: true,
            live: false,
            capable: None,
            other_gateway: None,
            unavailable_shown: false,
            rows: BTreeMap::new(),
            generations: 0,
        }
    }
}

impl Preview {
    /// Whether preview flows run now (section 5.2, "When it runs").
    pub fn runs(&self) -> bool {
        self.wanted
            && self.visible
            && !self.live
            && self.capable == Some(true)
            && self.other_gateway.is_none()
    }

    /// What `preview {on}` replies: false when the Strom lacks the blocks, another
    /// gateway runs, or the app is live.
    pub fn available(&self) -> bool {
        self.capable != Some(false) && self.other_gateway.is_none() && !self.live
    }

    pub fn capable(&self) -> Option<bool> {
        self.capable
    }

    pub fn set_wanted(&mut self, wanted: bool) {
        self.wanted = wanted;
    }

    pub fn set_visible(&mut self, visible: bool) {
        self.visible = visible;
    }

    /// Whether the held Strom has the blocks. Without them the notice says so, once.
    pub fn set_capable(&mut self, capable: Option<bool>) -> Vec<Event> {
        self.capable = capable;
        if capable == Some(false) && !self.unavailable_shown {
            self.unavailable_shown = true;
            tracing::info!("the local Strom has no thumbnail or meter block: no preview");
            return vec![window_notice(
                PREVIEW_UNAVAILABLE,
                &Failure::PreviewUnavailable.message(),
            )];
        }
        Vec::new()
    }

    /// The gateway `gateway.pid` names, other than this app; the notice follows it.
    pub fn set_other_gateway(&mut self, pid: Option<u32>) -> Vec<Event> {
        let was = std::mem::replace(&mut self.other_gateway, pid);
        match (was, pid) {
            (None, Some(pid)) => {
                tracing::info!(pid, "another gateway is live: the preview is off");
                vec![window_notice(
                    PREVIEW_OFF_OTHER_GATEWAY,
                    &Failure::PreviewOffOtherGateway.message(),
                )]
            }
            (Some(_), None) => {
                tracing::info!("the other gateway is gone: the preview may run again");
                vec![Event::new(
                    "notice",
                    json!({ "clear": PREVIEW_OFF_OTHER_GATEWAY }),
                )]
            }
            _ => Vec::new(),
        }
    }

    fn next_generation(&mut self) -> u64 {
        self.generations += 1;
        self.generations
    }

    /// Compares the preview flows with what should run now: `targets` when
    /// [`runs`](Self::runs), else nothing. Rows to remove leave the model at once;
    /// rows to create enter it as `starting`. A changed audio pick or capture mode is
    /// a removal and a start; a changed label is nothing. A row whose flow went away
    /// is created again; one that did not come up waits to be ticked again.
    pub fn plan(
        &mut self,
        gateway_id: &str,
        targets: Vec<Target>,
        now: Instant,
    ) -> (Plan, Vec<Event>) {
        let mut plan = Plan::default();
        let mut events = Vec::new();
        let wanted: BTreeMap<String, Target> = if self.runs() {
            targets.into_iter().map(|t| (t.device.clone(), t)).collect()
        } else {
            BTreeMap::new()
        };
        let previews: Vec<String> = self
            .rows
            .iter()
            .filter(|(_, r)| r.kind == Kind::Preview)
            .map(|(d, _)| d.clone())
            .collect();
        for device in previews {
            let row = &self.rows[&device];
            let desired = wanted
                .get(&device)
                .map(|t| fingerprint(&flow::preview(gateway_id, &t.name, &t.source)));
            let keep = match (&desired, &row.picture) {
                (None, _) => false,
                (Some(_), PictureState::Failed { retry: true }) => false,
                (Some(fp), _) => row.fingerprint.as_ref() == Some(fp),
            };
            if keep {
                // The label may have changed: the names follow, the flow stays.
                let target = wanted[&device].clone();
                if let Some(row) = self.rows.get_mut(&device) {
                    row.target = target;
                }
                continue;
            }
            let row = self.rows.remove(&device).expect("listed above");
            events.extend(row.clear_notices());
            let failed = matches!(row.picture, PictureState::Failed { .. });
            if !failed {
                plan.remove.push(Removal {
                    device: device.clone(),
                    flow: row.flow,
                    announce: desired.is_none().then_some("stopped"),
                });
            } else if desired.is_none() {
                events.push(row.event("stopped", None, None));
            }
        }
        for (device, target) in wanted {
            if self.rows.contains_key(&device) {
                continue;
            }
            let flow = flow::preview(gateway_id, &target.name, &target.source);
            let generation = self.next_generation();
            let mut row = Row::new(target.clone(), flow.id, Kind::Preview, generation, now);
            row.fingerprint = Some(fingerprint(&flow));
            tracing::info!(device = %device, "preview: starting");
            events.push(row.event("starting", None, None));
            plan.start.push(Start {
                device: device.clone(),
                flow,
                generation,
                needs: target.needs(),
            });
            self.rows.insert(device, row);
        }
        (plan, events)
    }

    /// The flow of a removed row is gone (or was left to the session's `prepare`).
    pub fn removed(&mut self, removal: &Removal) -> Vec<Event> {
        match removal.announce {
            Some(state) if !self.rows.contains_key(&removal.device) => {
                tracing::info!(device = %removal.device, "preview: {state}");
                vec![picture_event(&removal.device, state, None, None)]
            }
            _ => Vec::new(),
        }
    }

    /// macOS refuses `media` (section 4): the starts that need it are dropped, and
    /// their rows say so.
    pub fn denied(&mut self, media: Media, plan: &mut Plan) -> Vec<Event> {
        let needs = |n: &Needs| match media {
            Media::Camera => n.camera,
            Media::Microphone => n.microphone,
        };
        let mut events = Vec::new();
        plan.start.retain(|start| {
            if !needs(&start.needs) {
                return true;
            }
            if let Some(row) = self.rows.get_mut(&start.device) {
                row.picture = PictureState::Failed { retry: false };
                events.push(row.event(
                    "failed",
                    None,
                    Some(Failure::PreviewAccessDenied { media }.message()),
                ));
            }
            false
        });
        events
    }

    /// A preview flow was created and started: `Ok(true)` it runs; otherwise the
    /// camera could not be opened.
    pub fn started(
        &mut self,
        device: &str,
        generation: u64,
        result: &anyhow::Result<bool>,
        now: Instant,
    ) -> Vec<Event> {
        let Some(row) = self
            .rows
            .get_mut(device)
            .filter(|r| r.generation == generation)
        else {
            return Vec::new();
        };
        match result {
            Ok(true) => {
                row.picture = PictureState::Starting { since: now };
                row.next_poll = now;
                row.silence = row
                    .target
                    .microphone
                    .is_some()
                    .then(|| SilenceDetector::new(now));
                Vec::new()
            }
            Ok(false) | Err(_) => {
                if let Err(err) = result {
                    tracing::warn!(device = %device, err = %format_args!("{err:#}"), "preview: the flow could not be started");
                } else {
                    tracing::warn!(device = %device, "preview: the flow did not reach running");
                }
                row.picture = PictureState::Failed { retry: false };
                let message = Failure::PreviewCameraNotOpened {
                    camera: row.target.camera.clone(),
                }
                .message();
                vec![row.event("failed", None, Some(message))]
            }
        }
    }

    /// The rows whose picture is due now, each marked as being asked for: the
    /// preview rows while the preview runs, the uplinks while live, and only while
    /// the window is visible.
    pub fn due(&mut self, now: Instant) -> Vec<(String, FlowId, u64)> {
        if !self.visible {
            return Vec::new();
        }
        let runs = self.runs();
        let live = self.live;
        self.rows
            .values_mut()
            .filter(|r| match r.kind {
                Kind::Preview => runs,
                Kind::Uplink => live,
            })
            .filter(|r| r.picture.polled() && !r.polling && r.next_poll <= now)
            .map(|r| {
                r.polling = true;
                (r.target.device.clone(), r.flow, r.generation)
            })
            .collect()
    }

    /// What Strom answered for a row's picture (section 5.2's table). Strom not
    /// answering changes nothing per row.
    pub fn pictured(
        &mut self,
        device: &str,
        generation: u64,
        answer: anyhow::Result<Thumbnail>,
        now: Instant,
    ) -> Vec<Event> {
        let Some(row) = self
            .rows
            .get_mut(device)
            .filter(|r| r.generation == generation)
        else {
            return Vec::new();
        };
        row.polling = false;
        let shown = row.last_image.is_some();
        row.next_poll = now
            + if shown {
                PICTURE_EVERY
            } else {
                FIRST_PICTURE_EVERY
            };
        let answer = match answer {
            Ok(answer) => answer,
            Err(err) => {
                tracing::debug!(device = %device, err = %format_args!("{err:#}"), "preview: no answer for the picture");
                return Vec::new();
            }
        };
        let frame = match answer {
            Thumbnail::Jpeg(bytes) if is_picture(&bytes) => Some(bytes),
            Thumbnail::Jpeg(bytes) => {
                if !row.bad_picture_logged {
                    row.bad_picture_logged = true;
                    tracing::debug!(device = %device, len = bytes.len(), "preview: dropped a picture that is not a JPEG of at most 256 KiB");
                }
                None
            }
            Thumbnail::NoFrame { capture_failed } => {
                if capture_failed && !row.bad_picture_logged {
                    row.bad_picture_logged = true;
                    tracing::debug!(device = %device, "preview: Strom says the capture failed (500); treated as no picture");
                }
                None
            }
            Thumbnail::Gone => {
                return match row.kind {
                    // The session's own states say why an uplink has no flow.
                    Kind::Uplink => Vec::new(),
                    Kind::Preview => {
                        tracing::info!(device = %device, "preview: failed (the flow is gone)");
                        row.picture = PictureState::Failed { retry: true };
                        let mut events = row.clear_notices();
                        row.silence = None;
                        row.camera = CameraCheck::Pending;
                        events.push(row.event("failed", None, None));
                        events
                    }
                };
            }
        };
        match frame {
            Some(bytes) => {
                let hash = hash_of(&bytes);
                let changed = row.last_image != Some(hash);
                let was_showing = matches!(row.picture, PictureState::Showing { .. });
                row.picture = PictureState::Showing { at: now };
                row.next_poll = now + PICTURE_EVERY;
                row.last_image = Some(hash);
                if !was_showing {
                    tracing::info!(device = %device, "preview: showing");
                }
                if !was_showing || changed {
                    vec![row.event("showing", Some(data_url(&bytes)), None)]
                } else {
                    Vec::new()
                }
            }
            None => {
                let quiet_since = match row.picture {
                    PictureState::Starting { since } => Some(since),
                    PictureState::Showing { at } => Some(at),
                    _ => None,
                };
                match quiet_since {
                    Some(since) if now.saturating_duration_since(since) >= NO_PICTURE_AFTER => {
                        row.picture = PictureState::NoPicture;
                        tracing::info!(device = %device, "preview: no picture");
                        let message = Failure::PreviewNoPicture {
                            camera: row.target.camera.clone(),
                        }
                        .message();
                        vec![row.event("no_picture", None, Some(message))]
                    }
                    _ => Vec::new(),
                }
            }
        }
    }

    /// One `MeterData` reading, for the row whose flow it is.
    pub fn reading(&mut self, reading: &MeterReading) {
        if let Some(row) = self.rows.values_mut().find(|r| r.flow == reading.flow_id) {
            row.levels.add(&reading.peak_db, &reading.rms_db);
        }
    }

    /// Ends a level window at `now`: the `level` event for every row with sound that
    /// had readings (none while the window is hidden), and the silence detectors'
    /// notices, which run whether or not the window is visible.
    pub fn window(&mut self, now: Instant) -> Vec<Event> {
        let mut levels = Vec::new();
        let mut events = Vec::new();
        for row in self.rows.values_mut() {
            if matches!(row.picture, PictureState::Failed { .. }) {
                row.levels = LevelWindow::default();
                continue;
            }
            let peak = row.levels.highest_peak();
            if let Some(silence) = row.silence.as_mut() {
                match silence.window(peak, now) {
                    Some(true) => {
                        let microphone = row.target.microphone.clone().unwrap_or_default();
                        tracing::info!(device = %row.target.device, "no sound from the microphone");
                        events.push(row_notice(
                            NO_SOUND,
                            &row.target.device,
                            &Failure::NoSound { microphone }.message(),
                        ));
                    }
                    Some(false) => {
                        tracing::info!(device = %row.target.device, "the microphone gives sound again");
                        events.push(clear_row_notice(NO_SOUND, &row.target.device));
                    }
                    None => {}
                }
            }
            if let Some((peak_db, rms_db)) = row.levels.take() {
                if self.visible && row.target.has_sound() {
                    levels.push(json!({
                        "device": row.target.device,
                        "peak_db": peak_db,
                        "rms_db": rms_db,
                    }));
                }
            }
        }
        if !levels.is_empty() {
            events.insert(0, Event::new("level", json!({ "levels": levels })));
        }
        events
    }

    /// What Strom opened for a row's camera (strom#1278): another device than the
    /// one picked raises `wrong_camera`, with no other action (question 22).
    pub fn camera_checked(&mut self, device: &str, generation: u64, opened: Opened) -> Vec<Event> {
        let Some(row) = self
            .rows
            .get_mut(device)
            .filter(|r| r.generation == generation)
        else {
            return Vec::new();
        };
        let Some(expected) = row.target.camera_device.clone() else {
            return Vec::new();
        };
        match opened {
            Opened::Device(name) if name.trim() == expected.trim() => {
                row.camera = CameraCheck::Same;
                Vec::new()
            }
            Opened::Device(name) => {
                tracing::warn!(device = %device, picked = %expected, opened = %name, "Strom opened another camera than the one picked (strom#1278)");
                row.camera = CameraCheck::Other;
                let message = Failure::WrongCamera {
                    opened: name,
                    camera: expected,
                    live: row.kind == Kind::Uplink,
                }
                .message();
                vec![row_notice(WRONG_CAMERA, device, &message)]
            }
            Opened::Unknown => {
                tracing::debug!(device = %device, "the camera's element has no device-name; not checked");
                row.camera = CameraCheck::Unknown;
                Vec::new()
            }
            Opened::NotRunning => Vec::new(),
        }
    }

    /// Go live: from here the session owns the Strom. Every preview row leaves the
    /// model; their flows are returned, to be removed before the session starts.
    pub fn hand_over(&mut self) -> (Vec<Removal>, Vec<Event>) {
        self.live = true;
        self.take_previews("camera_off")
    }

    /// Quitting: the preview is off, and every preview flow is returned, to be
    /// removed before the hold is released.
    pub fn shut_down(&mut self) -> (Vec<Removal>, Vec<Event>) {
        self.wanted = false;
        self.take_previews("stopped")
    }

    /// The preview rows of a Strom no longer held: gone with it.
    fn forget_previews(&mut self) -> Vec<Event> {
        let (removals, mut events) = self.take_previews("stopped");
        events.extend(
            removals
                .iter()
                .map(|r| picture_event(&r.device, "stopped", None, None)),
        );
        events
    }

    fn take_previews(&mut self, announce: &'static str) -> (Vec<Removal>, Vec<Event>) {
        let mut removals = Vec::new();
        let mut events = Vec::new();
        let previews: Vec<String> = self
            .rows
            .iter()
            .filter(|(_, r)| r.kind == Kind::Preview)
            .map(|(d, _)| d.clone())
            .collect();
        for device in previews {
            let row = self.rows.remove(&device).expect("listed above");
            events.extend(row.clear_notices());
            if matches!(row.picture, PictureState::Failed { .. }) {
                events.push(row.event(announce, None, None));
            } else {
                removals.push(Removal {
                    device,
                    flow: row.flow,
                    announce: Some(announce),
                });
            }
        }
        (removals, events)
    }

    /// The session has ended (stopped, failed or refused). The uplinks' rows go; the
    /// page asks for the preview again when step 2 is enabled (`preview {on}`), as
    /// after Stop, so the preview stays off until then.
    pub fn live_ended(&mut self) -> Vec<Event> {
        self.live = false;
        self.wanted = false;
        self.drop_uplinks(|_| false)
    }

    fn drop_uplinks(&mut self, keep: impl Fn(&Row) -> bool) -> Vec<Event> {
        let mut events = Vec::new();
        let gone: Vec<String> = self
            .rows
            .iter()
            .filter(|(_, r)| r.kind == Kind::Uplink && !keep(r))
            .map(|(d, _)| d.clone())
            .collect();
        for device in gone {
            let row = self.rows.remove(&device).expect("listed above");
            events.extend(row.clear_notices());
            events.push(row.event("camera_off", None, None));
        }
        events
    }

    /// While live: the uplinks running now, from the session's stats, each with its
    /// flow id. A new one is polled from now on, and its camera checked (returned); one
    /// no longer running is `camera_off`.
    pub fn uplinks(
        &mut self,
        running: Vec<(Target, FlowId)>,
        now: Instant,
    ) -> (Vec<Event>, Vec<(String, FlowId, u64)>) {
        if !self.live || self.capable != Some(true) {
            return (Vec::new(), Vec::new());
        }
        let flows: Vec<FlowId> = running.iter().map(|(_, f)| *f).collect();
        let mut events = self.drop_uplinks(|r| flows.contains(&r.flow));
        let mut checks = Vec::new();
        for (target, flow_id) in running {
            if self
                .rows
                .get(&target.device)
                .is_some_and(|r| r.flow == flow_id)
            {
                continue;
            }
            let generation = self.next_generation();
            let device = target.device.clone();
            let mut row = Row::new(target, flow_id, Kind::Uplink, generation, now);
            row.silence = row
                .target
                .microphone
                .is_some()
                .then(|| SilenceDetector::new(now));
            if let Some(old) = self.rows.remove(&device) {
                events.extend(old.clear_notices());
            }
            events.push(row.event("starting", None, None));
            if row.target.camera_device.is_some() {
                checks.push((device.clone(), flow_id, generation));
            }
            self.rows.insert(device, row);
        }
        (events, checks)
    }
}

/// A JPEG of at most [`MAX_PICTURE`] bytes.
fn is_picture(bytes: &[u8]) -> bool {
    bytes.len() <= MAX_PICTURE && bytes.starts_with(&JPEG_MARKER)
}

fn hash_of(bytes: &[u8]) -> u64 {
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}

fn data_url(bytes: &[u8]) -> String {
    format!(
        "data:image/jpeg;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(bytes)
    )
}

// ---------------------------------------------------------------------------
// Strom
// ---------------------------------------------------------------------------

/// Creates `flow` and starts it. True when it runs; one that does not come up is
/// removed again. A flow with its id left over (an earlier app that crashed) is
/// removed first.
async fn run_flow(strom: &StromClient, flow: &Flow) -> anyhow::Result<bool> {
    if strom.create_flow(flow).await.is_err() {
        strom.delete_flow(&flow.id).await?;
        strom.create_flow(flow).await?;
    }
    match strom.start_flow(&flow.id).await {
        Ok(true) => Ok(true),
        Ok(false) => {
            strom.delete_flow(&flow.id).await.ok();
            Ok(false)
        }
        Err(err) => {
            strom.delete_flow(&flow.id).await.ok();
            Err(err)
        }
    }
}

/// Removes a flow and asks Strom every `gone_poll` until it answers 404, at most
/// `gone_within`. False when it was still there (or Strom stopped answering): it is
/// left to the session's `prepare`, which removes this gateway's preview flows.
async fn remove_and_wait(strom: &StromClient, id: FlowId, timing: Timing) -> bool {
    if let Err(err) = strom.delete_flow(&id).await {
        tracing::warn!(flow = %id, err = %format_args!("{err:#}"), "could not remove a preview flow");
    }
    let deadline = Instant::now() + timing.gone_within;
    loop {
        match strom.get_flow(&id).await {
            Ok(None) => return true,
            Ok(Some(_)) => {}
            Err(err) => {
                tracing::warn!(flow = %id, err = %format_args!("{err:#}"), "could not ask whether a preview flow is gone");
                return false;
            }
        }
        if Instant::now() >= deadline {
            tracing::warn!(flow = %id, "a preview flow is still there after {}s; the session's prepare removes it", timing.gone_within.as_secs_f32());
            return false;
        }
        tokio::time::sleep(timing.gone_poll).await;
    }
}

/// What Strom says it opened for a flow's camera: `device-name` of its video source.
async fn opened_camera(strom: &StromClient, id: &FlowId) -> anyhow::Result<Opened> {
    Ok(match strom.element_properties(id, CAMERA_ELEMENT).await? {
        None => Opened::NotRunning,
        Some(properties) => match properties.get(DEVICE_NAME) {
            Some(PropertyValue::String(name)) => Opened::Device(name.clone()),
            _ => Opened::Unknown,
        },
    })
}

// ---------------------------------------------------------------------------
// The controller
// ---------------------------------------------------------------------------

/// Runs the [`Preview`] against the Strom the app holds: reconciles the preview flows,
/// polls the pictures, reads the levels from Strom's WebSocket, and hands the camera
/// over at Go live. Its events go to the page through [`Deliver`] once
/// [`attach`](Self::attach)ed.
pub struct PreviewController {
    settings_file: PathBuf,
    held: HeldStrom,
    host: Arc<dyn Host>,
    timing: Timing,
    model: Mutex<Preview>,
    /// A client of the held Strom, while the app holds one or a session runs on it.
    strom: Mutex<Option<StromClient>>,
    listed: Mutex<Arc<Mutex<Option<Devices>>>>,
    deliver: Mutex<Option<Deliver>>,
    /// One reconcile (or handover) at a time.
    reconciling: tokio::sync::Mutex<()>,
    wake: Arc<Notify>,
    meter: Mutex<Option<JoinHandle<()>>>,
}

impl PreviewController {
    pub fn new(settings_file: PathBuf, held: HeldStrom, host: Arc<dyn Host>) -> Self {
        PreviewController {
            settings_file,
            held,
            host,
            timing: Timing::default(),
            model: Mutex::new(Preview::default()),
            strom: Mutex::default(),
            listed: Mutex::default(),
            deliver: Mutex::default(),
            reconciling: tokio::sync::Mutex::new(()),
            wake: Arc::new(Notify::new()),
            meter: Mutex::default(),
        }
    }

    /// With removals waited for as `timing` says: shorter, in tests.
    pub fn with_timing(mut self, timing: Timing) -> Self {
        self.timing = timing;
        self
    }

    fn model(&self) -> MutexGuard<'_, Preview> {
        lock(&self.model)
    }

    fn strom(&self) -> Option<StromClient> {
        lock(&self.strom).clone()
    }

    fn listed(&self) -> Option<Devices> {
        let listed = lock(&self.listed).clone();
        let devices = lock(&listed).clone();
        devices
    }

    fn deliver(&self, events: Vec<Event>) {
        if events.is_empty() {
            return;
        }
        let deliver = lock(&self.deliver).clone();
        if let Some(deliver) = deliver {
            for event in events {
                deliver(event.script());
            }
        }
    }

    /// Sends the events to `deliver`, reads the device names from `listed`, and runs
    /// the reconcile, the pictures and the levels on `runtime` for as long as this
    /// controller lives.
    pub fn attach(
        self: &Arc<Self>,
        deliver: Deliver,
        runtime: &tokio::runtime::Handle,
        listed: Arc<Mutex<Option<Devices>>>,
    ) {
        *lock(&self.deliver) = Some(deliver);
        *lock(&self.listed) = listed;
        let task = drive(Arc::downgrade(self), self.wake.clone());
        runtime.spawn(task.with_current_subscriber());
    }

    /// Sends the events to `deliver`, with no clock running: tests drive it.
    #[cfg(test)]
    pub(crate) fn deliver_to(&self, deliver: Deliver, listed: Option<Devices>) {
        *lock(&self.deliver) = Some(deliver);
        *lock(&self.listed) = Arc::new(Mutex::new(listed));
    }

    /// Asks for a reconcile now.
    pub fn wake(&self) {
        self.wake.notify_one();
    }

    /// The page's `preview {on}`.
    pub fn set_wanted(&self, wanted: bool) {
        self.model().set_wanted(wanted);
        self.wake();
    }

    /// The window was minimised or shown again.
    pub fn set_visible(&self, visible: bool) {
        self.model().set_visible(visible);
        self.wake();
    }

    /// What `preview {on}` replies.
    pub fn available(&self) -> bool {
        self.model().available()
    }

    /// Whether the held Strom has the preview blocks, so the session's uplinks may
    /// carry the taps.
    pub fn capable(&self) -> bool {
        self.model().capable() == Some(true)
    }

    /// Looks at `gateway.pid` now.
    pub async fn look_for_other_gateway(&self) {
        let host = self.host.clone();
        let other = tokio::task::spawn_blocking(move || host.other_gateway())
            .await
            .unwrap_or(None);
        let events = self.model().set_other_gateway(other);
        self.deliver(events);
    }

    /// Holds the Strom `settings` name ([`HeldStrom::ensure`], which reclaims first),
    /// and returns a client of it. A new hold is asked once whether it has the preview
    /// blocks (`GET /api/blocks`; unreadable counts as no), and with them the meter
    /// WebSocket is opened.
    pub async fn hold(self: &Arc<Self>, settings: &Config) -> anyhow::Result<StromClient> {
        let taken = self.held.ensure(settings, self.host.clone()).await?;
        let client = self
            .held
            .client()
            .ok_or_else(|| anyhow::anyhow!("the Strom hold was handed over meanwhile"))?;
        if taken || self.strom().is_none() {
            let capable = match client.block_ids().await {
                Ok(ids) => flow::has_preview_blocks(&ids),
                Err(err) => {
                    tracing::warn!(err = %format_args!("{err:#}"), "could not read the local Strom's blocks: no preview");
                    false
                }
            };
            let events = {
                let mut model = self.model();
                // Rows of an earlier hold were on the Strom it released.
                let mut events = model.forget_previews();
                events.extend(model.set_capable(Some(capable)));
                events
            };
            *lock(&self.strom) = Some(client.clone());
            self.stop_meter();
            if capable {
                match strom_events::meter_readings(
                    &settings.strom.url,
                    settings.strom.api_key.as_deref(),
                ) {
                    Ok(readings) => self.read_meter(readings),
                    Err(err) => {
                        tracing::warn!(err = %format_args!("{err:#}"), "no sound levels: the local Strom's events");
                    }
                }
            }
            self.deliver(events);
        }
        Ok(client)
    }

    fn read_meter(self: &Arc<Self>, mut readings: strom_events::MeterReadings) {
        let this = Arc::downgrade(self);
        let task = tokio::spawn(
            async move {
                while let Some(reading) = readings.next().await {
                    let Some(this) = this.upgrade() else { return };
                    this.model().reading(&reading);
                }
            }
            .with_current_subscriber(),
        );
        *lock(&self.meter) = Some(task);
    }

    fn stop_meter(&self) {
        if let Some(task) = lock(&self.meter).take() {
            task.abort();
        }
    }

    /// One reconcile: the preview flows to what should run now (section 5.2).
    pub async fn reconcile(self: &Arc<Self>) {
        let _one = self.reconciling.lock().await;
        let strom = self.strom();
        // Only while there is something to pause: a held Strom, and not live (the
        // session refuses another gateway itself).
        if strom.is_some() && !self.model().live {
            self.look_for_other_gateway().await;
        }
        let settings = api::load_from(&self.settings_file).ok();
        let (gateway_id, targets) = match (&settings, &strom) {
            (Some(settings), Some(_)) => (
                settings.gateway.resolved_id(),
                targets(settings, self.listed().as_ref()),
            ),
            _ => (String::new(), Vec::new()),
        };
        let (mut plan, events) = self.model().plan(&gateway_id, targets, Instant::now());
        self.deliver(events);
        let Some(strom) = strom else { return };

        let mut removing = JoinSet::new();
        for removal in plan.remove.drain(..) {
            let (strom, timing) = (strom.clone(), self.timing);
            removing.spawn(async move {
                remove_and_wait(&strom, removal.flow, timing).await;
                removal
            });
        }
        while let Some(done) = removing.join_next().await {
            if let Ok(removal) = done {
                let events = self.model().removed(&removal);
                self.deliver(events);
            }
        }

        if plan.start.is_empty() {
            return;
        }
        // Before the first flow for a camera (or microphone): macOS asks now, so the
        // prompt comes when the row is ticked (section 4).
        let needs = plan.start.iter().fold(Needs::default(), |acc, s| Needs {
            camera: acc.camera || s.needs.camera,
            microphone: acc.microphone || s.needs.microphone,
        });
        let host = self.host.clone();
        let access = tokio::task::spawn_blocking(move || host.media_access(needs))
            .await
            .unwrap_or(Ok(()));
        if let Err(media) = access {
            tracing::info!(?media, "preview: access refused");
            let events = self.model().denied(media, &mut plan);
            self.deliver(events);
        }

        let mut starting = JoinSet::new();
        for start in plan.start {
            let strom = strom.clone();
            starting.spawn(async move {
                let result = run_flow(&strom, &start.flow).await;
                (start, result)
            });
        }
        while let Some(done) = starting.join_next().await {
            let Ok((start, result)) = done else { continue };
            let events =
                self.model()
                    .started(&start.device, start.generation, &result, Instant::now());
            self.deliver(events);
            if matches!(result, Ok(true)) {
                self.check_camera(strom.clone(), start.device, start.flow.id, start.generation);
            }
        }
    }

    /// Reads which camera Strom opened for a running flow (one retry after a second
    /// when it does not run yet) and hands it to the model.
    fn check_camera(
        self: &Arc<Self>,
        strom: StromClient,
        device: String,
        flow: FlowId,
        generation: u64,
    ) {
        let this = Arc::downgrade(self);
        let retry = self.timing.camera_retry;
        let task = async move {
            let mut opened = opened_camera(&strom, &flow).await;
            if matches!(opened, Ok(Opened::NotRunning)) {
                tokio::time::sleep(retry).await;
                opened = opened_camera(&strom, &flow).await;
            }
            let opened = match opened {
                Ok(opened) => opened,
                Err(err) => {
                    tracing::debug!(device = %device, err = %format_args!("{err:#}"), "could not read which camera Strom opened");
                    return;
                }
            };
            let Some(this) = this.upgrade() else { return };
            let events = this.model().camera_checked(&device, generation, opened);
            this.deliver(events);
        };
        tokio::spawn(task.with_current_subscriber());
    }

    /// Asks for each picture due now, each on a task of its own.
    fn poll_pictures(self: &Arc<Self>) {
        let due = self.model().due(Instant::now());
        if due.is_empty() {
            return;
        }
        let Some(strom) = self.strom() else { return };
        for (device, flow, generation) in due {
            let (this, strom) = (Arc::downgrade(self), strom.clone());
            let task = async move {
                let answer = strom.thumbnail(&flow, flow::BLOCK_PICTURE).await;
                let Some(this) = this.upgrade() else { return };
                let events = this
                    .model()
                    .pictured(&device, generation, answer, Instant::now());
                this.deliver(events);
            };
            tokio::spawn(task.with_current_subscriber());
        }
    }

    fn end_window(&self) {
        let events = self.model().window(Instant::now());
        self.deliver(events);
    }

    /// Go live, step 1 of the handover (section 4.1): every preview flow is removed and
    /// Strom asked until it answers 404 (at most 5 s each, all in parallel), and each
    /// row's picture goes to `camera_off`. No preview flow is created again until the
    /// session has ended ([`live_ended`](Self::live_ended)).
    pub async fn hand_over(self: &Arc<Self>) {
        let _one = self.reconciling.lock().await;
        let (removals, events) = self.model().hand_over();
        self.deliver(events);
        self.remove_all(removals).await;
    }

    async fn remove_all(self: &Arc<Self>, removals: Vec<Removal>) {
        let strom = self.strom();
        let mut removing = JoinSet::new();
        for removal in removals {
            let (strom, timing) = (strom.clone(), self.timing);
            removing.spawn(async move {
                if let Some(strom) = strom {
                    remove_and_wait(&strom, removal.flow, timing).await;
                }
                removal
            });
        }
        while let Some(done) = removing.join_next().await {
            if let Ok(removal) = done {
                let events = self.model().removed(&removal);
                self.deliver(events);
            }
        }
    }

    /// While live: the session's stats name the inputs whose uplink runs; their taps
    /// are read from now on.
    pub fn session_stats(self: &Arc<Self>, inputs: &[InputStats]) {
        let Ok(settings) = api::load_from(&self.settings_file) else {
            return;
        };
        let gateway_id = settings.gateway.resolved_id();
        let running: Vec<(Target, FlowId)> = targets(&settings, self.listed().as_ref())
            .into_iter()
            .filter_map(|target| {
                let flow_id = flow::flow_id(&gateway_id, &flow::input_id_for(&target.source));
                inputs
                    .iter()
                    .find(|s| s.flow_id == flow_id)
                    .filter(|s| {
                        matches!(
                            s.state,
                            InputState::Starting | InputState::Waiting | InputState::OnAir
                        )
                    })
                    .map(|_| (target, flow_id))
            })
            .collect();
        let (events, checks) = self.model().uplinks(running, Instant::now());
        self.deliver(events);
        if let Some(strom) = self.strom() {
            for (device, flow, generation) in checks {
                self.check_camera(strom.clone(), device, flow, generation);
            }
        }
    }

    /// The session has ended, or Go live was refused: back to step 2, with the preview
    /// off until the page asks again. A session that ran owned the Strom and stopped
    /// it; without a hold left there is nothing to preview on until the next one.
    pub fn live_ended(&self) {
        let events = self.model().live_ended();
        if !self.held.is_held() {
            *lock(&self.strom) = None;
            self.stop_meter();
            self.model().set_capable(None);
        }
        self.deliver(events);
    }

    /// Quitting while not live (section 4.1): the preview flows are removed, then the
    /// hold is released ([`HeldStrom::release`], which stops a Strom the app started
    /// only when it runs no other flow). Bound it with [`SHUT_DOWN_WITHIN`].
    pub async fn shut_down(self: &Arc<Self>) {
        let _one = self.reconciling.lock().await;
        let (removals, _) = self.model().shut_down();
        self.remove_all(removals).await;
        self.stop_meter();
        *lock(&self.strom) = None;
        self.held.release().await;
    }
}

/// The controller's clock: a level window every [`LEVEL_WINDOW`], the pictures due,
/// and a reconcile every [`RECONCILE_EVERY`] or when woken. Ends with the controller.
async fn drive(this: Weak<PreviewController>, wake: Arc<Notify>) {
    let mut tick = tokio::time::interval(LEVEL_WINDOW);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last_reconcile = Instant::now();
    loop {
        let woken = tokio::select! {
            _ = tick.tick() => false,
            _ = wake.notified() => true,
        };
        let Some(this) = this.upgrade() else { return };
        if woken || last_reconcile.elapsed() >= RECONCILE_EVERY {
            last_reconcile = Instant::now();
            let reconciling = this.clone();
            tokio::spawn(async move { reconciling.reconcile().await }.with_current_subscriber());
        }
        if !woken {
            this.end_window();
            this.poll_pictures();
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests;
