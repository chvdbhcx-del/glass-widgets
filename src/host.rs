//! The one top-level layered window, timers, presenting, click-through toggling, fullscreen guard, `--bench` and everything about
//! arranging the desk: drag & drop with a spring settle, the context menu, edit mode (jiggle, "−", dim, settings strip), the gallery
//! ("Edit Widgets" with live previews and a search box) and persistence of the layout / settings.
//!
//! Idle behaviour: the UI thread sits in GetMessage (kernel wait, same as MsgWaitForMultipleObjects with no handles).
//! Wake-ups: WM_BACKDROP (only when the wallpaper pixels behind a card changed, <= --backdrop-fps), a widget schedule timer ONLY
//! while something animates, a 100 ms cursor poll (16 ms near a card), a 500 ms fullscreen / idle-pause guard, a 2 s config.json poll and a 30 s day
//! check. Edit mode adds a 33 ms jiggle timer (16 ms while a card moves) and a 200 ms gallery timer; none of them runs outside edit mode.
//! The capture, the backdrop redraws and the widget schedule sleep while the layer is hidden, the session is locked, the monitors are off or every
//! card is covered by other windows (autopause.rs); the first wake-up repaints everything.
//!
//! Coordinates: the scene canvas is the monitor (card px = monitor px); the layered window only covers `win`, the bounding box of the
//! cards, and grows to the whole monitor while a card is dragged / settling and in edit mode.
use crate::autopause::{self, Pause};
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
use crate::menu::{self, ClickPick, MAct, CMD_CARD_OPT, CMD_DELETE, CMD_EDIT, CMD_SIZE};
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
use windows::Win32::System::Power::RegisterPowerSettingNotification;
use windows::Win32::System::RemoteDesktop::{WTSRegisterSessionNotification, WTSUnRegisterSessionNotification, NOTIFY_FOR_THIS_SESSION};
use windows::Win32::System::StationsAndDesktops::{CloseDesktop, OpenInputDesktop, DESKTOP_ACCESS_FLAGS, DESKTOP_CONTROL_FLAGS};
use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX};
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentProcess, GetCurrentThreadId, GetProcessTimes};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, GetDpiForSystem, GetDpiForWindow, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

mod modal;
mod scene;
mod winutil;
mod mouse_keys;
mod cards_drag;
mod anim;
mod settings_edit;
mod desk;
mod pause;
mod profiles;
use winutil::*;

