# open-live-gateway

A contribution gateway for [Open Live](https://github.com/Eyevinn/open-live). It runs on a Linux
box at the venue alongside a local [Strom](https://github.com/Eyevinn/strom) instance, and drives
it: Strom captures SDI (Blackmagic DeckLink or USB/UVC), encodes to H.264/AAC, and pushes MPEG-TS
over SRT to a cloud-hosted Strom. The gateway templates that flow, keeps it running, and registers
the feed with the Open Live API so it appears as an assignable source in
[Open Live Studio](https://github.com/Eyevinn/open-live-studio).

```
   VENUE                                         CLOUD
 SDI ──> Strom (local) ──MPEG-TS/SRT caller──> Strom (cloud) ──> Open Live ──> Studio
            ▲
            │ REST (create + start flow)
         gateway agent ──────HTTPS (register source)──────────> Open Live API
```

The gateway owns no media pipeline. Strom does the media; this is the control agent that means a
venue box is described by a config file rather than built by hand in Strom's editor, and that keeps
the flow provisioned, up to date with the config, and running.

See [docs/DESIGN.md](docs/DESIGN.md) for the full design: why media never transits Open Live, why
the SRT caller direction matters, what is deliberately left to Strom, and what is deferred.

## Running it

Run it and it asks for what it needs, checks each answer, remembers it, then registers every
capture device with Open Live and starts streaming:

```bash
open-live-gateway              # same as `up`
open-live-gateway status       # what is running, from Strom and Open Live directly
open-live-gateway down         # stop and remove the flows and sources
open-live-gateway setup        # ask for settings without starting anything
open-live-gateway up --all             # include virtual devices, skipped by default
open-live-gateway up --devices 1,3     # only these, by number from the printed list
open-live-gateway up --reconfigure     # ask for every setting again
```

Built for SSH: plain prompts on any terminal, no window and no full-screen UI. `up` stays in the
foreground and owns what it started — Ctrl-C stops and removes the feeds — but it **ignores
SIGHUP**, so a closed session or a dropped link does not take a venue off air. `status` and `down`
work from a second session, and from the recorded state rather than by talking to the running
process, so they behave the same after a `kill -9`.

Three credentials exist in this system and it only ever asks for one:

| Credential | Who needs it | Asked for |
|---|---|---|
| Open Live: an OSC personal access token, or Open Live's own `API_KEY` | the gateway, to register sources | **yes** — inferred from the URL, so you are not asked to choose |
| The local Strom's `STROM_API_KEY` | the gateway, only if that Strom runs with auth | only when Strom actually refuses without one |
| The cloud Strom's token | Open Live holds it itself | never — the gateway speaks only SRT to the cloud Strom |

Settings live in a per-user file (`~/Library/Application Support/open-live-gateway/gateway.toml` on
macOS, `$XDG_CONFIG_HOME` or `~/.config` on Linux), written mode 0600 because it holds that
credential. Pass `--config` to put it elsewhere. Nothing needs a text editor, though the file is
the same format if you would rather write one — and a declared `[[inputs]]` list still works for a
box that should stream something other than "every device".

## Status

Early. Flow templating, supervision, Open Live registration (including OSC token exchange), stall
detection and recovery, interactive setup, and `up`/`down`/`status` are in place. Verified end to
end against a live Strom and a live Open Live: a test pattern reached a production's program output
as decodable video.

## Requirements

- Linux, x86_64 or aarch64 — anything that can run Strom
- A Strom instance on the same box, with capture hardware configured (DeckLink Desktop Video
  installed, or a UVC capture device)
- Rust 1.97.1 (pinned in `rust-toolchain.toml`)

The gateway itself is an ordinary HTTP client: no GStreamer, no device access, no privileges.

## Build

```bash
cargo build --release
```

## Run

```bash
cp gateway.toml.example gateway.toml
# edit gateway.toml: Strom URL, capture device, uplink host/port, Open Live URL and API key
cargo run -- --config ./gateway.toml
```

`--check` validates the configuration and exits. Deploy with
`packaging/systemd/open-live-gateway.service`.

## Configuration

TOML file, overridden by environment variables, overridden by CLI flags. See
`gateway.toml.example` for every option. The file holds the Open Live API key, the Strom API key,
and the SRT passphrase — install it mode 0600.

| Variable | Overrides |
|---|---|
| `OLG_CONFIG` | config file path |
| `OLG_STROM_URL` | `strom.url` |
| `OLG_STROM_API_KEY` | `strom.api_key` |
| `OLG_OPEN_LIVE_URL` | `open_live.url` |
| `OLG_OPEN_LIVE_API_KEY` | `open_live.api_key` |
| `OLG_OPEN_LIVE_AUTH_MODE` | `open_live.auth_mode` (`direct` or `osc`) |

In `direct` mode the key is optional: a self-hosted Open Live with `API_KEY` unset leaves
`/api/v1` open, which is the usual local development setup. `osc` mode always needs the PAT.
| `OLG_CONTROL_BIND` | `control.bind` |
| `OLG_CONTROL_TOKEN` | `control.token` |
| `OLG_LOG_LEVEL` | `log.level` |

## Control API

Loopback by default; a non-loopback bind requires `control.token` and is rejected at startup
without one.

| Method | Path | Purpose |
|---|---|---|
| `GET` | `/healthz` | Liveness (unauthenticated) |
| `GET` | `/api/v1/status` | Per-input state, flow id, GStreamer state, registered listener address |
| `POST` | `/api/v1/inputs/:id/start` · `/stop` | Manual control |
| `GET` | `/metrics` | Prometheus exposition |

Device discovery and pipeline debugging live in Strom's own UI on the same box.

## License

Licensed under either of [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.
