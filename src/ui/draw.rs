//! Drawing: toolbar, settings panel and gallery.
use super::*;

impl Ui {
    /// Under the cards: the dim over the desktop and the placeholder of the cell a card is about to land in.
    pub fn draw_pre(&self, gfx: &Gfx, clip_px: RECT) {
        let dc = &gfx.dc;
        unsafe {
            let dim = self.dim.max(self.scrim());
            if dim > 0.002 {
                if let Ok(b) = dc.CreateSolidColorBrush(&col(0.0, 0.0, 0.0, dim), None) {
                    dc.FillRectangle(&rf(clip_px.left as f32, clip_px.top as f32, clip_px.right as f32, clip_px.bottom as f32), &b);
                }
            }
            if let Some((r, radius)) = self.placeholder {
                let rrect = rr(r.left as f32, r.top as f32, (r.right - r.left) as f32, (r.bottom - r.top) as f32, radius);
                if let Ok(f) = dc.CreateSolidColorBrush(&col(1.0, 1.0, 1.0, 0.13), None) {
                    dc.FillRoundedRectangle(&rrect, &f);
                }
                if let Ok(b) = dc.CreateSolidColorBrush(&col(1.0, 1.0, 1.0, 0.42), None) {
                    let inner = rr(r.left as f32 + 0.75 * self.s, r.top as f32 + 0.75 * self.s, (r.right - r.left) as f32 - 1.5 * self.s, (r.bottom - r.top) as f32 - 1.5 * self.s, radius);
                    dc.DrawRoundedRectangle(&inner, &b, 1.5 * self.s, None::<&ID2D1StrokeStyle>);
                }
            }
        }
    }

    /// Over the cards: "−" buttons, the settings strip, the gallery, the toast.
    pub fn draw_post(&self, scene: &Scene, gfx: &Gfx, now: &Now, clip_px: RECT) {
        let s = self.s;
        let base = mat(s, 0.0, 0.0);
        let hits = |r: Rect, pad: f32| {
            let (l, t, rr, b) = ((r.x * s - pad) as i32, (r.y * s - pad) as i32, (r.right() * s + pad) as i32, (r.bottom() * s + pad) as i32);
            l < clip_px.right && clip_px.left < rr && t < clip_px.bottom && clip_px.top < b
        };
        if self.edit && self.settings {
            if hits(self.panel_rect(), 40.0 * s) {
                self.draw_settings(scene, gfx, now);
            }
        } else if self.edit {
            scene.with_ui_cx(gfx, base, self.lang, now, |cx| {
                for i in 0..scene.cards.len() {
                    let c = &scene.cards[i];
                    if c.xf.lift > 0.0 || c.xf.alpha < 0.6 {
                        continue;
                    }
                    let (mx, my) = self.minus_center(scene, i);
                    if hits(Rect::new(mx / s - 16.0, my / s - 16.0, 32.0, 32.0), 0.0) {
                        self.draw_minus(cx, (mx / s, my / s), self.hover == Hit::Minus(i), c.xf.alpha);
                    }
                }
            });
            if hits(self.toolbar_rect(), 40.0 * s) {
                self.draw_toolbar(scene, gfx, now);
            }
            if self.gallery && hits(self.panel_rect(), 40.0 * s) {
                self.draw_gallery(scene, gfx, now);
            }
        }
        if let Some(p) = &self.prompt {
            if hits(p.lay.panel, 40.0 * s) {
                self.draw_prompt(p, scene, gfx, now);
            }
        }
        if let Some(m) = &self.menu {
            if hits(m.bounds(), 40.0 * s) {
                self.draw_menu(m, scene, gfx, now);
            }
        }
        if let Some(t) = &self.toast {
            // rises 8 px while it fades in, fades out in place (`toast_step` moves the numbers)
            let layer = self.push_alpha(gfx, self.toast_a);
            scene.with_ui_cx(gfx, mat(s, 0.0, self.toast_rise * s), self.lang, now, |cx| {
                let (tw, _) = cx.measure(t, Style::new(13.0, 500), 600.0);
                let r = Rect::new((self.mw - tw - 36.0) / 2.0, self.mh * 0.5 - 18.0, tw + 36.0, 36.0);
                cx.rrect(r.offset(0.0, 3.0), 18.0, Color::rgba(0.0, 0.0, 0.0, 0.18));
                cx.pill(r, Color::rgba(0.11, 0.11, 0.14, 0.92));
                cx.rrect_stroke(r, 18.0, 0.8, Color::rgba(1.0, 1.0, 1.0, 0.18));
                cx.text(r, t, Style::new(13.0, 500).center(), TEXT);
            });
            if layer {
                unsafe { gfx.dc.PopLayer() };
            }
        }
    }

