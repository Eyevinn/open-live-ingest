//! Reproduction for the late-connect uplink (#111): the uplink flow the gateway
//! builds for `up --test`, started on a real Strom against an SRT listener that comes
//! up only after the flow has been running for a while, as it does when the app goes
//! live before a production is active.
//!
//! The flow is `flow::build`'s own, with the settings of the report: test pattern,
//! 1280x720 at 30/1, H.264 software, 6000 kbit/s CBR, caller, latency 200 ms.
//!
//! Two receivers, picked with `--receiver`:
//!
//! - `srt-live-transmit` (the default) writes the stream to a file, and `ffprobe`
//!   counts the video and audio frames in it afterwards. It always reads, so it never
//!   stalls: it shows the backlog burst at connect, not the #111 drop.
//! - `strom` is the receiver of Eyevinn/strom#1277 and of Open Live: a flow built
//!   through a Strom's API (`--receiver-strom`, by default the sending Strom) with a
//!   `builtin.mpegtssrt_input` listener (`srt://:<port>?mode=listener`, latency 125,
//!   the two properties Open Live sets) whose `video_out` and `audio_out_0` each go
//!   through a `queue` into a `fakesink` (`async: false`, `sync: true`, or `false`
//!   with `--receiver-sync false`). The queues keep GStreamer from reporting an
//!   invalid construction; strom#1277 stalls with and without them. What the
//!   receiver got is read from that Strom before the flow is stopped: its `srtsrc`'s
//!   `packets_received` from `/api/flows/<id>/srt-stats`, and each sink's
//!   `last-message` (the sinks run with `silent: false`), which names the last
//!   buffer's PTS. Strom has no per-sink frame counter: a base sink's `stats` is a
//!   GstStructure, which its element property read skips, and a probe's
//!   `sample_count` is reset at every broadcast. So the media check is "the receiver
//!   got packets, and both sinks got buffers". The PTS is gated too, anchored to the
//!   connect like the drop counters: both sinks must have had a buffer by the first
//!   sample (+10 s), and each sink's last PTS, read there and again at the end of the
//!   window, must have advanced by the wall time between those two reads, less 5 s.
//!   Both sinks are also read at every later sample, and each step from one reading to
//!   the next (the last step ends at the end-of-window read) must advance by the wall
//!   time between them, less the same 5 s. A receiver that freezes without making the
//!   sender drop (its sinks stop getting buffers while SRT still acknowledges) fails on
//!   that, instead of passing on a buffer it got early on, and so does one that freezes
//!   mid-window and then catches up, which the first-to-last comparison alone would
//!   pass; so does one that only starts decoding later in the window, instead of
//!   passing on a short stretch of wall time at the end.
//!
//! It reads the uplink's `/api/flows/<id>/srt-stats` the moment the peer appears and
//! then every 10 s to the end of the window. The caller's SRT counters start at 0 for
//! each connection, so the check uses their absolute values (a baseline of 0), not
//! growth since the first sample: the backlog burst at connect is already in that
//! first sample, and a drop burst there must count. It fails (exit status 1) when
//! `packets_sent_dropped` is 1 % or more of `packets_sent` in any sample, or when the
//! receiver got no video or no audio (with the Strom receiver, also when a sink's PTS
//! stopped short of the end of the window, see above). Every sample counts, not only +10 s and the
//! end: a stalled Strom receiver drops nothing at +10 s, and the caller reconnects at
//! about +50 s with its counters back at 0, so a check on those two samples alone
//! would pass it. A sample with no peer, or with `packets_sent` lower than the one
//! before, is reported as a reconnect. `snd_buf_level_ms` is printed with every
//! sample: a send buffer that holds more than the SRT latency is the backlog the
//! issue first suspected.
//!
//! Needs a running Strom (the pinned release, with GStreamer's `srt`, `x264` and
//! `avenc_aac`). The default receiver needs `srt-live-transmit` and `ffprobe` on the
//! PATH; the Strom receiver needs nothing else (and decodes, so its Strom needs
//! `avdec_h264` and `avdec_aac` or equivalents). No Open Live, no device.
//!
//! ```text
//! # the late connect: the listener starts 35 s after the flow
//! cargo run --example late_connect -- --listener-delay 35
//! # the control: the listener is up before the flow starts
//! cargo run --example late_connect -- --listener-delay 0
//!
//! # AC1 of #111 (strom#1277): Strom's mpegtssrt_input with clock-synced sinks, late
//! cargo run --example late_connect -- --receiver strom --listener-delay 35
//! # its controls: the receiver first, or sinks that do not sync to the clock
//! cargo run --example late_connect -- --receiver strom --listener-delay 0
//! cargo run --example late_connect -- --receiver strom --listener-delay 35 --receiver-sync false
//! # the receiver on a second Strom, as in strom#1277
//! cargo run --example late_connect -- --receiver strom --receiver-strom http://127.0.0.1:8092
//! ```
//!
//! Expected with the Strom receiver, from svensson00's runs on strom#1277 (Strom
//! 0.8.1 on both ends, localhost) and until a Strom release fixes it:
//!
//! | run | expected |
//! |---|---|
//! | `--listener-delay 35`, `sync: true` | FAIL: 0 dropped at connect and +10 s, then about 6,055 sent and 6,055 dropped per 10 s from before +20 s (about 25 % dropped at +20 s, rising), the RTT unchanged across those samples, and a reconnect by about +50 s after which the counters restart clean |
//! | `--listener-delay 0`, `sync: true` | PASS: 0 dropped |
//! | `--listener-delay 35 --receiver-sync false` | PASS: 0 dropped |
//!
//! `--restart-on-late-connect` tries the ingest-side workaround that #111's
//! 2026-11-24 triage proposed for `up` (not shipped): it looks for the uplink's peer
//! every `CONNECT_POLL` (1 s), and when the first peer appears more than
//! `LATE_CONNECT_AFTER` (5 s) after the flow started, it stops and starts the flow
//! once, so the caller connects again into the receiver that is now running. The
//! window and its counters then start at that second connect. Measured with the
//! Strom receiver (PR #170, late-connect run 45490377672, Strom 0.8.2 on both ends),
//! and expected until a Strom release fixes strom#1277:
//!
//! | run | expected |
//! |---|---|
//! | `--listener-delay 35 --restart-on-late-connect`, `sync: true` | FAIL: the restart connects about 1 s after the receiver came up and stalls it the same way (0 dropped at +10 s, about 28 % at +20 s, 52 % at +30 s), until the caller reconnects on its own between +30 and +40 s (31-41 s after the receiver came up) |
//!
//! So the stall depends on how long the receiver has been running when the caller
//! connects, not on how long the caller has been dialling, and an immediate restart
//! does not avoid it. That is why `up` has no such restart; #127's stall restart
//! covers the later window.
//!
//! With the default `srt-live-transmit` receiver both the late run and the control
//! pass (0 dropped, #120 and the Mac run on #111).
//!
//! On Strom v0.8.4 on both ends (PR #171, late-connect run 45500947777) the late first
//! connect is clean: `--listener-delay 35` with `sync: true` PASSes (0 of 38,295
//! dropped, PTS advanced 50.0 s in 50.0 s). The restart still stalls the receiver
//! (0 dropped at +10 and +20 s, about 25 % at +30 s, sinks empty until +40 s), so a
//! reconnect right after the first connect still hits strom#1277, which stays open:
//!
//! | run | expected on v0.8.4 |
//! |---|---|
//! | `--listener-delay 35`, `sync: true` | PASS |
//! | `--listener-delay 0`, `sync: true` | PASS |
//! | `--listener-delay 35 --receiver-sync false` | PASS |
//! | `--listener-delay 35 --restart-on-late-connect`, `sync: true` | FAIL |
//!
//! Options: `--strom <url>` (default `http://127.0.0.1:8080`, or `OLI_STROM_URL`),
//! `--port <n>` (default 47199), `--receiver-port <n>` (default `--port`; set it
//! to put a relay or a link emulator between the two), `--listener-delay <s>` (default 35),
//! `--window <s>` (default 60), `--receiver <srt-live-transmit|strom>` (default
//! `srt-live-transmit`), `--receiver-strom <url>` (default `--strom`, or
//! `OLI_RECEIVER_STROM_URL`), `--receiver-sync <true|false>` (default true),
//! `--out <file>` (default a file in the temp dir; `srt-live-transmit` only),
//! `--keep` to leave the flows in Strom afterwards, `--restart-on-late-connect` (see
//! above). `OLI_STROM_API_KEY` is sent when
//! set, to the receiving Strom too unless `OLI_RECEIVER_STROM_API_KEY` is set.

