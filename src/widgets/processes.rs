//! Processes: the busiest programs by CPU or by RAM. M = 4 rows, L = 8 rows (name, relative bar, value). A click on the header
//! toggles CPU / RAM and is remembered in `cfg.sort` ("cpu" | "ram").
//! Data (read-only, worker thread, every 2 s): a Toolhelp snapshot, `OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION)`, `GetProcessTimes`
//! (CPU deltas, normalised by the logical CPU count so 100 % = the whole machine) and `GetProcessMemoryInfo` (working set).
//! Processes that cannot be opened are skipped; idle / system pseudo processes are left out; processes with the same exe name are
//! merged ("chrome x14") because that is what the user wants to see. Static header in `draw`, the rows in `overlay`.
use super::sysmon::fmt_size;
use crate::registry::{Registry, WidgetDef};
use crate::widget::*;
use std::collections::HashMap;
use std::time::Instant;
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::Foundation::FILETIME;
use windows::Win32::System::Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS};
use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
use windows::Win32::System::Threading::{GetActiveProcessorCount, GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};

pub fn register(r: &mut Registry) {
    r.register(WidgetDef {
        type_id: "processes",
        name: ("Процессы", "Processes"),
        desc: ("Самые нагруженные процессы", "The busiest processes"),
        group: "system",
        icon: Glyph::Ram,
        sizes: &[Size::M, Size::L],
        default_size: Size::M,
        make: || Box::new(Processes::default()),
    });
}

/// Pseudo processes that are not programs (compared case-insensitively, without ".exe").
const SKIP: [&str; 6] = ["system idle process", "[system process]", "system", "registry", "memory compression", "secure system"];

#[derive(Clone, Debug, PartialEq)]
struct Entry {
    name: String,
    count: u32,
    /// % of the whole machine (all logical CPUs)
    cpu: f32,
    ram: u64,
}

/// "chrome.exe" -> "chrome".
fn display_name(exe: &str) -> &str {
    let n = exe.len();
    if n > 4 && exe.is_char_boundary(n - 4) && exe[n - 4..].eq_ignore_ascii_case(".exe") {
        &exe[..n - 4]
    } else {
        exe
    }
}
fn skipped(name: &str) -> bool {
    SKIP.iter().any(|s| name.eq_ignore_ascii_case(s))
}

/// Process CPU share in % of the machine for `delta` 100-ns ticks of CPU time over `wall_secs` on `ncpu` logical CPUs.
fn cpu_share(delta: u64, wall_secs: f64, ncpu: u32) -> f32 {
    if wall_secs <= 0.0 || ncpu == 0 {
        return 0.0;
    }
    (delta as f64 / (wall_secs * 1e7 * ncpu as f64) * 100.0).clamp(0.0, 100.0) as f32
}

/// Merge per-process rows by exe name (case-insensitive); the first spelling wins.
fn merge(rows: impl Iterator<Item = (String, f32, u64)>) -> Vec<Entry> {
    let mut idx: HashMap<String, usize> = HashMap::new();
    let mut out: Vec<Entry> = Vec::new();
    for (name, cpu, ram) in rows {
        match idx.get(&name.to_lowercase()) {
            Some(&i) => {
                out[i].count += 1;
                out[i].cpu = (out[i].cpu + cpu).min(100.0);
                out[i].ram += ram;
            }
            None => {
                idx.insert(name.to_lowercase(), out.len());
                out.push(Entry { name, count: 1, cpu, ram });
            }
        }
    }
    out
}

/// The `n` top entries by CPU (or RAM when `by_ram`), ties broken by the other metric.
fn top(list: &[Entry], by_ram: bool, n: usize) -> Vec<Entry> {
    let mut v: Vec<Entry> = list.to_vec();
    if by_ram {
        v.sort_by(|a, b| b.ram.cmp(&a.ram).then(b.cpu.total_cmp(&a.cpu)));
    } else {
        v.sort_by(|a, b| b.cpu.total_cmp(&a.cpu).then(b.ram.cmp(&a.ram)));
    }
    v.truncate(n);
    v
}

fn ft(a: FILETIME, b: FILETIME) -> u64 {
    ((a.dwHighDateTime as u64) << 32 | a.dwLowDateTime as u64) + ((b.dwHighDateTime as u64) << 32 | b.dwLowDateTime as u64)
}

