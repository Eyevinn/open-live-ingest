//! The commands: bring inputs up and keep them registered, tear them down, report.
//!
//! `up` runs in the foreground and owns what it started, so a deliberate stop stops
//! the feeds. It ignores SIGHUP: a dropped SSH connection must not take a venue off
//! air, and that is the difference between logging out and pulling a plug.
//!
//! `down` and `status` keep no record of what `up` did. Flows are recognised by their
//! derived ids and sources by the gateway's name prefix, so both commands behave the
//! same whether `up` is running, finished, or was killed outright. The pidfile is only
//! there so `down` can ask a running `up` to stop first.
//!
//! Nothing here prints, reads the terminal, or handles a signal. Progress goes out as
//! [`Event`]s, reports come back as values, and `up` is told when to stop by a future
//! its caller resolves: the CLI on Ctrl-C or SIGTERM, an app on its Stop button.

use crate::config::{self, format_ports, mask_passphrase, Config, UplinkMode, Video};
use crate::devices;
use crate::flow::{self, Input};
use crate::gateway::{Event, Events, InputState as State, InputStats, SummaryRow, UplinkStats};
use crate::heartbeat::{self, Heartbeat};
use crate::local_strom::{self, LocalStrom};
use crate::openlive::{
    drifted, listener_port, OpenLiveClient, Refused, ServerInfo, Source, SourcePayload,
};
use crate::strom::{self as strom_api, StromClient};
use anyhow::{bail, Context, Result};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::future::Future;
use std::ops::RangeInclusive;
use std::time::Duration;
use strom_types::api::SrtCallerStats;
use strom_types::discovery::DeviceResponse;
use strom_types::FlowId;
use tracing::{info, warn};

const TICK: Duration = Duration::from_secs(10);

/// Quiet polls before an uplink that was delivering is reported inactive.
/// Hysteresis: every status flip rewrites the Open Live source, and CouchDB keeps a
/// revision per write, so one quiet poll must not flap the verdict.
const QUIET_TOLERANCE: u32 = 3;

/// The local Strom, adopted or started, and a client for it. Held for as long as the
/// command runs: dropping it stops a Strom we started.
async fn connect_strom(cfg: &Config, events: &Events) -> Result<(LocalStrom, StromClient)> {
    let local = local_strom::ensure_running(cfg, events).await?;
    Ok((
        local,
        StromClient::new(&cfg.strom.url, cfg.strom.api_key.as_deref())?,
    ))
}

fn open_live_client(cfg: &Config) -> Result<Option<OpenLiveClient>> {
    if !cfg.open_live.register {
        return Ok(None);
    }
    let url = cfg
        .open_live
        .url
        .as_deref()
        .context("open_live.url is unset")?;
    Ok(Some(OpenLiveClient::new(
        url,
        cfg.open_live.auth_mode,
        cfg.open_live.api_key.as_deref(),
    )?))
}

/// The gateway id to tag registered sources with, so Open Live's forget-gateway
/// cascade removes the right sources and Studio's Sources chip can attribute them.
/// Present only when the heartbeat is configured; otherwise sources go untagged.
fn source_gateway_id(cfg: &Config) -> Result<Option<String>> {
    Ok(heartbeat::Target::from_config(&cfg.open_live)?.map(|t| t.gateway_id))
}

/// Where feeds are sent, and which ports the links use.
struct Cloud {
    host: String,
    ports: BTreeSet<u16>,
    port_source: PortSource,
}

/// Who chose the port range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PortSource {
    /// Published by Open Live: the range its Strom has leased for callers.
    OpenLive,
    /// `uplink.port_range` in the settings.
    Settings,
}

impl fmt::Display for PortSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            PortSource::OpenLive => "Open Live",
            PortSource::Settings => "the settings",
        })
    }
}

/// Resolves the cloud host, configured or asked of Open Live, and the port range.
/// Open Live is asked whenever it can be and something is needed from it: the host,
/// or in caller mode its Strom's ports. A configured host together with a configured
/// range still starts when Open Live is down, as it always has; the sources are
/// registered once it is back.
async fn resolve_cloud(
    cfg: &Config,
    open_live: Option<&OpenLiveClient>,
    events: &Events,
) -> Result<Cloud> {
    let configured_host = cfg
        .uplink
        .host
        .as_deref()
        .map(str::trim)
        .filter(|h| !h.is_empty());
    let local = cfg.uplink.local_ports()?;
    let needs_open_live = configured_host.is_none() || cfg.uplink.mode == UplinkMode::Caller;
    let info: Option<ServerInfo> = match open_live {
        Some(client) if needs_open_live => match client.server_info().await {
            Ok(info) => info,
            Err(err) if configured_host.is_some() && local.is_some() => {
                warn!(err = %format_args!("{err:#}"), "could not ask Open Live for its server info; using uplink.host and uplink.port_range from the settings");
                events.emit(Event::Warning {
                    message: format!("could not ask Open Live for its server info ({err:#}); using uplink.host and uplink.port_range from the settings"),
                });
                None
            }
            Err(err) => return Err(err.context("asking Open Live where its Strom is")),
        },
        _ => None,
    };

    let host = match configured_host {
        Some(host) => host.to_string(),
        None => {
            open_live.context(
                "no cloud Strom host configured, and registration is off so there is nobody to ask",
            )?;
            info.as_ref()
                .and_then(|i| i.strom_host.clone())
                .context("Open Live did not report a Strom host; set uplink.host")?
        }
    };
    let (ports, port_source) = choose_ports(
        cfg.uplink.mode,
        local,
        info.as_ref().and_then(|i| i.srt_ports.clone()),
        info.as_ref().and_then(|i| i.srt_port_state.as_deref()),
        events,
    )?;
    Ok(Cloud {
        host,
        ports,
        port_source,
    })
}

/// Which range the links use. In caller mode the ports are the cloud Strom's, and
/// several venues share that Strom, so the range Open Live publishes wins over
/// anything in the settings; the settings are only a fallback for an Open Live that
/// publishes none. In listener mode the ports are this machine's own, which only the
/// settings can know.
fn choose_ports(
    mode: UplinkMode,
    local: Option<RangeInclusive<u16>>,
    published: Option<BTreeSet<u16>>,
    lease_status: Option<&str>,
    events: &Events,
) -> Result<(BTreeSet<u16>, PortSource)> {
    let local = local.map(|range| range.collect::<BTreeSet<u16>>());
    match mode {
        UplinkMode::Listener => local.map(|range| (range, PortSource::Settings)).context(
            "uplink.mode is \"listener\", so uplink.port_range must name this machine's SRT ports",
        ),
        UplinkMode::Caller => match (published, local) {
            (Some(published), Some(local)) => {
                if local != published {
                    warn!(
                        settings = %format_ports(&local),
                        open_live = %format_ports(&published),
                        "uplink.port_range is ignored: in caller mode the cloud Strom owns its ports, and Open Live publishes its range"
                    );
                    events.emit(Event::Warning {
                        message: format!(
                            "uplink.port_range ({}) is ignored: in caller mode the cloud Strom owns its ports, and Open Live publishes {}",
                            format_ports(&local),
                            format_ports(&published)
                        ),
                    });
                }
                Ok((published, PortSource::OpenLive))
            }
            (Some(published), None) => Ok((published, PortSource::OpenLive)),
            (None, Some(local)) => Ok((local, PortSource::Settings)),
            (None, None) => {
                let why = match lease_status {
                    Some("pending") => "Open Live is still waiting for its SRT ports from Strom (it retries every minute); wait and try again",
                    Some("unsupported") => "Open Live's Strom hands out no SRT ports: it has no port pool configured (STROM_PORTS), or is too old to have one",
                    Some("disabled") => "Open Live has SRT port reservation disabled",
                    _ => "Open Live publishes no SRT ports; upgrade it, or turn registration on",
                };
                bail!("no SRT port range for the cloud Strom: {why}, or set uplink.port_range in the settings")
            }
        },
    }
}

/// Lists what Strom can see, so an operator can pick before starting anything:
/// video sources first, then audio sources, each sorted by name.
pub async fn devices(cfg: &Config, events: &Events) -> Result<Vec<DeviceReport>> {
    // Started only to ask, if started at all; dropped again on the way out.
    let (_local, strom) = connect_strom(cfg, events).await?;
    let video = strom
        .video_devices()
        .await
        .context("asking Strom what capture devices it can see")?;
    let audio = strom
        .audio_devices()
        .await
        .context("asking Strom what audio sources it can see")?;
    Ok(device_report(video, audio))
}

