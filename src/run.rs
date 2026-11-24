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
use crate::credential::OscCredential;
use crate::devices;
use crate::flow::{self, Input};
use crate::gate::{self, Verdict};
use crate::gateway::{Event, Events, InputState as State, InputStats, SummaryRow, UplinkStats};
use crate::heartbeat::{self, Heartbeat};
use crate::local_strom::{self, LocalStrom};
use crate::media::{Media, ResolvedInputs, StromMedia};
use crate::openlive::{
    drifted, listener_port, OpenLiveClient, Refused, ServerInfo, Source, SourcePayload,
};
use crate::session::Inputs;
use crate::strom::StromClient;
use anyhow::{bail, Context, Result};
use serde::Serialize;
use std::collections::{BTreeSet, HashMap};
use std::fmt;
use std::future::Future;
use std::ops::RangeInclusive;
use std::path::Path;
use std::time::Duration;
use strom_types::api::SrtCallerStats;
use strom_types::discovery::DeviceResponse;
use strom_types::FlowId;
use tokio::time::Instant;
use tracing::{debug, info, warn};

const TICK: Duration = Duration::from_secs(10);

/// How often an armed session (`Sending::WhileLive`) asks Open Live for its
/// productions: well inside the 5 s from a production going live to its uplink
/// starting that #126 AC1 asks for.
const GATE_POLL: Duration = Duration::from_secs(2);

/// The same while a production holding one of the sources is activating, which on
/// Open Live takes well under a second.
const GATE_POLL_ACTIVATING: Duration = Duration::from_secs(1);

/// Quiet polls before an uplink that was delivering is reported inactive.
/// Hysteresis: every status flip rewrites the Open Live source, and CouchDB keeps a
/// revision per write, so one quiet poll must not flap the verdict.
const QUIET_TOLERANCE: u32 = 3;

/// Stalled polls in a row (see [`judge`]) before the first restart of a flow (#111).
/// Two ticks: a short loss burst at connect, which SRT repairs with retransmissions
/// rather than drops, is over by then; a receiver that has stopped acknowledging is
/// not. The first sample after a connect has no baseline, so a connection that stalls
/// from the start is restarted about three ticks (30 s) after it connected.
const STALLED_POLLS: u32 = 2;

/// The longest run of stalled polls waited for before a restart. Each restart that
/// does not bring a delivering interval back doubles the wait (20 s, 40 s, 80 s,
/// 160 s), up to this: three minutes.
const MAX_STALLED_POLLS: u32 = 18;

/// Restarts in a row that never reached a delivering interval before the gateway
/// gives up restarting. A receiver that is broken for good would otherwise have the
/// camera, the encoder and the receiver's queue reset for as long as `up` runs. After
/// this the input stays Waiting, with one warning, until it delivers again.
const MAX_RESTARTS: u32 = 4;

/// Packets a poll interval must have sent to be judged at all, so a near-idle sample
/// (a flow still starting) never counts.
const STALL_MIN_SENT: u64 = 500;

/// The share of an interval's packets the sender must have dropped for it to count
/// as stalled. #111's stall drops all of them ("dropped ≈ sent"); a congested uplink
/// with less capacity than the stream can drop a large share and still deliver a
/// degraded, live picture, and must not be torn down.
const STALL_DROP_PERCENT: u64 = 90;

/// How long an uplink the gateway started may dial with no peer before a connect
/// counts as late (#111). A late first connect into an Open Live production's Strom
/// (an `mpegtssrt_input` whose receiver came up after the caller started dialling)
/// stalls (Eyevinn/strom#1277), while a connect into a receiver that is already
/// running is clean; so a late first connect is restarted once, at once, rather than
/// after [`STALLED_POLLS`] stalled polls. A receiver that is up connects within a
/// second or two of the flow starting.
pub const LATE_CONNECT_AFTER: Duration = Duration::from_secs(5);

/// How often an uplink that is dialling with no peer yet is looked at, so a late
/// first connect is restarted within a second of the peer appearing, before the
/// receiver's stall sets in (drops start within about 20 s).
pub const CONNECT_POLL: Duration = Duration::from_secs(1);

/// Where the receiver-side fault behind a stall is tracked.
const STALL_UPSTREAM_ISSUE: &str = "https://github.com/Eyevinn/strom/issues/1277";

/// The local Strom, adopted or started, and a client for it. Held for as long as the
/// command runs: dropping it stops a Strom we started.
pub(crate) async fn connect_strom(
    cfg: &Config,
    events: &Events,
) -> Result<(LocalStrom, StromClient)> {
    let local = local_strom::ensure_running(cfg, events).await?;
    Ok((
        local,
        StromClient::new(&cfg.strom.url, cfg.strom.api_key.as_deref())?,
    ))
}

/// The Open Live client `up` registers with: with the app's Open Source Cloud
/// credential when one is given, otherwise with the one the settings point at. None
/// when the settings say not to register with Open Live.
pub fn open_live_client(
    cfg: &Config,
    credential: Option<&OscCredential>,
) -> Result<Option<OpenLiveClient>> {
    if !cfg.open_live.register {
        return Ok(None);
    }
    Ok(Some(match credential {
        Some(credential) => OpenLiveClient::with_osc_credential(&cfg.open_live, credential)?,
        None => OpenLiveClient::from_settings(&cfg.open_live)?,
    }))
}

/// Before going live: a credential that cannot be renewed and ends during the
/// session is said now, while signing in again is still easy (#65 AC2).
fn warn_of_sign_in_end(open_live: Option<&OpenLiveClient>, events: &Events) {
    let Some(at) =
        open_live.and_then(|c| c.osc_sign_in_ends_within(crate::credential::SESSION_HORIZON))
    else {
        return;
    };
    let left = Duration::from_secs(at.saturating_sub(crate::osc::now().as_secs()));
    warn!(
        "the Open Source Cloud credential ends in {} minute(s) and the gateway cannot renew it itself; after that Open Live refuses this gateway's requests, though the feeds keep running. To avoid it, {}",
        left.as_secs() / 60,
        crate::credential::SIGN_IN_AGAIN
    );
    events.emit(Event::OscSignInEnds { at });
}

/// The gateway id to tag registered sources with, so Open Live's forget-gateway
/// cascade removes the right sources and Studio's Sources chip can attribute them.
/// Present only when the heartbeat is configured; otherwise sources go untagged.
fn source_gateway_id(cfg: &Config) -> Result<Option<String>> {
    Ok(heartbeat::Target::from_config(&cfg.open_live)?.map(|t| t.gateway_id))
}

/// Where feeds are sent, and which ports the links use.
struct Cloud {
    /// The cloud Strom, or through a relay the relay's public host when Open Live
    /// reports it (empty when not).
    host: String,
    /// Empty through a relay: it needs no port of the cloud Strom.
    ports: BTreeSet<u16>,
    port_source: PortSource,
    /// Set when the links go through Open Live's SRT relay.
    relay: Option<CloudRelay>,
}

/// The SRT relay the links go through.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CloudRelay {
    name: String,
    /// Its public `host:port`, as Open Live reports it, when it does.
    public: Option<String>,
}

impl CloudRelay {
    /// The host in [`CloudRelay::public`], for the links until Open Live has given
    /// each its dial-in address.
    fn host(&self) -> String {
        let public = self.public.as_deref().unwrap_or_default();
        let public = public.strip_prefix("srt://").unwrap_or(public);
        match public.rsplit_once(':') {
            Some((host, port)) if port.parse::<u16>().is_ok() => host
                .trim_start_matches('[')
                .trim_end_matches(']')
                .to_string(),
            _ => String::new(),
        }
    }
}

/// Who chose the port range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PortSource {
    /// Published by Open Live: the range its Strom has leased for callers.
    OpenLive,
    /// `uplink.port_range` in the settings.
    Settings,
    /// None: the links go through Open Live's SRT relay.
    Relay,
}

impl fmt::Display for PortSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            PortSource::OpenLive => "Open Live",
            PortSource::Settings => "the settings",
            PortSource::Relay => "Open Live's SRT relay",
        })
    }
}

