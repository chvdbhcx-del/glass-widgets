//! Settings / gallery / toast animation state of the UI: slider and switch tracks, panel fades, toasts.
use super::*;

impl Ui {
    /// The settings panel opens: everything is drawn at its real value at once, no glide from stale numbers.
    pub fn sa_reset(&mut self) {
        self.sa = SetAnim::new();
        for i in 0..1 + LOOK.len() {
            self.sa.kp[i] = Some(self.set_pos(i));
        }
        for o in Opt::ALL {
            self.sa.seg[opt_idx(o)] = Some(self.opt_sel(o) as f32);
        }
        self.sa.sw = [Some(f32::from(self.autostart)), Some(f32::from(self.check_start))];
        self.sa.btns = if matches!(self.update, UpdateState::Available { .. }) { 1.0 } else { 0.0 };
        self.sa.st_key = upd_key(&self.update);
        self.sa.st_last = self.upd_line();
    }

    /// Remember the status line as shown (the line a later change fades from; a download's percent moves without a step).
    pub fn sa_track(&mut self) {
        if upd_key(&self.update) == self.sa.st_key {
            self.sa.st_last = self.upd_line();
        }
    }

    /// A press on look slider `i` at canvas px `x`, the value under the pointer `t`: on the handle it follows 1:1 at once, anywhere
    /// else the handle glides to the spot until the pointer moves (a few px) and then follows 1:1.
    pub fn sa_drag_start(&mut self, i: usize, x: i32, t: f32) {
        let tw = self.set_lay().rows.get(i).map_or(1.0, |r| r.track.w);
        let shown = self.slider_shown(i);
        self.sa.drag = Some((i, x));
        self.sa.snapped = (t - shown).abs() * tw <= 12.0;
    }

    /// The pointer moved with slider `i` pressed.
    pub fn sa_drag_move(&mut self, i: usize, x: i32) {
        if let Some((j, x0)) = self.sa.drag {
            if j == i && (x - x0).abs() as f32 > 4.0 * self.s {
                self.sa.snapped = true;
            }
        }
    }

    pub fn sa_drag_end(&mut self) {
        if let (true, Some((i, _))) = (self.sa.snapped, self.sa.drag) {
            self.sa.kp[i] = Some(self.set_pos(i)); // the handle stays where the drag left it
        }
        self.sa.drag = None;
        self.sa.snapped = false;
    }

    /// Position of the handle of look slider `i` as drawn.
    pub(super) fn slider_shown(&self, i: usize) -> f32 {
        if self.sa.snapped && self.sa.drag.is_some_and(|d| d.0 == i) {
            return self.set_pos(i);
        }
        self.sa.kp.get(i).copied().flatten().unwrap_or_else(|| self.set_pos(i))
    }

    /// Where a segmented control's pill is drawn, in segments.
    pub(super) fn seg_shown(&self, o: Opt) -> f32 {
        self.sa.seg[opt_idx(o)].unwrap_or_else(|| self.opt_sel(o) as f32)
    }

    pub(super) fn sw_shown(&self, j: usize, on: bool) -> f32 {
        self.sa.sw[j].unwrap_or(if on { 1.0 } else { 0.0 })
    }

    /// The status line of the update check: text and colour.
    pub(super) fn upd_line(&self) -> (String, Color) {
        match &self.update {
            UpdateState::Idle => (format!("{} {}", self.t("Версия", "Version"), env!("CARGO_PKG_VERSION")), FAINT),
            UpdateState::Checking => (self.t("Проверка…", "Checking…").to_string(), DIM),
            UpdateState::Latest => (self.t("У вас последняя версия", "You have the latest version").to_string(), Color::rgb8(52, 199, 89)),
            UpdateState::Available { error: Some(e), .. } => (if self.lang == Lang::Ru { format!("Не удалось обновить: {e}") } else { format!("Update failed: {e}") }, ORANGE),
            UpdateState::Available { version, .. } => (if self.lang == Lang::Ru { format!("Доступна версия {version}") } else { format!("Version {version} is available") }, TEXT),
            UpdateState::Downloading(p) => (if self.lang == Lang::Ru { format!("Загрузка обновления… {p}%") } else { format!("Downloading the update… {p}%") }, TEXT),
            UpdateState::Verifying => (self.t("Проверка контрольной суммы…", "Verifying the checksum…").to_string(), TEXT),
            UpdateState::Failed => (self.t("Не удалось проверить", "Could not check").to_string(), ORANGE),
        }
    }

    /// Hovered key and pressed key of the settings panel.
    pub(super) fn sa_keys(&self) -> (Option<usize>, Option<usize>) {
        (hkey(self.hover), hkey(self.press))
    }

