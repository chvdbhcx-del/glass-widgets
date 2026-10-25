//! Timer: three modes in one card - Timer, Stopwatch, Pomodoro (25 / 5).
//! S = a ring with the time in the middle and a play/pause button; M = ring + presets (1, 5, 10, 25 min, custom) + start/pause/reset;
//! L = segmented mode tabs + ring + presets (or the Pomodoro cycle dots) + start/pause/reset.
//! Settings (`cx.cfg`, all unix ms / seconds): `mode` "timer"|"stopwatch"|"pomodoro", `running`, `end_ms` (timer/pomodoro: when the phase
//! ends), `start_ms` (stopwatch: virtual start), `paused_ms` (timer/pomodoro: remaining, stopwatch: elapsed), `dur_s` (timer length),
//! `brk` (pomodoro: break phase), `cycle` (finished work phases), `finished` ("Готово" until a click). Wall-clock based, so it survives restarts.
//! Interaction: play/pause, reset, presets, custom length (the "…" pill or a click on the digits in S; typed "90" = 90 min, "1:30" = 1 min 30 s,
//! "1:02:03", "25m", "90s", "2h", "1h30m"), click the small mode name inside the ring (S, M) or a tab (L) to switch modes, right click = reset,
//! any click while "Готово" is shown clears it. At the end: one system beep, the card flashes green for 3 s, the ring turns green.
//! Performance: the idle / paused state is fully static; only a running timer paints the overlay (ring arc + digits): 1 frame per
//! second for long timers (aligned to the digit flip), 10 fps for short ones and the stopwatch, 30 fps during the 3 s finish flash.
use super::reminders::now_ms;
use crate::registry::{Registry, WidgetDef};
use crate::widget::*;
use std::time::Instant;
use windows::Win32::System::Diagnostics::Debug::MessageBeep;
use windows::Win32::UI::WindowsAndMessaging::MB_ICONASTERISK;

pub fn register(r: &mut Registry) {
    r.register(WidgetDef {
        type_id: "timer",
        name: ("Таймер", "Timer"),
        desc: ("Таймер, секундомер и обратный отсчёт", "Timer, stopwatch and countdown"),
        group: "clock",
        icon: Glyph::Clock,
        sizes: &[Size::S, Size::M, Size::L],
        default_size: Size::S,
        make: || Box::new(TimerW::default()),
    });
}

// ================================================================ pure state

const WORK_S: i64 = 25 * 60;
const BREAK_S: i64 = 5 * 60;
const PRESETS: [i64; 4] = [1, 5, 10, 25];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    Timer,
    Stopwatch,
    Pomodoro,
}
impl Mode {
    const ALL: [Mode; 3] = [Mode::Timer, Mode::Stopwatch, Mode::Pomodoro];
    fn key(self) -> &'static str {
        match self {
            Mode::Timer => "timer",
            Mode::Stopwatch => "stopwatch",
            Mode::Pomodoro => "pomodoro",
        }
    }
    fn idx(self) -> usize {
        Mode::ALL.iter().position(|m| *m == self).unwrap_or(0)
    }
}

#[derive(Clone, Debug, PartialEq)]
struct T {
    mode: Mode,
    running: bool,
    end_ms: i64,
    start_ms: i64,
    /// timer / pomodoro: remaining ms while not running; stopwatch: elapsed ms while not running
    paused_ms: i64,
    dur_s: i64,
    brk: bool,
    cycle: u32,
    finished: bool,
}

