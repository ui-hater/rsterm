//! Font loading (via fontconfig's `fc-match`), glyph rasterization (fontdue)
//! and a cell-slot glyph atlas.

use fontdue::{Font, FontSettings};
use std::collections::HashMap;
use std::process::Command;

pub const ATLAS_SIZE: u32 = 2048;

pub const STYLE_BOLD: u32 = 1;
pub const STYLE_ITALIC: u32 = 2;

fn fc_match(pattern: &str) -> Option<(String, u32)> {
    let out = Command::new("fc-match").args(["-f", "%{file}\n%{index}", pattern]).output().ok()?;
    let s = String::from_utf8(out.stdout).ok()?;
    let mut lines = s.lines();
    let file = lines.next()?.to_string();
    let index = lines.next().and_then(|i| i.trim().parse().ok()).unwrap_or(0);
    if file.is_empty() { None } else { Some((file, index)) }
}

fn load_font(path: &str, index: u32, px: f32) -> Option<Font> {
    let data = std::fs::read(path).ok()?;
    Font::from_bytes(data, FontSettings { collection_index: index, scale: px, ..Default::default() })
        .ok()
}

pub struct Glyphs {
    family: String,
    /// regular, bold, italic, bold-italic (None = synthesize from regular).
    fonts: [Option<Font>; 4],
    fallbacks: Vec<(String, Font)>,
    fallback_tried: HashMap<String, ()>,
    pub px: f32,
    pub cell_w: u32,
    pub cell_h: u32,
    baseline: i32,
    pub underline_pos: u32,
    pub underline_thick: u32,
    pub strike_pos: u32,

    ascii: Box<[u32; 512]>,
    cache: HashMap<u32, u32>,
    next_x: u32,
    next_y: u32,

    /// CPU copy of the atlas (R8).
    pub atlas: Vec<u8>,
    /// Dirty row range of `atlas` that still needs uploading.
    pub dirty: Option<(u32, u32)>,
    /// Bumped whenever the atlas is wiped and all slots are invalidated.
    pub epoch: u64,
}

const NONE: u32 = u32::MAX;

impl Glyphs {
    pub fn new(family: &str, px: f32) -> Glyphs {
        let load = |style: &str| {
            fc_match(&format!("{family}:{style}")).and_then(|(p, i)| load_font(&p, i, px).map(|f| (p, f)))
        };
        let regular = load("style=Regular").or_else(|| load("")).expect("no usable font found");
        let mut fonts: [Option<Font>; 4] = [None, None, None, None];
        for (i, style) in [(1, "weight=bold"), (2, "slant=italic"), (3, "weight=bold:slant=italic")] {
            if let Some((p, f)) = load(style) {
                if p != regular.0 {
                    fonts[i] = Some(f);
                }
            }
        }
        fonts[0] = Some(regular.1);
        let mut g = Glyphs {
            family: family.to_string(),
            fonts,
            fallbacks: Vec::new(),
            fallback_tried: HashMap::new(),
            px,
            cell_w: 1,
            cell_h: 1,
            baseline: 0,
            underline_pos: 0,
            underline_thick: 1,
            strike_pos: 0,
            ascii: Box::new([NONE; 512]),
            cache: HashMap::new(),
            next_x: 0,
            next_y: 0,
            atlas: vec![0; (ATLAS_SIZE * ATLAS_SIZE) as usize],
            dirty: None,
            epoch: 0,
        };
        g.compute_metrics();
        g.clear_atlas();
        g
    }

    fn regular(&self) -> &Font {
        self.fonts[0].as_ref().unwrap()
    }

    fn compute_metrics(&mut self) {
        let f = self.regular();
        let lm = f.horizontal_line_metrics(self.px).expect("font has no horizontal metrics");
        let adv = f.metrics('M', self.px).advance_width;
        let xh = f.metrics('x', self.px).height as i32;
        let cell_w = adv.round().max(1.0) as u32;
        let cell_h = (lm.ascent - lm.descent + lm.line_gap).ceil().max(1.0) as u32;
        let baseline = (lm.line_gap / 2.0 + lm.ascent).round() as i32;
        self.cell_w = cell_w;
        self.cell_h = cell_h;
        self.baseline = baseline;
        self.underline_thick = (self.px / 14.0).round().max(1.0) as u32;
        self.underline_pos = ((baseline + 2) as u32).min(cell_h - self.underline_thick);
        self.strike_pos = (baseline - xh / 2).max(0) as u32;
    }

    pub fn clear_atlas(&mut self) {
        self.atlas.fill(0);
        self.ascii.fill(NONE);
        self.cache.clear();
        // Slot (0,0) is the permanently empty glyph.
        self.next_x = self.cell_w * 2;
        self.next_y = 0;
        self.dirty = Some((0, ATLAS_SIZE));
        self.epoch += 1;
    }

