//! Ping: round-trip time to a few hosts, S = the first target (big latency, loss, sparkline), M = up to 3 targets as rows with a
//! sparkline of the last 60 results. Default targets: 1.1.1.1 and the default gateway (read from `GetAdaptersInfo`).
//! Network rule (same as Weather / Rates / Crypto): the card on the desk IS the opt-in; previews and the gallery use built-in sample data and
//! send nothing. A live card sends one ICMP echo (32 bytes of payload) per target every 2 s, on a worker thread, through the Windows
//! `IcmpSendEcho` API (no `ping.exe`, no console window), 1.5 s timeout, IPv4 only.
//! Interaction: "+" asks for a host name or IPv4 address (stored in the card's own settings, `targets`, at most 4), a right click on a
//! target removes it. Nothing is drawn per frame: results arrive every 2 s and bump `version()`.
use super::sysmon::{level_color, PANEL};
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
const KEEP: usize = 60;
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

/// Graph values of a window: answered pings as they are, lost ones at the top of the scale. Returns (values, scale max).
fn graph_values(w: &[Option<u32>]) -> (Vec<f32>, f32) {
    let hi = (w.iter().flatten().copied().max().unwrap_or(0) as f32 * 1.25).max(30.0);
    (w.iter().map(|r| r.map_or(hi, |v| v as f32)).collect(), hi)
}

