//! A small modal text prompt for widgets (reminders, notes, shortcuts, timer duration...): `cx.prompt_text(...)` runs `prompt` on a
//! worker thread (`cx.spawn`), so the widget layer (NOACTIVATE, click-through) never needs keyboard focus itself.
//! A real Win32 EDIT control is used on purpose: IME, selection, clipboard and accessibility come for free.
//! Enter = OK (multiline: Ctrl+Enter), Esc / click elsewhere = cancel.
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Controls::EM_SETSEL;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, SetFocus, VK_CONTROL, VK_ESCAPE, VK_RETURN};
use windows::Win32::UI::WindowsAndMessaging::*;

static BUSY: AtomicBool = AtomicBool::new(false);
static RESULT: Mutex<Option<String>> = Mutex::new(None);

struct St {
    edit: HWND,
    multiline: bool,
    title: Vec<u16>,
    hint: Vec<u16>,
    font: HFONT,
    small: HFONT,
    bg: HBRUSH,
    edit_bg: HBRUSH,
    done: bool,
}
// the handles are only used by the prompt thread (and its window proc): moving the struct between threads is safe
unsafe impl Send for St {}
static ST: Mutex<Option<St>> = Mutex::new(None);

/// Blocks (own message loop) until the user answers. `None` = cancelled / another prompt is open / the window could not be created.
pub fn prompt(title: &str, initial: &str, multiline: bool) -> Option<String> {
    if BUSY.swap(true, Ordering::SeqCst) {
        return None;
    }
    let r = unsafe { run(title, initial, multiline) };
    BUSY.store(false, Ordering::SeqCst);
    r
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

unsafe fn run(title: &str, initial: &str, multiline: bool) -> Option<String> {
    *RESULT.lock().ok()? = None;
    let hi = GetModuleHandleW(None).ok()?;
    let cls = w!("MsWidgetsPrompt");
    let wc = WNDCLASSW { lpfnWndProc: Some(proc), hInstance: hi.into(), lpszClassName: cls, hCursor: LoadCursorW(None, IDC_ARROW).ok()?, ..Default::default() };
    RegisterClassW(&wc); // fails harmlessly when already registered by an earlier prompt

    let (ww, eh) = (400, if multiline { 120 } else { 34 });
    let wh = 16 + 24 + 8 + eh + 8 + 20 + 14;
    let mut pt = POINT::default();
    let _ = GetCursorPos(&mut pt);
    let mon = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
    let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
    let _ = GetMonitorInfoW(mon, &mut mi);
    let wa: RECT = mi.rcWork;
    let x = (pt.x - ww / 2).clamp(wa.left + 8, (wa.right - ww - 8).max(wa.left + 8));
    let y = (pt.y + 12).clamp(wa.top + 8, (wa.bottom - wh - 8).max(wa.top + 8));

    let font = CreateFontW(-18, 0, 0, 0, 400, 0, 0, 0, 1, 0, 0, 5, 0, w!("Segoe UI Variable Text"));
    let small = CreateFontW(-13, 0, 0, 0, 400, 0, 0, 0, 1, 0, 0, 5, 0, w!("Segoe UI Variable Text"));
    let bg = CreateSolidBrush(COLORREF(0x2a2622)); // BGR of rgb(34,38,42)
    let edit_bg = CreateSolidBrush(COLORREF(0x1c1a18));
    let hwnd = CreateWindowExW(WS_EX_TOPMOST | WS_EX_TOOLWINDOW, cls, PCWSTR::null(), WS_POPUP | WS_CLIPCHILDREN, x, y, ww, wh, None, None, hi, None).ok()?;
    let _ = DwmSetWindowAttribute(hwnd, DWMWA_WINDOW_CORNER_PREFERENCE, &DWMWCP_ROUND as *const _ as *const _, 4);

    let style = WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(if multiline { (ES_MULTILINE | ES_AUTOVSCROLL | ES_WANTRETURN) as u32 } else { ES_AUTOHSCROLL as u32 });
    let init: Vec<u16> = initial.encode_utf16().chain(Some(0)).collect();
    let edit = CreateWindowExW(WINDOW_EX_STYLE(0), w!("EDIT"), PCWSTR(init.as_ptr()), style, 16, 16 + 24 + 8, ww - 32, eh, hwnd, None, hi, None).ok()?;
    SendMessageW(edit, WM_SETFONT, WPARAM(font.0 as usize), LPARAM(1));
    let n = initial.encode_utf16().count();
    SendMessageW(edit, EM_SETSEL, WPARAM(n), LPARAM(n as isize));

    *ST.lock().ok()? = Some(St {
        edit,
        multiline,
        title: wide(title),
        hint: wide(if multiline { "Ctrl+Enter — готово · Esc — отмена" } else { "Enter — готово · Esc — отмена" }),
        font,
        small,
        bg,
        edit_bg,
        done: false,
    });

    let _ = ShowWindow(hwnd, SW_SHOWNORMAL);
    force_foreground(hwnd);
    let _ = SetFocus(edit);

    let mut msg = MSG::default();
    while GetMessageW(&mut msg, None, 0, 0).as_bool() {
        if msg.message == WM_KEYDOWN && msg.hwnd == edit {
            let k = msg.wParam.0 as u16;
            let ctrl = GetKeyState(VK_CONTROL.0 as i32) < 0;
            if k == VK_ESCAPE.0 {
                let _ = DestroyWindow(hwnd);
                continue;
            }
            if k == VK_RETURN.0 && (!multiline || ctrl) {
                let len = GetWindowTextLengthW(edit) as usize;
                let mut buf = vec![0u16; len + 1];
                let got = GetWindowTextW(edit, &mut buf) as usize;
                if let Ok(mut r) = RESULT.lock() {
                    *r = Some(String::from_utf16_lossy(&buf[..got]));
                }
                let _ = DestroyWindow(hwnd);
                continue;
            }
        }
        let _ = TranslateMessage(&msg);
        DispatchMessageW(&msg);
        if ST.lock().ok().and_then(|g| g.as_ref().map(|s| s.done)).unwrap_or(true) {
            break;
        }
    }
    if let Some(s) = ST.lock().ok().and_then(|mut g| g.take()) {
        let _ = DeleteObject(s.font);
        let _ = DeleteObject(s.small);
        let _ = DeleteObject(s.bg);
        let _ = DeleteObject(s.edit_bg);
    }
    RESULT.lock().ok().and_then(|mut r| r.take()).filter(|s| !s.trim().is_empty())
}

/// The layer is NOACTIVATE and we run on a worker thread: borrow the foreground thread's input state so SetForegroundWindow is allowed.
unsafe fn force_foreground(hwnd: HWND) {
    use windows::Win32::System::Threading::AttachThreadInput;
    let fg = GetForegroundWindow();
    let (me, theirs) = (GetCurrentThreadId(), GetWindowThreadProcessId(fg, None));
    let attached = theirs != 0 && theirs != me && AttachThreadInput(me, theirs, true).as_bool();
    let _ = SetForegroundWindow(hwnd);
    let _ = BringWindowToTop(hwnd);
    if attached {
        let _ = AttachThreadInput(me, theirs, false);
    }
}

unsafe extern "system" fn proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_ERASEBKGND => {
            if let Some(s) = ST.lock().ok().and_then(|g| g.as_ref().map(|s| s.bg)) {
                let mut r = RECT::default();
                let _ = GetClientRect(hwnd, &mut r);
                FillRect(HDC(wp.0 as *mut _), &r, s);
                return LRESULT(1);
            }
            DefWindowProcW(hwnd, msg, wp, lp)
        }
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let dc = BeginPaint(hwnd, &mut ps);
            if let Ok(g) = ST.lock() {
                if let Some(s) = g.as_ref() {
                    let mut r = RECT::default();
                    let _ = GetClientRect(hwnd, &mut r);
                    SetBkMode(dc, TRANSPARENT);
                    let old = SelectObject(dc, s.font);
                    SetTextColor(dc, COLORREF(0xf5f5f5));
                    let mut tr = RECT { left: 16, top: 12, right: r.right - 16, bottom: 40 };
                    DrawTextW(dc, &mut s.title.clone(), &mut tr, DT_LEFT | DT_SINGLELINE | DT_END_ELLIPSIS);
                    SelectObject(dc, s.small);
                    SetTextColor(dc, COLORREF(0x9a9a9a));
                    let mut hr = RECT { left: 16, top: r.bottom - 26, right: r.right - 16, bottom: r.bottom - 6 };
                    DrawTextW(dc, &mut s.hint.clone(), &mut hr, DT_LEFT | DT_SINGLELINE);
                    SelectObject(dc, old);
                }
            }
            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        }
        WM_CTLCOLOREDIT => {
            if let Some(b) = ST.lock().ok().and_then(|g| g.as_ref().map(|s| s.edit_bg)) {
                let dc = HDC(wp.0 as *mut _);
                SetTextColor(dc, COLORREF(0xf5f5f5));
                SetBkColor(dc, COLORREF(0x1c1a18));
                return LRESULT(b.0 as isize);
            }
            DefWindowProcW(hwnd, msg, wp, lp)
        }
        WM_ACTIVATE => {
            if (wp.0 & 0xffff) as u32 == WA_INACTIVE {
                let _ = DestroyWindow(hwnd); // clicked elsewhere = cancel
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            if let Ok(mut g) = ST.lock() {
                if let Some(s) = g.as_mut() {
                    s.done = true;
                }
            }
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}
