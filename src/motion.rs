//! Pure (no GPU, no clock) state machines behind the Liquid Glass motion: the inertial light, the materialise timeline,
//! the backdrop-driven light / dark appearance with hysteresis, the pointer glint / hover / press response and the neighbour-merge distance logic.
//! Everything takes explicit `dt` / `t` so it is unit-testable and the headless renderer can show any phase.

// ---------------------------------------------------------------- light

/// The virtual light of ONE card: a base direction plus an offset vector that is pushed against the card velocity and relaxes back
/// with a spring (about 400 ms), so a dragged card's highlight swings the other way and settles.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct LightSpring {
    pub o: (f32, f32),
    v: (f32, f32),
}

impl LightSpring {
    const K: f32 = 110.0; // omega ~ 10.5 rad/s
    const C: f32 = 17.0; // zeta ~ 0.8: one soft overshoot, ~400 ms to rest

    /// A spring at rest on the offset `o` (headless: a fixed light direction).
    pub fn with_offset(o: (f32, f32)) -> LightSpring {
        LightSpring { o, v: (0.0, 0.0) }
    }

    /// Drive target for a card moving with velocity (vx, vy) px/s: the light lags opposite to the motion (saturates at ~1800 px/s).
    pub fn drive_for(vx: f32, vy: f32) -> (f32, f32) {
        let (nx, ny) = (vx / 1800.0, vy / 1800.0);
        let l = nx.hypot(ny);
        let k = if l > 1.0 { 1.0 / l } else { 1.0 };
        (-nx * k * 0.9, -ny * k * 0.9)
    }

    /// One semi-implicit Euler step towards `drive`; true when at rest on it (nothing more to animate).
    pub fn step(&mut self, dt: f32, drive: (f32, f32)) -> bool {
        let h = dt / 2.0;
        for _ in 0..2 {
            self.v.0 += (Self::K * (drive.0 - self.o.0) - Self::C * self.v.0) * h;
            self.v.1 += (Self::K * (drive.1 - self.o.1) - Self::C * self.v.1) * h;
            self.o.0 += self.v.0 * h;
            self.o.1 += self.v.1 * h;
        }
        let done = (drive.0 - self.o.0).abs() < 0.004 && (drive.1 - self.o.1).abs() < 0.004 && self.v.0.abs() < 0.03 && self.v.1.abs() < 0.03;
        if done {
            self.o = drive;
            self.v = (0.0, 0.0);
        }
        done
    }

    /// At rest in the neutral place (no offset, no motion).
    pub fn is_rest(&self) -> bool {
        self.o == (0.0, 0.0) && self.v == (0.0, 0.0)
    }

    /// Effective light direction, CSS degrees (the light comes FROM there): the base unit vector plus the offset.
    pub fn angle(&self, base_deg: f32) -> f32 {
        let a = base_deg.to_radians();
        let (x, y) = (a.sin() + self.o.0, -a.cos() + self.o.1);
        x.atan2(-y).to_degrees().rem_euclid(360.0)
    }
}

// ---------------------------------------------------------------- pointer glint

