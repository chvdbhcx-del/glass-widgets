//! `Select`: a dropdown. The closed control looks like a text field with a chevron (`select.field` of Settings); clicking it opens a list panel built on
//! the context-menu model of `menu.rs` (`MenuPanel`: keyboard walk, hover, press-then-release picking, placement inside the work area).
//!
//! * open: the panel grows from its top edge (scale .94 -> 1 and a fade, 140 ms ease-out cubic, the model's own timing) while the chevron turns 180 degrees;
//! * the highlighted row's pill follows the pointer / keyboard with a 100 ms fade (a hover ramp per row);
//! * close: the panel fades out in 100 ms; Escape, a click outside, Tab and picking an option all close it;
//! * the chosen option carries a check mark; arrow keys on a FOCUSED closed select change the value without opening it (as native selects do).
//!
//! **Drawing order**: the closed control is drawn with [`Widget::draw`]; the open panel must float above everything, so call [`Select::draw_popup`] last.
//! While [`Select::is_open`] the host routes all input to the select (the panel may extend outside `rect`).
use super::{Interact, Widget};
use crate::canvas::{mul, Canvas};
use crate::color::Color;
use crate::ease::{curves, dur, reduced, Lerp, Tween};
use crate::geom::Rect;
use crate::icons::Glyph;
use crate::input::{Cursor, InputEvent, KeyCode, MouseButton, Response};
use crate::menu::{Entry, Item, MAct, MenuPanel, CHECK_W, PAD, ROW_PAD_X};
use crate::motion::HoverRamps;
use crate::text::Style;
use crate::textfield::Key;
use crate::theme::Theme;
use std::cell::RefCell;
use windows::Foundation::Numerics::Matrix3x2;

const GAP: f32 = 4.0;
/// radius of the open panel (Pill's menus use 12; the model's glass panel uses 18)
const PANEL_R: f32 = 12.0;

struct Open {
    panel: MenuPanel,
    ramps: HoverRamps,
}

pub struct Select {
    options: Vec<String>,
    selected: Option<usize>,
    /// shown (dim) when nothing is selected; the caller supplies the string
    pub placeholder: String,
    open: Option<Open>,
    it: Interact,
    chevron: Tween<f32>,
    work: Rect,
    /// label widths measured by `natural_size` / `draw`, in the text style of the panel
    widths: RefCell<Vec<f32>>,
}

fn field_style(th: &Theme) -> Style {
    Style::new(th.m.btn_fs, 400)
}

impl Select {
    pub fn new(options: &[&str], selected: Option<usize>) -> Select {
        Select {
            options: options.iter().map(|s| s.to_string()).collect(),
            selected: selected.filter(|&i| i < options.len()),
            placeholder: String::new(),
            open: None,
            it: Interact::new(),
            chevron: Tween::at_rest(0.0),
            work: Rect::new(0.0, 0.0, 4000.0, 4000.0),
            widths: RefCell::new(Vec::new()),
        }
    }

    pub fn with_placeholder(mut self, p: &str) -> Select {
        self.placeholder = p.into();
        self
    }

    pub fn selected(&self) -> Option<usize> {
        self.selected
    }

    pub fn selected_label(&self) -> Option<&str> {
        self.selected.map(|i| self.options[i].as_str())
    }

    pub fn set_selected(&mut self, i: Option<usize>) {
        self.selected = i.filter(|&k| k < self.options.len());
    }

    /// The rectangle the open panel must stay inside (the window's client area). Default: unbounded.
    pub fn set_work_area(&mut self, r: Rect) {
        self.work = r;
    }

    pub fn is_open(&self) -> bool {
        self.open.as_ref().is_some_and(|o| !o.panel.closing)
    }

    /// The panel is on screen (open or fading out): keep drawing the popup.
    pub fn popup_visible(&self) -> bool {
        self.open.is_some()
    }

    /// The panel's rectangle in window coordinates (for damage tracking), if shown.
    pub fn popup_bounds(&self) -> Option<Rect> {
        self.open.as_ref().map(|o| o.panel.bounds())
    }

