//! Quick Actions: S = 2x2 tiles (Блокировка, Сон, Без звука, Тема); M = volume + brightness sliders and 4 round buttons;
//! L = sliders, up to 6 round buttons and a power row (Перезагрузка, Выключение) that needs a second click within 3 s.
//!  * Volume: IAudioEndpointVolume of the default render endpoint (master scalar + mute), read every 2 s and after each interaction.
//!  * Brightness: DDC/CI of the primary monitor (GetMonitorBrightness/SetMonitorBrightness); laptops without DDC fall back to the
//!    WMI brightness class through PowerShell; when neither works the row is hidden. Read at start and every 2 min.
//!  * Wi-Fi / Bluetooth: Windows.Devices.Radios (only shown when the radio exists, they replace Блокировка/Сон in M).
//!  * Theme: HKCU ...\Themes\Personalize AppsUseLightTheme + SystemUsesLightTheme, then a WM_SETTINGCHANGE "ImmersiveColorSet" broadcast.
//!  * Drag a slider or turn the wheel over the card (2 % a notch; over the brightness row it changes brightness).
//! All system calls run in `cx.spawn` workers; writes are coalesced (one in flight, the newest value follows). Preview = fixed sample.
//! Preview-only settings (to check the other looks): `radios` (default true), `bri` (default true), `muted`, `armed` ("restart" | "shutdown").
//! Motion: the slider knob / percent follow the real value on an exponential approach (the real `set_volume` is never delayed), the knob
//! swells under the pointer, button plates ease in / out on hover and press, and a button whose look changes (a toggle, the armed
//! "Click again") cross-fades: `draw` leaves it out for ~170 ms and `overlay` paints the old and the new look. `Tick::Fps(60)` only while
//! something moves.
use crate::registry::{Registry, WidgetDef};
use crate::widget::*;
use std::time::Instant;
use windows::core::{w, PCWSTR};
use windows::Devices::Radios::{Radio, RadioAccessStatus, RadioKind, RadioState};
use windows::Win32::Devices::Display::{DestroyPhysicalMonitors, GetMonitorBrightness, GetNumberOfPhysicalMonitorsFromHMONITOR, GetPhysicalMonitorsFromHMONITOR, SetMonitorBrightness, PHYSICAL_MONITOR};
use windows::Win32::Foundation::{BOOL, HANDLE, LPARAM, POINT, WPARAM};
use windows::Win32::Graphics::Gdi::{MonitorFromPoint, MONITOR_DEFAULTTOPRIMARY};
use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::Media::Audio::{eConsole, eRender, IMMDeviceEnumerator, MMDeviceEnumerator};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED};
use windows::Win32::System::Power::SetSuspendState;
use windows::Win32::System::Registry::{RegGetValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_DWORD, RRF_RT_REG_DWORD};
use windows::Win32::System::Shutdown::LockWorkStation;
use windows::Win32::UI::WindowsAndMessaging::{SendMessageTimeoutW, HWND_BROADCAST, SMTO_ABORTIFHUNG, WM_SETTINGCHANGE};

/// deeper red of an armed power button (white text stays legible)
const RED_D: Color = Color::rgb8(224, 58, 48);
const ARM_MS: u64 = 3000;
const PERSONALIZE: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize");

// ================================================================ pure logic

/// slider value 0..=100 for a pointer x over the track `x0..x1`
fn slider_val(x: f32, x0: f32, x1: f32) -> u32 {
    if x1 <= x0 {
        return 0;
    }
    (((x - x0) / (x1 - x0)).clamp(0.0, 1.0) * 100.0).round() as u32
}

/// wheel: 2 % per notch, at least one step in the wheel direction
fn wheel_step(v: u32, notches: f32) -> u32 {
    let d = if notches > 0.0 { (notches * 2.0).round().max(2.0) } else { (notches * 2.0).round().min(-2.0) };
    (v as f32 + d).clamp(0.0, 100.0) as u32
}

/// DDC range -> percent and back
fn to_pct(cur: u32, min: u32, max: u32) -> u32 {
    if max <= min {
        return 0;
    }
    (cur.saturating_sub(min) as f32 * 100.0 / (max - min) as f32).round().min(100.0) as u32
}
fn from_pct(p: u32, min: u32, max: u32) -> u32 {
    min + ((max.saturating_sub(min)) as f32 * p.min(100) as f32 / 100.0).round() as u32
}

/// One write in flight, the newest wanted value waits for it (drags produce events far faster than DDC/audio can apply).
#[derive(Default, Debug)]
struct Coalesce {
    want: Option<u32>,
    busy: bool,
}
impl Coalesce {
    /// Some(v) = start a job for v now
    fn request(&mut self, v: u32) -> Option<u32> {
        if self.busy {
            self.want = Some(v);
            None
        } else {
            self.busy = true;
            Some(v)
        }
    }
    /// the running job finished: Some(v) = start the next one
    fn done(&mut self) -> Option<u32> {
        self.busy = false;
        self.want.take().map(|v| {
            self.busy = true;
            v
        })
    }
}

/// Power buttons: the first click arms (red, "Нажмите ещё раз"), the second one within 3 s executes.
#[derive(Default, Debug)]
struct Arm {
    id: Option<u8>,
    at: u64,
}
impl Arm {
    /// true = execute now
    fn click(&mut self, id: u8, now_ms: u64) -> bool {
        if self.live(now_ms) == Some(id) {
            *self = Arm::default();
            true
        } else {
            *self = Arm { id: Some(id), at: now_ms };
            false
        }
    }
    fn live(&self, now_ms: u64) -> Option<u8> {
        self.id.filter(|_| now_ms.saturating_sub(self.at) < ARM_MS)
    }
    fn remaining(&self, now_ms: u64) -> u64 {
        ARM_MS.saturating_sub(now_ms.saturating_sub(self.at))
    }
}

/// How one button looks, kept so that a change can cross-fade: fill, icon / text colour, glyph, the mute slash, the armed pill.
#[derive(Clone, Copy, PartialEq)]
struct BtnLook {
    fill: Color,
    ink: Color,
    glyph: Glyph,
    slash: bool,
    armed: bool,
}
/// A change of look in flight (button id, the look it left, since when).
struct Trans {
    id: u8,
    from: BtnLook,
    t0: Instant,
}
const TRANS_T: f32 = 0.17;

