//! The text prompt of the widgets (reminders, notes, shortcuts, timer duration...): `cx.prompt_text(...)` runs `prompt_ex` on a
//! worker thread (`cx.spawn`), so the widget layer (NOACTIVATE, click-through) never needs keyboard focus itself.
//!
//! Normally the prompt is ON THE HOST'S CANVAS: `prompt_ex` hands the request to the host window (`set_host` / `take_request`, a private
//! window message) and blocks the worker on a channel until the host answers; the host draws the glass panel with its own text field
//! (prompt.rs = the model, ui.rs = the drawing, host.rs = keys and mouse). The UI thread is never blocked.
//!
//! THIS FILE'S WIN32 POPUP IS NOW ONLY THE FALLBACK: it runs when no host is running (previews, `--bench`, tests, `--prompt-demo`), when the host
//! cannot show the prompt right now (the layer is hidden by the fullscreen guard, a card is being dragged) and with
//! `MSWIDGETS_PROMPT_WIN32=1`. It uses a real Win32 EDIT control: IME, selection, clipboard and accessibility come for free. Everything around
//! it (panel, rounded "well" with a focus ring, placeholder, buttons) is drawn by Direct2D into a premultiplied DIB that is BitBlt-ed into the
//! window. Enter = OK (multiline: Ctrl+Enter), Esc / click elsewhere = cancel, Tab walks field > OK > Cancel (the same semantics as the canvas
//! prompt). The panel is a plain opaque dark one: GDI pixels of the EDIT in an acrylic frame get alpha 0 and show up as a grey slab.
//! `MSWIDGETS_PROMPT_ACRYLIC=1` opts into the backdrop anyway.
use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Mutex;
use std::time::Duration;
use windows::core::{w, Interface, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Dwm::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::SystemInformation::GetTickCount64;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Controls::{SetWindowTheme, ShowScrollBar, EM_SETSEL, MARGINS};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, ReleaseCapture, SetCapture, SetFocus, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT, VK_CONTROL, VK_ESCAPE, VK_RETURN, VK_SHIFT, VK_SPACE, VK_TAB};
use windows::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::gfx::{col, pt, rf, rr};
use crate::widget::Lang;

static BUSY: AtomicBool = AtomicBool::new(false);
/// The host window and the private message that asks it to show a prompt on its canvas (None = no host: previews, `--bench`, tests).
static HOST: Mutex<Option<(isize, u32)>> = Mutex::new(None);
/// The request the host picks up with `take_request` (one at a time: `BUSY` sees to that).
static PENDING: Mutex<Option<Request>> = Mutex::new(None);

// ---------------------------------------------------------------- public API

/// Everything the prompt can show. `PromptOpts::new` fills the rest with defaults (language from config.json, button "Готово"/"Done").
#[derive(Clone)]
pub struct PromptOpts {
    pub title: String,
    pub initial: String,
    pub multiline: bool,
    /// shown dimmed while the field is empty ("" = none)
    pub placeholder: String,
    /// label of the primary button ("" = "Готово" / "Done")
    pub ok_label: String,
    pub lang: Lang,
    /// screen point to open next to (None = the mouse cursor)
    pub anchor: Option<(i32, i32)>,
}
impl PromptOpts {
    pub fn new(title: &str, initial: &str, multiline: bool) -> PromptOpts {
        PromptOpts { title: title.into(), initial: initial.into(), multiline, placeholder: String::new(), ok_label: String::new(), lang: crate::layout::lang(), anchor: None }
    }
}

/// A prompt the host is asked to show: what to show, the screen point to open next to and where the answer goes.
pub struct Request {
    pub opts: PromptOpts,
    pub anchor: (i32, i32),
    pub reply: mpsc::Sender<Reply>,
}

/// The host's answer to a `Request`.
pub enum Reply {
    /// the user's answer (None = cancelled / blank)
    Answer(Option<String>),
    /// the host cannot show it now: the worker runs the Win32 popup instead
    Fallback,
}

/// The host announces itself (its window and the message to post for a prompt) or leaves (`None`).
pub fn set_host(h: Option<(isize, u32)>) {
    if let Ok(mut g) = HOST.lock() {
        *g = h;
    }
}

/// The host's side of the message: the request that was posted.
pub fn take_request() -> Option<Request> {
    PENDING.lock().ok().and_then(|mut g| g.take())
}

/// Blocks until the user answers. `None` = cancelled / empty / another prompt is open / the window could not be created.
pub fn prompt(title: &str, initial: &str, multiline: bool) -> Option<String> {
    prompt_ex(PromptOpts::new(title, initial, multiline))
}

pub fn prompt_ex(o: PromptOpts) -> Option<String> {
    if BUSY.swap(true, Ordering::SeqCst) {
        return None;
    }
    let r = match via_host(&o) {
        Some(answer) => answer,
        None => unsafe { run(o) },
    };
    BUSY.store(false, Ordering::SeqCst);
    r
}

/// Ask the host to show the prompt on its canvas and wait for the answer. None = no host / it cannot show it: use the popup.
/// The host dying while we wait (window gone, `Sender` dropped) counts as a cancel: the worker never hangs.
fn via_host(o: &PromptOpts) -> Option<Option<String>> {
    if std::env::var_os("MSWIDGETS_PROMPT_WIN32").is_some() {
        return None;
    }
    let (hwnd, msg) = (*HOST.lock().ok()?)?;
    let anchor = o.anchor.unwrap_or_else(|| {
        let mut p = POINT::default();
        unsafe {
            let _ = GetCursorPos(&mut p);
        }
        (p.x, p.y)
    });
    let (tx, rx) = mpsc::channel();
    *PENDING.lock().ok()? = Some(Request { opts: o.clone(), anchor, reply: tx });
    let h = HWND(hwnd as *mut _);
    if unsafe { PostMessageW(h, msg, WPARAM(0), LPARAM(0)) }.is_err() {
        take_request();
        return None;
    }
    loop {
        match rx.recv_timeout(Duration::from_millis(250)) {
            Ok(Reply::Answer(a)) => return Some(a),
            Ok(Reply::Fallback) => return None,
            Err(RecvTimeoutError::Timeout) => {
                if !unsafe { IsWindow(h) }.as_bool() {
                    take_request();
                    return Some(None);
                }
            }
            Err(RecvTimeoutError::Disconnected) => return Some(None),
        }
    }
}

// ---------------------------------------------------------------- pure parts (unit-tested)