    /// Atlas position (x | y << 16) of the glyph for `c` with `style`.
    #[inline]
    pub fn get(&mut self, c: char, style: u32) -> u32 {
        let cu = c as u32;
        if cu < 128 {
            let i = (cu | style << 7) as usize;
            let v = self.ascii[i];
            if v != NONE {
                return v;
            }
            let v = self.rasterize(c, style);
            self.ascii[i] = v;
            return v;
        }
        let key = cu | style << 21;
        if let Some(&v) = self.cache.get(&key) {
            return v;
        }
        let v = self.rasterize(c, style);
        self.cache.insert(key, v);
        v
    }

    fn alloc(&mut self, w: u32) -> Option<(u32, u32)> {
        if self.next_x + w > ATLAS_SIZE {
            self.next_x = 0;
            self.next_y += self.cell_h;
        }
        if self.next_y + self.cell_h > ATLAS_SIZE {
            return None;
        }
        let pos = (self.next_x, self.next_y);
        self.next_x += w;
        Some(pos)
    }

    fn mark_dirty(&mut self, y0: u32, y1: u32) {
        self.dirty = Some(match self.dirty {
            Some((a, b)) => (a.min(y0), b.max(y1)),
            None => (y0, y1),
        });
    }

    fn font_for(&mut self, c: char, style: u32) -> Option<(&Font, u16, bool)> {
        // Returns (font, glyph index, synthesize-bold)
        let s = style as usize;
        if let Some(f) = &self.fonts[s] {
            let idx = f.lookup_glyph_index(c);
            if idx != 0 {
                return Some((self.fonts[s].as_ref().unwrap(), idx, false));
            }
        }
        let synth_bold = style & STYLE_BOLD != 0 && self.fonts[s].is_none();
        let idx = self.regular().lookup_glyph_index(c);
        if idx != 0 {
            return Some((self.regular(), idx, synth_bold));
        }
        // Already-loaded fallback fonts.
        if let Some(i) = self.fallbacks.iter().position(|(_, f)| f.lookup_glyph_index(c) != 0) {
            let f = &self.fallbacks[i].1;
            return Some((f, f.lookup_glyph_index(c), synth_bold));
        }
        // Ask fontconfig for a font covering this codepoint.
        let (path, index) = fc_match(&format!("{}:charset={:x}", self.family, c as u32))?;
        if self.fallback_tried.contains_key(&path) {
            return None;
        }
        self.fallback_tried.insert(path.clone(), ());
        let font = load_font(&path, index, self.px)?;
        let idx = font.lookup_glyph_index(c);
        if idx == 0 {
            return None;
        }
        self.fallbacks.push((path, font));
        Some((&self.fallbacks.last().unwrap().1, idx, synth_bold))
    }

    fn rasterize(&mut self, c: char, style: u32) -> u32 {
        if c == ' ' || c == '\0' {
            return 0;
        }
        let wide = unicode_width::UnicodeWidthChar::width(c) == Some(2);
        let slot_w = self.cell_w * if wide { 2 } else { 1 };
        let (cw, ch) = (slot_w, self.cell_h);

        let mut bitmap = vec![0u8; (cw * ch) as usize];
        if !draw_builtin(c, cw as usize, ch as usize, &mut bitmap) {
            let px = self.px;
            let baseline = self.baseline;
            let Some((font, idx, synth_bold)) = self.font_for(c, style) else { return 0 };
            let (m, bmp) = font.rasterize_indexed(idx, px);
            let x_off = m.xmin.max(0).min(cw as i32);
            let y_top = baseline - (m.ymin + m.height as i32);
            for gy in 0..m.height as i32 {
                let ty = y_top + gy;
                if ty < 0 || ty >= ch as i32 {
                    continue;
                }
                for gx in 0..m.width as i32 {
                    let tx = x_off + gx;
                    if tx >= cw as i32 {
                        break;
                    }
                    let v = bmp[(gy * m.width as i32 + gx) as usize];
                    let dst = &mut bitmap[(ty as u32 * cw + tx as u32) as usize];
                    *dst = (*dst).max(v);
                    if synth_bold && tx + 1 < cw as i32 {
                        let d2 = &mut bitmap[(ty as u32 * cw + tx as u32 + 1) as usize];
                        *d2 = (*d2).max(v);
                    }
                }
            }
        }

        let Some((x, y)) = self.alloc(slot_w) else {
            // Atlas full: wipe it. Callers detect this via `epoch`.
            self.clear_atlas();
            return self.rasterize(c, style);
        };
        for row in 0..ch {
            let dst = ((y + row) * ATLAS_SIZE + x) as usize;
            let src = (row * cw) as usize;
            self.atlas[dst..dst + cw as usize].copy_from_slice(&bitmap[src..src + cw as usize]);
        }
        self.mark_dirty(y, y + ch);
        x | y << 16
    }
}

