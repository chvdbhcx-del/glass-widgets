//! Scrolling for the kit: the physics ([`ScrollState`]), the fading overlay scrollbar ([`Scrollbar`]), a virtualised list of variable-height rows
//! ([`ScrollList`] over a [`RowSource`]) and a plain clipped region ([`ScrollView`]).
//!
//! * [`ScrollState`]  one axis: content / viewport length, an offset that glides on a critically damped [`Spring`] when a wheel notch or a
//!                    keyboard command moves it, follows a touchpad (`precise` wheel events) exactly, and flings with decaying velocity when the
//!                    fingers lift. Pure and clock-free: `step(dt)` returns `true` while it still moves. Reduced motion jumps.
//! * [`Scrollbar`]    a 6 px overlay thumb (10 px under the pointer) that fades in on scrolling / hover / drag and out after 0.9 s idle.
//!                    Thumb maths are plain functions ([`thumb_span`], [`offset_for_thumb`]).
//! * [`ScrollList`]   rows of different heights from a [`RowSource`]; heights are measured lazily (only rows near the viewport) into a Fenwick
//!                    tree, so 10 000 rows cost a few dozen `height()` calls; `scroll_to_index`, hover row, keyboard selection, stick-to-bottom for
//!                    chats, optional fade + rise entrance (28 ms stagger) for added rows.
//! * [`ScrollView`]   one tall content block drawn through a closure.
//!
//! ```ignore
//! struct Msgs(Vec<String>);
//! impl RowSource for Msgs {
//!     fn len(&self) -> usize { self.0.len() }
//!     fn height(&self, i: usize, width: f32) -> f32 { 24.0 + (self.0[i].len() as f32 * 7.0 / width).floor() * 16.0 }
//!     fn draw_row(&self, cv: &Canvas, i: usize, r: Rect, st: RowState) { /* paint self.0[i] in r */ }
//! }
//! let mut list = ScrollList::new(48.0);          // 48 px = the guess for rows not measured yet
//! list.stick_to_end = true;                      // a chat follows new messages until the user scrolls up
//! // per input event:   let resp = list.event(&ev, &src, view);
//! // per frame:         if list.step(dt, &src, view) { /* ask for another frame */ }
//! // per paint:         list.draw(&cv, &src, view, &theme);
//! ```
//! A host that never calls `step` still gets correct geometry (`event` and `step` both call [`ScrollList::update`]); it just gets no animation.
use crate::canvas::Canvas;
use crate::ease::{self, curves, dur, Spring, Tween};
use crate::geom::{Pt, Rect};
use crate::input::{Cursor, InputEvent, KeyCode, Modifiers, MouseButton, Response};
use crate::theme::Theme;
use std::ops::Range;

/// One keyboard "line" in px (what Chromium scrolls on an arrow key).
pub const LINE_PX: f32 = 40.0;
/// Stiffness (omega squared) of the wheel glide: ~0.2 s to settle after a notch.
const GLIDE_K: f32 = 700.0;
/// Fling: below this speed (px/s) the inertia stops; the speed decays with this time constant (s).
const FLING_MIN: f32 = 60.0;
const FLING_TAU: f32 = 0.30;
/// A precise (touchpad) scroll with no new event for this long (s) has ended: fling starts.
const FLING_GAP: f32 = 0.05;

fn motion_off(o: Option<bool>) -> bool {
    o.unwrap_or_else(ease::reduced)
}

// ================================================================ ScrollState

/// One scrolling axis. See the module doc.
#[derive(Clone, Debug)]
pub struct ScrollState {
    spring: Spring,
    content: f32,
    viewport: f32,
    /// fling speed in px/s (positive = towards the end)
    vel: f32,
    flinging: bool,
    /// speed estimate from the last direct (touchpad) deltas
    est: f32,
    since_direct: f32,
    pending_fling: bool,
    /// test hook: `Some(b)` overrides the system "reduce motion" setting
    pub reduced: Option<bool>,
}

impl Default for ScrollState {
    fn default() -> Self {
        ScrollState::new()
    }
}

impl ScrollState {
    pub fn new() -> ScrollState {
        ScrollState { spring: Spring::new(0.0, GLIDE_K, 1.0), content: 0.0, viewport: 0.0, vel: 0.0, flinging: false, est: 0.0, since_direct: f32::INFINITY, pending_fling: false, reduced: None }
    }

    /// Length of the content and of the visible window along the axis. The offset is clamped into the new range.
    pub fn set_metrics(&mut self, content: f32, viewport: f32) {
        self.content = content.max(0.0);
        self.viewport = viewport.max(0.0);
        let m = self.max_offset();
        self.spring.target = self.spring.target.clamp(0.0, m);
        if self.spring.x > m || self.spring.x < 0.0 {
            self.spring.x = self.spring.x.clamp(0.0, m);
            self.spring.v = 0.0;
        }
    }

    pub fn content(&self) -> f32 {
        self.content
    }
    pub fn viewport(&self) -> f32 {
        self.viewport
    }
    /// Largest offset: `content - viewport`, never negative.
    pub fn max_offset(&self) -> f32 {
        (self.content - self.viewport).max(0.0)
    }
    /// Can the content move at all?
    pub fn scrollable(&self) -> bool {
        self.max_offset() > 0.5
    }
    /// The offset to draw at now.
    pub fn offset(&self) -> f32 {
        self.spring.x
    }
    /// Where the offset is heading (equals `offset()` at rest).
    pub fn target(&self) -> f32 {
        self.spring.target
    }
    /// Is the INTENDED position the start / the end (so a glide that has not arrived yet counts as arrived)?
    pub fn at_start(&self) -> bool {
        self.spring.target <= 0.5
    }
    pub fn at_end(&self) -> bool {
        self.spring.target >= self.max_offset() - 0.5
    }

    fn off(&self) -> bool {
        motion_off(self.reduced)
    }

    /// Move offset and target together (content above the viewport changed size); no animation.
    fn shift(&mut self, d: f32) {
        let m = self.max_offset().max(self.spring.x + d);
        self.spring.x = (self.spring.x + d).clamp(0.0, m);
        self.spring.target = (self.spring.target + d).clamp(0.0, m);
    }

    fn stop_fling(&mut self) {
        self.flinging = false;
        self.pending_fling = false;
        self.vel = 0.0;
        self.est = 0.0;
    }

    /// Go to `offset` (clamped): glide when `animated`, else at once. Reduced motion always jumps.
    pub fn scroll_to(&mut self, offset: f32, animated: bool) {
        let o = offset.clamp(0.0, self.max_offset());
        self.stop_fling();
        if animated && !self.off() {
            self.spring.set_target(o);
        } else {
            self.spring.jump(o);
        }
    }

    /// Move by `px` from where the glide is heading (repeated notches add up), or from the shown offset when not animated.
    pub fn scroll_by(&mut self, px: f32, animated: bool) {
        let base = if animated && !self.flinging { self.spring.target } else { self.spring.x };
        self.scroll_to(base + px, animated);
    }

    /// A wheel event: a notch (`precise` false) glides and accumulates, a touchpad delta moves the content exactly and arms the fling.
    /// True when the intended position changed (a parent should get the event when it did not: the child is at its end).
    pub fn wheel(&mut self, delta: f32, precise: bool) -> bool {
        let before = self.spring.target;
        if !precise {
            self.scroll_by(delta, true);
        } else {
            let o = (self.spring.x + delta).clamp(0.0, self.max_offset());
            let dt = self.since_direct.clamp(1.0 / 240.0, 0.1);
            self.est = if self.est == 0.0 { delta / dt } else { self.est * 0.5 + delta / dt * 0.5 };
            self.since_direct = 0.0;
            self.flinging = false;
            self.vel = 0.0;
            self.spring.jump(o);
            self.pending_fling = !self.off();
        }
        (self.spring.target - before).abs() > 1e-4
    }

    /// Keyboard scrolling: Up / Down (`line` px), PageUp / PageDown / Space (a window minus a sliver), Home / End. True when the key is ours.
    pub fn key(&mut self, code: KeyCode, mods: Modifiers, line: f32) -> bool {
        if !self.scrollable() {
            return false;
        }
        let page = (self.viewport * 0.875).max(line);
        match code {
            KeyCode::Up => self.scroll_by(-line, true),
            KeyCode::Down => self.scroll_by(line, true),
            KeyCode::PageUp => self.scroll_by(-page, true),
            KeyCode::PageDown => self.scroll_by(page, true),
            KeyCode::Space => self.scroll_by(if mods.shift { -page } else { page }, true),
            KeyCode::Home => self.scroll_to(0.0, true),
            KeyCode::End => self.scroll_to(self.max_offset(), true),
            _ => return false,
        }
        true
    }