// design metrics at 100 % (px = dip * dpi / 96)
const W: i32 = 420;
const PAD: i32 = 20;
const TITLE_TOP: i32 = 18;
const TITLE_H: i32 = 24;
const WELL_GAP: i32 = 12;
const WELL_H: i32 = 40;
const WELL_H_MULTI: i32 = 132;
const HINT_GAP: i32 = 8;
const HINT_H: i32 = 16;
const BTN_GAP_TOP: i32 = 16;
const BTN_H: i32 = 32;
const BTN_GAP: i32 = 8;
const BTN_MIN_W: i32 = 88;
const BOTTOM: i32 = 18;
const WELL_PAD_X: i32 = 12;
const WELL_PAD_Y: i32 = 10;
const OPEN_MS: f32 = 150.0;
const CLOSE_MS: f32 = 100.0;
const RISE: f32 = 8.0; // the window glides this many dip upwards while it fades in

#[derive(Clone, Copy, Debug, PartialEq)]
enum Item {
    Edit,
    Ok,
    Cancel,
}

/// All rectangles in client pixels of the DPI the window lives on.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Layout {
    w: i32,
    h: i32,
    title: RECT,
    well: RECT,
    edit: RECT,
    hint: RECT,
    ok: RECT,
    cancel: RECT,
    line_h: i32,
}

fn rc(l: i32, t: i32, r: i32, b: i32) -> RECT {
    RECT { left: l, top: t, right: r, bottom: b }
}
fn inside(r: &RECT, x: i32, y: i32) -> bool {
    x >= r.left && x < r.right && y >= r.top && y < r.bottom
}

/// `ok_w` / `cancel_w` are the wanted button widths in dip (text + padding), `line_h` the EDIT font's line height in px.
fn layout(dpi: u32, multiline: bool, ok_w: i32, cancel_w: i32, line_h: i32) -> Layout {
    let k = dpi.max(48) as f32 / 96.0;
    let px = |v: i32| (v as f32 * k).round() as i32;
    let (w, pad) = (px(W), px(PAD));
    let title = rc(pad, px(TITLE_TOP), w - pad, px(TITLE_TOP + TITLE_H));
    let well_h = px(if multiline { WELL_H_MULTI } else { WELL_H });
    let well_top = title.bottom + px(WELL_GAP);
    let well = rc(pad, well_top, w - pad, well_top + well_h);
    let edit = if multiline {
        rc(well.left + px(WELL_PAD_X), well.top + px(WELL_PAD_Y), well.right - px(WELL_PAD_X), well.bottom - px(WELL_PAD_Y))
    } else {
        let t = well.top + (well_h - line_h) / 2;
        rc(well.left + px(WELL_PAD_X), t, well.right - px(WELL_PAD_X), t + line_h)
    };
    let hint = rc(pad, well.bottom + px(HINT_GAP), w - pad, well.bottom + px(HINT_GAP + HINT_H));
    let bt = hint.bottom + px(BTN_GAP_TOP);
    let ok = rc(w - pad - px(ok_w.max(BTN_MIN_W)), bt, w - pad, bt + px(BTN_H));
    let cancel = rc(ok.left - px(BTN_GAP) - px(cancel_w.max(BTN_MIN_W)), bt, ok.left - px(BTN_GAP), bt + px(BTN_H));
    Layout { w, h: ok.bottom + px(BOTTOM), title, well, edit, hint, ok, cancel, line_h }
}

impl Layout {
    /// which button (if any) is under a client point
    fn button_at(&self, x: i32, y: i32) -> Option<Item> {
        if inside(&self.ok, x, y) {
            Some(Item::Ok)
        } else if inside(&self.cancel, x, y) {
            Some(Item::Cancel)
        } else {
            None
        }
    }
}

/// Top-left of a `size` window opened next to `anchor`: centred on it horizontally, below it (above when there is no room),
/// then kept `margin` px inside the monitor work area `work` (the left/top edge wins when it does not fit at all).
pub(crate) fn place(anchor: (i32, i32), size: (i32, i32), work: RECT, gap: i32, margin: i32) -> (i32, i32) {
    let (w, h) = size;
    let mut y = anchor.1 + gap;
    if y + h > work.bottom - margin {
        y = anchor.1 - gap - h;
    }
    let fit = |v: i32, len: i32, lo: i32, hi: i32| v.min(hi - margin - len).max(lo + margin);
    (fit(anchor.0 - w / 2, w, work.left, work.right), fit(y, h, work.top, work.bottom))
}

fn ease_out(p: f32) -> f32 {
    let q = 1.0 - p.clamp(0.0, 1.0);
    1.0 - q * q * q
}

pub(crate) struct Texts {
    pub hint: String,
    pub ok: String,
    pub cancel: &'static str,
}
pub(crate) fn texts(lang: Lang, multiline: bool, ok_label: &str) -> Texts {
    let ok = ok_label.trim();
    Texts {
        hint: match (lang, multiline) {
            (Lang::Ru, false) => "Enter — готово · Esc — отмена",
            (Lang::Ru, true) => "Ctrl+Enter — готово · Esc — отмена",
            (Lang::En, false) => "Enter to confirm · Esc to cancel",
            (Lang::En, true) => "Ctrl+Enter to confirm · Esc to cancel",
        }
        .into(),
        ok: if ok.is_empty() { lang.pick("Готово", "Done") } else { ok }.into(),
        cancel: lang.pick("Отмена", "Cancel"),
    }
}

// ---------------------------------------------------------------- state

#[derive(Clone, Copy, PartialEq)]
enum Phase {
    Open(u64),
    Idle,
    Close(u64, f32), // start tick, alpha at the start
}

struct St {
    edit: HWND,
    multiline: bool,
    lay: Layout,
    k: f32,
    tx: Texts,
    title: Vec<u16>,
    cue: Vec<u16>,
    // drawing
    rt: ID2D1DCRenderTarget,
    dw: IDWriteFactory,
    brush: ID2D1SolidColorBrush,
    f_title: IDWriteTextFormat,
    f_hint: IDWriteTextFormat,
    f_btn: IDWriteTextFormat,
    memdc: HDC,
    bmp: HBITMAP,
    old_bmp: HGDIOBJ,
    edit_font: HFONT,
    well_brush: HBRUSH,
    // interaction
    focus: Item,
    ring: bool, // keyboard focus ring on the buttons
    hover: Option<Item>,
    pressed: Option<Item>,
    tracking: bool,
    composing: bool,
    backdrop: bool,
    // window animation
    phase: Phase,
    alpha: f32,
    pos: (i32, i32),
    result: Option<String>,
    done: bool,
}