/// Worker-side state: the previous CPU times per pid.
#[derive(Default)]
struct Sampler {
    prev: HashMap<u32, u64>,
    next: HashMap<u32, u64>,
    last: Option<Instant>,
    rows: Vec<(String, f32, u64)>,
}
struct Snapshot {
    list: Vec<Entry>,
    /// CPU numbers are meaningful (a previous reading existed)
    ready: bool,
}

impl Sampler {
    fn sample(&mut self) -> Snapshot {
        let now = Instant::now();
        let wall = self.last.map(|t| now.duration_since(t).as_secs_f64());
        let ncpu = unsafe { GetActiveProcessorCount(0xFFFF) };
        self.rows.clear();
        self.next.clear();
        unsafe {
            if let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) {
                let mut e = PROCESSENTRY32W { dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
                let mut ok = Process32FirstW(snap, &mut e).is_ok();
                while ok {
                    let pid = e.th32ProcessID;
                    if pid > 4 {
                        let end = e.szExeFile.iter().position(|&c| c == 0).unwrap_or(e.szExeFile.len());
                        let exe = String::from_utf16_lossy(&e.szExeFile[..end]);
                        let name = display_name(&exe);
                        if !skipped(name) {
                            if let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) {
                                let (mut c, mut x, mut k, mut u) = (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
                                if GetProcessTimes(h, &mut c, &mut x, &mut k, &mut u).is_ok() {
                                    let cpu_time = ft(k, u);
                                    let mut m = PROCESS_MEMORY_COUNTERS { cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32, ..Default::default() };
                                    let ram = if GetProcessMemoryInfo(h, &mut m, m.cb).is_ok() { m.WorkingSetSize as u64 } else { 0 };
                                    let share = match (wall, self.prev.get(&pid)) {
                                        (Some(w), Some(&p)) => cpu_share(cpu_time.saturating_sub(p), w, ncpu),
                                        _ => 0.0,
                                    };
                                    self.next.insert(pid, cpu_time);
                                    self.rows.push((name.to_string(), share, ram));
                                }
                                let _ = CloseHandle(h);
                            }
                        }
                    }
                    ok = Process32NextW(snap, &mut e).is_ok();
                }
                let _ = CloseHandle(snap);
            }
        }
        std::mem::swap(&mut self.prev, &mut self.next);
        self.last = Some(now);
        Snapshot { list: merge(self.rows.drain(..)), ready: wall.is_some() }
    }
}

#[derive(Default)]
struct Processes {
    sampler: Option<Sampler>,
    job: Option<Pending<(Sampler, Snapshot)>>,
    last: Option<Instant>,
    samples: u32,
    started: bool,
    by_ram: bool,
    /// all merged entries of the last snapshot
    list: Vec<Entry>,
    /// the rows on screen for the current sort
    view: Vec<Entry>,
    ready: bool,
    ver: u64,
}

fn sample_list() -> Vec<Entry> {
    const MB: u64 = 1 << 20;
    let e = |name: &str, count, cpu, ram_mb: u64| Entry { name: name.into(), count, cpu, ram: ram_mb * MB };
    vec![
        e("ollama", 2, 24.1, 2450),
        e("chrome", 14, 11.8, 3120),
        e("vmmem", 1, 6.2, 6100),
        e("Code", 7, 4.4, 1830),
        e("dwm", 1, 2.7, 410),
        e("explorer", 1, 1.9, 380),
        e("Discord", 5, 1.4, 940),
        e("svchost", 58, 1.1, 720),
        e("Spotify", 4, 0.8, 560),
        e("steamwebhelper", 6, 0.5, 690),
        e("OneDrive", 1, 0.2, 210),
    ]
}

