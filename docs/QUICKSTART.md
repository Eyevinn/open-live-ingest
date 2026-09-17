# Quickstart

An exact command sequence for a fresh Linux box or Mac, written to be followed by a script or an
agent as well as a person. Each step gives the command, what success looks like, and what to do
when it does not. Nothing here prompts.

You need, from whoever runs the production:

| Item | Looks like | Where it comes from |
|---|---|---|
| Open Live URL | `https://<name>.eyevinn-open-live.auto.prod-se.osaas.io` | the Open Source Cloud console, or a self-hosted address |
| A credential | an Open Source Cloud personal access token, or a self-hosted `API_KEY` | the OSC web console (Settings, API), or the Open Live operator |
| A name for this gateway | `Venue A` | prefixes every source name in Open Live |

Both machines need to be able to reach the internet outbound. Nothing inbound is required in the
default caller mode.

## 1. Install

```bash
curl -fsSL https://raw.githubusercontent.com/Eyevinn/open-live-ingest/main/install.sh | sh
```

Success: exit code 0 and the last lines read

```
==> Installed /usr/local/bin/open-live-ingest (open-live-ingest X.Y.Z)
==> Installing Strom vX.Y.Z with its own installer
...
==> Done. Next: open-live-ingest setup, then open-live-ingest up
```

On Linux without write access to `/usr/local/bin` the path is `~/.local/bin` instead, and a
`Note:` line says so if that is not on `PATH`. Add it:

```bash
export PATH="$HOME/.local/bin:$PATH"
```

Strom's installer needs `sudo` on Linux to install GStreamer packages. If Strom is already on
`PATH` the script leaves it alone and says `Strom already installed at ...`.

Confirm:

```bash
open-live-ingest --version   # open-live-ingest X.Y.Z
command -v strom             # a path
```

## 2. Put the credential in the environment

For Open Source Cloud, a personal access token:

```bash
export OLI_OPEN_LIVE_API_KEY='<token>'
```

For a self-hosted Open Live with an `API_KEY`, the same variable. For one without, skip this step.

Setup stores the token in the settings file, mode 0600, so the variable is needed only for this
shell. Never pass it as a flag; the CLI has no flag for it.

## 3. Configure

```bash
open-live-ingest setup --non-interactive \
  --name "<gateway name>" \
  --open-live-url "<Open Live URL>"
```

Success: exit code 0 and every line under each heading starts with `✔` (or `ok` on a terminal
without Unicode), ending with `saved to <path>`:

```
Open Live Ingest setup, non-interactive

Gateway
  ✔ gateway name Venue A

Open Live
  ✔ Open Source Cloud credential from the settings (a personal access token)
  ✔ workspace <workspace>
  ✔ reached Open Live; feeds will go to its Strom at <host>
  ✔ its Strom takes SRT on ports 47110-47129, which callers use

Local Strom
  nothing listening at http://127.0.0.1:8080 (...); up will start /usr/local/bin/strom

Uplink
  ✔ caller; SRT port range 47110-47129 comes from Open Live
  ✔ SRT latency 200 ms

Capture
  ✔ resolution auto, framerate auto, 6000 kbps

  ✔ saved to /home/<user>/.config/open-live-ingest/gateway.toml
```

The `nothing listening` line under Local Strom is normal: the gateway starts its own Strom.

Failure: exit code 1 and an `Error:` line. The message names the fix.

| Message contains | Do |
|---|---|
| `no Open Live URL` | pass `--open-live-url` |
| `needs a credential` | step 2 |
| `token has expired` | get a new token, step 2 |
| `could not reach Open Live at` | check the URL and that the box has internet; a `401` or `403` under `Caused by` means the credential is wrong |
| `could not find "strom" on PATH` | step 1 did not install Strom, or `PATH` lacks its directory |
| `Strom at ... wants a credential` | an existing Strom runs with `STROM_API_KEY`; `export OLI_STROM_API_KEY='<that key>'` and rerun |
| `Open Live publishes no SRT port range` | pass `--port-range 47110-47129` |
| `listener mode needs` | you chose `--uplink-mode listener`; pass `--public-host` and `--port-range` |

Then validate the file on its own:

```bash
open-live-ingest check   # Settings at <path> are valid.   exit 0
```

