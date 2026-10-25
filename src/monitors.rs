//! Which monitor the desk lives on. config.json `widgetsMonitor`: "primary" (default) | "cursor" (the monitor under the mouse when the
//! process started, then pinned) | a 1-based number (display order of `order`: primary first, then left to right) | a device name
//! (`\\.\DISPLAY2`, the stable key: numbers move when a monitor is plugged in). A monitor that is gone falls back to the primary one
//! and the choice is honoured again when it returns. Everything but `enumerate` is pure (synthetic monitor lists in the tests).
use crate::widget::Lang;
use serde_json::Value;
use std::sync::Mutex;
use windows::Win32::Foundation::{BOOL, LPARAM, RECT};
use windows::Win32::Graphics::Gdi::*;

#[derive(Clone, Debug, PartialEq)]
pub struct MonInfo {
    /// `MONITORINFOEXW::szDevice`, e.g. `\\.\DISPLAY2`
    pub name: String,
    pub rect: RECT,
    pub work: RECT,
    pub primary: bool,
    /// the HMONITOR (0 in synthetic lists)
    pub hmon: isize,
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub enum Choice {
    #[default]
    Primary,
    Cursor,
    /// 1-based position in `order`
    Index(usize),
    Name(String),
}

impl Choice {
    pub fn parse(s: &str) -> Choice {
        let t = s.trim();
        match t.to_ascii_lowercase().as_str() {
            "" | "primary" => Choice::Primary,
            "cursor" => Choice::Cursor,
            n if n.bytes().all(|b| b.is_ascii_digit()) => match n.parse::<usize>() {
                Ok(i) if i >= 1 => Choice::Index(i),
                _ => Choice::Primary,
            },
            _ => Choice::Name(t.to_string()),
        }
    }

    /// The `widgetsMonitor` value.
    pub fn key(&self) -> String {
        match self {
            Choice::Primary => "primary".into(),
            Choice::Cursor => "cursor".into(),
            Choice::Index(i) => i.to_string(),
            Choice::Name(n) => n.clone(),
        }
    }