impl Processes {
    fn rows(&self, cx: &Cx) -> usize {
        if cx.size == Size::L {
            8
        } else {
            4
        }
    }
    fn rebuild(&mut self, cx: &Cx) {
        self.view = top(&self.list, self.by_ram, self.rows(cx));
    }
    fn start(&mut self, cx: &Cx) {
        if self.started {
            return;
        }
        self.started = true;
        self.by_ram = cx.cfg_str("sort") == Some("ram");
        if cx.preview {
            self.list = sample_list();
            self.ready = true;
            self.rebuild(cx);
            self.ver += 1;
        } else {
            self.sampler = Some(Sampler::default());
        }
    }
    /// Header geometry: (title, CPU segment, RAM segment, clickable strip).
    fn header(cx: &Cx) -> (Rect, Rect, Rect, Rect) {
        let c = cx.content();
        let (seg_w, seg_h) = (42.0, 20.0);
        let y = c.y - 1.0;
        (Rect::new(c.x, c.y, 150.0, 18.0), Rect::new(c.right() - 2.0 * seg_w - 2.0, y, seg_w, seg_h), Rect::new(c.right() - seg_w, y, seg_w, seg_h), Rect::new(0.0, 0.0, cx.w, c.y + 26.0))
    }
    /// Row `i`: y and height.
    fn row(cx: &Cx, i: usize) -> Rect {
        let c = cx.content();
        let (top, rows) = if cx.size == Size::L { (c.y + 30.0, 8.0) } else { (c.y + 22.0, 4.0) };
        let h = (c.bottom() - top) / rows;
        Rect::new(c.x, top + i as f32 * h, c.w, h)
    }
}

fn fmt_cpu(v: f32) -> String {
    if v < 0.05 {
        "0%".to_string()
    } else if v < 10.0 {
        format!("{:.1}%", v).replace('.', ",")
    } else {
        format!("{}%", v.round() as i32)
    }
}

impl Widget for Processes {
    fn draw(&mut self, cx: &mut Cx) -> Result<()> {
        self.start(cx);
        let (title, cpu, ram, _) = Self::header(cx);
        cx.text(title, &cx.t("ПРОЦЕССЫ", "PROCESSES"), Style::LABEL, accent());
        let seg = Rect::new(cpu.x, cpu.y, cpu.w + ram.w + 2.0, cpu.h);
        cx.rrect(seg, 10.0, Color::rgba(1.0, 1.0, 1.0, 0.10));
        cx.rrect(if self.by_ram { ram } else { cpu }.inset_xy(0.0, 1.5), 8.5, Color::rgba(1.0, 1.0, 1.0, 0.22));
        let st = Style::new(11.0, 600).center();
        cx.text(cpu, "CPU", st, if self.by_ram { DIM } else { TEXT });
        cx.text(ram, cx.t("ОЗУ", "RAM"), st, if self.by_ram { TEXT } else { DIM });
        if cx.size == Size::L {
            for i in 1..self.rows(cx) {
                let r = Self::row(cx, i);
                cx.rect(Rect::new(r.x, r.y, r.w, 0.6), FILL);
            }
        }
        Ok(())
    }

    fn overlay(&mut self, cx: &mut Cx) -> Result<()> {
        self.start(cx);
        let lang = cx.lang;
        if self.view.is_empty() {
            let msg = if self.ready { cx.t("Нет данных", "No data") } else { cx.t("Загрузка…", "Loading…") };
            cx.text(cx.content().cut_top(30.0).1, msg, Style::BODY.center(), DIM);
            return Ok(());
        }
        let peak = self.view.iter().map(|e| if self.by_ram { e.ram as f64 } else { e.cpu as f64 }).fold(0.0, f64::max).max(if self.by_ram { 1.0 } else { 5.0 });
        let color = if self.by_ram { TEAL } else { accent() };
        for (i, e) in self.view.iter().enumerate() {
            let r = Self::row(cx, i);
            let name = if e.count > 1 { format!("{} ×{}", e.name, e.count) } else { e.name.clone() };
            let (v, frac, text) = if self.by_ram { (e.ram as f64, e.ram as f64 / peak, fmt_size(e.ram, lang)) } else { (e.cpu as f64, e.cpu as f64 / peak, fmt_cpu(e.cpu)) };
            let _ = v;
            cx.text(Rect::new(r.x, r.y, 128.0, r.h), &name, Style::BODY.ellipsis(), TEXT);
            cx.bar(Rect::new(r.x + 136.0, r.cy() - 3.0, r.w - 136.0 - 70.0, 6.0), frac as f32, color);
            cx.text(Rect::new(r.right() - 62.0, r.y, 62.0, r.h), &text, Style::MONO.right(), DIM);
        }
        Ok(())
    }

