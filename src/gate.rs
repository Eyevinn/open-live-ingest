//! When an armed session sends: the uplink of an input runs only while a production
//! holding its Open Live source is live (#126, OL-63c).
//!
//! The trigger is the production state `active` (#128): a production with no
//! sender reaches it on its own within its activation window, so waiting for it
//! cannot deadlock the activation, and a connect into an already active production
//! is clean. `activating` does not start the uplink, nor does it stop one that runs
//! for another production.
//!
//! One thing an active production cannot do is take a slot added while it is live:
//! Open Live gives the slot no receiver until the production is deactivated and
//! reactivated (#128). [`Watch`] notices that from two looks in a row (the
//! production active both times, the source in it only the second time) so the
//! operator can be told what the director has to do. The uplink still runs for such
//! a slot; the connect simply waits for the receiver.
//!
//! Everything here is pure, over the production lists the session reads.

use crate::openlive::{Production, ProductionState};
use std::collections::{BTreeMap, BTreeSet};

/// What the productions say about one source.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Verdict {
    /// The names of the productions holding the source that are live, in Open Live's
    /// order. Non-empty means send.
    pub live_in: Vec<String>,
    /// Whether a production holding the source is activating, so the next look should
    /// come sooner.
    pub activating: bool,
    /// The names of the live productions the source was put in while they were live:
    /// no receiver for it until they are deactivated and reactivated.
    pub added_while_live: Vec<String>,
}

impl Verdict {
    /// Whether the uplink should run.
    pub fn send(&self) -> bool {
        !self.live_in.is_empty()
    }
}

/// The productions as last seen, to tell a slot added while live from one that was
/// there at activation.
#[derive(Debug, Default)]
pub struct Watch {
    /// Per production id: its state and the sources it held at the last look.
    seen: BTreeMap<String, (ProductionState, BTreeSet<String>)>,
    /// (production id, source id) pairs added while the production was active, for as
    /// long as it stays active and holds the source.
    added_while_live: BTreeSet<(String, String)>,
    /// Whether anything has been seen yet: at the first look a slot in an active
    /// production may well have been there at activation, so it is not suspected.
    looked: bool,
}

impl Watch {
    pub fn new() -> Self {
        Self::default()
    }

    /// Takes in one look at every production. Call it once per list, before asking
    /// [`Watch::verdict`] about each source.
    pub fn observe(&mut self, list: &[Production]) {
        let mut now = BTreeMap::new();
        let mut still = BTreeSet::new();
        for p in list {
            let sources: BTreeSet<String> = p.sources.iter().map(|s| s.source_id.clone()).collect();
            if p.state == ProductionState::Active {
                let before = self.seen.get(&p.id);
                for source in &sources {
                    let pair = (p.id.clone(), source.clone());
                    let added_live = self.looked
                        && before.is_some_and(|(state, held)| {
                            *state == ProductionState::Active && !held.contains(source)
                        });
                    if added_live || self.added_while_live.contains(&pair) {
                        still.insert(pair);
                    }
                }
            }
            now.insert(p.id.clone(), (p.state, sources));
        }
        self.seen = now;
        self.added_while_live = still;
        self.looked = true;
    }

