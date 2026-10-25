//! System monitor: S three ring gauges (CPU / RAM / GPU); M three rows with a 60-sample sparkline; L three graphs + GPU details
//! (temperature, VRAM bar, power) and the RAM used/total line. Up to 2 NVIDIA GPUs through NVML (loaded dynamically, no NVIDIA = no GPU rows).
//!
//! Sampling happens on a worker thread once per second (`Tick::Every(1 s)` only while the card is visible): CPU from `GetSystemTimes`
//! deltas, RAM from `GlobalMemoryStatusEx`, GPUs from `nvml.dll`. Static labels live in `draw`; everything that changes (values, rings,
//! graphs) is painted in `overlay`, so the glass layer is never re-rendered by a sample.
//! Previews use deterministic sample data and do no I/O; `cfg.sample_gpus` (0..2) and `cfg.sample_hot` (bool) only affect previews.
//!
//! This file also hosts the small shared helpers of the "system" group (`pub(crate)`): colour ramps, byte/rate formatters, `Hist`.
use crate::registry::{Registry, WidgetDef};
use crate::widget::*;
use std::time::Instant;
use windows::core::{w, PCSTR};
use windows::Win32::Foundation::{FILETIME, HANDLE, HMODULE};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryExW, LoadLibraryW, LOAD_LIBRARY_SEARCH_SYSTEM32};
use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
use windows::Win32::System::Threading::{GetActiveProcessorCount, GetSystemTimes};

pub fn register(r: &mut Registry) {
    r.register(WidgetDef {
        type_id: "sysmon",
        name: ("Система", "System"),
        desc: ("Загрузка процессора, памяти и видеокарты", "CPU, memory and GPU load"),
        group: "system",
        icon: Glyph::Cpu,
        sizes: &[Size::S, Size::M, Size::L],
        default_size: Size::M,
        make: || Box::new(Sysmon::new()),
    });
}

// ================================================================ shared helpers (system group)

/// Faint panel behind graphs.
pub(crate) const PANEL: Color = Color::rgba(1.0, 1.0, 1.0, 0.07);

/// The accent below `amber` (the system's accent colour, like Apple's rings and progress bars), amber from `amber`, red from `red`
/// (all in the same unit as `v`). Warnings keep their own colours.
pub(crate) fn level_color(v: f32, amber: f32, red: f32) -> Color {
    if v >= red {
        RED
    } else if v >= amber {
        ORANGE
    } else {
        accent()
    }
}
/// Load percentage ramp: accent, amber > 70 %, red > 90 %.
pub(crate) fn load_color(pct: f32) -> Color {
    level_color(pct, 70.0, 90.0)
}

const UNITS_RU: [&str; 5] = ["Б", "КБ", "МБ", "ГБ", "ТБ"];
const UNITS_EN: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
const RATES_RU: [&str; 3] = ["КБ/с", "МБ/с", "ГБ/с"];
const RATES_EN: [&str; 3] = ["KB/s", "MB/s", "GB/s"];

/// "12.4" / "412" (decimal comma in Russian). `dec` digits after the point.
fn num(v: f64, dec: usize, lang: Lang) -> String {
    let s = format!("{:.*}", dec, v);
    if lang == Lang::Ru {
        s.replace('.', ",")
    } else {
        s
    }
}
/// 1 decimal below 100, none above.
fn auto_dec(v: f64) -> usize {
    if v >= 99.95 {
        0
    } else {
        1
    }
}
/// Index of the 1024-based unit for `bytes` (0 B .. 4 TB); switches at 1000 so "1000 MB" never appears.
fn unit_index(bytes: u64) -> usize {
    let (mut u, mut v) = (0, bytes as f64);
    while v >= 1000.0 && u < 4 {
        v /= 1024.0;
        u += 1;
    }
    u
}
fn scaled(bytes: u64, unit: usize) -> f64 {
    bytes as f64 / 1024f64.powi(unit as i32)
}
fn unit_name(unit: usize, lang: Lang) -> &'static str {
    (if lang == Lang::Ru { &UNITS_RU } else { &UNITS_EN })[unit.min(4)]
}
/// ("412", "ГБ"): a byte count with an automatic unit.
pub(crate) fn fmt_size_parts(bytes: u64, lang: Lang) -> (String, &'static str) {
    let u = unit_index(bytes);
    let v = scaled(bytes, u);
    (num(v, if u == 0 { 0 } else { auto_dec(v) }, lang), unit_name(u, lang))
}
/// "412 ГБ".
pub(crate) fn fmt_size(bytes: u64, lang: Lang) -> String {
    let (n, u) = fmt_size_parts(bytes, lang);
    format!("{n} {u}")
}
/// "19.4 / 31.9 ГБ": both numbers in the unit of `total`.
pub(crate) fn fmt_pair(used: u64, total: u64, lang: Lang) -> String {
    let u = unit_index(total);
    let t = scaled(total, u);
    let d = if u == 0 { 0 } else { auto_dec(t) };
    format!("{} / {} {}", num(scaled(used, u), d, lang), num(t, d, lang), unit_name(u, lang))
}
/// ("12.4", "МБ/с") for a speed in bytes per second.
pub(crate) fn fmt_rate_parts(bps: f64, lang: Lang) -> (String, &'static str) {
    let kb = (bps.max(0.0)) / 1024.0;
    let (v, u) = if kb < 1000.0 {
        (kb, 0)
    } else if kb / 1024.0 < 1000.0 {
        (kb / 1024.0, 1)
    } else {
        (kb / 1048576.0, 2)
    };
    let n = if v < 0.05 { "0".to_string() } else { num(v, auto_dec(v), lang) };
    (n, (if lang == Lang::Ru { &RATES_RU } else { &RATES_EN })[u])
}

