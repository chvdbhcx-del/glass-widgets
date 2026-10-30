//! `MarkdownView`: markdown blocks laid out into a vertical stack with Pill's look, with hit-testing, a text selection across blocks,
//! copy buttons on code blocks and the animations that belong to them. Built on [`super::RichLayout`].
//!
//! ```ignore
//! let blocks = markdown::parse(&answer);
//! let mut view = MarkdownView::new(&cv, blocks, 360.0, &theme, &MarkdownLabels { copy: "Copy", copied: "Copied" })?;
//! view.draw(&cv, (16.0, 16.0), &theme);                       // every frame
//! match view.hit((mx - 16.0, my - 16.0)) {                    // on mouse move / click
//!     Hit::CopyButton(leaf) => { clipboard.set(view.leaf_text(leaf)); view.mark_copied(leaf); }
//!     Hit::Link(url) => open(&url),
//!     Hit::Text(pos) => view.set_selection(Some(Selection { anchor: pos, focus: pos })),
//!     Hit::None => {}
//! }
//! view.set_hover(&hit);                                       // animates the button hover
//! if view.step(dt) { /* ask for another frame */ }
//! // streaming: re-parse the growing answer; only the changed tail is laid out again
//! view.update(&cv, markdown::parse(&answer_so_far), &theme)?;
//! ```
//!
//! # Look
//! Body 13 px / 19 px lines, paragraphs 10 px apart, headings h1 20, h2 16, h3 14, others 13 (semibold; an extra 6 px above), bullet lists
//! with a 20 px gutter (26 px for numbers), block quotes with a 3 px bar and dimmed text, rules, fenced code in a rounded `theme.card` frame:
//! a 28 px header with the language on the left and a copy button on the right, then the monospace text (12 px / 18 px, wrapped).
//!
//! # Indices
//! Text is addressed by [`TextPos`] = (leaf, UTF-16 offset in that leaf). A *leaf* is one run of text: a paragraph, a heading, a list item's
//! own text, a code block, in document order. A code block's copy button is identified by its leaf index (`leaf_text(leaf)` is the code).
//!
//! # Theme
//! Colours of links (accent) and inline code (background) are baked into the text layouts when they are built; frames, bullets, bars and the
//! base text colours are read from the theme given to `draw`. After a theme change call `relayout`/`update` with the new theme.
use super::*;
use crate::ease::{curves, dur, Tween};
use crate::icons::{Glyph, Paint};
use crate::markdown::{Block, Inline, ListItem};
use crate::theme::Theme;

const GAP: f32 = 10.0;
const HEADING_TOP: f32 = 6.0;
const ITEM_GAP: f32 = 4.0;
const BULLET_GUTTER: f32 = 20.0;
const NUMBER_GUTTER: f32 = 26.0;
const QUOTE_INDENT: f32 = 14.0;
const CODE_HEAD: f32 = 28.0;
const CODE_PAD: f32 = 12.0;
const BTN_H: f32 = 22.0;
const COPIED_HOLD: f32 = 1.6;
const BODY: f32 = 13.0;

/// Text of the copy button; the crate has no strings of its own.
#[derive(Clone, Copy, Debug)]
pub struct MarkdownLabels<'a> {
    pub copy: &'a str,
    pub copied: &'a str,
}

/// A position in the text: leaf index and UTF-16 offset inside it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct TextPos {
    pub leaf: usize,
    pub index: usize,
}

/// What is under a point.
#[derive(Clone, Debug, PartialEq)]
pub enum Hit {
    None,
    /// the copy button of the code block whose leaf index this is
    CopyButton(usize),
    /// a safe link (`http`, `https`, `mailto`)
    Link(String),
    Text(TextPos),
}

/// A selection between two positions (either order).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Selection {
    pub anchor: TextPos,
    pub focus: TextPos,
}

