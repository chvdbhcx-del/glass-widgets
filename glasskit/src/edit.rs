//! Text editing: [`TextInput`], a single- or multi-line text field with the behaviour of a native edit control, drawn with Direct2D in the look of
//! Pill's `.field` (`theme.css`).
//!
//! * [`model::EditModel`]  the text, caret, selection, word / cluster movement and the undo history. Pure and unit-tested.
//! * [`grapheme`]          cluster boundaries (combining marks, ZWJ emoji, flags, Hangul): the caret never lands inside a character.
//! * [`clip`]              the [`Clipboard`] trait, the Win32 [`WinClipboard`] and an in-memory [`FakeClipboard`].
//! * [`TextInput`]         the widget: DirectWrite layout, hit testing, mouse selection, scrolling, caret blink, focus animation, placeholder,
//!                         password bullets, events as a queue.
//!
//! ```ignore
//! let theme = Theme::pill_dark();
//! let mut input = TextInput::new(false, &theme);          // single line
//! input.placeholder = "Search".into();                    // strings come from the caller
//! input.model.set_max_len(Some(200));
//! // per input event (rect = the field's border box in window logical px; any Canvas on the same Gfx/Res will do for measuring):
//! let cv = Canvas::new(&gfx, &res, scale);
//! let r = input.event(&ev, rect, &cv);                    // r.focus / r.capture / r.cursor: see input::Response
//! for e in input.take_events() {
//!     match e { EditEvent::Changed => { /* input.text() */ } EditEvent::Submit => {} _ => {} }
//! }
//! // per frame while `input.busy()` (and at `input.blink_wake()`):
//! input.step(dt);
//! // per paint:
//! input.draw(&cv, rect, &theme);
//! ```
//!
//! # Keyboard map (while focused)
//! | keys | action |
//! |------|--------|
//! | Left / Right (Ctrl: by word, Shift: extend) | move the caret by cluster; a selection collapses to its edge |
//! | Up / Down | multi-line: by visual line keeping the column; single line: start / end |
//! | Home / End (Ctrl: document) | start / end of the visual line (single line: of the text) |
//! | PageUp / PageDown | multi-line: a window of lines |
//! | Backspace / Delete (Ctrl: by word) | delete a cluster / word / the selection; Shift+Delete cuts |
//! | Enter | single line: `Submit`; multi-line: newline, Ctrl+Enter: `Submit` |
//! | Esc | collapse a selection, else `Cancel` |
//! | Ctrl+A / C / X / V | select all / copy / cut / paste (Ctrl+Insert copies, Shift+Insert pastes) |
//! | Ctrl+Z, Ctrl+Y or Ctrl+Shift+Z | undo / redo |
//! | Tab | not used: left to the focus chain |
//!
//! # Mouse
//! Click places the caret, Shift+click extends, drag selects (with capture), double-click selects a word and dragging then extends by words,
//! triple-click selects the line (single line: everything), right-click emits `ContextMenu` for the host to show a menu. The wheel scrolls
//! a multi-line field (and is NOT consumed when the field is already at that end, so a parent can take it).
//!
//! # Not done yet
//! IME composition: `InputEvent::Ime` is accepted and ignored, the composition string is not drawn (the `Char` path, which Windows also uses to
//! deliver finished composition text, is complete). Bidirectional text is laid out by DirectWrite but the caret keyboard movement is logical.
pub mod clip;
pub mod grapheme;
pub mod model;

use crate::canvas::Canvas;
use crate::ease::{self, curves, dur, Tween};
use crate::geom::{Pt, Rect};
use crate::input::{Cursor, InputEvent, KeyCode, Modifiers, MouseButton, Response};
use crate::scroll::{ScrollState, Scrollbar};
use crate::text::{Style, VAlign};
use crate::theme::Theme;
use windows::Win32::Foundation::BOOL;
use windows::Win32::Graphics::DirectWrite::*;

pub use clip::{Clipboard, FakeClipboard, WinClipboard};
pub use model::{EditKind, EditModel};

/// Caret blink half period (the Windows default).
pub const BLINK_HALF: f32 = 0.53;
const BLINK_PERIOD: f32 = BLINK_HALF * 2.0;
/// layout box of single-line text: wide enough that nothing wraps or trims
const WIDE: f32 = 100_000.0;

