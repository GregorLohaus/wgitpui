//! Immediate-mode UI core: geometry, colors, a batched painter that emits SDF-shaded quads,
//! input state, and a handful of widgets. The app redraws its whole UI every frame it is awake.

use crate::font::{Fonts, FontId};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub const fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Rect { x, y, w, h }
    }
    pub fn right(&self) -> f32 {
        self.x + self.w
    }
    pub fn bottom(&self) -> f32 {
        self.y + self.h
    }
    pub fn cy(&self) -> f32 {
        self.y + self.h * 0.5
    }
    pub fn contains(&self, p: (f32, f32)) -> bool {
        p.0 >= self.x && p.0 < self.right() && p.1 >= self.y && p.1 < self.bottom()
    }
    pub fn shrink(&self, d: f32) -> Rect {
        self.pad(d, d)
    }
    pub fn pad(&self, dx: f32, dy: f32) -> Rect {
        Rect::new(self.x + dx, self.y + dy, (self.w - 2.0 * dx).max(0.0), (self.h - 2.0 * dy).max(0.0))
    }
    pub fn take_top(&mut self, h: f32) -> Rect {
        let h = h.clamp(0.0, self.h);
        let r = Rect::new(self.x, self.y, self.w, h);
        self.y += h;
        self.h -= h;
        r
    }
    pub fn take_left(&mut self, w: f32) -> Rect {
        let w = w.clamp(0.0, self.w);
        let r = Rect::new(self.x, self.y, w, self.h);
        self.x += w;
        self.w -= w;
        r
    }
    pub fn take_right(&mut self, w: f32) -> Rect {
        let w = w.clamp(0.0, self.w);
        self.w -= w;
        Rect::new(self.x + self.w, self.y, w, self.h)
    }
    pub fn intersect(&self, o: &Rect) -> Rect {
        let x = self.x.max(o.x);
        let y = self.y.max(o.y);
        let r = self.right().min(o.right());
        let b = self.bottom().min(o.bottom());
        Rect::new(x, y, (r - x).max(0.0), (b - y).max(0.0))
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Color(pub [f32; 4]);

impl Color {
    pub const fn hex(h: u32) -> Color {
        Color([
            ((h >> 16) & 255) as f32 / 255.0,
            ((h >> 8) & 255) as f32 / 255.0,
            (h & 255) as f32 / 255.0,
            1.0,
        ])
    }
    pub const fn alpha(self, a: f32) -> Color {
        Color([self.0[0], self.0[1], self.0[2], self.0[3] * a])
    }
    pub fn mix(self, o: Color, t: f32) -> Color {
        let m = |a: f32, b: f32| a + (b - a) * t;
        Color([m(self.0[0], o.0[0]), m(self.0[1], o.0[1]), m(self.0[2], o.0[2]), m(self.0[3], o.0[3])])
    }
}

pub mod theme {
    use super::Color;
    pub const BG: Color = Color::hex(0x0c0f15);
    pub const PANEL: Color = Color::hex(0x11151d);
    pub const PANEL2: Color = Color::hex(0x171c26);
    pub const RAISED: Color = Color::hex(0x1e2430);
    pub const HOVER: Color = Color::hex(0x1f2633);
    pub const SELECT: Color = Color::hex(0x22304d);
    pub const BORDER: Color = Color::hex(0x232a37);
    pub const TEXT: Color = Color::hex(0xe2e6ee);
    pub const DIM: Color = Color::hex(0x8b94a7);
    pub const FAINT: Color = Color::hex(0x586174);
    pub const ACCENT: Color = Color::hex(0x7aa2f7);
    pub const GREEN: Color = Color::hex(0x9ece6a);
    pub const RED: Color = Color::hex(0xf7768e);
    pub const YELLOW: Color = Color::hex(0xe0af68);
    pub const PURPLE: Color = Color::hex(0xbb9af7);
    pub const CYAN: Color = Color::hex(0x7dcfff);
    pub const ORANGE: Color = Color::hex(0xff9e64);
    pub const LANES: [Color; 8] = [
        Color::hex(0x7aa2f7),
        Color::hex(0x9ece6a),
        Color::hex(0xbb9af7),
        Color::hex(0xff9e64),
        Color::hex(0x7dcfff),
        Color::hex(0xf7768e),
        Color::hex(0xe0af68),
        Color::hex(0x73daca),
    ];
    pub fn lane(i: usize) -> Color {
        LANES[i % LANES.len()]
    }
}

// ---------------------------------------------------------------------------------------------
// Painter
// ---------------------------------------------------------------------------------------------

/// Shader modes (params[0]).
const MODE_SOLID: f32 = 0.0;
const MODE_GLYPH: f32 = 1.0;
const MODE_RRECT: f32 = 2.0;
const MODE_CAPSULE: f32 = 3.0;

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct Vertex {
    pub pos: [f32; 2],
    pub uv: [f32; 2],
    pub color: [f32; 4],
    /// rrect: center.xy, half-size.wh  |  capsule: a.xy, b.xy
    pub shape: [f32; 4],
    /// mode, radius, stroke width, unused
    pub params: [f32; 4],
}

pub struct DrawCmd {
    pub clip: Rect,
    pub start: u32,
    pub count: u32,
}

pub struct Painter {
    pub verts: Vec<Vertex>,
    pub indices: Vec<u32>,
    pub cmds: Vec<DrawCmd>,
    clips: Vec<Rect>,
    pub fonts: Fonts,
    pub scale: f32,
}

impl Painter {
    pub fn new(fonts: Fonts) -> Self {
        Painter { verts: vec![], indices: vec![], cmds: vec![], clips: vec![], fonts, scale: 1.0 }
    }

    pub fn begin(&mut self, screen: Rect, scale: f32) {
        self.verts.clear();
        self.indices.clear();
        self.cmds.clear();
        self.clips.clear();
        self.clips.push(screen);
        self.cmds.push(DrawCmd { clip: screen, start: 0, count: 0 });
        self.scale = scale;
    }

    pub fn clip(&self) -> Rect {
        *self.clips.last().unwrap()
    }

    pub fn push_clip(&mut self, r: Rect) {
        let c = r.intersect(&self.clip());
        self.clips.push(c);
        self.set_cmd_clip(c);
    }

    pub fn pop_clip(&mut self) {
        if self.clips.len() > 1 {
            self.clips.pop();
        }
        self.set_cmd_clip(self.clip());
    }

    fn set_cmd_clip(&mut self, clip: Rect) {
        let last = self.cmds.last_mut().unwrap();
        if last.count == 0 {
            last.clip = clip;
        } else {
            let start = self.indices.len() as u32;
            self.cmds.push(DrawCmd { clip, start, count: 0 });
        }
    }

    fn quad(&mut self, p: [[f32; 2]; 4], uv: [[f32; 2]; 4], color: Color, shape: [f32; 4], params: [f32; 4]) {
        let c = self.clip();
        let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
        for q in &p {
            x0 = x0.min(q[0]);
            y0 = y0.min(q[1]);
            x1 = x1.max(q[0]);
            y1 = y1.max(q[1]);
        }
        if x1 < c.x || y1 < c.y || x0 > c.right() || y0 > c.bottom() || color.0[3] <= 0.0 {
            return;
        }
        let base = self.verts.len() as u32;
        for i in 0..4 {
            self.verts.push(Vertex { pos: p[i], uv: uv[i], color: color.0, shape, params });
        }
        self.indices.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
        self.cmds.last_mut().unwrap().count += 6;
    }

    fn corners(r: Rect) -> [[f32; 2]; 4] {
        [[r.x, r.y], [r.right(), r.y], [r.right(), r.bottom()], [r.x, r.bottom()]]
    }

    pub fn rect(&mut self, r: Rect, c: Color) {
        self.quad(Self::corners(r), [[0.0; 2]; 4], c, [0.0; 4], [MODE_SOLID, 0.0, 0.0, 0.0]);
    }

    fn rrect_impl(&mut self, r: Rect, radius: f32, stroke: f32, c: Color) {
        let e = 1.0;
        let q = Rect::new(r.x - e, r.y - e, r.w + 2.0 * e, r.h + 2.0 * e);
        let radius = radius.min(r.w * 0.5).min(r.h * 0.5);
        let shape = [r.x + r.w * 0.5, r.y + r.h * 0.5, r.w * 0.5, r.h * 0.5];
        self.quad(Self::corners(q), [[0.0; 2]; 4], c, shape, [MODE_RRECT, radius, stroke, 0.0]);
    }

    pub fn rounded(&mut self, r: Rect, radius: f32, c: Color) {
        self.rrect_impl(r, radius, 0.0, c);
    }

    pub fn rounded_stroke(&mut self, r: Rect, radius: f32, width: f32, c: Color) {
        self.rrect_impl(r, radius, width, c);
    }

    pub fn circle(&mut self, (x, y): (f32, f32), radius: f32, c: Color) {
        self.rounded(Rect::new(x - radius, y - radius, radius * 2.0, radius * 2.0), radius, c);
    }

    pub fn ring(&mut self, (x, y): (f32, f32), radius: f32, width: f32, c: Color) {
        self.rounded_stroke(Rect::new(x - radius, y - radius, radius * 2.0, radius * 2.0), radius, width, c);
    }

    /// Anti-aliased line with round caps (capsule SDF on an oriented quad).
    pub fn line(&mut self, a: (f32, f32), b: (f32, f32), width: f32, c: Color) {
        let (dx, dy) = (b.0 - a.0, b.1 - a.1);
        let len = (dx * dx + dy * dy).sqrt();
        let (ux, uy) = if len > 1e-4 { (dx / len, dy / len) } else { (1.0, 0.0) };
        let (nx, ny) = (-uy, ux);
        let r = width * 0.5;
        let e = r + 1.0;
        let p = [
            [a.0 - ux * e + nx * e, a.1 - uy * e + ny * e],
            [b.0 + ux * e + nx * e, b.1 + uy * e + ny * e],
            [b.0 + ux * e - nx * e, b.1 + uy * e - ny * e],
            [a.0 - ux * e - nx * e, a.1 - uy * e - ny * e],
        ];
        self.quad(p, [[0.0; 2]; 4], c, [a.0, a.1, b.0, b.1], [MODE_CAPSULE, r, 0.0, 0.0]);
    }

    /// Vertical S-curve between two points (used for graph edges that change lanes).
    pub fn curve(&mut self, a: (f32, f32), b: (f32, f32), width: f32, c: Color) {
        if (a.0 - b.0).abs() < 0.5 {
            return self.line(a, b, width, c);
        }
        let my = (a.1 + b.1) * 0.5;
        let (c1, c2) = ((a.0, my), (b.0, my));
        let n = 10;
        let mut prev = a;
        for i in 1..=n {
            let t = i as f32 / n as f32;
            let mt = 1.0 - t;
            let p = (
                mt * mt * mt * a.0 + 3.0 * mt * mt * t * c1.0 + 3.0 * mt * t * t * c2.0 + t * t * t * b.0,
                mt * mt * mt * a.1 + 3.0 * mt * mt * t * c1.1 + 3.0 * mt * t * t * c2.1 + t * t * t * b.1,
            );
            self.line(prev, p, width, c);
            prev = p;
        }
    }

    pub fn measure(&mut self, font: FontId, size: f32, s: &str) -> f32 {
        let px = size * self.scale;
        s.chars().map(|c| self.fonts.advance(font, c, px)).sum::<f32>() / self.scale
    }

    /// Draws a single line of text whose visual center sits at `cy`. Returns the advance width.
    pub fn text(&mut self, x: f32, cy: f32, font: FontId, size: f32, s: &str, c: Color) -> f32 {
        self.text_impl(x, cy, font, size, s, c, f32::MAX)
    }

    /// Like `text`, but truncates with an ellipsis to fit `max_w`.
    pub fn text_fit(&mut self, x: f32, cy: f32, max_w: f32, font: FontId, size: f32, s: &str, c: Color) -> f32 {
        if max_w <= 0.0 {
            return 0.0;
        }
        if self.measure(font, size, s) <= max_w {
            return self.text(x, cy, font, size, s, c);
        }
        let ell = self.measure(font, size, "…");
        let mut w = 0.0;
        let mut end = 0;
        let px = size * self.scale;
        for (i, ch) in s.char_indices() {
            let a = self.fonts.advance(font, ch, px) / self.scale;
            if w + a + ell > max_w {
                break;
            }
            w += a;
            end = i + ch.len_utf8();
        }
        let w1 = self.text(x, cy, font, size, &s[..end], c);
        w1 + self.text(x + w1, cy, font, size, "…", c)
    }

    fn text_impl(&mut self, x: f32, cy: f32, font: FontId, size: f32, s: &str, c: Color, max_w: f32) -> f32 {
        let sc = self.scale;
        let px = size * sc;
        // Center on cap height (~0.7em) so mixed-case labels look vertically centered.
        let baseline = ((cy + size * 0.36) * sc).round();
        let x0 = x * sc;
        let mut pen = x0;
        let clip = self.clip();
        for ch in s.chars() {
            let adv = self.fonts.advance(font, ch, px);
            if (pen - x0) / sc > max_w {
                break;
            }
            let gx = pen / sc;
            if gx > clip.right() {
                pen += adv;
                break;
            }
            if gx + adv / sc >= clip.x && !ch.is_whitespace() {
                if let Some(g) = self.fonts.glyph(font, ch, px) {
                    let x = (pen + g.left).round() / sc;
                    let y = (baseline + g.top) / sc;
                    let r = Rect::new(x, y, g.w / sc, g.h / sc);
                    let [u0, v0, u1, v1] = g.uv;
                    self.quad(
                        Self::corners(r),
                        [[u0, v0], [u1, v0], [u1, v1], [u0, v1]],
                        c,
                        [0.0; 4],
                        [MODE_GLYPH, 0.0, 0.0, 0.0],
                    );
                }
            }
            pen += adv;
        }
        (pen - x0) / sc
    }

    /// Greedy word wrap.
    pub fn wrap(&mut self, font: FontId, size: f32, text: &str, max_w: f32) -> Vec<String> {
        let mut out = Vec::new();
        for para in text.split('\n') {
            let mut line = String::new();
            for word in para.split(' ') {
                let candidate = if line.is_empty() { word.to_string() } else { format!("{line} {word}") };
                if !line.is_empty() && self.measure(font, size, &candidate) > max_w {
                    out.push(std::mem::take(&mut line));
                    line = word.to_string();
                } else {
                    line = candidate;
                }
            }
            out.push(line);
        }
        out
    }
}

// ---------------------------------------------------------------------------------------------
// Input
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Key {
    Enter,
    Escape,
    Backspace,
    Delete,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    PageUp,
    PageDown,
    Tab,
    Char(char),
}

#[derive(Clone, Copy, Default, Debug)]
pub struct Mods {
    #[allow(dead_code)]
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub logo: bool,
}

impl Mods {
    /// Cmd on macOS, Ctrl elsewhere.
    pub fn primary(&self) -> bool {
        if cfg!(target_os = "macos") { self.logo } else { self.ctrl }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct KeyPress {
    pub key: Key,
    pub mods: Mods,
}

#[derive(Default)]
pub struct Input {
    pub mouse: (f32, f32),
    pub down: bool,
    pub pressed: bool,
    /// Secondary click (right button, or ctrl-click on macOS).
    pub right_pressed: bool,
    pub released: bool,
    pub double: bool,
    pub scroll: (f32, f32),
    /// True when the scroll came from a precise device (trackpad) and should not be smoothed.
    pub scroll_precise: bool,
    pub text: String,
    pub keys: Vec<KeyPress>,
    pub mods: Mods,
}

impl Input {
    /// Removes and returns the first key press matching `key` with the primary modifier state `primary`.
    pub fn take_key(&mut self, key: Key, primary: bool) -> Option<KeyPress> {
        let i = self.keys.iter().position(|k| k.key == key && k.mods.primary() == primary)?;
        Some(self.keys.remove(i))
    }

    pub fn end_frame(&mut self) {
        self.pressed = false;
        self.right_pressed = false;
        self.released = false;
        self.double = false;
        self.scroll = (0.0, 0.0);
        self.text.clear();
        self.keys.clear();
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Cursor {
    Default,
    Pointer,
    Text,
    ResizeH,
    Grab,
    Grabbing,
}

pub struct Ui {
    pub p: Painter,
    pub input: Input,
    pub screen: Rect,
    /// When set, the background is covered by a modal and must not react to the mouse.
    pub blocked: bool,
    pub animating: bool,
    pub cursor: Cursor,
    pub wake_at: Option<Instant>,
    /// Requests for the window shell (custom title bar behavior).
    pub drag_window: bool,
    pub toggle_maximize: bool,
    pub now: Instant,
    pub dt: f32,
}

impl Ui {
    pub fn new(fonts: Fonts) -> Self {
        Ui {
            p: Painter::new(fonts),
            input: Input::default(),
            screen: Rect::default(),
            blocked: false,
            animating: false,
            cursor: Cursor::Default,
            wake_at: None,
            drag_window: false,
            toggle_maximize: false,
            now: Instant::now(),
            dt: 1.0 / 60.0,
        }
    }

    pub fn begin(&mut self, w: f32, h: f32, scale: f32) {
        let now = Instant::now();
        self.dt = (now - self.now).as_secs_f32().clamp(0.0, 0.1);
        self.now = now;
        self.screen = Rect::new(0.0, 0.0, w, h);
        self.p.begin(self.screen, scale);
        self.animating = false;
        self.cursor = Cursor::Default;
        self.wake_at = None;
        self.blocked = false;
        self.drag_window = false;
        self.toggle_maximize = false;
    }

    pub fn wake_in(&mut self, d: Duration) {
        let t = self.now + d;
        self.wake_at = Some(self.wake_at.map_or(t, |w| w.min(t)));
    }

    pub fn hovered(&self, r: Rect) -> bool {
        !self.blocked && r.contains(self.input.mouse) && self.p.clip().contains(self.input.mouse)
    }


    pub fn button(&mut self, r: Rect, label: &str, primary: bool, enabled: bool) -> bool {
        let hov = enabled && self.hovered(r);
        let (bg, fg) = match (primary, enabled) {
            (_, false) => (theme::RAISED.alpha(0.5), theme::FAINT),
            (true, true) => (if hov { theme::ACCENT.mix(theme::TEXT, 0.15) } else { theme::ACCENT }, theme::BG),
            (false, true) => (if hov { theme::HOVER.mix(theme::TEXT, 0.08) } else { theme::RAISED }, theme::TEXT),
        };
        self.p.rounded(r, 6.0, bg);
        if !primary {
            self.p.rounded_stroke(r, 6.0, 1.0, theme::BORDER.mix(theme::TEXT, if hov { 0.15 } else { 0.0 }));
        }
        let tw = self.p.measure(crate::font::UI, 12.5, label);
        self.p.text(r.x + (r.w - tw) * 0.5, r.cy(), crate::font::UI, 12.5, label, fg);
        if hov {
            self.cursor = Cursor::Pointer;
        }
        hov && self.input.pressed
    }

    /// A quiet button that turns red on hover, for destructive actions.
    pub fn danger_button(&mut self, r: Rect, label: &str) -> bool {
        let hov = self.hovered(r);
        let (bg, fg) = if hov { (theme::RED.alpha(0.2), theme::RED) } else { (theme::RAISED, theme::DIM) };
        self.p.rounded(r, 6.0, bg);
        self.p.rounded_stroke(r, 6.0, 1.0, if hov { theme::RED.alpha(0.5) } else { theme::BORDER });
        let tw = self.p.measure(crate::font::UI, 12.5, label);
        self.p.text(r.x + (r.w - tw) * 0.5, r.cy(), crate::font::UI, 12.5, label, fg);
        if hov {
            self.cursor = Cursor::Pointer;
        }
        hov && self.input.pressed
    }

    /// A row of mutually exclusive options. Returns (width, clicked index).
    pub fn segmented(&mut self, x: f32, cy: f32, options: &[&str], active: usize) -> (f32, Option<usize>) {
        let size = 11.5;
        let widths: Vec<f32> = options.iter().map(|o| self.p.measure(crate::font::UI, size, o) + 18.0).collect();
        let total: f32 = widths.iter().sum::<f32>() + 4.0;
        let outer = Rect::new(x, cy - 12.0, total, 24.0);
        self.p.rounded(outer, 6.0, theme::BG);
        self.p.rounded_stroke(outer, 6.0, 1.0, theme::BORDER);
        let mut cx = x + 2.0;
        let mut clicked = None;
        for (i, (o, w)) in options.iter().zip(&widths).enumerate() {
            let r = Rect::new(cx, cy - 10.0, *w, 20.0);
            let hov = self.hovered(r);
            if i == active {
                self.p.rounded(r, 4.5, theme::RAISED.mix(theme::TEXT, 0.06));
            } else if hov {
                self.p.rounded(r, 4.5, theme::HOVER);
            }
            let fg = if i == active { theme::TEXT } else { theme::DIM };
            self.p.text(cx + 9.0, cy, crate::font::UI, size, o, fg);
            if hov {
                self.cursor = Cursor::Pointer;
                if self.input.pressed && i != active {
                    clicked = Some(i);
                }
            }
            cx += w;
        }
        (total, clicked)
    }

    /// A pill-shaped on/off toggle. Returns (width, clicked).
    pub fn toggle_chip(&mut self, x: f32, cy: f32, label: &str, on: bool) -> (f32, bool) {
        let size = 11.5;
        let w = self.p.measure(crate::font::UI, size, label) + 30.0;
        let r = Rect::new(x, cy - 12.0, w, 24.0);
        let hov = self.hovered(r);
        self.p.rounded(r, 12.0, if on { theme::ACCENT.alpha(0.16) } else if hov { theme::HOVER } else { theme::BG });
        self.p.rounded_stroke(r, 12.0, 1.0, if on { theme::ACCENT.alpha(0.5) } else { theme::BORDER });
        let dot = (x + 11.0, cy);
        if on {
            self.p.circle(dot, 3.5, theme::ACCENT);
        } else {
            self.p.ring(dot, 3.5, 1.2, theme::FAINT);
        }
        self.p.text(x + 20.0, cy, crate::font::UI, size, label, if on { theme::TEXT } else { theme::DIM });
        if hov {
            self.cursor = Cursor::Pointer;
        }
        (w, hov && self.input.pressed)
    }

    /// A small rounded label.
    pub fn pill(&mut self, x: f32, cy: f32, label: &str, fg: Color, bg: Color, outline: bool) -> f32 {
        let size = 11.0;
        let w = self.p.measure(crate::font::UI, size, label) + 12.0;
        let r = Rect::new(x, cy - 9.0, w, 18.0);
        if outline {
            self.p.rounded_stroke(r, 9.0, 1.0, fg.alpha(0.6));
        } else {
            self.p.rounded(r, 9.0, bg);
        }
        self.p.text(x + 6.0, cy, crate::font::UI, size, label, fg);
        w
    }
}

// ---------------------------------------------------------------------------------------------
// Smooth scrolling
// ---------------------------------------------------------------------------------------------

#[derive(Default)]
pub struct Scroll {
    pub offset: f32,
    pub target: f32,
    pub max: f32,
}

impl Scroll {
    /// Consumes wheel input over `area`, clamps against `content` height and animates.
    pub fn update(&mut self, ui: &mut Ui, area: Rect, content: f32) -> f32 {
        self.max = (content - area.h).max(0.0);
        if ui.hovered(area) && ui.input.scroll.1 != 0.0 {
            self.target -= ui.input.scroll.1;
            if ui.input.scroll_precise {
                self.target = self.target.clamp(0.0, self.max);
                self.offset = self.target;
            }
            ui.input.scroll.1 = 0.0;
        }
        self.target = self.target.clamp(0.0, self.max);
        let d = self.target - self.offset;
        if d.abs() < 0.5 {
            self.offset = self.target;
        } else {
            self.offset += d * (ui.dt * 18.0).min(1.0);
            ui.animating = true;
        }
        self.offset = self.offset.clamp(0.0, self.max);
        self.offset
    }

    /// Scrolls the minimum amount so that [y0, y1) (content coordinates) is visible.
    pub fn reveal(&mut self, y0: f32, y1: f32, view_h: f32) {
        if y0 < self.target {
            self.target = y0;
        } else if y1 > self.target + view_h {
            self.target = y1 - view_h;
        }
    }

    pub fn jump(&mut self, y: f32) {
        self.target = y.max(0.0);
    }
}

// ---------------------------------------------------------------------------------------------
// Text editing
// ---------------------------------------------------------------------------------------------

#[derive(Default)]
pub struct TextEdit {
    pub text: String,
    pub cursor: usize,
}

impl TextEdit {
    pub fn set(&mut self, s: &str) {
        self.text = s.to_string();
        self.cursor = self.text.len();
    }

    fn prev(&self, i: usize) -> usize {
        self.text[..i].char_indices().next_back().map_or(0, |(j, _)| j)
    }

    fn next(&self, i: usize) -> usize {
        self.text[i..].chars().next().map_or(i, |c| i + c.len_utf8())
    }

    fn word_left(&self, mut i: usize) -> usize {
        let b = self.text.as_bytes();
        while i > 0 && b[i - 1].is_ascii_whitespace() {
            i -= 1;
        }
        while i > 0 && !b[i - 1].is_ascii_whitespace() {
            i = self.prev(i);
        }
        i
    }

    fn word_right(&self, mut i: usize) -> usize {
        let b = self.text.as_bytes();
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        while i < b.len() && !b[i].is_ascii_whitespace() {
            i = self.next(i);
        }
        i
    }

    fn line_start(&self, i: usize) -> usize {
        self.text[..i].rfind('\n').map_or(0, |p| p + 1)
    }

    fn line_end(&self, i: usize) -> usize {
        self.text[i..].find('\n').map_or(self.text.len(), |p| i + p)
    }

    fn insert(&mut self, s: &str) {
        self.text.insert_str(self.cursor, s);
        self.cursor += s.len();
    }

    /// Applies this frame's keyboard input. Returns true if the text changed.
    pub fn handle(&mut self, input: &mut Input, multiline: bool) -> bool {
        let before = self.text.len();
        let mut changed = false;
        let typed: String = input.text.chars().filter(|c| !c.is_control()).collect();
        if !typed.is_empty() {
            self.insert(&typed);
            changed = true;
        }
        input.text.clear();
        let keys = std::mem::take(&mut input.keys);
        let mut rest = Vec::new();
        for k in keys {
            let (alt, prim) = (k.mods.alt, k.mods.primary());
            match k.key {
                Key::Backspace if self.cursor > 0 => {
                    let to = if prim {
                        self.line_start(self.cursor)
                    } else if alt {
                        self.word_left(self.cursor)
                    } else {
                        self.prev(self.cursor)
                    };
                    let to = if to == self.cursor { self.prev(self.cursor) } else { to };
                    self.text.replace_range(to..self.cursor, "");
                    self.cursor = to;
                    changed = true;
                }
                Key::Delete if self.cursor < self.text.len() => {
                    let to = if alt { self.word_right(self.cursor) } else { self.next(self.cursor) };
                    self.text.replace_range(self.cursor..to, "");
                    changed = true;
                }
                Key::Left => {
                    self.cursor = if prim {
                        self.line_start(self.cursor)
                    } else if alt {
                        self.word_left(self.cursor)
                    } else {
                        self.prev(self.cursor)
                    }
                }
                Key::Right => {
                    self.cursor = if prim {
                        self.line_end(self.cursor)
                    } else if alt {
                        self.word_right(self.cursor)
                    } else {
                        self.next(self.cursor)
                    }
                }
                Key::Home => self.cursor = self.line_start(self.cursor),
                Key::End => self.cursor = self.line_end(self.cursor),
                Key::Up if multiline && !prim => {
                    let ls = self.line_start(self.cursor);
                    if ls > 0 {
                        let col = self.text[ls..self.cursor].chars().count();
                        let pls = self.line_start(ls - 1);
                        self.cursor = self.text[pls..ls - 1]
                            .char_indices()
                            .nth(col)
                            .map_or(ls - 1, |(i, _)| pls + i);
                    }
                }
                Key::Down if multiline && !prim => {
                    let le = self.line_end(self.cursor);
                    if le < self.text.len() {
                        let col = self.text[self.line_start(self.cursor)..self.cursor].chars().count();
                        let nls = le + 1;
                        let nle = self.line_end(nls);
                        self.cursor =
                            self.text[nls..nle].char_indices().nth(col).map_or(nle, |(i, _)| nls + i);
                    }
                }
                Key::Enter if multiline && !prim => {
                    self.insert("\n");
                    changed = true;
                }
                Key::Char('v') if prim => {
                    let clip = crate::clipboard::paste();
                    let clip = if multiline { clip } else { clip.replace(['\n', '\r'], " ") };
                    self.insert(&clip.replace('\r', ""));
                    changed = true;
                }
                Key::Char('a') if prim => self.cursor = self.text.len(),
                Key::Backspace | Key::Delete => {}
                _ => rest.push(k),
            }
        }
        input.keys = rest;
        changed || before != self.text.len()
    }

    /// Draws the editor; returns true when clicked (so the caller can focus it).
    pub fn draw(&self, ui: &mut Ui, r: Rect, placeholder: &str, focused: bool, multiline: bool) -> bool {
        let hov = ui.hovered(r);
        if hov {
            ui.cursor = Cursor::Text;
        }
        ui.p.rounded(r, 6.0, theme::BG);
        let border = if focused { theme::ACCENT.alpha(0.8) } else if hov { theme::FAINT } else { theme::BORDER };
        ui.p.rounded_stroke(r, 6.0, 1.0, border);
        let inner = r.pad(10.0, 7.0);
        let size = 13.0;
        let lh = 18.0;
        ui.p.push_clip(r.shrink(2.0));
        if self.text.is_empty() {
            let cy = if multiline { inner.y + lh * 0.5 } else { r.cy() };
            ui.p.text(inner.x, cy, crate::font::UI, size, placeholder, theme::FAINT);
        }
        let mut cursor_pos = None;
        let mut off = 0;
        for (li, line) in self.text.split('\n').enumerate() {
            let cy = if multiline { inner.y + lh * (li as f32 + 0.5) } else { r.cy() };
            // First line of a commit message is the subject: render it brighter.
            let color = if multiline && li > 0 { theme::DIM.mix(theme::TEXT, 0.5) } else { theme::TEXT };
            ui.p.text(inner.x, cy, crate::font::UI, size, line, color);
            if self.cursor >= off && self.cursor <= off + line.len() && cursor_pos.is_none() {
                let w = ui.p.measure(crate::font::UI, size, &line[..self.cursor - off]);
                cursor_pos = Some((inner.x + w, cy));
            }
            off += line.len() + 1;
        }
        if focused {
            if let Some((x, cy)) = cursor_pos {
                ui.p.rect(Rect::new(x, cy - 8.0, 1.5, 16.0), theme::ACCENT);
            }
        }
        ui.p.pop_clip();
        hov && ui.input.pressed
    }
}
