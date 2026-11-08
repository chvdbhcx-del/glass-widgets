//! Building the desk: rebuild, monitor changes, config polling, hit testing.
use super::*;

impl App {
    /// Re-create the backdrop and every GPU object of the scene (settings / monitor / DPI change); widget instances stay.
    pub(super) unsafe fn rebuild(&mut self, keep_layout: bool) {
        self.abort_prompt(); // new geometry: the prompt is cancelled
        self.abort_menu(); // ... and the menu is closed
        let mon = monitor();
        let (mw, mh) = (mon.rect.right - mon.rect.left, mon.rect.bottom - mon.rect.top);
        let s = if keep_layout && same_rect(&mon.rect, &self.mon) { self.s } else { mon.scale };
        // another monitor's desk (its own layout; the primary one when the chosen monitor is unplugged): every card is replaced
        let new_desk = mon.slot != self.slot;
        if new_desk {
            self.slot = mon.slot.clone();
            layout::set_slot(mon.slot.clone());
            self.items = layout::items_for(&layout::read_config(), mon.slot.as_deref());
            self.clear_desk();
        }
        (self.ui.mon_count, self.ui.mon_sel) = (mon.count, mon.pos);
        self.backdrop = None; // stops the capture thread before a new duplication is requested
        self.mon = mon.rect;
        self.work = mon.work;
        let cs = s * self.ui.look.card_k;
        self.s = s;
        self.cs = cs;
        self.dims = grid_dims(&self.mon, &self.work, cs);
        let m = (MARGIN * cs).round() as i32;
        // card positions: from the grid again (a new monitor / scale); the stored layout is not rewritten
        let specs = place_items(&mut self.items, &self.dims, cs);
        // a card that lost its room (bigger cards, a smaller monitor) leaves the desk, the stored layout keeps it
        let gone: Vec<usize> = self.scene.cards.iter().filter(|c| !specs.iter().any(|sp| sp.id == c.id)).map(|c| c.uid).collect();
        for uid in gone {
            if let Some(i) = self.scene.index_of_uid(uid) {
                self.scene.remove_card(i);
            }
            self.pops.remove(&uid);
        }
        let mut rects = Vec::new();
        for c in &self.scene.cards {
            match specs.iter().find(|sp| sp.id == c.id) {
                Some(sp) => rects.push((sp.x, sp.y, sp.w, sp.h)),
                None => rects.push((c.x, c.y, c.w, c.h)),
            }
        }
        let card_rects: Vec<RECT> = rects.iter().map(|&(x, y, w, h)| RECT { left: x, top: y, right: x + w, bottom: y + h }).collect();
        let bd = make_backdrop(&self.gfx, &mon, mw, mh, self.settings.live, capture_rate(self.fps, &self.settings), &region_list(&card_rects, None, None), m, self.gfx_luid);
        bd.set_hwnd(self.hwnd);
        self.use_dupl = bd.name().starts_with("desktop");
        if let Err(e) = self.scene.reconfigure(&self.gfx, mw, mh, cs, bd.bitmap(), &rects) {
            eprintln!("mswidgets: rebuild: {e}");
        }
        self.scene.params.card_k = self.ui.look.card_k; // the applied card scale
        self.backdrop = Some(bd);
        // cards that got room again come back
        for sp in &specs {
            if self.scene.index_of_id(&sp.id).is_none() {
                self.spawn_card(sp, false);
            }
        }
        self.ui.set_screen(s, mw as f32 / s, mh as f32 / s);
        self.springs.clear();
        let r = self.desired_window();
        // the window moves with the monitor: force a new position and size
        self.win = RECT { left: -1, top: -1, right: 0, bottom: 0 };
        self.set_window(r);
        self.ui.sync_panels(&mut self.scene, &self.gfx);
        self.sync_regions();
        let all = self.all_mask();
        // same order as at start: one frame while the GPU is quiet, THEN the capture thread (a new backdrop does nothing until started)
        self.redraw(all | BIT_GALLERY | BIT_TOOLBAR, true);
        self.bd().set_paused(self.pause.any()); // the new capture inherits the sleep
        self.bd().start();
        if self.use_dupl && !self.pause.any() {
            let t = Instant::now();
            while self.bd().take_dirty() == 0 && t.elapsed() < Duration::from_millis(600) {
                std::thread::sleep(Duration::from_millis(15));
            }
        }
        self.redraw(all | BIT_GALLERY | BIT_TOOLBAR, true);
        let d = RECT { left: 0, top: 0, right: self.scene.w, bottom: self.scene.h };
        self.frame(d);
        self.update_tick_timer();
        if new_desk {
            self.pop_all_in();
        }
    }

