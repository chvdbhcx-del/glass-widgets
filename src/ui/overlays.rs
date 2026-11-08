//! Dirty rectangles and the context menu / prompt overlays.
use super::*;

impl Ui {
    /// Canvas px rectangle the UI occupies (gallery with its shadow margin + strip + toast) for dirty tracking.
    pub fn dirty_rect(&self, scene: &Scene) -> Option<RECT> {
        let pd = match (self.prompt_dirty(scene), self.menu_dirty(scene)) {
            (Some(a), Some(b)) => Some(union(a, b)),
            (a, b) => a.or(b),
        };
        if !self.edit {
            return pd;
        }
        let s = self.s;
        let m = scene.margin() + (30.0 * s) as i32;
        let r = |r: Rect| RECT { left: (r.x * s) as i32 - m, top: (r.y * s) as i32 - m, right: (r.right() * s).ceil() as i32 + m, bottom: (r.bottom() * s).ceil() as i32 + m };
        let mut d = r(if self.settings { self.panel_rect() } else { self.toolbar_rect() });
        if self.gallery {
            d = union(d, r(self.panel_rect()));
        }
        let d = clip(d, scene.w, scene.h);
        Some(pd.map_or(d, |p| union(d, p)))
    }

    // ------------------------------------------------ the text prompt

    /// Canvas px rectangle of the prompt with its shadow margin and the glide (for dirty tracking).
    pub fn prompt_dirty(&self, scene: &Scene) -> Option<RECT> {
        let p = self.prompt.as_ref()?;
        let s = self.s;
        let m = scene.margin() + ((30.0 + prompt::RISE) * s) as i32;
        let r = p.lay.panel;
        Some(clip(RECT { left: (r.x * s) as i32 - m, top: (r.y * s) as i32 - m, right: (r.right() * s).ceil() as i32 + m, bottom: (r.bottom() * s).ceil() as i32 + m }, scene.w, scene.h))
    }

    /// Alpha of the veil over the desktop while the prompt / the context menu is up (0 without one).
    pub fn scrim(&self) -> f32 {
        self.prompt.as_ref().map_or(0.0, |p| p.scrim()).max(self.menu.as_ref().map_or(0.0, |m| m.scrim()))
    }

    // ------------------------------------------------ the context menu

    /// Canvas px rectangle of the menu panels with their shadow margin (for dirty tracking).
    pub fn menu_dirty(&self, scene: &Scene) -> Option<RECT> {
        let b = self.menu.as_ref()?.bounds();
        let s = self.s;
        let m = scene.margin() + (30.0 * s) as i32;
        Some(clip(RECT { left: (b.x * s) as i32 - m, top: (b.y * s) as i32 - m, right: (b.right() * s).ceil() as i32 + m, bottom: (b.bottom() * s).ceil() as i32 + m }, scene.w, scene.h))
    }

    /// Show the menu `entries` at `anchor` (logical px on the canvas) inside `work`. Call `sync_panels` after.
    pub fn open_menu(&mut self, scene: &Scene, gfx: &Gfx, entries: Vec<Entry>, anchor: (f32, f32), work: Rect) {
        let now = Now::local();
        let m = scene.with_ui_cx(gfx, mat(1.0, 0.0, 0.0), self.lang, &now, |cx| {
            let w = |s: &str| cx.measure(s, MENU_FONT, 4096.0).0;
            MenuPanel::new(entries, anchor, work, &w)
        });
        self.menu = Some(m);
    }

