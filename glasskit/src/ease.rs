//! Easing curves and time-based tweens, mirroring the motion tokens of Pill's web UI (`theme.css`, `motion.js`).
//!
//! * [`CubicBezier`]  the CSS `cubic-bezier(x1, y1, x2, y2)` timing function (Newton with a bisection fallback, like browsers do).
//! * [`Ease`]         a curve value: linear, a Bezier, or a CSS `linear(...)` list of evenly spaced points (the two springs of `theme.css`).
//! * [`curves`]       the named curves (`EASE_OUT_EXPO`, `SPRING_POP`...) with the exact numbers of the CSS tokens, and [`dur`] the durations.
//! * [`Tween`]        a time-based animation between two values (anything [`Lerp`]): duration, delay, reverse, retarget from the value on
//!                    screen, and a jump to the end state when the user turned animations off ([`reduced`]).
//! * [`Spring`]       a damped-spring follower for values that chase a moving target (scroll inertia, a slider that glides).
//!
//! Everything takes an explicit `dt` in seconds: no clock, so it is unit-testable and the headless gallery can show any phase.
//! A host with a frame loop calls `step(dt)` every frame while it returns `true` ("still moving") and stops asking for frames when it
//! returns `false`; a settled animation costs nothing.
use crate::color::Color;
use crate::geom::Rect;
use std::sync::atomic::{AtomicU8, Ordering};

// ---------------------------------------------------------------- reduced motion

static REDUCED: AtomicU8 = AtomicU8::new(0); // 0 = ask the system, 1 = forced off, 2 = forced on

/// Force "reduce motion" on / off for the whole process (`None` = follow the system again). Tests and the headless gallery use it so the
/// result does not depend on the machine's accessibility setting.
pub fn force_reduced(v: Option<bool>) {
    REDUCED.store(match v { None => 0, Some(false) => 1, Some(true) => 2 }, Ordering::Relaxed);
}

/// Should motion jump to its end state? The forced value, else the system setting (`motion::reduced_motion`).
pub fn reduced() -> bool {
    match REDUCED.load(Ordering::Relaxed) {
        1 => false,
        2 => true,
        _ => crate::motion::reduced_motion(),
    }
}

// ---------------------------------------------------------------- curves

/// A CSS `cubic-bezier(x1, y1, x2, y2)` timing function. `x1` and `x2` must lie in 0..1 (CSS rule); `y` may overshoot.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CubicBezier {
    pub x1: f32,
    pub y1: f32,
    pub x2: f32,
    pub y2: f32,
}

impl CubicBezier {
    pub const fn new(x1: f32, y1: f32, x2: f32, y2: f32) -> CubicBezier {
        CubicBezier { x1, y1, x2, y2 }
    }

    fn coef(a: f32, b: f32) -> (f32, f32, f32) {
        // B(t) = ((c3 t + c2) t + c1) t with the end points fixed at 0 and 1
        let c1 = 3.0 * a;
        let c2 = 3.0 * (b - a) - c1;
        let c3 = 1.0 - c1 - c2;
        (c3, c2, c1)
    }

    fn sample(c: (f32, f32, f32), t: f32) -> f32 {
        ((c.0 * t + c.1) * t + c.2) * t
    }

    fn slope(c: (f32, f32, f32), t: f32) -> f32 {
        (3.0 * c.0 * t + 2.0 * c.1) * t + c.2
    }

    /// The parameter `t` whose x equals `x` (Newton, then bisection when the slope is too flat to trust).
    fn solve_t(&self, x: f32) -> f32 {
        let cx = Self::coef(self.x1, self.x2);
        let mut t = x;
        for _ in 0..8 {
            let err = Self::sample(cx, t) - x;
            if err.abs() < 1e-6 {
                return t;
            }
            let d = Self::slope(cx, t);
            if d.abs() < 1e-6 {
                break;
            }
            t -= err / d;
        }
        let (mut lo, mut hi) = (0.0f32, 1.0f32);
        t = x;
        for _ in 0..40 {
            let v = Self::sample(cx, t);
            if (v - x).abs() < 1e-6 {
                break;
            }
            if v < x {
                lo = t;
            } else {
                hi = t;
            }
            t = (lo + hi) / 2.0;
        }
        t
    }

