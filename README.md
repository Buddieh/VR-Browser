# VR-Browser

A lightweight, video-first VR browser for Steam Frame (SteamOS, ARM64), for
personal use. Goal: open a website, find a video, and watch it in VR (flat,
180, 360, side-by-side 3D) smoothly. WebXR sites should work too. DRM content
is out of scope for now.

## Plan

1. ~~**Test Wolvic on the Frame.**~~ Done 2026-10-16: the Frame build does not
   launch, and Android apps get no hardware video decode. See
   [docs/step1-wolvic-test.md](docs/step1-wolvic-test.md).
2. **Build a native app:** a WPE WebKit browser panel inside a native OpenXR
   app. WPE plays `<video>` through GStreamer, which can use the Frame's
   `qcom-iris` V4L2 hardware decoder; 180/360/SBS video is rendered natively.
   First check: run [scripts/frame-probe.sh](scripts/frame-probe.sh) on the
   headset to confirm what's available.
3. **Installs and updates:** GitHub Actions builds an ARM64 release; install
   with one command; the app checks GitHub Releases for updates.
4. **Playback hardening:** codec checks (H.264/HEVC/AV1), quick layout
   switching, per-site settings.
