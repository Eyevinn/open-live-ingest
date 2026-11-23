## Project Overview
A Cargo workspace of three packages sharing one version and one `Cargo.lock`. The root package
`open-live-ingest` is a library and a thin CLI binary over it; `app/` (`open-live-ingest-app`) is
the desktop app for macOS and Windows, a second front end over the same library (ADR-001); `mobile/`
(`open-live-ingest-mobile`) is the iPhone app's Rust core, a third (ADR-002). The
gateway drives a Strom instance on the same machine and registers sources with Open Live. It owns
no media pipeline; Strom does the media. The iOS app (#133, ADR-002) links the same library with no
Strom: its own Swift code does the media there, behind the `Media` seam (`src/media.rs`).

- `src/lib.rs`: the library. It never prints, reads stdin, calls `std::process::exit` or
  handles a signal (`tests/library_boundary.rs` checks this); progress is `gateway::Event`s.
- `src/main.rs`: the CLI. clap, tracing init, signals, rendering events and reports as text.
- `src/gateway.rs`: the events and the sink they go to.
- `src/config.rs`: settings, validation, the two SRT URIs of a link.
- `src/flow.rs`: the flow template and flow ownership by derived id.
- `src/strom.rs`, `src/openlive.rs`: the two HTTP clients.
- `src/heartbeat.rs`: the status heartbeat to Open Live, one outbound WebSocket.
- `src/osc.rs`: Open Source Cloud sign-in, token refresh, the workspace's instances.
- `src/keychain.rs`: the OS keychain behind a trait (macOS Keychain, Windows Credential Manager;
  an in-memory store for tests and Linux).
- `src/credential.rs`: the OSC credential kept in the keychain, refreshed during a session.
- `src/devices.rs`: from Strom's device list to inputs with names and ports.
- `src/run.rs`: `up`, `down`, `status`, `devices`. `up` is generic over `media::Media`.
- `src/media.rs`: the seam between the session and the media. `StromMedia` (the local Strom, the
  single-instance pidfile, flow ids) on a computer; `ExternalMedia` (start/stop/restart handed to
  the host as requests, state and SRT counters from its reports) on the phone.
- `src/gate.rs`: when an armed session sends: the productions holding a source, and a slot added
  to a live production. Pure.
- `src/setup.rs`: interactive setup. Binary only: it prompts on the terminal.
- `src/local_strom.rs`: adopt a running Strom, or start and later stop a headless one.
- `app/`: the desktop app. `app/src/main.rs` and `window.rs` are the window (tao), the system
  webview (wry) and the macOS menu bar (muda); `app/src/lib.rs` is its window-free core: the `oli`
  custom protocol and its CSP, the navigation filter, the IPC dispatcher, and the model of each
  step of the window (`where_to_send.rs`), tested against fake servers in `testing.rs`, and the
  quit guard (`quit.rs`, with the macOS `applicationShouldTerminate:` and the Windows
  `WM_QUERYENDSESSION` subclass and single-instance mutex under `quit/`). `app/ui/` holds the
  three static page files, compiled in. No npm, no Node, no bundler.
- `mobile/`: the iPhone app's Rust core (docs/specs/ios-app.md, section 8). `mobile/src/lib.rs` is
  the C ABI Swift calls (four `extern "C"` functions, the crate's only `unsafe` code, declared in
  the hand-written `mobile/include/oli.h`); `mobile/src/core.rs` the command dispatcher and view
  model over the desktop app's JSON envelope, tested against fake servers in `testing.rs`;
  `mobile/src/logs.rs` its `app.log`. Its media is Swift's, through `media::ExternalMedia`.
  `mobile/examples/fake_servers.rs` (feature `fake-servers`) runs those fakes as the host process
  the iOS UI tests use.
- `ios/`: the iPhone app's Xcode project (SwiftUI, no Swift package). `Core.swift` wraps the C ABI
  with Codable messages (`Messages.swift`); `MediaController.swift` carries out the core's `media`
  events with the capture, encoder, muxer and SRT classes; `AppModel.swift` and the `*View.swift`
  files are the three steps. The "Build the Rust core" phase runs `ios/build-core.sh`;
  `ios/run-tests.sh` runs the unit and UI tests on a simulator as CI does.
- The terminal crates (`clap`, `dialoguer`, `console`) sit behind the root package's `cli` feature
  (default on, required by the binary). The app and `mobile/` depend on the library with default
  features off.

## Language
- All code, comments, commit messages, PR titles, PR descriptions, and documentation in English.

## Security
- Anonymize sensitive data (IP addresses, hostnames, credentials) in commits, PRs, and docs. Use
  `example.com` or `192.0.2.x`.
- Never commit a settings file. `gateway.toml` and `gateway-*.toml` are ignored for that reason.
- The Open Live credential and the SRT passphrase must never reach the logs or the terminal. Build
  errors from status codes, not from request or response dumps, and print addresses through
  `config::mask_passphrase`.

## Code Style
- No emojis in log macros (`info!`, `debug!`, `trace!`, `warn!`, `error!`).

## Strom Integration
- The library owns no media. On a computer Strom owns the media plane. Before adding anything to
  the gateway, check whether a Strom block already does it: encoder selection, SRT reconnect,
  capture handling, device discovery, and stats all exist there. A Strom bug is fixed in Strom, not
  worked around here. On the phone there is no Strom: the iOS app's Swift code owns capture,
  encode, mux and SRT (ADR-002), and the library reaches it only through `media::ExternalMedia`.
- Strom concepts (flow ids, flows, Strom's device lookup, `local_strom`, the pidfile) stay inside
  `media::StromMedia`; the session code in `run::up` and `media::ExternalMedia` use none of them.
- Flow, block, link, device, and SRT statistics types come from `strom-types`, pinned in `Cargo.toml`
  to the Strom release the venue runs. Bump the tag on purpose when that Strom is upgraded. Block
  definition ids and property names are still Strom's strings, because they live in each block's
  builder in the backend; `GET /api/blocks` on a live Strom is the authority for those.
- Do not link the Strom engine itself. The process boundary is what keeps the gateway out of the
  media path, keeps its build free of GStreamer, and lets Strom and the gateway upgrade separately.
- Flow ids are derived (UUIDv5 over gateway id + input id), never stored. That is what lets `down`
  and `status` find the flows with no local state.
- Do not restart a flow for a transient uplink drop. `builtin.mpegtssrt_output` reconnects itself.

## Invariants
- Open Live being unreachable must never disturb the feed: a warning and a retry on the next tick,
  never a stopped or restarted flow.
- `up` owns what it started. Ctrl-C and SIGTERM remove the flows and sources; SIGHUP does not.
  The app's Stop removes the flows and keeps the sources, for the next Go live to adopt with the
  same id (`run::Sources::Keep`).
- No local state beyond the settings file, two pidfiles (the running `up`, and a Strom it
  started), the one keychain entry an app sign-in keeps the OSC credential in, the desktop app's
  `app.log` (with one rotated copy), and on Windows the app's WebView2 cache folder
  (`%LOCALAPPDATA%\open-live-ingest\webview`). The iPhone app keeps the settings file, its one
  Keychain entry and its own `app.log` with one rotated copy, and no pidfile. Flows are recognised by derived id, sources by the
  gateway's name prefix.
- A Strom already listening is adopted and never stopped. Only a Strom the gateway started is
  stopped, on the way out.

## Build
- `cargo check`, `cargo clippy --all-targets -- -D warnings`, and `cargo test` run anywhere: no
  GStreamer, no device access. Keep it that way. `default-members = ["."]`, so at the root they
  build the root package only.
- The iPhone app's core is built with `-p open-live-ingest-mobile`: its tests run on any OS, and
  the CI `ios` job lints it for `aarch64-apple-ios` and `-sim` and checks the crate count recorded
  in `mobile/Cargo.toml` (update it on purpose with a dependency change).
- The iPhone app's Xcode project is `ios/` (no Swift package, no project generator). Its SRT and
  crypto are `ios/Vendor/SRT.xcframework`, built by `ios/Vendor/build-libsrt.sh` from the pinned,
  digest-checked libsrt and mbedTLS tarballs and never committed; bump a pin on purpose, with
  `ThirdPartyLicenses.swift`. `ios/run-tests.sh` builds it for a device and runs its tests on a
  simulator (macOS only), as the CI `ios` job does.
- The app is built with `-p open-live-ingest-app`. On macOS and Windows that includes the window;
  elsewhere only its core builds (the window needs webkit2gtk there, and Linux is not a target).
  The CI `app` job runs its clippy and tests on macOS and Windows.

## Documentation
- The code is the source of truth. `docs/DESIGN.md` records decisions and reasoning, not
  implementation. Do not narrate past bugs in comments; git history holds that.
- Doc filenames in `docs/` use `UPPER_SNAKE_CASE`. `README.md` stays lowercase. Specs in
  `docs/specs/` and decision records in `docs/decisions/` are exempt and keep their own names
  (`desktop-app.md`, `ADR-001-desktop-app-toolkit.md`).

## Tests
- `tests/golden.rs` pins what `devices`, `status`, `up --test` and `down` print, running the
  binary against a fake Strom and Open Live. Output changes only on purpose, with the expected
  text changed in the same commit.
- A regression test must exercise the code it guards and must fail if the fix is reverted.
- Tests must not require a running Strom or Open Live. The flow builder is pure: assert on the JSON
  it emits.
- State which tests actually ran, and which were skipped.
