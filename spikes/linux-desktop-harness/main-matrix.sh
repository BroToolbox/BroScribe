#!/usr/bin/env bash
set -euo pipefail
pid="$(cat "$HOME/app.pid")"
node() { python3 /harness/a11y.py "$pid" "$@"; }
snap() {
  sleep .4
  /harness/snapshot.sh "largest-$scheme-$1"
}
for scheme in light dark; do
  gsettings set org.gnome.desktop.interface color-scheme "prefer-$scheme"
  sleep 1
  xdotool key ctrl+1
  node find "Moonberry planning rehearsal"
  snap library
  node click "Moonberry planning rehearsal"
  node find "Moonberry planning rehearsal" --role heading
  snap meeting
  xdotool key ctrl+3
  node find People --role heading
  node find "Moonberry Person 01"
  snap people
  xdotool key ctrl+2
  node find "Review queue" --role heading
  snap review
  xdotool key ctrl+comma
  node find General --role heading
  snap settings
  xdotool key ctrl+w
  sleep .4
done
