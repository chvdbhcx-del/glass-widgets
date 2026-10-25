//! Notification-area (tray) icon: a small glass tile drawn in code, left click = settings, right click = the menu below. The icon is
//! re-added when Explorer restarts (`TaskbarCreated`). The host owns the window; this module only knows the Win32 plumbing.
use crate::widget::Lang;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{BOOL, HWND, POINT};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;

/// The notification callback message of the tray icon (`lParam` = the mouse message).
pub const WM_TRAY: u32 = WM_APP + 4;
const ICON_ID: u32 = 1;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TrayCmd {
    None,
    Settings,
    Edit,
    ToggleVisible,
    ToggleAutostart,
    /// "Layouts" submenu: switch to the n-th saved profile (the list in `MenuState::profiles`)
    Profile(usize),
    SaveProfile,
    DeleteProfile,
    Quit,
}

/// What the menu needs to know to label and check its items.
pub struct MenuState {
    pub lang: Lang,
    pub editing: bool,
    pub hidden: bool,
    pub autostart: bool,
    /// saved layout profiles (config `profiles`) and the checked one (`activeProfile`)
    pub profiles: Vec<String>,
    pub active: Option<usize>,
}

/// The id of the broadcast Explorer sends after it (re)creates the taskbar: the icon must be added again.
pub fn taskbar_created_msg() -> u32 {
    unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) }
}

/// Add (or re-add) the icon. Returns false when the shell refused it.
pub unsafe fn add(hwnd: HWND) -> bool {
    let size = GetSystemMetrics(SM_CXSMICON).clamp(16, 64);
    let Some(icon) = make_icon(size) else { return false };
    let mut nid = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: ICON_ID,
        uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
        uCallbackMessage: WM_TRAY,
        hIcon: icon,
        ..Default::default()
    };
    for (d, s) in nid.szTip.iter_mut().zip("Glass Widgets".encode_utf16()) {
        *d = s;
    }
    let ok = Shell_NotifyIconW(NIM_ADD, &nid).as_bool() || Shell_NotifyIconW(NIM_MODIFY, &nid).as_bool();
    let _ = DestroyIcon(icon); // the shell keeps its own copy
    ok
}

pub unsafe fn remove(hwnd: HWND) {
    let nid = NOTIFYICONDATAW { cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32, hWnd: hwnd, uID: ICON_ID, ..Default::default() };
    let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
}

const C_SETTINGS: usize = 1;
const C_EDIT: usize = 2;
const C_VISIBLE: usize = 3;
const C_AUTOSTART: usize = 4;
const C_QUIT: usize = 5;
const C_SAVE_PROFILE: usize = 6;
/// the "Layouts" submenu entry of the main menu (a popup, no command id of its own)
const C_LAYOUTS: usize = 8;
const C_DELETE_PROFILE: usize = 7;
/// + the index of the profile; at most `MAX_PROFILES` are listed
const C_PROFILE: usize = 100;
const MAX_PROFILES: usize = 40;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

/// The right-click menu (modal: call it with the App NOT borrowed). `foreground` makes `hwnd` the foreground window first, which
/// a popup needs to close when the user clicks elsewhere.
pub unsafe fn run_menu(hwnd: HWND, st: &MenuState, foreground: impl Fn(HWND)) -> TrayCmd {
    let t = |ru: &'static str, en: &'static str| wide(st.lang.pick(ru, en));
    let Ok(menu) = CreatePopupMenu() else { return TrayCmd::None };
    // the "Layouts" submenu: the saved profiles (check mark on the active one), then save / delete
    let layouts = CreatePopupMenu().ok();
    if let Some(sub) = layouts {
        for (i, name) in st.profiles.iter().take(MAX_PROFILES).enumerate() {
            let flags = MF_STRING | if st.active == Some(i) { MF_CHECKED } else { MF_UNCHECKED };
            let _ = AppendMenuW(sub, flags, C_PROFILE + i, PCWSTR(wide(&name.replace('&', "&&")).as_ptr()));
        }
        if !st.profiles.is_empty() {
            let _ = AppendMenuW(sub, MF_SEPARATOR, 0, PCWSTR::null());
        }
        let _ = AppendMenuW(sub, MF_STRING, C_SAVE_PROFILE, PCWSTR(t("Сохранить текущий макет как…", "Save current layout as…").as_ptr()));
        let del = MF_STRING | if st.active.is_some() { MF_ENABLED } else { MF_GRAYED };
        let _ = AppendMenuW(sub, del, C_DELETE_PROFILE, PCWSTR(t("Удалить текущий макет", "Delete current layout").as_ptr()));
    }
    let items = [
        (C_SETTINGS, t("Настройки…", "Settings…"), false),
        (C_EDIT, if st.editing { t("Готово", "Done") } else { t("Изменить виджеты…", "Edit Widgets…") }, false),
        (C_VISIBLE, if st.hidden { t("Показать виджеты", "Show Widgets") } else { t("Скрыть виджеты", "Hide Widgets") }, false),
        (C_LAYOUTS, t("Макеты", "Layouts"), false),
        (0, Vec::new(), false),
        (C_AUTOSTART, t("Запускать вместе с Windows", "Start with Windows"), st.autostart),
        (0, Vec::new(), false),
        (C_QUIT, t("Выход", "Quit"), false),
    ];
    for (id, label, checked) in &items {
        if *id == 0 {
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        } else if *id == C_LAYOUTS {
            if let Some(sub) = layouts {
                let _ = AppendMenuW(menu, MF_POPUP, sub.0 as usize, PCWSTR(label.as_ptr()));
            }
        } else {
            let flags = MF_STRING | if *checked { MF_CHECKED } else { MF_UNCHECKED };
            let _ = AppendMenuW(menu, flags, *id, PCWSTR(label.as_ptr()));
        }
    }
    let _ = SetMenuDefaultItem(menu, C_SETTINGS as u32, 0);
    let mut p = POINT::default();
    let _ = GetCursorPos(&mut p);
    foreground(hwnd);
    let cmd = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_NONOTIFY | TPM_BOTTOMALIGN, p.x, p.y, 0, hwnd, None).0 as usize;
    let _ = PostMessageW(hwnd, WM_NULL, windows::Win32::Foundation::WPARAM(0), windows::Win32::Foundation::LPARAM(0));
    let _ = DestroyMenu(menu); // destroys the attached submenu too
    match cmd {
        C_SETTINGS => TrayCmd::Settings,
        C_EDIT => TrayCmd::Edit,
        C_VISIBLE => TrayCmd::ToggleVisible,
        C_AUTOSTART => TrayCmd::ToggleAutostart,
        C_SAVE_PROFILE => TrayCmd::SaveProfile,
        C_DELETE_PROFILE => TrayCmd::DeleteProfile,
        C_QUIT => TrayCmd::Quit,
        c if (C_PROFILE..C_PROFILE + MAX_PROFILES).contains(&c) => TrayCmd::Profile(c - C_PROFILE),
        _ => TrayCmd::None,
    }
}

