//! `Canvas`: the thin drawing surface the kit's controls paint on. It wraps the device context of a [`Gfx`] and the text cache [`Res`], and works in
//! **logical pixels**: the host (or [`crate::testkit`]) sets the device-context transform to the DPI scale before it hands the canvas out.
//!
//! Everything a control needs and nothing more: filled / stroked rounded rectangles (strokes sit INSIDE the rectangle like a CSS border), circles,
//! lines, text through the shared layout cache, clips, group opacity, scale/translate around a point (press effects) and soft shadows.
//! Colours are straight RGBA [`Color`]s; the canvas does not re-ink them (that is the job of a [`crate::theme::Theme`]).
//! Layers use content bounds of +-1e7: the "infinite" +-3e38 produces an EMPTY layer on the WARP software device (found by the headless tests).
use crate::color::Color;
use crate::geom::{Pt, Rect};
use crate::gfx::*;
use crate::text::{Align, Res, Style, VAlign};
use std::cell::{Cell, RefCell};
use std::mem::ManuallyDrop;
use windows::core::{Interface, Result};
use windows::Foundation::Numerics::Matrix3x2;
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::DirectWrite::*;

/// Matrix product `a * b` (apply `a` first, then `b`, like `ID2D1DeviceContext::SetTransform` composition).
pub fn mul(a: &Matrix3x2, b: &Matrix3x2) -> Matrix3x2 {
    Matrix3x2 {
        M11: a.M11 * b.M11 + a.M12 * b.M21,
        M12: a.M11 * b.M12 + a.M12 * b.M22,
        M21: a.M21 * b.M11 + a.M22 * b.M21,
        M22: a.M21 * b.M12 + a.M22 * b.M22,
        M31: a.M31 * b.M11 + a.M32 * b.M21 + b.M31,
        M32: a.M31 * b.M12 + a.M32 * b.M22 + b.M32,
    }
}

/// Scale by `s` around the point `c`.
pub fn scale_about(c: Pt, sx: f32, sy: f32) -> Matrix3x2 {
    Matrix3x2 { M11: sx, M12: 0.0, M21: 0.0, M22: sy, M31: c.0 - c.0 * sx, M32: c.1 - c.1 * sy }
}

pub fn translation(dx: f32, dy: f32) -> Matrix3x2 {
    Matrix3x2 { M11: 1.0, M12: 0.0, M21: 0.0, M22: 1.0, M31: dx, M32: dy }
}

/// See the module doc. Cheap to create (three references); make one per paint.
pub struct Canvas<'a> {
    pub gfx: &'a Gfx,
    pub res: &'a Res,
    pub dc: &'a ID2D1DeviceContext,
    /// device px per logical px, for code that must snap to device pixels (hairlines)
    pub scale: f32,
    tf: Cell<Matrix3x2>,
    clips: RefCell<Vec<bool>>,
}

fn clamp_r(r: Rect, rad: f32) -> f32 {
    rad.min(r.w / 2.0).min(r.h / 2.0).max(0.0)
}

