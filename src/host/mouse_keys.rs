//! Mouse and keyboard input of the desk: cursor poll, hover, the edit-mode mouse, settings clicks and sliders, keys.
use super::*;

impl App {
    pub(super) unsafe fn send_mouse(&mut self, i: usize, kind: MouseKind, x: i32, y: i32, button: Button, delta: f32, now: &Now) -> bool {
        let changed = self.scene.mouse(&self.gfx, i, kind, x, y, button, delta, now);
        if let Some(r) = self.settle(i, now, changed) {
            self.frame(r);
        }
        if self.scene.take_fx_wake() {
            // hover / press / glint moved: the animation timer paints it, then stops again. Only a stopped timer restarts its clock:
            // resetting it on every mouse move would shrink each step's dt and slow the easing down while the pointer moves
            if self.anim_period == 0 {
                self.last_anim = Instant::now();
            }
            self.arm_anim();
        }
        changed
    }

    pub(super) unsafe fn leave(&mut self, now: &Now) {
        if let Some(o) = self.hot.take() {
            if o < self.scene.len() {
                self.send_mouse(o, MouseKind::Leave, 0, 0, Button::None, 0.0, now);
            }
        }
    }

    // ------------------------------------------------ mouse

    /// Cursor position in monitor px for the message being handled.
    pub(super) unsafe fn mouse_pos(&self) -> (i32, i32) {
        let p = GetMessagePos();
        (((p & 0xFFFF) as i16 as i32) - self.mon.left, (((p >> 16) & 0xFFFF) as i16 as i32) - self.mon.top)
    }