// ---------------------------------------------------------------- the icon

/// A glass tile: rounded square, blue -> violet gradient, a light rim and a soft sheen on the upper half. Straight-alpha BGRA, `size`^2.
pub fn icon_pixels(size: i32) -> Vec<u8> {
    let n = size as f32;
    let (half, r) = (n / 2.0 - 0.5, n * 0.30);
    let mut px = vec![0u8; (size * size * 4) as usize];
    for y in 0..size {
        for x in 0..size {
            let (fx, fy) = (x as f32 + 0.5 - n / 2.0, y as f32 + 0.5 - n / 2.0);
            let (qx, qy) = (fx.abs() - (half - r), fy.abs() - (half - r));
            let d = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt() + qx.max(qy).min(0.0) - r;
            let cover = (0.5 - d).clamp(0.0, 1.0);
            if cover <= 0.0 {
                continue;
            }
            let t = ((x as f32 + y as f32) / (2.0 * n)).clamp(0.0, 1.0); // diagonal gradient
            let mut c = [96.0 + 54.0 * t, 190.0 - 80.0 * t, 255.0 - 5.0 * t]; // r, g, b: sky blue -> violet
            let sheen = (1.0 - (y as f32 / n) * 2.2).clamp(0.0, 1.0) * 0.38;
            let rim = ((d + 1.4) / 1.4).clamp(0.0, 1.0) * 0.55; // light band along the edge
            let wht = (sheen + rim).min(0.85);
            for v in &mut c {
                *v += (255.0 - *v) * wht;
            }
            let i = ((y * size + x) * 4) as usize;
            px[i] = c[2] as u8;
            px[i + 1] = c[1] as u8;
            px[i + 2] = c[0] as u8;
            px[i + 3] = (cover * 245.0) as u8;
        }
    }
    px
}

unsafe fn make_icon(size: i32) -> Option<HICON> {
    let px = icon_pixels(size);
    let bmi = BITMAPINFO { bmiHeader: BITMAPINFOHEADER { biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32, biWidth: size, biHeight: -size, biPlanes: 1, biBitCount: 32, biCompression: BI_RGB.0, ..Default::default() }, ..Default::default() };
    let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
    let dc = GetDC(None);
    let color = CreateDIBSection(dc, &bmi, DIB_RGB_COLORS, &mut bits, None, 0);
    ReleaseDC(None, dc);
    let color = color.ok()?;
    std::ptr::copy_nonoverlapping(px.as_ptr(), bits as *mut u8, px.len());
    let mask = CreateBitmap(size, size, 1, 1, None); // all zero: the colour bitmap's alpha does the shaping
    let info = ICONINFO { fIcon: BOOL(1), xHotspot: 0, yHotspot: 0, hbmMask: mask, hbmColor: color };
    let icon = CreateIconIndirect(&info).ok();
    let _ = DeleteObject(color);
    let _ = DeleteObject(mask);
    icon
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icon_is_a_rounded_tile() {
        let s = 32;
        let px = icon_pixels(s);
        let a = |x: i32, y: i32| px[((y * s + x) * 4 + 3) as usize];
        assert_eq!(a(0, 0), 0, "corner is transparent");
        assert!(a(16, 16) > 200, "centre is opaque");
        assert!(a(16, 1) > 100, "top edge is covered");
        // a 16 px icon renders too
        assert_eq!(icon_pixels(16).len(), 16 * 16 * 4);
    }
}