impl Selection {
    pub fn ordered(&self) -> (TextPos, TextPos) {
        if self.anchor <= self.focus {
            (self.anchor, self.focus)
        } else {
            (self.focus, self.anchor)
        }
    }
    pub fn is_empty(&self) -> bool {
        self.anchor == self.focus
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Role {
    Text,
    Dim,
}

struct Leaf {
    layout: RichLayout,
    pos: Pt,
    role: Role,
    code: bool,
}

enum Deco {
    Bullet(Pt),
    Marker { layout: RichLayout, pos: Pt },
    Bar(Rect),
    Rule(Rect),
    Frame { leaf: usize, rect: Rect, btn: Rect, lang: Option<(RichLayout, Pt)> },
}

struct Top {
    src: Block,
    end_y: f32,
    leaf0: usize,
    deco0: usize,
}

/// Animation state of one copy button.
struct BtnFx {
    hover: Tween<f32>,
    copied: Tween<f32>,
    hold: f32,
}

impl BtnFx {
    fn new() -> BtnFx {
        BtnFx { hover: Tween::at_rest(0.0), copied: Tween::at_rest(0.0), hold: 0.0 }
    }
}

pub struct MarkdownView {
    tops: Vec<Top>,
    leaves: Vec<Leaf>,
    decos: Vec<Deco>,
    width: f32,
    height: f32,
    theme: Theme,
    copy: String,
    copied: String,
    btn_w: f32,
    built: u32,
    selection: Option<Selection>,
    fx: HashMap<usize, BtnFx>,
}

fn push_inlines(rt: &mut RichText, inl: &[Inline], st: &SpanStyle, th: &Theme) {
    for x in inl {
        match x {
            Inline::Text(t) => rt.push_mut(t, st.clone()),
            Inline::Bold(c) => push_inlines(rt, c, &SpanStyle { bold: true, ..st.clone() }, th),
            Inline::Italic(c) => push_inlines(rt, c, &SpanStyle { italic: true, ..st.clone() }, th),
            Inline::Strike(c) => push_inlines(rt, c, &SpanStyle { strike: true, ..st.clone() }, th),
            Inline::Code(t) => rt.push_mut(t, SpanStyle { mono: true, bg: Some(th.raised2), ..st.clone() }),
            Inline::Link(l) => {
                let s = SpanStyle { color: Some(th.accent), underline: true, link: l.is_safe().then(|| l.url.clone()), ..st.clone() };
                rt.push_mut(&l.text, s);
            }
        }
    }
}

fn rich(inl: &[Inline], size: f32, weight: u16, th: &Theme) -> RichText {
    let mut rt = RichText::new().with_base(Style::new(size, weight)).with_line_height((size * 1.45).round());
    push_inlines(&mut rt, inl, &SpanStyle::default(), th);
    rt
}

/// Draw a 24 x 24 glyph into `r`.
fn glyph(cv: &Canvas, g: Glyph, r: Rect, c: Color) {
    if c.a <= 0.0 {
        return;
    }
    let Ok(parts) = cv.res.glyph(cv.gfx, g) else { return };
    let k = r.w / 24.0;
    cv.with_transform(windows::Foundation::Numerics::Matrix3x2 { M11: k, M12: 0.0, M21: 0.0, M22: k, M31: r.x, M32: r.y }, |cv| {
        for (geom, paint) in parts.iter() {
            let Ok(geom) = geom.cast::<ID2D1Geometry>() else { continue };
            match *paint {
                Paint::Fill => cv.geometry(&geom, Some(c), None),
                Paint::Stroke(w) => cv.geometry(&geom, None, Some((c, w))),
                Paint::Both(w) => cv.geometry(&geom, Some(c), Some((c, w))),
            }
        }
    });
}

impl MarkdownView {
    /// Lay `blocks` out at `width` logical px.
    pub fn new(cv: &Canvas, blocks: Vec<Block>, width: f32, theme: &Theme, labels: &MarkdownLabels) -> Result<MarkdownView> {
        let mut v = MarkdownView {
            tops: Vec::new(),
            leaves: Vec::new(),
            decos: Vec::new(),
            width,
            height: 0.0,
            theme: *theme,
            copy: labels.copy.to_string(),
            copied: labels.copied.to_string(),
            btn_w: 0.0,
            built: 0,
            selection: None,
            fx: HashMap::new(),
        };
        v.measure_button(cv);
        v.build_from(cv, 0, &blocks)?;
        Ok(v)
    }

    fn measure_button(&mut self, cv: &Canvas) {
        let st = Style::new(11.0, 500);
        // icon + gap + the longer label, so the button does not change size when it says "copied"
        self.btn_w = (8.0 + 12.0 + 5.0 + cv.text_width(&self.copy, st).max(cv.text_width(&self.copied, st)) + 10.0).ceil();
    }

    // ------------------------------------------------------------ layout

    /// Total height in px.
    pub fn height(&self) -> f32 {
        self.height
    }
    pub fn width(&self) -> f32 {
        self.width
    }
    /// The blocks being shown.
    pub fn blocks(&self) -> impl Iterator<Item = &Block> {
        self.tops.iter().map(|t| &t.src)
    }
    /// How many DirectWrite layouts were built so far (tests assert streaming reuses the finished blocks).
    pub fn rebuilds(&self) -> u32 {
        self.built
    }
    pub fn leaf_count(&self) -> usize {
        self.leaves.len()
    }

    /// Wrap to another width (everything is laid out again).
    pub fn relayout(&mut self, cv: &Canvas, width: f32, theme: &Theme) -> Result<()> {
        let blocks: Vec<Block> = self.tops.iter().map(|t| t.src.clone()).collect();
        self.width = width;
        self.theme = *theme;
        self.build_from(cv, 0, &blocks)
    }

    /// Show new blocks (a growing answer): the leading blocks that are identical stay as they are, the rest is laid out again.
    /// Returns how many layouts were built.
    pub fn update(&mut self, cv: &Canvas, blocks: Vec<Block>, theme: &Theme) -> Result<u32> {
        let before = self.built;
        let mut k = 0;
        if *theme == self.theme {
            while k < self.tops.len() && k < blocks.len() && self.tops[k].src == blocks[k] {
                k += 1;
            }
            if k == self.tops.len() && k == blocks.len() {
                return Ok(0);
            }
        }
        self.theme = *theme;
        self.build_from(cv, k, &blocks)?;
        Ok(self.built - before)
    }

    fn build_from(&mut self, cv: &Canvas, from: usize, blocks: &[Block]) -> Result<()> {
        let from = from.min(self.tops.len());
        let (leaf0, deco0) = self.tops.get(from).map_or((self.leaves.len(), self.decos.len()), |t| (t.leaf0, t.deco0));
        let mut y = if from == 0 { 0.0 } else { self.tops[from - 1].end_y + GAP };
        self.tops.truncate(from);
        self.leaves.truncate(leaf0);
        self.decos.truncate(deco0);
        self.fx.retain(|k, _| *k < leaf0);
        if self.selection.is_some_and(|s| s.anchor.leaf >= leaf0 || s.focus.leaf >= leaf0) {
            self.selection = None;
        }
        for (i, b) in blocks.iter().enumerate().skip(from) {
            if i > 0 && matches!(b, Block::Heading { .. }) {
                y += HEADING_TOP;
            }
            self.tops.push(Top { src: b.clone(), end_y: 0.0, leaf0: self.leaves.len(), deco0: self.decos.len() });
            self.lay_block(cv, b, 0.0, self.width, &mut y, Role::Text)?;
            self.tops[i].end_y = y;
            y += GAP;
        }
        self.height = self.tops.last().map_or(0.0, |t| t.end_y);
        Ok(())
    }

    fn leaf(&mut self, cv: &Canvas, rt: RichText, x: f32, w: f32, y: &mut f32, role: Role, code: bool) -> Result<usize> {
        let l = RichLayout::new(cv, &rt, w.max(20.0))?;
        self.built += l.rebuilds();
        let h = l.height();
        self.leaves.push(Leaf { layout: l, pos: (x, *y), role, code });
        *y += h;
        Ok(self.leaves.len() - 1)
    }

    fn lay_blocks(&mut self, cv: &Canvas, bs: &[Block], x: f32, w: f32, y: &mut f32, role: Role) -> Result<()> {
        for (i, b) in bs.iter().enumerate() {
            if i > 0 {
                *y += GAP;
            }
            self.lay_block(cv, b, x, w, y, role)?;
        }
        Ok(())
    }

    fn lay_block(&mut self, cv: &Canvas, b: &Block, x: f32, w: f32, y: &mut f32, role: Role) -> Result<()> {
        let th = self.theme;
        match b {
            Block::Paragraph(c) => {
                self.leaf(cv, rich(c, BODY, 400, &th), x, w, y, role, false)?;
            }
            Block::Heading { level, content } => {
                let size = [20.0, 16.0, 14.0, BODY, BODY, BODY][(*level as usize).clamp(1, 6) - 1];
                self.leaf(cv, rich(content, size, 600, &th), x, w, y, role, false)?;
            }
            Block::List { ordered, start, items } => {
                let gutter = if *ordered { NUMBER_GUTTER } else { BULLET_GUTTER };
                for (i, it) in items.iter().enumerate() {
                    self.lay_item(cv, it, *ordered, *start as usize + i, x, w, gutter, y, role)?;
                    if i + 1 < items.len() {
                        *y += ITEM_GAP;
                    }
                }
            }
            Block::Quote(bs) => {
                let y0 = *y;
                self.lay_blocks(cv, bs, x + QUOTE_INDENT, w - QUOTE_INDENT, y, Role::Dim)?;
                self.decos.push(Deco::Bar(Rect::new(x, y0, 3.0, (*y - y0).max(0.0))));
            }
            Block::Rule => {
                self.decos.push(Deco::Rule(Rect::new(x, *y + 4.0, w, 1.0)));
                *y += 9.0;
            }
            Block::Code { lang, text } => {
                let y0 = *y;
                let mut rt = RichText::new().with_base(Style::new(12.0, 400)).with_line_height(18.0);
                rt.push_mut(text, SpanStyle { mono: true, size: Some(12.0), ..Default::default() });
                let mut ty = y0 + CODE_HEAD;
                let leaf = self.leaf(cv, rt, x + CODE_PAD, w - 2.0 * CODE_PAD, &mut ty, Role::Text, true)?;
                let h = ty - y0 + CODE_PAD;
                let lang = if lang.is_empty() {
                    None
                } else {
                    let mut l = RichText::new().with_base(Style::new(11.0, 500)).with_line_height(16.0);
                    l.push_mut(lang, SpanStyle { color: Some(th.dim), ..Default::default() });
                    let lay = RichLayout::new(cv, &l, w * 0.5)?;
                    self.built += lay.rebuilds();
                    Some((lay, (x + CODE_PAD, y0 + (CODE_HEAD - 16.0) / 2.0)))
                };
                let btn = Rect::new(x + w - 8.0 - self.btn_w, y0 + (CODE_HEAD - BTN_H) / 2.0, self.btn_w, BTN_H);
                self.decos.push(Deco::Frame { leaf, rect: Rect::new(x, y0, w, h), btn, lang });
                self.fx.insert(leaf, BtnFx::new());
                *y = y0 + h;
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn lay_item(&mut self, cv: &Canvas, it: &ListItem, ordered: bool, number: usize, x: f32, w: f32, gutter: f32, y: &mut f32, role: Role) -> Result<()> {
        let th = self.theme;
        let y0 = *y;
        let lh = (BODY * 1.45).round();
        if ordered {
            let mut m = RichText::new().with_base(Style::new(BODY, 400)).with_line_height(lh);
            m.push_mut(&format!("{number}."), SpanStyle { color: Some(th.dim), ..Default::default() });
            let lay = RichLayout::new(cv, &m, gutter)?;
            self.built += lay.rebuilds();
            let mw = lay.size().0;
            self.decos.push(Deco::Marker { layout: lay, pos: (x + gutter - 6.0 - mw, y0) });
        } else {
            self.decos.push(Deco::Bullet((x + 7.0, y0 + lh / 2.0)));
        }
        self.leaf(cv, rich(&it.content, BODY, 400, &th), x + gutter, w - gutter, y, role, false)?;
        if !it.children.is_empty() {
            *y += ITEM_GAP;
            self.lay_blocks(cv, &it.children, x + gutter, w - gutter, y, role)?;
        }
        Ok(())
    }

    // ------------------------------------------------------------ hit testing

    /// Rectangles of the copy buttons, with the leaf index of their code block (view coordinates).
    pub fn copy_button_rects(&self) -> Vec<(usize, Rect)> {
        self.decos
            .iter()
            .filter_map(|d| match d {
                Deco::Frame { leaf, btn, .. } => Some((*leaf, *btn)),
                _ => None,
            })
            .collect()
    }

    /// The frame (header + text) of every code block with its leaf index.
    pub fn code_frames(&self) -> Vec<(usize, Rect)> {
        self.decos
            .iter()
            .filter_map(|d| match d {
                Deco::Frame { leaf, rect, .. } => Some((*leaf, *rect)),
                _ => None,
            })
            .collect()
    }

    /// Bounds of a leaf's text (view coordinates).
    pub fn leaf_rect(&self, leaf: usize) -> Option<Rect> {
        self.leaves.get(leaf).map(|l| l.layout.rect(l.pos))
    }

    /// The text of a leaf (the code of a code block).
    pub fn leaf_text(&self, leaf: usize) -> String {
        self.leaves.get(leaf).map(|l| l.layout.text()).unwrap_or_default()
    }

    pub fn leaf_layout(&self, leaf: usize) -> Option<&RichLayout> {
        self.leaves.get(leaf).map(|l| &l.layout)
    }

    /// What is at `p` (view coordinates).
    pub fn hit(&self, p: Pt) -> Hit {
        if let Some((leaf, _)) = self.copy_button_rects().into_iter().find(|(_, r)| r.contains(p.0, p.1)) {
            return Hit::CopyButton(leaf);
        }
        for (i, l) in self.leaves.iter().enumerate() {
            if l.layout.rect(l.pos).contains(p.0, p.1) {
                let local = (p.0 - l.pos.0, p.1 - l.pos.1);
                if let Some(u) = l.layout.link_at(local) {
                    return Hit::Link(u.to_string());
                }
                return Hit::Text(TextPos { leaf: i, index: l.layout.index_at(local) });
            }
        }
        Hit::None
    }

    /// The nearest text position to `p`, always (for dragging a selection past the text).
    pub fn text_pos_at(&self, p: Pt) -> Option<TextPos> {
        if self.leaves.is_empty() {
            return None;
        }
        let i = self.leaves.partition_point(|l| l.pos.1 <= p.1).saturating_sub(1);
        let l = &self.leaves[i];
        Some(TextPos { leaf: i, index: l.layout.index_at((p.0 - l.pos.0, p.1 - l.pos.1)) })
    }

    // ------------------------------------------------------------ selection

    pub fn set_selection(&mut self, s: Option<Selection>) {
        self.selection = s.filter(|s| s.anchor.leaf < self.leaves.len() && s.focus.leaf < self.leaves.len());
    }
    pub fn selection(&self) -> Option<Selection> {
        self.selection
    }
    pub fn select_all(&mut self) {
        let Some(last) = self.leaves.last() else { return };
        self.selection = Some(Selection { anchor: TextPos { leaf: 0, index: 0 }, focus: TextPos { leaf: self.leaves.len() - 1, index: last.layout.len() } });
    }

    /// UTF-16 range of leaf `i` covered by `sel` (ordered).
    fn leaf_range(&self, i: usize, a: TextPos, b: TextPos) -> Option<Range<usize>> {
        if i < a.leaf || i > b.leaf {
            return None;
        }
        let len = self.leaves[i].layout.len();
        Some(if i == a.leaf { a.index.min(len) } else { 0 }..if i == b.leaf { b.index.min(len) } else { len })
    }

    /// The selected text for Ctrl+C: leaves joined by a newline.
    pub fn selection_text(&self) -> String {
        let Some(s) = self.selection.filter(|s| !s.is_empty()) else { return String::new() };
        let (a, b) = s.ordered();
        (a.leaf..=b.leaf.min(self.leaves.len().saturating_sub(1)))
            .filter_map(|i| self.leaf_range(i, a, b).map(|r| self.leaves[i].layout.text_range(r)))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The whole answer as plain text.
    pub fn plain_text(&self) -> String {
        crate::markdown::blocks_plain(&self.tops.iter().map(|t| t.src.clone()).collect::<Vec<_>>())
    }

    // ------------------------------------------------------------ animation

    /// Tell the view what the pointer is over so the copy buttons ease their hover. True when something changed.
    pub fn set_hover(&mut self, hit: &Hit) -> bool {
        let mut changed = false;
        for (leaf, fx) in self.fx.iter_mut() {
            let on = if *hit == Hit::CopyButton(*leaf) { 1.0 } else { 0.0 };
            if fx.hover.target() != on {
                fx.hover.retarget(on, if on > 0.0 { dur::FAST } else { dur::MED }, curves::EASE_OUT);
                changed = true;
            }
        }
        changed
    }

    /// Show the "copied" state on a code block's button; it fades back by itself after 1.6 s.
    pub fn mark_copied(&mut self, leaf: usize) {
        if let Some(fx) = self.fx.get_mut(&leaf) {
            fx.copied.retarget(1.0, dur::MED, curves::EASE_OUT);
            fx.hold = 0.0;
        }
    }

    /// Advance the animations by `dt` seconds; true while something is moving (draw again).
    pub fn step(&mut self, dt: f32) -> bool {
        let mut busy = false;
        for fx in self.fx.values_mut() {
            busy |= fx.hover.step(dt);
            busy |= fx.copied.step(dt);
            if fx.copied.target() > 0.5 && fx.copied.finished() {
                fx.hold += dt;
                if fx.hold >= COPIED_HOLD {
                    fx.copied.retarget(0.0, dur::MED, curves::EASE_OUT);
                }
                busy = true;
            }
        }
        busy
    }

    /// Eased "copied" amount of a button (0..1), for hosts that draw their own.
    pub fn copied_amount(&self, leaf: usize) -> f32 {
        self.fx.get(&leaf).map_or(0.0, |f| f.copied.value())
    }

    // ------------------------------------------------------------ drawing

    pub fn draw(&self, cv: &Canvas, origin: Pt, th: &Theme) {
        let at = |r: Rect| r.offset(origin.0, origin.1);
        for d in &self.decos {
            match d {
                Deco::Bullet(c) => cv.fill_circle((origin.0 + c.0, origin.1 + c.1), 2.2, th.dim),
                Deco::Marker { layout, pos } => layout.draw(cv, (origin.0 + pos.0, origin.1 + pos.1), th.dim),
                Deco::Bar(r) => cv.fill_rr(at(*r), 1.5, th.dim.fade(0.45)),
                Deco::Rule(r) => cv.fill_rect(at(*r), th.border),
                Deco::Frame { leaf, rect, btn, lang } => {
                    cv.fill_rr(at(*rect), th.m.radius_sm, th.card);
                    cv.stroke_rr(at(*rect), th.m.radius_sm, 1.0, th.line);
                    if let Some((l, p)) = lang {
                        l.draw(cv, (origin.0 + p.0, origin.1 + p.1), th.dim);
                    }
                    self.draw_button(cv, at(*btn), *leaf, th);
                }
            }
        }
        if let Some(s) = self.selection.filter(|s| !s.is_empty()) {
            let (a, b) = s.ordered();
            for i in a.leaf..=b.leaf.min(self.leaves.len().saturating_sub(1)) {
                if let Some(r) = self.leaf_range(i, a, b) {
                    let l = &self.leaves[i];
                    l.layout.draw_selection(cv, (origin.0 + l.pos.0, origin.1 + l.pos.1), r, th.accent.alpha(0.35));
                }
            }
        }
        for l in &self.leaves {
            let c = if l.role == Role::Dim && !l.code { th.text2 } else { th.text };
            l.layout.draw(cv, (origin.0 + l.pos.0, origin.1 + l.pos.1), c);
        }
    }

    fn draw_button(&self, cv: &Canvas, r: Rect, leaf: usize, th: &Theme) {
        let (hv, cp) = self.fx.get(&leaf).map_or((0.0, 0.0), |f| (f.hover.value(), f.copied.value()));
        let bg = th.raised.mix(th.raised2, hv).mix(th.green_bg, cp * 0.8);
        cv.fill_rr(r, 7.0, bg);
        let ink = th.dim.mix(th.text, hv);
        let icon = Rect::new(r.x + 8.0, r.cy() - 6.0, 12.0, 12.0);
        glyph(cv, Glyph::Copy, icon, ink.fade(1.0 - cp));
        glyph(cv, Glyph::Check, icon, th.green.fade(cp));
        let label = Rect::new(r.x + 25.0, r.y, r.w - 25.0, r.h);
        let st = Style::new(11.0, 500);
        cv.text(label, &self.copy, st, ink.fade(1.0 - cp));
        cv.text(label, &self.copied, st, th.green.fade(cp));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::markdown::parse;
    use crate::testkit::Harness;

    const LABELS: MarkdownLabels = MarkdownLabels { copy: "Copy", copied: "Copied" };
    const DOC: &str = "# Title\n\nSome **bold** text with `code` and a [link](https://e.com/x) that is long enough to wrap onto several lines when narrow.\n\n- first\n- second\n  - nested\n\n1. one\n2. two\n\n> a quote\n\n---\n\n```rust\nfn main() {\n    println!(\"hi\");\n}\n```\n\nПривет, мир 😀";

    fn view(h: &Harness, md: &str, w: f32) -> MarkdownView {
        let cv = Canvas::new(&h.gfx, &h.res, 1.0);
        MarkdownView::new(&cv, parse(md), w, &Theme::pill_dark(), &LABELS).expect("view")
    }

    #[test]
    fn height_grows_with_content_and_shrinks_when_widened() {
        let h = Harness::new();
        let short = view(&h, "one paragraph", 300.0);
        let long = view(&h, DOC, 300.0);
        assert!(long.height() > short.height() * 5.0, "{} {}", long.height(), short.height());
        let cv = Canvas::new(&h.gfx, &h.res, 1.0);
        let mut v = view(&h, DOC, 200.0);
        let narrow = v.height();
        v.relayout(&cv, 500.0, &Theme::pill_dark()).unwrap();
        assert!(v.height() < narrow, "{} vs {narrow}", v.height());
        assert_eq!(view(&h, "", 300.0).height(), 0.0);
    }

    #[test]
    fn layout_is_a_vertical_stack_without_overlap() {
        let h = Harness::new();
        let v = view(&h, DOC, 320.0);
        let mut last_bottom = -1.0f32;
        let mut quote_seen = false;
        for i in 0..v.leaf_count() {
            let r = v.leaf_rect(i).unwrap();
            if r.x > 100.0 {
                continue;
            }
            // leaves of the same column never overlap vertically and read top to bottom
            if r.x < 5.0 || r.x > 10.0 {
                assert!(r.y >= last_bottom - 0.5 || quote_seen, "leaf {i} {r:?} after {last_bottom}");
            }
            last_bottom = last_bottom.max(r.bottom());
            quote_seen |= r.x > 10.0;
        }
        assert!(last_bottom <= v.height() + 0.5);
        assert!(v.leaf_count() >= 9, "{}", v.leaf_count());
    }

    #[test]
    fn code_block_frame_button_and_pixels() {
        let h = Harness::new();
        let th = Theme::pill_dark();
        let mut v = view(&h, "text\n\n```rust\nlet x = 1;\n```", 300.0);
        let frames = v.code_frames();
        let btns = v.copy_button_rects();
        assert_eq!((frames.len(), btns.len()), (1, 1));
        let (leaf, frame) = frames[0];
        let (bl, btn) = btns[0];
        assert_eq!(leaf, bl);
        assert!(frame.x <= btn.x && frame.right() >= btn.right() && frame.y <= btn.y && frame.bottom() >= btn.bottom(), "{frame:?} {btn:?}");
        assert_eq!(v.leaf_text(leaf), "let x = 1;");
        assert_eq!(v.hit((btn.cx(), btn.cy())), Hit::CopyButton(leaf));
        assert!(matches!(v.hit((frame.x + 14.0, frame.y + CODE_HEAD + 4.0)), Hit::Text(TextPos { leaf: l, .. }) if l == leaf));
        assert_eq!(v.hit((frame.x + 2.0, frame.y + 4.0)), Hit::None, "header outside the button");
        let img = h.render(320, 120, 1.0, th.bg, |cv| v.draw(cv, (10.0, 10.0), &th));
        let at = img.at(10.0 + frame.right() - 30.0, 10.0 + frame.bottom() - 5.0);
        assert_eq!(&at[..3], &[0x0f, 0x0f, 0x0f], "the frame is filled with the card colour: {at:?}");
        // hover and copied animate: the button colour moves
        let before = img.at(10.0 + btn.x + 3.0, 10.0 + btn.cy());
        crate::ease::force_reduced(Some(false));
        assert!(v.set_hover(&Hit::CopyButton(leaf)));
        assert!(v.step(0.06));
        assert!(!v.set_hover(&Hit::CopyButton(leaf)), "same hover again changes nothing");
        while v.step(0.016) {}
        let hovered = h.render(320, 120, 1.0, th.bg, |cv| v.draw(cv, (10.0, 10.0), &th)).at(10.0 + btn.x + 3.0, 10.0 + btn.cy());
        assert_ne!(before, hovered, "hover paints a lighter button");
        v.mark_copied(leaf);
        assert!(v.step(0.05) && v.copied_amount(leaf) > 0.0);
        let mut t = 0.0;
        while v.copied_amount(leaf) < 1.0 {
            v.step(0.016);
            t += 0.016;
            assert!(t < 1.0);
        }
        // after the hold time it fades back by itself
        let mut guard = 0;
        while v.step(0.05) {
            guard += 1;
            assert!(guard < 100);
        }
        assert_eq!(v.copied_amount(leaf), 0.0);
        crate::ease::force_reduced(None);
    }

    #[test]
    fn links_hits_and_unsafe_links() {
        let h = Harness::new();
        let v = view(&h, "go [here](https://e.com/x) or [bad](javascript:alert(1))", 400.0);
        let lay = v.leaf_layout(0).unwrap();
        let i = lay.text().find("here").unwrap();
        let c = lay.caret_rect(i);
        assert_eq!(v.hit((c.x + 3.0, c.y + 9.0)), Hit::Link("https://e.com/x".into()));
        let j = lay.text().find("bad").unwrap();
        let c = lay.caret_rect(j);
        assert!(matches!(v.hit((c.x + 3.0, c.y + 9.0)), Hit::Text(_)), "an unsafe link is only text");
        assert_eq!(v.hit((-5.0, -5.0)), Hit::None);
    }

    #[test]
    fn selection_across_blocks_and_text() {
        let h = Harness::new();
        let mut v = view(&h, "alpha beta\n\n- gamma\n- delta\n\n```\ncode here\n```", 300.0);
        assert_eq!(v.leaf_count(), 4);
        let a = TextPos { leaf: 0, index: 6 };
        let b = TextPos { leaf: 2, index: 3 };
        v.set_selection(Some(Selection { anchor: b, focus: a }));
        assert_eq!(v.selection_text(), "beta\ngamma\ndel", "either direction, leaves joined with a newline");
        v.select_all();
        assert_eq!(v.selection_text(), "alpha beta\ngamma\ndelta\ncode here");
        v.set_selection(None);
        assert_eq!(v.selection_text(), "");
        // text_pos_at clamps below and above everything
        let end = v.text_pos_at((9999.0, 9999.0)).unwrap();
        assert_eq!(end.leaf, 3);
        assert_eq!(end.index, "code here".len());
        assert_eq!(v.text_pos_at((-9.0, -9.0)).unwrap(), TextPos { leaf: 0, index: 0 });
        assert!(v.plain_text().contains("alpha beta"));
        // selection pixels are painted
        v.set_selection(Some(Selection { anchor: a, focus: b }));
        let th = Theme::pill_dark();
        let with = h.render(320, 140, 1.0, th.bg, |cv| v.draw(cv, (0.0, 0.0), &th));
        v.set_selection(None);
        let without = h.render(320, 140, 1.0, th.bg, |cv| v.draw(cv, (0.0, 0.0), &th));
        assert!(with.diff(&without, 8) > 100);
    }

    #[test]
    fn streaming_update_reuses_the_finished_blocks() {
        let h = Harness::new();
        let cv = Canvas::new(&h.gfx, &h.res, 1.0);
        let th = Theme::pill_dark();
        let mut v = MarkdownView::new(&cv, parse("# Head\n\nfirst paragraph\n\nsecond"), 300.0, &th, &LABELS).unwrap();
        let built = v.rebuilds();
        // identical -> nothing
        assert_eq!(v.update(&cv, parse("# Head\n\nfirst paragraph\n\nsecond"), &th).unwrap(), 0);
        // the tail grows token by token: exactly the one tail block is laid out again
        let mut text = String::from("# Head\n\nfirst paragraph\n\nsecond");
        for tok in [" tok", "en", " and more"] {
            text.push_str(tok);
            assert_eq!(v.update(&cv, parse(&text), &th).unwrap(), 1, "only the changed tail is built");
        }
        assert!(v.rebuilds() - built == 3);
        assert_eq!(v.leaf_text(2), "second token and more");
        // a new block starts: the old tail is complete and stays; one layout for the new block
        text.push_str("\n\n- item");
        assert_eq!(v.update(&cv, parse(&text), &th).unwrap(), 1, "the paragraph (now final) is identical and stays, the new list item is one layout");
        // same result as a fresh layout
        let fresh = MarkdownView::new(&cv, parse(&text), 300.0, &th, &LABELS).unwrap();
        assert_eq!(fresh.height(), v.height());
        assert_eq!(fresh.leaf_count(), v.leaf_count());
        // a theme change lays everything out again
        let n = v.update(&cv, parse(&text), &Theme::pill_light()).unwrap();
        assert!(n >= 4);
        // shrinking (a different answer) works too
        v.update(&cv, parse("short"), &Theme::pill_light()).unwrap();
        assert_eq!(v.leaf_count(), 1);
        assert_eq!(v.selection(), None);
    }

    #[test]
    fn renders_a_mixed_document_to_png_at_both_scales_dark_and_light() {
        let h = Harness::new();
        for (name, th) in [("dark", Theme::pill_dark()), ("light", Theme::pill_light())] {
            for scale in [1.0f32, 2.0] {
                let cv = Canvas::new(&h.gfx, &h.res, scale);
                let v = MarkdownView::new(&cv, parse(DOC), 360.0, &th, &LABELS).unwrap();
                let img = h.render(392, v.height().ceil() as u32 + 32, scale, th.bg, |cv| v.draw(cv, (16.0, 16.0), &th));
                assert!(img.count(None, |p| p[0] != img.px(0, 0)[0] || p[1] != img.px(0, 0)[1]) > 500, "{name} {scale}: something was drawn");
                let dir = std::path::Path::new(&std::env::var("CARGO_TARGET_DIR").unwrap_or_else(|_| "target".into())).join("kit-images");
                if let Err(e) = img.save_png(&h, dir.join(format!("richtext_markdown_{name}@{scale}x.png"))) {
                    eprintln!("warning: could not write the markdown PNG: {e}");
                }
            }
        }
    }

    #[test]
    fn headings_scale_and_lists_indent() {
        let h = Harness::new();
        let v = view(&h, "# One\n\n###### Six\n\n- a\n  - b", 300.0);
        let h1 = v.leaf_rect(0).unwrap().h;
        let h6 = v.leaf_rect(1).unwrap().h;
        assert!(h1 > h6, "{h1} {h6}");
        let a = v.leaf_rect(2).unwrap();
        let b = v.leaf_rect(3).unwrap();
        assert_eq!(a.x, BULLET_GUTTER);
        assert_eq!(b.x, 2.0 * BULLET_GUTTER, "nested items indent by one gutter");
        assert!(b.y > a.bottom());
    }
}
