//! Ui state and geometry: construction, panels, selection, the settings layout and slider / option values.
use super::*;

impl Ui {
    pub fn new(lang: Lang, s: f32, mw: f32, mh: f32) -> Ui {
        let mut groups: Vec<Group> = Vec::new();
        for d in registry::global().all() {
            match groups.iter_mut().find(|g| g.id == d.group) {
                Some(g) => g.defs.push(d),
                None => groups.push(Group { id: d.group, name: group_name(d.group).unwrap_or(d.name), icon: d.icon, defs: vec![d] }),
            }
        }
        Ui {
            lang,
            lang_pref: crate::i18n::LangPref::Auto,
            s,
            mw,
            mh,
            edit: false,
            gallery: false,
            settings: false,
            anim: 0.0,
            away: 0.0,
            dim: 0.0,
            st: GalState::default(),
            hover: Hit::None,
            press: Hit::None,
            placeholder: None,
            toast: None,
            toast_t: 0.0,
            gal_fade: 1.0e3,
            gal_sig: 0,
            toast_out: None,
            toast_a: 0.0,
            toast_rise: 0.0,
            sa: SetAnim::new(),
            prompt: None,
            menu: None,
            caret_on: true,
            glass: 0.5,
            live: true,
            look: glass::global_params(),
            base: glass::global_params(),
            appearance: AppearancePref::default(),
            dim_mode: Dim::Auto,
            accent_mode: AccentMode::Blue,
            accent_scope: AccentScope::Main,
            mon_count: 1,
            mon_sel: 0,
            autostart: false,
            check_start: false,
            update: UpdateState::Idle,
            sfocus: None,
            gal_off: (0.0, 0.0),
            set_off: (0.0, 0.0),
            groups,
            pv: HashMap::new(),
            pv_bd: HashMap::new(),
        }
    }

    /// New monitor / DPI: previews are rendered for one scale only.
    pub fn set_screen(&mut self, s: f32, mw: f32, mh: f32) {
        if (self.s - s).abs() > 1e-3 {
            self.pv.clear();
            self.pv_bd.clear();
        }
        self.s = s;
        self.mw = mw;
        self.mh = mh;
    }

    pub(super) fn t<'a>(&self, ru: &'a str, en: &'a str) -> &'a str {
        self.lang.pick(ru, en)
    }

    // ------------------------------------------------ gallery content model

    /// The widgets shown right now + the header title.
    pub(super) fn shown(&self) -> (Vec<&'static WidgetDef>, String) {
        let q = self.st.query.text();
        if !q.trim().is_empty() {
            let defs = registry::global().all().iter().filter(|d| grid::matches(&q, &[d.name.0, d.name.1, d.desc.0, d.desc.1])).collect();
            return (defs, self.t("Результаты поиска", "Search results").to_string());
        }
        let g = self.st.group.as_ref().and_then(|id| self.groups.iter().find(|g| g.id == id)).or(self.groups.first());
        match g {
            Some(g) => (g.defs.clone(), self.t(g.name.0, g.name.1).to_string()),
            None => (Vec::new(), String::new()),
        }
    }

    /// Size chips offered by the shown widgets, and the size previewed for all of them.
    pub(super) fn chips_and_size(&self, defs: &[&WidgetDef]) -> (Vec<Size>, Size) {
        let avail: Vec<Size> = Size::ALL.iter().copied().filter(|s| defs.iter().any(|d| d.sizes.contains(s))).collect();
        let size = match self.st.size {
            Some(s) if avail.contains(&s) => s,
            _ => defs.first().map(|d| d.default_size).or(avail.first().copied()).unwrap_or(Size::M),
        };
        (avail, size)
    }

    /// (definition, previewed size) of the k-th preview.
    pub fn item(&self, k: usize) -> Option<(&'static WidgetDef, Size)> {
        let (defs, _) = self.shown();
        let (_, size) = self.chips_and_size(&defs);
        defs.get(k).map(|d| (*d, if d.sizes.contains(&size) { size } else { d.default_size }))
    }

