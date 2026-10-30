//! The text-editing model behind [`super::TextInput`]: characters, a caret and an anchor (the selection is the range between them), cluster-aware
//! movement and deletion, word movement, a length limit, and an undo / redo history made of small operations. No GPU, no Win32, no layout:
//! everything that needs pixels (visual lines, hit testing) lives in the widget.
//!
//! Positions are indices into `chars()` (Unicode scalar values, not UTF-16 units and not bytes) and always sit on grapheme-cluster
//! boundaries (see [`super::grapheme`]); a position handed in from outside is snapped to the cluster start.
//!
//! **Undo.** Every edit is one [`Op`] (what was removed and inserted at where, and the selection before and after). Runs of typing, of
//! Backspace and of Delete merge into one step while the caret has not been moved in between; paste, cut, replacing a selection, deleting a word
//! or a selection and every programmatic `insert_text` are steps of their own. Moving the caret or changing the selection ends a run. Undo
//! restores the text AND the selection that existed before the edit. `set_text` is a reset: it clears the history; `replace_all` is the undoable
//! version.
use super::grapheme::{next_boundary, prev_boundary, snap};
use crate::textfield::MAX_CHARS;

/// Why an edit happened; decides whether it merges into the previous undo step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditKind {
    Typing,
    Backspace,
    Delete,
    Other,
}

#[derive(Clone, Debug)]
struct Op {
    at: usize,
    removed: Vec<char>,
    inserted: Vec<char>,
    caret_before: usize,
    anchor_before: usize,
    caret_after: usize,
    kind: EditKind,
}

/// Undo steps kept.
const HISTORY: usize = 500;

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

#[derive(Clone, Debug)]
pub struct EditModel {
    chars: Vec<char>,
    caret: usize,
    anchor: usize,
    pub multiline: bool,
    max_len: usize,
    undo: Vec<Op>,
    redo: Vec<Op>,
    /// the next edit may merge into the last undo step
    run: bool,
}

impl EditModel {
    pub fn new(multiline: bool, text: &str) -> EditModel {
        let mut m = EditModel { chars: Vec::new(), caret: 0, anchor: 0, multiline, max_len: MAX_CHARS, undo: Vec::new(), redo: Vec::new(), run: false };
        m.set_text(text);
        m
    }

    // ------------------------------------------------------------ reading

