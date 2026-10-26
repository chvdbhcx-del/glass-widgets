//! Shortcuts: a launcher grid of app / folder / site tiles. S = 2x2 (3 tiles + "+"), M = 4x2 (7 + "+"), L = 4x4 (15 + "+").
//! The LAST visible slot is always the "+" tile (it asks for a path or URL); right click on a tile removes it; click launches it.
//! Settings: `cfg.items = [{"name": "Проводник", "target": "explorer.exe"}, ...]` where `target` is an exe / lnk / folder path, a
//! protocol (`ms-settings:`) or an http(s) URL (an optional `"icon"` = any shell parsing name overrides the icon source).
//! No `items` key: the list is seeded (read-only) from `%APPDATA%\macshell\dock.json`, else four built-ins; the seed is written to
//! the config only after the first add / remove. Items beyond the visible slots stay stored and show on a larger size.
//! Icons: `IShellItemImageFactory` (96 px) on a worker -> premultiplied BGRA -> D2D bitmap on the UI thread, kept in memory only.
//! URL tiles show the site's own icon: `/apple-touch-icon.png`, else `/favicon.ico` (worker, `net::http_get`), the raw bytes cached in
//! `%APPDATA%\macshell\cache\favicons\{host}.bin` (a host without a usable icon is skipped for 24 h); no icon -> the globe tile.
//! Everything static is recorded in `draw`; the hover look (a plate + the icon scaled 1.12) lives in `overlay` and eases in / out (`HoverAnim`; `Tick::Fps(60)` only while it moves, else `Never`).
//! Preview: eight+ sample tiles (coloured squircles with letters, one URL tile with a globe), no I/O.
use crate::registry::{Registry, WidgetDef};
use crate::widget::*;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use windows::core::HSTRING;

pub fn register(r: &mut Registry) {
    r.register(WidgetDef {
        type_id: "shortcuts",
        name: ("Ярлыки", "Shortcuts"),
        desc: ("Быстрый запуск приложений и папок", "Launch apps and folders in one click"),
        group: "controls",
        icon: Glyph::Folder,
        sizes: &[Size::S, Size::M, Size::L],
        default_size: Size::M,
        make: || Box::new(Shortcuts::default()),
    });
}

const PALETTE: [Color; 8] = [BLUE, ORANGE, GREEN, PINK, PURPLE, TEAL, Color::rgb8(88, 86, 214), Color::rgb8(120, 120, 128)];
const SETTINGS_ICON: &str = r"shell:AppsFolder\windows.immersivecontrolpanel_cw5n1h2txyewy!microsoft.windows.immersivecontrolpanel";
const CALC_ICON: &str = r"shell:AppsFolder\Microsoft.WindowsCalculator_8wekyb3d8bbwe!App";

// ================================================================ pure logic (unit-tested)

