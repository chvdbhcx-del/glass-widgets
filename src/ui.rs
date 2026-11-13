//! Host UI drawn on top of the cards while the desk is being edited: the dim, the placeholder under a dragged card, the "−" buttons, the
//! settings strip (glass slider, backdrop mode), the "Edit Widgets" gallery (search, group sidebar, size chips, LIVE previews, Done) and a
//! toast. Everything is laid out in LOGICAL px (absolute screen coordinates) and drawn through `Cx` inside the scene's open frame session.
//! The gallery previews are tiny one-card `Scene`s in preview mode (the same draw / overlay code as the desk) cached per (widget, size).
use crate::cards::*;
use crate::gfx::*;
use crate::glass::{self, GlassParams, GlassStyle, LOOK};
use crate::grid;
use crate::layout::{AccentMode, AccentScope, Dim};
use crate::input::{self, PromptOpts};
use crate::motion::{self, AppearancePref, Bump, HoverRamps};
use crate::menu::{self, Entry, MenuPanel};
use crate::prompt::{self, Act, Focus, Key, Mods, PromptPanel};
use crate::registry::{self, WidgetDef};
use crate::update::UpdateState;
use crate::widget::*;
use std::collections::HashMap;
use windows::Foundation::Numerics::Matrix3x2;
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;

mod anim;
mod state;
mod hit_previews;
mod overlays;
mod draw;

