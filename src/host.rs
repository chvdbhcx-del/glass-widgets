//! The one top-level layered window, timers, presenting, click-through toggling, fullscreen guard, `--bench` and everything about
//! arranging the desk: drag & drop with a spring settle, the context menu, edit mode (jiggle, "−", dim, settings strip), the gallery
//! ("Edit Widgets" with live previews and a search box) and persistence of the layout / settings.
//!
//! Idle behaviour: the UI thread sits in GetMessage (kernel wait, same as MsgWaitForMultipleObjects with no handles).
//! Wake-ups: WM_BACKDROP (only when the wallpaper pixels behind a card changed, <= --backdrop-fps), a widget schedule timer ONLY
//! while something animates, a 100 ms cursor poll (16 ms near a card), a 500 ms fullscreen guard, a 2 s config.json poll and a 30 s day
//! check. Edit mode adds a 33 ms jiggle timer (16 ms while a card moves) and a 200 ms gallery timer; none of them runs outside edit mode.
//!
//! Coordinates: the scene canvas is the monitor (card px = monitor px); the layered window only covers `win`, the bounding box of the
//! cards, and grows to the whole monitor while a card is dragged / settling and in edit mode.
use crate::backdrop::*;
use crate::cards::*;
use crate::gfx::*;
use crate::grid;
use crate::layout;
use crate::input;
use crate::monitors;
use crate::motion::{self, MatState};
use crate::prompt::{Act, Key, Mods};
use crate::layout::{AccentMode, AccentScope};
use crate::menu::{self, MAct, CMD_DELETE, CMD_EDIT, CMD_SIZE};
use crate::tray::{self, TrayCmd};
use crate::ui::{self, Hit, SAct, SKey, Ui};
use crate::update::{self, UpdateState};
use crate::widget::{Button, Lang, MouseKind, Now, Rect, Size, WM_WIDGET};
use std::cell::RefCell;
use std::collections::HashMap;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::time::{Duration, Instant, SystemTime};
use windows::core::{w, Result, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::DataExchange::{CloseClipboard, GetClipboardData, OpenClipboard};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX};
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentProcess, GetCurrentThreadId, GetProcessTimes};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, GetDpiForSystem, GetDpiForWindow, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

const WM_MOUSELEAVE: u32 = 0x02A3; // Win32_UI_Controls in the windows crate; not worth the feature
const WM_DPICHANGED: u32 = 0x02E0;
/// posted by the mouse handler: run the context menu of card `wparam` at screen point `lparam` (outside of the App borrow)
const WM_MENU: u32 = WM_APP + 3;
/// posted by `input::prompt_ex` (a worker thread): a text prompt waits in `input::take_request` to be shown on the canvas
const WM_PROMPT: u32 = WM_APP + 21;
/// posted by the "Save current layout as..." prompt thread when the user typed a name (the name waits in `PROFILE_NAME`)
const WM_PROFILE_NAME: u32 = WM_APP + 20;
/// the longest layout name
const PROFILE_NAME_MAX: usize = 40;
static PROFILE_NAME: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
const T_HIT: usize = 1;
const T_GUARD: usize = 2;
const T_TICK: usize = 3;
const T_DAY: usize = 4;
const T_BENCH: usize = 5;
const T_ANIM: usize = 6;
const T_UI: usize = 7;
const T_CFG: usize = 8;
const T_SHRINK: usize = 9;
const T_TOAST: usize = 10;
const T_REPLACE: usize = 11;
/// the prompt: 16 ms while it fades in / out, 500 ms (the caret blink) while it waits for the user
const T_PROMPT: usize = 12;
/// the context menu: 16 ms while it fades or waits to open a submenu, off otherwise
const T_MENU: usize = 13;
/// throttled apply of the look sliders (settings panel)
const T_LOOK: usize = 40;
/// commits a look slider moved from the keyboard / wheel once the burst of key repeats is over
const T_LCOMMIT: usize = 41;
const LCOMMIT_AFTER: u32 = 400;
/// at most one live look apply per this long while a slider is dragged
const LOOK_EVERY: Duration = Duration::from_millis(100);
const NEAR_PX: i32 = 8;
/// bits 62 / 63 of the backdrop dirty mask belong to the gallery panel and the settings strip
const BIT_GALLERY: u64 = 1 << 62;
const BIT_TOOLBAR: u64 = 1 << 63;
const MAX_CARDS: usize = 62;
const DRAG_PX: i32 = 5;
const DIM_ALPHA: f32 = 0.28;

pub struct Opts {
    pub appearance: Option<crate::motion::AppearancePref>,
    pub fps: u32,
    pub bench: Option<u32>,
    pub scale: Option<f32>,
    pub no_config: bool,
    pub time: Option<Now>,
    pub no_hotkeys: bool,
}

#[derive(Default)]
struct Stats {
    backdrop_batches: u64,
    card_redraws: u64,
    clock_frames: u64,
    present_ms: f64,
    render_ms: f64,
    lum_reads: u64,
}

#[derive(Clone, Copy)]
struct Spring {
    x: f32,
    y: f32,
    vx: f32,
    vy: f32,
    tx: f32,
    ty: f32,
}
impl Spring {
    /// One integration step (semi-implicit Euler, two substeps); true when it came to rest on the target.
    fn step(&mut self, dt: f32) -> bool {
        const K: f32 = 420.0;
        const C: f32 = 30.0;
        let h = dt / 2.0;
        for _ in 0..2 {
            self.vx += (K * (self.tx - self.x) - C * self.vx) * h;
            self.vy += (K * (self.ty - self.y) - C * self.vy) * h;
            self.x += self.vx * h;
            self.y += self.vy * h;
        }
        let done = (self.tx - self.x).abs() < 0.4 && (self.ty - self.y).abs() < 0.4 && self.vx.abs() < 14.0 && self.vy.abs() < 14.0;
        if done {
            self.x = self.tx;
            self.y = self.ty;
        }
        done
    }
}

/// A card that materialises (condenses out of blur, rim sweep) or dematerialises; `t0` may lie in the future (staggered start-up).
#[derive(Clone, Copy)]
struct Pop {
    t0: Instant,
    out: bool,
}
impl Pop {
    fn at(&self, now: Instant, reduced: bool) -> MatState {
        motion::materialize(now.saturating_duration_since(self.t0).as_secs_f32(), self.out, reduced)
    }
}



#[derive(Clone, Copy, PartialEq)]
enum Press {
    None,
    /// a widget got the press (it reported a change): no drag
    Widget(usize),
    /// pressed on a card, not yet moved far enough to drag
    Cand { uid: usize, x: i32, y: i32 },
    Drag,
    /// pressed on a gallery preview
    Preview { k: usize, x: i32, y: i32 },
    /// pressed on an empty part of the gallery / settings panel: dragging it. (x, y) = the pointer at the press (canvas px), (ox, oy) =
    /// the panel's top left then (logical px)
    Move { x: i32, y: i32, ox: f32, oy: f32 },
    /// pressed on a UI control (acts on release when still over it)
    Ui(Hit),
}

struct Drag {
    uid: usize,
    /// pointer offset inside the card, px
    off: (i32, i32),
    /// pointer, monitor px
    cur: (i32, i32),
    /// the free cell the card would land in
    target: Option<grid::Place>,
    /// the card was created by the gallery for this drag (removed again when dropped nowhere)
    fresh: bool,
    /// cell the card came from (None for a fresh card)
    from: Option<(i32, i32)>,
    /// card velocity px/s (low-passed): drives the light inertia
    vel: (f32, f32),
}

struct MenuReq {
    uid: usize,
    x: i32,
    y: i32,
    sizes: Vec<Size>,
    cur: Size,
    editing: bool,
    lang: Lang,
}

struct App {
    gfx: Gfx,
    scene: Scene,
    ui: Ui,
    backdrop: Option<Box<dyn Backdrop>>,
    hwnd: HWND,
    mem_dc: HDC,
    dib: HBITMAP,
    bits: *mut u8,
    /// LUID of the GPU the UI device lives on (None = unknown): a monitor on another GPU gets the GDI capture
    gfx_luid: Option<i64>,
    /// whose layout `items` is: None = the primary monitor, else a device name (`monitorLayouts`); mirrors `layout::slot()`
    slot: Option<String>,
    mon: RECT,  // monitor, desktop px
    work: RECT, // work area, desktop px
    s: f32,
    /// scale of the cards (`s` x the look's `card_k`); `s` is the host UI / DPI scale
    cs: f32,
    dims: grid::Dims,
    win: RECT, // window rect inside the canvas (monitor px)
    win_dirty: bool,
    items: Vec<layout::Item>, // the stored layout (shown cards + the ones without room)
    settings: layout::Settings,
    cfg_mtime: Option<SystemTime>,
    own_write: bool,
    hot: Option<usize>,     // card under the cursor (mouse events)
    pressed: Option<usize>, // card that owns a pressed button (mouse capture)
    press: Press,
    drag: Option<Drag>,
    springs: HashMap<usize, Spring>,
    pops: HashMap<usize, Pop>,
    tracking: bool,
    interactive: bool,
    near: bool,
    visible: bool,
    /// hidden from the tray menu ("Hide Widgets"): the guard keeps the layer hidden until it is shown again
    user_hidden: bool,
    first_present: bool,
    last_lum: Instant,
    last_day: u32,
    guard_n: u32,
    fixed_time: Option<Now>,
    fps: u32,
    use_dupl: bool,
    // edit mode
    editing: bool,
    gal_t: f32,
    dim_t: f32,
    prev_fg: HWND,
    reduced: bool,
    t_edit: Instant,
    /// 0..1 strength of the edit-mode jiggle (eased in and out)
    jig_amp: f32,
    last_anim: Instant,
    anim_period: u32,
    ui_tick: u32,
    esc_down: bool,
    in_menu: bool,
    app_front: bool,
    /// the layer has been the foreground window at least once in this edit session (else no auto-exit on focus loss)
    was_fg: bool,
    /// settings panel: the look as stored (`widgetsLook`), the base look it is relative to (defaults + glass.json), slider changes
    /// not yet applied to the cards, the time of the last apply, and whether the panel was opened from the gallery (Done goes back there)
    look_cfg: serde_json::Value,
    look_base: crate::glass::GlassParams,
    look_dirty: bool,
    look_last: Instant,
    settings_back: bool,
    /// slider moved from the keyboard / wheel whose value is not in config.json yet (0 = Glass)
    look_commit: Option<usize>,
    /// where the gallery / settings panel offsets were last written to config.json
    panels_saved: ((f32, f32), (f32, f32)),
    /// the update check in flight was the silent one at start (an available update is a toast, a failure is not shown)
    update_silent: bool,
    /// the panel state to go back to when "Update now" fails (the available update with its asset)
    update_prev: Option<UpdateState>,
    /// `accentMode` "wallpaper": the wallpaper file (and its time) the accent was taken from; a change recomputes it
    accent_key: Option<(String, Option<SystemTime>)>,
    /// live-backdrop accent: when it was last taken and what was picked (a new pick that is close to it is ignored: no flicker on video wallpapers)
    accent_last: Instant,
    accent_cur: Option<[u8; 3]>,
    st: Stats,
    bench: Option<u32>,
    t0: Instant,
    cpu0: u64,
    cpu_mark: Option<(Instant, u64)>,
    /// the text prompt on the canvas (`ui.prompt` is its state): where the answer goes (a dropped sender = a cancel), who had the foreground,
    /// whether the layer ever got it, when it opened, the timer period, the veil last drawn, a pending UTF-16 high surrogate, the last click
    prompt_reply: Option<std::sync::mpsc::Sender<input::Reply>>,
    prompt_fg: HWND,
    prompt_was_fg: bool,
    prompt_t0: Instant,
    prompt_period: u32,
    prompt_last: Instant,
    prompt_scrim: f32,
    prompt_hi: Option<u16>,
    prompt_click: Option<(Instant, i32, i32)>,
    /// the context menu on the canvas (`ui.menu` is its state): the card it belongs to, the timer period, the last tick and the canvas
    /// rectangle it was last drawn in. The foreground hand-off fields of the prompt (`prompt_fg`, `prompt_was_fg`, `prompt_t0`) serve it too:
    /// the two are never up together.
    menu_uid: usize,
    menu_period: u32,
    menu_last: Instant,
    menu_shown: Option<RECT>,
}

thread_local! { static APP: RefCell<Option<App>> = const { RefCell::new(None) }; }
/// Registered id of Explorer's `TaskbarCreated` broadcast (0 until the tray icon is set up): the tray icon is added again on it.
static TASKBAR_CREATED: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

pub fn run(o: Opts) -> i32 {
    match unsafe { setup(&o) } {
        Ok(()) => {
            unsafe {
                let mut msg = MSG::default();
                while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
            APP.with(|a| a.borrow_mut().take()); // stops the capture thread
            0
        }
        Err(e) => {
            eprintln!("mswidgets: start failed: {e}");
            1
        }
    }
}

fn reduced_motion() -> bool {
    crate::widget::reduced_motion()
}

struct Monitor {
    out: Option<OutputInfo>,
    rect: RECT,
    work: RECT,
    /// DPI scale of this monitor
    scale: f32,
    /// whose layout the desk shows: None = the primary monitor (`widgetsLayout`), else the device name (`monitorLayouts`)
    slot: Option<String>,
    /// how many monitors are connected, and the display position (0 = primary) of this one
    count: usize,
    pos: usize,
}

/// The monitor the desk lives on (`widgetsMonitor`, see monitors.rs): rect, work area, DPI and capture output all come from it.
unsafe fn monitor() -> Monitor {
    let list = monitors::enumerate();
    let sel = monitors::pick(&list);
    let mi = sel.and_then(|i| list.get(i));
    let out = find_output(mi.filter(|m| !m.primary).map(|m| m.name.as_str()));
    let rect = match (&out, mi) {
        (Some(x), _) => x.rect,
        (None, Some(m)) => m.rect,
        (None, None) => RECT { left: 0, top: 0, right: GetSystemMetrics(SM_CXSCREEN), bottom: GetSystemMetrics(SM_CYSCREEN) },
    };
    let work = match mi {
        Some(m) => m.work,
        None => {
            let mut w = rect;
            let _ = SystemParametersInfoW(SPI_GETWORKAREA, 0, Some(&mut w as *mut RECT as *mut std::ffi::c_void), SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0));
            w
        }
    };
    let dpi = mi.filter(|m| m.hmon != 0).and_then(|m| {
        let (mut x, mut y) = (0u32, 0u32);
        GetDpiForMonitor(HMONITOR(m.hmon as *mut _), MDT_EFFECTIVE_DPI, &mut x, &mut y).ok().map(|_| x)
    });
    Monitor {
        out,
        rect,
        work,
        scale: dpi.filter(|d| *d > 0).unwrap_or_else(|| GetDpiForSystem()) as f32 / 96.0,
        slot: sel.and_then(|i| monitors::slot_of(&list, i)),
        count: list.len().max(1),
        pos: sel.map_or(0, |i| monitors::position(&list, i)),
    }
}

fn grid_dims(mon: &RECT, work: &RECT, s: f32) -> grid::Dims {
    grid::dims((mon.right - mon.left) as f32 / s, (work.bottom - mon.top) as f32 / s)
}

/// Lay the stored items out on the grid: every item that has room gets a CardSpec (items without room stay stored, not drawn);
/// the stored col / row are updated to what is shown.
fn place_items(items: &mut [layout::Item], dims: &grid::Dims, s: f32) -> Vec<CardSpec> {
    let res = grid::resolve_all(dims, &items.iter().map(|i| (i.size, i.col as i32, i.row as i32)).collect::<Vec<_>>());
    let mut out = Vec::new();
    for (it, p) in items.iter_mut().zip(res) {
        let Some(p) = p else { continue };
        if out.len() >= MAX_CARDS {
            break;
        }
        it.col = p.col as u32;
        it.row = p.row as u32;
        let (x, y, w, h) = cell_rect(it.size, p.col, p.row, s);
        out.push(CardSpec { id: it.id.clone(), type_id: it.type_id.clone(), size: it.size, cfg: it.cfg.clone(), x, y, w, h });
    }
    out
}

/// A capture output can share textures with the UI device when the device's GPU is unknown (`owner` None: default adapter / WARP, the old
/// behaviour) or is the output's own.
fn same_gpu(owner: Option<i64>, output_luid: i64) -> bool {
    owner.is_none_or(|l| l == output_luid)
}

/// `owner`: LUID of the GPU the UI device lives on. The WGC / duplication textures are shared with that device, which only works on the same GPU;
/// a monitor on another adapter gets the GDI capture (CPU copy, any monitor) instead.
/// `captureFps` of the settings wins (a fixed rate); else auto, capped by `--backdrop-fps`.
fn capture_rate(cli_fps: u32, s: &layout::Settings) -> Rate {
    match s.capture_fps {
        Some(n) => Rate { fps: n, auto: false },
        None => Rate { fps: cli_fps, auto: true },
    }
}

unsafe fn make_backdrop(gfx: &Gfx, mon: &Monitor, w: i32, h: i32, live: bool, rate: Rate, rects: &[RECT], margin: i32, owner: Option<i64>) -> Box<dyn Backdrop> {
    if live {
        // Graphics Capture of the desktop window is the default (the layer does not see itself, so it can stay visible to screenshots);
        // `MSW_CAPTURE=gdi` selects GDI region capture of the screen (the layer is then excluded from captures), `dxgi` Desktop Duplication
        let pick = std::env::var("MSW_CAPTURE").unwrap_or_default().to_ascii_lowercase();
        let out = mon.out.as_ref().filter(|x| same_gpu(owner, x.luid));
        if mon.out.is_some() && out.is_none() {
            eprintln!("mswidgets: the monitor is on another GPU than the UI device, using GDI capture");
        }
        if let (true, Some(x), false) = (pick.is_empty() || pick == "wgc", out, gfx.warp) {
            match WgcBackdrop::new(gfx, x, rects, margin, rate) {
                Ok(d) => return Box::new(d),
                Err(e) => eprintln!("mswidgets: graphics capture unavailable ({e}), trying GDI"),
            }
        }
        if (pick != "dxgi" || (mon.out.is_some() && out.is_none())) && !gfx.warp {
            match GdiBackdrop::new(gfx, mon.rect, rects, margin, rate) {
                Ok(d) => return Box::new(d),
                Err(e) => eprintln!("mswidgets: GDI capture unavailable ({e}), trying duplication"),
            }
        }
        if let (Some(x), false) = (out, gfx.warp) {
            match DuplBackdrop::new(gfx, x, RECT { left: 0, top: 0, right: w, bottom: h }, rects, margin, rate) {
                Ok(d) => return Box::new(d),
                Err(e) => eprintln!("mswidgets: desktop duplication unavailable ({e}), plain base"),
            }
        } else {
            eprintln!("mswidgets: no hardware output found, plain base");
        }
    }
    match PlainBackdrop::new(gfx, w as u32, h as u32) {
        Ok(p) => Box::new(p),
        Err(e) => {
            eprintln!("mswidgets: plain backdrop: {e}");
            std::process::exit(1)
        }
    }
}

