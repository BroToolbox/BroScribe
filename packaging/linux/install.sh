#!/bin/sh
# Install the extracted release for the current desktop user.
set -eu
umask 022

source_dir=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd -P)
# `install.sh --uninstall [options]` hands over to uninstall.sh, which runs
# the installed CLI's `broscribe uninstall` with the same options.
if [ "${1:-}" = --uninstall ]; then
    shift
    exec "$source_dir/uninstall.sh" "$@"
fi
[ "$#" -eq 0 ] || { echo "Usage: install.sh [--uninstall [options]]" >&2; exit 2; }
prefix="$HOME/.local/lib/broscribe"
case ${XDG_DATA_HOME:-} in
    /*) data=$XDG_DATA_HOME ;;
    *) data="$HOME/.local/share" ;;
esac
app_id=org.broscribe.BroScribe
desktop="share/applications/$app_id.desktop"
entry="$data/applications/$app_id.desktop"
bin="$HOME/.local/bin/broscribe"
sizes="22 32 64 128"
[ "$(id -u)" != 0 ] || { echo "Run this as your desktop user, without sudo." >&2; exit 1; }

# Reject control characters before using the path in a desktop entry.
# Exec tokens escape %, then shell-like reserved characters within quotes,
# then the desktop entry value's own backslash escapes.
app="$prefix/broscribe-app"
[ "$app" = "$(printf '%s' "$app" | LC_ALL=C tr -d '\000-\037\177')" ] || {
    echo "The install path contains a control character." >&2
    exit 1
}
exec_path=$(printf '%s' "$app" | sed 's/%/%%/g; s/[\\"$`]/\\&/g; s/\\/\\\\/g')

check_free() {
    if [ -e "$1" ] || [ -L "$1" ]; then
        printf 'Already exists: %s. Use the built-in updater or uninstall BroScribe before reinstalling.\n' "$1" >&2
        exit 1
    fi
}
icon() { printf 'icons/hicolor/%sx%s/apps/%s.png' "$1" "$1" "$app_id"; }

copy_release() {
    for file in broscribe-app broscribe broscribed broscribe-worker broscribe-update-runner \
        LICENSE NOTICE THIRD_PARTY_NOTICES.txt third-party-inventory.json build-info.json install.sh uninstall.sh; do
        cp -p -- "$source_dir/$file" "$1/$file"
    done
    cp -R -- "$source_dir/share" "$1/share"
}

check_free "$prefix"
check_free "$entry"
check_free "$bin"
for size in $sizes; do check_free "$data/$(icon "$size")"; done
mkdir -p -- "$(dirname -- "$prefix")" "$data/applications" "$(dirname -- "$bin")"
for size in $sizes; do mkdir -p -- "$data/$(dirname -- "$(icon "$size")")"; done
mkdir -- "$prefix"

entry_created=false
desktop_tmp=
cleanup() {
    if [ -n "$desktop_tmp" ]; then rm -f -- "$desktop_tmp"; fi
    if "$entry_created"; then rm -f -- "$entry"; fi
    if [ "$(readlink -- "$bin" 2>/dev/null || :)" = "$prefix/broscribe" ]; then
        rm -f -- "$bin"
    fi
    for link in $(for size in $sizes; do icon "$size"; printf '\n'; done); do
        if [ "$(readlink -- "$data/$link" 2>/dev/null || :)" = "$prefix/share/$link" ]; then
            rm -f -- "$data/$link"
        fi
    done
    rm -rf -- "$prefix"
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM
copy_release "$prefix"
# Publish the completed desktop entry atomically without overwriting.
desktop_tmp=$(mktemp -- "$data/applications/.broscribe.XXXXXX")
sed "s|^Exec=broscribe-app\$|Exec=\"$(printf '%s' "$exec_path" | sed 's/[\\&|]/\\&/g')\"|" \
    "$source_dir/$desktop" > "$desktop_tmp"
chmod 644 "$desktop_tmp"
ln -T -- "$desktop_tmp" "$entry"
entry_created=true
rm -f -- "$desktop_tmp"
ln -sT -- "$prefix/broscribe" "$bin"
for size in $sizes; do
    link=$(icon "$size")
    ln -sT -- "$prefix/share/$link" "$data/$link"
done
trap - EXIT HUP INT TERM
printf 'Installed BroScribe in %s\nOpen it from the applications menu. See the install guide for removal.\n' "$prefix"
