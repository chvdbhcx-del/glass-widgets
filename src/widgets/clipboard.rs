//! Clipboard: a history of copied TEXT. M = header + 4 rows (one-line preview + relative time), L = header with a "Очистить" button
//! + 8 rows (preview + "age · length"). Click a row = put it back on the clipboard (the row gets a check mark and a short
//! "Скопировано" flash in the overlay; it is NOT re-added as new), right click = forget that entry.
//! One listener thread per process (message-only window + `AddClipboardFormatListener`, own message loop) is started by the first live
//! `tick`; it keeps at most 20 entries in MEMORY ONLY (never on disk) and wakes every clipboard card when something changes.
//! Privacy: text over 4 KB is ignored, so is anything copied with the formats `ExcludeClipboardContentFromMonitorProcessing`,
//! `Clipboard Viewer Ignore` or `CanIncludeInClipboardHistory = 0`, and anything that looks like a secret (see `looks_secret`:
//! card numbers passing Luhn, well-known token prefixes, private keys, long high-entropy single tokens).
//! Settings: none. Preview: eight sample strings, no listener, no clipboard access.
//! Test hooks (previews only): `cfg.preview_hot` = row index (100 = the clear button), `cfg.preview_flash` = row index.
use crate::registry::{Registry, WidgetDef};
use crate::widget::*;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use windows::core::w;
use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::DataExchange::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::UI::WindowsAndMessaging::*;

pub fn register(r: &mut Registry) {
    r.register(WidgetDef {
        type_id: "clipboard",
        name: ("Буфер обмена", "Clipboard"),
        desc: ("История скопированного", "What you copied recently"),
        group: "utilities",
        icon: Glyph::Clipboard,
        sizes: &[Size::M, Size::L],
        default_size: Size::M,
        make: || Box::new(Clip::default()),
    });
}

const CAP: usize = 20;
const MAX_BYTES: usize = 4096;
const CF_UNICODETEXT: u32 = 13;
const CLEAR_SLOT: usize = 100;
/// The "Copied" flash: total time, fade-in and fade-out (seconds).
const FLASH_T: f32 = 1.3;
const FLASH_IN: f32 = 0.12;
const FLASH_OUT: f32 = 0.22;

// ================================================================ pure logic (unit-tested)

#[derive(Clone, Debug, PartialEq)]
struct Entry {
    text: String,
    /// unix seconds
    at: i64,
}

