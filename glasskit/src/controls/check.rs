//! `Checkbox`, `Radio` and `RadioGroup`.
//!
//! * Checkbox: 18 px box (5 px radius). Checking crossfades the fill to the accent in 220 ms and DRAWS the tick on (a stroke that grows along its path
//!   in 220 ms ease-out); the box pops on the spring-pop curve when pressed and released. A third state, `Mixed`, shows a dash.
//! * Radio: 18 px ring; the dot grows from nothing to 8 px on the spring-pop curve (380 ms) and shrinks back the same way.
//! * `RadioGroup`: a vertical list of radios with the arrow-key behaviour of a native group (one tab stop, arrows move AND select).
//!
//! Labels are drawn to the right of the box when the control is given one; the whole rectangle you pass is the click target.
use super::{dim_if, Interact, Widget};
use crate::canvas::Canvas;
use crate::color::Color;
use crate::ease::{curves, dur, Lerp, Tween};
use crate::geom::Rect;
use crate::input::{InputEvent, KeyCode, Response};
use crate::theme::Theme;

const GAP: f32 = 8.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CheckState {
    Off,
    On,
    /// indeterminate (some children checked): shown as a dash; a click makes it `On`
    Mixed,
}

impl CheckState {
    fn target(self) -> f32 {
        (self != CheckState::Off) as u8 as f32
    }
}

pub struct Checkbox {
    pub label: String,
    state: CheckState,
    /// fill crossfade 0..1
    fill: Tween<f32>,
    /// tick draw-on 0..1
    tick: Tween<f32>,
    it: Interact,
}

fn box_rect(rect: Rect, size: f32) -> Rect {
    Rect::new(rect.x, rect.cy() - size / 2.0, size, size)
}

fn label_rect(rect: Rect, size: f32) -> Rect {
    Rect::new(rect.x + size + GAP, rect.y, (rect.w - size - GAP).max(0.0), rect.h)
}

impl Checkbox {
    pub fn new(label: &str, state: CheckState) -> Checkbox {
        let v = state.target();
        Checkbox { label: label.into(), state, fill: Tween::at_rest(v), tick: Tween::at_rest(v), it: Interact::new() }
    }

    pub fn state(&self) -> CheckState {
        self.state
    }

    pub fn checked(&self) -> bool {
        self.state == CheckState::On
    }

    pub fn set_state(&mut self, s: CheckState, animated: bool) {
        self.state = s;
        let v = s.target();
        if animated {
            self.fill.retarget(v, dur::MED, curves::EASE_OUT);
            self.tick.retarget(v, dur::MED, curves::EASE_OUT);
        } else {
            self.fill.jump(v);
            self.tick.jump(v);
        }
    }

    fn flip(&mut self) {
        let next = if self.state == CheckState::On { CheckState::Off } else { CheckState::On };
        self.set_state(next, true);
    }
}

/// Point `t` (0..1) of the polyline `pts` measured by length.
fn along(pts: &[(f32, f32)], t: f32) -> Vec<(f32, f32)> {
    let len: Vec<f32> = pts.windows(2).map(|w| ((w[1].0 - w[0].0).powi(2) + (w[1].1 - w[0].1).powi(2)).sqrt()).collect();
    let mut left = len.iter().sum::<f32>() * t.clamp(0.0, 1.0);
    let mut out = vec![pts[0]];
    for (i, l) in len.iter().enumerate() {
        if left >= *l {
            out.push(pts[i + 1]);
            left -= l;
        } else {
            let k = if *l > 0.0 { left / l } else { 0.0 };
            out.push((pts[i].0 + (pts[i + 1].0 - pts[i].0) * k, pts[i].1 + (pts[i + 1].1 - pts[i].1) * k));
            break;
        }
    }
    out
}

impl Widget for Checkbox {
    fn event(&mut self, ev: &InputEvent, rect: Rect) -> Response {
        let (mut r, p) = self.it.pointer(ev, |q| rect.contains(q.0, q.1));
        let (rk, k) = self.it.activation_key(ev, &[KeyCode::Space]);
        r = r.merge(rk);
        r.changed = false;
        if p.click || k {
            self.flip();
            r.changed = true;
            r.redraw = true;
        }
        r
    }

