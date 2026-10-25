//! Batteries: ring gauges with a device glyph inside and the percent below (S = 2 rings, M = up to 4), L = a list (glyph, name, bar, percent)
//! under a "Батареи" header. Port of src/widgets/batteries.js + src-tauri/src/devices.rs.
//! Data: this PC from `GetSystemPowerStatus` (a desktop without a battery shows a plug + "От сети"); Bluetooth devices from a hidden
//! PowerShell `Get-PnpDeviceProperty` scan (6 s timeout, only our own child is killed), cached process-wide for 60 s so several cards share one scan.
//! The scan runs in a worker (`cx.spawn`); until the first scan lands the tick polls every 3 s, afterwards every 30 s. Everything is static (`draw` only).
//! Settings: none. Previews only: `preview_state` = "desktop" | "low" selects an edge-case sample (no battery / red ring + missing levels).
use crate::registry::{Registry, WidgetDef};
use crate::widget::*;
use std::sync::Mutex;
use std::time::Instant;

pub fn register(r: &mut Registry) {
    r.register(WidgetDef {
        type_id: "batteries",
        name: ("Батареи", "Batteries"),
        desc: ("Заряд компьютера и подключённых устройств", "Charge of this PC and connected devices"),
        group: "batteries",
        icon: Glyph::Battery,
        sizes: &[Size::S, Size::M, Size::L],
        default_size: Size::M,
        make: || Box::new(Batteries::default()),
    });
}

// ================================================================ data

#[derive(Clone, Copy, Debug, PartialEq)]
struct Pc {
    has_battery: bool,
    percent: u8,
    charging: bool,
}

#[derive(Clone, Debug, PartialEq)]
struct Device {
    name: String,
    /// mouse | keyboard | headphones | gamepad | phone | other
    kind: &'static str,
    percent: Option<u8>,
    connected: bool,
}

/// One ring column / list row.
struct Item {
    name: String,
    glyph: Glyph,
    pct: Option<u8>,
    charging: bool,
    plug: bool,
    connected: bool,
}

fn read_pc() -> Pc {
    use windows::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
    let mut s = SYSTEM_POWER_STATUS::default();
    if unsafe { GetSystemPowerStatus(&mut s) }.is_err() {
        return Pc { has_battery: false, percent: 0, charging: false };
    }
    pc_from_flags(s.BatteryFlag, s.BatteryLifePercent)
}

/// BatteryFlag: 128 = no system battery, 255 = unknown, 8 = charging. Pure, tested.
fn pc_from_flags(flag: u8, life: u8) -> Pc {
    Pc { has_battery: flag & 128 == 0 && flag != 255, percent: if life <= 100 { life } else { 0 }, charging: flag & 8 != 0 }
}

/// Tiny name heuristic, first match wins. Pure, tested.
fn classify(name: &str) -> &'static str {
    let n = name.to_lowercase();
    let has = |ws: &[&str]| ws.iter().any(|w| n.contains(w));
    if has(&["mouse", "мыш", "mx master", "mx anywhere", "trackpad", "trackball"]) {
        "mouse"
    } else if has(&["keyboard", "клав", "keychron", "mx keys", "k380", "k780"]) {
        "keyboard"
    } else if has(&["headphone", "headset", "earbud", "earphone", "airpods", "buds", "наушн", "wh-1000", "wf-1000", "pods", "arctis", "hyperx cloud"]) {
        "headphones"
    } else if has(&["controller", "gamepad", "xbox", "dualsense", "dualshock", "joy-con", "pro controller", "джойст"]) {
        "gamepad"
    } else if has(&["iphone", "ipad", "phone", "galaxy", "pixel", "android", "redmi", "телефон"]) {
        "phone"
    } else {
        "other"
    }
}

