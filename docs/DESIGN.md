# Open Live Gateway: Design

> The code is the source of truth. This file records the decisions and the reasoning behind them,
> not the current implementation.

## 1. What this is

A command line tool that drives a venue-side [Strom](https://github.com/Eyevinn/strom). Strom runs
on the same machine and does all the media work: capture, encode, MPEG-TS mux, SRT uplink. The
gateway builds one flow per camera, keeps it running, and registers the feed with the
[Open Live](https://github.com/Eyevinn/open-live) API so operators can assign it to a mixer input in
[Open Live Studio](https://github.com/Eyevinn/open-live-studio).

```
      VENUE                                          CLOUD
 ┌────────────────────────┐                    ┌──────────────────────────────┐
 │ camera ──> Strom       │───MPEG-TS/SRT────> │ Strom (cloud)                │
 │            ▲           │                    │  builtin.mpegtssrt_input     │
 │            │ REST      │                    │       │                      │
 │        gateway ────────┼───HTTPS──────────> │  Open Live API ──> Studio    │
 └────────────────────────┘   register source  └──────────────────────────────┘
```

**Media goes venue Strom to cloud Strom directly, never through Open Live.** Open Live is an
orchestration API with no media plane.

**The gateway talks to both sides, and that is the whole design.** Local Strom over REST to create
and start the flow; Open Live over HTTPS to register the source.

### Why a venue-side tool, and not Open Live driving the venue Strom

- Open Live has a single `STROM_URL`. One engine per instance.
- Open Live would have to call *into* the venue box. Behind venue NAT that needs a reverse tunnel.
  The gateway inverts the direction: everything is outbound.

### The invariant

**Open Live being unreachable must never disturb the feed.** Registration failures are logged and
retried on the next tick; they never stop or restart a flow.

## 2. Why the venue encodes

SDI and USB capture deliver uncompressed frames. There is no bitstream to relay, so the venue has to
encode, and Strom already does that well.

## 3. What the gateway deliberately does not do

Strom already does these, and a worse copy here would have to be maintained:

| Concern | Owned by |
|---|---|
| Encoder selection | `builtin.videoenc`, hardware first |
| SRT reconnect | `builtin.mpegtssrt_output`, `auto_reconnect` on by default |
| Capture device handling and discovery | `builtin.local_input`, `GET /api/discovery/devices` |
| Pipeline graphs, stats, field debugging | Strom's own UI and API |

The gateway expresses intent, a bitrate and an encoder preference, and lets Strom satisfy it. A
transient SRT drop is likewise not its business: restarting a flow over a network blip would turn a
recoverable gap into an encoder restart.

**A Strom bug is fixed in Strom.** An earlier version restarted a flow whose SRT socket had frozen
at `connected` with no bytes moving. That papered over an srtsink fault, and the restart trigger
misfired on the far more common case of a feed that nobody is receiving yet, restarting the camera
and encoder every 40 seconds. The frozen socket belongs upstream.

## 4. The flow

One flow per input, so one camera failing cannot disturb another.

```
  builtin.local_input ── video_out ──> builtin.videoenc ── encoded_out ──> builtin.mpegtssrt_output
```

Video only for devices: a webcam's microphone is a separate device on its own clock and drifts
against the video over a long show. Embedded audio means SDI, and the audio track count follows the
source, because declaring a track the flow never delivers stalls the pipeline on a pad that never
produces. The test pattern carries a tone so a link can be commissioned end to end.

**Flow ids are derived, not stored.** A UUIDv5 over `gateway id + input id`, where the input id is
derived from Strom's device id. A flow is ours if and only if its id is the one we would derive for
the device it captures. That gives `down` and `status` a precise ownership test with no local state
and no naming convention, and a flow someone built by hand on the same camera is never touched.
Changing `gateway.id` hides the flows already created.

**Block ids and property names are Strom's**, sent as strings over HTTP, so a rename upstream fails
at runtime rather than at compile time. `GET /api/blocks` on a live Strom is the authority.

## 5. Which end dials

One link, two URIs that mirror each other. The choice is a deployment constraint: **whichever end
listens is the end that needs an inbound UDP port.**

| Mode | Venue output block | Registered on the source | Needs an inbound port |
|---|---|---|---|
| `caller` (default) | `srt://cloud:9000?mode=caller` | `srt://:9000?mode=listener` | the cloud |
| `listener` | `srt://:9000?mode=listener` | `srt://venue:9000?mode=caller` | the venue |

`srtsrc` defaults to caller mode, so the registered address always carries an explicit `mode=`.
Ports are allocated per input from a range, lowest first in device-name order, so the same cameras
land on the same ports across runs and the registered addresses stay stable. The passphrase is owned
by the gateway: Open Live masks it on read, so it is never read back and is compared masked.
Rendezvous mode was removed; nobody used it, and it doubled the URI logic.

## 6. The command line owns what it starts

There is one front end and it is a terminal, because the machine is reached over SSH.

**Setup is running it.** Anything missing is asked for, checked immediately, and written to the
settings file. The Open Live credential kind is inferred from the address, since nobody can answer
"osc or direct" from those words, and the local Strom's key is asked for only when Strom answers
401, because "wants a credential" and "cannot be reached" send an operator to different places.

**`up` stays in the foreground and owns what it started.** Ctrl-C and SIGTERM tear the feeds down.
SIGHUP does not: a closed SSH session must not take a venue off air. A deliberate stop stops, an
accident does not. This replaced an earlier daemon model in which flows were meant to outlive the
agent and be reconciled forever from a config file. Carrying both models meant drift detection,
per-input supervisor tasks, a persisted state file, and a control API that the command line never
used. The daemon is a different product; if it is ever wanted, it should be built as one.

**Adopt, never replace.** A Strom already listening at the configured URL is used as it is and
never stopped: it may be a service the box depends on, or another operator's session. Only when
nothing answers does `up` start a headless Strom itself, with its own data directory so it never
writes into an existing install, and it stops that Strom on the way out so a camera is not left open
with nothing supervising it. Its pid is recorded so `down` can stop it after a hard kill. This is
what lets a laptop run the whole venue side from one command; a fixed installation that runs Strom
as a service is adopted the same way. `down` does *not* refuse when Strom is gone: its flows are
already stopped and the sources still need removing.

**No local state.** Flows are found by derived id, sources by the gateway's name prefix. The
exceptions are two pidfiles: the running `up`, so `down` can ask it to stop before sweeping, and a
Strom the gateway started, so `down` can stop it after a hard kill. Both pids are checked against
the process name before they are signalled, because pids are reused after a reboot.

**One loop.** Every ten seconds, sequentially over all inputs: make sure the flow exists and runs,
read its SRT statistics, then list the Open Live sources once and create or patch what differs. No
task per input, no shared state, no file written by concurrent writers.

## 7. Source status

A source is `active` only when its uplink is delivering, not merely when its flow runs. A source
assigned to a production whose feed never arrives stops the cloud flow from reaching playing at all,
so `active` has to mean "safe to assign".

Delivery is judged from Strom's `/api/flows/{id}/srt-stats`, by `bytes_sent` *changing* between
polls. Strom's own `connected` flag stays true against a vanished peer, and a bare increase would
treat the counter reset of a reconnect as a stall. A feed that was on air is held for three quiet
polls before it is reported inactive, because each status flip rewrites the source and CouchDB keeps
a revision per write. When Strom itself is unreachable the status is left as it was: losing contact
with Strom is not evidence that the feed stopped.

A source left by an earlier run is adopted by name rather than recreated, so a Studio assignment
that references its id survives a restart.

## 8. Open questions

- **Who allocates SRT ports?** A range in the settings today, which makes fleet-wide collisions an
  operator problem. An allocation endpoint in Open Live would remove it.
- **Should Open Live gain a gateway resource?** A source's `status` is the only channel, and there is
  nowhere to put telemetry, a version, or a last-seen timestamp. It would also collapse registration
  idempotency and cleanup into a server-side call.
- **EFP or MPEG-TS for contribution?** EFP carries multi-track audio and both ends support it;
  MPEG-TS is the interoperable default and is what ships.