/// Glint strength for a pointer at signed distance `d` px from the card contour (< 0 inside): full on the edge, gone 62 logical px away.
pub fn glint_target(d_px: f32, scale: f32) -> f32 {
    let t = 1.0 - (d_px.abs() / (62.0 * scale)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Exponential approach (fade in / out of the glint, ~120 ms time constant).
pub fn approach(v: f32, target: f32, dt: f32, tau: f32) -> f32 {
    follow(v, target, dt, tau, 0.004)
}

/// Exponential approach with time constant `tau` that snaps onto `target` once within `eps` (so an animation really ends).
pub fn follow(v: f32, target: f32, dt: f32, tau: f32, eps: f32) -> f32 {
    let n = v + (target - v) * (1.0 - (-dt / tau).exp());
    if (n - target).abs() < eps {
        target
    } else {
        n
    }
}

/// `follow` for a value that wraps at 1 (a contour fraction): it goes the short way round, so a glint whose nearest contour
/// point jumps across the card slides along the rim instead of teleporting.
pub fn follow_wrap(u: f32, target: f32, dt: f32, tau: f32, eps: f32) -> f32 {
    let d = wrap_dist(u, target);
    if d.abs() < eps {
        return target.rem_euclid(1.0);
    }
    (u + d * (1.0 - (-dt / tau).exp())).rem_euclid(1.0)
}

/// Signed shortest distance from `u` to `target` on a circle of circumference 1 (-0.5..0.5).
pub fn wrap_dist(u: f32, target: f32) -> f32 {
    (target - u + 0.5).rem_euclid(1.0) - 0.5
}

// ---------------------------------------------------------------- pointer hover

/// Hover eases in this long after the pointer enters a card, out this long after it leaves (seconds).
pub const HOVER_IN: f32 = 0.18;
pub const HOVER_OUT: f32 = 0.25;
/// The press response: in fast, out a bit slower (seconds).
pub const PRESS_IN: f32 = 0.08;
pub const PRESS_OUT: f32 = 0.2;
/// Rim glint strength anywhere inside a hovered card (1 on the contour).
pub const RIM_FLOOR: f32 = 0.5;
/// Card scale while hovered / pressed.
pub const HOVER_SCALE: f32 = 1.012;
pub const PRESS_SCALE: f32 = 0.985;

/// Linear progress 0..1 towards `on`, taking `t_in` seconds up and `t_out` down (read it through `smooth`).
pub fn ramp(v: f32, on: bool, dt: f32, t_in: f32, t_out: f32) -> f32 {
    if on {
        (v + dt / t_in).min(1.0)
    } else {
        (v - dt / t_out).max(0.0)
    }
}

/// Smoothstep easing of a 0..1 progress.
pub fn smooth(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Rim glint strength for a pointer at signed distance `d` px from the contour (< 0 inside) of a hovered card: never below
/// `RIM_FLOOR` inside, 1 on the contour; outside the 62 px approach fall-off of `glint_target`.
pub fn hover_rim(d_px: f32, scale: f32) -> f32 {
    let g = glint_target(d_px, scale);
    if d_px < 0.0 {
        RIM_FLOOR + (1.0 - RIM_FLOOR) * g
    } else {
        g
    }
}

/// Where the card scale springs to: pressed, hovered or at rest.
pub fn bump_target(hover: bool, press: bool) -> f32 {
    if press {
        PRESS_SCALE
    } else if hover {
        HOVER_SCALE
    } else {
        1.0
    }
}

/// The card scale spring of hover / press: a soft overshoot on hover and release (zeta ~0.6), critically damped into a press.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Bump {
    pub x: f32,
    v: f32,
}
impl Default for Bump {
    fn default() -> Self {
        Bump { x: 1.0, v: 0.0 }
    }
}
impl Bump {
    const K: f32 = 580.0; // omega ~ 24 rad/s

    /// At rest on scale `x` (headless renders of a hover / press state).
    pub fn resting(x: f32) -> Bump {
        Bump { x, v: 0.0 }
    }

    pub fn at_rest(&self, target: f32) -> bool {
        self.x == target && self.v == 0.0
    }

    /// One semi-implicit Euler step towards `target`; true when at rest on it.
    pub fn step(&mut self, dt: f32, target: f32) -> bool {
        // pressing: no overshoot (a press must not bounce); everything else: zeta 0.6
        let zeta2 = if target < self.x && target < 1.0 { 2.0 } else { 1.2 };
        self.step_with(dt, target, Self::K, zeta2)
    }

    /// The same integrator with an explicit stiffness `k` (omega = sqrt k) and damping `zeta2` = 2 zeta (2 = critical).
    pub fn step_with(&mut self, dt: f32, target: f32, k: f32, zeta2: f32) -> bool {
        let c = zeta2 * k.sqrt();
        let h = dt / 4.0;
        for _ in 0..4 {
            self.v += (k * (target - self.x) - c * self.v) * h;
            self.x += self.v * h;
        }
        let done = (target - self.x).abs() < 3e-4 && self.v.abs() < 6e-3;
        if done {
            self.x = target;
            self.v = 0.0;
        }
        done
    }
}

// ---------------------------------------------------------------- hover ramps / toast

/// Hover ramps of a row of cells (buttons, segments, list rows, calendar days): one linear 0..1 progress per cell, read eased
/// through `get`. Nothing is stored for time: `busy` says whether any cell is still on its way, so the owner asks for frames
/// (a timer, `request_frame`) only while it is, and a settled set costs nothing.
#[derive(Clone, Debug, Default)]
pub struct HoverRamps {
    v: Vec<f32>,
}
impl HoverRamps {
    pub fn new(n: usize) -> HoverRamps {
        HoverRamps { v: vec![0.0; n] }
    }

    /// Make room for at least `n` cells (a list that grows).
    pub fn ensure(&mut self, n: usize) {
        if self.v.len() < n {
            self.v.resize(n, 0.0);
        }
    }

    /// Eased progress of cell `i` (0 when unknown).
    pub fn get(&self, i: usize) -> f32 {
        smooth(self.v.get(i).copied().unwrap_or(0.0))
    }

    /// True while some cell has not reached the end `hot` asks for.
    pub fn busy(&self, hot: impl Fn(usize) -> bool) -> bool {
        self.v.iter().enumerate().any(|(i, &v)| v != if hot(i) { 1.0 } else { 0.0 })
    }

    /// One step: hot cells ramp up in `t_in` seconds, the others down in `t_out`; `changed(i)` is called for every cell that moved.
    pub fn step(&mut self, dt: f32, t_in: f32, t_out: f32, hot: impl Fn(usize) -> bool, mut changed: impl FnMut(usize)) {
        for (i, v) in self.v.iter_mut().enumerate() {
            let n = ramp(*v, hot(i), dt, t_in, t_out);
            if n != *v {
                *v = n;
                changed(i);
            }
        }
    }

    /// Reduced motion / first show: every cell jumps to its end at once.
    pub fn snap(&mut self, hot: impl Fn(usize) -> bool, mut changed: impl FnMut(usize)) {
        for (i, v) in self.v.iter_mut().enumerate() {
            let n = if hot(i) { 1.0 } else { 0.0 };
            if n != *v {
                *v = n;
                changed(i);
            }
        }
    }
}

/// Toast timings (seconds) and the rise in logical px, the same 8 px as the prompt's glide.
pub const TOAST_IN: f32 = 0.18;
pub const TOAST_OUT: f32 = 0.22;
pub const TOAST_RISE: f32 = 8.0;

/// How a toast looks `t_in` seconds after it appeared, `t_out` seconds into its fade-out (None = still held):
/// (opacity, rise in logical px still to climb, done = fully gone and may be dropped). Reduced motion: no rise, no fade.
pub fn toast_look(t_in: f32, t_out: Option<f32>, reduced: bool) -> (f32, f32, bool) {
    if reduced {
        return (1.0, 0.0, t_out.is_some());
    }
    let p = (t_in / TOAST_IN).clamp(0.0, 1.0);
    let e = 1.0 - (1.0 - p).powi(3); // ease-out cubic
    match t_out {
        None => (e, (1.0 - e) * TOAST_RISE, false),
        Some(o) => {
            let q = (o / TOAST_OUT).clamp(0.0, 1.0);
            (e * (1.0 - q), 0.0, q >= 1.0)
        }
    }
}

// ---------------------------------------------------------------- materialise

/// What a card looks like `t` seconds into its (de)materialisation.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MatState {
    pub scale: f32,
    pub alpha: f32,
    /// 0 = normal lens blur .. 1 = fully diffused (the lens sigma goes from normal to `BLUR_MAX`)
    pub blur: f32,
    /// rim sweep progress 0..1 around the contour (None = no sweep)
    pub sweep: Option<f32>,
    pub finished: bool,
}

pub const MAT_IN: f32 = 0.45;
pub const MAT_OUT: f32 = 0.25;
/// lens sigma (logical px) of a fully diffused card
pub const BLUR_MAX: f32 = 24.0;

/// Condense (`out` = false: scale 0.92 -> 1 with a spring overshoot, blur 24 -> normal, fade in, one rim sweep, 450 ms) or
/// dematerialise (blur out + fade, 250 ms). `reduced`: only a fade.
pub fn materialize(t: f32, out: bool, reduced: bool) -> MatState {
    let t = t.max(0.0);
    if reduced {
        let p = (t / 0.18).clamp(0.0, 1.0);
        return MatState { scale: 1.0, alpha: if out { 1.0 - p } else { p }, blur: 0.0, sweep: None, finished: p >= 1.0 };
    }
    if out {
        let p = (t / MAT_OUT).clamp(0.0, 1.0);
        let e = p * p;
        MatState { scale: 1.0 - 0.05 * e, alpha: (1.0 - p).powf(1.4), blur: e, sweep: None, finished: p >= 1.0 }
    } else {
        let (a, w) = (9.0f32, 17.0f32);
        let spring = (-a * t).exp() * ((w * t).cos() + a / w * (w * t).sin());
        let p = (t / MAT_IN).clamp(0.0, 1.0);
        let sw = ((t - 0.04) / 0.38).clamp(0.0, 1.0);
        MatState {
            scale: 1.0 - 0.08 * spring,
            alpha: (t / 0.22).clamp(0.0, 1.0),
            blur: (1.0 - (t / 0.36).clamp(0.0, 1.0)).powi(2),
            sweep: (sw > 0.0 && sw < 1.0).then_some(sw),
            finished: p >= 1.0,
        }
    }
}

// ---------------------------------------------------------------- appearance

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Appearance {
    #[default]
    Dark,
    Light,
}
impl Appearance {
    pub fn k(self) -> f32 {
        if self == Appearance::Light {
            1.0
        } else {
            0.0
        }
    }
}

/// `widgetsAppearance` / `--appearance`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum AppearancePref {
    Auto,
    #[default]
    Dark,
    Light,
}
impl AppearancePref {
    pub fn parse(s: &str) -> Option<AppearancePref> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(AppearancePref::Auto),
            "dark" => Some(AppearancePref::Dark),
            "light" => Some(AppearancePref::Light),
            _ => None,
        }
    }
}

