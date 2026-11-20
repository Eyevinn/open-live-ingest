# Open Live Ingest: Design

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
          └──────────── audio_out (only when an audio source is paired) ──> audio_in_0
```

**Audio is paired, not assumed.** A camera sends video only unless an audio source is paired with
it (`up --audio "<camera>=<audio>"`, or `audio` on the camera's `[[input]]` entry). A paired source
is a second device in the same `builtin.local_input` block (`stream_mode` `audio_video` with its own
`audio_device`), and its raw audio goes straight to the uplink, which encodes it. Pairing is
explicit because a desk usually has several microphones and only the operator knows which one
belongs to which camera; one audio source goes with one camera, and a run that pairs it twice is
refused before anything starts. Audio embedded in an SDI signal is not carried yet. The audio
track count follows the flow, because declaring a track the flow never delivers stalls the
pipeline on a pad that never produces. The test pattern carries a tone so a link can be
commissioned end to end.

**Drift is unmeasured.** A separate audio device runs on its own clock, so its audio may drift
against the camera's video over a long show. How far it drifts in practice has not been measured
yet (#64 AC3: a 60-minute camera plus USB microphone run). Until it has, treat a paired source as
fine for a short segment and check lip sync on a long one.

**Flow ids are derived, not stored.** A UUIDv5 over `gateway id + input id`, where the input id is
derived from Strom's device id. A flow is ours if and only if its id is the one we would derive for
the device it captures. That gives `down` and `status` a precise ownership test with no local state
and no naming convention, and a flow someone built by hand on the same camera is never touched.
Changing `gateway.id` hides the flows already created.

**The flow's structure is Strom's own type.** `strom-types` is pure serde with no GStreamer, so the
gateway builds a typed `Flow` and decodes Strom's responses into Strom's types, pinned to the Strom
release the venue runs. Block definition ids and property names remain strings: they live in each
block's builder in the backend, so a rename there fails at runtime. `GET /api/blocks` on a live
Strom is the authority for those.

**The engine itself stays a separate process.** Linking it was considered and rejected: the gateway
would become a GStreamer program with Strom's build and version lock, a gateway crash would take the
camera off air, and the operator would lose Strom's editor and stats pages for field debugging.
The cost of a separate process is one more thing to install, and adopt-or-start already makes that
one command.

## 5. Which end dials

One link, two URIs that mirror each other. The choice is a deployment constraint: **whichever end
listens is the end that needs an inbound UDP port.**

| Mode | Venue output block | Registered on the source | Needs an inbound port |
|---|---|---|---|
| `caller` (default) | `srt://cloud:9000?mode=caller` | `srt://:9000?mode=listener` | the cloud |
| `listener` | `srt://:9000?mode=listener` | `srt://venue:9000?mode=caller` | the venue |

`srtsrc` defaults to caller mode, so the registered address always carries an explicit `mode=`.
Ports come from a range, one per input. In listener mode the gateway allocates them itself, lowest
first in device-name order, so the same cameras land on the same ports across runs. In caller mode
Open Live assigns them (see §5), and a source left by an earlier run keeps its port, which stabilises
the registered addresses the same way. The passphrase is owned
by the gateway: Open Live masks it on read, so it is never read back and is compared masked.
Rendezvous mode was removed; nobody used it, and it doubled the URI logic.

**The end that listens owns the range.** In caller mode the listener ports are the cloud Strom's,
and one cloud Strom serves several venues and several Open Live instances, so a range chosen at the
venue is a fleet-wide collision waiting to happen. Open Live reserves ports from Strom's port pool
and publishes them with the Strom host on `server-info`; the gateway takes them from there, and Open
Live rejects a registration on a port it does not hold. The published ports are a list rather than a
range, because Strom's pool can have holes and a port it found already bound leaves a gap — the
gateway treats them as a set and must not assume they are consecutive. Ports in the settings are
then only a fallback for an Open Live that
publishes none, and is ignored with a warning when one is published, because a setting that silently
overrode the cloud's allocation would recreate the collision. In listener mode the
ports are the venue's own, which only the venue can know, so there the settings are required.

