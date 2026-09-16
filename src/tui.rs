//! `status --watch`: the status report drawn full screen and gathered again every
//! few seconds, for a look over SSH at what the gateway is doing right now.
//!
//! Read-only. Nothing here starts, stops, or registers anything; `up` and `down` do
//! that. The screen shows what `status` prints, plus what only a second look can
//! tell: whether bytes have moved since the last poll, and how the send rate has
//! trended. The terminal is handed back on quit, on error, and on panic.

use crate::config::Config;
use crate::run::{self, InputReport, StatusReport};
use anyhow::{bail, Context, Result};
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Padding, Paragraph, Row, Table};
use ratatui::{DefaultTerminal, Frame};
use std::collections::{BTreeMap, VecDeque};
use std::io::IsTerminal;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

/// How often Strom and Open Live are asked. Fast enough to see a link come and go,
/// slow enough that a watched gateway is no noticeable load on either.
const REFRESH: Duration = Duration::from_secs(2);

/// Rate samples kept per input. At `REFRESH` that is two minutes of trend.
const HISTORY: usize = 60;

/// A report older than this is drawn as stale: a poll is taking longer than it
/// should, usually because Strom or Open Live has stopped answering.
const STALE_AFTER: Duration = Duration::from_secs(6);

/// Narrower than this and the input column would be unreadable, so detail columns
/// are dropped instead.
const MIN_INPUT_WIDTH: u16 = 16;

/// Narrower than this and the header's two columns would run into each other, so
/// they are stacked.
const SIDE_BY_SIDE_HEADER: u16 = 110;

fn boxed(title: &str) -> Block<'static> {
    Block::new()
        .borders(Borders::ALL)
        .padding(Padding::horizontal(1))
        .title(format!(" {title} "))
}

pub async fn watch(cfg: Config) -> Result<()> {
    if !std::io::stdout().is_terminal() {
        bail!("status --watch draws on a terminal; use `status` or `status --json` here");
    }
    let mut terminal = ratatui::try_init().context("taking over the terminal")?;
    let outcome = run_loop(&mut terminal, cfg).await;
    let restored = ratatui::try_restore().context("handing the terminal back");
    outcome.and(restored)
}

async fn run_loop(terminal: &mut DefaultTerminal, cfg: Config) -> Result<()> {
    let mut view = View::new(cfg.gateway.name.clone(), colours_wanted());
    let (report_tx, mut reports) = mpsc::channel(1);
    let (refresh_tx, refresh_rx) = mpsc::channel(1);
    tokio::spawn(poll(cfg, report_tx, refresh_rx));
    let mut keys = key_events();
    loop {
        terminal.draw(|frame| draw(frame, &view, Instant::now()))?;
        tokio::select! {
            report = reports.recv() => match report {
                Some(report) => view.apply(report, Instant::now()),
                None => bail!("the status poller stopped"),
            },
            Some(event) = keys.recv() => match action(&event) {
                Some(Action::Quit) => return Ok(()),
                Some(Action::Refresh) => {
                    // A full channel means a refresh is already queued.
                    let _ = refresh_tx.try_send(());
                }
                None => {}
            },
            // Keeps the "refreshed N s ago" reading honest while a poll is slow.
            _ = tokio::time::sleep(Duration::from_secs(1)) => {}
        }
    }
}

/// Gathers the report on a cadence, or sooner when asked. Runs until the screen
/// stops listening.
async fn poll(
    cfg: Config,
    reports: mpsc::Sender<Result<StatusReport>>,
    mut refresh: mpsc::Receiver<()>,
) {
    loop {
        let report = run::gather_status(&cfg).await;
        if reports.send(report).await.is_err() {
            return;
        }
        tokio::select! {
            _ = tokio::time::sleep(REFRESH) => {}
            _ = refresh.recv() => {}
        }
    }
}

