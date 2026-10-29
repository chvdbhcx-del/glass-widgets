//! Host-agnostic input: the neutral [`InputEvent`] every control of the kit consumes, plus hit-testing, keyboard focus ([`FocusChain`]),
//! click counting ([`ClickCounter`]) and the cursor request ([`Cursor`]) a control hands back in its [`Response`].
//!
//! Controls never see `WM_*` messages, so the same widget runs in a layered overlay window, a normal window, a headless test or the
//! gallery. All positions are **logical pixels** relative to the window's client origin (the host divides device pixels by its scale).
//!
//! # Mapping a Win32 window to this module
//! [`win32::Translator`] does it for you; the table of what it does (so you can do it by hand in another host):
//!
//! | Win32                                   | [`InputEvent`]                                                              |
//! |-----------------------------------------|-----------------------------------------------------------------------------|
//! | `WM_MOUSEMOVE` (+ `TrackMouseEvent`)    | `MouseMove`; `WM_MOUSELEAVE` -> `MouseLeave`                                |
//! | `WM_{L,R,M,X}BUTTONDOWN` / `...UP`      | `MouseDown` / `MouseUp` (call `SetCapture` while [`Response::capture`])     |
//! | `WM_*BUTTONDBLCLK`                      | `MouseDown` (the [`ClickCounter`] numbers the clicks, no `CS_DBLCLKS` needed)|
//! | `WM_MOUSEWHEEL` / `WM_MOUSEHWHEEL`      | `Wheel` (120 units = one notch = [`WHEEL_NOTCH_PX`]; wheel up = negative y)  |
//! | `WM_KEYDOWN` / `WM_KEYUP` (`SYSKEY` too)| `Key { code, down }`, modifiers from `GetKeyState`                          |
//! | `WM_CHAR` (UTF-16 units)                | `Char` (surrogate pairs joined, control characters dropped: Enter, Tab, Esc, Backspace arrive as `Key`) |
//! | `WM_SETFOCUS` / `WM_KILLFOCUS`          | `Focus(bool)`                                                               |
//! | `WM_DPICHANGED`                         | `DpiChanged(dpi / 96)`                                                      |
//! | `WM_IME_*`                              | `Ime` (**TODO**: composition strings need `ImmGetCompositionString`; the `Char` path is complete without it) |
//!
//! The host asks controls for the mouse cursor from [`Response::cursor`] and handles `WM_SETCURSOR` with it.
use crate::geom::{Pt, Rect};

/// One notch of the mouse wheel in logical px (what Chromium, i.e. the old web UI, scrolled per notch).
pub const WHEEL_NOTCH_PX: f32 = 100.0;

/// Keyboard modifiers held while an event happened.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Modifiers {
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
    /// the Windows key (or Command on a host that has one)
    pub meta: bool,
}

impl Modifiers {
    pub const NONE: Modifiers = Modifiers { shift: false, ctrl: false, alt: false, meta: false };
    pub const SHIFT: Modifiers = Modifiers { shift: true, ..Modifiers::NONE };
    pub const CTRL: Modifiers = Modifiers { ctrl: true, ..Modifiers::NONE };
    pub const ALT: Modifiers = Modifiers { alt: true, ..Modifiers::NONE };
    pub const CTRL_SHIFT: Modifiers = Modifiers { ctrl: true, shift: true, ..Modifiers::NONE };

    /// The "command" modifier of editing shortcuts (Ctrl on Windows).
    pub fn command(&self) -> bool {
        self.ctrl
    }

    /// Ctrl or Alt or Win held (a typed character with these is a shortcut, not text).
    pub fn any_command(&self) -> bool {
        self.ctrl || self.alt || self.meta
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    X1,
    X2,
}

/// The set of mouse buttons held down.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Buttons(u8);

impl Buttons {
    pub const NONE: Buttons = Buttons(0);

    fn bit(b: MouseButton) -> u8 {
        1 << (b as u8)
    }
    pub fn has(&self, b: MouseButton) -> bool {
        self.0 & Self::bit(b) != 0
    }
    pub fn with(mut self, b: MouseButton) -> Buttons {
        self.0 |= Self::bit(b);
        self
    }
    pub fn without(mut self, b: MouseButton) -> Buttons {
        self.0 &= !Self::bit(b);
        self
    }
    pub fn any(&self) -> bool {
        self.0 != 0
    }
    pub fn left(&self) -> bool {
        self.has(MouseButton::Left)
    }
}

/// A key by meaning, not by scan code. Letters are upper case (`Char('A')`), digits `Char('0')`; the text a key types arrives separately as
/// [`InputEvent::Char`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum KeyCode {
    Char(char),
    Enter,
    Escape,
    Tab,
    Backspace,
    Delete,
    Insert,
    Space,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    PageUp,
    PageDown,
    /// F1..F24
    F(u8),
    /// a key the kit does not name (the raw virtual-key code)
    Other(u32),
}

