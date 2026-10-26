//! Network: S download speed (big) + upload + a tiny dual sparkline; M two stacked sparklines (down blue, up green) with the current
//! speeds and the traffic since the widget started.
//! Data: `GetIfTable2` byte counters once a second (`Tick::Every(1 s)`, a few hundred microseconds, UI thread). Only operational,
//! non-loopback, non-tunnel interfaces count; when a physical adapter is up, virtual ones (Hyper-V / WSL / VPN, whose traffic is already
//! counted on the physical NIC) are ignored so nothing is counted twice. Speeds are per-interface counter deltas, so an adapter that
//! appears or resets never produces a spike.
use super::sysmon::{fmt_rate_parts, fmt_size, Hist, PANEL};
use crate::registry::{Registry, WidgetDef};
use crate::widget::*;
use std::collections::HashMap;
use std::time::Instant;
use windows::Win32::NetworkManagement::IpHelper::{FreeMibTable, GetIfTable2, MIB_IF_TABLE2};
use windows::Win32::NetworkManagement::Ndis::IfOperStatusUp;

pub fn register(r: &mut Registry) {
    r.register(WidgetDef {
        type_id: "network",
        name: ("Сеть", "Network"),
        desc: ("Скорость сети и состояние подключения", "Network speed and connection state"),
        group: "system",
        icon: Glyph::Wifi,
        sizes: &[Size::S, Size::M],
        default_size: Size::S,
        make: || Box::new(Network::new()),
    });
}

const UP: Color = GREEN;
const IF_LOOPBACK: u32 = 24;
const IF_TUNNEL: u32 = 131;
/// the smallest graph scale (bytes/s) so an idle line does not look like a storm
const MIN_SCALE: f32 = 64.0 * 1024.0;

#[derive(Clone, Copy, Debug)]
struct Row {
    luid: u64,
    up: bool,
    typ: u32,
    hw: bool,
    rx: u64,
    tx: u64,
}

/// The interfaces that count: operational, not loopback / tunnel; physical only when any physical one is up.
fn select(rows: &[Row]) -> Vec<Row> {
    let ok: Vec<Row> = rows.iter().copied().filter(|r| r.up && r.typ != IF_LOOPBACK && r.typ != IF_TUNNEL).collect();
    if ok.iter().any(|r| r.hw) {
        ok.into_iter().filter(|r| r.hw).collect()
    } else {
        ok
    }
}

/// Bytes received / sent since `prev` summed over the selected interfaces (new interfaces count from zero, resets count as zero);
/// `prev` is replaced by the current counters.
fn step(prev: &mut HashMap<u64, (u64, u64)>, rows: &[Row]) -> (u64, u64) {
    let (mut rx, mut tx) = (0u64, 0u64);
    let mut next = HashMap::with_capacity(rows.len());
    for r in rows {
        if let Some(&(prx, ptx)) = prev.get(&r.luid) {
            rx += r.rx.saturating_sub(prx);
            tx += r.tx.saturating_sub(ptx);
        }
        next.insert(r.luid, (r.rx, r.tx));
    }
    *prev = next;
    (rx, tx)
}

/// Reads the interface table into `out` (cleared first). False when the call failed.
fn read_rows(out: &mut Vec<Row>) -> bool {
    out.clear();
    unsafe {
        let mut t: *mut MIB_IF_TABLE2 = std::ptr::null_mut();
        if GetIfTable2(&mut t).0 != 0 || t.is_null() {
            return false;
        }
        let rows = std::slice::from_raw_parts((*t).Table.as_ptr(), (*t).NumEntries as usize);
        for r in rows {
            out.push(Row { luid: r.InterfaceLuid.Value, up: r.OperStatus == IfOperStatusUp, typ: r.Type, hw: r.InterfaceAndOperStatusFlags._bitfield & 1 != 0, rx: r.InOctets, tx: r.OutOctets });
        }
        FreeMibTable(t as *const _);
    }
    true
}

