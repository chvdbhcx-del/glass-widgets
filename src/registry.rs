//! Widget registry: every `widgets/<id>.rs` registers one `WidgetDef` (metadata + factory). The gallery, `--sheet`, the layout loader
//! and the card system only ever talk to this table, so adding a widget never touches the core.
use crate::widget::{Glyph, Size, Widget};
use std::sync::OnceLock;

#[derive(Clone, Copy)]
pub struct WidgetDef {
    /// unique lower-case id, also the `type` in config.json widgetsLayout
    pub type_id: &'static str,
    /// (ru, en)
    pub name: (&'static str, &'static str),
    /// one line shown under the gallery preview (ru, en)
    pub desc: (&'static str, &'static str),
    /// gallery sidebar group (one per widget family: "clock", "calendar", "reminders", "music", "batteries", "weather", "system", ...)
    pub group: &'static str,
    /// glyph of the gallery sidebar entry
    pub icon: Glyph,
    /// supported sizes, in display order
    pub sizes: &'static [Size],
    pub default_size: Size,
    /// factory: one fresh instance per card
    pub make: fn() -> Box<dyn Widget>,
}

#[derive(Default)]
pub struct Registry {
    defs: Vec<WidgetDef>,
    /// ids starting with '_' are developer tools (primitive gallery): reachable by `get`, never listed
    hidden: Vec<WidgetDef>,
    stubs: Vec<&'static str>,
}

impl Registry {
    pub fn register(&mut self, d: WidgetDef) {
        debug_assert!(!d.sizes.is_empty() && d.sizes.contains(&d.default_size), "{}: default_size must be one of sizes", d.type_id);
        let list = if d.type_id.starts_with('_') { &mut self.hidden } else { &mut self.defs };
        if let Some(i) = list.iter().position(|x| x.type_id == d.type_id) {
            list[i] = d; // a later registration replaces an earlier one
        } else {
            list.push(d);
        }
        self.stubs.retain(|s| *s != d.type_id);
    }
    /// Placeholder registration ("скоро" card): excluded from the default layout.
    pub fn register_stub(&mut self, d: WidgetDef) {
        self.register(d);
        self.stubs.push(d.type_id);
    }
    /// All widgets in registration order (the order of `widgets/mod.rs`).
    pub fn all(&self) -> &[WidgetDef] {
        &self.defs
    }
    pub fn get(&self, id: &str) -> Option<&WidgetDef> {
        self.defs.iter().chain(self.hidden.iter()).find(|d| d.type_id == id)
    }
    pub fn is_stub(&self, id: &str) -> bool {
        self.stubs.contains(&id)
    }
}

/// The process-wide registry, built once from `widgets::register_all`.
pub fn global() -> &'static Registry {
    static R: OnceLock<Registry> = OnceLock::new();
    R.get_or_init(|| {
        let mut r = Registry::default();
        crate::widgets::register_all(&mut r);
        r
    })
}
