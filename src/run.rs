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

use crate::config::{self, format_port_range, mask_passphrase, Config, UplinkMode, Video};
use crate::devices;
use crate::flow::{self, Input};
use crate::local_strom::{self, LocalStrom};
use crate::openlive::{drifted, listener_port, OpenLiveClient, ServerInfo, Source, SourcePayload};
use crate::strom::StromClient;
use anyhow::{bail, Context, Result};
use serde::Serialize;
use std::collections::{BTreeSet, HashMap};
use std::fmt;
use std::ops::RangeInclusive;
use std::time::Duration;
use strom_types::FlowId;
use tracing::{info, warn};

const TICK: Duration = Duration::from_secs(10);

/// Quiet polls before an uplink that was delivering is reported inactive.
/// Hysteresis: every status flip rewrites the Open Live source, and CouchDB keeps a
/// revision per write, so one quiet poll must not flap the verdict.
const QUIET_TOLERANCE: u32 = 3;

/// The local Strom, adopted or started, and a client for it. Held for as long as the
/// command runs: dropping it stops a Strom we started.
async fn connect_strom(cfg: &Config) -> Result<(LocalStrom, StromClient)> {
    let local = local_strom::ensure_running(cfg).await?;
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

/// Where feeds are sent, and which ports the links use.
struct Cloud {
    host: String,
    ports: RangeInclusive<u16>,
    port_source: PortSource,
}

impl Cloud {
    fn describe_ports(&self) -> String {
        format!(
            "SRT ports {} from {}",
            format_port_range(&self.ports),
            self.port_source
        )
    }
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
async fn resolve_cloud(cfg: &Config, open_live: Option<&OpenLiveClient>) -> Result<Cloud> {
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
                warn!(%err, "could not ask Open Live for its server info; using uplink.host and uplink.port_range from the settings");
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
        info.as_ref().and_then(|i| i.srt_port_range.clone()),
        info.as_ref().and_then(|i| i.srt_port_lease.as_deref()),
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
    published: Option<RangeInclusive<u16>>,
    lease_status: Option<&str>,
) -> Result<(RangeInclusive<u16>, PortSource)> {
    match mode {
        UplinkMode::Listener => local.map(|range| (range, PortSource::Settings)).context(
            "uplink.mode is \"listener\", so uplink.port_range must name this machine's SRT ports",
        ),
        UplinkMode::Caller => match (published, local) {
            (Some(published), Some(local)) => {
                if local != published {
                    warn!(
                        settings = %format_port_range(&local),
                        open_live = %format_port_range(&published),
                        "uplink.port_range is ignored: in caller mode the cloud Strom owns its ports, and Open Live publishes its range"
                    );
                }
                Ok((published, PortSource::OpenLive))
            }
            (Some(published), None) => Ok((published, PortSource::OpenLive)),
            (None, Some(local)) => Ok((local, PortSource::Settings)),
            (None, None) => {
                let why = match lease_status {
                    Some("pending") => "Open Live is still waiting for its SRT port range from Strom (it retries every minute); wait and try again",
                    Some("unsupported") => "Open Live's Strom does not lease SRT ports; upgrade it",
                    Some("disabled") => "Open Live has SRT port leasing disabled",
                    _ => "Open Live publishes no SRT port range; upgrade it, or turn registration on",
                };
                bail!("no SRT port range for the cloud Strom: {why}, or set uplink.port_range in the settings")
            }
        },
    }
}

/// Lists what Strom can see, so an operator can pick before starting anything.
pub async fn devices(cfg: Config, json: bool) -> Result<()> {
    // Started only to ask, if started at all; dropped again on the way out.
    let (_local, strom) = connect_strom(&cfg).await?;
    let mut all = strom
        .devices()
        .await
        .context("asking Strom what capture devices it can see")?;
    all.sort_by(|a, b| a.name.cmp(&b.name));
    let report: Vec<DeviceReport> = all
        .iter()
        .map(|d| DeviceReport {
            name: d.name.clone(),
            id: d.id.clone(),
            virtual_: devices::is_probably_virtual(d),
        })
        .collect();
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print!("{}", render_devices(&report));
    }
    Ok(())
}

/// One capture device, as `devices` reports it.
#[derive(Debug, Serialize)]
struct DeviceReport {
    name: String,
    id: String,
    /// Skipped by `up` unless `--all`: it produces nothing on its own.
    #[serde(rename = "virtual")]
    virtual_: bool,
}

fn render_devices(all: &[DeviceReport]) -> String {
    if all.is_empty() {
        return "Strom reports no video sources on this machine.\n".to_string();
    }
    let mut out = format!("{:<30} {:<24} NOTE\n", "DEVICE", "ID");
    for device in all {
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum State {
    /// Start requested, first poll pending.
    Starting,
    /// The flow runs but nothing is receiving the uplink. Normal until the source is
    /// assigned to a production.
    Waiting,
    /// Bytes are leaving. Reported to Open Live as `active`.
    OnAir,
    /// Strom is not answering. Not evidence that the feed stopped: the source's
    /// status is left as it was.
    Unreachable,
    Failed(String),
}

impl State {
    fn describe(&self) -> String {
        match self {
            State::Starting => "starting".to_string(),
            State::Waiting => "waiting for a receiver".to_string(),
            State::OnAir => "on air, the far end is receiving".to_string(),
            State::Unreachable => "lost contact with Strom".to_string(),
            State::Failed(err) => format!("failed: {err}"),
        }
    }
}

/// Brings every chosen device up and keeps it registered until stopped.
pub async fn up(
    cfg: Config,
    include_virtual: bool,
    selection: Option<String>,
    test_pattern: bool,
) -> Result<()> {
    let gateway_id = cfg.gateway.resolved_id();
    let (local_strom, strom) = connect_strom(&cfg).await?;
    let open_live = open_live_client(&cfg)?;

    let chosen = if test_pattern {
        Vec::new()
    } else {
        let all = strom
            .devices()
            .await
            .context("asking Strom what capture devices it can see")?;
        devices::choose(all, include_virtual, selection.as_deref())?
    };
    if chosen.is_empty() && !test_pattern {
        bail!("Strom reports no capture devices. Virtual ones are skipped unless --all; --test streams a test pattern instead.");
    }

    // Without somewhere to send there is no point starting: the feeds would go nowhere.
    let cloud = resolve_cloud(&cfg, open_live.as_ref()).await?;
    let inputs = devices::inputs_for(
        &chosen,
        test_pattern,
        &cfg.gateway.name,
        &cfg.uplink,
        &cfg.capture,
        &cloud.host,
        &cloud.ports,
    )?;

    // Each input costs an encoder and its share of the uplink, which is easy to
    // overlook when one command starts all of them.
    println!(
        "Bringing up {} input(s) to {}, {}, about {:.0} Mbps in total.",
        inputs.len(),
        cloud.host,
        cloud.describe_ports(),
        inputs.len() as f64 * cfg.video.bitrate_kbps as f64 / 1000.0
    );

    // When the range is Open Live's, so is the port inside it: register first and
    // take the port Open Live wrote into the source, then build the flows to it.
    let mut inputs = inputs;
    let mut source_ids = match (&open_live, cloud.port_source) {
        (Some(client), PortSource::OpenLive) => {
            assign_ports_from_open_live(client, &mut inputs, &cloud.ports).await?
        }
        _ => HashMap::new(),
    };

    clear_conflicting_flows(&strom, &gateway_id, &inputs).await?;

    let mut live = Vec::new();
    for input in inputs {
        let flow_id = flow::flow_id(&gateway_id, &input.id);
        let source_id = source_ids.remove(&input.id);
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
            }),
            // One camera failing must not stop the rest of the venue coming up.
            Err(err) => warn!(input = %input.name, %err, "could not start this input"),
        }
    }
    if live.is_empty() {
        bail!("no input could be started");
    }

    print_summary(&live);
    if let Err(err) = write_pidfile() {
        warn!(%err, "could not write the pidfile; `down` will still find the flows and sources");
    }

    let outcome = run_until_stopped(&strom, open_live.as_ref(), &gateway_id, &cfg, &mut live).await;

    println!("\nStopping.");
    teardown(&strom, open_live.as_ref(), &cfg.gateway.name, &live).await;
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
    /// The stored source has no usable port (none, or one outside the range Open
    /// Live now publishes, left by an older run): ask Open Live to reassign it.
    Reassign { source_id: String },
    /// No source yet: create one and let Open Live pick the port.
    Create,
}