    fn step(&mut self, dt: f32) -> bool {
        let a = self.fill.step(dt);
        let b = self.tick.step(dt);
        let c = self.it.step(dt);
        a | b | c
    }

    fn draw(&self, cv: &Canvas, th: &Theme, rect: Rect) {
        let size = th.m.box_size;
        let b = box_rect(rect, size);
        let f = self.fill.value().clamp(0.0, 1.0);
        let h = self.it.hover();
        cv.with_opacity(if self.it.enabled { 1.0 } else { 0.45 }, |cv| {
            cv.with_scale_about(b.center(), self.it.press_scale(0.9), |cv| {
                cv.fill_rr(b, th.m.box_r, Color::lerp(th.field, th.accent, f));
                let ring = Color::lerp(th.dim.mix(th.text, h * 0.5), th.accent, f);
                cv.stroke_rr(b, th.m.box_r, 1.5, ring);
                let u = size / 18.0;
                let pts: Vec<(f32, f32)> = if self.state == CheckState::Mixed { vec![(5.0, 9.0), (13.0, 9.0)] } else { vec![(4.6, 9.4), (7.7, 12.5), (13.4, 5.9)] };
                let pts: Vec<(f32, f32)> = pts.into_iter().map(|(x, y)| (b.x + x * u, b.y + y * u)).collect();
                let path = along(&pts, self.tick.value());
                if path.len() > 1 && f > 0.02 {
                    for w in path.windows(2) {
                        cv.line(w[0], w[1], 2.0, th.on_accent);
                    }
                }
            });
            if !self.label.is_empty() {
                cv.text(label_rect(rect, size), &self.label, th.body(), dim_if(true, th.text));
            }
            cv.focus_ring(b, th.m.box_r, th.accent, self.it.ring());
        });
    }

    fn natural_size(&self, cv: &Canvas, th: &Theme) -> (f32, f32) {
        let size = th.m.box_size;
        let lw = if self.label.is_empty() { 0.0 } else { GAP + cv.text_width(&self.label, th.body()).ceil() };
        (size + lw, size.max(20.0))
    }

    fn set_focus(&mut self, focused: bool, ring: bool) {
        self.it.set_focus(focused, ring);
    }

    fn focusable(&self) -> bool {
        self.it.enabled
    }

    fn set_enabled(&mut self, on: bool) {
        self.it.set_enabled(on);
    }
}

// ---------------------------------------------------------------- radio

fn draw_radio(cv: &Canvas, th: &Theme, b: Rect, dot: f32, hover: f32, ring: f32) {
    let c = b.center();
    let r = b.w / 2.0;
    let sel = dot.clamp(0.0, 1.0);
    cv.fill_circle(c, r, th.field);
    let col = Color::lerp(th.dim.mix(th.text, hover * 0.5), th.accent, sel);
    cv.stroke_circle(c, r - 0.75, 1.5, col);
    // the dot overshoots with the spring (dot may be > 1)
    let dr = (r - 5.0).max(1.0) * dot.max(0.0);
    cv.fill_circle(c, dr, th.accent);
    cv.focus_ring(b, r, th.accent, ring);
}

/// One radio button. For a group use [`RadioGroup`]; a lone `Radio` only selects (the owner clears the others).
pub struct Radio {
    pub label: String,
    selected: bool,
    dot: Tween<f32>,
    it: Interact,
}

impl Radio {
    pub fn new(label: &str, selected: bool) -> Radio {
        Radio { label: label.into(), selected, dot: Tween::at_rest(selected as u8 as f32), it: Interact::new() }
    }

    pub fn selected(&self) -> bool {
        self.selected
    }

    pub fn set_selected(&mut self, on: bool, animated: bool) {
        self.selected = on;
        let v = on as u8 as f32;
        if animated {
            self.dot.retarget(v, dur::SLOW, curves::SPRING_POP);
        } else {
            self.dot.jump(v);
        }
    }
}

