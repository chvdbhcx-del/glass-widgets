//! The text prompt and the context menu on the canvas, plus the foreground hand-off they share (split out of host.rs).
use super::*;

// ---------------------------------------------------------------- the text prompt on the canvas

/// Put `s` on the clipboard as CF_UNICODETEXT (CR LF line breaks).
fn set_clipboard_text(hwnd: HWND, s: &str) {
    use windows::Win32::System::DataExchange::{EmptyClipboard, SetClipboardData};
    use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
    let t = s.replace("\r\n", "\n").replace('\n', "\r\n");
    let w: Vec<u16> = t.encode_utf16().chain(Some(0)).collect();
    unsafe {
        if OpenClipboard(hwnd).is_err() {
            return;
        }
        let _ = EmptyClipboard();
        if let Ok(h) = GlobalAlloc(GMEM_MOVEABLE, w.len() * 2) {
            let p = GlobalLock(h) as *mut u16;
            if !p.is_null() {
                std::ptr::copy_nonoverlapping(w.as_ptr(), p, w.len());
                let _ = GlobalUnlock(h);
                let _ = SetClipboardData(13, HANDLE(h.0)); // the clipboard owns the memory now
            }
        }
        let _ = CloseClipboard();
    }
}

impl App {
    /// A widget thread asked for a text prompt (`input::prompt_ex`): show it on the canvas, next to the cursor. Refused (the worker runs the
    /// Win32 popup) while the layer is hidden or a card is being dragged.
    pub(super) unsafe fn on_prompt_request(&mut self) {
        let Some(req) = input::take_request() else { return };
        self.abort_menu(); // a prompt and a menu are never up together
        if self.ui.prompt.is_some() {
            self.finish_prompt(); // the previous one is still fading out
        }
        if !self.visible || self.drag.is_some() {
            let _ = req.reply.send(input::Reply::Fallback);
            return;
        }
        let now = self.now();
        self.leave(&now);
        let s = self.s;
        let work = Rect::new((self.work.left - self.mon.left) as f32 / s, (self.work.top - self.mon.top) as f32 / s, (self.work.right - self.work.left) as f32 / s, (self.work.bottom - self.work.top) as f32 / s);
        // the cursor (screen px) on this canvas in logical px; from another monitor the panel opens in the upper middle of the work area
        let (ax, ay) = req.anchor;
        let on = ax >= self.mon.left && ax < self.mon.right && ay >= self.mon.top && ay < self.mon.bottom;
        let anchor = if on { ((ax - self.mon.left) as f32 / s, (ay - self.mon.top) as f32 / s) } else { (work.cx(), work.y + work.h * 0.35) };
        // the keyboard: the layer becomes activatable and takes the foreground (as in edit mode); remember who had it
        self.modal_begin();
        self.boost_capture();
        self.reduced = reduced_motion();
        self.ui.open_prompt(&self.scene, &self.gfx, &req.opts, anchor, work);
        self.prompt_reply = Some(req.reply);
        (self.prompt_was_fg, self.prompt_t0, self.prompt_last, self.prompt_scrim, self.prompt_hi, self.prompt_click) = (false, Instant::now(), Instant::now(), 0.0, None, None);
        let full = RECT { left: 0, top: 0, right: self.scene.w, bottom: self.scene.h };
        self.set_window(full); // live mode: the layer covers the whole monitor, like for the toast
        self.ui.sync_panels(&mut self.scene, &self.gfx);
        self.sync_regions();
        force_foreground(self.hwnd);
        let _ = SetFocus(self.hwnd);
        self.prompt_period = 16;
        SetTimer(self.hwnd, T_PROMPT, 16, None);
        self.prompt_tick();
    }

    /// Open / close animation (16 ms), then the caret blink (500 ms); the veil over the desktop fades in 4 steps (each one a full frame).
    pub(super) unsafe fn prompt_tick(&mut self) {
        let t = Instant::now();
        let dt = t.saturating_duration_since(self.prompt_last).as_secs_f32().clamp(0.0, 0.1);
        self.prompt_last = t;
        let reduced = self.reduced;
        let Some(p) = self.ui.prompt.as_mut() else {
            let _ = KillTimer(self.hwnd, T_PROMPT);
            self.prompt_period = 0;
            return;
        };
        let closing = p.closing;
        let busy = if reduced {
            p.t = if closing { 0.0 } else { 1.0 };
            false
        } else {
            p.step(dt)
        };
        let scrim = p.scrim();
        let mut d = self.ui.prompt_dirty(&self.scene);
        if (scrim - self.prompt_scrim).abs() > 1e-4 {
            self.prompt_scrim = scrim;
            d = Some(RECT { left: 0, top: 0, right: self.scene.w, bottom: self.scene.h });
        }
        if closing && !busy {
            return self.finish_prompt();
        }
        let period = if busy { 16 } else { 500 };
        if period != self.prompt_period {
            self.prompt_period = period;
            SetTimer(self.hwnd, T_PROMPT, period, None);
            self.ui.caret_on = true;
        } else if !busy {
            self.ui.caret_on = !self.ui.caret_on;
        }
        if let Some(d) = d {
            self.frame(d);
        }
    }

