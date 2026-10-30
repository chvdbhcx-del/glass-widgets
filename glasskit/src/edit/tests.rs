//! Widget tests: events through a headless WARP device (DirectWrite measures for real, so hit testing is exercised against actual glyph
//! positions), plus pixel checks and review images of every state.
use super::*;
use crate::color::Color;
use crate::input::Buttons;
use crate::testkit::Harness;

const R: Rect = Rect::new(10.0, 10.0, 200.0, 36.0);
const RM: Rect = Rect::new(10.0, 10.0, 160.0, 90.0);

fn img_dir() -> std::path::PathBuf {
    let base = std::env::var_os("CARGO_TARGET_DIR").map(std::path::PathBuf::from).unwrap_or_else(|| std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../target"));
    base.join("kit-images")
}

fn calm() -> bool {
    !ease::reduced()
}

fn cvs(h: &Harness) -> Canvas<'_> {
    Canvas::new(&h.gfx, &h.res, 1.0)
}

fn input(multi: bool) -> (TextInput, FakeClipboard) {
    let fc = FakeClipboard::default();
    let mut i = TextInput::new(multi, &Theme::pill_dark()).with_clipboard(Box::new(fc.clone()));
    i.reduced = Some(true);
    (i, fc)
}

fn key(i: &mut TextInput, code: KeyCode, mods: Modifiers, cv: &Canvas, r: Rect) -> Response {
    i.event(&InputEvent::Key { code, mods, down: true, repeat: false }, r, cv)
}

fn press(i: &mut TextInput, code: KeyCode, cv: &Canvas, r: Rect) -> Response {
    key(i, code, Modifiers::NONE, cv, r)
}

fn typ(i: &mut TextInput, s: &str, cv: &Canvas, r: Rect) {
    for c in s.chars() {
        i.event(&InputEvent::Char(c), r, cv);
    }
}

fn down(p: Pt, clicks: u8, mods: Modifiers) -> InputEvent {
    InputEvent::MouseDown { pos: p, button: MouseButton::Left, mods, clicks }
}
fn mv(p: Pt) -> InputEvent {
    InputEvent::MouseMove { pos: p, mods: Modifiers::NONE, buttons: Buttons::NONE.with(MouseButton::Left) }
}
fn up(p: Pt) -> InputEvent {
    InputEvent::MouseUp { pos: p, button: MouseButton::Left, mods: Modifiers::NONE }
}

/// Window x of the caret at index `i`, nudged `dx` px (a click lands on the nearer side of a glyph).
fn xat(i: &TextInput, r: Rect, cv: &Canvas, idx: usize, dx: f32) -> Pt {
    let (x, y, h) = i.caret_point(r, cv, idx).expect("layout");
    (x + dx, y + h / 2.0)
}

// ---------------------------------------------------------------- typing and events

#[test]
fn typing_inserts_text_and_raises_one_changed_per_burst() {
    let h = Harness::new();
    let cv = cvs(&h);
    let (mut i, _) = input(false);
    typ(&mut i, "no focus", &cv, R);
    assert_eq!(i.text(), "", "an unfocused field ignores typing");
    i.set_focused(true);
    typ(&mut i, "Привет, мир", &cv, R);
    assert_eq!(i.text(), "Привет, мир");
    assert_eq!(i.take_events(), vec![EditEvent::FocusChanged(true), EditEvent::Changed]);
    assert!(i.take_events().is_empty());
    press(&mut i, KeyCode::Backspace, &cv, R);
    assert_eq!(i.text(), "Привет, ми");
    assert_eq!(i.take_events(), vec![EditEvent::Changed]);
    // programmatic changes raise no Changed, but reset the history
    i.set_text("fresh");
    assert!(i.take_events().is_empty());
    assert!(!i.model.can_undo());
    assert_eq!(i.selection(), (5, 5));
    // a character with Ctrl+Alt (AltGr) is text, a Ctrl+letter is a shortcut: Key events for letters are not consumed
    let r = key(&mut i, KeyCode::Char('Q'), Modifiers { ctrl: true, alt: true, ..Modifiers::NONE }, &cv, R);
    assert!(!r.consumed);
    assert!(!press(&mut i, KeyCode::Tab, &cv, R).consumed, "Tab belongs to the focus chain");
    // a Char delivered while the window has no focus is dropped
    i.event(&InputEvent::Focus(false), R, &cv);
    typ(&mut i, "x", &cv, R);
    assert_eq!(i.text(), "fresh");
    i.event(&InputEvent::Focus(true), R, &cv);
    typ(&mut i, "!", &cv, R);
    assert_eq!(i.text(), "fresh!");
}

