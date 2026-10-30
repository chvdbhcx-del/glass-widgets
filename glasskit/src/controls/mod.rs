//! The control set of the kit: [`Button`], [`Toggle`] (switch), [`Slider`], [`Segmented`] control, [`Checkbox`], [`Radio`] / [`RadioGroup`],
//! [`Select`] (dropdown on top of `menu.rs`), [`Tooltip`], [`ProgressBar`] / [`Spinner`] and [`Badge`]. Look and timings follow Pill's `theme.css` /
//! `settings.css`; colours and sizes come from a [`Theme`], so the same control renders for Pill (dark, light), the Settings window and the glass widgets.
//!
//! # How a control is used
//! Controls are *retained state, immediate layout*: the owner decides each control's rectangle (logical px) and passes it with every call, so any layout
//! scheme works (`Rect` slicing, a flex-like helper, hand placement).
//!
//! ```ignore
//! let mut sw = Toggle::new(false);
//! // event loop:
//! let r = sw.event(&ev, rect);              // Response { redraw, changed, capture, cursor, .. }
//! if r.changed { settings.dark = sw.on(); }
//! // frame loop, while anything is busy:
//! let busy = sw.step(dt);                   // advance animations by the real frame time
//! // paint:
//! sw.draw(&cv, &theme, rect);
//! ```
//! The common behaviour is the [`Widget`] trait, so a page can hold `Vec<Box<dyn Widget>>` next to a [`crate::input::FocusChain`].
//!
//! # Animation
//! Every state change animates with the tokens of the CSS (`crate::ease::{curves, dur}`): hover colours 120 ms ease-out, press scale .96 in 80 ms and a
//! spring-pop release in 380 ms, knobs and indicators on the spring-pop curve, focus rings fade in 120 ms. `step(dt)` returns `true` while a control is
//! moving; the host asks for frames only then (a settled UI costs nothing). With *Reduce motion* on (`crate::ease::reduced`) every change lands on its end
//! state at once.
//!
//! # Keyboard and focus
//! A control acts on keys only while it is focused ([`Widget::set_focus`]); the host owns the [`crate::input::FocusChain`] and tells each control whether
//! it has focus and whether the ring is visible (keyboard navigation) or hidden (a mouse click), like `:focus-visible`.
mod badge;
mod button;
mod check;
mod progress;
mod segmented;
mod select;
mod slider;
mod toggle;
mod tooltip;

pub use badge::{Badge, BadgeKind};
pub use button::{Button, ButtonKind};
pub use check::{CheckState, Checkbox, Radio, RadioGroup};
pub use progress::{ProgressBar, Spinner};
pub use segmented::Segmented;
pub use select::Select;
pub use slider::Slider;
pub use toggle::Toggle;
pub use tooltip::Tooltip;

use crate::canvas::Canvas;
use crate::color::Color;
use crate::ease::{curves, dur, Tween};
use crate::geom::{Pt, Rect};
use crate::input::{Cursor, InputEvent, KeyCode, MouseButton, Response};
use crate::theme::Theme;
use std::rc::Rc;

/// An icon a control can show: any function that paints into a rectangle with a colour (a [`crate::Glyph`], an SVG, a bitmap).
pub type Icon = Rc<dyn Fn(&Canvas, Rect, Color)>;

/// A built-in vector glyph as an [`Icon`].
pub fn glyph_icon(g: crate::icons::Glyph) -> Icon {
    Rc::new(move |cv: &Canvas, r: Rect, c: Color| cv.glyph(g, r, c))
}

/// What every control can do (object-safe). Positions in events are in the same coordinate space as `rect`.
pub trait Widget {
    /// Feed an input event; `rect` is where the control is drawn.
    fn event(&mut self, ev: &InputEvent, rect: Rect) -> Response;
    /// Advance animations by `dt` seconds; `true` while still moving (ask for another frame).
    fn step(&mut self, dt: f32) -> bool;
    fn draw(&self, cv: &Canvas, th: &Theme, rect: Rect);
    /// Preferred size (the width of a label + padding, the fixed height of the control).
    fn natural_size(&self, cv: &Canvas, th: &Theme) -> (f32, f32);
    /// Keyboard focus: `focused` and whether the focus ring shows (`ring`: focus came from the keyboard).
    fn set_focus(&mut self, focused: bool, ring: bool);
    /// Can the control take keyboard focus (disabled ones cannot)?
    fn focusable(&self) -> bool;
    fn set_enabled(&mut self, on: bool);
}

