//! The liquid pass: two glass cards that come closer than `motion::MERGE_GAP` pull a gooey neck between them.
//!
//! Per merging pair (one `Slot`): the two rounded rects are drawn white into a small mask bitmap (outside the frame session,
//! `Liquid::prepare_neck`), then a Gaussian blur followed by an alpha slope/threshold (D2D LinearTransfer) turns the pair into
//! a metaball union: the neck exists exactly while the blurred alphas of the two facing edges add up above the threshold, thins as the gap
//! grows and tears off at `MERGE_GAP`. In the frame session (`Liquid::draw_neck`) the glass is poured into that mask: the lens of the
//! card (blurred, saturated desktop) + the card tint / veils, clipped to (zone between the cards - the two card interiors) so the normal
//! card layers stay untouched, plus a thin rim along the neck contour (mask minus its erosion). Nothing here runs when no pair is close.
use crate::gfx::*;
use std::mem::ManuallyDrop;
use windows::core::{Interface, Result};
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;

pub type R4 = (f32, f32, f32, f32);

pub fn union_rect(a: R4, b: R4) -> R4 {
    let (x0, y0) = (a.0.min(b.0), a.1.min(b.1));
    let (x1, y1) = ((a.0 + a.2).max(b.0 + b.2), (a.1 + a.3).max(b.1 + b.3));
    (x0, y0, x1 - x0, y1 - y0)
}

/// The rectangle between two cards in which the neck may be painted: across the gap axis from `e` px inside one card to `e` px inside the
/// other, along the overlap of their projections plus `flare` px each side (the neck widens towards the cards).
pub fn zone(a: R4, b: R4, e: f32, flare: f32) -> R4 {
    let gx = (b.0 - (a.0 + a.2)).max(a.0 - (b.0 + b.2));
    let gy = (b.1 - (a.1 + a.3)).max(a.1 - (b.1 + b.3));
    let span = |lo1: f32, hi1: f32, lo2: f32, hi2: f32| {
        let (mut lo, mut hi) = (lo1.max(lo2), hi1.min(hi2));
        if lo > hi {
            std::mem::swap(&mut lo, &mut hi);
        }
        (lo - flare, hi + flare)
    };
    if gx >= gy {
        let (l, r) = if a.0 < b.0 { (a, b) } else { (b, a) };
        let (x0, x1) = (l.0 + l.2 - e, r.0 + e);
        let (y0, y1) = span(a.1, a.1 + a.3, b.1, b.1 + b.3);
        (x0, y0, x1 - x0, y1 - y0)
    } else {
        let (t, bt) = if a.1 < b.1 { (a, b) } else { (b, a) };
        let (y0, y1) = (t.1 + t.3 - e, bt.1 + e);
        let (x0, x1) = span(a.0, a.0 + a.2, b.0, b.0 + b.2);
        (x0, y0, x1 - x0, y1 - y0)
    }
}

/// Everything the neck of one pair is made of (canvas px).
pub struct NeckIn<'a> {
    pub a: R4,
    pub b: R4,
    /// corner radii of the two cards, px
    pub ra: f32,
    pub rb: f32,
    /// the lens of one of the cards (desktop blurred + saturated), in canvas coordinates
    pub lens: &'a ID2D1Image,
    /// white tint alpha, dark veil alpha, milky veil alpha, appearance 0..1
    pub tint: f32,
    pub dark: f32,
    pub milk: f32,
    pub k: f32,
    /// rim alpha and stroke width (logical px)
    pub rim: f32,
    pub rim_w: f32,
    /// how merged the pair is 0..1 (fades the very first sliver in)
    pub amount: f32,
}

struct Slot {
    rect: (i32, i32, i32, i32),
    bmp: Option<(i32, i32, ID2D1Bitmap1)>,
    blur: ID2D1Effect,
    thr: ID2D1Effect,
    ero: ID2D1Effect,
    band: ID2D1Effect,
    mask: ID2D1ImageBrush,
    rim_mask: ID2D1ImageBrush,
}

pub struct Liquid {
    s: f32,
    slots: Vec<Slot>,
    white: ID2D1SolidColorBrush,
    fill: ID2D1SolidColorBrush,
}

