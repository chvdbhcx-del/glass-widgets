//! The on-canvas text prompt, model half: the panel state (layout, focus order,
//! Enter / Esc / Tab semantics, mouse). Nothing here touches Direct2D or Win32: text widths come in as a closure (`Measure`), so all of it is
//! unit-tested with a monospace stand-in. `ui.rs` draws a `PromptPanel`, `host.rs` feeds it keys / mouse and shows it on the host canvas;
//! `input.rs` is the worker-thread API (and the Win32 popup fallback).
//! The text field under it (caret, selection, word wrap) is `glasskit::textfield`, re-exported here.
use crate::input::{self, PromptOpts};
use crate::widget::Rect;
pub use glasskit::textfield::{index_at, wrap, Field, Key, Measure, VLine, MAX_CHARS};
use windows::Win32::Foundation::RECT;

/// Height of one text line, logical px (13 px body text).
pub const LINE_H: f32 = 19.0;

// ---------------------------------------------------------------- panel

/// Panel metrics, logical px.
pub const P_W: f32 = 420.0;
const PAD: f32 = 20.0;
const TOP: f32 = 16.0;
const TITLE_H: f32 = 24.0;
const WELL_H: f32 = 34.0;
const WELL_H_MULTI: f32 = 132.0;
const HINT_H: f32 = 16.0;
const BTN_H: f32 = 30.0;
const BOTTOM: f32 = 16.0;
/// the buttons are at least this wide (the "Done" of the other panels)
pub const BTN_MIN_W: f32 = 82.0;
/// the panel glides this many px while it fades in / out
pub const RISE: f32 = 8.0;
const OPEN_S: f32 = 0.18;
const CLOSE_S: f32 = 0.12;
/// the veil over the desktop while the prompt is up (the layer needs a non-transparent surface to catch clicks outside the panel anyway)
pub const SCRIM: f32 = 0.12;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Focus {
    Field,
    Ok,
    Cancel,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PHit {
    Field,
    Ok,
    Cancel,
    /// inside the panel, nothing there
    Panel,
    /// outside the panel: a click cancels
    Outside,
}

/// Rectangles of the panel (logical px, canvas coordinates).
#[derive(Clone, Copy, Debug)]
pub struct PLay {
    pub panel: Rect,
    pub title: Rect,
    pub well: Rect,
    /// where the text goes (inside the well)
    pub text: Rect,
    pub hint: Rect,
    pub ok: Rect,
    pub cancel: Rect,
}

/// Panel next to `anchor` (a point on the canvas): centred on it horizontally, below it (above when there is no room), kept inside `work`.
pub fn layout(multiline: bool, ok_w: f32, cancel_w: f32, anchor: (f32, f32), work: Rect) -> PLay {
    let well_h = if multiline { WELL_H_MULTI } else { WELL_H };
    let h = TOP + TITLE_H + 12.0 + well_h + 8.0 + HINT_H + 14.0 + BTN_H + BOTTOM;
    let w = P_W.min((work.w - 16.0).max(200.0));
    let wk = RECT { left: work.x.round() as i32, top: work.y.round() as i32, right: work.right().round() as i32, bottom: work.bottom().round() as i32 };
    let (x, y) = input::place((anchor.0.round() as i32, anchor.1.round() as i32), (w.round() as i32, h as i32), wk, 12, 8);
    let panel = Rect::new(x as f32, y as f32, w, h);
    let title = Rect::new(panel.x + PAD, panel.y + TOP, w - 2.0 * PAD, TITLE_H);
    let well = Rect::new(panel.x + PAD, title.bottom() + 12.0, w - 2.0 * PAD, well_h);
    let text = if multiline { well.inset_xy(12.0, 9.0) } else { well.inset_xy(12.0, 0.0) };
    let hint = Rect::new(panel.x + PAD, well.bottom() + 8.0, w - 2.0 * PAD, HINT_H);
    let by = hint.bottom() + 14.0;
    let ok = Rect::new(panel.right() - PAD - ok_w.max(BTN_MIN_W), by, ok_w.max(BTN_MIN_W), BTN_H);
    let cancel = Rect::new(ok.x - 8.0 - cancel_w.max(BTN_MIN_W), by, cancel_w.max(BTN_MIN_W), BTN_H);
    PLay { panel, title, well, text, hint, ok, cancel }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Mods {
    pub ctrl: bool,
    pub shift: bool,
}

/// What the host has to do after an event.
#[derive(Clone, Debug, PartialEq)]
pub enum Act {
    None,
    Redraw,
    /// the prompt is over: the answer (None = cancelled or blank)
    Done(Option<String>),
    /// put this text on the clipboard (and redraw)
    Clip(String),
}

pub struct PromptPanel {
    pub title: String,
    pub placeholder: String,
    pub hint: String,
    pub ok: String,
    pub cancel: String,
    pub multiline: bool,
    pub f: Field,
    pub lay: PLay,
    pub focus: Focus,
    /// the keyboard focus ring on a button
    pub ring: bool,
    pub hover: Option<Focus>,
    pub press: Option<Focus>,
    /// open progress 0..1 (linear; `eased` for drawing); runs back to 0 while `closing`
    pub t: f32,
    pub closing: bool,
    /// horizontal (single line) or vertical (multiline) scroll of the text, px
    pub scroll: f32,
    pub selecting: bool,
}

impl PromptPanel {
    pub fn new(o: &PromptOpts, ok_w: f32, cancel_w: f32, anchor: (f32, f32), work: Rect) -> PromptPanel {
        let tx = input::texts(o.lang, o.multiline, &o.ok_label);
        PromptPanel {
            title: o.title.clone(),
            placeholder: o.placeholder.clone(),
            hint: tx.hint,
            ok: tx.ok,
            cancel: tx.cancel.to_string(),
            multiline: o.multiline,
            f: Field::new(o.multiline, &o.initial),
            lay: layout(o.multiline, ok_w, cancel_w, anchor, work),
            focus: Focus::Field,
            ring: false,
            hover: None,
            press: None,
            t: 0.0,
            closing: false,
            scroll: 0.0,
            selecting: false,
        }
    }

    pub fn eased(&self) -> f32 {
        let q = 1.0 - self.t.clamp(0.0, 1.0);
        1.0 - q * q * q
    }
    /// The veil over the desktop at the current progress (4 steps: each change is a full-monitor frame).
    pub fn scrim(&self) -> f32 {
        SCRIM * (self.eased() * 4.0).round() / 4.0
    }
    /// Advance the open / close animation by `dt` seconds; true while it still moves.
    pub fn step(&mut self, dt: f32) -> bool {
        if self.closing {
            self.t = (self.t - dt / CLOSE_S).max(0.0);
            self.t > 0.0
        } else {
            self.t = (self.t + dt / OPEN_S).min(1.0);
            self.t < 1.0
        }
    }

    // ------------------------------------------------ text layout

    fn wrap_w(&self) -> f32 {
        self.lay.text.w - 3.0
    }
    pub fn lines(&self, w: Measure) -> Vec<VLine> {
        if self.multiline {
            wrap(&self.f.chars, self.wrap_w(), w)
        } else {
            vec![VLine { start: 0, end: self.f.chars.len() }]
        }
    }
    /// Scroll just enough to keep the caret in view (and never beyond the text after it got shorter).
    pub fn reveal(&mut self, w: Measure) {
        let lines = self.lines(w);
        let (li, x) = self.f.caret_x(&lines, w);
        if self.multiline {
            let (top, h) = (li as f32 * LINE_H, self.lay.text.h);
            if top < self.scroll {
                self.scroll = top;
            } else if top + LINE_H > self.scroll + h {
                self.scroll = top + LINE_H - h;
            }
            self.scroll = self.scroll.clamp(0.0, (lines.len() as f32 * LINE_H - h).max(0.0));
        } else {
            let avail = self.lay.text.w - 4.0;
            if x < self.scroll {
                self.scroll = x;
            } else if x > self.scroll + avail {
                self.scroll = x - avail;
            }
            self.scroll = self.scroll.clamp(0.0, (w(&self.f.chars) - avail + 2.0).max(0.0));
        }
    }
    /// Caret index under the point (canvas logical px).
    fn index_at_point(&self, x: f32, y: f32, w: Measure) -> usize {
        let t = self.lay.text;
        if self.multiline {
            let lines = self.lines(w);
            let li = (((y - t.y + self.scroll) / LINE_H).floor().max(0.0) as usize).min(lines.len() - 1);
            index_at(&self.f.chars, lines[li], x - t.x, w)
        } else {
            index_at(&self.f.chars, VLine { start: 0, end: self.f.chars.len() }, x - t.x + self.scroll, w)
        }
    }

    // ------------------------------------------------ keyboard

    fn confirm(&self) -> Act {
        let t = self.f.text();
        if t.trim().is_empty() {
            Act::Done(None)
        } else {
            // the multi-line answer keeps the EDIT control's CR LF (the notes widget converts it)
            Act::Done(Some(if self.multiline { t.replace('\n', "\r\n") } else { t }))
        }
    }
    fn activate(&self, f: Focus) -> Act {
        if f == Focus::Cancel {
            Act::Done(None)
        } else {
            self.confirm()
        }
    }
    /// Tab / Shift+Tab: field > Done > Cancel > field. Coming back to the field selects its text (as the EDIT did).
    pub fn cycle(&mut self, back: bool) {
        let order = [Focus::Field, Focus::Ok, Focus::Cancel];
        let i = order.iter().position(|&f| f == self.focus).unwrap_or(0);
        self.focus = order[(i + if back { 2 } else { 1 }) % 3];
        self.ring = self.focus != Focus::Field;
        if self.focus == Focus::Field {
            self.f.select_all();
        }
    }

    /// Enter = confirm (multiline: Ctrl+Enter, Enter is a newline), Esc = cancel, Tab walks the focus; the rest edits the field.
    pub fn key(&mut self, k: Key, m: Mods, w: Measure) -> Act {
        match k {
            Key::Esc => return Act::Done(None),
            Key::Tab => {
                self.cycle(m.shift);
                self.reveal(w);
                return Act::Redraw;
            }
            Key::Enter => {
                return match self.focus {
                    Focus::Field if self.multiline && !m.ctrl => {
                        self.f.insert('\n');
                        self.reveal(w);
                        Act::Redraw
                    }
                    f => self.activate(f),
                }
            }
            _ if self.focus != Focus::Field => {
                return if k == Key::Char(' ') { self.activate(self.focus) } else { Act::None };
            }
            _ => {}
        }
        let lines = self.lines(w);
        let (ctrl, shift) = (m.ctrl, m.shift);
        let mut clip = None;
        match k {
            Key::Char(c) => self.f.insert(c),
            Key::Back if ctrl => self.f.backspace_word(),
            Key::Back => self.f.backspace(),
            Key::Delete if ctrl => self.f.delete_word(),
            Key::Delete => self.f.delete(),
            Key::Left => self.f.left(shift, ctrl),
            Key::Right => self.f.right(shift, ctrl),
            Key::Up if self.multiline => self.f.vertical(&lines, w, -1, shift),
            Key::Down if self.multiline => self.f.vertical(&lines, w, 1, shift),
            Key::Home => self.f.home(&lines, shift, ctrl),
            Key::End => self.f.end(&lines, shift, ctrl),
            Key::SelectAll => self.f.select_all(),
            Key::Copy => clip = self.f.sel().map(|_| self.f.selected_text()),
            Key::Cut => {
                clip = self.f.sel().map(|_| self.f.selected_text());
                if clip.is_some() {
                    self.f.backspace();
                }
            }
            Key::Paste(s) => self.f.insert_str(&s),
            _ => {}
        }
        self.reveal(w);
        clip.map_or(Act::Redraw, Act::Clip)
    }

    // ------------------------------------------------ mouse (logical px)

    pub fn hit(&self, x: f32, y: f32) -> PHit {
        let l = &self.lay;
        if !l.panel.contains(x, y) {
            PHit::Outside
        } else if l.ok.contains(x, y) {
            PHit::Ok
        } else if l.cancel.contains(x, y) {
            PHit::Cancel
        } else if l.well.contains(x, y) {
            PHit::Field
        } else {
            PHit::Panel
        }
    }
    fn hit_focus(h: PHit) -> Option<Focus> {
        match h {
            PHit::Ok => Some(Focus::Ok),
            PHit::Cancel => Some(Focus::Cancel),
            _ => None,
        }
    }
    /// Left button down. A click outside the panel cancels; on the field it places the caret (Shift extends, `double` selects the word) and
    /// starts a drag selection; on a button it presses it (it acts on release).
    pub fn mouse_down(&mut self, x: f32, y: f32, m: Mods, double: bool, w: Measure) -> Act {
        match self.hit(x, y) {
            PHit::Outside => Act::Done(None),
            PHit::Panel => Act::None,
            PHit::Field => {
                self.focus = Focus::Field;
                self.ring = false;
                let i = self.index_at_point(x, y, w);
                if double {
                    self.f.select_word_at(i);
                } else {
                    self.f.click(i, m.shift);
                    self.selecting = true;
                }
                self.reveal(w);
                Act::Redraw
            }
            h => {
                self.focus = Self::hit_focus(h).unwrap_or(Focus::Field);
                self.ring = false;
                self.press = Self::hit_focus(h);
                Act::Redraw
            }
        }
    }
    pub fn mouse_move(&mut self, x: f32, y: f32, w: Measure) -> Act {
        let hover = Self::hit_focus(self.hit(x, y));
        let mut redraw = hover != self.hover;
        self.hover = hover;
        if self.selecting {
            let i = self.index_at_point(x, y, w);
            if i != self.f.caret {
                self.f.click(i, true);
                self.reveal(w);
                redraw = true;
            }
        }
        if redraw { Act::Redraw } else { Act::None }
    }
    pub fn mouse_up(&mut self, x: f32, y: f32) -> Act {
        self.selecting = false;
        match (self.press.take(), Self::hit_focus(self.hit(x, y))) {
            (Some(p), Some(h)) if p == h => self.activate(p),
            (Some(_), _) => Act::Redraw,
            _ => Act::None,
        }
    }
    /// Wheel over a multiline field: three lines a notch (+ = up). True when it scrolled.
    pub fn wheel(&mut self, notches: f32, w: Measure) -> bool {
        if !self.multiline {
            return false;
        }
        let n = self.lines(w).len() as f32;
        let max = (n * LINE_H - self.lay.text.h).max(0.0);
        let s = (self.scroll - notches * 3.0 * LINE_H).clamp(0.0, max);
        let moved = s != self.scroll;
        self.scroll = s;
        moved
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::widget::Lang;

    /// 8 px per char
    fn mono(s: &[char]) -> f32 {
        s.len() as f32 * 8.0
    }
    fn opts(multi: bool, initial: &str) -> PromptOpts {
        PromptOpts { title: "Заметка".into(), initial: initial.into(), multiline: multi, placeholder: "Текст…".into(), ok_label: String::new(), lang: Lang::Ru, anchor: None }
    }
    fn panel(multi: bool, initial: &str) -> PromptPanel {
        PromptPanel::new(&opts(multi, initial), 90.0, 90.0, (500.0, 300.0), Rect::new(0.0, 0.0, 1920.0, 1040.0))
    }
    const NO: Mods = Mods { ctrl: false, shift: false };
    const CTRL: Mods = Mods { ctrl: true, shift: false };

    #[test]
    fn layout_is_inside_the_work_area_and_ordered() {
        let work = Rect::new(0.0, 0.0, 1920.0, 1040.0);
        for multi in [false, true] {
            for anchor in [(960.0, 300.0), (3.0, 3.0), (1915.0, 1035.0), (-50.0, 500.0), (960.0, 1030.0)] {
                let l = layout(multi, 90.0, 90.0, anchor, work);
                let p = l.panel;
                assert!(p.x >= 8.0 && p.y >= 8.0 && p.right() <= 1912.0 && p.bottom() <= 1032.0, "{anchor:?}: {p:?}");
                for r in [l.title, l.well, l.text, l.hint, l.ok, l.cancel] {
                    assert!(r.x >= p.x && r.y >= p.y && r.right() <= p.right() + 0.01 && r.bottom() <= p.bottom() + 0.01, "{r:?} outside {p:?}");
                }
                assert!(l.title.bottom() <= l.well.y && l.well.bottom() <= l.hint.y && l.hint.bottom() <= l.ok.y);
                assert!(l.cancel.right() < l.ok.x && (l.ok.right() - (p.right() - 20.0)).abs() < 0.01);
                assert!(l.text.x > l.well.x && l.text.right() < l.well.right());
            }
        }
        // under the anchor with a gap, flipped above it near the bottom
        let l = layout(false, 90.0, 90.0, (960.0, 300.0), work);
        assert_eq!((l.panel.cx(), l.panel.y), (960.0, 312.0));
        let l = layout(false, 90.0, 90.0, (960.0, 1030.0), work);
        assert!(l.panel.bottom() <= 1030.0 - 12.0 + 0.01);
        // the multiline well is the taller one, 132 px like the Win32 popup
        assert_eq!(layout(true, 90.0, 90.0, (960.0, 300.0), work).well.h, 132.0);
        assert!(layout(true, 90.0, 90.0, (960.0, 300.0), work).panel.h > layout(false, 90.0, 90.0, (960.0, 300.0), work).panel.h + 90.0);
        // wide labels widen the buttons, the minimum is the "Done" of the other panels
        let l = layout(false, 130.0, 10.0, (960.0, 300.0), work);
        assert_eq!((l.ok.w, l.cancel.w), (130.0, BTN_MIN_W));
        // a work area narrower than the panel shrinks it
        assert!(layout(false, 90.0, 90.0, (100.0, 100.0), Rect::new(0.0, 0.0, 300.0, 600.0)).panel.w <= 284.0);
    }

    #[test]
    fn enter_esc_tab_semantics() {
        // single line: Enter confirms, Ctrl+Enter too, blank is a cancel
        let mut p = panel(false, "");
        assert_eq!(p.key(Key::Enter, NO, &mono), Act::Done(None));
        p.key(Key::Char('о'), NO, &mono);
        p.key(Key::Char('к'), NO, &mono);
        assert_eq!(p.key(Key::Enter, NO, &mono), Act::Done(Some("ок".into())));
        assert_eq!(p.key(Key::Enter, CTRL, &mono), Act::Done(Some("ок".into())));
        assert_eq!(p.key(Key::Esc, NO, &mono), Act::Done(None));
        // multiline: Enter is a newline, Ctrl+Enter confirms (with CR LF like the EDIT), whitespace only is blank
        let mut m = panel(true, "");
        m.key(Key::Char('a'), NO, &mono);
        assert_eq!(m.key(Key::Enter, NO, &mono), Act::Redraw);
        m.key(Key::Char('b'), NO, &mono);
        assert_eq!(m.f.text(), "a\nb");
        assert_eq!(m.key(Key::Enter, CTRL, &mono), Act::Done(Some("a\r\nb".into())));
        let mut ws = panel(true, "  \n ");
        assert_eq!(ws.key(Key::Enter, CTRL, &mono), Act::Done(None));
        // Tab: field > Done > Cancel > field, Shift+Tab goes back; the ring shows on the buttons only
        let mut p = panel(false, "text");
        assert_eq!((p.focus, p.ring), (Focus::Field, false));
        p.key(Key::Tab, NO, &mono);
        assert_eq!((p.focus, p.ring), (Focus::Ok, true));
        p.key(Key::Tab, NO, &mono);
        assert_eq!(p.focus, Focus::Cancel);
        p.key(Key::Tab, NO, &mono);
        assert_eq!((p.focus, p.ring, p.f.sel()), (Focus::Field, false, Some((0, 4)))); // back in the field: its text is selected
        p.key(Key::Tab, Mods { ctrl: false, shift: true }, &mono);
        assert_eq!(p.focus, Focus::Cancel);
        // on a button: Enter / Space press it, typing does nothing to the field
        assert_eq!(p.key(Key::Char('x'), NO, &mono), Act::None);
        assert_eq!(p.f.text(), "text");
        assert_eq!(p.key(Key::Enter, NO, &mono), Act::Done(None));
        p.key(Key::Tab, Mods { ctrl: false, shift: true }, &mono);
        assert_eq!(p.focus, Focus::Ok);
        assert_eq!(p.key(Key::Char(' '), NO, &mono), Act::Done(Some("text".into())));
        assert_eq!(p.key(Key::Enter, NO, &mono), Act::Done(Some("text".into())));
    }

    #[test]
    fn clipboard_keys_and_scroll() {
        let mut p = panel(false, "hello world");
        p.key(Key::Home, NO, &mono);
        p.key(Key::Right, Mods { ctrl: false, shift: true }, &mono);
        p.key(Key::Right, Mods { ctrl: false, shift: true }, &mono);
        assert_eq!(p.key(Key::Copy, CTRL, &mono), Act::Clip("he".into()));
        assert_eq!(p.key(Key::Cut, CTRL, &mono), Act::Clip("he".into()));
        assert_eq!(p.f.text(), "llo world");
        p.key(Key::Paste("ab\r\ncd".into()), CTRL, &mono);
        assert_eq!(p.f.text(), "ab cdllo world");
        assert_eq!(p.key(Key::Copy, CTRL, &mono), Act::Redraw); // nothing selected: nothing to copy
        p.key(Key::SelectAll, CTRL, &mono);
        assert_eq!(p.f.sel(), Some((0, 14)));
        // a long single line scrolls to keep the caret in view (field 356 px = 44 chars of 8 px), and back when it gets short
        let mut p = panel(false, "");
        for _ in 0..60 {
            p.key(Key::Char('w'), NO, &mono);
        }
        assert!(p.scroll > 100.0 && p.scroll <= 60.0 * 8.0, "{}", p.scroll);
        p.key(Key::Home, NO, &mono);
        assert_eq!(p.scroll, 0.0);
        p.key(Key::End, NO, &mono);
        p.key(Key::Back, CTRL, &mono);
        assert_eq!(p.scroll, 0.0);
    }

    #[test]
    fn multiline_wraps_scrolls_and_moves_by_visual_lines() {
        let text = "one two three four five six seven eight nine ten eleven twelve thirteen fourteen fifteen sixteen seventeen eighteen nineteen twenty";
        let mut p = panel(true, text);
        let lines = p.lines(&mono);
        // 44 chars per line in the 353 px wrap width: three-plus lines, every one fits
        assert!(lines.len() >= 3, "{lines:?}");
        assert!(lines.iter().all(|l| mono(&p.f.chars[l.start..l.end]) <= p.wrap_w() + 8.0));
        // 6 lines fit in the 132 px well: type until it overflows, the caret stays visible
        for _ in 0..12 {
            p.key(Key::Enter, NO, &mono);
            p.key(Key::Char('x'), NO, &mono);
        }
        let n = p.lines(&mono).len();
        assert!(p.scroll > 0.0 && p.scroll <= n as f32 * LINE_H);
        let (li, _) = p.f.caret_x(&p.lines(&mono), &mono);
        let top = li as f32 * LINE_H - p.scroll;
        assert!(top >= 0.0 && top + LINE_H <= p.lay.text.h + 0.01, "caret line {li} at {top}");
        // Up walks to the top and the view follows
        for _ in 0..n {
            p.key(Key::Up, NO, &mono);
        }
        assert_eq!((p.f.caret, p.scroll), (0, 0.0));
        // wheel: 3 lines a notch, clamped
        assert!(p.wheel(-1.0, &mono));
        assert_eq!(p.scroll, 3.0 * LINE_H);
        assert!(!p.wheel(10.0, &mono) || p.scroll == 0.0);
        let mut single = panel(false, "abc");
        assert!(!single.wheel(1.0, &mono));
    }

    #[test]
    fn mouse_hits_buttons_field_and_outside() {
        let mut p = panel(false, "hello");
        let (ok, cancel, well) = (p.lay.ok, p.lay.cancel, p.lay.well);
        assert_eq!(p.hit(ok.cx(), ok.cy()), PHit::Ok);
        assert_eq!(p.hit(cancel.cx(), cancel.cy()), PHit::Cancel);
        assert_eq!(p.hit(well.cx(), well.cy()), PHit::Field);
        assert_eq!(p.hit(p.lay.title.x + 2.0, p.lay.title.y + 2.0), PHit::Panel);
        assert_eq!(p.hit(1.0, 1.0), PHit::Outside);
        // outside = cancel; Done / Cancel act on release, only when still over the button
        assert_eq!(p.mouse_down(1.0, 1.0, NO, false, &mono), Act::Done(None));
        assert_eq!(p.mouse_down(ok.cx(), ok.cy(), NO, false, &mono), Act::Redraw);
        assert_eq!((p.focus, p.ring, p.press), (Focus::Ok, false, Some(Focus::Ok)));
        assert_eq!(p.mouse_up(ok.cx(), ok.cy()), Act::Done(Some("hello".into())));
        p.mouse_down(ok.cx(), ok.cy(), NO, false, &mono);
        assert_eq!(p.mouse_up(cancel.cx(), cancel.cy()), Act::Redraw); // released elsewhere: nothing happens
        p.mouse_down(cancel.cx(), cancel.cy(), NO, false, &mono);
        assert_eq!(p.mouse_up(cancel.cx(), cancel.cy()), Act::Done(None));
        // hover follows the pointer
        assert_eq!(p.mouse_move(ok.cx(), ok.cy(), &mono), Act::Redraw);
        assert_eq!(p.hover, Some(Focus::Ok));
        assert_eq!(p.mouse_move(ok.cx() + 1.0, ok.cy(), &mono), Act::None);
    }

    #[test]
    fn mouse_places_the_caret_and_drags_a_selection() {
        let mut p = panel(false, "hello world");
        let t = p.lay.text;
        // a click 3.4 chars in puts the caret there; Tab away and the click brings the field back
        p.cycle(false);
        p.mouse_down(t.x + 3.4 * 8.0, t.cy(), NO, false, &mono);
        assert_eq!((p.focus, p.f.caret, p.f.sel(), p.selecting), (Focus::Field, 3, None, true));
        // dragging to the right selects, the button release ends it
        p.mouse_move(t.x + 8.0 * 8.0 + 1.0, t.cy(), &mono);
        assert_eq!(p.f.sel(), Some((3, 8)));
        p.mouse_up(t.x, t.cy());
        assert!(!p.selecting);
        p.mouse_move(t.x, t.cy(), &mono);
        assert_eq!(p.f.sel(), Some((3, 8)));
        // Shift+click extends, a double click takes the word
        p.mouse_down(t.x + 11.0 * 8.0, t.cy(), Mods { ctrl: false, shift: true }, false, &mono);
        assert_eq!(p.f.sel(), Some((3, 11)));
        p.mouse_down(t.x + 7.0 * 8.0, t.cy(), NO, true, &mono);
        assert_eq!(p.f.selected_text(), "world");
        // multiline: the click picks the visual line under it
        let mut m = panel(true, "ab\ncd\nef");
        let t = m.lay.text;
        m.mouse_down(t.x + 8.0, t.y + LINE_H * 1.5, NO, false, &mono);
        assert_eq!(m.f.caret, 4);
    }

    #[test]
    fn open_and_close_animation() {
        let mut p = panel(false, "");
        assert_eq!((p.t, p.eased(), p.scrim()), (0.0, 0.0, 0.0));
        let mut steps = 0;
        while p.step(0.016) {
            steps += 1;
        }
        assert!((8..=14).contains(&steps), "{steps}");
        assert_eq!((p.t, p.eased(), p.scrim()), (1.0, 1.0, SCRIM));
        p.closing = true;
        let mut steps = 0;
        while p.step(0.016) {
            steps += 1;
        }
        assert!(steps < 10 && p.t == 0.0);
    }
}