    /// Window mouse messages (live mode: only delivered while the window is not click-through, i.e. the cursor is over a card;
    /// edit mode / drag: the whole window).
    pub(super) unsafe fn on_mouse(&mut self, msg: u32, wp: WPARAM, lp: LPARAM) {
        let now = self.now();
        match msg {
            WM_MOUSELEAVE => {
                self.tracking = false;
                if self.pressed.is_none() && !self.ui.edit {
                    self.leave(&now);
                }
                if self.ui.edit && self.ui.hover != Hit::None && self.press == Press::None {
                    self.set_hover(Hit::None);
                }
                return;
            }
            WM_CAPTURECHANGED => {
                if self.drag.is_some() {
                    // another window took the capture (alt-tab, a popup): drop the card where it is instead of leaving it stuck to the pointer
                    if HWND(lp.0 as *mut std::ffi::c_void) != self.hwnd {
                        self.end_drag();
                    }
                } else {
                    self.pressed = None;
                    self.press = Press::None;
                }
                return;
            }
            _ => {}
        }
        let (x, y) = self.mouse_pos();
        if !self.tracking && msg == WM_MOUSEMOVE {
            let mut t = TRACKMOUSEEVENT { cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: self.hwnd, dwHoverTime: 0 };
            if TrackMouseEvent(&mut t).is_ok() {
                self.tracking = true;
            }
        }
        if self.ui.menu.is_some() {
            return self.menu_mouse(msg, x, y);
        }
        if self.ui.prompt.is_some() {
            return self.prompt_mouse(msg, wp, x, y);
        }
        if self.ui.edit {
            return self.edit_mouse(msg, wp, x, y);
        }
        let under = self.scene.cards.iter().rposition(|c| x >= c.x && x < c.x + c.w && y >= c.y && y < c.y + c.h);
        if self.pressed.is_none() && self.hot != under {
            self.leave(&now);
            self.hot = under;
        }
        let uid_of = |s: &Scene, i: usize| s.cards.get(i).map_or(0, |c| c.uid);
        // a press that turned into a drag: the card follows the pointer, the widget hears nothing more
        if let Press::Drag = self.press {
            match msg {
                WM_MOUSEMOVE => {
                    if let Some(d) = self.drag.as_mut() {
                        d.cur = (x, y);
                    }
                }
                WM_LBUTTONUP => self.end_drag(),
                _ => {}
            }
            if matches!(msg, WM_LBUTTONUP) && wp.0 & 0x13 == 0 {
                let _ = ReleaseCapture();
                self.pressed = None;
            }
            return;
        }
        if let (Press::Cand { uid, x: x0, y: y0 }, WM_MOUSEMOVE) = (self.press, msg) {
            if (x - x0).abs().max((y - y0).abs()) > DRAG_PX {
                if let Some(i) = self.scene.index_of_uid(uid) {
                    self.send_mouse(i, MouseKind::Leave, 0, 0, Button::None, 0.0, &now);
                    self.hot = None;
                }
                self.begin_drag(uid, x0, y0, x, y, false, None);
                return;
            }
        }
        let Some(i) = self.pressed.or(under) else { return };
        let (kind, button, delta) = match msg {
            WM_MOUSEMOVE => (MouseKind::Move, Button::None, 0.0),
            WM_LBUTTONDOWN => (MouseKind::Down, Button::Left, 0.0),
            WM_LBUTTONUP => (MouseKind::Up, Button::Left, 0.0),
            WM_RBUTTONDOWN => (MouseKind::Down, Button::Right, 0.0),
            WM_RBUTTONUP => (MouseKind::Up, Button::Right, 0.0),
            WM_MBUTTONDOWN => (MouseKind::Down, Button::Middle, 0.0),
            WM_MBUTTONUP => (MouseKind::Up, Button::Middle, 0.0),
            WM_MOUSEWHEEL => (MouseKind::Wheel, Button::None, ((wp.0 >> 16) as u16 as i16) as f32 / 120.0),
            _ => return,
        };
        if matches!(msg, WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN) && self.pressed.is_none() {
            SetCapture(self.hwnd);
            self.pressed = Some(i);
        }
        let changed = self.send_mouse(i, kind, x, y, button, delta, &now);
        match (msg, changed) {
            (WM_LBUTTONDOWN, true) => self.press = Press::Widget(uid_of(&self.scene, i)),
            (WM_LBUTTONDOWN, false) => self.press = Press::Cand { uid: uid_of(&self.scene, i), x, y },
            (WM_RBUTTONUP, false) if under == Some(i) => {
                // the widget did not use the right click: the card context menu (run outside of this borrow)
                let uid = uid_of(&self.scene, i);
                let p = GetMessagePos();
                let _ = PostMessageW(self.hwnd, WM_MENU, WPARAM(uid), LPARAM(p as isize));
            }
            _ => {}
        }
        if matches!(msg, WM_LBUTTONUP | WM_RBUTTONUP | WM_MBUTTONUP) && wp.0 & 0x13 == 0 {
            let _ = ReleaseCapture();
            self.pressed = None;
            self.press = Press::None;
        }
    }

    // ------------------------------------------------ edit mode mouse

    pub(super) fn hover_dirty(&self, h: Hit) -> Option<RECT> {
        let s = self.s;
        let px = |r: Rect| RECT { left: (r.x * s) as i32 - 8, top: (r.y * s) as i32 - 8, right: (r.right() * s).ceil() as i32 + 8, bottom: (r.bottom() * s).ceil() as i32 + 8 };
        match h {
            Hit::Minus(i) | Hit::Card(i) => (i < self.scene.len()).then(|| self.scene.layer_rect_ext(i, self.ext())),
            Hit::Slider(_) | Hit::Seg(_) | Hit::SettingsBtn | Hit::Toolbar => Some(px(self.ui.toolbar_rect())),
            Hit::None => None,
            _ => Some(px(self.ui.panel_rect())),
        }
    }

