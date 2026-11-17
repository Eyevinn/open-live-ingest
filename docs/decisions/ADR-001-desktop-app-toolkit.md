# ADR-001: Desktop app toolkit, and a second front end next to the terminal

**Date**: 2026-11-17
**Status**: Proposed

Spec: [`docs/specs/desktop-app.md`](../specs/desktop-app.md). Epic: #62 (OL-59).

## Context

Open Live Ingest has one front end today, and it is a terminal. `docs/DESIGN.md` §6 states the
decision and the reason: "There is one front end and it is a terminal, because the machine is
reached over SSH." `src/main.rs` says the same ("prompts on a plain terminal, no window, no
full-screen UI"), and the macOS app bundle built by `.github/scripts/build-macos-app.sh` has the
CLI itself as `CFBundleExecutable`. #27 planned for that app to open Terminal.

#62, filed by the product decision owner, reverses this for one audience: someone setting up a
camera at a venue who should never need a terminal. On 2026-11-16 a tester opened the app from the
release workflow, got line-by-line prompts and a log, and read "Starting Strom on port 8080" as the
cause of a failure it did not cause. #62 asks for a window on macOS (Apple Silicon) and Windows
(x86_64): where to send, what to send, Go live / Stop. The CLI stays exactly as it is for SSH and
headless boxes.

Two decisions are needed:

1. Whether a second front end is allowed, and what it may and may not change about the gateway's
   design (process boundary, local state, ownership of what it starts).
2. Which UI toolkit, under the standing dependency rule (CTO, 2026-10-22): an external library only
   when needed, the default is no, and a yes needs a written reason with the transitive count.

Constraints that shape the toolkit choice:

