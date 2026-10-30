//! `Toggle`: the on / off switch (`.switch` of Pill, `.sw` of Settings).
//!
//! The knob travels on the spring-pop curve (it overshoots the end by a hair and settles, 380 ms; 450 ms with the Settings metrics), the track colour
//! crossfades in 220 ms ease-out, and the knob stretches by 4 px while the button is held, keeping its far edge flush with the track.
use super::{Interact, Widget};
use crate::canvas::Canvas;
use crate::color::Color;
use crate::ease::{curves, dur, Lerp, Tween};
use crate::geom::Rect;
use crate::input::{InputEvent, KeyCode, Response};
use crate::theme::Theme;

pub struct Toggle {
    on: bool,
    /// knob position 0..1 on the spring (slightly beyond 1 while it overshoots)
    pos: Tween<f32>,
    /// track colour mix 0..1
    tint: Tween<f32>,
    spring: f32,
    it: Interact,
}

impl Toggle {
    pub fn new(on: bool) -> Toggle {
        let v = on as u8 as f32;
        Toggle { on, pos: Tween::at_rest(v), tint: Tween::at_rest(v), spring: dur::SLOW, it: Interact::new() }
    }

    /// Spring duration in seconds (`theme.m.sw_spring`: .38 Pill, .45 Settings).
    pub fn spring(mut self, secs: f32) -> Toggle {
        self.spring = secs;
        self
    }

    pub fn on(&self) -> bool {
        self.on
    }

    /// Set the state; `animated` false jumps (initial state from settings).
    pub fn set_on(&mut self, on: bool, animated: bool) {
        self.on = on;
        let v = on as u8 as f32;
        if animated {
            self.pos.retarget(v, self.spring, curves::SPRING_POP);
            self.tint.retarget(v, dur::MED, curves::EASE_OUT);
        } else {
            self.pos.jump(v);
            self.tint.jump(v);
        }
    }

    pub fn toggle(&mut self) {
        self.set_on(!self.on, true);
    }

    pub fn state(&self) -> &Interact {
        &self.it
    }

    /// Where the switch itself is drawn inside `rect`.
    fn body(&self, th: &Theme, rect: Rect) -> Rect {
        rect.centered(th.m.sw_w, th.m.sw_h)
    }
}

impl Widget for Toggle {
    fn event(&mut self, ev: &InputEvent, rect: Rect) -> Response {
        let (mut r, p) = self.it.pointer(ev, |q| rect.contains(q.0, q.1));
        let (rk, clicked) = self.it.activation_key(ev, &[KeyCode::Space]);
        r = r.merge(rk);
        if p.click || clicked {
            self.toggle();
            r.changed = true;
            r.redraw = true;
        } else {
            r.changed = false;
        }
        r
    }

    fn step(&mut self, dt: f32) -> bool {
        let a = self.pos.step(dt);
        let b = self.tint.step(dt);
        let c = self.it.step(dt);
        a | b | c
    }

    fn draw(&self, cv: &Canvas, th: &Theme, rect: Rect) {
        let m = &th.m;
        let b = self.body(th, rect);
        let travel = m.sw_w - m.sw_knob - 2.0 * m.sw_inset;
        let grow = (m.sw_press_grow * self.it.press()).max(0.0);
        let p = self.pos.value();
        let w = m.sw_knob + grow;
        // the far edge stays flush while the knob stretches: on the right the knob grows to the left
        let x = b.x + m.sw_inset + travel * p - grow * p.clamp(0.0, 1.0);
        let track = Color::lerp(th.switch_off, th.accent, self.tint.value().clamp(0.0, 1.0));
        cv.with_opacity(if self.it.enabled { 1.0 } else { 0.45 }, |cv| {
            cv.fill_rr(b, b.h / 2.0, track);
            cv.fill_rr(Rect::new(x, b.cy() - m.sw_knob / 2.0, w, m.sw_knob), m.sw_knob / 2.0, th.thumb);
            cv.focus_ring(b, b.h / 2.0, th.accent, self.it.ring());
        });
    }