impl<'a> Canvas<'a> {
    /// A canvas over `gfx.dc`. `scale` = device px per logical px; the caller has set the transform to that scale already.
    pub fn new(gfx: &'a Gfx, res: &'a Res, scale: f32) -> Canvas<'a> {
        let mut m = Matrix3x2::identity();
        unsafe { gfx.dc.GetTransform(&mut m) };
        Canvas { gfx, res, dc: &gfx.dc, scale, tf: Cell::new(m), clips: RefCell::new(Vec::new()) }
    }

    fn brush(&self, c: Color) -> &ID2D1SolidColorBrush {
        unsafe { self.res.brush().SetColor(&c.d2d()) };
        self.res.brush()
    }

    // ------------------------------------------------------------ shapes

    pub fn fill_rect(&self, r: Rect, c: Color) {
        if c.a <= 0.0 {
            return;
        }
        unsafe { self.dc.FillRectangle(&rf(r.x, r.y, r.right(), r.bottom()), self.brush(c)) };
    }

    pub fn fill_rr(&self, r: Rect, rad: f32, c: Color) {
        if c.a <= 0.0 || r.w <= 0.0 || r.h <= 0.0 {
            return;
        }
        unsafe { self.dc.FillRoundedRectangle(&rr(r.x, r.y, r.w, r.h, clamp_r(r, rad)), self.brush(c)) };
    }

    /// Border of width `w` that stays INSIDE `r` (a CSS `border`): the stroke is centred `w / 2` in from the edge.
    pub fn stroke_rr(&self, r: Rect, rad: f32, w: f32, c: Color) {
        if c.a <= 0.0 || w <= 0.0 {
            return;
        }
        let i = r.inset(w / 2.0);
        if i.w <= 0.0 || i.h <= 0.0 {
            return;
        }
        unsafe { self.dc.DrawRoundedRectangle(&rr(i.x, i.y, i.w, i.h, clamp_r(i, rad - w / 2.0)), self.brush(c), w, self.res.round()) };
    }

    pub fn fill_circle(&self, c: Pt, r: f32, col: Color) {
        if col.a <= 0.0 || r <= 0.0 {
            return;
        }
        unsafe { self.dc.FillEllipse(&D2D1_ELLIPSE { point: pt(c.0, c.1), radiusX: r, radiusY: r }, self.brush(col)) };
    }

    pub fn stroke_circle(&self, c: Pt, r: f32, w: f32, col: Color) {
        if col.a <= 0.0 || r <= 0.0 {
            return;
        }
        unsafe { self.dc.DrawEllipse(&D2D1_ELLIPSE { point: pt(c.0, c.1), radiusX: r, radiusY: r }, self.brush(col), w, self.res.round()) };
    }

    /// Line with round caps.
    pub fn line(&self, a: Pt, b: Pt, w: f32, c: Color) {
        if c.a <= 0.0 {
            return;
        }
        unsafe { self.dc.DrawLine(pt(a.0, a.1), pt(b.0, b.1), self.brush(c), w, self.res.round()) };
    }

    /// Arc of a circle, `start` degrees clockwise from 12 o'clock, `sweep` clockwise (the spinner and the circular progress).
    pub fn arc(&self, c: Pt, r: f32, w: f32, start: f32, sweep: f32, col: Color) {
        if sweep.abs() < 0.05 || col.a <= 0.0 {
            return;
        }
        if sweep.abs() >= 359.9 {
            return self.stroke_circle(c, r, w, col);
        }
        let at = |deg: f32| {
            let a = deg.to_radians();
            pt(c.0 + r * a.sin(), c.1 - r * a.cos())
        };
        unsafe {
            let Ok(g) = self.gfx.d2d.CreatePathGeometry() else { return };
            let Ok(sink) = g.Open() else { return };
            sink.BeginFigure(at(start), D2D1_FIGURE_BEGIN_HOLLOW);
            sink.AddArc(&D2D1_ARC_SEGMENT {
                point: at(start + sweep),
                size: D2D_SIZE_F { width: r, height: r },
                rotationAngle: 0.0,
                sweepDirection: if sweep > 0.0 { D2D1_SWEEP_DIRECTION_CLOCKWISE } else { D2D1_SWEEP_DIRECTION_COUNTER_CLOCKWISE },
                arcSize: if sweep.abs() > 180.0 { D2D1_ARC_SIZE_LARGE } else { D2D1_ARC_SIZE_SMALL },
            });
            sink.EndFigure(D2D1_FIGURE_END_OPEN);
            if sink.Close().is_ok() {
                self.dc.DrawGeometry(&g, self.brush(col), w, self.res.round());
            }
        }
    }

    /// Fill and / or stroke an arbitrary geometry (SVG paths: see `svgpath`). The caller sets any transform first.
    pub fn geometry(&self, g: &ID2D1Geometry, fill: Option<Color>, stroke: Option<(Color, f32)>) {
        unsafe {
            if let Some(c) = fill.filter(|c| c.a > 0.0) {
                self.dc.FillGeometry(g, self.brush(c), None);
            }
            if let Some((c, w)) = stroke.filter(|(c, w)| c.a > 0.0 && *w > 0.0) {
                self.dc.DrawGeometry(g, self.brush(c), w, self.res.round());
            }
        }
    }

    /// A built-in vector glyph (24 x 24 design box) fitted into the centre of `r`, painted with `color`.
    pub fn glyph(&self, g: crate::icons::Glyph, r: Rect, color: Color) {
        let Ok(shapes) = self.res.glyph(self.gfx, g) else { return };
        let k = r.w.min(r.h) / 24.0;
        let m = Matrix3x2 { M11: k, M12: 0.0, M21: 0.0, M22: k, M31: r.cx() - 12.0 * k, M32: r.cy() - 12.0 * k };
        self.with_transform(m, |cv| unsafe {
            let b = cv.brush(color);
            for (geom, paint) in shapes.iter() {
                match *paint {
                    crate::icons::Paint::Fill => cv.dc.FillGeometry(geom, b, None::<&ID2D1Brush>),
                    crate::icons::Paint::Stroke(w) => cv.dc.DrawGeometry(geom, b, w, cv.res.round()),
                    crate::icons::Paint::Both(w) => {
                        cv.dc.FillGeometry(geom, b, None::<&ID2D1Brush>);
                        cv.dc.DrawGeometry(geom, b, w, cv.res.round());
                    }
                }
            }
        });
    }

    /// A soft drop shadow under a rounded rectangle, made of stacked translucent rounded rectangles (no blur effect, so it is cheap and
    /// identical on WARP and on a GPU). `blur` is the reach in px, `dy` the vertical offset, `c` the colour at the darkest.
    pub fn shadow_rr(&self, r: Rect, rad: f32, blur: f32, dy: f32, c: Color) {
        let n = (blur / 2.0).ceil().clamp(1.0, 12.0) as i32;
        for i in (0..n).rev() {
            let t = (i + 1) as f32 / n as f32;
            let grow = blur * t;
            let a = c.a * (1.0 - t).powi(2) / n as f32 * 2.2;
            self.fill_rr(Rect::new(r.x - grow, r.y - grow + dy, r.w + 2.0 * grow, r.h + 2.0 * grow), rad + grow, c.alpha(a.min(1.0)));
        }
    }

    /// The focus ring: 2 px accent outline 1 px outside `r` (see `input::focus_ring_geometry`), drawn at opacity `k` (it fades in).
    pub fn focus_ring(&self, r: Rect, rad: f32, accent: Color, k: f32) {
        if k <= 0.0 {
            return;
        }
        let (fr, frad) = crate::input::focus_ring_geometry(r, rad);
        let w = crate::input::FOCUS_RING_WIDTH;
        self.stroke_rr(fr.inset(-w / 2.0), frad + w / 2.0, w, accent.fade(k));
    }

    // ------------------------------------------------------------ text

    /// The (cached) layout of `s` in a `w` x `h` box; callers must not modify it.
    pub fn layout(&self, s: &str, st: Style, w: f32, h: f32) -> Result<IDWriteTextLayout> {
        self.res.layout(self.gfx, s, st, w, h)
    }

    /// Text inside `r` (alignment, ellipsis and wrapping come from `st`).
    pub fn text(&self, r: Rect, s: &str, st: Style, c: Color) {
        if s.is_empty() || c.a <= 0.0 {
            return;
        }
        if let Ok(l) = self.layout(s, st, r.w, r.h) {
            unsafe { self.dc.DrawTextLayout(pt(r.x, r.y), &l, self.brush(c), D2D1_DRAW_TEXT_OPTIONS_NONE) };
        }
    }

    /// Natural (width, height) of `s`; `max_w` only matters for wrapped styles.
    pub fn measure(&self, s: &str, st: Style, max_w: f32) -> (f32, f32) {
        let st = Style { valign: VAlign::Top, ellipsis: false, align: Align::Left, ..st };
        let Ok(l) = self.layout(s, st, if st.wrap { max_w } else { 4096.0 }, 4096.0) else { return (0.0, 0.0) };
        let mut m = DWRITE_TEXT_METRICS::default();
        unsafe {
            let _ = l.GetMetrics(&mut m);
        }
        (m.widthIncludingTrailingWhitespace.max(m.width), m.height)
    }

    pub fn text_width(&self, s: &str, st: Style) -> f32 {
        self.measure(s, st, 4096.0).0
    }

    // ------------------------------------------------------------ clip, opacity, transform

    /// Clip everything drawn until `pop_clip` to a plain rectangle.
    pub fn clip(&self, r: Rect) {
        unsafe { self.dc.PushAxisAlignedClip(&rf(r.x, r.y, r.right(), r.bottom()), D2D1_ANTIALIAS_MODE_PER_PRIMITIVE) };
        self.clips.borrow_mut().push(false);
    }

    /// Clip to an anti-aliased rounded rectangle until `pop_clip`.
    pub fn clip_rr(&self, r: Rect, rad: f32) {
        unsafe {
            let Ok(g) = self.gfx.d2d.CreateRoundedRectangleGeometry(&rr(r.x, r.y, r.w, r.h, clamp_r(r, rad))) else { return self.clips.borrow_mut().push(false) };
            let mut lp = D2D1_LAYER_PARAMETERS1 {
                contentBounds: rf(-1.0e7, -1.0e7, 1.0e7, 1.0e7),
                geometricMask: ManuallyDrop::new(Some(g.cast().unwrap_or_else(|_| unreachable!()))),
                maskAntialiasMode: D2D1_ANTIALIAS_MODE_PER_PRIMITIVE,
                maskTransform: mat(1.0, 0.0, 0.0),
                opacity: 1.0,
                opacityBrush: ManuallyDrop::new(None),
                layerOptions: D2D1_LAYER_OPTIONS1_NONE,
            };
            self.dc.PushLayer(&lp, None::<&ID2D1Layer>);
            ManuallyDrop::drop(&mut lp.geometricMask);
            self.clips.borrow_mut().push(true);
        }
    }

    pub fn pop_clip(&self) {
        let layer = self.clips.borrow_mut().pop();
        unsafe {
            match layer {
                Some(true) => self.dc.PopLayer(),
                Some(false) => self.dc.PopAxisAlignedClip(),
                None => {}
            }
        }
    }

    pub fn with_clip(&self, r: Rect, f: impl FnOnce(&Canvas)) {
        self.clip(r);
        f(self);
        self.pop_clip();
    }

    pub fn with_clip_rr(&self, r: Rect, rad: f32, f: impl FnOnce(&Canvas)) {
        self.clip_rr(r, rad);
        f(self);
        self.pop_clip();
    }

    /// `f` draws as one group at opacity `k` (fades of whole controls: a tooltip, a row entering).
    pub fn with_opacity(&self, k: f32, f: impl FnOnce(&Canvas)) {
        if k >= 0.999 {
            return f(self);
        }
        if k <= 0.0 {
            return;
        }
        let lp = D2D1_LAYER_PARAMETERS1 {
            contentBounds: rf(-1.0e7, -1.0e7, 1.0e7, 1.0e7),
            geometricMask: ManuallyDrop::new(None),
            maskAntialiasMode: D2D1_ANTIALIAS_MODE_PER_PRIMITIVE,
            maskTransform: mat(1.0, 0.0, 0.0),
            opacity: k,
            opacityBrush: ManuallyDrop::new(None),
            layerOptions: D2D1_LAYER_OPTIONS1_NONE,
        };
        unsafe { self.dc.PushLayer(&lp, None::<&ID2D1Layer>) };
        f(self);
        unsafe { self.dc.PopLayer() };
    }

    /// Run `f` with `m` applied on top of the current transform (in logical space), then restore.
    pub fn with_transform(&self, m: Matrix3x2, f: impl FnOnce(&Canvas)) {
        let saved = self.tf.get();
        let now = mul(&m, &saved);
        self.tf.set(now);
        unsafe { self.dc.SetTransform(&now) };
        f(self);
        self.tf.set(saved);
        unsafe { self.dc.SetTransform(&saved) };
    }

    /// `f` draws scaled by `s` around `c` (the press effect of buttons: `scale(.96)`).
    pub fn with_scale_about(&self, c: Pt, s: f32, f: impl FnOnce(&Canvas)) {
        if (s - 1.0).abs() < 1e-4 {
            return f(self);
        }
        self.with_transform(scale_about(c, s, s), f)
    }

    /// `f` draws moved by `(dx, dy)` logical px (rise on enter, hover lift).
    pub fn with_offset(&self, dx: f32, dy: f32, f: impl FnOnce(&Canvas)) {
        if dx == 0.0 && dy == 0.0 {
            return f(self);
        }
        self.with_transform(translation(dx, dy), f)
    }
}