unsafe fn setup(o: &Opts) -> Result<()> {
    // `widgetsMonitor`: which monitor the desk lives on (a headless / --no-config run always uses the primary one)
    monitors::set_choice(if o.no_config { monitors::Choice::Primary } else { monitors::Choice::from_value(&layout::read_config()) });
    let mon = monitor();
    layout::set_slot(mon.slot.clone());
    let (mw, mh) = (mon.rect.right - mon.rect.left, mon.rect.bottom - mon.rect.top);
    let (gfx, gfx_luid) = match Gfx::new(mon.out.as_ref().map(|x| &x.adapter)) {
        Ok(g) => (g, mon.out.as_ref().map(|x| x.luid)),
        Err(e) => {
            eprintln!("mswidgets: device on the output adapter failed ({e}), default adapter");
            (Gfx::new(None)?, None)
        }
    };
    let s = o.scale.unwrap_or(mon.scale);
    let settings = layout::read_settings();
    // the accent colour is needed before the first card is recorded
    // with the live glass source the accent is what is on the screen, not what the wallpaper file says: start with the last live colour
    let (accent_rgb, accent_key) = if !o.no_config && settings.live && settings.accent == AccentMode::Wallpaper {
        (crate::accent::load_cache(), None)
    } else {
        resolve_accent(if o.no_config { AccentMode::Blue } else { settings.accent })
    };
    crate::widget::set_accent(accent_rgb);
    crate::widget::set_tint(!o.no_config && settings.accent_scope == AccentScope::All);
    // the look: defaults -> glass.json (the process-wide base) -> `widgetsLook` of config.json; its `card_k` scales the cards (not the host UI)
    let look_base = crate::glass::global_params();
    let look_cfg = if o.no_config { serde_json::json!({}) } else { layout::read_look() };
    let look = crate::glass::apply_look(&look_base, &look_cfg);
    let cs = s * look.card_k;
    let dims = grid_dims(&mon.rect, &mon.work, cs);

    let mut items = layout::load_path(if o.no_config { None } else { layout::config_path() }.as_deref(), mon.slot.as_deref());
    let specs = place_items(&mut items, &dims, cs);
    let m = (MARGIN * cs).round() as i32;
    let card_rects: Vec<RECT> = specs.iter().map(|c| c.rect()).collect();
    let win = window_rect(&card_rects, m, mw, mh);

    // window: layered, tool window, never activated, click-through until the cursor is over a card
    let hinst = GetModuleHandleW(None)?;
    let cls = w!("MsWidgetsNative");
    RegisterClassExW(&WNDCLASSEXW { cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32, lpfnWndProc: Some(wndproc), hInstance: hinst.into(), lpszClassName: cls, hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(), ..Default::default() });
    let hwnd = CreateWindowExW(
        WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TRANSPARENT,
        cls,
        w!("mswidgets"),
        WS_POPUP,
        mon.rect.left + win.left,
        mon.rect.top + win.top,
        (win.right - win.left).max(8),
        (win.bottom - win.top).max(8),
        None,
        None,
        hinst,
        None,
    )?;
    // backdrop: graphics capture of the wallpaper window, else screen capture, else a plain dark translucent base
    let regions = region_list(&card_rects, None, None);
    let backdrop = make_backdrop(&gfx, &mon, mw, mh, settings.live, capture_rate(o.fps, &settings), &regions, m, gfx_luid);
    // a backdrop that reads the SCREEN would see the layer itself: keep the window out of every capture then (screenshots miss it);
    // `MSW_NO_EXCLUDE=1` (diagnostics only) skips that
    if backdrop.name() != "desktop-wgc" && std::env::var_os("MSW_NO_EXCLUDE").is_none() && SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE).is_err() {
        eprintln!("mswidgets: WDA_EXCLUDEFROMCAPTURE refused (needs Windows 10 2004+)");
    }
    let dupl_ok = backdrop.name().starts_with("desktop");
    backdrop.set_hwnd(hwnd);
    eprintln!("mswidgets: backdrop = {}, scale {s:.2}, monitor {mw}x{mh}, window {}x{} at {},{}", backdrop.name(), win.right - win.left, win.bottom - win.top, mon.rect.left + win.left, mon.rect.top + win.top);

    let env = SceneEnv { preview: false, hwnd: hwnd.0 as isize, lang: layout::lang(), utc_off: crate::sys::local_utc_offset_min() };
    let mut scene = Scene::new(&gfx, mw, mh, cs, settings.glass, backdrop.bitmap(), &specs, &env)?;
    scene.set_appearance_pref(o.appearance.unwrap_or(settings.appearance));
    scene.set_params(&gfx, &look)?;
    // start-up: every card condenses out of the glass (staggered), a plain fade with reduced motion
    let reduced0 = reduced_motion();
    let mut pops = HashMap::new();
    let t_start = Instant::now();
    for i in 0..scene.len() {
        let uid = scene.cards[i].uid;
        pops.insert(uid, Pop { t0: t_start + Duration::from_millis(70 * i as u64), out: false });
        scene.set_xf(i, mat_xf(&motion::materialize(0.0, false, reduced0), 0.0, 0.0));
    }
    let t0 = o.time.unwrap_or_else(Now::local);
    for i in 0..scene.len() {
        scene.tick_card(&gfx, i, &t0); // first tick: widgets start their jobs / report their schedule
    }
    let mut ui = Ui::new(env.lang, s, mw as f32 / s, mh as f32 / s);
    (ui.mon_count, ui.mon_sel) = (mon.count, mon.pos);
    ui.glass = settings.glass;
    ui.live = settings.live;
    ui.look = look;
    ui.base = look_base;
    ui.appearance = settings.appearance;
    ui.dim_mode = settings.dim;
    ui.check_start = settings.check_updates;
    let panels = if o.no_config { ((0.0, 0.0), (0.0, 0.0)) } else { layout::read_panels() };
    (ui.gal_off, ui.set_off) = panels;
    let check_now = settings.check_updates && !o.no_config && o.bench.is_none();
    ui.accent_mode = settings.accent;
    ui.accent_scope = settings.accent_scope;

    // premultiplied BGRA DIB the layered window is fed from (only the dirty rect is copied per frame)
    let screen = GetDC(None);
    let mem_dc = CreateCompatibleDC(screen);
    ReleaseDC(None, screen);
    let (dib, bits) = new_dib(mem_dc, win.right - win.left, win.bottom - win.top)?;
    SelectObject(mem_dc, dib);

    let mut app = App {
        gfx,
        scene,
        ui,
        backdrop: Some(backdrop),
        hwnd,
        mem_dc,
        dib,
        bits,
        gfx_luid,
        slot: mon.slot.clone(),
        mon: mon.rect,
        work: mon.work,
        s,
        cs,
        dims,
        win,
        win_dirty: true,
        items,
        settings,
        cfg_mtime: layout::config_mtime(),
        own_write: false,
        hot: None,
        pressed: None,
        press: Press::None,
        drag: None,
        springs: HashMap::new(),
        pops,
        tracking: false,
        interactive: false,
        near: false,
        visible: false,
        user_hidden: false,
        first_present: true,
        last_lum: Instant::now() - Duration::from_secs(1),
        last_day: 0,
        guard_n: 0,
        fixed_time: o.time,
        fps: o.fps,
        use_dupl: dupl_ok,
        editing: false,
        gal_t: 0.0,
        dim_t: 0.0,
        prev_fg: HWND::default(),
        reduced: reduced0,
        t_edit: Instant::now(),
        jig_amp: 0.0,
        last_anim: Instant::now(),
        anim_period: 0,
        ui_tick: 0,
        esc_down: false,
        in_menu: false,
        app_front: false,
        was_fg: false,
        look_cfg,
        look_base,
        look_dirty: false,
        look_last: Instant::now() - Duration::from_secs(1),
        settings_back: false,
        look_commit: None,
        panels_saved: panels,
        update_silent: false,
        update_prev: None,
        accent_key,
        accent_last: Instant::now() - Duration::from_secs(60),
        accent_cur: accent_rgb,
        st: Stats::default(),
        bench: o.bench,
        t0: Instant::now(),
        cpu0: cpu_time_100ns(),
        cpu_mark: None,
        prompt_reply: None,
        prompt_fg: HWND::default(),
        prompt_was_fg: false,
        prompt_t0: Instant::now(),
        prompt_period: 0,
        prompt_last: Instant::now(),
        prompt_scrim: 0.0,
        prompt_hi: None,
        prompt_click: None,
        menu_uid: 0,
        menu_period: 0,
        menu_last: Instant::now(),
        menu_shown: None,
    };
    // First frame is drawn BEFORE the capture thread exists (probe staging and layers get created while the GPU is quiet: the very first
    // luminance probe racing the first full-texture copy of the duplication thread used to wedge the start). Then the capture starts,
    // we wait (<= 600 ms) for its first copy so the window never shows an empty backdrop, and redraw once with the real pixels.
    app.sync_regions();
    let all = app.all_mask();
    app.redraw(all, true);
    if let Some(b) = &app.backdrop {
        b.start();
    }
    if dupl_ok {
        // wait (<= 600 ms) for the first duplication copy so the window never shows an empty backdrop, then redraw with the real pixels
        let t = Instant::now();
        while app.backdrop.as_ref().map_or(0, |b| b.take_dirty()) == 0 && t.elapsed() < Duration::from_millis(600) {
            std::thread::sleep(Duration::from_millis(15));
        }
        app.redraw(all, true);
    }
    app.scene.trim_staging();
    app.show();
    app.update_dim();

    if !o.no_hotkeys {
        if RegisterHotKey(hwnd, 1, MOD_CONTROL | MOD_ALT | MOD_NOREPEAT, 'Q' as u32).is_err() {
            eprintln!("mswidgets: Ctrl+Alt+Q could not be registered (macshell holds it?); close the window to quit");
        }
        if RegisterHotKey(hwnd, 2, MOD_CONTROL | MOD_ALT | MOD_NOREPEAT, 'W' as u32).is_err() {
            eprintln!("mswidgets: Ctrl+Alt+W could not be registered (macshell holds it?); edit mode via the context menu only");
        }
        if RegisterHotKey(hwnd, 3, MOD_CONTROL | MOD_ALT | MOD_NOREPEAT, 'G' as u32).is_err() {
            eprintln!("mswidgets: Ctrl+Alt+G could not be registered; the settings stay reachable from the tray icon");
        }
    }
    TASKBAR_CREATED.store(tray::taskbar_created_msg(), std::sync::atomic::Ordering::Relaxed);
    if !tray::add(hwnd) {
        eprintln!("mswidgets: tray icon could not be added (the settings stay reachable with Ctrl+Alt+W)");
    }
    SetTimer(hwnd, T_HIT, 100, None);
    SetTimer(hwnd, T_GUARD, 500, None);
    SetTimer(hwnd, T_DAY, 30_000, None);
    SetTimer(hwnd, T_CFG, 2_000, None);
    if let Some(sec) = o.bench {
        SetTimer(hwnd, T_BENCH, sec * 1000, None);
    }
    APP.with(|a| *a.borrow_mut() = Some(app));
    if o.bench.is_none() {
        input::set_host(Some((hwnd.0 as isize, WM_PROMPT))); // from now on `prompt_text` shows its panel on our canvas
    }
    APP.with(|a| {
        if let Some(app) = a.borrow_mut().as_mut() {
            app.update_tick_timer();
            app.last_anim = Instant::now();
            app.arm_anim(); // the start-up materialisation
            if check_now {
                app.start_update_check(true); // opt-in (`checkUpdates`): the one request of the session
            }
        }
    });
    Ok(())
}

/// Bounding box of the card layers (+ margin) clipped to the canvas.
fn window_rect(cards: &[RECT], m: i32, mw: i32, mh: i32) -> RECT {
    let bb = cards.iter().copied().reduce(union).unwrap_or(RECT { left: 0, top: 0, right: 8, bottom: 8 });
    clip(RECT { left: bb.left - m, top: bb.top - m, right: bb.right + m, bottom: bb.bottom + m }, mw, mh)
}

unsafe fn new_dib(dc: HDC, w: i32, h: i32) -> Result<(HBITMAP, *mut u8)> {
    let bmi = BITMAPINFO { bmiHeader: BITMAPINFOHEADER { biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32, biWidth: w.max(1), biHeight: -h.max(1), biPlanes: 1, biBitCount: 32, biCompression: BI_RGB.0, ..Default::default() }, ..Default::default() };
    let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
    let dib = CreateDIBSection(dc, &bmi, DIB_RGB_COLORS, &mut bits, None, 0)?;
    Ok((dib, bits as *mut u8))
}

/// The 64 backdrop regions: card i = bit i, the gallery = bit 62, the settings strip = bit 63 (unused ones are empty rectangles).
fn region_list(cards: &[RECT], gallery: Option<RECT>, toolbar: Option<RECT>) -> Vec<RECT> {
    let mut v = vec![RECT::default(); 64];
    for (i, c) in cards.iter().take(MAX_CARDS).enumerate() {
        v[i] = *c;
    }
    if let Some(g) = gallery {
        v[62] = g;
    }
    if let Some(t) = toolbar {
        v[63] = t;
    }
    v
}

