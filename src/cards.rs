//! The generic card system: for every layout item a "layer" bitmap (card + 48 px margin) holding shadow + glass + the widget's static
//! content (a command list recorded by `Widget::draw`), plus the per-frame overlay (`Widget::overlay`) painted straight into the frame
//! over the cached layers. A layer is redrawn only when the backdrop behind that card changed, the widget's `version()` changed or
//! the day changed, so a 30 fps animation never re-runs the glass effect graph.
//!
//! The scene canvas is the whole MONITOR (card coordinates are monitor px); the layered window only presents a sub-rectangle of it.
//! Cards can be added, removed, moved and resized at run time (drag & drop, gallery, size menu); a card with a non-identity `Xf`
//! (jiggle, lift, pop) is first composed upright into its own scratch bitmap and then drawn rotated / scaled / faded.
use crate::gfx::*;
use crate::glass::{self, contour_point, nearest_contour, tl_corner_u, Fx, Glass, GlassParams, GlassStyle, Hi};
use crate::motion::{self, Appearance, AppearancePref, AppearanceState, LightSpring};
pub use crate::grid::{ORIGIN, PITCH};
use crate::layout::Item;
use crate::registry;
use crate::widget::*;
use std::collections::HashMap;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::time::{Duration, Instant};
use windows::core::Result;
use windows::Foundation::Numerics::Matrix3x2;
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;

pub const CELL: f32 = 170.0;
/// shadow margin of a card layer, logical px
pub const MARGIN: f32 = 48.0;

#[derive(Clone, Debug)]
pub struct CardSpec {
    pub id: String,
    pub type_id: String,
    pub size: Size,
    pub cfg: Value,
    /// physical px, relative to the monitor (or to the canvas in the dump modes)
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}
impl CardSpec {
    pub fn rect(&self) -> RECT {
        RECT { left: self.x, top: self.y, right: self.x + self.w, bottom: self.y + self.h }
    }
}

/// Physical-px rectangle (x, y, w, h) of a card of `size` at grid cell (col, row).
pub fn cell_rect(size: Size, col: i32, row: i32, s: f32) -> (i32, i32, i32, i32) {
    let (lw, lh) = size.logical();
    (((ORIGIN.0 + col as f32 * PITCH) * s).round() as i32, ((ORIGIN.1 + row as f32 * PITCH) * s).round() as i32, (lw * s).round() as i32, (lh * s).round() as i32)
}

/// Grid cells -> physical-px card rectangles.
pub fn specs(items: &[Item], s: f32) -> Vec<CardSpec> {
    items
        .iter()
        .map(|it| {
            let (x, y, w, h) = cell_rect(it.size, it.col as i32, it.row as i32, s);
            CardSpec { id: it.id.clone(), type_id: it.type_id.clone(), size: it.size, cfg: it.cfg.clone(), x, y, w, h }
        })
        .collect()
}

pub fn union(a: RECT, b: RECT) -> RECT {
    RECT { left: a.left.min(b.left), top: a.top.min(b.top), right: a.right.max(b.right), bottom: a.bottom.max(b.bottom) }
}
pub fn clip(r: RECT, w: i32, h: i32) -> RECT {
    RECT { left: r.left.clamp(0, w), top: r.top.clamp(0, h), right: r.right.clamp(0, w), bottom: r.bottom.clamp(0, h) }
}
fn hit(a: &RECT, b: &RECT) -> bool {
    a.left < b.right && b.left < a.right && a.top < b.bottom && b.top < a.bottom
}

// ---------------------------------------------------------------- scheduling

/// Next wake-up for a card that just ran `tick` (`prev` = its previous due time).
fn schedule(t: Tick, prev: Option<Instant>) -> Option<Instant> {
    let now = Instant::now();
    let period = |d: Duration| {
        let d = d.max(Duration::from_millis(10));
        Some(match prev {
            Some(p) if p > now + Duration::from_millis(3) => p, // woken early (job / mouse): keep the schedule
            Some(p) if p + d > now => p + d,                    // on time: no drift
            _ => now + d,                                       // fell behind
        })
    };
    match t {
        Tick::Never => None,
        Tick::Every(d) | Tick::Poll(d) => period(d),
        Tick::Fps(n) => period(Duration::from_micros(1_000_000 / n.clamp(1, 60) as u64)),
        Tick::Second => Some(now + Duration::from_millis(1008 - Now::local().ms as u64)),
        Tick::Minute => {
            let n = Now::local();
            Some(now + Duration::from_millis((60 - n.sec as u64) * 1000 - n.ms as u64 + 30))
        }
    }
}

// ---------------------------------------------------------------- scene

pub struct SceneEnv {
    /// gallery / sheet / dump: sample data, no I/O, overlay always painted
    pub preview: bool,
    /// window the job waker posts to (0 = none)
    pub hwnd: isize,
    pub lang: Lang,
    pub utc_off: i32,
}

struct Shared {
    res: Res,
    s: f32,
    preview: bool,
    hwnd: isize,
    lang: Lang,
    utc_off: i32,
}

/// Visual transform of a card (edit-mode jiggle, drag lift, pop in / out). Identity = the fast path.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Xf {
    /// degrees, clockwise, around the card centre
    pub rot: f32,
    pub scale: f32,
    pub alpha: f32,
    /// 0..1: how far the card is lifted off the desk (extra shadow)
    pub lift: f32,
    /// 0..1 materialise diffusion: the lens blur grows towards `motion::BLUR_MAX` and the composed card is softened
    pub blur: f32,
    /// materialise rim sweep progress 0..1 (negative = none)
    pub sweep: f32,
}
impl Xf {
    pub const ID: Xf = Xf { rot: 0.0, scale: 1.0, alpha: 1.0, lift: 0.0, blur: 0.0, sweep: -1.0 };
    pub fn is_id(&self) -> bool {
        self.rot.abs() < 0.004 && (self.scale - 1.0).abs() < 0.0005 && (self.alpha - 1.0).abs() < 0.002 && self.lift <= 0.0 && self.blur < 0.002 && self.sweep < 0.0
    }
}

pub struct CardView {
    pub id: String,
    pub type_id: String,
    pub size: Size,
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    /// stable handle (indices shift when cards are removed; job wake-ups carry this)
    pub uid: usize,
    pub xf: Xf,
    glass: Glass,
    layer: ID2D1Bitmap1,
    /// grayscale + darken of the layer ("Dim widgets on desktop"), created while the dim is on
    dimfx: Option<(ID2D1Effect, ID2D1Image)>,
    scratch: Option<ID2D1Bitmap1>,
    scratch_ok: bool,
    /// materialise softening of the scratch image (created on demand next to the scratch)
    soften: Option<(ID2D1Effect, ID2D1Effect)>,
    /// recorded static content: [dark ink, light ink]; only the looks that are on screen are recorded
    content: [Option<ID2D1CommandList>; 2],
    key: (u64, u32),
    /// appearance: 0 dark .. 1 light (cross-fades in 250 ms), the backdrop-driven state with hysteresis and an explicit override
    look: f32,
    app: AppearanceState,
    forced: Option<Appearance>,
    /// inertial light of this card, its drive (opposite to the velocity), pointer glint (current / goal / contour point, card px)
    light: LightSpring,
    light_drive: (f32, f32),
    light_settled: bool,
    glint: f32,
    glint_goal: f32,
    glint_pt: (f32, f32),
    /// the glass layer has to be re-rendered before the next compose (materialise blur changed)
    layer_stale: bool,
    widget: Box<dyn Widget>,
    pub cfg: Value,
    flags: Flags,
    pub tick: Tick,
    due: Option<Instant>,
    hover: Option<Pt>,
    tshadow: ID2D1Effect,
    lum: f32,
    lum_known: bool,
    err_logged: bool,
    frame_once: bool,
}

/// A glass-only surface (no widget): the gallery panel and the settings strip. Drawn by the host between the cards and its own UI.
pub struct GlassPanel {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    radius: f32,
    glass: Glass,
    layer: ID2D1Bitmap1,
    lum: f32,
}

#[derive(Clone, Copy, Debug)]
pub struct PanelSpec {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    /// logical px
    pub radius: f32,
    pub g: f32,
}

pub struct Scene {
    pub w: i32,
    pub h: i32,
    s: f32,
    m: i32,
    g: f32,
    pub cards: Vec<CardView>,
    pub panels: [Option<GlassPanel>; 2],
    sh: Shared,
    backdrop: ID2D1Bitmap1,
    maps: HashMap<(i32, i32, u32), ID2D1Bitmap1>,
    next_uid: usize,
    dim: bool,
    frame: ID2D1Bitmap1,
    /// CPU read-back staging bitmap (grown on demand, only the dirty rectangle is ever copied through it)
    cpu: Option<(i32, i32, ID2D1Bitmap1)>,
    probe: Probe,
    pub params: GlassParams,
    /// global appearance preference (`widgetsAppearance` / `--appearance`)
    pref: AppearancePref,
    /// merging neighbours (index, index, amount) and the liquid neck renderer
    merge: Vec<(usize, usize, f32)>,
    liquid: Option<crate::liquid::Liquid>,
    fx_wake: bool,
    lift: Option<((i32, i32, u32), ID2D1Effect, ID2D1CommandList)>,
    /// (text, x, y) in canvas px, drawn small above cards by `--sheet`
    pub labels: Vec<(String, f32, f32)>,
}

/// 8x8 GPU downscale of the backdrop under a card, read back (256 bytes) at most twice a second.
struct Probe {
    scale: ID2D1Effect,
    out: ID2D1Image,
    tgt: ID2D1Bitmap1,
    cpu: ID2D1Bitmap1,
}