    pub fn select_group(&mut self, i: usize) {
        if let Some(g) = self.groups.get(i) {
            self.st.group = Some(g.id.to_string());
            self.st.query.clear();
            self.st.size = None;
            self.st.scroll = 0.0;
        }
    }
    pub fn select_size(&mut self, s: Size) {
        self.st.size = Some(s);
        self.st.scroll = 0.0;
    }
    pub fn query_changed(&mut self) {
        self.st.scroll = 0.0;
        self.st.size = None;
    }
    /// Wheel over the gallery (notches, + = up). Returns true when something scrolled.
    pub fn wheel(&mut self, lx: f32, ly: f32, delta: f32) -> bool {
        if self.settings {
            return false;
        }
        let lay = self.lay();
        let step = -delta * 54.0;
        if lay.grid.contains(lx, ly) {
            let max = (lay.content_h + 12.0 - lay.grid.h).max(0.0);
            let n = (self.st.scroll + step).clamp(0.0, max);
            let changed = n != self.st.scroll;
            self.st.scroll = n;
            return changed;
        }
        if lay.list.contains(lx, ly) {
            let max = (lay.side_h - lay.list.h).max(0.0);
            let n = (self.st.side_scroll + step).clamp(0.0, max);
            let changed = n != self.st.side_scroll;
            self.st.side_scroll = n;
            return changed;
        }
        false
    }

    // ------------------------------------------------ geometry (logical px)

    /// The open glass panel: the settings panel while it is up, else the gallery.
    pub fn panel_rect(&self) -> Rect {
        if self.settings {
            self.settings_rect()
        } else {
            self.gallery_rect()
        }
    }

    /// Where the gallery opens: bottom left, with room under it for the settings strip.
    pub(super) fn gallery_home(&self) -> Rect {
        let w = PANEL_W.min(self.mw * 0.88);
        let h = PANEL_H.min(self.mh * 0.72);
        Rect::new((self.mw * 0.06).min(self.mw - w - 8.0).max(0.0), self.mh - (self.mh * 0.03).max(10.0) - TB_H - 12.0 - h, w, h)
    }

    /// Keeps a dragged panel on the screen.
    pub(super) fn on_screen(&self, r: Rect, off: (f32, f32)) -> Rect {
        let x = (r.x + off.0).clamp(8.0_f32.min(self.mw - r.w), (self.mw - r.w - 8.0).max(8.0));
        let y = (r.y + off.1).clamp(8.0_f32.min(self.mh - r.h), (self.mh - r.h - 8.0).max(8.0));
        Rect::new(x, y, r.w, r.h)
    }

    pub fn gallery_rect(&self) -> Rect {
        self.on_screen(self.gallery_home(), self.gal_off)
    }

    /// The settings panel opens at the right edge, vertically centred: the cards (top left of the desk) stay visible to judge the look.
    pub(super) fn settings_home(&self) -> Rect {
        let w = SET_W.min(self.mw - 16.0);
        let h = (SET_TOP + (1 + LOOK.len()) as f32 * SET_ROW + 14.0 + SET_FOOT + OPT_ROW * 2.0).min(self.mh - 24.0);
        Rect::new((self.mw - w - (self.mw * 0.04).max(8.0)).max(8.0), ((self.mh - h) / 2.0).max(8.0), w, h)
    }

    pub fn settings_rect(&self) -> Rect {
        self.on_screen(self.settings_home(), self.set_off)
    }

    /// Drag the open panel (the settings panel while it is up, else the gallery) so that its top left is at (x, y), logical px.
    pub fn move_panel(&mut self, x: f32, y: f32) {
        let home = if self.settings { self.settings_home() } else { self.gallery_home() };
        let r = self.on_screen(Rect::new(x, y, home.w, home.h), (0.0, 0.0));
        let off = (r.x - home.x, r.y - home.y);
        if self.settings {
            self.set_off = off;
        } else {
            self.gal_off = off;
        }
    }

    /// The settings strip sits right under the gallery and follows it; with no room below (the gallery was dragged down) it goes above.
    pub fn toolbar_rect(&self) -> Rect {
        let w = TB_W.min(self.mw - 16.0);
        let p = self.gallery_rect();
        let x = (p.x + (p.w - w) / 2.0).clamp(8.0, (self.mw - w - 8.0).max(8.0));
        let y = if p.bottom() + 12.0 + TB_H <= self.mh - 6.0 { p.bottom() + 12.0 } else { (p.y - TB_H - 12.0).max(8.0) };
        Rect::new(x, y, w, TB_H)
    }

    /// Glass slider value for a pointer at canvas px `x`.
    pub fn slider_value(&self, x: i32) -> f32 {
        let t = self.tb_lay().track;
        ((x as f32 / self.s - t.x) / t.w).clamp(0.0, 1.0)
    }