#[test]
fn enter_escape_and_the_event_queue() {
    let h = Harness::new();
    let cv = cvs(&h);
    let (mut i, _) = input(false);
    i.set_focused(true);
    i.take_events();
    typ(&mut i, "abc", &cv, R);
    i.take_events();
    press(&mut i, KeyCode::Enter, &cv, R);
    assert_eq!(i.take_events(), vec![EditEvent::Submit]);
    assert_eq!(i.text(), "abc");
    i.select_all();
    press(&mut i, KeyCode::Escape, &cv, R);
    assert!(i.take_events().is_empty(), "Esc first collapses the selection");
    assert_eq!(i.model.sel(), None);
    press(&mut i, KeyCode::Escape, &cv, R);
    assert_eq!(i.take_events(), vec![EditEvent::Cancel]);
    // multi-line: Enter is a newline, Ctrl+Enter submits
    let (mut m, _) = input(true);
    m.set_focused(true);
    typ(&mut m, "a", &cv, RM);
    press(&mut m, KeyCode::Enter, &cv, RM);
    typ(&mut m, "b", &cv, RM);
    assert_eq!(m.text(), "a\nb");
    m.take_events();
    key(&mut m, KeyCode::Enter, Modifiers::CTRL, &cv, RM);
    assert_eq!(m.take_events(), vec![EditEvent::Submit]);
    assert_eq!(m.text(), "a\nb");
}

#[test]
fn read_only_selects_and_copies_but_does_not_edit_and_disabled_ignores_everything() {
    let h = Harness::new();
    let cv = cvs(&h);
    let (mut i, fc) = input(false);
    i.set_text("locked");
    i.read_only = true;
    i.set_focused(true);
    typ(&mut i, "x", &cv, R);
    press(&mut i, KeyCode::Backspace, &cv, R);
    key(&mut i, KeyCode::Char('V'), Modifiers::CTRL, &cv, R);
    assert_eq!(i.text(), "locked");
    key(&mut i, KeyCode::Char('A'), Modifiers::CTRL, &cv, R);
    key(&mut i, KeyCode::Char('C'), Modifiers::CTRL, &cv, R);
    assert_eq!(fc.content().as_deref(), Some("locked"));
    key(&mut i, KeyCode::Char('X'), Modifiers::CTRL, &cv, R);
    assert_eq!(i.text(), "locked", "cut is refused");
    i.enabled = false;
    let r = i.event(&down((50.0, 20.0), 1, Modifiers::NONE), R, &cv);
    assert_eq!(r, Response::NONE);
}

#[test]
fn max_length_limits_typing_and_paste_through_the_widget() {
    let h = Harness::new();
    let cv = cvs(&h);
    let (mut i, fc) = input(false);
    i.model.set_max_len(Some(6));
    i.set_focused(true);
    typ(&mut i, "abcdefghij", &cv, R);
    assert_eq!(i.text(), "abcdef");
    *fc.0.borrow_mut() = Some("XYZ".into());
    i.select_all();
    key(&mut i, KeyCode::Char('V'), Modifiers::CTRL, &cv, R);
    assert_eq!(i.text(), "XYZ");
    assert!(i.insert("12345678"));
    assert_eq!(i.text(), "XYZ123");
}

// ---------------------------------------------------------------- clipboard and undo

#[test]
fn copy_cut_paste_and_the_password_rule() {
    let h = Harness::new();
    let cv = cvs(&h);
    let (mut i, fc) = input(false);
    i.set_focused(true);
    i.set_text("hello world");
    i.set_selection(6, 11);
    key(&mut i, KeyCode::Char('C'), Modifiers::CTRL, &cv, R);
    assert_eq!(fc.content().as_deref(), Some("world"));
    assert_eq!(i.text(), "hello world");
    key(&mut i, KeyCode::Char('X'), Modifiers::CTRL, &cv, R);
    assert_eq!(i.text(), "hello ");
    key(&mut i, KeyCode::Char('V'), Modifiers::CTRL, &cv, R);
    key(&mut i, KeyCode::Char('V'), Modifiers::CTRL, &cv, R);
    assert_eq!(i.text(), "hello worldworld");
    // multi-line text pasted into a single line becomes one line; CR LF is normalised
    *fc.0.borrow_mut() = Some("a\r\nb\tc".into());
    i.select_all();
    key(&mut i, KeyCode::Char('V'), Modifiers::CTRL, &cv, R);
    assert_eq!(i.text(), "a b c");
    // Ctrl+Insert / Shift+Insert / Shift+Delete
    i.select_all();
    key(&mut i, KeyCode::Insert, Modifiers::CTRL, &cv, R);
    assert_eq!(fc.content().as_deref(), Some("a b c"));
    key(&mut i, KeyCode::Delete, Modifiers::SHIFT, &cv, R);
    assert_eq!(i.text(), "");
    key(&mut i, KeyCode::Insert, Modifiers::SHIFT, &cv, R);
    assert_eq!(i.text(), "a b c");
    // password: nothing leaves the field
    let (mut p, pc) = input(false);
    *pc.0.borrow_mut() = Some("untouched".into());
    p.password = true;
    p.set_focused(true);
    p.set_text("s3cret");
    p.select_all();
    key(&mut p, KeyCode::Char('C'), Modifiers::CTRL, &cv, R);
    key(&mut p, KeyCode::Char('X'), Modifiers::CTRL, &cv, R);
    assert_eq!(pc.content().as_deref(), Some("untouched"));
    assert_eq!(p.text(), "s3cret", "cut is refused for a password");
}

