//! The animation clock: the schedule timer and the per-frame step (jiggle, springs, materials, toasts).
use super::*;

impl App {
    pub(super) fn anim_busy(&self) -> bool {
        let want_gal = if self.editing { 1.0 } else { 0.0 };
        self.drag.is_some()
            || !self.springs.is_empty()
            || !self.pops.is_empty()
            || (self.gal_t - want_gal).abs() > 1e-3
            || (self.dim_t - want_gal).abs() > 1e-3
            || self.scene.cards.iter().any(|c| c.xf.lift > 0.0)
            || self.scene.fx_busy()
            || self.ui.away > 0.0
            || self.jig_amp > 0.0 && !(self.ui.edit && !self.ui.settings && self.editing && !self.reduced)
            || self.jig_amp < 1.0 && self.ui.edit && !self.ui.settings && self.editing && !self.reduced
            || self.ui.sa_busy()
            || self.ui.gal_busy()
            || self.ui.toast_busy(self.reduced)
    }

    /// One timer drives every animation: 16 ms while something moves, 33 ms for the jiggle of a resting edit mode, off otherwise.
    pub(super) unsafe fn arm_anim(&mut self) {
        let period = if self.anim_busy() {
            16
        } else if self.ui.edit && !self.ui.settings && !self.reduced {
            33 // the jiggle period is 0.8 s: 100 ms steps moved a big card's edge by 2-3 px per frame
        } else {
            0
        };
        if period != self.anim_period {
            if self.anim_period == 0 {
                self.last_anim = Instant::now(); // the first step of a fresh run is one period long, not the whole idle time
            }
            self.anim_period = period;
            if period == 0 {
                let _ = KillTimer(self.hwnd, T_ANIM);
            } else {
                SetTimer(self.hwnd, T_ANIM, period, None);
            }
        }
    }