/// Packed screen point -> (x, y) (signed 16 bit each, as in mouse messages).
fn unpack(lp: isize) -> (i32, i32) {
    ((lp & 0xFFFF) as i16 as i32, ((lp >> 16) & 0xFFFF) as i16 as i32)
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_TIMER | WM_BACKDROP | WM_HOTKEY | WM_WIDGET | update::WM_UPDATE | update::WM_UPDATE_DL | WM_DISPLAYCHANGE | WM_DPICHANGED | WM_SETTINGCHANGE => {
            with_app(|app| app.on_msg(msg, wp.0));
            LRESULT(0)
        }
        WM_MOUSEMOVE | WM_LBUTTONDOWN | WM_LBUTTONUP | WM_RBUTTONDOWN | WM_RBUTTONUP | WM_MBUTTONDOWN | WM_MBUTTONUP | WM_MOUSEWHEEL | WM_MOUSELEAVE | WM_CAPTURECHANGED => {
            with_app(|app| app.on_mouse(msg, wp, lp));
            LRESULT(0)
        }
        WM_KEYDOWN | WM_CHAR | WM_SYSKEYDOWN => {
            with_app(|app| app.on_key(msg, wp.0));
            if msg == WM_SYSKEYDOWN {
                return DefWindowProcW(hwnd, msg, wp, lp);
            }
            LRESULT(0)
        }
        WM_PROFILE_NAME => {
            with_app(|app| app.profile_named());
            LRESULT(0)
        }
        WM_PROMPT => {
            let ran = std::cell::Cell::new(false);
            with_app(|app| {
                ran.set(true);
                app.on_prompt_request();
            });
            if !ran.get() {
                // the App is busy (re-entrancy): the worker shows the Win32 popup instead of waiting for ever
                if let Some(r) = input::take_request() {
                    let _ = r.reply.send(input::Reply::Fallback);
                }
            }
            LRESULT(0)
        }
        WM_SETCURSOR if (lp.0 & 0xFFFF) as u32 == HTCLIENT => {
            let over = APP.with(|a| a.try_borrow().ok().is_some_and(|g| g.as_ref().is_some_and(|app| app.prompt_field_under_cursor())));
            if over {
                if let Ok(c) = LoadCursorW(None, IDC_IBEAM) {
                    SetCursor(c);
                    return LRESULT(1);
                }
            }
            DefWindowProcW(hwnd, msg, wp, lp)
        }
        WM_MENU => {
            // the menu is drawn on the canvas; only when it cannot be (`MSWIDGETS_NATIVE_MENU=1`, the prompt / settings are up) the native popup
            // runs, which is modal: it must run without the App borrowed, the result is applied afterwards
            let req = APP.with(|a| a.try_borrow_mut().ok().and_then(|mut g| g.as_mut().and_then(|app| app.menu_request(wp.0, lp.0))));
            if let Some(req) = req {
                let shown = std::cell::Cell::new(false);
                with_app(|app| {
                    shown.set(app.show_menu(&req));
                    if !shown.get() {
                        app.in_menu = true;
                    }
                });
                if !shown.get() {
                    let cmd = run_menu(hwnd, &req);
                    with_app(|app| app.menu_result(&req, cmd));
                }
            }
            LRESULT(0)
        }
        WM_MOUSEACTIVATE => {
            // the layer never takes the foreground by a click, except while editing (the search box needs the keyboard)
            if APP.with(|a| a.try_borrow().ok().is_some_and(|g| g.as_ref().is_some_and(|app| app.ui.edit || app.ui.prompt.is_some() || app.ui.menu.is_some()))) {
                DefWindowProcW(hwnd, msg, wp, lp)
            } else {
                LRESULT(MA_NOACTIVATE as isize)
            }
        }
        tray::WM_TRAY => {
            match (lp.0 & 0xFFFF) as u32 {
                WM_LBUTTONUP => with_app(|app| app.apply_tray(TrayCmd::Settings)),
                WM_RBUTTONUP | WM_CONTEXTMENU => {
                    // the popup is modal: it runs without the App borrowed, the choice is applied afterwards
                    let st = APP.with(|a| a.try_borrow().ok().and_then(|g| g.as_ref().map(|app| app.tray_state())));
                    if let Some(st) = st {
                        let cmd = tray::run_menu(hwnd, &st, |h| force_foreground(h));
                        with_app(|app| app.apply_tray(cmd));
                    }
                }
                _ => {}
            }
            LRESULT(0)
        }
        m if m != 0 && m == TASKBAR_CREATED.load(std::sync::atomic::Ordering::Relaxed) => {
            tray::add(hwnd); // Explorer restarted: its notification area is empty again
            LRESULT(0)
        }
        WM_CLOSE => {
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        WM_DESTROY => {
            input::set_host(None);
            tray::remove(hwnd);
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

/// Run `f` on the App when it is not borrowed already (SetWindowPos / UpdateLayeredWindow can re-enter us with messages we do not
/// handle anyway); a panic in the host code is logged and the Direct2D context is put back in a sane state instead of killing the process.
fn with_app(f: impl FnOnce(&mut App)) {
    APP.with(|a| {
        if let Ok(mut g) = a.try_borrow_mut() {
            if let Some(app) = g.as_mut() {
                if catch_unwind(AssertUnwindSafe(|| f(app))).is_err() {
                    eprintln!("mswidgets: internal error caught (see above); continuing");
                    unsafe {
                        let _ = app.gfx.dc.EndDraw(None, None);
                        app.gfx.dc.SetTarget(None::<&windows::Win32::Graphics::Direct2D::ID2D1Image>);
                        let _ = ReleaseCapture();
                    }
                    app.press = Press::None;
                    app.pressed = None;
                }
            }
        }
    });
}

// ---------------------------------------------------------------- context menu (native popup, the fallback of the canvas menu)

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

unsafe fn run_menu(hwnd: HWND, r: &MenuReq) -> usize {
    let t = |ru: &'static str, en: &'static str| wide(r.lang.pick(ru, en));
    let Ok(menu) = CreatePopupMenu() else { return 0 };
    let Ok(sub) = CreatePopupMenu() else {
        let _ = DestroyMenu(menu);
        return 0;
    };
    let label = if r.editing { t("Готово", "Done") } else { t("Изменить виджеты…", "Edit Widgets…") };
    let _ = AppendMenuW(menu, MF_STRING, CMD_EDIT, PCWSTR(label.as_ptr()));
    let sizes = [(Size::S, t("Маленький (S)", "Small (S)")), (Size::M, t("Средний (M)", "Medium (M)")), (Size::L, t("Большой (L)", "Large (L)"))];
    for (i, (sz, name)) in sizes.iter().enumerate() {
        if r.sizes.contains(sz) {
            let flags = MF_STRING | if *sz == r.cur { MF_CHECKED } else { MF_UNCHECKED };
            let _ = AppendMenuW(sub, flags, CMD_SIZE + i, PCWSTR(name.as_ptr()));
        }
    }
    let size_label = t("Размер", "Size");
    let _ = AppendMenuW(menu, MF_POPUP | if r.sizes.len() > 1 { MF_ENABLED } else { MF_GRAYED }, sub.0 as usize, PCWSTR(size_label.as_ptr()));
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
    let del = t("Удалить виджет", "Remove Widget");
    let _ = AppendMenuW(menu, MF_STRING, CMD_DELETE, PCWSTR(del.as_ptr()));
    // the owner must be foreground, else a click elsewhere does not close the menu
    let prev = GetForegroundWindow();
    force_foreground(hwnd);
    let cmd = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_NONOTIFY, r.x, r.y, 0, hwnd, None).0 as usize;
    let _ = PostMessageW(hwnd, WM_NULL, WPARAM(0), LPARAM(0));
    let _ = DestroyMenu(menu); // destroys the submenu too
    if !prev.0.is_null() && prev != hwnd && !r.editing && GetForegroundWindow() == hwnd {
        let _ = SetForegroundWindow(prev);
    }
    cmd
}

/// Borrow the foreground thread's input state so SetForegroundWindow is allowed (same trick as input.rs).
unsafe fn force_foreground(hwnd: HWND) {
    let fg = GetForegroundWindow();
    let (me, theirs) = (GetCurrentThreadId(), GetWindowThreadProcessId(fg, None));
    let attached = theirs != 0 && theirs != me && AttachThreadInput(me, theirs, true).as_bool();
    let _ = SetForegroundWindow(hwnd);
    let _ = BringWindowToTop(hwnd);
    if attached {
        let _ = AttachThreadInput(me, theirs, false);
    }
}

fn clipboard_text(hwnd: HWND) -> Option<String> {
    unsafe {
        OpenClipboard(hwnd).ok()?;
        let r = (|| {
            let h = GetClipboardData(13).ok()?; // CF_UNICODETEXT
            let p = windows::Win32::System::Memory::GlobalLock(windows::Win32::Foundation::HGLOBAL(h.0)) as *const u16;
            if p.is_null() {
                return None;
            }
            let mut n = 0usize;
            while *p.add(n) != 0 && n < 4096 {
                n += 1;
            }
            let s = String::from_utf16_lossy(std::slice::from_raw_parts(p, n));
            let _ = windows::Win32::System::Memory::GlobalUnlock(windows::Win32::Foundation::HGLOBAL(h.0));
            Some(s)
        })();
        let _ = CloseClipboard();
        r
    }
}

// ---------------------------------------------------------------- the host

impl App {
    fn now(&self) -> Now {
        self.fixed_time.unwrap_or_else(Now::local)
    }

    /// The user is about to look at the glass (edit mode, a drag, a look / appearance change): capture at full rate at once.
    fn boost_capture(&self) {
        if let Some(b) = &self.backdrop {
            b.boost();
        }
    }

    fn ext(&self) -> i32 {
        (60.0 * self.cs) as i32
    }

    fn bd(&self) -> &dyn Backdrop {
        self.backdrop.as_deref().expect("backdrop present")
    }

    unsafe fn on_msg(&mut self, msg: u32, id: usize) {
        match (msg, id) {
            (WM_HOTKEY, 2 | 3) if self.ui.prompt.is_some() => {} // the prompt is modal: no edit mode / settings behind it
            (WM_HOTKEY, 2) => {
                if self.editing {
                    self.leave_edit();
                } else {
                    self.enter_edit();
                }
            }
            (WM_HOTKEY, 3) => {
                if self.ui.settings {
                    self.close_settings();
                } else {
                    self.open_settings();
                }
            }
            (WM_HOTKEY, _) => {
                self.flush_look_commit();
                let _ = DestroyWindow(self.hwnd);
            }
            (update::WM_UPDATE, _) => self.update_done(),
            (update::WM_UPDATE_DL, p) => self.install_progress(p),
            (WM_BACKDROP, _) => {
                let mask = self.bd().take_dirty();
                if mask != 0 && self.visible {
                    self.redraw(mask, false);
                }
            }
            (WM_WIDGET, uid) => self.on_job(uid),
            (WM_DISPLAYCHANGE | WM_DPICHANGED | WM_SETTINGCHANGE, _) => {
                SetTimer(self.hwnd, T_REPLACE, 600, None); // debounce: a mode switch sends a burst
            }
            (_, T_TICK) => self.tick(),
            (_, T_HIT) => self.hit_test(),
            (_, T_GUARD) => self.guard(),
            (_, T_ANIM) => self.anim_step(),
            (_, T_UI) => self.ui_timer(),
            (_, T_PROMPT) => self.prompt_tick(),
            (_, T_MENU) => self.menu_tick(),
            (_, T_CFG) => {
                self.poll_config();
                self.refresh_accent(false); // a new wallpaper
            }
            (_, T_LCOMMIT) => self.flush_look_commit(),
            (_, T_LOOK) => {
                let _ = KillTimer(self.hwnd, T_LOOK);
                if self.look_dirty {
                    self.apply_look_now();
                }
            }
            (_, T_SHRINK) => {
                let _ = KillTimer(self.hwnd, T_SHRINK);
                self.fit_window();
            }
            (_, T_TOAST) => {
                let _ = KillTimer(self.hwnd, T_TOAST);
                // fade out first: the toast (and the monitor-wide canvas it needs in live mode) goes when the fade ended (`anim_step`)
                self.ui.toast_fade();
                self.arm_anim();
            }
            (_, T_REPLACE) => {
                let _ = KillTimer(self.hwnd, T_REPLACE);
                self.replace_monitor();
            }
            (_, T_DAY) => {
                if self.now().day_key() != self.last_day && self.visible {
                    let all = self.all_mask();
                    self.redraw(all, false);
                }
            }
            (_, T_BENCH) => {
                self.report();
                let _ = DestroyWindow(self.hwnd);
            }
            _ => {}
        }
        if let (Some(b), None) = (self.bench, self.cpu_mark) {
            if b > 6 && self.t0.elapsed() > Duration::from_secs(3) {
                self.cpu_mark = Some((Instant::now(), cpu_time_100ns()));
            }
        }
    }

    fn all_mask(&self) -> u64 {
        (1u64 << self.scene.len().min(MAX_CARDS)) - 1
    }

    fn card_rects(&self) -> Vec<RECT> {
        self.scene.card_rects()
    }

    fn all_dirty(&self) -> RECT {
        let mut d = RECT { left: 0, top: 0, right: 0, bottom: 0 };
        for i in 0..self.scene.len() {
            d = if d.right == 0 { self.scene.layer_rect_ext(i, self.ext()) } else { union(d, self.scene.layer_rect_ext(i, self.ext())) };
        }
        if let Some(u) = self.ui.dirty_rect(&self.scene) {
            d = if d.right == 0 { u } else { union(d, u) };
        }
        d
    }

    // ------------------------------------------------ backdrop regions, window geometry

    fn sync_regions(&self) {
        let s = self.s;
        let px = |r: Rect| RECT { left: (r.x * s) as i32, top: (r.y * s) as i32, right: (r.right() * s).ceil() as i32, bottom: (r.bottom() * s).ceil() as i32 };
        let g = (self.ui.edit && (self.ui.gallery || self.ui.settings)).then(|| px(self.ui.panel_rect()));
        let t = (self.ui.edit && !self.ui.settings).then(|| px(self.ui.toolbar_rect()));
        let pr = self.ui.prompt.as_ref().map(|p| px(p.lay.panel));
        let mn = self.ui.menu.as_ref().map(|m| px(m.bounds()));
        let g = [g, pr, mn].into_iter().flatten().reduce(union);
        if let Some(b) = &self.backdrop {
            b.set_regions(&region_list(&self.card_rects(), g, t));
        }
    }

    /// Window = whole monitor while editing / dragging / settling, else the card bounding box.
    fn want_full(&self) -> bool {
        self.ui.edit || self.drag.is_some() || !self.springs.is_empty() || self.in_menu || self.ui.prompt.is_some() || self.ui.menu.is_some()
    }

    unsafe fn desired_window(&self) -> RECT {
        let (mw, mh) = (self.scene.w, self.scene.h);
        if self.want_full() || self.ui.toast.is_some() {
            return RECT { left: 0, top: 0, right: mw, bottom: mh };
        }
        window_rect(&self.card_rects(), self.scene.margin(), mw, mh)
    }

    /// Resize the layered window to `r` (canvas px): a new DIB, a full present.
    unsafe fn set_window(&mut self, r: RECT) {
        if r.left == self.win.left && r.top == self.win.top && r.right == self.win.right && r.bottom == self.win.bottom {
            return;
        }
        match new_dib(self.mem_dc, r.right - r.left, r.bottom - r.top) {
            Ok((dib, bits)) => {
                let old = SelectObject(self.mem_dc, dib);
                let _ = old;
                let _ = DeleteObject(self.dib);
                self.dib = dib;
                self.bits = bits;
                self.win = r;
                self.win_dirty = true;
                self.present_full();
            }
            Err(e) => eprintln!("mswidgets: window bitmap: {e}"),
        }
    }

    /// Back to the bounding box once nothing needs the whole monitor any more.
    unsafe fn fit_window(&mut self) {
        if self.want_full() {
            return;
        }
        if self.scene.cards.iter().any(|c| !c.xf.is_id()) {
            let ids: Vec<usize> = (0..self.scene.len()).collect();
            for i in ids {
                self.scene.set_xf(i, Xf::ID);
            }
        }
        self.scene.release_scratch();
        self.scene.trim_staging();
        let r = self.desired_window();
        self.set_window(r);
    }

    // ------------------------------------------------ rendering

    /// Re-render the cards in `mask` (their backdrop changed), then present the union of their layers.
    unsafe fn redraw(&mut self, mask: u64, force_lum: bool) {
        let t = Instant::now();
        let now = self.now();
        // luminance at most twice a second (from an 8x8 GPU downscale), otherwise the previous value is reused
        if force_lum || self.last_lum.elapsed() >= Duration::from_millis(500) {
            self.last_lum = Instant::now();
            if let Err(e) = self.scene.measure_lum(&self.gfx, mask, !force_lum) {
                eprintln!("mswidgets: luminance probe: {e}");
            } else {
                self.st.lum_reads += 1;
            }
        }
        let mut dirty: Option<RECT> = None;
        let add = |d: &mut Option<RECT>, r: RECT| *d = Some(d.map_or(r, |x| union(x, r)));
        for i in 0..self.scene.cards.len().min(MAX_CARDS) {
            if mask & (1 << i) == 0 {
                continue;
            }
            if let Err(e) = self.scene.render_card(&self.gfx, i, &now) {
                eprintln!("mswidgets: render card {i}: {e}");
                continue;
            }
            self.st.card_redraws += 1;
            add(&mut dirty, self.scene.layer_rect_ext(i, if self.ui.edit { self.ext() } else { 0 }));
        }
        if self.scene.fx_busy() {
            // a card's appearance flipped (or is still fading): cross-fade it
            self.last_anim = Instant::now();
            self.arm_anim();
        }
        for (bit, slot) in [(BIT_GALLERY, ui::SLOT_GALLERY), (BIT_TOOLBAR, ui::SLOT_TOOLBAR)] {
            if mask & bit != 0 && self.ui.edit {
                if let Err(e) = self.scene.render_panel(&self.gfx, slot) {
                    eprintln!("mswidgets: panel {slot}: {e}");
                }
                if let Some(u) = self.ui.dirty_rect(&self.scene) {
                    add(&mut dirty, u);
                }
            }
        }
        if mask & (BIT_GALLERY | BIT_TOOLBAR) != 0 && self.ui.prompt.is_some() {
            if let Err(e) = self.scene.render_panel(&self.gfx, ui::SLOT_PROMPT) {
                eprintln!("mswidgets: prompt panel: {e}");
            }
            if let Some(u) = self.ui.prompt_dirty(&self.scene) {
                add(&mut dirty, u);
            }
        }
        if mask & (BIT_GALLERY | BIT_TOOLBAR) != 0 && self.ui.menu.is_some() {
            for slot in [ui::SLOT_MENU, ui::SLOT_SUB] {
                if let Err(e) = self.scene.render_panel(&self.gfx, slot) {
                    eprintln!("mswidgets: menu panel: {e}");
                }
            }
            if let Some(u) = self.ui.menu_dirty(&self.scene) {
                add(&mut dirty, u);
            }
        }
        self.last_day = now.day_key();
        self.st.backdrop_batches += 1;
        if let Some(d) = dirty {
            self.frame(d);
        }
        self.st.render_ms += t.elapsed().as_secs_f64() * 1000.0;
    }

    /// Compose the dirty rectangle `d` (canvas px) of the scene + the host UI and present it.
    unsafe fn frame(&mut self, d: RECT) {
        let d = clip(d, self.scene.w, self.scene.h);
        if d.right <= d.left || d.bottom <= d.top {
            return;
        }
        let now = self.now();
        if let Err(e) = self.scene.prepare(&self.gfx, d, &now) {
            eprintln!("mswidgets: prepare: {e}");
            return;
        }
        self.scene.begin(&self.gfx, d, None);
        if self.ui.dim > 0.002 || self.ui.placeholder.is_some() || self.ui.scrim() > 0.002 {
            self.ui.draw_pre(&self.gfx, d);
        }
        self.scene.draw_cards(&self.gfx, d, &now);
        if self.ui.edit || self.ui.toast.is_some() || self.ui.prompt.is_some() || self.ui.menu.is_some() {
            self.ui.draw_post(&self.scene, &self.gfx, &now, d);
        }
        if let Err(e) = self.scene.end(&self.gfx) {
            eprintln!("mswidgets: composite: {e}");
            return;
        }
        self.present(d);
        // whatever changed the settings panel or the toast may have left something to glide: wake the animation timer
        if self.ui.settings || self.ui.toast.is_some() {
            self.ui.sa_track();
            self.arm_anim();
        }
    }

    /// Copy `d` (canvas px, inside the window) of the frame into the DIB and push it to the layered window.
    unsafe fn present(&mut self, d: RECT) {
        if self.win_dirty {
            return self.present_full();
        }
        let d = RECT { left: d.left.max(self.win.left), top: d.top.max(self.win.top), right: d.right.min(self.win.right), bottom: d.bottom.min(self.win.bottom) };
        if d.right <= d.left || d.bottom <= d.top {
            return;
        }
        let t = Instant::now();
        let ww = (self.win.right - self.win.left) as usize;
        let (bits, wl, wt) = (self.bits, self.win.left, self.win.top);
        let r = self.scene.read_rect(&self.gfx, d, |y, row| {
            let off = ((y - wt) as usize * ww + (d.left - wl) as usize) * 4;
            std::ptr::copy_nonoverlapping(row.as_ptr(), bits.add(off), row.len());
        });
        if let Err(e) = r {
            eprintln!("mswidgets: readback: {e}");
            return;
        }
        let dirty = RECT { left: d.left - wl, top: d.top - wt, right: d.right - wl, bottom: d.bottom - wt };
        self.update_layered(Some(&dirty));
        self.st.present_ms += t.elapsed().as_secs_f64() * 1000.0;
    }

    /// The whole window (after a resize / move): read everything back and pass position + size.
    unsafe fn present_full(&mut self) {
        let t = Instant::now();
        let ww = (self.win.right - self.win.left) as usize;
        let (bits, wl, wt) = (self.bits, self.win.left, self.win.top);
        let r = self.scene.read_rect(&self.gfx, self.win, |y, row| {
            let off = (y - wt) as usize * ww * 4;
            std::ptr::copy_nonoverlapping(row.as_ptr(), bits.add(off), row.len());
        });
        let _ = wl;
        if let Err(e) = r {
            eprintln!("mswidgets: readback: {e}");
            return;
        }
        self.update_layered(None);
        self.st.present_ms += t.elapsed().as_secs_f64() * 1000.0;
    }

    unsafe fn update_layered(&mut self, dirty: Option<&RECT>) {
        let (ww, wh) = (self.win.right - self.win.left, self.win.bottom - self.win.top);
        let pos = POINT { x: self.mon.left + self.win.left, y: self.mon.top + self.win.top };
        let size = SIZE { cx: ww, cy: wh };
        let src = POINT { x: 0, y: 0 };
        let blend = BLENDFUNCTION { BlendOp: AC_SRC_OVER as u8, BlendFlags: 0, SourceConstantAlpha: 255, AlphaFormat: AC_SRC_ALPHA as u8 };
        // diagnostics: `MSW_PARTIAL=1` restores partial (prcDirty) updates; by default every update passes position + size
        let full = dirty.is_none() || self.win_dirty || self.first_present || std::env::var_os("MSW_PARTIAL").is_none();
        let info = UPDATELAYEREDWINDOWINFO {
            cbSize: std::mem::size_of::<UPDATELAYEREDWINDOWINFO>() as u32,
            hdcDst: HDC::default(),
            pptDst: if full { &pos } else { std::ptr::null() },
            psize: if full { &size } else { std::ptr::null() },
            hdcSrc: self.mem_dc,
            pptSrc: &src,
            crKey: COLORREF(0),
            pblend: &blend,
            dwFlags: ULW_ALPHA,
            prcDirty: if full { std::ptr::null() } else { dirty.map_or(std::ptr::null(), |d| d as *const RECT) },
        };
        if UpdateLayeredWindowIndirect(self.hwnd, &info).as_bool() {
            self.first_present = false;
            self.win_dirty = false;
            if let Some(mode) = std::env::var_os("MSW_SOLID") {
                // diagnostics: `1` paints the whole layer opaque red and presents it again; `dup` presents the same frame twice
                let px = std::slice::from_raw_parts_mut(self.bits, (ww * wh * 4) as usize);
                if mode == "opaque" {
                    px.chunks_exact_mut(4).for_each(|p| p[3] = 255);
                } else if mode == "clamp" {
                    px.chunks_exact_mut(4).for_each(|p| {
                        let a = p[3];
                        p[0] = p[0].min(a);
                        p[1] = p[1].min(a);
                        p[2] = p[2].min(a);
                    });
                } else if mode != "dup" {
                    let v: [u8; 4] = if mode == "half" { [0, 0, 128, 128] } else { [0, 0, 255, 255] };
                    px.chunks_exact_mut(4).for_each(|p| p.copy_from_slice(&v));
                }
                let _ = UpdateLayeredWindowIndirect(self.hwnd, &info);
            }
            // diagnostics: `MSW_DUMP=<file.png>` saves what is handed to Windows (at most every 2 s)
            if let Some(path) = std::env::var_os("MSW_DUMP") {
                thread_local!(static LAST_DUMP: std::cell::Cell<Option<Instant>> = const { std::cell::Cell::new(None) });
                if LAST_DUMP.with(|l| l.get()).is_none_or(|t| t.elapsed() > Duration::from_secs(2)) {
                    LAST_DUMP.with(|l| l.set(Some(Instant::now())));
                    let px = std::slice::from_raw_parts(self.bits, (ww * wh * 4) as usize);
                    let _ = self.gfx.save_png(&path.to_string_lossy(), ww as u32, wh as u32, px);
                }
            }
        } else {
            eprintln!("mswidgets: UpdateLayeredWindow: {}", windows::core::Error::from_win32());
        }
    }

    // ------------------------------------------------ widget schedules

    /// Widget schedules: call `tick` of every due card; animating cards repaint only their own rectangle (cached glass layers are just
    /// blitted + the overlay), a card whose static content went stale is re-rendered first.
    unsafe fn tick(&mut self) {
        if !self.visible {
            return;
        }
        let t = Instant::now();
        let now = self.now();
        let mut dirty: Option<RECT> = None;
        for i in self.scene.due_cards(t) {
            self.scene.tick_card(&self.gfx, i, &now);
            let animated = self.scene.cards[i].tick.animates();
            if let Some(r) = self.settle(i, &now, animated) {
                dirty = Some(dirty.map_or(r, |d| union(d, r)));
            }
        }
        if let Some(d) = dirty {
            self.frame(d);
            self.st.clock_frames += 1;
        }
        self.st.render_ms += t.elapsed().as_secs_f64() * 1000.0;
        self.update_tick_timer();
    }

    /// A background job of the card with handle `uid` finished: let the widget pick up the result.
    unsafe fn on_job(&mut self, uid: usize) {
        let Some(i) = self.scene.index_of_uid(uid) else { return };
        if !self.visible {
            return;
        }
        let now = self.now();
        self.scene.tick_card(&self.gfx, i, &now);
        if let Some(r) = self.settle(i, &now, false) {
            self.frame(r);
        }
        self.update_tick_timer();
    }

    /// After any widget callback on card `i`: persist changed settings, re-render the layer when its static content is stale
    /// (version / day / invalidate), and return the rectangle to recompose (None = nothing visible changed).
    unsafe fn settle(&mut self, i: usize, now: &Now, animated: bool) -> Option<RECT> {
        if let Some((id, cfg)) = self.scene.take_cfg(i) {
            if let Some(it) = self.items.iter_mut().find(|x| x.id == id) {
                it.cfg = cfg;
            }
            layout::persist_cfg(&self.items, &id);
            self.note_own_write();
        }
        let frame = self.scene.take_frame_request(i);
        let edit_ext = if self.ui.edit { self.ext() } else { 0 };
        if self.scene.content_stale(i, now) {
            return match self.scene.render_card(&self.gfx, i, now) {
                Ok(()) => {
                    self.st.card_redraws += 1;
                    Some(self.scene.layer_rect_ext(i, edit_ext))
                }
                Err(e) => {
                    eprintln!("mswidgets: render card {i}: {e}");
                    None
                }
            };
        }
        (animated || frame).then(|| if edit_ext > 0 { self.scene.layer_rect_ext(i, edit_ext) } else { self.scene.card_rect(i) })
    }

    unsafe fn send_mouse(&mut self, i: usize, kind: MouseKind, x: i32, y: i32, button: Button, delta: f32, now: &Now) -> bool {
        let changed = self.scene.mouse(&self.gfx, i, kind, x, y, button, delta, now);
        if let Some(r) = self.settle(i, now, changed) {
            self.frame(r);
        }
        if self.scene.take_fx_wake() {
            // hover / press / glint moved: the animation timer paints it, then stops again. Only a stopped timer restarts its clock:
            // resetting it on every mouse move would shrink each step's dt and slow the easing down while the pointer moves
            if self.anim_period == 0 {
                self.last_anim = Instant::now();
            }
            self.arm_anim();
        }
        changed
    }

    unsafe fn leave(&mut self, now: &Now) {
        if let Some(o) = self.hot.take() {
            if o < self.scene.len() {
                self.send_mouse(o, MouseKind::Leave, 0, 0, Button::None, 0.0, now);
            }
        }
    }

    // ------------------------------------------------ mouse

    /// Cursor position in monitor px for the message being handled.
    unsafe fn mouse_pos(&self) -> (i32, i32) {
        let p = GetMessagePos();
        (((p & 0xFFFF) as i16 as i32) - self.mon.left, (((p >> 16) & 0xFFFF) as i16 as i32) - self.mon.top)
    }

    /// Window mouse messages (live mode: only delivered while the window is not click-through, i.e. the cursor is over a card;
    /// edit mode / drag: the whole window).
    unsafe fn on_mouse(&mut self, msg: u32, wp: WPARAM, lp: LPARAM) {
        let now = self.now();
        match msg {
            WM_MOUSELEAVE => {
                self.tracking = false;
                if self.pressed.is_none() && !self.ui.edit {
                    self.leave(&now);
                }
                if self.ui.edit && self.ui.hover != Hit::None && self.press == Press::None {
                    self.set_hover(Hit::None);
                }
                return;
            }
            WM_CAPTURECHANGED => {
                if self.drag.is_some() {
                    // another window took the capture (alt-tab, a popup): drop the card where it is instead of leaving it stuck to the pointer
                    if HWND(lp.0 as *mut std::ffi::c_void) != self.hwnd {
                        self.end_drag();
                    }
                } else {
                    self.pressed = None;
                    self.press = Press::None;
                }
                return;
            }
            _ => {}
        }
        let (x, y) = self.mouse_pos();
        if !self.tracking && msg == WM_MOUSEMOVE {
            let mut t = TRACKMOUSEEVENT { cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: self.hwnd, dwHoverTime: 0 };
            if TrackMouseEvent(&mut t).is_ok() {
                self.tracking = true;
            }
        }
        if self.ui.menu.is_some() {
            return self.menu_mouse(msg, x, y);
        }
        if self.ui.prompt.is_some() {
            return self.prompt_mouse(msg, wp, x, y);
        }
        if self.ui.edit {
            return self.edit_mouse(msg, wp, x, y);
        }
        let under = self.scene.cards.iter().rposition(|c| x >= c.x && x < c.x + c.w && y >= c.y && y < c.y + c.h);
        if self.pressed.is_none() && self.hot != under {
            self.leave(&now);
            self.hot = under;
        }
        let uid_of = |s: &Scene, i: usize| s.cards.get(i).map_or(0, |c| c.uid);
        // a press that turned into a drag: the card follows the pointer, the widget hears nothing more
        if let Press::Drag = self.press {
            match msg {
                WM_MOUSEMOVE => {
                    if let Some(d) = self.drag.as_mut() {
                        d.cur = (x, y);
                    }
                }
                WM_LBUTTONUP => self.end_drag(),
                _ => {}
            }
            if matches!(msg, WM_LBUTTONUP) && wp.0 & 0x13 == 0 {
                let _ = ReleaseCapture();
                self.pressed = None;
            }
            return;
        }
        if let (Press::Cand { uid, x: x0, y: y0 }, WM_MOUSEMOVE) = (self.press, msg) {
            if (x - x0).abs().max((y - y0).abs()) > DRAG_PX {
                if let Some(i) = self.scene.index_of_uid(uid) {
                    self.send_mouse(i, MouseKind::Leave, 0, 0, Button::None, 0.0, &now);
                    self.hot = None;
                }
                self.begin_drag(uid, x0, y0, x, y, false, None);
                return;
            }
        }
        let Some(i) = self.pressed.or(under) else { return };
        let (kind, button, delta) = match msg {
            WM_MOUSEMOVE => (MouseKind::Move, Button::None, 0.0),
            WM_LBUTTONDOWN => (MouseKind::Down, Button::Left, 0.0),
            WM_LBUTTONUP => (MouseKind::Up, Button::Left, 0.0),
            WM_RBUTTONDOWN => (MouseKind::Down, Button::Right, 0.0),
            WM_RBUTTONUP => (MouseKind::Up, Button::Right, 0.0),
            WM_MBUTTONDOWN => (MouseKind::Down, Button::Middle, 0.0),
            WM_MBUTTONUP => (MouseKind::Up, Button::Middle, 0.0),
            WM_MOUSEWHEEL => (MouseKind::Wheel, Button::None, ((wp.0 >> 16) as u16 as i16) as f32 / 120.0),
            _ => return,
        };
        if matches!(msg, WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN) && self.pressed.is_none() {
            SetCapture(self.hwnd);
            self.pressed = Some(i);
        }
        let changed = self.send_mouse(i, kind, x, y, button, delta, &now);
        match (msg, changed) {
            (WM_LBUTTONDOWN, true) => self.press = Press::Widget(uid_of(&self.scene, i)),
            (WM_LBUTTONDOWN, false) => self.press = Press::Cand { uid: uid_of(&self.scene, i), x, y },
            (WM_RBUTTONUP, false) if under == Some(i) => {
                // the widget did not use the right click: the card context menu (run outside of this borrow)
                let uid = uid_of(&self.scene, i);
                let p = GetMessagePos();
                let _ = PostMessageW(self.hwnd, WM_MENU, WPARAM(uid), LPARAM(p as isize));
            }
            _ => {}
        }
        if matches!(msg, WM_LBUTTONUP | WM_RBUTTONUP | WM_MBUTTONUP) && wp.0 & 0x13 == 0 {
            let _ = ReleaseCapture();
            self.pressed = None;
            self.press = Press::None;
        }
    }

    // ------------------------------------------------ edit mode mouse

    fn hover_dirty(&self, h: Hit) -> Option<RECT> {
        let s = self.s;
        let px = |r: Rect| RECT { left: (r.x * s) as i32 - 8, top: (r.y * s) as i32 - 8, right: (r.right() * s).ceil() as i32 + 8, bottom: (r.bottom() * s).ceil() as i32 + 8 };
        match h {
            Hit::Minus(i) | Hit::Card(i) => (i < self.scene.len()).then(|| self.scene.layer_rect_ext(i, self.ext())),
            Hit::Slider(_) | Hit::Seg(_) | Hit::SettingsBtn | Hit::Toolbar => Some(px(self.ui.toolbar_rect())),
            Hit::None => None,
            _ => Some(px(self.ui.panel_rect())),
        }
    }

    unsafe fn set_hover(&mut self, h: Hit) {
        let same = |a: Hit, b: Hit| match (a, b) {
            (Hit::Slider(_), Hit::Slider(_)) => true,
            (Hit::Search(_), Hit::Search(_)) => true,
            (Hit::Look(i, _), Hit::Look(j, _)) => i == j,
            _ => a == b,
        };
        if same(self.ui.hover, h) {
            return;
        }
        let old = self.ui.hover;
        self.ui.hover = h;
        // the settings panel eases its hover looks in and out: the animation timer repaints just the control
        if self.ui.settings && (ui::hkey(old).is_some() || ui::hkey(h).is_some()) {
            self.arm_anim();
            return;
        }
        // only hovers that change how something looks need a frame
        let looks = |h: Hit| matches!(h, Hit::Minus(_) | Hit::Group(_) | Hit::Item(_) | Hit::Done | Hit::SettingsBtn | Hit::Look(..) | Hit::Opt(..) | Hit::Autostart | Hit::CheckStart | Hit::UpdBtn | Hit::UpdInstall | Hit::UpdPage | Hit::ResetLook | Hit::EditBtn | Hit::QuitBtn);
        if looks(old) || looks(h) {
            let mut d: Option<RECT> = None;
            for r in [self.hover_dirty(old), self.hover_dirty(h)].into_iter().flatten() {
                d = Some(d.map_or(r, |x| union(x, r)));
            }
            if let Some(d) = d {
                self.frame(d);
            }
        }
    }

    unsafe fn edit_mouse(&mut self, msg: u32, wp: WPARAM, x: i32, y: i32) {
        let hit = self.ui.hit(&self.scene, x, y);
        match msg {
            WM_MOUSEMOVE => {
                match self.press {
                    Press::Drag => {
                        if let Some(d) = self.drag.as_mut() {
                            d.cur = (x, y);
                        }
                        return;
                    }
                    Press::Cand { uid, x: x0, y: y0 } => {
                        if (x - x0).abs().max((y - y0).abs()) > DRAG_PX - 2 {
                            self.begin_drag(uid, x0, y0, x, y, false, None);
                        }
                        return;
                    }
                    Press::Preview { k, x: x0, y: y0 } => {
                        if (x - x0).abs().max((y - y0).abs()) > DRAG_PX {
                            self.drag_from_preview(k, x, y);
                        }
                        return;
                    }
                    Press::Ui(Hit::Slider(_)) => {
                        let v = self.ui.slider_value(x);
                        self.apply_glass(v);
                        return;
                    }
                    Press::Ui(Hit::Look(i, _)) => {
                        self.ui.sa_drag_move(i, x); // far from where it was pressed: the handle follows 1:1
                        let t = self.ui.set_value_at(i, x);
                        self.set_look_slider(i, t);
                        return;
                    }
                    Press::Move { x: x0, y: y0, ox, oy } => {
                        let old = self.ui.dirty_rect(&self.scene);
                        self.ui.move_panel(ox + (x - x0) as f32 / self.s, oy + (y - y0) as f32 / self.s);
                        self.ui.sync_panels(&mut self.scene, &self.gfx);
                        if let (Some(a), Some(b)) = (old, self.ui.dirty_rect(&self.scene)) {
                            self.frame(union(a, b));
                        }
                        return;
                    }
                    _ => {}
                }
                self.set_hover(hit);
            }
            WM_LBUTTONDOWN => {
                SetCapture(self.hwnd);
                self.ui.press = hit;
                self.flush_look_commit();
                let old_focus = self.ui.sfocus;
                self.ui.sfocus = match hit {
                    Hit::Look(i, _) => Some(i),
                    Hit::Slider(_) => Some(0),
                    _ => None,
                };
                if self.ui.sfocus.is_some() {
                    self.ui.st.focus = false;
                }
                if old_focus != self.ui.sfocus {
                    self.redraw_ui();
                }
                match hit {
                    Hit::Card(i) => {
                        self.ui.st.focus = false;
                        self.press = Press::Cand { uid: self.scene.cards[i].uid, x, y };
                    }
                    Hit::Item(k) => self.press = Press::Preview { k, x, y },
                    Hit::Slider(v) => {
                        self.press = Press::Ui(hit);
                        self.apply_glass(v);
                    }
                    Hit::Search(dx) => {
                        self.ui.st.focus = true;
                        let c = self.ui.caret_at(&self.scene, &self.gfx, dx);
                        self.ui.st.query.set_caret(c);
                        self.ui.caret_on = true;
                        self.press = Press::Ui(hit);
                        let d = self.hover_dirty(hit);
                        if let Some(d) = d {
                            self.frame(d);
                        }
                    }
                    Hit::Look(i, t) => {
                        self.press = Press::Ui(hit);
                        self.ui.sa_drag_start(i, x, t);
                        self.set_look_slider(i, t);
                        self.arm_anim(); // the knob swells, the handle glides to a click on the track
                    }
                    Hit::Panel => {
                        // an empty part of the panel is a handle: drag the whole panel
                        let p = self.ui.panel_rect();
                        self.press = Press::Move { x, y, ox: p.x, oy: p.y };
                        self.ui.st.focus = false;
                    }
                    Hit::None if self.ui.settings => self.press = Press::Ui(Hit::None), // a click outside the panel closes it on release
                    Hit::None => self.ui.st.focus = false,
                    _ if self.ui.settings && ui::hkey(hit).is_some() => {
                        // a button / segment / switch of the settings panel: the press spring repaints it
                        self.press = Press::Ui(hit);
                        self.arm_anim();
                    }
                    _ => {
                        self.press = Press::Ui(hit);
                        if let Some(d) = self.hover_dirty(hit) {
                            self.frame(d);
                        }
                    }
                }
            }
            WM_LBUTTONUP => {
                let press = std::mem::replace(&mut self.press, Press::None);
                self.ui.press = Hit::None;
                self.ui.sa_drag_end();
                let _ = ReleaseCapture();
                match press {
                    Press::Drag => self.end_drag(),
                    Press::Preview { k, .. } => {
                        if let Some((def, size)) = self.ui.item(k) {
                            self.add_widget(def.type_id, size, None);
                        }
                    }
                    Press::Ui(Hit::Slider(_)) => {
                        let v = self.ui.slider_value(x);
                        self.apply_glass(v);
                        let v = (v * 100.0).round() / 100.0;
                        self.write_setting("widgetsGlass", serde_json::json!(v));
                        self.settings.glass = v;
                    }
                    Press::Ui(Hit::Look(i, _)) => {
                        let t = self.ui.set_value_at(i, x);
                        self.set_look_slider(i, t);
                        self.commit_look(i);
                    }
                    Press::Move { .. } => self.save_panels(),
                    Press::Ui(h) => {
                        let same = match (h, hit) {
                            (Hit::Search(_), Hit::Search(_)) => true,
                            (a, b) => a == b,
                        };
                        if same {
                            self.ui_click(h);
                        } else if self.ui.settings && ui::hkey(h).is_some() {
                            self.arm_anim(); // released elsewhere: the press spring lets go
                        } else if let Some(d) = self.hover_dirty(h) {
                            self.frame(d);
                        }
                    }
                    _ => {}
                }
                self.set_hover(hit);
                if self.ui.settings {
                    self.arm_anim(); // the press spring of whatever was held
                }
            }
            WM_RBUTTONUP => {
                if let Hit::Card(i) | Hit::Minus(i) = hit {
                    let uid = self.scene.cards[i].uid;
                    let p = GetMessagePos();
                    let _ = PostMessageW(self.hwnd, WM_MENU, WPARAM(uid), LPARAM(p as isize));
                }
            }
            WM_MOUSEWHEEL => {
                let delta = ((wp.0 >> 16) as u16 as i16) as f32 / 120.0;
                // over a slider the wheel changes it (2 % a notch); elsewhere it scrolls the gallery
                let (lx, ly) = (x as f32 / self.s, y as f32 / self.s);
                let slider = if self.ui.settings {
                    self.ui.row_at(lx, ly)
                } else {
                    matches!(hit, Hit::Slider(_)).then_some(0)
                };
                if let (Some(i), Press::None) = (slider, self.press) {
                    self.ui.sfocus = Some(i);
                    self.ui.st.focus = false;
                    let t = ui::wheel_pos(self.ui.set_pos(i), delta);
                    self.nudge_slider(i, t);
                    self.redraw_ui();
                    return;
                }
                if self.ui.wheel(x as f32 / self.s, y as f32 / self.s, delta) {
                    if let Some(d) = self.hover_dirty(Hit::Panel) {
                        self.frame(d);
                    }
                }
            }
            _ => {}
        }
    }

    unsafe fn ui_click(&mut self, h: Hit) {
        match h {
            Hit::Done if self.ui.settings => self.close_settings(),
            Hit::None if self.ui.settings => self.close_settings(),
            Hit::SettingsBtn => self.open_settings(),
            Hit::ResetLook => self.reset_look(),
            Hit::Opt(o, k) => self.set_option(o, k),
            Hit::Autostart => self.toggle_autostart(),
            Hit::CheckStart => self.toggle_check_start(),
            Hit::UpdBtn => self.start_update_check(false),
            Hit::UpdInstall => self.start_install(),
            Hit::UpdPage => {
                if let UpdateState::Available { url, .. } = self.ui.update.clone() {
                    crate::sys::perform(&crate::sys::Action::Open(url)); // the release page in the browser
                }
            }
            Hit::EditBtn => self.switch_panel(false),
            Hit::QuitBtn => {
                self.flush_look_commit();
                let _ = DestroyWindow(self.hwnd); // the Ctrl+Alt+Q path
            }
            Hit::Done => self.leave_edit(),
            Hit::Minus(i) => {
                if let Some(c) = self.scene.cards.get(i) {
                    let uid = c.uid;
                    self.remove_card(uid);
                }
            }
            Hit::Group(i) => {
                self.ui.select_group(i);
                self.gallery_changed();
            }
            Hit::Chip(s) => {
                self.ui.select_size(s);
                self.gallery_changed();
            }
            Hit::Seg(live) => self.set_live(live),
            _ => {}
        }
    }

    unsafe fn gallery_changed(&mut self) {
        let now = Now::local();
        self.ui.sync_previews(&self.gfx, &now);
        if !self.reduced {
            self.ui.gal_note();
        }
        self.arm_ui_timer();
        if let Some(d) = self.hover_dirty(Hit::Panel) {
            self.frame(d);
        }
        self.arm_anim(); // the tiles fade in
    }

    // ------------------------------------------------ keyboard (edit mode: the search box, the sliders)

    /// Keys for the sliders: all of them in the settings panel; in the gallery only while the strip's glass slider has the focus (Tab goes
    /// between the search box and that slider). True = the key was used.
    unsafe fn slider_key(&mut self, k: SKey, shift: bool) -> bool {
        if self.ui.gallery {
            match (self.ui.sfocus.is_some(), k) {
                (false, SKey::Tab) => {
                    self.ui.sfocus = Some(0);
                    self.ui.st.focus = false;
                }
                (true, SKey::Tab) => {
                    self.ui.sfocus = None;
                    self.ui.st.focus = true;
                    self.ui.caret_on = true;
                }
                (true, SKey::Up | SKey::Down) => return true,
                (true, _) => {
                    if let Some(SAct::Set(i, t)) = self.ui.slider_key(k, shift) {
                        self.nudge_slider(i, t);
                    }
                    return true;
                }
                (false, _) => return false,
            }
            self.redraw_ui();
            return true;
        }
        match self.ui.slider_key(k, shift) {
            Some(SAct::Set(i, t)) => self.nudge_slider(i, t),
            Some(SAct::Focus) => self.redraw_ui(),
            None => {}
        }
        true
    }

    /// Maps a virtual key to a slider key; true = used.
    unsafe fn slider_key_vk(&mut self, vk: VIRTUAL_KEY) -> bool {
        let k = match vk {
            VK_UP => SKey::Up,
            VK_DOWN => SKey::Down,
            VK_LEFT => SKey::Left,
            VK_RIGHT => SKey::Right,
            VK_HOME => SKey::Home,
            VK_END => SKey::End,
            VK_BACK | VK_DELETE => SKey::Reset,
            VK_TAB => SKey::Tab,
            _ => return self.ui.settings, // the settings panel has no text field: other keys are swallowed
        };
        self.slider_key(k, GetKeyState(VK_SHIFT.0 as i32) < 0)
    }

    /// A slider moved from the keyboard / wheel: shown and applied live like a drag, committed to config.json (`commit_look`, as on release)
    /// once the burst of key repeats is over.
    unsafe fn nudge_slider(&mut self, i: usize, t: f32) {
        if self.look_commit.is_some_and(|p| p != i) {
            self.flush_look_commit();
        }
        if self.ui.settings {
            self.set_look_slider(i, t);
            self.frame_row(i);
        } else {
            self.apply_glass(t.clamp(0.0, 1.0)); // the strip's glass slider
        }
        self.look_commit = Some(i);
        SetTimer(self.hwnd, T_LCOMMIT, LCOMMIT_AFTER, None);
    }

    unsafe fn flush_look_commit(&mut self) {
        let _ = KillTimer(self.hwnd, T_LCOMMIT);
        if let Some(i) = self.look_commit.take() {
            self.commit_look(i);
        }
    }

    /// Repaint the open panel (and the strip under the gallery).
    unsafe fn redraw_ui(&mut self) {
        let mut d = self.hover_dirty(Hit::Panel);
        if !self.ui.settings {
            if let (Some(a), Some(b)) = (d, self.hover_dirty(Hit::Toolbar)) {
                d = Some(union(a, b));
            }
        }
        if let Some(d) = d {
            self.frame(d);
        }
    }

    /// Remember where the user left the gallery / settings panel (only when it changed).
    fn save_panels(&mut self) {
        let now = (self.ui.gal_off, self.ui.set_off);
        if now == self.panels_saved {
            return;
        }
        self.panels_saved = now;
        if let Err(e) = layout::write_panels(now.0, now.1) {
            eprintln!("mswidgets: {e}");
        }
        self.note_own_write();
    }

    unsafe fn on_key(&mut self, msg: u32, wp: usize) {
        if self.ui.menu.is_some() {
            return self.menu_key(msg, wp);
        }
        if self.ui.prompt.is_some() {
            return self.prompt_key(msg, wp);
        }
        if !self.ui.edit {
            return;
        }
        let ctrl = GetKeyState(VK_CONTROL.0 as i32) < 0;
        let mut changed = false;
        match msg {
            WM_KEYDOWN | WM_SYSKEYDOWN => {
                let vk = VIRTUAL_KEY(wp as u16);
                match vk {
                    VK_ESCAPE => {
                        self.esc_down = true;
                        return if self.ui.settings { self.close_settings() } else { self.leave_edit() };
                    }
                    _ if !ctrl && self.slider_key_vk(vk) => return,
                    _ if !self.ui.gallery => return,
                    VK_BACK => {
                        self.ui.st.query.backspace();
                        changed = true;
                    }
                    VK_DELETE => {
                        self.ui.st.query.delete();
                        changed = true;
                    }
                    VK_LEFT => self.ui.st.query.left(),
                    VK_RIGHT => self.ui.st.query.right(),
                    VK_HOME => self.ui.st.query.home(),
                    VK_END => self.ui.st.query.end(),
                    VK_V if ctrl => {
                        if let Some(t) = clipboard_text(self.hwnd) {
                            self.ui.st.query.insert_str(&t);
                            changed = true;
                        }
                    }
                    VK_A if ctrl => self.ui.st.query.select_all(),
                    _ => return,
                }
            }
            WM_CHAR => {
                if !self.ui.gallery || ctrl {
                    return;
                }
                let u = wp as u32;
                let Some(c) = char::from_u32(u) else { return }; // lone surrogates (astral plane) are ignored: no emoji search
                if c.is_control() {
                    return;
                }
                self.ui.st.query.insert(c);
                changed = true;
            }
            _ => return,
        }
        self.ui.st.focus = true;
        self.ui.sfocus = None;
        self.ui.caret_on = true;
        if changed {
            self.ui.query_changed();
            self.gallery_changed();
        } else if let Some(d) = self.hover_dirty(Hit::Panel) {
            self.frame(d);
        }
    }

    // ------------------------------------------------ drag & drop

    fn occupied(&self, except: Option<&str>) -> Vec<grid::Place> {
        self.items.iter().filter(|it| Some(it.id.as_str()) != except && self.scene.index_of_id(&it.id).is_some()).map(|it| grid::Place::new(it.size, it.col as i32, it.row as i32)).collect()
    }

    /// Lift card `uid` and let it follow the pointer. (x0, y0) = where the press happened, (x, y) = the pointer now.
    unsafe fn begin_drag(&mut self, uid: usize, x0: i32, y0: i32, x: i32, y: i32, fresh: bool, from: Option<(i32, i32)>) {
        let Some(i) = self.scene.index_of_uid(uid) else { return };
        let c = &self.scene.cards[i];
        let off = if fresh { (c.w / 2, c.h / 2) } else { (x0 - c.x, y0 - c.y) };
        let from = from.or_else(|| self.items.iter().find(|it| it.id == c.id).map(|it| (it.col as i32, it.row as i32)));
        self.drag = Some(Drag { uid, off, cur: (x, y), target: None, fresh, from, vel: (0.0, 0.0) });
        self.boost_capture();
        self.press = Press::Drag;
        self.springs.remove(&uid);
        let mut xf = self.scene.cards[i].xf;
        xf.lift = xf.lift.max(0.02);
        self.scene.set_xf(i, xf);
        self.last_anim = Instant::now();
        let r = self.desired_window();
        self.set_window(r);
        self.set_transparent(false);
        SetCapture(self.hwnd);
        self.arm_anim();
        self.anim_step();
    }

    unsafe fn drag_from_preview(&mut self, k: usize, x: i32, y: i32) {
        let Some((def, size)) = self.ui.item(k) else { return };
        if self.scene.len() >= MAX_CARDS {
            return self.show_toast(self.ui.lang.pick("Слишком много виджетов", "Too many widgets").to_string());
        }
        let (lw, lh) = size.logical();
        let (w, h) = ((lw * self.cs).round() as i32, (lh * self.cs).round() as i32);
        let id = layout::new_id_time(def.type_id, &self.items);
        let spec = CardSpec { id: id.clone(), type_id: def.type_id.to_string(), size, cfg: serde_json::json!({}), x: x - w / 2, y: y - h / 2, w, h };
        let Some(uid) = self.spawn_card(&spec, false) else { return };
        self.items.push(layout::Item { id, type_id: def.type_id.to_string(), size, col: 0, row: 0, cfg: serde_json::json!({}) });
        self.begin_drag(uid, x, y, x, y, true, None);
    }

    /// Build the card for `spec`, tick it once, measure its backdrop and render it. Returns its uid.
    unsafe fn spawn_card(&mut self, spec: &CardSpec, pop: bool) -> Option<usize> {
        if self.scene.len() >= MAX_CARDS {
            return None;
        }
        let i = match self.scene.add_card(&self.gfx, spec) {
            Ok(i) => i,
            Err(e) => {
                eprintln!("mswidgets: add card '{}': {e}", spec.type_id);
                return None;
            }
        };
        let now = self.now();
        self.scene.tick_card(&self.gfx, i, &now);
        let _ = self.scene.measure_lum(&self.gfx, 1 << i, false);
        if let Err(e) = self.scene.render_card(&self.gfx, i, &now) {
            eprintln!("mswidgets: render card: {e}");
        }
        let uid = self.scene.cards[i].uid;
        if pop && !self.reduced {
            self.pops.insert(uid, Pop { t0: Instant::now(), out: false });
            self.scene.set_xf(i, mat_xf(&motion::materialize(0.0, false, false), 0.0, 0.0));
        }
        self.update_tick_timer();
        Some(uid)
    }

    /// Drop the dragged card: snap to the free cell (or go home / vanish), spring settle, persist.
    unsafe fn end_drag(&mut self) {
        let Some(d) = self.drag.take() else { return };
        self.press = Press::None;
        self.pressed = None;
        let _ = ReleaseCapture();
        self.ui.placeholder = None;
        let Some(i) = self.scene.index_of_uid(d.uid) else { return };
        let (id, size) = (self.scene.cards[i].id.clone(), self.scene.cards[i].size);
        let landing = d.target.map(|p| (p.col, p.row)).or(d.from);
        let dropped_on_gallery = d.fresh && self.ui.gallery && self.ui.edit && {
            let (lx, ly) = (d.cur.0 as f32 / self.s, d.cur.1 as f32 / self.s);
            self.ui.panel_rect().contains(lx, ly)
        };
        match landing {
            Some((col, row)) if !dropped_on_gallery => {
                if let Some(it) = self.items.iter_mut().find(|x| x.id == id) {
                    it.col = col as u32;
                    it.row = row as u32;
                }
                let (x, y, _, _) = cell_rect(size, col, row, self.cs);
                let c = &self.scene.cards[i];
                self.springs.insert(d.uid, Spring { x: c.x as f32, y: c.y as f32, vx: 0.0, vy: 0.0, tx: x as f32, ty: y as f32 });
                self.save_layout();
                self.sync_regions();
            }
            _ => {
                // dropped on the gallery / nowhere free: a fresh card is discarded, a moved one stays where it was
                if d.fresh {
                    self.discard_card(d.uid);
                    if !dropped_on_gallery {
                        self.show_toast(self.ui.lang.pick("На рабочем столе нет свободного места", "No free space on the desktop").to_string());
                    }
                    return;
                }
            }
        }
        self.last_anim = Instant::now();
        self.arm_anim();
        self.anim_step();
    }

    /// Remove a card without animation (cancelled drag).
    unsafe fn discard_card(&mut self, uid: usize) {
        let Some(i) = self.scene.index_of_uid(uid) else { return };
        let r = self.scene.layer_rect_ext(i, self.ext());
        let id = self.scene.cards[i].id.clone();
        self.scene.remove_card(i);
        self.items.retain(|x| x.id != id);
        self.springs.remove(&uid);
        self.pops.remove(&uid);
        self.sync_regions();
        self.frame(r);
        self.update_tick_timer();
        self.arm_anim();
    }

    // ------------------------------------------------ add / remove / resize

    /// Gallery click / drop: a new card at (want) or the first free cell.
    unsafe fn add_widget(&mut self, type_id: &str, size: Size, want: Option<(i32, i32)>) {
        let Some(def) = crate::registry::global().get(type_id) else { return };
        let occ = self.occupied(None);
        let (c0, r0) = want.unwrap_or((0, 0));
        let Some(p) = grid::find_free(&self.dims, &occ, size, c0, r0) else {
            return self.show_toast(self.ui.lang.pick("На рабочем столе нет свободного места", "No free space on the desktop").to_string());
        };
        let id = layout::new_id_time(def.type_id, &self.items);
        let (x, y, w, h) = cell_rect(size, p.col, p.row, self.cs);
        let spec = CardSpec { id: id.clone(), type_id: def.type_id.to_string(), size, cfg: serde_json::json!({}), x, y, w, h };
        let Some(uid) = self.spawn_card(&spec, true) else { return };
        self.items.push(layout::Item { id, type_id: def.type_id.to_string(), size, col: p.col as u32, row: p.row as u32, cfg: serde_json::json!({}) });
        self.save_layout();
        self.sync_regions();
        self.last_anim = Instant::now();
        self.arm_anim();
        if let Some(i) = self.scene.index_of_uid(uid) {
            let r = self.scene.layer_rect_ext(i, self.ext());
            self.frame(r);
        }
    }

    /// "−" / menu "Удалить": pop out, then forget the card.
    unsafe fn remove_card(&mut self, uid: usize) {
        if self.reduced {
            self.finish_remove(uid);
            return;
        }
        self.pops.insert(uid, Pop { t0: Instant::now(), out: true });
        self.last_anim = Instant::now();
        self.arm_anim();
    }

    unsafe fn finish_remove(&mut self, uid: usize) {
        self.discard_card(uid);
        self.save_layout();
        if !self.want_full() {
            SetTimer(self.hwnd, T_SHRINK, 150, None);
        }
    }

    unsafe fn change_size(&mut self, uid: usize, size: Size) {
        let Some(i) = self.scene.index_of_uid(uid) else { return };
        let id = self.scene.cards[i].id.clone();
        let Some(me) = self.items.iter().find(|x| x.id == id).cloned() else { return };
        if me.size == size {
            return;
        }
        let occ = self.occupied(Some(&id));
        let Some(p) = grid::resize(&self.dims, &occ, size, me.col as i32, me.row as i32) else {
            return self.show_toast(self.ui.lang.pick("Нет места для этого размера", "No room for this size").to_string());
        };
        let old = self.scene.layer_rect_ext(i, self.ext());
        let (x, y, _, _) = cell_rect(size, p.col, p.row, self.cs);
        if let Err(e) = self.scene.resize_card(&self.gfx, i, size, x, y) {
            eprintln!("mswidgets: resize: {e}");
            return;
        }
        let now = self.now();
        self.scene.tick_card(&self.gfx, i, &now);
        let _ = self.scene.measure_lum(&self.gfx, 1 << i, false);
        let _ = self.scene.render_card(&self.gfx, i, &now);
        if let Some(it) = self.items.iter_mut().find(|x| x.id == id) {
            it.size = size;
            it.col = p.col as u32;
            it.row = p.row as u32;
        }
        if !self.reduced {
            self.pops.insert(uid, Pop { t0: Instant::now(), out: false });
            self.scene.set_xf(i, mat_xf(&motion::materialize(0.0, false, false), 0.0, 0.0));
        }
        self.save_layout();
        self.sync_regions();
        self.update_tick_timer();
        let d = union(old, self.scene.layer_rect_ext(i, self.ext()));
        // the window may have to grow to hold the new size
        let r = self.desired_window();
        let w = if self.want_full() { r } else { union(self.win, r) };
        self.set_window(w);
        self.last_anim = Instant::now();
        self.arm_anim();
        self.frame(d);
        if !self.want_full() {
            SetTimer(self.hwnd, T_SHRINK, 400, None);
        }
    }

    // ------------------------------------------------ context menu glue

    unsafe fn menu_request(&mut self, uid: usize, lp: isize) -> Option<MenuReq> {
        let i = self.scene.index_of_uid(uid)?;
        let c = &self.scene.cards[i];
        let def = crate::registry::global().get(&c.type_id)?;
        let (x, y) = unpack(lp);
        Some(MenuReq { uid, x, y, sizes: def.sizes.to_vec(), cur: c.size, editing: self.ui.edit, lang: self.ui.lang })
    }

    unsafe fn menu_result(&mut self, req: &MenuReq, cmd: usize) {
        self.in_menu = false;
        match cmd {
            CMD_EDIT => {
                if self.editing {
                    self.leave_edit();
                } else {
                    self.enter_edit();
                }
            }
            c if (CMD_SIZE..CMD_SIZE + 3).contains(&c) => self.change_size(req.uid, Size::ALL[c - CMD_SIZE]),
            CMD_DELETE => self.remove_card(req.uid),
            _ => {}
        }
        if !self.ui.edit {
            place_above_desktop(self.hwnd);
            if !self.want_full() {
                SetTimer(self.hwnd, T_SHRINK, 150, None);
            }
        }
    }

    // ------------------------------------------------ animation

    fn anim_busy(&self) -> bool {
        let want_gal = if self.editing { 1.0 } else { 0.0 };
        self.drag.is_some()
            || !self.springs.is_empty()
            || !self.pops.is_empty()
            || (self.gal_t - want_gal).abs() > 1e-3
            || (self.dim_t - want_gal).abs() > 1e-3
            || self.scene.cards.iter().any(|c| c.xf.lift > 0.0)
            || self.scene.fx_busy()
            || self.ui.away > 0.0
            || self.jig_amp > 0.0 && !(self.ui.edit && !self.ui.settings && self.editing && !self.reduced)
            || self.jig_amp < 1.0 && self.ui.edit && !self.ui.settings && self.editing && !self.reduced
            || self.ui.sa_busy()
            || self.ui.gal_busy()
            || self.ui.toast_busy(self.reduced)
    }

    /// One timer drives every animation: 16 ms while something moves, 33 ms for the jiggle of a resting edit mode, off otherwise.
    unsafe fn arm_anim(&mut self) {
        let period = if self.anim_busy() {
            16
        } else if self.ui.edit && !self.ui.settings && !self.reduced {
            33 // the jiggle period is 0.8 s: 100 ms steps moved a big card's edge by 2-3 px per frame
        } else {
            0
        };
        if period != self.anim_period {
            if self.anim_period == 0 {
                self.last_anim = Instant::now(); // the first step of a fresh run is one period long, not the whole idle time
            }
            self.anim_period = period;
            if period == 0 {
                let _ = KillTimer(self.hwnd, T_ANIM);
            } else {
                SetTimer(self.hwnd, T_ANIM, period, None);
            }
        }
    }

    unsafe fn anim_step(&mut self) {
        let t = Instant::now();
        let dt = t.saturating_duration_since(self.last_anim).as_secs_f32().clamp(0.004, 0.05);
        self.last_anim = t;
        let ext = self.ext();
        let mut dirty: Option<RECT> = None;
        let add = |d: &mut Option<RECT>, r: RECT| *d = Some(d.map_or(r, |x| union(x, r)));

        // gallery / dim progress
        let want = if self.editing { 1.0 } else { 0.0 };
        if self.reduced {
            self.gal_t = want;
            self.dim_t = want;
        } else {
            let rate = if self.editing { 1.0 / 0.34 } else { 1.0 / 0.2 };
            self.gal_t = step_toward(self.gal_t, want, rate * dt);
            self.dim_t = step_toward(self.dim_t, want, dt / 0.25);
        }
        let eased = 1.0 - (1.0 - self.gal_t).powi(3);
        if (self.ui.anim - eased).abs() > 1e-4 {
            self.ui.anim = eased;
            if let Some(u) = self.ui.dirty_rect(&self.scene) {
                add(&mut dirty, u);
            }
        }
        // the dim covers the whole monitor: it fades in 6 steps (each one a full-monitor compose + present)
        let dim = (if self.ui.settings { ui::SETTINGS_DIM } else { DIM_ALPHA }) * (self.dim_t * 6.0).round() / 6.0;
        if (self.ui.dim - dim).abs() > 1e-4 {
            self.ui.dim = dim;
            add(&mut dirty, RECT { left: 0, top: 0, right: self.scene.w, bottom: self.scene.h });
        }

        // the dragged card follows the pointer
        let now0 = self.now();
        if let Some(d) = self.drag.as_mut() {
            if let Some(i) = self.scene.cards.iter().position(|c| c.uid == d.uid) {
                let c = &self.scene.cards[i];
                let old = self.scene.layer_rect_ext(i, ext);
                let nx = (d.cur.0 - d.off.0).clamp(-c.w / 2, self.scene.w - c.w / 2);
                let ny = (d.cur.1 - d.off.1).clamp(-c.h / 2, self.scene.h - c.h / 2);
                let (size, id) = (c.size, c.id.clone());
                let occ = self.items.iter().filter(|it| it.id != id && self.scene.index_of_id(&it.id).is_some()).map(|it| grid::Place::new(it.size, it.col as i32, it.row as i32)).collect::<Vec<_>>();
                let target = grid::target_for(&self.dims, &occ, size, nx, ny, self.cs);
                let ph_old = self.ui.placeholder;
                self.ui.placeholder = target.map(|p| {
                    let (x, y, w, h) = cell_rect(size, p.col, p.row, self.cs);
                    (RECT { left: x, top: y, right: x + w, bottom: y + h }, self.scene.radius_of(size) * self.cs)
                });
                if let Some((r, _)) = ph_old {
                    add(&mut dirty, RECT { left: r.left - 4, top: r.top - 4, right: r.right + 4, bottom: r.bottom + 4 });
                }
                if let Some((r, _)) = self.ui.placeholder {
                    add(&mut dirty, RECT { left: r.left - 4, top: r.top - 4, right: r.right + 4, bottom: r.bottom + 4 });
                }
                d.target = target;
                let lum_ok = self.scene.cards[i].x != nx || self.scene.cards[i].y != ny;
                // card velocity (low-passed) for the light inertia
                let (ivx, ivy) = ((nx - self.scene.cards[i].x) as f32 / dt, (ny - self.scene.cards[i].y) as f32 / dt);
                d.vel = (d.vel.0 * 0.55 + ivx * 0.45, d.vel.1 * 0.55 + ivy * 0.45);
                if lum_ok {
                    self.scene.set_card_pos(i, nx, ny);
                    let _ = self.scene.render_card(&self.gfx, i, &now0);
                }
                add(&mut dirty, old);
                add(&mut dirty, self.scene.layer_rect_ext(i, ext));
            }
        }
        // the gallery fades while a card is out on the desk and comes back when it is dropped
        if self.ui.gallery && self.ui.edit {
            let out = self.drag.as_ref().is_some_and(|d| !self.ui.panel_rect().contains(d.cur.0 as f32 / self.s, d.cur.1 as f32 / self.s));
            let a = step_toward(self.ui.away, if out { 1.0 } else { 0.0 }, dt / 0.15);
            if (a - self.ui.away).abs() > 1e-4 {
                self.ui.away = a;
                if let Some(u) = self.ui.dirty_rect(&self.scene) {
                    add(&mut dirty, u);
                }
            }
        }

        // springs (settle after a drop)
        let mut settled = false;
        let uids: Vec<usize> = self.springs.keys().copied().collect();
        for uid in uids {
            let Some(i) = self.scene.index_of_uid(uid) else {
                self.springs.remove(&uid);
                continue;
            };
            let old = self.scene.layer_rect_ext(i, ext);
            let done = match self.springs.get_mut(&uid) {
                Some(sp) => sp.step(dt),
                None => continue,
            };
            let sp = self.springs[&uid];
            self.scene.set_card_pos(i, sp.x.round() as i32, sp.y.round() as i32);
            let now = self.now();
            let _ = self.scene.render_card(&self.gfx, i, &now);
            add(&mut dirty, old);
            add(&mut dirty, self.scene.layer_rect_ext(i, ext));
            if done {
                self.springs.remove(&uid);
                // landed: the backdrop (luminance, capture region) is the one under the new place
                let _ = self.scene.measure_lum(&self.gfx, 1 << i, false);
                let _ = self.scene.render_card(&self.gfx, i, &now);
                settled = true;
            }
        }
        if settled {
            self.sync_regions();
        }

        // pops (add / resize / remove)
        let reduced = self.reduced;
        let mut finished_out = Vec::new();
        let mut finished: Vec<usize> = Vec::new();
        for (uid, p) in self.pops.iter() {
            if p.at(t, reduced).finished {
                finished.push(*uid);
            }
        }
        for uid in finished {
            if let Some(p) = self.pops.remove(&uid) {
                if p.out {
                    finished_out.push(uid);
                }
            }
        }

        // light inertia: every card's light is pushed against its own velocity (dragged: the pointer, settling: the spring)
        for i in 0..self.scene.len() {
            let uid = self.scene.cards[i].uid;
            let v = match (self.drag.as_ref().filter(|d| d.uid == uid), self.springs.get(&uid)) {
                (Some(d), _) => d.vel,
                (None, Some(sp)) => (sp.vx, sp.vy),
                _ => (0.0, 0.0),
            };
            self.scene.set_light_drive(i, v.0, v.1);
        }

        // per-card visual transform: jiggle, lift, pop
        let jig_t = t.saturating_duration_since(self.t_edit).as_secs_f32();
        // the jiggle swells in and fades out (leaving edit mode, opening the settings) instead of snapping on and off
        let want_jig = if self.ui.edit && !self.ui.settings && self.editing && !self.reduced { 1.0 } else { 0.0 };
        self.jig_amp = if self.reduced { want_jig } else { step_toward(self.jig_amp, want_jig, dt / 0.4) };
        let amp = self.jig_amp * self.jig_amp * (3.0 - 2.0 * self.jig_amp);
        for i in 0..self.scene.len() {
            let c = &self.scene.cards[i];
            let dragging = self.drag.as_ref().is_some_and(|d| d.uid == c.uid);
            let lift = if dragging { (c.xf.lift + dt / 0.12).min(1.0) } else { (c.xf.lift - dt / 0.18).max(0.0) };
            // the same swing of the edges for every size: the angle shrinks with the card's half-diagonal (S = 1.2 degrees)
            let k = (170.0 * self.cs * std::f32::consts::SQRT_2) / (c.w as f32).hypot(c.h as f32);
            let rot = if amp > 0.0 && !dragging { ui::jiggle(i, jig_t) * amp * k.min(1.0) } else { 0.0 };
            let xf = match self.pops.get(&c.uid) {
                Some(p) => mat_xf(&p.at(t, reduced), rot, lift),
                None => Xf { rot, scale: 1.0 + 0.03 * lift, alpha: 1.0, lift, blur: 0.0, sweep: -1.0 },
            };
            if xf != c.xf {
                self.scene.set_xf(i, xf);
                add(&mut dirty, self.scene.layer_rect_ext(i, ext));
            }
        }
        for uid in finished_out {
            self.finish_remove(uid);
        }
        // glass motion: light spring, pointer glint, appearance cross-fade
        if let Some(r) = self.scene.step_fx(&self.gfx, dt, &now0) {
            add(&mut dirty, r);
        }
        // the settings controls (hover, sliding segment, switches, handles) repaint only the controls that moved
        if let Some(r) = self.ui.step_sa(dt, self.reduced) {
            add(&mut dirty, r);
        }
        if let Some(r) = self.ui.step_gal(dt, self.reduced) {
            add(&mut dirty, r);
        }
        // toast: rise / fade; dropped once the fade-out ended
        let (toast_ch, toast_gone) = self.ui.toast_step(dt, self.reduced);
        if toast_ch {
            add(&mut dirty, self.toast_rect());
        }

        if let Some(d) = dirty {
            self.frame(d);
        }
        if toast_gone {
            self.fit_window(); // the toast needed the whole monitor in live mode
        }
        // finished?
        if !self.editing && self.ui.edit && self.gal_t <= 0.0 && self.dim_t <= 0.0 && self.drag.is_none() {
            self.finish_leave_edit();
        }
        if self.drag.is_none() && self.springs.is_empty() && !self.ui.edit && !self.in_menu {
            SetTimer(self.hwnd, T_SHRINK, 150, None);
        }
        self.arm_anim();
    }

    // ------------------------------------------------ edit mode

    unsafe fn set_transparent(&mut self, on: bool) {
        let ex = GetWindowLongPtrW(self.hwnd, GWL_EXSTYLE);
        let tr = WS_EX_TRANSPARENT.0 as isize;
        SetWindowLongPtrW(self.hwnd, GWL_EXSTYLE, if on { ex | tr } else { ex & !tr });
        self.interactive = !on;
    }

    fn tray_state(&self) -> tray::MenuState {
        let root = layout::read_config();
        let profiles = layout::profile_names(&root);
        let active = layout::active_profile(&root).and_then(|a| profiles.iter().position(|p| *p == a));
        let list = monitors::enumerate();
        let monitor = monitors::pick(&list).map_or(0, |i| monitors::position(&list, i));
        let monitors = (0..list.len()).map(|p| monitors::label(&list, p, self.ui.lang)).collect();
        tray::MenuState { lang: self.ui.lang, editing: self.editing, hidden: self.user_hidden, autostart: crate::autostart::enabled(), profiles, active, monitors, monitor }
    }

    /// A tray click / menu choice.
    unsafe fn apply_tray(&mut self, cmd: TrayCmd) {
        if matches!(cmd, TrayCmd::Settings | TrayCmd::Edit) && self.user_hidden {
            self.user_hidden = false; // asking for the settings brings the widgets back
            self.guard();
        }
        match cmd {
            TrayCmd::None => {}
            TrayCmd::Settings => self.open_settings(),
            TrayCmd::Edit => {
                if self.editing {
                    self.leave_edit();
                } else {
                    self.enter_edit();
                }
            }
            TrayCmd::ToggleVisible => {
                self.user_hidden = !self.user_hidden;
                self.guard();
            }
            TrayCmd::ToggleAutostart => {
                if let Err(e) = crate::autostart::set(!crate::autostart::enabled()) {
                    self.show_toast(format!("{}: {e}", self.ui.lang.pick("Автозапуск не изменён", "Autostart not changed")));
                }
            }
            TrayCmd::Profile(i) => self.switch_profile(i),
            TrayCmd::SaveProfile => self.ask_profile_name(),
            TrayCmd::DeleteProfile => self.delete_profile(),
            TrayCmd::Monitor(i) => self.set_monitor(i),
            TrayCmd::Quit => {
                let _ = DestroyWindow(self.hwnd);
            }
        }
    }

    /// Entry point of the settings panel (tray menu, Ctrl+Alt+G, the strip's button): from the live desk the layer becomes interactive
    /// exactly as for edit mode (activatable, raised, click-through off); inside edit mode the panel replaces the gallery.
    unsafe fn open_settings(&mut self) {
        self.abort_menu();
        if self.ui.settings {
            return;
        }
        if self.editing {
            self.switch_panel(true);
        } else {
            self.enter_edit_as(true);
        }
    }

    /// Esc / Done / click outside / Ctrl+Alt+G: back to the gallery when the panel came from it, else out of edit mode altogether.
    unsafe fn close_settings(&mut self) {
        if !self.ui.settings {
            return;
        }
        if self.settings_back {
            self.switch_panel(false);
        } else {
            self.leave_edit();
        }
    }

    /// The values the settings panel shows (behaviour options, the autostart switch); the look sliders are kept in `ui` all the time.
    fn sync_settings_ui(&mut self) {
        self.ui.appearance = self.settings.appearance;
        self.ui.dim_mode = self.settings.dim;
        self.ui.live = self.settings.live;
        self.ui.accent_mode = self.settings.accent;
        self.ui.accent_scope = self.settings.accent_scope;
        self.ui.autostart = crate::autostart::enabled();
        self.ui.check_start = self.settings.check_updates;
        self.ui.sa_reset(); // the panel opens showing the real values, nothing glides from stale ones
    }

    /// Swap the open panel in place (gallery <-> settings) inside the running edit mode; the new panel fades in.
    unsafe fn switch_panel(&mut self, settings: bool) {
        if !self.editing || self.ui.settings == settings {
            return;
        }
        if self.drag.is_some() {
            self.end_drag();
        }
        self.flush_look_commit();
        self.ui.sfocus = None;
        self.ui.settings = settings;
        self.ui.gallery = !settings;
        self.settings_back = settings;
        self.ui.hover = Hit::None;
        self.ui.press = Hit::None;
        self.press = Press::None;
        self.ui.st.focus = !settings;
        self.ui.caret_on = true;
        if settings {
            self.sync_settings_ui();
        } else {
            self.ui.sync_previews(&self.gfx, &Now::local());
        }
        self.ui.anim = 0.0;
        self.gal_t = 0.0;
        self.ui.sync_panels(&mut self.scene, &self.gfx);
        self.sync_regions();
        self.last_anim = Instant::now();
        self.arm_ui_timer();
        self.arm_anim();
        self.anim_step();
        let d = RECT { left: 0, top: 0, right: self.scene.w, bottom: self.scene.h };
        self.frame(d);
    }

    unsafe fn enter_edit(&mut self) {
        self.enter_edit_as(false);
    }

    unsafe fn enter_edit_as(&mut self, settings: bool) {
        self.abort_menu(); // the menu belongs to the desk as it was
        if self.editing || self.ui.edit || !self.visible {
            return;
        }
        self.editing = true;
        self.boost_capture();
        self.was_fg = false;
        self.reduced = reduced_motion();
        let now = self.now();
        self.leave(&now); // the widget under the cursor stops hovering
        self.ui.edit = true;
        self.update_dim();
        self.t_edit = Instant::now();
        // keyboard: the layer becomes activatable and takes the foreground (the search box needs it); remember who had it
        let fg = GetForegroundWindow();
        if fg != self.hwnd {
            self.prev_fg = fg;
        }
        let ex = GetWindowLongPtrW(self.hwnd, GWL_EXSTYLE);
        SetWindowLongPtrW(self.hwnd, GWL_EXSTYLE, ex & !(WS_EX_NOACTIVATE.0 as isize) & !(WS_EX_TRANSPARENT.0 as isize));
        self.interactive = true;
        let _ = SetWindowPos(self.hwnd, HWND_TOP, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
        self.ui.edit = true;
        self.ui.gallery = !settings;
        self.ui.settings = settings;
        self.settings_back = false;
        self.ui.sfocus = None;
        self.ui.st.focus = !settings;
        self.ui.caret_on = true;
        self.ui.anim = 0.0;
        self.ui.dim = 0.0;
        self.gal_t = 0.0;
        self.dim_t = 0.0;
        let now = Now::local();
        let r = RECT { left: 0, top: 0, right: self.scene.w, bottom: self.scene.h };
        self.set_window(r);
        if settings {
            self.sync_settings_ui();
        } else {
            self.ui.sync_previews(&self.gfx, &now);
        }
        self.ui.sync_panels(&mut self.scene, &self.gfx);
        self.sync_regions();
        force_foreground(self.hwnd);
        let _ = SetFocus(self.hwnd);
        self.last_anim = Instant::now();
        self.arm_ui_timer();
        self.arm_anim();
        self.anim_step();
    }

    /// Esc / "Готово" / hotkey / menu: start the closing animation; `finish_leave_edit` runs when it is over.
    unsafe fn leave_edit(&mut self) {
        self.abort_menu();
        if !self.editing {
            return;
        }
        self.editing = false;
        self.boost_capture();
        self.flush_look_commit();
        self.ui.sfocus = None;
        if self.drag.is_some() {
            self.end_drag();
        }
        self.ui.press = Hit::None;
        self.ui.hover = Hit::None;
        self.last_anim = Instant::now();
        self.arm_anim();
        self.anim_step();
    }

    unsafe fn finish_leave_edit(&mut self) {
        self.ui.edit = false;
        self.update_dim();
        self.ui.gallery = false;
        self.ui.settings = false;
        self.settings_back = false;
        self.ui.anim = 0.0;
        self.ui.dim = 0.0;
        self.ui.st.focus = false;
        self.ui.sync_panels(&mut self.scene, &self.gfx);
        self.sync_regions();
        let _ = KillTimer(self.hwnd, T_UI);
        // activation off again, the layer goes back under the app windows and the foreground to whoever had it
        let ex = GetWindowLongPtrW(self.hwnd, GWL_EXSTYLE);
        SetWindowLongPtrW(self.hwnd, GWL_EXSTYLE, ex | WS_EX_NOACTIVATE.0 as isize | WS_EX_TRANSPARENT.0 as isize);
        self.interactive = false;
        if GetForegroundWindow() == self.hwnd && !self.prev_fg.0.is_null() && IsWindow(self.prev_fg).as_bool() {
            let _ = SetForegroundWindow(self.prev_fg);
        }
        place_above_desktop(self.hwnd);
        self.fit_window();
        let w = self.win;
        self.frame(w);
        self.arm_anim();
    }

    // ------------------------------------------------ gallery timer / toast

    unsafe fn arm_ui_timer(&mut self) {
        if self.ui.edit && (self.ui.gallery || self.ui.settings) {
            SetTimer(self.hwnd, T_UI, 200, None);
        }
    }

    /// 200 ms while the gallery is open: animated previews (<= 5 fps), the search caret blink, Esc fallback.
    unsafe fn ui_timer(&mut self) {
        if !(self.ui.edit && (self.ui.gallery || self.ui.settings)) {
            let _ = KillTimer(self.hwnd, T_UI);
            return;
        }
        let mut redraw = false;
        if self.ui.gallery && self.ui.refresh_previews(&self.gfx, &Now::local()) {
            redraw = true;
        }
        self.ui_tick += 1;
        if self.ui.gallery && self.ui.st.focus && self.ui_tick % 3 == 0 {
            self.ui.caret_on = !self.ui.caret_on;
            redraw = true;
        }
        // Esc even when the key message went elsewhere (the layer is foreground, but a popup may have eaten it)
        let esc = GetAsyncKeyState(VK_ESCAPE.0 as i32) < 0 && GetForegroundWindow() == self.hwnd;
        if esc && !self.esc_down {
            self.esc_down = true;
            return if self.ui.settings { self.close_settings() } else { self.leave_edit() };
        }
        if !esc {
            self.esc_down = false;
        }
        if redraw {
            if let Some(d) = self.hover_dirty(Hit::Panel) {
                self.frame(d);
            }
        }
    }

    fn toast_rect(&self) -> RECT {
        let s = self.s;
        RECT { left: ((self.ui.mw / 2.0 - 420.0) * s) as i32, top: ((self.ui.mh * 0.5 - 40.0) * s) as i32, right: ((self.ui.mw / 2.0 + 420.0) * s) as i32, bottom: ((self.ui.mh * 0.5 + 40.0) * s) as i32 }
    }

    unsafe fn show_toast(&mut self, msg: String) {
        self.ui.toast_show(msg, self.reduced);
        SetTimer(self.hwnd, T_TOAST, 2000, None);
        let r = self.desired_window();
        self.set_window(r); // live mode: the layer only covers the cards, the toast sits mid-screen
        let d = self.toast_rect();
        self.frame(d); // arms the animation timer for the rise
    }

    // ------------------------------------------------ settings

    fn note_own_write(&mut self) {
        self.cfg_mtime = layout::config_mtime();
    }

    fn write_setting(&mut self, key: &str, v: serde_json::Value) {
        if let Err(e) = layout::write_setting(key, v) {
            eprintln!("mswidgets: {e}");
        }
        self.note_own_write();
    }

    fn save_layout(&mut self) {
        if let Err(e) = layout::save_layout(&self.items) {
            eprintln!("mswidgets: layout not saved: {e}");
        }
        self.note_own_write();
    }

    /// The glass slider: new tint strength for every card, live.
    unsafe fn apply_glass(&mut self, g: f32) {
        if (self.ui.glass - g).abs() < 1e-4 {
            return;
        }
        self.boost_capture();
        self.ui.glass = g;
        if let Err(e) = self.scene.set_glass(&self.gfx, g) {
            eprintln!("mswidgets: glass: {e}");
            return;
        }
        let now = self.now();
        for i in 0..self.scene.len() {
            let _ = self.scene.render_card(&self.gfx, i, &now);
        }
        let d = self.all_dirty();
        self.frame(d);
    }

    /// A look slider of the settings panel moved to `t` (0..1; slider 0 = Glass): the panel shows it at once, the cards follow at most
    /// 10 times a second (`T_LOOK` applies a change that arrived too early; `commit_look` applies the final value on release).
    unsafe fn set_look_slider(&mut self, i: usize, t: f32) {
        if i == 0 {
            self.ui.glass = t.clamp(0.0, 1.0);
        } else if let Some(d) = crate::glass::LOOK.get(i - 1) {
            d.set_pos(&mut self.ui.look, &self.ui.base, t);
        }
        self.look_dirty = true;
        let wait = LOOK_EVERY.saturating_sub(self.look_last.elapsed());
        if wait.is_zero() {
            self.apply_look_now();
            return;
        }
        SetTimer(self.hwnd, T_LOOK, wait.as_millis().max(1) as u32, None);
        // only the row of the slider is redrawn meanwhile: the knob and the value follow the pointer
        self.frame_row(i);
    }

    /// Repaint row `i` of the settings panel.
    unsafe fn frame_row(&mut self, i: usize) {
        let s = self.s;
        if let Some(r) = self.ui.set_lay().rows.get(i).map(|r| r.row) {
            self.frame(RECT { left: ((r.x - 8.0) * s) as i32, top: ((r.y - 4.0) * s) as i32, right: ((r.right() + 8.0) * s).ceil() as i32, bottom: ((r.bottom() + 4.0) * s).ceil() as i32 });
        }
    }

    /// Give the cards the look / glass value the panel shows: new effect graphs, every layer re-rendered, then the cards' area and the panel recomposed.
    unsafe fn apply_look_now(&mut self) {
        self.look_dirty = false;
        self.look_last = Instant::now();
        let _ = KillTimer(self.hwnd, T_LOOK);
        // the card scale applies on release (`apply_card_scale`: it needs a new desk grid and capture); nothing else changed = nothing to do
        let mut look = self.ui.look;
        look.card_k = self.scene.params().card_k;
        if look == *self.scene.params() && (self.ui.glass - self.scene.glass_value()).abs() < 1e-6 {
            return;
        }
        let ok = self.scene.set_glass(&self.gfx, self.ui.glass).and_then(|_| self.scene.set_params(&self.gfx, &look));
        if let Err(e) = ok {
            eprintln!("mswidgets: look: {e}");
            return;
        }
        let now = self.now();
        let mut cards: Option<RECT> = None;
        for i in 0..self.scene.len() {
            let _ = self.scene.render_card(&self.gfx, i, &now);
            let r = self.scene.layer_rect_ext(i, self.ext());
            cards = Some(cards.map_or(r, |d| union(d, r)));
        }
        if let Some(d) = cards {
            self.frame(d);
        }
        if let Some(u) = self.ui.dirty_rect(&self.scene) {
            self.frame(u);
        }
    }

    /// A look slider was released: the final value reaches the cards at once and config.json (`widgetsGlass` / `widgetsLook`).
    unsafe fn commit_look(&mut self, i: usize) {
        if self.look_dirty {
            self.apply_look_now();
        }
        if i == 0 {
            let v = (self.ui.glass * 100.0).round() / 100.0;
            self.write_setting("widgetsGlass", serde_json::json!(v));
            self.settings.glass = v;
        } else {
            self.save_look();
            self.apply_card_scale();
        }
    }

    /// The card scale slider settled: lay the desk out again (new grid, margin and capture regions; widgets and their state stay).
    unsafe fn apply_card_scale(&mut self) {
        if (self.ui.look.card_k - self.scene.params().card_k).abs() > 1e-4 {
            self.rebuild(true);
        }
    }

    /// Write the sliders' differences from the base look into `widgetsLook` (an empty diff removes the sliders' keys).
    fn save_look(&mut self) {
        let diff = crate::glass::look_diff(&self.look_base, &self.ui.look);
        match layout::write_look(&diff) {
            Ok(()) => self.look_cfg = layout::merge_look(&self.look_cfg, &diff),
            Err(e) => eprintln!("mswidgets: {e}"),
        }
        self.note_own_write();
    }

    /// "Reset look": the base look (defaults + glass.json) and the default glass value.
    unsafe fn reset_look(&mut self) {
        self.ui.look = self.look_base;
        self.ui.glass = layout::Settings::default().glass;
        self.apply_look_now();
        self.write_setting("widgetsGlass", serde_json::json!(self.ui.glass));
        self.settings.glass = self.ui.glass;
        self.save_look();
        self.apply_card_scale();
    }

    /// A segmented control of the settings panel: appearance, dimming, glass source.
    unsafe fn set_option(&mut self, o: ui::Opt, k: usize) {
        match o {
            ui::Opt::Appearance => {
                let p = ui::Opt::appearance(k);
                self.ui.appearance = p;
                self.settings.appearance = p;
                self.boost_capture();
                self.scene.set_appearance_pref(p);
                self.last_anim = Instant::now();
                self.arm_anim();
                self.write_setting("widgetsAppearance", serde_json::json!(layout::appearance_key(p)));
            }
            ui::Opt::Dim => {
                let d = ui::Opt::dim(k);
                self.ui.dim_mode = d;
                self.settings.dim = d;
                self.write_setting("widgetsDim", serde_json::json!(d.key()));
                self.update_dim(); // takes effect when the panel closes (the desk is not dimmed while editing)
            }
            ui::Opt::Source => self.set_live(k == 0),
            ui::Opt::Accent => {
                let m = ui::Opt::accent(k);
                self.ui.accent_mode = m;
                self.settings.accent = m;
                self.write_setting("accentMode", serde_json::json!(m.key()));
                self.refresh_accent(true);
            }
            ui::Opt::Scope => {
                let sc = ui::Opt::scope(k);
                self.ui.accent_scope = sc;
                self.settings.accent_scope = sc;
                self.write_setting("accentScope", serde_json::json!(sc.key()));
                self.apply_scope();
            }
            ui::Opt::Monitor => return self.set_monitor(k), // redraws the panel itself
        }
        if let Some(d) = self.hover_dirty(Hit::Panel) {
            self.frame(d);
        }
    }

    /// The "Start with Windows" switch: the per-user Run value is written / removed right now.
    unsafe fn toggle_autostart(&mut self) {
        match crate::autostart::set(!self.ui.autostart) {
            Ok(()) => self.ui.autostart = crate::autostart::enabled(),
            Err(e) => {
                eprintln!("mswidgets: autostart: {e}");
                self.show_toast(self.ui.lang.pick("Не удалось изменить автозапуск", "Could not change the autostart").to_string());
            }
        }
        if let Some(d) = self.hover_dirty(Hit::Panel) {
            self.frame(d);
        }
    }

    /// The "Check for updates on start" switch (`checkUpdates`, off by default).
    unsafe fn toggle_check_start(&mut self) {
        let on = !self.ui.check_start;
        self.ui.check_start = on;
        self.settings.check_updates = on;
        self.write_setting("checkUpdates", serde_json::json!(on));
        self.redraw_ui();
    }

    /// One GET of the latest release on a worker thread (`silent` = the check at start: only an available update is announced, as a toast).
    unsafe fn start_update_check(&mut self, silent: bool) {
        if matches!(self.ui.update, UpdateState::Checking | UpdateState::Downloading(_) | UpdateState::Verifying) {
            return;
        }
        self.update_silent = silent;
        self.ui.update = UpdateState::Checking;
        update::start(self.hwnd.0 as isize);
        if self.ui.settings {
            self.redraw_ui();
        }
    }

    unsafe fn update_done(&mut self) {
        let Some(r) = update::take() else { return };
        let silent = std::mem::take(&mut self.update_silent);
        self.ui.update = if silent && r.is_err() { UpdateState::Idle } else { update::state_from(r, env!("CARGO_PKG_VERSION")) };
        if let (true, UpdateState::Available { version, .. }) = (silent, &self.ui.update) {
            let msg = self.ui.lang.pick(&format!("Доступна новая версия {version}"), &format!("Version {version} is available")).to_string();
            self.show_toast(msg);
        }
        if self.ui.settings {
            self.redraw_ui();
        }
    }

    /// "Update now" (the user's click): download the installer of the available update and check its SHA-256 on a worker thread.
    unsafe fn start_install(&mut self) {
        let UpdateState::Available { version, asset, .. } = self.ui.update.clone() else { return };
        self.update_prev = Some(self.ui.update.clone());
        let Some(asset) = asset else {
            self.install_failed("the release has no installer with a checksum".into());
            return;
        };
        self.ui.update = UpdateState::Downloading(0);
        update::start_install(self.hwnd.0 as isize, &version, asset);
        self.redraw_ui();
    }

    /// `WM_UPDATE_DL`: download percent, "verifying", or the end of the worker.
    unsafe fn install_progress(&mut self, p: usize) {
        match p {
            update::DL_DONE => match update::take_install() {
                Some(Ok(v)) => {
                    self.flush_look_commit();
                    match update::launch(v) {
                        // the installer closes this copy itself; leaving now keeps its file replacement from racing with us
                        Ok(()) => {
                            let _ = DestroyWindow(self.hwnd);
                        }
                        Err(e) => self.install_failed(e),
                    }
                }
                Some(Err(e)) => self.install_failed(e),
                None => {}
            },
            update::DL_VERIFYING => self.ui.update = UpdateState::Verifying,
            p => self.ui.update = UpdateState::Downloading(p.min(100) as u8),
        }
        if self.ui.settings {
            self.redraw_ui();
        }
    }

    /// Back to the available update, with the reason shown in the status line (the button stays for a retry).
    unsafe fn install_failed(&mut self, why: String) {
        self.ui.update = match self.update_prev.take() {
            Some(UpdateState::Available { version, url, asset, .. }) => UpdateState::Available { version, url, asset, error: Some(why) },
            _ => UpdateState::Failed,
        };
        if self.ui.settings {
            self.redraw_ui();
        }
    }

    /// Live (desktop duplication) <-> plain glass.
    unsafe fn set_live(&mut self, live: bool) {
        if self.ui.live == live {
            return;
        }
        self.ui.live = live;
        self.settings.live = live;
        self.write_setting("widgetsGlassMode", serde_json::json!(if live { "capture" } else { "plain" }));
        self.rebuild(true);
    }

    /// Re-create the backdrop and every GPU object of the scene (settings / monitor / DPI change); widget instances stay.
    unsafe fn rebuild(&mut self, keep_layout: bool) {
        self.abort_prompt(); // new geometry: the prompt is cancelled
        self.abort_menu(); // ... and the menu is closed
        let mon = monitor();
        let (mw, mh) = (mon.rect.right - mon.rect.left, mon.rect.bottom - mon.rect.top);
        let s = if keep_layout && same_rect(&mon.rect, &self.mon) { self.s } else { mon.scale };
        // another monitor's desk (its own layout; the primary one when the chosen monitor is unplugged): every card is replaced
        let new_desk = mon.slot != self.slot;
        if new_desk {
            self.slot = mon.slot.clone();
            layout::set_slot(mon.slot.clone());
            self.items = layout::items_for(&layout::read_config(), mon.slot.as_deref());
            self.clear_desk();
        }
        (self.ui.mon_count, self.ui.mon_sel) = (mon.count, mon.pos);
        self.backdrop = None; // stops the capture thread before a new duplication is requested
        self.mon = mon.rect;
        self.work = mon.work;
        let cs = s * self.ui.look.card_k;
        self.s = s;
        self.cs = cs;
        self.dims = grid_dims(&self.mon, &self.work, cs);
        let m = (MARGIN * cs).round() as i32;
        // card positions: from the grid again (a new monitor / scale); the stored layout is not rewritten
        let specs = place_items(&mut self.items, &self.dims, cs);
        // a card that lost its room (bigger cards, a smaller monitor) leaves the desk, the stored layout keeps it
        let gone: Vec<usize> = self.scene.cards.iter().filter(|c| !specs.iter().any(|sp| sp.id == c.id)).map(|c| c.uid).collect();
        for uid in gone {
            if let Some(i) = self.scene.index_of_uid(uid) {
                self.scene.remove_card(i);
            }
            self.pops.remove(&uid);
        }
        let mut rects = Vec::new();
        for c in &self.scene.cards {
            match specs.iter().find(|sp| sp.id == c.id) {
                Some(sp) => rects.push((sp.x, sp.y, sp.w, sp.h)),
                None => rects.push((c.x, c.y, c.w, c.h)),
            }
        }
        let card_rects: Vec<RECT> = rects.iter().map(|&(x, y, w, h)| RECT { left: x, top: y, right: x + w, bottom: y + h }).collect();
        let bd = make_backdrop(&self.gfx, &mon, mw, mh, self.settings.live, capture_rate(self.fps, &self.settings), &region_list(&card_rects, None, None), m, self.gfx_luid);
        bd.set_hwnd(self.hwnd);
        self.use_dupl = bd.name().starts_with("desktop");
        if let Err(e) = self.scene.reconfigure(&self.gfx, mw, mh, cs, bd.bitmap(), &rects) {
            eprintln!("mswidgets: rebuild: {e}");
        }
        self.scene.params.card_k = self.ui.look.card_k; // the applied card scale
        self.backdrop = Some(bd);
        // cards that got room again come back
        for sp in &specs {
            if self.scene.index_of_id(&sp.id).is_none() {
                self.spawn_card(sp, false);
            }
        }
        self.ui.set_screen(s, mw as f32 / s, mh as f32 / s);
        self.springs.clear();
        let r = self.desired_window();
        // the window moves with the monitor: force a new position and size
        self.win = RECT { left: -1, top: -1, right: 0, bottom: 0 };
        self.set_window(r);
        self.ui.sync_panels(&mut self.scene, &self.gfx);
        self.sync_regions();
        let all = self.all_mask();
        // same order as at start: one frame while the GPU is quiet, THEN the capture thread (a new backdrop does nothing until started)
        self.redraw(all | BIT_GALLERY | BIT_TOOLBAR, true);
        self.bd().start();
        if self.use_dupl {
            let t = Instant::now();
            while self.bd().take_dirty() == 0 && t.elapsed() < Duration::from_millis(600) {
                std::thread::sleep(Duration::from_millis(15));
            }
        }
        self.redraw(all | BIT_GALLERY | BIT_TOOLBAR, true);
        let d = RECT { left: 0, top: 0, right: self.scene.w, bottom: self.scene.h };
        self.frame(d);
        self.update_tick_timer();
        if new_desk {
            self.pop_all_in();
        }
    }

    /// Display / DPI / work-area change, a monitor (un)plugged or a new `widgetsMonitor`: when the monitor the desk belongs on (see monitors.rs) is not
    /// the one we sit on, or its geometry / DPI changed, everything is re-placed. An unplugged monitor falls back to the primary one, and the desk
    /// returns when it comes back.
    unsafe fn replace_monitor(&mut self) {
        let cur = monitor();
        let wa = cur.work.bottom == self.work.bottom && cur.work.top == self.work.top;
        let (count, pos) = (self.ui.mon_count, self.ui.mon_sel);
        (self.ui.mon_count, self.ui.mon_sel) = (cur.count, cur.pos);
        if same_rect(&cur.rect, &self.mon) && (cur.scale - self.s).abs() < 0.01 && wa && cur.slot == self.slot {
            if (count, pos) != (cur.count, cur.pos) && self.ui.settings {
                self.redraw_ui(); // a monitor came or went: the "Monitor" control has another number of segments
            }
            return;
        }
        if self.drag.is_some() {
            self.end_drag();
        }
        eprintln!("mswidgets: monitor / DPI / work area changed, re-placing the widgets");
        self.rebuild(false);
    }

    /// Every card leaves the scene (the stored layout stays): a new desk is about to be built from `items`.
    unsafe fn clear_desk(&mut self) {
        let now = self.now();
        self.leave(&now);
        self.hot = None;
        self.pressed = None;
        self.press = Press::None;
        self.ui.hover = Hit::None;
        self.ui.press = Hit::None;
        while self.scene.len() > 0 {
            self.scene.remove_card(0);
        }
        self.pops.clear();
        self.springs.clear();
    }

    /// The freshly built desk condenses card by card (a plain appearance with reduced motion).
    unsafe fn pop_all_in(&mut self) {
        if !self.reduced {
            let t = Instant::now();
            for i in 0..self.scene.len() {
                let uid = self.scene.cards[i].uid;
                self.pops.insert(uid, Pop { t0: t + Duration::from_millis(60 * i as u64), out: false });
                self.scene.set_xf(i, mat_xf(&motion::materialize(0.0, false, false), 0.0, 0.0));
            }
            self.last_anim = t;
            self.arm_anim();
        }
        if !self.ui.edit && !self.want_full() {
            SetTimer(self.hwnd, T_SHRINK, 400, None);
        }
    }

    /// Move the desk to the monitor at display position `pos` (settings panel / tray menu): this monitor's layout is written back to its own slot,
    /// the choice is stored in `widgetsMonitor` (the primary monitor as "primary", the others by device name) and the desk is rebuilt there with
    /// that monitor's layout (an empty desk the first time). Nothing changes when config.json cannot be written.
    unsafe fn set_monitor(&mut self, pos: usize) {
        let list = monitors::enumerate();
        let Some(choice) = monitors::choice_for_position(&list, pos) else { return };
        if monitors::choice() == choice {
            return;
        }
        if self.drag.is_some() {
            self.end_drag();
        }
        if let Err(e) = layout::save_layout(&self.items).and_then(|_| layout::write_setting("widgetsMonitor", serde_json::json!(choice.key()))) {
            self.note_own_write();
            return self.notify(format!("{}: {e}", self.ui.lang.pick("Монитор не изменён", "Monitor not changed")));
        }
        self.note_own_write();
        monitors::set_choice(choice);
        self.replace_monitor();
        if self.ui.settings {
            self.redraw_ui();
        }
    }

    /// config.json changed behind our back (the settings of the Tauri app / a manual edit): glass, backdrop mode, dim.
    unsafe fn poll_config(&mut self) {
        if !self.visible {
            return;
        }
        let m = layout::config_mtime();
        if m == self.cfg_mtime {
            return;
        }
        self.cfg_mtime = m;
        let new = layout::read_settings();
        let old = self.settings;
        self.settings = new;
        if new.dim != old.dim {
            self.update_dim();
        }
        self.boost_capture(); // any edit of config.json: look at the glass at full rate for a while
        if new.appearance != old.appearance {
            self.scene.set_appearance_pref(new.appearance);
            self.last_anim = Instant::now();
            self.arm_anim();
        }
        if (new.glass - old.glass).abs() > 1e-4 {
            self.apply_glass(new.glass);
        }
        self.ui.appearance = new.appearance;
        self.ui.dim_mode = new.dim;
        self.ui.check_start = new.check_updates;
        if new.accent != old.accent {
            self.ui.accent_mode = new.accent;
            self.refresh_accent(true);
        }
        if new.accent_scope != old.accent_scope {
            self.ui.accent_scope = new.accent_scope;
            self.apply_scope();
        }
        if new.live != old.live || new.capture_fps != old.capture_fps {
            self.ui.live = new.live;
            self.rebuild(true);
        }
        // `widgetsMonitor` edited behind our back: the desk moves (our own writes set the choice first, so they are not seen as a change)
        if let Some(root) = layout::try_read_config() {
            let choice = monitors::Choice::from_value(&root);
            if choice != monitors::choice() {
                monitors::set_choice(choice);
                self.replace_monitor();
            }
        }
        // `widgetsLook` edited behind our back (the sliders of the Tauri app / by hand)
        let look = layout::read_look();
        if look != self.look_cfg {
            self.look_cfg = look;
            self.ui.look = crate::glass::apply_look(&self.look_base, &self.look_cfg);
            self.apply_look_now();
            self.apply_card_scale();
        }
    }

    // ------------------------------------------------ click-through

    /// WS_EX_TRANSPARENT toggled from a cursor poll: 100 ms normally, 16 ms within 8 px of a card.
    unsafe fn hit_test(&mut self) {
        if !self.visible {
            return;
        }
        if self.want_full() {
            if !self.interactive {
                self.set_transparent(false);
            }
            return;
        }
        let mut p = POINT::default();
        if GetCursorPos(&mut p).is_err() {
            return;
        }
        let (mut inside, mut near) = (false, false);
        for c in &self.scene.cards {
            let r = RECT { left: c.x + self.mon.left, top: c.y + self.mon.top, right: c.x + c.w + self.mon.left, bottom: c.y + c.h + self.mon.top };
            inside |= p.x >= r.left && p.x < r.right && p.y >= r.top && p.y < r.bottom;
            near |= p.x >= r.left - NEAR_PX && p.x < r.right + NEAR_PX && p.y >= r.top - NEAR_PX && p.y < r.bottom + NEAR_PX;
        }
        let held = self.interactive && (GetAsyncKeyState(0x01) < 0 || GetAsyncKeyState(0x02) < 0);
        let want = inside || held;
        if want != self.interactive {
            if !want && self.pressed.is_none() {
                let now = self.now();
                self.leave(&now); // the cursor left the cards: clear hover states
            }
            self.set_transparent(!want);
        }
        if near != self.near {
            self.near = near;
            SetTimer(self.hwnd, T_HIT, if near { 16 } else { 100 }, None);
        }
    }

    unsafe fn show(&mut self) {
        let _ = ShowWindow(self.hwnd, SW_SHOWNOACTIVATE);
        place_above_desktop(self.hwnd);
        self.visible = true;
        if let Some(b) = &self.backdrop {
            b.set_paused(false);
        }
        self.update_tick_timer();
    }

    /// One timer, armed for the earliest widget schedule; nothing is armed (zero wake-ups) while no visible card animates.
    unsafe fn update_tick_timer(&mut self) {
        match (self.visible, self.scene.next_due()) {
            (true, Some(d)) => {
                let ms = d.saturating_duration_since(Instant::now()).as_millis().clamp(1, 3_600_000) as u32;
                SetTimer(self.hwnd, T_TICK, ms, None);
            }
            _ => {
                let _ = KillTimer(self.hwnd, T_TICK);
            }
        }
    }

    /// Fullscreen guard (500 ms): hide while a fullscreen app is in front, pause the capture, re-stack on show.
    unsafe fn guard(&mut self) {
        // the menu closes when its card is gone (removed by a layout reload, a profile switch...)
        if self.ui.menu.as_ref().is_some_and(|m| !m.closing) && self.scene.index_of_uid(self.menu_uid).is_none() {
            self.abort_menu();
        }
        // the prompt / the menu end when another window takes the foreground (a click elsewhere); a layer that never got the keyboard is asked again
        let prompt_up = self.ui.prompt.as_ref().is_some_and(|p| !p.closing);
        if prompt_up || self.ui.menu.as_ref().is_some_and(|m| !m.closing) {
            let fg = GetForegroundWindow();
            let mut cls = [0u16; 64];
            let n = GetClassNameW(fg, &mut cls);
            let popup = String::from_utf16_lossy(&cls[..n.max(0) as usize]) == "#32768";
            if fg == self.hwnd {
                self.prompt_was_fg = true;
            } else if self.prompt_was_fg {
                if !fg.0.is_null() && !popup && self.prompt_t0.elapsed() > Duration::from_millis(1000) {
                    if prompt_up {
                        self.end_prompt(None);
                    } else {
                        self.close_menu();
                    }
                }
            } else if self.prompt_t0.elapsed() < Duration::from_millis(2000) {
                force_foreground(self.hwnd);
                let _ = SetFocus(self.hwnd);
            }
        }
        // edit mode ends when another window takes the foreground (alt-tab, a click on another app)
        if self.editing && self.ui.edit && !self.in_menu && self.drag.is_none() {
            let fg = GetForegroundWindow();
            let mut cls = [0u16; 64];
            let n = GetClassNameW(fg, &mut cls);
            let popup = String::from_utf16_lossy(&cls[..n.max(0) as usize]) == "#32768";
            if fg == self.hwnd {
                self.was_fg = true;
            } else if self.was_fg && !fg.0.is_null() && !popup && self.t_edit.elapsed() > Duration::from_millis(1500) {
                self.leave_edit();
            }
        }
        // insurance against a lost WM_BACKDROP wake-up (the capture thread only posts when its mask was empty)
        let m = self.bd().take_dirty();
        if m != 0 && self.visible {
            self.redraw(m, false);
        }
        let front = app_in_front(self.hwnd);
        if front != self.app_front {
            self.app_front = front;
            self.update_dim();
        }
        let fs = self.user_hidden || foreground_is_fullscreen(self.hwnd, &self.mon);
        if fs && self.visible {
            self.abort_prompt();
            self.abort_menu();
            let _ = ShowWindow(self.hwnd, SW_HIDE);
            self.visible = false;
            if let Some(b) = &self.backdrop {
                b.set_paused(true);
            }
            self.update_tick_timer();
        } else if !fs && !self.visible {
            self.show();
            let all = self.all_mask();
            self.redraw(all, false);
        } else if self.visible {
            self.guard_n += 1;
            if self.guard_n % 8 == 0 && !self.ui.edit && self.ui.prompt.is_none() && self.ui.menu.is_none() {
                place_above_desktop(self.hwnd); // explorer restarts / shell re-stacks
            }
        }
    }

    /// `widgetsDim`: monochrome cards while an app window is in front (auto) / always / never; off while editing.
    unsafe fn update_dim(&mut self) {
        let want = !self.ui.edit
            && match self.settings.dim {
                layout::Dim::Always => true,
                layout::Dim::Never => false,
                layout::Dim::Auto => self.app_front,
            };
        if want != self.scene.dimmed() {
            self.scene.set_dim(want);
            if self.visible {
                let d = RECT { left: self.win.left, top: self.win.top, right: self.win.right, bottom: self.win.bottom };
                self.frame(d);
            }
        }
    }

    unsafe fn report(&self) {
        let secs = self.t0.elapsed().as_secs_f64();
        let (acq, copied) = self.bd().stats();
        let mut c = PROCESS_MEMORY_COUNTERS_EX::default();
        c.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
        let _ = GetProcessMemoryInfo(GetCurrentProcess(), &mut c as *mut _ as *mut PROCESS_MEMORY_COUNTERS, c.cb);
        let cpu = (cpu_time_100ns() - self.cpu0) as f64 / 1e7;
        let steady = self.cpu_mark.map(|(t, c0)| (cpu_time_100ns() - c0) as f64 / 1e7 / t.elapsed().as_secs_f64() * 100.0);
        let frames = self.st.backdrop_batches + self.st.clock_frames;
        println!("backdrop={}", self.bd().name());
        println!("seconds={secs:.1}");
        println!("working_set_mb={:.1}", c.WorkingSetSize as f64 / 1048576.0);
        println!("private_mb={:.1}", c.PrivateUsage as f64 / 1048576.0);
        println!("cpu_percent_of_one_core={:.2}", cpu / secs * 100.0);
        if let Some(s) = steady {
            println!("cpu_percent_steady_state={s:.2}");
        }
        println!("frames_rendered={frames}");
        println!("  backdrop_batches={} card_redraws={} clock_frames={}", self.st.backdrop_batches, self.st.card_redraws, self.st.clock_frames);
        println!("duplication_frames={acq}");
        println!("duplication_frames_with_gpu_copy={copied}");
        println!("capture_rate_hz_at_exit={}", self.bd().rate_hz());
        println!("achieved_backdrop_fps={:.1}", self.st.backdrop_batches as f64 / secs);
        println!("achieved_clock_fps={:.1}", self.st.clock_frames as f64 / secs);
        println!("render_cpu_ms_avg={:.3}", self.st.render_ms / frames.max(1) as f64);
        println!("present_cpu_ms_avg={:.3}", self.st.present_ms / frames.max(1) as f64);
        println!("luminance_probes={}", self.st.lum_reads);
    }
}

// ---------------------------------------------------------------- accent from the wallpaper, layout profiles

/// The accent for `mode` (`None` = the default blue) and the wallpaper file it was taken from.
fn resolve_accent(mode: AccentMode) -> (Option<[u8; 3]>, Option<(String, Option<SystemTime>)>) {
    if mode != AccentMode::Wallpaper {
        return (None, None);
    }
    let key = crate::accent::wallpaper_key();
    (key.as_ref().and_then(|k| crate::accent::from_file(&k.0)), key)
}

impl App {
    /// `accentScope`: "everywhere" tints all content of every card with the accent (Apple's Tinted style); "main" draws it as the widgets do.
    unsafe fn apply_scope(&mut self) {
        crate::widget::set_tint(self.settings.accent_scope == AccentScope::All);
        self.scene.invalidate_all();
        let now = self.now();
        for i in 0..self.scene.len() {
            let _ = self.scene.render_card(&self.gfx, i, &now);
        }
        let d = self.all_dirty();
        self.frame(d);
    }

    /// `accentMode`: take the accent from the wallpaper (again when the wallpaper file changed; `force` = the mode itself changed) or go back to blue,
    /// and record every card again when the colour is different. Runs on the 2 s config poll: one SystemParametersInfo call while nothing changed.
    unsafe fn refresh_accent(&mut self, force: bool) {
        if !self.visible && !force {
            return;
        }
        let rgb = if self.settings.accent == AccentMode::Wallpaper {
            let key = crate::accent::wallpaper_key();
            // what is on the screen: the captured desktop (a dynamic / slideshow wallpaper included); the wallpaper file only without a capture
            let live = if self.settings.live && (force || self.accent_last.elapsed() >= Duration::from_secs(8)) {
                // the throttle starts only when a capture really existed: a failed read is retried at the next poll
                self.scene.backdrop_colors(&self.gfx).ok().map(|px| {
                    self.accent_last = Instant::now();
                    crate::accent::pick_or_neutral(&px)
                })
            } else {
                None
            };
            let rgb = match live {
                Some(p) => match (p, self.accent_cur) {
                    // close to what is shown already: keep it (a video wallpaper must not make the accent flicker)
                    (Some(n), Some(o)) if !force && n.iter().zip(o).map(|(a, b)| (*a as i32 - b as i32).abs()).sum::<i32>() < 60 => Some(o),
                    (p, _) => p,
                },
                None if self.settings.live => {
                    if !force {
                        return;
                    }
                    self.accent_cur
                }
                None => {
                    if !force && key == self.accent_key {
                        return;
                    }
                    key.as_ref().and_then(|k| crate::accent::from_file(&k.0))
                }
            };
            self.accent_key = key;
            rgb
        } else {
            self.accent_key = None;
            None
        };
        self.accent_cur = rgb;
        if self.settings.live {
            if let Some(c) = rgb {
                crate::accent::save_cache(c); // the next start begins with this colour instead of the wallpaper file's
            }
        }
        if crate::widget::set_accent(rgb) {
            self.scene.invalidate_all();
            let now = self.now();
            for i in 0..self.scene.len() {
                let _ = self.scene.render_card(&self.gfx, i, &now);
            }
            let d = self.all_dirty();
            self.frame(d);
        }
    }

    /// A message for the user: a toast while the panels are up (the toast lives on the whole-monitor canvas), else the log.
    unsafe fn notify(&mut self, msg: String) {
        if self.ui.edit {
            self.show_toast(msg);
        } else {
            eprintln!("mswidgets: {msg}");
        }
    }

    /// Tray "Layouts" > profile `i` (index into the sorted list the menu showed): the current desk goes back into the active profile, the chosen one is loaded.
    unsafe fn switch_profile(&mut self, i: usize) {
        let names = layout::profile_names(&layout::read_config());
        let Some(name) = names.get(i).cloned() else { return };
        if self.drag.is_some() {
            self.end_drag();
        }
        let stash = self.ui.lang.pick("Без имени", "Unsaved");
        if let Err(e) = layout::profile_op(|r| layout::profile_switch(r, &name, stash)) {
            return self.notify(format!("{}: {e}", self.ui.lang.pick("Макет не загружен", "Layout not loaded")));
        }
        self.note_own_write();
        // the new desk: items + look from the file, every card replaced (a card of the same id may have another size / cfg in the new layout)
        let root = layout::read_config();
        self.items = layout::items_for(&root, self.slot.as_deref()); // the active monitor's layout
        self.look_cfg = layout::look_from_value(&root);
        self.ui.look = crate::glass::apply_look(&self.look_base, &self.look_cfg);
        self.clear_desk();
        self.apply_look_now(); // the glass parameters first: the new cards are built with them
        self.rebuild(true); // also the card scale of the new look: a new grid, the cards come back from `items`
        self.pop_all_in();
    }

    /// Tray "Save current layout as...": the prompt runs on its own thread (it has a message loop); the name comes back as `WM_PROFILE_NAME`.
    unsafe fn ask_profile_name(&mut self) {
        let (lang, hwnd) = (self.ui.lang, self.hwnd.0 as isize);
        let _ = std::thread::Builder::new().name("profile-name".into()).spawn(move || {
            let mut o = crate::input::PromptOpts::new(lang.pick("Название макета", "Layout name"), "", false);
            (o.placeholder, o.ok_label, o.lang) = (lang.pick("Например: Работа", "e.g. Work").to_string(), lang.pick("Сохранить", "Save").to_string(), lang);
            if let Some(name) = crate::input::prompt_ex(o) {
                if let Ok(mut g) = PROFILE_NAME.lock() {
                    *g = Some(name);
                    let _ = PostMessageW(HWND(hwnd as *mut _), WM_PROFILE_NAME, WPARAM(0), LPARAM(0));
                }
            }
        });
    }

    /// The prompt answered: the desk (cards + look) is saved as that profile and becomes the active one.
    unsafe fn profile_named(&mut self) {
        let Some(name) = PROFILE_NAME.lock().ok().and_then(|mut g| g.take()) else { return };
        let name: String = name.trim().chars().take(PROFILE_NAME_MAX).collect();
        if name.is_empty() {
            return;
        }
        match layout::profile_op(|r| layout::profile_save(r, &name)) {
            Ok(()) => {
                self.note_own_write();
                let msg = format!("{} «{name}»", self.ui.lang.pick("Макет сохранён:", "Layout saved:"));
                self.notify(msg);
            }
            Err(e) => self.notify(format!("{}: {e}", self.ui.lang.pick("Макет не сохранён", "Layout not saved"))),
        }
    }

    /// Tray "Delete current layout": the active profile is forgotten, the desk stays as it is.
    unsafe fn delete_profile(&mut self) {
        let mut gone = String::new();
        match layout::profile_op(|r| layout::profile_delete_active(r).map(|n| gone = n)) {
            Ok(()) => {
                self.note_own_write();
                let msg = format!("{} «{gone}»", self.ui.lang.pick("Макет удалён:", "Layout deleted:"));
                self.notify(msg);
            }
            Err(e) => self.notify(format!("{}: {e}", self.ui.lang.pick("Макет не удалён", "Layout not deleted"))),
        }
    }
}

// ---------------------------------------------------------------- the text prompt on the canvas

/// Put `s` on the clipboard as CF_UNICODETEXT (CR LF line breaks).
fn set_clipboard_text(hwnd: HWND, s: &str) {
    use windows::Win32::System::DataExchange::{EmptyClipboard, SetClipboardData};
    use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
    let t = s.replace("\r\n", "\n").replace('\n', "\r\n");
    let w: Vec<u16> = t.encode_utf16().chain(Some(0)).collect();
    unsafe {
        if OpenClipboard(hwnd).is_err() {
            return;
        }
        let _ = EmptyClipboard();
        if let Ok(h) = GlobalAlloc(GMEM_MOVEABLE, w.len() * 2) {
            let p = GlobalLock(h) as *mut u16;
            if !p.is_null() {
                std::ptr::copy_nonoverlapping(w.as_ptr(), p, w.len());
                let _ = GlobalUnlock(h);
                let _ = SetClipboardData(13, HANDLE(h.0)); // the clipboard owns the memory now
            }
        }
        let _ = CloseClipboard();
    }
}

impl App {
    /// A widget thread asked for a text prompt (`input::prompt_ex`): show it on the canvas, next to the cursor. Refused (the worker runs the
    /// Win32 popup) while the layer is hidden or a card is being dragged.
    unsafe fn on_prompt_request(&mut self) {
        let Some(req) = input::take_request() else { return };
        self.abort_menu(); // a prompt and a menu are never up together
        if self.ui.prompt.is_some() {
            self.finish_prompt(); // the previous one is still fading out
        }
        if !self.visible || self.drag.is_some() {
            let _ = req.reply.send(input::Reply::Fallback);
            return;
        }
        let now = self.now();
        self.leave(&now);
        let s = self.s;
        let work = Rect::new((self.work.left - self.mon.left) as f32 / s, (self.work.top - self.mon.top) as f32 / s, (self.work.right - self.work.left) as f32 / s, (self.work.bottom - self.work.top) as f32 / s);
        // the cursor (screen px) on this canvas in logical px; from another monitor the panel opens in the upper middle of the work area
        let (ax, ay) = req.anchor;
        let on = ax >= self.mon.left && ax < self.mon.right && ay >= self.mon.top && ay < self.mon.bottom;
        let anchor = if on { ((ax - self.mon.left) as f32 / s, (ay - self.mon.top) as f32 / s) } else { (work.cx(), work.y + work.h * 0.35) };
        // the keyboard: the layer becomes activatable and takes the foreground (as in edit mode); remember who had it
        self.modal_begin();
        self.boost_capture();
        self.reduced = reduced_motion();
        self.ui.open_prompt(&self.scene, &self.gfx, &req.opts, anchor, work);
        self.prompt_reply = Some(req.reply);
        (self.prompt_was_fg, self.prompt_t0, self.prompt_last, self.prompt_scrim, self.prompt_hi, self.prompt_click) = (false, Instant::now(), Instant::now(), 0.0, None, None);
        let full = RECT { left: 0, top: 0, right: self.scene.w, bottom: self.scene.h };
        self.set_window(full); // live mode: the layer covers the whole monitor, like for the toast
        self.ui.sync_panels(&mut self.scene, &self.gfx);
        self.sync_regions();
        force_foreground(self.hwnd);
        let _ = SetFocus(self.hwnd);
        self.prompt_period = 16;
        SetTimer(self.hwnd, T_PROMPT, 16, None);
        self.prompt_tick();
    }

    /// Open / close animation (16 ms), then the caret blink (500 ms); the veil over the desktop fades in 4 steps (each one a full frame).
    unsafe fn prompt_tick(&mut self) {
        let t = Instant::now();
        let dt = t.saturating_duration_since(self.prompt_last).as_secs_f32().clamp(0.0, 0.1);
        self.prompt_last = t;
        let reduced = self.reduced;
        let Some(p) = self.ui.prompt.as_mut() else {
            let _ = KillTimer(self.hwnd, T_PROMPT);
            self.prompt_period = 0;
            return;
        };
        let closing = p.closing;
        let busy = if reduced {
            p.t = if closing { 0.0 } else { 1.0 };
            false
        } else {
            p.step(dt)
        };
        let scrim = p.scrim();
        let mut d = self.ui.prompt_dirty(&self.scene);
        if (scrim - self.prompt_scrim).abs() > 1e-4 {
            self.prompt_scrim = scrim;
            d = Some(RECT { left: 0, top: 0, right: self.scene.w, bottom: self.scene.h });
        }
        if closing && !busy {
            return self.finish_prompt();
        }
        let period = if busy { 16 } else { 500 };
        if period != self.prompt_period {
            self.prompt_period = period;
            SetTimer(self.hwnd, T_PROMPT, period, None);
            self.ui.caret_on = true;
        } else if !busy {
            self.ui.caret_on = !self.ui.caret_on;
        }
        if let Some(d) = d {
            self.frame(d);
        }
    }

    /// The user answered (or cancelled): the worker gets the answer at once, the panel fades out.
    unsafe fn end_prompt(&mut self, answer: Option<String>) {
        if let Some(tx) = self.prompt_reply.take() {
            let _ = tx.send(input::Reply::Answer(answer));
        }
        let Some(p) = self.ui.prompt.as_mut() else { return };
        if p.closing {
            return;
        }
        (p.closing, p.selecting, p.hover, p.press) = (true, false, None, None);
        let _ = ReleaseCapture();
        self.prompt_last = Instant::now();
        self.prompt_period = 16;
        SetTimer(self.hwnd, T_PROMPT, 16, None);
        self.prompt_tick();
    }

    /// The panel is gone: no veil, the layer goes back to click-through under the app windows and the foreground to whoever had it.
    unsafe fn finish_prompt(&mut self) {
        self.prompt_reply = None; // a prompt that was never answered counts as a cancel (the worker sees the dropped channel)
        let _ = KillTimer(self.hwnd, T_PROMPT);
        self.prompt_period = 0;
        if self.ui.prompt.take().is_none() {
            return;
        }
        self.prompt_scrim = 0.0;
        self.ui.sync_panels(&mut self.scene, &self.gfx);
        self.sync_regions();
        let full = RECT { left: 0, top: 0, right: self.scene.w, bottom: self.scene.h };
        self.frame(full);
        self.modal_end();
    }

    /// The keyboard hand-off of a modal (prompt, menu) on the live desk: remember who has the foreground, the layer becomes activatable,
    /// stops being click-through and is raised (as in edit mode, which already has all of that).
    unsafe fn modal_begin(&mut self) {
        let fg = GetForegroundWindow();
        if fg != self.hwnd {
            self.prompt_fg = fg;
        }
        if !self.ui.edit {
            let ex = GetWindowLongPtrW(self.hwnd, GWL_EXSTYLE);
            SetWindowLongPtrW(self.hwnd, GWL_EXSTYLE, ex & !(WS_EX_NOACTIVATE.0 as isize) & !(WS_EX_TRANSPARENT.0 as isize));
            self.interactive = true;
            let _ = SetWindowPos(self.hwnd, HWND_TOP, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
        }
    }

    /// The modal is gone: the layer goes back to click-through under the app windows, the foreground to whoever had it, the window to the cards.
    unsafe fn modal_end(&mut self) {
        if !self.ui.edit {
            let ex = GetWindowLongPtrW(self.hwnd, GWL_EXSTYLE);
            SetWindowLongPtrW(self.hwnd, GWL_EXSTYLE, ex | WS_EX_NOACTIVATE.0 as isize | WS_EX_TRANSPARENT.0 as isize);
            self.interactive = false;
            if GetForegroundWindow() == self.hwnd && !self.prompt_fg.0.is_null() && IsWindow(self.prompt_fg).as_bool() {
                let _ = SetForegroundWindow(self.prompt_fg);
            }
            place_above_desktop(self.hwnd);
            self.fit_window();
            let w = self.win;
            self.frame(w);
        }
    }

    /// Teardown (fullscreen guard, monitor change): answer "cancelled" and drop the panel at once, no animation.
    unsafe fn abort_prompt(&mut self) {
        if let Some(tx) = self.prompt_reply.take() {
            let _ = tx.send(input::Reply::Answer(None));
        }
        if self.ui.prompt.is_some() {
            self.finish_prompt();
        }
    }

    unsafe fn redraw_prompt(&mut self) {
        self.ui.caret_on = true;
        if let Some(d) = self.ui.prompt_dirty(&self.scene) {
            self.frame(d);
        }
    }

    unsafe fn apply_act(&mut self, a: Act) {
        match a {
            Act::None => {}
            Act::Redraw => self.redraw_prompt(),
            Act::Clip(t) => {
                set_clipboard_text(self.hwnd, &t);
                self.redraw_prompt();
            }
            Act::Done(r) => self.end_prompt(r),
        }
    }

    fn prompt_field_under_cursor(&self) -> bool {
        let Some(p) = self.ui.prompt.as_ref().filter(|p| !p.closing) else { return false };
        let mut c = POINT::default();
        unsafe {
            if GetCursorPos(&mut c).is_err() {
                return false;
            }
        }
        p.hit((c.x - self.mon.left) as f32 / self.s, (c.y - self.mon.top) as f32 / self.s) == crate::prompt::PHit::Field
    }

    /// Mouse while the prompt is up: nothing behind it is reachable; a click outside the panel cancels.
    unsafe fn prompt_mouse(&mut self, msg: u32, wp: WPARAM, x: i32, y: i32) {
        if self.ui.prompt.as_ref().is_some_and(|p| p.closing) {
            return;
        }
        let (lx, ly) = (x as f32 / self.s, y as f32 / self.s);
        let m = Mods { ctrl: GetKeyState(VK_CONTROL.0 as i32) < 0, shift: GetKeyState(VK_SHIFT.0 as i32) < 0 };
        let act = match msg {
            WM_MOUSEMOVE => self.ui.prompt_mouse(&self.scene, &self.gfx, 0, lx, ly, m, false),
            WM_LBUTTONDOWN => {
                let t = Instant::now();
                let dbl = self.prompt_click.is_some_and(|(t0, x0, y0)| t.duration_since(t0).as_millis() as u32 <= GetDoubleClickTime() && (x - x0).abs() <= 4 && (y - y0).abs() <= 4);
                self.prompt_click = if dbl { None } else { Some((t, x, y)) };
                SetCapture(self.hwnd);
                self.ui.prompt_mouse(&self.scene, &self.gfx, 1, lx, ly, m, dbl)
            }
            WM_LBUTTONUP => {
                let _ = ReleaseCapture();
                self.ui.prompt_mouse(&self.scene, &self.gfx, 2, lx, ly, m, false)
            }
            WM_MOUSEWHEEL => {
                let delta = ((wp.0 >> 16) as u16 as i16) as f32 / 120.0;
                if self.ui.prompt_wheel(&self.scene, &self.gfx, delta) {
                    Act::Redraw
                } else {
                    Act::None
                }
            }
            _ => Act::None,
        };
        self.apply_act(act);
    }

    /// Keys while the prompt is up (the layer is the foreground window, so WM_KEYDOWN / WM_CHAR arrive here).
    unsafe fn prompt_key(&mut self, msg: u32, wp: usize) {
        if self.ui.prompt.as_ref().is_some_and(|p| p.closing) {
            return;
        }
        let m = Mods { ctrl: GetKeyState(VK_CONTROL.0 as i32) < 0, shift: GetKeyState(VK_SHIFT.0 as i32) < 0 };
        let key = match msg {
            WM_SYSKEYDOWN => return, // Alt+Tab and friends belong to Windows
            WM_KEYDOWN => match VIRTUAL_KEY(wp as u16) {
                VK_ESCAPE => Key::Esc,
                VK_RETURN => Key::Enter,
                VK_TAB => Key::Tab,
                VK_BACK => Key::Back,
                VK_DELETE => Key::Delete,
                VK_LEFT => Key::Left,
                VK_RIGHT => Key::Right,
                VK_UP => Key::Up,
                VK_DOWN => Key::Down,
                VK_HOME => Key::Home,
                VK_END => Key::End,
                VK_A if m.ctrl => Key::SelectAll,
                VK_C if m.ctrl => Key::Copy,
                VK_X if m.ctrl => Key::Cut,
                VK_V if m.ctrl => match clipboard_text(self.hwnd) {
                    Some(t) => Key::Paste(t),
                    None => return,
                },
                VK_SPACE if self.ui.prompt.as_ref().is_some_and(|p| p.focus != crate::prompt::Focus::Field) => Key::Char(' '),
                _ => return,
            },
            WM_CHAR => {
                // UTF-16 units: a high surrogate waits for its low one (emoji)
                let u = wp as u32;
                let c = match u {
                    0xD800..=0xDBFF => {
                        self.prompt_hi = Some(u as u16);
                        return;
                    }
                    0xDC00..=0xDFFF => {
                        let Some(hi) = self.prompt_hi.take() else { return };
                        match char::decode_utf16([hi, u as u16]).next() {
                            Some(Ok(c)) => c,
                            _ => return,
                        }
                    }
                    _ => {
                        self.prompt_hi = None;
                        let Some(c) = char::from_u32(u) else { return };
                        c
                    }
                };
                if c.is_control() || (c == ' ' && self.ui.prompt.as_ref().is_some_and(|p| p.focus != crate::prompt::Focus::Field)) {
                    return; // control codes come as key presses; the space on a button was handled at WM_KEYDOWN
                }
                Key::Char(c)
            }
            _ => return,
        };
        let act = self.ui.prompt_key(&self.scene, &self.gfx, key, m);
        self.apply_act(act);
    }
}

// ---------------------------------------------------------------- the card context menu on the canvas

impl App {
    /// Show the menu of a card on the canvas at the cursor (glass panels, see menu.rs). False = it cannot be shown now and the caller runs
    /// the native popup instead: `MSWIDGETS_NATIVE_MENU=1`, the layer is hidden, a card is dragged, the prompt or the settings panel is up.
    unsafe fn show_menu(&mut self, req: &MenuReq) -> bool {
        if std::env::var_os("MSWIDGETS_NATIVE_MENU").is_some() || !self.visible || self.drag.is_some() || self.ui.prompt.is_some() || self.ui.settings {
            return false;
        }
        self.abort_menu(); // the previous one, if any
        let now = self.now();
        self.leave(&now);
        let s = self.s;
        let work = Rect::new((self.work.left - self.mon.left) as f32 / s, (self.work.top - self.mon.top) as f32 / s, (self.work.right - self.work.left) as f32 / s, (self.work.bottom - self.work.top) as f32 / s);
        // the cursor (screen px) on this canvas in logical px
        let on = req.x >= self.mon.left && req.x < self.mon.right && req.y >= self.mon.top && req.y < self.mon.bottom;
        let anchor = if on { ((req.x - self.mon.left) as f32 / s, (req.y - self.mon.top) as f32 / s) } else { (work.cx(), work.cy()) };
        // the keyboard (Esc, arrows): the same hand-off as the prompt; a native menu needed the foreground too
        self.modal_begin();
        self.boost_capture();
        self.reduced = reduced_motion();
        self.ui.open_menu(&self.scene, &self.gfx, menu::card_menu(req.lang, req.editing, &req.sizes, req.cur), anchor, work);
        self.menu_uid = req.uid;
        (self.prompt_was_fg, self.prompt_t0, self.menu_last, self.menu_shown) = (false, Instant::now(), Instant::now(), None);
        let full = RECT { left: 0, top: 0, right: self.scene.w, bottom: self.scene.h };
        self.set_window(full); // live mode: the layer covers the whole monitor, so a click outside lands on it (the faint veil makes it opaque to hits)
        self.ui.sync_panels(&mut self.scene, &self.gfx);
        self.sync_regions();
        force_foreground(self.hwnd);
        let _ = SetFocus(self.hwnd);
        self.menu_shown = self.ui.menu_dirty(&self.scene);
        self.frame(full);
        self.menu_period = 16;
        SetTimer(self.hwnd, T_MENU, 16, None);
        true
    }

    /// Open / close animation and the hover delay of the submenu (16 ms while something moves, no timer otherwise).
    unsafe fn menu_tick(&mut self) {
        let t = Instant::now();
        let dt = t.saturating_duration_since(self.menu_last).as_secs_f32().clamp(0.0, 0.1);
        self.menu_last = t;
        let reduced = self.reduced;
        let Some(m) = self.ui.menu.as_mut() else {
            let _ = KillTimer(self.hwnd, T_MENU);
            self.menu_period = 0;
            return;
        };
        let closing = m.closing;
        let (busy, layout) = m.step(dt, reduced);
        if closing && !busy {
            return self.finish_menu();
        }
        if layout {
            self.ui.sync_panels(&mut self.scene, &self.gfx);
            self.sync_regions();
        }
        if !busy {
            let _ = KillTimer(self.hwnd, T_MENU);
            self.menu_period = 0;
        }
        self.menu_frame();
    }

    /// Compose the menu's rectangle: where it is now and where it was drawn last (a closed submenu leaves its place).
    unsafe fn menu_frame(&mut self) {
        let now = self.ui.menu_dirty(&self.scene);
        let d = match (now, self.menu_shown) {
            (Some(a), Some(b)) => Some(union(a, b)),
            (a, b) => a.or(b),
        };
        self.menu_shown = now;
        if let Some(d) = d {
            self.frame(d);
        }
    }

    /// Start the timer when it is off (a hover that waits for its delay needs it).
    unsafe fn menu_kick(&mut self) {
        if self.menu_period == 0 {
            self.menu_last = Instant::now();
            self.menu_period = 16;
            SetTimer(self.hwnd, T_MENU, 16, None);
        }
    }

    unsafe fn menu_act(&mut self, act: MAct) {
        match act {
            MAct::None => {}
            MAct::Redraw | MAct::Layout => {
                if act == MAct::Layout {
                    self.ui.sync_panels(&mut self.scene, &self.gfx);
                    self.sync_regions();
                }
                self.menu_frame();
                self.menu_kick();
            }
            MAct::Close => self.close_menu(),
            MAct::Pick(id) => self.pick_menu(id),
        }
    }

    /// The choice: "Edit widgets…" switches the mode (the menu is dropped at once, edit mode takes the foreground itself), the others fade the
    /// menu out while they run.
    unsafe fn pick_menu(&mut self, id: usize) {
        let uid = self.menu_uid;
        if id == CMD_EDIT {
            self.abort_menu();
            if !self.ui.edit && !self.prompt_fg.0.is_null() && GetForegroundWindow() == self.hwnd {
                self.prev_fg = self.prompt_fg; // the foreground has not moved back yet: edit mode must return it to the same window
            }
            if self.editing {
                self.leave_edit();
            } else {
                self.enter_edit();
            }
            return;
        }
        self.close_menu();
        match id {
            c if (CMD_SIZE..CMD_SIZE + 3).contains(&c) => self.change_size(uid, Size::ALL[c - CMD_SIZE]),
            CMD_DELETE => self.remove_card(uid),
            _ => {}
        }
    }

    /// Fade the menu out (Esc, a click outside, a choice, another window in front).
    unsafe fn close_menu(&mut self) {
        let Some(m) = self.ui.menu.as_mut() else { return };
        if m.closing {
            return;
        }
        (m.closing, m.press) = (true, None);
        self.menu_last = Instant::now();
        self.menu_period = 16;
        SetTimer(self.hwnd, T_MENU, 16, None);
        self.menu_tick();
    }

    /// The menu is gone: no veil, the layer goes back to click-through under the app windows and the foreground to whoever had it.
    unsafe fn finish_menu(&mut self) {
        let _ = KillTimer(self.hwnd, T_MENU);
        self.menu_period = 0;
        if self.ui.menu.take().is_none() {
            return;
        }
        self.menu_shown = None;
        self.ui.sync_panels(&mut self.scene, &self.gfx);
        self.sync_regions();
        let full = RECT { left: 0, top: 0, right: self.scene.w, bottom: self.scene.h };
        self.frame(full);
        self.modal_end();
    }

    /// Teardown (edit mode toggle, fullscreen guard, monitor change, the card is gone): drop the menu at once, no animation.
    unsafe fn abort_menu(&mut self) {
        if self.ui.menu.is_some() {
            self.finish_menu();
        }
    }

    /// Mouse while the menu is up (monitor px): nothing behind it is reachable; a click outside the panels closes it.
    unsafe fn menu_mouse(&mut self, msg: u32, x: i32, y: i32) {
        let Some(m) = self.ui.menu.as_mut().filter(|m| !m.closing) else { return };
        let (lx, ly) = (x as f32 / self.s, y as f32 / self.s);
        let act = match msg {
            WM_MOUSEMOVE => m.mouse_move(lx, ly),
            WM_LBUTTONDOWN | WM_RBUTTONDOWN => m.mouse_down(lx, ly),
            WM_LBUTTONUP | WM_RBUTTONUP => m.mouse_up(lx, ly),
            _ => MAct::None, // the wheel does nothing
        };
        self.menu_act(act);
    }

    /// Keys while the menu is up (the layer is the foreground window, so WM_KEYDOWN arrives here).
    unsafe fn menu_key(&mut self, msg: u32, wp: usize) {
        if msg != WM_KEYDOWN {
            return;
        }
        let Some(m) = self.ui.menu.as_mut().filter(|m| !m.closing) else { return };
        let key = match VIRTUAL_KEY(wp as u16) {
            VK_ESCAPE => Key::Esc,
            VK_RETURN => Key::Enter,
            VK_SPACE => Key::Char(' '),
            VK_UP => Key::Up,
            VK_DOWN => Key::Down,
            VK_LEFT => Key::Left,
            VK_RIGHT => Key::Right,
            VK_HOME => Key::Home,
            VK_END => Key::End,
            _ => return,
        };
        let act = m.key(key);
        self.menu_act(act);
    }
}

fn same_rect(a: &RECT, b: &RECT) -> bool {
    (a.left, a.top, a.right, a.bottom) == (b.left, b.top, b.right, b.bottom)
}

fn step_toward(v: f32, target: f32, d: f32) -> f32 {
    if v < target {
        (v + d).min(target)
    } else {
        (v - d).max(target)
    }
}

fn cpu_time_100ns() -> u64 {
    unsafe {
        let (mut c, mut e, mut k, mut u) = (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
        let _ = GetProcessTimes(GetCurrentProcess(), &mut c, &mut e, &mut k, &mut u);
        let f = |t: FILETIME| ((t.dwHighDateTime as u64) << 32) | t.dwLowDateTime as u64;
        f(k) + f(u)
    }
}

/// Directly above the desktop host (Progman / the WorkerW owning SHELLDLL_DefView), below every app window.
unsafe fn place_above_desktop(me: HWND) {
    let has_view = |h: HWND| FindWindowExW(h, HWND::default(), w!("SHELLDLL_DefView"), PCWSTR::null()).is_ok();
    let mut host = FindWindowW(w!("Progman"), PCWSTR::null()).ok().filter(|&h| has_view(h));
    if host.is_none() {
        let mut prev = HWND::default();
        while let Ok(ww) = FindWindowExW(HWND::default(), prev, w!("WorkerW"), PCWSTR::null()) {
            if has_view(ww) {
                host = Some(ww);
                break;
            }
            prev = ww;
        }
    }
    if let Some(h) = host {
        let _ = SetWindowPos(me, h, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
    }
}

/// An ordinary app window is in front (not the desktop, the taskbar, the shell's own surfaces or this process): what "auto" dim reacts to.
unsafe fn app_in_front(me: HWND) -> bool {
    let hwnd = GetForegroundWindow();
    if hwnd.0.is_null() || hwnd == me {
        return false;
    }
    let mut pid = 0u32;
    GetWindowThreadProcessId(hwnd, Some(&mut pid));
    if pid == windows::Win32::System::Threading::GetCurrentProcessId() {
        return false;
    }
    let mut buf = [0u16; 128];
    let len = GetClassNameW(hwnd, &mut buf);
    let class = String::from_utf16_lossy(&buf[..len.max(0) as usize]);
    !matches!(
        class.as_str(),
        "Progman" | "WorkerW" | "Shell_TrayWnd" | "Shell_SecondaryTrayWnd" | "Windows.UI.Core.CoreWindow" | "XamlExplorerHostIslandWindow" | "TopLevelWindowForOverflowXamlIsland" | "NotifyIconOverflowWindow" | "ForegroundStaging" | "#32768"
    )
}

/// Same logic as `foreground_is_fullscreen` in src-tauri/src/main.rs (the own window does not count). Only a fullscreen window on `ours`, the
/// monitor the desk lives on, counts: a game on the other monitor leaves the widgets alone.
unsafe fn foreground_is_fullscreen(me: HWND, ours: &RECT) -> bool {
    let hwnd = GetForegroundWindow();
    if hwnd.0.is_null() || hwnd == me {
        return false;
    }
    let mut buf = [0u16; 256];
    let len = GetClassNameW(hwnd, &mut buf);
    let class = String::from_utf16_lossy(&buf[..len.max(0) as usize]);
    if matches!(class.as_str(), "Progman" | "WorkerW" | "Shell_TrayWnd" | "Shell_SecondaryTrayWnd") {
        return false;
    }
    let mut r = RECT::default();
    if GetWindowRect(hwnd, &mut r).is_err() {
        return false;
    }
    let hmon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
    let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
    if !GetMonitorInfoW(hmon, &mut mi).as_bool() {
        return false;
    }
    let m = mi.rcMonitor;
    if !same_rect(&m, ours) {
        return false;
    }
    r.left <= m.left && r.top <= m.top && r.right >= m.right && r.bottom >= m.bottom
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spring_settles_on_the_target_with_a_little_overshoot() {
        let mut s = Spring { x: 0.0, y: 0.0, vx: 0.0, vy: 0.0, tx: 400.0, ty: -40.0 };
        let (mut t, mut max_x, mut done) = (0.0f32, 0.0f32, false);
        while !done && t < 2.0 {
            done = s.step(0.016);
            t += 0.016;
            max_x = max_x.max(s.x);
        }
        assert!(done && (s.x - 400.0).abs() < 0.01 && (s.y + 40.0).abs() < 0.01);
        assert!((0.2..0.7).contains(&t), "settled after {t}s");
        assert!(max_x > 403.0 && max_x < 420.0, "overshoot {max_x}");
    }

    #[test]
    fn pop_in_and_out() {
        let t0 = Instant::now();
        let p = Pop { t0, out: false };
        let s0 = p.at(t0, false);
        assert!((s0.scale - 0.92).abs() < 1e-3 && s0.alpha == 0.0 && s0.blur > 0.99 && !s0.finished);
        // a start in the future (staggered start-up) shows the first frame
        assert_eq!(p.at(t0 - Duration::from_millis(5), false).alpha, 0.0);
        let s1 = p.at(t0 + Duration::from_secs(1), false);
        assert!((s1.scale - 1.0).abs() < 1e-3 && s1.alpha == 1.0 && s1.finished && s1.blur == 0.0);
        let o = Pop { t0, out: true };
        let e = o.at(t0 + Duration::from_millis(300), false);
        assert!(e.alpha == 0.0 && e.finished);
        let x = mat_xf(&s0, 0.0, 0.0);
        assert!(!x.is_id() && x.blur > 0.99 && x.sweep < 0.0);
        assert!(mat_xf(&s1, 0.0, 0.0).is_id());
    }

    /// Capture outputs are only used on the GPU the UI device lives on (shared textures); an unknown device GPU keeps the old behaviour.
    #[test]
    fn capture_output_must_share_the_gpu() {
        assert!(same_gpu(None, 7));
        assert!(same_gpu(Some(7), 7));
        assert!(!same_gpu(Some(7), (1 << 32) | 7));
        assert!(same_rect(&RECT { left: -1920, top: 0, right: 0, bottom: 1080 }, &RECT { left: -1920, top: 0, right: 0, bottom: 1080 }));
        assert!(!same_rect(&RECT { left: 0, top: 0, right: 1, bottom: 1 }, &RECT { left: 0, top: 0, right: 1, bottom: 2 }));
    }

    #[test]
    fn window_is_the_card_box_plus_margin() {
        let cards = [RECT { left: 100, top: 200, right: 270, bottom: 370 }, RECT { left: 300, top: 150, right: 470, bottom: 320 }];
        let w = window_rect(&cards, 48, 1920, 1080);
        assert_eq!((w.left, w.top, w.right, w.bottom), (52, 102, 518, 418));
        let edge = window_rect(&[RECT { left: 10, top: 10, right: 100, bottom: 100 }], 48, 1920, 1080);
        assert_eq!((edge.left, edge.top), (0, 0));
    }

    #[test]
    fn regions_reserve_gallery_and_toolbar_bits() {
        let r = region_list(&[RECT { left: 1, top: 2, right: 3, bottom: 4 }], Some(RECT { left: 5, top: 5, right: 9, bottom: 9 }), None);
        assert_eq!(r.len(), 64);
        assert_eq!(r[0].right, 3);
        assert_eq!(r[62].right, 9);
        assert_eq!(r[63].right, 0);
        assert_eq!(unpack(((300i32 << 16) | 200) as isize), (200, 300));
        assert_eq!(unpack(0xFFFF_FFFF_u32 as isize), (-1, -1));
    }
}
