# VR-Browser

A lightweight, video-first VR browser for Steam Frame (SteamOS, ARM64), for
personal use. Goal: open a website, find a video, and watch it in VR (flat,
180, 360, side-by-side 3D) smoothly. WebXR sites should work too. DRM content
is out of scope for now.

## Plan

1. **Test Wolvic on the Frame.** If an existing VR browser is good enough, we
   don't build one. See [docs/step1-wolvic-test.md](docs/step1-wolvic-test.md).
2. **If not, build a native app:** a small browser panel (WPE WebKit) for
   finding videos, with playback handed to a native OpenXR video renderer
   (Rust, Vulkan, hardware decode) for 180/360/SBS.
3. **Installs and updates:** GitHub Actions builds an ARM64 release; install
   with one command; the app checks GitHub Releases for updates.
4. **Playback hardening:** codec checks (H.264/HEVC/AV1), quick layout
   switching, per-site settings.