/// Statistics of the 8x8 luminance probe of the backdrop under a card.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ProbeStats {
    pub mean: f32,
    /// 25th percentile (the dark end of "mostly bright")
    pub p25: f32,
    /// 90th percentile
    pub p90: f32,
    /// share of cells brighter than 0.62
    pub bright: f32,
    /// standard deviation over the text area (the inner 6x6 cells)
    pub contrast: f32,
}

/// `cells`: 64 luminances 0..1, row-major.
pub fn analyze(cells: &[f32; 64]) -> ProbeStats {
    let mean = cells.iter().sum::<f32>() / 64.0;
    let mut s = *cells;
    s.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let bright = cells.iter().filter(|&&v| v > 0.62).count() as f32 / 64.0;
    let (mut n, mut sum, mut sq) = (0.0f32, 0.0f32, 0.0f32);
    for y in 1..7 {
        for x in 1..7 {
            let v = cells[y * 8 + x];
            n += 1.0;
            sum += v;
            sq += v * v;
        }
    }
    let m = sum / n;
    ProbeStats { mean, p25: s[16], p90: s[57], bright, contrast: (sq / n - m * m).max(0.0).sqrt() }
}

/// 0..1 "how much does this backdrop want dark ink": mostly bright AND calm. A mixed bright / dark backdrop scores low
/// (the dark glass keeps white text legible on both halves).
pub fn light_score(st: &ProbeStats) -> f32 {
    0.5 * st.mean + 0.5 * st.p25 - 0.5 * (st.contrast - 0.10).max(0.0)
}

pub const TO_LIGHT: f32 = 0.66;
pub const TO_DARK: f32 = 0.56;

/// Per-card appearance with hysteresis: the score is low-passed, the thresholds are 0.10 apart and a flip in live mode needs two
/// consecutive probes on the other side (no flicker on a video wallpaper).
#[derive(Clone, Copy, Debug, Default)]
pub struct AppearanceState {
    pub cur: Appearance,
    pub score: f32,
    pending: u8,
    known: bool,
}
impl AppearanceState {
    /// Feed a new probe; true when the appearance flipped. `smooth` = live mode (low-pass + dwell), else the first value wins at once.
    pub fn update(&mut self, st: &ProbeStats, smooth: bool) -> bool {
        let v = light_score(st);
        let first = !self.known || !smooth;
        self.score = if first { v } else { self.score + (v - self.score) * 0.6 };
        self.known = true;
        let want = match self.cur {
            Appearance::Dark if self.score >= TO_LIGHT => Appearance::Light,
            Appearance::Light if self.score <= TO_DARK => Appearance::Dark,
            c => c,
        };
        if want == self.cur {
            self.pending = 0;
            return false;
        }
        self.pending += 1;
        if first || self.pending >= 2 {
            self.cur = want;
            self.pending = 0;
            return true;
        }
        false
    }
}

/// Cross-fade progress step: `look` (0 dark .. 1 light) moves to `target` in 250 ms.
pub fn look_step(look: f32, target: f32, dt: f32) -> f32 {
    let d = dt / 0.25;
    if look < target {
        (look + d).min(target)
    } else {
        (look - d).max(target)
    }
}

// ---------------------------------------------------------------- size morph

/// A card changing size class (S / M / L): the drawn rectangle springs from `from` to `to` (canvas px, x y w h) with a tiny overshoot
/// (zeta ~0.8, about 1 %, 2-3 px: less than the 16 px gap to a neighbour). Nothing runs once `step` reported rest.
#[derive(Clone, Copy, Debug)]
pub struct Morph {
    pub from: R4,
    pub to: R4,
    p: Bump,
}
impl Morph {
    const K: f32 = 380.0; // omega ~ 19.5 rad/s: settles in ~0.3 s
    const ZETA2: f32 = 1.6;

    pub fn new(from: R4, to: R4) -> Morph {
        Morph { from, to, p: Bump::resting(0.0) }
    }

