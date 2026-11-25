//! The media seam: what a session asks of whatever does the media.
//!
//! The session (`run::up`: the gate, source registration and adoption, the heartbeat
//! and the teardown) never touches media. It needs to know which inputs there are,
//! to start an input's uplink to its endpoint, to stop and restart it, whether it
//! runs, and its SRT counters. [`Media`] is exactly that.
//!
//! On a computer [`StromMedia`] does it through the local Strom, as the gateway always
//! has: flows by derived id, Strom's device list, and the single-instance pidfile.
//! On a phone there is no Strom: the app's own code captures, encodes, muxes and
//! sends, and [`ExternalMedia`] hands each start, stop and restart to it as a
//! [`MediaRequest`] and answers from what it reports. Either way the library owns no
//! media. Flow ids and every other Strom concept stay inside [`StromMedia`].

use crate::api::StromHold;
use crate::config::{Config, Video};
use crate::devices;
use crate::flow::{self, Input};
use crate::gateway::{Event, Events};
use crate::local_strom::LocalStrom;
use crate::run::{self, AlreadyRunning, Pidfile};
use crate::session::Inputs;
use crate::strom::{self as strom_api, StromClient};
use anyhow::{bail, Context, Result};
use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
pub use strom_types::api::SrtCallerStats;
use strom_types::discovery::{DeviceCategory, DeviceResponse};
use strom_types::FlowId;
use tokio::sync::{mpsc, Notify};
use tracing::warn;

/// What the session asks of whatever does the media. Not object-safe on purpose:
/// `run::up` is generic over it, so no `async-trait` and no boxing. Each method
/// returns `impl Future<Output = ..> + Send` rather than being an `async fn`: a bare
/// `async fn` in a `pub trait` trips rustc's `async_fn_in_trait` lint, which CI's
/// `cargo clippy --all-targets -- -D warnings` makes an error, and its future has no
/// `Send` bound, so `Session::start_inner`'s `tokio::spawn` of `run::up` would not
/// compile.
pub trait Media: Send + Sync {
    /// Once, before any input: Strom's version for the heartbeat, `None` on the phone.
    fn version(&self) -> impl Future<Output = Option<String>> + Send;

    /// Once, before any port is asked for: which cameras go live (or the test
    /// pattern) and the camera -> audio pairing. Warnings (a saved device not
    /// connected) go out as `events`.
    fn resolve_inputs(
        &self,
        cfg: &Config,
        inputs: Inputs,
        events: &Events,
    ) -> impl Future<Output = Result<ResolvedInputs>> + Send;

    /// Once, after the inputs are known (resolved, ports from the cloud,
    /// `inputs_for`) and before any is started: clears what would collide with them.
    fn prepare(&self, inputs: &[Input], events: &Events)
        -> impl Future<Output = Result<()>> + Send;

    /// Starts the input's uplink: capture, encode, MPEG-TS, SRT caller to
    /// `input.endpoint.venue_uri()`.
    fn start(&self, input: &Input, video: &Video) -> impl Future<Output = Result<()>> + Send;

    /// Stops and forgets it. Stopping one that does not run is not an error.
    fn stop(&self, input: &Input) -> impl Future<Output = Result<()>> + Send;

    /// Starts again an uplink that exists, running (the stall restart) or not.
    /// Whether it reports running afterwards.
    fn restart(&self, input: &Input) -> impl Future<Output = Result<bool>> + Send;

    /// Whether it runs: `None` when it does not exist (never started, or gone),
    /// `Some(false)` when it exists but does not run.
    fn running(&self, input: &Input) -> impl Future<Output = Result<Option<bool>>> + Send;

    /// The SRT sender's cumulative counters, `None` before a peer exists.
    fn uplink(&self, input: &Input) -> impl Future<Output = Result<Option<SrtCallerStats>>> + Send;

    /// Whether it can send through an SRT relay: dial its address with a stream id
    /// (`input.endpoint.venue_uri()` does that). Only then does `caller` mode go
    /// through a relay Open Live reports. Yes on a computer, where Strom dials.
    fn dials_relay(&self) -> bool {
        true
    }

    /// Whether the media is held back for now, whatever the productions say: an
    /// armed session then stops each uplink (its source kept, marked inactive) as
    /// though no production holding it were live, and starts it again at the first
    /// look after the hold ends. A phone in the background or in a call, whose camera
    /// the OS has taken. Never on a computer.
    fn held_back(&self) -> bool {
        false
    }