thread_local! {
    // only the prompt thread (its window procs) touches this; `with` never panics on re-entrancy, it just skips
    static ST: RefCell<Option<St>> = const { RefCell::new(None) };
}
fn with<R>(f: impl FnOnce(&mut St) -> R) -> Option<R> {
    ST.with(|c| c.try_borrow_mut().ok().and_then(|mut g| g.as_mut().map(f)))
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}
fn now_ms() -> u64 {
    unsafe { GetTickCount64() }
}
fn cref(r: u8, g: u8, b: u8) -> COLORREF {
    COLORREF(r as u32 | (g as u32) << 8 | (b as u32) << 16)
}

// palette (straight RGB 0..255; the well is opaque so the EDIT, which GDI paints opaque, matches it exactly)
const PANEL: (u8, u8, u8) = (28, 29, 34);
const WELL: (u8, u8, u8) = (55, 56, 61); // the panel with 12 % white on it, like the gallery's search field (opaque for the EDIT)
const INK: (u8, u8, u8) = (245, 245, 247);
const CUE: (u8, u8, u8) = (136, 138, 146);
const ON_WHITE: (u8, u8, u8) = (18, 18, 23);
const PANEL_ALPHA: f32 = 0.66; // with the acrylic backdrop; opaque without it
const TIMER: usize = 1;
const EN_CHANGE: u32 = 0x0300;
const EM_GETRECT: u32 = 0x00B2;
const EM_GETLINECOUNT: u32 = 0x00BA;

fn c8(c: (u8, u8, u8), a: f32) -> D2D1_COLOR_F {
    col(c.0 as f32 / 255.0, c.1 as f32 / 255.0, c.2 as f32 / 255.0, a)
}
fn white(a: f32) -> D2D1_COLOR_F {
    col(1.0, 1.0, 1.0, a)
}
fn mixc(c: D2D1_COLOR_F, t: (f32, f32, f32), k: f32) -> D2D1_COLOR_F {
    col(c.r + (t.0 - c.r) * k, c.g + (t.1 - c.g) * k, c.b + (t.2 - c.b) * k, c.a)
}

// ---------------------------------------------------------------- the window

unsafe fn text_fmt(dw: &IDWriteFactory, family: PCWSTR, px: f32, weight: i32, center: bool) -> Option<IDWriteTextFormat> {
    // the bundled family when available (same face as the widgets), else the system family that was asked for
    let coll = crate::fonts::collection(dw);
    let name = windows::core::HSTRING::from(crate::fonts::family());
    let family = if coll.is_some() { PCWSTR(name.as_ptr()) } else { family };
    let f = dw.CreateTextFormat(family, coll.as_ref(),DWRITE_FONT_WEIGHT(weight), DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_STRETCH_NORMAL, px, w!("ru-ru")).ok()?;
    let _ = f.SetTextAlignment(if center { DWRITE_TEXT_ALIGNMENT_CENTER } else { DWRITE_TEXT_ALIGNMENT_LEADING });
    let _ = f.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER);
    let _ = f.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP);
    if let Ok(sign) = dw.CreateEllipsisTrimmingSign(&f) {
        let _ = f.SetTrimming(&DWRITE_TRIMMING { granularity: DWRITE_TRIMMING_GRANULARITY_CHARACTER, delimiter: 0, delimiterCount: 0 }, &sign);
    }
    Some(f)
}

unsafe fn text_width(dw: &IDWriteFactory, f: &IDWriteTextFormat, s: &[u16]) -> f32 {
    let Ok(l) = dw.CreateTextLayout(s, f, 4000.0, 100.0) else { return 0.0 };
    let mut m = DWRITE_TEXT_METRICS::default();
    let _ = l.GetMetrics(&mut m);
    m.width
}

unsafe fn run(o: PromptOpts) -> Option<String> {
    let hi = GetModuleHandleW(None).ok()?;
    let cls = w!("MsWidgetsPrompt");
    let wc = WNDCLASSW { lpfnWndProc: Some(proc), hInstance: hi.into(), lpszClassName: cls, hCursor: LoadCursorW(None, IDC_ARROW).ok()?, ..Default::default() };
    RegisterClassW(&wc); // fails harmlessly when already registered by an earlier prompt

    // where: the anchor (or the cursor), on the monitor that holds it
    let mut cur = POINT::default();
    let _ = GetCursorPos(&mut cur);
    let anchor = o.anchor.unwrap_or((cur.x, cur.y));
    let mon = MonitorFromPoint(POINT { x: anchor.0, y: anchor.1 }, MONITOR_DEFAULTTONEAREST);
    let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
    let _ = GetMonitorInfoW(mon, &mut mi);

    // created tiny and hidden first, only to learn the DPI of that monitor
    let hwnd = CreateWindowExW(WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_LAYERED, cls, PCWSTR::null(), WS_POPUP | WS_CLIPCHILDREN, anchor.0, anchor.1, 1, 1, None, None, hi, None).ok()?;
    let ok = build(hwnd, hi.into(), &o, mi.rcWork, anchor);
    if ok.is_none() {
        let _ = DestroyWindow(hwnd);
    }
    ok?;

    let mut msg = MSG::default();
    while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
        let _ = TranslateMessage(&msg);
        DispatchMessageW(&msg);
        if with(|s| s.done).unwrap_or(true) {
            break;
        }
    }
    let s = ST.with(|c| c.borrow_mut().take())?;
    let _ = SelectObject(s.memdc, s.old_bmp);
    let _ = DeleteObject(s.bmp);
    let _ = DeleteDC(s.memdc);
    let _ = DeleteObject(s.edit_font);
    let _ = DeleteObject(s.well_brush);
    s.result.filter(|t| !t.trim().is_empty())
}

