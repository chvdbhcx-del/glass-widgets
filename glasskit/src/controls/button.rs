//! `Button`: text and / or icon, in the five looks of `.btn`, `.btn-primary`, `.btn-ghost`, the danger button of Settings and the round `.icon-btn`.
//!
//! Timings (`theme.css`): hover colour 120 ms ease-out, press `scale(.96)` in 80 ms (icon buttons `.92`), release on the spring-pop curve in 380 ms,
//! focus ring fades in 120 ms. Disabled buttons draw at 45 % opacity and ignore input.
use super::{dim_if, Icon, Interact, Widget};
use crate::canvas::Canvas;
use crate::color::Color;
use crate::ease::Lerp;
use crate::geom::Rect;
use crate::input::{InputEvent, KeyCode, Response};
use crate::theme::Theme;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ButtonKind {
    /// `.btn`: raised fill, light text
    Normal,
    /// `.btn-primary`: accent fill, dark text, semibold
    Primary,
    /// `.btn-ghost`: no fill, dim text that lights up on hover
    Ghost,
    /// red text, a red wash on hover (Settings `.btn.danger`)
    Danger,
    /// `.icon-btn`: a round, 28 px icon-only button
    Icon,
}

pub struct Button {
    pub label: String,
    pub icon: Option<Icon>,
    pub kind: ButtonKind,
    /// the 28 px variant (`.btn.sm`)
    pub small: bool,
    it: Interact,
}

const ICON: f32 = 16.0;
const GAP: f32 = 8.0;

impl Button {
    pub fn new(label: &str) -> Button {
        Button { label: label.into(), icon: None, kind: ButtonKind::Normal, small: false, it: Interact::new() }
    }

    pub fn with_icon(label: &str, icon: Icon) -> Button {
        Button { icon: Some(icon), ..Button::new(label) }
    }

    /// A round icon-only button (28 x 28); `label` is the accessible name (not drawn).
    pub fn icon_only(label: &str, icon: Icon) -> Button {
        Button { icon: Some(icon), kind: ButtonKind::Icon, ..Button::new(label) }
    }

    pub fn kind(mut self, k: ButtonKind) -> Button {
        self.kind = k;
        self
    }

    pub fn small(mut self, on: bool) -> Button {
        self.small = on;
        self
    }

    pub fn state(&self) -> &Interact {
        &self.it
    }

    pub fn state_mut(&mut self) -> &mut Interact {
        &mut self.it
    }

    fn colors(&self, th: &Theme) -> (Color, Color) {
        let h = self.it.hover();
        match self.kind {
            ButtonKind::Normal => (Color::lerp(th.btn, th.btn_hover, h), th.text),
            ButtonKind::Primary => (Color::lerp(th.accent, th.accent_hover, h), th.on_accent),
            ButtonKind::Ghost => (Color::lerp(th.hover.alpha(0.0), th.hover, h), Color::lerp(th.dim, th.text, h)),
            ButtonKind::Danger => (Color::lerp(th.btn, th.btn.mix(th.red, th.red_bg.a), h), th.red),
            ButtonKind::Icon => (Color::lerp(th.hover.alpha(0.0), th.hover, h), Color::lerp(th.icon, th.text, h)),
        }
    }
}

impl Widget for Button {
    fn event(&mut self, ev: &InputEvent, rect: Rect) -> Response {
        let (mut r, p) = self.it.pointer(ev, |q| rect.contains(q.0, q.1));
        let (rk, clicked) = self.it.activation_key(ev, &[KeyCode::Enter, KeyCode::Space]);
        r = r.merge(rk);
        r.changed = p.click || clicked;
        r
    }

    fn step(&mut self, dt: f32) -> bool {
        self.it.step(dt)
    }

    fn draw(&self, cv: &Canvas, th: &Theme, rect: Rect) {
        let (bg, fg) = self.colors(th);
        let rad = if self.kind == ButtonKind::Icon { rect.h / 2.0 } else { th.m.btn_r };
        let depth = if self.kind == ButtonKind::Icon { 0.92 } else { 0.96 };
        let enabled = self.it.enabled;
        cv.with_opacity(if enabled { 1.0 } else { 0.45 }, |cv| {
            cv.with_scale_about(rect.center(), self.it.press_scale(depth), |cv| {
                cv.fill_rr(rect, rad, bg);
                if self.kind == ButtonKind::Danger {
                    // the red wash of Settings' danger hover sits ON the button fill
                    cv.fill_rr(rect, rad, th.red_bg.fade(self.it.hover()));
                }
                let mut st = th.btn_text();
                if self.kind == ButtonKind::Primary {
                    st = st.weight(st.weight.max(600));
                }
                let fg = dim_if(true, fg);
                match (&self.icon, self.kind) {
                    (Some(ic), ButtonKind::Icon) => ic(cv, rect.centered(ICON, ICON), fg),
                    (icon, _) => {
                        let tw = if self.label.is_empty() { 0.0 } else { cv.text_width(&self.label, st) };
                        let iw = if icon.is_some() { ICON + if tw > 0.0 { GAP } else { 0.0 } } else { 0.0 };
                        let x0 = rect.cx() - (iw + tw) / 2.0;
                        if let Some(ic) = icon {
                            ic(cv, Rect::new(x0, rect.cy() - ICON / 2.0, ICON, ICON), fg);
                        }
                        cv.text(Rect::new(x0 + iw, rect.y, tw + 2.0, rect.h), &self.label, st, fg);
                    }
                }
            });
            cv.focus_ring(rect, rad, th.accent, self.it.ring());
        });
    }

