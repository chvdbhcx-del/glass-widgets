//! `Slider`: a horizontal value slider (`input[type=range]` of Pill and Settings).
//!
//! * the thumb is a circle that swells 1.18x while held (spring-pop release), the filled part of the 4 px track follows the thumb;
//! * a click on the track makes the value GLIDE to the pointer (120 ms ease-out); dragging follows the pointer without lag;
//! * programmatic `set_value` glides as well; the optional value bubble (formatter supplied by the caller: the crate holds no strings) fades in above
//!   the thumb while it is held or hovered;
//! * keyboard (focused): arrows +-1 step (or 1 % of the range), PageUp / PageDown +-10 %, Home / End;
//! * `thin()` hides the thumb until hover / press / focus (`.slider.thin`).
use super::tooltip::draw_bubble;
use super::{Interact, Widget};
use crate::canvas::Canvas;
use crate::color::Color;
use crate::ease::{curves, dur, Lerp, Tween};
use crate::geom::{Pt, Rect};
use crate::input::{Cursor, InputEvent, KeyCode, MouseButton, Response};
use crate::theme::{brighten, Theme};
use std::cell::Cell;

pub struct Slider {
    pub min: f32,
    pub max: f32,
    /// snap to multiples of `step` from `min` (None = continuous)
    pub step: Option<f32>,
    pub thin: bool,
    /// formats the value for the bubble; `None` = no bubble
    pub format: Option<fn(f32) -> String>,
    value: f32,
    /// what is drawn: glides towards `value` after a click or `set_value`
    shown: Tween<f32>,
    dragging: bool,
    bubble: Tween<f32>,
    it: Interact,
    /// thumb diameter of the last draw (events map x to a value with it)
    thumb: Cell<f32>,
}

fn lerp_inv(v: f32, a: f32, b: f32) -> f32 {
    if b > a {
        ((v - a) / (b - a)).clamp(0.0, 1.0)
    } else {
        0.0
    }
}

impl Slider {
    pub fn new(min: f32, max: f32, value: f32) -> Slider {
        let v = value.clamp(min.min(max), max.max(min));
        Slider { min, max, step: None, thin: false, format: None, value: v, shown: Tween::at_rest(v), dragging: false, bubble: Tween::at_rest(0.0), it: Interact::new(), thumb: Cell::new(12.0) }
    }

    /// Snap to multiples of `s` from `min` (the current value is snapped now).
    pub fn step_by(mut self, s: f32) -> Slider {
        self.step = Some(s);
        let v = self.snap(self.value);
        self.value = v;
        self.shown.jump(v);
        self
    }

    pub fn thin(mut self) -> Slider {
        self.thin = true;
        self
    }

    pub fn with_format(mut self, f: fn(f32) -> String) -> Slider {
        self.format = Some(f);
        self
    }

    pub fn value(&self) -> f32 {
        self.value
    }

    /// The value as it is drawn right now (differs from `value()` while gliding).
    pub fn shown(&self) -> f32 {
        self.shown.value()
    }

    pub fn dragging(&self) -> bool {
        self.dragging
    }

    /// Set the value; it glides there unless `animated` is false.
    pub fn set_value(&mut self, v: f32, animated: bool) {
        let v = self.snap(v);
        self.value = v;
        if animated {
            self.shown.retarget(v, 0.12, curves::EASE_OUT);
        } else {
            self.shown.jump(v);
        }
    }

    fn snap(&self, v: f32) -> f32 {
        let (lo, hi) = (self.min.min(self.max), self.max.max(self.min));
        let v = match self.step {
            Some(s) if s > 0.0 => self.min + ((v - self.min) / s).round() * s,
            _ => v,
        };
        v.clamp(lo, hi)
    }

    /// The horizontal range the thumb centre moves over.
    fn span(&self, rect: Rect) -> (f32, f32) {
        let r = self.thumb.get() / 2.0;
        (rect.x + r, rect.right() - r)
    }

