//! The widget API: everything a widget file needs is exported from here (`use crate::widget::*;`).
//!
//! A widget implements `Widget`; the framework (cards.rs / host.rs) owns glass, layers, scheduling, input and persistence.
//!  * `draw`    records the STATIC content (a Direct2D command list cached in the card layer). Re-run only when `version()` changed,
//!              the day changed, the size changed or `cx.invalidate()` was called.
//!  * `overlay` paints the ANIMATED part over the cached layer, every frame, only while the card is visible and `tick` animates.
//!  * `tick`    is called once at start, whenever the schedule it returned is due, and right after a background job finished
//!              (`cx.spawn`). Poll your `Pending` jobs here. The returned `Tick` is the next wake-up (static scene = `Tick::Never`).
//!  * `mouse`   card-local logical px; return true when something visible changed.
//! All drawing helpers take LOGICAL px (the framework applies the UI scale) in card-local coordinates (0,0 = top-left of the card).
use crate::gfx::*;
use crate::icons;
use crate::motion::HoverRamps;
use crate::sys;
pub use crate::icons::Glyph;
pub use crate::sys::{decode_image_bytes, decode_image_file, http_get, powershell, run_hidden, spawn_hidden, Action, Decoded, MediaKey, PowerOp};
pub use crate::tz;
pub use serde_json::{json, Value};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::mem::ManuallyDrop;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
pub use std::time::Duration;
use std::time::Instant;
use windows::core::{w, Interface, HSTRING};
use windows::Foundation::Numerics::Matrix3x2;
use windows::Win32::Foundation::{BOOL, HWND, LPARAM, WPARAM};
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, SystemParametersInfoW, SPI_GETCLIENTAREAANIMATION, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, WM_APP};
pub use windows::core::Result;
pub use windows::Win32::Graphics::Direct2D::ID2D1Bitmap1 as Bitmap;

/// Posted to the window when a background job of card `wparam` finished.
pub const WM_WIDGET: u32 = WM_APP + 2;

// ================================================================ sizes, ticks, time, input

/// Widget size class: S = 1x1 cells (170x170), M = 2x1 (356x170), L = 2x2 (356x356) at UI scale 1.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Size {
    S,
    M,
    L,
}
impl Size {
    pub const ALL: [Size; 3] = [Size::S, Size::M, Size::L];
    /// grid cells (columns, rows)
    pub fn cells(self) -> (u32, u32) {
        match self {
            Size::S => (1, 1),
            Size::M => (2, 1),
            Size::L => (2, 2),
        }
    }
    /// logical size in px at scale 1 (cell 170, gap 16)
    pub fn logical(self) -> (f32, f32) {
        match self {
            Size::S => (170.0, 170.0),
            Size::M => (356.0, 170.0),
            Size::L => (356.0, 356.0),
        }
    }
    pub fn radius(self) -> f32 {
        if self == Size::L {
            30.0
        } else {
            28.0
        }
    }
    /// "s" | "m" | "l" (the key used in config.json)
    pub fn key(self) -> &'static str {
        match self {
            Size::S => "s",
            Size::M => "m",
            Size::L => "l",
        }
    }
    pub fn parse(s: &str) -> Option<Size> {
        match s.to_ascii_lowercase().as_str() {
            "s" => Some(Size::S),
            "m" => Some(Size::M),
            "l" => Some(Size::L),
            _ => None,
        }
    }
}

/// When the framework should wake the widget next. The loop sleeps (0 % CPU) when every visible card says `Never`/`Poll`.
/// The system's "show animations" setting is off (SPI_GETCLIENTAREAANIMATION): motion goes straight to its end value.
pub fn reduced_motion() -> bool {
    unsafe {
        let mut on = BOOL(1);
        let ok = SystemParametersInfoW(SPI_GETCLIENTAREAANIMATION, 0, Some(&mut on as *mut BOOL as *mut std::ffi::c_void), SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0)).is_ok();
        ok && !on.as_bool()
    }
}

/// Hover ramps INSIDE a widget (a hot cell, a calendar day, a list row): `set` from `mouse` with the cell under the pointer, `step` from
/// `tick` (it advances by the real time elapsed), `get(i)` 0..1 in `overlay`. Draw the highlight in `overlay`, not in `draw`: the glass
/// layer is not re-rendered. While a ramp is moving `step` is true, so the widget returns `Tick::Fps(60)`; the frame after the last
/// movement is requested here, then the widget goes back to its idle schedule and costs nothing. Reduced motion and previews jump to the end.
pub struct HoverAnim {
    ramps: HoverRamps,
    hot: Option<usize>,
    since: Option<Instant>,
    t_in: f32,
    t_out: f32,
}
impl Default for HoverAnim {
    fn default() -> HoverAnim {
        HoverAnim::new(0)
    }
}
impl HoverAnim {
    /// A hover ramp: 120 ms in, 180 ms out.
    pub fn new(cells: usize) -> HoverAnim {
        HoverAnim::timed(cells, 0.12, 0.18)
    }

    /// With its own times in seconds (a press: faster in, slower out).
    pub fn timed(cells: usize, t_in: f32, t_out: f32) -> HoverAnim {
        HoverAnim { ramps: HoverRamps::new(cells), hot: None, since: None, t_in, t_out }
    }

    /// The cell under the pointer (None = none). True when it changed; the ramps start moving on their own after that.
    pub fn set(&mut self, hot: Option<usize>, cx: &Cx) -> bool {
        if hot == self.hot {
            return false;
        }
        self.hot = hot;
        let h = hot;
        if let Some(i) = hot {
            self.ramps.ensure(i + 1);
        }
        if cx.preview || reduced_motion() {
            self.ramps.snap(|i| Some(i) == h, |_| {}); // previews have no timers: show the end state
        } else {
            self.since.get_or_insert_with(Instant::now);
            cx.waker().wake(); // mouse events do not call `tick`: poke it so the schedule starts now
        }
        true
    }

    /// Advance to now (call from `tick`). True while something is still moving: the widget returns `Tick::Fps(60)`.
    pub fn step(&mut self, cx: &Cx) -> bool {
        let Some(t0) = self.since else { return false };
        let now = Instant::now();
        let dt = now.saturating_duration_since(t0).as_secs_f32().min(0.05);
        let h = self.hot;
        self.ramps.step(dt, self.t_in, self.t_out, |i| Some(i) == h, |_| {});
        if self.ramps.busy(|i| Some(i) == h) {
            self.since = Some(now);
            return true;
        }
        self.since = None;
        cx.request_frame(); // the last position of the ramps
        false
    }

    /// Eased hover amount of cell `i`.
    pub fn get(&self, i: usize) -> f32 {
        self.ramps.get(i)
    }