    pub(super) unsafe fn set_hover(&mut self, h: Hit) {
        let same = |a: Hit, b: Hit| match (a, b) {
            (Hit::Slider(_), Hit::Slider(_)) => true,
            (Hit::Search(_), Hit::Search(_)) => true,
            (Hit::Look(i, _), Hit::Look(j, _)) => i == j,
            _ => a == b,
        };
        if same(self.ui.hover, h) {
            return;
        }
        let old = self.ui.hover;
        self.ui.hover = h;
        // the settings panel eases its hover looks in and out: the animation timer repaints just the control
        if self.ui.settings && (ui::hkey(old).is_some() || ui::hkey(h).is_some()) {
            self.arm_anim();
            return;
        }
        // only hovers that change how something looks need a frame
        let looks = |h: Hit| matches!(h, Hit::Minus(_) | Hit::Group(_) | Hit::Item(_) | Hit::Done | Hit::SettingsBtn | Hit::Look(..) | Hit::Opt(..) | Hit::Autostart | Hit::CheckStart | Hit::UpdBtn | Hit::UpdInstall | Hit::UpdPage | Hit::ResetLook | Hit::EditBtn | Hit::QuitBtn);
        if looks(old) || looks(h) {
            let mut d: Option<RECT> = None;
            for r in [self.hover_dirty(old), self.hover_dirty(h)].into_iter().flatten() {
                d = Some(d.map_or(r, |x| union(x, r)));
            }
            if let Some(d) = d {
                self.frame(d);
            }
        }
    }

