# ADR-003: Desktop preview of picture and sound through Strom's thumbnail and meter blocks

**Date**: 2026-11-25
**Status**: Proposed

Spec: [`docs/specs/desktop-app.md`](../specs/desktop-app.md) §4.1, §5.2, §12. Issue: #172 (OL-70).
Builds on [ADR-001](ADR-001-desktop-app-toolkit.md).

## Context

#172 asks the desktop app to show, for each ticked row in step 2 and for each input while live, a
picture of the camera about once a second and a level for its microphone, with a warning when a
microphone gives no sound. The preview must run only while it is shown or the input is live (no
camera light with nothing shown or sent), and must keep the boundary the app already has: the
webview never talks to Strom or the camera (spec §4, §9.4, `connect-src 'none'`).

Constraints:

- **The camera has one user at a time.** Media Foundation on Windows gives a camera to one process;
  AVFoundation shares, but strom#1278 shows a stop and start of one flow on macOS can already open the
  wrong device. Since #130, Go live only arms and the uplink flow opens the camera when a production
  goes active, so before that nothing has the camera open.
- **The app links no GStreamer** (`DESIGN.md` §4, ADR-001): capture is Strom's.
- **Strom v0.8.4** (the pin, `Cargo.toml`) has `builtin.thumbnail`, a passthrough `tee` that attaches
  a JPEG branch only when `GET /api/flows/{id}/blocks/{block_id}/thumbnail` is called (320×180, one
  new frame a second, detached after 10 s idle, leaky one-buffer queue), and `builtin.meter`,
  GStreamer `level` in line, whose values exist only as `MeterData` events on Strom's WebSocket
  `/api/ws`. There is no REST read for levels.
- Before #172, the app started Strom only to list devices and stopped it again; the session started
  (or adopted) its own Strom at Go live, after reclaiming one a crash left behind.
- Standing dependency rule (CTO, 2026-10-22): no new crate without a written reason.

The decisions: where the picture and the level come from, how the camera passes from the preview to
the uplink, and how they reach the page.

## Decision

1. **Picture and level come from Strom's own blocks.** In step 2, one short-lived **preview flow**
   per ticked row: capture (the uplink's capture properties) → `builtin.thumbnail` → `fakesink`, and
   → `builtin.meter` → `fakesink`. While live, the same two blocks are **taps inside the uplink flow**
   (between capture and encoder, between capture and the TS output's audio input), built only by the
   app (`Taps::On`); the CLI's `up` and the iPhone app build today's flow.
2. **The camera is handed over, never shared.** The app core holds the Strom from step 2
   (`StromHold`) and hands that same Strom to the session at Go live, in this order: remove every
   preview flow and wait until Strom answers 404 for each; start the session with the hold; the
   uplink flow (with taps) opens the camera when a production goes active. The crash reclaim moves to
   the moment the hold is taken.
3. **The core reads Strom; the page gets data over the existing IPC.** Pictures: the core polls the
   thumbnail route (250 ms until the first, then 1 s) and sends `data:image/jpeg;base64,…` in a
   `preview` event (the CSP already allows `img-src data:`). Levels: the core keeps one WebSocket to
   the local Strom's `/api/ws`, decodes only `MeterData` into its own struct, and sends the per-window
   maximum at 4 Hz in a `level` event. Silence (below -60 dBFS for 5 s) is decided in the core.
4. **An older Strom degrades to no preview.** `GET /api/blocks` at hold time; without both blocks no
   preview flows and no taps, and going live is unaffected.
5. **Dependencies: +0 crates.** The WebSocket uses the `tokio-tungstenite` the heartbeat already
   uses; `base64` 0.22 becomes a direct dependency of the app, already in the lockfile through
   `reqwest` and `strom-types`.

## Consequences

**Positive**: No camera is ever open in two places, so the same design works on macOS and Windows.
What the row shows in step 3 is taken from the flow that sends, so a picture or sound that stops
going out is seen in the window. The webview boundary and the CSP are unchanged. No new crate, no new
setting, nothing stored. The CLI's flow is byte-identical.

**Negative**: The library gains a second way to start a session (with a held Strom) and the app a
long-lived Strom while step 2 is shown. The uplink flow of the app gains two in-line elements.
Strom's WebSocket becomes a dependency of the app core, and it is not a versioned API. After Stop the
preview starts a fresh Strom (a few seconds), because the session does not hand it back.

**Risks**: strom#1278 (wrong camera on a second start) becomes likelier, since every Go live now
stops and starts a camera: detected by reading `capture:videosrc`'s `device-name` after each start
and warning on the row. A tee before the encoder could change zero-copy into a hardware encoder:
detected by OL-70e's drop and CPU figures (≤ 5 % of one core per input while live, AC5 unchanged),
with taps off for video as the fallback. A device released late by the OS at the handover: with
registration on, the uplink starts seconds later anyway; otherwise the existing camera-busy retry.
A later Strom changing `MeterData`: the reader ignores what it cannot decode (no levels, no failure)
and the pin moves on purpose.

## Alternatives Considered

- **Native capture in the app (AVFoundation / Media Foundation) for the preview.** Rejected: the
  camera would be opened by the app and by Strom, which Windows refuses and macOS makes fragile; it
  adds per-OS capture code next to Strom's, and the preview would show what the app captured, not
  what Strom sends.
- **WebRTC (WHEP) from Strom into the webview.** Rejected: the page would talk to Strom
  (`connect-src` opened, against spec §4 and §9.4), it costs an encoder per input for a picture a
  second, and #172 asks for checking, not watching.
- **One capture flow per camera feeding both preview and uplink through Strom's inter blocks.**
  Rejected for now: it keeps the camera open across Go live with no handover at all, but changes the
  uplink's shape more (two flows per input, ordering of their lifecycles, the stall restart of #127
  across two flows), and keeps the camera open while armed, against #172 item 3 and #130.
- **Taps as separate flows while live (no change to the uplink flow).** Rejected: a second flow on
  the same camera is exactly the sharing Windows refuses.
- **Levels by polling element properties instead of the WebSocket.** Not possible: `level` posts its
  values as bus messages; it has no readable property for them, and Strom offers no REST read.
- **Pictures through a dynamic route on the app's `oli` protocol instead of `data:` URLs.** Rejected:
  the protocol handler would grow from three compiled-in files to a live route, for a saving of about
  a third on 10–20 KB a second per row.
