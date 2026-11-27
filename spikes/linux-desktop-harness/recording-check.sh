#!/usr/bin/env bash
# Exercise native panel and notification delivery with offline synthetic speech.
# A verified default model cache is needed to produce a final transcript.
set -euo pipefail
pid="$(cat "$HOME/app.pid")"
a11y() { python3 /harness/a11y.py "$pid" "$@"; }
window() { wmctrl -lp | awk -v pid="$pid" -v title="$1" '$3 == pid && index($0, title) {print $1; exit}'; }
snap() { /harness/snapshot.sh "$1" "$2" "$pid"; }

gsettings set org.gnome.desktop.interface color-scheme prefer-light
xdotool key ctrl+1
sleep .5
main="$(window 'Library - BroScribe')"
wmctrl -ia "$main"
caller_reached="$(xdotool getactivewindow)"
ffmpeg -y -v error -f lavfi -i "flite=text=Tomorrow the Moonberry crew will review the cardboard observatory plans. Please send the paper telescope designs before the rehearsal.:voice=slt" -ar 48000 "$HOME/synthetic-speech.wav"
stdbuf -oL dbus-monitor --session \
  "type='method_call',interface='org.freedesktop.Notifications',member='Notify'" \
  "type='signal',interface='org.freedesktop.Notifications',member='ActionInvoked'" > /evidence/notification-dbus.txt &
monitor=$!
trap 'kill "$monitor" 2>/dev/null || true' EXIT
broscribe record start --title "Moonberry spoken rehearsal" --app "Synthetic fixture" --json > /evidence/record-start.json &
start=$!
python3 /harness/link-speech.py
wait "$start"
a11y find "Hide transcript" --timeout 20
panel="$(window 'Recording controls - BroScribe')"
[[ -n "$panel" ]]
printf 'Before/after automatic recording panel open: %s %s\n' "$caller_reached" "$(xdotool getactivewindow)"
wmctrl -ia "$panel"
sleep .5
snap largest-light-panel "$panel"
a11y click "Hide transcript"
sleep .5
snap largest-light-compact "$panel"
gsettings set org.gnome.desktop.interface color-scheme prefer-dark
sleep .7
snap largest-dark-compact "$panel"
a11y click "Show transcript"
sleep .5
snap largest-dark-panel "$panel"
wmctrl -ic "$panel"
sleep .6
broscribe record status --json > /evidence/record-after-close.json
jq -e '.recording.status == "recording"' /evidence/record-after-close.json
pw-play --target qa_output "$HOME/synthetic-speech.wav" > "$HOME/logs/play.log" 2>&1
sleep .5
broscribe record stop --json > /evidence/record-stop.json
for _ in {1..100}; do
  if grep -q 'Final transcript ready' /evidence/notification-dbus.txt; then
    broscribe daemon status --json > /evidence/notification-pending.json
    import -window root /evidence/final-notification.png
    printf 'XFCE received the final notification.\n'
    break
  fi
  sleep .5
done
grep -q 'Final transcript ready' /evidence/notification-dbus.txt
# Keep the notification monitor available for the separate body click.
trap - EXIT
printf '%s\n' "$monitor" > "$HOME/notification-monitor.pid"