/// Parse the PowerShell JSON (array, single object or empty) into devices, merged by name, connected first. Pure, tested.
fn parse_devices(json: &str) -> Vec<Device> {
    let s = json.trim().trim_start_matches('\u{feff}');
    let Ok(v) = serde_json::from_str::<Value>(s) else { return vec![] };
    let raws: Vec<&Value> = match &v {
        Value::Array(a) => a.iter().collect(),
        Value::Object(_) => vec![&v],
        _ => vec![],
    };
    let mut out: Vec<Device> = vec![];
    for r in raws {
        let name = r.get("name").and_then(|n| n.as_str()).unwrap_or("").trim().to_string();
        if name.is_empty() {
            continue;
        }
        let connected = r.get("present").and_then(|p| p.as_bool()).unwrap_or(false);
        let percent = r.get("battery").and_then(|b| b.as_u64()).filter(|p| *p <= 100).map(|p| p as u8);
        if let Some(d) = out.iter_mut().find(|d| d.name == name) {
            d.connected |= connected;
            d.percent = d.percent.or(percent);
        } else {
            out.push(Device { kind: classify(&name), name, percent, connected });
        }
    }
    // a disconnected device has no meaningful level
    for d in out.iter_mut().filter(|d| !d.connected) {
        d.percent = None;
    }
    out.sort_by_key(|d| !d.connected); // stable: connected first
    out
}

// Only real devices (instance id has DEV_), not adapters / enumerators.
const SCRIPT: &str = "[Console]::OutputEncoding=[Text.Encoding]::UTF8; $ProgressPreference='SilentlyContinue'; \
$r=@(Get-PnpDevice -Class Bluetooth -ErrorAction SilentlyContinue | ?{ $_.InstanceId -match 'DEV_' } | %{ \
$p=@(Get-PnpDeviceProperty -InstanceId $_.InstanceId -KeyName '{104EA319-6EE2-4701-BD47-8DDBF425BBE5} 2','DEVPKEY_Device_IsPresent' -ErrorAction SilentlyContinue); \
[pscustomobject]@{ id=$_.InstanceId; name=$_.FriendlyName; \
present=[bool]($p | ?{ $_.KeyName -eq 'DEVPKEY_Device_IsPresent' }).Data; \
battery=($p | ?{ $_.KeyName -like '{104EA319*' }).Data } }); \
ConvertTo-Json -InputObject $r -Compress";

/// Run the scan hidden; None on spawn failure or timeout (the child is killed, nothing else).
fn run_ps() -> Option<String> {
    use std::io::Read;
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};
    let mut child = Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", SCRIPT])
        .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut out = child.stdout.take()?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = out.read_to_end(&mut b);
        let _ = tx.send(b);
    });
    match rx.recv_timeout(Duration::from_secs(6)) {
        Ok(b) => {
            let _ = child.wait();
            Some(String::from_utf8_lossy(&b).into_owned())
        }
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            None
        }
    }
}

/// Process-wide cache shared by every Batteries card.
static CACHE: Mutex<Option<(Vec<Device>, Instant)>> = Mutex::new(None);
/// Serialises scans: a second card waits here, then finds the fresh cache.
static SCAN: Mutex<()> = Mutex::new(());

/// Worker job: the device list, from the 60 s cache or a fresh scan. A failed scan keeps the previous list (or an empty one: never "pending" forever).
fn fetch_devices() -> Vec<Device> {
    let _g = SCAN.lock().unwrap_or_else(|e| e.into_inner());
    let fresh = |c: &Option<(Vec<Device>, Instant)>| c.as_ref().filter(|(_, at)| at.elapsed() < Duration::from_secs(60)).map(|(d, _)| d.clone());
    if let Some(d) = fresh(&CACHE.lock().unwrap_or_else(|e| e.into_inner())) {
        return d;
    }
    let parsed = run_ps().map(|s| parse_devices(&s));
    let mut c = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let list = parsed.or_else(|| c.as_ref().map(|(d, _)| d.clone())).unwrap_or_default();
    *c = Some((list.clone(), Instant::now()));
    list
}

