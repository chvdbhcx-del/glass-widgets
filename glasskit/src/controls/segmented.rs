//! `Segmented`: the segmented control (`.seg` of Pill, Settings' `.seg` with the `.ind` indicator).
//!
//! The selected segment is marked by a **sliding pill**: when the selection changes the indicator slides (position AND width) to the new segment on the
//! spring-pop curve in 320 ms (`.seg.anim .ind { transition: transform .32s var(--spring-pop), width .32s var(--spring-pop) }`). Labels crossfade between
//! dim and bright in 120 ms, a held segment shrinks to .95. Keyboard (focused): Left / Right / Home / End move the selection.
//!
//! Segment widths come from the labels (measured on the first `draw` / `natural_size` and cached); a wider rectangle spreads the extra width evenly.
//! The indicator's destination is resolved from the CURRENT layout on every frame, so resizing the window during a slide cannot leave it behind.
use super::{Interact, Widget};
use crate::canvas::Canvas;
use crate::color::Color;
use crate::ease::{curves, dur, Lerp, Tween};
use crate::geom::Rect;
use crate::input::{InputEvent, KeyCode, Response};
use crate::theme::{Metrics, Theme};
use std::cell::{Cell, RefCell};

/// Duration of the indicator slide.
pub const SLIDE: f32 = 0.32;

pub struct Segmented {
    labels: Vec<String>,
    selected: usize,
    /// the indicator rectangle (relative to the control's top-left) the current slide starts from
    from: Rect,
    /// slide progress 0 -> 1 on the spring-pop curve (at rest = 1)
    slide: Tween<f32>,
    /// label brightness per segment (selected)
    bright: Vec<Tween<f32>>,
    items: Vec<Interact>,
    enabled: bool,
    focused: bool,
    ring: Tween<f32>,
    ring_on: bool,
    /// measured label widths (cache, filled by `natural_size` / `draw`)
    widths: RefCell<Vec<f32>>,
    /// the metrics of the last draw (events need them to place the segments)
    metrics: Cell<Metrics>,
}

impl Segmented {
    pub fn new(labels: &[&str], selected: usize) -> Segmented {
        let n = labels.len();
        let selected = selected.min(n.saturating_sub(1));
        Segmented {
            labels: labels.iter().map(|s| s.to_string()).collect(),
            selected,
            from: Rect::default(),
            slide: Tween::at_rest(1.0),
            bright: (0..n).map(|i| Tween::at_rest((i == selected) as u8 as f32)).collect(),
            items: (0..n).map(|_| Interact::new()).collect(),
            enabled: true,
            focused: false,
            ring: Tween::at_rest(0.0),
            ring_on: false,
            widths: RefCell::new(Vec::new()),
            metrics: Cell::new(Metrics::PILL),
        }
    }

    pub fn selected(&self) -> usize {
        self.selected
    }

    pub fn len(&self) -> usize {
        self.labels.len()
    }

    pub fn is_empty(&self) -> bool {
        self.labels.is_empty()
    }

    /// Absolute rectangles of the segments for a control drawn at `rect` (what the pointer hits). Before the first draw the labels are unmeasured and
    /// the segments split the width equally.
    pub fn item_rects(&self, rect: Rect) -> Vec<Rect> {
        let m = self.metrics.get();
        let n = self.labels.len();
        if n == 0 {
            return Vec::new();
        }
        let w = self.widths.borrow();
        let measured = w.len() == n;
        let nat: Vec<f32> = (0..n).map(|i| if measured { w[i] + 2.0 * m.seg_pad_x } else { 0.0 }).collect();
        let total = nat.iter().sum::<f32>() + m.seg_gap * (n as f32 - 1.0) + 2.0 * m.seg_pad;
        let extra = ((rect.w - total) / n as f32).max(0.0);
        let mut x = rect.x + m.seg_pad;
        let y = rect.cy() - m.seg_h / 2.0;
        nat.iter()
            .map(|&nw| {
                let iw = if measured { nw + extra } else { (rect.w - 2.0 * m.seg_pad - m.seg_gap * (n as f32 - 1.0)) / n as f32 };
                let r = Rect::new(x, y, iw, m.seg_h);
                x += iw + m.seg_gap;
                r
            })
            .collect()
    }

