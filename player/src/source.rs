//! Turns what the user gave (a file, a direct media URL or a web page URL)
//! into streams ffmpeg can open, and guesses the VR layout.

use std::{path::PathBuf, process::Command};

use crate::{Projection, Stereo};

pub struct Source {
    /// What ffmpeg decodes video from.
    pub video: String,
    /// Separate audio stream (web pages often split audio and video), or
    /// `None` to take audio from `video` when it has any.
    pub audio: Option<String>,
    pub title: String,
}

const MEDIA_EXTENSIONS: &[&str] = &[".mp4", ".mkv", ".webm", ".mov", ".m4v", ".avi", ".ts", ".m3u8", ".mpd"];

fn is_web_page(input: &str) -> bool {
    let lower = input.to_ascii_lowercase();
    let path = lower.split(['?', '#']).next().unwrap_or("");
    (lower.starts_with("http://") || lower.starts_with("https://"))
        && !MEDIA_EXTENSIONS.iter().any(|e| path.ends_with(e))
}

/// A helper installed next to the player (yt-dlp, deno), if present.
fn bundled(name: &str) -> Option<PathBuf> {
    std::env::current_exe().ok()?.parent().map(|d| d.join(name)).filter(|p| p.exists())
}

pub fn resolve(input: &str, max_width: u32) -> Result<Source, String> {
    if !is_web_page(input) {
        let title = input.rsplit('/').next().unwrap_or(input).to_string();
        return Ok(Source { video: input.to_string(), audio: None, title });
    }

    // Prefer codecs the Frame decodes in hardware (VP9, H.264, HEVC) at a
    // width the player handles, then anything that fits. The hardware VP9
    // decoder takes 4K at 30 fps but not at 60 (it stalls even on a
    // downloaded clip), so high-resolution VP9 must be 30 fps or less, and
    // faster video tops out at 1440p.
    let w = max_width;
    let format = format!(
        "bv*[vcodec^=vp9][width<={w}][height>=1440][fps<=31]+ba/bv*[vcodec^=vp9][width<={w}][height<=1440]+ba/\
         bv*[vcodec^=avc1][width<={w}]+ba/bv*[vcodec^=hev][width<={w}]+ba/\
         bv*[width<={w}]+ba/b[width<={w}]/b"
    );
    let ytdlp = bundled("yt-dlp").unwrap_or_else(|| PathBuf::from("yt-dlp"));
    eprintln!("source: resolving {input} with {}", ytdlp.display());
    let mut cmd = Command::new(&ytdlp);
    if let Some(deno) = bundled("deno") {
        // YouTube needs a JavaScript runtime to unlock its streams.
        cmd.arg("--js-runtimes").arg(format!("deno:{}", deno.display()));
    }
    let out = crate::video::output_within(
        cmd.args(["--no-playlist", "--no-warnings", "-f", &format]).args(["--print", "title", "--print", "urls", input]),
        90,
    )?;
    if !out.status.success() {
        return Err(format!(
            "yt-dlp could not find a video on that page: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut lines = text.lines().filter(|l| !l.trim().is_empty());
    let title = lines.next().unwrap_or("").to_string();
    let urls: Vec<String> = lines.map(str::to_string).collect();
    let video = urls.first().cloned().ok_or("yt-dlp returned no stream URL")?;
    let audio = urls.get(1).cloned();
    eprintln!("source: \"{title}\" ({} stream{})", urls.len(), if urls.len() == 1 { "" } else { "s" });
    Ok(Source { video, audio, title })
}

/// Layout hints from ffprobe's spherical / stereo side data.
#[derive(Default)]
pub struct Hints {
    pub projection: Option<Projection>,
    pub stereo: Option<Stereo>,
}

pub fn probe_hints(video: &str) -> Hints {
    let mut h = Hints::default();
    // YouTube marks its equi-angular cubemap streams in the stream URL
    // ("xtags=vproj=mesh"); ffmpeg only reports them as an unknown type.
    let lower = video.to_ascii_lowercase();
    if lower.contains("vproj%3dmesh") || lower.contains("vproj=mesh") {
        h.projection = Some(Projection::Cube360);
    }
    let Ok(out) = crate::video::output_within(
        Command::new("/usr/bin/ffprobe")
            .args(["-v", "error", "-select_streams", "v:0", "-show_entries", "stream_side_data"])
            .args(["-of", "default=nw=1", video]),
        30,
    ) else {
        return h;
    };
    let text = String::from_utf8_lossy(&out.stdout).to_ascii_lowercase();
    if h.projection.is_none() && text.contains("projection=equirectangular") {
        // A partial sphere (bounds set) is almost always 180.
        let bounded = text.lines().any(|l| l.starts_with("bound_left=") && !l.ends_with("=0"));
        h.projection = Some(if bounded { Projection::Dome180 } else { Projection::Sphere360 });
    }
    if text.contains("type=top and bottom") || text.contains("type=topbottom") {
        h.stereo = Some(Stereo::TopBottom);
    } else if text.contains("type=side by side") || text.contains("type=sidebyside") {
        h.stereo = Some(Stereo::SideBySide);
    }
    h
}

/// Guess the layout from the title or file name when the stream carries no
/// metadata (common for web videos).
pub fn guess_from_name(name: &str) -> Hints {
    let n = name.to_ascii_lowercase();
    let has = |words: &[&str]| words.iter().any(|w| n.contains(w));
    let mut h = Hints::default();
    if has(&["vr180", "180°", "180 degree", "180vr", "_180", " 180"]) {
        h.projection = Some(Projection::Dome180);
        h.stereo = Some(Stereo::SideBySide);
    } else if has(&["360", "vr360"]) {
        h.projection = Some(Projection::Sphere360);
    }
    if has(&["_tb", "top-bottom", "top bottom", "over-under", "_ou"]) {
        h.stereo = Some(Stereo::TopBottom);
    } else if has(&["_sbs", "_lr", "side-by-side", "side by side", "3d sbs"]) {
        h.stereo = Some(Stereo::SideBySide);
    } else if h.stereo.is_none() && has(&["3d", "stereo"]) && h.projection == Some(Projection::Sphere360) {
        h.stereo = Some(Stereo::TopBottom);
    }
    h
}
