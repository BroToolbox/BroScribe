# Linux XFCE verification harness

This arm64 Docker desktop uses Xvfb, Mesa CPU rendering and the Debian snapshot
from 2026-11-18. Build dependencies and the Rust image pin come from
`scripts/docker-linux-test.sh`. The build can fetch the locked registry and
pinned sherpa-onnx archive. The desktop runs as the `qa` user without network
access or Linux capabilities. The repository and host profile stay outside
the running desktop. Only the evidence folder is mounted for writing.

```sh
spikes/linux-desktop-harness/build.sh
spikes/linux-desktop-harness/run.sh
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
`damage.py` accepts an X11 window ID and duration in seconds.

Stopping the desktop removes its container and synthetic profile. The
`broscribe-linux-cargo` and `broscribe-linux-target` volumes are reusable
build caches. No model downloads are needed for the UI fixtures.
`recording-check.sh` can also exercise the real agent and final notification
when the default profile's verified models are copied into
`/home/qa/profiles/main/models` before launching the agent.
`panel-fixture.py` can supply a synthetic recording state for layout
rechecks without audio. Start it before the app and give both processes
the same fresh `BROSCRIBE_DATA_DIR` and `BROSCRIBE_LIBRARY` in the desktop.
