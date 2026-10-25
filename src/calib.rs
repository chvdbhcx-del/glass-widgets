//! `mswidgets --calib`: numeric calibration of the glass against the Apple reference screenshots (native/calib/ref/*.png).
//!
//! Apple's system components cannot be queried, so the references are measured instead:
//!  * a synthetic backdrop is rebuilt for every reference crop from the wallpaper beside the card (per row: a linear blend of the strip left
//!    of the card and the strip right of it) - the glass is rendered over exactly that, at the reference pixel size;
//!  * the render and the reference are reduced to the same FEATURES and compared:
//!      (a) edge profiles: RGB perpendicular to every usable edge from 10 px outside to 24 px inside (median along the middle half of the edge):
//!          rim brightness peak / width, inner fall-off, and the shadow outside;
//!      (b) interior tint and contrast against the backdrop: the vertical profile of the per-row MEDIAN colour over the middle 60 % of the card
//!          (the median removes the text), which also carries the blur (how a backdrop gradient is smeared) and the colour matrix;
//!      (c) corner profiles along the 45 degree diagonals (rim at the corner arc, refraction band);
//!  * `distance` = weighted mean absolute RGB difference (0..1) of those groups; the search is a coordinate descent over `GlassParams`
//!    (step = 25 % of the parameter range, halved every round) with a small prior towards the defaults.
//! The references contain no grid / text close-up, so the refraction strength itself is only constrained through the edge / corner profiles.
//! Everything is deterministic: same references + same start parameters = same result (`--calib fit --calib-out glass.json`).
use crate::gfx::*;
use crate::glass::{Fx, Glass, GlassParams, GlassStyle, Hi};
use windows::core::Result;
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;

// ---------------------------------------------------------------- images and features

/// RGB image, 0..255 floats.
#[derive(Clone)]
pub struct Img {
    pub w: usize,
    pub h: usize,
    pub px: Vec<[f32; 3]>,
}

impl Img {
    pub fn new(w: usize, h: usize, f: impl Fn(usize, usize) -> [f32; 3]) -> Img {
        let mut px = Vec::with_capacity(w * h);
        for y in 0..h {
            for x in 0..w {
                px.push(f(x, y));
            }
        }
        Img { w, h, px }
    }
    pub fn at(&self, x: i32, y: i32) -> [f32; 3] {
        self.px[(y.clamp(0, self.h as i32 - 1) as usize) * self.w + x.clamp(0, self.w as i32 - 1) as usize]
    }
    fn bilinear(&self, x: f32, y: f32) -> [f32; 3] {
        let (x0, y0) = (x.floor(), y.floor());
        let (fx, fy) = (x - x0, y - y0);
        let (a, b, c, d) = (self.at(x0 as i32, y0 as i32), self.at(x0 as i32 + 1, y0 as i32), self.at(x0 as i32, y0 as i32 + 1), self.at(x0 as i32 + 1, y0 as i32 + 1));
        let mut o = [0.0; 3];
        for k in 0..3 {
            o[k] = (a[k] * (1.0 - fx) + b[k] * fx) * (1.0 - fy) + (c[k] * (1.0 - fx) + d[k] * fx) * fy;
        }
        o
    }
    /// From premultiplied BGRA rows (opaque pixels).
    pub fn from_bgra(w: usize, h: usize, bgra: &[u8]) -> Img {
        Img::new(w, h, |x, y| {
            let i = (y * w + x) * 4;
            [bgra[i + 2] as f32, bgra[i + 1] as f32, bgra[i] as f32]
        })
    }
    pub fn save(&self, gfx: &Gfx, path: &str) -> Result<()> {
        let mut b = Vec::with_capacity(self.w * self.h * 4);
        for p in &self.px {
            b.extend_from_slice(&[p[2].clamp(0.0, 255.0) as u8, p[1].clamp(0.0, 255.0) as u8, p[0].clamp(0.0, 255.0) as u8, 255]);
        }
        gfx.save_png(path, self.w as u32, self.h as u32, &b)
    }
}