/// Worker-side reader: the interface buffer and the previous counters.
#[derive(Default)]
struct Reader {
    rows: Vec<Row>,
    prev: HashMap<u64, (u64, u64)>,
}
/// One reading: when, any interface online, bytes received / sent since the previous reading.
type Reading = (Instant, bool, u64, u64);
impl Reader {
    fn read(&mut self) -> Option<Reading> {
        let at = Instant::now();
        if !read_rows(&mut self.rows) {
            return None;
        }
        let sel = select(&self.rows);
        let (rx, tx) = step(&mut self.prev, &sel);
        Some((at, !sel.is_empty(), rx, tx))
    }
}

struct Network {
    init: bool,
    reader: Option<Reader>,
    job: Option<Pending<(Reader, Option<Reading>)>>,
    spawned: Option<Instant>,
    /// time of the previous applied reading
    last: Option<Instant>,
    /// bytes per second history
    down: Hist,
    up: Hist,
    total_rx: u64,
    total_tx: u64,
    online: bool,
}

impl Network {
    fn new() -> Network {
        Network { init: false, reader: Some(Reader::default()), job: None, spawned: None, last: None, down: Hist::new(60), up: Hist::new(60), total_rx: 0, total_tx: 0, online: true }
    }

    fn init_preview(&mut self, cx: &Cx) {
        if self.init || !cx.preview {
            return;
        }
        self.init = true;
        const MB: f32 = 1048576.0;
        self.down = Hist::wave(60, |i| ((7.0 + 5.5 * (i as f32 * 0.23).sin() + 3.0 * (i as f32 * 0.8 + 1.0).sin()) * MB).max(0.02 * MB));
        self.up = Hist::wave(60, |i| ((0.8 + 0.5 * (i as f32 * 0.31 + 2.0).sin() + 0.25 * (i as f32 * 1.1).sin()) * MB).max(0.01 * MB));
        self.total_rx = 2_580_000_000;
        self.total_tx = 327_000_000;
    }

    fn apply(&mut self, (at, online, rx, tx): Reading) {
        self.online = online;
        if let Some(dt) = self.last.map(|t| at.duration_since(t).as_secs_f64()).filter(|d| *d > 0.2) {
            self.down.push((rx as f64 / dt) as f32);
            self.up.push((tx as f64 / dt) as f32);
            self.total_rx += rx;
            self.total_tx += tx;
        }
        self.last = Some(at);
    }
}

/// Number + unit drawn on one baseline: the unit follows the measured width of the number.
fn value_unit(cx: &Cx, x: f32, y: f32, h: f32, bps: f32, big: f32, unit_w: f32) {
    let (n, u) = fmt_rate_parts(bps as f64, cx.lang);
    let st = Style::new(big, 300).mono();
    let (nw, _) = cx.measure(&n, st, 400.0);
    cx.text(Rect::new(x, y, nw + 4.0, h), &n, st, TEXT);
    cx.text(Rect::new(x + nw + 3.0, y + h * 0.5 + big * 0.12, unit_w, 16.0), u, Style::CAPTION.weight(500), DIM);
}

fn scale_of(a: &Hist, b: Option<&Hist>) -> f32 {
    (a.max().max(b.map_or(0.0, |b| b.max())) * 1.15).max(MIN_SCALE)
}