    fn text_w(&self) -> impl Fn(&str) -> f32 + '_ {
        move |s: &str| {
            let w = self.widths.borrow();
            match self.options.iter().position(|o| o == s) {
                Some(i) if i < w.len() => w[i],
                _ => s.chars().count() as f32 * 7.0,
            }
        }
    }

    pub fn open(&mut self, rect: Rect) {
        if self.open.is_some() || self.options.is_empty() {
            return;
        }
        let entries: Vec<Entry> = self.options.iter().enumerate().map(|(i, o)| Entry::Item(Item::new(i, o).checked(self.selected == Some(i)))).collect();
        let mut panel = {
            let w = self.text_w();
            MenuPanel::new(entries, (rect.x, rect.bottom() + GAP), self.work, &w)
        };
        // the panel is at least as wide as the field; keep it inside the work area
        let want = rect.w.max(panel.root.panel.w);
        if want > panel.root.panel.w {
            let pw = want;
            let x = panel.root.panel.x.min(self.work.right() - 8.0 - pw).max(self.work.x + 8.0);
            let dx = x - panel.root.panel.x;
            panel.root.panel.w = pw;
            panel.root.panel.x = x;
            for r in &mut panel.root.rows {
                r.x += dx;
                r.w = pw - 2.0 * PAD;
            }
        }
        if let Some(i) = self.selected {
            panel.hot[0] = Some(i);
        }
        let n = self.options.len();
        self.open = Some(Open { panel, ramps: HoverRamps::new(n) });
        self.chevron.retarget(1.0, dur::MED, curves::EASE_OUT);
    }

    pub fn close(&mut self) {
        if let Some(o) = &mut self.open {
            o.panel.closing = true;
        }
        self.chevron.retarget(0.0, dur::MED, curves::EASE_OUT);
    }

    fn pick(&mut self, id: usize) {
        self.selected = Some(id);
        self.close();
    }

    fn key_of(code: KeyCode) -> Option<Key> {
        Some(match code {
            KeyCode::Up => Key::Up,
            KeyCode::Down => Key::Down,
            KeyCode::Home => Key::Home,
            KeyCode::End => Key::End,
            KeyCode::Enter => Key::Enter,
            KeyCode::Space => Key::Char(' '),
            KeyCode::Escape => Key::Esc,
            _ => return None,
        })
    }

    fn open_event(&mut self, ev: &InputEvent, rect: Rect) -> Response {
        let Some(o) = self.open.as_mut() else { return Response::NONE };
        if o.panel.closing {
            // fading out: swallow input so a double click cannot reopen it mid-fade
            return Response { consumed: matches!(ev, InputEvent::MouseDown { .. }), ..Response::NONE };
        }
        let mut r = Response::consumed();
        r.redraw = true;
        let act = match ev {
            InputEvent::MouseMove { pos, .. } => {
                r.cursor = Some(Cursor::Hand);
                o.panel.mouse_move(pos.0, pos.1)
            }
            InputEvent::MouseDown { pos, button: MouseButton::Left, .. } => {
                // a press on the field itself toggles it shut; anywhere else outside closes via the model
                if rect.contains(pos.0, pos.1) && !o.panel.bounds().contains(pos.0, pos.1) {
                    MAct::Close
                } else {
                    o.panel.mouse_down(pos.0, pos.1)
                }
            }
            InputEvent::MouseUp { pos, button: MouseButton::Left, .. } => o.panel.mouse_up(pos.0, pos.1),
            InputEvent::Key { code: KeyCode::Tab, down: true, .. } => MAct::Close,
            InputEvent::Key { code, down: true, .. } => match Select::key_of(*code) {
                Some(k) => o.panel.key(k),
                None => MAct::None,
            },
            _ => {
                r.consumed = false;
                MAct::None
            }
        };
        match act {
            MAct::Pick(id) => {
                self.pick(id);
                r.changed = true;
            }
            MAct::Close => self.close(),
            _ => {}
        }
        r
    }
}