/// Resolves the cloud host, configured or asked of Open Live, and the port range,
/// or the SRT relay the links go through instead ([`choose_relay`]). Open Live is
/// asked whenever it can be and something is needed from it: the host, or in caller
/// and relay mode its relays and its Strom's ports. A configured host together with
/// a configured range still starts when Open Live is down, as it always has; the
/// sources are registered once it is back. `dials_relay` is whether the media can
/// send through a relay at all.
async fn resolve_cloud(
    cfg: &Config,
    open_live: Option<&OpenLiveClient>,
    dials_relay: bool,
    events: &Events,
) -> Result<Cloud> {
    let configured_host = cfg
        .uplink
        .host
        .as_deref()
        .map(str::trim)
        .filter(|h| !h.is_empty());
    let local = cfg.uplink.local_ports()?;
    let needs_open_live = configured_host.is_none() || cfg.uplink.mode != UplinkMode::Listener;
    let info: Option<ServerInfo> = match open_live {
        Some(client) if needs_open_live => match client.server_info().await {
            Ok(info) => info,
            Err(err)
                if configured_host.is_some()
                    && local.is_some()
                    && cfg.uplink.mode != UplinkMode::Relay =>
            {
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

    if let Some(relay) = choose_relay(
        cfg.uplink.mode,
        cfg.uplink.relay_name(),
        open_live.is_some(),
        dials_relay,
        info.as_ref(),
    )? {
        return Ok(Cloud {
            host: relay.host(),
            ports: BTreeSet::new(),
            port_source: PortSource::Relay,
            relay: Some(relay),
        });
    }

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
        relay: None,
    })
}

/// Whether the links go through Open Live's SRT relay, and which. Pure, so the
/// choice is tested without a server.
///
/// - `listener`: never.
/// - `caller` (the default, and what every settings file before relays says):
///   through the relay when Open Live reports at least one, the media can send
///   through one, and the sources are registered (only Open Live can give the
///   address); otherwise to a port on the cloud Strom, exactly as before relays.
/// - `relay`: always, and an error when that cannot be.
///
/// The relay is `name` (`uplink.relay`, or `default`). One Open Live does not report
/// is an error naming those it does, never a quiet switch to another mode.
fn choose_relay(
    mode: UplinkMode,
    name: &str,
    registering: bool,
    dials_relay: bool,
    info: Option<&ServerInfo>,
) -> Result<Option<CloudRelay>> {
    let relays = info.map(ServerInfo::relays).unwrap_or_default();
    match mode {
        UplinkMode::Listener => return Ok(None),
        UplinkMode::Caller if !(registering && dials_relay && !relays.is_empty()) => {
            return Ok(None)
        }
        UplinkMode::Caller => {}
        UplinkMode::Relay => {
            if !registering {
                bail!("uplink.mode is \"relay\", whose address only Open Live can give, and registration is off");
            }
            if !dials_relay {
                bail!("uplink.mode is \"relay\", and this device cannot send through an SRT relay yet; set uplink.mode = \"caller\"");
            }
            if relays.is_empty() {
                let why = match info.and_then(|i| i.srt.as_ref()) {
                    None => "Open Live is older than SRT relays",
                    Some(srt) if srt.state.as_deref() == Some("unreachable") => {
                        "Open Live cannot reach its Strom to ask about SRT relays"
                    }
                    Some(_) => "Open Live reports no SRT relay",
                };
                bail!("uplink.mode is \"relay\", but {why}; set uplink.mode = \"caller\" to send to a port on its Strom instead");
            }
        }
    }
    let Some(relay) = relays.iter().find(|r| r.name == name) else {
        let names: Vec<&str> = relays.iter().map(|r| r.name.as_str()).collect();
        bail!(
            "Open Live has no SRT relay named {name:?}; it has {}: set uplink.relay to one of them",
            names.join(", ")
        );
    };
    Ok(Some(CloudRelay {
        name: relay.name.clone(),
        public: Some(relay.public.trim().to_string()).filter(|p| !p.is_empty()),
    }))
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
        // A relay link never gets here: `choose_relay` answers for it or refuses.
        UplinkMode::Caller | UplinkMode::Relay => match (published, local) {
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
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
    /// The uplink connection's packet counters at the last poll, to judge the next
    /// interval against.
    last_counters: Option<Counters>,
    /// Polls in a row whose interval was [`Interval::Stalled`].
    stalled_polls: u32,
    /// Restarts for a stall since the uplink last delivered. Survives the restart
    /// itself, so the wait before the next one can grow and the count can be capped.
    stall_restarts: u32,
    /// Whether the gateway has given up restarting this input's stalled uplink, so the
    /// warning saying so is emitted once.
    stall_given_up: bool,
    /// When the gateway last started this input's uplink, while it has had no peer
    /// since: the late-connect watch ([`LATE_CONNECT_AFTER`]). Cleared at the first
    /// peer, so a reconnect after that is never taken for a late first connect.
    dialling_since: Option<Instant>,
    source_id: Option<String>,
    /// The `connect.warning` Open Live last reported for the source, so one that
    /// persists is logged once per run rather than every tick.
    connect_warning: Option<String>,
    /// The last SRT sample with a peer, for the heartbeat. None while nothing receives.
    uplink: Option<SrtCallerStats>,
    /// Whether the input's flow is meant to run. Always, unless the session is armed
    /// (`Sending::WhileLive`): then only while a production holding its source is live.
    sending: bool,
    /// Why the last start of an armed input's uplink failed, so a failure that
    /// persists is logged and told once; it is retried at every look.
    gate_error: Option<String>,
    /// The live productions last told (`Event::SlotAddedWhileLive`) to have been
    /// given the source while live.
    added_while_live: Vec<String>,
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
            State::Armed | State::Unreachable | State::Failed { .. } => heartbeat::FlowState::Idle,
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
            heartbeat: Heartbeat::start(target, initial, events.clone()),
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
                relay: l.input.endpoint.relay_name().map(str::to_string),
                dial_in: l.input.endpoint.dial_in().map(str::to_string),
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
                flow_id: l.flow_id,
                source_id: l.source_id.clone(),
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

/// The passphrase for links through the relay when the settings have none. The
/// relay's port is public, so without one anyone who learns a stream id could send
/// or pull it. One is generated and kept in the settings file the settings came from
/// ([`config::keep_passphrase`]), so the next run, and Open Live's source, use the
/// same one. Should it not be kept, it still protects this run, and the next one
/// generates another and writes it to the source when it registers.
fn relay_passphrase(cfg: &Config, events: &Events) -> Result<String> {
    let generated = config::generate_passphrase()?;
    let Some(file) = &cfg.file else {
        warn!("generated an SRT passphrase for the relay for this run only: these settings were not loaded from a file to keep it in");
        events.emit(Event::PassphraseGenerated { kept_in: None });
        return Ok(generated);
    };
    match config::keep_passphrase(file, &generated) {
        Ok(kept) => {
            info!(settings = %file.display(), "generated an SRT passphrase for the relay and kept it in the settings");
            events.emit(Event::PassphraseGenerated {
                kept_in: Some(file.clone()),
            });
            Ok(kept)
        }
        Err(err) => {
            warn!(err = %format_args!("{err:#}"), "could not keep the generated SRT passphrase in the settings; it is used for this run only");
            events.emit(Event::Warning {
                message: format!(
                    "could not keep the generated SRT passphrase in {} ({err:#}); it is used for this run only, and the next run generates another",
                    file.display()
                ),
            });
            events.emit(Event::PassphraseGenerated { kept_in: None });
            Ok(generated)
        }
    }
}

/// [`up`] with the media done by the local Strom ([`StromMedia`]), as the CLI and the
/// desktop app run it. The arguments are checked first, so a rejected one never
/// starts Strom; then a second live gateway is refused ([`AlreadyRunning`]) and this
/// one recorded in the pidfile, and Strom is adopted or started. Only a Strom started
/// here is stopped on the way out, then the pidfile is removed.
pub async fn up_on_strom(
    cfg: Config,
    credential: Option<OscCredential>,
    inputs: Inputs,
    (sources, sending): (Sources, Sending),
    events: &Events,
    stop: impl Future<Output = Stop>,
) -> Result<()> {
    check_arguments(&cfg, &inputs, sending)?;
    let media = StromMedia::start(&cfg, events).await?;
    let outcome = up(
        &media,
        cfg,
        credential,
        inputs,
        (sources, sending),
        events,
        stop,
    )
    .await;
    // Only a Strom we started is stopped here; an adopted one is left running. The
    // pidfile goes after it.
    drop(media);
    outcome
}

/// Refuses what no session can do, before anything is started.
fn check_arguments(cfg: &Config, inputs: &Inputs, sending: Sending) -> Result<()> {
    if inputs.test_pattern && !inputs.audio.is_empty() {
        bail!("--audio pairs an audio source with a camera, and --test streams no camera; the test pattern carries its own tone");
    }
    if sending == Sending::WhileLive && !cfg.open_live.register {
        bail!("sending only while a production is live needs registration with Open Live, and [open_live] register is off");
    }
    Ok(())
}

/// Brings every chosen input up through `media` and keeps it registered until `stop`
/// resolves, then removes what it started: the uplinks always, the Open Live sources
/// as `sources` says. `sending` says whether each input's uplink runs all along or
/// only while a production holding its source is live. `stop` says why it is
/// stopping, which decides how long the teardown may take. `credential`, when given,
/// is the Open Source Cloud credential to register with instead of the one the
/// settings point at. Whatever `media` holds (a Strom, a pidfile) is the caller's.
pub async fn up<M: Media>(
    media: &M,
    cfg: Config,
    credential: Option<OscCredential>,
    inputs: Inputs,
    (sources, sending): (Sources, Sending),
    events: &Events,
    stop: impl Future<Output = Stop>,
) -> Result<()> {
    check_arguments(&cfg, &inputs, sending)?;
    let gateway_id = cfg.gateway.resolved_id();
    let source_gateway_id = source_gateway_id(&cfg)?;
    let strom_version = media.version().await;
    let open_live = open_live_client(&cfg, credential.as_ref())?;
    warn_of_sign_in_end(open_live.as_ref(), events);

    let ResolvedInputs {
        cameras: chosen,
        test_pattern,
        audio,
    } = media.resolve_inputs(&cfg, inputs, events).await?;

    // Without somewhere to send there is no point starting: the feeds would go nowhere.
    let cloud = resolve_cloud(&cfg, open_live.as_ref(), media.dials_relay(), events).await?;
    let mut uplink = cfg.uplink.clone();
    if let Some(relay) = &cloud.relay {
        uplink.mode = UplinkMode::Relay;
        uplink.relay = Some(relay.name.clone());
        if uplink.passphrase.as_deref().is_none_or(str::is_empty) {
            uplink.passphrase = Some(relay_passphrase(&cfg, events)?);
        }
    }
    let mut inputs = devices::inputs_for(
        &chosen,
        test_pattern,
        &cfg.gateway.name,
        &uplink,
        &cfg.capture,
        &cloud.host,
        &cloud.ports,
    )?;
    devices::attach_audio(&mut inputs, &audio);
    devices::apply_labels(&mut inputs, &cfg.inputs, &chosen, &cfg.gateway.name);
    // Sources are found by name: two inputs with one name would share a source.
    devices::check_unique_names(&inputs)?;

    // Each input costs an encoder and its share of the uplink, which is easy to
    // overlook when one command starts all of them.
    let total_mbps = inputs.len() as f64 * cfg.video.bitrate_kbps as f64 / 1000.0;
    events.emit(match &cloud.relay {
        Some(relay) => Event::BringingUpViaRelay {
            inputs: inputs.len(),
            relay: relay.name.clone(),
            public: relay.public.clone(),
            total_mbps,
        },
        None => Event::BringingUp {
            inputs: inputs.len(),
            host: cloud.host.clone(),
            ports: format_ports(&cloud.ports),
            port_source: cloud.port_source.to_string(),
            total_mbps,
        },
    });

    // From here on sources may be registered with Open Live before any flow runs.
    // A startup that fails after that removes them again before returning the
    // error, so a failed start leaves nothing behind (what `Event::Failed`
    // promises), unless the sources are kept. Flows need nothing extra: `start`
    // removes one that does not come up.
    let mut registered = HashMap::new();
    // Every input of this run, started or not: the sources a stop keeps (`Sources::Keep`).
    let names: BTreeSet<String> = inputs.iter().map(|i| i.name.clone()).collect();
    let started = start_inputs(
        &cfg,
        media,
        open_live.as_ref(),
        &cloud,
        &gateway_id,
        source_gateway_id.as_deref(),
        (inputs, sending),
        &mut registered,
        events,
    )
    .await;
    let mut live = match started {
        Ok(live) => live,
        Err(err) => {
            if let Some(client) = &open_live {
                match sources {
                    Sources::Remove => remove_registered(client, &registered, events).await,
                    Sources::Keep => keep_registered(&registered, events),
                }
            }
            return Err(err);
        }
    };

    // Armed inputs say so before the summary, so a row never reads as connecting.
    for l in live.iter_mut().filter(|l| l.state == State::Armed) {
        events.emit(Event::Input {
            input: l.input.name.clone(),
            state: State::Armed,
        });
        l.shown = Some(State::Armed);
    }
    events.emit(summary(&live));
    let reporter = Reporter::start(&cfg, strom_version, &live, events)?;

    let (outcome, stop) = match run_until_stopped(
        media,
        open_live.as_ref(),
        reporter.as_ref(),
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
            teardown(
                media,
                open_live.as_ref(),
                &cfg.gateway.name,
                &live,
                (sources, &names),
                events,
            )
            .await;
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
                    media,
                    open_live.as_ref(),
                    &cfg.gateway.name,
                    &live,
                    (sources, &names),
                    deadline,
                    events
                )
            );
        }
    }
    outcome
}