    pub(super) fn draw_minus(&self, cx: &Cx, c: Pt, hot: bool, alpha: f32) {
        for (i, a) in [0.06f32, 0.07, 0.09].iter().enumerate() {
            cx.circle((c.0, c.1 + 2.0), MINUS_R + 3.0 - i as f32 * 1.4, Color::rgba(0.0, 0.0, 0.0, a * alpha));
        }
        let bg = if hot { Color::rgba(1.0, 0.27, 0.23, 0.97) } else { Color::rgba(0.24, 0.24, 0.26, 0.96) };
        cx.circle(c, MINUS_R, bg.fade(alpha));
        cx.circle_stroke(c, MINUS_R - 0.6, 1.0, Color::rgba(1.0, 1.0, 1.0, 0.24 * alpha));
        cx.line((c.0 - 5.2, c.1), (c.0 + 5.2, c.1), 2.2, WHITE.fade(alpha));
    }

    pub(super) fn seg(&self, cx: &Cx, r: Rect, labels: &[(&str, bool)], pad: f32, hot: Option<usize>) {
        cx.pill(r, FILL);
        for (i, (txt, on)) in labels.iter().enumerate() {
            let n = labels.len() as f32;
            let c = Rect::new(r.x + pad + (r.w - 2.0 * pad) / n * i as f32, r.y + pad, (r.w - 2.0 * pad) / n, r.h - 2.0 * pad);
            if hot == Some(i) && !*on {
                cx.pill(c, Color::rgba(1.0, 1.0, 1.0, 0.13));
            }
            if *on {
                cx.pill(c.offset(0.0, 1.0), Color::rgba(0.0, 0.0, 0.0, 0.18));
                cx.pill(c, Color::rgba(1.0, 1.0, 1.0, 0.92));
            }
            cx.text(c, txt, Style::new(11.5, 600).center(), if *on { ON_WHITE } else { TEXT });
        }
    }

    /// A segmented control of the settings panel: the white pill sits at `pos` (in segments, it slides), the labels darken as it passes
    /// under them, `hov(i)` is the hover amount of segment i.
    pub(super) fn seg_anim(&self, cx: &Cx, r: Rect, labels: &[&str], pad: f32, pos: f32, hov: impl Fn(usize) -> f32) {
        cx.pill(r, FILL);
        let n = labels.len().max(1) as f32;
        let cw = (r.w - 2.0 * pad) / n;
        let cell = |i: f32| Rect::new(r.x + pad + cw * i, r.y + pad, cw, r.h - 2.0 * pad);
        let near = |i: usize| (1.0 - (pos - i as f32).abs()).clamp(0.0, 1.0);
        for i in 0..labels.len() {
            let h = hov(i) * (1.0 - near(i));
            if h > 0.001 {
                cx.pill(cell(i as f32), Color::rgba(1.0, 1.0, 1.0, 0.13 * h));
            }
        }
        let p = cell(pos.clamp(0.0, n - 1.0));
        cx.pill(p.offset(0.0, 1.0), Color::rgba(0.0, 0.0, 0.0, 0.18));
        cx.pill(p, Color::rgba(1.0, 1.0, 1.0, 0.92));
        let st = Style::new(11.5, 600).center();
        for (i, txt) in labels.iter().enumerate() {
            cx.text(cell(i as f32), txt, st, TEXT);
        }
        // the part of a label the pill is over turns dark with it
        cx.with_clip_round(p, p.h / 2.0, |cx| {
            for (i, txt) in labels.iter().enumerate() {
                if near(i) > 0.0 {
                    cx.text(cell(i as f32), txt, st, ON_WHITE);
                }
            }
        });
    }