    /// The eased value at progress `x` (0..1; outside is clamped). 0 maps to 0 and 1 to 1 exactly.
    pub fn at(&self, x: f32) -> f32 {
        if x <= 0.0 {
            return 0.0;
        }
        if x >= 1.0 {
            return 1.0;
        }
        let t = self.solve_t(x);
        Self::sample(Self::coef(self.y1, self.y2), t)
    }
}

/// A timing function: what `transition-timing-function` accepts, as a value.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Ease {
    Linear,
    Bezier(CubicBezier),
    /// CSS `linear(p0, p1, ...)` with the points evenly spaced over 0..1 (what `--spring-settle` / `--spring-pop` use), linear between them.
    Points(&'static [f32]),
}

impl Ease {
    pub const fn bezier(x1: f32, y1: f32, x2: f32, y2: f32) -> Ease {
        Ease::Bezier(CubicBezier::new(x1, y1, x2, y2))
    }

    /// Eased value at progress `t` (clamped to 0..1). May leave 0..1 for overshooting curves.
    pub fn at(&self, t: f32) -> f32 {
        let t = t.clamp(0.0, 1.0);
        match self {
            Ease::Linear => t,
            Ease::Bezier(b) => b.at(t),
            Ease::Points(p) => match p.len() {
                0 => t,
                1 => p[0],
                n => {
                    let f = t * (n - 1) as f32;
                    let i = (f.floor() as usize).min(n - 2);
                    p[i] + (p[i + 1] - p[i]) * (f - i as f32)
                }
            },
        }
    }
}

/// The named curves. The numbers are copied from `theme.css` / `motion.js` of Pill, so a native view moves like the web one did.
pub mod curves {
    use super::Ease;
    /// `--ease`: `cubic-bezier(0.2, 0.9, 0.3, 1)`
    pub const EASE: Ease = Ease::bezier(0.2, 0.9, 0.3, 1.0);
    /// `--ease-out`: `cubic-bezier(0.22, 1, 0.36, 1)` (hover colours, fades)
    pub const EASE_OUT: Ease = Ease::bezier(0.22, 1.0, 0.36, 1.0);
    /// `--ease-in`: `cubic-bezier(0.4, 0, 1, 1)` (exits)
    pub const EASE_IN: Ease = Ease::bezier(0.4, 0.0, 1.0, 1.0);
    /// `--ease-spring`: `cubic-bezier(0.34, 1.56, 0.64, 1)` (small overshoot; the fallback of `--spring-pop`)
    pub const EASE_SPRING: Ease = Ease::bezier(0.34, 1.56, 0.64, 1.0);
    /// `--ease-out-expo`: `cubic-bezier(0.16, 1, 0.3, 1)` (collapse / expand, fades of `PillMotion.enter`)
    pub const EASE_OUT_EXPO: Ease = Ease::bezier(0.16, 1.0, 0.3, 1.0);
    /// `--ease-in-out`: `cubic-bezier(0.65, 0, 0.35, 1)`
    pub const EASE_IN_OUT: Ease = Ease::bezier(0.65, 0.0, 0.35, 1.0);
    /// CSS keyword `ease`: `cubic-bezier(0.25, 0.1, 0.25, 1)`
    pub const CSS_EASE: Ease = Ease::bezier(0.25, 0.1, 0.25, 1.0);
    /// CSS keyword `ease-in-out`: `cubic-bezier(0.42, 0, 0.58, 1)`
    pub const CSS_EASE_IN_OUT: Ease = Ease::bezier(0.42, 0.0, 0.58, 1.0);

