//! Rich text on DirectWrite: spans with bold / italic / code / strike / underline / link / colour / size / background, wrapped to a width,
//! with hit-testing, caret and selection rectangles, link lookup and cheap re-layout while text streams in. The markdown blocks of
//! [`crate::markdown`] are laid out on top of it by [`view::MarkdownView`].
//!
//! ```ignore
//! let rt = RichText::new().text("Hello ").bold("world").text(", see ").link("docs", "https://example.com");
//! let lay = RichLayout::new(&cv, &rt, 240.0)?;          // wraps at 240 logical px
//! lay.draw(&cv, (8.0, 8.0), theme.text);                 // base colour for spans without their own
//! if let Some(url) = lay.link_at((mx - 8.0, my - 8.0)) { /* open it (markdown::is_safe_url first) */ }
//! // streaming: only the last line is laid out again
//! lay.append_str(&cv, " more tokens")?;
//! ```
//!
//! # Index conventions
//! Every text index in this module is a **UTF-16 code unit** offset into [`RichLayout::text`] (what DirectWrite uses; an emoji outside the
//! BMP is two units). `'\n'` counts as one unit. [`RichLayout::utf16_to_char`] and [`RichLayout::char_to_utf16`] convert to and from `char`
//! indices (what [`crate::textfield::Field`] uses). Positions are logical px relative to the layout's top-left; callers add their origin.
//!
//! # How it is built
//! The text is cut at every `'\n'` into lines; each line is its own `IDWriteTextLayout` (wrapping inside the line is DirectWrite's), stacked
//! vertically with a uniform line height. That is what makes streaming cheap: appending tokens changes only the last line, so
//! [`RichLayout::append`] rebuilds just that one layout ([`RichLayout::rebuilds`] counts the layouts built, for tests).
//! Per-span colours are drawing effects (brushes) on the layout; backgrounds (inline code) are rounded rectangles drawn behind the text.
//! Fonts come from the same [`crate::text::Res`] as every other text of the kit (the app's bundled family), monospace is Cascadia Mono, else
//! Consolas, else Courier New. Text sizes are exact (no snapping to the widgets' type scale).
use crate::canvas::Canvas;
use crate::color::Color;
use crate::geom::{Pt, Rect};
use crate::gfx::pt;
use crate::text::{Style, VAlign};
use std::collections::HashMap;
use std::ops::Range;
use std::sync::OnceLock;
use windows::core::{Interface, Result, HSTRING};
use windows::Win32::Foundation::BOOL;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::DirectWrite::*;

pub mod view;
pub use view::{Hit, MarkdownLabels, MarkdownView, Selection, TextPos};

// ---------------------------------------------------------------- model

/// Formatting of one span.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SpanStyle {
    pub bold: bool,
    pub italic: bool,
    /// monospace (inline code); drawn at 92 % of the base size unless `size` is set
    pub mono: bool,
    pub strike: bool,
    pub underline: bool,
    /// the url a click on this span opens (the caller checks it with `markdown::is_safe_url`)
    pub link: Option<String>,
    pub color: Option<Color>,
    /// exact size in px, overriding the base size
    pub size: Option<f32>,
    /// rounded background behind the span's text
    pub bg: Option<Color>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Span {
    pub text: String,
    pub style: SpanStyle,
}

/// A run of formatted text plus its base look.
#[derive(Clone, Debug)]
pub struct RichText {
    pub spans: Vec<Span>,
    /// size and weight of the text without span overrides (alignment and `wrap` follow it too)
    pub base: Style,
    /// uniform line height in px; 0 = 1.45 x the base size, rounded
    pub line_height: f32,
}

impl Default for RichText {
    fn default() -> Self {
        RichText::new()
    }
}