    /// Open / close transform of the gallery and the strip: slide up 26 px, scale .97 -> 1 about the bottom-left corner.
    pub(super) fn open_xf(&self) -> Matrix3x2 {
        let t = self.anim.clamp(0.0, 1.0);
        let k = 0.97 + 0.03 * t;
        let p = self.panel_rect();
        let (ax, ay) = (p.x * self.s, p.bottom() * self.s);
        Matrix3x2 { M11: k, M12: 0.0, M21: 0.0, M22: k, M31: ax - k * ax, M32: ay - k * ay + (1.0 - t) * 26.0 * self.s }
    }

    pub(super) fn push_alpha(&self, gfx: &Gfx, a: f32) -> bool {
        if a >= 0.999 {
            return false;
        }
        let lp = D2D1_LAYER_PARAMETERS1 {
            contentBounds: rf(-3.0e38, -3.0e38, 3.0e38, 3.0e38),
            geometricMask: std::mem::ManuallyDrop::new(None),
            maskAntialiasMode: D2D1_ANTIALIAS_MODE_PER_PRIMITIVE,
            maskTransform: mat(1.0, 0.0, 0.0),
            opacity: a.clamp(0.0, 1.0),
            opacityBrush: std::mem::ManuallyDrop::new(None),
            layerOptions: D2D1_LAYER_OPTIONS1_NONE,
        };
        unsafe { gfx.dc.PushLayer(&lp, None::<&ID2D1Layer>) };
        true
    }

    pub(super) fn draw_toolbar(&self, scene: &Scene, gfx: &Gfx, now: &Now) {
        let a = self.anim.clamp(0.0, 1.0);
        let tb = self.tb_lay();
        scene.draw_panel(gfx, SLOT_TOOLBAR, a, None);
        let layer = self.push_alpha(gfx, a);
        scene.with_ui_cx(gfx, mat(self.s, 0.0, 0.0), self.lang, now, |cx| {
            cx.text(tb.label_glass, self.t("Стекло", "Glass"), Style::new(12.5, 500), TEXT);
            let v = self.glass.clamp(0.0, 1.0);
            cx.pill(tb.track, FILL);
            cx.pill(Rect::new(tb.track.x, tb.track.y, (tb.track.w * v).max(4.0), tb.track.h), Color::rgba(1.0, 1.0, 1.0, 0.55));
            let kx = tb.track.x + tb.track.w * v;
            cx.circle((kx, tb.track.cy() + 1.5), 9.0, Color::rgba(0.0, 0.0, 0.0, 0.25));
            cx.circle((kx, tb.track.cy()), 8.0, Color::rgba(1.0, 1.0, 1.0, 0.97));
            if self.sfocus == Some(0) {
                cx.circle_stroke((kx, tb.track.cy()), 11.5, 2.0, Color::rgba(1.0, 1.0, 1.0, 0.70));
            }
            cx.rect(tb.sep, FILL);
            cx.text(tb.label_bg, self.t("Фон", "Back"), Style::new(12.5, 500), TEXT);
            let seg = Rect::new(tb.seg_live.x, tb.seg_live.y, tb.seg_live.w + tb.seg_plain.w, tb.seg_live.h);
            self.seg(cx, seg, &[(self.t("Живое", "Live"), self.live), (self.t("Простой", "Plain"), !self.live)], 2.0, if let Hit::Seg(l) = self.hover { Some(usize::from(!l)) } else { None });
            cx.rect(tb.sep2, FILL);
            let hot = self.hover == Hit::SettingsBtn || self.press == Hit::SettingsBtn;
            cx.pill(tb.btn_settings, if hot { Color::rgba(1.0, 1.0, 1.0, 0.30) } else { FILL });
            cx.text(tb.btn_settings, self.t("Настройки", "Settings"), Style::new(11.5, 600).center(), TEXT);
        });
        if layer {
            unsafe { gfx.dc.PopLayer() };
        }
    }