impl T {
    fn idle(mode: Mode, dur_s: i64) -> T {
        let mut t = T { mode, running: false, end_ms: 0, start_ms: 0, paused_ms: 0, dur_s, brk: false, cycle: 0, finished: false };
        t.paused_ms = t.phase_ms();
        t
    }
    /// full length of the current phase (0 for the stopwatch)
    fn phase_ms(&self) -> i64 {
        1000 * match self.mode {
            Mode::Timer => self.dur_s,
            Mode::Stopwatch => 0,
            Mode::Pomodoro => {
                if self.brk {
                    BREAK_S
                } else {
                    WORK_S
                }
            }
        }
    }
    fn remaining(&self, now: i64) -> i64 {
        if self.running {
            (self.end_ms - now).max(0)
        } else {
            self.paused_ms
        }
    }
    fn elapsed(&self, now: i64) -> i64 {
        if self.running {
            (now - self.start_ms).max(0)
        } else {
            self.paused_ms
        }
    }
    /// the number shown in the middle (ms): remaining rounded UP to a whole second, or the stopwatch's elapsed time
    fn shown_ms(&self, now: i64) -> i64 {
        match self.mode {
            Mode::Stopwatch => self.elapsed(now),
            _ => (self.remaining(now) + 999) / 1000 * 1000,
        }
    }
    /// ring fill 0..1: the remaining share of the phase; the stopwatch ring is one lap per minute
    fn frac(&self, now: i64) -> f32 {
        if self.finished {
            return 1.0;
        }
        match self.mode {
            Mode::Stopwatch => (self.elapsed(now) % 60_000) as f32 / 60_000.0,
            _ => (self.remaining(now) as f32 / self.phase_ms().max(1) as f32).clamp(0.0, 1.0),
        }
    }
    fn start(&mut self, now: i64) {
        if self.running {
            return;
        }
        self.finished = false;
        match self.mode {
            Mode::Stopwatch => self.start_ms = now - self.paused_ms,
            _ => {
                if self.paused_ms <= 0 {
                    self.paused_ms = self.phase_ms();
                }
                self.end_ms = now + self.paused_ms;
            }
        }
        self.running = true;
    }
    fn pause(&mut self, now: i64) {
        if !self.running {
            return;
        }
        self.paused_ms = if self.mode == Mode::Stopwatch { self.elapsed(now) } else { self.remaining(now) };
        self.running = false;
    }
    fn toggle(&mut self, now: i64) {
        if self.running {
            self.pause(now)
        } else {
            self.start(now)
        }
    }
    /// nothing to reset: stopped and untouched
    fn is_pristine(&self) -> bool {
        !self.running && !self.finished && self.paused_ms == self.phase_ms() && (self.mode != Mode::Pomodoro || (self.cycle == 0 && !self.brk))
    }
    /// reset the phase; a second reset on an untouched Pomodoro restarts the whole cycle
    fn reset(&mut self) {
        let was_full = !self.running && self.paused_ms == self.phase_ms();
        self.running = false;
        self.finished = false;
        if self.mode == Mode::Pomodoro && was_full {
            self.brk = false;
            self.cycle = 0;
        }
        self.paused_ms = self.phase_ms();
    }
    fn set_mode(&mut self, m: Mode) {
        if m != self.mode {
            *self = T::idle(m, self.dur_s);
        }
    }
    fn set_duration(&mut self, secs: i64) {
        self.dur_s = secs.clamp(1, 86_400);
        self.mode = Mode::Timer;
        self.running = false;
        self.finished = false;
        self.paused_ms = self.phase_ms();
    }
    /// A running timer / pomodoro phase that has reached its end: back to idle (next phase for the Pomodoro), `finished` set.
    fn check_finish(&mut self, now: i64) -> bool {
        if !self.running || self.mode == Mode::Stopwatch || now < self.end_ms {
            return false;
        }
        if self.mode == Mode::Pomodoro {
            if !self.brk {
                self.cycle += 1;
            }
            self.brk = !self.brk;
        }
        self.running = false;
        self.finished = true;
        self.paused_ms = self.phase_ms();
        true
    }
    /// filled dots of the 4-dot Pomodoro cycle
    fn dots(&self) -> u32 {
        if self.brk && self.cycle > 0 && self.cycle % 4 == 0 {
            4
        } else {
            self.cycle % 4
        }
    }
    fn to_pairs(&self) -> Vec<(&'static str, Value)> {
        vec![
            ("mode", json!(self.mode.key())),
            ("running", json!(self.running)),
            ("end_ms", json!(self.end_ms)),
            ("start_ms", json!(self.start_ms)),
            ("paused_ms", json!(self.paused_ms)),
            ("dur_s", json!(self.dur_s)),
            ("brk", json!(self.brk)),
            ("cycle", json!(self.cycle)),
            ("finished", json!(self.finished)),
        ]
    }
    fn from_value(v: &Value) -> T {
        let mode = match v.get("mode").and_then(Value::as_str) {
            Some("stopwatch") => Mode::Stopwatch,
            Some("pomodoro") => Mode::Pomodoro,
            _ => Mode::Timer,
        };
        let num = |k: &str| v.get(k).and_then(Value::as_f64).map(|x| x as i64);
        let flag = |k: &str| v.get(k).and_then(Value::as_bool).unwrap_or(false);
        let mut t = T::idle(mode, num("dur_s").filter(|d| (1..=86_400).contains(d)).unwrap_or(300));
        t.brk = mode == Mode::Pomodoro && flag("brk");
        t.cycle = num("cycle").unwrap_or(0).clamp(0, 10_000) as u32;
        t.finished = flag("finished");
        t.paused_ms = num("paused_ms").unwrap_or_else(|| t.phase_ms()).clamp(0, if mode == Mode::Stopwatch { 100 * 3_600_000 } else { t.phase_ms() });
        t.running = flag("running");
        t.end_ms = num("end_ms").unwrap_or(0);
        t.start_ms = num("start_ms").unwrap_or(0);
        if t.running && ((mode != Mode::Stopwatch && t.end_ms <= 0) || (mode == Mode::Stopwatch && t.start_ms <= 0)) {
            t.running = false;
        }
        t
    }
}