/// Fixed-length history (oldest first) drawn as a sparkline that grows in from the right edge.
#[derive(Clone)]
pub(crate) struct Hist {
    v: Vec<f32>,
    cap: usize,
}
impl Hist {
    pub(crate) fn new(cap: usize) -> Hist {
        Hist { v: Vec::with_capacity(cap), cap }
    }
    pub(crate) fn push(&mut self, x: f32) {
        if self.v.len() >= self.cap {
            self.v.remove(0);
        }
        self.v.push(x);
    }
    pub(crate) fn last(&self) -> Option<f32> {
        self.v.last().copied()
    }
    pub(crate) fn max(&self) -> f32 {
        self.v.iter().cloned().fold(0.0, f32::max)
    }
    /// A full history built from a deterministic function (previews).
    pub(crate) fn wave(cap: usize, f: impl Fn(usize) -> f32) -> Hist {
        let mut h = Hist::new(cap);
        for i in 0..cap {
            h.push(f(i));
        }
        h
    }
    pub(crate) fn draw(&self, cx: &Cx, r: Rect, lo: f32, hi: f32, color: Color) {
        let n = self.v.len();
        if n < 2 {
            return;
        }
        let w = r.w * (n - 1) as f32 / (self.cap - 1).max(1) as f32;
        cx.sparkline_range(Rect::new(r.right() - w, r.y, w, r.h), &self.v, lo, hi, color);
    }
}

// ================================================================ pure sampling maths

#[derive(Clone, Copy, Default, Debug, PartialEq)]
struct CpuTimes {
    idle: u64,
    kernel: u64,
    user: u64,
}
/// CPU busy % between two `GetSystemTimes` readings (kernel time includes idle time). None when no time has passed.
fn cpu_pct(a: CpuTimes, b: CpuTimes) -> Option<f32> {
    let idle = b.idle.wrapping_sub(a.idle);
    let total = b.kernel.wrapping_sub(a.kernel).wrapping_add(b.user.wrapping_sub(a.user));
    if total == 0 || total > u64::MAX / 2 {
        return None;
    }
    Some((total.saturating_sub(idle) as f64 / total as f64 * 100.0).clamp(0.0, 100.0) as f32)
}
fn ft(f: FILETIME) -> u64 {
    (f.dwHighDateTime as u64) << 32 | f.dwLowDateTime as u64
}
fn cpu_times() -> Option<CpuTimes> {
    let (mut i, mut k, mut u) = (FILETIME::default(), FILETIME::default(), FILETIME::default());
    unsafe { GetSystemTimes(Some(&mut i), Some(&mut k), Some(&mut u)).ok()? };
    Some(CpuTimes { idle: ft(i), kernel: ft(k), user: ft(u) })
}

// ================================================================ NVML (dynamic)

