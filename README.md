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
venue box needs no manual work in Strom's editor and comes back on air by itself after a power cut
— Strom does not auto-start flows on boot.

See [docs/DESIGN.md](docs/DESIGN.md) for the full design: why media never transits Open Live, why
the SRT caller direction matters, what is deliberately left to Strom, and what is deferred.

## Status

Early. Design, flow templating, supervision, and Open Live registration are in place. Not yet
exercised against a live Strom.

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