    /// The user answered (or cancelled): the worker gets the answer at once, the panel fades out.
    pub(super) unsafe fn end_prompt(&mut self, answer: Option<String>) {
        if let Some(tx) = self.prompt_reply.take() {
            let _ = tx.send(input::Reply::Answer(answer));
        }
        let Some(p) = self.ui.prompt.as_mut() else { return };
        if p.closing {
            return;
        }
        (p.closing, p.selecting, p.hover, p.press) = (true, false, None, None);
        let _ = ReleaseCapture();
        self.prompt_last = Instant::now();
        self.prompt_period = 16;
        SetTimer(self.hwnd, T_PROMPT, 16, None);
        self.prompt_tick();
    }

    /// The panel is gone: no veil, the layer goes back to click-through under the app windows and the foreground to whoever had it.
    unsafe fn finish_prompt(&mut self) {
        self.prompt_reply = None; // a prompt that was never answered counts as a cancel (the worker sees the dropped channel)
        let _ = KillTimer(self.hwnd, T_PROMPT);
        self.prompt_period = 0;
        if self.ui.prompt.take().is_none() {
            return;
        }
        self.prompt_scrim = 0.0;
        self.ui.sync_panels(&mut self.scene, &self.gfx);
        self.sync_regions();
        let full = RECT { left: 0, top: 0, right: self.scene.w, bottom: self.scene.h };
        self.frame(full);
        self.modal_end();
    }

