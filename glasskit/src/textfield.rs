//! A text-field model with no UI in it: a `Field` (chars, caret, selection, word-wise movement, line start / end, up / down by visual lines)
//! and soft word wrap (`wrap`, `index_at`). Nothing here touches Direct2D or Win32: text widths come in as a closure (`Measure`), so the whole
//! model is unit-tested with a monospace stand-in. A view draws the lines `wrap` returns and feeds the keys it gets as a `Key`.

/// The longest text a prompt holds (the old EDIT took ~30000; a note this long is not a note).
pub const MAX_CHARS: usize = 10_000;
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

/// A key press as the models (text field, menu, prompt) see it; the host translates its native key events into these.
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