/// Newest-first ring buffer of at most `CAP` entries.
#[derive(Default)]
struct History {
    v: VecDeque<Entry>,
    ver: u64,
}
impl History {
    /// Returns false when `text` is the newest entry already. An older identical entry moves to the front instead of duplicating.
    fn push(&mut self, text: &str, at: i64) -> bool {
        if self.v.front().is_some_and(|e| e.text == text) {
            return false;
        }
        if let Some(p) = self.v.iter().position(|e| e.text == text) {
            self.v.remove(p);
        }
        self.v.push_front(Entry { text: text.to_string(), at });
        self.v.truncate(CAP);
        self.ver += 1;
        true
    }
    fn remove(&mut self, i: usize) {
        if self.v.remove(i).is_some() {
            self.ver += 1;
        }
    }
    fn clear(&mut self) {
        if !self.v.is_empty() {
            self.v.clear();
            self.ver += 1;
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Verdict {
    Keep,
    Blank,
    TooBig,
    Secret,
}

fn judge(text: &str) -> Verdict {
    if text.trim().is_empty() {
        Verdict::Blank
    } else if text.len() > MAX_BYTES {
        Verdict::TooBig
    } else if looks_secret(text) {
        Verdict::Secret
    } else {
        Verdict::Keep
    }
}

fn luhn(digits: &[u32]) -> bool {
    let mut sum = 0;
    for (i, &d) in digits.iter().rev().enumerate() {
        sum += if i % 2 == 1 { let x = d * 2; if x > 9 { x - 9 } else { x } } else { d };
    }
    !digits.is_empty() && sum % 10 == 0
}

/// Shannon entropy in bits per char.
fn entropy(s: &str) -> f32 {
    let mut counts: std::collections::HashMap<char, u32> = std::collections::HashMap::new();
    let mut n = 0u32;
    for c in s.chars() {
        *counts.entry(c).or_insert(0) += 1;
        n += 1;
    }
    if n == 0 {
        return 0.0;
    }
    counts.values().map(|&k| { let p = k as f32 / n as f32; -p * p.log2() }).sum()
}

/// Heuristic secret detector: cheap and deliberately a bit eager (a false positive only costs a missing history row).
fn looks_secret(text: &str) -> bool {
    let t = text.trim();
    if t.contains("PRIVATE KEY-----") {
        return true;
    }
    // payment card: 13..19 digits (spaces / dashes allowed) with a valid Luhn checksum
    if t.chars().all(|c| c.is_ascii_digit() || c == ' ' || c == '-') {
        let d: Vec<u32> = t.chars().filter_map(|c| c.to_digit(10)).collect();
        if (13..=19).contains(&d.len()) && luhn(&d) {
            return true;
        }
    }
    if t.chars().any(char::is_whitespace) {
        return false;
    }
    const PREFIXES: [&str; 10] = ["sk-", "sk_live_", "ghp_", "gho_", "ghs_", "github_pat_", "glpat-", "xoxb-", "xoxp-", "AIza"];
    if t.len() >= 20 && PREFIXES.iter().any(|p| t.starts_with(p)) {
        return true;
    }
    if t.starts_with("AKIA") && t.len() == 20 && t.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit()) {
        return true;
    }
    if t.starts_with("eyJ") && t.matches('.').count() == 2 {
        return true; // JWT
    }
    let n = t.chars().count();
    if n < 16 || t.contains("://") || t.starts_with("www.") {
        return false;
    }
    // file names, paths and identifiers split into clean words / numbers; random tokens do not
    let segs: Vec<&str> = t.split(['_', '-', '.', '/', '\\', ':']).filter(|s| !s.is_empty()).collect();
    let clean = segs.iter().filter(|s| s.chars().all(char::is_alphabetic) || s.chars().all(|c| c.is_ascii_digit())).count();
    if segs.len() >= 3 && clean * 4 >= segs.len() * 3 {
        return false;
    }
    let classes = [t.chars().any(|c| c.is_lowercase()), t.chars().any(|c| c.is_uppercase()), t.chars().any(|c| c.is_ascii_digit()), t.chars().any(|c| !c.is_alphanumeric())];
    let classes = classes.iter().filter(|&&b| b).count();
    let h = entropy(t);
    classes >= 3 && h >= 3.5 && h / (n as f32).log2() >= 0.85
}

/// One-line preview: whitespace runs collapse to a single space, at most 160 chars.
fn preview_text(s: &str) -> String {
    let mut out = String::new();
    for (i, w) in s.split_whitespace().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        out.push_str(w);
        if out.chars().count() > 160 {
            break;
        }
    }
    out.chars().take(160).collect()
}

fn age_label(secs: i64, ru: bool) -> String {
    let s = secs.max(0);
    match s {
        0..=44 => (if ru { "сейчас" } else { "now" }).to_string(),
        45..=3599 => format!("{} {}", (s / 60).max(1), if ru { "мин" } else { "min" }),
        3600..=86399 => format!("{} {}", s / 3600, if ru { "ч" } else { "h" }),
        _ => format!("{} {}", s / 86400, if ru { "дн" } else { "d" }),
    }
}

#[derive(Clone, Debug, PartialEq)]
struct Row {
    text: String,
    preview: String,
    age: String,
    chars: usize,
    current: bool,
}

fn make_rows(entries: &[Entry], current: Option<&str>, now: i64, ru: bool) -> Vec<Row> {
    entries
        .iter()
        .map(|e| Row { text: e.text.clone(), preview: preview_text(&e.text), age: age_label(now - e.at, ru), chars: e.text.chars().count(), current: current == Some(e.text.as_str()) })
        .collect()
}

fn sample_rows(ru: bool) -> Vec<Row> {
    let s: [(&str, i64); 8] = [
        ("https://github.com/anthropics/claude-code", 20),
        ("Созвон завтра в 15:30 — ссылка в календаре", 3 * 60),
        ("npm install --save-dev typescript", 12 * 60),
        ("+7 (916) 555-01-32", 47 * 60),
        ("Купить молоко, яйца и хлеб", 2 * 3600),
        ("SELECT * FROM users WHERE id = 42;", 3 * 3600),
        ("Привет! Как дела? Давно не виделись, заходи в гости", 5 * 3600),
        (r"C:\Users\Anna\Documents\отчёт.xlsx", 26 * 3600),
    ];
    let entries: Vec<Entry> = s.iter().map(|&(t, age)| Entry { text: t.to_string(), at: 1_000_000 - age }).collect();
    make_rows(&entries, Some(s[0].0), 1_000_000, ru)
}

fn unix_now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

// ================================================================ process-wide state + listener thread

struct Shared {
    hist: History,
    /// the text that is on the clipboard right now (None = not text / filtered)
    current: Option<String>,
    /// set before WE write the clipboard, so the resulting update is not recorded as a new copy
    suppress: Option<(String, Instant)>,
    wakers: Vec<Waker>,
}
static SHARED: Mutex<Shared> = Mutex::new(Shared { hist: History { v: VecDeque::new(), ver: 0 }, current: None, suppress: None, wakers: Vec::new() });
static STARTED: AtomicBool = AtomicBool::new(false);
static LISTENER_HWND: AtomicIsize = AtomicIsize::new(0);

fn shared() -> MutexGuard<'static, Shared> {
    SHARED.lock().unwrap_or_else(|e| e.into_inner())
}