    /// The keyboard hand-off of a modal (prompt, menu) on the live desk: remember who has the foreground, the layer becomes activatable,
    /// stops being click-through and is raised (as in edit mode, which already has all of that).
    unsafe fn modal_begin(&mut self) {
        let fg = GetForegroundWindow();
        if fg != self.hwnd {
            self.prompt_fg = fg;
        }
        if !self.ui.edit {
            let ex = GetWindowLongPtrW(self.hwnd, GWL_EXSTYLE);
            SetWindowLongPtrW(self.hwnd, GWL_EXSTYLE, ex & !(WS_EX_NOACTIVATE.0 as isize) & !(WS_EX_TRANSPARENT.0 as isize));
            self.interactive = true;
            let _ = SetWindowPos(self.hwnd, HWND_TOP, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
        }
    }

    /// The modal is gone: the layer goes back to click-through under the app windows, the foreground to whoever had it, the window to the cards.
    unsafe fn modal_end(&mut self) {
        if !self.ui.edit {
            let ex = GetWindowLongPtrW(self.hwnd, GWL_EXSTYLE);
            SetWindowLongPtrW(self.hwnd, GWL_EXSTYLE, ex | WS_EX_NOACTIVATE.0 as isize | WS_EX_TRANSPARENT.0 as isize);
            self.interactive = false;
            if GetForegroundWindow() == self.hwnd && !self.prompt_fg.0.is_null() && IsWindow(self.prompt_fg).as_bool() {
                let _ = SetForegroundWindow(self.prompt_fg);
            }
            place_above_desktop(self.hwnd);
            self.fit_window();
            let w = self.win;
            self.frame(w);
        }
    }

    /// Teardown (fullscreen guard, monitor change): answer "cancelled" and drop the panel at once, no animation.
    pub(super) unsafe fn abort_prompt(&mut self) {
        if let Some(tx) = self.prompt_reply.take() {
            let _ = tx.send(input::Reply::Answer(None));
        }
        if self.ui.prompt.is_some() {
            self.finish_prompt();
        }
    }

    unsafe fn redraw_prompt(&mut self) {
        self.ui.caret_on = true;
        if let Some(d) = self.ui.prompt_dirty(&self.scene) {
            self.frame(d);
        }
    }

    unsafe fn apply_act(&mut self, a: Act) {
        match a {
            Act::None => {}
            Act::Redraw => self.redraw_prompt(),
            Act::Clip(t) => {
                set_clipboard_text(self.hwnd, &t);
                self.redraw_prompt();
            }
            Act::Done(r) => self.end_prompt(r),
        }
    }

    pub(super) fn prompt_field_under_cursor(&self) -> bool {
        let Some(p) = self.ui.prompt.as_ref().filter(|p| !p.closing) else { return false };
        let mut c = POINT::default();
        unsafe {
            if GetCursorPos(&mut c).is_err() {
                return false;
            }
        }
        p.hit((c.x - self.mon.left) as f32 / self.s, (c.y - self.mon.top) as f32 / self.s) == crate::prompt::PHit::Field
    }

    /// Mouse while the prompt is up: nothing behind it is reachable; a click outside the panel cancels.
    pub(super) unsafe fn prompt_mouse(&mut self, msg: u32, wp: WPARAM, x: i32, y: i32) {
        if self.ui.prompt.as_ref().is_some_and(|p| p.closing) {
            return;
        }
        let (lx, ly) = (x as f32 / self.s, y as f32 / self.s);
        let m = Mods { ctrl: GetKeyState(VK_CONTROL.0 as i32) < 0, shift: GetKeyState(VK_SHIFT.0 as i32) < 0 };
        let act = match msg {
            WM_MOUSEMOVE => self.ui.prompt_mouse(&self.scene, &self.gfx, 0, lx, ly, m, false),
            WM_LBUTTONDOWN => {
                let t = Instant::now();
                let dbl = self.prompt_click.is_some_and(|(t0, x0, y0)| t.duration_since(t0).as_millis() as u32 <= GetDoubleClickTime() && (x - x0).abs() <= 4 && (y - y0).abs() <= 4);
                self.prompt_click = if dbl { None } else { Some((t, x, y)) };
                SetCapture(self.hwnd);
                self.ui.prompt_mouse(&self.scene, &self.gfx, 1, lx, ly, m, dbl)
            }
            WM_LBUTTONUP => {
                let _ = ReleaseCapture();
                self.ui.prompt_mouse(&self.scene, &self.gfx, 2, lx, ly, m, false)
            }
            WM_MOUSEWHEEL => {
                let delta = ((wp.0 >> 16) as u16 as i16) as f32 / 120.0;
                if self.ui.prompt_wheel(&self.scene, &self.gfx, delta) {
                    Act::Redraw
                } else {
                    Act::None
                }
            }
            _ => Act::None,
        };
        self.apply_act(act);
    }

    /// Keys while the prompt is up (the layer is the foreground window, so WM_KEYDOWN / WM_CHAR arrive here).
    pub(super) unsafe fn prompt_key(&mut self, msg: u32, wp: usize) {
        if self.ui.prompt.as_ref().is_some_and(|p| p.closing) {
            return;
        }
        let m = Mods { ctrl: GetKeyState(VK_CONTROL.0 as i32) < 0, shift: GetKeyState(VK_SHIFT.0 as i32) < 0 };
        let key = match msg {
            WM_SYSKEYDOWN => return, // Alt+Tab and friends belong to Windows
            WM_KEYDOWN => match VIRTUAL_KEY(wp as u16) {
                VK_ESCAPE => Key::Esc,
                VK_RETURN => Key::Enter,
                VK_TAB => Key::Tab,
                VK_BACK => Key::Back,
                VK_DELETE => Key::Delete,
                VK_LEFT => Key::Left,
                VK_RIGHT => Key::Right,
                VK_UP => Key::Up,
                VK_DOWN => Key::Down,
                VK_HOME => Key::Home,
                VK_END => Key::End,
                VK_A if m.ctrl => Key::SelectAll,
                VK_C if m.ctrl => Key::Copy,
                VK_X if m.ctrl => Key::Cut,
                VK_V if m.ctrl => match clipboard_text(self.hwnd) {
                    Some(t) => Key::Paste(t),
                    None => return,
                },
                VK_SPACE if self.ui.prompt.as_ref().is_some_and(|p| p.focus != crate::prompt::Focus::Field) => Key::Char(' '),
                _ => return,
            },
            WM_CHAR => {
                // UTF-16 units: a high surrogate waits for its low one (emoji)
                let u = wp as u32;
                let c = match u {
                    0xD800..=0xDBFF => {
                        self.prompt_hi = Some(u as u16);
                        return;
                    }
                    0xDC00..=0xDFFF => {
                        let Some(hi) = self.prompt_hi.take() else { return };
                        match char::decode_utf16([hi, u as u16]).next() {
                            Some(Ok(c)) => c,
                            _ => return,
                        }
                    }
                    _ => {
                        self.prompt_hi = None;
                        let Some(c) = char::from_u32(u) else { return };
                        c
                    }
                };
                if c.is_control() || (c == ' ' && self.ui.prompt.as_ref().is_some_and(|p| p.focus != crate::prompt::Focus::Field)) {
                    return; // control codes come as key presses; the space on a button was handled at WM_KEYDOWN
                }
                Key::Char(c)
            }
            _ => return,
        };
        let act = self.ui.prompt_key(&self.scene, &self.gfx, key, m);
        self.apply_act(act);
    }
}

// ---------------------------------------------------------------- the card context menu on the canvas

impl App {
    /// Show the menu of a card on the canvas at the cursor (glass panels, see menu.rs). False = it cannot be shown now and the caller runs
    /// the native popup instead: `MSWIDGETS_NATIVE_MENU=1`, the layer is hidden, a card is dragged, the prompt or the settings panel is up.
    pub(super) unsafe fn show_menu(&mut self, req: &MenuReq) -> bool {
        if std::env::var_os("MSWIDGETS_NATIVE_MENU").is_some() || !self.visible || self.drag.is_some() || self.ui.prompt.is_some() || self.ui.settings {
            return false;
        }
        self.abort_menu(); // the previous one, if any
        let now = self.now();
        self.leave(&now);
        let s = self.s;
        let work = Rect::new((self.work.left - self.mon.left) as f32 / s, (self.work.top - self.mon.top) as f32 / s, (self.work.right - self.work.left) as f32 / s, (self.work.bottom - self.work.top) as f32 / s);
        // the cursor (screen px) on this canvas in logical px
        let on = req.x >= self.mon.left && req.x < self.mon.right && req.y >= self.mon.top && req.y < self.mon.bottom;
        let anchor = if on { ((req.x - self.mon.left) as f32 / s, (req.y - self.mon.top) as f32 / s) } else { (work.cx(), work.cy()) };
        // the keyboard (Esc, arrows): the same hand-off as the prompt; a native menu needed the foreground too
        self.modal_begin();
        self.boost_capture();
        self.reduced = reduced_motion();
        self.ui.open_menu(&self.scene, &self.gfx, menu::card_menu_with(req.lang, req.editing, &req.sizes, req.cur, req.click.clone()), anchor, work);
        self.menu_uid = req.uid;
        (self.prompt_was_fg, self.prompt_t0, self.menu_last, self.menu_shown) = (false, Instant::now(), Instant::now(), None);
        let full = RECT { left: 0, top: 0, right: self.scene.w, bottom: self.scene.h };
        self.set_window(full); // live mode: the layer covers the whole monitor, so a click outside lands on it (the faint veil makes it opaque to hits)
        self.ui.sync_panels(&mut self.scene, &self.gfx);
        self.sync_regions();
        force_foreground(self.hwnd);
        let _ = SetFocus(self.hwnd);
        self.menu_shown = self.ui.menu_dirty(&self.scene);
        self.frame(full);
        self.menu_period = 16;
        SetTimer(self.hwnd, T_MENU, 16, None);
        true
    }

