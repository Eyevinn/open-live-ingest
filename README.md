# open-live-ingest

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
open-live-ingest                  # same as `up`
open-live-ingest up --devices "FaceTime,DeckLink"   # only these, by name fragment or id
open-live-ingest up --all         # include virtual devices, skipped by default
open-live-ingest up --test        # a test pattern and tone, to commission the link
open-live-ingest up --reconfigure # ask for every setting again
open-live-ingest status           # what is running, from Strom and Open Live directly
open-live-ingest down             # stop and remove the flows and sources
open-live-ingest devices          # what Strom can see
open-live-ingest setup            # ask for settings without starting anything
open-live-ingest check            # validate the settings file
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
`~/.config/open-live-ingest/gateway.toml` on Linux, `~/Library/Application Support/open-live-ingest/gateway.toml`
on macOS, or wherever `--config` points. The same format can be written by hand; see
[`gateway.toml.example`](gateway.toml.example) for every option.

| Variable | Overrides |
|---|---|
| `OLI_CONFIG` | settings file path |
| `OLI_STROM_URL` | `strom.url` |
| `OLI_STROM_API_KEY` | `strom.api_key` |
| `OLI_OPEN_LIVE_URL` | `open_live.url` |
| `OLI_OPEN_LIVE_API_KEY` | `open_live.api_key` |
| `OLI_OPEN_LIVE_AUTH_MODE` | `open_live.auth_mode` (`direct` or `osc`) |
| `OLI_LOG_LEVEL` | `log.level` |

In `caller` mode, the default, the SRT ports belong to the cloud Strom. Open Live leases a range
from that Strom and publishes it on `GET /api/v1/server-info` together with the Strom host, and the
gateway registers each input with port 0 and Open Live assigns it a free port in that range, so
several gateways can feed one Open Live; `uplink.port_range` is only a fallback for an
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
key in the settings or in `OLI_OPEN_LIVE_API_KEY` wins over the environment, which wins over the
saved login; the gateway reads the login again at each token exchange, so signing in again renews a
running gateway. On a box with no browser, paste a token, or copy `~/.osc/token` over from a machine
you signed in on.

## Install

One command installs the latest release and, unless one is already on `PATH`, the Strom release
it is built against, GStreamer included. Nothing is asked, so it works from a script too:

```bash
curl -fsSL https://raw.githubusercontent.com/Eyevinn/open-live-ingest/main/install.sh | sh
```

Binaries land in `/usr/local/bin` when it is writable, else `~/.local/bin`. `INSTALL_DIR`,
`VERSION`, and `SKIP_STROM=true` override that; the script's header lists every knob.

Other ways in:

- **By hand.** Each [release](https://github.com/Eyevinn/open-live-ingest/releases) carries a
  static binary for Linux (x86_64, aarch64) and macOS (Apple silicon, Intel), plus `SHA256SUMS`
  and a signed build provenance you can check with `gh attestation verify`. The asset names do
  not carry a version, so `releases/latest/download/open-live-ingest-<target>.tar.gz` is stable.
- **Rust users.** `cargo binstall open-live-ingest` fetches the same tarball. `cargo build
  --release` builds from source with Rust 1.97.1, pinned in `rust-toolchain.toml`.

To keep it running across reboots, [`contrib/`](contrib/) has a systemd user unit and a launchd
agent, each with install steps in its header.

## Requirements

- Linux or macOS, with a Strom instance on the same machine that can see the capture hardware
- Node.js, only to sign in to Open Source Cloud with `npx @osaas/cli login`. A pasted token needs none

The gateway itself is an ordinary HTTP client: no GStreamer, no device access, no privileges.
Strom's API types come from the `strom-types` crate, pinned in `Cargo.toml` to the Strom release
the venue runs.

## Releasing

Bump `version` in `Cargo.toml`, commit, then tag and push:

```bash
git tag v0.3.0 && git push origin v0.3.0
```

The release workflow refuses a tag that does not match `Cargo.toml`, builds the four binaries,
and publishes them with a checksum file, build provenance, and generated notes. When the
`strom-types` pin moves, move `STROM_VERSION` in `install.sh` with it; a test checks they agree. Run the workflow by hand from the
Actions tab to build the artifacts without publishing.

## License

Licensed under either of [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.
