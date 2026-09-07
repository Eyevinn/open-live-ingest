## Project Overview
- `types` (`open-live-gateway-types`): shared config and status types. Must not depend on
  GStreamer, the gateway crate, or anything but pure utility crates such as `serde`.
- `gateway`: the daemon — a Strom flow templater and supervisor, an Open Live registration loop,
  and a local control API (axum). It owns no media pipeline; Strom does the media.

## Language
- All code, comments, commit messages, PR titles, PR descriptions, and documentation in English.

## Security
- Anonymize sensitive data (IP addresses, hostnames, credentials) in commits, PRs, and docs. Use
  `example.com` or `192.0.2.x`.
- The Open Live API key and the SRT passphrase must never reach the logs. Build errors from status
  codes, not from request or response dumps — request bodies carry the passphrase.

## Code Style
- No emojis in log macros (`info!`, `debug!`, `trace!`, `warn!`, `error!`). Remove any you find.

## Strom Integration
- Strom owns the media plane. Before adding anything to the gateway, check whether a Strom block
  already does it — encoder selection, SRT reconnect, capture handling, device discovery, and stats
  all already exist there. Duplicating them means maintaining a worse copy.
- Block ids and property names are Strom's, and the gateway sends them as strings over HTTP, so an
  upstream rename fails at runtime rather than at compile time. `GET /api/blocks` on a live Strom is
  the authority — prefer it over Strom's checked-in block reference, which can lag.
- Flow ids are derived (UUIDv5 over gateway id + input id), never stored. Keep it that way: it is
  what lets a rebooted agent find its own flow with no local state.
- Do not restart a flow for a transient uplink drop. `builtin.mpegtssrt_output` reconnects itself.

## Invariants
- Open Live being unreachable must never disturb the feed: a retry and a warning, never a stopped
  or restarted flow. The agent is not in the media path — if it dies, Strom keeps pushing.
- Registration is idempotent: resolve the source id from the state file, patch if it drifted,
  create only when absent. Restarts must not accumulate sources in Studio.

## Build
- Build from the workspace root with `cargo check`, `cargo build`, or `cargo test`.
- The gateway has no system dependencies: `cargo check`, `cargo clippy --all-targets -- -D warnings`,
  and `cargo test` all run anywhere. Keep it that way — no GStreamer, no device access.

## Documentation
- The code is the source of truth. Do not add docs that describe how the code works — they drift.
  `docs/DESIGN.md` records decisions and reasoning, not implementation.
- Doc filenames in `docs/` use `UPPER_SNAKE_CASE`. `README.md` stays lowercase.

## Tests
- A regression test must exercise the code it guards and must fail if the fix is reverted. A test
  that rebuilds the behaviour inline documents a bug; it does not stop it returning.
- Tests must not require a running Strom or Open Live. The flow builder is pure: assert on the JSON
  it emits. A test that skips when a service is absent passes green and guards nothing.
- State which tests actually ran, and which were skipped. "CI is green" is not "the new test ran".