    /// Cell under the pointer.
    pub fn hot(&self) -> Option<usize> {
        self.hot
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Tick {
    /// static scene: nothing until a mouse event, a finished job or `cx.request_frame()`
    Never,
    /// animation at a fixed interval (overlay is painted every time)
    Every(Duration),
    /// animation at N frames per second (clamped 1..=60)
    Fps(u32),
    /// at every wall-clock second boundary (digital clocks, countdowns); overlay painted
    Second,
    /// at every wall-clock minute boundary; overlay painted
    Minute,
    /// data refresh timer: `tick` is called but no overlay frame is painted unless `version()` changed or you called `request_frame()`
    Poll(Duration),
}
impl Tick {
    /// true when the overlay is painted on every wake-up
    pub fn animates(self) -> bool {
        !matches!(self, Tick::Never | Tick::Poll(_))
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Now {
    pub y: i32,
    pub mo: u32,
    pub d: u32,
    pub h: u32,
    pub mi: u32,
    pub sec: u32,
    pub ms: u32,
}
impl Now {
    pub fn local() -> Now {
        let t = unsafe { GetLocalTime() };
        Now { y: t.wYear as i32, mo: t.wMonth as u32, d: t.wDay as u32, h: t.wHour as u32, mi: t.wMinute as u32, sec: t.wSecond as u32, ms: t.wMilliseconds as u32 }
    }
    /// "YYYY-MM-DD[ T]HH:MM[:SS]"
    pub fn parse(s: &str) -> Option<Now> {
        let n: Vec<u32> = s.split(|c: char| !c.is_ascii_digit()).filter(|p| !p.is_empty()).map(|p| p.parse().ok()).collect::<Option<_>>()?;
        if n.len() < 5 || !(1..=12).contains(&n[1]) || !(1..=31).contains(&n[2]) {
            return None;
        }
        Some(Now { y: n[0] as i32, mo: n[1], d: n[2], h: n[3], mi: n[4], sec: *n.get(5).unwrap_or(&0), ms: 0 })
    }
    /// The fixed instant used by previews (`--sheet`, `--dump`, the gallery) when no `--time` is given.
    pub fn sample() -> Now {
        Now { y: 2026, mo: 10, d: 24, h: 10, mi: 8, sec: 35, ms: 0 }
    }
    /// yyyymmdd, changes at local midnight
    pub fn day_key(&self) -> u32 {
        self.y as u32 * 10000 + self.mo * 100 + self.d
    }
    /// changes every minute (day + hh:mm)
    pub fn minute_key(&self) -> u64 {
        self.day_key() as u64 * 10000 + (self.h * 100 + self.mi) as u64
    }
    /// 0 = Monday .. 6 = Sunday
    pub fn dow(&self) -> u32 {
        tz::dow_mon0(self.y, self.mo, self.d)
    }
    pub fn days_in_month(&self) -> u32 {
        tz::days_in(self.y, self.mo)
    }
    /// seconds since midnight as a float (with ms)
    pub fn secs_f(&self) -> f32 {
        (self.h * 3600 + self.mi * 60 + self.sec) as f32 + self.ms as f32 / 1000.0
    }
    /// local wall clock as unix-like seconds (NOT UTC): days_from_civil * 86400 + time of day
    pub fn local_secs(&self) -> i64 {
        tz::days_from_civil(self.y, self.mo, self.d) * 86400 + (self.h * 3600 + self.mi * 60 + self.sec) as i64
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Lang {
    Ru,
    En,
}
impl Lang {
    pub fn pick<'a>(self, ru: &'a str, en: &'a str) -> &'a str {
        if self == Lang::Ru {
            ru
        } else {
            en
        }
    }
}
const WD_RU: [&str; 7] = ["Понедельник", "Вторник", "Среда", "Четверг", "Пятница", "Суббота", "Воскресенье"];
const WD_EN: [&str; 7] = ["Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday", "Sunday"];
const MO_RU: [&str; 12] = ["Январь", "Февраль", "Март", "Апрель", "Май", "Июнь", "Июль", "Август", "Сентябрь", "Октябрь", "Ноябрь", "Декабрь"];
const MO_GEN: [&str; 12] = ["января", "февраля", "марта", "апреля", "мая", "июня", "июля", "августа", "сентября", "октября", "ноября", "декабря"];
const MO_EN: [&str; 12] = ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];
/// Full weekday name, `dow` 0 = Monday ("Пятница" / "Friday").
pub fn weekday_name(l: Lang, dow: u32) -> &'static str {
    (if l == Lang::Ru { &WD_RU } else { &WD_EN })[(dow % 7) as usize]
}
/// Month name in the nominative, `mo` 1..=12 ("Октябрь" / "October").
pub fn month_name(l: Lang, mo: u32) -> &'static str {
    (if l == Lang::Ru { &MO_RU } else { &MO_EN })[(mo.clamp(1, 12) - 1) as usize]
}
/// Month name for "24 октября" (Russian genitive, lower case; English = full name).
pub fn month_genitive(l: Lang, mo: u32) -> &'static str {
    (if l == Lang::Ru { &MO_GEN } else { &MO_EN })[(mo.clamp(1, 12) - 1) as usize]
}
/// Russian plural form: `plural(5, "задача", "задачи", "задач")`.
pub fn plural<'a>(n: u32, one: &'a str, few: &'a str, many: &'a str) -> &'a str {
    let (m10, m100) = (n % 10, n % 100);
    if m10 == 1 && m100 != 11 {
        one
    } else if (2..=4).contains(&m10) && !(12..=14).contains(&m100) {
        few
    } else {
        many
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Button {
    None,
    Left,
    Right,
    Middle,
}
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MouseKind {
    Move,
    Down,
    Up,
    Wheel,
    Leave,
}
/// Mouse event in card-local LOGICAL px. `delta` = wheel notches (+ = away from the user / up).
#[derive(Clone, Copy, Debug)]
pub struct MouseEv {
    pub kind: MouseKind,
    pub x: f32,
    pub y: f32,
    pub button: Button,
    pub delta: f32,
}
impl MouseEv {
    pub fn pos(&self) -> Pt {
        (self.x, self.y)
    }
    pub fn inside(&self, r: Rect) -> bool {
        self.kind != MouseKind::Leave && r.contains(self.x, self.y)
    }
    /// left button pressed inside `r`
    pub fn pressed(&self, r: Rect) -> bool {
        self.kind == MouseKind::Down && self.button == Button::Left && self.inside(r)
    }
    /// left button released inside `r` (use this as "clicked")
    pub fn clicked(&self, r: Rect) -> bool {
        self.kind == MouseKind::Up && self.button == Button::Left && self.inside(r)
    }
    pub fn right_clicked(&self, r: Rect) -> bool {
        self.kind == MouseKind::Up && self.button == Button::Right && self.inside(r)
    }
    pub fn wheel(&self) -> f32 {
        if self.kind == MouseKind::Wheel {
            self.delta
        } else {
            0.0
        }
    }
}

// ================================================================ geometry & colour value types

pub type Pt = (f32, f32);

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}
impl Rect {
    pub const fn new(x: f32, y: f32, w: f32, h: f32) -> Rect {
        Rect { x, y, w, h }
    }
    pub fn right(&self) -> f32 {
        self.x + self.w
    }
    pub fn bottom(&self) -> f32 {
        self.y + self.h
    }
    pub fn cx(&self) -> f32 {
        self.x + self.w / 2.0
    }
    pub fn cy(&self) -> f32 {
        self.y + self.h / 2.0
    }
    pub fn center(&self) -> Pt {
        (self.cx(), self.cy())
    }
    pub fn contains(&self, px: f32, py: f32) -> bool {
        px >= self.x && px < self.x + self.w && py >= self.y && py < self.y + self.h
    }
    pub fn inset(&self, d: f32) -> Rect {
        Rect::new(self.x + d, self.y + d, (self.w - 2.0 * d).max(0.0), (self.h - 2.0 * d).max(0.0))
    }
    pub fn inset_xy(&self, dx: f32, dy: f32) -> Rect {
        Rect::new(self.x + dx, self.y + dy, (self.w - 2.0 * dx).max(0.0), (self.h - 2.0 * dy).max(0.0))
    }
    pub fn offset(&self, dx: f32, dy: f32) -> Rect {
        Rect::new(self.x + dx, self.y + dy, self.w, self.h)
    }
    /// same centre, new size
    pub fn centered(&self, w: f32, h: f32) -> Rect {
        Rect::new(self.cx() - w / 2.0, self.cy() - h / 2.0, w, h)
    }
    /// (top slice of height h, the rest)
    pub fn cut_top(&self, h: f32) -> (Rect, Rect) {
        let h = h.min(self.h);
        (Rect::new(self.x, self.y, self.w, h), Rect::new(self.x, self.y + h, self.w, self.h - h))
    }
    /// (the rest, bottom slice of height h)
    pub fn cut_bottom(&self, h: f32) -> (Rect, Rect) {
        let h = h.min(self.h);
        (Rect::new(self.x, self.y, self.w, self.h - h), Rect::new(self.x, self.bottom() - h, self.w, h))
    }
    /// (left slice of width w, the rest)
    pub fn cut_left(&self, w: f32) -> (Rect, Rect) {
        let w = w.min(self.w);
        (Rect::new(self.x, self.y, w, self.h), Rect::new(self.x + w, self.y, self.w - w, self.h))
    }
    /// (the rest, right slice of width w)
    pub fn cut_right(&self, w: f32) -> (Rect, Rect) {
        let w = w.min(self.w);
        (Rect::new(self.x, self.y, self.w - w, self.h), Rect::new(self.right() - w, self.y, w, self.h))
    }
    /// i-th of n equal rows (gap between rows)
    pub fn row(&self, i: u32, n: u32, gap: f32) -> Rect {
        let h = (self.h - gap * (n as f32 - 1.0)) / n as f32;
        Rect::new(self.x, self.y + i as f32 * (h + gap), self.w, h)
    }
    /// i-th of n equal columns (gap between columns)
    pub fn col(&self, i: u32, n: u32, gap: f32) -> Rect {
        let w = (self.w - gap * (n as f32 - 1.0)) / n as f32;
        Rect::new(self.x + i as f32 * (w + gap), self.y, w, self.h)
    }
}

/// Straight (non-premultiplied) RGBA, 0..1.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Color {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}
impl Color {
    pub const fn rgba(r: f32, g: f32, b: f32, a: f32) -> Color {
        Color { r, g, b, a }
    }
    pub const fn rgb8(r: u8, g: u8, b: u8) -> Color {
        Color { r: r as f32 / 255.0, g: g as f32 / 255.0, b: b as f32 / 255.0, a: 1.0 }
    }
    /// same colour, alpha replaced
    pub const fn alpha(self, a: f32) -> Color {
        Color { a, ..self }
    }
    /// same colour, alpha multiplied
    pub fn fade(self, k: f32) -> Color {
        Color { a: self.a * k, ..self }
    }
    pub fn mix(self, o: Color, t: f32) -> Color {
        let l = |a: f32, b: f32| a + (b - a) * t;
        Color { r: l(self.r, o.r), g: l(self.g, o.g), b: l(self.b, o.b), a: l(self.a, o.a) }
    }
    fn d2d(self) -> D2D1_COLOR_F {
        col(self.r, self.g, self.b, self.a)
    }
}
// palette (white text with alpha steps, like the Apple references)
pub const TEXT: Color = Color::rgba(1.0, 1.0, 1.0, 0.96);
pub const DIM: Color = Color::rgba(1.0, 1.0, 1.0, 0.70);
/// secondary text that must stay readable on bright glass: titles of cards, units next to a big number
pub const SUBTLE: Color = Color::rgba(1.0, 1.0, 1.0, 0.80);
pub const FAINT: Color = Color::rgba(1.0, 1.0, 1.0, 0.50);
/// subtle fills, ring tracks, bar tracks, separators
pub const FILL: Color = Color::rgba(1.0, 1.0, 1.0, 0.18);
pub const HAIR: Color = Color::rgba(1.0, 1.0, 1.0, 0.14);
pub const RED: Color = Color::rgb8(255, 69, 58);
pub const GREEN: Color = Color::rgb8(48, 209, 88);
pub const BLUE: Color = Color::rgb8(10, 132, 255);
pub const ORANGE: Color = Color::rgb8(255, 159, 10);
pub const YELLOW: Color = Color::rgb8(255, 214, 10);
pub const PINK: Color = Color::rgb8(255, 55, 95);
pub const TEAL: Color = Color::rgb8(64, 200, 224);
pub const PURPLE: Color = Color::rgb8(191, 90, 242);
/// The DEFAULT accent (`accentMode` "blue"). Widgets must call `accent()` instead: it follows the wallpaper in `accentMode` "wallpaper".
pub const ACCENT: Color = Color::rgb8(100, 190, 255); // lighter than BLUE: stays readable on grey, orange and pink glass
const ACCENT_RGB: [u8; 3] = [100, 190, 255];
static ACCENT_NOW: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(pack_rgb(ACCENT_RGB));
const fn pack_rgb(c: [u8; 3]) -> u32 {
    (c[0] as u32) << 16 | (c[1] as u32) << 8 | c[2] as u32
}
/// The accent colour of headers, links and highlights (calendar month, reminders title): `ACCENT`, or the colour taken from the wallpaper.
pub fn accent() -> Color {
    let v = ACCENT_NOW.load(Ordering::Relaxed);
    Color::rgb8((v >> 16) as u8, (v >> 8) as u8, v as u8)
}
/// Set the accent (`None` = the default blue); true when it changed (the cached card content has to be recorded again then).
pub fn set_accent(rgb: Option<[u8; 3]>) -> bool {
    ACCENT_NOW.swap(pack_rgb(rgb.unwrap_or(ACCENT_RGB)), Ordering::Relaxed) != pack_rgb(rgb.unwrap_or(ACCENT_RGB))
}
/// dark text on a light/cream fill (today circle, buttons)
pub const INK: Color = Color::rgba(20.0 / 255.0, 20.0 / 255.0, 24.0 / 255.0, 0.92);
pub const BLACK: Color = Color::rgba(0.0, 0.0, 0.0, 1.0);
pub const WHITE: Color = Color::rgba(1.0, 1.0, 1.0, 1.0);

// ================================================================ text styles

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Align {
    Left,
    Center,
    Right,
}
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum VAlign {
    Top,
    Middle,
    Bottom,
}
/// A text style; start from a preset and chain builders: `Style::BODY.center().ellipsis()`.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Style {
    pub size: f32,
    pub weight: u16,
    pub align: Align,
    pub valign: VAlign,
    /// tabular (fixed width) digits: clocks, counters, anything that changes in place
    pub mono: bool,
    /// single line cut with "..." instead of overflowing
    pub ellipsis: bool,
    /// wrap to the rect width (valign Top)
    pub wrap: bool,
    /// letter spacing in px
    pub track: f32,
}
impl Style {
    pub const fn new(size: f32, weight: u16) -> Style {
        Style { size, weight, align: Align::Left, valign: VAlign::Middle, mono: false, ellipsis: false, wrap: false, track: 0.0 }
    }
    /// 13 semibold
    pub const TITLE: Style = Style::new(13.0, 600);
    /// 13 regular
    pub const BODY: Style = Style::new(13.0, 400);
    /// 11 regular
    pub const CAPTION: Style = Style::new(11.0, 400);
    /// 44 light (temperature, counters)
    pub const BIG: Style = Style::new(44.0, 300);
    /// 64 light
    pub const HUGE: Style = Style::new(64.0, 300);
    /// 13 regular with tabular digits
    pub const MONO: Style = Style { mono: true, ..Style::new(13.0, 400) };
    /// 12 bold, letter-spaced: month names, "ПЯТНИЦА", section headers (colour it ACCENT or RED, UPPERCASE the text yourself)
    pub const LABEL: Style = Style { track: 0.48, ..Style::new(12.0, 700) };
    pub const fn size(self, s: f32) -> Style {
        Style { size: s, ..self }
    }
    pub const fn weight(self, w: u16) -> Style {
        Style { weight: w, ..self }
    }
    pub const fn center(self) -> Style {
        Style { align: Align::Center, ..self }
    }
    pub const fn right(self) -> Style {
        Style { align: Align::Right, ..self }
    }
    pub const fn top(self) -> Style {
        Style { valign: VAlign::Top, ..self }
    }
    pub const fn bottom(self) -> Style {
        Style { valign: VAlign::Bottom, ..self }
    }
    pub const fn mono(self) -> Style {
        Style { mono: true, ..self }
    }
    pub const fn ellipsis(self) -> Style {
        Style { ellipsis: true, ..self }
    }
    pub const fn wrap(self) -> Style {
        Style { wrap: true, valign: VAlign::Top, ..self }
    }
    pub const fn track(self, px: f32) -> Style {
        Style { track: px, ..self }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Fit {
    /// fill the rect, crop the overflow (photos, album art)
    Cover,
    /// whole image visible, letterboxed
    Contain,
    /// stretch to the rect
    Stretch,
}

// ================================================================ shared drawing resources (one per scene)

type FmtKey = (u32, u16, u8, u8, bool, bool);
pub struct Res {
    brush: ID2D1SolidColorBrush,
    pub(crate) round: ID2D1StrokeStyle,
    fams: [HSTRING; 3],
    /// the bundled Nunito collection; `None` = system fonts (Segoe UI Variable)
    coll: Option<IDWriteFontCollection>,
    fmts: RefCell<HashMap<FmtKey, IDWriteTextFormat>>,
    glyphs: RefCell<HashMap<Glyph, Rc<Vec<(ID2D1PathGeometry, icons::Paint)>>>>,
}

/// The installed optical-size cut of Segoe UI Variable (Small / Text / Display), else plain Segoe UI.
fn family(gfx: &Gfx, cut: &str) -> HSTRING {
    unsafe {
        let mut coll: Option<IDWriteFontCollection> = None;
        if gfx.dw.GetSystemFontCollection(&mut coll, false).is_ok() {
            if let Some(c) = coll {
                for name in [format!("Segoe UI Variable {cut}"), "Segoe UI".to_string()] {
                    let (mut i, mut ok) = (0u32, windows::Win32::Foundation::BOOL(0));
                    if c.FindFamilyName(&HSTRING::from(name.as_str()), &mut i, &mut ok).is_ok() && ok.as_bool() {
                        return HSTRING::from(name.as_str());
                    }
                }
            }
        }
        HSTRING::from("Segoe UI")
    }
}

/// Optical size: captions use the Small cut, running text the Text cut, big numbers and headlines the Display cut.
fn cut_for(size: f32) -> usize {
    if size <= 11.5 {
        0
    } else if size <= 21.0 {
        1
    } else {
        2
    }
}

/// Nunito is lighter and rounder than Segoe: captions get one step more weight so they hold their contrast on glass.
fn weight_for(size: f32, weight: u16, nunito: bool) -> i32 {
    let w = weight as i32;
    if nunito && size <= 12.5 {
        (w + 100).min(700)
    } else {
        w
    }
}

/// `accentScope` "all" (Apple's Tinted icon and widget style): every colour a card draws becomes the accent, as bright as the colour was (white text =
/// the accent lightened a little, a green ring = a half-bright accent); alpha is kept, the red / orange warning colours keep their meaning.
static TINT: AtomicBool = AtomicBool::new(false);

/// Turn the tint on / off; true when that changed it (the caller re-renders every card).
pub fn set_tint(on: bool) -> bool {
    TINT.swap(on, Ordering::Relaxed) != on
}

fn tinted(c: Color, light: bool) -> Color {
    let near = |a: Color, b: Color| (a.r - b.r).abs() + (a.g - b.g).abs() + (a.b - b.b).abs() < 0.06;
    if near(c, RED) || near(c, ORANGE) {
        return c;
    }
    let acc = accent();
    // brightness = the strongest channel: white text and the accent itself (titles) get the full tint, a green ring a bit less, grey text its grey
    let v = c.r.max(c.g).max(c.b).clamp(0.0, 1.0);
    if light {
        // dark ink on light glass: a darkened accent, the darker the ink the fuller; white stays white (text on a filled accent); coloured items full
        if v > 0.9 && c.r.min(c.g).min(c.b) > 0.8 {
            return c;
        }
        let chroma = c.r.max(c.g).max(c.b) - c.r.min(c.g).min(c.b);
        let k = if chroma > 0.2 { 1.0 } else { (1.1 - v).clamp(0.25, 1.0) } * 0.62;
        Color::rgba(acc.r * k, acc.g * k, acc.b * k, c.a)
    } else {
        let t = 0.28; // toward white: the accent itself is often a mid pastel and thin text needs the contrast
        let (r, g, b) = (acc.r + (1.0 - acc.r) * t, acc.g + (1.0 - acc.g) * t, acc.b + (1.0 - acc.b) * t);
        Color::rgba(r * v, g * v, b * v, c.a)
    }
}

/// The type scale of the cards (px). Widgets ask for the size they want (and fit loops step through fractions: 14.276, 11.5, 9.5...); every request
/// snaps to the nearest step so the whole desk shares one hierarchy: caption 11, small 12, body 13, emphasis 15, headline 17, then the number sizes.
/// `MSW_TEXT_SCALE=off` keeps the requested sizes.
const TYPE_SCALE: [f32; 15] = [10.0, 11.0, 12.0, 13.0, 15.0, 17.0, 20.0, 24.0, 28.0, 34.0, 40.0, 46.0, 54.0, 64.0, 76.0];

fn snap_size(size: f32) -> f32 {
    static OFF: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if *OFF.get_or_init(|| std::env::var("MSW_TEXT_SCALE").is_ok_and(|v| v == "off")) || size <= 0.0 {
        return size;
    }
    let mut best = TYPE_SCALE[0];
    for &t in &TYPE_SCALE {
        // ties go up: 11.5 -> 12, 12.5 -> 13
        if (t - size).abs() <= (best - size).abs() {
            best = t;
        }
    }
    if size > TYPE_SCALE[TYPE_SCALE.len() - 1] { size } else { best }
}

/// `MSW_TEXT_LINT` is set: every `Cx::text` logs its geometry (a developer aid, see tools/text-lint.py).
fn text_lint() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("MSW_TEXT_LINT").is_some())
}

