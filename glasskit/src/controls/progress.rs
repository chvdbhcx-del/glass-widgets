//! `ProgressBar` (determinate with an eased fill, or indeterminate with a sliding segment) and `Spinner` (a rotating arc).
//!
//! * Determinate: the fill glides to a new value in 300 ms ease-out, so a download that reports in chunks moves smoothly; the fill is never narrower
//!   than the bar is high (a pill, not a sliver).
//! * Indeterminate: a segment 40 % of the width travels across on `ease-in-out` every 1.4 s.
//! * Spinner: a 270 degree arc turning once per 0.9 s whose sweep breathes between 90 and 270 degrees.
//! * Both fade in / out in 220 ms when `set_active` / `set_visible` change.
//! * **Reduced motion**: nothing travels or turns. A progress indicator is *essential* feedback, so it does not vanish: the indeterminate bar and the
//!   spinner pulse their opacity slowly (1.6 s) in place instead.
use crate::canvas::Canvas;
use crate::color::Color;
use crate::ease::{curves, dur, reduced, Lerp, Tween};
use crate::geom::Rect;
use crate::theme::Theme;

const SEGMENT: f32 = 0.4;
const PERIOD: f32 = 1.4;

/// 0.5 - 0.5 cos: a smooth 0 -> 1 -> 0 over one unit of phase.
fn breathe(phase: f32) -> f32 {
    0.5 - 0.5 * (phase * std::f32::consts::TAU).cos()
}

pub struct ProgressBar {
    value: Option<f32>,
    shown: Tween<f32>,
    phase: f32,
    visible: Tween<f32>,
    want: bool,
}

impl ProgressBar {
    pub fn determinate(v: f32) -> ProgressBar {
        let v = v.clamp(0.0, 1.0);
        ProgressBar { value: Some(v), shown: Tween::at_rest(v), phase: 0.0, visible: Tween::at_rest(1.0), want: true }
    }

    pub fn indeterminate() -> ProgressBar {
        ProgressBar { value: None, shown: Tween::at_rest(0.0), phase: 0.0, visible: Tween::at_rest(1.0), want: true }
    }

    /// A new value (0..1); the fill glides. Switches an indeterminate bar to determinate.
    pub fn set_value(&mut self, v: f32) {
        let v = v.clamp(0.0, 1.0);
        if self.value.is_none() {
            self.shown.jump(0.0);
        }
        self.value = Some(v);
        self.shown.retarget(v, 0.3, curves::EASE_OUT);
    }

    pub fn set_indeterminate(&mut self) {
        self.value = None;
    }

    pub fn value(&self) -> Option<f32> {
        self.value
    }

    /// Fade the bar in or out (a finished download).
    pub fn set_visible(&mut self, on: bool) {
        if on != self.want {
            self.want = on;
            self.visible.retarget(on as u8 as f32, dur::MED, curves::EASE_OUT);
        }
    }

    /// Advance; true while moving (an indeterminate bar always is, while visible).
    pub fn step(&mut self, dt: f32) -> bool {
        let mut busy = self.shown.step(dt);
        busy |= self.visible.step(dt);
        if self.value.is_none() && (self.want || self.visible.value() > 0.0) {
            self.phase = (self.phase + dt / PERIOD).fract();
            busy = true;
        }
        busy
    }

    pub fn draw(&self, cv: &Canvas, th: &Theme, rect: Rect) {
        let k = self.visible.value().clamp(0.0, 1.0);
        let rad = rect.h / 2.0;
        cv.with_opacity(k, |cv| {
            cv.fill_rr(rect, rad, th.track);
            match self.value {
                Some(_) => {
                    let f = self.shown.value().clamp(0.0, 1.0);
                    if f > 0.0 {
                        cv.fill_rr(Rect::new(rect.x, rect.y, (rect.w * f).max(rect.h), rect.h), rad, th.accent);
                    }
                }
                None if reduced() => {
                    let a = 0.35 + 0.65 * breathe(self.phase);
                    cv.fill_rr(Rect::new(rect.x + rect.w * 0.3, rect.y, rect.w * 0.4, rect.h), rad, th.accent.fade(a));
                }
                None => {
                    // the segment enters from the left edge and leaves through the right one; clip it to the track
                    let e = curves::EASE_IN_OUT.at(self.phase);
                    let w = rect.w * SEGMENT;
                    let x = rect.x - w + (rect.w + w) * e;
                    cv.with_clip_rr(rect, rad, |cv| cv.fill_rr(Rect::new(x, rect.y, w, rect.h), rad, th.accent));
                }
            }
        });
    }