#[derive(Clone, Debug, PartialEq)]
struct Item {
    name: String,
    target: String,
    /// explicit icon source (shell parsing name); otherwise derived from the target
    icon: Option<String>,
    /// index into PALETTE for the fallback tile
    color: usize,
}
impl Item {
    fn new(name: &str, target: &str, icon: Option<String>) -> Item {
        Item { name: name.to_string(), target: target.to_string(), icon, color: color_for(name) }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Url,
    /// `ms-settings:`, `shell:...`, `mailto:` ...
    Uri,
    Path,
}

fn color_for(name: &str) -> usize {
    let mut h: u32 = 2166136261;
    for b in name.bytes() {
        h = (h ^ b as u32).wrapping_mul(16777619);
    }
    (h >> 8) as usize % PALETTE.len()
}

fn classify(t: &str) -> Kind {
    let l = t.trim().to_ascii_lowercase();
    if l.starts_with("http://") || l.starts_with("https://") {
        return Kind::Url;
    }
    match l.find(':') {
        // a scheme is 2+ chars (a single letter is a drive), and "C:\" style is never followed by "//" or a backslash for a URI
        Some(i) if i >= 2 && l[..i].chars().all(|c| c.is_ascii_alphanumeric() || "+.-".contains(c)) && !l[i + 1..].starts_with(['\\', '/']) => Kind::Uri,
        Some(i) if i >= 2 && l[..i] == *"shell" => Kind::Uri,
        _ => Kind::Path,
    }
}

fn host_of(url: &str) -> String {
    let rest = url.split("://").nth(1).unwrap_or(url);
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = host.rsplit('@').next().unwrap_or(host);
    let host = host.split(':').next().unwrap_or(host);
    host.trim_start_matches("www.").to_string()
}

/// What the user typed -> the stored target. `None` = nothing usable. A bare `example.com` becomes `https://example.com`.
fn normalize_target(input: &str) -> Option<String> {
    let s = input.trim().trim_matches('"').trim();
    if s.is_empty() {
        return None;
    }
    if classify(s) != Kind::Path {
        return Some(s.to_string());
    }
    let looks_domain = !s.contains([' ', '\\']) && !s.starts_with("..") && s.contains('.') && !s.ends_with('.') && !Path::new(s).exists();
    let tail = s.rsplit('.').next().unwrap_or("");
    let is_file_ext = matches!(tail.to_ascii_lowercase().as_str(), "exe" | "lnk" | "bat" | "cmd" | "msi" | "txt" | "pdf" | "url");
    if looks_domain && !is_file_ext {
        return Some(format!("https://{s}"));
    }
    Some(s.to_string())
}

/// Default tile name for a new target: the file stem for paths, the second-level domain for sites.
fn default_name(target: &str) -> String {
    let cap = |s: &str| {
        let mut c = s.chars();
        c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
    };
    match classify(target) {
        Kind::Url => {
            let host = host_of(target);
            let labels: Vec<&str> = host.split('.').collect();
            cap(if labels.len() >= 2 { labels[labels.len() - 2] } else { &host })
        }
        Kind::Uri => cap(target.split(':').next().unwrap_or(target)),
        Kind::Path => {
            let t = target.trim_end_matches(['\\', '/']);
            let stem = Path::new(t).file_stem().and_then(|s| s.to_str()).unwrap_or(t);
            if stem.is_empty() { target.to_string() } else { cap(stem) }
        }
    }
}

/// Bare names ("notepad.exe") are looked up on PATH; anything with a separator or scheme passes through.
fn resolve_path(p: &str) -> String {
    if p.contains(['\\', '/', ':']) {
        return p.to_string();
    }
    std::env::var_os("PATH")
        .and_then(|v| std::env::split_paths(&v).map(|d| d.join(p)).find(|f| f.is_file()))
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.to_string())
}

/// Icon source prefix of a site tile: `favicon:{host}` is fetched by `load_favicon`, anything else goes to the shell.
const FAV_PREFIX: &str = "favicon:";

/// What to hand to the shell (or `load_favicon`) to get this item's icon (None = draw a letter tile).
fn icon_source(it: &Item) -> Option<String> {
    if let Some(i) = &it.icon {
        return Some(i.clone());
    }
    match classify(&it.target) {
        Kind::Url => Some(host_of(&it.target)).filter(|h| !h.is_empty()).map(|h| format!("{FAV_PREFIX}{h}")),
        Kind::Path => Some(resolve_path(&it.target)),
        Kind::Uri if it.target.to_ascii_lowercase().starts_with("shell:") => Some(it.target.clone()),
        _ => None,
    }
}

/// `cfg.items` -> list (`None` = the key is absent or malformed: seed instead).
fn parse_items(v: Option<&Value>) -> Option<Vec<Item>> {
    let a = v?.as_array()?;
    Some(
        a.iter()
            .filter_map(|p| {
                let target = p.get("target")?.as_str()?.trim();
                if target.is_empty() {
                    return None;
                }
                let name = p.get("name").and_then(|n| n.as_str()).map(str::trim).filter(|n| !n.is_empty()).map(str::to_string).unwrap_or_else(|| default_name(target));
                Some(Item::new(&name, target, p.get("icon").and_then(|i| i.as_str()).map(str::to_string)))
            })
            .collect(),
    )
}

fn items_json(items: &[Item]) -> Value {
    Value::Array(
        items
            .iter()
            .map(|i| match &i.icon {
                Some(ic) => json!({"name": i.name, "target": i.target, "icon": ic}),
                None => json!({"name": i.name, "target": i.target}),
            })
            .collect(),
    )
}

