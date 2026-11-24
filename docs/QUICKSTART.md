# Quickstart

An exact command sequence for a fresh Linux box or Mac (Windows: see [Windows](#windows) at the
end; a Mac set up from the DMG instead: see
[Mac: install from the DMG, open the app](#mac-install-from-the-dmg-open-the-app)), written to be
followed by a script or an
agent as well as a person. Each step gives the command, what success looks like, and what to do
when it does not. Nothing here prompts.

You need, from whoever runs the production:

| Item | Looks like | Where it comes from |
|---|---|---|
| Open Live URL | `https://<name>.eyevinn-open-live.auto.prod-se.osaas.io` | the Open Source Cloud console, or a self-hosted address |
| A credential | an Open Source Cloud personal access token, or a self-hosted `API_KEY` | the OSC web console (Settings, API), or the Open Live operator |
| A name for this gateway | `Venue A` | prefixes every source name in Open Live |

Both machines need to be able to reach the internet outbound. Nothing inbound is required in the
default caller mode, which sends through Open Live's SRT relay when Open Live has one and to a port
on its Strom when it does not. `--uplink-mode relay` insists on the relay; `--uplink-mode listener`
has the cloud dial this machine instead (README, "Setup", has the three modes).

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
  nothing listening at http://127.0.0.1:8080 (...); up will start /usr/local/bin/strom (the one next to open-live-ingest)

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
| `up will start` names a different Strom than `which strom` | a `strom` in the same directory as `open-live-ingest` is used before `PATH`; to use another one, set `binary` under `[strom]` in `gateway.toml` to its full path |
| `Strom at ... wants a credential` | an existing Strom runs with `STROM_API_KEY`; `export OLI_STROM_API_KEY='<that key>'` and rerun |
| `Open Live publishes no SRT port range` | pass `--port-range 47110-47129` |
| `no SRT relay named` | pass `--relay <name>` with one of the names the message lists |
| `uplink.mode is "relay", but Open Live` | that Open Live has no relay: pass `--uplink-mode caller` |
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

Success: exit 0 and a JSON array, one object per device Strom can see, cameras first and then audio
sources:

```json
[
  { "name": "DeckLink SDI (1)", "id": "decklink-0", "kind": "video", "virtual": false },
  { "name": "OBS Virtual Camera", "id": "obs-0", "kind": "video", "virtual": true },
  { "name": "USB Microphone", "id": "dev-5e1a0c3f9b2d7e41", "kind": "audio", "virtual": false }
]
```

`virtual: true` devices are skipped by `up` unless `--all`. Audio sources are sent only when paired
with a camera: `up --audio "DeckLink=USB Microphone"` (repeat for more cameras), or an `[[input]]`
entry in the settings with `device` and `audio`. One audio source goes with one camera. An empty array means no capture device
is connected or recognised; `up --test` streams a test pattern and tone instead, to commission the
link before cameras arrive.

With `[[input]]` entries in the settings (the desktop app writes them; `gateway.toml.example` shows
the fields), `up` sends exactly those inputs, each optionally with an audio source and a `label` for
its Studio name, and skips one that is not connected. `--devices`, `--all` and `--test` pick for
one run instead.

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

## Mac: install from the DMG, open the app

On an Apple silicon Mac, the app replaces step 1 and needs no terminal. Only what differs is listed
here.

Install: download `open-live-ingest-macos-aarch64.dmg` from
<https://github.com/Eyevinn/open-live-ingest/releases/latest>, open it, and drag
**Open Live Ingest** onto the Applications folder next to it. Then open Open Live Ingest from
Applications.

Success: a window titled Open Live Ingest opens, and no terminal does. The app carries its own
Strom and GStreamer; it neither installs nor uses another one. macOS asks once for the camera and
the microphone, the first time a feed uses them: allow both.

If the release has no DMG, it is not yet notarized with Apple (#25): use step 1's `install.sh`
instead. A DMG from a release workflow run's artifacts is signed ad hoc, and macOS refuses to open
the app once it has been downloaded.

For SSH use, and for steps 2 to 7 as written, put the CLI that comes with the app on `PATH` (once,
from Terminal on the Mac or over SSH):

```bash
sudo mkdir -p /usr/local/bin
sudo ln -sf "/Applications/Open Live Ingest.app/Contents/MacOS/open-live-ingest" /usr/local/bin/open-live-ingest
open-live-ingest --version   # open-live-ingest X.Y.Z, the app's version
```

Success: exit 0 and the version line. `open-live-ingest: command not found` means `/usr/local/bin`
is not on `PATH`; add it (`export PATH="/usr/local/bin:$PATH"`). Through the symlink the CLI runs
the app's own Strom and reads the same settings file as the app, so a box set up in the window
answers `open-live-ingest status` and `down`, and the launchd agent of step 5 (which runs
`/usr/local/bin/open-live-ingest`) works unchanged. The symlink replaces an `open-live-ingest`
that `install.sh` put there. Deleting the app breaks it: remove `/usr/local/bin/open-live-ingest`
with the app.

## Windows

On 64-bit (x86_64) Windows the steps are the same, in PowerShell rather than a POSIX shell. Only
what differs is listed here.

Install:

```powershell
irm https://raw.githubusercontent.com/Eyevinn/open-live-ingest/main/install.ps1 | iex
```

Success: no red error text, and the last lines read

```
==> Installed C:\Users\<user>\AppData\Local\Programs\open-live-ingest\open-live-ingest.exe (open-live-ingest X.Y.Z)
==> Added C:\Users\<user>\AppData\Local\Programs\open-live-ingest to your user PATH; consoles opened from now on find open-live-ingest
==> Downloading Strom vX.Y.Z (strom-X.Y.Z-windows-x86_64.msi)
...
==> Installed C:\Program Files\Strom\bin\strom.exe
...
==> Done. Next: open-live-ingest setup, then open-live-ingest up
```

The gateway needs no administrator rights. Strom's MSI does, so in a console that is not elevated
Windows asks once to allow `msiexec`; refusing stops the script with `elevation was refused`. If a
Strom is already installed (`strom.exe` on PATH or in `%ProgramFiles%\Strom\bin`) the script
leaves it alone and says `Strom already installed at ...`. The MSI only runs once it matches the
SHA-256 digest GitHub records for it. If the script cannot read that digest (`cannot read the Strom
... release from the GitHub API`, typically the unauthenticated rate limit on a shared network),
it stops: set `GH_TOKEN` and rerun, or rerun with `SKIP_STROM=true` and install the Strom MSI
from its release page yourself. Then **open a new console**, so the user PATH and Strom's
`GST_PLUGIN_PATH` apply.

The binary is unsigned, so SmartScreen or Defender may warn on the first run; allow it.

Credential (step 2):

```powershell
Set-Item Env:OLI_OPEN_LIVE_API_KEY '<token>'
```

Settings are saved to `%APPDATA%\open-live-ingest\gateway.toml`, readable by your user, SYSTEM
and Administrators only.

Start (step 5): there is no Windows service. Run `open-live-ingest up` in a console and leave
the window open. Ctrl-C, or closing the window, stops and removes the feeds. `open-live-ingest
down` from a second console stops a running `up` the same way.

Not covered on Windows: ARM64 (there is no build) and running as a Windows service.
