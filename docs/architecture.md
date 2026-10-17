# How the VR player works

This page lists the standards, protocols and outside tools the project relies
on, and how they fit together. It describes the parts that are settled; fine
details of playback timing are still being tuned and live in the code
comments ([player/src/video.rs](../player/src/video.rs),
[player/src/main.rs](../player/src/main.rs)).

Keep this page up to date when a feature lands that adds or replaces a
protocol, format or outside tool.

## The pieces

```
Chromium (on the Frame)
  └─ "Play in VR" bookmark ── vrplay:<page URL> ──► vr-play-url.sh
                                                      │
                                                      ▼
                         vr-player (Rust, OpenXR + Vulkan, on SteamVR)
                           ├─ yt-dlp (+ Deno) ── web page ► stream URLs
                           ├─ ffprobe ───────── codec, size, fps, VR layout
                           ├─ ffmpeg ────────── video ► raw NV12 frames (pipe)
                           └─ ffplay ────────── audio ► speakers, clock on stderr
```

## Standards and protocols

| Area | What we use | Where |
| --- | --- | --- |
| XR runtime | **OpenXR 1.0** through the `openxr` crate, running on **SteamVR**, the Steam Frame's OpenXR runtime | `player/src/main.rs` |
| Graphics | **Vulkan 1.1** through `ash`, connected to OpenXR with `XR_KHR_vulkan_enable2`; shaders are GLSL compiled to SPIR-V at build time (`glslangValidator`) | `player/src/main.rs`, `player/shaders/`, `player/build.rs` |
| Display rate | `XR_FB_display_refresh_rate` (optional) to ask for a refresh rate that fits the video's frame rate; frames are blended when it isn't available | `match_refresh_rate` in `main.rs` |
| Picture | One OpenXR **projection layer** drawn by our own ray-cast shader (360 equirectangular, YouTube's equi-angular cubemap, 180, flat), plus a **quad layer** for the control panel | `main.rs`, `shaders/video.frag` |
| Stereo 3D | Mono, side-by-side and top-bottom frame packing, one half per eye | `shaders/video.frag` |
| Controllers | OpenXR action bindings for the Oculus Touch, Valve Index and Khronos simple controller profiles; the aim pose ray-casts onto the panel | `main.rs`, `player/src/ui.rs` |
| Video decode | System **ffmpeg** with the Frame's hardware decoder through **V4L2 mem2mem** (`h264_v4l2m2m`, `hevc_v4l2m2m`, `vp9_v4l2m2m`), software decode as a fallback | `player/src/video.rs` |
| Frame transport | Raw **NV12** (Y plane + interleaved UV) over a Unix pipe into GPU staging buffers; a shader converts YUV to RGB with BT.601 or BT.709 matrices, limited or full range, once per video frame | `video.rs`, `shaders/convert.frag` |
| Audio | **ffplay** plays the audio stream; the player reads ffplay's status line for the audio clock and paces video to it, so picture follows sound. ffplay reports raw timestamps, so the clock (and the `atrim` that cuts the sound before a seek target) is shifted by the input's first timestamp, which is often 1.4 s or 10 s in HLS and MPEG-TS. Pause and resume use `SIGSTOP`/`SIGCONT`; seeking restarts ffmpeg and ffplay at the new position | `video.rs` |
| Layout detection | Spherical-video and stereo-3D metadata read with ffprobe (the Google Spherical Video V2 boxes ffmpeg exposes as side data), YouTube's `vproj=mesh` stream tag for cubemaps, then the title or file name | `player/src/source.rs` |
| Web video | **yt-dlp** turns a web page into direct stream URLs (DASH/HLS/progressive over HTTPS), preferring codecs the Frame decodes in hardware; **Deno** is the JavaScript runtime yt-dlp needs for YouTube | `source.rs`, `scripts/install.sh` |
| Browser hand-off | A `javascript:` bookmarklet sends the page to a custom **`vrplay:` URL scheme**, registered as a freedesktop `.desktop` handler with `xdg-mime` | `scripts/play-in-vr.html`, `scripts/vr-play-url.sh`, `scripts/install.sh` |
| Containers | Whatever ffmpeg opens: MP4, MKV, WebM, MOV, TS, HLS (`.m3u8`), DASH (`.mpd`) | `source.rs` |

## Build and install

- **GitHub Actions** builds a release binary on a native ARM64 Ubuntu 24.04
  runner (same glibc as SteamOS on the Frame) and publishes it as the
  `nightly` pre-release ([.github/workflows/build.yml](../.github/workflows/build.yml)).
- [scripts/install.sh](../scripts/install.sh) downloads that release over
  HTTPS, checks its SHA-256, and installs the player, yt-dlp and Deno under
  `~/.local/share/vr-player`.

## Not used (yet)

- **WebXR** and an in-app browser (WPE WebKit) are planned but not built;
  browsing happens in the Chromium already on the Frame.
- **DRM** (Widevine) is out of scope.
- **GStreamer**: the Frame's system GStreamer has no usable decoders, so we
  use ffmpeg instead.

## Credits

This project was designed and written by **Claude**, an AI model made by
[Anthropic](https://www.anthropic.com), working through
[Claude Code](https://claude.com/claude-code), with direction, testing on the
Steam Frame and decisions by the project's owner,
[@Buddieh](https://github.com/Buddieh).

It stands on these open-source projects: [OpenXR](https://www.khronos.org/openxr/)
and [Vulkan](https://www.vulkan.org/) (Khronos), the Rust crates
[`openxr`](https://crates.io/crates/openxr) and [`ash`](https://crates.io/crates/ash),
[FFmpeg](https://ffmpeg.org/) (ffmpeg, ffprobe, ffplay),
[yt-dlp](https://github.com/yt-dlp/yt-dlp), [Deno](https://deno.com/), and the
[DejaVu](https://dejavu-fonts.github.io/) font used for the panel text
(see [player/assets/DejaVu-LICENSE](../player/assets/DejaVu-LICENSE)).
