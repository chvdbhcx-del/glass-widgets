//! Text styles and the per-scene drawing resources behind them. `Style` is a value type (size, weight, alignment, tabular digits, ellipsis,
//! wrapping, tracking) with presets (`Style::BODY.center().ellipsis()`); `Res` owns what is expensive to create and cheap to reuse: the solid
//! brush, the round-capped stroke style, the DirectWrite text formats (one per style, snapped to the type scale), the text layouts (cached by
//! string + style + box, so an overlay repainting 60 times a second with unchanged text rebuilds nothing), the vector-glyph geometries and the
//! sparkline gradients. Fonts come from a `FontSet` (see `fonts.rs`); without bundled fonts the installed Segoe UI Variable is used.
use crate::fonts::FontSet;
use crate::gfx::*;
use crate::icons::{self, Glyph};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use windows::core::{w, Interface, Result, HSTRING};
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::DirectWrite::*;
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Align {
    Left,
    Center,
    Right,
}
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum VAlign {
    Top,
    Middle,
    Bottom,
}
/// A text style; start from a preset and chain builders: `Style::BODY.center().ellipsis()`.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Style {
    pub size: f32,
    pub weight: u16,
    pub align: Align,
    pub valign: VAlign,
    /// tabular (fixed width) digits: clocks, counters, anything that changes in place
    pub mono: bool,
    /// single line cut with "..." instead of overflowing
    pub ellipsis: bool,
    /// wrap to the rect width (valign Top)
    pub wrap: bool,
    /// letter spacing in px
    pub track: f32,
}
impl Style {
    pub const fn new(size: f32, weight: u16) -> Style {
        Style { size, weight, align: Align::Left, valign: VAlign::Middle, mono: false, ellipsis: false, wrap: false, track: 0.0 }
    }
    /// 13 semibold
    pub const TITLE: Style = Style::new(13.0, 600);
    /// 13 regular
    pub const BODY: Style = Style::new(13.0, 400);
    /// 11 regular
    pub const CAPTION: Style = Style::new(11.0, 400);
    /// 44 light (temperature, counters)
    pub const BIG: Style = Style::new(44.0, 300);
    /// 64 light
    pub const HUGE: Style = Style::new(64.0, 300);
    /// 13 regular with tabular digits
    pub const MONO: Style = Style { mono: true, ..Style::new(13.0, 400) };
    /// 12 bold, letter-spaced: month names, "ПЯТНИЦА", section headers (colour it ACCENT or RED, UPPERCASE the text yourself)
    pub const LABEL: Style = Style { track: 0.48, ..Style::new(12.0, 700) };
    pub const fn size(self, s: f32) -> Style {
        Style { size: s, ..self }
    }
    pub const fn weight(self, w: u16) -> Style {
        Style { weight: w, ..self }
    }
    pub const fn center(self) -> Style {
        Style { align: Align::Center, ..self }
    }
    pub const fn right(self) -> Style {
        Style { align: Align::Right, ..self }
    }
    pub const fn top(self) -> Style {
        Style { valign: VAlign::Top, ..self }
    }
    pub const fn bottom(self) -> Style {
        Style { valign: VAlign::Bottom, ..self }
    }
    pub const fn mono(self) -> Style {
        Style { mono: true, ..self }
    }
    pub const fn ellipsis(self) -> Style {
        Style { ellipsis: true, ..self }
    }
    pub const fn wrap(self) -> Style {
        Style { wrap: true, valign: VAlign::Top, ..self }
    }
    pub const fn track(self, px: f32) -> Style {
        Style { track: px, ..self }
    }
}

// ================================================================ shared drawing resources (one per scene)