impl Widget for Radio {
    fn event(&mut self, ev: &InputEvent, rect: Rect) -> Response {
        let (mut r, p) = self.it.pointer(ev, |q| rect.contains(q.0, q.1));
        let (rk, k) = self.it.activation_key(ev, &[KeyCode::Space]);
        r = r.merge(rk);
        r.changed = false;
        if (p.click || k) && !self.selected {
            self.set_selected(true, true);
            r.changed = true;
            r.redraw = true;
        }
        r
    }

    fn step(&mut self, dt: f32) -> bool {
        let a = self.dot.step(dt);
        a | self.it.step(dt)
    }

    fn draw(&self, cv: &Canvas, th: &Theme, rect: Rect) {
        let size = th.m.box_size;
        let b = box_rect(rect, size);
        cv.with_opacity(if self.it.enabled { 1.0 } else { 0.45 }, |cv| {
            cv.with_scale_about(b.center(), self.it.press_scale(0.92), |cv| draw_radio(cv, th, b, self.dot.value(), self.it.hover(), self.it.ring()));
            if !self.label.is_empty() {
                cv.text(label_rect(rect, size), &self.label, th.body(), th.text);
            }
        });
    }

    fn natural_size(&self, cv: &Canvas, th: &Theme) -> (f32, f32) {
        let size = th.m.box_size;
        let lw = if self.label.is_empty() { 0.0 } else { GAP + cv.text_width(&self.label, th.body()).ceil() };
        (size + lw, size.max(20.0))
    }

    fn set_focus(&mut self, focused: bool, ring: bool) {
        self.it.set_focus(focused, ring);
    }

    fn focusable(&self) -> bool {
        self.it.enabled
    }

    fn set_enabled(&mut self, on: bool) {
        self.it.set_enabled(on);
    }
}

/// A vertical group of radios: one selection, arrow keys move and select.
pub struct RadioGroup {
    labels: Vec<String>,
    selected: usize,
    dots: Vec<Tween<f32>>,
    items: Vec<Interact>,
    focused: bool,
    enabled: bool,
    /// row height
    pub row_h: f32,
}

impl RadioGroup {
    pub fn new(labels: &[&str], selected: usize) -> RadioGroup {
        let n = labels.len();
        let selected = selected.min(n.saturating_sub(1));
        RadioGroup {
            labels: labels.iter().map(|s| s.to_string()).collect(),
            selected,
            dots: (0..n).map(|i| Tween::at_rest((i == selected) as u8 as f32)).collect(),
            items: (0..n).map(|_| Interact::new()).collect(),
            focused: false,
            enabled: true,
            row_h: 28.0,
        }
    }

    pub fn selected(&self) -> usize {
        self.selected
    }

    pub fn row_rects(&self, rect: Rect) -> Vec<Rect> {
        (0..self.labels.len()).map(|i| Rect::new(rect.x, rect.y + i as f32 * self.row_h, rect.w, self.row_h)).collect()
    }

    pub fn select(&mut self, i: usize, animated: bool) -> bool {
        if i >= self.labels.len() || i == self.selected {
            return false;
        }
        self.selected = i;
        for (k, d) in self.dots.iter_mut().enumerate() {
            let v = (k == i) as u8 as f32;
            if animated {
                d.retarget(v, dur::SLOW, curves::SPRING_POP);
            } else {
                d.jump(v);
            }
        }
        true
    }
}

impl Widget for RadioGroup {
    fn event(&mut self, ev: &InputEvent, rect: Rect) -> Response {
        let rows = self.row_rects(rect);
        let mut out = Response::NONE;
        let mut clicked = None;
        for (i, rr) in rows.iter().enumerate() {
            let (r, p) = self.items[i].pointer(ev, |q| rr.contains(q.0, q.1));
            if p.click {
                clicked = Some(i);
            }
            out = out.merge(r);
        }
        out.changed = false;
        if let Some(i) = clicked {
            out.changed = self.select(i, true);
            out.redraw = true;
        }
        if self.focused && self.enabled {
            if let Some((code, _, _)) = ev.key_down() {
                let n = self.labels.len();
                let to = match code {
                    KeyCode::Up | KeyCode::Left => Some((self.selected + n - 1) % n.max(1)),
                    KeyCode::Down | KeyCode::Right => Some((self.selected + 1) % n.max(1)),
                    _ => None,
                };
                if let Some(t) = to {
                    out.consumed = true;
                    out.redraw = true;
                    out.changed = self.select(t, true);
                }
            }
        }
        out
    }