/// The dock pins file (`[{name, path, icon?}]`) -> items.
fn items_from_dock(s: &str) -> Vec<Item> {
    let Ok(Value::Array(a)) = serde_json::from_str::<Value>(s) else { return Vec::new() };
    a.iter()
        .filter_map(|p| {
            let (name, target) = (p.get("name")?.as_str()?.trim(), p.get("path")?.as_str()?.trim());
            if name.is_empty() || target.is_empty() {
                return None;
            }
            Some(Item::new(name, target, p.get("icon").and_then(|v| v.as_str()).map(str::to_string)))
        })
        .collect()
}

fn builtin_items(ru: bool) -> Vec<Item> {
    let n = |r: &'static str, e: &'static str| if ru { r } else { e };
    vec![
        Item::new(n("Проводник", "Explorer"), "explorer.exe", None),
        Item::new(n("Браузер", "Browser"), "https://www.google.com", None),
        Item::new(n("Настройки", "Settings"), "ms-settings:", Some(SETTINGS_ICON.into())),
        Item::new(n("Калькулятор", "Calculator"), "calc.exe", Some(CALC_ICON.into())),
    ]
}

/// Worker: dock.json pins when present and non-empty, else the built-ins.
fn seed_items(ru: bool) -> Vec<Item> {
    let pins = std::env::var_os("APPDATA")
        .map(|a| Path::new(&a).join("macshell").join("dock.json"))
        .and_then(|f| std::fs::read_to_string(f).ok())
        .map(|s| items_from_dock(&s))
        .unwrap_or_default();
    if pins.is_empty() { builtin_items(ru) } else { pins }
}

/// Visible tiles before the "+" slot for `n` items in a grid of `cap` slots (the last slot always belongs to "+").
fn shown(n: usize, cap: usize) -> usize {
    n.min(cap.saturating_sub(1))
}

