//! The on-canvas text prompt, model half: the text field (selection, word wrap, vertical scroll) and the panel state (layout, focus order,
//! Enter / Esc / Tab semantics, mouse). Nothing here touches Direct2D or Win32: text widths come in as a closure (`Measure`), so all of it is
//! unit-tested with a monospace stand-in. `ui.rs` draws a `PromptPanel`, `host.rs` feeds it keys / mouse and shows it on the host canvas;
//! `input.rs` is the worker-thread API (and the Win32 popup fallback).
use crate::input::{self, PromptOpts};
use crate::widget::Rect;
use windows::Win32::Foundation::RECT;

/// The longest text a prompt holds (the old EDIT took ~30000; a note this long is not a note).
pub const MAX_CHARS: usize = 10_000;
/// Height of one text line, logical px (13 px body text).
pub const LINE_H: f32 = 19.0;
/// Width of a visual line is measured through this (text -> logical px).
pub type Measure<'a> = &'a dyn Fn(&[char]) -> f32;

// ---------------------------------------------------------------- text field

/// One visual (wrapped) line: char range `start..end`, the newline that ended it is not part of it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VLine {
    pub start: usize,
    pub end: usize,
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Longest stretch measured at once when wrapping: no visual line has more chars than this.
const PROBE: usize = 300;

/// Soft word wrap of `chars` to `max_w`: hard lines (`\n`) split further at the last word start that fits (the spaces stay at the end of the
/// line they follow); a word wider than the field breaks by characters. Always at least one line (the empty text has the empty line).
pub fn wrap(chars: &[char], max_w: f32, w: Measure) -> Vec<VLine> {
    let mut out = Vec::new();
    let mut hs = 0;
    loop {
        let he = chars[hs..].iter().position(|&c| c == '\n').map_or(chars.len(), |p| hs + p);
        wrap_hard(chars, hs, he, max_w, w, &mut out);
        if he >= chars.len() {
            break;
        }
        hs = he + 1;
    }
    out
}

fn wrap_hard(chars: &[char], mut s: usize, e: usize, max_w: f32, w: Measure, out: &mut Vec<VLine>) {
    loop {
        let lim = e.min(s + PROBE);
        if s >= lim || w(&chars[s..lim]) <= max_w {
            out.push(VLine { start: s, end: lim });
            if lim >= e {
                return;
            }
            s = lim;
            continue;
        }
        let mut brk = None;
        for i in s + 1..lim {
            if chars[i - 1] == ' ' && chars[i] != ' ' {
                let mut t = i;
                while t > s && chars[t - 1] == ' ' {
                    t -= 1;
                }
                if w(&chars[s..t]) <= max_w {
                    brk = Some(i);
                } else {
                    break;
                }
            }
        }
        let b = brk.unwrap_or_else(|| {
            // one word wider than the field: the most chars that fit (at least one)
            let (mut lo, mut hi) = (1usize, (lim - s).saturating_sub(1));
            while lo < hi {
                let mid = (lo + hi + 1) / 2;
                if w(&chars[s..s + mid]) <= max_w {
                    lo = mid;
                } else {
                    hi = mid - 1;
                }
            }
            s + lo
        });
        out.push(VLine { start: s, end: b });
        s = b;
        if s >= e {
            return;
        }
    }
}