    /// The indicator rectangle (relative to `rect`'s top-left) as drawn now: the slide between where it started and the selected segment.
    fn indicator(&self, rect: Rect) -> Option<Rect> {
        let to = self.item_rects(rect).get(self.selected)?.offset(-rect.x, -rect.y);
        Some(if self.slide.finished() { to } else { Rect::lerp(self.from, to, self.slide.eased()) })
    }

    /// Select `i`; the indicator slides there when `animated` (a slide in flight turns around from where it is).
    pub fn select(&mut self, i: usize, rect: Rect, animated: bool) {
        if i >= self.labels.len() || i == self.selected {
            return;
        }
        let start = self.indicator(rect);
        self.selected = i;
        for (k, b) in self.bright.iter_mut().enumerate() {
            let v = (k == i) as u8 as f32;
            if animated {
                b.retarget(v, dur::FAST, curves::EASE_OUT);
            } else {
                b.jump(v);
            }
        }
        match (animated, start) {
            (true, Some(s)) => {
                self.from = s;
                self.slide = Tween::new(0.0, 1.0, SLIDE, curves::SPRING_POP);
            }
            _ => self.slide = Tween::at_rest(1.0),
        }
    }

    /// Select without a rectangle at hand (initial state, no animation).
    pub fn set_selected(&mut self, i: usize) {
        if i < self.labels.len() {
            self.selected = i;
            for (k, b) in self.bright.iter_mut().enumerate() {
                b.jump((k == i) as u8 as f32);
            }
            self.slide = Tween::at_rest(1.0);
        }
    }

    fn measure(&self, cv: &Canvas, th: &Theme) {
        self.metrics.set(th.m);
        let st = th.seg_text();
        let mut w = self.widths.borrow_mut();
        if w.len() != self.labels.len() {
            *w = self.labels.iter().map(|l| cv.text_width(l, st).ceil()).collect();
        }
    }
}

impl Widget for Segmented {
    fn event(&mut self, ev: &InputEvent, rect: Rect) -> Response {
        let rects = self.item_rects(rect);
        let mut out = Response::NONE;
        let mut clicked = None;
        for (i, ir) in rects.iter().enumerate() {
            let (r, p) = self.items[i].pointer(ev, |q| ir.contains(q.0, q.1));
            if p.click {
                clicked = Some(i);
            }
            out = out.merge(r);
        }
        out.changed = false;
        if let Some(i) = clicked {
            let was = self.selected;
            self.select(i, rect, true);
            out.changed = was != i;
            out.redraw = true;
        }
        if self.focused && self.enabled {
            if let Some((code, _, _)) = ev.key_down() {
                let n = self.labels.len();
                let to = match code {
                    KeyCode::Left | KeyCode::Up => Some(self.selected.saturating_sub(1)),
                    KeyCode::Right | KeyCode::Down => Some((self.selected + 1).min(n.saturating_sub(1))),
                    KeyCode::Home => Some(0),
                    KeyCode::End => Some(n.saturating_sub(1)),
                    _ => None,
                };
                if let Some(t) = to {
                    out.consumed = true;
                    out.redraw = true;
                    if t != self.selected {
                        self.select(t, rect, true);
                        out.changed = true;
                    }
                }
            }
        }
        out
    }

    fn step(&mut self, dt: f32) -> bool {
        let mut busy = self.slide.step(dt);
        busy |= self.ring.step(dt);
        for b in &mut self.bright {
            busy |= b.step(dt);
        }
        for it in &mut self.items {
            busy |= it.step(dt);
        }
        busy
    }

