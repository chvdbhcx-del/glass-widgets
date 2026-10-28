//! Win32 window helpers of the host: no `App` state, only arguments (split out of host.rs).
use super::*;

pub(super) fn same_rect(a: &RECT, b: &RECT) -> bool {
    (a.left, a.top, a.right, a.bottom) == (b.left, b.top, b.right, b.bottom)
}

pub(super) fn step_toward(v: f32, target: f32, d: f32) -> f32 {
    if v < target {
        (v + d).min(target)
    } else {
        (v - d).max(target)
    }
}

pub(super) fn cpu_time_100ns() -> u64 {
    unsafe {
        let (mut c, mut e, mut k, mut u) = (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
        let _ = GetProcessTimes(GetCurrentProcess(), &mut c, &mut e, &mut k, &mut u);
        let f = |t: FILETIME| ((t.dwHighDateTime as u64) << 32) | t.dwLowDateTime as u64;
        f(k) + f(u)
    }
}

/// Directly above the desktop host (Progman / the WorkerW owning SHELLDLL_DefView), below every app window.
pub(super) unsafe fn place_above_desktop(me: HWND) {
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
pub(super) unsafe fn app_in_front(me: HWND) -> bool {
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

/// The input desktop can be opened: false on the secure desktop (lock screen, UAC prompt).
pub(super) unsafe fn input_desktop_open() -> bool {
    match OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, DESKTOP_ACCESS_FLAGS(0x0100) /* DESKTOP_SWITCHDESKTOP */) {
        Ok(d) => {
            let _ = CloseDesktop(d);
            true
        }
        Err(_) => false,
    }
}

struct Cover<'a> {
    me: HWND,
    pid: u32,
    cards: &'a [RECT],
    out: Vec<RECT>,
}

unsafe extern "system" fn cover_cb(h: HWND, lp: LPARAM) -> BOOL {
    let c = &mut *(lp.0 as *mut Cover);
    if h == c.me {
        return BOOL(0); // everything above the layer is collected; what is below it cannot hide it
    }
    if !IsWindowVisible(h).as_bool() || IsIconic(h).as_bool() {
        return BOOL(1);
    }
    let mut r = RECT::default();
    if GetWindowRect(h, &mut r).is_err() || !c.cards.iter().any(|k| r.left < k.right && k.left < r.right && r.top < k.bottom && k.top < r.bottom) {
        return BOOL(1);
    }
    // from here on only for windows over a card; every doubt that a window really hides what is behind it leaves it out (=> awake)
    let ex = GetWindowLongW(h, GWL_EXSTYLE) as u32;
    if ex & (WS_EX_TRANSPARENT.0 | WS_EX_TOOLWINDOW.0 | WS_EX_NOACTIVATE.0) != 0 {
        return BOOL(1);
    }
    if ex & WS_EX_LAYERED.0 != 0 {
        // only a layered window with one constant, fully opaque alpha hides anything (per-pixel alpha: the call fails)
        let (mut key, mut alpha, mut flags) = (COLORREF(0), 0u8, LAYERED_WINDOW_ATTRIBUTES_FLAGS(0));
        if GetLayeredWindowAttributes(h, Some(&mut key), Some(&mut alpha), Some(&mut flags)).is_err() || flags.0 & LWA_COLORKEY.0 != 0 || (flags.0 & LWA_ALPHA.0 != 0 && alpha < 255) {
            return BOOL(1);
        }
    }
    if GetWindowRgnBox(h, &mut RECT::default()) != RGN_ERROR {
        return BOOL(1); // a shaped window
    }
    let mut pid = 0u32;
    GetWindowThreadProcessId(h, Some(&mut pid));
    let mut cloaked = 0u32;
    if pid == c.pid || windows::Win32::Graphics::Dwm::DwmGetWindowAttribute(h, windows::Win32::Graphics::Dwm::DWMWA_CLOAKED, &mut cloaked as *mut _ as *mut _, 4).is_ok() && cloaked != 0 {
        return BOOL(1);
    }
    let mut buf = [0u16; 64];
    let n = GetClassNameW(h, &mut buf);
    if matches!(String::from_utf16_lossy(&buf[..n.max(0) as usize]).as_str(), "Progman" | "WorkerW" | "Shell_TrayWnd" | "Shell_SecondaryTrayWnd") {
        return BOOL(1);
    }
    // the visible frame (without the invisible resize borders), when DWM knows it
    let mut f = RECT::default();
    if windows::Win32::Graphics::Dwm::DwmGetWindowAttribute(h, windows::Win32::Graphics::Dwm::DWMWA_EXTENDED_FRAME_BOUNDS, &mut f as *mut _ as *mut _, std::mem::size_of::<RECT>() as u32).is_ok() {
        r = f;
    }
    c.out.push(r);
    BOOL(1)
}

/// The rectangles (desktop px) of the windows above the layer in z-order that really hide what lies under them, limited to those that touch a
/// card: visible, not minimized, not cloaked (other virtual desktop), not ours, no tool / click-through / no-activate window, no
/// translucent or shaped one, not the shell's desktop / taskbar.
pub(super) unsafe fn covering_windows(me: HWND, cards: &[RECT]) -> Vec<RECT> {
    let mut c = Cover { me, pid: windows::Win32::System::Threading::GetCurrentProcessId(), cards, out: Vec::new() };
    let _ = EnumWindows(Some(cover_cb), LPARAM(&mut c as *mut _ as isize));
    c.out
}

/// Same logic as `foreground_is_fullscreen` in src-tauri/src/main.rs (the own window does not count). Only a fullscreen window on `ours`, the
/// monitor the desk lives on, counts: a game on the other monitor leaves the widgets alone.
pub(super) unsafe fn foreground_is_fullscreen(me: HWND, ours: &RECT) -> bool {
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