#[repr(C)]
#[derive(Default)]
struct NvmlUtil {
    gpu: u32,
    memory: u32,
}
#[repr(C)]
#[derive(Default)]
struct NvmlMem {
    total: u64,
    free: u64,
    used: u64,
}
type FnInit = unsafe extern "C" fn() -> i32;
type FnCount = unsafe extern "C" fn(*mut u32) -> i32;
type FnHandle = unsafe extern "C" fn(u32, *mut usize) -> i32;
type FnName = unsafe extern "C" fn(usize, *mut u8, u32) -> i32;
type FnUtil = unsafe extern "C" fn(usize, *mut NvmlUtil) -> i32;
type FnTemp = unsafe extern "C" fn(usize, u32, *mut u32) -> i32;
type FnMem = unsafe extern "C" fn(usize, *mut NvmlMem) -> i32;
type FnPower = unsafe extern "C" fn(usize, *mut u32) -> i32;

/// Resolved NVML entry points + the device handles. The library is never unloaded (process lifetime).
struct Nvml {
    util: FnUtil,
    temp: FnTemp,
    mem: FnMem,
    power: FnPower,
    devices: Vec<(usize, String)>,
}

unsafe fn sym<T: Copy>(h: HMODULE, name: &'static [u8]) -> Option<T> {
    let p = GetProcAddress(h, PCSTR(name.as_ptr()))?;
    Some(std::mem::transmute_copy(&p))
}

impl Nvml {
    fn load() -> Option<Nvml> {
        unsafe {
            let lib = LoadLibraryExW(w!("nvml.dll"), HANDLE::default(), LOAD_LIBRARY_SEARCH_SYSTEM32)
                .or_else(|_| LoadLibraryW(w!(r"C:\Program Files\NVIDIA Corporation\NVSMI\nvml.dll")))
                .ok()?;
            let init: FnInit = sym(lib, b"nvmlInit_v2\0")?;
            let count: FnCount = sym(lib, b"nvmlDeviceGetCount_v2\0")?;
            let handle: FnHandle = sym(lib, b"nvmlDeviceGetHandleByIndex_v2\0")?;
            let name: FnName = sym(lib, b"nvmlDeviceGetName\0")?;
            let util: FnUtil = sym(lib, b"nvmlDeviceGetUtilizationRates\0")?;
            let temp: FnTemp = sym(lib, b"nvmlDeviceGetTemperature\0")?;
            let mem: FnMem = sym(lib, b"nvmlDeviceGetMemoryInfo\0")?;
            let power: FnPower = sym(lib, b"nvmlDeviceGetPowerUsage\0")?;
            if init() != 0 {
                return None;
            }
            let mut n = 0u32;
            if count(&mut n) != 0 {
                return None;
            }
            let mut devices = Vec::new();
            for i in 0..n.min(2) {
                let mut h = 0usize;
                if handle(i, &mut h) != 0 || h == 0 {
                    continue;
                }
                let mut buf = [0u8; 96];
                let nm = if name(h, buf.as_mut_ptr(), buf.len() as u32) == 0 {
                    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
                    String::from_utf8_lossy(&buf[..end]).into_owned()
                } else {
                    format!("GPU {i}")
                };
                devices.push((h, nm));
            }
            if devices.is_empty() {
                return None;
            }
            Some(Nvml { util, temp, mem, power, devices })
        }
    }

    fn read(&self) -> Vec<GpuSample> {
        let mut out = Vec::new();
        for (h, name) in &self.devices {
            unsafe {
                let mut u = NvmlUtil::default();
                if (self.util)(*h, &mut u) != 0 {
                    continue;
                }
                let mut t = 0u32;
                let temp = ((self.temp)(*h, 0, &mut t) == 0).then_some(t);
                let mut m = NvmlMem::default();
                let (used, total) = if (self.mem)(*h, &mut m) == 0 { (m.used, m.total) } else { (0, 0) };
                let mut p = 0u32;
                let power_w = ((self.power)(*h, &mut p) == 0).then_some(p as f32 / 1000.0);
                out.push(GpuSample { name: name.clone(), util: u.gpu.min(100) as f32, temp, used, total, power_w });
            }
        }
        out
    }
}

// ================================================================ samples

#[derive(Clone, Debug)]
struct GpuSample {
    name: String,
    util: f32,
    temp: Option<u32>,
    used: u64,
    total: u64,
    power_w: Option<f32>,
}
#[derive(Clone, Debug)]
struct Sample {
    cpu: Option<f32>,
    ram_pct: f32,
    ram_used: u64,
    ram_total: u64,
    threads: u32,
    gpus: Vec<GpuSample>,
}

