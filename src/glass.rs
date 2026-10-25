//! Liquid Glass for one card, rebuilt from Direct2D effects (port of glass.js + the .msw-* layers of widgets.css), second generation.
//!
//! Layer-local coordinates: the card layer bitmap is (w + 2m) x (h + 2m) px and the card sits at (m, m); the
//! margin holds the shadow. The backdrop bitmap is in WINDOW coordinates; its lens/blur/colour chain is drawn
//! with a translation of (m - card_x, m - card_y). Everything is created once; per redraw only the dynamic inputs (`Fx`: luminance,
//! appearance blend, blur sigma) change.
//!
//! Two passes:
//!  * `Glass::draw`      the STATIC layer: shadow, chromatic lens (3 displacement passes recombined with PLUS) -> blur -> colour matrix,
//!                       tint, adaptive dark veil / milky light veil, inner glow, sheen, a thin uniform base rim.
//!  * `Glass::draw_hi`   the DYNAMIC highlights over the finished card (a handful of gradient strokes, no layer, no allocation):
//!                       the specular rim that follows the virtual light, a soft counter-glow, the pointer glint, the materialise sweep.
//!
//! Every tunable constant lives in `GlassParams` (JSON override: `--glass-params file.json` or `%APPDATA%\macshell\glass.json`);
//! the defaults were fitted against the Apple reference screenshots by `mswidgets --calib` (see NATIVE.md, "Glass calibration").
use crate::gfx::*;
use serde_json::{json, Value};
use std::mem::ManuallyDrop;
use std::sync::OnceLock;
use windows::core::{Interface, Result};
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;

// ---------------------------------------------------------------- parameters