fn wake_all() {
    let ws: Vec<Waker> = shared().wakers.clone();
    ws.iter().for_each(Waker::wake);
}

/// Clipboard formats that mean "do not record this".
fn format_ids() -> &'static [u32; 3] {
    static F: OnceLock<[u32; 3]> = OnceLock::new();
    F.get_or_init(|| unsafe {
        [
            RegisterClipboardFormatW(w!("ExcludeClipboardContentFromMonitorProcessing")),
            RegisterClipboardFormatW(w!("Clipboard Viewer Ignore")),
            RegisterClipboardFormatW(w!("CanIncludeInClipboardHistory")),
        ]
    })
}

/// With the clipboard open: the text, or None for "no text / excluded by its owner / too big".
unsafe fn read_open_clipboard() -> Option<String> {
    let f = format_ids();
    if IsClipboardFormatAvailable(f[0]).is_ok() || IsClipboardFormatAvailable(f[1]).is_ok() {
        return None;
    }
    if IsClipboardFormatAvailable(f[2]).is_ok() {
        if let Ok(h) = GetClipboardData(f[2]) {
            let hg = HGLOBAL(h.0);
            let p = GlobalLock(hg) as *const u32;
            let zero = !p.is_null() && GlobalSize(hg) >= 4 && *p == 0;
            let _ = GlobalUnlock(hg);
            if zero {
                return None;
            }
        }
    }
    if IsClipboardFormatAvailable(CF_UNICODETEXT).is_err() {
        return None;
    }
    let hg = HGLOBAL(GetClipboardData(CF_UNICODETEXT).ok()?.0);
    let bytes = GlobalSize(hg);
    if bytes > MAX_BYTES * 2 + 2 {
        return None; // way over 4 KB even in UTF-16: do not even copy it
    }
    let p = GlobalLock(hg) as *const u16;
    if p.is_null() {
        return None;
    }
    let max = bytes / 2;
    let mut n = 0;
    while n < max && *p.add(n) != 0 {
        n += 1;
    }
    let s = String::from_utf16_lossy(std::slice::from_raw_parts(p, n));
    let _ = GlobalUnlock(hg);
    Some(s)
}

fn open_clipboard(owner: HWND) -> bool {
    for _ in 0..10 {
        if unsafe { OpenClipboard(owner) }.is_ok() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(15));
    }
    false
}

/// Listener thread: read the clipboard after an update and record it when the privacy filter allows.
fn capture(owner: HWND) {
    if !open_clipboard(owner) {
        return;
    }
    let text = unsafe { read_open_clipboard() };
    let _ = unsafe { CloseClipboard() };
    {
        let mut s = shared();
        match text.filter(|t| judge(t) == Verdict::Keep) {
            Some(t) => {
                let ours = s.suppress.as_ref().is_some_and(|(x, at)| *x == t && at.elapsed().as_secs() < 2);
                if !ours {
                    s.hist.push(&t, unix_now());
                }
                s.current = Some(t);
            }
            None => s.current = None,
        }
    }
    wake_all();
}