/// Size-dependent tracking in px (SF-like): small text opens up a little, large text tightens.
fn auto_track(size: f32) -> f32 {
    match size {
        s if s <= 11.5 => 0.1,
        s if s <= 15.0 => 0.0,
        s if s <= 21.0 => -0.006 * s,
        s if s <= 36.0 => -0.012 * s,
        s => -0.016 * s,
    }
}

impl Res {
    pub fn new(gfx: &Gfx) -> Result<Res> {
        unsafe {
            let sp = D2D1_STROKE_STYLE_PROPERTIES1 {
                startCap: D2D1_CAP_STYLE_ROUND,
                endCap: D2D1_CAP_STYLE_ROUND,
                dashCap: D2D1_CAP_STYLE_ROUND,
                lineJoin: D2D1_LINE_JOIN_ROUND,
                miterLimit: 4.0,
                dashStyle: D2D1_DASH_STYLE_SOLID,
                dashOffset: 0.0,
                transformType: D2D1_STROKE_TRANSFORM_TYPE_NORMAL,
            };
            let round: ID2D1StrokeStyle = gfx.d2d.CreateStrokeStyle(&sp, None)?.cast()?;
            let coll = crate::fonts::collection(&gfx.dw);
            let fams = if coll.is_some() { std::array::from_fn(|_| HSTRING::from(crate::fonts::family())) } else { [family(gfx, "Small"), family(gfx, "Text"), family(gfx, "Display")] };
            Ok(Res {
                brush: gfx.dc.CreateSolidColorBrush(&col(1.0, 1.0, 1.0, 1.0), None)?,
                round,
                fams,
                coll,
                fmts: RefCell::new(HashMap::new()),
                glyphs: RefCell::new(HashMap::new()),
            })
        }
    }

