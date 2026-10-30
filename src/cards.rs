//! The generic card system: for every layout item a "layer" bitmap (card + 48 px margin) holding shadow + glass + the widget's static
//! content (a command list recorded by `Widget::draw`), plus the per-frame overlay (`Widget::overlay`) painted straight into the frame
//! over the cached layers. A layer is redrawn only when the backdrop behind that card changed, the widget's `version()` changed or
//! the day changed, so a 30 fps animation never re-runs the glass effect graph.
//!
//! The scene canvas is the whole MONITOR (card coordinates are monitor px); the layered window only presents a sub-rectangle of it.
//! Cards can be added, removed, moved and resized at run time (drag & drop, gallery, size menu); a card with a non-identity `Xf`
//! (jiggle, lift, pop) is first composed upright into its own scratch bitmap and then drawn rotated / scaled / faded.
use crate::gfx::*;
use crate::glass::{self, contour_point, contour_u, nearest_contour, tl_corner_u, Fx, Glass, GlassParams, GlassStyle, Hi};
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
    /// inertial light of this card, its drive (opposite to the velocity)
    light: LightSpring,
    light_drive: (f32, f32),
    light_settled: bool,
    /// pointer hover / press: linear progress 0..1 (eased on use) and whether they are on, the card scale spring,
    /// the smoothed pointer (light spot) and its goal (card px), the smoothed contour fraction of the rim glint
    hover_t: f32,
    hover_on: bool,
    press_t: f32,
    press_on: bool,
    bump: motion::Bump,
    spot: (f32, f32),
    spot_goal: (f32, f32),
    glint_u: f32,
    /// the glass layer has to be re-rendered before the next compose (materialise blur changed)
    layer_stale: bool,
    /// a size change in flight (S / M / L): the card is drawn by `Scene::draw_morph` until it settles
    morph: Option<SizeMorph>,
    widget: Box<dyn Widget>,
    pub cfg: Value,
    flags: Flags,
    pub tick: Tick,
    due: Option<Instant>,
    hover: Option<Pt>,
    tshadow: ID2D1Effect,
    /// the text shadow of the recorded content, rendered once (see `TextShadow`)
    tsh: TextShadow,
    lum: f32,
    lum_known: bool,
    err_logged: bool,
    frame_once: bool,
}

/// A card changing size class. The glass rectangle springs from the old to the new size (`motion::Morph`); the glass is cut out of
/// `shell` (shadow + lens + tint + rim of the BIGGER of the two sizes, rendered once) as a 9-slice, so the corners and edges stay
/// sharp and unscaled and the flat middle is cropped (it lines up with the real backdrop); the new content is laid out at its final
/// size and clipped to the rectangle while the old layout fades out. Nothing is allocated per frame.
struct SizeMorph {
    m: motion::Morph,
    shell: ID2D1Bitmap1,
    /// card size (px, without margin) the shell was rendered for
    shell_wh: (i32, i32),
    /// the previous layout (per ink) with its text shadow
    old: [Option<ID2D1CommandList>; 2],
    old_shadow: ID2D1Effect,
    /// every card rectangle (canvas px) the morph can touch, with the spring overshoot
    cover: RECT,
}

/// The soft text shadow is a blur of the whole recorded content: its pixels do not depend on the backdrop, only its alpha (the luminance weight)
/// does. It is rendered by the same effect into a layer-sized bitmap for one (content, ink, alpha) and redrawn as a plain copy until one of them
/// changes; the alpha follows the 2 Hz luminance probe, not the 15 Hz backdrop updates. `MSWIDGETS_NO_GLASS_CACHE=1` keeps the effect on the
/// per-redraw path.
struct TextShadow {
    /// `glass::cache_enabled()` when the card was created
    enabled: bool,
    /// reused in place (one allocation per card), re-created when the layer size changed
    bmp: Option<ID2D1Bitmap1>,
    /// (ink 0 dark / 1 light, bits of the alpha) the bitmap holds; None = stale (the content was re-recorded)
    key: Option<(usize, u32)>,
}
impl TextShadow {
    fn new() -> TextShadow {
        TextShadow { enabled: crate::glass::cache_enabled(), bmp: None, key: None }
    }
}

/// A glass-only surface (no widget): the gallery panel, the settings strip and the text prompt. Drawn by the host between the cards and its own UI.
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

/// Glass panels of the host UI: gallery / settings, strip, text prompt, context menu, its submenu (see `ui::SLOT_*`).
pub const PANEL_SLOTS: usize = 5;

pub struct Scene {
    pub w: i32,
    pub h: i32,
    s: f32,
    m: i32,
    g: f32,
    pub cards: Vec<CardView>,
    pub panels: [Option<GlassPanel>; PANEL_SLOTS],
    sh: Shared,
    backdrop: ID2D1Bitmap1,
    maps: HashMap<(i32, i32, u32), ID2D1Bitmap1>,
    next_uid: usize,
    dim: bool,
    frame: ID2D1Bitmap1,
    /// float scratch of the glass pass of `render_card` (grown to the biggest layer), see there
    hdr: Option<(u32, u32, ID2D1Bitmap1)>,
    /// CPU read-back staging bitmap (grown on demand, only the dirty rectangle is ever copied through it)
    cpu: Option<(i32, i32, ID2D1Bitmap1)>,
    probe: Probe,
    pub params: GlassParams,
    /// the base look (defaults + glass.json): the host panels (gallery, settings) are built with it, whatever the user's look sliders say
    panel_params: GlassParams,
    /// global appearance preference (`widgetsAppearance` / `--appearance`)
    pref: AppearancePref,
    /// merging neighbours (index, index, amount) and the liquid neck renderer
    merge: Vec<(usize, usize, f32)>,
    liquid: Option<crate::liquid::Liquid>,
    fx_wake: bool,
    /// a size morph settled since the host last asked (it re-syncs the backdrop regions and the window)
    morph_done: bool,
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
    /// target + staging bitmap of `colors` (kept between its calls: the wallpaper accent asks every few seconds)
    col: std::cell::RefCell<Option<(u32, u32, ID2D1Bitmap1, ID2D1Bitmap1)>>,
}

