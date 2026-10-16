//! Video decoding through the system `ffmpeg` binary.
//!
//! ffmpeg decodes (with the Frame's iris hardware decoder via V4L2 mem2mem when
//! the codec allows) and writes raw NV12 frames (the decoder's own layout:
//! full-size luma, then half-size interleaved chroma) to a pipe; the shader
//! turns them into colour. A
//! reader thread paces those frames and writes each
//! one straight into a GPU-visible staging buffer picked by `FrameSlots`.
//! Audio is played by a separate `ffplay` process, and frames are held until
//! ffplay's clock reaches them, so picture follows sound. Seeking restarts
//! both processes at the new position; pausing freezes ffplay.

use std::{
    collections::HashMap,
    io::Read,
    process::{Child, ChildStderr, Command, Stdio},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

pub struct VideoInfo {
    pub codec: String,
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    pub has_audio: bool,
    /// Colour matrix and range for turning YUV into RGB.
    pub bt601: bool,
    pub full_range: bool,
    /// Length in seconds, when the container says.
    pub duration: Option<f64>,
}

fn tool(name: &str) -> String {
    let sys = format!("/usr/bin/{name}");
    if std::path::Path::new(&sys).exists() { sys } else { name.to_string() }
}

pub fn probe(input: &str) -> Result<VideoInfo, String> {
    let out = Command::new(tool("ffprobe"))
        .args(["-v", "error", "-select_streams", "v:0"])
        .args(["-show_entries", "stream=codec_name,width,height,avg_frame_rate,r_frame_rate,color_space,color_range:format=duration"])
        .args(["-of", "default=nw=1", input])
        .output()
        .map_err(|e| format!("could not run ffprobe: {e}"))?;
    if !out.status.success() {
        return Err(format!("ffprobe failed: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let kv: HashMap<&str, &str> = text.lines().filter_map(|l| l.split_once('=')).collect();
    let rate = |key: &str| -> Option<f64> {
        let (n, d) = kv.get(key)?.split_once('/')?;
        let (n, d): (f64, f64) = (n.parse().ok()?, d.parse().ok()?);
        (d > 0.0 && n > 0.0).then(|| n / d)
    };
    let height: u32 = kv.get("height").and_then(|v| v.parse().ok()).ok_or("no video height")?;
    Ok(VideoInfo {
        bt601: match kv.get("color_space").copied() {
            Some("bt470bg" | "smpte170m") => true,
            Some("bt709") => false,
            _ => height < 720,
        },
        full_range: kv.get("color_range") == Some(&"pc"),
        codec: kv.get("codec_name").unwrap_or(&"unknown").to_string(),
        width: kv.get("width").and_then(|v| v.parse().ok()).ok_or("no video width")?,
        height,
        fps: rate("avg_frame_rate").or_else(|| rate("r_frame_rate")).unwrap_or(30.0),
        duration: kv.get("duration").and_then(|v| v.parse().ok()).filter(|d: &f64| *d > 0.0),
        has_audio: Command::new(tool("ffprobe"))
            .args(["-v", "error", "-select_streams", "a", "-show_entries", "stream=index", "-of", "csv=p=0", input])
            .output()
            .map(|o| !o.stdout.is_empty())
            .unwrap_or(false),
    })
}

/// Hardware decoder for a codec, if the Frame's iris block handles it.
pub fn hw_decoder(codec: &str) -> Option<&'static str> {
    match codec {
        "h264" => Some("h264_v4l2m2m"),
        "hevc" => Some("hevc_v4l2m2m"),
        "vp9" => Some("vp9_v4l2m2m"),
        _ => None,
    }
}

/// Output size: the source size, scaled down to fit `max_width`, rounded to even.
pub fn output_size(info: &VideoInfo, max_width: u32) -> (u32, u32) {
    if info.width <= max_width {
        return (info.width & !1, info.height & !1);
    }
    let h = (info.height as u64 * max_width as u64 / info.width as u64) as u32;
    (max_width & !1, h & !1)
}

/// Ring of staging buffers shared between the reader thread and the renderer.
/// The reader fills any slot that is neither the latest published frame nor
/// the one the GPU is copying from.
pub struct FrameSlots {
    ptrs: Vec<usize>,
    pub frame_bytes: usize,
    state: Mutex<SlotState>,
    freed: Condvar,
}

#[derive(Default)]
struct SlotState {
    latest: Option<usize>,
    seq: u64,
    in_use: Option<usize>,
}

impl FrameSlots {
    /// # Safety
    /// Each pointer must address `frame_bytes` of persistently mapped,
    /// host-coherent memory that outlives this struct.
    pub unsafe fn new(ptrs: Vec<*mut u8>, frame_bytes: usize) -> Self {
        assert!(ptrs.len() >= 3);
        Self {
            ptrs: ptrs.into_iter().map(|p| p as usize).collect(),
            frame_bytes,
            state: Mutex::new(SlotState::default()),
            freed: Condvar::new(),
        }
    }

    /// Renderer: take the newest frame if it is newer than `last_seq`.
    pub fn take_latest(&self, last_seq: u64) -> Option<(usize, u64)> {
        let mut s = self.state.lock().unwrap();
        match s.latest {
            Some(i) if s.seq != last_seq => {
                s.in_use = Some(i);
                Some((i, s.seq))
            }
            _ => None,
        }
    }

    /// Renderer: the GPU copy from the slot taken last has finished.
    pub fn release(&self) {
        self.state.lock().unwrap().in_use = None;
        self.freed.notify_all();
    }

    fn free_slot(&self) -> usize {
        let mut s = self.state.lock().unwrap();
        loop {
            if let Some(i) = (0..self.ptrs.len()).find(|&i| Some(i) != s.latest && Some(i) != s.in_use) {
                return i;
            }
            s = self.freed.wait(s).unwrap();
        }
    }

    fn publish(&self, i: usize) {
        let mut s = self.state.lock().unwrap();
        s.latest = Some(i);
        s.seq += 1;
    }

    /// Renderer: read the slot returned by `take_latest` (until `release`).
    pub fn slot(&self, i: usize) -> &[u8] {
        // SAFETY: guaranteed by `new`; the reader never writes the slot in use.
        unsafe { std::slice::from_raw_parts(self.ptrs[i] as *const u8, self.frame_bytes) }
    }

    fn slot_mut(&self, i: usize) -> &mut [u8] {
        // SAFETY: guaranteed by `new`; the slot protocol gives the reader
        // exclusive access to slot `i` until it is published.
        unsafe { std::slice::from_raw_parts_mut(self.ptrs[i] as *mut u8, self.frame_bytes) }
    }
}

/// How to decode one input; worked out once, then used for every seek.
pub struct Decoder {
    audio_input: Option<String>,
    /// Input as given to ffmpeg (web streams go through a read-ahead buffer).
    video_url: String,
    filter: Option<String>,
    fps: f64,
    /// `Some(name)` = hardware decoder, `None` = ffmpeg's software decoder.
    decoder: Option<&'static str>,
}

impl Decoder {
    /// Pick the decoder for `input`, producing NV12 frames of size `out`.
    pub fn new(
        input: &str,
        audio_input: Option<&str>,
        info: &VideoInfo,
        out: (u32, u32),
        want_hw: bool,
    ) -> Result<Self, String> {
        let scale = (out.0, out.1) != (info.width, info.height);
        let mut d = Self {
            video_url: input.to_string(),
            audio_input: audio_input.map(str::to_string),
            filter: scale.then(|| format!("scale={}:{}:flags=bilinear", out.0, out.1)),
            fps: info.fps,
            decoder: None,
        };
        // Try the hardware decoder first, then software; for web streams,
        // first with ffmpeg's read-ahead buffer ("async:") so network hiccups
        // don't stall the picture. Each candidate is test-run for one frame
        // with exactly the arguments playback uses, so a failure here is
        // logged with ffmpeg's own message.
        let web = input.starts_with("http://") || input.starts_with("https://");
        let mut candidates: Vec<(Option<&'static str>, bool)> = Vec::new();
        for read_ahead in if web { &[true, false][..] } else { &[false][..] } {
            if want_hw && let Some(hw) = hw_decoder(&info.codec) {
                candidates.push((Some(hw), *read_ahead));
            }
            candidates.push((None, *read_ahead));
        }
        for (candidate, read_ahead) in candidates {
            d.decoder = candidate;
            d.video_url = if read_ahead { format!("async:{input}") } else { input.to_string() };
            let check = Command::new(tool("ffmpeg"))
                .args(["-loglevel", "warning"])
                .args(d.decode_args(0.0))
                .args(["-frames:v", "1", "-y", "/dev/null"])
                .stdin(Stdio::null())
                .output()
                .map_err(|e| format!("could not run ffmpeg: {e}"))?;
            if check.status.success() {
                return Ok(d);
            }
            eprintln!(
                "video: decoder {}{} failed its test. ffmpeg said:",
                candidate.unwrap_or("software"),
                if read_ahead { " with read-ahead" } else { "" }
            );
            for line in String::from_utf8_lossy(&check.stderr).lines().take(20) {
                eprintln!("    {line}");
            }
        }
        Err("ffmpeg could not decode this video (see messages above)".into())
    }

    pub fn is_hw(&self) -> bool {
        self.decoder.is_some()
    }

    fn decode_args(&self, start_secs: f64) -> Vec<String> {
        let mut a: Vec<String> = vec!["-hide_banner".into(), "-nostdin".into()];
        if start_secs > 0.0 {
            a.extend(["-ss".into(), format!("{start_secs:.3}")]);
        }
        if let Some(d) = self.decoder {
            a.extend(["-c:v".into(), d.into()]);
        }
        a.extend(["-i".into(), self.video_url.clone(), "-an".into(), "-sn".into()]);
        if let Some(f) = &self.filter {
            a.extend(["-vf".into(), f.clone()]);
        }
        a.extend(["-pix_fmt".into(), "nv12".into(), "-f".into(), "rawvideo".into()]);
        a
    }

    /// Start playing at `start_secs`, writing frames into `slots`.
    pub fn start(&self, start_secs: f64, slots: Arc<FrameSlots>, paused: bool) -> Result<Playback, String> {
        let mut cmd = Command::new(tool("ffmpeg"));
        cmd.args(["-loglevel", "warning"])
            .args(self.decode_args(start_secs))
            .arg("pipe:1")
            .stdout(Stdio::piped())
            .stdin(Stdio::null());
        eprintln!("video: {:?}", cmd);
        let mut video = cmd.spawn().map_err(|e| format!("could not run ffmpeg: {e}"))?;
        let mut pipe = video.stdout.take().unwrap();

        let mut clock = None;
        let audio = if let Some(audio_input) = &self.audio_input {
            let mut a = Command::new(tool("ffplay"));
            // -loglevel error still prints the status line we take the clock from.
            a.args(["-nodisp", "-autoexit", "-loglevel", "error"]);
            if start_secs > 0.0 {
                a.args(["-ss", &format!("{start_secs:.3}")]);
            }
            a.arg(audio_input).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped());
            match a.spawn() {
                Ok(mut c) => {
                    let audio_clock = Arc::new(AudioClock::default());
                    audio_clock.clone().watch(c.stderr.take().unwrap());
                    clock = Some(audio_clock);
                    Some(c)
                }
                Err(e) => {
                    eprintln!("audio: ffplay unavailable ({e}); playing without sound");
                    None
                }
            }
        } else {
            None
        };

        let shared = Arc::new(Shared::default());
        shared.paused.store(paused, Ordering::Relaxed);
        let s = shared.clone();
        let fps = self.fps.clamp(1.0, 240.0);
        let reader = thread::spawn(move || {
            let started = Instant::now();
            // Frame clock (instant, video time) for when there is no sound to follow.
            let mut anchor: Option<(Instant, f64)> = None;
            let mut n: u32 = 0;
            // Time spent waiting for ffmpeg, to tell decoding limits from pacing.
            let mut reading = Duration::ZERO;
            while !s.stop.load(Ordering::Relaxed) {
                let i = slots.free_slot();
                let read_start = Instant::now();
                let read = pipe.read_exact(slots.slot_mut(i));
                reading += read_start.elapsed();
                if n > 0 && n % 300 == 0 {
                    eprintln!(
                        "video: decoder delivers up to {:.1} fps (needs {fps:.1})",
                        300.0 / reading.as_secs_f64().max(1e-3)
                    );
                    reading = Duration::ZERO;
                }
                if let Err(e) = read {
                    if !s.stop.load(Ordering::Relaxed) {
                        eprintln!("video: decoder stream ended after {n} frames ({e})");
                        s.ended.store(true, Ordering::Relaxed);
                    }
                    break;
                }
                if n == 0 {
                    eprintln!("video: first frame decoded after {:.2}s", started.elapsed().as_secs_f64());
                }
                // Hold the frame until the sound reaches it. Without sound (or
                // before it starts, for a while) follow the frame rate instead.
                let pts = start_secs + n as f64 / fps;
                while !s.stop.load(Ordering::Relaxed) && !s.paused.load(Ordering::Relaxed) {
                    let heard = clock.as_ref().filter(|c| !c.gone()).map(|c| c.now());
                    let wait = match heard {
                        Some(Some(t)) => {
                            anchor = None;
                            if t - pts > 0.5 && n % 60 == 0 {
                                eprintln!("video: {:.0} ms behind the sound", (t - pts) * 1e3);
                            }
                            pts - t
                        }
                        Some(None) if started.elapsed() < Duration::from_secs(8) => 0.005,
                        _ => {
                            let (at, from) = *anchor.get_or_insert((Instant::now(), pts));
                            pts - from - at.elapsed().as_secs_f64()
                        }
                    };
                    if wait <= 0.002 {
                        break;
                    }
                    thread::sleep(Duration::from_secs_f64(wait.min(0.01)));
                }
                slots.publish(i);
                n += 1;
                s.frames.store(n as u64, Ordering::Relaxed);
                // While paused, hold the next frame back; the frame clock restarts after.
                if s.paused.load(Ordering::Relaxed) {
                    while s.paused.load(Ordering::Relaxed) && !s.stop.load(Ordering::Relaxed) {
                        thread::sleep(Duration::from_millis(10));
                    }
                    anchor = None;
                }
            }
        });

        let p = Playback { video, audio, shared, reader: Some(reader), start_secs, fps: self.fps };
        if paused {
            p.signal_audio("-STOP");
        }
        Ok(p)
    }
}

/// ffplay's audio clock, read from the status line it prints every 30 ms
/// ("  12.34 M-A: ..."): the video time of the sound being heard.
#[derive(Default)]
struct AudioClock {
    state: Mutex<ClockState>,
}

#[derive(Default)]
struct ClockState {
    value: Option<f64>,
    changed_at: Option<Instant>,
    gone: bool,
}

impl AudioClock {
    /// Current sound position, smoothed between ffplay's 30 ms updates.
    fn now(&self) -> Option<f64> {
        let s = self.state.lock().unwrap();
        Some(s.value? + s.changed_at?.elapsed().as_secs_f64().min(0.05))
    }

    /// ffplay has exited (end of the sound, or an error).
    fn gone(&self) -> bool {
        self.state.lock().unwrap().gone
    }

    fn watch(self: Arc<Self>, mut stderr: ChildStderr) {
        thread::spawn(move || {
            let mut buf = [0u8; 4096];
            let mut line = Vec::new();
            while let Ok(len) = stderr.read(&mut buf) {
                if len == 0 {
                    break;
                }
                for &b in &buf[..len] {
                    if b != b'\r' && b != b'\n' {
                        line.push(b);
                        continue;
                    }
                    let text = String::from_utf8_lossy(&line).into_owned();
                    line.clear();
                    if text.contains("M-A:") {
                        let value = text.split_whitespace().next().and_then(|v| v.parse::<f64>().ok());
                        if let Some(v) = value.filter(|v| v.is_finite()) {
                            let mut s = self.state.lock().unwrap();
                            if s.value != Some(v) {
                                s.value = Some(v);
                                s.changed_at = Some(Instant::now());
                            }
                        }
                    } else if !text.trim().is_empty() {
                        eprintln!("audio: {}", text.trim());
                    }
                }
            }
            self.state.lock().unwrap().gone = true;
        });
    }
}

#[derive(Default)]
struct Shared {
    stop: AtomicBool,
    paused: AtomicBool,
    ended: AtomicBool,
    frames: AtomicU64,
}

pub struct Playback {
    video: Child,
    audio: Option<Child>,
    shared: Arc<Shared>,
    reader: Option<thread::JoinHandle<()>>,
    start_secs: f64,
    fps: f64,
}

impl Playback {
    /// Seconds into the video of the frame on screen.
    pub fn position(&self) -> f64 {
        let n = self.shared.frames.load(Ordering::Relaxed).saturating_sub(1);
        self.start_secs + n as f64 / self.fps.max(1.0)
    }

    pub fn ended(&self) -> bool {
        self.shared.ended.load(Ordering::Relaxed)
    }

    pub fn paused(&self) -> bool {
        self.shared.paused.load(Ordering::Relaxed)
    }

    pub fn set_paused(&mut self, paused: bool) {
        if self.paused() != paused {
            self.shared.paused.store(paused, Ordering::Relaxed);
            self.signal_audio(if paused { "-STOP" } else { "-CONT" });
        }
    }

    /// Freeze or continue the audio player (ffplay has no pause command on a pipe).
    fn signal_audio(&self, signal: &str) {
        if let Some(a) = &self.audio {
            let _ = Command::new("kill").arg(signal).arg(a.id().to_string()).status();
        }
    }

    /// Stop decoding and sound; safe to call more than once.
    pub fn stop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        let _ = self.video.kill();
        let _ = self.video.wait();
        if let Some(mut a) = self.audio.take() {
            let _ = Command::new("kill").arg("-CONT").arg(a.id().to_string()).status();
            let _ = a.kill();
            let _ = a.wait();
        }
        if let Some(r) = self.reader.take() {
            let _ = r.join();
        }
    }
}

impl Drop for Playback {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Whether a decoded 360 frame looks like YouTube's equi-angular cubemap
/// (EAC: six cube faces in a 3x2 grid) rather than equirectangular. In EAC
/// the two rows of faces meet at the middle of the picture with a hard
/// edge, while an equirectangular picture is continuous at its horizon.
/// Works on the luma plane (`w` x `h` bytes). `None` when the frame has too
/// little detail to tell (black, fades).
pub fn looks_like_eac(luma: &[u8], w: usize, h: usize) -> Option<bool> {
    if w < 64 || h < 64 || luma.len() < w * h {
        return None;
    }
    let px = |x: usize, y: usize| luma[y * w + x];
    let row_diff = |y0: usize, y1: usize| {
        (0..w).step_by(2).map(|x| px(x, y0).abs_diff(px(x, y1)) as f64).sum::<f64>() / w.div_ceil(2) as f64
    };

    // Detail: average difference between nearby rows across the frame.
    let detail = (1..16).map(|k| row_diff(h * k / 16, h * k / 16 + 2)).sum::<f64>() / 15.0;
    if detail < 1.0 {
        return None;
    }
    let mid = h / 2;
    let seam = row_diff(mid - 1, mid);
    let normal = (row_diff(mid - 8, mid - 7) + row_diff(mid - 7, mid - 6) + row_diff(mid + 6, mid + 7) + row_diff(mid + 7, mid + 8)) / 4.0;
    // Top-bottom 3D also has an edge in the middle, but its halves match.
    let halves = (0..32)
        .flat_map(|j| (0..32).map(move |i| (i, j)))
        .map(|(i, j)| {
            let (x, y) = (w * (2 * i + 1) / 64, mid * (2 * j + 1) / 64);
            px(x, y).abs_diff(px(x, y + mid)) as f64
        })
        .sum::<f64>()
        / 1024.0;
    eprintln!("video: layout check: seam {seam:.1}, normal {normal:.1}, halves differ by {halves:.1}");
    Some(seam > 2.5 * normal + 3.0 && halves > 12.0)
}