    pub(super) fn tb_lay(&self) -> TbLay {
        let bar = self.toolbar_rect();
        let cy = bar.cy();
        TbLay {
            bar,
            label_glass: Rect::new(bar.x + 18.0, cy - 10.0, 50.0, 20.0),
            track: Rect::new(bar.x + 76.0, cy - 2.0, 136.0, 4.0),
            sep: Rect::new(bar.x + 232.0, cy - 9.0, 1.0, 18.0),
            label_bg: Rect::new(bar.x + 248.0, cy - 10.0, 38.0, 20.0), // wide enough for "Mode" / "Фон" in Medeo Sans with a clear gap to the control
            seg_live: Rect::new(bar.x + 296.0, cy - 12.0, 68.0, 24.0),
            seg_plain: Rect::new(bar.x + 364.0, cy - 12.0, 68.0, 24.0),
            sep2: Rect::new(bar.x + 452.0, cy - 9.0, 1.0, 18.0),
            btn_settings: Rect::new(bar.x + 468.0, cy - 12.0, 92.0, 24.0),
        }
    }

    // ------------------------------------------------ settings panel geometry and values

    pub fn set_lay(&self) -> SetLay {
        let p = self.settings_rect();
        let n = 1 + LOOK.len();
        let top = p.y + SET_TOP;
        let rh = ((p.bottom() - SET_FOOT - 12.0 - top) / n as f32).clamp(22.0, SET_ROW);
        // the right column squeezes its rows on a short screen (the panel is capped at the screen height) so the buttons stay above the footer
        let orow = ((p.bottom() - SET_FOOT - 14.0 - top - 40.0) / (Opt::ALL.len() + 2) as f32).clamp(28.0, OPT_ROW);
        // the first control row of the right column sits on the centre line of the first slider row
        let dy = (rh - 28.0) / 2.0;
        let lx = p.x + 22.0;
        // the left column is as wide as its sliders need; the right one gets the rest, so its 3-segment controls keep ~60 px a segment
        let lw = (p.w * 0.484).round();
        let rx = p.x + lw + 46.0;
        let rw = p.right() - 22.0 - rx;
        let rows = (0..n)
            .map(|i| {
                let row = Rect::new(lx, top + i as f32 * rh, lw, rh);
                SliderRow { row, label: Rect::new(lx, row.y, SET_LABEL_W, rh), track: Rect::new(lx + SET_LABEL_W + 12.0, row.cy() - 2.0, lw - SET_LABEL_W - 12.0 - 56.0 - 12.0, 4.0), value: Rect::new(row.right() - 54.0, row.y, 54.0, rh) }
            })
            .collect();
        let opts = Opt::ALL
            .iter()
            .enumerate()
            .map(|(k, o)| {
                let y = top + k as f32 * orow + dy;
                OptRow { which: *o, label: Rect::new(rx, y, SET_LABEL_W, 28.0), seg: Rect::new(rx + SET_LABEL_W + 12.0, y + 2.0, rw - SET_LABEL_W - 12.0, 24.0) }
            })
            .collect();
        let ay = top + Opt::ALL.len() as f32 * orow + dy;
        let cy = top + (Opt::ALL.len() + 1) as f32 * orow + dy;
        let by = top + (Opt::ALL.len() + 2) as f32 * orow + 10.0;
        let bw = ((rw - 8.0) * 0.6).round();
        let fy = p.bottom() - SET_FOOT + 13.0;
        let done = Rect::new(p.right() - 16.0 - 82.0, fy, 82.0, 30.0);
        let upd_btn = Rect::new(p.x + 22.0, fy, 176.0, 30.0);
        let upd_dl = Rect::new(done.x - 8.0 - 120.0, fy, 120.0, 30.0);
        let upd_page = Rect::new(upd_dl.x - 8.0 - 116.0, fy, 116.0, 30.0);
        let sx = upd_btn.right() + 12.0;
        SetLay {
            panel: p,
            title: Rect::new(p.x + 22.0, p.y + 14.0, 300.0, 28.0),
            look_hdr: Rect::new(lx, p.y + 46.0, lw - 124.0, 24.0),
            beh_hdr: Rect::new(rx, p.y + 46.0, 200.0, 24.0),
            reset: Rect::new(lx + lw - 108.0, p.y + 46.0, 108.0, 24.0),
            sep: Rect::new(p.x + lw + 34.0, p.y + 50.0, 0.8, p.h - SET_FOOT - 58.0),
            rows,
            opts,
            auto_row: Rect::new(rx, ay, rw, 28.0),
            auto_sw: Rect::new(rx + rw - 44.0, ay + 2.0, 44.0, 24.0),
            check_row: Rect::new(rx, cy, rw, 28.0),
            check_sw: Rect::new(rx + rw - 44.0, cy + 2.0, 44.0, 24.0),
            edit_btn: Rect::new(rx, by, bw, 30.0),
            quit_btn: Rect::new(rx + bw + 8.0, by, rw - bw - 8.0, 30.0),
            keys: Rect::new(rx, by + 44.0, rw, (p.bottom() - SET_FOOT - 10.0 - (by + 44.0)).max(0.0)),
            done,
            upd_btn,
            upd_status: Rect::new(sx, p.bottom() - SET_FOOT, upd_page.x - 8.0 - sx, SET_FOOT),
            upd_page,
            upd_dl,
            foot: Rect::new(p.x + 14.0, p.bottom() - SET_FOOT, p.w - 28.0, 0.8),
        }
    }