/// Owned by the worker while a sample is being taken (moved in and out of the job).
#[derive(Default)]
struct Sampler {
    prev: Option<CpuTimes>,
    nvml: Option<Nvml>,
    nvml_tried: bool,
}
impl Sampler {
    fn sample(&mut self) -> Sample {
        let (prev, cur) = match (self.prev, cpu_times()) {
            (Some(p), c) => (Some(p), c),
            (None, Some(c0)) => {
                // first reading: take a second one shortly after so the first sample already has a value (we are on a worker)
                std::thread::sleep(Duration::from_millis(120));
                (Some(c0), cpu_times())
            }
            (None, None) => (None, None),
        };
        self.prev = cur.or(self.prev);
        let cpu = match (prev, cur) {
            (Some(a), Some(b)) => cpu_pct(a, b),
            _ => None,
        };
        let mut ms = MEMORYSTATUSEX { dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32, ..Default::default() };
        let ok = unsafe { GlobalMemoryStatusEx(&mut ms).is_ok() };
        let (total, avail) = if ok { (ms.ullTotalPhys, ms.ullAvailPhys) } else { (0, 0) };
        let used = total.saturating_sub(avail);
        if !self.nvml_tried {
            self.nvml_tried = true;
            self.nvml = Nvml::load();
        }
        Sample {
            cpu,
            ram_pct: if total > 0 { (used as f64 / total as f64 * 100.0) as f32 } else { 0.0 },
            ram_used: used,
            ram_total: total,
            threads: unsafe { GetActiveProcessorCount(0xFFFF) },
            gpus: self.nvml.as_ref().map(|n| n.read()).unwrap_or_default(),
        }
    }
}

/// Deterministic preview data: (histories, sample). `hot` pushes the values into the amber/red range.
fn preview_data(ngpu: usize, hot: bool) -> (Hist, Hist, Vec<Hist>, Sample) {
    let k = if hot { 1.0 } else { 0.0 };
    let wave = |i: usize, base: f32, a1: f32, f1: f32, a2: f32, f2: f32| base + a1 * (i as f32 * f1).sin() + a2 * (i as f32 * f2 + 1.0).sin();
    let cpu = Hist::wave(60, |i| wave(i, 36.0 + 38.0 * k, 22.0, 0.31, 8.0, 0.97).clamp(2.0, 99.0));
    let ram = Hist::wave(60, |i| wave(i, 58.0 + 30.0 * k, 1.5, 0.12, 0.0, 0.0) + i as f32 * 0.04);
    let gpu0 = Hist::wave(60, |i| (30.0 + 18.0 * (i as f32 * 0.21 + 2.0).sin() + 14.0 * (i as f32 * 0.07).cos() + 52.0 * k).clamp(1.0, 99.0));
    let gpu1 = Hist::wave(60, |i| (12.0 + 10.0 * (i as f32 * 0.17 + 2.0).sin() + 6.0 * (i as f32 * 0.5).sin()).clamp(1.0, 99.0));
    let hists = vec![gpu0, gpu1];
    const GB: u64 = 1 << 30;
    let gpu = |i: usize| {
        if i == 0 {
            GpuSample { name: "NVIDIA GeForce RTX 4090".into(), util: hists[0].last().unwrap_or(0.0), temp: Some(if hot { 87 } else { 58 }), used: (9.6 * GB as f64) as u64, total: 24 * GB, power_w: Some(if hot { 412.0 } else { 168.0 }) }
        } else {
            GpuSample { name: "NVIDIA GeForce RTX 5090".into(), util: hists[1].last().unwrap_or(0.0), temp: Some(49), used: (3.1 * GB as f64) as u64, total: 32 * GB, power_w: Some(74.0) }
        }
    };
    let ram_total = 31_900_000_000u64 + 800_000_000;
    let ram_pct = ram.last().unwrap_or(0.0);
    let s = Sample { cpu: cpu.last(), ram_pct, ram_used: (ram_total as f64 * ram_pct as f64 / 100.0) as u64, ram_total, threads: 24, gpus: (0..ngpu.min(2)).map(gpu).collect() };
    (cpu, ram, hists.into_iter().take(ngpu.min(2)).collect(), s)
}

// ================================================================ the widget

struct Sysmon {
    ver: u64,
    sampler: Option<Sampler>,
    job: Option<Pending<(Sampler, Sample)>>,
    last: Option<Instant>,
    cur: Option<Sample>,
    cpu: Hist,
    ram: Hist,
    gpu: Vec<Hist>,
    /// GPU count the static layout was recorded for
    ngpu: usize,
    init: bool,
}

