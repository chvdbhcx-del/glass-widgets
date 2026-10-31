//! Remember the brightness the user has and put it back after the display wakes up (something outside the widget, monitor firmware or
//! Windows, sometimes jumps it to 100 on wake). Pure state machine: the Quick Actions widget feeds it events and probe reads, does the
//! hardware I/O itself and obeys the `Act` it gets back. Never loops: a restore is at most `MAX_WRITES` writes, each verified by a read.
use std::collections::VecDeque;
use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// the same value on two reads at least this far apart is the user's new brightness
pub const STABLE_GAP: Duration = Duration::from_secs(5);
/// first read after the wake, and the spacing of the following reads / verifications
pub const WAKE_WAIT: Duration = Duration::from_secs(2);
pub const PROBE_GAP: Duration = Duration::from_secs(1);
const MAX_READS: u32 = 5;
const MAX_WRITES: u32 = 3;
/// percentage points a read may differ from the remembered value without a restore
const TOLERANCE: u32 = 2;
const LOG_CAP: usize = 200;

/// Diagnostic ring log: the last `LOG_CAP` lines, mirrored to a file when it has a path (the file is only touched on the first event).
pub struct Log {
    path: Option<PathBuf>,
    pub lines: VecDeque<String>,
    since_trim: usize,
}

impl Log {
    pub fn mem() -> Log {
        Log { path: None, lines: VecDeque::new(), since_trim: 0 }
    }

    /// `%LOCALAPPDATA%\Glass Widgets\brightness.log`
    pub fn file() -> Log {
        let path = std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("Glass Widgets").join("brightness.log"));
        Log { path, lines: VecDeque::new(), since_trim: usize::MAX }
    }

    pub fn push(&mut self, msg: &str) {
        let line = format!("{} {msg}", stamp());
        self.lines.push_back(line.clone());
        while self.lines.len() > LOG_CAP {
            self.lines.pop_front();
        }
        let Some(p) = self.path.clone() else { return };
        let _ = std::fs::create_dir_all(p.parent().unwrap_or(&p));
        if self.since_trim >= LOG_CAP / 2 {
            // first event of this run, then every 100 lines: cut the file to the last LOG_CAP lines
            let keep: Vec<String> = std::fs::read_to_string(&p).unwrap_or_default().lines().map(str::to_string).collect();
            let from = keep.len().saturating_sub(LOG_CAP - 1);
            let _ = std::fs::write(&p, keep[from..].iter().map(|l| format!("{l}\n")).collect::<String>());
            self.since_trim = 0;
        }
        self.since_trim += 1;
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&p) {
            let _ = writeln!(f, "{line}");
        }
    }
}

fn stamp() -> String {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
    let c = glasskit::tz::civil_from_unix(secs, crate::sys::local_utc_offset_min());
    format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}", c.y, c.mo, c.d, c.h, c.mi, c.sec)
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Phase {
    Awake,
    Asleep,
    /// after the wake: reading until two reads agree with the remembered value, or one disagrees (then restore)
    Waking { reads: u32, ok: u32 },
    /// a restore write went out: `before` = the read that triggered it
    Restoring { target: u32, before: u32, writes: u32, reads: u32 },
}

/// What the widget must do after an event.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Act {
    None,
    /// write this percentage to the monitor now (through the normal DDC / WMI path)
    Write(u32),
}

pub struct Keeper {
    pub log: Log,
    last_good: Option<u32>,
    pre_sleep: Option<u32>,
    /// the awake read that may become `last_good` once it repeats after `STABLE_GAP`
    stable: Option<(u32, Instant)>,
    phase: Phase,
    next: Option<Instant>,
    last_gesture: Option<Instant>,
    last_logged_read: Option<Option<u32>>,
}

impl Keeper {
    pub fn new(log: Log) -> Keeper {
        Keeper { log, last_good: None, pre_sleep: None, stable: None, phase: Phase::Awake, next: None, last_gesture: None, last_logged_read: None }
    }

    pub fn last_good(&self) -> Option<u32> {
        self.last_good
    }

    /// When the next brightness read is wanted (None = only the slow periodic one).
    pub fn next_probe(&self) -> Option<Instant> {
        self.next
    }

