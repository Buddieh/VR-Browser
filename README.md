# VR-Browser

A lightweight, video-first VR browser for Steam Frame (SteamOS, ARM64), for
personal use. Goal: open a website, find a video, and watch it in VR (flat,
180, 360, side-by-side 3D) smoothly. WebXR sites should work too. DRM content
is out of scope for now.

## On the Frame: install the VR player (current step)

Open **Konsole** in Desktop mode and paste this. It installs (or updates) the
player and downloads two small 360 sample videos:

```sh
curl -fsSL https://raw.githubusercontent.com/Buddieh/VR-Browser/claude/project-thread-g1s0zv/scripts/install.sh | bash
```

Then play a sample (still in Konsole, while wearing the headset):

```sh
~/.local/bin/vr-player ~/Videos/vr-samples/360-mono.mp4 --mode 360
~/.local/bin/vr-player ~/Videos/vr-samples/360-stereo-tb.mp4 --mode 360 --stereo tb
```

Play any video page (YouTube and other sites yt-dlp supports):

```sh
~/.local/bin/vr-player 'https://www.youtube.com/watch?v=VIDEO_ID'
```

**Play in VR from Chromium:** open `file:///home/steamos/Downloads/play-in-vr.html`
in Chromium and drag the button onto the bookmarks bar. On a video page, click
it; the first time, allow Chromium to open the VR player.

Controls: **A** switches 360 / 180 / flat, **X** switches mono / side-by-side /
top-bottom 3D, **B** quits. The layout is detected from the video's metadata
or title, otherwise it starts on a flat screen. Every run writes
`~/vr-player.log`; paste it into the project thread if something looks wrong.

Options: `--mode 360|180|flat`, `--stereo mono|sbs|tb`, `--sw` (software
decode, for comparison), `--start SECONDS`, `--max-width N` (default 4096),
`--screen METERS` (flat screen width).

<details><summary>Earlier hardware checks</summary>

```sh
curl -fsSL https://raw.githubusercontent.com/Buddieh/VR-Browser/claude/project-thread-g1s0zv/scripts/frame-probe.sh -o ~/frame-probe.sh && bash ~/frame-probe.sh
curl -fsSL https://raw.githubusercontent.com/Buddieh/VR-Browser/claude/project-thread-g1s0zv/scripts/ffmpeg-decode-test.sh -o ~/ffmpeg-decode-test.sh && bash ~/ffmpeg-decode-test.sh
```

Result (2026-10-16): the iris hardware decoder works through the system
ffmpeg (H.264, H.265, VP9); the system GStreamer has no usable decoders.
</details>

## Plan

1. ~~**Test Wolvic on the Frame.**~~ Done 2026-10-16: the Frame build does not
   launch, and Android apps get no hardware video decode. See
   [docs/step1-wolvic-test.md](docs/step1-wolvic-test.md).
2. **VR player first** (decided 2026-10-16): [player/](player/) is a native
   OpenXR app that shows 360/180/flat, mono/SBS/TB video using OpenXR
   composition layers, decoded by the system ffmpeg on the Frame's iris
   hardware decoder. Browsing happens in the Chromium already on the Frame;
   next comes a "Play in VR" hand-off (with yt-dlp for sites like YouTube).
   A full in-app browser (WPE WebKit) and WebXR come later.
3. **Installs and updates:** GitHub Actions builds an ARM64 release; install
   with one command; the app checks GitHub Releases for updates.
4. **Playback hardening:** codec checks (H.264/HEVC/AV1), quick layout
   switching, per-site settings.