use anyhow::{bail, Context, Result};
use open_live_ingest::config::{Uplink, Video};
use open_live_ingest::flow::{self, Input, Source, TEST_INPUT_ID};
use open_live_ingest::strom::{StromClient, PINNED_VERSION};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Stdio};
use std::time::{Duration, Instant};
use strom_types::api::{SrtCallerStats, SrtRole};
use strom_types::block::Position;
use strom_types::flow::FlowProperties;
use strom_types::{BlockInstance, Element, Flow, FlowId, Link, PropertyValue};

const GATEWAY_ID: &str = "repro-late-connect";
const LATENCY_MS: u32 = 200;
/// The latency Open Live sets on its `mpegtssrt_input` sources.
const RECEIVER_LATENCY_MS: u64 = 125;
/// The share of `packets_sent` that `packets_sent_dropped` may reach, counted from 0 at
/// connect.
const MAX_DROP_RATIO: f64 = 0.01;
/// How long to wait for the uplink to report a peer once the listener is up.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
/// `--restart-on-late-connect`: how often the peer is looked for while dialling.
const CONNECT_POLL: Duration = Duration::from_secs(1);
/// `--restart-on-late-connect`: a first peer later than this after the flow started
/// counts as a late connect and restarts the flow once.
const LATE_CONNECT_AFTER: Duration = Duration::from_secs(5);
/// Time between two samples of the uplink's counters.
const SAMPLE_INTERVAL: Duration = Duration::from_secs(10);
/// How far a Strom receiver sink's PTS may fall behind the wall time between its first
/// and its last read (the queue, the SRT latency and the read itself), before the run
/// counts the receiver as frozen.
const PTS_SLACK: Duration = Duration::from_secs(5);
/// How long after connect the Strom receiver's first PTS reading may be taken: the
/// first sample (+10 s), plus `PTS_SLACK` for the reads before it. A receiver whose
/// sinks have no buffer yet by then started late, and the PTS gate fails it, rather
/// than measuring from whenever its first buffers arrived (from the +60 s sample, the
/// wall time to cover would be about 0).
const FIRST_PTS_WITHIN: Duration = SAMPLE_INTERVAL.saturating_add(PTS_SLACK);

/// The Strom receiver's flow: the SRT input block and the two sinks behind it.
const RX_INPUT: &str = "receiver";
const RX_VIDEO_SINK: &str = "videosink";
const RX_AUDIO_SINK: &str = "audiosink";