fn push_result(w: &mut Vec<Option<u32>>, r: Option<u32>) {
    if w.len() >= KEEP {
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
}

#[derive(Default)]
struct Ping {
    rows: Vec<Row>,
    ver: u64,
    started: bool,
    ask: Option<Pending<Option<String>>>,
    /// the target list changed in `tick`: `mouse` (the only place with `&mut Cx`) writes it to the settings
    save: bool,
}

fn sample_row(target: &str, base: f32, lost_every: usize) -> Row {
    let results = (0..KEEP).map(|i| if lost_every > 0 && i % lost_every == lost_every - 1 { None } else { Some((base + 4.0 * (i as f32 * 0.7).sin() + 9.0 * (i as f32 * 0.19).sin().max(0.0)).round() as u32) }).collect();
    Row { target: target.to_string(), ip: None, results, job: None, last: None }
}

fn label(cx: &Cx, r: &Row) -> String {
    if r.target == GATEWAY {
        match r.ip {
            Some(ip) => format!("{} {ip}", cx.t("Шлюз", "Gateway")),
            None => cx.t("Шлюз", "Gateway").to_string(),
        }
    } else {
        r.target.clone()
    }
}

fn ms_text(cx: &Cx, v: Option<u32>) -> String {
    match v {
        Some(0) => "<1".to_string(),
        Some(v) => v.to_string(),
        None => cx.t("нет", "n/a").to_string(),
    }
}

fn colour(v: Option<u32>) -> Color {
    v.map_or(RED, |v| level_color(v as f32, 80.0, 200.0))
}

impl Ping {
    fn new_row(target: String) -> Row {
        Row { target, ip: None, results: Vec::new(), job: None, last: None }
    }

    fn targets(&self) -> Vec<Value> {
        self.rows.iter().map(|r| json!(r.target)).collect()
    }

    fn init(&mut self, cx: &Cx) {
        if self.started {
            return;
        }
        self.started = true;
        self.ver += 1;
        if cx.preview {
            self.rows = vec![sample_row("1.1.1.1", 18.0, 0), { let mut g = sample_row(GATEWAY, 2.0, 23); g.ip = Some(Ipv4Addr::new(192, 168, 1, 1)); g }, sample_row("example.org", 74.0, 0)];
        } else {
            self.rows = parse_targets(cx.cfg.get("targets")).into_iter().map(Ping::new_row).collect(); // read once; changes are kept here and written back by `mouse`
        }
    }

    fn plus(c: Rect) -> Rect {
        Rect::new(c.right() - 20.0, c.y - 1.0, 20.0, 20.0)
    }
    fn m_row(c: Rect, i: usize) -> Rect {
        Rect::new(c.x, c.y + 24.0 + i as f32 * 38.0, c.w, 36.0)
    }
}

impl Widget for Ping {
    fn draw(&mut self, cx: &mut Cx) -> Result<()> {
        self.init(cx);
        let c = cx.content();
        cx.text(Rect::new(c.x, c.y - 1.0, 60.0, 20.0), cx.t("Пинг", "Ping"), Style::TITLE, accent());
        let can_add = self.rows.len() < MAX_TARGETS;
        if can_add {
            cx.icon(Glyph::Plus, Ping::plus(c).inset(2.0), accent());
        }
        let Some(first) = self.rows.first() else { return Ok(()) };
        match cx.size {
            Size::M => {
                for (i, r) in self.rows.iter().take(3).enumerate() {
                    let row = Ping::m_row(c, i);
                    let s = stats(&r.results);
                    let col = if r.results.is_empty() { DIM } else { colour(s.last) };
                    cx.circle((row.x + 4.0, row.y + 9.0), 3.5, col);
                    cx.text(Rect::new(row.x + 14.0, row.y, 112.0, 18.0), &label(cx, r), Style::BODY.ellipsis(), TEXT);
                    let detail = if r.results.is_empty() { cx.t("ожидание…", "waiting…").to_string() } else { format!("{} {} · {} {}%", ms_text(cx, s.last), cx.t("мс", "ms"), cx.t("потери", "loss"), s.loss_pct.round()) };
                    cx.text(Rect::new(row.x + 14.0, row.y + 18.0, 112.0, 16.0), &detail, Style::CAPTION.ellipsis(), DIM);
                    let g = Rect::new(row.x + 130.0, row.y, row.w - 130.0, row.h);
                    cx.rrect(g, 8.0, PANEL);
                    if r.results.len() >= 2 {
                        let (v, hi) = graph_values(&r.results);
                        cx.sparkline_range(g.inset_xy(1.0, 4.0), &v, 0.0, hi, col);
                    }
                }
            }
            _ => {
                let s = stats(&first.results);
                cx.text(Rect::new(c.x + 50.0, c.y - 1.0, c.w - 50.0 - 24.0, 20.0), &label(cx, first), Style::CAPTION.ellipsis(), DIM);
                if first.results.is_empty() {
                    cx.text(Rect::new(c.x, c.y + 22.0, c.w, 44.0), "…", Style::new(38.0, 300), FAINT);
                } else {
                    let big = Style::new(38.0, 300).mono();
                    let num = ms_text(cx, s.last);
                    cx.text(Rect::new(c.x, c.y + 22.0, c.w, 44.0), &num, big, if s.last.is_none() { RED } else { TEXT });
                    if s.last.is_some() {
                        let nw = cx.measure(&num, big, 0.0).0;
                        cx.text(Rect::new(c.x + nw + 6.0, c.y + 44.0, 40.0, 16.0), cx.t("мс", "ms"), Style::CAPTION, DIM);
                    }
                    let col = colour(s.last);
                    let loss = format!("{} {}%", cx.t("потери", "loss"), s.loss_pct.round());
                    let avg = s.avg.map_or(String::new(), |a| format!("  ·  {} {}", cx.t("средн.", "avg"), a.round()));
                    cx.text(Rect::new(c.x, c.y + 66.0, c.w, 16.0), &format!("{loss}{avg}"), Style::CAPTION.ellipsis(), if s.loss_pct > 0.0 { col } else { DIM });
                    if first.results.len() >= 2 {
                        let (v, hi) = graph_values(&first.results);
                        cx.sparkline_range(Rect::new(c.x, c.y + 88.0, c.w, c.h - 88.0), &v, 0.0, hi, col);
                    }
                }
            }
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
                        }
                    }
                }
                None if !busy => self.ask = None,
                None => {}
            }
        }
        let mut changed = false;
        for r in &mut self.rows {
            if let Some(j) = &r.job {
                match j.take() {
                    Some((ip, rtt)) => {
                        r.job = None;
                        r.ip = ip.or(r.ip);
                        push_result(&mut r.results, rtt);
                        changed = true;
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
        }
        if changed {
            self.ver += 1;
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
            if let Some(i) = (0..self.rows.len().min(3)).find(|i| ev.inside(Ping::m_row(c, *i))) {
                self.rows.remove(i);
                if self.rows.is_empty() {
                    self.rows = parse_targets(None).into_iter().map(Ping::new_row).collect(); // the last one removed: the defaults come back
                }
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
    fn graph_scale_and_window() {
        let (v, hi) = graph_values(&[Some(10), None, Some(80)]);
        assert_eq!(hi, 100.0);
        assert_eq!(v, vec![10.0, 100.0, 80.0]); // a lost ping reaches the top
        assert_eq!(graph_values(&[Some(2)]).1, 30.0); // an idle line does not look like a storm
        let mut w = Vec::new();
        for i in 0..100 {
            push_result(&mut w, Some(i));
        }
        assert_eq!((w.len(), w[0], w[KEEP - 1]), (KEEP, Some(40), Some(99)));
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