    fn step(&mut self, dt: f32) -> bool {
        let mut busy = false;
        for d in &mut self.dots {
            busy |= d.step(dt);
        }
        for it in &mut self.items {
            busy |= it.step(dt);
        }
        busy
    }

    fn draw(&self, cv: &Canvas, th: &Theme, rect: Rect) {
        let size = th.m.box_size;
        cv.with_opacity(if self.enabled { 1.0 } else { 0.45 }, |cv| {
            for (i, rr) in self.row_rects(rect).iter().enumerate() {
                let b = box_rect(*rr, size);
                // only the selected radio shows the ring when the group has focus
                let ring = if self.selected == i { self.items[i].ring() } else { 0.0 };
                cv.with_scale_about(b.center(), self.items[i].press_scale(0.92), |cv| draw_radio(cv, th, b, self.dots[i].value(), self.items[i].hover(), ring));
                cv.text(label_rect(*rr, size), &self.labels[i], th.body(), th.text);
            }
        });
    }

    fn natural_size(&self, cv: &Canvas, th: &Theme) -> (f32, f32) {
        let w = self.labels.iter().map(|l| cv.text_width(l, th.body()).ceil()).fold(0.0, f32::max);
        (th.m.box_size + GAP + w, self.row_h * self.labels.len() as f32)
    }

    fn set_focus(&mut self, focused: bool, ring: bool) {
        self.focused = focused;
        let sel = self.selected;
        for (i, it) in self.items.iter_mut().enumerate() {
            it.set_focus(focused && i == sel, ring);
        }
    }

    fn focusable(&self) -> bool {
        self.enabled
    }