#[derive(Clone, Copy, PartialEq, Eq)]
enum ReceiverKind {
    SrtLiveTransmit,
    Strom,
}

struct Options {
    strom: String,
    port: u16,
    receiver_port: Option<u16>,
    listener_delay: Duration,
    window: Duration,
    out: PathBuf,
    keep: bool,
    receiver: ReceiverKind,
    receiver_strom: Option<String>,
    receiver_sync: bool,
    restart_on_late_connect: bool,
}

impl Options {
    fn listen_port(&self) -> u16 {
        self.receiver_port.unwrap_or(self.port)
    }
}

fn options() -> Result<Options> {
    let mut o = Options {
        strom: std::env::var("OLI_STROM_URL").unwrap_or_else(|_| "http://127.0.0.1:8080".into()),
        port: 47199,
        receiver_port: None,
        listener_delay: Duration::from_secs(35),
        window: Duration::from_secs(60),
        out: std::env::temp_dir().join("oli-late-connect.ts"),
        keep: false,
        receiver: ReceiverKind::SrtLiveTransmit,
        receiver_strom: std::env::var("OLI_RECEIVER_STROM_URL").ok(),
        receiver_sync: true,
        restart_on_late_connect: false,
    };
    // --receiver-strom and --receiver-sync given without --receiver strom.
    let mut strom_receiver_option = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = || args.next().with_context(|| format!("{arg} needs a value"));
        match arg.as_str() {
            "--strom" => o.strom = value()?,
            "--port" => o.port = value()?.parse().context("--port")?,
            "--receiver-port" => {
                o.receiver_port = Some(value()?.parse().context("--receiver-port")?)
            }
            "--listener-delay" => {
                o.listener_delay =
                    Duration::from_secs(value()?.parse().context("--listener-delay")?)
            }
            "--window" => o.window = Duration::from_secs(value()?.parse().context("--window")?),
            "--out" => o.out = value()?.into(),
            "--keep" => o.keep = true,
            "--restart-on-late-connect" => o.restart_on_late_connect = true,
            "--receiver" => {
                o.receiver = match value()?.as_str() {
                    "srt-live-transmit" => ReceiverKind::SrtLiveTransmit,
                    "strom" => ReceiverKind::Strom,
                    other => bail!("--receiver is srt-live-transmit or strom, not {other}"),
                }
            }
            "--receiver-strom" => {
                o.receiver_strom = Some(value()?);
                strom_receiver_option = true;
            }
            "--receiver-sync" => {
                o.receiver_sync = value()?
                    .parse()
                    .context("--receiver-sync is true or false")?;
                strom_receiver_option = true;
            }
            other => bail!("unknown argument {other}; see the header of examples/late_connect.rs"),
        }
    }
    if o.window < SAMPLE_INTERVAL {
        bail!("--window must be at least 10 s");
    }
    if o.receiver != ReceiverKind::Strom && strom_receiver_option {
        bail!("--receiver-strom and --receiver-sync need --receiver strom");
    }
    Ok(o)
}

