//! The on-canvas context menu, model half: items (label, check mark, disabled, destructive, optional submenu, separators), the layout of
//! the glass panels (root + one submenu), placement inside the work area, keyboard / mouse semantics and the open / close animation.
//! Nothing here touches Direct2D or Win32: text widths come in as a closure (`TextW`), so all of it is unit-tested with a monospace
//! stand-in. `ui.rs` draws a `MenuPanel`, `host.rs` feeds it keys / mouse and shows it on the host canvas (see the prompt for the twin).
//! The model is generic on purpose: the tray menu could be described with the same `Entry` list later.
use crate::prompt::Key;
use crate::widget::{Lang, Rect, Size};

/// Command ids of the card menu (the native fallback in host.rs uses the same ones).
pub const CMD_EDIT: usize = 1;
pub const CMD_SIZE: usize = 10; // + 0 / 1 / 2 for S / M / L
pub const CMD_DELETE: usize = 20;

/// Text width in logical px of a label in the menu font.
pub type TextW<'a> = &'a dyn Fn(&str) -> f32;

#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    pub id: usize,
    pub label: String,
    pub checked: bool,
    pub enabled: bool,
    pub destructive: bool,
    /// a submenu (one level); the item itself activates nothing then
    pub children: Vec<Entry>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Entry {
    Item(Item),
    Sep,
}

impl Item {
    pub fn new(id: usize, label: &str) -> Item {
        Item { id, label: label.into(), checked: false, enabled: true, destructive: false, children: Vec::new() }
    }
    pub fn checked(mut self, on: bool) -> Item {
        self.checked = on;
        self
    }
    pub fn enabled(mut self, on: bool) -> Item {
        self.enabled = on;
        self
    }
    pub fn destructive(mut self) -> Item {
        self.destructive = true;
        self
    }
    pub fn sub(mut self, children: Vec<Entry>) -> Item {
        self.children = children;
        self
    }
    pub fn has_sub(&self) -> bool {
        !self.children.is_empty()
    }
}

impl Entry {
    /// Can the highlight rest on it?
    fn live(&self) -> bool {
        matches!(self, Entry::Item(i) if i.enabled)
    }
    fn item(&self) -> Option<&Item> {
        match self {
            Entry::Item(i) => Some(i),
            Entry::Sep => None,
        }
    }
}

/// The menu of a widget card: "Edit widgets…" (or "Done" while editing), "Size" > S / M / L, a separator, "Remove widget". The sizes the
/// widget does not offer are greyed; a widget with a single size has the whole "Size" item greyed.
pub fn card_menu(lang: Lang, editing: bool, sizes: &[Size], cur: Size) -> Vec<Entry> {
    card_menu_with(lang, editing, sizes, cur, None)
}

/// Command ids of the "Click action" submenu: Default / Nothing / Custom… and the presets of the widget (`CMD_CLICK_PRESET + k`).
pub const CMD_CLICK: usize = 100;
pub const CMD_CLICK_NONE: usize = 101;
pub const CMD_CLICK_CUSTOM: usize = 102;
pub const CMD_CLICK_PRESET: usize = 110;
const CLICK_PRESETS_MAX: usize = 20;

/// What a "Click action" command means.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ClickPick {
    Default,
    Nothing,
    Custom,
    /// index into `targets::presets(widget)`
    Preset(usize),
}

pub fn click_pick(cmd: usize) -> Option<ClickPick> {
    match cmd {
        CMD_CLICK => Some(ClickPick::Default),
        CMD_CLICK_NONE => Some(ClickPick::Nothing),
        CMD_CLICK_CUSTOM => Some(ClickPick::Custom),
        c if (CMD_CLICK_PRESET..CMD_CLICK_PRESET + CLICK_PRESETS_MAX).contains(&c) => Some(ClickPick::Preset(c - CMD_CLICK_PRESET)),
        _ => None,
    }
}

/// The "Click action" item of a widget's menu (a submenu with the current choice checked); None for a widget whose card body has no click.
pub fn click_item(lang: Lang, widget: &str, cur: &crate::sys::targets::Click) -> Option<Item> {
    use crate::sys::targets::{presets, Click};
    let ps = presets(widget);
    if ps.is_empty() {
        return None;
    }
    let mut kids = vec![Entry::Item(Item::new(CMD_CLICK, lang.pick("По умолчанию", "Default")).checked(*cur == Click::Default))];
    kids.push(Entry::Item(Item::new(CMD_CLICK_NONE, lang.pick("Ничего", "Nothing")).checked(*cur == Click::Nothing)));
    kids.push(Entry::Sep);
    for (k, p) in ps.iter().take(CLICK_PRESETS_MAX).enumerate() {
        kids.push(Entry::Item(Item::new(CMD_CLICK_PRESET + k, lang.pick(p.ru, p.en)).checked(matches!(cur, Click::Preset(key) if key == p.key))));
    }
    kids.push(Entry::Sep);
    kids.push(Entry::Item(Item::new(CMD_CLICK_CUSTOM, lang.pick("Свой вариант…", "Custom…")).checked(matches!(cur, Click::Custom(_)))));
    Some(Item::new(0, lang.pick("Действие по клику", "Click action")).sub(kids))
}