    /// What the settings panel has at (lx, ly) (logical px); `Panel` for the empty parts.
    pub fn set_hit(&self, lx: f32, ly: f32) -> Hit {
        let l = self.set_lay();
        if !l.panel.contains(lx, ly) {
            return Hit::None;
        }
        if l.done.contains(lx, ly) {
            return Hit::Done;
        }
        if l.reset.contains(lx, ly) {
            return Hit::ResetLook;
        }
        if l.upd_btn.contains(lx, ly) {
            return Hit::UpdBtn;
        }
        if matches!(self.update, UpdateState::Available { .. }) {
            if l.upd_dl.contains(lx, ly) {
                return Hit::UpdInstall;
            }
            if l.upd_page.contains(lx, ly) {
                return Hit::UpdPage;
            }
        }
        if let Some(i) = l.rows.iter().position(|r| r.track.inset_xy(-12.0, -(r.row.h / 2.0 - 2.0)).contains(lx, ly)) {
            return Hit::Look(i, ((lx - l.rows[i].track.x) / l.rows[i].track.w).clamp(0.0, 1.0));
        }
        for o in &l.opts {
            if o.seg.contains(lx, ly) {
                let n = self.opt_n(o.which);
                let k = (((lx - o.seg.x) / o.seg.w) * n as f32) as usize;
                return Hit::Opt(o.which, k.min(n - 1));
            }
        }
        if l.auto_row.contains(lx, ly) {
            return Hit::Autostart;
        }
        if l.check_row.contains(lx, ly) {
            return Hit::CheckStart;
        }
        if l.edit_btn.contains(lx, ly) {
            return Hit::EditBtn;
        }
        if l.quit_btn.contains(lx, ly) {
            return Hit::QuitBtn;
        }
        Hit::Panel
    }

    /// Slider `i` of the settings panel at canvas px `x`, as 0..1.
    pub fn set_value_at(&self, i: usize, x: i32) -> f32 {
        let t = self.set_lay().rows[i].track;
        ((x as f32 / self.s - t.x) / t.w).clamp(0.0, 1.0)
    }

    /// Position 0..1 of slider `i` (0 = Glass) for the values the panel currently shows.
    pub fn set_pos(&self, i: usize) -> f32 {
        match i {
            0 => self.glass.clamp(0.0, 1.0),
            _ => LOOK[i - 1].pos(&self.look),
        }
    }

    /// Where slider `i` sits at the base look (the small tick under the track).
    pub fn set_default_pos(&self, i: usize) -> f32 {
        match i {
            0 => 0.5,
            _ => LOOK[i - 1].pos(&self.base),
        }
    }

    /// The value text of slider `i`: Glass in %, Blur in px, Corners as the radius in px, the rest relative to the base look in %.
    pub fn set_value_text(&self, i: usize) -> String {
        match i {
            0 => format!("{:.0}%", self.glass * 100.0),
            _ => {
                let d = &LOOK[i - 1];
                match d.primary {
                    "blur" => format!("{:.1} px", self.look.blur).replace('.', if self.lang == Lang::Ru { "," } else { "." }),
                    "radius_k" => format!("{:.0} px", Size::S.radius() * self.look.radius_k),
                    "card_k" => format!("{:.0}%", self.look.card_k * 100.0),
                    _ => {
                        // a wide slider can reach many times the base look: "x10" reads better than "1000%"
                        let r = d.ratio(&self.look, &self.base);
                        if r >= 9.95 { format!("x{:.0}", r) } else { format!("{:.0}%", r * 100.0) }
                    }
                }
            }
        }
    }

