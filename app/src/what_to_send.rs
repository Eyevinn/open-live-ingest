//! The What to send step (docs/specs/desktop-app.md, sections 5 and 6): one row per video
//! device Strom reports plus the test pattern, each with a tick box, an audio picker and
//! the Studio name; and the ticked rows saved as the settings' `[[input]]` entries, which
//! `up` sends exactly.
//!
//! Window-free, so the model is unit tested on any OS: [`model`] builds the rows from a
//! device list and the settings, [`entries`] turns what the page sends back into
//! `[[input]]` entries, and [`check`] refuses a selection before it is saved.

use crate::ipc::InputArgs;
use open_live_ingest::api::Devices;
use open_live_ingest::config::{self, Config, InputSettings, TEST_PATTERN_DEVICE};
use open_live_ingest::devices::name_prefix;
use open_live_ingest::gateway::Events;
use open_live_ingest::run::DeviceReport;
use serde::Serialize;
use std::future::Future;
use std::pin::Pin;

/// The test pattern row's name, the one the CLI gives its source.
pub const TEST_PATTERN_NAME: &str = "Test pattern";

/// Shown when Strom reports no video device at all (spec, section 10).
pub const NO_CAMERAS: &str = "No cameras found. Connect one, or tick Test pattern.";

/// A device list on its way.
pub type DeviceList<'a> = Pin<Box<dyn Future<Output = anyhow::Result<Devices>> + Send + 'a>>;

/// Where the device list comes from: Strom through the library, or a fake in tests.
/// Run on the core's runtime, off the window's thread: listing may start a Strom and
/// wait for it.
pub trait DeviceSource: Send + Sync {
    fn devices<'a>(&'a self, settings: &'a Config) -> DeviceList<'a>;
}

/// The devices the local Strom reports, through the library's `devices`: a managed
/// Strom is started to ask, if nothing is listening, and stopped again.
#[derive(Debug, Default)]
pub struct Strom;

impl DeviceSource for Strom {
    fn devices<'a>(&'a self, settings: &'a Config) -> DeviceList<'a> {
        Box::pin(async move { open_live_ingest::api::devices(settings, &Events::discard()).await })
    }
}

/// What the page shows in the What to send step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WhatToSend {
    /// `<gateway.name> — `, shown before every label and never editable: `down` and the
    /// source sweep find this gateway's sources by it.
    pub name_prefix: String,
    /// Whether virtual devices are listed.
    pub show_all: bool,
    /// The test pattern first, then the cameras by name, then saved inputs that are
    /// not connected now.
    pub video: Vec<Row>,
    /// Every audio source Strom reports, by name. "None" is the page's own first choice.
    pub audio: Vec<AudioChoice>,
    /// One line about the list as a whole, such as no cameras at all.
    pub notice: Option<&'static str>,
}

/// One input the person can tick.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Row {
    /// The Strom device id, or `test` for the test pattern; what `[[input]]` saves.
    pub device: String,
    /// The device's name, as Strom reports it.
    pub name: String,
    pub test_pattern: bool,
    #[serde(rename = "virtual")]
    pub virtual_: bool,
    /// False for a saved input Strom does not report now: shown so it can be unticked,
    /// skipped when going live.
    pub connected: bool,
    pub ticked: bool,
    /// The paired audio source's device id, when ticked with one.
    pub audio: Option<String>,
    /// The editable part of the Studio name, prefilled with the device's name.
    pub label: String,
}

/// One entry of the audio picker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AudioChoice {
    pub device: String,
    pub name: String,
    /// The ticked row this source is paired with. The other rows show it disabled:
    /// one audio source goes with one camera.
    pub paired_with: Option<String>,
}

/// The device among `devices` that `wanted` names, as `up` resolves an `[[input]]`
/// entry: the exact id, else the one device whose id or name contains it.
fn resolve<'a>(devices: &'a [DeviceReport], wanted: &str) -> Option<&'a DeviceReport> {
    let wanted = wanted.trim();
    if let Some(exact) = devices.iter().find(|d| d.id == wanted) {
        return Some(exact);
    }
    let needle = wanted.to_lowercase();
    let mut found = devices
        .iter()
        .filter(|d| d.id.eq_ignore_ascii_case(wanted) || d.name.to_lowercase().contains(&needle));
    match (found.next(), found.next()) {
        (Some(one), None) => Some(one),
        _ => None,
    }
}