/// blur sigma (logical px) of the metaball field: the neck exists while two facing edges are closer than 1.35 sigma
const SIGMA: f32 = crate::motion::MERGE_GAP / 1.35;
const SLOPE: f32 = 14.0;

impl Liquid {
    pub fn new(gfx: &Gfx, s: f32) -> Result<Liquid> {
        unsafe {
            Ok(Liquid {
                s,
                slots: Vec::new(),
                white: gfx.dc.CreateSolidColorBrush(&col(1.0, 1.0, 1.0, 1.0), None)?,
                fill: gfx.dc.CreateSolidColorBrush(&col(1.0, 1.0, 1.0, 1.0), None)?,
            })
        }
    }

    fn new_slot(&self, gfx: &Gfx) -> Result<Slot> {
        unsafe {
            let dc = &gfx.dc;
            let blur = dc.CreateEffect(&CLSID_D2D1GaussianBlur)?;
            fx_f32(&blur, 0, SIGMA * self.s)?;
            fx_u32(&blur, 1, 1)?;
            let thr = dc.CreateEffect(&CLSID_D2D1LinearTransfer)?;
            thr.SetInput(0, &blur.GetOutput()?, true);
            fx_f32(&thr, D2D1_LINEARTRANSFER_PROP_ALPHA_SLOPE.0 as u32, SLOPE)?;
            fx_f32(&thr, D2D1_LINEARTRANSFER_PROP_ALPHA_Y_INTERCEPT.0 as u32, 0.5 - SLOPE * 0.5)?;
            fx_bool(&thr, D2D1_LINEARTRANSFER_PROP_CLAMP_OUTPUT.0 as u32, true)?;
            let thr_out = thr.GetOutput()?;
            let ero = dc.CreateEffect(&CLSID_D2D1Morphology)?;
            ero.SetInput(0, &thr_out, true);
            fx_u32(&ero, D2D1_MORPHOLOGY_PROP_MODE.0 as u32, D2D1_MORPHOLOGY_MODE_ERODE.0 as u32)?;
            let kw = (1.25 * 2.0 * self.s).round().max(2.0) as u32;
            // plain UINT32 properties (not enums)
            for prop in [D2D1_MORPHOLOGY_PROP_WIDTH, D2D1_MORPHOLOGY_PROP_HEIGHT] {
                ero.SetValue(prop.0 as u32, D2D1_PROPERTY_TYPE_UINT32, &kw.to_ne_bytes())?;
            }
            // band = mask OUT eroded mask (input 0 = destination, input 1 = source)
            let band = dc.CreateEffect(&CLSID_D2D1Composite)?;
            band.SetInput(0, &ero.GetOutput()?, true);
            band.SetInput(1, &thr_out, true);
            fx_u32(&band, 0, D2D1_COMPOSITE_MODE_SOURCE_OUT.0 as u32)?;
            let props = |w: f32, h: f32| D2D1_IMAGE_BRUSH_PROPERTIES { sourceRectangle: rf(0.0, 0.0, w, h), extendModeX: D2D1_EXTEND_MODE_CLAMP, extendModeY: D2D1_EXTEND_MODE_CLAMP, interpolationMode: D2D1_INTERPOLATION_MODE_LINEAR };
            let mask = dc.CreateImageBrush(&thr_out, &props(8.0, 8.0), None)?;
            let rim_mask = dc.CreateImageBrush(&band.GetOutput()?, &props(8.0, 8.0), None)?;
            Ok(Slot { rect: (0, 0, 0, 0), bmp: None, blur, thr, ero, band, mask, rim_mask })
        }
    }