    pub(super) unsafe fn edit_mouse(&mut self, msg: u32, wp: WPARAM, x: i32, y: i32) {
        let hit = self.ui.hit(&self.scene, x, y);
        match msg {
            WM_MOUSEMOVE => {
                match self.press {
                    Press::Drag => {
                        if let Some(d) = self.drag.as_mut() {
                            d.cur = (x, y);
                        }
                        return;
                    }
                    Press::Cand { uid, x: x0, y: y0 } => {
                        if (x - x0).abs().max((y - y0).abs()) > DRAG_PX - 2 {
                            self.begin_drag(uid, x0, y0, x, y, false, None);
                        }
                        return;
                    }
                    Press::Preview { k, x: x0, y: y0 } => {
                        if (x - x0).abs().max((y - y0).abs()) > DRAG_PX {
                            self.drag_from_preview(k, x, y);
                        }
                        return;
                    }
                    Press::Ui(Hit::Slider(_)) => {
                        let v = self.ui.slider_value(x);
                        self.apply_glass(v);
                        return;
                    }
                    Press::Ui(Hit::Look(i, _)) => {
                        self.ui.sa_drag_move(i, x); // far from where it was pressed: the handle follows 1:1
                        let t = self.ui.set_value_at(i, x);
                        self.set_look_slider(i, t);
                        return;
                    }
                    Press::Move { x: x0, y: y0, ox, oy } => {
                        let old = self.ui.dirty_rect(&self.scene);
                        self.ui.move_panel(ox + (x - x0) as f32 / self.s, oy + (y - y0) as f32 / self.s);
                        self.ui.sync_panels(&mut self.scene, &self.gfx);
                        if let (Some(a), Some(b)) = (old, self.ui.dirty_rect(&self.scene)) {
                            self.frame(union(a, b));
                        }
                        return;
                    }
                    _ => {}
                }
                self.set_hover(hit);
            }
            WM_LBUTTONDOWN => {
                SetCapture(self.hwnd);
                self.ui.press = hit;
                self.flush_look_commit();
                let old_focus = self.ui.sfocus;
                self.ui.sfocus = match hit {
                    Hit::Look(i, _) => Some(i),
                    Hit::Slider(_) => Some(0),
                    _ => None,
                };
                if self.ui.sfocus.is_some() {
                    self.ui.st.focus = false;
                }
                if old_focus != self.ui.sfocus {
                    self.redraw_ui();
                }
                match hit {
                    Hit::Card(i) => {
                        self.ui.st.focus = false;
                        self.press = Press::Cand { uid: self.scene.cards[i].uid, x, y };
                    }
                    Hit::Item(k) => self.press = Press::Preview { k, x, y },
                    Hit::Slider(v) => {
                        self.press = Press::Ui(hit);
                        self.apply_glass(v);
                    }
                    Hit::Search(dx) => {
                        self.ui.st.focus = true;
                        let c = self.ui.caret_at(&self.scene, &self.gfx, dx);
                        self.ui.st.query.set_caret(c);
                        self.ui.caret_on = true;
                        self.press = Press::Ui(hit);
                        let d = self.hover_dirty(hit);
                        if let Some(d) = d {
                            self.frame(d);
                        }
                    }
                    Hit::Look(i, t) => {
                        self.press = Press::Ui(hit);
                        self.ui.sa_drag_start(i, x, t);
                        self.set_look_slider(i, t);
                        self.arm_anim(); // the knob swells, the handle glides to a click on the track
                    }
                    Hit::Panel => {
                        // an empty part of the panel is a handle: drag the whole panel
                        let p = self.ui.panel_rect();
                        self.press = Press::Move { x, y, ox: p.x, oy: p.y };
                        self.ui.st.focus = false;
                    }
                    Hit::None if self.ui.settings => self.press = Press::Ui(Hit::None), // a click outside the panel closes it on release
                    Hit::None => self.ui.st.focus = false,
                    _ if self.ui.settings && ui::hkey(hit).is_some() => {
                        // a button / segment / switch of the settings panel: the press spring repaints it
                        self.press = Press::Ui(hit);
                        self.arm_anim();
                    }
                    _ => {
                        self.press = Press::Ui(hit);
                        if let Some(d) = self.hover_dirty(hit) {
                            self.frame(d);
                        }
                    }
                }
            }
            WM_LBUTTONUP => {
                let press = std::mem::replace(&mut self.press, Press::None);
                self.ui.press = Hit::None;
                self.ui.sa_drag_end();
                let _ = ReleaseCapture();
                match press {
                    Press::Drag => self.end_drag(),
                    Press::Preview { k, .. } => {
                        if let Some((def, size)) = self.ui.item(k) {
                            self.add_widget(def.type_id, size, None);
                        }
                    }
                    Press::Ui(Hit::Slider(_)) => {
                        let v = self.ui.slider_value(x);
                        self.apply_glass(v);
                        let v = (v * 100.0).round() / 100.0;
                        self.write_setting("widgetsGlass", serde_json::json!(v));
                        self.settings.glass = v;
                    }
                    Press::Ui(Hit::Look(i, _)) => {
                        let t = self.ui.set_value_at(i, x);
                        self.set_look_slider(i, t);
                        self.commit_look(i);
                    }
                    Press::Move { .. } => self.save_panels(),
                    Press::Ui(h) => {
                        let same = match (h, hit) {
                            (Hit::Search(_), Hit::Search(_)) => true,
                            (a, b) => a == b,
                        };
                        if same {
                            self.ui_click(h);
                        } else if self.ui.settings && ui::hkey(h).is_some() {
                            self.arm_anim(); // released elsewhere: the press spring lets go
                        } else if let Some(d) = self.hover_dirty(h) {
                            self.frame(d);
                        }
                    }
                    _ => {}
                }
                self.set_hover(hit);
                if self.ui.settings {
                    self.arm_anim(); // the press spring of whatever was held
                }
            }
            WM_RBUTTONUP => {
                if let Hit::Card(i) | Hit::Minus(i) = hit {
                    let uid = self.scene.cards[i].uid;
                    let p = GetMessagePos();
                    let _ = PostMessageW(self.hwnd, WM_MENU, WPARAM(uid), LPARAM(p as isize));
                }
            }
            WM_MOUSEWHEEL => {
                let delta = ((wp.0 >> 16) as u16 as i16) as f32 / 120.0;
                // over a slider the wheel changes it (2 % a notch); elsewhere it scrolls the gallery
                let (lx, ly) = (x as f32 / self.s, y as f32 / self.s);
                let slider = if self.ui.settings {
                    self.ui.row_at(lx, ly)
                } else {
                    matches!(hit, Hit::Slider(_)).then_some(0)
                };
                if let (Some(i), Press::None) = (slider, self.press) {
                    self.ui.sfocus = Some(i);
                    self.ui.st.focus = false;
                    let t = ui::wheel_pos(self.ui.set_pos(i), delta);
                    self.nudge_slider(i, t);
                    self.redraw_ui();
                    return;
                }
                if self.ui.wheel(x as f32 / self.s, y as f32 / self.s, delta) {
                    if let Some(d) = self.hover_dirty(Hit::Panel) {
                        self.frame(d);
                    }
                }
            }
            _ => {}
        }
    }