fn rotation_about(c: (f32, f32), deg: f32) -> Matrix3x2 {
    let (s, k) = deg.to_radians().sin_cos();
    Matrix3x2 { M11: k, M12: s, M21: -s, M22: k, M31: c.0 - c.0 * k + c.1 * s, M32: c.1 - c.0 * s - c.1 * k }
}

impl Widget for Select {
    fn event(&mut self, ev: &InputEvent, rect: Rect) -> Response {
        if self.open.is_some() {
            return self.open_event(ev, rect);
        }
        let (mut r, p) = self.it.pointer(ev, |q| rect.contains(q.0, q.1));
        r.changed = false;
        if p.click {
            self.open(rect);
            r.redraw = true;
        }
        if self.it.focused() && self.it.enabled {
            if let Some((code, mods, _)) = ev.key_down() {
                match code {
                    KeyCode::Enter | KeyCode::Space => {
                        self.open(rect);
                        r = Response { consumed: true, redraw: true, ..r };
                    }
                    KeyCode::Down if mods.alt => {
                        self.open(rect);
                        r = Response { consumed: true, redraw: true, ..r };
                    }
                    KeyCode::Up | KeyCode::Down => {
                        let n = self.options.len();
                        if n > 0 {
                            let cur = self.selected;
                            let next = match (code, cur) {
                                (KeyCode::Down, Some(i)) => (i + 1).min(n - 1),
                                (KeyCode::Down, None) => 0,
                                (_, Some(i)) => i.saturating_sub(1),
                                (_, None) => n - 1,
                            };
                            self.selected = Some(next);
                            r = Response { consumed: true, redraw: true, changed: cur != Some(next), ..r };
                        }
                    }
                    _ => {}
                }
            }
        }
        r
    }

    fn step(&mut self, dt: f32) -> bool {
        let mut busy = self.it.step(dt);
        busy |= self.chevron.step(dt);
        let mut done = false;
        let mut layout = false;
        if let Some(o) = &mut self.open {
            let (b, l) = o.panel.step(dt, reduced());
            busy |= b;
            layout |= l;
            let hot = o.panel.hot[0];
            o.ramps.step(dt, 0.1, 0.14, |i| Some(i) == hot, |_| {});
            busy |= o.ramps.busy(|i| Some(i) == hot);
            done = o.panel.closing && o.panel.t <= 0.0;
        }
        let _ = layout;
        if done {
            self.open = None;
        }
        busy
    }

    fn draw(&self, cv: &Canvas, th: &Theme, rect: Rect) {
        let m = &th.m;
        // measure the options once (used for the panel width)
        if self.widths.borrow().len() != self.options.len() {
            let st = field_style(th);
            *self.widths.borrow_mut() = self.options.iter().map(|o| cv.text_width(o, st).ceil()).collect();
        }
        let h = self.it.hover();
        let open_t = self.chevron.value().clamp(0.0, 1.0);
        cv.with_opacity(if self.it.enabled { 1.0 } else { 0.45 }, |cv| {
            cv.fill_rr(rect, m.field_r, Color::lerp(th.field, th.card_hover, h));
            // `.field:focus`: an accent line inside; an open select keeps it
            let focus = open_t.max(if self.it.focused() { 1.0 } else { 0.0 });
            if focus > 0.0 {
                cv.stroke_rr(rect, m.field_r, 1.0, th.accent.fade(0.45 * focus));
            }
            let st = field_style(th);
            let tr = Rect::new(rect.x + m.field_pad_x, rect.y, (rect.w - m.field_pad_x * 2.0 - 18.0).max(0.0), rect.h);
            match self.selected_label() {
                Some(l) => cv.text(tr, l, st.ellipsis(), th.text),
                None => cv.text(tr, &self.placeholder, st.ellipsis(), th.faint),
            }
            let cc = (rect.right() - m.field_pad_x - 5.0, rect.cy());
            cv.with_transform(rotation_about(cc, 180.0 * curves::EASE_OUT.at(open_t)), |cv| {
                cv.glyph(Glyph::ChevDown, Rect::new(cc.0 - 6.0, cc.1 - 6.0, 12.0, 12.0), th.icon.mix(th.text, h));
            });
            cv.focus_ring(rect, m.field_r, th.accent, self.it.ring());
        });
    }