    fn format(&self, gfx: &Gfx, st: Style) -> Result<IDWriteTextFormat> {
        let st = Style { size: snap_size(st.size), ..st };
        let key: FmtKey = ((st.size * 10.0) as u32, st.weight, st.align as u8, st.valign as u8, st.wrap, st.ellipsis);
        if let Some(f) = self.fmts.borrow().get(&key) {
            return Ok(f.clone());
        }
        unsafe {
            let f = gfx.dw.CreateTextFormat(&self.fams[cut_for(st.size)], self.coll.as_ref(), DWRITE_FONT_WEIGHT(weight_for(st.size, st.weight, self.coll.is_some() && crate::fonts::family() == "Nunito")), DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_STRETCH_NORMAL, st.size, w!("ru-ru"))?;
            f.SetTextAlignment(match st.align {
                Align::Left => DWRITE_TEXT_ALIGNMENT_LEADING,
                Align::Center => DWRITE_TEXT_ALIGNMENT_CENTER,
                Align::Right => DWRITE_TEXT_ALIGNMENT_TRAILING,
            })?;
            f.SetParagraphAlignment(match st.valign {
                VAlign::Top => DWRITE_PARAGRAPH_ALIGNMENT_NEAR,
                VAlign::Middle => DWRITE_PARAGRAPH_ALIGNMENT_CENTER,
                VAlign::Bottom => DWRITE_PARAGRAPH_ALIGNMENT_FAR,
            })?;
            f.SetWordWrapping(if st.wrap { DWRITE_WORD_WRAPPING_WRAP } else { DWRITE_WORD_WRAPPING_NO_WRAP })?;
            if st.wrap {
                // multi-line text breathes: 1.32 line height instead of the font's tight default
                let lh = (st.size * 1.32).round();
                f.SetLineSpacing(DWRITE_LINE_SPACING_METHOD_UNIFORM, lh, (lh * 0.8).round())?;
            }
            if st.ellipsis {
                let sign = gfx.dw.CreateEllipsisTrimmingSign(&f)?;
                f.SetTrimming(&DWRITE_TRIMMING { granularity: DWRITE_TRIMMING_GRANULARITY_CHARACTER, delimiter: 0, delimiterCount: 0 }, &sign)?;
            }
            self.fmts.borrow_mut().insert(key, f.clone());
            Ok(f)
        }
    }

    fn glyph(&self, gfx: &Gfx, g: Glyph) -> Result<Rc<Vec<(ID2D1PathGeometry, icons::Paint)>>> {
        if let Some(v) = self.glyphs.borrow().get(&g) {
            return Ok(v.clone());
        }
        let v = Rc::new(icons::geometries(&gfx.d2d, g)?);
        self.glyphs.borrow_mut().insert(g, v.clone());
        Ok(v)
    }
}

// ================================================================ background jobs

/// Mailbox for the result of a background job started with `cx.spawn`. Poll it with `take()` from `tick`.
pub struct Pending<T>(Arc<PendingInner<T>>);
struct PendingInner<T> {
    val: Mutex<Option<T>>,
    busy: AtomicBool,
}
impl<T> Clone for Pending<T> {
    fn clone(&self) -> Self {
        Pending(self.0.clone())
    }
}
impl<T> Default for Pending<T> {
    fn default() -> Self {
        Pending(Arc::new(PendingInner { val: Mutex::new(None), busy: AtomicBool::new(false) }))
    }
}
impl<T> Pending<T> {
    /// the finished result (once), or None while running / not started / the job panicked
    pub fn take(&self) -> Option<T> {
        self.0.val.lock().ok().and_then(|mut g| g.take())
    }
    /// a job is running right now
    pub fn busy(&self) -> bool {
        self.0.busy.load(Ordering::Relaxed)
    }
}

/// Wakes the UI thread for one card from any thread. Cheap to clone.
#[derive(Clone, Copy, Debug, Default)]
pub struct Waker {
    pub hwnd: isize,
    pub idx: usize,
}
impl Waker {
    pub fn wake(&self) {
        if self.hwnd != 0 {
            unsafe {
                let _ = PostMessageW(HWND(self.hwnd as *mut _), WM_WIDGET, WPARAM(self.idx), LPARAM(0));
            }
        }
    }
}

/// The worker threads behind `Cx::spawn`. A thread per job (the old way) meant a few dozen thread creations per second on a busy desk, and the
/// NVIDIA user-mode driver keeps ~192 B per thread it has ever seen (the 0.5.10 memory growth). Now: up to `core` workers, created on demand and
/// kept; jobs run in submission order; a job that blocks (curl, a prompt, a sleeping disk) is noticed after `slow` and then an extra worker is
/// added so the jobs queued behind it never wait for it (the extras leave after a quiet spell, `max` bounds them).
mod jobpool {
    use std::collections::{HashMap, VecDeque};
    use std::panic::{catch_unwind, AssertUnwindSafe};
    use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
    use std::time::{Duration, Instant};

    pub type Job = Box<dyn FnOnce() + Send + 'static>;

    struct State {
        queue: VecDeque<Job>,
        /// live worker threads / of them waiting for a job
        workers: usize,
        idle: usize,
        /// worker id -> when its current job started
        running: HashMap<u64, Instant>,
        next_id: u64,
        supervisor: bool,
    }
    struct Inner {
        st: Mutex<State>,
        /// workers wait here for jobs, the supervisor here for a non-empty queue
        work: Condvar,
        watch: Condvar,
        core: usize,
        max: usize,
        slow: Duration,
        cap: usize,
    }
    pub struct Pool(Arc<Inner>);