/// "H:MM:SS" / "MM:SS" (+ ".t" tenths for the stopwatch under an hour).
fn fmt_clock(ms: i64, tenths: bool) -> String {
    let ms = ms.max(0);
    let s = ms / 1000;
    let (h, m, sec) = (s / 3600, s / 60 % 60, s % 60);
    if h > 0 {
        format!("{h}:{m:02}:{sec:02}")
    } else if tenths {
        format!("{m:02}:{sec:02}.{}", ms % 1000 / 100)
    } else {
        format!("{m:02}:{sec:02}")
    }
}

/// Custom length -> seconds (1..=86400). "90" = minutes, "1:30" = min:sec, "1:02:03" = h:m:s, units m/м/min/мин, s/с/sec/сек, h/ч, compound "1h30m".
fn parse_duration(input: &str) -> Option<i64> {
    let s = input.trim().to_lowercase().replace(',', ".");
    let ok = |secs: f64| (1.0..=86_400.0).contains(&secs.round()).then_some(secs.round() as i64);
    if s.contains(':') {
        let p: Vec<u32> = s.split(':').map(|x| x.trim().parse().ok()).collect::<Option<_>>()?;
        return match p.as_slice() {
            [m, sec] if *sec < 60 => ok((m * 60 + sec) as f64),
            [h, m, sec] if *m < 60 && *sec < 60 => ok((h * 3600 + m * 60 + sec) as f64),
            _ => None,
        };
    }
    let (mut total, mut num, mut unit, mut any) = (0.0f64, String::new(), String::new(), false);
    let mut flush = |num: &mut String, unit: &mut String| -> Option<()> {
        if num.is_empty() {
            return if unit.is_empty() { Some(()) } else { None };
        }
        let v: f64 = num.parse().ok()?;
        let k = match unit.as_str() {
            "" | "m" | "м" | "min" | "мин" => 60.0,
            "s" | "с" | "sec" | "сек" => 1.0,
            "h" | "ч" | "hr" | "час" => 3600.0,
            _ => return None,
        };
        total += v * k;
        num.clear();
        unit.clear();
        any = true;
        Some(())
    };
    for c in s.chars() {
        if c.is_ascii_digit() || c == '.' {
            if !unit.is_empty() {
                flush(&mut num, &mut unit)?;
            }
            num.push(c);
        } else if c.is_whitespace() {
            continue;
        } else if c.is_alphabetic() {
            unit.push(c);
        } else {
            return None;
        }
    }
    flush(&mut num, &mut unit)?;
    if any {
        ok(total)
    } else {
        None
    }
}

// ================================================================ geometry

struct Hits {
    c: Pt,
    r: f32,
    stroke: f32,
    /// the mode name inside the ring (S, M: click = next mode)
    label: Rect,
    digits: Rect,
    /// font size of the digits (<= 5 chars, longer)
    font: (f32, f32),
    /// L: the line under the digits
    sub: Rect,
    play: Rect,
    reset: Rect,
    tabs: [Rect; 3],
    presets: [Rect; 5],
    /// the row of presets (Timer) / the cycle dots (Pomodoro) / a hint (Stopwatch)
    extra: Rect,
}