/// Eased slider values: the percent and the knob radius of the volume (0) and brightness (1) rows.
#[derive(Default)]
struct Mo {
    /// the values were set to the real ones once (the first snapshot does not glide up from 0)
    init: bool,
    since: Option<Instant>,
    shown: [f32; 2],
    rad: [f32; 2],
}
impl Mo {
    /// One step towards `goal` / `rad`; `fast` = being dragged (a short lag), `snap` = no motion (not ready yet, reduced motion). True while moving.
    fn step(&mut self, dt: f32, snap: bool, goal: [f32; 2], rad: [f32; 2], fast: [bool; 2]) -> bool {
        for i in 0..2 {
            if snap {
                (self.shown[i], self.rad[i]) = (goal[i], rad[i]);
            } else {
                self.shown[i] = crate::motion::follow(self.shown[i], goal[i], dt, if fast[i] { 0.03 } else { 0.06 }, 0.05);
                self.rad[i] = crate::motion::follow(self.rad[i], rad[i], dt, 0.05, 0.01);
            }
        }
        self.moving(goal, rad)
    }
    fn moving(&self, goal: [f32; 2], rad: [f32; 2]) -> bool {
        self.shown != goal || self.rad != rad
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Btn {
    Wifi,
    Bt,
    Lock,
    Sleep,
    Theme,
    Mute,
    Restart,
    Shutdown,
}

/// Round buttons of M (4) and L (up to 6): radios first, then the rest; the lowest priority (Сон, Блокировка) drops out of M.
fn round_set(size: Size, wifi: bool, bt: bool) -> Vec<Btn> {
    let mut v: Vec<Btn> = Vec::new();
    if wifi {
        v.push(Btn::Wifi);
    }
    if bt {
        v.push(Btn::Bt);
    }
    v.extend([Btn::Theme, Btn::Mute, Btn::Lock, Btn::Sleep]);
    if size == Size::M {
        v.truncate(4);
    }
    let order = |b: &Btn| [Btn::Wifi, Btn::Bt, Btn::Lock, Btn::Sleep, Btn::Theme, Btn::Mute].iter().position(|x| x == b);
    v.sort_by_key(order);
    v
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Hit {
    Btn(Btn),
    Vol,
    Bri,
}
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Look {
    Tile,
    Round,
    Pill,
}
#[derive(Clone, Copy, Debug)]
struct Item {
    btn: Btn,
    /// click area
    hit: Rect,
    /// the visible shape (circle square / tile / pill)
    shape: Rect,
    label: Rect,
    look: Look,
}
#[derive(Default, Debug)]
struct Lay {
    items: Vec<Item>,
    vol: Option<Rect>,
    bri: Option<Rect>,
}

impl Lay {
    fn hit(&self, x: f32, y: f32) -> Option<Hit> {
        if self.vol.is_some_and(|r| r.contains(x, y)) {
            return Some(Hit::Vol);
        }
        if self.bri.is_some_and(|r| r.contains(x, y)) {
            return Some(Hit::Bri);
        }
        self.items.iter().find(|i| i.hit.contains(x, y)).map(|i| Hit::Btn(i.btn))
    }
}

/// `btns` = the round/tile buttons of this size, `bri` = the brightness row is shown.
fn layout(size: Size, btns: &[Btn], bri: bool) -> Lay {
    let mut lay = Lay::default();
    match size {
        Size::S => {
            let (t, gap) = (65.0, 8.0);
            for (i, &b) in btns.iter().take(4).enumerate() {
                let r = Rect::new(16.0 + (i % 2) as f32 * (t + gap), 16.0 + (i / 2) as f32 * (t + gap), t, t);
                lay.items.push(Item { btn: b, hit: r, shape: r, label: Rect::new(r.x, r.bottom() - 22.0, r.w, 16.0), look: Look::Tile });
            }
        }
        Size::M => {
            let (vol_y, top, d) = if bri { (16.0, 90.0, 40.0) } else { (22.0, 78.0, 46.0) };
            lay.vol = Some(Rect::new(16.0, vol_y, 324.0, 30.0));
            if bri {
                lay.bri = Some(Rect::new(16.0, vol_y + 38.0, 324.0, 30.0));
            }
            round_row(&mut lay, btns, Rect::new(16.0, top, 324.0, d + 20.0), d);
        }
        Size::L => {
            lay.vol = Some(Rect::new(16.0, 44.0, 324.0, 32.0));
            let mut top = 84.0;
            if bri {
                lay.bri = Some(Rect::new(16.0, 84.0, 324.0, 32.0));
                top = 124.0;
            }
            let cols = match btns.len() {
                0..=3 => btns.len().max(1),
                4 => 2,
                _ => 3,
            };
            let (d, cell_h) = (48.0, 78.0);
            let rows = btns.len().div_ceil(cols);
            // centre the button block between the sliders and the power row
            let y0 = top + (292.0 - top - rows as f32 * cell_h).max(0.0) / 2.0;
            for (row, chunk) in btns.chunks(cols).enumerate() {
                let w = 324.0 / cols as f32;
                for (i, &b) in chunk.iter().enumerate() {
                    let cell = Rect::new(16.0 + i as f32 * w, y0 + row as f32 * cell_h, w, cell_h);
                    lay.items.push(round_item(b, cell, d));
                }
            }
            for (i, b) in [Btn::Restart, Btn::Shutdown].iter().enumerate() {
                let r = Rect::new(16.0 + i as f32 * 167.0, 300.0, 157.0, 40.0);
                lay.items.push(Item { btn: *b, hit: r, shape: r, label: r, look: Look::Pill });
            }
        }
    }
    lay
}

fn round_item(b: Btn, cell: Rect, d: f32) -> Item {
    let shape = Rect::new(cell.cx() - d / 2.0, cell.y, d, d);
    Item { btn: b, hit: Rect::new(cell.x, cell.y, cell.w, d + 20.0), shape, label: Rect::new(cell.x, shape.bottom() + 4.0, cell.w, 14.0), look: Look::Round }
}

fn round_row(lay: &mut Lay, btns: &[Btn], area: Rect, d: f32) {
    let n = btns.len().max(1);
    let w = area.w / n as f32;
    for (i, &b) in btns.iter().enumerate() {
        lay.items.push(round_item(b, Rect::new(area.x + i as f32 * w, area.y, w, area.h), d));
    }
}

// ================================================================ system calls (worker threads only)

fn com() {
    unsafe {
        // S_FALSE / RPC_E_CHANGED_MODE just mean COM is already set up on this thread
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
}

fn endpoint() -> windows::core::Result<IAudioEndpointVolume> {
    com();
    unsafe {
        let en: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let dev = en.GetDefaultAudioEndpoint(eRender, eConsole)?;
        dev.Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None)
    }
}

fn read_audio() -> Option<(u32, bool)> {
    let ep = endpoint().ok()?;
    unsafe { Some(((ep.GetMasterVolumeLevelScalar().ok()? * 100.0).round() as u32, ep.GetMute().ok()?.as_bool())) }
}

fn set_volume(p: u32) {
    if let Ok(ep) = endpoint() {
        unsafe {
            let _ = ep.SetMasterVolumeLevelScalar(p.min(100) as f32 / 100.0, std::ptr::null());
        }
    }
}

fn set_mute(m: bool) {
    if let Ok(ep) = endpoint() {
        unsafe {
            let _ = ep.SetMute(BOOL::from(m), std::ptr::null());
        }
    }
}

/// true = dark apps theme (a missing value means light)
fn read_dark() -> Option<bool> {
    let (mut data, mut len) = (1u32, 4u32);
    unsafe {
        let r = RegGetValueW(HKEY_CURRENT_USER, PERSONALIZE, w!("AppsUseLightTheme"), RRF_RT_REG_DWORD, None, Some(&mut data as *mut u32 as *mut _), Some(&mut len));
        r.is_ok().then_some(data == 0)
    }
}

fn set_theme(dark: bool) {
    let val: u32 = if dark { 0 } else { 1 };
    unsafe {
        for name in [w!("AppsUseLightTheme"), w!("SystemUsesLightTheme")] {
            let _ = RegSetKeyValueW(HKEY_CURRENT_USER, PERSONALIZE, name, REG_DWORD.0, Some(&val as *const u32 as *const _), 4);
        }
        let _ = SendMessageTimeoutW(HWND_BROADCAST, WM_SETTINGCHANGE, WPARAM(0), LPARAM(w!("ImmersiveColorSet").as_ptr() as isize), SMTO_ABORTIFHUNG, 2000, None);
    }
}

fn find_radio(kind: RadioKind) -> Option<Radio> {
    com();
    let radios = Radio::GetRadiosAsync().ok()?.get().ok()?;
    radios.into_iter().find(|r| r.Kind().ok() == Some(kind))
}

/// Some(on) when such a radio exists
fn read_radio(kind: RadioKind) -> Option<bool> {
    find_radio(kind).and_then(|r| r.State().ok()).map(|s| s == RadioState::On)
}

fn set_radio(kind: RadioKind, on: bool) {
    let Some(r) = find_radio(kind) else { return };
    let allowed = Radio::RequestAccessAsync().ok().and_then(|a| a.get().ok()) == Some(RadioAccessStatus::Allowed);
    if allowed {
        if let Ok(op) = r.SetStateAsync(if on { RadioState::On } else { RadioState::Off }) {
            let _ = op.get();
        }
    }
}

fn do_lock() {
    unsafe {
        let _ = LockWorkStation();
    }
}

/// blocks until the PC wakes up: always call from a worker
fn do_sleep() {
    unsafe {
        let _ = SetSuspendState(false, false, false);
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum BriMode {
    /// DDC/CI range of the primary monitor
    Ddc { min: u32, max: u32 },
    /// WMI WmiMonitorBrightness, 0..100 (laptop panels)
    Wmi,
}

/// Run `f` with the first physical monitor of the primary display.
fn with_monitor<T>(f: impl FnOnce(HANDLE) -> Option<T>) -> Option<T> {
    unsafe {
        let hm = MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY);
        let mut n = 0u32;
        GetNumberOfPhysicalMonitorsFromHMONITOR(hm, &mut n).ok()?;
        if n == 0 {
            return None;
        }
        let mut v = vec![PHYSICAL_MONITOR::default(); n as usize];
        GetPhysicalMonitorsFromHMONITOR(hm, &mut v).ok()?;
        let h = v[0].hPhysicalMonitor;
        let r = f(h);
        let _ = DestroyPhysicalMonitors(&v);
        r
    }
}

fn read_ddc() -> Option<(u32, BriMode)> {
    with_monitor(|h| unsafe {
        let (mut mn, mut cur, mut mx) = (0u32, 0u32, 0u32);
        if GetMonitorBrightness(h, &mut mn, &mut cur, &mut mx) == 0 || mx <= mn {
            return None;
        }
        Some((to_pct(cur, mn, mx), BriMode::Ddc { min: mn, max: mx }))
    })
}

fn read_wmi() -> Option<u32> {
    let out = powershell("(Get-CimInstance -Namespace root/WMI -ClassName WmiMonitorBrightness -ErrorAction SilentlyContinue | Select-Object -First 1).CurrentBrightness").ok()?;
    out.trim().parse::<u32>().ok().map(|v| v.min(100))
}

fn read_brightness() -> Option<(u32, BriMode)> {
    read_ddc().or_else(|| read_wmi().map(|v| (v, BriMode::Wmi)))
}

fn set_brightness(mode: BriMode, pct: u32) {
    match mode {
        BriMode::Ddc { min, max } => {
            let v = from_pct(pct, min, max);
            with_monitor(|h| unsafe { (SetMonitorBrightness(h, v) != 0).then_some(()) });
        }
        BriMode::Wmi => {
            let p = pct.min(100);
            let _ = powershell(&format!("Get-CimInstance -Namespace root/WMI -ClassName WmiMonitorBrightnessMethods | Invoke-CimMethod -MethodName WmiSetBrightness -Arguments @{{Timeout=1;Brightness={p}}} | Out-Null"));
        }
    }
}

/// Everything the 2 s poll reads. `radios` only on every 5th poll and after toggles (enumerating radios is the slow part).
#[derive(Default, Clone, Debug)]
struct Snap {
    audio: Option<(u32, bool)>,
    dark: Option<bool>,
    radios: Option<(Option<bool>, Option<bool>)>,
}

/// Poll spacing: a bit under the 2 s schedule so a tick that lands a hair early still polls.
const POLL_GAP: Duration = Duration::from_millis(1900);

/// Is the next poll due, given the time since the previous one (None = never polled)?
fn poll_due(since_last: Option<Duration>) -> bool {
    since_last.map_or(true, |d| d >= POLL_GAP)
}

fn read_snap(with_radios: bool) -> Snap {
    Snap { audio: read_audio(), dark: read_dark(), radios: with_radios.then(|| (read_radio(RadioKind::WiFi), read_radio(RadioKind::Bluetooth))) }
}

// ================================================================ widget

struct Controls {
    started: bool,
    ver: u64,
    t0: Instant,
    /// first snapshot and the brightness probe are in
    snap_in: bool,
    bri_in: bool,
    vol: u32,
    muted: bool,
    dark: bool,
    wifi: Option<bool>,
    bt: Option<bool>,
    bri: Option<u32>,
    bri_mode: Option<BriMode>,
    polls: u32,
    /// when the last poll job was spawned (None = none yet): a finished job wakes `tick` at once, so the 2 s pace needs its own gate
    last_poll: Option<Instant>,
    snap_job: Option<Pending<Snap>>,
    act_jobs: Vec<Pending<Snap>>,
    bri_probe: Option<Pending<Option<(u32, BriMode)>>>,
    vol_q: Coalesce,
    vol_job: Option<Pending<()>>,
    bri_q: Coalesce,
    bri_job: Option<Pending<()>>,
    drag: Option<Hit>,
    hot: Option<Hit>,
    down: Option<Hit>,
    /// last user interaction (snapshots right after it must not overwrite the optimistic values)
    touched: Instant,
    arm: Arm,
    /// armed button as last drawn (a change re-records `draw`)
    arm_shown: Option<u8>,
    /// hover / press amounts per cell (0 volume, 1 brightness, 2 + button id), eased in `overlay`
    hv: HoverAnim,
    pr: HoverAnim,
    mo: Mo,
    /// the look each button had when last seen, and the changes still cross-fading
    prev: Vec<(u8, BtnLook)>,
    trans: Vec<Trans>,
}

impl Default for Controls {
    fn default() -> Self {
        let now = Instant::now();
        Controls {
            started: false,
            ver: 0,
            t0: now,
            snap_in: false,
            bri_in: false,
            vol: 0,
            muted: false,
            dark: true,
            wifi: None,
            bt: None,
            bri: None,
            bri_mode: None,
            polls: 0,
            last_poll: None,
            snap_job: None,
            act_jobs: Vec::new(),
            bri_probe: None,
            vol_q: Coalesce::default(),
            vol_job: None,
            bri_q: Coalesce::default(),
            bri_job: None,
            drag: None,
            hot: None,
            down: None,
            touched: now - Duration::from_secs(60),
            arm: Arm::default(),
            arm_shown: None,
            hv: HoverAnim::new(10),
            pr: HoverAnim::timed(10, 0.08, 0.2),
            mo: Mo::default(),
            prev: Vec::new(),
            trans: Vec::new(),
        }
    }
}

pub fn register(r: &mut Registry) {
    r.register(WidgetDef {
        type_id: "controls",
        name: ("Быстрые действия", "Quick Actions"),
        desc: ("Блокировка, сон, громкость и яркость", "Lock, sleep, volume and brightness"),
        group: "controls",
        icon: Glyph::Power,
        sizes: &[Size::S, Size::M, Size::L],
        default_size: Size::M,
        make: || Box::new(Controls::default()),
    });
}

fn btn_id(b: Btn) -> u8 {
    b as u8
}

impl Controls {
    fn ms(&self) -> u64 {
        self.t0.elapsed().as_millis() as u64
    }
    fn ready(&self) -> bool {
        self.snap_in && self.bri_in
    }
    fn btns(&self, size: Size) -> Vec<Btn> {
        match size {
            Size::S => vec![Btn::Lock, Btn::Sleep, Btn::Mute, Btn::Theme],
            _ => round_set(size, self.wifi.is_some(), self.bt.is_some()),
        }
    }
    fn lay(&self, size: Size) -> Lay {
        layout(size, &self.btns(size), self.bri.is_some())
    }
    fn touch(&mut self) {
        self.touched = Instant::now();
    }
    fn fresh(&self) -> bool {
        self.touched.elapsed() < Duration::from_millis(1500)
    }

    fn set_vol(&mut self, cx: &Cx, v: u32) {
        let v = v.min(100);
        if v == self.vol {
            return;
        }
        self.vol = v;
        self.touch();
        if let Some(go) = self.vol_q.request(v) {
            self.vol_job = Some(cx.spawn(move || set_volume(go)));
        }
    }

    fn set_bri(&mut self, cx: &Cx, v: u32) {
        let (Some(mode), Some(cur)) = (self.bri_mode, self.bri) else { return };
        let v = v.min(100);
        if v == cur {
            return;
        }
        self.bri = Some(v);
        self.touch();
        if let Some(go) = self.bri_q.request(v) {
            self.bri_job = Some(cx.spawn(move || set_brightness(mode, go)));
        }
    }

    /// run an action on a worker, then read the state back
    fn act(&mut self, cx: &Cx, f: impl FnOnce() + Send + 'static, radios: bool) {
        self.touch();
        self.act_jobs.push(cx.spawn(move || {
            f();
            read_snap(radios)
        }));
    }

    fn apply(&mut self, s: Snap, from_poll: bool) {
        let mut changed = false;
        let hold = from_poll && self.fresh();
        if let Some((v, m)) = s.audio {
            let vol_busy = self.drag == Some(Hit::Vol) || self.vol_q.busy || self.fresh();
            if !vol_busy {
                self.vol = v;
            }
            if !hold && m != self.muted {
                self.muted = m;
                changed = true;
            }
        }
        if let Some(d) = s.dark {
            if !hold && d != self.dark {
                self.dark = d;
                changed = true;
            }
        }
        if let Some((w, b)) = s.radios {
            if !hold && (w != self.wifi || b != self.bt) {
                self.wifi = w;
                self.bt = b;
                changed = true;
            }
        }
        if !self.snap_in {
            self.snap_in = true;
            changed = true;
        }
        if changed {
            self.ver += 1;
        }
    }

    fn press(&mut self, cx: &Cx, b: Btn) {
        match b {
            Btn::Lock => self.act(cx, do_lock, false),
            Btn::Sleep => self.act(cx, do_sleep, false),
            Btn::Mute => {
                self.muted = !self.muted;
                let m = self.muted;
                self.act(cx, move || set_mute(m), false);
            }
            Btn::Theme => {
                self.dark = !self.dark;
                let d = self.dark;
                self.act(cx, move || set_theme(d), false);
            }
            Btn::Wifi => {
                let on = !self.wifi.unwrap_or(false);
                self.wifi = Some(on);
                self.act(cx, move || set_radio(RadioKind::WiFi, on), true);
            }
            Btn::Bt => {
                let on = !self.bt.unwrap_or(false);
                self.bt = Some(on);
                self.act(cx, move || set_radio(RadioKind::Bluetooth, on), true);
            }
            Btn::Restart | Btn::Shutdown => {
                if self.arm.click(btn_id(b), self.ms()) {
                    cx.invoke_system(Action::Power(if b == Btn::Restart { PowerOp::Restart } else { PowerOp::Shutdown }));
                } else {
                    // a sleeping no-op job wakes `tick` right after the 3 s are over, so the red look reverts on time
                    self.act(cx, || std::thread::sleep(Duration::from_millis(ARM_MS + 100)), false);
                }
                self.arm_shown = self.arm.live(self.ms());
            }
        }
        self.ver += 1;
    }

    /// (fill, icon colour) of a round button / tile
    fn look(&self, b: Btn) -> (Color, Color) {
        let on = |c: Color| (c, WHITE);
        match b {
            Btn::Wifi if self.wifi == Some(true) => on(BLUE),
            Btn::Bt if self.bt == Some(true) => on(BLUE),
            Btn::Mute if self.muted => (TEXT, INK),
            Btn::Theme if self.dark => (TEXT, INK),
            _ => (FILL, TEXT),
        }
    }

    fn label(cx: &Cx, b: Btn) -> &'static str {
        match b {
            Btn::Wifi => "Wi-Fi",
            Btn::Bt => "Bluetooth",
            Btn::Lock => cx.t("Блокировка", "Lock"),
            Btn::Sleep => cx.t("Сон", "Sleep"),
            Btn::Theme => cx.t("Тема", "Theme"),
            Btn::Mute => cx.t("Без звука", "Mute"),
            Btn::Restart => cx.t("Перезагрузка", "Restart"),
            Btn::Shutdown => cx.t("Выключение", "Shut down"),
        }
    }

    fn glyph(&self, b: Btn) -> Glyph {
        match b {
            Btn::Wifi => Glyph::Wifi,
            Btn::Bt => Glyph::Bluetooth,
            Btn::Lock => Glyph::Lock,
            Btn::Sleep => Glyph::Sleep,
            Btn::Theme => {
                if self.dark {
                    Glyph::Moon
                } else {
                    Glyph::Sun
                }
            }
            Btn::Mute => Glyph::Volume,
            Btn::Restart => Glyph::Restart,
            Btn::Shutdown => Glyph::Power,
        }
    }

    fn track(row: Rect) -> Rect {
        Rect::new(row.x + 30.0, row.cy() - 4.0, row.w - 30.0 - 44.0, 8.0)
    }

    fn slider_at(row: Rect, x: f32) -> u32 {
        let t = Self::track(row);
        slider_val(x, t.x + 4.0, t.right() - 4.0)
    }

    /// static part of a slider row: glyph and the empty track
    fn draw_slider(&self, cx: &Cx, row: Rect, g: Glyph, slashed: bool) {
        let ic = Rect::new(row.x + 1.0, row.cy() - 9.0, 18.0, 18.0);
        cx.icon(g, ic, if slashed { FAINT } else { DIM });
        if slashed {
            cx.line((ic.x + 2.0, ic.y + 15.0), (ic.right() - 1.0, ic.y + 2.0), 1.6, DIM);
        }
        cx.pill(Self::track(row), WHITE.alpha(0.27));
    }

    /// animated part: fill, knob and the percent; `v` is the eased percent, `r` the eased knob radius
    fn slider_live(&self, cx: &Cx, row: Rect, v: f32, r: f32, dim: bool) {
        let t = Self::track(row);
        let k = v / 100.0;
        let x = t.x + 4.0 + (t.w - 8.0) * k;
        let fill = Rect::new(t.x, t.y, (x - t.x + 2.0).max(t.h), t.h);
        cx.pill(fill, if dim { TEXT.alpha(0.45) } else { TEXT.alpha(0.92) });
        cx.circle((x, t.cy()), r + 0.8, BLACK.alpha(0.16));
        cx.circle((x, t.cy()), r, WHITE);
        cx.text(Rect::new(row.right() - 38.0, row.y, 38.0, row.h), &format!("{}%", v.round() as u32), Style::new(12.0, 500).mono().right(), if dim { FAINT } else { DIM });
    }

    /// The look of a button right now.
    fn cur_look(&self, b: Btn) -> BtnLook {
        let (fill, ink) = self.look(b);
        let armed = self.arm_shown == Some(btn_id(b));
        let pill = matches!(b, Btn::Restart | Btn::Shutdown);
        BtnLook { fill: if pill && armed { RED_D } else { fill }, ink: if pill && armed { WHITE } else { ink }, glyph: self.glyph(b), slash: b == Btn::Mute && self.muted, armed }
    }

    /// One button (fill, glyph, and for tiles / pills the text inside) in look `l` at opacity `k`.
    fn paint_item(cx: &Cx, it: &Item, l: &BtnLook, k: f32, ru: bool) {
        let (fill, ink) = (l.fill.fade(k), l.ink.fade(k));
        match it.look {
            Look::Round => {
                cx.circle(it.shape.center(), it.shape.w / 2.0, fill);
                let d = it.shape.w * 0.46;
                Self::draw_glyph(cx, l, it.shape.centered(d, d), ink);
            }
            Look::Tile => {
                cx.rrect(it.shape, 20.0, fill);
                Self::draw_glyph(cx, l, Rect::new(it.shape.cx() - 12.0, it.shape.y + 11.0, 24.0, 24.0), ink);
                cx.text(it.label, Self::label(cx, it.btn), Style::new(9.5, 500).center().ellipsis(), (if l.fill == FILL { DIM } else { l.ink }).fade(k));
            }
            Look::Pill => {
                cx.pill(it.shape, fill);
                let text = if l.armed { if ru { "Нажмите ещё раз" } else { "Click again" } } else { Self::label(cx, it.btn) };
                let st = Style::new(13.0, 600);
                let tw = cx.measure(text, st, 0.0).0.min(it.shape.w - 50.0);
                let x0 = it.shape.cx() - (tw + 24.0) / 2.0;
                Self::draw_glyph(cx, l, Rect::new(x0, it.shape.cy() - 8.0, 16.0, 16.0), ink);
                cx.text(Rect::new(x0 + 24.0, it.shape.y, tw + 4.0, it.shape.h), text, st.ellipsis(), ink);
            }
        }
    }

    fn draw_item(&self, cx: &Cx, it: &Item, ru: bool) {
        // a button that is cross-fading is painted by `overlay` meanwhile (its label outside the shape stays here)
        if !self.trans.iter().any(|t| t.id == btn_id(it.btn)) {
            Self::paint_item(cx, it, &self.cur_look(it.btn), 1.0, ru);
        }
        if it.look == Look::Round {
            cx.text(it.label, Self::label(cx, it.btn), Style::new(10.5, 500).center().ellipsis(), DIM);
        }
    }

    fn draw_glyph(cx: &Cx, l: &BtnLook, r: Rect, c: Color) {
        cx.icon(l.glyph, r, c);
        if l.slash {
            let s = r.w / 24.0;
            cx.line((r.x + 3.0 * s, r.bottom() - 3.0 * s), (r.right() - 3.0 * s, r.y + 3.0 * s), 1.8, c);
        }
    }

    /// The cell of a hit target in `hv` / `pr`.
    fn cell(h: Option<Hit>) -> Option<usize> {
        match h? {
            Hit::Vol => Some(0),
            Hit::Bri => Some(1),
            Hit::Btn(b) => Some(2 + btn_id(b) as usize),
        }
    }

    /// Slider values being shown: (goal percent, goal knob radius, being dragged) of volume and brightness.
    fn mo_goals(&self) -> ([f32; 2], [f32; 2], [bool; 2]) {
        let act = |h: Hit| self.drag == Some(h) || self.hot == Some(h);
        let (v, b) = (self.vol as f32, self.bri.map_or(0.0, |b| b as f32));
        ([v, b], [if act(Hit::Vol) { 9.5 } else { 8.0 }, if act(Hit::Bri) { 9.5 } else { 8.0 }], [self.drag == Some(Hit::Vol), self.drag == Some(Hit::Bri)])
    }

    /// Start the slider motion when the values it shows are off their goals (a mouse event or a poll moved them).
    fn mo_arm(&mut self, cx: &Cx) {
        let (goal, rad, _) = self.mo_goals();
        if !self.mo.init && self.ready() {
            self.mo.init = true;
            (self.mo.shown, self.mo.rad) = (goal, rad);
        }
        if self.mo.since.is_none() && self.mo.moving(goal, rad) && self.ready() {
            self.mo.since = Some(Instant::now());
            cx.waker().wake(); // mouse events do not call `tick`: poke it so the schedule starts now
        }
    }

    /// Advance the slider motion by the time since the last step; true while it still moves.
    fn mo_step(&mut self, cx: &Cx) -> bool {
        let Some(t0) = self.mo.since else { return false };
        let now = Instant::now();
        let dt = now.saturating_duration_since(t0).as_secs_f32().min(0.05);
        let (goal, rad, fast) = self.mo_goals();
        let snap = !self.ready() || reduced_motion();
        if self.mo.step(dt, snap, goal, rad, fast) {
            self.mo.since = Some(now);
            return true;
        }
        self.mo.since = None;
        cx.request_frame(); // the last position
        false
    }

    /// Register a change of any button's look as a cross-fade (not before the first snapshot, not in previews, not with reduced motion).
    fn note_looks(&mut self, cx: &Cx) {
        if cx.preview {
            return;
        }
        let lay = self.lay(cx.size);
        for it in &lay.items {
            let (id, cur) = (btn_id(it.btn), self.cur_look(it.btn));
            match self.prev.iter().position(|p| p.0 == id) {
                None => self.prev.push((id, cur)),
                Some(k) if self.prev[k].1 != cur => {
                    let from = std::mem::replace(&mut self.prev[k].1, cur);
                    if self.ready() && !reduced_motion() {
                        self.trans.retain(|t| t.id != id);
                        self.trans.push(Trans { id, from, t0: Instant::now() });
                        cx.waker().wake();
                    }
                }
                _ => {}
            }
        }
    }
}

impl Widget for Controls {
    fn draw(&mut self, cx: &mut Cx) -> Result<()> {
        let ru = cx.lang == Lang::Ru;
        let lay = self.lay(cx.size);
        if cx.size == Size::L {
            cx.text(Rect::new(16.0, 16.0, 324.0, 18.0), cx.t("БЫСТРЫЕ ДЕЙСТВИЯ", "QUICK ACTIONS"), Style::LABEL, accent());
        }
        if cx.size != Size::S && !self.ready() {
            cx.text(cx.bounds(), "…", Style::new(20.0, 300).center(), FAINT);
            return Ok(());
        }
        if let Some(r) = lay.vol {
            self.draw_slider(cx, r, Glyph::Volume, self.muted);
        }
        if let Some(r) = lay.bri {
            self.draw_slider(cx, r, Glyph::Brightness, false);
        }
        for it in &lay.items {
            self.draw_item(cx, it, ru);
        }
        Ok(())
    }

    fn overlay(&mut self, cx: &mut Cx) -> Result<()> {
        if cx.size != Size::S && !self.ready() {
            return Ok(());
        }
        let lay = self.lay(cx.size);
        // eased percent / knob (the real values are in `vol` / `bri`; a preview has no motion, it shows them as they are)
        let (shown, rad) = if cx.preview { (self.mo_goals().0, [8.0; 2]) } else { (self.mo.shown, self.mo.rad) };
        if let Some(r) = lay.vol {
            self.slider_live(cx, r, shown[0], rad[0], self.muted);
        }
        if let (Some(r), Some(_)) = (lay.bri, self.bri) {
            self.slider_live(cx, r, shown[1], rad[1], false);
        }
        // buttons that changed look: the old one fades out while the new one fades in
        let ru = cx.lang == Lang::Ru;
        for t in &self.trans {
            let Some(it) = lay.items.iter().find(|i| btn_id(i.btn) == t.id) else { continue };
            let p = (t.t0.elapsed().as_secs_f32() / TRANS_T).clamp(0.0, 1.0);
            Self::paint_item(cx, it, &t.from, 1.0 - p, ru);
            Self::paint_item(cx, it, &self.cur_look(it.btn), p, ru);
        }
        for it in &lay.items {
            let cell = 2 + btn_id(it.btn) as usize;
            let h = self.hv.get(cell);
            let k = 0.14 * h + 0.16 * self.pr.get(cell).min(h);
            if k <= 0.0 {
                continue;
            }
            let (fill, _) = self.look(it.btn);
            let c = if fill == TEXT { BLACK.alpha(k * 0.6) } else { WHITE.alpha(k) };
            match it.look {
                Look::Round => cx.circle(it.shape.center(), it.shape.w / 2.0, c),
                Look::Tile => cx.rrect(it.shape, 20.0, c),
                Look::Pill => cx.pill(it.shape, c),
            }
        }
        Ok(())
    }

    fn tick(&mut self, cx: &Cx) -> Tick {
        if !self.started {
            self.started = true;
            if cx.preview {
                self.vol = 64;
                self.muted = cx.cfg_bool("muted").unwrap_or(false);
                self.dark = true;
                (self.wifi, self.bt) = if cx.cfg_bool("radios").unwrap_or(true) { (Some(true), Some(false)) } else { (None, None) };
                self.bri = cx.cfg_bool("bri").unwrap_or(true).then_some(72);
                self.bri_mode = self.bri.map(|_| BriMode::Wmi);
                self.snap_in = true;
                self.bri_in = true;
                match cx.cfg_str("armed") {
                    Some("restart") => self.arm_shown = Some(btn_id(Btn::Restart)),
                    Some("shutdown") => self.arm_shown = Some(btn_id(Btn::Shutdown)),
                    _ => {}
                }
                self.ver += 1;
                return Tick::Never;
            }
            self.snap_job = Some(cx.spawn(|| read_snap(true)));
            self.last_poll = Some(Instant::now());
            self.bri_probe = Some(cx.spawn(read_brightness));
        }
        if cx.preview {
            return Tick::Never;
        }
        let seen = (self.vol, self.bri);
        // finished jobs
        if let Some((res, busy)) = self.snap_job.as_ref().map(|j| (j.take(), j.busy())) {
            if let Some(s) = res {
                self.snap_job = None;
                self.apply(s, true);
            } else if !busy {
                self.snap_job = None;
                if !self.snap_in {
                    self.snap_in = true; // the worker died: show the widget with defaults
                    self.ver += 1;
                }
            }
        }
        let mut finished: Vec<Snap> = Vec::new();
        self.act_jobs.retain(|j| match j.take() {
            Some(s) => {
                finished.push(s);
                false
            }
            None => j.busy(),
        });
        for s in finished {
            self.apply(s, false);
        }
        if let Some((res, busy)) = self.bri_probe.as_ref().map(|j| (j.take(), j.busy())) {
            if res.is_some() || !busy {
                self.bri_probe = None;
                if let Some((v, mode)) = res.flatten() {
                    if !self.bri_q.busy {
                        self.bri = Some(v);
                    }
                    self.bri_mode = Some(mode);
                } else if self.bri_mode.is_none() {
                    self.bri = None;
                }
                self.bri_in = true;
                self.ver += 1;
            }
        }
        if self.vol_job.as_ref().is_some_and(|j| j.take().is_some() || !j.busy()) {
            self.vol_job = None;
            if let Some(v) = self.vol_q.done() {
                self.vol_job = Some(cx.spawn(move || set_volume(v)));
            } else {
                self.act(cx, || {}, false); // read the real state back once the writes are done
            }
        }
        if self.bri_job.as_ref().is_some_and(|j| j.take().is_some() || !j.busy()) {
            self.bri_job = None;
            if let (Some(v), Some(mode)) = (self.bri_q.done(), self.bri_mode) {
                self.bri_job = Some(cx.spawn(move || set_brightness(mode, v)));
            }
        }
        if seen != (self.vol, self.bri) {
            cx.request_frame();
        }
        // the 2 s poll
        if self.snap_job.is_none() && self.snap_in && self.drag.is_none() && poll_due(self.last_poll.map(|t| t.elapsed())) {
            self.last_poll = Some(Instant::now());
            self.polls += 1;
            self.snap_job = Some(cx.spawn({
                let radios = self.polls % 5 == 0;
                move || read_snap(radios)
            }));
            if self.polls % 60 == 0 && self.bri_probe.is_none() && self.bri_mode.is_some() && !self.bri_q.busy {
                self.bri_probe = Some(cx.spawn(read_brightness));
            }
        }
        // arm / confirm expiry
        let now = self.ms();
        let live = self.arm.live(now);
        if live != self.arm_shown {
            self.arm_shown = live;
            self.ver += 1;
        }
        // motion: 60 fps only while a slider value, a hover / press plate or a cross-fade is moving
        self.note_looks(cx);
        let before = self.trans.len();
        self.trans.retain(|t| t.t0.elapsed().as_secs_f32() < TRANS_T);
        if self.trans.len() != before {
            self.ver += 1; // the static layer takes the finished buttons back
        }
        self.mo_arm(cx);
        let (a, b, c) = (self.hv.step(cx), self.pr.step(cx), self.mo_step(cx));
        if a || b || c || !self.trans.is_empty() {
            Tick::Fps(60)
        } else if live.is_some() {
            Tick::Every(Duration::from_millis(self.arm.remaining(now).clamp(100, 1000)))
        } else {
            Tick::Every(Duration::from_secs(2)) // the sliders live in `overlay`: repainted on every recompose; the poll itself is gated by `poll_due`
        }
    }

    fn mouse(&mut self, ev: &MouseEv, cx: &mut Cx) -> bool {
        if cx.size != Size::S && !self.ready() {
            return false;
        }
        let lay = self.lay(cx.size);
        let hit = if ev.kind == MouseKind::Leave { None } else { lay.hit(ev.x, ev.y) };
        let mut changed = false;
        match ev.kind {
            MouseKind::Leave => {
                changed = self.hot.is_some() || self.drag.is_some() || self.down.is_some();
                (self.hot, self.down) = (None, None);
                self.drag = None;
            }
            MouseKind::Move => {
                if self.hot != hit {
                    self.hot = hit;
                    changed = true;
                }
                match (self.drag, lay.vol, lay.bri) {
                    (Some(Hit::Vol), Some(r), _) => {
                        let v = Self::slider_at(r, ev.x);
                        self.set_vol(cx, v);
                        changed = true;
                    }
                    (Some(Hit::Bri), _, Some(r)) => {
                        let v = Self::slider_at(r, ev.x);
                        self.set_bri(cx, v);
                        changed = true;
                    }
                    _ => {}
                }
            }
            MouseKind::Down if ev.button == Button::Left => {
                self.down = hit;
                self.hot = hit;
                match (hit, lay.vol, lay.bri) {
                    (Some(Hit::Vol), Some(r), _) => {
                        self.drag = hit;
                        let v = Self::slider_at(r, ev.x);
                        self.set_vol(cx, v);
                    }
                    (Some(Hit::Bri), _, Some(r)) => {
                        self.drag = hit;
                        let v = Self::slider_at(r, ev.x);
                        self.set_bri(cx, v);
                    }
                    _ => {}
                }
                changed = hit.is_some();
            }
            MouseKind::Up if ev.button == Button::Left => {
                let (down, drag) = (self.down.take(), self.drag.take());
                if drag.is_some() {
                    self.act(cx, || {}, false);
                    changed = true;
                } else if let (Some(Hit::Btn(b)), Some(Hit::Btn(d))) = (hit, down) {
                    if b == d {
                        self.press(cx, b);
                    }
                    changed = true;
                } else {
                    changed = down.is_some();
                }
            }
            MouseKind::Wheel if cx.size != Size::S => {
                let target = if hit == Some(Hit::Bri) && self.bri.is_some() { Hit::Bri } else { Hit::Vol };
                if target == Hit::Bri {
                    let v = wheel_step(self.bri.unwrap_or(0), ev.delta);
                    self.set_bri(cx, v);
                } else {
                    let v = wheel_step(self.vol, ev.delta);
                    self.set_vol(cx, v);
                }
                changed = true;
            }
            _ => {}
        }
        // eased hover / press plates, the sliders' glide, a toggle's cross-fade
        changed |= self.hv.set(Self::cell(self.hot), cx);
        changed |= self.pr.set(Self::cell(self.down), cx);
        self.note_looks(cx);
        self.mo_arm(cx);
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
    fn poll_gate_waits_for_the_gap() {
        assert!(poll_due(None), "first poll is immediate");
        assert!(!poll_due(Some(Duration::from_millis(2))), "a job that just finished must not respawn");
        assert!(!poll_due(Some(Duration::from_millis(1899))));
        assert!(poll_due(Some(Duration::from_millis(1900))));
        assert!(poll_due(Some(Duration::from_secs(2))));
    }

    #[test]
    fn slider_motion_glides_settles_and_snaps() {
        let mut m = Mo::default();
        let (goal, rad) = ([64.0, 72.0], [8.0, 8.0]);
        assert!(m.moving(goal, rad), "at 0 with goals 64 / 72");
        assert!(!m.step(0.016, true, goal, rad, [false; 2]), "snap = no motion (reduced / first snapshot)");
        assert_eq!((m.shown, m.rad), (goal, rad));
        // the volume moves to 20: the shown value trails, the real one is not involved
        let goal = [20.0, 72.0];
        assert!(m.step(0.016, false, goal, rad, [false; 2]));
        assert!(m.shown[0] < 64.0 && m.shown[0] > 20.0 && m.shown[1] == 72.0);
        let mut n = 0;
        while m.step(0.016, false, goal, rad, [false; 2]) {
            n += 1;
            assert!(n < 100, "never settles");
        }
        assert!((15..40).contains(&n), "tau 60 ms: {n}");
        assert_eq!(m.shown[0], 20.0);
        assert!(!m.moving(goal, rad), "settled: the card may sleep");
        // a dragged slider follows with a shorter lag and the knob swells
        let a = Mo::default();
        let (mut slow, mut fast) = (a, Mo::default());
        slow.shown[0] = 0.0;
        fast.shown[0] = 0.0;
        slow.step(0.016, false, [100.0, 0.0], [8.0; 2], [false; 2]);
        fast.step(0.016, false, [100.0, 0.0], [8.0; 2], [true, false]);
        assert!(fast.shown[0] > slow.shown[0]);
        let mut k = Mo { rad: [8.0, 8.0], ..Mo::default() };
        k.step(0.05, false, [0.0; 2], [9.5, 8.0], [false; 2]);
        assert!(k.rad[0] > 8.0 && k.rad[0] < 9.5);
    }

    #[test]
    fn slider_maths() {
        assert_eq!(slider_val(50.0, 0.0, 100.0), 50);
        assert_eq!(slider_val(-30.0, 0.0, 100.0), 0);
        assert_eq!(slider_val(400.0, 0.0, 100.0), 100);
        assert_eq!(slider_val(10.0, 5.0, 5.0), 0);
        assert_eq!(wheel_step(50, 1.0), 52);
        assert_eq!(wheel_step(50, -1.0), 48);
        assert_eq!(wheel_step(99, 1.0), 100);
        assert_eq!(wheel_step(1, -3.0), 0);
        assert_eq!(wheel_step(50, 0.2), 52); // a tiny scroll still moves one step
        assert_eq!(to_pct(50, 0, 100), 50);
        assert_eq!(to_pct(30, 10, 50), 50);
        assert_eq!(from_pct(50, 10, 50), 30);
        assert_eq!(from_pct(200, 0, 100), 100);
        assert_eq!(to_pct(5, 5, 5), 0);
    }

    #[test]
    fn coalescing_keeps_only_the_newest() {
        let mut c = Coalesce::default();
        assert_eq!(c.request(10), Some(10));
        assert_eq!(c.request(20), None);
        assert_eq!(c.request(30), None);
        assert_eq!(c.done(), Some(30));
        assert_eq!(c.request(40), None);
        assert_eq!(c.done(), Some(40));
        assert_eq!(c.done(), None);
        assert_eq!(c.request(50), Some(50));
    }

    #[test]
    fn arm_confirm() {
        let mut a = Arm::default();
        assert!(!a.click(1, 1000)); // arms
        assert_eq!(a.live(1500), Some(1));
        assert!(a.click(1, 2500)); // second click inside 3 s executes
        assert_eq!(a.live(2600), None);
        assert!(!a.click(1, 10_000));
        assert!(!a.click(1, 13_500)); // too late: arms again instead of executing
        assert_eq!(a.live(13_600), Some(1));
        assert_eq!(a.live(16_600), None);
        assert!(!a.click(1, 20_000));
        assert!(!a.click(2, 20_500)); // another button only re-arms
        assert_eq!(a.live(20_600), Some(2));
        assert_eq!(a.remaining(21_000), 2500);
    }

    #[test]
    fn button_sets() {
        assert_eq!(round_set(Size::M, false, false), vec![Btn::Lock, Btn::Sleep, Btn::Theme, Btn::Mute]);
        assert_eq!(round_set(Size::M, true, true), vec![Btn::Wifi, Btn::Bt, Btn::Theme, Btn::Mute]);
        assert_eq!(round_set(Size::M, false, true), vec![Btn::Bt, Btn::Lock, Btn::Theme, Btn::Mute]);
        assert_eq!(round_set(Size::L, true, true).len(), 6);
        assert_eq!(round_set(Size::L, false, false).len(), 4);
    }

    #[test]
    fn layouts_hit_and_fit() {
        for size in [Size::S, Size::M, Size::L] {
            let (w, h) = size.logical();
            for bri in [true, false] {
                for radios in [false, true] {
                    let btns = if size == Size::S { vec![Btn::Lock, Btn::Sleep, Btn::Mute, Btn::Theme] } else { round_set(size, radios, radios) };
                    let lay = layout(size, &btns, bri);
                    for it in &lay.items {
                        assert!(it.hit.right() <= w && it.hit.bottom() <= h, "{size:?} {:?} {:?}", it.btn, it.hit);
                        assert_eq!(lay.hit(it.shape.cx(), it.shape.cy()), Some(Hit::Btn(it.btn)));
                    }
                    for r in [lay.vol, lay.bri].into_iter().flatten() {
                        assert!(r.right() <= w && r.bottom() <= h);
                        assert!(lay.items.iter().all(|i| i.hit.bottom() <= r.y || i.hit.y >= r.bottom()), "{size:?} overlap");
                    }
                    assert_eq!(lay.bri.is_some(), bri && size != Size::S);
                }
            }
        }
        let l = layout(Size::M, &round_set(Size::M, false, false), true);
        assert_eq!(l.hit(100.0, 30.0), Some(Hit::Vol));
        assert_eq!(l.hit(100.0, 70.0), Some(Hit::Bri));
        assert_eq!(l.hit(1.0, 1.0), None);
    }

    /// Read-only hardware probe, run once by hand: `cargo test -- --ignored --nocapture hw_probe`
    /// (re-applies the CURRENT volume and brightness, which changes nothing).
    #[test]
    #[ignore]
    fn hw_probe() {
        let a = read_audio();
        println!("audio {a:?}  dark {:?}", read_dark());
        println!("wifi {:?}  bt {:?}", read_radio(RadioKind::WiFi), read_radio(RadioKind::Bluetooth));
        let b = read_brightness();
        println!("brightness {b:?}");
        if let Some((v, _)) = a {
            set_volume(v);
            assert_eq!(read_audio().map(|x| x.0), Some(v));
        }
        if let Some((v, m)) = b {
            set_brightness(m, v);
        }
    }
}