// ================================================================ widget

#[derive(Default)]
struct Batteries {
    started: bool,
    pc: Option<Pc>,
    /// None = the first scan has not landed yet
    devices: Option<Vec<Device>>,
    job: Option<Pending<Vec<Device>>>,
    last_job: Option<Instant>,
    ver: u64,
}

fn glyph_of(kind: &str) -> Glyph {
    match kind {
        "mouse" => Glyph::Mouse,
        "keyboard" => Glyph::Keyboard,
        "headphones" => Glyph::Headphones,
        "phone" => Glyph::Phone,
        _ => Glyph::Bluetooth, // ponytail: no gamepad glyph in icons.rs, Bluetooth stands in
    }
}

impl Batteries {
    /// The PC first, then the devices; rings show connected devices only, the list shows all.
    fn items(&self, cx: &Cx) -> Vec<Item> {
        let mut v = Vec::new();
        let pcname = cx.t("Компьютер", "This PC").to_string();
        match self.pc {
            Some(p) if p.has_battery => v.push(Item { name: pcname, glyph: Glyph::Laptop, pct: Some(p.percent), charging: p.charging, plug: false, connected: true }),
            Some(_) => v.push(Item { name: pcname, glyph: Glyph::Plug, pct: None, charging: false, plug: true, connected: true }),
            None => v.push(Item { name: pcname, glyph: Glyph::Laptop, pct: None, charging: false, plug: false, connected: true }),
        }
        let all = cx.size == Size::L;
        for d in self.devices.iter().flatten().filter(|d| all || d.connected) {
            v.push(Item { name: d.name.clone(), glyph: glyph_of(d.kind), pct: d.percent, charging: false, plug: false, connected: d.connected });
        }
        v.truncate(match cx.size {
            Size::S => 2,
            Size::M => 4,
            Size::L => 6,
        });
        v
    }
}

fn level_color(it: &Item) -> Color {
    if it.charging || it.pct.map_or(true, |p| p >= 20) {
        GREEN
    } else {
        RED
    }
}

fn pct_text(cx: &Cx, it: &Item) -> String {
    if it.plug {
        cx.t("От сети", "AC power").to_string()
    } else {
        it.pct.map_or("—".to_string(), |p| format!("{p}%"))
    }
}

/// Text with an optional green bolt in front, centred (or right-aligned when `right`) in `r`.
fn pct_label(cx: &Cx, r: Rect, it: &Item, st: Style, c: Color, right: bool) {
    let txt = pct_text(cx, it);
    if !it.charging {
        return cx.text(r, &txt, if right { st.right() } else { st.center() }, c);
    }
    let (tw, _) = cx.measure(&txt, st, 400.0);
    let (bw, gap) = (st.size * 1.1, 1.0);
    let total = bw + gap + tw;
    let x0 = if right { r.right() - total } else { r.cx() - total / 2.0 };
    cx.icon(Glyph::Bolt, Rect::new(x0, r.cy() - bw / 2.0, bw, bw), GREEN);
    cx.text(Rect::new(x0 + bw + gap, r.y, tw + 2.0, r.h), &txt, st, c);
}

/// One ring column: gauge with the glyph inside, the percent below. `c` = ring centre.
fn ring_item(cx: &Cx, c: Pt, it: &Item) {
    let r = 27.25; // 60 px outer diameter with the 5.5 stroke
    if it.plug {
        cx.ring(c, r, 5.5, 1.0, GREEN.fade(0.55));
    } else {
        cx.ring(c, r, 5.5, it.pct.map_or(0.0, |p| p as f32 / 100.0), level_color(it));
    }
    let gc = if it.plug { DIM } else if it.pct.is_none() { FAINT } else { TEXT };
    cx.icon(it.glyph, Rect::new(c.0 - 12.0, c.1 - 12.0, 24.0, 24.0), gc);
    let label = Rect::new(c.0 - 40.0, c.1 + r + 8.0, 80.0, 18.0);
    pct_label(cx, label, it, Style::new(13.0, 600), if it.pct.is_none() && !it.plug { DIM } else { TEXT }, false);
}