/// `card_menu` plus the "Click action" submenu (between "Size" and the separator).
pub fn card_menu_with(lang: Lang, editing: bool, sizes: &[Size], cur: Size, click: Option<Item>) -> Vec<Entry> {
    let edit = lang.pick(if editing { "Готово" } else { "Изменить виджеты…" }, if editing { "Done" } else { "Edit Widgets…" });
    let names = [(Size::S, lang.pick("Маленький (S)", "Small (S)")), (Size::M, lang.pick("Средний (M)", "Medium (M)")), (Size::L, lang.pick("Большой (L)", "Large (L)"))];
    let kids = names.iter().enumerate().map(|(i, (sz, name))| Entry::Item(Item::new(CMD_SIZE + i, name).checked(*sz == cur).enabled(sizes.contains(sz)))).collect();
    let mut v = vec![Entry::Item(Item::new(CMD_EDIT, edit)), Entry::Item(Item::new(0, lang.pick("Размер", "Size")).enabled(sizes.len() > 1).sub(kids))];
    v.extend(click.map(Entry::Item));
    v.push(Entry::Sep);
    v.push(Entry::Item(Item::new(CMD_DELETE, lang.pick("Удалить виджет", "Remove Widget")).destructive()));
    v
}

// ---------------------------------------------------------------- layout

/// Metrics, logical px.
pub const PAD: f32 = 6.0;
pub const ROW_H: f32 = 32.0;
const SEP_H: f32 = 9.0;
pub const ROW_PAD_X: f32 = 12.0;
/// the check mark column (only when some item of the level is checked) and the submenu chevron column
pub const CHECK_W: f32 = 22.0;
const ARROW_W: f32 = 22.0;
const MIN_W: f32 = 176.0;
/// radius of the glass panel; the hover pill inside is `PAD` smaller (concentric)
pub const RADIUS: f32 = 18.0;
pub const PILL_R: f32 = RADIUS - PAD;
/// gap between a panel and its submenu, distance kept from the work area edge
const GAP: f32 = 2.0;
const EDGE: f32 = 8.0;
const OPEN_S: f32 = 0.14;
const CLOSE_S: f32 = 0.10;
const SUB_S: f32 = 0.12;
/// hover on a row with a submenu opens it after this long
const HOVER_S: f32 = 0.22;
/// the veil over the desktop while the menu is up: only there so the layer catches clicks outside the panels (barely visible)
pub const SCRIM: f32 = 0.03;

/// One panel: its rectangle (canvas logical px), one rectangle per entry and the corner the open animation grows from.
#[derive(Clone, Debug)]
pub struct Level {
    pub panel: Rect,
    pub rows: Vec<Rect>,
    pub origin: (f32, f32),
    pub check: bool,
    pub arrow: bool,
}

/// (width, height, check column, chevron column) of a panel for `entries`.
fn size_of(entries: &[Entry], w: TextW) -> (f32, f32, bool, bool) {
    let check = entries.iter().any(|e| e.item().is_some_and(|i| i.checked));
    let arrow = entries.iter().any(|e| e.item().is_some_and(|i| i.has_sub()));
    let label = entries.iter().filter_map(|e| e.item()).map(|i| w(&i.label)).fold(0.0f32, f32::max);
    let width = (2.0 * PAD + 2.0 * ROW_PAD_X + label.ceil() + if check { CHECK_W } else { 0.0 } + if arrow { ARROW_W } else { 0.0 }).max(MIN_W);
    let h = 2.0 * PAD + entries.iter().map(|e| if matches!(e, Entry::Sep) { SEP_H } else { ROW_H }).sum::<f32>();
    (width.ceil(), h, check, arrow)
}

fn make_level(entries: &[Entry], (w, h, check, arrow): (f32, f32, bool, bool), pos: (f32, f32), origin: (f32, f32)) -> Level {
    let panel = Rect::new(pos.0, pos.1, w, h);
    let mut y = panel.y + PAD;
    let rows = entries
        .iter()
        .map(|e| {
            let rh = if matches!(e, Entry::Sep) { SEP_H } else { ROW_H };
            let r = Rect::new(panel.x + PAD, y, w - 2.0 * PAD, rh);
            y += rh;
            r
        })
        .collect();
    Level { panel, rows, origin, check, arrow }
}

fn clamp_in(v: f32, len: f32, lo: f32, hi: f32) -> f32 {
    v.min(hi - EDGE - len).max(lo + EDGE)
}

/// Top-left of a `size` panel opened at `anchor` (the cursor): its corner sits on the anchor; no room to the right / below flips it to the
/// left / above, then it is kept `EDGE` inside `work`. Also returns the corner the animation grows from.
pub fn place(anchor: (f32, f32), size: (f32, f32), work: Rect) -> ((f32, f32), (f32, f32)) {
    let (w, h) = size;
    let x = if anchor.0 + w > work.right() - EDGE { anchor.0 - w } else { anchor.0 };
    let y = if anchor.1 + h > work.bottom() - EDGE { anchor.1 - h } else { anchor.1 };
    let (x, y) = (clamp_in(x, w, work.x, work.right()), clamp_in(y, h, work.y, work.bottom()));
    ((x, y), (anchor.0.clamp(x, x + w), anchor.1.clamp(y, y + h)))
}

/// A submenu next to `parent` (the panel) at `row`: to its right with its first row level with `row`, to the left when there is no room.
fn place_sub(parent: Rect, row: Rect, size: (f32, f32), work: Rect) -> ((f32, f32), (f32, f32)) {
    let (w, h) = size;
    let right = parent.right() + GAP + w <= work.right() - EDGE;
    let x = if right { parent.right() + GAP } else { parent.x - GAP - w };
    let y = if row.y - PAD + h > work.bottom() - EDGE { row.bottom() + PAD - h } else { row.y - PAD };
    let (x, y) = (clamp_in(x, w, work.x, work.right()), clamp_in(y, h, work.y, work.bottom()));
    ((x, y), (if right { x } else { x + w }, row.cy().clamp(y, y + h)))
}

