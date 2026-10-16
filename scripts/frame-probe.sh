#!/usr/bin/env bash
# Read-only probe of a Steam Frame: what the VR browser can build on.
# Changes no settings and needs no sudo; only writes ~/frame-probe.txt and
# test clips in ~/frame-probe-videos. Run in Konsole (Desktop mode) or over SSH:
#   bash frame-probe.sh [path/to/test-video.mp4]
# Without a file it downloads two ~10 MB test clips to ~/frame-probe-videos.
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

# --- Decode test -----------------------------------------------------------
# Uses the file given as $1, or downloads two 10-second 1080p test clips
# (H.264 and H.265, ~10 MB each) from test-videos.co.uk.
clips=()
if [ -n "$1" ] && [ -f "$1" ]; then
  clips=("$1")
elif have curl; then
  vdir="$HOME/frame-probe-videos"
  mkdir -p "$vdir"
  base="https://test-videos.co.uk/vids/jellyfish/mp4"
  for codec in h264 h265; do
    f="$vdir/jellyfish-1080-$codec.mp4"
    [ -s "$f" ] || curl -fsSL -o "$f" "$base/$codec/1080/Jellyfish_1080_10s_10MB.mp4" || { echo "download of $codec clip failed"; rm -f "$f"; }
    [ -s "$f" ] && clips+=("$f")
  done
fi

decode() { # $1=file $2=pipeline tail after demux; prints seconds or FAIL
  local t0 t1
  t0=$(date +%s.%N)
  if timeout 60 gst-launch-1.0 -q filesrc location="$1" ! qtdemux ! $2 ! fakesink sync=false >/dev/null 2>&1; then
    t1=$(date +%s.%N); awk -v a="$t0" -v b="$t1" 'BEGIN{printf "%.2fs\n", b-a}'
  else
    echo "FAIL"
  fi
}

if have gst-launch-1.0 && [ ${#clips[@]} -gt 0 ]; then
  section "Decode test (10-second clips; lower time = faster)"
  for f in "${clips[@]}"; do
    echo "--- $f"
    case "$f" in *h265*|*hevc*) codecs="h265";; *h264*) codecs="h264";; *) codecs="h264 h265";; esac
    for c in $codecs; do
      gst-inspect-1.0 "v4l2${c}dec" >/dev/null 2>&1 && echo "hardware v4l2${c}dec: $(decode "$f" "${c}parse ! v4l2${c}dec")"
      gst-inspect-1.0 "avdec_${c}" >/dev/null 2>&1 && echo "software avdec_${c}: $(decode "$f" "${c}parse ! avdec_${c}")"
    done
    echo "auto (decodebin3): $(timeout 60 gst-launch-1.0 -q filesrc location="$f" ! parsebin ! decodebin3 ! fakesink sync=false >/dev/null 2>&1 && echo ok || echo FAIL)"
  done
fi

printf '\nSaved to %s\n' "$out"