#[test]
fn undo_and_redo_by_keyboard() {
    let h = Harness::new();
    let cv = cvs(&h);
    let (mut i, _) = input(false);
    i.set_focused(true);
    typ(&mut i, "one", &cv, R);
    key(&mut i, KeyCode::Char('A'), Modifiers::CTRL, &cv, R);
    typ(&mut i, "two", &cv, R);
    assert_eq!(i.text(), "two");
    i.take_events();
    // steps: "one" | "t" replacing the selected "one" | "wo"
    key(&mut i, KeyCode::Char('Z'), Modifiers::CTRL, &cv, R);
    assert_eq!(i.text(), "t");
    assert_eq!(i.take_events(), vec![EditEvent::Changed]);
    key(&mut i, KeyCode::Char('Z'), Modifiers::CTRL, &cv, R);
    assert_eq!((i.text().as_str(), i.model.sel()), ("one", Some((0, 3))), "the selection comes back");
    key(&mut i, KeyCode::Char('Z'), Modifiers::CTRL, &cv, R);
    assert_eq!(i.text(), "");
    key(&mut i, KeyCode::Char('Y'), Modifiers::CTRL, &cv, R);
    assert_eq!(i.text(), "one");
    key(&mut i, KeyCode::Char('Z'), Modifiers::CTRL_SHIFT, &cv, R);
    assert_eq!(i.text(), "t", "Ctrl+Shift+Z redoes too");
}

// ---------------------------------------------------------------- mouse

#[test]
fn a_click_puts_the_caret_between_glyphs_and_asks_for_focus() {
    let h = Harness::new();
    let cv = cvs(&h);
    let (mut i, _) = input(false);
    i.set_text("hello world");
    let r = i.event(&down(xat(&i, R, &cv, 5, 0.5), 1, Modifiers::NONE), R, &cv);
    assert!(r.consumed && r.focus && r.capture && i.is_focused());
    assert_eq!(i.selection(), (5, 5));
    i.event(&up((0.0, 0.0)), R, &cv);
    // in the middle of the second glyph's left half and right half
    let a = xat(&i, R, &cv, 2, 0.0);
    let b = xat(&i, R, &cv, 3, 0.0);
    i.event(&down(((a.0 + b.0) / 2.0 - 1.0, a.1), 1, Modifiers::NONE), R, &cv);
    assert_eq!(i.selection().1, 2);
    i.event(&up((0.0, 0.0)), R, &cv);
    i.event(&down(((a.0 + b.0) / 2.0 + 1.0, a.1), 1, Modifiers::NONE), R, &cv);
    assert_eq!(i.selection().1, 3);
    i.event(&up((0.0, 0.0)), R, &cv);
    // far right of the text and left of it
    i.event(&down((R.right() - 3.0, 28.0), 1, Modifiers::NONE), R, &cv);
    assert_eq!(i.selection().1, 11);
    i.event(&up((0.0, 0.0)), R, &cv);
    i.event(&down((R.x + 1.0, 28.0), 1, Modifiers::NONE), R, &cv);
    assert_eq!(i.selection().1, 0);
    // a press outside does nothing
    let r = i.event(&down((500.0, 500.0), 1, Modifiers::NONE), R, &cv);
    assert_eq!(r, Response::NONE);
    i.event(&up((0.0, 0.0)), R, &cv);
    // hovering shows the I-beam, without consuming
    let r = i.event(&InputEvent::MouseMove { pos: (50.0, 20.0), mods: Modifiers::NONE, buttons: Buttons::NONE }, R, &cv);
    assert_eq!((r.cursor, r.consumed), (Some(Cursor::Text), false));
}

#[test]
fn drag_selects_and_shift_click_extends() {
    let h = Harness::new();
    let cv = cvs(&h);
    let (mut i, _) = input(false);
    i.set_text("hello world");
    i.event(&down(xat(&i, R, &cv, 1, 0.5), 1, Modifiers::NONE), R, &cv);
    let r = i.event(&mv(xat(&i, R, &cv, 4, 0.5)), R, &cv);
    assert!(r.capture && r.consumed);
    assert_eq!(i.selection(), (1, 4));
    // dragging far outside the rectangle still works (capture) and clamps to the ends
    i.event(&mv((9999.0, 500.0)), R, &cv);
    assert_eq!(i.selection(), (1, 11));
    i.event(&mv((-500.0, 0.0)), R, &cv);
    assert_eq!(i.selection(), (1, 0));
    let r = i.event(&up((0.0, 0.0)), R, &cv);
    assert!(r.consumed);
    assert!(!i.event(&mv(xat(&i, R, &cv, 4, 0.5)), R, &cv).capture, "the drag is over");
    // shift+click keeps the anchor
    i.set_selection(2, 2);
    i.event(&down(xat(&i, R, &cv, 7, 0.5), 1, Modifiers::SHIFT), R, &cv);
    assert_eq!(i.selection(), (2, 7));
}

