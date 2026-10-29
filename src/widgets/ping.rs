//! Ping: round-trip time to a few hosts, S = the first target (big latency, loss, sparkline), M = up to 3 targets as rows with a
//! sparkline of the last 60 results. Default targets: 1.1.1.1 and the default gateway (read from `GetAdaptersInfo`).
//! Network rule (same as Weather / Rates / Crypto): the card on the desk IS the opt-in; previews and the gallery use built-in sample data and
//! send nothing. A live card sends one ICMP echo (32 bytes of payload) per target every 2 s, on a worker thread, through the Windows
//! `IcmpSendEcho` API (no `ping.exe`, no console window), 1.5 s timeout, IPv4 only.
//! Interaction: "+" asks for a host name or IPv4 address (stored in the card's own settings, `targets`, at most 4), a right click on a
//! target removes it. Nothing is drawn per frame: results arrive every 2 s and bump `version()`.
use super::sysmon::PANEL;
use super::tasks::edge_fade;
use crate::registry::{Registry, WidgetDef};
use crate::widget::*;
use std::net::{Ipv4Addr, ToSocketAddrs};
use std::time::Instant;
use windows::Win32::NetworkManagement::IpHelper::{GetAdaptersInfo, IcmpCloseHandle, IcmpCreateFile, IcmpSendEcho, ICMP_ECHO_REPLY, IP_ADAPTER_INFO};

pub fn register(r: &mut Registry) {
    r.register(WidgetDef {
        type_id: "ping",
        name: ("Пинг", "Ping"),
        desc: ("Задержка до сервера и шлюза", "Latency to a server and your gateway"),
        group: "system",
        icon: Glyph::Wifi,
        sizes: &[Size::S, Size::M],
        default_size: Size::S,
        make: || Box::new(Ping::default()),
    });
}

const TIMEOUT_MS: u32 = 1500;
/// results the graph shows; one more is kept just outside its left edge for the flow
const KEEP: usize = 60;
const WINDOW: usize = KEEP + 1;
const MAX_TARGETS: usize = 4;
const GATEWAY: &str = "gateway";
const DEFAULTS: [&str; 2] = ["1.1.1.1", GATEWAY];

// ================================================================ pure parts

/// A host name (letters, digits, dots, dashes) or an IPv4 address, or the word `gateway`; lower-cased. None = refused.
fn clean_target(s: &str) -> Option<String> {
    let t = s.trim().trim_end_matches('.').to_ascii_lowercase();
    let ok = !t.is_empty()
        && t.len() <= 253
        && !t.starts_with(['.', '-'])
        && !t.contains("..")
        && t.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-');
    ok.then_some(t)
}

/// The `targets` setting: valid, distinct, at most 4; missing or empty gives the defaults.
fn parse_targets(v: Option<&Value>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for t in v.and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).filter_map(clean_target) {
        if !out.contains(&t) && out.len() < MAX_TARGETS {
            out.push(t);
        }
    }
    if out.is_empty() {
        out = DEFAULTS.iter().map(|s| s.to_string()).collect();
    }
    out
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Stats {
    last: Option<u32>,
    avg: Option<f32>,
    min: Option<u32>,
    max: Option<u32>,
    /// mean absolute difference of successive answered pings (ms)
    jitter: Option<f32>,
    loss_pct: f32,
}

/// Statistics of a window of results (oldest first); `None` = lost.
fn stats(w: &[Option<u32>]) -> Stats {
    let ok: Vec<u32> = w.iter().flatten().copied().collect();
    let avg = (!ok.is_empty()).then(|| ok.iter().sum::<u32>() as f32 / ok.len() as f32);
    let jitter = (ok.len() > 1).then(|| ok.windows(2).map(|p| p[0].abs_diff(p[1]) as f32).sum::<f32>() / (ok.len() - 1) as f32);
    let loss_pct = if w.is_empty() { 0.0 } else { (w.len() - ok.len()) as f32 * 100.0 / w.len() as f32 };
    Stats { last: w.last().copied().flatten(), avg, min: ok.iter().min().copied(), max: ok.iter().max().copied(), jitter, loss_pct }
}

/// The top of the graph scale for a window: the slowest answer plus a quarter, at least 30 ms (an idle line does not look like a storm).
fn scale_hi(w: &[Option<u32>]) -> f32 {
    (w.iter().flatten().copied().max().unwrap_or(0) as f32 * 1.25).max(30.0)
}

