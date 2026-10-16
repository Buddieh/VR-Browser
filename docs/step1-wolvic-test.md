# Step 1: Does Wolvic already do the job on Steam Frame?

Goal: spend ~15 minutes finding out whether an existing VR browser is good
enough before we build anything.

## What to install

Upstream Wolvic (Igalia) ships no Steam Frame build. The only Frame build is a
community fork, marked "earliest test version":

- **APK:** [Wolvic-steam-arm64-gecko-generic-debug.apk](https://github.com/bryanlyon/wolvic/releases/download/v1/Wolvic-steam-arm64-gecko-generic-debug.apk)
  (from [bryanlyon/wolvic release v1](https://github.com/bryanlyon/wolvic/releases/tag/v1))
- Background: [Igalia/wolvic#1935](https://github.com/Igalia/wolvic/issues/1935)

Known issues reported by its author:

- Controllers show up as "hands", so pinch does not work and you may not be
  able to leave an immersive (WebXR) session without closing Wolvic.
- Anything touching the clipboard can crash it (Lepton has no clipboard service).
- Refresh rate is hard-coded to 72 Hz.

Also important: Lepton (the Frame's Android layer) reportedly has **no hardware
video decoding yet**, so Android apps decode video on the CPU. Expect high
resolution 180/360 video to be the weak spot.

## How to install it

Pick one:

- **On the headset (SteamOS 0.5.6 beta or newer):** switch to the beta in
  Settings > System > System Update Channel, then install the update and
  restart (check the version reads 0.5.6+). Download the APK in Desktop mode,
  right-click it and choose "Add to Steam" (not double-click). It appears in
  your Steam library. Community fallback in Konsole:
  `lepton install_app dev ~/Downloads/<file>.apk`.
- **From a PC with adb:** enable Developer Mode, launch "Lepton Development"
  from your library, then run:

  ```sh
  adb connect <headset-ip>     # "frame" hostname no longer resolves on 0.5.6 beta
  adb install Wolvic-steam-arm64-gecko-generic-debug.apk
  ```

## Tests

Mark each one pass / fail / partly, with a one-line note.

| # | Test | Result | Note |
|---|------|--------|------|
| 1 | App launches and you can type a URL and scroll a page with the controllers | | |
| 2 | WebXR: open <https://immersive-web.github.io/webxr-samples/>, enter "Immersive VR Session", then get back out | | |
| 3 | YouTube 360 video (search "360 video 4K"), plays without stutter at 1440p+ | | |
| 4 | YouTube VR180 video, looks 3D and correct | | |
| 5 | A side-by-side 3D video (YouTube or local file), can it be shown in 3D | | |
| 6 | Local video file from the headset's storage plays | | |
| 7 | 20 minutes of playback without a crash, overheating or audio drift | | |

## How we decide

- **Mostly pass (1, 2, 3, 7 pass):** use Wolvic. We may make a small fork to
  fix rough edges and set up updates. No new app.
- **Video fails (3, 4 or 7) but browsing works:** this is the expected
  outcome given Lepton's missing hardware decode. Go to step 2 and build the
  native app, where video plays through native hardware decode instead.
- **Doesn't launch:** go to step 2.

Report back in the project thread with the table filled in (a photo or a quick
list is fine).