unsafe extern "system" fn wndproc(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    if m == WM_CLIPBOARDUPDATE {
        capture(h);
        return LRESULT(0);
    }
    DefWindowProcW(h, m, w, l)
}

fn start_listener() {
    if STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    let r = std::thread::Builder::new().name("clipboard-listener".into()).spawn(|| unsafe {
        let Ok(hi) = GetModuleHandleW(None) else { return STARTED.store(false, Ordering::SeqCst) };
        let cls = w!("MsWidgetsClipboard");
        let wc = WNDCLASSW { lpfnWndProc: Some(wndproc), hInstance: hi.into(), lpszClassName: cls, ..Default::default() };
        RegisterClassW(&wc);
        let Ok(h) = CreateWindowExW(WINDOW_EX_STYLE(0), cls, w!(""), WINDOW_STYLE(0), 0, 0, 0, 0, HWND_MESSAGE, None, hi, None) else { return STARTED.store(false, Ordering::SeqCst) };
        if AddClipboardFormatListener(h).is_err() {
            let _ = DestroyWindow(h);
            return STARTED.store(false, Ordering::SeqCst);
        }
        LISTENER_HWND.store(h.0 as isize, Ordering::SeqCst);
        capture(h); // whatever is on the clipboard at start
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    });
    if r.is_err() {
        STARTED.store(false, Ordering::SeqCst);
    }
}

/// Worker: put `text` on the clipboard (the matching update was marked in `suppress` by the caller).
fn set_clipboard(text: &str) {
    let owner = HWND(LISTENER_HWND.load(Ordering::SeqCst) as *mut _);
    if owner.0.is_null() || !open_clipboard(owner) {
        return;
    }
    unsafe {
        let _ = EmptyClipboard();
        let wide: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
        if let Ok(hm) = GlobalAlloc(GMEM_MOVEABLE, wide.len() * 2) {
            let p = GlobalLock(hm) as *mut u16;
            if !p.is_null() {
                std::ptr::copy_nonoverlapping(wide.as_ptr(), p, wide.len());
            }
            let _ = GlobalUnlock(hm);
            if p.is_null() || SetClipboardData(CF_UNICODETEXT, HANDLE(hm.0)).is_err() {
                let _ = GlobalFree(hm); // the clipboard only owns the memory when SetClipboardData succeeded
            }
        }
        let _ = CloseClipboard();
    }
}

// ================================================================ widget

#[derive(Default)]
struct Clip {
    started: bool,
    rows: Vec<Row>,
    /// row under the pointer (or `CLEAR_SLOT`) and its eased hover amount
    hv: HoverAnim,
    /// (when, row) of the last copy-back flash; it fades in, holds and fades out over `FLASH_T`
    flash: Option<(Instant, usize)>,
    flash_job: Option<Pending<()>>,
    /// the flash is in its fade-out (`flash_job` ran down): keep painting until it is gone
    flash_fade: bool,
    /// reduced motion when the flash started: no fades
    flash_still: bool,
    ver: u64,
}

fn poll<T>(slot: &mut Option<Pending<T>>) -> Option<T> {
    let p = slot.as_ref()?;
    let busy = p.busy();
    match p.take() {
        Some(v) => {
            *slot = None;
            Some(v)
        }
        None => {
            if !busy {
                *slot = None;
            }
            None
        }
    }
}

/// Visible rows, row height, first row y per size.
fn layout(size: Size) -> (usize, f32, f32) {
    match size {
        Size::L => (8, 37.0, 46.0),
        _ => (4, 29.0, 38.0),
    }
}
fn row_rect(size: Size, i: usize) -> Rect {
    let (_, h, y0) = layout(size);
    Rect::new(16.0, y0 + i as f32 * h, 324.0, h)
}
fn clear_rect() -> Rect {
    Rect::new(356.0 - 16.0 - 84.0, 12.0, 84.0, 26.0)
}

impl Clip {
    fn refresh(&mut self, ru: bool) -> bool {
        let (entries, current) = {
            let s = shared();
            (s.hist.v.iter().cloned().collect::<Vec<_>>(), s.current.clone())
        };
        let rows = make_rows(&entries, current.as_deref(), unix_now(), ru);
        if rows != self.rows {
            self.rows = rows;
            self.ver += 1;
            return true;
        }
        false
    }
}

