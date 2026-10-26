//! Nobody can see the widgets: the capture and every backdrop / tick driven redraw sleep. The reasons are a bitmask (asleep while ANY bit is
//! set), the window messages that raise some of them are decoded here, and "every card is hidden" is a pure geometry test. The Win32 side
//! (collecting the covering windows, registering for the notifications) lives in host.rs.
//!
//! `MSWIDGETS_NO_AUTOPAUSE=1` turns the new reasons off (the fullscreen / hidden pause stays).
use std::sync::atomic::{AtomicU8, Ordering::Relaxed};
use windows::core::GUID;
use windows::Win32::Foundation::RECT;

/// WM_POWERBROADCAST `wparam` of a power setting change; its payload starts with the setting's GUID, then a u32 length, then the data
pub const PBT_POWERSETTINGCHANGE: usize = 0x8013;
/// WM_WTSSESSION_CHANGE
pub const WM_WTSSESSION_CHANGE: u32 = 0x02B1;
/// GUID_CONSOLE_DISPLAY_STATE (the windows crate keeps it behind the whole SystemServices feature)
pub const GUID_CONSOLE_DISPLAY_STATE: GUID = GUID::from_u128(0x6fe69556_704a_47a0_8f24_c28d936fda47);

/// a fullscreen app is in front, or the widgets are hidden from the tray (the layer is hidden)
pub const HIDDEN: u8 = 1;
/// the session is locked / disconnected / switched away
pub const LOCKED: u8 = 2;
/// the input desktop is a secure one (lock screen, UAC prompt)
pub const SECURE: u8 = 4;
/// the monitors are off (console display state)
pub const DISPLAY_OFF: u8 = 8;
/// the system is suspending
pub const SUSPEND: u8 = 16;
/// every card is entirely hidden behind other windows
pub const COVERED: u8 = 32;
/// the reasons that window messages raise (see `raise`)
pub const MESSAGES: u8 = LOCKED | DISPLAY_OFF | SUSPEND;

#[derive(Clone, Copy, Default, PartialEq, Debug)]
pub struct Pause(u8);

impl Pause {
    /// Raise / clear one reason; true when the union went from awake to asleep or back.
    pub fn set(&mut self, bit: u8, on: bool) -> bool {
        let was = self.any();
        self.0 = if on { self.0 | bit } else { self.0 & !bit };
        was != self.any()
    }
    pub fn any(self) -> bool {
        self.0 != 0
    }
    pub fn has(self, bit: u8) -> bool {
        self.0 & bit != 0
    }
    /// Asleep for a reason other than the occlusion test (which is then not worth asking).
    pub fn hard(self) -> bool {
        self.0 & !COVERED != 0
    }
}

/// The message-driven reasons, written straight from the window procedure: a message that finds the App borrowed is not lost, the guard
/// timer reads this again (a lost "unlock" would otherwise strand the pause).
static RAISED: AtomicU8 = AtomicU8::new(0);

pub fn raise(bit: u8, on: bool) {
    if on {
        RAISED.fetch_or(bit, Relaxed);
    } else {
        RAISED.fetch_and(!bit, Relaxed);
    }
}

pub fn raised() -> u8 {
    RAISED.load(Relaxed)
}

/// WM_WTSSESSION_CHANGE `wparam` -> Some(true) = the session is away (lock, disconnect), Some(false) = back, None = irrelevant.
pub fn session_event(code: usize) -> Option<bool> {
    match code {
        2 | 4 | 7 => Some(true), // WTS_CONSOLE_DISCONNECT, WTS_REMOTE_DISCONNECT, WTS_SESSION_LOCK
        1 | 3 | 8 => Some(false), // WTS_CONSOLE_CONNECT, WTS_REMOTE_CONNECT, WTS_SESSION_UNLOCK
        _ => None,
    }
}

/// WM_POWERBROADCAST `wparam` (PBT_APM*) -> Some(true) = suspending, Some(false) = resumed, None = irrelevant.
pub fn suspend_event(code: usize) -> Option<bool> {
    match code {
        4 => Some(true),          // PBT_APMSUSPEND
        7 | 18 => Some(false),    // PBT_APMRESUMESUSPEND, PBT_APMRESUMEAUTOMATIC
        _ => None,
    }
}

/// GUID_CONSOLE_DISPLAY_STATE payload: 0 = off, 1 = on, 2 = dimmed (still lit).
pub fn display_off(state: u32) -> bool {
    state == 0
}

/// The part of `r` that `w` leaves free, as up to four rectangles.
fn cut(r: RECT, w: RECT, out: &mut Vec<RECT>) {
    if w.right <= r.left || w.left >= r.right || w.bottom <= r.top || w.top >= r.bottom {
        out.push(r);
        return;
    }
    if w.top > r.top {
        out.push(RECT { bottom: w.top, ..r });
    }
    if w.bottom < r.bottom {
        out.push(RECT { top: w.bottom, ..r });
    }
    let (top, bottom) = (r.top.max(w.top), r.bottom.min(w.bottom));
    if w.left > r.left {
        out.push(RECT { right: w.left, top, bottom, ..r });
    }
    if w.right < r.right {
        out.push(RECT { left: w.right, top, bottom, ..r });
    }
}

