//! `vr-player --check`: tries sources the way playback would (find the
//! streams, pick a decoder, decode from the start and from the middle, open
//! the sound) without the headset, and reports what will and won't play
//! smoothly.

use std::time::Instant;

use crate::{Projection, Stereo, source, video};

/// Tried when no sources are given: one of each kind of site and format.
const DEFAULT_SOURCES: &[(&str, &str)] = &[
    ("YouTube 360 (cubemap, VP9 4K)", "https://www.youtube.com/watch?v=tR8ZtyhSDYw"),
    ("YouTube flat 4K 60 fps", "https://www.youtube.com/watch?v=LXb3EKWsInQ"),
    ("Internet Archive page", "https://archive.org/details/BigBuckBunny_124"),
    ("Direct MP4 (H.264 1080p)", "https://test-videos.co.uk/vids/bigbuckbunny/mp4/h264/1080/Big_Buck_Bunny_1080_10s_30MB.mp4"),
    ("HLS stream (.m3u8)", "https://test-streams.mux.dev/x36xhzz/x36xhzz.m3u8"),
    ("DASH stream (.mpd)", "https://dash.akamaized.net/akamai/bbb_30fps/bbb_30fps.mpd"),
];

/// Seconds of video downloaded to test the network apart from the decoder.
const CLIP_SECS: f64 = 10.0;

/// Local samples the installer downloads, tried too when present.
const LOCAL_SAMPLES: &[(&str, &str)] =
    &[("Local 360 file", "Videos/vr-samples/360-mono.mp4"), ("Local 360 3D file", "Videos/vr-samples/360-stereo-tb.mp4")];

struct Outcome {
    name: String,
    verdict: String,
    ok: bool,
}

/// Check each source; true when all of them should play smoothly.
pub fn run(given: &[String], max_width: u32, hw: bool) -> bool {
    let mut sources: Vec<(String, String)> = given.iter().map(|s| (s.clone(), s.clone())).collect();
    if sources.is_empty() {
        sources.extend(DEFAULT_SOURCES.iter().map(|(n, u)| (n.to_string(), u.to_string())));
        if let Some(home) = std::env::var_os("HOME") {
            for (name, rel) in LOCAL_SAMPLES {
                let path = std::path::Path::new(&home).join(rel);
                if path.exists() {
                    sources.push((name.to_string(), path.display().to_string()));
                }
            }
        }
    }
    eprintln!("check: testing {} sources; this takes a minute or two. No headset needed.", sources.len());
    let mut results = Vec::new();
    for (name, input) in &sources {
        eprintln!("\n=== {name}\n    {input}");
        let (verdict, ok) = match check_one(input, max_width, hw) {
            Ok(problems) if problems.is_empty() => ("OK".to_string(), true),
            Ok(problems) => (problems.join("; "), false),
            Err(e) => (format!("FAILED: {e}"), false),
        };
        eprintln!("--> {verdict}");
        results.push(Outcome { name: name.clone(), verdict, ok });
    }
    eprintln!("\n=== Summary");
    for r in &results {
        eprintln!("{} {:32} {}", if r.ok { "ok  " } else { "BAD " }, r.name, r.verdict);
    }
    eprintln!("\nThe full report is in ~/vr-player.log; paste it into the project thread.");
    results.iter().all(|r| r.ok)
}

