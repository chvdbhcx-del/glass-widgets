//! Host UI drawn on top of the cards while the desk is being edited: the dim, the placeholder under a dragged card, the "−" buttons, the
//! settings strip (glass slider, backdrop mode), the "Edit Widgets" gallery (search, group sidebar, size chips, LIVE previews, Done) and a
//! toast. Everything is laid out in LOGICAL px (absolute screen coordinates) and drawn through `Cx` inside the scene's open frame session.
//! The gallery previews are tiny one-card `Scene`s in preview mode (the same draw / overlay code as the desk) cached per (widget, size).
use crate::cards::*;
use crate::gfx::*;
use crate::glass::GlassStyle;
use crate::grid;
use crate::registry::{self, WidgetDef};
use crate::widget::*;
use std::collections::HashMap;
use windows::Foundation::Numerics::Matrix3x2;
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;

/// preview scale (gallery card = this fraction of the real card)
pub const K: f32 = 0.6;
const PANEL_W: f32 = 780.0;
const PANEL_H: f32 = 430.0;
const PANEL_R: f32 = 26.0;
const SIDE_W: f32 = 200.0;
const FOOT_H: f32 = 58.0;
const TB_W: f32 = 440.0;
const TB_H: f32 = 40.0;
const MINUS_R: f32 = 13.0;
pub const SLOT_GALLERY: usize = 0;
pub const SLOT_TOOLBAR: usize = 1;

// ---------------------------------------------------------------- text input

/// A single-line text field model: caret, backspace / delete / arrows / home / end, paste, select-all.
#[derive(Default, Clone, Debug)]
pub struct TextInput {
    pub chars: Vec<char>,
    pub caret: usize,
    pub sel_all: bool,
}
impl TextInput {
    const MAX: usize = 64;
    pub fn text(&self) -> String {
        self.chars.iter().collect()
    }
    fn take_sel(&mut self) {
        if self.sel_all {
            self.chars.clear();
            self.caret = 0;
            self.sel_all = false;
        }
    }
    pub fn insert(&mut self, c: char) {
        if c.is_control() {
            return;
        }
        self.take_sel();
        if self.chars.len() < Self::MAX {
            self.chars.insert(self.caret, c);
            self.caret += 1;
        }
    }
    pub fn insert_str(&mut self, s: &str) {
        for c in s.chars() {
            self.insert(if c == '\n' || c == '\r' || c == '\t' { ' ' } else { c });
        }
    }
    pub fn backspace(&mut self) {
        if self.sel_all {
            return self.take_sel();
        }
        if self.caret > 0 {
            self.caret -= 1;
            self.chars.remove(self.caret);
        }
    }
    pub fn delete(&mut self) {
        if self.sel_all {
            return self.take_sel();
        }
        if self.caret < self.chars.len() {
            self.chars.remove(self.caret);
        }
    }
    pub fn left(&mut self) {
        self.sel_all = false;
        self.caret = self.caret.saturating_sub(1);
    }
    pub fn right(&mut self) {
        self.sel_all = false;
        self.caret = (self.caret + 1).min(self.chars.len());
    }
    pub fn home(&mut self) {
        self.sel_all = false;
        self.caret = 0;
    }
    pub fn end(&mut self) {
        self.sel_all = false;
        self.caret = self.chars.len();
    }
    pub fn clear(&mut self) {
        *self = TextInput::default();
    }
    pub fn select_all(&mut self) {
        self.sel_all = !self.chars.is_empty();
        self.caret = self.chars.len();
    }
    pub fn set_caret(&mut self, i: usize) {
        self.sel_all = false;
        self.caret = i.min(self.chars.len());
    }
}

// ---------------------------------------------------------------- model

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Hit {
    None,
    /// inside the gallery panel, nothing clickable
    Panel,
    /// the search field (logical x for the caret)
    Search(f32),
    Group(usize),
    Chip(Size),
    /// the k-th preview of the current list
    Item(usize),
    Done,
    /// settings strip: glass slider (value 0..1), backdrop mode (true = live), the rest of the strip
    Slider(f32),
    Seg(bool),
    Toolbar,
    /// the "−" of card index i
    Minus(usize),
    Card(usize),
}

#[derive(Default)]
pub struct GalState {
    pub group: Option<String>,
    pub size: Option<Size>,
    pub query: TextInput,
    pub focus: bool,
    pub scroll: f32,
    pub side_scroll: f32,
}