**Inside the range, Open Live picks the port.** Several gateways can feed one Open Live, and each
knows only its own inputs, so a gateway choosing "lowest free" inside the published range would
collide with the next gateway doing the same. The one party that sees every source and output is
Open Live, so in caller mode the gateway registers each input first with port 0, which asks Open Live
to assign the lowest port no other source or output holds, and builds its flow to the port that
comes back. A source of the same name left by an earlier run keeps its port when it is still inside
the range, and is reassigned when it is not. Registration therefore precedes the flows in caller
mode, where it used to follow them; Open Live being down at start-up is then a start-up error rather
than a warning, which is right, since without a port there is nothing to build.

## 6. The front ends own what they start

There are two front ends: a terminal, because a venue box is often reached over SSH, and a desktop
app for a person at the machine (ADR-001). Both drive the same library and own what they start.
The app is a peer of the command line, not a daemon or a control API: it runs in the foreground,
starts or adopts Strom through the same `local_strom` code, and everything below holds for it as it
does for `up`. The terminal stays the front end for SSH and headless boxes, and nothing in it
changes for the app.

**Setup is running it.** Anything missing is asked for, checked immediately, and written to the
settings file. The Open Live credential kind is inferred from the address, since nobody can answer
"osc or direct" from those words, and the local Strom's key is asked for only when Strom answers
401, because "wants a credential" and "cannot be reached" send an operator to different places.

**The Open Source Cloud side is chosen, not typed.** Setup asks how to authenticate: the OSC CLI's
environment variable, its browser login, or a pasted personal access token. With the token in hand it
reads back the workspace the token belongs to, since that is what the operator picked in the browser
and the platform offers no way to list or switch workspaces from outside, and then lists that
workspace's Open Live instances the way `osc list` does, so the address is picked rather than pasted.
The login is exchanged exactly like a personal access token, so nothing secret has to cross the
terminal or land in the settings file; but it lasts an hour, so it fits setup and a personal access
token carries the show. A token in the settings or the environment wins over the saved login: a
deployment tool that injects one must not be overridden by whoever last signed in on the box. A
typed address and a pasted token remain the way in on a machine with no browser, since the login's
redirect lands on the machine running the CLI.

**An app sign-in is kept in the OS keychain and renewed during the show.** The app signs in natively
(`osc::browser_login`) and keeps OSC's refresh token, not the one-hour access token, in the macOS
Keychain or the Windows Credential Manager; the settings file gets only `open_live.credential =
"keychain"`, so the file never holds that secret and stays safe to copy between machines. A pasted
personal access token goes to the keychain the same way. While running, the access token is renewed
from the refresh token five minutes before it expires and the new refresh token is written back, so a
show of any length runs on one sign-in; the keychain is read again before each refresh so two
processes sharing the entry do not undo each other. A token endpoint that is down is waited out on the
access token in hand; a refused refresh means signing in again. A credential that cannot be renewed
and ends within twelve hours is announced before going live (`Event::OscSignInEnds`), when signing in
again is still easy, rather than discovered when Open Live starts refusing mid-show. The keychain is
reached through a trait, so the refresh logic is tested against an in-memory store and a fake token
endpoint on a fake clock; the platform calls are own code (`security-framework`'s generic-password
calls on macOS, Credential Manager through the `windows-sys` already in the tree), which costs four
crates on macOS and none on Windows, where the `keyring` crate would cost five on each. The CLI keeps
`OSC_ACCESS_TOKEN` and `~/.osc/token`; a token in `open_live.api_key` still wins over the keychain.

