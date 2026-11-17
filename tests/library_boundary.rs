//! The library is driven by the CLI today and by an app next (#62), so it must not
//! assume a terminal or own the process: it never reads stdin, never calls
//! `std::process::exit`, never prints, and installs no signal handler. Those belong to
//! `src/main.rs` and the setup prompts in `src/setup.rs`, which only the binary
//! compiles.
//!
//! Checked on the source: every module `src/lib.rs` declares is read, comment lines
//! skipped, and any use of what only the binary may do is reported by file and line.

use std::path::Path;

/// What only the binary may do, and why each is listed.
const FORBIDDEN: &[(&str, &str)] = &[
    ("stdin()", "reads stdin"),
    ("io::stdin", "reads stdin"),
    ("is_terminal", "asks whether a terminal is attached"),
    ("dialoguer", "prompts on the terminal"),
    ("process::exit", "ends the process"),
    ("process::abort", "ends the process"),
    ("print!", "prints to stdout"),
    ("println!", "prints to stdout"),
    ("eprint!", "prints to stderr"),
    ("eprintln!", "prints to stderr"),
    ("dbg!", "prints to stderr"),
    ("io::stdout", "writes to stdout"),
    ("io::stderr", "writes to stderr"),
    ("tokio::signal", "handles signals"),
    ("ctrl_c", "handles signals"),
];

fn src(rel: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("src").join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
}

/// The modules `src/lib.rs` declares, as `pub mod name;` or `mod name;`.
fn library_modules() -> Vec<String> {
    src("lib.rs")
        .lines()
        .map(str::trim)
        .filter_map(|l| {
            l.strip_prefix("pub mod ")
                .or_else(|| l.strip_prefix("mod "))?
                .strip_suffix(';')
                .map(str::to_string)
        })
        .collect()
}

/// Every forbidden use in `text`, as `file:line: what (why)`.
fn violations(file: &str, text: &str) -> Vec<String> {
    let mut found = Vec::new();
    for (n, line) in text.lines().enumerate() {
        if line.trim_start().starts_with("//") {
            continue;
        }
        for (needle, why) in FORBIDDEN {
            if line.contains(needle) {
                found.push(format!("{file}:{}: {needle} ({why})", n + 1));
            }
        }
    }
    found
}

#[test]
fn the_library_never_reads_stdin_exits_prints_or_handles_signals() {
    let modules = library_modules();
    assert!(
        modules.len() >= 10,
        "found only {modules:?} in src/lib.rs; did its layout change?"
    );
    let mut found = violations("lib.rs", &src("lib.rs"));
    for module in &modules {
        let file = format!("{module}.rs");
        found.extend(violations(&file, &src(&file)));
    }
    assert!(
        found.is_empty(),
        "the library must leave the terminal and the process to the CLI:\n{}",
        found.join("\n")
    );
}

/// The prompts read stdin by design, so they stay out of the library.
#[test]
fn setup_and_its_prompts_are_the_binarys_only() {
    assert!(!library_modules().iter().any(|m| m == "setup"));
    assert!(src("main.rs").lines().any(|l| l.trim() == "mod setup;"));
    assert!(!violations("setup.rs", &src("setup.rs")).is_empty());
}

/// The check itself: each kind of use is caught, and a comment is not.
#[test]
fn the_check_catches_each_kind_of_use() {
    for (needle, _) in FORBIDDEN {
        let line = format!("    let _ = {needle};");
        assert!(!violations("x.rs", &line).is_empty(), "{needle}");
    }
    assert!(violations("x.rs", "    // println! in a comment").is_empty());
    assert!(violations("x.rs", "    cmd.stdin(Stdio::null());").is_empty());
}