/// Video sources first, then audio sources, each sorted by name.
fn device_report(
    mut video: Vec<DeviceResponse>,
    mut audio: Vec<DeviceResponse>,
) -> Vec<DeviceReport> {
    video.sort_by(|a, b| a.name.cmp(&b.name));
    audio.sort_by(|a, b| a.name.cmp(&b.name));
    let video = video.iter().map(|d| DeviceReport {
        name: d.name.clone(),
        id: d.id.clone(),
        kind: DeviceKind::Video,
        virtual_: devices::is_probably_virtual(d),
    });
    let audio = audio.iter().map(|d| DeviceReport {
        name: d.name.clone(),
        id: d.id.clone(),
        kind: DeviceKind::Audio,
        // `up` only ever skips cameras; an audio source is used when it is named.
        virtual_: false,
    });
    video.chain(audio).collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DeviceKind {
    Video,
    Audio,
}

/// One capture device, as `devices` reports it.
#[derive(Debug, Serialize)]
pub struct DeviceReport {
    pub name: String,
    pub id: String,
    pub kind: DeviceKind,
    /// Skipped by `up` unless `--all`: it produces nothing on its own.
    #[serde(rename = "virtual")]
    pub virtual_: bool,
}

/// The `devices` table, as the CLI prints it.
pub fn render_devices(all: &[DeviceReport]) -> String {
    let (video, audio): (Vec<&DeviceReport>, Vec<&DeviceReport>) =
        all.iter().partition(|d| d.kind == DeviceKind::Video);
    let mut out = if video.is_empty() {
        "Strom reports no video sources on this machine.\n".to_string()
    } else {
        let mut out = format!("{:<30} {:<24} NOTE\n", "DEVICE", "ID");
        for device in &video {
            let note = if device.virtual_ {
                "virtual, skipped unless --all"
            } else {
                ""
            };
            out += &format!(
                "{:<30} {:<24} {note}\n",
                truncate(&device.name, 30),
                truncate(&device.id, 24)
            );
        }
        out += "\n`up` starts all of these except the virtual ones; `up --devices \"FaceTime\"` picks by name or id.\n";
        out
    };
    if !audio.is_empty() {
        out += &format!("\n{:<30} ID\n", "AUDIO SOURCE");
        for device in &audio {
            out += &format!(
                "{:<30} {}\n",
                truncate(&device.name, 30),
                truncate(&device.id, 24)
            );
        }
        out += "\nNo audio is sent unless paired: `up --audio \"FaceTime=USB Microphone\"` sends one with a camera.\n";
    }
    out
}

/// One streaming input and what the last poll found out about it.
struct Live {
    input: Input,
    flow_id: FlowId,
    state: State,
    shown: Option<State>,
    last_bytes: Option<u64>,
    quiet_polls: u32,
    source_id: Option<String>,
    /// The `connect.warning` Open Live last reported for the source, so one that
    /// persists is logged once per run rather than every tick.
    connect_warning: Option<String>,
    /// The last SRT sample with a peer, for the heartbeat. None while nothing receives.
    uplink: Option<SrtCallerStats>,
}

// `Unreachable` is not evidence that the feed stopped: the source's status is left
// as it was.
impl State {
    /// What Strom would say about the flow, for the heartbeat. `Unreachable` cannot
    /// be confirmed running and is reported as idle: a venue's inputs all dropping to
    /// idle while its heartbeat keeps arriving is a true signal that the box has lost
    /// its Strom, and the source status is what carries the benefit of the doubt.
    fn flow_state(&self) -> heartbeat::FlowState {
        match self {
            State::Starting | State::Waiting | State::OnAir => heartbeat::FlowState::Playing,
            State::Unreachable | State::Failed { .. } => heartbeat::FlowState::Idle,
        }
    }
}

/// The heartbeat to Open Live and the facts about this box that do not change
/// between ticks.
struct Reporter {
    heartbeat: Heartbeat,
    host: String,
    strom_version: Option<String>,
}

impl Reporter {
    /// Starts the heartbeat when the settings name a gateway. `strom_version` is the
    /// one `up` read at startup; an older Strom without the route goes unversioned.
    fn start(
        cfg: &Config,
        strom_version: Option<String>,
        live: &[Live],
        events: &Events,
    ) -> Result<Option<Self>> {
        let Some(target) = heartbeat::Target::from_config(&cfg.open_live)? else {
            return Ok(None);
        };
        let host = config::hostname().unwrap_or_else(|| cfg.gateway.resolved_id());
        events.emit(Event::HeartbeatStarted {
            gateway_id: target.gateway_id.clone(),
        });
        let initial = snapshot_of(live, &host, strom_version.as_deref());
        Ok(Some(Self {
            heartbeat: Heartbeat::start(target, initial),
            host,
            strom_version,
        }))
    }

    fn publish(&self, live: &[Live]) {
        self.heartbeat
            .update(snapshot_of(live, &self.host, self.strom_version.as_deref()));
    }

    async fn stop(self) {
        self.heartbeat.stop().await;
    }
}

/// What the last tick found out, in the shape Open Live stores. Pure, so the mapping
/// from the loop's states to the wire can be tested without a socket.
fn snapshot_of(live: &[Live], host: &str, strom_version: Option<&str>) -> heartbeat::Snapshot {
    heartbeat::Snapshot {
        host: host.to_string(),
        strom_version: strom_version.map(str::to_string),
        ingest_version: env!("CARGO_PKG_VERSION").to_string(),
        device_count: live.len(),
        streaming_count: live.iter().filter(|l| l.state == State::OnAir).count(),
        inputs: live
            .iter()
            .map(|l| heartbeat::InputStatus {
                input_id: l.input.id.clone(),
                name: l.input.name.clone(),
                flow_state: l.state.flow_state(),
                source_id: l.source_id.clone(),
                uplink: l.uplink.as_ref().map(heartbeat::Uplink::from_srt),
            })
            .collect(),
    }
}

/// Each input's state and uplink, for [`Event::Stats`]: the states and SRT samples
/// [`snapshot_of`] hands the heartbeat, with the uplink converted the same way.
fn stats_of(live: &[Live]) -> Event {
    Event::Stats {
        inputs: live
            .iter()
            .map(|l| InputStats {
                input: l.input.name.clone(),
                state: l.state.clone(),
                uplink: l.uplink.as_ref().map(|sample| {
                    let uplink = heartbeat::Uplink::from_srt(sample);
                    UplinkStats {
                        bitrate_kbps: uplink.bitrate_kbps,
                        rtt_ms: uplink.rtt_ms,
                        dropped: uplink.dropped,
                    }
                }),
            })
            .collect(),
    }
}

/// Brings every chosen device up and keeps it registered until `stop` resolves, then
/// removes what it started. `stop` says why it is stopping, which decides how long
/// the teardown may take.
pub async fn up(
    cfg: Config,
    include_virtual: bool,
    selection: Option<String>,
    audio_pairings: Vec<devices::AudioPairing>,
    test_pattern: bool,
    events: &Events,
    stop: impl Future<Output = Stop>,
) -> Result<()> {
    if test_pattern && !audio_pairings.is_empty() {
        bail!("--audio pairs an audio source with a camera, and --test streams no camera; the test pattern carries its own tone");
    }
    let gateway_id = cfg.gateway.resolved_id();
    let source_gateway_id = source_gateway_id(&cfg)?;
    let (local_strom, strom) = connect_strom(&cfg, events).await?;
    // Asked once, here, whether or not the heartbeat is on; the heartbeat reuses it.
    // An older Strom without the route is warned about, never refused.
    let strom_version = strom.version().await.ok();
    if let Some(warning) =
        strom_api::version_warning(strom_api::PINNED_VERSION, strom_version.as_deref())
    {
        warn!("{warning}");
        events.emit(Event::Warning { message: warning });
    }
    let open_live = open_live_client(&cfg)?;

    let chosen = if test_pattern {
        Vec::new()
    } else {
        let all = strom
            .video_devices()
            .await
            .context("asking Strom what capture devices it can see")?;
        devices::choose(all, include_virtual, selection.as_deref())?
    };
    if chosen.is_empty() && !test_pattern {
        bail!("Strom reports no capture devices. Virtual ones are skipped unless --all; --test streams a test pattern instead.");
    }

    // Resolved and checked here, before any port is asked for or any flow exists.
    let pairings = if !audio_pairings.is_empty() || test_pattern {
        audio_pairings
    } else {
        let (pairings, absent) = devices::settings_pairings(&cfg.inputs, &chosen);
        for device in absent {
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
        let sources = strom
            .audio_devices()
            .await
            .context("asking Strom what audio sources it can see")?;
        devices::pair_audio(&chosen, &sources, &pairings)?
    };

    // Without somewhere to send there is no point starting: the feeds would go nowhere.
    let cloud = resolve_cloud(&cfg, open_live.as_ref(), events).await?;
    let mut inputs = devices::inputs_for(
        &chosen,
        test_pattern,
        &cfg.gateway.name,
        &cfg.uplink,
        &cfg.capture,
        &cloud.host,
        &cloud.ports,
    )?;
    devices::attach_audio(&mut inputs, &audio);

    // Each input costs an encoder and its share of the uplink, which is easy to
    // overlook when one command starts all of them.
    events.emit(Event::BringingUp {
        inputs: inputs.len(),
        host: cloud.host.clone(),
        ports: format_ports(&cloud.ports),
        port_source: cloud.port_source.to_string(),
        total_mbps: inputs.len() as f64 * cfg.video.bitrate_kbps as f64 / 1000.0,
    });

    // When the range is Open Live's, so is the port inside it: register first and
    // take the port Open Live wrote into the source, then build the flows to it.
    let mut registered = match (&open_live, cloud.port_source) {
        (Some(client), PortSource::OpenLive) => {
            assign_ports_from_open_live(
                client,
                &mut inputs,
                &cloud.ports,
                source_gateway_id.as_deref(),
                events,
            )
            .await?
        }
        _ => HashMap::new(),
    };

    // When the ports are the settings', nothing stops a source from an earlier run
    // still holding one, and Open Live refuses every registration on it.
    if let (Some(client), PortSource::Settings) = (&open_live, cloud.port_source) {
        free_held_ports(
            client,
            &inputs,
            &cfg.gateway.name,
            source_gateway_id.as_deref(),
            events,
        )
        .await?;
    }

    clear_conflicting_flows(&strom, &gateway_id, &inputs, events).await?;

    let mut live = Vec::new();
    for input in inputs {
        let flow_id = flow::flow_id(&gateway_id, &input.id);
        let (source_id, connect_warning) = match registered.remove(&input.id) {
            Some(r) => (Some(r.source_id), r.warning),
            None => (None, None),
        };
        match start(
            &strom,
            &flow_id,
            &flow::build(&gateway_id, &input, &cfg.video),
        )
        .await
        {
            Ok(()) => live.push(Live {
                input,
                flow_id,
                state: State::Starting,
                shown: None,
                last_bytes: None,
                quiet_polls: 0,
                source_id,
                connect_warning,
                uplink: None,
            }),
            // One camera failing must not stop the rest of the venue coming up.
            Err(err) => {
                warn!(input = %input.name, err = %format_args!("{err:#}"), "could not start this input");
                events.emit(Event::InputFailed {
                    input: input.name.clone(),
                    reason: format!("{err:#}"),
                });
            }
        }
    }
    if live.is_empty() {
        bail!("no input could be started");
    }

    events.emit(summary(&live));
    if let Err(err) = write_pidfile() {
        warn!(err = %format_args!("{err:#}"), "could not write the pidfile; `down` will still find the flows and sources");
        events.emit(Event::Warning {
            message: format!(
                "could not write the pidfile ({err:#}); `down` will still find the flows and sources"
            ),
        });
    }
    let reporter = Reporter::start(&cfg, strom_version, &live, events)?;

    let (outcome, stop) = match run_until_stopped(
        &strom,
        open_live.as_ref(),
        reporter.as_ref(),
        &gateway_id,
        source_gateway_id.as_deref(),
        &cfg,
        &mut live,
        events,
        stop,
    )
    .await
    {
        Ok(stop) => (Ok(()), stop),
        Err(err) => (Err(err), Stop::Interrupt),
    };

    events.emit(Event::Stopping);
    match stop {
        Stop::Interrupt => {
            // Studio hears first, so the inputs vanishing next is read as a stop, not a fault.
            if let Some(reporter) = reporter {
                reporter.stop().await;
            }
            teardown(&strom, open_live.as_ref(), &cfg.gateway.name, &live, events).await;
        }
        Stop::Ending => {
            // Windows ends the process shortly after, so nothing waits on anything else.
            let deadline = tokio::time::Instant::now() + ENDING_TEARDOWN;
            let quiet = async {
                if let Some(reporter) = reporter {
                    reporter.stop().await;
                }
            };
            tokio::join!(
                quiet,
                teardown_within(
                    &strom,
                    open_live.as_ref(),
                    &cfg.gateway.name,
                    &live,
                    deadline,
                    events
                )
            );
        }
    }
    // Only a Strom we started is stopped here; an adopted one is left running.
    drop(local_strom);
    remove_pidfile();
    outcome
}

/// What to do about an input's port given the source Open Live already holds for it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum PortPlan {
    /// The stored source names a port inside the range: build to it.
    Keep { source_id: String, port: u16 },
    /// The stored source has no usable port (none, or one Open Live no longer
    /// holds, left by an older run): ask Open Live to reassign it.
    Reassign { source_id: String },
    /// No source yet: create one and let Open Live pick the port.
    Create,
}

/// Pure, so the three cases can be tested without a server.
fn plan_port(existing: Option<&Source>, ports: &BTreeSet<u16>) -> PortPlan {
    match existing {
        None => PortPlan::Create,
        Some(stored) => match listener_port(&stored.address) {
            Some(port) if ports.contains(&port) => PortPlan::Keep {
                source_id: stored.id.clone(),
                port,
            },
            _ => PortPlan::Reassign {
                source_id: stored.id.clone(),
            },
        },
    }
}

/// Lets Open Live choose each input's port inside the range it publishes. Several
/// gateways can feed one Open Live, and each only knows its own inputs, so the one
/// party that sees every source and output has to hand the ports out. An input
/// registers with port 0, which means "assign one", and takes the port Open Live
/// wrote into the source. A source left by an earlier run keeps its port, so the
/// cloud side stays stable across restarts. Returns what each input registered as,
/// by input id.
async fn assign_ports_from_open_live(
    client: &OpenLiveClient,
    inputs: &mut [Input],
    ports: &BTreeSet<u16>,
    gateway_id: Option<&str>,
    events: &Events,
) -> Result<HashMap<String, Registered>> {
    let sources = client
        .list_sources()
        .await
        .context("listing Open Live's sources to assign ports")?;
    let mut ids = HashMap::new();
    for input in inputs.iter_mut() {
        let existing = sources.iter().find(|s| s.name == input.name);
        let mut warning = None;
        let (source_id, address) = match plan_port(existing, ports) {
            PortPlan::Keep { source_id, port } => {
                if let Some(stored) = existing {
                    note_connect_warning(&input.name, stored, &mut warning, false, events);
                }
                input.endpoint.port = port;
                ids.insert(input.id.clone(), Registered { source_id, warning });
                continue;
            }
            PortPlan::Create => {
                events.emit(Event::Registering {
                    input: input.name.clone(),
                });
                input.endpoint.port = 0;
                let payload = SourcePayload::new(
                    &input.name,
                    &input.endpoint.cloud_uri(),
                    false,
                    input.endpoint.latency_ms,
                    gateway_id,
                );
                let created = client
                    .create_source(&payload)
                    .await
                    .with_context(|| format!("registering {:?} with Open Live", input.name))?;
                info!(input = %input.name, source_id = %created.id, "registered the source with Open Live");
                events.emit(Event::Registered {
                    input: input.name.clone(),
                    source_id: created.id.clone(),
                });
                note_connect_warning(&input.name, &created, &mut warning, true, events);
                (created.id, created.address)
            }
            PortPlan::Reassign { source_id } => {
                events.emit(Event::Registering {
                    input: input.name.clone(),
                });
                input.endpoint.port = 0;
                let payload = SourcePayload::new(
                    &input.name,
                    &input.endpoint.cloud_uri(),
                    false,
                    input.endpoint.latency_ms,
                    gateway_id,
                );
                let patched = client
                    .patch_source(&source_id, &payload)
                    .await
                    .with_context(|| {
                        format!("asking Open Live for a new port for {:?}", input.name)
                    })?;
                note_connect_warning(&input.name, &patched, &mut warning, true, events);
                (source_id, patched.address)
            }
        };
        let port = listener_port(&address).with_context(|| {
            format!(
                "Open Live stored {} for {:?}, which names no listener port; is it running a version that assigns ports?",
                mask_passphrase(&address),
                input.name
            )
        })?;
        input.endpoint.port = port;
        ids.insert(input.id.clone(), Registered { source_id, warning });
    }
    Ok(ids)
}

/// What registering an input with Open Live left behind.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Registered {
    source_id: String,
    /// The `connect.warning` already logged for it, if any.
    warning: Option<String>,
}

