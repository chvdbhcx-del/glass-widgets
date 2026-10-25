//! Pure (no GPU, no clock) state machines behind the Liquid Glass motion: the inertial light, the materialise timeline,
//! the backdrop-driven light / dark appearance with hysteresis, the pointer glint and the neighbour-merge distance logic.
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
    let k = 1.0 - (-dt / tau).exp();
    let n = v + (target - v) * k;
    if (n - target).abs() < 0.004 {
        target
    } else {
        n
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

#[cfg(test)]
mod tests {
    use super::*;

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
