#!/usr/bin/env bash
# Time a real GTK settings-portal change and sample the idle app process.
set -euo pipefail
window="${1:?X11 window ID}"
pid="${2:?app PID}"
appearance="${3:?light or dark}"
read_portal() {
  gdbus call --session -d org.freedesktop.portal.Desktop -o /org/freedesktop/portal/desktop \
    -m org.freedesktop.portal.Settings.ReadOne org.freedesktop.appearance contrast
}
snap() {
  import -window "$window" "$1"
}
gsettings set org.gnome.desktop.interface color-scheme "prefer-$appearance"
gsettings set org.gnome.desktop.a11y.interface high-contrast false
sleep 5.3
read_portal
snap "/evidence/contrast-${appearance}-regular.png"
baseline="$(magick identify -format '%#' "/evidence/contrast-${appearance}-regular.png")"
start="$(date +%s%3N)"
gsettings set org.gnome.desktop.a11y.interface high-contrast true
read_portal
changed=false
for _ in {1..30}; do
  snap "/evidence/contrast-${appearance}-more.png"
  current="$(magick identify -format '%#' "/evidence/contrast-${appearance}-more.png")"
  if [[ "$baseline" != "$current" ]]; then
    printf 'Window pixels changed after %s ms\n' "$(( $(date +%s%3N) - start ))"
    changed=true
    break
  fi
  sleep .1
done
"$changed"
top -b -p "$pid" -d 1 -n 12
gsettings set org.gnome.desktop.a11y.interface high-contrast false