/// The first installed browser (Chrome, Edge, Firefox) for the preview; Edge's path when none is found (a letter tile then).
fn browser_item() -> Item {
    const BROWSERS: [(&str, &str); 4] = [
        ("Chrome", r"C:\Program Files\Google\Chrome\Application\chrome.exe"),
        ("Edge", r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe"),
        ("Edge", r"C:\Program Files\Microsoft\Edge\Application\msedge.exe"),
        ("Firefox", r"C:\Program Files\Mozilla Firefox\firefox.exe"),
    ];
    let (n, p) = BROWSERS.iter().find(|(_, p)| Path::new(p).is_file()).copied().unwrap_or(BROWSERS[1]);
    Item::new(n, p, None)
}

/// Preview content: standard Windows apps with their real shell icons (a missing app falls back to a letter tile).
fn sample_items() -> Vec<Item> {
    let app = |pfn: &str| Some(format!(r"shell:AppsFolder\{pfn}!App"));
    let mk = |n: &str, t: &str, icon: Option<String>| Item::new(n, t, icon);
    vec![
        mk("Проводник", "explorer.exe", None),
        browser_item(),
        mk("Терминал", "wt.exe", app("Microsoft.WindowsTerminal_8wekyb3d8bbwe")),
        mk("Блокнот", "notepad.exe", app("Microsoft.WindowsNotepad_8wekyb3d8bbwe")),
        mk("Калькулятор", "calc.exe", Some(CALC_ICON.into())),
        mk("Настройки", "ms-settings:", Some(SETTINGS_ICON.into())),
        mk("Paint", "mspaint.exe", app("Microsoft.Paint_8wekyb3d8bbwe")),
        mk("Фотографии", "ms-photos:", app("Microsoft.Windows.Photos_8wekyb3d8bbwe")),
        mk("Часы", "ms-clock:", app("Microsoft.WindowsAlarms_8wekyb3d8bbwe")),
        mk("Ножницы", "ms-screenclip:", app("Microsoft.ScreenSketch_8wekyb3d8bbwe")),
        mk("Магазин", "ms-windows-store:", app("Microsoft.WindowsStore_8wekyb3d8bbwe")),
        mk("Задачи", "taskmgr.exe", None),
        mk("Cmd", "cmd.exe", None),
        mk("PowerShell", r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe", None),
        mk("Панель", "control.exe", None),
        mk("Сведения", "msinfo32.exe", None),
    ]
}

/// Shell icons of the preview apps, decoded once per process (the gallery and the contact sheets build many previews).
fn preview_icon(src: &str) -> Option<Decoded> {
    static CACHE: std::sync::Mutex<Vec<(String, Option<(u32, u32, Vec<u8>)>)>> = std::sync::Mutex::new(Vec::new());
    let mut c = CACHE.lock().ok()?;
    if let Some((_, v)) = c.iter().find(|(s, _)| s == src) {
        return v.as_ref().map(|(w, h, b)| Decoded { w: *w, h: *h, bgra: b.clone() });
    }
    let d = load_icon(src);
    c.push((src.to_string(), d.as_ref().map(|d| (d.w, d.h, d.bgra.clone()))));
    d
}

/// Icon pixels from a shell HBITMAP are premultiplied on most systems and straight on some; D2D wants premultiplied.
/// An all-zero alpha channel means a DIB without alpha: make it opaque.
fn fix_alpha(px: &mut [u8]) {
    if px.chunks_exact(4).all(|p| p[3] == 0) {
        px.chunks_exact_mut(4).for_each(|p| p[3] = 255);
    } else if px.chunks_exact(4).any(|p| p[0] > p[3] || p[1] > p[3] || p[2] > p[3]) {
        for p in px.chunks_exact_mut(4) {
            let a = p[3] as u32;
            for c in &mut p[..3] {
                *c = ((*c as u32 * a + 127) / 255) as u8;
            }
        }
    }
}

// ================================================================ worker: site icon

/// Negative-cache lifetime of a host that answered without a usable icon.
const FAV_MISS_TTL: std::time::Duration = std::time::Duration::from_secs(24 * 3600);

/// Site icon: `%APPDATA%\macshell\cache\favicons\{host}.bin` (the raw downloaded bytes), else `/apple-touch-icon.png`, then
/// `/favicon.ico` of the site itself (no third-party service). A host that answered but had no decodable icon gets an empty
/// `{host}.none` marker for 24 h; a network failure leaves no marker (offline starts must not blank the cache).
fn load_favicon(host: &str) -> Option<Decoded> {
    if host.is_empty() || !host.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-') {
        return None; // the host becomes a file name
    }
    let dir = Path::new(&std::env::var_os("APPDATA")?).join("macshell").join("cache").join("favicons");
    let (hit, miss) = (dir.join(format!("{host}.bin")), dir.join(format!("{host}.none")));
    let decode = |b: &[u8]| decode_image_bytes(b, 128, 128);
    if let Some(d) = std::fs::read(&hit).ok().and_then(|b| decode(&b)) {
        return Some(d);
    }
    let fresh = std::fs::metadata(&miss).and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).is_some_and(|age| age < FAV_MISS_TTL);
    if fresh {
        return None;
    }
    let mut answered = false;
    for h in [host.to_string(), format!("www.{host}")] {
        for path in ["apple-touch-icon.png", "favicon.ico"] {
            match crate::net::http_get(&format!("https://{h}/{path}"), 6000) {
                Ok(b) => {
                    answered = true;
                    if let Some(d) = decode(&b) {
                        let _ = std::fs::create_dir_all(&dir);
                        let _ = std::fs::write(&hit, &b);
                        return Some(d);
                    }
                }
                Err(e) => answered |= e.starts_with("http "),
            }
        }
        if answered {
            break; // the bare host exists: www. would say the same
        }
    }
    if answered {
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::write(&miss, b"");
    }
    None
}

// ================================================================ worker: shell icon

fn load_icon(src: &str) -> Option<Decoded> {
    if let Some(host) = src.strip_prefix(FAV_PREFIX) {
        return load_favicon(host);
    }
    use std::ffi::c_void;
    use windows::Win32::Foundation::SIZE;
    use windows::Win32::Graphics::Gdi::*;
    use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
    use windows::Win32::UI::Shell::{IShellItemImageFactory, SHCreateItemFromParsingName, SIIGBF_BIGGERSIZEOK, SIIGBF_ICONONLY};
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let f: IShellItemImageFactory = SHCreateItemFromParsingName(&HSTRING::from(src), None).ok()?;
        let hb = f.GetImage(SIZE { cx: 96, cy: 96 }, SIIGBF_ICONONLY | SIIGBF_BIGGERSIZEOK).ok()?;
        let mut bm = BITMAP::default();
        GetObjectW(hb, std::mem::size_of::<BITMAP>() as i32, Some(&mut bm as *mut _ as *mut c_void));
        let (w, h) = (bm.bmWidth, bm.bmHeight);
        let mut bi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER { biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32, biWidth: w, biHeight: -h, biPlanes: 1, biBitCount: 32, biCompression: BI_RGB.0, ..Default::default() },
            ..Default::default()
        };
        if w <= 0 || h <= 0 || w > 1024 || h > 1024 {
            let _ = DeleteObject(hb);
            return None;
        }
        let mut px = vec![0u8; (w * h * 4) as usize];
        let dc = GetDC(None);
        let rows = GetDIBits(dc, hb, 0, h as u32, Some(px.as_mut_ptr() as *mut c_void), &mut bi, DIB_RGB_COLORS);
        ReleaseDC(None, dc);
        let _ = DeleteObject(hb);
        if rows == 0 {
            return None;
        }
        fix_alpha(&mut px);
        Some(Decoded { w: w as u32, h: h as u32, bgra: px })
    }
}