/// Input-method composition (East-Asian text entry). Placeholder: no host sends it yet, editors ignore it.
#[derive(Clone, Debug, PartialEq)]
pub enum ImeEvent {
    Start,
    /// the composition string so far and the caret inside it (in chars)
    Update { text: String, cursor: usize },
    Commit(String),
    End,
}

/// A host-independent input event (logical px, client coordinates).
#[derive(Clone, Debug, PartialEq)]
pub enum InputEvent {
    MouseMove { pos: Pt, mods: Modifiers, buttons: Buttons },
    /// `clicks` is 1 for a single click, 2 for the second click of a double click, 3 for a triple (see [`ClickCounter`])
    MouseDown { pos: Pt, button: MouseButton, mods: Modifiers, clicks: u8 },
    MouseUp { pos: Pt, button: MouseButton, mods: Modifiers },
    /// the pointer left the window
    MouseLeave,
    /// `delta` in logical px, positive y = the content moves up (scroll towards later content), positive x = towards the right.
    /// `precise` = a touchpad / high-resolution wheel (no notches: do not add your own smoothing on top of the spring)
    Wheel { pos: Pt, delta: Pt, mods: Modifiers, precise: bool },
    Key { code: KeyCode, mods: Modifiers, down: bool, repeat: bool },
    /// typed text (already composed: dead keys, AltGr, Cyrillic layouts), one scalar value
    Char(char),
    Focus(bool),
    Ime(ImeEvent),
    DpiChanged(f32),
}

impl InputEvent {
    /// Position of a pointer event.
    pub fn pos(&self) -> Option<Pt> {
        match self {
            InputEvent::MouseMove { pos, .. } | InputEvent::MouseDown { pos, .. } | InputEvent::MouseUp { pos, .. } | InputEvent::Wheel { pos, .. } => Some(*pos),
            _ => None,
        }
    }

    /// The same event with its position moved by `(dx, dy)` (a parent translating into a child's coordinates).
    pub fn translated(&self, dx: f32, dy: f32) -> InputEvent {
        let mut e = self.clone();
        match &mut e {
            InputEvent::MouseMove { pos, .. } | InputEvent::MouseDown { pos, .. } | InputEvent::MouseUp { pos, .. } | InputEvent::Wheel { pos, .. } => {
                pos.0 += dx;
                pos.1 += dy;
            }
            _ => {}
        }
        e
    }

    /// A key press (not a release) of `code`.
    pub fn key_down(&self) -> Option<(KeyCode, Modifiers, bool)> {
        match self {
            InputEvent::Key { code, mods, down: true, repeat } => Some((*code, *mods, *repeat)),
            _ => None,
        }
    }
}

/// The mouse cursor a control wants over its area.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cursor {
    Arrow,
    /// text I-beam
    Text,
    /// pointing hand (buttons, links)
    Hand,
    /// horizontal resize / slider drag
    ResizeEw,
    ResizeNs,
    /// not allowed
    No,
}

/// What a control tells its host after an event.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Response {
    /// the control used the event (the host does not pass it on)
    pub consumed: bool,
    /// something visible changed: draw again (animations ask for frames through `busy()` instead)
    pub redraw: bool,
    /// the control's value changed or it was activated (a click, a toggle, a slider step): read it
    pub changed: bool,
    /// the control wants the mouse captured (a press / drag in progress) so moves and the release arrive even outside its rectangle
    pub capture: bool,
    /// the control wants keyboard focus (a click on a text input)
    pub focus: bool,
    /// the cursor for the position of the event
    pub cursor: Option<Cursor>,
}

impl Response {
    pub const NONE: Response = Response { consumed: false, redraw: false, changed: false, capture: false, focus: false, cursor: None };

    pub fn redraw() -> Response {
        Response { redraw: true, ..Response::NONE }
    }
    pub fn consumed() -> Response {
        Response { consumed: true, ..Response::NONE }
    }
    pub fn with_cursor(mut self, c: Cursor) -> Response {
        self.cursor = Some(c);
        self
    }
    /// Combine two responses (a container asking several children): flags are OR-ed, the first cursor wins.
    pub fn merge(self, o: Response) -> Response {
        Response {
            consumed: self.consumed || o.consumed,
            redraw: self.redraw || o.redraw,
            changed: self.changed || o.changed,
            capture: self.capture || o.capture,
            focus: self.focus || o.focus,
            cursor: self.cursor.or(o.cursor),
        }
    }
}

