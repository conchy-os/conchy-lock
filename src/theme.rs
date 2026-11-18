//! Palette + lock configuration.
//!
//! Colors come from `~/.config/conchy/theme.kdl` (the same base16 theme the
//! shell uses) so the lock screen always matches the rest of the desktop.
//! Lock-specific options live in `~/.config/conchy/lock.kdl`:
//!
//! ```kdl
//! wallpaper "~/wallpapers/h_1.jpg"
//! blur 28      // logical px
//! dim 0.30     // 0..1 darkening on top of the blurred wallpaper
//! ```

use std::path::PathBuf;

use kdl::KdlDocument;

#[derive(Clone, Copy, Debug)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl Rgb {
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim().trim_start_matches('#');
        if s.len() < 6 {
            return None;
        }
        let p = |i: usize| u8::from_str_radix(&s[i..i + 2], 16).ok();
        Some(Rgb(p(0)?, p(2)?, p(4)?))
    }

    pub fn skia(self, alpha: f32) -> tiny_skia::Color {
        tiny_skia::Color::from_rgba8(self.0, self.1, self.2, (alpha.clamp(0.0, 1.0) * 255.0) as u8)
    }

    pub fn lerp(self, o: Rgb, t: f32) -> Rgb {
        let m = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * t.clamp(0.0, 1.0)) as u8;
        Rgb(m(self.0, o.0), m(self.1, o.1), m(self.2, o.2))
    }
}

#[derive(Clone, Debug)]
pub struct Palette {
    pub base: Rgb,
    pub surface: Rgb,
    pub text: Rgb,
    pub subtext: Rgb,
    pub accent: Rgb,
    pub danger: Rgb,
    pub success: Rgb,
    pub warn: Rgb,
}

impl Default for Palette {
    /// Nord.
    fn default() -> Self {
        Self {
            base: Rgb(0x2e, 0x34, 0x40),
            surface: Rgb(0x3b, 0x42, 0x52),
            text: Rgb(0xec, 0xef, 0xf4),
            subtext: Rgb(0xd8, 0xde, 0xe9),
            accent: Rgb(0xb4, 0x8e, 0xad),
            danger: Rgb(0xbf, 0x61, 0x6a),
            success: Rgb(0xa3, 0xbe, 0x8c),
            warn: Rgb(0xeb, 0xcb, 0x8b),
        }
    }
}

fn config_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_default()
        .join("conchy")
}

fn expand(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(path)
}

impl Palette {
    pub fn load() -> Self {
        let mut pal = Self::default();
        let Ok(src) = std::fs::read_to_string(config_dir().join("theme.kdl")) else {
            return pal;
        };
        let Ok(doc) = src.parse::<KdlDocument>() else {
            tracing::warn!("theme.kdl: parse error, using Nord");
            return pal;
        };
        let Some(b16) = doc.get("base16").and_then(|n| n.children()) else {
            return pal;
        };
        let get = |name: &str| {
            b16.get(name)
                .and_then(|n| n.get(0))
                .and_then(|e| e.value().as_string())
                .and_then(Rgb::parse)
        };
        if let Some(c) = get("base00") {
            pal.base = c;
        }
        if let Some(c) = get("base01") {
            pal.surface = c;
        }
        if let Some(c) = get("base06") {
            pal.text = c;
        }
        if let Some(c) = get("base04") {
            pal.subtext = c;
        }
        if let Some(c) = get("base0e") {
            pal.accent = c;
        }
        if let Some(c) = get("base08") {
            pal.danger = c;
        }
        if let Some(c) = get("base0b") {
            pal.success = c;
        }
        if let Some(c) = get("base0a") {
            pal.warn = c;
        }
        pal
    }
}

#[derive(Clone, Debug)]
pub struct LockConfig {
    pub wallpaper: Option<PathBuf>,
    /// Blur radius in logical pixels.
    pub blur: f32,
    pub dim: f32,
}

impl Default for LockConfig {
    fn default() -> Self {
        Self { wallpaper: None, blur: 28.0, dim: 0.16 }
    }
}

impl LockConfig {
    pub fn load() -> Self {
        let mut cfg = Self::default();
        let Ok(src) = std::fs::read_to_string(config_dir().join("lock.kdl")) else {
            return cfg;
        };
        let Ok(doc) = src.parse::<KdlDocument>() else {
            tracing::warn!("lock.kdl: parse error, using defaults");
            return cfg;
        };
        if let Some(p) = doc.get("wallpaper").and_then(|n| n.get(0)).and_then(|e| e.value().as_string()) {
            cfg.wallpaper = Some(expand(p));
        }
        let num = |name: &str| {
            doc.get(name).and_then(|n| n.get(0)).and_then(|e| {
                let v = e.value();
                v.as_f64().or_else(|| v.as_i64().map(|i| i as f64))
            })
        };
        if let Some(v) = num("blur") {
            cfg.blur = v as f32;
        }
        if let Some(v) = num("dim") {
            cfg.dim = (v as f32).clamp(0.0, 1.0);
        }
        cfg
    }
}