    /// The glass slider of the strip, restyled for the settings rows: track, fill, base-look tick and a knob that swells under the pointer
    /// (hover and press are eased, the handle glides to a value set by a click, Reset or the keyboard and follows a drag 1:1).
    pub(super) fn set_slider(&self, cx: &Cx, r: &SliderRow, i: usize) {
        let t = r.track;
        let (h, p) = (self.sa.hov.get(i), self.sa.press(i));
        let v = self.slider_shown(i);
        if self.sfocus == Some(i) {
            cx.rrect(r.row.inset_xy(-8.0, 1.0), 8.0, Color::rgba(1.0, 1.0, 1.0, 0.07));
        }
        cx.pill(t, FILL);
        cx.pill(Rect::new(t.x, t.y, (t.w * v).max(4.0), t.h), Color::rgba(1.0, 1.0, 1.0, 0.55 + 0.15 * h));
        let dx = t.x + t.w * self.set_default_pos(i);
        cx.line((dx, t.cy() + 5.0), (dx, t.cy() + 8.0), 1.2, FAINT);
        let kx = t.x + t.w * v;
        let kr = 8.0 + 0.6 * h + 0.4 * p;
        if p > 0.01 {
            cx.circle_stroke((kx, t.cy()), kr + 3.0, 3.0, Color::rgba(1.0, 1.0, 1.0, 0.20 * p));
        }
        cx.circle((kx, t.cy() + 1.5), kr + 1.0, Color::rgba(0.0, 0.0, 0.0, 0.25));
        cx.circle((kx, t.cy()), kr, Color::rgba(1.0, 1.0, 1.0, 0.97));
        if self.sfocus == Some(i) {
            cx.circle_stroke((kx, t.cy()), kr + 3.5, 2.0, Color::rgba(1.0, 1.0, 1.0, 0.70));
        }
    }

    /// A label with an on / off switch on the right: the fill cross-fades and the knob glides (`pos` 0..1), and it stretches while pressed.
    pub(super) fn set_switch(&self, cx: &Cx, row: Rect, sw: Rect, label: &str, key: usize, pos: f32) {
        cx.text(Rect::new(row.x, row.y, row.w - 52.0, row.h), label, Style::new(12.5, 500).ellipsis(), TEXT);
        let (h, p, e) = (self.sa.hov.get(key), self.sa.press(key), motion::smooth(pos));
        let sw = sw.centered(40.0, 24.0);
        cx.pill(sw, FILL.mix(Color::rgba(1.0, 1.0, 1.0, 0.26), h).mix(Color::rgb8(52, 199, 89), e));
        let (lx, rx) = (sw.x + 12.0, sw.right() - 12.0);
        let ext = 4.0 * p; // grows towards the middle of the switch
        let kr = Rect::new(lx + (rx - lx) * e - 9.5 - ext * e, sw.cy() - 9.5, 19.0 + ext, 19.0);
        cx.rrect(kr.inset(-0.5).offset(0.0, 1.2), 10.0, Color::rgba(0.0, 0.0, 0.0, 0.22));
        cx.rrect(kr, 9.5, Color::rgba(1.0, 1.0, 1.0, 0.98));
    }

    /// A button of the settings panel: the white layer lightens on hover and press, the button shrinks to `PRESS_BTN` on a spring.
    pub(super) fn set_button(&self, cx: &Cx, r: Rect, label: &str, key: usize, enabled: bool, color: Color) {
        let h = if enabled { self.sa.hov.get(key) } else { 0.0 };
        let p = if enabled { self.sa.press(key) } else { 0.0 };
        let x = if enabled { self.sa.prs.get(key).map_or(1.0, |b| b.x) } else { 1.0 };
        cx.scaled((r.cx(), r.cy()), x, |cx| {
            cx.pill(r, Color::rgba(1.0, 1.0, 1.0, 0.18 + 0.09 * h + 0.07 * p));
            cx.text(r, label, Style::new(12.5, 600).center().ellipsis(), color);
        });
    }

    /// The update status line: the old text fades out while the new one fades in when the kind of status changes.
    pub(super) fn draw_upd_status(&self, cx: &Cx, r: Rect) {
        let st = Style::new(12.0, 500).ellipsis();
        if upd_key(&self.update) != self.sa.st_key {
            // the change has not been stepped yet: keep the old line for this frame
            let (m, c) = &self.sa.st_last;
            return cx.text(r, m, st, *c);
        }
        let (msg, color) = self.upd_line();
        let k = motion::smooth(self.sa.st_t);
        match &self.sa.st_prev {
            Some((pm, pc)) if k < 1.0 => {
                cx.text(r, pm, st, pc.fade(1.0 - k));
                cx.text(r, &msg, st, color.fade(k));
            }
            _ => cx.text(r, &msg, st, color),
        }
    }