    /// Render the mask source of pair `idx` (two white rounded rects) into its bitmap. Must run OUTSIDE the frame session.
    pub fn prepare_neck(&mut self, gfx: &Gfx, idx: usize, n: &NeckIn, canvas: (i32, i32)) -> Result<()> {
        while self.slots.len() <= idx {
            let sl = self.new_slot(gfx)?;
            self.slots.push(sl);
        }
        let s = self.s;
        let pad = (SIGMA * 3.0 * s).ceil();
        let u = union_rect(n.a, n.b);
        let (x0, y0) = (((u.0 - pad).floor() as i32).max(0), ((u.1 - pad).floor() as i32).max(0));
        let (x1, y1) = (((u.0 + u.2 + pad).ceil() as i32).min(canvas.0), ((u.1 + u.3 + pad).ceil() as i32).min(canvas.1));
        let (rw, rh) = ((x1 - x0).max(1), (y1 - y0).max(1));
        let white = &self.white;
        let sl = &mut self.slots[idx];
        sl.rect = (x0, y0, rw, rh);
        if sl.bmp.as_ref().is_none_or(|b| b.0 < rw || b.1 < rh) {
            let (bw, bh) = ((rw + 63) / 64 * 64, (rh + 63) / 64 * 64);
            let bmp = gfx.target8(bw as u32, bh as u32)?;
            unsafe {
                sl.blur.SetInput(0, &bmp, true);
                sl.mask.SetSourceRectangle(&rf(0.0, 0.0, bw as f32, bh as f32));
                sl.rim_mask.SetSourceRectangle(&rf(0.0, 0.0, bw as f32, bh as f32));
            }
            sl.bmp = Some((bw, bh, bmp));
        }
        let Some((_, _, bmp)) = sl.bmp.as_ref() else { return Ok(()) };
        let dc = &gfx.dc;
        unsafe {
            dc.SetTarget(bmp);
            dc.BeginDraw();
            dc.SetTransform(&mat(1.0, 0.0, 0.0));
            dc.Clear(None);
            dc.FillRoundedRectangle(&rr(n.a.0 - x0 as f32, n.a.1 - y0 as f32, n.a.2, n.a.3, n.ra), white);
            dc.FillRoundedRectangle(&rr(n.b.0 - x0 as f32, n.b.1 - y0 as f32, n.b.2, n.b.3, n.rb), white);
            let e = dc.EndDraw(None, None);
            dc.SetTarget(None::<&ID2D1Image>);
            e
        }
    }

