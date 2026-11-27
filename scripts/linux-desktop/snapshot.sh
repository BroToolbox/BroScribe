#!/usr/bin/env bash
set -euo pipefail
name="${1:?evidence name}"
window="${2:-$(xdotool getactivewindow)}"
pid="${3:-$(cat "$HOME/app.pid")}"
{
  xdotool getwindowname "$window"
  xdotool getwindowgeometry "$window"
} > "/evidence/$name.window.txt"
import -window "$window" "/evidence/$name.png"
python3 /harness/a11y.py "$pid" dump > "/evidence/$name.atspi.json"