#[test]
fn double_click_selects_a_word_triple_click_the_line_and_a_word_drag_extends_by_words() {
    let h = Harness::new();
    let cv = cvs(&h);
    let (mut i, _) = input(false);
    i.set_text("привет мир тест");
    i.event(&down(xat(&i, R, &cv, 8, 0.5), 2, Modifiers::NONE), R, &cv);
    assert_eq!(i.model.selected_text(), "мир");
    // drag to the third word: whole words, from the first selected one
    i.event(&mv(xat(&i, R, &cv, 13, 0.5)), R, &cv);
    assert_eq!(i.model.selected_text(), "мир тест");
    i.event(&mv(xat(&i, R, &cv, 2, 0.5)), R, &cv);
    assert_eq!(i.model.selected_text(), "привет мир");
    i.event(&up((0.0, 0.0)), R, &cv);
    i.event(&down(xat(&i, R, &cv, 3, 0.5), 3, Modifiers::NONE), R, &cv);
    assert_eq!(i.model.selected_text(), "привет мир тест", "single line: everything");
    i.event(&up((0.0, 0.0)), R, &cv);
    // a password never reveals word boundaries
    i.password = true;
    i.event(&down(xat(&i, R, &cv, 3, 0.5), 1, Modifiers::NONE), R, &cv);
    i.event(&up((0.0, 0.0)), R, &cv);
    i.event(&down(xat(&i, R, &cv, 3, 0.5), 2, Modifiers::NONE), R, &cv);
    assert_eq!(i.model.sel(), Some((0, 15)));
    // multi-line triple click: the hard line
    let (mut m, _) = input(true);
    m.set_text("one\ntwo words\nthree");
    m.event(&down(xat(&m, RM, &cv, 6, 0.5), 3, Modifiers::NONE), RM, &cv);
    assert_eq!(m.model.selected_text(), "two words\n");
}

#[test]
fn right_click_emits_a_context_menu_request_and_keeps_an_existing_selection() {
    let h = Harness::new();
    let cv = cvs(&h);
    let (mut i, _) = input(false);
    i.set_text("hello world");
    i.set_selection(0, 5);
    i.take_events();
    let p = xat(&i, R, &cv, 2, 0.5);
    let r = i.event(&InputEvent::MouseDown { pos: p, button: MouseButton::Right, mods: Modifiers::NONE, clicks: 1 }, R, &cv);
    assert!(r.consumed && r.focus);
    assert_eq!(i.model.sel(), Some((0, 5)));
    assert!(i.take_events().contains(&EditEvent::ContextMenu(p)));
    let q = xat(&i, R, &cv, 9, 0.5);
    i.event(&InputEvent::MouseDown { pos: q, button: MouseButton::Right, mods: Modifiers::NONE, clicks: 1 }, R, &cv);
    assert_eq!(i.model.sel(), None, "outside the selection the caret moves there");
}

// ---------------------------------------------------------------- keyboard movement

#[test]
fn caret_keys_move_by_cluster_word_and_line() {
    let h = Harness::new();
    let cv = cvs(&h);
    let (mut i, _) = input(false);
    i.set_focused(true);
    i.set_text("раз два e\u{301}x");
    press(&mut i, KeyCode::Home, &cv, R);
    assert_eq!(i.selection(), (0, 0));
    key(&mut i, KeyCode::Right, Modifiers::CTRL, &cv, R);
    assert_eq!(i.selection().1, 4);
    key(&mut i, KeyCode::Right, Modifiers::SHIFT, &cv, R);
    assert_eq!(i.selection(), (4, 5));
    press(&mut i, KeyCode::End, &cv, R);
    assert_eq!(i.selection(), (11, 11));
    press(&mut i, KeyCode::Left, &cv, R);
    press(&mut i, KeyCode::Left, &cv, R);
    assert_eq!(i.selection().1, 8, "left over x then over e + accent as one step");
    key(&mut i, KeyCode::Left, Modifiers::CTRL, &cv, R);
    assert_eq!(i.selection().1, 4);
    key(&mut i, KeyCode::Home, Modifiers::SHIFT, &cv, R);
    assert_eq!(i.model.selected_text(), "раз ");
    press(&mut i, KeyCode::Left, &cv, R);
    assert_eq!(i.selection(), (0, 0), "left collapses to the start of the selection");
    press(&mut i, KeyCode::Down, &cv, R);
    assert_eq!(i.selection().1, 11, "single line: Down goes to the end");
    press(&mut i, KeyCode::Up, &cv, R);
    assert_eq!(i.selection().1, 0);
    key(&mut i, KeyCode::Backspace, Modifiers::CTRL, &cv, R);
    key(&mut i, KeyCode::End, Modifiers::NONE, &cv, R);
    key(&mut i, KeyCode::Backspace, Modifiers::CTRL, &cv, R);
    assert_eq!(i.text(), "раз два ");
    key(&mut i, KeyCode::Backspace, Modifiers::CTRL, &cv, R);
    assert_eq!(i.text(), "раз ");
    i.set_selection(0, 0);
    key(&mut i, KeyCode::Delete, Modifiers::CTRL, &cv, R);
    assert_eq!(i.text(), "");
}

