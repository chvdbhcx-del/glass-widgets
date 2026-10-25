//! Notes: sticky-note look on the glass (a warm tint). S = the current note (title + body, ~6 lines), M = the same on the wide card,
//! L = a list of up to 5 notes (title + one preview line, the selected one highlighted) with the selected note expanded below.
//! Storage: `%APPDATA%\macshell\notes.json` = `[{"id":u64,"text":str,"updated_ms":i64}]`, atomic write, fresh read-modify-write on every
//! change, re-read when the mtime changes (5 s poll). Newest (last edited) note first. Caps: 200 notes, 20000 chars each.
//! Interaction: click the text = edit (popup, Ctrl+Enter saves), "+" = new note, the trash icon needs two clicks within 3 s (the icon
//! turns red), wheel = previous/next note (S, M, L), L: a click on a list row selects it, a click on the selected row edits.
//! NOTE: `cx.prompt_text` returns None for both Esc and an empty text, so "an empty text deletes the note" cannot be detected: the trash
//! icon is the way to delete. Notes use \n internally; the Win32 EDIT control wants \r\n, converted both ways here.
use super::reminders::{load_json, mtime, now_ms, save_json, store_path, wrap_lines, Load};
use crate::registry::{Registry, WidgetDef};
use crate::widget::*;
use std::time::{Instant, SystemTime};

pub fn register(r: &mut Registry) {
    r.register(WidgetDef {
        type_id: "notes",
        name: ("Заметки", "Notes"),
        desc: ("Быстрые заметки на рабочем столе", "Quick notes on your desktop"),
        group: "notes",
        icon: Glyph::Pin,
        sizes: &[Size::S, Size::M, Size::L],
        default_size: Size::M,
        make: || Box::new(Notes::default()),
    });
}

// ================================================================ pure store

const MAX_NOTES: usize = 200;
const MAX_TEXT: usize = 20_000;

#[derive(Clone, Debug, PartialEq)]
struct Note {
    id: u64,
    text: String,
    updated_ms: i64,
}

fn notes_from_json(v: &Value) -> Option<Vec<Note>> {
    let mut l: Vec<Note> = v
        .as_array()?
        .iter()
        .filter_map(|e| Some(Note { id: e.get("id")?.as_u64()?, text: e.get("text")?.as_str()?.to_string(), updated_ms: e.get("updated_ms").and_then(Value::as_i64).unwrap_or(0) }))
        .collect();
    sort(&mut l);
    Some(l)
}
fn notes_to_json(l: &[Note]) -> Value {
    Value::Array(l.iter().map(|n| json!({"id": n.id, "text": n.text, "updated_ms": n.updated_ms})).collect())
}
/// newest first
fn sort(l: &mut [Note]) {
    l.sort_by(|a, b| b.updated_ms.cmp(&a.updated_ms).then(b.id.cmp(&a.id)));
}
/// \r\n -> \n, trailing blanks removed, capped
fn clean(s: &str) -> String {
    let s = s.replace("\r\n", "\n").replace('\r', "\n");
    s.trim_end().chars().take(MAX_TEXT).collect()
}
fn add_note(l: &mut Vec<Note>, text: &str, now: i64) -> bool {
    let text = clean(text);
    if text.trim().is_empty() || l.len() >= MAX_NOTES {
        return false;
    }
    let id = l.iter().map(|n| n.id).max().unwrap_or(0) + 1;
    l.push(Note { id, text, updated_ms: now });
    sort(l);
    true
}
fn edit_note(l: &mut [Note], id: u64, text: &str, now: i64) -> bool {
    let text = clean(text);
    match l.iter_mut().find(|n| n.id == id) {
        Some(n) if !text.trim().is_empty() && n.text != text => {
            n.text = text;
            n.updated_ms = now;
            sort(l);
            true
        }
        _ => false,
    }
}
fn delete_note(l: &mut Vec<Note>, id: u64) -> bool {
    let n = l.len();
    l.retain(|x| x.id != id);
    l.len() != n
}
/// first non-empty line
fn title_of(t: &str) -> &str {
    t.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("")
}
/// the lines after the title, joined by blanks
fn preview_of(t: &str) -> String {
    t.lines().map(str::trim).filter(|l| !l.is_empty()).skip(1).collect::<Vec<_>>().join(" ")
}
fn win_nl(s: &str) -> String {
    s.replace('\n', "\r\n")
}

