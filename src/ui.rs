//! Host UI drawn on top of the cards while the desk is being edited: the dim, the placeholder under a dragged card, the "−" buttons, the
//! settings strip (glass slider, backdrop mode), the "Edit Widgets" gallery (search, group sidebar, size chips, LIVE previews, Done) and a
//! toast. Everything is laid out in LOGICAL px (absolute screen coordinates) and drawn through `Cx` inside the scene's open frame session.
//! The gallery previews are tiny one-card `Scene`s in preview mode (the same draw / overlay code as the desk) cached per (widget, size).
use crate::cards::*;
use crate::gfx::*;
use crate::glass::{self, GlassParams, GlassStyle, LOOK};
use crate::grid;
use crate::layout::{AccentMode, AccentScope, Dim};
use crate::motion::AppearancePref;
use crate::registry::{self, WidgetDef};
use crate::update::UpdateState;
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
const TB_W: f32 = 552.0;
const TB_H: f32 = 40.0;
const MINUS_R: f32 = 13.0;
/// settings panel: width, height of the title + column header block above the first row, row pitch, footer height
const SET_W: f32 = 760.0;
const SET_TOP: f32 = 74.0;
const SET_ROW: f32 = 31.0;
const SET_FOOT: f32 = 56.0;
/// rows of the right column (segmented controls, the autostart switch)
const OPT_ROW: f32 = 38.0;
/// dim over the desktop while the settings panel is up (lighter than the edit mode's: the look is being judged)
pub const SETTINGS_DIM: f32 = 0.12;
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

/// The segmented controls of the settings panel.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Opt {
    /// dark / light / auto glass (`widgetsAppearance`)
    Appearance,
    /// auto / never / always (`widgetsDim`)
    Dim,
    /// live / plain (`widgetsGlassMode`)
    Source,
    /// blue / from the wallpaper (`accentMode`)
    Accent,
    /// accent on the main things / everywhere, Apple's "Tinted" (`accentScope`)
    Scope,
    /// which monitor the desk lives on (`widgetsMonitor`): one segment per connected monitor, `Ui::opt_n` of them
    Monitor,
}
/// At most this many monitors get a segment (the tray menu lists all of them).
pub const MON_SEGS: usize = 5;
impl Opt {
    pub const ALL: [Opt; 6] = [Opt::Appearance, Opt::Dim, Opt::Source, Opt::Accent, Opt::Scope, Opt::Monitor];
    /// Number of options of a fixed control (`Monitor` depends on the connected monitors: `Ui::opt_n`).
    pub fn count(self) -> usize {
        match self {
            Opt::Source | Opt::Accent | Opt::Scope => 2,
            Opt::Monitor => 1,
            _ => 3,
        }
    }
    /// Option `k` as the appearance preference (Dark, Light, Auto).
    pub fn appearance(k: usize) -> AppearancePref {
        [AppearancePref::Dark, AppearancePref::Light, AppearancePref::Auto][k.min(2)]
    }
    /// Option `k` as the accent source (Blue, Wallpaper).
    pub fn accent(k: usize) -> AccentMode {
        [AccentMode::Blue, AccentMode::Wallpaper][k.min(1)]
    }
    /// Option `k` as the accent scope (Main, All).
    pub fn scope(k: usize) -> AccentScope {
        [AccentScope::Main, AccentScope::All][k.min(1)]
    }
    /// Option `k` as the dimming mode (Auto, Never, Always).
    pub fn dim(k: usize) -> Dim {
        [Dim::Auto, Dim::Never, Dim::Always][k.min(2)]
    }
}

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
    /// the "Settings" button of the strip
    SettingsBtn,
    Toolbar,
    /// settings panel: look slider i (0 = Glass, i >= 1 = `LOOK[i - 1]`) with the value 0..1 at the pointer, option k of a
    /// segmented control, the autostart switch, "Reset look", "Edit widgets", "Quit"
    Look(usize, f32),
    Opt(Opt, usize),
    Autostart,
    /// the "Check on start" switch, the "Check for updates" button, the "Update now" and "Release page" buttons of an available update
    CheckStart,
    UpdBtn,
    UpdInstall,
    UpdPage,
    ResetLook,
    EditBtn,
    QuitBtn,
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
    /// the settings panel is open (edit mode machinery without the gallery: no jiggle, no "−", the desk is not interactive)
    pub settings: bool,
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
    /// settings panel: the look being edited, the base look it resets to, the behaviour options, the "Start with Windows" state
    pub look: GlassParams,
    pub base: GlassParams,
    pub appearance: AppearancePref,
    pub dim_mode: Dim,
    pub accent_mode: AccentMode,
    pub accent_scope: AccentScope,
    /// number of connected monitors and the display position (0 = primary) of the one the desk is on
    pub mon_count: usize,
    pub mon_sel: usize,
    pub autostart: bool,
    /// settings panel: `checkUpdates` and the state of the update check
    pub check_start: bool,
    pub update: UpdateState,
    /// keyboard focus on a slider: the settings panel's row (0 = Glass), or 0 = the glass slider of the strip while the gallery is open
    pub sfocus: Option<usize>,
    /// how far the gallery / the settings panel were dragged from where they open, logical px
    pub gal_off: (f32, f32),
    pub set_off: (f32, f32),
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
    pub sep2: Rect,
    pub btn_settings: Rect,
}

/// Keys the sliders react to (the host maps the virtual keys).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SKey {
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    /// Backspace / Delete
    Reset,
    Tab,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum SAct {
    /// the focus moved
    Focus,
    /// slider i goes to position t (0..1)
    Set(usize, f32),
}