    /// One panel of the menu: the glass layer, then the rows. `alpha` fades the whole panel; it grows from `level.origin` while it fades in.
    pub(super) fn draw_menu_level(&self, m: &MenuPanel, lvl: usize, slot: usize, alpha: f32, scene: &Scene, gfx: &Gfx, now: &Now) {
        let Some(level) = (if lvl == 0 { Some(&m.root) } else { m.sub.as_ref().map(|(_, l)| l) }) else { return };
        let s = self.s;
        let k = 0.94 + 0.06 * alpha;
        let (ox, oy) = (level.origin.0 * s, level.origin.1 * s);
        let xf = Matrix3x2 { M11: k, M12: 0.0, M21: 0.0, M22: k, M31: ox - k * ox, M32: oy - k * oy };
        scene.draw_panel(gfx, slot, alpha, Some(xf));
        let layer = self.push_alpha(gfx, alpha);
        scene.with_ui_cx(gfx, mul(&mat(s, 0.0, 0.0), &xf), self.lang, now, |cx| {
            let entries: &[Entry] = if lvl == 0 { &m.entries } else { m.sub.as_ref().and_then(|(i, _)| m.item_at(0, *i)).map_or(&[][..], |it| &it.children[..]) };
            for (i, e) in entries.iter().enumerate() {
                let r = level.rows[i];
                let Entry::Item(it) = e else {
                    cx.hr(r.inset_xy(PAD_SEP, 0.0));
                    continue;
                };
                if it.enabled && m.hot[lvl] == Some(i) {
                    let down = m.press == Some((lvl, i));
                    cx.rrect(r, menu::PILL_R, Color::rgba(1.0, 1.0, 1.0, if down { 0.20 } else { 0.13 }));
                }
                let c = if !it.enabled { Color::rgba(1.0, 1.0, 1.0, 0.34) } else if it.destructive { RED } else { TEXT };
                let tx = r.x + menu::ROW_PAD_X + if level.check { menu::CHECK_W } else { 0.0 };
                let tr = Rect::new(tx, r.y, r.right() - menu::ROW_PAD_X - tx - if level.arrow { 14.0 } else { 0.0 }, r.h);
                cx.text(tr, &it.label, MENU_FONT.ellipsis(), c);
                if it.checked {
                    cx.icon(Glyph::Check, Rect::new(r.x + menu::ROW_PAD_X - 2.0, r.cy() - 7.0, 14.0, 14.0), c);
                }
                if it.has_sub() {
                    cx.icon(Glyph::ChevR, Rect::new(r.right() - menu::ROW_PAD_X - 8.0, r.cy() - 6.0, 12.0, 12.0), if it.enabled { DIM } else { c });
                }
            }
        });
        if layer {
            unsafe { gfx.dc.PopLayer() };
        }
    }

    pub(super) fn draw_menu(&self, m: &MenuPanel, scene: &Scene, gfx: &Gfx, now: &Now) {
        let a = m.eased();
        self.draw_menu_level(m, 0, SLOT_MENU, a, scene, gfx, now);
        if m.sub.is_some() {
            self.draw_menu_level(m, 1, SLOT_SUB, m.sub_eased(), scene, gfx, now);
        }
    }

    /// Run `f` with the text-width closure of the prompt's field: the body font, logical px, trailing spaces count.
    pub fn with_measure<R>(&self, scene: &Scene, gfx: &Gfx, f: impl FnOnce(prompt::Measure) -> R) -> R {
        let now = Now::local();
        scene.with_ui_cx(gfx, mat(1.0, 0.0, 0.0), self.lang, &now, |cx| {
            let bar = cx.measure("|", Style::BODY, 4096.0).0;
            let w = |s: &[char]| -> f32 {
                let t: String = s.iter().collect();
                cx.measure(&format!("{t}|"), Style::BODY, 4096.0).0 - bar
            };
            f(&w)
        })
    }

    /// Show the prompt `o` next to `anchor` (logical px on the canvas) inside `work` (the work area, logical px). Call `sync_panels` after.
    pub fn open_prompt(&mut self, scene: &Scene, gfx: &Gfx, o: &PromptOpts, anchor: (f32, f32), work: Rect) {
        let tx = input::texts(o.lang, o.multiline, &o.ok_label);
        let now = Now::local();
        let (ok_w, cancel_w) = scene.with_ui_cx(gfx, mat(1.0, 0.0, 0.0), o.lang, &now, |cx| {
            let bw = |s: &str| (cx.measure(s, Style::new(13.0, 600), 4096.0).0 + 36.0).ceil();
            (bw(&tx.ok), bw(tx.cancel))
        });
        let mut p = PromptPanel::new(o, ok_w, cancel_w, anchor, work);
        self.with_measure(scene, gfx, |w| p.reveal(w));
        self.prompt = Some(p);
        self.caret_on = true;
    }