// ---------------------------------------------------------------- hit testing

/// The topmost of `rects` under `p`; later rectangles are painted on top, so the LAST match wins.
pub fn hit_test(rects: &[Rect], p: Pt) -> Option<usize> {
    rects.iter().rposition(|r| r.contains(p.0, p.1))
}

/// Is `p` inside the rounded rectangle `r` (corner radius `rad`)? Use it for pills and circles, where a corner click must not count.
pub fn hit_rounded(r: Rect, rad: f32, p: Pt) -> bool {
    if !r.contains(p.0, p.1) {
        return false;
    }
    let rad = rad.min(r.w / 2.0).min(r.h / 2.0).max(0.0);
    let (dx, dy) = ((p.0 - r.cx()).abs() - (r.w / 2.0 - rad), (p.1 - r.cy()).abs() - (r.h / 2.0 - rad));
    dx <= 0.0 || dy <= 0.0 || dx * dx + dy * dy <= rad * rad
}

/// Grow a rectangle for hit-testing (fat finger targets: a 12 px knob is clickable inside a 24 px square).
pub fn expand(r: Rect, by: f32) -> Rect {
    Rect::new(r.x - by, r.y - by, r.w + 2.0 * by, r.h + 2.0 * by)
}

// ---------------------------------------------------------------- click counting

/// Numbers consecutive clicks: a second click within `interval_ms` and `max_dist` px of the first is a double click, the third a triple;
/// a fourth starts over. Pure: the host passes the message time (`GetMessageTime`) in milliseconds.
#[derive(Clone, Copy, Debug)]
pub struct ClickCounter {
    last: Option<(u32, Pt, MouseButton)>,
    count: u8,
    pub interval_ms: u32,
    pub max_dist: f32,
}

impl Default for ClickCounter {
    fn default() -> Self {
        ClickCounter { last: None, count: 0, interval_ms: 500, max_dist: 4.0 }
    }
}

impl ClickCounter {
    /// Register a press and return its click number (1, 2 or 3).
    pub fn press(&mut self, time_ms: u32, pos: Pt, button: MouseButton) -> u8 {
        let again = self.last.is_some_and(|(t, p, b)| {
            b == button && time_ms.wrapping_sub(t) <= self.interval_ms && (p.0 - pos.0).abs() <= self.max_dist && (p.1 - pos.1).abs() <= self.max_dist
        });
        self.count = if again && self.count < 3 { self.count + 1 } else { 1 };
        self.last = Some((time_ms, pos, button));
        self.count
    }
}

// ---------------------------------------------------------------- keyboard focus

/// A widget's identity for focus purposes (the caller numbers its controls).
pub type WidgetId = u32;

/// Keyboard focus order and the "focus-visible" rule: the ring shows after Tab / Shift+Tab / arrow navigation, not after a mouse click
/// (the same rule as `:focus-visible` in `theme.css`).
#[derive(Clone, Debug, Default)]
pub struct FocusChain {
    order: Vec<WidgetId>,
    cur: Option<WidgetId>,
    visible: bool,
}

impl FocusChain {
    pub fn new(order: &[WidgetId]) -> FocusChain {
        FocusChain { order: order.to_vec(), cur: None, visible: false }
    }

    /// Replace the order (a page changed). The focus stays when the widget is still there, else it is dropped.
    pub fn set_order(&mut self, order: &[WidgetId]) {
        self.order = order.to_vec();
        if self.cur.is_some_and(|c| !self.order.contains(&c)) {
            self.cur = None;
            self.visible = false;
        }
    }

    pub fn focused(&self) -> Option<WidgetId> {
        self.cur
    }

    pub fn is_focused(&self, id: WidgetId) -> bool {
        self.cur == Some(id)
    }

    /// Should `id` draw its focus ring? Focused AND focus came from the keyboard.
    pub fn ring_visible(&self, id: WidgetId) -> bool {
        self.visible && self.cur == Some(id)
    }

    /// Focus `id`. `from_keyboard` false = a mouse click (no ring). Unknown ids are ignored.
    pub fn focus(&mut self, id: WidgetId, from_keyboard: bool) -> bool {
        if !self.order.contains(&id) {
            return false;
        }
        let changed = self.cur != Some(id) || self.visible != from_keyboard;
        self.cur = Some(id);
        self.visible = from_keyboard;
        changed
    }

    pub fn blur(&mut self) {
        self.cur = None;
        self.visible = false;
    }

