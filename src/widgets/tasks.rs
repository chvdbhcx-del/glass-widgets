//! Tasks: a plain checklist without dates (Reminders is the dated one). S = the open count and the next task, M = up to 5 tasks with
//! round checkboxes. Done tasks stay in the list (struck out, after the open ones) until they are deleted.
//! Storage: `%APPDATA%\macshell\tasks.json` = `[{"id":u64,"text":str,"done":bool,"created_ms":i64}]`, atomic write, fresh read-modify-write
//! on every change, re-read when the mtime changes (5 s poll). Caps: 200 tasks, 200 characters each.
//! Interaction: "+" = new task (text popup), click a task = done / not done (S: the next task), right click a task = delete (the card
//! context menu belongs to the host and cannot be extended by a widget, so the delete gesture is the one Reminders and Shortcuts use).
use super::reminders::{load_json, mtime, now_ms, save_json, store_path, wrap_lines, Load};
use crate::registry::{Registry, WidgetDef};
use crate::widget::*;
use std::time::SystemTime;

pub fn register(r: &mut Registry) {
    r.register(WidgetDef {
        type_id: "tasks",
        name: ("Задачи", "Tasks"),
        desc: ("Простой список дел с галочками", "A simple checklist"),
        group: "reminders",
        icon: Glyph::CheckCircle,
        sizes: &[Size::S, Size::M],
        default_size: Size::M,
        make: || Box::new(Tasks::default()),
    });
}

// ================================================================ pure store

const MAX_TASKS: usize = 200;
const MAX_TEXT: usize = 200;

#[derive(Clone, Debug, PartialEq)]
struct Task {
    id: u64,
    text: String,
    done: bool,
    created_ms: i64,
}

fn tasks_from_json(v: &Value) -> Option<Vec<Task>> {
    Some(
        v.as_array()?
            .iter()
            .filter_map(|e| {
                let (id, text) = (e.get("id")?.as_u64()?, clean(e.get("text")?.as_str()?));
                (!text.is_empty()).then(|| Task { id, text, done: e.get("done").and_then(Value::as_bool).unwrap_or(false), created_ms: e.get("created_ms").and_then(Value::as_i64).unwrap_or(0) })
            })
            .take(MAX_TASKS)
            .collect(),
    )
}
fn tasks_to_json(l: &[Task]) -> Value {
    Value::Array(l.iter().map(|t| json!({"id": t.id, "text": t.text, "done": t.done, "created_ms": t.created_ms})).collect())
}
/// One line: whitespace collapsed, capped.
fn clean(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(MAX_TEXT).collect()
}
fn add_task(l: &mut Vec<Task>, text: &str, now: i64) -> bool {
    let text = clean(text);
    if text.is_empty() || l.len() >= MAX_TASKS {
        return false;
    }
    let id = l.iter().map(|t| t.id).max().unwrap_or(0) + 1;
    l.push(Task { id, text, done: false, created_ms: now });
    true
}
fn toggle_task(l: &mut [Task], id: u64) -> bool {
    l.iter_mut().find(|t| t.id == id).map(|t| t.done = !t.done).is_some()
}
fn delete_task(l: &mut Vec<Task>, id: u64) -> bool {
    let n = l.len();
    l.retain(|t| t.id != id);
    l.len() != n
}
/// What the card lists: the open tasks in the order they were added, then the done ones.
fn shown(l: &[Task]) -> Vec<&Task> {
    let mut v: Vec<&Task> = l.iter().collect();
    v.sort_by_key(|t| (t.done, t.id));
    v
}

// ================================================================ widget

#[derive(Default)]
struct Tasks {
    tasks: Vec<Task>,
    mtime: Option<SystemTime>,
    corrupt: bool,
    started: bool,
    ver: u64,
    ask: Option<Pending<Option<String>>>,
}

fn sample() -> Vec<Task> {
    let mk = |id: u64, t: &str, done: bool| Task { id, text: t.into(), done, created_ms: id as i64 };
    vec![
        mk(1, "Отправить отчёт", false),
        mk(2, "Купить билеты на пятницу", false),
        mk(3, "Позвонить маме", false),
        mk(4, "Записаться к врачу и узнать про анализы", false),
        mk(5, "Оплатить интернет", true),
        mk(6, "Прибраться на столе", true),
    ]
}

const ROW_H: f32 = 22.0;
const BOX_R: f32 = 7.0;

impl Tasks {
    fn init(&mut self, cx: &Cx) {
        if self.started {
            return;
        }
        self.started = true;
        self.ver += 1;
        if cx.preview {
            self.tasks = sample();
        } else {
            self.reload();
        }
    }

    fn reload(&mut self) {
        let Some(p) = store_path("tasks.json") else { return };
        self.mtime = mtime(&p);
        let (list, corrupt) = match load_json(&p) {
            Load::Missing => (Vec::new(), false),
            Load::Ok(v) => match tasks_from_json(&v) {
                Some(l) => (l, false),
                None => (Vec::new(), true),
            },
            Load::Corrupt => (Vec::new(), true),
        };
        if corrupt != self.corrupt || list != self.tasks {
            self.tasks = list;
            self.corrupt = corrupt;
            self.ver += 1;
        }
    }