impl Widget for Clip {
    fn draw(&mut self, cx: &mut Cx) -> Result<()> {
        let size = cx.size;
        let (n, _, _) = layout(size);
        let l = size == Size::L;
        cx.icon(Glyph::Clipboard, Rect::new(16.0, 17.0, 15.0, 15.0), accent());
        cx.text(Rect::new(37.0, 14.0, 200.0, 22.0), cx.t("Буфер обмена", "Clipboard"), Style::TITLE.ellipsis(), accent());
        if l {
            if !self.rows.is_empty() {
                cx.text(clear_rect(), cx.t("Очистить", "Clear"), Style::BODY.right(), accent());
            }
        }
        if self.rows.is_empty() {
            let mid = Rect::new(16.0, 38.0, 324.0, cx.h - 38.0 - 14.0);
            cx.icon(Glyph::Clipboard, mid.centered(30.0, 30.0).offset(0.0, -12.0), FAINT);
            cx.text(Rect::new(mid.x, mid.cy() + 8.0, mid.w, 22.0), cx.t("Скопируйте что-нибудь", "Copy something"), Style::BODY.center(), DIM);
            return Ok(());
        }
        let shown = self.rows.len().min(n);
        for (i, row) in self.rows.iter().take(n).enumerate() {
            let r = row_rect(size, i);
            if i + 1 < shown || l && i + 1 < n {
                cx.rect(Rect::new(r.x, r.bottom() - 0.3, r.w, 0.6), FILL);
            }
            let right = if l { 28.0 } else { 76.0 }; // room for the time column (M) and the check mark
            let text_w = r.w - right - 8.0;
            if l {
                cx.text(Rect::new(r.x, r.y + 4.0, text_w, 19.0), &row.preview, Style::BODY.ellipsis(), TEXT);
                let meta = format!("{} · {} {}", row.age, row.chars, cx.t("симв.", "chars"));
                cx.text(Rect::new(r.x, r.y + 21.0, text_w, 13.0), &meta, Style::CAPTION.ellipsis(), DIM);
            } else {
                cx.text(Rect::new(r.x, r.y, text_w, r.h), &row.preview, Style::BODY.ellipsis(), TEXT);
                cx.text(Rect::new(r.right() - 52.0, r.y, 52.0, r.h), &row.age, Style::CAPTION.mono().right(), DIM);
            }
            if row.current {
                let c = if l { Rect::new(r.right() - 16.0, r.cy() - 8.0, 16.0, 16.0) } else { Rect::new(r.right() - 52.0 - 20.0, r.cy() - 7.0, 14.0, 14.0) };
                cx.icon(Glyph::Check, c, accent());
            }
        }
        Ok(())
    }

    fn overlay(&mut self, cx: &mut Cx) -> Result<()> {
        let size = cx.size;
        let (n, _, _) = layout(size);
        // the row (or the "Clear" button) under the pointer and the one it just left, easing in / out
        let pre = if cx.preview { cx.cfg_f64("preview_hot").map(|v| v as usize) } else { None };
        let amount = |slot: usize| if pre == Some(slot) { 1.0 } else { self.hv.get(slot) };
        if size == Size::L && !self.rows.is_empty() {
            let k = amount(CLEAR_SLOT);
            if k > 0.0 {
                cx.pill(Rect::new(274.0, 13.0, 74.0, 24.0), WHITE.alpha(0.16 * k));
            }
        }
        for i in 0..self.rows.len().min(n) {
            let k = amount(i);
            if k > 0.0 {
                cx.rrect(row_rect(size, i).inset_xy(-6.0, 1.0), 9.0, WHITE.alpha(0.11 * k));
            }
        }
        let flash = self.flash.map(|(_, i)| i).or(if cx.preview { cx.cfg_f64("preview_flash").map(|v| v as usize) } else { None });
        if let Some(i) = flash.filter(|&i| i < self.rows.len().min(n)) {
            let e = self.flash.map_or(1.0, |(t0, _)| t0.elapsed().as_secs_f32());
            let a = if self.flash_still { 1.0 } else { crate::motion::smooth(e / FLASH_IN).min(crate::motion::smooth((FLASH_T - e) / FLASH_OUT)) };
            let r = row_rect(size, i);
            let pill = Rect::new(cx.w / 2.0 - 62.0, r.cy() - 14.0, 124.0, 28.0);
            cx.pill(pill.inset(-1.0), WHITE.alpha(0.22 * a));
            cx.pill(pill, Color::rgba(0.07, 0.07, 0.09, 0.82 * a));
            cx.icon(Glyph::CheckCircle, Rect::new(pill.x + 10.0, pill.cy() - 8.0, 16.0, 16.0), GREEN.fade(a));
            cx.text(Rect::new(pill.x + 30.0, pill.y, pill.w - 36.0, pill.h), cx.t("Скопировано", "Copied"), Style::new(12.5, 600).ellipsis(), TEXT.fade(a));
        }
        Ok(())
    }

