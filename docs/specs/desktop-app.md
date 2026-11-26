# Spec: Open Live Ingest desktop app (macOS + Windows)

**Epic**: #62 (OL-59)
**Status**: Proposed
**Date**: 2026-11-17
**Decision record**: [ADR-001](../decisions/ADR-001-desktop-app-toolkit.md) (toolkit, and a second
front end next to the terminal); [ADR-003](../decisions/ADR-003-desktop-preview-through-strom.md)
(preview of picture and sound through Strom, #172)
**Amended**: 2026-11-25, #172 (OL-70): picture and sound preview in steps 2 and 3 (§4.1, §5.2,
§9, §10, §12–§16)

## 1. Problem

A person setting up a camera at a venue should be able to go live without a terminal. Today the
only front end is the CLI (`docs/DESIGN.md` §6: "one front end and it is a terminal"), and the
macOS app bundle's `CFBundleExecutable` is that CLI (`.github/scripts/build-macos-app.sh`,
Info.plist). In the 2026-11-16 test (Release run 43865757748) the tester got line-by-line prompts
and a log, and read "Starting Strom on port 8080" as the cause of a failure it did not cause.

#62 asks for one window with three steps (where to send, what to send, Go live / Stop) on macOS
(Apple Silicon) and Windows (x86_64), running the same gateway code, settings file and Strom as
the CLI. The CLI does not change for SSH and headless boxes.

