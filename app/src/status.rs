//! The status lines under each input while live (docs/specs/desktop-app.md, section
//! 5): which productions hold the input's Open Live source, their state and the slot,
//! and the state of the uplink.
//!
//! [`Poller`] asks Open Live for its productions and the local Strom for each flow's
//! `srt-stats` every [`POLL`], independently of the session's own, slower tick. What
//! it finds is mapped to the page by pure functions ([`productions_of`], [`link_of`],
//! [`waiting_hint`]), so every case is tested without a server. A failed request is
//! shown as unknown and never stops anything: the feeds do not depend on it.

use crate::messages::{Failure, Message};
use anyhow::Result;
use open_live_ingest::openlive::{OpenLiveClient, Production, ProductionState};
use open_live_ingest::strom::{FlowId, SrtCallerStats, StromClient};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::time::Duration;

use crate::ipc::Event;

/// How often the status lines are refreshed. Under the 5 s #126 AC4 asks for, with
/// room for a request that takes a moment.
pub const POLL: Duration = Duration::from_secs(4);

/// One input the poller looks at: its flow on the local Strom and its source in Open
/// Live, as the session's last stats named them.
#[derive(Debug, Clone, PartialEq)]
pub struct Target {
    pub input: String,
    pub flow_id: FlowId,
    /// None until the source is registered.
    pub source_id: Option<String>,
}

/// A production that holds the input's source.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Holder {
    pub name: String,
    pub state: ProductionState,
    /// The slots (mixer inputs, `video_in_<n>`) the source sits in.
    pub slots: Vec<String>,
}

/// What is known about the productions holding an input's source.
#[derive(Debug, Clone, PartialEq)]
pub enum Productions {
    /// Open Live could not be asked, or did not answer, or the source is not
    /// registered yet.
    Unknown,
    /// The productions that hold the source, in Open Live's order. Empty when none
    /// does.
    Held(Vec<Holder>),
}

impl Productions {
    /// `null` for unknown, else the list (empty when no production holds it).
    fn to_json(&self) -> Value {
        match self {
            Productions::Unknown => Value::Null,
            Productions::Held(holders) => json!(holders),
        }
    }
}

/// The productions out of Open Live's list that hold `source_id`.
pub fn productions_of(list: Option<&[Production]>, source_id: Option<&str>) -> Productions {
    let (Some(list), Some(source_id)) = (list, source_id) else {
        return Productions::Unknown;
    };
    Productions::Held(
        list.iter()
            .filter_map(|p| {
                let slots: Vec<String> = p.slots_of(source_id).map(str::to_string).collect();
                (!slots.is_empty()).then(|| Holder {
                    name: p.name.clone(),
                    state: p.state,
                    slots,
                })
            })
            .collect(),
    )
}

/// What the row says while the uplink waits for Open Live, if the productions tell
/// why: none holds the source, or none of those that do is running. None when that
/// is not known, or when one holding it is running or starting (the wait is then
/// not about the production).
pub fn waiting_hint(productions: &Productions) -> Option<Message> {
    let Productions::Held(holders) = productions else {
        return None;
    };
    if holders.is_empty() {
        return Some(Failure::NoProductionHolds.message());
    }
    let running = holders.iter().any(|h| {
        matches!(
            h.state,
            ProductionState::Active | ProductionState::Activating
        )
    });
    if running {
        return None;
    }
    Some(
        Failure::ProductionsInactive {
            names: holders.iter().map(|h| h.name.clone()).collect(),
        }
        .message(),
    )
}

/// The uplink as the link line shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkState {
    /// No flow, or the flow has no SRT peer.
    NotSending,
    /// A peer, but no packets moved since the last look.
    Connecting,
    /// Packets moved since the last look.
    Connected,
    /// The local Strom did not answer.
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Link {
    pub state: LinkState,
    /// `packets_sent_dropped` over `packets_sent`, both over the last interval, in
    /// percent. Only while connected.
    pub dropped_pct: Option<f64>,
    /// Smoothed round-trip time, while there is a peer.
    pub rtt_ms: Option<f64>,
}

/// The counters a link's next interval is measured from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Counters {
    sent: u64,
    dropped: u64,
}