/// Colour state of a result: 0 fine (< 80 ms), 1 slow, 2 bad (>= 200 ms or lost): the thresholds of `level_color`.
fn state_of(v: Option<u32>) -> f32 {
    match v {
        None => 2.0,
        Some(v) if v >= 200 => 2.0,
        Some(v) if v >= 80 => 1.0,
        _ => 0.0,
    }
}

/// The colour of a (glided, fractional) state: accent -> amber -> red.
fn heat(x: f32) -> Color {
    let x = x.clamp(0.0, 2.0);
    if x <= 1.0 {
        accent().mix(ORANGE, x)
    } else {
        ORANGE.mix(RED, x - 1.0)
    }
}

fn push_result(w: &mut Vec<Option<u32>>, r: Option<u32>) {
    if w.len() >= WINDOW {
        w.remove(0);
    }
    w.push(r);
}

// ================================================================ ICMP (worker thread only)

/// The first default gateway of an enabled adapter, from `GetAdaptersInfo` (IPv4).
fn default_gateway() -> Option<Ipv4Addr> {
    unsafe {
        let mut len = 0u32;
        GetAdaptersInfo(None, &mut len); // ERROR_BUFFER_OVERFLOW: `len` is the size needed
        if len == 0 || len > 1 << 20 {
            return None;
        }
        let mut buf = vec![0u64; (len as usize).div_ceil(8)]; // u64: the structure wants 8-byte alignment
        if GetAdaptersInfo(Some(buf.as_mut_ptr() as *mut IP_ADAPTER_INFO), &mut len) != 0 {
            return None;
        }
        let mut p = buf.as_ptr() as *const IP_ADAPTER_INFO;
        let mut guard = 0;
        while !p.is_null() && guard < 64 {
            let a = &*p;
            let raw: Vec<u8> = a.GatewayList.IpAddress.String.iter().take_while(|c| **c != 0).map(|c| *c as u8).collect();
            if let Some(ip) = std::str::from_utf8(&raw).ok().and_then(|s| s.parse::<Ipv4Addr>().ok()).filter(|ip| !ip.is_unspecified()) {
                return Some(ip);
            }
            p = a.Next;
            guard += 1;
        }
        None
    }
}

fn resolve(target: &str) -> Option<Ipv4Addr> {
    if target == GATEWAY {
        return default_gateway();
    }
    if let Ok(ip) = target.parse::<Ipv4Addr>() {
        return Some(ip);
    }
    (target, 0).to_socket_addrs().ok()?.find_map(|a| match a.ip() {
        std::net::IpAddr::V4(v4) => Some(v4),
        _ => None,
    })
}

/// One echo request: the round-trip time in ms, None on timeout or any error.
fn echo(ip: Ipv4Addr) -> Option<u32> {
    unsafe {
        let h = IcmpCreateFile().ok()?;
        let payload = [0x61u8; 32];
        let mut reply = [0u64; 64]; // 512 bytes: ICMP_ECHO_REPLY + the echoed payload + 8 bytes of error room
        let n = IcmpSendEcho(h, u32::from_ne_bytes(ip.octets()), payload.as_ptr().cast(), payload.len() as u16, None, reply.as_mut_ptr().cast(), std::mem::size_of_val(&reply) as u32, TIMEOUT_MS);
        let _ = IcmpCloseHandle(h);
        if n == 0 {
            return None;
        }
        let r = &*(reply.as_ptr() as *const ICMP_ECHO_REPLY);
        (r.Status == 0).then_some(r.RoundTripTime) // IP_SUCCESS
    }
}

/// (the address that was pinged, the answer)
type Answer = (Option<Ipv4Addr>, Option<u32>);

fn ping_target(target: String) -> Answer {
    let ip = resolve(&target);
    (ip, ip.and_then(echo))
}

// ================================================================ widget

struct Row {
    target: String,
    ip: Option<Ipv4Addr>,
    results: Vec<Option<u32>>,
    job: Option<Pending<Answer>>,
    last: Option<Instant>,
    /// when the newest result arrived, and the (smoothed) seconds between results: the sparkline flows between them
    at: Option<Instant>,
    period: f32,
    /// the colour state the glides were last aimed at (0 fine, 1 slow, 2 slow / lost)
    state: f32,
}