/// Terminal input on its own thread: crossterm's reader blocks, and the screen must
/// keep redrawing while it waits.
fn key_events() -> mpsc::UnboundedReceiver<Event> {
    let (tx, rx) = mpsc::unbounded_channel();
    std::thread::spawn(move || {
        while let Ok(event) = crossterm::event::read() {
            if tx.send(event).is_err() {
                return;
            }
        }
    });
    rx
}

fn colours_wanted() -> bool {
    !std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Quit,
    Refresh,
}

fn action(event: &Event) -> Option<Action> {
    let Event::Key(key) = event else {
        return None;
    };
    if key.kind == KeyEventKind::Release {
        return None;
    }
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => Some(Action::Quit),
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => Some(Action::Quit),
        KeyCode::Char('r') => Some(Action::Refresh),
        _ => None,
    }
}

/// What is on screen: the latest report and, per input, what successive reports
/// reveal that a single one cannot.
struct View {
    gateway: String,
    report: Option<StatusReport>,
    /// Why the last poll produced no report. The previous report stays on screen.
    poll_error: Option<String>,
    refreshed: Option<Instant>,
    trends: BTreeMap<String, Trend>,
    palette: Palette,
}

impl View {
    fn new(gateway: String, colour: bool) -> Self {
        Self {
            gateway,
            report: None,
            poll_error: None,
            refreshed: None,
            trends: BTreeMap::new(),
            palette: Palette { colour },
        }
    }

    fn apply(&mut self, report: Result<StatusReport>, now: Instant) {
        match report {
            Ok(report) => {
                for input in &report.inputs {
                    self.trends
                        .entry(input.name.clone())
                        .or_default()
                        .observe(input);
                }
                self.trends
                    .retain(|name, _| report.inputs.iter().any(|i| &i.name == name));
                self.report = Some(report);
                self.poll_error = None;
                self.refreshed = Some(now);
            }
            Err(err) => self.poll_error = Some(format!("{err:#}")),
        }
    }

    fn trend(&self, name: &str) -> Trend {
        self.trends.get(name).cloned().unwrap_or_default()
    }

    /// Everything that deserves a line under the table.
    fn messages(&self) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(r) = &self.report {
            if let (None, Some(err)) = (&r.uplink, &r.uplink_error) {
                out.push(format!("Uplink not resolved: {err}"));
            }
            if let Some(err) = &r.strom.error {
                out.push(format!("Strom at {} is not reachable ({err})", r.strom.url));
            }
            if let Some(err) = r.open_live.as_ref().and_then(|o| o.error.as_deref()) {
                out.push(format!("Open Live is not reachable ({err})"));
            }
        }
        if let Some(err) = &self.poll_error {
            out.push(format!("Could not gather status: {err}"));
        }
        out
    }
}

/// Where an input's bytes are going, judged the way `up` judges it: bytes moving
/// between two polls is the test, not a `connected` flag, which srtsink keeps
/// reporting for a peer that has gone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Delivery {
    /// No flow, or a flow that is not running.
    #[default]
    None,
    /// The flow runs but nothing is connected to the uplink. Normal until the source
    /// is assigned to a production.
    NoReceiver,
    /// A receiver is connected; the next poll tells whether bytes move.
    Connected,
    /// Bytes moved since the last poll.
    OnAir,
    /// A receiver is connected but the byte counter did not move.
    Stalled,
}

impl Delivery {
    fn label(self) -> &'static str {
        match self {
            Delivery::None => "-",
            Delivery::NoReceiver => "no receiver",
            Delivery::Connected => "connected",
            Delivery::OnAir => "on air",
            Delivery::Stalled => "stalled",
        }
    }
}

/// What successive reports say about one input.
#[derive(Debug, Clone, Default)]
struct Trend {
    last_bytes: Option<u64>,
    delivery: Delivery,
    /// Send rate per poll, newest last. None when there was nothing to measure.
    rates: VecDeque<Option<f64>>,
}

