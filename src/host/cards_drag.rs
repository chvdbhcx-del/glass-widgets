//! Cards on the desk: drag & drop with the spring settle, adding, removing and resizing, the context menu and its results.
use super::*;

impl App {
    pub(super) fn occupied(&self, except: Option<&str>) -> Vec<grid::Place> {
        self.items.iter().filter(|it| Some(it.id.as_str()) != except && self.scene.index_of_id(&it.id).is_some()).map(|it| grid::Place::new(it.size, it.col as i32, it.row as i32)).collect()
    }

    /// Lift card `uid` and let it follow the pointer. (x0, y0) = where the press happened, (x, y) = the pointer now.
    pub(super) unsafe fn begin_drag(&mut self, uid: usize, x0: i32, y0: i32, x: i32, y: i32, fresh: bool, from: Option<(i32, i32)>) {
        let Some(i) = self.scene.index_of_uid(uid) else { return };
        let c = &self.scene.cards[i];
        let off = if fresh { (c.w / 2, c.h / 2) } else { (x0 - c.x, y0 - c.y) };
        let from = from.or_else(|| self.items.iter().find(|it| it.id == c.id).map(|it| (it.col as i32, it.row as i32)));
        self.drag = Some(Drag { uid, off, cur: (x, y), target: None, fresh, from, vel: (0.0, 0.0) });
        self.boost_capture();
        self.press = Press::Drag;
        self.springs.remove(&uid);
        let mut xf = self.scene.cards[i].xf;
        xf.lift = xf.lift.max(0.02);
        self.scene.set_xf(i, xf);
        self.last_anim = Instant::now();
        let r = self.desired_window();
        self.set_window(r);
        self.set_transparent(false);
        SetCapture(self.hwnd);
        self.arm_anim();
        self.anim_step();
    }

    pub(super) unsafe fn drag_from_preview(&mut self, k: usize, x: i32, y: i32) {
        let Some((def, size)) = self.ui.item(k) else { return };
        if self.scene.len() >= MAX_CARDS {
            return self.show_toast(self.ui.lang.pick("Слишком много виджетов", "Too many widgets").to_string());
        }
        let (lw, lh) = size.logical();
        let (w, h) = ((lw * self.cs).round() as i32, (lh * self.cs).round() as i32);
        let id = layout::new_id_time(def.type_id, &self.items);
        let spec = CardSpec { id: id.clone(), type_id: def.type_id.to_string(), size, cfg: serde_json::json!({}), x: x - w / 2, y: y - h / 2, w, h };
        let Some(uid) = self.spawn_card(&spec, false) else { return };
        self.items.push(layout::Item { id, type_id: def.type_id.to_string(), size, col: 0, row: 0, cfg: serde_json::json!({}) });
        self.begin_drag(uid, x, y, x, y, true, None);
    }

    /// Build the card for `spec`, tick it once, measure its backdrop and render it. Returns its uid.
    pub(super) unsafe fn spawn_card(&mut self, spec: &CardSpec, pop: bool) -> Option<usize> {
        if self.scene.len() >= MAX_CARDS {
            return None;
        }
        let i = match self.scene.add_card(&self.gfx, spec) {
            Ok(i) => i,
            Err(e) => {
                eprintln!("mswidgets: add card '{}': {e}", spec.type_id);
                return None;
            }
        };
        let now = self.now();
        self.scene.tick_card(&self.gfx, i, &now);
        let _ = self.scene.measure_lum(&self.gfx, 1 << i, false);
        if let Err(e) = self.scene.render_card(&self.gfx, i, &now) {
            eprintln!("mswidgets: render card: {e}");
        }
        let uid = self.scene.cards[i].uid;
        if pop && !self.reduced {
            self.pops.insert(uid, Pop { t0: Instant::now(), out: false });
            self.scene.set_xf(i, mat_xf(&motion::materialize(0.0, false, false), 0.0, 0.0));
        }
        self.update_tick_timer();
        Some(uid)
    }

