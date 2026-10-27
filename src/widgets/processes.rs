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
    /// `view` plus the next `HIDDEN` ranks: they wait below the card and rows glide through them when the order changes
    hid: Vec<Entry>,
    /// the sort `view` was built for (the numbers' unit while a sort change is still gliding)
    view_ram: bool,
    ready: bool,
    ver: u64,
    /// per process (lower-case name): "y:" row slot, "a:" opacity, "f:" bar fraction, "n:" number (CPU % or RAM bytes)
    glide: GlideAnim,
    /// per process: "f:" bar fraction, "n:" number (these follow their value, they are not moves)
    vals: GlideAnim,
    /// rows that left the top list: drawn sliding down and fading out until they are gone
    ghosts: Vec<Ghost>,
}

/// A row on its way out: what it showed when it left.
struct Ghost {
    key: String,
    label: String,
    text: String,
}

/// Which names enter and which leave between two row lists (lower-case keys).
fn diff_rows(old: &[String], new: &[String]) -> (Vec<String>, Vec<String>) {
    let entering = new.iter().filter(|k| !old.contains(k)).cloned().collect();
    let leaving = old.iter().filter(|k| !new.contains(k)).cloned().collect();
    (entering, leaving)
}

/// `sorted` (best first) re-ordered so that processes of nearly equal load keep the order they had in `prev`: a row only overtakes another
/// when it is clearly bigger (25 % and a real difference), so the low rows no longer trade places at every sample.
fn sticky(mut sorted: Vec<Entry>, prev: &[String], by_ram: bool) -> Vec<Entry> {
    let pos = |e: &Entry| prev.iter().position(|k| *k == rkey(e));
    let eps = if by_ram { (64u64 << 20) as f32 } else { 0.3 };
    for _ in 0..sorted.len() {
        let mut swapped = false;
        for i in 1..sorted.len() {
            let (a, b) = (&sorted[i - 1], &sorted[i]);
            if let (Some(pa), Some(pb)) = (pos(a), pos(b)) {
                if pa > pb && metric(a, by_ram) <= metric(b, by_ram) * 1.25 + eps {
                    sorted.swap(i - 1, i); // `b` was above `a` and `a` did not clearly overtake it
                    swapped = true;
                }
            }
        }
        if !swapped {
            break;
        }
    }
    sorted
}

/// Ranks kept below the card: a row that falls out of the top slides down through them instead of vanishing on the spot.
const HIDDEN: usize = 3;

/// Opacity of a row at the (fractional) slot `y` on a card of `rows` slots: full on the card, fading out over the bottom 0.7 of the last slot.
fn edge_fade(y: f32, rows: f32) -> f32 {
    let last = rows - 1.0;
    if y <= last {
        1.0
    } else {
        1.0 - crate::motion::smooth((y - last) / 0.7)
    }
}