impl Trend {
    fn observe(&mut self, input: &InputReport) {
        let running = input.flow.as_ref().is_some_and(|f| f.running);
        let stats = input.uplink.as_ref().filter(|_| running);
        self.delivery = match stats {
            None => {
                self.last_bytes = None;
                if running {
                    Delivery::NoReceiver
                } else {
                    Delivery::None
                }
            }
            Some(s) => {
                let bytes = s.bytes_sent.unwrap_or(0);
                let delivery = match self.last_bytes {
                    None => Delivery::Connected,
                    Some(before) if bytes != before && bytes > 0 => Delivery::OnAir,
                    Some(_) => Delivery::Stalled,
                };
                self.last_bytes = Some(bytes);
                delivery
            }
        };
        self.rates.push_back(stats.and_then(|s| s.send_rate_mbps));
        while self.rates.len() > HISTORY {
            self.rates.pop_front();
        }
    }
}

/// The newest `width` samples as bars, newest at the right, scaled to the highest
/// of them. A poll with nothing to measure leaves a gap.
fn sparkline(rates: &VecDeque<Option<f64>>, width: usize) -> String {
    const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let recent: Vec<Option<f64>> = rates.iter().rev().take(width).rev().copied().collect();
    let max = recent.iter().flatten().fold(0.0_f64, |m, r| m.max(*r));
    let mut out = " ".repeat(width.saturating_sub(recent.len()));
    for rate in recent {
        out.push(match rate {
            None => ' ',
            Some(_) if max <= 0.0 => BARS[0],
            Some(r) => BARS[((r / max * 7.0).round() as usize).min(7)],
        });
    }
    out
}

/// Colours, or none under `NO_COLOR`. Bold and dim survive either way.
#[derive(Debug, Clone, Copy)]
struct Palette {
    colour: bool,
}

impl Palette {
    fn paint(self, colour: Color) -> Style {
        if self.colour {
            Style::new().fg(colour)
        } else {
            Style::new()
        }
    }
    fn good(self) -> Style {
        self.paint(Color::Green)
    }
    fn warn(self) -> Style {
        self.paint(Color::Yellow)
    }
    fn bad(self) -> Style {
        self.paint(Color::Red)
    }
    fn dim(self) -> Style {
        Style::new().dim()
    }
    fn label(self) -> Style {
        Style::new().bold()
    }
}

fn draw(frame: &mut Frame, view: &View, now: Instant) {
    let messages = view.messages();
    let notes_height = if messages.is_empty() {
        0
    } else {
        messages.len() as u16 + 2
    };
    let stacked = frame.area().width < SIDE_BY_SIDE_HEADER;
    let [header, body, notes, footer] = Layout::vertical([
        Constraint::Length(if stacked { 6 } else { 4 }),
        Constraint::Min(3),
        Constraint::Length(notes_height),
        Constraint::Length(1),
    ])
    .areas(frame.area());
    draw_header(frame, header, view, stacked);
    draw_inputs(frame, body, view);
    if !messages.is_empty() {
        draw_messages(frame, notes, &messages, view.palette);
    }
    draw_footer(frame, footer, view, now);
}

