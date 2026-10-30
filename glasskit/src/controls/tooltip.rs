//! `Tooltip`: the small dark bubble that names a control. One instance per window: tell it which thing the pointer is over (`hover`) and it shows
//! the bubble after a 450 ms delay (instantly when another tooltip was visible a moment ago), fading in 140 ms while rising 4 px, and fading out when
//! the pointer leaves or a button goes down. Keep it the LAST thing you draw so it floats above everything.
//!
//! The text comes from the caller (the crate holds no strings); `draw_bubble` is the same bubble for controls that show a value (the slider).
use crate::canvas::Canvas;
use crate::ease::{curves, dur, Tween};
use crate::geom::{Pt, Rect};
use crate::text::Style;
use crate::theme::Theme;

/// Seconds the pointer must rest before the first tooltip appears.
pub const SHOW_DELAY: f32 = 0.45;
/// After a tooltip hid, the next one appears at once within this long ("warm" state, like native tooltips).
pub const WARM: f32 = 0.35;
const GAP: f32 = 6.0;
const RISE: f32 = 4.0;
const PAD_X: f32 = 8.0;
const H: f32 = 22.0;

fn style() -> Style {
    Style::new(11.0, 500)
}

/// Where a bubble of `size` goes for `anchor`: centred below it (above when there is no room), kept 4 px inside `work`.
pub fn place_bubble(size: (f32, f32), anchor: Rect, work: Rect) -> Rect {
    let below = anchor.bottom() + GAP + size.1 <= work.bottom() - 4.0;
    let y = if below { anchor.bottom() + GAP } else { anchor.y - GAP - size.1 };
    let x = (anchor.cx() - size.0 / 2.0).min(work.right() - 4.0 - size.0).max(work.x + 4.0);
    Rect::new(x, y, size.0, size.1)
}

/// A bubble standing on the point `from` (its bottom centre), faded by `k` with the rise of the enter animation.
pub fn draw_bubble(cv: &Canvas, th: &Theme, from: Pt, text: &str, k: f32) {
    let w = cv.text_width(text, style()).ceil() + 2.0 * PAD_X;
    let rise = (1.0 - k) * RISE;
    let r = Rect::new(from.0 - w / 2.0, from.1 - H + rise, w, H);
    cv.with_opacity(k, |cv| paint(cv, th, r, text));
}

fn paint(cv: &Canvas, th: &Theme, r: Rect, text: &str) {
    cv.shadow_rr(r, 6.0, 8.0, 2.0, th.shadow);
    cv.fill_rr(r, 6.0, th.tip_bg);
    cv.stroke_rr(r, 6.0, 1.0, th.border);
    cv.text(r, text, style().center(), th.tip_text);
}

#[derive(Clone, Debug, Default)]
pub struct Tooltip {
    text: String,
    anchor: Rect,
    work: Rect,
    /// which thing the pointer is over (any stable id the caller picks)
    id: Option<u64>,
    waited: f32,
    since_hide: f32,
    shown: bool,
    alpha: Option<Tween<f32>>,
}

impl Tooltip {
    pub fn new() -> Tooltip {
        Tooltip { since_hide: 10.0, ..Default::default() }
    }

    /// The pointer is over thing `id` (anchored at `anchor`, tip `text`) in a window of size `work`. Call on every mouse move; cheap.
    pub fn hover(&mut self, id: u64, anchor: Rect, text: &str, work: Rect) {
        if self.id == Some(id) {
            return;
        }
        let was_shown = self.shown;
        self.id = Some(id);
        self.anchor = anchor;
        self.work = work;
        self.text = text.to_string();
        self.waited = 0.0;
        if was_shown {
            // moving from one tooltip-carrying thing to the next: swap at once, no new delay, no re-fade
            self.shown = true;
        } else if self.since_hide < WARM {
            self.waited = SHOW_DELAY;
        }
        if text.is_empty() {
            self.leave();
        }
    }

    /// The pointer left the thing (or went to something without a tip).
    pub fn leave(&mut self) {
        self.id = None;
        self.waited = 0.0;
        if self.shown {
            self.shown = false;
            self.since_hide = 0.0;
            let a = self.alpha();
            let mut t = Tween::at_rest(a);
            t.retarget(0.0, 0.1, curves::EASE_IN);
            self.alpha = Some(t);
        }
    }

    /// A button went down: hide at once and stay hidden until the pointer moves to something else.
    pub fn press(&mut self) {
        self.leave();
        if let Some(t) = &mut self.alpha {
            t.jump(0.0);
        }
    }

    pub fn is_visible(&self) -> bool {
        self.alpha() > 0.01
    }

    fn alpha(&self) -> f32 {
        self.alpha.map_or(0.0, |t| t.value())
    }