/// What happened to the field; drain with [`TextInput::take_events`].
#[derive(Clone, Debug, PartialEq)]
pub enum EditEvent {
    /// the text changed by the user (typing, paste, cut, delete, undo / redo); at most one per input event
    Changed,
    /// Enter (single line) or Ctrl+Enter (multi-line)
    Submit,
    /// Esc with nothing to collapse
    Cancel,
    FocusChanged(bool),
    /// right click at this window position: show a context menu (`menu.rs`) with cut / copy / paste
    ContextMenu(Pt),
}

enum Drag {
    Chars,
    Words(usize, usize),
    Lines(usize, usize),
}

/// The text as DirectWrite sees it: a UTF-16 string plus where each model char (cluster, for a password) starts in it.
struct Disp {
    s: String,
    u16_at: Vec<u32>,
}

impl Disp {
    fn new(chars: &[char], password: bool) -> Disp {
        let n = chars.len();
        let mut u = Vec::with_capacity(n + 1);
        if password {
            let (mut s, mut cl, mut i) = (String::new(), 0u32, 0);
            while i < n {
                let j = grapheme::next_boundary(chars, i);
                for _ in i..j {
                    u.push(cl);
                }
                s.push('\u{2022}');
                cl += 1;
                i = j;
            }
            u.push(cl);
            Disp { s, u16_at: u }
        } else {
            let mut at = 0u32;
            for &c in chars {
                u.push(at);
                at += c.len_utf16() as u32;
            }
            u.push(at);
            Disp { s: chars.iter().collect(), u16_at: u }
        }
    }
    fn at(&self, i: usize) -> u32 {
        self.u16_at[i.min(self.u16_at.len() - 1)]
    }
    fn char_of(&self, p: u32) -> usize {
        self.u16_at.partition_point(|&u| u < p).min(self.u16_at.len() - 1)
    }
}

struct Lay {
    lay: IDWriteTextLayout,
    disp: Disp,
    /// layout extent
    w: f32,
    h: f32,
    /// height of one text line (the whole layout height for single-line text)
    line_h: f32,
    style: Style,
    box_w: f32,
    box_h: f32,
}

pub struct TextInput {
    /// the text, caret, selection and undo history. Edit it through the widget's methods when the change should raise `Changed` and reveal
    /// the caret; reading it directly is always fine.
    pub model: EditModel,
    /// shown while the text is empty (the caller's string)
    pub placeholder: String,
    /// draw bullets instead of the text; copy and cut are refused
    pub password: bool,
    /// the text can be selected and copied but not changed
    pub read_only: bool,
    /// a disabled field ignores input and is drawn faded
    pub enabled: bool,
    pub style: Style,
    /// text padding (x, y); y matters for multi-line fields (single-line text is centred)
    pub pad: (f32, f32),
    pub radius: f32,
    /// test hook: `Some(b)` overrides the system "reduce motion" setting
    pub reduced: Option<bool>,
    clipboard: Box<dyn Clipboard>,
    focused: bool,
    window_focus: bool,
    focus_t: Tween<f32>,
    ph_t: Tween<f32>,
    blink: f32,
    sx: ScrollState,
    sy: ScrollState,
    bar: Scrollbar,
    want_x: Option<f32>,
    drag: Option<Drag>,
    reveal: bool,
    events: Vec<EditEvent>,
}

impl TextInput {
    /// `multiline`: wraps to the width, Enter makes a newline, scrolls vertically. Metrics and type come from `theme`.
    pub fn new(multiline: bool, theme: &Theme) -> TextInput {
        let m = &theme.m;
        TextInput {
            model: EditModel::new(multiline, ""),
            placeholder: String::new(),
            password: false,
            read_only: false,
            enabled: true,
            style: theme.body(),
            pad: (m.field_pad_x, if multiline { 10.0 } else { 0.0 }),
            radius: m.field_r,
            reduced: None,
            clipboard: Box::new(WinClipboard),
            focused: false,
            window_focus: true,
            focus_t: Tween::at_rest(0.0),
            ph_t: Tween::at_rest(1.0),
            blink: 0.0,
            sx: ScrollState::new(),
            sy: ScrollState::new(),
            bar: Scrollbar::new(crate::scroll::Axis::Vertical),
            want_x: None,
            drag: None,
            reveal: false,
            events: Vec::new(),
        }
    }

