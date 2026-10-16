#!/usr/bin/env bash
# Handler for vrplay: links, sent by the "Play in VR" bookmark in the browser.
#   vrplay:<url-encoded page URL>   (also accepts vrplay://...)
# Stops any player that is already running, then plays the page's video.
dir="$(cd "$(dirname "$(readlink -f "$0")")" && pwd)"
link="${1#vrplay:}"
link="${link#//}"
url="$(python3 -c 'import sys, urllib.parse; print(urllib.parse.unquote(sys.argv[1]))' "$link")"
case "$url" in
  http://*|https://*) ;;
  *) echo "vrplay: ignoring non-web link: $url" >&2; exit 1 ;;
esac
pkill -x vr-player 2>/dev/null && sleep 1
exec "$dir/vr-player-launcher.sh" "$url"