fn median(v: &mut [f32]) -> f32 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    v[v.len() / 2]
}

fn median_rgb(samples: &[[f32; 3]]) -> [f32; 3] {
    let mut o = [0.0; 3];
    for k in 0..3 {
        let mut c: Vec<f32> = samples.iter().map(|s| s[k]).collect();
        o[k] = median(&mut c);
    }
    o
}

/// One reference crop: where the card is, what it is, where the backdrop can be sampled and which features are trustworthy.
pub struct RefSpec {
    pub name: &'static str,
    pub file: &'static str,
    /// card rectangle (x, y, w, h) in reference px
    pub card: (i32, i32, i32, i32),
    /// logical size and corner radius of the card (sets the UI scale = px / logical)
    pub size: (f32, f32),
    pub radius: f32,
    /// wallpaper strips (x ranges) left / right of the card, used to rebuild the backdrop behind it
    pub left: (i32, i32),
    pub right: (i32, i32),
    /// 0 = dark glass, 1 = light glass
    pub look: f32,
    /// lens blur override (frosted panels)
    pub blur: Option<f32>,
    /// usable edges: left, top, right, bottom
    pub edges: [bool; 4],
    /// usable corners: TL, TR, BR, BL
    pub corners: [bool; 4],
    /// weight of this reference in the objective
    pub weight: f32,
}

/// The references (see native/calib/ref; the pictures are the Apple screenshots the lead supplied).
pub fn references() -> Vec<RefSpec> {
    let sharp = |name, file, x, left, right| RefSpec { name, file, card: (x, 12, 330, 328), size: (170.0, 170.0), radius: 28.0, left, right, look: 0.0, blur: None, edges: [true; 4], corners: [true; 4], weight: 1.0 };
    vec![
        // cmp01 / cmp03: calendar S and weather S on a warm wallpaper (sharp, 1.94x): the main references
        sharp("cmp01 calendar", "cmp01.png", 12, (0, 10), (346, 368)),
        sharp("cmp01 weather", "cmp01.png", 372, (346, 366), (704, 713)),
        sharp("cmp03 calendar", "cmp03.png", 12, (0, 10), (346, 368)),
        sharp("cmp03 weather", "cmp03.png", 372, (346, 366), (704, 713)),
        // zoomed video frames (blurry, edges uncertain): interior tone only
        RefSpec { name: "clear_z calendar", file: "clear_z.png", card: (32, 28, 318, 302), size: (170.0, 170.0), radius: 28.0, left: (2, 26), right: (356, 380), look: 0.0, blur: None, edges: [false; 4], corners: [false; 4], weight: 0.35 },
        RefSpec { name: "clear_z weather", file: "clear_z.png", card: (385, 14, 680, 316), size: (356.0, 170.0), radius: 28.0, left: (356, 380), right: (1075, 1105), look: 0.0, blur: None, edges: [false; 4], corners: [false; 4], weight: 0.35 },
        RefSpec { name: "dark_z calendar", file: "dark_z.png", card: (38, 335, 297, 300), size: (170.0, 170.0), radius: 28.0, left: (2, 30), right: (342, 372), look: 0.0, blur: None, edges: [false; 4], corners: [false; 4], weight: 0.35 },
        RefSpec { name: "dark_z weather", file: "dark_z.png", card: (380, 335, 308, 305), size: (170.0, 170.0), radius: 28.0, left: (342, 372), right: (695, 730), look: 0.0, blur: None, edges: [false; 4], corners: [false; 4], weight: 0.35 },
        // gallery.png: Apple's light frosted panel (top / left / right edge, top corners): the light appearance
        RefSpec { name: "gallery panel (light)", file: "gallery.png", card: (47, 380, 777, 418), size: (777.0, 418.0), radius: 14.0, left: (0, 44), right: (828, 1140), look: 1.0, blur: Some(20.0), edges: [true, true, true, false], corners: [true, true, false, false], weight: 1.0 },
    ]
}