    /// Drop the dragged card: snap to the free cell (or go home / vanish), spring settle, persist.
    pub(super) unsafe fn end_drag(&mut self) {
        let Some(d) = self.drag.take() else { return };
        self.press = Press::None;
        self.pressed = None;
        let _ = ReleaseCapture();
        self.ui.placeholder = None;
        let Some(i) = self.scene.index_of_uid(d.uid) else { return };
        let (id, size) = (self.scene.cards[i].id.clone(), self.scene.cards[i].size);
        let landing = d.target.map(|p| (p.col, p.row)).or(d.from);
        let dropped_on_gallery = d.fresh && self.ui.gallery && self.ui.edit && {
            let (lx, ly) = (d.cur.0 as f32 / self.s, d.cur.1 as f32 / self.s);
            self.ui.panel_rect().contains(lx, ly)
        };
        match landing {
            Some((col, row)) if !dropped_on_gallery => {
                if let Some(it) = self.items.iter_mut().find(|x| x.id == id) {
                    it.col = col as u32;
                    it.row = row as u32;
                }
                let (x, y, _, _) = cell_rect(size, col, row, self.cs);
                let c = &self.scene.cards[i];
                self.springs.insert(d.uid, Spring { x: c.x as f32, y: c.y as f32, vx: 0.0, vy: 0.0, tx: x as f32, ty: y as f32 });
                self.save_layout();
                self.sync_regions();
            }
            _ => {
                // dropped on the gallery / nowhere free: a fresh card is discarded, a moved one stays where it was
                if d.fresh {
                    self.discard_card(d.uid);
                    if !dropped_on_gallery {
                        self.show_toast(self.ui.lang.pick("На рабочем столе нет свободного места", "No free space on the desktop").to_string());
                    }
                    return;
                }
            }
        }
        self.last_anim = Instant::now();
        self.arm_anim();
        self.anim_step();
    }

    /// Remove a card without animation (cancelled drag).
    pub(super) unsafe fn discard_card(&mut self, uid: usize) {
        let Some(i) = self.scene.index_of_uid(uid) else { return };
        let r = self.scene.layer_rect_ext(i, self.ext());
        let id = self.scene.cards[i].id.clone();
        self.scene.remove_card(i);
        self.items.retain(|x| x.id != id);
        self.springs.remove(&uid);
        self.pops.remove(&uid);
        self.sync_regions();
        self.frame(r);
        self.update_tick_timer();
        self.arm_anim();
    }

    // ------------------------------------------------ add / remove / resize

    /// Gallery click / drop: a new card at (want) or the first free cell.
    pub(super) unsafe fn add_widget(&mut self, type_id: &str, size: Size, want: Option<(i32, i32)>) {
        let Some(def) = crate::registry::global().get(type_id) else { return };
        let occ = self.occupied(None);
        let (c0, r0) = want.unwrap_or((0, 0));
        let Some(p) = grid::find_free(&self.dims, &occ, size, c0, r0) else {
            return self.show_toast(self.ui.lang.pick("На рабочем столе нет свободного места", "No free space on the desktop").to_string());
        };
        let id = layout::new_id_time(def.type_id, &self.items);
        let (x, y, w, h) = cell_rect(size, p.col, p.row, self.cs);
        let spec = CardSpec { id: id.clone(), type_id: def.type_id.to_string(), size, cfg: serde_json::json!({}), x, y, w, h };
        let Some(uid) = self.spawn_card(&spec, true) else { return };
        self.items.push(layout::Item { id, type_id: def.type_id.to_string(), size, col: p.col as u32, row: p.row as u32, cfg: serde_json::json!({}) });
        self.save_layout();
        self.sync_regions();
        self.last_anim = Instant::now();
        self.arm_anim();
        if let Some(i) = self.scene.index_of_uid(uid) {
            let r = self.scene.layer_rect_ext(i, self.ext());
            self.frame(r);
        }
    }

    /// "−" / menu "Удалить": pop out, then forget the card.
    pub(super) unsafe fn remove_card(&mut self, uid: usize) {
        if self.reduced {
            self.finish_remove(uid);
            return;
        }
        self.pops.insert(uid, Pop { t0: Instant::now(), out: true });
        self.last_anim = Instant::now();
        self.arm_anim();
    }