/// The rows for `devices` (none listed yet: only the test pattern and the saved inputs)
/// with the settings' `[[input]]` entries ticked.
pub fn model(devices: Option<&Devices>, settings: &Config, show_all: bool) -> WhatToSend {
    let (video, audio): (&[DeviceReport], &[DeviceReport]) = match devices {
        Some(d) => (&d.video, &d.audio),
        None => (&[], &[]),
    };
    let saved = &settings.inputs;
    let audio_of = |entry: &InputSettings| {
        entry
            .audio
            .as_deref()
            .map(|a| resolve(audio, a).map_or_else(|| a.trim().to_string(), |d| d.id.clone()))
    };
    let label_or = |entry: Option<&InputSettings>, name: &str| {
        entry
            .and_then(InputSettings::label)
            .unwrap_or(name)
            .to_string()
    };

    let test = saved.iter().find(|e| e.is_test_pattern());
    let mut rows = vec![Row {
        device: TEST_PATTERN_DEVICE.to_string(),
        name: TEST_PATTERN_NAME.to_string(),
        test_pattern: true,
        virtual_: false,
        connected: true,
        ticked: test.is_some(),
        audio: None,
        label: label_or(test, TEST_PATTERN_NAME),
    }];
    let mut matched = vec![false; saved.len()];
    for device in video {
        let at = saved
            .iter()
            .position(|e| !e.is_test_pattern() && resolve(video, &e.device) == Some(device));
        let entry = at.map(|i| &saved[i]);
        if let Some(i) = at {
            matched[i] = true;
        }
        // A ticked virtual device stays listed, or it could not be unticked.
        if device.virtual_ && !show_all && entry.is_none() {
            continue;
        }
        rows.push(Row {
            device: device.id.clone(),
            name: device.name.clone(),
            test_pattern: false,
            virtual_: device.virtual_,
            connected: true,
            ticked: entry.is_some(),
            audio: entry.and_then(audio_of),
            label: label_or(entry, &device.name),
        });
    }
    // Saved inputs Strom does not report: listed only once there is a list to miss
    // them from.
    if devices.is_some() {
        for (entry, _) in saved
            .iter()
            .zip(&matched)
            .filter(|(e, matched)| !**matched && !e.is_test_pattern())
        {
            let device = entry.device.trim();
            rows.push(Row {
                device: device.to_string(),
                name: device.to_string(),
                test_pattern: false,
                virtual_: false,
                connected: false,
                ticked: true,
                audio: audio_of(entry),
                label: label_or(Some(entry), device),
            });
        }
    }

    let audio = audio
        .iter()
        .map(|source| AudioChoice {
            device: source.id.clone(),
            name: source.name.clone(),
            paired_with: rows
                .iter()
                .find(|r| r.ticked && r.audio.as_deref() == Some(source.id.as_str()))
                .map(|r| r.device.clone()),
        })
        .collect();
    WhatToSend {
        name_prefix: name_prefix(&settings.gateway.name),
        show_all,
        video: rows,
        audio,
        notice: devices
            .is_some_and(|d| d.video.is_empty())
            .then_some(NO_CAMERAS),
    }
}

/// The `[[input]]` entries for the ticked rows the page sends: device ids as they are,
/// no audio for "None", and no label where it is blank or still the device's name, so
/// the source keeps following the device's name.
pub fn entries(inputs: &[InputArgs], devices: Option<&Devices>) -> Vec<InputSettings> {
    let name_of = |device: &str| -> Option<String> {
        if device.eq_ignore_ascii_case(TEST_PATTERN_DEVICE) {
            return Some(TEST_PATTERN_NAME.to_string());
        }
        devices?
            .video
            .iter()
            .find(|d| d.id == device)
            .map(|d| d.name.clone())
    };
    let some_trimmed = |value: &Option<String>| {
        value
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_string)
    };
    inputs
        .iter()
        .map(|input| {
            let device = input.device.trim().to_string();
            let name = name_of(&device);
            InputSettings {
                audio: some_trimmed(&input.audio),
                label: some_trimmed(&input.label).filter(|l| Some(l) != name.as_ref()),
                device,
            }
        })
        .collect()
}