`setup --help` lists every flag. Rerunning setup with a subset of flags changes only those.

### Optional: show the box in Studio

Skip this unless whoever runs Open Live has given you a gateway id (`gw-…`) and its token
(`olgw_v1_…`), created with `POST /api/v1/gateways`. With them, the gateway pushes its status to
Open Live so a producer sees the venue in Studio:

```bash
OLI_OPEN_LIVE_GATEWAY_TOKEN='<token>' open-live-ingest setup --non-interactive --gateway-id '<id>'
```

Success adds one line under `Open Live`:

```
  ✔ Open Live greets gateway gw-…; status will be pushed to it
```

| Message contains | Do |
|---|---|
| `refused the token` | the id and token do not belong together, or the token was rotated; get a fresh pair |
| `gateway_id is set but open_live.gateway_token is not` | the variable was not exported in this shell |

## 4. See the cameras

```bash
open-live-ingest devices --json
```

Success: exit 0 and a JSON array, one object per device Strom can see:

```json
[
  { "name": "DeckLink SDI (1)", "id": "decklink-0", "virtual": false },
  { "name": "OBS Virtual Camera", "id": "obs-0", "virtual": true }
]
```

`virtual: true` devices are skipped by `up` unless `--all`. An empty array means no capture device
is connected or recognised; `up --test` streams a test pattern and tone instead, to commission the
link before cameras arrive.

## 5. Start

To try the link once, in the foreground:

```bash
open-live-ingest up --test
```

For real, as a service that survives reboots. Linux:

```bash
mkdir -p ~/.config/systemd/user
curl -fsSL https://raw.githubusercontent.com/Eyevinn/open-live-ingest/main/contrib/open-live-ingest.service \
  -o ~/.config/systemd/user/open-live-ingest.service
systemctl --user daemon-reload
systemctl --user enable --now open-live-ingest
sudo loginctl enable-linger "$USER"
```

If the binary is in `/usr/local/bin` rather than `~/.local/bin`, edit `ExecStart` in the unit
first. Logs: `journalctl --user -u open-live-ingest -f`.

macOS:

```bash
curl -fsSL https://raw.githubusercontent.com/Eyevinn/open-live-ingest/main/contrib/se.eyevinn.open-live-ingest.plist \
  | sed "s|REPLACE_ME|$USER|g" > ~/Library/LaunchAgents/se.eyevinn.open-live-ingest.plist
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/se.eyevinn.open-live-ingest.plist
```

Logs: `~/Library/Logs/open-live-ingest.log`.

## 6. Verify

```bash
open-live-ingest status --json
```

Success looks like:

```json
{
  "process": { "running": true, "pid": 4242 },
  "uplink": { "host": "<host>", "ports": "47110-47129", "port_source": "Open Live" },
  "uplink_error": null,
  "strom": { "url": "http://127.0.0.1:8080", "reachable": true, "error": null },
  "open_live": { "url": "<Open Live URL>", "reachable": true, "error": null },
  "inputs": [
    {
      "name": "Venue A — DeckLink SDI (1)",
      "flow": { "id": "...", "running": true },
      "uplink": null,
      "source": { "id": "...", "status": "active" }
    }
  ]
}
```

What to check, in order:

1. `process.running` is `true`. Otherwise the service did not start; read its log.
2. `strom.reachable` is `true`. Otherwise the local Strom failed to start; `strom` alone in a
   terminal shows why, usually a missing GStreamer plugin.
3. Every input has `flow.running: true` and a `source`. A missing `source` with
   `open_live.reachable: false` is a transient Open Live outage: the feed keeps running and the
   source is registered on the next tick.
4. `uplink` is `null` until the source is assigned to a production in Open Live. That is normal;
   it fills in with `send_rate_mbps` and `rtt_ms` once something receives the feed.

Without `--json` the same appears as a table.

## 7. Stop

```bash
open-live-ingest down
```

Removes the flows from Strom and the sources from Open Live, and stops a Strom the gateway
started. With a service installed, stop the service instead, which does the same:

```bash
systemctl --user disable --now open-live-ingest        # Linux
launchctl bootout gui/$(id -u)/se.eyevinn.open-live-ingest   # macOS
```