    /// The user set the brightness in the widget: it is the truth now, and any restore in progress stands down.
    pub fn gesture(&mut self, v: u32, now: Instant) {
        if self.phase == Phase::Asleep {
            return;
        }
        if self.last_gesture.map_or(true, |t| now.saturating_duration_since(t) > Duration::from_secs(2)) {
            self.log.push(&format!("gesture {v}"));
        }
        self.last_gesture = Some(now);
        (self.last_good, self.stable) = (Some(v), None);
        if matches!(self.phase, Phase::Waking { .. } | Phase::Restoring { .. }) {
            self.log.push("restore cancelled: user gesture");
            (self.phase, self.next) = (Phase::Awake, None);
        }
    }

    /// The display went off / the session locked / the PC suspends: remember what the user had.
    pub fn sleep(&mut self) {
        if self.phase == Phase::Asleep {
            return;
        }
        self.pre_sleep = self.last_good;
        (self.phase, self.next, self.stable) = (Phase::Asleep, None, None);
        self.log.push(&format!("sleep, remembered {:?}", self.pre_sleep));
    }

    /// The desk is awake again: start the checks (first read after `WAKE_WAIT`), or nothing when the setting is off.
    pub fn wake(&mut self, now: Instant, enabled: bool) {
        if self.phase != Phase::Asleep {
            return;
        }
        if enabled && self.pre_sleep.is_some() {
            self.phase = Phase::Waking { reads: 0, ok: 0 };
            self.next = Some(now + WAKE_WAIT);
            self.log.push("wake, checking the brightness");
        } else {
            self.phase = Phase::Awake;
            self.log.push(&format!("wake, restore {}", if enabled { "has nothing remembered" } else { "is off" }));
        }
    }

    /// A brightness read finished (None = it failed or was junk). Returns the write to make, if any.
    pub fn observe(&mut self, now: Instant, read: Option<u32>, enabled: bool) -> Act {
        if !enabled && matches!(self.phase, Phase::Waking { .. } | Phase::Restoring { .. }) {
            self.log.push("restore stopped: setting off");
            (self.phase, self.next) = (Phase::Awake, None);
        }
        match self.phase {
            Phase::Asleep => Act::None,
            Phase::Awake => {
                if self.last_logged_read != Some(read) {
                    self.log.push(&format!("read {read:?}"));
                    self.last_logged_read = Some(read);
                }
                if let Some(r) = read {
                    self.track(now, r);
                }
                Act::None
            }
            Phase::Waking { reads, ok } => {
                let reads = reads + 1;
                self.log.push(&format!("wake read {reads}: {read:?}, remembered {:?}", self.pre_sleep));
                let pre = self.pre_sleep.unwrap_or(0);
                match read {
                    Some(r) if r.abs_diff(pre) > TOLERANCE => {
                        self.log.push(&format!("restore: write {pre} (display shows {r})"));
                        self.phase = Phase::Restoring { target: pre, before: r, writes: 1, reads: 0 };
                        self.next = Some(now + PROBE_GAP);
                        return Act::Write(pre);
                    }
                    Some(_) if ok + 1 >= 2 => return self.settle("brightness unchanged after the wake"),
                    Some(_) => self.phase = Phase::Waking { reads, ok: ok + 1 },
                    None if reads >= MAX_READS => return self.settle("no usable read after the wake: nothing written"),
                    None => self.phase = Phase::Waking { reads, ok },
                }
                if reads >= MAX_READS {
                    return self.settle("brightness unchanged after the wake");
                }
                self.next = Some(now + PROBE_GAP);
                Act::None
            }
            Phase::Restoring { target, before, writes, reads } => {
                let reads = reads + 1;
                self.log.push(&format!("verify read: {read:?} (target {target})"));
                match read {
                    Some(r) if r.abs_diff(target) <= TOLERANCE => {
                        self.last_good = Some(target);
                        self.settle("restored")
                    }
                    Some(r) if r != before => {
                        // somebody else moved it after our write: theirs is the truth
                        self.stable = Some((r, now));
                        self.settle("changed again after the restore: respected")
                    }
                    Some(_) if writes < MAX_WRITES => {
                        self.log.push(&format!("restore: write {target} again ({}/{MAX_WRITES})", writes + 1));
                        self.phase = Phase::Restoring { target, before, writes: writes + 1, reads };
                        self.next = Some(now + PROBE_GAP);
                        Act::Write(target)
                    }
                    Some(_) => self.settle("restore did not stick after 3 writes: giving up"),
                    None if reads >= MAX_READS => self.settle("verify reads failing: giving up"),
                    None => {
                        self.phase = Phase::Restoring { target, before, writes, reads };
                        self.next = Some(now + PROBE_GAP);
                        Act::None
                    }
                }
            }
        }
    }