#[tokio::main]
async fn main() -> Result<ExitCode> {
    let o = options()?;
    let api_key = std::env::var("OLI_STROM_API_KEY").ok();
    let strom = StromClient::new(&o.strom, api_key.as_deref())?;
    let version = strom.version().await.context("reaching Strom")?;
    println!(
        "Strom {version} at {} (gateway pinned to {PINNED_VERSION})",
        o.strom
    );

    let input = Input {
        id: TEST_INPUT_ID.to_string(),
        name: "late-connect reproduction (#111)".to_string(),
        source: Source::Test {
            resolution: "1280x720".to_string(),
            framerate: "30/1".to_string(),
        },
        endpoint: Uplink {
            latency_ms: LATENCY_MS,
            ..Uplink::default()
        }
        .endpoint("127.0.0.1", o.port),
    };
    let video = Video {
        encoder_preference: "software".to_string(),
        bitrate_kbps: 6000,
        rate_control: "cbr".to_string(),
        ..Video::default()
    };
    let flow = flow::build(GATEWAY_ID, &input, &video);
    let id = flow.id;
    println!("uplink: {}", input.endpoint.venue_uri());

    // The Strom receiver's flow is created up front, so a Strom that rejects it fails
    // the run before the uplink starts; only starting it is timed.
    let rx_strom = match o.receiver {
        ReceiverKind::SrtLiveTransmit => None,
        ReceiverKind::Strom => {
            let url = o.receiver_strom.as_deref().unwrap_or(&o.strom);
            let key = std::env::var("OLI_RECEIVER_STROM_API_KEY")
                .ok()
                .or_else(|| api_key.clone());
            let client = StromClient::new(url, key.as_deref())?;
            let version = client
                .version()
                .await
                .context("reaching the receiving Strom")?;
            let rx_flow = receiver_flow(o.listen_port(), o.receiver_sync);
            println!(
                "receiver: Strom {version} at {url}, mpegtssrt_input on srt://:{}?mode=listener, latency {RECEIVER_LATENCY_MS} ms, sinks sync {}",
                o.listen_port(),
                o.receiver_sync
            );
            client
                .delete_flow(&rx_flow.id)
                .await
                .context("removing an earlier run's receiver flow")?;
            client
                .create_flow(&rx_flow)
                .await
                .context("creating the receiver flow")?;
            Some((client, rx_flow.id))
        }
    };

    strom
        .delete_flow(&id)
        .await
        .context("removing an earlier run's flow")?;
    let mut receiver = None;
    let outcome = async {
        if o.listener_delay.is_zero() {
            receiver = Some(start_receiver(&o, rx_strom.as_ref()).await?);
            println!("control: listener up before the flow");
        }
        strom.create_flow(&flow).await?;
        if !strom.start_flow(&id).await? {
            bail!("Strom did not start the flow");
        }
        let started = Instant::now();
        println!("flow {id} started");

        if receiver.is_none() {
            println!("listener starts in {} s", o.listener_delay.as_secs());
            tokio::time::sleep(o.listener_delay).await;
            receiver = Some(start_receiver(&o, rx_strom.as_ref()).await?);
            println!(
                "listener up {} s after the flow",
                started.elapsed().as_secs()
            );
        }

        // With the proposed workaround, the peer is looked for every CONNECT_POLL, and
        // a late first peer restarts the flow once.
        let poll = if o.restart_on_late_connect {
            CONNECT_POLL
        } else {
            Duration::from_millis(250)
        };
        let mut first = wait_for_peer(&strom, &id, poll).await?;
        if o.restart_on_late_connect {
            let dialled = started.elapsed();
            if dialled > LATE_CONNECT_AFTER {
                sample("late peer", &first);
                println!(
                    "late connect: first peer {:.1} s after the flow started (over {} s), restarting the uplink once",
                    dialled.as_secs_f64(),
                    LATE_CONNECT_AFTER.as_secs()
                );
                strom.stop_flow(&id).await?;
                if !strom.start_flow(&id).await? {
                    bail!("Strom did not start the flow again after the late connect");
                }
                let restarted = Instant::now();
                first = wait_for_peer(&strom, &id, poll).await?;
                println!(
                    "late connect: connected again {:.1} s after the restart",
                    restarted.elapsed().as_secs_f64()
                );
            } else {
                println!(
                    "first peer {:.1} s after the flow started, in time: no restart",
                    dialled.as_secs_f64()
                );
            }
        }
        let connected = Instant::now();
        sample("connected", &first);
        let mut samples = vec![("connected".to_string(), totals(&first))];
        let mut reconnects = 0;
        let mut previous_sent = totals(&first).0;
        let mut next = SAMPLE_INTERVAL;
        // The Strom receiver's PTS readings, from the first sample at which both sinks
        // have had a buffer (`pts_reached_end` fails it unless that is the +10 s
        // sample) and then at every sample (`pts_kept_pace` checks each step).
        let mut pts_readings: Vec<PtsReading> = Vec::new();
        while next <= o.window {
            tokio::time::sleep(next.saturating_sub(connected.elapsed())).await;
            let label = format!("+{} s", next.as_secs());
            match strom.srt_uplink(&id).await? {
                Some(stats) if stats.packets_sent.is_some() => {
                    sample(&label, &stats);
                    let (sent, dropped) = totals(&stats);
                    if sent < previous_sent {
                        reconnects += 1;
                        println!("{label:>10}: packets_sent went down: the caller reconnected");
                    }
                    previous_sent = sent;
                    samples.push((label.clone(), (sent, dropped)));
                }
                _ => {
                    reconnects += 1;
                    previous_sent = 0;
                    println!("{label:>10}: no peer: the caller is reconnecting");
                }
            }
            if let Some((client, rx_id)) = &rx_strom {
                match pts_reading(client, rx_id).await? {
                    Some(reading) => {
                        println!(
                            "{label:>10}: receiver PTS video {} audio {}",
                            show_pts(reading.video),
                            show_pts(reading.audio)
                        );
                        pts_readings.push(reading);
                    }
                    // A sink whose last message was an event, not a buffer: no PTS to
                    // compare, so the next step spans two intervals. A frozen sink
                    // keeps its last buffer's message, so this does not hide a stall.
                    None if !pts_readings.is_empty() => {
                        println!("{label:>10}: receiver PTS not readable from both sinks, skipped");
                    }
                    None => {}
                }
            }
            next += SAMPLE_INTERVAL;
        }
        // The Strom receiver is read while its flow still runs.
        let strom_media = match &rx_strom {
            Some((client, rx_id)) => Some(strom_receiver_media(client, rx_id).await?),
            None => None,
        };
        Ok((samples, reconnects, strom_media, pts_readings, connected))
    }
    .await;

    if let Some(mut child) = receiver.flatten() {
        let _ = child.kill();
        let _ = child.wait();
    }
    if !o.keep {
        let _ = strom.stop_flow(&id).await;
        let _ = strom.delete_flow(&id).await;
        if let Some((client, rx_id)) = &rx_strom {
            let _ = client.stop_flow(rx_id).await;
            let _ = client.delete_flow(rx_id).await;
        }
    }
    let (samples, reconnects, strom_media, pts_readings, connected) = outcome?;

    let mut drops_ok = true;
    for (label, (sent, dropped)) in &samples {
        println!(
            "since connect, at {label}: {dropped} of {sent} sent dropped ({})",
            percent(*dropped, *sent)
        );
        drops_ok &= sample_ok(*sent, *dropped);
    }
    // Something was sent at some point: a run whose samples all read 0 sent fails.
    if !samples.iter().any(|(_, (sent, _))| *sent > 0) {
        println!("the uplink sent nothing during the window");
        drops_ok = false;
    }
    if reconnects > 0 {
        println!("the caller reconnected {reconnects} time(s) during the window");
    }

    let media_ok = match strom_media {
        Some(media) => {
            println!(
                "receiver: {} SRT packets received; last video buffer {}, last audio buffer {}",
                media.packets_received,
                media.video_pts.as_deref().unwrap_or("none"),
                media.audio_pts.as_deref().unwrap_or("none"),
            );
            let got_media = media.packets_received > 0
                && media.video_pts.is_some()
                && media.audio_pts.is_some();
            // Both run, so the log shows both gates' lines.
            let reached_end = pts_reached_end(connected, pts_readings.first(), &media);
            let kept_pace = pts_kept_pace(&pts_readings, &media);
            got_media && reached_end && kept_pace
        }
        None => {
            let (video_frames, audio_frames) = count_frames(&o.out)?;
            println!(
                "receiver: {video_frames} video and {audio_frames} audio frames in {}",
                o.out.display()
            );
            video_frames > 0 && audio_frames > 0
        }
    };

    if drops_ok && media_ok {
        println!("PASS: under 1 % dropped, and the receiver got video and audio");
        Ok(ExitCode::SUCCESS)
    } else {
        if !drops_ok {
            println!(
                "FAIL: packets_sent_dropped reached 1 % or more of packets_sent since connect"
            );
        }
        if !media_ok {
            println!(
                "FAIL: the receiver did not get both video and audio up to the end of the window"
            );
        }
        Ok(ExitCode::FAILURE)
    }
}