/// preview scale (gallery card = this fraction of the real card)
pub const K: f32 = 0.6;
const PANEL_W: f32 = 780.0;
const PANEL_H: f32 = 430.0;
const PANEL_R: f32 = 26.0;
const SIDE_W: f32 = 200.0;
const FOOT_H: f32 = 58.0;
const TB_W: f32 = 576.0;
const TB_H: f32 = 40.0;
const MINUS_R: f32 = 13.0;
/// settings panel: width, height of the title + column header block above the first row, row pitch, footer height
const SET_W: f32 = 760.0;
const SET_TOP: f32 = 74.0;
const SET_ROW: f32 = 31.0;
const SET_FOOT: f32 = 56.0;
/// width of the label column of both settings columns: the longest label ("Источник стекла", "Corner roundness") plus a gap of at least 12 px to its control
const SET_LABEL_W: f32 = 124.0;
/// rows of the right column (segmented controls, the autostart switch)
const OPT_ROW: f32 = 38.0;
/// dim over the desktop while the settings panel is up (lighter than the edit mode's: the look is being judged)
pub const SETTINGS_DIM: f32 = 0.12;
pub const SLOT_GALLERY: usize = 0;
pub const SLOT_TOOLBAR: usize = 1;
/// the text prompt (a glass panel of its own; it shares the region bit of the gallery, see host.rs `sync_regions`)
pub const SLOT_PROMPT: usize = 2;
const PROMPT_R: f32 = 22.0;
/// the context menu and its submenu (glass panels of their own; they share the region bit of the gallery like the prompt)
pub const SLOT_MENU: usize = 3;
pub const SLOT_SUB: usize = 4;
/// text of the menu rows; a separator is inset this far from the panel's rows
const MENU_FONT: Style = Style::new(13.0, 500);
const PAD_SEP: f32 = 8.0;

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
    /// the UI language: auto / Russian / English (`lang`)
    Language,
}
/// At most this many monitors get a segment (the tray menu lists all of them).
pub const MON_SEGS: usize = 5;
impl Opt {
    pub const ALL: [Opt; 7] = [Opt::Appearance, Opt::Dim, Opt::Source, Opt::Accent, Opt::Scope, Opt::Monitor, Opt::Language];
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
    /// Option `k` as the language preference (Auto, Russian, English).
    pub fn language(k: usize) -> crate::i18n::LangPref {
        crate::i18n::LangPref::from_index(k)
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
    /// the language SETTING (auto / ru / en); `lang` is what it resolves to
    pub lang_pref: crate::i18n::LangPref,
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
    /// toast motion: seconds since it appeared, seconds into its fade-out (None = held), the opacity and the rise (logical px) being drawn
    pub toast_t: f32,
    /// gallery: seconds since the list of previews changed (the tiles fade in with a stagger) and a signature of the list shown
    gal_fade: f32,
    gal_sig: u64,
    pub toast_out: Option<f32>,
    toast_a: f32,
    toast_rise: f32,
    /// the motion of the settings panel's controls (hover ramps, sliding segment, switches ...)
    sa: SetAnim,
    /// the modal text prompt drawn on the canvas (any mode); None = no prompt
    pub prompt: Option<PromptPanel>,
    /// the context menu of a card drawn on the canvas (any mode); None = no menu
    pub menu: Option<MenuPanel>,
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
/// Selected row of the gallery: a lighter glass, no accent (the panels stay neutral on any wallpaper).
const SEL: Color = Color::rgba(1.0, 1.0, 1.0, 0.24);
/// Text on a white button / segment.
const ON_WHITE: Color = Color::rgba(0.07, 0.07, 0.09, 1.0);

/// The "Done" button of both panels: white glass pill with dark text (like the selected segment).
fn done_button(cx: &Cx, r: Rect, label: &str, hot: bool) {
    done_button_k(cx, r, label, if hot { 1.0 } else { 0.0 });
}

/// `done_button` with a hover amount 0..1 (the settings panel ramps it).
fn done_button_k(cx: &Cx, r: Rect, label: &str, hot: f32) {
    cx.pill(r.offset(0.0, 1.5), Color::rgba(0.0, 0.0, 0.0, 0.20));
    cx.pill(r, Color::rgba(1.0, 1.0, 1.0, 0.92 + 0.08 * hot));
    cx.text(r, label, Style::new(13.0, 600).center(), ON_WHITE);
}

// ---------------------------------------------------------------- settings panel motion

// Animation keys of the controls (`hkey`): the look sliders first, then everything else.
const K_RESET: usize = 16;
const K_OPT: usize = 20; // + option index * MON_SEGS + segment
const K_AUTO: usize = K_OPT + Opt::ALL.len() * MON_SEGS;
const K_CHECK: usize = K_AUTO + 1;
const K_EDIT: usize = K_AUTO + 2;
const K_QUIT: usize = K_AUTO + 3;
const K_UPD: usize = K_AUTO + 4;
const K_DL: usize = K_AUTO + 5;
const K_PAGE: usize = K_AUTO + 6;
const K_DONE: usize = K_AUTO + 7;
/// not a control: the update status line (a dirty-rect key only)
const K_STATUS: usize = K_AUTO + 8;
const N_KEYS: usize = K_STATUS;
/// Scale of a pressed button / switch / slider knob (a spring, `Bump`).
const PRESS_BTN: f32 = 0.97;
/// Hover ramp timings (seconds), the sliding segment's follow constant, the switch cross-fade, the status / button fade.
const HOV_IN: f32 = 0.12;
const HOV_OUT: f32 = 0.18;
const SEG_TAU: f32 = 0.06;
const SLIDE_TAU: f32 = 0.05;
const SW_T: f32 = 0.17;
const FADE_T: f32 = 0.15;
/// Gallery tiles: fade-in time and the delay between neighbours.
const GAL_FADE: f32 = 0.18;
const GAL_STAGGER: f32 = 0.02;

fn opt_idx(o: Opt) -> usize {
    match o {
        Opt::Appearance => 0,
        Opt::Dim => 1,
        Opt::Source => 2,
        Opt::Accent => 3,
        Opt::Scope => 4,
        Opt::Monitor => 5,
        Opt::Language => 6,
    }
}

/// The animation key of a hit target of the settings panel (None = nothing animated there).
pub fn hkey(h: Hit) -> Option<usize> {
    Some(match h {
        Hit::Look(i, _) => i.min(K_RESET - 1),
        Hit::ResetLook => K_RESET,
        Hit::Opt(o, k) => K_OPT + opt_idx(o) * MON_SEGS + k.min(MON_SEGS - 1),
        Hit::Autostart => K_AUTO,
        Hit::CheckStart => K_CHECK,
        Hit::EditBtn => K_EDIT,
        Hit::QuitBtn => K_QUIT,
        Hit::UpdBtn => K_UPD,
        Hit::UpdInstall => K_DL,
        Hit::UpdPage => K_PAGE,
        Hit::Done => K_DONE,
        _ => return None,
    })
}

/// Which kind of status line the update state shows (a change of kind cross-fades; the percent of a download does not).
fn upd_key(u: &UpdateState) -> u8 {
    match u {
        UpdateState::Idle => 0,
        UpdateState::Checking => 1,
        UpdateState::Latest => 2,
        UpdateState::Available { error: Some(_), .. } => 3,
        UpdateState::Available { .. } => 4,
        UpdateState::Downloading(_) => 5,
        UpdateState::Verifying => 6,
        UpdateState::Failed => 7,
    }
}

/// The state the settings controls are drawn from while they move; `Ui::step_sa` (from the host's animation timer) walks it towards the
/// real values, `Ui::sa_busy` says when it has arrived (then the timer may stop).
struct SetAnim {
    /// hover amount per key (hovered or pressed)
    hov: HoverRamps,
    /// press spring per key (buttons, switches, slider knobs)
    prs: Vec<Bump>,
    /// the handle position of each look slider
    kp: [Option<f32>; K_RESET],
    /// the sliding pill of each segmented control, in segments
    seg: [Option<f32>; Opt::ALL.len()],
    /// the autostart / check-on-start switches, 0..1 linear
    sw: [Option<f32>; 2],
    /// a slider press: the slider and the pointer x (canvas px) it started at; `snapped` once the pointer left that spot (1:1 from then on)
    drag: Option<(usize, i32)>,
    snapped: bool,
    /// the status line: kind being shown, the line before a change and the progress of the cross-fade (1 = settled), the line last shown
    st_key: u8,
    st_prev: Option<(String, Color)>,
    st_t: f32,
    st_last: (String, Color),
    /// presence of the "Release page" / "Update now" buttons
    btns: f32,
}

impl SetAnim {
    fn new() -> SetAnim {
        SetAnim {
            hov: HoverRamps::new(N_KEYS),
            prs: vec![Bump::default(); N_KEYS],
            kp: [None; K_RESET],
            seg: [None; Opt::ALL.len()],
            sw: [None; 2],
            drag: None,
            snapped: false,
            st_key: 0,
            st_prev: None,
            st_t: 1.0,
            st_last: (String::new(), FAINT),
            btns: 0.0,
        }
    }

    /// How far a key is pressed, 0..1.
    fn press(&self, k: usize) -> f32 {
        self.prs.get(k).map_or(0.0, |b| ((1.0 - b.x) / (1.0 - PRESS_BTN)).clamp(0.0, 1.0))
    }
}

impl Ui {
}

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
            // typography: every label keeps a gap of at least 12 px to its control, both columns share the label width, a 3-segment control keeps >= 56 px a segment,
            // and the first control row of the right column sits on the centre line of the first slider row
            assert!(l.rows.iter().all(|r| r.track.x - r.label.right() >= 12.0 - 0.01 && r.label.w == SET_LABEL_W));
            assert!(l.opts.iter().all(|o| o.seg.x - o.label.right() >= 12.0 - 0.01 && o.label.w == SET_LABEL_W && o.seg.w / 3.0 >= 56.0));
            assert!((l.opts[0].seg.cy() - l.rows[0].row.cy()).abs() < 0.01, "{} vs {}", l.opts[0].seg.cy(), l.rows[0].row.cy());
            assert!(l.reset.w >= 104.0 && l.reset.cy() == l.look_hdr.cy());
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
            assert!(matches!(ui.nth_control(0), Hit::Look(0, _)) && ui.nth_control(l.rows.len() + 15) == Hit::Done && ui.nth_control(99) == Hit::None);
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

    /// The language row of the settings panel: three segments (auto / Russian / English), the selection is the SETTING (not what it resolves
    /// to), a click lands on the right segment, and `set_language` switches the texts and throws the cached gallery previews away.
    #[test]
    fn language_control_and_live_switch() {
        use crate::i18n::LangPref;
        let mut ui = Ui::new(Lang::Ru, 1.0, 1600.0, 900.0);
        ui.edit = true;
        ui.settings = true;
        assert_eq!((Opt::Language.count(), ui.opt_n(Opt::Language), ui.opt_sel(Opt::Language)), (3, 3, 0), "auto by default");
        let l = ui.set_lay();
        let o = l.opts.iter().find(|o| o.which == Opt::Language).expect("language row");
        for k in 0..3 {
            assert_eq!(ui.set_hit(o.seg.x + o.seg.w * (k as f32 + 0.5) / 3.0, o.seg.cy()), Hit::Opt(Opt::Language, k));
            assert_eq!(Opt::language(k), LangPref::from_index(k));
        }
        assert!(l.panel.contains(o.seg.cx(), o.seg.cy()) && o.seg.bottom() <= l.auto_row.y + 0.5, "the row sits above the switches");
        assert!(l.edit_btn.bottom() < l.panel.bottom());
        // Russian Windows + "auto" shows Russian, the selection still says auto; picking English changes both
        assert_eq!((ui.t("Язык", "Language"), ui.lang_pref), ("Язык", LangPref::Auto));
        ui.set_language(LangPref::En, Lang::En);
        assert_eq!((ui.t("Язык", "Language"), ui.opt_sel(Opt::Language)), ("Language", 2));
        ui.set_language(LangPref::Auto, Lang::Ru);
        assert_eq!((ui.t("Язык", "Language"), ui.opt_sel(Opt::Language)), ("Язык", 0));
        // keyboard order: the language row is reachable like the others
        assert!((0..40).any(|k| ui.nth_control(k) == Hit::Opt(Opt::Language, 1)));
        // every row has its own animation key
        let keys: std::collections::HashSet<usize> = Opt::ALL.iter().map(|o| hkey(Hit::Opt(*o, 0)).unwrap()).collect();
        assert_eq!(keys.len(), Opt::ALL.len());
        assert!(hkey(Hit::Opt(Opt::Language, MON_SEGS - 1)).unwrap() < hkey(Hit::Autostart).unwrap());
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

    /// Steps the settings motion until it stops asking for the timer; returns (steps, repainted rectangles).
    fn settle(ui: &mut Ui, reduced: bool) -> (usize, Vec<RECT>) {
        let (mut n, mut rects) = (0, Vec::new());
        while ui.sa_busy() {
            rects.extend(ui.step_sa(0.016, reduced));
            n += 1;
            assert!(n < 200, "the settings motion never settles");
        }
        (n, rects)
    }

    fn settings_ui() -> Ui {
        let mut ui = Ui::new(Lang::En, 1.0, 1600.0, 900.0);
        ui.edit = true;
        ui.settings = true;
        ui.sa_reset();
        ui
    }

    /// Hover ramps in and out, then nothing is busy; repaints stay inside the control, not the whole panel.
    #[test]
    fn settings_hover_ramps_settle_with_narrow_dirty_rects() {
        let mut ui = settings_ui();
        assert!(!ui.sa_busy(), "a freshly opened panel is idle");
        let panel = ui.settings_rect();
        let quit = ui.set_lay().quit_btn;
        ui.hover = Hit::QuitBtn;
        assert!(ui.sa_busy());
        let (n, rects) = settle(&mut ui, false);
        assert!((6..=12).contains(&n), "120 ms in: {n}");
        assert_eq!(ui.sa.hov.get(K_QUIT), 1.0);
        for r in &rects {
            assert!(r.right - r.left < 200 && r.bottom - r.top < 80, "only the button repaints: {r:?}");
            assert!(r.left as f32 >= quit.x - 12.0 && r.right as f32 <= quit.right() + 12.0 && r.right as f32 <= panel.right());
        }
        ui.hover = Hit::None;
        let (n, _) = settle(&mut ui, false);
        assert!((9..=14).contains(&n), "180 ms out: {n}");
        assert!(!ui.sa_busy() && ui.step_sa(0.016, false).is_none(), "settled: no timer, no repaint");
        // pressed: the spring shrinks the button, release lets it go (and it comes to rest exactly)
        ui.press = Hit::QuitBtn;
        ui.step_sa(0.016, false);
        ui.step_sa(0.016, false);
        assert!(ui.sa.prs[K_QUIT].x < 1.0);
        ui.press = Hit::None;
        settle(&mut ui, false);
        assert_eq!(ui.sa.prs[K_QUIT].x, 1.0);
    }

    /// The selected-segment pill slides to the new option and the control stops asking for frames once it is there.
    #[test]
    fn settings_segment_pill_slides_to_the_option() {
        let mut ui = settings_ui();
        assert_eq!(ui.seg_shown(Opt::Appearance), 0.0);
        ui.appearance = AppearancePref::Auto; // option 2
        assert!(ui.sa_busy());
        ui.step_sa(0.016, false);
        let mid = ui.seg_shown(Opt::Appearance);
        assert!(mid > 0.0 && mid < 2.0, "in between after one step: {mid}");
        let (n, _) = settle(&mut ui, false);
        assert!(n > 5 && n < 80, "{n}");
        assert_eq!(ui.seg_shown(Opt::Appearance), 2.0);
        // reduced motion: straight there
        ui.appearance = AppearancePref::Dark;
        let (n, _) = settle(&mut ui, true);
        assert_eq!((n, ui.seg_shown(Opt::Appearance)), (1, 0.0));
    }

    /// Switch fill and knob glide in ~170 ms; a slider handle glides to a Reset / track click but a drag is 1:1.
    #[test]
    fn settings_switch_and_slider_glide_but_drag_is_1_to_1() {
        let mut ui = settings_ui();
        ui.autostart = true;
        let (n, _) = settle(&mut ui, false);
        assert!((9..=12).contains(&n), "170 ms: {n}");
        assert_eq!(ui.sw_shown(0, true), 1.0);
        // glass slider set by Reset / the keyboard: the handle trails the value
        ui.glass = 0.9;
        assert_eq!(ui.slider_shown(0), 0.5, "still at the old spot");
        ui.step_sa(0.016, false);
        let k = ui.slider_shown(0);
        assert!(k > 0.5 && k < 0.9, "{k}");
        settle(&mut ui, false);
        assert_eq!(ui.slider_shown(0), 0.9);
        // a click on the track far from the handle glides; moving the pointer makes it 1:1
        ui.sa_drag_start(0, 100, 0.1);
        ui.press = Hit::Look(0, 0.1);
        ui.glass = 0.1;
        ui.step_sa(0.016, false);
        assert!(ui.slider_shown(0) > 0.1, "glides to the click");
        ui.sa_drag_move(0, 101);
        assert!(ui.slider_shown(0) > 0.1, "a 1 px wobble is not a drag");
        ui.sa_drag_move(0, 120);
        assert_eq!(ui.slider_shown(0), 0.1, "dragging: no lag");
        ui.glass = 0.4;
        assert_eq!(ui.slider_shown(0), 0.4);
        ui.sa_drag_end();
        ui.press = Hit::None;
        assert_eq!(ui.slider_shown(0), 0.4, "the handle stays where the drag left it");
        settle(&mut ui, false);
        // pressed on the handle itself: 1:1 at once
        ui.sa_drag_start(0, 500, ui.slider_shown(0));
        assert!(ui.sa.snapped);
    }

    /// The status line cross-fades when its kind changes (not for every download percent), the update buttons fade in.
    #[test]
    fn settings_status_line_crossfades() {
        let mut ui = settings_ui();
        ui.update = UpdateState::Checking;
        assert!(ui.sa_busy());
        ui.step_sa(0.016, false);
        assert!(ui.sa.st_t < 1.0 && ui.sa.st_prev.is_some());
        settle(&mut ui, false);
        assert!(ui.sa.st_prev.is_none() && ui.sa.st_t == 1.0);
        ui.update = UpdateState::Downloading(10);
        settle(&mut ui, false);
        ui.update = UpdateState::Downloading(11);
        assert!(!ui.sa_busy(), "a percent is a text change, not a fade");
        ui.update = UpdateState::Available { version: "9.9.9".into(), url: String::new(), asset: None, error: None };
        let (_, rects) = settle(&mut ui, false);
        assert!(!rects.is_empty() && ui.sa.btns == 1.0);
        ui.update = UpdateState::Latest;
        settle(&mut ui, false);
        assert_eq!(ui.sa.btns, 0.0);
        // reduced motion: no fade at all
        ui.update = UpdateState::Failed;
        let (n, _) = settle(&mut ui, true);
        assert!(n <= 1 && ui.sa.st_t == 1.0 && ui.sa.st_prev.is_none());
    }

    /// Gallery tiles fade in with a stagger when the list changes, not when the same list is asked for again; reduced motion shows them at once.
    #[test]
    fn gallery_tiles_fade_in_on_a_new_list() {
        let mut ui = Ui::new(Lang::En, 1.0, 1600.0, 900.0);
        ui.edit = true;
        ui.gallery = true;
        ui.gal_note(); // the first list: nothing to fade
        assert!(!ui.gal_busy() && ui.tile_alpha(0) == 1.0);
        ui.select_group(1);
        ui.gal_note();
        assert!(ui.gal_busy());
        assert_eq!((ui.tile_alpha(0), ui.tile_alpha(5)), (0.0, 0.0));
        let mut n = 0;
        while ui.gal_busy() {
            let r = ui.step_gal(0.016, false).expect("repaints the tile area");
            assert!(r.bottom - r.top < 600, "only the grid, not the whole screen");
            if n == 4 {
                assert!(ui.tile_alpha(0) > ui.tile_alpha(3) && ui.tile_alpha(3) > ui.tile_alpha(9), "a stagger: the first tile leads");
            }
            n += 1;
            assert!(n < 60);
        }
        assert!((20..=28).contains(&n), "380 ms: {n}");
        assert_eq!(ui.tile_alpha(9), 1.0);
        ui.gal_note(); // the same list again: no restart
        assert!(!ui.gal_busy());
        ui.select_group(0);
        ui.gal_note();
        assert!(ui.gal_busy());
        assert!(ui.step_gal(0.016, true).is_some() && !ui.gal_busy() && ui.tile_alpha(9) == 1.0);
    }

    /// A toast rises while it fades in, fades out after its hold and is only dropped at the end of the fade; reduced motion is instant.
    #[test]
    fn toast_lifetime() {
        let mut ui = Ui::new(Lang::En, 1.0, 1600.0, 900.0);
        ui.toast_show("hi".into(), false);
        assert!(ui.toast_busy(false) && ui.toast_a == 0.0 && ui.toast_rise == motion::TOAST_RISE);
        let mut n = 0;
        while ui.toast_busy(false) {
            let (ch, gone) = ui.toast_step(0.016, false);
            assert!(ch && !gone);
            n += 1;
            assert!(n < 30);
        }
        assert!((10..=12).contains(&n), "180 ms: {n}");
        assert_eq!((ui.toast_a, ui.toast_rise), (1.0, 0.0));
        assert!(!ui.toast_busy(false), "held: the timer stops");
        // a new text while it is up does not rise again
        ui.toast_show("again".into(), false);
        assert_eq!((ui.toast_a, ui.toast_rise), (1.0, 0.0));
        ui.toast_fade();
        assert!(ui.toast_busy(false));
        let mut n = 0;
        loop {
            let (_, gone) = ui.toast_step(0.016, false);
            n += 1;
            if gone {
                break;
            }
            assert!(ui.toast.is_some(), "kept until the fade ended");
            assert!(n < 30);
        }
        assert!((13..=15).contains(&n), "220 ms: {n}");
        assert!(ui.toast.is_none() && !ui.toast_busy(false));
        // reduced motion: shown at full opacity at once, gone on the first step after the hold
        ui.toast_show("x".into(), true);
        assert_eq!((ui.toast_a, ui.toast_rise), (1.0, 0.0));
        assert!(!ui.toast_busy(true));
        ui.toast_fade();
        assert_eq!(ui.toast_step(0.016, true), (true, true));
        assert!(ui.toast.is_none());
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

    /// The prompt on a real scene: it opens next to the anchor inside the work area, typing / Enter / Esc / a click outside go through the
    /// same `Ui` entry points as the host, the frame shows the veil and the panel (and nothing of either without a prompt), and the
    /// dirty rectangle covers it.
    #[test]
    fn prompt_opens_types_and_draws() {
        use crate::backdrop::{Backdrop, PlainBackdrop};
        unsafe {
            let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_MULTITHREADED);
        }
        let gfx = Gfx::new(None).unwrap();
        let bd = PlainBackdrop::new(&gfx, 1000, 700).unwrap();
        let env = SceneEnv { preview: true, hwnd: 0, lang: Lang::En, utc_off: 0 };
        let mut scene = Scene::new(&gfx, 1000, 700, 1.0, 0.5, bd.bitmap(), &[], &env).unwrap();
        let mut ui = Ui::new(Lang::En, 1.0, 1000.0, 700.0);
        assert!(ui.dirty_rect(&scene).is_none() && ui.scrim() == 0.0);
        let o = PromptOpts { title: "New reminder".into(), initial: String::new(), multiline: false, placeholder: "Remind me to…".into(), ok_label: String::new(), lang: Lang::En, anchor: None };
        ui.open_prompt(&scene, &gfx, &o, (500.0, 300.0), Rect::new(0.0, 0.0, 1000.0, 660.0));
        let lay = ui.prompt.as_ref().unwrap().lay;
        assert!(lay.panel.contains(500.0, 400.0) && lay.panel.y > 300.0 && lay.panel.right() <= 992.0 && lay.panel.bottom() <= 652.0);
        assert!(ui.dirty_rect(&scene).is_some_and(|d| d.left < lay.panel.x as i32 && d.right > lay.panel.right() as i32 && d.bottom > lay.panel.bottom() as i32));
        // opening: the panel is still invisible, fully open it is on the frame together with the veil
        let draw = |ui: &mut Ui, scene: &mut Scene| -> (u8, u8) {
            ui.sync_panels(scene, &gfx);
            let full = RECT { left: 0, top: 0, right: 1000, bottom: 700 };
            let now = Now::sample();
            scene.prepare(&gfx, full, &now).unwrap();
            scene.begin(&gfx, full, Some(bd.bitmap()));
            ui.draw_pre(&gfx, full);
            scene.draw_cards(&gfx, full, &now);
            ui.draw_post(scene, &gfx, &now, full);
            scene.end(&gfx).unwrap();
            let mut px = vec![0u8; 1000 * 700 * 4];
            scene
                .read_rect(&gfx, full, |y, row| {
                    let o = y as usize * 4000;
                    px[o..o + row.len()].copy_from_slice(row);
                })
                .unwrap();
            let a = |x: usize, y: usize| px[(y * 1000 + x) * 4 + 3];
            (a(5, 5), a(lay.panel.cx() as usize, (lay.panel.y + 4.0) as usize))
        };
        let base = draw(&mut ui, &mut scene); // the plain backdrop alone (the panel at 0 % draws nothing)
        ui.prompt.as_mut().unwrap().t = 1.0;
        let (veil, panel) = draw(&mut ui, &mut scene);
        assert!(veil > base.0 && veil < base.0 + 40, "veil alpha {veil} over {}", base.0); // the 12 % veil: what catches the clicks outside the panel
        assert!(panel > base.1 + 20, "panel alpha {panel} over {}", base.1);
        // keys: type, Esc cancels, Enter confirms
        for c in "buy milk".chars() {
            assert_eq!(ui.prompt_key(&scene, &gfx, Key::Char(c), Mods::default()), Act::Redraw);
        }
        assert_eq!(ui.prompt.as_ref().unwrap().f.text(), "buy milk");
        assert_eq!(ui.prompt_key(&scene, &gfx, Key::Enter, Mods::default()), Act::Done(Some("buy milk".into())));
        assert_eq!(ui.prompt_key(&scene, &gfx, Key::Esc, Mods::default()), Act::Done(None));
        // the mouse: a click on the text puts the caret there (measured with the real font), outside the panel cancels
        let t = ui.prompt.as_ref().unwrap().lay.text;
        assert_eq!(ui.prompt_mouse(&scene, &gfx, 1, t.x + 120.0, t.cy(), Mods::default(), false), Act::Redraw);
        assert_eq!(ui.prompt.as_ref().unwrap().f.caret, 8);
        ui.prompt_mouse(&scene, &gfx, 2, t.x, t.cy(), Mods::default(), false);
        assert_eq!(ui.prompt_mouse(&scene, &gfx, 1, 3.0, 3.0, Mods::default(), false), Act::Done(None));
        // closed again: no veil, no panel
        ui.prompt = None;
        assert_eq!(draw(&mut ui, &mut scene), base);
        assert!(ui.dirty_rect(&scene).is_none());
    }

    /// The context menu on a real scene: it opens at the cursor with the real font widths inside the work area, the frame shows the faint
    /// veil, the root panel and (after the click on "Size") the submenu panel, and nothing of it once closed; the dirty rectangle covers it.
    #[test]
    fn menu_opens_navigates_and_draws() {
        use crate::backdrop::{Backdrop, PlainBackdrop};
        unsafe {
            let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_MULTITHREADED);
        }
        let gfx = Gfx::new(None).unwrap();
        let bd = PlainBackdrop::new(&gfx, 1000, 700).unwrap();
        let env = SceneEnv { preview: true, hwnd: 0, lang: Lang::En, utc_off: 0 };
        let mut scene = Scene::new(&gfx, 1000, 700, 1.0, 0.5, bd.bitmap(), &[], &env).unwrap();
        let mut ui = Ui::new(Lang::En, 1.0, 1000.0, 700.0);
        assert!(ui.dirty_rect(&scene).is_none() && ui.scrim() == 0.0);
        let work = Rect::new(0.0, 0.0, 1000.0, 660.0);
        ui.open_menu(&scene, &gfx, menu::card_menu(Lang::En, false, &Size::ALL, Size::M), (400.0, 300.0), work);
        let root = ui.menu.as_ref().unwrap().root.clone();
        assert_eq!((root.panel.x, root.panel.y), (400.0, 300.0));
        assert!(root.panel.w > 150.0 && root.panel.w < 260.0, "{:?}", root.panel); // measured with the real font, not the 176 px floor of a short list
        assert!(ui.scrim() > 0.0 && ui.scrim() < 0.05);
        assert!(ui.dirty_rect(&scene).is_some_and(|d| d.left < 400 && d.right > root.panel.right() as i32 && d.bottom > root.panel.bottom() as i32));
        let draw = |ui: &mut Ui, scene: &mut Scene| -> (u8, u8, u8) {
            ui.sync_panels(scene, &gfx);
            let full = RECT { left: 0, top: 0, right: 1000, bottom: 700 };
            let now = Now::sample();
            scene.prepare(&gfx, full, &now).unwrap();
            scene.begin(&gfx, full, Some(bd.bitmap()));
            ui.draw_pre(&gfx, full);
            scene.draw_cards(&gfx, full, &now);
            ui.draw_post(scene, &gfx, &now, full);
            scene.end(&gfx).unwrap();
            let mut px = vec![0u8; 1000 * 700 * 4];
            scene
                .read_rect(&gfx, full, |y, row| {
                    let o = y as usize * 4000;
                    px[o..o + row.len()].copy_from_slice(row);
                })
                .unwrap();
            let a = |x: usize, y: usize| px[(y * 1000 + x) * 4 + 3];
            let sub_probe = root.panel.right() as usize + 40;
            (a(5, 5), a(root.panel.x as usize + 3, root.panel.cy() as usize), a(sub_probe, (root.rows[1].cy()) as usize))
        };
        let base = draw(&mut ui, &mut scene); // the veil only: the panel is still at 0 %
        ui.menu.as_mut().unwrap().t = 1.0;
        let (veil, panel, sub0) = draw(&mut ui, &mut scene);
        assert!(sub0 <= veil + 2, "no submenu yet: {sub0}");
        // a click on "Size" opens the submenu: a second glass panel next to it
        let r = root.rows[1];
        {
            let m = ui.menu.as_mut().unwrap();
            assert_eq!(m.mouse_down(r.cx(), r.cy()), menu::MAct::Layout);
            m.sub_t = 1.0;
        }
        let (_, _, sub1) = draw(&mut ui, &mut scene);
        assert!(sub1 > sub0 + 20, "submenu alpha {sub1} over {sub0}");
        assert_eq!(scene.panels.iter().filter(|p| p.is_some()).count(), 2);
        // keys and the mouse go through the model: Down walks, Esc closes, a click outside closes, a click on a size picks it
        let sub = ui.menu.as_ref().unwrap().sub.as_ref().unwrap().1.clone();
        assert_eq!(ui.menu.as_mut().unwrap().mouse_down(3.0, 3.0), menu::MAct::Close);
        let m = ui.menu.as_mut().unwrap();
        m.mouse_down(sub.rows[2].cx(), sub.rows[2].cy());
        assert_eq!(m.mouse_up(sub.rows[2].cx(), sub.rows[2].cy()), menu::MAct::Pick(menu::CMD_SIZE + 2));
        assert_eq!(m.key(Key::Esc), menu::MAct::Close);
        // closed again: no veil, no panels, nothing dirty
        ui.menu = None;
        let gone = draw(&mut ui, &mut scene);
        // the veil is faint but not zero (what catches the clicks outside the panels), the panel is far stronger than the veil
        assert!(veil > gone.0 && veil < gone.0 + 20, "veil alpha {veil} over {}", gone.0);
        assert!(panel > gone.1 + 20 && panel > base.1 + 20, "panel alpha {panel} over {}", gone.1);
        assert!(ui.dirty_rect(&scene).is_none() && scene.panels.iter().all(|p| p.is_none()));
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

/// `--dump-ui`: compose the host UI of a state (live | edit | gallery | settings | prompt | prompt-multi | menu) headless into a PNG with the SAME scene / UI code as the live host:
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
    ui.lang_pref = crate::i18n::pref();
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
    if ui.settings {
        // the hover / press look is drawn from the eased state: let it settle (a still render shows the end of the motion)
        ui.sa_reset();
        for _ in 0..40 {
            ui.step_sa(0.03, false);
        }
        // `MSW_SA_MS=n`: a motion still instead: Appearance -> Auto, autostart off, Glass -> 90 % set `n` ms ago
        if let Some(ms) = std::env::var("MSW_SA_MS").ok().and_then(|v| v.parse::<u32>().ok()) {
            ui.appearance = AppearancePref::Auto;
            ui.autostart = false;
            ui.glass = 0.9;
            for _ in 0..ms / 16 {
                ui.step_sa(0.016, false);
            }
        }
    }
    if edit && !ui.settings {
        for i in 0..scene.len() {
            scene.set_xf(i, Xf { rot: jiggle(i, 0.13), ..Xf::ID });
        }
    }
    if a.state.starts_with("menu") {
        // `--state menu|menu-size|menu-one [--hover n]`: the context menu of the first card, opened on it (`menu-size`: with the Size submenu
        // open, `menu-one`: a widget with one size, so "Size" is greyed); `--hover n` highlights root row n (submenu row n for `menu-size`)
        let (anchor, sizes, cur) = match scene.cards.first() {
            Some(c) => (((c.x as f32 + c.w as f32 * 0.5) / s, (c.y as f32 + c.h as f32 * 0.45) / s), registry::global().get(&c.type_id).map_or(Size::ALL.to_vec(), |d| d.sizes.to_vec()), c.size),
            None => ((lw * 0.4, lh * 0.4), Size::ALL.to_vec(), Size::M),
        };
        let sizes = if a.state == "menu-one" { vec![cur] } else { sizes };
        // `menu-click`: the "Click action" submenu of the first card (its widget must have one) open, `--hover n` = its row n
        let click = if a.state == "menu-click" { scene.cards.first().and_then(|c| menu::click_item(env.lang, &c.type_id, &crate::sys::targets::Click::from_cfg(&c.cfg))) } else { None };
        ui.open_menu(&scene, &gfx, menu::card_menu_with(env.lang, false, &sizes, cur, click), anchor, Rect::new(0.0, 0.0, lw, lh));
        if let Some(m) = ui.menu.as_mut() {
            (m.t, m.sub_t) = (1.0, 1.0);
            if a.state == "menu-size" || a.state == "menu-click" {
                m.open_sub(if a.state == "menu-click" { 2 } else { 1 }, false);
                m.sub_t = 1.0;
                m.hot[1] = Some(a.hover.unwrap_or(1));
            } else {
                m.hot[0] = Some(a.hover.unwrap_or(0));
            }
        }
    }
    if a.state.starts_with("prompt") {
        // `--state prompt|prompt-multi [--search text] [--hover 1]`: the text prompt over the desk (`--search` = the text in the field, `--hover 1`
        // selects its last word, else the caret sits at the end; no text = the empty field with its placeholder)
        let multi = a.state == "prompt-multi";
        let text = a.search.clone().unwrap_or_default();
        let (title, hint) = if multi { (("Новая заметка", "New note"), ("Текст заметки…", "Write a note…")) } else { (("Новое напоминание", "New reminder"), ("Что напомнить?", "Remind me to…")) };
        let o = PromptOpts { title: env.lang.pick(title.0, title.1).into(), initial: text.clone(), multiline: multi, placeholder: env.lang.pick(hint.0, hint.1).into(), ok_label: String::new(), lang: env.lang, anchor: None };
        ui.open_prompt(&scene, &gfx, &o, (lw / 2.0, lh * 0.42), Rect::new(0.0, 0.0, lw, lh));
        ui.st.focus = false;
        if let Some(p) = ui.prompt.as_mut() {
            p.t = 1.0;
            if let (Some(1), Some(i)) = (a.hover, text.rfind(' ')) {
                p.f.anchor = text[..=i].chars().count();
            }
        }
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
