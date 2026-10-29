//! Headless rendering for tests and the gallery: a software (WARP) [`Gfx`], a text cache, and [`Harness::render`] that runs a drawing closure
//! on a canvas of a given logical size and DPI scale and gives back the pixels as an [`Image`] (premultiplied BGRA, like Direct2D).
//!
//! ```ignore
//! let h = Harness::new();
//! let img = h.render(200, 60, 2.0, Color::rgb8(0, 0, 0), |cv| button.draw(cv, &theme));
//! assert!(img.px(20, 20)[3] == 255);
//! img.save_png(&h, "out/button.png");
//! ```
//! Nothing here is needed at run time of an app; it is public so downstream crates (Pill) can test their own views the same way.
use crate::canvas::Canvas;
use crate::color::Color;
use crate::fonts::FontSet;
use crate::geom::Rect;
use crate::gfx::*;
use crate::text::Res;
use windows::core::Result;
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

static SYSTEM_FONTS: FontSet = FontSet::system();

/// A WARP device plus the shared text resources. One per test (a `Harness` is not `Send`).
pub struct Harness {
    pub gfx: Gfx,
    pub res: Res,
}

impl Default for Harness {
    fn default() -> Self {
        Harness::new()
    }
}

impl Harness {
    /// Software device and system fonts. Panics when no device can be created (a test machine without WARP).
    pub fn new() -> Harness {
        Harness::with_fonts(&SYSTEM_FONTS)
    }

    pub fn with_fonts(fonts: &FontSet) -> Harness {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED); // WIC wants COM on this thread
        }
        let gfx = Gfx::warp().expect("a WARP Direct3D device");
        let res = Res::new(&gfx, fonts).expect("text resources");
        Harness { gfx, res }
    }

    /// Draw `f` on a transparent-cleared `w` x `h` logical-px canvas at `scale` device px per logical px, over a `bg` fill.
    pub fn render(&self, w: u32, h: u32, scale: f32, bg: Color, f: impl FnOnce(&Canvas)) -> Image {
        self.try_render(w, h, scale, bg, f).expect("render")
    }

    pub fn try_render(&self, w: u32, h: u32, scale: f32, bg: Color, f: impl FnOnce(&Canvas)) -> Result<Image> {
        let (pw, ph) = (((w as f32 * scale).ceil() as u32).max(1), ((h as f32 * scale).ceil() as u32).max(1));
        let tgt = self.gfx.target(pw, ph)?;
        unsafe {
            self.gfx.dc.SetTarget(&tgt);
            self.gfx.dc.BeginDraw();
            self.gfx.dc.Clear(Some(&bg.d2d()));
            self.gfx.dc.SetTransform(&mat(scale, 0.0, 0.0));
            {
                let cv = Canvas::new(&self.gfx, &self.res, scale);
                f(&cv);
            }
            self.gfx.dc.SetTransform(&mat(1.0, 0.0, 0.0));
            let r = self.gfx.dc.EndDraw(None, None);
            self.gfx.dc.SetTarget(None::<&ID2D1Image>);
            r?;
        }
        let cpu = self.gfx.cpu_read(pw, ph)?;
        let mut px = Vec::with_capacity((pw * ph * 4) as usize);
        self.gfx.read_rect(&tgt, &cpu, RECT { left: 0, top: 0, right: pw as i32, bottom: ph as i32 }, |_, row| px.extend_from_slice(row))?;
        Ok(Image { w: pw, h: ph, scale, bgra: px })
    }

    /// Several frames of the same drawing at times `ts` (seconds), stacked side by side with `gap` logical px between them: the
    /// contact sheet of an animation. `f(canvas, t)` draws the state at time `t`.
    pub fn render_sequence(&self, w: u32, h: u32, scale: f32, bg: Color, ts: &[f32], gap: u32, mut f: impl FnMut(&Canvas, f32)) -> Image {
        let total = w * ts.len() as u32 + gap * ts.len().saturating_sub(1) as u32;
        self.render(total, h, scale, bg, |cv| {
            for (i, &t) in ts.iter().enumerate() {
                let x = (i as u32 * (w + gap)) as f32;
                cv.with_clip(Rect::new(x, 0.0, w as f32, h as f32), |cv| cv.with_offset(x, 0.0, |cv| f(cv, t)));
            }
        })
    }
}