type FmtKey = (u32, u16, u8, u8, bool, bool);
/// Per-scene drawing resources (see the module doc). Create one per Direct2D device; it is not `Send` (the caches are `RefCell`s).
pub struct Res {
    brush: ID2D1SolidColorBrush,
    round: ID2D1StrokeStyle,
    fams: [HSTRING; 3],
    /// the bundled font collection; `None` = system fonts (Segoe UI Variable)
    coll: Option<IDWriteFontCollection>,
    /// the font family gets one more weight step at small sizes (Nunito is lighter than Segoe)
    bolder_small: bool,
    fmts: RefCell<HashMap<FmtKey, IDWriteTextFormat>>,
    glyphs: RefCell<HashMap<Glyph, Rc<Vec<(ID2D1PathGeometry, icons::Paint)>>>>,
    /// text layouts by (string, style, box): an overlay that repaints 60 times a second with unchanged text reuses them
    lays: RefCell<HashMap<u64, LayEntry>>,
    /// sparkline area gradients by (top, bottom, inked colour)
    grads: RefCell<Vec<([u32; 6], ID2D1LinearGradientBrush)>>,
}
/// More than this many cached layouts / gradients: start over (bounded memory; a miss only costs one rebuild).
const LAYOUT_CACHE: usize = 1024;
const GRADIENT_CACHE: usize = 32;
struct LayEntry {
    s: String,
    st: Style,
    w: f32,
    h: f32,
    lay: IDWriteTextLayout,
}
/// Hash of everything a text layout depends on; `LayEntry` compares the real values, so a collision only costs a rebuild.
fn lay_key(s: &str, st: Style, w: f32, h: f32) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut k = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut k);
    (st.size.to_bits(), st.weight, st.align as u8, st.valign as u8, st.mono, st.ellipsis, st.wrap, st.track.to_bits(), w.to_bits(), h.to_bits()).hash(&mut k);
    k.finish()
}

/// The installed optical-size cut of Segoe UI Variable (Small / Text / Display), else plain Segoe UI.
fn family(gfx: &Gfx, cut: &str) -> HSTRING {
    unsafe {
        let mut coll: Option<IDWriteFontCollection> = None;
        if gfx.dw.GetSystemFontCollection(&mut coll, false).is_ok() {
            if let Some(c) = coll {
                for name in [format!("Segoe UI Variable {cut}"), "Segoe UI".to_string()] {
                    let (mut i, mut ok) = (0u32, windows::Win32::Foundation::BOOL(0));
                    if c.FindFamilyName(&HSTRING::from(name.as_str()), &mut i, &mut ok).is_ok() && ok.as_bool() {
                        return HSTRING::from(name.as_str());
                    }
                }
            }
        }
        HSTRING::from("Segoe UI")
    }
}

/// Optical size: captions use the Small cut, running text the Text cut, big numbers and headlines the Display cut.
fn cut_for(size: f32) -> usize {
    if size <= 11.5 {
        0
    } else if size <= 21.0 {
        1
    } else {
        2
    }
}

/// Nunito is lighter and rounder than Segoe: captions get one step more weight so they hold their contrast on glass.
fn weight_for(size: f32, weight: u16, nunito: bool) -> i32 {
    let w = weight as i32;
    if nunito && size <= 12.5 {
        (w + 100).min(700)
    } else {
        w
    }
}

/// The type scale of the cards (px). Widgets ask for the size they want (and fit loops step through fractions: 14.276, 11.5, 9.5...); every request
/// snaps to the nearest step so the whole desk shares one hierarchy: caption 11, small 12, body 13, emphasis 15, headline 17, then the number sizes.
/// `MSW_TEXT_SCALE=off` keeps the requested sizes.
const TYPE_SCALE: [f32; 15] = [10.0, 11.0, 12.0, 13.0, 15.0, 17.0, 20.0, 24.0, 28.0, 34.0, 40.0, 46.0, 54.0, 64.0, 76.0];

fn snap_size(size: f32) -> f32 {
    static OFF: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if *OFF.get_or_init(|| std::env::var("MSW_TEXT_SCALE").is_ok_and(|v| v == "off")) || size <= 0.0 {
        return size;
    }
    let mut best = TYPE_SCALE[0];
    for &t in &TYPE_SCALE {
        // ties go up: 11.5 -> 12, 12.5 -> 13
        if (t - size).abs() <= (best - size).abs() {
            best = t;
        }
    }
    if size > TYPE_SCALE[TYPE_SCALE.len() - 1] { size } else { best }
}

/// Size-dependent tracking in px (SF-like): small text opens up a little, large text tightens.
fn auto_track(size: f32) -> f32 {
    match size {
        s if s <= 11.5 => 0.1,
        s if s <= 15.0 => 0.0,
        s if s <= 21.0 => -0.006 * s,
        s if s <= 36.0 => -0.012 * s,
        s => -0.016 * s,
    }
}