/// Everything after the bare window exists: fonts, layout, DWM effects, the EDIT, Direct2D, the DIB; then shows it.
unsafe fn build(hwnd: HWND, hi: windows::Win32::Foundation::HINSTANCE, o: &PromptOpts, work: RECT, anchor: (i32, i32)) -> Option<()> {
    let dpi = GetDpiForWindow(hwnd);
    let k = if dpi == 0 { 1.0 } else { dpi as f32 / 96.0 };

    let dw: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED).ok()?;
    let tx = texts(o.lang, o.multiline, &o.ok_label);
    let f_title = text_fmt(&dw, w!("Segoe UI Variable Display"), 17.0 * k, 600, false)?;
    let f_hint = text_fmt(&dw, w!("Segoe UI Variable Text"), 12.0 * k, 400, false)?;
    let f_btn = text_fmt(&dw, w!("Segoe UI Variable Text"), 14.0 * k, 600, true)?;
    let bw = |s: &str| (text_width(&dw, &f_btn, &wide(s)) / k).ceil() as i32 + 36;

    // the EDIT's font and its line height
    crate::fonts::install_gdi();
    let edit_font = CreateFontW(-((15.0 * k).round() as i32), 0, 0, 0, 400, 0, 0, 0, 1, 0, 0, 5, 0, &windows::core::HSTRING::from(crate::fonts::family()));
    let dc0 = GetDC(hwnd);
    let old = SelectObject(dc0, edit_font);
    let mut tm = TEXTMETRICW::default();
    let _ = GetTextMetricsW(dc0, &mut tm);
    SelectObject(dc0, old);
    ReleaseDC(hwnd, dc0);
    let lay = layout(dpi, o.multiline, bw(&tx.ok), bw(tx.cancel), tm.tmHeight.max(1));

    let (x, y) = place(anchor, (lay.w, lay.h), work, (12.0 * k) as i32, (8.0 * k) as i32);
    let rise = (RISE * k) as i32;
    let _ = SetWindowPos(hwnd, HWND_TOPMOST, x, y + rise, lay.w, lay.h, SWP_NOACTIVATE);

    // DWM: rounded corners, dark system frame, acrylic when the build knows it (else a dark tint + the 1 px margin that gives popups a shadow)
    let flat = std::env::var_os("MSWIDGETS_PROMPT_ACRYLIC").is_none();
    let on: i32 = 1;
    let _ = DwmSetWindowAttribute(hwnd, DWMWA_WINDOW_CORNER_PREFERENCE, &DWMWCP_ROUND as *const _ as *const _, 4);
    let _ = DwmSetWindowAttribute(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE, &on as *const _ as *const _, 4);
    let backdrop = !flat && DwmSetWindowAttribute(hwnd, DWMWA_SYSTEMBACKDROP_TYPE, &DWMSBT_TRANSIENTWINDOW as *const _ as *const _, 4).is_ok();
    let m = if backdrop { MARGINS { cxLeftWidth: -1, cxRightWidth: -1, cyTopHeight: -1, cyBottomHeight: -1 } } else { MARGINS { cxLeftWidth: 0, cxRightWidth: 0, cyTopHeight: 0, cyBottomHeight: 1 } };
    let _ = DwmExtendFrameIntoClientArea(hwnd, &m);

    // the EDIT: borderless, inside the well; multiline gets a (dark) scrollbar that only exists while the text does not fit
    let init: Vec<u16> = o.initial.encode_utf16().chain(Some(0)).collect();
    let es = if o.multiline { ES_MULTILINE | ES_AUTOVSCROLL | ES_WANTRETURN } else { ES_AUTOHSCROLL };
    let style = WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(es as u32) | if o.multiline { WS_VSCROLL } else { WINDOW_STYLE(0) };
    let e = lay.edit;
    let edit = CreateWindowExW(WINDOW_EX_STYLE(0), w!("EDIT"), PCWSTR(init.as_ptr()), style, e.left, e.top, e.right - e.left, e.bottom - e.top, hwnd, None, hi, None).ok()?;
    SendMessageW(edit, WM_SETFONT, WPARAM(edit_font.0 as usize), LPARAM(1));
    let n = o.initial.encode_utf16().count();
    SendMessageW(edit, EM_SETSEL, WPARAM(n), LPARAM(n as isize));
    let _ = SetWindowSubclass(edit, Some(edit_proc), 1, 0);
    if o.multiline {
        let _ = SetWindowTheme(edit, w!("DarkMode_Explorer"), PCWSTR::null());
        let _ = ShowScrollBar(edit, SB_VERT, false);
    }

    // Direct2D into a top-down 32 bit DIB
    let factory: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None).ok()?;
    let props = D2D1_RENDER_TARGET_PROPERTIES {
        r#type: D2D1_RENDER_TARGET_TYPE_DEFAULT,
        pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED },
        dpiX: 96.0,
        dpiY: 96.0,
        usage: D2D1_RENDER_TARGET_USAGE_NONE,
        minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
    };
    let rt = factory.CreateDCRenderTarget(&props).ok()?;
    let _ = rt.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
    let brush = rt.CreateSolidColorBrush(&white(1.0), None).ok()?;
    let wdc = GetDC(hwnd);
    let memdc = CreateCompatibleDC(wdc);
    ReleaseDC(hwnd, wdc);
    let bmi = BITMAPINFO { bmiHeader: BITMAPINFOHEADER { biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32, biWidth: lay.w, biHeight: -lay.h, biPlanes: 1, biBitCount: 32, biCompression: 0, ..Default::default() }, ..Default::default() };
    let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
    let bmp = CreateDIBSection(memdc, &bmi, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
    let old_bmp = SelectObject(memdc, bmp);
    rt.BindDC(memdc, &rc(0, 0, lay.w, lay.h)).ok()?;

    let well_brush = CreateSolidBrush(cref(WELL.0, WELL.1, WELL.2));
    let st = St {
        edit,
        multiline: o.multiline,
        lay,
        k,
        tx,
        title: wide(&o.title),
        cue: wide(&o.placeholder),
        rt,
        dw,
        brush,
        f_title,
        f_hint,
        f_btn,
        memdc,
        bmp,
        old_bmp,
        edit_font,
        well_brush,
        focus: Item::Edit,
        ring: false,
        hover: None,
        pressed: None,
        tracking: false,
        composing: false,
        backdrop,
        phase: Phase::Open(now_ms()),
        alpha: 0.0,
        pos: (x, y),
        result: None,
        done: false,
    };
    ST.with(|c| *c.borrow_mut() = Some(st));
    if o.multiline {
        update_scrollbar(edit);
    }

    let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), 0, LWA_ALPHA);
    let _ = ShowWindow(hwnd, SW_SHOW);
    force_foreground(hwnd);
    let _ = SetFocus(edit);
    SetTimer(hwnd, TIMER, 15, None);
    Some(())
}