    fn value_at(&self, rect: Rect, x: f32) -> f32 {
        let (x0, x1) = self.span(rect);
        let t = if x1 > x0 { ((x - x0) / (x1 - x0)).clamp(0.0, 1.0) } else { 0.0 };
        self.min + (self.max - self.min) * t
    }

    /// [`Widget::event`] for a host that has not drawn the slider yet (the thumb size of the theme is needed to map x to a value; after the first
    /// `draw` the trait method knows it).
    pub fn event_themed(&mut self, ev: &InputEvent, rect: Rect, th: &Theme) -> Response {
        self.thumb.set(th.m.sl_thumb);
        self.event(ev, rect)
    }
}

impl Widget for Slider {
    fn event(&mut self, ev: &InputEvent, rect: Rect) -> Response {
        let hit_rect = Rect::new(rect.x, rect.y - 4.0, rect.w, rect.h + 8.0);
        let (mut r, p) = self.it.pointer(ev, |q: Pt| hit_rect.contains(q.0, q.1));
        r.changed = false;
        let set = |s: &mut Slider, x: f32, glide: bool| -> bool {
            let v = s.snap(s.value_at(rect, x));
            if v != s.value {
                s.value = v;
                if glide {
                    s.shown.retarget(v, 0.12, curves::EASE_OUT);
                } else {
                    s.shown.jump(v);
                }
                true
            } else {
                false
            }
        };
        match ev {
            InputEvent::MouseDown { pos, button: MouseButton::Left, .. } if p.down => {
                self.dragging = true;
                r.changed = set(self, pos.0, true);
                r.cursor = Some(Cursor::Hand);
            }
            InputEvent::MouseMove { pos, buttons, .. } if self.dragging => {
                if buttons.left() {
                    // a drag follows the pointer exactly: drop any glide still running from the first click
                    r.changed = set(self, pos.0, false);
                    r.capture = true;
                    r.redraw = true;
                    r.consumed = true;
                } else {
                    self.dragging = false;
                }
            }
            InputEvent::MouseUp { button: MouseButton::Left, .. } if self.dragging => {
                self.dragging = false;
                r.redraw = true;
            }
            InputEvent::Key { code, mods, down: true, .. } if self.it.focused() && self.it.enabled => {
                let span = self.max - self.min;
                let unit = self.step.unwrap_or(span / 100.0);
                let big = span / 10.0;
                let dv = match code {
                    KeyCode::Left | KeyCode::Down => Some(-unit),
                    KeyCode::Right | KeyCode::Up => Some(unit),
                    KeyCode::PageDown => Some(-big),
                    KeyCode::PageUp => Some(big),
                    KeyCode::Home => Some(self.min - self.value),
                    KeyCode::End => Some(self.max - self.value),
                    _ => None,
                };
                if let Some(dv) = dv {
                    let _ = mods;
                    let before = self.value;
                    self.set_value(self.value + dv, true);
                    r.consumed = true;
                    r.redraw = true;
                    r.changed = self.value != before;
                }
            }
            _ => {}
        }
        let want_bubble = (self.dragging || self.it.hovered() || self.it.ring() > 0.0) && self.format.is_some();
        self.bubble.retarget(want_bubble as u8 as f32, dur::FAST, curves::EASE_OUT);
        if self.dragging {
            r.capture = true;
        }
        r
    }

    fn step(&mut self, dt: f32) -> bool {
        let a = self.shown.step(dt);
        let b = self.bubble.step(dt);
        let c = self.it.step(dt);
        a | b | c
    }