    /// Advance by `dt` seconds; true while anything still moves (or a fling is about to start): ask the host for another frame.
    pub fn step(&mut self, dt: f32) -> bool {
        if self.off() {
            self.spring.jump(self.spring.target);
            self.stop_fling();
            return false;
        }
        self.since_direct += dt;
        if self.pending_fling && self.since_direct > FLING_GAP {
            self.pending_fling = false;
            if self.est.abs() > FLING_MIN * 2.0 {
                self.vel = self.est;
                self.flinging = true;
            }
            self.est = 0.0;
        }
        if self.flinging {
            let m = self.max_offset();
            let x = (self.spring.x + self.vel * dt).clamp(0.0, m);
            self.vel *= (-dt / FLING_TAU).exp();
            if x <= 0.0 || x >= m || self.vel.abs() < FLING_MIN {
                self.flinging = false;
                self.vel = 0.0;
            }
            self.spring.jump(x);
            return self.flinging;
        }
        self.spring.step(dt) || self.pending_fling
    }
}

// ================================================================ scrollbar maths

/// Thumb of a scrollbar along a track of `track` px: `(position, length)`, or `None` when everything is visible. The length is the visible
/// share of the track but at least `min_thumb`.
pub fn thumb_span(track: f32, content: f32, viewport: f32, offset: f32, min_thumb: f32) -> Option<(f32, f32)> {
    if content <= viewport + 0.5 || track <= 0.0 || content <= 0.0 {
        return None;
    }
    let len = (track * viewport / content).clamp(min_thumb.min(track), track);
    let max = content - viewport;
    Some(((track - len) * (offset / max).clamp(0.0, 1.0), len))
}

/// Inverse of [`thumb_span`]: the content offset for a thumb whose start sits `thumb_pos` px down the track.
pub fn offset_for_thumb(track: f32, content: f32, viewport: f32, thumb_pos: f32, min_thumb: f32) -> f32 {
    let Some((_, len)) = thumb_span(track, content, viewport, 0.0, min_thumb) else { return 0.0 };
    let room = track - len;
    if room <= 0.0 {
        return 0.0;
    }
    (thumb_pos / room).clamp(0.0, 1.0) * (content - viewport)
}

// ================================================================ Scrollbar

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Axis {
    Vertical,
    Horizontal,
}

/// Overlay scrollbar. Thickness 6 px, 10 px when the pointer is over its zone; 2 px from the edge; the thumb is at least 28 px.
pub const BAR_THIN: f32 = 6.0;
pub const BAR_WIDE: f32 = 10.0;
const BAR_EDGE: f32 = 2.0;
/// width of the pointer zone along the cross axis
const BAR_ZONE: f32 = 14.0;
const BAR_MIN_THUMB: f32 = 28.0;
/// idle time before the bar fades out (s)
pub const BAR_IDLE: f32 = 0.9;
const BAR_FADE_IN: f32 = 0.12;
const BAR_FADE_OUT: f32 = 0.30;

#[derive(Clone, Debug)]
pub struct Scrollbar {
    pub axis: Axis,
    alpha: Tween<f32>,
    wide: Tween<f32>,
    idle: f32,
    hover: bool,
    /// while dragging: where on the thumb (px from its start) the pointer grabbed it
    drag: Option<f32>,
    pub reduced: Option<bool>,
}

impl Scrollbar {
    pub fn new(axis: Axis) -> Scrollbar {
        Scrollbar { axis, alpha: Tween::at_rest(0.0), wide: Tween::at_rest(0.0), idle: 0.0, hover: false, drag: None, reduced: None }
    }

    fn off(&self) -> bool {
        motion_off(self.reduced)
    }

    fn go(t: &mut Tween<f32>, to: f32, secs: f32, e: ease::Ease, off: bool) {
        if off {
            t.jump(to);
        } else {
            t.retarget(to, secs, e);
        }
    }

    /// Current opacity 0..1.
    pub fn alpha(&self) -> f32 {
        self.alpha.value()
    }
    pub fn dragging(&self) -> bool {
        self.drag.is_some()
    }
    pub fn hovered(&self) -> bool {
        self.hover
    }

    /// Show the bar now and restart the idle timer (call on any scrolling).
    pub fn show(&mut self) {
        self.idle = 0.0;
        let off = self.off();
        Self::go(&mut self.alpha, 1.0, BAR_FADE_IN, curves::EASE_OUT, off);
    }

    /// Pointer zone (hit area) of the bar inside the scrolled viewport `view`.
    pub fn zone(&self, view: Rect) -> Rect {
        match self.axis {
            Axis::Vertical => Rect::new(view.right() - BAR_ZONE, view.y + BAR_EDGE, BAR_ZONE, (view.h - 2.0 * BAR_EDGE).max(0.0)),
            Axis::Horizontal => Rect::new(view.x + BAR_EDGE, view.bottom() - BAR_ZONE, (view.w - 2.0 * BAR_EDGE).max(0.0), BAR_ZONE),
        }
    }

    fn track_len(&self, view: Rect) -> f32 {
        match self.axis {
            Axis::Vertical => (view.h - 2.0 * BAR_EDGE).max(0.0),
            Axis::Horizontal => (view.w - 2.0 * BAR_EDGE).max(0.0),
        }
    }

    fn along(&self, view: Rect, p: Pt) -> f32 {
        match self.axis {
            Axis::Vertical => p.1 - view.y - BAR_EDGE,
            Axis::Horizontal => p.0 - view.x - BAR_EDGE,
        }
    }

    /// The thumb as drawn now (its thickness follows the hover animation); `None` when nothing scrolls.
    pub fn thumb(&self, view: Rect, st: &ScrollState) -> Option<Rect> {
        let track = self.track_len(view);
        let (pos, len) = thumb_span(track, st.content(), st.viewport(), st.offset(), BAR_MIN_THUMB)?;
        let w = BAR_THIN + (BAR_WIDE - BAR_THIN) * self.wide.value();
        Some(match self.axis {
            Axis::Vertical => Rect::new(view.right() - BAR_EDGE - w, view.y + BAR_EDGE + pos, w, len),
            Axis::Horizontal => Rect::new(view.x + BAR_EDGE + pos, view.bottom() - BAR_EDGE - w, len, w),
        })
    }

    fn thumb_along(&self, view: Rect, st: &ScrollState) -> Option<(f32, f32)> {
        thumb_span(self.track_len(view), st.content(), st.viewport(), st.offset(), BAR_MIN_THUMB)
    }

    /// Pointer events: hover (fades the bar in, widens it), press on the thumb (drag, mouse captured), press on the track (page towards it),
    /// release. Wheel events are the owner's: call [`Scrollbar::show`] when it scrolls.
    pub fn event(&mut self, ev: &InputEvent, view: Rect, st: &mut ScrollState) -> Response {
        let off = self.off();
        match ev {
            InputEvent::MouseMove { pos, .. } => {
                if let Some(grab) = self.drag {
                    let a = self.along(view, *pos) - grab;
                    let o = offset_for_thumb(self.track_len(view), st.content(), st.viewport(), a, BAR_MIN_THUMB);
                    st.scroll_to(o, false);
                    self.show();
                    return Response { consumed: true, redraw: true, capture: true, changed: true, ..Response::NONE };
                }
                let inside = st.scrollable() && self.zone(view).contains(pos.0, pos.1);
                if inside != self.hover {
                    self.hover = inside;
                    Self::go(&mut self.wide, if inside { 1.0 } else { 0.0 }, ease::dur::FAST, curves::EASE_OUT, off);
                    if inside {
                        self.show();
                    }
                    return Response { redraw: true, ..Response::NONE };
                }
                if inside {
                    self.idle = 0.0;
                }
                Response::NONE
            }
            InputEvent::MouseDown { pos, button: MouseButton::Left, .. } if st.scrollable() && self.zone(view).contains(pos.0, pos.1) => {
                let a = self.along(view, *pos);
                let Some((tp, tl)) = self.thumb_along(view, st) else { return Response::NONE };
                self.show();
                if a >= tp && a <= tp + tl {
                    self.drag = Some(a - tp);
                    Response { consumed: true, redraw: true, capture: true, ..Response::NONE }
                } else {
                    let page = (st.viewport() * 0.875).max(LINE_PX);
                    st.scroll_by(if a < tp { -page } else { page }, true);
                    Response { consumed: true, redraw: true, changed: true, ..Response::NONE }
                }
            }
            InputEvent::MouseUp { button: MouseButton::Left, .. } if self.drag.is_some() => {
                self.drag = None;
                self.idle = 0.0;
                Response { consumed: true, redraw: true, ..Response::NONE }
            }
            InputEvent::MouseLeave if self.hover => {
                self.hover = false;
                Self::go(&mut self.wide, 0.0, ease::dur::FAST, curves::EASE_OUT, off);
                Response { redraw: true, ..Response::NONE }
            }
            _ => Response::NONE,
        }
    }

    /// Advance the fade / widen animations and the idle timer. True while frames are needed (an animation runs or the bar waits to fade out).
    pub fn step(&mut self, dt: f32) -> bool {
        let off = self.off();
        let mut busy = self.alpha.step(dt);
        busy |= self.wide.step(dt);
        self.idle += dt;
        let shown = self.alpha.target() > 0.5;
        if shown {
            if self.idle >= BAR_IDLE && !self.hover && self.drag.is_none() {
                Self::go(&mut self.alpha, 0.0, BAR_FADE_OUT, curves::EASE_IN, off);
                busy |= !off;
            } else {
                busy = true; // waiting for the idle time to run out
            }
        }
        busy
    }