- The gateway is Rust and becomes a library (#71, #72, #73). The app must call it in process; it
  must not scrape the CLI's text (#63).
- Build and CI today have no JavaScript toolchain, no GStreamer and no system libraries:
  `cargo check`, `clippy` and `test` run anywhere (repo `CLAUDE.md`, "Build"). Build tooling runs
  in CI next to signing secrets (#25), so it counts as a dependency too.
- macOS: every Mach-O runs under the hardened runtime with library validation on, ad hoc or
  Developer ID signed, then notarized (`build-macos-app.sh`, `release.yml` job `macos-app`).
  Whatever the toolkit loads must fit that without new hardened-runtime exceptions.
- Quitting while live must ask first and then tear down (#62 AC2). That needs control over the
  platform's quit and session-end paths, which several toolkits do not give.
- The UI is small: one window, a sign-in button, two pickers, a device list with a tick box, an
  audio picker and a name per row, one button, a status line per row, an Advanced section. Text
  fields take an API key and a pasted token, so copy and paste must work with the platform's keys.

### Measured dependency cost

`cargo tree -e normal,build` per target on 2026-11-17, in a scratch crate with only the toolkit as a
dependency. "New" is crate names not already in the gateway's own tree (154 crates on
aarch64-apple-darwin, 157 on x86_64-pc-windows-msvc today).

| Option | Version | macOS arm64: total / new | Windows x64: total / new | Renders with |
|---|---|---|---|---|
| Tauri framework (minimal features: `wry`, `common-controls-v6`, `custom-protocol`) | 2.12.6 | 213 / 133 | 216 / 132 | system webview |
| Tauri framework (default features) | 2.12.6 | 217 / 137 | 220 / 136 | system webview |
| iced | 0.14.0 | 152 / 113 | 159 / 118 | wgpu |
| egui / eframe | 0.33.3 | 133 / 86 | 125 / 79 | OpenGL (glow) |
| **wry + tao + muda** (Tauri's own webview, window and menu layers, without the framework) | 0.57.2 / 0.37.2 / 0.20.0 | **89 / 43** | **82 / 37** | system webview |

The webview is the operating system's: WKWebView on macOS (part of the OS, signed by Apple, its
JIT runs in Apple's WebContent process, so the app needs no JIT entitlement), WebView2 on Windows
(part of Windows 11, which is the AC1b target).

## Decision

### 1. A second front end, with the CLI's rules

The desktop app is a second front end over the same library. It is a peer of the CLI, not a
daemon and not a control API: it runs in the foreground, owns what it starts, and stops it when the
person presses Stop or quits. Everything `docs/DESIGN.md` and the repo `CLAUDE.md` require of the
CLI holds for the app:

- **Strom stays a separate process.** The app links the gateway library, never the Strom engine,
  and starts or adopts Strom through the same `local_strom` code `up` uses. Adopt, never replace.
- **No new local state.** The same settings file (`config::default_path()`), the same two pidfiles.
  The app writes `gateway.pid` while live, so `open-live-ingest status` and `down` see it. The
  only additions are the OS keychain entry from #76 and a log file (both covered in the spec).
- **Open Live being unreachable never disturbs the feed**, in the app as in the CLI.
- **The terminal front end stays the SSH and headless front end.** Nothing in it changes for the
  app beyond what #71 to #73 already change (a library with events, output byte-identical).

`docs/DESIGN.md` §6's sentence is amended to: "There are two front ends: a terminal, because a
venue box is often reached over SSH, and a desktop app for a person at the machine (ADR-001). Both
drive the same library and own what they start." The DESIGN.md edit is made by the first app
sub-issue, not by this spec PR.

### 2. Toolkit: the system webview through wry + tao + muda, without the Tauri framework

The app is a Rust binary that opens one window with **tao** (windowing and the event loop), shows
the UI in the system webview with **wry**, and builds the menu bar with **muda**. These are the
three crates the Tauri framework is built on, maintained by the same organisation (tauri-apps) and
released with it. The Tauri framework itself (`tauri`, `tauri-build`, `tauri-cli`, plugins) is not
used.

The UI is three static files (HTML, CSS, plain ES-module JavaScript) compiled into the binary with
`include_bytes!` and served over a wry custom protocol. No npm, no Node, no bundler, no CDN, no
remote content. JavaScript talks to Rust through wry's IPC handler (one JSON message type each way),
and Rust pushes events to the page with `evaluate_script`. The command and event set is small and
fixed (spec, "App API"), so the dispatcher is a `serde` enum and a `match`, a few hundred lines of
own code.

Reasons, in order:

1. **Dependency cost.** 43 new crates on macOS and 37 on Windows, against 86/79 for eframe and
   133/132 for the Tauri framework. What the framework adds on top of wry/tao/muda is a command
   macro, an ACL and capability system, config codegen at build time, an event bus and a bundler
   CLI. For a fixed set of about fifteen commands none of that is worth 90 crates, and the
   capability system exists to restrict a large plugin surface this app does not have.
2. **The platform does the hard UI parts.** Text rendering, high-DPI, accessibility (VoiceOver,
   Narrator), IME, and copy and paste in the credential fields come from the system webview. egui
   and iced draw their own widgets: usable, but they look like a developer tool to the venue
   operator AC1 is written for, and accessibility depends on accesskit's coverage.
3. **One UI for both operating systems**, which #62 asks for, against two native apps.
4. **Signing stays as it is.** One more Mach-O in `Contents/MacOS`, signed with the hardened
   runtime and the existing camera and microphone entitlements; it loads only system frameworks
   (WebKit, AppKit) and nothing from the bundle's GStreamer. No new entitlement or exception.
   On Windows one more `.exe` in the installer.
5. **No JavaScript toolchain in CI**, so no npm supply chain next to the signing secrets.
6. **The quit paths have to be own code with any toolkit.** Checked in the sources: tao 0.37.2
   implements no `applicationShouldTerminate:` on macOS (only `applicationWillTerminate:`), and on
   Windows it leaves `WM_QUERYENDSESSION` unhandled and calls `std::process::exit(0)` on
   `WM_ENDSESSION` (`platform_impl/windows/event_loop.rs`). The Tauri framework inherits both, so
   it would not save that work. The spec places this code in the app crate, using `objc2` and
   `windows`, which are already in the tree through tao and wry.

Dependency record (CTO rule), for `wry`, `tao` and `muda` together:

1. *What is needed:* a native window, the system webview inside it, a native menu bar (macOS needs
   an Edit menu for Cmd-C/Cmd-V to reach a web text field). A window and a webview cannot be done
   in a small amount of own code: each OS needs a few thousand lines of FFI (AppKit/WebKit,
   Win32/WebView2 COM).
2. *Fraction used:* most of wry's and tao's public surface that matters for one window; muda's
   predefined items and accelerators. The parts of Tauri that would be thin usage are exactly the
   parts left out.
3. *Transitive count and maintenance:* 43 new crates (macOS) / 37 (Windows), measured above.
   tauri-apps releases wry, tao and muda alongside Tauri 2, which is in active development with
   regular releases. Pinned in `Cargo.lock` like everything else; updates are reviewed as diffs.
4. *Sensitive area:* the webview renders only the app's own compiled-in files and never loads a
   remote URL (enforced by a navigation handler and a strict CSP; spec, "Security"). Credentials
   typed into the window cross IPC into Rust and are never echoed back to the page or logged.

Build-time only: none. `tauri-cli`, `tauri-build` and `tauri-bundler` are not used; the macOS
bundle keeps being assembled by `build-macos-app.sh`, and the Windows installer by an NSIS script
(spec, "Packaging").

## Consequences

**Positive**:
- A person who never used a terminal can go live (#62 AC1) on the same gateway code, settings and
  Strom as the CLI.
- The dependency increase is the smallest of the options that give a native-quality UI: about 40
  crates per target, and no JavaScript toolchain.
- One UI codebase for macOS and Windows; UI logic (state per input, error text, quit guard) lives
  in plain Rust and is unit tested with `cargo test`, no webview needed.

**Negative**:
- A few hundred lines of own IPC, asset serving and quit-path code that Tauri would have
  partly provided. It is small, fixed and tested.
- The UI is HTML/CSS/JS, a second language in the repo, without a type checker or linter (no npm).
  It is kept deliberately thin: it renders state and sends commands, and holds no logic.
- The app crate does not build on Linux without webkit2gtk, so it is excluded from the Linux CI
  job and from `default-members` (spec, "Workspace").
- On Windows the app depends on the WebView2 runtime. It ships with Windows 11; the installer
  checks for it and stops with a clear message where it is missing (spec).

**Risks**:
- *A webview loads something it should not.* Detected by a unit test over the navigation and
  protocol handlers (only the app's own scheme is allowed) and the CSP header.
- *tao changes its quit behaviour.* The quit-guard tests on CI (macOS and Windows) drive the
  window close and quit paths; a pinned version and reviewed lockfile diffs catch the change.
- *macOS refuses the new executable under library validation.* The `Check the DMG` step of
  `release.yml` runs the app binary's `--version` from the mounted DMG on every PR that touches
  the build, as it already does for Strom.

## Alternatives Considered

- **Tauri framework (v2).** The issue's suggestion, and the closest fit: same webview, same
  windowing. Rejected on cost: 133 new crates against 43 for its own lower layers, for features
  (ACL, plugin system, codegen, bundler CLI) this app does not need, and it does not solve the quit
  paths either. If the app grows into many windows, plugins or an auto-updater, revisit with a new
  ADR.
- **egui / eframe.** Pure Rust, no webview, no JavaScript, 86/79 new crates. Rejected: draws its
  own widgets with OpenGL (deprecated on macOS) or wgpu (more crates), looks like a tool rather than
  an app for the AC1 audience, and accessibility and IME are its own rather than the platform's.
  Runner-up: the choice if the webview ever turns out not to fit the hardened runtime.
- **iced.** 113/118 new crates, wgpu rendering. Same objections as egui at a higher cost.
- **A native app per OS (SwiftUI + WinUI/C#).** The best platform fit, but two UI codebases in
  two more languages, an FFI layer to the Rust library (C ABI or uniffi), Xcode and .NET toolchains
  in CI, and every UI change made twice. Rejected for a three-step window.
- **Electron.** Bundles Chromium and Node (over 100 MB per platform), needs npm in CI, and would
  have to drive the gateway as a subprocess or through a native module. Rejected.
- **The CLI as a subprocess with a JSON event stream.** Avoids linking the library into a GUI, but
  #63 already decided against scraping the CLI, and it would need a second protocol to keep stable.
  Rejected.
- **No app; better terminal UX (a full-screen TUI, PR #7).** Still a terminal, which is what #62
  removes for this audience. #7 stays useful for SSH.