/// The link line from Strom's answer for one flow and the counters of the previous
/// look; with the counters the next look starts from.
pub fn link_of(
    previous: Option<Counters>,
    sample: &Result<Option<SrtCallerStats>>,
) -> (Link, Option<Counters>) {
    let peer = match sample {
        Err(_) => {
            return (
                Link {
                    state: LinkState::Unknown,
                    dropped_pct: None,
                    rtt_ms: None,
                },
                None,
            )
        }
        Ok(None) => {
            return (
                Link {
                    state: LinkState::NotSending,
                    dropped_pct: None,
                    rtt_ms: None,
                },
                None,
            )
        }
        Ok(Some(peer)) => peer,
    };
    let now = Counters {
        sent: peer.packets_sent.unwrap_or(0),
        dropped: peer.packets_sent_dropped.unwrap_or(0),
    };
    // Packets moving between two looks is what proves delivery; a counter going
    // backwards is a new connection, measured from here.
    let (state, dropped_pct) = match previous {
        Some(before) if now.sent > before.sent => {
            let sent = now.sent - before.sent;
            let dropped = now.dropped.saturating_sub(before.dropped);
            (
                LinkState::Connected,
                Some(dropped as f64 * 100.0 / sent as f64),
            )
        }
        _ => (LinkState::Connecting, None),
    };
    (
        Link {
            state,
            dropped_pct,
            rtt_ms: peer.rtt_ms,
        },
        Some(now),
    )
}

/// One input's status lines.
#[derive(Debug, Clone, PartialEq)]
pub struct InputStatus {
    pub input: String,
    pub productions: Productions,
    pub link: Link,
}

/// The page's `status` event for one look at every input.
pub fn event(statuses: &[InputStatus]) -> Event {
    Event::new(
        "status",
        json!({
            "inputs": statuses.iter().map(|s| json!({
                "input": s.input,
                "productions": s.productions.to_json(),
                "link": s.link,
            })).collect::<Vec<Value>>()
        }),
    )
}

/// Looks at Open Live's productions and the local Strom's flows, keeping the link
/// counters between looks.
pub struct Poller {
    /// None when the settings do not register with Open Live: productions unknown.
    open_live: Option<OpenLiveClient>,
    strom: StromClient,
    counters: HashMap<String, Counters>,
}

impl Poller {
    pub fn new(open_live: Option<OpenLiveClient>, strom: StromClient) -> Self {
        Poller {
            open_live,
            strom,
            counters: HashMap::new(),
        }
    }