    /// Next widget in the order (wraps). Shows the ring.
    pub fn next(&mut self) -> Option<WidgetId> {
        self.step(1)
    }

    /// Previous widget (wraps). Shows the ring.
    pub fn prev(&mut self) -> Option<WidgetId> {
        self.step(-1)
    }

    fn step(&mut self, d: i32) -> Option<WidgetId> {
        let n = self.order.len() as i32;
        if n == 0 {
            return None;
        }
        let i = match self.cur.and_then(|c| self.order.iter().position(|&x| x == c)) {
            Some(i) => (i as i32 + d).rem_euclid(n),
            None if d > 0 => 0,
            None => n - 1,
        };
        self.cur = Some(self.order[i as usize]);
        self.visible = true;
        self.cur
    }

    /// Handle Tab / Shift+Tab from an event. True when the focus moved (the event is used up).
    pub fn handle(&mut self, ev: &InputEvent) -> bool {
        match ev.key_down() {
            Some((KeyCode::Tab, m, _)) if !m.ctrl && !m.alt && !self.order.is_empty() => {
                if m.shift {
                    self.prev();
                } else {
                    self.next();
                }
                true
            }
            _ => false,
        }
    }
}

/// Geometry of the focus ring: `theme.css` draws `outline: 2px solid var(--accent); outline-offset: 1px`. Returns the rectangle and the corner
/// radius to STROKE (with width [`FOCUS_RING_WIDTH`]) so the ring sits outside `r` with a 1 px gap.
pub fn focus_ring_geometry(r: Rect, radius: f32) -> (Rect, f32) {
    let out = FOCUS_RING_OFFSET + FOCUS_RING_WIDTH / 2.0;
    (Rect::new(r.x - out, r.y - out, r.w + 2.0 * out, r.h + 2.0 * out), radius + out)
}
pub const FOCUS_RING_WIDTH: f32 = 2.0;
pub const FOCUS_RING_OFFSET: f32 = 1.0;

// ---------------------------------------------------------------- Win32

/// Translation of `WM_*` messages into [`InputEvent`]s (see the module table). Pure except for `GetKeyState`.
pub mod win32 {
    use super::*;
    use windows::Win32::UI::Input::KeyboardAndMouse::*;
    use windows::Win32::UI::WindowsAndMessaging::*;

    /// `WM_MOUSELEAVE` lives in the Controls feature set; the value is part of the stable ABI.
    const WM_MOUSELEAVE: u32 = 0x02A3;
    // `MK_*` wparam flags of the mouse messages (Win32_System_SystemServices in the windows crate, not enabled here)
    const MK_LBUTTON: u32 = 0x0001;
    const MK_RBUTTON: u32 = 0x0002;
    const MK_SHIFT: u32 = 0x0004;
    const MK_CONTROL: u32 = 0x0008;
    const MK_MBUTTON: u32 = 0x0010;
    const MK_XBUTTON1: u32 = 0x0020;
    const MK_XBUTTON2: u32 = 0x0040;

    /// Virtual-key code to [`KeyCode`].
    pub fn key_code(vk: u32) -> KeyCode {
        match vk {
            v if v == VK_RETURN.0 as u32 => KeyCode::Enter,
            v if v == VK_ESCAPE.0 as u32 => KeyCode::Escape,
            v if v == VK_TAB.0 as u32 => KeyCode::Tab,
            v if v == VK_BACK.0 as u32 => KeyCode::Backspace,
            v if v == VK_DELETE.0 as u32 => KeyCode::Delete,
            v if v == VK_INSERT.0 as u32 => KeyCode::Insert,
            v if v == VK_SPACE.0 as u32 => KeyCode::Space,
            v if v == VK_LEFT.0 as u32 => KeyCode::Left,
            v if v == VK_RIGHT.0 as u32 => KeyCode::Right,
            v if v == VK_UP.0 as u32 => KeyCode::Up,
            v if v == VK_DOWN.0 as u32 => KeyCode::Down,
            v if v == VK_HOME.0 as u32 => KeyCode::Home,
            v if v == VK_END.0 as u32 => KeyCode::End,
            v if v == VK_PRIOR.0 as u32 => KeyCode::PageUp,
            v if v == VK_NEXT.0 as u32 => KeyCode::PageDown,
            0x30..=0x39 | 0x41..=0x5A => KeyCode::Char(char::from_u32(v_to_char(vk)).unwrap_or('?')),
            v if (VK_F1.0 as u32..=VK_F24.0 as u32).contains(&v) => KeyCode::F((v - VK_F1.0 as u32 + 1) as u8),
            v => KeyCode::Other(v),
        }
    }

