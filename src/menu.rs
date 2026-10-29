//! The on-canvas context menu of the widget host: the generic model (items, panels, placement, keyboard / mouse) lives in `glasskit::menu` and
//! is re-exported here; this file adds the card menu (Edit, Size, Click action, Remove) and its command ids. `ui.rs` draws a `MenuPanel`,
//! `host.rs` feeds it keys / mouse and shows it on the host canvas.
pub use glasskit::menu::*;

use crate::widget::{Lang, Size};

/// Command ids of the card menu (the native fallback in host.rs uses the same ones).
pub const CMD_EDIT: usize = 1;
pub const CMD_SIZE: usize = 10; // + 0 / 1 / 2 for S / M / L
pub const CMD_DELETE: usize = 20;

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::widget::Rect;

    /// 8 px per char
    fn mono(s: &str) -> f32 {
        s.chars().count() as f32 * 8.0
    }
    const WORK: Rect = Rect::new(0.0, 0.0, 1920.0, 1040.0);

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
