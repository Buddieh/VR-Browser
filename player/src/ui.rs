//! The control panel: drawn on the CPU into an RGBA image that the
//! compositor shows as a flat panel floating in front of the viewer. The
//! controllers point at it; where a pointer hits, a dot is drawn.

pub const W: usize = 1024;
pub const H: usize = 440;
/// Panel size in metres.
pub const WIDTH_M: f32 = 1.1;
pub const HEIGHT_M: f32 = WIDTH_M * H as f32 / W as f32;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Control {
    Back,
    PlayPause,
    Forward,
    Picture,
    Stereo,
    Recenter,
    Quit,
    /// Seek bar, at this fraction of the video.
    Seek(f32),
}

/// What the panel shows.
#[derive(Clone, PartialEq)]
pub struct View {
    pub title: String,
    /// Whole seconds, so the panel only redraws when the clock changes.
    pub position: u32,
    pub duration: Option<u32>,
    pub paused: bool,
    pub ended: bool,
    pub picture: &'static str,
    pub stereo: &'static str,
    pub hover: Option<Control>,
    /// Pointer dots, in panel pixels.
    pub cursors: Vec<(i32, i32)>,
}

struct Rect {
    x: usize,
    y: usize,
    w: usize,
    h: usize,
}

impl Rect {
    fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x as f32 && x < (self.x + self.w) as f32 && y >= self.y as f32 && y < (self.y + self.h) as f32
    }
}

const SEEK_BAR: Rect = Rect { x: 40, y: 92, w: 944, h: 26 };
/// The seek bar reacts a little above and below what is drawn.
const SEEK_HIT: Rect = Rect { x: 30, y: 72, w: 964, h: 66 };

const BUTTONS: [(Control, Rect); 7] = [
    (Control::Back, Rect { x: 40, y: 180, w: 160, h: 90 }),
    (Control::PlayPause, Rect { x: 215, y: 180, w: 200, h: 90 }),
    (Control::Forward, Rect { x: 430, y: 180, w: 160, h: 90 }),
    (Control::Picture, Rect { x: 605, y: 180, w: 190, h: 90 }),
    (Control::Stereo, Rect { x: 810, y: 180, w: 174, h: 90 }),
    (Control::Recenter, Rect { x: 40, y: 290, w: 300, h: 70 }),
    (Control::Quit, Rect { x: 784, y: 290, w: 200, h: 70 }),
];

/// The control at panel pixel (x, y), if any.
pub fn hit(x: f32, y: f32) -> Option<Control> {
    if SEEK_HIT.contains(x, y) {
        let f = (x - SEEK_BAR.x as f32) / SEEK_BAR.w as f32;
        return Some(Control::Seek(f.clamp(0.0, 1.0)));
    }
    BUTTONS.iter().find(|(_, r)| r.contains(x, y)).map(|(c, _)| *c)
}

struct Font {
    height: usize,
    /// (offset into data, width) for characters 32..=126.
    glyphs: Vec<(usize, usize)>,
    data: &'static [u8],
}

impl Font {
    /// Format written by scripts/gen-font.py.
    fn load(data: &'static [u8]) -> Self {
        let height = data[0] as usize;
        let mut glyphs = Vec::new();
        let mut at = 2;
        for _ in 32..=126 {
            let w = data[at] as usize;
            glyphs.push((at + 1, w));
            at += 1 + w * height;
        }
        Self { height, glyphs, data }
    }

    fn glyph(&self, c: char) -> (usize, usize) {
        let i = if (' '..='~').contains(&c) { c as usize - 32 } else { '?' as usize - 32 };
        self.glyphs[i]
    }

    fn width(&self, s: &str) -> usize {
        s.chars().map(|c| self.glyph(c).1).sum()
    }
}

pub struct Panel {
    big: Font,
    small: Font,
    /// Swap red and blue when the swapchain is BGRA.
    bgra: bool,
}

type Rgb = [u8; 3];
const BG: Rgb = [24, 26, 32];
const BUTTON: Rgb = [52, 56, 66];
const BUTTON_HOVER: Rgb = [84, 110, 160];
const TEXT: Rgb = [235, 236, 240];
const DIM: Rgb = [150, 154, 165];
const TRACK: Rgb = [70, 74, 84];
const PROGRESS: Rgb = [90, 150, 255];
const QUIT: Rgb = [110, 50, 50];

impl Panel {
    pub fn new(bgra: bool) -> Self {
        Self {
            big: Font::load(include_bytes!("../assets/font-34.bin")),
            small: Font::load(include_bytes!("../assets/font-24.bin")),
            bgra,
        }
    }