    /// Display / DPI / work-area change, a monitor (un)plugged or a new `widgetsMonitor`: when the monitor the desk belongs on (see monitors.rs) is not
    /// the one we sit on, or its geometry / DPI changed, everything is re-placed. An unplugged monitor falls back to the primary one, and the desk
    /// returns when it comes back.
    pub(super) unsafe fn replace_monitor(&mut self) {
        // monitors came or went, changed resolution or became the primary one: the stored keys follow the hardware first
        if let Some(root) = layout::migrate_monitors(&monitors::enumerate()) {
            self.note_own_write();
            monitors::set_choice(monitors::Choice::from_value(&root));
        }
        let cur = monitor();
        let wa = cur.work.bottom == self.work.bottom && cur.work.top == self.work.top;
        let (count, pos) = (self.ui.mon_count, self.ui.mon_sel);
        (self.ui.mon_count, self.ui.mon_sel) = (cur.count, cur.pos);
        if same_rect(&cur.rect, &self.mon) && (cur.scale - self.s).abs() < 0.01 && wa && cur.slot == self.slot {
            if (count, pos) != (cur.count, cur.pos) && self.ui.settings {
                self.redraw_ui(); // a monitor came or went: the "Monitor" control has another number of segments
            }
            return;
        }
        if self.drag.is_some() {
            self.end_drag();
        }
        eprintln!("mswidgets: monitor / DPI / work area changed, re-placing the widgets");
        self.rebuild(false);
    }

    /// Every card leaves the scene (the stored layout stays): a new desk is about to be built from `items`.
    pub(super) unsafe fn clear_desk(&mut self) {
        let now = self.now();
        self.leave(&now);
        self.hot = None;
        self.pressed = None;
        self.press = Press::None;
        self.ui.hover = Hit::None;
        self.ui.press = Hit::None;
        while self.scene.len() > 0 {
            self.scene.remove_card(0);
        }
        self.pops.clear();
        self.springs.clear();
    }

    /// The freshly built desk condenses card by card (a plain appearance with reduced motion).
    pub(super) unsafe fn pop_all_in(&mut self) {
        if !self.reduced {
            let t = Instant::now();
            for i in 0..self.scene.len() {
                let uid = self.scene.cards[i].uid;
                self.pops.insert(uid, Pop { t0: t + Duration::from_millis(60 * i as u64), out: false });
                self.scene.set_xf(i, mat_xf(&motion::materialize(0.0, false, false), 0.0, 0.0));
            }
            self.last_anim = t;
            self.arm_anim();
        }
        if !self.ui.edit && !self.want_full() {
            SetTimer(self.hwnd, T_SHRINK, 400, None);
        }
    }

    /// Move the desk to the monitor at display position `pos` (settings panel / tray menu): this monitor's layout is written back to its own slot,
    /// the choice is stored in `widgetsMonitor` (the primary monitor as "primary", the others by device name) and the desk is rebuilt there with
    /// that monitor's layout (an empty desk the first time). Nothing changes when config.json cannot be written.
    pub(super) unsafe fn set_monitor(&mut self, pos: usize) {
        let list = monitors::enumerate();
        let Some(choice) = monitors::choice_for_position(&list, pos) else { return };
        if monitors::choice() == choice {
            return;
        }
        if self.drag.is_some() {
            self.end_drag();
        }
        if let Err(e) = layout::save_layout(&self.items).and_then(|_| layout::write_setting("widgetsMonitor", serde_json::json!(choice.key()))) {
            self.note_own_write();
            return self.notify(format!("{}: {e}", self.ui.lang.pick("Монитор не изменён", "Monitor not changed")));
        }
        self.note_own_write();
        monitors::set_choice(choice);
        self.replace_monitor();
        if self.ui.settings {
            self.redraw_ui();
        }
    }