#[test]
fn multi_line_vertical_movement_keeps_the_column() {
    let h = Harness::new();
    let cv = cvs(&h);
    let (mut m, _) = input(true);
    m.set_focused(true);
    m.set_text("aaaaaaaa\nbb\naaaaaaaa");
    m.set_selection(8, 8); // end of line 1 (column 8)
    press(&mut m, KeyCode::Down, &cv, RM);
    assert_eq!(m.selection().1, 11, "the short line: its end");
    press(&mut m, KeyCode::Down, &cv, RM);
    assert_eq!(m.selection().1, 20, "back to column 8: the remembered x");
    press(&mut m, KeyCode::Up, &cv, RM);
    press(&mut m, KeyCode::Up, &cv, RM);
    assert_eq!(m.selection().1, 8);
    press(&mut m, KeyCode::Up, &cv, RM);
    assert_eq!(m.selection().1, 0, "off the first line: the start");
    key(&mut m, KeyCode::End, Modifiers::CTRL, &cv, RM);
    assert_eq!(m.selection().1, 20);
    press(&mut m, KeyCode::Down, &cv, RM);
    assert_eq!(m.selection().1, 20, "off the last line: the end");
    // Home / End are per line
    m.set_selection(14, 14);
    press(&mut m, KeyCode::Home, &cv, RM);
    assert_eq!(m.selection().1, 12);
    press(&mut m, KeyCode::End, &cv, RM);
    assert_eq!(m.selection().1, 20);
    m.set_selection(3, 3);
    key(&mut m, KeyCode::End, Modifiers::SHIFT, &cv, RM);
    assert_eq!(m.model.selected_text(), "aaaaa");
    // shift+Down selects across lines
    m.set_selection(2, 2);
    key(&mut m, KeyCode::Down, Modifiers::SHIFT, &cv, RM);
    assert_eq!(m.selection(), (2, 11));
}

#[test]
fn soft_wrapped_lines_have_their_own_home_and_end() {
    let h = Harness::new();
    let cv = cvs(&h);
    let (mut m, _) = input(true);
    m.set_focused(true);
    let words = "alpha beta gamma delta epsilon zeta eta theta iota kappa";
    m.set_text(words);
    let narrow = Rect::new(10.0, 10.0, 110.0, 120.0);
    m.update(narrow, &cv);
    let (_, y0, _) = m.caret_point(narrow, &cv, 0).unwrap();
    let (_, ye, _) = m.caret_point(narrow, &cv, words.chars().count()).unwrap();
    assert!(ye > y0 + 20.0, "the text wraps into several lines");
    m.set_selection(2, 2);
    press(&mut m, KeyCode::End, &cv, narrow);
    let end1 = m.selection().1;
    assert!(end1 > 2 && end1 < words.len(), "End stops at the end of the first visual line ({end1})");
    assert_eq!(words.chars().nth(end1 - 1), Some('a'), "before the space the line wraps at");
    press(&mut m, KeyCode::Right, &cv, narrow);
    press(&mut m, KeyCode::Home, &cv, narrow);
    assert_eq!(m.selection().1, end1 + 1, "Home on the second visual line goes to its own start");
    key(&mut m, KeyCode::Home, Modifiers::CTRL, &cv, narrow);
    assert_eq!(m.selection().1, 0);
    // PageDown moves a window of lines
    press(&mut m, KeyCode::PageDown, &cv, narrow);
    assert!(m.selection().1 > end1);
}

#[test]
fn the_caret_after_a_final_newline_is_on_the_next_line() {
    let h = Harness::new();
    let cv = cvs(&h);
    let (mut m, _) = input(true);
    m.set_text("ab\n");
    let (_, y0, lh) = m.caret_point(RM, &cv, 0).unwrap();
    let (x1, y1, _) = m.caret_point(RM, &cv, 3).unwrap();
    assert!(y1 >= y0 + lh - 1.0, "second line: {y1} vs {y0} + {lh}");
    assert!((x1 - (RM.x + m.pad.0)).abs() < 1.0, "at the left edge");
    m.set_text("ab");
    let (_, y2, _) = m.caret_point(RM, &cv, 2).unwrap();
    assert!((y2 - y0).abs() < 1.0);
}

