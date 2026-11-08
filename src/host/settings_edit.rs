//! Settings panel, edit mode, toast, look sliders, options, autostart, update check and one-click install.
use super::*;

impl App {
    pub(super) unsafe fn set_transparent(&mut self, on: bool) {
        let ex = GetWindowLongPtrW(self.hwnd, GWL_EXSTYLE);
        let tr = WS_EX_TRANSPARENT.0 as isize;
        SetWindowLongPtrW(self.hwnd, GWL_EXSTYLE, if on { ex | tr } else { ex & !tr });
        self.interactive = !on;
    }

    pub(super) fn tray_state(&self) -> tray::MenuState {
        let root = layout::read_config();
        let profiles = layout::profile_names(&root);
        let active = layout::active_profile_for(&root, self.slot.as_deref()).and_then(|a| profiles.iter().position(|p| *p == a));
        let list = monitors::enumerate();
        let monitor = monitors::pick(&list).map_or(0, |i| monitors::position(&list, i));
        let monitors = (0..list.len()).map(|p| monitors::label(&list, p, self.ui.lang)).collect();
        let scene_programs = unsafe { running_programs() };
        if let Ok(mut g) = scene::SCENE_PROGRAMS.lock() {
            g.clone_from(&scene_programs);
        }
        tray::MenuState { lang: self.ui.lang, editing: self.editing, hidden: self.user_hidden, autostart: crate::autostart::enabled(), profiles, active, monitors, monitor, scene_rules: self.scene_menu_lines(), scene_programs, scene_active: self.scenes.applied_rule().and_then(|a| self.scene_rules.iter().position(|r| r == a)) }
    }

    /// A tray click / menu choice.
    pub(super) unsafe fn apply_tray(&mut self, cmd: TrayCmd) {
        if matches!(cmd, TrayCmd::Settings | TrayCmd::Edit) && self.user_hidden {
            self.user_hidden = false; // asking for the settings brings the widgets back
            self.guard();
        }
        match cmd {
            TrayCmd::None => {}
            TrayCmd::Settings => self.open_settings(),
            TrayCmd::Edit => {
                if self.editing {
                    self.leave_edit();
                } else {
                    self.enter_edit();
                }
            }
            TrayCmd::ToggleVisible => {
                self.user_hidden = !self.user_hidden;
                self.guard();
            }
            TrayCmd::ToggleAutostart => {
                if let Err(e) = crate::autostart::set(!crate::autostart::enabled()) {
                    self.show_toast(format!("{}: {e}", self.ui.lang.pick("Автозапуск не изменён", "Autostart not changed")));
                }
            }
            TrayCmd::Profile(i) => self.switch_profile(i),
            TrayCmd::SaveProfile => self.ask_profile_name(),
            TrayCmd::DeleteProfile => self.delete_profile(),
            TrayCmd::Monitor(i) => self.set_monitor(i),
            TrayCmd::SceneRemove(i) => self.scene_remove(i),
            TrayCmd::SceneProgram(prog, prof) => self.scene_add_program(prog, prof),
            TrayCmd::SceneTyped(prof) => self.ask_scene_text(false, prof),
            TrayCmd::SceneTime(prof) => self.ask_scene_text(true, prof),
            TrayCmd::Quit => {
                let _ = DestroyWindow(self.hwnd);
            }
        }
    }

    /// Entry point of the settings panel (tray menu, Ctrl+Alt+G, the strip's button): from the live desk the layer becomes interactive
    /// exactly as for edit mode (activatable, raised, click-through off); inside edit mode the panel replaces the gallery.
    pub(super) unsafe fn open_settings(&mut self) {
        self.abort_menu();
        if self.ui.settings {
            return;
        }
        if self.editing {
            self.switch_panel(true);
        } else {
            self.enter_edit_as(true);
        }
    }

    /// Esc / Done / click outside / Ctrl+Alt+G: back to the gallery when the panel came from it, else out of edit mode altogether.
    pub(super) unsafe fn close_settings(&mut self) {
        if !self.ui.settings {
            return;
        }
        if self.settings_back {
            self.switch_panel(false);
        } else {
            self.leave_edit();
        }
    }