fn rkey(e: &Entry) -> String {
    e.name.to_lowercase()
}
fn label(e: &Entry) -> String {
    if e.count > 1 {
        format!("{} ×{}", e.name, e.count)
    } else {
        e.name.clone()
    }
}
fn metric(e: &Entry, by_ram: bool) -> f32 {
    if by_ram {
        e.ram as f32
    } else {
        e.cpu
    }
}
/// The number column of a row: `v` is CPU % or RAM bytes.
fn cell_text(v: f32, by_ram: bool, lang: Lang) -> String {
    if by_ram {
        fmt_size(v.max(0.0) as u64, lang)
    } else {
        fmt_cpu(v)
    }
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
    /// The longest bar's value: the bars are relative to it (so a change of it moves every bar; the fractions glide).
    fn peak(&self) -> f32 {
        self.view.iter().map(|e| metric(e, self.by_ram)).fold(0.0, f32::max).max(if self.by_ram { 1.0 } else { 5.0 })
    }
    /// New order: every row glides to its rank (the ranks below the card are real slots too, so a process that drops out of the top
    /// slides down and fades at the bottom edge, one that climbs in slides up from below). A process that vanished entirely becomes a ghost.
    fn rebuild(&mut self, cx: &Cx) {
        let rows = self.rows(cx);
        let old = std::mem::take(&mut self.hid);
        let old_k: Vec<String> = old.iter().map(rkey).collect();
        // the unit changed (CPU <-> RAM): the old order means nothing for the new one
        let prev = if self.view_ram == self.by_ram { old_k.clone() } else { Vec::new() };
        self.hid = sticky(top(&self.list, self.by_ram, rows + HIDDEN + 4), &prev, self.by_ram);
        self.hid.truncate(rows + HIDDEN);
        self.view = self.hid.iter().take(rows).cloned().collect();
        let new_k: Vec<String> = self.hid.iter().map(rkey).collect();
        let (entering, leaving) = diff_rows(&old_k, &new_k);
        let anim = !cx.preview && !reduced_motion();
        if anim {
            for e in old.iter().take(rows).filter(|e| leaving.contains(&rkey(e))) {
                let k = rkey(e);
                let text = cell_text(self.vals.or(&format!("n:{k}"), metric(e, self.view_ram)), self.view_ram, cx.lang);
                self.ghosts.retain(|g| g.key != k);
                self.ghosts.push(Ghost { key: k.clone(), label: label(e), text });
                self.glide.set(&format!("y:{k}"), (rows + HIDDEN) as f32, cx);
            }
        }
        if self.view_ram != self.by_ram {
            self.vals.retain(|k| !k.starts_with("n:")); // CPU % and RAM bytes do not glide into each other: the numbers start at the new unit
            self.view_ram = self.by_ram;
        }
        let peak = self.peak();
        for (i, e) in self.hid.iter().enumerate() {
            let k = rkey(e);
            self.ghosts.retain(|g| g.key != k); // a row that came back
            let (y, a) = (format!("y:{k}"), format!("a:{k}"));
            if anim && !old.is_empty() && entering.contains(&k) && i < rows {
                self.glide.set_from(&y, i as f32 + 1.0, i as f32, cx); // a brand new process: up from below, fading in
                self.glide.set_from(&a, 0.0, 1.0, cx);
            } else {
                self.glide.set(&y, i as f32, cx);
                self.glide.set(&a, 1.0, cx);
            }
            let m = metric(e, self.by_ram);
            self.vals.set(&format!("f:{k}"), m / peak, cx);
            self.vals.set(&format!("n:{k}"), m, cx);
        }
    }
    /// Drop what has finished: ghosts that slid below the card, glides of processes that are no longer tracked.
    fn tidy(&mut self, busy: bool, rows: usize) {
        if busy {
            let g = &self.glide;
            self.ghosts.retain(|x| g.or(&format!("y:{}", x.key), 0.0) < rows as f32);
        } else {
            self.ghosts.clear();
        }
        let keep: Vec<String> = self.hid.iter().map(rkey).chain(self.ghosts.iter().map(|g| g.key.clone())).collect();
        self.glide.retain(|k| k.split_once(':').is_some_and(|(_, n)| keep.iter().any(|x| x == n)));
        self.vals.retain(|k| k.split_once(':').is_some_and(|(_, n)| keep.iter().any(|x| x == n)));
    }
    fn start(&mut self, cx: &Cx) {
        if self.started {
            return;
        }
        self.started = true;
        self.glide = GlideAnim::smooth(0.6); // rows change places: a soft start and end over 0.6 s, not the quick exponential of a bar
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
        Self::row_at(cx, i as f32)
    }
    /// Row at the (fractional, while it glides) slot `i`.
    fn row_at(cx: &Cx, i: f32) -> Rect {
        let c = cx.content();
        let (top, rows) = if cx.size == Size::L { (c.y + 30.0, 8.0) } else { (c.y + 22.0, 4.0) };
        let h = (c.bottom() - top) / rows;
        Rect::new(c.x, top + i * h, c.w, h)
    }
    /// One row: name, relative bar, value.
    fn paint_row(cx: &Cx, r: Rect, name: &str, frac: f32, text: &str, color: Color) {
        cx.text(Rect::new(r.x, r.y, 128.0, r.h), name, Style::BODY.ellipsis(), TEXT);
        cx.bar(Rect::new(r.x + 136.0, r.cy() - 3.0, r.w - 136.0 - 70.0, 6.0), frac, color);
        cx.text(Rect::new(r.right() - 62.0, r.y, 62.0, r.h), text, Style::MONO.right(), DIM);
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
        let peak = self.peak();
        let color = if self.by_ram { TEAL } else { accent() };
        let (g, v) = (&self.glide, &self.vals);
        let rows = self.rows(cx) as f32;
        for (i, e) in self.hid.iter().enumerate() {
            let k = rkey(e);
            let y = g.or(&format!("y:{k}"), i as f32);
            let a = g.or(&format!("a:{k}"), 1.0) * edge_fade(y, rows);
            if a <= 0.0 {
                continue; // waiting below the card
            }
            let m = metric(e, self.by_ram);
            let text = cell_text(v.or(&format!("n:{k}"), m), self.by_ram, lang);
            let r = Self::row_at(cx, y);
            cx.with_opacity(a, |c| Self::paint_row(c, r, &label(e), v.or(&format!("f:{k}"), m / peak), &text, color));
        }
        // processes that left the list entirely: sliding down and fading out at the bottom edge
        for x in &self.ghosts {
            let y = g.or(&format!("y:{}", x.key), 0.0);
            let a = edge_fade(y, rows);
            if a > 0.0 {
                let r = Self::row_at(cx, y);
                cx.with_opacity(a, |c| Self::paint_row(c, r, &x.label, v.or(&format!("f:{}", x.key), 0.0), &x.text, color));
            }
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
        let (moving, changing) = (self.glide.step(cx), self.vals.step(cx));
        let busy = moving || changing;
        let rows = self.rows(cx);
        self.tidy(busy, rows);
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
        if busy {
            return Tick::Fps(60); // rows glide to their new slots, bars and numbers to their new values
        }
        Tick::Every(if self.samples < 2 { Duration::from_millis(600) } else { Duration::from_secs(2) })
    }

    fn mouse(&mut self, ev: &MouseEv, cx: &mut Cx) -> bool {
        let (_, _, _, strip) = Self::header(cx);
        if ev.clicked(strip) {
            self.by_ram = !self.by_ram;
            self.rebuild(cx);
            cx.waker().wake(); // a click does not call `tick`: poke it so the rows start to glide now
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
    fn rows_enter_and_leave() {
        let k = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let (enter, leave) = diff_rows(&k(&["a", "b", "c"]), &k(&["b", "d", "a"]));
        assert_eq!((enter, leave), (k(&["d"]), k(&["c"])));
        assert_eq!(diff_rows(&[], &k(&["a"])), (k(&["a"]), vec![])); // the very first list: all "enter" (the caller skips the animation then)
        assert_eq!(diff_rows(&k(&["a"]), &k(&["a"])), (vec![], vec![]));
    }

    #[test]
    fn reordered_rows_glide_between_their_slots() {
        use crate::motion::Glide;
        let mut g = Glide::default();
        g.set("y:a", 0.0);
        g.set("y:b", 1.0);
        g.set("y:a", 1.0); // a and b swap
        g.set("y:b", 0.0);
        g.set_from("y:new", 4.7, 3.0); // slides in from below its slot
        g.set("y:gone", 2.0);
        g.set("y:gone", 2.7); // slides down
        g.step(0.06);
        let (a, b, n, o) = (g.get("y:a").unwrap(), g.get("y:b").unwrap(), g.get("y:new").unwrap(), g.get("y:gone").unwrap());
        assert!(a > 0.0 && a < 1.0 && b > 0.0 && b < 1.0 && (a + b - 1.0).abs() < 1e-4);
        assert!(n < 4.7 && n > 3.0 && o > 2.0 && o < 2.7);
        while g.step(0.016) {}
        assert_eq!((g.get("y:a"), g.get("y:b"), g.get("y:new")), (Some(1.0), Some(0.0), Some(3.0)));
    }

    #[test]
    fn near_equal_rows_keep_their_order() {
        let e = |n: &str, cpu: f32| Entry { name: n.into(), count: 1, cpu, ram: 0 };
        let prev: Vec<String> = ["a", "b", "c"].iter().map(|s| s.to_string()).collect();
        let names = |v: Vec<Entry>| v.iter().map(|x| x.name.clone()).collect::<Vec<_>>();
        // b and c are a hair above their predecessors: nothing moves
        assert_eq!(names(sticky(vec![e("b", 3.1), e("a", 3.0), e("c", 0.2)], &prev, false)), ["a", "b", "c"]);
        // a clear overtake goes through, and a new process is never held back
        assert_eq!(names(sticky(vec![e("c", 9.0), e("a", 3.0), e("b", 2.0)], &prev, false)), ["c", "a", "b"]);
        assert_eq!(names(sticky(vec![e("n", 8.0), e("a", 3.0)], &prev, false)), ["n", "a"]);
    }

    #[test]
    fn rows_fade_out_over_the_bottom_edge() {
        assert_eq!((edge_fade(0.0, 4.0), edge_fade(3.0, 4.0)), (1.0, 1.0)); // on the card
        let mid = edge_fade(3.35, 4.0);
        assert!(mid > 0.2 && mid < 0.8, "{mid}");
        assert_eq!((edge_fade(3.7, 4.0), edge_fade(6.0, 4.0)), (0.0, 0.0)); // below it
    }

    #[test]
    fn cell_text_units() {
        assert_eq!(cell_text(24.6, false, Lang::En), "25%");
        assert_eq!(cell_text(3.14, false, Lang::En), "3,1%");
        assert_eq!(cell_text(2.0 * 1073741824.0, true, Lang::En), fmt_size(2 * 1073741824, Lang::En));
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