/// The layer is NOACTIVATE and we run on a worker thread: borrow the foreground thread's input state so SetForegroundWindow is allowed.
unsafe fn force_foreground(hwnd: HWND) {
    use windows::Win32::System::Threading::AttachThreadInput;
    let fg = GetForegroundWindow();
    let (me, theirs) = (GetCurrentThreadId(), GetWindowThreadProcessId(fg, None));
    let attached = theirs != 0 && theirs != me && AttachThreadInput(me, theirs, true).as_bool();
    let _ = SetWindowPos(hwnd, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
    let _ = SetForegroundWindow(hwnd);
    let _ = BringWindowToTop(hwnd);
    if attached {
        let _ = AttachThreadInput(me, theirs, false);
    }
}

// ---------------------------------------------------------------- actions

unsafe fn read_text(edit: HWND) -> String {
    let len = GetWindowTextLengthW(edit) as usize;
    let mut buf = vec![0u16; len + 1];
    let got = GetWindowTextW(edit, &mut buf) as usize;
    String::from_utf16_lossy(&buf[..got])
}

/// Start the fade-out; the window is destroyed when it ends. `result` None = cancelled.
unsafe fn begin_close(hwnd: HWND, result: Option<String>) {
    let started = with(|s| {
        if matches!(s.phase, Phase::Close(..)) {
            return false;
        }
        s.result = result;
        s.phase = Phase::Close(now_ms(), s.alpha.max(1.0));
        true
    });
    if started != Some(true) {
        return;
    }
    let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
    if ex & WS_EX_LAYERED.0 as isize == 0 {
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex | WS_EX_LAYERED.0 as isize);
        let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), 255, LWA_ALPHA);
    }
    SetTimer(hwnd, TIMER, 15, None);
}

unsafe fn confirm(hwnd: HWND) {
    if let Some(edit) = with(|s| s.edit) {
        begin_close(hwnd, Some(read_text(edit)));
    }
}

unsafe fn activate(hwnd: HWND, it: Item) {
    match it {
        Item::Cancel => begin_close(hwnd, None),
        _ => confirm(hwnd),
    }
}

/// Tab / Shift+Tab: field > OK > Cancel > field.
unsafe fn move_focus(hwnd: HWND, back: bool) {
    let order = [Item::Edit, Item::Ok, Item::Cancel];
    let Some((cur, edit)) = with(|s| (s.focus, s.edit)) else { return };
    let i = order.iter().position(|&x| x == cur).unwrap_or(0);
    let next = order[(i + if back { 2 } else { 1 }) % 3];
    let _ = with(|s| {
        s.focus = next;
        s.ring = next != Item::Edit;
    });
    if next == Item::Edit {
        let _ = SetFocus(edit);
        SendMessageW(edit, EM_SETSEL, WPARAM(0), LPARAM(-1));
    } else {
        let _ = SetFocus(hwnd);
    }
    let _ = InvalidateRect(hwnd, None, false);
}

/// Multiline EDIT: show its scrollbar only while the text is taller than the field.
unsafe fn update_scrollbar(edit: HWND) {
    let Some(line_h) = with(|s| s.lay.line_h) else { return };
    let lines = SendMessageW(edit, EM_GETLINECOUNT, WPARAM(0), LPARAM(0)).0 as i32;
    let mut r = RECT::default();
    let _ = GetClientRect(edit, &mut r);
    let _ = ShowScrollBar(edit, SB_VERT, lines * line_h > r.bottom - r.top);
}

// ---------------------------------------------------------------- drawing

unsafe fn text(s: &St, t: &[u16], f: &IDWriteTextFormat, r: RECT, c: D2D1_COLOR_F) {
    if t.is_empty() {
        return;
    }
    if let Ok(l) = s.dw.CreateTextLayout(t, f, (r.right - r.left) as f32, (r.bottom - r.top) as f32) {
        s.brush.SetColor(&c);
        s.rt.DrawTextLayout(pt(r.left as f32, r.top as f32), &l, &s.brush, D2D1_DRAW_TEXT_OPTIONS_NONE);
    }
}

unsafe fn render(s: &St) {
    let (rt, b, k, lay) = (&s.rt, &s.brush, s.k, &s.lay);
    let fill = |c: D2D1_COLOR_F| {
        b.SetColor(&c);
        b
    };
    rt.BeginDraw();
    rt.Clear(Some(&col(0.0, 0.0, 0.0, 0.0)));
    // panel + hairline rim (DWM rounds the window itself)
    // neutral glass, no accent (like the gallery and the settings): a light sheen fading down from the top, a hairline rim
    // (DWM rounds the window itself)
    let (pw, ph) = (lay.w as f32, lay.h as f32);
    rt.FillRectangle(&rf(0.0, 0.0, pw, ph), fill(c8(PANEL, if s.backdrop { PANEL_ALPHA } else { 1.0 })));
    let strips = 24;
    let sheen_h = (ph * 0.55).min(110.0 * k);
    for i in 0..strips {
        let t = i as f32 / strips as f32;
        let a = 0.075 * (1.0 - t) * (1.0 - t);
        rt.FillRectangle(&rf(0.0, t * sheen_h, pw, (t + 1.0 / strips as f32) * sheen_h + 0.5), fill(white(a)));
    }
    rt.DrawRoundedRectangle(&rr(0.5, 0.5, pw - 1.0, ph - 1.0, 8.0 * k), fill(white(0.14)), 1.0, None::<&ID2D1StrokeStyle>);
    text(s, &s.title, &s.f_title, lay.title, c8(INK, 1.0));

    // the well (a white 12 % layer, as the gallery's search field); focused = a white ring with a soft halo
    let wl = lay.well;
    let (wx, wy, ww, wh, rad) = (wl.left as f32, wl.top as f32, (wl.right - wl.left) as f32, (wl.bottom - wl.top) as f32, 10.0 * k);
    rt.FillRoundedRectangle(&rr(wx, wy, ww, wh, rad), fill(c8(WELL, 1.0)));
    if s.focus == Item::Edit {
        let e = 2.0 * k;
        rt.DrawRoundedRectangle(&rr(wx - e, wy - e, ww + 2.0 * e, wh + 2.0 * e, rad + e), fill(white(0.16)), 3.0 * k, None::<&ID2D1StrokeStyle>);
        rt.DrawRoundedRectangle(&rr(wx, wy, ww, wh, rad), fill(white(0.70)), 1.4 * k, None::<&ID2D1StrokeStyle>);
    } else {
        rt.DrawRoundedRectangle(&rr(wx + 0.5, wy + 0.5, ww - 1.0, wh - 1.0, rad), fill(white(0.10)), 1.0, None::<&ID2D1StrokeStyle>);
    }
    text(s, &wide(&s.tx.hint), &s.f_hint, lay.hint, white(0.50));

    // buttons are pills: primary = white glass with dark text (the gallery's "Done"), secondary = quiet white layer;
    // hover lightens, press darkens, keyboard focus gets a ring
    for it in [Item::Cancel, Item::Ok] {
        let (r, label, primary) = if it == Item::Ok { (lay.ok, wide(&s.tx.ok), true) } else { (lay.cancel, wide(s.tx.cancel), false) };
        let (hot, down) = (s.hover == Some(it), s.pressed == Some(it) && s.hover == Some(it));
        let (x, y, bw, bh) = (r.left as f32, r.top as f32, (r.right - r.left) as f32, (r.bottom - r.top) as f32);
        let rad = bh / 2.0;
        let c = if primary {
            let base = white(if hot { 1.0 } else { 0.92 });
            if down { mixc(base, (0.0, 0.0, 0.0), 0.14) } else { base }
        } else {
            white(if down { 0.08 } else if hot { 0.20 } else { 0.12 })
        };
        if primary && !down {
            rt.FillRoundedRectangle(&rr(x, y + 1.5 * k, bw, bh, rad), fill(col(0.0, 0.0, 0.0, 0.20)));
        }
        rt.FillRoundedRectangle(&rr(x, y, bw, bh, rad), fill(c));
        if s.ring && s.focus == it {
            let e = 2.5 * k;
            rt.DrawRoundedRectangle(&rr(x - e, y - e, bw + 2.0 * e, bh + 2.0 * e, rad + e), fill(white(0.90)), 1.5 * k, None::<&ID2D1StrokeStyle>);
        }
        text(s, &label, &s.f_btn, r, if primary { c8(ON_WHITE, 1.0) } else { c8(INK, 1.0) });
    }
    let _ = rt.EndDraw(None, None);
}