#[derive(Default)]
struct Ping {
    rows: Vec<Row>,
    ver: u64,
    started: bool,
    ask: Option<Pending<Option<String>>>,
    /// the target list changed in `tick`: `mouse` (the only place with `&mut Cx`) writes it to the settings
    save: bool,
    /// per target: "y:" slot, "a:" opacity (a row enters / leaves), "s:" the S card's fade when its target changes, "p:" the state-change pulse
    slots: GlideAnim,
    /// per target: "v:" latency, "l:" loss %, "c:" colour state, "hi:" graph scale
    vals: GlideAnim,
    /// removed targets fading out in their slot
    ghosts: Vec<Row>,
    prev_targets: Vec<String>,
    primed: bool,
    /// something moves (a glide, or the flowing graph): `draw` leaves the dynamic parts out and `overlay` paints them
    moving: bool,
    pts: std::cell::RefCell<Vec<(f32, f32)>>,
}

fn sample_row(target: &str, base: f32, lost_every: usize) -> Row {
    let results = (0..KEEP).map(|i| if lost_every > 0 && i % lost_every == lost_every - 1 { None } else { Some((base + 4.0 * (i as f32 * 0.7).sin() + 9.0 * (i as f32 * 0.19).sin().max(0.0)).round() as u32) }).collect();
    Row { target: target.to_string(), ip: None, results, job: None, last: None, at: None, period: 2.0, state: 0.0 }
}

fn label(cx: &Cx, target: &str, ip: Option<Ipv4Addr>) -> String {
    if target == GATEWAY {
        match ip {
            Some(ip) => format!("{} {ip}", cx.t("Шлюз", "Gateway")),
            None => cx.t("Шлюз", "Gateway").to_string(),
        }
    } else {
        target.to_string()
    }
}

fn ms_text(cx: &Cx, v: Option<u32>) -> String {
    match v {
        Some(0) => "<1".to_string(),
        Some(v) => v.to_string(),
        None => cx.t("нет", "n/a").to_string(),
    }
}

const ROW_H: f32 = 38.0;

impl Ping {
    fn new_row(target: String) -> Row {
        Row { target, ip: None, results: Vec::new(), job: None, last: None, at: None, period: 2.0, state: 0.0 }
    }

    fn targets(&self) -> Vec<Value> {
        self.rows.iter().map(|r| json!(r.target)).collect()
    }

    /// Aim the value glides at row `i`'s newest numbers. A change of the colour state pulses the dot.
    fn aim(&mut self, cx: &Cx, i: usize) {
        let r = &mut self.rows[i];
        let (s, t) = (stats(&r.results), r.target.clone());
        let st = state_of(if r.results.is_empty() { Some(0) } else { s.last });
        if st != r.state && r.results.len() > 1 {
            self.vals.start(&format!("p:{t}"), 0.0, 1.0, cx);
        }
        r.state = st;
        self.vals.set(&format!("c:{t}"), st, cx);
        if let Some(ms) = s.last {
            self.vals.set(&format!("v:{t}"), ms as f32, cx);
        }
        self.vals.set(&format!("l:{t}"), s.loss_pct, cx);
        self.vals.set(&format!("hi:{t}"), scale_hi(&r.results), cx);
    }

    /// Aim the row glides (slot, opacity, S fade) at the current target list: a new target enters from below, the others glide to their slot.
    fn retarget(&mut self, cx: &Cx) {
        let now: Vec<String> = self.rows.iter().map(|r| r.target.clone()).collect();
        let anim = self.primed && !cx.preview && !reduced_motion();
        for (i, t) in now.iter().enumerate() {
            self.ghosts.retain(|g| g.target != *t);
            let (y, a) = (format!("y:{t}"), format!("a:{t}"));
            if anim && !self.prev_targets.contains(t) {
                self.slots.set_from(&y, i as f32 + 0.8, i as f32, cx);
                self.slots.set_from(&a, 0.0, 1.0, cx);
            } else {
                self.slots.set(&y, i as f32, cx);
                self.slots.set(&a, 1.0, cx);
            }
        }
        if anim && now.first() != self.prev_targets.first() {
            if let Some(t) = now.first() {
                self.slots.start(&format!("s:{t}"), 0.0, 1.0, cx);
            }
        }
        self.prev_targets = now;
        self.primed = true;
        cx.waker().wake(); // a click does not call `tick`: poke it so the glides start now
    }