    pub(super) fn draw_settings(&self, scene: &Scene, gfx: &Gfx, now: &Now) {
        let a = self.anim.clamp(0.0, 1.0);
        let xf = self.open_xf();
        let to_canvas = mul(&mat(self.s, 0.0, 0.0), &xf);
        scene.draw_panel(gfx, SLOT_GALLERY, a, Some(xf));
        let layer = self.push_alpha(gfx, a);
        let l = self.set_lay();
        scene.with_ui_cx(gfx, to_canvas, self.lang, now, |cx| {
            cx.text(l.title, self.t("Настройки", "Settings"), Style::new(15.0, 600), TEXT);
            cx.text(l.look_hdr, &self.t("Вид стекла", "Glass look").to_uppercase(), Style::LABEL.size(11.0), DIM);
            cx.text(l.beh_hdr, &self.t("Поведение", "Behaviour").to_uppercase(), Style::LABEL.size(11.0), DIM);
            cx.rect(l.sep, FILL);

            // look sliders
            let names = [("Стекло", "Glass"), ("Размытие", "Blur"), ("Преломление", "Refraction"), ("Цветная кайма", "Chromatic fringe"), ("Насыщенность", "Saturation"), ("Яркость", "Brightness"), ("Матовость", "Frosting"), ("Блики", "Highlights"), ("Свечение", "Glow"), ("Скругление", "Corner roundness"), ("Размер карточек", "Card size")];
            for (i, r) in l.rows.iter().enumerate() {
                let (ru, en) = names.get(i).copied().unwrap_or(("", ""));
                cx.text(r.label, self.t(ru, en), Style::new(12.5, 500).ellipsis(), TEXT);
                self.set_slider(cx, r, i);
                cx.text(r.value, &self.set_value_text(i), Style::new(11.5, 500).right().mono(), DIM.mix(TEXT, self.sa.hov.get(i)));
            }
            self.set_button(cx, l.reset, self.t("Сбросить вид", "Reset look"), K_RESET, true, TEXT);

            // behaviour: segmented controls
            let labels: [(Opt, (&str, &str), [(&str, &str); 3]); 6] = [
                (Opt::Appearance, ("Оформление", "Appearance"), [("Тёмное", "Dark"), ("Светлое", "Light"), ("Авто", "Auto")]),
                (Opt::Dim, ("Приглушение", "Dimming"), [("Авто", "Auto"), ("Никогда", "Never"), ("Всегда", "Always")]),
                (Opt::Source, ("Источник стекла", "Glass source"), [("Живой", "Live"), ("Простой", "Plain"), ("", "")]),
                (Opt::Accent, ("Акцент", "Accent"), [("Синий", "Blue"), ("Из обоев", "Wallpaper"), ("", "")]),
                (Opt::Scope, ("Акцент на", "Accent on"), [("Главном", "Main"), ("Везде", "Everywhere"), ("", "")]),
                (Opt::Monitor, ("Монитор", "Monitor"), [("", ""), ("", ""), ("", "")]),
            ];
            let nums: Vec<String> = (1..=self.opt_n(Opt::Monitor)).map(|n| n.to_string()).collect();
            for o in &l.opts {
                let Some((_, (ru, en), opts)) = labels.iter().find(|x| x.0 == o.which) else { continue };
                cx.text(Rect::new(o.label.x, o.label.y, o.label.w, o.label.h), self.t(ru, en), Style::new(12.5, 500).ellipsis(), TEXT);
                if o.which == Opt::Monitor && nums.len() == 1 {
                    // one monitor: nothing to choose, so no white pill: the same track as its neighbours with the plain fact in it
                    cx.pill(o.seg, FILL);
                    cx.text(o.seg, self.t("Один монитор", "One display"), Style::new(11.5, 500).center().ellipsis(), DIM);
                    continue;
                }
                let items: Vec<&str> = if o.which == Opt::Monitor {
                    nums.iter().map(|n| n.as_str()).collect()
                } else {
                    opts.iter().take(o.which.count()).map(|(r, e)| self.t(r, e)).collect()
                };
                let base = K_OPT + opt_idx(o.which) * MON_SEGS;
                self.seg_anim(cx, o.seg, &items, 2.0, self.seg_shown(o.which), |k| self.sa.hov.get(base + k.min(MON_SEGS - 1)));
            }

            // start with Windows, check for updates on start: label + switch
            self.set_switch(cx, l.auto_row, l.auto_sw, self.t("Запускать с Windows", "Start with Windows"), K_AUTO, self.sw_shown(0, self.autostart));
            self.set_switch(cx, l.check_row, l.check_sw, self.t("Проверять обновления при запуске", "Check for updates on start"), K_CHECK, self.sw_shown(1, self.check_start));

            // buttons
            self.set_button(cx, l.edit_btn, self.t("Изменить виджеты", "Edit widgets"), K_EDIT, true, TEXT);
            self.set_button(cx, l.quit_btn, self.t("Выйти", "Quit"), K_QUIT, true, Color::rgb8(255, 105, 97));

            // shortcuts: a key chip and its description, the chips one width so the descriptions line up, both in one size so they share a baseline
            let k = Style::new(11.0, 500);
            let lines = [
                ("Ctrl+Alt+G", ("Настройки", "Settings")),
                ("Ctrl+Alt+W", ("Изменить виджеты", "Edit widgets")),
                ("Ctrl+Alt+Q", ("Выйти", "Quit")),
                ("Esc", ("Закрыть (или щелчок снаружи)", "Close (or click outside)")),
                ("Tab / ← →", ("Ползунки с клавиатуры", "Sliders by keyboard")),
            ];
            let chip_w = lines.iter().map(|(keys, _)| cx.measure(keys, k.mono(), 0.0).0).fold(0.0, f32::max).ceil() + 14.0;
            for (n, (keys, (ru, en))) in lines.iter().enumerate() {
                let r = Rect::new(l.keys.x, l.keys.y + n as f32 * 24.0, l.keys.w, 22.0);
                if r.bottom() > l.keys.bottom() + 0.5 {
                    break;
                }
                let chip = Rect::new(r.x, r.y, chip_w, r.h);
                cx.rrect(chip, 6.0, Color::rgba(1.0, 1.0, 1.0, 0.09));
                cx.text(chip, keys, k.mono().center(), DIM);
                cx.text(Rect::new(chip.right() + 10.0, r.y, (r.right() - chip.right() - 10.0).max(0.0), r.h), self.t(ru, en), k.weight(400).ellipsis(), DIM);
            }

            // footer
            cx.rect(l.foot, FILL);
            let busy = matches!(self.update, UpdateState::Checking | UpdateState::Downloading(_) | UpdateState::Verifying);
            self.set_button(cx, l.upd_btn, self.t("Проверить обновления", "Check for updates"), K_UPD, !busy, if busy { DIM } else { TEXT });
            self.draw_upd_status(cx, l.upd_status);
            // "Release page" / "Update now" fade in with an available update (and out again)
            let ba = motion::smooth(self.sa.btns);
            if ba > 0.0 {
                let layer = self.push_alpha(gfx, ba);
                self.set_button(cx, l.upd_page, self.t("О релизе", "Release page"), K_PAGE, true, DIM);
                self.set_button(cx, l.upd_dl, self.t("Обновить", "Update now"), K_DL, true, TEXT);
                if layer {
                    unsafe { gfx.dc.PopLayer() };
                }
            }
            let (h, x) = (self.sa.hov.get(K_DONE), self.sa.prs.get(K_DONE).map_or(1.0, |b| b.x));
            cx.scaled((l.done.cx(), l.done.cy()), x, |cx| done_button_k(cx, l.done, self.t("Готово", "Done"), h));
        });
        if layer {
            unsafe { gfx.dc.PopLayer() };
        }
    }

