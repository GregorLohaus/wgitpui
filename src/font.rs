//! A tiny TrueType stack: parse `glyf` outlines from .ttf/.ttc files, rasterize them with an
//! exact-area coverage accumulator, and pack the bitmaps into a single-channel glyph atlas.
//!
//! Supported: cmap formats 4 and 12, simple + composite glyphs, TrueType collections.
//! Not supported (by design, to stay small): CFF outlines, hinting, kerning, shaping.

use std::collections::HashMap;

pub type FontId = usize;
pub const UI: FontId = 0;
pub const MONO: FontId = 1;

// ---------------------------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------------------------

pub struct Font {
    data: Vec<u8>,
    units_per_em: f32,
    pub ascent: f32,
    pub descent: f32,
    num_glyphs: u16,
    num_hmetrics: u16,
    loca_long: bool,
    cmap: usize,
    cmap_format: u16,
    loca: usize,
    glyf: usize,
    hmtx: usize,
}

#[derive(Clone, Copy)]
struct Pt {
    x: f32,
    y: f32,
    on: bool,
}

impl Font {
    fn u8(&self, o: usize) -> u8 {
        self.data.get(o).copied().unwrap_or(0)
    }
    fn u16(&self, o: usize) -> u16 {
        u16::from_be_bytes([self.u8(o), self.u8(o + 1)])
    }
    fn i16(&self, o: usize) -> i16 {
        self.u16(o) as i16
    }
    fn u32(&self, o: usize) -> u32 {
        u32::from_be_bytes([self.u8(o), self.u8(o + 1), self.u8(o + 2), self.u8(o + 3)])
    }

    pub fn parse(data: Vec<u8>) -> Option<Font> {
        let mut f = Font {
            data,
            units_per_em: 1000.0,
            ascent: 0.0,
            descent: 0.0,
            num_glyphs: 0,
            num_hmetrics: 0,
            loca_long: false,
            cmap: 0,
            cmap_format: 0,
            loca: 0,
            glyf: 0,
            hmtx: 0,
        };
        let mut base = 0usize;
        if f.data.get(0..4)? == b"ttcf" {
            base = f.u32(12) as usize; // first face of the collection
        }
        let num_tables = f.u16(base + 4) as usize;
        let table = |tag: &[u8; 4]| -> Option<usize> {
            (0..num_tables).find_map(|i| {
                let rec = base + 12 + 16 * i;
                (f.data.get(rec..rec + 4)? == tag).then(|| f.u32(rec + 8) as usize)
            })
        };
        let head = table(b"head")?;
        let hhea = table(b"hhea")?;
        let maxp = table(b"maxp")?;
        let cmap = table(b"cmap")?;
        let loca = table(b"loca")?;
        let glyf = table(b"glyf")?; // CFF-only fonts are rejected here
        let hmtx = table(b"hmtx")?;

        f.units_per_em = f.u16(head + 18).max(1) as f32;
        f.loca_long = f.i16(head + 50) != 0;
        f.ascent = f.i16(hhea + 4) as f32;
        f.descent = f.i16(hhea + 6) as f32;
        f.num_hmetrics = f.u16(hhea + 34).max(1);
        f.num_glyphs = f.u16(maxp + 4);
        f.loca = loca;
        f.glyf = glyf;
        f.hmtx = hmtx;

        // Pick the best Unicode cmap subtable: prefer full-repertoire format 12, else BMP format 4.
        let n = f.u16(cmap + 2) as usize;
        let mut best: Option<(u16, usize)> = None;
        for i in 0..n {
            let rec = cmap + 4 + 8 * i;
            let (pid, eid) = (f.u16(rec), f.u16(rec + 2));
            let sub = cmap + f.u32(rec + 4) as usize;
            let unicode = pid == 0 || (pid == 3 && (eid == 1 || eid == 10));
            if !unicode {
                continue;
            }
            let fmt = f.u16(sub);
            match (fmt, best) {
                (12, _) => best = Some((12, sub)),
                (4, None) => best = Some((4, sub)),
                _ => {}
            }
        }
        let (fmt, sub) = best?;
        f.cmap_format = fmt;
        f.cmap = sub;
        Some(f)
    }