fn hits(size: Size, c: Rect) -> Hits {
    let z = Rect::default();
    match size {
        Size::S => {
            let ctr = (c.cx(), c.cy());
            Hits {
                c: ctr,
                r: 60.0,
                stroke: 6.0,
                label: Rect::new(ctr.0 - 38.0, ctr.1 - 38.0, 76.0, 18.0),
                digits: Rect::new(ctr.0 - 56.0, ctr.1 - 22.0, 112.0, 36.0),
                font: (29.0, 21.0),
                sub: z,
                play: Rect::new(ctr.0 - 15.0, ctr.1 + 19.0, 30.0, 30.0),
                reset: z,
                tabs: [z; 3],
                presets: [z; 5],
                extra: z,
            }
        }
        Size::M => {
            let ctr = (c.x + 62.0, c.cy());
            let x0 = c.x + 142.0;
            let w = c.right() - x0;
            let pw = (w - 4.0 * 6.0) / 5.0;
            let mut presets = [z; 5];
            for (i, p) in presets.iter_mut().enumerate() {
                *p = Rect::new(x0 + i as f32 * (pw + 6.0), c.y + 4.0, pw, 28.0);
            }
            Hits {
                c: ctr,
                r: 56.0,
                stroke: 6.0,
                label: Rect::new(ctr.0 - 38.0, ctr.1 - 34.0, 76.0, 18.0),
                digits: Rect::new(ctr.0 - 52.0, ctr.1 - 19.0, 104.0, 38.0),
                font: (27.0, 20.0),
                sub: z,
                play: Rect::new(x0 + 52.0, c.bottom() - 38.0, w - 52.0, 38.0),
                reset: Rect::new(x0, c.bottom() - 38.0, 44.0, 38.0),
                tabs: [z; 3],
                presets,
                extra: Rect::new(x0, c.y + 44.0, w, 40.0),
            }
        }
        Size::L => {
            let ctr = (c.cx(), c.y + 46.0 + 84.5);
            let tw = c.w / 3.0;
            let pw = (c.w - 4.0 * 8.0) / 5.0;
            let mut presets = [z; 5];
            for (i, p) in presets.iter_mut().enumerate() {
                *p = Rect::new(c.x + i as f32 * (pw + 8.0), 244.0, pw, 28.0);
            }
            Hits {
                c: ctr,
                r: 80.0,
                stroke: 9.0,
                label: Rect::new(ctr.0 - 60.0, ctr.1 - 50.0, 120.0, 18.0),
                digits: Rect::new(ctr.0 - 76.0, ctr.1 - 30.0, 152.0, 58.0),
                font: (48.0, 35.0),
                sub: Rect::new(ctr.0 - 70.0, ctr.1 + 30.0, 140.0, 18.0),
                play: Rect::new(c.x + 112.0, c.bottom() - 42.0, c.w - 112.0, 42.0),
                reset: Rect::new(c.x, c.bottom() - 42.0, 100.0, 42.0),
                tabs: [Rect::new(c.x, c.y, tw, 30.0), Rect::new(c.x + tw, c.y, tw, 30.0), Rect::new(c.x + 2.0 * tw, c.y, tw, 30.0)],
                presets,
                extra: Rect::new(c.x, 244.0, c.w, 28.0),
            }
        }
    }
}

// ================================================================ widget

const FLASH_S: f32 = 3.0;

#[derive(Default)]
struct TimerW {
    t: Option<T>,
    started: bool,
    ver: u64,
    flash: Option<Instant>,
    /// state changed in tick() (which cannot write cfg): persisted on the next mouse event
    dirty: bool,
    ask: Option<Pending<Option<String>>>,
}

fn sample(size: Size, now: i64) -> T {
    let mut t = T::idle(if size == Size::L { Mode::Pomodoro } else { Mode::Timer }, 1500);
    if size == Size::L {
        t.cycle = 2;
        t.running = true;
        t.end_ms = now + (14 * 60 + 23) * 1000 - 400;
    } else {
        t.running = true;
        t.end_ms = now + (17 * 60 + 42) * 1000 - 400;
    }
    t
}

fn ring_color(t: &T) -> Color {
    if t.finished {
        return GREEN;
    }
    match t.mode {
        Mode::Timer => ORANGE,
        Mode::Stopwatch => TEAL,
        Mode::Pomodoro => {
            if t.brk {
                GREEN
            } else {
                RED
            }
        }
    }
}

impl TimerW {
    fn t(&self) -> &T {
        static IDLE: std::sync::OnceLock<T> = std::sync::OnceLock::new();
        self.t.as_ref().unwrap_or_else(|| IDLE.get_or_init(|| T::idle(Mode::Timer, 300)))
    }

    fn init(&mut self, cx: &Cx) {
        if self.started {
            return;
        }
        self.started = true;
        self.ver += 1;
        let now = now_ms(cx);
        if cx.preview {
            self.t = Some(sample(cx.size, now));
        } else {
            let mut t = T::from_value(cx.cfg);
            t.check_finish(now); // ended while the app was not running: shown as "Готово", no beep
            self.t = Some(t);
        }
    }

    fn persist(&mut self, cx: &mut Cx) {
        for (k, v) in self.t().to_pairs() {
            cx.cfg_set(k, v);
        }
        self.dirty = false;
    }