/// Run `f` with a `Cx` for this card; panics inside the widget are caught (needs panic=unwind, set in Cargo.toml).
fn with_cx<R>(c: &mut CardView, sh: &Shared, gfx: &Gfx, now: &Now, base: Matrix3x2, light: bool, what: &str, f: impl FnOnce(&mut dyn Widget, &mut Cx) -> R) -> Option<R> {
    let CardView { widget, cfg, flags, size, hover, lum, uid, type_id, err_logged, .. } = c;
    let mut cx = Cx::new(CxParts {
        gfx,
        res: &sh.res,
        flags,
        cfg,
        size: *size,
        scale: sh.s,
        now: *now,
        preview: sh.preview,
        lum: *lum,
        lang: sh.lang,
        hover: *hover,
        waker: Waker { hwnd: sh.hwnd, idx: *uid },
        utc_off: sh.utc_off,
        base,
        light,
    });
    unsafe { gfx.dc.SetTransform(&base) };
    let r = catch_unwind(AssertUnwindSafe(|| f(&mut **widget, &mut cx)));
    cx.finish();
    match r {
        Ok(v) => Some(v),
        Err(_) => {
            if !*err_logged {
                *err_logged = true;
                eprintln!("mswidgets: widget '{type_id}' panicked in {what}");
            }
            None
        }
    }
}

/// The card transform of a materialisation state (+ the drag lift scale).
pub fn mat_xf(st: &motion::MatState, rot: f32, lift: f32) -> Xf {
    Xf { rot, scale: st.scale * (1.0 + 0.03 * lift), alpha: st.alpha, lift, blur: st.blur, sweep: st.sweep.unwrap_or(-1.0) }
}

/// Rotation + scale about the centre (cx, cy), row-vector convention like Direct2D.
pub fn xf_matrix(cx: f32, cy: f32, k: f32, rot_deg: f32) -> Matrix3x2 {
    let (s, c) = rot_deg.to_radians().sin_cos();
    Matrix3x2 { M11: k * c, M12: k * s, M21: -k * s, M22: k * c, M31: cx - cx * k * c + cy * k * s, M32: cy - cx * k * s - cy * k * c }
}

/// The highlight inputs of a card: light direction (base + inertia), appearance, glint, materialise sweep.
fn hi_of(c: &CardView, base_angle: f32) -> Hi {
    let (w, h) = c.glass.size_px();
    let r = c.glass.radius_px();
    let sweep = (c.xf.sweep >= 0.0).then(|| {
        let p = contour_point(w, h, r, tl_corner_u(w, h, r) + c.xf.sweep);
        (p.0, p.1, (std::f32::consts::PI * c.xf.sweep).sin().max(0.0).powf(0.7))
    });
    Hi { angle: c.light.angle(base_angle), k: c.look, glint: (c.glint > 0.004).then_some((c.glint_pt.0, c.glint_pt.1, c.glint)), sweep, alpha: 1.0 }
}

/// Card rectangle in canvas px as drawn: scaled about its centre by the transform (lift / materialise).
fn eff_rect(c: &CardView) -> motion::R4 {
    let (w, h) = (c.w as f32 * c.xf.scale, c.h as f32 * c.xf.scale);
    (c.x as f32 + (c.w as f32 - w) / 2.0, c.y as f32 + (c.h as f32 - h) / 2.0, w, h)
}

/// The neck inputs of a merging pair: rects as drawn, corner radii, the lens and the tint / veils of the cards.
fn neck_in<'a>(cards: &'a [CardView], p: &GlassParams, a: usize, b: usize, amount: f32) -> crate::liquid::NeckIn<'a> {
    let (ca, cb) = (&cards[a], &cards[b]);
    let k = (ca.look + cb.look) / 2.0;
    let lum = (ca.lum + cb.lum) / 2.0;
    let g = ca.glass.slider();
    let tint = (p.tint0 + p.tint0_g * g + p.tint1 + p.tint1_g * g) / 2.0 * (1.0 + (p.light_tint - 1.0) * k);
    let dark = ((lum - p.veil_thresh) * p.veil_gain).clamp(0.0, p.veil_max) * (0.4 + 0.9 * g) * (1.0 - k);
    crate::liquid::NeckIn {
        a: eff_rect(ca),
        b: eff_rect(cb),
        ra: ca.glass.radius_px() * ca.xf.scale,
        rb: cb.glass.radius_px() * cb.xf.scale,
        lens: ca.glass.lens().0,
        tint,
        dark,
        milk: p.milk * k,
        k,
        rim: p.rim_base * (1.0 + (p.light_rim - 1.0) * k),
        rim_w: p.rim_w,
        amount,
    }
}

fn pair_hits(n: &crate::liquid::NeckIn, clip: &RECT) -> bool {
    let u = crate::liquid::union_rect(n.a, n.b);
    hit(&RECT { left: u.0 as i32 - 4, top: u.1 as i32 - 4, right: (u.0 + u.2) as i32 + 4, bottom: (u.1 + u.3) as i32 + 4 }, clip)
}

/// Record the static content of `c` for one ink (light = dark ink on milky glass).
fn record_content(c: &mut CardView, sh: &Shared, gfx: &Gfx, now: &Now, s: f32, m: f32, light: bool) -> Result<ID2D1CommandList> {
    gfx.record(|_| {
        let r = with_cx(c, sh, gfx, now, mat(s, m, m), light, "draw", |w, cx| w.draw(cx));
        if let Some(Err(e)) = r {
            if !c.err_logged {
                c.err_logged = true;
                eprintln!("mswidgets: widget '{}' draw failed: {e}", c.type_id);
            }
        }
        Ok(())
    })
}

/// Push a group layer with an opacity (cross-fades); pop with `dc.PopLayer()`.
unsafe fn push_opacity(dc: &ID2D1DeviceContext, a: f32) {
    let lp = D2D1_LAYER_PARAMETERS1 {
        contentBounds: rf(-3.0e38, -3.0e38, 3.0e38, 3.0e38),
        geometricMask: std::mem::ManuallyDrop::new(None),
        maskAntialiasMode: D2D1_ANTIALIAS_MODE_PER_PRIMITIVE,
        maskTransform: mat(1.0, 0.0, 0.0),
        opacity: a,
        opacityBrush: std::mem::ManuallyDrop::new(None),
        layerOptions: D2D1_LAYER_OPTIONS1_NONE,
    };
    dc.PushLayer(&lp, None::<&ID2D1Layer>);
}

impl Scene {
    /// An empty scene on a `w` x `h` px canvas over `backdrop` (canvas-sized).
    pub fn empty(gfx: &Gfx, w: i32, h: i32, s: f32, glass_g: f32, backdrop: &ID2D1Bitmap1, env: &SceneEnv) -> Result<Scene> {
        let sh = Shared { res: Res::new(gfx)?, s, preview: env.preview, hwnd: env.hwnd, lang: env.lang, utc_off: env.utc_off };
        Ok(Scene {
            w,
            h,
            s,
            m: (MARGIN * s).round() as i32,
            g: glass_g,
            cards: Vec::new(),
            panels: [None, None],
            sh,
            backdrop: backdrop.clone(),
            maps: HashMap::new(),
            next_uid: 1,
            dim: false,
            frame: gfx.target(w as u32, h as u32)?,
            cpu: None,
            probe: Probe::new(gfx, backdrop)?,
            params: crate::glass::global_params(),
            pref: AppearancePref::Auto,
            merge: Vec::new(),
            liquid: None,
            fx_wake: false,
            lift: None,
            labels: Vec::new(),
        })
    }

    pub fn new(gfx: &Gfx, w: i32, h: i32, s: f32, glass_g: f32, backdrop: &ID2D1Bitmap1, specs: &[CardSpec], env: &SceneEnv) -> Result<Scene> {
        let mut sc = Scene::empty(gfx, w, h, s, glass_g, backdrop, env)?;
        let reg = registry::global();
        for c in specs {
            if reg.get(&c.type_id).is_none() {
                eprintln!("mswidgets: unknown widget type '{}' skipped", c.type_id);
                continue;
            }
            sc.add_card(gfx, c)?;
        }
        Ok(sc)
    }

    pub fn len(&self) -> usize {
        self.cards.len()
    }
    pub fn scale(&self) -> f32 {
        self.s
    }
    pub fn margin(&self) -> i32 {
        self.m
    }
    pub fn glass_value(&self) -> f32 {
        self.g
    }
    pub fn index_of_uid(&self, uid: usize) -> Option<usize> {
        self.cards.iter().position(|c| c.uid == uid)
    }
    pub fn index_of_id(&self, id: &str) -> Option<usize> {
        self.cards.iter().position(|c| c.id == id)
    }
    /// Rectangles of the cards (canvas px), for hit-testing.
    pub fn card_rects(&self) -> Vec<RECT> {
        self.cards.iter().map(|c| RECT { left: c.x, top: c.y, right: c.x + c.w, bottom: c.y + c.h }).collect()
    }
    /// Card layer rectangle (card + shadow margin) clipped to the canvas.
    pub fn layer_rect(&self, i: usize) -> RECT {
        self.layer_rect_ext(i, 0)
    }
    /// Same with `extra` more px on every side (lifted / rotated cards draw outside their layer a little).
    pub fn layer_rect_ext(&self, i: usize, extra: i32) -> RECT {
        let c = &self.cards[i];
        let e = self.m + extra;
        clip(RECT { left: c.x - e, top: c.y - e, right: c.x + c.w + e, bottom: c.y + c.h + e }, self.w, self.h)
    }
    /// The card rectangle itself (canvas px): what an animating overlay repaints.
    pub fn card_rect(&self, i: usize) -> RECT {
        let c = &self.cards[i];
        RECT { left: c.x, top: c.y, right: c.x + c.w, bottom: c.y + c.h }
    }
    pub fn lum(&self, i: usize) -> f32 {
        self.cards[i].lum
    }
    pub fn type_id(&self, i: usize) -> &str {
        &self.cards[i].type_id
    }
    pub fn frame_bitmap(&self) -> &ID2D1Bitmap1 {
        &self.frame
    }