impl Widget for Network {
    fn draw(&mut self, cx: &mut Cx) -> Result<()> {
        self.init_preview(cx);
        let c = cx.content();
        match cx.size {
            Size::S => {
                cx.icon(Glyph::NetDown, Rect::new(c.x, c.y + 1.0, 14.0, 14.0), accent());
                cx.text(Rect::new(c.x + 19.0, c.y, c.w - 19.0, 16.0), &cx.t("ЗАГРУЗКА", "DOWNLOAD"), Style::LABEL.size(11.0), DIM);
                cx.icon(Glyph::NetUp, Rect::new(c.x, c.y + 73.0, 14.0, 14.0), UP);
            }
            Size::M => {
                for i in 0..2 {
                    let lane = m_lane(i);
                    let (glyph, color, label) = if i == 0 { (Glyph::NetDown, accent(), cx.t("ЗАГРУЗКА", "DOWNLOAD")) } else { (Glyph::NetUp, UP, cx.t("ОТДАЧА", "UPLOAD")) };
                    cx.icon(glyph, Rect::new(lane.x, lane.y + 1.0, 13.0, 13.0), color);
                    cx.text(Rect::new(lane.x + 18.0, lane.y, 96.0, 15.0), label, Style::LABEL.size(10.5), DIM);
                    cx.rrect(Rect::new(lane.x + 124.0, lane.y, lane.w - 124.0, lane.h), 9.0, PANEL);
                }
            }
            Size::L => {}
        }
        Ok(())
    }

    fn overlay(&mut self, cx: &mut Cx) -> Result<()> {
        self.init_preview(cx);
        let c = cx.content();
        let (d, u) = (self.down.last().unwrap_or(0.0), self.up.last().unwrap_or(0.0));
        let lang = cx.lang;
        match cx.size {
            Size::S => {
                if self.online {
                    value_unit(cx, c.x, c.y + 22.0, 46.0, d, 40.0, 50.0);
                    let (n, un) = fmt_rate_parts(u as f64, lang);
                    cx.text(Rect::new(c.x + 20.0, c.y + 70.0, c.w - 20.0, 20.0), &format!("{n} {un}"), Style::new(14.0, 500).mono().ellipsis(), TEXT);
                } else {
                    cx.text(Rect::new(c.x, c.y + 22.0, c.w, 46.0), "–", Style::new(40.0, 300), FAINT);
                    cx.text(Rect::new(c.x + 20.0, c.y + 70.0, c.w - 20.0, 20.0), cx.t("Нет подключения", "Offline"), Style::BODY.ellipsis(), DIM);
                }
                let r = Rect::new(c.x, c.y + 96.0, c.w, 42.0);
                let hi = scale_of(&self.down, Some(&self.up));
                self.down.draw(cx, r, 0.0, hi, accent());
                self.up.draw(cx, r, 0.0, hi, UP);
            }
            Size::M => {
                for i in 0..2 {
                    let lane = m_lane(i);
                    let (h, v, color) = if i == 0 { (&self.down, d, accent()) } else { (&self.up, u, UP) };
                    if self.online {
                        value_unit(cx, lane.x, lane.y + 14.0, lane.h - 14.0, v, 28.0, 50.0);
                    } else {
                        cx.text(Rect::new(lane.x, lane.y + 14.0, 110.0, lane.h - 14.0), "–", Style::new(28.0, 300), FAINT);
                    }
                    h.draw(cx, Rect::new(lane.x + 128.0, lane.y + 5.0, lane.w - 132.0, lane.h - 10.0), 0.0, scale_of(h, None), color);
                }
                let cap = Rect::new(c.x, c.bottom() - 14.0, c.w, 14.0);
                let text = if self.online { format!("{}: ↓ {}   ↑ {}", cx.t("С запуска", "Since start"), fmt_size(self.total_rx, lang), fmt_size(self.total_tx, lang)) } else { cx.t("Нет подключения", "No connection").to_string() };
                cx.text(cap, &text, Style::CAPTION.ellipsis(), DIM);
            }
            Size::L => {}
        }
        Ok(())
    }

