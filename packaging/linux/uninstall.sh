#!/bin/sh
# Run the installed CLI so it can verify and remove its own installation.
set -eu

prefix="$HOME/.local/lib/broscribe"
cli="$prefix/broscribe"
if [ -f "$cli" ] && [ ! -L "$cli" ] && [ -x "$cli" ]; then
    exec "$cli" uninstall "$@"
fi
printf '%s\n' "Expected a regular executable at $cli. For a portable installation, run its broscribe uninstall command." >&2
exit 1