fn ellipsize(mut l: String, max_w: f32, mw: &dyn Fn(&str) -> f32) -> String {
    loop {
        let t = format!("{}…", l.trim_end());
        if mw(&t) <= max_w || l.chars().count() <= 1 {
            return t;
        }
        l.pop();
    }
}

/// The note as display lines: (text, is_title). The first paragraph is a single title line, the others wrap; blank lines (one at most
/// in a row) separate paragraphs; at most `max` lines, the last gets an ellipsis when text was cut.
fn text_lines(text: &str, w: f32, max: usize, mt: &dyn Fn(&str) -> f32, mb: &dyn Fn(&str) -> f32) -> Vec<(String, bool)> {
    let mut out: Vec<(String, bool)> = Vec::new();
    let mut first = true;
    for p in text.split('\n') {
        let p = p.trim();
        if p.is_empty() {
            if !first && out.last().is_some_and(|l| !l.0.is_empty()) {
                out.push((String::new(), false));
            }
            continue;
        }
        if first {
            first = false;
            out.extend(wrap_lines(p, 1, w, mt).into_iter().map(|s| (s, true)));
        } else {
            out.extend(wrap_lines(p, 1000, w, mb).into_iter().map(|s| (s, false)));
        }
    }
    while out.last().is_some_and(|l| l.0.is_empty()) {
        out.pop();
    }
    if out.len() > max.max(1) {
        out.truncate(max.max(1));
        while out.len() > 1 && out.last().is_some_and(|l| l.0.is_empty()) {
            out.pop();
        }
        if let Some((s, t)) = out.pop() {
            out.push((ellipsize(s, w, if t { mt } else { mb }), t));
        }
    }
    out
}

// ================================================================ widget

#[derive(Clone, Copy)]
enum Ask {
    New,
    Edit(u64),
}

#[derive(Default)]
struct Notes {
    notes: Vec<Note>,
    sel: u64,
    mtime: Option<SystemTime>,
    corrupt: bool,
    started: bool,
    ver: u64,
    armed: Option<Instant>,
    ask: Option<(Pending<Option<String>>, Ask)>,
}

fn sample() -> Vec<Note> {
    let mk = |id: u64, t: &str| Note { id, text: t.into(), updated_ms: 1_000 - id as i64 };
    vec![
        mk(1, "План на неделю\nПонедельник: дописать виджеты и прогнать сборку\nСреда: созвон с командой в 15:00\nПятница: релиз и отчёт\n\nНе забыть купить кофе и позвонить маме"),
        mk(2, "Список покупок\nмолоко, хлеб, яйца, кофе, сыр"),
        mk(3, "Идея для приложения\nЗаметки, которые живут прямо на рабочем столе"),
        mk(4, "Пароль от Wi-Fi для гостей\nнаписан на холодильнике"),
        mk(5, "Книги\nПрочитать: Чистая архитектура, Rust в действии"),
        mk(6, "Подарок на день рождения"),
    ]
}

const LH: f32 = 17.0;
const ICON: f32 = 20.0;
const ROW_H: f32 = 36.0;

impl Notes {
    fn init(&mut self, cx: &Cx) {
        if self.started {
            return;
        }
        self.started = true;
        self.ver += 1;
        if cx.preview {
            self.notes = sample();
            self.sel = 1;
        } else {
            self.reload();
        }
    }

    fn fix_sel(&mut self) {
        if !self.notes.iter().any(|n| n.id == self.sel) {
            self.sel = self.notes.first().map_or(0, |n| n.id);
        }
    }

    fn reload(&mut self) {
        let Some(p) = store_path("notes.json") else { return };
        self.mtime = mtime(&p);
        let (list, corrupt) = match load_json(&p) {
            Load::Missing => (Vec::new(), false),
            Load::Ok(v) => match notes_from_json(&v) {
                Some(l) => (l, false),
                None => (Vec::new(), true),
            },
            Load::Corrupt => (Vec::new(), true),
        };
        if corrupt != self.corrupt || list != self.notes {
            self.notes = list;
            self.corrupt = corrupt;
            self.fix_sel();
            self.ver += 1;
        }
    }