    // ------------------------------------------------ run-time editing

    fn gpu(&mut self, gfx: &Gfx, size: Size, x: i32, y: i32, w: i32, h: i32) -> Result<(Glass, ID2D1Bitmap1, ID2D1Effect)> {
        let (s, m, g) = (self.s, self.m, self.g);
        let radius = size.radius();
        let key = (w, h, radius.to_bits());
        if !self.maps.contains_key(&key) {
            self.maps.insert(key, Glass::make_map(gfx, w, h, s, radius, &self.params)?);
        }
        let glass = Glass::new(gfx, &self.backdrop, x, y, w, h, s, m, g, radius, &self.maps[&key], &self.params)?;
        let layer = gfx.target((w + 2 * m) as u32, (h + 2 * m) as u32)?;
        let tshadow = unsafe { gfx.dc.CreateEffect(&CLSID_D2D1Shadow)? };
        fx_f32(&tshadow, 0, 4.0 * s)?;
        Ok((glass, layer, tshadow))
    }

    /// Append a card; returns its index. The widget is created fresh (its first `tick` still has to be run by the caller).
    pub fn add_card(&mut self, gfx: &Gfx, c: &CardSpec) -> Result<usize> {
        let Some(def) = registry::global().get(&c.type_id) else { return Err(windows::core::Error::from(windows::Win32::Foundation::E_INVALIDARG)) };
        let (glass, layer, tshadow) = self.gpu(gfx, c.size, c.x, c.y, c.w, c.h)?;
        let uid = self.next_uid;
        self.next_uid += 1;
        self.cards.push(CardView {
            id: c.id.clone(),
            type_id: c.type_id.clone(),
            size: c.size,
            x: c.x,
            y: c.y,
            w: c.w,
            h: c.h,
            uid,
            xf: Xf::ID,
            glass,
            layer,
            dimfx: None,
            scratch: None,
            scratch_ok: false,
            soften: None,
            content: [None, None],
            key: (u64::MAX, 0),
            look: 0.0,
            app: AppearanceState::default(),
            forced: c.cfg.get("_appearance").and_then(|v| v.as_str()).and_then(AppearancePref::parse).and_then(|p| match p {
                AppearancePref::Dark => Some(Appearance::Dark),
                AppearancePref::Light => Some(Appearance::Light),
                AppearancePref::Auto => None,
            }),
            light: LightSpring::default(),
            light_drive: (0.0, 0.0),
            light_settled: true,
            glint: 0.0,
            glint_goal: 0.0,
            glint_pt: (0.0, 0.0),
            layer_stale: false,
            widget: (def.make)(),
            cfg: if c.cfg.is_object() { c.cfg.clone() } else { json!({}) },
            flags: Flags::default(),
            tick: Tick::Never,
            due: None,
            hover: None,
            tshadow,
            lum: 0.2,
            lum_known: false,
            err_logged: false,
            frame_once: false,
        });
        let i = self.cards.len() - 1;
        self.cards[i].look = self.look_target(i);
        self.update_merge();
        Ok(i)
    }

    pub fn remove_card(&mut self, i: usize) {
        if i < self.cards.len() {
            self.cards.remove(i);
            self.update_merge();
        }
    }

    /// Move card `i` to canvas px (x, y). The glass samples the backdrop under the new place; the caller re-renders the layer.
    pub fn set_card_pos(&mut self, i: usize, x: i32, y: i32) {
        let c = &mut self.cards[i];
        c.x = x;
        c.y = y;
        c.glass.set_pos(x, y);
        self.update_merge();
    }

    /// New size class for card `i` (a fresh widget instance with the same settings, as the size of a Widget never changes) at (x, y).
    pub fn resize_card(&mut self, gfx: &Gfx, i: usize, size: Size, x: i32, y: i32) -> Result<()> {
        let (type_id, cfg) = (self.cards[i].type_id.clone(), self.cards[i].cfg.clone());
        let Some(def) = registry::global().get(&type_id) else { return Ok(()) };
        let (lw, lh) = size.logical();
        let (w, h) = ((lw * self.s).round() as i32, (lh * self.s).round() as i32);
        let (glass, layer, tshadow) = self.gpu(gfx, size, x, y, w, h)?;
        let c = &mut self.cards[i];
        c.size = size;
        c.x = x;
        c.y = y;
        c.w = w;
        c.h = h;
        c.glass = glass;
        c.layer = layer;
        c.tshadow = tshadow;
        c.dimfx = None;
        c.scratch = None;
        c.scratch_ok = false;
        c.content = [None, None];
        c.soften = None;
        c.key = (u64::MAX, 0);
        c.widget = (def.make)();
        c.cfg = cfg;
        c.flags = Flags::default();
        c.tick = Tick::Never;
        c.due = None;
        c.hover = None;
        c.err_logged = false;
        let t = self.look_target(i);
        self.cards[i].look = t;
        self.update_merge();
        Ok(())
    }

    pub fn set_xf(&mut self, i: usize, xf: Xf) {
        let c = &mut self.cards[i];
        if (c.xf.lift - xf.lift).abs() > 1e-4 || (c.xf.sweep - xf.sweep).abs() > 1e-4 {
            c.scratch_ok = false;
        }
        // the lens sigma follows the materialise diffusion: the glass layer is re-rendered before the next compose
        if (c.xf.blur - xf.blur).abs() > 1e-4 {
            c.layer_stale = true;
            c.scratch_ok = false;
        }
        let moved = (c.xf.scale - xf.scale).abs() > 1e-5;
        c.xf = xf;
        if moved {
            self.update_merge();
        }
    }

    /// The glass slider: tint strength / veil weight of every card, no effect graph rebuild. Layers must be re-rendered by the caller.
    pub fn set_glass(&mut self, gfx: &Gfx, g: f32) -> Result<()> {
        self.g = g;
        for c in &mut self.cards {
            c.glass.set_g(gfx, g)?;
        }
        Ok(())
    }

    /// New canvas / scale / backdrop (monitor or DPI change, live <-> plain glass): every GPU object is rebuilt, the widget instances
    /// (state, settings, jobs) stay. `rects` = the new (x, y, w, h) of every card, in card order.
    pub fn reconfigure(&mut self, gfx: &Gfx, w: i32, h: i32, s: f32, backdrop: &ID2D1Bitmap1, rects: &[(i32, i32, i32, i32)]) -> Result<()> {
        self.w = w;
        self.h = h;
        self.s = s;
        self.sh.s = s;
        self.m = (MARGIN * s).round() as i32;
        self.backdrop = backdrop.clone();
        self.maps.clear();
        self.frame = gfx.target(w as u32, h as u32)?;
        self.cpu = None;
        self.lift = None;
        self.probe = Probe::new(gfx, backdrop)?;
        for i in 0..self.cards.len() {
            let (x, y, cw, ch) = rects.get(i).copied().unwrap_or((self.cards[i].x, self.cards[i].y, self.cards[i].w, self.cards[i].h));
            let size = self.cards[i].size;
            let (glass, layer, tshadow) = self.gpu(gfx, size, x, y, cw, ch)?;
            let c = &mut self.cards[i];
            c.x = x;
            c.y = y;
            c.w = cw;
            c.h = ch;
            c.glass = glass;
            c.layer = layer;
            c.tshadow = tshadow;
            c.dimfx = None;
            c.scratch = None;
            c.scratch_ok = false;
            c.content = [None, None];
        c.soften = None;
            c.key = (u64::MAX, 0);
            c.lum_known = false;
        }
        self.panels = [None, None];
        self.liquid = None;
        self.update_merge();
        Ok(())
    }

    // ------------------------------------------------ glass panels

    /// Create / move / drop panel `slot`. A moved panel only re-aims its glass; call `render_panel` afterwards.
    pub fn set_panel(&mut self, gfx: &Gfx, slot: usize, spec: Option<PanelSpec>, style: GlassStyle) -> Result<()> {
        let Some(p) = spec else {
            self.panels[slot] = None;
            return Ok(());
        };
        if let Some(cur) = self.panels[slot].as_mut() {
            if cur.w == p.w && cur.h == p.h && cur.radius == p.radius {
                if (cur.x, cur.y) != (p.x, p.y) {
                    cur.x = p.x;
                    cur.y = p.y;
                    cur.glass.set_pos(p.x, p.y);
                }
                return Ok(());
            }
        }
        let (s, m) = (self.s, self.m);
        let map = Glass::make_map(gfx, p.w, p.h, s, p.radius, &self.params)?;
        let glass = Glass::styled(gfx, &self.backdrop, p.x, p.y, p.w, p.h, s, m, p.g, p.radius, &map, style, &self.params)?;
        let layer = gfx.target((p.w + 2 * m) as u32, (p.h + 2 * m) as u32)?;
        self.panels[slot] = Some(GlassPanel { x: p.x, y: p.y, w: p.w, h: p.h, radius: p.radius, glass, layer, lum: 0.3 });
        Ok(())
    }