    const SETTLE: [f32; 23] = [
        0.0, 0.034, 0.116, 0.223, 0.338, 0.451, 0.556, 0.648, 0.728, 0.794, 0.848, 0.891, 0.925, 0.95, 0.969, 0.983, 0.992, 0.998, 1.002, 1.005,
        1.006, 1.006, 1.0,
    ];
    const POP: [f32; 29] = [
        0.0, 0.044, 0.152, 0.296, 0.452, 0.604, 0.741, 0.856, 0.946, 1.013, 1.057, 1.083, 1.094, 1.093, 1.086, 1.073, 1.059, 1.044, 1.03, 1.018,
        1.009, 1.001, 0.996, 0.993, 0.991, 0.991, 0.991, 0.992, 1.0,
    ];
    /// `--spring-settle`: a soft landing with a hair of overshoot (enter transforms, FLIP moves, 0.4 s)
    pub const SPRING_SETTLE: Ease = Ease::Points(&SETTLE);
    /// `--spring-pop`: a visible overshoot (button release, switch knob, indicator slide, 0.32 - 0.5 s). Never use it on opacity or colour.
    pub const SPRING_POP: Ease = Ease::Points(&POP);
}

/// The duration tokens of `theme.css` in seconds.
pub mod dur {
    /// `--dur-xs` (press)
    pub const XS: f32 = 0.08;
    /// `--dur-fast` (hover colour)
    pub const FAST: f32 = 0.12;
    /// `--dur-med` (fades, switch track)
    pub const MED: f32 = 0.22;
    /// `--dur-slow` (springs on transforms)
    pub const SLOW: f32 = 0.38;
    /// `--dur-xl`
    pub const XL: f32 = 0.6;
    /// the enter spring of `PillMotion.enter` / `.fx-in`
    pub const SETTLE: f32 = 0.4;
    /// exits (`PillMotion.exit`)
    pub const EXIT: f32 = 0.16;
    /// stagger step of list rows (`.stag`: 28 ms, at most 8 steps)
    pub const STAGGER: f32 = 0.028;
}

// ---------------------------------------------------------------- lerp

/// Linear interpolation; `t` may leave 0..1 (overshooting curves), types with a valid range clamp.
pub trait Lerp: Copy {
    fn lerp(a: Self, b: Self, t: f32) -> Self;
}

impl Lerp for f32 {
    fn lerp(a: f32, b: f32, t: f32) -> f32 {
        a + (b - a) * t
    }
}

impl Lerp for (f32, f32) {
    fn lerp(a: Self, b: Self, t: f32) -> Self {
        (f32::lerp(a.0, b.0, t), f32::lerp(a.1, b.1, t))
    }
}

impl Lerp for Rect {
    fn lerp(a: Rect, b: Rect, t: f32) -> Rect {
        Rect::new(f32::lerp(a.x, b.x, t), f32::lerp(a.y, b.y, t), f32::lerp(a.w, b.w, t), f32::lerp(a.h, b.h, t))
    }
}

impl Lerp for Color {
    fn lerp(a: Color, b: Color, t: f32) -> Color {
        // straight-alpha channels interpolated independently, like CSS `transition: background` on non-premultiplied sRGB, clamped for overshoot
        let m = a.mix(b, t);
        Color::rgba(m.r.clamp(0.0, 1.0), m.g.clamp(0.0, 1.0), m.b.clamp(0.0, 1.0), m.a.clamp(0.0, 1.0))
    }
}

// ---------------------------------------------------------------- tween

/// A time-based animation `from -> to`. Create it, `step(dt)` it every frame while it is busy, read `value()`.
///
/// * [`Tween::retarget`] starts a new move from the value currently shown (hover in, then out again mid-way is smooth).
/// * [`Tween::reverse`] plays back to where it came from, taking only as long as the way already covered (a CSS transition reversal).
/// * [`reduced`] motion makes `new`, `retarget` and `reverse` land on the end state at once.
#[derive(Clone, Copy, Debug)]
pub struct Tween<T: Lerp> {
    from: T,
    to: T,
    dur: f32,
    delay: f32,
    ease: Ease,
    /// seconds since the start (includes the delay)
    t: f32,
}