    pub fn glyph_index(&self, c: char) -> u16 {
        let c = c as u32;
        let s = self.cmap;
        if self.cmap_format == 12 {
            let groups = self.u32(s + 12) as usize;
            let (mut lo, mut hi) = (0usize, groups);
            while lo < hi {
                let mid = (lo + hi) / 2;
                let g = s + 16 + 12 * mid;
                let (start, end) = (self.u32(g), self.u32(g + 4));
                if c < start {
                    hi = mid;
                } else if c > end {
                    lo = mid + 1;
                } else {
                    return (self.u32(g + 8) + (c - start)) as u16;
                }
            }
            return 0;
        }
        if c > 0xFFFF {
            return 0;
        }
        let seg_x2 = self.u16(s + 6) as usize;
        let ends = s + 14;
        let starts = ends + seg_x2 + 2;
        let deltas = starts + seg_x2;
        let ranges = deltas + seg_x2;
        for i in 0..seg_x2 / 2 {
            if c > self.u16(ends + 2 * i) as u32 {
                continue;
            }
            let start = self.u16(starts + 2 * i) as u32;
            if c < start {
                return 0;
            }
            let delta = self.u16(deltas + 2 * i) as u32;
            let ro = self.u16(ranges + 2 * i) as usize;
            if ro == 0 {
                return ((c + delta) & 0xFFFF) as u16;
            }
            let addr = ranges + 2 * i + ro + 2 * (c - start) as usize;
            let g = self.u16(addr) as u32;
            return if g == 0 { 0 } else { ((g + delta) & 0xFFFF) as u16 };
        }
        0
    }

    pub fn advance(&self, gid: u16) -> f32 {
        let i = gid.min(self.num_hmetrics - 1) as usize;
        self.u16(self.hmtx + 4 * i) as f32
    }

    pub fn scale(&self, px: f32) -> f32 {
        px / self.units_per_em
    }

    fn glyph_range(&self, gid: u16) -> Option<(usize, usize)> {
        if gid >= self.num_glyphs {
            return None;
        }
        let g = gid as usize;
        let (a, b) = if self.loca_long {
            (self.u32(self.loca + 4 * g) as usize, self.u32(self.loca + 4 * g + 4) as usize)
        } else {
            (self.u16(self.loca + 2 * g) as usize * 2, self.u16(self.loca + 2 * g + 2) as usize * 2)
        };
        (b > a).then_some((self.glyf + a, self.glyf + b))
    }

