//! The SRT relay under Advanced (OL-66, #160 item 1).
//!
//! Go live needs no choice: the library sends through Open Live's SRT relay when
//! Open Live reports one, and to a port on its Strom otherwise (#159, OL-65). Only
//! when Open Live reports more than one relay does Advanced offer a picker, which
//! writes `uplink.relay`. It is also offered when the relay the settings name is not
//! among those Open Live reports, so a relay that went away can be changed: Go live
//! otherwise refuses with the names Open Live does have (`messages::Failure::RelayUnknown`).

use crate::messages::Message;
use anyhow::Result;
use open_live_ingest::api::{self, OscCredential, SaveError};
use open_live_ingest::config::{self, Config, UplinkMode};
use open_live_ingest::run;
use serde::Serialize;
use std::path::Path;

/// What Advanced shows of the relays.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Relays {
    /// The relays Open Live reports, by name, `default` first as it lists them.
    /// Empty when it reports none, is older than relays (v0.8.0), or is not asked.
    pub relays: Vec<String>,
    /// The relay Go live sends through: `uplink.relay`, or `default`.
    pub chosen: String,
    /// Whether Advanced offers the picker.
    pub offer: bool,
}

impl Relays {
    pub fn new(relays: Vec<String>, chosen: &str) -> Self {
        let offer = offers_picker(&relays, chosen);
        Relays {
            relays,
            chosen: chosen.to_string(),
            offer,
        }
    }
}

/// More than one relay to pick from, or a chosen one Open Live no longer reports
/// while it reports others.
pub fn offers_picker(relays: &[String], chosen: &str) -> bool {
    relays.len() > 1 || (!relays.is_empty() && !relays.iter().any(|r| r == chosen))
}

/// The relays Open Live reports in its server info, for the settings as saved.
/// Nothing is asked when the settings do not register with Open Live, or say
/// `listener`, which never goes through a relay. `credential` is the Open Source
/// Cloud sign-in in the keychain, for an instance on Open Source Cloud.
pub async fn lookup(settings: &Config, credential: Option<OscCredential>) -> Result<Relays> {
    let chosen = settings.uplink.relay_name();
    let asks = settings.open_live.register
        && settings.uplink.mode != UplinkMode::Listener
        && settings
            .open_live
            .url
            .as_deref()
            .is_some_and(|u| !u.trim().is_empty());
    if !asks {
        return Ok(Relays::new(Vec::new(), chosen));
    }
    let Some(client) = run::open_live_client(settings, credential.as_ref())? else {
        return Ok(Relays::new(Vec::new(), chosen));
    };
    let names = client
        .server_info()
        .await?
        .map(|info| info.relays().iter().map(|r| r.name.clone()).collect())
        .unwrap_or_default();
    Ok(Relays::new(names, chosen))
}