fn draw_header(frame: &mut Frame, area: Rect, view: &View, stacked: bool) {
    let p = view.palette;
    let block = boxed(&format!("Open Live Ingest: {}", view.gateway));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let [left, right] = if stacked {
        Layout::vertical([Constraint::Length(2), Constraint::Length(2)]).areas(inner)
    } else {
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(inner)
    };

    let (process, uplink) = match &view.report {
        Some(r) => {
            let process = match r.process.pid {
                Some(pid) => Span::styled(format!("running (pid {pid})"), p.good()),
                None => Span::styled("not running", p.warn()),
            };
            let uplink = match &r.uplink {
                Some(u) => Span::raw(format!(
                    "{}, SRT ports {} from {}",
                    u.host, u.ports, u.port_source
                )),
                None => Span::styled("not resolved", p.bad()),
            };
            (process, uplink)
        }
        None => (
            Span::styled("asking", p.dim()),
            Span::styled("asking", p.dim()),
        ),
    };
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(vec![Span::styled("Gateway  ", p.label()), process]),
            Line::from(vec![Span::styled("Uplink   ", p.label()), uplink]),
        ]),
        left,
    );

    let endpoint = |label: &str, e: Option<&run::EndpointReport>, absent: &str| -> Line<'static> {
        let state = match e {
            Some(e) if e.reachable => Span::styled("reachable  ", p.good()),
            Some(_) => Span::styled("unreachable", p.bad()),
            None => Span::styled(absent.to_string(), p.dim()),
        };
        let url = e.map(|e| format!("  {}", e.url)).unwrap_or_default();
        Line::from(vec![
            Span::styled(format!("{label:<10} "), p.label()),
            state,
            Span::styled(url, p.dim()),
        ])
    };
    let (strom, open_live) = match &view.report {
        Some(r) => (
            endpoint("Strom", Some(&r.strom), ""),
            endpoint("Open Live", r.open_live.as_ref(), "registration off"),
        ),
        None => (
            endpoint("Strom", None, "asking"),
            endpoint("Open Live", None, "asking"),
        ),
    };
    frame.render_widget(Paragraph::new(vec![strom, open_live]), right);
}

/// One table column. `keep` orders what goes when the terminal is narrow: lowest
/// first, and `ESSENTIAL` never.
struct Column {
    title: &'static str,
    width: u16,
    keep: u8,
}

const ESSENTIAL: u8 = u8::MAX;

/// The input column is first and takes the width the others leave.
const COLUMNS: [Column; 12] = [
    Column {
        title: "INPUT",
        width: MIN_INPUT_WIDTH,
        keep: ESSENTIAL,
    },
    Column {
        title: "FLOW",
        width: 7,
        keep: ESSENTIAL,
    },
    Column {
        title: "UPLINK",
        width: 11,
        keep: ESSENTIAL,
    },
    Column {
        title: "RATE",
        width: 10,
        keep: ESSENTIAL,
    },
    Column {
        title: "RTT",
        width: 7,
        keep: 6,
    },
    Column {
        title: "LAT",
        width: 7,
        keep: 2,
    },
    Column {
        title: "LOST",
        width: 6,
        keep: 5,
    },
    Column {
        title: "RETX",
        width: 6,
        keep: 3,
    },
    Column {
        title: "DROP",
        width: 6,
        keep: 2,
    },
    Column {
        title: "BUF",
        width: 7,
        keep: 1,
    },
    Column {
        title: "OPEN LIVE",
        width: 14,
        keep: ESSENTIAL,
    },
    Column {
        title: "TREND",
        width: 20,
        keep: 4,
    },
];

const COLUMN_SPACING: u16 = 2;

/// Which columns fit in `width`, by index into `COLUMNS`, dropping the least
/// important until the rest do.
fn visible_columns(width: u16) -> Vec<usize> {
    let mut shown: Vec<usize> = (0..COLUMNS.len()).collect();
    loop {
        let needed: u16 = shown.iter().map(|&i| COLUMNS[i].width).sum::<u16>()
            + COLUMN_SPACING * shown.len().saturating_sub(1) as u16;
        if needed <= width {
            return shown;
        }
        let Some(pos) = shown
            .iter()
            .enumerate()
            .filter(|(_, &i)| COLUMNS[i].keep != ESSENTIAL)
            .min_by_key(|(_, &i)| COLUMNS[i].keep)
            .map(|(pos, _)| pos)
        else {
            return shown;
        };
        shown.remove(pos);
    }
}