    fn init(&mut self, cx: &Cx) {
        if self.started {
            return;
        }
        self.started = true;
        self.ver += 1;
        self.slots = GlideAnim::smooth(0.4);
        if cx.preview {
            self.rows = vec![sample_row("1.1.1.1", 18.0, 0), { let mut g = sample_row(GATEWAY, 2.0, 23); g.ip = Some(Ipv4Addr::new(192, 168, 1, 1)); g }, sample_row("example.org", 74.0, 0)];
        } else {
            self.rows = parse_targets(cx.cfg.get("targets")).into_iter().map(Ping::new_row).collect(); // read once; changes are kept here and written back by `mouse`
        }
        for i in 0..self.rows.len() {
            self.aim(cx, i);
        }
        self.retarget(cx);
    }

    fn plus(c: Rect) -> Rect {
        Rect::new(c.right() - 20.0, c.y - 1.0, 20.0, 20.0)
    }
    fn m_row(c: Rect, y: f32) -> Rect {
        Rect::new(c.x, c.y + 24.0 + y * ROW_H, c.w, 36.0)
    }
    /// The graph of a row: the flowing line while results keep arriving, the plain one in previews / reduced motion.
    fn spark(&self, cx: &Cx, r: Rect, row: &Row, col: Color) {
        if row.results.len() < 2 {
            return;
        }
        let hi = self.vals.or(&format!("hi:{}", row.target), scale_hi(&row.results));
        let values: Vec<f32> = row.results.iter().map(|x| x.map_or(1.0e9, |v| v as f32)).collect(); // a lost ping reaches the top (the line clamps)
        if cx.preview || reduced_motion() || row.at.is_none() {
            cx.sparkline_range(r, &values[values.len().saturating_sub(KEEP)..], 0.0, hi, col);
        } else {
            let mut pts = self.pts.borrow_mut();
            let since = row.at.map_or(0.0, |t| t.elapsed().as_secs_f32());
            crate::motion::flow_points(&values, KEEP, crate::motion::flow_phase(since, row.period), &mut pts);
            cx.sparkline_flow(r, &pts, 0.0, hi, col);
        }
    }

    /// One M row at slot `y`.
    fn paint_m_row(&self, cx: &Cx, c: Rect, row: &Row, y: f32, a: f32) {
        let a = a * edge_fade(y, 3);
        if a <= 0.0 {
            return;
        }
        let rect = Ping::m_row(c, y);
        let t = &row.target;
        let g = |p: &str, d: f32| self.vals.or(&format!("{p}:{t}"), d);
        let s = stats(&row.results);
        let col = if row.results.is_empty() { DIM } else { heat(g("c", row.state)) };
        cx.with_opacity(a, |cx| {
            cx.circle((rect.x + 4.0, rect.y + 9.0), 3.5, col);
            let p = self.vals.or(&format!("p:{t}"), 1.0);
            if p < 1.0 {
                cx.circle_stroke((rect.x + 4.0, rect.y + 9.0), 3.5 + 7.0 * p, 1.4, col.fade(0.7 * (1.0 - p))); // the pulse of a state change
            }
            cx.text(Rect::new(rect.x + 14.0, rect.y, 112.0, 18.0), &label(cx, t, row.ip), Style::BODY.ellipsis(), TEXT);
            let detail = if row.results.is_empty() {
                cx.t("ожидание…", "waiting…").to_string()
            } else {
                let shown = if s.last.is_some() { Some(g("v", s.last.unwrap_or(0) as f32).round() as u32) } else { None };
                format!("{} {} · {} {}%", ms_text(cx, shown), cx.t("мс", "ms"), cx.t("потери", "loss"), g("l", s.loss_pct).round())
            };
            cx.text(Rect::new(rect.x + 14.0, rect.y + 18.0, 112.0, 16.0), &detail, Style::CAPTION.ellipsis(), DIM);
            let panel = Rect::new(rect.x + 130.0, rect.y, rect.w - 130.0, rect.h);
            cx.rrect(panel, 8.0, PANEL);
            self.spark(cx, panel.inset_xy(1.0, 4.0), row, col);
        });
    }