// ---------------------------------------------------------------- scrolling

#[test]
fn a_long_single_line_scrolls_to_keep_the_caret_in_view() {
    let h = Harness::new();
    let cv = cvs(&h);
    let (mut i, _) = input(false);
    i.set_focused(true);
    i.set_text(&"слово ".repeat(40));
    i.update(R, &cv);
    for _ in 0..30 {
        i.step(0.016);
    }
    let (cx, _, _) = i.caret_point(R, &cv, i.model.len()).unwrap();
    assert!(i.sx.offset() > 100.0);
    assert!(cx <= R.right() - i.pad.0 + 1.0 && cx >= R.x + i.pad.0 - 1.0, "caret at {cx} inside {:?}", R);
    // Home scrolls back
    press(&mut i, KeyCode::Home, &cv, R);
    for _ in 0..60 {
        i.step(0.016);
    }
    assert_eq!(i.sx.offset(), 0.0);
    let (cx, _, _) = i.caret_point(R, &cv, 0).unwrap();
    assert!((cx - (R.x + i.pad.0)).abs() < 0.5);
    // dragging past the right edge scrolls along
    i.event(&down(xat(&i, R, &cv, 3, 0.5), 1, Modifiers::NONE), R, &cv);
    i.event(&mv((R.right() + 40.0, 28.0)), R, &cv);
    for _ in 0..30 {
        i.step(0.016);
    }
    assert!(i.sx.offset() > 0.0);
}

#[test]
fn a_multi_line_field_scrolls_with_the_caret_and_the_wheel() {
    let h = Harness::new();
    let cv = cvs(&h);
    let (mut m, _) = input(true);
    m.set_focused(true);
    let text: Vec<String> = (0..30).map(|n| format!("строка {n}")).collect();
    m.set_text(&text.join("\n"));
    m.update(RM, &cv);
    for _ in 0..40 {
        m.step(0.016);
    }
    let (_, y, hh) = m.caret_point(RM, &cv, m.model.len()).unwrap();
    assert!(y >= RM.y && y + hh <= RM.bottom() + 0.5, "caret row visible: {y}");
    assert!(m.sy.offset() > 100.0);
    // a wheel notch up moves the content (and is consumed), a notch down at the end is not
    let top = m.sy.offset();
    let wheel = |dy: f32| InputEvent::Wheel { pos: (50.0, 50.0), delta: (0.0, dy), mods: Modifiers::NONE, precise: false };
    assert!(m.event(&wheel(-100.0), RM, &cv).consumed);
    for _ in 0..40 {
        m.step(0.016);
    }
    assert!(m.sy.offset() < top - 50.0);
    m.sy.scroll_to(f32::MAX, false);
    assert!(!m.event(&wheel(100.0), RM, &cv).consumed);
    // typing brings the caret back into view
    typ(&mut m, "x", &cv, RM);
    for _ in 0..40 {
        m.step(0.016);
    }
    let (_, y, hh) = m.caret_point(RM, &cv, m.model.len()).unwrap();
    assert!(y >= RM.y && y + hh <= RM.bottom() + 0.5);
}

#[test]
fn content_height_follows_the_lines() {
    let h = Harness::new();
    let cv = cvs(&h);
    let (mut m, _) = input(true);
    let one = m.content_height(200.0, &cv);
    m.set_text("a\nb\nc");
    let three = m.content_height(200.0, &cv);
    assert!(three > one + 30.0, "{one} -> {three}");
    m.set_text(&"word ".repeat(60));
    assert!(m.content_height(100.0, &cv) > m.content_height(400.0, &cv));
    let (s, _) = input(false);
    assert!(s.content_height(200.0, &cv) >= 14.0 && s.content_height(200.0, &cv) < 30.0);
}

// ---------------------------------------------------------------- password, blink, animation

#[test]
fn the_displayed_text_of_a_password_is_one_bullet_per_cluster() {
    let chars: Vec<char> = "pa👨\u{200D}👩ss".chars().collect();
    let d = Disp::new(&chars, true);
    assert_eq!(d.s.chars().count(), 5);
    assert!(d.s.chars().all(|c| c == '\u{2022}'));
    assert_eq!(d.at(0), 0);
    assert_eq!(d.at(2), 2);
    assert_eq!(d.at(4), 2, "inside the emoji cluster: its bullet");
    assert_eq!(d.at(5), 3);
    assert_eq!(d.at(chars.len()), 5);
    assert_eq!(d.char_of(3), 5);
    assert_eq!(d.char_of(2), 2);
    // plain text: UTF-16 offsets (an emoji is two units)
    let p = Disp::new(&['a', '😀', 'b'], false);
    assert_eq!(p.u16_at, vec![0, 1, 3, 4]);
    assert_eq!(p.char_of(3), 2);
}