    fn tick(&mut self, cx: &Cx) -> Tick {
        if !self.started {
            self.started = true;
            if cx.preview {
                self.rows = sample_rows(cx.lang == Lang::Ru);
                self.ver += 1;
            } else {
                {
                    let mut s = shared();
                    let me = cx.waker();
                    if !s.wakers.iter().any(|w| w.hwnd == me.hwnd && w.idx == me.idx) {
                        s.wakers.push(me);
                    }
                }
                start_listener();
            }
        }
        if cx.preview {
            return Tick::Never;
        }
        if poll(&mut self.flash_job).is_some() {
            if self.flash_still {
                self.flash = None; // reduced motion: gone at once
                cx.request_frame(); // repaint the card without the flash
            } else {
                self.flash_fade = true; // the fade-out runs until FLASH_T
            }
        }
        if self.flash_fade && self.flash.is_none_or(|(t0, _)| t0.elapsed().as_secs_f32() >= FLASH_T) {
            self.flash = None;
            self.flash_fade = false;
            cx.request_frame();
        }
        self.refresh(cx.lang == Lang::Ru);
        // 60 fps only while a row highlight eases or the flash fades in / out; otherwise the data schedule
        let moving = self.hv.step(cx) || (!self.flash_still && self.flash.is_some_and(|(t0, _)| self.flash_fade || t0.elapsed().as_secs_f32() < FLASH_IN));
        if moving {
            Tick::Fps(60)
        } else if self.rows.is_empty() {
            Tick::Never
        } else {
            Tick::Poll(Duration::from_secs(30)) // ages are re-formatted twice a minute
        }
    }

    fn mouse(&mut self, ev: &MouseEv, cx: &mut Cx) -> bool {
        let size = cx.size;
        let (n, _, _) = layout(size);
        let shown = self.rows.len().min(n);
        let slot = if ev.kind == MouseKind::Leave {
            None
        } else if size == Size::L && !self.rows.is_empty() && clear_rect().contains(ev.x, ev.y) {
            Some(CLEAR_SLOT)
        } else {
            (0..shown).find(|&i| row_rect(size, i).contains(ev.x, ev.y))
        };
        let mut changed = self.hv.set(slot, cx);
        let ru = cx.lang == Lang::Ru;
        match slot {
            Some(CLEAR_SLOT) if ev.clicked(clear_rect()) => {
                let mut s = shared();
                s.hist.clear();
                s.current = None;
                drop(s);
                changed |= self.refresh(ru);
            }
            Some(i) if i < CLEAR_SLOT && ev.clicked(row_rect(size, i)) => {
                let text = self.rows[i].text.clone();
                {
                    let mut s = shared();
                    s.suppress = Some((text.clone(), Instant::now()));
                    s.current = Some(text.clone());
                }
                cx.spawn(move || set_clipboard(&text));
                self.flash = Some((Instant::now(), i));
                self.flash_still = reduced_motion();
                self.flash_fade = false;
                // wakes `tick` once the fade-out starts (and right away, which starts the fade-in)
                self.flash_job = Some(cx.spawn(|| std::thread::sleep(std::time::Duration::from_millis(((FLASH_T - FLASH_OUT) * 1000.0) as u64))));
                cx.waker().wake();
                self.refresh(ru);
                changed = true;
            }
            Some(i) if i < CLEAR_SLOT && ev.right_clicked(row_rect(size, i)) => {
                shared().hist.remove(i);
                self.refresh(ru);
                changed = true;
            }
            _ => {}
        }
        changed
    }