    fn draw(&self, cv: &Canvas, th: &Theme, rect: Rect) {
        self.measure(cv, th);
        let m = &th.m;
        let rects = self.item_rects(rect);
        cv.with_opacity(if self.enabled { 1.0 } else { 0.45 }, |cv| {
            cv.fill_rr(rect, m.seg_track_r, th.seg_bg);
            if let Some(ind) = self.indicator(rect) {
                cv.fill_rr(ind.offset(rect.x, rect.y), m.seg_r, th.seg_on);
            }
            let st = th.seg_text();
            for (i, ir) in rects.iter().enumerate() {
                let lit = self.bright[i].value().clamp(0.0, 1.0).max(self.items[i].hover());
                let hover_col = if self.selected == i { th.text } else { th.seg_hover_text };
                let col = Color::lerp(th.dim, hover_col, lit);
                cv.with_scale_about(ir.center(), self.items[i].press_scale(0.95), |cv| cv.text(*ir, &self.labels[i], st.center(), col));
            }
            cv.focus_ring(rect, m.seg_track_r, th.accent, self.ring.value().clamp(0.0, 1.0));
        });
    }

    fn natural_size(&self, cv: &Canvas, th: &Theme) -> (f32, f32) {
        self.measure(cv, th);
        let m = &th.m;
        let w: f32 = self.widths.borrow().iter().map(|w| w + 2.0 * m.seg_pad_x).sum();
        let n = self.labels.len() as f32;
        (w + m.seg_gap * (n - 1.0).max(0.0) + 2.0 * m.seg_pad, m.seg_h + 2.0 * m.seg_pad)
    }