    pub(super) unsafe fn finish_remove(&mut self, uid: usize) {
        self.discard_card(uid);
        self.save_layout();
        if !self.want_full() {
            SetTimer(self.hwnd, T_SHRINK, 150, None);
        }
    }

    pub(super) unsafe fn change_size(&mut self, uid: usize, size: Size) {
        let Some(i) = self.scene.index_of_uid(uid) else { return };
        let id = self.scene.cards[i].id.clone();
        let Some(me) = self.items.iter().find(|x| x.id == id).cloned() else { return };
        if me.size == size {
            return;
        }
        let occ = self.occupied(Some(&id));
        let Some(p) = grid::resize(&self.dims, &occ, size, me.col as i32, me.row as i32) else {
            return self.show_toast(self.ui.lang.pick("Нет места для этого размера", "No room for this size").to_string());
        };
        let old = self.scene.layer_rect_ext(i, self.ext());
        let (x, y, _, _) = cell_rect(size, p.col, p.row, self.cs);
        // the glass springs to the new size (a plain jump with "animation effects" off)
        if let Err(e) = self.scene.resize_card(&self.gfx, i, size, x, y, !self.reduced) {
            eprintln!("mswidgets: resize: {e}");
            return;
        }
        let now = self.now();
        self.scene.tick_card(&self.gfx, i, &now);
        let _ = self.scene.measure_lum(&self.gfx, 1 << i, false);
        let _ = self.scene.render_card(&self.gfx, i, &now);
        if let Some(it) = self.items.iter_mut().find(|x| x.id == id) {
            it.size = size;
            it.col = p.col as u32;
            it.row = p.row as u32;
        }
        self.pops.remove(&uid); // a pending condense of the old size is moot
        self.save_layout();
        self.sync_regions();
        self.update_tick_timer();
        let d = union(old, self.scene.layer_rect_ext(i, self.ext()));
        // the window may have to grow to hold the new size
        let r = self.desired_window();
        let w = if self.want_full() { r } else { union(self.win, r) };
        self.set_window(w);
        self.last_anim = Instant::now();
        self.arm_anim();
        self.frame(d);
        if !self.want_full() {
            SetTimer(self.hwnd, T_SHRINK, 400, None);
        }
    }

    // ------------------------------------------------ context menu glue

    pub(super) unsafe fn menu_request(&mut self, uid: usize, lp: isize) -> Option<MenuReq> {
        let i = self.scene.index_of_uid(uid)?;
        let c = &self.scene.cards[i];
        let def = crate::registry::global().get(&c.type_id)?;
        let (x, y) = unpack(lp);
        let click = menu::click_item(self.ui.lang, &c.type_id, &crate::sys::targets::Click::from_cfg(&c.cfg));
        let opt = menu::option_item(self.ui.lang, &c.type_id, &c.cfg);
        Some(MenuReq { uid, x, y, sizes: def.sizes.to_vec(), cur: c.size, editing: self.ui.edit, lang: self.ui.lang, click, opt })
    }

    /// A "Click action" menu command for card `uid`: store the choice in the card's cfg (persisted like any widget setting), or ask for a
    /// custom target.
    pub(super) unsafe fn click_choice(&mut self, uid: usize, cmd: usize) {
        use crate::sys::targets::{presets, Click};
        let Some(i) = self.scene.index_of_uid(uid) else { return };
        let widget = self.scene.cards[i].type_id.clone();
        let choice = match menu::click_pick(cmd) {
            Some(ClickPick::Default) => Click::Default,
            Some(ClickPick::Nothing) => Click::Nothing,
            Some(ClickPick::Preset(k)) => match presets(&widget).get(k) {
                Some(p) => Click::Preset(p.key.to_string()),
                None => return,
            },
            Some(ClickPick::Custom) => {
                let cur = match Click::from_cfg(&self.scene.cards[i].cfg) {
                    Click::Custom(v) => v,
                    _ => String::new(),
                };
                return self.ask_click_custom(uid, cur);
            }
            None => return,
        };
        self.set_click(uid, choice);
    }