impl Res {
    pub fn new(gfx: &Gfx, fonts: &FontSet) -> Result<Res> {
        unsafe {
            let sp = D2D1_STROKE_STYLE_PROPERTIES1 {
                startCap: D2D1_CAP_STYLE_ROUND,
                endCap: D2D1_CAP_STYLE_ROUND,
                dashCap: D2D1_CAP_STYLE_ROUND,
                lineJoin: D2D1_LINE_JOIN_ROUND,
                miterLimit: 4.0,
                dashStyle: D2D1_DASH_STYLE_SOLID,
                dashOffset: 0.0,
                transformType: D2D1_STROKE_TRANSFORM_TYPE_NORMAL,
            };
            let round: ID2D1StrokeStyle = gfx.d2d.CreateStrokeStyle(&sp, None)?.cast()?;
            let coll = fonts.collection(&gfx.dw);
            let fams = if coll.is_some() { std::array::from_fn(|_| HSTRING::from(fonts.family())) } else { [family(gfx, "Small"), family(gfx, "Text"), family(gfx, "Display")] };
            Ok(Res {
                brush: gfx.dc.CreateSolidColorBrush(&col(1.0, 1.0, 1.0, 1.0), None)?,
                round,
                fams,
                bolder_small: coll.is_some() && fonts.bolder_small(),
                coll,
                fmts: RefCell::new(HashMap::new()),
                glyphs: RefCell::new(HashMap::new()),
                lays: RefCell::new(HashMap::new()),
                grads: RefCell::new(Vec::new()),
            })
        }
    }

    /// The shared solid brush (set its colour before every use).
    pub fn brush(&self) -> &ID2D1SolidColorBrush {
        &self.brush
    }

    /// The round-capped, round-joined stroke style of lines, arcs and outlines.
    pub fn round(&self) -> &ID2D1StrokeStyle {
        &self.round
    }

    fn format(&self, gfx: &Gfx, st: Style) -> Result<IDWriteTextFormat> {
        let st = Style { size: snap_size(st.size), ..st };
        let key: FmtKey = ((st.size * 10.0) as u32, st.weight, st.align as u8, st.valign as u8, st.wrap, st.ellipsis);
        if let Some(f) = self.fmts.borrow().get(&key) {
            return Ok(f.clone());
        }
        unsafe {
            let f = gfx.dw.CreateTextFormat(&self.fams[cut_for(st.size)], self.coll.as_ref(), DWRITE_FONT_WEIGHT(weight_for(st.size, st.weight, self.bolder_small)), DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_STRETCH_NORMAL, st.size, w!("ru-ru"))?;
            f.SetTextAlignment(match st.align {
                Align::Left => DWRITE_TEXT_ALIGNMENT_LEADING,
                Align::Center => DWRITE_TEXT_ALIGNMENT_CENTER,
                Align::Right => DWRITE_TEXT_ALIGNMENT_TRAILING,
            })?;
            f.SetParagraphAlignment(match st.valign {
                VAlign::Top => DWRITE_PARAGRAPH_ALIGNMENT_NEAR,
                VAlign::Middle => DWRITE_PARAGRAPH_ALIGNMENT_CENTER,
                VAlign::Bottom => DWRITE_PARAGRAPH_ALIGNMENT_FAR,
            })?;
            f.SetWordWrapping(if st.wrap { DWRITE_WORD_WRAPPING_WRAP } else { DWRITE_WORD_WRAPPING_NO_WRAP })?;
            if st.wrap {
                // multi-line text breathes: 1.32 line height instead of the font's tight default
                let lh = (st.size * 1.32).round();
                f.SetLineSpacing(DWRITE_LINE_SPACING_METHOD_UNIFORM, lh, (lh * 0.8).round())?;
            }
            if st.ellipsis {
                let sign = gfx.dw.CreateEllipsisTrimmingSign(&f)?;
                f.SetTrimming(&DWRITE_TRIMMING { granularity: DWRITE_TRIMMING_GRANULARITY_CHARACTER, delimiter: 0, delimiterCount: 0 }, &sign)?;
            }
            self.fmts.borrow_mut().insert(key, f.clone());
            Ok(f)
        }
    }