    fn natural_size(&self, cv: &Canvas, th: &Theme) -> (f32, f32) {
        if self.kind == ButtonKind::Icon {
            return (28.0, 28.0);
        }
        let mut st = th.btn_text();
        if self.kind == ButtonKind::Primary {
            st = st.weight(st.weight.max(600));
        }
        let tw = if self.label.is_empty() { 0.0 } else { cv.text_width(&self.label, st).ceil() };
        let iw = if self.icon.is_some() { ICON + if tw > 0.0 { GAP } else { 0.0 } } else { 0.0 };
        let pad = if self.small { th.m.btn_pad_x * 0.75 } else { th.m.btn_pad_x };
        (tw + iw + 2.0 * pad, if self.small { th.m.btn_h_sm } else { th.m.btn_h })
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

    fn r() -> Rect {
        Rect::new(10.0, 6.0, 90.0, 32.0)
    }

    #[test]
    fn click_fires_changed_once_and_animates_the_hover() {
        with_motion(true, || {
            let mut b = Button::new("OK");
            let rect = r();
            assert!(b.event(&mv(20.0, 20.0), rect).redraw);
            assert!(b.step(0.016), "hover is moving");
            assert!(!b.event(&down(20.0, 20.0), rect).changed);
            let up = b.event(&up(20.0, 20.0), rect);
            assert!(up.changed && up.consumed);
            assert!(!b.event(&mv(200.0, 20.0), rect).changed);
        });
    }

    #[test]
    fn keyboard_enter_and_space_activate_only_with_focus() {
        with_motion(true, || {
            let mut b = Button::new("OK");
            assert!(!b.event(&key(KeyCode::Space), r()).consumed);
            b.set_focus(true, true);
            b.event(&key(KeyCode::Enter), r());
            assert!(b.event(&key_up(KeyCode::Enter), r()).changed);
            b.set_enabled(false);
            assert!(!b.focusable());
            assert!(!b.event(&down(20.0, 20.0), r()).consumed);
        });
    }

    #[test]
    fn renders_kinds_with_distinct_fills_and_states() {
        with_motion(false, || {
            let h = Harness::new();
            let th = Theme::pill_dark();
            let draw = |kind: ButtonKind, hover: bool, enabled: bool| {
                h.render(110, 44, 1.0, th.bg, |cv| {
                    let mut b = Button::new("Save").kind(kind);
                    b.set_enabled(enabled);
                    if hover {
                        b.event(&mv(30.0, 20.0), r());
                    }
                    b.draw(cv, &th, r());
                })
            };
            let n = draw(ButtonKind::Normal, false, true);
            let nh = draw(ButtonKind::Normal, true, true);
            let p = draw(ButtonKind::Primary, false, true);
            let g = draw(ButtonKind::Ghost, false, true);
            let d = draw(ButtonKind::Normal, false, false);
            assert_eq!(n.at(14.0, 10.0)[..3], [0x0f, 0x0f, 0x0f], "button fill is the card colour");
            assert_eq!(nh.at(14.0, 10.0)[..3], [0x18, 0x18, 0x18], "hover swaps to the hover fill");
            let a = p.at(14.0, 10.0);
            assert!(a[2] > 200 && a[0] > 100, "accent fill {a:?}");
            assert_eq!(g.at(14.0, 10.0)[..3], [0, 0, 0], "ghost has no fill");
            assert!(d.at(14.0, 10.0)[0] < n.at(14.0, 10.0)[0] + 1 && d.diff(&n, 0) > 0, "disabled differs from enabled");
            assert!(n.count(None, |q| q[0] > 150) > 15, "label pixels");
            assert_eq!(n.at(2.0, 2.0)[..3], [0, 0, 0], "nothing outside the rect");
        });
    }

    #[test]
    fn pressed_button_shrinks_and_focus_ring_is_outside() {
        with_motion(true, || {
            let h = Harness::new();
            let th = Theme::pill_dark();
            let mut b = Button::new("");
            b.event(&down(20.0, 20.0), r());
            for _ in 0..10 {
                b.step(0.016);
            }
            let img = h.render(110, 44, 1.0, th.bg, |cv| b.draw(cv, &th, r()));
            let (x0, _, x1, _) = img.bounds(|p| p[0] > 8).unwrap();
            assert!(x1 - x0 < 90 && x1 - x0 >= 86, "scaled to .96: {}", x1 - x0);
            let mut f = Button::new("");
            f.set_focus(true, true);
            for _ in 0..20 {
                f.step(0.016);
            }
            let img = h.render(110, 44, 1.0, th.bg, |cv| f.draw(cv, &th, r()));
            let (x0, _, x1, _) = img.bounds(|p| p[2] > 200).unwrap();
            assert!(x0 < 10 && x1 > 100, "ring is outside the rectangle: {x0}..{x1}");
        });
    }

    #[test]
    fn natural_size_follows_label_and_icon() {
        let h = Harness::new();
        let th = Theme::pill_dark();
        h.render(10, 10, 1.0, th.bg, |cv| {
            let a = Button::new("A").natural_size(cv, &th);
            let b = Button::new("A much longer label").natural_size(cv, &th);
            assert_eq!(a.1, 32.0);
            assert!(b.0 > a.0 + 40.0);
            let ic = Button::with_icon("A", super::super::glyph_icon(crate::icons::Glyph::Plus)).natural_size(cv, &th);
            assert!((ic.0 - a.0 - 24.0).abs() < 0.01, "icon + gap = 24");
            assert_eq!(Button::icon_only("x", super::super::glyph_icon(crate::icons::Glyph::X)).natural_size(cv, &th), (28.0, 28.0));
            assert_eq!(Button::new("A").small(true).natural_size(cv, &th).1, 28.0);
        });
    }
}