    /// The card's on/off menu option: flip its cfg bool (persisted like any widget setting; the widget reads it on its next tick).
    pub(super) unsafe fn toggle_option(&mut self, uid: usize) {
        let Some(i) = self.scene.index_of_uid(uid) else { return };
        let Some((key, default, ..)) = menu::card_option(&self.scene.cards[i].type_id) else { return };
        self.scene.touch_cfg(i, |cfg| {
            let on = cfg.get(key).and_then(|v| v.as_bool()).unwrap_or(default);
            if !cfg.is_object() {
                *cfg = serde_json::json!({});
            }
            cfg[key] = serde_json::json!(!on);
        });
        let now = self.now();
        if let Some(r) = self.settle(i, &now, false) {
            self.frame(r);
        }
    }

    pub(super) unsafe fn set_click(&mut self, uid: usize, choice: crate::sys::targets::Click) {
        let Some(i) = self.scene.index_of_uid(uid) else { return };
        self.scene.touch_cfg(i, |cfg| choice.store(cfg));
        let now = self.now();
        if let Some(r) = self.settle(i, &now, false) {
            self.frame(r);
        }
    }

    /// "Custom…": the prompt runs on its own thread (it has a message loop) and asks again until the value passes `parse_custom` or the
    /// user cancels; a good value comes back as `WM_CLICK_CUSTOM`.
    pub(super) unsafe fn ask_click_custom(&mut self, uid: usize, initial: String) {
        let (lang, hwnd) = (self.ui.lang, self.hwnd.0 as isize);
        let _ = std::thread::Builder::new().name("click-custom".into()).spawn(move || {
            let mut text = initial;
            let mut bad = false;
            loop {
                let title = if bad { lang.pick("Не подходит. Нужен адрес или файл", "Not allowed. Use a web address or a file") } else { lang.pick("Что открывать по клику", "What to open on click") };
                let mut o = crate::input::PromptOpts::new(title, &text, false);
                (o.placeholder, o.ok_label, o.lang) = (lang.pick("https://…, ms-settings:…, shell:… или C:\\путь", "https://…, ms-settings:…, shell:… or C:\\path").to_string(), lang.pick("Сохранить", "Save").to_string(), lang);
                let Some(raw) = crate::input::prompt_ex(o) else { return };
                if let Some(v) = crate::sys::targets::parse_custom(&raw, |p| std::path::Path::new(p).exists()) {
                    if let Ok(mut g) = CLICK_CUSTOM.lock() {
                        *g = Some((uid, v));
                        let _ = PostMessageW(HWND(hwnd as *mut _), WM_CLICK_CUSTOM, WPARAM(0), LPARAM(0));
                    }
                    return;
                }
                (text, bad) = (raw, true);
            }
        });
    }

    pub(super) unsafe fn click_custom_named(&mut self) {
        if let Some((uid, v)) = CLICK_CUSTOM.lock().ok().and_then(|mut g| g.take()) {
            self.set_click(uid, crate::sys::targets::Click::Custom(v));
        }
    }

    pub(super) unsafe fn menu_result(&mut self, req: &MenuReq, cmd: usize) {
        self.in_menu = false;
        match cmd {
            CMD_EDIT => {
                if self.editing {
                    self.leave_edit();
                } else {
                    self.enter_edit();
                }
            }
            c if (CMD_SIZE..CMD_SIZE + 3).contains(&c) => self.change_size(req.uid, Size::ALL[c - CMD_SIZE]),
            CMD_DELETE => self.remove_card(req.uid),
            CMD_CARD_OPT => self.toggle_option(req.uid),
            c if menu::click_pick(c).is_some() => self.click_choice(req.uid, c),
            _ => {}
        }
        if !self.ui.edit {
            place_above_desktop(self.hwnd);
            if !self.want_full() {
                SetTimer(self.hwnd, T_SHRINK, 150, None);
            }
        }
    }

    // ------------------------------------------------ animation
}