    pub(super) unsafe fn anim_step(&mut self) {
        let t = Instant::now();
        let dt = t.saturating_duration_since(self.last_anim).as_secs_f32().clamp(0.004, 0.05);
        self.last_anim = t;
        let ext = self.ext();
        let mut dirty: Option<RECT> = None;
        let add = |d: &mut Option<RECT>, r: RECT| *d = Some(d.map_or(r, |x| union(x, r)));

        // gallery / dim progress
        let want = if self.editing { 1.0 } else { 0.0 };
        if self.reduced {
            self.gal_t = want;
            self.dim_t = want;
        } else {
            let rate = if self.editing { 1.0 / 0.34 } else { 1.0 / 0.2 };
            self.gal_t = step_toward(self.gal_t, want, rate * dt);
            self.dim_t = step_toward(self.dim_t, want, dt / 0.25);
        }
        let eased = 1.0 - (1.0 - self.gal_t).powi(3);
        if (self.ui.anim - eased).abs() > 1e-4 {
            self.ui.anim = eased;
            if let Some(u) = self.ui.dirty_rect(&self.scene) {
                add(&mut dirty, u);
            }
        }
        // the dim covers the whole monitor: it fades in 6 steps (each one a full-monitor compose + present)
        let dim = (if self.ui.settings { ui::SETTINGS_DIM } else { DIM_ALPHA }) * (self.dim_t * 6.0).round() / 6.0;
        if (self.ui.dim - dim).abs() > 1e-4 {
            self.ui.dim = dim;
            add(&mut dirty, RECT { left: 0, top: 0, right: self.scene.w, bottom: self.scene.h });
        }

        // the dragged card follows the pointer
        let now0 = self.now();
        if let Some(d) = self.drag.as_mut() {
            if let Some(i) = self.scene.cards.iter().position(|c| c.uid == d.uid) {
                let c = &self.scene.cards[i];
                let old = self.scene.layer_rect_ext(i, ext);
                let nx = (d.cur.0 - d.off.0).clamp(-c.w / 2, self.scene.w - c.w / 2);
                let ny = (d.cur.1 - d.off.1).clamp(-c.h / 2, self.scene.h - c.h / 2);
                let (size, id) = (c.size, c.id.clone());
                let occ = self.items.iter().filter(|it| it.id != id && self.scene.index_of_id(&it.id).is_some()).map(|it| grid::Place::new(it.size, it.col as i32, it.row as i32)).collect::<Vec<_>>();
                let target = grid::target_for(&self.dims, &occ, size, nx, ny, self.cs);
                let ph_old = self.ui.placeholder;
                self.ui.placeholder = target.map(|p| {
                    let (x, y, w, h) = cell_rect(size, p.col, p.row, self.cs);
                    (RECT { left: x, top: y, right: x + w, bottom: y + h }, self.scene.radius_of(size) * self.cs)
                });
                if let Some((r, _)) = ph_old {
                    add(&mut dirty, RECT { left: r.left - 4, top: r.top - 4, right: r.right + 4, bottom: r.bottom + 4 });
                }
                if let Some((r, _)) = self.ui.placeholder {
                    add(&mut dirty, RECT { left: r.left - 4, top: r.top - 4, right: r.right + 4, bottom: r.bottom + 4 });
                }
                d.target = target;
                let lum_ok = self.scene.cards[i].x != nx || self.scene.cards[i].y != ny;
                // card velocity (low-passed) for the light inertia
                let (ivx, ivy) = ((nx - self.scene.cards[i].x) as f32 / dt, (ny - self.scene.cards[i].y) as f32 / dt);
                d.vel = (d.vel.0 * 0.55 + ivx * 0.45, d.vel.1 * 0.55 + ivy * 0.45);
                if lum_ok {
                    self.scene.set_card_pos(i, nx, ny);
                    let _ = self.scene.render_card(&self.gfx, i, &now0);
                }
                add(&mut dirty, old);
                add(&mut dirty, self.scene.layer_rect_ext(i, ext));
            }
        }
        // the gallery fades while a card is out on the desk and comes back when it is dropped
        if self.ui.gallery && self.ui.edit {
            let out = self.drag.as_ref().is_some_and(|d| !self.ui.panel_rect().contains(d.cur.0 as f32 / self.s, d.cur.1 as f32 / self.s));
            let a = step_toward(self.ui.away, if out { 1.0 } else { 0.0 }, dt / 0.15);
            if (a - self.ui.away).abs() > 1e-4 {
                self.ui.away = a;
                if let Some(u) = self.ui.dirty_rect(&self.scene) {
                    add(&mut dirty, u);
                }
            }
        }

        // springs (settle after a drop)
        let mut settled = false;
        let uids: Vec<usize> = self.springs.keys().copied().collect();
        for uid in uids {
            let Some(i) = self.scene.index_of_uid(uid) else {
                self.springs.remove(&uid);
                continue;
            };
            let old = self.scene.layer_rect_ext(i, ext);
            let done = match self.springs.get_mut(&uid) {
                Some(sp) => sp.step(dt),
                None => continue,
            };
            let sp = self.springs[&uid];
            self.scene.set_card_pos(i, sp.x.round() as i32, sp.y.round() as i32);
            let now = self.now();
            let _ = self.scene.render_card(&self.gfx, i, &now);
            add(&mut dirty, old);
            add(&mut dirty, self.scene.layer_rect_ext(i, ext));
            if done {
                self.springs.remove(&uid);
                // landed: the backdrop (luminance, capture region) is the one under the new place
                let _ = self.scene.measure_lum(&self.gfx, 1 << i, false);
                let _ = self.scene.render_card(&self.gfx, i, &now);
                settled = true;
            }
        }
        if settled {
            self.sync_regions();
        }

        // pops (add / resize / remove)
        let reduced = self.reduced;
        let mut finished_out = Vec::new();
        let mut finished: Vec<usize> = Vec::new();
        for (uid, p) in self.pops.iter() {
            if p.at(t, reduced).finished {
                finished.push(*uid);
            }
        }
        for uid in finished {
            if let Some(p) = self.pops.remove(&uid) {
                if p.out {
                    finished_out.push(uid);
                }
            }
        }

        // light inertia: every card's light is pushed against its own velocity (dragged: the pointer, settling: the spring)
        for i in 0..self.scene.len() {
            let uid = self.scene.cards[i].uid;
            let v = match (self.drag.as_ref().filter(|d| d.uid == uid), self.springs.get(&uid)) {
                (Some(d), _) => d.vel,
                (None, Some(sp)) => (sp.vx, sp.vy),
                _ => (0.0, 0.0),
            };
            self.scene.set_light_drive(i, v.0, v.1);
        }

        // per-card visual transform: jiggle, lift, pop
        let jig_t = t.saturating_duration_since(self.t_edit).as_secs_f32();
        // the jiggle swells in and fades out (leaving edit mode, opening the settings) instead of snapping on and off
        let want_jig = if self.ui.edit && !self.ui.settings && self.editing && !self.reduced { 1.0 } else { 0.0 };
        self.jig_amp = if self.reduced { want_jig } else { step_toward(self.jig_amp, want_jig, dt / 0.4) };
        let amp = self.jig_amp * self.jig_amp * (3.0 - 2.0 * self.jig_amp);
        for i in 0..self.scene.len() {
            let c = &self.scene.cards[i];
            let dragging = self.drag.as_ref().is_some_and(|d| d.uid == c.uid);
            let lift = if dragging { (c.xf.lift + dt / 0.12).min(1.0) } else { (c.xf.lift - dt / 0.18).max(0.0) };
            // the same swing of the edges for every size: the angle shrinks with the card's half-diagonal (S = 1.2 degrees)
            let k = (170.0 * self.cs * std::f32::consts::SQRT_2) / (c.w as f32).hypot(c.h as f32);
            let rot = if amp > 0.0 && !dragging { ui::jiggle(i, jig_t) * amp * k.min(1.0) } else { 0.0 };
            let xf = match self.pops.get(&c.uid) {
                Some(p) => mat_xf(&p.at(t, reduced), rot, lift),
                None => Xf { rot, scale: 1.0 + 0.03 * lift, alpha: 1.0, lift, blur: 0.0, sweep: -1.0 },
            };
            if xf != c.xf {
                self.scene.set_xf(i, xf);
                add(&mut dirty, self.scene.layer_rect_ext(i, ext));
            }
        }
        for uid in finished_out {
            self.finish_remove(uid);
        }
        // glass motion: light spring, pointer glint, appearance cross-fade
        if let Some(r) = self.scene.step_fx(&self.gfx, dt, &now0) {
            add(&mut dirty, r);
        }
        if self.scene.take_morph_done() {
            self.sync_regions(); // a size change landed: the capture regions and the window shrink back to the final rectangles
        }
        // the settings controls (hover, sliding segment, switches, handles) repaint only the controls that moved
        if let Some(r) = self.ui.step_sa(dt, self.reduced) {
            add(&mut dirty, r);
        }
        if let Some(r) = self.ui.step_gal(dt, self.reduced) {
            add(&mut dirty, r);
        }
        // toast: rise / fade; dropped once the fade-out ended
        let (toast_ch, toast_gone) = self.ui.toast_step(dt, self.reduced);
        if toast_ch {
            add(&mut dirty, self.toast_rect());
        }

        if let Some(d) = dirty {
            self.frame(d);
        }
        if toast_gone {
            self.fit_window(); // the toast needed the whole monitor in live mode
        }
        // finished?
        if !self.editing && self.ui.edit && self.gal_t <= 0.0 && self.dim_t <= 0.0 && self.drag.is_none() {
            self.finish_leave_edit();
        }
        if self.drag.is_none() && self.springs.is_empty() && !self.ui.edit && !self.in_menu {
            SetTimer(self.hwnd, T_SHRINK, 150, None);
        }
        self.arm_anim();
    }

    // ------------------------------------------------ edit mode
}
