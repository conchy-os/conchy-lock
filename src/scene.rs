//! Everything drawn on top of the wallpaper: clock, date, avatar, greeting,
//! password pill. Pure drawing + animation math; no Wayland in here.
//!
//! Layout is expressed in logical px relative to the screen centre, multiplied
//! by `scale` at draw time. The scene only repaints a centred region, see
//! [`REGION_W`] / [`REGION_H`].

use std::{collections::HashMap, f32::consts::PI, rc::Rc, time::Instant};

use cosmic_text::{Attrs, Buffer, Color as CtColor, Family, FontSystem, Metrics, Shaping, SwashCache, Weight};
use tiny_skia::{
    FillRule, Paint, PathBuilder, Pixmap, PixmapPaint, Stroke, StrokeDash, Transform, LineCap,
};

use crate::theme::{Palette, Rgb};

/// Logical size of the repainted region, centred on the screen.
pub const REGION_W: f32 = 640.0;
pub const REGION_H: f32 = 560.0;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Phase {
    Idle,
    Checking,
    Failed,
    Success,
}

#[derive(Clone)]
pub struct UiState {
    pub time: String,
    pub date: String,
    pub len: usize,
    pub phase: Phase,
    pub caps: bool,
    pub attempts: u32,
    pub message: Option<String>,
    pub started: Instant,
    pub phase_since: Instant,
    pub last_key: Instant,
}

// ---------------------------------------------------------------- text ----

#[derive(Hash, PartialEq, Eq)]
struct TextKey {
    text: String,
    px: u32,
    weight: u16,
    color: [u8; 3],
}

struct Text {
    fs: FontSystem,
    swash: SwashCache,
    cache: HashMap<TextKey, Rc<Pixmap>>,
}

impl Text {
    fn new() -> Self {
        Self { fs: FontSystem::new(), swash: SwashCache::new(), cache: HashMap::new() }
    }

    fn get(&mut self, text: &str, px: f32, weight: u16, color: Rgb) -> Rc<Pixmap> {
        let key = TextKey {
            text: text.to_owned(),
            px: px.round() as u32,
            weight,
            color: [color.0, color.1, color.2],
        };
        if let Some(p) = self.cache.get(&key) {
            return p.clone();
        }
        if self.cache.len() > 64 {
            self.cache.clear();
        }

        let line_h = (px * 1.3).ceil();
        let mut buf = Buffer::new(&mut self.fs, Metrics::new(px, line_h));
        buf.set_size(&mut self.fs, None, None);
        let attrs = Attrs::new().family(Family::Name("JetBrainsMono Nerd Font")).weight(Weight(weight));
        buf.set_text(&mut self.fs, text, &attrs, Shaping::Advanced);
        buf.shape_until_scroll(&mut self.fs, false);
        let width = buf.layout_runs().map(|r| r.line_w).fold(0.0_f32, f32::max);

        let pad = 4;
        let (w, h) = (width.ceil() as u32 + pad * 2, line_h as u32);
        let mut pm = Pixmap::new(w.max(1), h.max(1)).unwrap();
        let stride = pm.width() as i32;
        let rows = pm.height() as i32;
        let data = pm.data_mut();
        buf.draw(
            &mut self.fs,
            &mut self.swash,
            CtColor::rgba(color.0, color.1, color.2, 255),
            |x, y, gw, gh, c| {
                let a = c.a() as u32;
                if a == 0 {
                    return;
                }
                for yy in y..y + gh as i32 {
                    for xx in x..x + gw as i32 {
                        let xx = xx + pad as i32;
                        if xx < 0 || yy < 0 || xx >= stride || yy >= rows {
                            continue;
                        }
                        let i = ((yy * stride + xx) * 4) as usize;
                        let inv = 255 - a;
                        // source-over on premultiplied pixels
                        data[i] = ((c.r() as u32 * a + data[i] as u32 * inv) / 255) as u8;
                        data[i + 1] = ((c.g() as u32 * a + data[i + 1] as u32 * inv) / 255) as u8;
                        data[i + 2] = ((c.b() as u32 * a + data[i + 2] as u32 * inv) / 255) as u8;
                        data[i + 3] = (a + data[i + 3] as u32 * inv / 255) as u8;
                    }
                }
            },
        );
        let pm = Rc::new(pm);
        self.cache.insert(key, pm.clone());
        pm
    }
}

// --------------------------------------------------------------- helpers ---

fn ease_out_cubic(t: f32) -> f32 {
    1.0 - (1.0 - t.clamp(0.0, 1.0)).powi(3)
}