struct Group {
    id: &'static str,
    name: (&'static str, &'static str),
    icon: Glyph,
    defs: Vec<&'static WidgetDef>,
}

/// A cached live preview: a one-card scene in preview mode.
struct Pv {
    scene: Scene,
    next_poll: std::time::Instant,
    /// static previews are re-ticked now and then (a clock face shows the time it was opened at otherwise)
    next_static: std::time::Instant,
}

pub struct Ui {
    pub lang: Lang,
    pub s: f32,
    /// monitor size, logical px
    pub mw: f32,
    pub mh: f32,
    pub edit: bool,
    pub gallery: bool,
    /// open / close progress 0..1 of the gallery + settings strip (eased by the host)
    pub anim: f32,
    /// 0..1: the gallery fades while a card is dragged out of it
    pub away: f32,
    /// alpha of the black veil over the desktop
    pub dim: f32,
    pub st: GalState,
    pub hover: Hit,
    pub press: Hit,
    /// grid cell under a dragged card: canvas px rect + corner radius px
    pub placeholder: Option<(RECT, f32)>,
    pub toast: Option<String>,
    pub caret_on: bool,
    /// values shown by the settings strip
    pub glass: f32,
    pub live: bool,
    groups: Vec<Group>,
    pv: HashMap<(&'static str, Size), Pv>,
    pv_bd: HashMap<(i32, i32), ID2D1Bitmap1>,
}

/// Per-group sidebar icon colours (top, bottom).
fn group_colors(id: &str) -> (Color, Color) {
    let c = |r: u8, g: u8, b: u8| Color::rgb8(r, g, b);
    match id {
        "clock" => (c(70, 70, 76), c(28, 28, 32)),
        "calendar" => (c(255, 96, 86), c(235, 52, 44)),
        "reminders" => (c(64, 156, 255), c(10, 110, 235)),
        "music" => (c(255, 82, 112), c(235, 40, 78)),
        "batteries" => (c(76, 217, 100), c(40, 180, 70)),
        "weather" => (c(88, 190, 250), c(30, 120, 230)),
        "system" => (c(150, 110, 240), c(100, 70, 210)),
        "notes" => (c(255, 224, 80), c(250, 190, 20)),
        "controls" => (c(142, 142, 150), c(100, 100, 108)),
        "utilities" => (c(255, 170, 60), c(250, 130, 20)),
        "photos" => (c(255, 140, 120), c(240, 90, 140)),
        "info" => (c(80, 210, 230), c(40, 160, 200)),
        _ => (c(120, 130, 150), c(80, 90, 110)),
    }
}

fn group_name(id: &str) -> Option<(&'static str, &'static str)> {
    Some(match id {
        "clock" => ("Часы", "Clock"),
        "calendar" => ("Календарь", "Calendar"),
        "reminders" => ("Напоминания", "Reminders"),
        "music" => ("Музыка", "Music"),
        "batteries" => ("Батареи", "Batteries"),
        "weather" => ("Погода", "Weather"),
        "system" => ("Система", "System"),
        "notes" => ("Заметки", "Notes"),
        "controls" => ("Управление", "Controls"),
        "utilities" => ("Утилиты", "Utilities"),
        "photos" => ("Фото", "Photos"),
        "info" => ("Информация", "Info"),
        _ => return None,
    })
}

fn mul(a: &Matrix3x2, b: &Matrix3x2) -> Matrix3x2 {
    Matrix3x2 {
        M11: a.M11 * b.M11 + a.M12 * b.M21,
        M12: a.M11 * b.M12 + a.M12 * b.M22,
        M21: a.M21 * b.M11 + a.M22 * b.M21,
        M22: a.M21 * b.M12 + a.M22 * b.M22,
        M31: a.M31 * b.M11 + a.M32 * b.M21 + b.M31,
        M32: a.M31 * b.M12 + a.M32 * b.M22 + b.M32,
    }
}

/// Layout of one preview in the gallery grid (logical px; absolute screen coordinates, scroll applied).
#[derive(Clone, Copy, Debug)]
pub struct ItemLay {
    pub k: usize,
    pub size: Size,
    pub pv: Rect,
    pub title: Rect,
    pub desc: Rect,
    pub cell: Rect,
}

pub struct Lay {
    pub panel: Rect,
    pub search: Rect,
    pub list: Rect,
    pub rows: Vec<Rect>,
    pub side_h: f32,
    pub title: Rect,
    pub chips: Vec<(Size, Rect)>,
    pub grid: Rect,
    pub items: Vec<ItemLay>,
    pub content_h: f32,
    pub done: Rect,
    pub hint: Rect,
}

pub struct TbLay {
    pub bar: Rect,
    pub label_glass: Rect,
    pub track: Rect,
    pub sep: Rect,
    pub label_bg: Rect,
    pub seg_live: Rect,
    pub seg_plain: Rect,
}

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
            s,
            mw,
            mh,
            edit: false,
            gallery: false,
            anim: 0.0,
            away: 0.0,
            dim: 0.0,
            st: GalState::default(),
            hover: Hit::None,
            press: Hit::None,
            placeholder: None,
            toast: None,
            caret_on: true,
            glass: 0.5,
            live: true,
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