const WM_MOUSELEAVE: u32 = 0x02A3; // Win32_UI_Controls in the windows crate; not worth the feature
const WM_DPICHANGED: u32 = 0x02E0;
/// posted by the mouse handler: run the context menu of card `wparam` at screen point `lparam` (outside of the App borrow)
const WM_MENU: u32 = WM_APP + 3;
/// posted by `input::prompt_ex` (a worker thread): a text prompt waits in `input::take_request` to be shown on the canvas
const WM_PROMPT: u32 = WM_APP + 21;
/// posted by the "Save current layout as..." prompt thread when the user typed a name (the name waits in `PROFILE_NAME`)
const WM_PROFILE_NAME: u32 = WM_APP + 20;
/// posted by the scene-rule prompt thread (an exe name or a time range was typed; the text waits in `scene::SCENE_REQ`)
const WM_SCENE_TEXT: u32 = WM_APP + 23;
/// the longest layout name
const PROFILE_NAME_MAX: usize = 40;
/// posted by the "Click action > Custom…" prompt thread with a validated target (it waits in `CLICK_CUSTOM` with the card's uid)
const WM_CLICK_CUSTOM: u32 = WM_APP + 22;
static CLICK_CUSTOM: std::sync::Mutex<Option<(usize, String)>> = std::sync::Mutex::new(None);
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
    /// the "Click action" submenu (None = the widget has no card-body click)
    click: Option<menu::Item>,
    /// the widget's on/off option (checked = on), e.g. Quick Actions' "Keep brightness after sleep"
    opt: Option<menu::Item>,
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
    /// why the capture and the backdrop / tick driven redraws sleep (asleep while any reason is set), see autopause.rs
    pause: Pause,
    /// `MSWIDGETS_NO_AUTOPAUSE=1`: only the fullscreen / hidden reason is used
    autopause_off: bool,
    /// the input desktop could be opened at start (else the secure-desktop poll would pause for ever, it stays off)
    secure_poll: bool,
    /// when the occlusion test last ran
    last_occ: Instant,
    /// cards whose background job finished while asleep: they pick the result up on waking
    missed_jobs: Vec<usize>,
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
    /// scenes (scenes.rs): the rules, their state machine, the last evaluation, the foreground lookup cache (hwnd, exe), the last program seen in front
    /// (None = the desktop) and what the evaluation reads from config.json
    scene_rules: Vec<crate::scenes::Rule>,
    scenes: crate::scenes::Auto,
    scene_t: Instant,
    scene_fg: (isize, Option<String>),
    scene_prog: Option<String>,
    scene_cfg: scene::Cfg,
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
    if !o.no_config {
        layout::migrate_monitors(&monitors::enumerate()); // old device-name keys / a new primary monitor, before anything reads the layout
    }
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
    scene.set_high_contrast(&gfx, crate::a11y::high_contrast())?;
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
    ui.lang_pref = settings.lang;
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
        pause: Pause::default(),
        autopause_off: std::env::var_os("MSWIDGETS_NO_AUTOPAUSE").is_some_and(|v| v == "1"),
        secure_poll: input_desktop_open(),
        last_occ: Instant::now(),
        missed_jobs: Vec::new(),
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
        scene_rules: if o.no_config { Vec::new() } else { crate::scenes::rules_from_value(&layout::read_config()) },
        scenes: crate::scenes::Auto::default(),
        scene_t: Instant::now(),
        scene_fg: (0, None),
        scene_prog: None,
        scene_cfg: scene::Cfg::default(),
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
    if !app.autopause_off {
        // session lock / switch away and the monitors' power state wake us through window messages (the guard re-reads them as insurance)
        let _ = WTSRegisterSessionNotification(hwnd, NOTIFY_FOR_THIS_SESSION);
        if let Err(e) = RegisterPowerSettingNotification(HANDLE(hwnd.0), &autopause::GUID_CONSOLE_DISPLAY_STATE, DEVICE_NOTIFY_WINDOW_HANDLE) {
            eprintln!("mswidgets: display power notification refused ({e}); the monitors' sleep will not pause the widgets");
        }
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
        crate::a11y::WM_GETOBJECT => {
            // a screen reader / UI Automation client asks for the window: publish the cards, then hand out the providers
            with_app(|app| crate::a11y::publish(app.a11y_snapshot()));
            if let Ok(Some(r)) = catch_unwind(|| crate::a11y::get_object(hwnd, wp, lp)) {
                return r;
            }
            DefWindowProcW(hwnd, msg, wp, lp)
        }
        WM_TIMER | WM_BACKDROP | WM_HOTKEY | WM_WIDGET | update::WM_UPDATE | update::WM_UPDATE_DL | WM_DISPLAYCHANGE | WM_DPICHANGED | WM_SETTINGCHANGE => {
            with_app(|app| app.on_msg(msg, wp.0));
            LRESULT(0)
        }
        autopause::WM_WTSSESSION_CHANGE => {
            if let Some(away) = autopause::session_event(wp.0) {
                autopause::raise(autopause::LOCKED, away);
                with_app(|app| app.sync_pause());
            }
            LRESULT(0)
        }
        WM_POWERBROADCAST => {
            if wp.0 == autopause::PBT_POWERSETTINGCHANGE && lp.0 != 0 {
                // POWERBROADCAST_SETTING { GUID PowerSetting; u32 DataLength; u8 Data[] }
                let p = lp.0 as *const u8;
                if std::ptr::read_unaligned(p as *const windows::core::GUID) == autopause::GUID_CONSOLE_DISPLAY_STATE && std::ptr::read_unaligned(p.add(16) as *const u32) >= 4 {
                    autopause::raise(autopause::DISPLAY_OFF, autopause::display_off(std::ptr::read_unaligned(p.add(20) as *const u32)));
                    with_app(|app| app.sync_pause());
                }
            } else if let Some(suspend) = autopause::suspend_event(wp.0) {
                autopause::raise(autopause::SUSPEND, suspend);
                with_app(|app| app.sync_pause());
            }
            LRESULT(1) // TRUE: the request is granted
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
        WM_SCENE_TEXT => {
            with_app(|app| app.scene_text_named());
            LRESULT(0)
        }
        WM_CLICK_CUSTOM => {
            with_app(|app| app.click_custom_named());
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
            let _ = WTSUnRegisterSessionNotification(hwnd);
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
    if let (Some(it), Ok(csub)) = (&r.click, CreatePopupMenu()) {
        for e in &it.children {
            match e {
                menu::Entry::Sep => {
                    let _ = AppendMenuW(csub, MF_SEPARATOR, 0, PCWSTR::null());
                }
                menu::Entry::Item(k) => {
                    let l = wide(&k.label);
                    let _ = AppendMenuW(csub, MF_STRING | if k.checked { MF_CHECKED } else { MF_UNCHECKED }, k.id, PCWSTR(l.as_ptr()));
                }
            }
        }
        let l = wide(&it.label);
        let _ = AppendMenuW(menu, MF_POPUP, csub.0 as usize, PCWSTR(l.as_ptr())); // destroyed with `menu`
    }
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
                if mask != 0 && self.live() {
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
                self.apply_contrast();
                self.replace_monitor();
            }
            (_, T_DAY) => {
                if self.now().day_key() != self.last_day && self.live() {
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
        if !self.live() {
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
        if !self.live() {
            // asleep: the card picks the result up when the desk wakes (`wake`)
            if !self.missed_jobs.contains(&uid) {
                self.missed_jobs.push(uid);
            }
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
