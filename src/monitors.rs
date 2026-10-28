//! Which monitor the desk lives on. config.json `widgetsMonitor`: "primary" (default) | "cursor" (the monitor under the mouse when the
//! process started, then pinned) | a 1-based number (display order of `order`: primary first, then left to right) | a device name
//! (`\\.\DISPLAY2`, or the hardware key `<id>@<w>x<h>` of `MonInfo::key`, which `choice_for_position` stores: names and numbers move when a monitor is plugged in). A monitor that is gone falls back to the primary one
//! and the choice is honoured again when it returns. Everything but `enumerate` is pure (synthetic monitor lists in the tests).
use crate::widget::Lang;
use serde_json::Value;
use std::collections::HashSet;
use std::sync::Mutex;
use windows::core::PCWSTR;
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
    /// the hardware identity from the device interface path (`DELA0A8#5&1a2b3c&0&UID4352`: model code + the output it hangs on), empty when
    /// Windows does not say. Unlike `name` it does not renumber when another monitor is plugged in.
    pub id: String,
}

impl MonInfo {
    pub fn size(&self) -> (i32, i32) {
        (self.rect.right - self.rect.left, self.rect.bottom - self.rect.top)
    }

    /// The key a layout is stored under (`monitorLayouts`, `widgetsMonitor`): `<id>@<width>x<height>`, the device name when there is no id.
    pub fn key(&self) -> String {
        if self.id.is_empty() {
            self.name.clone()
        } else {
            format!("{}@{}x{}", self.id, self.size().0, self.size().1)
        }
    }
}

/// `<id>@<w>x<h>` -> (id, size); anything else (a device name) -> (the whole key, None).
fn split_key(k: &str) -> (&str, Option<(i32, i32)>) {
    let parsed = k.rsplit_once('@').and_then(|(id, wh)| {
        let (w, h) = wh.split_once('x')?;
        Some((id, (w.parse().ok()?, h.parse().ok()?)))
    });
    match parsed {
        Some((id, wh)) => (id, Some(wh)),
        None => (k, None),
    }
}

/// `DELA0A8#5&1a2b3c&0&UID4352` out of a device interface path (`\\?\DISPLAY#DELA0A8#5&1a2b3c&0&UID4352#{guid}`).
pub fn hw_id(interface_path: &str) -> Option<String> {
    let mut p = interface_path.split('#');
    let (_, model, inst) = (p.next()?, p.next()?, p.next()?);
    (!model.is_empty() && !inst.is_empty()).then(|| format!("{model}#{inst}"))
}

/// How well the stored key `k` names monitor `m`: 0 = exactly its key, 1 = its (old style) device name, 2 = the same hardware id but another
/// resolution, 3 = the same model at the same resolution on another output (the cable moved).
fn rank(k: &str, m: &MonInfo) -> Option<u8> {
    let key = m.key();
    if k.eq_ignore_ascii_case(&key) {
        return Some(0);
    }
    if k.eq_ignore_ascii_case(&m.name) {
        return Some(1);
    }
    let (id, wh) = split_key(k);
    if m.id.is_empty() || wh.is_none() {
        return None;
    }
    if id.eq_ignore_ascii_case(&m.id) {
        return Some(2);
    }
    let model = |s: &str| s.split('#').next().unwrap_or("").to_ascii_lowercase();
    (model(id) == model(&m.id) && wh == Some(m.size())).then_some(3)
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
    let by_name = |n: &str| list.iter().position(|m| matches!(rank(n, m), Some(0 | 1)));
    match choice {
        Choice::Primary => primary_idx(list),
        Choice::Name(n) => by_name(n).or_else(|| primary_idx(list)),
        Choice::Index(i) => order(list).get(i.saturating_sub(1)).copied().or_else(|| primary_idx(list)),
        Choice::Cursor => {
            if pin.is_none() {
                *pin = cursor.and_then(|p| at_point(list, p)).map(|i| list[i].key());
            }
            pin.as_deref().and_then(by_name).or_else(|| primary_idx(list))
        }
    }
}