impl Sysmon {
    fn new() -> Sysmon {
        Sysmon { ver: 0, sampler: Some(Sampler::default()), job: None, last: None, cur: None, cpu: Hist::new(60), ram: Hist::new(60), gpu: Vec::new(), ngpu: 0, init: false }
    }

    fn init_preview(&mut self, cx: &Cx) {
        if self.init || !cx.preview {
            return;
        }
        self.init = true;
        let n = cx.cfg_f64("sample_gpus").map(|v| v as usize).unwrap_or(1).min(2);
        let (c, r, g, s) = preview_data(n, cx.cfg_bool("sample_hot").unwrap_or(false));
        (self.cpu, self.ram, self.gpu, self.cur, self.ngpu) = (c, r, g, Some(s), n);
        self.ver += 1;
    }

    fn apply(&mut self, s: Sample) {
        if let Some(c) = s.cpu.or_else(|| self.cpu.last()) {
            self.cpu.push(c);
        }
        self.ram.push(s.ram_pct);
        self.gpu.resize(s.gpus.len(), Hist::new(60));
        for (h, g) in self.gpu.iter_mut().zip(&s.gpus) {
            h.push(g.util);
        }
        if s.gpus.len() != self.ngpu {
            self.ngpu = s.gpus.len();
            self.ver += 1; // the static layout depends on the GPU count
        }
        self.cur = Some(s);
    }

    fn cpu_now(&self) -> Option<f32> {
        self.cpu.last()
    }
    fn ram_now(&self) -> Option<f32> {
        self.cur.as_ref().map(|s| s.ram_pct)
    }
    fn gpu_now(&self, i: usize) -> Option<&GpuSample> {
        self.cur.as_ref().and_then(|s| s.gpus.get(i))
    }
    /// The busiest GPU (S and M show one GPU).
    fn main_gpu(&self) -> usize {
        let mut best = 0;
        for i in 0..self.ngpu {
            if self.gpu_now(i).map_or(0.0, |g| g.util) > self.gpu_now(best).map_or(0.0, |g| g.util) {
                best = i;
            }
        }
        best
    }
}

fn pct_text(v: Option<f32>) -> String {
    v.map_or("–".to_string(), |v| format!("{}%", v.round() as i32))
}
fn short_name(n: &str) -> &str {
    n.strip_prefix("NVIDIA ").unwrap_or(n)
}

// ---------------------------------------------------------------- geometry

/// S: centre, diameter and caption rect of ring `i` of `n`.
fn s_ring(i: usize, n: usize) -> (Pt, f32, Rect) {
    let area = Rect::new(16.0, 40.0, 138.0, 114.0);
    let d = if n >= 3 { 42.0 } else { 56.0 };
    let col = area.col(i as u32, n.max(1) as u32, 0.0);
    let top = area.y + (area.h - (d + 20.0)) / 2.0;
    ((col.cx(), top + d / 2.0), d, Rect::new(col.x, top + d + 6.0, col.w, 14.0))
}

/// M: row rectangles for `n` rows (2 without a GPU, otherwise 3).
fn m_row(i: usize, n: usize) -> Rect {
    Rect::new(16.0, 16.0, 324.0, 138.0).row(i as u32, n as u32, 6.0)
}

/// L section geometry.
#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Cpu,
    Ram,
    Gpu(usize),
}
struct Sec {
    kind: Kind,
    head: Rect,
    graph: Rect,
    detail: Option<Rect>,
}
fn l_layout(ngpu: usize) -> Vec<Sec> {
    let gn = 2 + ngpu;
    let (head_h, detail_h, gap, pad) = (20.0, 38.0, 12.0, 5.0);
    let fixed = gn as f32 * (head_h + pad) + ngpu as f32 * (detail_h + pad) + (gn as f32 - 1.0) * gap;
    let max_g = if ngpu == 0 { 118.0 } else { 84.0 };
    let gh = ((324.0 - fixed) / gn as f32).clamp(24.0, max_g);
    let mut y = 16.0;
    let mut v = Vec::new();
    for i in 0..gn {
        let kind = match i {
            0 => Kind::Cpu,
            1 => Kind::Ram,
            n => Kind::Gpu(n - 2),
        };
        let head = Rect::new(16.0, y, 324.0, head_h);
        let graph = Rect::new(16.0, y + head_h + pad, 324.0, gh);
        y = graph.bottom();
        let detail = if matches!(kind, Kind::Gpu(_)) {
            let d = Rect::new(16.0, y + pad, 324.0, detail_h);
            y = d.bottom();
            Some(d)
        } else {
            None
        };
        y += gap;
        v.push(Sec { kind, head, graph, detail });
    }
    v
}
/// The three columns of the GPU detail row: temperature, VRAM, power.
fn l_cols(d: Rect) -> (Rect, Rect, Rect) {
    let (cw, gap) = (84.0, 12.0);
    (Rect::new(d.x, d.y, cw, d.h), Rect::new(d.x + cw + gap, d.y, d.w - 2.0 * (cw + gap), d.h), Rect::new(d.right() - cw, d.y, cw, d.h))
}