#[test]
fn the_caret_blinks_with_a_period_of_a_second_and_is_solid_under_reduced_motion() {
    if !calm() {
        return;
    }
    let (mut i, _) = input(false);
    i.reduced = Some(false);
    assert_eq!(i.blink_wake(), None, "no wake-up while unfocused");
    i.set_focused(true);
    assert!(i.caret_visible());
    assert!((i.blink_wake().unwrap() - BLINK_HALF).abs() < 1e-4);
    i.set_blink_phase(0.60);
    assert!(!i.caret_visible());
    assert!((i.blink_wake().unwrap() - (BLINK_HALF - 0.07)).abs() < 1e-3);
    i.step(0.5);
    assert!(i.caret_visible(), "{}", i.blink);
    i.set_blink_phase(0.6);
    i.reduced = Some(true);
    assert!(i.caret_visible() && i.blink_wake().is_none());
}

#[test]
fn focus_and_placeholder_animate_over_120_ms() {
    if !calm() {
        return;
    }
    let (mut i, _) = input(false);
    i.reduced = Some(false);
    i.placeholder = "Search".into();
    assert_eq!(i.focus_t.value(), 0.0);
    i.set_focused(true);
    assert!(i.busy());
    assert_eq!(i.focus_t.value(), 0.0);
    assert!(i.step(0.06));
    let mid = i.focus_t.value();
    assert!(mid > 0.3 && mid < 1.0, "{mid}");
    assert!(!i.step(0.1));
    assert_eq!(i.focus_t.value(), 1.0);
    assert!(!i.busy());
    // the placeholder fades out when text arrives and back in when it is emptied
    i.set_text("x");
    assert!(i.busy() && i.ph_t.value() == 1.0);
    i.step(0.2);
    assert_eq!(i.ph_t.value(), 0.0);
    i.set_text("");
    i.step(0.2);
    assert_eq!(i.ph_t.value(), 1.0);
    // reduced motion: instant
    let (mut r, _) = input(false);
    r.set_focused(true);
    assert_eq!(r.focus_t.value(), 1.0);
    assert!(!r.busy());
}

// ---------------------------------------------------------------- pixels

fn field_px(img: &crate::testkit::Image, x: f32, y: f32) -> [u8; 4] {
    img.at(x, y)
}

#[test]
fn the_focused_field_has_an_accent_border_and_the_unfocused_one_does_not() {
    let h = Harness::new();
    let theme = Theme::pill_dark();
    let (mut i, _) = input(false);
    i.set_text("hello");
    let shot = |i: &mut TextInput| {
        h.render(240, 60, 1.0, theme.bg, |cv| {
            i.update(R, cv);
            i.draw(cv, R, &theme)
        })
    };
    let plain = shot(&mut i);
    let f = theme.field;
    let want = |c: Color| [(c.r * 255.0).round() as u8, (c.g * 255.0).round() as u8, (c.b * 255.0).round() as u8, 255];
    assert_eq!(field_px(&plain, R.x + 0.5, R.cy()), want(f), "no border without focus");
    i.set_focused(true);
    let focused = shot(&mut i);
    assert_eq!(field_px(&focused, R.x + 0.5, R.cy()), want(theme.accent), "1 px accent inside the edge");
    let outer = field_px(&focused, R.x - 0.5, R.cy());
    assert!(outer[2] > 20 && outer != field_px(&plain, R.x - 0.5, R.cy()), "the soft outer ring: {outer:?}");
    assert!(plain.diff(&focused, 0) > 100);
    // disabled is dimmer
    let mut d = input(false).0;
    d.set_text("hello");
    d.enabled = false;
    let dim = shot(&mut d);
    assert!(plain.diff(&dim, 0) > 0, "a disabled field is drawn faded");
}

#[test]
fn the_selection_is_painted_in_the_accent_and_the_caret_follows_the_blink_phase() {
    let h = Harness::new();
    let theme = Theme::pill_dark();
    let (mut i, _) = input(false);
    i.set_focused(true);
    i.set_text("hello world "); // the trailing space keeps glyph pixels out of the caret column
    i.select_all();
    let sel = h.render(240, 60, 1.0, theme.bg, |cv| {
        i.update(R, cv);
        i.draw(cv, R, &theme)
    });
    let c = theme.field.mix(theme.accent, 0.35);
    let near = |p: [u8; 4]| (p[0] as f32 / 255.0 - c.r).abs() < 0.03 && (p[1] as f32 / 255.0 - c.g).abs() < 0.03 && (p[2] as f32 / 255.0 - c.b).abs() < 0.03;
    assert!(sel.count(Some(R.inset(3.0)), near) > 400, "selection fill pixels: {}", sel.count(Some(R.inset(3.0)), near));
    // no selection: a caret column of text-coloured pixels at the end of the text, visible in the first half period only
    i.set_selection(12, 12);
    let (cx, cy, ch) = i.caret_point(R, &Canvas::new(&h.gfx, &h.res, 1.0), 12).unwrap();
    let col = Rect::new(cx - 1.0, cy, 3.0, ch);
    let bright = |p: [u8; 4]| p[0] > 200 && p[1] > 200 && p[2] > 200;
    let on = h.render(240, 60, 1.0, theme.bg, |cv| {
        i.set_blink_phase(0.1);
        i.draw(cv, R, &theme)
    });
    assert!(on.count(Some(col), bright) >= 10, "caret drawn: {}", on.count(Some(col), bright));
    if !calm() {
        return;
    }
    let mut j = input(false).0;
    j.reduced = Some(false);
    j.set_text("hello world ");
    j.set_focused(true);
    j.step(0.3); // the focus ring finishes fading in
    j.set_blink_phase(0.7);
    let off = h.render(240, 60, 1.0, theme.bg, |cv| j.draw(cv, R, &theme));
    assert_eq!(off.count(Some(col), bright), 0, "caret hidden in the second half period");
}