/// Whether Open Live's `connect.warning` for a source is logged. A create or PATCH
/// answer (`written`) is logged whenever it carries one: it is the answer to a write
/// the gateway just made. A source adopted unchanged from the list is logged only
/// when its warning differs from the one last logged for the input, so a warning
/// that persists is said once per run, not on every tick.
fn should_log_warning(current: Option<&str>, last: Option<&str>, written: bool) -> bool {
    current.is_some() && (written || current != last)
}

/// Logs Open Live's `connect.warning` for an input's source when
/// [`should_log_warning`] says so, and remembers it in `last`. Registration has
/// already succeeded; the warning is only reported, never an error.
fn note_connect_warning(
    input: &str,
    source: &Source,
    last: &mut Option<String>,
    written: bool,
    events: &Events,
) {
    let current = source.connect_warning();
    if should_log_warning(current, last.as_deref(), written) {
        warn!(
            input = %input,
            address = %mask_passphrase(&source.address),
            warning = current.unwrap_or_default(),
            "Open Live registered the source but warns its address may not work against the cloud Strom"
        );
        events.emit(Event::ConnectWarning {
            input: input.to_string(),
            warning: current.unwrap_or_default().to_string(),
        });
    }
    *last = current.map(str::to_string);
}

/// An Open Live source holding a listener port one of the inputs is about to
/// register on.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PortClash {
    input: String,
    port: u16,
    holder_id: String,
    holder_name: String,
    /// Carries this gateway's name prefix, and its gateway id where both sides have
    /// one: left by an earlier run of this gateway, so it may be removed.
    ours: bool,
}

/// Which sources hold a port an input wants, under another name. A source with the
/// input's own name is not a clash: the tick adopts it. Pure, so the choice of what
/// to remove and what to report is tested without a server.
fn port_clashes(
    wanted: &[(String, u16)],
    sources: &[Source],
    prefix: &str,
    gateway_id: Option<&str>,
) -> Vec<PortClash> {
    let mut clashes = Vec::new();
    for (input, port) in wanted {
        for holder in sources
            .iter()
            .filter(|s| listener_port(&s.address) == Some(*port) && s.name != *input)
        {
            let same_gateway = match (holder.gateway_id.as_deref(), gateway_id) {
                (Some(theirs), Some(mine)) => theirs == mine,
                _ => true,
            };
            clashes.push(PortClash {
                input: input.clone(),
                port: *port,
                holder_id: holder.id.clone(),
                holder_name: holder.name.clone(),
                ours: holder.name.starts_with(prefix) && same_gateway,
            });
        }
    }
    clashes
}

/// Frees the inputs' ports from this gateway's own leftover sources, and stops with
/// a message naming the holder when that cannot be done. Only sources actually on a
/// wanted port are touched, never the whole prefix: nothing prevents a second `up`
/// with the same settings, and its sources are not this run's to remove.
async fn free_held_ports(
    client: &OpenLiveClient,
    inputs: &[Input],
    gateway_name: &str,
    gateway_id: Option<&str>,
    events: &Events,
) -> Result<()> {
    let wanted: Vec<(String, u16)> = inputs
        .iter()
        .filter_map(|i| listener_port(&i.endpoint.cloud_uri()).map(|p| (i.name.clone(), p)))
        .collect();
    if wanted.is_empty() {
        return Ok(());
    }
    let sources = match client.list_sources().await {
        Ok(sources) => sources,
        // Open Live being away must not keep the feeds from starting; the tick
        // registers them once it is back.
        Err(err) => {
            warn!(err = %format_args!("{err:#}"), "could not list Open Live sources to check the ports");
            return Ok(());
        }
    };
    let clashes = port_clashes(
        &wanted,
        &sources,
        &devices::name_prefix(gateway_name),
        gateway_id,
    );
    // Someone else's source is reported before anything of ours is removed.
    if let Some(c) = clashes.iter().find(|c| !c.ours) {
        bail!(
            "port {} for {:?} is already used by the Open Live source {:?}, which is not this gateway's. Remove it in Studio, or set uplink.port_range to ports it does not use.",
            c.port,
            c.input,
            c.holder_name
        );
    }
    for c in &clashes {
        match client.delete_source(&c.holder_id).await {
            Ok(()) => events.emit(Event::LeftoverSourceRemoved {
                name: c.holder_name.clone(),
                port: c.port,
            }),
            Err(err) => match err.downcast_ref::<Refused>() {
                Some(refused) => bail!(
                    "port {} for {:?} is held by the Open Live source {:?}, left over from an earlier run, and Open Live will not remove it: {}. Deactivate that production, then start again.",
                    c.port,
                    c.input,
                    c.holder_name,
                    refused.explain()
                ),
                None => {
                    return Err(err).with_context(|| {
                        format!(
                            "removing the Open Live source {:?}, which holds port {} for {:?}",
                            c.holder_name, c.port, c.input
                        )
                    })
                }
            },
        }
    }
    Ok(())
}

/// Removes our own leftovers from a run that ended without cleanup, and any other
/// flow already holding one of the cameras about to be opened. Two pipelines on one
/// device contend for frames, which presents as stutter rather than as an error.
async fn clear_conflicting_flows(
    strom: &StromClient,
    gateway_id: &str,
    inputs: &[Input],
    events: &Events,
) -> Result<()> {
    let devices: Vec<&str> = inputs
        .iter()
        .filter_map(|i| match &i.source {
            flow::Source::Device { device_id, .. } => Some(device_id.as_str()),
            flow::Source::Test { .. } => None,
        })
        .collect();
    for existing in strom.list_flows().await.context("listing Strom's flows")? {
        let ours = flow::is_ours(&existing, gateway_id);
        let holds_device = flow::capture_device_of(&existing).is_some_and(|d| devices.contains(&d));
        if !(ours || holds_device) {
            continue;
        }
        remove_flow(strom, &existing.id).await?;
        events.emit(Event::ConflictingFlowRemoved {
            name: existing.name.clone(),
            ours,
        });
    }
    Ok(())
}

async fn start(strom: &StromClient, flow_id: &FlowId, desired: &strom_types::Flow) -> Result<()> {
    strom.create_flow(desired).await?;
    match strom.start_flow(flow_id).await {
        Ok(true) => Ok(()),
        Ok(false) => {
            strom.delete_flow(flow_id).await.ok();
            bail!("Strom did not report the flow running after start")
        }
        Err(err) => {
            strom.delete_flow(flow_id).await.ok();
            Err(err)
        }
    }
}

async fn remove_flow(strom: &StromClient, flow_id: &FlowId) -> Result<()> {
    strom.stop_flow(flow_id).await.ok();
    strom.delete_flow(flow_id).await
}

/// The inputs that started, with the address each registered, passphrase masked.
fn summary(live: &[Live]) -> Event {
    Event::Summary {
        inputs: live
            .iter()
            .map(|l| SummaryRow {
                name: l.input.name.clone(),
                port: l.input.endpoint.port,
                address: mask_passphrase(&l.input.endpoint.cloud_uri()),
            })
            .collect(),
    }
}

/// `s` cut to `width` characters, an ellipsis marking the cut, for a table column.
pub fn truncate(s: &str, width: usize) -> String {
    if s.chars().count() <= width {
        return s.to_string();
    }
    s.chars().take(width.saturating_sub(1)).collect::<String>() + "…"
}