    pub(super) unsafe fn ui_click(&mut self, h: Hit) {
        match h {
            Hit::Done if self.ui.settings => self.close_settings(),
            Hit::None if self.ui.settings => self.close_settings(),
            Hit::SettingsBtn => self.open_settings(),
            Hit::ResetLook => self.reset_look(),
            Hit::Opt(o, k) => self.set_option(o, k),
            Hit::Autostart => self.toggle_autostart(),
            Hit::CheckStart => self.toggle_check_start(),
            Hit::UpdBtn => self.start_update_check(false),
            Hit::UpdInstall => self.start_install(),
            Hit::UpdPage => {
                if let UpdateState::Available { url, .. } = self.ui.update.clone() {
                    crate::sys::perform(&crate::sys::Action::Open(url)); // the release page in the browser
                }
            }
            Hit::EditBtn => self.switch_panel(false),
            Hit::QuitBtn => {
                self.flush_look_commit();
                let _ = DestroyWindow(self.hwnd); // the Ctrl+Alt+Q path
            }
            Hit::Done => self.leave_edit(),
            Hit::Minus(i) => {
                if let Some(c) = self.scene.cards.get(i) {
                    let uid = c.uid;
                    self.remove_card(uid);
                }
            }
            Hit::Group(i) => {
                self.ui.select_group(i);
                self.gallery_changed();
            }
            Hit::Chip(s) => {
                self.ui.select_size(s);
                self.gallery_changed();
            }
            Hit::Seg(live) => self.set_live(live),
            _ => {}
        }
    }

    pub(super) unsafe fn gallery_changed(&mut self) {
        let now = Now::local();
        self.ui.sync_previews(&self.gfx, &now);
        if !self.reduced {
            self.ui.gal_note();
        }
        self.arm_ui_timer();
        if let Some(d) = self.hover_dirty(Hit::Panel) {
            self.frame(d);
        }
        self.arm_anim(); // the tiles fade in
    }

    // ------------------------------------------------ keyboard (edit mode: the search box, the sliders)

    /// Keys for the sliders: all of them in the settings panel; in the gallery only while the strip's glass slider has the focus (Tab goes
    /// between the search box and that slider). True = the key was used.
    pub(super) unsafe fn slider_key(&mut self, k: SKey, shift: bool) -> bool {
        if self.ui.gallery {
            match (self.ui.sfocus.is_some(), k) {
                (false, SKey::Tab) => {
                    self.ui.sfocus = Some(0);
                    self.ui.st.focus = false;
                }
                (true, SKey::Tab) => {
                    self.ui.sfocus = None;
                    self.ui.st.focus = true;
                    self.ui.caret_on = true;
                }
                (true, SKey::Up | SKey::Down) => return true,
                (true, _) => {
                    if let Some(SAct::Set(i, t)) = self.ui.slider_key(k, shift) {
                        self.nudge_slider(i, t);
                    }
                    return true;
                }
                (false, _) => return false,
            }
            self.redraw_ui();
            return true;
        }
        match self.ui.slider_key(k, shift) {
            Some(SAct::Set(i, t)) => self.nudge_slider(i, t),
            Some(SAct::Focus) => self.redraw_ui(),
            None => {}
        }
        true
    }

    /// Maps a virtual key to a slider key; true = used.
    pub(super) unsafe fn slider_key_vk(&mut self, vk: VIRTUAL_KEY) -> bool {
        let k = match vk {
            VK_UP => SKey::Up,
            VK_DOWN => SKey::Down,
            VK_LEFT => SKey::Left,
            VK_RIGHT => SKey::Right,
            VK_HOME => SKey::Home,
            VK_END => SKey::End,
            VK_BACK | VK_DELETE => SKey::Reset,
            VK_TAB => SKey::Tab,
            _ => return self.ui.settings, // the settings panel has no text field: other keys are swallowed
        };
        self.slider_key(k, GetKeyState(VK_SHIFT.0 as i32) < 0)
    }