    fn modify(&mut self, f: impl FnOnce(&mut Vec<Note>) -> bool) -> bool {
        let Some(p) = store_path("notes.json") else { return false };
        let mut list = match load_json(&p) {
            Load::Missing => Vec::new(),
            Load::Ok(v) => match notes_from_json(&v) {
                Some(l) => l,
                None => {
                    self.corrupt = true;
                    return false;
                }
            },
            Load::Corrupt => {
                self.corrupt = true;
                return false;
            }
        };
        if !f(&mut list) {
            return false;
        }
        let _ = save_json(&p, &notes_to_json(&list));
        self.notes = list;
        self.mtime = mtime(&p);
        self.corrupt = false;
        self.fix_sel();
        self.ver += 1;
        true
    }

    fn sel_idx(&self) -> Option<usize> {
        self.notes.iter().position(|n| n.id == self.sel)
    }
    fn selected(&self) -> Option<&Note> {
        self.sel_idx().map(|i| &self.notes[i])
    }

    fn ask_for(&mut self, cx: &Cx, a: Ask) {
        if self.ask.is_some() {
            return;
        }
        let (title, initial) = match a {
            Ask::New => (cx.t("Новая заметка", "New note"), String::new()),
            Ask::Edit(id) => (cx.t("Заметка", "Note"), self.notes.iter().find(|n| n.id == id).map(|n| win_nl(&n.text)).unwrap_or_default()),
        };
        self.ask = Some((cx.prompt_text(title, &initial, true), a));
    }

    fn step(&mut self, d: i32) -> bool {
        let Some(i) = self.sel_idx() else { return false };
        let j = (i as i32 + d).clamp(0, self.notes.len() as i32 - 1) as usize;
        if j == i {
            return false;
        }
        self.sel = self.notes[j].id;
        self.ver += 1;
        true
    }

    /// (header icons, body rect, L list rect) in card px
    fn geo(size: Size, c: Rect) -> (Rect, Rect, Rect, Rect) {
        let plus = Rect::new(c.right() - ICON, c.y - 2.0, ICON, ICON);
        let trash = Rect::new(c.right() - 2.0 * ICON - 2.0, c.y - 2.0, ICON, ICON);
        let (list, body) = if size == Size::L {
            let rows = Rect::new(c.x, c.y + 30.0, c.w, 0.0);
            (rows, Rect::new(c.x, c.y + 30.0, c.w, c.h - 30.0))
        } else {
            (Rect::default(), Rect::new(c.x, c.y + 26.0, c.w, c.h - 26.0))
        };
        (plus, trash, list, body)
    }

    /// L: the notes shown in the list: (first index, count) with the selection kept in view
    fn window(&self) -> (usize, usize) {
        let n = self.notes.len().min(5);
        let i = self.sel_idx().unwrap_or(0);
        (i.saturating_sub(n.saturating_sub(1) / 2).min(self.notes.len() - n), n)
    }
}