    /// Pour the glass into the neck of pair `idx` (inside the frame session, identity transform, frame clip already pushed).
    pub fn draw_neck(&mut self, gfx: &Gfx, idx: usize, n: &NeckIn, clip: RECT) -> Result<()> {
        let Some(sl) = self.slots.get(idx) else { return Ok(()) };
        if sl.bmp.is_none() {
            return Ok(());
        }
        let s = self.s;
        let (rx, ry, rw, rh) = sl.rect;
        let (e, flare) = (3.0 * s, 14.0 * s);
        let z = zone(n.a, n.b, e, flare);
        if (z.0 + z.2) < clip.left as f32 || z.0 > clip.right as f32 || (z.1 + z.3) < clip.top as f32 || z.1 > clip.bottom as f32 {
            return Ok(());
        }
        let d2d = &gfx.d2d;
        let dc = &gfx.dc;
        unsafe {
            // paintable area = zone - card interiors (a little deflated, so the neck also covers the card rims at its mouth)
            let g0 = d2d.CreateRectangleGeometry(&rf(z.0, z.1, z.0 + z.2, z.1 + z.3))?;
            let ga = d2d.CreateRoundedRectangleGeometry(&rr(n.a.0 + e, n.a.1 + e, n.a.2 - 2.0 * e, n.a.3 - 2.0 * e, (n.ra - e).max(0.0)))?;
            let gb = d2d.CreateRoundedRectangleGeometry(&rr(n.b.0 + e, n.b.1 + e, n.b.2 - 2.0 * e, n.b.3 - 2.0 * e, (n.rb - e).max(0.0)))?;
            let p1 = d2d.CreatePathGeometry()?;
            {
                let sink = p1.Open()?;
                g0.CombineWithGeometry(&ga, D2D1_COMBINE_MODE_EXCLUDE, None, 0.1, &sink)?;
                sink.Close()?;
            }
            let p2 = d2d.CreatePathGeometry()?;
            {
                let sink = p2.Open()?;
                p1.CombineWithGeometry(&gb, D2D1_COMBINE_MODE_EXCLUDE, None, 0.1, &sink)?;
                sink.Close()?;
            }
            let geom: ID2D1Geometry = p2.cast()?;
            let bounds = rf(rx as f32, ry as f32, (rx + rw) as f32, (ry + rh) as f32);
            let fade = (n.amount * 5.0).clamp(0.0, 1.0);

            // ---- the glass in the neck
            sl.mask.SetTransform(&mat(1.0, rx as f32, ry as f32));
            let mut lp = D2D1_LAYER_PARAMETERS1 {
                contentBounds: bounds,
                geometricMask: ManuallyDrop::new(Some(geom.clone())),
                maskAntialiasMode: D2D1_ANTIALIAS_MODE_PER_PRIMITIVE,
                maskTransform: mat(1.0, 0.0, 0.0),
                opacity: fade,
                opacityBrush: ManuallyDrop::new(Some(sl.mask.cast::<ID2D1Brush>()?)),
                layerOptions: D2D1_LAYER_OPTIONS1_NONE,
            };
            dc.PushLayer(&lp, None::<&ID2D1Layer>);
            dc.DrawImage(n.lens, None, None, D2D1_INTERPOLATION_MODE_LINEAR, D2D1_COMPOSITE_MODE_SOURCE_OVER);
            let full = rf(z.0 - 2.0, z.1 - 2.0, z.0 + z.2 + 2.0, z.1 + z.3 + 2.0);
            let paint = |c: [f32; 4]| {
                if c[3] > 0.002 {
                    self.fill.SetColor(&col(c[0], c[1], c[2], c[3]));
                    dc.FillRectangle(&full, &self.fill);
                }
            };
            paint([1.0, 1.0, 1.0, n.tint]);
            paint([12.0 / 255.0, 14.0 / 255.0, 20.0 / 255.0, n.dark]);
            paint([0.97, 0.98, 1.0, n.milk]);
            dc.PopLayer();
            ManuallyDrop::drop(&mut lp.opacityBrush);
            ManuallyDrop::drop(&mut lp.geometricMask);

            // ---- the rim along the neck contour: white on dark glass, a cool grey line on light glass
            sl.rim_mask.SetTransform(&mat(1.0, rx as f32, ry as f32));
            let mut lp = D2D1_LAYER_PARAMETERS1 {
                contentBounds: bounds,
                geometricMask: ManuallyDrop::new(Some(geom)),
                maskAntialiasMode: D2D1_ANTIALIAS_MODE_PER_PRIMITIVE,
                maskTransform: mat(1.0, 0.0, 0.0),
                opacity: fade,
                opacityBrush: ManuallyDrop::new(Some(sl.rim_mask.cast::<ID2D1Brush>()?)),
                layerOptions: D2D1_LAYER_OPTIONS1_NONE,
            };
            dc.PushLayer(&lp, None::<&ID2D1Layer>);
            let k = n.k;
            self.fill.SetColor(&col(1.0 + (0.36 - 1.0) * k, 1.0 + (0.40 - 1.0) * k, 1.0 + (0.48 - 1.0) * k, (n.rim * 1.8).min(1.0)));
            dc.FillRectangle(&full, &self.fill);
            dc.PopLayer();
            ManuallyDrop::drop(&mut lp.opacityBrush);
            ManuallyDrop::drop(&mut lp.geometricMask);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zone_spans_the_gap_between_two_cards() {
        let a: R4 = (0.0, 0.0, 170.0, 170.0);
        let b: R4 = (180.0, 20.0, 170.0, 170.0);
        let z = zone(a, b, 3.0, 14.0);
        assert!((z.0 - 167.0).abs() < 1e-3 && (z.2 - (183.0 - 167.0)).abs() < 1e-3, "x span {z:?}");
        // y: overlap 20..170 plus the flare
        assert!((z.1 - 6.0).abs() < 1e-3 && (z.3 - (150.0 + 28.0)).abs() < 1e-3, "y span {z:?}");
        // vertical neighbours
        let c: R4 = (10.0, 176.0, 170.0, 170.0);
        let z = zone(a, c, 3.0, 14.0);
        assert!((z.1 - 167.0).abs() < 1e-3 && (z.3 - 12.0).abs() < 1e-3, "{z:?}");
        assert!((z.0 - (10.0 - 14.0)).abs() < 1e-3, "{z:?}");
    }

    #[test]
    fn union_covers_both() {
        let u = union_rect((0.0, 0.0, 10.0, 10.0), (20.0, 5.0, 10.0, 10.0));
        assert_eq!(u, (0.0, 0.0, 30.0, 15.0));
    }
}