impl Widget for Sysmon {
    fn draw(&mut self, cx: &mut Cx) -> Result<()> {
        self.init_preview(cx);
        let gpu_label = |i: usize, n: usize| if n > 1 { format!("GPU {}", i + 1) } else { "GPU".to_string() };
        match cx.size {
            Size::S => {
                cx.text(Rect::new(16.0, 16.0, 138.0, 18.0), cx.t("Система", "System"), Style::TITLE.ellipsis(), DIM);
                let n = if self.ngpu > 0 { 3 } else { 2 };
                for i in 0..n {
                    let cap = match i {
                        0 => "CPU".to_string(),
                        1 => cx.t("ОЗУ", "RAM").to_string(),
                        _ => "GPU".to_string(),
                    };
                    let (_, _, r) = s_ring(i, n);
                    cx.text(r, &cap, Style::CAPTION.center(), DIM);
                }
            }
            Size::M => {
                let n = if self.ngpu > 0 { 3 } else { 2 };
                for i in 0..n {
                    let r = m_row(i, n);
                    let label = match i {
                        0 => "CPU".to_string(),
                        1 => cx.t("ОЗУ", "RAM").to_string(),
                        _ => "GPU".to_string(),
                    };
                    cx.text(Rect::new(r.x, r.y, 40.0, 14.0), &label, Style::CAPTION.weight(600), DIM);
                    cx.rrect(Rect::new(r.x + 120.0, r.y, r.w - 120.0, r.h), 9.0, PANEL);
                }
            }
            Size::L => {
                for s in l_layout(self.ngpu) {
                    let (glyph, label) = match s.kind {
                        Kind::Cpu => (Glyph::Cpu, "CPU".to_string()),
                        Kind::Ram => (Glyph::Ram, cx.t("ОЗУ", "RAM").to_uppercase()),
                        Kind::Gpu(i) => (Glyph::Gpu, gpu_label(i, self.ngpu)),
                    };
                    cx.icon(glyph, Rect::new(s.head.x, s.head.y + 2.0, 16.0, 16.0), DIM);
                    cx.text(Rect::new(s.head.x + 22.0, s.head.y, 52.0, s.head.h), &label, Style::LABEL, DIM);
                    cx.rrect(s.graph, 9.0, PANEL);
                    if let Some(d) = s.detail {
                        let (t, v, p) = l_cols(d);
                        let cap = Style::CAPTION;
                        cx.text(Rect::new(t.x, t.y, t.w, 14.0), cx.t("Температура", "Temperature"), cap.ellipsis(), DIM);
                        cx.text(Rect::new(v.x, v.y, v.w, 14.0), cx.t("Видеопамять", "VRAM"), cap.ellipsis(), DIM);
                        cx.text(Rect::new(p.x, p.y, p.w, 14.0), cx.t("Мощность", "Power"), cap.ellipsis(), DIM);
                    }
                }
            }
        }
        Ok(())
    }