    pub(super) fn draw_gallery(&self, scene: &Scene, gfx: &Gfx, now: &Now) {
        let a = (self.anim * (1.0 - 0.72 * self.away)).clamp(0.0, 1.0);
        let xf = self.open_xf();
        let to_canvas = mul(&mat(self.s, 0.0, 0.0), &xf);
        scene.draw_panel(gfx, SLOT_GALLERY, a, Some(xf));
        let layer = self.push_alpha(gfx, a);
        let l = self.lay();
        let (defs, title) = self.shown();
        let (_, size) = self.chips_and_size(&defs);
        let s = self.s;
        scene.with_ui_cx(gfx, to_canvas, self.lang, now, |cx| {
            let p = l.panel;
            // sidebar: separator, search field, groups
            cx.rect(Rect::new(p.x + SIDE_W, p.y + 14.0, 0.8, p.h - FOOT_H - 18.0), FILL);
            let focus = self.st.focus;
            cx.rrect(l.search, 9.0, Color::rgba(1.0, 1.0, 1.0, if focus { 0.16 } else { 0.12 }));
            if focus {
                cx.rrect_stroke(l.search.inset(-2.0), 11.0, 3.0, Color::rgba(1.0, 1.0, 1.0, 0.16));
                cx.rrect_stroke(l.search, 9.0, 1.4, Color::rgba(1.0, 1.0, 1.0, 0.70));
            }
            cx.icon(Glyph::Search, Rect::new(l.search.x + 9.0, l.search.cy() - 7.0, 14.0, 14.0), DIM);
            let text_area = Rect::new(l.search.x + 30.0, l.search.y, l.search.w - 38.0, l.search.h);
            cx.clip(text_area);
            let q = self.st.query.text();
            let off = self.search_scroll_cx(cx);
            if q.is_empty() {
                cx.text(text_area, self.t("Поиск виджетов", "Search Widgets"), Style::BODY.ellipsis(), DIM);
            } else {
                let tw = cx.measure(&q, Style::BODY, 4096.0).0;
                if self.st.query.sel_all {
                    cx.rrect(Rect::new(text_area.x - off - 1.0, text_area.cy() - 9.0, tw + 2.0, 18.0), 4.0, Color::rgba(1.0, 1.0, 1.0, 0.30));
                }
                cx.text(Rect::new(text_area.x - off, text_area.y, 4096.0, text_area.h), &q, Style::BODY, TEXT);
            }
            if focus && self.caret_on && !self.st.query.sel_all {
                let pre: String = self.st.query.chars[..self.st.query.caret.min(self.st.query.chars.len())].iter().collect();
                let cw = cx.measure(&format!("{pre}|"), Style::BODY, 4096.0).0 - cx.measure("|", Style::BODY, 4096.0).0;
                let x = text_area.x - off + cw;
                cx.line((x, l.search.cy() - 8.0), (x, l.search.cy() + 8.0), 1.3, TEXT);
            }
            cx.pop_clip();

            cx.clip(l.list);
            let searching = !q.trim().is_empty();
            let cur = self.st.group.as_deref().or(self.groups.first().map(|g| g.id));
            for (i, g) in self.groups.iter().enumerate() {
                let r = l.rows[i];
                if r.bottom() < l.list.y || r.y > l.list.bottom() {
                    continue;
                }
                let sel = !searching && cur == Some(g.id);
                if sel {
                    cx.rrect(r, 9.0, SEL);
                } else if self.hover == Hit::Group(i) {
                    cx.rrect(r, 9.0, FILL);
                }
                let ic = Rect::new(r.x + 8.0, r.cy() - 12.0, 24.0, 24.0);
                let (c0, c1) = group_colors(g.id);
                cx.gradient_rrect(ic, 6.0, c0, c1, 180.0);
                cx.rrect_stroke(ic, 6.0, 0.8, Color::rgba(1.0, 1.0, 1.0, 0.22));
                cx.icon(g.icon, ic.inset(4.5), WHITE);
                cx.text(Rect::new(r.x + 40.0, r.y, r.w - 46.0, r.h), self.t(g.name.0, g.name.1), Style::BODY.ellipsis(), if sel { WHITE } else { TEXT });
            }
            cx.pop_clip();

            // header: title + size chips
            cx.text(l.title, &title, Style::new(15.0, 600), TEXT);
            if !l.chips.is_empty() {
                let r = Rect::new(l.chips[0].1.x - 2.0, l.chips[0].1.y, l.chips.len() as f32 * 34.0 + 4.0, 24.0);
                let labels: Vec<(&str, bool)> = l.chips.iter().map(|(s, _)| (s.key(), *s == size)).collect();
                let up: Vec<String> = labels.iter().map(|(k, _)| k.to_uppercase()).collect();
                let lab: Vec<(&str, bool)> = up.iter().zip(labels.iter()).map(|(u, (_, on))| (u.as_str(), *on)).collect();
                self.seg(cx, r, &lab, 2.0, None);
            }

            // previews
            cx.clip(l.grid);
            if l.items.is_empty() {
                cx.text(l.grid, self.t("Ничего не найдено", "No widgets found"), Style::BODY.center(), DIM);
            }
            for it in &l.items {
                if it.cell.bottom() < l.grid.y || it.cell.y > l.grid.bottom() {
                    continue;
                }
                let Some((def, _)) = self.item(it.k) else { continue };
                let hot = self.hover == Hit::Item(it.k);
                // a new list (group, size, search) fades in tile by tile
                cx.with_opacity(self.tile_alpha(it.k), |cx| {
                if let Some(pv) = self.pv.get(&(def.type_id, it.size)) {
                    // the preview frame is in physical px at scale s*K: draw it 1:1 in logical space (scale 1/s) at the card origin minus the shadow margin
                    let mg = MARGIN * K;
                    let t = mul(&mat(1.0 / s, it.pv.x - mg, it.pv.y - mg), &to_canvas);
                    unsafe {
                        cx.dc.SetTransform(&t);
                        cx.dc.DrawImage(pv.scene.frame_bitmap(), None, None, D2D1_INTERPOLATION_MODE_LINEAR, D2D1_COMPOSITE_MODE_SOURCE_OVER);
                        cx.dc.SetTransform(&to_canvas);
                    }
                }
                if hot {
                    cx.rrect_stroke(it.pv.inset(-2.0), 17.0 * K + 2.0, 1.4, Color::rgba(1.0, 1.0, 1.0, 0.35));
                    let b = (it.pv.x + 14.0, it.pv.y + 14.0);
                    cx.circle((b.0, b.1 + 1.0), 12.0, Color::rgba(0.0, 0.0, 0.0, 0.25));
                    cx.circle(b, 11.0, Color::rgba(1.0, 1.0, 1.0, 0.95));
                    cx.line((b.0 - 4.6, b.1), (b.0 + 4.6, b.1), 1.9, ON_WHITE);
                    cx.line((b.0, b.1 - 4.6), (b.0, b.1 + 4.6), 1.9, ON_WHITE);
                }
                cx.text(it.title, self.t(def.name.0, def.name.1), Style::new(12.5, 600).center().ellipsis(), TEXT);
                cx.clip(it.desc);
                cx.text(it.desc, self.t(def.desc.0, def.desc.1), Style::new(11.5, 400).center().wrap(), DIM);
                cx.pop_clip();
                });
            }
            cx.pop_clip();

            // footer: hint + Done
            cx.rect(Rect::new(p.x + 14.0, p.bottom() - FOOT_H, p.w - 28.0, 0.8), FILL);
            cx.text(l.hint, self.t("Перетащите виджет на рабочий стол…", "Drag a widget to place it on the desktop…"), Style::new(12.5, 600).ellipsis(), TEXT);
            let hot = self.hover == Hit::Done || self.press == Hit::Done;
            done_button(cx, l.done, self.t("Готово", "Done"), hot);
        });
        if layer {
            unsafe { gfx.dc.PopLayer() };
        }
    }

    pub(super) fn search_scroll_cx(&self, cx: &Cx) -> f32 {
        let l = self.lay();
        let avail = l.search.w - 38.0 - 4.0;
        let chars = &self.st.query.chars;
        let pre: String = chars[..self.st.query.caret.min(chars.len())].iter().collect();
        let cw = cx.measure(&format!("{pre}|"), Style::BODY, 4096.0).0 - cx.measure("|", Style::BODY, 4096.0).0;
        (cw - avail).max(0.0)
    }
}