**`up` stays in the foreground and owns what it started.** Ctrl-C and SIGTERM tear the feeds down.
SIGHUP does not: a closed SSH session must not take a venue off air. A deliberate stop stops, an
accident does not. On Windows, Ctrl-C and Ctrl-Break stop the same way, and so do closing the
console window, logoff and shutdown. Windows ends the process about five seconds after those three,
so their teardown removes flows and sources concurrently under a four-second deadline, names what
it could not remove for the next `up` or `down` to sweep, and then ends the managed Strom at once
(TerminateProcess) rather than waiting for it. Two limits apply to those three events. First,
"Strom last" is only the order `up` follows: a managed Strom shares the console, and Windows sends
close, logoff and shutdown to every process attached to it (a separate process group only scopes
Ctrl-C and Ctrl-Break), so Strom may already be ending while the flows are removed. Those removals
then fail and are logged; the Open Live sources, which are what make a later `up` hit a 409, are
removed independently of Strom. Second, Windows does not deliver logoff and shutdown to a console
process that has loaded `user32.dll` or `gdi32.dll`, so those two are verified by hand on Windows,
not assumed. This replaced an earlier daemon model in which flows were meant to outlive the
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

**The bundled Strom comes first, and brings its own GStreamer.** The macOS app ships Strom next to
the gateway in `Contents/MacOS`, with GStreamer's libraries under `Contents/Frameworks`, its plugins
under `Contents/PlugIns/gstreamer` and the plugin scanner in `Contents/Helpers`: the places
`codesign` accepts nested code, with no dotted directory names, which `codesign` would take for
nested bundles. With `strom.binary` at its default, a `strom` next to the running executable
(symlinks resolved) is used before `PATH`; an explicit setting always wins. A `strom` next to the
executable is *bundled* only when the bundle layout is really there: the parent of the executable's
directory (the bundle's `Contents`) holds `PlugIns/gstreamer`. Only a bundled Strom is started with
the GStreamer plugin path, scanner and registry pointed into the bundle and the managed data
directory, so a Homebrew GStreamer on the same Mac is never mixed in. The same goes for the bundled
GLib, which was built for Homebrew's prefix and would otherwise load GIO modules from
`/opt/homebrew/lib/gio/modules` and look for shared data under `/usr/local/share`: a bundled Strom
gets `GIO_MODULE_DIR` pointed at an empty `Contents/PlugIns/gio`, `GIO_EXTRA_MODULES` cleared and
`XDG_DATA_DIRS` set to `Contents/Resources`. The bundle ships no GIO module because no flow of ours
needs one: the SRT uplink encrypts with libsrt, not with GIO's TLS. A `strom` next to the
executable without that layout, as `install.sh` leaves it in `/usr/local/bin` or `~/.local/bin`, is
still preferred over `PATH` but is not bundled: like a Strom from `PATH` or an explicit path, it
brings its own GStreamer and gets no GStreamer environment. Pointing `GST_PLUGIN_SYSTEM_PATH` at a
directory that does not exist would leave it with no plugins at all. The bundle-relative paths are
constants in `src/local_strom.rs` that the bundle script must match; a unit test checks that it
does.

**Every Mach-O in the app runs under the hardened runtime, signed or not.** Notarization requires
the hardened runtime, so the bundle script always signs with it, ad hoc when the release workflow
has no Developer ID certificate and with that certificate when it has. The ad hoc build then hits
the same hardened-runtime rules the notarized one will, on every pull request that touches the
build, and the workflow starts the bundled Strom from the DMG to prove it still loads its GStreamer.
Only `strom` and `open-live-ingest` carry entitlements, camera and microphone, because Strom opens
the devices. `strom` alone also carries `com.apple.security.cs.allow-jit`, so GStreamer's ORC can compile the
inner loops of videoconvert, videoscale and audioconvert at run time instead of falling back to
slower C (#113); the bundle script's check refuses it on any other signature, and the workflow's
start check runs a test-pattern flow and fails on an `ORC: ERROR` line in Strom's log. Apart from
that, a Developer ID build gets no hardened-runtime exception: every library and plugin is in
the bundle and signed with the same Team ID, so library validation stays on. An ad hoc signature has
no Team ID, and macOS 27 refuses to map the bundled libraries into an ad hoc signed `strom` under
library validation (#66), so an ad hoc build alone adds
`com.apple.security.cs.disable-library-validation` to `strom`, `open-live-ingest` and
`gst-plugin-scanner`. The bundle script's check requires it on an ad hoc bundle and refuses it on
any other, so the exception cannot reach a notarized build. The CI start check runs on an older
macOS, so a release is also checked by hand on macOS 27 (README, "Releasing").

**No local state.** Flows are found by derived id, sources by the gateway's name prefix. The
exceptions are two pidfiles: the running `up`, so `down` can ask it to stop before sweeping, and a
Strom the gateway started, so `down` can stop it after a hard kill. Both pids are checked against
the process name before they are signalled, because pids are reused after a reboot. A sign-in from
the app adds one entry in the OS keychain (above). The desktop app adds two more and nothing else:
its log file, `app.log` with one rotated copy, and on Windows the WebView2 cache folder
(`%LOCALAPPDATA%\open-live-ingest\webview`), which WebView2 keeps even for a page that stores
nothing. The app remembers no window size, tab or recent list (ADR-001, spec section 6).

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

## 8. Seen from Studio

Open Live has a gateway resource: an id, a name, a health derived from when it last heard from the
box, and the last snapshot the box sent. The gateway pushes into it over **one outbound WebSocket**,
for the reason everything here is outbound (§1): Open Live cannot dial a venue behind NAT, and the
box can always dial out. Studio reads the resource; the gateway never hears from Studio.

**The socket has a credential of its own.** Open Live mints a token per gateway when the gateway is
created, stores only its hash, and accepts that token on the heartbeat socket and nowhere else; the
API key is not accepted there. A box at a venue, run by venue staff, then holds nothing that can
drive a production, and one lost box is revoked by rotating one token rather than re-keying a
fleet. The token is a setting like the API key, with the same file mode and environment override,
and it travels in a header, never in the URL, so it appears in no address anyone logs.

**Nothing new is measured.** The loop in §6 already knows each flow's state, its uplink statistics,
and the source it registered as, and hands that over as a snapshot after every tick. The socket
re-sends the latest snapshot on its own clock, at the interval Open Live names in its greeting,
five seconds by default. Two clocks rather than one because the loop's ten-second tick is set by
how often Strom and Open Live's source list should be polled, while the heartbeat's cadence is set
by Open Live's down threshold of fifteen seconds; tying them would mean polling Strom twice as
often for Studio's benefit, or reading as down after one missed tick. A slow or absent Open Live
never holds the loop, because the loop only writes a value.

**The frame speaks Open Live's vocabulary**, which is Strom's: a flow is `playing` or `idle`. A flow
the loop cannot confirm because Strom stopped answering is reported idle rather than as it last
was: all of a venue's inputs dropping to idle while its heartbeat keeps arriving is a true
description of a box that has lost its Strom, and the source status, not the heartbeat, is what
carries the benefit of the doubt for a production already on air (§7).

**Nothing inbound is acted on.** Open Live's frames are a greeting, acknowledgements, and errors;
anything else is ignored, as the contract requires of both ends. Start, stop, and device picking
over this socket are a later phase of the same epic, and adding them is a deliberate decision
about what a venue accepts from the cloud, not an extension of this one.

**Failure is a warning, once.** Like registration, the heartbeat is best effort and never touches
a flow. A socket that cannot be opened is retried with exponential backoff to a minute; a refused
token is reported once with the settings to check and retried once a minute, since nothing faster
would help. Each failure is logged when it starts and when it ends, so a log read after the show
says when Studio could and could not see the venue. A stop announces itself before the feeds go
down, bounded by two seconds, so a dead socket cannot delay the teardown.

Setup opens the socket once and waits for the greeting. A wrong token is then a failed check at
the prompt, in the same voice as a wrong API key, rather than a warning repeated through a show.

## 9. Open questions

- **Who allocates SRT ports?** Resolved: Strom does, through its port pool. Open Live reserves
  ports from its Strom and publishes them through `server-info`; in caller mode the gateway uses
  those ports and treats a range in the settings as a fallback for an Open Live that publishes
  none. See §5.
- **Should Open Live gain a gateway resource?** Resolved: it has, and the heartbeat reports into
  it (§8). Registering sources under the gateway, so that cleanup and idempotency become a
  server-side call, remains open.
- **EFP or MPEG-TS for contribution?** EFP carries multi-track audio and both ends support it;
  MPEG-TS is the interoperable default and is what ships.