    pub fn draw(&self, cv: &Canvas, view: Rect, st: &ScrollState, t: &Theme) {
        let a = self.alpha.value();
        if a <= 0.01 {
            return;
        }
        let Some(th) = self.thumb(view, st) else { return };
        let k = if self.drag.is_some() { 1.0 } else { self.wide.value() };
        let w = self.wide.value();
        if w > 0.01 {
            // a faint gutter under the thumb while the pointer is on the bar
            let g = match self.axis {
                Axis::Vertical => Rect::new(th.x, view.y + BAR_EDGE, th.w, self.track_len(view)),
                Axis::Horizontal => Rect::new(view.x + BAR_EDGE, th.y, self.track_len(view), th.h),
            };
            cv.fill_rr(g, th.w.min(th.h) / 2.0, t.hover.fade(a * w));
        }
        cv.fill_rr(th, th.w.min(th.h) / 2.0, t.scrollbar.mix(t.scrollbar_hover, k).fade(a));
    }
}

// ================================================================ rows

/// What a row is told about itself when it is drawn.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RowState {
    pub index: usize,
    pub hover: bool,
    pub selected: bool,
}

/// The rows of a [`ScrollList`]. The list never stores the source: it is passed to every call, so the caller owns and mutates its data freely
/// (then calls `invalidate_row` / `invalidate_all` if heights changed).
pub trait RowSource {
    fn len(&self) -> usize;
    /// Height of row `index` when laid out `width` px wide. Called lazily, only for rows near the viewport.
    fn height(&self, index: usize, width: f32) -> f32;
    /// Paint row `index` into `rect` (already clipped to the list; enter animation is applied by the list).
    fn draw_row(&self, cv: &Canvas, index: usize, rect: Rect, state: RowState);
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Align {
    Start,
    Center,
    End,
    /// scroll only as far as needed to bring the row fully into view
    Nearest,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EnterMode {
    Off,
    /// rows appended to the source fade and rise in
    Added,
    /// like `Added`, and the rows visible at the first `update` enter with the stagger (a page opening)
    FirstShow,
}

/// What the user did to the list; drain with [`ScrollList::take_events`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListEvent {
    Click(usize),
    /// Enter on the selected row
    Activate(usize),
    SelectionChanged(usize),
}

/// Fenwick tree over row heights: prefix sums and "which row holds y" in O(log n) with point updates.
#[derive(Clone, Debug, Default)]
struct Fenwick {
    t: Vec<f64>,
}

impl Fenwick {
    fn build(v: &[f32]) -> Fenwick {
        let n = v.len();
        let mut t = vec![0.0f64; n + 1];
        for i in 1..=n {
            t[i] += v[i - 1] as f64;
            let j = i + (i & i.wrapping_neg());
            if j <= n {
                let x = t[i];
                t[j] += x;
            }
        }
        Fenwick { t }
    }
    fn n(&self) -> usize {
        self.t.len().saturating_sub(1)
    }
    fn add(&mut self, i: usize, d: f64) {
        let mut k = i + 1;
        while k <= self.n() {
            self.t[k] += d;
            k += k & k.wrapping_neg();
        }
    }
    /// Sum of the first `i` rows.
    fn prefix(&self, i: usize) -> f64 {
        let (mut k, mut s) = (i.min(self.n()), 0.0);
        while k > 0 {
            s += self.t[k];
            k -= k & k.wrapping_neg();
        }
        s
    }
    /// Number of rows that end at or before `y` (= the index of the row holding `y`, `n` when `y` is past the end).
    fn lower(&self, y: f64) -> usize {
        let n = self.n();
        if n == 0 {
            return 0;
        }
        let (mut pos, mut rem) = (0usize, y);
        let mut step = 1usize << (usize::BITS - 1 - n.leading_zeros());
        while step > 0 {
            if pos + step <= n && self.t[pos + step] <= rem {
                pos += step;
                rem -= self.t[pos];
            }
            step >>= 1;
        }
        pos
    }
}

#[derive(Clone, Copy, Debug)]
struct Enter {
    index: usize,
    age: f32,
    delay: f32,
}

const ENTER_RISE: f32 = 6.0;
/// rows measured beyond the viewport on each side (px)
const OVERSCAN: f32 = 240.0;

/// A virtualised, animated list; see the module doc.
pub struct ScrollList {
    pub state: ScrollState,
    pub bar: Scrollbar,
    eff: Vec<f32>,
    measured: Vec<bool>,
    fen: Fenwick,
    estimate: f32,
    width: f32,
    view_h: f32,
    total: f32,
    len: usize,
    first: bool,
    hover: Option<usize>,
    selected: Option<usize>,
    pinned: bool,
    /// follow rows appended at the end (and growth of the last row) while the user has not scrolled away from the end
    pub stick_to_end: bool,
    /// Up / Down / Page / Home / End move the selection (and scroll it into view) instead of scrolling the viewport
    pub keyboard_select: bool,
    pub enter: EnterMode,
    enters: Vec<Enter>,
    events: Vec<ListEvent>,
    pub reduced: Option<bool>,
}

impl ScrollList {
    /// `estimate`: the height assumed for rows that were not measured yet (it is replaced by the mean of measured rows after a width change).
    pub fn new(estimate: f32) -> ScrollList {
        ScrollList {
            state: ScrollState::new(),
            bar: Scrollbar::new(Axis::Vertical),
            eff: Vec::new(),
            measured: Vec::new(),
            fen: Fenwick::default(),
            estimate: estimate.max(1.0),
            width: f32::NAN,
            view_h: 0.0,
            total: 0.0,
            len: 0,
            first: true,
            hover: None,
            selected: None,
            pinned: true,
            stick_to_end: false,
            keyboard_select: true,
            enter: EnterMode::Off,
            enters: Vec::new(),
            events: Vec::new(),
            reduced: None,
        }
    }

    fn off(&self) -> bool {
        motion_off(self.reduced)
    }