const OUT_PX: i32 = 10;
const IN_PX: i32 = 24;

/// What the metric looks at.
#[derive(Clone, Debug, Default)]
pub struct Features {
    /// per edge (left, top, right, bottom): RGB from OUT_PX outside to IN_PX inside
    pub edges: [Vec<[f32; 3]>; 4],
    /// per row (every 3 px, 5 % .. 95 % of the card height): median RGB over the middle 60 % of the width
    pub rows: Vec<[f32; 3]>,
    /// per corner: RGB along the inward diagonal from OUT_PX outside the contour to IN_PX inside
    pub corners: [Vec<[f32; 3]>; 4],
}

/// Features of `img` for the card `rs.card` (px) with corner radius `r_px`.
pub fn features(img: &Img, rs: &RefSpec, r_px: f32) -> Features {
    let (x0, y0, w, h) = rs.card;
    let (x1, y1) = (x0 + w, y0 + h);
    let mut f = Features::default();
    let (ya, yb) = (y0 + h / 4, y1 - h / 4);
    let (xa, xb) = (x0 + w / 4, x1 - w / 4);
    for e in 0..4 {
        if !rs.edges[e] {
            continue;
        }
        for d in -OUT_PX..IN_PX {
            let mut s = Vec::new();
            match e {
                0 => (ya..yb).for_each(|y| s.push(img.at(x0 + d, y))),
                1 => (xa..xb).for_each(|x| s.push(img.at(x, y0 + d))),
                2 => (ya..yb).for_each(|y| s.push(img.at(x1 - 1 - d, y))),
                _ => (xa..xb).for_each(|x| s.push(img.at(x, y1 - 1 - d))),
            }
            f.edges[e].push(median_rgb(&s));
        }
    }
    let (rxa, rxb) = (x0 + w / 5, x1 - w / 5);
    let mut y = y0 + h / 20;
    while y < y1 - h / 20 {
        let s: Vec<[f32; 3]> = (rxa..rxb).map(|x| img.at(x, y)).collect();
        f.rows.push(median_rgb(&s));
        y += 3;
    }
    let k = std::f32::consts::FRAC_1_SQRT_2;
    for c in 0..4 {
        if !rs.corners[c] {
            continue;
        }
        // arc centre and outward direction
        let (cx, cy, dx, dy) = match c {
            0 => (x0 as f32 + r_px, y0 as f32 + r_px, -k, -k),
            1 => (x1 as f32 - r_px, y0 as f32 + r_px, k, -k),
            2 => (x1 as f32 - r_px, y1 as f32 - r_px, k, k),
            _ => (x0 as f32 + r_px, y1 as f32 - r_px, -k, k),
        };
        let (px, py) = (cx + dx * r_px, cy + dy * r_px);
        for d in -OUT_PX..IN_PX {
            let mut acc = [0.0f32; 3];
            for t in [-2.0f32, 0.0, 2.0] {
                let s = img.bilinear(px - dx * d as f32 + (-dy) * t, py - dy * d as f32 + dx * t);
                for q in 0..3 {
                    acc[q] += s[q] / 3.0;
                }
            }
            f.corners[c].push(acc);
        }
    }
    f
}

fn l1(a: &[[f32; 3]], b: &[[f32; 3]]) -> f32 {
    let n = a.len().min(b.len());
    if n == 0 {
        return 0.0;
    }
    let mut s = 0.0;
    for i in 0..n {
        for k in 0..3 {
            s += (a[i][k] - b[i][k]).abs();
        }
    }
    s / (n * 3) as f32 / 255.0
}