    /// Use another clipboard (a [`FakeClipboard`] in tests).
    pub fn with_clipboard(mut self, c: Box<dyn Clipboard>) -> TextInput {
        self.clipboard = c;
        self
    }
    pub fn set_clipboard(&mut self, c: Box<dyn Clipboard>) {
        self.clipboard = c;
    }

    fn multiline(&self) -> bool {
        self.model.multiline
    }
    fn off(&self) -> bool {
        self.reduced.unwrap_or_else(ease::reduced)
    }

    // ------------------------------------------------------------ plain accessors

    pub fn text(&self) -> String {
        self.model.text()
    }
    /// Replace the text as a reset: no undo step, no `Changed` event, the caret goes to the end and into view.
    pub fn set_text(&mut self, s: &str) {
        self.model.set_text(s);
        self.after_programmatic();
    }
    /// Insert at the caret (replacing the selection) as one undo step; raises `Changed`.
    pub fn insert(&mut self, s: &str) -> bool {
        let ok = self.model.insert_text(s);
        self.edited(ok);
        ok
    }
    /// `(anchor, caret)` in chars.
    pub fn selection(&self) -> (usize, usize) {
        (self.model.anchor(), self.model.caret())
    }
    pub fn set_selection(&mut self, anchor: usize, caret: usize) {
        self.model.set_selection(anchor, caret);
        self.after_programmatic();
    }
    pub fn select_all(&mut self) {
        self.model.select_all();
        self.after_programmatic();
    }
    pub fn is_focused(&self) -> bool {
        self.focused
    }
    pub fn take_events(&mut self) -> Vec<EditEvent> {
        std::mem::take(&mut self.events)
    }

    fn after_programmatic(&mut self) {
        self.blink = 0.0;
        self.want_x = None;
        self.reveal = true;
        let empty = self.model.is_empty();
        self.ph_target(empty);
    }

    fn edited(&mut self, changed: bool) {
        if changed {
            if self.events.last() != Some(&EditEvent::Changed) {
                self.events.push(EditEvent::Changed);
            }
            self.after_programmatic();
        }
    }

    fn ph_target(&mut self, empty: bool) {
        let to = if empty { 1.0 } else { 0.0 };
        if self.off() {
            self.ph_t.jump(to);
        } else {
            self.ph_t.retarget(to, dur::FAST, curves::EASE_OUT);
        }
    }

    /// Give or take the keyboard focus (a `FocusChain` calls this). Emits `FocusChanged`, animates the ring.
    pub fn set_focused(&mut self, on: bool) {
        if on == self.focused {
            return;
        }
        self.focused = on;
        self.blink = 0.0;
        self.events.push(EditEvent::FocusChanged(on));
        if self.off() {
            self.focus_t.jump(if on { 1.0 } else { 0.0 });
        } else {
            self.focus_t.retarget(if on { 1.0 } else { 0.0 }, dur::FAST, curves::EASE_OUT);
        }
        if !on {
            self.drag = None;
        } else {
            self.reveal = true;
        }
    }

    // ------------------------------------------------------------ animation

    /// Advance the focus / placeholder / scroll animations by `dt` seconds and the caret blink clock. True while frames are needed.
    pub fn step(&mut self, dt: f32) -> bool {
        self.blink = (self.blink + dt) % BLINK_PERIOD;
        let mut busy = self.focus_t.step(dt);
        busy |= self.ph_t.step(dt);
        self.sx.reduced = self.reduced;
        self.sy.reduced = self.reduced;
        self.bar.reduced = self.reduced;
        busy |= self.sx.step(dt);
        busy |= self.sy.step(dt);
        busy |= self.bar.step(dt);
        busy
    }

    /// Is an animation running (the host should keep ticking)? The blink alone is not: see [`TextInput::blink_wake`].
    pub fn busy(&self) -> bool {
        !self.focus_t.finished() || !self.ph_t.finished() || self.sx.offset() != self.sx.target() || self.sy.offset() != self.sy.target()
    }

    /// Seconds until the caret next toggles, when it is blinking (focused, motion enabled): the host sets a timer for this and repaints,
    /// instead of ticking at 60 Hz for a caret.
    pub fn blink_wake(&self) -> Option<f32> {
        (self.focused && self.window_focus && !self.off()).then(|| BLINK_HALF - (self.blink % BLINK_HALF))
    }

    /// Is the caret in its visible half period? (Always, under reduced motion.)
    pub fn caret_visible(&self) -> bool {
        self.off() || self.blink < BLINK_HALF
    }