/// Registers the inputs (when the ports are Open Live's), clears what stands in their
/// way, and starts a flow for each, unless `sending` arms them instead: then no flow
/// starts here, and each input is [`State::Armed`] until a production holding its
/// source is live. Every source registered on the way is recorded in `registered`
/// as soon as it exists, so that the caller can remove them all again when this
/// fails, part way through or with no input started.
#[allow(clippy::too_many_arguments)]
async fn start_inputs<M: Media>(
    cfg: &Config,
    media: &M,
    open_live: Option<&OpenLiveClient>,
    cloud: &Cloud,
    gateway_id: &str,
    source_gateway_id: Option<&str>,
    (mut inputs, sending): (Vec<Input>, Sending),
    registered: &mut HashMap<String, Registered>,
    events: &Events,
) -> Result<Vec<Live>> {
    // Through a relay, Open Live gives each source its stream id and the address to
    // dial: register first, then build the flows to that address.
    if let (Some(client), PortSource::Relay) = (open_live, cloud.port_source) {
        inputs =
            register_through_relay(client, inputs, source_gateway_id, registered, events).await?;
    }

    // When the range is Open Live's, so is the port inside it: register first and
    // take the port Open Live wrote into the source, then build the flows to it.
    if let (Some(client), PortSource::OpenLive) = (open_live, cloud.port_source) {
        assign_ports_from_open_live(
            client,
            &mut inputs,
            &cloud.ports,
            source_gateway_id,
            registered,
            events,
        )
        .await?;
    }

    // When the ports are the settings', nothing stops a source from an earlier run
    // still holding one, and Open Live refuses every registration on it.
    if let (Some(client), PortSource::Settings) = (open_live, cloud.port_source) {
        free_held_ports(
            client,
            &inputs,
            &cfg.gateway.name,
            source_gateway_id,
            events,
        )
        .await?;
    }

    media.prepare(&inputs, events).await?;

    let mut live = Vec::new();
    for input in inputs {
        let flow_id = flow::flow_id(gateway_id, &input.id);
        // Looked up, not taken: until startup has succeeded, `registered` is what a
        // failure removes again.
        let (source_id, connect_warning) = match registered.get(&input.id) {
            Some(r) => (Some(r.source_id.clone()), r.warning.clone()),
            None => (None, None),
        };
        let armed = sending == Sending::WhileLive;
        let started = if armed {
            Ok(())
        } else {
            media.start(&input, &cfg.video).await
        };
        match started {
            Ok(()) => live.push(Live {
                input,
                flow_id,
                state: if armed { State::Armed } else { State::Starting },
                shown: None,
                last_bytes: None,
                quiet_polls: 0,
                last_counters: None,
                stalled_polls: 0,
                stall_restarts: 0,
                stall_given_up: false,
                dialling_since: (!armed).then(Instant::now),
                source_id,
                connect_warning,
                uplink: None,
                sending: !armed,
                gate_error: None,
                added_while_live: Vec::new(),
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
    Ok(live)
}

/// Removes the sources a failed startup registered, and says so for each. Open Live
/// refusing (a source of an earlier run held by an active production) is reported as
/// `SourceKept`, as at teardown; anything else as a warning. `down` finds the rest
/// by name prefix.
async fn remove_registered(
    client: &OpenLiveClient,
    registered: &HashMap<String, Registered>,
    events: &Events,
) {
    let mut sources: Vec<&Registered> = registered.values().collect();
    sources.sort_by(|a, b| a.input.cmp(&b.input));
    for r in sources {
        if remove_source(client, &r.source_id, &r.input, events).await {
            events.emit(Event::SourceRemoved {
                input: r.input.clone(),
            });
        }
    }
}

/// A failed startup that keeps its sources: says so for each it registered, so the
/// next run adopts them by name.
fn keep_registered(registered: &HashMap<String, Registered>, events: &Events) {
    let mut sources: Vec<&Registered> = registered.values().collect();
    sources.sort_by(|a, b| a.input.cmp(&b.input));
    for r in sources {
        events.emit(Event::SourceRetained {
            input: r.input.clone(),
        });
    }
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
/// cloud side stays stable across restarts. Records what each input registered as,
/// by input id, in `ids`, each as soon as it exists: on an error part way through,
/// `ids` holds every source registered before it.
async fn assign_ports_from_open_live(
    client: &OpenLiveClient,
    inputs: &mut [Input],
    ports: &BTreeSet<u16>,
    gateway_id: Option<&str>,
    ids: &mut HashMap<String, Registered>,
    events: &Events,
) -> Result<()> {
    let sources = client
        .list_sources()
        .await
        .context("listing Open Live's sources to assign ports")?;
    for input in inputs.iter_mut() {
        let existing = sources.iter().find(|s| s.name == input.name);
        let mut warning = None;
        let (source_id, address) = match plan_port(existing, ports) {
            PortPlan::Keep { source_id, port } => {
                info!(input = %input.name, source_id = %source_id, port, "adopted the source Open Live already holds, same id and port");
                events.emit(Event::SourceAdopted {
                    input: input.name.clone(),
                    source_id: source_id.clone(),
                    port,
                });
                if let Some(stored) = existing {
                    note_connect_warning(&input.name, stored, &mut warning, false, events);
                }
                input.endpoint.port = port;
                ids.insert(
                    input.id.clone(),
                    Registered {
                        input: input.name.clone(),
                        source_id,
                        warning,
                    },
                );
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
        ids.insert(
            input.id.clone(),
            Registered {
                input: input.name.clone(),
                source_id,
                warning,
            },
        );
        let port = listener_port(&address).with_context(|| {
            format!(
                "Open Live stored {} for {:?}, which names no listener port; is it running a version that assigns ports?",
                mask_passphrase(&address),
                input.name
            )
        })?;
        input.endpoint.port = port;
    }
    Ok(())
}

/// Registers each input's source through the relay and takes the address to dial
/// from Open Live's answer (`connect.uri`). No port is leased. A source Open Live
/// already holds under the input's name is adopted with a PATCH, keeping its id: a
/// source of an earlier relay run keeps its stream id too (`auto` keeps it), and one
/// left in caller mode by an older gateway is moved to the relay. Every source is
/// written, so it carries this run's passphrase. An input Open Live gives no address
/// for is reported with `connect.reason` and left out; the others carry on. Returns
/// the inputs ready to send. Records each source in `ids` as soon as it exists.
async fn register_through_relay(
    client: &OpenLiveClient,
    inputs: Vec<Input>,
    gateway_id: Option<&str>,
    ids: &mut HashMap<String, Registered>,
    events: &Events,
) -> Result<Vec<Input>> {
    let sources = client
        .list_sources()
        .await
        .context("listing Open Live's sources to register them through the SRT relay")?;
    let mut ready = Vec::with_capacity(inputs.len());
    for mut input in inputs {
        let existing = sources.iter().find(|s| s.name == input.name);
        events.emit(Event::Registering {
            input: input.name.clone(),
        });
        let payload = SourcePayload::new(
            &input.name,
            &input.endpoint.cloud_uri(),
            false,
            input.endpoint.latency_ms,
            gateway_id,
        );
        let written = match existing {
            Some(stored) => client.patch_source(&stored.id, &payload).await,
            None => client.create_source(&payload).await,
        }
        .map_err(|err| relay_refusal(err, &input.name))?;
        let mut warning = None;
        note_connect_warning(&input.name, &written, &mut warning, true, events);
        ids.insert(
            input.id.clone(),
            Registered {
                input: input.name.clone(),
                source_id: written.id.clone(),
                warning,
            },
        );
        let dial_in = written.relay_dial_in();
        match existing {
            Some(_) => {
                info!(input = %input.name, source_id = %written.id, "adopted the source Open Live already holds, same id, through the SRT relay");
                events.emit(Event::SourceAdopted {
                    input: input.name.clone(),
                    source_id: written.id.clone(),
                    port: dial_in.as_ref().map(|d| d.port).unwrap_or(0),
                });
            }
            None => {
                info!(input = %input.name, source_id = %written.id, "registered the source with Open Live, through the SRT relay");
                events.emit(Event::Registered {
                    input: input.name.clone(),
                    source_id: written.id.clone(),
                });
            }
        }
        match dial_in {
            Ok(d) => {
                input
                    .endpoint
                    .dial_relay(&d.stream_id, &d.uri, &d.host, d.port);
                ready.push(input);
            }
            Err(reason) => {
                warn!(input = %input.name, reason = %reason, "Open Live registered the source but gave no address to send it through the relay");
                events.emit(Event::InputFailed {
                    input: input.name.clone(),
                    reason,
                });
            }
        }
    }
    Ok(ready)
}

/// A failed relay registration, worded for the operator: Open Live's refusal and,
/// for a relay it does not have (422 with `code`), the relays it does.
fn relay_refusal(err: anyhow::Error, input: &str) -> anyhow::Error {
    let refused = err.downcast_ref::<Refused>();
    if let Some((reason, advice)) = refused.and_then(|r| Some((r.explain(), r.relay_advice()?))) {
        return anyhow::anyhow!(
            "Open Live would not register {input:?} through the SRT relay: {reason}. {advice}"
        );
    }
    err.context(format!(
        "registering {input:?} with Open Live through the SRT relay"
    ))
}

/// What registering an input with Open Live left behind.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Registered {
    /// The input's name, which is also the source's.
    input: String,
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

pub(crate) async fn remove_flow(strom: &StromClient, flow_id: &FlowId) -> Result<()> {
    strom.stop_flow(flow_id).await.ok();
    strom.delete_flow(flow_id).await
}

/// The inputs that started, with the address each registered and the address its
/// uplink goes to, passphrases masked.
fn summary(live: &[Live]) -> Event {
    Event::Summary {
        inputs: live
            .iter()
            .map(|l| SummaryRow {
                name: l.input.name.clone(),
                port: l.input.endpoint.port,
                address: mask_passphrase(&l.input.endpoint.cloud_uri()),
                uplink: mask_passphrase(&l.input.endpoint.venue_uri()),
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
/// task per input, and one Open Live list call per tick. An armed session also looks
/// at the productions every [`GATE_POLL`], and starts or stops each input's uplink
/// as they say ([`look_at_productions`]).
// Called once, from `up`, with what `up` holds; a struct to carry them buys nothing.
#[allow(clippy::too_many_arguments)]
async fn run_until_stopped<M: Media>(
    media: &M,
    open_live: Option<&OpenLiveClient>,
    reporter: Option<&Reporter>,
    source_gateway_id: Option<&str>,
    cfg: &Config,
    live: &mut [Live],
    events: &Events,
    stop: impl Future<Output = Stop>,
) -> Result<Stop> {
    let mut ticker = tokio::time::interval(TICK);
    tokio::pin!(stop);
    // Armed inputs are those not sending from the start; `up` refuses to arm without
    // an Open Live client.
    let gated = open_live.filter(|_| live.iter().any(|l| !l.sending));
    let mut watch = gate::Watch::new();
    let mut unreadable = false;
    let next_look = tokio::time::sleep(Duration::ZERO);
    tokio::pin!(next_look);
    let next_watch = tokio::time::sleep(CONNECT_POLL);
    tokio::pin!(next_watch);
    loop {
        let dialling = live.iter().any(|l| l.sending && l.dialling_since.is_some());
        tokio::select! {
            () = &mut next_look, if gated.is_some() => {
                let client = gated.expect("guarded");
                let sending = live.iter().filter(|l| l.sending).count();
                let soon = look_at_productions(
                    media,
                    client,
                    &cfg.video,
                    live,
                    (&mut watch, &mut unreadable),
                    events,
                )
                .await;
                // Held back (a phone going to the background): the stopped sources are
                // marked inactive now, not at the next tick, as iOS gives the app 4 s.
                if media.held_back() && live.iter().filter(|l| l.sending).count() < sending {
                    update_sources(client, live, source_gateway_id, events).await;
                }
                if let Some(reporter) = reporter {
                    reporter.publish(live);
                }
                tell_changes(live, events);
                let wait = if soon { GATE_POLL_ACTIVATING } else { GATE_POLL };
                next_look.as_mut().reset(tokio::time::Instant::now() + wait);
            }
            _ = ticker.tick() => {
                // An armed input with no production live has no flow to look at.
                for l in live.iter_mut().filter(|l| l.sending) {
                    l.state = match poll_input(media, &cfg.video, l, events).await {
                        Ok(state) => state,
                        Err(err) => {
                            warn!(input = %l.input.name, err = %format_args!("{err:#}"), "lost contact with Strom, retrying next tick");
                            l.last_bytes = None;
                            l.uplink = None;
                            l.forget_counters();
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
                    update_sources(client, live, source_gateway_id, events).await;
                }
                tell_changes(live, events);
                events.emit(stats_of(live));
            }
            // An uplink dialling with no peer yet: looked at every second, so a late
            // first connect is restarted before the receiver stalls (#111).
            () = &mut next_watch, if dialling => {
                let mut changed = false;
                for l in live.iter_mut().filter(|l| l.sending && l.dialling_since.is_some()) {
                    match watch_connect(media, l, events).await {
                        Ok(Some(state)) => {
                            l.state = state;
                            changed = true;
                        }
                        Ok(None) => {}
                        // The tick reports a Strom that cannot be reached.
                        Err(err) => {
                            debug!(input = %l.input.name, err = %format_args!("{err:#}"), "could not look at the dialling uplink, retrying");
                        }
                    }
                }
                if changed {
                    if let Some(reporter) = reporter {
                        reporter.publish(live);
                    }
                    tell_changes(live, events);
                }
                next_watch.as_mut().reset(tokio::time::Instant::now() + CONNECT_POLL);
            }
            // The hold changed: look at the productions now.
            () = media.hold_changed(), if gated.is_some() => {
                next_look.as_mut().reset(tokio::time::Instant::now());
            }
            stop = &mut stop => return Ok(stop),
        }
    }
}

/// [`reconcile_sources`], warning rather than failing: never fatal, the feeds are
/// already flowing. Retried at the next tick.
async fn update_sources(
    client: &OpenLiveClient,
    live: &mut [Live],
    gateway_id: Option<&str>,
    events: &Events,
) {
    if let Err(err) = reconcile_sources(client, live, gateway_id, events).await {
        warn!(err = %format_args!("{err:#}"), "could not update the Open Live sources, retrying next tick");
        events.emit(Event::Warning {
            message: format!("could not update the Open Live sources ({err:#}); the feeds are unaffected, retrying next tick"),
        });
    }
}

/// Tells each input's state that changed since it was last told. Only changes are
/// told, so the CLI appends a line per change rather than redrawing a screen: it is
/// read over SSH, in tmux, and in a log file after the fact.
fn tell_changes(live: &mut [Live], events: &Events) {
    for l in live.iter_mut() {
        if l.shown.as_ref() != Some(&l.state) {
            events.emit(Event::Input {
                input: l.input.name.clone(),
                state: l.state.clone(),
            });
            l.shown = Some(l.state.clone());
        }
    }
}

/// One look at the productions for an armed session: each input's uplink is started
/// when a production holding its source is live, and stopped (its source kept) when
/// none is any more. Idempotent: an input already in the state the productions ask
/// for is left alone, however many productions hold it. Returns whether a production
/// holding one of the sources is activating, so the next look comes sooner.
///
/// The productions not being readable changes nothing: a running uplink keeps
/// running and an armed one stays armed, retried at the next look. `watch` and
/// `unreadable` are kept by the caller between looks.
async fn look_at_productions<M: Media>(
    media: &M,
    client: &OpenLiveClient,
    video: &Video,
    live: &mut [Live],
    (watch, unreadable): (&mut gate::Watch, &mut bool),
    events: &Events,
) -> bool {
    let list = match client.list_productions().await {
        Ok(list) => {
            if std::mem::take(unreadable) {
                info!("the Open Live productions are readable again");
            }
            list
        }
        Err(err) => {
            // The tick's own source update warns the operator about Open Live.
            if !std::mem::replace(unreadable, true) {
                warn!(err = %format_args!("{err:#}"), "could not read the Open Live productions; every uplink stays as it is, retrying");
            }
            return false;
        }
    };
    watch.observe(&list);
    let mut soon = false;
    for l in live.iter_mut() {
        let verdict = match &l.source_id {
            Some(id) => watch.verdict(&list, id),
            None => Verdict::default(),
        };
        soon |= verdict.activating;
        if verdict.added_while_live != l.added_while_live {
            if !verdict.added_while_live.is_empty() {
                warn!(input = %l.input.name, productions = ?verdict.added_while_live, "the source was put in a production while it was live; Open Live has no receiver for it until the production is deactivated and reactivated");
            }
            l.added_while_live = verdict.added_while_live.clone();
            events.emit(Event::SlotAddedWhileLive {
                input: l.input.name.clone(),
                productions: verdict.added_while_live.clone(),
            });
        }
        // Held back (a phone in the background): stopped as though nothing were live.
        match (verdict.send() && !media.held_back(), l.sending) {
            (true, false) => send_uplink(media, video, l, &verdict, events).await,
            (false, true) => arm_again(media, l, events).await,
            // A start that failed, for a production no longer live: armed again.
            (false, false) if l.gate_error.take().is_some() => l.state = State::Armed,
            _ => {}
        }
    }
    soon
}

/// Starts an armed input's uplink: a production holding its source is live.
async fn send_uplink<M: Media>(
    media: &M,
    video: &Video,
    l: &mut Live,
    verdict: &Verdict,
    events: &Events,
) {
    if l.gate_error.is_none() {
        info!(input = %l.input.name, productions = ?verdict.live_in, "a production holding the source is live, starting the uplink");
        events.emit(Event::ProductionLive {
            input: l.input.name.clone(),
            productions: verdict.live_in.clone(),
        });
    }
    l.forget_uplink();
    match ensure_started(media, video, l).await {
        Ok(()) => {
            l.dialling_since = Some(Instant::now());
            l.sending = true;
            l.gate_error = None;
            l.state = State::Starting;
        }
        Err(err) => {
            let reason = format!("{err:#}");
            if l.gate_error.as_deref() != Some(reason.as_str()) {
                warn!(input = %l.input.name, err = %reason, "could not start the uplink, retrying at the next look");
            }
            l.state = State::Failed {
                reason: reason.clone(),
            };
            l.gate_error = Some(reason);
        }
    }
}

/// The input's uplink running, whether it was there already (running or not) or had
/// to be created.
async fn ensure_started<M: Media>(media: &M, video: &Video, l: &Live) -> Result<()> {
    match media.running(&l.input).await? {
        Some(true) => Ok(()),
        Some(false) => {
            if media.restart(&l.input).await? {
                Ok(())
            } else {
                bail!("Strom did not report the flow running after start")
            }
        }
        None => media.start(&l.input, video).await,
    }
}

/// Stops an armed input's uplink and arms it again: no production holding its source
/// is live any more. The flow goes, the source stays. A flow Strom would not remove
/// is retried at the next look, and polled as usual meanwhile.
async fn arm_again<M: Media>(media: &M, l: &mut Live, events: &Events) {
    match media.stop(&l.input).await {
        Ok(()) => {
            info!(input = %l.input.name, "no production holding the source is live, stopped the uplink");
            l.sending = false;
            l.state = State::Armed;
            l.forget_uplink();
            events.emit(Event::NoProductionLive {
                input: l.input.name.clone(),
            });
        }
        Err(err) => {
            warn!(input = %l.input.name, err = %format_args!("{err:#}"), "could not stop the uplink, retrying at the next look");
        }
    }
}

/// [`poll_input`] over a Strom, as the stall tests drive it.
#[cfg(test)]
async fn poll_flow(
    strom: &StromClient,
    gateway_id: &str,
    video: &Video,
    l: &mut Live,
    events: &Events,
) -> Result<State> {
    poll_input(&StromMedia::over(strom, gateway_id), video, l, events).await
}

/// Makes sure the input's uplink exists and runs, then judges it from its statistics.
async fn poll_input<M: Media>(
    media: &M,
    video: &Video,
    l: &mut Live,
    events: &Events,
) -> Result<State> {
    match media.running(&l.input).await? {
        Some(true) => {}
        Some(false) => return start_again(media, l).await,
        None => {
            // Deleted under us, in Strom's editor or by another tool. Put it back.
            info!(input = %l.input.name, "flow is gone from Strom, recreating it");
            l.last_bytes = None;
            l.uplink = None;
            l.forget_counters();
            media.start(&l.input, video).await?;
            l.dialling_since = Some(Instant::now());
            return Ok(State::Starting);
        }
    }

    let Some(sample) = media.uplink(&l.input).await? else {
        l.last_bytes = None;
        l.uplink = None;
        l.quiet_polls = 0;
        l.forget_counters();
        return Ok(State::Waiting);
    };
    if connected_late(l, Instant::now()) {
        return restart_late_connect(media, l, events).await;
    }
    l.uplink = Some(sample.clone());

    // A connected uplink whose sender drops about as much as it sends delivers
    // nothing, while its bytes still move. That is what a late connect into an Open
    // Live production did in #111: the receiver stops acknowledging, and the sender
    // drops at the stream rate until the connection breaks, or for good. A fresh
    // connection into the then-active production came back clean, so the flow is
    // restarted, with a growing wait and a cap. The receiver's fault itself is not
    // fixed here (Eyevinn/strom#1277).
    let counters = Counters::of(&sample);
    match judge(l.last_counters, counters) {
        Interval::Stalled => l.stalled_polls = l.stalled_polls.saturating_add(1),
        Interval::Delivering => {
            l.stalled_polls = 0;
            if l.stall_restarts > 0 || l.stall_given_up {
                info!(input = %l.input.name, "uplink delivering again after a stall");
            }
            l.stall_restarts = 0;
            l.stall_given_up = false;
        }
        Interval::Unjudged => l.stalled_polls = 0,
    }
    l.last_counters = Some(counters);
    if l.stalled_polls > 0 {
        if l.stalled_polls >= polls_before_restart(l.stall_restarts) {
            if l.stall_restarts < MAX_RESTARTS {
                l.stall_restarts += 1;
                let secs = u64::from(l.stalled_polls) * TICK.as_secs();
                warn!(input = %l.input.name, sent = counters.sent, dropped = counters.dropped, restart = l.stall_restarts, "uplink stalled: the receiver is not acknowledging, restarting the flow");
                events.emit(Event::Warning {
                    message: format!(
                        "{}: the uplink dropped {STALL_DROP_PERCENT} % or more of what it sent over the last {secs} s (sent {}, dropped {} on this connection), so the receiver is not taking it; restarting the flow to connect again (restart {} of {MAX_RESTARTS})",
                        l.input.name, counters.sent, counters.dropped, l.stall_restarts,
                    ),
                });
                return start_again(media, l).await;
            }
            if !l.stall_given_up {
                l.stall_given_up = true;
                warn!(input = %l.input.name, "uplink still stalled after {MAX_RESTARTS} restarts, no more restarts");
                events.emit(Event::Warning {
                    message: format!(
                        "{}: the uplink still drops what it sends after {MAX_RESTARTS} restarts, so the receiver is not taking it. No more restarts: the input stays waiting until the receiver takes the feed again. This is a receiver-side fault, see {STALL_UPSTREAM_ISSUE}",
                        l.input.name,
                    ),
                });
            }
        }
        // Bytes move, but nothing arrives: not delivering, whatever the hysteresis.
        l.last_bytes = sample.bytes_sent;
        l.quiet_polls = QUIET_TOLERANCE;
        return Ok(State::Waiting);
    }
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

/// Starts an uplink again (Strom: `stop_flow` + `start_flow`), whether it stalled or
/// is not running, forgetting what was measured on its last run. The new connection
/// is watched for a late connect like any start ([`LATE_CONNECT_AFTER`]).
async fn start_again<M: Media>(media: &M, l: &mut Live) -> Result<State> {
    l.last_bytes = None;
    l.uplink = None;
    l.forget_counters();
    Ok(if media.restart(&l.input).await? {
        l.dialling_since = Some(Instant::now());
        State::Starting
    } else {
        State::Failed {
            reason: "Strom did not report the flow running after start".to_string(),
        }
    })
}

/// Whether the uplink's first peer, which it has now, came late: the gateway started
/// it more than [`LATE_CONNECT_AFTER`] before `now` and it had no peer since. Ends
/// the watch either way, so only a first connect is ever judged.
fn connected_late(l: &mut Live, now: Instant) -> bool {
    l.dialling_since
        .take()
        .is_some_and(|since| now.saturating_duration_since(since) > LATE_CONNECT_AFTER)
}

/// The late-connect watch, every [`CONNECT_POLL`] while an input's uplink dials with
/// no peer: restarts it once if its first peer came late. Returns the input's new
/// state when it restarted; `None` while it still dials or connected in time.
async fn watch_connect<M: Media>(
    media: &M,
    l: &mut Live,
    events: &Events,
) -> Result<Option<State>> {
    if l.dialling_since.is_none() || media.uplink(&l.input).await?.is_none() {
        return Ok(None);
    }
    if connected_late(l, Instant::now()) {
        return restart_late_connect(media, l, events).await.map(Some);
    }
    Ok(None)
}

/// Restarts an uplink whose first peer came late, once (#111). The receiver behind a
/// late connect stalls (Eyevinn/strom#1277); a connect into one already running is
/// clean, so the flow is stopped and started to connect again at once, before the
/// stall. Not a stall restart: [`MAX_RESTARTS`] and the backoff are left alone, and
/// the watch stays off for the new connection, so it never restarts twice.
async fn restart_late_connect<M: Media>(media: &M, l: &mut Live, events: &Events) -> Result<State> {
    info!(input = %l.input.name, upstream = STALL_UPSTREAM_ISSUE, "the uplink connected more than {} s after it started dialling; restarting it once to connect into the running receiver", LATE_CONNECT_AFTER.as_secs());
    events.emit(Event::Warning {
        message: format!(
            "{}: the uplink connected more than {} s after it started, which can stall the receiver ({STALL_UPSTREAM_ISSUE}); restarting the flow once to connect again",
            l.input.name,
            LATE_CONNECT_AFTER.as_secs(),
        ),
    });
    let state = start_again(media, l).await;
    l.dialling_since = None;
    state
}

/// An uplink connection's cumulative packet counters. libsrt keeps them per
/// connection, so they start again from zero when the caller reconnects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Counters {
    sent: u64,
    dropped: u64,
}

impl Counters {
    fn of(sample: &SrtCallerStats) -> Self {
        Self {
            sent: sample.packets_sent.unwrap_or(0),
            dropped: sample.packets_sent_dropped.unwrap_or(0),
        }
    }
}

impl Live {
    fn forget_counters(&mut self) {
        self.last_counters = None;
        self.stalled_polls = 0;
    }

    /// Everything measured on the uplink, stall restarts included: for a flow the
    /// productions start or stop, whose next connection owes nothing to the last.
    fn forget_uplink(&mut self) {
        self.forget_counters();
        self.last_bytes = None;
        self.uplink = None;
        self.quiet_polls = 0;
        self.stall_restarts = 0;
        self.stall_given_up = false;
        self.dialling_since = None;
    }
}

/// What one poll interval of an uplink connection shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Interval {
    /// Nothing to judge: no earlier sample on this connection, counters that went
    /// down (libsrt reconnected, so this is a new connection), or fewer than
    /// [`STALL_MIN_SENT`] packets sent.
    Unjudged,
    /// Enough was sent and most of it left the sender. A lossy link counts: it still
    /// delivers a picture.
    Delivering,
    /// Enough was sent and the sender dropped [`STALL_DROP_PERCENT`] or more of it
    /// before transmission: the receiver is not taking the stream.
    Stalled,
}

/// Judges the interval between two polls of the same uplink connection. A healthy
/// link drops none (every run in #120 and on #111, late connects included) and
/// #111's stall drops all of them; the threshold sits near the stall, so a link
/// that is congested but still delivering is not mistaken for one.
fn judge(before: Option<Counters>, now: Counters) -> Interval {
    let Some(before) = before else {
        return Interval::Unjudged;
    };
    let (Some(sent), Some(dropped)) = (
        now.sent.checked_sub(before.sent),
        now.dropped.checked_sub(before.dropped),
    ) else {
        return Interval::Unjudged;
    };
    if sent < STALL_MIN_SENT {
        Interval::Unjudged
    } else if dropped.saturating_mul(100) >= sent.saturating_mul(STALL_DROP_PERCENT) {
        Interval::Stalled
    } else {
        Interval::Delivering
    }
}

/// Stalled polls in a row to wait for before the next restart, after `restarts`
/// restarts that brought no delivering interval back: [`STALLED_POLLS`], doubled per
/// restart, at most [`MAX_STALLED_POLLS`].
fn polls_before_restart(restarts: u32) -> u32 {
    STALLED_POLLS
        .saturating_mul(1u32.checked_shl(restarts).unwrap_or(u32::MAX))
        .min(MAX_STALLED_POLLS)
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

/// What a stop does with the Open Live sources the run registered.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Sources {
    /// Remove them, and every other source carrying this gateway's name prefix: `up`
    /// owns what it started.
    #[default]
    Remove,
    /// Leave each input's source in Open Live, marked inactive, for the next run to
    /// adopt by name ([`plan_port`]'s `Keep`, `reconcile_sources`): its id stays the
    /// same, so a production slot that references it keeps working after Stop and Go
    /// live (the desktop app). Other sources carrying the prefix (a label renamed, an
    /// input not in this run) are still swept. `down` removes them all.
    Keep,
}

/// When a session sends each input's uplink.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Sending {
    /// From the start until the stop: `up`.
    #[default]
    Always,
    /// Only while a production holding the input's Open Live source is live
    /// ([`gate`]): the session registers (or keeps) the sources and arms, and starts
    /// and stops each uplink as the productions go `active` and back. Needs
    /// registration with Open Live. The desktop app's Go live; `up
    /// --wait-for-production`.
    WhileLive,
}

/// How long teardown may take when Windows is ending the process, leaving a second
/// of the five Windows allows for stopping the managed Strom. That second is enough
/// only because `LocalStrom`'s drop ends a managed Strom at once on Windows
/// (`Child::kill`, i.e. TerminateProcess) before it waits, rather than giving it
/// `STOP_TIMEOUT` to exit by itself.
pub const ENDING_TEARDOWN: Duration = Duration::from_secs(4);

/// Removes one flow and says so. Returns whether it is gone.
async fn remove_flow_reported<M: Media>(media: &M, l: &Live, events: &Events) -> bool {
    match media.stop(&l.input).await {
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

/// Leaves one input's Open Live source in place for the next run and says so. A
/// source that may still read `active` is marked inactive first: nothing is sending
/// to it any more. Returns whether that went through (or was not needed).
async fn keep_source_reported(
    client: &OpenLiveClient,
    id: &str,
    l: &Live,
    events: &Events,
) -> bool {
    let mut done = true;
    if matches!(l.state, State::OnAir | State::Unreachable) {
        let idle = SourcePayload::new(
            &l.input.name,
            &l.input.endpoint.cloud_uri(),
            false,
            l.input.endpoint.latency_ms,
            None,
        );
        if let Err(err) = client.patch_source(id, &idle).await {
            warn!(input = %l.input.name, err = %format_args!("{err:#}"), "could not mark the kept source inactive");
            events.emit(Event::Warning {
                message: format!(
                    "could not mark the source of {} inactive ({err:#})",
                    l.input.name
                ),
            });
            done = false;
        }
    }
    events.emit(Event::SourceRetained {
        input: l.input.name.clone(),
    });
    done
}

/// What the sweep after a teardown leaves alone: when the sources are kept, those of
/// this run's inputs, by id and by name (`names`, every input of the run, including
/// any that failed to start).
fn kept_by_this_run(live: &[Live], sources: Sources, names: &BTreeSet<String>) -> BTreeSet<String> {
    match sources {
        Sources::Remove => BTreeSet::new(),
        Sources::Keep => live
            .iter()
            .filter_map(|l| l.source_id.clone())
            .chain(names.iter().cloned())
            .collect(),
    }
}

/// Stops and removes everything the run created; its sources only when `sources`
/// says so.
async fn teardown<M: Media>(
    media: &M,
    open_live: Option<&OpenLiveClient>,
    gateway_name: &str,
    live: &[Live],
    (sources, names): (Sources, &BTreeSet<String>),
    events: &Events,
) {
    // Sources Open Live refused to remove, already reported, so the sweep below does
    // not try them again and print the same refusal twice; and the ones kept.
    let mut kept = kept_by_this_run(live, sources, names);
    for l in live {
        remove_flow_reported(media, l, events).await;
        if let (Some(client), Some(id)) = (open_live, &l.source_id) {
            match sources {
                Sources::Remove => {
                    if !remove_source_reported(client, id, l, events).await {
                        kept.insert(id.clone());
                    }
                }
                Sources::Keep => {
                    keep_source_reported(client, id, l, events).await;
                }
            }
        }
    }
    // Anything else of ours still there goes too: a source recreated by a race, one
    // whose id we never learned, or one under a name no longer in use. Matched on the
    // name prefix, so sources belonging to another gateway or made by hand are never
    // touched.
    if let Some(client) = open_live {
        sweep_sources(client, gateway_name, &kept, events).await;
    }
}

/// Teardown for when Windows is ending the process: every flow and source removal
/// runs at once, and whatever has not finished by `deadline` is named in the log and
/// left for the next `up` or `down`, which find it by derived id and name prefix.
async fn teardown_within<M: Media>(
    media: &M,
    open_live: Option<&OpenLiveClient>,
    gateway_name: &str,
    live: &[Live],
    (sources, names): (Sources, &BTreeSet<String>),
    deadline: tokio::time::Instant,
    events: &Events,
) {
    // Send, so that `up` can run on a spawned task (`session::Session::start`).
    type Job<'a> = std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>>;
    // What each job does, the source id when it removes a source, and whether it is a
    // removal: a kept source's inactive mark not done in time is left for the next
    // run's first tick, which sets the status anyway.
    let mut what: Vec<(String, Option<String>, bool)> = Vec::new();
    let mut jobs: Vec<Job> = Vec::new();
    for l in live {
        what.push((format!("flow of {}", l.input.name), None, true));
        jobs.push(Box::pin(remove_flow_reported(media, l, events)));
        if let (Some(client), Some(id)) = (open_live, &l.source_id) {
            match sources {
                Sources::Remove => {
                    what.push((
                        format!("source of {}", l.input.name),
                        Some(id.clone()),
                        true,
                    ));
                    jobs.push(Box::pin(remove_source_reported(client, id, l, events)));
                }
                Sources::Keep => {
                    what.push((format!("source of {}", l.input.name), None, false));
                    jobs.push(Box::pin(keep_source_reported(client, id, l, events)));
                }
            }
        }
    }
    let mut kept = kept_by_this_run(live, sources, names);
    let mut left = Vec::new();
    for ((label, id, removal), done) in what.into_iter().zip(join_until(deadline, jobs).await) {
        match done {
            Some(true) => {}
            _ if !removal => {}
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

/// Removes every source carrying this gateway's name prefix, except those whose id
/// or name is in `skip`. Returns how many.
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
        .filter(|s| s.name.starts_with(&prefix) && !skip.contains(&s.id) && !skip.contains(&s.name))
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
    if let Some(client) = open_live_client(cfg, None)? {
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

/// Where the uplink goes. Through a relay `host` is the relay's public `host:port`
/// (empty when Open Live does not report it) and `relay` its name, with no ports.
#[derive(Debug, Serialize)]
pub struct UplinkReport {
    pub host: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ports: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port_source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub relay: Option<String>,
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
    /// Through a relay only: the relay's name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub relay: Option<String>,
    /// Through a relay only: the address the uplink dials
    /// (`srt://<relay>?streamid=<id>`), never with a passphrase.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dial_in: Option<String>,
    /// Through a relay only, when Open Live gives no `dial_in`: why, in words.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dial_in_error: Option<String>,
}

impl SourceReport {
    fn of(s: &Source) -> Self {
        let relay = s.relay().map(str::to_string);
        let (dial_in, dial_in_error) = match (&relay, s.relay_dial_in()) {
            (None, _) => (None, None),
            (Some(_), Ok(d)) => (Some(d.uri), None),
            (Some(_), Err(why)) => (None, Some(why)),
        };
        SourceReport {
            id: s.id.clone(),
            status: s.status.clone(),
            warning: s.connect_warning().map(str::to_string),
            relay,
            dial_in,
            dial_in_error,
        }
    }
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
    let open_live = open_live_client(cfg, None)?;
    // Warnings while working out the uplink are the log's, not the report's.
    let (uplink, uplink_error) =
        match resolve_cloud(cfg, open_live.as_ref(), true, &Events::discard()).await {
            Ok(Cloud {
                relay: Some(relay), ..
            }) => (
                Some(UplinkReport {
                    host: relay.public.unwrap_or_default(),
                    ports: None,
                    port_source: None,
                    relay: Some(relay.name),
                }),
                None,
            ),
            Ok(cloud) => (
                Some(UplinkReport {
                    host: cloud.host,
                    ports: Some(format_ports(&cloud.ports)),
                    port_source: Some(cloud.port_source.to_string()),
                    relay: None,
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
                .map(SourceReport::of),
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
        (Some(u), _) => match (&u.relay, &u.ports, &u.port_source) {
            (Some(relay), _, _) if u.host.is_empty() => {
                out += &format!("Uplink through Open Live's SRT relay {relay:?}.\n\n")
            }
            (Some(relay), _, _) => {
                out += &format!(
                    "Uplink through Open Live's SRT relay {relay:?} at {}.\n\n",
                    u.host
                )
            }
            (None, ports, source) => {
                out += &format!(
                    "Uplink to {}, SRT ports {} from {}.\n\n",
                    u.host,
                    ports.as_deref().unwrap_or_default(),
                    source.as_deref().unwrap_or_default()
                )
            }
        },
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
            Some(s) => {
                let mut text = match &s.warning {
                    Some(warning) => format!("{} (warning: {warning})", s.status),
                    None => s.status.clone(),
                };
                if let Some(relay) = &s.relay {
                    text += &format!(", via relay {relay:?}");
                    match (&s.dial_in, &s.dial_in_error) {
                        (Some(dial_in), _) => text += &format!(" to {dial_in}"),
                        (None, Some(why)) => text += &format!(": {why}"),
                        (None, None) => {}
                    }
                }
                text
            }
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

/// Another gateway is live on this machine: a CLI `up` or the app, named by the
/// pidfile. `up` refuses to start a second one; `down` stops it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AlreadyRunning {
    pub pid: u32,
}

impl fmt::Display for AlreadyRunning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "already running (pid {}); run 'open-live-ingest down' first",
            self.pid
        )
    }
}

impl std::error::Error for AlreadyRunning {}

/// The pid of a gateway that is live now, by the pidfile: a CLI `up` or the app,
/// checked by process name so a stale pidfile names nothing. This process is not
/// counted: an app that has ended one session may start the next.
pub fn running_gateway() -> Option<u32> {
    running_gateway_in(&config::pidfile())
}

/// [`running_gateway`], by the pidfile at `pidfile`.
pub(crate) fn running_gateway_in(pidfile: &Path) -> Option<u32> {
    read_pidfile_at(pidfile).filter(|&pid| pid != std::process::id() && is_running_gateway(pid))
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

/// This process's entry in the pidfile for the length of one `up`, from its refusal
/// check to its return, whichever way it returns.
pub(crate) struct Pidfile {
    written: bool,
}

impl Pidfile {
    pub(crate) fn write(events: &Events) -> Self {
        let written = match write_pidfile() {
            Ok(()) => true,
            Err(err) => {
                warn!(err = %format_args!("{err:#}"), "could not write the pidfile; `down` will still find the flows and sources");
                events.emit(Event::Warning {
                    message: format!(
                        "could not write the pidfile ({err:#}); `down` will still find the flows and sources"
                    ),
                });
                false
            }
        };
        Pidfile { written }
    }

    /// No entry at all, for a Strom backend in the tests.
    #[cfg(test)]
    pub(crate) fn none() -> Self {
        Pidfile { written: false }
    }
}

impl Drop for Pidfile {
    fn drop(&mut self) {
        // Only while it is still ours: `down` may have removed it already.
        if self.written && read_pidfile() == Some(std::process::id()) {
            remove_pidfile();
        }
    }
}

fn read_pidfile() -> Option<u32> {
    read_pidfile_at(&config::pidfile())
}

fn read_pidfile_at(path: &Path) -> Option<u32> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

fn remove_pidfile() {
    std::fs::remove_file(config::pidfile()).ok();
}

/// Whether the pid is alive and is a gateway. Pids are reused after a reboot, so a
/// stale pidfile must never have `down` signal an unrelated process.
fn is_running_gateway(pid: u32) -> bool {
    process_is(pid, Exe::StartsWith(GATEWAY_COMM))
}

/// What `ps -o comm=` shows for this binary. Linux keeps only the first 15 bytes of
/// a command name (`open-live-inges`); macOS prints the full path, whose file name is
/// the whole name. Both file names start with it, and so does the app's
/// (`open-live-ingest-app`).
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

/// What a process's executable file name is matched against. Only the file name: the
/// rest of a path (`/Users/lindstrom/…`, a folder named `nostromo`) never counts, so
/// a pid reused by an unrelated process is not taken for ours.
#[derive(Debug, Clone, Copy)]
enum Exe<'a> {
    /// Exactly this name (`strom`); `.exe` and case do not count on Windows, and a
    /// name longer than Linux's 15-byte command name may be cut to it.
    Is(&'a str),
    /// A name that starts with this (the gateway's, which the app's extends).
    StartsWith(&'a str),
}

/// Whether the pid is alive and its executable is named `name` (`strom`, `ping`),
/// by file name, not by anything in its path.
pub fn process_named(pid: u32, name: &str) -> bool {
    process_is(pid, Exe::Is(name))
}

#[cfg(not(windows))]
fn process_is(pid: u32, exe: Exe) -> bool {
    if !process_alive(pid) {
        return false;
    }
    let Ok(out) = std::process::Command::new("ps")
        .args(["-o", "comm=", "-p", &pid.to_string()])
        .output()
    else {
        return false;
    };
    comm_matches(&String::from_utf8_lossy(&out.stdout), exe)
}

/// The image name comes from the row tasklist prints for exactly this pid, so a stale
/// pid that has been reused matches only if the new process has the same name.
#[cfg(windows)]
fn process_is(pid: u32, exe: Exe) -> bool {
    tasklist_image(pid).is_some_and(|image| image_matches(&image, exe))
}

/// Whether a command name from `ps -o comm=` (a bare name on Linux, a full path on
/// macOS) names `exe`, by its file name.
#[cfg_attr(windows, allow(dead_code))]
fn comm_matches(comm: &str, exe: Exe) -> bool {
    let file = comm.trim().rsplit('/').next().unwrap_or_default();
    match exe {
        Exe::Is(name) => {
            !name.is_empty() && (file == name || (name.len() > 15 && file == truncate_comm(name)))
        }
        Exe::StartsWith(prefix) => !prefix.is_empty() && file.starts_with(prefix),
    }
}

/// Whether a Windows image name names `exe`. Windows file names are case insensitive,
/// so `Strom.EXE` is the same program as `strom.exe`, and `strom` names it too.
#[cfg_attr(not(windows), allow(dead_code))]
fn image_matches(image: &str, exe: Exe) -> bool {
    fn stem(name: &str) -> String {
        let file = name.trim().rsplit(['\\', '/']).next().unwrap_or_default();
        let file = file.to_lowercase();
        match file.strip_suffix(".exe") {
            Some(stem) => stem.to_string(),
            None => file,
        }
    }
    let file = stem(image);
    match exe {
        Exe::Is(name) => {
            let name = stem(name);
            !name.is_empty() && file == name
        }
        Exe::StartsWith(prefix) => !prefix.is_empty() && file.starts_with(&prefix.to_lowercase()),
    }
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
    use std::sync::{Arc, Mutex};

    fn sample_status() -> StatusReport {
        StatusReport {
            process: ProcessReport {
                running: true,
                pid: Some(4242),
            },
            uplink: Some(UplinkReport {
                host: "cloud.example.com".to_string(),
                ports: Some("47110-47129".to_string()),
                port_source: Some(PortSource::OpenLive.to_string()),
                relay: None,
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
                        relay: None,
                        dial_in: None,
                        dial_in_error: None,
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
            ..Default::default()
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

    /// #115: Go live after a Stop that kept the source (`Sources::Keep`) adopts it by
    /// name. The `SourceAdopted` event, and the log line beside it, name its id and
    /// port, so a run shows the id reused instead of leaving it to be inferred from a
    /// missing `Registered`. Nothing is created or patched: the fake Open Live answers
    /// the source list and nothing else.
    #[tokio::test]
    async fn a_kept_source_is_adopted_with_its_id_and_port_in_the_event() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let served = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let n = socket.read(&mut buf).await.unwrap();
            let request = String::from_utf8_lossy(&buf[..n]).to_string();
            let body = serde_json::json!([{
                "id": "src-kept",
                "name": "Venue — Cam",
                "address": "srt://:47103?mode=listener&passphrase=***",
                "streamType": "srt",
                "status": "inactive",
            }])
            .to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            let _ = socket.shutdown().await;
            request
        });
        let client = OpenLiveClient::new(
            &format!("http://{addr}"),
            crate::config::AuthMode::Direct,
            None,
        )
        .unwrap();
        let mut inputs = vec![Input {
            id: "dev-cam".to_string(),
            name: "Venue — Cam".to_string(),
            source: flow::Source::Test {
                resolution: "1280x720".to_string(),
                framerate: "25/1".to_string(),
            },
            endpoint: crate::config::Uplink::default().endpoint("cloud.example.com", 0),
        }];
        let range: BTreeSet<u16> = (47100..=47109).collect();
        let mut ids = HashMap::new();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let events = {
            let seen = seen.clone();
            Events::new(move |e| seen.lock().unwrap().push(e))
        };
        assign_ports_from_open_live(&client, &mut inputs, &range, None, &mut ids, &events)
            .await
            .unwrap();
        assert!(
            served.await.unwrap().starts_with("GET /api/v1/sources "),
            "only the source list is read"
        );
        assert_eq!(
            *seen.lock().unwrap(),
            vec![Event::SourceAdopted {
                input: "Venue — Cam".to_string(),
                source_id: "src-kept".to_string(),
                port: 47103,
            }]
        );
        assert_eq!(inputs[0].endpoint.port, 47103);
        assert_eq!(
            ids.get("dev-cam"),
            Some(&Registered {
                input: "Venue — Cam".to_string(),
                source_id: "src-kept".to_string(),
                warning: None,
            })
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
    fn info_with_relays(relays: &[&str]) -> ServerInfo {
        ServerInfo {
            strom_host: Some("strom.example.com".into()),
            srt: Some(crate::openlive::SrtInfo {
                state: Some("ok".into()),
                relays: relays
                    .iter()
                    .map(|name| crate::openlive::Relay {
                        name: name.to_string(),
                        public: format!("{name}.relay.example.com:6000"),
                    })
                    .collect(),
                ..Default::default()
            }),
            ..ServerInfo::default()
        }
    }

    /// `caller` goes through the relay when Open Live reports one, the media can
    /// dial one and the sources are registered; otherwise it stays on a port, as
    /// before relays (an Open Live without `srt`, AC5). `listener` never does.
    #[test]
    fn caller_uses_the_relay_when_open_live_reports_one() {
        let with = info_with_relays(&["default", "eu"]);
        let relay = choose_relay(UplinkMode::Caller, "default", true, true, Some(&with))
            .unwrap()
            .unwrap();
        assert_eq!(relay.name, "default");
        assert_eq!(
            relay.public.as_deref(),
            Some("default.relay.example.com:6000")
        );
        assert_eq!(relay.host(), "default.relay.example.com");
        let eu = choose_relay(UplinkMode::Caller, "eu", true, true, Some(&with))
            .unwrap()
            .unwrap();
        assert_eq!(eu.name, "eu");

        let v0_8_0 = ServerInfo {
            strom_host: Some("strom.example.com".into()),
            srt_ports: Some([47110].into_iter().collect()),
            ..ServerInfo::default()
        };
        for (info, registering, dials) in [
            (Some(&v0_8_0), true, true),
            (Some(&info_with_relays(&[])), true, true),
            (None, true, true),
            (Some(&with), false, true),
            (Some(&with), true, false),
        ] {
            assert_eq!(
                choose_relay(UplinkMode::Caller, "default", registering, dials, info).unwrap(),
                None
            );
        }
        assert_eq!(
            choose_relay(UplinkMode::Listener, "default", true, true, Some(&with)).unwrap(),
            None
        );
    }

    /// No quiet switch: a relay Open Live does not report, or relay mode where there
    /// is none, is an error that says what to set.
    #[test]
    fn a_missing_relay_is_an_error_naming_the_ones_there_are() {
        let with = info_with_relays(&["default", "eu"]);
        let err = choose_relay(UplinkMode::Caller, "us", true, true, Some(&with)).unwrap_err();
        assert_eq!(
            err.to_string(),
            r#"Open Live has no SRT relay named "us"; it has default, eu: set uplink.relay to one of them"#
        );
        let err = choose_relay(UplinkMode::Relay, "default", true, true, None).unwrap_err();
        assert!(err.to_string().contains("older than SRT relays"), "{err}");
        let err = choose_relay(
            UplinkMode::Relay,
            "default",
            true,
            true,
            Some(&info_with_relays(&[])),
        )
        .unwrap_err();
        assert!(err.to_string().contains("reports no SRT relay"), "{err}");
        assert!(choose_relay(UplinkMode::Relay, "default", false, true, Some(&with)).is_err());
        assert!(choose_relay(UplinkMode::Relay, "default", true, false, Some(&with)).is_err());
        assert!(
            choose_relay(UplinkMode::Relay, "eu", true, true, Some(&with))
                .unwrap()
                .is_some()
        );
    }

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
            last_counters: None,
            stalled_polls: 0,
            stall_restarts: 0,
            stall_given_up: false,
            dialling_since: None,
            source_id: source_id.map(str::to_string),
            connect_warning: None,
            uplink,
            sending: true,
            gate_error: None,
            added_while_live: Vec::new(),
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

    fn counters(sent: u64, dropped: u64) -> Counters {
        Counters { sent, dropped }
    }

    /// #111 run B, read 10 s apart: +6,065 sent and +6,065 dropped, RTT steady.
    /// The receiver has stopped acknowledging; this is a stall.
    #[test]
    fn an_uplink_dropping_what_it_sends_is_stalled() {
        assert_eq!(
            judge(Some(counters(21_409, 11_406)), counters(27_474, 17_471)),
            Interval::Stalled
        );
        // 90 % dropped is already one.
        assert_eq!(
            judge(Some(counters(0, 0)), counters(1_000, 900)),
            Interval::Stalled
        );
    }

    /// #111 run A and the reconnect after run B: nothing dropped. #120's late
    /// connect through a capped link: a loss burst repaired by retransmission, no
    /// sender drops. Neither is a stall.
    #[test]
    fn a_delivering_uplink_is_not_stalled_even_with_a_connect_burst() {
        assert_eq!(
            judge(Some(counters(9_000, 0)), counters(15_376, 0)),
            Interval::Delivering
        );
        assert_eq!(
            judge(Some(counters(0, 0)), counters(7_882, 0)),
            Interval::Delivering
        );
    }

    /// A venue uplink with less capacity than the stream: the sender drops a large
    /// share of each interval, but what is left still reaches the receiver as a
    /// degraded, live picture. That is not #111's "dropped ≈ sent".
    #[test]
    fn a_congested_but_delivering_uplink_is_not_stalled() {
        for dropped in [3_000, 4_200, 5_000, 5_399] {
            assert_eq!(
                judge(Some(counters(0, 0)), counters(6_000, dropped)),
                Interval::Delivering,
                "{dropped} of 6000 dropped"
            );
        }
        assert_eq!(
            judge(Some(counters(0, 0)), counters(6_000, 5_400)),
            Interval::Stalled
        );
    }

    #[test]
    fn stalls_need_a_baseline_on_the_same_connection_and_enough_traffic() {
        // No earlier sample on this connection.
        assert_eq!(judge(None, counters(27_474, 17_471)), Interval::Unjudged);
        // Counters went down: libsrt reconnected, so this is a new connection.
        assert_eq!(
            judge(Some(counters(27_474, 17_471)), counters(6_000, 0)),
            Interval::Unjudged
        );
        assert_eq!(
            judge(Some(counters(27_474, 17_471)), counters(30_000, 10)),
            Interval::Unjudged
        );
        // Too little sent to judge.
        assert_eq!(
            judge(Some(counters(100, 0)), counters(500, 400)),
            Interval::Unjudged
        );
        // An absent counter reads as zero.
        let sample: SrtCallerStats =
            serde_json::from_value(serde_json::json!({ "bytes_sent": 10 })).unwrap();
        assert_eq!(Counters::of(&sample), counters(0, 0));
    }

    #[test]
    fn the_wait_before_a_restart_doubles_up_to_three_minutes() {
        let waits: Vec<u32> = (0..7).map(polls_before_restart).collect();
        assert_eq!(waits, [2, 4, 8, 16, 18, 18, 18]);
        assert_eq!(polls_before_restart(40), MAX_STALLED_POLLS);
    }

    /// A fake of the Strom routes `poll_flow` uses, for one flow that runs: each
    /// srt-stats call answers the next scripted `(sent, dropped)` sample, or no peer
    /// (404) for `None` or once the script is spent. Every request is recorded.
    #[derive(Default)]
    struct FakeStrom {
        flow: serde_json::Value,
        running: bool,
        stats: std::collections::VecDeque<Option<(u64, u64)>>,
        calls: Vec<String>,
    }

    async fn fake_strom(state: Arc<Mutex<FakeStrom>>) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    match socket.read(&mut chunk).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    }
                }
                let head = String::from_utf8_lossy(&buf).to_string();
                let mut words = head.split_whitespace();
                let call = format!(
                    "{} {}",
                    words.next().unwrap_or(""),
                    words.next().unwrap_or("")
                );
                let (status, body) = {
                    let mut s = state.lock().unwrap();
                    s.calls.push(call.clone());
                    let action = call.rsplit('/').next().unwrap_or("");
                    let mut flow = s.flow.clone();
                    match (call.split(' ').next().unwrap_or(""), action) {
                        ("POST", "start") => {
                            s.running = true;
                            flow["running"] = serde_json::json!(true);
                            ("200 OK", serde_json::json!({ "flow": flow }))
                        }
                        ("POST", "stop") => {
                            s.running = false;
                            flow["running"] = serde_json::json!(false);
                            ("200 OK", serde_json::json!({ "flow": flow }))
                        }
                        ("GET", "srt-stats") => match s.stats.pop_front().flatten() {
                            Some((sent, dropped)) => (
                                "200 OK",
                                serde_json::json!({
                                    "flow_id": flow["id"],
                                    "stats": { "connections": { "uplink:srtsink": {
                                        "role": "sink", "mode": "caller", "connected": true,
                                        "callers": [{
                                            "bytes_sent": sent * 1316,
                                            "packets_sent": sent,
                                            "packets_sent_dropped": dropped
                                        }]
                                    }}}
                                }),
                            ),
                            None => ("404 Not Found", serde_json::json!({ "error": "no peer" })),
                        },
                        ("GET", _) => {
                            flow["running"] = serde_json::json!(s.running);
                            ("200 OK", serde_json::json!({ "flow": flow }))
                        }
                        _ => ("404 Not Found", serde_json::Value::Null),
                    }
                };
                let body = body.to_string();
                let response = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
            }
        });
        format!("http://{addr}")
    }

    /// Drives `poll_flow` over a scripted srt-stats sequence, one poll per sample,
    /// as the loop's tick does, and returns each poll's state, the warnings emitted,
    /// and the stop and start calls Strom saw, in order.
    struct Script {
        states: Vec<State>,
        warnings: Vec<String>,
        restarts: Vec<String>,
        live: Live,
    }

    async fn run_script(initial: State, samples: &[Option<(u64, u64)>]) -> Script {
        run_script_on(live("A", initial, None, None), samples).await
    }

    /// [`run_script`] from a given input, for one that is dialling (the late-connect
    /// watch on).
    async fn run_script_on(mut l: Live, samples: &[Option<(u64, u64)>]) -> Script {
        let video = Video::default();
        let state = Arc::new(Mutex::new(FakeStrom {
            flow: serde_json::to_value(flow::build("gw", &l.input, &video)).unwrap(),
            running: true,
            stats: samples.iter().copied().collect(),
            calls: Vec::new(),
        }));
        let strom = StromClient::new(&fake_strom(state.clone()).await, None).unwrap();
        let warnings = Arc::new(Mutex::new(Vec::new()));
        let events = {
            let warnings = warnings.clone();
            Events::new(move |e| {
                if let Event::Warning { message } = e {
                    warnings.lock().unwrap().push(message);
                }
            })
        };
        let mut states = Vec::new();
        for _ in samples {
            let polled = poll_flow(&strom, "gw", &video, &mut l, &events)
                .await
                .unwrap();
            l.state = polled.clone();
            states.push(polled);
        }
        let restarts = state
            .lock()
            .unwrap()
            .calls
            .iter()
            .filter(|c| c.starts_with("POST"))
            .map(|c| c.rsplit('/').next().unwrap().to_string())
            .collect();
        let warnings = warnings.lock().unwrap().clone();
        Script {
            states,
            warnings,
            restarts,
            live: l,
        }
    }

    /// #111 run B's rate: +6,065 sent and +6,065 dropped per poll.
    const STALL: u64 = 6_065;

    /// A stalled sample `n` polls into a connection.
    fn stall(n: u64) -> Option<(u64, u64)> {
        Some((n * STALL, n * STALL))
    }

    /// A clean sample `n` polls into a connection.
    fn clean(n: u64) -> Option<(u64, u64)> {
        Some((n * STALL, 0))
    }

    /// The stall of #111 on an input that is on air: the first stalled poll
    /// is already Waiting, the second stops and starts the flow with a warning, and
    /// the restarted flow reports Starting.
    #[tokio::test]
    async fn a_stalled_uplink_is_waiting_then_restarted_with_a_warning() {
        let run = run_script(
            State::Waiting,
            &[
                clean(1),
                clean(2),
                Some((3 * STALL, STALL)),
                Some((4 * STALL, 2 * STALL)),
            ],
        )
        .await;
        assert_eq!(
            run.states,
            [
                State::Waiting,
                State::OnAir,
                State::Waiting,
                State::Starting
            ]
        );
        assert_eq!(run.restarts, ["stop", "start"]);
        assert_eq!(run.warnings.len(), 1, "{:?}", run.warnings);
        let warning = &run.warnings[0];
        assert!(warning.contains("Venue — A"), "{warning}");
        assert!(warning.contains("over the last 20 s"), "{warning}");
        assert!(warning.contains("restart 1 of 4"), "{warning}");
        assert_eq!(run.live.stall_restarts, 1);
        assert_eq!(run.live.last_counters, None);
    }

    /// One stalled interval followed by a clean one is a burst, not a stall: no
    /// restart, no warning, and the input is on air again.
    #[tokio::test]
    async fn a_stall_followed_by_a_clean_interval_is_not_restarted() {
        let run = run_script(
            State::Waiting,
            &[
                clean(1),
                clean(2),
                Some((3 * STALL, STALL)),
                Some((4 * STALL, STALL)),
                Some((5 * STALL, STALL)),
            ],
        )
        .await;
        assert_eq!(
            run.states,
            [
                State::Waiting,
                State::OnAir,
                State::Waiting,
                State::OnAir,
                State::OnAir
            ]
        );
        assert!(run.restarts.is_empty(), "{:?}", run.restarts);
        assert!(run.warnings.is_empty(), "{:?}", run.warnings);
    }

    /// A congested link dropping 70 % of each interval keeps delivering: it stays on
    /// air and is never restarted.
    #[tokio::test]
    async fn a_lossy_but_delivering_uplink_is_never_restarted() {
        let lossy = |n: u64| Some((n * 6_000, n * 4_200));
        let run = run_script(State::Waiting, &[lossy(1), lossy(2), lossy(3), lossy(4)]).await;
        assert_eq!(
            run.states,
            [State::Waiting, State::OnAir, State::OnAir, State::OnAir]
        );
        assert!(run.restarts.is_empty(), "{:?}", run.restarts);
        assert!(run.warnings.is_empty(), "{:?}", run.warnings);
    }

    /// One connection that stalls from its first sample, after the flow (re)starts:
    /// the no-baseline sample, then `stalled` stalled polls.
    fn stalled_connection(stalled: u64) -> Vec<Option<(u64, u64)>> {
        (1..=stalled + 1).map(stall).collect()
    }

    /// A receiver that never takes the feed: each restart waits twice as long as the
    /// one before, and after the cap the input stays Waiting, with one warning that
    /// points at the receiver, and no more restarts.
    #[tokio::test]
    async fn a_receiver_that_never_recovers_gets_backed_off_restarts_then_none() {
        let mut samples = Vec::new();
        for restart in 0..MAX_RESTARTS {
            samples.extend(stalled_connection(u64::from(polls_before_restart(restart))));
        }
        // Long after the cap: still stalled, still no restart.
        samples.extend(stalled_connection(40));
        let run = run_script(State::Waiting, &samples).await;

        let stops = run.restarts.iter().filter(|c| *c == "stop").count();
        assert_eq!(stops, MAX_RESTARTS as usize, "{:?}", run.restarts);
        // The restarts landed after 2, 4, 8 and 16 stalled polls of their connection.
        let starting: Vec<usize> = run
            .states
            .iter()
            .enumerate()
            .filter(|(_, s)| **s == State::Starting)
            .map(|(i, _)| i)
            .collect();
        assert_eq!(starting, [2, 7, 16, 33]);
        assert!(run.states[34..].iter().all(|s| *s == State::Waiting));

        assert_eq!(
            run.warnings.len(),
            MAX_RESTARTS as usize + 1,
            "{:?}",
            run.warnings
        );
        assert!(run.warnings[1].contains("over the last 40 s"));
        assert!(run.warnings[3].contains("over the last 160 s"));
        let given_up = run.warnings.last().unwrap();
        assert!(given_up.contains("No more restarts"), "{given_up}");
        assert!(given_up.contains("strom/issues/1277"), "{given_up}");
        assert!(run.live.stall_given_up);
    }

    /// A delivering interval after restarts resets the backoff: the next stall is
    /// restarted after two polls again, and a given-up input can restart again.
    #[tokio::test]
    async fn a_delivering_interval_resets_the_backoff() {
        let mut samples = stalled_connection(2);
        samples.extend([
            stall(1),
            Some((2 * STALL, STALL)),
            Some((3 * STALL, STALL)),
            Some((4 * STALL, 2 * STALL)),
            Some((5 * STALL, 3 * STALL)),
        ]);
        let run = run_script(State::Waiting, &samples).await;
        assert_eq!(run.restarts, ["stop", "start", "stop", "start"]);
        assert_eq!(*run.states.last().unwrap(), State::Starting);
        assert!(
            run.warnings[1].contains("restart 1 of 4"),
            "{:?}",
            run.warnings
        );

        let mut l = live("A", State::Waiting, None, None);
        l.stall_restarts = MAX_RESTARTS;
        l.stall_given_up = true;
        l.last_counters = Some(counters(0, 0));
        let video = Video::default();
        let state = Arc::new(Mutex::new(FakeStrom {
            flow: serde_json::to_value(flow::build("gw", &l.input, &video)).unwrap(),
            running: true,
            stats: [clean(1)].into_iter().collect(),
            calls: Vec::new(),
        }));
        let strom = StromClient::new(&fake_strom(state).await, None).unwrap();
        poll_flow(&strom, "gw", &video, &mut l, &Events::discard())
            .await
            .unwrap();
        assert_eq!(l.stall_restarts, 0);
        assert!(!l.stall_given_up);
    }

    /// An input whose uplink the gateway started `ago` and that has had no peer since.
    fn dialling_for(ago: Duration) -> Live {
        let mut l = live("A", State::Starting, None, None);
        l.dialling_since = Some(Instant::now() - ago);
        l
    }

    #[test]
    fn a_connect_is_late_only_past_the_threshold_and_only_the_first_one() {
        let now = Instant::now();
        let mut l = live("A", State::Starting, None, None);
        l.dialling_since = Some(now - Duration::from_secs(2));
        assert!(!connected_late(&mut l, now));
        assert_eq!(l.dialling_since, None, "the watch ends at the first peer");
        // Long after: a reconnect, not a first connect, is never late.
        assert!(!connected_late(&mut l, now + Duration::from_secs(60)));

        l.dialling_since = Some(now - LATE_CONNECT_AFTER);
        assert!(!connected_late(&mut l, now));
        l.dialling_since = Some(now - LATE_CONNECT_AFTER - Duration::from_millis(100));
        assert!(connected_late(&mut l, now));
        assert_eq!(l.dialling_since, None);
    }

    /// strom#1277 / the `late` CI run: the receiver comes up 35 s after the uplink
    /// started dialling. The first peer is restarted at once, with a warning that
    /// points at strom#1277, and only once: the new connection, and a stall on it
    /// later, are judged as usual, and the late restart does not count toward
    /// `MAX_RESTARTS`.
    #[tokio::test]
    async fn a_late_first_connect_is_restarted_once() {
        let run = run_script_on(
            dialling_for(Duration::from_secs(35)),
            &[
                None,
                Some((1_900, 0)),
                clean(1),
                clean(2),
                Some((3 * STALL, STALL)),
                Some((4 * STALL, 2 * STALL)),
            ],
        )
        .await;
        assert_eq!(
            run.states,
            [
                State::Waiting,
                State::Starting,
                State::Waiting,
                State::OnAir,
                State::Waiting,
                State::Starting
            ]
        );
        // The late connect's restart, then #127's stall restart.
        assert_eq!(run.restarts, ["stop", "start", "stop", "start"]);
        assert_eq!(run.warnings.len(), 2, "{:?}", run.warnings);
        let late = &run.warnings[0];
        assert!(late.contains("Venue — A"), "{late}");
        assert!(late.contains("more than 5 s"), "{late}");
        assert!(late.contains("strom/issues/1277"), "{late}");
        assert!(
            run.warnings[1].contains("restart 1 of 4"),
            "{:?}",
            run.warnings
        );
        assert_eq!(run.live.stall_restarts, 1);
        // The stall restart dials again, so its connect is watched again.
        assert!(run.live.dialling_since.is_some());
    }

    /// A receiver that is up: the peer appears within a second or two of the start.
    /// No restart, no warning, and the watch is off for good.
    #[tokio::test]
    async fn a_peer_that_appears_promptly_is_not_restarted() {
        let run = run_script_on(
            dialling_for(Duration::from_secs(1)),
            &[None, Some((0, 0)), clean(1), clean(2)],
        )
        .await;
        assert_eq!(
            run.states,
            [State::Waiting, State::Waiting, State::OnAir, State::OnAir]
        );
        assert!(run.restarts.is_empty(), "{:?}", run.restarts);
        assert!(run.warnings.is_empty(), "{:?}", run.warnings);
        assert_eq!(run.live.dialling_since, None);
    }

    /// An uplink that delivered, then lost its peer and connected again (libsrt's own
    /// reconnect), however long that took, is not a late first connect.
    #[tokio::test]
    async fn a_reconnect_after_delivering_is_not_restarted() {
        let mut samples = vec![clean(1), clean(2), clean(3)];
        samples.extend([None; 6]);
        samples.extend([clean(1), clean(2)]);
        let run = run_script_on(dialling_for(Duration::from_secs(1)), &samples).await;
        assert!(run.restarts.is_empty(), "{:?}", run.restarts);
        assert!(run.warnings.is_empty(), "{:?}", run.warnings);
        assert_eq!(*run.states.last().unwrap(), State::OnAir);
    }

    /// The one-second watch between ticks does the same: nothing while there is no
    /// peer, one restart at a late first peer, and nothing after.
    #[tokio::test]
    async fn the_connect_watch_restarts_a_late_first_connect_once() {
        let mut l = dialling_for(Duration::from_secs(35));
        let video = Video::default();
        let state = Arc::new(Mutex::new(FakeStrom {
            flow: serde_json::to_value(flow::build("gw", &l.input, &video)).unwrap(),
            running: true,
            stats: [None, Some((1_900, 0)), Some((10, 0))]
                .into_iter()
                .collect(),
            calls: Vec::new(),
        }));
        let strom = StromClient::new(&fake_strom(state.clone()).await, None).unwrap();
        let media = StromMedia::over(&strom, "gw");
        let events = Events::discard();
        assert_eq!(watch_connect(&media, &mut l, &events).await.unwrap(), None);
        assert!(l.dialling_since.is_some(), "still dialling");
        assert_eq!(
            watch_connect(&media, &mut l, &events).await.unwrap(),
            Some(State::Starting)
        );
        assert_eq!(l.dialling_since, None);
        assert_eq!(l.stall_restarts, 0);
        // The watch is off: no Strom call, no restart.
        assert_eq!(watch_connect(&media, &mut l, &events).await.unwrap(), None);
        let posts: Vec<String> = state
            .lock()
            .unwrap()
            .calls
            .iter()
            .filter(|c| c.starts_with("POST"))
            .map(|c| c.rsplit('/').next().unwrap().to_string())
            .collect();
        assert_eq!(posts, ["stop", "start"]);
        assert_eq!(
            state.lock().unwrap().stats.len(),
            1,
            "the third sample is never read"
        );
    }

    #[test]
    fn forgetting_the_counters_resets_the_stall_count() {
        let mut l = live("A", State::Waiting, None, None);
        l.last_counters = Some(counters(10, 10));
        l.stalled_polls = 1;
        l.forget_counters();
        assert_eq!(l.last_counters, None);
        assert_eq!(l.stalled_polls, 0);
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

    #[test]
    fn already_running_names_the_pid_and_the_way_out() {
        assert_eq!(
            AlreadyRunning { pid: 4242 }.to_string(),
            "already running (pid 4242); run 'open-live-ingest down' first"
        );
    }

    /// The app's executable is matched by the same name check as the CLI's.
    #[test]
    fn the_gateway_comm_matches_the_app_too() {
        let gateway = Exe::StartsWith(GATEWAY_COMM);
        assert!(comm_matches("open-live-ingest-app", gateway));
        assert!(comm_matches(
            "/Applications/Open Live Ingest.app/Contents/MacOS/open-live-ingest-app",
            gateway
        ));
        assert!(image_matches("open-live-ingest-app.exe", gateway));
    }

    /// `down` finds a running `up` by its command name, so the name must be what `ps`
    /// prints for this binary: a prefix of the package name, within Linux's 15-byte
    /// limit.
    #[test]
    fn the_gateway_comm_is_the_package_name_cut_to_fifteen_bytes() {
        assert!(env!("CARGO_PKG_NAME").starts_with(GATEWAY_COMM));
        assert!(GATEWAY_COMM.len() <= 15);
        assert_eq!(GATEWAY_COMM, "open-live-inges");
        assert_eq!(truncate_comm("strom"), "strom");
    }

    #[test]
    fn a_running_gateway_is_recognised_on_linux_and_macos() {
        let gateway = Exe::StartsWith(GATEWAY_COMM);
        // Linux: the comm truncated to 15 characters, with ps's trailing newline.
        assert!(comm_matches("open-live-inges\n", gateway));
        // macOS: the full path of the executable.
        assert!(comm_matches("/usr/local/bin/open-live-ingest\n", gateway));
        assert!(!comm_matches("strom\n", gateway));
        assert!(!comm_matches("", gateway));
        // A folder named after the gateway does not make what is in it one.
        assert!(!comm_matches("/opt/open-live-ingest/bin/strom\n", gateway));
    }

    /// #95 W1: Strom is matched by its file name only. A reused pid whose path merely
    /// contains "strom" (a home folder, `nostromo`) is not a Strom, so the reclaim
    /// and `down` never signal it.
    #[test]
    fn strom_is_matched_by_its_file_name_not_its_path() {
        let strom = Exe::Is("strom");
        assert!(comm_matches("strom\n", strom));
        assert!(comm_matches(
            "/Applications/Open Live Ingest.app/Contents/MacOS/strom\n",
            strom
        ));
        assert!(!comm_matches("/Users/lindstrom/.cargo/bin/cargo\n", strom));
        assert!(!comm_matches("/Users/bergstrom/bin/sleep\n", strom));
        assert!(!comm_matches("nostromo\n", strom));
        assert!(!comm_matches("/usr/bin/strom-helper\n", strom));
        assert!(!comm_matches("", strom));
        assert!(!comm_matches("strom\n", Exe::Is("")));
        // A name longer than Linux's 15 bytes matches its cut form.
        assert!(comm_matches(
            "open-live-inges\n",
            Exe::Is("open-live-ingest")
        ));

        assert!(image_matches("Strom.EXE", strom));
        assert!(image_matches("strom.exe", strom));
        assert!(image_matches(
            "C:\\Program Files\\Open Live Ingest\\strom.exe",
            strom
        ));
        assert!(!image_matches("nostromo.exe", strom));
        assert!(!image_matches("lindstrom.exe", strom));
        assert!(!image_matches("strom-helper.exe", strom));
        assert!(image_matches("PING.EXE", Exe::Is("ping.exe")));
    }

    /// What `tasklist /FI "PID eq <pid>" /FO CSV /NH` prints, parsed on every OS.
    #[test]
    fn a_running_gateway_and_strom_are_recognised_on_windows() {
        let gateway = "\"open-live-ingest.exe\",\"4242\",\"Console\",\"1\",\"12,345 K\"\r\n";
        let image = parse_tasklist_csv(gateway, 4242).expect("a row for pid 4242");
        assert_eq!(image, "open-live-ingest.exe");
        assert!(image_matches(&image, Exe::StartsWith(GATEWAY_COMM)));
        assert!(!image_matches(&image, Exe::Is("strom")));

        let strom = "\"Strom.EXE\",\"77\",\"Services\",\"0\",\"51,204 K\"\r\n";
        let image = parse_tasklist_csv(strom, 77).expect("a row for pid 77");
        assert!(image_matches(&image, Exe::Is("strom")));
        assert!(!image_matches(&image, Exe::StartsWith(GATEWAY_COMM)));
        assert!(!image_matches(&image, Exe::Is("")));
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