    /// One look at every target. Never fails: what could not be read is unknown.
    pub async fn poll(&mut self, targets: &[Target]) -> Vec<InputStatus> {
        let list = match &self.open_live {
            Some(client) if targets.iter().any(|t| t.source_id.is_some()) => {
                match client.list_productions().await {
                    Ok(list) => Some(list),
                    Err(err) => {
                        tracing::debug!(
                            err = %format_args!("{err:#}"),
                            "could not list the productions; shown as unknown"
                        );
                        None
                    }
                }
            }
            _ => None,
        };
        let mut out = Vec::with_capacity(targets.len());
        for target in targets {
            let sample = self.strom.srt_uplink(&target.flow_id).await;
            if let Err(err) = &sample {
                tracing::debug!(input = %target.input, err = %format_args!("{err:#}"), "could not read the uplink's srt-stats");
            }
            let (link, next) = link_of(self.counters.get(&target.input).copied(), &sample);
            match next {
                Some(next) => self.counters.insert(target.input.clone(), next),
                None => self.counters.remove(&target.input),
            };
            out.push(InputStatus {
                input: target.input.clone(),
                productions: productions_of(list.as_deref(), target.source_id.as_deref()),
                link,
            });
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::messages::Action;
    use crate::testing;
    use open_live_ingest::config::AuthMode;
    use std::sync::Arc;

    fn production(id: &str, name: &str, state: &str, slots: &[(&str, &str)]) -> Value {
        json!({
            "_id": id,
            "name": name,
            "status": state,
            "sources": slots.iter().map(|(s, m)| json!({"sourceId": s, "mixerInput": m})).collect::<Vec<_>>(),
        })
    }

    fn list(productions: Value) -> Vec<Production> {
        serde_json::from_value(productions).unwrap()
    }

    fn holder(name: &str, state: ProductionState, slots: &[&str]) -> Holder {
        Holder {
            name: name.to_string(),
            state,
            slots: slots.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn no_production_holding_the_source_is_an_empty_list_and_says_so() {
        let all = list(json!([
            production(
                "prod-1",
                "Morning Show",
                "active",
                &[("src-2", "video_in_0")]
            ),
            production("prod-2", "Empty", "inactive", &[]),
        ]));
        let held = productions_of(Some(&all), Some("src-1"));
        assert_eq!(held, Productions::Held(vec![]));
        assert_eq!(held.to_json(), json!([]));
        let hint = waiting_hint(&held).expect("a hint");
        assert!(hint.message.contains("no production holds this source"));
        assert_eq!(hint.action, Some(Action::OpenStudio));
    }

    #[test]
    fn several_productions_holding_the_source_are_each_listed_with_their_slots() {
        let all = list(json!([
            production(
                "prod-1",
                "Morning Show",
                "active",
                &[
                    ("src-1", "video_in_0"),
                    ("src-2", "video_in_1"),
                    ("src-1", "video_in_3")
                ]
            ),
            production("prod-2", "Other", "active", &[("src-2", "video_in_0")]),
            production(
                "prod-3",
                "Rehearsal",
                "inactive",
                &[("src-1", "video_in_2")]
            ),
        ]));
        let held = productions_of(Some(&all), Some("src-1"));
        assert_eq!(
            held,
            Productions::Held(vec![
                holder(
                    "Morning Show",
                    ProductionState::Active,
                    &["video_in_0", "video_in_3"]
                ),
                holder("Rehearsal", ProductionState::Inactive, &["video_in_2"]),
            ])
        );
        assert_eq!(
            held.to_json(),
            json!([
                {"name": "Morning Show", "state": "active", "slots": ["video_in_0", "video_in_3"]},
                {"name": "Rehearsal", "state": "inactive", "slots": ["video_in_2"]},
            ])
        );
        // One of them runs: the wait is not about the production.
        assert_eq!(waiting_hint(&held), None);
    }

    #[test]
    fn only_inactive_or_ended_productions_are_named_in_the_hint() {
        let held = Productions::Held(vec![
            holder("Morning Show", ProductionState::Inactive, &["video_in_0"]),
            holder("Yesterday", ProductionState::Ended, &["video_in_1"]),
        ]);
        let hint = waiting_hint(&held).expect("a hint");
        assert_eq!(
            hint.message,
            "Waiting for Open Live: productions \"Morning Show\", \"Yesterday\" are inactive. Activate one in Studio."
        );
        let activating = Productions::Held(vec![holder(
            "Morning Show",
            ProductionState::Activating,
            &["video_in_0"],
        )]);
        assert_eq!(waiting_hint(&activating), None);
    }

    #[test]
    fn an_api_error_or_an_unregistered_source_is_unknown_with_no_hint() {
        let all = list(json!([production(
            "prod-1",
            "x",
            "active",
            &[("src-1", "video_in_0")]
        )]));
        for unknown in [
            productions_of(None, Some("src-1")),
            productions_of(Some(&all), None),
        ] {
            assert_eq!(unknown, Productions::Unknown);
            assert_eq!(unknown.to_json(), Value::Null);
            assert_eq!(waiting_hint(&unknown), None);
        }
    }

    fn peer(sent: u64, dropped: u64, rtt: f64) -> Result<Option<SrtCallerStats>> {
        Ok(Some(
            serde_json::from_value(json!({
                "packets_sent": sent, "packets_sent_dropped": dropped, "rtt_ms": rtt
            }))
            .unwrap(),
        ))
    }

    #[test]
    fn the_link_is_connected_once_packets_move_with_the_intervals_dropped_share() {
        let (first, counters) = link_of(None, &peer(1000, 10, 31.5));
        assert_eq!(first.state, LinkState::Connecting);
        assert_eq!(first.dropped_pct, None);
        assert_eq!(first.rtt_ms, Some(31.5));

        let (second, counters) = link_of(counters, &peer(1400, 12, 30.0));
        assert_eq!(second.state, LinkState::Connected);
        assert_eq!(second.dropped_pct, Some(0.5));
        assert_eq!(second.rtt_ms, Some(30.0));

        // Nothing moved: back to connecting.
        let (stalled, counters) = link_of(counters, &peer(1400, 12, 30.0));
        assert_eq!(stalled.state, LinkState::Connecting);
        assert_eq!(stalled.dropped_pct, None);

        // A new connection starts its counters again.
        let (reset, counters) = link_of(counters, &peer(5, 0, 40.0));
        assert_eq!(reset.state, LinkState::Connecting);
        let (after, _) = link_of(counters, &peer(105, 1, 40.0));
        assert_eq!(after.state, LinkState::Connected);
        assert_eq!(after.dropped_pct, Some(1.0));
    }

    #[test]
    fn no_peer_is_not_sending_and_a_strom_error_is_unknown() {
        let before = Some(Counters {
            sent: 10,
            dropped: 0,
        });
        let (none, counters) = link_of(before, &Ok(None));
        assert_eq!(none.state, LinkState::NotSending);
        assert_eq!(counters, None);
        let (error, counters) = link_of(before, &Err(anyhow::anyhow!("GET srt-stats failed")));
        assert_eq!(error.state, LinkState::Unknown);
        assert_eq!(error.rtt_ms, None);
        assert_eq!(counters, None);
        assert_eq!(
            serde_json::to_value(error).unwrap(),
            json!({"state": "unknown", "dropped_pct": null, "rtt_ms": null})
        );
    }

    const FLOW: &str = "4f1c2a8e-0000-5000-8000-000000000001";

    fn target(source_id: Option<&str>) -> Target {
        Target {
            input: "Venue — Camera".to_string(),
            flow_id: FLOW.parse().unwrap(),
            source_id: source_id.map(str::to_string),
        }
    }

    /// A fake that is both Open Live and the local Strom: the productions answer with
    /// `productions` (status and body), and the flow's `srt-stats` count up by 100
    /// packets per request with one dropped.
    fn fake(productions: (u16, Value)) -> String {
        let polls = Arc::new(std::sync::atomic::AtomicU64::new(0));
        testing::serve(Arc::new(move |req: &testing::Request| {
            if req.path == "/api/v1/productions" {
                return productions.clone();
            }
            if req.path == format!("/api/flows/{FLOW}/srt-stats") {
                let n = polls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                return (
                    200,
                    json!({"flow_id": FLOW, "stats": {"connections": {
                        "uplink:srtsink": {"role": "sink", "mode": "caller", "connected": true,
                            "callers": [{"packets_sent": n * 100, "packets_sent_dropped": n, "rtt_ms": 25.0}]}
                    }}}),
                );
            }
            (404, json!({}))
        }))
    }

    fn poller(base: &str) -> Poller {
        Poller::new(
            Some(OpenLiveClient::new(base, AuthMode::Direct, None).unwrap()),
            StromClient::new(base, None).unwrap(),
        )
    }

    #[tokio::test]
    async fn a_poll_reads_the_productions_and_the_links_counters() {
        let base = fake((
            200,
            json!([production(
                "prod-1",
                "Morning Show",
                "inactive",
                &[("src-1", "video_in_1")]
            )]),
        ));
        let mut poller = poller(&base);
        let first = poller.poll(&[target(Some("src-1"))]).await;
        assert_eq!(first.len(), 1);
        assert_eq!(
            first[0].productions,
            Productions::Held(vec![holder(
                "Morning Show",
                ProductionState::Inactive,
                &["video_in_1"]
            )])
        );
        assert_eq!(first[0].link.state, LinkState::Connecting);
        let second = poller.poll(&[target(Some("src-1"))]).await;
        assert_eq!(second[0].link.state, LinkState::Connected);
        assert_eq!(second[0].link.dropped_pct, Some(1.0));
        assert_eq!(second[0].link.rtt_ms, Some(25.0));
    }

    /// Open Live failing is unknown, not a crash, and the link line still shows.
    #[tokio::test]
    async fn a_failing_open_live_shows_unknown_and_the_link_still_shows() {
        let base = fake((503, json!({"error": "Database unavailable"})));
        let mut poller = poller(&base);
        let statuses = poller.poll(&[target(Some("src-1"))]).await;
        assert_eq!(statuses[0].productions, Productions::Unknown);
        assert_eq!(statuses[0].link.state, LinkState::Connecting);
        assert_eq!(
            serde_json::to_value(event(&statuses)).unwrap()["data"]["inputs"][0]["productions"],
            Value::Null
        );
    }

    /// Nothing listening at all: everything unknown, nothing panics.
    #[tokio::test]
    async fn nothing_answering_is_unknown_everywhere() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        let mut poller = poller(&base);
        let statuses = poller.poll(&[target(Some("src-1"))]).await;
        assert_eq!(statuses[0].productions, Productions::Unknown);
        assert_eq!(statuses[0].link.state, LinkState::Unknown);
    }
}