    pub fn render_panel(&mut self, gfx: &Gfx, slot: usize) -> Result<()> {
        let Some(p) = self.panels[slot].as_mut() else { return Ok(()) };
        if let Ok(cells) = self.probe.measure(gfx, (p.x, p.y, p.w, p.h)) {
            p.lum = cells.iter().sum::<f32>() / 64.0;
        }
        let dc = &gfx.dc;
        unsafe {
            dc.SetTarget(&p.layer);
            dc.BeginDraw();
            dc.SetTransform(&mat(1.0, 0.0, 0.0));
            let r = p.glass.draw(dc, &Fx::new(p.lum));
            let e = dc.EndDraw(None, None);
            dc.SetTarget(None::<&ID2D1Image>);
            r?;
            e
        }
    }

    /// Draw panel `slot` into the open frame session with opacity `alpha` and an optional transform (open / close animation).
    pub fn draw_panel(&self, gfx: &Gfx, slot: usize, alpha: f32, xf: Option<Matrix3x2>) {
        let Some(p) = self.panels[slot].as_ref() else { return };
        let m = self.m as f32;
        let dc = &gfx.dc;
        unsafe {
            dc.SetTransform(&xf.unwrap_or_else(|| mat(1.0, 0.0, 0.0)));
            let dest = rf(p.x as f32 - m, p.y as f32 - m, (p.x + p.w) as f32 + m, (p.y + p.h) as f32 + m);
            let src = rf(0.0, 0.0, (p.w + 2 * self.m) as f32, (p.h + 2 * self.m) as f32);
            dc.DrawBitmap(&p.layer, Some(&dest), alpha, D2D1_INTERPOLATION_MODE_LINEAR, Some(&src), None);
            dc.SetTransform(&mat(1.0, 0.0, 0.0));
        }
    }

    // ------------------------------------------------ scheduling & input

    /// Earliest wake-up among the cards (None = nothing animates, the loop may sleep).
    pub fn next_due(&self) -> Option<Instant> {
        self.cards.iter().filter_map(|c| c.due).min()
    }
    /// Cards whose schedule is due (3 ms tolerance).
    pub fn due_cards(&self, at: Instant) -> Vec<usize> {
        let at = at + Duration::from_millis(3);
        (0..self.cards.len()).filter(|&i| self.cards[i].due.is_some_and(|d| d <= at)).collect()
    }
    /// Call `Widget::tick` of card `i` and reschedule it.
    pub fn tick_card(&mut self, gfx: &Gfx, i: usize, now: &Now) {
        let c = &mut self.cards[i];
        let t = with_cx(c, &self.sh, gfx, now, mat(1.0, 0.0, 0.0), false, "tick", |w, cx| w.tick(cx)).unwrap_or(Tick::Never);
        c.tick = t;
        c.due = schedule(t, c.due);
    }
    /// Deliver a mouse event (canvas px). Returns true when the widget reports a visible change.
    pub fn mouse(&mut self, gfx: &Gfx, i: usize, kind: MouseKind, px: i32, py: i32, button: Button, delta: f32, now: &Now) -> bool {
        let s = self.s;
        let c = &mut self.cards[i];
        let (lx, ly) = ((px - c.x) as f32 / s, (py - c.y) as f32 / s);
        c.hover = if kind == MouseKind::Leave { None } else { Some((lx, ly)) };
        // pointer glint: the contour point nearest to the pointer, strength by distance (animated by `step_fx`)
        let (gp, gd) = nearest_contour(c.w as f32, c.h as f32, c.glass.radius_px(), (px - c.x) as f32, (py - c.y) as f32);
        let goal = if kind == MouseKind::Leave { 0.0 } else { motion::glint_target(gd, s) };
        let moved = (gp.0 - c.glint_pt.0).abs() + (gp.1 - c.glint_pt.1).abs() > 1.5;
        if (goal - c.glint_goal).abs() > 0.01 || (goal > 0.0 && moved) {
            c.glint_goal = goal;
            if goal > 0.0 {
                c.glint_pt = gp;
            }
            self.fx_wake = true;
        }
        let ev = MouseEv { kind, x: lx, y: ly, button, delta };
        let r = with_cx(c, &self.sh, gfx, now, mat(1.0, 0.0, 0.0), false, "mouse", |w, cx| w.mouse(&ev, cx)).unwrap_or(false);
        if r {
            c.frame_once = true;
        }
        r
    }
    /// Take the "please produce a frame" request (`cx.request_frame()`).
    pub fn take_frame_request(&mut self, i: usize) -> bool {
        self.cards[i].flags.frame.replace(false)
    }
    /// If the widget changed its settings: the card id and the new cfg (to persist).
    pub fn take_cfg(&mut self, i: usize) -> Option<(String, Value)> {
        let c = &self.cards[i];
        c.flags.cfg_dirty.replace(false).then(|| (c.id.clone(), c.cfg.clone()))
    }
    pub fn animating(&self) -> bool {
        self.cards.iter().any(|c| c.tick.animates())
    }

    // ------------------------------------------------ headless debug knobs

    /// Put card `i`'s light at `deg` (CSS degrees) without any motion (headless light-sweep strips).
    pub fn set_card_light(&mut self, i: usize, deg: f32) {
        let b = self.params.light_angle.to_radians();
        let a = deg.to_radians();
        let c = &mut self.cards[i];
        c.light = LightSpring::with_offset((a.sin() - b.sin(), -a.cos() + b.cos()));
        c.light_drive = c.light.o;
        c.light_settled = true;
    }

    /// Card cfg keys that start with an underscore drive the headless renderer (`--cards "clock:s@0,0{\"_light\":90}"`):
    /// `_light` deg, `_mat` ms into the materialisation, `_mout` ms into the dematerialisation, `_move` [dx, dy] logical px from the grid cell, `_hover` [x, y] pointer in card px (glint + widget hover).
    pub fn apply_debug(&mut self, gfx: &Gfx, now: &Now) {
        for i in 0..self.cards.len() {
            let cfg = self.cards[i].cfg.clone();
            if let Some(a) = cfg.get("_light").and_then(|v| v.as_f64()) {
                self.set_card_light(i, a as f32);
            }
            if let Some(m) = cfg.get("_move").and_then(|v| v.as_array()) {
                // logical px offset from the grid cell (headless merge tests)
                let d = |k: usize| m.get(k).and_then(|v| v.as_f64()).unwrap_or(0.0) as f32 * self.s;
                let (x, y) = (self.cards[i].x + d(0).round() as i32, self.cards[i].y + d(1).round() as i32);
                self.set_card_pos(i, x, y);
            }
            for (key, out) in [("_mat", false), ("_mout", true)] {
                if let Some(ms) = cfg.get(key).and_then(|v| v.as_f64()) {
                    self.set_xf(i, mat_xf(&motion::materialize(ms as f32 / 1000.0, out, false), 0.0, 0.0));
                }
            }
            if let Some(h) = cfg.get("_hover").and_then(|v| v.as_array()) {
                if let (Some(x), Some(y)) = (h.first().and_then(|v| v.as_f64()), h.get(1).and_then(|v| v.as_f64())) {
                    let (px, py) = (self.cards[i].x + (x as f32 * self.s) as i32, self.cards[i].y + (y as f32 * self.s) as i32);
                    self.mouse(gfx, i, MouseKind::Move, px, py, Button::None, 0.0, now);
                    self.cards[i].glint = self.cards[i].glint_goal;
                }
            }
        }
    }

    // ------------------------------------------------ light, glint, merging neighbours

    /// Recompute which cards are close enough to pull a liquid neck (call after any move / scale / add / remove).
    fn update_merge(&mut self) {
        self.merge.clear();
        let s = self.s;
        for i in 0..self.cards.len() {
            for j in i + 1..self.cards.len() {
                let a = motion::merge_amount(eff_rect(&self.cards[i]), eff_rect(&self.cards[j]), s);
                if a > 0.0 {
                    self.merge.push((i, j, a));
                }
            }
        }
    }
    /// Merging pairs (card index, card index, amount 0..1).
    pub fn merging(&self) -> &[(usize, usize, f32)] {
        &self.merge
    }

    /// The card moves with velocity (vx, vy) px/s: its light swings against it and relaxes back afterwards (zero velocity = relax).
    pub fn set_light_drive(&mut self, i: usize, vx: f32, vy: f32) {
        let d = LightSpring::drive_for(vx, vy);
        let c = &mut self.cards[i];
        if (c.light_drive.0 - d.0).abs() > 1e-3 || (c.light_drive.1 - d.1).abs() > 1e-3 {
            c.light_drive = d;
            c.light_settled = false;
        }
    }

    /// Something of the glass motion is still moving (light spring, glint fade, appearance cross-fade): keep the animation timer.
    pub fn fx_busy(&self) -> bool {
        (0..self.cards.len()).any(|i| {
            let c = &self.cards[i];
            !c.light_settled || (c.glint - c.glint_goal).abs() > 1e-3 || (c.look - self.look_target(i)).abs() > 1e-3
        })
    }
    /// The pointer moved near an edge / the appearance preference changed: the host should arm its animation timer.
    pub fn take_fx_wake(&mut self) -> bool {
        std::mem::replace(&mut self.fx_wake, false)
    }

