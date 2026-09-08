//! The three things the command line does: bring devices up, tear them down, report.
//!
//! `up` runs in the foreground and owns what it started, so a deliberate stop stops
//! the feeds. It ignores SIGHUP, though: a dropped SSH connection must not take a
//! venue off air, and that is the difference between logging out and pulling a plug.
//!
//! `down` and `status` are stateless. Neither talks to a running `up` about what
//! exists — they work it out from the persisted source ids and derived flow ids, so
//! they behave the same whether `up` is running, finished, or was killed outright.

use crate::identity;
use crate::openlive::client::OpenLiveClient;
use crate::openlive::registration;
use crate::session::{self, ActiveInput};
use crate::state::SharedState;
use crate::strom::client::{CaptureDevice, Reachability, StromClient};
use crate::strom::process;
use crate::strom::{flow, supervisor};
use anyhow::{bail, Context, Result};
use open_live_gateway_types::config::GatewayConfig;
use open_live_gateway_types::status::InputState;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::{info, warn};

/// Where the pid of a running `up` is kept, beside the source-id state.
fn pidfile(cfg: &GatewayConfig) -> PathBuf {
    PathBuf::from(&cfg.open_live.state_path).with_extension("pid")
}

/// Devices to bring up, after filtering and selection.
fn choose_devices(
    all_devices: Vec<CaptureDevice>,
    include_virtual: bool,
    selection: Option<&str>,
) -> Result<Vec<CaptureDevice>> {
    let mut devices: Vec<CaptureDevice> = if include_virtual {
        all_devices
    } else {
        all_devices
            .into_iter()
            .filter(|d| !session::is_probably_virtual(d))
            .collect()
    };
    devices.sort_by(|a, b| a.display_name.cmp(&b.display_name));

    let Some(selection) = selection else {
        return Ok(devices);
    };

    // Matched by id or by name, never by position. A device list changes between
    // runs — a camera reconnecting is enough — so `--devices 2` can silently mean a
    // different camera today than it did yesterday.
    let mut chosen = Vec::new();
    for wanted in selection.split(',') {
        let wanted = wanted.trim();
        if wanted.is_empty() {
            continue;
        }
        let needle = wanted.to_lowercase();

        let matches: Vec<&CaptureDevice> = devices
            .iter()
            .filter(|d| {
                d.id.eq_ignore_ascii_case(wanted) || d.display_name.to_lowercase().contains(&needle)
            })
            .collect();

        match matches.as_slice() {
            [device] => chosen.push((*device).clone()),
            [] => {
                bail!("no device matches {wanted:?}. Run `open-live-gateway devices` to see them.")
            }
            // Refuse rather than guess: starting the wrong camera is worse than
            // asking again with a longer name.
            several => bail!(
                "{wanted:?} matches {} devices ({}). Use a longer name or an id.",
                several.len(),
                several
                    .iter()
                    .map(|d| d.display_name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }
    if chosen.is_empty() {
        bail!("--devices selected nothing");
    }
    Ok(chosen)
}

/// Lists what Strom can see, so an operator can pick before starting anything.
pub async fn devices(cfg: GatewayConfig) -> Result<()> {
    let local_strom = process::ensure_running(&cfg).await?;
    let strom = StromClient::new(&cfg.strom.url, cfg.strom.api_key.as_deref())?;
    let all = strom
        .devices("video_source")
        .await
        .context("asking Strom what capture devices it can see")?;

    let mut all = all;
    all.sort_by(|a, b| a.display_name.cmp(&b.display_name));

    if all.is_empty() {
        println!("Strom reports no video sources on this machine.");
    } else {
        println!("{:<30} ID", "DEVICE");
        for device in &all {
            let note = if session::is_probably_virtual(device) {
                "virtual — skipped unless --all"
            } else {
                ""
            };
            println!(
                "{:<30} {:<24} {}",
                truncate(&device.display_name, 30),
                truncate(&device.id, 24),
                note
            );
        }
        println!();
        println!("`up` starts all of these except the virtual ones.");
        println!("Pick some with `up --devices \"FaceTime\"` or by id.");
    }

    // Started only to ask; do not leave it behind.
    local_strom.shutdown();
    Ok(())
}

/// Brings every chosen device up and supervises it until stopped.
pub async fn up(
    cfg: GatewayConfig,
    gateway_id: String,
    include_virtual: bool,
    selection: Option<String>,
) -> Result<()> {
    // Strom is not assumed to be running: adopt one if it is, start a headless one if
    // not. Only a Strom started here is stopped again at the end.
    let local_strom = process::ensure_running(&cfg).await?;
    let strom = StromClient::new(&cfg.strom.url, cfg.strom.api_key.as_deref())?;

    let devices = choose_devices(
        strom
            .devices("video_source")
            .await
            .context("asking Strom what capture devices it can see")?,
        include_virtual,
        selection.as_deref(),
    )?;

    if devices.is_empty() {
        bail!("Strom reports no capture devices (virtual ones are skipped; --all includes them)");
    }

    // The cloud host may be configured or discovered; without it there is nowhere to
    // send, and starting anyway would produce feeds that go nowhere.
    let open_live = if cfg.open_live.register {
        Some(registration::client_from(&cfg)?)
    } else {
        None
    };
    let cloud_host = resolve_cloud_host(&cfg, open_live.as_ref()).await?;

    // Each device costs an encoder and its own share of the uplink, which is easy to
    // overlook when one command starts all of them.
    let total_mbps = devices.len() as f64 * cfg.app.video.bitrate_kbps as f64 / 1000.0;
    println!(
        "Bringing up {} device(s) to {cloud_host} — about {:.0} Mbps in total.",
        devices.len(),
        total_mbps
    );

    let state = Arc::new(SharedState::new(gateway_id.clone(), &cfg));
    let state_path = PathBuf::from(&cfg.open_live.state_path);

    // Clear anything a previous run left behind before claiming the same ids.
    let input_ids: Vec<String> = devices
        .iter()
        .map(|d| session::input_id_for_device(&d.id))
        .collect();
    let reaped = session::reap_orphans(&strom, &gateway_id, &input_ids).await;
    if reaped > 0 {
        info!(reaped, "removed flows left over from an earlier run");
    }

    let mut taken: BTreeSet<u16> = session::ports_in_config(&cfg);
    let mut active: BTreeMap<String, ActiveInput> = BTreeMap::new();

    for device in &devices {
        let port = session::allocate_port(&cfg.app.uplink, &taken)?;
        taken.insert(port);

        let input = session::input_for_device(&cfg.app, device, port, &cloud_host);
        match session::start_input(&strom, &gateway_id, &cfg.gateway.name, &input).await {
            Ok(started) => {
                state.add_input(&input, &gateway_id);
                tokio::spawn(supervisor::supervise(
                    Arc::clone(&state),
                    StromClient::new(&cfg.strom.url, cfg.strom.api_key.as_deref())?,
                    input.clone(),
                    gateway_id.clone(),
                    cfg.gateway.name.clone(),
                ));
                if let Some(_client) = &open_live {
                    tokio::spawn(registration::reconcile_forever(
                        Arc::clone(&state),
                        registration::client_from(&cfg)?,
                        input.clone(),
                        cfg.gateway.name.clone(),
                        state_path.clone(),
                    ));
                }
                if let Err(err) =
                    identity::record_started(&state_path, &input.id, &device.display_name, port)
                {
                    warn!(input = %input.id, %err, "could not record the started input");
                }
                active.insert(input.id.clone(), started);
            }
            // One camera failing must not stop the rest of the venue coming up.
            Err(err) => warn!(device = %device.display_name, %err, "could not start this device"),
        }
    }

    if active.is_empty() {
        bail!("no device could be started");
    }

    print_summary(&devices, &active);
    write_pidfile(&cfg)?;
    // Recorded so a `down` after a hard kill can stop the Strom we started, which
    // would otherwise keep holding the capture devices.
    if let Some(pid) = local_strom.managed_pid() {
        identity::record_strom_pid(&state_path, pid).ok();
    }

    let outcome = wait_for_stop(Arc::clone(&state)).await;

    println!("\nStopping.");
    teardown(&strom, open_live.as_ref(), &state_path, &active).await;
    let managed = local_strom.managed_pid().is_some();
    local_strom.shutdown();
    if managed {
        println!("  strom — stopped");
        identity::forget_strom_pid(&state_path).ok();
    }
    remove_pidfile(&cfg);
    outcome
}

/// Resolves where feeds are sent: configured, or asked of Open Live.
async fn resolve_cloud_host(
    cfg: &GatewayConfig,
    open_live: Option<&OpenLiveClient>,
) -> Result<String> {
    if let Some(host) = cfg.app.uplink.host.as_deref().filter(|h| !h.is_empty()) {
        return Ok(host.to_string());
    }
    let client = open_live.context(
        "no cloud Strom host configured, and registration is off so there is nobody to ask",
    )?;
    client
        .cloud_strom_host()
        .await?
        .context("Open Live did not report a Strom host — set [app.uplink] host")
}

fn print_summary(devices: &[CaptureDevice], active: &BTreeMap<String, ActiveInput>) {
    println!();
    println!(
        "{:<28} {:<22} {:<7} REGISTERED ADDRESS",
        "DEVICE", "SOURCE", "PORT"
    );
    for device in devices {
        let id = session::input_id_for_device(&device.id);
        if let Some(a) = active.get(&id) {
            println!(
                "{:<28} {:<22} {:<7} {}",
                truncate(&device.display_name, 28),
                truncate(a.input.name.as_deref().unwrap_or(&a.input.id), 22),
                a.input.uplink.port,
                a.input.uplink.cloud_uri()
            );
        }
    }
    println!();
    println!("Assign these in Studio and activate the production; they read as inactive until");
    println!("something is receiving them. Ctrl-C stops and removes them.");
    println!();
}

fn truncate(s: &str, width: usize) -> String {
    if s.chars().count() <= width {
        return s.to_string();
    }
    s.chars().take(width.saturating_sub(1)).collect::<String>() + "…"
}

/// Blocks until asked to stop, logging state changes as they happen.
///
/// Appended lines rather than a redrawn screen: this is read over SSH, in tmux, and
/// in a log file after the fact.
async fn wait_for_stop(state: Arc<SharedState>) -> Result<()> {
    let mut previous: BTreeMap<String, InputState> = BTreeMap::new();
    let mut ticker = tokio::time::interval(std::time::Duration::from_secs(2));

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                for input in state.snapshot().inputs {
                    let changed = previous.get(&input.id) != Some(&input.state);
                    if changed {
                        previous.insert(input.id.clone(), input.state);
                        let detail = match input.state {
                            InputState::Running => "on air — the far end is receiving".to_string(),
                            InputState::Stalled => "waiting for a receiver".to_string(),
                            InputState::Unknown => "lost contact with Strom".to_string(),
                            InputState::Failed => input
                                .last_error
                                .clone()
                                .unwrap_or_else(|| "failed".to_string()),
                            InputState::Starting | InputState::Provisioning => {
                                "starting".to_string()
                            }
                            InputState::Idle => "idle".to_string(),
                        };
                        println!("  {} — {detail}", input.id);
                    }
                }
            }
            _ = stop_signal() => return Ok(()),
        }
    }
}