    /// Resolves when the hold may have changed ([`Media::held_back`]): an armed
    /// session then looks at the productions at once rather than at its next poll,
    /// and marks a source whose uplink the hold stopped inactive at once, so a phone
    /// going to the background is done within the 4 s iOS gives it. Never resolves
    /// on a computer.
    fn hold_changed(&self) -> impl Future<Output = ()> + Send {
        std::future::pending()
    }
}

/// The inputs of a run once the devices are chosen: what `devices::inputs_for` turns
/// into inputs with ports.
#[derive(Debug, Clone, Default)]
pub struct ResolvedInputs {
    /// The cameras to send, sorted by name, so ports are allocated in name order.
    pub cameras: Vec<DeviceResponse>,
    /// Whether the test pattern is one of the inputs.
    pub test_pattern: bool,
    /// Camera device id -> audio source id.
    pub audio: BTreeMap<String, String>,
}

/// The media done by the local Strom, as the gateway on a computer always has. Holds
/// the single-instance pidfile and the Strom it adopted or started for as long as it
/// lives: dropping it stops a Strom it started, then removes the pidfile.
pub struct StromMedia {
    strom: StromClient,
    gateway_id: String,
    version: Option<String>,
    // Dropped in this order: the Strom first, the pidfile after it.
    _local: LocalStrom,
    _pidfile: Pidfile,
}

impl StromMedia {
    /// Refuses a second live gateway on this machine ([`AlreadyRunning`]), records
    /// this one in the pidfile, then adopts the local Strom or starts one, and reads
    /// its version.
    pub async fn start(cfg: &Config, events: &Events) -> Result<Self> {
        Self::start_on(cfg, None, events).await
    }

    /// [`start`](Self::start), over `held` when given: the Strom an app took before
    /// going live ([`crate::api::hold_strom`]) is used instead of adopting or starting
    /// one, and is owned from here on, so dropping this stops it if it was started
    /// for the hold, exactly as for a Strom started here. Refused like `start` when
    /// another gateway is live, and the hold is then released
    /// ([`StromHold::release`]), never just dropped: that gateway may use it.
    pub async fn start_on(cfg: &Config, held: Option<StromHold>, events: &Events) -> Result<Self> {
        // One live session per settings file: a second loop over the same flows and
        // sources would fight the first for them.
        if let Some(pid) = run::running_gateway() {
            if let Some(held) = held {
                held.release().await;
            }
            return Err(AlreadyRunning { pid }.into());
        }
        // Recorded at once, not once the inputs are live: a gateway that is still
        // starting (Strom's readiness wait, registration) is a running gateway too. A
        // second `up` or the app's Go live refuses it, and the app's Strom reclaim
        // never takes its Strom for one left behind (#95). Removed again when this
        // drops, however the session ends.
        let pidfile = Pidfile::write(events);
        let (local, strom, version) = match held {
            Some(held) => {
                if !held.serves(cfg) {
                    bail!("the Strom held for this session is not the one the settings name ([strom] url, api_key or manage changed since it was taken)");
                }
                let (mut local, strom, version) = held.into_parts();
                // Its stop is the session's to report now.
                local.report_to(events.clone());
                (local, strom, version)
            }
            None => {
                let (local, strom) = run::connect_strom(cfg, events).await?;
                // Asked once, here, whether or not the heartbeat is on; the heartbeat
                // reuses it.
                let version = strom.version().await.ok();
                (local, strom, version)
            }
        };
        // An older Strom without the version route is warned about, never refused.
        if let Some(warning) =
            strom_api::version_warning(strom_api::PINNED_VERSION, version.as_deref())
        {
            warn!("{warning}");
            events.emit(Event::Warning { message: warning });
        }
        Ok(Self {
            strom,
            gateway_id: cfg.gateway.resolved_id(),
            version,
            _local: local,
            _pidfile: pidfile,
        })
    }

    /// Over a Strom someone else keeps running, with no pidfile: for the tests that
    /// drive the session's poll against a fake Strom.
    #[cfg(test)]
    pub(crate) fn over(strom: &StromClient, gateway_id: &str) -> Self {
        Self {
            strom: strom.clone(),
            gateway_id: gateway_id.to_string(),
            version: None,
            _local: LocalStrom::Adopted,
            _pidfile: Pidfile::none(),
        }
    }

