//! Video decoding through the system `ffmpeg` binary.
//!
//! ffmpeg decodes (with the Frame's iris hardware decoder via V4L2 mem2mem when
//! the codec allows) and writes raw NV12 frames (the decoder's own layout:
//! full-size luma, then half-size interleaved chroma) to a pipe; the shader
//! turns them into colour. A
//! reader thread writes each one straight into a GPU-visible staging buffer
//! from `FrameSlots`, decoding a few frames ahead; the renderer shows each
//! when the playback clock reaches it.
//! Audio is played by a separate `ffplay` process, and the playback clock
//! follows ffplay's, so picture follows sound. Seeking restarts
//! both processes at the new position; pausing freezes ffplay.

use std::{
    collections::{HashMap, VecDeque},
    io::Read,
    process::{Child, ChildStderr, Command, Stdio},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
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

/// Staging buffers shared between the reader thread and the renderer. The
/// reader decodes ahead into free slots and queues them with their time; the
/// renderer takes, each display frame, the newest frame whose time has come,
/// so frames change on a steady beat whatever the decoder's timing.
pub struct FrameSlots {
    ptrs: Vec<usize>,
    pub frame_bytes: usize,
    state: Mutex<SlotState>,
    freed: Condvar,
}

#[derive(Default)]
struct SlotState {
    free: Vec<usize>,
    /// Decoded frames in order: (slot, video time in seconds).
    ready: VecDeque<(usize, f64)>,
    /// Frames dropped because their time passed before they were shown.
    skipped: u64,
}

impl FrameSlots {
    /// # Safety
    /// Each pointer must address `frame_bytes` of persistently mapped,
    /// host-coherent memory that outlives this struct.
    pub unsafe fn new(ptrs: Vec<*mut u8>, frame_bytes: usize) -> Self {
        assert!(ptrs.len() >= 2);
        let state = SlotState { free: (0..ptrs.len()).collect(), ..Default::default() };
        Self {
            ptrs: ptrs.into_iter().map(|p| p as usize).collect(),
            frame_bytes,
            state: Mutex::new(state),
            freed: Condvar::new(),
        }
    }

    /// Renderer: the newest queued frame due at `time` (dropping older ones),
    /// or with `first`, the next frame whatever its time.
    pub fn take_due(&self, time: f64, first: bool) -> Option<(usize, f64)> {
        let mut s = self.state.lock().unwrap();
        let mut chosen: Option<(usize, f64)> = None;
        while let Some(&(i, pts)) = s.ready.front() {
            if pts > time && !(first && chosen.is_none()) {
                break;
            }
            s.ready.pop_front();
            if let Some((old, _)) = chosen.replace((i, pts)) {
                s.free.push(old);
                s.skipped += 1;
            }
        }
        drop(s);
        self.freed.notify_all();
        chosen
    }

    /// Renderer: done with a slot from `take_due` (its GPU copy has finished).
    pub fn release(&self, i: usize) {
        self.state.lock().unwrap().free.push(i);
        self.freed.notify_all();
    }

    /// Frames dropped so far because they came too late.
    pub fn skipped(&self) -> u64 {
        self.state.lock().unwrap().skipped
    }

    /// No decoded frames waiting.
    pub fn is_empty(&self) -> bool {
        self.state.lock().unwrap().ready.is_empty()
    }

    /// Drop all queued frames (after the reader has stopped, before a seek).
    fn clear(&self) {
        let mut s = self.state.lock().unwrap();
        while let Some((i, _)) = s.ready.pop_front() {
            s.free.push(i);
        }
    }

    /// Reader: wait for a free slot; `None` once `stop` is set.
    fn free_slot(&self, stop: &AtomicBool) -> Option<usize> {
        let mut s = self.state.lock().unwrap();
        loop {
            if stop.load(Ordering::Relaxed) {
                return None;
            }
            if let Some(i) = s.free.pop() {
                return Some(i);
            }
            s = self.freed.wait_timeout(s, Duration::from_millis(50)).unwrap().0;
        }
    }

    fn queue(&self, i: usize, pts: f64) {
        self.state.lock().unwrap().ready.push_back((i, pts));
    }

    /// Renderer: read a slot returned by `take_due` (until `release`).
    pub fn slot(&self, i: usize) -> &[u8] {
        // SAFETY: guaranteed by `new`; the reader never writes a queued or taken slot.
        unsafe { std::slice::from_raw_parts(self.ptrs[i] as *const u8, self.frame_bytes) }
    }

    fn slot_mut(&self, i: usize) -> &mut [u8] {
        // SAFETY: guaranteed by `new`; the slot protocol gives the reader
        // exclusive access to a free slot until it is queued.
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
            // -stats makes ffplay print its status line (which we take the
            // clock from) even at -loglevel error; ffmpeg 7 hides it otherwise.
            a.args(["-nodisp", "-autoexit", "-loglevel", "error", "-stats"]);
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
        let s = shared.clone();
        let fps = self.fps.clamp(1.0, 240.0);
        let reader_slots = slots.clone();
        let reader = thread::spawn(move || {
            let slots = reader_slots;
            let started = Instant::now();
            let mut n: u32 = 0;
            // Time spent waiting for ffmpeg, to tell decoding limits from pacing.
            let mut reading = Duration::ZERO;
            while let Some(i) = slots.free_slot(&s.stop) {
                let read_start = Instant::now();
                let read = pipe.read_exact(slots.slot_mut(i));
                reading += read_start.elapsed();
                if let Err(e) = read {
                    slots.release(i);
                    if !s.stop.load(Ordering::Relaxed) {
                        eprintln!("video: decoder stream ended after {n} frames ({e})");
                        s.ended.store(true, Ordering::Relaxed);
                    }
                    break;
                }
                if n == 0 {
                    eprintln!("video: first frame decoded after {:.2}s", started.elapsed().as_secs_f64());
                }
                slots.queue(i, start_secs + n as f64 / fps);
                n += 1;
                if n % 300 == 0 {
                    eprintln!(
                        "video: decoder delivers up to {:.1} fps (needs {fps:.1})",
                        300.0 / reading.as_secs_f64().max(1e-3)
                    );
                    reading = Duration::ZERO;
                }
            }
        });

        let p = Playback {
            video,
            audio,
            clock,
            shared,
            slots,
            reader: Some(reader),
            started: Instant::now(),
            anchor: None,
            shown: None,
            paused_at: paused.then_some(start_secs),
            start_secs,
        };
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
        Some(s.value? + s.changed_at?.elapsed().as_secs_f64().min(0.2))
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
                    // Status lines end in "fd= ... aq= ..."; before the sound
                    // starts they read "nan" and carry no clock yet.
                    if text.contains("fd=") {
                        let value = text.split_whitespace().next().and_then(|v| v.parse::<f64>().ok());
                        if let Some(v) = value.filter(|v| v.is_finite()) {
                            let mut s = self.state.lock().unwrap();
                            if s.value.is_none() {
                                eprintln!("audio: playing from {v:.2}s");
                            }
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
    ended: AtomicBool,
}

pub struct Playback {
    video: Child,
    audio: Option<Child>,
    clock: Option<Arc<AudioClock>>,
    shared: Arc<Shared>,
    slots: Arc<FrameSlots>,
    reader: Option<thread::JoinHandle<()>>,
    started: Instant,
    /// Playback clock: video time `.1` at instant `.0`, kept close to the sound.
    anchor: Option<(Instant, f64)>,
    /// Time of the frame on screen.
    shown: Option<f64>,
    /// Video time while paused.
    paused_at: Option<f64>,
    start_secs: f64,
}

impl Playback {
    /// Video time the picture should show now: follows the sound when there
    /// is any, else the wall clock from the first frame. `None` before
    /// playback has started (the first frame is still shown at once).
    pub fn clock(&mut self) -> Option<f64> {
        if let Some(t) = self.paused_at {
            return Some(t);
        }
        let heard = self.clock.as_ref().filter(|c| !c.gone()).and_then(|c| c.now());
        if let Some((at, t)) = self.anchor.as_mut() {
            if let Some(h) = heard {
                // Steer gently towards the sound (at most 1 ms per frame, which
                // can't be seen); jump only when far off.
                let err = h - (*t + at.elapsed().as_secs_f64());
                if err.abs() > 0.5 {
                    eprintln!("video: clock re-synced to the sound ({:+.0} ms)", err * 1e3);
                    *at = Instant::now();
                    *t = h;
                } else {
                    *t += (err * 0.02).clamp(-0.001, 0.001);
                }
            }
        } else if let Some(h) = heard {
            self.anchor = Some((Instant::now(), h));
        } else if let Some(t) = self.shown
            && (self.clock.is_none() || self.started.elapsed() > Duration::from_secs(3))
        {
            if self.clock.is_some() {
                eprintln!("audio: no clock from ffplay; following the frame rate");
            }
            self.anchor = Some((Instant::now(), t));
        }
        self.anchor.map(|(at, t)| t + at.elapsed().as_secs_f64())
    }

    /// Renderer: a frame with time `t` is now on screen.
    pub fn showing(&mut self, t: f64) {
        self.shown = Some(t);
    }

    /// Seconds into the video of the frame on screen.
    pub fn position(&self) -> f64 {
        self.shown.unwrap_or(self.start_secs)
    }

    /// No frame of this playback shown yet.
    pub fn fresh(&self) -> bool {
        self.shown.is_none()
    }

    pub fn ended(&self) -> bool {
        self.shared.ended.load(Ordering::Relaxed) && self.slots.is_empty()
    }

    pub fn paused(&self) -> bool {
        self.paused_at.is_some()
    }

    pub fn set_paused(&mut self, paused: bool) {
        if self.paused() == paused {
            return;
        }
        if paused {
            self.paused_at = Some(self.clock().unwrap_or(self.position()));
        } else {
            // Restart the clock where it stopped; the sound picks up there too.
            self.anchor = self.paused_at.take().map(|t| (Instant::now(), t));
        }
        self.signal_audio(if paused { "-STOP" } else { "-CONT" });
    }

    /// Freeze or continue the audio player (ffplay has no pause command on a pipe).
    fn signal_audio(&self, signal: &str) {
        if let Some(a) = &self.audio {
            let _ = Command::new("kill").arg(signal).arg(a.id().to_string()).status();
        }
    }

    /// Stop decoding and sound, and drop queued frames; safe to call more than once.
    pub fn stop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        let _ = self.video.kill();
        let _ = self.video.wait();
        if let Some(mut a) = self.audio.take() {
            let _ = Command::new("kill").arg("-CONT").arg(a.id().to_string()).status();
            let _ = a.kill();
            let _ = a.wait();
        }
        // Only the first call clears: by a second one (when a replaced
        // playback is dropped) the slots may hold the new playback's frames.
        if let Some(r) = self.reader.take() {
            let _ = r.join();
            self.slots.clear();
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