    /// Set the blink clock (seconds since the last edit), for previews and tests.
    pub fn set_blink_phase(&mut self, secs: f32) {
        self.blink = secs.rem_euclid(BLINK_PERIOD);
    }

    // ------------------------------------------------------------ layout

    fn inner(&self, rect: Rect) -> Rect {
        rect.inset_xy(self.pad.0, self.pad.1)
    }

    fn lay(&self, rect: Rect, cv: &Canvas) -> Option<Lay> {
        let disp = Disp::new(self.model.chars(), self.password);
        let inner = self.inner(rect);
        let (style, box_w, box_h) = if self.multiline() {
            (self.style.wrap(), inner.w.max(1.0), WIDE)
        } else {
            (Style { valign: VAlign::Top, wrap: false, ellipsis: false, ..self.style }, WIDE, 200.0)
        };
        let lay = cv.layout(&disp.s, style, box_w, box_h).ok()?;
        let mut m = DWRITE_TEXT_METRICS::default();
        unsafe { lay.GetMetrics(&mut m).ok()? };
        let fallback = (style.size * 1.35).round();
        let line_h = if self.multiline() { (style.size * 1.32).round() } else if m.height > 1.0 { m.height } else { fallback };
        Some(Lay { lay, disp, w: m.widthIncludingTrailingWhitespace.max(m.width), h: m.height.max(line_h), line_h, style, box_w, box_h })
    }

    /// Window position of the layout origin (current scroll applied).
    fn origin(&self, rect: Rect, l: &Lay) -> Pt {
        if self.multiline() {
            (rect.x + self.pad.0, rect.y + self.pad.1 - self.sy.offset())
        } else {
            (rect.x + self.pad.0 - self.sx.offset(), rect.y + ((rect.h - l.line_h) / 2.0).round())
        }
    }

    /// Caret position in layout coordinates: (x, y top, height).
    fn caret_local(&self, l: &Lay, i: usize) -> (f32, f32, f32) {
        let n = self.model.len();
        let (mut x, mut y, mut m) = (0f32, 0f32, DWRITE_HIT_TEST_METRICS::default());
        let chars = self.model.chars();
        if self.multiline() && i >= n && n > 0 && chars[n - 1] == '\n' {
            // after a final newline the caret sits at the start of the (empty) next line
            let ls = self.lines(l);
            let all: f32 = ls.iter().map(|ln| ln.height).sum();
            let y = match ls.last() {
                Some(last) if last.length == 0 => all - last.height,
                _ => all,
            };
            return (0.0, y, l.line_h);
        }
        let ok = unsafe { l.lay.HitTestTextPosition(l.disp.at(i), BOOL(0), &mut x, &mut y, &mut m) }.is_ok();
        if !ok {
            return (0.0, 0.0, l.line_h);
        }
        (x, y, if m.height > 1.0 { m.height } else { l.line_h })
    }

    fn lines(&self, l: &Lay) -> Vec<DWRITE_LINE_METRICS> {
        let mut n = 0u32;
        unsafe {
            let _ = l.lay.GetLineMetrics(None, &mut n);
            let mut v = vec![DWRITE_LINE_METRICS::default(); n as usize];
            if n > 0 && l.lay.GetLineMetrics(Some(&mut v), &mut n).is_err() {
                v.clear();
            }
            v
        }
    }

    /// Caret index nearest to layout-local point (x, y).
    fn index_at_local(&self, l: &Lay, x: f32, y: f32) -> usize {
        let (mut trailing, mut inside) = (BOOL(0), BOOL(0));
        let mut m = DWRITE_HIT_TEST_METRICS::default();
        if unsafe { l.lay.HitTestPoint(x, y, &mut trailing, &mut inside, &mut m) }.is_err() {
            return 0;
        }
        let p = m.textPosition + if trailing.as_bool() { m.length } else { 0 };
        grapheme::snap(self.model.chars(), l.disp.char_of(p))
    }

    /// Caret index under the window point `p`.
    fn index_at(&self, rect: Rect, l: &Lay, p: Pt) -> usize {
        let o = self.origin(rect, l);
        self.index_at_local(l, p.0 - o.0, p.1 - o.1)
    }

