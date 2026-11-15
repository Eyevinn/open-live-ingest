//! The install scripts install the Strom release the gateway is built against. That
//! version is written in Cargo.toml, install.sh and install.ps1, so this checks they
//! agree, and that build.rs hands the same one to the binary. The macOS app script
//! reads it from Cargo.toml, and the release workflow's DMG is built by that script.

use std::fs;
use std::path::Path;

fn read(rel: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(rel);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
}

fn quoted_after<'a>(text: &'a str, key: &str) -> &'a str {
    let line = text
        .lines()
        .find(|l| l.trim_start().starts_with(key))
        .unwrap_or_else(|| panic!("no line starting with {key:?}"));
    let start = line.find('"').expect("opening quote") + 1;
    let end = start + line[start..].find('"').expect("closing quote");
    &line[start..end]
}

/// The `tag = "..."` of the strom-types dependency in Cargo.toml.
fn strom_types_tag(cargo: &str) -> &str {
    let strom_line = cargo
        .lines()
        .find(|l| l.starts_with("strom-types"))
        .expect("strom-types dependency");
    let tag_pos = strom_line
        .find("tag = \"")
        .expect("strom-types pinned by tag")
        + 7;
    &strom_line[tag_pos..tag_pos + strom_line[tag_pos..].find('"').unwrap()]
}

#[test]
fn install_script_pins_the_same_strom_as_cargo_toml() {
    let cargo = read("Cargo.toml");
    let tag = strom_types_tag(&cargo);

    let script = read("install.sh");
    let script_version = quoted_after(&script, "STROM_VERSION=");

    assert_eq!(
        script_version, tag,
        "install.sh installs Strom {script_version} but Cargo.toml pins strom-types to {tag}"
    );
}

#[test]
fn windows_install_script_pins_the_same_strom_as_cargo_toml() {
    let cargo = read("Cargo.toml");
    let tag = strom_types_tag(&cargo);

    let script = read("install.ps1");
    let script_version = quoted_after(&script, "$StromVersion =");

    assert_eq!(
        script_version, tag,
        "install.ps1 installs Strom {script_version} ($StromVersion) but Cargo.toml pins strom-types to {tag}"
    );
}

/// build.rs hands the binary the `strom-types` tag as the pinned Strom; the startup
/// version check compares the running Strom with it.
#[test]
fn the_binary_is_given_the_strom_types_tag_as_its_pinned_strom() {
    let cargo = read("Cargo.toml");
    assert!(
        cargo.contains(&format!("tag = \"{}\"", env!("STROM_PINNED_VERSION"))),
        "STROM_PINNED_VERSION is {} but Cargo.toml pins strom-types elsewhere",
        env!("STROM_PINNED_VERSION")
    );
}

/// Every `vX.Y.Z` written in `text`.
fn version_literals(text: &str) -> Vec<&str> {
    let bytes = text.as_bytes();
    let mut found = Vec::new();
    for (i, _) in text.match_indices('v') {
        let rest = &text[i + 1..];
        let len = rest
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .unwrap_or(rest.len());
        let candidate = rest[..len].trim_end_matches('.');
        let parts: Vec<&str> = candidate.split('.').collect();
        let preceded_by_word =
            i > 0 && (bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_');
        if !preceded_by_word && parts.len() == 3 && parts.iter().all(|p| !p.is_empty()) {
            found.push(&text[i..i + 1 + candidate.len()]);
        }
    }
    found
}

/// The macOS app script bundles the Strom release named by the strom-types tag. It reads
/// the tag from Cargo.toml rather than carrying its own copy, so it cannot drift; any
/// version it does spell out (in its docs, say) must be the pinned one.
#[test]
fn the_macos_app_script_bundles_the_strom_cargo_toml_pins() {
    let script = read(".github/scripts/build-macos-app.sh");
    assert!(
        script
            .lines()
            .any(|l| l.trim_start().starts_with("strom_tag=")
                && l.contains("strom-types")
                && l.contains("Cargo.toml")),
        "build-macos-app.sh must derive strom_tag from the strom-types tag in Cargo.toml"
    );
    assert!(
        !script.contains("STROM_VERSION="),
        "build-macos-app.sh must not carry its own STROM_VERSION"
    );
    let pinned = env!("STROM_PINNED_VERSION");
    for literal in version_literals(&script) {
        assert_eq!(
            literal, pinned,
            "build-macos-app.sh names Strom {literal} but Cargo.toml pins strom-types to {pinned}"
        );
    }
}

/// The release DMG is built by the macOS app script, so it bundles the Strom the
/// strom-types tag names. The workflow must not hand the script a Strom of its own.
#[test]
fn the_release_dmg_bundles_the_strom_cargo_toml_pins() {
    let workflow = read(".github/workflows/release.yml");
    assert!(
        workflow.lines().any(|l| {
            let l = l.trim_start();
            !l.starts_with('#') && l.contains(".github/scripts/build-macos-app.sh \"")
        }),
        "release.yml must build the app with build-macos-app.sh"
    );
    for knob in ["STROM_ASSET", "STROM_VERSION", "strom_tag"] {
        assert!(
            !workflow.contains(knob),
            "release.yml sets {knob}; the DMG must bundle the Strom Cargo.toml pins"
        );
    }
}

#[test]
fn version_literals_finds_tags_and_skips_the_rest() {
    let text = "Strom v0.8.2, not v0.8, nor env1.2.3, regex ^v[0-9]+, and v1.10.0.";
    assert_eq!(version_literals(text), vec!["v0.8.2", "v1.10.0"]);
}