// ---------------------------------------------------------------- shared interaction state

/// Hover, press and focus-ring state of one clickable thing, with the animated amounts the drawing reads. Used by every control; also usable on its
/// own for custom clickables.
#[derive(Clone, Debug)]
pub struct Interact {
    pub enabled: bool,
    hovered: bool,
    pressed: bool,
    /// the left button went down on the control and is still held (the press look follows the pointer in and out, the click needs a release inside)
    armed: bool,
    focused: bool,
    ring: bool,
    hover_t: Tween<f32>,
    press_t: Tween<f32>,
    focus_t: Tween<f32>,
}

/// The result of feeding an event to [`Interact::pointer`].
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Pointer {
    /// the pointer went down on the control
    pub down: bool,
    /// a full click: down and up on the control (release inside)
    pub click: bool,
    /// the pointer is over the control now
    pub over: bool,
    /// the press was cancelled by releasing outside
    pub cancelled: bool,
}

impl Default for Interact {
    fn default() -> Self {
        Interact::new()
    }
}

impl Interact {
    pub fn new() -> Interact {
        Interact {
            enabled: true,
            hovered: false,
            pressed: false,
            armed: false,
            focused: false,
            ring: false,
            hover_t: Tween::at_rest(0.0),
            press_t: Tween::at_rest(0.0),
            focus_t: Tween::at_rest(0.0),
        }
    }

    pub fn hovered(&self) -> bool {
        self.hovered
    }
    pub fn pressed(&self) -> bool {
        self.pressed
    }
    pub fn focused(&self) -> bool {
        self.focused
    }

    /// Eased hover amount 0..1.
    pub fn hover(&self) -> f32 {
        self.hover_t.value().clamp(0.0, 1.0)
    }
    /// Eased press amount; overshoots slightly below 0 on the release spring.
    pub fn press(&self) -> f32 {
        self.press_t.value()
    }
    /// Eased focus-ring opacity 0..1.
    pub fn ring(&self) -> f32 {
        self.focus_t.value().clamp(0.0, 1.0)
    }
    /// Scale of the pressed control: 1 at rest, `.96` pressed (`.btn:active { transform: scale(.96) }`), a springy overshoot on release.
    pub fn press_scale(&self, depth: f32) -> f32 {
        1.0 - (1.0 - depth) * self.press()
    }

    pub fn set_hover(&mut self, on: bool) -> bool {
        let on = on && self.enabled;
        if on == self.hovered {
            return false;
        }
        self.hovered = on;
        self.hover_t.retarget(on as u8 as f32, dur::FAST, curves::EASE_OUT);
        true
    }

    pub fn set_press(&mut self, on: bool) {
        if on == self.pressed {
            return;
        }
        self.pressed = on;
        if on {
            self.press_t.retarget(1.0, dur::XS, curves::EASE_OUT);
        } else {
            self.press_t.retarget(0.0, dur::SLOW, curves::SPRING_POP);
        }
    }

    pub fn set_focus(&mut self, focused: bool, ring: bool) {
        self.focused = focused;
        let show = focused && ring;
        if show != self.ring {
            self.ring = show;
            self.focus_t.retarget(show as u8 as f32, dur::FAST, curves::EASE_OUT);
        }
    }

    pub fn set_enabled(&mut self, on: bool) {
        self.enabled = on;
        if !on {
            self.set_hover(false);
            self.set_press(false);
            self.armed = false;
        }
    }

    /// Advance the three amounts; true while any is moving.
    pub fn step(&mut self, dt: f32) -> bool {
        let a = self.hover_t.step(dt);
        let b = self.press_t.step(dt);
        let c = self.focus_t.step(dt);
        a | b | c
    }

    /// Show the end state of everything now (previews).
    pub fn settle(&mut self) {
        self.hover_t.finish();
        self.press_t.finish();
        self.focus_t.finish();
    }