    pub fn chars(&self) -> &[char] {
        &self.chars
    }
    pub fn len(&self) -> usize {
        self.chars.len()
    }
    pub fn is_empty(&self) -> bool {
        self.chars.is_empty()
    }
    pub fn text(&self) -> String {
        self.chars.iter().collect()
    }
    pub fn caret(&self) -> usize {
        self.caret
    }
    pub fn anchor(&self) -> usize {
        self.anchor
    }
    /// The selected range, ordered; `None` when nothing is selected.
    pub fn sel(&self) -> Option<(usize, usize)> {
        (self.anchor != self.caret).then(|| (self.anchor.min(self.caret), self.anchor.max(self.caret)))
    }
    pub fn selected_text(&self) -> String {
        self.sel().map(|(a, b)| self.chars[a..b].iter().collect()).unwrap_or_default()
    }
    pub fn max_len(&self) -> usize {
        self.max_len
    }
    /// Longest text in chars (`None` = the crate-wide cap `textfield::MAX_CHARS`). Existing longer text is kept; only growth is refused.
    pub fn set_max_len(&mut self, n: Option<usize>) {
        self.max_len = n.map_or(MAX_CHARS, |n| n.min(MAX_CHARS));
    }
    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }
    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    // ------------------------------------------------------------ text normalisation

    /// Paste / programmatic text: CR LF and CR become LF, tabs become spaces, a single-line model turns newlines into spaces, other control
    /// characters are dropped.
    pub fn normalize(&self, s: &str) -> Vec<char> {
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
        v
    }

    // ------------------------------------------------------------ caret and selection

    fn set_caret(&mut self, i: usize, extend: bool) {
        self.caret = snap(&self.chars, i);
        if !extend {
            self.anchor = self.caret;
        }
        self.run = false;
    }

    /// Put the caret at `i` (snapped to a cluster start); `extend` keeps the anchor (shift-click, shift-arrows).
    pub fn move_to(&mut self, i: usize, extend: bool) {
        self.set_caret(i, extend);
    }

    pub fn move_left(&mut self, extend: bool, word: bool) {
        if let (false, false, Some((a, _))) = (extend, word, self.sel()) {
            return self.set_caret(a, false);
        }
        let t = if word { self.word_left(self.caret) } else { prev_boundary(&self.chars, self.caret) };
        self.set_caret(t, extend);
    }

    pub fn move_right(&mut self, extend: bool, word: bool) {
        if let (false, false, Some((_, b))) = (extend, word, self.sel()) {
            return self.set_caret(b, false);
        }
        let t = if word { self.word_right(self.caret) } else { next_boundary(&self.chars, self.caret) };
        self.set_caret(t, extend);
    }

    pub fn select_all(&mut self) {
        self.anchor = 0;
        self.caret = self.chars.len();
        self.run = false;
    }

    /// Select `anchor..caret` (either order, snapped to cluster starts).
    pub fn set_selection(&mut self, anchor: usize, caret: usize) {
        self.anchor = snap(&self.chars, anchor);
        self.caret = snap(&self.chars, caret);
        self.run = false;
    }

    // ------------------------------------------------------------ words and lines

    fn word_at(&self, i: usize) -> bool {
        self.chars.get(i).is_some_and(|&c| is_word_char(c))
    }

    /// Start of the word to the left of `i` (skips separators first), cluster-aware.
    pub fn word_left(&self, i: usize) -> usize {
        let mut j = snap(&self.chars, i);
        while j > 0 {
            let p = prev_boundary(&self.chars, j);
            if self.word_at(p) {
                break;
            }
            j = p;
        }
        while j > 0 {
            let p = prev_boundary(&self.chars, j);
            if !self.word_at(p) {
                break;
            }
            j = p;
        }
        j
    }

    /// Start of the next word after `i` (skips the rest of the current word, then separators).
    pub fn word_right(&self, i: usize) -> usize {
        let n = self.chars.len();
        let mut j = snap(&self.chars, i);
        while j < n && self.word_at(j) {
            j = next_boundary(&self.chars, j);
        }
        while j < n && !self.word_at(j) {
            j = next_boundary(&self.chars, j);
        }
        j
    }

    /// The word under `i` (a double click), or the one cluster there when `i` is not on a word; empty at the end of an empty text.
    pub fn word_range(&self, i: usize) -> (usize, usize) {
        let n = self.chars.len();
        let i = snap(&self.chars, i.min(n));
        let before = i > 0 && self.word_at(prev_boundary(&self.chars, i));
        if self.word_at(i) || before {
            let mut a = i;
            while a > 0 && self.word_at(prev_boundary(&self.chars, a)) {
                a = prev_boundary(&self.chars, a);
            }
            let mut b = i;
            while b < n && self.word_at(b) {
                b = next_boundary(&self.chars, b);
            }
            (a, b)
        } else if i < n {
            (i, next_boundary(&self.chars, i))
        } else {
            (i, i)
        }
    }

    /// Start of the hard line (the text between newlines) that holds `i`.
    pub fn hard_line_start(&self, i: usize) -> usize {
        self.chars[..i.min(self.chars.len())].iter().rposition(|&c| c == '\n').map_or(0, |p| p + 1)
    }

    /// End of the hard line that holds `i` (the index of its newline, or the end of the text).
    pub fn hard_line_end(&self, i: usize) -> usize {
        let i = i.min(self.chars.len());
        self.chars[i..].iter().position(|&c| c == '\n').map_or(self.chars.len(), |p| i + p)
    }

    /// The hard line under `i` including its newline (a triple click), so deleting the selection removes the whole line.
    pub fn line_range(&self, i: usize) -> (usize, usize) {
        let (a, e) = (self.hard_line_start(i), self.hard_line_end(i));
        (a, if e < self.chars.len() { e + 1 } else { e })
    }

    // ------------------------------------------------------------ editing

    /// Replace `a..b` by `ins` (cut to the room the length limit leaves) and record the undo step. False when nothing changed.
    fn apply(&mut self, a: usize, b: usize, mut ins: Vec<char>, kind: EditKind) -> bool {
        let room = self.max_len.saturating_sub(self.chars.len() - (b - a));
        if ins.len() > room {
            ins.truncate(room);
            // never end on half a cluster (the base of a sequence without its mark is fine, a lone ZWJ is not)
            while !ins.is_empty() && !is_boundary_after(&ins) {
                ins.pop();
            }
        }
        if a == b && ins.is_empty() {
            return false;
        }
        let op = Op { at: a, removed: self.chars[a..b].to_vec(), inserted: ins.clone(), caret_before: self.caret, anchor_before: self.anchor, caret_after: a + ins.len(), kind };
        self.chars.splice(a..b, ins);
        self.caret = op.caret_after;
        self.anchor = self.caret;
        self.redo.clear();
        let merged = self.run && self.merge(&op);
        if !merged {
            self.undo.push(op);
            if self.undo.len() > HISTORY {
                self.undo.remove(0);
            }
        }
        self.run = matches!(kind, EditKind::Typing | EditKind::Backspace | EditKind::Delete);
        true
    }

    /// Try to fold `n` into the last undo step.
    fn merge(&mut self, n: &Op) -> bool {
        let Some(l) = self.undo.last_mut() else { return false };
        if l.kind != n.kind {
            return false;
        }
        match n.kind {
            EditKind::Typing if l.removed.is_empty() && n.removed.is_empty() && n.at == l.at + l.inserted.len() => {
                l.inserted.extend_from_slice(&n.inserted);
                l.caret_after = n.caret_after;
                true
            }
            EditKind::Backspace if l.inserted.is_empty() && n.inserted.is_empty() && n.at + n.removed.len() == l.at => {
                let mut r = n.removed.clone();
                r.extend_from_slice(&l.removed);
                l.removed = r;
                l.at = n.at;
                l.caret_after = n.caret_after;
                true
            }
            EditKind::Delete if l.inserted.is_empty() && n.inserted.is_empty() && n.at == l.at => {
                l.removed.extend_from_slice(&n.removed);
                true
            }
            _ => false,
        }
    }

    fn range_or_caret(&self) -> (usize, usize) {
        self.sel().unwrap_or((self.caret, self.caret))
    }

    /// Typed character (replaces the selection). Control characters are refused, except a newline in a multi-line model.
    pub fn insert_char(&mut self, c: char) -> bool {
        if c.is_control() && !(c == '\n' && self.multiline) {
            return false;
        }
        let (a, b) = self.range_or_caret();
        self.apply(a, b, vec![c], EditKind::Typing)
    }

    /// Paste / programmatic insertion at the caret (replaces the selection); see [`EditModel::normalize`]. One undo step.
    pub fn insert_text(&mut self, s: &str) -> bool {
        let v = self.normalize(s);
        let (a, b) = self.range_or_caret();
        self.apply(a, b, v, EditKind::Other)
    }

    pub fn delete_selection(&mut self) -> bool {
        match self.sel() {
            Some((a, b)) => self.apply(a, b, Vec::new(), EditKind::Other),
            None => false,
        }
    }

    pub fn backspace(&mut self) -> bool {
        if self.sel().is_some() {
            return self.delete_selection();
        }
        if self.caret == 0 {
            return false;
        }
        let a = prev_boundary(&self.chars, self.caret);
        self.apply(a, self.caret, Vec::new(), EditKind::Backspace)
    }

    pub fn delete(&mut self) -> bool {
        if self.sel().is_some() {
            return self.delete_selection();
        }
        if self.caret >= self.chars.len() {
            return false;
        }
        let b = next_boundary(&self.chars, self.caret);
        self.apply(self.caret, b, Vec::new(), EditKind::Delete)
    }

    pub fn backspace_word(&mut self) -> bool {
        if self.sel().is_some() {
            return self.delete_selection();
        }
        let a = self.word_left(self.caret);
        self.apply(a, self.caret, Vec::new(), EditKind::Other)
    }

    pub fn delete_word(&mut self) -> bool {
        if self.sel().is_some() {
            return self.delete_selection();
        }
        let b = self.word_right(self.caret);
        self.apply(self.caret, b, Vec::new(), EditKind::Other)
    }

    /// Replace the whole text as one undoable step; the caret goes to the end.
    pub fn replace_all(&mut self, s: &str) -> bool {
        let v = self.normalize(s);
        let n = self.chars.len();
        self.apply(0, n, v, EditKind::Other)
    }

    /// Reset the text (no undo step, history cleared), caret at the end. Cut to the length limit.
    pub fn set_text(&mut self, s: &str) {
        let mut v = self.normalize(s);
        v.truncate(self.max_len);
        while !v.is_empty() && !is_boundary_after(&v) {
            v.pop();
        }
        self.chars = v;
        self.caret = self.chars.len();
        self.anchor = self.caret;
        self.undo.clear();
        self.redo.clear();
        self.run = false;
    }

    // ------------------------------------------------------------ history

    pub fn undo(&mut self) -> bool {
        let Some(op) = self.undo.pop() else { return false };
        let end = op.at + op.inserted.len();
        self.chars.splice(op.at..end, op.removed.iter().copied());
        self.caret = op.caret_before.min(self.chars.len());
        self.anchor = op.anchor_before.min(self.chars.len());
        self.redo.push(op);
        self.run = false;
        true
    }

    pub fn redo(&mut self) -> bool {
        let Some(op) = self.redo.pop() else { return false };
        let end = op.at + op.removed.len();
        self.chars.splice(op.at..end, op.inserted.iter().copied());
        self.caret = op.caret_after.min(self.chars.len());
        self.anchor = self.caret;
        self.undo.push(op);
        self.run = false;
        true
    }
}