/// The caret index on line `l` nearest to `x` px from the line's left edge. A wrapped line never answers its own `end` (that index shows at the
/// start of the next line).
pub fn index_at(chars: &[char], l: VLine, x: f32, w: Measure) -> usize {
    if x <= 0.0 || l.end <= l.start {
        return l.start;
    }
    let (mut lo, mut hi) = (l.start, l.end);
    while lo < hi {
        let mid = (lo + hi) / 2;
        if w(&chars[l.start..mid]) < x {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    let mut i = lo;
    if i > l.start {
        let (a, b) = (w(&chars[l.start..i - 1]), w(&chars[l.start..i]));
        if x - a < b - x {
            i -= 1;
        }
    }
    if i == l.end && l.end < chars.len() && chars[l.end] != '\n' {
        i = i.saturating_sub(1).max(l.start);
    }
    i
}

/// Text with a caret and a selection (`anchor..caret`, either order). Single-line fields hold no newline.
#[derive(Clone, Debug, Default)]
pub struct Field {
    pub chars: Vec<char>,
    pub caret: usize,
    pub anchor: usize,
    pub multiline: bool,
    /// the x the caret keeps while Up / Down walk over lines of different length
    pub want_x: Option<f32>,
}

impl Field {
    pub fn new(multiline: bool, text: &str) -> Field {
        let mut f = Field { multiline, ..Default::default() };
        f.insert_str(text);
        f
    }
    pub fn text(&self) -> String {
        self.chars.iter().collect()
    }
    /// The selected range, ordered; None when nothing is selected.
    pub fn sel(&self) -> Option<(usize, usize)> {
        (self.anchor != self.caret).then(|| (self.anchor.min(self.caret), self.anchor.max(self.caret)))
    }
    pub fn selected_text(&self) -> String {
        self.sel().map(|(a, b)| self.chars[a..b].iter().collect()).unwrap_or_default()
    }
    fn set_caret(&mut self, i: usize, extend: bool) {
        self.caret = i.min(self.chars.len());
        if !extend {
            self.anchor = self.caret;
        }
        self.want_x = None;
    }
    pub fn click(&mut self, i: usize, extend: bool) {
        self.set_caret(i, extend);
    }
    fn delete_sel(&mut self) -> bool {
        let Some((a, b)) = self.sel() else { return false };
        self.chars.drain(a..b);
        self.caret = a;
        self.anchor = a;
        self.want_x = None;
        true
    }
    pub fn insert(&mut self, c: char) {
        if c.is_control() && !(c == '\n' && self.multiline) {
            return;
        }
        self.splice(&[c]);
    }
    fn splice(&mut self, new: &[char]) {
        self.delete_sel();
        let room = MAX_CHARS.saturating_sub(self.chars.len());
        let new = &new[..new.len().min(room)];
        self.chars.splice(self.caret..self.caret, new.iter().copied());
        self.caret += new.len();
        self.anchor = self.caret;
        self.want_x = None;
    }
    /// Paste / initial text: CR LF and CR become LF, tabs become spaces, a single-line field turns newlines into spaces, other controls go.
    pub fn insert_str(&mut self, s: &str) {
        let mut v = Vec::with_capacity(s.len());
        let mut cr = false;
        for c in s.chars() {
            if c == '\n' && cr {
                cr = false;
                continue;
            }
            cr = c == '\r';
            let c = match c {
                '\r' => '\n',
                '\t' => ' ',
                c => c,
            };
            let c = if c == '\n' && !self.multiline { ' ' } else { c };
            if !c.is_control() || c == '\n' {
                v.push(c);
            }
        }
        if !v.is_empty() {
            self.splice(&v);
        }
    }
    pub fn backspace(&mut self) {
        if self.delete_sel() || self.caret == 0 {
            return;
        }
        self.caret -= 1;
        self.chars.remove(self.caret);
        self.anchor = self.caret;
        self.want_x = None;
    }
    pub fn delete(&mut self) {
        if self.delete_sel() || self.caret >= self.chars.len() {
            return;
        }
        self.chars.remove(self.caret);
        self.want_x = None;
    }
    pub fn backspace_word(&mut self) {
        if self.sel().is_none() {
            self.anchor = self.word_left(self.caret);
        }
        self.delete_sel();
    }
    pub fn delete_word(&mut self) {
        if self.sel().is_none() {
            self.anchor = self.word_right(self.caret);
            std::mem::swap(&mut self.anchor, &mut self.caret); // caret stays where it is, the selection runs to the next word
        }
        self.delete_sel();
    }
    pub fn word_left(&self, mut i: usize) -> usize {
        while i > 0 && !is_word(self.chars[i - 1]) {
            i -= 1;
        }
        while i > 0 && is_word(self.chars[i - 1]) {
            i -= 1;
        }
        i
    }
    pub fn word_right(&self, mut i: usize) -> usize {
        let n = self.chars.len();
        while i < n && is_word(self.chars[i]) {
            i += 1;
        }
        while i < n && !is_word(self.chars[i]) {
            i += 1;
        }
        i
    }
    pub fn left(&mut self, extend: bool, word: bool) {
        if let (false, false, Some((a, _))) = (extend, word, self.sel()) {
            return self.set_caret(a, false);
        }
        let t = if word { self.word_left(self.caret) } else { self.caret.saturating_sub(1) };
        self.set_caret(t, extend);
    }
    pub fn right(&mut self, extend: bool, word: bool) {
        if let (false, false, Some((_, b))) = (extend, word, self.sel()) {
            return self.set_caret(b, false);
        }
        let t = if word { self.word_right(self.caret) } else { (self.caret + 1).min(self.chars.len()) };
        self.set_caret(t, extend);
    }
    /// Is `l` a line that was wrapped (not ended by a newline or the end of the text)?
    fn soft(&self, l: VLine) -> bool {
        l.end < self.chars.len() && self.chars[l.end] != '\n'
    }
    pub fn line_of(&self, lines: &[VLine], i: usize) -> usize {
        lines.iter().rposition(|l| l.start <= i).unwrap_or(0)
    }
    /// Home: start of the visual line (`doc`: of the text).
    pub fn home(&mut self, lines: &[VLine], extend: bool, doc: bool) {
        let t = if doc { 0 } else { lines[self.line_of(lines, self.caret)].start };
        self.set_caret(t, extend);
    }
    /// End: end of the visual line (before the space a wrapped line ends in); `doc`: end of the text.
    pub fn end(&mut self, lines: &[VLine], extend: bool, doc: bool) {
        let l = lines[self.line_of(lines, self.caret)];
        let t = if doc {
            self.chars.len()
        } else if self.soft(l) {
            l.end.saturating_sub(1).max(l.start)
        } else {
            l.end
        };
        self.set_caret(t, extend);
    }
    /// (visual line of the caret, x of the caret on it)
    pub fn caret_x(&self, lines: &[VLine], w: Measure) -> (usize, f32) {
        let li = self.line_of(lines, self.caret);
        (li, w(&self.chars[lines[li].start..self.caret.max(lines[li].start)]))
    }
    /// Up / Down by visual lines, keeping the column; off the first / last line the caret goes to the start / end of the text.
    pub fn vertical(&mut self, lines: &[VLine], w: Measure, dir: i32, extend: bool) {
        let (li, x) = self.caret_x(lines, w);
        let want = self.want_x.unwrap_or(x);
        let t = li as i32 + dir;
        let i = if t < 0 {
            0
        } else if t as usize >= lines.len() {
            self.chars.len()
        } else {
            index_at(&self.chars, lines[t as usize], want, w)
        };
        self.set_caret(i, extend);
        self.want_x = Some(want);
    }
    pub fn select_all(&mut self) {
        self.anchor = 0;
        self.caret = self.chars.len();
        self.want_x = None;
    }
    /// Double click: the word under index `i` (or the one char there).
    pub fn select_word_at(&mut self, i: usize) {
        let n = self.chars.len();
        let i = i.min(n);
        let (mut a, mut b) = (i, i);
        if (i < n && is_word(self.chars[i])) || (i > 0 && is_word(self.chars[i - 1])) {
            while a > 0 && is_word(self.chars[a - 1]) {
                a -= 1;
            }
            while b < n && is_word(self.chars[b]) {
                b += 1;
            }
        } else if i < n {
            b = i + 1;
        }
        self.anchor = a;
        self.caret = b;
        self.want_x = None;
    }
}

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

#[derive(Clone, Debug, PartialEq)]
pub enum Key {
    Char(char),
    Enter,
    Esc,
    Tab,
    Back,
    Delete,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    SelectAll,
    Copy,
    Cut,
    /// Ctrl+V with the clipboard text
    Paste(String),
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
    fn txt(f: &Field) -> String {
        f.text()
    }
    fn ranges(l: &[VLine]) -> Vec<(usize, usize)> {
        l.iter().map(|l| (l.start, l.end)).collect()
    }
    fn chars(s: &str) -> Vec<char> {
        s.chars().collect()
    }

    #[test]
    fn field_edits_and_selection() {
        let mut f = Field::new(false, "hello world");
        assert_eq!((f.caret, f.anchor), (11, 11));
        f.left(true, false);
        f.left(true, false);
        assert_eq!((f.sel(), f.selected_text().as_str()), (Some((9, 11)), "ld"));
        f.insert('X'); // replaces the selection
        assert_eq!((txt(&f).as_str(), f.caret, f.sel()), ("hello worX", 10, None));
        f.select_all();
        assert_eq!(f.selected_text(), "hello worX");
        f.backspace();
        assert_eq!((txt(&f).as_str(), f.caret), ("", 0));
        // arrows collapse a selection to its edge, Shift+arrows grow it
        let mut f = Field::new(false, "abcdef");
        f.click(1, false);
        f.right(true, false);
        f.right(true, false);
        assert_eq!(f.sel(), Some((1, 3)));
        f.right(false, false);
        assert_eq!((f.caret, f.sel()), (3, None));
        f.click(4, false);
        f.left(true, false);
        f.left(false, false);
        assert_eq!(f.caret, 3, "{f:?}"); // collapses to the left edge of 3..4
        f.delete();
        assert_eq!(txt(&f), "abcef");
        // control characters never get in, a single line has no newline
        f.insert('\u{7}');
        f.insert('\n');
        assert_eq!(txt(&f), "abcef");
    }

    #[test]
    fn field_words_and_limits() {
        let mut f = Field::new(false, "one two  three");
        f.home(&[VLine { start: 0, end: 14 }], false, false);
        f.right(false, true);
        assert_eq!(f.caret, 4);
        f.right(false, true);
        assert_eq!(f.caret, 9);
        f.left(false, true);
        assert_eq!(f.caret, 4);
        f.backspace_word();
        assert_eq!(txt(&f), "two  three");
        f.delete_word();
        assert_eq!(txt(&f), "three");
        f.select_word_at(2);
        assert_eq!(f.selected_text(), "three");
        // the limit
        let mut big = Field::new(true, "");
        big.insert_str(&"ж".repeat(MAX_CHARS + 50));
        assert_eq!(big.chars.len(), MAX_CHARS);
        big.insert('a');
        assert_eq!(big.chars.len(), MAX_CHARS);
    }

    #[test]
    fn paste_normalises_line_breaks() {
        let mut one = Field::new(false, "");
        one.insert_str("a\r\nb\tc\rd\u{1}e");
        assert_eq!(txt(&one), "a b c de");
        let mut multi = Field::new(true, "");
        multi.insert_str("a\r\nb\rc\nd");
        assert_eq!(txt(&multi), "a\nb\nc\nd");
        // the selection is replaced by a paste
        multi.select_all();
        multi.insert_str("Привет");
        assert_eq!(txt(&multi), "Привет");
    }

    #[test]
    fn wraps_at_words_and_breaks_long_words() {
        let c = chars("hello world foo");
        assert_eq!(ranges(&wrap(&c, 80.0, &mono)), vec![(0, 6), (6, 15)]);
        assert_eq!(ranges(&wrap(&c, 1000.0, &mono)), vec![(0, 15)]);
        // a word wider than the field breaks by characters, then goes on
        let c = chars("abcdefghijklmnop");
        assert_eq!(ranges(&wrap(&c, 80.0, &mono)), vec![(0, 10), (10, 16)]);
        let c = chars("hi abcdefghijklmnop");
        assert_eq!(ranges(&wrap(&c, 80.0, &mono)), vec![(0, 3), (3, 13), (13, 19)]);
        // hard newlines: empty lines count, a trailing newline leaves an empty last line, the empty text is one empty line
        assert_eq!(ranges(&wrap(&chars("ab\n\ncd"), 80.0, &mono)), vec![(0, 2), (3, 3), (4, 6)]);
        assert_eq!(ranges(&wrap(&chars("ab\n"), 80.0, &mono)), vec![(0, 2), (3, 3)]);
        assert_eq!(ranges(&wrap(&[], 80.0, &mono)), vec![(0, 0)]);
        // a field narrower than one char still terminates
        assert_eq!(wrap(&chars("abc"), 1.0, &mono).len(), 3);
        // several spaces stay on the line they follow
        let c = chars("aaaa      bbbb");
        assert_eq!(ranges(&wrap(&c, 80.0, &mono)), vec![(0, 10), (10, 14)]);
    }

    #[test]
    fn caret_lines_and_home_end() {
        let mut f = Field::new(true, "hello world foo");
        let lines = wrap(&f.chars, 80.0, &mono); // "hello " | "world foo"
        // the caret at a wrap point belongs to the lower line
        f.click(6, false);
        assert_eq!(f.caret_x(&lines, &mono), (1, 0.0));
        f.click(5, false);
        assert_eq!(f.caret_x(&lines, &mono), (0, 40.0));
        // End of a wrapped line stops before its trailing space, Home goes to the line start
        f.end(&lines, false, false);
        assert_eq!(f.caret, 5);
        f.click(9, false);
        f.home(&lines, false, false);
        assert_eq!(f.caret, 6);
        f.end(&lines, false, false);
        assert_eq!(f.caret, 15); // the last line ends at the text end
        f.home(&lines, true, true);
        assert_eq!(f.sel(), Some((0, 15)));
    }

    #[test]
    fn up_down_keep_the_column() {
        let mut f = Field::new(true, "abcdefgh\nab\nabcdefgh");
        let lines = wrap(&f.chars, 200.0, &mono);
        assert_eq!(ranges(&lines), vec![(0, 8), (9, 11), (12, 20)]);
        f.click(6, false); // column 6 of line 0
        f.vertical(&lines, &mono, 1, false);
        assert_eq!(f.caret, 11); // the short line: its end
        f.vertical(&lines, &mono, 1, false);
        assert_eq!(f.caret, 18); // the column comes back
        f.vertical(&lines, &mono, 1, false);
        assert_eq!(f.caret, 20); // off the last line: the end of the text
        f.vertical(&lines, &mono, -1, true); // Shift+Up extends from the old caret
        assert_eq!(f.sel(), Some((11, 20)));
        let mut g = Field::new(true, "ab\ncd");
        let l = wrap(&g.chars, 200.0, &mono);
        g.click(1, false);
        g.vertical(&l, &mono, -1, false);
        assert_eq!(g.caret, 0); // off the first line: the start
    }

    #[test]
    fn index_at_is_the_nearest_boundary() {
        let c = chars("abcdef");
        let l = VLine { start: 0, end: 6 };
        assert_eq!(index_at(&c, l, 0.0, &mono), 0);
        assert_eq!(index_at(&c, l, 3.9, &mono), 0);
        assert_eq!(index_at(&c, l, 4.1, &mono), 1);
        assert_eq!(index_at(&c, l, 19.0, &mono), 2);
        assert_eq!(index_at(&c, l, 1000.0, &mono), 6);
        // a wrapped line never answers its end (that is the next line's start)
        let c = chars("hello world");
        let wrapped = VLine { start: 0, end: 6 };
        assert_eq!(index_at(&c, wrapped, 1000.0, &mono), 5);
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