    /// What the last list observed says about `source_id`.
    pub fn verdict(&self, list: &[Production], source_id: &str) -> Verdict {
        let mut verdict = Verdict::default();
        for p in list {
            if p.slots_of(source_id).next().is_none() {
                continue;
            }
            match p.state {
                ProductionState::Active => {
                    verdict.live_in.push(p.name.clone());
                    if self
                        .added_while_live
                        .contains(&(p.id.clone(), source_id.to_string()))
                    {
                        verdict.added_while_live.push(p.name.clone());
                    }
                }
                ProductionState::Activating => verdict.activating = true,
                ProductionState::Inactive | ProductionState::Ended | ProductionState::Unknown => {}
            }
        }
        verdict
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn production(id: &str, state: &str, sources: &[&str]) -> Production {
        serde_json::from_value(json!({
            "_id": id,
            "name": format!("Show {id}"),
            "status": state,
            "sources": sources.iter().enumerate()
                .map(|(n, s)| json!({"sourceId": s, "mixerInput": format!("video_in_{n}")}))
                .collect::<Vec<_>>(),
        }))
        .unwrap()
    }

    fn look(watch: &mut Watch, list: &[Production], source: &str) -> Verdict {
        watch.observe(list);
        watch.verdict(list, source)
    }

    #[test]
    fn only_an_active_production_holding_the_source_sends() {
        let mut watch = Watch::new();
        for (state, send, activating) in [
            ("inactive", false, false),
            ("activating", false, true),
            ("active", true, false),
            ("ended", false, false),
            ("something-new", false, false),
        ] {
            let list = [production("p1", state, &["src-1"])];
            let verdict = look(&mut watch, &list, "src-1");
            assert_eq!(verdict.send(), send, "{state}");
            assert_eq!(verdict.activating, activating, "{state}");
        }
        // Active, but without this source.
        let list = [production("p1", "active", &["src-2"])];
        assert!(!look(&mut watch, &list, "src-1").send());
        assert_eq!(look(&mut watch, &[], "src-1"), Verdict::default());
    }

    /// Several productions holding the source: it sends while any is active, and the
    /// verdict does not change as long as one still is.
    #[test]
    fn several_productions_holding_the_source_send_while_any_is_active() {
        let mut watch = Watch::new();
        let both = [
            production("p1", "active", &["src-1"]),
            production("p2", "active", &["src-1", "src-2"]),
        ];
        let verdict = look(&mut watch, &both, "src-1");
        assert_eq!(verdict.live_in, ["Show p1", "Show p2"]);
        let one = [
            production("p1", "inactive", &["src-1"]),
            production("p2", "active", &["src-1", "src-2"]),
        ];
        let verdict = look(&mut watch, &one, "src-1");
        assert!(verdict.send());
        assert_eq!(verdict.live_in, ["Show p2"]);
        let none = [
            production("p1", "inactive", &["src-1"]),
            production("p2", "activating", &["src-1"]),
        ];
        let verdict = look(&mut watch, &none, "src-1");
        assert!(!verdict.send());
        assert!(verdict.activating);
    }

    /// #128: a source put in a production that is already live is told, for as long
    /// as that production stays live; a source that was there at activation, or at
    /// the first look, is not.
    #[test]
    fn a_slot_added_while_the_production_is_live_is_told_until_it_is_reactivated() {
        let mut watch = Watch::new();
        // First look: already active with the source. Nothing suspected.
        let at_start = [production("p1", "active", &["src-1"])];
        assert!(look(&mut watch, &at_start, "src-1")
            .added_while_live
            .is_empty());

        // Another production live without the source, then with it.
        let before = [
            production("p1", "active", &["src-1"]),
            production("p2", "active", &["src-9"]),
        ];
        look(&mut watch, &before, "src-1");
        let after = [
            production("p1", "active", &["src-1"]),
            production("p2", "active", &["src-9", "src-1"]),
        ];
        let verdict = look(&mut watch, &after, "src-1");
        assert!(verdict.send());
        assert_eq!(verdict.added_while_live, ["Show p2"]);
        // Still so on the next look.
        assert_eq!(
            look(&mut watch, &after, "src-1").added_while_live,
            ["Show p2"]
        );
        // The other source of p2 was there before: not suspected.
        assert!(watch.verdict(&after, "src-9").added_while_live.is_empty());

        // Deactivated and reactivated: the slot has its receiver now.
        let off = [
            production("p1", "active", &["src-1"]),
            production("p2", "inactive", &["src-9", "src-1"]),
        ];
        assert!(look(&mut watch, &off, "src-1").added_while_live.is_empty());
        let on = [
            production("p1", "active", &["src-1"]),
            production("p2", "activating", &["src-9", "src-1"]),
        ];
        look(&mut watch, &on, "src-1");
        let on = [
            production("p1", "active", &["src-1"]),
            production("p2", "active", &["src-9", "src-1"]),
        ];
        assert!(look(&mut watch, &on, "src-1").added_while_live.is_empty());
    }

    /// Added while inactive, then activated: the normal path, nothing told.
    #[test]
    fn a_slot_added_before_activation_is_not_suspected() {
        let mut watch = Watch::new();
        look(&mut watch, &[production("p1", "inactive", &[])], "src-1");
        look(
            &mut watch,
            &[production("p1", "inactive", &["src-1"])],
            "src-1",
        );
        let verdict = look(
            &mut watch,
            &[production("p1", "active", &["src-1"])],
            "src-1",
        );
        assert!(verdict.send());
        assert!(verdict.added_while_live.is_empty());
    }

    /// A slot removed and put back while live is still a late slot; one removed for
    /// good is forgotten.
    #[test]
    fn a_late_slot_removed_is_forgotten() {
        let mut watch = Watch::new();
        look(&mut watch, &[production("p1", "active", &[])], "src-1");
        let late = look(
            &mut watch,
            &[production("p1", "active", &["src-1"])],
            "src-1",
        );
        assert_eq!(late.added_while_live, ["Show p1"]);
        let gone = look(&mut watch, &[production("p1", "active", &[])], "src-1");
        assert_eq!(gone, Verdict::default());
        let back = look(
            &mut watch,
            &[production("p1", "active", &["src-1"])],
            "src-1",
        );
        assert_eq!(back.added_while_live, ["Show p1"]);
    }
}