/// Does `v` end on a cluster boundary (nothing dangling: a trailing ZWJ or a lone regional indicator may still be joined by what follows,
/// but an insertion cut by the length limit must not leave them)?
fn is_boundary_after(v: &[char]) -> bool {
    match v.last() {
        None => true,
        Some('\u{200D}') => false,
        Some(&c) if ('\u{1F1E6}'..='\u{1F1FF}').contains(&c) => v.iter().rev().take_while(|&&c| ('\u{1F1E6}'..='\u{1F1FF}').contains(&c)).count() % 2 == 0,
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(s: &str) -> EditModel {
        EditModel::new(true, s)
    }
    fn line(s: &str) -> EditModel {
        EditModel::new(false, s)
    }
    fn type_str(m: &mut EditModel, s: &str) {
        for c in s.chars() {
            m.insert_char(c);
        }
    }

    #[test]
    fn typing_replaces_the_selection_and_places_the_caret() {
        let mut e = line("hello world");
        assert_eq!((e.caret(), e.anchor()), (11, 11));
        e.set_selection(0, 5);
        assert!(e.insert_char('J'));
        assert_eq!((e.text().as_str(), e.caret()), ("J world", 1));
        assert!(!e.insert_char('\u{7}'), "controls are refused");
        assert!(!line("").insert_char('\n'), "no newline in a single line");
        let mut ml = m("");
        assert!(ml.insert_char('\n') && ml.text() == "\n");
    }

    #[test]
    fn paste_normalises_line_breaks_tabs_and_controls() {
        let mut e = m("");
        e.insert_text("a\r\nb\rc\td\u{7}e");
        assert_eq!(e.text(), "a\nb\nc de");
        let mut s = line("");
        s.insert_text("one\ntwo\r\nthree");
        assert_eq!(s.text(), "one two three", "single line: newlines become spaces");
    }

    #[test]
    fn cyrillic_words_and_word_movement() {
        let mut e = line("привет, мир! hello_world");
        e.move_to(0, false);
        e.move_right(false, true);
        assert_eq!(e.caret(), 8, "to the start of the next word");
        e.move_right(false, true);
        assert_eq!(e.caret(), 13);
        e.move_left(false, true);
        assert_eq!(e.caret(), 8);
        e.move_left(false, true);
        assert_eq!(e.caret(), 0);
        e.move_to(24, false);
        e.move_left(false, true);
        assert_eq!(e.caret(), 13, "underscore is part of a word");
        assert_eq!(e.word_range(2), (0, 6));
        assert_eq!(e.word_range(6), (0, 6), "just after a word still selects it");
        assert_eq!(e.word_range(7), (7, 8), "a separator alone: one cluster");
        assert_eq!(e.word_range(9), (8, 11));
        // ctrl+backspace / ctrl+delete
        let mut e = line("привет мир");
        assert!(e.backspace_word());
        assert_eq!(e.text(), "привет ");
        assert!(e.backspace_word());
        assert_eq!(e.text(), "");
        let mut e = line("раз два три");
        e.move_to(0, false);
        assert!(e.delete_word());
        assert_eq!(e.text(), "два три");
        assert!(!line("").delete_word() && !line("").backspace_word());
    }

    #[test]
    fn clusters_move_and_delete_as_one() {
        let family = "👨\u{200D}👩\u{200D}👧";
        let mut e = line(&format!("a{family}é"));
        // é as e + U+0301
        let mut e2 = line("e\u{301}x");
        e2.move_to(0, false);
        e2.move_right(false, false);
        assert_eq!(e2.caret(), 2, "one step over e + the accent");
        assert!(e2.backspace());
        assert_eq!(e2.text(), "x", "backspace removes the whole cluster");
        e.move_to(1, false);
        e.move_right(false, false);
        assert_eq!(e.caret(), 6, "one step over the whole family");
        assert!(e.backspace());
        assert_eq!(e.text(), "aé");
        let mut f = line("🇷🇺🇺🇸");
        f.move_to(0, false);
        f.delete();
        assert_eq!(f.text(), "🇺🇸", "a flag is deleted whole");
        // a position inside a cluster snaps to its start
        let mut g = line("e\u{301}");
        g.move_to(1, false);
        assert_eq!(g.caret(), 0);
        g.set_selection(1, 2);
        assert_eq!(g.sel(), Some((0, 2)), "a selection covers whole clusters");
    }

    #[test]
    fn selection_movement_collapses_and_extends() {
        let mut e = line("abcdef");
        e.move_to(2, false);
        e.move_right(true, false);
        e.move_right(true, false);
        assert_eq!((e.sel(), e.selected_text().as_str()), (Some((2, 4)), "cd"));
        e.move_left(false, false);
        assert_eq!((e.caret(), e.sel()), (2, None), "left collapses to the start of the selection");
        e.set_selection(1, 4);
        e.move_right(false, false);
        assert_eq!(e.caret(), 4, "right collapses to the end");
        e.select_all();
        assert_eq!(e.selected_text(), "abcdef");
        assert!(e.delete());
        assert_eq!(e.text(), "");
        assert!(!e.delete() && !e.backspace());
    }

    #[test]
    fn max_length_truncates_inserts() {
        let mut e = line("");
        e.set_max_len(Some(5));
        type_str(&mut e, "abcdefg");
        assert_eq!(e.text(), "abcde");
        assert!(!e.insert_char('z'));
        e.insert_text("12345");
        assert_eq!(e.text(), "abcde");
        e.set_selection(1, 3);
        assert!(e.insert_text("XYZ"), "the selection frees room");
        assert_eq!(e.text(), "aXYde");
        let mut f = line("");
        f.set_max_len(Some(3));
        f.insert_text("a👨\u{200D}👩");
        assert_eq!(f.text(), "a👨", "never cut right after a ZWJ");
        f.set_text("abcdefgh");
        assert_eq!(f.text(), "abc", "set_text is cut as well");
        f.set_max_len(None);
        assert_eq!(f.max_len(), MAX_CHARS);
    }

    #[test]
    fn typing_runs_coalesce_into_one_undo_step() {
        let mut e = line("");
        type_str(&mut e, "hello");
        e.move_to(5, false); // a caret jump ends the run
        type_str(&mut e, " world");
        assert_eq!(e.text(), "hello world");
        assert!(e.undo());
        assert_eq!(e.text(), "hello", "the second run");
        assert!(e.undo());
        assert_eq!(e.text(), "");
        assert!(!e.undo());
        assert!(e.redo() && e.text() == "hello");
        assert!(e.redo() && e.text() == "hello world");
        assert_eq!(e.caret(), 11);
        assert!(!e.redo());
        // a new edit clears redo
        e.undo();
        e.insert_char('!');
        assert!(!e.can_redo());
    }

    #[test]
    fn backspace_and_delete_runs_coalesce_and_kinds_break_runs() {
        let mut e = line("abcdef");
        e.backspace();
        e.backspace();
        e.backspace();
        assert_eq!(e.text(), "abc");
        e.insert_char('X'); // kind change: new step
        assert!(e.undo());
        assert_eq!(e.text(), "abc");
        assert!(e.undo());
        assert_eq!(e.text(), "abcdef", "three backspaces were one step");
        assert_eq!(e.caret(), 6);
        e.move_to(1, false);
        e.delete();
        e.delete();
        assert_eq!(e.text(), "adef");
        assert!(e.undo());
        assert_eq!(e.text(), "abcdef");
        assert_eq!(e.caret(), 1);
        // paste, cut and word deletes are steps of their own
        let mut p = line("");
        type_str(&mut p, "ab");
        p.insert_text("CD");
        type_str(&mut p, "ef");
        assert_eq!(p.text(), "abCDef");
        assert!(p.undo() && p.text() == "abCD");
        assert!(p.undo() && p.text() == "ab");
        assert!(p.undo() && p.text() == "");
    }

    #[test]
    fn undo_restores_the_selection() {
        let mut e = line("hello world");
        e.set_selection(0, 5);
        e.insert_text("bye");
        assert_eq!(e.text(), "bye world");
        assert!(e.undo());
        assert_eq!((e.text().as_str(), e.sel()), ("hello world", Some((0, 5))));
        assert!(e.redo());
        assert_eq!((e.text().as_str(), e.caret()), ("bye world", 3));
        // typing over a selection is its own step, not merged with the next characters
        let mut t = line("abc");
        t.select_all();
        t.insert_char('x');
        t.insert_char('y');
        assert!(t.undo());
        assert_eq!(t.text(), "x");
        assert!(t.undo());
        assert_eq!((t.text().as_str(), t.sel()), ("abc", Some((0, 3))));
    }

    #[test]
    fn set_text_resets_and_replace_all_is_undoable() {
        let mut e = line("one");
        type_str(&mut e, "!");
        e.set_text("fresh start");
        assert!(!e.can_undo());
        assert_eq!((e.text().as_str(), e.caret()), ("fresh start", 11));
        assert!(e.replace_all("again"));
        assert_eq!(e.text(), "again");
        assert!(e.undo());
        assert_eq!(e.text(), "fresh start");
        assert!(!e.replace_all("fresh start") || e.text() == "fresh start");
    }

    #[test]
    fn hard_lines() {
        let e = m("one\ntwo\n\nfour");
        assert_eq!((e.hard_line_start(5), e.hard_line_end(5)), (4, 7));
        assert_eq!(e.line_range(5), (4, 8), "includes the newline");
        assert_eq!(e.line_range(8), (8, 9), "an empty line is just its newline");
        assert_eq!(e.line_range(12), (9, 13), "the last line has no newline");
        assert_eq!((e.hard_line_start(0), e.hard_line_end(100)), (0, 13));
    }

    #[test]
    fn history_is_bounded() {
        let mut e = line("");
        for i in 0..(HISTORY + 20) {
            e.insert_text(&i.to_string());
        }
        let mut steps = 0;
        while e.undo() {
            steps += 1;
        }
        assert_eq!(steps, HISTORY);
    }
}