    fn natural_size(&self, _cv: &Canvas, th: &Theme) -> (f32, f32) {
        (th.m.sw_w, th.m.sw_h)
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

#[cfg(test)]
mod tests {
    use super::super::testutil::*;
    use super::*;
    use crate::testkit::Harness;

    fn rect() -> Rect {
        Rect::new(8.0, 8.0, 40.0, 24.0)
    }

    #[test]
    fn click_and_space_toggle_and_report_changes() {
        with_motion(true, || {
            let mut t = Toggle::new(false);
            t.event(&mv(20.0, 20.0), rect());
            t.event(&down(20.0, 20.0), rect());
            assert!(!t.on(), "the click completes on release");
            let r = t.event(&up(20.0, 20.0), rect());
            assert!(r.changed && t.on());
            t.set_focus(true, true);
            t.event(&key(KeyCode::Space), rect());
            let r = t.event(&key_up(KeyCode::Space), rect());
            assert!(r.changed && !t.on());
            t.set_on(true, false);
            assert!(t.on() && t.pos.value() == 1.0, "set_on(_, false) jumps");
        });
    }

    #[test]
    fn knob_travels_with_an_overshoot_then_rests_on_the_end() {
        with_motion(true, || {
            let mut t = Toggle::new(false);
            t.set_on(true, true);
            let mut max = 0.0f32;
            let mut frames = 0;
            while t.step(1.0 / 60.0) {
                max = max.max(t.pos.value());
                frames += 1;
                assert!(frames < 200);
            }
            assert!(max > 1.05, "spring-pop overshoots the end: {max}");
            assert_eq!(t.pos.value(), 1.0);
            assert!(frames >= 20 && frames <= 40, "~380 ms: {frames} frames");
        });
        with_motion(false, || {
            let mut t = Toggle::new(false);
            t.set_on(true, true);
            assert_eq!(t.pos.value(), 1.0, "reduced motion: instant");
            assert!(!t.step(0.016));
        });
    }

    #[test]
    fn draws_off_and_on_with_the_right_colours_and_knob_position() {
        with_motion(false, || {
            let h = Harness::new();
            let th = Theme::pill_dark();
            let off = h.render(56, 40, 2.0, th.bg, |cv| Toggle::new(false).draw(cv, &th, rect()));
            let on = h.render(56, 40, 2.0, th.bg, |cv| Toggle::new(true).draw(cv, &th, rect()));
            // track colour at the far end from the knob
            assert_eq!(off.at(44.0, 20.0)[..3], [0x2a, 0x2a, 0x2a]);
            let a = on.at(12.0, 20.0);
            assert!((a[0] as i32 - 0x8f).abs() <= 2 && (a[2] as i32 - 0xff).abs() <= 2, "accent track {a:?}");
            // knob: white, near the left when off and near the right when on
            assert_eq!(off.at(20.0, 20.0)[..3], [255, 255, 255]);
            assert_eq!(on.at(36.0, 20.0)[..3], [255, 255, 255]);
            assert_ne!(on.at(20.0, 20.0)[..3], [255, 255, 255]);
        });
    }

    #[test]
    fn holding_stretches_the_knob_but_stays_inside_the_track() {
        with_motion(true, || {
            let h = Harness::new();
            let th = Theme::pill_dark();
            let mut t = Toggle::new(true);
            t.event(&down(20.0, 20.0), rect());
            for _ in 0..12 {
                t.step(0.016);
            }
            let img = h.render(56, 40, 2.0, th.bg, |cv| t.draw(cv, &th, rect()));
            let white = |p: [u8; 4]| p[0] > 240 && p[1] > 240 && p[2] > 240;
            let (x0, _, x1, _) = img.bounds(white).unwrap();
            assert!(x1 - x0 >= 2 * 21, "knob grew to ~22 px: {}", (x1 - x0) / 2);
            assert!(x1 <= 2 * 45, "right edge stays in the track");
        });
    }

    #[test]
    fn settings_metrics_give_the_wider_switch() {
        let th = Theme::settings_dark();
        let t = Toggle::new(false).spring(th.m.sw_spring);
        let h = Harness::new();
        h.render(4, 4, 1.0, th.bg, |cv| assert_eq!(t.natural_size(cv, &th), (44.0, 24.0)));
    }
}
