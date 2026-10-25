//! How often the capture threads look at the desktop. A still wallpaper needs no 15 looks a second, a video wallpaper does: the pace
//! backs off 15 -> 8 -> 4 -> 2 fps while nothing under the cards changes and snaps back to full rate on the first change (or when the
//! host says so: a card moved, edit mode, a settings change). Pure state, no clocks of its own: callers pass `now`.
use std::time::{Duration, Instant};

/// Capture rate request: `fps` is the ceiling (`auto`) or the fixed rate (`!auto`, the `captureFps` number).
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Rate {
    pub fps: u32,
    pub auto: bool,
}

/// The slower rungs of the back-off ladder (fps); the first rung is the requested ceiling.
const LOW_RUNGS: [u32; 3] = [8, 4, 2];
/// Still for this long at a rung -> one rung down.
const STILL_STEP: Duration = Duration::from_secs(2);

pub struct Pace {
    ladder: Vec<u32>,
    level: usize,
    still_since: Option<Instant>,
    auto: bool,
}

impl Pace {
    pub fn new(r: Rate) -> Pace {
        let top = r.fps.max(1);
        let mut ladder = vec![top];
        if r.auto {
            ladder.extend(LOW_RUNGS.iter().copied().filter(|&f| f < top));
        }
        Pace { ladder, level: 0, still_since: None, auto: r.auto }
    }

    pub fn fps(&self) -> u32 {
        self.ladder[self.level]
    }

    pub fn interval(&self) -> Duration {
        Duration::from_micros(1_000_000 / self.fps() as u64)
    }

    /// Back to full rate at once (card moved, edit mode, settings or appearance changed, resume from pause).
    pub fn snap(&mut self) {
        self.level = 0;
        self.still_since = None;
    }

    /// One look at the desktop: did the pixels under the cards change? Returns the interval until the next look.
    pub fn observe(&mut self, changed: bool, now: Instant) -> Duration {
        if changed {
            self.snap();
        } else if self.auto {
            let since = *self.still_since.get_or_insert(now);
            if self.level + 1 < self.ladder.len() && now.saturating_duration_since(since) >= STILL_STEP {
                self.level += 1;
                self.still_since = Some(now);
            }
        }
        self.interval()
    }
}

/// `captureFps` of config.json: "auto" / missing / garbage = None (auto), a number or numeric string 1..30 = a fixed rate.
pub fn parse_capture_fps(v: Option<&serde_json::Value>) -> Option<u32> {
    let n = match v? {
        serde_json::Value::Number(n) => n.as_f64()?,
        serde_json::Value::String(s) => s.trim().parse::<f64>().ok()?,
        _ => return None,
    };
    n.is_finite().then(|| n.round().clamp(1.0, 30.0) as u32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn auto(fps: u32) -> Pace {
        Pace::new(Rate { fps, auto: true })
    }
    fn at(t0: Instant, s: f64) -> Instant {
        t0 + Duration::from_secs_f64(s)
    }

    #[test]
    fn backs_off_one_rung_per_still_period() {
        let (mut p, t0) = (auto(15), Instant::now());
        assert_eq!(p.observe(false, t0), Duration::from_micros(1_000_000 / 15));
        assert_eq!(p.fps(), 15);
        p.observe(false, at(t0, 1.9));
        assert_eq!(p.fps(), 15);
        p.observe(false, at(t0, 2.0));
        assert_eq!(p.fps(), 8);
        p.observe(false, at(t0, 3.9));
        assert_eq!(p.fps(), 8);
        p.observe(false, at(t0, 4.0));
        assert_eq!(p.fps(), 4);
        p.observe(false, at(t0, 6.0));
        assert_eq!(p.fps(), 2);
        // the floor
        assert_eq!(p.observe(false, at(t0, 600.0)), Duration::from_millis(500));
    }

    #[test]
    fn any_change_snaps_back_to_full_rate() {
        let (mut p, t0) = (auto(15), Instant::now());
        for s in [0.0, 2.0, 4.0, 6.0] {
            p.observe(false, at(t0, s));
        }
        assert_eq!(p.fps(), 2);
        p.observe(true, at(t0, 6.5));
        assert_eq!(p.fps(), 15);
        // the still timer restarts: no back-off right after the change
        p.observe(false, at(t0, 7.0));
        p.observe(false, at(t0, 8.9));
        assert_eq!(p.fps(), 15);
        p.observe(false, at(t0, 9.0));
        assert_eq!(p.fps(), 8);
        // a host snap (card moved / edit mode) does the same
        p.snap();
        assert_eq!(p.fps(), 15);
    }

    #[test]
    fn a_video_wallpaper_never_backs_off() {
        let (mut p, t0) = (auto(15), Instant::now());
        for i in 0..300 {
            p.observe(true, at(t0, i as f64 / 15.0));
        }
        assert_eq!(p.fps(), 15);
    }

    #[test]
    fn forced_rate_is_constant() {
        let (mut p, t0) = (Pace::new(Rate { fps: 24, auto: false }), Instant::now());
        for s in [0.0, 5.0, 50.0] {
            assert_eq!(p.observe(false, at(t0, s)), Duration::from_micros(1_000_000 / 24));
        }
        p.observe(true, at(t0, 51.0));
        assert_eq!(p.fps(), 24);
    }

    #[test]
    fn ladder_never_goes_above_the_ceiling() {
        assert_eq!(auto(30).ladder, [30, 8, 4, 2]);
        assert_eq!(auto(5).ladder, [5, 4, 2]);
        assert_eq!(auto(2).ladder, [2]);
        assert_eq!(auto(0).ladder, [1]);
    }

    #[test]
    fn capture_fps_setting() {
        assert_eq!(parse_capture_fps(None), None);
        assert_eq!(parse_capture_fps(Some(&json!("auto"))), None);
        assert_eq!(parse_capture_fps(Some(&json!(12))), Some(12));
        assert_eq!(parse_capture_fps(Some(&json!("20"))), Some(20));
        assert_eq!(parse_capture_fps(Some(&json!(0))), Some(1));
        assert_eq!(parse_capture_fps(Some(&json!(500))), Some(30));
        assert_eq!(parse_capture_fps(Some(&json!(true))), None);
    }
}