impl RichText {
    pub fn new() -> RichText {
        RichText { spans: Vec::new(), base: Style::BODY, line_height: 0.0 }
    }
    pub fn with_base(mut self, base: Style) -> RichText {
        self.base = base;
        self
    }
    pub fn with_line_height(mut self, px: f32) -> RichText {
        self.line_height = px;
        self
    }
    /// Add `text` in `style`; empty text is ignored and a span with the same style as the last one is merged into it.
    pub fn push(mut self, text: &str, style: SpanStyle) -> RichText {
        self.push_mut(text, style);
        self
    }
    pub fn push_mut(&mut self, text: &str, style: SpanStyle) {
        if text.is_empty() {
            return;
        }
        match self.spans.last_mut() {
            Some(l) if l.style == style => l.text.push_str(text),
            _ => self.spans.push(Span { text: text.to_string(), style }),
        }
    }
    pub fn text(self, t: &str) -> RichText {
        self.push(t, SpanStyle::default())
    }
    pub fn bold(self, t: &str) -> RichText {
        self.push(t, SpanStyle { bold: true, ..Default::default() })
    }
    pub fn italic(self, t: &str) -> RichText {
        self.push(t, SpanStyle { italic: true, ..Default::default() })
    }
    pub fn strike(self, t: &str) -> RichText {
        self.push(t, SpanStyle { strike: true, ..Default::default() })
    }
    pub fn code(self, t: &str) -> RichText {
        self.push(t, SpanStyle { mono: true, ..Default::default() })
    }
    pub fn colored(self, t: &str, c: Color) -> RichText {
        self.push(t, SpanStyle { color: Some(c), ..Default::default() })
    }
    pub fn link(self, t: &str, url: &str) -> RichText {
        self.push(t, SpanStyle { underline: true, link: Some(url.to_string()), ..Default::default() })
    }
    /// Append text in the style of the last span (streaming tokens), or unstyled when empty.
    pub fn append_str(&mut self, t: &str) {
        let st = self.spans.last().map(|s| s.style.clone()).unwrap_or_default();
        self.push_mut(t, st);
    }
    /// The text of all spans.
    pub fn plain_text(&self) -> String {
        self.spans.iter().map(|s| s.text.as_str()).collect()
    }
    pub fn is_empty(&self) -> bool {
        self.spans.iter().all(|s| s.text.is_empty())
    }
    fn lh(&self) -> f32 {
        if self.line_height > 0.0 {
            self.line_height
        } else {
            (self.base.size * 1.45).round()
        }
    }
}

// ---------------------------------------------------------------- fonts

/// The monospace family to use: Cascadia Mono, else Consolas, else Courier New (first one installed).
pub fn mono_family(dw: &IDWriteFactory) -> &'static str {
    static F: OnceLock<&'static str> = OnceLock::new();
    F.get_or_init(|| unsafe {
        let mut coll: Option<IDWriteFontCollection> = None;
        if dw.GetSystemFontCollection(&mut coll, false).is_ok() {
            if let Some(c) = coll {
                for name in ["Cascadia Mono", "Consolas"] {
                    let (mut i, mut ok) = (0u32, BOOL(0));
                    if c.FindFamilyName(&HSTRING::from(name), &mut i, &mut ok).is_ok() && ok.as_bool() {
                        return name;
                    }
                }
            }
        }
        "Courier New"
    })
}

fn bold_weight(base: u16) -> i32 {
    if base < 600 {
        600
    } else {
        700
    }
}

// ---------------------------------------------------------------- layout

/// One hard line (between `'\n'`s) of the text with its DirectWrite layout.
struct Line {
    /// UTF-16 offset of the line in the whole text
    start: u32,
    len: u32,
    spans: Vec<Span>,
    layout: Option<IDWriteTextLayout>,
    y: f32,
    h: f32,
    /// widest row
    w: f32,
    bgs: Vec<(Rect, Color)>,
}

/// Wrapped, formatted text. See the module docs.
pub struct RichLayout {
    rt: RichText,
    width: f32,
    lines: Vec<Line>,
    built: u32,
    height: f32,
    content_w: f32,
}

fn utf16_len(s: &str) -> u32 {
    s.encode_utf16().count() as u32
}

/// Cut spans at `'\n'` into lines of spans (adjacent equal styles merged). Always at least one line.
fn split_lines(rt: &RichText) -> Vec<Vec<Span>> {
    let mut lines: Vec<Vec<Span>> = vec![Vec::new()];
    for sp in &rt.spans {
        for (i, piece) in sp.text.split('\n').enumerate() {
            if i > 0 {
                lines.push(Vec::new());
            }
            if piece.is_empty() {
                continue;
            }
            let cur = lines.last_mut().expect("a line");
            match cur.last_mut() {
                Some(l) if l.style == sp.style => l.text.push_str(piece),
                _ => cur.push(Span { text: piece.to_string(), style: sp.style.clone() }),
            }
        }
    }
    lines
}