    /// Window position of the caret at char index `i`: `(x, top, height)`. For an IME candidate window, tooltips, tests.
    pub fn caret_point(&self, rect: Rect, cv: &Canvas, i: usize) -> Option<(f32, f32, f32)> {
        let l = self.lay(rect, cv)?;
        let o = self.origin(rect, &l);
        let (x, y, h) = self.caret_local(&l, i);
        Some((o.0 + x, o.1 + y, h))
    }

    /// Height the field needs at `width` to show all its text without scrolling (multi-line: grows with the content, for composers).
    pub fn content_height(&self, width: f32, cv: &Canvas) -> f32 {
        let probe = Rect::new(0.0, 0.0, width, 0.0);
        match self.lay(probe, cv) {
            Some(l) if self.multiline() => l.h + 2.0 * self.pad.1,
            Some(l) => l.line_h + 2.0 * self.pad.1,
            None => 0.0,
        }
    }

    /// Visual-line bounds (char indices) of the line holding `i`: `(start, end)` where `end` is where End puts the caret.
    fn line_bounds(&self, l: &Lay, i: usize) -> (usize, usize) {
        let lines = self.lines(l);
        let total = *l.disp.u16_at.last().unwrap_or(&0);
        let p = l.disp.at(i);
        let mut start = 0u32;
        for (k, ln) in lines.iter().enumerate() {
            let end = start + ln.length;
            if p < end || k + 1 == lines.len() {
                let e16 = if ln.newlineLength > 0 {
                    end - ln.newlineLength
                } else if end >= total {
                    total
                } else {
                    end - ln.trailingWhitespaceLength.min(ln.length)
                };
                return (l.disp.char_of(start), l.disp.char_of(e16));
            }
            start = end;
        }
        (0, self.model.len())
    }

    fn selection_rects(&self, l: &Lay, a: usize, b: usize) -> Vec<Rect> {
        let (pa, pb) = (l.disp.at(a), l.disp.at(b));
        if pb <= pa {
            return Vec::new();
        }
        let mut buf = vec![DWRITE_HIT_TEST_METRICS::default(); 16];
        let mut n = 0u32;
        let r = unsafe { l.lay.HitTestTextRange(pa, pb - pa, 0.0, 0.0, Some(&mut buf), &mut n) };
        if r.is_err() && n as usize > buf.len() {
            buf = vec![DWRITE_HIT_TEST_METRICS::default(); n as usize];
            if unsafe { l.lay.HitTestTextRange(pa, pb - pa, 0.0, 0.0, Some(&mut buf), &mut n) }.is_err() {
                return Vec::new();
            }
        } else if r.is_err() {
            return Vec::new();
        }
        buf.iter().take(n as usize).map(|m| Rect::new(m.left, m.top, m.width, m.height)).collect()
    }

    /// Recompute scroll extents and bring a pending caret into view. Cheap; `event` calls it, call it before `draw` after changing the text,
    /// the rect or the theme from the outside.
    pub fn update(&mut self, rect: Rect, cv: &Canvas) {
        let Some(l) = self.lay(rect, cv) else { return };
        let inner = self.inner(rect);
        self.sx.reduced = self.reduced;
        self.sy.reduced = self.reduced;
        if self.multiline() {
            self.sy.set_metrics(l.h + 2.0 * self.pad.1, rect.h);
            self.sx.set_metrics(0.0, 0.0);
        } else {
            self.sx.set_metrics(l.w + 2.0, inner.w);
            self.sy.set_metrics(0.0, 0.0);
        }
        if !self.reveal {
            return;
        }
        self.reveal = false;
        let (cx, cy, ch) = self.caret_local(&l, self.model.caret());
        if self.multiline() {
            let (top, bottom) = (cy + self.pad.1, cy + ch + self.pad.1);
            let (off, vh) = (self.sy.target(), rect.h);
            if top < off + self.pad.1 {
                self.sy.scroll_to((top - self.pad.1).max(0.0), true);
            } else if bottom > off + vh - self.pad.1 {
                self.sy.scroll_to(bottom - vh + self.pad.1, true);
            }
        } else {
            let (off, vw) = (self.sx.target(), inner.w);
            let margin = (vw / 4.0).min(16.0);
            if cx < off + 0.5 {
                self.sx.scroll_to((cx - margin).max(0.0), true);
            } else if cx > off + vw - 1.5 {
                self.sx.scroll_to(cx - vw + margin, true);
            }
        }
    }

    // ------------------------------------------------------------ editing commands

    fn copy(&mut self) {
        if self.password {
            return;
        }
        let t = self.model.selected_text();
        if !t.is_empty() {
            self.clipboard.set_text(&t);
        }
    }