    /// `widgetsMonitor` of a parsed config.json (anything but a string is "primary").
    pub fn from_value(root: &Value) -> Choice {
        root.get("widgetsMonitor").and_then(|v| v.as_str()).map_or(Choice::Primary, Choice::parse)
    }
}

/// Display order: the primary monitor first ("1"), the others by position (left, top) and name.
pub fn order(list: &[MonInfo]) -> Vec<usize> {
    let mut v: Vec<usize> = (0..list.len()).collect();
    v.sort_by(|&a, &b| {
        let (x, y) = (&list[a], &list[b]);
        y.primary.cmp(&x.primary).then((x.rect.left, x.rect.top).cmp(&(y.rect.left, y.rect.top))).then_with(|| x.name.cmp(&y.name))
    });
    v
}

/// 0-based position of monitor `i` in `order`.
pub fn position(list: &[MonInfo], i: usize) -> usize {
    order(list).iter().position(|&x| x == i).unwrap_or(0)
}

/// "1 (основной)" / "2" ...: the label of the monitor at display position `pos`.
pub fn label(list: &[MonInfo], pos: usize, lang: Lang) -> String {
    let primary = order(list).get(pos).is_some_and(|&i| list[i].primary);
    let n = pos + 1;
    if primary {
        format!("{n} ({})", lang.pick("основной", "primary"))
    } else {
        n.to_string()
    }
}

/// The monitor holding `pt`, else the nearest one (None only for an empty list).
pub fn at_point(list: &[MonInfo], pt: (i32, i32)) -> Option<usize> {
    let dist = |r: &RECT| {
        let dx = (r.left - pt.0).max(0).max(pt.0 - (r.right - 1)) as i64;
        let dy = (r.top - pt.1).max(0).max(pt.1 - (r.bottom - 1)) as i64;
        dx * dx + dy * dy
    };
    (0..list.len()).min_by_key(|&i| dist(&list[i].rect))
}

fn primary_idx(list: &[MonInfo]) -> Option<usize> {
    list.iter().position(|m| m.primary).or(if list.is_empty() { None } else { Some(0) })
}

/// The monitor a choice means (an index into `list`); the primary one when it does not exist (any more). `pin` keeps the monitor a
/// "cursor" choice resolved to (by device name) so the desk does not follow the mouse around on later rebuilds.
pub fn resolve(list: &[MonInfo], choice: &Choice, pin: &mut Option<String>, cursor: Option<(i32, i32)>) -> Option<usize> {
    let by_name = |n: &str| list.iter().position(|m| m.name.eq_ignore_ascii_case(n));
    match choice {
        Choice::Primary => primary_idx(list),
        Choice::Name(n) => by_name(n).or_else(|| primary_idx(list)),
        Choice::Index(i) => order(list).get(i.saturating_sub(1)).copied().or_else(|| primary_idx(list)),
        Choice::Cursor => {
            if pin.is_none() {
                *pin = cursor.and_then(|p| at_point(list, p)).map(|i| list[i].name.clone());
            }
            pin.as_deref().and_then(by_name).or_else(|| primary_idx(list))
        }
    }
}

/// The layout slot of monitor `i`: None = `widgetsLayout` (the primary monitor), else its device name in `monitorLayouts`.
pub fn slot_of(list: &[MonInfo], i: usize) -> Option<String> {
    list.get(i).filter(|m| !m.primary).map(|m| m.name.clone())
}

/// The `widgetsMonitor` value for picking display position `pos`: the primary one is stored as "primary" (follows a changed primary),
/// the others by device name.
pub fn choice_for_position(list: &[MonInfo], pos: usize) -> Option<Choice> {
    let i = *order(list).get(pos)?;
    Some(if list[i].primary { Choice::Primary } else { Choice::Name(list[i].name.clone()) })
}

// ---------------------------------------------------------------- process state + Win32

struct State {
    choice: Choice,
    pin: Option<String>,
}
static STATE: Mutex<State> = Mutex::new(State { choice: Choice::Primary, pin: None });

/// The choice in force (set from config.json at start and on every change).
pub fn set_choice(c: Choice) {
    if let Ok(mut g) = STATE.lock() {
        if g.choice != c {
            g.pin = None;
        }
        g.choice = c;
    }
}

pub fn choice() -> Choice {
    STATE.lock().map(|g| g.choice.clone()).unwrap_or_default()
}

/// Index into `list` of the monitor the desk belongs on right now.
pub fn pick(list: &[MonInfo]) -> Option<usize> {
    let cursor = unsafe {
        let mut p = windows::Win32::Foundation::POINT::default();
        windows::Win32::UI::WindowsAndMessaging::GetCursorPos(&mut p).ok().map(|_| (p.x, p.y))
    };
    let mut g = STATE.lock().ok()?;
    let State { choice, pin } = &mut *g;
    resolve(list, choice, pin, cursor)
}

/// The connected monitors (EnumDisplayMonitors + GetMonitorInfoW).
pub fn enumerate() -> Vec<MonInfo> {
    unsafe extern "system" fn cb(h: HMONITOR, _: HDC, _: *mut RECT, lp: LPARAM) -> BOOL {
        let out = &mut *(lp.0 as *mut Vec<MonInfo>);
        let mut mi = MONITORINFOEXW::default();
        mi.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        if GetMonitorInfoW(h, &mut mi as *mut MONITORINFOEXW as *mut MONITORINFO).as_bool() {
            let n = mi.szDevice.iter().position(|&c| c == 0).unwrap_or(mi.szDevice.len());
            out.push(MonInfo { name: String::from_utf16_lossy(&mi.szDevice[..n]), rect: mi.monitorInfo.rcMonitor, work: mi.monitorInfo.rcWork, primary: mi.monitorInfo.dwFlags & 1 != 0, hmon: h.0 as isize });
        }
        BOOL(1)
    }
    let mut v: Vec<MonInfo> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(None, None, Some(cb), LPARAM(&mut v as *mut Vec<MonInfo> as isize));
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn r(l: i32, t: i32, w: i32, h: i32) -> RECT {
        RECT { left: l, top: t, right: l + w, bottom: t + h }
    }
    fn m(name: &str, rect: RECT, primary: bool) -> MonInfo {
        MonInfo { name: name.into(), rect, work: rect, primary, hmon: 0 }
    }
    /// \\.\DISPLAY2 left of the primary (negative x), DISPLAY3 right of it, enumerated in an arbitrary order.
    fn three() -> Vec<MonInfo> {
        vec![m(r"\\.\DISPLAY3", r(1920, -200, 2560, 1440), false), m(r"\\.\DISPLAY1", r(0, 0, 1920, 1080), true), m(r"\\.\DISPLAY2", r(-1280, 100, 1280, 1024), false)]
    }

    #[test]
    fn choice_parses_config_values() {
        assert_eq!(Choice::parse("primary"), Choice::Primary);
        assert_eq!(Choice::parse("  "), Choice::Primary);
        assert_eq!(Choice::parse("Cursor"), Choice::Cursor);
        assert_eq!(Choice::parse("2"), Choice::Index(2));
        assert_eq!(Choice::parse("0"), Choice::Primary);
        assert_eq!(Choice::parse(r"\\.\DISPLAY2"), Choice::Name(r"\\.\DISPLAY2".into()));
        assert_eq!(Choice::from_value(&json!({})), Choice::Primary);
        assert_eq!(Choice::from_value(&json!({"widgetsMonitor": 3})), Choice::Primary);
        assert_eq!(Choice::from_value(&json!({"widgetsMonitor": "cursor"})), Choice::Cursor);
        for c in [Choice::Primary, Choice::Cursor, Choice::Index(4), Choice::Name(r"\\.\DISPLAY9".into())] {
            assert_eq!(Choice::parse(&c.key()), c);
        }
    }

    #[test]
    fn display_order_is_primary_then_left_to_right() {
        let l = three();
        assert_eq!(order(&l), [1, 2, 0]);
        assert_eq!((position(&l, 1), position(&l, 2), position(&l, 0)), (0, 1, 2));
        assert_eq!(label(&l, 0, Lang::Ru), "1 (основной)");
        assert_eq!(label(&l, 0, Lang::En), "1 (primary)");
        assert_eq!(label(&l, 2, Lang::Ru), "3");
        assert!(order(&[]).is_empty());
    }

    #[test]
    fn resolve_picks_and_falls_back_to_primary() {
        let l = three();
        let mut pin = None;
        assert_eq!(resolve(&l, &Choice::Primary, &mut pin, None), Some(1));
        assert_eq!(resolve(&l, &Choice::Name(r"\\.\display2".into()), &mut pin, None), Some(2)); // case-insensitive
        assert_eq!(resolve(&l, &Choice::Index(3), &mut pin, None), Some(0));
        assert_eq!(resolve(&l, &Choice::Index(2), &mut pin, None), Some(2));
        // gone: primary
        assert_eq!(resolve(&l, &Choice::Name(r"\\.\DISPLAY7".into()), &mut pin, None), Some(1));
        assert_eq!(resolve(&l, &Choice::Index(9), &mut pin, None), Some(1));
        assert_eq!(resolve(&[], &Choice::Name("x".into()), &mut pin, None), None);
        // hot-plug: the chosen monitor leaves, then returns
        let c = Choice::Name(r"\\.\DISPLAY3".into());
        let mut without = l.clone();
        without.remove(0);
        assert_eq!(resolve(&without, &c, &mut pin, None), Some(0)); // DISPLAY1 is first in the shortened list
        assert_eq!(without[0].name, r"\\.\DISPLAY1");
        assert_eq!(resolve(&l, &c, &mut pin, None), Some(0));
        // no flagged primary (should not happen): the first one
        let odd = vec![m("a", r(0, 0, 10, 10), false), m("b", r(10, 0, 10, 10), false)];
        assert_eq!(resolve(&odd, &Choice::Primary, &mut pin, None), Some(0));
    }

    #[test]
    fn cursor_choice_is_pinned() {
        let l = three();
        let mut pin = None;
        // the mouse is on the left monitor (negative coordinates)
        assert_eq!(resolve(&l, &Choice::Cursor, &mut pin, Some((-500, 300))), Some(2));
        assert_eq!(pin.as_deref(), Some(r"\\.\DISPLAY2"));
        // later the mouse is elsewhere: the desk stays
        assert_eq!(resolve(&l, &Choice::Cursor, &mut pin, Some((2000, 0))), Some(2));
        // the pinned monitor unplugged: primary, and back when it returns
        let without: Vec<MonInfo> = l.iter().filter(|x| x.name != r"\\.\DISPLAY2").cloned().collect();
        assert_eq!(without[resolve(&without, &Choice::Cursor, &mut pin, Some((0, 0))).unwrap()].name, r"\\.\DISPLAY1");
        assert_eq!(resolve(&l, &Choice::Cursor, &mut pin, None), Some(2));
        // no cursor position known: primary
        assert_eq!(resolve(&l, &Choice::Cursor, &mut None, None), Some(1));
    }

    #[test]
    fn point_lookup_includes_edges_gaps_and_negative_space() {
        let l = three();
        assert_eq!(at_point(&l, (0, 0)), Some(1));
        assert_eq!(at_point(&l, (1919, 1079)), Some(1));
        assert_eq!(at_point(&l, (-1, 500)), Some(2));
        assert_eq!(at_point(&l, (-1280, 100)), Some(2));
        assert_eq!(at_point(&l, (1920, 0)), Some(0));
        // outside every monitor: the nearest (below the left one / far right)
        assert_eq!(at_point(&l, (-1200, 5000)), Some(2));
        assert_eq!(at_point(&l, (99999, 0)), Some(0));
        assert_eq!(at_point(&[], (0, 0)), None);
    }

    #[test]
    fn slots_and_menu_choices() {
        let l = three();
        assert_eq!(slot_of(&l, 1), None); // the primary monitor keeps `widgetsLayout`
        assert_eq!(slot_of(&l, 2).as_deref(), Some(r"\\.\DISPLAY2"));
        assert_eq!(slot_of(&l, 9), None);
        assert_eq!(choice_for_position(&l, 0), Some(Choice::Primary));
        assert_eq!(choice_for_position(&l, 1), Some(Choice::Name(r"\\.\DISPLAY2".into())));
        assert_eq!(choice_for_position(&l, 2), Some(Choice::Name(r"\\.\DISPLAY3".into())));
        assert_eq!(choice_for_position(&l, 3), None);
    }
}