    /// Progress at an arbitrary instant (headless filmstrips): `ms` after the start.
    pub fn at(from: R4, to: R4, ms: f32) -> Morph {
        let mut m = Morph::new(from, to);
        let mut t = 0.0;
        while t + 4.0 <= ms {
            m.step(0.004);
            t += 4.0;
        }
        m
    }

    /// One animation step; true when at rest on `to`.
    pub fn step(&mut self, dt: f32) -> bool {
        self.p.step_with(dt, 1.0, Self::K, Self::ZETA2)
    }

    /// 0 at the start, 1 at rest (slightly above 1 during the overshoot).
    pub fn progress(&self) -> f32 {
        self.p.x
    }

    /// The rectangle as drawn now.
    pub fn rect(&self) -> R4 {
        let k = self.p.x;
        let l = |a: f32, b: f32| a + (b - a) * k;
        (l(self.from.0, self.to.0), l(self.from.1, self.to.1), l(self.from.2, self.to.2).max(1.0), l(self.from.3, self.to.3).max(1.0))
    }

    /// Opacity of the old layout (1 -> 0 over the first 60 %) and of the new one (0 -> 1 from 10 % to 70 %): a smoothed cross-fade
    /// whose two halves overlap, so the card is never empty in between.
    pub fn fades(&self) -> (f32, f32) {
        let k = self.p.x.clamp(0.0, 1.0);
        (1.0 - smooth(k / 0.6), smooth((k - 0.1) / 0.6))
    }
}

// ---------------------------------------------------------------- merge

/// Cards closer than this (logical px) pull a liquid neck between them; the grid gap is 16 so resting cards never merge.
pub const MERGE_GAP: f32 = 15.0;

pub type R4 = (f32, f32, f32, f32); // x, y, w, h

/// Euclidean distance between two rectangles (0 when they touch or overlap).
pub fn rect_gap(a: R4, b: R4) -> f32 {
    let dx = (b.0 - (a.0 + a.2)).max(a.0 - (b.0 + b.2)).max(0.0);
    let dy = (b.1 - (a.1 + a.3)).max(a.1 - (b.1 + b.3)).max(0.0);
    dx.hypot(dy)
}

pub fn overlaps(a: R4, b: R4) -> bool {
    a.0 < b.0 + b.2 && b.0 < a.0 + a.2 && a.1 < b.1 + b.3 && b.1 < a.1 + a.3
}

/// 0 (separate) .. 1 (touching): how merged two cards are. Overlapping cards (one dragged across another) do not merge.
pub fn merge_amount(a: R4, b: R4, scale: f32) -> f32 {
    if overlaps(a, b) {
        return 0.0;
    }
    let g = rect_gap(a, b);
    (1.0 - g / (MERGE_GAP * scale)).clamp(0.0, 1.0)
}

/// Indices (i < j) and amount of every merging pair.
pub fn merge_pairs(rects: &[R4], scale: f32) -> Vec<(usize, usize, f32)> {
    let mut out = Vec::new();
    for i in 0..rects.len() {
        for j in i + 1..rects.len() {
            let m = merge_amount(rects[i], rects[j], scale);
            if m > 0.0 {
                out.push((i, j, m));
            }
        }
    }
    out
}

// ---------------------------------------------------------------- keyed glides

/// Eased values by key (a ring's fraction, a bar, a percent, a list row's slot): `set` a target, `step` moves every shown value
/// towards its target by `dt`, `get` reads what to draw. A key seen for the first time starts AT its target (no animation on first show).
/// Exponential approach: about 95 % of the way in 3 `tau`, snapping once within a hair of the target so the motion really ends.
#[derive(Clone, Debug)]
pub struct Glide {
    v: std::collections::HashMap<String, Slot>,
    tau: f32,
    /// `Some(seconds)`: a fixed-time ease-in-out (smoothstep) instead of the exponential approach: it starts gently (list rows that change places)
    dur: Option<f32>,
}
/// One key: the shown value, its target, and (smooth mode) where the current move started and how far along it is.
#[derive(Clone, Copy, Debug)]
struct Slot {
    v: f32,
    to: f32,
    from: f32,
    t: f32,
}
impl Slot {
    fn at(v: f32, to: f32) -> Slot {
        Slot { v, to, from: v, t: 0.0 }
    }
}
impl Default for Glide {
    fn default() -> Glide {
        Glide::new(0.09)
    }
}
impl Glide {
    /// `tau` = the time constant in seconds (0.09 settles in about 300 ms).
    pub fn new(tau: f32) -> Glide {
        Glide { v: Default::default(), tau, dur: None }
    }
    /// Fixed-time ease-in-out of `secs` per move; a new target in mid-air restarts the move from where the value is now.
    pub fn smooth(secs: f32) -> Glide {
        Glide { v: Default::default(), tau: 0.09, dur: Some(secs.max(0.05)) }
    }
    /// New target. True when it changed (something will move).
    pub fn set(&mut self, key: &str, target: f32) -> bool {
        match self.v.get_mut(key) {
            Some(e) if e.to != target => {
                e.to = target;
                e.from = e.v;
                e.t = 0.0;
                true
            }
            Some(_) => false,
            None => {
                self.v.insert(key.to_string(), Slot::at(target, target));
                false
            }
        }
    }
    /// Like `set`, but a key seen for the first time starts at `from` (a row sliding in, a row fading out).
    pub fn set_from(&mut self, key: &str, from: f32, target: f32) -> bool {
        if !self.v.contains_key(key) {
            self.v.insert(key.to_string(), Slot::at(from, target));
            return from != target;
        }
        self.set(key, target)
    }
    /// Restart the move of `key` from `from` to `target` (also when the key exists): a blend 0 -> 1 between two pictures.
    pub fn start(&mut self, key: &str, from: f32, target: f32) -> bool {
        self.v.insert(key.to_string(), Slot::at(from, target));
        from != target
    }
    /// Show `target` at once (previews, reduced motion).
    pub fn jump(&mut self, key: &str, target: f32) {
        self.v.insert(key.to_string(), Slot::at(target, target));
    }
    /// What to draw now (None = the key was never set).
    pub fn get(&self, key: &str) -> Option<f32> {
        self.v.get(key).map(|e| e.v)
    }
    pub fn target(&self, key: &str) -> Option<f32> {
        self.v.get(key).map(|e| e.to)
    }
    pub fn forget(&mut self, key: &str) {
        self.v.remove(key);
    }
    /// Keep only the keys `keep` accepts.
    pub fn retain(&mut self, mut keep: impl FnMut(&str) -> bool) {
        self.v.retain(|k, _| keep(k));
    }
    /// Is any shown value off its target?
    pub fn moving(&self) -> bool {
        self.v.values().any(|e| e.v != e.to)
    }
    /// Advance every value by `dt` seconds. True while something is still moving.
    pub fn step(&mut self, dt: f32) -> bool {
        let (tau, dur) = (self.tau, self.dur);
        let mut busy = false;
        for e in self.v.values_mut() {
            if e.v == e.to {
                continue;
            }
            if let Some(d) = dur {
                e.t = (e.t + dt / d).min(1.0);
                e.v = if e.t >= 1.0 { e.to } else { e.from + (e.to - e.from) * smooth(e.t) };
            } else {
                // relative eps: byte counts and percents both end (an absolute one would never be met by a float near 1e9)
                e.v = follow(e.v, e.to, dt, tau, 0.002f32.max(e.to.abs() * 1e-3));
            }
            busy |= e.v != e.to;
        }
        busy
    }
}