/// Resolves when the operator asks to stop. SIGHUP is deliberately not included.
#[cfg(unix)]
async fn stop_signal() {
    use tokio::signal::unix::{signal, SignalKind};

    // Ignoring SIGHUP is the point: closing an SSH session, or a link dropping
    // mid-show, must not tear down a venue's feeds.
    let mut hangup = signal(SignalKind::hangup()).ok();
    let mut terminate = match signal(SignalKind::terminate()) {
        Ok(s) => s,
        Err(_) => {
            let _ = tokio::signal::ctrl_c().await;
            return;
        }
    };

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => return,
            _ = terminate.recv() => return,
            _ = async {
                match hangup.as_mut() {
                    Some(h) => { h.recv().await; }
                    None => std::future::pending::<()>().await,
                }
            } => {
                info!("ignoring SIGHUP so the feeds survive a closed session");
            }
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
    state_path: &Path,
    active: &BTreeMap<String, ActiveInput>,
) {
    for (input_id, input) in active {
        if let Some(client) = open_live {
            delete_source(client, state_path, input_id).await;
        }
        if let Err(err) = session::stop_input(strom, input).await {
            warn!(input = %input_id, %err, "could not remove the flow");
        } else {
            println!("  {input_id} — stopped and removed");
        }
        identity::forget_started(state_path, input_id).ok();
    }
}