/// Starts the listener: `srt-live-transmit` as a child process (returned), or the
/// Strom receiver's flow, created beforehand (nothing to return; it is stopped by id).
async fn start_receiver(
    o: &Options,
    rx_strom: Option<&(StromClient, FlowId)>,
) -> Result<Option<Child>> {
    match rx_strom {
        None => spawn_receiver(o.listen_port(), &o.out).map(Some),
        Some((client, rx_id)) => {
            if !client.start_flow(rx_id).await? {
                bail!("the receiving Strom did not start the receiver flow");
            }
            Ok(None)
        }
    }
}

/// `srt-live-transmit` listening on `port`, writing what it receives to `out`.
fn spawn_receiver(port: u16, out: &Path) -> Result<Child> {
    let file = std::fs::File::create(out).with_context(|| format!("creating {}", out.display()))?;
    Command::new("srt-live-transmit")
        .arg(format!("srt://:{port}?mode=listener&latency={LATENCY_MS}"))
        .arg("file://con")
        .stdin(Stdio::null())
        .stdout(file)
        .spawn()
        .context("starting srt-live-transmit (is it on the PATH?)")
}

/// The receiver of strom#1277: Open Live's SRT input, `builtin.mpegtssrt_input` as a
/// listener with the two properties Open Live always sets, its video and audio each
/// through a queue into a fakesink. `sync` is the sinks' clock sync, the variable
/// strom#1277 found the stall to need. The sinks are not silent, so their
/// `last-message` names the last buffer they got.
fn receiver_flow(port: u16, sync: bool) -> Flow {
    let sink = || {
        props([
            ("sync", PropertyValue::Bool(sync)),
            ("async", PropertyValue::Bool(false)),
            ("silent", PropertyValue::Bool(false)),
        ])
    };
    Flow {
        id: flow::flow_id(GATEWAY_ID, "receiver"),
        name: "late-connect reproduction receiver (#111)".to_string(),
        elements: vec![
            element("videoqueue", "queue", HashMap::new(), (250.0, 0.0)),
            element(RX_VIDEO_SINK, "fakesink", sink(), (400.0, 0.0)),
            element("audioqueue", "queue", HashMap::new(), (250.0, 150.0)),
            element(RX_AUDIO_SINK, "fakesink", sink(), (400.0, 150.0)),
        ],
        blocks: vec![BlockInstance {
            id: RX_INPUT.to_string(),
            block_definition_id: "builtin.mpegtssrt_input".to_string(),
            name: Some("SRT Input".to_string()),
            properties: props([
                (
                    "srt_uri",
                    PropertyValue::String(format!("srt://:{port}?mode=listener")),
                ),
                ("latency", PropertyValue::UInt(RECEIVER_LATENCY_MS)),
            ]),
            position: Position { x: 0.0, y: 0.0 },
            runtime_data: None,
            computed_external_pads: None,
        }],
        links: vec![
            link(RX_INPUT, "video_out", "videoqueue", "sink"),
            link("videoqueue", "src", RX_VIDEO_SINK, "sink"),
            link(RX_INPUT, "audio_out_0", "audioqueue", "sink"),
            link("audioqueue", "src", RX_AUDIO_SINK, "sink"),
        ],
        running: false,
        gst_state: None,
        properties: FlowProperties::default(),
    }
}

fn props<const N: usize>(pairs: [(&str, PropertyValue); N]) -> HashMap<String, PropertyValue> {
    pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect()
}

fn element(
    id: &str,
    element_type: &str,
    properties: HashMap<String, PropertyValue>,
    position: (f32, f32),
) -> Element {
    Element {
        id: id.to_string(),
        element_type: element_type.to_string(),
        properties,
        pad_properties: HashMap::new(),
        position,
    }
}

fn link(from_id: &str, from_pad: &str, to_id: &str, to_pad: &str) -> Link {
    Link {
        from: format!("{from_id}:{from_pad}"),
        to: format!("{to_id}:{to_pad}"),
    }
}

/// What the Strom receiver got, read from that Strom's API.
struct StromMedia {
    /// `packets_received` summed over the receiving `srtsrc`'s peers.
    packets_received: u64,
    /// The PTS of the last buffer each sink got, from its `last-message`.
    video_pts: Option<String>,
    audio_pts: Option<String>,
    /// When the sinks were read.
    read_at: Instant,
}

/// Both Strom receiver sinks' last PTS, and when they were read.
struct PtsReading {
    video: Duration,
    audio: Duration,
    read_at: Instant,
}