    /// The panel's controls still have to move (the animation timer keeps running).
    pub fn sa_busy(&self) -> bool {
        if !(self.edit && self.settings) {
            return false;
        }
        let (hk, pk) = self.sa_keys();
        if self.sa.hov.busy(|i| Some(i) == hk || Some(i) == pk) {
            return true;
        }
        if self.sa.prs.iter().enumerate().any(|(i, b)| !(K_OPT..K_AUTO).contains(&i) && !b.at_rest(if Some(i) == pk { PRESS_BTN } else { 1.0 })) {
            return true;
        }
        let n = 1 + LOOK.len();
        (0..n).any(|i| self.sa.kp[i] != Some(self.set_pos(i)))
            || Opt::ALL.iter().any(|o| self.sa.seg[opt_idx(*o)] != Some(self.opt_sel(*o) as f32))
            || [self.autostart, self.check_start].iter().enumerate().any(|(j, on)| self.sa.sw[j] != Some(if *on { 1.0 } else { 0.0 }))
            || self.sa.btns != if matches!(self.update, UpdateState::Available { .. }) { 1.0 } else { 0.0 }
            || upd_key(&self.update) != self.sa.st_key
            || self.sa.st_t < 1.0
    }

    /// Dirty rectangle (logical) of animation key `k` (generous: the shadow and the overshoot of a pressed button included).
    pub(super) fn sa_rect(&self, l: &SetLay, k: usize) -> Option<Rect> {
        Some(match k {
            _ if k < K_RESET => l.rows.get(k)?.row.inset_xy(-14.0, -2.0),
            K_RESET => l.reset.inset(-5.0),
            _ if k < K_AUTO => l.opts.get(k.checked_sub(K_OPT)? / MON_SEGS)?.seg.inset(-5.0),
            K_AUTO => l.auto_row.inset(-4.0),
            K_CHECK => l.check_row.inset(-4.0),
            K_EDIT => l.edit_btn.inset(-5.0),
            K_QUIT => l.quit_btn.inset(-5.0),
            K_UPD => l.upd_btn.inset(-5.0),
            K_DL => l.upd_dl.inset(-5.0),
            K_PAGE => l.upd_page.inset(-5.0),
            K_DONE => l.done.inset(-5.0),
            K_STATUS => l.upd_status,
            _ => return None,
        })
    }

    /// One step of every moving control of the settings panel (`reduced`: straight to the end values). Returns the canvas px rectangle
    /// that has to be repainted: the controls that moved, not the whole panel.
    pub fn step_sa(&mut self, dt: f32, reduced: bool) -> Option<RECT> {
        if !(self.edit && self.settings) {
            return None;
        }
        let l = self.set_lay();
        let (hk, pk) = self.sa_keys();
        let hot = |i: usize| Some(i) == hk || Some(i) == pk;
        let mut ch: u128 = 0; // bit k = key k changed
        if reduced {
            self.sa.hov.snap(hot, |i| ch |= 1 << i);
        } else {
            self.sa.hov.step(dt, HOV_IN, HOV_OUT, hot, |i| ch |= 1 << i);
        }
        for (i, b) in self.sa.prs.iter_mut().enumerate() {
            if (K_OPT..K_AUTO).contains(&i) {
                continue;
            }
            let t = if Some(i) == pk { PRESS_BTN } else { 1.0 };
            if b.at_rest(t) {
                continue;
            }
            if reduced {
                *b = Bump::resting(t);
            } else {
                b.step(dt, t);
            }
            ch |= 1 << i;
        }
        for i in 0..1 + LOOK.len() {
            let t = self.set_pos(i);
            let drag = self.sa.snapped && self.sa.drag.is_some_and(|d| d.0 == i);
            let new = match self.sa.kp[i] {
                Some(c) if !reduced && !drag => motion::follow(c, t, dt, SLIDE_TAU, 0.0005),
                _ => t,
            };
            if self.sa.kp[i].is_some_and(|c| c != new) {
                ch |= 1 << i;
            }
            self.sa.kp[i] = Some(new);
        }
        for o in Opt::ALL {
            let (j, t) = (opt_idx(o), self.opt_sel(o) as f32);
            let new = match self.sa.seg[j] {
                Some(c) if !reduced => motion::follow(c, t, dt, SEG_TAU, 0.003),
                _ => t,
            };
            if self.sa.seg[j].is_some_and(|c| c != new) {
                ch |= 1 << (K_OPT + j * MON_SEGS);
            }
            self.sa.seg[j] = Some(new);
        }
        for (j, on) in [self.autostart, self.check_start].into_iter().enumerate() {
            let new = match self.sa.sw[j] {
                Some(c) if !reduced => motion::ramp(c, on, dt, SW_T, SW_T),
                _ => {
                    if on {
                        1.0
                    } else {
                        0.0
                    }
                }
            };
            if self.sa.sw[j].is_some_and(|c| c != new) {
                ch |= 1 << (K_AUTO + j);
            }
            self.sa.sw[j] = Some(new);
        }
        let avail = matches!(self.update, UpdateState::Available { .. });
        let b = if reduced { f32::from(avail) } else { motion::ramp(self.sa.btns, avail, dt, FADE_T, FADE_T) };
        if b != self.sa.btns {
            self.sa.btns = b;
            ch |= (1 << K_DL) | (1 << K_PAGE);
        }
        let key = upd_key(&self.update);
        if key != self.sa.st_key {
            self.sa.st_key = key;
            self.sa.st_prev = (!reduced).then(|| self.sa.st_last.clone());
            self.sa.st_t = if reduced { 1.0 } else { 0.0 };
            ch |= 1 << K_STATUS;
        } else if self.sa.st_t < 1.0 {
            self.sa.st_t = (self.sa.st_t + dt / FADE_T).min(1.0);
            if self.sa.st_t >= 1.0 {
                self.sa.st_prev = None;
            }
            ch |= 1 << K_STATUS;
        }
        self.sa.st_last = self.upd_line();
        if ch == 0 {
            return None;
        }
        let mut u: Option<Rect> = None;
        for k in 0..=N_KEYS {
            if ch >> k & 1 == 1 {
                if let Some(r) = self.sa_rect(&l, k) {
                    u = Some(u.map_or(r, |a| {
                        let (x, y) = (a.x.min(r.x), a.y.min(r.y));
                        Rect::new(x, y, a.right().max(r.right()) - x, a.bottom().max(r.bottom()) - y)
                    }));
                }
            }
        }
        let (s, m) = (self.s, 2);
        u.map(|r| RECT { left: (r.x * s).floor() as i32 - m, top: (r.y * s).floor() as i32 - m, right: (r.right() * s).ceil() as i32 + m, bottom: (r.bottom() * s).ceil() as i32 + m })
    }