    /// A key for the prompt (nothing without one).
    pub fn prompt_key(&mut self, scene: &Scene, gfx: &Gfx, k: Key, m: Mods) -> Act {
        let Some(mut p) = self.prompt.take() else { return Act::None };
        let a = self.with_measure(scene, gfx, |w| p.key(k, m, w));
        self.prompt = Some(p);
        a
    }

    /// Mouse for the prompt, logical px: `kind` 0 = move, 1 = left down (`double`: a double click), 2 = left up.
    pub fn prompt_mouse(&mut self, scene: &Scene, gfx: &Gfx, kind: u8, x: f32, y: f32, m: Mods, double: bool) -> Act {
        let Some(mut p) = self.prompt.take() else { return Act::None };
        let a = self.with_measure(scene, gfx, |w| match kind {
            0 => p.mouse_move(x, y, w),
            1 => p.mouse_down(x, y, m, double, w),
            _ => p.mouse_up(x, y),
        });
        self.prompt = Some(p);
        a
    }

    /// Wheel over the prompt's field; true when the text scrolled.
    pub fn prompt_wheel(&mut self, scene: &Scene, gfx: &Gfx, notches: f32) -> bool {
        let Some(mut p) = self.prompt.take() else { return false };
        let moved = self.with_measure(scene, gfx, |w| p.wheel(notches, w));
        self.prompt = Some(p);
        moved
    }

    pub(super) fn draw_prompt(&self, p: &PromptPanel, scene: &Scene, gfx: &Gfx, now: &Now) {
        let a = p.eased();
        // the panel glides up a few px while it fades in (and back down while it fades out)
        let xf = Matrix3x2 { M11: 1.0, M12: 0.0, M21: 0.0, M22: 1.0, M31: 0.0, M32: (1.0 - a) * prompt::RISE * self.s };
        let to_canvas = mul(&mat(self.s, 0.0, 0.0), &xf);
        scene.draw_panel(gfx, SLOT_PROMPT, a, Some(xf));
        let layer = self.push_alpha(gfx, a);
        scene.with_ui_cx(gfx, to_canvas, self.lang, now, |cx| {
            let l = &p.lay;
            cx.text(l.title, &p.title, Style::new(15.0, 600).ellipsis(), TEXT);
            // the field: a soft white well, no ring (a white outline + halo looked like a web form); focus = a lighter well and the caret
            let focus = p.focus == Focus::Field;
            cx.rrect(l.well, 12.0, Color::rgba(1.0, 1.0, 1.0, if focus { 0.18 } else { 0.11 }));
            cx.rrect_stroke(l.well, 12.0, 1.0, Color::rgba(1.0, 1.0, 1.0, if focus { 0.10 } else { 0.06 }));
            self.draw_field(cx, p, focus);
            cx.text(l.hint, &p.hint, Style::new(11.5, 400).ellipsis(), FAINT);
            // Cancel = a quiet white layer, Done = the white glass pill of the panels; hover lightens, press darkens, the keyboard ring is white
            let btn = |r: Rect, label: &str, f: Focus| {
                let hot = p.hover == Some(f);
                let down = p.press == Some(f) && hot;
                if f == Focus::Ok {
                    done_button(cx, r, label, hot);
                    if down {
                        cx.pill(r, Color::rgba(0.0, 0.0, 0.0, 0.14));
                    }
                } else {
                    cx.pill(r, Color::rgba(1.0, 1.0, 1.0, if down { 0.08 } else if hot { 0.20 } else { 0.12 }));
                    cx.text(r, label, Style::new(13.0, 600).center(), TEXT);
                }
                if p.ring && p.focus == f {
                    cx.rrect_stroke(r.inset(-2.5), r.h / 2.0 + 2.5, 1.5, Color::rgba(1.0, 1.0, 1.0, 0.90));
                }
            };
            btn(l.cancel, &p.cancel, Focus::Cancel);
            btn(l.ok, &p.ok, Focus::Ok);
        });
        if layer {
            unsafe { gfx.dc.PopLayer() };
        }
    }