    fn lock(m: &Mutex<State>) -> MutexGuard<'_, State> {
        m.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The pool every `Cx::spawn` uses: 4 permanent workers, up to 16 while jobs block, 256 queued jobs.
    pub fn global() -> &'static Pool {
        static P: OnceLock<Pool> = OnceLock::new();
        P.get_or_init(|| Pool::new(4, 16, Duration::from_millis(250), 256))
    }

    impl Pool {
        pub fn new(core: usize, max: usize, slow: Duration, cap: usize) -> Pool {
            let st = State { queue: VecDeque::new(), workers: 0, idle: 0, running: HashMap::new(), next_id: 0, supervisor: false };
            Pool(Arc::new(Inner { st: Mutex::new(st), work: Condvar::new(), watch: Condvar::new(), core: core.max(1), max: max.max(core.max(1)), slow, cap }))
        }

        /// Queue `job`; false = not accepted (queue full or no thread could be created), the job is dropped.
        pub fn submit(&self, job: Job) -> bool {
            let inner = &self.0;
            let mut st = lock(&inner.st);
            if st.queue.len() >= inner.cap {
                return false;
            }
            st.queue.push_back(job);
            if st.queue.len() > st.idle && st.workers < inner.core && !spawn_worker(inner, &mut st) && st.workers == 0 {
                st.queue.pop_back();
                return false;
            }
            if st.idle > 0 {
                inner.work.notify_one();
            } else if st.workers >= inner.core {
                if !st.supervisor {
                    let i2 = inner.clone();
                    st.supervisor = std::thread::Builder::new().name("widget-job-watch".into()).spawn(move || supervise(i2)).is_ok();
                }
                inner.watch.notify_one();
            }
            true
        }

        #[cfg(test)]
        pub fn workers(&self) -> usize {
            lock(&self.0.st).workers
        }
    }

    fn spawn_worker(inner: &Arc<Inner>, st: &mut State) -> bool {
        let (i2, id) = (inner.clone(), st.next_id);
        st.next_id += 1;
        let ok = std::thread::Builder::new().name("widget-job".into()).spawn(move || worker(i2, id)).is_ok();
        if ok {
            st.workers += 1;
        }
        ok
    }

    /// A thread that ends takes its COM apartment with it; a pooled one must not carry the state of one job (STA / MTA, init counts) into the next.
    fn reset_com() {
        use windows::Win32::System::Com::{CoGetApartmentType, CoUninitialize, APTTYPE, APTTYPEQUALIFIER, APTTYPEQUALIFIER_IMPLICIT_MTA};
        unsafe {
            for _ in 0..16 {
                let (mut t, mut q) = (APTTYPE::default(), APTTYPEQUALIFIER::default());
                if CoGetApartmentType(&mut t, &mut q).is_err() || q == APTTYPEQUALIFIER_IMPLICIT_MTA {
                    break;
                }
                CoUninitialize();
            }
        }
    }

    fn worker(inner: Arc<Inner>, id: u64) {
        let mut st = lock(&inner.st);
        loop {
            if let Some(job) = st.queue.pop_front() {
                st.running.insert(id, Instant::now());
                drop(st);
                let _ = catch_unwind(AssertUnwindSafe(job)); // a panicking job must not take the worker down
                reset_com();
                st = lock(&inner.st);
                st.running.remove(&id);
                continue;
            }
            st.idle += 1;
            let extra = st.workers > inner.core;
            if extra {
                let (g, t) = inner.work.wait_timeout(st, Duration::from_secs(3)).unwrap_or_else(|e| e.into_inner());
                st = g;
                st.idle -= 1;
                if t.timed_out() && st.queue.is_empty() && st.workers > inner.core {
                    st.workers -= 1;
                    return;
                }
            } else {
                st = inner.work.wait(st).unwrap_or_else(|e| e.into_inner());
                st.idle -= 1;
            }
        }
    }

    /// While jobs wait and every worker is busy, one of them for longer than `slow`: add a worker (up to `max`).
    fn supervise(inner: Arc<Inner>) {
        let mut st = lock(&inner.st);
        loop {
            while st.queue.is_empty() {
                st = inner.watch.wait(st).unwrap_or_else(|e| e.into_inner());
            }
            let stuck = st.running.values().any(|t| t.elapsed() >= inner.slow);
            if st.idle == 0 && stuck && st.workers < inner.max {
                spawn_worker(&inner, &mut st);
            }
            st = inner.watch.wait_timeout(st, Duration::from_millis(50)).unwrap_or_else(|e| e.into_inner()).0;
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::mpsc;

        fn pool(core: usize, max: usize, slow_ms: u64, cap: usize) -> Pool {
            Pool::new(core, max, Duration::from_millis(slow_ms), cap)
        }

        #[test]
        fn jobs_run_in_order_and_results_arrive() {
            let p = pool(1, 1, 250, 256);
            let (tx, rx) = mpsc::channel();
            for i in 0..200 {
                let tx = tx.clone();
                assert!(p.submit(Box::new(move || tx.send(i).unwrap())));
            }
            let got: Vec<i32> = (0..200).map(|_| rx.recv_timeout(Duration::from_secs(5)).unwrap()).collect();
            assert_eq!(got, (0..200).collect::<Vec<_>>(), "one worker = strict submission order");
        }

        #[test]
        fn a_panicking_job_does_not_kill_the_pool() {
            let p = pool(2, 2, 250, 256);
            for _ in 0..10 {
                assert!(p.submit(Box::new(|| panic!("job panic (expected in this test)"))));
            }
            let (tx, rx) = mpsc::channel();
            for i in 0..20 {
                let tx = tx.clone();
                assert!(p.submit(Box::new(move || tx.send(i).unwrap())));
            }
            let mut got: Vec<i32> = (0..20).map(|_| rx.recv_timeout(Duration::from_secs(5)).unwrap()).collect();
            got.sort();
            assert_eq!(got, (0..20).collect::<Vec<_>>());
            assert_eq!(p.workers(), 2, "both workers survived ten panics");
        }

        fn process_threads() -> usize {
            use windows::Win32::System::Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32};
            use windows::Win32::System::Threading::GetCurrentProcessId;
            unsafe {
                let pid = GetCurrentProcessId();
                let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) else { return 0 };
                let mut e = THREADENTRY32 { dwSize: std::mem::size_of::<THREADENTRY32>() as u32, ..Default::default() };
                let (mut n, mut ok) = (0, Thread32First(snap, &mut e).is_ok());
                while ok {
                    n += (e.th32OwnerProcessID == pid) as usize;
                    ok = Thread32Next(snap, &mut e).is_ok();
                }
                let _ = windows::Win32::Foundation::CloseHandle(snap);
                n
            }
        }

        #[test]
        fn many_jobs_do_not_create_many_threads() {
            let p = pool(4, 16, 250, 4096);
            let before = process_threads();
            let ids = Arc::new(Mutex::new(std::collections::HashSet::new()));
            let done = Arc::new(AtomicUsize::new(0));
            for _ in 0..3000 {
                while !p.submit(Box::new({
                    let (ids, done) = (ids.clone(), done.clone());
                    move || {
                        ids.lock().unwrap().insert(std::thread::current().id());
                        done.fetch_add(1, Ordering::SeqCst);
                    }
                })) {
                    std::thread::sleep(Duration::from_millis(1)); // queue full: wait for the workers
                }
            }
            let t0 = Instant::now();
            while done.load(Ordering::SeqCst) < 3000 && t0.elapsed() < Duration::from_secs(10) {
                std::thread::sleep(Duration::from_millis(5));
            }
            assert_eq!(done.load(Ordering::SeqCst), 3000);
            let distinct = ids.lock().unwrap().len();
            let after = process_threads();
            println!("3000 jobs ran on {distinct} threads (workers {}); process threads {before} -> {after} (other tests run in parallel); a thread per job would have been 3000", p.workers());
            assert!(distinct <= 4, "{distinct} threads ran 3000 quick jobs");
            assert_eq!(p.workers(), 4);
        }

        #[test]
        fn a_blocking_job_does_not_starve_the_others() {
            let p = pool(2, 8, 80, 256);
            let (tx, rx) = mpsc::channel();
            // both permanent workers block for 700 ms (a hung download) ...
            for _ in 0..2 {
                let tx = tx.clone();
                assert!(p.submit(Box::new(move || {
                    std::thread::sleep(Duration::from_millis(700));
                    tx.send("slow").unwrap();
                })));
            }
            std::thread::sleep(Duration::from_millis(20));
            // ... a quick job queued behind them still runs long before they end
            let t0 = Instant::now();
            let tx2 = tx.clone();
            assert!(p.submit(Box::new(move || tx2.send("quick").unwrap())));
            assert_eq!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), "quick");
            assert!(t0.elapsed() < Duration::from_millis(500), "the quick job waited {:?}", t0.elapsed());
            assert!(p.workers() >= 3 && p.workers() <= 8);
            assert_eq!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), "slow");
            assert_eq!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), "slow");
        }

        #[test]
        fn the_queue_and_the_extra_workers_are_bounded() {
            let p = pool(2, 4, 30, 5);
            let gate = Arc::new((Mutex::new(false), Condvar::new()));
            let mut accepted = 0;
            for _ in 0..40 {
                let gate = gate.clone();
                if p.submit(Box::new(move || {
                    let (m, c) = &*gate;
                    let mut open = m.lock().unwrap();
                    while !*open {
                        open = c.wait(open).unwrap();
                    }
                })) {
                    accepted += 1;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            std::thread::sleep(Duration::from_millis(300));
            assert!(p.workers() <= 4, "workers {}", p.workers());
            assert!(accepted <= 4 + 5, "accepted {accepted}: 4 running + a queue of 5 at most");
            *gate.0.lock().unwrap() = true;
            gate.1.notify_all();
        }
    }
}

// ================================================================ the widget trait

pub trait Widget {
    /// Record the STATIC content. Called again only when `version()` changed, the day changed or `cx.invalidate()` was called.
    fn draw(&mut self, cx: &mut Cx) -> Result<()>;
    /// Paint the ANIMATED part over the cached layer (every frame while `tick` animates). Same coordinates as `draw`.
    /// In previews (`--sheet`, `--dump`, gallery) it is called once per composite regardless of `tick`.
    fn overlay(&mut self, _cx: &mut Cx) -> Result<()> {
        Ok(())
    }
    /// Called at start, when the returned schedule is due, and after a background job finished. Update state, poll `Pending`s,
    /// start jobs (`cx.spawn`). Do not draw. `cx.invalidate()` is allowed (shared ref). Return the next schedule.
    fn tick(&mut self, cx: &Cx) -> Tick;
    /// Card-local logical-px mouse event. Return true if something visible changed (a frame is produced).
    fn mouse(&mut self, _ev: &MouseEv, _cx: &mut Cx) -> bool {
        false
    }
    /// Bump whenever the static content should be re-recorded (any state shown by `draw`).
    fn version(&self) -> u64 {
        0
    }
}

// ================================================================ Cx

/// Per-card flags shared between the framework and `Cx` (interior mutability so `&Cx` can set them).
#[derive(Default)]
pub struct Flags {
    pub invalidate: Cell<bool>,
    pub frame: Cell<bool>,
    pub cfg_dirty: Cell<bool>,
}

/// Everything a widget receives: card geometry, time, settings, and the drawing helpers.
pub struct Cx<'a> {
    pub size: Size,
    /// logical card size (170x170, 356x170 or 356x356 at scale 1)
    pub w: f32,
    pub h: f32,
    /// UI scale (physical px per logical px), informational: all helpers take logical px
    pub scale: f32,
    /// local time (fixed in previews)
    pub now: Now,
    /// true in the gallery, `--sheet` and `--dump`: show deterministic SAMPLE data, no I/O, no network, no jobs
    pub preview: bool,
    /// backdrop luminance behind this card, 0 (dark) .. 1 (bright)
    pub lum: f32,
    pub dark_backdrop: bool,
    pub lang: Lang,
    /// per-instance persisted settings (a JSON object); call `cx.save_cfg()` after changing it
    pub cfg: &'a mut Value,
    /// cursor position inside the card (logical px) while the mouse is over it
    pub hover: Option<Pt>,
    /// raw access for power users (bitmaps, effects); drawing outside draw/overlay is invalid
    pub dc: &'a ID2D1DeviceContext,
    pub gfx: &'a Gfx,
    res: &'a Res,
    flags: &'a Flags,
    waker: Waker,
    utc_off: i32,
    base: Matrix3x2,
    cur: Cell<Matrix3x2>,
    clips: RefCell<Vec<bool>>,
    /// the card is in its light (milky glass) appearance: palette whites become dark ink (see `Cx::ink`)
    light: bool,
    /// host chrome (gallery, settings, strip): never tinted by `accentScope` "all"
    pub(crate) plain: bool,
}

/// Row-vector matrix product: apply `a` first, then `b`.
fn mul(a: &Matrix3x2, b: &Matrix3x2) -> Matrix3x2 {
    Matrix3x2 {
        M11: a.M11 * b.M11 + a.M12 * b.M21,
        M12: a.M11 * b.M12 + a.M12 * b.M22,
        M21: a.M21 * b.M11 + a.M22 * b.M21,
        M22: a.M21 * b.M12 + a.M22 * b.M22,
        M31: a.M31 * b.M11 + a.M32 * b.M21 + b.M31,
        M32: a.M31 * b.M12 + a.M32 * b.M22 + b.M32,
    }
}
fn rotation(deg: f32, c: Pt) -> Matrix3x2 {
    let (s, k) = deg.to_radians().sin_cos();
    Matrix3x2 { M11: k, M12: s, M21: -s, M22: k, M31: c.0 - c.0 * k + c.1 * s, M32: c.1 - c.0 * s - c.1 * k }
}