    fn v_to_char(vk: u32) -> u32 {
        vk // VK_0..VK_9 and VK_A..VK_Z equal their ASCII upper-case codes
    }

    fn down(vk: VIRTUAL_KEY) -> bool {
        unsafe { GetKeyState(vk.0 as i32) < 0 }
    }

    /// The modifier keys held right now.
    pub fn modifiers_now() -> Modifiers {
        Modifiers { shift: down(VK_SHIFT), ctrl: down(VK_CONTROL), alt: down(VK_MENU), meta: down(VK_LWIN) || down(VK_RWIN) }
    }

    fn mods_from_mk(wparam: usize) -> Modifiers {
        Modifiers { shift: wparam & MK_SHIFT as usize != 0, ctrl: wparam & MK_CONTROL as usize != 0, alt: down(VK_MENU), meta: false }
    }

    fn buttons_from_mk(wparam: usize) -> Buttons {
        let mut b = Buttons::NONE;
        for (flag, btn) in [(MK_LBUTTON, MouseButton::Left), (MK_RBUTTON, MouseButton::Right), (MK_MBUTTON, MouseButton::Middle), (MK_XBUTTON1, MouseButton::X1), (MK_XBUTTON2, MouseButton::X2)] {
            if wparam & flag as usize != 0 {
                b = b.with(btn);
            }
        }
        b
    }

    /// Signed client x / y of a mouse-message `lparam`.
    pub fn lparam_point(lparam: isize) -> (i32, i32) {
        ((lparam & 0xFFFF) as u16 as i16 as i32, ((lparam >> 16) & 0xFFFF) as u16 as i16 as i32)
    }

    /// Per-window translator state: the scale, click counting, a pending high surrogate, whether `TrackMouseEvent` was armed.
    #[derive(Clone, Debug)]
    pub struct Translator {
        scale: f32,
        origin: (i32, i32),
        clicks: ClickCounter,
        high: Option<u16>,
        /// the pointer is inside the window: arm `TrackMouseEvent(TME_LEAVE)` when this flips to true (the host reads `take_track_request`)
        inside: bool,
        track: bool,
    }

    impl Translator {
        /// `scale` = device px per logical px (dpi / 96).
        pub fn new(scale: f32) -> Translator {
            Translator { scale: scale.max(0.1), origin: (0, 0), clicks: ClickCounter::default(), high: None, inside: false, track: false }
        }

        pub fn set_scale(&mut self, scale: f32) {
            self.scale = scale.max(0.1);
        }

        /// Screen position of the client area's top-left (`ClientToScreen`): `WM_MOUSEWHEEL` carries screen coordinates.
        pub fn set_client_origin(&mut self, x: i32, y: i32) {
            self.origin = (x, y);
        }

        /// True once after the pointer entered: call `TrackMouseEvent` with `TME_LEAVE` so `WM_MOUSELEAVE` comes.
        pub fn take_track_request(&mut self) -> bool {
            std::mem::take(&mut self.track)
        }

        fn logical(&self, p: (i32, i32)) -> Pt {
            (p.0 as f32 / self.scale, p.1 as f32 / self.scale)
        }