/// Position of a slider after `notches` of the mouse wheel (+ = up = more): 2 % per notch.
pub fn wheel_pos(p: f32, notches: f32) -> f32 {
    (p + notches * 0.02).clamp(0.0, 1.0)
}

/// One look slider row of the settings panel.
#[derive(Clone, Copy, Debug)]
pub struct SliderRow {
    pub row: Rect,
    pub label: Rect,
    pub track: Rect,
    pub value: Rect,
}

/// A segmented control row of the settings panel.
#[derive(Clone, Copy, Debug)]
pub struct OptRow {
    pub which: Opt,
    pub label: Rect,
    pub seg: Rect,
}

/// Layout of the settings panel (logical px, absolute screen coordinates).
pub struct SetLay {
    pub panel: Rect,
    pub title: Rect,
    pub look_hdr: Rect,
    pub beh_hdr: Rect,
    pub reset: Rect,
    pub sep: Rect,
    pub rows: Vec<SliderRow>,
    pub opts: Vec<OptRow>,
    pub auto_row: Rect,
    pub auto_sw: Rect,
    pub check_row: Rect,
    pub check_sw: Rect,
    pub edit_btn: Rect,
    pub quit_btn: Rect,
    pub keys: Rect,
    pub done: Rect,
    /// footer: "Check for updates", its status line, "Release page" and "Update now" (both only offered for an available update)
    pub upd_btn: Rect,
    pub upd_status: Rect,
    pub upd_page: Rect,
    pub upd_dl: Rect,
    pub foot: Rect,
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
            settings: false,
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
    fn gallery_home(&self) -> Rect {
        let w = PANEL_W.min(self.mw * 0.88);
        let h = PANEL_H.min(self.mh * 0.72);
        Rect::new((self.mw * 0.06).min(self.mw - w - 8.0).max(0.0), self.mh - (self.mh * 0.03).max(10.0) - TB_H - 12.0 - h, w, h)
    }

    /// Keeps a dragged panel on the screen.
    fn on_screen(&self, r: Rect, off: (f32, f32)) -> Rect {
        let x = (r.x + off.0).clamp(8.0_f32.min(self.mw - r.w), (self.mw - r.w - 8.0).max(8.0));
        let y = (r.y + off.1).clamp(8.0_f32.min(self.mh - r.h), (self.mh - r.h - 8.0).max(8.0));
        Rect::new(x, y, r.w, r.h)
    }

    pub fn gallery_rect(&self) -> Rect {
        self.on_screen(self.gallery_home(), self.gal_off)
    }

    /// The settings panel opens at the right edge, vertically centred: the cards (top left of the desk) stay visible to judge the look.
    fn settings_home(&self) -> Rect {
        let w = SET_W.min(self.mw - 16.0);
        let h = (SET_TOP + (1 + LOOK.len()) as f32 * SET_ROW + 14.0 + SET_FOOT + OPT_ROW).min(self.mh - 24.0);
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
            sep2: Rect::new(bar.x + 442.0, cy - 9.0, 1.0, 18.0),
            btn_settings: Rect::new(bar.x + 458.0, cy - 12.0, 78.0, 24.0),
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
        let lx = p.x + 22.0;
        let lw = (p.w * 0.5).round();
        let rx = p.x + lw + 60.0;
        let rw = p.right() - 22.0 - rx;
        let rows = (0..n)
            .map(|i| {
                let row = Rect::new(lx, top + i as f32 * rh, lw, rh);
                SliderRow { row, label: Rect::new(lx, row.y, 116.0, rh), track: Rect::new(lx + 124.0, row.cy() - 2.0, lw - 124.0 - 56.0 - 12.0, 4.0), value: Rect::new(row.right() - 54.0, row.y, 54.0, rh) }
            })
            .collect();
        let opts = Opt::ALL
            .iter()
            .enumerate()
            .map(|(k, o)| {
                let y = top + k as f32 * orow;
                OptRow { which: *o, label: Rect::new(rx, y, 112.0, 28.0), seg: Rect::new(rx + 112.0, y + 2.0, rw - 112.0, 24.0) }
            })
            .collect();
        let ay = top + Opt::ALL.len() as f32 * orow;
        let cy = top + (Opt::ALL.len() + 1) as f32 * orow;
        let by = top + (Opt::ALL.len() + 2) as f32 * orow + 10.0;
        let bw = ((rw - 8.0) * 0.6).round();
        let fy = p.bottom() - SET_FOOT + 13.0;
        let done = Rect::new(p.right() - 16.0 - 82.0, fy, 82.0, 30.0);
        let upd_btn = Rect::new(p.x + 22.0, fy, 176.0, 30.0);
        let upd_dl = Rect::new(done.x - 8.0 - 112.0, fy, 112.0, 30.0);
        let upd_page = Rect::new(upd_dl.x - 8.0 - 100.0, fy, 100.0, 30.0);
        let sx = upd_btn.right() + 12.0;
        SetLay {
            panel: p,
            title: Rect::new(p.x + 22.0, p.y + 14.0, 300.0, 28.0),
            look_hdr: Rect::new(lx, p.y + 48.0, 200.0, 20.0),
            beh_hdr: Rect::new(rx, p.y + 48.0, 200.0, 20.0),
            reset: Rect::new(lx + lw - 92.0, p.y + 46.0, 92.0, 24.0),
            sep: Rect::new(p.x + lw + 30.0, p.y + 50.0, 0.8, p.h - SET_FOOT - 58.0),
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
                    "blur" => format!("{:.1} px", self.look.blur),
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
        [Hit::ResetLook, Hit::Opt(Opt::Appearance, 1), Hit::Opt(Opt::Dim, 0), Hit::Opt(Opt::Source, 1), Hit::Opt(Opt::Accent, 1), Hit::Opt(Opt::Scope, 1), Hit::Opt(Opt::Monitor, 0), Hit::Autostart, Hit::CheckStart, Hit::EditBtn, Hit::QuitBtn, Hit::UpdBtn, Hit::UpdInstall, Hit::UpdPage, Hit::Done].get(k - n).copied().unwrap_or(Hit::None)
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
        }
    }