    /// Everything that moves. `draw` calls it when nothing moves, `overlay` while something does.
    fn paint(&self, cx: &Cx) {
        let c = cx.content();
        let Some(first) = self.rows.first() else { return };
        match cx.size {
            Size::M => {
                for (i, r) in self.rows.iter().enumerate() {
                    let g = |p: &str, d: f32| self.slots.or(&format!("{p}:{}", r.target), d);
                    self.paint_m_row(cx, c, r, g("y", i as f32), g("a", 1.0));
                }
                for r in &self.ghosts {
                    let g = |p: &str, d: f32| self.slots.or(&format!("{p}:{}", r.target), d);
                    self.paint_m_row(cx, c, r, g("y", 0.0), g("a", 0.0));
                }
            }
            _ => {
                let t = &first.target;
                let f = self.slots.or(&format!("s:{t}"), 1.0); // a new first target fades in
                let g = |p: &str, d: f32| self.vals.or(&format!("{p}:{t}"), d);
                let s = stats(&first.results);
                cx.with_opacity(f, |cx| {
                    cx.text(Rect::new(c.x + 50.0, c.y - 1.0, c.w - 50.0 - 24.0, 20.0), &label(cx, t, first.ip), Style::CAPTION.ellipsis(), DIM);
                    if first.results.is_empty() {
                        cx.text(Rect::new(c.x, c.y + 22.0, c.w, 44.0), "…", Style::new(38.0, 300), FAINT);
                        return;
                    }
                    let col = heat(g("c", first.state));
                    let big = Style::new(38.0, 300).mono();
                    let shown = s.last.map(|l| g("v", l as f32).round() as u32);
                    let num = ms_text(cx, shown);
                    cx.text(Rect::new(c.x, c.y + 22.0, c.w, 44.0), &num, big, if s.last.is_none() { RED } else { TEXT });
                    if s.last.is_some() {
                        let nw = cx.measure(&num, big, 0.0).0;
                        cx.text(Rect::new(c.x + nw + 6.0, c.y + 44.0, 40.0, 16.0), cx.t("мс", "ms"), Style::CAPTION, DIM);
                    }
                    let loss_pct = g("l", s.loss_pct);
                    let loss = format!("{} {}%", cx.t("потери", "loss"), loss_pct.round());
                    let avg = s.avg.map_or(String::new(), |a| format!("  ·  {} {}", cx.t("средн.", "avg"), a.round()));
                    cx.text(Rect::new(c.x, c.y + 66.0, c.w, 16.0), &format!("{loss}{avg}"), Style::CAPTION.ellipsis(), if loss_pct.round() > 0.0 { col } else { DIM });
                    self.spark(cx, Rect::new(c.x, c.y + 88.0, c.w, c.h - 88.0), first, col);
                    let p = self.vals.or(&format!("p:{t}"), 1.0);
                    if p < 1.0 {
                        cx.circle_stroke((c.right() - 6.0, c.y + 50.0), 4.0 + 8.0 * p, 1.4, col.fade(0.7 * (1.0 - p))); // the pulse of a state change
                    }
                    cx.circle((c.right() - 6.0, c.y + 50.0), 4.0, col);
                });
            }
        }
    }
}

impl Widget for Ping {
    fn draw(&mut self, cx: &mut Cx) -> Result<()> {
        self.init(cx);
        let c = cx.content();
        cx.text(Rect::new(c.x, c.y - 1.0, 60.0, 20.0), cx.t("Пинг", "Ping"), Style::TITLE, accent());
        if self.rows.len() < MAX_TARGETS {
            cx.icon(Glyph::Plus, Ping::plus(c).inset(2.0), accent());
        }
        if !self.moving {
            self.paint(cx);
        }
        Ok(())
    }

    fn overlay(&mut self, cx: &mut Cx) -> Result<()> {
        if self.moving {
            self.paint(cx);
        }
        Ok(())
    }