impl Widget for Notes {
    fn draw(&mut self, cx: &mut Cx) -> Result<()> {
        self.init(cx);
        let c = cx.content();
        // the warm "sticky" tint over the whole card
        let r = cx.size.radius() - 0.6;
        cx.gradient_rrect(cx.bounds().inset(0.6), r, YELLOW.alpha(0.20), ORANGE.alpha(0.09), 160.0);
        let (plus, trash, _, body) = Notes::geo(cx.size, c);
        let n = self.notes.len();
        // header: title, position, trash, plus
        cx.text(Rect::new(c.x, c.y - 1.0, 100.0, 18.0), cx.t("Заметки", "Notes"), Style::TITLE, YELLOW);
        if n > 1 {
            let pos = if cx.size == Size::L { format!("{n}") } else { format!("{}/{}", self.sel_idx().map_or(1, |i| i + 1), n) };
            cx.text(Rect::new(c.x + 62.0, c.y - 1.0, 60.0, 18.0), &pos, Style::CAPTION.mono(), DIM);
        }
        cx.icon(Glyph::Plus, plus.inset(2.0), TEXT);
        if n > 0 {
            cx.icon(Glyph::Trash, trash.inset(2.5), if self.armed.is_some() { RED } else { DIM });
        }
        if n == 0 {
            let msg = if self.corrupt { cx.t("Не удалось прочитать файл", "Cannot read the file") } else { cx.t("Нет заметок", "No notes") };
            cx.text(body.inset_xy(0.0, 12.0), msg, Style::BODY.center(), DIM);
            return Ok(());
        }
        let bs = Style::BODY.size(12.5);
        let (mt, mb) = (|s: &str| cx.measure(s, Style::TITLE, 0.0).0, |s: &str| cx.measure(s, bs, 0.0).0);
        let draw_text = |text: &str, area: Rect, max: usize| {
            let lines = text_lines(text, area.w, max, &mt, &mb);
            let mut y = area.y;
            for (s, is_title) in &lines {
                if s.is_empty() {
                    y += LH * 0.5;
                    continue;
                }
                cx.text(Rect::new(area.x, y, area.w, LH), s, if *is_title { Style::TITLE.ellipsis() } else { bs.ellipsis() }, if *is_title { TEXT } else { TEXT.fade(0.86) });
                y += LH;
            }
        };
        let Some(cur) = self.selected() else { return Ok(()) };
        match cx.size {
            Size::S | Size::M => {
                let max = (body.h / LH).floor() as usize;
                draw_text(&cur.text, body, max);
            }
            Size::L => {
                let (first, shown) = self.window();
                let list_h = shown as f32 * ROW_H;
                for k in 0..shown {
                    let note = &self.notes[first + k];
                    let row = Rect::new(c.x, body.y + k as f32 * ROW_H, c.w, ROW_H);
                    let on = note.id == self.sel;
                    if on {
                        cx.rrect(row.inset_xy(-8.0, 1.0), 9.0, FILL);
                    } else if k + 1 < shown && self.notes[first + k + 1].id != self.sel {
                        cx.rect(Rect::new(row.x, row.bottom() - 0.3, row.w, 0.6), FILL);
                    }
                    cx.text(Rect::new(row.x, row.y + 3.0, row.w, 17.0), title_of(&note.text), Style::TITLE.ellipsis(), TEXT);
                    let pv = preview_of(&note.text);
                    cx.text(Rect::new(row.x, row.y + 19.0, row.w, 14.0), if pv.is_empty() { cx.t("Нет дополнительного текста", "No additional text") } else { &pv }, Style::CAPTION.ellipsis(), DIM);
                }
                let text_area = Rect::new(c.x, body.y + list_h + 14.0, c.w, c.bottom() - (body.y + list_h + 14.0));
                cx.rect(Rect::new(c.x, text_area.y - 7.0, c.w, 0.6), FILL);
                draw_text(&cur.text, text_area, (text_area.h / LH).floor() as usize);
            }
        }
        Ok(())
    }

    fn tick(&mut self, cx: &Cx) -> Tick {
        self.init(cx);
        if !cx.preview {
            let (nm, polled) = (now_ms(cx), self.ask.as_ref().map(|(p, a)| (p.take(), p.busy(), *a)));
            if let Some((res, busy, a)) = polled {
                match res {
                    Some(r) => {
                        self.ask = None;
                        if let Some(s) = r {
                            match a {
                                Ask::New => {
                                    if self.modify(|l| add_note(l, &s, nm)) {
                                        self.sel = self.notes.first().map_or(0, |n| n.id);
                                    }
                                }
                                Ask::Edit(id) => {
                                    self.modify(|l| edit_note(l, id, &s, nm));
                                }
                            }
                        }
                    }
                    None if !busy => self.ask = None,
                    None => {}
                }
            }
            if self.armed.is_some_and(|t| t.elapsed().as_secs_f32() > 3.0) {
                self.armed = None;
                self.ver += 1;
            }
            if store_path("notes.json").is_some_and(|p| mtime(&p) != self.mtime) {
                self.reload();
            }
        }
        if cx.preview {
            Tick::Never
        } else if self.armed.is_some() {
            Tick::Poll(Duration::from_secs(1))
        } else {
            Tick::Poll(Duration::from_secs(5))
        }
    }