// ================================================================ geometry

struct Grid {
    cols: u32,
    rows: u32,
    area: Rect,
    icon: f32,
}
fn grid(size: Size) -> Grid {
    match size {
        Size::S => Grid { cols: 2, rows: 2, area: Rect::new(12.0, 12.0, 146.0, 146.0), icon: 44.0 },
        Size::M => Grid { cols: 4, rows: 2, area: Rect::new(12.0, 12.0, 332.0, 146.0), icon: 44.0 },
        Size::L => Grid { cols: 4, rows: 4, area: Rect::new(12.0, 12.0, 332.0, 332.0), icon: 48.0 },
    }
}
impl Grid {
    fn cap(&self) -> usize {
        (self.cols * self.rows) as usize
    }
    fn cell(&self, i: usize) -> Rect {
        let (c, r) = (i as u32 % self.cols, i as u32 / self.cols);
        let (w, h) = (self.area.w / self.cols as f32, self.area.h / self.rows as f32);
        Rect::new(self.area.x + c as f32 * w, self.area.y + r as f32 * h, w, h)
    }
    /// (icon rect, label rect) inside a cell
    fn parts(&self, cell: Rect) -> (Rect, Rect) {
        let total = self.icon + 4.0 + 14.0;
        let top = cell.y + (cell.h - total) / 2.0;
        (Rect::new(cell.cx() - self.icon / 2.0, top, self.icon, self.icon), Rect::new(cell.x, top + self.icon + 4.0, cell.w, 14.0))
    }
}

// ================================================================ widget

#[derive(Default)]
struct Shortcuts {
    started: bool,
    items: Vec<Item>,
    seeding: Option<Pending<Vec<Item>>>,
    icons: HashMap<String, Bitmap>,
    /// icon sources already requested (a failed lookup is not retried)
    asked: HashSet<String>,
    job: Option<Pending<Vec<(String, Option<Decoded>)>>>,
    prompt: Option<Pending<Option<String>>>,
    /// slot under the cursor (items, then the "+" slot) and its eased hover amount
    hv: HoverAnim,
    /// `items` changed in `tick` (which cannot write the config): flushed from `mouse` / `draw`
    cfg_dirty: bool,
    ver: u64,
}

/// Take the result of a job once; a job that died without a result is dropped.
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

impl Shortcuts {
    fn sync_cfg(&mut self, cx: &mut Cx) -> bool {
        if self.cfg_dirty {
            self.cfg_dirty = false;
            cx.cfg_set("items", items_json(&self.items));
            return true;
        }
        false
    }

    fn persist(&mut self, cx: &mut Cx) {
        self.ver += 1;
        cx.cfg_set("items", items_json(&self.items));
    }

    /// Fallback tile: a coloured squircle with the first letter (URL: a faint globe behind the host's letter).
    fn letter_tile(cx: &Cx, r: Rect, it: &Item) {
        let rad = r.w * 0.225;
        let base = PALETTE[it.color % PALETTE.len()];
        cx.gradient_rrect(r, rad, base.mix(WHITE, 0.20), base.mix(BLACK, 0.16), 180.0);
        cx.rrect_stroke(r.inset(0.5), rad, 1.0, WHITE.alpha(0.20));
        let url = classify(&it.target) == Kind::Url;
        let letter = if url { host_of(&it.target) } else { it.name.clone() };
        let letter: String = letter.chars().next().map(|c| c.to_uppercase().collect()).unwrap_or_default();
        if url {
            globe(cx, r.center(), r.w * 0.36, WHITE.alpha(0.34));
        }
        cx.text(r, &letter, Style::new(r.w * 0.5, 650).center(), WHITE);
    }

