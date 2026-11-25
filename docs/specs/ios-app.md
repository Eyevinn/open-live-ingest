# Spec: Open Live Ingest for iPhone (iOS)

**Epic**: #133 (OL-64)
**Status**: Accepted
**Date**: 2026-11-22
**Amended**: 2026-11-25, #173 (OL-71): the sound level in steps 2 and 3, the picture being sent in
step 3, and the no-sound warning (§5, "Level and picture")
**Decision record**: [ADR-002](../decisions/ADR-002-ios-app-toolkit.md) (the library on iOS, the
Swift binding, and the media and SRT stack on the phone)
**Model**: [`desktop-app.md`](desktop-app.md) (#62) and
[ADR-001](../decisions/ADR-001-desktop-app-toolkit.md). Where this spec says "as the desktop app",
the desktop spec's section is the authority.

## 1. Problem

A person at a venue should be able to use an iPhone as a camera into Open Live the way the desktop
app (#62) works: sign in with Open Source Cloud, pick the instance, pick a lens and a microphone,
Go live, and send only while a production holding the source is live (#130, PR #132).

What exists today is off-the-shelf software. In one manual test on 2026-11-21, Larix Broadcaster on
an iPhone, as an SRT caller into an Open Live SRT listener source in an active production, showed
picture and sound in Studio. Drops, network, settings, duration and connect order were not
recorded. Larix needs a host, a port and a passphrase typed by hand, does not register the source,
and sends whether or not a production is live, so it is exposed to the late-connect stall in
Eyevinn/strom#1277 whenever the phone connects first.

What an app of our own adds is the Open Live side: no address or port typed, the source registered
for the person, kept at Stop and reused at the next Go live (#115), and the uplink held back until a
production holding the source is live.

Out of scope (from #133 and as decided, §14): sending while the app is in the background, several
cameras from one phone, a return feed or talkback, destinations other than Open Live, iPad- and
Mac-Catalyst-specific layouts, Android, the App Store listing (TestFlight first, #133 item 5).

## 2. Decisions at a glance

| Topic | Decision | Section |
|---|---|---|
| Shared logic | This repo's Rust library, built for `aarch64-apple-ios` and `aarch64-apple-ios-sim`, linked into the app as a static library | ADR-002 §1, §3 |
| Media seam | `run::up` drives a `Media` backend instead of `StromClient` directly; `Strom` (today's behaviour) and `External` (the phone) implement it | §4 |
| Binding | A new crate `mobile/`: a hand-written C ABI of four functions carrying the desktop app's JSON message envelope (desktop §9.1). No UniFFI, no cbindgen | ADR-002 §2, §8 |
| Media on the phone | Own Swift over AVFoundation, VideoToolbox and AudioToolbox; own MPEG-TS muxer; libsrt built from source at a pinned tag with mbedTLS. HaishinKit not added | ADR-002 §3, §5 |
| Sign-in | The library's PKCE loopback sign-in (`osc::sign_in`), the page shown in `ASWebAuthenticationSession`; personal access token paste as the fallback | §6 |
| Credential | iOS Keychain through the existing `keychain::OsKeychain` (`security-framework`, widened to iOS); on iOS it writes with `set_generic_password_options` and `kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly` | §7 |
| Background, calls | The uplink pauses; the session stays armed and the source is kept, marked inactive; resumes in the foreground. No background modes | §9 |
| Wi-Fi / cellular | On a path change the SRT caller reconnects on the new path; no restart of the session | §9 |
| Distribution | TestFlight upload job in `release.yml`, guarded by the presence of the App Store Connect secrets, like `macos-app`'s Developer ID guard | §12 |

## 3. Layout

```
Cargo.toml            [workspace] members = [".", "app", "mobile"], default-members = ["."]
src/                  the library: gains the Media seam (§4), keychain and user_dir on iOS (§7)
mobile/Cargo.toml     [package] open-live-ingest-mobile, crate-type = ["staticlib", "rlib"], publish = false
mobile/src/lib.rs     the C ABI (§8.1): oli_core_new / oli_core_send / oli_core_free / oli_string_free
mobile/src/core.rs    the command dispatcher and view model, pure Rust, unit tested on the host
mobile/include/oli.h  the hand-written header (four functions, one callback type)
ios/                  the Xcode project: SwiftUI app, no third-party Swift package
ios/OpenLiveIngest/   Core.swift (C ABI wrapper, Codable messages), Capture.swift, Encoder.swift,
                      TSMuxer.swift, SRTSender.swift, Phone.swift (lifecycle, path monitor), views
ios/Vendor/           build-libsrt.sh: libsrt + mbedTLS from pinned source tarballs (digest checked)
                      into SRT.xcframework (device + simulator); the output is not committed
```

- **`mobile/` is a third workspace member and not a default member**, like `app/`. A plain
  `cargo build`, `test` or `clippy` at the root still builds the gateway only; the Linux CI job
  is unchanged. `mobile/` depends on `open-live-ingest = { path = "..", default-features = false }`,
  so no terminal crate is linked.
- **The `mobile` crate's core is host-testable.** Everything but the `extern "C"` shims is plain
  Rust and runs under `cargo test -p open-live-ingest-mobile` on Linux and macOS against the fake
  OSC and Open Live servers the desktop app's `testing.rs` already uses.
- **One version.** `mobile/` inherits `[workspace.package] version`; the iOS app's
  `CFBundleShortVersionString` is set from it at build time, so the phone, the desktop app and the
  CLI of one release report the same version.
- **Xcode project, no generator.** The `.xcodeproj` is committed and kept small (one app target,
  one unit-test target, one UI-test target). XcodeGen, Tuist and Fastlane are not added (§13).

## 4. The media seam in the library

Today `run::up` and everything it calls take a `StromClient` and call it directly
(`src/run.rs`: `start` → `create_flow` + `start_flow`, `remove_flow` → `stop_flow` +
`delete_flow`, `ensure_started` / `poll_flow` → `get_flow`, `poll_flow` → `srt_uplink`,
`start_again` → `start_flow`, `clear_conflicting_flows` → `list_flows`, `up` → `version`, and
`connect_strom` → `local_strom::ensure_running`), and it picks the inputs through Strom
(`video_devices`, `audio_devices`, then `devices::choose_saved` / `choose` / `pair_audio` /
`inputs_for`). Before any of that it refuses a second live session and records this one
(`running_gateway()`, `Pidfile::write`). The gate (`gate.rs`), source registration and
adoption (`reconcile_sources`, `plan_port`, `assign_ports_from_open_live`), the heartbeat and the
teardown never touch media; they only need "which inputs are there", "start this input's uplink
to this endpoint", "stop it", "is it running", and "its SRT counters".

The seam is exactly that, and nothing more:

```rust
/// What the session asks of whatever does the media. Not object-safe on purpose:
/// `run::up` is generic over it, so no `async-trait` and no boxing. Each method
/// returns `impl Future<Output = ..> + Send` rather than being an `async fn`: a bare
/// `async fn` in a `pub trait` trips rustc's `async_fn_in_trait` lint, which CI's
/// `cargo clippy --all-targets -- -D warnings` makes an error, and its future has no
/// `Send` bound, so `Session::start_inner`'s `tokio::spawn` of `run::up` would not
/// compile. Stable Rust, still no `async-trait`.
pub trait Media: Send + Sync {
    /// Once, before any input: Strom's version for the heartbeat, `None` on the phone.
    fn version(&self) -> impl Future<Output = Option<String>> + Send;
    /// Once, before any port is asked for: which cameras go live (or the test
    /// pattern) and the camera -> audio pairing, as `run::up` resolves them today.
    /// Warnings (a saved device not connected) go out as `events` as today.
    fn resolve_inputs(&self, cfg: &Config, inputs: Inputs, events: &Events)
        -> impl Future<Output = Result<ResolvedInputs>> + Send;
    /// Once, after the inputs are known (resolved, ports from the cloud, `inputs_for`)
    /// and before any is started, where `run::up` calls `clear_conflicting_flows`
    /// today: clears what would collide with them. `Ok(())` on the phone.
    fn prepare(&self, inputs: &[flow::Input], events: &Events)
        -> impl Future<Output = Result<()>> + Send;
    /// Starts the input's uplink: capture, encode, MPEG-TS, SRT caller to
    /// `input.endpoint.venue_uri()`.
    fn start(&self, input: &flow::Input, video: &Video)
        -> impl Future<Output = Result<()>> + Send;
    /// Stops and forgets it. Stopping one that does not run is not an error.
    fn stop(&self, input: &flow::Input) -> impl Future<Output = Result<()>> + Send;
    /// Restarts a running uplink (the #127 stall restart). Strom: stop_flow + start_flow.
    fn restart(&self, input: &flow::Input) -> impl Future<Output = Result<bool>> + Send;
    /// Whether it runs, as the session's state machine reads a flow today.
    fn running(&self, input: &flow::Input) -> impl Future<Output = Result<Option<bool>>> + Send;
    /// The SRT sender's cumulative counters, `None` before a peer exists.
    fn uplink(&self, input: &flow::Input)
        -> impl Future<Output = Result<Option<SrtCallerStats>>> + Send;
}
```

The input type is `flow::Input` (`flow.rs:74`: `id`, `name`, `source`, `endpoint`), which
`devices::inputs_for` returns (`devices.rs:322`) and `run::up` already holds; its `endpoint`
carries the uplink, so `start` takes no separate `Endpoint`. `Endpoint`'s `passphrase` and
`pbkeylen` are private (`config.rs:381-382`) and §8.2's `uplink` needs them, so OL-64a adds the
read accessors `Endpoint::passphrase()` and `Endpoint::pbkeylen()`, used by `ExternalMedia`.

- **`StromMedia`** wraps `LocalStrom` + `StromClient` and is today's code moved, not changed:
  flow ids stay derived with `flow::flow_id(gateway_id, &input.id)` (UUIDv5 over gateway id +
  input id) and its `start` builds with `flow::build(gateway_id, input, video)`, as today. Its
  constructor does only, in today's order, the single-instance check and pidfile write
  (`running_gateway()` → `AlreadyRunning`, then `Pidfile::write`, `run.rs:521-528`; the guard held
  by `StromMedia` and removed when it drops), then the Strom start / adopt / reclaim of
  `connect_strom` / `local_strom`. The two argument checks at `run.rs:535-540` (`--audio` with
  `--test`; sending while live without registration) run in `run::up` before `StromMedia` is
  built, so a rejected argument never starts Strom. `clear_conflicting_flows` (`run.rs:1174`,
  over `&[flow::Input]`) cannot run in the constructor, since the inputs exist only after
  `resolve_inputs`, the cloud's ports and `inputs_for`: it is `StromMedia::prepare`, called by
  `run::up` where it is called today (`run.rs:790`, after source registration and
  `free_held_ports`); `ExternalMedia::prepare` returns `Ok(())`. So the CLI
  and the desktop app behave as today (a second `up` or Go live is refused, #95's reclaim still
  sees a starting gateway), and the phone, which never builds a `StromMedia`, writes no pidfile.
  Its `resolve_inputs` is today's device code unchanged (`video_devices`, `audio_devices`,
  `devices::choose_saved` / `choose` / `pair_audio`). `tests/golden.rs`, `tests/session.rs` and
  every unit test in `run.rs` pass unchanged; that is the acceptance check for the refactor.
- **`ExternalMedia`** holds, per input, the last state and counters the host reported, and hands
  each `start` / `stop` / `restart` to the host over its own channel: `ExternalMedia::new()`
  returns it with a receiver of media requests, which `mobile/` turns into `media` events
  (§8.2). They are not a new `gateway::Event` variant, so the CLI's exhaustive event rendering
  in `main.rs` is untouched. `running` and `uplink` answer from the last report. Its
  `resolve_inputs` uses `cfg.inputs` as Swift picked them: one camera per `[[input]]`, `device`
  the AVFoundation unique id, `audio` the microphone's port UID, no lookup. It is plain Rust and
  is tested with a fake host in `tests/session.rs`, through the same armed-session scenarios #132
  added (production goes `active` → `start`; leaves `active` → `stop`; Stop → `stop` and the
  source kept).
- **The armed entry point with a backend** is `Session::start_armed_with(media, settings, inputs,
  credential, sources)`, generic over `M: Media + 'static`. `Session::start_armed` (and `start`,
  `start_with`, `start_keeping_sources`) keep their signatures and build `StromMedia` inside the
  session task, as `run::up` connects to Strom today; `mobile/`'s Go live calls
  `start_armed_with` with an `ExternalMedia`.
- **`SrtCallerStats` stays the one stats type.** It is `strom-types`' (`bytes_sent`,
  `packets_sent`, `packets_sent_dropped`, `rtt_ms`, `send_rate_mbps`, all optional), so the
  stall detection (`judge`, `Counters`) and the desktop §5.1 Link line work unchanged. The phone
  fills it from libsrt's `srt_bstats` (`byteSentTotal`, `pktSentTotal`, `pktSndDropTotal`,
  `msRTT`, `mbpsSendRate`), cumulative per connection as libsrt keeps them, which is the
  semantics `Counters` already assumes ("they start again from zero when the caller
  reconnects").
- **Device discovery stays on each side of the seam.** On the phone the inputs come from
  AVFoundation, picked in Swift, and reach the library as one `InputSettings` (`device` = the
  AVFoundation unique id of the lens, `audio` = the microphone's port UID, `label`), which
  `ExternalMedia::resolve_inputs` takes as is. `ResolvedInputs` is what `run::up` holds today
  after device choice: the chosen cameras, whether the test pattern is used, and the camera →
  audio map. Strom's device lookup and `local_strom` are not called on the phone; what follows
  resolution stays shared in `run::up` (`resolve_cloud`, `devices::inputs_for` with the cloud's
  ports, `attach_audio`, `apply_labels`, `check_unique_names`).
- **`CLAUDE.md` and `DESIGN.md` §1/§3 are amended in OL-64a:** "Strom owns the media plane" stays
  true for the gateway on a computer; on the phone there is no Strom and the iOS app's Swift code
  owns capture, encode, mux and SRT, with the library still owning no media. The process-boundary
  rule (never link the Strom engine) is unchanged.

## 5. The screens

SwiftUI, one navigation stack, the desktop's three steps on separate screens because a phone has no
room for all three at once.

1. **Where to send.** "Sign in with Open Source Cloud" (§6); then the workspace and the list of its
   Open Live instances (`api::instances`), one picked. "Use a self-hosted Open Live" (URL + key,
   `api::check_self_hosted`) and "Paste a personal access token" under Advanced, as on the desktop.
   Saved at once; later launches open on "Sending to <instance> (<workspace>)" with Change.
2. **What to send.** The lens (back wide, back ultra wide, back telephoto, front: whatever
   `AVCaptureDevice.DiscoverySession` reports on this phone) and the microphone (built-in, wired
   or Bluetooth headset, USB-C: `AVAudioSession.availableInputs`), and the label. The Studio name
   is `<gateway.name> — <label>`, only the label editable (§6.1). A live camera preview
   (`AVCaptureVideoPreviewLayer`, part of the OS) fills the screen behind the controls, and a level
   bar for the picked microphone sits under its picker ("Level and picture" below).
3. **Go live / Stop.** One button. The state line as the desktop's per-input states (`Armed`,
   `Waiting for production`, `Connecting`, `Live`, `Failed: <reason>`, `Stopping`), and under it
   the desktop §5.1 status lines: `Production: Morning Show (active), slot video_in_1` and
   `Link: connected, 0.2 % dropped, RTT 31 ms`, plus the network (`Wi-Fi` / `Cellular`) and the
   bitrate. While the uplink runs, the picture being sent (192×108 pt) and its level bar sit under
   the status lines; Armed or Waiting for production shows neither (the camera is off). A banner
   carries session-wide notices (Open Live unreachable, sign-in ending).

**Level and picture (#173, OL-71).** The numbers are the desktop app's (desktop-app.md §5.2):

- **Level.** `AudioLevel` (`LevelMeter.swift`): per 250 ms window, the highest channel's sample
  peak and RMS in dBFS, clamped to -90…0 (digital silence is -90), shown at 4 Hz as a bar from -60
  to 0 dBFS (the peak, with the RMS as a darker inner bar). Its accessibility value is the level
  (`-20.0 dBFS peak, -23.0 dBFS RMS`), id `levelBar`. The test pattern's 1 kHz tone at -20 dBFS
  reads -20 peak and -23 RMS.
- **No sound.** `SilenceDetector`, a pure function of the windows and a clock: silent when every
  window's peak is below -60 dBFS, or no sound arrived at all, for 5 s; cleared at the first window
  whose peak reaches -54 dBFS (6 dB of hysteresis). The clock starts when the source runs. Silent
  shows "No sound from `<microphone>`." under the bar (id `noSoundWarning`), the microphone named by
  the audio route's port name (`AVAudioSession.currentRoute`). A muted microphone warns within
  5.25 s, inside #173 AC3's 10 s.
- **Step 2.** `PreviewSession` routes the audio session to the picked microphone as capture does
  (`Capture.routeMicrophone`, shared) and adds the microphone and an `AVCaptureAudioDataOutput` to
  its own capture session; its PCM feeds a `MediaMonitor` for the bar. It asks for the microphone
  permission the first time; without it the picture shows and the bar does not. Nothing for the
  test pattern (the simulator): step 2's level is checked on a real phone.
- **Step 3.** iOS allows one ordinary capture session on the camera, so the picture comes from the
  capture that feeds the encoder: `MediaSession.run()` hands the same pixel and PCM buffers it gives
  `MediaPipeline` (camera or test pattern), after it, to `MediaMonitor`. The tap never blocks or
  copies in the encode path: it checks a flag under a lock and hands the retained buffer to the
  monitor's own queue, which measures the level and scales a picture to 320 px wide (Core Image)
  about twice a second, skipping one while the last is still being made. The picture (id
  `livePreview`, value `showing` once a frame arrived) is what goes out; the sent stream is
  unchanged.
- **Background.** Both run only while their source does: step 2's preview stops when the screen
  goes and when the app goes to the background (and comes back with it if step 2 is still shown);
  step 3's monitor starts and stops with `MediaSession`, which the background pause (§9) stops.

**Advanced** (collapsed): venue name (`gateway.name`), uplink latency, passphrase and key length,
video bitrate, resolution and frame rate, log level. The values are the library's `Config` fields;
defaults are §11's.

**Orientation.** The interface rotates; the picture is always encoded 16:9 landscape (the
production's format). In portrait the preview shows the landscape crop with a "Turn the phone"
hint, and the encoder is fed the landscape-oriented connection
(`AVCaptureConnection.videoRotationAngle`), never letterboxed.

## 6. Sign-in

**The library's sign-in, unchanged.** `osc::sign_in` (#75) binds a listener on `127.0.0.1:0`,
registers a public client (`POST https://app.osaas.io/api/connect/register` with
`{client_name, redirect_uris: ["http://127.0.0.1/callback"]}`, falling back to the `cli` client
id), builds the authorize URL (`/api/connect/authorize`, `response_type=code`, PKCE S256, `state`)
and exchanges the code at `/api/connect/token`. `osc::browser_login_with(open)` already takes the
"open this page" step as a closure; on iOS that closure emits the `osc_sign_in_page` event (§8.2)
and the Swift side presents the URL in an `ASWebAuthenticationSession`
(`prefersEphemeralWebBrowserSession = false`, so an existing OSC browser login is reused).

- The authentication sheet runs while the app stays in the foreground, so the app's loopback
  listener is up when OSC redirects to `http://127.0.0.1:<port>/callback`. The library answers the
  callback and returns the `Credential`; the core then emits `osc_signed_in` and Swift cancels the
  sheet (`ASWebAuthenticationSession.cancel()`), because a loopback `http:` redirect is not a
  callback scheme the session completes on by itself.
- **To be verified in OL-64b on the simulator** (agent-run, §15): that the sheet's out-of-process
  web view reaches the app's `127.0.0.1` listener, and that the cancel after the callback leaves no
  error page. If it does not, the fallback is a custom-scheme redirect
  (`com.eyevinn.open-live-ingest.ios:/callback`) with `ASWebAuthenticationSession`'s own callback, which
  needs OSC's dynamic registration to accept a non-`http(s)` redirect; the research
  ([`OSC_BROWSER_SIGN_IN.md`](../research/OSC_BROWSER_SIGN_IN.md) §2) only records that `http:` is
  loopback-only. OL-64b tries the registration and records the answer.
- **Personal access token paste** (Advanced) is the fallback that always works, as on the desktop
  (`api::keep_osc_token`), and is what the agent-run end to end uses (§15, OL-64h).
- **Refresh.** As the desktop: the kept credential is OSC's refresh token (30 days, renewed at
  each refresh, `osc::refresh`, `grant_type=refresh_token`), and the one-hour access token is
  refreshed from it during a session by `credential::KeptCredential`.

**Open Live calls, as the desktop makes them** (all in `src/openlive.rs` and `src/osc.rs`, used
unchanged):

| Step | Request | Auth | Notes |
|---|---|---|---|
| Instances | `GET https://catalog.svc.prod.osaas.io/mysubscriptions`, find `serviceId == "eyevinn-open-live"`, then `GET <apiUrl>` | `x-pat-jwt: Bearer <OSC token>`; then `x-jwt: Bearer <service token>` | Only `name` and `url` of each instance are decoded (`osc::Instance`) |
| Service token | `POST https://token.svc.prod.osaas.io/servicetoken`, `{"serviceId":"eyevinn-open-live"}` | `x-pat-jwt: Bearer <OSC token>` | Answer `{token, expiry}`; cached, refreshed 5 min before `expiry`; 401/403 → sign in again |
| Where the cloud Strom is | `GET <instance>/api/v1/server-info` | `Authorization: Bearer <service token>` | `stromHost`, `srtPorts` (or the older `srtPortRange`), `srtPortState`; 404 = an Open Live older than the route |
| Sources | `GET`, `POST <instance>/api/v1/sources`; `PATCH`, `DELETE /api/v1/sources/:id` | same | Body `SourcePayload`: `name`, `address`, `streamType: "srt"`, `status: "active"/"inactive"`, `latency`, `liveCamera: true`, optional `gatewayId`. 409/422 keep Open Live's `error` (`openlive::Refused`) |
| Productions | `GET <instance>/api/v1/productions` | same | `_id` (or `id`), `name`, `status` (`inactive`/`activating`/`active`/`ended`), `sources[].{sourceId, mixerInput}`; drives the gate (`gate::Watch`) every 2 s armed, 1 s while activating |
| Heartbeat (optional) | `wss://<instance>/ws/gateways/<gateway_id>/heartbeat` | `Authorization: Bearer <gateway token>` | Only when a gateway id and token are set under Advanced; close code 4401 = token refused |

No Open Live or OSC endpoint is added or changed by this epic.

## 7. Settings and data model

**Same `Config`, same TOML, same code.** The core reads and writes the library's settings file
through `api::load_from` / `api::validate` / `api::save_to`; the path is
`<app sandbox>/Library/Application Support/open-live-ingest/gateway.toml`. OL-64b widens
`config::user_dir()`'s `cfg!(target_os = "macos")` branch to iOS (today iOS falls into the
XDG branch, `$HOME/.config`), so the file lands where iOS backs up application support. It is
excluded from iCloud backup (`isExcludedFromBackup`), since it can hold a passphrase.

**No schema change.** The phone writes:

```toml
[gateway]
id   = "3f2a…"                  # a UUID made at first launch (§7.1)
name = "iPhone 3F2A"            # editable; the source prefix (§7.1)

[open_live]
url = "https://<instance>"      # picked in step 1
auth_mode = "osc"
credential = "keychain"         # the OSC credential is in the iOS Keychain, never in the file

[uplink]                        # mode stays "caller"; host and ports come from server-info
latency_ms = 200
# passphrase / pbkeylen only when set under Advanced

[video]
bitrate_kbps = 4000             # §11
keyframe_interval = 30          # 1 s at 30 fps

[capture]
video_resolution = "1280x720"
video_framerate = "30/1"

[[input]]
device = "<AVCaptureDevice.uniqueID>"
audio  = "<AVAudioSessionPortDescription.uid>"
label  = "Back camera"
```

`[video] codec`, `encoder_preference`, `quality_preset`, `tune` and `rate_control` are not
shown: the phone always sends H.264, hardware, CBR; `validate` rejects other values on iOS with
a `Problem` so a hand-copied desktop file cannot silently ask for AV1.

### 7.1 Identity on the phone

- **`gateway.id` is a UUID generated at first launch** and saved. `hostname()` on iOS returns a
  generic name ("localhost" or the model name), and two phones with the same id would fight over
  each other's sources.
- **`gateway.name` defaults to `iPhone <first 4 hex of the id>`.** The source sweep at Stop
  (`sweep_sources`) and `down` find sources by the `<gateway.name> — ` prefix
  (`devices::name_prefix`), so two phones named alike would remove each other's sources. A
  unique default prevents that; editing it is allowed (Advanced), and `validate` warns when the
  instance already has sources under the new prefix that this phone did not register.
- **The Keychain entry** is the existing one (`keychain::SERVICE` = `open-live-ingest`). Today
  `keychain::OsKeychain::set` calls `set_generic_password`, which sets no accessibility; on iOS
  OL-64b writes it with `set_generic_password_options` instead, its `PasswordOptions` carrying
  `kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly` (`SecAccessControl::create_with_protection`),
  so it is never synced or restored to another device. macOS keeps today's call.
- **Local state**, completing the repo invariant for this front end: the settings file, the one
  Keychain entry, and `app.log` with one rotated copy (desktop §7's own rotation code, in
  `Library/Logs`, shared from Advanced → "Share logs" through the share sheet). No pidfile: the
  single-instance check and pidfile write live in `StromMedia`'s constructor (§4), which the phone
  never builds; iOS runs one instance of an app, and there is no Strom to record.

## 8. App API (internal: Swift ⇄ Rust core)

Internal to the app, not a network API: no port is opened except the sign-in's short-lived
loopback listener (desktop §9's reasoning; the `/health` and `/ready` rule for services does not
apply).

### 8.1 C ABI

```c
/* mobile/include/oli.h */
typedef struct OliCore OliCore;
/* Called on a core thread with one UTF-8 JSON message (reply or event). The string
   is valid only during the call; Swift copies it. */
typedef void (*oli_deliver)(void *ctx, const char *json);

OliCore *oli_core_new(const char *app_support_dir, const char *log_dir,
                      oli_deliver deliver, void *ctx);      /* NULL on failure */
void     oli_core_send(OliCore *core, const char *json);    /* one command (§8.2) */
void     oli_core_free(OliCore *core);                      /* bounded teardown (4 s), then frees */
void     oli_string_free(char *s);                          /* reserved; nothing returns an owned string today */
```

- The core owns a multi-threaded Tokio runtime; `oli_core_send` never blocks on the network.
- Every `extern "C"` function catches panics (`std::panic::catch_unwind`) and turns them into a
  `{"event":"fatal"}` message; nothing unwinds into Swift.
- Null and non-UTF-8 input is an error reply, never undefined behaviour. The four shims are the
  only `unsafe` code, each with a `// SAFETY:` comment and a test in `mobile/tests/abi.rs` that calls
  the exported symbols the way Swift does (null, non-UTF-8, a panic in a command, free while live).

### 8.2 Messages

The desktop envelope (desktop §9.1), unchanged:

- Swift → core: `{"id": <u32>, "cmd": "<name>", "args": { ... }}`
- core → Swift reply: `{"id": <u32>, "ok": <value>}` or
  `{"id": <u32>, "error": {"message": "<one line>", "action": "<action id or null>"}}`
- core → Swift event: `{"event": "<name>", "data": { ... }}`

Commands, one `serde` enum (`#[serde(tag = "cmd", content = "args")]`); an unknown command or bad
arguments is an error reply:

| `cmd` | `args` | `ok` | Library call |
|---|---|---|---|
| `state` | — | the view model (where, input, live, Advanced) | `api::load_from` |
| `osc_sign_in` | — | `{workspace, instances[]}` (after `osc_sign_in_page`) | `api::osc_sign_in` (`osc::browser_login_with`) |
| `osc_sign_in_cancel` | — | `null` | drops the sign-in future (the person closed the sheet) |
| `osc_paste_token` | `{token}` | `{workspace, instances[]}` | `api::keep_osc_token`, `api::workspace` |
| `choose_instance` | `{url}` | view model | `api::save_to` |
| `self_hosted` | `{url, api_key}` | view model | `api::check_self_hosted`, `api::save_to` |
| `set_input` | `{device, audio?, label?}` | view model | `config::input_problems`, `api::save_to` |
| `set_advanced` | the Advanced values | view model, or `problems[]` | `api::validate`, `api::save_to` |
| `go_live` | — | `null` (progress as events) | `Session::start_armed_with(ExternalMedia, settings, inputs, credential, Sources::Keep)` (§4) |
| `stop` | — | `null` | `SessionHandle::stop` |
| `media_report` | `{input, state: "starting" \| "running" \| "failed" \| "stopped", reason?, stats?: {bytes_sent, packets_sent, packets_sent_dropped, rtt_ms, send_rate_mbps}}` | `null` | `ExternalMedia` (§4); sent by Swift on each change and every 2 s while running |
| `phone` | `{foreground: bool, interrupted: bool, network: "wifi" \| "cellular" \| "wired" \| "other" \| "none"}` | `null` | §9: pauses / resumes the uplink, notes the network in the status |
| `forget_sign_in` | — | view model | `api::forget_osc_credential` |

Events: the desktop §9.3 set where it applies (`where`, `osc_sign_in_page`, `input`, `summary`,
`stats`, `status`, `notice`, `stopping`, `stopped`), plus:

| `event` | `data` | Meaning |
|---|---|---|
| `osc_signed_in` | `{}` | the loopback callback arrived; Swift cancels the authentication sheet |
| `media` | `{input, action: "start", uplink: {host, port, latency_ms, passphrase?, pbkeylen?, streamid?, relay?, dial_in?}, video: {width, height, fps, bitrate_kbps, keyframe_interval}, audio: {sample_rate: 48000, channels: 2, bitrate_kbps: 128}}` | `ExternalMedia::start`: open capture, encoders and the SRT caller. Through Open Live's SRT relay (#161) `host` and `port` are the relay's, `streamid` is set on the socket (`SRTO_STREAMID`) before it calls and kept for every reconnect, and `relay` (its name) and `dial_in` (Open Live's `connect.uri`, never with a passphrase) are for the status lines; without a relay none of the three is there and the caller dials a port of the cloud Strom |
| `media` | `{input, action: "stop" \| "restart"}` | `ExternalMedia::stop` / `restart` |
| `fatal` | `{message}` | a panic was caught at the ABI; the app shows "Something went wrong" and offers Share logs |

**The one secret that crosses the boundary** is `media.start`'s `uplink.passphrase`: libsrt on the
Swift side needs it. Desktop §9.4's "events carry no secret" is kept for every other event, and
the core logs no message bodies (only command and event names, as the desktop's app log). Swift
never logs `media` payloads, and a unit test feeds a known passphrase through a Go live and asserts
it appears in neither log.

### 8.3 Who decides what

The Rust core decides **when** to send (the gate, from the productions), **where** (host, port
and SRT parameters from `server-info`, the source's port, the settings) and **what is registered**
(the source). Swift decides **how**: which `AVCaptureDevice` and audio route, encoder settings it
is handed, the SRT socket. Swift never talks to Open Live or OSC except to show the sign-in page.

## 9. Phone behaviour

| Situation | What the app does | Why |
|---|---|---|
| Live (armed or sending) | `UIApplication.isIdleTimerDisabled = true`; back to `false` at Stop | the screen must not lock mid-show (#133 item 4) |
| App goes to the background, screen locked | Swift sends `phone {foreground:false}`. The core stops the uplink (`ExternalMedia::stop`, as for `NoProductionLive`), PATCHes the source `inactive` and stays armed. All of this inside `beginBackgroundTask`, bounded at 4 s (the CLI's `teardown_within` deadline) | iOS stops camera capture in the background (`AVCaptureSession` interruption `videoDeviceNotAvailableInBackground`); a source left `active` with nothing arriving is the "stale source" #133 item 4 names |
| Back in the foreground | `phone {foreground:true}`; the next gate look starts the uplink again if a production holding the source is still `active` | the same path as a production going live (#130) |
| Phone call, Siri, another app taking the camera or the audio session | `AVCaptureSession.wasInterrupted` / `AVAudioSession.interruptionNotification` → `phone {interrupted:true}`, handled as the background row; `interruptionEnded` → resume | the same reason |
| App suspended before teardown finished, or killed | The source stays as it was. The next launch's Go live adopts it by name (same id, #115); on the server side Open Live sees the SRT caller gone | nothing else is possible after suspension; the source id is what the production slot needs |
| Wi-Fi ↔ cellular | `NWPathMonitor` → `phone {network}`. On a change of the satisfied interface Swift closes the SRT socket and calls again on the new path (`SRTSender.reconnect`), keeping encoder and capture running; the core counts it as the desktop counts a libsrt reconnect (counters start again from zero) | libsrt 1.5 has no connection migration: a socket stays bound to the old local address, so waiting for it to recover takes the peer idle timeout (5 s by default). Reconnecting at once costs one keyframe interval |
| No network | `phone {network:"none"}`; the uplink keeps trying (libsrt's connect timeout, then retry every 2 s); the Link line says `not sending` | Open Live being unreachable must never disturb the feed (repo invariant): nothing is torn down |
| Thermal state `serious` or `critical` (`ProcessInfo.thermalStateDidChangeNotification`) | Notice: "The phone is hot; the picture may drop frames." No automatic bitrate change in v1 | a visible reason beats a silent quality drop; adaptive bitrate is a follow-up (§14 Q8) |

**No background modes.** `UIBackgroundModes` stays empty: no `audio` mode to keep the process
alive, no multitasking camera entitlement. App Review guideline 2.5.4 limits background audio to
apps that play or record audio for the user in the background, which a paused uplink is not, and
the camera is not available in the background on iPhone anyway.

## 10. Service interactions

```mermaid
sequenceDiagram
    actor P as Person
    participant UI as SwiftUI
    participant M as Swift media (capture, VideoToolbox, TS, libsrt)
    participant C as Rust core (mobile/, the library)
    participant OSC as Open Source Cloud
    participant OL as Open Live
    participant CS as Cloud Strom

    P->>UI: Sign in
    UI->>C: {"cmd":"osc_sign_in"}
    C->>OSC: POST /api/connect/register (loopback redirect)
    C-->>UI: event osc_sign_in_page {url}
    UI->>OSC: ASWebAuthenticationSession(url)
    OSC-->>C: redirect http://127.0.0.1:<port>/callback?code&state
    C->>OSC: POST /api/connect/token (code + PKCE verifier)
    C->>C: keep refresh token in iOS Keychain
    C-->>UI: event osc_signed_in; reply {workspace, instances[]}
    C->>OSC: GET mysubscriptions, POST servicetoken, GET instances
    P->>UI: pick instance, lens, microphone; Go live
    UI->>C: {"cmd":"go_live"}
    C->>OL: GET /api/v1/server-info (stromHost, srtPorts)
    C->>OL: GET /api/v1/sources; adopt by name (same id) or POST /api/v1/sources
    C-->>UI: event input "armed"
    loop every 2 s while armed (1 s while activating)
        C->>OL: GET /api/v1/productions
        alt a production holding the source is active
            C-->>UI: event media {action:"start", uplink, video, audio}
            UI->>M: start capture + encoders, SRT caller to host:port
            M->>CS: MPEG-TS (H.264 + AAC) over SRT, mode=caller
            M-->>C: media_report running + stats (every 2 s)
            C->>OL: PATCH /api/v1/sources/:id status active (once delivering)
            C-->>UI: event input "live", status lines
        else none holding it is active any more
            C-->>UI: event media {action:"stop"}
            UI->>M: stop SRT, encoders, capture
            C-->>UI: event input "armed"
        end
    end
    opt app to background / phone call
        UI->>C: {"cmd":"phone","args":{"foreground":false}}
        C-->>UI: event media {action:"stop"}
        C->>OL: PATCH /api/v1/sources/:id status inactive
    end
    P->>UI: Stop
    UI->>C: {"cmd":"stop"}
    C-->>UI: event media {action:"stop"}
    C->>OL: keep the source, status inactive; sweep other sources of the prefix
    C-->>UI: event stopped
```

## 11. Configuration

- **No new environment variables, no new settings keys.** The phone writes existing `Config`
  fields only (§7). `OLI_*` overrides are still applied by `config::load_or_default`, but an iOS app has no environment to set them in.
- **Phone defaults** (written at first launch, changeable under Advanced): 1280×720 at 30 fps,
  H.264 High profile, CBR at 4000 kbps (`kVTCompressionPropertyKey_AverageBitRate` plus
  `DataRateLimits` at 1.0 s; `kVTCompressionPropertyKey_ConstantBitRate` where the OS offers it),
  keyframe every 30 frames, no B-frames (`AllowFrameReordering = false`), AAC-LC 48 kHz stereo
  128 kbps; SRT latency 200 ms (the library's default), caller mode. Lower than the desktop's
  6000 kbps because cellular uplinks are the expected case; the "Before building" measurement
  (§15, OL-64-0) may change these, and the change is a settings default, not a design change.
- **SRT parameters handed to libsrt**, from `config::Endpoint`'s caller URI
  (`srt://<host>:<port>?mode=caller&latency=<ms>`, plus `passphrase` and `pbkeylen` when set):
  `SRTO_LATENCY` (= `SRTO_PEERLATENCY`), `SRTO_PASSPHRASE`, `SRTO_PBKEYLEN`, `SRTO_TRANSTYPE`
  live, `SRTO_PAYLOADSIZE` 1316 (7 TS packets), `SRTO_MAXBW` 0 with `SRTO_INPUTBW` set to the
  CBR rate and `SRTO_OHEADBW` 25 %. The cloud side is unchanged: a source address
  `srt://:<port>?mode=listener` (`Endpoint::cloud_uri`) read by `builtin.mpegtssrt_input`.
- **MPEG-TS shape**: a standard single-program transport stream carrying the same codecs the
  desktop's flow sends (H.264 from `builtin.videoenc`, AAC encoded by `builtin.mpegtssrt_output`,
  `src/flow.rs`): H.264 as stream type 0x1B (Annex B, SPS/PPS before every IDR), AAC-LC as stream
  type 0x0F (ADTS), PCR on the video PID at least every 40 ms, PAT/PMT at least every 100 ms. The
  PID numbers are the muxer's choice; `builtin.mpegtssrt_input` reads the program from the PMT.
  `TSMuxer.swift` (OL-64d) uses program 1, PMT PID 0x1000, video PID 0x0100 (also the PCR PID)
  and audio PID 0x0101.
  OL-64d checks the stream with `ffprobe`, and OL-64h checks that the cloud Strom decodes it.
- **No Strom pipeline change and no EFP path.** Contribution stays MPEG-TS over SRT
  (`DESIGN.md` §9); the EFP sequence-validation rule for ingest designs does not apply.
- **Info.plist**: `NSCameraUsageDescription`, `NSMicrophoneUsageDescription`,
  `NSLocalNetworkUsageDescription` is **not** needed (the loopback listener is not the local
  network), `ITSAppUsesNonExemptEncryption = false` with the export-compliance answer recorded in
  OL-64g (TLS and SRT's AES only, standard algorithms), `UIRequiredDeviceCapabilities` `arm64`,
  `LSApplicationCategoryType` `public.app-category.video`, bundle id
  `com.eyevinn.open-live-ingest.ios` (decided; the macOS app keeps `com.eyevinn.open-live-ingest`; registered by a human, §14 Q11).
- **Minimum iOS 17.0** (decided, §14 Q10).

## 12. Build, CI and distribution

- **CI job `ios`** (`macos-latest`, `ci.yml`): `rustup target add aarch64-apple-ios
  aarch64-apple-ios-sim`; `cargo clippy -p open-live-ingest-mobile --target aarch64-apple-ios
  -- -D warnings`; `cargo test -p open-live-ingest-mobile` (host); `ios/Vendor/build-libsrt.sh`
  (cached by the pinned versions' digest); `xcodebuild build-for-testing` and `test-without-building`
  on an iOS Simulator destination, which runs the Swift unit tests (TS muxer, message coding,
  lifecycle state machine) and the UI tests against the fake servers.
- **`Cargo.lock` stays one lockfile**; iOS adds no Rust crate (§13). The `ios` job also runs
  `cargo tree -p open-live-ingest-mobile --target aarch64-apple-ios -e normal,build` and fails
  when the crate count differs from the number recorded in `mobile/Cargo.toml`'s comment, so a
  new transitive dependency is a conscious change.
- **Release job `ios-app`** (`release.yml`, `needs: build`): `xcodebuild archive` and
  `-exportArchive` with `method = app-store-connect` and `destination = upload`, authenticated
  with an App Store Connect API key (`-authenticationKeyPath`, `-authenticationKeyID`,
  `-authenticationKeyIssuerID`). Guarded like `macos-app`'s `HAS_DEVELOPER_ID`: without the
  secrets (`APPLE_DISTRIBUTION_P12`, `APPLE_DISTRIBUTION_P12_PASSWORD`,
  `APPLE_IOS_PROVISIONING_PROFILE`, `APP_STORE_CONNECT_KEY_ID`,
  `APP_STORE_CONNECT_ISSUER_ID`, `APP_STORE_CONNECT_KEY_P8`) it builds an unsigned simulator
  `.app` and a device archive with signing disabled, uploads them as workflow artifacts, and
  skips the upload with a notice. The existing `APPLE_NOTARY_*` key is reused if its role is App
  Manager or higher. The role is not readable by an agent, so whoever supplies the §14 Q11
  certificate confirms it (Q11's API-key item), and it is recorded in #133's closing check next to
  the TestFlight upload (re-scoped from OL-64g on #143).
- **Internal TestFlight testers only.** External testers trigger Beta App Review, an
  outward-facing step for a human (§14 Q12).

## 13. Dependencies

Standing rule (CTO, 2026-10-22). Counts measured 2026-11-22 from the manifests on GitHub.

| What | Where | Decision |
|---|---|---|
| Rust crates for iOS | `mobile/` | **None new.** `serde`, `serde_json`, `tokio`, `tracing`, `tracing-subscriber`, `anyhow` are already the gateway's and the desktop app's. reqwest's `rustls-tls` and tokio-tungstenite's `rustls-tls-webpki-roots` build for iOS as they are (no OpenSSL) |
| `security-framework` | library, iOS | **Already in the tree** (macOS, #76): the dependency's target `cfg` widens from `target_os = "macos"` to `any(target_os = "macos", target_os = "ios")`. On iOS the write uses its `set_generic_password_options` with `kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly` (§7.1) instead of today's `set_generic_password`; read and delete are the same calls. +0 crates |
| UniFFI, cbindgen, swift-bridge, `safer-ffi` | — | **Not added.** Four hand-written `extern "C"` functions and a 20-line header (§8.1) carry JSON the desktop already defines; a binding generator would add a macro and build tooling stack (UniFFI: `uniffi_bindgen`, `askama`, `goblin`, `camino`, `cargo_metadata` and more) for a surface that never grows past the envelope |
| `async-trait` | — | **Not added**: `Media`'s methods return `impl Future<Output = ..> + Send` (stable Rust) with `run::up` generic over it (§4) |
| libsrt | `ios/Vendor` | **Add, built from source.** SRT is protocol handling and the reason to use the reference implementation is parity with the cloud side's GStreamer `srtsink`/`srtsrc`, which link libsrt. MPL-2.0 (file-level copyleft: we ship it unmodified, the licence and source link go in the app's About). Pinned release tarball, SHA-256 checked in `build-libsrt.sh`; the tag matches the libsrt the cloud Strom's GStreamer is built with (OL-64c records it). Its C++ standard library comes from the OS |
| mbedTLS | `ios/Vendor` | **Add, built from source**, as libsrt's crypto for the passphrase (AES-CTR, AES key wrap, PBKDF2). Crypto is never reimplemented. Apache-2.0, pinned LTS tarball, digest checked. Chosen over OpenSSL for size (libsrt needs only its cipher and KDF) and because no prebuilt OpenSSL binary is needed |
| HaishinKit (`SRTHaishinKit` + `HaishinKit`) | — | **Not added** (ADR-002 §5). `Package.swift` of 2.3.1 declares 6 package dependencies (`libaom-xcframework`, `libvpx-xcframework`, `libdatachannel-xcframework`, `libsrt-xcframework`, `swift-docc-plugin`, `Logboard`), all resolved even when only `SRTHaishinKit` is used; `libsrt-xcframework` is a prebuilt `libsrt.xcframework` binary plus `krzyzanowskim/OpenSSL-Package` (a second prebuilt binary). We would use its capture, codec, TS writer and SRT wrapper (a large fraction of the SRT path) but could not build its libsrt or OpenSSL from source without a fork |
| Swift packages | `ios/` | **None.** AVFoundation, VideoToolbox, AudioToolbox, Network (`NWPathMonitor`), AuthenticationServices and SwiftUI are the OS |
| XcodeGen, Tuist, Fastlane, CocoaPods | — | **Not added.** A committed `.xcodeproj` and `xcodebuild` cover build, test, archive and upload |
| Dev/CI tooling | `ci.yml` `ios` job | `cmake` from the runner image (libsrt's build). `ffprobe` and `srt-live-transmit` for the Swift tests (TS validity, an SRT listener on `127.0.0.1` for the sender test): Homebrew `ffmpeg` and `srt` at versions recorded in the job; they run only in CI, never ship, and see no secret (the job has none) |

Existing dependencies touched: none removed. `strom-types` stays a library dependency
(`SrtCallerStats`, and the Strom backend); it is pure serde and builds for iOS.

## 14. Open questions (resolved, with the decision)

Resolved at the Phase 1.5 spec gate, 2026-11-22. Only Q11 and Q13's device part need a human,
in one batched request; neither blocks OL-64-0 or OL-64a to OL-64c.

1. **How is the Rust library called from Swift?** **Decided:** a hand-written C ABI of four
   functions in a new `mobile/` crate, carrying the desktop app's JSON envelope (§8, ADR-002 §2).
   UniFFI, cbindgen, swift-bridge and a Swift reimplementation are rejected (ADR-002).
2. **Which media and SRT stack on the phone, and is HaishinKit the base?** **Decided:** own Swift
   over AVFoundation, VideoToolbox and AudioToolbox, an own TS muxer, and libsrt plus mbedTLS
   built from pinned, digest-checked source; HaishinKit is not added (ADR-002 §3, §5, with the
   measured counts). HaishinKit stays the named fallback if OL-64c cannot produce working device
   and simulator slices; switching to it supersedes ADR-002 (ADR-002, "Risks").
3. **Does HaishinKit meet the desktop's latency, passphrase, CBR and stats needs?** **Decided:**
   moot under Q2. For the record (2026-11-22, `SRTHaishinKit/Sources/SRT/SRTSocket.swift`): it
   sets socket options and reads `srt_bstats` into `SRTPerformanceData`, so it would have; the
   decision rests on the supply chain, not on capability.
4. **How does OSC sign-in work on iOS?** **Decided:** the library's PKCE loopback sign-in, shown
   in `ASWebAuthenticationSession`, with PAT paste as the fallback that always works. OL-64b
   checks the loopback path on the simulator first; a custom-scheme redirect is used only if that
   check fails and OSC's registration accepts the scheme. OL-64b records the answer either way
   (§6).
5. **What happens on background, lock or a phone call?** **Decided:** the uplink pauses, the
   session stays armed, the source is kept and PATCHed inactive inside a 4 s background task
   (`ENDING_TEARDOWN`); returning to the foreground resumes it through the gate.
   `UIBackgroundModes` stays empty (§9).
6. **Wi-Fi ↔ cellular without a restart?** **Decided:** reconnect the SRT caller on the new path
   at once, keeping capture and encoders running; libsrt 1.5 has no SRT connection migration
   (§9). Bonding both paths is out of scope.
7. **Identity of a phone?** **Decided:** `gateway.id` is a UUID made at first launch;
   `gateway.name` defaults to `iPhone <4 hex>` so two phones never share the sweep prefix, and
   `validate` warns when a renamed prefix collides with existing sources (§7.1).
8. **Encode settings and adaptation?** **Decided:** 720p30, H.264 High CBR 4000 kbps, 1 s GOP,
   no B-frames, AAC-LC 48 kHz stereo 128 kbps, 200 ms SRT latency by default, changeable under
   Advanced; no adaptive bitrate in v1 (§11). OL-64-0's numbers may change these defaults (a
   settings change, not a design change). An adaptive-bitrate follow-up issue is filed only if
   cellular drops exceed 1 % at the default.
9. **One camera per phone?** **Decided:** yes, one lens and one microphone per session.
   `AVCaptureMultiCamSession` is out of scope.
10. **Minimum iOS and bundle id?** **Decided:** minimum iOS 17.0: covers iPhone XS/XR and later,
    and gives the current `AVCaptureConnection.videoRotationAngle` and VideoToolbox CBR APIs
    without fallbacks. Bundle id `com.eyevinn.open-live-ingest.ios` (§11).
11. **Apple distribution credentials?** **Decided: human-only** (credentials and an account no
    agent holds). They block only OL-64g's upload step; OL-64g still ships the unsigned simulator
    and device builds and the guarded upload path without them. Goes into the single batched
    escalation together with Q13's device part.
    - *Needed now:* an Apple Distribution certificate (`.p12` + password), the bundle id
      `com.eyevinn.open-live-ingest.ios` registered with the Camera and Microphone capabilities, an App
      Store Connect app record, an App Store provisioning profile for it, and an App Store Connect
      API key with the App Manager role or higher (or confirmation that the existing
      `APPLE_NOTARY_KEY_*` key has that role), stored as the six secrets of §12.
    - *Cause category:* credential and account no agent holds.
    - *Prevent next time:* request the iOS distribution set together with the Developer ID set
      whenever an Apple deliverable is planned.
    - *Owner of that change:* the Eyevinn Apple Developer account holder who set up #25.
12. **External TestFlight testers?** **Decided:** internal testers only in this epic. Adding
    external testers sends the build through Beta App Review, an outward-facing step for a human
    after the epic closes, listed in the closing comment's "What a human does next".
13. **The "Before building" measurement on a real iPhone.** **Decided:** split by what needs a
    device. OL-64a to OL-64c proceed before the device numbers arrive: the issue's condition
    (cellular over 1 % drops, or a phone-first stall) can only change encode defaults, adaptation
    or gate behaviour, none of which touch the library seam, the iOS build or the libsrt
    cross-build.
    - *Connect order and strom#1277*: device-independent, agent-run. OL-64-0 runs it from a macOS
      runner with an SRT caller (`ffmpeg` test pattern, H.264 + AAC in TS, the phone defaults of
      §11) into an `openlivedev` SRT listener source: 60 s production-first and 60 s caller-first,
      SRT stats read twice a minute apart. If caller-first stalls, the arm-until-live gate (§4,
      #130) is what prevents it on the phone, and OL-64-0 records that the gate never lets the
      phone connect first. OL-64-0 must finish before OL-64d's defaults are frozen.
    - *Cellular and Wi-Fi drops from an iPhone*: needs a device and a SIM. **Human-only**, in the
      same batched request as Q11, and blocks no ticket: the Larix run of #133 "Before building"
      (Wi-Fi and cellular, 60 s each, stats twice). *Cause category:* hardware no agent holds.
      *Prevent next time:* a test iPhone on a SIM, reachable by the runner (a self-hosted macOS
      runner with a USB-attached device), named in the next device-app epic. *Owner:* the CTO.
      OL-64d's defaults are revisited when these numbers arrive (Q8).
14. **Does the phone create the Open Live gateway resource (heartbeat)?** **Decided:** no, as
    desktop Q10: gateway id and token stay optional under Advanced.

## 15. Sub-issue breakdown (cut after this spec merges)

Each is machine-completable; "What a human does next" goes in the PR body and closing comment.
Order: OL-64-0 and a in parallel → b, c → d → e, f → g → h.

| # | Title | Depends on | Done when (checkable) |
|---|---|---|---|
| OL-64-0 | Agent-run connect-order measurement against `openlivedev`: SRT caller from a macOS runner with the phone defaults, production-first and caller-first, 60 s each, stats twice | — | Results (drop %, RTT, whether caller-first stalls) posted on #133 with run links |
| OL-64a | Library: the `Media` seam (§4); `StromMedia` with today's behaviour, `ExternalMedia` with host reports; `DESIGN.md` §1/§3 and `CLAUDE.md` amended per ADR-002 | — | Every existing test passes unchanged (golden output byte-identical); new `tests/session.rs` cases drive an armed session through `ExternalMedia` (start on `active`, stop on leaving it, Stop keeps the source, stall restart) |
| OL-64b | iOS build: `mobile/` crate with the C ABI and dispatcher (§8), `user_dir` and keychain widened to iOS, `validate` rules for the phone (§7); CI `ios` job (Rust part); loopback-in-`ASWebAuthenticationSession` check on the simulator, and the custom-scheme registration answer recorded (§6) | a | `cargo test -p open-live-ingest-mobile` passes on Linux and macOS; `clippy` clean for `aarch64-apple-ios` and `-sim`; ABI tests pass; the crate count is recorded; the sign-in finding is in the PR |
| OL-64c | libsrt + mbedTLS from source into `SRT.xcframework`; `SRTSender.swift` (options of §11, `srt_bstats` stats, reconnect) | b | Simulator test: sender → `srt-live-transmit` listener on `127.0.0.1` delivers bytes; wrong passphrase is rejected; stats increase; reconnect restarts counters from zero |
| OL-64d | Capture, VideoToolbox H.264 CBR, AAC, `TSMuxer.swift`, and a test-pattern source for the simulator (no camera there) | c | Muxer unit tests; a 10 s simulator recording passes `ffprobe` with the §11 PIDs, stream types and keyframe interval; the sender test now carries the encoded stream |
| OL-64e | Screens: where to send (sign-in, PAT paste, instances, self-hosted), what to send (lens, microphone, label, preview), Go live / Stop with the status lines and notices | b, d | XCUITests on the simulator against the fake OSC and Open Live servers cover sign-in by PAT, instance pick, Go live → Armed → (fake production active) → Live → Stop |
| OL-64f | Phone behaviour (§9): idle timer, background and interruption pause with the bounded background task, foreground resume, `NWPathMonitor` reconnect, thermal notice | e | Unit tests drive the lifecycle state machine with synthetic notifications and assert the `phone` commands and the source PATCH; simulator UI test backgrounds the app while live and sees the source `inactive` on the fake server |
| OL-64g | Release job `ios-app` (§12): unsigned builds without the secrets, archive + TestFlight upload with them; version from the workspace | e | A `release.yml` PR run builds the simulator `.app` and the unsigned archive and skips the upload with the notice; with the secrets present the upload step succeeds |
| OL-64h | Agent-run end to end on the iOS Simulator (`macos-latest`) with the test pattern against `openlivedev`: PAT sign-in, Go live arms and sends nothing (AC2); activating a production through the Open Live API starts the uplink and the source's SRT input receives packets, under 1 % dropped over 60 s on the runner's network (AC1's numbers on the simulator); deactivating stops it (AC2); Stop keeps the source, Go live again reuses the id with no 409 (AC3); background while live pauses and marks the source inactive | b–g | Results posted on #133 with run links |

What a human does next, for #133's closing comment: install the TestFlight build on an iPhone
(needs Q11), sign in, pick an instance, the back camera and the built-in microphone, Go live with a
production holding the source active, and watch the input in Studio for 60 s on Wi-Fi and then on
cellular. Expected: picture and sound in Studio, the Link line under 1 % dropped on Wi-Fi (AC1);
switching Wi-Fi off mid-show recovers on cellular within a few seconds without pressing anything.

## 16. Risks

- **Late-connect stall (Eyevinn/strom#1277).** The gate (#130) is the mitigation: the phone sends
  only into an `active` production, which #128 found to be a clean connect. A reconnect after a
  network switch or a return from the background is a connect into an already active production,
  the case #128 found clean; OL-64-0 and OL-64h check both orders.
- **A slot added to a live production gets no receiver (#128).** Same as desktop: the core tells
  it (`SlotAddedWhileLive`) and the status line says the director must reactivate.
- **Loopback sign-in in `ASWebAuthenticationSession` may not reach the app.** Checked first in
  OL-64b; PAT paste always works; custom scheme as the second fallback (§6).
- **VideoToolbox CBR is approximate.** iOS encoders overshoot on scene changes; `DataRateLimits`
  bounds it per second, and `SRTO_INPUTBW` plus 25 % overhead absorbs bursts. OL-64d measures the
  peak-to-average ratio in the simulator recording and on OL-64h.
- **Thermal throttling** on a long show in the sun lowers frame rate; v1 tells the person (§9),
  adaptation is a follow-up.
- **App Review.** A camera app that streams is ordinary; the risks are background audio (not
  used, §9) and export compliance (TLS and AES in SRT, standard algorithms, declared in OL-64g).
- **Building libsrt and mbedTLS for iOS** is a maintained but non-trivial CMake cross-build;
  OL-64c owns the script and caches its output by digest. If it cannot produce a working
  simulator and device slice, ADR-002's fallback applies.
- **Source name prefix collisions** between phones or with a computer gateway would let one
  sweep remove another's source; prevented by the unique default name and the `validate`
  warning (§7.1).
- **Hardware no agent holds.** The device-only parts of AC1 (cellular, a real camera) are checked
  by a human after close, from the closing comment; everything else is agent-run (OL-64h).