    /// Standard pointer handling for a clickable `hit` area: hover, press with capture, click on release inside.
    /// Left button only. The returned [`Response`] carries redraw / capture / cursor; `Pointer` says what happened.
    pub fn pointer(&mut self, ev: &InputEvent, hit: impl Fn(Pt) -> bool) -> (Response, Pointer) {
        let mut r = Response::NONE;
        let mut p = Pointer::default();
        if !self.enabled {
            if matches!(ev, InputEvent::MouseMove { .. }) {
                self.set_hover(false);
            }
            return (r, p);
        }
        match ev {
            InputEvent::MouseMove { pos, .. } => {
                let over = hit(*pos);
                p.over = over;
                if self.set_hover(over) {
                    r.redraw = true;
                }
                if over || self.armed {
                    r.cursor = Some(Cursor::Hand);
                }
                if self.armed {
                    r.capture = true;
                    r.redraw = true;
                    self.set_press(over); // the press look follows the pointer out of the control and back in
                }
            }
            InputEvent::MouseDown { pos, button: MouseButton::Left, .. } if hit(*pos) => {
                p.down = true;
                p.over = true;
                self.armed = true;
                self.set_hover(true);
                self.set_press(true);
                r = Response { consumed: true, redraw: true, capture: true, focus: true, cursor: Some(Cursor::Hand), ..Response::NONE };
            }
            InputEvent::MouseUp { pos, button: MouseButton::Left, .. } if self.armed => {
                let over = hit(*pos);
                self.armed = false;
                p.over = over;
                self.set_press(false);
                self.set_hover(over);
                p.click = over;
                p.cancelled = !over;
                r = Response { consumed: true, redraw: true, changed: over, ..Response::NONE };
                if over {
                    r.cursor = Some(Cursor::Hand);
                }
            }
            InputEvent::MouseLeave => {
                if self.set_hover(false) {
                    r.redraw = true;
                }
            }
            _ => {}
        }
        (r, p)
    }

    /// Space / Enter activation while focused: down presses, up clicks. Returns (response, clicked).
    pub fn activation_key(&mut self, ev: &InputEvent, keys: &[KeyCode]) -> (Response, bool) {
        if !self.enabled || !self.focused {
            return (Response::NONE, false);
        }
        if let InputEvent::Key { code, down, repeat, .. } = ev {
            if keys.contains(code) {
                if *down && !*repeat {
                    self.set_press(true);
                    return (Response { consumed: true, redraw: true, ..Response::NONE }, false);
                }
                if !*down {
                    let was = self.pressed;
                    self.set_press(false);
                    return (Response { consumed: true, redraw: true, changed: was, ..Response::NONE }, was);
                }
                return (Response::consumed(), false);
            }
        }
        (Response::NONE, false)
    }
}

/// `color` faded when the control is disabled (`opacity: .45`).
pub(crate) fn dim_if(enabled: bool, c: Color) -> Color {
    if enabled {
        c
    } else {
        c.fade(0.45)
    }
}

#[cfg(test)]
pub(crate) mod testutil {
    use super::*;
    use crate::input::{Buttons, Modifiers};

    pub fn mv(x: f32, y: f32) -> InputEvent {
        InputEvent::MouseMove { pos: (x, y), mods: Modifiers::NONE, buttons: Buttons::NONE }
    }
    pub fn down(x: f32, y: f32) -> InputEvent {
        InputEvent::MouseDown { pos: (x, y), button: MouseButton::Left, mods: Modifiers::NONE, clicks: 1 }
    }
    pub fn up(x: f32, y: f32) -> InputEvent {
        InputEvent::MouseUp { pos: (x, y), button: MouseButton::Left, mods: Modifiers::NONE }
    }
    pub fn key(code: KeyCode) -> InputEvent {
        InputEvent::Key { code, mods: Modifiers::NONE, down: true, repeat: false }
    }
    pub fn key_up(code: KeyCode) -> InputEvent {
        InputEvent::Key { code, mods: Modifiers::NONE, down: false, repeat: false }
    }
    /// Force animations on for the duration of a test body and release after (the flag is process wide: tests that depend on it hold this lock).
    pub fn with_motion<R>(on: bool, f: impl FnOnce() -> R) -> R {
        static L: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _g = L.lock().unwrap_or_else(|e| e.into_inner());
        crate::ease::force_reduced(Some(!on));
        let r = f();
        crate::ease::force_reduced(None);
        r
    }
}