    fn paint_item(&self, cx: &Cx, r: Rect, it: &Item) {
        match icon_source(it).and_then(|s| self.icons.get(&s)) {
            Some(b) => cx.bitmap(r, b, Fit::Contain),
            None => Self::letter_tile(cx, r, it),
        }
    }

    fn paint_plus(cx: &Cx, r: Rect, k: f32) {
        let rad = r.w * 0.225;
        cx.rrect(r, rad, WHITE.alpha(0.12 + 0.04 * k));
        cx.rrect_stroke(r.inset(0.5), rad, 1.0, WHITE.alpha(0.26));
        cx.icon(Glyph::Plus, r.centered(r.w * 0.44, r.w * 0.44), TEXT.fade(0.7 + 0.3 * k));
    }
}

/// A small wireframe globe: outline, equator, two parallels and a meridian ellipse.
fn globe(cx: &Cx, c: Pt, r: f32, col: Color) {
    let w = (r * 0.09).max(1.0);
    cx.circle_stroke(c, r, w, col);
    cx.line((c.0 - r, c.1), (c.0 + r, c.1), w, col);
    cx.line((c.0, c.1 - r), (c.0, c.1 + r), w, col);
    for k in [-0.5f32, 0.5] {
        let half = (r * r - (r * k) * (r * k)).sqrt();
        cx.line((c.0 - half, c.1 + r * k), (c.0 + half, c.1 + r * k), w, col);
    }
    let n = 24;
    let pt = |i: usize| {
        let a = i as f32 / n as f32 * std::f32::consts::TAU;
        (c.0 + a.cos() * r * 0.46, c.1 + a.sin() * r)
    };
    for i in 0..n {
        cx.line(pt(i), pt(i + 1), w, col);
    }
}

impl Widget for Shortcuts {
    fn draw(&mut self, cx: &mut Cx) -> Result<()> {
        if self.sync_cfg(cx) {
            cx.waker().wake(); // let the framework persist the config now
        }
        let g = grid(cx.size);
        let n = shown(self.items.len(), g.cap());
        let busy = self.seeding.is_some();
        for i in 0..n {
            let (icon, label) = g.parts(g.cell(i));
            let it = &self.items[i];
            self.paint_item(cx, icon, it);
            cx.text(label.inset_xy(2.0, 0.0), &it.name, Style::CAPTION.center().ellipsis(), TEXT);
        }
        if !busy {
            let (icon, label) = g.parts(g.cell(n));
            Self::paint_plus(cx, icon, 0.0);
            cx.text(label.inset_xy(2.0, 0.0), cx.t("Добавить", "Add"), Style::CAPTION.center().ellipsis(), DIM);
        }
        Ok(())
    }

    fn overlay(&mut self, cx: &mut Cx) -> Result<()> {
        let g = grid(cx.size);
        let n = shown(self.items.len(), g.cap());
        let pre = if cx.preview { cx.cfg_f64("preview_hot").map(|v| v as usize) } else { None };
        // the slot under the cursor and the one it just left (still fading out): a plate behind + the icon scaled up around its centre
        for slot in 0..=n {
            let k = if pre == Some(slot) { 1.0 } else { self.hv.get(slot) };
            if k <= 0.0 {
                continue;
            }
            let (icon, _) = g.parts(g.cell(slot));
            cx.rrect(icon.inset(-7.0), 15.0, WHITE.alpha(0.14 * k));
            let s = 1.0 + 0.12 * k;
            let big = icon.centered(icon.w * s, icon.h * s);
            if slot < n {
                let it = &self.items[slot];
                self.paint_item(cx, big, it);
            } else {
                Self::paint_plus(cx, big, k);
            }
        }
        Ok(())
    }