    fn draw(&self, cv: &Canvas, th: &Theme, rect: Rect) {
        let m = &th.m;
        self.thumb.set(m.sl_thumb);
        let (x0, x1) = self.span(rect);
        let t = lerp_inv(self.shown.value(), self.min, self.max);
        let cx = x0 + (x1 - x0) * t;
        let track = Rect::new(rect.x, rect.cy() - m.sl_track / 2.0, rect.w, m.sl_track);
        let thumb_alpha = if self.thin { self.it.hover().max(self.it.press()).max(self.it.ring()).clamp(0.0, 1.0) } else { 1.0 };
        cv.with_opacity(if self.it.enabled { 1.0 } else { 0.4 }, |cv| {
            cv.fill_rr(track, m.sl_track / 2.0, th.track);
            if cx > track.x {
                cv.fill_rr(Rect::new(track.x, track.y, (cx - track.x).max(m.sl_track), track.h), m.sl_track / 2.0, th.slider_fill);
            }
            let swell = 1.0 + 0.18 * self.it.press().clamp(-0.2, 1.1);
            let r = m.sl_thumb / 2.0 * swell;
            let col = Color::lerp(th.thumb, brighten(th.thumb, 1.1), self.it.hover());
            cv.with_opacity(thumb_alpha, |cv| {
                cv.fill_circle((cx, rect.cy()), r + 0.5, Color::rgba(0.0, 0.0, 0.0, 0.25));
                cv.fill_circle((cx, rect.cy()), r, col);
            });
            cv.focus_ring(Rect::new(cx - r, rect.cy() - r, 2.0 * r, 2.0 * r), r, th.accent, self.it.ring());
        });
        let k = self.bubble.value().clamp(0.0, 1.0);
        if let (Some(f), true) = (self.format, k > 0.01) {
            draw_bubble(cv, th, (cx, rect.cy() - m.sl_thumb / 2.0 - 4.0), &f(self.shown.value()), k);
        }
    }

    fn natural_size(&self, _cv: &Canvas, th: &Theme) -> (f32, f32) {
        (140.0, th.m.sl_h)
    }

    fn set_focus(&mut self, focused: bool, ring: bool) {
        self.it.set_focus(focused, ring);
    }

    fn focusable(&self) -> bool {
        self.it.enabled
    }

