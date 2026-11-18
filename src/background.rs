//! Wallpaper -> cover-scaled, blurred, dimmed background, built once per
//! output size and cached. Frames then only repaint the small centre region.

use image::{imageops, imageops::FilterType, RgbaImage};
use tiny_skia::{
    Color, FillRule, GradientStop, Paint, PathBuilder, Pixmap, Point, RadialGradient, SpreadMode,
    Transform,
};

use crate::theme::{LockConfig, Palette};

pub struct Background {
    pub width: u32,
    pub height: u32,
    /// Premultiplied RGBA (fully opaque), used as the base for compositing.
    pub rgba: Pixmap,
    /// Same pixels as BGRA bytes == wl_shm `Xrgb8888` on little endian.
    pub bgra: Vec<u8>,
}

pub fn load_wallpaper(cfg: &LockConfig) -> Option<RgbaImage> {
    let path = cfg.wallpaper.as_ref()?;
    match image::open(path) {
        Ok(img) => Some(img.to_rgba8()),
        Err(e) => {
            tracing::warn!("wallpaper {}: {e}", path.display());
            None
        }
    }
}

pub fn build(
    wallpaper: Option<&RgbaImage>,
    w: u32,
    h: u32,
    scale: f32,
    cfg: &LockConfig,
    pal: &Palette,
) -> Background {
    let mut pm = Pixmap::new(w, h).expect("non-zero size");

    match wallpaper {
        Some(img) => {
            let cover = cover(img, w, h);
            // Gaussian blur on a 1/4 size copy: ~16x cheaper and visually identical.
            let (sw, sh) = ((w / 4).max(1), (h / 4).max(1));
            let small = imageops::resize(&cover, sw, sh, FilterType::Triangle);
            let sigma = (cfg.blur * scale / 4.0).max(0.5);
            let blurred = imageops::blur(&small, sigma);
            let full = imageops::resize(&blurred, w, h, FilterType::CatmullRom);
            for (dst, src) in pm.data_mut().chunks_exact_mut(4).zip(full.as_raw().chunks_exact(4)) {
                dst.copy_from_slice(&[src[0], src[1], src[2], 255]);
            }
        }
        None => gradient_fallback(&mut pm, pal),
    }

    // Dim, then vignette to pull the eye to the centre.
    let mut paint = Paint::default();
    paint.set_color(pal.base.skia(cfg.dim));
    pm.fill_rect(
        tiny_skia::Rect::from_xywh(0.0, 0.0, w as f32, h as f32).unwrap(),
        &paint,
        Transform::identity(),
        None,
    );

    let (cx, cy) = (w as f32 / 2.0, h as f32 / 2.0);
    let radius = (cx * cx + cy * cy).sqrt();
    if let Some(shader) = RadialGradient::new(
        Point::from_xy(cx, cy),
        Point::from_xy(cx, cy),
        radius,
        vec![
            GradientStop::new(0.0, Color::from_rgba8(0, 0, 0, 0)),
            GradientStop::new(0.55, Color::from_rgba8(0, 0, 0, 0)),
            GradientStop::new(1.0, pal.base.lerp(crate::theme::Rgb(0, 0, 0), 0.6).skia(0.38)),
        ],
        SpreadMode::Pad,
        Transform::identity(),
    ) {
        let mut p = Paint::default();
        p.shader = shader;
        let mut pb = PathBuilder::new();
        pb.push_rect(tiny_skia::Rect::from_xywh(0.0, 0.0, w as f32, h as f32).unwrap());
        if let Some(path) = pb.finish() {
            pm.fill_path(&path, &p, FillRule::Winding, Transform::identity(), None);
        }
    }

    dither(&mut pm);

    let mut bgra = pm.data().to_vec();
    for px in bgra.chunks_exact_mut(4) {
        px.swap(0, 2);
        px[3] = 255;
    }
    Background { width: w, height: h, rgba: pm, bgra }
}

/// Scale to cover `w`x`h`, centre-crop.
fn cover(img: &RgbaImage, w: u32, h: u32) -> RgbaImage {
    let (iw, ih) = img.dimensions();
    let s = (w as f32 / iw as f32).max(h as f32 / ih as f32);
    let (nw, nh) = (((iw as f32 * s).ceil() as u32).max(w), ((ih as f32 * s).ceil() as u32).max(h));
    let resized = imageops::resize(img, nw, nh, FilterType::Triangle);
    imageops::crop_imm(&resized, (nw - w) / 2, (nh - h) / 2, w, h).to_image()
}

fn gradient_fallback(pm: &mut Pixmap, pal: &Palette) {
    let (w, h) = (pm.width() as f32, pm.height() as f32);
    let top = pal.base.lerp(pal.accent, 0.22);
    let bottom = pal.base.lerp(crate::theme::Rgb(0, 0, 0), 0.35);
    if let Some(shader) = tiny_skia::LinearGradient::new(
        Point::from_xy(0.0, 0.0),
        Point::from_xy(w * 0.4, h),
        vec![GradientStop::new(0.0, top.skia(1.0)), GradientStop::new(1.0, bottom.skia(1.0))],
        SpreadMode::Pad,
        Transform::identity(),
    ) {
        let mut p = Paint::default();
        p.shader = shader;
        pm.fill_rect(tiny_skia::Rect::from_xywh(0.0, 0.0, w, h).unwrap(), &p, Transform::identity(), None);
    }
}

/// Cheap deterministic noise, kills banding in blurred gradients.
fn dither(pm: &mut Pixmap) {
    let mut s: u32 = 0x9E37_79B9;
    for px in pm.data_mut().chunks_exact_mut(4) {
        s ^= s << 13;
        s ^= s >> 17;
        s ^= s << 5;
        let n = ((s & 3) as i16) - 1;
        for c in &mut px[..3] {
            *c = (*c as i16 + n).clamp(0, 255) as u8;
        }
    }
}
