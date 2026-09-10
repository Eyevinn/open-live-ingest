| The local Strom's `STROM_API_KEY` | the gateway, only if an adopted Strom runs with auth | only when Strom actually refuses without one |# open-live-gateway

A contribution gateway for [Open Live](https://github.com/Eyevinn/open-live). It runs on a Linux
box or a laptop at the venue alongside a local [Strom](https://github.com/Eyevinn/strom), and drives
it: Strom captures the cameras, encodes to H.264/AAC, and pushes MPEG-TS over SRT to a cloud-hosted
Strom. The gateway builds those flows, keeps them running, and registers each feed with the Open
Live API so it appears as an assignable source in
[Open Live Studio](https://github.com/Eyevinn/open-live-studio).

```
   VENUE                                         CLOUD
 camera ──> Strom (local) ──MPEG-TS/SRT──────> Strom (cloud) ──> Open Live ──> Studio
              ▲
              │ REST (create + start flow)
           gateway ───────HTTPS (register source)─────────────> Open Live API
```

The gateway owns no media pipeline and keeps no state. Strom does the media; this is the control
tool that means a venue is one command rather than a set of flows built by hand in Strom's editor.

See [docs/DESIGN.md](docs/DESIGN.md) for the reasoning: why media never transits Open Live, which
end of the SRT link dials, what is deliberately left to Strom, and what was removed.

## Running it

```bash
open-live-gateway                  # same as `up`
open-live-gateway up --devices "FaceTime,DeckLink"   # only these, by name fragment or id
open-live-gateway up --all         # include virtual devices, skipped by default
open-live-gateway up --test        # a test pattern and tone, to commission the link
open-live-gateway up --reconfigure # ask for every setting again
open-live-gateway status           # what is running, from Strom and Open Live directly
open-live-gateway down             # stop and remove the flows and sources
open-live-gateway devices          # what Strom can see
open-live-gateway setup            # ask for settings without starting anything
open-live-gateway check            # validate the settings file
```

Run it and it asks for what it needs, checks each answer, remembers it, then registers every capture
device Strom can see and starts streaming. Built for SSH: line-by-line prompts on any terminal, lists
picked with the arrow keys, no window. Colour steps back to plain text under `NO_COLOR` or on a dumb
terminal.

`up` stays in the foreground and owns what it started. Ctrl-C or SIGTERM stops and removes the
feeds. It **ignores SIGHUP**, so a closed session or a dropped link does not take a venue off air.
`status` and `down` work from a second session and need nothing from the running process: flows are
recognised by their derived ids and sources by the gateway's name prefix, so they behave the same
after a `kill -9`.

Three credentials exist in this system and it only ever asks for one:

| Credential | Who needs it | Asked for |
|---|---|---|
| Open Live: an Open Source Cloud login, or Open Live's own `API_KEY` | the gateway, to register sources | **yes**, and the kind is inferred from the URL |
| The local Strom's `STROM_API_KEY` | the gateway, only if that Strom runs with auth | only when Strom actually refuses without one |
| The cloud Strom's token | Open Live holds it itself | never |

Strom does the capturing and encoding, as its own process on the same machine. If one is already
listening at `strom.url` the gateway adopts it and never stops it. If nothing is listening, it
starts a headless one from `strom.binary`, with its own data directory beside the settings, and
stops it again on the way out. `down` also stops a Strom left behind by a hard kill. Set
`strom.manage = false` to insist on a Strom you run yourself.

## Settings

Written by setup to a per-user file, mode 0600 because it holds the Open Live credential:
`~/.config/open-live-gateway/gateway.toml` on Linux, `~/Library/Application Support/open-live-gateway/gateway.toml`
on macOS, or wherever `--config` points. The same format can be written by hand; see
[`gateway.toml.example`](gateway.toml.example) for every option.

| Variable | Overrides |
|---|---|
| `OLG_CONFIG` | settings file path |
| `OLG_STROM_URL` | `strom.url` |
| `OLG_STROM_API_KEY` | `strom.api_key` |
| `OLG_OPEN_LIVE_URL` | `open_live.url` |
| `OLG_OPEN_LIVE_API_KEY` | `open_live.api_key` |
| `OLG_OPEN_LIVE_AUTH_MODE` | `open_live.auth_mode` (`direct` or `osc`) |
| `OLG_LOG_LEVEL` | `log.level` |

In `caller` mode, the default, the SRT ports belong to the cloud Strom. Open Live leases a range
from that Strom and publishes it on `GET /api/v1/server-info` together with the Strom host, and the
gateway allocates one port per input from that range; `uplink.port_range` is only a fallback for an
Open Live that publishes none. In `listener` mode the ports are the venue's own and
`uplink.port_range` is required. `up` and `status` print the range in use and where it came from.

In `direct` mode the key is optional: a self-hosted Open Live with `API_KEY` unset leaves `/api/v1`
open. `osc` mode always needs a credential, and setup asks which of three to use:

| Choice | Credential | Where it lives |
|---|---|---|
| `env` | the OSC CLI's `OSC_ACCESS_TOKEN` | the environment; nothing is stored |
| `cli` | `npx @osaas/cli login`, a browser sign-in where you pick the workspace | `~/.osc/token`, written by the CLI |
| `paste` | a personal access token from the OSC web console | `open_live.api_key`, mode 0600 |

Setup then shows the workspace the token belongs to, lists that workspace's Open Live instances to
pick from, and checks the pick before moving on. A typed address is always the last entry.

A CLI login lasts one hour, which covers setup but not a show, so for `up` use `env` or `paste`. A
key in the settings or in `OLG_OPEN_LIVE_API_KEY` wins over the environment, which wins over the
saved login; the gateway reads the login again at each token exchange, so signing in again renews a
running gateway. On a box with no browser, paste a token, or copy `~/.osc/token` over from a machine
you signed in on.

## Requirements

- Linux or macOS, with a Strom instance on the same machine that can see the capture hardware
- Rust 1.97.1 to build, pinned in `rust-toolchain.toml`. Strom's API types come from the `strom-types`
  crate, pinned in `Cargo.toml` to the Strom release the venue runs
- Node.js, only to sign in to Open Source Cloud with `npx @osaas/cli login`. A pasted token needs none

The gateway itself is an ordinary HTTP client: no GStreamer, no device access, no privileges.

```bash
cargo build --release
```

## License

Licensed under either of [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.