/// Run `f` with a `Cx` for this card; panics inside the widget are caught (needs panic=unwind, set in Cargo.toml).
fn with_cx<R>(c: &mut CardView, sh: &Shared, gfx: &Gfx, now: &Now, base: Matrix3x2, light: bool, what: &str, f: impl FnOnce(&mut dyn Widget, &mut Cx) -> R) -> Option<R> {
    let CardView { widget, cfg, flags, size, hover, lum, uid, type_id, err_logged, .. } = c;
    if what == "draw" && std::env::var_os("MSW_TEXT_LINT").is_some() {
        eprintln!("CARD\t{}\t{:?}", type_id, size);
    }
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
        waker: Waker { hwnd: sh.hwnd, idx: *uid, msg: WM_WIDGET },
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

impl CardView {
    /// No transform and no hover / press scale: drawn straight from its layer (the fast path, no scratch compose).
    fn upright(&self) -> bool {
        self.xf.is_id() && self.bump.x == 1.0 && self.morph.is_none()
    }
    /// The card rectangle, or while it changes size everything it can touch (canvas px).
    fn bounds(&self) -> RECT {
        self.morph.as_ref().map_or(RECT { left: self.x, top: self.y, right: self.x + self.w, bottom: self.y + self.h }, |m| m.cover)
    }
    fn bump_goal(&self) -> f32 {
        motion::bump_target(self.hover_on, self.press_on)
    }
    /// Where the rim glint heads: the contour fraction of the point nearest to the smoothed pointer.
    fn glint_goal(&self) -> f32 {
        let (w, h) = self.glass.size_px();
        let r = self.glass.radius_px();
        contour_u(w, h, r, nearest_contour(w, h, r, self.spot.0, self.spot.1).0)
    }
    /// Hover / press still easing (the animation timer must run); a pointer resting on a fully hovered card is idle.
    fn hover_busy(&self) -> bool {
        let on = |b: bool| if b { 1.0 } else { 0.0 };
        self.hover_t != on(self.hover_on)
            || self.press_t != on(self.press_on)
            || !self.bump.at_rest(self.bump_goal())
            || (self.hover_t > 0.0 && (self.spot != self.spot_goal || motion::wrap_dist(self.glint_u, self.glint_goal()) != 0.0))
    }
}

/// The highlight inputs of a card: light direction (base + inertia), appearance, hover glint + spot, materialise sweep.
fn hi_of(c: &CardView, base_angle: f32, s: f32) -> Hi {
    let (w, h) = c.glass.size_px();
    let r = c.glass.radius_px();
    let sweep = (c.xf.sweep >= 0.0).then(|| {
        let p = contour_point(w, h, r, tl_corner_u(w, h, r) + c.xf.sweep);
        (p.0, p.1, (std::f32::consts::PI * c.xf.sweep).sin().max(0.0).powf(0.7))
    });
    let e = motion::smooth(c.hover_t);
    let (glint, spot) = if e > 0.004 {
        let d = nearest_contour(w, h, r, c.spot.0, c.spot.1).1;
        let g = contour_point(w, h, r, c.glint_u);
        // a press dims the spot a little
        (Some((g.0, g.1, e * motion::hover_rim(d, s))), Some((c.spot.0, c.spot.1, e * (1.0 - 0.4 * motion::smooth(c.press_t)))))
    } else {
        (None, None)
    };
    Hi { angle: c.light.angle(base_angle), k: c.look, glint, sweep, spot, alpha: 1.0 }
}

/// Card rectangle in canvas px as drawn: scaled about its centre by the transform (lift / materialise).
fn eff_rect(c: &CardView) -> motion::R4 {
    if let Some(mo) = &c.morph {
        return mo.m.rect(); // the springing rectangle: the neighbours' necks follow it
    }
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

/// The soft text shadow and the static content of one layout (both inks, cross-faded by the appearance `k`) into the current target,
/// layer-local (the caller has set the transform), all of it at opacity `fade` (the shadow effect is scaled through its colour: layers
/// around effects do not draw reliably, layers around the command lists do).
unsafe fn draw_content(dc: &ID2D1DeviceContext, tshadow: &ID2D1Effect, content: &[Option<ID2D1CommandList>; 2], k: f32, lum: f32, s: f32, fade: f32) {
    // subtle text shadow, stronger over a bright backdrop (CSS: text-shadow 0 1px 8px); nearly none under dark ink
    if let Some(cl) = &content[if k >= 0.5 { 1 } else { 0 }] {
        tshadow.SetInput(0, cl, true);
        let _ = fx_vec4(tshadow, 1, [0.0, 0.0, 0.0, ((0.10 + 0.25 * lum) * (1.0 - k) + 0.03 * k) * fade]);
        if let Ok(o) = tshadow.GetOutput() {
            dc.DrawImage(&o, Some(&pt(0.0, s)), None, D2D1_INTERPOLATION_MODE_LINEAR, D2D1_COMPOSITE_MODE_SOURCE_OVER);
        }
    }
    // the content of the two inks cross-fades while the appearance changes
    for l in 0..2 {
        let a = (if l == 0 { 1.0 - k } else { k }) * fade;
        if a < 0.002 {
            continue;
        }
        if let Some(cl) = &content[l] {
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

/// Render the text shadow of `c.content[ink]` at `alpha` into the card's reused bitmap (own draw session: the effect plays the recorded content,
/// and a command list that holds layers must not be played as an effect input and directly inside one session, D2D then reports
/// D2DERR_LAYER_ALREADY_IN_USE).
fn build_text_shadow(gfx: &Gfx, c: &mut CardView, ink: usize, alpha: f32, s: f32, lw: u32, lh: u32) -> Result<()> {
    let Some(cl) = c.content[ink].clone() else { return Ok(()) };
    let bmp = match c.tsh.bmp.take() {
        Some(b) if unsafe { b.GetPixelSize() } == (D2D_SIZE_U { width: lw, height: lh }) => b,
        _ => gfx.target_hdr(lw, lh)?,
    };
    c.tsh.key = None;
    let dc = &gfx.dc;
    unsafe {
        dc.SetTarget(&bmp);
        dc.BeginDraw();
        dc.SetTransform(&mat(1.0, 0.0, 0.0));
        dc.Clear(None);
        c.tshadow.SetInput(0, &cl, true);
        let _ = fx_vec4(&c.tshadow, 1, [0.0, 0.0, 0.0, alpha]);
        if let Ok(o) = c.tshadow.GetOutput() {
            dc.DrawImage(&o, Some(&pt(0.0, s)), None, D2D1_INTERPOLATION_MODE_LINEAR, D2D1_COMPOSITE_MODE_SOURCE_OVER);
        }
        let e = dc.EndDraw(None, None);
        dc.SetTarget(None::<&ID2D1Image>);
        c.tsh.bmp = Some(bmp);
        e?;
    }
    c.tsh.key = Some((ink, alpha.to_bits()));
    Ok(())
}

impl Scene {
    /// An empty scene on a `w` x `h` px canvas over `backdrop` (canvas-sized).
    pub fn empty(gfx: &Gfx, w: i32, h: i32, s: f32, glass_g: f32, backdrop: &ID2D1Bitmap1, env: &SceneEnv) -> Result<Scene> {
        let sh = Shared { res: Res::new(gfx, &crate::fonts::SET)?, s, preview: env.preview, hwnd: env.hwnd, lang: env.lang, utc_off: env.utc_off };
        Ok(Scene {
            w,
            h,
            s,
            m: (MARGIN * s).round() as i32,
            g: glass_g,
            cards: Vec::new(),
            panels: [None, None, None, None, None],
            sh,
            backdrop: backdrop.clone(),
            maps: HashMap::new(),
            next_uid: 1,
            dim: false,
            frame: gfx.target(w as u32, h as u32)?,
            hdr: None,
            cpu: None,
            probe: Probe::new(gfx, backdrop)?,
            params: crate::glass::global_params(),
            panel_params: crate::glass::global_params(),
            pref: AppearancePref::Auto,
            merge: Vec::new(),
            liquid: None,
            fx_wake: false,
            morph_done: false,
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
        self.cards.iter().map(|c| c.bounds()).collect()
    }
    /// Card layer rectangle (card + shadow margin) clipped to the canvas.
    pub fn layer_rect(&self, i: usize) -> RECT {
        self.layer_rect_ext(i, 0)
    }
    /// Same with `extra` more px on every side (lifted / rotated cards draw outside their layer a little).
    pub fn layer_rect_ext(&self, i: usize, extra: i32) -> RECT {
        let b = self.cards[i].bounds();
        let e = self.m + extra;
        clip(RECT { left: b.left - e, top: b.top - e, right: b.right + e, bottom: b.bottom + e }, self.w, self.h)
    }
    /// The card rectangle itself (canvas px): what an animating overlay repaints.
    pub fn card_rect(&self, i: usize) -> RECT {
        self.cards[i].bounds()
    }
    pub fn lum(&self, i: usize) -> f32 {
        self.cards[i].lum
    }
    /// Benches: pretend the luminance probe measured `d` more (a moving backdrop changes the shadow weights and the text shadow alpha).
    #[cfg(test)]
    pub fn nudge_lum(&mut self, d: f32) {
        for c in &mut self.cards {
            c.lum += d;
        }
    }
    pub fn type_id(&self, i: usize) -> &str {
        &self.cards[i].type_id
    }
    pub fn frame_bitmap(&self) -> &ID2D1Bitmap1 {
        &self.frame
    }

    // ------------------------------------------------ run-time editing

    /// Corner radius (logical px) of a card of `size` with the current look (`radius_k`).
    pub fn radius_of(&self, size: Size) -> f32 {
        size.radius() * self.params.radius_k
    }

    /// A fresh glass effect graph for a card rectangle, with the scene's current parameters.
    fn new_glass(&mut self, gfx: &Gfx, size: Size, x: i32, y: i32, w: i32, h: i32) -> Result<Glass> {
        let (s, m, g) = (self.s, self.m, self.g);
        let radius = self.radius_of(size);
        let key = (w, h, radius.to_bits());
        if !self.maps.contains_key(&key) {
            self.maps.insert(key, Glass::make_map(gfx, w, h, s, radius, &self.params)?);
        }
        Glass::new(gfx, &self.backdrop, x, y, w, h, s, m, g, radius, &self.maps[&key], &self.params)
    }

    fn gpu(&mut self, gfx: &Gfx, size: Size, x: i32, y: i32, w: i32, h: i32) -> Result<(Glass, ID2D1Bitmap1, ID2D1Effect)> {
        let (s, m) = (self.s, self.m);
        let glass = self.new_glass(gfx, size, x, y, w, h)?;
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
            hover_t: 0.0,
            hover_on: false,
            press_t: 0.0,
            press_on: false,
            bump: motion::Bump::default(),
            spot: (0.0, 0.0),
            spot_goal: (0.0, 0.0),
            glint_u: 0.0,
            layer_stale: false,
            morph: None,
            widget: (def.make)(),
            cfg: if c.cfg.is_object() { c.cfg.clone() } else { json!({}) },
            flags: Flags::default(),
            tick: Tick::Never,
            due: None,
            hover: None,
            tshadow,
            tsh: TextShadow::new(),
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
        if c.morph.take().is_some() {
            // a drag / drop settle takes over: the size change ends where it stands
            c.scratch_ok = false;
            self.morph_done = true;
        }
        self.update_merge();
    }

    /// New size class for card `i` (a fresh widget instance with the same settings, as the size of a Widget never changes) at (x, y).
    /// `animate`: the glass springs from what is on screen to the new rectangle (`SizeMorph`) instead of jumping; the host passes
    /// false with reduced motion. The caller still ticks the fresh widget and renders the card (the final layer).
    pub fn resize_card(&mut self, gfx: &Gfx, i: usize, size: Size, x: i32, y: i32, animate: bool) -> Result<()> {
        let (type_id, cfg) = (self.cards[i].type_id.clone(), self.cards[i].cfg.clone());
        let Some(def) = registry::global().get(&type_id) else { return Ok(()) };
        let (lw, lh) = size.logical();
        let (w, h) = ((lw * self.s).round() as i32, (lh * self.s).round() as i32);
        let (glass, layer, tshadow) = self.gpu(gfx, size, x, y, w, h)?;
        // a size change in flight carries on from the rectangle drawn now
        let (from, old_wh, k, lum) = {
            let c = &self.cards[i];
            (c.morph.as_ref().map_or((c.x as f32, c.y as f32, c.w as f32, c.h as f32), |mo| mo.m.rect()), (c.w, c.h), c.look, c.lum)
        };
        let to = (x as f32, y as f32, w as f32, h as f32);
        let morph = if animate && from != to {
            // the shell comes from whichever of the two glasses is bigger (sizes are nested: S in M in L)
            let shell = if w * h >= old_wh.0 * old_wh.1 { self.render_shell(gfx, &glass, (w, h), k, lum) } else { self.render_shell(gfx, &self.cards[i].glass, old_wh, k, lum) };
            match shell {
                Ok(shell) => Some((shell, if w * h >= old_wh.0 * old_wh.1 { (w, h) } else { old_wh })),
                Err(e) => {
                    eprintln!("mswidgets: size morph unavailable: {e}");
                    None
                }
            }
        } else {
            None
        };
        let pad = (8.0 * self.s).ceil() as i32;
        let cover = {
            let (a, b) = (RECT { left: from.0.floor() as i32, top: from.1.floor() as i32, right: (from.0 + from.2).ceil() as i32, bottom: (from.1 + from.3).ceil() as i32 }, RECT { left: x, top: y, right: x + w, bottom: y + h });
            let u = union(a, b);
            RECT { left: u.left - pad, top: u.top - pad, right: u.right + pad, bottom: u.bottom + pad }
        };
        let c = &mut self.cards[i];
        let old_content = std::mem::take(&mut c.content);
        let old_shadow = std::mem::replace(&mut c.tshadow, tshadow);
        c.morph = morph.map(|(shell, shell_wh)| SizeMorph { m: motion::Morph::new(from, to), shell, shell_wh, old: old_content, old_shadow, cover });
        c.size = size;
        c.x = x;
        c.y = y;
        c.w = w;
        c.h = h;
        c.glass = glass;
        c.layer = layer;
        c.dimfx = None;
        c.scratch = None;
        c.scratch_ok = false;
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

    /// A new glass look (the settings sliders): every card gets a fresh effect graph for `p`; layers, widget instances and their state stay.
    /// The glass panels keep the base look. The caller re-renders the layers.
    pub fn set_params(&mut self, gfx: &Gfx, p: &GlassParams) -> Result<()> {
        if *p == self.params {
            return Ok(());
        }
        // the displacement maps depend on the corner radius and the refraction profile only
        if (p.radius_k, p.refr_edge, p.refr_pow) != (self.params.radius_k, self.params.refr_edge, self.params.refr_pow) {
            self.maps.clear();
        }
        self.params = *p;
        for i in 0..self.cards.len() {
            let (size, x, y, w, h) = (self.cards[i].size, self.cards[i].x, self.cards[i].y, self.cards[i].w, self.cards[i].h);
            let glass = self.new_glass(gfx, size, x, y, w, h)?;
            glass.adopt_cache(&self.cards[i].glass);
            self.cards[i].glass = glass;
            self.cards[i].scratch_ok = false;
        }
        self.update_merge();
        Ok(())
    }
    pub fn params(&self) -> &GlassParams {
        &self.params
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
        self.panels = [None, None, None, None, None];
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
        let map = Glass::make_map(gfx, p.w, p.h, s, p.radius, &self.panel_params)?;
        let glass = Glass::styled(gfx, &self.backdrop, p.x, p.y, p.w, p.h, s, m, p.g, p.radius, &map, style, &self.panel_params)?;
        let layer = gfx.target((p.w + 2 * m) as u32, (p.h + 2 * m) as u32)?;
        self.panels[slot] = Some(GlassPanel { x: p.x, y: p.y, w: p.w, h: p.h, radius: p.radius, glass, layer, lum: 0.3 });
        Ok(())
    }

    pub fn render_panel(&mut self, gfx: &Gfx, slot: usize) -> Result<()> {
        let Some(p) = self.panels[slot].as_mut() else { return Ok(()) };
        if let Ok(cells) = self.probe.measure(gfx, (p.x, p.y, p.w, p.h)) {
            p.lum = cells.iter().sum::<f32>() / 64.0;
        }
        let _ = p.glass.prepare(gfx, &Fx::new(p.lum));
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
        // hover / press response (eased by `step_fx`; the widget below hears the event unchanged, nothing is delayed)
        let on = kind != MouseKind::Leave;
        let press = match (kind, button) {
            (MouseKind::Down, Button::Left) => true,
            (MouseKind::Up, Button::Left) | (MouseKind::Leave, _) => false,
            _ => c.press_on,
        };
        let mut wake = on != c.hover_on || press != c.press_on;
        if on {
            let p = ((px - c.x) as f32, (py - c.y) as f32);
            if c.hover_t <= 0.0 && !c.hover_on {
                // entering from rest: spot and glint start under the pointer instead of sliding in from the last place
                c.spot = p;
                c.spot_goal = p;
                c.glint_u = c.glint_goal();
            }
            wake |= (p.0 - c.spot_goal.0).abs() + (p.1 - c.spot_goal.1).abs() > 0.5;
            c.spot_goal = p;
        }
        c.hover_on = on;
        c.press_on = press;
        if wake {
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
    /// The host changed card `i`'s `cfg` itself (the "Click action" menu): have `take_cfg` hand it over for persisting.
    pub fn touch_cfg(&mut self, i: usize, f: impl FnOnce(&mut Value)) {
        let c = &mut self.cards[i];
        f(&mut c.cfg);
        c.flags.cfg_dirty.set(true);
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
    /// `_light` deg, `_mat` ms into the materialisation, `_mout` ms into the dematerialisation, `_move` [dx, dy] logical px from the grid cell, `_morph` ["s", ms] that far into a size change from the named size (the card's own size is the end size), `_hover` [x, y] pointer in card px (glint + spot + lift + widget hover),
    /// with `_hover_t` 0..1 (the ease-in progress, default 1) and `_press` true (the left button held: press scale, dimmer spot).
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
            if let Some(a) = cfg.get("_morph").and_then(|v| v.as_array()) {
                // ["s", ms]: the card (its spec size is the END size) is `ms` into a size morph from the named size
                let from = a.first().and_then(|v| v.as_str()).and_then(|k| Size::ALL.into_iter().find(|s| s.key() == k));
                let ms = a.get(1).and_then(|v| v.as_f64()).unwrap_or(0.0) as f32;
                let (to, x, y) = (self.cards[i].size, self.cards[i].x, self.cards[i].y);
                if let Some(from) = from.filter(|f| *f != to) {
                    let _ = self.resize_card(gfx, i, from, x, y, false);
                    self.tick_card(gfx, i, now);
                    let _ = self.render_card(gfx, i, now);
                    let _ = self.resize_card(gfx, i, to, x, y, true);
                    self.tick_card(gfx, i, now);
                    if let Some(mo) = self.cards[i].morph.as_mut() {
                        mo.m = motion::Morph::at(mo.m.from, mo.m.to, ms);
                    }
                    self.update_merge();
                }
            }
            if let Some(h) = cfg.get("_hover").and_then(|v| v.as_array()) {
                if let (Some(x), Some(y)) = (h.first().and_then(|v| v.as_f64()), h.get(1).and_then(|v| v.as_f64())) {
                    let (px, py) = (self.cards[i].x + (x as f32 * self.s) as i32, self.cards[i].y + (y as f32 * self.s) as i32);
                    self.mouse(gfx, i, MouseKind::Move, px, py, Button::None, 0.0, now);
                    // `_press`: the left button is held; `_hover_t` 0..1: that far into the hover ease-in (default: fully in)
                    let c = &mut self.cards[i];
                    c.press_on = cfg.get("_press").and_then(|v| v.as_bool()) == Some(true); // the widget hears no click
                    c.hover_t = cfg.get("_hover_t").and_then(|v| v.as_f64()).map_or(1.0, |t| t.clamp(0.0, 1.0) as f32);
                    c.press_t = if c.press_on { 1.0 } else { 0.0 };
                    let rest = if c.press_on { motion::PRESS_SCALE } else { 1.0 + (motion::HOVER_SCALE - 1.0) * motion::smooth(c.hover_t) };
                    c.bump = motion::Bump::resting(rest);
                    c.scratch_ok = false;
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

    /// Something of the glass motion is still moving (light spring, hover / press, appearance cross-fade): keep the animation timer.
    pub fn fx_busy(&self) -> bool {
        (0..self.cards.len()).any(|i| {
            let c = &self.cards[i];
            !c.light_settled || c.hover_busy() || c.morph.is_some() || (c.look - self.look_target(i)).abs() > 1e-3
        })
    }
    /// A size morph settled since the last call: the host re-syncs the backdrop regions (they covered the whole morph) and the window.
    pub fn take_morph_done(&mut self) -> bool {
        std::mem::replace(&mut self.morph_done, false)
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
        let mut morphed = false;
        for i in 0..self.cards.len() {
            // size change: the rectangle springs; the whole area it can touch is recomposed, the last frame lands on the final layer
            if self.cards[i].morph.is_some() {
                let r = self.layer_rect_ext(i, 0);
                let c = &mut self.cards[i];
                if c.morph.as_mut().is_some_and(|mo| mo.m.step(dt)) {
                    c.morph = None;
                    c.scratch_ok = false;
                    self.morph_done = true;
                }
                morphed = true;
                dirty = Some(dirty.map_or(r, |d| union(d, r)));
            }
            let target = self.look_target(i);
            let c = &mut self.cards[i];
            let mut hi_changed = false;
            if !c.light_settled {
                c.light_settled = c.light.step(dt, c.light_drive);
                hi_changed = true;
            }
            // hover / press: progress ramps, the spot trails the pointer (~70 ms), the glint slides along the rim (~80 ms), the scale springs
            let mut scaled = false;
            if c.hover_busy() {
                use motion::{follow, ramp};
                c.hover_t = ramp(c.hover_t, c.hover_on, dt, motion::HOVER_IN, motion::HOVER_OUT);
                c.press_t = ramp(c.press_t, c.press_on, dt, motion::PRESS_IN, motion::PRESS_OUT);
                c.spot = (follow(c.spot.0, c.spot_goal.0, dt, 0.07, 0.25), follow(c.spot.1, c.spot_goal.1, dt, 0.07, 0.25));
                let gu = c.glint_goal();
                c.glint_u = motion::follow_wrap(c.glint_u, gu, dt, 0.08, 5e-4);
                let (b0, goal) = (c.bump.x, c.bump_goal());
                c.bump.step(dt, goal);
                scaled = c.bump.x != b0;
                hi_changed = true;
            }
            let look_changed = (c.look - target).abs() > 1e-4;
            if look_changed {
                c.look = motion::look_step(c.look, target, dt);
            }
            // the hover / press scale (<= 1.2 %) stays inside the shadow margin; the edit transforms (rotation, lift) need the extension
            let e = if self.cards[i].xf.is_id() { 0 } else { ext };
            let r = if look_changed {
                if let Err(e) = self.render_card(gfx, i, now) {
                    eprintln!("mswidgets: render card {i}: {e}");
                }
                Some(self.layer_rect_ext(i, e))
            } else if hi_changed {
                self.cards[i].scratch_ok = false;
                // a scale step (also the last one back to upright) repaints the margin the bigger card covered
                Some(if self.cards[i].upright() && !scaled { self.card_rect(i) } else { self.layer_rect_ext(i, e) })
            } else {
                None
            };
            if let Some(r) = r {
                dirty = Some(dirty.map_or(r, |d| union(d, r)));
            }
        }
        if morphed {
            self.update_merge(); // the necks to the neighbours follow the springing rectangle
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
    /// Record every card's static content again on its next `render_card` (the accent colour changed).
    pub fn invalidate_all(&mut self) {
        for c in &self.cards {
            c.flags.invalidate.set(true);
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
    /// The captured desktop behind the cards, 64 x 36 px BGRA: what the user actually sees (a live / slideshow wallpaper included).
    pub fn backdrop_colors(&self, gfx: &Gfx) -> Result<Vec<u8>> {
        self.probe.colors(gfx, (self.w, self.h), 64, 36)
    }

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
            c.tsh.key = None;
        }
        let blur_max = motion::BLUR_MAX;
        let base_blur = self.params.blur;
        let m_px = self.m;
        let c = &mut self.cards[i];
        c.scratch_ok = false;
        c.layer_stale = false;
        let k = c.look.clamp(0.0, 1.0);
        let blur = if c.xf.blur > 0.002 { base_blur + (blur_max - base_blur) * c.xf.blur } else { 0.0 };
        let ink = if k >= 0.5 { 1 } else { 0 };
        let fx = Fx { lum: c.lum, k, blur };
        // the cached parts of the glass and of the text shadow are brought up to date in draw sessions of their own, before the layer's
        let _ = c.glass.prepare(gfx, &fx);
        let alpha = (0.10 + 0.25 * c.lum) * (1.0 - k) + 0.03 * k;
        if c.tsh.enabled && c.content[ink].is_some() && c.tsh.key != Some((ink, alpha.to_bits())) {
            if let Err(e) = build_text_shadow(gfx, c, ink, alpha, s, (c.w + 2 * m_px) as u32, (c.h + 2 * m_px) as u32) {
                c.tsh.key = None;
                eprintln!("mswidgets: text shadow cache: {e}");
            }
        }
        let dc = &gfx.dc;
        // the glass (shadow, lens, tint, veils, glow, sheen, rim) and the text shadow are rendered into a 16-bit float scratch and dithered down
        // into the 8-bit layer; the content (text, icons, bars) is drawn onto that layer exactly as before, so it stays pixel-identical
        let (lw, lh) = ((c.w + 2 * m_px) as u32, (c.h + 2 * m_px) as u32);
        let hdr = if gfx.fp16 {
            if self.hdr.as_ref().is_none_or(|h| h.0 < lw || h.1 < lh) {
                let (hw, hh) = (self.hdr.as_ref().map_or(0, |h| h.0).max(lw), self.hdr.as_ref().map_or(0, |h| h.1).max(lh));
                self.hdr = Some((hw, hh, gfx.target_hdr(hw, hh)?));
            }
            self.hdr.as_ref().map(|h| h.2.clone())
        } else {
            None
        };
        unsafe {
            dc.SetTarget(hdr.as_ref().unwrap_or(&c.layer));
            dc.BeginDraw();
            dc.SetTransform(&mat(1.0, 0.0, 0.0));
            if hdr.is_some() {
                dc.PushAxisAlignedClip(&rf(0.0, 0.0, lw as f32, lh as f32), D2D1_ANTIALIAS_MODE_ALIASED);
            }
            let r = c.glass.draw(dc, &fx);
            if r.is_ok() {
                // subtle text shadow, stronger over a bright backdrop (CSS: text-shadow 0 1px 8px); nearly none under dark ink
                if let Some(cl) = &c.content[ink] {
                    match (&c.tsh.bmp, c.tsh.key == Some((ink, alpha.to_bits()))) {
                        (Some(b), true) => {
                            let lr = rf(0.0, 0.0, (c.w + 2 * m_px) as f32, (c.h + 2 * m_px) as f32);
                            dc.DrawBitmap(b, Some(&lr), 1.0, D2D1_INTERPOLATION_MODE_NEAREST_NEIGHBOR, Some(&lr), None);
                        }
                        _ => {
                            c.tshadow.SetInput(0, cl, true);
                            let _ = fx_vec4(&c.tshadow, 1, [0.0, 0.0, 0.0, alpha]);
                            if let Ok(o) = c.tshadow.GetOutput() {
                                dc.DrawImage(&o, Some(&pt(0.0, s)), None, D2D1_INTERPOLATION_MODE_LINEAR, D2D1_COMPOSITE_MODE_SOURCE_OVER);
                            }
                        }
                    }
                }
                // glass done: add the dither, then copy the float scratch into the 8-bit layer (a plain copy rounds to the nearest level) and carry on
                // with the content on that layer, in the same draw session
                if let Some(h) = &hdr {
                    dc.PopAxisAlignedClip();
                    gfx.add_noise(lw as f32, lh as f32)?;
                    dc.SetTarget(&c.layer);
                    dc.SetPrimitiveBlend(D2D1_PRIMITIVE_BLEND_COPY);
                    dc.DrawBitmap(h, Some(&rf(0.0, 0.0, lw as f32, lh as f32)), 1.0, D2D1_INTERPOLATION_MODE_NEAREST_NEIGHBOR, Some(&rf(0.0, 0.0, lw as f32, lh as f32)), None);
                    dc.SetPrimitiveBlend(D2D1_PRIMITIVE_BLEND_SOURCE_OVER);
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

    /// The glass of a card without its content (shadow, lens, tint, veils, rim) in a layer-sized bitmap: the source of the morph's 9-slice.
    fn render_shell(&self, gfx: &Gfx, glass: &Glass, (w, h): (i32, i32), k: f32, lum: f32) -> Result<ID2D1Bitmap1> {
        let m = self.m;
        let shell = gfx.target((w + 2 * m) as u32, (h + 2 * m) as u32)?;
        let dc = &gfx.dc;
        unsafe {
            dc.SetTarget(&shell);
            dc.BeginDraw();
            dc.SetTransform(&mat(1.0, 0.0, 0.0));
            let r = glass.draw(dc, &Fx { lum, k: k.clamp(0.0, 1.0), blur: 0.0 });
            let e = dc.EndDraw(None, None);
            dc.SetTarget(None::<&ID2D1Image>);
            r?;
            e?;
        }
        Ok(shell)
    }

    /// One frame of a card that changes size, inside an open frame session: the glass rectangle (9-slice of the shell), the old layout
    /// fading out and the new one fading in, both clipped to the rectangle, the live overlay, and the specular highlights once it has landed.
    unsafe fn draw_morph(&mut self, gfx: &Gfx, i: usize, now: &Now) {
        let (s, m) = (self.s, self.m);
        let base_angle = self.params.light_angle;
        let dc = &gfx.dc;
        let sh = &self.sh;
        let c = &mut self.cards[i];
        let Some(mo) = c.morph.as_ref() else { return };
        let (ax, ay, aw, ah) = mo.m.rect();
        let (x0, y0, w, h) = (ax.round() as i32, ay.round() as i32, aw.round() as i32, ah.round() as i32);
        let (ox, oy) = (x0 - m, y0 - m);
        // 9-slice: the corners (radius, rim, shadow) and the edge bands keep their pixels, the middle is cropped / stretched
        let (sw, shh) = (mo.shell_wh.0 + 2 * m, mo.shell_wh.1 + 2 * m);
        let (dw, dh) = (w + 2 * m, h + 2 * m);
        let inset = (m + (40.0 * s) as i32).min(dw / 2).min(dh / 2).min(sw / 2).min(shh / 2);
        let (dx, dy) = ([0, inset, dw - inset, dw], [0, inset, dh - inset, dh]);
        let (sx, sy) = ([0, inset, sw - inset, sw], [0, inset, shh - inset, shh]);
        for r in 0..3 {
            for q in 0..3 {
                if dx[q + 1] <= dx[q] || dy[r + 1] <= dy[r] {
                    continue;
                }
                let d = rf((ox + dx[q]) as f32, (oy + dy[r]) as f32, (ox + dx[q + 1]) as f32, (oy + dy[r + 1]) as f32);
                let src = rf(sx[q] as f32, sy[r] as f32, sx[q + 1] as f32, sy[r + 1] as f32);
                dc.DrawBitmap(&mo.shell, Some(&d), 1.0, D2D1_INTERPOLATION_MODE_NEAREST_NEIGHBOR, Some(&src), None);
            }
        }
        // the content: recorded in layer-local coordinates, drawn under a translation to the rectangle's layer corner and clipped to the
        // rectangle (the clip is set at the identity transform, in frame px)
        let (q_old, q_new) = mo.m.fades();
        let k = c.look.clamp(0.0, 1.0);
        let (id, t) = (mat(1.0, 0.0, 0.0), mat(1.0, ox as f32, oy as f32));
        dc.SetTransform(&id);
        dc.PushAxisAlignedClip(&rf(x0 as f32, y0 as f32, (x0 + w) as f32, (y0 + h) as f32), D2D1_ANTIALIAS_MODE_PER_PRIMITIVE);
        dc.SetTransform(&t);
        for (a, shadow, list) in [(q_old, &mo.old_shadow, &mo.old), (q_new, &c.tshadow, &c.content)] {
            if a > 0.004 {
                draw_content(dc, shadow, list, k, c.lum, s, a);
            }
        }
        dc.SetTransform(&id);
        dc.PopAxisAlignedClip();
        // the live overlay of the new widget (second hand, equaliser): at the new size, fading in with the new layout
        if (c.tick.animates() || sh.preview || c.frame_once) && q_new > 0.004 {
            dc.SetTransform(&mat(1.0, 0.0, 0.0));
            dc.PushAxisAlignedClip(&rf(x0 as f32, y0 as f32, (x0 + w) as f32, (y0 + h) as f32), D2D1_ANTIALIAS_MODE_PER_PRIMITIVE);
            let light = c.look >= 0.5;
            let fade = q_new < 0.999; // (no layer once fully in: the overlays draw effects, which a layer does not wrap reliably)
            if fade {
                push_opacity(dc, q_new);
            }
            let r = with_cx(c, sh, gfx, now, mat(s, x0 as f32, y0 as f32), light, "overlay", |w, cx| w.overlay(cx));
            if fade {
                dc.PopLayer();
            }
            dc.PopAxisAlignedClip();
            if let Some(Err(e)) = r {
                if !c.err_logged {
                    c.err_logged = true;
                    eprintln!("mswidgets: widget '{}' overlay failed: {e}", c.type_id);
                }
            }
            c.frame_once = false;
        }
        // the specular rim belongs to the final geometry: it fades in only as the rectangle lands on it
        let landed = motion::smooth((c.morph.as_ref().map_or(1.0, |mo| mo.m.progress()) - 0.96) / 0.04);
        if landed > 0.004 {
            let mut hi = hi_of(c, base_angle, s);
            hi.alpha = landed;
            c.glass.draw_hi(dc, c.x as f32, c.y as f32, &hi);
        }
        dc.SetTransform(&mat(1.0, 0.0, 0.0));
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
        // saturation .45, brightness .95: out_o = k * (.45 * in_o + .55 * luma(in)); the colours stay readable (rings, accents), only calmer
        let (k, a) = (0.95f32, 0.45f32);
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
            if self.cards[i].upright() || self.cards[i].morph.is_some() || !hit(&lr, &clip) {
                continue; // (a card changing size is drawn straight into the frame by `draw_morph`)
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
            let shadow = if self.cards[i].xf.lift > 0.0 { Some(self.lift_shadow(gfx, self.cards[i].w, self.cards[i].h, self.radius_of(self.cards[i].size))?) } else { None };
            let (sh, dim) = (&self.sh, self.dim);
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
                // "Dim widgets on desktop" holds for a transformed (hovered, pressed) card too
                match (&c.dimfx, dim) {
                    (Some((_, out)), true) => dc.DrawImage(out, None, None, D2D1_INTERPOLATION_MODE_NEAREST_NEIGHBOR, D2D1_COMPOSITE_MODE_SOURCE_OVER),
                    _ => dc.DrawImage(&c.layer, None, None, D2D1_INTERPOLATION_MODE_NEAREST_NEIGHBOR, D2D1_COMPOSITE_MODE_SOURCE_OVER),
                }
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
                let hi = hi_of(c, base_angle, s);
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
        // a hovered / pressed card keeps composing through its scratch (no re-allocation per animation end)
        for c in self.cards.iter_mut().filter(|c| c.upright()) {
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
                if self.cards[i].morph.is_some() {
                    self.draw_morph(gfx, i, now);
                    continue;
                }
                let c = &self.cards[i];
                if c.upright() {
                    let at = pt((c.x - m) as f32, (c.y - m) as f32);
                    match (&c.dimfx, self.dim) {
                        (Some((_, out)), true) => dc.DrawImage(out, Some(&at), None, D2D1_INTERPOLATION_MODE_NEAREST_NEIGHBOR, D2D1_COMPOSITE_MODE_SOURCE_OVER),
                        _ => dc.DrawImage(&c.layer, Some(&at), None, D2D1_INTERPOLATION_MODE_NEAREST_NEIGHBOR, D2D1_COMPOSITE_MODE_SOURCE_OVER),
                    }
                } else if let Some(sc) = &c.scratch {
                    let (lw, lh) = ((c.w + 2 * m) as f32, (c.h + 2 * m) as f32);
                    let t = xf_matrix(c.x as f32 + c.w as f32 / 2.0, c.y as f32 + c.h as f32 / 2.0, c.xf.scale * c.bump.x, c.xf.rot);
                    // a pure scale (hover / press) resamples with the cubic filter: the text stays crisp at 1.2 %
                    let interp = if c.xf.rot.abs() < 0.004 { D2D1_INTERPOLATION_MODE_HIGH_QUALITY_CUBIC } else { D2D1_INTERPOLATION_MODE_LINEAR };
                    dc.SetTransform(&t);
                    let dest = rf((c.x - m) as f32, (c.y - m) as f32, (c.x - m) as f32 + lw, (c.y - m) as f32 + lh);
                    match (&c.soften, c.xf.blur > 0.002) {
                        (Some((_, e)), true) => {
                            // materialise: the composed card is diffused too (blur, then the fade)
                            if let Ok(o) = e.GetOutput() {
                                dc.DrawImage(&o, Some(&pt(dest.left, dest.top)), None, D2D1_INTERPOLATION_MODE_LINEAR, D2D1_COMPOSITE_MODE_SOURCE_OVER);
                            }
                        }
                        _ => dc.DrawBitmap(sc, Some(&dest), c.xf.alpha.clamp(0.0, 1.0), interp, Some(&rf(0.0, 0.0, lw, lh)), None),
                    }
                    dc.SetTransform(&mat(1.0, 0.0, 0.0));
                }
            }
            self.draw_merges(gfx, clip);
            let sh = &self.sh;
            for c in self.cards.iter_mut() {
                let lr = clip_rect(c, m, self.w, self.h);
                if !c.upright() || !hit(&lr, &clip) || !(c.tick.animates() || sh.preview || c.frame_once) {
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
                if !c.upright() || !hit(&clip_rect(c, 0, self.w, self.h), &clip) {
                    continue;
                }
                c.glass.draw_hi(dc, c.x as f32, c.y as f32, &hi_of(c, base_angle, s));
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
        let mut cx = Cx::new(CxParts { gfx, res: &self.sh.res, flags: &flags, cfg: &mut cfg, size: Size::S, scale: self.s, now: *now, preview: true, lum: 0.2, lang, hover: None, waker: Waker::default(), utc_off: 0, base, light: false });
        cx.plain = true;
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
        // the FP16 frame is converted to dithered 8 bit on the GPU first (only the dirty rectangle)
        let q;
        let src = if Gfx::is_fp16(&self.frame) {
            q = gfx.quantize(&self.frame, r)?;
            &q
        } else {
            &self.frame
        };
        unsafe {
            let sr = D2D_RECT_U { left: r.left as u32, top: r.top as u32, right: r.right as u32, bottom: r.bottom as u32 };
            cpu.CopyFromBitmap(Some(&D2D_POINT_2U { x: 0, y: 0 }), src, Some(&sr))?;
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
    let b = c.bounds();
    clip(RECT { left: b.left - m, top: b.top - m, right: b.right + m, bottom: b.bottom + m }, w, h)
}

impl Probe {
    fn new(gfx: &Gfx, backdrop: &ID2D1Bitmap1) -> Result<Probe> {
        unsafe {
            let scale = gfx.dc.CreateEffect(&CLSID_D2D1Scale)?;
            scale.SetInput(0, backdrop, true);
            fx_vec2(&scale, 1, [0.0, 0.0])?; // centre point
            fx_u32(&scale, 2, 5)?; // HIGH_QUALITY_CUBIC (prefiltered when shrinking)
            Ok(Probe { out: scale.GetOutput()?, scale, tgt: gfx.target8(8, 8)?, cpu: gfx.cpu_read(8, 8)?, col: Default::default() })
        }
    }

    /// The whole backdrop (`bw` x `bh` px) shrunk to `w` x `h` as premultiplied BGRA rows (a few ms: the wallpaper accent only).
    fn colors(&self, gfx: &Gfx, (bw, bh): (i32, i32), w: u32, h: u32) -> Result<Vec<u8>> {
        let (sx, sy) = (w as f32 / bw.max(1) as f32, h as f32 / bh.max(1) as f32);
        fx_vec2(&self.scale, 0, [sx, sy])?;
        let mut bufs = self.col.borrow_mut();
        if bufs.as_ref().is_none_or(|b| (b.0, b.1) != (w, h)) {
            *bufs = Some((w, h, gfx.target8(w, h)?, gfx.cpu_read(w, h)?));
        }
        let Some((_, _, tgt, cpu)) = bufs.as_ref() else { return Err(windows::core::Error::empty()) };
        let dc = &gfx.dc;
        unsafe {
            dc.SetTarget(tgt);
            dc.BeginDraw();
            dc.SetTransform(&mat(1.0, 0.0, 0.0));
            dc.Clear(None);
            dc.DrawImage(&self.out, Some(&pt(0.0, 0.0)), Some(&rf(0.0, 0.0, w as f32, h as f32)), D2D1_INTERPOLATION_MODE_LINEAR, D2D1_COMPOSITE_MODE_SOURCE_COPY);
            let e = dc.EndDraw(None, None);
            dc.SetTarget(None::<&ID2D1Image>);
            e?;
        }
        let mut px = Vec::with_capacity((w * h * 4) as usize);
        gfx.read_rect(tgt, cpu, RECT { left: 0, top: 0, right: w as i32, bottom: h as i32 }, |_, row| px.extend_from_slice(row))?;
        Ok(px)
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

    /// Banding regression (0.7.3): the soft shadow under a dark glass card on a near-black backdrop is a ramp of a few levels. Rounded to 8 bit it was
    /// a staircase (every pixel of a row equal, 1-level steps between the rows); the dithered FP16 pipeline mixes the two neighbouring levels inside
    /// the transition rows. Measured on the rows of the shadow ramp (alpha): the share of steps that run against the trend of the row (0 for a rounded ramp) must be at least 4 %.
    #[test]
    fn dark_glass_shadow_is_dithered_not_banded() {
        unsafe {
            let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_MULTITHREADED);
        }
        let gfx = Gfx::new(None).unwrap();
        let px: Vec<u8> = (0..800 * 600).flat_map(|_| [18u8, 14, 13, 255]).collect(); // BGRA (13, 14, 18): a very dark smooth wallpaper
        let bd = gfx.from_bgra(800, 600, &px).unwrap();
        let env = SceneEnv { preview: true, hwnd: 0, lang: Lang::Ru, utc_off: 0 };
        let now = Now::sample();
        let mut sc = Scene::empty(&gfx, 800, 600, 1.0, 0.5, &bd, &env).unwrap();
        sc.add_card(&gfx, &CardSpec { id: "a".into(), type_id: "clock".into(), size: Size::S, cfg: json!({"_appearance": "dark"}), x: 100, y: 60, w: 170, h: 170 }).unwrap();
        sc.tick_card(&gfx, 0, &now);
        sc.measure_lum(&gfx, 1, false).unwrap();
        sc.render_card(&gfx, 0, &now).unwrap();
        let full = RECT { left: 0, top: 0, right: 800, bottom: 600 };
        sc.composite(&gfx, full, None, &now).unwrap();
        let mut rows = Vec::new();
        sc.read_rect(&gfx, RECT { left: 140, top: 232, right: 230, bottom: 232 + 44 }, |_, row| {
            rows.push(row.chunks(4).map(|p| p[3]).collect::<Vec<u8>>());
        })
        .unwrap();
        let ramp: Vec<&Vec<u8>> = rows.iter().filter(|r| r.iter().any(|&v| v > 0)).collect(); // rows with shadow: its alpha is the ramp (the frame is transparent, as in the window)
        // a rounded ramp only ever steps one way along a row; a dithered one flips back and forth between the two neighbouring levels
        let (mut pairs, mut flips) = (0usize, 0usize);
        for r in &ramp {
            let trend = r[r.len() - 1] as i32 - r[0] as i32;
            for w in r.windows(2) {
                pairs += 1;
                if trend != 0 && trend.signum() * (w[1] as i32 - w[0] as i32) < 0 {
                    flips += 1;
                }
            }
        }
        assert!(ramp.len() >= 10, "the shadow darkens {} rows", ramp.len());
        if gfx.fp16 {
            assert!(flips * 25 >= pairs, "{flips} of {pairs} steps run against the trend of the row: the shadow is banded, not dithered");
        }
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
        // hover: eases in and then rests (no timer while the pointer stays), press + release spring back, leave goes idle and upright
        let (cx0, cy0) = (sc.cards[0].x + 60, sc.cards[0].y + 70);
        let settle = |sc: &mut Scene| {
            let mut n = 0;
            while sc.fx_busy() && n < 100 {
                sc.step_fx(&gfx, 0.016, &now);
                n += 1;
            }
            n
        };
        sc.mouse(&gfx, 0, MouseKind::Move, cx0, cy0, Button::None, 0.0, &now);
        assert!(sc.take_fx_wake() && sc.fx_busy());
        let n = settle(&mut sc);
        assert!(n > 8 && n < 60 && sc.cards[0].hover_t == 1.0 && sc.cards[0].bump.x == motion::HOVER_SCALE, "hover in {n} steps");
        sc.composite(&gfx, full, None, &now).unwrap();
        sc.mouse(&gfx, 0, MouseKind::Move, cx0 + 1, cy0, Button::None, 0.0, &now); // a sub-pixel jitter is no animation
        assert!(!sc.fx_busy() || settle(&mut sc) < 30);
        sc.mouse(&gfx, 0, MouseKind::Down, cx0, cy0, Button::Left, 0.0, &now);
        settle(&mut sc);
        assert_eq!(sc.cards[0].bump.x, motion::PRESS_SCALE);
        sc.mouse(&gfx, 0, MouseKind::Up, cx0, cy0, Button::Left, 0.0, &now);
        settle(&mut sc);
        assert_eq!(sc.cards[0].bump.x, motion::HOVER_SCALE);
        sc.mouse(&gfx, 0, MouseKind::Leave, 0, 0, Button::None, 0.0, &now);
        let n = settle(&mut sc);
        assert!(n < 60 && !sc.fx_busy() && sc.cards[0].upright() && sc.cards[0].hover_t == 0.0, "leave in {n} steps");
    }

    /// The settings sliders: `set_params` swaps every card's effect graph at run time; the widget instances stay, the pixels change
    /// (a rounder corner is transparent where the square one was opaque), the corner radius follows `radius_k`, and it is cheap enough for 10 Hz.
    #[test]
    fn set_params_changes_the_look_live() {
        unsafe {
            let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_MULTITHREADED);
        }
        let gfx = Gfx::new(None).unwrap();
        let bd = PlainBackdrop::new(&gfx, 800, 400).unwrap();
        let env = SceneEnv { preview: true, hwnd: 0, lang: Lang::Ru, utc_off: 0 };
        let specs: Vec<CardSpec> = (0..4).map(|k| CardSpec { id: format!("clock-{k}"), type_id: "clock".into(), size: Size::S, cfg: json!({}), x: 20 + 190 * k, y: 60, w: 170, h: 170 }).collect();
        let mut sc = Scene::new(&gfx, 800, 400, 1.0, 0.5, bd.bitmap(), &specs, &env).unwrap();
        let now = Now::sample();
        let full = RECT { left: 0, top: 0, right: 800, bottom: 400 };
        let render = |sc: &mut Scene| {
            for i in 0..sc.len() {
                sc.render_card(&gfx, i, &now).unwrap();
            }
            sc.composite(&gfx, full, None, &now).unwrap();
        };
        render(&mut sc);
        let uids: Vec<usize> = sc.cards.iter().map(|c| c.uid).collect();
        assert!(alpha_at(&mut sc, &gfx, 20 + 3, 60 + 3) < 40, "default corner is rounded");
        let mut p = *sc.params();
        p.radius_k = 0.15;
        p.blur = 9.0;
        let t = std::time::Instant::now();
        sc.set_params(&gfx, &p).unwrap();
        render(&mut sc);
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        eprintln!("set_params + render of 4 cards: {ms:.1} ms");
        assert!(ms < 400.0, "{ms} ms");
        assert!(alpha_at(&mut sc, &gfx, 20 + 3, 60 + 3) > 150, "the square corner is opaque");
        assert_eq!(sc.params().blur, 9.0);
        assert_eq!(sc.cards.iter().map(|c| c.uid).collect::<Vec<_>>(), uids);
        assert!((sc.radius_of(Size::S) - 28.0 * 0.15).abs() < 1e-4);
        // the same params again are a no-op
        sc.set_params(&gfx, &p).unwrap();
    }

    /// A card that grows towards a neighbour pulls the liquid neck while it is springing (the merge follows the animated rectangle),
    /// and the animation stops asking for frames once it has landed.
    #[test]
    fn size_morph_keeps_the_merge_bridge_and_goes_idle() {
        unsafe {
            let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_MULTITHREADED);
        }
        let gfx = Gfx::new(None).unwrap();
        let bd = PlainBackdrop::new(&gfx, 800, 400).unwrap();
        let env = SceneEnv { preview: true, hwnd: 0, lang: Lang::Ru, utc_off: 0 };
        let mut sc = Scene::empty(&gfx, 800, 400, 1.0, 0.5, bd.bitmap(), &env).unwrap();
        let now = Now::sample();
        let full = RECT { left: 0, top: 0, right: 800, bottom: 400 };
        // a at x 40 (S, ends at 210), b at 226: a 16 px gap, so S -> M (ends at 396) runs INTO b: the necks appear on the way
        let a = sc.add_card(&gfx, &CardSpec { id: "clock-a".into(), type_id: "clock".into(), size: Size::S, cfg: json!({}), x: 40, y: 60, w: 170, h: 170 }).unwrap();
        let b = sc.add_card(&gfx, &CardSpec { id: "clock-b".into(), type_id: "clock".into(), size: Size::S, cfg: json!({}), x: 226, y: 60, w: 170, h: 170 }).unwrap();
        for i in [a, b] {
            sc.tick_card(&gfx, i, &now);
            sc.render_card(&gfx, i, &now).unwrap();
        }
        assert!(sc.merging().is_empty(), "resting cards 16 px apart do not merge");
        sc.resize_card(&gfx, a, Size::M, 40, 60, true).unwrap();
        sc.tick_card(&gfx, a, &now);
        sc.render_card(&gfx, a, &now).unwrap();
        assert!(sc.merging().is_empty(), "the morph starts on the old S rectangle");
        let mut merged = false;
        for _ in 0..40 {
            let _ = sc.step_fx(&gfx, 0.004, &now);
            if !sc.merging().is_empty() {
                merged = true;
                sc.composite(&gfx, full, None, &now).unwrap(); // the neck pass runs over the morphing card
                break;
            }
        }
        assert!(merged, "the springing rectangle reaches the neighbour: a neck");
        // (this layout is deliberately overlapping at the end, so only idleness is checked: no frame is asked for once landed)
        let mut n = 0;
        while sc.fx_busy() && n < 200 {
            let _ = sc.step_fx(&gfx, 0.016, &now);
            n += 1;
        }
        assert!(!sc.fx_busy() && sc.step_fx(&gfx, 0.016, &now).is_none(), "settled: idle after {n} steps");
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
        sc.resize_card(&gfx, i, Size::M, 100, 100, false).unwrap();
        assert_eq!((sc.cards[i].w, sc.cards[i].h, sc.cards[i].uid), (356, 170, uid));
        sc.tick_card(&gfx, i, &now);
        sc.render_card(&gfx, i, &now).unwrap();
        sc.composite(&gfx, full, None, &now).unwrap();
        let (in_a, out_a) = (alpha_at(&mut sc, &gfx, 400, 185), alpha_at(&mut sc, &gfx, 700, 500));
        assert!(in_a > 150 && out_a == 0, "inside {in_a}, outside {out_a}");
        assert!(sc.cards[i].morph.is_none() && !sc.fx_busy(), "no animation asked for, none running");

        // animated M -> L: starts on the M rectangle, grows through the 9-slice path, lands on the L layer and then costs nothing
        sc.resize_card(&gfx, i, Size::L, 100, 100, true).unwrap();
        sc.tick_card(&gfx, i, &now);
        sc.render_card(&gfx, i, &now).unwrap();
        assert!(sc.cards[i].morph.is_some() && sc.fx_busy());
        assert_eq!(sc.card_rect(i).bottom, 100 + 356 + 8, "the dirty area covers the whole morph");
        sc.composite(&gfx, full, None, &now).unwrap();
        assert!(alpha_at(&mut sc, &gfx, 400, 185) > 150, "the glass is there at the start");
        assert_eq!(alpha_at(&mut sc, &gfx, 400, 400), 0, "the new area is still empty");
        let (mut n, mut mid) = (0, 0);
        while sc.fx_busy() && n < 200 {
            let _ = sc.step_fx(&gfx, 0.016, &now);
            n += 1;
            if n == 8 {
                sc.composite(&gfx, full, None, &now).unwrap();
                mid = alpha_at(&mut sc, &gfx, 400, 130 + 170 + 20);
            }
        }
        assert!(n < 60 && !sc.fx_busy(), "settles in {n} steps");
        assert!(sc.take_morph_done() && !sc.take_morph_done());
        assert!(mid > 100, "the glass reached below the old bottom edge mid-way: {mid}");
        sc.composite(&gfx, full, None, &now).unwrap();
        assert!(alpha_at(&mut sc, &gfx, 400, 400) > 150, "landed on the L card");
        // and back: the shell of the bigger (old) glass
        sc.resize_card(&gfx, i, Size::M, 100, 100, true).unwrap();
        sc.tick_card(&gfx, i, &now);
        sc.render_card(&gfx, i, &now).unwrap();
        sc.composite(&gfx, full, None, &now).unwrap();
        assert!(alpha_at(&mut sc, &gfx, 400, 400) > 150, "still L-sized at the start of the shrink");
        while sc.fx_busy() {
            let _ = sc.step_fx(&gfx, 0.016, &now);
        }
        sc.composite(&gfx, full, None, &now).unwrap();
        assert_eq!(alpha_at(&mut sc, &gfx, 400, 400), 0, "shrunk to M");

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

    /// A card whose recorded content holds layers (the music cover clip) can be redrawn any number of times: its text shadow effect plays that
    /// content in a draw session of its own. (As an effect input and directly inside ONE session D2D fails the third redraw with
    /// D2DERR_LAYER_ALREADY_IN_USE; `MSWIDGETS_NO_GLASS_CACHE=1` keeps that old behaviour.) The luminance changes between the redraws.
    #[test]
    fn layered_content_survives_repeated_redraws() {
        unsafe {
            let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_MULTITHREADED);
        }
        if !crate::glass::cache_enabled() {
            return;
        }
        let gfx = Gfx::new(None).unwrap();
        let bd = PlainBackdrop::new(&gfx, 1400, 800).unwrap();
        let env = SceneEnv { preview: true, hwnd: 0, lang: Lang::Ru, utc_off: 0 };
        let items = crate::layout::parse_cards("calendar:s@0,0;reminders:s@1,0;music:s@2,0;sysmon:s@3,0;weather:m@0,1;network:s@2,1;controls:m@3,1;music:l@5,0").unwrap();
        let mut sc = Scene::new(&gfx, 1400, 800, 1.0, 0.5, bd.bitmap(), &specs(&items, 1.0), &env).unwrap();
        let now = Now::sample();
        for i in 0..sc.len() {
            sc.tick_card(&gfx, i, &now);
        }
        sc.measure_lum(&gfx, 255, false).unwrap();
        for n in 0..12 {
            sc.nudge_lum(0.01 * (n % 3) as f32);
            for i in 0..sc.len() {
                sc.render_card(&gfx, i, &now).unwrap_or_else(|e| panic!("redraw {n} of card {i}: {e}"));
            }
        }
    }
}