    fn label(&self, cx: &Cx) -> &'static str {
        let t = self.t();
        let ru = cx.lang == Lang::Ru;
        match (t.mode, t.brk) {
            (Mode::Timer, _) => {
                if ru {
                    "Таймер"
                } else {
                    "Timer"
                }
            }
            (Mode::Stopwatch, _) => {
                if ru {
                    "Секундомер"
                } else {
                    "Stopwatch"
                }
            }
            (Mode::Pomodoro, false) => {
                if ru {
                    "Работа"
                } else {
                    "Focus"
                }
            }
            (Mode::Pomodoro, true) => {
                if ru {
                    "Перерыв"
                } else {
                    "Break"
                }
            }
        }
    }

    fn digits_text(&self, cx: &Cx, now: i64) -> (String, Color) {
        let t = self.t();
        if t.finished {
            return (cx.t("Готово", "Done").to_string(), GREEN);
        }
        (fmt_clock(t.shown_ms(now), t.mode == Mode::Stopwatch), TEXT)
    }

    fn draw_digits(&self, cx: &Cx, h: &Hits, now: i64) {
        let (s, col) = self.digits_text(cx, now);
        let size = if s.chars().count() <= 5 { h.font.0 } else { h.font.1 };
        cx.text(h.digits, &s, Style::new(size, 300).mono().center(), col);
    }

    fn draw_arc(&self, cx: &Cx, h: &Hits, now: i64) {
        let t = self.t();
        let f = t.frac(now);
        if f > 0.0 {
            cx.arc(h.c, h.r, h.stroke, 0.0, 360.0 * f, ring_color(t));
        }
    }

    fn live(&self) -> bool {
        let t = self.t();
        t.running && !t.finished
    }
}

fn pill_button(cx: &Cx, r: Rect, label: &str, glyph: Glyph, fg: Color, bg: Color) {
    cx.pill(r, bg);
    let ic = (r.h * 0.42).min(18.0);
    let w = cx.measure(label, Style::TITLE, 0.0).0;
    let total = ic + 6.0 + w;
    let x = r.cx() - total / 2.0;
    cx.icon(glyph, Rect::new(x, r.cy() - ic / 2.0, ic, ic), fg);
    cx.text(Rect::new(x + ic + 6.0, r.y, w + 4.0, r.h), label, Style::TITLE, fg);
}