macro_rules! glass_params {
    ($($(#[$m:meta])* $name:ident : $def:expr, $lo:expr, $hi:expr;)*) => {
        /// All tunable constants of the glass. Logical px unless noted; alphas 0..1.
        #[derive(Clone, Copy, Debug, PartialEq)]
        pub struct GlassParams { $($(#[$m])* pub $name: f32,)* }
        impl Default for GlassParams {
            fn default() -> Self { GlassParams { $($name: $def,)* } }
        }
        impl GlassParams {
            pub const NAMES: &'static [&'static str] = &[$(stringify!($name),)*];
            pub fn get(&self, n: &str) -> Option<f32> {
                $(if n == stringify!($name) { return Some(self.$name); })*
                None
            }
            pub fn set(&mut self, n: &str, v: f32) -> bool {
                $(if n == stringify!($name) { self.$name = v; return true; })*
                false
            }
            /// (min, max) the calibration search may use.
            pub fn range(n: &str) -> Option<(f32, f32)> {
                $(if n == stringify!($name) { return Some(($lo, $hi)); })*
                None
            }
        }
    };
}

glass_params! {
    /// lens gaussian sigma
    blur: 4.19, 1.0, 30.0;
    /// refraction strength multiplier (x the size dependent shift)
    refr: 1.0, 0.0, 4.0;
    /// refraction band width multiplier
    refr_edge: 1.0, 0.5, 3.0;
    /// refraction profile exponent: displacement ~ (1 - t)^pow
    refr_pow: 2.3, 1.2, 4.0;
    /// chromatic split: the red pass is displaced by (1 - chroma_r), the blue one by (1 + chroma_b) of the green pass
    chroma_r: 0.04, 0.0, 0.7;
    chroma_b: 0.06, 0.0, 1.0;
    /// lens colour matrix: saturation and brightness
    sat: 1.92, 0.8, 2.8;
    bright: 0.81, 0.5, 1.4;
    /// white tint gradient: alpha at the start / end plus the slider (glass) slope
    tint0: 0.0, 0.0, 0.2;
    tint0_g: 0.13, 0.0, 0.3;
    tint1: 0.016, 0.0, 0.2;
    tint1_g: 0.05, 0.0, 0.2;
    /// diagonal sheen: peak alpha at the lit corner and alpha at 26 % of the diagonal
    sheen: 0.22, 0.0, 1.0;
    sheen_mid: 0.05, 0.0, 0.5;
    /// inner glow (white, gaussian sigma)
    glow: 0.06, 0.0, 0.8;
    glow_sigma: 11.5, 4.0, 24.0;
    /// adaptive dark veil over bright backdrops: gain, luminance threshold, ceiling
    veil_gain: 0.79, 0.0, 2.5;
    veil_thresh: 0.3575, 0.2, 0.7;
    veil_max: 0.138, 0.0, 0.7;
    /// thin uniform base rim: stroke width and alpha
    rim_w: 0.6, 0.6, 2.5;
    rim_base: 0.14, 0.0, 0.7;
    /// thin dark edge line under the rim (the references show a dark 1 px outline on the sides)
    edge_dark: 0.134, 0.0, 0.7;
    /// specular rim: peak alpha, stroke width, extent along the light axis (0..1), strength of the opposite (counter) highlight
    spec: 0.75, 0.0, 1.0;
    spec_w: 0.9, 0.6, 3.0;
    spec_len: 0.20, 0.05, 0.35;
    spec_back: 0.30, 0.0, 1.0;
    /// soft inner band along the lit edges: alpha and width
    spec_glow: 0.10, 0.06, 0.3;
    spec_glow_w: 12.0, 3.0, 24.0;
    /// pointer glint strength and radius
    glint: 0.9, 0.0, 1.0;
    glint_r: 46.0, 16.0, 90.0;
    /// virtual light direction, CSS degrees (315 = from the upper left)
    light_angle: 315.0, 0.0, 360.0;
    /// light appearance: milky white veil alpha, tint multiplier, rim multiplier, shadow multiplier, lens brightness multiplier
    milk: 0.56, 0.0, 0.7;
    light_tint: 1.5, 0.5, 3.0;
    light_rim: 0.7, 0.3, 1.5;
    light_shadow: 0.55, 0.1, 1.0;
    light_bright: 1.22, 0.8, 1.4;
    /// shadow: base alpha, extra alpha over dark backdrops, small-shadow ratio, big blur / offset, small blur / offset
    sh_a: 0.10, 0.06, 0.3;
    sh_dark: 0.20, 0.10, 0.5;
    sh_ratio: 0.4, 0.1, 1.0;
    sh_blur: 18.0, 6.0, 40.0;
    sh_dy: 9.0, 0.0, 36.0;
    sh_b_blur: 5.0, 1.0, 14.0;
    sh_b_dy: 2.0, 0.0, 8.0;
    /// corner radius multiplier of the cards (the settings panel's "Corners")
    radius_k: 1.0, 0.15, 1.7;
    /// card scale multiplier (the settings panel's "Card size"): the host lays the desk out with it, the glass renderer never reads it
    card_k: 1.0, 0.7, 1.5;
}

impl GlassParams {
    pub fn to_json(&self) -> Value {
        let mut o = serde_json::Map::new();
        for n in Self::NAMES {
            if let Some(v) = self.get(n) {
                o.insert((*n).to_string(), json!((v * 10000.0).round() as f64 / 10000.0));
            }
        }
        Value::Object(o)
    }
    /// Apply the keys of a JSON object; the names of unknown / non-numeric entries are returned (values are taken as they are).
    pub fn apply_json(&mut self, v: &Value) -> Vec<String> {
        let mut unknown = Vec::new();
        if let Some(o) = v.as_object() {
            for (k, x) in o {
                match x.as_f64() {
                    Some(f) if self.set(k, f as f32) => {}
                    _ => unknown.push(k.clone()),
                }
            }
        }
        unknown
    }
    pub fn from_json(v: &Value) -> GlassParams {
        let mut p = GlassParams::default();
        p.apply_json(v);
        p
    }
    pub fn load(path: &std::path::Path) -> std::result::Result<GlassParams, String> {
        let t = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let v: Value = serde_json::from_str(&t).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut p = GlassParams::default();
        let bad = p.apply_json(&v);
        if !bad.is_empty() {
            eprintln!("mswidgets: {}: ignored keys {bad:?}", path.display());
        }
        Ok(p)
    }
}

static GLOBAL: OnceLock<GlassParams> = OnceLock::new();

/// Fix the process-wide parameters: `path` (`--glass-params`) or `%APPDATA%\macshell\glass.json`, absent / unreadable = the defaults.
pub fn init_params(path: Option<&str>) {
    let p = match path {
        Some(p) => GlassParams::load(std::path::Path::new(p)).unwrap_or_else(|e| {
            eprintln!("mswidgets: --glass-params: {e}; using the defaults");
            GlassParams::default()
        }),
        None => std::env::var_os("APPDATA")
            .map(|a| std::path::PathBuf::from(a).join("macshell").join("glass.json"))
            .filter(|p| p.exists())
            .and_then(|p| GlassParams::load(&p).map_err(|e| eprintln!("mswidgets: {e}")).ok())
            .unwrap_or_default(),
    };
    let _ = GLOBAL.set(p);
}

/// The parameters in force (`init_params` is called at start; tests / tools that skip it get the defaults).
pub fn global_params() -> GlassParams {
    *GLOBAL.get_or_init(GlassParams::default)
}

// ---------------------------------------------------------------- the user's look (settings panel sliders, config.json `widgetsLook`)

/// One look slider: its `primary` parameter runs over lo..hi (a sub-range of the calibration range, so both ends still look good);
/// the `follow` parameters keep their ratio to the primary relative to the base values (the chromatic fringe moves both channels together).
pub struct LookDef {
    pub primary: &'static str,
    pub lo: f32,
    pub hi: f32,
    pub follow: &'static [&'static str],
}

/// The sliders of the settings panel, in panel order (after "Glass", which is `widgetsGlass`).
pub const LOOK: [LookDef; 10] = [
    LookDef { primary: "blur", lo: 1.0, hi: 24.0, follow: &[] },
    LookDef { primary: "refr", lo: 0.0, hi: 4.0, follow: &["refr_edge"] },
    LookDef { primary: "chroma_r", lo: 0.0, hi: 0.4, follow: &["chroma_b"] },
    LookDef { primary: "sat", lo: 0.8, hi: 2.6, follow: &[] },
    LookDef { primary: "bright", lo: 0.6, hi: 1.2, follow: &[] },
    LookDef { primary: "tint0_g", lo: 0.0, hi: 0.26, follow: &["tint1_g"] },
    LookDef { primary: "sheen", lo: 0.0, hi: 0.9, follow: &["spec", "sheen_mid", "rim_base"] },
    LookDef { primary: "glow", lo: 0.0, hi: 0.5, follow: &[] },
    LookDef { primary: "radius_k", lo: 0.15, hi: 1.7, follow: &[] },
    LookDef { primary: "card_k", lo: 0.7, hi: 1.5, follow: &[] },
];

impl LookDef {
    /// Every parameter this slider owns.
    pub fn names(&self) -> impl Iterator<Item = &'static str> {
        std::iter::once(self.primary).chain(self.follow.iter().copied())
    }
    /// Slider position 0..1 of the primary parameter in `p`.
    pub fn pos(&self, p: &GlassParams) -> f32 {
        ((p.get(self.primary).unwrap_or(self.lo) - self.lo) / (self.hi - self.lo)).clamp(0.0, 1.0)
    }
    /// Move the slider to `t` (0..1): sets the primary and scales the followers. Near the base value the knob snaps to it exactly.
    pub fn set_pos(&self, p: &mut GlassParams, base: &GlassParams, t: f32) {
        let mut v = self.lo + (self.hi - self.lo) * t.clamp(0.0, 1.0);
        let b = base.get(self.primary).unwrap_or(v);
        if (v - b).abs() < (self.hi - self.lo) * 0.02 {
            v = b;
        }
        p.set(self.primary, v);
        if b.abs() > 1e-6 {
            for f in self.follow {
                if let Some(bf) = base.get(f) {
                    let (lo, hi) = GlassParams::range(f).unwrap_or((f32::MIN, f32::MAX));
                    p.set(f, (bf * v / b).clamp(lo, hi));
                }
            }
        }
    }
    /// Primary value relative to the base (1.0 = unchanged).
    pub fn ratio(&self, p: &GlassParams, base: &GlassParams) -> f32 {
        let b = base.get(self.primary).unwrap_or(1.0);
        if b.abs() > 1e-6 {
            p.get(self.primary).unwrap_or(b) / b
        } else {
            1.0
        }
    }
}

/// The `widgetsLook` object for `cur`: only the look parameters that differ from `base` (the defaults + glass.json).
pub fn look_diff(base: &GlassParams, cur: &GlassParams) -> Value {
    let mut o = serde_json::Map::new();
    for n in LOOK.iter().flat_map(|d| d.names()) {
        if let (Some(b), Some(c)) = (base.get(n), cur.get(n)) {
            if (b - c).abs() > 1e-4 {
                o.insert(n.to_string(), json!((c * 10000.0).round() as f64 / 10000.0));
            }
        }
    }
    Value::Object(o)
}

/// `base` + the `widgetsLook` object: only the look parameters are honoured, each clamped to its calibration range.
pub fn apply_look(base: &GlassParams, look: &Value) -> GlassParams {
    let mut p = *base;
    for n in LOOK.iter().flat_map(|d| d.names()) {
        if let (Some(v), Some((lo, hi))) = (look.get(n).and_then(|v| v.as_f64()), GlassParams::range(n)) {
            p.set(n, (v as f32).clamp(lo, hi));
        }
    }
    p
}

// ---------------------------------------------------------------- styles and per-draw inputs

/// Look of a glass surface: the cards use the default; the gallery panel / toolbar are denser and frostier (wider blur + a dark veil).
#[derive(Clone, Copy, Debug)]
pub struct GlassStyle {
    /// gaussian sigma of the lens blur, logical px (None = `GlassParams::blur`)
    pub blur: Option<f32>,
    /// extra rgba veil over the lens (straight alpha), None for the cards
    pub veil: Option<[f32; 4]>,
}
impl Default for GlassStyle {
    fn default() -> Self {
        GlassStyle { blur: None, veil: None }
    }
}
impl GlassStyle {
    pub fn frosted() -> Self {
        GlassStyle { blur: Some(20.0), veil: Some([0.085, 0.08, 0.12, 0.74]) }
    }
}

/// Dynamic inputs of the static layer.
#[derive(Clone, Copy, Debug)]
pub struct Fx {
    /// backdrop luminance 0..1 (dark veil and shadow weight)
    pub lum: f32,
    /// appearance blend: 0 = dark glass, 1 = light (milky) glass
    pub k: f32,
    /// lens sigma override, logical px (<= 0: the style / params value); materialise animates this
    pub blur: f32,
}
impl Fx {
    pub fn new(lum: f32) -> Fx {
        Fx { lum, k: 0.0, blur: 0.0 }
    }
}

/// Dynamic inputs of the highlight pass.
#[derive(Clone, Copy, Debug)]
pub struct Hi {
    /// light direction, CSS degrees (the light comes FROM there)
    pub angle: f32,
    /// appearance blend 0..1
    pub k: f32,
    /// pointer glint: (contour point x, y in card px, strength 0..1)
    pub glint: Option<(f32, f32, f32)>,
    /// materialise sweep: (contour point x, y in card px, strength 0..1)
    pub sweep: Option<(f32, f32, f32)>,
    /// overall opacity
    pub alpha: f32,
}
impl Default for Hi {
    fn default() -> Self {
        Hi { angle: 315.0, k: 0.0, glint: None, sweep: None, alpha: 1.0 }
    }
}

// ---------------------------------------------------------------- pure geometry

/// CSS linear-gradient(angle) line through a box (angle clockwise from "up").
pub fn css_line(deg: f32, x: f32, y: f32, w: f32, h: f32) -> ((f32, f32), (f32, f32)) {
    let a = deg.to_radians();
    let (dx, dy) = (a.sin(), -a.cos());
    let len = (w * dx).abs() + (h * dy).abs();
    let (cx, cy) = (x + w / 2.0, y + h / 2.0);
    ((cx - dx * len / 2.0, cy - dy * len / 2.0), (cx + dx * len / 2.0, cy + dy * len / 2.0))
}

/// Total length of a rounded-rect contour.
pub fn contour_len(w: f32, h: f32, r: f32) -> f32 {
    2.0 * (w - 2.0 * r) + 2.0 * (h - 2.0 * r) + std::f32::consts::TAU * r
}

/// Point of the contour at fraction `u` (0..1, wraps): clockwise from the start of the top edge (x = r, y = 0).
pub fn contour_point(w: f32, h: f32, r: f32, u: f32) -> (f32, f32) {
    let r = r.min(w / 2.0).min(h / 2.0);
    let total = contour_len(w, h, r);
    let mut d = u.rem_euclid(1.0) * total;
    let arc = std::f32::consts::FRAC_PI_2 * r;
    let (sw, sh) = (w - 2.0 * r, h - 2.0 * r);
    // (length, start point, direction) of the 8 pieces; arcs are handled by their centre
    if d < sw {
        return (r + d, 0.0);
    }
    d -= sw;
    if d < arc {
        let a = d / r; // 0 at the top of the TR corner
        return (w - r + r * a.sin(), r - r * a.cos());
    }
    d -= arc;
    if d < sh {
        return (w, r + d);
    }
    d -= sh;
    if d < arc {
        let a = d / r;
        return (w - r + r * a.cos(), h - r + r * a.sin());
    }
    d -= arc;
    if d < sw {
        return (w - r - d, h);
    }
    d -= sw;
    if d < arc {
        let a = d / r;
        return (r - r * a.sin(), h - r + r * a.cos());
    }
    d -= arc;
    if d < sh {
        return (0.0, h - r - d);
    }
    d -= sh;
    let a = d / r;
    (r - r * a.cos(), r - r * a.sin())
}

/// Fraction `u` of the contour at the middle of the top-left corner arc (where the materialise sweep starts).
pub fn tl_corner_u(w: f32, h: f32, r: f32) -> f32 {
    let r = r.min(w / 2.0).min(h / 2.0);
    let total = contour_len(w, h, r);
    let arc = std::f32::consts::FRAC_PI_2 * r;
    // the TL arc is the last piece: it ends at the contour start
    1.0 - arc / 2.0 / total
}

/// Nearest contour point to the pointer (px, py) in card px, and the signed distance of the pointer to the contour (< 0 inside).
pub fn nearest_contour(w: f32, h: f32, r: f32, px: f32, py: f32) -> ((f32, f32), f32) {
    let r = r.min(w / 2.0).min(h / 2.0);
    let (cx, cy) = (px.clamp(r, w - r), py.clamp(r, h - r));
    let (dx, dy) = (px - cx, py - cy);
    let l = dx.hypot(dy);
    if l > 1e-4 {
        return ((cx + dx / l * r, cy + dy / l * r), l - r);
    }
    // inside the inner rectangle: the closest straight side
    let d = [(px, 0), (w - px, 1), (py, 2), (h - py, 3)];
    let (m, side) = d.iter().fold((f32::MAX, 0), |a, &(v, s)| if v < a.0 { (v, s) } else { a });
    let p = match side {
        0 => (0.0, py),
        1 => (w, py),
        2 => (px, 0.0),
        _ => (px, h),
    };
    (p, -m)
}

// ---------------------------------------------------------------- displacement map

/// Port of glass.js buildMap: BGRA, R/G = 128 + 127 * (inward unit normal * (1-t)^pow) inside the bevel, 128 elsewhere, alpha 255.
fn build_map(bw: i32, bh: i32, cx: i32, cy: i32, w: i32, h: i32, r: f32, bezel: f32, pow: f32) -> Vec<u8> {
    let mut d = vec![128u8; (bw * bh * 4) as usize];
    for i in (3..d.len()).step_by(4) {
        d[i] = 255;
    }
    let (hx, hy) = (w as f32 / 2.0, h as f32 / 2.0);
    let (mx, my) = (cx as f32 + hx, cy as f32 + hy);
    for y in (cy - 1).max(0)..(cy + h + 1).min(bh) {
        for x in (cx - 1).max(0)..(cx + w + 1).min(bw) {
            let px = x as f32 + 0.5 - mx;
            let py = y as f32 + 0.5 - my;
            let qx = px.abs() - (hx - r);
            let qy = py.abs() - (hy - r);
            let (ox, oy) = (qx.max(0.0), qy.max(0.0));
            let sdf = ox.hypot(oy) + qx.max(qy).min(0.0) - r; // < 0 inside
            let dist = -sdf;
            if dist > 0.0 && dist < bezel {
                let t = dist / bezel;
                let f = (1.0 - t).powf(pow);
                let (mut gx, mut gy);
                if qx > 0.0 && qy > 0.0 {
                    let l = qx.hypot(qy).max(1e-6);
                    gx = qx / l;
                    gy = qy / l;
                } else if qx > qy {
                    gx = 1.0;
                    gy = 0.0;
                } else {
                    gx = 0.0;
                    gy = 1.0;
                }
                gx *= if px < 0.0 { -1.0 } else { 1.0 };
                gy *= if py < 0.0 { -1.0 } else { 1.0 };
                let i = ((y * bw + x) * 4) as usize;
                d[i + 2] = (128.0 - 127.0 * gx * f).round() as u8; // R
                d[i + 1] = (128.0 - 127.0 * gy * f).round() as u8; // G
            }
        }
    }
    d
}

/// Border (px) of neutral pixels around the card in a shared displacement map; the map is extended to infinity by a clamp border effect.
pub const MAP_PAD: i32 = 4;

/// (radius, bezel, shift) in physical px for a card of w x h px.
fn geometry(w: i32, h: i32, s: f32, radius: f32, p: &GlassParams) -> (f32, f32, f32) {
    let (wl, hl) = (w as f32 / s, h as f32 / s);
    (radius * s, (wl.min(hl) * 0.17).clamp(14.0, 34.0) * s * p.refr_edge, (wl.min(hl) * 0.12).clamp(10.0, 24.0) * s * p.refr)
}

fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

/// D2D colour matrix (rows = input channel, columns = output channel) of CSS saturate(sat) * brightness(k) on opaque pixels.
pub fn lens_matrix(sat: f32, k: f32) -> [f32; 20] {
    let sm = [
        [0.213 + 0.787 * sat, 0.715 - 0.715 * sat, 0.072 - 0.072 * sat],
        [0.213 - 0.213 * sat, 0.715 + 0.285 * sat, 0.072 - 0.072 * sat],
        [0.213 - 0.213 * sat, 0.715 - 0.715 * sat, 0.072 + 0.928 * sat],
    ];
    let mut cm = [0f32; 20];
    for i in 0..3 {
        for o in 0..3 {
            cm[i * 4 + o] = sm[o][i] * k;
        }
    }
    cm[3 * 4 + 3] = 1.0;
    cm
}

/// Matrix that keeps only colour channel `ch` (0 = R, 1 = G, 2 = B); `keep_alpha` for exactly one branch so the PLUS recombination
/// reproduces the source alpha.
fn isolate(ch: usize, keep_alpha: bool) -> [f32; 20] {
    let mut cm = [0f32; 20];
    cm[ch * 4 + ch] = 1.0;
    if keep_alpha {
        cm[3 * 4 + 3] = 1.0;
    }
    cm
}

// ---------------------------------------------------------------- the glass

pub struct Glass {
    s: f32,
    g: f32,
    m: i32,
    w: f32,
    h: f32,
    r: f32,
    p: GlassParams,
    blur_sigma: f32,
    place: ID2D1Effect,
    tint_line: ((f32, f32), (f32, f32)),
    veil: Option<ID2D1SolidColorBrush>,
    off: D2D_POINT_2F,
    lens_out: ID2D1Image,
    blur: ID2D1Effect,
    mx: ID2D1Effect,
    _keep: Vec<ID2D1Effect>,
    _map: ID2D1Bitmap1,
    sh_a: ID2D1Effect,
    sh_a_out: ID2D1Image,
    sh_b: ID2D1Effect,
    sh_b_out: ID2D1Image,
    glow_out: ID2D1Image,
    _glow: ID2D1Effect,
    geom: ID2D1Geometry,
    tint: ID2D1LinearGradientBrush,
    sheen: ID2D1LinearGradientBrush,
    base_rim: ID2D1SolidColorBrush,
    edge: ID2D1SolidColorBrush,
    milk: ID2D1SolidColorBrush,
    dark: ID2D1SolidColorBrush,
    rrect: D2D1_ROUNDED_RECT,
    rim_rr: D2D1_ROUNDED_RECT,
    // highlight pass (card-local coordinates, origin = card top-left)
    lit: ID2D1LinearGradientBrush,
    spot: ID2D1RadialGradientBrush,
    sweep: ID2D1RadialGradientBrush,
}

unsafe fn white_stops(gfx: &Gfx, stops: &[(f32, f32)]) -> Result<ID2D1GradientStopCollection1> {
    let st: Vec<D2D1_GRADIENT_STOP> = stops.iter().map(|&(p, a)| D2D1_GRADIENT_STOP { position: p, color: col(1.0, 1.0, 1.0, a) }).collect();
    gfx.dc.CreateGradientStopCollection(&st, D2D1_COLOR_SPACE_SRGB, D2D1_COLOR_SPACE_SRGB, D2D1_BUFFER_PRECISION_8BPC_UNORM, D2D1_EXTEND_MODE_CLAMP, D2D1_COLOR_INTERPOLATION_MODE_STRAIGHT)
}

impl Glass {
    /// The displacement map of a w x h px card (the card sits MAP_PAD px inside); identical for every card of that size,
    /// so scenes build one per (size, radius) and share it instead of one window-sized map per card.
    pub fn make_map(gfx: &Gfx, w: i32, h: i32, s: f32, radius: f32, p: &GlassParams) -> Result<ID2D1Bitmap1> {
        let (r, bezel, _) = geometry(w, h, s, radius, p);
        let (bw, bh) = (w + 2 * MAP_PAD, h + 2 * MAP_PAD);
        gfx.from_bgra(bw as u32, bh as u32, &build_map(bw, bh, MAP_PAD, MAP_PAD, w, h, r, bezel, p.refr_pow))
    }

    /// `backdrop`: window-sized bitmap; card (x, y, w, h) in window px; `s` = UI scale; `m` = layer margin px; `g` = glass slider 0..1;
    /// `radius` = logical corner radius; `map` = `make_map` for this card size.
    pub fn new(gfx: &Gfx, backdrop: &ID2D1Bitmap1, x: i32, y: i32, w: i32, h: i32, s: f32, m: i32, g: f32, radius: f32, map: &ID2D1Bitmap1, p: &GlassParams) -> Result<Glass> {
        Glass::styled(gfx, backdrop, x, y, w, h, s, m, g, radius, map, GlassStyle::default(), p)
    }

    /// Move the glass to a new card position (window px): the lens samples the backdrop under the NEW place (live while dragging).
    pub fn set_pos(&mut self, x: i32, y: i32) {
        self.off = pt((self.m - x) as f32, (self.m - y) as f32);
        let _ = fx_mat3x2(&self.place, 2, mat(1.0, (x - MAP_PAD) as f32, (y - MAP_PAD) as f32));
    }

    fn tint_stops(&self, g: f32) -> [(f32, f32); 2] {
        let p = &self.p;
        [(0.0, p.tint0 + p.tint0_g * g), (1.0, p.tint1 + p.tint1_g * g)]
    }

    /// New glass slider value (tint strength + dark veil weight) without rebuilding the effect graph.
    pub fn set_g(&mut self, gfx: &Gfx, g: f32) -> Result<()> {
        let (a, b) = self.tint_line;
        self.tint = gfx.white_gradient(a, b, &self.tint_stops(g))?;
        self.g = g;
        Ok(())
    }

    pub fn params(&self) -> &GlassParams {
        &self.p
    }

    pub fn styled(gfx: &Gfx, backdrop: &ID2D1Bitmap1, x: i32, y: i32, w: i32, h: i32, s: f32, m: i32, g: f32, radius: f32, map: &ID2D1Bitmap1, style: GlassStyle, p: &GlassParams) -> Result<Glass> {
        unsafe {
            let dc = &gfx.dc;
            let (r, _, shift) = geometry(w, h, s, radius, p);

            // ---- lens: displacement map (moved under the card, clamp-extended) -> 3 displaced copies (R, G, B at slightly different
            //      strengths = chromatic aberration) recombined with PLUS -> blur -> saturate * brightness
            let place = dc.CreateEffect(&CLSID_D2D12DAffineTransform)?;
            place.SetInput(0, map, true);
            fx_u32(&place, 0, 0)?; // nearest: the move is a whole number of pixels
            fx_mat3x2(&place, 2, mat(1.0, (x - MAP_PAD) as f32, (y - MAP_PAD) as f32))?;
            let edge = dc.CreateEffect(&CLSID_D2D1Border)?;
            edge.SetInput(0, &place.GetOutput()?, true);
            fx_u32(&edge, 0, 0)?; // clamp X
            fx_u32(&edge, 1, 0)?; // clamp Y
            let edge_out = edge.GetOutput()?;
            let comp = dc.CreateEffect(&CLSID_D2D1Composite)?;
            comp.SetInputCount(3)?;
            fx_u32(&comp, 0, D2D1_COMPOSITE_MODE_PLUS.0 as u32)?;
            let mut keep = vec![place.clone(), edge];
            let strengths = [1.0 - p.chroma_r, 1.0, 1.0 + p.chroma_b];
            for (ch, k) in strengths.iter().enumerate() {
                let disp = dc.CreateEffect(&CLSID_D2D1DisplacementMap)?;
                disp.SetInput(0, backdrop, true);
                disp.SetInput(1, &edge_out, true);
                fx_f32(&disp, 0, shift * 2.0 * k)?; // D2D1_DISPLACEMENTMAP_PROP_SCALE: shift = scale * (C - 0.5)
                fx_u32(&disp, 1, 0)?; // X channel = R
                fx_u32(&disp, 2, 1)?; // Y channel = G
                let iso = dc.CreateEffect(&CLSID_D2D1ColorMatrix)?;
                iso.SetInput(0, &disp.GetOutput()?, true);
                fx_mat5x4(&iso, 0, isolate(ch, ch == 1))?;
                fx_u32(&iso, 1, 2)?; // D2D1_COLORMATRIX_ALPHA_MODE_STRAIGHT: raw channel arithmetic, no (un)premultiply
                comp.SetInput(ch as u32, &iso.GetOutput()?, true);
                keep.push(disp);
                keep.push(iso);
            }
            let blur = dc.CreateEffect(&CLSID_D2D1GaussianBlur)?;
            blur.SetInput(0, &comp.GetOutput()?, true);
            let sigma = style.blur.unwrap_or(p.blur);
            fx_f32(&blur, 0, sigma * s)?;
            fx_u32(&blur, 1, 1)?; // balanced
            fx_u32(&blur, 2, 1)?; // hard border
            let mx = dc.CreateEffect(&CLSID_D2D1ColorMatrix)?;
            mx.SetInput(0, &blur.GetOutput()?, true);
            fx_mat5x4(&mx, 0, lens_matrix(p.sat, p.bright))?;
            fx_bool(&mx, 2, true)?; // clamp output
            let lens_out = mx.GetOutput()?;
            keep.push(comp);

            // ---- geometry (layer-local)
            let (mf, wf, hf) = (m as f32, w as f32, h as f32);
            let rrect = rr(mf, mf, wf, hf, r);
            let geom: ID2D1Geometry = gfx.d2d.CreateRoundedRectangleGeometry(&rrect)?.cast()?;
            let white = dc.CreateSolidColorBrush(&col(1.0, 1.0, 1.0, 1.0), None)?;

            // ---- adaptive shadow: two rounded rects blurred (CSS: 0 18px 42px -8px, 0 2px 10px)
            let a_in = 8.0 * s;
            let cl_a = gfx.record(|dc| {
                dc.FillRoundedRectangle(&rr(mf + a_in, mf + a_in, wf - 2.0 * a_in, hf - 2.0 * a_in, (r - a_in).max(0.0)), &white);
                Ok(())
            })?;
            let cl_b = gfx.record(|dc| {
                dc.FillRoundedRectangle(&rrect, &white);
                Ok(())
            })?;
            let sh_a = dc.CreateEffect(&CLSID_D2D1Shadow)?;
            sh_a.SetInput(0, &cl_a, true);
            fx_f32(&sh_a, 0, p.sh_blur * s)?;
            let sh_b = dc.CreateEffect(&CLSID_D2D1Shadow)?;
            sh_b.SetInput(0, &cl_b, true);
            fx_f32(&sh_b, 0, p.sh_b_blur * s)?;

            // ---- inner glow: (big rect - card) blurred, clipped to the card (CSS inset 0 0 22px white .10)
            let big = gfx.d2d.CreateRectangleGeometry(&rf(mf - 90.0 * s, mf - 90.0 * s, mf + wf + 90.0 * s, mf + hf + 90.0 * s))?;
            let ring = gfx.d2d.CreatePathGeometry()?;
            {
                let sink = ring.Open()?;
                big.CombineWithGeometry(&geom, D2D1_COMBINE_MODE_EXCLUDE, None, 0.1, &sink)?;
                sink.Close()?;
            }
            let glow_brush = dc.CreateSolidColorBrush(&col(1.0, 1.0, 1.0, p.glow), None)?;
            let cl_ring = gfx.record(|dc| {
                dc.FillGeometry(&ring, &glow_brush, None::<&ID2D1Brush>);
                Ok(())
            })?;
            let glow = dc.CreateEffect(&CLSID_D2D1GaussianBlur)?;
            glow.SetInput(0, &cl_ring, true);
            fx_f32(&glow, 0, p.glow_sigma * s)?;
            fx_u32(&glow, 1, 1)?;

            // ---- gradients: tint veil (155deg), sheen (towards the side opposite the light), uniform base rim
            let (t0, t1) = css_line(155.0, mf, mf, wf, hf);
            let st = [(0.0, p.tint0 + p.tint0_g * g), (1.0, p.tint1 + p.tint1_g * g)];
            let tint = gfx.white_gradient(t0, t1, &st)?;
            let (s0, s1) = css_line((p.light_angle + 180.0).rem_euclid(360.0), mf, mf, wf, hf);
            let sheen = gfx.white_gradient(s0, s1, &[(0.0, p.sheen), (0.26, p.sheen_mid), (0.52, 0.0), (1.0, 0.0)])?;
            let hw = p.rim_w * s / 2.0;
            let rim_rr = rr(mf + hw, mf + hw, wf - 2.0 * hw, hf - 2.0 * hw, r - hw);
            let base_rim = dc.CreateSolidColorBrush(&col(1.0, 1.0, 1.0, p.rim_base), None)?;
            let edge = dc.CreateSolidColorBrush(&col(0.0, 0.0, 0.0, p.edge_dark), None)?;
            let milk = dc.CreateSolidColorBrush(&col(0.97, 0.98, 1.0, 0.0), None)?;
            let dark = dc.CreateSolidColorBrush(&col(12.0 / 255.0, 14.0 / 255.0, 20.0 / 255.0, 0.0), None)?;

            let veil = match style.veil {
                Some(v) => Some(dc.CreateSolidColorBrush(&col(v[0], v[1], v[2], v[3]), None)?),
                None => None,
            };

            // ---- highlight brushes (card-local): the lit gradient is re-aimed per frame with SetStart/EndPoint, its opacity carries the strength
            let sl = p.spec_len.clamp(0.05, 0.35);
            let lit_stops = [(0.0, 1.0), (sl * 0.55, 0.62), (sl * 1.25, 0.16), (0.5, 0.03), (0.68, 0.03), (0.84, 0.45 * p.spec_back), (1.0, p.spec_back)];
            let lit = {
                let coll = white_stops(gfx, &lit_stops)?;
                dc.CreateLinearGradientBrush(&D2D1_LINEAR_GRADIENT_BRUSH_PROPERTIES { startPoint: pt(0.0, 0.0), endPoint: pt(1.0, 1.0) }, None, &coll)?
            };
            let radial = |rad: f32| -> Result<ID2D1RadialGradientBrush> {
                let coll = white_stops(gfx, &[(0.0, 1.0), (0.45, 0.5), (1.0, 0.0)])?;
                dc.CreateRadialGradientBrush(&D2D1_RADIAL_GRADIENT_BRUSH_PROPERTIES { center: pt(0.0, 0.0), gradientOriginOffset: pt(0.0, 0.0), radiusX: rad, radiusY: rad }, None, &coll)
            };
            let spot = radial(p.glint_r * s)?;
            let sweep = radial(p.glint_r * 1.4 * s)?;

            Ok(Glass {
                s,
                g,
                m,
                w: wf,
                h: hf,
                r,
                p: *p,
                blur_sigma: sigma,
                tint_line: (t0, t1),
                veil,
                place: place.clone(),
                off: pt((m - x) as f32, (m - y) as f32),
                lens_out,
                blur: blur.clone(),
                mx: mx.clone(),
                _keep: {
                    keep.push(blur);
                    keep.push(mx);
                    keep
                },
                _map: map.clone(),
                sh_a_out: sh_a.GetOutput()?,
                sh_a,
                sh_b_out: sh_b.GetOutput()?,
                sh_b,
                glow_out: glow.GetOutput()?,
                _glow: glow,
                geom,
                tint,
                sheen,
                base_rim,
                edge,
                milk,
                dark,
                rrect,
                rim_rr,
                lit,
                spot,
                sweep,
            })
        }
    }

    /// Draw the static layer (shadow, lens, veils, glow, sheen, base rim) into the current target (layer-local, identity transform).
    /// `fx.lum` = backdrop luminance 0..1 (dark veil and shadow weight, like --lum / --shadow-a), `fx.k` = appearance blend.
    pub fn draw(&self, dc: &ID2D1DeviceContext, fx: &Fx) -> Result<()> {
        unsafe {
            let (s, p) = (self.s, &self.p);
            let k = fx.k.clamp(0.0, 1.0);
            let lum = fx.lum;
            dc.Clear(None);
            let a = (p.sh_a + p.sh_dark * (1.0 - lum)) * lerp(1.0, p.light_shadow, k);
            fx_vec4(&self.sh_a, 1, [0.0, 0.0, 0.0, a])?;
            fx_vec4(&self.sh_b, 1, [0.0, 0.0, 0.0, a * p.sh_ratio])?;
            dc.DrawImage(&self.sh_b_out, Some(&pt(0.0, p.sh_b_dy * s)), None, D2D1_INTERPOLATION_MODE_LINEAR, D2D1_COMPOSITE_MODE_SOURCE_OVER);
            dc.DrawImage(&self.sh_a_out, Some(&pt(0.0, p.sh_dy * s)), None, D2D1_INTERPOLATION_MODE_LINEAR, D2D1_COMPOSITE_MODE_SOURCE_OVER);

            // lens colour (appearance dependent brightness) and blur (materialise animates the sigma)
            fx_mat5x4(&self.mx, 0, lens_matrix(p.sat, p.bright * lerp(1.0, p.light_bright, k)))?;
            let sigma = if fx.blur > 0.0 { fx.blur } else { self.blur_sigma };
            fx_f32(&self.blur, 0, sigma * s)?;

            let mut lp = D2D1_LAYER_PARAMETERS1 {
                contentBounds: rf(-3.0e38, -3.0e38, 3.0e38, 3.0e38),
                geometricMask: ManuallyDrop::new(Some(self.geom.clone())),
                maskAntialiasMode: D2D1_ANTIALIAS_MODE_PER_PRIMITIVE,
                maskTransform: mat(1.0, 0.0, 0.0),
                opacity: 1.0,
                opacityBrush: ManuallyDrop::new(None),
                layerOptions: D2D1_LAYER_OPTIONS1_NONE,
            };
            dc.PushLayer(&lp, None::<&ID2D1Layer>);
            dc.DrawImage(&self.lens_out, Some(&self.off), None, D2D1_INTERPOLATION_MODE_LINEAR, D2D1_COMPOSITE_MODE_SOURCE_OVER);
            self.tint.SetOpacity(lerp(1.0, p.light_tint, k));
            dc.FillRoundedRectangle(&self.rrect, &self.tint);
            // adaptive dark veil: appears over bright content so the white text stays legible (dark appearance only)
            let dv = ((lum - p.veil_thresh) * p.veil_gain).clamp(0.0, p.veil_max) * (0.4 + 0.9 * self.g) * (1.0 - k);
            if dv > 0.002 {
                self.dark.SetColor(&col(12.0 / 255.0, 14.0 / 255.0, 20.0 / 255.0, dv));
                dc.FillRoundedRectangle(&self.rrect, &self.dark);
            }
            // light appearance: a milky white veil
            if k > 0.002 && p.milk > 0.0 {
                self.milk.SetColor(&col(0.97, 0.98, 1.0, p.milk * k));
                dc.FillRoundedRectangle(&self.rrect, &self.milk);
            }
            if let Some(v) = &self.veil {
                dc.FillRoundedRectangle(&self.rrect, v);
            }
            dc.DrawImage(&self.glow_out, None, None, D2D1_INTERPOLATION_MODE_LINEAR, D2D1_COMPOSITE_MODE_SOURCE_OVER);
            self.sheen.SetOpacity(lerp(1.0, 0.8, k));
            dc.FillRoundedRectangle(&self.rrect, &self.sheen);
            dc.PopLayer();
            ManuallyDrop::drop(&mut lp.geometricMask);
            if p.edge_dark > 0.002 {
                self.edge.SetColor(&col(0.0, 0.0, 0.0, p.edge_dark * lerp(1.0, 0.5, k)));
                dc.DrawRoundedRectangle(&self.rim_rr, &self.edge, p.rim_w * s, None::<&ID2D1StrokeStyle>);
            }
            // thin uniform base rim: white on dark glass, a cool grey line on light glass
            let c = lerp(1.0, 0.36, k);
            self.base_rim.SetColor(&col(c, lerp(1.0, 0.40, k), lerp(1.0, 0.48, k), p.rim_base * lerp(1.0, p.light_rim, k)));
            dc.DrawRoundedRectangle(&self.rim_rr, &self.base_rim, p.rim_w * s, None::<&ID2D1StrokeStyle>);
            Ok(())
        }
    }

    /// The dynamic highlights, drawn over the finished card. (ox, oy) = card top-left in the current target (physical px).
    /// Leaves the transform at identity. No allocation: a few gradient strokes along the card contour.
    pub fn draw_hi(&self, dc: &ID2D1DeviceContext, ox: f32, oy: f32, hi: &Hi) {
        unsafe {
            let (s, p) = (self.s, &self.p);
            let k = hi.k.clamp(0.0, 1.0);
            let al = hi.alpha.clamp(0.0, 1.0);
            if al < 0.004 {
                return;
            }
            dc.SetTransform(&mat(1.0, ox, oy));
            let (wf, hf) = (self.w, self.h);
            // the strength of the specular rim is lower on light glass (white on white); the glow stays
            let spec = p.spec * lerp(1.0, 0.75, k) * al;
            let (a, b) = css_line(hi.angle, 0.0, 0.0, wf, hf);
            // the gradient runs from the lit extreme (alpha 1) to the opposite one
            self.lit.SetStartPoint(pt(a.0, a.1));
            self.lit.SetEndPoint(pt(b.0, b.1));
            let stroke = |w_logical: f32| -> (D2D1_ROUNDED_RECT, f32) {
                let w = w_logical * s;
                (rr(w / 2.0, w / 2.0, wf - w, hf - w, (self.r - w / 2.0).max(0.0)), w)
            };
            // soft inner bands first (wide -> narrow), then the thin hard rim
            if p.spec_glow > 0.0 {
                for (wk, ok) in [(1.0, 0.2), (0.8, 0.2), (0.62, 0.2), (0.46, 0.2), (0.3, 0.2), (0.17, 0.2)] {
                    let (r, w) = stroke(p.spec_glow_w * wk);
                    self.lit.SetOpacity(p.spec_glow * ok * al);
                    dc.DrawRoundedRectangle(&r, &self.lit, w, None::<&ID2D1StrokeStyle>);
                }
            }
            if spec > 0.002 {
                let (r, w) = stroke(p.spec_w);
                self.lit.SetOpacity(spec);
                dc.DrawRoundedRectangle(&r, &self.lit, w, None::<&ID2D1StrokeStyle>);
            }
            // pointer glint and materialise sweep: radial falloff around a contour point
            // (brush, point, strength, rim width x, band width x, band alpha): the sweep is bolder than the pointer glint
            for (b, v, strength, wx, gw, ga) in [(&self.spot, hi.glint, p.glint, 1.15, 0.6, 0.22), (&self.sweep, hi.sweep, 1.0, 2.0, 0.9, 0.5)] {
                if let Some((x, y, t)) = v {
                    if t > 0.004 {
                        b.SetCenter(pt(x, y));
                        let (r, w) = stroke(p.spec_w * wx);
                        b.SetOpacity(t * strength * al);
                        dc.DrawRoundedRectangle(&r, b, w, None::<&ID2D1StrokeStyle>);
                        let (r, w) = stroke(p.spec_glow_w * gw);
                        b.SetOpacity(t * ga * al);
                        dc.DrawRoundedRectangle(&r, b, w, None::<&ID2D1StrokeStyle>);
                    }
                }
            }
            dc.SetTransform(&mat(1.0, 0.0, 0.0));
        }
    }

    /// Card radius in px (for contour helpers).
    pub fn radius_px(&self) -> f32 {
        self.r
    }
    pub fn size_px(&self) -> (f32, f32) {
        (self.w, self.h)
    }
    /// The lens output (blur + colour matrix applied) and where to draw it: the merge pass fills the liquid neck with the same lens.
    pub fn lens(&self) -> (&ID2D1Image, D2D_POINT_2F) {
        (&self.lens_out, self.off)
    }
    pub fn slider(&self) -> f32 {
        self.g
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn params_json_round_trip() {
        let mut p = GlassParams::default();
        p.blur = 7.25;
        p.spec_len = 0.3;
        p.light_angle = 200.0;
        let j = p.to_json();
        let q = GlassParams::from_json(&j);
        assert!((q.blur - 7.25).abs() < 1e-4 && (q.spec_len - 0.3).abs() < 1e-4 && (q.light_angle - 200.0).abs() < 1e-4);
        assert_eq!(GlassParams::from_json(&json!({})), GlassParams::default());
        let mut z = GlassParams::default();
        let bad = z.apply_json(&json!({"blur": 3.0, "nonsense": 1, "sat": "x"}));
        assert_eq!(z.blur, 3.0);
        assert_eq!(bad.len(), 2);
        // every name has a range that contains its default
        for n in GlassParams::NAMES {
            let (lo, hi) = GlassParams::range(n).unwrap();
            let d = GlassParams::default().get(n).unwrap();
            assert!(lo <= d && d <= hi, "{n}: {d} not in {lo}..{hi}");
        }
    }

    /// The slider <-> parameter mapping: sub-ranges inside the calibration ranges around the base value, followers scale with the
    /// primary, the knob snaps to the base, the `widgetsLook` diff round-trips and only carries changed look keys.
    #[test]
    fn look_sliders_map_to_params() {
        let base = GlassParams::default();
        for d in &LOOK {
            let (rl, rh) = GlassParams::range(d.primary).unwrap();
            assert!(rl <= d.lo && d.hi <= rh && d.lo < d.hi, "{}", d.primary);
            let t = d.pos(&base);
            assert!(t > 0.0 && t < 1.0, "{}: base at the end of its slider", d.primary);
            for f in d.follow {
                assert!(GlassParams::range(f).is_some());
            }
        }
        let chroma = &LOOK[2];
        let mut p = base;
        chroma.set_pos(&mut p, &base, 1.0);
        assert!((p.chroma_r - 0.4).abs() < 1e-5 && (p.chroma_b - base.chroma_b * 0.4 / base.chroma_r).abs() < 1e-3, "{p:?}");
        chroma.set_pos(&mut p, &base, 0.0);
        assert_eq!((p.chroma_r, p.chroma_b), (0.0, 0.0));
        // near the base value the knob snaps to it exactly (so "back at default" leaves no key behind)
        let blur = &LOOK[0];
        blur.set_pos(&mut p, &base, blur.pos(&base) + 0.01);
        assert_eq!(p.blur, base.blur);
        assert_eq!(look_diff(&base, &p).as_object().map(|o| o.keys().cloned().collect::<Vec<_>>()), Some(vec!["chroma_b".to_string(), "chroma_r".to_string()]));
        // out of range positions are clamped; position <-> value round trip
        blur.set_pos(&mut p, &base, 7.0);
        assert_eq!(p.blur, blur.hi);
        blur.set_pos(&mut p, &base, 0.5);
        assert!((blur.pos(&p) - 0.5).abs() < 1e-5);
        // widgetsLook: only differences, applying them gives the same look back
        let mut q = base;
        LOOK[6].set_pos(&mut q, &base, 1.0); // highlights: sheen + spec + sheen_mid + rim_base
        LOOK[8].set_pos(&mut q, &base, 0.9);
        let diff = look_diff(&base, &q);
        let keys: Vec<&str> = diff.as_object().unwrap().keys().map(|k| k.as_str()).collect();
        assert_eq!(keys.len(), 5, "{keys:?}");
        for k in ["spec", "sheen", "sheen_mid", "rim_base", "radius_k"] {
            assert!(keys.contains(&k), "{k}");
        }
        let back = apply_look(&base, &diff);
        for n in GlassParams::NAMES {
            assert!((back.get(n).unwrap() - q.get(n).unwrap()).abs() < 1e-3, "{n}");
        }
        assert_eq!(look_diff(&base, &base), json!({}));
        // the base is glass.json + defaults: a diff is relative to it; foreign / out of range entries cannot break the look
        let mut gj = base;
        gj.blur = 6.0;
        assert_eq!(look_diff(&gj, &gj), json!({}));
        let wild = apply_look(&gj, &json!({"blur": 900, "sat": -3, "nonsense": 1, "light_angle": 10, "refr": "x"}));
        assert_eq!((wild.blur, wild.sat), (30.0, 0.8));
        assert_eq!((wild.light_angle, wild.refr), (gj.light_angle, gj.refr)); // not a look key / not a number
    }

    #[test]
    fn contour_walks_the_rounded_rect() {
        let (w, h, r) = (170.0, 100.0, 24.0);
        let (x, y) = contour_point(w, h, r, 0.0);
        assert!((x - r).abs() < 1e-3 && y.abs() < 1e-3);
        // every point lies on the contour: signed distance ~ 0
        for i in 0..200 {
            let (x, y) = contour_point(w, h, r, i as f32 / 200.0);
            let (_, d) = nearest_contour(w, h, r, x, y);
            assert!(d.abs() < 0.05, "u={} d={d}", i as f32 / 200.0);
        }
        let (x, y) = contour_point(w, h, r, tl_corner_u(w, h, r));
        assert!(x < r && y < r, "TL corner arc: {x},{y}");
    }

    #[test]
    fn nearest_point_inside_and_outside() {
        let ((x, y), d) = nearest_contour(100.0, 100.0, 20.0, 50.0, 8.0);
        assert!((x - 50.0).abs() < 1e-3 && y.abs() < 1e-3 && (d + 8.0).abs() < 1e-3);
        let ((x, y), d) = nearest_contour(100.0, 100.0, 20.0, -10.0, 50.0);
        assert!(x.abs() < 1e-3 && (y - 50.0).abs() < 1e-3 && (d - 10.0).abs() < 1e-3);
        // corner: pointer outside the arc along the diagonal
        let ((x, y), d) = nearest_contour(100.0, 100.0, 20.0, -5.0, -5.0);
        assert!(x > 0.0 && y > 0.0 && d > 0.0);
    }

    /// The three chromatic passes recombined with PLUS must reproduce an opaque AND a translucent backdrop exactly where nothing refracts
    /// (the plain / translucent base of the "plain" glass mode keeps its alpha).
    #[test]
    fn chromatic_lens_keeps_colour_and_alpha() {
        unsafe {
            let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_MULTITHREADED);
        }
        let gfx = Gfx::new(None).unwrap();
        for a in [1.0f32, 0.55] {
            let px: Vec<u8> = (0..300 * 300).flat_map(|_| [(120.0 * a) as u8, (80.0 * a) as u8, (50.0 * a) as u8, (255.0 * a) as u8]).collect();
            let bd = gfx.from_bgra(300, 300, &px).unwrap();
            let mut p = GlassParams::default();
            p.sat = 1.0;
            p.bright = 1.0;
            p.blur = 1.0;
            let map = Glass::make_map(&gfx, 200, 200, 1.0, 28.0, &p).unwrap();
            let g = Glass::new(&gfx, &bd, 50, 50, 200, 200, 1.0, 48, 0.5, 28.0, &map, &p).unwrap();
            let t = gfx.target(300, 300).unwrap();
            let cpu = gfx.cpu_read(300, 300).unwrap();
            unsafe {
                gfx.dc.SetTarget(&t);
                gfx.dc.BeginDraw();
                gfx.dc.Clear(None);
                let (lens, off) = g.lens();
                gfx.dc.DrawImage(lens, Some(&pt(off.x + (50 - 48) as f32 * 0.0, off.y)), None, D2D1_INTERPOLATION_MODE_NEAREST_NEIGHBOR, D2D1_COMPOSITE_MODE_SOURCE_COPY);
                gfx.dc.EndDraw(None, None).unwrap();
                gfx.dc.SetTarget(None::<&ID2D1Image>);
            }
            let mut got = [0u8; 4];
            // lens drawn at off = (m - x, m - y): the backdrop pixel (150, 150) lands on (150 - 2, 150 - 2)
            gfx.read_rect(&t, &cpu, windows::Win32::Foundation::RECT { left: 148, top: 148, right: 149, bottom: 149 }, |_, row| got.copy_from_slice(&row[..4])).unwrap();
            for (k, want) in [(0, 120.0 * a), (1, 80.0 * a), (2, 50.0 * a), (3, 255.0 * a)] {
                assert!((got[k] as f32 - want).abs() <= 3.0, "alpha {a}: channel {k} = {} want {want}", got[k]);
            }
        }
    }

    #[test]
    fn css_line_matches_the_css_spec() {
        let (a, b) = css_line(180.0, 0.0, 0.0, 100.0, 50.0);
        assert!((a.1 - 0.0).abs() < 1e-3 && (b.1 - 50.0).abs() < 1e-3);
        let (a, b) = css_line(90.0, 0.0, 0.0, 100.0, 50.0);
        assert!((a.0 - 0.0).abs() < 1e-3 && (b.0 - 100.0).abs() < 1e-3);
    }
}