/// Pixels of a render: premultiplied BGRA, row-major, `w` x `h` DEVICE pixels.
#[derive(Clone, Debug)]
pub struct Image {
    pub w: u32,
    pub h: u32,
    pub scale: f32,
    pub bgra: Vec<u8>,
}

impl Image {
    /// RGBA of the device pixel (x, y), premultiplied.
    pub fn px(&self, x: u32, y: u32) -> [u8; 4] {
        let i = ((y.min(self.h - 1) * self.w + x.min(self.w - 1)) * 4) as usize;
        [self.bgra[i + 2], self.bgra[i + 1], self.bgra[i], self.bgra[i + 3]]
    }

    /// RGBA at a LOGICAL position (the centre of the device pixel it falls in).
    pub fn at(&self, x: f32, y: f32) -> [u8; 4] {
        self.px((x * self.scale) as u32, (y * self.scale) as u32)
    }

    /// How many pixels differ from `other` by more than `tol` in any channel (images of different size: all of them).
    pub fn diff(&self, other: &Image, tol: u8) -> usize {
        if (self.w, self.h) != (other.w, other.h) {
            return (self.w * self.h) as usize;
        }
        self.bgra.chunks_exact(4).zip(other.bgra.chunks_exact(4)).filter(|(a, b)| a.iter().zip(b.iter()).any(|(x, y)| x.abs_diff(*y) > tol)).count()
    }

    /// Number of pixels in the logical rectangle `r` (all of the image when `None`) for which `f(rgba)` holds.
    pub fn count(&self, r: Option<Rect>, f: impl Fn([u8; 4]) -> bool) -> usize {
        let r = r.unwrap_or(Rect::new(0.0, 0.0, self.w as f32 / self.scale, self.h as f32 / self.scale));
        let (x0, y0) = ((r.x * self.scale).max(0.0) as u32, (r.y * self.scale).max(0.0) as u32);
        let (x1, y1) = (((r.right() * self.scale).ceil() as u32).min(self.w), ((r.bottom() * self.scale).ceil() as u32).min(self.h));
        let mut n = 0;
        for y in y0..y1 {
            for x in x0..x1 {
                if f(self.px(x, y)) {
                    n += 1;
                }
            }
        }
        n
    }

    /// Bounding box (device px, inclusive-exclusive) of the pixels for which `f` holds.
    pub fn bounds(&self, f: impl Fn([u8; 4]) -> bool) -> Option<(u32, u32, u32, u32)> {
        let (mut x0, mut y0, mut x1, mut y1) = (u32::MAX, u32::MAX, 0, 0);
        for y in 0..self.h {
            for x in 0..self.w {
                if f(self.px(x, y)) {
                    x0 = x0.min(x);
                    y0 = y0.min(y);
                    x1 = x1.max(x + 1);
                    y1 = y1.max(y + 1);
                }
            }
        }
        (x1 > 0).then_some((x0, y0, x1, y1))
    }

    /// Straight (un-premultiplied) BGRA, what PNG encoders and `Gfx::save_png` take.
    pub fn straight_bgra(&self) -> Vec<u8> {
        let mut o = self.bgra.clone();
        for p in o.chunks_exact_mut(4) {
            let a = p[3] as u32;
            if a != 0 && a != 255 {
                for c in &mut p[..3] {
                    *c = ((*c as u32 * 255 + a / 2) / a).min(255) as u8;
                }
            }
        }
        o
    }