    pub fn len(&self) -> usize {
        self.len
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
    pub fn selected(&self) -> Option<usize> {
        self.selected
    }
    pub fn hovered(&self) -> Option<usize> {
        self.hover
    }
    pub fn set_selected(&mut self, i: Option<usize>) {
        self.selected = i.filter(|&i| i < self.len);
    }
    /// Total content height as currently known (measured rows exact, the rest estimated).
    pub fn content_height(&self) -> f32 {
        self.total
    }
    /// Is the viewport pinned to the end (relevant with `stick_to_end`)?
    pub fn pinned(&self) -> bool {
        self.pinned
    }
    pub fn take_events(&mut self) -> Vec<ListEvent> {
        std::mem::take(&mut self.events)
    }
    /// Top of row `i` in content coordinates.
    pub fn row_top(&self, i: usize) -> f32 {
        self.fen.prefix(i) as f32
    }
    /// Height of row `i` as currently known.
    pub fn row_height(&self, i: usize) -> f32 {
        self.eff.get(i).copied().unwrap_or(0.0)
    }

    fn rebuild(&mut self) {
        self.fen = Fenwick::build(&self.eff);
        self.total = self.fen.prefix(self.len) as f32;
    }

    fn set_height(&mut self, i: usize, h: f32) {
        let h = h.max(0.0);
        let d = h as f64 - self.eff[i] as f64;
        self.eff[i] = h;
        self.measured[i] = true;
        self.fen.add(i, d);
        self.total += d as f32;
    }

    fn range_for(&self, y0: f32, y1: f32) -> Range<usize> {
        if self.len == 0 {
            return 0..0;
        }
        let first = self.fen.lower(y0.max(0.0) as f64).min(self.len - 1);
        let last = self.fen.lower((y1 - 0.01).max(y0) as f64).min(self.len - 1);
        first..last + 1
    }

    /// Rows that intersect the viewport.
    pub fn visible_range(&self) -> Range<usize> {
        let o = self.state.offset();
        self.range_for(o, o + self.view_h)
    }

    /// Mark one row's height stale (its content changed, e.g. a streaming message grew); it is measured again on the next update.
    pub fn invalidate_row(&mut self, i: usize) {
        if let Some(m) = self.measured.get_mut(i) {
            *m = false;
        }
    }

    /// Mark every height stale (a font or a style changed). Rows keep their old height until measured again.
    pub fn invalidate_all(&mut self) {
        self.measured.iter_mut().for_each(|m| *m = false);
    }

    fn measure_range(&mut self, src: &dyn RowSource, r: Range<usize>) -> bool {
        let mut changed = false;
        for i in r {
            if i < self.len && !self.measured[i] {
                let h = src.height(i, self.width);
                if (h - self.eff[i]).abs() > 1e-4 {
                    changed = true;
                }
                self.set_height(i, h);
            }
        }
        changed
    }

    /// Measure the rows near the viewport. Rows ABOVE the first visible one that turn out taller or shorter than their estimate shift the
    /// offset by the difference, so what the user looks at does not jump while scrolling up through unmeasured rows.
    fn measure_visible(&mut self, src: &dyn RowSource) {
        for _ in 0..4 {
            let o = self.state.offset();
            let first = self.range_for(o, o + self.view_h).start;
            let r = self.range_for(o - OVERSCAN, o + self.view_h + OVERSCAN);
            let above_before = self.row_top(first);
            if !self.measure_range(src, r) {
                break;
            }
            let shift = self.row_top(first) - above_before;
            if shift != 0.0 {
                self.state.shift(shift);
            }
        }
    }

    /// Measure every row now (short lists, tests, a jump to a far row that must land exactly).
    pub fn measure_all(&mut self, src: &dyn RowSource) {
        self.sync_len(src);
        self.measure_range(src, 0..self.len);
        self.state.set_metrics(self.total, self.view_h);
    }

    fn sync_len(&mut self, src: &dyn RowSource) -> bool {
        let n = src.len();
        if n == self.len {
            return false;
        }
        let old = self.len;
        if n > old {
            self.eff.extend(std::iter::repeat(self.estimate).take(n - old));
            self.measured.extend(std::iter::repeat(false).take(n - old));
            let added = n - old;
            if self.enter != EnterMode::Off && !self.first && added <= 64 && !self.off() {
                for k in 0..added {
                    self.enters.push(Enter { index: old + k, age: 0.0, delay: k.min(8) as f32 * dur::STAGGER });
                }
            }
        } else {
            self.eff.truncate(n);
            self.measured.truncate(n);
            self.enters.retain(|e| e.index < n);
        }
        self.len = n;
        self.hover = self.hover.filter(|&i| i < n);
        self.selected = self.selected.filter(|&i| i < n);
        self.rebuild();
        n > old
    }

    fn relayout_width(&mut self) {
        let r = self.visible_range();
        let anchor = r.start;
        let within = (self.state.offset() - self.row_top(anchor)).max(0.0);
        let (sum, cnt) = self.eff.iter().zip(&self.measured).filter(|(_, &m)| m).fold((0.0f32, 0usize), |(s, c), (h, _)| (s + h, c + 1));
        if cnt > 0 {
            self.estimate = (sum / cnt as f32).max(1.0);
        }
        self.eff.iter_mut().for_each(|h| *h = self.estimate);
        self.measured.iter_mut().for_each(|m| *m = false);
        self.rebuild();
        self.state.set_metrics(self.total, self.view_h);
        let was_end = self.stick_to_end && self.pinned;
        if !was_end {
            let o = self.row_top(anchor) + within.min(self.estimate);
            self.state.scroll_to(o, false);
        }
    }

    /// Bring the geometry up to date with the source and the view: appended / removed rows, a new width (the first visible row stays put),
    /// heights of the rows near the viewport, stick-to-end. Cheap when nothing changed; `event` and `step` call it.
    pub fn update(&mut self, src: &dyn RowSource, view: Rect) {
        self.view_h = view.h;
        if !self.width.is_nan() && (view.w - self.width).abs() > 0.5 && self.len > 0 {
            self.width = view.w;
            self.relayout_width();
        }
        self.width = view.w;
        let grew = self.sync_len(src);
        let before = self.total;
        self.state.set_metrics(self.total, view.h);
        self.measure_visible(src);
        self.state.set_metrics(self.total, view.h);
        if self.first {
            self.first = false;
            if self.enter == EnterMode::FirstShow && !self.off() {
                for (k, i) in self.visible_range().enumerate() {
                    self.enters.push(Enter { index: i, age: 0.0, delay: k.min(8) as f32 * dur::STAGGER });
                }
            }
            if self.stick_to_end {
                self.state.scroll_to(self.state.max_offset(), false);
            }
        } else if self.stick_to_end && self.pinned && (grew || self.total > before + 0.5) {
            self.state.scroll_to(self.state.max_offset(), true);
        }
    }

    /// Scroll so row `i` is placed by `align`. The row is measured first, so its own height is exact (rows above it may still be estimates
    /// until they come near the viewport; call [`ScrollList::measure_all`] first when the target must land to the pixel).
    pub fn scroll_to_index(&mut self, src: &dyn RowSource, i: usize, animated: bool, align: Align) {
        self.sync_len(src);
        if i >= self.len {
            return;
        }
        self.measure_range(src, i..i + 1);
        self.state.set_metrics(self.total, self.view_h);
        let (top, h, vh) = (self.row_top(i), self.row_height(i), self.view_h);
        let cur = self.state.target();
        let o = match align {
            Align::Start => top,
            Align::End => top + h - vh,
            Align::Center => top + h / 2.0 - vh / 2.0,
            Align::Nearest => {
                if top < cur {
                    top
                } else if top + h > cur + vh {
                    top + h - vh
                } else {
                    cur
                }
            }
        };
        self.state.scroll_to(o, animated);
        self.pinned = self.state.at_end();
        self.bar.show();
    }

    /// Row under the point `p` (window coordinates) in a list drawn at `view`.
    pub fn row_at(&self, view: Rect, p: Pt) -> Option<usize> {
        if !view.contains(p.0, p.1) || self.len == 0 {
            return None;
        }
        let y = (p.1 - view.y + self.state.offset()) as f64;
        let i = self.fen.lower(y);
        (i < self.len && y < self.fen.prefix(self.len)).then_some(i)
    }

    fn rows_per_page(&self) -> usize {
        self.visible_range().len().max(1)
    }

    fn navigate(&mut self, src: &dyn RowSource, code: KeyCode) -> bool {
        let n = self.len;
        if n == 0 {
            return false;
        }
        let page = self.rows_per_page();
        let cur = self.selected;
        let t = match code {
            KeyCode::Up => cur.map_or(n - 1, |c| c.saturating_sub(1)),
            KeyCode::Down => cur.map_or(0, |c| (c + 1).min(n - 1)),
            KeyCode::PageUp => cur.unwrap_or(0).saturating_sub(page),
            KeyCode::PageDown => (cur.unwrap_or(0) + page).min(n - 1),
            KeyCode::Home => 0,
            KeyCode::End => n - 1,
            _ => return false,
        };
        if cur != Some(t) {
            self.selected = Some(t);
            self.events.push(ListEvent::SelectionChanged(t));
        }
        self.scroll_to_index(src, t, true, Align::Nearest);
        true
    }

    /// Pointer, wheel and key events. `view` is where the list is drawn (window coordinates).
    pub fn event(&mut self, ev: &InputEvent, src: &dyn RowSource, view: Rect) -> Response {
        self.update(src, view);
        let r = self.bar.event(ev, view, &mut self.state);
        if r.consumed || r.capture {
            self.pinned = self.state.at_end();
            return r;
        }
        match ev {
            InputEvent::Wheel { pos, delta, precise, .. } if view.contains(pos.0, pos.1) => {
                let moved = self.state.wheel(delta.1, *precise);
                if moved {
                    self.bar.show();
                    self.pinned = self.state.at_end();
                }
                Response { consumed: moved, redraw: moved, ..r }
            }
            InputEvent::MouseMove { pos, .. } => {
                let h = self.row_at(view, *pos);
                let ch = h != self.hover;
                self.hover = h;
                if view.contains(pos.0, pos.1) {
                    self.bar.show_if_hot();
                }
                Response { redraw: ch || r.redraw, ..r }
            }
            InputEvent::MouseLeave => {
                let ch = self.hover.take().is_some();
                Response { redraw: ch || r.redraw, ..r }
            }
            InputEvent::MouseDown { pos, button: MouseButton::Left, .. } if view.contains(pos.0, pos.1) => match self.row_at(view, *pos) {
                Some(i) => {
                    if self.selected != Some(i) {
                        self.selected = Some(i);
                        self.events.push(ListEvent::SelectionChanged(i));
                    }
                    self.events.push(ListEvent::Click(i));
                    Response { consumed: true, redraw: true, changed: true, focus: true, cursor: Some(Cursor::Arrow), ..r }
                }
                None => Response { focus: true, ..r },
            },
            InputEvent::Key { code, mods, down: true, .. } => {
                if *code == KeyCode::Enter && self.keyboard_select {
                    return match self.selected {
                        Some(i) => {
                            self.events.push(ListEvent::Activate(i));
                            Response { consumed: true, changed: true, ..r }
                        }
                        None => r,
                    };
                }
                let used = if self.keyboard_select && matches!(code, KeyCode::Up | KeyCode::Down | KeyCode::PageUp | KeyCode::PageDown | KeyCode::Home | KeyCode::End) {
                    self.navigate(src, *code)
                } else {
                    let u = self.state.key(*code, *mods, LINE_PX);
                    if u {
                        self.pinned = self.state.at_end();
                        self.bar.show();
                    }
                    u
                };
                Response { consumed: used, redraw: used, ..r }
            }
            _ => r,
        }
    }

    /// Advance animations by `dt` seconds (also brings the geometry up to date). True while frames are needed.
    pub fn step(&mut self, dt: f32, src: &dyn RowSource, view: Rect) -> bool {
        self.update(src, view);
        let mut busy = self.state.step(dt);
        // the offset moved: rows that came into reach need measuring before they are drawn
        self.measure_visible(src);
        self.state.set_metrics(self.total, self.view_h);
        busy |= self.bar.step(dt);
        for e in &mut self.enters {
            e.age += dt;
        }
        let end = ease::dur::SETTLE;
        self.enters.retain(|e| e.age - e.delay < end);
        busy |= !self.enters.is_empty();
        busy
    }

    /// Entrance look of row `i`: (opacity, y offset).
    fn enter_look(&self, i: usize) -> (f32, f32) {
        match self.enters.iter().find(|e| e.index == i) {
            None => (1.0, 0.0),
            Some(e) => {
                let t = e.age - e.delay;
                if t <= 0.0 {
                    (0.0, ENTER_RISE)
                } else {
                    (curves::EASE_OUT.at(t / dur::MED), (1.0 - curves::SPRING_SETTLE.at(t / dur::SETTLE)) * ENTER_RISE)
                }
            }
        }
    }

    pub fn draw(&self, cv: &Canvas, src: &dyn RowSource, view: Rect, t: &Theme) {
        cv.with_clip(view, |cv| {
            let o = self.state.offset();
            for i in self.visible_range() {
                if i >= src.len() {
                    break;
                }
                let r = Rect::new(view.x, view.y + self.row_top(i) - o, view.w, self.row_height(i));
                let st = RowState { index: i, hover: self.hover == Some(i), selected: self.selected == Some(i) };
                let (a, dy) = self.enter_look(i);
                cv.with_opacity(a, |cv| cv.with_offset(0.0, dy, |cv| src.draw_row(cv, i, r, st)));
            }
        });
        self.bar.draw(cv, view, &self.state, t);
    }
}

impl Scrollbar {
    /// Pointer activity inside the list keeps an already visible bar from fading out under the cursor.
    fn show_if_hot(&mut self) {
        if self.alpha.target() > 0.5 {
            self.idle = 0.0;
        }
    }
}

// ================================================================ ScrollView

/// A clipped region over one tall piece of content; the closure of [`ScrollView::draw`] paints it in content coordinates.
pub struct ScrollView {
    pub state: ScrollState,
    pub bar: Scrollbar,
    content_h: f32,
    /// arrow / page keys scroll it
    pub keys: bool,
}

impl Default for ScrollView {
    fn default() -> Self {
        ScrollView::new()
    }
}

impl ScrollView {
    pub fn new() -> ScrollView {
        ScrollView { state: ScrollState::new(), bar: Scrollbar::new(Axis::Vertical), content_h: 0.0, keys: true }
    }