    fn flow_id(&self, input: &Input) -> FlowId {
        flow::flow_id(&self.gateway_id, &input.id)
    }
}

impl Media for StromMedia {
    async fn version(&self) -> Option<String> {
        self.version.clone()
    }

    async fn resolve_inputs(
        &self,
        cfg: &Config,
        inputs: Inputs,
        events: &Events,
    ) -> Result<ResolvedInputs> {
        let Inputs {
            include_virtual,
            selection,
            audio: audio_pairings,
            test_pattern,
        } = inputs;
        // A saved `[[input]]` selection says what to send, unless a flag picks for this run.
        let saved =
            !test_pattern && !include_virtual && selection.is_none() && !cfg.inputs.is_empty();
        let (chosen, test_pattern) = if test_pattern {
            (Vec::new(), true)
        } else {
            let all = self
                .strom
                .video_devices()
                .await
                .context("asking Strom what capture devices it can see")?;
            if saved {
                let picked = devices::choose_saved(&all, &cfg.inputs)?;
                for device in &picked.absent {
                    let message = format!(
                        "[[input]] device {device:?} is not connected; it is skipped, the others go live"
                    );
                    warn!("{message}");
                    events.emit(Event::Warning { message });
                }
                (picked.cameras, picked.test_pattern)
            } else {
                (
                    devices::choose(all, include_virtual, selection.as_deref())?,
                    false,
                )
            }
        };
        if chosen.is_empty() && !test_pattern {
            if saved {
                bail!("None of the devices in the settings' [[input]] entries is connected. `open-live-ingest devices` lists what Strom can see; --devices or --all picks for one run.");
            }
            bail!("Strom reports no capture devices. Virtual ones are skipped unless --all; --test streams a test pattern instead.");
        }

        // Resolved and checked here, before any port is asked for or any flow exists.
        let pairings = if !audio_pairings.is_empty() || chosen.is_empty() {
            audio_pairings
        } else {
            let (pairings, absent) = devices::settings_pairings(&cfg.inputs, &chosen);
            // A saved selection has said so already, as not connected.
            for device in absent.into_iter().filter(|_| !saved) {
                warn!("[[input]] pairs audio with {device:?}, which is not among this run's cameras; skipped");
                events.emit(Event::Warning {
                    message: format!("[[input]] pairs audio with {device:?}, which is not among this run's cameras; skipped"),
                });
            }
            pairings
        };
        let audio = if pairings.is_empty() {
            BTreeMap::new()
        } else {
            let sources = self
                .strom
                .audio_devices()
                .await
                .context("asking Strom what audio sources it can see")?;
            devices::pair_audio(&chosen, &sources, &pairings)?
        };
        Ok(ResolvedInputs {
            cameras: chosen,
            test_pattern,
            audio,
        })
    }

    /// Removes our own leftovers from a run that ended without cleanup, and any other
    /// flow already holding one of the cameras about to be opened. Two pipelines on one
    /// device contend for frames, which presents as stutter rather than as an error.
    async fn prepare(&self, inputs: &[Input], events: &Events) -> Result<()> {
        let devices: Vec<&str> = inputs
            .iter()
            .filter_map(|i| match &i.source {
                flow::Source::Device { device_id, .. } => Some(device_id.as_str()),
                flow::Source::Test { .. } => None,
            })
            .collect();
        for existing in self
            .strom
            .list_flows()
            .await
            .context("listing Strom's flows")?
        {
            let ours = flow::is_ours(&existing, &self.gateway_id);
            let holds_device =
                flow::capture_device_of(&existing).is_some_and(|d| devices.contains(&d));
            if !(ours || holds_device) {
                continue;
            }
            run::remove_flow(&self.strom, &existing.id).await?;
            events.emit(Event::ConflictingFlowRemoved {
                name: existing.name.clone(),
                ours,
            });
        }
        Ok(())
    }

    /// Creates the input's flow and starts it; a flow that does not come up is
    /// removed again.
    async fn start(&self, input: &Input, video: &Video) -> Result<()> {
        let flow_id = self.flow_id(input);
        let desired = flow::build(&self.gateway_id, input, video);
        self.strom.create_flow(&desired).await?;
        match self.strom.start_flow(&flow_id).await {
            Ok(true) => Ok(()),
            Ok(false) => {
                self.strom.delete_flow(&flow_id).await.ok();
                bail!("Strom did not report the flow running after start")
            }
            Err(err) => {
                self.strom.delete_flow(&flow_id).await.ok();
                Err(err)
            }
        }
    }

