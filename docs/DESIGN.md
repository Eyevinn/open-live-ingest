# Open Live Gateway — Design

> Pre-implementation design document. Once the code exists, **the code is the source of truth** —
> this file records the decisions and the reasoning behind them, not the current implementation.
> It will drift; read the code for what the gateway actually does.

## 1. What this is

A control agent for a venue-side [Strom](https://github.com/Eyevinn/strom) instance. Strom runs on
the same Linux box and does all the media work — SDI capture, encode, MPEG-TS mux, SRT uplink. The
gateway templates the flow, keeps it running, and registers the feed with the
[Open Live](https://github.com/Eyevinn/open-live) API so operators can assign it to a mixer input
in [Open Live Studio](https://github.com/Eyevinn/open-live-studio) without touching config files.

```
      VENUE                                          CLOUD
 ┌────────────────────────┐                    ┌──────────────────────────────┐
 │ SDI ──> Strom (local)  │───MPEG-TS/SRT────> │ Strom (cloud)                │
 │          │             │   caller -> listener│  builtin.mpegtssrt_input    │
 │          │ REST        │                    │       │                      │
 │          v             │                    │       v  vision/audio mixer  │
 │      gateway agent ────┼───HTTPS──────────> │  Open Live API ──> Studio    │
 └────────────────────────┘   register source  └──────────────────────────────┘
```

**Media goes venue Strom → cloud Strom directly, never through Open Live.** Open Live is an
orchestration API (Fastify + CouchDB) that drives Strom over REST and WebSocket; it has no media
plane. Its one media-adjacent route proxies WHIP *signaling*, and even then the RTP flows to Strom.

**The agent talks to both sides, and that is the whole design.** Local Strom over REST to create
and start the flow; Open Live over HTTPS to register the source.

### Why the agent, and not Open Live driving the venue Strom

Three reasons, and the second is decisive:

- Open Live has a single `STROM_URL`. One engine per instance, so orchestrating a second Strom is a
  real feature change upstream.
- Open Live would have to call *into* the venue box. Behind venue NAT that is dead on arrival
  without a reverse tunnel. The gateway inverts the direction: everything is outbound.
- Autonomy. A WAN drop would leave the venue with no local control at all.

### The invariant

**Open Live being unreachable must never disturb the feed.** Registration failures are logged and
retried; they never stop or restart a flow. The same holds in the other direction — if the agent
itself dies, Strom keeps pushing, because the agent is not in the media path.

## 2. Why the venue encodes

SDI is uncompressed baseband — SMPTE 292M/424M, ~1.5/3.0 Gbps of 10-bit 4:2:2 YUV with embedded PCM
audio. There is no bitstream to repackage, so a pure relay (the mediamtx model) cannot apply. The
same holds for USB/UVC capture, which presents raw or MJPEG frames over V4L2.

A relay would only apply if venue devices already emitted H.264/H.265 over RTSP/RTMP/SRT. That is
not the target: the inputs are SDI.

## 3. What the gateway deliberately does not do

Strom already does these, better, and duplicating them would mean maintaining a worse copy:

| Concern | Owned by |
|---|---|
| Encoder selection | `builtin.videoenc` — hardware first across NVENC, QSV, VA-API, VideoToolbox, AMF, and V4L2, with GStreamer rank checks and per-element property normalisation |
| SRT reconnect | `builtin.mpegtssrt_output` — `auto_reconnect` defaults to true, so the block re-dials the cloud itself |
| Capture device handling | `builtin.decklink_input`, `builtin.local_input` |
| Device discovery, pipeline graphs, stats, field debugging | Strom's own web UI and API |

So the gateway expresses *intent* — `encoder_preference = "auto"`, a bitrate, a GOP length — and
lets Strom decide how to satisfy it. A transient SRT drop is likewise not the agent's business:
restarting a flow over a network blip would turn a recoverable gap into an encoder restart.

## 4. The flow the agent builds

One flow per input, so one camera failing cannot disturb another.

```
  builtin.decklink_input          builtin.videoenc          builtin.mpegtssrt_output
  (or builtin.local_input)
       video_out  ───────────>  video_in    encoded_out ───────────>  video_in
       audio_out  ─────────────────────────────────────────────────>  audio_in_0
```

Audio bypasses the encoder: `builtin.videoenc` is video-only, and the MPEG-TS output block
auto-encodes raw audio to AAC. Declaring an audio track the flow does not actually deliver would
leave that block waiting on a pad that never produces, stalling the pipeline — so the track count
follows the capture configuration.

**Flow ids are derived, not stored.** A UUIDv5 over `gateway id + input id` means the agent finds
its own flow after a reboot with no local state, and two gateways never collide. Consequence worth
knowing: changing `gateway.id` orphans the flows already created.

**Block ids and property names are Strom's.** The gateway talks to Strom over HTTP rather than
linking its crates, so those names live here as string literals — a rename upstream surfaces as a
runtime error from Strom, not a compile error. `GET /api/blocks` on the target Strom is the
authority; the block reference in Strom's docs may lag it.

## 5. Uplink addressing — the two-URI problem

One link, two URIs, and conflating them is the easiest way to lose an afternoon:

| Side | URI | Meaning |
|---|---|---|
| Venue (local Strom's uplink block) | `srt://strom.example.com:9000?mode=caller` | Local Strom dials out |
| Cloud (stored on the Open Live source) | `srt://:9000?mode=listener` | Cloud Strom binds UDP 9000 and waits |

The gateway config holds the public cloud host and port, puts the caller form on the uplink block,
and *derives* the listener form it registers with Open Live. Open Live's source validation
explicitly permits the hostless listener form, so this works against the API as it stands.

Consequences to design around:

- **The caller direction is deliberate.** The venue needs no inbound firewall rule and no static IP.
- **Ports must be unique per input across every gateway** pointing at one cloud Strom, and must be
  opened on the cloud firewall — Strom's media-plane ports are per-flow, not part of its control
  port. The gateway enforces local uniqueness only; fleet-wide is the operator's problem today (§8).
- **SRT `latency` should be 3–4× the measured RTT.** The gateway writes its configured value into
  the source's `latency` field so the cloud receiver matches; Open Live takes the maximum across a
  production's sources when generating the flow.
- **The passphrase is owned by the gateway.** Open Live encrypts it at rest and masks it on read, so
  the gateway never tries to read it back — it sets the same value on both URIs.

## 6. Control plane

**Flow supervision.** One task per input, reconciling desired state against what Strom reports:
create the flow if absent, update it if the config changed, start it if it is not running. The
reconcile is idempotent and runs on a timer rather than once at startup, because **Strom does not
auto-start flows on boot** — there is no `auto_start` in its code, only in a stale doc example. That
loop is what brings a venue back on air after a power cut, and it is the clearest single
justification for the agent existing at all.

Failures back off (1 s → 30 s). An unreachable Strom moves the input to `Unknown`, not `Failed`:
the agent losing contact is not evidence that the feed stopped.

**Registration is idempotent.** The Open Live source id is resolved from a local state file, then
patched if a field drifted, or created if absent. Persisting it is what stops every reboot from
leaving another orphaned source in Studio's list. A remembered id that has since been deleted in
Studio falls through to creating a fresh one rather than failing forever.

**Status reconciliation.** The source `status` (`active`/`inactive`) is reconciled against flow
state every ~10 s. That field is the only health channel the current Open Live API offers, so
richer telemetry is exposed locally instead, until Open Live grows somewhere to put it (§8).

**Local control API** (axum, loopback by default; a non-loopback bind requires a token, enforced at
startup rather than at first request):

| Method | Path | Purpose |
|---|---|---|
| `GET` | `/healthz` | Liveness, unauthenticated |
| `GET` | `/api/v1/status` | Per-input state, flow id, GStreamer state, registered listener address |
| `POST` | `/api/v1/inputs/:id/start` · `/stop` | Manual control |
| `GET` | `/metrics` | Prometheus exposition |

There is no device-discovery endpoint: Strom's own API and UI already do that, on the same box.

## 7. Configuration and operations

TOML file, then environment overrides, then CLI flags — the same precedence Strom uses, so
operators moving between the two are not surprised. Config section and property names mirror the
Strom blocks they configure, so a value can be traced straight to the block it lands on.

Secrets (Open Live API key, Strom API key, SRT passphrase) live in the config file at mode 0600 and
are redacted from logs. Errors are built from HTTP status codes, never from request or response
bodies — a flow body echoes block properties, and those carry the passphrase.

Deployment is a systemd unit with `Restart=always` and the usual hardening. The gateway needs no
device access of its own: Strom holds the DeckLink and V4L2 devices, so the agent is an ordinary
unprivileged HTTP client. Target is any x86_64 or aarch64 Linux host that can run Strom.

## 8. Delivery phases

1. **MVP** — flow templating for DeckLink, USB/UVC, and test capture; create/update/start
   supervision; self-registration with Open Live; systemd unit; `/healthz` and `/api/v1/status`.
2. **Field-ready** — `/metrics`, SRT statistics read from Strom's stats API, manual start/stop,
   log redaction, and surfacing Strom's own flow errors rather than just "not running".
3. **Fleet** — EFP uplink for multi-track audio (Open Live already accepts `efp` sources and Strom
   has `builtin.efpsrt_input`/`_output`), remote provisioning where the gateway pulls its own config
   from Open Live, and a fleet view in Studio. Return feed and tally back to the venue — WHEP from
   the cloud into a local monitor, which the venue Strom can already receive — also sits here.

## 9. Open questions

- **Who allocates SRT ports?** Config-explicit today, which makes fleet-wide collisions an operator
  footgun. An allocation endpoint in Open Live would remove it.
- **Should Open Live gain a gateway or heartbeat resource?** A source's `status` is currently the
  only channel, and there is nowhere to put telemetry, a version, or a last-seen timestamp.
- **Should the agent own Strom's lifecycle** (start it, wait for it, restart it), or only assume it
  is there? Today it assumes, and systemd orders them.
- **EFP or MPEG-TS for contribution?** EFP carries multi-track audio and both ends already support
  it; MPEG-TS is the interoperable default and is what phase 1 ships.