    /// Appends the contours of `gid` (transformed by the 2x3 matrix `m`) to `out`.
    fn outline(&self, gid: u16, m: [f32; 6], out: &mut Vec<Vec<Pt>>, depth: u32) {
        let Some((g, _)) = self.glyph_range(gid) else { return };
        if depth > 8 {
            return;
        }
        let n_contours = self.i16(g);
        if n_contours >= 0 {
            let nc = n_contours as usize;
            let ends: Vec<usize> = (0..nc).map(|i| self.u16(g + 10 + 2 * i) as usize).collect();
            let n_pts = ends.last().map_or(0, |e| e + 1);
            let mut p = g + 10 + 2 * nc;
            p += 2 + self.u16(p) as usize; // skip instructions
            let mut flags = Vec::with_capacity(n_pts);
            while flags.len() < n_pts {
                let fl = self.u8(p);
                p += 1;
                flags.push(fl);
                if fl & 8 != 0 {
                    let rep = self.u8(p);
                    p += 1;
                    for _ in 0..rep {
                        flags.push(fl);
                    }
                }
            }
            flags.truncate(n_pts);
            let mut read_coords = |short: u8, same: u8| -> Vec<f32> {
                let mut v = 0i32;
                flags
                    .iter()
                    .map(|&fl| {
                        if fl & short != 0 {
                            let d = self.u8(p) as i32;
                            p += 1;
                            v += if fl & same != 0 { d } else { -d };
                        } else if fl & same == 0 {
                            v += self.i16(p) as i32;
                            p += 2;
                        }
                        v as f32
                    })
                    .collect()
            };
            let xs = read_coords(2, 16);
            let ys = read_coords(4, 32);
            let mut start = 0;
            for &end in &ends {
                if end < start || end >= n_pts {
                    break;
                }
                out.push(
                    (start..=end)
                        .map(|i| Pt {
                            x: m[0] * xs[i] + m[2] * ys[i] + m[4],
                            y: m[1] * xs[i] + m[3] * ys[i] + m[5],
                            on: flags[i] & 1 != 0,
                        })
                        .collect(),
                );
                start = end + 1;
            }
        } else {
            let mut p = g + 10;
            loop {
                let fl = self.u16(p);
                let child = self.u16(p + 2);
                p += 4;
                let (dx, dy) = if fl & 1 != 0 {
                    let r = (self.i16(p) as f32, self.i16(p + 2) as f32);
                    p += 4;
                    r
                } else {
                    let r = (self.u8(p) as i8 as f32, self.u8(p + 1) as i8 as f32);
                    p += 2;
                    r
                };
                let (dx, dy) = if fl & 2 != 0 { (dx, dy) } else { (0.0, 0.0) };
                let f2 = |o: usize| self.i16(o) as f32 / 16384.0;
                let (mut a, mut b, mut c, mut d) = (1.0, 0.0, 0.0, 1.0);
                if fl & 8 != 0 {
                    a = f2(p);
                    d = a;
                    p += 2;
                } else if fl & 0x40 != 0 {
                    a = f2(p);
                    d = f2(p + 2);
                    p += 4;
                } else if fl & 0x80 != 0 {
                    a = f2(p);
                    b = f2(p + 2);
                    c = f2(p + 4);
                    d = f2(p + 6);
                    p += 8;
                }
                // child transform = parent * [a c dx; b d dy]
                let cm = [
                    m[0] * a + m[2] * b,
                    m[1] * a + m[3] * b,
                    m[0] * c + m[2] * d,
                    m[1] * c + m[3] * d,
                    m[0] * dx + m[2] * dy + m[4],
                    m[1] * dx + m[3] * dy + m[5],
                ];
                self.outline(child, cm, out, depth + 1);
                if fl & 0x20 == 0 {
                    break;
                }
            }
        }
    }

    /// Rasterizes a glyph at `px` pixels per em. Returns `None` for empty glyphs (e.g. space).
    pub fn rasterize(&self, gid: u16, px: f32) -> Option<Bitmap> {
        let s = self.scale(px);
        let mut contours = Vec::new();
        // y is flipped so the bitmap is top-down.
        self.outline(gid, [s, 0.0, 0.0, -s, 0.0, 0.0], &mut contours, 0);
        let pts = contours.iter().flatten();
        let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
        for p in pts {
            x0 = x0.min(p.x);
            y0 = y0.min(p.y);
            x1 = x1.max(p.x);
            y1 = y1.max(p.y);
        }
        if x0 >= x1 || y0 >= y1 {
            return None;
        }
        let (left, top) = (x0.floor(), y0.floor());
        let w = (x1.ceil() - left) as usize + 1;
        let h = (y1.ceil() - top) as usize + 1;
        let mut r = Raster::new(w, h);
        for c in &contours {
            for_each_line(c, |a, b| r.line((a.0 - left, a.1 - top), (b.0 - left, b.1 - top)));
        }
        Some(Bitmap { w, h, left, top, data: r.finish() })
    }
}