// ---------------------------------------------------------------- panel state

/// What the host has to do after an event.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum MAct {
    None,
    Redraw,
    /// the submenu opened / closed: panels to sync, then redraw
    Layout,
    /// the menu is over without a choice
    Close,
    /// the menu is over: the id of the chosen item
    Pick(usize),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum MHit {
    /// a live item: (level 0 root / 1 submenu, entry index)
    Row(usize, usize),
    /// inside a panel, nothing to click (padding, separator, greyed item)
    Panel(usize),
    Outside,
}

pub struct MenuPanel {
    pub entries: Vec<Entry>,
    work: Rect,
    pub root: Level,
    /// size of the submenu of every root entry (None = no submenu)
    sub_dims: Vec<Option<(f32, f32, bool, bool)>>,
    /// the open submenu: the root entry it belongs to, its layout
    pub sub: Option<(usize, Level)>,
    /// highlighted entry of the root / of the submenu (the root keeps the parent of an open submenu)
    pub hot: [Option<usize>; 2],
    /// the keyboard works in the submenu
    pub in_sub: bool,
    /// a pressed row (level, entry): acts on release while still over it
    pub press: Option<(usize, usize)>,
    /// hovering a row with a submenu: (entry, seconds so far)
    pending: Option<(usize, f32)>,
    /// open progress 0..1 (linear; `eased` for drawing); runs back to 0 while `closing`
    pub t: f32,
    pub sub_t: f32,
    pub closing: bool,
}

fn ease(t: f32) -> f32 {
    let q = 1.0 - t.clamp(0.0, 1.0);
    1.0 - q * q * q
}

impl MenuPanel {
    /// The menu `entries` opened at `anchor` (canvas logical px) inside `work`.
    pub fn new(entries: Vec<Entry>, anchor: (f32, f32), work: Rect, w: TextW) -> MenuPanel {
        let sz = size_of(&entries, w);
        let (pos, origin) = place(anchor, (sz.0, sz.1), work);
        let root = make_level(&entries, sz, pos, origin);
        let sub_dims = entries.iter().map(|e| e.item().filter(|i| i.has_sub()).map(|i| size_of(&i.children, w))).collect();
        MenuPanel { entries, work, root, sub_dims, sub: None, hot: [None, None], in_sub: false, press: None, pending: None, t: 0.0, sub_t: 0.0, closing: false }
    }

    pub fn eased(&self) -> f32 {
        ease(self.t)
    }
    /// Opacity of the submenu (it also fades with the root).
    pub fn sub_eased(&self) -> f32 {
        ease(self.sub_t) * self.eased()
    }
    pub fn scrim(&self) -> f32 {
        SCRIM
    }

    /// Advance the animations / the hover delay by `dt` seconds. Returns (still moving, the submenu opened by the hover delay).
    pub fn step(&mut self, dt: f32, reduced: bool) -> (bool, bool) {
        let mut busy = false;
        if reduced {
            self.t = if self.closing { 0.0 } else { 1.0 };
            self.sub_t = 1.0;
        } else {
            if self.closing {
                self.t = (self.t - dt / CLOSE_S).max(0.0);
                busy = self.t > 0.0;
            } else {
                self.t = (self.t + dt / OPEN_S).min(1.0);
                busy = self.t < 1.0;
            }
            if self.sub.is_some() {
                self.sub_t = (self.sub_t + dt / SUB_S).min(1.0);
                busy |= self.sub_t < 1.0;
            }
        }
        let mut layout = false;
        if let Some((i, acc)) = self.pending {
            if acc + dt >= HOVER_S || reduced {
                self.pending = None;
                layout = self.open_sub(i, false);
            } else {
                self.pending = Some((i, acc + dt));
                busy = true;
            }
        }
        (busy, layout)
    }

    // ------------------------------------------------ structure

    fn entries_of(&self, lvl: usize) -> &[Entry] {
        match (lvl, &self.sub) {
            (1, Some((i, _))) => self.entries[*i].item().map_or(&[][..], |it| &it.children),
            (1, None) => &[],
            _ => &self.entries,
        }
    }
    fn level_of(&self, lvl: usize) -> Option<&Level> {
        if lvl == 0 {
            Some(&self.root)
        } else {
            self.sub.as_ref().map(|(_, l)| l)
        }
    }
    /// The item behind a (level, entry) pair.
    pub fn item_at(&self, lvl: usize, i: usize) -> Option<&Item> {
        self.entries_of(lvl).get(i).and_then(|e| e.item())
    }
    /// The rectangle all panels of the menu occupy (logical px).
    pub fn bounds(&self) -> Rect {
        let r = self.root.panel;
        match &self.sub {
            Some((_, l)) => {
                let (x0, y0, x1, y1) = (r.x.min(l.panel.x), r.y.min(l.panel.y), r.right().max(l.panel.right()), r.bottom().max(l.panel.bottom()));
                Rect::new(x0, y0, x1 - x0, y1 - y0)
            }
            None => r,
        }
    }