        /// Translate one message; `time_ms` is `GetMessageTime()`. `None` = not an input message (pass it on).
        pub fn translate(&mut self, msg: u32, wparam: usize, lparam: isize, time_ms: u32) -> Option<InputEvent> {
            match msg {
                WM_MOUSEMOVE => {
                    if !self.inside {
                        self.inside = true;
                        self.track = true;
                    }
                    Some(InputEvent::MouseMove { pos: self.logical(lparam_point(lparam)), mods: mods_from_mk(wparam), buttons: buttons_from_mk(wparam) })
                }
                WM_MOUSELEAVE => {
                    self.inside = false;
                    Some(InputEvent::MouseLeave)
                }
                WM_LBUTTONDOWN | WM_LBUTTONDBLCLK | WM_RBUTTONDOWN | WM_RBUTTONDBLCLK | WM_MBUTTONDOWN | WM_MBUTTONDBLCLK | WM_XBUTTONDOWN | WM_XBUTTONDBLCLK => {
                    let button = match msg {
                        WM_LBUTTONDOWN | WM_LBUTTONDBLCLK => MouseButton::Left,
                        WM_RBUTTONDOWN | WM_RBUTTONDBLCLK => MouseButton::Right,
                        WM_MBUTTONDOWN | WM_MBUTTONDBLCLK => MouseButton::Middle,
                        _ => {
                            if (wparam >> 16) & 0xFFFF == 1 {
                                MouseButton::X1
                            } else {
                                MouseButton::X2
                            }
                        }
                    };
                    let pos = self.logical(lparam_point(lparam));
                    Some(InputEvent::MouseDown { pos, button, mods: mods_from_mk(wparam), clicks: self.clicks.press(time_ms, pos, button) })
                }
                WM_LBUTTONUP | WM_RBUTTONUP | WM_MBUTTONUP | WM_XBUTTONUP => {
                    let button = match msg {
                        WM_LBUTTONUP => MouseButton::Left,
                        WM_RBUTTONUP => MouseButton::Right,
                        WM_MBUTTONUP => MouseButton::Middle,
                        _ => {
                            if (wparam >> 16) & 0xFFFF == 1 {
                                MouseButton::X1
                            } else {
                                MouseButton::X2
                            }
                        }
                    };
                    Some(InputEvent::MouseUp { pos: self.logical(lparam_point(lparam)), button, mods: mods_from_mk(wparam) })
                }
                WM_MOUSEWHEEL | WM_MOUSEHWHEEL => {
                    let notches = ((wparam >> 16) & 0xFFFF) as u16 as i16 as f32 / 120.0;
                    let (sx, sy) = lparam_point(lparam);
                    let pos = self.logical((sx - self.origin.0, sy - self.origin.1));
                    let px = notches * WHEEL_NOTCH_PX;
                    // wheel forward (away from the user) = positive delta = scroll UP = content moves down: our y is the opposite
                    let delta = if msg == WM_MOUSEWHEEL { (0.0, -px) } else { (px, 0.0) };
                    // a high-resolution wheel / touchpad reports fractions of a notch
                    let precise = (((wparam >> 16) & 0xFFFF) as u16 as i16).rem_euclid(120) != 0;
                    Some(InputEvent::Wheel { pos, delta, mods: mods_from_mk(wparam), precise })
                }
                WM_KEYDOWN | WM_SYSKEYDOWN | WM_KEYUP | WM_SYSKEYUP => Some(InputEvent::Key {
                    code: key_code(wparam as u32),
                    mods: modifiers_now(),
                    down: msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN,
                    repeat: msg == WM_KEYDOWN && lparam & (1 << 30) != 0,
                }),
                WM_CHAR => self.char_unit(wparam as u16),
                WM_SETFOCUS => Some(InputEvent::Focus(true)),
                WM_KILLFOCUS => Some(InputEvent::Focus(false)),
                WM_DPICHANGED => {
                    let dpi = (wparam & 0xFFFF) as f32;
                    self.scale = (dpi / 96.0).max(0.1);
                    Some(InputEvent::DpiChanged(self.scale))
                }
                _ => None,
            }
        }