impl Widget for TimerW {
    fn draw(&mut self, cx: &mut Cx) -> Result<()> {
        self.init(cx);
        let c = cx.content();
        let h = hits(cx.size, c);
        let now = now_ms(cx);
        let (t, live) = (self.t(), self.live());
        // ring track, then (when nothing animates) the static arc and digits
        cx.circle_stroke(h.c, h.r, h.stroke, FILL);
        if !live {
            self.draw_arc(cx, &h, now);
            self.draw_digits(cx, &h, now);
        }
        let name = self.label(cx);
        match cx.size {
            Size::S | Size::M => {
                cx.text(h.label, name, Style::CAPTION.center().ellipsis(), DIM);
            }
            Size::L => {
                cx.text(h.label, name, Style::CAPTION.center().ellipsis(), DIM);
                // tabs
                cx.pill(Rect::new(c.x, c.y, c.w, 30.0), FILL.fade(0.7));
                let names = [cx.t("Таймер", "Timer"), cx.t("Секундомер", "Stopwatch"), cx.t("Помодоро", "Pomodoro")];
                for (i, r) in h.tabs.iter().enumerate() {
                    let on = t.mode.idx() == i;
                    if on {
                        cx.pill(r.inset(2.0), WHITE.alpha(0.92));
                    }
                    cx.text(*r, names[i], Style::new(12.0, 600).center().ellipsis(), if on { INK } else { DIM });
                }
                let sub = match t.mode {
                    Mode::Timer => format!("{} {}", cx.t("из", "of"), fmt_clock(t.phase_ms(), false)),
                    Mode::Stopwatch => cx.t("круг = 1 минута", "lap = 1 minute").to_string(),
                    Mode::Pomodoro => format!("{} {}", cx.t("раунд", "round"), t.cycle + if t.brk { 0 } else { 1 }),
                };
                cx.text(h.sub, &sub, Style::CAPTION.center(), FAINT);
            }
        }
        // presets / cycle dots / hint
        if cx.size != Size::S {
            match t.mode {
                Mode::Timer => {
                    let labels: [String; 5] = [
                        if cx.size == Size::L { format!("{} {}", PRESETS[0], cx.t("мин", "min")) } else { PRESETS[0].to_string() },
                        if cx.size == Size::L { format!("{} {}", PRESETS[1], cx.t("мин", "min")) } else { PRESETS[1].to_string() },
                        if cx.size == Size::L { format!("{} {}", PRESETS[2], cx.t("мин", "min")) } else { PRESETS[2].to_string() },
                        if cx.size == Size::L { format!("{} {}", PRESETS[3], cx.t("мин", "min")) } else { PRESETS[3].to_string() },
                        if cx.size == Size::L { cx.t("Своё", "Custom").to_string() } else { "…".to_string() },
                    ];
                    for (i, r) in h.presets.iter().enumerate() {
                        let on = i < 4 && t.dur_s == PRESETS[i] * 60;
                        cx.pill(*r, if on { ORANGE } else { FILL });
                        cx.text(*r, &labels[i], Style::new(12.0, 600).center().ellipsis(), if on { INK } else { TEXT });
                    }
                    if cx.size == Size::M {
                        cx.text(Rect::new(h.extra.x, h.extra.y, h.extra.w, 16.0), cx.t("минуты", "minutes"), Style::CAPTION, FAINT);
                    }
                }
                Mode::Pomodoro => {
                    let e = h.extra;
                    let (n, gap) = (4, 18.0);
                    let x0 = if cx.size == Size::L { e.cx() - 1.5 * gap } else { e.x + 6.0 };
                    let cy = if cx.size == Size::L { e.cy() } else { e.y + 8.0 };
                    for i in 0..n {
                        let p = (x0 + i as f32 * gap, cy);
                        if (i as u32) < t.dots() {
                            cx.circle(p, 5.0, ring_color(t).alpha(1.0));
                        } else {
                            cx.circle_stroke(p, 4.5, 1.4, FAINT);
                        }
                    }
                    if cx.size == Size::M {
                        cx.text(Rect::new(e.x, e.y + 20.0, e.w, 18.0), cx.t("25 мин работа · 5 отдых", "25 focus · 5 break"), Style::CAPTION, DIM);
                    }
                }
                Mode::Stopwatch => {
                    if cx.size == Size::M {
                        cx.text(Rect::new(h.extra.x, h.extra.y - 36.0, h.extra.w, 40.0), cx.t("Кольцо — один оборот в минуту", "The ring is one lap per minute"), Style::CAPTION.wrap(), DIM);
                    }
                }
            }
        }
        // buttons
        let (running, fin) = (t.running, t.finished);
        if cx.size == Size::S {
            cx.circle(h.play.center(), 14.0, FILL);
            let g = if fin { Glyph::Restart } else if running { Glyph::Pause } else { Glyph::Play };
            cx.icon(g, h.play.inset(6.0), TEXT);
        } else {
            let (label, glyph, col) = if fin {
                (cx.t("Готово", "Done"), Glyph::Check, GREEN)
            } else if running {
                (cx.t("Пауза", "Pause"), Glyph::Pause, ORANGE)
            } else {
                (cx.t("Старт", "Start"), Glyph::Play, GREEN)
            };
            pill_button(cx, h.play, label, glyph, INK, col); // solid: legible on any backdrop
            if cx.size == Size::L {
                pill_button(cx, h.reset, cx.t("Сброс", "Reset"), Glyph::Restart, if t.is_pristine() { FAINT } else { TEXT }, FILL);
            } else {
                cx.pill(h.reset, FILL);
                cx.icon(Glyph::Restart, h.reset.centered(18.0, 18.0), if t.is_pristine() { FAINT } else { TEXT });
            }
        }
        Ok(())
    }

    fn overlay(&mut self, cx: &mut Cx) -> Result<()> {
        let h = hits(cx.size, cx.content());
        if self.live() {
            let now = now_ms(cx);
            self.draw_arc(cx, &h, now);
            self.draw_digits(cx, &h, now);
        }
        if let Some(t0) = self.flash {
            let t = t0.elapsed().as_secs_f32();
            if t < FLASH_S {
                let k = ((t * std::f32::consts::TAU * 1.5).sin() * 0.5 + 0.5) * (1.0 - t / FLASH_S);
                cx.rrect(cx.bounds().inset(1.0), cx.size.radius() - 1.0, GREEN.alpha(0.42 * k));
            }
        }
        Ok(())
    }