    /// Advance the delay and the fade; true while something is moving (including the wait, so the host keeps a timer running).
    pub fn step(&mut self, dt: f32) -> bool {
        self.since_hide += dt;
        let mut busy = false;
        if self.id.is_some() && !self.shown {
            self.waited += dt;
            busy = true;
            if self.waited >= SHOW_DELAY {
                self.shown = true;
                let mut t = Tween::at_rest(self.alpha());
                t.retarget(1.0, dur::FAST + 0.02, curves::EASE_OUT);
                self.alpha = Some(t);
            }
        }
        if let Some(t) = &mut self.alpha {
            busy |= t.step(dt);
        }
        busy
    }

    /// Draw the bubble (nothing when hidden).
    pub fn draw(&self, cv: &Canvas, th: &Theme) {
        let k = self.alpha().clamp(0.0, 1.0);
        if k <= 0.01 || self.text.is_empty() {
            return;
        }
        let w = cv.text_width(&self.text, style()).ceil() + 2.0 * PAD_X;
        let r = place_bubble((w, H), self.anchor, self.work);
        let rise = (1.0 - k) * RISE * if r.y < self.anchor.y { -1.0 } else { 1.0 };
        let r = Rect::new(r.x, r.y - rise, r.w, r.h);
        cv.with_opacity(k, |cv| paint(cv, th, r, &self.text));
    }

    /// The rectangle the bubble occupies when fully shown (for hit-testing / damage rectangles).
    pub fn bounds(&self, cv: &Canvas) -> Rect {
        let w = cv.text_width(&self.text, style()).ceil() + 2.0 * PAD_X;
        place_bubble((w, H), self.anchor, self.work)
    }
}

#[cfg(test)]
mod tests {
    use super::super::testutil::with_motion;
    use super::*;
    use crate::testkit::Harness;

    fn work() -> Rect {
        Rect::new(0.0, 0.0, 300.0, 200.0)
    }

    #[test]
    fn waits_the_delay_then_fades_in() {
        with_motion(true, || {
            let mut t = Tooltip::new();
            t.hover(1, Rect::new(100.0, 50.0, 30.0, 20.0), "Copy", work());
            for _ in 0..20 {
                t.step(0.016);
            }
            assert!(!t.is_visible(), "320 ms < 450 ms");
            for _ in 0..10 {
                t.step(0.016);
            }
            assert!(t.is_visible(), "past the delay it is fading in");
            for _ in 0..20 {
                t.step(0.016);
            }
            assert!((t.alpha() - 1.0).abs() < 1e-3);
            assert!(!t.step(0.016), "settled: no more frames");
        });
    }

    #[test]
    fn leaving_fades_out_and_the_next_tip_is_warm() {
        with_motion(true, || {
            let mut t = Tooltip::new();
            t.hover(1, Rect::new(10.0, 10.0, 20.0, 20.0), "A", work());
            for _ in 0..60 {
                t.step(0.016);
            }
            t.leave();
            for _ in 0..3 {
                t.step(0.016);
            }
            assert!(t.alpha() < 1.0, "fading");
            for _ in 0..10 {
                t.step(0.016);
            }
            assert!(!t.is_visible());
            // just hidden: a new target appears without the 450 ms wait
            t.hover(2, Rect::new(60.0, 10.0, 20.0, 20.0), "B", work());
            for _ in 0..3 {
                t.step(0.016);
            }
            assert!(t.alpha() > 0.0, "warm start");
            t.press();
            assert!(!t.is_visible());
        });
    }

    #[test]
    fn placement_flips_above_and_clamps() {
        let w = work();
        let below = place_bubble((60.0, 22.0), Rect::new(100.0, 50.0, 30.0, 20.0), w);
        assert_eq!(below.y, 76.0);
        let above = place_bubble((60.0, 22.0), Rect::new(100.0, 180.0, 30.0, 16.0), w);
        assert!(above.bottom() < 180.0);
        let left = place_bubble((60.0, 22.0), Rect::new(0.0, 10.0, 10.0, 10.0), w);
        assert_eq!(left.x, 4.0);
        let right = place_bubble((60.0, 22.0), Rect::new(290.0, 10.0, 10.0, 10.0), w);
        assert_eq!(right.right(), 296.0);
    }

    #[test]
    fn draws_a_dark_bubble_with_text_only_when_visible() {
        with_motion(false, || {
            let h = Harness::new();
            let th = Theme::pill_dark();
            let mut t = Tooltip::new();
            let blank = h.render(160, 80, 1.0, th.bg, |cv| t.draw(cv, &th));
            assert_eq!(blank.count(None, |p| p[0] > 0), 0);
            t.hover(1, Rect::new(60.0, 10.0, 40.0, 20.0), "Скопировать", Rect::new(0.0, 0.0, 160.0, 80.0));
            t.step(1.0);
            let img = h.render(160, 80, 1.0, th.bg, |cv| t.draw(cv, &th));
            assert!(img.count(None, |p| p[0] > 0x20) > 30, "bubble + text pixels");
            let bounds = img.bounds(|p| p[0] > 0x10).unwrap();
            assert!(bounds.1 >= 36, "below the anchor: {bounds:?}");
        });
    }
}