    /// One animation step of the glass motion; returns the canvas rectangle to recompose (None = nothing changed).
    /// The appearance cross-fade re-renders the glass layer of its card; light and glint only repaint the highlights.
    pub fn step_fx(&mut self, gfx: &Gfx, dt: f32, now: &Now) -> Option<RECT> {
        let mut dirty: Option<RECT> = None;
        let ext = (60.0 * self.s) as i32;
        for i in 0..self.cards.len() {
            let target = self.look_target(i);
            let c = &mut self.cards[i];
            let mut hi_changed = false;
            if !c.light_settled {
                c.light_settled = c.light.step(dt, c.light_drive);
                hi_changed = true;
            }
            let g = motion::approach(c.glint, c.glint_goal, dt, 0.12);
            if g != c.glint {
                c.glint = g;
                hi_changed = true;
            }
            let look_changed = (c.look - target).abs() > 1e-4;
            if look_changed {
                c.look = motion::look_step(c.look, target, dt);
            }
            let r = if look_changed {
                if let Err(e) = self.render_card(gfx, i, now) {
                    eprintln!("mswidgets: render card {i}: {e}");
                }
                Some(self.layer_rect_ext(i, if self.cards[i].xf.is_id() { 0 } else { ext }))
            } else if hi_changed {
                self.cards[i].scratch_ok = false;
                Some(if self.cards[i].xf.is_id() { self.card_rect(i) } else { self.layer_rect_ext(i, ext) })
            } else {
                None
            };
            if let Some(r) = r {
                dirty = Some(dirty.map_or(r, |d| union(d, r)));
            }
        }
        dirty
    }

    /// Render the mask sources of the merging pairs that touch `clip`. Must run OUTSIDE the frame session (called by `prepare`).
    fn prepare_merges(&mut self, gfx: &Gfx, clip: RECT) -> Result<()> {
        if self.merge.is_empty() {
            return Ok(());
        }
        if self.liquid.is_none() {
            match crate::liquid::Liquid::new(gfx, self.s) {
                Ok(l) => self.liquid = Some(l),
                Err(e) => {
                    eprintln!("mswidgets: liquid pass unavailable: {e}");
                    self.merge.clear();
                    return Ok(());
                }
            }
        }
        let p = self.params;
        let Some(liq) = self.liquid.as_mut() else { return Ok(()) };
        for (k, &(a, b, amount)) in self.merge.iter().enumerate() {
            let nk = neck_in(&self.cards, &p, a, b, amount);
            if !pair_hits(&nk, &clip) {
                continue;
            }
            liq.prepare_neck(gfx, k, &nk, (self.w, self.h))?;
        }
        Ok(())
    }

    /// The neck between merging neighbours (inside an open frame session, after the card layers, under overlays and highlights).
    fn draw_merges(&mut self, gfx: &Gfx, clip: RECT) {
        let p = self.params;
        let Some(liq) = self.liquid.as_mut() else { return };
        for (k, &(a, b, amount)) in self.merge.iter().enumerate() {
            let nk = neck_in(&self.cards, &p, a, b, amount);
            if !pair_hits(&nk, &clip) {
                continue;
            }
            if let Err(e) = liq.draw_neck(gfx, k, &nk, clip) {
                eprintln!("mswidgets: liquid neck: {e}");
            }
        }
    }

    // ------------------------------------------------ luminance and appearance

    /// Where the appearance cross-fade of card `i` is heading: 0 = dark glass, 1 = light (milky) glass.
    pub fn look_target(&self, i: usize) -> f32 {
        let c = &self.cards[i];
        match c.forced {
            Some(a) => a.k(),
            None => match self.pref {
                AppearancePref::Auto => c.app.cur.k(),
                AppearancePref::Dark => 0.0,
                AppearancePref::Light => 1.0,
            },
        }
    }
    pub fn set_appearance_pref(&mut self, p: AppearancePref) {
        if self.pref != p {
            self.pref = p;
            self.fx_wake = true;
        }
    }
    /// Headless: jump every card to its target appearance (no cross-fade).
    pub fn snap_looks(&mut self) {
        for i in 0..self.cards.len() {
            let t = self.look_target(i);
            self.cards[i].look = t;
        }
    }
    pub fn look(&self, i: usize) -> f32 {
        self.cards[i].look
    }
    pub fn appearance(&self, i: usize) -> Appearance {
        if self.look_target(i) > 0.5 {
            Appearance::Light
        } else {
            Appearance::Dark
        }
    }

    /// Re-measure the backdrop luminance of the cards in `mask` (8x8 GPU downscale + 256 byte readback) and re-decide their light / dark
    /// appearance from the 8x8 statistics (mean, percentile, bright share, contrast under the text, hysteresis).
    /// `smooth`: low-pass the new value (live mode) so the adaptive veil never flickers; a flip then needs two consecutive probes.
    pub fn measure_lum(&mut self, gfx: &Gfx, mask: u64, smooth: bool) -> Result<()> {
        for i in 0..self.cards.len().min(62) {
            if mask & (1 << i) == 0 {
                continue;
            }
            let c = &self.cards[i];
            let cells = self.probe.measure(gfx, (c.x, c.y, c.w, c.h))?;
            let st = motion::analyze(&cells);
            let c = &mut self.cards[i];
            c.lum = if smooth && c.lum_known { c.lum + (st.mean - c.lum) * 0.6 } else { st.mean };
            c.lum_known = true;
            c.app.update(&st, smooth);
            if !smooth {
                let t = self.look_target(i);
                self.cards[i].look = t;
            }
        }
        Ok(())
    }

    // ------------------------------------------------ rendering

    fn need_lists(look: f32) -> [bool; 2] {
        [look < 0.999, look > 0.001]
    }

    /// Does card `i` need its layer redrawn for a reason other than the backdrop (widget version, day, invalidate, an appearance
    /// whose ink has not been recorded yet)?
    pub fn content_stale(&self, i: usize, now: &Now) -> bool {
        let c = &self.cards[i];
        let need = Self::need_lists(c.look);
        c.key != (c.widget.version(), now.day_key()) || c.flags.invalidate.get() || (need[0] && c.content[0].is_none()) || (need[1] && c.content[1].is_none())
    }

    /// Redraw one card layer: shadow, glass, veils, rim and the static content with its soft text shadow.
    /// The widget's `draw` runs only when the content is stale (once per ink: dark / light).
    pub fn render_card(&mut self, gfx: &Gfx, i: usize, now: &Now) -> Result<()> {
        let (s, m) = (self.s, self.m as f32);
        if self.content_stale(i, now) {
            let need = Self::need_lists(self.cards[i].look);
            let c = &mut self.cards[i];
            let key = (c.widget.version(), now.day_key());
            if c.flags.invalidate.replace(false) || c.key != key {
                c.content = [None, None];
            }
            for l in 0..2 {
                if need[l] && c.content[l].is_none() {
                    c.content[l] = Some(record_content(c, &self.sh, gfx, now, s, m, l == 1)?);
                }
            }
            c.key = key;
        }
        let blur_max = motion::BLUR_MAX;
        let base_blur = self.params.blur;
        let c = &mut self.cards[i];
        c.scratch_ok = false;
        c.layer_stale = false;
        let k = c.look.clamp(0.0, 1.0);
        let blur = if c.xf.blur > 0.002 { base_blur + (blur_max - base_blur) * c.xf.blur } else { 0.0 };
        let dc = &gfx.dc;
        unsafe {
            dc.SetTarget(&c.layer);
            dc.BeginDraw();
            dc.SetTransform(&mat(1.0, 0.0, 0.0));
            let r = c.glass.draw(dc, &Fx { lum: c.lum, k, blur });
            if r.is_ok() {
                // subtle text shadow, stronger over a bright backdrop (CSS: text-shadow 0 1px 8px); nearly none under dark ink
                if let Some(cl) = &c.content[if k >= 0.5 { 1 } else { 0 }] {
                    c.tshadow.SetInput(0, cl, true);
                    let _ = fx_vec4(&c.tshadow, 1, [0.0, 0.0, 0.0, (0.10 + 0.25 * c.lum) * (1.0 - k) + 0.03 * k]);
                    if let Ok(o) = c.tshadow.GetOutput() {
                        dc.DrawImage(&o, Some(&pt(0.0, s)), None, D2D1_INTERPOLATION_MODE_LINEAR, D2D1_COMPOSITE_MODE_SOURCE_OVER);
                    }
                }
                // the content of the two inks cross-fades while the appearance changes
                for l in 0..2 {
                    let a = if l == 0 { 1.0 - k } else { k };
                    if a < 0.002 {
                        continue;
                    }
                    if let Some(cl) = &c.content[l] {
                        let part = a < 0.998;
                        if part {
                            push_opacity(dc, a);
                        }
                        dc.DrawImage(cl, None, None, D2D1_INTERPOLATION_MODE_LINEAR, D2D1_COMPOSITE_MODE_SOURCE_OVER);
                        if part {
                            dc.PopLayer();
                        }
                    }
                }
            }
            let e = dc.EndDraw(None, None);
            dc.SetTarget(None::<&ID2D1Image>);
            r?;
            e
        }
    }