    // ------------------------------------------------ gallery tiles

    /// The gallery shows another list of previews (group, size chip, search): restart the staggered fade-in. Same list = no restart.
    pub fn gal_note(&mut self) {
        use std::hash::{Hash, Hasher};
        let (defs, _) = self.shown();
        let (_, size) = self.chips_and_size(&defs);
        let mut h = std::collections::hash_map::DefaultHasher::new();
        defs.iter().for_each(|d| d.type_id.hash(&mut h));
        (size as u8).hash(&mut h);
        let sig = h.finish() | 1;
        if self.gal_sig != 0 && sig != self.gal_sig {
            self.gal_fade = 0.0;
        }
        self.gal_sig = sig;
    }

    /// Opacity of the preview tile `k` of the list: 20 ms later per tile (the first ten), 180 ms each.
    pub(super) fn tile_alpha(&self, k: usize) -> f32 {
        motion::smooth((self.gal_fade - GAL_STAGGER * k.min(10) as f32) / GAL_FADE)
    }

    pub fn gal_busy(&self) -> bool {
        self.edit && self.gallery && !self.settings && self.gal_fade < GAL_FADE + GAL_STAGGER * 10.0
    }

    /// One step of the tile fade; the canvas px rectangle of the tile area to repaint. Reduced motion: no fade at all.
    pub fn step_gal(&mut self, dt: f32, reduced: bool) -> Option<RECT> {
        if !self.gal_busy() {
            return None;
        }
        self.gal_fade = if reduced { 1.0e3 } else { self.gal_fade + dt };
        let (g, s) = (self.lay().grid, self.s);
        Some(RECT { left: (g.x * s).floor() as i32 - 2, top: (g.y * s).floor() as i32 - 2, right: (g.right() * s).ceil() as i32 + 2, bottom: (g.bottom() * s).ceil() as i32 + 2 })
    }

    // ------------------------------------------------ toast

    /// A toast appears (or its text changes while it is up: no second rise). Reduced motion shows it at once.
    pub fn toast_show(&mut self, msg: String, reduced: bool) {
        if self.toast.is_none() || self.toast_out.is_some() {
            // fresh, or coming back from a fade-out: continue from the opacity it has now
            self.toast_t = self.toast_out.map_or(0.0, |o| motion::TOAST_IN * (1.0 - o / motion::TOAST_OUT).clamp(0.0, 1.0));
        }
        self.toast_out = None;
        self.toast = Some(msg);
        let (a, r, _) = motion::toast_look(self.toast_t, None, reduced);
        (self.toast_a, self.toast_rise) = (a, r);
    }

    /// Its hold time is over: fade out (reduced motion: drop it on the next step).
    pub fn toast_fade(&mut self) {
        if self.toast.is_some() && self.toast_out.is_none() {
            self.toast_out = Some(0.0);
        }
    }

    pub fn toast_busy(&self, reduced: bool) -> bool {
        self.toast.is_some() && (self.toast_out.is_some() || if reduced { self.toast_a != 1.0 } else { self.toast_t < motion::TOAST_IN })
    }

    /// One step of the toast: (something to repaint, the toast has just been dropped).
    pub fn toast_step(&mut self, dt: f32, reduced: bool) -> (bool, bool) {
        if self.toast.is_none() {
            return (false, false);
        }
        self.toast_t += dt;
        if let Some(o) = self.toast_out.as_mut() {
            *o += dt;
        }
        let (a, r, done) = motion::toast_look(self.toast_t, self.toast_out, reduced);
        let changed = a != self.toast_a || r != self.toast_rise;
        (self.toast_a, self.toast_rise) = (a, r);
        if done {
            self.toast = None;
            self.toast_out = None;
            self.toast_a = 0.0;
        }
        (changed || done, done)
    }
}