    fn mouse(&mut self, ev: &MouseEv, cx: &mut Cx) -> bool {
        if cx.preview {
            return false;
        }
        match ev.kind {
            MouseKind::Wheel => return self.step(if ev.wheel() > 0.0 { -1 } else { 1 }),
            MouseKind::Leave => {
                return self.armed.take().is_some_and(|_| {
                    self.ver += 1;
                    true
                })
            }
            MouseKind::Up if ev.button == Button::Left => {}
            _ => return false,
        }
        let c = cx.content();
        let (plus, trash, _, body) = Notes::geo(cx.size, c);
        let disarmed = self.armed.take().is_some();
        if disarmed {
            self.ver += 1;
        }
        if ev.inside(plus.inset(-4.0)) {
            self.ask_for(cx, Ask::New);
            return disarmed;
        }
        if ev.inside(trash.inset(-4.0)) && !self.notes.is_empty() {
            if disarmed {
                // second click: delete the selected note, select its neighbour
                let (id, i) = (self.sel, self.sel_idx().unwrap_or(0));
                if self.modify(|l| delete_note(l, id)) {
                    self.sel = self.notes.get(i.min(self.notes.len().saturating_sub(1))).map_or(0, |n| n.id);
                    self.ver += 1;
                }
            } else {
                self.armed = Some(Instant::now());
                self.ver += 1;
                let _ = cx.spawn(|| ()); // pokes tick() so the 3 s disarm timer runs (mouse events do not call tick)
            }
            return true;
        }
        if self.notes.is_empty() {
            if ev.inside(body) {
                self.ask_for(cx, Ask::New);
            }
            return disarmed;
        }
        if cx.size == Size::L {
            let (first, shown) = self.window();
            for k in 0..shown {
                let row = Rect::new(c.x - 8.0, body.y + k as f32 * ROW_H, c.w + 16.0, ROW_H);
                if ev.inside(row) {
                    let id = self.notes[first + k].id;
                    if id == self.sel {
                        self.ask_for(cx, Ask::Edit(id));
                        return disarmed;
                    }
                    self.sel = id;
                    self.ver += 1;
                    return true;
                }
            }
        }
        if ev.inside(body) {
            if let Some(id) = self.selected().map(|n| n.id) {
                self.ask_for(cx, Ask::Edit(id));
            }
        }
        disarmed
    }

    fn version(&self) -> u64 {
        self.ver
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mw(s: &str) -> f32 {
        s.chars().count() as f32 * 7.0
    }

    #[test]
    fn store_operations() {
        let mut l = Vec::new();
        assert!(add_note(&mut l, "first\r\nline two  ", 10));
        assert!(add_note(&mut l, "second", 20));
        assert!(!add_note(&mut l, "  \n ", 30));
        assert_eq!(l[0].text, "second"); // newest first
        assert_eq!(l[1].text, "first\nline two");
        let id_first = l[1].id;
        assert!(edit_note(&mut l, id_first, "first!", 40));
        assert_eq!(l[0].id, id_first); // an edited note jumps to the top
        assert!(!edit_note(&mut l, id_first, "first!", 50) && !edit_note(&mut l, 99, "x", 50) && !edit_note(&mut l, id_first, " ", 50));
        assert!(delete_note(&mut l, id_first) && !delete_note(&mut l, id_first));
        let mut full: Vec<Note> = (0..MAX_NOTES as u64).map(|i| Note { id: i + 1, text: "x".into(), updated_ms: i as i64 }).collect();
        assert!(!add_note(&mut full, "over", 1));
    }

    #[test]
    fn title_and_preview() {
        assert_eq!(title_of("\n  Заголовок \nтекст"), "Заголовок");
        assert_eq!(preview_of("Заголовок\n\nстрока 1\n строка 2 "), "строка 1 строка 2");
        assert_eq!(preview_of("только заголовок"), "");
        assert_eq!(win_nl("a\nb"), "a\r\nb");
    }

    #[test]
    fn lines_layout() {
        let l = text_lines("Title\nsome words here and there\n\n\nlast", 70.0, 10, &mw, &mw);
        let t: Vec<&str> = l.iter().map(|x| x.0.as_str()).collect();
        assert_eq!(t, vec!["Title", "some words", "here and", "there", "", "last"]);
        assert!(l[0].1 && !l[1].1);
        let cut = text_lines("Title\nsome words here and there\nlast", 70.0, 3, &mw, &mw);
        assert_eq!(cut.len(), 3);
        assert_eq!(cut[2].0, "here and…");
        assert!(text_lines("", 70.0, 3, &mw, &mw).is_empty());
        assert_eq!(text_lines("a very long title indeed", 70.0, 5, &mw, &mw).len(), 1); // the title never wraps
    }

    #[test]
    fn json_round_trip() {
        let dir = std::env::temp_dir().join(format!("mswtest-notes-{}", std::process::id()));
        let f = dir.join("notes.json");
        let l = vec![Note { id: 2, text: "б\nв \"q\"".into(), updated_ms: 9 }, Note { id: 1, text: "а".into(), updated_ms: 3 }];
        save_json(&f, &notes_to_json(&l)).unwrap();
        let Load::Ok(v) = load_json(&f) else { panic!("load") };
        assert_eq!(notes_from_json(&v).unwrap(), l);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
