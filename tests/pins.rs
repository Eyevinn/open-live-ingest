//! The install script installs the Strom release the gateway is built against. That
//! version is written in two places, so this checks they agree.

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

#[test]
fn install_script_pins_the_same_strom_as_cargo_toml() {
    let cargo = read("Cargo.toml");
    let strom_line = cargo
        .lines()
        .find(|l| l.starts_with("strom-types"))
        .expect("strom-types dependency");
    let tag_pos = strom_line
        .find("tag = \"")
        .expect("strom-types pinned by tag")
        + 7;
    let tag = &strom_line[tag_pos..tag_pos + strom_line[tag_pos..].find('"').unwrap()];

    let script = read("install.sh");
    let script_version = quoted_after(&script, "STROM_VERSION=");

    assert_eq!(
        script_version, tag,
        "install.sh installs Strom {script_version} but Cargo.toml pins strom-types to {tag}"
    );
}