/// Why a selection cannot be saved, in one line for the person: the §10 wording for
/// an audio source paired twice, else the first problem `validate` finds in it.
pub fn check(entries: &[InputSettings], devices: Option<&Devices>) -> Result<(), String> {
    let name = |list: fn(&Devices) -> &[DeviceReport], id: &str| {
        devices
            .and_then(|d| list(d).iter().find(|r| r.id == id))
            .map_or_else(|| id.to_string(), |r| r.name.clone())
    };
    for (i, entry) in entries.iter().enumerate() {
        let Some(audio) = entry.audio.as_deref() else {
            continue;
        };
        if let Some(first) = entries[..i].iter().find(|e| {
            e.audio
                .as_deref()
                .is_some_and(|a| a.eq_ignore_ascii_case(audio))
        }) {
            return Err(format!(
                "{} is already used with {}. Pick another audio input.",
                name(|d| &d.audio, audio),
                name(|d| &d.video, &first.device)
            ));
        }
    }
    match config::input_problems(entries).into_iter().next() {
        Some(problem) => Err(problem.message),
        None => Ok(()),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use open_live_ingest::run::DeviceKind;

    fn report(id: &str, name: &str, kind: DeviceKind, virtual_: bool) -> DeviceReport {
        DeviceReport {
            name: name.to_string(),
            id: id.to_string(),
            kind,
            virtual_,
        }
    }

    pub(crate) fn devices() -> Devices {
        Devices {
            video: vec![
                report("dev-cam", "FaceTime HD Camera", DeviceKind::Video, false),
                report("dev-obs", "OBS Virtual Camera", DeviceKind::Video, true),
            ],
            audio: vec![
                report("dev-mic", "USB Microphone", DeviceKind::Audio, false),
                report(
                    "dev-int",
                    "MacBook Pro Microphone",
                    DeviceKind::Audio,
                    false,
                ),
            ],
        }
    }

    fn settings(inputs: &str) -> Config {
        toml::from_str(&format!("[gateway]\nname = \"Venue\"\n{inputs}")).unwrap()
    }

    fn input(device: &str, audio: Option<&str>, label: Option<&str>) -> InputArgs {
        InputArgs {
            device: device.to_string(),
            audio: audio.map(str::to_string),
            label: label.map(str::to_string),
        }
    }

    fn summary(model: &WhatToSend) -> Vec<(&str, bool, bool, Option<&str>, &str)> {
        model
            .video
            .iter()
            .map(|r| {
                (
                    r.device.as_str(),
                    r.connected,
                    r.ticked,
                    r.audio.as_deref(),
                    r.label.as_str(),
                )
            })
            .collect()
    }

    #[test]
    fn nothing_saved_lists_the_test_pattern_and_the_real_cameras_unticked() {
        let model = model(Some(&devices()), &settings(""), false);
        assert_eq!(model.name_prefix, "Venue — ");
        assert_eq!(
            summary(&model),
            [
                ("test", true, false, None, "Test pattern"),
                ("dev-cam", true, false, None, "FaceTime HD Camera"),
            ]
        );
        assert!(model.video[0].test_pattern);
        assert_eq!(model.notice, None);
        let audio: Vec<(&str, Option<&str>)> = model
            .audio
            .iter()
            .map(|a| (a.device.as_str(), a.paired_with.as_deref()))
            .collect();
        assert_eq!(audio, [("dev-mic", None), ("dev-int", None)]);
    }

    #[test]
    fn no_video_device_at_all_says_so() {
        let none = Devices {
            video: Vec::new(),
            audio: devices().audio,
        };
        let model = model(Some(&none), &settings(""), false);
        assert_eq!(model.notice, Some(NO_CAMERAS));
        assert_eq!(
            summary(&model),
            [("test", true, false, None, "Test pattern")]
        );
    }

    #[test]
    fn show_all_lists_virtual_devices_too() {
        let model = model(Some(&devices()), &settings(""), true);
        assert!(model.show_all);
        let row = &model.video[2];
        assert_eq!((row.device.as_str(), row.virtual_), ("dev-obs", true));
    }

    #[test]
    fn the_saved_selection_is_ticked_with_its_audio_and_label() {
        let saved = settings(
            "[[input]]\ndevice = \"dev-obs\"\naudio = \"USB Microphone\"\nlabel = \"Stage left\"\n\n\
             [[input]]\ndevice = \"test\"\n\n[[input]]\ndevice = \"dev-gone\"\nlabel = \"Gone\"\n",
        );
        let model = model(Some(&devices()), &saved, false);
        assert_eq!(
            summary(&model),
            [
                ("test", true, true, None, "Test pattern"),
                ("dev-cam", true, false, None, "FaceTime HD Camera"),
                // Virtual, but ticked, so listed without Show all.
                ("dev-obs", true, true, Some("dev-mic"), "Stage left"),
                ("dev-gone", false, true, None, "Gone"),
            ]
        );
        // The paired source is taken by that row; the page disables it in the others.
        assert_eq!(model.audio[0].paired_with.as_deref(), Some("dev-obs"));
        assert_eq!(model.audio[1].paired_with, None);
    }

    #[test]
    fn before_strom_answers_only_the_test_pattern_is_listed() {
        let saved = settings("[[input]]\ndevice = \"dev-cam\"\n");
        let model = model(None, &saved, false);
        assert_eq!(
            summary(&model),
            [("test", true, false, None, "Test pattern")]
        );
        assert!(model.audio.is_empty());
    }

    #[test]
    fn entries_keep_ids_drop_none_audio_and_a_label_that_is_the_device_name() {
        let saved = entries(
            &[
                input("dev-cam", Some("dev-mic"), Some("FaceTime HD Camera")),
                input("dev-obs", Some(" "), Some(" Stage left ")),
                input("test", None, Some("Test pattern")),
            ],
            Some(&devices()),
        );
        assert_eq!(
            saved,
            [
                InputSettings {
                    device: "dev-cam".into(),
                    audio: Some("dev-mic".into()),
                    label: None,
                },
                InputSettings {
                    device: "dev-obs".into(),
                    audio: None,
                    label: Some("Stage left".into()),
                },
                InputSettings {
                    device: "test".into(),
                    audio: None,
                    label: None,
                },
            ]
        );
    }

    /// #64 AC5 in the app: one audio source goes with one camera, in the person's words.
    #[test]
    fn an_audio_source_paired_twice_is_refused_by_name() {
        let saved = entries(
            &[
                input("dev-cam", Some("dev-mic"), None),
                input("dev-obs", Some("dev-mic"), None),
            ],
            Some(&devices()),
        );
        assert_eq!(
            check(&saved, Some(&devices())),
            Err(
                "USB Microphone is already used with FaceTime HD Camera. Pick another audio input."
                    .to_string()
            )
        );
        // `validate` refuses the same file.
        let mut cfg = settings("");
        cfg.inputs = saved;
        assert!(config::validate(&cfg).is_err());
    }

    #[test]
    fn other_problems_are_validates_own_words() {
        let saved = entries(
            &[
                input("dev-cam", None, Some("Stage")),
                input("dev-obs", None, Some("Stage")),
            ],
            None,
        );
        let err = check(&saved, None).unwrap_err();
        assert!(err.contains("cannot share a name"), "{err}");
        assert_eq!(
            check(&entries(&[input("test", None, None)], None), None),
            Ok(())
        );
    }

    /// What the app saves, `up` reads the same way: the model of a saved selection is
    /// the selection the page sent.
    #[test]
    fn a_saved_selection_reads_back_as_sent() {
        let sent = [
            input("test", None, None),
            input("dev-cam", Some("dev-int"), Some("Wide")),
        ];
        let mut cfg = settings("");
        cfg.inputs = entries(&sent, Some(&devices()));
        let text = toml::to_string_pretty(&cfg).unwrap();
        let back: Config = toml::from_str(&text).unwrap();
        let model = model(Some(&devices()), &back, false);
        assert_eq!(
            summary(&model),
            [
                ("test", true, true, None, "Test pattern"),
                ("dev-cam", true, true, Some("dev-int"), "Wide"),
            ]
        );
    }
}