    /// The extra soft shadow of a lifted card (effect + its input, cached per card size).
    fn lift_shadow(&mut self, gfx: &Gfx, w: i32, h: i32, radius: f32) -> Result<ID2D1Image> {
        let key = (w, h, radius.to_bits());
        if self.lift.as_ref().is_none_or(|l| l.0 != key) {
            let (s, m) = (self.s, self.m as f32);
            let cl = gfx.record(|dc| {
                let white = unsafe { dc.CreateSolidColorBrush(&col(1.0, 1.0, 1.0, 1.0), None)? };
                unsafe { dc.FillRoundedRectangle(&rr(m, m, w as f32, h as f32, radius * s), &white) };
                Ok(())
            })?;
            let fx = unsafe { gfx.dc.CreateEffect(&CLSID_D2D1Shadow)? };
            unsafe { fx.SetInput(0, &cl, true) };
            fx_f32(&fx, 0, 16.0 * s)?;
            self.lift = Some((key, fx, cl));
        }
        let fx = &self.lift.as_ref().map(|l| l.1.clone());
        match fx {
            Some(fx) => unsafe { fx.GetOutput() },
            None => Err(windows::core::Error::empty()),
        }
    }

    /// "Dim widgets on desktop": every card is drawn monochrome and a little darker (the live overlays stay as they are).
    pub fn set_dim(&mut self, on: bool) {
        self.dim = on;
    }
    pub fn dimmed(&self) -> bool {
        self.dim
    }

    /// The grayscale / darken effect of every card (created on demand, outside a frame session).
    fn ensure_dim(&mut self, gfx: &Gfx) -> Result<()> {
        // saturation .08, brightness .92: out_o = k * (.08 * in_o + .92 * luma(in))
        let (k, a) = (0.92f32, 0.08f32);
        let lw = [0.2126f32, 0.7152, 0.0722];
        let mut cm = [0f32; 20];
        for i in 0..3 {
            for o in 0..3 {
                cm[i * 4 + o] = k * (if i == o { a } else { 0.0 } + (1.0 - a) * lw[i]);
            }
        }
        cm[3 * 4 + 3] = 1.0;
        for c in &mut self.cards {
            if c.dimfx.is_some() {
                continue;
            }
            unsafe {
                let e = gfx.dc.CreateEffect(&CLSID_D2D1ColorMatrix)?;
                e.SetInput(0, &c.layer, true);
                fx_mat5x4(&e, 0, cm)?;
                fx_bool(&e, 2, true)?;
                let out = e.GetOutput()?;
                c.dimfx = Some((e, out));
            }
        }
        Ok(())
    }

    /// Pre-pass of a frame: every card with a non-identity transform that touches `clip` is composed upright (layer + overlay +
    /// lift shadow) into its own scratch bitmap. Must run OUTSIDE `begin`/`end`.
    pub fn prepare(&mut self, gfx: &Gfx, clip: RECT, now: &Now) -> Result<()> {
        let (s, m) = (self.s, self.m);
        let base_angle = self.params.light_angle;
        if self.dim {
            self.ensure_dim(gfx)?;
        }
        for i in 0..self.cards.len() {
            let lr = self.layer_rect_ext(i, (60.0 * s) as i32);
            if self.cards[i].xf.is_id() || !hit(&lr, &clip) {
                continue;
            }
            if self.cards[i].layer_stale {
                self.render_card(gfx, i, now)?; // the materialise blur moved: new glass layer
            }
            let (lw, lh) = (self.cards[i].w + 2 * m, self.cards[i].h + 2 * m);
            if self.cards[i].scratch.is_none() {
                self.cards[i].scratch = Some(gfx.target(lw as u32, lh as u32)?);
                self.cards[i].scratch_ok = false;
                self.cards[i].soften = None;
            }
            if self.cards[i].xf.blur > 0.002 {
                let c = &mut self.cards[i];
                if c.soften.is_none() {
                    unsafe {
                        let e = gfx.dc.CreateEffect(&CLSID_D2D1GaussianBlur)?;
                        if let Some(sc) = &c.scratch {
                            e.SetInput(0, sc, true);
                        }
                        fx_u32(&e, 1, 1)?;
                        // the fade (opacity) is a raw premultiplied scale: layers around effects do not draw reliably
                        let f = gfx.dc.CreateEffect(&CLSID_D2D1ColorMatrix)?;
                        f.SetInput(0, &e.GetOutput()?, true);
                        fx_u32(&f, 1, 2)?;
                        c.soften = Some((e, f));
                    }
                }
                if let Some((e, f)) = &c.soften {
                    fx_f32(e, 0, c.xf.blur * 6.0 * s)?;
                    let a = c.xf.alpha.clamp(0.0, 1.0);
                    let mut cm = [0f32; 20];
                    for k in 0..4 {
                        cm[k * 4 + k] = a;
                    }
                    fx_mat5x4(f, 0, cm)?;
                }
            }
            let animated = {
                let c = &self.cards[i];
                c.tick.animates() || c.frame_once || self.sh.preview
            };
            if self.cards[i].scratch_ok && !animated {
                continue;
            }
            let shadow = if self.cards[i].xf.lift > 0.0 { Some(self.lift_shadow(gfx, self.cards[i].w, self.cards[i].h, self.cards[i].size.radius())?) } else { None };
            let sh = &self.sh;
            let c = &mut self.cards[i];
            let Some(scratch) = c.scratch.clone() else { continue };
            let dc = &gfx.dc;
            unsafe {
                dc.SetTarget(&scratch);
                dc.BeginDraw();
                dc.SetTransform(&mat(1.0, 0.0, 0.0));
                dc.Clear(None);
                if let (Some(sd), true) = (&shadow, c.xf.lift > 0.0) {
                    if let Some(l) = self.lift.as_ref() {
                        let _ = fx_vec4(&l.1, 1, [0.0, 0.0, 0.0, 0.30 * c.xf.lift]);
                    }
                    dc.DrawImage(sd, Some(&pt(0.0, 18.0 * s * c.xf.lift)), None, D2D1_INTERPOLATION_MODE_LINEAR, D2D1_COMPOSITE_MODE_SOURCE_OVER);
                }
                dc.DrawImage(&c.layer, None, None, D2D1_INTERPOLATION_MODE_NEAREST_NEIGHBOR, D2D1_COMPOSITE_MODE_SOURCE_OVER);
                if c.tick.animates() || sh.preview || c.frame_once {
                    dc.PushAxisAlignedClip(&rf(m as f32, m as f32, (m + c.w) as f32, (m + c.h) as f32), D2D1_ANTIALIAS_MODE_PER_PRIMITIVE);
                    let light = c.look >= 0.5;
                    let r = with_cx(c, sh, gfx, now, mat(s, m as f32, m as f32), light, "overlay", |w, cx| w.overlay(cx));
                    dc.PopAxisAlignedClip();
                    if let Some(Err(e)) = r {
                        if !c.err_logged {
                            c.err_logged = true;
                            eprintln!("mswidgets: widget '{}' overlay failed: {e}", c.type_id);
                        }
                    }
                    c.frame_once = false;
                }
                let hi = hi_of(c, base_angle);
                c.glass.draw_hi(dc, m as f32, m as f32, &hi);
                let e = dc.EndDraw(None, None);
                dc.SetTarget(None::<&ID2D1Image>);
                e?;
            }
            c.scratch_ok = true;
        }
        self.prepare_merges(gfx, clip)?;
        Ok(())
    }

    /// Drop the scratch bitmaps (edit mode / drag over).
    pub fn release_scratch(&mut self) {
        for c in &mut self.cards {
            c.scratch = None;
            c.soften = None;
            c.scratch_ok = false;
        }
        self.lift = None;
    }

    /// Open a frame session: target = the frame, clipped to `clip`, cleared inside it, `under` (dump mode) drawn first.
    pub fn begin(&self, gfx: &Gfx, clip: RECT, under: Option<&ID2D1Bitmap1>) {
        let dc = &gfx.dc;
        unsafe {
            dc.SetTarget(&self.frame);
            dc.BeginDraw();
            dc.SetTransform(&mat(1.0, 0.0, 0.0));
            dc.PushAxisAlignedClip(&rf(clip.left as f32, clip.top as f32, clip.right as f32, clip.bottom as f32), D2D1_ANTIALIAS_MODE_ALIASED);
            dc.Clear(None);
            if let Some(b) = under {
                dc.DrawImage(b, None, None, D2D1_INTERPOLATION_MODE_NEAREST_NEIGHBOR, D2D1_COMPOSITE_MODE_SOURCE_OVER);
            }
        }
    }

    pub fn end(&self, gfx: &Gfx) -> Result<()> {
        let dc = &gfx.dc;
        unsafe {
            dc.PopAxisAlignedClip();
            let e = dc.EndDraw(None, None);
            dc.SetTarget(None::<&ID2D1Image>);
            e
        }
    }

