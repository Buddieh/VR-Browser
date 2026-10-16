#!/usr/bin/env bash
# Times hardware (iris via V4L2 mem2mem) vs software decode with the system
# ffmpeg, using the clips frame-probe.sh downloaded. Changes nothing.
# Output goes to the screen and to ~/ffmpeg-decode-test.txt.

out="$HOME/ffmpeg-decode-test.txt"
exec > >(tee "$out") 2>&1
vdir="$HOME/frame-probe-videos"

run() { # $1=label $2=decoder $3=file
  local t0 t1 log
  log=$(mktemp)
  t0=$(date +%s.%N)
  if timeout 60 ffmpeg -hide_banner -nostdin -benchmark -c:v "$2" -i "$3" -f null - >"$log" 2>&1; then
    t1=$(date +%s.%N)
    printf '%-10s %-14s OK   %ss  frames=%s\n' "$1" "$2" \
      "$(awk -v a="$t0" -v b="$t1" 'BEGIN{printf "%.2f", b-a}')" \
      "$(grep -oE 'frame= *[0-9]+' "$log" | tail -1 | tr -dc 0-9)"
  else
    printf '%-10s %-14s FAIL\n' "$1" "$2"
    grep -iE 'error|fail|invalid|could not|unsupported' "$log" | head -5 | sed 's/^/    /'
  fi
  rm -f "$log"
}

echo "=== ffmpeg decode test (10 s, 1080p, ~300 frames each) ==="
run h264 h264_v4l2m2m "$vdir/jellyfish-1080-h264.mp4"
run h264 h264         "$vdir/jellyfish-1080-h264.mp4"
run h265 hevc_v4l2m2m "$vdir/jellyfish-1080-h265.mp4"
run h265 hevc         "$vdir/jellyfish-1080-h265.mp4"

echo
echo "=== decoder device access ==="
id -nG
getfacl -p /dev/video-dec0 2>/dev/null | grep -v '^#' || ls -lL /dev/video-dec0

printf '\nSaved to %s\n' "$out"