        fn char_unit(&mut self, u: u16) -> Option<InputEvent> {
            let c = match u {
                0xD800..=0xDBFF => {
                    self.high = Some(u);
                    return None;
                }
                0xDC00..=0xDFFF => {
                    let hi = self.high.take()?;
                    char::from_u32(0x10000 + (((hi as u32) & 0x3FF) << 10) + ((u as u32) & 0x3FF))?
                }
                _ => {
                    self.high = None;
                    char::from_u32(u as u32)?
                }
            };
            // Enter, Tab, Esc, Backspace and Ctrl+letter come as control characters: they reached the controls as `Key` already
            (!c.is_control()).then_some(InputEvent::Char(c))
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn lp(x: i32, y: i32) -> isize {
            ((y as u16 as isize) << 16) | (x as u16 as isize)
        }

        #[test]
        fn virtual_keys_map_to_codes() {
            assert_eq!(key_code(0x0D), KeyCode::Enter);
            assert_eq!(key_code(0x1B), KeyCode::Escape);
            assert_eq!(key_code(0x41), KeyCode::Char('A'));
            assert_eq!(key_code(0x39), KeyCode::Char('9'));
            assert_eq!(key_code(0x70), KeyCode::F(1));
            assert_eq!(key_code(0x7B), KeyCode::F(12));
            assert_eq!(key_code(0x25), KeyCode::Left);
            assert_eq!(key_code(0x21), KeyCode::PageUp);
            assert_eq!(key_code(0xBA), KeyCode::Other(0xBA));
        }

        #[test]
        fn mouse_messages_scale_to_logical_px_and_count_clicks() {
            let mut t = Translator::new(2.0);
            let ev = t.translate(WM_MOUSEMOVE, 0, lp(100, 50), 0).unwrap();
            assert_eq!(ev.pos(), Some((50.0, 25.0)));
            assert!(t.take_track_request(), "the first move arms TrackMouseEvent");
            assert!(!t.take_track_request());
            let d1 = t.translate(WM_LBUTTONDOWN, MK_LBUTTON as usize, lp(100, 50), 1000).unwrap();
            let d2 = t.translate(WM_LBUTTONDBLCLK, MK_LBUTTON as usize, lp(101, 50), 1200).unwrap();
            let d3 = t.translate(WM_LBUTTONDOWN, MK_LBUTTON as usize, lp(100, 51), 1400).unwrap();
            let d4 = t.translate(WM_LBUTTONDOWN, MK_LBUTTON as usize, lp(100, 51), 1500).unwrap();
            let n = |e: &InputEvent| match e {
                InputEvent::MouseDown { clicks, .. } => *clicks,
                _ => 0,
            };
            assert_eq!((n(&d1), n(&d2), n(&d3), n(&d4)), (1, 2, 3, 1));
            // negative coordinates (a captured drag left of the window)
            let ev = t.translate(WM_MOUSEMOVE, MK_LBUTTON as usize, lp(-10, -4), 0).unwrap();
            assert_eq!(ev.pos(), Some((-5.0, -2.0)));
            match ev {
                InputEvent::MouseMove { buttons, .. } => assert!(buttons.left()),
                _ => panic!(),
            }
            assert_eq!(t.translate(WM_MOUSELEAVE, 0, 0, 0), Some(InputEvent::MouseLeave));
            assert!(t.translate(WM_MOUSEMOVE, 0, lp(1, 1), 0).is_some() && t.take_track_request(), "re-armed after a leave");
        }

        #[test]
        fn wheel_notches_become_px_with_scroll_down_positive() {
            let mut t = Translator::new(1.0);
            t.set_client_origin(100, 100);
            let w = (((-120i16) as u16 as usize) << 16) | 0;
            let ev = t.translate(WM_MOUSEWHEEL, w, lp(150, 160), 0).unwrap();
            assert_eq!(ev, InputEvent::Wheel { pos: (50.0, 60.0), delta: (0.0, WHEEL_NOTCH_PX), mods: Modifiers::NONE, precise: false });
            let half = (((30i16) as u16 as usize) << 16) | 0;
            match t.translate(WM_MOUSEWHEEL, half, lp(150, 160), 0).unwrap() {
                InputEvent::Wheel { delta, precise, .. } => assert!(delta.1 < 0.0 && precise),
                _ => panic!(),
            }
        }

        #[test]
        fn chars_join_surrogates_and_drop_controls() {
            let mut t = Translator::new(1.0);
            assert_eq!(t.translate(WM_CHAR, 'ж' as usize, 0, 0), Some(InputEvent::Char('ж')));
            assert_eq!(t.translate(WM_CHAR, 0x0D, 0, 0), None);
            assert_eq!(t.translate(WM_CHAR, 0x08, 0, 0), None);
            assert_eq!(t.translate(WM_CHAR, 0x01, 0, 0), None);
            // U+1F600 = D83D DE00
            assert_eq!(t.translate(WM_CHAR, 0xD83D, 0, 0), None);
            assert_eq!(t.translate(WM_CHAR, 0xDE00, 0, 0), Some(InputEvent::Char('😀')));
            // a lone low surrogate is dropped
            assert_eq!(t.translate(WM_CHAR, 0xDE00, 0, 0), None);
        }

        #[test]
        fn dpi_change_updates_the_scale() {
            let mut t = Translator::new(1.0);
            assert_eq!(t.translate(WM_DPICHANGED, 192, 0, 0), Some(InputEvent::DpiChanged(2.0)));
            assert_eq!(t.translate(WM_MOUSEMOVE, 0, lp(20, 20), 0).unwrap().pos(), Some((10.0, 10.0)));
            assert_eq!(t.translate(WM_PAINT, 0, 0, 0), None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hit_test_prefers_the_topmost() {
        let rs = [Rect::new(0.0, 0.0, 100.0, 100.0), Rect::new(10.0, 10.0, 20.0, 20.0)];
        assert_eq!(hit_test(&rs, (15.0, 15.0)), Some(1));
        assert_eq!(hit_test(&rs, (50.0, 50.0)), Some(0));
        assert_eq!(hit_test(&rs, (150.0, 50.0)), None);
        assert_eq!(hit_test(&[], (0.0, 0.0)), None);
    }

    #[test]
    fn rounded_hit_ignores_the_corners() {
        let r = Rect::new(0.0, 0.0, 40.0, 40.0);
        assert!(hit_rounded(r, 20.0, (20.0, 20.0)));
        assert!(!hit_rounded(r, 20.0, (1.0, 1.0)), "corner of a circle");
        assert!(hit_rounded(r, 20.0, (20.0, 1.0)));
        assert!(hit_rounded(r, 0.0, (1.0, 1.0)));
        assert!(!hit_rounded(r, 20.0, (41.0, 20.0)));
        assert!(hit_rounded(Rect::new(0.0, 0.0, 60.0, 24.0), 12.0, (30.0, 12.0)), "a pill's straight middle");
        assert_eq!(expand(r, 4.0), Rect::new(-4.0, -4.0, 48.0, 48.0));
    }

    #[test]
    fn click_counter_needs_time_distance_and_the_same_button() {
        let mut c = ClickCounter::default();
        assert_eq!(c.press(0, (10.0, 10.0), MouseButton::Left), 1);
        assert_eq!(c.press(300, (12.0, 10.0), MouseButton::Left), 2);
        assert_eq!(c.press(600, (12.0, 10.0), MouseButton::Left), 3);
        assert_eq!(c.press(900, (12.0, 10.0), MouseButton::Left), 1, "a fourth click starts over");
        assert_eq!(c.press(1000, (12.0, 10.0), MouseButton::Right), 1, "another button");
        assert_eq!(c.press(1100, (12.0, 10.0), MouseButton::Right), 2);
        assert_eq!(c.press(1900, (12.0, 10.0), MouseButton::Right), 1, "too slow");
        assert_eq!(c.press(1950, (40.0, 10.0), MouseButton::Right), 1, "too far");
        // GetMessageTime wraps after 49 days
        assert_eq!(c.press(u32::MAX - 5, (0.0, 0.0), MouseButton::Left), 1);
        assert_eq!(c.press(100, (0.0, 0.0), MouseButton::Left), 2);
    }

    #[test]
    fn focus_chain_cycles_and_hides_the_ring_after_a_click() {
        let mut f = FocusChain::new(&[10, 20, 30]);
        assert_eq!(f.focused(), None);
        assert_eq!(f.next(), Some(10));
        assert!(f.ring_visible(10));
        assert_eq!(f.next(), Some(20));
        assert_eq!(f.prev(), Some(10));
        assert_eq!(f.prev(), Some(30), "wraps");
        assert!(f.focus(20, false), "a click focuses without a ring");
        assert!(f.is_focused(20) && !f.ring_visible(20));
        assert!(!f.focus(99, true), "unknown id");
        f.set_order(&[20, 40]);
        assert_eq!(f.focused(), Some(20), "still present");
        f.set_order(&[40]);
        assert_eq!(f.focused(), None);
        let mut g = FocusChain::new(&[1, 2]);
        assert_eq!(g.prev(), Some(2), "Shift+Tab with no focus starts at the end");
        let tab = InputEvent::Key { code: KeyCode::Tab, mods: Modifiers::NONE, down: true, repeat: false };
        let stab = InputEvent::Key { code: KeyCode::Tab, mods: Modifiers::SHIFT, down: true, repeat: false };
        assert!(g.handle(&tab) && g.focused() == Some(1));
        assert!(g.handle(&stab) && g.focused() == Some(2));
        assert!(!g.handle(&InputEvent::Char('a')));
        assert!(!FocusChain::default().handle(&tab), "an empty chain leaves Tab to the host");
    }

    #[test]
    fn events_translate_and_responses_merge() {
        let e = InputEvent::MouseDown { pos: (10.0, 10.0), button: MouseButton::Left, mods: Modifiers::NONE, clicks: 1 };
        assert_eq!(e.translated(-4.0, 6.0).pos(), Some((6.0, 16.0)));
        assert_eq!(InputEvent::Char('x').translated(5.0, 5.0), InputEvent::Char('x'));
        let a = Response { redraw: true, cursor: Some(Cursor::Hand), ..Response::NONE };
        let b = Response { changed: true, cursor: Some(Cursor::Text), ..Response::consumed() };
        let m = a.merge(b);
        assert!(m.redraw && m.changed && m.consumed);
        assert_eq!(m.cursor, Some(Cursor::Hand));
        let b = Buttons::NONE.with(MouseButton::Left).with(MouseButton::Right);
        assert!(b.left() && b.has(MouseButton::Right) && !b.has(MouseButton::Middle));
        assert!(!b.without(MouseButton::Left).left() && b.any());
    }

    #[test]
    fn focus_ring_sits_outside_with_a_gap() {
        let (r, rad) = focus_ring_geometry(Rect::new(10.0, 10.0, 40.0, 30.0), 8.0);
        assert_eq!(r, Rect::new(8.0, 8.0, 44.0, 34.0));
        assert_eq!(rad, 10.0);
    }
}