    /// Read-modify-write on the fresh file; a corrupt file is never overwritten.
    fn modify(&mut self, f: impl FnOnce(&mut Vec<Task>) -> bool) {
        let Some(p) = store_path("tasks.json") else { return };
        let mut list = match load_json(&p) {
            Load::Missing => Vec::new(),
            Load::Ok(v) => match tasks_from_json(&v) {
                Some(l) => l,
                None => {
                    self.corrupt = true;
                    return;
                }
            },
            Load::Corrupt => {
                self.corrupt = true;
                return;
            }
        };
        if f(&mut list) {
            let _ = save_json(&p, &tasks_to_json(&list));
            self.tasks = list;
            self.mtime = mtime(&p);
            self.corrupt = false;
            self.ver += 1;
        }
    }

    fn open_count(&self) -> usize {
        self.tasks.iter().filter(|t| !t.done).count()
    }

    fn ask_new(&mut self, cx: &Cx) {
        if self.ask.is_none() {
            self.ask = Some(cx.prompt_text_ex(cx.t("Новая задача", "New task"), "", false, cx.t("Что нужно сделать?", "What needs doing?"), cx.t("Добавить", "Add")));
        }
    }

    /// The rect of row `i` (M) in card px.
    fn row(c: Rect, i: usize) -> Rect {
        Rect::new(c.x, c.y + 26.0 + i as f32 * ROW_H, c.w, ROW_H)
    }
    fn plus(c: Rect) -> Rect {
        Rect::new(c.right() - 20.0, c.y - 1.0, 20.0, 20.0)
    }
    /// S: the area of the next task.
    fn next_area(c: Rect) -> Rect {
        Rect::new(c.x, c.y + 70.0, c.w, c.h - 70.0)
    }
    /// The task under the pointer: M = the row, S = the next open task.
    fn hit(&self, size: Size, c: Rect, x: f32, y: f32) -> Option<u64> {
        let list = shown(&self.tasks);
        if size == Size::S {
            return Tasks::next_area(c).contains(x, y).then(|| list.first().filter(|t| !t.done).map(|t| t.id)).flatten();
        }
        list.iter().take(5).enumerate().find(|(i, _)| Tasks::row(c, *i).inset_xy(-8.0, 0.0).contains(x, y)).map(|(_, t)| t.id)
    }
}

impl Widget for Tasks {
    fn draw(&mut self, cx: &mut Cx) -> Result<()> {
        self.init(cx);
        let c = cx.content();
        let (open, total) = (self.open_count(), self.tasks.len());
        cx.text(Rect::new(c.x, c.y - 1.0, c.w - 26.0, 20.0), cx.t("Задачи", "Tasks"), Style::TITLE.ellipsis(), accent());
        cx.icon(Glyph::Plus, Tasks::plus(c).inset(2.0), accent());
        if total == 0 {
            let msg = if self.corrupt { cx.t("Не удалось прочитать файл", "Cannot read the file") } else { cx.t("Нет задач", "No tasks") };
            cx.icon(Glyph::CheckCircle, Rect::new(c.cx() - 18.0, c.y + 40.0, 36.0, 36.0), FAINT);
            cx.text(Rect::new(c.x, c.y + 84.0, c.w, 20.0), msg, Style::BODY.center().ellipsis(), DIM);
            return Ok(());
        }
        let list = shown(&self.tasks);
        match cx.size {
            Size::M => {
                cx.text(Rect::new(c.x + 62.0, c.y - 1.0, 70.0, 20.0), &format!("{open}/{total}"), Style::CAPTION.mono(), DIM);
                for (i, t) in list.iter().take(5).enumerate() {
                    let r = Tasks::row(c, i);
                    let ctr = (r.x + BOX_R + 1.0, r.cy());
                    if t.done {
                        cx.circle(ctr, BOX_R, accent());
                        cx.icon(Glyph::Check, Rect::new(ctr.0 - 5.0, ctr.1 - 5.0, 10.0, 10.0), WHITE);
                    } else {
                        cx.circle_stroke(ctr, BOX_R - 0.75, 1.5, DIM);
                    }
                    let tr = Rect::new(r.x + 24.0, r.y, r.w - 24.0, r.h);
                    cx.text(tr, &t.text, Style::BODY.ellipsis(), if t.done { DIM } else { TEXT });
                    if t.done {
                        let w = cx.measure(&t.text, Style::BODY, 0.0).0.min(tr.w);
                        cx.line((tr.x, r.cy() + 0.5), (tr.x + w, r.cy() + 0.5), 1.2, DIM);
                    }
                }
                if list.len() > 5 {
                    cx.text(Rect::new(c.right() - 60.0, c.bottom() - 12.0, 60.0, 12.0), &format!("+{}", list.len() - 5), Style::CAPTION.right().mono(), FAINT);
                }
            }
            _ => {
                let num = open.to_string();
                cx.text(Rect::new(c.x, c.y + 22.0, c.w, 44.0), &num, Style::new(38.0, 300).mono(), TEXT);
                let nw = cx.measure(&num, Style::new(38.0, 300).mono(), 0.0).0;
                let cap = if open == 0 { cx.t("всё сделано", "all done") } else { cx.t("осталось", "left") };
                cx.text(Rect::new(c.x + nw + 8.0, c.y + 44.0, (c.w - nw - 8.0).max(10.0), 16.0), cap, Style::CAPTION.ellipsis(), DIM);
                if let Some(t) = list.first().filter(|t| !t.done) {
                    let a = Tasks::next_area(c);
                    let mw = |s: &str| cx.measure(s, Style::BODY, 0.0).0;
                    for (k, line) in wrap_lines(&t.text, 3, a.w, &mw).iter().enumerate() {
                        cx.text(Rect::new(a.x, a.y + k as f32 * 17.0, a.w, 17.0), line, Style::BODY.ellipsis(), TEXT);
                    }
                }
            }
        }
        Ok(())
    }