/// One tick every few seconds, sequentially over every input: no shared state, no
/// task per input, and one Open Live list call per tick.
// Called once, from `up`, with what `up` holds; a struct to carry them buys nothing.
#[allow(clippy::too_many_arguments)]
async fn run_until_stopped(
    strom: &StromClient,
    open_live: Option<&OpenLiveClient>,
    reporter: Option<&Reporter>,
    gateway_id: &str,
    source_gateway_id: Option<&str>,
    cfg: &Config,
    live: &mut [Live],
    events: &Events,
    stop: impl Future<Output = Stop>,
) -> Result<Stop> {
    let mut ticker = tokio::time::interval(TICK);
    tokio::pin!(stop);
    loop {
        tokio::select! {
            _ = ticker.tick() => {
                for l in live.iter_mut() {
                    l.state = match poll_flow(strom, gateway_id, &cfg.video, l).await {
                        Ok(state) => state,
                        Err(err) => {
                            warn!(input = %l.input.name, err = %format_args!("{err:#}"), "lost contact with Strom, retrying next tick");
                            l.last_bytes = None;
                            l.uplink = None;
                            State::Unreachable
                        }
                    };
                }
                // Handed over as a snapshot; the heartbeat sends it on its own clock,
                // so a slow or absent Open Live never holds this loop.
                if let Some(reporter) = reporter {
                    reporter.publish(live);
                }
                if let Some(client) = open_live {
                    if let Err(err) = reconcile_sources(client, live, source_gateway_id, events).await {
                        // Never fatal: the feeds are already flowing. Retried next tick.
                        warn!(err = %format_args!("{err:#}"), "could not update the Open Live sources, retrying next tick");
                        events.emit(Event::Warning {
                            message: format!("could not update the Open Live sources ({err:#}); the feeds are unaffected, retrying next tick"),
                        });
                    }
                }
                // Only changes are told, so the CLI appends a line per change rather
                // than redrawing a screen: it is read over SSH, in tmux, and in a log
                // file after the fact.
                for l in live.iter_mut() {
                    if l.shown.as_ref() != Some(&l.state) {
                        events.emit(Event::Input {
                            input: l.input.name.clone(),
                            state: l.state.clone(),
                        });
                        l.shown = Some(l.state.clone());
                    }
                }
                events.emit(stats_of(live));
            }
            stop = &mut stop => return Ok(stop),
        }
    }
}

/// Makes sure the flow exists and runs, then judges the uplink from its statistics.
async fn poll_flow(
    strom: &StromClient,
    gateway_id: &str,
    video: &Video,
    l: &mut Live,
) -> Result<State> {
    let running = match strom.get_flow(&l.flow_id).await? {
        Some(flow) => flow.running,
        None => {
            // Deleted under us, in Strom's editor or by another tool. Put it back.
            info!(input = %l.input.name, "flow is gone from Strom, recreating it");
            strom
                .create_flow(&flow::build(gateway_id, &l.input, video))
                .await?;
            false
        }
    };
    if !running {
        l.last_bytes = None;
        l.uplink = None;
        return Ok(if strom.start_flow(&l.flow_id).await? {
            State::Starting
        } else {
            State::Failed {
                reason: "Strom did not report the flow running after start".to_string(),
            }
        });
    }

    let Some(sample) = strom.srt_uplink(&l.flow_id).await? else {
        l.last_bytes = None;
        l.uplink = None;
        l.quiet_polls = 0;
        return Ok(State::Waiting);
    };
    l.uplink = Some(sample.clone());
    // Bytes moving between two polls is the test, not growth: a reconnect resets the
    // counter. The first sample has nothing to compare against and proves nothing.
    let delivering = l.last_bytes.is_some_and(|before| {
        sample.bytes_sent.unwrap_or(0) != before && sample.bytes_sent.unwrap_or(0) > 0
    });
    l.last_bytes = Some(sample.bytes_sent.unwrap_or(0));
    if delivering {
        l.quiet_polls = 0;
        return Ok(State::OnAir);
    }
    l.quiet_polls += 1;
    Ok(
        if l.state == State::OnAir && l.quiet_polls < QUIET_TOLERANCE {
            State::OnAir
        } else {
            State::Waiting
        },
    )
}

/// Creates or updates each input's Open Live source. One list call proves the API is
/// serving and gives the stored sources to compare against. A source left by an
/// earlier run is adopted by name rather than recreated, so a Studio assignment that
/// references its id survives a restart.
async fn reconcile_sources(
    client: &OpenLiveClient,
    live: &mut [Live],
    gateway_id: Option<&str>,
    events: &Events,
) -> Result<()> {
    let sources = client.list_sources().await?;
    for l in live.iter_mut() {
        let existing = l
            .source_id
            .as_deref()
            .and_then(|id| sources.iter().find(|s| s.id == id))
            .or_else(|| sources.iter().find(|s| s.name == l.input.name));
        let active = match l.state {
            State::OnAir => true,
            State::Unreachable => existing.is_some_and(|s| s.status == "active"),
            _ => false,
        };
        let desired = SourcePayload::new(
            &l.input.name,
            &l.input.endpoint.cloud_uri(),
            active,
            l.input.endpoint.latency_ms,
            gateway_id,
        );
        match existing {
            Some(stored) => {
                if drifted(stored, &desired) {
                    let patched = client.patch_source(&stored.id, &desired).await?;
                    note_connect_warning(
                        &l.input.name,
                        &patched,
                        &mut l.connect_warning,
                        true,
                        events,
                    );
                } else {
                    note_connect_warning(
                        &l.input.name,
                        stored,
                        &mut l.connect_warning,
                        false,
                        events,
                    );
                }
                l.source_id = Some(stored.id.clone());
            }
            None => {
                events.emit(Event::Registering {
                    input: l.input.name.clone(),
                });
                let created = client.create_source(&desired).await?;
                info!(input = %l.input.name, source_id = %created.id, "registered the source with Open Live");
                events.emit(Event::Registered {
                    input: l.input.name.clone(),
                    source_id: created.id.clone(),
                });
                note_connect_warning(
                    &l.input.name,
                    &created,
                    &mut l.connect_warning,
                    true,
                    events,
                );
                l.source_id = Some(created.id);
            }
        }
    }
    Ok(())
}

/// Why `up` is stopping, which decides how long teardown may take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stop {
    /// A deliberate stop (Ctrl-C, Ctrl-Break, SIGTERM, a Stop button): teardown takes
    /// as long as it needs.
    Interrupt,
    /// The process is about to be ended from outside (on Windows: the console window
    /// closed, the user logging off or the machine shutting down, after which Windows
    /// ends the process about five seconds later): teardown gets [`ENDING_TEARDOWN`].
    Ending,
}

/// How long teardown may take when Windows is ending the process, leaving a second
/// of the five Windows allows for stopping the managed Strom. That second is enough
/// only because `LocalStrom`'s drop ends a managed Strom at once on Windows
/// (`Child::kill`, i.e. TerminateProcess) before it waits, rather than giving it
/// `STOP_TIMEOUT` to exit by itself.
pub const ENDING_TEARDOWN: Duration = Duration::from_secs(4);

/// Removes one flow and says so. Returns whether it is gone.
async fn remove_flow_reported(strom: &StromClient, l: &Live, events: &Events) -> bool {
    match remove_flow(strom, &l.flow_id).await {
        Ok(()) => {
            events.emit(Event::FlowRemoved {
                input: l.input.name.clone(),
            });
            true
        }
        Err(err) => {
            warn!(input = %l.input.name, err = %format_args!("{err:#}"), "could not remove the flow");
            events.emit(Event::Warning {
                message: format!("could not remove the flow of {} ({err:#})", l.input.name),
            });
            false
        }
    }
}

/// Removes one input's Open Live source and says so. Returns whether it is gone.
async fn remove_source_reported(
    client: &OpenLiveClient,
    id: &str,
    l: &Live,
    events: &Events,
) -> bool {
    let gone = remove_source(client, id, &l.input.name, events).await;
    if gone {
        events.emit(Event::SourceRemoved {
            input: l.input.name.clone(),
        });
    }
    gone
}

/// Stops and removes everything the run created.
async fn teardown(
    strom: &StromClient,
    open_live: Option<&OpenLiveClient>,
    gateway_name: &str,
    live: &[Live],
    events: &Events,
) {
    // Sources Open Live refused to remove, already reported, so the sweep below does
    // not try them again and print the same refusal twice.
    let mut kept = BTreeSet::new();
    for l in live {
        remove_flow_reported(strom, l, events).await;
        if let (Some(client), Some(id)) = (open_live, &l.source_id) {
            if !remove_source_reported(client, id, l, events).await {
                kept.insert(id.clone());
            }
        }
    }
    // Anything of ours still there goes too: a source recreated by a race, or one
    // whose id we never learned. Matched on the name prefix, so sources belonging to
    // another gateway or made by hand are never touched.
    if let Some(client) = open_live {
        sweep_sources(client, gateway_name, &kept, events).await;
    }
}

/// Teardown for when Windows is ending the process: every flow and source removal
/// runs at once, and whatever has not finished by `deadline` is named in the log and
/// left for the next `up` or `down`, which find it by derived id and name prefix.
async fn teardown_within(
    strom: &StromClient,
    open_live: Option<&OpenLiveClient>,
    gateway_name: &str,
    live: &[Live],
    deadline: tokio::time::Instant,
    events: &Events,
) {
    // Send, so that `up` can run on a spawned task (`session::Session::start`).
    type Job<'a> = std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>>;
    // What each job removes, and the source id when it is a source.
    let mut what: Vec<(String, Option<String>)> = Vec::new();
    let mut jobs: Vec<Job> = Vec::new();
    for l in live {
        what.push((format!("flow of {}", l.input.name), None));
        jobs.push(Box::pin(remove_flow_reported(strom, l, events)));
        if let (Some(client), Some(id)) = (open_live, &l.source_id) {
            what.push((format!("source of {}", l.input.name), Some(id.clone())));
            jobs.push(Box::pin(remove_source_reported(client, id, l, events)));
        }
    }
    let mut kept = BTreeSet::new();
    let mut left = Vec::new();
    for ((label, id), done) in what.into_iter().zip(join_until(deadline, jobs).await) {
        match done {
            Some(true) => {}
            Some(false) => {
                kept.extend(id);
                left.push(label);
            }
            None => left.push(label),
        }
    }
    if let Some(client) = open_live {
        let sweep = sweep_sources(client, gateway_name, &kept, events);
        if tokio::time::timeout_at(deadline, sweep).await.is_err() {
            left.push("the sweep for other sources of this gateway".to_string());
        }
    }
    if !left.is_empty() {
        warn!(
            left = %left.join(", "),
            "not removed before Windows ends the process; the next `open-live-ingest up` or `down` removes them"
        );
        events.emit(Event::Warning {
            message: format!(
                "not removed before the process ends: {}; the next `open-live-ingest up` or `down` removes them",
                left.join(", ")
            ),
        });
    }
}