/// What the framework hands over to build a `Cx` (not for widget code).
pub struct CxParts<'a> {
    pub gfx: &'a Gfx,
    pub res: &'a Res,
    pub flags: &'a Flags,
    pub cfg: &'a mut Value,
    pub size: Size,
    pub scale: f32,
    pub now: Now,
    pub preview: bool,
    pub lum: f32,
    pub lang: Lang,
    pub hover: Option<Pt>,
    pub waker: Waker,
    pub utc_off: i32,
    /// transform that maps logical card px to the target (scale + origin)
    pub base: Matrix3x2,
    /// record for the light glass appearance (dark ink on milky glass)
    pub light: bool,
}

impl<'a> Cx<'a> {
    pub fn new(p: CxParts<'a>) -> Cx<'a> {
        let (w, h) = p.size.logical();
        Cx {
            size: p.size,
            w,
            h,
            scale: p.scale,
            now: p.now,
            preview: p.preview,
            lum: p.lum,
            dark_backdrop: p.lum < 0.5,
            lang: p.lang,
            cfg: p.cfg,
            hover: p.hover,
            dc: &p.gfx.dc,
            gfx: p.gfx,
            res: p.res,
            flags: p.flags,
            waker: p.waker,
            utc_off: p.utc_off,
            base: p.base,
            cur: Cell::new(p.base),
            clips: RefCell::new(Vec::new()),
            light: p.light,
            plain: false,
        }
    }

    // ------------------------------------------------ framework services

    /// Re-record the static content (`draw`) on the next frame.
    pub fn invalidate(&self) {
        self.flags.invalidate.set(true);
    }
    /// Produce one frame (overlay repaint) soon even if `tick` is `Never`.
    pub fn request_frame(&self) {
        self.flags.frame.set(true);
    }
    /// Persist `cx.cfg` into config.json (widgetsLayout[i].cfg). Cheap; call after every user change.
    pub fn save_cfg(&self) {
        self.flags.cfg_dirty.set(true);
    }
    /// Read a string setting.
    pub fn cfg_str(&self, key: &str) -> Option<&str> {
        self.cfg.get(key).and_then(|v| v.as_str())
    }
    pub fn cfg_f64(&self, key: &str) -> Option<f64> {
        self.cfg.get(key).and_then(|v| v.as_f64())
    }
    pub fn cfg_bool(&self, key: &str) -> Option<bool> {
        self.cfg.get(key).and_then(|v| v.as_bool())
    }
    /// Set a key of the settings object (turns `cfg` into an object if it is not one) and schedule saving.
    pub fn cfg_set(&mut self, key: &str, v: Value) {
        if !self.cfg.is_object() {
            *self.cfg = json!({});
        }
        self.cfg[key] = v;
        self.save_cfg();
    }
    /// Run `job` on a worker thread (HTTP, PowerShell, WMI, file scans); when it finishes the UI thread wakes and calls `tick`,
    /// where you `take()` the result. In preview mode nothing runs. The job must not touch Direct2D (UI thread only).
    pub fn spawn<T: Send + 'static>(&self, job: impl FnOnce() -> T + Send + 'static) -> Pending<T> {
        let p = Pending::<T>::default();
        if self.preview {
            return p;
        }
        let (inner, waker) = (p.0.clone(), self.waker);
        inner.busy.store(true, Ordering::Relaxed);
        // a pool worker runs it (see `jobpool`): the same body as the old per-job thread, in submission order
        let queued = jobpool::global().submit(Box::new(move || {
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(job));
            if let Ok(v) = r {
                if let Ok(mut g) = inner.val.lock() {
                    *g = Some(v);
                }
            }
            inner.busy.store(false, Ordering::Relaxed);
            waker.wake();
        }));
        if !queued {
            p.0.busy.store(false, Ordering::Relaxed);
        }
        p
    }
    /// Ask the user for a line of text (a glass panel on the host's canvas next to the cursor, with its own text field; Enter = OK, Esc = cancel;
    /// `multiline`: Ctrl+Enter; the Win32 popup of input.rs only when the host cannot show the panel).
    /// Returns a `Pending`: poll `take()` in `tick`; the value is `Some(text)` (non-empty) or `None` when cancelled. No-op in preview mode.
    pub fn prompt_text(&self, title: &str, initial: &str, multiline: bool) -> Pending<Option<String>> {
        self.prompt_text_ex(title, initial, multiline, "", "")
    }
    /// `prompt_text` plus a dimmed `placeholder` (shown while the field is empty) and the label of the primary button
    /// (`""` = "Готово" / "Done"); the prompt follows the widget language.
    pub fn prompt_text_ex(&self, title: &str, initial: &str, multiline: bool, placeholder: &str, ok_label: &str) -> Pending<Option<String>> {
        let mut o = crate::input::PromptOpts::new(title, initial, multiline);
        (o.placeholder, o.ok_label, o.lang) = (placeholder.to_string(), ok_label.to_string(), self.lang);
        self.spawn(move || crate::input::prompt_ex(o))
    }
    /// A waker for your own threads (e.g. a long-lived listener): `waker.wake()` makes the framework call `tick`.
    pub fn waker(&self) -> Waker {
        self.waker
    }
    /// Launch a URL / app / power action / media key. No-op in preview mode.
    pub fn invoke_system(&self, a: Action) {
        if !self.preview {
            sys::perform(&a);
        }
    }
    /// A click on the body of widget `id`'s card: open the Windows tool mapped to it in `sys::targets::of` (Task Manager, This PC, ...).
    /// No-op in preview mode (like `invoke_system`) and for ids without a target.
    /// The card's own "Click action" (`cfg.click`, set from the card's context menu) wins over the table: another preset, a custom
    /// URL / program, or nothing at all.
    pub fn open_card_target(&self, id: &str) {
        if let Some(t) = sys::targets::resolve(id, self.cfg, ("", "")) {
            self.invoke_system(Action::Open(t));
        }
    }
    /// The weather card's click outside the city label: the default search (or the site chosen in "Click action") for `city`;
    /// `word` is the localized "weather".
    pub fn open_weather(&self, word: &str, city: &str) {
        if let Some(t) = sys::targets::resolve("weather", self.cfg, (word, city)) {
            self.invoke_system(Action::Open(t));
        }
    }
    /// Current UTC instant as unix seconds (derived from `now` and the local offset, so previews are stable).
    pub fn utc_secs(&self) -> i64 {
        self.now.local_secs() - self.utc_off as i64 * 60
    }
    /// Local UTC offset in minutes.
    pub fn utc_offset_min(&self) -> i32 {
        self.utc_off
    }
    /// Wall clock of a table zone right now.
    pub fn zone_time(&self, z: &tz::Zone) -> tz::Civil {
        z.at(self.utc_secs())
    }
    /// ru / en string by the UI language.
    pub fn t<'b>(&self, ru: &'b str, en: &'b str) -> &'b str {
        self.lang.pick(ru, en)
    }
    /// the card rectangle (0,0,w,h)
    pub fn bounds(&self) -> Rect {
        Rect::new(0.0, 0.0, self.w, self.h)
    }
    /// the card minus the standard 16 px padding
    pub fn content(&self) -> Rect {
        self.bounds().inset(16.0)
    }
    /// true while the cursor is inside `r`
    pub fn hovering(&self, r: Rect) -> bool {
        self.hover.is_some_and(|(x, y)| r.contains(x, y))
    }

    // ------------------------------------------------ images

    /// Device bitmap from a decoded image (UI thread). Decode on a worker with `decode_image_file` / `decode_image_bytes`.
    pub fn image(&self, d: &Decoded) -> Result<Bitmap> {
        self.gfx.from_bgra(d.w, d.h, &d.bgra)
    }
    /// Convenience for small local files (decodes on the calling thread: use a job for big images).
    pub fn image_file(&self, path: &str, max_w: u32, max_h: u32) -> Option<Bitmap> {
        decode_image_file(path, max_w, max_h).and_then(|d| self.image(&d).ok())
    }

    // ------------------------------------------------ primitives

    /// Light appearance palette: white (any alpha) -> dark ink with the same alpha, the dark `INK` -> white, saturated accents a little
    /// deeper. Identity in the dark appearance.
    fn ink(&self, c: Color) -> Color {
        let c = self.ink_plain(c);
        if !self.plain && TINT.load(Ordering::Relaxed) {
            tinted(c, self.light)
        } else {
            c
        }
    }
    fn ink_plain(&self, c: Color) -> Color {
        if !self.light {
            return c;
        }
        let (mx, mn) = (c.r.max(c.g).max(c.b), c.r.min(c.g).min(c.b));
        if mn > 0.995 {
            Color::rgba(0.07, 0.08, 0.10, c.a)
        } else if mx < 0.2 && mx > 0.05 {
            Color::rgba(1.0, 1.0, 1.0, c.a)
        } else if mx - mn > 0.2 {
            Color::rgba(c.r * 0.8, c.g * 0.8, c.b * 0.8, c.a)
        } else {
            c
        }
    }
    fn brush(&self, c: Color) -> &ID2D1SolidColorBrush {
        unsafe { self.res.brush.SetColor(&self.ink(c).d2d()) };
        &self.res.brush
    }
    fn set_tf(&self, m: Matrix3x2) {
        self.cur.set(m);
        unsafe { self.dc.SetTransform(&m) };
    }

    fn layout(&self, s: &str, st: Style, w: f32, h: f32) -> Result<IDWriteTextLayout> {
        let f = self.res.format(self.gfx, st)?;
        let wide: Vec<u16> = s.encode_utf16().collect();
        unsafe {
            let lay = self.gfx.dw.CreateTextLayout(&wide, &f, w.max(0.0), h.max(0.0))?;
            let range = DWRITE_TEXT_RANGE { startPosition: 0, length: wide.len() as u32 };
            let track = if st.track != 0.0 { st.track } else { auto_track(st.size) };
            if track != 0.0 {
                if let Ok(l1) = lay.cast::<IDWriteTextLayout1>() {
                    let _ = l1.SetCharacterSpacing(0.0, track, 0.0, range);
                }
            }
            // Inter's stylistic set 3 = round commas (the default comma hangs far below the baseline: ugly in a big "9,0"); tabular figures for numbers that
            // change in place, but not at display sizes: a tabular "9,0" at 40 px has wide holes around the comma, and nothing jitters at that size anyway
            if let Ok(t) = self.gfx.dw.CreateTypography() {
                let _ = t.AddFontFeature(DWRITE_FONT_FEATURE { nameTag: DWRITE_FONT_FEATURE_TAG_STYLISTIC_SET_3, parameter: 1 });
                if st.mono && st.size < 36.0 {
                    let _ = t.AddFontFeature(DWRITE_FONT_FEATURE { nameTag: DWRITE_FONT_FEATURE_TAG_TABULAR_FIGURES, parameter: 1 });
                }
                let _ = lay.SetTypography(&t, range);
            }
            Ok(lay)
        }
    }

    /// Text inside `r` (DirectWrite, Segoe UI Variable). Colour is a palette constant (`TEXT`, `DIM`, `FAINT`, `ACCENT`...).
    pub fn text(&self, r: Rect, s: &str, st: Style, c: Color) {
        if s.is_empty() {
            return;
        }
        if text_lint() {
            // `MSW_TEXT_LINT=1`: one TSV line per text run (card size, box, natural size, font size / weight / alignment, colour alpha) for tools/text-lint.py
            let (nw, nh) = self.measure(s, st, r.w);
            let al = match st.align {
                Align::Left => 'L',
                Align::Center => 'C',
                Align::Right => 'R',
            };
            let chars: String = s.chars().take(22).collect();
            eprintln!("TXT\t{:?}\t{:.1}\t{:.1}\t{:.1}\t{:.1}\t{:.1}\t{:.1}\t{}\t{}\t{}\t{:.2}\t{}", self.size, r.x, r.y, r.w, r.h, nw, nh, st.size, st.weight, al, c.a, chars.replace('\t', " "));
        }
        if let Ok(l) = self.layout(s, st, r.w, r.h) {
            // `MSW_TEXT_SNAP=1`: the text origin on whole pixels, so stems do not straddle pixel boundaries (needs an upright, unscaled card)
            let (x, y) = if std::env::var_os("MSW_TEXT_SNAP").is_some() { (r.x.round(), r.y.round()) } else { (r.x, r.y) };
            unsafe { self.dc.DrawTextLayout(pt(x, y), &l, self.brush(c), D2D1_DRAW_TEXT_OPTIONS_NONE) };
        }
    }
    /// Natural (width, height) of `s` in this style; `max_w` only matters for wrapped styles.
    pub fn measure(&self, s: &str, st: Style, max_w: f32) -> (f32, f32) {
        let st = Style { valign: VAlign::Top, ellipsis: false, align: Align::Left, ..st };
        let Ok(l) = self.layout(s, st, if st.wrap { max_w } else { 4096.0 }, 4096.0) else { return (0.0, 0.0) };
        let mut m = DWRITE_TEXT_METRICS::default();
        unsafe {
            let _ = l.GetMetrics(&mut m);
        }
        (m.width, m.height)
    }

    /// Filled rectangle.
    pub fn rect(&self, r: Rect, c: Color) {
        unsafe { self.dc.FillRectangle(&rf(r.x, r.y, r.right(), r.bottom()), self.brush(c)) };
    }
    /// Filled rounded rectangle.
    pub fn rrect(&self, r: Rect, rad: f32, c: Color) {
        let rad = rad.min(r.w / 2.0).min(r.h / 2.0).max(0.0);
        unsafe { self.dc.FillRoundedRectangle(&rr(r.x, r.y, r.w, r.h, rad), self.brush(c)) };
    }
    /// Outlined rounded rectangle (stroke centred on the edge).
    pub fn rrect_stroke(&self, r: Rect, rad: f32, width: f32, c: Color) {
        let rad = rad.min(r.w / 2.0).min(r.h / 2.0).max(0.0);
        unsafe { self.dc.DrawRoundedRectangle(&rr(r.x, r.y, r.w, r.h, rad), self.brush(c), width, &self.res.round) };
    }
    /// Pill (rounded rectangle with radius h/2).
    pub fn pill(&self, r: Rect, c: Color) {
        self.rrect(r, r.h / 2.0, c);
    }
    pub fn circle(&self, c: Pt, r: f32, color: Color) {
        unsafe { self.dc.FillEllipse(&D2D1_ELLIPSE { point: pt(c.0, c.1), radiusX: r, radiusY: r }, self.brush(color)) };
    }
    pub fn circle_stroke(&self, c: Pt, r: f32, width: f32, color: Color) {
        unsafe { self.dc.DrawEllipse(&D2D1_ELLIPSE { point: pt(c.0, c.1), radiusX: r, radiusY: r }, self.brush(color), width, &self.res.round) };
    }
    /// Line with round caps.
    pub fn line(&self, a: Pt, b: Pt, width: f32, c: Color) {
        unsafe { self.dc.DrawLine(pt(a.0, a.1), pt(b.0, b.1), self.brush(c), width, &self.res.round) };
    }
    /// Hairline separator across `r` at its vertical middle.
    pub fn hr(&self, r: Rect) {
        self.line((r.x, r.cy()), (r.right(), r.cy()), 0.8, HAIR);
    }
    /// Arc stroke: `start` degrees clockwise from 12 o'clock, `sweep` degrees clockwise (negative = counter-clockwise).
    pub fn arc(&self, c: Pt, r: f32, width: f32, start: f32, sweep: f32, color: Color) {
        if sweep.abs() < 0.05 {
            return;
        }
        if sweep.abs() >= 359.9 {
            return self.circle_stroke(c, r, width, color);
        }
        let at = |deg: f32| {
            let a = deg.to_radians();
            pt(c.0 + r * a.sin(), c.1 - r * a.cos())
        };
        unsafe {
            let Ok(g) = self.gfx.d2d.CreatePathGeometry() else { return };
            let Ok(sink) = g.Open() else { return };
            sink.BeginFigure(at(start), D2D1_FIGURE_BEGIN_HOLLOW);
            sink.AddArc(&D2D1_ARC_SEGMENT {
                point: at(start + sweep),
                size: D2D_SIZE_F { width: r, height: r },
                rotationAngle: 0.0,
                sweepDirection: if sweep > 0.0 { D2D1_SWEEP_DIRECTION_CLOCKWISE } else { D2D1_SWEEP_DIRECTION_COUNTER_CLOCKWISE },
                arcSize: if sweep.abs() > 180.0 { D2D1_ARC_SIZE_LARGE } else { D2D1_ARC_SIZE_SMALL },
            });
            sink.EndFigure(D2D1_FIGURE_END_OPEN);
            if sink.Close().is_ok() {
                self.dc.DrawGeometry(&g, self.brush(color), width, &self.res.round);
            }
        }
    }
    /// Ring gauge: a faint full track plus the arc for `frac` (0..1) from 12 o'clock clockwise, round caps (batteries, disks, timers).
    pub fn ring(&self, c: Pt, r: f32, stroke: f32, frac: f32, color: Color) {
        self.circle_stroke(c, r, stroke, FILL);
        let f = frac.clamp(0.0, 1.0);
        if f > 0.0 {
            self.arc(c, r, stroke, 0.0, 360.0 * f, color);
        }
    }
    /// Horizontal progress bar: faint pill track + pill fill of `frac` (0..1).
    pub fn bar(&self, r: Rect, frac: f32, color: Color) {
        self.pill(r, FILL);
        let f = frac.clamp(0.0, 1.0);
        if f > 0.0 {
            self.pill(Rect::new(r.x, r.y, (r.w * f).max(r.h), r.h), color);
        }
    }
    /// Linear gradient brush from `p0` to `p1` (logical px) with colour stops.
    fn gradient(&self, p0: Pt, p1: Pt, stops: &[(f32, Color)]) -> Option<ID2D1LinearGradientBrush> {
        unsafe {
            let st: Vec<D2D1_GRADIENT_STOP> = stops.iter().map(|&(p, c)| D2D1_GRADIENT_STOP { position: p, color: self.ink(c).d2d() }).collect();
            let coll = self.dc.CreateGradientStopCollection(&st, D2D1_COLOR_SPACE_SRGB, D2D1_COLOR_SPACE_SRGB, D2D1_BUFFER_PRECISION_8BPC_UNORM, D2D1_EXTEND_MODE_CLAMP, D2D1_COLOR_INTERPOLATION_MODE_STRAIGHT).ok()?;
            self.dc.CreateLinearGradientBrush(&D2D1_LINEAR_GRADIENT_BRUSH_PROPERTIES { startPoint: pt(p0.0, p0.1), endPoint: pt(p1.0, p1.1) }, None, &coll).ok()
        }
    }
    /// Rounded rectangle filled with a gradient: `angle` degrees clockwise from "up" (180 = top to bottom).
    pub fn gradient_rrect(&self, r: Rect, rad: f32, c0: Color, c1: Color, angle: f32) {
        let a = angle.to_radians();
        let (dx, dy) = (a.sin(), -a.cos());
        let len = (r.w * dx).abs() + (r.h * dy).abs();
        let p0 = (r.cx() - dx * len / 2.0, r.cy() - dy * len / 2.0);
        let p1 = (r.cx() + dx * len / 2.0, r.cy() + dy * len / 2.0);
        if let Some(b) = self.gradient(p0, p1, &[(0.0, c0), (1.0, c1)]) {
            let rad = rad.min(r.w / 2.0).min(r.h / 2.0).max(0.0);
            unsafe { self.dc.FillRoundedRectangle(&rr(r.x, r.y, r.w, r.h, rad), &b) };
        }
    }
    /// Line chart with a soft area under it, auto-ranged to the data (min..max). Needs >= 2 values.
    pub fn sparkline(&self, r: Rect, values: &[f32], color: Color) {
        let lo = values.iter().cloned().fold(f32::MAX, f32::min);
        let hi = values.iter().cloned().fold(f32::MIN, f32::max);
        self.sparkline_range(r, values, lo, if hi > lo { hi } else { lo + 1.0 }, color);
    }
    /// Like `sparkline` with a fixed value range (e.g. 0..100 for percentages).
    pub fn sparkline_range(&self, r: Rect, values: &[f32], lo: f32, hi: f32, color: Color) {
        let n = values.len();
        if n < 2 || hi <= lo {
            return;
        }
        let p = |i: usize| (r.x + r.w * i as f32 / (n - 1) as f32, r.bottom() - r.h * ((values[i] - lo) / (hi - lo)).clamp(0.0, 1.0));
        unsafe {
            let (Ok(line), Ok(area)) = (self.gfx.d2d.CreatePathGeometry(), self.gfx.d2d.CreatePathGeometry()) else { return };
            let (Ok(sl), Ok(sa)) = (line.Open(), area.Open()) else { return };
            let p0 = p(0);
            sl.BeginFigure(pt(p0.0, p0.1), D2D1_FIGURE_BEGIN_HOLLOW);
            sa.BeginFigure(pt(p0.0, r.bottom()), D2D1_FIGURE_BEGIN_FILLED);
            sa.AddLine(pt(p0.0, p0.1));
            for i in 1..n {
                let q = p(i);
                sl.AddLine(pt(q.0, q.1));
                sa.AddLine(pt(q.0, q.1));
            }
            sa.AddLine(pt(r.right(), r.bottom()));
            sl.EndFigure(D2D1_FIGURE_END_OPEN);
            sa.EndFigure(D2D1_FIGURE_END_CLOSED);
            if sl.Close().is_err() || sa.Close().is_err() {
                return;
            }
            if let Some(b) = self.gradient((0.0, r.y), (0.0, r.bottom()), &[(0.0, color.fade(0.35)), (1.0, color.fade(0.0))]) {
                self.dc.FillGeometry(&area, &b, None::<&ID2D1Brush>);
            }
            self.dc.DrawGeometry(&line, self.brush(color), 1.6, &self.res.round);
        }
    }

    /// Built-in vector glyph scaled into `r` (drawn in a square fitted to the rect), tinted with `color`.
    pub fn icon(&self, g: Glyph, r: Rect, color: Color) {
        let Ok(shapes) = self.res.glyph(self.gfx, g) else { return };
        let k = r.w.min(r.h) / 24.0;
        let (ox, oy) = (r.cx() - 12.0 * k, r.cy() - 12.0 * k);
        let saved = self.cur.get();
        self.set_tf(mul(&Matrix3x2 { M11: k, M12: 0.0, M21: 0.0, M22: k, M31: ox, M32: oy }, &saved));
        unsafe {
            let b = self.brush(color);
            for (geom, paint) in shapes.iter() {
                match *paint {
                    icons::Paint::Fill => self.dc.FillGeometry(geom, b, None::<&ID2D1Brush>),
                    icons::Paint::Stroke(w) => self.dc.DrawGeometry(geom, b, w, &self.res.round),
                    icons::Paint::Both(w) => {
                        self.dc.FillGeometry(geom, b, None::<&ID2D1Brush>);
                        self.dc.DrawGeometry(geom, b, w, &self.res.round);
                    }
                }
            }
        }
        self.set_tf(saved);
    }

    /// Bitmap (album art, photo, app icon) into `r`.
    pub fn bitmap(&self, r: Rect, bmp: &Bitmap, fit: Fit) {
        self.bitmap_round(r, bmp, fit, 0.0);
    }
    /// Bitmap with rounded corners.
    pub fn bitmap_round(&self, r: Rect, bmp: &Bitmap, fit: Fit, rad: f32) {
        unsafe {
            let sz = bmp.GetPixelSize();
            let (bw, bh) = (sz.width.max(1) as f32, sz.height.max(1) as f32);
            // brush transform: bitmap px -> card px
            let (k, tx, ty, dest) = match fit {
                Fit::Cover => {
                    let k = (r.w / bw).max(r.h / bh);
                    (k, r.cx() - bw * k / 2.0, r.cy() - bh * k / 2.0, r)
                }
                Fit::Contain => {
                    let k = (r.w / bw).min(r.h / bh);
                    let d = r.centered(bw * k, bh * k);
                    (k, d.x, d.y, d)
                }
                Fit::Stretch => (1.0, r.x, r.y, r),
            };
            let (kx, ky) = if fit == Fit::Stretch { (r.w / bw, r.h / bh) } else { (k, k) };
            let bp = D2D1_BITMAP_BRUSH_PROPERTIES1 { extendModeX: D2D1_EXTEND_MODE_CLAMP, extendModeY: D2D1_EXTEND_MODE_CLAMP, interpolationMode: D2D1_INTERPOLATION_MODE_LINEAR };
            let props = D2D1_BRUSH_PROPERTIES { opacity: 1.0, transform: Matrix3x2 { M11: kx, M12: 0.0, M21: 0.0, M22: ky, M31: tx, M32: ty } };
            let Ok(b) = self.dc.CreateBitmapBrush(bmp, Some(&bp), Some(&props)) else { return };
            if rad > 0.0 {
                self.dc.FillRoundedRectangle(&rr(dest.x, dest.y, dest.w, dest.h, rad.min(dest.w / 2.0).min(dest.h / 2.0)), &b);
            } else {
                self.dc.FillRectangle(&rf(dest.x, dest.y, dest.right(), dest.bottom()), &b);
            }
        }
    }

    /// Clip everything drawn until `pop_clip` to a rounded rectangle (anti-aliased). Prefer `with_clip_round`.
    pub fn clip_round(&self, r: Rect, rad: f32) {
        unsafe {
            let rad = rad.min(r.w / 2.0).min(r.h / 2.0).max(0.0);
            let Ok(g) = self.gfx.d2d.CreateRoundedRectangleGeometry(&rr(r.x, r.y, r.w, r.h, rad)) else { return self.clips.borrow_mut().push(false) };
            let mut lp = D2D1_LAYER_PARAMETERS1 {
                contentBounds: rf(-3.0e38, -3.0e38, 3.0e38, 3.0e38),
                geometricMask: ManuallyDrop::new(Some(g.cast().unwrap_or_else(|_| unreachable!()))),
                maskAntialiasMode: D2D1_ANTIALIAS_MODE_PER_PRIMITIVE,
                maskTransform: mat(1.0, 0.0, 0.0), // the mask is interpreted in the current user space
                opacity: 1.0,
                opacityBrush: ManuallyDrop::new(None),
                layerOptions: D2D1_LAYER_OPTIONS1_NONE,
            };
            self.dc.PushLayer(&lp, None::<&ID2D1Layer>);
            ManuallyDrop::drop(&mut lp.geometricMask);
            self.clips.borrow_mut().push(true);
        }
    }
    /// Clip to a plain rectangle until `pop_clip`.
    pub fn clip(&self, r: Rect) {
        unsafe { self.dc.PushAxisAlignedClip(&rf(r.x, r.y, r.right(), r.bottom()), D2D1_ANTIALIAS_MODE_PER_PRIMITIVE) };
        self.clips.borrow_mut().push(false);
    }
    pub fn pop_clip(&self) {
        let layer = self.clips.borrow_mut().pop();
        unsafe {
            match layer {
                Some(true) => self.dc.PopLayer(),
                Some(false) => self.dc.PopAxisAlignedClip(),
                None => {}
            }
        }
    }
    /// `f` draws clipped to a rounded rectangle.
    pub fn with_clip_round(&self, r: Rect, rad: f32, f: impl FnOnce(&Cx)) {
        self.clip_round(r, rad);
        f(self);
        self.pop_clip();
    }
    /// `f` draws as one group at opacity `k` (a picture that cross-fades into the next one).
    pub fn with_opacity(&self, k: f32, f: impl FnOnce(&Cx)) {
        if k >= 0.999 {
            return f(self);
        }
        let lp = D2D1_LAYER_PARAMETERS1 {
            contentBounds: rf(-3.0e38, -3.0e38, 3.0e38, 3.0e38),
            geometricMask: ManuallyDrop::new(None),
            maskAntialiasMode: D2D1_ANTIALIAS_MODE_PER_PRIMITIVE,
            maskTransform: mat(1.0, 0.0, 0.0),
            opacity: k.clamp(0.0, 1.0),
            opacityBrush: ManuallyDrop::new(None),
            layerOptions: D2D1_LAYER_OPTIONS1_NONE,
        };
        unsafe { self.dc.PushLayer(&lp, None::<&ID2D1Layer>) };
        f(self);
        unsafe { self.dc.PopLayer() };
    }
    /// `f` draws rotated by `deg` (clockwise) around `center`.
    pub fn rotated(&self, center: Pt, deg: f32, f: impl FnOnce(&Cx)) {
        let saved = self.cur.get();
        self.set_tf(mul(&rotation(deg, center), &saved));
        f(self);
        self.set_tf(saved);
    }
    /// `f` draws scaled by `k` about `center` (a pressed button, a knob that swells).
    pub fn scaled(&self, center: Pt, k: f32, f: impl FnOnce(&Cx)) {
        let saved = self.cur.get();
        self.set_tf(mul(&Matrix3x2 { M11: k, M12: 0.0, M21: 0.0, M22: k, M31: center.0 - k * center.0, M32: center.1 - k * center.1 }, &saved));
        f(self);
        self.set_tf(saved);
    }
    /// Framework: balance unclosed clips and the transform after a callback.
    pub fn finish(&self) {
        while !self.clips.borrow().is_empty() {
            self.pop_clip();
        }
        self.set_tf(self.base);
    }
}