    /// Height of the content (call whenever it or the view height changes).
    pub fn set_content_height(&mut self, content_h: f32, view_h: f32) {
        self.content_h = content_h;
        self.state.set_metrics(content_h, view_h);
    }

    pub fn offset(&self) -> f32 {
        self.state.offset()
    }

    /// `ev` in the coordinates of the content (pointer events moved by the view origin and the scroll offset), for the controls inside.
    pub fn to_content(&self, ev: &InputEvent, view: Rect) -> InputEvent {
        ev.translated(-view.x, self.state.offset() - view.y)
    }

    /// Wheel, keys, scrollbar. A wheel that cannot move the content (already at that end) is NOT consumed, so a parent can take it.
    pub fn event(&mut self, ev: &InputEvent, view: Rect) -> Response {
        self.state.set_metrics(self.content_h, view.h);
        let r = self.bar.event(ev, view, &mut self.state);
        if r.consumed || r.capture {
            return r;
        }
        match ev {
            InputEvent::Wheel { pos, delta, precise, .. } if view.contains(pos.0, pos.1) => {
                let moved = self.state.wheel(delta.1, *precise);
                if moved {
                    self.bar.show();
                }
                Response { consumed: moved, redraw: moved, ..r }
            }
            InputEvent::Key { code, mods, down: true, .. } if self.keys => {
                let u = self.state.key(*code, *mods, LINE_PX);
                if u {
                    self.bar.show();
                }
                Response { consumed: u, redraw: u, ..r }
            }
            _ => r,
        }
    }

    pub fn step(&mut self, dt: f32) -> bool {
        let a = self.state.step(dt);
        let b = self.bar.step(dt);
        a | b
    }