// ---------------------------------------------------------------- flowing graphs

/// Polyline of a sparkline that FLOWS instead of jumping when a sample arrives: `values` is the history (oldest first), `visible` the number
/// of samples that fit the graph width, `p` 0..1 how far the NEXT sample interval has come (time since the newest sample / the sample period).
/// Output points are (x, value) with x = 0 at the left edge of the graph and 1 at the right (points left of 0 are meant to be clipped).
/// The graph is CURRENT: the right edge always shows the newest sample's value (a spike appears at once and keeps its real shape, it is not
/// smeared over a whole interval). Between samples every point slides left by `p` of a step and the head holds the newest value out to the
/// right edge, so the line scrolls continuously; when the next sample arrives (`p` back at 0) every old point is exactly where it was at
/// `p` = 1 and only the head turns from a flat hold into the real step to the new value.
pub fn flow_points(values: &[f32], visible: usize, p: f32, out: &mut Vec<(f32, f32)>) {
    out.clear();
    let n = values.len();
    if n < 2 {
        return;
    }
    let step = 1.0 / (visible.max(2) - 1) as f32;
    let p = p.clamp(0.0, 1.0);
    for (i, &v) in values.iter().enumerate() {
        let x = 1.0 - ((n - 1 - i) as f32 + p) * step;
        if x >= -step {
            out.push((x, v));
        }
    }
    out.push((1.0, values[n - 1])); // the hold up to the right edge
}

