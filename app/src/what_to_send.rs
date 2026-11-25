//! The What to send step (docs/specs/desktop-app.md, sections 5 and 6): one row per video
//! device Strom reports plus the test pattern, each with a tick box, an audio picker and
//! the Studio name; and the ticked rows saved as the settings' `[[input]]` entries, which
//! `up` sends exactly.
//!
//! Window-free, so the model is unit tested on any OS: [`model`] builds the rows from a
//! device list and the settings, [`entries`] turns what the page sends back into
//! `[[input]]` entries, and [`check`] refuses a selection before it is saved.

use crate::go_live::RowState;
use crate::ipc::InputArgs;
use open_live_ingest::api::Devices;
use open_live_ingest::config::{self, Config, InputSettings, TEST_PATTERN_DEVICE};
use open_live_ingest::devices::name_prefix;
use open_live_ingest::run::DeviceReport;
use serde::Serialize;
use std::future::Future;
use std::pin::Pin;

/// The test pattern row's name, the one the CLI gives its source.
pub const TEST_PATTERN_NAME: &str = "Test pattern";

/// Shown when Strom reports no video device at all (spec, section 10).
pub const NO_CAMERAS: &str = "No cameras found. Connect one, or tick Test pattern.";

/// Shown instead of a level bar on a row whose audio is "None" (spec, section 5.2).
pub const NO_MICROPHONE: &str = "No microphone";

/// Shown in the picture area of a step 3 row that is Armed or Waiting for production:
/// no production is live, so its uplink, and its camera, are off (spec, section 5.2).
pub const CAMERA_OFF_UNTIL_LIVE: &str = "Camera off until a production goes live";

/// A device list on its way.
pub type DeviceList<'a> = Pin<Box<dyn Future<Output = anyhow::Result<Devices>> + Send + 'a>>;

/// Where the device list comes from: Strom through the library, or a fake in tests.
/// Run on the core's runtime, off the window's thread: listing may start a Strom and
/// wait for it.
pub trait DeviceSource: Send + Sync {
    fn devices<'a>(&'a self, settings: &'a Config) -> DeviceList<'a>;
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
    /// The preview's fixed words, from the core like every other line the rows show.
    pub preview_text: PreviewText,
}

/// The preview's fixed words (spec, section 5.2). The `preview` and `notice` events
/// carry everything else the rows show.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PreviewText {
    /// Instead of a level bar, on a row with no microphone.
    pub no_microphone: &'static str,
    /// In the picture area of a step 3 row that is Armed or Waiting for production.
    pub camera_off: &'static str,
    /// The step 3 input states whose camera is off: no flow, so no picture and no bar.
    pub camera_off_states: [RowState; 2],
}