    async fn stop(&self, input: &Input) -> Result<()> {
        run::remove_flow(&self.strom, &self.flow_id(input)).await
    }

    /// `stop_flow` + `start_flow`. Strom takes a stop of a flow that does not run as
    /// done, so this also starts one that is there but stopped.
    async fn restart(&self, input: &Input) -> Result<bool> {
        let flow_id = self.flow_id(input);
        self.strom.stop_flow(&flow_id).await?;
        self.strom.start_flow(&flow_id).await
    }

    async fn running(&self, input: &Input) -> Result<Option<bool>> {
        Ok(self
            .strom
            .get_flow(&self.flow_id(input))
            .await?
            .map(|flow| flow.running))
    }

    async fn uplink(&self, input: &Input) -> Result<Option<SrtCallerStats>> {
        self.strom.srt_uplink(&self.flow_id(input)).await
    }
}

/// What [`ExternalMedia`] asks of the host that does the media.
// A handful per session, each moved once: boxing `Start` would only make it clumsier.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub enum MediaRequest {
    /// Start sending `input`: capture `input.source` (on a phone, `device` is the
    /// AVFoundation unique id of the lens and `audio_device` the microphone's port
    /// UID), encode as `video` says, MPEG-TS over SRT to `input.endpoint`
    /// (`venue_uri()`, or its parts: `cloud_host`, `port`, `latency_ms`,
    /// `passphrase()`, `pbkeylen()`, and through a relay the stream id of
    /// `relay`, set on the socket before it calls). Then report with
    /// [`ExternalMedia::report`].
    Start { input: Input, video: Video },
    /// Stop sending the input with this id, and forget it.
    Stop { input_id: String },
    /// Close the input's SRT connection and connect again: it delivers nothing
    /// (the stall restart). Counters reported afterwards start from zero.
    Restart { input_id: String },
}

/// The last the host reported for one started input.
#[derive(Debug, Clone)]
struct Reported {
    running: bool,
    uplink: Option<SrtCallerStats>,
}

/// The media done by the host: the iOS app's own capture, encode, mux and SRT. Each
/// start, stop and restart goes to the host as a [`MediaRequest`] on the channel
/// [`ExternalMedia::new`] returns; whether an input runs and its counters are the
/// last the host reported with [`ExternalMedia::report`]. No Strom, no pidfile.
///
/// Cheap to clone: the clones share the reports, so the host keeps one to report on
/// while the session holds another.
#[derive(Debug, Clone)]
pub struct ExternalMedia {
    requests: mpsc::UnboundedSender<MediaRequest>,
    reported: Arc<Mutex<HashMap<String, Reported>>>,
    held: Arc<AtomicBool>,
    /// Told on each change of `held`, for [`Media::hold_changed`].
    hold_changed: Arc<Notify>,
}

impl ExternalMedia {
    /// The backend, and the receiver of its requests for the host.
    pub fn new() -> (Self, mpsc::UnboundedReceiver<MediaRequest>) {
        let (requests, rx) = mpsc::unbounded_channel();
        (
            Self {
                requests,
                reported: Arc::default(),
                held: Arc::default(),
                hold_changed: Arc::default(),
            },
            rx,
        )
    }

    /// What the host knows about an input it was asked to start: whether it sends,
    /// and its SRT sender's cumulative counters (libsrt's `srt_bstats`), `None`
    /// while it has no peer. A report for an input that is not started (never, or
    /// stopped since) is ignored, so a late one cannot bring it back.
    pub fn report(&self, input_id: &str, running: bool, uplink: Option<SrtCallerStats>) {
        if let Some(r) = self.reports().get_mut(input_id) {
            *r = Reported { running, uplink };
        }
    }

    /// Holds the media back, or lets it go again: the host cannot send for now (the
    /// app in the background, a phone call). An armed session stops each uplink at
    /// its next look at the productions, its source kept and marked inactive, and
    /// starts it again at the first look after the hold ends if a production holding
    /// the source is still live. See [`Media::held_back`].
    /// A change wakes the session at once ([`Media::hold_changed`]); the same value
    /// again changes nothing.
    pub fn hold(&self, held: bool) {
        if self.held.swap(held, Ordering::SeqCst) != held {
            self.hold_changed.notify_one();
        }
    }