    /// Open the submenu of root entry `i` (a live item with children); `focus`: the keyboard goes in and selects its first live row.
    /// True when the layout changed.
    pub fn open_sub(&mut self, i: usize, focus: bool) -> bool {
        let Some(dims) = self.sub_dims.get(i).copied().flatten() else { return false };
        if !self.entries[i].live() {
            return false;
        }
        let mut changed = false;
        if self.sub.as_ref().map(|s| s.0) != Some(i) {
            let (pos, origin) = place_sub(self.root.panel, self.root.rows[i], (dims.0, dims.1), self.work);
            let it = self.entries[i].item().expect("item");
            self.sub = Some((i, make_level(&it.children, dims, pos, origin)));
            self.sub_t = 0.0;
            self.hot[1] = None;
            changed = true;
        }
        self.hot[0] = Some(i);
        self.pending = None;
        if focus {
            self.in_sub = true;
            self.hot[1] = self.entries_of(1).iter().position(|e| e.live());
        }
        changed || focus
    }
    /// Close the submenu; true when one was open.
    pub fn close_sub(&mut self) -> bool {
        let was = self.sub.take().is_some();
        self.hot[1] = None;
        self.in_sub = false;
        self.pending = None;
        self.sub_t = 0.0;
        was
    }

    // ------------------------------------------------ keyboard

    /// Move the highlight of the active level: `dir` +1 / -1 steps (wrapping, skipping separators and greyed items), 0 = first, 2 = last.
    fn walk(&mut self, dir: i32) {
        let lvl = usize::from(self.in_sub);
        let live: Vec<usize> = self.entries_of(lvl).iter().enumerate().filter(|(_, e)| e.live()).map(|(i, _)| i).collect();
        if live.is_empty() {
            return;
        }
        let at = self.hot[lvl].and_then(|h| live.iter().position(|&i| i == h));
        let k = match (dir, at) {
            (0, _) => 0,
            (2, _) => live.len() - 1,
            (1, None) => 0,
            (_, None) => live.len() - 1,
            (1, Some(k)) => (k + 1) % live.len(),
            (_, Some(k)) => (k + live.len() - 1) % live.len(),
        };
        self.hot[lvl] = Some(live[k]);
        if lvl == 0 && self.sub.as_ref().is_some_and(|s| s.0 != live[k]) {
            self.close_sub();
        }
    }

    /// Up / Down / Home / End walk, Right / Enter open a submenu or activate, Left closes the submenu, Esc closes the menu.
    pub fn key(&mut self, k: Key) -> MAct {
        let sub_before = self.sub.as_ref().map(|s| s.0);
        match k {
            Key::Esc => return MAct::Close,
            Key::Down | Key::Up | Key::Home | Key::End => {
                self.walk(match k {
                    Key::Down => 1,
                    Key::Up => -1,
                    Key::Home => 0,
                    _ => 2,
                });
                self.press = None;
                if sub_before != self.sub.as_ref().map(|s| s.0) { MAct::Layout } else { MAct::Redraw }
            }
            Key::Right if !self.in_sub => match self.hot[0].map(|i| self.open_sub(i, true)) {
                Some(true) => MAct::Layout,
                _ => MAct::None,
            },
            Key::Left if self.in_sub => {
                self.close_sub();
                MAct::Layout
            }
            Key::Enter | Key::Char(' ') => {
                let lvl = usize::from(self.in_sub);
                let Some(i) = self.hot[lvl] else { return MAct::None };
                let Some(it) = self.item_at(lvl, i).filter(|it| it.enabled) else { return MAct::None };
                if it.has_sub() {
                    if self.open_sub(i, true) { MAct::Layout } else { MAct::None }
                } else {
                    MAct::Pick(it.id)
                }
            }
            _ => MAct::None,
        }
    }

    // ------------------------------------------------ mouse (logical px)

    fn row_at(&self, lvl: usize, x: f32, y: f32) -> Option<usize> {
        self.level_of(lvl)?.rows.iter().position(|r| r.contains(x, y))
    }
    pub fn hit(&self, x: f32, y: f32) -> MHit {
        for lvl in [1, 0] {
            let Some(l) = self.level_of(lvl) else { continue };
            if !l.panel.contains(x, y) {
                continue;
            }
            return match self.row_at(lvl, x, y) {
                Some(i) if self.entries_of(lvl)[i].live() => MHit::Row(lvl, i),
                _ => MHit::Panel(lvl),
            };
        }
        MHit::Outside
    }

    /// The pointer moved: hover highlight; a row with a submenu opens it after a short delay; moving to another root row closes it.
    pub fn mouse_move(&mut self, x: f32, y: f32) -> MAct {
        let before = (self.hot, self.in_sub, self.sub.as_ref().map(|s| s.0), self.pending.map(|p| p.0));
        match self.hit(x, y) {
            MHit::Row(0, i) => {
                self.in_sub = false;
                if self.sub.as_ref().is_some_and(|s| s.0 != i) {
                    self.close_sub();
                }
                self.hot[0] = Some(i);
                let sub_open = self.sub.as_ref().is_some_and(|s| s.0 == i);
                self.pending = match self.item_at(0, i) {
                    Some(it) if it.has_sub() && !sub_open => Some((i, self.pending.filter(|p| p.0 == i).map_or(0.0, |p| p.1))),
                    _ => None,
                };
            }
            MHit::Row(_, j) => {
                self.in_sub = true;
                self.hot[1] = Some(j);
                self.pending = None;
            }
            MHit::Panel(0) => {
                self.hot[0] = self.sub.as_ref().map(|s| s.0);
                self.pending = None;
            }
            MHit::Panel(_) => self.hot[1] = None,
            MHit::Outside => {}
        }
        let after = (self.hot, self.in_sub, self.sub.as_ref().map(|s| s.0), self.pending.map(|p| p.0));
        if before.2 != after.2 {
            MAct::Layout
        } else if before != after {
            MAct::Redraw
        } else {
            MAct::None
        }
    }