    fn cut(&mut self) {
        if self.password || self.read_only {
            return;
        }
        self.copy();
        let ok = self.model.delete_selection();
        self.edited(ok);
    }

    fn paste(&mut self) {
        if self.read_only {
            return;
        }
        if let Some(t) = self.clipboard.get_text() {
            let ok = self.model.insert_text(&t);
            self.edited(ok);
        }
    }

    fn moved(&mut self) {
        self.blink = 0.0;
        self.reveal = true;
    }

    /// Up / Down by `lines` visual lines keeping the column; off the first / last line the caret goes to the start / end.
    fn vertical(&mut self, lines: i32, extend: bool, rect: Rect, cv: &Canvas) {
        let Some(l) = self.lay(rect, cv) else { return };
        let (cx, cy, ch) = self.caret_local(&l, self.model.caret());
        let want = *self.want_x.get_or_insert(cx);
        let ty = cy + ch / 2.0 + lines as f32 * l.line_h.max(ch.min(l.line_h + 1.0));
        let total = self.lines(&l).iter().map(|ln| ln.height).sum::<f32>().max(l.h);
        let i = if ty < 0.0 {
            0
        } else if ty > total {
            self.model.len()
        } else {
            self.index_at_local(&l, want, ty)
        };
        self.model.move_to(i, extend);
        self.moved();
    }

    fn key(&mut self, code: KeyCode, mods: Modifiers, rect: Rect, cv: &Canvas) -> Response {
        let ctrl = mods.ctrl && !mods.alt; // AltGr arrives as Ctrl+Alt: that is text, not a shortcut
        let shift = mods.shift;
        let ml = self.multiline();
        let ro = self.read_only;
        let mut used = true;
        let keep_x = matches!(code, KeyCode::Up | KeyCode::Down | KeyCode::PageUp | KeyCode::PageDown);
        match code {
            KeyCode::Left => {
                self.model.move_left(shift, ctrl);
                self.moved();
            }
            KeyCode::Right => {
                self.model.move_right(shift, ctrl);
                self.moved();
            }
            KeyCode::Up if ml => self.vertical(-1, shift, rect, cv),
            KeyCode::Down if ml => self.vertical(1, shift, rect, cv),
            KeyCode::PageUp | KeyCode::PageDown if ml => {
                let n = ((rect.h - 2.0 * self.pad.1) / (self.style.size * 1.32).round()).floor().max(1.0) as i32;
                self.vertical(if code == KeyCode::PageUp { -n } else { n }, shift, rect, cv);
            }
            KeyCode::Up | KeyCode::PageUp => {
                self.model.move_to(0, shift);
                self.moved();
            }
            KeyCode::Down | KeyCode::PageDown => {
                self.model.move_to(self.model.len(), shift);
                self.moved();
            }
            KeyCode::Home | KeyCode::End => {
                let home = code == KeyCode::Home;
                let target = if ctrl || !ml {
                    if home { 0 } else { self.model.len() }
                } else if let Some(l) = self.lay(rect, cv) {
                    let (s, e) = self.line_bounds(&l, self.model.caret());
                    if home { s } else { e }
                } else if home {
                    self.model.hard_line_start(self.model.caret())
                } else {
                    self.model.hard_line_end(self.model.caret())
                };
                self.model.move_to(target, shift);
                self.moved();
            }
            KeyCode::Backspace if !ro => {
                let ok = if ctrl { self.model.backspace_word() } else { self.model.backspace() };
                self.edited(ok);
            }
            KeyCode::Delete if !ro => {
                if shift && !ctrl {
                    self.cut();
                } else {
                    let ok = if ctrl { self.model.delete_word() } else { self.model.delete() };
                    self.edited(ok);
                }
            }
            KeyCode::Backspace | KeyCode::Delete => {}
            KeyCode::Enter => {
                if !ml || ctrl {
                    self.events.push(EditEvent::Submit);
                } else if !ro {
                    let ok = self.model.insert_char('\n');
                    self.edited(ok);
                }
            }
            KeyCode::Escape => {
                if self.model.sel().is_some() {
                    self.model.move_to(self.model.caret(), false);
                    self.moved();
                } else {
                    self.events.push(EditEvent::Cancel);
                }
            }
            KeyCode::Insert if ctrl => self.copy(),
            KeyCode::Insert if shift => self.paste(),
            KeyCode::Char('A') if ctrl => {
                self.model.select_all();
                self.moved();
            }
            KeyCode::Char('C') if ctrl => self.copy(),
            KeyCode::Char('X') if ctrl => self.cut(),
            KeyCode::Char('V') if ctrl => self.paste(),
            KeyCode::Char('Z') if ctrl && !ro => {
                let ok = if shift { self.model.redo() } else { self.model.undo() };
                self.edited(ok);
            }
            KeyCode::Char('Y') if ctrl && !ro => {
                let ok = self.model.redo();
                self.edited(ok);
            }
            _ => used = false,
        }
        if !keep_x {
            self.want_x = None;
        }
        Response { consumed: used, redraw: used, ..Response::NONE }
    }