impl<T: Lerp> Tween<T> {
    /// A tween that starts now. `dur` 0 (or reduced motion) = already at `to`.
    pub fn new(from: T, to: T, dur: f32, ease: Ease) -> Tween<T> {
        let dur = if reduced() { 0.0 } else { dur.max(0.0) };
        Tween { from, to, dur, delay: 0.0, ease, t: 0.0 }
    }

    /// A tween that rests on `v` (nothing to animate yet).
    pub fn at_rest(v: T) -> Tween<T> {
        Tween { from: v, to: v, dur: 0.0, delay: 0.0, ease: Ease::Linear, t: 0.0 }
    }

    /// Wait `secs` before the move starts (staggered lists). Not applied under reduced motion.
    pub fn delayed(mut self, secs: f32) -> Tween<T> {
        self.delay = if self.dur == 0.0 { 0.0 } else { secs.max(0.0) };
        self
    }

    /// Total seconds from start to end, delay included.
    pub fn total(&self) -> f32 {
        self.delay + self.dur
    }

    /// Linear progress 0..1 of the move (the delay not counted).
    pub fn progress(&self) -> f32 {
        if self.dur <= 0.0 {
            1.0
        } else {
            ((self.t - self.delay) / self.dur).clamp(0.0, 1.0)
        }
    }

    /// Eased progress (may leave 0..1 on overshooting curves).
    pub fn eased(&self) -> f32 {
        self.ease.at(self.progress())
    }

    /// The value to draw now.
    pub fn value(&self) -> T {
        T::lerp(self.from, self.to, self.eased())
    }

    pub fn target(&self) -> T {
        self.to
    }

    pub fn origin(&self) -> T {
        self.from
    }

    /// Advance by `dt` seconds. True while the tween is still moving (ask for another frame).
    pub fn step(&mut self, dt: f32) -> bool {
        if self.finished() {
            return false;
        }
        self.t += dt.max(0.0);
        !self.finished()
    }

    pub fn finished(&self) -> bool {
        self.t >= self.total()
    }

    /// Start a new move from the value on screen to `to`. Same target while moving: nothing changes (no restart stutter).
    pub fn retarget(&mut self, to: T, dur: f32, ease: Ease)
    where
        T: PartialEq,
    {
        if self.to == to {
            return;
        }
        *self = Tween::new(self.value(), to, dur, ease);
    }

    /// Play back to the origin. The way back takes `dur` scaled by how far the tween got (so a hover that lasted 30 ms un-hovers in 30 ms).
    pub fn reverse(&mut self) {
        let p = if self.finished() { 1.0 } else { self.progress() };
        let (a, b) = (self.value(), self.from);
        let d = if reduced() { 0.0 } else { self.dur * p.max(0.05) };
        *self = Tween { from: a, to: b, dur: d, delay: 0.0, ease: self.ease, t: 0.0 };
    }

    /// Show the end state now (previews, reduced motion).
    pub fn finish(&mut self) {
        self.t = self.total();
    }

    /// Show `v` now and stay there.
    pub fn jump(&mut self, v: T) {
        *self = Tween::at_rest(v);
    }
}

// ---------------------------------------------------------------- spring

/// A damped spring that follows a (possibly moving) target: `x'' = k (target - x) - c x'`. Stable at any `dt` (sub-stepped).
///
/// `Spring::new(stiffness, damping_ratio)`: ratio 1 = critically damped (no overshoot), 0.6 = a soft overshoot. At rest when the position is
/// within a hair of the target and the velocity is gone: `step` then returns `false` and snaps onto the target.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Spring {
    pub x: f32,
    pub v: f32,
    pub target: f32,
    k: f32,
    zeta: f32,
    eps: f32,
}

impl Spring {
    /// `stiffness` = omega squared (580 is omega 24 rad/s, the hover bump of the widgets).
    pub fn new(x: f32, stiffness: f32, damping_ratio: f32) -> Spring {
        Spring { x, v: 0.0, target: x, k: stiffness, zeta: damping_ratio, eps: 0.01 }
    }