/// `r` is entirely behind the union of `wins` (an empty `r` is nothing we know: not hidden).
pub fn hidden(r: RECT, wins: &[RECT]) -> bool {
    if r.right <= r.left || r.bottom <= r.top {
        return false;
    }
    let mut free = vec![r];
    for w in wins {
        let mut next = Vec::with_capacity(free.len() + 3);
        for p in free {
            cut(p, *w, &mut next);
        }
        free = next;
        if free.is_empty() {
            return true;
        }
    }
    free.is_empty()
}

/// Every card is hidden (no card = nothing is known: awake). `wins` = the opaque windows above the desk, any order.
pub fn all_hidden(cards: &[RECT], wins: &[RECT]) -> bool {
    !cards.is_empty() && cards.iter().all(|c| hidden(*c, wins))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(l: i32, t: i32, rt: i32, b: i32) -> RECT {
        RECT { left: l, top: t, right: rt, bottom: b }
    }

    #[test]
    fn reasons_union() {
        let mut p = Pause::default();
        assert!(!p.any());
        assert!(p.set(HIDDEN, true), "awake -> asleep");
        assert!(!p.set(LOCKED, true), "a second reason changes nothing");
        assert!(!p.set(HIDDEN, false) && p.any(), "still asleep for the lock");
        assert!(p.has(LOCKED) && !p.has(HIDDEN));
        assert!(p.set(LOCKED, false) && !p.any(), "the last reason cleared: awake");
        assert!(!p.set(COVERED, false), "clearing an unset bit is no change");
        p.set(COVERED, true);
        assert!(!p.hard(), "covered alone is not a hard reason");
        p.set(DISPLAY_OFF, true);
        assert!(p.hard());
    }

    #[test]
    fn message_reasons_survive_a_lost_message() {
        raise(LOCKED, true);
        raise(DISPLAY_OFF, true);
        assert_eq!(raised() & MESSAGES, LOCKED | DISPLAY_OFF);
        raise(LOCKED, false);
        assert_eq!(raised() & MESSAGES, DISPLAY_OFF);
        raise(DISPLAY_OFF, false);
        assert_eq!(raised() & MESSAGES, 0);
    }

    #[test]
    fn decodes_session_and_power_events() {
        assert_eq!(session_event(7), Some(true)); // lock
        assert_eq!(session_event(8), Some(false)); // unlock
        assert_eq!(session_event(2), Some(true)); // user switched away
        assert_eq!(session_event(1), Some(false));
        assert_eq!(session_event(5), None); // logon
        assert_eq!(suspend_event(4), Some(true));
        assert_eq!(suspend_event(18), Some(false));
        assert_eq!(suspend_event(0x8013), None);
        assert!(display_off(0) && !display_off(1) && !display_off(2), "dimmed still shows the desk");
    }

    #[test]
    fn one_window_covering_a_card() {
        let c = r(100, 100, 300, 200);
        assert!(hidden(c, &[r(0, 0, 1920, 1080)]));
        assert!(hidden(c, &[r(100, 100, 300, 200)]), "exact fit");
        assert!(!hidden(c, &[r(101, 100, 1920, 1080)]), "one pixel column left over");
        assert!(!hidden(c, &[r(0, 0, 1920, 199)]), "one pixel row left over");
        assert!(!hidden(c, &[]), "no window above");
        assert!(!hidden(c, &[r(400, 0, 500, 500)]), "a window elsewhere");
    }

    #[test]
    fn tiled_windows_cover_together() {
        let c = r(100, 100, 300, 200);
        let (a, b) = (r(0, 0, 200, 1080), r(200, 0, 1920, 1080));
        assert!(hidden(c, &[a, b]));
        assert!(hidden(c, &[b, a]), "order does not matter");
        assert!(!hidden(c, &[a, r(201, 0, 1920, 1080)]), "a one pixel gap between the tiles");
        // four windows, each over one quadrant of the card
        let q = [r(0, 0, 200, 150), r(200, 0, 400, 150), r(0, 150, 200, 400), r(200, 150, 400, 400)];
        assert!(hidden(c, &q));
        assert!(!hidden(c, &q[..3]));
    }

    #[test]
    fn a_hole_in_the_middle_keeps_the_card_visible() {
        let c = r(0, 0, 100, 100);
        let ring = [r(0, 0, 100, 40), r(0, 60, 100, 100), r(0, 40, 40, 60), r(60, 40, 100, 60)];
        assert!(!hidden(c, &ring), "the 20x20 hole is still glass");
        let mut full = ring.to_vec();
        full.push(r(40, 40, 60, 60));
        assert!(hidden(c, &full));
    }

    #[test]
    fn every_card_must_be_hidden() {
        let cards = [r(100, 100, 300, 200), r(1500, 100, 1700, 200)];
        let left = r(0, 0, 800, 1080);
        assert!(!all_hidden(&cards, &[left]), "one card still shows");
        assert!(all_hidden(&cards, &[left, r(800, 0, 1920, 1080)]));
        assert!(all_hidden(&cards, &[r(0, 0, 1920, 1080)]));
        assert!(!all_hidden(&[], &[r(0, 0, 1920, 1080)]), "no cards: nothing known, stay awake");
        assert!(!all_hidden(&[r(5, 5, 5, 50)], &[r(0, 0, 1920, 1080)]), "a degenerate card never pauses");
    }
}
