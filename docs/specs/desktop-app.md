# Spec: Open Live Ingest desktop app (macOS + Windows)

**Epic**: #62 (OL-59)
**Status**: Proposed
**Date**: 2026-11-17
**Decision record**: [ADR-001](../decisions/ADR-001-desktop-app-toolkit.md) (toolkit, and a second
front end next to the terminal)

## 1. Problem

A person setting up a camera at a venue should be able to go live without a terminal. Today the
only front end is the CLI (`docs/DESIGN.md` §6: "one front end and it is a terminal"), and the
macOS app bundle's `CFBundleExecutable` is that CLI (`.github/scripts/build-macos-app.sh`,
Info.plist). In the 2026-11-16 test (Release run 43865757748) the tester got line-by-line prompts
and a log, and read "Starting Strom on port 8080" as the cause of a failure it did not cause.

#62 asks for one window with three steps (where to send, what to send, Go live / Stop) on macOS
(Apple Silicon) and Windows (x86_64), running the same gateway code, settings file and Strom as
the CLI. The CLI does not change for SSH and headless boxes.

Out of scope (from #62): a live video preview, destinations other than Open Live, Linux desktop,
running as a background service, auto-update.

## 2. Decisions at a glance

| Topic | Decision | Section |
|---|---|---|
| Second front end | Allowed, as a peer of the CLI with the CLI's rules; DESIGN.md §6 amended | ADR-001 |
| Toolkit | System webview via `wry` + `tao` + `muda`, no Tauri framework, no npm | ADR-001, §9 |
| Workspace | Root package keeps lib + CLI bin; new member `app/`; `default-members = ["."]`; terminal crates behind a `cli` feature | §3 |
| Strom | Separate child process, started or adopted by the library's `local_strom`, never linked | §4 |
| Settings | Same file as the CLI; per-input selection in `[[input]]`; OSC credential in the keychain (#76) | §6 |
| Logs | Own file logger, `app.log` + one rotated copy; "Show logs" reveals it; Strom's log stays where it is | §7 |
| Quit while live | Close, Cmd-Q, Dock Quit and logout ask first; Windows shutdown runs the bounded teardown | §8 |
| Errors | One table, failure → one line + action, unit tested | §10 |
| macOS | App becomes `CFBundleExecutable`; CLI stays in `Contents/MacOS` for PATH/SSH; same signing and notarization | §11.1 |
| Windows | NSIS installer (per machine), chains the pinned Strom MSI, Start menu entry; next to the zip | §11.2 |

## 3. Workspace layout

```
Cargo.toml            [package] open-live-ingest (lib + CLI bin)  +  [workspace]
Cargo.lock            one lockfile for both
src/lib.rs            the library (#71): config, devices, flow, heartbeat, local_strom,
                      openlive, osc, run (session), strom, events
src/main.rs           the CLI: clap, tracing init, signals, the event reporter, exit codes
src/setup.rs          CLI-only prompts (dialoguer), behind the `cli` feature (#73)
app/Cargo.toml        [package] open-live-ingest-app, publish = false
app/src/main.rs       window, event loop, menus, IPC dispatcher, quit guard
app/src/model.rs      UI state: inputs, rows, messages. Pure Rust, unit tested
app/src/messages.rs   the failure → message table of §10
app/src/platform/     macos.rs (applicationShouldTerminate, camera/mic authorisation),
                      windows.rs (WM_QUERYENDSESSION subclass, single-instance mutex)
app/ui/               index.html, app.css, app.js (static, compiled in with include_bytes!)
packaging/windows/    open-live-ingest.nsi
```

- **Lib and CLI stay one package.** `cargo binstall`, the release paths
  (`target/<target>/release/open-live-ingest`), `build.rs`, `tests/pins.rs` and the tag check in
  `release.yml` all keep working. Splitting them into two
  crates buys nothing the feature below does not.
- **A `cli` feature (default on)** makes `clap`, `dialoguer` and `console` optional and is
  `required-features` of the bin target. The app depends on
  `open-live-ingest = { path = "..", default-features = false }`, so no terminal crate is linked
  into it. `cargo install` / `binstall` use default features and are unchanged.
- **`[workspace] members = [".", "app"]`, `default-members = ["."]`.** A plain `cargo build`,
  `cargo test` or `cargo clippy --all-targets` at the root builds only the gateway, so the Linux CI
  job and the musl release builds never need webkit2gtk. The app is built with `-p
  open-live-ingest-app` on macOS and Windows only.
- **One version.** `version` moves to `[workspace.package]` and both packages inherit it, so the
  app and the CLI in one release always report the same version. The release tag check reads the
  `open-live-ingest` package's version with `cargo metadata --no-deps` rather than the first
  `^version` line of `Cargo.toml`.
- **CI** (`.github/workflows/ci.yml`) gains an `app` job on `macos-latest` and `windows-latest`:
  `cargo clippy -p open-live-ingest-app --all-targets -- -D warnings` and
  `cargo test -p open-live-ingest-app`. The existing job is unchanged. `cargo fmt --all` covers
  both packages.

## 4. Process model and the Strom boundary

```
 ┌──────────────── Open Live Ingest (app process) ────────────────┐
 │ webview (system)  ⇄ IPC ⇄  app core (Rust)  ──▶  gateway library │──HTTPS / WSS──▶ Open Live
 │  index.html/app.js          model, quit guard     Session, loop  │
 └───────────────────────────────────────────────────│─────────────┘
                                                     │ REST (127.0.0.1)
                                         ┌───────────▼───────────┐
                                         │ Strom (child process) │──MPEG-TS/SRT──▶ cloud Strom
                                         └───────────────────────┘
```

- The app **links the gateway library and never the Strom engine.** It calls `Session::start`
  (#72), which runs the same `local_strom::ensure_running` as `up`: a Strom answering at
  `strom.url` is adopted and never stopped; otherwise the bundled or installed Strom is started
  headless, recorded in `strom.pid`, and stopped after teardown. The `DESIGN.md` §4 reasoning for
  the boundary (no GStreamer in the gateway's build, a gateway crash does not take a camera off
  air, Strom's own UI stays available) holds unchanged.
- The **webview never talks to Strom or Open Live.** It has no network access beyond the app's own
  scheme (§9.3). Every request goes through the app core.
- **Strom's UI is one click away for field debugging**: Advanced → "Open Strom" opens
  `strom.url` in the system browser.
- **The app is the `up` of this machine while it is live.** It writes `gateway.pid` (same
  `config::pidfile()`), so `open-live-ingest status` reports it and `open-live-ingest down` stops
  it: SIGTERM on macOS, which the app handles as Stop without asking; `taskkill /F` on Windows,
  after which `down` removes what is left, as it does for a CLI `up`. The app executable is named
  `open-live-ingest-app` so the existing process-name check (`GATEWAY_COMM` in `src/run.rs`, a
  prefix of the executable's file name) matches it without change.
- **One live session per settings file.** Before Go live the app reads `gateway.pid`; if it names
  a running gateway (a CLI `up`, or another app instance) it does not start, and offers "Stop it"
  (the library's `down`). The CLI `up` gains the same refusal (`already running (pid N); run
  'open-live-ingest down' first`), which today silently runs two loops over the same flows. `up`
  writes `gateway.pid` right after this check, before it starts or asks Strom, and removes it
  however it returns, so a gateway that is still starting counts as running too: a second `up`
  or Go live started meanwhile refuses it, and `down` stops it (a CLI `up` has no signal handler
  until its inputs are live, so SIGTERM ends it at once and `down` removes what it left).
- **A Strom left behind by a crash is reclaimed, not adopted.** If `gateway.pid` names no running
  gateway (starting or live, see above) and `strom.pid` names a live Strom (checked by the
  executable's file name, `strom` or `strom.exe`, never by its path), Go live first stops it with
  `local_strom::reclaim_recorded()`, which, unlike `down`'s `stop_recorded()`, waits until it has
  exited, then starts fresh. Otherwise the next Stop would leave a camera
  open, because an adopted Strom is never stopped. Go live checks `gateway.pid` again after the
  camera check (on macOS that can wait minutes on the prompt), and `reclaim_recorded()` checks it
  once more itself, after it has read `strom.pid`: since `up` writes `gateway.pid` before it starts
  Strom, a gateway that starts at any point during the checks is refused, and its Strom is never
  stopped.
- **Windows: no console window for Strom.** The app is a GUI-subsystem executable
  (`#![windows_subsystem = "windows"]`). A console child started from a GUI process gets a console
  window of its own, so when the app starts Strom the library adds `CREATE_NO_WINDOW` to the
  creation flags. This is a launch option the app sets; the CLI keeps today's flags and console
  behaviour (`DESIGN.md` §6).
- **macOS: the camera prompt names the app.** Launched from Finder, the app is the responsible
  process for the Strom it starts, so TCC asks on behalf of "Open Live Ingest" (Info.plist already
  carries `NSCameraUsageDescription` and `NSMicrophoneUsageDescription`). Before Go live the app
  asks AVFoundation for the authorisation status of video and audio
  (`AVCaptureDevice authorizationStatusForMediaType:`, through `objc2`, already in the tree) and
  requests access when it is not determined, so the prompt comes from the app at a predictable
  moment and a denial becomes the §10 message rather than a Strom error.

### Go live and Stop

```mermaid
sequenceDiagram
    actor P as Person
    participant UI as Webview (app.js)
    participant App as App core (Rust)
    participant Lib as Gateway library
    participant S as Strom (child)
    participant OL as Open Live

    P->>UI: Go live
    UI->>App: {"cmd":"go_live"}
    App->>App: gateway.pid live? camera/mic authorised? (macOS)
    App->>Lib: validate(settings) → problems[]
    alt problems
        App-->>UI: error (one line + action, §10)
    else ok
        App->>Lib: Session::start(settings, inputs)
        Lib->>S: probe strom.url; start headless if unreachable (strom.pid)
        Lib-->>App: Event: Strom starting / ready
        Lib->>OL: register sources (caller mode: port 0 → assigned port)
        Lib->>S: create + start one flow per input (derived flow id)
        Lib-->>App: Event per input: registering → connecting → live
        App-->>UI: {"event":"input", ...} per change, {"event":"stats"} every tick
    end
    P->>UI: Stop (or quit, §8)
    UI->>App: {"cmd":"stop"}
    App->>Lib: SessionHandle::stop()
    Lib->>OL: heartbeat "stopping", then remove sources
    Lib->>S: remove flows; stop the Strom we started
    Lib-->>App: Event: removed … / stopped
    App-->>UI: {"event":"stopped"}
```

## 5. The window

One window, about 720×640, resizable, no other windows. Three sections top to bottom; a section
below an unfinished one is visible but disabled.

1. **Where to send.**
   - "Sign in with Open Source Cloud" (#75, #76). After sign-in: the workspace (#77), and a list of
     that workspace's Open Live instances (#73 `instances`), one picked.
   - "Use a self-hosted Open Live": URL and API key, checked on entry (#73 `check_self_hosted`).
   - Advanced: paste a personal access token (#65 fallback).
   - The choice is saved to the settings file at once (`open_live.url`, `open_live.auth_mode`) and
     shown as "Sending to <instance name> (<workspace>)" with a "Change" link on later launches.
2. **What to send.**
   - One row per video device Strom reports (#73 `devices`), virtual devices hidden unless "Show
     all" is ticked, plus a "Test pattern" row.
   - Per row: a tick box; an audio picker ("None" by default, then every audio source Strom
     reports, #69); the Studio name, shown as the fixed prefix `<gateway.name> — ` followed by an
     editable label prefilled with the device name. Only the label is editable: `down` and the
     source sweep find sources by that prefix (`devices::name_prefix`), so the prefix is never
     the user's to change.
   - An audio source already paired with another ticked camera is shown disabled in the other
     rows' pickers (#64 AC5's rule, enforced again by `validate`).
   - Selection is saved on change (§6).
3. **Go live / Stop.** One button. While live each ticked row shows a state and a one-line reason
   when failed, plus bitrate. A banner above the rows carries session-wide notices (Open Live
   unreachable, Studio cannot see the venue). The rest of the window is read-only while live.

**Advanced** (collapsed): venue name (`gateway.name`), local Strom (URL, manage, binary, API key,
"Open Strom"), uplink (mode, host, public host, port range, latency, passphrase), video (codec,
bitrate, encoder preference), capture (resolution, frame rate), gateway id and token, log level.
Defaults are today's `Config::default()` values; nothing here is needed for AC1.

**Menu bar** (muda). macOS: app menu (About, Quit — routed to the quit guard), Edit (Undo, Redo,
Cut, Copy, Paste, Select All: needed for Cmd-V into the token and key fields), Window, Help (Show
logs, Open Live Ingest on GitHub). Windows: no menu bar; "Show logs" is a link in the window footer.

Per-input states shown: `Starting Strom…` (session-wide), `Registering`, `Connecting`, `Live`,
`Failed: <reason>`, `Stopping`, `Not connected` (a saved device that is not present now; skipped,
the others go live).

## 6. Settings and data model

**Same file, same code.** The app reads and writes `config::default_path()` through the library's
`load` / `validate` / `save` (#73): same TOML, atomic rename, mode 0600 or the user-only ACL on
Windows. `OLI_*` environment overrides apply to the app as to the CLI. A box set up in the app works
with `open-live-ingest status`, `down` and `up` (#62 AC4), and the other way round.

**New: the per-input selection.** #69 (OL-61a) adds "an optional per-input settings entry"; this is
its shape, and the app writes it:

```toml
# One entry per input to send. Without any [[input]], `up` streams every device, as today.
[[input]]
device = "<Strom device id>"     # or "test" for the test pattern
audio  = "<Strom device id>"     # optional: an audio source to pair (#64)
label  = "Stage left"            # optional: the source is "<gateway.name> — <label>";
                                 # defaults to the device name
```

- Matching is by Strom device id, exact. The flow id stays derived from gateway id + device id, so
  changing a label never changes a flow id; it renames the source, and the old name is swept as a
  source of this gateway that is no longer wanted.
- **`up` honours a saved selection.** With `[[input]]` present, `up` sends exactly those inputs, so
  `up` over SSH reproduces what the app sends. `--devices`, `--all` and `--test` override it for that
  run. A file without `[[input]]` behaves exactly as today.
- If #69 lands with a different field layout before the app work starts, the app uses #69's
  layout; the three facts above (device id, optional audio id, optional label) are what the app
  needs.

**OSC credential.** In the OS keychain, not in the file, with the marker #76 defines (#65 AC3). The
app never writes `open_live.api_key` for an OSC sign-in. After a browser sign-in the stored
credential is OSC's refresh token (30 days, renewed at each refresh; #74,
[research](../research/OSC_BROWSER_SIGN_IN.md)), and the one-hour access token is refreshed from it
during a session. A personal access token pasted under Advanced goes to the keychain too.

**Nothing else is stored.** No window size, no "last tab", no recent list. The webview runs
non-persistent (`with_incognito(true)`); on Windows its user-data folder is
`%LOCALAPPDATA%\open-live-ingest\webview` (WebView2 otherwise tries to create one next to the
executable, under `Program Files`, which is not writable). The repo invariant "no local state beyond
the settings file and two pidfiles" gains exactly the keychain entry (#76), the log file (§7) and
the webview's cache folder on Windows; `DESIGN.md` §6 records this when it is amended.

**Version skew.** `Config` is `deny_unknown_fields`, so an older CLI fails on a file a newer app
wrote. The DMG and the installer ship the CLI of the same version next to the app; the error a
stray older CLI prints names the unknown key, which is enough to see the cause.

## 7. Logs

- **Where.** macOS: `~/Library/Logs/open-live-ingest/app.log` (Console.app lists it). Windows:
  `%LOCALAPPDATA%\open-live-ingest\logs\app.log` (logs do not roam). The managed Strom keeps
  logging to `<data_dir>/strom.log` as today (`local_strom.rs`); the app logs that path at start.
- **What.** The library's `tracing` output at `log.level` (default `info`), formatted as the CLI's,
  plus one line per UI command (command name only, never its arguments) and per session event.
  Credentials never reach it: the library already logs through `config::mask_passphrase` and never
  dumps requests; the app adds the rule that IPC payloads are not logged. A unit test runs a
  sign-in, a self-hosted check and a Go live against fakes with known secrets and asserts none of
  them appears in the log.
- **How.** Own code, about 60 lines: a `tracing_subscriber::fmt` writer over a file. At start,
  `app.log` is renamed to `app.log.1` (replacing the previous `.1`) when it exceeds 10 MB, so a
  show's log survives the next launch and disk use is bounded at about 20 MB.
  `tracing-appender` is not added.
- **Show logs.** Help → Show logs (macOS) and a footer link (Windows) reveal `app.log` in Finder
  (`open -R`) or Explorer (`explorer /select,`). Advanced also offers "Show Strom log". A failed
  row's reason has a "Show logs" link when the reason says to look there (§10).

## 8. Quitting while live

The rule is the CLI's (`DESIGN.md` §6): a deliberate stop stops, an accident does not.

| Trigger | Not live | Live |
|---|---|---|
| Window close button, Cmd-W | Quit (single-window app) | Ask: "You are live with N inputs. Stop and quit?" [Stop and quit] [Cancel] |
| Cmd-Q, app menu Quit, Dock → Quit (macOS) | Quit | Same question. `applicationShouldTerminate:` returns `NSTerminateCancel`, shows it, and quits after the teardown |
| Logout / restart (macOS) | Quit | Same question; macOS reports that Open Live Ingest cancelled the logout. A venue does not go off air because someone logged out by accident |
| Logout / shutdown (Windows) | Quit | `WM_QUERYENDSESSION`: `ShutdownBlockReasonCreate("Stopping the live feeds")`, run the bounded teardown (`teardown_within`, the CLI's 4 s deadline), then allow the session to end. Windows does not wait for a dialog here |
| `open-live-ingest down` from a terminal | — | macOS: SIGTERM → Stop without asking, then exit. Windows: `taskkill /F`, and `down` removes the rest |
| Crash, Force Quit, Task Manager end task | — | Flows and sources remain; the next Go live or `down` cleans up (§4, Strom reclaim) |

- **Stop and quit** runs the same teardown as the Stop button (`SessionHandle::stop()`, with
  progress in the rows), then exits. If it has not finished after 15 s, the dialog adds "Quit
  anyway"; whatever is left is swept by the next Go live or `down`, which find flows by derived id
  and sources by name prefix.
- **macOS implementation.** tao 0.37 implements no `applicationShouldTerminate:` (ADR-001). The app
  adds that method to tao's application delegate class at start with the Objective-C runtime
  (`class_addMethod`, through `objc2`). A unit test on the macOS CI job asserts the method is
  present after start. If adding it fails, the app logs it and falls back to running the bounded
  teardown on `applicationWillTerminate:`.
- **Windows implementation.** tao leaves `WM_QUERYENDSESSION` unhandled and calls
  `std::process::exit(0)` on `WM_ENDSESSION`. The app subclasses its own window
  (`SetWindowSubclass`, `windows` crate, already in the tree) and does the teardown in
  `WM_QUERYENDSESSION`, before tao ever sees `WM_ENDSESSION`.
- **Single instance.** macOS: LaunchServices activates the running app. Windows: a named mutex
  `Local\open-live-ingest-app`; a second launch brings the existing window to the front and exits.

## 9. App API (internal: webview ⇄ app core)

The only interface the app adds. It is internal to the app crate and is not a network API: no
port, no HTTP server, so the `/health` and `/ready` rule for services does not apply.

### 9.1 Messages

- Page → core: `{"id": <u32>, "cmd": "<name>", "args": { ... }}` through
  `window.ipc.postMessage`.
- Core → page, reply: `{"id": <u32>, "ok": <value>}` or
  `{"id": <u32>, "error": {"message": "<one line>", "action": "<action id or null>"}}`.
- Core → page, event: `{"event": "<name>", "data": { ... }}`, delivered with
  `evaluate_script("window.oli.receive(<json>)")`.

Commands deserialize into one `serde` enum (`#[serde(tag = "cmd", content = "args")]`); an
unknown command or bad arguments is an error reply, never a panic.

### 9.2 Commands

| `cmd` | `args` | `ok` | Library call |
|---|---|---|---|
| `state` | — | the full view model (where-to-send, rows, live flag, Advanced values) | `load` |
| `osc_sign_in` | — | `{workspace, instances[]}` | `api::osc_sign_in` (#77): `osc::browser_login` (#75), kept in the keychain (#76), then `api::workspace` |
| `osc_paste_token` | `{token}` | `{workspace, instances[]}` | `api::keep_osc_token` (#76), `api::workspace(OscCredential::Keychain)` (#77) |
| `osc_workspaces` | — | `{workspace, instances[]}` | No list API exists (#74, [research](../research/OSC_BROWSER_SIGN_IN.md)): `api::osc_sign_in` again, where the workspace is picked on the OSC consent page and the new sign-in replaces the kept one (#77) |
| `choose_instance` | `{url}` | view model | `save` |
| `self_hosted` | `{url, api_key}` | view model | `check_self_hosted` (#73), `save` |
| `devices` | `{show_all}` | `{video[], audio[]}` | `devices` (#73, audio from #69) |
| `set_inputs` | `{inputs: [{device, audio?, label?}]}` | view model | `validate`, `save` |
| `set_advanced` | the Advanced values | view model, or `problems[]` | `validate` (#73), `save` |
| `go_live` | — | `null` (progress arrives as events) | §4 checks, `Session::start` (#72) |
| `stop` | — | `null` | `SessionHandle::stop` |
| `stop_other` | — | `null` | `down` (another gateway named by `gateway.pid`) |
| `quit` | `{confirmed}` | `null` | quit guard (§8) |
| `show_logs` | `{which: "app" \| "strom"}` | `null` | reveal in Finder/Explorer |
| `open_url` | `{which: "strom" \| "studio" \| "help" \| "privacy" \| "privacy_microphone"}` | `null` | system browser; URLs come from settings, never from the page (`privacy`, `privacy_microphone`: macOS Privacy & Security → Camera or Microphone, for §10's camera/microphone row) |

`open_url` takes a name, not a URL, so the page cannot make the app open an arbitrary address.

### 9.3 Events

| `event` | `data` | From library event (#71/#72) |
|---|---|---|
| `strom` | `{state: "starting" \| "ready" \| "failed", message?}` | Strom starting / ready / failed |
| `input` | `{input, state, message?, action?}` | per-input registering / connecting / live / failed / stopping / stopped / not_connected. Keyed by the input's Studio name, which is what the library's events carry |
| `stats` | `{inputs: [{input, bitrate_kbps, rtt_ms, dropped}]}` | periodic stats (the heartbeat snapshot data) |
| `notice` | `{kind: "open_live_unreachable" \| "heartbeat_refused" \| ..., message, action?}` / `{clear: kind}` | warnings |
| `stopping` | `{done, total}` | teardown progress |
| `stopped` | `{left_behind: [], error}` | teardown finished; anything not removed is named. `error` (`{message, action}`, §10) when the session ended on a failure, else `null` |
| `quit_requested` | `{inputs_live}` | the quit guard wants the §8 question shown |
| `osc_sign_in_page` | `{url}` | the sign-in page was opened in the browser; the window shows its address too, for when no browser opens |
| `where` | step 1's view (as `state`'s `where`) | after `state`, the workspace and instance names looked up with the kept sign-in |

The variant names on the right are #71's; the app maps whatever names #71 merges with. `message`
is always produced by `app/src/messages.rs` (§10), never passed through from an error chain.

### 9.4 Security of the webview

- Content only from the app's own custom protocol `oli`, loaded as `oli://localhost/` (macOS;
  wry maps it to `http://oli.localhost/` on Windows), served from the three compiled-in files. Any
  other path is a 404.
- A navigation handler allows only that origin; every other navigation and every `window.open` is
  refused (links to the outside go through `open_url`).
- `Content-Security-Policy: default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self'
  data:; connect-src 'none'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'` on every
  response.
- Devtools are compiled out of release builds (wry's `devtools` feature is not enabled).
- Secrets go page → core only. Replies and events never contain the API key, token, passphrase or
  gateway token; Advanced shows "set" / "not set" for them.
- Unit tests cover the protocol handler (unknown path → 404, CSP present), the navigation filter,
  and that no reply or event serialises a field holding a secret.

## 10. Failures: one line and what to do (#62 AC3)

Every failure the library reports as an event (#71, #72 AC3 lists them) maps to one row here.
`app/src/messages.rs` holds the table; each row has a unit test that feeds the library error or
event and asserts the message and action. Anything not matched shows "Something went wrong. Show
logs for details." with the Show logs action, and the unmatched case is logged at `warn` so a new
row can be added.

| Failure (where it comes from) | Shown | Action |
|---|---|---|
| Open Live unreachable at Go live (server-info / source list fails; caller mode needs a port) | Can't reach Open Live at `<host>`. Check the internet connection, then press Go live again. | Retry |
| Open Live unreachable while live (registration or heartbeat fails; feed unaffected) | Banner: Open Live can't be reached. Your cameras are still sending; Studio may show this venue as offline. | — (clears itself) |
| OSC sign-in expired or refused (token exchange 401/403, `openlive.rs` `exchange`) | Your Open Source Cloud sign-in has expired. Sign in again to go live. | Sign in |
| OSC credential expires within the session and cannot be refreshed (#76) | Your sign-in ends at `<time>`, during this session. Sign in again, or paste a personal access token under Advanced. | Sign in |
| API key refused (self-hosted, 401/403) | Open Live refused the API key. Check it under Where to send. | Change |
| No free SRT port (no range published and none set; Open Live cannot assign one) | Open Live has no free port for `<input>`. Remove an unused source in Studio, or ask the Open Live admin to add ports. | Open Studio |
| Port held by another gateway's source (`run.rs` clash, not ours) | Port `<n>` is used by the source "`<name>`" in Studio. Remove that source in Studio, then press Go live. | Open Studio |
| Port held by our leftover source in an active production (`Refused`) | "`<name>`" from an earlier session is still in an active production. Deactivate that production in Studio, then press Go live. | Open Studio |
| Camera or microphone access denied (macOS, AVFoundation status) | Open Live Ingest is not allowed to use the camera (microphone). Allow it in System Settings → Privacy & Security, then press Go live. | Open Settings |
| Camera busy or failed to open (flow does not reach running; Strom error) | `<camera>` could not be opened. Close other apps that use it, then press Go live. | Retry |
| Saved device not present | `<device>` is not connected. It is skipped; the others go live. | — |
| No inputs ticked | Tick at least one camera or the test pattern. | — |
| No video devices found | No cameras found. Connect one, or tick Test pattern. | — |
| Audio source paired twice (`validate`, #69) | `<mic>` is already used with `<camera>`. Pick another audio input. | — |
| Strom exited during start-up (#68) | The video engine stopped while starting. Show logs, and send strom.log to support. | Show Strom log |
| Strom did not answer in time (45 s `READY_TIMEOUT`) | The video engine did not start within 45 seconds. Show logs for details. | Show Strom log |
| An adopted Strom wants a key (`NeedsCredential`) | Another Strom on this computer needs a key. Enter it under Advanced → Local Strom. | Advanced |
| Strom not found (no bundled, PATH or MSI Strom) | The video engine is missing. Reinstall Open Live Ingest. | Help |
| Strom's port in use by another program | Port `<port>` is used by another program. Change the local Strom port under Advanced. | Advanced |
| Another gateway already live (`gateway.pid`) | Open Live Ingest is already live from another window or a terminal. | Stop it |
| Input connected but nothing delivered for 30 s (bytes not moving, §7 of DESIGN) | Row stays Connecting, with: Nothing has reached the cloud yet. Check that this network allows outgoing UDP. | — |
| Gateway token refused on the heartbeat | Banner: Studio can't see this venue's status (gateway token refused). Your cameras are still sending. | Advanced |
| Settings file invalid (hand-edited) | The settings file has a problem: `<first problem>`. | Show file |
| WebView2 missing (Windows, installer) | Installer: Microsoft Edge WebView2 is required. Install it from Microsoft, then run this installer again. | — |

## 11. Packaging

### 11.1 macOS: the app replaces the CLI as the bundle's executable

Changes to `.github/scripts/build-macos-app.sh` (after #67, which edits the same signing block):

- A new argument (or `APP_BIN`) names the app binary. It goes to
  `Contents/MacOS/open-live-ingest-app` and becomes `CFBundleExecutable`. The CLI stays at
  `Contents/MacOS/open-live-ingest` for SSH and the PATH symlink, so `local_strom`'s "a `strom`
  next to the running executable" and the bundle detection work for both.
- Info.plist adds `CFBundleIconFile` (`AppIcon`, from a committed `app/icons/AppIcon.icns`),
  `NSHighResolutionCapable` = true and `LSApplicationCategoryType` =
  `public.app-category.video`. `CFBundleIdentifier` stays `com.eyevinn.open-live-ingest`, so camera
  and microphone permissions already granted carry over.
- Signing order: libraries and plugins, `gst-plugin-scanner`, `strom` (entitlements), then the CLI
  `open-live-ingest` as a nested executable (entitlements: it opens no device itself, but over SSH
  it is the process TCC sees), then the bundle with the entitlements, which signs the app binary.
  No new entitlement: WKWebView's JIT runs in Apple's WebContent process. Whatever #67 grants the
  ad hoc build applies to the app binary in the same way.
- `--check` requires all three executables and the entitlements on each.

Changes to `.github/workflows/release.yml`:

- `build`, on `aarch64-apple-darwin` and `x86_64-pc-windows-msvc` only, also runs
  `cargo build --release --locked -p open-live-ingest-app --target <target>` and uploads the binary
  as artifact `app-binary-<target>`. The name must not start with `open-live-ingest-`: `checksums`
  and `release` download `open-live-ingest-*.tar.gz` / `*.zip` by pattern.
- `macos-app` downloads `app-binary-aarch64-apple-darwin` and passes it to the script.
- `Check the DMG` also runs `"$contents/MacOS/open-live-ingest-app" --version` from the mounted
  DMG, so dyld and library validation load the app binary under the hardened runtime on every run.
- The pull request `paths` filter adds `app/**` and `packaging/**`.
- Notarization, stapling, `release-macos` and the "only a notarized DMG is published" rule are
  unchanged: the DMG is the same asset (`open-live-ingest-macos-aarch64.dmg`) with a different main
  executable.

`README.md` and `docs/QUICKSTART.md` get the "Mac: install from the DMG, open the app" path (#27
AC6), with the PATH symlink to `Contents/MacOS/open-live-ingest` for SSH use.

### 11.2 Windows: an NSIS installer next to the zip

**Choice: NSIS.** Compared:

| | NSIS | WiX (MSI) | Inno Setup | MSIX |
|---|---|---|---|---|
| Toolchain in CI | `makensis`, one native tool | .NET SDK + `wix` dotnet tool (NuGet) | `iscc`, one native tool | Windows SDK |
| Chaining the Strom MSI | `ExecWait msiexec` | needs a Burn bundle (second project) | `Exec msiexec` | not possible |
| Unsigned install | works (SmartScreen warns) | works (SmartScreen warns) | works | refuses to install |
| Script size for this app | ~120 lines | ~200 lines + bundle | ~100 lines | — |

NSIS and Inno Setup are close; NSIS wins on being what wry/Tauri apps commonly ship with on
Windows, so WebView2 detection recipes and known issues are well trodden. WiX loses on the .NET and
NuGet supply chain and the Burn bundle; MSIX cannot install unsigned.

`packaging/windows/open-live-ingest.nsi`:

- **Per machine**, `RequestExecutionLevel admin`, into `%ProgramFiles%\Open Live Ingest\`:
  `open-live-ingest-app.exe`, `open-live-ingest.exe` (the CLI, for `status` / `down` from a
  console by full path; PATH is not changed), the licences, an `.ico`. Per machine because the
  Strom MSI it chains is per machine and needs elevation anyway; one prompt instead of two.
- **Strom**: the pinned Strom MSI (`strom-<pin>-windows-x86_64.msi`, the `strom-types` tag) is
  embedded. On install, if `%ProgramFiles%\Strom\bin\strom.exe` is absent it runs
  `msiexec /i … /qn /norestart` and fails the install with the MSI's exit code if that fails. An
  existing Strom is left alone: the same rule `install.ps1` applies by default, and the same place
  `local_strom` already looks (`MSI_STROM`). The workflow downloads the MSI and checks it against
  the SHA-256 digest GitHub records for the asset before `makensis` runs, as `install.ps1` does.
- **WebView2**: checks the Evergreen runtime's registry key (`HKLM` and `HKCU`, `...\EdgeUpdate\
  Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}`, value `pv`). Missing → stop with the §10
  message. No bootstrapper is embedded: Windows 11, the AC1b target, ships the runtime.
- **Start menu** shortcut "Open Live Ingest" with the icon; an uninstaller registered under
  Apps & features. Uninstall removes the app, the CLI and the shortcut; it leaves Strom (its own
  entry in Apps & features) and the user's settings, logs and keychain entry.
- **Icons.** The `.ico` is used for the shortcut and the uninstaller; the window and taskbar icon
  is set at runtime from compiled-in RGBA bytes (tao `Icon::from_rgba`). The `.exe` itself keeps
  the default icon in Explorer: embedding a resource would need `rc.exe` plumbing or a
  `winres`/`embed-resource` dependency, not worth it for v1.

`release.yml`:

- New job `windows-app` (`windows-latest`, `needs: build`): download `app-binary-x86_64-pc-windows-msvc`
  and the CLI zip, download and digest-check the Strom MSI, run `makensis` (from the runner image;
  if absent, `choco install nsis` at a pinned version, recorded in the job), producing
  `open-live-ingest-x86_64-pc-windows-msvc-setup.exe`.
- Smoke test in the same job: silent install (`/S`), check both executables report the release
  version with `--version`, the Start menu shortcut exists and `strom.exe` is at the MSI path;
  silent uninstall; check the app and shortcut are gone.
- `checksums` needs `[build, windows-app]` and adds the pattern `open-live-ingest-*-setup.exe`;
  `release` downloads and attests it with the archives; `release-macos` includes it when it
  rewrites `SHA256SUMS`. The installer is unsigned until a code-signing certificate exists (Open
  question 9); it then gets signed in this job before checksumming.
- The installer is published with the Windows zip, so it follows the same #36 go/no-go that holds
  #43: no separate gate.

## 12. Configuration

No new environment variables. The app honours the existing `OLI_*` overrides (they are applied in
`config::load_or_default`) and `RUST_LOG` through the same `EnvFilter`. Advanced exposes the
existing settings only. No Strom pipeline change: the flow shape is the library's (§4 of
`DESIGN.md`, plus #69's audio pairing). Contribution stays MPEG-TS over SRT (`DESIGN.md` §9); the
EFP sequence-validation rule for ingest designs does not apply, as this epic adds no EFP path.

## 13. Dependencies

| Crate | Where | Decision |
|---|---|---|
| `wry`, `tao`, `muda` | app | **Add.** ADR-001: 43 new crates on macOS, 37 on Windows (measured), no JS toolchain |
| `objc2`, `objc2-foundation`, `objc2-app-kit`, `block2` | app, macOS | Direct use of crates already in the tree through tao/wry; no new crate. AVFoundation status call via `msg_send!` on the class by name, no `objc2-av-foundation`; `block2` for the access request's completion handler |
| `windows` | app, Windows | Already in the tree through tao/wry; direct use for the subclass, mutex and shutdown block reason. Same version as the lockfile resolves |
| `serde`, `serde_json`, `tokio`, `tracing`, `tracing-subscriber`, `anyhow` | app | Already used by the gateway |
| `sha2`, `getrandom` | library | **Added** in #75 for PKCE (S256 challenge, random verifier and state): `sha2` 0.11 is the one new crate (its `digest` 0.11 stack is already in the tree through `sha1`); `getrandom` 0.4 was already in the tree |
| keychain access | library | **Own code** (#76, `src/keychain.rs`): `security-framework` on macOS (+4 crates: it, its `-sys`, `core-foundation`, `core-foundation-sys`), the `windows-sys` already in the tree on Windows (+0). `keyring` 3 would add 5 on each, including a second `windows-sys` |
| `tauri`, `tauri-build`, `tauri-cli`, any `tauri-plugin-*` | — | **Not added** (ADR-001) |
| `tracing-appender` | — | Not added: own rotation, §7 |
| `rfd` (dialogs) | — | Not added: the quit question is in-window HTML |
| `open` / `opener` | — | Not added: `open` / `rundll32 url.dll,FileProtocolHandler` / `xdg-open` via `std::process::Command`, as `osc::open_browser` (#75) does |
| `winres` / `embed-resource` | — | Not added: §11.2 icons |
| npm, Node, a bundler, a UI framework | — | Not added: three static files |

Existing dependencies touched: `clap`, `dialoguer` and `console` become optional behind `cli` (§3),
so the app links none of them.

## 14. Sub-issue breakdown (cut after this spec merges)

Each is machine-completable; "What a human does next" goes in the PR body and closing comment, per
the standing rule. Order: a → (b, c, d, f, g, h in parallel where their dependencies allow) → e →
i.

| # | Title | Depends on | Done when (checkable) |
|---|---|---|---|
| OL-59a | Workspace and app skeleton: `[workspace]`, `default-members`, `cli` feature, `app/` crate with tao/wry/muda window, custom protocol, IPC dispatcher, CSP and navigation guard, `--version`; CI `app` job; amend `DESIGN.md` §6 per ADR-001 | #71 | CI green on Linux (unchanged job), macOS and Windows app jobs; protocol/CSP/navigation/IPC unit tests pass; `cargo tree` counts recorded in the PR |
| OL-59b | Where to send: OSC sign-in, workspace, instance picker, self-hosted URL + key, token paste; saved to settings | #73, #77, a | Model tests against fake OSC and Open Live servers; settings round-trip (app write → CLI `check`) |
| OL-59c | What to send: video and audio device rows, Show all, Test pattern, audio picker with the duplicate guard, labels, `[[input]]` saved; `up` honours `[[input]]`, flags override | #73, #69, a | Model tests; CLI golden tests: no `[[input]]` unchanged, with `[[input]]` exactly those inputs |
| OL-59d | Go live / Stop: session events to rows, stats, banner notices, §10 table with one test per row, `gateway.pid` refusal in app and CLI `up`, Strom reclaim, macOS camera/mic authorisation, Windows `CREATE_NO_WINDOW` launch option | #72, #68, a | Every §10 row has a passing test; CLI golden output unchanged except the new refusal |
| OL-59e | Quit while live: close, Cmd-Q, Dock Quit, logout (`applicationShouldTerminate:`), Windows `WM_QUERYENDSESSION`, SIGTERM as Stop, "Quit anyway" after 15 s, Windows single instance | d | Quit-guard model tests; macOS CI test that the delegate method is installed; Windows CI test that the subclass handles `WM_QUERYENDSESSION` with a fake session |
| OL-59f | Logs: file writer with rotation, Show logs / Show Strom log, secret-free log test | a | Rotation and no-secrets tests pass |
| OL-59g | macOS packaging: `build-macos-app.sh` app binary, Info.plist keys, icon, signing order, `--check`; `release.yml` artifact and DMG check; README/QUICKSTART | a, #67 | `release.yml` PR run passes `Check the DMG`, including the app's `--version` |
| OL-59h | Windows installer: NSIS script, Strom MSI chaining with digest check, WebView2 check, shortcut, uninstaller; `release.yml` `windows-app` job, checksums, attestation | a | `release.yml` PR run builds the installer and passes the install/uninstall smoke test |
| OL-59i | Agent-run end to end: on a macOS runner, from the PR's DMG artifact, drive the app core (the same commands the page sends, through the model, no webview) with the test pattern against the `openlivedev` instance: Go live → source in Open Live → Stop → source and flow gone → Go live again with no 409 → quit while live removes them (#62 AC2). Same on Windows once #36 is a go | b–h | Results posted on #62 with run links |

What a human does next, for #62's closing comment: on an Apple Silicon Mac, install the notarized
DMG (needs #25), sign in, pick an instance, tick a camera with its microphone, Go live, and time it
(#62 AC1, target under 5 minutes). The same on Windows 11 x86_64 with the installer once #36 is a
go (AC1b). Expected: the camera with sound as a source in Studio, no terminal opened.

## 15. Open questions (resolved, with the decision)

1. **Toolkit?** wry + tao + muda, no Tauri framework (ADR-001). Tauri framework, egui, iced,
   native per OS and Electron considered and rejected with measured costs.
2. **One crate or a workspace?** A workspace with two packages: the root package keeps lib + CLI,
   `app/` is the second member, `default-members = ["."]`; terminal crates behind a `cli` feature
   (§3). Nothing about the CLI's build or release paths moves.
3. **Where does the app's per-input selection live, and does the CLI read it?** `[[input]]` in the
   same settings file (§6), and yes, `up` honours it with flags overriding. A file without it is
   unchanged.
4. **Can the person rename the Studio source freely?** No: only the label after
   `<gateway.name> — `, because cleanup finds sources by that prefix (§5).
5. **What happens when the app and a CLI `up` both try to be live?** Neither starts a second loop:
   the app refuses and offers to stop the other, and `up` gets the same refusal (§4).
6. **What if the app crashed and left Strom running?** The next Go live stops the Strom recorded in
   `strom.pid`, then starts fresh; `down` does the same today (§4).
7. **Quit while live on macOS logout and Windows shutdown?** macOS asks (and cancels the logout);
   Windows runs the 4 s bounded teardown, since it does not wait for a dialog (§8).
8. **Windows installer?** NSIS, per machine, chaining the pinned Strom MSI when Strom is absent,
   next to the zip, gated by #36 like the zip (§11.2).
9. **Windows code signing?** **Needs a human decision; not blocking any sub-issue.** The installer
   and executables ship unsigned at first, so SmartScreen shows "Windows protected your PC" (More
   info → Run anyway), which will cost time in AC1b's 5-minute target.
   - *Needed now:* a choice of Authenticode signing for Eyevinn and the credential for CI.
     Recommendation: Azure Trusted Signing (managed certificate, no hardware token, signs from
     GitHub Actions with a service principal), as one secret set like #25's.
   - *Cause category:* credential and account no agent holds (and a recurring cost).
   - *Prevent next time:* create the Windows signing secret alongside Apple's in the same request
     whenever a desktop deliverable is planned.
   - *Owner of that change:* the Eyevinn account owner who holds #25.
   OL-59h ships the unsigned installer and adds a signing step guarded by the secret's presence, the
   way `macos-app` handles `APPLE_DEVELOPER_ID_P12`, so signing turns on when the secret exists.
10. **Does the app create the Open Live gateway resource (heartbeat) itself?** Not in this epic.
    Gateway id and token stay under Advanced, as `setup` treats them today. Creating it from the
    app is a follow-up issue if Studio's venue view becomes part of the first-run path.
11. **Log location?** Platform log folders, own rotation, Show logs (§7).
12. **Auto-update?** No. A release is downloaded like today; the window's About shows the version
    and links to the releases page.
13. **Minimum OS?** macOS: as the bundle sets today (`LSMinimumSystemVersion`, the build Mac's major
    version, because Homebrew bottles target it). Windows: Windows 11 x86_64 (AC1b); Windows 10
    works when WebView2 is present but is not tested.
14. **Should the app keep running Strom after Stop for a faster next Go live?** No. It stops the
    Strom it started, as `up` does, so no camera stays open with nothing supervising it.
15. **Does OSC offer a browser sign-in the app can use, and can it be refreshed (#74, #65 AC0)?**
    Yes. Authorization code + PKCE with a loopback redirect, the CLI's flow, and
    `/api/connect/token` returns a refresh token and accepts `grant_type=refresh_token`. The app
    registers its own public client id (dynamic client registration, no secret) so the consent page
    names Open Live Ingest, with `cli` as the fallback. There is no API listing a user's workspaces:
    the workspace is picked on the consent page and changing it is a new sign-in. Sources and the
    optional request to the OSC team: [`docs/research/OSC_BROWSER_SIGN_IN.md`](../research/OSC_BROWSER_SIGN_IN.md).

## 16. Risks

- **#69's settings shape lands differently.** §6 says the app follows #69; OL-59c checks.
- **Unsigned or ad hoc builds block AC1.** An ad hoc signed DMG is rejected by Gatekeeper once
  downloaded; AC1 needs #25. Windows SmartScreen without question 9.
- **macOS 27 library validation (#66).** The app binary loads only system frameworks, but #67's
  ad hoc exception must be applied the same way to it; OL-59g depends on #67.
- **Camera busy is not always distinguishable from other open failures.** The §10 line is
  worded so it is true for both ("could not be opened").
- **WebView2 user-data folder.** Left at its default it is created next to the executable in
  `Program Files` and fails. Set explicitly (§6); covered by the installer smoke test launching the
  app once (`--version` does not create a webview, so OL-59h also starts the window for 5 s under
  the smoke test and checks it is still running).
- **A GUI app on Windows does not get console control events.** All stop paths are the ones in §8;
  `DESIGN.md` §6's console-close handling stays the CLI's.
- **Audio drift (#64 AC3).** The audio picker ships after #64's measurement, as #64 requires.