fn draw_inputs(frame: &mut Frame, area: Rect, view: &View) {
    let p = view.palette;
    let block = boxed("Inputs");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let Some(report) = &view.report else {
        frame.render_widget(
            Paragraph::new(Span::styled("Asking Strom and Open Live...", p.dim())),
            inner,
        );
        return;
    };
    if report.inputs.is_empty() {
        frame.render_widget(
            Paragraph::new("Nothing of ours in Strom or Open Live."),
            inner,
        );
        return;
    }

    let shown = visible_columns(inner.width);
    let widths: Vec<Constraint> = shown
        .iter()
        .map(|&i| match COLUMNS[i].title {
            "INPUT" => Constraint::Fill(1),
            _ => Constraint::Length(COLUMNS[i].width),
        })
        .collect();
    let header = Row::new(
        shown
            .iter()
            .map(|&i| Cell::from(Span::styled(COLUMNS[i].title, p.label()))),
    );
    let rows = report.inputs.iter().map(|input| {
        let trend = view.trend(&input.name);
        Row::new(
            shown
                .iter()
                .map(|&i| cell(COLUMNS[i].title, input, &trend, p)),
        )
    });
    frame.render_widget(
        Table::new(rows, widths)
            .header(header)
            .column_spacing(COLUMN_SPACING),
        inner,
    );
}

fn cell<'a>(column: &str, input: &'a InputReport, trend: &Trend, p: Palette) -> Cell<'a> {
    let stats = input.uplink.as_ref();
    let count = |n: Option<u64>| -> Cell<'a> {
        match n {
            Some(0) => Cell::from(Span::styled("0", p.dim())),
            Some(n) => Cell::from(n.to_string()),
            None => Cell::from(Span::styled("-", p.dim())),
        }
    };
    let millis = |n: Option<u32>| -> Cell<'a> {
        match n {
            Some(n) => Cell::from(format!("{n} ms")),
            None => Cell::from(Span::styled("-", p.dim())),
        }
    };
    match column {
        "INPUT" => Cell::from(input.name.as_str()),
        "FLOW" => match &input.flow {
            Some(f) if f.running => Cell::from(Span::styled("running", p.good())),
            Some(_) => Cell::from(Span::styled("stopped", p.warn())),
            None => Cell::from(Span::styled("-", p.dim())),
        },
        "UPLINK" => {
            let style = match trend.delivery {
                Delivery::OnAir => p.good(),
                Delivery::Stalled => p.warn(),
                Delivery::None => p.dim(),
                Delivery::NoReceiver | Delivery::Connected => Style::new(),
            };
            Cell::from(Span::styled(trend.delivery.label(), style))
        }
        "RATE" => match stats.and_then(|s| s.send_rate_mbps) {
            Some(rate) => Cell::from(format!("{rate:.2} Mbps")),
            None => Cell::from(Span::styled("-", p.dim())),
        },
        "RTT" => match stats.and_then(|s| s.rtt_ms) {
            Some(rtt) => Cell::from(format!("{rtt:.0} ms")),
            None => Cell::from(Span::styled("-", p.dim())),
        },
        "LAT" => millis(stats.and_then(|s| s.negotiated_latency_ms)),
        "LOST" => count(stats.and_then(|s| s.packets_sent_lost)),
        "RETX" => count(stats.and_then(|s| s.packets_retransmitted)),
        "DROP" => count(stats.and_then(|s| s.packets_sent_dropped)),
        "BUF" => millis(stats.and_then(|s| s.snd_buf_level_ms)),
        "OPEN LIVE" => match &input.source {
            Some(s) => {
                let style = match s.status.as_str() {
                    "active" => p.good(),
                    "inactive" => p.warn(),
                    _ => Style::new(),
                };
                Cell::from(Span::styled(s.status.as_str(), style))
            }
            None if input.flow.is_some() => Cell::from(Span::styled("not registered", p.bad())),
            None => Cell::from(Span::styled("-", p.dim())),
        },
        "TREND" => Cell::from(sparkline(&trend.rates, 20)),
        _ => Cell::from(""),
    }
}

fn draw_messages(frame: &mut Frame, area: Rect, messages: &[String], p: Palette) {
    let lines: Vec<Line> = messages
        .iter()
        .map(|m| Line::from(Span::styled(m.as_str(), p.warn())))
        .collect();
    frame.render_widget(Paragraph::new(lines).block(boxed("Attention")), area);
}