    /// Write a PNG (creates the parent directory).
    pub fn save_png(&self, h: &Harness, path: impl AsRef<std::path::Path>) -> Result<()> {
        let p = path.as_ref();
        if let Some(d) = p.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        h.gfx.save_png(&p.to_string_lossy(), self.w, self.h, &self.straight_bgra())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::Style;

    #[test]
    fn renders_shapes_at_both_scales() {
        let h = Harness::new();
        for scale in [1.0, 2.0] {
            let img = h.render(40, 20, scale, Color::rgb8(0, 0, 0), |cv| {
                cv.fill_rr(Rect::new(10.0, 5.0, 20.0, 10.0), 3.0, Color::rgb8(255, 0, 0));
            });
            assert_eq!((img.w, img.h), ((40.0 * scale) as u32, (20.0 * scale) as u32));
            assert_eq!(img.at(20.0, 10.0), [255, 0, 0, 255]);
            assert_eq!(img.at(2.0, 2.0), [0, 0, 0, 255]);
            let (x0, y0, x1, y1) = img.bounds(|p| p[0] > 200).unwrap();
            assert_eq!((x0, y0, x1, y1), ((10.0 * scale) as u32, (5.0 * scale) as u32, (30.0 * scale) as u32, (15.0 * scale) as u32));
        }
    }

    #[test]
    fn strokes_stay_inside_and_text_draws() {
        let h = Harness::new();
        let img = h.render(60, 30, 1.0, Color::rgb8(0, 0, 0), |cv| {
            cv.stroke_rr(Rect::new(10.0, 5.0, 40.0, 20.0), 0.0, 2.0, Color::rgb8(0, 255, 0));
            cv.text(Rect::new(0.0, 0.0, 60.0, 30.0), "Hi", Style::BODY.center(), Color::rgb8(255, 255, 255));
        });
        assert_eq!(img.px(10, 15), [0, 255, 0, 255], "the border covers the first pixels of the rectangle");
        assert_eq!(img.px(9, 15)[1], 0, "and nothing outside it");
        assert!(img.count(None, |p| p[0] > 100 && p[1] > 100 && p[2] > 100) > 10, "glyph pixels");
    }

    #[test]
    fn opacity_clip_and_scale_about() {
        let h = Harness::new();
        let img = h.render(40, 40, 1.0, Color::rgb8(0, 0, 0), |cv| {
            cv.with_opacity(0.5, |cv| cv.fill_rect(Rect::new(0.0, 0.0, 10.0, 10.0), Color::rgb8(255, 255, 255)));
            cv.with_clip(Rect::new(20.0, 0.0, 5.0, 40.0), |cv| cv.fill_rect(Rect::new(0.0, 20.0, 40.0, 10.0), Color::rgb8(0, 0, 255)));
            cv.with_scale_about((30.0, 5.0), 0.5, |cv| cv.fill_rect(Rect::new(20.0, 0.0, 20.0, 10.0), Color::rgb8(255, 0, 0)));
        });
        let p = img.px(5, 5);
        assert!((p[0] as i32 - 128).abs() <= 2, "{p:?}");
        assert_eq!(img.px(22, 25), [0, 0, 255, 255]);
        assert_eq!(img.px(30, 25), [0, 0, 0, 255], "clipped away");
        assert_eq!(img.px(30, 5), [255, 0, 0, 255]);
        assert_eq!(img.px(21, 5), [0, 0, 0, 255], "the scaled rect shrank around its anchor");
    }

    #[test]
    fn diff_and_sequence() {
        let h = Harness::new();
        let a = h.render(10, 10, 1.0, Color::rgb8(0, 0, 0), |_| {});
        let b = h.render(10, 10, 1.0, Color::rgb8(0, 0, 0), |cv| cv.fill_rect(Rect::new(0.0, 0.0, 2.0, 2.0), Color::rgb8(255, 255, 255)));
        assert_eq!(a.diff(&a, 0), 0);
        assert_eq!(a.diff(&b, 0), 4);
        let seq = h.render_sequence(10, 10, 1.0, Color::rgb8(0, 0, 0), &[0.0, 1.0], 2, |cv, t| cv.fill_rect(Rect::new(0.0, 0.0, 10.0 * t, 10.0), Color::rgb8(255, 255, 255)));
        assert_eq!(seq.w, 22);
        assert_eq!(seq.px(5, 5)[0], 0, "t = 0 draws nothing");
        assert_eq!(seq.px(17, 5)[0], 255, "t = 1 fills the second tile");
        assert_eq!(seq.px(11, 5)[0], 0, "the gap stays clear");
        let lw = h.render(4, 4, 1.0, Color::rgba(0.0, 0.0, 0.0, 0.0), |cv| cv.fill_rect(Rect::new(0.0, 0.0, 4.0, 4.0), Color::rgba(1.0, 0.0, 0.0, 0.5)));
        let s = lw.straight_bgra();
        assert!(s[2] >= 250, "un-premultiplied red {s:?}");
    }
}