    /// Paint: `content(cv, rect)` draws the content in its own coordinates (`rect` = 0,0,view width,content height).
    pub fn draw(&self, cv: &Canvas, view: Rect, t: &Theme, content: impl FnOnce(&Canvas, Rect)) {
        cv.with_clip(view, |cv| {
            cv.with_offset(view.x, view.y - self.state.offset(), |cv| content(cv, Rect::new(0.0, 0.0, view.w, self.content_h)));
        });
        self.bar.draw(cv, view, &self.state, t);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::color::Color;
    use crate::input::Buttons;
    use crate::testkit::Harness;
    use std::cell::Cell;

    /// The tests assume the system has motion enabled (like the ease tests); on a machine with "reduce motion" they would be vacuous.
    fn calm() -> bool {
        !ease::reduced()
    }

    /// Where the review images go: the shared cargo target dir (never committed).
    fn img_dir() -> std::path::PathBuf {
        let base = std::env::var_os("CARGO_TARGET_DIR").map(std::path::PathBuf::from).unwrap_or_else(|| std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../target"));
        base.join("kit-images")
    }

    fn run(st: &mut ScrollState, secs: f32, fps: f32) {
        let n = (secs * fps).round() as i32;
        for _ in 0..n {
            st.step(1.0 / fps);
        }
    }

    fn state(content: f32, vp: f32) -> ScrollState {
        let mut s = ScrollState::new();
        s.set_metrics(content, vp);
        s
    }

    // ---------------------------------------------------------- physics

    #[test]
    fn a_notch_glides_and_lands_exactly_without_overshoot() {
        if !calm() {
            return;
        }
        let mut s = state(5000.0, 400.0);
        assert!(s.wheel(100.0, false));
        assert_eq!(s.offset(), 0.0, "the glide starts from where it is");
        let mut max = 0.0f32;
        let mut frames = 0;
        while s.step(1.0 / 60.0) {
            max = max.max(s.offset());
            frames += 1;
            assert!(frames < 120, "never settles");
        }
        assert!(max <= 100.0 + 1e-3, "critically damped: no overshoot ({max})");
        assert_eq!(s.offset(), 100.0);
        assert!(frames > 6 && frames < 40, "{frames} frames");
        // notches accumulate on the target, not on the shown offset
        s.wheel(100.0, false);
        s.step(0.016);
        s.wheel(100.0, false);
        assert_eq!(s.target(), 300.0);
    }

    #[test]
    fn offsets_are_clamped_and_metrics_shrinking_pulls_the_offset_in() {
        let mut s = state(1000.0, 400.0);
        s.scroll_to(-50.0, false);
        assert_eq!(s.offset(), 0.0);
        assert!(s.at_start());
        s.scroll_to(9999.0, false);
        assert_eq!(s.offset(), 600.0);
        assert!(s.at_end());
        assert!(!s.wheel(100.0, false), "already at the end: nothing moved, so a parent may take the event");
        s.set_metrics(700.0, 400.0);
        assert_eq!(s.offset(), 300.0);
        s.set_metrics(100.0, 400.0);
        assert_eq!(s.offset(), 0.0);
        assert!(!s.scrollable());
        assert!(!s.key(KeyCode::Down, Modifiers::NONE, LINE_PX), "nothing to scroll: the key is not ours");
    }

    #[test]
    fn touchpad_follows_the_fingers_then_flings_and_decays() {
        if !calm() {
            return;
        }
        let mut s = state(100000.0, 400.0);
        for _ in 0..6 {
            assert!(s.wheel(20.0, true));
            s.step(1.0 / 120.0); // 20 px / 8.3 ms = 2400 px/s
        }
        assert!((s.offset() - 120.0).abs() < 1e-3, "exact, no lag");
        let held = s.offset();
        assert!(s.step(0.02), "waiting for the fling to start counts as busy");
        run(&mut s, 0.1, 60.0);
        assert!(s.offset() > held + 50.0, "the fling carries on ({} -> {})", held, s.offset());
        let mut last = s.offset();
        let mut speeds = Vec::new();
        for _ in 0..200 {
            if !s.step(1.0 / 60.0) {
                break;
            }
            speeds.push(s.offset() - last);
            last = s.offset();
        }
        assert!(speeds.windows(2).all(|w| w[1] <= w[0] + 1e-3), "monotonically slowing");
        assert!(!s.step(1.0 / 60.0), "it comes to rest");
        // a new touch stops the fling
        let mut s = state(100000.0, 400.0);
        for _ in 0..4 {
            s.wheel(30.0, true);
            s.step(1.0 / 120.0);
        }
        run(&mut s, 0.1, 60.0);
        let x = s.offset();
        s.scroll_to(x, false);
        run(&mut s, 0.3, 60.0);
        assert_eq!(s.offset(), x);
    }

    #[test]
    fn a_fling_stops_at_the_end() {
        if !calm() {
            return;
        }
        let mut s = state(1000.0, 400.0);
        s.scroll_to(550.0, false);
        for _ in 0..4 {
            s.wheel(40.0, true);
            s.step(1.0 / 120.0);
        }
        run(&mut s, 1.0, 60.0);
        assert_eq!(s.offset(), 600.0);
    }

    #[test]
    fn reduced_motion_jumps() {
        let mut s = state(5000.0, 400.0);
        s.reduced = Some(true);
        s.wheel(300.0, false);
        assert_eq!(s.offset(), 300.0);
        assert!(!s.step(0.016));
        s.scroll_to(700.0, true);
        assert_eq!(s.offset(), 700.0);
        s.wheel(10.0, true);
        assert!(!s.step(0.2), "no fling either");
        assert_eq!(s.offset(), 710.0);
    }

    #[test]
    fn frame_rate_does_not_change_where_a_glide_is_after_a_given_time() {
        if !calm() {
            return;
        }
        let at = |fps: f32| {
            let mut s = state(5000.0, 400.0);
            s.wheel(100.0, false);
            run(&mut s, 0.1, fps);
            s.offset()
        };
        let (a, b, c) = (at(30.0), at(60.0), at(240.0));
        assert!((a - b).abs() < 1.0 && (b - c).abs() < 1.0, "{a} {b} {c}");
    }

    #[test]
    fn keyboard_commands() {
        let mut s = state(5000.0, 400.0);
        assert!(s.key(KeyCode::Down, Modifiers::NONE, LINE_PX));
        assert_eq!(s.target(), 40.0);
        assert!(s.key(KeyCode::PageDown, Modifiers::NONE, LINE_PX));
        assert_eq!(s.target(), 40.0 + 350.0);
        assert!(s.key(KeyCode::Space, Modifiers::SHIFT, LINE_PX));
        assert_eq!(s.target(), 40.0);
        assert!(s.key(KeyCode::End, Modifiers::NONE, LINE_PX));
        assert!(s.at_end());
        assert!(s.key(KeyCode::Home, Modifiers::NONE, LINE_PX));
        assert!(s.at_start());
        assert!(!s.key(KeyCode::Char('A'), Modifiers::NONE, LINE_PX));
    }

    // ---------------------------------------------------------- scrollbar

    #[test]
    fn thumb_maths_round_trip_and_minimum() {
        // half of the content visible: half the track
        let (p, l) = thumb_span(400.0, 800.0, 400.0, 0.0, 28.0).unwrap();
        assert_eq!((p, l), (0.0, 200.0));
        let (p, _) = thumb_span(400.0, 800.0, 400.0, 400.0, 28.0).unwrap();
        assert_eq!(p, 200.0, "at the end the thumb touches the track end");
        // huge content: the minimum length holds
        let (_, l) = thumb_span(400.0, 1.0e6, 400.0, 0.0, 28.0).unwrap();
        assert_eq!(l, 28.0);
        assert_eq!(thumb_span(400.0, 400.0, 400.0, 0.0, 28.0), None, "fits: no thumb");
        for off in [0.0, 1.0, 123.0, 4999.0, 10_000.0] {
            let (p, _) = thumb_span(300.0, 10_400.0, 400.0, off, 28.0).unwrap();
            let back = offset_for_thumb(300.0, 10_400.0, 400.0, p, 28.0);
            assert!((back - off).abs() < 0.5, "{off} -> {back}");
        }
        assert_eq!(offset_for_thumb(400.0, 100.0, 400.0, 50.0, 28.0), 0.0);
        assert_eq!(offset_for_thumb(400.0, 800.0, 400.0, 9999.0, 28.0), 400.0, "clamped");
    }

    fn mv(x: f32, y: f32, left: bool) -> InputEvent {
        InputEvent::MouseMove { pos: (x, y), mods: Modifiers::NONE, buttons: if left { Buttons::NONE.with(MouseButton::Left) } else { Buttons::NONE } }
    }
    fn down(x: f32, y: f32) -> InputEvent {
        InputEvent::MouseDown { pos: (x, y), button: MouseButton::Left, mods: Modifiers::NONE, clicks: 1 }
    }
    fn up(x: f32, y: f32) -> InputEvent {
        InputEvent::MouseUp { pos: (x, y), button: MouseButton::Left, mods: Modifiers::NONE }
    }

    #[test]
    fn dragging_the_thumb_maps_pointer_to_offset_and_captures() {
        let view = Rect::new(10.0, 20.0, 200.0, 404.0);
        let mut st = state(1600.0, 404.0);
        let mut bar = Scrollbar::new(Axis::Vertical);
        bar.reduced = Some(true);
        st.reduced = Some(true);
        let th = bar.thumb(view, &st).unwrap();
        assert!(th.x >= view.right() - 12.0 && th.w == BAR_THIN);
        let grab_y = th.y + 10.0;
        let r = bar.event(&down(th.cx(), grab_y), view, &mut st);
        assert!(r.consumed && r.capture && bar.dragging());
        // move the pointer to the middle of the track (even far to the left of it: capture)
        let r = bar.event(&mv(-300.0, view.y + BAR_EDGE + 100.0 + 10.0, true), view, &mut st);
        assert!(r.capture && r.changed);
        let (_, len) = thumb_span(400.0, 1600.0, 404.0, 0.0, 28.0).unwrap();
        let want = offset_for_thumb(400.0, 1600.0, 404.0, 100.0, 28.0);
        assert!((st.offset() - want).abs() < 0.01, "{} vs {want} (len {len})", st.offset());
        // dragging past the ends clamps
        bar.event(&mv(0.0, 5000.0, true), view, &mut st);
        assert_eq!(st.offset(), st.max_offset());
        bar.event(&mv(0.0, -5000.0, true), view, &mut st);
        assert_eq!(st.offset(), 0.0);
        let r = bar.event(&up(0.0, 0.0), view, &mut st);
        assert!(r.consumed && !bar.dragging());
    }

    #[test]
    fn clicking_the_track_pages_towards_the_click() {
        let view = Rect::new(0.0, 0.0, 200.0, 400.0);
        let mut st = state(4000.0, 400.0);
        st.reduced = Some(true);
        let mut bar = Scrollbar::new(Axis::Vertical);
        bar.reduced = Some(true);
        let r = bar.event(&down(195.0, 390.0), view, &mut st);
        assert!(r.consumed && r.changed && !r.capture);
        assert_eq!(st.offset(), 350.0);
        bar.event(&down(195.0, 3.0), view, &mut st);
        assert_eq!(st.offset(), 0.0);
        // a press outside the zone is not ours
        assert!(!bar.event(&down(100.0, 200.0), view, &mut st).consumed);
    }

    #[test]
    fn the_bar_fades_in_waits_and_fades_out() {
        if !calm() {
            return;
        }
        let mut bar = Scrollbar::new(Axis::Vertical);
        assert_eq!(bar.alpha(), 0.0);
        bar.show();
        let mut t = 0.0;
        while t < 0.12 {
            bar.step(1.0 / 100.0);
            t += 0.01;
        }
        assert!((bar.alpha() - 1.0).abs() < 1e-3, "fully shown after 120 ms ({})", bar.alpha());
        // still there just before the idle time ran out
        for _ in 0..70 {
            assert!(bar.step(0.01), "waiting counts as busy so the host keeps ticking");
        }
        assert!(bar.alpha() > 0.99);
        // idle over (0.9 s): it fades over 300 ms
        for _ in 0..25 {
            bar.step(0.01);
        }
        let mid = bar.alpha();
        assert!(mid < 1.0, "fading ({mid})");
        for _ in 0..40 {
            bar.step(0.01);
        }
        assert_eq!(bar.alpha(), 0.0);
        assert!(!bar.step(0.01), "hidden bar costs no frames");
        // activity restarts the timer
        bar.show();
        for _ in 0..80 {
            bar.step(0.01);
        }
        bar.show();
        for _ in 0..50 {
            bar.step(0.01);
        }
        assert!(bar.alpha() > 0.99);
    }

    #[test]
    fn hovering_the_zone_shows_widens_and_holds_the_bar() {
        if !calm() {
            return;
        }
        let view = Rect::new(0.0, 0.0, 200.0, 400.0);
        let mut st = state(4000.0, 400.0);
        let mut bar = Scrollbar::new(Axis::Vertical);
        let r = bar.event(&mv(195.0, 100.0, false), view, &mut st);
        assert!(r.redraw && bar.hovered());
        for _ in 0..30 {
            bar.step(0.01);
        }
        assert!((bar.thumb(view, &st).unwrap().w - BAR_WIDE).abs() < 1e-3);
        for _ in 0..300 {
            bar.step(0.01);
        }
        assert!(bar.alpha() > 0.99, "no fade-out while the pointer is on it");
        bar.event(&InputEvent::MouseLeave, view, &mut st);
        for _ in 0..300 {
            bar.step(0.01);
        }
        assert_eq!(bar.alpha(), 0.0);
        // a content that fits has no hover
        let mut fit = state(100.0, 400.0);
        let mut b2 = Scrollbar::new(Axis::Vertical);
        assert!(!b2.event(&mv(195.0, 100.0, false), view, &mut fit).redraw);
    }

    #[test]
    fn horizontal_bar_geometry() {
        let view = Rect::new(0.0, 0.0, 300.0, 100.0);
        let st = state(900.0, 300.0);
        let bar = Scrollbar::new(Axis::Horizontal);
        let th = bar.thumb(view, &st).unwrap();
        assert!(th.bottom() <= view.bottom() - BAR_EDGE + 1e-3 && th.w > th.h);
        assert_eq!(bar.zone(view).h, 14.0);
    }

    // ---------------------------------------------------------- rows

    /// 10 000 rows of heights 20..90; counts how often `height` is asked.
    struct Rows {
        n: usize,
        calls: Cell<usize>,
        /// scale: heights depend on the width like wrapped text
        wrap: bool,
    }
    impl Rows {
        fn new(n: usize) -> Rows {
            Rows { n, calls: Cell::new(0), wrap: false }
        }
        fn base(i: usize) -> f32 {
            20.0 + ((i * 37) % 8) as f32 * 10.0
        }
    }
    impl RowSource for Rows {
        fn len(&self) -> usize {
            self.n
        }
        fn height(&self, i: usize, width: f32) -> f32 {
            self.calls.set(self.calls.get() + 1);
            if self.wrap {
                Rows::base(i) * (1.0 + (600.0 / width).floor())
            } else {
                Rows::base(i)
            }
        }
        fn draw_row(&self, cv: &Canvas, i: usize, r: Rect, st: RowState) {
            let c = if st.selected { Color::rgb8(90, 90, 200) } else if st.hover { Color::rgb8(70, 70, 70) } else if i % 2 == 0 { Color::rgb8(40, 40, 40) } else { Color::rgb8(30, 30, 30) };
            cv.fill_rect(r.inset_xy(0.0, 1.0), c);
        }
    }

    fn view() -> Rect {
        Rect::new(0.0, 0.0, 300.0, 400.0)
    }

    fn exact_top(i: usize) -> f32 {
        (0..i).map(Rows::base).sum()
    }

    fn list_for(src: &dyn RowSource) -> ScrollList {
        let mut l = ScrollList::new(55.0);
        l.reduced = Some(true);
        l.state.reduced = Some(true);
        l.bar.reduced = Some(true);
        l.update(src, view());
        l
    }

    #[test]
    fn fenwick_prefix_and_lookup() {
        let v: Vec<f32> = (0..1000).map(|i| 1.0 + (i % 5) as f32).collect();
        let f = Fenwick::build(&v);
        let mut acc = 0.0f64;
        for i in 0..v.len() {
            assert!((f.prefix(i) - acc).abs() < 1e-9);
            // the middle of row i and its first pixel both map to i
            assert_eq!(f.lower(acc + 0.01), i);
            assert_eq!(f.lower(acc), i);
            acc += v[i] as f64;
        }
        assert_eq!(f.lower(acc + 10.0), v.len());
        let mut g = f.clone();
        g.add(3, 10.0);
        assert!((g.prefix(4) - f.prefix(4) - 10.0).abs() < 1e-9);
        assert_eq!(Fenwick::default().lower(5.0), 0);
    }

    #[test]
    fn ten_thousand_rows_are_virtualised() {
        let src = Rows::new(10_000);
        let mut l = list_for(&src);
        assert!(src.calls.get() < 60, "only rows near the viewport were measured ({})", src.calls.get());
        assert_eq!(l.len(), 10_000);
        let r = l.visible_range();
        assert_eq!(r.start, 0);
        assert!(r.end >= 6 && r.end <= 20, "{r:?}");
        // a jump deep into the list
        l.state.scroll_to(250_000.0, false);
        l.update(&src, view());
        let r = l.visible_range();
        // rows around that offset have been measured exactly; the rows above are estimates (measuring them shifted the offset a little,
        // so the row at the top stays where it was), so check consistency with the list's own geometry
        let o = l.state.offset();
        assert!((o - 250_000.0).abs() < 10_000.0, "{o}");
        assert!(l.row_top(r.start) <= o + 1e-3 && l.row_top(r.start) + l.row_height(r.start) > o - 1e-3);
        assert!(l.row_top(r.end - 1) < o + 400.0 && (l.row_top(r.end) >= o + 400.0 - 1e-3 || r.end == 10_000));
        assert!(src.calls.get() < 120, "{}", src.calls.get());
        // content height = measured exact + estimated rest
        assert!(l.content_height() > 100_000.0);
        // at the very end
        l.state.scroll_to(f32::MAX, false);
        l.update(&src, view());
        assert_eq!(l.visible_range().end, 10_000);
        assert!(l.state.at_end());
    }

    #[test]
    fn visible_range_is_exact_once_measured() {
        let src = Rows::new(300);
        let mut l = list_for(&src);
        l.measure_all(&src);
        for off in [0.0f32, 1.0, 19.9, 20.0, 777.0, 5000.0] {
            l.state.scroll_to(off, false);
            let r = l.visible_range();
            let want_first = (0..300).find(|&i| exact_top(i) + Rows::base(i) > off).unwrap();
            let want_last = (0..300).rev().find(|&i| exact_top(i) < off + 400.0).unwrap();
            assert_eq!((r.start, r.end - 1), (want_first, want_last), "offset {off}");
        }
        // empty and tiny sources
        let none = Rows::new(0);
        let l0 = list_for(&none);
        assert_eq!(l0.visible_range(), 0..0);
        assert_eq!(l0.row_at(view(), (5.0, 5.0)), None);
    }

    #[test]
    fn scroll_to_index_alignments() {
        let src = Rows::new(500);
        let mut l = list_for(&src);
        l.measure_all(&src);
        let i = 200;
        let (top, h) = (exact_top(i), Rows::base(i));
        l.scroll_to_index(&src, i, false, Align::Start);
        assert!((l.state.offset() - top).abs() < 1e-2);
        l.scroll_to_index(&src, i, false, Align::End);
        assert!((l.state.offset() - (top + h - 400.0)).abs() < 1e-2);
        l.scroll_to_index(&src, i, false, Align::Center);
        assert!((l.state.offset() - (top + h / 2.0 - 200.0)).abs() < 1e-2);
        // nearest: already visible = no move; below = row at the bottom; above = row at the top
        let before = l.state.offset();
        l.scroll_to_index(&src, i, false, Align::Nearest);
        assert_eq!(l.state.offset(), before);
        l.scroll_to_index(&src, i + 30, false, Align::Nearest);
        let (t2, h2) = (exact_top(i + 30), Rows::base(i + 30));
        assert!((l.state.offset() - (t2 + h2 - 400.0)).abs() < 1e-2);
        l.scroll_to_index(&src, 3, false, Align::Nearest);
        assert!((l.state.offset() - exact_top(3)).abs() < 1e-2);
        // out of range is ignored; the first row aligns to 0
        l.scroll_to_index(&src, 9999, false, Align::Start);
        l.scroll_to_index(&src, 0, false, Align::End);
        assert_eq!(l.state.offset(), 0.0);
    }

    #[test]
    fn a_width_change_keeps_the_first_visible_row() {
        let mut src = Rows::new(2000);
        src.wrap = true;
        let mut l = list_for(&src);
        l.state.scroll_to(1500.0, false);
        l.update(&src, view());
        l.state.scroll_to(1500.0, false);
        l.update(&src, view());
        let first = l.visible_range().start;
        let within = l.state.offset() - l.row_top(first);
        assert!(first > 5);
        // narrower: rows get taller
        let narrow = Rect::new(0.0, 0.0, 150.0, 400.0);
        l.update(&src, narrow);
        assert_eq!(l.visible_range().start, first, "the same row is at the top");
        let d = l.state.offset() - l.row_top(first);
        assert!(d >= -1e-3 && d <= l.row_height(first) && d <= within + 1.0, "{d} {within}");
        assert!(l.row_height(first) > 0.0);
        // measured heights were refreshed for the new width
        let r = l.visible_range();
        assert!((r.start..r.end).all(|i| (l.row_height(i) - Rows::base(i) * (1.0 + (600.0f32 / 150.0).floor())).abs() < 1e-3));
    }

    #[test]
    fn stick_to_end_follows_appends_until_the_user_scrolls_away() {
        let mut src = Rows::new(40);
        let mut l = list_for(&src);
        l.stick_to_end = true;
        l.update(&src, view());
        // (first update happened before the flag; scroll to the end ourselves, like opening a chat)
        l.state.scroll_to(f32::MAX, false);
        l.pinned = true;
        let end0 = l.state.offset();
        src.n = 41;
        l.update(&src, view());
        assert!(l.state.at_end() && l.state.offset() > end0, "followed the append");
        // the user scrolls up: appends no longer move the view
        let ev = InputEvent::Wheel { pos: (50.0, 50.0), delta: (0.0, -300.0), mods: Modifiers::NONE, precise: false };
        let r = l.event(&ev, &src, view());
        assert!(r.consumed);
        assert!(!l.pinned());
        let a = l.visible_range().start;
        let rel = l.row_top(a) - l.state.offset();
        src.n = 45;
        l.update(&src, view());
        assert_eq!(l.visible_range().start, a);
        assert!((l.row_top(a) - l.state.offset() - rel).abs() < 1e-3, "the row the user looks at has not moved on screen");
        // scrolling back to the end re-pins
        l.state.reduced = Some(true);
        let ev = InputEvent::Key { code: KeyCode::End, mods: Modifiers::NONE, down: true, repeat: false };
        l.keyboard_select = false;
        l.event(&ev, &src, view());
        assert!(l.pinned());
        src.n = 46;
        l.update(&src, view());
        assert!(l.state.at_end());
        // the last row growing (streaming) is followed too
        struct Grow(Cell<f32>);
        impl RowSource for Grow {
            fn len(&self) -> usize {
                10
            }
            fn height(&self, i: usize, _w: f32) -> f32 {
                if i == 9 { self.0.get() } else { 50.0 }
            }
            fn draw_row(&self, _: &Canvas, _: usize, _: Rect, _: RowState) {}
        }
        let g = Grow(Cell::new(50.0));
        let mut l = list_for(&g);
        l.stick_to_end = true;
        l.update(&g, view());
        l.state.scroll_to(f32::MAX, false);
        g.0.set(200.0);
        l.invalidate_row(9);
        l.update(&g, view());
        assert!(l.state.at_end());
        assert_eq!(l.state.offset(), 50.0 * 9.0 + 200.0 - 400.0);
    }

    #[test]
    fn hover_click_selection_and_keyboard() {
        let src = Rows::new(100);
        let mut l = list_for(&src);
        l.measure_all(&src);
        let v = view();
        // hover
        let r = l.event(&mv(10.0, 25.0, false), &src, v);
        assert!(r.redraw);
        assert_eq!(l.hovered(), Some(1), "y = 25 is below row 0 (20 px)");
        assert_eq!(l.row_at(v, (10.0, 25.0)), l.hovered());
        l.event(&InputEvent::MouseLeave, &src, v);
        assert_eq!(l.hovered(), None);
        // click selects and reports
        let y = Rows::base(0) + 1.0;
        let r = l.event(&down(10.0, y), &src, v);
        assert!(r.consumed && r.focus);
        assert_eq!(l.selected(), Some(1));
        assert_eq!(l.take_events(), vec![ListEvent::SelectionChanged(1), ListEvent::Click(1)]);
        // keys move the selection and keep it in view
        let key = |c| InputEvent::Key { code: c, mods: Modifiers::NONE, down: true, repeat: false };
        for _ in 0..30 {
            l.event(&key(KeyCode::Down), &src, v);
        }
        assert_eq!(l.selected(), Some(31));
        let r = l.visible_range();
        assert!(r.contains(&31));
        let (t, h) = (l.row_top(31), l.row_height(31));
        assert!(t >= l.state.target() - 1e-3 && t + h <= l.state.target() + 400.0 + 1e-3, "fully visible");
        l.event(&key(KeyCode::End), &src, v);
        assert_eq!(l.selected(), Some(99));
        l.event(&key(KeyCode::Home), &src, v);
        assert_eq!((l.selected(), l.state.target()), (Some(0), 0.0));
        l.event(&key(KeyCode::PageDown), &src, v);
        assert!(l.selected().unwrap() >= 5);
        l.take_events();
        l.event(&key(KeyCode::Enter), &src, v);
        assert_eq!(l.take_events(), vec![ListEvent::Activate(l.selected().unwrap())]);
        // with keyboard_select off the keys scroll the viewport instead
        l.keyboard_select = false;
        l.state.scroll_to(0.0, false);
        let sel = l.selected();
        l.event(&key(KeyCode::Down), &src, v);
        assert_eq!(l.state.target(), LINE_PX);
        assert_eq!(l.selected(), sel);
    }

    #[test]
    fn rows_enter_with_a_stagger_and_not_under_reduced_motion() {
        if !calm() {
            return;
        }
        let mut src = Rows::new(5);
        let mut l = ScrollList::new(55.0);
        l.enter = EnterMode::Added;
        l.update(&src, view());
        assert_eq!(l.enter_look(0), (1.0, 0.0), "the first show does not animate in Added mode");
        src.n = 8;
        l.update(&src, view());
        assert_eq!(l.enter_look(5), (0.0, ENTER_RISE));
        assert_eq!(l.enter_look(7), (0.0, ENTER_RISE));
        l.step(dur::STAGGER + 0.001, &src, view());
        let (a5, y5) = l.enter_look(5);
        let (a6, _) = l.enter_look(6);
        assert!(a5 > 0.0 && a6 == 0.0 || a6 < a5, "row 5 started, row 6 waits its turn ({a5} {a6})");
        assert!(y5 < ENTER_RISE);
        let mut n = 0;
        while l.step(0.016, &src, view()) {
            n += 1;
            assert!(n < 200);
        }
        assert_eq!(l.enter_look(5), (1.0, 0.0));
        // FirstShow animates what is visible at the start
        let mut l = ScrollList::new(55.0);
        l.enter = EnterMode::FirstShow;
        l.update(&Rows::new(30), view());
        assert_eq!(l.enter_look(0).0, 0.0);
        assert_eq!(l.enter_look(20).0, 1.0, "off-screen rows are not animated");
        // reduced motion: nothing animates
        let mut l = ScrollList::new(55.0);
        l.enter = EnterMode::FirstShow;
        l.reduced = Some(true);
        l.update(&Rows::new(30), view());
        assert_eq!(l.enter_look(0), (1.0, 0.0));
    }

    #[test]
    fn a_shrinking_source_drops_stale_state() {
        let mut src = Rows::new(50);
        let mut l = list_for(&src);
        l.set_selected(Some(40));
        l.state.scroll_to(1500.0, false);
        src.n = 10;
        l.update(&src, view());
        assert_eq!(l.selected(), None);
        assert_eq!(l.len(), 10);
        assert!(l.state.offset() <= l.state.max_offset());
        src.n = 0;
        l.update(&src, view());
        assert_eq!(l.visible_range(), 0..0);
        assert_eq!(l.content_height(), 0.0);
    }

    #[test]
    fn nested_wheel_passes_to_the_parent_at_the_end() {
        let mut v = ScrollView::new();
        v.state.reduced = Some(true);
        v.bar.reduced = Some(true);
        v.set_content_height(1000.0, 400.0);
        let area = Rect::new(0.0, 0.0, 300.0, 400.0);
        let wheel = |dy| InputEvent::Wheel { pos: (10.0, 10.0), delta: (0.0, dy), mods: Modifiers::NONE, precise: false };
        assert!(!v.event(&wheel(-100.0), area).consumed, "at the top, scrolling up: not ours");
        assert!(v.event(&wheel(100.0), area).consumed);
        assert_eq!(v.offset(), 100.0);
        v.event(&wheel(10_000.0), area);
        assert_eq!(v.offset(), 600.0);
        assert!(!v.event(&wheel(100.0), area).consumed);
        // pointer events are re-expressed in content coordinates
        let e = v.to_content(&mv(50.0, 30.0, false), Rect::new(10.0, 20.0, 300.0, 400.0));
        assert_eq!(e.pos(), Some((40.0, 610.0)));
    }

    // ---------------------------------------------------------- pixels

    #[test]
    fn list_and_bar_render() {
        if !calm() {
            return;
        }
        let h = Harness::new();
        let theme = Theme::pill_dark();
        let src = Rows::new(200);
        let mut l = ScrollList::new(55.0);
        let v = Rect::new(10.0, 10.0, 220.0, 200.0);
        l.update(&src, v);
        l.state.scroll_to(300.0, false);
        l.update(&src, v);
        l.set_selected(Some(l.visible_range().start + 1));
        l.bar.show();
        for _ in 0..20 {
            l.step(0.01, &src, v);
        }
        let mut images = Vec::new();
        for scale in [1.0, 2.0] {
            let img = h.render(240, 220, scale, theme.bg, |cv| l.draw(cv, &src, v, &theme));
            images.push((scale, img));
        }
        for (scale, img) in &images {
            // selected row colour is present; nothing leaks outside the clip
            assert!(img.count(None, |p| p[2] > 180 && p[0] < 110) > 100, "selected row");
            assert_eq!(img.at(5.0, 100.0), [0, 0, 0, 255], "outside the clip");
            assert_eq!(img.at(120.0, 215.0), [0, 0, 0, 255], "below the clip");
            // the bar: light pixels in the right-hand 12 px of the view
            let zone = Rect::new(v.right() - 12.0, v.y, 12.0, v.h);
            assert!(img.count(Some(zone), |p| p[0] > 50 && p[0] == p[1]) > 20, "thumb at scale {scale}");
            img.save_png(&h, img_dir().join(format!("scroll_list_{}x.png", *scale as u32))).ok();
        }
        // a hidden bar draws nothing
        let mut quiet = ScrollList::new(55.0);
        quiet.update(&src, v);
        let img = h.render(240, 220, 1.0, theme.bg, |cv| quiet.draw(cv, &src, v, &theme));
        let zone = Rect::new(v.right() - 12.0, v.y, 12.0, v.h);
        assert_eq!(img.count(Some(zone), |p| p[0] > 140 && p[0] == p[1] && p[1] == p[2]), 0);
    }
}
