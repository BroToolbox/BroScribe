# Linux XFCE desktop harness

This arm64 Docker desktop uses Xvfb, Mesa CPU rendering and the Debian snapshot
from 2026-11-18. Build dependencies and the Rust image pin come from
`scripts/docker-linux-test.sh`. `build.sh` builds the image, fetches and
verifies the pinned sherpa-onnx archive inside it and builds the debug
binaries; only that build has network access, for the locked registry and the
pinned archive. The desktop runs as the `qa` user without network access or
Linux capabilities. The repository and host profile stay outside the running
desktop. Only the evidence folder is mounted for writing. The harness is CI
infrastructure: the [screenshots workflow](../../.github/workflows/screenshots.yml)
runs it, so changes here get the same line-by-line review as the workflows.

## README screenshots

```sh
scripts/linux-desktop/screenshots.sh /tmp/broscribe-shots
```

This builds as above, starts the screenshot desktop in a fresh, uniquely named
container and removes it afterwards. The output folder gets exactly
`library.png`, `meeting.png`, `meeting-dark.png`, `recording.png` (1280×800
each, no PNG metadata) and `manifest.json` with the source commit and each
image's file and pixel SHA-256. Logs and AT-SPI dumps go to
`/tmp/broscribe-shots.diagnostics` (or a second argument) and are never
published; in CI they are uploaded as a separate short-lived
`screenshots-diagnostics` artifact only when the capture fails.
`CARGO_BUILD_JOBS` defaults to 2; more can exhaust Docker Desktop's memory.
The host needs Docker and GNU `timeout` (coreutils). The cargo build runs with
`RUSTUP_TOOLCHAIN` set to the image's toolchain and `RUSTUP_AUTO_INSTALL=0`,
so rustup downloads nothing.

Everything shown is invented (`showcase.py`). `seed.py --showcase` writes the
meetings and people, then uses the CLI for the rest: `speakers name`
confirms every speaker, and `enrich` runs a synthetic local agent CLI,
`showcase-notes` (shown as "Local notes model"), the only enabled provider,
which answers with the invented notes. Updates are off and the text size is
the default. These seeding CLI calls also run under libfaketime, at the seed
clock (`showcase.SEED_CLOCK`, ticking), so revision times fall on the pinned
"today".

The screenshot desktop (`BROSCRIBE_SHOT_MODE=1`) is a fixed 1440×900 screen
at 96 DPI and scale 1 with xfwm4 and no compositor, panel, desktop or
notification daemon. Besides the seeding CLI calls above, only
`broscribe-app` (and the CLI it starts) runs under libfaketime, on a ticking
clock from 2026-11-24 10:30 UTC; the agent, the session and Xvfb use the real
clock. `screenshots.sh` waits up to 120 s for the desktop (failing at once if
the container exits) and gives the capture 20 minutes; `capture.sh` polls for
each window for up to 60 s, naming the window and checking that the app (and
the panel fixture) is still running. It waits for each screen's own content
and for three identical captures in a row about 300 ms apart, checks that no
loading, offline or error state and no "no tray icons" banner or Dismiss
button is on screen, and fails rather than saving a half-loaded screen. The
recording controls come from `panel-fixture.py --showcase`, a paused
recording whose time does not tick; that image is the main window's area with
the controls in front of it.
`shots.py` writes and validates the manifest; the publish job compares only
the pixel hashes.

## Manual QA desktop

```sh
scripts/linux-desktop/build.sh
scripts/linux-desktop/run.sh
docker exec broscribe-linux-xfce test -f /home/qa/desktop-ready
docker exec broscribe-linux-xfce /harness/inside.sh python3 /harness/seed.py
docker exec -d broscribe-linux-xfce /harness/inside.sh bash -c 'exec broscribed > ~/logs/agent.log 2>&1'
docker exec -d broscribe-linux-xfce /harness/inside.sh bash -c 'echo $$ > ~/app.pid; exec broscribe-app > ~/logs/app.log 2>&1'
docker exec broscribe-linux-xfce /harness/inside.sh /harness/main-matrix.sh
docker exec broscribe-linux-xfce /harness/inside.sh bash -c 'python3 /harness/a11y.py "$(cat ~/app.pid)" dump'
docker exec broscribe-linux-xfce /harness/inside.sh import -window root /evidence/desktop.png
docker stop broscribe-linux-xfce
```

The app launches with 120 invented meetings, 40 invented people and the
Largest text preference once seeded. The audio is a generated tone. Start
the desktop again for a fresh profile. Evidence defaults to
`/tmp/broscribe-linux-verify/`; override it with `BROSCRIBE_XFCE_EVIDENCE`.
The registry cache is mounted read-only so the debug binary can read its
dependency-provided SVG assets.

The Settings portal uses the Debian GTK backend. Change its preference with
`gsettings set org.gnome.desktop.a11y.interface high-contrast true` on the
harness bus. Use `gsettings set org.gnome.desktop.interface color-scheme prefer-dark`
for dark mode. Inspect the actual portal value with
`gdbus call --session -d org.freedesktop.portal.Desktop -o /org/freedesktop/portal/desktop -m org.freedesktop.portal.Settings.Read org.freedesktop.appearance contrast`.
Run `contrast.sh` inside the session with the X11 window ID, app PID and
`light` or `dark`. It records the color change and samples the app with `top`.
`damage.py` accepts an X11 window ID and duration in seconds. `a11y.py`
finds, clicks or locates (window coordinates) named AT-SPI nodes; `--contains`
matches inside a name.

Stopping the desktop removes its container and synthetic profile. The
`broscribe-linux-cargo` and `broscribe-linux-target` volumes are reusable
build caches. No model downloads are needed for the UI fixtures.
`recording-check.sh` can also exercise the real agent and final notification
when the default profile's verified models are copied into
`/home/qa/profiles/main/models` before launching the agent.
`panel-fixture.py` can supply a synthetic recording state for layout
rechecks without audio. Start it instead of the agent, before the app, and
give both processes the same fresh `BROSCRIBE_DATA_DIR` and
`BROSCRIBE_LIBRARY` in the desktop.