    fn tick(&mut self, cx: &Cx) -> Tick {
        self.init(cx);
        let now = now_ms(cx);
        if !cx.preview {
            if let Some(t) = self.t.as_mut() {
                if t.check_finish(now) {
                    unsafe {
                        let _ = MessageBeep(MB_ICONASTERISK);
                    }
                    self.flash = Some(Instant::now());
                    self.dirty = true;
                    self.ver += 1;
                }
            }
            if let Some(res) = self.ask.as_ref().map(|p| (p.take(), p.busy())) {
                match res {
                    (Some(r), _) => {
                        self.ask = None;
                        if let Some(secs) = r.as_deref().and_then(parse_duration) {
                            if let Some(t) = self.t.as_mut() {
                                t.set_duration(secs);
                            }
                            self.dirty = true;
                            self.ver += 1;
                        }
                    }
                    (None, false) => self.ask = None,
                    _ => {}
                }
            }
            if self.flash.is_some_and(|f| f.elapsed().as_secs_f32() >= FLASH_S) {
                self.flash = None;
                self.ver += 1; // one last re-record so the flash is wiped
            }
        }
        if self.flash.is_some() {
            return Tick::Fps(30);
        }
        let t = self.t();
        if cx.preview || !t.running {
            return Tick::Never;
        }
        if t.mode == Mode::Stopwatch || t.phase_ms() <= 180_000 {
            return Tick::Fps(10);
        }
        // long timers: one frame per second, aligned to the moment the shown second flips
        let r = t.remaining(now);
        if r <= 0 {
            return Tick::Fps(10);
        }
        Tick::Every(Duration::from_millis(((r - 1).rem_euclid(1000) + 1 + 8) as u64))
    }

    fn mouse(&mut self, ev: &MouseEv, cx: &mut Cx) -> bool {
        if cx.preview {
            return false;
        }
        if self.dirty {
            self.persist(cx); // state changed by tick(): the first mouse event after it saves it
        }
        if ev.kind != MouseKind::Up || !matches!(ev.button, Button::Left | Button::Right) || !ev.inside(cx.bounds()) {
            return false;
        }
        let (c, now) = (cx.content(), now_ms(cx));
        let h = hits(cx.size, c);
        let mut changed = true;
        if self.t().finished {
            // any click acknowledges "Готово"; the state is already reset
            if let Some(t) = self.t.as_mut() {
                t.finished = false;
            }
            self.flash = None;
        } else if ev.button == Button::Right {
            if let Some(t) = self.t.as_mut() {
                t.reset();
            }
        } else if let Some(i) = h.tabs.iter().position(|r| ev.inside(*r)).filter(|_| cx.size == Size::L) {
            if let Some(t) = self.t.as_mut() {
                t.set_mode(Mode::ALL[i]);
            }
        } else if cx.size != Size::L && ev.inside(h.label.inset_xy(-6.0, -3.0)) {
            if let Some(t) = self.t.as_mut() {
                t.set_mode(Mode::ALL[(t.mode.idx() + 1) % 3]);
            }
        } else if ev.inside(h.play.inset(if cx.size == Size::S { -4.0 } else { 0.0 })) {
            if let Some(t) = self.t.as_mut() {
                t.toggle(now);
            }
        } else if cx.size != Size::S && ev.inside(h.reset) {
            if let Some(t) = self.t.as_mut() {
                t.reset();
            }
        } else if self.t().mode == Mode::Timer && cx.size != Size::S && h.presets.iter().any(|r| ev.inside(*r)) {
            let i = h.presets.iter().position(|r| ev.inside(*r)).unwrap_or(0);
            if i < 4 {
                if let Some(t) = self.t.as_mut() {
                    t.set_duration(PRESETS[i] * 60);
                }
            } else {
                self.ask_custom(cx);
                changed = false;
            }
        } else if cx.size == Size::S && self.t().mode == Mode::Timer && !self.t().running && ev.inside(h.digits) {
            self.ask_custom(cx);
            changed = false;
        } else {
            changed = false;
        }
        if changed {
            self.ver += 1;
            self.persist(cx);
            let _ = cx.spawn(|| ()); // pokes tick() so the new schedule applies now (mouse events do not call tick)
        }
        changed
    }

    fn version(&self) -> u64 {
        self.ver
    }
}