/// Problems found playing `input` (empty when it should play smoothly).
fn check_one(input: &str, max_width: u32, hw: bool) -> Result<Vec<String>, String> {
    let mut problems = Vec::new();
    let t = Instant::now();
    eprintln!("    finding the streams...");
    let src = source::resolve(input, max_width)?;
    eprintln!("    found streams in {:.1}s; title \"{}\"", t.elapsed().as_secs_f64(), src.title);

    eprintln!("    reading the stream details...");
    let info = video::probe(&src.video)?;
    let (width, height) = video::output_size(&info, max_width);
    let duration = info.duration;
    eprintln!(
        "    video: {} {}x{} at {:.2} fps, {}, {}{}",
        info.codec,
        info.width,
        info.height,
        info.fps,
        duration.map_or("live or unknown length".to_string(), |d| format!("{d:.0}s long")),
        if info.bt601 { "BT.601" } else { "BT.709" },
        if info.full_range { " full range" } else { "" },
    );
    if (width, height) != (info.width, info.height) {
        eprintln!("    scaled to {width}x{height} (wider than --max-width {max_width})");
    }
    if info.fps > 61.0 || info.fps < 14.0 {
        problems.push(format!("unusual frame rate {:.1} fps (timing may be off)", info.fps));
    }

    let meta = source::probe_hints(&src.video);
    let named = source::guess_from_name(&src.title);
    let projection = meta.projection.or(named.projection).unwrap_or(Projection::Flat);
    let stereo = meta.stereo.or(named.stereo).unwrap_or(Stereo::Mono);
    eprintln!(
        "    layout: {} {} (from {})",
        projection.label(),
        stereo.label(),
        if meta.projection.is_some() || meta.stereo.is_some() {
            "stream metadata"
        } else if named.projection.is_some() || named.stereo.is_some() {
            "the title"
        } else {
            "nothing; flat screen by default"
        }
    );

    let audio = src.audio.clone().or_else(|| info.has_audio.then(|| src.video.clone()));
    eprintln!("    testing decoders...");
    let decoder = video::Decoder::new(&src.video, audio.as_deref(), &info, (width, height), hw)?;
    let hw_used = decoder.is_hw();
    eprintln!("    decoder: {}", if hw_used { "hardware" } else { "software" });
    if !hw_used && video::hw_decoder(&info.codec).is_none() {
        eprintln!("    note: the Frame has no hardware decoder for {}", info.codec);
    }

    // Playback needs the decoder comfortably faster than the video.
    let frame_bytes = width as usize * height as usize * 3 / 2;
    let need = info.fps.max(1.0);
    let web = input.starts_with("http://") || input.starts_with("https://");
    // The same decoder reading the stream the other way (with or without
    // the read-ahead buffer), to see which one keeps up.
    let other_way = (web && decoder.read_ahead()).then(|| decoder.with_input(&src.video, decoder.hw_name()));
    let rate = |label: &str, start: f64, problems: &mut Vec<String>| match {
        eprintln!("    decoding {label}...");
        decoder.benchmark(start, frame_bytes, 150, 10.0)
    } {
        Ok(b) => {
            eprintln!(
                "    decode {label}: first frame after {:.2}s, then {:.0} fps ({} frames; needs {need:.0})",
                b.first_frame, b.fps, b.frames
            );
            if b.frames > 10 && b.fps < need * 1.2 {
                problems.push(format!(
                    "frames {label} came too slowly: {:.0} fps for {need:.0} fps video ({} decoder, or the network), will stutter",
                    b.fps,
                    if hw_used { "hardware" } else { "software" }
                ));
            }
            if b.first_frame > 3.0 {
                problems.push(format!("{label}: first picture took {:.1}s", b.first_frame));
            }
        }
        Err(e) => problems.push(format!("decoding {label} failed: {e}")),
    };
    let rate = |label: &str, start: f64, problems: &mut Vec<String>| {
        rate(label, start, problems);
        if let Some(d) = &other_way {
            match d.benchmark(start, frame_bytes, 150, 10.0) {
                Ok(b) => eprintln!(
                    "    same {label} without read-ahead: first frame after {:.2}s, then {:.0} fps ({} frames)",
                    b.first_frame, b.fps, b.frames
                ),
                Err(e) => eprintln!("    same {label} without read-ahead: failed ({e})"),
            }
        }
    };
    rate("from the start", 0.0, &mut problems);
    let seek_to = duration.filter(|d| *d > 30.0).map(|d| (d * 0.5).floor());
    if let Some(t) = seek_to {
        rate(&format!("after seeking to {t:.0}s"), t, &mut problems);
    } else {
        eprintln!("    seek: skipped (live or shorter than 30s)");
    }

    // Playback mixes two things: the network delivering the stream, and the
    // decoder turning it into pictures. Download a short clip without
    // decoding (network speed), then decode that clip with each decoder
    // (decoder speed with the network out of the way).
    if web {
        let lower = src.video.to_ascii_lowercase();
        if ![".m3u8", ".mpd", "manifest"].iter().any(|p| lower.contains(p)) {
            connections(&src.video);
        }
        let clip = std::env::temp_dir().join(format!("vr-player-check-{}.mkv", std::process::id()));
        let clip = clip.display().to_string();
        let mut positions = vec![("from the start".to_string(), 0.0)];
        positions.extend(seek_to.map(|t| (format!("at {t:.0}s"), t)));
        for (label, at) in positions {
            eprintln!("    downloading {CLIP_SECS:.0}s of video {label} without decoding...");
            match video::fetch_clip(&src.video, at, CLIP_SECS, &clip) {
                Ok(took) => {
                    let speed = CLIP_SECS / took.max(1e-3);
                    let mbit = std::fs::metadata(&clip).map_or(0.0, |m| m.len() as f64 * 8.0 / 1e6);
                    eprintln!(
                        "    network {label}: {CLIP_SECS:.0}s of video ({:.0} Mbit/s to play) arrived in {took:.1}s ({speed:.1}x playback speed, {:.0} Mbit/s)",
                        mbit / CLIP_SECS,
                        mbit / took.max(1e-3)
                    );
                    if speed < 1.3 {
                        problems.push(format!(
                            "network {label}: {CLIP_SECS:.0}s of video took {took:.1}s to arrive, too slow to play smoothly"
                        ));
                    }
                }
                Err(e) => {
                    problems.push(format!("network {label}: download failed: {e}"));
                    continue;
                }
            }
            let mut decoders = vec![(None, "software")];
            if hw {
                if let Some(h) = video::hw_decoder(&info.codec) {
                    decoders.insert(0, (Some(h), "hardware"));
                }
            }
            for (d, name) in decoders {
                match decoder.with_input(&clip, d).benchmark(0.0, frame_bytes, 150, 10.0) {
                    Ok(b) => {
                        eprintln!(
                            "    {name} decoder on that clip: first frame after {:.2}s, then {:.0} fps ({} frames; needs {need:.0})",
                            b.first_frame, b.fps, b.frames
                        );
                        if d.is_some() == hw_used && b.frames > 10 && b.fps < need * 1.2 {
                            problems.push(format!("{name} decoder too slow {label}: {:.0} fps for {need:.0} fps video", b.fps));
                        }
                    }
                    Err(e) => {
                        eprintln!("    {name} decoder on that clip: failed ({e})");
                        if d.is_some() == hw_used {
                            problems.push(format!("{name} decoder failed on the clip {label}: {e}"));
                        }
                    }
                }
            }
        }
        let _ = std::fs::remove_file(&clip);
    }

    match &audio {
        None => eprintln!("    sound: none"),
        Some(a) => {
            let at = seek_to.unwrap_or(0.0);
            eprintln!("    starting the sound at {at:.0}s...");
            match video::audio_check(a, at, decoder.audio_start()) {
                Ok((took, from)) => {
                    eprintln!("    sound: plays from {from:.2}s after {took:.2}s");
                    if took > 3.0 {
                        problems.push(format!("sound took {took:.1}s to start"));
                    }
                    if (from - at).abs() > 0.15 {
                        problems.push(format!("sound starts at {from:.2}s instead of {at:.0}s, out of step with the picture"));
                    }
                }
                Err(e) => problems.push(format!("sound failed: {e}")),
            }
        }
    }
    Ok(problems)
}