    /// Open / close animation and the hover delay of the submenu (16 ms while something moves, no timer otherwise).
    pub(super) unsafe fn menu_tick(&mut self) {
        let t = Instant::now();
        let dt = t.saturating_duration_since(self.menu_last).as_secs_f32().clamp(0.0, 0.1);
        self.menu_last = t;
        let reduced = self.reduced;
        let Some(m) = self.ui.menu.as_mut() else {
            let _ = KillTimer(self.hwnd, T_MENU);
            self.menu_period = 0;
            return;
        };
        let closing = m.closing;
        let (busy, layout) = m.step(dt, reduced);
        if closing && !busy {
            return self.finish_menu();
        }
        if layout {
            self.ui.sync_panels(&mut self.scene, &self.gfx);
            self.sync_regions();
        }
        if !busy {
            let _ = KillTimer(self.hwnd, T_MENU);
            self.menu_period = 0;
        }
        self.menu_frame();
    }

    /// Compose the menu's rectangle: where it is now and where it was drawn last (a closed submenu leaves its place).
    unsafe fn menu_frame(&mut self) {
        let now = self.ui.menu_dirty(&self.scene);
        let d = match (now, self.menu_shown) {
            (Some(a), Some(b)) => Some(union(a, b)),
            (a, b) => a.or(b),
        };
        self.menu_shown = now;
        if let Some(d) = d {
            self.frame(d);
        }
    }

    /// Start the timer when it is off (a hover that waits for its delay needs it).
    unsafe fn menu_kick(&mut self) {
        if self.menu_period == 0 {
            self.menu_last = Instant::now();
            self.menu_period = 16;
            SetTimer(self.hwnd, T_MENU, 16, None);
        }
    }

