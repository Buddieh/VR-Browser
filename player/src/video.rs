//! Video decoding through the system `ffmpeg` binary.
//!
//! ffmpeg decodes (with the Frame's iris hardware decoder via V4L2 mem2mem when
//! the codec allows), converts to RGBA and writes raw frames to a pipe. A
//! reader thread paces those frames to the video's frame rate and writes each
//! one straight into a GPU-visible staging buffer picked by `FrameSlots`.
//! Audio is played by a separate `ffplay` process.

use std::{
    collections::HashMap,
    io::Read,
    process::{Child, Command, Stdio},
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
}

fn tool(name: &str) -> String {
    let sys = format!("/usr/bin/{name}");
    if std::path::Path::new(&sys).exists() { sys } else { name.to_string() }
}

pub fn probe(input: &str) -> Result<VideoInfo, String> {
    let out = Command::new(tool("ffprobe"))
        .args(["-v", "error", "-select_streams", "v:0"])
        .args(["-show_entries", "stream=codec_name,width,height,avg_frame_rate,r_frame_rate"])
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
    Ok(VideoInfo {
        codec: kv.get("codec_name").unwrap_or(&"unknown").to_string(),
        width: kv.get("width").and_then(|v| v.parse().ok()).ok_or("no video width")?,
        height: kv.get("height").and_then(|v| v.parse().ok()).ok_or("no video height")?,
        fps: rate("avg_frame_rate").or_else(|| rate("r_frame_rate")).unwrap_or(30.0),
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

    fn slot_mut(&self, i: usize) -> &mut [u8] {
        // SAFETY: guaranteed by `new`; the slot protocol gives the reader
        // exclusive access to slot `i` until it is published.
        unsafe { std::slice::from_raw_parts_mut(self.ptrs[i] as *mut u8, self.frame_bytes) }
    }
}

pub struct Playback {
    video: Child,
    audio: Option<Child>,
    stop: Arc<AtomicBool>,
    pub hw: bool,
}

impl Playback {
    /// Start decoding `input` into `slots`. `pix_fmt` is ffmpeg's name for the
    /// swapchain's byte order ("rgba" or "bgra").
    pub fn start(
        input: &str,
        info: &VideoInfo,
        out: (u32, u32),
        pix_fmt: &str,
        want_hw: bool,
        start_secs: f64,
        slots: Arc<FrameSlots>,
    ) -> Result<Self, String> {
        let filter = format!("scale={}:{}:flags=bilinear,format={pix_fmt}", out.0, out.1);
        let decode_args = |decoder: Option<&str>| {
            let mut a: Vec<String> = vec!["-hide_banner".into(), "-nostdin".into()];
            if start_secs > 0.0 {
                a.extend(["-ss".into(), format!("{start_secs:.3}")]);
            }
            if let Some(d) = decoder {
                a.extend(["-c:v".into(), d.into()]);
            }
            a.extend(["-i".into(), input.into(), "-an".into(), "-sn".into(), "-vf".into(), filter.clone()]);
            a
        };

        // The hardware decoder refuses some streams; check it can produce one
        // frame before committing to it, and fall back to software if not.
        let mut decoder = if want_hw { hw_decoder(&info.codec) } else { None };
        if let Some(d) = decoder {
            let check = Command::new(tool("ffmpeg"))
                .args(["-loglevel", "warning"])
                .args(decode_args(Some(d)))
                .args(["-frames:v", "1", "-f", "null", "-"])
                .stdin(Stdio::null())
                .output();
            match check {
                Ok(o) if o.status.success() => {}
                Ok(o) => {
                    eprintln!("video: hardware decoder {d} failed, using software. ffmpeg said:");
                    for line in String::from_utf8_lossy(&o.stderr).lines().take(20) {
                        eprintln!("    {line}");
                    }
                    decoder = None;
                }
                Err(e) => return Err(format!("could not run ffmpeg: {e}")),
            }
        }

        let mut cmd = Command::new(tool("ffmpeg"));
        cmd.args(["-loglevel", "error"])
            .args(decode_args(decoder))
            .args(["-filter_threads", "4", "-f", "rawvideo", "pipe:1"])
            .stdout(Stdio::piped())
            .stdin(Stdio::null());
        eprintln!("video: {:?}", cmd);
        let mut video = cmd.spawn().map_err(|e| format!("could not run ffmpeg: {e}"))?;
        let mut pipe = video.stdout.take().unwrap();

        let audio = {
            let mut a = Command::new(tool("ffplay"));
            a.args(["-nodisp", "-autoexit", "-loglevel", "error"]);
            if start_secs > 0.0 {
                a.args(["-ss", &format!("{start_secs:.3}")]);
            }
            a.arg(input).stdin(Stdio::null()).stdout(Stdio::null());
            match a.spawn() {
                Ok(c) => Some(c),
                Err(e) => {
                    eprintln!("audio: ffplay unavailable ({e}); playing without sound");
                    None
                }
            }
        };

        let stop = Arc::new(AtomicBool::new(false));
        let stop_r = stop.clone();
        let frame_time = Duration::from_secs_f64(1.0 / info.fps.clamp(1.0, 240.0));
        thread::spawn(move || {
            let t0 = Instant::now();
            let mut n: u32 = 0;
            while !stop_r.load(Ordering::Relaxed) {
                let i = slots.free_slot();
                if let Err(e) = pipe.read_exact(slots.slot_mut(i)) {
                    eprintln!("video: decoder stream ended ({e})");
                    break;
                }
                let due = t0 + frame_time * n;
                let now = Instant::now();
                if due > now {
                    thread::sleep(due - now);
                } else if now - due > frame_time * 30 {
                    eprintln!("video: decoder running {:.0} ms behind", (now - due).as_secs_f64() * 1e3);
                }
                slots.publish(i);
                n += 1;
            }
        });

        Ok(Self { video, audio, stop, hw: decoder.is_some() })
    }
}

impl Drop for Playback {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.video.kill();
        let _ = self.video.wait();
        if let Some(a) = &mut self.audio {
            let _ = a.kill();
            let _ = a.wait();
        }
    }
}