/// Overshooting pop: 0 -> 1.15 -> 1.
fn ease_out_back(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    let c1 = 1.70158;
    let c3 = c1 + 1.0;
    1.0 + c3 * (t - 1.0).powi(3) + c1 * (t - 1.0).powi(2)
}

fn round_rect(x: f32, y: f32, w: f32, h: f32, r: f32) -> tiny_skia::Path {
    let r = r.min(w / 2.0).min(h / 2.0);
    let k = 0.552_284_75 * r;
    let mut pb = PathBuilder::new();
    pb.move_to(x + r, y);
    pb.line_to(x + w - r, y);
    pb.cubic_to(x + w - r + k, y, x + w, y + r - k, x + w, y + r);
    pb.line_to(x + w, y + h - r);
    pb.cubic_to(x + w, y + h - r + k, x + w - r + k, y + h, x + w - r, y + h);
    pb.line_to(x + r, y + h);
    pb.cubic_to(x + r - k, y + h, x, y + h - r + k, x, y + h - r);
    pb.line_to(x, y + r);
    pb.cubic_to(x, y + r - k, x + r - k, y, x + r, y);
    pb.close();
    pb.finish().unwrap()
}

fn circle(cx: f32, cy: f32, r: f32) -> tiny_skia::Path {
    PathBuilder::from_circle(cx, cy, r).unwrap()
}

fn paint(color: tiny_skia::Color) -> Paint<'static> {
    let mut p = Paint::default();
    p.set_color(color);
    p.anti_alias = true;
    p
}

// ----------------------------------------------------------------- scene ---

pub struct Scene {
    text: Text,
    pal: Palette,
    user: String,
    avatar: Option<Pixmap>,
}

impl Scene {
    pub fn new(pal: Palette, user: String) -> Self {
        Self { text: Text::new(), pal, user, avatar: load_avatar() }
    }

