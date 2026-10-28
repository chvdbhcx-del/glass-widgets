//! One file per widget: `widgets/<id>.rs` with `pub fn register(r: &mut Registry)`. This list is complete on purpose:
//! a widget author only ever replaces the body of their own file; nothing in the core or in this file has to change.
use crate::registry::Registry;
use crate::widget::*;

pub mod agents;
pub mod batteries;
pub mod calendar;
pub mod clipboard;
pub mod clock;
pub mod controls;
pub mod countdown;
pub mod crypto;
pub mod dev;
pub mod disks;
pub mod music;
pub mod network;
pub mod nextevent;
pub mod notes;
pub mod photos;
pub mod processes;
pub mod quote;
pub mod rates;
pub mod reminders;
pub mod shortcuts;
pub mod sysmon;
pub mod timer;
pub mod weather;
pub mod worldclock;

/// Registration order = gallery order.
pub fn register_all(r: &mut Registry) {
    clock::register(r);
    calendar::register(r);
    reminders::register(r);
    music::register(r);
    batteries::register(r);
    weather::register(r);
    worldclock::register(r);
    sysmon::register(r);
    disks::register(r);
    network::register(r);
    processes::register(r);
    notes::register(r);
    timer::register(r);
    shortcuts::register(r);
    clipboard::register(r);
    photos::register(r);
    rates::register(r);
    quote::register(r);
    nextevent::register(r);
    countdown::register(r);
    crypto::register(r);
    controls::register(r);
    agents::register(r);
    dev::register(r); // hidden developer tool (id starts with '_')
}

/// Placeholder card used by the not-yet-written widgets: the title and a dim "скоро". Delete its use when you implement the real widget.
pub struct Stub {
    pub ru: &'static str,
    pub en: &'static str,
    pub icon: Glyph,
}
impl Widget for Stub {
    fn draw(&mut self, cx: &mut Cx) -> Result<()> {
        let r = cx.content();
        let (head, _) = r.cut_top(20.0);
        cx.text(head, cx.t(self.ru, self.en), Style::TITLE.ellipsis(), TEXT);
        let mid = cx.bounds().centered(40.0, 40.0).offset(0.0, 4.0);
        cx.icon(self.icon, mid, FAINT);
        let (_, foot) = r.cut_bottom(20.0);
        cx.text(foot, cx.t("скоро", "soon"), Style::CAPTION.center(), DIM);
        Ok(())
    }
    fn tick(&mut self, _cx: &Cx) -> Tick {
        Tick::Never
    }
}
