#!/usr/bin/env bash
# Runs the player and keeps a log at ~/vr-player.log for troubleshooting.
dir="$(cd "$(dirname "$(readlink -f "$0")")" && pwd)"
"$dir/vr-player" "$@" 2>&1 | tee "$HOME/vr-player.log"
exit "${PIPESTATUS[0]}"