/// Position inside the current sample interval: 0 right after a sample arrived, 1 one period later (it waits there for a late sample).
pub fn flow_phase(since_sample: f32, period: f32) -> f32 {
    (since_sample / period.max(0.05)).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flow_points_are_current_and_scroll_without_jumping() {
        let mut a = Vec::new();
        let v = [1.0, 5.0, 3.0, 9.0];
        flow_points(&v, 4, 1.0, &mut a); // visible 4: step 1/3; a whole interval later: the last sample sits one step left, the head holds it
        assert_eq!(a.last().copied(), Some((1.0, 9.0)));
        assert!((a[a.len() - 2].0 - (1.0 - 1.0 / 3.0)).abs() < 1e-6 && a[a.len() - 2].1 == 9.0);
        // a new sample arrives (p back to 0): every old point is where it was, and the right edge shows the NEW value at once
        let mut b = Vec::new();
        let w = [1.0, 5.0, 3.0, 9.0, 2.0];
        flow_points(&w, 4, 0.0, &mut b);
        let at = |pts: &[(f32, f32)], x: f32| pts.iter().find(|q| (q.0 - x).abs() < 1e-5).map(|q| q.1);
        for &(x, y) in &a[..a.len() - 1] {
            assert_eq!(at(&b, x), Some(y), "x = {x}");
        }
        assert_eq!(b.last().copied(), Some((1.0, 2.0)), "the spike / drop is not smeared over the interval");
        // half way: the newest sample has slid half a step left, the head still shows the newest value
        flow_points(&w, 4, 0.5, &mut b);
        assert_eq!(b.last().copied(), Some((1.0, 2.0)));
        assert!((b[b.len() - 2].0 - (1.0 - 0.5 / 3.0)).abs() < 1e-5 && b[b.len() - 2].1 == 2.0);
        // points far left of the graph are dropped (a full history keeps one spare sample just outside)
        let long: Vec<f32> = (0..10).map(|i| i as f32).collect();
        flow_points(&long, 4, 0.0, &mut b);
        assert!(b.iter().all(|q| q.0 >= -1.0 / 3.0 - 1e-6) && b.len() <= 6);
        // fewer than two samples: nothing to draw
        flow_points(&[1.0], 4, 0.5, &mut b);
        assert!(b.is_empty());
    }

    #[test]
    fn flow_phase_runs_one_period_and_waits() {
        assert_eq!(flow_phase(0.0, 1.0), 0.0);
        assert_eq!(flow_phase(0.25, 1.0), 0.25);
        assert_eq!(flow_phase(3.0, 1.0), 1.0);
        assert_eq!(flow_phase(0.5, 0.0), 1.0); // a zero period does not divide by zero
    }

    #[test]
    fn glide_first_seen_key_has_no_animation() {
        let mut g = Glide::default();
        assert!(!g.set("a", 40.0));
        assert_eq!((g.get("a"), g.target("a")), (Some(40.0), Some(40.0)));
        assert!(!g.moving() && !g.step(0.016));
        assert_eq!(g.get("nope"), None);
    }

    #[test]
    fn glide_settles_in_about_300_ms_and_snaps() {
        let mut g = Glide::default();
        g.set("a", 0.0);
        assert!(g.set("a", 1.0));
        let mut t = 0.0;
        let mut mid = None;
        while g.step(1.0 / 60.0) {
            t += 1.0 / 60.0;
            if mid.is_none() && t > 0.05 {
                mid = g.get("a");
            }
            assert!(t < 1.0, "never settled");
        }
        assert!(t > 0.15 && t < 0.6, "settled after {t}");
        assert!(mid.unwrap() > 0.0 && mid.unwrap() < 1.0);
        assert_eq!(g.get("a"), Some(1.0)); // exactly the target
        assert!(!g.moving());
        // big magnitudes (bytes) end as well
        g.set("ram", 3.0e9);
        g.set("ram", 3.2e9);
        let mut n = 0;
        while g.step(0.016) {
            n += 1;
            assert!(n < 200);
        }
        assert_eq!(g.get("ram"), Some(3.2e9));
    }

    #[test]
    fn smooth_glide_starts_gently_and_takes_its_time() {
        let (mut s, mut e) = (Glide::smooth(0.6), Glide::new(0.12));
        for g in [&mut s, &mut e] {
            g.set("y", 0.0);
            g.set("y", 1.0);
            g.step(0.03);
        }
        assert!(s.get("y").unwrap() < 0.02 && e.get("y").unwrap() > 0.2, "ease-in vs the exponential's instant start");
        let mut t = 0.03;
        while s.step(1.0 / 60.0) {
            t += 1.0 / 60.0;
            assert!(t < 1.0);
        }
        assert!(t > 0.55 && t < 0.7, "{t}");
        assert_eq!(s.get("y"), Some(1.0));
        // a new target in mid-air restarts the move from where the value is now
        s.set("y", 0.0);
        s.step(0.3);
        let mid = s.get("y").unwrap();
        s.set("y", 1.0);
        assert_eq!(s.get("y"), Some(mid));
        while s.step(1.0 / 60.0) {}
        assert_eq!(s.get("y"), Some(1.0));
    }

    #[test]
    fn glide_set_from_and_jump_and_retain() {
        let mut g = Glide::default();
        assert!(g.set_from("row", 4.6, 3.0)); // a row sliding in from below
        assert_eq!(g.get("row"), Some(4.6));
        g.step(0.05);
        let y = g.get("row").unwrap();
        assert!(y < 4.6 && y > 3.0);
        assert!(!g.set_from("row", 9.0, 3.0)); // already known: the start is ignored
        g.jump("row", 1.0);
        assert_eq!((g.get("row"), g.moving()), (Some(1.0), false));
        g.set("keep", 1.0);
        g.retain(|k| k == "keep");
        assert_eq!((g.get("row"), g.get("keep")), (None, Some(1.0)));
    }

    #[test]
    fn size_morph_springs_to_the_target_and_comes_to_rest() {
        let (from, to) = ((20.0, 50.0, 170.0, 170.0), (20.0, 50.0, 356.0, 356.0));
        let mut m = Morph::new(from, to);
        assert_eq!(m.rect(), from);
        let (mut t, mut max_w, mut done) = (0.0f32, 0.0f32, false);
        while !done && t < 2.0 {
            done = m.step(0.016);
            t += 0.016;
            max_w = max_w.max(m.rect().2);
            assert_eq!((m.rect().0, m.rect().1), (20.0, 50.0), "the top-left stays");
        }
        assert!(done && m.rect() == to, "rest after {t}s: {:?}", m.rect());
        assert!(t < 0.8, "settles within 0.8 s, took {t}");
        assert!(max_w < to.2 + 8.0, "overshoot stays below the 16 px gap: {max_w}");
        // a morph never leaves a frame behind once settled, and a filmstrip sample matches the stepped one
        assert!((Morph::at(from, to, 100.0).progress() - Morph::at(from, to, 100.0).progress()).abs() < 1e-6);
        let mid = Morph::at(from, to, 100.0);
        assert!(mid.progress() > 0.2 && mid.progress() < 0.9, "{}", mid.progress());
        let (old, new) = mid.fades();
        assert!((0.0..=1.0).contains(&old) && (0.0..=1.0).contains(&new));
        let (o0, n0) = Morph::new(from, to).fades();
        let (o1, n1) = m.fades();
        assert_eq!((o0, n0, o1, n1), (1.0, 0.0, 0.0, 1.0));
    }

    #[test]
    fn light_swings_against_the_motion_and_settles() {
        let mut l = LightSpring::default();
        assert!(l.is_rest());
        let drive = LightSpring::drive_for(1500.0, 0.0);
        assert!(drive.0 < -0.5 && drive.1 == 0.0);
        // dragged to the right for 300 ms: the offset points left
        for _ in 0..19 {
            l.step(0.016, drive);
        }
        assert!(l.o.0 < -0.5, "offset {:?}", l.o);
        let a0 = l.angle(315.0);
        assert!((a0 - 315.0).abs() > 10.0, "angle moved to {a0}");
        // released: back to rest within ~0.8 s, with at most a small overshoot
        let (mut t, mut max_o) = (0.0f32, 0.0f32);
        let mut done = false;
        while !done && t < 2.0 {
            done = l.step(0.016, (0.0, 0.0));
            t += 0.016;
            max_o = max_o.max(l.o.0);
        }
        assert!(done && l.is_rest(), "rest after {t}s");
        assert!(t < 1.0 && max_o < 0.25, "t={t} overshoot={max_o}");
        assert!((l.angle(315.0) - 315.0).abs() < 1e-3);
    }

    #[test]
    fn light_angle_of_the_neutral_state_is_the_base() {
        for a in [0.0, 45.0, 90.0, 180.0, 315.0] {
            assert!((LightSpring::default().angle(a) - a).abs() < 1e-3);
        }
    }

    #[test]
    fn glint_fades_with_distance() {
        assert!((glint_target(0.0, 1.0) - 1.0).abs() < 1e-5);
        assert!(glint_target(30.0, 1.0) > 0.2 && glint_target(30.0, 1.0) < 0.8);
        assert_eq!(glint_target(80.0, 1.0), 0.0);
        assert_eq!(glint_target(-70.0, 1.0), 0.0);
        let mut v = 0.0;
        for _ in 0..80 {
            v = approach(v, 1.0, 0.016, 0.12);
        }
        assert_eq!(v, 1.0);
    }

    #[test]
    fn hover_eases_in_and_out_and_holds() {
        // in: 180 ms, out: 250 ms, then it stays put (no timeout while the pointer rests)
        let (mut h, mut t) = (0.0, 0.0);
        while h < 1.0 {
            h = ramp(h, true, 0.016, HOVER_IN, HOVER_OUT);
            t += 0.016;
        }
        assert!((0.17..0.2).contains(&t), "in {t}");
        for _ in 0..500 {
            h = ramp(h, true, 0.016, HOVER_IN, HOVER_OUT);
        }
        assert_eq!(h, 1.0);
        t = 0.0;
        while h > 0.0 {
            h = ramp(h, false, 0.016, HOVER_IN, HOVER_OUT);
            t += 0.016;
        }
        assert!((0.24..0.27).contains(&t), "out {t}");
        assert_eq!((smooth(0.0), smooth(1.0), smooth(0.5)), (0.0, 1.0, 0.5));
        assert!(smooth(0.1) < 0.1 && smooth(0.9) > 0.9);
    }

    #[test]
    fn hover_rim_has_a_floor_inside() {
        assert!((hover_rim(0.0, 1.0) - 1.0).abs() < 1e-5);
        // the middle of a card (85 px from every edge) keeps the floor, at 2x too
        assert_eq!(hover_rim(-85.0, 1.0), RIM_FLOOR);
        assert_eq!(hover_rim(-170.0, 2.0), RIM_FLOOR);
        // it rises monotonically towards the edge
        let mut last = 0.0;
        for d in (0..=80).rev() {
            let v = hover_rim(-(d as f32), 1.0);
            assert!(v >= last && v >= RIM_FLOOR, "{d}: {v}");
            last = v;
        }
        // outside: the approach fall-off, gone at 62 px
        assert!(hover_rim(30.0, 1.0) < 0.8 && hover_rim(30.0, 1.0) > 0.2);
        assert_eq!(hover_rim(70.0, 1.0), 0.0);
    }

    #[test]
    fn follow_snaps_and_wraps_the_short_way() {
        let mut v = 0.0;
        let mut n = 0;
        while v != 100.0 {
            v = follow(v, 100.0, 0.016, 0.07, 0.25);
            n += 1;
            assert!(n < 60, "never snaps");
        }
        assert!(n > 5);
        assert!((wrap_dist(0.95, 0.05) - 0.1).abs() < 1e-5 && (wrap_dist(0.05, 0.95) + 0.1).abs() < 1e-5);
        // 0.95 -> 0.05 goes forward through 0 (0.97, 0.99 ...), not back through 0.5
        let u = follow_wrap(0.95, 0.05, 0.016, 0.08, 1e-4);
        assert!(u > 0.95 || u < 0.05, "{u}");
        let mut u = 0.95;
        for _ in 0..100 {
            u = follow_wrap(u, 0.05, 0.016, 0.08, 5e-4);
        }
        assert_eq!(u, 0.05);
    }

    #[test]
    fn bump_springs_with_a_small_overshoot_and_settles() {
        let mut b = Bump::default();
        assert!(b.at_rest(bump_target(false, false)));
        // hover: up to 1.012 with a soft overshoot, at rest within 0.5 s
        let (mut t, mut max) = (0.0f32, 0.0f32);
        while !b.step(0.016, bump_target(true, false)) {
            t += 0.016;
            max = max.max(b.x);
            assert!(t < 0.6, "hover never settles");
        }
        assert!(max > HOVER_SCALE && max < HOVER_SCALE + 0.004, "hover overshoot {max}");
        // press: down to 0.985 without bouncing below it
        let mut min = 2.0f32;
        while !b.step(0.016, bump_target(true, true)) {
            min = min.min(b.x);
        }
        assert!(min >= PRESS_SCALE - 1e-4, "press undershoot {min}");
        // release: springs back past the hover scale once, then rests
        let (mut t, mut max) = (0.0f32, 0.0f32);
        while !b.step(0.016, bump_target(true, false)) {
            t += 0.016;
            max = max.max(b.x);
            assert!(t < 0.7, "release never settles");
        }
        assert!(max > HOVER_SCALE + 0.001 && max < HOVER_SCALE + 0.006, "release overshoot {max}");
        assert!(b.at_rest(HOVER_SCALE));
    }

    #[test]
    fn materialize_in_condenses_and_out_dissolves() {
        let s0 = materialize(0.0, false, false);
        assert!((s0.scale - 0.92).abs() < 1e-3 && s0.alpha == 0.0 && (s0.blur - 1.0).abs() < 1e-5 && !s0.finished);
        let mid = materialize(0.2, false, false);
        assert!(mid.alpha > 0.8 && mid.blur < 0.3 && mid.sweep.is_some());
        let end = materialize(0.5, false, false);
        assert!(end.finished && end.alpha == 1.0 && end.blur == 0.0 && (end.scale - 1.0).abs() < 0.002 && end.sweep.is_none());
        // the spring overshoots 1.0 a little
        let max = (0..45).map(|i| materialize(i as f32 * 0.01, false, false).scale).fold(0.0f32, f32::max);
        assert!(max > 1.0 && max < 1.04, "overshoot {max}");
        let o = materialize(0.25, true, false);
        assert!(o.finished && o.alpha == 0.0);
        assert!(materialize(0.1, true, false).blur > 0.1);
        // reduced motion: a plain fade, no scale / blur / sweep
        let r = materialize(0.09, false, true);
        assert!(r.scale == 1.0 && r.blur == 0.0 && r.sweep.is_none() && (r.alpha - 0.5).abs() < 0.01);
    }

    fn cells(f: impl Fn(usize, usize) -> f32) -> [f32; 64] {
        let mut c = [0.0; 64];
        for y in 0..8 {
            for x in 0..8 {
                c[y * 8 + x] = f(x, y);
            }
        }
        c
    }

    #[test]
    fn appearance_follows_the_backdrop_with_hysteresis() {
        let dark = analyze(&cells(|_, _| 0.15));
        let white = analyze(&cells(|_, _| 0.9));
        let mixed = analyze(&cells(|x, _| if x < 4 { 0.95 } else { 0.1 })); // half bright, half dark
        let orange = analyze(&cells(|_, _| 0.55));
        assert!(light_score(&dark) < TO_DARK && light_score(&white) > TO_LIGHT);
        assert!(light_score(&mixed) < TO_LIGHT, "mixed backdrop keeps the dark glass: {}", light_score(&mixed));
        assert!(light_score(&orange) < TO_LIGHT, "the orange widgets of the references stay dark: {}", light_score(&orange));

        let mut st = AppearanceState::default();
        assert!(!st.update(&dark, false) && st.cur == Appearance::Dark);
        // live: needs two consecutive bright probes
        let mut n = 1;
        while !st.update(&white, true) {
            n += 1;
            assert!(n <= 4, "flipped too late");
        }
        assert!(n >= 2 && st.cur == Appearance::Light, "the dwell needs more than one probe: {n}");
        // a single dark blip does not flip it back, a score inside the band never does
        let band = analyze(&cells(|_, _| 0.61));
        for _ in 0..10 {
            assert!(!st.update(&band, true));
        }
        assert_eq!(st.cur, Appearance::Light);
        assert!(!st.update(&dark, true) || st.cur == Appearance::Light);
        for _ in 0..4 {
            st.update(&dark, true);
        }
        assert_eq!(st.cur, Appearance::Dark);
        // headless (not smooth): the first measurement wins immediately
        let mut h = AppearanceState::default();
        assert!(h.update(&white, false) && h.cur == Appearance::Light);
    }

    #[test]
    fn look_crossfade_takes_250ms() {
        let (mut k, mut t) = (0.0, 0.0);
        while k < 1.0 {
            k = look_step(k, 1.0, 0.016);
            t += 0.016;
        }
        assert!((0.24..0.28).contains(&t), "{t}");
        assert_eq!(look_step(1.0, 0.0, 1.0), 0.0);
    }

    #[test]
    fn hover_ramps_settle_and_stop_asking_for_frames() {
        let mut r = HoverRamps::new(3);
        let hot = |i: usize| i == 1;
        assert!(r.busy(hot), "a hovered cell has to ramp up");
        assert!(!r.busy(|_| false), "nothing hot and nothing lit: idle");
        let (mut n, mut moved) = (0, 0);
        while r.busy(hot) {
            r.step(0.016, 0.12, 0.18, hot, |i| {
                assert_eq!(i, 1);
                moved += 1;
            });
            n += 1;
            assert!(n < 20, "never settles");
        }
        assert!((7..=9).contains(&n), "120 ms in at 16 ms steps: {n}");
        assert_eq!((r.get(1), r.get(0)), (1.0, 0.0));
        // leaving ramps back out in 180 ms and then the set is idle again
        let mut m = 0;
        while r.busy(|_| false) {
            r.step(0.016, 0.12, 0.18, |_| false, |_| {});
            m += 1;
            assert!(m < 30);
        }
        assert!((11..=13).contains(&m), "{m}");
        assert!(moved > 0 && !r.busy(|_| false));
        // reduced motion: snap
        r.snap(|i| i == 2, |_| {});
        assert_eq!((r.get(2), r.get(1)), (1.0, 0.0));
        assert!(!r.busy(|i| i == 2));
    }

    #[test]
    fn toast_rises_fades_and_ends() {
        let (a0, r0, d0) = toast_look(0.0, None, false);
        assert!(a0 == 0.0 && (r0 - TOAST_RISE).abs() < 1e-6 && !d0);
        let (a, r, _) = toast_look(TOAST_IN / 2.0, None, false);
        assert!(a > 0.8 && r < TOAST_RISE / 2.0, "ease-out: most of the move is early ({a}, {r})");
        assert_eq!(toast_look(1.0, None, false), (1.0, 0.0, false));
        let (a, _, d) = toast_look(1.0, Some(TOAST_OUT / 2.0), false);
        assert!(a > 0.4 && a < 0.6 && !d);
        let (a, _, d) = toast_look(1.0, Some(TOAST_OUT), false);
        assert!(a == 0.0 && d, "the toast is only dropped once the fade ended");
        // reduced motion: shown at once, gone at once
        assert_eq!(toast_look(0.0, None, true), (1.0, 0.0, false));
        assert!(toast_look(1.0, Some(0.0), true).2);
    }

    #[test]
    fn merge_distance_logic() {
        let a: R4 = (0.0, 0.0, 170.0, 170.0);
        let at = |gap: f32| -> R4 { (170.0 + gap, 0.0, 170.0, 170.0) };
        assert_eq!(rect_gap(a, at(40.0)), 40.0);
        assert_eq!(merge_amount(a, at(40.0), 1.0), 0.0);
        assert_eq!(merge_amount(a, at(20.0), 1.0), 0.0);
        assert_eq!(merge_amount(a, at(16.0), 1.0), 0.0, "cards at the grid gap stay separate");
        assert!(merge_amount(a, at(12.0), 1.0) > 0.0);
        assert!(merge_amount(a, at(6.0), 1.0) > merge_amount(a, at(12.0), 1.0));
        assert!((merge_amount(a, at(0.0), 1.0) - 1.0).abs() < 1e-6);
        // scale: the threshold is in logical px
        assert_eq!(merge_amount(a, (170.0 + 32.0, 0.0, 170.0, 170.0), 2.0), 0.0);
        assert!(merge_amount(a, (170.0 + 24.0, 0.0, 170.0, 170.0), 2.0) > 0.0);
        // diagonal neighbours use the corner distance
        assert!((rect_gap(a, (170.0 + 6.0, 170.0 + 8.0, 50.0, 50.0)) - 10.0).abs() < 1e-5);
        // overlapping cards do not merge
        assert_eq!(merge_amount(a, (100.0, 100.0, 170.0, 170.0), 1.0), 0.0);
        let p = merge_pairs(&[a, at(8.0), (1000.0, 0.0, 170.0, 170.0)], 1.0);
        assert_eq!(p.len(), 1);
        assert_eq!((p[0].0, p[0].1), (0, 1));
    }
}
