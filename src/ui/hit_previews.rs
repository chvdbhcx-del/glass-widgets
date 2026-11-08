//! Hit testing, the search caret, and the live widget previews of the gallery.
use super::*;

impl Ui {
    /// What is under the canvas-px point (x, y).
    pub fn hit(&self, scene: &Scene, x: i32, y: i32) -> Hit {
        let (lx, ly) = (x as f32 / self.s, y as f32 / self.s);
        if self.edit && self.settings {
            // the desk is not interactive while the panel is up; a click outside closes it
            return self.set_hit(lx, ly);
        }
        if self.edit {
            let tb = self.tb_lay();
            if tb.bar.contains(lx, ly) {
                if tb.btn_settings.contains(lx, ly) {
                    return Hit::SettingsBtn;
                }
                if tb.track.inset_xy(-10.0, -14.0).contains(lx, ly) {
                    return Hit::Slider(((lx - tb.track.x) / tb.track.w).clamp(0.0, 1.0));
                }
                if tb.seg_live.contains(lx, ly) {
                    return Hit::Seg(true);
                }
                if tb.seg_plain.contains(lx, ly) {
                    return Hit::Seg(false);
                }
                return Hit::Toolbar;
            }
        }
        if self.edit && self.gallery {
            let l = self.lay();
            if l.panel.contains(lx, ly) {
                if l.done.contains(lx, ly) {
                    return Hit::Done;
                }
                if l.search.contains(lx, ly) {
                    return Hit::Search(lx - l.search.x);
                }
                if l.list.contains(lx, ly) {
                    if let Some(i) = l.rows.iter().position(|r| r.contains(lx, ly)) {
                        return Hit::Group(i);
                    }
                }
                if let Some((s, _)) = l.chips.iter().find(|(_, r)| r.contains(lx, ly)) {
                    return Hit::Chip(*s);
                }
                if l.grid.contains(lx, ly) {
                    if let Some(it) = l.items.iter().find(|it| it.pv.contains(lx, ly) || it.cell.contains(lx, ly)) {
                        return Hit::Item(it.k);
                    }
                }
                return Hit::Panel;
            }
        }
        if self.edit {
            for i in (0..scene.cards.len()).rev() {
                if scene.cards[i].xf.alpha < 0.5 || scene.cards[i].xf.lift > 0.0 {
                    continue;
                }
                let (mx, my) = self.minus_center(scene, i);
                if (x as f32 - mx).hypot(y as f32 - my) <= (MINUS_R + 2.0) * self.s {
                    return Hit::Minus(i);
                }
            }
        }
        match scene.cards.iter().rposition(|c| x >= c.x && x < c.x + c.w && y >= c.y && y < c.y + c.h && c.xf.alpha >= 0.5) {
            Some(i) => Hit::Card(i),
            None => Hit::None,
        }
    }

    /// Caret index of a click `dx` logical px from the left edge of the search text area (measured with the real font).
    pub fn caret_at(&self, scene: &Scene, gfx: &Gfx, dx: f32) -> usize {
        let text = self.st.query.text();
        let chars: Vec<char> = text.chars().collect();
        let l = self.lay();
        let off = self.search_scroll(scene, gfx);
        let target = dx - 30.0 + off;
        let now = Now::local();
        scene.with_ui_cx(gfx, mat(1.0, 0.0, 0.0), self.lang, &now, |cx| {
            let width = |n: usize| -> f32 {
                let pre: String = chars[..n].iter().collect();
                cx.measure(&format!("{pre}|"), Style::BODY, 4096.0).0 - cx.measure("|", Style::BODY, 4096.0).0
            };
            let _ = l;
            let mut best = (0usize, f32::MAX);
            for n in 0..=chars.len() {
                let d = (width(n) - target).abs();
                if d < best.1 {
                    best = (n, d);
                }
            }
            best.0
        })
    }