/// Weighted distance between two feature sets and its (edges, rows, corners) parts. 0 = identical.
pub fn distance(a: &Features, b: &Features) -> (f32, [f32; 3]) {
    let mut e = 0.0;
    let mut ne = 0.0;
    for i in 0..4 {
        if !a.edges[i].is_empty() {
            e += l1(&a.edges[i], &b.edges[i]);
            ne += 1.0;
        }
    }
    let e = if ne > 0.0 { e / ne } else { 0.0 };
    let r = l1(&a.rows, &b.rows);
    let mut c = 0.0;
    let mut nc = 0.0;
    for i in 0..4 {
        if !a.corners[i].is_empty() {
            c += l1(&a.corners[i], &b.corners[i]);
            nc += 1.0;
        }
    }
    let c = if nc > 0.0 { c / nc } else { 0.0 };
    (1.0 * e + 1.0 * r + 0.6 * c, [e, r, c])
}

// ---------------------------------------------------------------- synthetic backdrop

/// Per row: a linear blend of the median colour of the left strip and the right strip, across the whole width.
pub fn synth_backdrop(img: &Img, rs: &RefSpec) -> Img {
    let rows: Vec<([f32; 3], [f32; 3])> = (0..img.h)
        .map(|y| {
            let l: Vec<[f32; 3]> = (rs.left.0..rs.left.1).map(|x| img.at(x, y as i32)).collect();
            let r: Vec<[f32; 3]> = (rs.right.0..rs.right.1).map(|x| img.at(x, y as i32)).collect();
            (median_rgb(&l), median_rgb(&r))
        })
        .collect();
    // smooth the rows a little (3 px box): the references are noisy
    let sm = |y: usize, side: usize| {
        let mut a = [0.0; 3];
        let mut n = 0.0;
        for yy in y.saturating_sub(1)..(y + 2).min(img.h) {
            let v = if side == 0 { rows[yy].0 } else { rows[yy].1 };
            for k in 0..3 {
                a[k] += v[k];
            }
            n += 1.0;
        }
        [a[0] / n, a[1] / n, a[2] / n]
    };
    let (xl, xr) = (((rs.left.0 + rs.left.1) / 2) as f32, ((rs.right.0 + rs.right.1) / 2) as f32);
    Img::new(img.w, img.h, |x, y| {
        let t = ((x as f32 - xl) / (xr - xl).max(1.0)).clamp(0.0, 1.0);
        let (l, r) = (sm(y, 0), sm(y, 1));
        [l[0] + (r[0] - l[0]) * t, l[1] + (r[1] - l[1]) * t, l[2] + (r[2] - l[2]) * t]
    })
}

// ---------------------------------------------------------------- rendering a glass card over a backdrop image

struct Prepared {
    spec: RefSpec,
    reference: Img,
    ref_feat: Features,
    backdrop: ID2D1Bitmap1,
    scale: f32,
    margin: i32,
    lum: f32,
    canvas: ID2D1Bitmap1,
    cpu: ID2D1Bitmap1,
}

pub struct Calib {
    gfx: Gfx,
    refs: Vec<Prepared>,
}

impl Calib {
    pub fn new(dir: &str) -> std::result::Result<Calib, String> {
        let gfx = Gfx::new(None).map_err(|e| format!("device: {e}"))?;
        let mut refs = Vec::new();
        for spec in references() {
            let path = format!("{dir}/{}", spec.file);
            let d = crate::sys::decode_image_file(&path, 8192, 8192).ok_or_else(|| format!("cannot read {path}"))?;
            let reference = Img::from_bgra(d.w as usize, d.h as usize, &d.bgra);
            let bd = synth_backdrop(&reference, &spec);
            let mut bgra = Vec::with_capacity(bd.w * bd.h * 4);
            for p in &bd.px {
                bgra.extend_from_slice(&[p[2].round() as u8, p[1].round() as u8, p[0].round() as u8, 255]);
            }
            let backdrop = gfx.from_bgra(bd.w as u32, bd.h as u32, &bgra).map_err(|e| format!("backdrop: {e}"))?;
            let scale = (spec.card.2 as f32 / spec.size.0 + spec.card.3 as f32 / spec.size.1) / 2.0;
            let (cx, cy, cw, ch) = spec.card;
            let mut s = 0.0;
            for y in cy..cy + ch {
                for x in cx..cx + cw {
                    let p = bd.at(x, y);
                    s += 0.2126 * p[0] + 0.7152 * p[1] + 0.0722 * p[2];
                }
            }
            let lum = s / (cw * ch) as f32 / 255.0;
            let ref_feat = features(&reference, &spec, spec.radius * scale);
            refs.push(Prepared {
                canvas: gfx.target(d.w, d.h).map_err(|e| format!("canvas: {e}"))?,
                cpu: gfx.cpu_read(d.w, d.h).map_err(|e| format!("staging: {e}"))?,
                margin: (48.0 * scale).round() as i32,
                spec,
                reference,
                ref_feat,
                backdrop,
                scale,
                lum,
            });
        }
        Ok(Calib { gfx, refs })
    }