/// Both sinks' last PTS, when both have had a buffer with a readable PTS.
async fn pts_reading(client: &StromClient, rx_id: &FlowId) -> Result<Option<PtsReading>> {
    let read_at = Instant::now();
    let video = last_pts(client, rx_id, RX_VIDEO_SINK).await?;
    let audio = last_pts(client, rx_id, RX_AUDIO_SINK).await?;
    Ok(
        match (
            video.as_deref().and_then(parse_pts),
            audio.as_deref().and_then(parse_pts),
        ) {
            (Some(video), Some(audio)) => Some(PtsReading {
                video,
                audio,
                read_at,
            }),
            _ => None,
        },
    )
}

/// The first reading was taken within `FIRST_PTS_WITHIN` of `connected` (the receiver
/// decoded from the start, not only late in the window), and each sink's PTS advanced,
/// from that reading to the end of the window, by the wall time between the two reads
/// less `PTS_SLACK`: the receiver kept getting media to the end, rather than freezing
/// after its first buffers.
fn pts_reached_end(connected: Instant, first: Option<&PtsReading>, media: &StromMedia) -> bool {
    let Some(first) = first else {
        println!("receiver PTS: never read from both sinks during the window");
        return false;
    };
    let first_at = first.read_at.saturating_duration_since(connected);
    let started_on_time = first_at <= FIRST_PTS_WITHIN;
    println!(
        "receiver PTS: first read from both sinks {:.1} s after connect ({})",
        first_at.as_secs_f64(),
        if started_on_time {
            "at the first sample"
        } else {
            "too late: no buffer on both sinks by the first sample"
        },
    );
    let wall = media.read_at.saturating_duration_since(first.read_at);
    let mut ok = true;
    for (kind, from, to) in [
        ("video", first.video, media.video_pts.as_deref()),
        ("audio", first.audio, media.audio_pts.as_deref()),
    ] {
        let to = to.and_then(parse_pts);
        let advanced = to.map(|to| to.saturating_sub(from));
        let reached = advanced.is_some_and(|a| a + PTS_SLACK >= wall);
        println!(
            "receiver {kind} PTS: {} to {}, advanced {} in {:.1} s of wall time ({})",
            show_pts(from),
            to.map_or_else(|| "none".to_string(), show_pts),
            advanced.map_or_else(|| "-".to_string(), |a| format!("{:.1} s", a.as_secs_f64())),
            wall.as_secs_f64(),
            if reached {
                "reached the end of the window"
            } else {
                "stopped short of the end of the window"
            },
        );
        ok &= reached;
    }
    started_on_time && ok
}

/// Each step between two consecutive PTS readings (every sample from the first
/// reading on, then the end-of-window read) advanced each sink's PTS by the wall time
/// between those reads, less `PTS_SLACK`. `pts_reached_end` compares only the first
/// and the last reading, so a receiver that froze mid-window and then caught up (its
/// PTS jumping forward after the freeze) would pass it; this fails it at the step
/// where it froze. A PTS that went backwards (a reset) counts as no advance. With no
/// readings there is nothing to step through, and `pts_reached_end` fails that.
fn pts_kept_pace(readings: &[PtsReading], media: &StromMedia) -> bool {
    let end = match (
        media.video_pts.as_deref().and_then(parse_pts),
        media.audio_pts.as_deref().and_then(parse_pts),
    ) {
        (Some(video), Some(audio)) => Some(PtsReading {
            video,
            audio,
            read_at: media.read_at,
        }),
        _ => None,
    };
    let steps: Vec<&PtsReading> = readings.iter().chain(end.as_ref()).collect();
    let mut ok = true;
    for pair in steps.windows(2) {
        let (from, to) = (pair[0], pair[1]);
        let wall = to.read_at.saturating_duration_since(from.read_at);
        for (kind, a, b) in [
            ("video", from.video, to.video),
            ("audio", from.audio, to.audio),
        ] {
            let advanced = b.saturating_sub(a);
            if advanced + PTS_SLACK < wall {
                println!(
                    "receiver {kind} PTS stalled mid-window: {} to {}, advanced {:.1} s in {:.1} s of wall time",
                    show_pts(a),
                    show_pts(b),
                    advanced.as_secs_f64(),
                    wall.as_secs_f64(),
                );
                ok = false;
            }
        }
    }
    if ok && steps.len() >= 2 {
        println!(
            "receiver PTS: kept pace at each of {} steps between readings",
            steps.len() - 1
        );
    }
    ok
}

/// A GStreamer clock time as printed in `last-message`, `H:MM:SS.NNNNNNNNN`; `None`
/// for anything else, including `99:99:99.999999999` (no time).
fn parse_pts(pts: &str) -> Option<Duration> {
    let mut parts = pts.trim().splitn(3, ':');
    let hours: u64 = parts.next()?.parse().ok()?;
    let minutes: u64 = parts.next()?.parse().ok()?;
    let (seconds, fraction) = parts.next()?.split_once('.')?;
    let seconds: u64 = seconds.parse().ok()?;
    if minutes >= 60 || seconds >= 60 || fraction.len() != 9 {
        return None;
    }
    let nanos: u32 = fraction.parse().ok()?;
    Some(Duration::new(hours * 3600 + minutes * 60 + seconds, nanos))
}

fn show_pts(pts: Duration) -> String {
    let s = pts.as_secs();
    format!(
        "{}:{:02}:{:02}.{:09}",
        s / 3600,
        s / 60 % 60,
        s % 60,
        pts.subsec_nanos()
    )
}