/// Pure, so the three cases can be tested without a server.
fn plan_port(existing: Option<&Source>, range: &RangeInclusive<u16>) -> PortPlan {
    match existing {
        None => PortPlan::Create,
        Some(stored) => match listener_port(&stored.address) {
            Some(port) if range.contains(&port) => PortPlan::Keep {
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
/// cloud side stays stable across restarts. Returns each input's source id.
async fn assign_ports_from_open_live(
    client: &OpenLiveClient,
    inputs: &mut [Input],
    range: &RangeInclusive<u16>,
) -> Result<HashMap<String, String>> {
    let sources = client
        .list_sources()
        .await
        .context("listing Open Live's sources to assign ports")?;
    let mut ids = HashMap::new();
    for input in inputs.iter_mut() {
        let existing = sources.iter().find(|s| s.name == input.name);
        let (source_id, address) = match plan_port(existing, range) {
            PortPlan::Keep { source_id, port } => {
                input.endpoint.port = port;
                ids.insert(input.id.clone(), source_id);
                continue;
            }
            PortPlan::Create => {
                input.endpoint.port = 0;
                let payload = SourcePayload::new(
                    &input.name,
                    &input.endpoint.cloud_uri(),
                    false,
                    input.endpoint.latency_ms,
                );
                let created = client
                    .create_source(&payload)
                    .await
                    .with_context(|| format!("registering {:?} with Open Live", input.name))?;
                info!(input = %input.name, source_id = %created.id, "registered the source with Open Live");
                (created.id, created.address)
            }
            PortPlan::Reassign { source_id } => {
                input.endpoint.port = 0;
                let payload = SourcePayload::new(
                    &input.name,
                    &input.endpoint.cloud_uri(),
                    false,
                    input.endpoint.latency_ms,
                );
                client
                    .patch_source(&source_id, &payload)
                    .await
                    .with_context(|| {
                        format!("asking Open Live for a new port for {:?}", input.name)
                    })?;
                // PATCH answers with the source, but the client does not read it back;
                // the list is the one shape it already parses.
                let address = client
                    .list_sources()
                    .await?
                    .into_iter()
                    .find(|s| s.id == source_id)
                    .map(|s| s.address)
                    .with_context(|| {
                        format!(
                            "Open Live lost the source for {:?} while reassigning its port",
                            input.name
                        )
                    })?;
                (source_id, address)
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
        ids.insert(input.id.clone(), source_id);
    }
    Ok(ids)
}

/// Removes our own leftovers from a run that ended without cleanup, and any other
/// flow already holding one of the cameras about to be opened. Two pipelines on one
/// device contend for frames, which presents as stutter rather than as an error.
async fn clear_conflicting_flows(
    strom: &StromClient,
    gateway_id: &str,
    inputs: &[Input],
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
        let why = if ours {
            "left over from an earlier run"
        } else {
            "which was holding one of these devices"
        };
        println!("  removed {:?}, {why}", existing.name);
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

fn print_summary(live: &[Live]) {
    println!("\n{:<36} {:<6} REGISTERED ADDRESS", "INPUT", "PORT");
    for l in live {
        println!(
            "{:<36} {:<6} {}",
            truncate(&l.input.name, 36),
            l.input.endpoint.port,
            mask_passphrase(&l.input.endpoint.cloud_uri())
        );
    }
    println!("\nAssign these in Studio and activate the production; they read as inactive until");
    println!("something is receiving them. Ctrl-C stops and removes them.\n");
}

fn truncate(s: &str, width: usize) -> String {
    if s.chars().count() <= width {
        return s.to_string();
    }
    s.chars().take(width.saturating_sub(1)).collect::<String>() + "…"
}

/// One tick every few seconds, sequentially over every input: no shared state, no
/// task per input, and one Open Live list call per tick.
async fn run_until_stopped(
    strom: &StromClient,
    open_live: Option<&OpenLiveClient>,
    gateway_id: &str,
    cfg: &Config,
    live: &mut [Live],
) -> Result<()> {
    let mut ticker = tokio::time::interval(TICK);
    let stop = stop_signal();
    tokio::pin!(stop);
    loop {
        tokio::select! {
            _ = ticker.tick() => {
                for l in live.iter_mut() {
                    l.state = match poll_flow(strom, gateway_id, &cfg.video, l).await {
                        Ok(state) => state,
                        Err(err) => {
                            warn!(input = %l.input.name, %err, "lost contact with Strom, retrying next tick");
                            l.last_bytes = None;
                            State::Unreachable
                        }
                    };
                }
                if let Some(client) = open_live {
                    if let Err(err) = reconcile_sources(client, live).await {
                        // Never fatal: the feeds are already flowing. Retried next tick.
                        warn!(%err, "could not update the Open Live sources, retrying next tick");
                    }
                }
                // Appended lines rather than a redrawn screen: this is read over SSH,
                // in tmux, and in a log file after the fact.
                for l in live.iter_mut() {
                    if l.shown.as_ref() != Some(&l.state) {
                        println!("  {} — {}", l.input.name, l.state.describe());
                        l.shown = Some(l.state.clone());
                    }
                }
            }
            _ = &mut stop => return Ok(()),
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
        return Ok(if strom.start_flow(&l.flow_id).await? {
            State::Starting
        } else {
            State::Failed("Strom did not report the flow running after start".to_string())
        });
    }

    let Some(sample) = strom.srt_uplink(&l.flow_id).await? else {
        l.last_bytes = None;
        l.quiet_polls = 0;
        return Ok(State::Waiting);
    };
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
async fn reconcile_sources(client: &OpenLiveClient, live: &mut [Live]) -> Result<()> {
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
        );
        match existing {
            Some(stored) => {
                if drifted(stored, &desired) {
                    client.patch_source(&stored.id, &desired).await?;
                }
                l.source_id = Some(stored.id.clone());
            }
            None => {
                let created = client.create_source(&desired).await?;
                info!(input = %l.input.name, source_id = %created.id, "registered the source with Open Live");
                l.source_id = Some(created.id);
            }
        }
    }
    Ok(())
}

/// Resolves on Ctrl-C or SIGTERM. SIGHUP is deliberately swallowed: closing an SSH
/// session, or a link dropping mid-show, must not tear down a venue's feeds. The
/// signal streams live for the whole run, so a signal is never lost between polls.
#[cfg(unix)]
async fn stop_signal() {
    use tokio::signal::unix::{signal, Signal, SignalKind};

    async fn recv(sig: &mut Option<Signal>) {
        match sig {
            Some(s) => {
                s.recv().await;
            }
            None => std::future::pending().await,
        }
    }

    let mut interrupt = signal(SignalKind::interrupt()).ok();
    let mut terminate = signal(SignalKind::terminate()).ok();
    let mut hangup = signal(SignalKind::hangup()).ok();
    if interrupt.is_none() && terminate.is_none() {
        let _ = tokio::signal::ctrl_c().await;
        return;
    }
    loop {
        tokio::select! {
            _ = recv(&mut interrupt) => return,
            _ = recv(&mut terminate) => return,
            _ = recv(&mut hangup) => info!("ignoring SIGHUP so the feeds survive a closed session"),
        }
    }
}

#[cfg(not(unix))]
async fn stop_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

/// Stops and removes everything the run created.
async fn teardown(
    strom: &StromClient,
    open_live: Option<&OpenLiveClient>,
    gateway_name: &str,
    live: &[Live],
) {
    for l in live {
        match remove_flow(strom, &l.flow_id).await {
            Ok(()) => println!("  {} — flow stopped and removed", l.input.name),
            Err(err) => warn!(input = %l.input.name, %err, "could not remove the flow"),
        }
        if let (Some(client), Some(id)) = (open_live, &l.source_id) {
            match client.delete_source(id).await {
                Ok(()) => println!("  {} — source removed from Open Live", l.input.name),
                Err(err) => warn!(input = %l.input.name, %err, "could not remove the source"),
            }
        }
    }
    // Anything of ours still there goes too: a source recreated by a race, or one
    // whose id we never learned. Matched on the name prefix, so sources belonging to
    // another gateway or made by hand are never touched.
    if let Some(client) = open_live {
        sweep_sources(client, gateway_name).await;
    }
}

/// Removes every source carrying this gateway's name prefix. Returns how many.
async fn sweep_sources(client: &OpenLiveClient, gateway_name: &str) -> usize {
    let prefix = devices::name_prefix(gateway_name);
    let sources = match client.list_sources().await {
        Ok(sources) => sources,
        Err(err) => {
            warn!(%err, "could not list Open Live sources to clean up");
            return 0;
        }
    };
    let mut removed = 0;
    for source in sources.iter().filter(|s| s.name.starts_with(&prefix)) {
        match client.delete_source(&source.id).await {
            Ok(()) => {
                println!("  {} — source removed from Open Live", source.name);
                removed += 1;
            }
            Err(err) => warn!(source = %source.name, %err, "could not remove the source"),
        }
    }
    removed
}

/// Stops a running `up` if there is one, then removes whatever is left.
pub async fn down(cfg: Config) -> Result<()> {
    let gateway_id = cfg.gateway.resolved_id();

    if let Some(pid) = read_pidfile() {
        if is_running_gateway(pid) {
            println!("Asking the running gateway (pid {pid}) to stop.");
            signal_stop(pid);
            for _ in 0..60 {
                if !process_alive(pid) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            if process_alive(pid) {
                println!("It has not finished stopping; removing what is left anyway.");
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
                        println!("  {} — flow stopped and removed", f.name);
                        removed += 1;
                    }
                    Err(err) => warn!(flow = %f.name, %err, "could not remove the flow"),
                }
            }
        }
        // Deliberately not fatal: if Strom has gone its flows are already stopped, and
        // the Open Live sources still need removing or they sit in Studio forever.
        Err(err) => println!(
            "Strom at {} is not reachable ({err}); leaving its flows alone.",
            cfg.strom.url
        ),
    }
    if let Some(client) = open_live_client(&cfg)? {
        removed += sweep_sources(&client, &cfg.gateway.name).await;
    }
    local_strom::stop_recorded();
    if removed == 0 {
        println!("Nothing of ours left to remove.");
    }
    Ok(())
}

/// Reports what is running, from Strom and Open Live directly.
pub async fn status(cfg: Config, json: bool) -> Result<()> {
    let report = gather_status(&cfg).await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print!("{}", render_status(&report));
    }
    Ok(())
}

/// Everything `status` reports, gathered once and rendered as text or JSON. `status
/// --watch` gathers it again every few seconds and draws it; see `tui`.
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

/// The sender's view of the SRT link, as Strom's srtsink reports it. Counters are
/// totals for the current connection; a reconnect starts them over.
#[derive(Debug, Default, Serialize)]
pub struct UplinkStatsReport {
    pub send_rate_mbps: Option<f64>,
    pub rtt_ms: Option<f64>,
    pub bytes_sent: Option<u64>,
    pub bandwidth_mbps: Option<f64>,
    pub negotiated_latency_ms: Option<u32>,
    pub packets_sent: Option<u64>,
    pub packets_sent_lost: Option<u64>,
    pub packets_sent_dropped: Option<u64>,
    pub packets_retransmitted: Option<u64>,
    pub snd_buf_level_ms: Option<u32>,
}

#[derive(Debug, Serialize)]
pub struct SourceReport {
    pub id: String,
    pub status: String,
}

pub async fn gather_status(cfg: &Config) -> Result<StatusReport> {
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
    let (uplink, uplink_error) = match resolve_cloud(cfg, open_live.as_ref()).await {
        Ok(cloud) => (
            Some(UplinkReport {
                host: cloud.host,
                ports: format_port_range(&cloud.ports),
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
                    bandwidth_mbps: s.bandwidth_mbps,
                    negotiated_latency_ms: s.negotiated_latency_ms,
                    packets_sent: s.packets_sent,
                    packets_sent_lost: s.packets_sent_lost,
                    packets_sent_dropped: s.packets_sent_dropped,
                    packets_retransmitted: s.packets_retransmitted,
                    snd_buf_level_ms: s.snd_buf_level_ms,
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

fn render_status(r: &StatusReport) -> String {
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
        let source = input
            .source
            .as_ref()
            .map(|s| s.status.as_str())
            .unwrap_or("not registered");
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
    // Linux truncates the command name to 15 characters.
    process_named(pid, "open-live-gatew")
}

/// Whether the pid is alive and its command name contains `needle`.
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
    String::from_utf8_lossy(&out.stdout).contains(needle)
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

#[cfg(not(unix))]
fn process_alive(_pid: u32) -> bool {
    false
}

#[cfg(not(unix))]
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
                        ..UplinkStatsReport::default()
                    }),
                    source: Some(SourceReport {
                        id: "s1".to_string(),
                        status: "active".to_string(),
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
        assert_eq!(v["inputs"][1]["uplink"], serde_json::Value::Null);
        assert_eq!(v["inputs"][1]["source"], serde_json::Value::Null);
    }

    #[test]
    fn devices_render_as_a_table_and_as_json() {
        let all = vec![
            DeviceReport {
                name: "DeckLink SDI".to_string(),
                id: "decklink-0".to_string(),
                virtual_: false,
            },
            DeviceReport {
                name: "OBS Virtual Camera".to_string(),
                id: "obs-0".to_string(),
                virtual_: true,
            },
        ];
        let text = render_devices(&all);
        assert!(text.starts_with("DEVICE"));
        assert!(text.contains("OBS Virtual Camera"));
        assert!(text.contains("virtual, skipped unless --all"));
        assert_eq!(
            render_devices(&[]),
            "Strom reports no video sources on this machine.\n"
        );

        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&all).unwrap()).unwrap();
        assert_eq!(v[0]["id"], "decklink-0");
        assert_eq!(v[0]["virtual"], false);
        assert_eq!(v[1]["virtual"], true);
    }

    const CLOUD: RangeInclusive<u16> = 47110..=47129;
    const LOCAL: RangeInclusive<u16> = 9000..=9019;

    /// Several venues share the cloud Strom, so the venue must not pick its ports.
    fn stored(address: &str) -> Source {
        Source {
            id: "src-1".to_string(),
            name: "Venue — Cam".to_string(),
            address: address.to_string(),
            stream_type: "srt".to_string(),
            status: "inactive".to_string(),
            latency: None,
        }
    }

    #[test]
    fn a_stored_source_keeps_its_port_only_while_it_is_inside_the_range() {
        let range = 47100..=47109;
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
    fn in_caller_mode_the_published_range_wins_over_the_settings() {
        assert_eq!(
            choose_ports(UplinkMode::Caller, Some(LOCAL), Some(CLOUD), Some("leased")).unwrap(),
            (CLOUD, PortSource::OpenLive)
        );
        assert_eq!(
            choose_ports(UplinkMode::Caller, None, Some(CLOUD), Some("leased")).unwrap(),
            (CLOUD, PortSource::OpenLive)
        );
    }

    /// An older Open Live, or one that cannot lease, leaves the settings in charge.
    #[test]
    fn in_caller_mode_the_settings_are_the_fallback_when_nothing_is_published() {
        for status in [None, Some("pending"), Some("unsupported"), Some("disabled")] {
            assert_eq!(
                choose_ports(UplinkMode::Caller, Some(LOCAL), None, status).unwrap(),
                (LOCAL, PortSource::Settings),
                "{status:?}"
            );
        }
    }

    /// Neither side has a range: the error names both fixes, and says when the fix
    /// is simply to wait for Open Live.
    #[test]
    fn in_caller_mode_no_range_at_all_is_an_error_that_names_the_fixes() {
        let err = choose_ports(UplinkMode::Caller, None, None, None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("uplink.port_range"), "{err}");
        assert!(err.contains("upgrade"), "{err}");
        assert!(!err.contains("waiting"), "{err}");

        let err = choose_ports(UplinkMode::Caller, None, None, Some("pending"))
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
                Some(CLOUD),
                Some("leased")
            )
            .unwrap(),
            (LOCAL, PortSource::Settings)
        );
        let err = choose_ports(UplinkMode::Listener, None, Some(CLOUD), Some("leased"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("uplink.port_range"), "{err}");
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
}