impl TimerW {
    fn ask_custom(&mut self, cx: &Cx) {
        if self.ask.is_none() {
            self.ask = Some(cx.prompt_text(cx.t("Длительность: 90 (мин), 1:30, 25m, 90s", "Length: 90 (min), 1:30, 25m, 90s"), "", false));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const N: i64 = 1_800_000_000_000;

    #[test]
    fn timer_start_pause_resume() {
        let mut t = T::idle(Mode::Timer, 300);
        assert_eq!(t.paused_ms, 300_000);
        t.start(N);
        assert_eq!((t.end_ms, t.remaining(N + 100_000)), (N + 300_000, 200_000));
        assert_eq!(t.shown_ms(N + 100_400), 200_000); // 199.6 s left shows 200 (rounded up)
        t.pause(N + 100_000);
        assert_eq!((t.running, t.paused_ms), (false, 200_000));
        assert_eq!(t.remaining(N + 999_999), 200_000); // frozen while paused
        t.start(N + 500_000);
        assert_eq!(t.end_ms, N + 700_000);
        assert!((t.frac(N + 500_000) - 200.0 / 300.0).abs() < 1e-6);
        assert!(!t.check_finish(N + 699_999));
        assert!(t.check_finish(N + 700_000));
        assert!(t.finished && !t.running && t.paused_ms == 300_000);
        assert_eq!(t.frac(N), 1.0);
        t.finished = false;
        assert!(t.is_pristine());
    }

    #[test]
    fn stopwatch_laps() {
        let mut t = T::idle(Mode::Stopwatch, 300);
        t.start(N);
        assert_eq!(t.elapsed(N + 61_500), 61_500);
        assert!((t.frac(N + 61_500) - 1.5 / 60.0).abs() < 1e-6);
        t.pause(N + 90_000);
        t.start(N + 200_000); // resume continues from 90 s
        assert_eq!(t.elapsed(N + 210_000), 100_000);
        assert!(!t.check_finish(N + 10_000_000));
        t.reset();
        assert_eq!((t.running, t.paused_ms), (false, 0));
    }

    #[test]
    fn pomodoro_cycle() {
        let mut t = T::idle(Mode::Pomodoro, 300);
        assert_eq!(t.phase_ms(), 25 * 60_000);
        let mut now = N;
        for round in 1..=4u32 {
            t.start(now);
            now += 25 * 60_000;
            assert!(t.check_finish(now));
            assert!(t.brk && t.cycle == round && t.paused_ms == 5 * 60_000);
            t.finished = false;
            t.start(now);
            now += 5 * 60_000;
            assert!(t.check_finish(now));
            assert!(!t.brk);
            t.finished = false;
            assert_eq!(t.dots(), round % 4);
        }
        t.reset(); // untouched work phase: a second reset restarts the whole cycle
        assert_eq!((t.cycle, t.brk), (0, false));
        // the 4th work phase shows four dots during its break
        let mut u = T::idle(Mode::Pomodoro, 300);
        u.cycle = 4;
        u.brk = true;
        assert_eq!(u.dots(), 4);
    }

    #[test]
    fn persistence_round_trip() {
        let mut t = T::idle(Mode::Pomodoro, 600);
        t.start(N);
        t.cycle = 3;
        let mut v = json!({});
        for (k, x) in t.to_pairs() {
            v[k] = x;
        }
        assert_eq!(T::from_value(&v), t);
        // survives a restart: the end passed while the app was off
        let mut late = T::from_value(&v);
        assert!(late.check_finish(N + 26 * 60_000) && late.finished && late.brk && late.cycle == 4);
        // garbage in the settings is repaired
        let g = T::from_value(&json!({"mode": "pomodoro", "running": true, "end_ms": 0, "dur_s": -5, "paused_ms": 99999999999999i64}));
        assert!(!g.running && g.dur_s == 300 && g.paused_ms == 25 * 60_000);
        assert_eq!(T::from_value(&json!(null)), T::idle(Mode::Timer, 300));
        let mut d = T::idle(Mode::Timer, 300);
        d.set_duration(90);
        assert_eq!((d.dur_s, d.paused_ms), (90, 90_000));
        d.set_mode(Mode::Stopwatch);
        assert_eq!(d.dur_s, 90); // the timer length is remembered across modes
    }

    #[test]
    fn clock_format() {
        assert_eq!(fmt_clock(0, false), "00:00");
        assert_eq!(fmt_clock(59_000, false), "00:59");
        assert_eq!(fmt_clock(25 * 60_000, false), "25:00");
        assert_eq!(fmt_clock(3_723_000, false), "1:02:03");
        assert_eq!(fmt_clock(75_340, true), "01:15.3");
        assert_eq!(fmt_clock(-5, false), "00:00");
    }

    #[test]
    fn custom_duration() {
        assert_eq!(parse_duration("90"), Some(5400));
        assert_eq!(parse_duration("25m"), Some(1500));
        assert_eq!(parse_duration("25 мин"), Some(1500));
        assert_eq!(parse_duration("1:30"), Some(90));
        assert_eq!(parse_duration("1:02:03"), Some(3723));
        assert_eq!(parse_duration("90s"), Some(90));
        assert_eq!(parse_duration("2h"), Some(7200));
        assert_eq!(parse_duration("1h30m"), Some(5400));
        assert_eq!(parse_duration("1,5"), Some(90));
        assert_eq!(parse_duration("0"), None);
        assert_eq!(parse_duration("abc"), None);
        assert_eq!(parse_duration("1:75"), None);
        assert_eq!(parse_duration("25 hours"), None);
        assert_eq!(parse_duration("99999"), None);
        assert_eq!(parse_duration(""), None);
    }
}