async fn strom_receiver_media(client: &StromClient, rx_id: &FlowId) -> Result<StromMedia> {
    let packets_received = client
        .srt_stats(rx_id)
        .await?
        .map(|stats| {
            stats
                .connections
                .values()
                .filter(|c| c.role == SrtRole::Source)
                .flat_map(|c| &c.callers)
                .filter_map(|c| c.packets_received)
                .sum()
        })
        .unwrap_or(0);
    let read_at = Instant::now();
    Ok(StromMedia {
        packets_received,
        video_pts: last_pts(client, rx_id, RX_VIDEO_SINK).await?,
        audio_pts: last_pts(client, rx_id, RX_AUDIO_SINK).await?,
        read_at,
    })
}

/// The PTS in a fakesink's `last-message`, when it has had a buffer: the message reads
/// `chain   ******* (videosink:sink) (… bytes, dts: …, pts: 0:00:12.345678900, …`.
async fn last_pts(client: &StromClient, rx_id: &FlowId, sink: &str) -> Result<Option<String>> {
    let Some(properties) = client.element_properties(rx_id, sink).await? else {
        return Ok(None);
    };
    let Some(PropertyValue::String(message)) = properties.get("last-message") else {
        return Ok(None);
    };
    if !message.starts_with("chain") {
        return Ok(None);
    }
    let pts = message
        .split_once("pts: ")
        .and_then(|(_, rest)| rest.split([',', ')']).next())
        .unwrap_or("unknown");
    Ok(Some(pts.trim().to_string()))
}

async fn wait_for_peer(strom: &StromClient, id: &FlowId, poll: Duration) -> Result<SrtCallerStats> {
    let deadline = Instant::now() + CONNECT_TIMEOUT;
    loop {
        // The caller entry appears before its counters do.
        if let Some(stats) = strom.srt_uplink(id).await? {
            if stats.packets_sent.is_some() {
                return Ok(stats);
            }
        }
        if Instant::now() > deadline {
            bail!(
                "the uplink reported no peer within {} s",
                CONNECT_TIMEOUT.as_secs()
            );
        }
        tokio::time::sleep(poll).await;
    }
}

fn sample(label: &str, s: &SrtCallerStats) {
    println!(
        "{label:>10}: sent {} dropped {} lost {} retransmitted {} snd_buf {} ms rtt {} ms",
        show(s.packets_sent),
        show(s.packets_sent_dropped),
        show(s.packets_sent_lost),
        show(s.packets_retransmitted),
        show(s.snd_buf_level_ms),
        show(s.rtt_ms),
    );
}

fn show<T: std::fmt::Display>(v: Option<T>) -> String {
    v.map_or_else(|| "-".to_string(), |v| v.to_string())
}

/// Packets sent and packets dropped since the connection began. The caller's counters
/// start at 0 per connection, so the absolute values include the burst at connect.
fn totals(s: &SrtCallerStats) -> (u64, u64) {
    (
        s.packets_sent.unwrap_or(0),
        s.packets_sent_dropped.unwrap_or(0),
    )
}

/// Something was sent, and under `MAX_DROP_RATIO` of it was dropped.
fn under_ratio(sent: u64, dropped: u64) -> bool {
    sent > 0 && (dropped as f64) < MAX_DROP_RATIO * sent as f64
}

/// One sample passes when under `MAX_DROP_RATIO` was dropped, or when nothing was sent
/// or dropped yet: with the listener up first, the peer appears before the first packet,
/// so the `connected` sample reads 0 of 0. That the uplink sent anything at all is
/// checked over the whole run.
fn sample_ok(sent: u64, dropped: u64) -> bool {
    (sent == 0 && dropped == 0) || under_ratio(sent, dropped)
}

fn percent(part: u64, whole: u64) -> String {
    if whole == 0 {
        return "nothing sent".to_string();
    }
    format!("{:.2} %", 100.0 * part as f64 / whole as f64)
}