    pub(super) fn search_scroll(&self, scene: &Scene, gfx: &Gfx) -> f32 {
        let chars: Vec<char> = self.st.query.chars.clone();
        let l = self.lay();
        let avail = l.search.w - 30.0 - 10.0;
        let now = Now::local();
        let caret_w = scene.with_ui_cx(gfx, mat(1.0, 0.0, 0.0), self.lang, &now, |cx| {
            let pre: String = chars[..self.st.query.caret.min(chars.len())].iter().collect();
            cx.measure(&format!("{pre}|"), Style::BODY, 4096.0).0 - cx.measure("|", Style::BODY, 4096.0).0
        });
        (caret_w - avail).max(0.0)
    }

    // ------------------------------------------------ previews

    pub(super) fn bd_for(&mut self, gfx: &Gfx, w: i32, h: i32) -> Result<ID2D1Bitmap1> {
        if let Some(b) = self.pv_bd.get(&(w, h)) {
            return Ok(b.clone());
        }
        let mut px = vec![0u8; (w * h * 4) as usize];
        for y in 0..h {
            for x in 0..w {
                let (u, v) = (x as f32 / w as f32, y as f32 / h as f32);
                let k = (u * 0.55 + v * 0.45).clamp(0.0, 1.0);
                let glow = (1.0 - ((u - 0.25).powi(2) + (v - 0.1).powi(2)).sqrt() * 1.6).clamp(0.0, 1.0) * 26.0;
                let (r, g, b) = (74.0 + 52.0 * k + glow, 86.0 + 6.0 * k + glow, 128.0 - 8.0 * k + glow * 0.8);
                let o = ((y * w + x) * 4) as usize;
                px[o] = b.min(255.0) as u8;
                px[o + 1] = g.min(255.0) as u8;
                px[o + 2] = r.min(255.0) as u8;
                px[o + 3] = 255;
            }
        }
        let bmp = gfx.from_bgra(w as u32, h as u32, &px)?;
        self.pv_bd.insert((w, h), bmp.clone());
        Ok(bmp)
    }

    pub(super) fn ensure_pv(&mut self, gfx: &Gfx, def: &'static WidgetDef, size: Size, now: &Now) -> Result<()> {
        let key = (def.type_id, size);
        if self.pv.contains_key(&key) {
            return Ok(());
        }
        let sp = self.s * K;
        let m = (MARGIN * sp).round() as i32;
        let (lw, lh) = size.logical();
        let (w, h) = ((lw * sp).round() as i32, (lh * sp).round() as i32);
        let (fw, fh) = (w + 2 * m, h + 2 * m);
        let bd = self.bd_for(gfx, fw, fh)?;
        let env = SceneEnv { preview: true, hwnd: 0, lang: self.lang, utc_off: crate::sys::local_utc_offset_min() };
        let mut scene = Scene::empty(gfx, fw, fh, sp, 0.5, &bd, &env)?;
        scene.add_card(gfx, &CardSpec { id: format!("pv-{}", def.type_id), type_id: def.type_id.to_string(), size, cfg: json!({}), x: m, y: m, w, h })?;
        scene.tick_card(gfx, 0, now);
        scene.measure_lum(gfx, 1, false)?;
        scene.render_card(gfx, 0, now)?;
        let full = RECT { left: 0, top: 0, right: fw, bottom: fh };
        scene.composite(gfx, full, None, now)?;
        if self.pv.len() > 64 {
            self.pv.clear();
        }
        let t = std::time::Instant::now();
        self.pv.insert(key, Pv { scene, next_poll: t, next_static: t + std::time::Duration::from_secs(10) });
        Ok(())
    }

    /// Make sure every preview of the current list exists (call after the list changed, outside a frame session).
    pub fn sync_previews(&mut self, gfx: &Gfx, now: &Now) {
        let (defs, _) = self.shown();
        let (_, size) = self.chips_and_size(&defs);
        for d in defs {
            let sz = if d.sizes.contains(&size) { size } else { d.default_size };
            if let Err(e) = self.ensure_pv(gfx, d, sz, now) {
                eprintln!("mswidgets: preview {}: {e}", d.type_id);
            }
        }
    }