    /// The (cached) layout of `s` in box `w` x `h`; callers must not modify it.
    pub fn layout(&self, gfx: &Gfx, s: &str, st: Style, w: f32, h: f32) -> Result<IDWriteTextLayout> {
        let (w, h) = (w.max(0.0), h.max(0.0));
        let key = lay_key(s, st, w, h);
        if let Some(e) = self.lays.borrow().get(&key) {
            if e.s == s && e.st == st && e.w == w && e.h == h {
                return Ok(e.lay.clone());
            }
        }
        let lay = self.build_layout(gfx, s, st, w, h)?;
        let mut m = self.lays.borrow_mut();
        if m.len() >= LAYOUT_CACHE {
            m.clear();
        }
        m.insert(key, LayEntry { s: s.to_string(), st, w, h, lay: lay.clone() });
        Ok(lay)
    }
    fn build_layout(&self, gfx: &Gfx, s: &str, st: Style, w: f32, h: f32) -> Result<IDWriteTextLayout> {
        let f = self.format(gfx, st)?;
        let wide: Vec<u16> = s.encode_utf16().collect();
        unsafe {
            let lay = gfx.dw.CreateTextLayout(&wide, &f, w, h)?;
            let range = DWRITE_TEXT_RANGE { startPosition: 0, length: wide.len() as u32 };
            let track = if st.track != 0.0 { st.track } else { auto_track(st.size) };
            if track != 0.0 {
                if let Ok(l1) = lay.cast::<IDWriteTextLayout1>() {
                    let _ = l1.SetCharacterSpacing(0.0, track, 0.0, range);
                }
            }
            // tabular figures for numbers that change in place, but not at display sizes: a tabular "9,0" at 40 px has wide holes around the comma, and nothing jitters at that size anyway
            if let Ok(t) = gfx.dw.CreateTypography() {
                if st.mono && st.size < 36.0 {
                    let _ = t.AddFontFeature(DWRITE_FONT_FEATURE { nameTag: DWRITE_FONT_FEATURE_TAG_TABULAR_FIGURES, parameter: 1 });
                }
                let _ = lay.SetTypography(&t, range);
            }
            Ok(lay)
        }
    }

    /// The cached geometries of a built-in or registered glyph.
    pub fn glyph(&self, gfx: &Gfx, g: Glyph) -> Result<Rc<Vec<(ID2D1PathGeometry, icons::Paint)>>> {
        if let Some(v) = self.glyphs.borrow().get(&g) {
            return Ok(v.clone());
        }
        let v = Rc::new(icons::geometries(&gfx.d2d, g)?);
        self.glyphs.borrow_mut().insert(g, v.clone());
        Ok(v)
    }

    /// A cached gradient by its key (callers build the key from everything the gradient depends on).
    pub fn cached_gradient(&self, key: &[u32; 6]) -> Option<ID2D1LinearGradientBrush> {
        self.grads.borrow().iter().find(|(k, _)| k == key).map(|(_, b)| b.clone())
    }
    /// Remember a gradient under `key`; more than `GRADIENT_CACHE` entries start the cache over.
    pub fn cache_gradient(&self, key: [u32; 6], b: ID2D1LinearGradientBrush) {
        let mut g = self.grads.borrow_mut();
        if g.len() >= GRADIENT_CACHE {
            g.clear();
        }
        g.push((key, b));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_key_follows_every_input() {
        let st = Style::CAPTION;
        let k = lay_key("12%", st, 40.0, 14.0);
        assert_eq!(k, lay_key("12%", st, 40.0, 14.0));
        for other in [lay_key("13%", st, 40.0, 14.0), lay_key("12%", st.size(12.0), 40.0, 14.0), lay_key("12%", st.center(), 40.0, 14.0), lay_key("12%", st.mono(), 40.0, 14.0), lay_key("12%", st, 41.0, 14.0), lay_key("12%", st, 40.0, 15.0)] {
            assert_ne!(k, other);
        }
    }

    #[test]
    fn sizes_snap_to_the_type_scale_and_tracking_follows_size() {
        // ties go up, anything past the last step stays as asked
        assert_eq!(snap_size(11.5), 12.0);
        assert_eq!(snap_size(12.5), 13.0);
        assert_eq!(snap_size(14.276), 15.0);
        assert_eq!(snap_size(90.0), 90.0);
        assert_eq!(snap_size(0.0), 0.0);
        assert!(auto_track(11.0) > 0.0 && auto_track(13.0) == 0.0 && auto_track(44.0) < 0.0);
        // optical-size cut and the Nunito caption weight bump
        assert_eq!((cut_for(11.0), cut_for(13.0), cut_for(40.0)), (0, 1, 2));
        assert_eq!((weight_for(12.0, 400, true), weight_for(12.0, 400, false), weight_for(15.0, 400, true), weight_for(12.0, 700, true)), (500, 400, 400, 700));
    }
}