    /// Render reference `i` with `p`: the glass over the synthetic backdrop at the reference size.
    fn render(&self, i: usize, p: &GlassParams) -> Result<Img> {
        let r = &self.refs[i];
        let gfx = &self.gfx;
        let (x, y, w, h) = r.spec.card;
        let (cw, ch) = (r.reference.w as i32, r.reference.h as i32);
        let m = r.margin;
        let map = Glass::make_map(gfx, w, h, r.scale, r.spec.radius, p)?;
        let style = GlassStyle { blur: r.spec.blur, veil: None };
        let glass = Glass::styled(gfx, &r.backdrop, x, y, w, h, r.scale, m, 0.5, r.spec.radius, &map, style, p)?;
        let layer = gfx.target((w + 2 * m) as u32, (h + 2 * m) as u32)?;
        let dc = &gfx.dc;
        unsafe {
            dc.SetTarget(&layer);
            dc.BeginDraw();
            dc.SetTransform(&mat(1.0, 0.0, 0.0));
            let e = glass.draw(dc, &Fx { lum: r.lum, k: r.spec.look, blur: 0.0 });
            let e2 = dc.EndDraw(None, None);
            dc.SetTarget(None::<&ID2D1Image>);
            e?;
            e2?;
            dc.SetTarget(&r.canvas);
            dc.BeginDraw();
            dc.SetTransform(&mat(1.0, 0.0, 0.0));
            dc.DrawImage(&r.backdrop, None, None, D2D1_INTERPOLATION_MODE_NEAREST_NEIGHBOR, D2D1_COMPOSITE_MODE_SOURCE_COPY);
            dc.DrawImage(&layer, Some(&pt((x - m) as f32, (y - m) as f32)), None, D2D1_INTERPOLATION_MODE_NEAREST_NEIGHBOR, D2D1_COMPOSITE_MODE_SOURCE_OVER);
            glass.draw_hi(dc, x as f32, y as f32, &Hi { angle: p.light_angle, k: r.spec.look, ..Hi::default() });
            let e = dc.EndDraw(None, None);
            dc.SetTarget(None::<&ID2D1Image>);
            e?;
        }
        let mut bgra = vec![0u8; (cw * ch * 4) as usize];
        gfx.read_rect(&r.canvas, &r.cpu, RECT { left: 0, top: 0, right: cw, bottom: ch }, |yy, row| {
            let o = yy as usize * cw as usize * 4;
            bgra[o..o + row.len()].copy_from_slice(row);
        })?;
        Ok(Img::from_bgra(cw as usize, ch as usize, &bgra))
    }

