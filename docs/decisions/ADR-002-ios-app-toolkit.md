# ADR-002: iOS app toolkit: the library on iOS, a C ABI to Swift, and own media code over libsrt

**Date**: 2026-11-22
**Status**: Accepted

Spec: [`docs/specs/ios-app.md`](../specs/ios-app.md). Epic: #133 (OL-64). Model:
[ADR-001](ADR-001-desktop-app-toolkit.md).

## Context

#133, filed by the product decision owner, asks for an iPhone app that does what the desktop app
(#62) does: OSC sign-in, instance pick, lens and microphone, Go live, and sending only while a
production holding the source is live (#130). The link is SRT, like the desktop's: MPEG-TS over
SRT in caller mode to the cloud Strom, with the desktop's latency and passphrase settings.

Three decisions are needed:

1. **Where the shared logic comes from.** #133 item 1 asks for it to come from this repo's
   library: OSC sign-in and refresh, the instance list, source registration and adoption, the
   heartbeat, and the arm-until-live gate. The library today drives a local Strom for media
   (`run.rs` calls `StromClient` directly; `local_strom` starts or adopts the Strom process). On
   iOS there is no Strom: an app cannot start another process, and Strom on GStreamer is a server,
   not an iOS component.
2. **How Swift calls it.** The library is Rust and async (Tokio).
3. **What does the media on the phone**: capture, hardware H.264 and AAC, MPEG-TS, SRT, with the
   desktop's SRT settings (latency, passphrase, key length), CBR, and SRT stats (dropped share,
   RTT) for the desktop's Link line. #133 names HaishinKit.swift as "the likely base; to be
   confirmed in a spike".

Constraints:

- The dependency rule (CTO, 2026-10-22): an external library only when needed; a yes needs a
  written reason with the fraction used, the transitive count and the maintenance state. SRT is
  protocol handling, the passphrase is crypto: sensitive areas, where a well-maintained library
  stays and crypto is never reimplemented.
- The repo's rules: the library never prints, exits or handles signals
  (`tests/library_boundary.rs`); Open Live being unreachable never disturbs the feed; credentials
  and passphrases never reach logs.
- `cargo check`, `clippy` and `test` at the root run anywhere with no system libraries; the iOS
  work must not change that (as ADR-001 kept it for the desktop).
- Build tooling runs in CI next to signing secrets, so it counts as a dependency too.

### Facts gathered (2026-11-22)

- **HaishinKit.swift** 2.3.1 (released 2026-11-21, BSD-3-Clause, active: pushed 2026-11-22).
  Its `Package.swift` declares 6 package dependencies: `HaishinKit/libaom-xcframework`,
  `HaishinKit/libvpx-xcframework`, `HaishinKit/libdatachannel-xcframework`,
  `HaishinKit/libsrt-xcframework`, `swiftlang/swift-docc-plugin`, `shogo4405/Logboard`. SwiftPM
  resolves every declared package dependency even when only the `SRTHaishinKit` product is used.
- **`HaishinKit/libsrt-xcframework`**: its manifest is a `binaryTarget` (`libsrt.xcframework`,
  prebuilt) plus `krzyzanowskim/OpenSSL-Package` (a second prebuilt binary). GitHub detects no
  licence file in the repository. So the SRT and crypto code a HaishinKit-based app ships is two
  binaries built by third parties, not from source we can review or rebuild.
- **Capability**: `SRTHaishinKit/Sources/SRT/SRTSocket.swift` sets SRT socket options and reads
  `srt_bstats` into `SRTPerformanceData`. Latency, passphrase and stats would be reachable;
  capability is not what decides this.
- **The library on iOS**: its tree is pure Rust over `reqwest` with `rustls-tls`,
  `tokio-tungstenite` with `rustls-tls-webpki-roots`, `tokio`, `serde`; `aarch64-apple-ios` and
  `aarch64-apple-ios-sim` are tier-2 Rust targets with `std`. Two `cfg`s need widening:
  `security-framework` is `target_os = "macos"` only, and `config::user_dir()` sends iOS to the
  XDG branch. `local_strom` compiles on iOS but is never called there. The pidfile code is
  not: `run::up` calls `running_gateway()` and `Pidfile::write` on every run (`run.rs`), before
  any Strom is touched, so it moves behind the seam (§1).
- **The desktop app already defines a message protocol** between a UI and the Rust core
  (`docs/specs/desktop-app.md` §9.1: `{id, cmd, args}` / `{id, ok | error}` / `{event, data}`),
  driven over stdin/stdout too (`--ipc-stdio`, §9.3a). It is the natural shape for a second UI.

## Decision

### 1. The library on iOS, with a media seam

The iOS app links this repo's library, built for `aarch64-apple-ios` (device) and
`aarch64-apple-ios-sim` (simulator, for CI). Everything that is not media is the desktop's code,
unchanged: `osc`, `credential`, `keychain`, `openlive`, `gate`, `heartbeat`, `session`, `config`.

`run::up` stops calling `StromClient` directly and becomes generic over a `Media` trait with seven
operations (version, input resolution, start, stop, restart, running, uplink stats; spec §4),
each declared `fn op(&self, ..) -> impl Future<Output = ..> + Send` so `run::up` stays spawnable
and clippy-clean without `async-trait`. `StromMedia` is today's code moved behind it, behaviour
unchanged and pinned by the existing golden and session tests: its `resolve_inputs` runs today's
device code (`video_devices`, `audio_devices`, `devices::choose_saved` / `choose` / `pair_audio`;
`devices::inputs_for` stays shared, after the cloud's ports are known), and its constructor does the single-instance pidfile check and write that `run::up`
does today (`running_gateway()`, `Pidfile::write`), so the CLI and the desktop app behave as
before and the phone writes no pidfile. `ExternalMedia` resolves inputs from the settings'
`[[input]]` as Swift picked them, hands start / stop / restart to the host over its own channel,
and answers state and stats from what the host reports. The seam carries `strom-types`'
`SrtCallerStats`, so stall detection and the Link line are shared too.

The repo rule "Strom owns the media plane; the gateway owns no media" is amended to: **the library
owns no media**. On a computer Strom does it, as before, behind the same process boundary; on the
phone the iOS app's Swift code does it over the OS frameworks. The rule "never link the Strom
engine" is unchanged.

### 2. A hand-written C ABI carrying the desktop's JSON messages

A new workspace member `mobile/` (`open-live-ingest-mobile`, `staticlib`, not a default member)
exports four `extern "C"` functions (`oli_core_new`, `oli_core_send`, `oli_core_free`,
`oli_string_free`) and one callback type, declared in a hand-written header. Commands, replies
and events are the desktop's JSON envelope; the command set is the phone's (spec §8.2). Swift
decodes them with `Codable`. Panics are caught at the boundary; nothing unwinds into Swift.

### 3. Own media code over Apple's frameworks, and libsrt built from source

- **Capture and encode: the OS.** `AVCaptureSession`, VideoToolbox (`VTCompressionSession`, H.264,
  CBR) and AudioToolbox (`AVAudioConverter`, AAC-LC). These are Apple's, signed with the OS; no
  library is needed to call them.
- **MPEG-TS: own Swift code** (`TSMuxer.swift`): one program, H.264 and AAC elementary streams,
  PAT/PMT, PES, PCR, continuity counters. A few hundred lines of well-specified, deterministic code
  that a unit test can pin byte for byte and `ffprobe` can check in CI.
- **SRT: libsrt, built from source** at a pinned release with **mbedTLS** (pinned LTS) as its
  crypto, by a script in the repo (`ios/Vendor/build-libsrt.sh`), digest-checked, into an
  `SRT.xcframework` for device and simulator, built in CI and never committed. A thin
  `SRTSender.swift` sets the options and reads `srt_bstats`. The SRT protocol and its crypto are
  exactly where a well-maintained library stays, and libsrt is the implementation the cloud side's
  GStreamer SRT elements use.

### 4. Sign-in

The library's PKCE loopback sign-in, unchanged, with the page shown in
`ASWebAuthenticationSession`; personal access token paste as the fallback (spec §6).

### 5. HaishinKit is not added

The fraction of HaishinKit we would use is large on the SRT path (its capture mixer, codecs, TS
writer and SRT wrapper), so "use one function, write that function" does not apply on its face.
It is rejected on the supply chain instead: it brings at least 7 packages into resolution
(6 declared plus OpenSSL-Package), of which the two we would actually execute for SRT, libsrt and
OpenSSL, are prebuilt binaries from third-party repositories, one with no detected licence file,
that we cannot rebuild without forking two repositories. Taking the parts we need from source means
writing what HaishinKit's Swift layer does for us (capture glue, encoder setup, TS muxer: own code
under the rule's "prefer writing own code") and building the one hard part, libsrt, ourselves.

## Consequences

**Positive**:

- One implementation of sign-in, registration, adoption (#115), the gate (#130) and the status
  lines for the desktop and the phone; a fix to any of them reaches both.
- No new Rust crate and no Swift package. The SRT and crypto code in the app is built from pinned,
  digest-checked source in our CI.
- The phone's SRT stack is the same libsrt as the cloud's receiver, with the options set directly,
  and its stats are libsrt's own counters, read the way the library already interprets them.
- The Swift/Rust boundary is four functions and a message format the desktop already tests.

**Negative**:

- More own Swift code than with HaishinKit: capture and audio-session handling, encoder setup,
  the TS muxer, interruption handling. Estimated 1,500–2,500 lines with tests.
- A CMake cross-build of libsrt and mbedTLS to maintain, and a version to bump on purpose (as
  `strom-types` is).
- The `run.rs` refactor touches the session loop the desktop depends on; it is guarded by the
  existing tests passing unchanged.
- The iOS CI job needs a macOS runner and adds minutes to every PR that touches the library,
  `mobile/` or `ios/`.

**Risks**:

- *libsrt or mbedTLS will not cross-build cleanly for the simulator slice.* Detected by OL-64c's
  CI. Fallback: HaishinKit's `SRTHaishinKit` with this ADR superseded, after recording which
  binaries it brings at that version.
- *VideoToolbox CBR overshoots* on scene changes and the SRT send buffer drops. Detected by the
  drop share in OL-64h and the peak-to-average check in OL-64d; mitigated with `DataRateLimits`
  and SRT input bandwidth headroom (spec §11).
- *The loopback redirect does not complete inside `ASWebAuthenticationSession`.* Detected first in
  OL-64b; PAT paste always works, a custom-scheme redirect is the second fallback (spec §6).
- *The `Media` seam leaks Strom concepts* (flow ids) into the phone path. Detected in review of
  OL-64a; flow ids stay inside `StromMedia`.

## Alternatives Considered

- **HaishinKit.swift (`HaishinKit` + `SRTHaishinKit`) as the media base.** Fastest to a first
  picture and actively maintained, with SRT options and stats exposed. Rejected for the prebuilt
  libsrt and OpenSSL binaries and the unused packages its manifest resolves (§5). Kept as the
  named fallback.
- **Larix SDK or another commercial streaming SDK.** Closed source and licensed per app; not
  open-source-compatible for a free product. Rejected.
- **GStreamer for iOS (the Strom pipeline on the phone).** Strom's own elements, but the GStreamer
  iOS framework is a very large static binary with LGPL static-linking obligations awkward for
  App Store distribution, and still no Strom process (iOS cannot run one). Rejected.
- **A pure-Rust SRT implementation (`srt-tokio`)** so the media sender lives in the library too.
  Less maintained than libsrt, not the cloud side's implementation, and the capture and encode
  would still be Swift. Rejected.
- **UniFFI (or swift-bridge) for the binding.** Generates typed Swift for every exported function,
  at the cost of a proc-macro and bindgen stack in the build (`uniffi_bindgen`, `askama`,
  `goblin`, `camino`, `cargo_metadata` and more) for a surface of four functions carrying JSON
  the desktop already defines. Rejected under the dependency rule.
- **Reimplementing the shared logic in Swift.** No new dependency, but a second copy of the OSC
  sign-in, refresh, adoption, port planning and the gate, which #133 item 1 rules out and which
  would drift from the desktop's. Rejected.
- **WHIP/WebRTC instead of SRT** (as Eyevinn/open-live#382's guest page does). #133 asks for SRT,
  like the desktop app; a WHIP phone sender is a different product decision. Not considered
  further here.