    fn set_enabled(&mut self, on: bool) {
        self.enabled = on;
        for it in &mut self.items {
            it.set_enabled(on);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::testutil::*;
    use super::*;
    use crate::testkit::Harness;

    fn rc() -> Rect {
        Rect::new(8.0, 4.0, 120.0, 20.0)
    }

    #[test]
    fn checkbox_toggles_on_click_and_space_and_mixed_becomes_on() {
        with_motion(true, || {
            let mut c = Checkbox::new("Remember", CheckState::Off);
            c.event(&down(20.0, 14.0), rc());
            assert!(c.event(&up(20.0, 14.0), rc()).changed && c.checked());
            c.set_focus(true, true);
            c.event(&key(KeyCode::Space), rc());
            assert!(c.event(&key_up(KeyCode::Space), rc()).changed && !c.checked());
            c.set_state(CheckState::Mixed, false);
            c.event(&down(20.0, 14.0), rc());
            c.event(&up(20.0, 14.0), rc());
            assert_eq!(c.state(), CheckState::On);
        });
    }

    #[test]
    fn tick_draws_on_progressively() {
        assert_eq!(along(&[(0.0, 0.0), (10.0, 0.0), (10.0, 10.0)], 0.0).len(), 2);
        let half = along(&[(0.0, 0.0), (10.0, 0.0), (10.0, 10.0)], 0.5);
        assert_eq!(half.last().copied(), Some((10.0, 0.0)));
        let full = along(&[(0.0, 0.0), (10.0, 0.0), (10.0, 10.0)], 1.0);
        assert_eq!(full.last().copied(), Some((10.0, 10.0)));
        let q = along(&[(0.0, 0.0), (10.0, 0.0), (10.0, 10.0)], 0.75);
        assert_eq!(q.last().copied(), Some((10.0, 5.0)));
    }

    #[test]
    fn checkbox_fill_and_tick_animate_then_rest() {
        with_motion(true, || {
            let h = Harness::new();
            let th = Theme::pill_dark();
            let mut c = Checkbox::new("", CheckState::Off);
            let off = h.render(30, 28, 2.0, th.bg, |cv| c.draw(cv, &th, Rect::new(4.0, 4.0, 20.0, 20.0)));
            c.event(&down(10.0, 14.0), Rect::new(4.0, 4.0, 20.0, 20.0));
            c.event(&up(10.0, 14.0), Rect::new(4.0, 4.0, 20.0, 20.0));
            for _ in 0..4 {
                c.step(0.016);
            }
            let mid = h.render(30, 28, 2.0, th.bg, |cv| c.draw(cv, &th, Rect::new(4.0, 4.0, 20.0, 20.0)));
            while c.step(0.016) {}
            let on = h.render(30, 28, 2.0, th.bg, |cv| c.draw(cv, &th, Rect::new(4.0, 4.0, 20.0, 20.0)));
            // box centre-left (outside the tick): background-ish when off, accent when on
            assert_eq!(off.at(6.5, 20.0)[..3], [0x0f, 0x0f, 0x0f]);
            let a = on.at(6.5, 20.0);
            assert!(a[2] > 0xe0 && a[0] > 0x80, "accent fill {a:?}");
            let blue = |img: &crate::testkit::Image| img.at(6.5, 20.0)[2] as i32;
            assert!(blue(&mid) > blue(&off) && blue(&mid) < blue(&on), "mid-way fill: {} < {} < {}", blue(&off), blue(&mid), blue(&on));
            // the dark tick (on_accent) is visible inside the filled box
            assert!(on.count(Some(Rect::new(5.0, 5.0, 18.0, 18.0)), |p| p[0] < 0x40 && p[2] < 0x40) > 6, "tick pixels");
            assert!(mid.diff(&on, 0) > 0);
        });
    }

    #[test]
    fn radio_group_arrows_move_and_wrap_and_click_selects() {
        with_motion(true, || {
            let mut g = RadioGroup::new(&["A", "B", "C"], 0);
            let r = Rect::new(0.0, 0.0, 100.0, 84.0);
            assert!(!g.event(&key(KeyCode::Down), r).consumed, "no focus yet");
            g.set_focus(true, true);
            assert!(g.event(&key(KeyCode::Down), r).changed && g.selected() == 1);
            g.event(&key(KeyCode::Up), r);
            let w = g.event(&key(KeyCode::Up), r);
            assert!(w.changed && g.selected() == 2, "wraps");
            // click the first row
            g.event(&down(10.0, 10.0), r);
            let u = g.event(&up(10.0, 10.0), r);
            assert!(u.changed && g.selected() == 0);
            g.event(&down(10.0, 10.0), r);
            assert!(!g.event(&up(10.0, 10.0), r).changed, "already selected");
        });
    }

    #[test]
    fn radio_dot_grows_with_a_spring() {
        with_motion(true, || {
            let mut r = Radio::new("x", false);
            let rr = Rect::new(0.0, 0.0, 80.0, 20.0);
            r.event(&down(5.0, 10.0), rr);
            assert!(r.event(&up(5.0, 10.0), rr).changed);
            let mut max = 0.0f32;
            while r.step(1.0 / 60.0) {
                max = max.max(r.dot.value());
            }
            assert!(max > 1.05 && r.dot.value() == 1.0, "overshoots to {max}");
            r.event(&down(5.0, 10.0), rr);
            assert!(!r.event(&up(5.0, 10.0), rr).changed, "a selected radio stays selected");
        });
        with_motion(false, || {
            let h = Harness::new();
            let th = Theme::pill_dark();
            let on = h.render(40, 24, 2.0, th.bg, |cv| Radio::new("", true).draw(cv, &th, Rect::new(4.0, 2.0, 30.0, 20.0)));
            let off = h.render(40, 24, 2.0, th.bg, |cv| Radio::new("", false).draw(cv, &th, Rect::new(4.0, 2.0, 30.0, 20.0)));
            assert_eq!(on.at(13.0, 12.0)[..3], [0x8f, 0x96, 0xff], "dot in the middle of the ring");
            assert_eq!(off.at(13.0, 12.0)[..3], [0x0f, 0x0f, 0x0f]);
        });
    }
}