    fn overlay(&mut self, cx: &mut Cx) -> Result<()> {
        self.init_preview(cx);
        let lang = cx.lang;
        match cx.size {
            Size::S => {
                let mut gauges = vec![self.cpu_now(), self.ram_now()];
                if self.ngpu > 0 {
                    gauges.push(self.gpu_now(self.main_gpu()).map(|g| g.util));
                }
                let n = gauges.len();
                for (i, v) in gauges.iter().enumerate() {
                    let (c, d, _) = s_ring(i, n);
                    let stroke = if n == 3 { 5.0 } else { 6.0 };
                    let col = v.map_or(FAINT, load_color);
                    cx.ring(c, d / 2.0 - stroke / 2.0, stroke, v.unwrap_or(0.0) / 100.0, col);
                    cx.text(Rect::new(c.0 - 22.0, c.1 - 10.0, 44.0, 20.0), &pct_text(*v), Style::new(if n == 3 { 11.5 } else { 14.0 }, 600).mono().center(), TEXT);
                }
            }
            Size::M => {
                let n = if self.ngpu > 0 { 3 } else { 2 };
                let g = self.gpu_now(self.main_gpu());
                for i in 0..n {
                    let r = m_row(i, n);
                    let (v, hist, detail): (Option<f32>, Option<&Hist>, String) = match i {
                        0 => (self.cpu_now(), Some(&self.cpu), self.cur.as_ref().map_or(String::new(), |s| format!("{} {}", s.threads, plural(s.threads, cx.t("поток", "thread"), cx.t("потока", "threads"), cx.t("потоков", "threads"))))),
                        1 => (self.ram_now(), Some(&self.ram), self.cur.as_ref().map_or(String::new(), |s| fmt_pair(s.ram_used, s.ram_total, lang))),
                        _ => (
                            g.map(|g| g.util),
                            self.gpu.get(self.main_gpu()),
                            g.map_or(String::new(), |g| match g.temp {
                                Some(t) => format!("{t}°C"),
                                None => short_name(&g.name).to_string(),
                            }),
                        ),
                    };
                    let col = v.map_or(FAINT, load_color);
                    cx.text(Rect::new(r.x + 38.0, r.y, 76.0, 14.0), &detail, Style::CAPTION.ellipsis(), DIM);
                    let vs = ((r.h - 14.0) * 0.85).min(34.0);
                    cx.text(Rect::new(r.x, r.y + 14.0, 116.0, r.h - 14.0), &pct_text(v), Style::new(vs, 300).mono(), TEXT);
                    if let Some(h) = hist {
                        h.draw(cx, Rect::new(r.x + 124.0, r.y + 5.0, r.w - 128.0, r.h - 10.0), 0.0, 100.0, col);
                    }
                }
            }
            Size::L => {
                for s in l_layout(self.ngpu) {
                    let (v, hist, detail): (Option<f32>, Option<&Hist>, String) = match s.kind {
                        Kind::Cpu => (self.cpu_now(), Some(&self.cpu), self.cur.as_ref().map_or(String::new(), |c| format!("{} {}", c.threads, plural(c.threads, cx.t("поток", "thread"), cx.t("потока", "threads"), cx.t("потоков", "threads"))))),
                        Kind::Ram => (self.ram_now(), Some(&self.ram), self.cur.as_ref().map_or(String::new(), |c| fmt_pair(c.ram_used, c.ram_total, lang))),
                        Kind::Gpu(i) => (self.gpu_now(i).map(|g| g.util), self.gpu.get(i), self.gpu_now(i).map_or(String::new(), |g| short_name(&g.name).to_string())),
                    };
                    let col = v.map_or(FAINT, load_color);
                    cx.text(Rect::new(s.head.x + 74.0, s.head.y, s.head.w - 74.0 - 64.0, s.head.h), &detail, Style::CAPTION.ellipsis(), DIM);
                    cx.text(Rect::new(s.head.right() - 64.0, s.head.y - 3.0, 64.0, s.head.h + 6.0), &pct_text(v), Style::new(21.0, 300).mono().right(), TEXT);
                    if let Some(h) = hist {
                        h.draw(cx, s.graph.inset_xy(1.0, 5.0), 0.0, 100.0, col);
                    }
                    if let (Some(d), Kind::Gpu(i)) = (s.detail, s.kind) {
                        let (t, vr, p) = l_cols(d);
                        let val = Style::new(14.0, 600).mono().ellipsis();
                        let g = self.gpu_now(i);
                        let (temp_s, temp_c) = match g.and_then(|g| g.temp) {
                            Some(t) => (format!("{t}°C"), level_color(t as f32, 75.0, 85.0)),
                            None => ("–".to_string(), TEXT),
                        };
                        cx.text(Rect::new(t.x, t.y + 14.0, t.w, 18.0), &temp_s, val, temp_c);
                        let (vram_s, frac) = match g {
                            Some(g) if g.total > 0 => (fmt_pair(g.used, g.total, lang), g.used as f32 / g.total as f32),
                            _ => ("–".to_string(), 0.0),
                        };
                        cx.text(Rect::new(vr.x, vr.y + 14.0, vr.w, 18.0), &vram_s, val, TEXT);
                        cx.bar(Rect::new(vr.x, vr.y + 33.0, vr.w, 4.0), frac, load_color(frac * 100.0));
                        let pw = g.and_then(|g| g.power_w).map_or("–".to_string(), |w| format!("{} {}", w.round() as i32, cx.t("Вт", "W")));
                        cx.text(Rect::new(p.x, p.y + 14.0, p.w, 18.0), &pw, val, TEXT);
                    }
                }
            }
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
                Some((s, smp)) => {
                    self.sampler = Some(s);
                    self.job = None;
                    self.apply(smp);
                    cx.request_frame();
                }
                None if !j.busy() => {
                    // the worker panicked: start over with a fresh sampler
                    self.sampler = Some(Sampler::default());
                    self.job = None;
                }
                None => {}
            }
        }
        if self.job.is_none() && self.last.map_or(true, |t| t.elapsed() >= Duration::from_millis(900)) {
            if let Some(mut s) = self.sampler.take() {
                self.last = Some(Instant::now());
                self.job = Some(cx.spawn(move || {
                    let smp = s.sample();
                    (s, smp)
                }));
            }
        }
        Tick::Every(Duration::from_secs(1))
    }

    fn version(&self) -> u64 {
        self.ver
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_delta() {
        let a = CpuTimes { idle: 100, kernel: 200, user: 100 };
        let b = CpuTimes { idle: 150, kernel: 300, user: 200 }; // total 200, idle 50 -> 75 % busy
        assert_eq!(cpu_pct(a, b), Some(75.0));
        assert_eq!(cpu_pct(a, a), None);
        let all_idle = CpuTimes { idle: 200, kernel: 300, user: 100 };
        assert_eq!(cpu_pct(a, all_idle), Some(0.0));
    }

    #[test]
    fn colour_ramp() {
        assert_eq!(load_color(10.0), accent());
        assert_eq!(load_color(70.0), ORANGE);
        assert_eq!(load_color(89.9), ORANGE);
        assert_eq!(load_color(90.0), RED);
        assert_eq!(level_color(74.0, 75.0, 85.0), accent());
        assert_eq!(level_color(80.0, 75.0, 85.0), ORANGE);
        assert_eq!(level_color(85.0, 75.0, 85.0), RED);
    }

    #[test]
    fn sizes() {
        assert_eq!(fmt_size(0, Lang::En), "0 B");
        assert_eq!(fmt_size(512 * 1024, Lang::En), "512 KB");
        assert_eq!(fmt_size(50 * 1024, Lang::En), "50.0 KB");
        assert_eq!(fmt_size(238 * 1024 * 1024, Lang::En), "238 MB");
        assert_eq!(fmt_size(1000 * 1024 * 1024, Lang::En), "1.0 GB");
        assert_eq!(fmt_size(412 << 30, Lang::Ru), "412 ГБ");
        assert_eq!(fmt_size(((1.8f64) * (1u64 << 40) as f64) as u64, Lang::Ru), "1,8 ТБ");
        assert_eq!(fmt_pair(20_000_000_000, 34_359_738_368, Lang::En), "18.6 / 32.0 GB");
    }

    #[test]
    fn rates() {
        assert_eq!(fmt_rate_parts(0.0, Lang::En), ("0".into(), "KB/s"));
        assert_eq!(fmt_rate_parts(512.0, Lang::En), ("0.5".into(), "KB/s"));
        assert_eq!(fmt_rate_parts(845.0 * 1024.0, Lang::En), ("845".into(), "KB/s"));
        assert_eq!(fmt_rate_parts(1000.0 * 1024.0, Lang::En), ("1.0".into(), "MB/s"));
        assert_eq!(fmt_rate_parts(12.4 * 1048576.0, Lang::Ru), ("12,4".into(), "МБ/с"));
        assert_eq!(fmt_rate_parts(2.0 * 1073741824.0, Lang::En), ("2.0".into(), "GB/s"));
    }

    #[test]
    fn history_is_bounded() {
        let mut h = Hist::new(3);
        for i in 0..10 {
            h.push(i as f32);
        }
        assert_eq!(h.v, vec![7.0, 8.0, 9.0]);
        assert_eq!(h.last(), Some(9.0));
        assert_eq!(h.max(), 9.0);
    }

    /// Prints real numbers from this machine: `cargo test sysmon_live -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn sysmon_live() {
        let mut s = Sampler::default();
        let t = Instant::now();
        let a = s.sample();
        println!("first sample ({:?}): {:?}", t.elapsed(), a);
        std::thread::sleep(Duration::from_millis(1000));
        let t = Instant::now();
        let b = s.sample();
        println!("second sample ({:?}): {:?}", t.elapsed(), b);
        assert!(b.ram_total > 0);
    }
}