fn range_rects(lay: &IDWriteTextLayout, start: u32, len: u32) -> Vec<Rect> {
    if len == 0 {
        return Vec::new();
    }
    unsafe {
        let mut n = 0u32;
        let _ = lay.HitTestTextRange(start, len, 0.0, 0.0, None, &mut n); // E_NOT_SUFFICIENT_BUFFER, n is the count
        if n == 0 {
            return Vec::new();
        }
        let mut v = vec![DWRITE_HIT_TEST_METRICS::default(); n as usize];
        if lay.HitTestTextRange(start, len, 0.0, 0.0, Some(&mut v), &mut n).is_err() {
            return Vec::new();
        }
        v.truncate(n as usize);
        v.iter().map(|m| Rect::new(m.left, m.top, m.width, m.height)).collect()
    }
}

fn build_line(cv: &Canvas, spans: &[Span], base: Style, lh: f32, width: f32) -> Result<(IDWriteTextLayout, Vec<(Rect, Color)>, f32, f32)> {
    let text: String = spans.iter().map(|s| s.text.as_str()).collect();
    let wide: Vec<u16> = text.encode_utf16().collect();
    // the format of the shared text cache carries the app's font family / collection, alignment and word wrapping
    let tmpl = cv.layout(" ", Style { wrap: true, valign: VAlign::Top, ..base }, 100.0, 100.0)?;
    let fmt: IDWriteTextFormat = tmpl.cast()?;
    unsafe {
        let lay = cv.gfx.dw.CreateTextLayout(&wide, &fmt, width.max(1.0), 100_000.0)?;
        lay.SetLineSpacing(DWRITE_LINE_SPACING_METHOD_UNIFORM, lh, (lh * 0.8).round())?;
        let all = DWRITE_TEXT_RANGE { startPosition: 0, length: wide.len() as u32 };
        lay.SetFontSize(base.size, all)?;
        let mut sys: Option<IDWriteFontCollection> = None;
        let mut brushes: HashMap<[u32; 4], ID2D1SolidColorBrush> = HashMap::new();
        let mut bgs = Vec::new();
        let mut at = 0u32;
        for sp in spans {
            let len = utf16_len(&sp.text);
            let r = DWRITE_TEXT_RANGE { startPosition: at, length: len };
            let st = &sp.style;
            if st.bold {
                lay.SetFontWeight(DWRITE_FONT_WEIGHT(bold_weight(base.weight)), r)?;
            }
            if st.italic {
                lay.SetFontStyle(DWRITE_FONT_STYLE_ITALIC, r)?;
            }
            if st.mono {
                if sys.is_none() {
                    let mut c: Option<IDWriteFontCollection> = None;
                    let _ = cv.gfx.dw.GetSystemFontCollection(&mut c, false);
                    sys = c;
                }
                if let Some(c) = &sys {
                    lay.SetFontCollection(c, r)?;
                }
                lay.SetFontFamilyName(&HSTRING::from(mono_family(&cv.gfx.dw)), r)?;
            }
            if let Some(s) = st.size.or(st.mono.then_some((base.size * 0.92).round().max(8.0))) {
                lay.SetFontSize(s, r)?;
            }
            if st.strike {
                lay.SetStrikethrough(true, r)?;
            }
            if st.underline {
                lay.SetUnderline(true, r)?;
            }
            if let Some(c) = st.color {
                let key = [c.r.to_bits(), c.g.to_bits(), c.b.to_bits(), c.a.to_bits()];
                if !brushes.contains_key(&key) {
                    brushes.insert(key, cv.dc.CreateSolidColorBrush(&c.d2d(), None)?);
                }
                let b: windows::core::IUnknown = brushes[&key].cast()?;
                lay.SetDrawingEffect(&b, r)?;
            }
            if let Some(bg) = st.bg {
                for rc in range_rects(&lay, at, len) {
                    bgs.push((Rect::new(rc.x - 2.0, rc.y + 2.0, rc.w + 4.0, (rc.h - 4.0).max(1.0)), bg));
                }
            }
            at += len;
        }
        let mut m = DWRITE_TEXT_METRICS::default();
        lay.GetMetrics(&mut m)?;
        Ok((lay, bgs, m.height.max(lh), m.width))
    }
}