    /// A slider moved from the keyboard / wheel: shown and applied live like a drag, committed to config.json (`commit_look`, as on release)
    /// once the burst of key repeats is over.
    pub(super) unsafe fn nudge_slider(&mut self, i: usize, t: f32) {
        if self.look_commit.is_some_and(|p| p != i) {
            self.flush_look_commit();
        }
        if self.ui.settings {
            self.set_look_slider(i, t);
            self.frame_row(i);
        } else {
            self.apply_glass(t.clamp(0.0, 1.0)); // the strip's glass slider
        }
        self.look_commit = Some(i);
        SetTimer(self.hwnd, T_LCOMMIT, LCOMMIT_AFTER, None);
    }

    pub(super) unsafe fn flush_look_commit(&mut self) {
        let _ = KillTimer(self.hwnd, T_LCOMMIT);
        if let Some(i) = self.look_commit.take() {
            self.commit_look(i);
        }
    }

    /// Repaint the open panel (and the strip under the gallery).
    pub(super) unsafe fn redraw_ui(&mut self) {
        let mut d = self.hover_dirty(Hit::Panel);
        if !self.ui.settings {
            if let (Some(a), Some(b)) = (d, self.hover_dirty(Hit::Toolbar)) {
                d = Some(union(a, b));
            }
        }
        if let Some(d) = d {
            self.frame(d);
        }
    }

    /// Remember where the user left the gallery / settings panel (only when it changed).
    pub(super) fn save_panels(&mut self) {
        let now = (self.ui.gal_off, self.ui.set_off);
        if now == self.panels_saved {
            return;
        }
        self.panels_saved = now;
        if let Err(e) = layout::write_panels(now.0, now.1) {
            eprintln!("mswidgets: {e}");
        }
        self.note_own_write();
    }

    pub(super) unsafe fn on_key(&mut self, msg: u32, wp: usize) {
        if self.ui.menu.is_some() {
            return self.menu_key(msg, wp);
        }
        if self.ui.prompt.is_some() {
            return self.prompt_key(msg, wp);
        }
        if !self.ui.edit {
            return;
        }
        let ctrl = GetKeyState(VK_CONTROL.0 as i32) < 0;
        let mut changed = false;
        match msg {
            WM_KEYDOWN | WM_SYSKEYDOWN => {
                let vk = VIRTUAL_KEY(wp as u16);
                match vk {
                    VK_ESCAPE => {
                        self.esc_down = true;
                        return if self.ui.settings { self.close_settings() } else { self.leave_edit() };
                    }
                    _ if !ctrl && self.slider_key_vk(vk) => return,
                    _ if !self.ui.gallery => return,
                    VK_BACK => {
                        self.ui.st.query.backspace();
                        changed = true;
                    }
                    VK_DELETE => {
                        self.ui.st.query.delete();
                        changed = true;
                    }
                    VK_LEFT => self.ui.st.query.left(),
                    VK_RIGHT => self.ui.st.query.right(),
                    VK_HOME => self.ui.st.query.home(),
                    VK_END => self.ui.st.query.end(),
                    VK_V if ctrl => {
                        if let Some(t) = clipboard_text(self.hwnd) {
                            self.ui.st.query.insert_str(&t);
                            changed = true;
                        }
                    }
                    VK_A if ctrl => self.ui.st.query.select_all(),
                    _ => return,
                }
            }
            WM_CHAR => {
                if !self.ui.gallery || ctrl {
                    return;
                }
                let u = wp as u32;
                let Some(c) = char::from_u32(u) else { return }; // lone surrogates (astral plane) are ignored: no emoji search
                if c.is_control() {
                    return;
                }
                self.ui.st.query.insert(c);
                changed = true;
            }
            _ => return,
        }
        self.ui.st.focus = true;
        self.ui.sfocus = None;
        self.ui.caret_on = true;
        if changed {
            self.ui.query_changed();
            self.gallery_changed();
        } else if let Some(d) = self.hover_dirty(Hit::Panel) {
            self.frame(d);
        }
    }

    // ------------------------------------------------ drag & drop
}