    /// Cards inside an open frame session: cached layers first, live overlays over them. Cards with a transform come from `prepare`.
    pub fn draw_cards(&mut self, gfx: &Gfx, clip: RECT, now: &Now) {
        let dc = &gfx.dc;
        let (s, m) = (self.s, self.m);
        let order: Vec<usize> = {
            let mut o: Vec<usize> = (0..self.cards.len()).collect();
            o.sort_by_key(|&i| self.cards[i].xf.lift > 0.0); // lifted cards last (stable)
            o
        };
        let ext = (60.0 * s) as i32;
        unsafe {
            for &i in &order {
                let lr = clip_rect(&self.cards[i], m + if self.cards[i].xf.is_id() { 0 } else { ext }, self.w, self.h);
                if !hit(&lr, &clip) {
                    continue;
                }
                let c = &self.cards[i];
                if c.xf.is_id() {
                    let at = pt((c.x - m) as f32, (c.y - m) as f32);
                    match (&c.dimfx, self.dim) {
                        (Some((_, out)), true) => dc.DrawImage(out, Some(&at), None, D2D1_INTERPOLATION_MODE_NEAREST_NEIGHBOR, D2D1_COMPOSITE_MODE_SOURCE_OVER),
                        _ => dc.DrawImage(&c.layer, Some(&at), None, D2D1_INTERPOLATION_MODE_NEAREST_NEIGHBOR, D2D1_COMPOSITE_MODE_SOURCE_OVER),
                    }
                } else if let Some(sc) = &c.scratch {
                    let (lw, lh) = ((c.w + 2 * m) as f32, (c.h + 2 * m) as f32);
                    let t = xf_matrix(c.x as f32 + c.w as f32 / 2.0, c.y as f32 + c.h as f32 / 2.0, c.xf.scale, c.xf.rot);
                    dc.SetTransform(&t);
                    let dest = rf((c.x - m) as f32, (c.y - m) as f32, (c.x - m) as f32 + lw, (c.y - m) as f32 + lh);
                    match (&c.soften, c.xf.blur > 0.002) {
                        (Some((_, e)), true) => {
                            // materialise: the composed card is diffused too (blur, then the fade)
                            if let Ok(o) = e.GetOutput() {
                                dc.DrawImage(&o, Some(&pt(dest.left, dest.top)), None, D2D1_INTERPOLATION_MODE_LINEAR, D2D1_COMPOSITE_MODE_SOURCE_OVER);
                            }
                        }
                        _ => dc.DrawBitmap(sc, Some(&dest), c.xf.alpha.clamp(0.0, 1.0), D2D1_INTERPOLATION_MODE_LINEAR, Some(&rf(0.0, 0.0, lw, lh)), None),
                    }
                    dc.SetTransform(&mat(1.0, 0.0, 0.0));
                }
            }
            self.draw_merges(gfx, clip);
            let sh = &self.sh;
            for c in self.cards.iter_mut() {
                let lr = clip_rect(c, m, self.w, self.h);
                if !c.xf.is_id() || !hit(&lr, &clip) || !(c.tick.animates() || sh.preview || c.frame_once) {
                    continue;
                }
                dc.SetTransform(&mat(1.0, 0.0, 0.0)); // the clip rectangle is in frame px
                dc.PushAxisAlignedClip(&rf(c.x as f32, c.y as f32, (c.x + c.w) as f32, (c.y + c.h) as f32), D2D1_ANTIALIAS_MODE_PER_PRIMITIVE);
                let light = c.look >= 0.5;
                let r = with_cx(c, sh, gfx, now, mat(s, c.x as f32, c.y as f32), light, "overlay", |w, cx| w.overlay(cx));
                dc.PopAxisAlignedClip();
                if let Some(Err(e)) = r {
                    if !c.err_logged {
                        c.err_logged = true;
                        eprintln!("mswidgets: widget '{}' overlay failed: {e}", c.type_id);
                    }
                }
                c.frame_once = false;
            }
            // the dynamic highlights (specular rim following the light, glint, sweep) over the finished identity cards
            let base_angle = self.params.light_angle;
            for c in self.cards.iter() {
                if !c.xf.is_id() || !hit(&clip_rect(c, 0, self.w, self.h), &clip) {
                    continue;
                }
                c.glass.draw_hi(dc, c.x as f32, c.y as f32, &hi_of(c, base_angle));
            }
            dc.SetTransform(&mat(1.0, 0.0, 0.0));
        }
    }

    /// Compose the cached card layers + the live overlays into the frame inside `clip`.
    /// `under`: the backdrop drawn first (dump mode: the result looks like a screenshot); None in live mode (transparent window).
    pub fn composite(&mut self, gfx: &Gfx, clip: RECT, under: Option<&ID2D1Bitmap1>, now: &Now) -> Result<()> {
        self.prepare(gfx, clip, now)?;
        self.begin(gfx, clip, under);
        self.draw_cards(gfx, clip, now);
        self.draw_labels(gfx, now);
        self.end(gfx)
    }

    /// `--sheet` captions: small white text on a dark pill above each card (inside an open BeginDraw on the frame).
    fn draw_labels(&self, gfx: &Gfx, now: &Now) {
        if self.labels.is_empty() {
            return;
        }
        let mut cfg = json!({});
        let flags = Flags::default();
        let cx = Cx::new(CxParts { gfx, res: &self.sh.res, flags: &flags, cfg: &mut cfg, size: Size::S, scale: 1.0, now: *now, preview: true, lum: 0.2, lang: self.sh.lang, hover: None, waker: Waker::default(), utc_off: 0, base: mat(1.0, 0.0, 0.0), light: false });
        unsafe { gfx.dc.SetTransform(&mat(1.0, 0.0, 0.0)) };
        for (t, x, y) in &self.labels {
            let st = Style::CAPTION.weight(600);
            let (tw, _) = cx.measure(t, st, 400.0);
            cx.rrect(Rect::new(*x, *y, tw + 14.0, 18.0), 9.0, Color::rgba(0.0, 0.0, 0.0, 0.55));
            cx.text(Rect::new(*x + 7.0, *y, tw + 4.0, 18.0), t, st, WHITE);
        }
    }

    /// A `Cx` over the scene's text / glyph caches for host UI drawing (inside an open frame session): logical px -> canvas px
    /// through `base` (scale and origin). `f` must not outlive the call.
    pub fn with_ui_cx<R>(&self, gfx: &Gfx, base: Matrix3x2, lang: Lang, now: &Now, f: impl FnOnce(&Cx) -> R) -> R {
        let mut cfg = json!({});
        let flags = Flags::default();
        let cx = Cx::new(CxParts { gfx, res: &self.sh.res, flags: &flags, cfg: &mut cfg, size: Size::S, scale: self.s, now: *now, preview: true, lum: 0.2, lang, hover: None, waker: Waker::default(), utc_off: 0, base, light: false });
        unsafe { gfx.dc.SetTransform(&base) };
        let r = f(&cx);
        cx.finish();
        r
    }

    /// Dirty rectangle of the frame -> `f(y, row)` (premultiplied BGRA, `y` in canvas px; the row starts at `r.left`).
    pub fn read_rect(&mut self, gfx: &Gfx, r: RECT, mut f: impl FnMut(i32, &[u8])) -> Result<()> {
        let (w, h) = (r.right - r.left, r.bottom - r.top);
        if w <= 0 || h <= 0 {
            return Ok(());
        }
        let need_new = self.cpu.as_ref().is_none_or(|c| c.0 < w || c.1 < h);
        if need_new {
            let (cw, ch) = (((w + 255) / 256 * 256).min(self.w.max(w)), ((h + 255) / 256 * 256).min(self.h.max(h)));
            self.cpu = Some((cw, ch, gfx.cpu_read(cw as u32, ch as u32)?));
        }
        let Some((_, _, cpu)) = self.cpu.as_ref() else { return Ok(()) };
        unsafe {
            let sr = D2D_RECT_U { left: r.left as u32, top: r.top as u32, right: r.right as u32, bottom: r.bottom as u32 };
            cpu.CopyFromBitmap(Some(&D2D_POINT_2U { x: 0, y: 0 }), &self.frame, Some(&sr))?;
            let map = cpu.Map(D2D1_MAP_OPTIONS_READ)?;
            let n = (w * 4) as usize;
            for row in 0..h {
                f(r.top + row, std::slice::from_raw_parts(map.bits.add(row as usize * map.pitch as usize), n));
            }
            cpu.Unmap()
        }
    }

    /// Free the read-back staging memory (edit mode / drag over).
    pub fn trim_staging(&mut self) {
        self.cpu = None;
    }
}

fn clip_rect(c: &CardView, m: i32, w: i32, h: i32) -> RECT {
    clip(RECT { left: c.x - m, top: c.y - m, right: c.x + c.w + m, bottom: c.y + c.h + m }, w, h)
}

impl Probe {
    fn new(gfx: &Gfx, backdrop: &ID2D1Bitmap1) -> Result<Probe> {
        unsafe {
            let scale = gfx.dc.CreateEffect(&CLSID_D2D1Scale)?;
            scale.SetInput(0, backdrop, true);
            fx_vec2(&scale, 1, [0.0, 0.0])?; // centre point
            fx_u32(&scale, 2, 5)?; // HIGH_QUALITY_CUBIC (prefiltered when shrinking)
            Ok(Probe { out: scale.GetOutput()?, scale, tgt: gfx.target(8, 8)?, cpu: gfx.cpu_read(8, 8)? })
        }
    }

