#!/usr/bin/env bash
# Read-only probe of a Steam Frame: what the VR browser can build on.
# Changes nothing, needs no sudo. Run in Konsole (Desktop mode) or over SSH:
#   bash frame-probe.sh [path/to/test-video.mp4]
# Output goes to the screen and to ~/frame-probe.txt.

out="$HOME/frame-probe.txt"
exec > >(tee "$out") 2>&1

section() { printf '\n=== %s ===\n' "$1"; }
have() { command -v "$1" >/dev/null 2>&1; }

section "System"
grep -E '^(VERSION_ID|BUILD_ID|VARIANT_ID)=' /etc/os-release
uname -srm
ldd --version 2>/dev/null | head -1

section "Video devices (hardware decoder shows up here)"
ls -l /dev/video* 2>/dev/null || echo "no /dev/video* nodes"
if have v4l2-ctl; then
  for d in /dev/video*; do
    echo "--- $d"; v4l2-ctl -d "$d" --info 2>/dev/null | grep -E 'Driver name|Card type'
  done
else
  echo "v4l2-ctl not installed"
fi

section "GStreamer"
if have gst-inspect-1.0; then
  gst-inspect-1.0 --version | head -1
  echo "--- v4l2 elements:"
  gst-inspect-1.0 2>/dev/null | grep -i v4l2 || echo "none"
  echo "--- software decoders:"
  gst-inspect-1.0 2>/dev/null | grep -Ei 'avdec_h264|avdec_h265|dav1d|vp9dec|openh264' || echo "none"
else
  echo "gst-inspect-1.0 not installed"
fi

section "FFmpeg"
if have ffmpeg; then
  ffmpeg -hide_banner -version | head -1
  ffmpeg -hide_banner -decoders 2>/dev/null | grep -Ei 'v4l2m2m' || echo "no v4l2m2m decoders"
else
  echo "ffmpeg not installed"
fi

section "VA-API / Vulkan"
have vainfo && vainfo 2>&1 | head -20 || echo "vainfo not installed"
have vulkaninfo && vulkaninfo --summary 2>/dev/null | grep -E 'deviceName|driverName|apiVersion' || echo "vulkaninfo not installed"

section "OpenXR runtime"
cat "$HOME/.config/openxr/1/active_runtime.json" 2>/dev/null || echo "no user active_runtime.json"

section "WebKit / browsers already on the image"
ls /usr/lib/libWPE* /usr/lib/libwpe* /usr/lib/libwebkit* 2>/dev/null || echo "no WPE/WebKit libs in /usr/lib"
have flatpak && flatpak list --app --columns=application 2>/dev/null | grep -Ei 'chrom|firefox|cog|wpe|epiphany' || true

section "Free space"
df -h / /home | sed 1d

if [ -n "$1" ] && [ -f "$1" ] && have gst-launch-1.0; then
  section "Hardware decode test on $1 (10 s)"
  timeout 10 gst-launch-1.0 -q filesrc location="$1" ! parsebin ! decodebin3 ! fakesink sync=false 2>&1 | tail -5
  echo "--- which decoder was picked:"
  GST_DEBUG=decodebin3:4 timeout 10 gst-launch-1.0 filesrc location="$1" ! parsebin ! decodebin3 ! fakesink sync=false 2>&1 \
    | grep -oE '(v4l2[a-z0-9]*dec|avdec_[a-z0-9]+|dav1ddec|vp9dec|openh264dec)' | sort -u
fi

printf '\nSaved to %s\n' "$out"