    fn tick(&mut self, cx: &Cx) -> Tick {
        if !self.started {
            self.started = true;
            if cx.preview {
                self.items = sample_items();
                for src in self.items.iter().filter_map(icon_source).collect::<Vec<_>>() {
                    if let Some(b) = preview_icon(&src).and_then(|d| cx.image(&d).ok()) {
                        self.icons.insert(src, b);
                    }
                }
                self.ver += 1;
            } else if let Some(items) = parse_items(cx.cfg.get("items")) {
                self.items = items;
                self.ver += 1;
            } else {
                let ru = cx.lang == Lang::Ru;
                self.seeding = Some(cx.spawn(move || seed_items(ru)));
                self.ver += 1;
            }
        }
        let seeding = self.seeding.is_some();
        if let Some(items) = poll(&mut self.seeding) {
            self.items = items;
            self.ver += 1;
        } else if seeding && self.seeding.is_none() && !cx.preview {
            self.items = builtin_items(cx.lang == Lang::Ru); // the seed job died: fall back
            self.ver += 1;
        }
        // new tile typed into the prompt
        if let Some(r) = poll(&mut self.prompt) {
            if let Some(target) = r.and_then(|t| normalize_target(&t)) {
                let name = default_name(&target);
                let icon = None;
                self.items.push(Item::new(&name, &target, icon));
                self.cfg_dirty = true;
                self.ver += 1;
            }
        }
        // icons that are still missing: one worker batch at a time
        if !cx.preview {
            if let Some(batch) = poll(&mut self.job) {
                for (src, d) in batch {
                    if let Some(b) = d.and_then(|d| cx.image(&d).ok()) {
                        self.icons.insert(src, b);
                        self.ver += 1;
                    }
                }
            }
            if self.job.is_none() {
                let mut need: Vec<String> = Vec::new();
                for s in self.items.iter().filter_map(icon_source) {
                    if !self.asked.contains(&s) && !need.contains(&s) {
                        need.push(s);
                    }
                }
                // shell icons first: a slow site must not hold them back (favicons get the next batch)
                if need.iter().any(|s| !s.starts_with(FAV_PREFIX)) {
                    need.retain(|s| !s.starts_with(FAV_PREFIX));
                }
                if !need.is_empty() {
                    self.asked.extend(need.iter().cloned());
                    self.job = Some(cx.spawn(move || need.into_iter().map(|s| { let d = load_icon(&s); (s, d) }).collect()));
                }
            }
        }
        if self.hv.step(cx) {
            Tick::Fps(60) // the hover plate eases in / out
        } else {
            Tick::Never
        }
    }

