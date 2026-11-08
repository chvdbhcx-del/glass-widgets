//! Accent scope and colour, notifications and layout profiles.
use super::*;

impl App {
    /// `accentScope`: "everywhere" tints all content of every card with the accent (Apple's Tinted style); "main" draws it as the widgets do.
    pub(super) unsafe fn apply_scope(&mut self) {
        crate::widget::set_tint(self.settings.accent_scope == AccentScope::All);
        self.scene.invalidate_all();
        let now = self.now();
        for i in 0..self.scene.len() {
            let _ = self.scene.render_card(&self.gfx, i, &now);
        }
        let d = self.all_dirty();
        self.frame(d);
    }

    /// `accentMode`: take the accent from the wallpaper (again when the wallpaper file changed; `force` = the mode itself changed) or go back to blue,
    /// and record every card again when the colour is different. Runs on the 2 s config poll: one SystemParametersInfo call while nothing changed.
    pub(super) unsafe fn refresh_accent(&mut self, force: bool) {
        if !self.visible && !force {
            return;
        }
        let rgb = if self.settings.accent == AccentMode::Wallpaper {
            let key = crate::accent::wallpaper_key();
            // what is on the screen: the captured desktop (a dynamic / slideshow wallpaper included); the wallpaper file only without a capture
            let live = if self.settings.live && (force || self.accent_last.elapsed() >= Duration::from_secs(8)) {
                // the throttle starts only when a capture really existed: a failed read is retried at the next poll
                self.scene.backdrop_colors(&self.gfx).ok().map(|px| {
                    self.accent_last = Instant::now();
                    crate::accent::pick_or_neutral(&px)
                })
            } else {
                None
            };
            let rgb = match live {
                Some(p) => match (p, self.accent_cur) {
                    // close to what is shown already: keep it (a video wallpaper must not make the accent flicker)
                    (Some(n), Some(o)) if !force && n.iter().zip(o).map(|(a, b)| (*a as i32 - b as i32).abs()).sum::<i32>() < 60 => Some(o),
                    (p, _) => p,
                },
                None if self.settings.live => {
                    if !force {
                        return;
                    }
                    self.accent_cur
                }
                None => {
                    if !force && key == self.accent_key {
                        return;
                    }
                    key.as_ref().and_then(|k| crate::accent::from_file(&k.0))
                }
            };
            self.accent_key = key;
            rgb
        } else {
            self.accent_key = None;
            None
        };
        self.accent_cur = rgb;
        if self.settings.live {
            if let Some(c) = rgb {
                crate::accent::save_cache(c); // the next start begins with this colour instead of the wallpaper file's
            }
        }
        if crate::widget::set_accent(rgb) {
            self.scene.invalidate_all();
            let now = self.now();
            for i in 0..self.scene.len() {
                let _ = self.scene.render_card(&self.gfx, i, &now);
            }
            let d = self.all_dirty();
            self.frame(d);
        }
    }

    /// A message for the user: a toast while the panels are up (the toast lives on the whole-monitor canvas), else the log.
    pub(super) unsafe fn notify(&mut self, msg: String) {
        if self.ui.edit {
            self.show_toast(msg);
        } else {
            eprintln!("mswidgets: {msg}");
        }
    }

    /// Tray "Layouts" > profile `i` (index into the sorted list the menu showed): the current desk goes back into the active profile, the chosen one is loaded.
    pub(super) unsafe fn switch_profile(&mut self, i: usize) {
        let names = layout::profile_names(&layout::read_config());
        let Some(name) = names.get(i).cloned() else { return };
        self.switch_profile_named(&name);
    }

    /// Load profile `name` (the tray's choice and the scenes' automatic switch share this path).
    pub(super) unsafe fn switch_profile_named(&mut self, name: &str) {
        if self.drag.is_some() {
            self.end_drag();
        }
        let stash = self.ui.lang.pick("Без имени", "Unsaved");
        if let Err(e) = layout::profile_op(|r| layout::profile_switch(r, name, stash)) {
            return self.notify(format!("{}: {e}", self.ui.lang.pick("Макет не загружен", "Layout not loaded")));
        }
        self.note_own_write();
        // the new desk: items + look from the file, every card replaced (a card of the same id may have another size / cfg in the new layout)
        let root = layout::read_config();
        self.items = layout::items_for(&root, self.slot.as_deref()); // the active monitor's layout
        self.look_cfg = layout::look_from_value(&root);
        self.ui.look = crate::glass::apply_look(&self.look_base, &self.look_cfg);
        self.clear_desk();
        self.apply_look_now(); // the glass parameters first: the new cards are built with them
        self.rebuild(true); // also the card scale of the new look: a new grid, the cards come back from `items`
        self.pop_all_in();
    }

    /// Tray "Save current layout as...": the prompt runs on its own thread (it has a message loop); the name comes back as `WM_PROFILE_NAME`.
    pub(super) unsafe fn ask_profile_name(&mut self) {
        let (lang, hwnd) = (self.ui.lang, self.hwnd.0 as isize);
        let _ = std::thread::Builder::new().name("profile-name".into()).spawn(move || {
            let mut o = crate::input::PromptOpts::new(lang.pick("Название макета", "Layout name"), "", false);
            (o.placeholder, o.ok_label, o.lang) = (lang.pick("Например: Работа", "e.g. Work").to_string(), lang.pick("Сохранить", "Save").to_string(), lang);
            if let Some(name) = crate::input::prompt_ex(o) {
                if let Ok(mut g) = PROFILE_NAME.lock() {
                    *g = Some(name);
                    let _ = PostMessageW(HWND(hwnd as *mut _), WM_PROFILE_NAME, WPARAM(0), LPARAM(0));
                }
            }
        });
    }

    /// The prompt answered: the desk (cards + look) is saved as that profile and becomes the active one.
    pub(super) unsafe fn profile_named(&mut self) {
        let Some(name) = PROFILE_NAME.lock().ok().and_then(|mut g| g.take()) else { return };
        let name: String = name.trim().chars().take(PROFILE_NAME_MAX).collect();
        if name.is_empty() {
            return;
        }
        match layout::profile_op(|r| layout::profile_save(r, &name)) {
            Ok(()) => {
                self.note_own_write();
                let msg = format!("{} «{name}»", self.ui.lang.pick("Макет сохранён:", "Layout saved:"));
                self.notify(msg);
            }
            Err(e) => self.notify(format!("{}: {e}", self.ui.lang.pick("Макет не сохранён", "Layout not saved"))),
        }
    }

    /// Tray "Delete current layout": the active profile is forgotten, the desk stays as it is.
    pub(super) unsafe fn delete_profile(&mut self) {
        let mut gone = String::new();
        match layout::profile_op(|r| layout::profile_delete_active(r).map(|n| gone = n)) {
            Ok(()) => {
                self.note_own_write();
                let msg = format!("{} «{gone}»", self.ui.lang.pick("Макет удалён:", "Layout deleted:"));
                self.notify(msg);
            }
            Err(e) => self.notify(format!("{}: {e}", self.ui.lang.pick("Макет не удалён", "Layout not deleted"))),
        }
    }
}