/// Runs every job at once and collects what finished by `deadline`, in job order;
/// `None` for a job still running then, which is dropped.
async fn join_until<T, F>(deadline: tokio::time::Instant, jobs: Vec<F>) -> Vec<Option<T>>
where
    F: std::future::Future<Output = T>,
{
    use futures_util::stream::{FuturesUnordered, StreamExt};

    let mut results: Vec<Option<T>> = jobs.iter().map(|_| None).collect();
    let mut running: FuturesUnordered<_> = jobs
        .into_iter()
        .enumerate()
        .map(|(i, job)| async move { (i, job.await) })
        .collect();
    let _ = tokio::time::timeout_at(deadline, async {
        while let Some((i, out)) = running.next().await {
            results[i] = Some(out);
        }
    })
    .await;
    results
}

#[cfg(test)]
mod teardown_tests {
    use super::*;
    use std::time::Instant;

    #[tokio::test]
    async fn join_until_returns_at_the_deadline_when_a_job_never_finishes() {
        let started = Instant::now();
        let deadline = tokio::time::Instant::now() + Duration::from_millis(200);
        let jobs: Vec<std::pin::Pin<Box<dyn std::future::Future<Output = u8>>>> = vec![
            Box::pin(async { 1 }),
            Box::pin(std::future::pending()),
            Box::pin(async {
                tokio::time::sleep(Duration::from_millis(20)).await;
                3
            }),
        ];
        let results = join_until(deadline, jobs).await;
        let took = started.elapsed();
        assert_eq!(results, vec![Some(1), None, Some(3)]);
        assert!(
            took >= Duration::from_millis(200),
            "returned early: {took:?}"
        );
        assert!(
            took < Duration::from_secs(2),
            "overran the deadline: {took:?}"
        );
    }

    #[tokio::test]
    async fn join_until_runs_the_jobs_concurrently() {
        let started = Instant::now();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let jobs: Vec<_> = (0..4u8)
            .map(|i| async move {
                tokio::time::sleep(Duration::from_millis(300)).await;
                i
            })
            .collect();
        let results = join_until(deadline, jobs).await;
        assert_eq!(results, vec![Some(0), Some(1), Some(2), Some(3)]);
        // One after another would take 1.2 s.
        assert!(started.elapsed() < Duration::from_millis(900));
    }

