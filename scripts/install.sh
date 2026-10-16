#!/usr/bin/env bash
# Install or update the VR player on Steam Frame. Run it again to update.
#   curl -fsSL https://raw.githubusercontent.com/Buddieh/VR-Browser/claude/project-thread-g1s0zv/scripts/install.sh | bash
set -euo pipefail

repo="Buddieh/VR-Browser"
url="https://github.com/$repo/releases/download/nightly/vr-player-linux-arm64.tar.gz"
dest="$HOME/.local/share/vr-player"
bin="$HOME/.local/bin"
samples="$HOME/Videos/vr-samples"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

echo "Downloading the latest build..."
curl -fsSL "$url" -o "$tmp/vr-player.tar.gz"
curl -fsSL "$url.sha256" -o "$tmp/vr-player.tar.gz.sha256"
(cd "$tmp" && sed 's/vr-player-linux-arm64.tar.gz/vr-player.tar.gz/' vr-player.tar.gz.sha256 | sha256sum -c --quiet -)

rm -rf "$dest"
mkdir -p "$dest" "$bin"
tar -xzf "$tmp/vr-player.tar.gz" -C "$dest" --strip-components=1
chmod +x "$dest/vr-player" "$dest/vr-player-launcher.sh" "$dest/vr-play-url.sh"
ln -sf "$dest/vr-player-launcher.sh" "$bin/vr-player"

echo "Fetching yt-dlp (finds the video on web pages)..."
curl -fsSL "https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp_linux_aarch64" -o "$dest/yt-dlp"
chmod +x "$dest/yt-dlp"
echo "Fetching Deno (yt-dlp needs a JavaScript runtime for YouTube)..."
curl -fsSL "https://github.com/denoland/deno/releases/latest/download/deno-aarch64-unknown-linux-gnu.zip" -o "$tmp/deno.zip"
python3 -c 'import sys, zipfile; zipfile.ZipFile(sys.argv[1]).extract("deno", sys.argv[2])' "$tmp/deno.zip" "$dest"
chmod +x "$dest/deno"

echo "Registering vrplay: links for the Play in VR bookmark..."
apps="$HOME/.local/share/applications"
mkdir -p "$apps"
cat > "$apps/vr-player-url.desktop" <<DESKTOP
[Desktop Entry]
Type=Application
Name=VR Player
Comment=Play a web page's video in VR
Exec=$dest/vr-play-url.sh %u
MimeType=x-scheme-handler/vrplay;
NoDisplay=true
Terminal=false
DESKTOP
xdg-mime default vr-player-url.desktop x-scheme-handler/vrplay 2>/dev/null || true
update-desktop-database "$apps" 2>/dev/null || true
mkdir -p "$HOME/Downloads"
cp "$dest/play-in-vr.html" "$HOME/Downloads/play-in-vr.html"

echo "Fetching two small sample videos..."
mkdir -p "$samples"
base="https://raw.githubusercontent.com/mrdoob/three.js/dev/examples/textures"
[ -s "$samples/360-mono.mp4" ] || curl -fsSL "$base/pano.mp4" -o "$samples/360-mono.mp4"
[ -s "$samples/360-stereo-tb.mp4" ] || curl -fsSL "$base/MaryOculus.mp4" -o "$samples/360-stereo-tb.mp4"

echo
echo "Installed build $(cut -c1-7 "$dest/VERSION") to $dest"
echo "Try it (SteamVR must be running, i.e. you're in the headset):"
echo "  ~/.local/bin/vr-player ~/Videos/vr-samples/360-mono.mp4 --mode 360"
echo "  ~/.local/bin/vr-player ~/Videos/vr-samples/360-stereo-tb.mp4 --mode 360 --stereo tb"
echo "  ~/.local/bin/vr-player 'https://www.youtube.com/watch?v=...'   (any video page)"
echo "Controls: A = 360/180/flat, X = mono/side-by-side/top-bottom, B = quit. Log: ~/vr-player.log"
echo
echo "Play in VR from Chromium: open this file in Chromium and drag the button to the bookmarks bar:"
echo "  file://$HOME/Downloads/play-in-vr.html"