Out of scope (from #62): destinations other than Open Live, Linux desktop, running as a background
service, auto-update.

#62 also left out a live video preview. #172 (OL-70) brings in a **still preview**: for each ticked
row in step 2, and for each input while its uplink runs in step 3, about one picture a second and a
level bar for its microphone, plus a warning when a microphone gives no sound (§4.1, §5.2,
[ADR-003](../decisions/ADR-003-desktop-preview-through-strom.md)). It is for checking that the
right camera and microphone are picked and still delivering, not for watching: smooth video in the
window stays out of scope. The same for the iPhone app is OL-71, not this amendment.

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
| Preview (#172) | Strom's `builtin.thumbnail` and `builtin.meter`: one preview flow per ticked row while step 2 is on screen, the same two blocks as taps inside the uplink flow (app only) while live; pictures as `data:` JPEGs and levels over the existing IPC | ADR-003, §5.2, §12 |
| Strom from step 2 | Held by the app core from the first device list, handed to the session at Go live; the camera is never opened by two flows | §4.1 |

## 3. Workspace layout

```
Cargo.toml            [package] open-live-ingest (lib + CLI bin)  +  [workspace]
Cargo.lock            one lockfile for both
src/lib.rs            the library (#71): config, devices, flow, heartbeat, local_strom,
                      openlive, osc, run (session), strom, events
src/strom_events.rs   Strom's WebSocket /api/ws, read for MeterData only (#172, §5.2)
src/main.rs           the CLI: clap, tracing init, signals, the event reporter, exit codes
src/setup.rs          CLI-only prompts (dialoguer), behind the `cli` feature (#73)
app/Cargo.toml        [package] open-live-ingest-app, publish = false
app/src/main.rs       window, event loop, menus, IPC dispatcher, quit guard
app/src/model.rs      UI state: inputs, rows, messages. Pure Rust, unit tested
app/src/messages.rs   the failure → message table of §10
app/src/status.rs     the status lines while live (§5.1): productions holding each source, link
app/src/preview.rs    the preview controller (§5.2): Strom hold, preview flows, pictures, levels,
                      silence and wrong-camera checks. Pure state machine + I/O shell, unit tested
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
- **Stop keeps the Open Live sources (#115).** A production slot references a source by id, so
  Stop (and quitting while live, §8) removes the flows but leaves each input's source in Open Live,
  marked inactive (`run::Sources::Keep`, `Event::SourceRetained`). The next Go live adopts it by
  name with the same id and port (`plan_port`'s `Keep`, which emits `Event::SourceAdopted` and a
  log line naming the id and port, and `reconcile_sources`), so the slot keeps working without
  picking the source in Studio again. Sources carrying the prefix that are not
  this session's inputs (a renamed label, an input not ticked now) are still swept at Stop, and
  `open-live-ingest down` removes them all. The CLI `up` keeps removing its sources (`DESIGN.md`
  §6).
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
  stopped. With the preview (#172) the same checks and reclaim run when the app first takes its
  Strom hold in step 2, and Go live skips them while it holds one (§4.1).
- **Windows: no console window for Strom.** The app is a GUI-subsystem executable
  (`#![windows_subsystem = "windows"]`). A console child started from a GUI process gets a console
  window of its own, so when the app starts Strom the library adds `CREATE_NO_WINDOW` to the
  creation flags. This is a launch option the app sets; the CLI keeps today's flags and console
  behaviour (`DESIGN.md` §6).
- **macOS: no Dock icon for Strom.** The bundled Strom runs from `Contents/MacOS/strom`, so it
  shares the bundle's Info.plist and bundle identifier, and once GStreamer touches the window
  server it would register as a second foreground "Open Live Ingest" with a Dock icon of its own
  (#188). Info.plist therefore sets `LSUIElement` = true, which keeps Strom (and the CLI) out of
  the Dock, and the app sets `NSApplicationActivationPolicyRegular` before its event loop runs
  (tao's `EventLoopExtMacOS::set_activation_policy`), so the app alone has the Dock icon, the menu
  bar and a Cmd-Tab entry. `build-macos-app.sh --check` fails a bundle without the key.
- **macOS: the camera prompt names the app.** Launched from Finder, the app is the responsible
  process for the Strom it starts, so TCC asks on behalf of "Open Live Ingest" (Info.plist already
  carries `NSCameraUsageDescription` and `NSMicrophoneUsageDescription`). Before Go live the app
  asks AVFoundation for the authorisation status of video and audio
  (`AVCaptureDevice authorizationStatusForMediaType:`, through `objc2`, already in the tree) and
  requests access when it is not determined, so the prompt comes from the app at a predictable
  moment and a denial becomes the §10 message rather than a Strom error. With the preview (#172)
  the same check runs before the first preview flow for a camera (or microphone) is created, so the
  prompt comes when the first row is ticked in step 2, and Go live finds the answer already given.

### Go live and Stop

Go live **arms** (#130, OL-63c): the session registers each input's source, or adopts the one an
earlier Stop kept (same id, #115), and starts no flow. An input's uplink runs only while a
production holding its source is live, and airing stays the director's call in Studio. The
session reads `GET /api/v1/productions` every 2 s while armed (`run::GATE_POLL`; every 1 s while a
production holding one of the sources is `activating`), on a clock of its own apart from the 10 s
tick, and per input:

- a production holding the source is **`active`** (#128's verdict: a production with no sender
  reaches `active` on its own within its activation window, so arming does not deadlock it, and a
  connect into an already active production is clean): the flow is created and started
  (`ProductionLive`), and the input goes starting → waiting → on air as before, the stall detect
  and restart of #127 included;
- none is any more (every one holding it `inactive`, `ended`, `activating` or unknown, or the
  source taken out of it): the flow is removed and the input is `Armed` again (`NoProductionLive`).
  The source stays registered, and the next tick marks it inactive.

Both are idempotent: what the productions ask for is compared with whether the flow runs, so
several productions holding the source start it once and stop it only when none of them is live.
The productions not being readable changes nothing: a running uplink keeps running, an armed one
stays armed, and the next look tries again (logged once, not every look). A start that fails
(Strom refusing the flow) shows the row failed and is retried at every look while the production
is live. Stop leaves Armed: it removes any running flow and keeps each source, as before. A camera
that cannot be opened is therefore found when a production goes live, not at Go live.

**A slot added to a live production** gets no receiver until the production is deactivated and
reactivated (#128). The session tells it from two looks in a row (the production `active` both
times, the source in it only the second time; never at the first look, when a slot in an active
production may well have been there at activation) and names those productions
(`SlotAddedWhileLive`) until they leave `active` or drop the source. The uplink still runs, for
any other production holding the source; the row says the director has to deactivate and
reactivate the production (§10).

Without registration with Open Live (`[open_live] register` off) there are no productions to wait
for, and Go live sends at once, as before (`Session::start_keeping_sources`). Eyevinn/open-live's
`docs/specs/studio-gateways.md` Phase 2 START/STOP, a command from Studio over the gateway's
connection, could later replace the polling.

The CLI's `up` keeps sending from the start. `up --wait-for-production` arms as the app does
(`Session::start_armed` with `Sources::Remove`, so Ctrl-C still removes its sources).

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
        App->>Lib: Session::start_armed(settings, inputs, credential, Sources::Keep)
        Lib->>S: probe strom.url; start headless if unreachable (strom.pid)
        Lib-->>App: Event: Strom starting / ready
        Lib->>OL: adopt each input's source by name (same id, same port), or register it (caller mode: port 0 → assigned port)
        Lib-->>App: Event per input: registering → armed
        App-->>UI: {"event":"input", state "armed" / "waiting_for_production"}
        loop every 2 s while armed (1 s while activating)
            Lib->>OL: GET /api/v1/productions
            alt a production holding the source is active
                Lib->>S: create + start the input's flow (derived flow id)
                Lib-->>App: ProductionLive, then starting → waiting → on air
            else none holding it is active any more
                Lib->>S: remove the input's flow (the source stays)
                Lib-->>App: NoProductionLive, then armed
            end
        end
        App-->>UI: {"event":"input", ...} per change, {"event":"stats"} every tick
    end
    P->>UI: Stop (or quit, §8)
    UI->>App: {"cmd":"stop"}
    App->>Lib: SessionHandle::stop()
    Lib->>OL: heartbeat "stopping"
    Lib->>S: remove flows (an armed input has none); stop the Strom we started
    Lib->>OL: keep each input's source, marked inactive; sweep other sources of the prefix
    Lib-->>App: Event: removed … / kept … / stopped
    App-->>UI: {"event":"stopped"}
```

### 4.1 Strom from step 2 to Go live (#172)

Before #172, the `devices` command started a managed Strom only to list the devices and stopped it
again before replying (`api::devices` → `run::devices`, whose `LocalStrom` is dropped on return).
The preview needs Strom running while step 2 is on screen, and a camera must never be opened by two
flows at once: Media Foundation gives a camera to one user at a time, and on macOS strom#1278 shows
that even a stop and start of one flow can open the wrong device. So the app core holds the Strom
from step 2 and hands that same Strom to the session at Go live.

- **The hold.** The library gains `api::StromHold`: a `LocalStrom` (Managed or Adopted), its
  `StromClient`, its version, and whether it has the two preview blocks (§5.2, "Older Strom"). It
  is taken by `api::hold_strom(settings, events)`, used by `api::devices_with(&hold)`, and given up
  by `StromHold::release(self)`. The first `devices` command takes it; later ones reuse it. It lives
  until Go live (handed over), quit (released), or a change of `strom.url`, `strom.api_key` or
  `strom.manage` (released; the next `devices` takes a new one). A Strom with no flows opens no
  camera (device discovery does not open the devices), so holding it costs an idle process, never a
  camera light. `api::devices` (start, list, stop) stays for the CLI.
- **The reclaim moves to the hold.** Taking the hold runs §4's checks first: `gateway.pid` names no
  running gateway and `strom.pid` names a live Strom → `local_strom::reclaim_recorded()`, which
  checks `gateway.pid` once more itself. A Go live while the app holds a Strom skips the reclaim:
  the Strom `strom.pid` names is then the app's own (Managed), or the hold is Adopted from a Strom
  that was running and was not left behind by a crash. Without this, Go live's reclaim would kill
  the app's own Strom. A Go live with no hold (`--ipc-stdio` without a `devices` first, or a hold
  released by a settings change) reclaims as before.
- **Go live hands the hold over, in this order** (diagram below):
  1. The preview controller removes every preview flow (`DELETE /api/flows/{id}`) and polls
     `GET /api/flows/{id}` every 100 ms until it answers 404, at most 5 s per flow, all flows in
     parallel. Each row's picture goes to `camera_off`. A flow still present after 5 s is logged
     and left to the session's `prepare`, which removes every flow holding one of its cameras, and
     (new) every preview flow of this gateway (§5.2), before any uplink is created.
  2. §4's checks (`gateway.pid`, camera and microphone authorisation), then the session is started
     with the hold: `Session::start_armed` and `Session::start_keeping_sources` gain siblings that
     take a `Launch { strom: Option<StromHold>, taps: Taps }`: `Session::launch_armed` and
     `Session::launch_keeping_sources` (OL-70b).
     `StromMedia::start` writes `gateway.pid` as today and then uses the held Strom instead of
     calling `connect_strom`. From here the session owns it: when the session ends, its drop stops a
     Managed Strom after the teardown, exactly as for a Strom it started itself (§15 Q14).
  3. When a production holding a source goes `active`, the uplink flow is created with the taps
     inline (`Taps::On`, §12), and step 3 reads its picture and level. While armed no flow runs, so
     the row has no picture and its camera is off (#130).
- **The meter WebSocket carries on.** It belongs to the app core, not to a flow (§5.2). The Strom
  process is the same before and after the handover, so the connection stays open; only the flow
  ids it listens for change.
- **Stop does not hand the Strom back.** The session stops it after the teardown, as today (§15
  Q14). Back on step 2 with rows ticked, the next `devices` or `preview` takes a fresh hold, which
  costs a Strom start (a few seconds). Question 19.
- **Quit while not live** removes the preview flows and releases the hold within the CLI's 4 s
  bound (§8). `StromHold::release` stops a Managed Strom only when Strom runs no flow but this app's
  preview flows and `gateway.pid` names no other gateway. Otherwise (a CLI `up` adopted the app's
  Strom meanwhile) it leaves Strom running, keeps `strom.pid`, and logs it; `down` stops it as it
  stops any recorded Strom. An Adopted Strom is never stopped, as before.
- **Another gateway goes live while the app holds Strom.** A CLI `up` adopts the running Strom, and
  its `prepare` removes any flow holding one of its cameras, the app's preview flows included. The
  preview controller reads `run::running_gateway()` at every reconcile (2 s); while it names another
  process the app removes its own preview flows, creates none, and shows the
  `preview_off_other_gateway` notice (§10). Go live's refusal (§4) is unchanged.

```mermaid
sequenceDiagram
    actor P as Person
    participant UI as Webview (app.js)
    participant App as App core (preview controller)
    participant Lib as Gateway library
    participant S as Strom (child)
    participant OL as Open Live

    Note over UI,S: Step 2: what to send
    UI->>App: {"cmd":"devices","args":{"show_all":false}}
    App->>Lib: §4 reclaim checks, then api::hold_strom(settings)
    Lib->>S: probe strom.url; start headless if unreachable (strom.pid)
    Lib->>S: GET /api/blocks (builtin.thumbnail and builtin.meter present?)
    App->>Lib: api::devices_with(&hold)
    App-->>UI: ok {video[], audio[]}
    App->>S: WebSocket /api/ws (Authorization: Bearer, when a key is set), kept open
    UI->>App: {"cmd":"preview","args":{"on":true}}
    App->>App: macOS: camera/mic authorised? (ask if not determined)
    loop each ticked row
        App->>S: POST /api/flows (preview flow, id from {gateway}/preview/{input}), start
        App->>S: GET /api/flows/{id}/elements/capture:videosrc/properties (strom#1278 check)
    end
    loop per row: every 250 ms until the first picture, then every 1 s
        App->>S: GET /api/flows/{id}/blocks/picture/thumbnail
        S-->>App: 200 image/jpeg, or 504 (no fresh frame yet), or 404 (flow gone)
        App-->>UI: {"event":"preview","data":{"device","state","image":"data:image/jpeg;base64,…"}}
    end
    S--)App: MeterData {flow_id, element_id "sound", rms[], peak[]} every 100 ms
    App-->>UI: {"event":"level"} at 4 Hz; notice no_sound after 5 s below -60 dBFS

    Note over P,OL: Go live: the handover
    P->>UI: Go live
    UI->>App: {"cmd":"go_live"}
    par each preview flow
        App->>S: DELETE /api/flows/{preview id}
        App->>S: GET /api/flows/{preview id} every 100 ms until 404 (at most 5 s)
    end
    App-->>UI: {"event":"preview"} camera_off per row
    App->>App: gateway.pid live? authorisation; no reclaim (hold present)
    App->>Lib: start armed with Launch {strom: hold, taps: On}, Sources::Keep
    Lib->>Lib: write gateway.pid; StromMedia over the held Strom (owned by the session now)
    Lib->>OL: adopt or register each input's source
    Lib-->>App: Armed per input (no flow, camera off)

    Note over P,OL: A production holding the source goes active
    loop every 2 s while armed (1 s while activating)
        Lib->>OL: GET /api/v1/productions
    end
    Lib->>S: prepare (removes any flow holding the camera, and our preview flows)
    Lib->>S: create + start the uplink flow with the picture and sound taps
    Lib-->>App: ProductionLive, starting → waiting → on air; stats name the flow id
    App->>S: GET /api/flows/{uplink id}/elements/capture:videosrc/properties (strom#1278 check)
    loop every 1 s per running uplink, while the window is visible
        App->>S: GET /api/flows/{uplink id}/blocks/picture/thumbnail
        App-->>UI: {"event":"preview"} in step 3
    end
    S--)App: MeterData for the uplink flow, same WebSocket
    App-->>UI: {"event":"level"} at 4 Hz
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
   - Per ticked row, a small picture of the camera (or the test pattern) and a level bar for its
     microphone, with a warning when the microphone gives no sound (§5.2, #172).
3. **Go live / Stop.** One button. While live each ticked row shows a state and a one-line reason
   when failed, plus bitrate, and under it the status lines of §5.1. A banner above the rows
   carries session-wide notices (Open Live unreachable, Studio cannot see the venue). The rest of
   the window is read-only while live. While an input's uplink runs, its row shows the same
   picture and level as in step 2, taken from the uplink flow itself (§5.2).

**Advanced** (collapsed): venue name (`gateway.name`), local Strom (URL, manage, binary, API key,
"Open Strom"), uplink (mode, host, public host, port range, latency, passphrase), video (codec,
bitrate, encoder preference), capture (resolution, frame rate), gateway id and token, log level.
Defaults are today's `Config::default()` values; nothing here is needed for AC1.

**Menu bar** (muda). macOS: app menu (About, Quit — routed to the quit guard), Edit (Undo, Redo,
Cut, Copy, Paste, Select All: needed for Cmd-V into the token and key fields), Window, Help (Show
logs, Open Live Ingest on GitHub). Windows: no menu bar; "Show logs" is a link in the window footer.

Per-input states shown: `Starting Strom…` (session-wide), `Registering`, `Armed` (registered,
sending nothing: no production holds the source, or the productions are not known yet),
`Waiting for production` (armed, and productions hold the source, none of them live yet),
`Connecting`, `Live`, `Failed: <reason>`, `Stopping`, `Not connected` (a saved device that is not
present now; skipped, the others go live). Armed and Waiting for production come from the
session's `Armed` state and the §5.1 poll's productions; a production going live turns the row to
Connecting through the session's own states (§4, Go live and Stop). The button stays Go live /
Stop: Stop also ends an armed session. An armed row is not counted as sending by the quit
question (§8), which then reads "You are going live".

### 5.1 Status lines while live (#126 AC4, #129)

Open Live only receives an input once a production holding its source is activated, so each row
says what the uplink depends on. Two lines under each row, refreshed every 4 s (`status::POLL`,
under the 5 s #126 AC4 asks for) by a poller of their own, apart from the session's 10 s tick:

- **Production.** The productions on the instance (`GET /api/v1/productions`) whose
  `sources[].sourceId` is the input's source id, each with its name, its state (`inactive`,
  `activating`, `active`, `ended`; a state the app does not know shows as `unknown`) and the slot
  (`sources[].mixerInput`, `video_in_<n>`) or slots that hold the source:
  `Production: Morning Show (active), slot video_in_1`. Several are listed in Open Live's order,
  separated by `;`. None holding it: `Production: none holds this source`. When Open Live does
  not answer, refuses, or the source is not registered yet: `Production: unknown`. A failed poll
  never stops or changes the feed, and is logged at `debug` only.
  The production routes return CouchDB documents, so a production's id is `_id` (as Studio reads
  it); the client also takes `id`, which is what the OpenAPI document names.
- **Link.** From the local Strom's `GET /api/flows/<flow id>/srt-stats` for the input's flow (the
  sending element's first peer, as the session reads it):
  - `not sending`: no flow, or no SRT peer;
  - `connecting`: a peer, but `packets_sent` did not grow since the last poll (the first poll of a
    peer, too, since there is nothing to compare with);
  - `connected`: `packets_sent` grew; with the share dropped over that interval,
    `Δpackets_sent_dropped / Δpackets_sent`, as a percentage;
  - `unknown`: the local Strom did not answer.

  With the smoothed RTT whenever there is a peer: `Link: connected, 0.2 % dropped, RTT 31 ms`.

The input's flow id and source id come from the session's `stats` (`gateway::InputStats`), so
nothing is polled before the first tick names them. Stop ends the poller with the session; the
lines go when the page hears `stopped`. Stop keeps each input's source, marked inactive (§4,
#115), so the next Go live adopts it with the same id, and the Production line names the same
productions and slots again from the first tick; the source being inactive in between does not
change which productions hold it.

**Waiting names what is missing** (#116 item 1). A row whose flow waits for a receiver
(`InputState::Waiting`) is `Connecting`; when the last poll tells why, its line says so (§10):
no production holds the source, or every production holding it is `inactive` or `ended`. That
replaces the 30 s "Nothing has reached the cloud yet" line, which is about the network. With a
holding production `activating` or `active`, or the productions unknown, the row keeps the
ordinary wording. Starting and Unreachable (lost contact with Strom) are not waiting for Open
Live and never get the hint.

The library also has `OpenLiveClient::assign_source` (`POST /api/v1/productions/:id/sources`
with `{sourceId, mixerInput}`, Open Live's `SourceAssignmentInput`; it replaces whatever that slot
held). The window does not offer it yet: the slot is chosen in Studio.

### 5.2 Preview: picture and sound (#172, OL-70)

**What the row shows.**

- **Step 2**, each ticked row: a 160×90 picture (the 320×180 JPEG Strom makes, so it is sharp on a
  Retina display), replaced about once a second, and one horizontal level bar per microphone
  channel from -60 to 0 dBFS (peak, with the RMS as a darker inner bar). A row with audio "None"
  shows "No microphone" instead of a bar. The test pattern row shows the bars and the 1 kHz tone.
- **Step 3**, each input whose uplink runs (Connecting, Live): the same, from the uplink flow. An
  input that is Armed or Waiting for production has no flow and its camera is off (#130): the
  picture area says "Camera off until a production goes live" and there is no bar.
- **No picture**: no fresh frame for 5 s while the flow runs → the last picture dimmed, with "No
  picture from `<camera>`".
- **No sound**: §10's `no_sound` line on the row ("No sound from `<microphone>`.").
- **Wrong camera** (strom#1278): §10's `wrong_camera` line on the row.

**When it runs.** The preview controller (`app/src/preview.rs`) runs preview flows for exactly the
ticked rows of the saved `[[input]]` selection (§6) while all of these hold: the page asked for it
(`preview {on: true}`, sent while step 2 is shown and enabled), the window is not minimised or fully
occluded (tao's `WindowEvent::Occluded` / minimised state; the page's `visibilitychange` sends
`preview {on: false}` as well), the app is not live, it holds a Strom (§4.1), that Strom has the
blocks ("Older Strom" below), and `gateway.pid` names no other gateway. Otherwise it runs none, so
no camera light stays on with nothing shown. It reconciles on every command that changes one of
these (`preview`, `set_inputs`, `go_live`, `stop`) and every 2 s:

| Change | Preview flow |
|---|---|
| Row ticked | created and started |
| Row unticked, or preview off | removed; the row is `stopped` when Strom answers 404 (camera off). This holds for a row that is `failed` because its flow went away too (Strom may still run it, #190); a row whose flow did not come up has none left (it was removed then) and is `stopped` at once |
| Audio pick changed, or capture resolution / frame rate changed | removed, then created |
| Label changed | nothing (the flow id does not depend on it) |
| Flow gone while the controller thinks it runs: a thumbnail 404, and `GET /api/flows/{id}` 404 too | `failed`, created again at the next reconcile unless another gateway runs |
| Thumbnail 404 while Strom still lists the flow | nothing: no fresh frame, as a 504 (below). The flow is not removed or created again |

While live, the controller polls the taps of every running uplink flow (flow id from the session's
`stats`, `gateway::InputStats`, as §5.1 does) while the window is visible, regardless of the page's
preview flag. When the window is minimised it stops polling pictures, so the tap detaches after
Strom's 10 s idle timeout and costs nothing; the meter and the silence detector keep running so a
warning is there when the window comes back.

**Preview flow.** One Strom flow per ticked row, shape in §12. Its id is
`flow::preview_flow_id(gateway_id, input_id)`, the v5 UUID of `"{gateway_id}/preview/{input_id}"` in
the same namespace as `flow::flow_id`, so it never equals an uplink flow id, and nothing is stored to
find it again. Its name is `<gateway.name> — <label> (preview)`, so Strom's UI says what it is.
`flow::is_preview_of_ours(flow, gateway_id)` recognises it; the session's `prepare` and `down`
remove preview flows of this gateway along with their own (`prepare` already removes any flow
holding one of its cameras; the test pattern's preview holds none, hence the explicit rule). The
capture block has the same properties as the uplink's (device, audio device, resolution, frame
rate), so the camera is opened in the mode it will be sent in, and what the row shows is what goes
out.

**Pictures.** `StromClient::thumbnail(flow_id, "picture")` →
`GET /api/flows/{id}/blocks/picture/thumbnail` (Strom v0.8.4 `get_block_thumbnail`):

| Strom answers | Meaning | Row |
|---|---|---|
| 200 `image/jpeg` | the latest frame, 320×180, quality 75 (`ThumbnailTapConfig` defaults: one new frame per 1 s, served from cache, stale after 3 s) | `showing`; the event carries the image if its bytes changed |
| 504 | no fresh frame: the branch was just attached, or the source sends nothing | `starting` until the first picture; `no_picture` after 5 s of 504s once a picture was shown, or 5 s after start without one |
| 404 | the flow does not run, or has no such block. Strom also answers it for a flow it has just started, and at times for one `GET /api/flows/{id}` reports Playing (#189) | preview: confirmed with `StromClient::get_flow`. Flow listed: as a 504 (`starting`, then `no_picture` after 5 s), never a restart. Flow not listed (404): `failed`, created again at the next reconcile. No answer: as below. Uplink: nothing (the session's own states say why) |
| no answer | Strom is gone | nothing per row: the `strom` event and §10 handle it |

The first request attaches the tap's branch; it is polled every 250 ms until the first picture
(#172 AC1 asks for one within 3 s of ticking), then every 1 s. Polling stops when the row stops or
the window is hidden; the tap detaches itself after 10 s without requests. The core checks the body
starts with the JPEG marker `FF D8 FF` and is at most 256 KiB, else drops it (logged once at
`debug`), and hands it to the page as `data:image/jpeg;base64,…`, which the CSP already allows
(`img-src 'self' data:`, §9.4). The page never fetches anything.

**Levels.** `strom_events::meter_readings(strom_url, api_key)`: one WebSocket to the local Strom's
`/api/ws` for as long as the app holds or runs a Strom and the preview or a live input wants levels,
with `Authorization: Bearer <strom.api_key>` on the upgrade when a key is set (Strom accepts the
bearer header on every route; the `?auth_token=` form is not used, so the key never sits in a URL).
Strom sends every event to every subscriber (flows, system stats once a second, and so on); the
reader decodes only text frames whose `type` is `MeterData` (`StromEvent` is
`#[serde(tag = "type", content = "data")]`), into its own struct
`MeterReading { flow_id, element_id, rms_db: Vec<Option<f64>>, peak_db: Vec<Option<f64>> }`. It does
not decode `strom_types::StromEvent` as a whole, so a variant another Strom version adds never breaks
the stream. `serde_json` writes the `-inf` of digital silence as `null`, hence `Option`; `None` counts
as -inf. Readings for other flows or other elements than `sound` are dropped. Lost connection:
retried after 1 s, 2 s, then every 5 s; Strom pings every 15 s and tungstenite answers.

The meter block reports every 100 ms (`interval` `"100"`, its default and one of its listed values).
The core keeps, per row and channel, the highest peak and RMS of each 250 ms window and sends them
at 4 Hz, clamped to -90…0 dBFS, all rows in one `level` event.

**Silence.** `SilenceDetector`, per row with a microphone, a pure function of readings and a clock:
silent when every reading's highest channel peak is below -60 dBFS for 5 s, or when no reading at all
arrives for 5 s while the flow runs (a microphone that delivers nothing); it then raises the
`no_sound` notice for the row. It clears at the first 250 ms window whose peak reaches -54 dBFS (6 dB
of hysteresis, so a signal hovering at the threshold does not flicker). The clock starts when the
flow runs. #172 AC3 (warning within 10 s of a muted microphone) is 5 s plus the flow start. The
test pattern never warns.

**Wrong camera (strom#1278).** After each start of a preview or uplink flow for a camera, once it
runs, the app core reads `GET /api/flows/{id}/elements/capture:videosrc/properties`
(`StromClient::element_properties`, already in the library; one retry after 1 s on 404) and compares
`device-name` (both `avfvideosrc` and `mfvideosrc` have it) with the device's name in Strom's device
list. A mismatch is logged at `warn` naming both and raises the row's `wrong_camera` notice, with no
automatic action (question 22). A missing property is logged at `debug` and ignored. For the uplink
the app does the check after the session reports the flow starting; the CLI is unchanged.

**Older Strom.** Taking the hold reads `GET /api/blocks` once (`StromClient::block_ids`). Both
`builtin.thumbnail` and `builtin.meter` must be listed (Strom v0.8.4, the pin, has both). If either
is missing, or the list cannot be read (logged), the app runs no preview flows, sends `preview` →
`{available: false}`, shows the `preview_unavailable` notice once, and the session builds uplink
flows without taps (`Taps::Off`), so going live never depends on the preview.

**Cost** (#172 Q3). While live, the taps are a tee that passes buffers through (`allow-not-linked`,
no work until asked); when polled, one frame a second is scaled to 320×180 and JPEG-encoded, and
`level` reads 48 kHz audio: expected well under 1 % of a core each. The budget OL-70e measures: at
most 5 % of one core per input while live (Strom's CPU with taps polled minus without, 60 s each, test
pattern), with AC5's drop figures unchanged. In step 2 the preview costs the capture itself, up to
about 10 % of a core for a USB camera sending MJPEG at 1080p30, only while step 2 is on screen;
OL-70e records it, with no gate. The WebSocket's other events are a few KB/s, discarded.

**Same on macOS and Windows.** No OS-specific code beyond §4's authorisation check, which runs
before the first preview flow too. Windows cameras cannot be shared, and none is: the handover in
§4.1 never has two flows on one camera. #172 AC4 with a real Windows camera waits for #36; the test
pattern runs there now.

**Never logged.** Images and levels are IPC payloads, which §7 keeps out of the log; the controller
logs state changes only (`preview <device>: showing`).

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

**The preview stores nothing (#172).** No settings key, no file, no migration: what is previewed
follows the saved `[[input]]` selection, and the preview's state lives in the app core's memory
(`app/src/preview.rs`):

```rust
struct Preview {
    wanted: bool,                    // the page's `preview {on}`
    visible: bool,                   // window not minimised or occluded
    hold: Option<StromHold>,         // §4.1; None while live (the session owns the Strom)
    capable: Option<bool>,           // builtin.thumbnail and builtin.meter present; None: not asked yet
    other_gateway: Option<u32>,      // pid from `run::running_gateway()`
    rows: BTreeMap<DeviceKey, Row>,  // Strom device id of the camera, or "test"
}
struct Row {
    flow: FlowId,                    // preview flow id, or the uplink's from `stats` while live
    kind: FlowKind,                  // Preview | Uplink
    microphone: Option<String>,      // the paired audio device's name, for the no-sound line
    picture: Picture,                // Starting{since} | Showing{hash, at} | NoPicture{since} | CameraOff | Failed{reason}
    levels: LevelWindow,             // per channel max peak / RMS of the current 250 ms window
    silence: SilenceDetector,        // §5.2
    camera: CameraCheck,             // Pending | Same | Other{opened}
}
```

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
| Crash, Force Quit, Task Manager end task | — | Flows and sources remain; the next Go live removes the flows and adopts the sources, or `down` removes both (§4, Strom reclaim) |

- **Not live, preview on (#172).** Quit removes the preview flows and releases the Strom hold
  (§4.1) within the CLI's 4 s bound; no question is asked, since nothing is sent. A crash leaves
  the preview flows in a Strom recorded in `strom.pid`: the next hold reclaims that Strom (§4.1),
  and `down` removes this gateway's preview flows with its uplink flows (§5.2).
- **Stop and quit** runs the same teardown as the Stop button (`SessionHandle::stop()`, with
  progress in the rows): the flows are removed and the sources kept (§4), so a relaunch and Go live
  feeds the same production slots. Then it exits. If it has not finished after 15 s, the dialog
  adds "Quit anyway"; flows left behind are removed by the next Go live or `down`, which find them
  by derived id.
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
| `devices` | `{show_all}` | `{video[], audio[]}` | `api::hold_strom` the first time (§4.1, #172), then `api::devices_with(&hold)` (#73, audio from #69). The Strom keeps running for the preview and Go live |
| `set_inputs` | `{inputs: [{device, audio?, label?}]}` | view model | `validate`, `save`; the preview reconciles to the new selection (§5.2) |
| `preview` | `{on: bool}` | `{available: bool}` | Not a library call: the preview controller (§5.2). `on: true` takes the hold if there is none, runs §4's authorisation check, and starts preview flows for the ticked rows; `on: false` removes them. `available` is false when the Strom lacks the blocks, another gateway runs, or the app is live (step 3's taps need no command); the reason arrives as a `notice`. Idempotent. Error reply only for bad arguments, or when no Strom can be held (the §10 Strom rows: not found, did not start, wants a key, port in use). A camera that fails is a `preview` event on its row, never an error reply |
| `set_advanced` | the Advanced values | view model, or `problems[]` | `validate` (#73), `save`. So far only `{uplink: {relay}}` is saved (OL-66): the relay picked, `null` for `default`; anything else replies "cannot do that yet" |
| `relays` | — | `{relays[], chosen, offer}` | `GET /api/v1/server-info` `srt.relays` (OL-65). `offer` is true when Open Live reports more than one relay, or none named `chosen` while it reports others: only then does Advanced show the picker (OL-66) |
| `go_live` | — | `null` (progress arrives as events) | §4.1 handover (remove the preview flows, wait for 404), §4 checks, the session started with the hold and `Taps::On` (#72, #172) |
| `stop` | — | `null` | `SessionHandle::stop` |
| `stop_other` | — | `null` | `down` (another gateway named by `gateway.pid`) |
| `quit` | `{confirmed, anyway?}` | `null` | quit guard (§8): `confirmed: false` asks (the question when live), `true` is Stop and quit, `anyway: true` is Quit anyway once offered. Not live: removes the preview flows and releases the hold (§4.1) |
| `show_logs` | `{which: "app" \| "strom"}` | `null` | reveal in Finder/Explorer |
| `open_url` | `{which: "strom" \| "studio" \| "help" \| "privacy" \| "privacy_microphone" \| "release"}` | `null` | system browser; URLs come from settings, never from the page (`privacy`, `privacy_microphone`: macOS Privacy & Security → Camera or Microphone, for §10's camera/microphone row; `release`: the GitHub page of the newer release the update check found, built by the core from its version number) |

`open_url` takes a name, not a URL, so the page cannot make the app open an arbitrary address.

### 9.3 Events

| `event` | `data` | From library event (#71/#72) |
|---|---|---|
| `strom` | `{state: "starting" \| "ready" \| "failed", message?}` | Strom starting / ready / failed |
| `input` | `{input, state, message?, action?}` | per-input registering / armed / waiting_for_production / connecting / live / failed / stopping / stopped / not_connected. Keyed by the input's Studio name, which is what the library's events carry |
| `uplink` | `{via: "relay", relay, public}` | `BringingUpViaRelay` (OL-66): the session sends through Open Live's SRT relay `relay`, at `public` (`host:port`, or `null` when not reported). Not sent without a relay, so a session against an Open Live without one (v0.8.0) hears what it did before |
| `passphrase` | `{generated: true, kept}` | `PassphraseGenerated`: a passphrase was made for the relay, whose port is public, and saved in the settings file (`kept`), or used for this session only, which is also a `relay_passphrase_not_kept` notice. Never the passphrase itself |
| `summary` | `{inputs: [{input, port, uplink, registered, relay?}]}` | the start summary: `uplink` is where the gateway sends (in caller mode `srt://<host>:<port>?mode=caller&latency=<ms>`; through the relay the dial-in address `srt://<relay>?streamid=<id>&mode=caller&...`, and `relay` names it), `registered` the address registered with Open Live; passphrases masked. The row reads "Sending via relay `<relay>` to `<uplink>`" |
| `stats` | `{inputs: [{input, bitrate_kbps, rtt_ms, dropped}]}` | periodic stats (the heartbeat snapshot data) |
| `status` | `{inputs: [{input, productions: null \| [{name, state, slots: []}], link: {state: "not_sending" \| "connecting" \| "connected" \| "unknown", dropped_pct, rtt_ms}}]}` | the status lines of §5.1, every 4 s while live; not a library event (the app polls Open Live and the local Strom itself). `productions` is `null` when unknown, `[]` when none holds the source |
| `notice` | `{kind: "open_live_unreachable" \| "heartbeat_refused" \| ..., message, action?, device?}` / `{clear: kind, device?}` | warnings. #172 adds the row notices `no_sound`, `wrong_camera` (with `device`) and the window notices `preview_unavailable`, `preview_off_other_gateway` (§10) |
| `preview` | `{device, state: "starting" \| "showing" \| "no_picture" \| "camera_off" \| "failed" \| "stopped", image: "data:image/jpeg;base64,…" \| null, message?, action?}` | not a library event: the preview controller (§5.2). `device` is the camera's Strom device id, or `"test"`, as in `set_inputs`. Sent at every state change, and while `showing` at most once a second per row, only when the picture changed; `image` is `null` when it did not (the page keeps the last one). Typically 10–20 KB per image as base64 |
| `level` | `{levels: [{device, peak_db: [f64], rms_db: [f64]}]}` | not a library event: Strom's `MeterData` for the `sound` block (§5.2). 4 Hz, one event for all rows (so the number of `evaluate_script` calls does not grow with the inputs), one value per channel, -90…0 dBFS. Only rows with a microphone or the test pattern; none while the window is hidden |
| `stopping` | `{done, total}` | teardown progress |
| `stopped` | `{left_behind: [], error}` | teardown finished; anything not removed is named. `error` (`{message, action}`, §10) when the session ended on a failure, else `null` |
| `quit_requested` | `{inputs_live}` | the quit guard wants the §8 question shown |
| `quit_anyway` | `{}` | Stop and quit has run for 15 s: the question adds "Quit anyway" (§8) |
| `osc_sign_in_page` | `{url}` | the sign-in page was opened in the browser; the window shows its address too, for when no browser opens |
| `where` | step 1's view (as `state`'s `where`) | after `state`, the workspace and instance names looked up with the kept sign-in |
| `update` | `{version}` | not a library event: the app's update check (`update::check`, at startup and every 24 h unless `[app] check_for_updates = false`) found a newer release; also in `state` as `update` (`null` when none) |

The variant names on the right are #71's; the app maps whatever names #71 merges with. `message`
is always produced by `app/src/messages.rs` (§10), never passed through from an error chain.

Rates while the preview runs, per row: one `preview` event a second at most, four `level` events a
second in total. `--ipc-stdio` (§9.3a) carries them unchanged, so OL-70e's end to end can assert a
picture (`image` decodes as a JPEG) and a level.

### 9.3a The same API over stdin and stdout (`--ipc-stdio`)

`open-live-ingest-app --ipc-stdio` runs the app core with no window and no webview: each stdin
line is one page message (§9.1), each stdout line one reply or event, the JSON the page would be
handed by `window.oli.receive`. It is the same `ipc::Core` the window drives, so a test of the
release artifact (the DMG's or the installer's executable) exercises the same commands and model
the page does; OL-59i's end to end (`.github/scripts/e2e-app.py`) uses it. The process ends as the
window does: a `quit` with nothing live, Stop and quit once the teardown is done, SIGTERM on macOS
(Stop without asking), or stdin closing (the bounded teardown of §8). It opens no port and gives
nothing a local process could not do with the CLI as the same user; replies and events carry no
secret (§9.4), so stdout can be logged.

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
| Through the SRT relay (OL-66): Open Live reports no relay any more (`connect.reason` `srt-relay-not-configured`, or a 422 with that `code`, or `srt-relay-unknown` naming no relays) | Open Live has no SRT relay to send through any more. Press Go live again to send to it directly. | Retry |
| Through the SRT relay: Open Live has no relay of that name (`srt-relay-unknown`, a 422 with `code` and `relays`, or `uplink.relay` not among the relays) | Open Live has no SRT relay named "`<relay>`". Pick one of "`<a>`", "`<b>`" under Advanced. | Advanced |
| Through the SRT relay: Open Live cannot ask its Strom about the relay (`srt-relay-unreachable`) | Open Live can't reach its SRT relay right now. Wait a moment, then press Go live again. | Retry |
| Through the SRT relay: the relay has no public address (`srt-relay-public-unknown`) | Open Live's SRT relay has no public address to send to. Ask the Open Live admin to set one, then press Go live again. | Retry |
| Through the SRT relay: the passphrase generated for it could not be saved in the settings file | Banner: The passphrase made for the SRT relay could not be saved, so it protects this session only. The next Go live makes a new one. | Show file |
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
| Input waiting for a receiver, and no production holds its source (§5.1) | Row stays Connecting, with: Waiting for Open Live: no production holds this source. Put it in a production in Studio and activate it. | Open Studio |
| Input waiting for a receiver, and every production holding its source is inactive or ended (§5.1) | Row stays Connecting, with: Waiting for Open Live: production "`<name>`" is inactive. Activate it in Studio. (Several: productions "`<a>`", "`<b>`" are inactive. Activate one in Studio.) | Open Studio |
| Armed, and no production holds the input's source (or the productions are unknown) (§4, §5) | Row Armed, with: Ready. Nothing is sent until a production that holds this source is activated in Studio. | Open Studio |
| Armed, and productions hold the source but none is live (§4, §5) | Row Waiting for production, with: Ready. Sending starts when production "`<name>`" is activated in Studio. (Several: when one of productions "`<a>`", "`<b>`" is activated in Studio.) | Open Studio |
| The source was put in a production while that production was live (`SlotAddedWhileLive`, #128) | Row Connecting, with: Production "`<name>`" got this source while live, so Open Live has no receiver for it yet. The director has to deactivate and reactivate the production in Studio; until then it cannot show this input. | Open Studio |
| Gateway token refused on the heartbeat | Banner: Studio can't see this venue's status (gateway token refused). Your cameras are still sending. | Advanced |
| Settings file invalid (hand-edited) | The settings file has a problem: `<first problem>`. | Show file |
| Preview: the camera could not be opened (preview flow does not reach running, #172) | Row picture: `<camera>` could not be opened. Close other apps that use it, then untick and tick it again. | — (untick and tick the row again; `retry` is Go live's action) |
| Preview: no fresh frame for 5 s while the flow runs (§5.2) | Row picture, dimmed: No picture from `<camera>`. | — |
| Preview or live: the microphone gives no sound for 5 s (`no_sound`, §5.2) | Row: No sound from `<microphone>`. Check that it is on and not muted. | — (clears itself) |
| Preview or live: Strom opened another camera than the one picked (`wrong_camera`, strom#1278) | Row: Strom opened "`<opened>`" instead of "`<camera>`". Untick and tick it again (while live: Stop, then Go live). | Show logs |
| Preview: the local Strom has no thumbnail or meter block (`preview_unavailable`) | Banner, once: This video engine has no preview. Pictures and sound levels need Strom 0.8.4 or later; going live is not affected. | — |
| Preview: another gateway is live on this computer (`preview_off_other_gateway`) | Banner: The preview is off while another gateway is live on this computer. | Stop it |
| Preview: camera or microphone access denied (macOS, before the first preview flow) | Open Live Ingest is not allowed to use the camera (microphone). Allow it in System Settings → Privacy & Security, then tick the camera again. | Open Settings |
| WebView2 missing (Windows, installer) | Installer: Microsoft Edge WebView2 is required. Install it from Microsoft, then run this installer again. | — |

## 11. Packaging

### 11.1 macOS: the app replaces the CLI as the bundle's executable

Changes to `.github/scripts/build-macos-app.sh` (after #67, which edits the same signing block):

- A new argument (or `APP_BIN`) names the app binary. It goes to
  `Contents/MacOS/open-live-ingest-app` and becomes `CFBundleExecutable`. The CLI stays at
  `Contents/MacOS/open-live-ingest` for SSH and the PATH symlink, so `local_strom`'s "a `strom`
  next to the running executable" and the bundle detection work for both.
- Info.plist adds `CFBundleIconFile` (`AppIcon`, from a committed `app/icons/AppIcon.icns`),
  `NSHighResolutionCapable` = true, `LSApplicationCategoryType` =
  `public.app-category.video` and `LSUIElement` = true (no Dock icon for Strom, §4; added for
  #188). `CFBundleIdentifier` stays `com.eyevinn.open-live-ingest`, so camera
  and microphone permissions already granted carry over.
- Signing order: libraries and plugins, `gst-plugin-scanner`, `strom` (entitlements), then the CLI
  `open-live-ingest` as a nested executable (entitlements: it opens no device itself, but over SSH
  it is the process TCC sees), then the bundle with the entitlements, which signs the app binary.
  No new entitlement: WKWebView's JIT runs in Apple's WebContent process. Whatever #67 grants the
  ad hoc build applies to the app binary in the same way.
- `--check` requires all three executables, the entitlements on each and `LSUIElement` = true.

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
existing settings only. Contribution stays MPEG-TS over SRT (`DESIGN.md` §9); the EFP
sequence-validation rule for ingest designs does not apply, as this epic adds no EFP path.

**Strom flows (#172).** The flow shape is the library's (§4 of `DESIGN.md`, plus #69's audio
pairing), with two changes for the app only. No setting turns them on or off; the CLI's `up`, the
iPhone app's `ExternalMedia` and every other caller build exactly today's flow (`Taps::Off`).

The **preview flow** (§5.2), for a camera with a microphone, as `strom_types::Flow` JSON. Without a
microphone, `stream_mode` is `"video"`, `audio_device` is absent, and the `sound` block, its sink
and their links are left out. For the test pattern, the capture block is replaced by today's
`testvideo`/`testvideocaps`/`testaudio`/`testaudiocaps` elements, linked `testvideocaps:src →
picture:video_in` and `testaudiocaps:src → sound:audio_in`.

```json
{
  "id": "<v5 UUID of \"{gateway_id}/preview/{input_id}\">",
  "name": "<gateway.name> — <label> (preview)",
  "blocks": [
    { "id": "capture", "block_definition_id": "builtin.local_input",
      "properties": { "stream_mode": "audio_video", "video_device": "<camera id>",
                      "audio_device": "<microphone id>",
                      "video_resolution": "<capture.resolution, when set>",
                      "video_framerate": "<capture.framerate, when set>" } },
    { "id": "picture", "block_definition_id": "builtin.thumbnail", "properties": {} },
    { "id": "sound", "block_definition_id": "builtin.meter",
      "properties": { "interval": "100" } }
  ],
  "elements": [
    { "id": "picture_sink", "element_type": "fakesink",
      "properties": { "sync": false, "async": false } },
    { "id": "sound_sink", "element_type": "fakesink",
      "properties": { "sync": false, "async": false } }
  ],
  "links": [
    { "from": "capture:video_out", "to": "picture:video_in" },
    { "from": "picture:video_out", "to": "picture_sink:sink" },
    { "from": "capture:audio_out", "to": "sound:audio_in" },
    { "from": "sound:audio_out", "to": "sound_sink:sink" }
  ]
}
```

The `fakesink`s end each branch, so the pipeline has a sink to reach PLAYING with; `sync: false`
keeps them from holding buffers to the clock. Block and element ids are the flow's own (Strom names
the elements `picture:tee` and `sound:level`); the thumbnail route takes the block id `picture`, and
`MeterData` carries the block id `sound` as `element_id`.

The **uplink flow with taps** (`Taps::On`, the app's sessions only) is today's flow
(`src/flow.rs`) with the same two blocks inserted, and nothing else changed:

```text
  capture:video_out ──> picture:video_in   picture:video_out ──> encoder:video_in
  capture:audio_out ──> sound:audio_in     sound:audio_out   ──> uplink:audio_in_0
  (test pattern: testvideocaps:src ──> picture:video_in, testaudiocaps:src ──> sound:audio_in)
```

`picture` is a `tee` with `allow-not-linked` and one linked branch, the encoder; the thumbnail
branch Strom attaches on request starts with a leaky one-buffer queue, so it can never hold the
uplink back. `sound` is `level` in line, passing audio through unchanged. An input without a
microphone gets no `sound` block, as it gets no audio track today. `flow::is_ours`,
`capture_device_of` and the derived flow id are unchanged by the taps.

`flow::build` gains a `taps: Taps` argument (`Taps::Off` everywhere today), and `flow::preview`
builds the preview flow. Both are unit tested on their JSON, as `flow::build` is today.

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
| `tokio-tungstenite`, `futures-util` | library | **Already direct** (the heartbeat, `src/heartbeat.rs`). #172's `src/strom_events.rs` also opens a plain `ws://` connection to the local Strom with them: no new crate and no new feature (`connect` covers plain TCP) |
| `base64` | app | **Made direct, +0 crates** (#172): 0.22.1 is already in the lockfile through `reqwest`, `hyper-util` and `strom-types`; the app uses its standard engine's `encode` for the `data:` pictures. Own code would be about 25 lines; using the copy already built and shipped adds no new code to the supply chain. Pinned to `0.22`, the version the tree resolves. Not sensitive: it encodes bytes we pass on, it parses nothing |
| JPEG decoding or an image crate | — | Not added (#172): the core only checks the JPEG marker and the size; the webview decodes the picture |
| A WebSocket client in the page | — | Not added (#172): `connect-src 'none'` stays; levels come over the IPC |
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
| OL-59i | Agent-run end to end: on a macOS runner, from the PR's DMG artifact, drive the app core (the same commands the page sends, through the model, no webview) with the test pattern against the `openlivedev` instance: Go live → source in Open Live → Stop → the flow is gone, the source is kept → Go live again with no 409, reusing the source with the same id (#115) → quit while live removes the flow and keeps the source (#115, which supersedes #62 AC2's removal of the sources). The same run on Windows from the installer, with the test pattern, is `release.yml`'s `e2e-windows` (#145); camera capture there waits for #36 | b–h | Results posted on #62 with run links |

What a human does next, for #62's closing comment: on an Apple Silicon Mac, install the notarized
DMG (needs #25), sign in, pick an instance, tick a camera with its microphone, Go live, and time it
(#62 AC1, target under 5 minutes). The same on Windows 11 x86_64 with the installer once #36 is a
go (AC1b). Expected: the camera with sound as a source in Studio, no terminal opened.

### 14.1 Preview (#172, OL-70)

Cut after this amendment merges; each is machine-completable. Order: a and b in parallel → c → d →
e.

| # | Title | Depends on | Done when (checkable) |
|---|---|---|---|
| OL-70a | Library: `flow::preview` and `Taps` on `flow::build` (§12), `preview_flow_id`, `is_preview_of_ours`, `prepare` and `down` remove preview flows; `StromClient::thumbnail` (200/504/404) and `block_ids` (`GET /api/blocks`); `strom_events::meter_readings` (WS `/api/ws`, `MeterData` only, bearer header, reconnect) | — | Unit tests: flow JSON for camera, camera + mic and test pattern, with and without taps; uplink without taps byte-identical to today's; thumbnail statuses against a fake Strom; meter reader against a fake WS server sending `MeterData` (with `null` values), other event types and an unknown variant; `down` removes a preview flow; CLI golden output unchanged |
| OL-70b | Library: Strom ownership handover. `StromHold`, `hold_strom`, `devices_with`, `release` (stops a Managed Strom only with no foreign flows and no other gateway), session starts taking `Launch {strom, taps}`, `StromMedia` over a held Strom; app: reclaim at hold time, skipped at Go live with a hold (§4.1) | — | Unit tests: a hold handed to a session is stopped by the session's drop and not by the app; `release` leaves a Strom with a foreign flow running and keeps `strom.pid`; Go live with a hold does not reclaim, without one does; existing session and reclaim tests pass |
| OL-70c | App core: preview controller (§5.2, §6 state), `preview` command, `preview` / `level` events, `devices` keeps the hold, Go live handover order, silence detector, wrong-camera check, older-Strom fallback, other-gateway pause, new §10 rows in `messages.rs` | a, b | Model tests with a fake Strom and fake clock: tick → flow created; untick → removed; Go live removes every preview flow and waits for 404 before the session starts; silence at 5 s below -60 dBFS, cleared at -54; `wrong_camera` on a mismatched `device-name`; no blocks → no preview flow and `Taps::Off`; every new §10 row has a test; no event or log line carries an image |
| OL-70d | UI: picture and level bars in step 2 and step 3 rows, "No microphone", "Camera off until a production goes live", notices; `preview {on}` on step 2 shown/hidden and on `visibilitychange` | c | There is no JavaScript test harness (no npm, §13), so: CI app job green; the protocol/CSP tests pass unchanged; a test in the app crate asserts `app/ui/app.js` contains no `fetch`, `WebSocket` or `XMLHttpRequest` (the page still talks only through the IPC); a core test asserts the view model's rows carry what the page renders (picture state, microphone name or none, the armed wording) |
| OL-70e | Agent-run end to end on macOS and Windows runners from the PR's DMG / installer, over `--ipc-stdio`, with the test pattern against `openlivedev`: `preview {on}` → a JPEG `image` within 3 s and a `level` above -60 dBFS; `preview {on: false}` → the preview flow is gone (`GET /api/flows` on the local Strom); Go live → no preview flow remains before the uplink starts; production active → `preview` and `level` from the uplink flow; drop figures as in the existing e2e; Strom CPU with and without taps recorded per input | c (d for nothing: no webview) | Run links and the CPU figures posted on #172; ≤ 5 % of one core per input while live, AC5 drop figures within the existing e2e thresholds |

What a human does next, for #172's closing comment: on a Mac with a camera and a microphone, tick
the camera in step 2 and check the picture appears within 3 s and the level moves when speaking;
mute the microphone and see "No sound from …" within 10 s; untick and check the camera light goes
off; Go live with a production active in Studio and see the same picture and level in step 3
(AC1, AC2, AC3). AC4 with a real Windows camera once #36 is a go.

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

### Preview (#172, OL-70): resolved, with the decision

16. **Does the preview need its own short-lived Strom flow, and does it hand over cleanly at Go
    live on macOS and Windows?** Yes: one preview flow per ticked row, only while step 2 is shown.
    Go live removes them all and waits for 404 before the session starts; the uplink flow opens the
    camera only when a production goes active. No camera is ever open in two flows, which Windows
    would refuse (§4.1).
17. **One preview flow for all rows, or one per row?** One per row: a camera that fails does not
    stop the others' pictures, unticking removes exactly one flow and turns off exactly one camera
    light, and it mirrors the uplink's one flow per input.
18. **Who owns Strom between step 2 and Go live?** The app core, through `StromHold`, from the
    first `devices` until Go live (handed to the session), quit, or a change of the local-Strom
    settings. An idle Strom opens no camera. The §4 reclaim runs when the hold is taken, not at Go
    live (§4.1).
19. **Does Stop hand the Strom back to the preview?** No. The session stops it after the teardown
    as today (question 14), and the preview takes a fresh hold. The cost is a Strom start (a few
    seconds) after Stop; the gain is one ownership path through the session's teardown instead of
    two.
20. **Preview while armed (Go live pressed, no production live)?** No picture and no level: the
    camera stays off until a production holding the source goes live (#130). The row says so.
    Opening the camera for hours while armed would contradict #172 item 3 and keep the light on
    with nothing sent.
21. **What if the window is minimised, or another gateway goes live meanwhile?** Minimised or
    occluded: the preview flows are removed and live pictures are not polled. Another gateway
    (`gateway.pid` names another process): the app removes its preview flows and creates none until
    that gateway is gone, and quitting leaves a Strom that runs flows not ours running (§4.1).
22. **What does the app do when Strom opened another camera (strom#1278)?** It logs both names at
    `warn` and shows the row notice; it does not stop or restart the flow. Stopping an uplink is the
    director's call in Studio, and a restart can hit the same bug; the person sees the wrong picture
    in the row anyway and can Stop. Revisit when strom#1278's AC2 (Strom fails the flow itself)
    ships.
23. **Pictures over the IPC as `data:` URLs, or a dynamic route on the app's `oli` protocol?**
    `data:` over the IPC. The CSP already allows it, the protocol handler stays three compiled-in
    files (§9.4), and the overhead (one 10–20 KB string a second per row) is negligible.
24. **Which level rate, and what reaches the page?** The meter at 100 ms (its default, one of its
    listed values), the core sending the per-window maximum at 4 Hz for all rows in one event.
    Taking the maximum keeps short peaks visible that a 250 ms sample would miss.
25. **Silence threshold?** Highest channel peak below -60 dBFS for 5 s, or no readings for 5 s,
    cleared at -54 dBFS. -60 dBFS is below any working microphone's room noise and above the
    digital silence of a muted one; 5 s does not warn on a pause in speech and meets AC3's 10 s.
26. **Taps in the CLI's `up` too?** No: the CLI has no screen to show them, and Studio shows the
    received picture. `Taps::Off` keeps the CLI's flow exactly as today. A later `status` view could
    turn them on.
27. **A setting to turn the preview off (low-power laptops)?** Not in this version: it runs only
    while step 2 is shown or while live with the window visible, and the live taps are within the
    5 % budget. OL-70e's CPU figures decide; if over budget, a follow-up adds `[app] preview`.
28. **An adopted Strom older than the pin, without the blocks?** `GET /api/blocks` at hold time;
    without both blocks there is no preview, the uplink is built without taps, and one notice says
    why (§5.2). Going live never depends on the preview.
29. **Preview capture mode: the uplink's resolution and frame rate, or a cheap low mode?** The
    uplink's. The camera is then opened in step 2 in the mode it will be sent in, so a mode the
    camera refuses shows up before Go live, and the picture is what goes out. The cost is step 2
    only, and bounded (§5.2, "Cost").

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
- **strom#1278: the wrong camera on a stop and start (#172).** The preview adds a stop and start of
  a camera (preview flow, then uplink flow) to every Go live. Mitigation: the `capture:videosrc`
  check after every start (§5.2), a warning on the row and in the log. Detection: OL-70e on a
  two-camera runner is not available, so the log line is what a field report carries.
- **Camera not released in time at the handover.** AVFoundation or Media Foundation may release a
  device shortly after Strom reports the flow gone. With registration on the uplink starts only
  when a production goes active, normally seconds later; with registration off (Go live sends at
  once) a "camera busy" start is the existing §10 row, retried at every look while live (§4).
  Detection: OL-70e checks no preview flow remains before the uplink starts.
- **A tee in the uplink's encode path.** `picture` puts a tee between capture and encoder. With
  one linked branch it passes buffers through, but a tee answers allocation queries differently
  from a direct link, which could change zero-copy into a hardware encoder. Detection: OL-70e's
  drop figures and CPU with and without taps (AC5); fallback is `Taps::Off` for video only.
- **The thumbnail tap refuses some memory.** Strom's tap refuses GPU memory it cannot download
  before attaching anything; the row then shows no picture while the uplink is unaffected.
  `local_input` delivers system memory today.
- **Strom's WebSocket is not a versioned API.** `MeterData`'s shape may change in a later Strom.
  The reader decodes only that variant into its own struct and ignores what it cannot read (no
  levels, a `debug` log), and the pin moves on purpose (`Cargo.toml`), with OL-70a's fixture taken
  from the pinned `strom-types`.
- **An adopted Strom older than v0.8.4.** No blocks → no preview and no taps (§5.2); covered by a
  unit test with a fake `GET /api/blocks`.
- **A CLI `up` adopting the app's Strom.** Covered by §4.1: the app pauses the preview and never
  stops a Strom that runs flows not its own.