impl RichLayout {
    /// Lay `rt` out at `width` logical px.
    pub fn new(cv: &Canvas, rt: &RichText, width: f32) -> Result<RichLayout> {
        let mut l = RichLayout { rt: rt.clone(), width, lines: Vec::new(), built: 0, height: 0.0, content_w: 0.0 };
        l.sync(cv)?;
        Ok(l)
    }

    /// Re-flow the current text; reuses every line whose text, formatting and settings did not change. `force` rebuilds all.
    fn sync_with(&mut self, cv: &Canvas, force: bool) -> Result<()> {
        let new = split_lines(&self.rt);
        let lh = self.rt.lh();
        let mut old = std::mem::take(&mut self.lines);
        let mut out: Vec<Line> = Vec::with_capacity(new.len());
        let mut reuse = !force;
        let mut start = 0u32;
        for (i, spans) in new.into_iter().enumerate() {
            let len: u32 = spans.iter().map(|s| utf16_len(&s.text)).sum();
            let same = reuse && old.get(i).is_some_and(|o| o.spans == spans && o.start == start);
            let mut line = if same {
                std::mem::replace(&mut old[i], Line { start: 0, len: 0, spans: Vec::new(), layout: None, y: 0.0, h: 0.0, w: 0.0, bgs: Vec::new() })
            } else {
                reuse = false; // everything after the first change is new (its y and offsets moved)
                if len == 0 {
                    Line { start, len, spans, layout: None, y: 0.0, h: lh, w: 0.0, bgs: Vec::new() }
                } else {
                    let (lay, bgs, h, w) = build_line(cv, &spans, self.rt.base, lh, self.width)?;
                    self.built += 1;
                    Line { start, len, spans, layout: Some(lay), y: 0.0, h, w, bgs }
                }
            };
            line.start = start;
            start += len + 1;
            out.push(line);
        }
        let mut y = 0.0;
        let mut cw = 0.0f32;
        for l in &mut out {
            l.y = y;
            y += l.h;
            cw = cw.max(l.w);
        }
        self.lines = out;
        self.height = y;
        self.content_w = cw;
        Ok(())
    }

    fn sync(&mut self, cv: &Canvas) -> Result<()> {
        self.sync_with(cv, false)
    }

    /// Replace the whole text; lines identical to the old ones (from the top) are kept. Returns how many layouts were built.
    pub fn set_text(&mut self, cv: &Canvas, rt: &RichText) -> Result<u32> {
        let before = self.built;
        let force = rt.base != self.rt.base || rt.lh() != self.rt.lh();
        self.rt = rt.clone();
        self.sync_with(cv, force)?;
        Ok(self.built - before)
    }

    /// Append rich text (streaming): only the last line and the new ones are laid out.
    pub fn append(&mut self, cv: &Canvas, more: &RichText) -> Result<()> {
        for s in &more.spans {
            self.rt.push_mut(&s.text, s.style.clone());
        }
        self.sync(cv)
    }

    /// Append plain tokens in the style of the last span.
    pub fn append_str(&mut self, cv: &Canvas, tokens: &str) -> Result<()> {
        self.rt.append_str(tokens);
        self.sync(cv)
    }

    /// Wrap to another width (rebuilds every line).
    pub fn set_width(&mut self, cv: &Canvas, width: f32) -> Result<()> {
        if (width - self.width).abs() < 0.01 {
            return Ok(());
        }
        self.width = width;
        self.sync_with(cv, true)
    }

    // ------------------------------------------------------------ metrics