/// Walks a TrueType contour (quadratic B-splines with implied on-curve midpoints) as line segments.
fn for_each_line(c: &[Pt], mut emit: impl FnMut((f32, f32), (f32, f32))) {
    let n = c.len();
    if n < 2 {
        return;
    }
    let mid = |a: Pt, b: Pt| ((a.x + b.x) * 0.5, (a.y + b.y) * 0.5);
    let (start, order): ((f32, f32), Vec<usize>) = match c.iter().position(|p| p.on) {
        Some(i) => ((c[i].x, c[i].y), (1..n).map(|k| (i + k) % n).collect()),
        None => (mid(c[n - 1], c[0]), (0..n).collect()),
    };
    let mut quad = |p0: (f32, f32), p1: (f32, f32), p2: (f32, f32)| {
        let dd = ((p0.0 - 2.0 * p1.0 + p2.0).powi(2) + (p0.1 - 2.0 * p1.1 + p2.1).powi(2)).sqrt();
        let steps = (1.0 + (dd * 3.0).sqrt()).min(16.0) as usize;
        let mut prev = p0;
        for i in 1..=steps {
            let t = i as f32 / steps as f32;
            let mt = 1.0 - t;
            let p = (
                mt * mt * p0.0 + 2.0 * mt * t * p1.0 + t * t * p2.0,
                mt * mt * p0.1 + 2.0 * mt * t * p1.1 + t * t * p2.1,
            );
            emit(prev, p);
            prev = p;
        }
    };
    let mut cur = start;
    let mut ctrl: Option<(f32, f32)> = None;
    for &i in &order {
        let q = c[i];
        let qp = (q.x, q.y);
        if q.on {
            match ctrl.take() {
                Some(k) => quad(cur, k, qp),
                None => emit_line(&mut quad, cur, qp),
            }
            cur = qp;
        } else {
            if let Some(k) = ctrl {
                let m = ((k.0 + q.x) * 0.5, (k.1 + q.y) * 0.5);
                quad(cur, k, m);
                cur = m;
            }
            ctrl = Some(qp);
        }
    }
    match ctrl {
        Some(k) => quad(cur, k, start),
        None => emit_line(&mut quad, cur, start),
    }
}

/// A straight line is a degenerate quadratic; routing it through `quad` keeps one emitter.
fn emit_line(quad: &mut impl FnMut((f32, f32), (f32, f32), (f32, f32)), a: (f32, f32), b: (f32, f32)) {
    quad(a, ((a.0 + b.0) * 0.5, (a.1 + b.1) * 0.5), b);
}

pub struct Bitmap {
    pub w: usize,
    pub h: usize,
    pub left: f32,
    pub top: f32,
    pub data: Vec<u8>,
}

/// Signed-area accumulation rasterizer (the approach popularized by font-rs): each edge deposits
/// exact coverage deltas, and a running prefix sum turns them into per-pixel coverage.
struct Raster {
    w: usize,
    h: usize,
    a: Vec<f32>,
}

impl Raster {
    fn new(w: usize, h: usize) -> Self {
        Raster { w, h, a: vec![0.0; w * h + 4] }
    }

    fn line(&mut self, p0: (f32, f32), p1: (f32, f32)) {
        if (p0.1 - p1.1).abs() <= f32::EPSILON {
            return;
        }
        let (dir, p0, p1) = if p0.1 < p1.1 { (1.0, p0, p1) } else { (-1.0, p1, p0) };
        let dxdy = (p1.0 - p0.0) / (p1.1 - p0.1);
        let mut x = p0.0;
        let y0 = p0.1.max(0.0) as usize;
        if p0.1 < 0.0 {
            x -= p0.1 * dxdy;
        }
        let y_end = self.h.min(p1.1.ceil() as usize);
        for y in y0..y_end {
            let ls = y * self.w;
            let dy = ((y + 1) as f32).min(p1.1) - (y as f32).max(p0.1);
            let xnext = x + dxdy * dy;
            let d = dy * dir;
            let (xa, xb) = if x < xnext { (x, xnext) } else { (xnext, x) };
            let xa_floor = xa.floor();
            let xai = xa_floor.max(0.0) as usize;
            let xb_ceil = xb.ceil();
            let xbi = xb_ceil.max(0.0) as usize;
            let a = &mut self.a;
            if xbi <= xai + 1 {
                let xmf = 0.5 * (x + xnext) - xa_floor;
                a[ls + xai] += d - d * xmf;
                a[ls + xai + 1] += d * xmf;
            } else {
                let s = (xb - xa).recip();
                let x0f = xa - xa_floor;
                let a0 = 0.5 * s * (1.0 - x0f) * (1.0 - x0f);
                let x1f = xb - xb_ceil + 1.0;
                let am = 0.5 * s * x1f * x1f;
                a[ls + xai] += d * a0;
                if xbi == xai + 2 {
                    a[ls + xai + 1] += d * (1.0 - a0 - am);
                } else {
                    let a1 = s * (1.5 - x0f);
                    a[ls + xai + 1] += d * (a1 - a0);
                    for xi in xai + 2..xbi - 1 {
                        a[ls + xi] += d * s;
                    }
                    let a2 = a1 + (xbi - xai - 3) as f32 * s;
                    a[ls + xbi - 1] += d * (1.0 - a2 - am);
                }
                a[ls + xbi] += d * am;
            }
            x = xnext;
        }
    }