    /// Button down: outside the panels closes the menu; a row with a submenu opens it at once; any other live row is pressed (it acts on release).
    pub fn mouse_down(&mut self, x: f32, y: f32) -> MAct {
        match self.hit(x, y) {
            MHit::Outside => MAct::Close,
            MHit::Panel(_) => MAct::None,
            MHit::Row(lvl, i) => {
                if lvl == 0 && self.item_at(0, i).is_some_and(|it| it.has_sub()) {
                    self.press = None;
                    self.in_sub = false;
                    return if self.open_sub(i, false) { MAct::Layout } else { MAct::Redraw };
                }
                self.hot[lvl] = Some(i);
                self.in_sub = lvl == 1;
                self.press = Some((lvl, i));
                MAct::Redraw
            }
        }
    }

    pub fn mouse_up(&mut self, x: f32, y: f32) -> MAct {
        match (self.press.take(), self.hit(x, y)) {
            (Some((l, i)), MHit::Row(l2, i2)) if (l, i) == (l2, i2) => self.item_at(l, i).map_or(MAct::None, |it| MAct::Pick(it.id)),
            (Some(_), _) => MAct::Redraw,
            _ => MAct::None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 8 px per char
    fn mono(s: &str) -> f32 {
        s.chars().count() as f32 * 8.0
    }
    const WORK: Rect = Rect::new(0.0, 0.0, 1920.0, 1040.0);

    fn menu_at(anchor: (f32, f32)) -> MenuPanel {
        MenuPanel::new(card_menu(Lang::En, false, &Size::ALL, Size::M), anchor, WORK, &mono)
    }
    fn inside(r: Rect, work: Rect) -> bool {
        r.x >= work.x + EDGE - 0.01 && r.y >= work.y + EDGE - 0.01 && r.right() <= work.right() - EDGE + 0.01 && r.bottom() <= work.bottom() - EDGE + 0.01
    }

    #[test]
    fn card_menu_items_checks_and_greying() {
        let m = card_menu(Lang::En, false, &Size::ALL, Size::M);
        let ids: Vec<Option<usize>> = m.iter().map(|e| e.item().map(|i| i.id)).collect();
        assert_eq!(ids, vec![Some(CMD_EDIT), Some(0), None, Some(CMD_DELETE)]);
        let size = m[1].item().unwrap();
        assert!(size.enabled && size.has_sub() && m[3].item().unwrap().destructive && !m[0].item().unwrap().destructive);
        let kids: Vec<(usize, bool, bool)> = size.children.iter().map(|e| e.item().unwrap()).map(|i| (i.id, i.checked, i.enabled)).collect();
        assert_eq!(kids, vec![(CMD_SIZE, false, true), (CMD_SIZE + 1, true, true), (CMD_SIZE + 2, false, true)]);
        // a widget with S and M only: L is greyed; with one size the whole "Size" item is greyed
        let two = card_menu(Lang::Ru, false, &[Size::S, Size::M], Size::S);
        assert!(!two[1].item().unwrap().children[2].item().unwrap().enabled && two[1].item().unwrap().children[0].item().unwrap().checked);
        assert_eq!(two[1].item().unwrap().label, "Размер");
        let one = card_menu(Lang::En, false, &[Size::M], Size::M);
        assert!(!one[1].item().unwrap().enabled);
        // edit mode: the first item reads "Done"
        assert_eq!(card_menu(Lang::En, true, &Size::ALL, Size::M)[0].item().unwrap().label, "Done");
        assert_eq!(card_menu(Lang::Ru, false, &Size::ALL, Size::M)[0].item().unwrap().label, "Изменить виджеты…");
    }

    #[test]
    fn layout_rows_stay_inside_the_panel() {
        let mut m = menu_at((300.0, 200.0));
        let p = m.root.panel;
        assert_eq!((p.x, p.y), (300.0, 200.0)); // the corner sits on the cursor
        assert!(p.w >= MIN_W && p.h == 2.0 * PAD + 3.0 * ROW_H + SEP_H);
        let mut y = p.y + PAD;
        for (r, e) in m.root.rows.iter().zip(&m.entries) {
            assert_eq!(r.y, y);
            assert!(r.x >= p.x + PAD && r.right() <= p.right() - PAD + 0.01);
            y = r.bottom();
            assert_eq!(r.h, if matches!(e, Entry::Sep) { SEP_H } else { ROW_H });
        }
        assert_eq!(y + PAD, p.bottom());
        // the root has no check column (nothing checked), the submenu has one and no chevron
        assert!(!m.root.check && m.root.arrow);
        assert!(m.open_sub(1, false));
        let sub = &m.sub.as_ref().unwrap().1;
        assert!(sub.check && !sub.arrow && sub.rows.len() == 3);
        // a long label widens the panel
        let wide = MenuPanel::new(vec![Entry::Item(Item::new(1, &"x".repeat(40)))], (0.0, 0.0), WORK, &mono);
        assert!(wide.root.panel.w > 40.0 * 8.0);
    }

    #[test]
    fn placement_flips_and_stays_in_the_work_area() {
        let (w, h) = (menu_at((0.0, 0.0)).root.panel.w, menu_at((0.0, 0.0)).root.panel.h);
        for anchor in [(300.0, 200.0), (1915.0, 1035.0), (3.0, 3.0), (1915.0, 10.0), (10.0, 1035.0), (-50.0, 500.0), (960.0, 1030.0)] {
            let m = menu_at(anchor);
            assert!(inside(m.root.panel, WORK), "{anchor:?}: {:?}", m.root.panel);
            // the open animation grows from a point on the panel
            assert!(m.root.panel.contains(m.root.origin.0, m.root.origin.1) || m.root.origin.0 == m.root.panel.right() || m.root.origin.1 == m.root.panel.bottom());
        }
        // no room to the right / below: the menu opens to the left of / above the cursor
        let m = menu_at((1900.0, 1000.0));
        assert_eq!((m.root.panel.right(), m.root.panel.bottom()), (1900.0, 1000.0));
        assert_eq!(m.root.origin, (1900.0, 1000.0));
        let m = menu_at((1900.0, 100.0));
        assert_eq!((m.root.panel.right(), m.root.panel.y), (1900.0, 100.0));
        let _ = (w, h);
        // a work area that starts away from 0 (a second monitor, a taskbar on top)
        let work = Rect::new(1920.0, 40.0, 1280.0, 960.0);
        let m = MenuPanel::new(card_menu(Lang::En, false, &Size::ALL, Size::M), (1925.0, 45.0), work, &mono);
        assert!(inside(m.root.panel, work));
    }

    #[test]
    fn submenu_opens_beside_its_row_and_flips() {
        let mut m = menu_at((300.0, 200.0));
        assert!(m.open_sub(1, false));
        let (idx, sub) = m.sub.clone().unwrap();
        assert_eq!(idx, 1);
        let (root, row) = (m.root.panel, m.root.rows[1]);
        assert_eq!(sub.panel.x, root.right() + GAP);
        assert_eq!(sub.panel.y, row.y - PAD); // its first row is level with the parent row
        assert_eq!(sub.rows[0].y, row.y);
        assert!(m.hot[0] == Some(1) && m.hot[1].is_none() && !m.in_sub);
        assert!(!m.open_sub(1, false), "already open: no layout change");
        // no room on the right: to the left of the parent
        let mut m = menu_at((1700.0, 200.0));
        m.open_sub(1, false);
        let (root, sub) = (m.root.panel, m.sub.as_ref().unwrap().1.panel);
        assert!(root.right() + GAP + sub.w > 1912.0, "test setup: no room on the right");
        assert_eq!(sub.right(), root.x - GAP);
        assert!(inside(sub, WORK));
        // near the bottom the submenu is lifted so it stays inside
        let mut m = menu_at((300.0, 915.0));
        m.open_sub(1, false);
        let sub = m.sub.as_ref().unwrap().1.panel;
        assert!(inside(sub, WORK) && inside(m.root.panel, WORK));
        assert!(sub.y < m.root.rows[1].y - PAD, "lifted above the parent row");
        // bounds cover both
        let b = m.bounds();
        assert!(b.contains(m.root.panel.x, m.root.panel.y) && b.contains(m.sub.as_ref().unwrap().1.panel.right() - 1.0, m.sub.as_ref().unwrap().1.panel.bottom() - 1.0));
        // a greyed parent never opens
        let mut one = MenuPanel::new(card_menu(Lang::En, false, &[Size::M], Size::M), (300.0, 200.0), WORK, &mono);
        assert!(!one.open_sub(1, false) && one.sub.is_none());
        assert_eq!(one.key(Key::Right), MAct::None);
    }

    #[test]
    fn keyboard_walks_skips_and_wraps() {
        let mut m = menu_at((300.0, 200.0));
        assert_eq!(m.hot, [None, None]);
        assert_eq!(m.key(Key::Down), MAct::Redraw);
        assert_eq!(m.hot[0], Some(0));
        m.key(Key::Down);
        assert_eq!(m.hot[0], Some(1));
        m.key(Key::Down); // the separator is skipped
        assert_eq!(m.hot[0], Some(3));
        m.key(Key::Down); // wraps
        assert_eq!(m.hot[0], Some(0));
        m.key(Key::Up);
        assert_eq!(m.hot[0], Some(3));
        m.key(Key::Home);
        assert_eq!(m.hot[0], Some(0));
        m.key(Key::End);
        assert_eq!(m.hot[0], Some(3));
        // Up with nothing highlighted starts from the end
        let mut m = menu_at((300.0, 200.0));
        m.key(Key::Up);
        assert_eq!(m.hot[0], Some(3));
        // greyed items are skipped: only "Size" greyed
        let mut g = MenuPanel::new(card_menu(Lang::En, false, &[Size::S], Size::S), (300.0, 200.0), WORK, &mono);
        g.key(Key::Down);
        g.key(Key::Down);
        assert_eq!(g.hot[0], Some(3));
        // nothing highlighted: Enter / Right / Left do nothing, Esc closes
        let mut m = menu_at((300.0, 200.0));
        assert_eq!((m.key(Key::Enter), m.key(Key::Right), m.key(Key::Left)), (MAct::None, MAct::None, MAct::None));
        assert_eq!(m.key(Key::Esc), MAct::Close);
    }

    #[test]
    fn keyboard_submenu_activation_and_ids() {
        let mut m = menu_at((300.0, 200.0));
        m.key(Key::Down);
        assert_eq!(m.key(Key::Enter), MAct::Pick(CMD_EDIT));
        m.key(Key::Down); // Size
        assert_eq!(m.key(Key::Right), MAct::Layout);
        assert!(m.in_sub && m.sub.is_some() && m.hot[1] == Some(0) && m.hot[0] == Some(1));
        // walking inside the submenu, wrapping there
        m.key(Key::Down);
        m.key(Key::Down);
        assert_eq!(m.hot[1], Some(2));
        m.key(Key::Down);
        assert_eq!(m.hot[1], Some(0));
        m.key(Key::Up);
        assert_eq!(m.key(Key::Enter), MAct::Pick(CMD_SIZE + 2));
        // Left closes the submenu and returns to the root row, Right opens it again, Enter on the parent opens it too
        assert_eq!(m.key(Key::Left), MAct::Layout);
        assert!(!m.in_sub && m.sub.is_none() && m.hot[0] == Some(1));
        assert_eq!(m.key(Key::Left), MAct::None);
        assert_eq!(m.key(Key::Enter), MAct::Layout);
        assert!(m.in_sub);
        // Esc closes the whole menu from the submenu
        assert_eq!(m.key(Key::Esc), MAct::Close);
        // Down in the root with a submenu open on another row closes it
        let mut m = menu_at((300.0, 200.0));
        m.open_sub(1, false);
        m.hot[0] = Some(1);
        assert_eq!(m.key(Key::Down), MAct::Layout);
        assert!(m.sub.is_none() && m.hot[0] == Some(3));
        assert_eq!(m.key(Key::Char(' ')), MAct::Pick(CMD_DELETE));
        // a greyed size does not activate
        let mut two = MenuPanel::new(card_menu(Lang::En, false, &[Size::S, Size::M], Size::S), (300.0, 200.0), WORK, &mono);
        two.open_sub(1, true);
        two.key(Key::End);
        assert_eq!(two.hot[1], Some(1), "End lands on the last live size");
    }

    #[test]
    fn mouse_hover_click_and_outside() {
        let mut m = menu_at((300.0, 200.0));
        let rows = m.root.rows.clone();
        let c = |r: Rect| (r.cx(), r.cy());
        // hover highlights, the same spot again changes nothing
        assert_eq!(m.mouse_move(c(rows[0]).0, c(rows[0]).1), MAct::Redraw);
        assert_eq!(m.hot[0], Some(0));
        assert_eq!(m.mouse_move(c(rows[0]).0 + 1.0, c(rows[0]).1), MAct::None);
        // the separator and the padding highlight nothing
        m.mouse_move(c(rows[2]).0, c(rows[2]).1);
        assert_eq!(m.hot[0], None);
        assert_eq!(m.hit(c(rows[2]).0, c(rows[2]).1), MHit::Panel(0));
        assert_eq!(m.hit(m.root.panel.x + 2.0, rows[0].cy()), MHit::Panel(0));
        assert_eq!(m.hit(5.0, 5.0), MHit::Outside);
        // press + release on Delete picks it; released elsewhere does not
        assert_eq!(m.mouse_down(c(rows[3]).0, c(rows[3]).1), MAct::Redraw);
        assert_eq!(m.mouse_up(c(rows[3]).0, c(rows[3]).1), MAct::Pick(CMD_DELETE));
        m.mouse_down(c(rows[3]).0, c(rows[3]).1);
        assert_eq!(m.mouse_up(c(rows[0]).0, c(rows[0]).1), MAct::Redraw);
        assert_eq!(m.mouse_up(c(rows[0]).0, c(rows[0]).1), MAct::None);
        // outside closes
        assert_eq!(m.mouse_down(5.0, 5.0), MAct::Close);
        // a click on the padding / separator does nothing
        assert_eq!(m.mouse_down(c(rows[2]).0, c(rows[2]).1), MAct::None);
    }

    #[test]
    fn mouse_submenu_delay_click_and_switching() {
        let mut m = menu_at((300.0, 200.0));
        let size_row = m.root.rows[1];
        // hovering "Size" waits, then opens by itself; the animation timer must keep running meanwhile
        assert_eq!(m.mouse_move(size_row.cx(), size_row.cy()), MAct::Redraw);
        assert!(m.sub.is_none());
        assert_eq!(m.step(0.1, false).1, false);
        assert!(m.step(0.0, false).0, "pending hover keeps the timer busy");
        let (_, opened) = m.step(0.15, false);
        assert!(opened && m.sub.is_some() && !m.in_sub);
        // moving into the submenu through the gap keeps it, a row of the submenu highlights and takes the keyboard
        let sub = m.sub.as_ref().unwrap().1.clone();
        assert_eq!(m.hit(sub.panel.x - 1.0, size_row.cy()), MHit::Outside);
        assert_eq!(m.mouse_move(sub.panel.x - 1.0, size_row.cy()), MAct::None);
        m.mouse_move(sub.rows[1].cx(), sub.rows[1].cy());
        assert!(m.in_sub && m.hot[1] == Some(1) && m.hot[0] == Some(1));
        // click on "Large" picks it
        m.mouse_down(sub.rows[2].cx(), sub.rows[2].cy());
        assert_eq!(m.mouse_up(sub.rows[2].cx(), sub.rows[2].cy()), MAct::Pick(CMD_SIZE + 2));
        // hovering another root row closes the submenu
        let del = m.root.rows[3];
        assert_eq!(m.mouse_move(del.cx(), del.cy()), MAct::Layout);
        assert!(m.sub.is_none() && m.hot == [Some(3), None]);
        // a click on "Size" opens at once (no delay), a second click leaves it open
        let mut m = menu_at((300.0, 200.0));
        assert_eq!(m.mouse_down(size_row.cx(), size_row.cy()), MAct::Layout);
        assert!(m.sub.is_some());
        assert_eq!(m.mouse_up(size_row.cx(), size_row.cy()), MAct::None);
        assert_eq!(m.mouse_down(size_row.cx(), size_row.cy()), MAct::Redraw);
        assert!(m.sub.is_some());
        // leaving the "Size" row before the delay cancels it
        let mut m = menu_at((300.0, 200.0));
        m.mouse_move(size_row.cx(), size_row.cy());
        m.mouse_move(m.root.rows[0].cx(), m.root.rows[0].cy());
        assert!(!m.step(0.5, false).1 && m.sub.is_none());
        // a greyed "Size" opens nothing
        let mut one = MenuPanel::new(card_menu(Lang::En, false, &[Size::M], Size::M), (300.0, 200.0), WORK, &mono);
        let r = one.root.rows[1];
        assert_eq!(one.mouse_down(r.cx(), r.cy()), MAct::None);
        assert!(one.sub.is_none());
    }

    #[test]
    fn open_and_close_animation() {
        let mut m = menu_at((300.0, 200.0));
        assert_eq!((m.t, m.eased(), m.scrim()), (0.0, 0.0, SCRIM));
        let mut steps = 0;
        while m.step(0.016, false).0 {
            steps += 1;
        }
        assert!((5..=12).contains(&steps), "{steps}");
        assert_eq!((m.t, m.eased()), (1.0, 1.0));
        m.open_sub(1, false);
        assert_eq!(m.sub_eased(), 0.0);
        while m.step(0.016, false).0 {}
        assert_eq!(m.sub_eased(), 1.0);
        m.closing = true;
        let mut steps = 0;
        while m.step(0.016, false).0 {
            steps += 1;
        }
        assert!(steps < 10 && m.t == 0.0 && m.sub_eased() == 0.0);
        // reduced motion: no animation at all
        let mut r = menu_at((300.0, 200.0));
        assert!(!r.step(0.016, true).0 && r.eased() == 1.0);
        r.closing = true;
        assert!(!r.step(0.016, true).0 && r.eased() == 0.0);
    }

    #[test]
    fn click_action_submenu() {
        use crate::sys::targets::{presets, Click};
        // widgets without a card-body click get no item
        assert!(click_item(Lang::En, "timer", &Click::Default).is_none());
        assert_eq!(card_menu_with(Lang::En, false, &Size::ALL, Size::M, None), card_menu(Lang::En, false, &Size::ALL, Size::M));
        let it = click_item(Lang::En, "sysmon", &Click::Preset("resmon".into())).unwrap();
        assert_eq!(it.label, "Click action");
        let ids: Vec<Option<usize>> = it.children.iter().map(|e| e.item().map(|i| i.id)).collect();
        let n = presets("sysmon").len();
        assert_eq!(ids.len(), 2 + 1 + n + 1 + 1);
        assert_eq!(&ids[..2], &[Some(CMD_CLICK), Some(CMD_CLICK_NONE)]);
        assert_eq!(ids.last(), Some(&Some(CMD_CLICK_CUSTOM)));
        let checked: Vec<usize> = it.children.iter().filter_map(|e| e.item()).filter(|i| i.checked).map(|i| i.id).collect();
        assert_eq!(checked, vec![CMD_CLICK_PRESET + 1]); // exactly the current one
        for (cur, want) in [(Click::Default, CMD_CLICK), (Click::Nothing, CMD_CLICK_NONE), (Click::Custom("https://a.b".into()), CMD_CLICK_CUSTOM)] {
            let it = click_item(Lang::Ru, "clock", &cur).unwrap();
            let c: Vec<usize> = it.children.iter().filter_map(|e| e.item()).filter(|i| i.checked).map(|i| i.id).collect();
            assert_eq!(c, vec![want]);
            assert_eq!(it.label, "Действие по клику");
        }
        // an unknown preset key checks nothing (the click falls back to the default)
        let it = click_item(Lang::En, "disks", &Click::Preset("gone".into())).unwrap();
        assert!(it.children.iter().filter_map(|e| e.item()).all(|i| !i.checked));
        // in the card menu it sits between "Size" and the separator
        let m = card_menu_with(Lang::En, false, &Size::ALL, Size::M, click_item(Lang::En, "disks", &Click::Default));
        let ids: Vec<Option<usize>> = m.iter().map(|e| e.item().map(|i| i.id)).collect();
        assert_eq!(ids, vec![Some(CMD_EDIT), Some(0), Some(0), None, Some(CMD_DELETE)]);
        // command decoding
        assert_eq!(click_pick(CMD_CLICK), Some(ClickPick::Default));
        assert_eq!(click_pick(CMD_CLICK_NONE), Some(ClickPick::Nothing));
        assert_eq!(click_pick(CMD_CLICK_CUSTOM), Some(ClickPick::Custom));
        assert_eq!(click_pick(CMD_CLICK_PRESET + 3), Some(ClickPick::Preset(3)));
        for other in [0, CMD_EDIT, CMD_SIZE, CMD_DELETE, 103, 109, CMD_CLICK_PRESET + CLICK_PRESETS_MAX] {
            assert_eq!(click_pick(other), None, "{other}");
        }
        // the panel lays out and its submenu opens (row 2 = Click action)
        let mut p = MenuPanel::new(m, (300.0, 200.0), WORK, &mono);
        assert!(p.open_sub(2, false) && p.sub.as_ref().unwrap().1.check);
    }
}