    fn version(&self) -> u64 {
        self.ver
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_buffer_dedupes_and_caps() {
        let mut h = History::default();
        assert!(h.push("a", 1));
        assert!(!h.push("a", 2)); // consecutive repeat
        assert!(h.push("b", 3));
        assert!(h.push("a", 4)); // older duplicate moves to the front
        assert_eq!(h.v.iter().map(|e| e.text.as_str()).collect::<Vec<_>>(), ["a", "b"]);
        assert_eq!(h.v[0].at, 4);
        for i in 0..30 {
            h.push(&format!("x{i}"), 10 + i);
        }
        assert_eq!(h.v.len(), CAP);
        assert_eq!(h.v[0].text, "x29");
        assert_eq!(h.v[CAP - 1].text, "x10");
        let v = h.ver;
        h.remove(0);
        assert_eq!(h.v[0].text, "x28");
        assert!(h.ver > v);
        h.clear();
        assert!(h.v.is_empty());
    }

    #[test]
    fn privacy_filter() {
        // kept
        for t in [
            "https://github.com/anthropics/claude-code",
            "Report_2026_Final_v2.docx",
            r"C:\Users\Anna\Documents\отчёт.xlsx",
            "/usr/local/bin/python3",
            "getUserByIdAndFilterStuff",
            "npm install --save-dev typescript",
            "Привет! Как дела? Давно не виделись",
            "2026-10-24T10:08:35Z",
            "+7 (916) 555-01-32",
            "user@example.com",
            "123e4567-e89b-12d3-a456-426614174000",
            "a94a8fe5ccb19ba61c4c0873d391e987982fbbd3",
            "4111111111111112", // fails Luhn
            "hello",
        ] {
            assert_eq!(judge(t), Verdict::Keep, "{t}");
        }
        // secrets
        for t in [
            "4111 1111 1111 1111",
            "5500-0000-0000-0004",
            "Xk9#mP2$vL8@nQ4zRt7!",
            "ghp_16C7e42F292c6912E7710c838347Ae178B4a",
            "AKIAIOSFODNN7EXAMPLE",
            "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
            "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U",
            "-----BEGIN RSA PRIVATE KEY-----\nMIIE...\n-----END RSA PRIVATE KEY-----",
        ] {
            assert_eq!(judge(t), Verdict::Secret, "{t}");
        }
        assert_eq!(judge("   \n\t"), Verdict::Blank);
        assert_eq!(judge(&"a b ".repeat(2000)), Verdict::TooBig);
    }

    /// Read-only smoke test of the listener thread: window + `AddClipboardFormatListener` + the first capture (never writes the clipboard).
    #[test]
    fn listener_starts() {
        start_listener();
        for _ in 0..100 {
            if LISTENER_HWND.load(Ordering::SeqCst) != 0 {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        panic!("clipboard listener did not come up");
    }

    #[test]
    fn luhn_checks() {
        assert!(luhn(&[4, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1]));
        assert!(!luhn(&[4, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 2]));
        assert!(!luhn(&[]));
    }

    #[test]
    fn previews_and_ages() {
        assert_eq!(preview_text("  a \n\n b\t c  "), "a b c");
        assert_eq!(preview_text(&"x".repeat(500)).chars().count(), 160);
        assert_eq!(age_label(5, true), "сейчас");
        assert_eq!(age_label(50, true), "1 мин");
        assert_eq!(age_label(3599, true), "59 мин");
        assert_eq!(age_label(7300, true), "2 ч");
        assert_eq!(age_label(3 * 86400, false), "3 d");
        assert_eq!(age_label(-10, true), "сейчас");
    }

    #[test]
    fn rows_mark_the_current_text() {
        let e = vec![Entry { text: "a".into(), at: 100 }, Entry { text: "b".into(), at: 40 }];
        let r = make_rows(&e, Some("b"), 160, true);
        assert!(!r[0].current && r[1].current);
        assert_eq!(r[0].age, "1 мин");
        assert_eq!(sample_rows(true).len(), 8);
        assert!(sample_rows(true)[0].current);
    }
}