    // ------------------------------------------------------------ events

    /// Feed one input event. `rect` is the field's border box (window logical px); `cv` supplies text measurement (any canvas on the same
    /// device and text cache; it need not be inside a paint). The response says whether to redraw, capture the mouse, take keyboard focus,
    /// and which cursor to show.
    pub fn event(&mut self, ev: &InputEvent, rect: Rect, cv: &Canvas) -> Response {
        if !self.enabled {
            return Response::NONE;
        }
        self.update(rect, cv);
        let mut r = Response::NONE;
        if self.multiline() {
            let br = self.bar.event(ev, rect, &mut self.sy);
            if br.consumed || br.capture {
                return br;
            }
            r = br;
        }
        let active = self.focused && self.window_focus;
        let out = match ev {
            InputEvent::Focus(b) => {
                self.window_focus = *b;
                self.blink = 0.0;
                if !b {
                    self.drag = None;
                }
                Response { redraw: true, ..r }
            }
            InputEvent::MouseDown { pos, button, mods, clicks } if rect.contains(pos.0, pos.1) => {
                self.set_focused(true);
                self.window_focus = true;
                let Some(l) = self.lay(rect, cv) else { return Response { consumed: true, focus: true, ..r } };
                let i = self.index_at(rect, &l, *pos);
                match button {
                    MouseButton::Left => {
                        if *clicks <= 1 || self.password {
                            if *clicks >= 2 {
                                self.model.select_all();
                                self.drag = None;
                            } else {
                                self.model.move_to(i, mods.shift);
                                self.drag = Some(Drag::Chars);
                            }
                        } else if *clicks == 2 {
                            let (a, b) = self.model.word_range(i);
                            self.model.set_selection(a, b);
                            self.drag = Some(Drag::Words(a, b));
                        } else {
                            let (a, b) = if self.multiline() { self.model.line_range(i) } else { (0, self.model.len()) };
                            self.model.set_selection(a, b);
                            self.drag = Some(Drag::Lines(a, b));
                        }
                        self.want_x = None;
                        self.moved();
                        Response { consumed: true, redraw: true, capture: self.drag.is_some(), focus: true, cursor: Some(Cursor::Text), ..r }
                    }
                    MouseButton::Right => {
                        if !self.model.sel().is_some_and(|(a, b)| i >= a && i <= b) {
                            self.model.move_to(i, false);
                        }
                        self.moved();
                        self.events.push(EditEvent::ContextMenu(*pos));
                        Response { consumed: true, redraw: true, focus: true, ..r }
                    }
                    _ => Response { consumed: true, ..r },
                }
            }
            InputEvent::MouseMove { pos, .. } => {
                if self.drag.is_some() {
                    if let Some(l) = self.lay(rect, cv) {
                        let i = self.index_at(rect, &l, *pos);
                        match self.drag {
                            Some(Drag::Chars) => self.model.move_to(i, true),
                            Some(Drag::Words(a, b)) => {
                                let (wa, wb) = self.model.word_range(i);
                                if i < a {
                                    self.model.set_selection(b, wa);
                                } else {
                                    self.model.set_selection(a, wb.max(b));
                                }
                            }
                            Some(Drag::Lines(a, b)) => {
                                let (la, lb) = if self.multiline() { self.model.line_range(i) } else { (0, self.model.len()) };
                                if i < a {
                                    self.model.set_selection(b, la);
                                } else {
                                    self.model.set_selection(a, lb.max(b));
                                }
                            }
                            None => {}
                        }
                        self.moved();
                    }
                    Response { consumed: true, redraw: true, capture: true, cursor: Some(Cursor::Text), ..r }
                } else if rect.contains(pos.0, pos.1) {
                    Response { cursor: Some(Cursor::Text), ..r }
                } else {
                    r
                }
            }
            InputEvent::MouseUp { button: MouseButton::Left, .. } if self.drag.is_some() => {
                self.drag = None;
                Response { consumed: true, redraw: true, ..r }
            }
            InputEvent::Wheel { pos, delta, precise, .. } if self.multiline() && rect.contains(pos.0, pos.1) => {
                let moved = self.sy.wheel(delta.1, *precise);
                if moved {
                    self.bar.show();
                }
                Response { consumed: moved, redraw: moved, ..r }
            }
            InputEvent::Key { code, mods, down: true, .. } if active => {
                let k = self.key(*code, *mods, rect, cv);
                Response { cursor: r.cursor, ..k }
            }
            InputEvent::Char(c) if active && !self.read_only => {
                let ok = self.model.insert_char(*c);
                self.edited(ok);
                self.want_x = None;
                Response { consumed: ok, redraw: ok, changed: ok, ..r }
            }
            // IME composition: not drawn yet (see the module docs); finished text arrives as `Char`
            InputEvent::Ime(_) => r,
            _ => r,
        };
        let changed = self.events.iter().any(|e| *e == EditEvent::Changed);
        self.update(rect, cv);
        Response { changed: out.changed || changed && out.consumed, ..out }
    }