    #[tokio::test]
    async fn join_until_with_no_jobs_returns_at_once() {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let started = Instant::now();
        let results = join_until::<u8, std::future::Ready<u8>>(deadline, Vec::new()).await;
        assert!(results.is_empty());
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}

/// Deletes one source, and says so plainly when Open Live refuses: a source in an
/// active production stays, and the operator needs to know which production holds
/// it and how to clear it. Returns whether the source is gone.
async fn remove_source(client: &OpenLiveClient, id: &str, name: &str, events: &Events) -> bool {
    match client.delete_source(id).await {
        Ok(()) => true,
        Err(err) => {
            match err.downcast_ref::<Refused>() {
                Some(refused) => events.emit(Event::SourceKept {
                    input: name.to_string(),
                    reason: refused.explain(),
                }),
                None => {
                    warn!(source = %name, err = %format_args!("{err:#}"), "could not remove the source");
                    events.emit(Event::Warning {
                        message: format!("could not remove the source {name} ({err:#})"),
                    });
                }
            }
            false
        }
    }
}

/// Removes every source carrying this gateway's name prefix, except the ids in
/// `skip`. Returns how many.
async fn sweep_sources(
    client: &OpenLiveClient,
    gateway_name: &str,
    skip: &BTreeSet<String>,
    events: &Events,
) -> usize {
    let prefix = devices::name_prefix(gateway_name);
    let sources = match client.list_sources().await {
        Ok(sources) => sources,
        Err(err) => {
            warn!(err = %format_args!("{err:#}"), "could not list Open Live sources to clean up");
            return 0;
        }
    };
    let mut removed = 0;
    for source in sources
        .iter()
        .filter(|s| s.name.starts_with(&prefix) && !skip.contains(&s.id))
    {
        if remove_source(client, &source.id, &source.name, events).await {
            events.emit(Event::SourceRemoved {
                input: source.name.clone(),
            });
            removed += 1;
        }
    }
    removed
}

/// Stops a running `up` if there is one, then removes whatever is left.
pub async fn down(cfg: &Config, events: &Events) -> Result<()> {
    let gateway_id = cfg.gateway.resolved_id();

    if let Some(pid) = read_pidfile() {
        if is_running_gateway(pid) {
            events.emit(Event::AskingRunningToStop { pid });
            signal_stop(pid);
            for _ in 0..60 {
                if !process_alive(pid) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            if process_alive(pid) {
                events.emit(Event::RunningDidNotStop);
            }
        }
        remove_pidfile();
    }

    let mut removed = 0;
    let strom = StromClient::new(&cfg.strom.url, cfg.strom.api_key.as_deref())?;
    match strom.list_flows().await {
        Ok(flows) => {
            for f in flows.iter().filter(|f| flow::is_ours(f, &gateway_id)) {
                match remove_flow(&strom, &f.id).await {
                    Ok(()) => {
                        events.emit(Event::FlowRemoved {
                            input: f.name.clone(),
                        });
                        removed += 1;
                    }
                    Err(err) => {
                        warn!(flow = %f.name, err = %format_args!("{err:#}"), "could not remove the flow");
                        events.emit(Event::Warning {
                            message: format!("could not remove the flow {} ({err:#})", f.name),
                        });
                    }
                }
            }
        }
        // Deliberately not fatal: if Strom has gone its flows are already stopped, and
        // the Open Live sources still need removing or they sit in Studio forever.
        Err(err) => events.emit(Event::StromUnreachable {
            url: cfg.strom.url.clone(),
            error: err.to_string(),
        }),
    }
    if let Some(client) = open_live_client(cfg)? {
        removed += sweep_sources(&client, &cfg.gateway.name, &BTreeSet::new(), events).await;
    }
    local_strom::stop_recorded(events);
    if removed == 0 {
        events.emit(Event::NothingToRemove);
    }
    Ok(())
}

/// Everything `status` reports, gathered once and rendered as text or JSON.
#[derive(Debug, Serialize)]
pub struct StatusReport {
    /// The `up` process, by its pidfile.
    pub process: ProcessReport,
    /// Where feeds go, or why that could not be worked out.
    pub uplink: Option<UplinkReport>,
    pub uplink_error: Option<String>,
    pub strom: EndpointReport,
    /// Absent when registration is off.
    pub open_live: Option<EndpointReport>,
    pub inputs: Vec<InputReport>,
}

#[derive(Debug, Serialize)]
pub struct ProcessReport {
    pub running: bool,
    pub pid: Option<u32>,
}

#[derive(Debug, Serialize)]
pub struct UplinkReport {
    pub host: String,
    pub ports: String,
    pub port_source: String,
}

#[derive(Debug, Serialize)]
pub struct EndpointReport {
    pub url: String,
    pub reachable: bool,
    pub error: Option<String>,
}

/// One input, by name: its flow in Strom, that flow's uplink, and its source in
/// Open Live. Any of the three may be missing; that is what `status` is for.
#[derive(Debug, Serialize)]
pub struct InputReport {
    pub name: String,
    pub flow: Option<FlowReport>,
    pub uplink: Option<UplinkStatsReport>,
    pub source: Option<SourceReport>,
}

#[derive(Debug, Serialize)]
pub struct FlowReport {
    pub id: String,
    pub running: bool,
}

#[derive(Debug, Serialize)]
pub struct UplinkStatsReport {
    pub send_rate_mbps: Option<f64>,
    pub rtt_ms: Option<f64>,
    pub bytes_sent: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct SourceReport {
    pub id: String,
    pub status: String,
    /// Open Live's `connect.warning`, such as `srt-listener-unsupported`. `null` when
    /// it reports none, or is older than the field.
    pub warning: Option<String>,
}

/// Reports what is running, from Strom and Open Live directly. Neither being
/// reachable is an error: that is part of the report.
pub async fn status(cfg: &Config) -> Result<StatusReport> {
    let gateway_id = cfg.gateway.resolved_id();
    let process = match read_pidfile() {
        Some(pid) if is_running_gateway(pid) => ProcessReport {
            running: true,
            pid: Some(pid),
        },
        _ => ProcessReport {
            running: false,
            pid: None,
        },
    };
    let open_live = open_live_client(cfg)?;
    // Warnings while working out the uplink are the log's, not the report's.
    let (uplink, uplink_error) =
        match resolve_cloud(cfg, open_live.as_ref(), &Events::discard()).await {
            Ok(cloud) => (
                Some(UplinkReport {
                    host: cloud.host,
                    ports: format_ports(&cloud.ports),
                    port_source: cloud.port_source.to_string(),
                }),
                None,
            ),
            Err(err) => (None, Some(format!("{err:#}"))),
        };

    let strom = StromClient::new(&cfg.strom.url, cfg.strom.api_key.as_deref())?;
    let (flows, strom_report) = match strom.list_flows().await {
        Ok(flows) => (
            flows
                .into_iter()
                .filter(|f| flow::is_ours(f, &gateway_id))
                .collect(),
            EndpointReport {
                url: cfg.strom.url.clone(),
                reachable: true,
                error: None,
            },
        ),
        Err(err) => (
            Vec::new(),
            EndpointReport {
                url: cfg.strom.url.clone(),
                reachable: false,
                error: Some(err.to_string()),
            },
        ),
    };
    let prefix = devices::name_prefix(&cfg.gateway.name);
    let (sources, open_live_report) = match &open_live {
        Some(client) => {
            let url = cfg.open_live.url.clone().unwrap_or_default();
            match client.list_sources().await {
                Ok(sources) => (
                    sources
                        .into_iter()
                        .filter(|s| s.name.starts_with(&prefix))
                        .collect(),
                    Some(EndpointReport {
                        url,
                        reachable: true,
                        error: None,
                    }),
                ),
                Err(err) => (
                    Vec::new(),
                    Some(EndpointReport {
                        url,
                        reachable: false,
                        error: Some(err.to_string()),
                    }),
                ),
            }
        }
        None => (Vec::new(), None),
    };

    let names: BTreeSet<&str> = flows
        .iter()
        .map(|f| f.name.as_str())
        .chain(sources.iter().map(|s| s.name.as_str()))
        .collect();
    let mut inputs = Vec::with_capacity(names.len());
    for name in names {
        let flow = flows.iter().find(|f| f.name == name);
        let uplink = match flow {
            Some(f) => strom
                .srt_uplink(&f.id)
                .await
                .ok()
                .flatten()
                .map(|s| UplinkStatsReport {
                    send_rate_mbps: s.send_rate_mbps,
                    rtt_ms: s.rtt_ms,
                    bytes_sent: s.bytes_sent,
                }),
            None => None,
        };
        inputs.push(InputReport {
            name: name.to_string(),
            flow: flow.map(|f| FlowReport {
                id: f.id.to_string(),
                running: f.running,
            }),
            uplink,
            source: sources
                .iter()
                .find(|s| s.name == name)
                .map(|s| SourceReport {
                    id: s.id.clone(),
                    status: s.status.clone(),
                    warning: s.connect_warning().map(str::to_string),
                }),
        });
    }
    Ok(StatusReport {
        process,
        uplink,
        uplink_error,
        strom: strom_report,
        open_live: open_live_report,
        inputs,
    })
}

/// The `status` table, as the CLI prints it.
pub fn render_status(r: &StatusReport) -> String {
    let mut out = String::new();
    match r.process.pid {
        Some(pid) => out += &format!("Gateway running (pid {pid}).\n"),
        None => out += "No gateway process running.\n",
    }
    match (&r.uplink, &r.uplink_error) {
        (Some(u), _) => {
            out += &format!(
                "Uplink to {}, SRT ports {} from {}.\n\n",
                u.host, u.ports, u.port_source
            )
        }
        (None, Some(err)) => out += &format!("Uplink not resolved: {err}.\n\n"),
        (None, None) => {}
    }
    if let Some(err) = &r.strom.error {
        out += &format!("Strom at {} is not reachable ({err}).\n", r.strom.url);
    }
    if let Some(err) = r.open_live.as_ref().and_then(|o| o.error.as_deref()) {
        out += &format!("Open Live is not reachable ({err}).\n");
    }
    if r.inputs.is_empty() {
        out += "Nothing of ours in Strom or Open Live.\n";
        return out;
    }
    out += &format!("{:<36} {:<9} {:<26} OPEN LIVE\n", "INPUT", "FLOW", "UPLINK");
    for input in &r.inputs {
        let flow_state = match &input.flow {
            Some(f) if f.running => "running",
            Some(_) => "stopped",
            None => "-",
        };
        let uplink = match (&input.flow, &input.uplink) {
            (None, _) => "-".to_string(),
            (Some(_), None) => "no receiver".to_string(),
            (Some(_), Some(s)) => match (s.send_rate_mbps, s.rtt_ms) {
                (Some(rate), Some(rtt)) => format!("{rate:.2} Mbps, rtt {rtt:.0} ms"),
                _ => format!("{} bytes sent", s.bytes_sent.unwrap_or(0)),
            },
        };
        // Last column, so a warning lengthens the line without moving any column.
        let source = match &input.source {
            Some(s) => match &s.warning {
                Some(warning) => format!("{} (warning: {warning})", s.status),
                None => s.status.clone(),
            },
            None => "not registered".to_string(),
        };
        out += &format!(
            "{:<36} {:<9} {:<26} {source}\n",
            truncate(&input.name, 36),
            flow_state,
            uplink
        );
    }
    out
}

fn write_pidfile() -> Result<()> {
    let path = config::pidfile();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(&path, std::process::id().to_string())
        .with_context(|| format!("writing {}", path.display()))
}

fn read_pidfile() -> Option<u32> {
    std::fs::read_to_string(config::pidfile())
        .ok()?
        .trim()
        .parse()
        .ok()
}

fn remove_pidfile() {
    std::fs::remove_file(config::pidfile()).ok();
}

/// Whether the pid is alive and is a gateway. Pids are reused after a reboot, so a
/// stale pidfile must never have `down` signal an unrelated process.
fn is_running_gateway(pid: u32) -> bool {
    process_named(pid, GATEWAY_COMM)
}

/// What `ps -o comm=` shows for this binary. Linux keeps only the first 15 bytes of
/// a command name (`open-live-inges`); macOS prints the full path, which ends in the
/// whole name. The truncated name is contained in both.
const GATEWAY_COMM: &str = truncate_comm(env!("CARGO_PKG_NAME"));

/// The first 15 bytes of `name`, Linux's limit for a command name.
const fn truncate_comm(name: &str) -> &str {
    let bytes = name.as_bytes();
    if bytes.len() <= 15 {
        return name;
    }
    match std::str::from_utf8(bytes.split_at(15).0) {
        Ok(head) => head,
        Err(_) => panic!("the package name must be ASCII"),
    }
}

/// Whether the pid is alive and its command name contains `needle`.
#[cfg(not(windows))]
pub fn process_named(pid: u32, needle: &str) -> bool {
    if !process_alive(pid) {
        return false;
    }
    let Ok(out) = std::process::Command::new("ps")
        .args(["-o", "comm=", "-p", &pid.to_string()])
        .output()
    else {
        return false;
    };
    comm_matches(&String::from_utf8_lossy(&out.stdout), needle)
}

/// Whether the pid is alive and its image name (`strom.exe`,
/// `open-live-ingest.exe`) contains `needle`, ignoring case. The image name comes
/// from the row tasklist prints for exactly this pid, so a stale pid that has been
/// reused matches only if the new process has the same name.
#[cfg(windows)]
pub fn process_named(pid: u32, needle: &str) -> bool {
    tasklist_image(pid).is_some_and(|image| image_matches(&image, needle))
}

/// Whether a command name from `ps -o comm=` contains `needle`.
#[cfg_attr(windows, allow(dead_code))]
fn comm_matches(comm: &str, needle: &str) -> bool {
    comm.contains(needle)
}

/// Whether a Windows image name contains `needle`. Windows file names are case
/// insensitive, so `Strom.exe` is the same program as `strom.exe`.
#[cfg_attr(not(windows), allow(dead_code))]
fn image_matches(image: &str, needle: &str) -> bool {
    !needle.is_empty() && image.to_lowercase().contains(&needle.to_lowercase())
}

/// The image name tasklist reports for `pid`, or None when no such process runs.
#[cfg(windows)]
fn tasklist_image(pid: u32) -> Option<String> {
    let filter = format!("PID eq {pid}");
    let out = std::process::Command::new("tasklist")
        .args(["/FI", &filter, "/FO", "CSV", "/NH"])
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_tasklist_csv(&String::from_utf8_lossy(&out.stdout), pid)
}

/// The image name in the row of `tasklist /FO CSV /NH` output whose PID column is
/// `pid`. A row looks like `"strom.exe","4242","Console","1","51,204 K"`. When
/// nothing matches, tasklist prints a localised `INFO: ...` line instead of a row,
/// so only a row whose second field is exactly this pid counts; anything else,
/// including text in another language, means "not running".
#[cfg_attr(not(windows), allow(dead_code))]
fn parse_tasklist_csv(output: &str, pid: u32) -> Option<String> {
    let pid = pid.to_string();
    output.lines().find_map(|line| {
        let fields = csv_fields(line.trim())?;
        match fields.as_slice() {
            [image, row_pid, ..] if *row_pid == pid && !image.is_empty() => Some(image.clone()),
            _ => None,
        }
    })
}

/// The fields of one CSV line in which every field is quoted, as tasklist writes
/// them, with `""` standing for a literal quote. None for a line that is not in that
/// form.
#[cfg_attr(not(windows), allow(dead_code))]
fn csv_fields(line: &str) -> Option<Vec<String>> {
    let mut fields = Vec::new();
    let mut chars = line.chars().peekable();
    loop {
        if chars.next()? != '"' {
            return None;
        }
        let mut field = String::new();
        loop {
            match chars.next()? {
                '"' if chars.peek() == Some(&'"') => {
                    chars.next();
                    field.push('"');
                }
                '"' => break,
                c => field.push(c),
            }
        }
        fields.push(field);
        match chars.next() {
            None => return Some(fields),
            Some(',') => {}
            Some(_) => return None,
        }
    }
}

#[cfg(unix)]
fn process_alive(pid: u32) -> bool {
    // Signal 0 checks for existence without delivering anything.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

#[cfg(unix)]
fn signal_stop(pid: u32) {
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGTERM);
    }
}

#[cfg(windows)]
fn process_alive(pid: u32) -> bool {
    tasklist_image(pid).is_some()
}

/// Windows cannot deliver a Ctrl-C to a process in another console, and has no
/// SIGTERM, so the running `up` is ended hard and does not get to tear down. That is
/// safe because `down` does the teardown itself after this: it removes the flows and
/// the Open Live sources, then `local_strom::stop_recorded()` stops the Strom that
/// `up` started and left behind.
#[cfg(windows)]
fn signal_stop(pid: u32) {
    let _ = std::process::Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/F"])
        .stdin(std::process::Stdio::null())
        .output();
}

#[cfg(not(any(unix, windows)))]
fn process_alive(_pid: u32) -> bool {
    false
}

#[cfg(not(any(unix, windows)))]
fn signal_stop(_pid: u32) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_status() -> StatusReport {
        StatusReport {
            process: ProcessReport {
                running: true,
                pid: Some(4242),
            },
            uplink: Some(UplinkReport {
                host: "cloud.example.com".to_string(),
                ports: "47110-47129".to_string(),
                port_source: PortSource::OpenLive.to_string(),
            }),
            uplink_error: None,
            strom: EndpointReport {
                url: "http://127.0.0.1:8080".to_string(),
                reachable: true,
                error: None,
            },
            open_live: Some(EndpointReport {
                url: "https://open-live.example.com".to_string(),
                reachable: false,
                error: Some("connection refused".to_string()),
            }),
            inputs: vec![
                InputReport {
                    name: "Venue — Camera 1".to_string(),
                    flow: Some(FlowReport {
                        id: "f1".to_string(),
                        running: true,
                    }),
                    uplink: Some(UplinkStatsReport {
                        send_rate_mbps: Some(5.987),
                        rtt_ms: Some(31.4),
                        bytes_sent: Some(1_000),
                    }),
                    source: Some(SourceReport {
                        id: "s1".to_string(),
                        status: "active".to_string(),
                        warning: None,
                    }),
                },
                InputReport {
                    name: "Venue — Camera 2".to_string(),
                    flow: Some(FlowReport {
                        id: "f2".to_string(),
                        running: true,
                    }),
                    uplink: None,
                    source: None,
                },
            ],
        }
    }

    #[test]
    fn status_text_reads_as_before() {
        let text = render_status(&sample_status());
        assert!(text.starts_with("Gateway running (pid 4242).\n"));
        assert!(
            text.contains("Uplink to cloud.example.com, SRT ports 47110-47129 from Open Live.\n")
        );
        assert!(text.contains("Open Live is not reachable (connection refused).\n"));
        assert!(
            !text.contains("Strom at"),
            "a reachable Strom is not mentioned"
        );
        let camera_1 = text.lines().find(|l| l.contains("Camera 1")).unwrap();
        assert!(camera_1.contains("running"));
        assert!(camera_1.contains("5.99 Mbps, rtt 31 ms"));
        assert!(camera_1.ends_with("active"));
        let camera_2 = text.lines().find(|l| l.contains("Camera 2")).unwrap();
        assert!(camera_2.contains("no receiver"));
        assert!(camera_2.ends_with("not registered"));
    }

    /// The JSON is the contract a script reads, so its shape is pinned here.
    #[test]
    fn status_json_has_the_documented_shape() {
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&sample_status()).unwrap()).unwrap();
        assert_eq!(v["process"]["running"], true);
        assert_eq!(v["process"]["pid"], 4242);
        assert_eq!(v["uplink"]["host"], "cloud.example.com");
        assert_eq!(v["uplink"]["port_source"], "Open Live");
        assert_eq!(v["uplink_error"], serde_json::Value::Null);
        assert_eq!(v["strom"]["reachable"], true);
        assert_eq!(v["open_live"]["reachable"], false);
        assert_eq!(v["open_live"]["error"], "connection refused");
        assert_eq!(v["inputs"][0]["flow"]["running"], true);
        assert_eq!(v["inputs"][0]["uplink"]["rtt_ms"], 31.4);
        assert_eq!(v["inputs"][0]["source"]["status"], "active");
        assert_eq!(v["inputs"][0]["source"]["warning"], serde_json::Value::Null);
        assert_eq!(v["inputs"][1]["uplink"], serde_json::Value::Null);
        assert_eq!(v["inputs"][1]["source"], serde_json::Value::Null);