    fn natural_size(&self, cv: &Canvas, th: &Theme) -> (f32, f32) {
        let st = field_style(th);
        let w = self.options.iter().map(|o| cv.text_width(o, st)).fold(0.0, f32::max).ceil();
        (w + th.m.field_pad_x * 2.0 + 18.0, th.m.field_h)
    }

    fn set_focus(&mut self, focused: bool, ring: bool) {
        self.it.set_focus(focused, ring);
        if !focused && self.is_open() {
            self.close();
        }
    }

    fn focusable(&self) -> bool {
        self.it.enabled
    }

    fn set_enabled(&mut self, on: bool) {
        self.it.set_enabled(on);
        if !on {
            self.close();
        }
    }
}

impl Select {
    /// Draw the open panel (call after everything else). Nothing when closed.
    pub fn draw_popup(&self, cv: &Canvas, th: &Theme) {
        let Some(o) = &self.open else { return };
        let k = o.panel.eased();
        if k <= 0.0 {
            return;
        }
        let p = &o.panel;
        let r = p.root.panel;
        // grow from the edge nearest the field: scale .94 -> 1 around the model's origin corner
        let s = 0.94 + 0.06 * k;
        let grow = Matrix3x2 { M11: s, M12: 0.0, M21: 0.0, M22: s, M31: p.root.origin.0 * (1.0 - s), M32: p.root.origin.1 * (1.0 - s) };
        let id = Matrix3x2 { M11: 1.0, M12: 0.0, M21: 0.0, M22: 1.0, M31: 0.0, M32: 0.0 };
        cv.with_opacity(k, |cv| {
            cv.with_transform(mul(&grow, &id), |cv| {
                cv.shadow_rr(r, PANEL_R, 18.0, 6.0, th.shadow);
                cv.fill_rr(r, PANEL_R, th.menu_bg);
                cv.stroke_rr(r, PANEL_R, 1.0, th.border);
                let st = Style::new(th.m.btn_fs, 400);
                for (i, row) in p.root.rows.iter().enumerate() {
                    let Some(item) = p.item_at(0, i) else { continue };
                    let hot = o.ramps.get(i);
                    if hot > 0.0 {
                        cv.fill_rr(*row, PANEL_R - PAD, th.menu_hover.fade(hot));
                    }
                    let x = row.x + ROW_PAD_X;
                    if p.root.check && item.checked {
                        cv.glyph(Glyph::Check, Rect::new(x - 2.0, row.cy() - 7.0, 14.0, 14.0), th.accent);
                    }
                    let tx = x + if p.root.check { CHECK_W } else { 0.0 };
                    let col = if !item.enabled { th.faint } else { th.text };
                    cv.text(Rect::new(tx, row.y, row.right() - tx - ROW_PAD_X, row.h), &item.label, st.ellipsis(), col);
                }
            });
        });
    }
}

#[cfg(test)]
mod tests {
    use super::super::testutil::*;
    use super::*;
    use crate::testkit::Harness;

    fn rc() -> Rect {
        Rect::new(10.0, 10.0, 200.0, 36.0)
    }

    fn drawn(h: &Harness, th: &Theme, s: &Select) {
        h.render(4, 4, 1.0, th.bg, |cv| s.draw(cv, th, rc()));
    }

    fn mk() -> (Harness, Theme, Select) {
        let h = Harness::new();
        let th = Theme::settings_dark();
        let mut s = Select::new(&["Auto", "Dark", "Light"], Some(0)).with_placeholder("Pick");
        s.set_work_area(Rect::new(0.0, 0.0, 400.0, 300.0));
        drawn(&h, &th, &s);
        (h, th, s)
    }