    /// Draws into `layer` (transparent, sized to the region). Returns `true`
    /// while something is still animating and another frame is needed.
    pub fn draw(&mut self, layer: &mut Pixmap, st: &UiState, scale: f32, now: Instant) -> bool {
        let s = scale;
        let cx = layer.width() as f32 / 2.0;
        let cy = layer.height() as f32 / 2.0;
        let pal = self.pal.clone();
        let mut animating = false;

        let since_phase = now.duration_since(st.phase_since).as_secs_f32();

        // ---- clock
        let clock = self.text.get(&st.time, 124.0 * s, 800, pal.text);
        self.blit_center(layer, &clock, cx, cy - 172.0 * s, 1.0);

        // ---- date
        let date = self.text.get(&st.date, 21.0 * s, 500, pal.subtext);
        self.blit_center(layer, &date, cx, cy - 92.0 * s, 0.85);

        // ---- avatar
        self.draw_avatar(layer, cx, cy - 12.0 * s, 31.0 * s, &pal, s);

        // ---- greeting: "Welcome back, <user>"
        let g1 = self.text.get("Welcome back, ", 17.0 * s, 400, pal.subtext);
        let g2 = self.text.get(&self.user.clone(), 17.0 * s, 700, pal.accent.lerp(Rgb(255, 255, 255), 0.45));
        let total = (g1.width() + g2.width()) as f32 - 8.0 * s;
        let gy = cy + 46.0 * s;
        let gx = cx - total / 2.0;
        self.blit_left_mid(layer, &g1, gx, gy, 0.9);
        self.blit_left_mid(layer, &g2, gx + g1.width() as f32 - 8.0 * s, gy, 1.0);

        // ---- password pill
        let (pw, ph) = (344.0 * s, 56.0 * s);
        let mut px = cx - pw / 2.0;
        let py = cy + 80.0 * s;

        // shake on failure: 3 damped oscillations over 0.5s
        if st.phase == Phase::Failed && since_phase < 0.5 {
            let k = since_phase / 0.5;
            px += (k * 3.0 * 2.0 * PI).sin() * (1.0 - k) * 14.0 * s;
            animating = true;
        }

        let ring = match st.phase {
            Phase::Idle if st.len > 0 => pal.accent,
            Phase::Idle => pal.text,
            Phase::Checking => pal.accent,
            Phase::Success => pal.success,
            Phase::Failed => {
                let k = ((since_phase - 0.9) / 0.6).clamp(0.0, 1.0);
                if k < 1.0 {
                    animating = true;
                }
                pal.danger.lerp(if st.len > 0 { pal.accent } else { pal.text }, k)
            }
        };
        let ring_alpha = if st.phase == Phase::Idle && st.len == 0 { 0.22 } else { 0.95 };

        // soft shadow
        for i in 0..6 {
            let grow = i as f32 * 3.0 * s;
            let p = round_rect(px - grow, py - grow + 8.0 * s, pw + 2.0 * grow, ph + 2.0 * grow, ph / 2.0 + grow);
            layer.fill_path(&p, &paint(Rgb(0, 0, 0).skia(0.045)), FillRule::Winding, Transform::identity(), None);
        }
        let body = round_rect(px, py, pw, ph, ph / 2.0);
        layer.fill_path(&body, &paint(pal.base.skia(0.42)), FillRule::Winding, Transform::identity(), None);
        layer.fill_path(&body, &paint(Rgb(255, 255, 255).skia(0.07)), FillRule::Winding, Transform::identity(), None);
        // glow + ring
        let mut stroke = Stroke { width: 7.0 * s, ..Default::default() };
        layer.stroke_path(&body, &paint(ring.skia(ring_alpha * 0.16)), &stroke, Transform::identity(), None);
        stroke.width = 1.8 * s;
        layer.stroke_path(&body, &paint(ring.skia(ring_alpha)), &stroke, Transform::identity(), None);

        // placeholder / dots
        let mid_y = py + ph / 2.0;
        if st.len == 0 && st.phase != Phase::Checking {
            let ph_txt = self.text.get("Enter password", 16.0 * s, 400, pal.subtext);
            self.blit_center(layer, &ph_txt, px + pw / 2.0, mid_y, 0.6);
        } else {
            let spacing = 19.0 * s;
            let max_visible = ((pw - 90.0 * s) / spacing).floor().max(1.0) as usize;
            let n = st.len.min(max_visible);
            let start_x = px + pw / 2.0 - (n.saturating_sub(1)) as f32 * spacing / 2.0;
            let dim = if st.phase == Phase::Checking { 0.5 } else { 1.0 };
            let age = now.duration_since(st.last_key).as_secs_f32();
            for i in 0..n {
                let mut r = 5.2 * s;
                if i + 1 == n && age < 0.18 {
                    r *= ease_out_back(age / 0.18);
                    animating = true;
                }
                let color = if st.phase == Phase::Failed { pal.danger } else { pal.text };
                layer.fill_path(
                    &circle(start_x + i as f32 * spacing, mid_y, r.max(0.1)),
                    &paint(color.skia(0.95 * dim)),
                    FillRule::Winding,
                    Transform::identity(),
                    None,
                );
            }
        }

        // spinner while PAM thinks
        if st.phase == Phase::Checking {
            animating = true;
            let t = now.duration_since(st.phase_since).as_secs_f32();
            let (sx, sr) = (px + pw - 30.0 * s, 9.0 * s);
            let start = t * 2.0 * PI * 1.3;
            let mut pb = PathBuilder::new();
            for i in 0..=24 {
                let a = start + (i as f32 / 24.0) * PI * 1.35;
                let (x, y) = (sx + a.cos() * sr, mid_y + a.sin() * sr);
                if i == 0 {
                    pb.move_to(x, y);
                } else {
                    pb.line_to(x, y);
                }
            }
            if let Some(path) = pb.finish() {
                let st = Stroke { width: 2.4 * s, line_cap: LineCap::Round, ..Default::default() };
                layer.stroke_path(&path, &paint(pal.accent.skia(1.0)), &st, Transform::identity(), None);
            }
        }
        let _ = StrokeDash::new; // keep import used if dashes are added later

        // ---- status line under the pill
        let (msg, color) = if let Some(m) = &st.message {
            (Some(m.as_str()), pal.danger)
        } else if st.caps {
            (Some("Caps Lock is on"), pal.warn)
        } else {
            (None, pal.text)
        };
        if let Some(m) = msg {
            let mut alpha = 1.0;
            if st.phase == Phase::Failed && st.message.is_some() {
                // fade the error out after 2.5s
                alpha = (1.0 - (since_phase - 2.5)).clamp(0.0, 1.0);
                if alpha > 0.0 {
                    animating = true;
                }
            }
            if alpha > 0.0 {
                let color = color.lerp(Rgb(255, 255, 255), 0.35);
                let t = self.text.get(m, 14.5 * s, 600, color);
                let (cw, ch) = (t.width() as f32 + 8.0 * s, 30.0 * s);
                let (chip_x, chip_y) = (cx - cw / 2.0, py + ph + 28.0 * s - ch / 2.0);
                let chip = round_rect(chip_x, chip_y, cw, ch, ch / 2.0);
                layer.fill_path(&chip, &paint(pal.base.lerp(Rgb(0, 0, 0), 0.4).skia(0.62 * alpha)), FillRule::Winding, Transform::identity(), None);
                self.blit_center(layer, &t, cx, py + ph + 28.0 * s, alpha);
            }
        }

        animating
    }