    fn settle(&mut self, why: &str) -> Act {
        self.log.push(why);
        (self.phase, self.next) = (Phase::Awake, None);
        Act::None
    }

    /// Awake read: the first one seeds the memory, a value that repeats `STABLE_GAP` later replaces it (changes made with the monitor's
    /// buttons, the Fn keys or Windows while awake are accepted as the new truth).
    fn track(&mut self, now: Instant, r: u32) {
        if self.last_good.is_none() {
            self.last_good = Some(r);
            return;
        }
        match self.stable {
            Some((v, t0)) if v == r => {
                if now.saturating_duration_since(t0) >= STABLE_GAP && self.last_good != Some(r) {
                    self.log.push(&format!("brightness changed outside the widget: {r} is the new value"));
                    self.last_good = Some(r);
                }
            }
            _ => self.stable = Some((r, now)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k() -> (Keeper, Instant) {
        (Keeper::new(Log::mem()), Instant::now())
    }
    fn s(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    /// user has 26, the display sleeps and wakes, `reads` is what the next probes return
    fn sleep_wake(kp: &mut Keeper, t0: Instant) -> Instant {
        kp.observe(t0, Some(26), true);
        kp.sleep();
        let t = t0 + s(600);
        kp.wake(t, true);
        assert_eq!(kp.next_probe(), Some(t + WAKE_WAIT));
        t + WAKE_WAIT
    }

    #[test]
    fn jump_to_100_after_wake_is_restored_once() {
        let (mut kp, t0) = k();
        let t = sleep_wake(&mut kp, t0);
        assert_eq!(kp.observe(t, Some(100), true), Act::Write(26));
        // the verify read finds it back at 26: done, no second write
        assert_eq!(kp.observe(t + s(1), Some(26), true), Act::None);
        assert_eq!(kp.next_probe(), None);
        assert_eq!(kp.last_good(), Some(26));
        assert_eq!(kp.observe(t + s(2), Some(26), true), Act::None);
        assert!(kp.log.lines.iter().any(|l| l.ends_with("restored")));
    }

    #[test]
    fn unchanged_brightness_is_never_written() {
        let (mut kp, t0) = k();
        let t = sleep_wake(&mut kp, t0);
        assert_eq!(kp.observe(t, Some(27), true), Act::None, "within 2 points");
        assert_eq!(kp.next_probe(), Some(t + PROBE_GAP));
        assert_eq!(kp.observe(t + s(1), Some(26), true), Act::None);
        assert_eq!(kp.next_probe(), None, "two agreeing reads end the check");
    }

    #[test]
    fn a_gesture_after_wake_stands_the_restore_down() {
        let (mut kp, t0) = k();
        let t = sleep_wake(&mut kp, t0);
        kp.gesture(60, t + Duration::from_millis(500));
        assert_eq!(kp.observe(t, Some(100), true), Act::None, "the user already chose");
        assert_eq!(kp.last_good(), Some(60));
        // a gesture between the restore write and its verification also ends it
        let (mut kp, t0) = k();
        let t = sleep_wake(&mut kp, t0);
        assert_eq!(kp.observe(t, Some(100), true), Act::Write(26));
        kp.gesture(80, t + Duration::from_millis(300));
        assert_eq!(kp.observe(t + s(1), Some(80), true), Act::None);
        assert_eq!(kp.next_probe(), None);
        assert_eq!(kp.last_good(), Some(80));
    }

    #[test]
    fn a_stable_change_made_while_awake_becomes_the_new_truth() {
        let (mut kp, t0) = k();
        kp.observe(t0, Some(26), true);
        kp.observe(t0 + s(20), Some(70), true); // buttons on the monitor
        assert_eq!(kp.last_good(), Some(26), "one read is not enough");
        kp.observe(t0 + s(22), Some(70), true);
        assert_eq!(kp.last_good(), Some(26), "two reads under 5 s apart are not enough");
        kp.observe(t0 + s(26), Some(70), true);
        assert_eq!(kp.last_good(), Some(70));
        kp.sleep();
        kp.wake(t0 + s(100), true);
        assert_eq!(kp.observe(t0 + s(102), Some(100), true), Act::Write(70));
    }

    #[test]
    fn a_flickering_read_never_becomes_the_truth() {
        let (mut kp, t0) = k();
        kp.observe(t0, Some(26), true);
        for (i, v) in [100, 26, 100, 26, 100].into_iter().enumerate() {
            kp.observe(t0 + s(10 * (i as u64 + 1)), Some(v), true);
        }
        assert_eq!(kp.last_good(), Some(26));
    }

    #[test]
    fn failed_reads_never_write() {
        let (mut kp, t0) = k();
        let mut t = sleep_wake(&mut kp, t0);
        for _ in 0..5 {
            assert_eq!(kp.observe(t, None, true), Act::None);
            t += s(1);
        }
        assert_eq!(kp.next_probe(), None, "five failures end the check");
        assert_eq!(kp.last_good(), Some(26));
        // failing verification reads after a restore write: no further write either
        let (mut kp, t0) = k();
        let t = sleep_wake(&mut kp, t0);
        assert_eq!(kp.observe(t, Some(100), true), Act::Write(26));
        for i in 1..=5 {
            assert_eq!(kp.observe(t + s(i), None, true), Act::None);
        }
        assert_eq!(kp.next_probe(), None);
    }

    #[test]
    fn a_restore_that_does_not_stick_stops_after_three_writes() {
        let (mut kp, t0) = k();
        let t = sleep_wake(&mut kp, t0);
        let mut writes = 0;
        let mut act = kp.observe(t, Some(100), true);
        for i in 1..=10 {
            if let Act::Write(26) = act {
                writes += 1;
            }
            act = kp.observe(t + s(i), Some(100), true);
        }
        assert_eq!(writes, 3);
        assert_eq!(kp.next_probe(), None);
        assert!(kp.log.lines.iter().any(|l| l.contains("giving up")));
    }

    #[test]
    fn a_change_after_the_restore_is_respected() {
        let (mut kp, t0) = k();
        let t = sleep_wake(&mut kp, t0);
        assert_eq!(kp.observe(t, Some(100), true), Act::Write(26));
        assert_eq!(kp.observe(t + s(1), Some(55), true), Act::None, "not 100 any more, not 26: somebody else moved it");
        assert_eq!(kp.next_probe(), None);
        kp.observe(t + s(7), Some(55), true);
        assert_eq!(kp.last_good(), Some(55));
    }

    #[test]
    fn setting_off_restores_nothing() {
        let (mut kp, t0) = k();
        kp.observe(t0, Some(26), true);
        kp.sleep();
        kp.wake(t0 + s(60), false);
        assert_eq!(kp.next_probe(), None);
        assert_eq!(kp.observe(t0 + s(62), Some(100), false), Act::None);
        // switched off in the middle of a check
        let (mut kp, t0) = k();
        let t = sleep_wake(&mut kp, t0);
        assert_eq!(kp.observe(t, Some(100), false), Act::None);
        let (mut kp, t0) = k();
        let t = sleep_wake(&mut kp, t0);
        assert_eq!(kp.observe(t, Some(100), true), Act::Write(26));
        assert_eq!(kp.observe(t + s(1), Some(100), false), Act::None, "no second write once it is off");
    }

    #[test]
    fn nothing_remembered_nothing_restored() {
        let (mut kp, t0) = k();
        kp.sleep();
        kp.wake(t0, true);
        assert_eq!(kp.next_probe(), None);
        assert_eq!(kp.observe(t0 + s(2), Some(100), true), Act::None);
        assert_eq!(kp.last_good(), Some(100), "the first awake read seeds the memory");
    }

    #[test]
    fn the_log_is_a_ring_of_200_lines() {
        let mut l = Log::mem();
        for i in 0..250 {
            l.push(&format!("e{i}"));
        }
        assert_eq!(l.lines.len(), 200);
        assert!(l.lines.front().unwrap().ends_with("e50") && l.lines.back().unwrap().ends_with("e249"));
        // the file mirror is cut to the same size
        let dir = std::env::temp_dir().join(format!("bri-keeper-test-{}", std::process::id()));
        let p = dir.join("brightness.log");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&p, (0..500).map(|i| format!("old{i}\n")).collect::<String>()).unwrap();
        let mut l = Log { path: Some(p.clone()), lines: VecDeque::new(), since_trim: usize::MAX };
        l.push("fresh");
        let text = std::fs::read_to_string(&p).unwrap();
        assert_eq!(text.lines().count(), 200);
        assert!(text.lines().last().unwrap().ends_with("fresh"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