    fn reports(&self) -> std::sync::MutexGuard<'_, HashMap<String, Reported>> {
        // A panic elsewhere while holding it leaves the map as consistent as ever.
        self.reported.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn ask(&self, request: MediaRequest) -> Result<()> {
        self.requests
            .send(request)
            .map_err(|_| anyhow::anyhow!("the app no longer takes media requests"))
    }

    /// Started, and assumed running until the host reports otherwise, with no
    /// counters yet: the state Strom reports for a flow it has just started.
    fn started(&self, input_id: &str) {
        self.reports().insert(
            input_id.to_string(),
            Reported {
                running: true,
                uplink: None,
            },
        );
    }
}

impl Media for ExternalMedia {
    async fn version(&self) -> Option<String> {
        None
    }

    /// The settings' `[[input]]` entries as the app wrote them, one camera each:
    /// `device` the AVFoundation unique id of the lens, `audio` the microphone's
    /// port UID, `label` its name. Nothing is looked up. The CLI's device flags do
    /// not apply; the test pattern does.
    async fn resolve_inputs(
        &self,
        cfg: &Config,
        inputs: Inputs,
        _events: &Events,
    ) -> Result<ResolvedInputs> {
        if inputs.include_virtual || inputs.selection.is_some() || !inputs.audio.is_empty() {
            bail!("the app picks the camera and microphone in the settings' [[input]] entries; device selection and audio pairing flags do not apply");
        }
        if inputs.test_pattern {
            return Ok(ResolvedInputs {
                test_pattern: true,
                ..ResolvedInputs::default()
            });
        }
        let mut resolved = ResolvedInputs::default();
        for entry in &cfg.inputs {
            if entry.is_test_pattern() {
                resolved.test_pattern = true;
                continue;
            }
            let id = entry.device.trim();
            if id.is_empty() {
                bail!("an [[input]] entry names no device");
            }
            if resolved.cameras.iter().any(|c| c.id == id) {
                bail!("two [[input]] entries name {id:?}; one entry per input");
            }
            resolved.cameras.push(DeviceResponse {
                id: id.to_string(),
                name: entry.label().unwrap_or(id).to_string(),
                device_class: "Video/Source".to_string(),
                category: DeviceCategory::VideoSource,
                provider: "app".to_string(),
                properties: Default::default(),
                first_seen_secs_ago: 0,
                last_seen_secs_ago: 0,
            });
            if let Some(audio) = entry.audio.as_deref().map(str::trim) {
                if !audio.is_empty() {
                    resolved.audio.insert(id.to_string(), audio.to_string());
                }
            }
        }
        if resolved.cameras.is_empty() && !resolved.test_pattern {
            bail!("the settings name no [[input]] to send");
        }
        resolved.cameras.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(resolved)
    }

    async fn prepare(&self, _inputs: &[Input], _events: &Events) -> Result<()> {
        Ok(())
    }

    async fn start(&self, input: &Input, video: &Video) -> Result<()> {
        self.ask(MediaRequest::Start {
            input: input.clone(),
            video: video.clone(),
        })?;
        self.started(&input.id);
        Ok(())
    }

    async fn stop(&self, input: &Input) -> Result<()> {
        self.reports().remove(&input.id);
        // A host that is gone sends nothing any more: stopped.
        let _ = self.ask(MediaRequest::Stop {
            input_id: input.id.clone(),
        });
        Ok(())
    }

    async fn restart(&self, input: &Input) -> Result<bool> {
        self.ask(MediaRequest::Restart {
            input_id: input.id.clone(),
        })?;
        self.started(&input.id);
        Ok(true)
    }

    async fn running(&self, input: &Input) -> Result<Option<bool>> {
        Ok(self.reports().get(&input.id).map(|r| r.running))
    }

    async fn uplink(&self, input: &Input) -> Result<Option<SrtCallerStats>> {
        Ok(self.reports().get(&input.id).and_then(|r| r.uplink.clone()))
    }

    /// Yes: `Start` hands the host the relay's host, port and stream id
    /// (`input.endpoint`), and the phone's SRT sender sets the stream id on its
    /// socket before it calls.
    fn dials_relay(&self) -> bool {
        true
    }

    fn held_back(&self) -> bool {
        self.held.load(Ordering::SeqCst)
    }

    async fn hold_changed(&self) {
        self.hold_changed.notified().await;
    }
}