    /// The k-th control of the settings panel in reading order (headless `--hover k`): the sliders, "Reset look", one option of each
    /// segmented control, the switches, "Edit widgets", "Quit", "Check for updates", "Update now", "Release page", "Done".
    pub fn nth_control(&self, k: usize) -> Hit {
        let n = 1 + LOOK.len();
        if k < n {
            return Hit::Look(k, self.set_pos(k));
        }
        [Hit::ResetLook, Hit::Opt(Opt::Appearance, 1), Hit::Opt(Opt::Dim, 0), Hit::Opt(Opt::Source, 1), Hit::Opt(Opt::Accent, 1), Hit::Opt(Opt::Scope, 1), Hit::Opt(Opt::Monitor, 0), Hit::Opt(Opt::Language, 1), Hit::Autostart, Hit::CheckStart, Hit::EditBtn, Hit::QuitBtn, Hit::UpdBtn, Hit::UpdInstall, Hit::UpdPage, Hit::Done].get(k - n).copied().unwrap_or(Hit::None)
    }

    // ------------------------------------------------ keyboard / wheel on the sliders

    /// How many sliders the keyboard can reach: the settings panel's rows, or the glass slider of the strip.
    pub fn slider_count(&self) -> usize {
        if self.settings {
            1 + LOOK.len()
        } else {
            1
        }
    }

    /// The slider row of the settings panel under (lx, ly): the whole row counts for the wheel, not just the track.
    pub fn row_at(&self, lx: f32, ly: f32) -> Option<usize> {
        self.set_lay().rows.iter().position(|r| r.row.contains(lx, ly))
    }

    /// A key for the sliders. Up / Down / Tab / Shift+Tab move the focus (`SAct::Focus`); Left / Right (1 %, Shift 10 %), Home / End and
    /// Backspace / Delete (back to the base look) give the new position of the focused slider. None = nothing to do.
    pub fn slider_key(&mut self, k: SKey, shift: bool) -> Option<SAct> {
        let n = self.slider_count();
        let f = self.sfocus.filter(|i| *i < n);
        if matches!(k, SKey::Up | SKey::Down | SKey::Tab) {
            let back = k == SKey::Up || (k == SKey::Tab && shift);
            let to = match (f, back) {
                (None, false) => 0,
                (None, true) => n - 1,
                (Some(i), false) => (i + 1) % n,
                (Some(i), true) => (i + n - 1) % n,
            };
            self.sfocus = Some(to);
            return Some(SAct::Focus);
        }
        let i = f?;
        let step = if shift { 0.10 } else { 0.01 };
        let p = self.set_pos(i);
        let t = match k {
            SKey::Left => p - step,
            SKey::Right => p + step,
            SKey::Home => 0.0,
            SKey::End => 1.0,
            _ => self.set_default_pos(i),
        };
        Some(SAct::Set(i, t.clamp(0.0, 1.0)))
    }

    /// The selected option of a segmented control.
    pub fn opt_sel(&self, o: Opt) -> usize {
        match o {
            Opt::Appearance => [AppearancePref::Dark, AppearancePref::Light, AppearancePref::Auto].iter().position(|a| *a == self.appearance).unwrap_or(0),
            Opt::Dim => [Dim::Auto, Dim::Never, Dim::Always].iter().position(|d| *d == self.dim_mode).unwrap_or(0),
            Opt::Source => usize::from(!self.live),
            Opt::Accent => usize::from(self.accent_mode == AccentMode::Wallpaper),
            Opt::Scope => usize::from(self.accent_scope == AccentScope::All),
            Opt::Monitor => self.mon_sel.min(self.opt_n(o) - 1),
            Opt::Language => self.lang_pref.index(),
        }
    }

    /// Number of segments of a control: fixed, except `Monitor` (one per connected monitor, 1..=MON_SEGS).
    pub fn opt_n(&self, o: Opt) -> usize {
        match o {
            Opt::Monitor => self.mon_count.clamp(1, MON_SEGS),
            _ => o.count(),
        }
    }