    pub fn natural_size(&self) -> (f32, f32) {
        (120.0, 4.0)
    }
}

pub struct Spinner {
    phase: f32,
    active: bool,
    visible: Tween<f32>,
}

const SPIN_PERIOD: f32 = 0.9;
const BREATH_PERIOD: f32 = 1.4;

impl Spinner {
    pub fn new() -> Spinner {
        Spinner { phase: 0.0, active: true, visible: Tween::at_rest(1.0) }
    }

    /// Start or stop (fades out in 220 ms, then costs nothing).
    pub fn set_active(&mut self, on: bool) {
        if on != self.active {
            self.active = on;
            self.visible.retarget(on as u8 as f32, dur::MED, curves::EASE_OUT);
        }
    }

    pub fn active(&self) -> bool {
        self.active
    }

    /// Current (rotation degrees, sweep degrees) of the arc, for tests and custom drawing.
    pub fn arc(&self) -> (f32, f32) {
        let rot = (self.phase / SPIN_PERIOD).fract() * 360.0;
        let sweep = 90.0 + 180.0 * breathe((self.phase / BREATH_PERIOD).fract());
        (rot, sweep)
    }

    pub fn step(&mut self, dt: f32) -> bool {
        let mut busy = self.visible.step(dt);
        if self.active || self.visible.value() > 0.0 {
            self.phase += dt;
            // keep the phase small without a visible jump: both periods divide 12.6 s
            if self.phase > 12.6 {
                self.phase -= 12.6;
            }
            busy = true;
        }
        busy
    }

    pub fn draw(&self, cv: &Canvas, th: &Theme, rect: Rect) {
        let k = self.visible.value().clamp(0.0, 1.0);
        if k <= 0.0 {
            return;
        }
        let d = rect.w.min(rect.h);
        let w = (d / 8.0).clamp(1.5, 4.0);
        let r = d / 2.0 - w / 2.0;
        let c = rect.center();
        if reduced() {
            // no travelling motion: a fixed three-quarter arc that breathes in opacity
            let a = 0.4 + 0.6 * breathe((self.phase / 1.6).fract());
            cv.with_opacity(k, |cv| {
                cv.stroke_circle(c, r, w, th.track);
                cv.arc(c, r, w, 0.0, 270.0, Color::lerp(th.accent.alpha(0.0), th.accent, a));
            });
            return;
        }
        let (rot, sweep) = self.arc();
        cv.with_opacity(k, |cv| {
            cv.stroke_circle(c, r, w, th.track);
            cv.arc(c, r, w, rot, sweep, th.accent);
        });
    }

    pub fn natural_size(&self) -> (f32, f32) {
        (16.0, 16.0)
    }
}

impl Default for Spinner {
    fn default() -> Self {
        Spinner::new()
    }
}

#[cfg(test)]
mod tests {
    use super::super::testutil::with_motion;
    use super::*;
    use crate::testkit::Harness;

