//! Reproduction for the late-connect uplink (#111): the uplink flow the gateway
//! builds for `up --test`, started on a real Strom against an SRT listener that comes
//! up only after the flow has been running for a while, as it does when the app goes
//! live before a production is active.
//!
//! The flow is `flow::build`'s own, with the settings of the report: test pattern,
//! 1280x720 at 30/1, H.264 software, 6000 kbit/s CBR, caller, latency 200 ms. The
//! receiver is `srt-live-transmit` writing the stream to a file, and `ffprobe` counts
//! the video and audio frames in it afterwards.
//!
//! It reads the uplink's `/api/flows/<id>/srt-stats` the moment the peer appears, 10 s
//! later, and at the end of the window, and fails (exit status 1) when
//! `packets_sent_dropped` grew by 1 % or more of `packets_sent` over the window, or
//! when the receiver got no video or no audio frames. `snd_buf_level_ms` is printed
//! with every sample: a send buffer that holds more than the SRT latency is the
//! backlog the issue suspects.
//!
//! Needs a running Strom (the pinned release, with GStreamer's `srt`, `x264` and
//! `avenc_aac`), `srt-live-transmit` and `ffprobe` on the PATH. Nothing else: no
//! Open Live, no device.
//!
//! ```text
//! # the late connect: the listener starts 35 s after the flow
//! cargo run --example late_connect -- --listener-delay 35
//! # the control: the listener is up before the flow starts
//! cargo run --example late_connect -- --listener-delay 0
//! ```
//!
//! Options: `--strom <url>` (default `http://127.0.0.1:8080`, or `OLI_STROM_URL`),
//! `--port <n>` (default 47199), `--receiver-port <n>` (default `--port`; set it
//! to put a relay or a link emulator between the two), `--listener-delay <s>` (default 35),
//! `--window <s>` (default 60), `--out <file>` (default a file in the temp dir),
//! `--keep` to leave the flow in Strom afterwards. `OLI_STROM_API_KEY` is sent when
//! set.

use anyhow::{bail, Context, Result};
use open_live_ingest::config::{Uplink, Video};
use open_live_ingest::flow::{self, Input, Source, TEST_INPUT_ID};
use open_live_ingest::strom::{StromClient, PINNED_VERSION};
use std::path::PathBuf;
use std::process::{Child, Command, ExitCode, Stdio};
use std::time::{Duration, Instant};
use strom_types::api::SrtCallerStats;

const GATEWAY_ID: &str = "repro-late-connect";
const LATENCY_MS: u32 = 200;
/// The share of `packets_sent` that `packets_sent_dropped` may grow by.
const MAX_DROP_RATIO: f64 = 0.01;
/// How long to wait for the uplink to report a peer once the listener is up.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

struct Options {
    strom: String,
    port: u16,
    receiver_port: Option<u16>,
    listener_delay: Duration,
    window: Duration,
    out: PathBuf,
    keep: bool,
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
    };
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
            other => bail!("unknown argument {other}; see the header of examples/late_connect.rs"),
        }
    }
    if o.window < Duration::from_secs(10) {
        bail!("--window must be at least 10 s");
    }
    Ok(o)
}

#[tokio::main]
async fn main() -> Result<ExitCode> {
    let o = options()?;
    let strom = StromClient::new(&o.strom, std::env::var("OLI_STROM_API_KEY").ok().as_deref())?;
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

    strom
        .delete_flow(&id)
        .await
        .context("removing an earlier run's flow")?;
    let mut receiver = None;
    let outcome = async {
        if o.listener_delay.is_zero() {
            receiver = Some(spawn_receiver(o.receiver_port.unwrap_or(o.port), &o.out)?);
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
            receiver = Some(spawn_receiver(o.receiver_port.unwrap_or(o.port), &o.out)?);
            println!(
                "listener up {} s after the flow",
                started.elapsed().as_secs()
            );
        }

        let first = wait_for_peer(&strom, &id).await?;
        let connected = Instant::now();
        sample("connected", &first);
        tokio::time::sleep(Duration::from_secs(10)).await;
        let ten = peer(&strom, &id).await?;
        sample("+10 s", &ten);
        tokio::time::sleep(o.window.saturating_sub(connected.elapsed())).await;
        let last = peer(&strom, &id).await?;
        sample(&format!("+{} s", connected.elapsed().as_secs()), &last);

        Ok((growth(&first, &ten), growth(&first, &last)))
    }
    .await;

    if let Some(mut child) = receiver {
        let _ = child.kill();
        let _ = child.wait();
    }
    if !o.keep {
        let _ = strom.stop_flow(&id).await;
        let _ = strom.delete_flow(&id).await;
    }
    let ((sent10, dropped10), (sent, dropped)) = outcome?;

    println!(
        "first 10 s: {dropped10} of {sent10} sent dropped ({})",
        percent(dropped10, sent10)
    );
    println!(
        "window:     {dropped} of {sent} sent dropped ({})",
        percent(dropped, sent)
    );
    let (video_frames, audio_frames) = count_frames(&o.out)?;
    println!(
        "receiver:   {video_frames} video and {audio_frames} audio frames in {}",
        o.out.display()
    );

    let drops_ok = sent > 0 && (dropped as f64) < MAX_DROP_RATIO * sent as f64;
    let media_ok = video_frames > 0 && audio_frames > 0;
    if drops_ok && media_ok {
        println!("PASS: under 1 % dropped, and the receiver decodes video and audio");
        Ok(ExitCode::SUCCESS)
    } else {
        if !drops_ok {
            println!("FAIL: packets_sent_dropped grew by 1 % or more of packets_sent");
        }
        if !media_ok {
            println!("FAIL: the receiver did not get both video and audio frames");
        }
        Ok(ExitCode::FAILURE)
    }
}

/// `srt-live-transmit` listening on `port`, writing what it receives to `out`.
fn spawn_receiver(port: u16, out: &PathBuf) -> Result<Child> {
    let file = std::fs::File::create(out).with_context(|| format!("creating {}", out.display()))?;
    Command::new("srt-live-transmit")
        .arg(format!("srt://:{port}?mode=listener&latency={LATENCY_MS}"))
        .arg("file://con")
        .stdin(Stdio::null())
        .stdout(file)
        .spawn()
        .context("starting srt-live-transmit (is it on the PATH?)")
}

async fn wait_for_peer(strom: &StromClient, id: &strom_types::FlowId) -> Result<SrtCallerStats> {
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
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

async fn peer(strom: &StromClient, id: &strom_types::FlowId) -> Result<SrtCallerStats> {
    strom
        .srt_uplink(id)
        .await?
        .context("the uplink lost its peer during the window")
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

/// Packets sent and packets dropped between two samples.
fn growth(from: &SrtCallerStats, to: &SrtCallerStats) -> (u64, u64) {
    let delta = |a: Option<u64>, b: Option<u64>| b.unwrap_or(0).saturating_sub(a.unwrap_or(0));
    (
        delta(from.packets_sent, to.packets_sent),
        delta(from.packets_sent_dropped, to.packets_sent_dropped),
    )
}

fn percent(part: u64, whole: u64) -> String {
    if whole == 0 {
        return "nothing sent".to_string();
    }
    format!("{:.2} %", 100.0 * part as f64 / whole as f64)
}

/// Decoded video and audio frames in the received file, by `ffprobe -count_frames`.
fn count_frames(path: &PathBuf) -> Result<(u64, u64)> {
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