    /// Position tolerance at which the spring counts as arrived (default 0.01: a hundredth of a pixel for pixel values).
    pub fn with_eps(mut self, eps: f32) -> Spring {
        self.eps = eps;
        self
    }

    pub fn set_target(&mut self, t: f32) {
        self.target = t;
    }

    /// Move to `x` at once with no velocity (reduced motion, previews).
    pub fn jump(&mut self, x: f32) {
        self.x = x;
        self.v = 0.0;
        self.target = x;
    }

    pub fn at_rest(&self) -> bool {
        self.x == self.target && self.v == 0.0
    }

    /// Advance by `dt` seconds; true while still moving. Under reduced motion the spring lands on the target immediately.
    pub fn step(&mut self, dt: f32) -> bool {
        if reduced() {
            self.jump(self.target);
            return false;
        }
        if self.at_rest() {
            return false;
        }
        let c = 2.0 * self.zeta * self.k.sqrt();
        let n = ((dt / 0.004).ceil() as usize).clamp(1, 64);
        let h = dt / n as f32;
        for _ in 0..n {
            self.v += (self.k * (self.target - self.x) - c * self.v) * h;
            self.x += self.v * h;
        }
        if (self.target - self.x).abs() < self.eps && self.v.abs() < self.eps * 10.0 {
            self.x = self.target;
            self.v = 0.0;
            return false;
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::curves::*;
    use super::*;

    fn lock() -> std::sync::MutexGuard<'static, ()> {
        // `force_reduced` is process-wide: tests that depend on it run one at a time
        static L: std::sync::Mutex<()> = std::sync::Mutex::new(());
        L.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn bezier_end_points_and_known_values() {
        for e in [EASE, EASE_OUT, EASE_IN, EASE_SPRING, EASE_OUT_EXPO, EASE_IN_OUT, CSS_EASE, CSS_EASE_IN_OUT] {
            assert_eq!(e.at(0.0), 0.0);
            assert_eq!(e.at(1.0), 1.0);
        }
        // symmetric curve: the middle maps to the middle
        assert!((CSS_EASE_IN_OUT.at(0.5) - 0.5).abs() < 1e-4);
        assert!((EASE_IN_OUT.at(0.5) - 0.5).abs() < 1e-4);
        // reference values of the CSS `ease` curve (cubic-bezier(.25,.1,.25,1)) from browsers: 0.25 -> 0.4095, 0.5 -> 0.8024, 0.75 -> 0.9604
        assert!((CSS_EASE.at(0.25) - 0.4095).abs() < 2e-3, "{}", CSS_EASE.at(0.25));
        assert!((CSS_EASE.at(0.5) - 0.8024).abs() < 2e-3, "{}", CSS_EASE.at(0.5));
        assert!((CSS_EASE.at(0.75) - 0.9604).abs() < 2e-3, "{}", CSS_EASE.at(0.75));
        // the identity curve cubic-bezier(1/3, 1/3, 2/3, 2/3) is linear
        let id = CubicBezier::new(1.0 / 3.0, 1.0 / 3.0, 2.0 / 3.0, 2.0 / 3.0);
        for i in 0..=10 {
            let x = i as f32 / 10.0;
            assert!((id.at(x) - x).abs() < 1e-4);
        }
    }

    #[test]
    fn named_curves_carry_the_css_tokens() {
        assert_eq!(EASE_OUT_EXPO, Ease::bezier(0.16, 1.0, 0.3, 1.0));
        assert_eq!(EASE_OUT, Ease::bezier(0.22, 1.0, 0.36, 1.0));
        assert_eq!(EASE_IN, Ease::bezier(0.4, 0.0, 1.0, 1.0));
        assert_eq!(EASE_SPRING, Ease::bezier(0.34, 1.56, 0.64, 1.0));
        assert_eq!(EASE, Ease::bezier(0.2, 0.9, 0.3, 1.0));
        assert_eq!(EASE_IN_OUT, Ease::bezier(0.65, 0.0, 0.35, 1.0));
        // expo-out is nearly done after a third of the time
        assert!(EASE_OUT_EXPO.at(0.33) > 0.9);
        // the ease-in curve starts slower than linear
        assert!(EASE_IN.at(0.2) < 0.2);
        // the overshooting spring leaves 0..1, the settle spring barely
        let peak = (0..=100).map(|i| EASE_SPRING.at(i as f32 / 100.0)).fold(0.0f32, f32::max);
        assert!(peak > 1.05 && peak < 1.2, "{peak}");
    }

    #[test]
    fn bezier_is_monotonic_in_x_for_valid_control_points() {
        // Newton + bisection must not wander: sample densely, the result for monotone curves never goes backwards
        for e in [EASE_OUT_EXPO, EASE_IN, EASE_IN_OUT, EASE_OUT] {
            let mut last = -1.0;
            for i in 0..=1000 {
                let v = e.at(i as f32 / 1000.0);
                assert!(v >= last - 1e-5, "{e:?} at {i}");
                last = v;
            }
        }
        // a curve with a flat start (slope 0 at t = 0) takes the bisection path
        let flat = CubicBezier::new(0.0, 0.0, 1.0, 1.0);
        assert!((flat.at(0.5) - 0.5).abs() < 1e-3);
        assert!((flat.at(0.001) - 0.001).abs() < 1e-4, "x = y for this curve, whatever the slope at 0");
    }

    #[test]
    fn linear_points_interpolate_evenly() {
        // spring-pop has 29 points: 28 segments; progress 1/28 is the second point
        assert!((SPRING_POP.at(1.0 / 28.0) - 0.044).abs() < 1e-5);
        assert!((SPRING_POP.at(0.5) - SPRING_POP.at(0.5 + 1e-4)).abs() < 1e-2);
        assert_eq!(SPRING_POP.at(1.0), 1.0);
        assert_eq!(SPRING_POP.at(0.0), 0.0);
        // halfway between points 12 (1.094) and 13 (1.093) is on the plateau
        let peak = (0..=280).map(|i| SPRING_POP.at(i as f32 / 280.0)).fold(0.0f32, f32::max);
        assert!((peak - 1.094).abs() < 1e-4);
        // 23 points for the settle spring
        assert!((SPRING_SETTLE.at(1.0 / 22.0) - 0.034).abs() < 1e-5);
        assert_eq!(Ease::Points(&[]).at(0.3), 0.3);
        assert_eq!(Ease::Points(&[0.7]).at(0.3), 0.7);
    }

    #[test]
    fn tween_runs_for_its_duration_and_ends_exactly() {
        let _g = lock();
        force_reduced(Some(false));
        let mut t = Tween::new(0.0f32, 10.0, 0.2, Ease::Linear);
        assert_eq!(t.value(), 0.0);
        assert!(t.step(0.1));
        assert!((t.value() - 5.0).abs() < 1e-4);
        assert!(!t.step(0.1));
        assert_eq!(t.value(), 10.0);
        assert!(!t.step(1.0), "a finished tween asks for no more frames");
        assert_eq!(t.value(), 10.0);
        force_reduced(None);
    }

    #[test]
    fn tween_delay_holds_the_start_value() {
        let _g = lock();
        force_reduced(Some(false));
        let mut t = Tween::new(1.0f32, 3.0, 0.1, Ease::Linear).delayed(0.05);
        assert!(t.step(0.04));
        assert_eq!(t.value(), 1.0);
        assert!(t.step(0.03)); // 0.07 s: 0.02 s into the move
        assert!((t.value() - 1.4).abs() < 1e-4);
        assert!(!t.step(0.1));
        assert_eq!(t.value(), 3.0);
        force_reduced(None);
    }

    #[test]
    fn tween_retarget_continues_from_the_shown_value() {
        let _g = lock();
        force_reduced(Some(false));
        let mut t = Tween::new(0.0f32, 1.0, 0.2, Ease::Linear);
        t.step(0.1);
        let mid = t.value();
        t.retarget(0.0, 0.2, Ease::Linear);
        assert!((t.value() - mid).abs() < 1e-6, "no jump when the target changes");
        // the same target again is a no-op
        let before = t.progress();
        t.retarget(0.0, 0.2, Ease::Linear);
        assert_eq!(t.progress(), before);
        force_reduced(None);
    }

    #[test]
    fn tween_reverse_takes_as_long_as_the_way_covered() {
        let _g = lock();
        force_reduced(Some(false));
        let mut t = Tween::new(0.0f32, 1.0, 0.4, Ease::Linear);
        t.step(0.1); // a quarter of the way
        t.reverse();
        assert!((t.value() - 0.25).abs() < 1e-5);
        assert!((t.total() - 0.1).abs() < 1e-5, "{}", t.total());
        assert!(!t.step(0.1));
        assert_eq!(t.value(), 0.0);
        force_reduced(None);
    }

    #[test]
    fn reduced_motion_lands_on_the_end_state() {
        let _g = lock();
        force_reduced(Some(true));
        assert!(reduced());
        let mut t = Tween::new(0.0f32, 5.0, 0.3, SPRING_POP).delayed(0.2);
        assert_eq!(t.value(), 5.0);
        assert!(!t.step(0.0));
        let mut r = Tween::at_rest(2.0f32);
        r.retarget(7.0, 0.3, EASE_OUT);
        assert_eq!(r.value(), 7.0);
        r.reverse();
        assert_eq!(r.value(), 2.0, "a reversal under reduced motion jumps to the origin");
        let mut s = Spring::new(0.0, 580.0, 1.0);
        s.set_target(10.0);
        assert!(!s.step(0.016));
        assert_eq!(s.x, 10.0);
        force_reduced(None);
    }

    #[test]
    fn lerp_of_colours_rects_and_points() {
        let a = Color::rgba(0.0, 0.0, 0.0, 0.0);
        let b = Color::rgba(1.0, 0.5, 0.0, 1.0);
        let m = Color::lerp(a, b, 0.5);
        assert_eq!((m.r, m.g, m.a), (0.5, 0.25, 0.5));
        let over = Color::lerp(a, b, 1.2);
        assert!(over.r <= 1.0 && over.a <= 1.0, "overshoot is clamped for colours");
        let r = Rect::lerp(Rect::new(0.0, 0.0, 10.0, 10.0), Rect::new(10.0, 20.0, 30.0, 10.0), 0.5);
        assert_eq!(r, Rect::new(5.0, 10.0, 20.0, 10.0));
        assert_eq!(<(f32, f32)>::lerp((0.0, 0.0), (2.0, 4.0), 0.5), (1.0, 2.0));
    }

    #[test]
    fn spring_settles_without_overshoot_when_critically_damped() {
        let _g = lock();
        force_reduced(Some(false));
        let mut s = Spring::new(0.0, 580.0, 1.0);
        s.set_target(100.0);
        let mut max = 0.0f32;
        let mut frames = 0;
        while s.step(1.0 / 60.0) {
            max = max.max(s.x);
            frames += 1;
            assert!(frames < 600, "never settles");
        }
        assert!(max <= 100.0 + 0.01, "{max}");
        assert_eq!(s.x, 100.0);
        assert!(frames > 10 && frames < 120, "{frames}");
        force_reduced(None);
    }

    #[test]
    fn spring_underdamped_overshoots_and_is_stable_at_large_dt() {
        let _g = lock();
        force_reduced(Some(false));
        let mut s = Spring::new(0.0, 580.0, 0.5);
        s.set_target(1.0);
        let mut max = 0.0f32;
        while s.step(1.0 / 60.0) {
            max = max.max(s.x);
        }
        assert!(max > 1.05, "{max}");
        // a 200 ms stall must not blow the integrator up
        let mut s = Spring::new(0.0, 580.0, 1.0);
        s.set_target(50.0);
        s.step(0.2);
        assert!(s.x.is_finite() && s.x > 0.0 && s.x <= 50.5);
        force_reduced(None);
    }
}