    /// The language setting changed: texts come from `lang` now; the cached gallery previews (each a scene of its own) are drawn again in it.
    pub fn set_language(&mut self, pref: crate::i18n::LangPref, lang: Lang) {
        self.lang_pref = pref;
        self.lang = lang;
        self.pv.clear();
    }

    pub fn lay(&self) -> Lay {
        let p = self.gallery_rect();
        let search = Rect::new(p.x + 12.0, p.y + 12.0, SIDE_W - 22.0, 30.0);
        let list = Rect::new(p.x + 6.0, p.y + 50.0, SIDE_W - 10.0, (p.h - FOOT_H - 54.0).max(0.0));
        let rows: Vec<Rect> = (0..self.groups.len()).map(|i| Rect::new(list.x + 4.0, list.y + 2.0 + i as f32 * 34.0 - self.st.side_scroll, list.w - 8.0, 32.0)).collect();
        let side_h = self.groups.len() as f32 * 34.0 + 4.0;
        let main_x = p.x + SIDE_W + 18.0;
        let main_w = p.w - SIDE_W - 36.0;
        let (defs, _) = self.shown();
        let (avail, size) = self.chips_and_size(&defs);
        let title = Rect::new(main_x, p.y + 14.0, main_w - 130.0, 28.0);
        let chip_w = 34.0;
        let chips_w = avail.len() as f32 * chip_w + 4.0;
        let chips: Vec<(Size, Rect)> = avail.iter().enumerate().map(|(i, s)| (*s, Rect::new(p.x + p.w - 18.0 - chips_w + 2.0 + i as f32 * chip_w, p.y + 16.0, chip_w, 24.0))).collect();
        let grid = Rect::new(main_x - 6.0, p.y + 50.0, main_w + 12.0, (p.h - FOOT_H - 54.0).max(0.0));
        let (gx, gy) = (6.0, 6.0);
        let area_w = grid.w - 12.0;
        let (mut x, mut y, mut row_h) = (0.0f32, 0.0f32, 0.0f32);
        let mut items = Vec::new();
        for (k, d) in defs.iter().enumerate() {
            let sz = if d.sizes.contains(&size) { size } else { d.default_size };
            let (lw, lh) = sz.logical();
            let (pw, ph) = (lw * K, lh * K);
            let iw = pw.clamp(130.0, 230.0);
            let ih = ph + 8.0 + 17.0 + 32.0;
            if x > 0.0 && x + iw > area_w {
                x = 0.0;
                y += row_h + 20.0;
                row_h = 0.0;
            }
            let (ox, oy) = (grid.x + 6.0 + x, grid.y + gy + y - self.st.scroll);
            let cell = Rect::new(ox, oy, iw, ih);
            items.push(ItemLay {
                k,
                size: sz,
                pv: Rect::new(ox + (iw - pw) / 2.0, oy, pw, ph),
                title: Rect::new(ox, oy + ph + 8.0, iw, 17.0),
                desc: Rect::new(ox, oy + ph + 26.0, iw, 32.0),
                cell,
            });
            x += iw + 28.0 + gx - 6.0;
            row_h = row_h.max(ih);
        }
        let content_h = y + row_h + gy;
        Lay {
            panel: p,
            search,
            list,
            rows,
            side_h,
            title,
            chips,
            grid,
            items,
            content_h,
            done: Rect::new(p.right() - 16.0 - 82.0, p.bottom() - FOOT_H + 14.0, 82.0, 30.0),
            hint: Rect::new(p.x + 18.0, p.bottom() - FOOT_H, p.w - 130.0, FOOT_H),
        }
    }

    // ------------------------------------------------ hit testing

    /// "−" button centre of a card (canvas px), following the card's rotation / scale.
    pub(super) fn minus_center(&self, scene: &Scene, i: usize) -> (f32, f32) {
        let c = &scene.cards[i];
        let s = self.s;
        let (cx, cy) = (c.x as f32 + c.w as f32 / 2.0, c.y as f32 + c.h as f32 / 2.0);
        let (px, py) = (c.x as f32 + 4.0 * s, c.y as f32 + 4.0 * s);
        let (sn, cs) = c.xf.rot.to_radians().sin_cos();
        let (dx, dy) = ((px - cx) * c.xf.scale, (py - cy) * c.xf.scale);
        (cx + dx * cs - dy * sn, cy + dx * sn + dy * cs)
    }
}
