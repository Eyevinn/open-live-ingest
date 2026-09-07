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

## 5. Uplink addressing — which end dials

One link, two URIs that mirror each other, and conflating them is the easiest way to lose an
afternoon. What the pair looks like depends on `uplink.mode`, and that choice is a deployment
constraint rather than a preference: **whichever end listens is the end that needs an inbound UDP
port.**

| Mode | Venue output block | Registered on the Open Live source | Needs an inbound port |
|---|---|---|---|
| `caller` (default) | `srt://cloud:9000?mode=caller` | `srt://:9000?mode=listener` | the cloud |
| `listener` | `srt://:9000?mode=listener` | `srt://venue:9000?mode=caller` | the venue |
| `rendezvous` | `srt://cloud:9000?mode=rendezvous` | `srt://venue:9000?mode=rendezvous` | neither |

A subtlety that decides how an address is read: **`srtsrc` defaults to caller mode.** So a source
address carrying no `mode=` makes the cloud Strom *dial out* — which is exactly what Open Live's
own seeded demo sources (`srt://127.0.0.1:5010`) rely on, dialling something inside the cloud host
itself. Reading such an address as "listen on 5010" is wrong, and that reading is what makes
`caller` look like the obvious default when it is only one of three.

Consequences to design around:

- **`caller` keeps the venue free of inbound rules and static IPs**, which is what makes a NAT'd
  venue deployable — but it moves the requirement to the cloud, and a cloud Strom behind an
  HTTP-only reverse proxy cannot satisfy it. That is not hypothetical: it is what an OSC-hosted
  Strom looks like.
- **`listener` matches the convention Open Live's seeded sources use** and asks nothing of the
  cloud, at the cost of a public address or port forward at the venue. For a fixed installation
  that is normal; for a laptop on a corporate network it is not.
- **Ports must be unique** per input across every gateway pointing at one Strom, on whichever side
  is listening, and open on that side's firewall.
- **SRT `latency` should be 3–4x the measured RTT.** The gateway writes its configured value into
  the source's `latency` field so the cloud receiver matches; Open Live takes the maximum across a
  production's sources when generating the flow.
- **The passphrase is owned by the gateway.** Open Live encrypts it at rest and masks it on read,
  so the gateway never reads it back — it puts the same value on both URIs.

## 5a. Two front ends

The same core serves two deployment models, and they differ in who owns an input's lifetime.

The **headless daemon** takes its inputs from the config file and runs unattended under systemd.
Inputs outlive the process, so everything above — idempotent registration, derived flow ids, drift
reconciliation — exists to converge on a declared state no matter what happened before.

The **desktop app** inverts that: an input exists because an operator picked a device, and closing
the window ends it. That makes the lifecycle simpler rather than harder. The app owns each flow
outright and deletes it on the way out, so there is nothing to reconcile against next time, and no
control API is needed because the UI reads `SharedState` in-process.

Two things it still has to handle, because a window can be closed the hard way:

- **Orphans from a crash.** Derived flow ids mean a fresh start can find and delete its own
  leftovers with no stored state to consult.
- **Ports.** An operator picking a camera cannot be asked to choose a UDP port, so one is allocated
  per input from `[app.uplink] port_range`, skipping ports that config-declared inputs claimed.

Input ids are derived from the device id, so picking the same camera after a restart addresses the
same flow and the same Open Live source instead of accumulating a duplicate per session.

**Two Stroms, one of them Open Live's business.** The venue box runs its own Strom to capture and
encode; the cloud runs the Strom that Open Live drives. Only the second is discoverable: Open Live
reports its hostname from `GET /api/v1/server-info`, so the app can be configured with nothing but
the Open Live address and its credential — the local Strom defaults to loopback because it lives on
the same machine. What still cannot be discovered is the SRT port, since nothing allocates them
(§9), and the uplink direction, which depends on which side can publish a port.

The app is explicitly **not** a venue appliance: no unattended recovery, nothing under systemd. A
venue box that must come back by itself after a power cut runs the headless binary.

## 6. Control plane

**Flow supervision.** One task per input, reconciling desired state against what Strom reports:
create the flow if absent, update it if the config changed, start it if it is not running. The
reconcile is idempotent and runs on a timer rather than once at startup.

Note what this does *not* have to solve: Strom restarts flows itself on boot. `auto_restart` is set
on a flow when it is started and cleared when it is manually stopped, and at startup Strom starts
every flagged flow unless run with `--no-auto-restart`. So a power cut on a box whose flow was
already running recovers without the agent.

What the agent is actually for, then: **provisioning** — the flow does not exist until something
creates it, and auto-restart only helps flows that already do; **config drift** — a changed bitrate,
port, or passphrase is written on the next reconcile, and the flow is restarted so it
actually takes effect, since Strom's flow update rewrites stored data only and a
running pipeline keeps the properties it was built with; **recovery of a flow that fails while
running**, which boot-time auto-restart does not cover; and **fleet-scale templating**, so a venue
box is described by a config file rather than built by hand in Strom's editor.

Failures back off (1 s → 30 s). An unreachable Strom moves the input to `Unknown`, not `Failed`:
the agent losing contact is not evidence that the feed stopped.

**Registration is idempotent.** The Open Live source id is resolved from a local state file, then
patched if a field drifted, or created if absent. Persisting it is what stops every reboot from
leaving another orphaned source in Studio's list. A remembered id that has since been deleted in
Studio falls through to creating a fresh one rather than failing forever.

**Status reconciliation.** The source `status` (`active`/`inactive`) is reconciled every ~10 s
against whether the uplink is *delivering*, not merely whether the local flow runs. The
distinction is not cosmetic: a source assigned to a production whose feed never arrives stops the
cloud flow from reaching playing at all, so Open Live never publishes the WHEP endpoints and the
whole show fails to come up — one dead input is not degraded gracefully, it takes everything with
it. `active` therefore has to mean "safe to assign".

Delivery is judged from Strom's `/api/flows/{id}/srt-stats`, by `bytes_sent` *changing* between
polls. Strom's own `connected` flag is not usable for this, and neither is a bare increase:

- When the far end vanishes, `srtsink` keeps a stale caller entry reporting `connected: true`
  while it retries, with every metric beside it null.
- Worse, an SRT socket can sit at `connected: true` with a frozen `bytes_sent` and a decaying
  `send_rate` indefinitely — a dead feed that never heals itself. Seen in practice after the far
  end went away and came back. Rebuilding the flow clears it, so a stalled uplink is restarted
  rather than left dead.
- A flow restart resets the counter, so *change* rather than growth is the test; requiring growth
  would treat a healthy new connection as stalled until it passed the previous total.

Three consecutive dead polls are required before acting. That hysteresis is not timidity: each
status flip rewrites the Open Live source, and CouchDB keeps a revision per write, so a flapping
verdict pollutes the document's history as well as misleading the operator.

**A restart is not free.** Dropping the SRT connection makes a cloud production consuming the feed
see its input disappear, and Strom's own troubleshooting notes record where that leads: the
receiving `tsdemux` tears down its program, pushes EOS, `h264parse` posts a fatal error, and the
production's `whepserversink` never opens its port — answering 502 to every player for the rest of
that flow's life. The gateway restarts only a feed that is already dead, where there is nothing
left to protect, but the interaction is worth knowing before adding any other restart trigger. That field is the only health channel the current Open Live API offers, so
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