    fn tick(&mut self, cx: &Cx) -> Tick {
        self.init(cx);
        if cx.preview {
            return Tick::Never;
        }
        // the text popup of "+"
        let polled = self.ask.as_ref().map(|p| (p.take(), p.busy()));
        if let Some((res, busy)) = polled {
            match res {
                Some(r) => {
                    self.ask = None;
                    if let Some(t) = r.as_deref().and_then(clean_target) {
                        if self.rows.len() < MAX_TARGETS && !self.rows.iter().any(|r| r.target == t) {
                            self.rows.push(Ping::new_row(t));
                            self.save = true;
                            self.ver += 1;
                            self.retarget(cx);
                        }
                    }
                }
                None if !busy => self.ask = None,
                None => {}
            }
        }
        let mut changed = false;
        for i in 0..self.rows.len() {
            let r = &mut self.rows[i];
            let mut got = false;
            if let Some(j) = &r.job {
                match j.take() {
                    Some((ip, rtt)) => {
                        r.job = None;
                        r.ip = ip.or(r.ip);
                        let now = Instant::now();
                        if let Some(t) = r.at {
                            r.period = (r.period * 0.7 + now.duration_since(t).as_secs_f32().clamp(0.5, 4.0) * 0.3).clamp(0.5, 4.0);
                        }
                        r.at = Some(now);
                        push_result(&mut r.results, rtt);
                        got = true;
                    }
                    None if !j.busy() => r.job = None,
                    None => {}
                }
            }
            if r.job.is_none() && r.last.map_or(true, |t| t.elapsed() >= Duration::from_millis(1900)) {
                r.last = Some(Instant::now());
                let t = r.target.clone();
                r.job = Some(cx.spawn(move || ping_target(t)));
            }
            if got {
                self.aim(cx, i);
                changed = true;
            }
        }
        if changed {
            self.ver += 1;
        }
        let busy = self.slots.step(cx) | self.vals.step(cx);
        // the graphs flow between results (like System / Network): 60 fps while a graph has a line to scroll
        let flowing = !reduced_motion() && self.rows.iter().any(|r| r.results.len() >= 2 && r.at.is_some());
        let moving = busy || flowing;
        if moving != self.moving {
            self.moving = moving;
            self.ver += 1; // `draw` takes the dynamic parts over again (or hands them to `overlay`)
        }
        if !busy && !self.ghosts.is_empty() {
            self.ghosts.clear();
            let keep: Vec<String> = self.rows.iter().map(|r| r.target.clone()).collect();
            let alive = |k: &str| k.split_once(':').is_some_and(|(_, t)| keep.iter().any(|x| x == t));
            self.slots.retain(alive);
            self.vals.retain(alive);
            self.ver += 1;
        }
        if moving {
            return Tick::Fps(60);
        }
        Tick::Poll(Duration::from_secs(2))
    }

    fn mouse(&mut self, ev: &MouseEv, cx: &mut Cx) -> bool {
        if cx.preview {
            return false;
        }
        if self.save {
            self.save = false;
            cx.cfg_set("targets", Value::Array(self.targets()));
        }
        if ev.kind != MouseKind::Up {
            return false;
        }
        let c = cx.content();
        if ev.button == Button::Left && self.rows.len() < MAX_TARGETS && ev.inside(Ping::plus(c).inset(-4.0)) {
            if self.ask.is_none() {
                self.ask = Some(cx.prompt_text_ex(cx.t("Адрес для пинга", "Host to ping"), "", false, cx.t("Имя хоста или IPv4…", "Host name or IPv4…"), cx.t("Добавить", "Add")));
            }
            return false;
        }
        if ev.button == Button::Right && cx.size == Size::M {
            if let Some(i) = (0..self.rows.len().min(3)).find(|i| ev.inside(Ping::m_row(c, *i as f32))) {
                let gone = self.rows.remove(i);
                if !reduced_motion() {
                    self.ghosts.push(gone); // fades out in its slot while the rows below glide up
                    let t = self.ghosts.last().map(|g| g.target.clone()).unwrap_or_default();
                    self.slots.set(&format!("a:{t}"), 0.0, cx);
                }
                if self.rows.is_empty() {
                    self.rows = parse_targets(None).into_iter().map(Ping::new_row).collect(); // the last one removed: the defaults come back
                    for i in 0..self.rows.len() {
                        self.aim(cx, i);
                    }
                }
                self.retarget(cx);
                cx.cfg_set("targets", Value::Array(self.targets()));
                self.ver += 1;
                return true;
            }
        }
        false
    }