    fn tick(&mut self, cx: &Cx) -> Tick {
        self.init(cx);
        if cx.preview {
            return Tick::Never;
        }
        let polled = self.ask.as_ref().map(|p| (p.take(), p.busy()));
        if let Some((res, busy)) = polled {
            match res {
                Some(r) => {
                    self.ask = None;
                    if let Some(s) = r {
                        let nm = now_ms(cx);
                        self.modify(|l| add_task(l, &s, nm));
                    }
                }
                None if !busy => self.ask = None,
                None => {}
            }
        }
        if store_path("tasks.json").is_some_and(|p| mtime(&p) != self.mtime) {
            self.reload();
        }
        Tick::Poll(Duration::from_secs(5))
    }

    fn mouse(&mut self, ev: &MouseEv, cx: &mut Cx) -> bool {
        if cx.preview || ev.kind != MouseKind::Up {
            return false;
        }
        let c = cx.content();
        if ev.button == Button::Left && ev.inside(Tasks::plus(c).inset(-4.0)) {
            self.ask_new(cx);
            return false;
        }
        if self.tasks.is_empty() {
            if ev.button == Button::Left && ev.inside(cx.bounds()) {
                self.ask_new(cx);
            }
            return false;
        }
        let Some(id) = self.hit(cx.size, c, ev.x, ev.y) else { return false };
        match ev.button {
            Button::Left => self.modify(|l| toggle_task(l, id)),
            Button::Right => self.modify(|l| delete_task(l, id)),
            _ => {}
        }
        true
    }

    fn version(&self) -> u64 {
        self.ver
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(id: u64, text: &str, done: bool) -> Task {
        Task { id, text: text.into(), done, created_ms: 0 }
    }

    #[test]
    fn add_toggle_delete() {
        let mut l = Vec::new();
        assert!(!add_task(&mut l, "   \n ", 1));
        assert!(add_task(&mut l, "  Buy   milk\r\n", 1));
        assert!(add_task(&mut l, "Call mom", 2));
        assert_eq!((l[0].id, l[1].id, l[0].text.as_str()), (1, 2, "Buy milk"));
        assert!(toggle_task(&mut l, 1) && l[0].done);
        assert!(toggle_task(&mut l, 1) && !l[0].done);
        assert!(!toggle_task(&mut l, 99));
        assert!(delete_task(&mut l, 1) && !delete_task(&mut l, 1));
        assert_eq!(l.len(), 1);
        assert!(add_task(&mut l, &"x".repeat(500), 3));
        assert_eq!(l[1].text.chars().count(), MAX_TEXT);
        let mut full: Vec<Task> = (0..MAX_TASKS as u64).map(|i| t(i, "a", false)).collect();
        assert!(!add_task(&mut full, "one more", 0));
    }

    #[test]
    fn open_tasks_first_in_added_order() {
        let l = vec![t(1, "a", true), t(2, "b", false), t(3, "c", false), t(4, "d", true)];
        assert_eq!(shown(&l).iter().map(|t| t.id).collect::<Vec<_>>(), vec![2, 3, 1, 4]);
    }

    #[test]
    fn json_round_trip_through_the_store() {
        let l = vec![t(1, "Отправить отчёт", false), t(7, "second", true)];
        let p = std::env::temp_dir().join(format!("mswidgets-tasks-test-{}.json", std::process::id()));
        save_json(&p, &tasks_to_json(&l)).unwrap();
        let Load::Ok(v) = load_json(&p) else { panic!("not loaded") };
        assert_eq!(tasks_from_json(&v), Some(l));
        let _ = std::fs::remove_file(&p);
        assert!(matches!(load_json(&p), Load::Missing));
    }

    #[test]
    fn garbage_is_tolerated() {
        assert_eq!(tasks_from_json(&json!({"a": 1})), None);
        let v = json!([{"id": 1, "text": "ok"}, {"id": "x", "text": "bad id"}, {"id": 2, "text": "   "}, {"text": "no id"}, 5]);
        assert_eq!(tasks_from_json(&v), Some(vec![t(1, "ok", false)]));
    }
}