/// Pixel-perfect box drawing and block elements so lines connect seamlessly.
fn draw_builtin(c: char, w: usize, h: usize, out: &mut [u8]) -> bool {
    let mut fill = |x0: usize, y0: usize, x1: usize, y1: usize, v: u8| {
        for y in y0.min(h)..y1.min(h) {
            for x in x0.min(w)..x1.min(w) {
                out[y * w + x] = v;
            }
        }
    };
    let cu = c as u32;
    match cu {
        // Block elements
        0x2580 => fill(0, 0, w, h / 2, 255),
        0x2581..=0x2588 => {
            let n = (cu - 0x2580) as usize;
            fill(0, h - h * n / 8, w, h, 255)
        }
        0x2589..=0x258F => {
            let n = (0x2590 - cu) as usize;
            fill(0, 0, w * n / 8, h, 255)
        }
        0x2590 => fill(w / 2, 0, w, h, 255),
        0x2591 => fill(0, 0, w, h, 64),
        0x2592 => fill(0, 0, w, h, 128),
        0x2593 => fill(0, 0, w, h, 192),
        0x2594 => fill(0, 0, w, h / 8, 255),
        0x2595 => fill(w - w / 8, 0, w, h, 255),
        0x2500..=0x257F => {
            // (up, down, left, right) weights: 0 none, 1 light, 2 heavy
            let Some([u, d, l, r]) = box_weights(cu) else { return false };
            let light = (h / 18).max(1);
            let thick = |wt: u8| if wt == 2 { light * 2 + 1 } else { light };
            let cx = w / 2;
            let cy = h / 2;
            let hw = |wt: u8| thick(wt);
            if l > 0 {
                let t = hw(l);
                fill(0, cy - t / 2, cx + hw(u.max(d)).div_ceil(2), cy - t / 2 + t, 255);
            }
            if r > 0 {
                let t = hw(r);
                fill(cx - hw(u.max(d)) / 2, cy - t / 2, w, cy - t / 2 + t, 255);
            }
            if u > 0 {
                let t = hw(u);
                fill(cx - t / 2, 0, cx - t / 2 + t, cy + hw(l.max(r)).div_ceil(2), 255);
            }
            if d > 0 {
                let t = hw(d);
                fill(cx - t / 2, cy - hw(l.max(r)) / 2, cx - t / 2 + t, h, 255);
            }
        }
        _ => return false,
    }
    true
}

fn box_weights(c: u32) -> Option<[u8; 4]> {
    // [up, down, left, right]
    Some(match c {
        0x2500 => [0, 0, 1, 1],
        0x2501 => [0, 0, 2, 2],
        0x2502 => [1, 1, 0, 0],
        0x2503 => [2, 2, 0, 0],
        0x250C | 0x256D => [0, 1, 0, 1],
        0x250D => [0, 1, 0, 2],
        0x250E => [0, 2, 0, 1],
        0x250F => [0, 2, 0, 2],
        0x2510 | 0x256E => [0, 1, 1, 0],
        0x2511 => [0, 1, 2, 0],
        0x2512 => [0, 2, 1, 0],
        0x2513 => [0, 2, 2, 0],
        0x2514 | 0x2570 => [1, 0, 0, 1],
        0x2515 => [1, 0, 0, 2],
        0x2516 => [2, 0, 0, 1],
        0x2517 => [2, 0, 0, 2],
        0x2518 | 0x256F => [1, 0, 1, 0],
        0x2519 => [1, 0, 2, 0],
        0x251A => [2, 0, 1, 0],
        0x251B => [2, 0, 2, 0],
        0x251C => [1, 1, 0, 1],
        0x251D => [1, 1, 0, 2],
        0x2520 => [2, 2, 0, 1],
        0x2523 => [2, 2, 0, 2],
        0x2524 => [1, 1, 1, 0],
        0x2525 => [1, 1, 2, 0],
        0x2528 => [2, 2, 1, 0],
        0x252B => [2, 2, 2, 0],
        0x252C => [0, 1, 1, 1],
        0x252F => [0, 1, 2, 2],
        0x2530 => [0, 2, 1, 1],
        0x2533 => [0, 2, 2, 2],
        0x2534 => [1, 0, 1, 1],
        0x2537 => [1, 0, 2, 2],
        0x2538 => [2, 0, 1, 1],
        0x253B => [2, 0, 2, 2],
        0x253C => [1, 1, 1, 1],
        0x253F => [1, 1, 2, 2],
        0x2542 => [2, 2, 1, 1],
        0x254B => [2, 2, 2, 2],
        0x2574 => [0, 0, 1, 0],
        0x2575 => [1, 0, 0, 0],
        0x2576 => [0, 0, 0, 1],
        0x2577 => [0, 1, 0, 0],
        0x2578 => [0, 0, 2, 0],
        0x2579 => [2, 0, 0, 0],
        0x257A => [0, 0, 0, 2],
        0x257B => [0, 2, 0, 0],
        0x257C => [0, 0, 1, 2],
        0x257D => [1, 2, 0, 0],
        0x257E => [0, 0, 2, 1],
        0x257F => [2, 1, 0, 0],
        _ => return None,
    })
}