    fn version(&self) -> u64 {
        self.ver
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_names() {
        assert_eq!(clean_target(" Example.ORG. ").as_deref(), Some("example.org"));
        assert_eq!(clean_target("1.1.1.1").as_deref(), Some("1.1.1.1"));
        assert_eq!(clean_target("gateway").as_deref(), Some("gateway"));
        for bad in ["", "  ", "a b", "http://x.org", "x.org:80", "-x.org", ".x", "a..b", "x_y", "тест", &"a".repeat(254)] {
            assert_eq!(clean_target(bad), None, "{bad}");
        }
    }

    #[test]
    fn targets_setting() {
        assert_eq!(parse_targets(None), vec!["1.1.1.1", "gateway"]);
        assert_eq!(parse_targets(Some(&json!([]))), vec!["1.1.1.1", "gateway"]);
        assert_eq!(parse_targets(Some(&json!("8.8.8.8"))), vec!["1.1.1.1", "gateway"]);
        assert_eq!(parse_targets(Some(&json!(["8.8.8.8", "bad host", "8.8.8.8", 5, "a.b", "c.d", "e.f", "g.h"]))), vec!["8.8.8.8", "a.b", "c.d", "e.f"]);
    }

    #[test]
    fn statistics() {
        let w = [Some(10), Some(20), None, Some(40), Some(20)];
        let s = stats(&w);
        assert_eq!((s.last, s.min, s.max), (Some(20), Some(10), Some(40)));
        assert_eq!(s.avg, Some(22.5));
        assert_eq!(s.loss_pct, 20.0);
        // answered pings only: |20-10| + |40-20| + |20-40| = 50 over 3 steps
        assert!((s.jitter.unwrap() - 50.0 / 3.0).abs() < 1e-4);
        let none = stats(&[None, None]);
        assert_eq!((none.avg, none.min, none.last, none.jitter, none.loss_pct), (None, None, None, None, 100.0));
        assert_eq!(stats(&[]).loss_pct, 0.0);
        assert_eq!(stats(&[Some(5)]).jitter, None);
        assert_eq!(stats(&[Some(5), None]).last, None); // the newest one was lost
    }

    #[test]
    fn graph_scale_window_and_colours() {
        assert_eq!(scale_hi(&[Some(10), None, Some(80)]), 100.0);
        assert_eq!(scale_hi(&[Some(2)]), 30.0); // an idle line does not look like a storm
        assert_eq!(scale_hi(&[None]), 30.0);
        let mut w = Vec::new();
        for i in 0..100 {
            push_result(&mut w, Some(i));
        }
        assert_eq!((w.len(), w[0], w[WINDOW - 1]), (WINDOW, Some(39), Some(99))); // the shown 60 and the spare one outside the left edge
        assert_eq!((state_of(Some(5)), state_of(Some(80)), state_of(Some(199)), state_of(Some(200)), state_of(None)), (0.0, 1.0, 1.0, 2.0, 2.0));
        let near = |a: Color, b: Color| (a.r - b.r).abs() + (a.g - b.g).abs() + (a.b - b.b).abs() < 1e-4;
        assert!(near(heat(0.0), accent()) && near(heat(1.0), ORANGE) && near(heat(2.0), RED) && near(heat(9.0), RED));
        let mid = heat(0.5);
        assert!(mid != accent() && mid != ORANGE, "a state change cross-fades");
    }

    #[test]
    fn a_state_change_glides_and_ends_exactly() {
        use crate::motion::Glide;
        let mut g = Glide::default();
        g.set("c:x", 0.0);
        g.set("c:x", state_of(None)); // recovery to loss
        g.start("p:x", 0.0, 1.0);
        let mut seen_mid = false;
        let mut n = 0;
        while g.step(1.0 / 60.0) {
            let c = g.get("c:x").unwrap();
            seen_mid |= c > 0.2 && c < 1.8;
            n += 1;
            assert!(n < 120);
        }
        assert!(seen_mid);
        assert_eq!((g.get("c:x"), g.get("p:x")), (Some(2.0), Some(1.0)));
    }

    /// Needs a network: `cargo test ping_live -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn ping_live() {
        println!("gateway: {:?}", default_gateway());
        println!("loopback: {:?}", echo(Ipv4Addr::LOCALHOST));
        println!("1.1.1.1: {:?}", ping_target("1.1.1.1".into()));
    }
}