/// Removes an input's Open Live source and forgets its id.
async fn delete_source(client: &OpenLiveClient, state_path: &Path, input_id: &str) {
    let Ok(mut persisted) = identity::load(state_path) else {
        return;
    };
    if let Some(source_id) = persisted.source_ids.remove(input_id) {
        if let Err(err) = client.delete_source(&source_id).await {
            warn!(input = %input_id, %err, "could not remove the Open Live source");
            return;
        }
        let _ = identity::store(state_path, &persisted);
    }
}

/// Stops a running `up` if there is one, then removes whatever is left.
pub async fn down(cfg: GatewayConfig, gateway_id: String) -> Result<()> {
    let mut signalled = false;
    if let Some(pid) = read_pidfile(&cfg) {
        println!("Asking the running gateway (pid {pid}) to stop.");
        signalled = true;
        signal_stop(pid);
        // Give it time to tear its own flows down; the sweep below covers whatever it
        // did not manage, including the case where it was already gone.
        for _ in 0..30 {
            if !process_alive(pid) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
        remove_pidfile(&cfg);
    }

    let strom = StromClient::new(&cfg.strom.url, cfg.strom.api_key.as_deref())?;
    // Deliberately not a hard failure here: if Strom has gone away its flows are
    // already stopped, and the Open Live sources still need removing. Refusing would
    // leave them in Studio with no way to clear them.
    let strom_reachable = matches!(strom.probe().await, Reachability::Ok { .. });
    if !strom_reachable {
        println!(
            "Strom at {} is not reachable; removing the Open Live sources and leaving \
             its flows alone.",
            cfg.strom.url
        );
    }
    let state_path = PathBuf::from(&cfg.open_live.state_path);
    let persisted = identity::load(&state_path).unwrap_or_default();
    let input_ids = recorded_inputs(&persisted);

    if input_ids.is_empty() {
        // A running `up` tears down what it started, so an empty record here means it
        // did the work — not that there was nothing to do.
        if signalled {
            println!("It stopped and removed its feeds.");
        } else {
            println!("Nothing recorded as running.");
        }
        return Ok(());
    }

    let open_live = if cfg.open_live.register {
        registration::client_from(&cfg).ok()
    } else {
        None
    };

    for input_id in input_ids {
        let flow_id = flow::flow_id(&gateway_id, &input_id);
        if strom_reachable && matches!(strom.get_flow(&flow_id).await, Ok(Some(_))) {
            strom.stop_flow(&flow_id).await.ok();
            if let Err(err) = strom.delete_flow(&flow_id).await {
                warn!(input = %input_id, %err, "could not remove the flow");
            }
        }
        if let Some(client) = &open_live {
            delete_source(client, &state_path, &input_id).await;
        }
        identity::forget_started(&state_path, &input_id).ok();
        println!("  {input_id} — torn down");
    }

    stop_managed_strom(&state_path);
    Ok(())
}

/// Stops a Strom recorded as started by us, for the case where `up` was killed hard
/// and left it running. An adopted Strom is never recorded, so never stopped.
fn stop_managed_strom(state_path: &Path) {
    let Ok(persisted) = identity::load(state_path) else {
        return;
    };
    let Some(pid) = persisted.strom_pid else {
        return;
    };
    if process_alive(pid) {
        println!("  strom (pid {pid}) — stopping the one we started");
        signal_stop(pid);
    }
    identity::forget_strom_pid(state_path).ok();
}

/// Every input the last run recorded, whether or not it reached Open Live.
fn recorded_inputs(persisted: &identity::PersistedState) -> Vec<String> {
    let mut ids: BTreeSet<String> = persisted.started.keys().cloned().collect();
    ids.extend(persisted.source_ids.keys().cloned());
    ids.into_iter().collect()
}

/// Reports what is running, without needing a running `up`.
pub async fn status(cfg: GatewayConfig, gateway_id: String) -> Result<()> {
    let strom = StromClient::new(&cfg.strom.url, cfg.strom.api_key.as_deref())?;
    let state_path = PathBuf::from(&cfg.open_live.state_path);
    let persisted = identity::load(&state_path).unwrap_or_default();

    match read_pidfile(&cfg) {
        Some(pid) if process_alive(pid) => println!("Gateway running (pid {pid}).\n"),
        _ => println!("No gateway process running; reporting from Strom directly.\n"),
    }

    let input_ids = recorded_inputs(&persisted);
    if input_ids.is_empty() {
        println!("Nothing recorded as running.");
        return Ok(());
    }

    let open_live = if cfg.open_live.register {
        registration::client_from(&cfg).ok()
    } else {
        None
    };
    let sources = match &open_live {
        Some(client) => client.list_sources().await.unwrap_or_default(),
        None => Vec::new(),
    };

    println!("{:<22} {:<10} {:<14} UPLINK", "INPUT", "FLOW", "OPEN LIVE");
    for input_id in &input_ids {
        let flow_id = flow::flow_id(&gateway_id, input_id);
        let flow_state = match strom.get_flow(&flow_id).await {
            Ok(Some(f)) if f.state.running => "running",
            Ok(Some(_)) => "stopped",
            Ok(None) => "missing",
            Err(_) => "unreachable",
        };
        let source_state = persisted
            .source_ids
            .get(input_id)
            .and_then(|source_id| sources.iter().find(|s| &s.id == source_id))
            .map(|s| s.status.clone())
            .unwrap_or_else(|| "not registered".to_string());

        let uplink = match strom.srt_uplink(&flow_id).await {
            Ok(Some(u)) => match (u.send_rate_mbps, u.rtt_ms) {
                (Some(rate), Some(rtt)) => format!("{rate:.2} Mbps, rtt {rtt:.1} ms"),
                _ => format!("{} bytes sent", u.bytes_sent),
            },
            _ => "-".to_string(),
        };

        println!(
            "{:<22} {:<10} {:<14} {}",
            truncate(input_id, 22),
            flow_state,
            truncate(&source_state, 14),
            uplink
        );
    }
    Ok(())
}

fn write_pidfile(cfg: &GatewayConfig) -> Result<()> {
    let path = pidfile(cfg);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::write(&path, std::process::id().to_string()).context("writing the pidfile")
}

fn read_pidfile(cfg: &GatewayConfig) -> Option<u32> {
    std::fs::read_to_string(pidfile(cfg))
        .ok()?
        .trim()
        .parse()
        .ok()
}

fn remove_pidfile(cfg: &GatewayConfig) {
    std::fs::remove_file(pidfile(cfg)).ok();
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

    fn device(id: &str, name: &str) -> CaptureDevice {
        CaptureDevice {
            id: id.to_string(),
            display_name: name.to_string(),
            provider: None,
        }
    }

    fn devices() -> Vec<CaptureDevice> {
        vec![
            device("d1", "OBS Virtual Camera"),
            device("d2", "FaceTime HD Camera"),
            device("d3", "DeckLink Mini Recorder"),
        ]
    }

    #[test]
    fn virtual_devices_are_skipped_by_default() {
        let chosen = choose_devices(devices(), false, None).unwrap();
        let names: Vec<&str> = chosen.iter().map(|d| d.display_name.as_str()).collect();
        assert_eq!(names, vec!["DeckLink Mini Recorder", "FaceTime HD Camera"]);
    }

    #[test]
    fn all_includes_virtual_devices() {
        let chosen = choose_devices(devices(), true, None).unwrap();
        assert_eq!(chosen.len(), 3);
    }

    /// Numbers must match the printed list, which is sorted — otherwise `--devices 1`
    /// picks something other than the first row an operator can see.
    #[test]
    fn a_device_can_be_selected_by_name_fragment() {
        let chosen = choose_devices(devices(), false, Some("facetime")).unwrap();
        assert_eq!(chosen.len(), 1);
        assert_eq!(chosen[0].display_name, "FaceTime HD Camera");
    }

    #[test]
    fn a_device_can_be_selected_by_id() {
        let chosen = choose_devices(devices(), true, Some("d1")).unwrap();
        assert_eq!(chosen[0].display_name, "OBS Virtual Camera");
    }

    #[test]
    fn several_devices_can_be_named_at_once() {
        let chosen = choose_devices(devices(), true, Some("facetime,obs")).unwrap();
        assert_eq!(chosen.len(), 2);
    }

    /// Selection must not depend on position: a device list changes between runs,
    /// and a number that meant one camera yesterday can mean another today.
    #[test]
    fn positions_are_not_accepted_as_selectors() {
        assert!(
            choose_devices(devices(), false, Some("1")).is_err(),
            "a bare number must not select by position"
        );
    }

    /// Starting the wrong camera is worse than asking again, so an ambiguous name
    /// is refused rather than resolved by picking the first match.
    #[test]
    fn an_ambiguous_name_is_refused() {
        let mut list = devices();
        list.push(device("d4", "Camera Link Pro"));
        let err = choose_devices(list, true, Some("camera"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("matches"), "got {err}");
    }

    #[test]
    fn an_unknown_name_is_an_error() {
        assert!(choose_devices(devices(), false, Some("nonexistent")).is_err());
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