    /// (weighted total, per reference (name, distance, parts))
    pub fn evaluate(&self, p: &GlassParams) -> (f32, Vec<(&'static str, f32, [f32; 3])>) {
        let mut tot = 0.0;
        let mut wsum = 0.0;
        let mut rows = Vec::new();
        for (i, r) in self.refs.iter().enumerate() {
            let (d, parts) = match self.render(i, p) {
                Ok(img) => distance(&r.ref_feat, &features(&img, &r.spec, r.spec.radius * r.scale)),
                Err(e) => {
                    eprintln!("calib: render {}: {e}", r.spec.name);
                    (9.0, [9.0; 3])
                }
            };
            tot += d * r.spec.weight;
            wsum += r.spec.weight;
            rows.push((r.spec.name, d, parts));
        }
        (tot / wsum.max(1e-6), rows)
    }

    /// Side-by-side PNG per reference: reference | render | |difference| x4 (card region plus margin).
    pub fn dump(&self, p: &GlassParams, dir: &str, tag: &str) -> Result<()> {
        let _ = std::fs::create_dir_all(dir);
        for (i, r) in self.refs.iter().enumerate() {
            let img = self.render(i, p)?;
            let (x, y, w, h) = r.spec.card;
            let pad = 24;
            let (x0, y0) = ((x - pad).max(0), (y - pad).max(0));
            let (x1, y1) = ((x + w + pad).min(img.w as i32), (y + h + pad).min(img.h as i32));
            let (cw, chh) = ((x1 - x0) as usize, (y1 - y0) as usize);
            let out = Img::new(cw * 3 + 8, chh, |ox, oy| {
                let (panel, lx) = (ox / (cw + 4), ox % (cw + 4));
                if lx >= cw {
                    return [0.0; 3];
                }
                let (sx, sy) = (x0 + lx as i32, y0 + oy as i32);
                let (a, b) = (r.reference.at(sx, sy), img.at(sx, sy));
                match panel {
                    0 => a,
                    1 => b,
                    _ => [((a[0] - b[0]).abs() * 4.0).min(255.0), ((a[1] - b[1]).abs() * 4.0).min(255.0), ((a[2] - b[2]).abs() * 4.0).min(255.0)],
                }
            });
            let name: String = r.spec.name.chars().map(|c| if c.is_alphanumeric() { c } else { '_' }).collect();
            out.save(&self.gfx, &format!("{dir}/{tag}_{name}.png"))?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------- the search

/// Parameters the dark-glass fit may move (the light-appearance ones are fitted in their own pass).
///
/// Deliberately NOT searched (kept at their design values): the Liquid-Glass-only features (specular rim, sheen, inner glow, shadow shape,
/// base rim alpha) - the supplied references are older flat-lit widgets that show none of them, so a free fit simply deletes them
/// (measured: the unconstrained fit drives spec, sheen, glow and the shadow to zero) - and the refraction / chromatic constants, which no
/// reference constrains (no grid or text close-up crosses a card edge).
pub const DARK_SET: &[&str] = &["bright", "sat", "blur", "tint0", "tint1", "veil_gain", "veil_thresh", "veil_max", "rim_w", "edge_dark", "glow_sigma"];
pub const LIGHT_SET: &[&str] = &["milk", "light_tint", "light_bright", "light_rim", "light_shadow"];

/// Objective: mean distance + a prior that keeps parameters near their defaults (0.004 x the mean squared normalised deviation).
fn objective(calib: &Calib, p: &GlassParams, only_look: Option<f32>) -> f32 {
    let (_, rows) = calib.evaluate_filtered(p, only_look);
    let d = GlassParams::default();
    let mut prior = 0.0;
    for n in GlassParams::NAMES {
        let (lo, hi) = GlassParams::range(n).unwrap_or((0.0, 1.0));
        let dev = (p.get(n).unwrap_or(0.0) - d.get(n).unwrap_or(0.0)) / (hi - lo).max(1e-6);
        prior += dev * dev;
    }
    rows + 0.004 * prior / GlassParams::NAMES.len() as f32
}

impl Calib {
    fn evaluate_filtered(&self, p: &GlassParams, only_look: Option<f32>) -> (f32, f32) {
        let mut tot = 0.0;
        let mut wsum = 0.0;
        for (i, r) in self.refs.iter().enumerate() {
            if only_look.is_some_and(|l| (l - r.spec.look).abs() > 0.5) {
                continue;
            }
            let d = match self.render(i, p) {
                Ok(img) => distance(&r.ref_feat, &features(&img, &r.spec, r.spec.radius * r.scale)).0,
                Err(_) => 9.0,
            };
            tot += d * r.spec.weight;
            wsum += r.spec.weight;
        }
        (0.0, tot / wsum.max(1e-6))
    }
}

/// Coordinate descent over `names` starting at `start`; returns the best parameters and the objective trace.
pub fn fit(calib: &Calib, start: GlassParams, names: &[&str], rounds: usize, only_look: Option<f32>) -> (GlassParams, Vec<f32>) {
    let mut cur = start;
    let mut best = objective(calib, &cur, only_look);
    let mut trace = vec![best];
    for round in 0..rounds {
        let frac = 0.25 * 0.5f32.powi(round as i32);
        for n in names {
            let Some((lo, hi)) = GlassParams::range(n) else { continue };
            let step = (hi - lo) * frac;
            for dir in [1.0f32, -1.0] {
                let mut moved = false;
                loop {
                    let mut cand = cur;
                    let v = (cur.get(n).unwrap_or(0.0) + dir * step).clamp(lo, hi);
                    if (v - cur.get(n).unwrap_or(0.0)).abs() < 1e-6 {
                        break;
                    }
                    cand.set(n, v);
                    let o = objective(calib, &cand, only_look);
                    if o < best - 1e-6 {
                        best = o;
                        cur = cand;
                        moved = true;
                    } else {
                        break;
                    }
                }
                if moved {
                    break; // do not try the opposite direction after a gain
                }
            }
        }
        trace.push(best);
        eprintln!("calib: round {} objective {best:.5}", round + 1);
    }
    (cur, trace)
}

// ---------------------------------------------------------------- CLI

pub struct CalibArgs {
    pub mode: String,
    pub dir: String,
    pub out: Option<String>,
    pub params: Option<String>,
    pub rounds: usize,
    pub dump: Option<String>,
}

fn print_table(title: &str, tot: f32, rows: &[(&'static str, f32, [f32; 3])]) {
    println!("{title}: weighted distance {tot:.4}");
    for (n, d, p) in rows {
        println!("  {n:<24} {d:.4}  (edges {:.4}, rows {:.4}, corners {:.4})", p[0], p[1], p[2]);
    }
}

/// `--calib eval|fit|scan-light|defaults`.
pub fn run(a: &CalibArgs) -> std::result::Result<(), String> {
    let calib = Calib::new(&a.dir)?;
    let mut start = GlassParams::default();
    if let Some(p) = &a.params {
        start = GlassParams::load(std::path::Path::new(p))?;
    }
    let (t0, r0) = calib.evaluate(&start);
    print_table("start", t0, &r0);
    if let Some(d) = &a.dump {
        calib.dump(&start, d, "before").map_err(|e| e.to_string())?;
    }
    match a.mode.as_str() {
        "eval" => {}
        "defaults" => println!("{}", serde_json::to_string_pretty(&GlassParams::default().to_json()).map_err(|e| e.to_string())?),
        "scan-light" => {
            println!("light_angle scan (lower is better):");
            for deg in (0..360).step_by(30) {
                let mut p = start;
                p.light_angle = deg as f32;
                println!("  {deg:>3}  {:.4}", calib.evaluate(&p).0);
            }
        }
        "fit" => {
            let (p1, _) = fit(&calib, start, DARK_SET, a.rounds, Some(0.0));
            let (p2, _) = fit(&calib, p1, LIGHT_SET, a.rounds.min(4), Some(1.0));
            let (t1, r1) = calib.evaluate(&p2);
            print_table("fitted", t1, &r1);
            println!("improvement {:.1} %", (t0 - t1) / t0.max(1e-6) * 100.0);
            let json = serde_json::to_string_pretty(&p2.to_json()).map_err(|e| e.to_string())?;
            println!("{json}");
            if let Some(o) = &a.out {
                std::fs::write(o, json).map_err(|e| format!("{o}: {e}"))?;
                eprintln!("calib: wrote {o}");
            }
            if let Some(d) = &a.dump {
                calib.dump(&p2, d, "after").map_err(|e| e.to_string())?;
            }
        }
        m => return Err(format!("--calib expects eval|fit|scan-light|defaults, got '{m}'")),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> RefSpec {
        RefSpec { name: "t", file: "", card: (20, 20, 100, 100), size: (100.0, 100.0), radius: 20.0, left: (0, 10), right: (130, 140), look: 0.0, blur: None, edges: [true; 4], corners: [true; 4], weight: 1.0 }
    }

    /// background gradient + a card that is darker with a bright 1 px rim
    fn synthetic(card_dark: f32, rim: f32) -> Img {
        Img::new(160, 140, |x, y| {
            let bg = 120.0 + y as f32 * 0.3;
            let inside = x >= 20 && x < 120 && y >= 20 && y < 120;
            let edge = inside && (x == 20 || x == 119 || y == 20 || y == 119);
            let v = if edge { bg + rim } else if inside { bg - card_dark } else { bg };
            [v, v * 0.9, v * 0.7]
        })
    }

    #[test]
    fn identical_images_have_zero_distance() {
        let a = synthetic(30.0, 60.0);
        let s = spec();
        let (d, parts) = distance(&features(&a, &s, 20.0), &features(&a, &s, 20.0));
        assert_eq!(d, 0.0);
        assert!(parts.iter().all(|&p| p == 0.0));
    }

    #[test]
    fn distance_grows_with_tint_and_rim_errors() {
        let s = spec();
        let r = features(&synthetic(30.0, 60.0), &s, 20.0);
        let d_tint1 = distance(&r, &features(&synthetic(40.0, 60.0), &s, 20.0)).0;
        let d_tint2 = distance(&r, &features(&synthetic(60.0, 60.0), &s, 20.0)).0;
        assert!(d_tint1 > 0.0 && d_tint2 > d_tint1, "{d_tint1} {d_tint2}");
        let (d_rim, parts) = distance(&r, &features(&synthetic(30.0, 10.0), &s, 20.0));
        assert!(d_rim > 0.0 && parts[0] > parts[1], "a rim error shows in the edge profiles first: {parts:?}");
    }

    #[test]
    fn synthetic_backdrop_reproduces_a_vertical_gradient() {
        // a wallpaper that only varies with y: the rebuilt backdrop must match it away from the card
        let wall = Img::new(160, 140, |_, y| [100.0 + y as f32, 80.0, 60.0]);
        let b = synth_backdrop(&wall, &spec());
        for y in [10usize, 70, 130] {
            let p = b.at(75, y as i32);
            assert!((p[0] - (100.0 + y as f32)).abs() < 1.5, "y={y}: {p:?}");
        }
        // horizontal ramp between the two strips
        let ramp = Img::new(160, 140, |x, _| [x as f32, 0.0, 0.0]);
        let b = synth_backdrop(&ramp, &spec());
        assert!((b.at(70, 50)[0] - 70.0).abs() < 2.0 && (b.at(0, 50)[0] - 5.0).abs() < 2.0 && (b.at(159, 50)[0] - 135.0).abs() < 2.0);
    }

    #[test]
    fn edge_profile_finds_the_rim_row() {
        let s = spec();
        let f = features(&synthetic(30.0, 60.0), &s, 20.0);
        // left edge: index OUT_PX is the first pixel inside the card (the rim)
        let rim = f.edges[0][OUT_PX as usize][0];
        let out = f.edges[0][0][0];
        let inner = f.edges[0][(OUT_PX + 10) as usize][0];
        assert!(rim > out && rim > inner + 40.0, "rim {rim} out {out} inner {inner}");
        assert_eq!(f.edges[0].len(), (OUT_PX + IN_PX) as usize);
        assert!(!f.rows.is_empty() && f.corners[0].len() == (OUT_PX + IN_PX) as usize);
    }

    #[test]
    fn median_ignores_text_like_outliers() {
        let mut v = vec![10.0, 10.0, 10.0, 255.0, 10.0, 255.0, 10.0];
        assert_eq!(median(&mut v), 10.0);
    }
}