/// Saves `relay` as `uplink.relay` in the settings file; `None` (or `default`)
/// clears it, so Go live uses `default`. A name Open Live and Strom would not accept
/// is refused before anything is written.
pub fn choose(settings_file: &Path, relay: Option<&str>) -> Result<Config, Message> {
    let relay = relay.map(str::trim).filter(|r| !r.is_empty());
    if let Some(name) = relay {
        if !config::is_relay_name(name) {
            return Err(Message::new(
                "A relay name is 1 to 32 characters: lower-case letters, digits and -.",
                None,
            ));
        }
    }
    let mut settings = api::load_from(settings_file).map_err(|err| {
        Message::new(
            format!(
                "The settings file has a problem: {}.",
                err.root_cause()
                    .to_string()
                    .lines()
                    .next()
                    .unwrap_or_default()
            ),
            None,
        )
    })?;
    settings.uplink.relay = relay
        .filter(|r| *r != config::DEFAULT_RELAY)
        .map(str::to_string);
    match api::save_to(settings_file, &settings) {
        Ok(()) => Ok(settings),
        Err(SaveError::Invalid(problems)) => Err(Message::new(
            format!(
                "The settings file has a problem: {}.",
                problems
                    .first()
                    .map_or("", |p| p.message.as_str())
                    .trim_end_matches('.')
            ),
            None,
        )),
        Err(SaveError::Write(err)) => {
            tracing::warn!("saving the relay: {err:#}");
            Err(Message::new(
                "The settings could not be saved. Show logs for details.",
                None,
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|n| n.to_string()).collect()
    }

    #[test]
    fn the_picker_is_offered_only_for_more_than_one_relay_or_a_gone_one() {
        assert!(!offers_picker(&[], "default"));
        assert!(!offers_picker(&names(&["default"]), "default"));
        assert!(offers_picker(&names(&["default", "eu"]), "default"));
        // The one saved is gone, and Open Live has another: offered, to change it.
        assert!(offers_picker(&names(&["default"]), "eu"));
        // No relay at all: nothing to pick, Go live sends to a port as before.
        assert!(!offers_picker(&[], "eu"));
    }

    fn settings_for(scratch: &testing::Scratch, open_live: &str, extra: &str) -> Config {
        std::fs::write(
            scratch.settings(),
            format!(
                "[open_live]\nurl = \"{open_live}\"\napi_key = \"the-key\"\n{extra}\n[[input]]\ndevice = \"test\"\n"
            ),
        )
        .unwrap();
        api::load_from(&scratch.settings()).unwrap()
    }

    /// An Open Live with relays in its server info (`srt.relays`): their names, and
    /// the picker when there is more than one.
    #[tokio::test]
    async fn the_relays_are_read_from_open_lives_server_info() {
        let open_live = testing::fake_open_live_with(
            "the-key",
            testing::server_info_with_relays(&[
                ("default", "relay.example.com:9000"),
                ("eu", "relay-eu.example.com:9000"),
            ]),
        );
        let scratch = testing::Scratch::new("relay-lookup");
        let relays = lookup(&settings_for(&scratch, &open_live, ""), None)
            .await
            .unwrap();
        assert_eq!(
            relays,
            Relays {
                relays: names(&["default", "eu"]),
                chosen: "default".to_string(),
                offer: true,
            }
        );

        let one = testing::fake_open_live_with(
            "the-key",
            testing::server_info_with_relays(&[("default", "relay.example.com:9000")]),
        );
        let relays = lookup(&settings_for(&scratch, &one, ""), None)
            .await
            .unwrap();
        assert_eq!(relays.relays, names(&["default"]));
        assert!(!relays.offer);
    }

    /// OL-66 AC3: Open Live v0.8.0 has no `srt` block: no relays, no picker, and Go
    /// live sends to a port on its Strom as before.
    #[tokio::test]
    async fn an_open_live_without_relays_offers_no_picker() {
        let open_live = testing::fake_open_live_with("the-key", testing::server_info_v0_8_0());
        let scratch = testing::Scratch::new("relay-v080");
        let relays = lookup(&settings_for(&scratch, &open_live, ""), None)
            .await
            .unwrap();
        assert_eq!(relays, Relays::new(Vec::new(), "default"));
        assert!(!relays.offer);
    }

    /// `listener` never goes through a relay, so Open Live is not asked.
    #[tokio::test]
    async fn listener_mode_does_not_ask() {
        let scratch = testing::Scratch::new("relay-listener");
        // Nothing listens there: asking would fail.
        let settings = settings_for(
            &scratch,
            "http://127.0.0.1:9",
            "\n[uplink]\nmode = \"listener\"\nport_range = \"47110-47112\"\n",
        );
        let relays = lookup(&settings, None).await.unwrap();
        assert!(relays.relays.is_empty());
    }

    #[test]
    fn choosing_a_relay_writes_uplink_relay_and_default_clears_it() {
        let scratch = testing::Scratch::new("relay-choose");
        settings_for(&scratch, "https://open-live.example.com", "");
        let saved = choose(&scratch.settings(), Some("eu")).unwrap();
        assert_eq!(saved.uplink.relay.as_deref(), Some("eu"));
        let read = api::load_from(&scratch.settings()).unwrap();
        assert_eq!(read.uplink.relay.as_deref(), Some("eu"));
        assert_eq!(read.uplink.relay_name(), "eu");

        let saved = choose(&scratch.settings(), Some("default")).unwrap();
        assert_eq!(saved.uplink.relay, None);
        assert_eq!(
            api::load_from(&scratch.settings())
                .unwrap()
                .uplink
                .relay_name(),
            "default"
        );
        assert!(choose(&scratch.settings(), Some("eu")).is_ok());
        assert_eq!(
            choose(&scratch.settings(), None).unwrap().uplink.relay,
            None
        );
        testing::cli_check(&scratch);
    }

    #[test]
    fn a_relay_name_open_live_would_not_accept_is_refused() {
        let scratch = testing::Scratch::new("relay-bad-name");
        settings_for(&scratch, "https://open-live.example.com", "");
        let before = std::fs::read_to_string(scratch.settings()).unwrap();
        let err = choose(&scratch.settings(), Some("EU West")).unwrap_err();
        assert!(
            err.message.starts_with("A relay name is"),
            "{}",
            err.message
        );
        assert_eq!(std::fs::read_to_string(scratch.settings()).unwrap(), before);
    }
}