/// Download speed of `url` over one connection and over four at once: much
/// faster with four means the site limits each connection, not the network.
fn connections(url: &str) {
    const CHUNK: u64 = 4 << 20;
    let fetch = |from: u64, len: u64| {
        std::process::Command::new("curl")
            .args(["-s", "-o", "/dev/null", "-m", "20", "-w", "%{size_download}"])
            .args(["-r", &format!("{from}-{}", from + len - 1), url])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
    };
    let bytes = |c: std::io::Result<std::process::Child>| -> f64 {
        c.and_then(|c| c.wait_with_output())
            .ok()
            .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse().ok())
            .unwrap_or(0.0)
    };
    eprintln!("    timing downloads over one and over four connections...");
    let t = Instant::now();
    let one = bytes(fetch(0, 4 * CHUNK));
    let one = one * 8.0 / 1e6 / t.elapsed().as_secs_f64();
    let t = Instant::now();
    let children: Vec<_> = (0..4).map(|i| fetch((4 + i) * CHUNK, CHUNK)).collect();
    let four: f64 = children.into_iter().map(bytes).sum();
    let four = four * 8.0 / 1e6 / t.elapsed().as_secs_f64();
    eprintln!("    downloads: one connection {one:.0} Mbit/s, four connections {four:.0} Mbit/s");
}