#[test]
fn the_placeholder_shows_only_while_the_field_is_empty_and_a_password_draws_identical_bullets() {
    let h = Harness::new();
    let theme = Theme::pill_dark();
    let (mut i, _) = input(false);
    i.placeholder = "Search".into();
    let shot = |i: &mut TextInput| {
        h.render(240, 60, 1.0, theme.bg, |cv| {
            i.update(R, cv);
            i.draw(cv, R, &theme)
        })
    };
    let empty = shot(&mut i);
    let dim = |p: [u8; 4]| p[0] > 70 && p[0] < 140;
    assert!(empty.count(Some(R.inset(3.0)), dim) > 20, "placeholder glyphs");
    i.set_text("x");
    let typed = shot(&mut i);
    let bright = |p: [u8; 4]| p[0] > 200;
    assert!(typed.count(Some(R.inset(3.0)), bright) > 5);
    // two different passwords of the same length look the same
    let mut a = input(false).0;
    a.password = true;
    a.set_text("abcdef");
    let mut b = input(false).0;
    b.password = true;
    b.set_text("uvwxyz");
    let (ia, ib) = (shot(&mut a), shot(&mut b));
    assert_eq!(ia.diff(&ib, 0), 0, "nothing of the text is visible");
    assert!(ia.count(Some(R.inset(3.0)), bright) > 30);
}

#[test]
fn review_images_of_every_state() {
    let h = Harness::new();
    for (name, theme) in [("dark", Theme::pill_dark()), ("light", Theme::pill_light()), ("settings", Theme::settings_dark())] {
        let mk = |multi: bool| {
            let fc = FakeClipboard::default();
            let mut i = TextInput::new(multi, &theme).with_clipboard(Box::new(fc));
            i.reduced = Some(true);
            i
        };
        let mut empty = mk(false);
        empty.placeholder = "Search".into();
        let mut filled = mk(false);
        filled.set_text("Привет, мир! Hello");
        let mut focused = mk(false);
        focused.set_text("Focused field");
        focused.set_focused(true);
        let mut selected = mk(false);
        selected.set_text("Selected text here");
        selected.set_focused(true);
        selected.set_selection(9, 13);
        let mut pass = mk(false);
        pass.password = true;
        pass.set_text("hunter2");
        pass.set_focused(true);
        let mut long = mk(false);
        long.set_text(&"scrolled to the end of a long line ".repeat(3));
        long.set_focused(true);
        let mut off = mk(false);
        off.set_text("Disabled");
        off.enabled = false;
        let mut multi = mk(true);
        multi.set_text("Первая строка\nвторая строка, достаточно длинная чтобы перенестись\n\n😀 e\u{301} третья");
        multi.set_focused(true);
        multi.set_selection(14, 24);
        let mut scrolled = mk(true);
        scrolled.set_text(&(0..14).map(|n| format!("line {n}")).collect::<Vec<_>>().join("\n"));
        scrolled.set_focused(true);
        scrolled.bar.show();
        for scale in [1.0f32, 2.0] {
            let img = h.render(240, 540, scale, theme.bg, |cv| {
                let mut y = 10.0;
                for i in [&mut empty, &mut filled, &mut focused, &mut selected, &mut pass, &mut long, &mut off] {
                    let r = Rect::new(20.0, y, 200.0, theme.m.field_h);
                    i.update(r, cv);
                    i.set_blink_phase(0.1);
                    i.draw(cv, r, &theme);
                    y += theme.m.field_h + 12.0;
                }
                for (i, hgt) in [(&mut multi, 96.0), (&mut scrolled, 80.0)] {
                    let r = Rect::new(20.0, y, 200.0, hgt);
                    i.update(r, cv);
                    i.set_blink_phase(0.1);
                    i.draw(cv, r, &theme);
                    y += hgt + 12.0;
                }
            });
            assert!(img.count(None, |p| p[0] > 60) > 500, "something was drawn");
            img.save_png(&h, img_dir().join(format!("edit_{name}_{}x.png", scale as u32))).ok();
        }
    }
}
