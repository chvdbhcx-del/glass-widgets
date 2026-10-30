//! `Badge`: the small status label (`.badge` of Settings: 16 px high, 4 px radius; `.chip` of Pill: 24 px pill).
//!
//! A badge that appears pops in (scale .6 -> 1 on the spring-pop curve in 500 ms, `PillMotion.pop`, with a 220 ms opacity fade that never springs); a
//! badge whose kind changes crossfades its colours in 220 ms.
use crate::canvas::Canvas;
use crate::color::Color;
use crate::ease::{curves, dur, Lerp, Tween};
use crate::geom::Rect;
use crate::text::Style;
use crate::theme::Theme;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BadgeKind {
    /// grey: raised fill, secondary text
    Neutral,
    /// accent wash, accent text
    Accent,
    /// green (VPN on, "free")
    Green,
    Red,
    Amber,
    /// link-coloured text on an accent wash ("pro")
    Pro,
}

pub struct Badge {
    pub text: String,
    kind: BadgeKind,
    /// the 24 px round chip instead of the 16 px badge
    pub pill: bool,
    /// colour crossfade from the previous kind
    from: Option<BadgeKind>,
    mix: Tween<f32>,
    scale: Tween<f32>,
    fade: Tween<f32>,
}

fn colors(th: &Theme, k: BadgeKind) -> (Color, Color) {
    match k {
        BadgeKind::Neutral => (th.raised2, th.text2),
        BadgeKind::Accent => (th.accent_soft, th.accent),
        BadgeKind::Green => (th.green_bg, th.green),
        BadgeKind::Red => (th.red_bg, th.red),
        BadgeKind::Amber => (th.amber_bg, th.amber),
        BadgeKind::Pro => (th.accent_soft, th.link),
    }
}

fn style() -> Style {
    Style::new(11.0, 600)
}

impl Badge {
    pub fn new(text: &str, kind: BadgeKind) -> Badge {
        Badge { text: text.into(), kind, pill: false, from: None, mix: Tween::at_rest(1.0), scale: Tween::at_rest(1.0), fade: Tween::at_rest(1.0) }
    }

    pub fn pill(mut self) -> Badge {
        self.pill = true;
        self
    }

    pub fn kind(&self) -> BadgeKind {
        self.kind
    }

    /// Change the kind; the colours crossfade.
    pub fn set_kind(&mut self, k: BadgeKind) {
        if k == self.kind {
            return;
        }
        self.from = Some(self.kind);
        self.kind = k;
        self.mix = Tween::new(0.0, 1.0, dur::MED, curves::EASE_OUT);
    }

    /// Play the enter animation (call when the badge appears).
    pub fn pop_in(&mut self) {
        self.scale = Tween::new(0.6, 1.0, 0.5, curves::SPRING_POP);
        self.fade = Tween::new(0.0, 1.0, dur::MED, curves::EASE_OUT);
    }

    pub fn step(&mut self, dt: f32) -> bool {
        let a = self.mix.step(dt);
        let b = self.scale.step(dt);
        let c = self.fade.step(dt);
        a | b | c
    }

    pub fn natural_size(&self, cv: &Canvas, th: &Theme) -> (f32, f32) {
        let tw = cv.text_width(&self.text, style()).ceil();
        if self.pill {
            (tw + 16.0, th.m.chip_h.min(24.0))
        } else {
            (tw + 12.0, th.m.badge_h)
        }
    }

    pub fn draw(&self, cv: &Canvas, th: &Theme, rect: Rect) {
        let (bg1, fg1) = colors(th, self.kind);
        let (bg, fg) = match self.from {
            Some(f) if !self.mix.finished() => {
                let (bg0, fg0) = colors(th, f);
                let t = self.mix.value();
                (Color::lerp(bg0, bg1, t), Color::lerp(fg0, fg1, t))
            }
            _ => (bg1, fg1),
        };
        let rad = if self.pill { rect.h / 2.0 } else { 4.0 };
        cv.with_opacity(self.fade.value().clamp(0.0, 1.0), |cv| {
            cv.with_scale_about(rect.center(), self.scale.value(), |cv| {
                cv.fill_rr(rect, rad, bg);
                cv.text(rect, &self.text, style().center(), fg);
            });
        });
    }
}

#[cfg(test)]
mod tests {
    use super::super::testutil::with_motion;
    use super::*;
    use crate::testkit::Harness;

    #[test]
    fn sizes_follow_the_text_and_the_variant() {
        let h = Harness::new();
        let th = Theme::pill_dark();
        h.render(4, 4, 1.0, th.bg, |cv| {
            let b = Badge::new("PRO", BadgeKind::Pro);
            let c = Badge::new("PRO", BadgeKind::Pro).pill();
            assert_eq!(b.natural_size(cv, &th).1, 16.0);
            assert_eq!(c.natural_size(cv, &th).1, 24.0);
            assert!(c.natural_size(cv, &th).0 > b.natural_size(cv, &th).0);
            assert!(Badge::new("LONGER", BadgeKind::Neutral).natural_size(cv, &th).0 > b.natural_size(cv, &th).0);
        });
    }

    #[test]
    fn each_kind_draws_its_own_wash() {
        with_motion(false, || {
            let h = Harness::new();
            let th = Theme::pill_dark();
            let rc = Rect::new(2.0, 2.0, 40.0, 16.0);
            let fill_of = |k| {
                let img = h.render(44, 20, 2.0, th.bg, |cv| Badge::new("OK", k).draw(cv, &th, rc));
                img.at(4.0, 4.0)
            };
            let g = fill_of(BadgeKind::Green);
            assert_eq!(g[..3], [0x00, 0x20, 0x16]);
            let n = fill_of(BadgeKind::Neutral);
            assert_eq!(n[..3], [0x29, 0x29, 0x29]);
            assert_ne!(fill_of(BadgeKind::Red)[..3], n[..3]);
        });
    }

    #[test]
    fn pop_in_scales_up_with_a_fade_and_kind_changes_crossfade() {
        with_motion(true, || {
            let h = Harness::new();
            let th = Theme::pill_dark();
            let rc = Rect::new(2.0, 2.0, 40.0, 16.0);
            let mut b = Badge::new("OK", BadgeKind::Green);
            b.pop_in();
            let first = h.render(44, 20, 2.0, th.bg, |cv| b.draw(cv, &th, rc));
            assert_eq!(first.count(None, |p| p[1] > 0x10), 0, "invisible at t = 0");
            let mut max = 0.0f32;
            while b.step(1.0 / 60.0) {
                max = max.max(b.scale.value());
            }
            assert!(max > 1.02 && b.scale.value() == 1.0, "the pop overshoots a little past 1: {max}");
            b.set_kind(BadgeKind::Red);
            b.step(0.05);
            let mid = h.render(44, 20, 2.0, th.bg, |cv| b.draw(cv, &th, rc)).at(4.0, 4.0);
            let end = {
                while b.step(0.016) {}
                h.render(44, 20, 2.0, th.bg, |cv| b.draw(cv, &th, rc)).at(4.0, 4.0)
            };
            assert_ne!(mid, end, "mid-crossfade differs from the final colour");
        });
    }
}