fn draw_footer(frame: &mut Frame, area: Rect, view: &View, now: Instant) {
    let p = view.palette;
    let freshness = match view.refreshed {
        Some(at) => {
            let age = now.saturating_duration_since(at);
            let text = format!(
                "refreshed {}s ago, every {}s",
                age.as_secs(),
                REFRESH.as_secs()
            );
            if age > STALE_AFTER {
                Span::styled(format!("{text}, waiting on a slow answer"), p.warn())
            } else {
                Span::styled(text, p.dim())
            }
        }
        None => Span::styled("waiting for the first answer", p.dim()),
    };
    let line = Line::from(vec![
        Span::styled("q", p.label()),
        Span::raw(" quit   "),
        Span::styled("r", p.label()),
        Span::raw(" refresh now   "),
        freshness,
    ]);
    frame.render_widget(
        Paragraph::new(line).block(Block::new().padding(Padding::horizontal(1))),
        area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run::{EndpointReport, FlowReport, ProcessReport, SourceReport, UplinkStatsReport};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn input(name: &str, running: bool, bytes: Option<u64>, source: Option<&str>) -> InputReport {
        InputReport {
            name: name.to_string(),
            flow: Some(FlowReport {
                id: "flow".to_string(),
                running,
            }),
            uplink: bytes.map(|b| UplinkStatsReport {
                bytes_sent: Some(b),
                send_rate_mbps: Some(b as f64 / 1_000_000.0),
                rtt_ms: Some(31.4),
                packets_sent_lost: Some(0),
                packets_retransmitted: Some(3),
                ..UplinkStatsReport::default()
            }),
            source: source.map(|s| SourceReport {
                id: "src".to_string(),
                status: s.to_string(),
            }),
        }
    }

    fn report(inputs: Vec<InputReport>) -> StatusReport {
        StatusReport {
            process: ProcessReport {
                running: true,
                pid: Some(4242),
            },
            uplink: Some(run::UplinkReport {
                host: "192.0.2.10".to_string(),
                ports: "47110-47129".to_string(),
                port_source: "Open Live".to_string(),
            }),
            uplink_error: None,
            strom: EndpointReport {
                url: "http://127.0.0.1:3000".to_string(),
                reachable: true,
                error: None,
            },
            open_live: Some(EndpointReport {
                url: "https://open-live.example.com".to_string(),
                reachable: true,
                error: None,
            }),
            inputs,
        }
    }

    fn screen(view: &View, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| draw(frame, view, Instant::now()))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let mut out = String::new();
        for y in 0..height {
            for x in 0..width {
                out.push_str(buffer[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    /// The same test `up` applies: one sample proves nothing, moving bytes prove
    /// delivery, a still counter with a peer attached is a stall.
    #[test]
    fn delivery_is_judged_by_bytes_moving_between_polls() {
        let mut view = View::new("Venue".to_string(), false);
        let now = Instant::now();
        view.apply(
            Ok(report(vec![input("Cam 1", true, Some(1000), None)])),
            now,
        );
        assert_eq!(view.trend("Cam 1").delivery, Delivery::Connected);
        view.apply(
            Ok(report(vec![input("Cam 1", true, Some(2000), None)])),
            now,
        );
        assert_eq!(view.trend("Cam 1").delivery, Delivery::OnAir);
        view.apply(
            Ok(report(vec![input("Cam 1", true, Some(2000), None)])),
            now,
        );
        assert_eq!(view.trend("Cam 1").delivery, Delivery::Stalled);
        view.apply(Ok(report(vec![input("Cam 1", true, None, None)])), now);
        assert_eq!(view.trend("Cam 1").delivery, Delivery::NoReceiver);
        view.apply(
            Ok(report(vec![input("Cam 1", false, Some(5000), None)])),
            now,
        );
        assert_eq!(view.trend("Cam 1").delivery, Delivery::None);
    }

    /// A reconnect restarts srtsink's counter, so the first sample after a gap must
    /// not be compared with the one before it.
    #[test]
    fn a_receiver_that_comes_back_starts_over() {
        let mut view = View::new("Venue".to_string(), false);
        let now = Instant::now();
        view.apply(
            Ok(report(vec![input("Cam 1", true, Some(9000), None)])),
            now,
        );
        view.apply(Ok(report(vec![input("Cam 1", true, None, None)])), now);
        view.apply(Ok(report(vec![input("Cam 1", true, Some(100), None)])), now);
        assert_eq!(view.trend("Cam 1").delivery, Delivery::Connected);
    }

    #[test]
    fn a_failed_poll_keeps_the_last_report_and_says_why() {
        let mut view = View::new("Venue".to_string(), false);
        let now = Instant::now();
        view.apply(
            Ok(report(vec![input("Cam 1", true, Some(1), Some("active"))])),
            now,
        );
        view.apply(Err(anyhow::anyhow!("open_live.url is unset")), now);
        assert!(view.report.is_some());
        assert_eq!(
            view.messages(),
            vec!["Could not gather status: open_live.url is unset".to_string()]
        );
        view.apply(Ok(report(vec![])), now);
        assert!(view.messages().is_empty());
        assert!(
            view.trends.is_empty(),
            "trends of vanished inputs are dropped"
        );
    }

    #[test]
    fn the_sparkline_puts_the_newest_sample_at_the_right_and_leaves_gaps() {
        let rates: VecDeque<Option<f64>> = [Some(1.0), None, Some(4.0), Some(8.0)].into();
        assert_eq!(sparkline(&rates, 6), "  ▂ ▅█");
        assert_eq!(sparkline(&rates, 2), "▅█");
        let flat: VecDeque<Option<f64>> = [Some(0.0), Some(0.0)].into();
        assert_eq!(sparkline(&flat, 2), "▁▁");
    }

    #[test]
    fn a_narrow_terminal_drops_detail_columns_and_never_the_essentials() {
        let titles = |width: u16| -> Vec<&str> {
            visible_columns(width)
                .into_iter()
                .map(|i| COLUMNS[i].title)
                .collect()
        };
        assert_eq!(titles(200).len(), COLUMNS.len());
        let narrow = titles(78);
        for essential in ["INPUT", "FLOW", "UPLINK", "RATE", "OPEN LIVE"] {
            assert!(
                narrow.contains(&essential),
                "{essential} missing at 78 columns"
            );
        }
        assert!(!narrow.contains(&"BUF"));
        assert!(narrow.len() < COLUMNS.len());
        // Too narrow for even the essentials: they are still all there, clipped.
        assert_eq!(titles(10).len(), 5);
    }

    #[test]
    fn the_screen_shows_every_input_with_its_flow_uplink_and_source() {
        let mut view = View::new("Venue".to_string(), false);
        let now = Instant::now();
        let inputs = || {
            vec![
                input("Venue / Cam 1", true, Some(1_000_000), Some("active")),
                input("Venue / Cam 2", true, None, Some("inactive")),
                input("Venue / Cam 3", false, None, None),
            ]
        };
        view.apply(Ok(report(inputs())), now);
        let mut second = inputs();
        let cam1_stats = second[0].uplink.as_mut().unwrap();
        cam1_stats.bytes_sent = Some(2_000_000);
        cam1_stats.send_rate_mbps = Some(2.0);
        view.apply(Ok(report(second)), now);

        let s = screen(&view, 140, 24);
        assert!(s.contains("Open Live Ingest: Venue"), "{s}");
        assert!(s.contains("running (pid 4242)"), "{s}");
        assert!(
            s.contains("192.0.2.10, SRT ports 47110-47129 from Open Live"),
            "{s}"
        );
        assert!(s.contains("Strom      reachable"), "{s}");
        let cam1 = s.lines().find(|l| l.contains("Cam 1")).expect("Cam 1 row");
        assert!(
            cam1.contains("running") && cam1.contains("on air"),
            "{cam1}"
        );
        assert!(
            cam1.contains("2.00 Mbps") && cam1.contains("31 ms"),
            "{cam1}"
        );
        assert!(cam1.contains("active"), "{cam1}");
        let cam2 = s.lines().find(|l| l.contains("Cam 2")).expect("Cam 2 row");
        assert!(
            cam2.contains("no receiver") && cam2.contains("inactive"),
            "{cam2}"
        );
        let cam3 = s.lines().find(|l| l.contains("Cam 3")).expect("Cam 3 row");
        assert!(
            cam3.contains("stopped") && cam3.contains("not registered"),
            "{cam3}"
        );
        assert!(
            !s.contains("Attention"),
            "nothing is wrong, so no attention box: {s}"
        );
        assert!(s.contains("q quit"), "{s}");
    }

    #[test]
    fn trouble_is_listed_under_the_table() {
        let mut view = View::new("Venue".to_string(), false);
        let mut r = report(vec![]);
        r.strom = EndpointReport {
            url: "http://127.0.0.1:3000".to_string(),
            reachable: false,
            error: Some("connection refused".to_string()),
        };
        r.uplink = None;
        r.uplink_error = Some("no cloud Strom host configured".to_string());
        r.open_live = None;
        view.apply(Ok(r), Instant::now());

        let s = screen(&view, 120, 24);
        assert!(s.contains("Strom      unreachable"), "{s}");
        assert!(s.contains("Uplink   not resolved"), "{s}");
        assert!(s.contains("registration off"), "{s}");
        assert!(s.contains("Attention"), "{s}");
        assert!(
            s.contains("Strom at http://127.0.0.1:3000 is not reachable (connection refused)"),
            "{s}"
        );
        assert!(
            s.contains("Uplink not resolved: no cloud Strom host configured"),
            "{s}"
        );
        assert!(s.contains("Nothing of ours in Strom or Open Live."), "{s}");
    }

    /// At 80 columns the two header halves would overwrite each other; each line
    /// must survive whole.
    #[test]
    fn a_narrow_terminal_stacks_the_header() {
        let mut view = View::new("Venue".to_string(), false);
        view.apply(Ok(report(vec![])), Instant::now());
        let s = screen(&view, 80, 20);
        assert!(
            s.contains("192.0.2.10, SRT ports 47110-47129 from Open Live"),
            "{s}"
        );
        assert!(
            s.contains("Open Live  reachable    https://open-live.example.com"),
            "{s}"
        );
        let wide = screen(&view, 140, 20);
        assert!(wide.lines().count() > 0 && s.lines().count() > 0);
        assert!(
            wide.lines().nth(3).unwrap().starts_with('└'),
            "wide header is 4 rows: {wide}"
        );
        assert!(
            s.lines().nth(5).unwrap().starts_with('└'),
            "narrow header is 6 rows: {s}"
        );
    }

    #[test]
    fn before_the_first_answer_the_screen_says_it_is_asking() {
        let view = View::new("Venue".to_string(), false);
        let s = screen(&view, 100, 20);
        assert!(s.contains("Asking Strom and Open Live..."), "{s}");
        assert!(s.contains("waiting for the first answer"), "{s}");
    }

    #[test]
    fn q_escape_and_ctrl_c_quit_and_r_refreshes() {
        use crossterm::event::KeyEvent;
        let key =
            |code: KeyCode, modifiers: KeyModifiers| Event::Key(KeyEvent::new(code, modifiers));
        assert_eq!(
            action(&key(KeyCode::Char('q'), KeyModifiers::NONE)),
            Some(Action::Quit)
        );
        assert_eq!(
            action(&key(KeyCode::Esc, KeyModifiers::NONE)),
            Some(Action::Quit)
        );
        assert_eq!(
            action(&key(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Some(Action::Quit)
        );
        assert_eq!(
            action(&key(KeyCode::Char('r'), KeyModifiers::NONE)),
            Some(Action::Refresh)
        );
        assert_eq!(action(&key(KeyCode::Char('c'), KeyModifiers::NONE)), None);
        assert_eq!(action(&Event::Resize(80, 24)), None);
    }
}