    /// Draw `view` into `out` (W x H, 4 bytes per pixel).
    pub fn draw(&self, view: &View, out: &mut [u8]) {
        let mut c = Canvas { px: out, bgra: self.bgra };
        c.fill(&Rect { x: 0, y: 0, w: W, h: H }, BG);

        let title = fit(&self.big, &view.title, W - 80);
        c.text(&self.big, 40, 20, &title, TEXT);

        // Seek bar and time.
        c.fill(&SEEK_BAR, TRACK);
        if let Some(d) = view.duration.filter(|d| *d > 0) {
            let done = (SEEK_BAR.w as f32 * (view.position as f32 / d as f32).min(1.0)) as usize;
            c.fill(&Rect { w: done, ..SEEK_BAR }, PROGRESS);
        }
        if let Some(Control::Seek(f)) = view.hover {
            let x = SEEK_BAR.x + (f * SEEK_BAR.w as f32) as usize;
            c.fill(&Rect { x: x.saturating_sub(2), y: SEEK_BAR.y - 8, w: 4, h: SEEK_BAR.h + 16 }, TEXT);
            if let Some(d) = view.duration {
                let t = clock((f * d as f32) as u32);
                let tx = x.saturating_sub(self.small.width(&t) / 2).clamp(40, W - 40 - self.small.width(&t));
                c.text(&self.small, tx, SEEK_BAR.y - 34, &t, TEXT);
            }
        }
        let time = match view.duration {
            Some(d) => format!("{} / {}", clock(view.position), clock(d)),
            None => clock(view.position),
        };
        c.text(&self.small, 40, 128, &time, DIM);
        let state = if view.ended {
            "Finished"
        } else if view.paused {
            "Paused"
        } else {
            ""
        };
        c.text(&self.small, W - 40 - self.small.width(state), 128, state, DIM);

        for (control, r) in &BUTTONS {
            let hovered = view.hover == Some(*control);
            let base = if *control == Control::Quit { QUIT } else { BUTTON };
            c.fill(r, if hovered { BUTTON_HOVER } else { base });
            let (label, value) = match control {
                Control::Back => ("", "<< 10s"),
                Control::PlayPause => ("", if view.ended { "Replay" } else if view.paused { "Play" } else { "Pause" }),
                Control::Forward => ("", "10s >>"),
                Control::Picture => ("Picture", view.picture),
                Control::Stereo => ("3D", view.stereo),
                Control::Recenter => ("", "Recenter"),
                Control::Quit => ("", "Quit"),
                Control::Seek(_) => ("", ""),
            };
            let value = fit(&self.big, value, r.w - 16);
            if label.is_empty() {
                let x = r.x + (r.w - self.big.width(&value)) / 2;
                c.text(&self.big, x, r.y + (r.h - self.big.height) / 2, &value, TEXT);
            } else {
                c.text(&self.small, r.x + (r.w - self.small.width(label)) / 2, r.y + 8, label, DIM);
                let x = r.x + (r.w - self.big.width(&value)) / 2;
                c.text(&self.big, x, r.y + r.h - self.big.height - 8, &value, TEXT);
            }
        }

        let help = "Trigger: click    A / X: pause    Stick: skip 10s    B / Y: hide panel";
        c.text(&self.small, (W - self.small.width(help)) / 2, 392, help, DIM);

        for &(x, y) in &view.cursors {
            c.dot(x, y, 11, [20, 20, 20]);
            c.dot(x, y, 8, [255, 255, 255]);
        }
    }
}

/// `s`, shortened with "..." to fit `max` pixels.
fn fit(font: &Font, s: &str, max: usize) -> String {
    if font.width(s) <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().collect();
    while !out.is_empty() && font.width(&out) + font.width("...") > max {
        out.pop();
    }
    out + "..."
}

/// 75 -> "1:15", 3725 -> "1:02:05".
fn clock(secs: u32) -> String {
    let (h, m, s) = (secs / 3600, secs / 60 % 60, secs % 60);
    if h > 0 { format!("{h}:{m:02}:{s:02}") } else { format!("{m}:{s:02}") }
}

struct Canvas<'a> {
    px: &'a mut [u8],
    bgra: bool,
}

impl Canvas<'_> {
    fn put(&mut self, x: usize, y: usize, c: Rgb, alpha: u8) {
        if x >= W || y >= H || alpha == 0 {
            return;
        }
        let i = (y * W + x) * 4;
        let c = if self.bgra { [c[2], c[1], c[0]] } else { c };
        let a = alpha as u32;
        for k in 0..3 {
            self.px[i + k] = ((c[k] as u32 * a + self.px[i + k] as u32 * (255 - a)) / 255) as u8;
        }
        self.px[i + 3] = 255;
    }

    fn fill(&mut self, r: &Rect, c: Rgb) {
        for y in r.y..(r.y + r.h).min(H) {
            for x in r.x..(r.x + r.w).min(W) {
                self.put(x, y, c, 255);
            }
        }
    }

    fn text(&mut self, font: &Font, x: usize, y: usize, s: &str, c: Rgb) {
        let mut pen = x;
        for ch in s.chars() {
            let (at, w) = font.glyph(ch);
            for gy in 0..font.height {
                for gx in 0..w {
                    self.put(pen + gx, y + gy, c, font.data[at + gy * w + gx]);
                }
            }
            pen += w;
        }
    }

    fn dot(&mut self, cx: i32, cy: i32, r: i32, c: Rgb) {
        for y in cy - r..=cy + r {
            for x in cx - r..=cx + r {
                if x >= 0 && y >= 0 && (x - cx).pow(2) + (y - cy).pow(2) <= r * r {
                    self.put(x as usize, y as usize, c, 255);
                }
            }
        }
    }
}