    /// Refresh the animated previews of the current list (host timer, 5 fps at most). Returns true when a frame changed.
    pub fn refresh_previews(&mut self, gfx: &Gfx, now: &Now) -> bool {
        let (defs, _) = self.shown();
        let (_, size) = self.chips_and_size(&defs);
        let mut changed = false;
        let t = std::time::Instant::now();
        for d in defs {
            let sz = if d.sizes.contains(&size) { size } else { d.default_size };
            let Some(pv) = self.pv.get_mut(&(d.type_id, sz)) else { continue };
            if t < pv.next_poll {
                continue;
            }
            pv.next_poll = t + std::time::Duration::from_millis(190);
            let due = pv.scene.next_due().is_some_and(|due| due <= t + std::time::Duration::from_millis(3));
            let poll = t >= pv.next_static;
            if !(due || poll) {
                continue;
            }
            if poll {
                pv.next_static = t + std::time::Duration::from_secs(10);
            }
            pv.scene.tick_card(gfx, 0, now);
            let animated = pv.scene.cards[0].tick.animates();
            let stale = pv.scene.content_stale(0, now);
            if stale {
                let _ = pv.scene.render_card(gfx, 0, now);
            }
            if stale || animated || poll {
                let full = RECT { left: 0, top: 0, right: pv.scene.w, bottom: pv.scene.h };
                if pv.scene.composite(gfx, full, None, now).is_ok() {
                    changed = true;
                }
            }
        }
        changed
    }

    /// Does the current list contain a preview that animates (clock hands)? Decides whether the 5 fps timer runs.
    pub fn previews_animate(&self) -> bool {
        let (defs, _) = self.shown();
        let (_, size) = self.chips_and_size(&defs);
        defs.iter().any(|d| {
            let sz = if d.sizes.contains(&size) { size } else { d.default_size };
            self.pv.get(&(d.type_id, sz)).is_some_and(|p| p.scene.next_due().is_some())
        })
    }

    // ------------------------------------------------ panels

    /// Create / move the glass panels for the current state and render them. Call when the geometry or the backdrop under them changed.
    pub fn sync_panels(&mut self, scene: &mut Scene, gfx: &Gfx) {
        let s = self.s;
        let px = |r: Rect| ((r.x * s).round() as i32, (r.y * s).round() as i32, (r.w * s).round() as i32, (r.h * s).round() as i32);
        let spec = |r: Rect, radius: f32| {
            let (x, y, w, h) = px(r);
            PanelSpec { x, y, w, h, radius, g: 0.9 }
        };
        let g = if self.edit && (self.gallery || self.settings) { Some(spec(self.panel_rect(), PANEL_R)) } else { None };
        let t = if self.edit && !self.settings { Some(spec(self.toolbar_rect(), TB_H / 2.0)) } else { None };
        let pr = self.prompt.as_ref().map(|p| spec(p.lay.panel, PROMPT_R));
        let mn = self.menu.as_ref().map(|m| spec(m.root.panel, menu::RADIUS));
        let sb = self.menu.as_ref().and_then(|m| m.sub.as_ref()).map(|(_, l)| spec(l.panel, menu::RADIUS));
        for (slot, sp) in [(SLOT_GALLERY, g), (SLOT_TOOLBAR, t), (SLOT_PROMPT, pr), (SLOT_MENU, mn), (SLOT_SUB, sb)] {
            if let Err(e) = scene.set_panel(gfx, slot, sp, GlassStyle::frosted()) {
                eprintln!("mswidgets: panel {slot}: {e}");
            }
            if sp.is_some() {
                if let Err(e) = scene.render_panel(gfx, slot) {
                    eprintln!("mswidgets: panel {slot} render: {e}");
                }
            }
        }
    }
}