    /// The values the settings panel shows (behaviour options, the autostart switch); the look sliders are kept in `ui` all the time.
    pub(super) fn sync_settings_ui(&mut self) {
        self.ui.appearance = self.settings.appearance;
        self.ui.dim_mode = self.settings.dim;
        self.ui.live = self.settings.live;
        self.ui.accent_mode = self.settings.accent;
        self.ui.accent_scope = self.settings.accent_scope;
        self.ui.autostart = crate::autostart::enabled();
        self.ui.check_start = self.settings.check_updates;
        self.ui.sa_reset(); // the panel opens showing the real values, nothing glides from stale ones
    }

    /// Swap the open panel in place (gallery <-> settings) inside the running edit mode; the new panel fades in.
    pub(super) unsafe fn switch_panel(&mut self, settings: bool) {
        if !self.editing || self.ui.settings == settings {
            return;
        }
        if self.drag.is_some() {
            self.end_drag();
        }
        self.flush_look_commit();
        self.ui.sfocus = None;
        self.ui.settings = settings;
        self.ui.gallery = !settings;
        self.settings_back = settings;
        self.ui.hover = Hit::None;
        self.ui.press = Hit::None;
        self.press = Press::None;
        self.ui.st.focus = !settings;
        self.ui.caret_on = true;
        if settings {
            self.sync_settings_ui();
        } else {
            self.ui.sync_previews(&self.gfx, &Now::local());
        }
        self.ui.anim = 0.0;
        self.gal_t = 0.0;
        self.ui.sync_panels(&mut self.scene, &self.gfx);
        self.sync_regions();
        self.last_anim = Instant::now();
        self.arm_ui_timer();
        self.arm_anim();
        self.anim_step();
        let d = RECT { left: 0, top: 0, right: self.scene.w, bottom: self.scene.h };
        self.frame(d);
    }

    pub(super) unsafe fn enter_edit(&mut self) {
        self.enter_edit_as(false);
    }