    /// The 8x8 cells of the backdrop under the card as luminance 0..1, row-major.
    fn measure(&self, gfx: &Gfx, (x, y, w, h): (i32, i32, i32, i32)) -> Result<[f32; 64]> {
        let (sx, sy) = (8.0 / w as f32, 8.0 / h as f32);
        fx_vec2(&self.scale, 0, [sx, sy])?;
        let dc = &gfx.dc;
        unsafe {
            dc.SetTarget(&self.tgt);
            dc.BeginDraw();
            dc.SetTransform(&mat(1.0, 0.0, 0.0));
            dc.Clear(None);
            let (l, t) = (x as f32 * sx, y as f32 * sy);
            dc.DrawImage(&self.out, Some(&pt(0.0, 0.0)), Some(&rf(l, t, l + 8.0, t + 8.0)), D2D1_INTERPOLATION_MODE_LINEAR, D2D1_COMPOSITE_MODE_SOURCE_COPY);
            let e = dc.EndDraw(None, None);
            dc.SetTarget(None::<&ID2D1Image>);
            e?;
            let mut cells = [0.0f32; 64];
            let mut n = 0usize;
            gfx.read_rect(&self.tgt, &self.cpu, RECT { left: 0, top: 0, right: 8, bottom: 8 }, |_, row| {
                for p in row.chunks_exact(4) {
                    if n < 64 {
                        cells[n] = ((0.2126 * p[2] as f32 + 0.7152 * p[1] as f32 + 0.0722 * p[0] as f32) / 255.0).clamp(0.0, 1.0);
                        n += 1;
                    }
                }
            })?;
            Ok(cells)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backdrop::{Backdrop, PlainBackdrop};

    fn alpha_at(sc: &mut Scene, gfx: &Gfx, x: i32, y: i32) -> u8 {
        let mut a = 0u8;
        sc.read_rect(gfx, RECT { left: x, top: y, right: x + 1, bottom: y + 1 }, |_, row| a = row[3]).unwrap();
        a
    }

    /// Appearance from the backdrop (bright -> light, override wins), the liquid neck between close cards and the materialise transform
    /// all compose without errors and show up in the pixels.
    #[test]
    fn appearance_merge_and_materialise() {
        unsafe {
            let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_MULTITHREADED);
        }
        let gfx = Gfx::new(None).unwrap();
        let px: Vec<u8> = (0..800 * 600).flat_map(|_| [235u8, 240, 242, 255]).collect();
        let bright = gfx.from_bgra(800, 600, &px).unwrap();
        let env = SceneEnv { preview: true, hwnd: 0, lang: Lang::Ru, utc_off: 0 };
        let now = Now::sample();
        let full = RECT { left: 0, top: 0, right: 800, bottom: 600 };
        let mk = |gap: i32| {
            let mut sc = Scene::empty(&gfx, 800, 600, 1.0, 0.5, &bright, &env).unwrap();
            let a = CardSpec { id: "a".into(), type_id: "clock".into(), size: Size::S, cfg: json!({}), x: 40, y: 60, w: 170, h: 170 };
            let b = CardSpec { id: "b".into(), type_id: "clock".into(), size: Size::S, cfg: json!({"_appearance": "dark"}), x: 40 + 170 + gap, y: 60, w: 170, h: 170 };
            sc.add_card(&gfx, &a).unwrap();
            sc.add_card(&gfx, &b).unwrap();
            for i in 0..2 {
                sc.tick_card(&gfx, i, &now);
            }
            sc.measure_lum(&gfx, 3, false).unwrap();
            for i in 0..2 {
                sc.render_card(&gfx, i, &now).unwrap();
            }
            sc
        };
        let mut far = mk(40);
        assert!(far.look_target(0) > 0.5, "a bright backdrop picks the light glass");
        assert_eq!(far.look_target(1), 0.0, "the explicit override wins");
        assert!(far.merging().is_empty());
        far.composite(&gfx, full, None, &now).unwrap();
        let mid_far = alpha_at(&mut far, &gfx, 40 + 170 + 20, 145);
        let mut near = mk(8);
        assert_eq!(near.merging().len(), 1, "gap 8 merges");
        near.composite(&gfx, full, None, &now).unwrap();
        let mid_near = alpha_at(&mut near, &gfx, 40 + 170 + 4, 145);
        assert!(mid_near > 200 && mid_far < 60, "neck {mid_near} vs empty gap {mid_far}");
        // the grid gap (16) stays separate
        assert!(mk(16).merging().is_empty());

        // materialise: diffused, scaled, half faded, rim sweep: composes through the scratch path
        let st = motion::materialize(0.1, false, false);
        near.set_xf(0, mat_xf(&st, 0.0, 0.0));
        near.composite(&gfx, full, None, &now).unwrap();
        let c = alpha_at(&mut near, &gfx, 125, 145);
        assert!(c > 20 && c < 255, "materialising card alpha {c}");
        // appearance cross-fade: stepping the effects re-renders and finishes
        let mut sc = mk(40);
        sc.set_appearance_pref(AppearancePref::Dark);
        sc.cards[0].forced = None;
        let mut guard = 0;
        sc.cards[0].look = 1.0;
        while sc.fx_busy() && guard < 40 {
            sc.step_fx(&gfx, 0.016, &now);
            guard += 1;
        }
        assert!(guard > 5 && guard < 25 && sc.look(0) == 0.0, "cross-fade took {guard} steps");
    }

    /// Run-time edits of a scene on the GPU: add / move / lift + rotate (scratch path) / resize / glass slider / panel / reconfigure / remove.
    #[test]
    fn dynamic_scene_ops() {
        unsafe {
            let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_MULTITHREADED);
        }
        let gfx = Gfx::new(None).unwrap();
        let bd = PlainBackdrop::new(&gfx, 800, 600).unwrap();
        let env = SceneEnv { preview: true, hwnd: 0, lang: Lang::Ru, utc_off: 0 };
        let mut sc = Scene::empty(&gfx, 800, 600, 1.0, 0.5, bd.bitmap(), &env).unwrap();
        let now = Now::sample();
        let full = RECT { left: 0, top: 0, right: 800, bottom: 600 };
        let spec = CardSpec { id: "clock-t".into(), type_id: "clock".into(), size: Size::S, cfg: json!({}), x: 40, y: 60, w: 170, h: 170 };
        let i = sc.add_card(&gfx, &spec).unwrap();
        let uid = sc.cards[i].uid;
        sc.tick_card(&gfx, i, &now);
        sc.measure_lum(&gfx, 1, false).unwrap();
        sc.render_card(&gfx, i, &now).unwrap();
        sc.composite(&gfx, full, None, &now).unwrap();
        assert!(alpha_at(&mut sc, &gfx, 125, 145) > 200, "card drawn");
        assert_eq!(alpha_at(&mut sc, &gfx, 400, 400), 0);

        // move: the old place is cleared by the dirty-rect compose, the new one drawn
        sc.set_card_pos(i, 400, 300);
        sc.render_card(&gfx, i, &now).unwrap();
        sc.composite(&gfx, full, None, &now).unwrap();
        assert_eq!(alpha_at(&mut sc, &gfx, 125, 145), 0);
        assert!(alpha_at(&mut sc, &gfx, 485, 385) > 200);

        // lifted + rotated + scaled: composed through the scratch bitmap, with the extra shadow below the card
        sc.set_xf(i, Xf { rot: 4.0, scale: 1.1, lift: 1.0, ..Xf::ID });
        sc.composite(&gfx, full, None, &now).unwrap();
        assert!(alpha_at(&mut sc, &gfx, 485, 385) > 200, "scratch path draws the card");
        assert!(alpha_at(&mut sc, &gfx, 485, 300 + 170 + 14) > 0, "shadow under the lifted card");
        sc.set_xf(i, Xf { alpha: 0.5, ..Xf::ID });
        sc.composite(&gfx, full, None, &now).unwrap();
        let half = alpha_at(&mut sc, &gfx, 485, 385);
        assert!((90..170).contains(&half), "half transparent card: {half}");
        sc.set_xf(i, Xf::ID);

        // resize S -> M keeps the id / settings, makes a new widget, new geometry
        sc.resize_card(&gfx, i, Size::M, 100, 100).unwrap();
        assert_eq!((sc.cards[i].w, sc.cards[i].h, sc.cards[i].uid), (356, 170, uid));
        sc.tick_card(&gfx, i, &now);
        sc.render_card(&gfx, i, &now).unwrap();
        sc.composite(&gfx, full, None, &now).unwrap();
        let (in_a, out_a) = (alpha_at(&mut sc, &gfx, 400, 185), alpha_at(&mut sc, &gfx, 700, 500));
        assert!(in_a > 150 && out_a == 0, "inside {in_a}, outside {out_a}");

        // glass slider + dim effect
        sc.set_glass(&gfx, 0.9).unwrap();
        sc.set_dim(true);
        sc.render_card(&gfx, i, &now).unwrap();
        sc.composite(&gfx, full, None, &now).unwrap();
        assert!(alpha_at(&mut sc, &gfx, 400, 185) > 100);
        sc.set_dim(false);

        // a glass panel drawn inside an open frame session
        sc.set_panel(&gfx, 0, Some(PanelSpec { x: 200, y: 300, w: 400, h: 200, radius: 26.0, g: 0.9 }), GlassStyle::frosted()).unwrap();
        sc.render_panel(&gfx, 0).unwrap();
        sc.prepare(&gfx, full, &now).unwrap();
        sc.begin(&gfx, full, None);
        sc.draw_cards(&gfx, full, &now);
        sc.draw_panel(&gfx, 0, 1.0, None);
        sc.end(&gfx).unwrap();
        assert!(alpha_at(&mut sc, &gfx, 400, 400) > 100, "panel drawn");
        sc.set_panel(&gfx, 0, None, GlassStyle::default()).unwrap();

        // a new monitor / scale: everything GPU side is rebuilt, the widget stays
        let bd2 = PlainBackdrop::new(&gfx, 1200, 900).unwrap();
        sc.reconfigure(&gfx, 1200, 900, 1.5, bd2.bitmap(), &[(60, 75, 534, 255)]).unwrap();
        assert_eq!((sc.w, sc.h, sc.cards[0].w, sc.cards[0].uid), (1200, 900, 534, uid));
        sc.render_card(&gfx, 0, &now).unwrap();
        sc.composite(&gfx, RECT { left: 0, top: 0, right: 1200, bottom: 900 }, None, &now).unwrap();
        assert!(alpha_at(&mut sc, &gfx, 300, 200) > 150);

        sc.remove_card(0);
        assert_eq!(sc.len(), 0);
        assert!(sc.index_of_uid(uid).is_none());
    }
}