// ---------------------------------------------------------------- window procs

unsafe extern "system" fn proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let (mx, my) = ((lp.0 & 0xffff) as i16 as i32, ((lp.0 >> 16) & 0xffff) as i16 as i32);
    match msg {
        WM_ERASEBKGND => LRESULT(1),
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let dc = BeginPaint(hwnd, &mut ps); // sends WM_ERASEBKGND: must not hold the state borrow
            let _ = with(|s| {
                render(s);
                let _ = BitBlt(dc, 0, 0, s.lay.w, s.lay.h, s.memdc, 0, 0, SRCCOPY);
            });
            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        }
        WM_CTLCOLOREDIT => {
            if let Some(b) = with(|s| s.well_brush) {
                let dc = HDC(wp.0 as *mut _);
                SetTextColor(dc, cref(INK.0, INK.1, INK.2));
                SetBkColor(dc, cref(WELL.0, WELL.1, WELL.2));
                return LRESULT(b.0 as isize);
            }
            DefWindowProcW(hwnd, msg, wp, lp)
        }
        WM_COMMAND => {
            if (wp.0 >> 16) as u32 == EN_CHANGE && with(|s| s.multiline) == Some(true) {
                update_scrollbar(HWND(lp.0 as *mut _));
            }
            LRESULT(0)
        }
        WM_TIMER if wp.0 == TIMER => {
            tick(hwnd);
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            let need_track = with(|s| {
                let h = s.lay.button_at(mx, my);
                if h != s.hover {
                    s.hover = h;
                    let _ = InvalidateRect(hwnd, None, false);
                }
                !std::mem::replace(&mut s.tracking, true)
            });
            if need_track == Some(true) {
                let mut t = TRACKMOUSEEVENT { cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: hwnd, dwHoverTime: 0 };
                let _ = TrackMouseEvent(&mut t);
            }
            LRESULT(0)
        }
        0x02A3 => {
            // WM_MOUSELEAVE
            let _ = with(|s| {
                s.tracking = false;
                s.hover = None;
            });
            let _ = InvalidateRect(hwnd, None, false);
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            let hit = with(|s| {
                if matches!(s.phase, Phase::Close(..)) {
                    return (None, None);
                }
                let b = s.lay.button_at(mx, my);
                s.pressed = b;
                if let Some(it) = b {
                    s.focus = it;
                    s.ring = false;
                }
                (b, inside(&s.lay.well, mx, my).then_some(s.edit))
            });
            match hit {
                Some((Some(_), _)) => {
                    SetCapture(hwnd);
                    let _ = SetFocus(hwnd);
                    let _ = InvalidateRect(hwnd, None, false);
                }
                Some((None, Some(edit))) => {
                    let _ = SetFocus(edit); // a click on the well's padding focuses the field
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let _ = ReleaseCapture();
            let fired = with(|s| {
                let p = s.pressed.take();
                p.filter(|&p| s.lay.button_at(mx, my) == Some(p))
            })
            .flatten();
            let _ = InvalidateRect(hwnd, None, false);
            if let Some(it) = fired {
                activate(hwnd, it);
            }
            LRESULT(0)
        }
        WM_KEYDOWN => {
            // keyboard focus sits on the window itself while a button is focused
            let k = wp.0 as u16;
            if k == VK_ESCAPE.0 {
                begin_close(hwnd, None);
            } else if k == VK_TAB.0 {
                move_focus(hwnd, GetKeyState(VK_SHIFT.0 as i32) < 0);
            } else if k == VK_RETURN.0 || k == VK_SPACE.0 {
                if let Some(f) = with(|s| s.focus) {
                    activate(hwnd, f);
                }
            }
            LRESULT(0)
        }
        WM_ACTIVATE => {
            if (wp.0 & 0xffff) as u32 == WA_INACTIVE {
                begin_close(hwnd, None); // clicked elsewhere = cancel
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            let _ = KillTimer(hwnd, TIMER);
            let _ = with(|s| s.done = true);
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

/// Fade (+ a few px of glide) in and out; the layered style only lives while it animates, so the DWM backdrop is not disturbed afterwards.
unsafe fn tick(hwnd: HWND) {
    let now = now_ms();
    let step = with(|s| {
        let rise = RISE * s.k;
        match s.phase {
            Phase::Open(t0) => {
                let p = ((now.saturating_sub(t0)) as f32 / OPEN_MS).min(1.0);
                let e = ease_out(p);
                s.alpha = 255.0 * e;
                if p >= 1.0 {
                    s.phase = Phase::Idle;
                }
                Some((s.alpha as u8, s.pos.0, s.pos.1 + ((1.0 - e) * rise) as i32, p >= 1.0, false))
            }
            Phase::Close(t0, a0) => {
                let p = ((now.saturating_sub(t0)) as f32 / CLOSE_MS).min(1.0);
                s.alpha = a0 * (1.0 - p);
                Some((s.alpha as u8, s.pos.0, s.pos.1 + (p * rise * 0.5) as i32, false, p >= 1.0))
            }
            Phase::Idle => None,
        }
    })
    .flatten();
    let Some((a, x, y, opened, closed)) = step else {
        let _ = KillTimer(hwnd, TIMER);
        return;
    };
    let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), a, LWA_ALPHA);
    let _ = SetWindowPos(hwnd, None, x, y, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
    if closed {
        let _ = KillTimer(hwnd, TIMER);
        let _ = DestroyWindow(hwnd);
    } else if opened {
        let _ = KillTimer(hwnd, TIMER);
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex & !(WS_EX_LAYERED.0 as isize));
        let _ = RedrawWindow(hwnd, None, None, RDW_INVALIDATE | RDW_ALLCHILDREN | RDW_UPDATENOW);
    }
}

/// Subclass of the EDIT: our keys (Enter / Esc / Tab / Ctrl+A), the placeholder, focus changes for the well's ring.
unsafe extern "system" fn edit_proc(h: HWND, m: u32, wp: WPARAM, lp: LPARAM, _id: usize, _rd: usize) -> LRESULT {
    let parent = GetParent(h).unwrap_or_default();
    let multi = with(|s| s.multiline).unwrap_or(false);
    match m {
        WM_KEYDOWN => {
            let k = wp.0 as u16;
            let ctrl = GetKeyState(VK_CONTROL.0 as i32) < 0;
            if k == VK_ESCAPE.0 {
                begin_close(parent, None);
                return LRESULT(0);
            }
            if k == VK_RETURN.0 && (!multi || ctrl) {
                confirm(parent);
                return LRESULT(0);
            }
            if k == VK_TAB.0 {
                move_focus(parent, GetKeyState(VK_SHIFT.0 as i32) < 0);
                return LRESULT(0);
            }
            if k == b'A' as u16 && ctrl {
                SendMessageW(h, EM_SETSEL, WPARAM(0), LPARAM(-1));
                return LRESULT(0);
            }
        }
        // the characters those keys would also type: Esc, Tab, Ctrl+A, Ctrl+Enter (LF), Enter in a single line
        WM_CHAR if matches!(wp.0, 27 | 9 | 1 | 10) || (wp.0 == 13 && !multi) => return LRESULT(0),
        0x010D => {
            let _ = with(|s| s.composing = true); // WM_IME_STARTCOMPOSITION
        }
        0x010E => {
            let _ = with(|s| s.composing = false); // WM_IME_ENDCOMPOSITION
        }
        WM_SETFOCUS | WM_KILLFOCUS => {
            let r = DefSubclassProc(h, m, wp, lp);
            if m == WM_SETFOCUS {
                let _ = with(|s| {
                    s.focus = Item::Edit;
                    s.ring = false;
                });
            }
            let _ = InvalidateRect(parent, None, false);
            return r;
        }
        WM_PAINT => {
            let r = DefSubclassProc(h, m, wp, lp);
            draw_cue(h);
            return r;
        }
        _ => {}
    }
    DefSubclassProc(h, m, wp, lp)
}

/// Placeholder: EM_SETCUEBANNER needs comctl32 v6 (no manifest here), so paint it over the empty field ourselves.
unsafe fn draw_cue(h: HWND) {
    if GetWindowTextLengthW(h) != 0 {
        return;
    }
    let Some((mut cue, composing, multi)) = with(|s| (s.cue.clone(), s.composing, s.multiline)) else { return };
    if cue.is_empty() || composing {
        return;
    }
    let dc = GetDC(h);
    let font = SendMessageW(h, WM_GETFONT, WPARAM(0), LPARAM(0)).0;
    let old = SelectObject(dc, HGDIOBJ(font as *mut _));
    SetBkMode(dc, TRANSPARENT);
    SetTextColor(dc, cref(CUE.0, CUE.1, CUE.2));
    let mut r = RECT::default();
    SendMessageW(h, EM_GETRECT, WPARAM(0), LPARAM(&mut r as *mut RECT as isize));
    let flags = if multi { DT_LEFT | DT_WORDBREAK | DT_NOPREFIX } else { DT_LEFT | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS };
    DrawTextW(dc, &mut cue, &mut r, flags);
    SelectObject(dc, old);
    ReleaseDC(h, dc);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_scales_with_dpi() {
        let l1 = layout(96, false, 88, 88, 20);
        let l15 = layout(144, false, 88, 88, 30);
        let l2 = layout(192, false, 88, 88, 40);
        assert_eq!((l1.w, l15.w, l2.w), (420, 630, 840));
        assert_eq!(l1.well.bottom - l1.well.top, 40);
        assert_eq!(l2.h, l1.h * 2);
        assert!((l15.h * 2 - l1.h * 3).abs() <= 2); // rounding only
        // everything stays inside the window, buttons right-aligned inside the padding, cancel left of ok
        for l in [l1, l15, l2] {
            for r in [l.title, l.well, l.edit, l.hint, l.ok, l.cancel] {
                assert!(r.left >= 0 && r.top >= 0 && r.right <= l.w && r.bottom <= l.h, "{r:?} outside {}x{}", l.w, l.h);
            }
            assert!(l.cancel.right < l.ok.left && l.ok.right == l.w - (l.well.left));
            assert!(l.edit.left > l.well.left && l.edit.right < l.well.right);
        }
        // the single-line field is vertically centred in the well
        assert_eq!(l1.edit.top - l1.well.top, l1.well.bottom - l1.edit.bottom);
        assert_eq!(l1.edit.bottom - l1.edit.top, 20);
    }

    #[test]
    fn multiline_is_taller_and_wide_labels_widen_buttons() {
        let a = layout(96, false, 88, 88, 20);
        let b = layout(96, true, 120, 88, 20);
        assert!(b.h > a.h + 80);
        assert_eq!(b.edit.top - b.well.top, 10);
        assert_eq!(b.ok.right - b.ok.left, 120);
        assert_eq!(b.cancel.right - b.cancel.left, 88); // minimum width
        assert_eq!(layout(96, false, 10, 10, 20).ok.right - layout(96, false, 10, 10, 20).ok.left, 88);
    }

    #[test]
    fn button_hit_test() {
        let l = layout(96, false, 88, 88, 20);
        let (ox, oy) = ((l.ok.left + l.ok.right) / 2, (l.ok.top + l.ok.bottom) / 2);
        assert_eq!(l.button_at(ox, oy), Some(Item::Ok));
        assert_eq!(l.button_at(l.cancel.left + 1, l.cancel.top + 1), Some(Item::Cancel));
        assert_eq!(l.button_at(l.well.left + 5, l.well.top + 5), None);
        assert_eq!(l.button_at(l.ok.right, oy), None);
    }

    #[test]
    fn place_clamps_to_the_monitor() {
        let work = rc(0, 0, 1920, 1040);
        let size = (420, 184);
        // plenty of room: centred under the cursor
        assert_eq!(place((960, 300), size, work, 12, 8), (750, 312));
        // near the left / right edge
        assert_eq!(place((5, 300), size, work, 12, 8).0, 8);
        assert_eq!(place((1915, 300), size, work, 12, 8).0, 1920 - 8 - 420);
        // near the bottom it flips above the cursor, never leaves the screen
        let (_, y) = place((960, 1030), size, work, 12, 8);
        assert_eq!(y, 1030 - 12 - 184);
        // a secondary monitor with a negative origin
        let left = rc(-1920, 0, 0, 1080);
        let (x, y) = place((-10, -50), size, left, 12, 8);
        assert!(x >= -1920 + 8 && x + 420 <= -8 && y == 8);
        // a window bigger than the work area pins to the top-left margin
        assert_eq!(place((50, 50), (3000, 3000), work, 12, 8), (8, 8));
    }

    /// The worker never hangs on the host: a window that is gone is a cancel, a host that cannot show the prompt sends `Fallback`, an answer
    /// arrives as it was sent. (Posts to the NULL window = this thread's own queue, so no window is needed.)
    #[test]
    fn host_bridge_answers_and_never_hangs() {
        let o = PromptOpts { title: "t".into(), initial: String::new(), multiline: false, placeholder: String::new(), ok_label: String::new(), lang: Lang::En, anchor: Some((1, 2)) };
        assert_eq!(via_host(&o), None); // no host: the popup
        set_host(Some((0, WM_APP + 77)));
        assert_eq!(via_host(&o), Some(None)); // IsWindow(NULL) is false: the host is gone
        for reply in [Reply::Fallback, Reply::Answer(Some("hello".into())), Reply::Answer(None)] {
            let expect = match &reply {
                Reply::Fallback => None,
                Reply::Answer(a) => Some(a.clone()),
            };
            let th = std::thread::spawn(move || loop {
                if let Some(r) = take_request() {
                    assert_eq!((r.anchor, r.opts.title.as_str()), ((1, 2), "t"));
                    let _ = r.reply.send(reply);
                    return;
                }
                std::thread::sleep(Duration::from_millis(5));
            });
            assert_eq!(via_host(&o), expect);
            th.join().unwrap();
        }
        // a host that drops the request (shutting down) is a cancel too
        let th = std::thread::spawn(|| loop {
            if take_request().is_some() {
                return; // the Sender is dropped here
            }
            std::thread::sleep(Duration::from_millis(5));
        });
        assert_eq!(via_host(&o), Some(None));
        th.join().unwrap();
        set_host(None);
        assert_eq!(via_host(&o), None);
    }

    #[test]
    fn strings_follow_language_and_mode() {
        let ru = texts(Lang::Ru, false, "");
        assert_eq!((ru.ok.as_str(), ru.cancel), ("Готово", "Отмена"));
        assert!(ru.hint.starts_with("Enter"));
        let en = texts(Lang::En, true, "  Add ");
        assert_eq!((en.ok.as_str(), en.cancel), ("Add", "Cancel"));
        assert!(en.hint.starts_with("Ctrl+Enter"));
        assert_eq!(texts(Lang::En, false, "").ok, "Done");
    }

    /// Manual: `MSW_PROMPT_SHOTS=<dir> cargo test --release -- --ignored prompt_shots`. Opens the prompt three ways and copies
    /// only the window's own rectangle from the screen into <dir>\prompt<N>.bmp (the window flashes up for about a second each).
    #[test]
    #[ignore]
    fn prompt_shots() {
        let Some(dir) = std::env::var_os("MSW_PROMPT_SHOTS") else { return };
        let cases: [(&str, &str, bool, &str); 3] = [
            ("Новое напоминание", "", false, "Например: позвонить маме в 18:00"),
            ("Новое напоминание", "Купить молоко и хлеб", false, ""),
            ("Заметка", "Первая строка\r\nВторая строка", true, ""),
        ];
        for (n, (title, initial, multi, cue)) in cases.into_iter().enumerate() {
            let o = PromptOpts { placeholder: cue.into(), anchor: Some((900, 300)), ..PromptOpts::new(title, initial, multi) };
            let th = std::thread::spawn(move || prompt_ex(o));
            std::thread::sleep(std::time::Duration::from_millis(900));
            unsafe {
                let Ok(hwnd) = FindWindowW(w!("MsWidgetsPrompt"), PCWSTR::null()) else { panic!("no prompt window") };
                let mut r = RECT::default();
                let _ = GetWindowRect(hwnd, &mut r);
                let (w, h) = (r.right - r.left, r.bottom - r.top);
                let screen = GetDC(None);
                let mem = CreateCompatibleDC(screen);
                let bmi = BITMAPINFO { bmiHeader: BITMAPINFOHEADER { biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32, biWidth: w, biHeight: -h, biPlanes: 1, biBitCount: 32, ..Default::default() }, ..Default::default() };
                let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
                let bmp = CreateDIBSection(mem, &bmi, DIB_RGB_COLORS, &mut bits, None, 0).unwrap();
                let old = SelectObject(mem, bmp);
                let _ = BitBlt(mem, 0, 0, w, h, screen, r.left, r.top, SRCCOPY | CAPTUREBLT);
                let px = std::slice::from_raw_parts(bits as *const u8, (w * h * 4) as usize).to_vec();
                SelectObject(mem, old);
                let _ = DeleteObject(bmp);
                let _ = DeleteDC(mem);
                ReleaseDC(None, screen);
                // 32 bit top-down BMP (BITMAPINFOHEADER with a negative height)
                let mut f = Vec::new();
                f.extend_from_slice(b"BM");
                f.extend_from_slice(&(54 + px.len() as u32).to_le_bytes());
                f.extend_from_slice(&[0; 4]);
                f.extend_from_slice(&54u32.to_le_bytes());
                f.extend_from_slice(&40u32.to_le_bytes());
                f.extend_from_slice(&w.to_le_bytes());
                f.extend_from_slice(&(-h).to_le_bytes());
                f.extend_from_slice(&1u16.to_le_bytes());
                f.extend_from_slice(&32u16.to_le_bytes());
                f.extend_from_slice(&[0; 24]);
                f.extend_from_slice(&px);
                std::fs::write(std::path::Path::new(&dir).join(format!("prompt{n}.bmp")), f).unwrap();
                let _ = PostMessageW(hwnd, WM_KEYDOWN, WPARAM(VK_ESCAPE.0 as usize), LPARAM(0));
            }
            let _ = th.join();
            std::thread::sleep(std::time::Duration::from_millis(300));
        }
    }

    #[test]
    fn easing_is_monotonic_and_bounded() {
        assert_eq!((ease_out(0.0), ease_out(1.0)), (0.0, 1.0));
        assert!(ease_out(0.5) > 0.5 && ease_out(2.0) == 1.0 && ease_out(-1.0) == 0.0);
    }
}