    /// The text of the prompt: placeholder, wrapped lines, selection highlight, caret.
    pub(super) fn draw_field(&self, cx: &Cx, p: &PromptPanel, focus: bool) {
        let t = p.lay.text;
        let bar = cx.measure("|", Style::BODY, 4096.0).0;
        let w = |s: &[char]| -> f32 {
            let t: String = s.iter().collect();
            cx.measure(&format!("{t}|"), Style::BODY, 4096.0).0 - bar
        };
        let chars = &p.f.chars;
        let lines = p.lines(&w);
        let sel = p.f.sel();
        let hl = Color::rgba(1.0, 1.0, 1.0, 0.30);
        let (caret_on, line_h) = (focus && self.caret_on && sel.is_none(), prompt::LINE_H);
        cx.clip(t);
        if chars.is_empty() {
            let r = if p.multiline { Rect::new(t.x, t.y, t.w, line_h) } else { t };
            cx.text(r, &p.placeholder, Style::BODY.ellipsis(), DIM);
        }
        if p.multiline {
            for (i, l) in lines.iter().enumerate() {
                let y = t.y + i as f32 * line_h - p.scroll;
                if y + line_h < t.y || y > t.bottom() {
                    continue;
                }
                if let Some((a, b)) = sel {
                    let (lo, hi) = (a.max(l.start), b.min(l.end));
                    // a selection that runs over the line break shows a little tail after the line
                    let tail = if a <= l.end && b > l.end && l.end < chars.len() && chars[l.end] == '\n' { 5.0 } else { 0.0 };
                    if lo < hi || tail > 0.0 {
                        let lo = lo.clamp(l.start, l.end);
                        let hi = hi.clamp(lo, l.end);
                        let (x0, x1) = (w(&chars[l.start..lo]), w(&chars[l.start..hi]));
                        cx.rrect(Rect::new(t.x + x0 - 1.0, y + 1.0, x1 - x0 + tail + 2.0, line_h - 2.0), 4.0, hl);
                    }
                }
                let s: String = chars[l.start..l.end].iter().collect();
                cx.text(Rect::new(t.x, y, 4096.0, line_h), &s, Style::BODY, TEXT);
            }
            if caret_on {
                let (li, x) = p.f.caret_x(&lines, &w);
                let y = t.y + li as f32 * line_h - p.scroll;
                cx.line((t.x + x, y + 3.0), (t.x + x, y + line_h - 3.0), 1.3, TEXT);
            }
        } else {
            if let Some((a, b)) = sel {
                let (x0, x1) = (w(&chars[..a]), w(&chars[..b]));
                cx.rrect(Rect::new(t.x - p.scroll + x0 - 1.0, t.cy() - 9.0, x1 - x0 + 2.0, 18.0), 4.0, hl);
            }
            if !chars.is_empty() {
                let s: String = chars.iter().collect();
                cx.text(Rect::new(t.x - p.scroll, t.y, 4096.0, t.h), &s, Style::BODY, TEXT);
            }
            if caret_on {
                let x = t.x - p.scroll + w(&chars[..p.f.caret.min(chars.len())]);
                cx.line((x, t.cy() - 8.0), (x, t.cy() + 8.0), 1.3, TEXT);
            }
        }
        cx.pop_clip();
    }

    // ------------------------------------------------ drawing
}