#[cfg(test)]
mod tests {
    use super::testutil::*;
    use super::*;

    #[test]
    fn interact_hover_press_click() {
        with_motion(true, || {
            let mut i = Interact::new();
            let hit = |p: Pt| Rect::new(0.0, 0.0, 50.0, 20.0).contains(p.0, p.1);
            let (r, _) = i.pointer(&mv(10.0, 10.0), hit);
            assert!(r.redraw && i.hovered() && r.cursor == Some(Cursor::Hand));
            assert_eq!(i.hover(), 0.0, "the ramp starts at 0");
            i.step(0.06);
            let half = i.hover();
            assert!(half > 0.3 && half < 1.0, "{half}");
            let (r, p) = i.pointer(&down(10.0, 10.0), hit);
            assert!(p.down && r.capture && r.focus);
            i.step(0.1);
            assert!(i.press_scale(0.96) < 0.97);
            let (r, p) = i.pointer(&up(12.0, 10.0), hit);
            assert!(p.click && r.changed);
            // releasing the spring overshoots above scale 1 at some point
            let mut max = 0.0f32;
            for _ in 0..40 {
                i.step(1.0 / 60.0);
                max = max.max(i.press_scale(0.96));
            }
            assert!(max > 1.0, "spring-pop release overshoots: {max}");
            settle_interact(&mut i);
            assert_eq!(i.press_scale(0.96), 1.0);
        });
    }

    fn settle_interact(i: &mut Interact) {
        for _ in 0..300 {
            if !i.step(1.0 / 60.0) {
                return;
            }
        }
        panic!("never settled");
    }

    #[test]
    fn release_outside_cancels() {
        with_motion(true, || {
            let mut i = Interact::new();
            let hit = |p: Pt| Rect::new(0.0, 0.0, 50.0, 20.0).contains(p.0, p.1);
            i.pointer(&down(10.0, 10.0), hit);
            let (_, p) = i.pointer(&up(80.0, 10.0), hit);
            assert!(p.cancelled && !p.click);
            // a drag out and back in still clicks
            i.pointer(&down(10.0, 10.0), hit);
            i.pointer(&mv(80.0, 10.0), hit);
            i.pointer(&mv(20.0, 10.0), hit);
            let (_, p) = i.pointer(&up(20.0, 10.0), hit);
            assert!(p.click);
        });
    }

    #[test]
    fn disabled_ignores_everything_and_reduced_motion_jumps() {
        with_motion(false, || {
            let mut i = Interact::new();
            let hit = |_p: Pt| true;
            i.pointer(&mv(1.0, 1.0), hit);
            assert_eq!(i.hover(), 1.0, "reduced motion: end state at once");
            i.set_enabled(false);
            assert!(!i.hovered());
            let (r, p) = i.pointer(&down(1.0, 1.0), hit);
            assert!(!r.consumed && !p.down);
        });
        with_motion(true, || {
            let mut i = Interact::new();
            i.set_enabled(false);
            assert!(!i.set_hover(true));
        });
    }

    #[test]
    fn keyboard_activation_needs_focus() {
        with_motion(true, || {
            let mut i = Interact::new();
            let (_, c) = i.activation_key(&key(KeyCode::Space), &[KeyCode::Space]);
            assert!(!c, "no focus, no activation");
            i.set_focus(true, true);
            assert!(i.activation_key(&key(KeyCode::Space), &[KeyCode::Space]).0.consumed);
            let (r, c) = i.activation_key(&key_up(KeyCode::Space), &[KeyCode::Space]);
            assert!(c && r.changed);
            let (_, c) = i.activation_key(&key(KeyCode::Char('A')), &[KeyCode::Space]);
            assert!(!c);
            i.step(0.2);
            assert!(i.ring() > 0.99);
            i.set_focus(true, false);
            i.step(0.2);
            assert!(i.ring() < 0.01, "a mouse focus draws no ring");
        });
    }
}