    pub(super) unsafe fn enter_edit_as(&mut self, settings: bool) {
        self.abort_menu(); // the menu belongs to the desk as it was
        if self.editing || self.ui.edit || !self.visible {
            return;
        }
        self.editing = true;
        self.boost_capture();
        self.was_fg = false;
        self.reduced = reduced_motion();
        let now = self.now();
        self.leave(&now); // the widget under the cursor stops hovering
        self.ui.edit = true;
        self.update_dim();
        self.t_edit = Instant::now();
        // keyboard: the layer becomes activatable and takes the foreground (the search box needs it); remember who had it
        let fg = GetForegroundWindow();
        if fg != self.hwnd {
            self.prev_fg = fg;
        }
        let ex = GetWindowLongPtrW(self.hwnd, GWL_EXSTYLE);
        SetWindowLongPtrW(self.hwnd, GWL_EXSTYLE, ex & !(WS_EX_NOACTIVATE.0 as isize) & !(WS_EX_TRANSPARENT.0 as isize));
        self.interactive = true;
        let _ = SetWindowPos(self.hwnd, HWND_TOP, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
        self.ui.edit = true;
        self.ui.gallery = !settings;
        self.ui.settings = settings;
        self.settings_back = false;
        self.ui.sfocus = None;
        self.ui.st.focus = !settings;
        self.ui.caret_on = true;
        self.ui.anim = 0.0;
        self.ui.dim = 0.0;
        self.gal_t = 0.0;
        self.dim_t = 0.0;
        let now = Now::local();
        let r = RECT { left: 0, top: 0, right: self.scene.w, bottom: self.scene.h };
        self.set_window(r);
        if settings {
            self.sync_settings_ui();
        } else {
            self.ui.sync_previews(&self.gfx, &now);
        }
        self.ui.sync_panels(&mut self.scene, &self.gfx);
        self.sync_regions();
        force_foreground(self.hwnd);
        let _ = SetFocus(self.hwnd);
        self.last_anim = Instant::now();
        self.arm_ui_timer();
        self.arm_anim();
        self.anim_step();
    }

    /// Esc / "Готово" / hotkey / menu: start the closing animation; `finish_leave_edit` runs when it is over.
    pub(super) unsafe fn leave_edit(&mut self) {
        self.abort_menu();
        if !self.editing {
            return;
        }
        self.editing = false;
        self.boost_capture();
        self.flush_look_commit();
        self.ui.sfocus = None;
        if self.drag.is_some() {
            self.end_drag();
        }
        self.ui.press = Hit::None;
        self.ui.hover = Hit::None;
        self.last_anim = Instant::now();
        self.arm_anim();
        self.anim_step();
    }

    pub(super) unsafe fn finish_leave_edit(&mut self) {
        self.ui.edit = false;
        self.update_dim();
        self.ui.gallery = false;
        self.ui.settings = false;
        self.settings_back = false;
        self.ui.anim = 0.0;
        self.ui.dim = 0.0;
        self.ui.st.focus = false;
        self.ui.sync_panels(&mut self.scene, &self.gfx);
        self.sync_regions();
        let _ = KillTimer(self.hwnd, T_UI);
        // activation off again, the layer goes back under the app windows and the foreground to whoever had it
        let ex = GetWindowLongPtrW(self.hwnd, GWL_EXSTYLE);
        SetWindowLongPtrW(self.hwnd, GWL_EXSTYLE, ex | WS_EX_NOACTIVATE.0 as isize | WS_EX_TRANSPARENT.0 as isize);
        self.interactive = false;
        if GetForegroundWindow() == self.hwnd && !self.prev_fg.0.is_null() && IsWindow(self.prev_fg).as_bool() {
            let _ = SetForegroundWindow(self.prev_fg);
        }
        place_above_desktop(self.hwnd);
        self.fit_window();
        let w = self.win;
        self.frame(w);
        self.arm_anim();
    }

    // ------------------------------------------------ gallery timer / toast

    pub(super) unsafe fn arm_ui_timer(&mut self) {
        if self.ui.edit && (self.ui.gallery || self.ui.settings) {
            SetTimer(self.hwnd, T_UI, 200, None);
        }
    }

    /// 200 ms while the gallery is open: animated previews (<= 5 fps), the search caret blink, Esc fallback.
    pub(super) unsafe fn ui_timer(&mut self) {
        if !(self.ui.edit && (self.ui.gallery || self.ui.settings)) {
            let _ = KillTimer(self.hwnd, T_UI);
            return;
        }
        let mut redraw = false;
        if self.ui.gallery && self.ui.refresh_previews(&self.gfx, &Now::local()) {
            redraw = true;
        }
        self.ui_tick += 1;
        if self.ui.gallery && self.ui.st.focus && self.ui_tick % 3 == 0 {
            self.ui.caret_on = !self.ui.caret_on;
            redraw = true;
        }
        // Esc even when the key message went elsewhere (the layer is foreground, but a popup may have eaten it)
        let esc = GetAsyncKeyState(VK_ESCAPE.0 as i32) < 0 && GetForegroundWindow() == self.hwnd;
        if esc && !self.esc_down {
            self.esc_down = true;
            return if self.ui.settings { self.close_settings() } else { self.leave_edit() };
        }
        if !esc {
            self.esc_down = false;
        }
        if redraw {
            if let Some(d) = self.hover_dirty(Hit::Panel) {
                self.frame(d);
            }
        }
    }

    pub(super) fn toast_rect(&self) -> RECT {
        let s = self.s;
        RECT { left: ((self.ui.mw / 2.0 - 420.0) * s) as i32, top: ((self.ui.mh * 0.5 - 40.0) * s) as i32, right: ((self.ui.mw / 2.0 + 420.0) * s) as i32, bottom: ((self.ui.mh * 0.5 + 40.0) * s) as i32 }
    }

    pub(super) unsafe fn show_toast(&mut self, msg: String) {
        self.ui.toast_show(msg, self.reduced);
        SetTimer(self.hwnd, T_TOAST, 2000, None);
        let r = self.desired_window();
        self.set_window(r); // live mode: the layer only covers the cards, the toast sits mid-screen
        let d = self.toast_rect();
        self.frame(d); // arms the animation timer for the rise
    }

    // ------------------------------------------------ settings

    pub(super) fn note_own_write(&mut self) {
        self.cfg_mtime = layout::config_mtime();
    }

    pub(super) fn write_setting(&mut self, key: &str, v: serde_json::Value) {
        if let Err(e) = layout::write_setting(key, v) {
            eprintln!("mswidgets: {e}");
        }
        self.note_own_write();
    }

    pub(super) fn save_layout(&mut self) {
        if let Err(e) = layout::save_layout(&self.items) {
            eprintln!("mswidgets: layout not saved: {e}");
        }
        self.note_own_write();
    }

    /// The glass slider: new tint strength for every card, live.
    pub(super) unsafe fn apply_glass(&mut self, g: f32) {
        if (self.ui.glass - g).abs() < 1e-4 {
            return;
        }
        self.boost_capture();
        self.ui.glass = g;
        if let Err(e) = self.scene.set_glass(&self.gfx, g) {
            eprintln!("mswidgets: glass: {e}");
            return;
        }
        let now = self.now();
        for i in 0..self.scene.len() {
            let _ = self.scene.render_card(&self.gfx, i, &now);
        }
        let d = self.all_dirty();
        self.frame(d);
    }

    /// A look slider of the settings panel moved to `t` (0..1; slider 0 = Glass): the panel shows it at once, the cards follow at most
    /// 10 times a second (`T_LOOK` applies a change that arrived too early; `commit_look` applies the final value on release).
    pub(super) unsafe fn set_look_slider(&mut self, i: usize, t: f32) {
        if i == 0 {
            self.ui.glass = t.clamp(0.0, 1.0);
        } else if let Some(d) = crate::glass::LOOK.get(i - 1) {
            d.set_pos(&mut self.ui.look, &self.ui.base, t);
        }
        self.look_dirty = true;
        let wait = LOOK_EVERY.saturating_sub(self.look_last.elapsed());
        if wait.is_zero() {
            self.apply_look_now();
            return;
        }
        SetTimer(self.hwnd, T_LOOK, wait.as_millis().max(1) as u32, None);
        // only the row of the slider is redrawn meanwhile: the knob and the value follow the pointer
        self.frame_row(i);
    }

    /// Repaint row `i` of the settings panel.
    pub(super) unsafe fn frame_row(&mut self, i: usize) {
        let s = self.s;
        if let Some(r) = self.ui.set_lay().rows.get(i).map(|r| r.row) {
            self.frame(RECT { left: ((r.x - 8.0) * s) as i32, top: ((r.y - 4.0) * s) as i32, right: ((r.right() + 8.0) * s).ceil() as i32, bottom: ((r.bottom() + 4.0) * s).ceil() as i32 });
        }
    }

    /// Give the cards the look / glass value the panel shows: new effect graphs, every layer re-rendered, then the cards' area and the panel recomposed.
    pub(super) unsafe fn apply_look_now(&mut self) {
        self.look_dirty = false;
        self.look_last = Instant::now();
        let _ = KillTimer(self.hwnd, T_LOOK);
        // the card scale applies on release (`apply_card_scale`: it needs a new desk grid and capture); nothing else changed = nothing to do
        let mut look = self.ui.look;
        look.card_k = self.scene.params().card_k;
        if look == *self.scene.params() && (self.ui.glass - self.scene.glass_value()).abs() < 1e-6 {
            return;
        }
        let ok = self.scene.set_glass(&self.gfx, self.ui.glass).and_then(|_| self.scene.set_params(&self.gfx, &look));
        if let Err(e) = ok {
            eprintln!("mswidgets: look: {e}");
            return;
        }
        let now = self.now();
        let mut cards: Option<RECT> = None;
        for i in 0..self.scene.len() {
            let _ = self.scene.render_card(&self.gfx, i, &now);
            let r = self.scene.layer_rect_ext(i, self.ext());
            cards = Some(cards.map_or(r, |d| union(d, r)));
        }
        if let Some(d) = cards {
            self.frame(d);
        }
        if let Some(u) = self.ui.dirty_rect(&self.scene) {
            self.frame(u);
        }
    }

    /// A look slider was released: the final value reaches the cards at once and config.json (`widgetsGlass` / `widgetsLook`).
    pub(super) unsafe fn commit_look(&mut self, i: usize) {
        if self.look_dirty {
            self.apply_look_now();
        }
        if i == 0 {
            let v = (self.ui.glass * 100.0).round() / 100.0;
            self.write_setting("widgetsGlass", serde_json::json!(v));
            self.settings.glass = v;
        } else {
            self.save_look();
            self.apply_card_scale();
        }
    }

    /// The card scale slider settled: lay the desk out again (new grid, margin and capture regions; widgets and their state stay).
    pub(super) unsafe fn apply_card_scale(&mut self) {
        if (self.ui.look.card_k - self.scene.params().card_k).abs() > 1e-4 {
            self.rebuild(true);
        }
    }

    /// Write the sliders' differences from the base look into `widgetsLook` (an empty diff removes the sliders' keys).
    pub(super) fn save_look(&mut self) {
        let diff = crate::glass::look_diff(&self.look_base, &self.ui.look);
        match layout::write_look(&diff) {
            Ok(()) => self.look_cfg = layout::merge_look(&self.look_cfg, &diff),
            Err(e) => eprintln!("mswidgets: {e}"),
        }
        self.note_own_write();
    }

    /// "Reset look": the base look (defaults + glass.json) and the default glass value.
    pub(super) unsafe fn reset_look(&mut self) {
        self.ui.look = self.look_base;
        self.ui.glass = layout::Settings::default().glass;
        self.apply_look_now();
        self.write_setting("widgetsGlass", serde_json::json!(self.ui.glass));
        self.settings.glass = self.ui.glass;
        self.save_look();
        self.apply_card_scale();
    }

    /// A segmented control of the settings panel: appearance, dimming, glass source.
    pub(super) unsafe fn set_option(&mut self, o: ui::Opt, k: usize) {
        match o {
            ui::Opt::Appearance => {
                let p = ui::Opt::appearance(k);
                self.ui.appearance = p;
                self.settings.appearance = p;
                self.boost_capture();
                self.scene.set_appearance_pref(p);
                self.last_anim = Instant::now();
                self.arm_anim();
                self.write_setting("widgetsAppearance", serde_json::json!(layout::appearance_key(p)));
            }
            ui::Opt::Dim => {
                let d = ui::Opt::dim(k);
                self.ui.dim_mode = d;
                self.settings.dim = d;
                self.write_setting("widgetsDim", serde_json::json!(d.key()));
                self.update_dim(); // takes effect when the panel closes (the desk is not dimmed while editing)
            }
            ui::Opt::Source => self.set_live(k == 0),
            ui::Opt::Accent => {
                let m = ui::Opt::accent(k);
                self.ui.accent_mode = m;
                self.settings.accent = m;
                self.write_setting("accentMode", serde_json::json!(m.key()));
                self.refresh_accent(true);
            }
            ui::Opt::Scope => {
                let sc = ui::Opt::scope(k);
                self.ui.accent_scope = sc;
                self.settings.accent_scope = sc;
                self.write_setting("accentScope", serde_json::json!(sc.key()));
                self.apply_scope();
            }
            ui::Opt::Monitor => return self.set_monitor(k), // redraws the panel itself
        }
        if let Some(d) = self.hover_dirty(Hit::Panel) {
            self.frame(d);
        }
    }

    /// The "Start with Windows" switch: the per-user Run value is written / removed right now.
    pub(super) unsafe fn toggle_autostart(&mut self) {
        match crate::autostart::set(!self.ui.autostart) {
            Ok(()) => self.ui.autostart = crate::autostart::enabled(),
            Err(e) => {
                eprintln!("mswidgets: autostart: {e}");
                self.show_toast(self.ui.lang.pick("Не удалось изменить автозапуск", "Could not change the autostart").to_string());
            }
        }
        if let Some(d) = self.hover_dirty(Hit::Panel) {
            self.frame(d);
        }
    }

    /// The "Check for updates on start" switch (`checkUpdates`, off by default).
    pub(super) unsafe fn toggle_check_start(&mut self) {
        let on = !self.ui.check_start;
        self.ui.check_start = on;
        self.settings.check_updates = on;
        self.write_setting("checkUpdates", serde_json::json!(on));
        self.redraw_ui();
    }

    /// One GET of the latest release on a worker thread (`silent` = the check at start: only an available update is announced, as a toast).
    pub(super) unsafe fn start_update_check(&mut self, silent: bool) {
        if matches!(self.ui.update, UpdateState::Checking | UpdateState::Downloading(_) | UpdateState::Verifying) {
            return;
        }
        self.update_silent = silent;
        self.ui.update = UpdateState::Checking;
        update::start(self.hwnd.0 as isize);
        if self.ui.settings {
            self.redraw_ui();
        }
    }

    pub(super) unsafe fn update_done(&mut self) {
        let Some(r) = update::take() else { return };
        let silent = std::mem::take(&mut self.update_silent);
        self.ui.update = if silent && r.is_err() { UpdateState::Idle } else { update::state_from(r, env!("CARGO_PKG_VERSION")) };
        if let (true, UpdateState::Available { version, .. }) = (silent, &self.ui.update) {
            let msg = self.ui.lang.pick(&format!("Доступна новая версия {version}"), &format!("Version {version} is available")).to_string();
            self.show_toast(msg);
        }
        if self.ui.settings {
            self.redraw_ui();
        }
    }

    /// "Update now" (the user's click): download the installer of the available update and check its SHA-256 on a worker thread.
    pub(super) unsafe fn start_install(&mut self) {
        let UpdateState::Available { version, asset, .. } = self.ui.update.clone() else { return };
        self.update_prev = Some(self.ui.update.clone());
        let Some(asset) = asset else {
            self.install_failed("the release has no installer with a checksum".into());
            return;
        };
        self.ui.update = UpdateState::Downloading(0);
        update::start_install(self.hwnd.0 as isize, &version, asset);
        self.redraw_ui();
    }

    /// `WM_UPDATE_DL`: download percent, "verifying", or the end of the worker.
    pub(super) unsafe fn install_progress(&mut self, p: usize) {
        match p {
            update::DL_DONE => match update::take_install() {
                Some(Ok(v)) => {
                    self.flush_look_commit();
                    match update::launch(v) {
                        // the installer closes this copy itself; leaving now keeps its file replacement from racing with us
                        Ok(()) => {
                            let _ = DestroyWindow(self.hwnd);
                        }
                        Err(e) => self.install_failed(e),
                    }
                }
                Some(Err(e)) => self.install_failed(e),
                None => {}
            },
            update::DL_VERIFYING => self.ui.update = UpdateState::Verifying,
            p => self.ui.update = UpdateState::Downloading(p.min(100) as u8),
        }
        if self.ui.settings {
            self.redraw_ui();
        }
    }

    /// Back to the available update, with the reason shown in the status line (the button stays for a retry).
    pub(super) unsafe fn install_failed(&mut self, why: String) {
        self.ui.update = match self.update_prev.take() {
            Some(UpdateState::Available { version, url, asset, .. }) => UpdateState::Available { version, url, asset, error: Some(why) },
            _ => UpdateState::Failed,
        };
        if self.ui.settings {
            self.redraw_ui();
        }
    }

    /// Live (desktop duplication) <-> plain glass.
    pub(super) unsafe fn set_live(&mut self, live: bool) {
        if self.ui.live == live {
            return;
        }
        self.ui.live = live;
        self.settings.live = live;
        self.write_setting("widgetsGlassMode", serde_json::json!(if live { "capture" } else { "plain" }));
        self.rebuild(true);
    }
}
