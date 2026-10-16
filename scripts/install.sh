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
chmod +x "$dest/vr-player" "$dest/vr-player-launcher.sh"
ln -sf "$dest/vr-player-launcher.sh" "$bin/vr-player"

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
echo "Controls: A = switch 360/180/flat, B = quit. Log: ~/vr-player.log"
