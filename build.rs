//! Hands the binary the Strom release it is built against, read from the
//! `strom-types` tag in Cargo.toml, so that version is written in one place only.

use std::fs;

fn main() {
    println!("cargo:rerun-if-changed=Cargo.toml");
    let manifest = fs::read_to_string("Cargo.toml").expect("reading Cargo.toml");
    let tag = strom_types_tag(&manifest).expect("Cargo.toml pins strom-types by tag");
    println!("cargo:rustc-env=STROM_PINNED_VERSION={tag}");
}

fn strom_types_tag(manifest: &str) -> Option<&str> {
    let line = manifest
        .lines()
        .find(|l| l.trim_start().starts_with("strom-types"))?;
    let start = line.find("tag = \"")? + "tag = \"".len();
    let end = start + line[start..].find('"')?;
    Some(&line[start..end])
}
