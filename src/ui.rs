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
const K_AUTO: usize = K_OPT + 6 * MON_SEGS;
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
    seg: [Option<f32>; 6],
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
            seg: [None; 6],
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
    fn slider_shown(&self, i: usize) -> f32 {
        if self.sa.snapped && self.sa.drag.is_some_and(|d| d.0 == i) {
            return self.set_pos(i);
        }
        self.sa.kp.get(i).copied().flatten().unwrap_or_else(|| self.set_pos(i))
    }

    /// Where a segmented control's pill is drawn, in segments.
    fn seg_shown(&self, o: Opt) -> f32 {
        self.sa.seg[opt_idx(o)].unwrap_or_else(|| self.opt_sel(o) as f32)
    }

    fn sw_shown(&self, j: usize, on: bool) -> f32 {
        self.sa.sw[j].unwrap_or(if on { 1.0 } else { 0.0 })
    }

    /// The status line of the update check: text and colour.
    fn upd_line(&self) -> (String, Color) {
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
    fn sa_keys(&self) -> (Option<usize>, Option<usize>) {
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
    fn sa_rect(&self, l: &SetLay, k: usize) -> Option<Rect> {
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
    fn tile_alpha(&self, k: usize) -> f32 {
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
    fn draw_menu_level(&self, m: &MenuPanel, lvl: usize, slot: usize, alpha: f32, scene: &Scene, gfx: &Gfx, now: &Now) {
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

    fn draw_menu(&self, m: &MenuPanel, scene: &Scene, gfx: &Gfx, now: &Now) {
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

    fn draw_prompt(&self, p: &PromptPanel, scene: &Scene, gfx: &Gfx, now: &Now) {
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
    fn draw_field(&self, cx: &Cx, p: &PromptPanel, focus: bool) {
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
            cx.text(c, txt, Style::new(11.5, 600).center(), if *on { ON_WHITE } else { TEXT });
        }
    }

    /// A segmented control of the settings panel: the white pill sits at `pos` (in segments, it slides), the labels darken as it passes
    /// under them, `hov(i)` is the hover amount of segment i.
    fn seg_anim(&self, cx: &Cx, r: Rect, labels: &[&str], pad: f32, pos: f32, hov: impl Fn(usize) -> f32) {
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
    fn set_slider(&self, cx: &Cx, r: &SliderRow, i: usize) {
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
    fn set_switch(&self, cx: &Cx, row: Rect, sw: Rect, label: &str, key: usize, pos: f32) {
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
    fn set_button(&self, cx: &Cx, r: Rect, label: &str, key: usize, enabled: bool, color: Color) {
        let h = if enabled { self.sa.hov.get(key) } else { 0.0 };
        let p = if enabled { self.sa.press(key) } else { 0.0 };
        let x = if enabled { self.sa.prs.get(key).map_or(1.0, |b| b.x) } else { 1.0 };
        cx.scaled((r.cx(), r.cy()), x, |cx| {
            cx.pill(r, Color::rgba(1.0, 1.0, 1.0, 0.18 + 0.09 * h + 0.07 * p));
            cx.text(r, label, Style::new(12.5, 600).center().ellipsis(), color);
        });
    }

    /// The update status line: the old text fades out while the new one fades in when the kind of status changes.
    fn draw_upd_status(&self, cx: &Cx, r: Rect) {
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

    fn draw_settings(&self, scene: &Scene, gfx: &Gfx, now: &Now) {
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
                cx.text(o.label, self.t(ru, en), Style::new(12.5, 500).ellipsis(), TEXT);
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
        ui.open_menu(&scene, &gfx, menu::card_menu(env.lang, false, &sizes, cur), anchor, Rect::new(0.0, 0.0, lw, lh));
        if let Some(m) = ui.menu.as_mut() {
            (m.t, m.sub_t) = (1.0, 1.0);
            if a.state == "menu-size" {
                m.open_sub(1, false);
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