impl Widget for Batteries {
    fn draw(&mut self, cx: &mut Cx) -> Result<()> {
        let items = self.items(cx);
        if cx.size == Size::L {
            return self.draw_list(cx, &items);
        }
        let n = items.len();
        let colw = if cx.size == Size::S { 69.0 } else { 81.0 };
        let lone = n == 1; // only this PC: leave room for a hint underneath
        let top = (cx.h - 86.0) / 2.0 - if lone { 8.0 } else { 0.0 };
        let x0 = (cx.w - colw * n as f32) / 2.0;
        for (i, it) in items.iter().enumerate() {
            ring_item(cx, (x0 + colw * (i as f32 + 0.5), top + 30.0), it);
        }
        if lone {
            let hint = if self.devices.is_none() && !cx.preview { cx.t("Поиск устройств…", "Looking for devices…") } else { cx.t("Устройств Bluetooth нет", "No Bluetooth devices") };
            cx.text(Rect::new(16.0, cx.h - 36.0, cx.w - 32.0, 16.0), hint, Style::CAPTION.center().ellipsis(), DIM);
        }
        Ok(())
    }

    fn tick(&mut self, cx: &Cx) -> Tick {
        if cx.preview {
            if !self.started {
                self.started = true;
                let d = |n: &str, kind: &'static str, p: Option<u8>, on: bool| Device { name: n.to_string(), kind, percent: p, connected: on };
                // `preview_state`: "desktop" (no battery, no devices), "low" (red ring, a device without a level, a disconnected one)
                match cx.cfg_str("preview_state") {
                    Some("desktop") => {
                        self.pc = Some(Pc { has_battery: false, percent: 0, charging: false });
                        self.devices = Some(vec![]);
                    }
                    Some("low") => {
                        self.pc = Some(Pc { has_battery: true, percent: 14, charging: false });
                        self.devices = Some(vec![d("Magic Mouse", "mouse", Some(8), true), d("Xbox Controller", "gamepad", None, true), d(cx.t("Наушники", "Headphones"), "headphones", None, false)]);
                    }
                    _ => {
                        self.pc = Some(Pc { has_battery: true, percent: 82, charging: true });
                        self.devices = Some(vec![
                            d("Magic Mouse", "mouse", Some(64), true),
                            d(cx.t("Клавиатура", "Keyboard"), "keyboard", Some(100), true),
                            d(cx.t("Наушники", "Headphones"), "headphones", Some(23), true),
                        ]);
                    }
                }
                self.ver += 1;
            }
            return Tick::Never;
        }
        self.started = true;
        let pc = read_pc();
        if self.pc != Some(pc) {
            self.pc = Some(pc);
            self.ver += 1;
        }
        if let Some(j) = &self.job {
            if let Some(list) = j.take() {
                if self.devices.as_ref() != Some(&list) {
                    self.devices = Some(list);
                    self.ver += 1;
                }
                self.job = None;
            } else if !j.busy() {
                self.job = None; // the worker died without a result
                self.devices.get_or_insert_with(Vec::new);
                self.ver += 1;
            }
        }
        if self.job.is_none() && self.last_job.map_or(true, |t| t.elapsed() >= Duration::from_secs(30)) {
            self.last_job = Some(Instant::now());
            self.job = Some(cx.spawn(fetch_devices));
        }
        Tick::Poll(Duration::from_secs(if self.devices.is_none() { 3 } else { 30 }))
    }

    fn version(&self) -> u64 {
        self.ver
    }
}