    pub fn width(&self) -> f32 {
        self.width
    }
    /// Total height in px.
    pub fn height(&self) -> f32 {
        self.height
    }
    /// (widest row, total height): the box the text actually occupies.
    pub fn size(&self) -> (f32, f32) {
        (self.content_w, self.height)
    }
    /// How many DirectWrite layouts were built so far (tests assert streaming stays cheap with it).
    pub fn rebuilds(&self) -> u32 {
        self.built
    }
    pub fn text(&self) -> String {
        self.rt.plain_text()
    }
    pub fn rich(&self) -> &RichText {
        &self.rt
    }
    /// Length of the text in UTF-16 units.
    pub fn len(&self) -> usize {
        self.lines.last().map_or(0, |l| (l.start + l.len) as usize)
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// The text between two UTF-16 offsets (clamped).
    pub fn text_range(&self, r: Range<usize>) -> String {
        let w: Vec<u16> = self.text().encode_utf16().collect();
        let (a, b) = (r.start.min(w.len()), r.end.min(w.len()));
        if a >= b {
            return String::new();
        }
        String::from_utf16_lossy(&w[a..b])
    }
    /// `char` index of a UTF-16 offset.
    pub fn utf16_to_char(&self, u: usize) -> usize {
        let mut units = 0;
        for (i, c) in self.text().chars().enumerate() {
            if units >= u {
                return i;
            }
            units += c.len_utf16();
        }
        self.text().chars().count()
    }
    /// UTF-16 offset of a `char` index.
    pub fn char_to_utf16(&self, ci: usize) -> usize {
        self.text().chars().take(ci).map(char::len_utf16).sum()
    }

    // ------------------------------------------------------------ hit testing

    fn line_at_y(&self, y: f32) -> usize {
        self.lines.partition_point(|l| l.y + l.h <= y).min(self.lines.len().saturating_sub(1))
    }

    fn line_of(&self, idx: usize) -> usize {
        let i = idx as u32;
        self.lines.partition_point(|l| l.start <= i).saturating_sub(1)
    }

    /// (index, pointer is on a glyph, span link under it)
    fn probe(&self, p: Pt) -> (usize, bool, Option<&str>) {
        let Some(_) = self.lines.first() else { return (0, false, None) };
        let li = self.line_at_y(p.1);
        let l = &self.lines[li];
        let Some(lay) = &l.layout else { return (l.start as usize, false, None) };
        let (mut trailing, mut inside) = (BOOL(0), BOOL(0));
        let mut m = DWRITE_HIT_TEST_METRICS::default();
        if unsafe { lay.HitTestPoint(p.0, p.1 - l.y, &mut trailing, &mut inside, &mut m) }.is_err() {
            return (l.start as usize, false, None);
        }
        let idx = l.start + m.textPosition + if trailing.as_bool() { m.length } else { 0 };
        let idx = idx.min(l.start + l.len);
        let on_text = inside.as_bool() && p.1 >= l.y && p.1 < l.y + l.h;
        let mut at = 0;
        let mut link = None;
        for sp in &l.spans {
            let len = utf16_len(&sp.text);
            if m.textPosition >= at && m.textPosition < at + len {
                link = sp.style.link.as_deref();
                break;
            }
            at += len;
        }
        (idx as usize, on_text, link)
    }

    /// The nearest text index to `p`, always (dragging a selection outside the text clamps to the ends).
    pub fn index_at(&self, p: Pt) -> usize {
        self.probe(p).0
    }

    /// The text index under `p`, `None` when `p` is outside the layout's box.
    pub fn hit_test(&self, p: Pt) -> Option<usize> {
        (p.0 >= 0.0 && p.1 >= 0.0 && p.0 <= self.width && p.1 < self.height).then(|| self.probe(p).0)
    }

    /// The url of the link span under `p` (only when the pointer is on its glyphs).
    pub fn link_at(&self, p: Pt) -> Option<&str> {
        let (_, on_text, link) = self.probe(p);
        if on_text {
            link
        } else {
            None
        }
    }

    /// The caret at a UTF-16 index: a 1 px wide bar as high as its line.
    pub fn caret_rect(&self, idx: usize) -> Rect {
        let Some(_) = self.lines.first() else { return Rect::new(0.0, 0.0, 1.0, self.rt.lh()) };
        let li = self.line_of(idx.min(self.len()));
        let l = &self.lines[li];
        let local = (idx.min(self.len()) as u32).saturating_sub(l.start).min(l.len);
        let Some(lay) = &l.layout else { return Rect::new(0.0, l.y, 1.0, l.h) };
        let (mut x, mut y) = (0.0f32, 0.0f32);
        let mut m = DWRITE_HIT_TEST_METRICS::default();
        if unsafe { lay.HitTestTextPosition(local, false, &mut x, &mut y, &mut m) }.is_err() {
            return Rect::new(0.0, l.y, 1.0, l.h);
        }
        Rect::new(x, l.y + y, 1.0, m.height)
    }

    /// Rectangles covering the UTF-16 range, one per visual row (a selected line break adds a short stub at the row's end).
    pub fn selection_rects(&self, r: Range<usize>) -> Vec<Rect> {
        let (a, b) = (r.start.min(self.len()) as u32, r.end.min(self.len() + 1) as u32);
        let mut out = Vec::new();
        if a >= b {
            return out;
        }
        for (i, l) in self.lines.iter().enumerate() {
            let (ls, le) = (l.start, l.start + l.len);
            let (s, e) = (a.max(ls), b.min(le));
            if s < e {
                if let Some(lay) = &l.layout {
                    for mut rc in range_rects(lay, s - ls, e - s) {
                        rc.y += l.y;
                        out.push(rc);
                    }
                }
            }
            // the line break after this line is part of the range
            if b > le && a <= le && i + 1 < self.lines.len() {
                let end = self.caret_rect(le as usize);
                out.push(Rect::new(end.x, end.y, 6.0, end.h));
            }
        }
        out
    }

    // ------------------------------------------------------------ drawing

    /// Draw at `origin`; spans without their own colour use `color`.
    pub fn draw(&self, cv: &Canvas, origin: Pt, color: Color) {
        for l in &self.lines {
            for (rc, c) in &l.bgs {
                cv.fill_rr(rc.offset(origin.0, origin.1 + l.y), 4.0, *c);
            }
        }
        for l in &self.lines {
            if let Some(lay) = &l.layout {
                unsafe {
                    cv.res.brush().SetColor(&color.d2d());
                    cv.dc.DrawTextLayout(pt(origin.0, origin.1 + l.y), lay, cv.res.brush(), D2D1_DRAW_TEXT_OPTIONS_ENABLE_COLOR_FONT);
                }
            }
        }
    }

    /// Fill the selection rectangles of `range` (call before `draw`, so the text sits on top).
    pub fn draw_selection(&self, cv: &Canvas, origin: Pt, range: Range<usize>, color: Color) {
        for rc in self.selection_rects(range) {
            cv.fill_rr(rc.offset(origin.0, origin.1), 2.0, color);
        }
    }

    /// Bounds of the text box at `origin`.
    pub fn rect(&self, origin: Pt) -> Rect {
        Rect::new(origin.0, origin.1, self.width, self.height)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::Harness;

    fn lay(h: &Harness, rt: &RichText, w: f32) -> RichLayout {
        let cv = Canvas::new(&h.gfx, &h.res, 1.0);
        RichLayout::new(&cv, rt, w).expect("layout")
    }

    const LOREM: &str = "The quick brown fox jumps over the lazy dog and keeps running through the long meadow until night";

    #[test]
    fn narrower_text_wraps_and_gets_taller() {
        let h = Harness::new();
        let rt = RichText::new().text(LOREM);
        let wide = lay(&h, &rt, 600.0);
        let narrow = lay(&h, &rt, 120.0);
        assert!(narrow.height() > wide.height() * 2.0, "{} vs {}", narrow.height(), wide.height());
        assert!(narrow.size().0 <= 120.5);
        assert_eq!(wide.height(), 19.0, "one line of 13 px text is 19 px (1.45 line height)");
    }

    #[test]
    fn bold_and_mono_change_the_width() {
        let h = Harness::new();
        let plain = lay(&h, &RichText::new().text("Hello world"), 600.0).size().0;
        let bold = lay(&h, &RichText::new().bold("Hello world"), 600.0).size().0;
        let mono = lay(&h, &RichText::new().code("Hello world"), 600.0).size().0;
        let ital = lay(&h, &RichText::new().italic("Hello world"), 600.0).size().0;
        assert!(bold > plain + 1.0, "bold {bold} plain {plain}");
        assert!((mono - plain).abs() > 1.0, "mono {mono} plain {plain}");
        assert!(ital > 0.0);
    }

    #[test]
    fn hit_test_caret_and_selection_round_trip() {
        let h = Harness::new();
        let l = lay(&h, &RichText::new().text("Hello world"), 600.0);
        // the caret at an index, hit-tested from the middle of the next glyph, gives that index back
        for idx in [0usize, 3, 6, 11] {
            let c = l.caret_rect(idx);
            assert!(c.h > 10.0);
            let probe = (c.x + 0.3, c.y + c.h / 2.0);
            let got = l.hit_test(probe).unwrap();
            assert_eq!(got, idx, "caret {idx} at {c:?}");
        }
        let a = l.caret_rect(6).x;
        let b = l.caret_rect(11).x;
        let mid = (a + b) / 2.0 - 1.0;
        let i = l.hit_test((mid, 8.0)).unwrap();
        assert!((6..=11).contains(&i));
        let rs = l.selection_rects(0..5);
        assert_eq!(rs.len(), 1);
        assert!(rs[0].x < 1.0 && (rs[0].right() - l.caret_rect(5).x).abs() < 1.0);
        assert_eq!(l.text_range(6..11), "world");
        assert_eq!(l.hit_test((-5.0, 5.0)), None);
        assert_eq!(l.hit_test((5.0, 500.0)), None);
        assert_eq!(l.index_at((9999.0, 5.0)), 11, "dragging past the end clamps to the end");
    }

    #[test]
    fn selection_over_wrapped_lines_and_newlines_gives_several_rects() {
        let h = Harness::new();
        let l = lay(&h, &RichText::new().text(LOREM), 120.0);
        let rs = l.selection_rects(0..LOREM.len());
        assert!(rs.len() >= 3, "{}", rs.len());
        assert!(rs.windows(2).all(|w| w[1].y >= w[0].y));
        let two = lay(&h, &RichText::new().text("one\ntwo"), 300.0);
        assert_eq!(two.height(), 38.0);
        let rs = two.selection_rects(0..7);
        assert_eq!(rs.len(), 3, "two rows plus the stub of the selected line break: {rs:?}");
        assert!(rs[2].y > rs[0].y);
        assert_eq!(two.len(), 7);
        assert!((two.caret_rect(4).y - 19.0).abs() < 2.5, "index 4 is the start of the second line: {:?}", two.caret_rect(4));
        assert_eq!(two.hit_test((1.0, 25.0)), Some(4));
    }

    #[test]
    fn links_are_found_only_on_their_glyphs() {
        let h = Harness::new();
        let l = lay(&h, &RichText::new().text("see ").link("the docs", "https://e.com/x").text(" now"), 600.0);
        let on = l.caret_rect(6);
        assert_eq!(l.link_at((on.x + 2.0, 9.0)), Some("https://e.com/x"));
        assert_eq!(l.link_at((1.0, 9.0)), None, "plain text");
        assert_eq!(l.link_at((590.0, 9.0)), None, "empty space past the end of the line");
        assert_eq!(l.link_at((on.x + 2.0, 40.0)), None, "below the text");
    }

    #[test]
    fn cyrillic_emoji_and_index_conversions() {
        let h = Harness::new();
        let l = lay(&h, &RichText::new().text("Привет 😀 мир"), 600.0);
        assert_eq!(l.len(), "Привет 😀 мир".encode_utf16().count());
        assert_eq!(l.len(), 13, "the emoji is a surrogate pair: 12 chars, 13 units");
        assert_eq!(l.char_to_utf16(8), 9);
        assert_eq!(l.utf16_to_char(9), 8);
        assert_eq!(l.utf16_to_char(l.len()), 12);
        assert_eq!(l.text_range(7..9), "😀");
        // a hit on the emoji never lands between the two surrogates
        let c = l.caret_rect(7);
        let w = l.caret_rect(9).x - c.x;
        assert!(w > 5.0, "the emoji has width");
        for k in 1..8 {
            let i = l.hit_test((c.x + w * k as f32 / 8.0, 9.0)).unwrap();
            assert!(i == 7 || i == 9, "{i}");
        }
        assert!(l.size().0 > 80.0);
    }

    #[test]
    fn streaming_appends_rebuild_only_the_tail() {
        let h = Harness::new();
        let cv = Canvas::new(&h.gfx, &h.res, 1.0);
        let mut l = RichLayout::new(&cv, &RichText::new().text("first line\nsecond line\nthird"), 300.0).unwrap();
        assert_eq!(l.rebuilds(), 3);
        let h0 = l.height();
        for tok in [" tok", "en", " one", " two"] {
            let before = l.rebuilds();
            l.append_str(&cv, tok).unwrap();
            assert_eq!(l.rebuilds() - before, 1, "one layout per token");
        }
        assert_eq!(l.text(), "first line\nsecond line\nthird token one two");
        assert_eq!(l.height(), h0);
        // a new line: the finished lines stay
        let before = l.rebuilds();
        l.append_str(&cv, "\nfourth").unwrap();
        assert_eq!(l.rebuilds() - before, 1, "the finished lines are unchanged, only the new line is built");
        assert!(l.height() > h0);
        // same text again: nothing is rebuilt
        let t = l.rich().clone();
        assert_eq!(l.set_text(&cv, &t).unwrap(), 0);
        // changing the width rebuilds everything
        l.set_width(&cv, 100.0).unwrap();
        assert_eq!(l.rebuilds() - before, 5, "1 for the new line + the 4 lines rebuilt at the new width");
        // 200 tokens into one paragraph: 200 layouts, not 200 x lines
        let mut p = RichLayout::new(&cv, &RichText::new().text("x"), 200.0).unwrap();
        for _ in 0..200 {
            p.append_str(&cv, " word").unwrap();
        }
        assert_eq!(p.rebuilds(), 201);
        assert!(p.height() > 100.0);
    }

    #[test]
    fn colours_backgrounds_strike_underline_and_the_png_gallery() {
        let h = Harness::new();
        let t = crate::theme::Theme::pill_dark();
        let rt = RichText::new()
            .text("Normal ")
            .bold("bold ")
            .italic("italic ")
            .push("code", SpanStyle { mono: true, bg: Some(t.raised2), ..Default::default() })
            .text(" ")
            .strike("struck")
            .text(" ")
            .colored("red", t.red)
            .text(" ")
            .link("link", "https://e.com")
            .text(" Привет мир 😀");
        let mut seen_red = false;
        let mut seen_bg = false;
        for scale in [1.0f32, 2.0] {
            let img = h.render(300, 60, scale, t.bg, |cv| {
                let l = RichLayout::new(cv, &rt, 280.0).unwrap();
                l.draw_selection(cv, (10.0, 10.0), 3..9, t.accent.alpha(0.4));
                l.draw(cv, (10.0, 10.0), t.text);
            });
            seen_red |= img.count(None, |p| p[0] > 200 && p[1] < 120 && p[2] < 120) > 5;
            seen_bg |= img.count(None, |p| p[0] == 0x29 && p[1] == 0x29 && p[2] == 0x29) > 20;
            let dir = std::path::Path::new(&std::env::var("CARGO_TARGET_DIR").unwrap_or_else(|_| "target".into())).join("kit-images");
            if let Err(e) = img.save_png(&h, dir.join(format!("richtext_spans@{scale}x.png"))) {
                eprintln!("warning: could not write the richtext PNG: {e}");
            }
        }
        assert!(seen_red, "a span colour shows");
        assert!(seen_bg, "the inline-code background is painted");
        // text pixels exist in the base colour
        let img = h.render(200, 30, 1.0, t.bg, |cv| lay(&h, &RichText::new().text("Hi"), 100.0).draw(cv, (4.0, 4.0), t.text));
        assert!(img.count(None, |p| p[0] > 150 && p[1] > 150) > 5);
    }

    #[test]
    fn empty_text_has_one_line_and_a_caret() {
        let h = Harness::new();
        let l = lay(&h, &RichText::new(), 100.0);
        assert!(l.is_empty() && l.height() == 19.0);
        assert_eq!(l.caret_rect(0).h, 19.0);
        assert_eq!(l.hit_test((5.0, 5.0)), Some(0));
        assert!(l.selection_rects(0..3).is_empty());
        assert_eq!(l.link_at((1.0, 1.0)), None);
    }
}
