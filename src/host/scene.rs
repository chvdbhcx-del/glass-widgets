//! Scenes glue (the rules and their state machine are in scenes.rs): the ~1 s evaluation from the guard timer and the tray menu's rule editor.
use super::*;
use crate::scenes::{self, Rule, When};
use std::sync::Mutex;

/// How often the foreground program / the clock are looked at. One GetForegroundWindow plus a stat of config.json per evaluation.
const EVERY: Duration = Duration::from_millis(1000);

/// What the evaluation needs from config.json, re-read only when the file (or the monitor the desk is on) changed.
#[derive(Default)]
pub(super) struct Cfg {
    mtime: Option<SystemTime>,
    slot: Option<String>,
    read: bool,
    profiles: Vec<String>,
    cur: Option<String>,
}

/// The rule being typed in a prompt: (time range? else exe name, profile, text), posted back as `WM_SCENE_TEXT`.
static SCENE_REQ: Mutex<Option<(bool, String, String)>> = Mutex::new(None);
/// The programs the open tray menu listed (the menu answers with an index into it).
pub(super) static SCENE_PROGRAMS: Mutex<Vec<String>> = Mutex::new(Vec::new());

impl App {
    /// Called by the guard (every 500 ms), acts every `EVERY`: nothing at all while there are no rules and nothing to undo, nothing while the user is
    /// arranging the desk (edit mode, settings, a drag, a menu, a prompt) or the layer is hidden by a fullscreen app.
    pub(super) unsafe fn scenes_tick(&mut self) {
        if self.scene_t.elapsed() < EVERY {
            return;
        }
        self.scene_t = Instant::now();
        if (self.scene_rules.is_empty() && self.scenes.idle()) || !self.visible || self.interacting() {
            return;
        }
        match foreground_program(self.hwnd, &mut self.scene_fg) {
            Fg::Exe(e) => self.scene_prog = Some(e),
            Fg::Desktop => self.scene_prog = None,
            Fg::Keep => {}
        }
        let m = layout::config_mtime();
        if !self.scene_cfg.read || m != self.scene_cfg.mtime || self.scene_cfg.slot != self.slot {
            let root = layout::read_config();
            self.scene_cfg = Cfg { mtime: m, slot: self.slot.clone(), read: true, profiles: layout::profile_names(&root), cur: layout::active_profile_for(&root, self.slot.as_deref()) };
        }
        let now = self.now();
        let minute = (now.h * 60 + now.mi) as u16;
        let profiles = &self.scene_cfg.profiles;
        let exists = |p: &str| profiles.iter().any(|x| x == p);
        let win = scenes::winner(&self.scene_rules, self.scene_prog.as_deref(), minute, &exists).cloned();
        let go = self.scenes.step(win.as_ref(), self.scene_cfg.cur.as_deref(), &exists);
        if let Some(name) = go {
            self.switch_profile_named(&name);
        }
    }

    /// `sceneRules` read again (config.json changed behind our back).
    pub(super) fn scenes_reload(&mut self, root: &serde_json::Value) {
        self.scene_rules = scenes::rules_from_value(root);
    }

    pub(super) fn scene_menu_lines(&self) -> Vec<String> {
        self.scene_rules.iter().map(scenes::describe).collect()
    }

    /// Store `rules` as `sceneRules`; they take effect only when config.json was written.
    unsafe fn scene_store(&mut self, rules: Vec<Rule>) {
        let r = layout::write_setting("sceneRules", scenes::rules_to_value(&rules));
        self.note_own_write();
        match r {
            Ok(()) => self.scene_rules = rules,
            Err(e) => self.show_toast(format!("{}: {e}", self.ui.lang.pick("Правило не сохранено", "Rule not saved"))),
        }
    }

    unsafe fn scene_add(&mut self, when: When, profile: String) {
        let mut rules = self.scene_rules.clone();
        let rule = Rule { when, profile };
        if rules.contains(&rule) {
            return;
        }
        rules.push(rule.clone());
        self.scene_store(rules);
        if self.scene_rules.last() == Some(&rule) {
            self.show_toast(format!("{} {}", self.ui.lang.pick("Правило добавлено:", "Rule added:"), scenes::describe(&rule)));
        }
    }

    pub(super) unsafe fn scene_remove(&mut self, i: usize) {
        let mut rules = self.scene_rules.clone();
        if i < rules.len() {
            rules.remove(i);
            self.scene_store(rules);
        }
    }

    /// Tray: a rule for the `prog`-th program of the menu that was open, switching to the `prof`-th profile.
    pub(super) unsafe fn scene_add_program(&mut self, prog: usize, prof: usize) {
        let p = SCENE_PROGRAMS.lock().ok().and_then(|g| g.get(prog).cloned());
        let name = layout::profile_names(&layout::read_config()).get(prof).cloned();
        if let (Some(p), Some(name)) = (p, name) {
            self.scene_add(When::Program(p), name);
        }
    }

    /// Tray: ask for an exe name (`time` false) or a time range (`time` true) for the `prof`-th profile. The prompt runs on its own thread (it has a
    /// message loop); the text comes back as `WM_SCENE_TEXT`.
    pub(super) unsafe fn ask_scene_text(&mut self, time: bool, prof: usize) {
        let Some(profile) = layout::profile_names(&layout::read_config()).get(prof).cloned() else { return };
        let (lang, hwnd) = (self.ui.lang, self.hwnd.0 as isize);
        let _ = std::thread::Builder::new().name("scene-rule".into()).spawn(move || {
            let (title, hint) = if time { (lang.pick("Интервал времени", "Time range"), "22:00-06:00") } else { (lang.pick("Программа (имя .exe)", "Program (.exe name)"), "chrome.exe") };
            let mut o = crate::input::PromptOpts::new(&format!("{title} \u{2192} {profile}"), "", false);
            (o.placeholder, o.ok_label, o.lang) = (hint.to_string(), lang.pick("Добавить", "Add").to_string(), lang);
            if let Some(text) = crate::input::prompt_ex(o) {
                if let Ok(mut g) = SCENE_REQ.lock() {
                    *g = Some((time, profile, text));
                    let _ = PostMessageW(HWND(hwnd as *mut _), WM_SCENE_TEXT, WPARAM(0), LPARAM(0));
                }
            }
        });
    }

    /// The prompt answered: a rule, or a toast saying what was not understood.
    pub(super) unsafe fn scene_text_named(&mut self) {
        let Some((time, profile, text)) = SCENE_REQ.lock().ok().and_then(|mut g| g.take()) else { return };
        let when = if time { scenes::parse_range(&text).map(|(from, to)| When::Time { from, to }) } else { scenes::norm_program(&text).map(When::Program) };
        match when {
            Some(w) => self.scene_add(w, profile),
            None => {
                let msg = if time { self.ui.lang.pick("Нужно, например, 22:00-06:00", "Expected something like 22:00-06:00") } else { self.ui.lang.pick("Нужно имя программы, например chrome.exe", "Expected a program name like chrome.exe") };
                self.show_toast(msg.to_string());
            }
        }
    }
}