    fn tick(&mut self, cx: &Cx) -> Tick {
        self.start(cx);
        if cx.preview {
            return Tick::Never;
        }
        if let Some(j) = &self.job {
            match j.take() {
                Some((s, snap)) => {
                    self.sampler = Some(s);
                    self.job = None;
                    self.samples += 1;
                    self.ready = snap.ready;
                    if snap.ready {
                        self.list = snap.list;
                        self.rebuild(cx);
                    }
                    cx.request_frame();
                }
                None if !j.busy() => {
                    self.sampler = Some(Sampler::default());
                    self.job = None;
                }
                None => {}
            }
        }
        let gap = if self.samples < 2 { Duration::from_millis(500) } else { Duration::from_millis(1900) };
        if self.job.is_none() && self.last.map_or(true, |t| t.elapsed() >= gap) {
            if let Some(mut s) = self.sampler.take() {
                self.last = Some(Instant::now());
                self.job = Some(cx.spawn(move || {
                    let snap = s.sample();
                    (s, snap)
                }));
            }
        }
        Tick::Every(if self.samples < 2 { Duration::from_millis(600) } else { Duration::from_secs(2) })
    }

    fn mouse(&mut self, ev: &MouseEv, cx: &mut Cx) -> bool {
        let (_, _, _, strip) = Self::header(cx);
        if ev.clicked(strip) {
            self.by_ram = !self.by_ram;
            self.rebuild(cx);
            self.ver += 1;
            cx.cfg_set("sort", json!(if self.by_ram { "ram" } else { "cpu" }));
            return true;
        }
        if ev.clicked(cx.bounds()) {
            cx.open_card_target("processes"); // the rest of the card: Task Manager
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
    fn names() {
        assert_eq!(display_name("chrome.exe"), "chrome");
        assert_eq!(display_name("Code.EXE"), "Code");
        assert_eq!(display_name(".exe"), ".exe");
        assert_eq!(display_name("vmmem"), "vmmem");
        assert!(skipped("System Idle Process") && skipped("Registry") && !skipped("System32Tool"));
    }

    #[test]
    fn cpu_normalised_by_cpu_count() {
        // one fully busy core for 2 s on a 20-thread machine = 5 %
        assert!((cpu_share(2 * 10_000_000, 2.0, 20) - 5.0).abs() < 1e-4);
        assert_eq!(cpu_share(5, 0.0, 8), 0.0);
        assert_eq!(cpu_share(u64::MAX, 1.0, 1), 100.0);
    }

    #[test]
    fn merges_and_sorts() {
        let rows = vec![("chrome".to_string(), 3.0, 100u64), ("Code".to_string(), 1.0, 900), ("Chrome".to_string(), 2.0, 300)];
        let list = merge(rows.into_iter());
        assert_eq!(list.len(), 2);
        let c = list.iter().find(|e| e.name == "chrome").unwrap();
        assert_eq!((c.count, c.cpu, c.ram), (2, 5.0, 400));
        assert_eq!(top(&list, false, 1)[0].name, "chrome");
        assert_eq!(top(&list, true, 1)[0].name, "Code");
        assert_eq!(top(&list, true, 8).len(), 2);
    }

    #[test]
    fn cpu_text() {
        assert_eq!(fmt_cpu(0.0), "0%");
        assert_eq!(fmt_cpu(3.14), "3,1%");
        assert_eq!(fmt_cpu(24.6), "25%");
    }

    /// `cargo test processes_live -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn processes_live() {
        let mut s = Sampler::default();
        let t = Instant::now();
        let a = s.sample();
        println!("first sample {:?}: {} groups, ready {}", t.elapsed(), a.list.len(), a.ready);
        std::thread::sleep(Duration::from_millis(1500));
        let t = Instant::now();
        let b = s.sample();
        println!("second sample {:?}: {} groups, ready {}", t.elapsed(), b.list.len(), b.ready);
        for e in top(&b.list, false, 5) {
            println!("  CPU {:>5.1}%  {} x{}  {}", e.cpu, e.name, e.count, fmt_size(e.ram, Lang::En));
        }
        for e in top(&b.list, true, 5) {
            println!("  RAM {:>9}  {} x{}  CPU {:.1}%", fmt_size(e.ram, Lang::En), e.name, e.count, e.cpu);
        }
        assert!(!b.list.is_empty());
    }
}