/// The layout slot of monitor `i`: None = `widgetsLayout` (the primary monitor), else its key in `monitorLayouts`.
pub fn slot_of(list: &[MonInfo], i: usize) -> Option<String> {
    list.get(i).filter(|m| !m.primary).map(|m| m.key())
}

/// The `widgetsMonitor` value for picking display position `pos`: the primary one is stored as "primary" (follows a changed primary),
/// the others by key.
pub fn choice_for_position(list: &[MonInfo], pos: usize) -> Option<Choice> {
    let i = *order(list).get(pos)?;
    Some(if list[i].primary { Choice::Primary } else { Choice::Name(list[i].key()) })
}

// ---------------------------------------------------------------- stored keys follow the hardware

/// Every monitor key config.json refers to: the keys of `monitorLayouts` / `monitorProfiles`, a `widgetsMonitor` that names a monitor and the
/// `widgetsLayoutMonitor` owner of the primary layout.
fn stored_keys(root: &Value) -> Vec<String> {
    let mut v: Vec<String> = ["monitorLayouts", "monitorProfiles"].iter().filter_map(|k| root.get(k)?.as_object()).flat_map(|m| m.keys().cloned()).collect();
    for k in ["widgetsMonitor", "widgetsLayoutMonitor"] {
        if let Some(s) = root.get(k).and_then(|s| s.as_str()) {
            if matches!(Choice::parse(s), Choice::Name(_)) || k == "widgetsLayoutMonitor" {
                v.push(s.to_string());
            }
        }
    }
    v.sort();
    v.dedup();
    v
}

/// Rename key `from` to `to` everywhere it is stored; an entry whose new name is taken stays where it is (nothing is ever overwritten).
fn rename_key(root: &mut Value, from: &str, to: &str) -> bool {
    let mut changed = false;
    let Some(obj) = root.as_object_mut() else { return false };
    for k in ["monitorLayouts", "monitorProfiles"] {
        if let Some(m) = obj.get_mut(k).and_then(|m| m.as_object_mut()) {
            if !m.contains_key(to) {
                if let Some(v) = m.remove(from) {
                    m.insert(to.to_string(), v);
                    changed = true;
                }
            }
        }
    }
    for k in ["widgetsMonitor", "widgetsLayoutMonitor"] {
        if obj.get(k).and_then(|s| s.as_str()) == Some(from) {
            obj.insert(k.into(), Value::String(to.to_string()));
            changed = true;
        }
    }
    changed
}

/// Bring the monitor keys stored in config.json up to date with the monitors that are connected, so each monitor keeps its own widgets across
/// renumbering, a changed resolution or a cable moved to another output. Pure (a parsed config + a monitor list); true = `root` changed.
/// * a stored key that names a connected monitor less exactly than its current key (old `\\.\DISPLAYn` name, same hardware at another
///   resolution, same model + resolution on another output) is renamed to that key, but only when exactly one free monitor fits;
/// * `widgetsLayoutMonitor` says whose layout `widgetsLayout` is (the primary monitor's, for old configs: the primary one at the first run). When the
///   primary changed to another CONNECTED monitor the two layouts (and active profiles) trade places; when the owner is away the primary just
///   keeps using `widgetsLayout`, as before.
pub fn migrate(root: &mut Value, list: &[MonInfo]) -> bool {
    if !root.as_object().is_some_and(|o| !o.is_empty()) || list.is_empty() {
        return false;
    }
    let mut changed = false;
    // 1. keys
    let keys = stored_keys(root);
    let mut taken: HashSet<usize> = keys.iter().filter_map(|k| list.iter().position(|m| rank(k, m) == Some(0))).collect();
    for want in [1u8, 2, 3] {
        for k in keys.iter().filter(|k| !list.iter().any(|m| rank(k, m) == Some(0))) {
            let fit: Vec<usize> = (0..list.len()).filter(|i| !taken.contains(i) && rank(k, &list[*i]) == Some(want)).collect();
            if let [i] = fit[..] {
                if rename_key(root, k, &list[i].key()) {
                    taken.insert(i);
                    changed = true;
                }
            }
        }
    }
    // 2. who owns `widgetsLayout`
    let Some(p) = list.iter().find(|m| m.primary).or(list.first()) else { return changed };
    let pk = p.key();
    let owner = root.get("widgetsLayoutMonitor").and_then(|s| s.as_str()).map(str::to_string);
    match owner {
        None => {
            root["widgetsLayoutMonitor"] = Value::String(pk);
            changed = true;
        }
        Some(o) if o.eq_ignore_ascii_case(&pk) => {}
        Some(o) if list.iter().any(|m| !m.primary && m.key().eq_ignore_ascii_case(&o)) && swap_primary(root, &o, &pk) => changed = true,
        Some(_) => {}
    }
    changed
}