        let mut warned = sample_status();
        warned.inputs[0].source.as_mut().unwrap().warning =
            Some("srt-port-outside-strom-ports".to_string());
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&warned).unwrap()).unwrap();
        assert_eq!(
            v["inputs"][0]["source"]["warning"],
            "srt-port-outside-strom-ports"
        );
    }

    /// A source Open Live warns about shows the code in the last column, and the
    /// columns before it do not move.
    #[test]
    fn status_text_shows_a_connect_warning_without_moving_the_columns() {
        let plain = render_status(&sample_status());
        let mut warned = sample_status();
        warned.inputs[0].source.as_mut().unwrap().warning =
            Some("srt-listener-unsupported".to_string());
        let text = render_status(&warned);
        let before = plain.lines().find(|l| l.contains("Camera 1")).unwrap();
        let camera_1 = text.lines().find(|l| l.contains("Camera 1")).unwrap();
        assert_eq!(
            camera_1,
            format!("{before} (warning: srt-listener-unsupported)")
        );
        // Every other line is unchanged.
        let others = |t: &str| {
            t.lines()
                .filter(|l| !l.contains("Camera 1"))
                .map(str::to_string)
                .collect::<Vec<_>>()
        };
        assert_eq!(others(&text), others(&plain));
    }

    /// A warning that persists is logged once per run for a source adopted as is,
    /// while every create or PATCH answer that carries one is logged.
    #[test]
    fn a_persisting_connect_warning_is_logged_once_but_every_write_reports_it() {
        let code = Some("srt-listener-unsupported");
        // Adopted unchanged: first sighting logs, the same one on later ticks does not.
        assert!(should_log_warning(code, None, false));
        assert!(!should_log_warning(code, code, false));
        // A different code is news.
        assert!(should_log_warning(
            Some("srt-port-outside-strom-ports"),
            code,
            false
        ));
        // A write answered with a warning always says so.
        assert!(should_log_warning(code, code, true));
        // Nothing to warn about is never logged.
        assert!(!should_log_warning(None, code, false));
        assert!(!should_log_warning(None, None, true));
    }

    /// `note_connect_warning` remembers what it saw, so a warning that clears and
    /// comes back is logged again, and one that holds is not.
    #[test]
    fn the_last_connect_warning_is_remembered_and_cleared() {
        let mut warned = stored("srt://:47103?mode=listener");
        warned.connect = Some(crate::openlive::Connect {
            warning: Some("srt-listener-unsupported".to_string()),
        });
        let clear = stored("srt://:47103?mode=listener");
        let mut last = None;
        note_connect_warning("Venue — Cam", &warned, &mut last, false, &Events::discard());
        assert_eq!(last.as_deref(), Some("srt-listener-unsupported"));
        note_connect_warning("Venue — Cam", &clear, &mut last, false, &Events::discard());
        assert_eq!(last, None);
    }

    fn discovered(id: &str, name: &str, audio: bool) -> DeviceResponse {
        use strom_types::discovery::DeviceCategory;
        DeviceResponse {
            id: id.to_string(),
            name: name.to_string(),
            device_class: if audio {
                "Audio/Source"
            } else {
                "Video/Source"
            }
            .to_string(),
            category: if audio {
                DeviceCategory::AudioSource
            } else {
                DeviceCategory::VideoSource
            },
            provider: "test".to_string(),
            properties: Default::default(),
            first_seen_secs_ago: 0,
            last_seen_secs_ago: 0,
        }
    }

    #[test]
    fn devices_render_as_a_table_and_as_json() {
        let all = device_report(
            vec![
                discovered("obs-0", "OBS Virtual Camera", false),
                discovered("decklink-0", "DeckLink SDI", false),
            ],
            Vec::new(),
        );
        let text = render_devices(&all);
        assert!(text.starts_with("DEVICE"));
        assert!(text.contains("OBS Virtual Camera"));
        assert!(text.contains("virtual, skipped unless --all"));
        assert!(!text.contains("AUDIO SOURCE"));
        assert_eq!(
            render_devices(&[]),
            "Strom reports no video sources on this machine.\n"
        );

        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&all).unwrap()).unwrap();
        assert_eq!(v[0]["id"], "decklink-0");
        assert_eq!(v[0]["kind"], "video");
        assert_eq!(v[0]["virtual"], false);
        assert_eq!(v[1]["virtual"], true);
    }

    /// #64 AC2: audio sources are listed too, after the cameras and marked as audio.
    #[test]
    fn devices_list_audio_sources_after_the_cameras() {
        let all = device_report(
            vec![discovered("dev-cam", "FaceTime HD Camera", false)],
            vec![
                discovered("dev-usb", "USB Microphone", true),
                discovered("dev-loop", "Loopback Audio", true),
            ],
        );
        let text = render_devices(&all);
        let audio_at = text.find("AUDIO SOURCE").expect("an audio section");
        assert!(text.find("FaceTime HD Camera").unwrap() < audio_at);
        assert!(text[audio_at..].contains("USB Microphone"));
        assert!(text[audio_at..].contains("dev-usb"));
        assert!(text.contains("--audio"));

        let v: serde_json::Value = serde_json::to_value(&all).unwrap();
        let kinds: Vec<(&str, &str)> = v
            .as_array()
            .unwrap()
            .iter()
            .map(|d| (d["id"].as_str().unwrap(), d["kind"].as_str().unwrap()))
            .collect();
        assert_eq!(
            kinds,
            [
                ("dev-cam", "video"),
                ("dev-loop", "audio"),
                ("dev-usb", "audio")
            ]
        );
        assert_eq!(v[1]["virtual"], false);

        let only_audio = render_devices(&device_report(
            Vec::new(),
            vec![discovered("dev-usb", "USB Microphone", true)],
        ));
        assert!(only_audio.starts_with("Strom reports no video sources"));
        assert!(only_audio.contains("USB Microphone"));
    }

    const CLOUD: RangeInclusive<u16> = 47110..=47129;
    const LOCAL: RangeInclusive<u16> = 9000..=9019;

    fn cloud() -> BTreeSet<u16> {
        CLOUD.collect()
    }

    fn local() -> BTreeSet<u16> {
        LOCAL.collect()
    }

    /// Several venues share the cloud Strom, so the venue must not pick its ports.
    fn stored(address: &str) -> Source {
        Source {
            id: "src-1".to_string(),
            name: "Venue — Cam".to_string(),
            address: address.to_string(),
            stream_type: "srt".to_string(),
            status: "inactive".to_string(),
            latency: None,
            gateway_id: None,
            connect: None,
        }
    }

    #[test]
    fn a_stored_source_keeps_its_port_only_while_open_live_still_holds_it() {
        let range: BTreeSet<u16> = (47100..=47109).collect();
        assert_eq!(plan_port(None, &range), PortPlan::Create);
        assert_eq!(
            plan_port(
                Some(&stored("srt://:47103?mode=listener&passphrase=***")),
                &range
            ),
            PortPlan::Keep {
                source_id: "src-1".to_string(),
                port: 47103
            }
        );
        // Left by a run against the old default range: Open Live would refuse it now.
        assert_eq!(
            plan_port(Some(&stored("srt://:47110?mode=listener")), &range),
            PortPlan::Reassign {
                source_id: "src-1".to_string()
            }
        );
        assert_eq!(
            plan_port(
                Some(&stored("srt://cloud.example.com:47103?mode=caller")),
                &range
            ),
            PortPlan::Reassign {
                source_id: "src-1".to_string()
            }
        );
    }

    #[test]
    fn in_caller_mode_the_published_ports_win_over_the_settings() {
        assert_eq!(
            choose_ports(
                UplinkMode::Caller,
                Some(LOCAL),
                Some(cloud()),
                Some("reserved"),
                &Events::discard()
            )
            .unwrap(),
            (cloud(), PortSource::OpenLive)
        );
        assert_eq!(
            choose_ports(
                UplinkMode::Caller,
                None,
                Some(cloud()),
                Some("reserved"),
                &Events::discard()
            )
            .unwrap(),
            (cloud(), PortSource::OpenLive)
        );
    }

    /// An older Open Live, or one whose Strom hands out no ports, leaves the settings in charge.
    #[test]
    fn in_caller_mode_the_settings_are_the_fallback_when_nothing_is_published() {
        for status in [None, Some("pending"), Some("unsupported"), Some("disabled")] {
            assert_eq!(
                choose_ports(
                    UplinkMode::Caller,
                    Some(LOCAL),
                    None,
                    status,
                    &Events::discard()
                )
                .unwrap(),
                (local(), PortSource::Settings),
                "{status:?}"
            );
        }
    }

    /// Neither side has a range: the error names both fixes, and says when the fix
    /// is simply to wait for Open Live.
    #[test]
    fn in_caller_mode_no_range_at_all_is_an_error_that_names_the_fixes() {
        let err = choose_ports(UplinkMode::Caller, None, None, None, &Events::discard())
            .unwrap_err()
            .to_string();
        assert!(err.contains("uplink.port_range"), "{err}");
        assert!(err.contains("upgrade"), "{err}");
        assert!(!err.contains("waiting"), "{err}");

        let err = choose_ports(
            UplinkMode::Caller,
            None,
            None,
            Some("pending"),
            &Events::discard(),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("uplink.port_range"), "{err}");
        assert!(err.contains("waiting"), "{err}");
        assert!(err.contains("try again"), "{err}");
    }

    /// Listener ports are this machine's own; nothing Open Live publishes applies.
    #[test]
    fn in_listener_mode_only_the_settings_count() {
        assert_eq!(
            choose_ports(
                UplinkMode::Listener,
                Some(LOCAL),
                Some(cloud()),
                Some("leased"),
                &Events::discard()
            )
            .unwrap(),
            (local(), PortSource::Settings)
        );
        let err = choose_ports(
            UplinkMode::Listener,
            None,
            Some(cloud()),
            Some("reserved"),
            &Events::discard(),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("uplink.port_range"), "{err}");
    }

    fn live(
        name: &str,
        state: State,
        source_id: Option<&str>,
        uplink: Option<SrtCallerStats>,
    ) -> Live {
        let uplink_cfg = crate::config::Uplink::default();
        Live {
            input: Input {
                id: format!("dev-{}", name.to_lowercase()),
                name: format!("Venue — {name}"),
                source: flow::Source::Test {
                    resolution: "1280x720".to_string(),
                    framerate: "25/1".to_string(),
                },
                endpoint: uplink_cfg.endpoint("cloud.example.com", 47110),
            },
            flow_id: flow::flow_id("gw", name),
            state,
            shown: None,
            last_bytes: None,
            quiet_polls: 0,
            source_id: source_id.map(str::to_string),
            connect_warning: None,
            uplink,
        }
    }

    /// The heartbeat says what the loop knows, in Strom's flow vocabulary: a flow
    /// that runs is playing whether or not anything receives it, and one that cannot
    /// be confirmed is idle. Only a delivering uplink counts as streaming.
    #[test]
    fn the_heartbeat_snapshot_maps_the_loops_states_and_counts_delivering_inputs() {
        let sample: SrtCallerStats = serde_json::from_value(serde_json::json!({
            "send_rate_mbps": 6.2, "rtt_ms": 18.0, "bytes_sent": 1000
        }))
        .unwrap();
        let live = vec![
            live("A", State::OnAir, Some("src-a"), Some(sample)),
            live("B", State::Waiting, Some("src-b"), None),
            live("C", State::Starting, None, None),
            live("D", State::Unreachable, Some("src-d"), None),
            live(
                "E",
                State::Failed {
                    reason: "boom".to_string(),
                },
                None,
                None,
            ),
        ];
        let snapshot = snapshot_of(&live, "venue-box", Some("0.7.2"));
        assert_eq!(snapshot.host, "venue-box");
        assert_eq!(snapshot.strom_version.as_deref(), Some("0.7.2"));
        assert_eq!(snapshot.ingest_version, env!("CARGO_PKG_VERSION"));
        assert_eq!(snapshot.device_count, 5);
        assert_eq!(snapshot.streaming_count, 1);
        let states: Vec<heartbeat::FlowState> =
            snapshot.inputs.iter().map(|i| i.flow_state).collect();
        use heartbeat::FlowState::{Idle, Playing};
        assert_eq!(states, vec![Playing, Playing, Playing, Idle, Idle]);
        assert_eq!(snapshot.inputs[0].input_id, "dev-a");
        assert_eq!(snapshot.inputs[0].name, "Venue — A");
        assert_eq!(snapshot.inputs[0].source_id.as_deref(), Some("src-a"));
        assert_eq!(
            snapshot.inputs[0].uplink,
            Some(heartbeat::Uplink {
                bitrate_kbps: 6200,
                rtt_ms: 18.0,
                dropped: 0
            })
        );
        assert_eq!(snapshot.inputs[1].uplink, None);
        assert_eq!(snapshot.inputs[2].source_id, None);
    }

    /// The stats event carries the numbers the heartbeat sends, per input, with the
    /// loop's own state rather than the heartbeat's coarser flow state.
    #[test]
    fn the_stats_event_carries_the_heartbeats_uplink_numbers() {
        let sample: SrtCallerStats = serde_json::from_value(serde_json::json!({
            "send_rate_mbps": 6.2, "rtt_ms": 18.0, "bytes_sent": 1000,
            "packets_sent_dropped": 3
        }))
        .unwrap();
        let live = vec![
            live("A", State::OnAir, Some("src-a"), Some(sample)),
            live("B", State::Waiting, None, None),
        ];
        let heartbeat = snapshot_of(&live, "venue-box", None);
        let Event::Stats { inputs } = stats_of(&live) else {
            panic!("not stats");
        };
        assert_eq!(inputs.len(), 2);
        assert_eq!(inputs[0].input, "Venue — A");
        assert_eq!(inputs[0].state, State::OnAir);
        let sent = heartbeat.inputs[0].uplink.as_ref().unwrap();
        assert_eq!(
            inputs[0].uplink,
            Some(UplinkStats {
                bitrate_kbps: sent.bitrate_kbps,
                rtt_ms: sent.rtt_ms,
                dropped: sent.dropped,
            })
        );
        assert_eq!(inputs[0].uplink.unwrap().dropped, 3);
        assert_eq!(inputs[1].state, State::Waiting);
        assert_eq!(inputs[1].uplink, None);
    }

    #[test]
    fn truncation_keeps_names_within_the_column() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(
            truncate("an extremely long device name", 10)
                .chars()
                .count(),
            10
        );
    }

    fn source_at(id: &str, name: &str, port: u16, gateway_id: Option<&str>) -> Source {
        Source {
            id: id.into(),
            name: name.into(),
            address: format!("srt://:{port}?mode=listener"),
            stream_type: "srt".into(),
            status: "inactive".into(),
            latency: Some(200),
            gateway_id: gateway_id.map(str::to_string),
            connect: None,
        }
    }

    /// A source this gateway left on a port a new input wants is ours to remove; one
    /// on that port from another gateway or made by hand is reported, never removed;
    /// one with the input's own name is adopted by the tick, so it is no clash.
    #[test]
    fn port_clashes_tell_our_leftovers_from_other_holders() {
        let prefix = devices::name_prefix("Venue");
        let wanted = vec![("Venue — Test pattern".to_string(), 47110)];

        let leftover = [source_at("s1", "Venue — Camera", 47110, Some("gw-1"))];
        let clashes = port_clashes(&wanted, &leftover, &prefix, Some("gw-1"));
        assert_eq!(
            clashes,
            vec![PortClash {
                input: "Venue — Test pattern".into(),
                port: 47110,
                holder_id: "s1".into(),
                holder_name: "Venue — Camera".into(),
                ours: true,
            }]
        );
        // Untagged on either side: the name prefix decides.
        assert!(port_clashes(&wanted, &leftover, &prefix, None)[0].ours);
        let untagged = [source_at("s1", "Venue — Camera", 47110, None)];
        assert!(port_clashes(&wanted, &untagged, &prefix, Some("gw-1"))[0].ours);

        // Same prefix, another gateway's id: not ours.
        assert!(!port_clashes(&wanted, &leftover, &prefix, Some("gw-2"))[0].ours);
        // Another venue's source, or one made by hand.
        let foreign = [source_at("s2", "Studio B — Camera", 47110, None)];
        assert!(!port_clashes(&wanted, &foreign, &prefix, Some("gw-1"))[0].ours);

        // The input's own source, or one on another port, is no clash.
        let fine = [
            source_at("s3", "Venue — Test pattern", 47110, Some("gw-1")),
            source_at("s4", "Venue — Camera", 47111, Some("gw-1")),
        ];
        assert!(port_clashes(&wanted, &fine, &prefix, Some("gw-1")).is_empty());
    }

    /// `down` finds a running `up` by its command name, so the needle must be what
    /// `ps` prints for this binary: a prefix of the package name, within Linux's
    /// 15-byte limit.
    #[test]
    fn the_gateway_comm_is_the_package_name_cut_to_fifteen_bytes() {
        assert!(env!("CARGO_PKG_NAME").starts_with(GATEWAY_COMM));
        assert!(GATEWAY_COMM.len() <= 15);
        assert_eq!(GATEWAY_COMM, "open-live-inges");
        assert_eq!(truncate_comm("strom"), "strom");
    }

    #[test]
    fn a_running_gateway_is_recognised_on_linux_and_macos() {
        // Linux: the comm truncated to 15 characters, with ps's trailing newline.
        assert!(comm_matches("open-live-inges\n", GATEWAY_COMM));
        // macOS: the full path of the executable.
        assert!(comm_matches(
            "/usr/local/bin/open-live-ingest\n",
            GATEWAY_COMM
        ));
        assert!(!comm_matches("strom\n", GATEWAY_COMM));
        assert!(!comm_matches("", GATEWAY_COMM));
    }

    /// What `tasklist /FI "PID eq <pid>" /FO CSV /NH` prints, parsed on every OS.
    #[test]
    fn a_running_gateway_and_strom_are_recognised_on_windows() {
        let gateway = "\"open-live-ingest.exe\",\"4242\",\"Console\",\"1\",\"12,345 K\"\r\n";
        let image = parse_tasklist_csv(gateway, 4242).expect("a row for pid 4242");
        assert_eq!(image, "open-live-ingest.exe");
        assert!(image_matches(&image, GATEWAY_COMM));
        assert!(!image_matches(&image, "strom"));

        let strom = "\"Strom.EXE\",\"77\",\"Services\",\"0\",\"51,204 K\"\r\n";
        let image = parse_tasklist_csv(strom, 77).expect("a row for pid 77");
        assert!(image_matches(&image, "strom"));
        assert!(!image_matches(&image, GATEWAY_COMM));
        assert!(!image_matches(&image, ""));
    }

    /// A stale pid must never match an unrelated process: no row, a localised
    /// "no tasks" message, or a row for another pid all mean "not running".
    #[test]
    fn a_tasklist_row_for_another_pid_or_no_row_is_not_a_match() {
        assert_eq!(parse_tasklist_csv("", 4242), None);
        assert_eq!(
            parse_tasklist_csv(
                "INFO: No tasks are running which match the specified criteria.\r\n",
                4242
            ),
            None
        );
        assert_eq!(
            parse_tasklist_csv(
                "INFORMATION: Det finns inga aktiviteter som matchar angivna villkor.\r\n",
                4242
            ),
            None
        );
        // 42 is a prefix of 4242, and 42424 contains it: neither is the same pid.
        assert_eq!(
            parse_tasklist_csv("\"strom.exe\",\"42424\",\"Console\",\"1\",\"1 K\"", 4242),
            None
        );
        assert_eq!(
            parse_tasklist_csv("\"strom.exe\",\"4242\",\"Console\",\"1\",\"1 K\"", 42),
            None
        );
    }

    #[test]
    fn tasklist_csv_fields_are_unquoted() {
        assert_eq!(
            csv_fields(r#""a b.exe","1","x ""y""","2,5 K""#),
            Some(vec![
                "a b.exe".to_string(),
                "1".to_string(),
                "x \"y\"".to_string(),
                "2,5 K".to_string()
            ])
        );
        assert_eq!(csv_fields("INFO: nothing"), None);
        assert_eq!(csv_fields(r#""unterminated"#), None);
        assert_eq!(csv_fields(r#""a"b"#), None);
    }

    /// The real thing, on the windows-latest CI leg. `ping` is started directly
    /// rather than through `cmd /c`, so the pid is ping.exe's own and not cmd.exe's.
    /// The `Child` is dropped at once, so this test holds no handle that could keep
    /// the process object around after it is ended.
    #[cfg(windows)]
    #[test]
    fn a_windows_process_is_seen_named_and_stopped() {
        let pid = std::process::Command::new("ping")
            .args(["-n", "30", "127.0.0.1"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("starting ping")
            .id();
        assert!(process_alive(pid));
        assert!(process_named(pid, "ping"));
        assert!(process_named(pid, "PING.EXE"));
        assert!(!process_named(pid, "strom"));

        signal_stop(pid);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while process_alive(pid) && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(
            !process_alive(pid),
            "pid {pid} is still running after signal_stop"
        );
        assert!(!process_named(pid, "ping"));
    }
}