impl Batteries {
    /// L: "БАТАРЕИ" header, then rows of 44 px: glyph, name over a bar, percent on the right.
    fn draw_list(&self, cx: &Cx, items: &[Item]) -> Result<()> {
        cx.text(Rect::new(16.0, 16.0, 324.0, 16.0), &cx.t("БАТАРЕИ", "BATTERIES"), Style::LABEL, accent());
        for (i, it) in items.iter().enumerate() {
            let row = Rect::new(16.0, 44.0 + 44.0 * i as f32, 324.0, 44.0);
            if i > 0 {
                cx.rect(Rect::new(row.x, row.y, row.w, 0.6), FILL);
            }
            let dim = !it.connected;
            cx.icon(it.glyph, Rect::new(row.x, row.cy() - 11.0, 22.0, 22.0), if dim { FAINT } else { TEXT });
            cx.text(Rect::new(row.x + 36.0, row.y + 8.0, 220.0, 18.0), &it.name, Style::new(13.0, 600).ellipsis(), if dim { DIM } else { TEXT });
            let bar = Rect::new(row.x + 36.0, row.y + 30.0, 214.0, 5.0);
            if it.plug {
                cx.bar(bar, 1.0, GREEN.fade(0.55));
            } else {
                cx.bar(bar, it.pct.map_or(0.0, |p| p as f32 / 100.0), level_color(it));
            }
            let st = Style::new(14.0, 500).mono();
            pct_label(cx, Rect::new(row.right() - 62.0, row.y, 62.0, 44.0), it, if it.plug { Style::new(12.0, 500) } else { st }, if dim || (it.pct.is_none() && !it.plug) { DIM } else { TEXT }, true);
        }
        if self.devices.as_ref().is_some_and(|d| d.is_empty()) || items.len() == 1 {
            let y = 44.0 + 44.0 * items.len() as f32;
            let msg = if self.devices.is_none() && !cx.preview { cx.t("Поиск устройств…", "Looking for devices…") } else { cx.t("Устройств Bluetooth нет", "No Bluetooth devices") };
            cx.text(Rect::new(16.0, y, 324.0, 30.0), msg, Style::new(12.0, 400), FAINT);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_names() {
        assert_eq!(classify("Logitech MX Master 3"), "mouse");
        assert_eq!(classify("Keychron K2"), "keyboard");
        assert_eq!(classify("AirPods Pro"), "headphones");
        assert_eq!(classify("Xbox Wireless Controller"), "gamepad");
        assert_eq!(classify("iPhone Ивана"), "phone");
        assert_eq!(classify("MI BT 18I"), "other");
    }

    #[test]
    fn parse_array_merge_and_filter() {
        let j = r#"[{"id":"a","name":"MX Master","present":true,"battery":64},
                    {"id":"b","name":"MX Master","present":true,"battery":null},
                    {"id":"c","name":"Old Buds","present":false,"battery":50},
                    {"id":"d","name":"","present":true,"battery":10},
                    {"id":"e","name":"Pad","present":true,"battery":250}]"#;
        let d = parse_devices(j);
        assert_eq!(d.len(), 3);
        assert_eq!(d[0].name, "MX Master");
        assert_eq!(d[0].percent, Some(64));
        assert_eq!(d[0].kind, "mouse");
        assert_eq!(d[1].percent, None); // out of range
        assert!(!d[2].connected);
        assert_eq!(d[2].percent, None); // disconnected -> no level
    }

    #[test]
    fn parse_single_object_and_garbage() {
        let d = parse_devices("\u{feff}{\"id\":\"a\",\"name\":\"Pad\",\"present\":true,\"battery\":7}");
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].percent, Some(7));
        assert!(parse_devices("").is_empty());
        assert!(parse_devices("[]").is_empty());
        assert!(parse_devices("not json").is_empty());
    }

    #[test]
    fn power_flags() {
        assert_eq!(pc_from_flags(128, 255), Pc { has_battery: false, percent: 0, charging: false });
        assert_eq!(pc_from_flags(255, 50).has_battery, false);
        assert_eq!(pc_from_flags(9, 82), Pc { has_battery: true, percent: 82, charging: true });
    }
}