/// Decoded video and audio frames in the received file, by `ffprobe -count_frames`.
fn count_frames(path: &Path) -> Result<(u64, u64)> {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-count_frames", "-of", "json"])
        .args(["-show_entries", "stream=codec_type,nb_read_frames"])
        .arg(path)
        .output()
        .context("running ffprobe (is it on the PATH?)")?;
    if !out.status.success() {
        bail!(
            "ffprobe failed on {}: {}",
            path.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).context("ffprobe output")?;
    let mut frames = (0, 0);
    for stream in json["streams"].as_array().into_iter().flatten() {
        let n = stream["nb_read_frames"]
            .as_str()
            .and_then(|n| n.parse().ok())
            .unwrap_or(0);
        match stream["codec_type"].as_str() {
            Some("video") => frames.0 += n,
            Some("audio") => frames.1 += n,
            _ => {}
        }
    }
    Ok(frames)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sample_before_the_first_packet_passes() {
        assert!(sample_ok(0, 0));
        assert!(sample_ok(6148, 0));
        assert!(!sample_ok(14087, 4590));
        assert!(!sample_ok(0, 1));
    }

    #[test]
    fn parses_gstreamer_clock_times() {
        assert_eq!(
            parse_pts("0:00:12.345678900"),
            Some(Duration::new(12, 345_678_900))
        );
        assert_eq!(parse_pts("1:02:03.000000001"), Some(Duration::new(3723, 1)));
        assert_eq!(parse_pts("99:99:99.999999999"), None);
        assert_eq!(parse_pts("unknown"), None);
        assert_eq!(parse_pts("0:00:12"), None);
        assert_eq!(show_pts(Duration::new(3723, 1)), "1:02:03.000000001");
    }

    fn media(video: &str, audio: &str, read_at: Instant) -> StromMedia {
        StromMedia {
            packets_received: 1,
            video_pts: Some(video.to_string()),
            audio_pts: Some(audio.to_string()),
            read_at,
        }
    }

    #[test]
    fn pts_gate_catches_a_frozen_receiver() {
        let connected = Instant::now();
        // The first reading at the +10 s sample, as in the `first` and `nosync` runs.
        let start = connected + SAMPLE_INTERVAL;
        let first = PtsReading {
            video: Duration::from_secs(10),
            audio: Duration::from_secs(10),
            read_at: start,
        };
        let end = start + Duration::from_secs(50);
        // Kept up: 49 s of media in 50 s of wall time.
        assert!(pts_reached_end(
            connected,
            Some(&first),
            &media("0:00:59.000000000", "0:00:59.500000000", end)
        ));
        // Video froze 20 s into the 50 s.
        assert!(!pts_reached_end(
            connected,
            Some(&first),
            &media("0:00:30.000000000", "0:00:59.500000000", end)
        ));
        // The PTS went backwards (a reset), or was never read.
        assert!(!pts_reached_end(
            connected,
            Some(&first),
            &media("0:00:01.000000000", "0:00:59.500000000", end)
        ));
        assert!(!pts_reached_end(
            connected,
            None,
            &media("0:00:59.000000000", "0:00:59.000000000", end)
        ));
    }

    #[test]
    fn pts_step_gate_catches_a_mid_window_stall() {
        let connected = Instant::now();
        // Readings at +10 .. +60 s, then the end-of-window read just after +60 s.
        let readings = |pts: [u64; 6]| -> Vec<PtsReading> {
            pts.iter()
                .zip(1..)
                .map(|(&p, i)| PtsReading {
                    video: Duration::from_secs(p),
                    audio: Duration::from_secs(p),
                    read_at: connected + SAMPLE_INTERVAL * i,
                })
                .collect()
        };
        let end = connected + Duration::from_secs(60) + Duration::from_millis(200);
        // Kept up at every step (a non-zero PTS base, as in the `nosync` run).
        let steady = readings([47, 57, 67, 77, 87, 97]);
        let at_end = media("0:01:37.100000000", "0:01:37.100000000", end);
        assert!(pts_kept_pace(&steady, &at_end));
        // Froze from +20 to +40 s, then caught up: first-to-last advanced 50 s in 50 s,
        // so `pts_reached_end` passes it, and the step gate must not.
        let caught_up = readings([10, 20, 21, 22, 50, 60]);
        let caught_up_end = media("0:01:00.100000000", "0:01:00.100000000", end);
        assert!(pts_reached_end(
            connected,
            caught_up.first(),
            &caught_up_end
        ));
        assert!(!pts_kept_pace(&caught_up, &caught_up_end));
        // Only audio froze for one step.
        let mut audio_froze = readings([10, 20, 30, 40, 50, 60]);
        audio_froze[3].audio = Duration::from_secs(31);
        assert!(!pts_kept_pace(
            &audio_froze,
            &media("0:01:00.100000000", "0:01:00.100000000", end)
        ));
        // Froze in the last 10 s step, from +50 to +60 s.
        assert!(!pts_kept_pace(
            &readings([10, 20, 30, 40, 50, 51]),
            &media("0:00:51.000000000", "0:00:51.000000000", end)
        ));
        // A skipped sample: the step spans 20 s and advanced 20 s.
        let mut skipped = readings([10, 20, 30, 40, 50, 60]);
        skipped.remove(2);
        assert!(pts_kept_pace(
            &skipped,
            &media("0:01:00.100000000", "0:01:00.100000000", end)
        ));
        // A PTS that went backwards (a reset) is no advance.
        assert!(!pts_kept_pace(
            &readings([10, 20, 30, 2, 12, 22]),
            &media("0:00:22.100000000", "0:00:22.100000000", end)
        ));
        // Slow reads, within the slack: 6 s of media in a 10 s step.
        assert!(pts_kept_pace(
            &readings([10, 20, 26, 36, 46, 56]),
            &media("0:00:56.100000000", "0:00:56.100000000", end)
        ));
    }

    #[test]
    fn pts_gate_fails_a_receiver_that_starts_late() {
        let connected = Instant::now();
        let end = connected + Duration::from_secs(60);
        let reading = |at: u64, pts: u64| PtsReading {
            video: Duration::from_secs(pts),
            audio: Duration::from_secs(pts),
            read_at: connected + Duration::from_secs(at),
        };
        // First buffers by the +60 s sample: the wall time to cover is about 0, and the
        // gate must not pass on that.
        assert!(!pts_reached_end(
            connected,
            Some(&reading(60, 70)),
            &media("0:01:10.000000000", "0:01:10.000000000", end)
        ));
        // The `late` run: first reading at +40 s, then 20 s of media in 20 s.
        assert!(!pts_reached_end(
            connected,
            Some(&reading(40, 42)),
            &media("0:01:02.000000000", "0:01:02.000000000", end)
        ));
        // The +20 s sample is already too late.
        assert!(!pts_reached_end(
            connected,
            Some(&reading(20, 22)),
            &media("0:01:02.000000000", "0:01:02.000000000", end)
        ));
        // The `nosync` run: first reading at +10 s (a non-zero PTS base), 50 s of media
        // in 50 s; a slow first read still counts as the first sample.
        let mut on_time = reading(10, 47);
        on_time.read_at += Duration::from_millis(300);
        assert!(pts_reached_end(
            connected,
            Some(&on_time),
            &media("0:01:37.000000000", "0:01:37.000000000", end)
        ));
    }
}