    fn t<'a>(&self, ru: &'a str, en: &'a str) -> &'a str {
        self.lang.pick(ru, en)
    }

    // ------------------------------------------------ gallery content model

    /// The widgets shown right now + the header title.
    fn shown(&self) -> (Vec<&'static WidgetDef>, String) {
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
    fn chips_and_size(&self, defs: &[&WidgetDef]) -> (Vec<Size>, Size) {
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

    pub fn panel_rect(&self) -> Rect {
        let w = PANEL_W.min(self.mw * 0.88);
        let h = PANEL_H.min(self.mh * 0.72);
        Rect::new((self.mw * 0.06).min(self.mw - w - 8.0).max(0.0), self.mh - self.mh * 0.06 - h, w, h)
    }

    /// The settings strip sits at the bottom, right of the gallery and level with its lower edge: clear of the card grid and of the top bar.
    pub fn toolbar_rect(&self) -> Rect {
        let w = TB_W.min(self.mw - 16.0);
        let p = self.panel_rect();
        if p.right() + 24.0 + w + 16.0 <= self.mw {
            Rect::new(p.right() + 24.0, p.bottom() - TB_H, w, TB_H)
        } else {
            // a narrow screen: the strip sits right above the gallery instead
            Rect::new(p.x, (p.y - TB_H - 12.0).max(8.0), w, TB_H)
        }
    }

    /// Glass slider value for a pointer at canvas px `x`.
    pub fn slider_value(&self, x: i32) -> f32 {
        let t = self.tb_lay().track;
        ((x as f32 / self.s - t.x) / t.w).clamp(0.0, 1.0)
    }

    fn tb_lay(&self) -> TbLay {
        let bar = self.toolbar_rect();
        let cy = bar.cy();
        TbLay {
            bar,
            label_glass: Rect::new(bar.x + 18.0, cy - 10.0, 50.0, 20.0),
            track: Rect::new(bar.x + 76.0, cy - 2.0, 136.0, 4.0),
            sep: Rect::new(bar.x + 232.0, cy - 9.0, 1.0, 18.0),
            label_bg: Rect::new(bar.x + 248.0, cy - 10.0, 32.0, 20.0),
            seg_live: Rect::new(bar.x + 286.0, cy - 12.0, 68.0, 24.0),
            seg_plain: Rect::new(bar.x + 354.0, cy - 12.0, 68.0, 24.0),
        }
    }

    pub fn lay(&self) -> Lay {
        let p = self.panel_rect();
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
    fn minus_center(&self, scene: &Scene, i: usize) -> (f32, f32) {
        let c = &scene.cards[i];
        let s = self.s;
        let (cx, cy) = (c.x as f32 + c.w as f32 / 2.0, c.y as f32 + c.h as f32 / 2.0);
        let (px, py) = (c.x as f32 + 4.0 * s, c.y as f32 + 4.0 * s);
        let (sn, cs) = c.xf.rot.to_radians().sin_cos();
        let (dx, dy) = ((px - cx) * c.xf.scale, (py - cy) * c.xf.scale);
        (cx + dx * cs - dy * sn, cy + dx * sn + dy * cs)
    }

    /// What is under the canvas-px point (x, y).
    pub fn hit(&self, scene: &Scene, x: i32, y: i32) -> Hit {
        let (lx, ly) = (x as f32 / self.s, y as f32 / self.s);
        if self.edit {
            let tb = self.tb_lay();
            if tb.bar.contains(lx, ly) {
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

    fn search_scroll(&self, scene: &Scene, gfx: &Gfx) -> f32 {
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

    fn bd_for(&mut self, gfx: &Gfx, w: i32, h: i32) -> Result<ID2D1Bitmap1> {
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

    fn ensure_pv(&mut self, gfx: &Gfx, def: &'static WidgetDef, size: Size, now: &Now) -> Result<()> {
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
        let g = if self.edit && self.gallery { Some(spec(self.panel_rect(), PANEL_R)) } else { None };
        let t = if self.edit { Some(spec(self.toolbar_rect(), TB_H / 2.0)) } else { None };
        for (slot, sp) in [(SLOT_GALLERY, g), (SLOT_TOOLBAR, t)] {
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

    /// Canvas px rectangle the UI occupies (gallery with its shadow margin + strip + toast) for dirty tracking.
    pub fn dirty_rect(&self, scene: &Scene) -> Option<RECT> {
        if !self.edit {
            return None;
        }
        let s = self.s;
        let m = scene.margin() + (30.0 * s) as i32;
        let r = |r: Rect| RECT { left: (r.x * s) as i32 - m, top: (r.y * s) as i32 - m, right: (r.right() * s).ceil() as i32 + m, bottom: (r.bottom() * s).ceil() as i32 + m };
        let mut d = r(self.toolbar_rect());
        if self.gallery {
            d = union(d, r(self.panel_rect()));
        }
        Some(clip(d, scene.w, scene.h))
    }

    // ------------------------------------------------ drawing

    /// Under the cards: the dim over the desktop and the placeholder of the cell a card is about to land in.
    pub fn draw_pre(&self, gfx: &Gfx, clip_px: RECT) {
        let dc = &gfx.dc;
        unsafe {
            if self.dim > 0.002 {
                if let Ok(b) = dc.CreateSolidColorBrush(&col(0.0, 0.0, 0.0, self.dim), None) {
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
        if self.edit {
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
        if let Some(t) = &self.toast {
            scene.with_ui_cx(gfx, base, self.lang, now, |cx| {
                let (tw, _) = cx.measure(t, Style::new(13.0, 500), 600.0);
                let r = Rect::new((self.mw - tw - 36.0) / 2.0, self.mh * 0.5 - 18.0, tw + 36.0, 36.0);
                cx.rrect(r.offset(0.0, 3.0), 18.0, Color::rgba(0.0, 0.0, 0.0, 0.18));
                cx.pill(r, Color::rgba(0.11, 0.11, 0.14, 0.92));
                cx.rrect_stroke(r, 18.0, 0.8, Color::rgba(1.0, 1.0, 1.0, 0.18));
                cx.text(r, t, Style::new(13.0, 500).center(), TEXT);
            });
        }
    }

    fn draw_minus(&self, cx: &Cx, c: Pt, hot: bool, alpha: f32) {
        for (i, a) in [0.06f32, 0.07, 0.09].iter().enumerate() {
            cx.circle((c.0, c.1 + 2.0), MINUS_R + 3.0 - i as f32 * 1.4, Color::rgba(0.0, 0.0, 0.0, a * alpha));
        }
        let bg = if hot { Color::rgba(1.0, 0.27, 0.23, 0.97) } else { Color::rgba(0.24, 0.24, 0.26, 0.96) };
        cx.circle(c, MINUS_R, bg.fade(alpha));
        cx.circle_stroke(c, MINUS_R - 0.6, 1.0, Color::rgba(1.0, 1.0, 1.0, 0.24 * alpha));
        cx.line((c.0 - 5.2, c.1), (c.0 + 5.2, c.1), 2.2, WHITE.fade(alpha));
    }

    fn seg(&self, cx: &Cx, r: Rect, labels: &[(&str, bool)], pad: f32) {
        cx.pill(r, FILL);
        for (i, (txt, on)) in labels.iter().enumerate() {
            let n = labels.len() as f32;
            let c = Rect::new(r.x + pad + (r.w - 2.0 * pad) / n * i as f32, r.y + pad, (r.w - 2.0 * pad) / n, r.h - 2.0 * pad);
            if *on {
                cx.pill(c.offset(0.0, 1.0), Color::rgba(0.0, 0.0, 0.0, 0.18));
                cx.pill(c, Color::rgba(1.0, 1.0, 1.0, 0.92));
            }
            cx.text(c, txt, Style::new(11.5, 600).center(), if *on { Color::rgba(0.07, 0.07, 0.09, 1.0) } else { TEXT });
        }
    }

    /// Open / close transform of the gallery and the strip: slide up 26 px, scale .97 -> 1 about the bottom-left corner.
    fn open_xf(&self) -> Matrix3x2 {
        let t = self.anim.clamp(0.0, 1.0);
        let k = 0.97 + 0.03 * t;
        let p = self.panel_rect();
        let (ax, ay) = (p.x * self.s, p.bottom() * self.s);
        Matrix3x2 { M11: k, M12: 0.0, M21: 0.0, M22: k, M31: ax - k * ax, M32: ay - k * ay + (1.0 - t) * 26.0 * self.s }
    }

    fn push_alpha(&self, gfx: &Gfx, a: f32) -> bool {
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

    fn draw_toolbar(&self, scene: &Scene, gfx: &Gfx, now: &Now) {
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
            cx.rect(tb.sep, FILL);
            cx.text(tb.label_bg, self.t("Фон", "Back"), Style::new(12.5, 500), TEXT);
            let seg = Rect::new(tb.seg_live.x, tb.seg_live.y, tb.seg_live.w + tb.seg_plain.w, tb.seg_live.h);
            self.seg(cx, seg, &[(self.t("Живое", "Live"), self.live), (self.t("Простой", "Plain"), !self.live)], 2.0);
        });
        if layer {
            unsafe { gfx.dc.PopLayer() };
        }
    }

    fn draw_gallery(&self, scene: &Scene, gfx: &Gfx, now: &Now) {
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
                cx.rrect_stroke(l.search.inset(-2.0), 11.0, 3.0, BLUE.alpha(0.30));
                cx.rrect_stroke(l.search, 9.0, 1.6, BLUE.alpha(0.95));
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
                    cx.rrect(Rect::new(text_area.x - off - 1.0, text_area.cy() - 9.0, tw + 2.0, 18.0), 4.0, BLUE.alpha(0.55));
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
                    cx.rrect(r, 9.0, Color::rgba(10.0 / 255.0, 132.0 / 255.0, 1.0, 0.92));
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
                self.seg(cx, r, &lab, 2.0);
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
                    cx.circle(b, 11.0, Color::rgba(10.0 / 255.0, 132.0 / 255.0, 1.0, 0.97));
                    cx.line((b.0 - 4.6, b.1), (b.0 + 4.6, b.1), 1.9, WHITE);
                    cx.line((b.0, b.1 - 4.6), (b.0, b.1 + 4.6), 1.9, WHITE);
                }
                cx.text(it.title, self.t(def.name.0, def.name.1), Style::new(12.5, 600).center().ellipsis(), TEXT);
                cx.clip(it.desc);
                cx.text(it.desc, self.t(def.desc.0, def.desc.1), Style::new(11.5, 400).center().wrap(), DIM);
                cx.pop_clip();
            }
            cx.pop_clip();

            // footer: hint + Done
            cx.rect(Rect::new(p.x + 14.0, p.bottom() - FOOT_H, p.w - 28.0, 0.8), FILL);
            cx.text(l.hint, self.t("Перетащите виджет на рабочий стол…", "Drag a widget to place it on the desktop…"), Style::new(12.5, 600).ellipsis(), TEXT);
            let hot = self.hover == Hit::Done || self.press == Hit::Done;
            cx.pill(l.done.offset(0.0, 2.0).inset_xy(2.0, 0.0), BLUE.alpha(0.40));
            cx.pill(l.done, if hot { Color::rgb8(43, 149, 255) } else { Color::rgb8(10, 132, 255) });
            cx.text(l.done, self.t("Готово", "Done"), Style::new(13.0, 600).center(), WHITE);
        });
        if layer {
            unsafe { gfx.dc.PopLayer() };
        }
    }

    fn search_scroll_cx(&self, cx: &Cx) -> f32 {
        let l = self.lay();
        let avail = l.search.w - 38.0 - 4.0;
        let chars = &self.st.query.chars;
        let pre: String = chars[..self.st.query.caret.min(chars.len())].iter().collect();
        let cw = cx.measure(&format!("{pre}|"), Style::BODY, 4096.0).0 - cx.measure("|", Style::BODY, 4096.0).0;
        (cw - avail).max(0.0)
    }
}

// ---------------------------------------------------------------- edit-mode motion helpers (shared by the host and --dump-ui)

/// Jiggle rotation (degrees) of the `i`-th card at time `t` seconds: +-1.2 degrees, a distinct phase per card.
pub fn jiggle(i: usize, t: f32) -> f32 {
    let phase = (i as f32 * 1.7 + 0.4).fract() * std::f32::consts::TAU + i as f32 * 2.1;
    1.2 * (t * std::f32::consts::TAU / 0.8 + phase).sin()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_input_edits() {
        let mut t = TextInput::default();
        t.insert_str("ab\nc");
        assert_eq!(t.text(), "ab c");
        t.left();
        t.left();
        t.insert('X');
        assert_eq!(t.text(), "abX c");
        assert_eq!(t.caret, 3);
        t.backspace();
        t.delete();
        assert_eq!(t.text(), "abc");
        t.home();
        t.delete();
        assert_eq!(t.text(), "bc");
        t.end();
        t.insert('é');
        assert_eq!(t.text(), "bcé");
        t.select_all();
        t.insert('z');
        assert_eq!(t.text(), "z");
        t.select_all();
        t.backspace();
        assert_eq!(t.text(), "");
        t.insert('\u{7}'); // control characters are ignored
        assert_eq!(t.text(), "");
        for _ in 0..100 {
            t.insert('a');
        }
        assert_eq!(t.chars.len(), 64);
    }

    #[test]
    fn jiggle_is_bounded_and_phased() {
        for i in 0..8 {
            for k in 0..40 {
                assert!(jiggle(i, k as f32 * 0.05).abs() <= 1.2001);
            }
        }
        assert_ne!(jiggle(0, 0.1), jiggle(1, 0.1));
    }

    #[test]
    fn panel_sits_bottom_left() {
        let ui = Ui::new(Lang::Ru, 1.0, 1920.0, 1080.0);
        let p = ui.panel_rect();
        assert_eq!((p.w, p.h), (780.0, 430.0));
        assert!((p.x - 115.2).abs() < 0.1 && (p.bottom() - (1080.0 - 64.8)).abs() < 0.1);
        let small = Ui::new(Lang::Ru, 1.0, 800.0, 500.0).panel_rect();
        assert!(small.w <= 800.0 * 0.88 + 0.01 && small.h <= 360.1 && small.right() <= 800.0);
    }

    /// The hit map of edit mode on a real scene: card body, "−" button, gallery controls, previews, settings strip, empty desk.
    #[test]
    fn edit_mode_hit_map() {
        use crate::backdrop::{Backdrop, PlainBackdrop};
        unsafe {
            let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_MULTITHREADED);
        }
        let gfx = Gfx::new(None).unwrap();
        let bd = PlainBackdrop::new(&gfx, 1600, 900).unwrap();
        let env = SceneEnv { preview: true, hwnd: 0, lang: Lang::Ru, utc_off: 0 };
        let specs = [
            CardSpec { id: "clock-a".into(), type_id: "clock".into(), size: Size::S, cfg: json!({}), x: 20, y: 50, w: 170, h: 170 },
            CardSpec { id: "calendar-b".into(), type_id: "calendar".into(), size: Size::M, cfg: json!({}), x: 206, y: 50, w: 356, h: 170 },
        ];
        let scene = Scene::new(&gfx, 1600, 900, 1.0, 0.5, bd.bitmap(), &specs, &env).unwrap();
        let mut ui = Ui::new(Lang::Ru, 1.0, 1600.0, 900.0);
        let l0 = ui.lay();
        ui.edit = true;
        ui.gallery = true;
        let l = ui.lay();
        assert_eq!(l0.panel, l.panel);
        assert_eq!(ui.hit(&scene, 100, 150), Hit::Card(0));
        assert_eq!(ui.hit(&scene, 300, 150), Hit::Card(1));
        // the "−" sits on the top-left corner of the card, 4 px inside
        assert_eq!(ui.hit(&scene, 24, 54), Hit::Minus(0));
        assert_eq!(ui.hit(&scene, 210, 54), Hit::Minus(1));
        assert_eq!(ui.hit(&scene, 900, 300), Hit::None);
        let c = |r: Rect| ((r.cx()) as i32, (r.cy()) as i32);
        let (x, y) = c(l.done);
        assert_eq!(ui.hit(&scene, x, y), Hit::Done);
        let (x, y) = c(l.search);
        assert!(matches!(ui.hit(&scene, x, y), Hit::Search(_)));
        let (x, y) = c(l.rows[2]);
        assert_eq!(ui.hit(&scene, x, y), Hit::Group(2));
        let (x, y) = c(l.chips[0].1);
        assert_eq!(ui.hit(&scene, x, y), Hit::Chip(l.chips[0].0));
        let it = l.items[0];
        let (x, y) = c(it.pv);
        assert_eq!(ui.hit(&scene, x, y), Hit::Item(0));
        let (def, sz) = ui.item(0).unwrap();
        assert_eq!((def.type_id, sz), ("clock", Size::S));
        let (x, y) = c(Rect::new(l.panel.x + 400.0, l.panel.y + 300.0, 1.0, 1.0));
        assert!(matches!(ui.hit(&scene, x, y), Hit::Panel | Hit::Item(_)));
        let tb = ui.toolbar_rect();
        let track = ui.tb_lay().track;
        let (x, y) = c(Rect::new(track.x + track.w * 0.25, track.cy(), 0.0, 0.0));
        match ui.hit(&scene, x, y) {
            Hit::Slider(v) => assert!((v - 0.25).abs() < 0.02, "{v}"),
            h => panic!("{h:?}"),
        }
        assert!((ui.slider_value(x) - 0.25).abs() < 0.02);
        let (x, y) = c(ui.tb_lay().seg_plain);
        assert_eq!(ui.hit(&scene, x, y), Hit::Seg(false));
        assert!(tb.y > l.panel.y); // the strip is level with the gallery's lower edge
        // outside the gallery and the strip the desk answers again
        ui.gallery = false;
        let (x, y) = c(Rect::new(l.panel.x + 400.0, l.panel.y + 300.0, 0.0, 0.0));
        assert_eq!(ui.hit(&scene, x, y), Hit::None);
        // live mode: no UI at all
        ui.edit = false;
        assert_eq!(ui.hit(&scene, 24, 54), Hit::Card(0));
    }

    #[test]
    fn gallery_layout_and_hits() {
        let mut ui = Ui::new(Lang::Ru, 1.0, 1920.0, 1080.0);
        ui.edit = true;
        ui.gallery = true;
        let l = ui.lay();
        assert!(!l.rows.is_empty());
        assert!(l.done.right() <= l.panel.right() && l.done.bottom() <= l.panel.bottom());
        // every preview cell stays inside the grid width
        for it in &l.items {
            assert!(it.cell.x >= l.grid.x && it.cell.right() <= l.grid.right() + 0.5, "{:?}", it);
        }
        let first = l.rows[0];
        assert!(l.list.contains(first.cx(), first.cy()));
        // search filter drives the list
        ui.st.query.insert_str("часы");
        let (defs, title) = ui.shown();
        assert!(defs.iter().any(|d| d.type_id == "clock"));
        assert_eq!(title, "Результаты поиска");
        ui.st.query.clear();
        ui.st.query.insert_str("zzzzzz_nothing");
        assert!(ui.shown().0.is_empty());
    }
}

// ---------------------------------------------------------------- --dump-ui

pub struct DumpArgs {
    pub out: String,
    pub backdrop: Option<String>,
    pub state: String,
    pub w: u32,
    pub h: u32,
    pub scale: f32,
    pub search: Option<String>,
    pub group: Option<String>,
    pub hover: Option<usize>,
    pub cards: Option<String>,
    pub size: Option<Size>,
    pub no_config: bool,
    pub time: Option<Now>,
    /// `--dim`: draw the cards as "Dim widgets on desktop" does
    pub dim: bool,
    pub appearance: Option<crate::motion::AppearancePref>,
}

/// `--dump-ui`: compose the host UI of a state (live | edit | gallery | menu) headless into a PNG with the SAME scene / UI code as the live host:
/// cards at their cells, jiggle at a fixed phase, "−" buttons, the dim, the settings strip, the gallery with its live previews.
pub fn dump_ui(a: &DumpArgs) -> std::result::Result<(), String> {
    use crate::backdrop::{Backdrop, FileBackdrop, PlainBackdrop};
    let e = |what: &'static str| move |e: windows::core::Error| format!("{what}: {e}");
    let gfx = Gfx::new(None).map_err(e("device"))?;
    let (w, h, s) = (a.w, a.h, a.scale);
    let backdrop: Box<dyn Backdrop> = match a.backdrop.as_deref().map(|p| FileBackdrop::new(&gfx, p, w, h)) {
        Some(Ok(b)) => Box::new(b),
        other => {
            if let Some(Err(err)) = other {
                eprintln!("mswidgets: backdrop image: {err}; using the plain base");
            }
            Box::new(PlainBackdrop::new(&gfx, w, h).map_err(e("backdrop"))?)
        }
    };
    let settings = crate::layout::read_settings();
    let items = match &a.cards {
        Some(c) => crate::layout::parse_cards(c)?,
        None => crate::layout::load(!a.no_config),
    };
    let (lw, lh) = (w as f32 / s, h as f32 / s);
    let dims = grid::dims(lw, lh);
    let placed = grid::resolve_all(&dims, &items.iter().map(|i| (i.size, i.col as i32, i.row as i32)).collect::<Vec<_>>());
    let mut specs = Vec::new();
    for (it, p) in items.iter().zip(placed) {
        if let Some(p) = p {
            let (x, y, cw, ch) = cell_rect(it.size, p.col, p.row, s);
            specs.push(CardSpec { id: it.id.clone(), type_id: it.type_id.clone(), size: it.size, cfg: it.cfg.clone(), x, y, w: cw, h: ch });
        }
    }
    let env = SceneEnv { preview: true, hwnd: 0, lang: crate::layout::lang(), utc_off: crate::sys::local_utc_offset_min() };
    let mut scene = Scene::new(&gfx, w as i32, h as i32, s, settings.glass, backdrop.bitmap(), &specs, &env).map_err(e("scene"))?;
    let now = a.time.unwrap_or_else(Now::sample);
    scene.set_appearance_pref(a.appearance.unwrap_or(settings.appearance));
    for i in 0..scene.len() {
        scene.tick_card(&gfx, i, &now);
    }
    let all = if scene.len() >= 63 { u64::MAX >> 1 } else { (1u64 << scene.len()) - 1 };
    scene.measure_lum(&gfx, all, false).map_err(e("luminance"))?;
    scene.apply_debug(&gfx, &now);
    for i in 0..scene.len() {
        scene.render_card(&gfx, i, &now).map_err(e("render"))?;
    }
    let mut ui = Ui::new(env.lang, s, lw, lh);
    ui.glass = settings.glass;
    ui.live = settings.live;
    let edit = matches!(a.state.as_str(), "edit" | "gallery");
    ui.edit = edit;
    ui.gallery = a.state == "gallery";
    ui.anim = 1.0;
    ui.dim = if edit { 0.28 } else { 0.0 };
    ui.st.focus = a.search.is_some();
    if let Some(q) = &a.search {
        ui.st.query.insert_str(q);
    }
    if let Some(g) = &a.group {
        if let Some(i) = ui.groups.iter().position(|x| x.id == g) {
            ui.select_group(i);
        }
    }
    if let Some(sz) = a.size {
        ui.select_size(sz);
    }
    ui.hover = a.hover.map_or(Hit::None, Hit::Item);
    if edit {
        for i in 0..scene.len() {
            scene.set_xf(i, Xf { rot: jiggle(i, 0.13), ..Xf::ID });
        }
    }
    if a.state == "menu" && !scene.cards.is_empty() {
        eprintln!("mswidgets: the context menu is a native popup (TrackPopupMenu): nothing to draw in --dump-ui, showing the desk");
    }
    ui.sync_previews(&gfx, &now);
    ui.sync_panels(&mut scene, &gfx);
    scene.set_dim(a.dim && !edit);
    let full = RECT { left: 0, top: 0, right: w as i32, bottom: h as i32 };
    scene.prepare(&gfx, full, &now).map_err(e("prepare"))?;
    scene.begin(&gfx, full, Some(backdrop.bitmap()));
    ui.draw_pre(&gfx, full);
    scene.draw_cards(&gfx, full, &now);
    ui.draw_post(&scene, &gfx, &now, full);
    scene.end(&gfx).map_err(e("frame"))?;
    let mut px = vec![0u8; (w * h * 4) as usize];
    scene
        .read_rect(&gfx, full, |y, row| {
            let o = y as usize * w as usize * 4;
            px[o..o + row.len()].copy_from_slice(row);
        })
        .map_err(e("readback"))?;
    gfx.save_png(&a.out, w, h, &px).map_err(e("save"))?;
    eprintln!("saved {} ({w}x{h}, {} cards, state {})", a.out, scene.len(), a.state);
    Ok(())
}
