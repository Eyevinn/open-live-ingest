## Project Overview
One crate, one binary. The gateway is a command line tool that drives a Strom instance
on the same machine and registers sources with Open Live. It owns no media pipeline;
Strom does the media.

- `src/config.rs`: settings, validation, the two SRT URIs of a link.
- `src/flow.rs`: the flow template and flow ownership by derived id.
- `src/strom.rs`, `src/openlive.rs`: the two HTTP clients.
- `src/devices.rs`: from Strom's device list to inputs with names and ports.
- `src/run.rs`: `up`, `down`, `status`, `devices`.
- `src/setup.rs`: interactive setup.
- `src/local_strom.rs`: adopt a running Strom, or start and later stop a headless one.

## Language
- All code, comments, commit messages, PR titles, PR descriptions, and documentation in English.

## Security
- Anonymize sensitive data (IP addresses, hostnames, credentials) in commits, PRs, and docs. Use
  `example.com` or `192.0.2.x`.
- Never commit a settings file. `gateway.toml` and `gateway-*.toml` are ignored for that reason.
- The Open Live credential and the SRT passphrase must never reach the logs or the terminal. Build
  errors from status codes, not from request or response dumps, and print addresses through
  `config::mask_passphrase`.

## Code Style
- No emojis in log macros (`info!`, `debug!`, `trace!`, `warn!`, `error!`).

## Strom Integration
- Strom owns the media plane. Before adding anything to the gateway, check whether a Strom block
  already does it: encoder selection, SRT reconnect, capture handling, device discovery, and stats
  all exist there. A Strom bug is fixed in Strom, not worked around here.
- Flow, block, link, device, and SRT statistics types come from `strom-types`, pinned in `Cargo.toml`
  to the Strom release the venue runs. Bump the tag on purpose when that Strom is upgraded. Block
  definition ids and property names are still Strom's strings, because they live in each block's
  builder in the backend; `GET /api/blocks` on a live Strom is the authority for those.
- Do not link the Strom engine itself. The process boundary is what keeps the gateway out of the
  media path, keeps its build free of GStreamer, and lets Strom and the gateway upgrade separately.
- Flow ids are derived (UUIDv5 over gateway id + input id), never stored. That is what lets `down`
  and `status` find the flows with no local state.
- Do not restart a flow for a transient uplink drop. `builtin.mpegtssrt_output` reconnects itself.

## Invariants
- Open Live being unreachable must never disturb the feed: a warning and a retry on the next tick,
  never a stopped or restarted flow.
- `up` owns what it started. Ctrl-C and SIGTERM remove the flows and sources; SIGHUP does not.
- No local state beyond the settings file and two pidfiles (the running `up`, and a Strom it
  started). Flows are recognised by derived id, sources by the gateway's name prefix.
- A Strom already listening is adopted and never stopped. Only a Strom the gateway started is
  stopped, on the way out.

## Build
- `cargo check`, `cargo clippy --all-targets -- -D warnings`, and `cargo test` run anywhere: no
  GStreamer, no device access. Keep it that way.

## Documentation
- The code is the source of truth. `docs/DESIGN.md` records decisions and reasoning, not
  implementation. Do not narrate past bugs in comments; git history holds that.
- Doc filenames in `docs/` use `UPPER_SNAKE_CASE`. `README.md` stays lowercase.

## Tests
- A regression test must exercise the code it guards and must fail if the fix is reverted.
- Tests must not require a running Strom or Open Live. The flow builder is pure: assert on the JSON
  it emits.
- State which tests actually ran, and which were skipped.