/// The preview's words, as the page gets them.
pub const PREVIEW_TEXT: PreviewText = PreviewText {
    no_microphone: NO_MICROPHONE,
    camera_off: CAMERA_OFF_UNTIL_LIVE,
    camera_off_states: [RowState::Armed, RowState::WaitingForProduction],
};

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
    /// The whole Studio name, `<name_prefix><label>`: what the step 3 `input` events
    /// name the row by, so the page finds the row's `preview` and `level` (by `device`).
    pub input: String,
    /// Whether the row shows a picture: ticked and connected. The `preview` events
    /// carry its state (`starting`, `showing`, `no_picture`, ...).
    pub picture: bool,
    /// The paired microphone's name; None with audio "None" and for the test pattern.
    pub microphone: Option<String>,
    /// Whether the row shows a level bar: a microphone, or the test pattern's tone.
    /// Without one, a ticked row says [`NO_MICROPHONE`].
    pub sound: bool,
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

    let prefix = name_prefix(&settings.gateway.name);
    // The paired microphone's name, as the preview names it in "No sound from ...".
    let microphone_of = |audio_id: Option<&String>| {
        audio_id.map(|id| {
            audio
                .iter()
                .find(|d| &d.id == id)
                .map_or_else(|| id.clone(), |d| d.name.clone())
        })
    };

    let test = saved.iter().find(|e| e.is_test_pattern());
    let label = label_or(test, TEST_PATTERN_NAME);
    let mut rows = vec![Row {
        device: TEST_PATTERN_DEVICE.to_string(),
        name: TEST_PATTERN_NAME.to_string(),
        test_pattern: true,
        virtual_: false,
        connected: true,
        ticked: test.is_some(),
        audio: None,
        input: format!("{prefix}{label}"),
        label,
        picture: test.is_some(),
        microphone: None,
        sound: true,
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
        let paired = entry.and_then(audio_of);
        let microphone = microphone_of(paired.as_ref());
        let label = label_or(entry, &device.name);
        rows.push(Row {
            device: device.id.clone(),
            name: device.name.clone(),
            test_pattern: false,
            virtual_: device.virtual_,
            connected: true,
            ticked: entry.is_some(),
            audio: paired,
            input: format!("{prefix}{label}"),
            label,
            picture: entry.is_some(),
            sound: microphone.is_some(),
            microphone,
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
            let paired = audio_of(entry);
            let microphone = microphone_of(paired.as_ref());
            let label = label_or(Some(entry), device);
            // Not connected: no preview flow, so no picture.
            rows.push(Row {
                device: device.to_string(),
                name: device.to_string(),
                test_pattern: false,
                virtual_: false,
                connected: false,
                ticked: true,
                audio: paired,
                input: format!("{prefix}{label}"),
                label,
                picture: false,
                sound: microphone.is_some(),
                microphone,
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
        name_prefix: prefix,
        show_all,
        video: rows,
        audio,
        notice: devices
            .is_some_and(|d| d.video.is_empty())
            .then_some(NO_CAMERAS),
        preview_text: PREVIEW_TEXT,
    }
}

/// The name a row shows for `device` before any label: the test pattern's, the listed
/// device's, or for a device that is not listed its id (what [`model`] shows).
fn row_name(device: &str, devices: Option<&Devices>) -> String {
    if device.eq_ignore_ascii_case(TEST_PATTERN_DEVICE) {
        return TEST_PATTERN_NAME.to_string();
    }
    devices
        .and_then(|d| d.video.iter().find(|r| r.id == device))
        .map_or_else(|| device.to_string(), |r| r.name.clone())
}

/// The `[[input]]` entries for the ticked rows the page sends: device ids as they are,
/// no audio for "None", and no label where it is blank or still what the row was
/// prefilled with (the device's name, or the id of a device that is not connected), so
/// the source keeps following the device's name.
pub fn entries(inputs: &[InputArgs], devices: Option<&Devices>) -> Vec<InputSettings> {
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
            let name = row_name(&device, devices);
            InputSettings {
                audio: some_trimmed(&input.audio),
                label: some_trimmed(&input.label).filter(|l| *l != name),
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
    if let Some(problem) = config::input_problems(entries).into_iter().next() {
        return Err(problem.message);
    }
    // The name each input gets in the Studio, as `up` names it: the label, else the
    // device's name. Two the same would share one Open Live source. A device that is
    // not connected is skipped by `up`, so its name is only known by its label.
    let mut names: Vec<(&InputSettings, String)> = Vec::new();
    for entry in entries {
        let listed = entry.is_test_pattern()
            || devices.is_some_and(|d| d.video.iter().any(|r| r.id == entry.device));
        let studio_name = match entry.label() {
            Some(label) => label.to_string(),
            None if listed => row_name(&entry.device, devices),
            None => continue,
        };
        if let Some((first, _)) = names
            .iter()
            .find(|(_, n)| n.eq_ignore_ascii_case(&studio_name))
        {
            return Err(format!(
                "{} already goes to the Studio as \"{studio_name}\". Pick another name.",
                row_name(&first.device, devices)
            ));
        }
        names.push((entry, studio_name));
    }
    Ok(())
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

    /// A saved device that is not connected shows its id as its name and label. Sent
    /// back unchanged with any other change, it is saved as it was: no label, so the
    /// camera's own name returns once it is plugged in again.
    #[test]
    fn a_not_connected_row_round_trips_without_gaining_a_label() {
        let saved = settings(
            "[[input]]\ndevice = \"dev-gone\"\n\n[[input]]\ndevice = \"dev-away\"\nlabel = \"Gone\"\n",
        );
        let shown = model(Some(&devices()), &saved, false);
        let sent: Vec<InputArgs> = shown
            .video
            .iter()
            .filter(|r| r.ticked)
            .map(|r| input(&r.device, r.audio.as_deref(), Some(&r.label)))
            .collect();
        assert_eq!(entries(&sent, Some(&devices())), saved.inputs);
        // Before Strom has answered too.
        assert_eq!(
            entries(&[input("dev-gone", None, Some("dev-gone"))], None),
            [InputSettings {
                device: "dev-gone".into(),
                audio: None,
                label: None,
            }]
        );
    }

    /// Two inputs cannot go to the Studio under one name: a label may not repeat the
    /// test pattern's name or the name of another ticked camera that has no label.
    #[test]
    fn a_label_that_repeats_another_inputs_name_is_refused() {
        let listed = devices();
        let refused = [
            vec![
                input("test", None, None),
                input("dev-cam", None, Some("Test pattern")),
            ],
            vec![
                input("dev-cam", None, None),
                input("dev-obs", None, Some("facetime hd camera")),
            ],
            vec![
                input("dev-cam", None, Some("OBS Virtual Camera")),
                input("dev-obs", None, None),
            ],
        ];
        for sent in refused {
            let err = check(&entries(&sent, Some(&listed)), Some(&listed)).unwrap_err();
            assert!(err.ends_with("Pick another name."), "{err}");
        }
        assert_eq!(
            check(
                &entries(
                    &[
                        input("dev-cam", None, Some("Test pattern")),
                        input("dev-obs", None, None),
                    ],
                    Some(&listed),
                ),
                Some(&listed),
            ),
            Ok(()),
            "the test pattern's name is free when it is not ticked"
        );
        let err = check(
            &entries(
                &[
                    input("test", None, None),
                    input("dev-cam", None, Some("Test pattern")),
                ],
                Some(&listed),
            ),
            Some(&listed),
        )
        .unwrap_err();
        assert_eq!(
            err,
            "Test pattern already goes to the Studio as \"Test pattern\". Pick another name."
        );
    }

    /// OL-70d: each row carries what the page draws for it in steps 2 and 3: whether
    /// it has a picture, the microphone's name or none ("No microphone"), and the
    /// Studio name the step 3 `input` events use. The words are the core's.
    #[test]
    fn the_rows_carry_what_the_page_renders() {
        let saved = settings(
            "[[input]]\ndevice = \"dev-cam\"\naudio = \"dev-mic\"\nlabel = \"Wide\"\n\n\
             [[input]]\ndevice = \"test\"\n\n[[input]]\ndevice = \"dev-gone\"\naudio = \"dev-away\"\n",
        );
        let model = model(Some(&devices()), &saved, true);
        let rows: Vec<(&str, &str, bool, Option<&str>, bool)> = model
            .video
            .iter()
            .map(|r| {
                (
                    r.device.as_str(),
                    r.input.as_str(),
                    r.picture,
                    r.microphone.as_deref(),
                    r.sound,
                )
            })
            .collect();
        assert_eq!(
            rows,
            [
                // The test pattern: a picture and the tone's bar, no microphone.
                ("test", "Venue — Test pattern", true, None, true),
                // A camera with a microphone: its name, for the bar and "No sound from".
                (
                    "dev-cam",
                    "Venue — Wide",
                    true,
                    Some("USB Microphone"),
                    true
                ),
                // Not ticked: nothing to show.
                ("dev-obs", "Venue — OBS Virtual Camera", false, None, false),
                // Not connected: no preview flow, so no picture; a microphone that is not
                // listed is named by its id.
                (
                    "dev-gone",
                    "Venue — dev-gone",
                    false,
                    Some("dev-away"),
                    true
                ),
            ]
        );
        // A ticked camera with audio "None": no bar, and the page says so.
        let none = model_of_one("[[input]]\ndevice = \"dev-cam\"\n");
        assert_eq!(
            (none.picture, none.microphone.as_deref(), none.sound),
            (true, None, false)
        );

        let json = serde_json::to_value(&model).unwrap();
        assert_eq!(json["preview_text"]["no_microphone"], "No microphone");
        assert_eq!(
            json["preview_text"]["camera_off"],
            "Camera off until a production goes live"
        );
        // The armed wording goes with the states the `input` event names them by.
        assert_eq!(
            json["preview_text"]["camera_off_states"],
            serde_json::json!(["armed", "waiting_for_production"])
        );
        let row = &json["video"][1];
        for field in ["device", "input", "picture", "microphone", "sound"] {
            assert!(row.get(field).is_some(), "row has no {field}: {row}");
        }
    }

    fn model_of_one(inputs: &str) -> Row {
        let model = model(Some(&devices()), &settings(inputs), false);
        model
            .video
            .into_iter()
            .find(|r| r.device == "dev-cam")
            .unwrap()
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