    #[test]
    fn determinate_fill_glides_and_never_shrinks_below_a_pill() {
        with_motion(true, || {
            let h = Harness::new();
            let th = Theme::pill_dark();
            let rc = Rect::new(4.0, 4.0, 100.0, 4.0);
            let mut p = ProgressBar::determinate(0.0);
            let empty = h.render(110, 12, 2.0, th.bg, |cv| p.draw(cv, &th, rc));
            assert_eq!(empty.at(8.0, 6.0)[..3], [0x33, 0x33, 0x33], "track only");
            p.set_value(0.5);
            p.step(0.05);
            let mid = p.shown.value();
            assert!(mid > 0.0 && mid < 0.5, "gliding: {mid}");
            while p.step(0.016) {}
            let half = h.render(110, 12, 2.0, th.bg, |cv| p.draw(cv, &th, rc));
            let (x0, _, x1, _) = half.bounds(|q| q[2] > 0xe0).unwrap();
            assert!(((x1 - x0) as f32 / 2.0 - 50.0).abs() < 1.5, "fill is half the bar: {}", (x1 - x0) / 2);
            // tiny values keep a pill-sized fill
            let mut t = ProgressBar::determinate(0.01);
            t.step(1.0);
            let tiny = h.render(110, 12, 2.0, th.bg, |cv| t.draw(cv, &th, rc));
            let (a, _, b, _) = tiny.bounds(|q| q[2] > 0xe0).unwrap();
            assert!(b - a >= 8, "at least the bar height wide");
        });
    }

    #[test]
    fn indeterminate_segment_travels_across_and_loops() {
        with_motion(true, || {
            let h = Harness::new();
            let th = Theme::pill_dark();
            let rc = Rect::new(4.0, 4.0, 100.0, 4.0);
            let mut p = ProgressBar::indeterminate();
            let mut centres = vec![];
            for _ in 0..8 {
                p.step(PERIOD / 8.0 - 0.0001);
                let img = h.render(110, 12, 1.0, th.bg, |cv| p.draw(cv, &th, rc));
                centres.push(img.bounds(|q| q[2] > 0xc0).map(|b| (b.0 + b.2) as f32 / 2.0));
            }
            let seen: Vec<f32> = centres.iter().flatten().copied().collect();
            assert!(seen.len() >= 5, "visible most of the time");
            assert!(seen.windows(2).all(|w| w[1] >= w[0] - 1.0) || seen.iter().cloned().fold(0.0, f32::max) > 80.0, "moves right: {seen:?}");
            assert!(p.step(0.016), "always busy while visible");
        });
    }

    #[test]
    fn reduced_motion_pulses_in_place() {
        with_motion(false, || {
            let h = Harness::new();
            let th = Theme::pill_dark();
            let rc = Rect::new(4.0, 4.0, 100.0, 4.0);
            let mut p = ProgressBar::indeterminate();
            let a = h.render(110, 12, 1.0, th.bg, |cv| p.draw(cv, &th, rc));
            p.step(0.7);
            let b = h.render(110, 12, 1.0, th.bg, |cv| p.draw(cv, &th, rc));
            assert_eq!(a.bounds(|q| q[2] > 0x60).map(|b| b.0), b.bounds(|q| q[2] > 0x60).map(|b| b.0), "same place");
            assert!(a.diff(&b, 2) > 0, "but the opacity moved");
        });
    }

    #[test]
    fn spinner_turns_breathes_and_fades_out() {
        with_motion(true, || {
            let mut s = Spinner::new();
            let (r0, sw0) = s.arc();
            s.step(SPIN_PERIOD / 4.0);
            let (r1, sw1) = s.arc();
            assert!((r1 - r0 - 90.0).abs() < 1.0);
            assert!(sw1 > sw0 && (90.0..=270.0).contains(&sw1));
            let h = Harness::new();
            let th = Theme::pill_dark();
            let img = h.render(24, 24, 2.0, th.bg, |cv| s.draw(cv, &th, Rect::new(4.0, 4.0, 16.0, 16.0)));
            assert!(img.count(None, |q| q[2] > 0xe0) > 20, "arc pixels");
            s.set_active(false);
            let mut frames = 0;
            while s.step(0.016) {
                frames += 1;
                assert!(frames < 100);
            }
            assert!(frames <= 16, "fades out in ~220 ms");
            let gone = h.render(24, 24, 2.0, th.bg, |cv| s.draw(cv, &th, Rect::new(4.0, 4.0, 16.0, 16.0)));
            assert_eq!(gone.count(None, |q| q[0] > 0), 0);
            assert!(!s.step(0.016), "a stopped spinner costs nothing");
        });
    }
}