    /// config.json changed behind our back (the settings of the Tauri app / a manual edit): glass, backdrop mode, dim.
    pub(super) unsafe fn poll_config(&mut self) {
        if !self.visible {
            return;
        }
        let m = layout::config_mtime();
        if m == self.cfg_mtime {
            return;
        }
        self.cfg_mtime = m;
        let new = layout::read_settings();
        let old = self.settings;
        self.settings = new;
        if new.dim != old.dim {
            self.update_dim();
        }
        self.boost_capture(); // any edit of config.json: look at the glass at full rate for a while
        if new.appearance != old.appearance {
            self.scene.set_appearance_pref(new.appearance);
            self.last_anim = Instant::now();
            self.arm_anim();
        }
        if (new.glass - old.glass).abs() > 1e-4 {
            self.apply_glass(new.glass);
        }
        self.ui.appearance = new.appearance;
        self.ui.dim_mode = new.dim;
        self.ui.check_start = new.check_updates;
        if new.accent != old.accent {
            self.ui.accent_mode = new.accent;
            self.refresh_accent(true);
        }
        if new.accent_scope != old.accent_scope {
            self.ui.accent_scope = new.accent_scope;
            self.apply_scope();
        }
        if new.live != old.live || new.capture_fps != old.capture_fps {
            self.ui.live = new.live;
            self.rebuild(true);
        }
        // `widgetsMonitor` edited behind our back: the desk moves (our own writes set the choice first, so they are not seen as a change)
        if let Some(root) = layout::try_read_config() {
            let choice = monitors::Choice::from_value(&root);
            if choice != monitors::choice() {
                monitors::set_choice(choice);
                self.replace_monitor();
            }
            self.scenes_reload(&root);
        }
        // `widgetsLook` edited behind our back (the sliders of the Tauri app / by hand)
        let look = layout::read_look();
        if look != self.look_cfg {
            self.look_cfg = look;
            self.ui.look = crate::glass::apply_look(&self.look_base, &self.look_cfg);
            self.apply_look_now();
            self.apply_card_scale();
        }
    }

    // ------------------------------------------------ click-through

    /// WS_EX_TRANSPARENT toggled from a cursor poll: 100 ms normally, 16 ms within 8 px of a card.
    pub(super) unsafe fn hit_test(&mut self) {
        if self.pause.has(autopause::COVERED) && self.interacting() {
            self.set_pause(autopause::COVERED, false); // edit mode / a menu opened behind a covering window
        }
        if !self.visible {
            return;
        }
        if self.want_full() {
            if !self.interactive {
                self.set_transparent(false);
            }
            return;
        }
        let mut p = POINT::default();
        if GetCursorPos(&mut p).is_err() {
            return;
        }
        let (mut inside, mut near) = (false, false);
        for c in &self.scene.cards {
            let r = RECT { left: c.x + self.mon.left, top: c.y + self.mon.top, right: c.x + c.w + self.mon.left, bottom: c.y + c.h + self.mon.top };
            inside |= p.x >= r.left && p.x < r.right && p.y >= r.top && p.y < r.bottom;
            near |= p.x >= r.left - NEAR_PX && p.x < r.right + NEAR_PX && p.y >= r.top - NEAR_PX && p.y < r.bottom + NEAR_PX;
        }
        let held = self.interactive && (GetAsyncKeyState(0x01) < 0 || GetAsyncKeyState(0x02) < 0);
        let want = inside || held;
        if want != self.interactive {
            if !want && self.pressed.is_none() {
                let now = self.now();
                self.leave(&now); // the cursor left the cards: clear hover states
            }
            self.set_transparent(!want);
        }
        if near != self.near {
            self.near = near;
            SetTimer(self.hwnd, T_HIT, if near { 16 } else { 100 }, None);
        }
    }

    pub(super) unsafe fn show(&mut self) {
        let _ = ShowWindow(self.hwnd, SW_SHOWNOACTIVATE);
        place_above_desktop(self.hwnd);
        self.visible = true;
        self.update_tick_timer();
    }
}
