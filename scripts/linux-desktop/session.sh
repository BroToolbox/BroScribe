#!/usr/bin/env bash
# The desktop session. BROSCRIBE_SHOT_MODE=1 starts the screenshot desktop:
# a fixed 1440x900 screen at 96 DPI and scale 1, xfwm4 without compositor,
# and no panel, desktop or notification daemon that could overlap the app.
set -euo pipefail
umask 077

export DISPLAY=:91 XDG_RUNTIME_DIR=/tmp/broscribe-xdg XDG_CURRENT_DESKTOP=XFCE
export DESKTOP_SESSION=xfce XDG_SESSION_TYPE=x11 QT_ACCESSIBILITY=1
export LIBGL_ALWAYS_SOFTWARE=1 GALLIUM_DRIVER=llvmpipe
export VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.json
export PATH=/target/debug:$PATH BROSCRIBE_CLI_BIN=/target/debug/broscribe
export BROSCRIBE_DATA_DIR="$HOME/profiles/main" BROSCRIBE_LIBRARY="$HOME/library"
shot_mode="${BROSCRIBE_SHOT_MODE:-0}"
if [[ $shot_mode == 1 ]]; then
  export TZ=UTC LC_ALL=C.UTF-8 LANG=C.UTF-8 LP_NUM_THREADS=1 GPUI_X11_SCALE_FACTOR=1
fi

if [[ ${1:-} != desktop ]]; then
  mkdir -m 700 "$XDG_RUNTIME_DIR"
  if [[ $shot_mode == 1 ]]; then
    # Nothing may pop up over the app: notifications go nowhere.
    mkdir -p "$HOME/.local/share/dbus-1/services"
    printf '[D-BUS Service]\nName=org.freedesktop.Notifications\nExec=/bin/false\n' \
      > "$HOME/.local/share/dbus-1/services/org.freedesktop.Notifications.service"
  fi
  exec dbus-run-session -- "$0" desktop
fi

mkdir -p "$HOME/.config/xdg-desktop-portal" "$HOME/logs"
printf '[preferred]\ndefault=gtk\n' > "$HOME/.config/xdg-desktop-portal/portals.conf"
# inside.sh sources this from another process: write it whole, then rename.
env | sed -n '/^\(DBUS_SESSION_BUS_ADDRESS\|DISPLAY\|XDG_RUNTIME_DIR\|XDG_CURRENT_DESKTOP\|LIBGL_ALWAYS_SOFTWARE\|GALLIUM_DRIVER\|VK_ICD_FILENAMES\|PATH\|BROSCRIBE_CLI_BIN\|BROSCRIBE_DATA_DIR\|BROSCRIBE_LIBRARY\|TZ\|LC_ALL\|LANG\|LP_NUM_THREADS\|GPUI_X11_SCALE_FACTOR\)=/s/^/export /p' > "$HOME/session.env.tmp"
mv -f "$HOME/session.env.tmp" "$HOME/session.env"

screen=(-screen 0 1600x1080x24)
if [[ $shot_mode == 1 ]]; then screen=(-screen 0 1440x900x24 -dpi 96); fi
Xvfb "$DISPLAY" "${screen[@]}" -nolisten tcp > "$HOME/logs/xvfb.log" 2>&1 &
for _ in {1..60}; do xdpyinfo >/dev/null 2>&1 && break; sleep .1; done
xdpyinfo >/dev/null
if [[ $shot_mode == 1 ]]; then
  printf 'Xft.dpi: 96\n' | xrdb -merge
fi
dbus-update-activation-environment DISPLAY XDG_RUNTIME_DIR XDG_CURRENT_DESKTOP DESKTOP_SESSION XDG_SESSION_TYPE
gdbus call --session -d org.a11y.Bus -o /org/a11y/bus \
  -m org.freedesktop.DBus.Properties.Set org.a11y.Status IsEnabled '<true>'
gnome-keyring-daemon --start --components=secrets > "$HOME/logs/keyring.log" 2>&1
pipewire > "$HOME/logs/pipewire.log" 2>&1 &
for _ in {1..60}; do [[ -S "$XDG_RUNTIME_DIR/pipewire-0" ]] && break; sleep .1; done
pipewire-pulse > "$HOME/logs/pipewire-pulse.log" 2>&1 &
wireplumber > "$HOME/logs/wireplumber.log" 2>&1 &
pw-cli create-node adapter '{ factory.name = support.null-audio-sink node.name = qa_output node.description = "Synthetic playback" media.class = Audio/Sink object.linger = true audio.position = [ FL FR ] }'
pw-cli create-node adapter '{ factory.name = support.null-audio-sink node.name = qa_input node.description = "Synthetic microphone" media.class = Audio/Source object.linger = true audio.position = [ MONO ] }'
node_id() {
  pw-dump | jq -er --arg name "$1" '.[] | select(.info.props["node.name"] == $name) | .id'
}
for _ in {1..30}; do
  if wpctl set-default "$(node_id qa_output)" && wpctl set-default "$(node_id qa_input)"; then
    break
  fi
  sleep .2
done
wpctl status

if [[ $shot_mode == 1 ]]; then
  # One window manager with one compositor state, nothing else on screen.
  xfwm4 --compositor=off > "$HOME/logs/xfwm.log" 2>&1 &
  desktop=$!
  for _ in {1..60}; do wmctrl -m >/dev/null 2>&1 && break; sleep .2; done
  wmctrl -m
  touch "$HOME/desktop-ready"
  wait "$desktop"
  exit
fi

xfce4-session > "$HOME/logs/xfce.log" 2>&1 &
desktop=$!
for _ in {1..60}; do
  if wmctrl -m >/dev/null 2>&1; then break; fi
  sleep .2
done
if ! wmctrl -m >/dev/null 2>&1; then
  xfwm4 --compositor=off > "$HOME/logs/xfwm.log" 2>&1 &
fi
if ! pgrep -x xfsettingsd >/dev/null; then
  xfsettingsd --daemon
fi
if ! pgrep -x xfce4-panel >/dev/null; then
  xfce4-panel --disable-wm-check > "$HOME/logs/panel.log" 2>&1 &
fi
wmctrl -m
touch "$HOME/desktop-ready"
# Keep the bus alive for inspection with docker exec.
wait "$desktop"