    fn set_focus(&mut self, focused: bool, ring: bool) {
        self.focused = focused;
        let show = focused && ring;
        if show != self.ring_on {
            self.ring_on = show;
            self.ring.retarget(show as u8 as f32, dur::FAST, curves::EASE_OUT);
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

    fn setup(h: &Harness, th: &Theme, sel: usize) -> (Segmented, Rect) {
        let s = Segmented::new(&["Day", "Week", "Month"], sel);
        let mut size = (0.0, 0.0);
        h.render(4, 4, 1.0, th.bg, |cv| size = s.natural_size(cv, th));
        (s, Rect::new(10.0, 8.0, size.0, size.1))
    }

    #[test]
    fn layout_pads_gaps_and_spreads_extra_width() {
        let h = Harness::new();
        let th = Theme::pill_dark();
        let (s, r) = setup(&h, &th, 0);
        let ir = s.item_rects(r);
        assert_eq!(ir.len(), 3);
        assert_eq!(ir[0].x, r.x + 4.0);
        assert!((ir[1].x - ir[0].right() - 4.0).abs() < 1e-3, "gap 4");
        assert_eq!(ir[0].h, 24.0);
        assert!((ir[2].right() + 4.0 - r.right()).abs() < 1e-3, "natural width fits exactly");
        let wide = s.item_rects(Rect::new(r.x, r.y, r.w + 30.0, r.h));
        assert!((wide[2].right() + 4.0 - (r.right() + 30.0)).abs() < 1e-3, "extra spread evenly");
        assert!((wide[0].w - ir[0].w - 10.0).abs() < 1e-3);
        let st = Theme::settings_dark();
        let (s2, r2) = setup(&h, &st, 0);
        let i2 = s2.item_rects(r2);
        assert_eq!((i2[0].h, i2[1].x - i2[0].right()), (26.0, 0.0), "Settings metrics: 26 px segments, no gap");
    }

    #[test]
    fn click_selects_on_release_and_reports_the_change() {
        with_motion(true, || {
            let h = Harness::new();
            let th = Theme::pill_dark();
            let (mut s, r) = setup(&h, &th, 0);
            s.set_focus(false, false);
            let target = s.item_rects(r)[2].center();
            s.event(&mv(target.0, target.1), r);
            let d = s.event(&down(target.0, target.1), r);
            assert!(d.capture && d.focus && !d.changed);
            assert_eq!(s.selected(), 0);
            let u = s.event(&up(target.0, target.1), r);
            assert!(u.changed && s.selected() == 2);
            // clicking the selected one again changes nothing
            s.event(&down(target.0, target.1), r);
            assert!(!s.event(&up(target.0, target.1), r).changed);
        });
    }

    #[test]
    fn the_indicator_slides_with_an_overshoot_and_ends_on_the_segment() {
        with_motion(true, || {
            let h = Harness::new();
            let th = Theme::pill_dark();
            let (mut s, r) = setup(&h, &th, 0);
            let goal = s.item_rects(r)[2].offset(-r.x, -r.y);
            let start = s.indicator(r).unwrap();
            s.select(2, r, true);
            assert_eq!(s.indicator(r).unwrap(), start, "t = 0 is where it was");
            let mut max_x = 0.0f32;
            let mut mid = None;
            let mut frames = 0;
            while s.step(1.0 / 60.0) {
                let i = s.indicator(r).unwrap();
                max_x = max_x.max(i.x);
                frames += 1;
                if frames == 6 {
                    mid = Some(i);
                }
            }
            let mid = mid.unwrap();
            assert!(mid.x > start.x && mid.x < goal.x, "half way between");
            assert!(max_x > goal.x, "spring-pop overshoots the target position");
            assert_eq!(s.indicator(r).unwrap(), goal);
            assert!((16..=24).contains(&frames), "{} ms", frames * 1000 / 60);
        });
    }

    #[test]
    fn a_new_selection_mid_slide_continues_from_the_shown_position() {
        with_motion(true, || {
            let h = Harness::new();
            let th = Theme::pill_dark();
            let (mut s, r) = setup(&h, &th, 0);
            s.select(2, r, true);
            for _ in 0..5 {
                s.step(1.0 / 60.0);
            }
            let shown = s.indicator(r).unwrap();
            s.select(1, r, true);
            assert_eq!(s.indicator(r).unwrap(), shown, "no jump when the target changes mid-flight");
        });
    }

    #[test]
    fn keyboard_moves_the_selection_and_stops_at_the_ends() {
        with_motion(true, || {
            let h = Harness::new();
            let th = Theme::pill_dark();
            let (mut s, r) = setup(&h, &th, 1);
            assert!(!s.event(&key(KeyCode::Right), r).consumed, "needs focus");
            s.set_focus(true, true);
            assert!(s.event(&key(KeyCode::Right), r).changed && s.selected() == 2);
            let e = s.event(&key(KeyCode::Right), r);
            assert!(e.consumed && !e.changed && s.selected() == 2);
            s.event(&key(KeyCode::Home), r);
            assert_eq!(s.selected(), 0);
            s.event(&key(KeyCode::End), r);
            assert_eq!(s.selected(), 2);
        });
    }

    #[test]
    fn reduced_motion_jumps_and_the_sequence_renders() {
        with_motion(false, || {
            let h = Harness::new();
            let th = Theme::pill_dark();
            let (mut s, r) = setup(&h, &th, 0);
            s.select(2, r, true);
            assert!(!s.step(0.0));
            assert_eq!(s.indicator(r).unwrap(), s.item_rects(r)[2].offset(-r.x, -r.y));
        });
        with_motion(true, || {
            let h = Harness::new();
            let th = Theme::pill_dark();
            let (mut s, r) = setup(&h, &th, 0);
            let img = h.render(r.right() as u32 + 10, 40, 2.0, th.bg, |cv| s.draw(cv, &th, r));
            // the indicator (raised2 = #292929) sits under the first segment, not under the last
            let first = s.item_rects(r)[0];
            let last = s.item_rects(r)[2];
            let (fx, ly) = (first.x + 4.0, r.y + 8.0);
            assert_eq!(img.at(fx, ly)[..3], [0x29, 0x29, 0x29]);
            assert_eq!(img.at(last.x + 4.0, ly)[..3], [0x0f, 0x0f, 0x0f]);
            s.select(2, r, true);
            for _ in 0..30 {
                s.step(1.0 / 60.0);
            }
            let img2 = h.render(r.right() as u32 + 10, 40, 2.0, th.bg, |cv| s.draw(cv, &th, r));
            assert_eq!(img2.at(last.x + 4.0, ly)[..3], [0x29, 0x29, 0x29]);
            assert_eq!(img2.at(fx, ly)[..3], [0x0f, 0x0f, 0x0f]);
        });
    }
}