    fn finish(self) -> Vec<u8> {
        let mut acc = 0.0f32;
        self.a[..self.w * self.h]
            .iter()
            .map(|v| {
                acc += v;
                // A gentle gamma lift keeps light-on-dark text from looking anemic.
                let c = acc.abs().min(1.0).powf(0.85);
                (c * 255.0 + 0.5) as u8
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------------------------
// Atlas + font set
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
pub struct Glyph {
    /// u0, v0, u1, v1
    pub uv: [f32; 4],
    /// All metrics are in physical pixels.
    pub w: f32,
    pub h: f32,
    pub left: f32,
    pub top: f32,
}

pub struct Atlas {
    pub size: u32,
    pub data: Vec<u8>,
    pub dirty: bool,
    x: u32,
    y: u32,
    row_h: u32,
}

impl Atlas {
    fn new(size: u32) -> Self {
        Atlas { size, data: vec![0; (size * size) as usize], dirty: true, x: 1, y: 1, row_h: 0 }
    }

    fn alloc(&mut self, w: u32, h: u32) -> Option<(u32, u32)> {
        if self.x + w + 1 > self.size {
            self.x = 1;
            self.y += self.row_h + 1;
            self.row_h = 0;
        }
        if self.y + h + 1 > self.size || w + 2 > self.size {
            return None;
        }
        let pos = (self.x, self.y);
        self.x += w + 1;
        self.row_h = self.row_h.max(h);
        Some(pos)
    }

    fn clear(&mut self) {
        self.data.fill(0);
        self.x = 1;
        self.y = 1;
        self.row_h = 0;
        self.dirty = true;
    }
}

pub struct Fonts {
    pub faces: Vec<Font>,
    pub atlas: Atlas,
    chars: HashMap<(FontId, char), (usize, u16)>,
    glyphs: HashMap<(usize, u16, u32), Option<Glyph>>,
}

const UI_CANDIDATES: &[&str] = &[
    "/System/Library/Fonts/SFNS.ttf",
    "/System/Library/Fonts/Helvetica.ttc",
    "/System/Library/Fonts/Geneva.ttf",
    "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
    "/usr/share/fonts/TTF/DejaVuSans.ttf",
    "/usr/share/fonts/dejavu/DejaVuSans.ttf",
    "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
    "/usr/share/fonts/noto/NotoSans-Regular.ttf",
    "C:\\Windows\\Fonts\\segoeui.ttf",
    "C:\\Windows\\Fonts\\arial.ttf",
];

const MONO_CANDIDATES: &[&str] = &[
    "/System/Library/Fonts/SFNSMono.ttf",
    "/System/Library/Fonts/Menlo.ttc",
    "/System/Library/Fonts/Monaco.ttf",
    "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
    "/usr/share/fonts/TTF/DejaVuSansMono.ttf",
    "/usr/share/fonts/dejavu/DejaVuSansMono.ttf",
    "/usr/share/fonts/truetype/liberation/LiberationMono-Regular.ttf",
    "C:\\Windows\\Fonts\\consola.ttf",
    "C:\\Windows\\Fonts\\cour.ttf",
];

/// Extra faces consulted for characters missing from the primary fonts (arrows, symbols, CJK...).
const FALLBACK_CANDIDATES: &[&str] = &[
    "/System/Library/Fonts/Apple Symbols.ttf",
    "/System/Library/Fonts/Hiragino Sans GB.ttc",
    "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
    "C:\\Windows\\Fonts\\seguisym.ttf",
];

fn load_first(env: &str, list: &[&str]) -> Option<Font> {
    let env_path = std::env::var(env).ok();
    env_path
        .iter()
        .map(String::as_str)
        .chain(list.iter().copied())
        .find_map(|p| Font::parse(std::fs::read(p).ok()?))
}

impl Fonts {
    pub fn load() -> Result<Fonts, String> {
        let ui = load_first("WGITPUI_UI_FONT", UI_CANDIDATES);
        let mono = load_first("WGITPUI_MONO_FONT", MONO_CANDIDATES);
        let (ui, mono) = match (ui, mono) {
            (Some(u), Some(m)) => (u, m),
            (Some(u), None) => (u, load_first("", UI_CANDIDATES).unwrap()),
            (None, Some(m)) => (load_first("", MONO_CANDIDATES).unwrap(), m),
            (None, None) => {
                return Err("no usable TrueType font found; set WGITPUI_UI_FONT / WGITPUI_MONO_FONT \
                            to a .ttf file with glyf outlines"
                    .into());
            }
        };
        let mut faces = vec![ui, mono];
        faces.extend(FALLBACK_CANDIDATES.iter().filter_map(|p| Font::parse(std::fs::read(p).ok()?)));
        Ok(Fonts { faces, atlas: Atlas::new(2048), chars: HashMap::new(), glyphs: HashMap::new() })
    }

    /// Resolves a char to (face, glyph id), falling back through other faces when missing.
    fn resolve(&mut self, font: FontId, c: char) -> (usize, u16) {
        if let Some(&r) = self.chars.get(&(font, c)) {
            return r;
        }
        let order = std::iter::once(font).chain((0..self.faces.len()).filter(|&i| i != font));
        let r = order
            .map(|i| (i, self.faces[i].glyph_index(c)))
            .find(|&(_, g)| g != 0)
            .unwrap_or((font, 0));
        self.chars.insert((font, c), r);
        r
    }

    /// Advance width in physical pixels for `c` at `px` pixels per em.
    pub fn advance(&mut self, font: FontId, c: char, px: f32) -> f32 {
        let (face, gid) = self.resolve(font, c);
        let f = &self.faces[face];
        f.advance(gid) * f.scale(px)
    }


    pub fn glyph(&mut self, font: FontId, c: char, px: f32) -> Option<Glyph> {
        let (face, gid) = self.resolve(font, c);
        let key = (face, gid, (px * 4.0).round() as u32);
        if let Some(g) = self.glyphs.get(&key) {
            return *g;
        }
        let g = self.faces[face].rasterize(gid, px).and_then(|bm| {
            let (w, h) = (bm.w as u32, bm.h as u32);
            let (x, y) = match self.atlas.alloc(w, h) {
                Some(p) => p,
                None => {
                    // Atlas full: start over. Glyphs re-rasterize lazily on the next frame.
                    self.atlas.clear();
                    self.glyphs.clear();
                    self.atlas.alloc(w, h)?
                }
            };
            let size = self.atlas.size as usize;
            for row in 0..bm.h {
                let dst = (y as usize + row) * size + x as usize;
                self.atlas.data[dst..dst + bm.w].copy_from_slice(&bm.data[row * bm.w..(row + 1) * bm.w]);
            }
            self.atlas.dirty = true;
            let s = self.atlas.size as f32;
            Some(Glyph {
                uv: [x as f32 / s, y as f32 / s, (x + w) as f32 / s, (y + h) as f32 / s],
                w: w as f32,
                h: h as f32,
                left: bm.left,
                top: bm.top,
            })
        });
        self.glyphs.insert(key, g);
        g
    }
}