    fn set_enabled(&mut self, on: bool) {
        self.it.set_enabled(on);
        if !on {
            self.dragging = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::testutil::*;
    use super::*;
    use crate::input::{Buttons, Modifiers};
    use crate::testkit::Harness;

    fn rect() -> Rect {
        Rect::new(10.0, 10.0, 106.0, 16.0)
    }

    fn drag_to(x: f32) -> InputEvent {
        InputEvent::MouseMove { pos: (x, 18.0), mods: Modifiers::NONE, buttons: Buttons::NONE.with(MouseButton::Left) }
    }

    #[test]
    fn click_and_drag_map_x_to_value() {
        with_motion(true, || {
            let th = Theme::pill_dark();
            let mut s = Slider::new(0.0, 100.0, 0.0);
            // the thumb centre runs from x + 6 = 16 to 110: the middle of the span is 63
            let r = s.event_themed(&down(63.0, 18.0), rect(), &th);
            assert!(r.changed && r.capture && s.dragging());
            assert!((s.value() - 50.0).abs() < 0.01, "{}", s.value());
            assert!(s.shown() < 1.0, "a click glides: still at the old position");
            let r = s.event_themed(&drag_to(200.0), rect(), &th);
            assert!(r.changed && s.value() == 100.0, "clamped at the end while dragging outside");
            assert_eq!(s.shown(), 100.0, "a drag follows the pointer without lag");
            s.event_themed(&drag_to(-50.0), rect(), &th);
            assert_eq!(s.value(), 0.0);
            s.event_themed(&up(0.0, 18.0), rect(), &th);
            assert!(!s.dragging());
        });
    }

    #[test]
    fn click_glides_to_the_target_in_about_120ms() {
        with_motion(true, || {
            let th = Theme::pill_dark();
            let mut s = Slider::new(0.0, 100.0, 0.0);
            s.event_themed(&down(110.0, 18.0), rect(), &th);
            s.event_themed(&up(110.0, 18.0), rect(), &th);
            let mut frames = 0;
            while s.shown() != 100.0 {
                s.step(1.0 / 60.0);
                frames += 1;
                assert!(frames < 60);
            }
            assert!((6..=12).contains(&frames), "{frames}");
        });
    }

    #[test]
    fn steps_snap_and_keyboard_moves_by_step_page_home_end() {
        with_motion(true, || {
            let th = Theme::pill_dark();
            let mut s = Slider::new(0.0, 10.0, 3.0).step_by(2.0);
            assert_eq!(s.value(), 4.0, "step_by snaps the current value (3 is halfway: rounds up)");
            s.set_value(5.2, false);
            assert_eq!(s.value(), 6.0);
            s.set_focus(true, true);
            assert!(s.event_themed(&key(KeyCode::Right), rect(), &th).changed);
            assert_eq!(s.value(), 8.0);
            s.event_themed(&key(KeyCode::Home), rect(), &th);
            assert_eq!(s.value(), 0.0);
            assert!(!s.event_themed(&key(KeyCode::Left), rect(), &th).changed, "already at the start");
            s.event_themed(&key(KeyCode::End), rect(), &th);
            assert_eq!(s.value(), 10.0);
            let mut c = Slider::new(0.0, 200.0, 100.0);
            c.set_focus(true, true);
            c.event_themed(&key(KeyCode::PageDown), rect(), &th);
            assert_eq!(c.value(), 80.0);
            c.event_themed(&key(KeyCode::Up), rect(), &th);
            assert_eq!(c.value(), 82.0, "continuous: 1 % of the range");
        });
    }

    #[test]
    fn renders_fill_up_to_the_thumb_and_a_bubble_when_dragged() {
        with_motion(false, || {
            let h = Harness::new();
            let th = Theme::pill_dark();
            let mut s = Slider::new(0.0, 100.0, 50.0).with_format(|v| format!("{v:.0}"));
            let img = h.render(130, 50, 2.0, th.bg, |cv| s.draw(cv, &th, Rect::new(10.0, 30.0, 106.0, 16.0)));
            let fill = img.at(30.0, 38.0);
            let empty = img.at(100.0, 38.0);
            assert!(fill[2] > 200 && empty[2] < 100, "fill {fill:?} vs track {empty:?}");
            assert_eq!(img.at(63.0, 38.0)[..3], [255, 255, 255], "thumb at the middle");
            assert_eq!(img.at(63.0, 10.0)[3], 255);
            let before = img.count(Some(Rect::new(0.0, 0.0, 130.0, 24.0)), |p| p[0] > 60);
            assert_eq!(before, 0, "no bubble at rest");
            s.event_themed(&down(63.0, 38.0), Rect::new(10.0, 30.0, 106.0, 16.0), &th);
            s.step(0.5);
            let img = h.render(130, 50, 2.0, th.bg, |cv| s.draw(cv, &th, Rect::new(10.0, 30.0, 106.0, 16.0)));
            assert!(img.count(Some(Rect::new(0.0, 0.0, 130.0, 24.0)), |p| p[0] > 60) > 20, "the value bubble appears while held");
        });
    }

    #[test]
    fn thin_slider_hides_the_thumb_until_hover() {
        with_motion(false, || {
            let h = Harness::new();
            let th = Theme::pill_dark();
            let mut s = Slider::new(0.0, 100.0, 50.0).thin();
            let rc = Rect::new(10.0, 10.0, 106.0, 16.0);
            let a = h.render(130, 36, 1.0, th.bg, |cv| s.draw(cv, &th, rc));
            assert_ne!(a.at(63.0, 18.0)[..3], [255, 255, 255]);
            s.event_themed(&mv(63.0, 18.0), rc, &th);
            let b = h.render(130, 36, 1.0, th.bg, |cv| s.draw(cv, &th, rc));
            assert!(b.at(63.0, 18.0)[0] > 240);
        });
    }
}