    unsafe fn menu_act(&mut self, act: MAct) {
        match act {
            MAct::None => {}
            MAct::Redraw | MAct::Layout => {
                if act == MAct::Layout {
                    self.ui.sync_panels(&mut self.scene, &self.gfx);
                    self.sync_regions();
                }
                self.menu_frame();
                self.menu_kick();
            }
            MAct::Close => self.close_menu(),
            MAct::Pick(id) => self.pick_menu(id),
        }
    }

    /// The choice: "Edit widgets…" switches the mode (the menu is dropped at once, edit mode takes the foreground itself), the others fade the
    /// menu out while they run.
    unsafe fn pick_menu(&mut self, id: usize) {
        let uid = self.menu_uid;
        if id == CMD_EDIT {
            self.abort_menu();
            if !self.ui.edit && !self.prompt_fg.0.is_null() && GetForegroundWindow() == self.hwnd {
                self.prev_fg = self.prompt_fg; // the foreground has not moved back yet: edit mode must return it to the same window
            }
            if self.editing {
                self.leave_edit();
            } else {
                self.enter_edit();
            }
            return;
        }
        self.close_menu();
        match id {
            c if (CMD_SIZE..CMD_SIZE + 3).contains(&c) => self.change_size(uid, Size::ALL[c - CMD_SIZE]),
            CMD_DELETE => self.remove_card(uid),
            c if menu::click_pick(c).is_some() => self.click_choice(uid, c),
            _ => {}
        }
    }

    /// Fade the menu out (Esc, a click outside, a choice, another window in front).
    pub(super) unsafe fn close_menu(&mut self) {
        let Some(m) = self.ui.menu.as_mut() else { return };
        if m.closing {
            return;
        }
        (m.closing, m.press) = (true, None);
        self.menu_last = Instant::now();
        self.menu_period = 16;
        SetTimer(self.hwnd, T_MENU, 16, None);
        self.menu_tick();
    }

    /// The menu is gone: no veil, the layer goes back to click-through under the app windows and the foreground to whoever had it.
    unsafe fn finish_menu(&mut self) {
        let _ = KillTimer(self.hwnd, T_MENU);
        self.menu_period = 0;
        if self.ui.menu.take().is_none() {
            return;
        }
        self.menu_shown = None;
        self.ui.sync_panels(&mut self.scene, &self.gfx);
        self.sync_regions();
        let full = RECT { left: 0, top: 0, right: self.scene.w, bottom: self.scene.h };
        self.frame(full);
        self.modal_end();
    }

    /// Teardown (edit mode toggle, fullscreen guard, monitor change, the card is gone): drop the menu at once, no animation.
    pub(super) unsafe fn abort_menu(&mut self) {
        if self.ui.menu.is_some() {
            self.finish_menu();
        }
    }

    /// Mouse while the menu is up (monitor px): nothing behind it is reachable; a click outside the panels closes it.
    pub(super) unsafe fn menu_mouse(&mut self, msg: u32, x: i32, y: i32) {
        let Some(m) = self.ui.menu.as_mut().filter(|m| !m.closing) else { return };
        let (lx, ly) = (x as f32 / self.s, y as f32 / self.s);
        let act = match msg {
            WM_MOUSEMOVE => m.mouse_move(lx, ly),
            WM_LBUTTONDOWN | WM_RBUTTONDOWN => m.mouse_down(lx, ly),
            WM_LBUTTONUP | WM_RBUTTONUP => m.mouse_up(lx, ly),
            _ => MAct::None, // the wheel does nothing
        };
        self.menu_act(act);
    }

    /// Keys while the menu is up (the layer is the foreground window, so WM_KEYDOWN arrives here).
    pub(super) unsafe fn menu_key(&mut self, msg: u32, wp: usize) {
        if msg != WM_KEYDOWN {
            return;
        }
        let Some(m) = self.ui.menu.as_mut().filter(|m| !m.closing) else { return };
        let key = match VIRTUAL_KEY(wp as u16) {
            VK_ESCAPE => Key::Esc,
            VK_RETURN => Key::Enter,
            VK_SPACE => Key::Char(' '),
            VK_UP => Key::Up,
            VK_DOWN => Key::Down,
            VK_LEFT => Key::Left,
            VK_RIGHT => Key::Right,
            VK_HOME => Key::Home,
            VK_END => Key::End,
            _ => return,
        };
        let act = m.key(key);
        self.menu_act(act);
    }
}