    /// Number of segments of a control: fixed, except `Monitor` (one per connected monitor, 1..=MON_SEGS).
    pub fn opt_n(&self, o: Opt) -> usize {
        match o {
            Opt::Monitor => self.mon_count.clamp(1, MON_SEGS),
            _ => o.count(),
        }
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
        let g = if self.edit && (self.gallery || self.settings) { Some(spec(self.panel_rect(), PANEL_R)) } else { None };
        let t = if self.edit && !self.settings { Some(spec(self.toolbar_rect(), TB_H / 2.0)) } else { None };
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
        let mut d = r(if self.settings { self.panel_rect() } else { self.toolbar_rect() });
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

    fn seg(&self, cx: &Cx, r: Rect, labels: &[(&str, bool)], pad: f32, hot: Option<usize>) {
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
            if self.sfocus == Some(0) {
                cx.circle_stroke((kx, tb.track.cy()), 11.5, 2.0, accent().alpha(0.95));
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

    /// The glass slider of the strip, restyled for the settings rows: track, fill, base-look tick and a knob that grows under the pointer.
    fn set_slider(&self, cx: &Cx, r: &SliderRow, v: f32, def: f32, hot: bool, down: bool, focus: bool) {
        let t = r.track;
        if focus {
            cx.rrect(r.row.inset_xy(-8.0, 1.0), 8.0, Color::rgba(1.0, 1.0, 1.0, 0.07));
        }
        cx.pill(t, FILL);
        cx.pill(Rect::new(t.x, t.y, (t.w * v).max(4.0), t.h), Color::rgba(1.0, 1.0, 1.0, if hot || down { 0.70 } else { 0.55 }));
        let dx = t.x + t.w * def;
        cx.line((dx, t.cy() + 5.0), (dx, t.cy() + 8.0), 1.2, FAINT);
        let kx = t.x + t.w * v;
        let kr = if down { 9.0 } else if hot { 8.6 } else { 8.0 };
        if down {
            cx.circle_stroke((kx, t.cy()), kr + 3.0, 3.0, Color::rgba(1.0, 1.0, 1.0, 0.20));
        }
        cx.circle((kx, t.cy() + 1.5), kr + 1.0, Color::rgba(0.0, 0.0, 0.0, 0.25));
        cx.circle((kx, t.cy()), kr, Color::rgba(1.0, 1.0, 1.0, 0.97));
        if focus {
            cx.circle_stroke((kx, t.cy()), kr + 3.5, 2.0, accent().alpha(0.95));
        }
    }

    /// A label with an on / off switch on the right.
    fn set_switch(&self, cx: &Cx, row: Rect, sw: Rect, label: &str, on: bool, hot: bool) {
        cx.text(Rect::new(row.x, row.y, row.w - 52.0, row.h), label, Style::new(12.5, 500).ellipsis(), TEXT);
        let sw = sw.centered(40.0, 24.0);
        cx.pill(sw, if on { Color::rgb8(52, 199, 89) } else if hot { Color::rgba(1.0, 1.0, 1.0, 0.26) } else { FILL });
        let kx = if on { sw.right() - 12.0 } else { sw.x + 12.0 };
        cx.circle((kx, sw.cy() + 1.2), 10.0, Color::rgba(0.0, 0.0, 0.0, 0.22));
        cx.circle((kx, sw.cy()), 9.5, Color::rgba(1.0, 1.0, 1.0, 0.98));
    }

    fn set_button(&self, cx: &Cx, r: Rect, label: &str, hot: bool, down: bool, color: Color) {
        let a = if down { 0.34 } else if hot { 0.27 } else { 0.18 };
        cx.pill(r, Color::rgba(1.0, 1.0, 1.0, a));
        cx.text(r, label, Style::new(12.5, 600).center().ellipsis(), color);
    }

    fn draw_settings(&self, scene: &Scene, gfx: &Gfx, now: &Now) {
        let a = self.anim.clamp(0.0, 1.0);
        let xf = self.open_xf();
        let to_canvas = mul(&mat(self.s, 0.0, 0.0), &xf);
        scene.draw_panel(gfx, SLOT_GALLERY, a, Some(xf));
        let layer = self.push_alpha(gfx, a);
        let l = self.set_lay();
        let (hv, pr) = (self.hover, self.press);
        let look_at = |h: Hit, i: usize| matches!(h, Hit::Look(j, _) if j == i);
        scene.with_ui_cx(gfx, to_canvas, self.lang, now, |cx| {
            cx.text(l.title, self.t("Настройки", "Settings"), Style::new(15.0, 600), TEXT);
            cx.text(l.look_hdr, &self.t("Вид стекла", "Glass look").to_uppercase(), Style::LABEL.size(11.0), DIM);
            cx.text(l.beh_hdr, &self.t("Поведение", "Behaviour").to_uppercase(), Style::LABEL.size(11.0), DIM);
            cx.rect(l.sep, FILL);

            // look sliders
            let names = [("Стекло", "Glass"), ("Размытие", "Blur"), ("Преломление", "Refraction"), ("Цветная кайма", "Chromatic fringe"), ("Насыщенность", "Saturation"), ("Яркость", "Brightness"), ("Матовость", "Frosting"), ("Блики", "Highlights"), ("Свечение", "Glow"), ("Скругление", "Corner roundness"), ("Размер карточек", "Card size")];
            for (i, r) in l.rows.iter().enumerate() {
                let (hot, down) = (look_at(hv, i), look_at(pr, i));
                let (ru, en) = names.get(i).copied().unwrap_or(("", ""));
                cx.text(r.label, self.t(ru, en), Style::new(12.5, 500).ellipsis(), TEXT);
                self.set_slider(cx, r, self.set_pos(i), self.set_default_pos(i), hot, down, self.sfocus == Some(i));
                cx.text(r.value, &self.set_value_text(i), Style::new(11.5, 500).right().mono(), if hot || down { TEXT } else { DIM });
            }
            let hot = hv == Hit::ResetLook;
            self.set_button(cx, l.reset, self.t("Сбросить вид", "Reset look"), hot, pr == Hit::ResetLook, TEXT);

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
                cx.text(o.label, self.t(ru, en), Style::new(12.5, 500).ellipsis(), TEXT);
                let sel = self.opt_sel(o.which);
                let items: Vec<(&str, bool)> = if o.which == Opt::Monitor {
                    nums.iter().enumerate().map(|(k, n)| (n.as_str(), k == sel)).collect()
                } else {
                    opts.iter().take(o.which.count()).enumerate().map(|(k, (r, e))| (self.t(r, e), k == sel)).collect()
                };
                let hot = if let Hit::Opt(w, k) = hv { (w == o.which).then_some(k) } else { None };
                self.seg(cx, o.seg, &items, 2.0, hot);
            }

            // start with Windows, check for updates on start: label + switch
            let hot = hv == Hit::Autostart || pr == Hit::Autostart;
            self.set_switch(cx, l.auto_row, l.auto_sw, self.t("Запускать с Windows", "Start with Windows"), self.autostart, hot);
            let hot = hv == Hit::CheckStart || pr == Hit::CheckStart;
            self.set_switch(cx, l.check_row, l.check_sw, self.t("Проверять обновления при запуске", "Check for updates on start"), self.check_start, hot);

            // buttons
            self.set_button(cx, l.edit_btn, self.t("Изменить виджеты", "Edit widgets"), hv == Hit::EditBtn, pr == Hit::EditBtn, TEXT);
            self.set_button(cx, l.quit_btn, self.t("Выйти", "Quit"), hv == Hit::QuitBtn, pr == Hit::QuitBtn, Color::rgb8(255, 105, 97));

            // shortcuts
            let k = Style::new(11.0, 400);
            let lines = [
                ("Ctrl+Alt+G", ("Настройки", "Settings")),
                ("Ctrl+Alt+W", ("Изменить виджеты", "Edit widgets")),
                ("Ctrl+Alt+Q", ("Выйти", "Quit")),
                ("Esc", ("Закрыть (или щелчок снаружи)", "Close (or click outside)")),
                ("Tab / ← →", ("Ползунки с клавиатуры", "Sliders by keyboard")),
            ];
            for (n, (keys, (ru, en))) in lines.iter().enumerate() {
                let r = Rect::new(l.keys.x, l.keys.y + n as f32 * 20.0, l.keys.w, 20.0);
                if r.bottom() > l.keys.bottom() + 0.5 {
                    break;
                }
                cx.text(Rect::new(r.x, r.y, 84.0, r.h), keys, k.mono(), FAINT);
                cx.text(Rect::new(r.x + 90.0, r.y, r.w - 90.0, r.h), self.t(ru, en), k.ellipsis(), FAINT);
            }

            // footer
            cx.rect(l.foot, FILL);
            let busy = matches!(self.update, UpdateState::Checking | UpdateState::Downloading(_) | UpdateState::Verifying);
            self.set_button(cx, l.upd_btn, self.t("Проверить обновления", "Check for updates"), hv == Hit::UpdBtn && !busy, pr == Hit::UpdBtn && !busy, if busy { DIM } else { TEXT });
            let (msg, color) = match &self.update {
                UpdateState::Idle => (format!("{} {}", self.t("Версия", "Version"), env!("CARGO_PKG_VERSION")), FAINT),
                UpdateState::Checking => (self.t("Проверка…", "Checking…").to_string(), DIM),
                UpdateState::Latest => (self.t("У вас последняя версия", "You have the latest version").to_string(), Color::rgb8(52, 199, 89)),
                UpdateState::Available { error: Some(e), .. } => (if self.lang == Lang::Ru { format!("Не удалось обновить: {e}") } else { format!("Update failed: {e}") }, ORANGE),
                UpdateState::Available { version, .. } => (if self.lang == Lang::Ru { format!("Доступна версия {version}") } else { format!("Version {version} is available") }, TEXT),
                UpdateState::Downloading(p) => (if self.lang == Lang::Ru { format!("Загрузка обновления… {p}%") } else { format!("Downloading the update… {p}%") }, TEXT),
                UpdateState::Verifying => (self.t("Проверка контрольной суммы…", "Verifying the checksum…").to_string(), TEXT),
                UpdateState::Failed => (self.t("Не удалось проверить", "Could not check").to_string(), ORANGE),
            };
            cx.text(l.upd_status, &msg, Style::new(12.0, 500).ellipsis(), color);
            if matches!(self.update, UpdateState::Available { .. }) {
                self.set_button(cx, l.upd_page, self.t("О релизе", "Release page"), hv == Hit::UpdPage, pr == Hit::UpdPage, DIM);
                self.set_button(cx, l.upd_dl, self.t("Обновить", "Update now"), hv == Hit::UpdInstall, pr == Hit::UpdInstall, accent());
            }
            let hot = hv == Hit::Done || pr == Hit::Done;
            cx.pill(l.done.offset(0.0, 2.0).inset_xy(2.0, 0.0), accent_fill(false).alpha(0.40));
            cx.pill(l.done, accent_fill(hot));
            cx.text(l.done, self.t("Готово", "Done"), Style::new(13.0, 600).center(), WHITE);
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
                cx.rrect_stroke(l.search.inset(-2.0), 11.0, 3.0, accent().alpha(0.30));
                cx.rrect_stroke(l.search, 9.0, 1.6, accent().alpha(0.95));
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
                    cx.rrect(Rect::new(text_area.x - off - 1.0, text_area.cy() - 9.0, tw + 2.0, 18.0), 4.0, accent_fill(false).alpha(0.75));
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
                    cx.rrect(r, 9.0, accent_fill(false).alpha(0.92));
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
                    cx.circle(b, 11.0, accent_fill(false).alpha(0.97));
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
            cx.pill(l.done.offset(0.0, 2.0).inset_xy(2.0, 0.0), accent_fill(false).alpha(0.40));
            cx.pill(l.done, accent_fill(hot));
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
        assert!((p.x - 115.2).abs() < 0.1 && (p.bottom() - (1080.0 - 32.4 - 40.0 - 12.0)).abs() < 0.1);
        // the strip sits right under the panel, centred on it
        let t = ui.toolbar_rect();
        assert!((t.y - (p.bottom() + 12.0)).abs() < 0.1 && (t.cx() - p.cx()).abs() < 0.1 && t.bottom() <= 1080.0);
        let small = Ui::new(Lang::Ru, 1.0, 800.0, 500.0).panel_rect();
        assert!(small.w <= 800.0 * 0.88 + 0.01 && small.h <= 360.1 && small.right() <= 800.0);
    }

    #[test]
    fn panels_drag_and_stay_on_screen() {
        let mut ui = Ui::new(Lang::Ru, 1.0, 1920.0, 1080.0);
        let home = ui.panel_rect();
        ui.move_panel(home.x + 100.0, home.y - 50.0);
        let p = ui.panel_rect();
        assert!((p.x - (home.x + 100.0)).abs() < 0.1 && (p.y - (home.y - 50.0)).abs() < 0.1);
        assert!((ui.toolbar_rect().y - (p.bottom() + 12.0)).abs() < 0.1);
        // far off the screen: clamped, and the strip flips above the panel when there is no room below
        ui.move_panel(5000.0, 5000.0);
        let p = ui.panel_rect();
        assert!(p.right() <= 1920.0 && p.bottom() <= 1080.0);
        assert!(ui.toolbar_rect().bottom() <= p.y);
        // the settings panel has its own position
        ui.settings = true;
        let s0 = ui.panel_rect();
        ui.move_panel(10.0, 10.0);
        assert!(ui.panel_rect().x < s0.x && ui.gallery_rect().y == p.y);
    }

    /// The panel chrome (Done, selection, focus rings) and the text prompt's accent take the colour of `set_accent`, not a fixed blue. This is the only
    /// test that moves the process-wide accent (it puts the default back).
    #[test]
    fn panel_chrome_follows_the_accent() {
        set_accent(None);
        let (blue, blue_hot, blue_ring, blue_prompt) = (accent_fill(false), accent_fill(true), accent(), crate::input::acc(1.0));
        assert!(blue.b > blue.r && blue_ring.b > blue_ring.r && blue_prompt.b > blue_prompt.r);
        assert!(blue_hot.r + blue_hot.g + blue_hot.b > blue.r + blue.g + blue.b, "hover is lighter");
        assert!(set_accent(Some([230, 70, 70])), "a new accent reports a change");
        let (red, red_hot, red_ring, red_prompt) = (accent_fill(false), accent_fill(true), accent(), crate::input::acc(1.0));
        for c in [red.r > red.b && red.r > red.g, red_hot.r > red_hot.b, red_ring.r > red_ring.b && red_ring.r > red_ring.g, red_prompt.r > red_prompt.b && red_prompt.r > red_prompt.g] {
            assert!(c, "chrome stays blue after set_accent");
        }
        assert!((red_ring.r, red_ring.g, red_ring.b) == (red_prompt.r, red_prompt.g, red_prompt.b) && red.r < red_ring.r + 0.01 && red.r + red.g + red.b < red_ring.r + red_ring.g + red_ring.b, "the fill is the deeper shade of the ring");
        set_accent(None);
        assert_eq!((accent_fill(false).b > accent_fill(false).r, accent().b > accent().r), (true, true));
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

    /// The settings panel: every control sits inside the panel, rows do not overlap, hits map to the right control and value, the panel fits
    /// small screens, a click outside is `Hit::None`.
    #[test]
    fn settings_panel_layout_and_hits() {
        for (mw, mh) in [(1920.0, 1080.0), (1366.0, 768.0), (800.0, 500.0)] {
            let mut ui = Ui::new(Lang::En, 1.0, mw, mh);
            ui.edit = true;
            ui.settings = true;
            let l = ui.set_lay();
            let p = l.panel;
            assert!(p.x >= 0.0 && p.y >= 0.0 && p.right() <= mw && p.bottom() <= mh, "{mw}x{mh}: {p:?}");
            assert_eq!(l.rows.len(), 1 + LOOK.len());
            let inside = |r: Rect| r.x >= p.x && r.y >= p.y && r.right() <= p.right() + 0.01 && r.bottom() <= p.bottom() + 0.01;
            for r in &l.rows {
                assert!(inside(r.row) && inside(r.track) && inside(r.value) && inside(r.label));
                assert!(r.label.right() <= r.track.x && r.track.right() <= r.value.x, "{r:?}");
            }
            for w in l.rows.windows(2) {
                assert!(w[0].row.bottom() <= w[1].row.y + 0.01);
            }
            for o in &l.opts {
                assert!(inside(o.seg) && inside(o.label) && o.label.right() <= o.seg.x + 0.01);
            }
            for r in [l.reset, l.auto_row, l.auto_sw, l.check_row, l.check_sw, l.edit_btn, l.quit_btn, l.done, l.upd_btn, l.upd_status, l.upd_page, l.upd_dl] {
                assert!(inside(r), "{r:?}");
            }
            // the footer line is one row: update button, status, download, Done, none over another; the right column does not run into the footer
            assert!(l.upd_btn.right() <= l.upd_status.x && l.upd_status.right() <= l.upd_page.x && l.upd_page.right() <= l.upd_dl.x && l.upd_dl.right() <= l.done.x && l.upd_status.w > 150.0);
            assert!(l.auto_row.bottom() <= l.check_row.y && l.check_row.bottom() <= l.edit_btn.y && l.edit_btn.bottom() <= l.foot.y && l.keys.bottom() <= l.foot.y + 0.01);
            assert!(l.edit_btn.right() <= l.quit_btn.x && l.rows.iter().all(|r| r.value.right() <= l.sep.x));
            // sliders: the track ends map to 0 / 1, the middle to ~0.5; a click in the label column does not move a slider
            let t = l.rows[3].track;
            assert_eq!(ui.set_hit(t.x, t.cy()), Hit::Look(3, 0.0));
            assert!(matches!(ui.set_hit(t.right() - 0.01, t.cy()), Hit::Look(3, v) if v > 0.999));
            match ui.set_hit(t.cx(), l.rows[3].row.y + 2.0) {
                Hit::Look(3, v) => assert!((v - 0.5).abs() < 0.01),
                h => panic!("{h:?}"),
            }
            assert_eq!(ui.set_hit(l.rows[3].label.x + 4.0, l.rows[3].row.cy()), Hit::Panel);
            assert!((ui.set_value_at(3, (t.x + t.w * 0.25) as i32) - 0.25).abs() < 0.01);
            // segmented controls pick the option under the pointer
            for o in &l.opts {
                for k in 0..ui.opt_n(o.which) {
                    let x = o.seg.x + o.seg.w * (k as f32 + 0.5) / ui.opt_n(o.which) as f32;
                    assert_eq!(ui.set_hit(x, o.seg.cy()), Hit::Opt(o.which, k));
                }
            }
            assert_eq!(ui.set_hit(l.auto_sw.cx(), l.auto_sw.cy()), Hit::Autostart);
            assert_eq!(ui.set_hit(l.auto_row.x + 5.0, l.auto_row.cy()), Hit::Autostart);
            assert_eq!(ui.set_hit(l.reset.cx(), l.reset.cy()), Hit::ResetLook);
            assert_eq!(ui.set_hit(l.edit_btn.cx(), l.edit_btn.cy()), Hit::EditBtn);
            assert_eq!(ui.set_hit(l.quit_btn.cx(), l.quit_btn.cy()), Hit::QuitBtn);
            assert_eq!(ui.set_hit(l.done.cx(), l.done.cy()), Hit::Done);
            assert_eq!(ui.set_hit(l.check_sw.cx(), l.check_sw.cy()), Hit::CheckStart);
            assert_eq!(ui.set_hit(l.upd_btn.cx(), l.upd_btn.cy()), Hit::UpdBtn);
            assert_eq!(ui.set_hit(l.upd_status.x + 2.0, l.upd_status.cy()), Hit::Panel);
            // "Update now" and "Release page" exist only for an available update
            assert_eq!((ui.set_hit(l.upd_dl.cx(), l.upd_dl.cy()), ui.set_hit(l.upd_page.cx(), l.upd_page.cy())), (Hit::Panel, Hit::Panel));
            ui.update = UpdateState::Available { version: "9.9.9".into(), url: "https://github.com/x".into(), asset: None, error: None };
            assert_eq!((ui.set_hit(l.upd_dl.cx(), l.upd_dl.cy()), ui.set_hit(l.upd_page.cx(), l.upd_page.cy())), (Hit::UpdInstall, Hit::UpdPage));
            ui.update = UpdateState::Downloading(5);
            assert_eq!(ui.set_hit(l.upd_dl.cx(), l.upd_dl.cy()), Hit::Panel);
            assert_eq!(ui.set_hit(p.x - 1.0, p.cy()), Hit::None);
            assert_eq!(ui.set_hit(1.0, 1.0), Hit::None);
            // reading order for the headless --hover
            assert!(matches!(ui.nth_control(0), Hit::Look(0, _)) && ui.nth_control(l.rows.len() + 14) == Hit::Done && ui.nth_control(99) == Hit::None);
        }
    }

    /// The "Monitor" control has one segment per connected monitor (1..=MON_SEGS), the selected one follows `mon_sel`, a click picks the segment.
    #[test]
    fn monitor_control_follows_the_monitor_count() {
        let mut ui = Ui::new(Lang::En, 1.0, 1600.0, 900.0);
        ui.edit = true;
        ui.settings = true;
        assert_eq!((ui.opt_n(Opt::Monitor), ui.opt_sel(Opt::Monitor), ui.opt_n(Opt::Dim)), (1, 0, 3));
        (ui.mon_count, ui.mon_sel) = (3, 2);
        assert_eq!((ui.opt_n(Opt::Monitor), ui.opt_sel(Opt::Monitor)), (3, 2));
        (ui.mon_count, ui.mon_sel) = (9, 8);
        assert_eq!((ui.opt_n(Opt::Monitor), ui.opt_sel(Opt::Monitor)), (MON_SEGS, MON_SEGS - 1));
        (ui.mon_count, ui.mon_sel) = (0, 5);
        assert_eq!((ui.opt_n(Opt::Monitor), ui.opt_sel(Opt::Monitor)), (1, 0));
        ui.mon_count = 3;
        let l = ui.set_lay();
        let o = l.opts.iter().find(|o| o.which == Opt::Monitor).expect("monitor row");
        for k in 0..3 {
            assert_eq!(ui.set_hit(o.seg.x + o.seg.w * (k as f32 + 0.5) / 3.0, o.seg.cy()), Hit::Opt(Opt::Monitor, k));
        }
        // the row sits inside the panel above the switches, and the panel grew to hold it
        assert!(l.panel.contains(o.seg.cx(), o.seg.cy()) && o.seg.bottom() <= l.auto_row.y + 0.5);
        assert!(l.edit_btn.bottom() < l.panel.bottom());
    }

    /// The shown values follow the look: positions, texts, the selected options, the base tick.
    #[test]
    fn settings_values_follow_the_look() {
        let mut ui = Ui::new(Lang::En, 1.0, 1600.0, 900.0);
        let base = ui.base;
        assert_eq!(ui.set_value_text(0), "50%");
        assert_eq!(ui.set_value_text(2), "100%"); // refraction at the base look
        assert_eq!(ui.set_value_text(1), format!("{:.1} px", base.blur));
        assert_eq!(ui.set_value_text(LOOK.len()), "100%"); // card size
        glass::LOOK[2].set_pos(&mut ui.look, &base, 1.0); // fringe, index 3 in the panel
        assert_eq!(ui.set_value_text(3), "x10");
        assert!((ui.set_pos(3) - 1.0).abs() < 1e-6 && ui.set_default_pos(3) < 0.5);
        ui.glass = 0.25;
        assert_eq!((ui.set_pos(0), ui.set_default_pos(0)), (0.25, 0.5));
        ui.appearance = AppearancePref::Light;
        ui.dim_mode = Dim::Always;
        ui.live = false;
        ui.accent_mode = AccentMode::Wallpaper;
        assert_eq!((ui.opt_sel(Opt::Appearance), ui.opt_sel(Opt::Dim), ui.opt_sel(Opt::Source), ui.opt_sel(Opt::Accent)), (1, 2, 1, 1));
        assert_eq!((Opt::appearance(2), Opt::dim(1), Opt::count(Opt::Source), Opt::accent(0), Opt::count(Opt::Accent)), (AppearancePref::Auto, Dim::Never, 2, AccentMode::Blue, 2));
        // panel geometry: the settings panel replaces the gallery rect only while it is up
        assert_eq!(ui.panel_rect(), ui.gallery_rect());
        ui.settings = true;
        assert_eq!(ui.panel_rect(), ui.settings_rect());
        assert!(!ui.wheel(500.0, 400.0, 1.0));
    }

    /// Keyboard on the sliders: focus moves with wrap-around, Left / Right step 1 % (Shift 10 %), Home / End jump, Reset goes to the base look.
    #[test]
    fn slider_keys_move_focus_and_values() {
        let mut ui = Ui::new(Lang::En, 1.0, 1600.0, 900.0);
        ui.edit = true;
        ui.settings = true;
        let n = ui.slider_count();
        assert_eq!(n, 1 + LOOK.len());
        // no focus yet: values are untouched, Down / Tab take the first slider, Up / Shift+Tab the last
        assert_eq!(ui.slider_key(SKey::Left, false), None);
        assert_eq!(ui.slider_key(SKey::Down, false), Some(SAct::Focus));
        assert_eq!(ui.sfocus, Some(0));
        ui.slider_key(SKey::Tab, false);
        assert_eq!(ui.sfocus, Some(1));
        ui.slider_key(SKey::Up, false);
        ui.slider_key(SKey::Tab, true);
        assert_eq!(ui.sfocus, Some(n - 1));
        ui.slider_key(SKey::Down, false);
        assert_eq!(ui.sfocus, Some(0)); // wraps
        // the glass slider (index 0, base 0.5)
        ui.glass = 0.40;
        let Some(SAct::Set(0, t)) = ui.slider_key(SKey::Right, false) else { panic!() };
        assert!((t - 0.41).abs() < 1e-6);
        let Some(SAct::Set(0, t)) = ui.slider_key(SKey::Left, true) else { panic!() };
        assert!((t - 0.30).abs() < 1e-6);
        assert_eq!(ui.slider_key(SKey::Home, false), Some(SAct::Set(0, 0.0)));
        assert_eq!(ui.slider_key(SKey::End, true), Some(SAct::Set(0, 1.0)));
        assert_eq!(ui.slider_key(SKey::Reset, false), Some(SAct::Set(0, 0.5)));
        ui.glass = 0.995;
        assert_eq!(ui.slider_key(SKey::Right, false), Some(SAct::Set(0, 1.0))); // clamped
        // a look slider resets to where the base look sits
        ui.sfocus = Some(3);
        glass::LOOK[2].set_pos(&mut ui.look, &ui.base.clone(), 0.9);
        let Some(SAct::Set(3, t)) = ui.slider_key(SKey::Reset, false) else { panic!() };
        assert!((t - ui.set_default_pos(3)).abs() < 1e-6 && t < 0.9);
        // the strip of the gallery has the one slider
        ui.settings = false;
        ui.gallery = true;
        ui.sfocus = None;
        assert_eq!(ui.slider_count(), 1);
        ui.slider_key(SKey::Tab, false);
        ui.slider_key(SKey::Tab, false);
        assert_eq!(ui.sfocus, Some(0));
        // 2 % per wheel notch
        assert!((wheel_pos(0.5, 1.0) - 0.52).abs() < 1e-6 && (wheel_pos(0.5, -3.0) - 0.44).abs() < 1e-6);
        assert_eq!((wheel_pos(0.99, 5.0), wheel_pos(0.01, -5.0)), (1.0, 0.0));
        // the whole row answers the wheel
        ui.settings = true;
        let l = ui.set_lay();
        assert_eq!(ui.row_at(l.rows[2].label.x + 3.0, l.rows[2].row.cy()), Some(2));
        assert_eq!(ui.row_at(l.title.x, l.title.y), None);
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
    /// `--look '{"blur":9,...}'`: a `widgetsLook` object for the render (default: the user's config.json; a sample for `--state settings`)
    pub look: Option<String>,
    /// `--update-state idle|checking|latest|available|update-failed|downloading|verifying|failed`: the update line of the settings panel (sample)
    pub update_state: Option<String>,
    /// `--monitors count,selected`: the "Monitor" control of the settings panel (sample: two monitors, the first one)
    pub monitors: Option<(usize, usize)>,
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
    // the look: glass.json / defaults, then `widgetsLook` (--look, else the user's config; a sample look for the settings panel)
    let base = glass::global_params();
    let look_json: Value = match &a.look {
        Some(t) => serde_json::from_str(t).map_err(|e| format!("--look: {e}"))?,
        None if a.state == "settings" => json!({"blur": 7.5, "sat": 1.45, "chroma_r": 0.09, "chroma_b": 0.135, "spec": 0.95, "sheen": 0.29, "sheen_mid": 0.066, "radius_k": 0.8}),
        None if a.no_config => json!({}),
        None => crate::layout::read_look(),
    };
    let look = glass::apply_look(&base, &look_json);
    // the cards follow the look's card scale, the host UI keeps the plain scale
    let cs = s * look.card_k;
    let dims = grid::dims(w as f32 / cs, h as f32 / cs);
    let placed = grid::resolve_all(&dims, &items.iter().map(|i| (i.size, i.col as i32, i.row as i32)).collect::<Vec<_>>());
    let mut specs = Vec::new();
    for (it, p) in items.iter().zip(placed) {
        if let Some(p) = p {
            let (x, y, cw, ch) = cell_rect(it.size, p.col, p.row, cs);
            specs.push(CardSpec { id: it.id.clone(), type_id: it.type_id.clone(), size: it.size, cfg: it.cfg.clone(), x, y, w: cw, h: ch });
        }
    }
    let env = SceneEnv { preview: true, hwnd: 0, lang: crate::layout::lang(), utc_off: crate::sys::local_utc_offset_min() };
    let mut scene = Scene::new(&gfx, w as i32, h as i32, cs, settings.glass, backdrop.bitmap(), &specs, &env).map_err(e("scene"))?;
    scene.set_params(&gfx, &look).map_err(e("look"))?;
    crate::widget::set_tint(std::env::var("MSW_TINT").is_ok() || settings.accent_scope == AccentScope::All); // headless: `MSW_TINT=1` previews accentScope "all"
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
    let edit = matches!(a.state.as_str(), "edit" | "gallery" | "settings");
    ui.edit = edit;
    ui.gallery = a.state == "gallery";
    ui.settings = a.state == "settings";
    ui.look = look;
    ui.base = base;
    ui.appearance = a.appearance.unwrap_or(settings.appearance);
    ui.dim_mode = settings.dim;
    ui.accent_mode = settings.accent;
    ui.accent_scope = settings.accent_scope;
    (ui.mon_count, ui.mon_sel) = a.monitors.unwrap_or(if ui.settings { (2, 0) } else { (1, 0) });
    ui.autostart = true; // sample state: the registry is never read by a headless render
    ui.check_start = settings.check_updates;
    ui.update = match a.update_state.as_deref() {
        Some("checking") => UpdateState::Checking,
        Some("latest") => UpdateState::Latest,
        Some("available") => UpdateState::Available { version: "0.3.0".into(), url: "https://github.com/".into(), asset: None, error: None },
        Some("update-failed") => UpdateState::Available { version: "0.3.0".into(), url: "https://github.com/".into(), asset: None, error: Some("checksum mismatch".into()) },
        Some("downloading") => UpdateState::Downloading(42),
        Some("verifying") => UpdateState::Verifying,
        Some("failed") => UpdateState::Failed,
        _ => UpdateState::Idle,
    };
    ui.anim = 1.0;
    ui.dim = if ui.settings { SETTINGS_DIM } else if edit { 0.28 } else { 0.0 };
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
    ui.hover = a.hover.map_or(Hit::None, |k| if ui.settings { ui.nth_control(k % 100) } else { Hit::Item(k) });
    if let (true, Hit::Look(i, _)) = (ui.settings, ui.hover) {
        ui.sfocus = Some(i); // the hovered slider shows its keyboard focus ring
    }
    if ui.settings && a.hover.is_some_and(|k| k >= 100) {
        ui.press = ui.hover; // --hover 100+k: control k pressed
    }
    if edit && !ui.settings {
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