/// The primary layout (and active profile) go to the slot of `owner`, the layout stored for `new` (if any) becomes `widgetsLayout`.
fn swap_primary(root: &mut Value, owner: &str, new: &str) -> bool {
    let Some(obj) = root.as_object_mut() else { return false };
    let free = |obj: &serde_json::Map<String, Value>, k: &str| match obj.get(k) {
        None => true,
        Some(m) => m.as_object().is_some_and(|m| !m.contains_key(owner)),
    };
    if !free(obj, "monitorLayouts") || !free(obj, "monitorProfiles") {
        return false; // the old owner already has a layout of its own, or the table is damaged: leave everything as it is
    }
    for (slot_key, primary_key) in [("monitorLayouts", "widgetsLayout"), ("monitorProfiles", "activeProfile")] {
        let mine = obj.remove(primary_key);
        let theirs = obj.get_mut(slot_key).and_then(|m| m.as_object_mut()).and_then(|m| m.remove(new));
        if let Some(v) = mine {
            if let Some(m) = obj.entry(slot_key.to_string()).or_insert_with(|| Value::Object(Default::default())).as_object_mut() {
                m.insert(owner.to_string(), v);
            }
        }
        if let Some(v) = theirs {
            obj.insert(primary_key.into(), v);
        }
        if obj.get(slot_key).and_then(|m| m.as_object()).is_some_and(|m| m.is_empty()) {
            obj.remove(slot_key);
        }
    }
    obj.insert("widgetsLayoutMonitor".into(), Value::String(new.to_string()));
    true
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
            // the monitor attached to this output, by its device interface path (EDD_GET_DEVICE_INTERFACE_NAME)
            let mut dd = DISPLAY_DEVICEW { cb: std::mem::size_of::<DISPLAY_DEVICEW>() as u32, ..Default::default() };
            let id = if EnumDisplayDevicesW(PCWSTR(mi.szDevice.as_ptr()), 0, &mut dd, 1).as_bool() {
                let k = dd.DeviceID.iter().position(|&c| c == 0).unwrap_or(dd.DeviceID.len());
                hw_id(&String::from_utf16_lossy(&dd.DeviceID[..k])).unwrap_or_default()
            } else {
                String::new()
            };
            out.push(MonInfo { name: String::from_utf16_lossy(&mi.szDevice[..n]), rect: mi.monitorInfo.rcMonitor, work: mi.monitorInfo.rcWork, primary: mi.monitorInfo.dwFlags & 1 != 0, hmon: h.0 as isize, id });
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
        MonInfo { name: name.into(), rect, work: rect, primary, hmon: 0, id: String::new() }
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

    const DELL: &str = "DELA0A8#5&1a2b3c&0&UID4352";
    const LG: &str = "GSM5B09#5&1a2b3c&0&UID4353";
    /// A monitor with a hardware id at `w` x `h`, `x` px from the left.
    fn hw(name: &str, id: &str, x: i32, w: i32, h: i32, primary: bool) -> MonInfo {
        MonInfo { id: id.into(), ..m(name, r(x, 0, w, h), primary) }
    }

    #[test]
    fn hardware_id_and_key() {
        assert_eq!(hw_id(r"\\?\DISPLAY#DELA0A8#5&1a2b3c&0&UID4352#{e6f07b5f-ee97-4a90-b076-33f57bf4eaa7}").as_deref(), Some(DELL));
        assert_eq!(hw_id(r"\\?\DISPLAY#Default_Monitor#1&0&UID0#{x}").as_deref(), Some("Default_Monitor#1&0&UID0"));
        for bad in ["", "DISPLAY", r"\\?\DISPLAY#", r"\\?\DISPLAY#DELA0A8", r"\\?\DISPLAY##inst#{x}"] {
            assert_eq!(hw_id(bad), None, "{bad}");
        }
        let d = hw(r"\\.\DISPLAY2", DELL, 0, 2560, 1440, false);
        assert_eq!(d.key(), format!("{DELL}@2560x1440"));
        assert_eq!(m(r"\\.\DISPLAY2", r(0, 0, 1, 1), false).key(), r"\\.\DISPLAY2"); // no id: the device name
        assert_eq!(split_key(&d.key()), (DELL, Some((2560, 1440))));
        assert_eq!(split_key(r"\\.\DISPLAY2"), (r"\\.\DISPLAY2", None));
        // a plugged-in monitor renumbering the outputs does not change the key
        assert_eq!(hw(r"\\.\DISPLAY5", DELL, 0, 2560, 1440, false).key(), d.key());
        // rank: exact, old device name, same hardware at another resolution, same model + size on another output
        assert_eq!(rank(&d.key(), &d), Some(0));
        assert_eq!(rank(&d.key().to_uppercase(), &d), Some(0));
        assert_eq!(rank(r"\\.\display2", &d), Some(1));
        assert_eq!(rank(&format!("{DELL}@1920x1080"), &d), Some(2));
        assert_eq!(rank("DELA0A8#5&ffff&0&UID9@2560x1440", &d), Some(3));
        assert_eq!(rank("DELA0A8#5&ffff&0&UID9@1920x1080", &d), None);
        assert_eq!(rank(&format!("{LG}@2560x1440"), &d), None);
        assert_eq!(rank(r"\\.\DISPLAY3", &d), None);
    }

    #[test]
    fn slots_use_the_hardware_key_and_the_old_name_still_resolves() {
        let l = vec![hw(r"\\.\DISPLAY1", LG, 0, 1920, 1080, true), hw(r"\\.\DISPLAY7", DELL, 1920, 2560, 1440, false)];
        let key = format!("{DELL}@2560x1440");
        assert_eq!(slot_of(&l, 1).as_deref(), Some(key.as_str()));
        assert_eq!(choice_for_position(&l, 1), Some(Choice::Name(key.clone())));
        let mut pin = None;
        assert_eq!(resolve(&l, &Choice::Name(key.to_lowercase()), &mut pin, None), Some(1));
        assert_eq!(resolve(&l, &Choice::Name(r"\\.\DISPLAY7".into()), &mut pin, None), Some(1)); // an unmigrated config
        assert_eq!(resolve(&l, &Choice::Name(format!("{DELL}@800x600")), &mut pin, None), Some(0)); // unknown: primary
        // a cursor pin keeps the key, not the (renumbering) device name
        assert_eq!(resolve(&l, &Choice::Cursor, &mut pin, Some((2000, 10))), Some(1));
        assert_eq!(pin.as_deref(), Some(key.as_str()));
    }

    fn two() -> Vec<MonInfo> {
        vec![hw(r"\\.\DISPLAY1", LG, 0, 1920, 1080, true), hw(r"\\.\DISPLAY2", DELL, 1920, 2560, 1440, false)]
    }

    #[test]
    fn migration_gives_an_old_config_to_the_primary_and_renames_device_names() {
        // old single-layout config: nothing but the primary's layout, plus a layout stored under the old device name
        let mut v = json!({"widgetsLayout": [{"id": "c1"}], "activeProfile": "Work", "profiles": {"Work": {}},
            "monitorLayouts": {r"\\.\DISPLAY2": [{"id": "d1"}]}, "monitorProfiles": {r"\\.\DISPLAY2": "Work"}, "widgetsMonitor": r"\\.\DISPLAY2", "widgetsGlass": 0.4});
        assert!(migrate(&mut v, &two()));
        let (lg, dell) = (format!("{LG}@1920x1080"), format!("{DELL}@2560x1440"));
        assert_eq!(v["widgetsLayoutMonitor"], lg);
        assert_eq!(v["widgetsLayout"], json!([{"id": "c1"}])); // the primary's layout did not move
        assert_eq!(v["monitorLayouts"], json!({ dell.clone(): [{"id": "d1"}] }));
        assert_eq!(v["monitorProfiles"], json!({ dell.clone(): "Work" }));
        assert_eq!(v["widgetsMonitor"], dell);
        assert_eq!((v["widgetsGlass"].as_f64(), v["activeProfile"].as_str()), (Some(0.4), Some("Work")));
        assert!(!migrate(&mut v, &two()), "idempotent");
        // an empty / missing config is left alone (no file is invented), as is an empty monitor list
        let mut e = json!({});
        assert!(!migrate(&mut e, &two()) && e == json!({}));
        assert!(!migrate(&mut v, &[]));
        // a broken table is not touched
        let mut bad = json!({"monitorLayouts": 5, "widgetsLayoutMonitor": "gone"});
        assert!(!migrate(&mut bad, &two()));
        assert_eq!(bad, json!({"monitorLayouts": 5, "widgetsLayoutMonitor": "gone"}));
    }

    #[test]
    fn migration_follows_a_resolution_change_and_a_moved_cable() {
        let old = format!("{DELL}@1920x1080");
        let mut v = json!({"widgetsLayoutMonitor": format!("{LG}@1920x1080"), "monitorLayouts": { old.clone(): [{"id": "d"}] }, "widgetsMonitor": old.clone()});
        assert!(migrate(&mut v, &two())); // same hardware, now 2560x1440
        assert_eq!(v["monitorLayouts"], json!({ format!("{DELL}@2560x1440"): [{"id": "d"}] }));
        assert_eq!(v["widgetsMonitor"], format!("{DELL}@2560x1440"));
        // the cable moved to another output: same model, same size, other instance
        let mut v = json!({"widgetsLayoutMonitor": format!("{LG}@1920x1080"), "monitorLayouts": {"DELA0A8#5&9z9z&0&UID1111@2560x1440": [{"id": "d"}]}});
        assert!(migrate(&mut v, &two()));
        assert_eq!(v["monitorLayouts"], json!({ format!("{DELL}@2560x1440"): [{"id": "d"}] }));
        // two identical candidates: ambiguous, nothing is guessed
        let twin = vec![hw(r"\\.\DISPLAY1", DELL, 0, 2560, 1440, true), hw(r"\\.\DISPLAY2", "DELA0A8#5&1a2b3c&0&UID7777", 2560, 2560, 1440, false)];
        let mut v = json!({"widgetsLayoutMonitor": format!("{LG}@1920x1080"), "monitorLayouts": {"DELA0A8#5&0000&0&UID0@2560x1440": [{"id": "x"}]}});
        assert!(!migrate(&mut v, &twin));
        assert!(v["monitorLayouts"].get("DELA0A8#5&0000&0&UID0@2560x1440").is_some());
    }

    #[test]
    fn migration_never_steals_a_connected_monitors_key_or_overwrites() {
        // the stored key of an absent twin must not move onto a monitor that already has its own layout
        let twin = vec![hw(r"\\.\DISPLAY1", LG, 0, 1920, 1080, true), hw(r"\\.\DISPLAY2", DELL, 1920, 2560, 1440, false)];
        let own = format!("{DELL}@2560x1440");
        let mut v = json!({"widgetsLayoutMonitor": format!("{LG}@1920x1080"), "monitorLayouts": { own.clone(): [{"id": "mine"}], "DELA0A8#5&0000&0&UID0@2560x1440": [{"id": "away"}] }});
        assert!(!migrate(&mut v, &twin));
        assert_eq!(v["monitorLayouts"][own.as_str()], json!([{"id": "mine"}]));
        assert_eq!(v["monitorLayouts"]["DELA0A8#5&0000&0&UID0@2560x1440"], json!([{"id": "away"}]));
        // the destination name is taken: the old entry stays
        let mut v = json!({"widgetsLayoutMonitor": format!("{LG}@1920x1080"), "monitorLayouts": {r"\\.\DISPLAY2": [{"id": "old"}], own.clone(): [{"id": "new"}]}});
        assert!(!migrate(&mut v, &twin));
        assert_eq!(v["monitorLayouts"].as_object().unwrap().len(), 2);
    }

    #[test]
    fn a_new_primary_trades_layouts_with_the_old_owner() {
        let (lg, dell) = (format!("{LG}@1920x1080"), format!("{DELL}@2560x1440"));
        let mut v = json!({"widgetsLayoutMonitor": lg.clone(), "widgetsLayout": [{"id": "lg"}], "activeProfile": "A", "monitorLayouts": { dell.clone(): [{"id": "dell"}] }, "monitorProfiles": { dell.clone(): "B" }});
        // the user makes the Dell the primary one
        let swapped = vec![hw(r"\\.\DISPLAY2", LG, 2560, 1920, 1080, false), hw(r"\\.\DISPLAY1", DELL, 0, 2560, 1440, true)];
        assert!(migrate(&mut v, &swapped));
        assert_eq!(v["widgetsLayout"], json!([{"id": "dell"}]));
        assert_eq!(v["activeProfile"], "B");
        assert_eq!(v["monitorLayouts"], json!({ lg.clone(): [{"id": "lg"}] }));
        assert_eq!(v["monitorProfiles"], json!({ lg.clone(): "A" }));
        assert_eq!(v["widgetsLayoutMonitor"], dell);
        assert!(!migrate(&mut v, &swapped));
        // and back
        assert!(migrate(&mut v, &two()));
        assert_eq!(v["widgetsLayout"], json!([{"id": "lg"}]));
        assert_eq!(v["monitorLayouts"], json!({ dell.clone(): [{"id": "dell"}] }));
        // the new primary has no layout yet: it starts from the default one (no widgetsLayout), the old one is kept under the old owner
        let mut v = json!({"widgetsLayoutMonitor": lg.clone(), "widgetsLayout": [{"id": "lg"}]});
        assert!(migrate(&mut v, &swapped));
        assert!(v.get("widgetsLayout").is_none() && v.get("monitorLayouts").is_some());
        assert_eq!(v["monitorLayouts"], json!({ lg.clone(): [{"id": "lg"}] }));
        // the owner is not connected (undocked): the primary keeps using widgetsLayout, the owner stays recorded
        let mut v = json!({"widgetsLayoutMonitor": lg.clone(), "widgetsLayout": [{"id": "lg"}]});
        let alone = vec![hw(r"\\.\DISPLAY1", DELL, 0, 2560, 1440, true)];
        assert!(!migrate(&mut v, &alone));
        assert_eq!((v["widgetsLayoutMonitor"].as_str(), v["widgetsLayout"].clone()), (Some(lg.as_str()), json!([{"id": "lg"}])));
        // the old owner already has a layout of its own: no swap, nothing lost
        let mut v = json!({"widgetsLayoutMonitor": lg.clone(), "widgetsLayout": [{"id": "p"}], "monitorLayouts": { lg.clone(): [{"id": "own"}], dell.clone(): [{"id": "dell"}] }});
        assert!(!migrate(&mut v, &swapped));
        assert_eq!(v["widgetsLayout"], json!([{"id": "p"}]));
    }
}