    fn mouse(&mut self, ev: &MouseEv, cx: &mut Cx) -> bool {
        self.sync_cfg(cx);
        let g = grid(cx.size);
        let n = shown(self.items.len(), g.cap());
        let slot = if ev.kind == MouseKind::Leave { None } else { (0..=n).find(|&i| g.cell(i).contains(ev.x, ev.y)) };
        let mut changed = self.hv.set(slot, cx);
        if let Some(i) = slot {
            let cell = g.cell(i);
            if ev.clicked(cell) {
                if i < n {
                    cx.invoke_system(Action::Open(self.items[i].target.clone()));
                } else if self.prompt.is_none() {
                    self.prompt = Some(cx.prompt_text_ex(cx.t("Новый ярлык", "New shortcut"), "", false, cx.t("Путь или адрес (https://…)", "Path or URL (https://…)"), cx.t("Добавить", "Add")));
                }
            } else if ev.right_clicked(cell) && i < n {
                self.items.remove(i);
                self.persist(cx);
                changed = true;
            }
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
    fn classifies_targets() {
        assert_eq!(classify("https://example.com/a"), Kind::Url);
        assert_eq!(classify("HTTP://x.y"), Kind::Url);
        assert_eq!(classify("ms-settings:"), Kind::Uri);
        assert_eq!(classify("mailto:a@b.c"), Kind::Uri);
        assert_eq!(classify(r"shell:AppsFolder\X!App"), Kind::Uri);
        assert_eq!(classify(r"C:\Windows\notepad.exe"), Kind::Path);
        assert_eq!(classify("C:/Users"), Kind::Path);
        assert_eq!(classify("notepad.exe"), Kind::Path);
        assert_eq!(classify(r"\\server\share"), Kind::Path);
    }

    #[test]
    fn normalizes_input() {
        assert_eq!(normalize_target("  "), None);
        assert_eq!(normalize_target("github.com").as_deref(), Some("https://github.com"));
        assert_eq!(normalize_target("\"C:\\Program Files\\App\\app.exe\"").as_deref(), Some("C:\\Program Files\\App\\app.exe"));
        assert_eq!(normalize_target("https://a.b/c").as_deref(), Some("https://a.b/c"));
        assert_eq!(normalize_target("ms-settings:network").as_deref(), Some("ms-settings:network"));
        assert_eq!(normalize_target("notepad.exe").as_deref(), Some("notepad.exe"));
        assert_eq!(normalize_target("calc").as_deref(), Some("calc"));
    }

    #[test]
    fn names_and_hosts() {
        assert_eq!(host_of("https://www.docs.google.com:8080/x?y"), "docs.google.com");
        assert_eq!(default_name("https://www.github.com/a"), "Github");
        assert_eq!(default_name("https://docs.google.com"), "Google");
        assert_eq!(default_name(r"C:\Tools\sublime_text.exe"), "Sublime_text");
        assert_eq!(default_name(r"D:\Projects\"), "Projects");
        assert_eq!(default_name("ms-settings:"), "Ms-settings");
    }

    #[test]
    fn last_slot_is_plus() {
        assert_eq!(shown(2, 8), 2);
        assert_eq!(shown(7, 8), 7);
        assert_eq!(shown(8, 8), 7);
        assert_eq!(shown(30, 4), 3);
        assert_eq!(shown(0, 4), 0);
    }

    #[test]
    fn items_roundtrip_and_dock_seed() {
        let items = vec![Item::new("A", "a.exe", None), Item::new("B", "ms-settings:", Some("shell:x".into()))];
        let v = items_json(&items);
        assert_eq!(parse_items(Some(&v)).unwrap(), items);
        assert!(parse_items(None).is_none());
        assert_eq!(parse_items(Some(&json!([]))).unwrap().len(), 0);
        assert_eq!(parse_items(Some(&json!([{"target": "https://x.org"}]))).unwrap()[0].name, "X");
        let dock = r#"[{"name":"Explorer","path":"explorer.exe"},{"name":"Calc","path":"calc.exe","icon":"shell:AppsFolder\\C"},{"name":"","path":"x"}]"#;
        let d = items_from_dock(dock);
        assert_eq!(d.len(), 2);
        assert_eq!(d[1].icon.as_deref(), Some(r"shell:AppsFolder\C"));
        assert!(items_from_dock("not json").is_empty());
    }

    #[test]
    fn icon_sources() {
        assert_eq!(icon_source(&Item::new("U", "https://www.a.b/x", None)).as_deref(), Some("favicon:a.b"));
        assert_eq!(icon_source(&Item::new("U", "https://a.b", Some("shell:z".into()))).as_deref(), Some("shell:z"));
        assert!(load_favicon("a/../b").is_none() && load_favicon("").is_none()); // host -> file name: no path chars, no I/O
        assert_eq!(icon_source(&Item::new("S", "ms-settings:", None)), None);
        assert_eq!(icon_source(&Item::new("S", "ms-settings:", Some("shell:q".into()))).as_deref(), Some("shell:q"));
        assert_eq!(icon_source(&Item::new("P", r"C:\x\y.exe", None)).as_deref(), Some(r"C:\x\y.exe"));
    }

    #[test]
    fn alpha_fix() {
        let mut opaque = [10u8, 20, 30, 0, 40, 50, 60, 0];
        fix_alpha(&mut opaque);
        assert_eq!(opaque, [10, 20, 30, 255, 40, 50, 60, 255]);
        let mut pre = [10u8, 10, 10, 128, 0, 0, 0, 0];
        fix_alpha(&mut pre);
        assert_eq!(pre, [10, 10, 10, 128, 0, 0, 0, 0]); // already premultiplied: untouched
        let mut straight = [200u8, 100, 0, 128, 0, 0, 0, 0];
        fix_alpha(&mut straight);
        assert_eq!(straight, [100, 50, 0, 128, 0, 0, 0, 0]);
    }

    #[test]
    fn palette_pick_is_stable() {
        assert_eq!(color_for("Chrome"), color_for("Chrome"));
        assert!(color_for("anything") < PALETTE.len());
    }

    /// Real shell icon of a system exe: premultiplied, not empty (needs a desktop session; skipped silently when the shell has none).
    #[test]
    fn explorer_icon_loads() {
        if let Some(d) = load_icon(r"C:\Windows\explorer.exe") {
            assert!(d.w >= 16 && d.bgra.len() == (d.w * d.h * 4) as usize);
            assert!(d.bgra.chunks_exact(4).any(|p| p[3] > 0));
            assert!(d.bgra.chunks_exact(4).all(|p| p[0] <= p[3] && p[1] <= p[3] && p[2] <= p[3]));
        }
    }
}