    #[test]
    fn click_opens_below_the_field_and_a_pick_changes_the_value() {
        with_motion(true, || {
            let (_h, _th, mut s) = mk();
            s.event(&down(50.0, 20.0), rc());
            let r = s.event(&up(50.0, 20.0), rc());
            assert!(s.is_open() && r.redraw && !r.changed);
            let b = s.popup_bounds().unwrap();
            assert!(b.y >= rc().bottom(), "below the field");
            assert!(b.w >= rc().w, "at least as wide as the field");
            for _ in 0..12 {
                s.step(0.016);
            }
            // third row: Light
            let row = s.open.as_ref().unwrap().panel.root.rows[2];
            s.event(&mv(row.cx(), row.cy()), rc());
            s.event(&down(row.cx(), row.cy()), rc());
            let r = s.event(&up(row.cx(), row.cy()), rc());
            assert!(r.changed && s.selected() == Some(2) && !s.is_open());
            assert!(s.popup_visible(), "still fading out");
            for _ in 0..12 {
                s.step(0.016);
            }
            assert!(!s.popup_visible(), "gone after the fade");
        });
    }

    #[test]
    fn escape_outside_click_and_tab_close_without_changing() {
        with_motion(true, || {
            for how in 0..3 {
                let (_h, _th, mut s) = mk();
                s.open(rc());
                for _ in 0..12 {
                    s.step(0.016);
                }
                match how {
                    0 => {
                        s.event(&key(KeyCode::Escape), rc());
                    }
                    1 => {
                        s.event(&down(390.0, 290.0), rc());
                    }
                    _ => {
                        s.event(&key(KeyCode::Tab), rc());
                    }
                }
                assert!(!s.is_open(), "case {how}");
                assert_eq!(s.selected(), Some(0));
            }
        });
    }

    #[test]
    fn keyboard_walks_the_open_list_and_enter_picks() {
        with_motion(true, || {
            let (_h, _th, mut s) = mk();
            s.set_focus(true, true);
            s.event(&key(KeyCode::Enter), rc());
            assert!(s.is_open());
            s.event(&key(KeyCode::Down), rc());
            s.event(&key(KeyCode::Down), rc());
            let r = s.event(&key(KeyCode::Enter), rc());
            assert!(r.changed && s.selected() == Some(2));
        });
    }

    #[test]
    fn arrows_on_a_closed_focused_select_change_the_value() {
        with_motion(true, || {
            let (_h, _th, mut s) = mk();
            assert!(!s.event(&key(KeyCode::Down), rc()).consumed, "needs focus");
            s.set_focus(true, true);
            assert!(s.event(&key(KeyCode::Down), rc()).changed && s.selected() == Some(1));
            s.event(&key(KeyCode::Down), rc());
            assert!(!s.event(&key(KeyCode::Down), rc()).changed, "stops at the last option");
            assert!(s.event(&key(KeyCode::Up), rc()).changed && s.selected() == Some(1));
        });
    }

    #[test]
    fn closed_and_open_renders() {
        with_motion(false, || {
            let (h, th, mut s) = mk();
            let img = h.render(240, 60, 2.0, th.bg, |cv| s.draw(cv, &th, rc()));
            assert_eq!(img.at(14.0, 14.0)[..3], [0x10, 0x10, 0x10], "field fill");
            assert!(img.count(None, |p| p[0] > 0xc0) > 20, "text and chevron");
            s.open(rc());
            s.step(1.0);
            let b = s.popup_bounds().unwrap();
            let open = h.render(240, 200, 2.0, th.bg, |cv| {
                s.draw(cv, &th, rc());
                s.draw_popup(cv, &th);
            });
            assert!(b.bottom() < 200.0);
            assert_eq!(open.at(b.x + 8.0, b.bottom() - 3.0)[..3], [0x1b, 0x1a, 0x1d], "menu background");
            assert!(open.count(Some(b), |p| p[0] > 0xc0) > 30, "option labels");
            // empty select with a placeholder
            let e = Select::new(&["A"], None).with_placeholder("Pick one");
            let img = h.render(240, 60, 2.0, th.bg, |cv| e.draw(cv, &th, rc()));
            assert!(img.count(None, |p| p[0] > 0x50) > 15, "placeholder pixels");
        });
    }
}