    fn tick(&mut self, cx: &Cx) -> Tick {
        if cx.preview {
            self.init_preview(cx);
            return Tick::Never;
        }
        if let Some(j) = &self.job {
            match j.take() {
                Some((r, reading)) => {
                    self.reader = Some(r);
                    self.job = None;
                    if let Some(x) = reading {
                        self.apply(x);
                    }
                    cx.request_frame();
                }
                None if !j.busy() => {
                    self.reader = Some(Reader::default()); // the worker died: start over
                    self.job = None;
                }
                None => {}
            }
        }
        if self.job.is_none() && self.spawned.map_or(true, |t| t.elapsed() >= Duration::from_millis(900)) {
            if let Some(mut r) = self.reader.take() {
                self.spawned = Some(Instant::now());
                self.job = Some(cx.spawn(move || {
                    let x = r.read();
                    (r, x)
                }));
            }
        }
        Tick::Every(Duration::from_secs(1)) // the speeds live in `overlay`: see sysmon, `Poll` would lose them on the next recompose
    }
}

/// M: lane `i` (0 = down, 1 = up): the left value column + the graph panel.
fn m_lane(i: usize) -> Rect {
    let area = Rect::new(16.0, 16.0, 324.0, 138.0 - 20.0);
    area.row(i as u32, 2, 8.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(luid: u64, up: bool, typ: u32, hw: bool, rx: u64, tx: u64) -> Row {
        Row { luid, up, typ, hw, rx, tx }
    }

    #[test]
    fn selects_physical_only_when_available() {
        let rows = [row(1, true, 6, true, 0, 0), row(2, true, 6, false, 0, 0), row(3, true, IF_LOOPBACK, false, 0, 0), row(4, false, 6, true, 0, 0), row(5, true, IF_TUNNEL, false, 0, 0)];
        let s = select(&rows);
        assert_eq!(s.iter().map(|r| r.luid).collect::<Vec<_>>(), vec![1]);
        // no physical adapter up: virtual ones are used
        let rows = [row(2, true, 6, false, 0, 0), row(3, true, IF_LOOPBACK, false, 0, 0)];
        assert_eq!(select(&rows).len(), 1);
        assert!(select(&[row(1, false, 6, true, 0, 0)]).is_empty());
    }

    #[test]
    fn deltas_ignore_new_and_reset_interfaces() {
        let mut prev = HashMap::new();
        assert_eq!(step(&mut prev, &[row(1, true, 6, true, 1000, 100)]), (0, 0)); // baseline
        assert_eq!(step(&mut prev, &[row(1, true, 6, true, 1600, 150)]), (600, 50));
        // a second adapter appears with a big lifetime counter: no spike
        assert_eq!(step(&mut prev, &[row(1, true, 6, true, 1700, 160), row(2, true, 6, true, 9_000_000, 9_000)]), (100, 10));
        // counter reset on adapter 1: counts as zero
        assert_eq!(step(&mut prev, &[row(1, true, 6, true, 10, 5), row(2, true, 6, true, 9_000_500, 9_100)]), (500, 100));
        // adapter 2 vanishes and returns: counted from zero again
        assert_eq!(step(&mut prev, &[row(1, true, 6, true, 20, 5)]), (10, 0));
        assert_eq!(step(&mut prev, &[row(1, true, 6, true, 20, 5), row(2, true, 6, true, 1, 1)]), (0, 0));
    }

    /// `cargo test network_live -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn network_live() {
        let mut r = Reader::default();
        let t = Instant::now();
        let first = r.read();
        println!("first read {:?}, {} interfaces, {} selected, {:?}", t.elapsed(), r.rows.len(), select(&r.rows).len(), first.map(|x| x.1));
        for x in r.rows.iter().filter(|x| x.up && x.typ != IF_LOOPBACK && x.typ != IF_TUNNEL && x.hw) {
            println!("  counted: luid {:x} type {} rx {} tx {}", x.luid, x.typ, x.rx, x.tx);
        }
        std::thread::sleep(Duration::from_millis(1000));
        let t = Instant::now();
        let second = r.read();
        println!("second read {:?}: {:?}", t.elapsed(), second.map(|x| (x.2, x.3)));
    }
}