    // ------------------------------------------------------------ drawing

    /// Paint the field into `rect`. Call [`TextInput::update`] first when the text or rect changed without an `event` (events do it).
    pub fn draw(&self, cv: &Canvas, rect: Rect, t: &Theme) {
        let alpha = if self.enabled { 1.0 } else { 0.45 };
        cv.with_opacity(alpha, |cv| {
            cv.fill_rr(rect, self.radius, t.field);
            let f = self.focus_t.value();
            if f > 0.003 {
                cv.stroke_rr(rect, self.radius, 1.0, t.accent.fade(f));
                cv.stroke_rr(rect.inset(-1.0), self.radius + 1.0, 1.0, t.accent_soft.fade(f));
            }
            let inner = self.inner(rect);
            let clip = if self.multiline() { Rect::new(inner.x, rect.y + 2.0, inner.w, (rect.h - 4.0).max(0.0)) } else { Rect::new(inner.x, rect.y, inner.w, rect.h) };
            let Some(l) = self.lay(rect, cv) else { return };
            let o = self.origin(rect, &l);
            cv.with_clip(clip, |cv| {
                // placeholder (fades out when text arrives)
                let ph = self.ph_t.value();
                if ph > 0.01 && !self.placeholder.is_empty() {
                    let st = if self.multiline() { Style { ellipsis: false, ..self.style.wrap() } } else { Style { valign: VAlign::Middle, wrap: false, ellipsis: true, ..self.style } };
                    let r = if self.multiline() { Rect::new(inner.x, rect.y + self.pad.1, inner.w, (rect.h - self.pad.1).max(1.0)) } else { Rect::new(inner.x, rect.y, inner.w, rect.h) };
                    cv.text(r, &self.placeholder, st, t.dim.fade(ph));
                }
                // selection
                if let Some((a, b)) = self.model.sel() {
                    let col = t.accent.alpha(if self.focused && self.window_focus { 0.35 } else { 0.18 });
                    for r in self.selection_rects(&l, a, b) {
                        cv.fill_rect(Rect::new(o.0 + r.x, o.1 + r.y, r.w, r.h), col);
                    }
                }
                if !l.disp.s.is_empty() {
                    cv.text(Rect::new(o.0, o.1, l.box_w, l.box_h), &l.disp.s, l.style, t.text);
                }
                // caret
                if self.focused && self.window_focus && self.model.sel().is_none() && self.caret_visible() {
                    let (cx, cy, ch) = self.caret_local(&l, self.model.caret());
                    let px = ((o.0 + cx) * cv.scale).round() / cv.scale;
                    let h = ch.min(l.line_h + 2.0);
                    cv.fill_rect(Rect::new(px, (o.1 + cy + (ch - h) / 2.0).round(), 1.0, h), t.text.fade(self.focus_t.value().max(0.0)));
                }
            });
            if self.multiline() {
                self.bar.draw(cv, rect, &self.sy, t);
            }
        });
    }
}

#[cfg(test)]
mod tests;