    fn blit_center(&self, layer: &mut Pixmap, pm: &Pixmap, cx: f32, cy: f32, opacity: f32) {
        let x = cx - pm.width() as f32 / 2.0;
        let y = cy - pm.height() as f32 / 2.0;
        self.blit(layer, pm, x, y, opacity);
    }

    fn blit_left_mid(&self, layer: &mut Pixmap, pm: &Pixmap, x: f32, cy: f32, opacity: f32) {
        self.blit(layer, pm, x, cy - pm.height() as f32 / 2.0, opacity);
    }

    fn blit(&self, layer: &mut Pixmap, pm: &Pixmap, x: f32, y: f32, opacity: f32) {
        let pp = PixmapPaint { opacity, ..Default::default() };
        layer.draw_pixmap(x.round() as i32, y.round() as i32, pm.as_ref(), &pp, Transform::identity(), None);
    }

    fn draw_avatar(&mut self, layer: &mut Pixmap, cx: f32, cy: f32, r: f32, pal: &Palette, s: f32) {
        // halo
        let mut ring = Stroke { width: 6.0 * s, ..Default::default() };
        layer.stroke_path(&circle(cx, cy, r + 2.0 * s), &paint(pal.accent.skia(0.14)), &ring, Transform::identity(), None);
        ring.width = 2.0 * s;

        if let Some(av) = &self.avatar {
            // scale the (square) avatar into the circle via a pattern shader
            let size = (2.0 * r).round() as u32;
            let scaled = scale_square(av, size);
            let shader = tiny_skia::Pattern::new(
                scaled.as_ref(),
                tiny_skia::SpreadMode::Pad,
                tiny_skia::FilterQuality::Bilinear,
                1.0,
                Transform::from_translate(cx - r, cy - r),
            );
            let mut p = Paint::default();
            p.shader = shader;
            p.anti_alias = true;
            layer.fill_path(&circle(cx, cy, r), &p, FillRule::Winding, Transform::identity(), None);
        } else {
            layer.fill_path(&circle(cx, cy, r), &paint(pal.accent.lerp(pal.base, 0.45).skia(1.0)), FillRule::Winding, Transform::identity(), None);
            let initial = self.user.chars().next().map(|c| c.to_uppercase().to_string()).unwrap_or_default();
            let t = self.text.get(&initial, 28.0 * s, 700, pal.text);
            let pm = t;
            self.blit_center(layer, &pm, cx, cy, 1.0);
        }
        layer.stroke_path(&circle(cx, cy, r), &paint(Rgb(255, 255, 255).skia(0.28)), &ring, Transform::identity(), None);
    }
}

fn scale_square(src: &Pixmap, size: u32) -> Pixmap {
    let mut out = Pixmap::new(size.max(1), size.max(1)).unwrap();
    let k = size as f32 / src.width() as f32;
    out.draw_pixmap(
        0,
        0,
        src.as_ref(),
        &PixmapPaint { quality: tiny_skia::FilterQuality::Bilinear, ..Default::default() },
        Transform::from_scale(k, k),
        None,
    );
    out
}

/// `~/.face` (the classic avatar location), cropped to a square.
fn load_avatar() -> Option<Pixmap> {
    let home = std::env::var_os("HOME")?;
    let img = image::open(std::path::Path::new(&home).join(".face")).ok()?.to_rgba8();
    let side = img.width().min(img.height());
    let sq = image::imageops::crop_imm(&img, (img.width() - side) / 2, (img.height() - side) / 2, side, side).to_image();
    let mut pm = Pixmap::new(side, side)?;
    for (dst, src) in pm.data_mut().chunks_exact_mut(4).zip(sq.as_raw().chunks_exact(4)) {
        let a = src[3] as u32;
        dst.copy_from_slice(&[
            (src[0] as u32 * a / 255) as u8,
            (src[1] as u32 * a / 255) as u8,
            (src[2] as u32 * a / 255) as u8,
            src[3],
        ]);
    }
    Some(pm)
}

#[allow(dead_code)]
pub fn fade_in(st: &UiState, now: Instant) -> f32 {
    ease_out_cubic(now.duration_since(st.started).as_secs_f32() / 0.35)
}
