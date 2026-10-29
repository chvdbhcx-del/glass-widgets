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

// ================================================================ motion (pure plan)

/// Which task ids enter and which leave between two lists.
fn diff_ids(old: &[u64], new: &[u64]) -> (Vec<u64>, Vec<u64>) {
    (new.iter().filter(|i| !old.contains(i)).copied().collect(), old.iter().filter(|i| !new.contains(i)).copied().collect())
}

/// Row opacity at the (fractional) slot `y`: rows that glide past the last visible slot fade out.
fn edge_fade(y: f32, rows: usize) -> f32 {
    let last = rows as f32 - 1.0;
    if y <= last {
        1.0
    } else {
        1.0 - crate::motion::smooth((y - last) / 0.7)
    }
}

/// The id out of a glide key ("y:12" -> 12).
fn key_id(k: &str) -> Option<u64> {
    k.split_once(':')?.1.parse().ok()
}

// ================================================================ widget

/// A deleted task on its way out: it keeps its slot and fades while the rows below glide up.
struct Ghost {
    task: Task,
}

#[derive(Default)]
struct Tasks {
    tasks: Vec<Task>,
    mtime: Option<SystemTime>,
    corrupt: bool,
    started: bool,
    ver: u64,
    ask: Option<Pending<Option<String>>>,
    /// per task id: "y:" slot (a task changing place glides), "a:" opacity (enters / leaves), "k:" done progress 0..1 (the circle
    /// fills, the check draws itself, the line strikes the text), "s:" the S card's next-task fade
    slots: GlideAnim,
    /// "open" the count shown, "more" the opacity of the "+n" mark
    vals: GlideAnim,
    /// the order the glides were last aimed at (what a vanished task looked like is in `prev`)
    prev: Vec<Task>,
    ghosts: Vec<Ghost>,
    s_next: Option<u64>,
    /// a first order was set: from now on changes animate
    primed: bool,
    /// something is gliding: `draw` leaves the tasks out and `overlay` paints them (re-recorded when it starts and ends)
    moving: bool,
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
const ROWS: usize = 5;

impl Tasks {
    fn init(&mut self, cx: &Cx) {
        if self.started {
            return;
        }
        self.started = true;
        self.ver += 1;
        self.slots = GlideAnim::smooth(0.4);
        if cx.preview {
            self.tasks = sample();
        } else {
            self.reload(cx);
        }
        self.retarget(cx);
    }

    /// Aim every glide at the current list. The first call (and previews, reduced motion) jump; later calls animate: a new task enters,
    /// a deleted one stays as a fading ghost, the others glide to their new rank, a ticked one draws its check and strikes its text.
    fn retarget(&mut self, cx: &Cx) {
        let view: Vec<Task> = shown(&self.tasks).into_iter().cloned().collect();
        let (entering, leaving) = diff_ids(&self.prev.iter().map(|t| t.id).collect::<Vec<_>>(), &view.iter().map(|t| t.id).collect::<Vec<_>>());
        let anim = self.primed && !cx.preview && !reduced_motion();
        for id in &leaving {
            self.ghosts.retain(|g| g.task.id != *id);
            match self.prev.iter().find(|t| t.id == *id).filter(|_| anim) {
                Some(t) => {
                    self.ghosts.push(Ghost { task: t.clone() });
                    self.slots.set(&format!("a:{id}"), 0.0, cx);
                }
                None => {
                    for p in ["y", "a", "k", "s"] {
                        self.slots.forget(&format!("{p}:{id}"));
                    }
                }
            }
        }
        for (i, t) in view.iter().enumerate() {
            self.ghosts.retain(|g| g.task.id != t.id);
            let (y, a, k) = (format!("y:{}", t.id), format!("a:{}", t.id), format!("k:{}", t.id));
            if anim && entering.contains(&t.id) {
                self.slots.set_from(&y, i as f32 + 0.8, i as f32, cx);
                self.slots.set_from(&a, 0.0, 1.0, cx);
            } else {
                self.slots.set(&y, i as f32, cx);
                self.slots.set(&a, 1.0, cx);
            }
            self.slots.set(&k, if t.done { 1.0 } else { 0.0 }, cx);
        }
        let next = view.first().filter(|t| !t.done).map(|t| t.id);
        if next != self.s_next {
            if let Some(id) = next.filter(|_| anim) {
                self.slots.start(&format!("s:{id}"), 0.0, 1.0, cx);
            }
            self.s_next = next;
        }
        self.vals.set("open", self.open_count() as f32, cx);
        self.vals.set("more", if view.len() > ROWS { 1.0 } else { 0.0 }, cx);
        self.prev = view;
        self.primed = true;
        cx.waker().wake(); // a click does not call `tick`: poke it so the glides start now
    }

    fn reload(&mut self, cx: &Cx) {
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
            self.retarget(cx);
        }
    }

    /// Read-modify-write on the fresh file; a corrupt file is never overwritten.
    fn modify(&mut self, cx: &Cx, f: impl FnOnce(&mut Vec<Task>) -> bool) {
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
            self.retarget(cx);
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

    /// The rect of slot `y` (M, fractional while it glides) in card px.
    fn row_at(c: Rect, y: f32) -> Rect {
        Rect::new(c.x, c.y + 26.0 + y * ROW_H, c.w, ROW_H)
    }
    fn plus(c: Rect) -> Rect {
        Rect::new(c.right() - 20.0, c.y - 1.0, 20.0, 20.0)
    }
    /// S: the area of the next task.
    fn next_area(c: Rect) -> Rect {
        Rect::new(c.x, c.y + 70.0, c.w, c.h - 70.0)
    }
    /// The task under the pointer: M = the row it settles in, S = the next open task.
    fn hit(&self, size: Size, c: Rect, x: f32, y: f32) -> Option<u64> {
        let list = shown(&self.tasks);
        if size == Size::S {
            return Tasks::next_area(c).contains(x, y).then(|| list.first().filter(|t| !t.done).map(|t| t.id)).flatten();
        }
        list.iter().take(ROWS).enumerate().find(|(i, _)| Tasks::row_at(c, *i as f32).inset_xy(-8.0, 0.0).contains(x, y)).map(|(_, t)| t.id)
    }

    /// One M row at slot `y`: circle (fills, draws its check), text (fades to dim, struck by a growing line).
    fn paint_row(cx: &Cx, c: Rect, t: &Task, y: f32, a: f32, k: f32) {
        let a = a * edge_fade(y, ROWS);
        if a <= 0.0 {
            return;
        }
        let r = Tasks::row_at(c, y);
        cx.with_opacity(a, |cx| {
            let ctr = (r.x + BOX_R + 1.0, r.cy());
            cx.circle_stroke(ctr, BOX_R - 0.75, 1.5, DIM.fade(1.0 - k));
            if k > 0.0 {
                cx.circle(ctr, BOX_R * crate::motion::smooth((k * 1.6).min(1.0)), accent());
                let reveal = ((k - 0.35) / 0.65).clamp(0.0, 1.0); // the check is drawn left to right after the fill
                if reveal > 0.0 {
                    cx.clip(Rect::new(ctr.0 - 5.0, ctr.1 - 6.0, 10.0 * reveal, 12.0));
                    cx.icon(Glyph::Check, Rect::new(ctr.0 - 5.0, ctr.1 - 5.0, 10.0, 10.0), WHITE);
                    cx.pop_clip();
                }
            }
            let tr = Rect::new(r.x + 24.0, r.y, r.w - 24.0, r.h);
            cx.text(tr, &t.text, Style::BODY.ellipsis(), TEXT.mix(DIM, k));
            if k > 0.0 {
                let w = cx.measure(&t.text, Style::BODY, 0.0).0.min(tr.w);
                cx.line((tr.x, r.cy() + 0.5), (tr.x + w * k, r.cy() + 0.5), 1.2, DIM);
            }
        });
    }

    /// Everything that moves: the count, the rows, the S card's next task. `draw` calls it when nothing glides, `overlay` while something does.
    fn paint(&self, cx: &Cx) {
        let c = cx.content();
        let total = self.tasks.len();
        if total == 0 && self.ghosts.is_empty() {
            let msg = if self.corrupt { cx.t("Не удалось прочитать файл", "Cannot read the file") } else { cx.t("Нет задач", "No tasks") };
            cx.icon(Glyph::CheckCircle, Rect::new(c.cx() - 18.0, c.y + 40.0, 36.0, 36.0), FAINT);
            cx.text(Rect::new(c.x, c.y + 84.0, c.w, 20.0), msg, Style::BODY.center().ellipsis(), DIM);
            return;
        }
        let open = self.vals.or("open", self.open_count() as f32).round() as usize;
        match cx.size {
            Size::M => {
                cx.text(Rect::new(c.x + 62.0, c.y - 1.0, 70.0, 20.0), &format!("{open}/{total}"), Style::CAPTION.mono(), DIM);
                let view = shown(&self.tasks);
                for (i, t) in view.iter().enumerate() {
                    let g = |p: &str, d: f32| self.slots.or(&format!("{p}:{}", t.id), d);
                    Tasks::paint_row(cx, c, t, g("y", i as f32), g("a", 1.0), g("k", if t.done { 1.0 } else { 0.0 }));
                }
                for gh in &self.ghosts {
                    let g = |p: &str, d: f32| self.slots.or(&format!("{p}:{}", gh.task.id), d);
                    Tasks::paint_row(cx, c, &gh.task, g("y", 0.0), g("a", 0.0), g("k", if gh.task.done { 1.0 } else { 0.0 }));
                }
                let more = self.vals.or("more", 0.0);
                if more > 0.0 {
                    cx.text(Rect::new(c.right() - 60.0, c.bottom() - 12.0, 60.0, 12.0), &format!("+{}", total.saturating_sub(ROWS)), Style::CAPTION.right().mono(), FAINT.fade(more));
                }
            }
            _ => {
                let num = open.to_string();
                cx.text(Rect::new(c.x, c.y + 22.0, c.w, 44.0), &num, Style::new(38.0, 300).mono(), TEXT);
                let nw = cx.measure(&num, Style::new(38.0, 300).mono(), 0.0).0;
                let cap = if self.open_count() == 0 { cx.t("всё сделано", "all done") } else { cx.t("осталось", "left") };
                cx.text(Rect::new(c.x + nw + 8.0, c.y + 44.0, (c.w - nw - 8.0).max(10.0), 16.0), cap, Style::CAPTION.ellipsis(), DIM);
                if let Some(t) = shown(&self.tasks).first().filter(|t| !t.done) {
                    let a = Tasks::next_area(c);
                    let f = self.slots.or(&format!("s:{}", t.id), 1.0); // the new next task rises in
                    let mw = |s: &str| cx.measure(s, Style::BODY, 0.0).0;
                    cx.with_opacity(f, |cx| {
                        for (k, line) in wrap_lines(&t.text, 3, a.w, &mw).iter().enumerate() {
                            cx.text(Rect::new(a.x, a.y + k as f32 * 17.0 + (1.0 - f) * 8.0, a.w, 17.0), line, Style::BODY.ellipsis(), TEXT);
                        }
                    });
                }
            }
        }
    }
}

impl Widget for Tasks {
    fn draw(&mut self, cx: &mut Cx) -> Result<()> {
        self.init(cx);
        let c = cx.content();
        cx.text(Rect::new(c.x, c.y - 1.0, c.w - 26.0, 20.0), cx.t("Задачи", "Tasks"), Style::TITLE.ellipsis(), accent());
        cx.icon(Glyph::Plus, Tasks::plus(c).inset(2.0), accent());
        if !self.moving {
            self.paint(cx);
        }
        Ok(())
    }

    /// The gliding part: rows at their fractional slots, the check being drawn, the fading ghosts.
    fn overlay(&mut self, cx: &mut Cx) -> Result<()> {
        if self.moving {
            self.paint(cx);
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
                        self.modify(cx, |l| add_task(l, &s, nm));
                    }
                }
                None if !busy => self.ask = None,
                None => {}
            }
        }
        if store_path("tasks.json").is_some_and(|p| mtime(&p) != self.mtime) {
            self.reload(cx);
        }
        let busy = self.slots.step(cx) | self.vals.step(cx);
        if busy != self.moving {
            self.moving = busy;
            self.ver += 1; // `draw` takes the rows over again (or hands them to `overlay`)
        }
        if !busy && !self.ghosts.is_empty() {
            self.ghosts.clear();
            let ids: Vec<u64> = self.tasks.iter().map(|t| t.id).collect();
            self.slots.retain(|k| key_id(k).is_some_and(|i| ids.contains(&i)));
            self.ver += 1;
        }
        if busy {
            return Tick::Fps(60); // rows glide, the check draws itself, the count counts
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
            Button::Left => self.modify(cx, |l| toggle_task(l, id)),
            Button::Right => self.modify(cx, |l| delete_task(l, id)),
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

    #[test]
    fn motion_plan() {
        assert_eq!(diff_ids(&[1, 2, 3], &[2, 3, 4]), (vec![4], vec![1]));
        assert_eq!(diff_ids(&[], &[1]), (vec![1], vec![]));
        assert_eq!((edge_fade(0.0, 5), edge_fade(4.0, 5)), (1.0, 1.0));
        assert!(edge_fade(4.3, 5) < 1.0 && edge_fade(4.3, 5) > 0.0);
        assert_eq!(edge_fade(5.0, 5), 0.0);
        assert_eq!((key_id("y:12"), key_id("open"), key_id("a:x")), (Some(12), None, None));
    }

    #[test]
    fn ticked_task_glides_to_the_bottom_and_ends_exactly() {
        use crate::motion::Glide;
        let mut g = Glide::smooth(0.4);
        // [a, b, c] with a ticked: the order becomes b, c, a; the done progress runs 0 -> 1
        for (i, id) in [1, 2, 3].iter().enumerate() {
            g.set(&format!("y:{id}"), i as f32);
        }
        g.set("k:1", 0.0);
        g.set("y:1", 2.0);
        g.set("y:2", 0.0);
        g.set("y:3", 1.0);
        g.set("k:1", 1.0);
        let mut mid = None;
        let mut n = 0;
        while g.step(1.0 / 60.0) {
            n += 1;
            if n == 12 {
                mid = g.get("y:1");
            }
            assert!(g.get("y:1").unwrap() >= 0.0 && g.get("y:1").unwrap() <= 2.0, "no overshoot");
            assert!(n < 120);
        }
        assert!(mid.unwrap() > 0.0 && mid.unwrap() < 2.0, "it was gliding at 0.2 s");
        assert_eq!((g.get("y:1"), g.get("y:2"), g.get("y:3"), g.get("k:1")), (Some(2.0), Some(0.0), Some(1.0), Some(1.0)));
        // a task that enters: from below and transparent, at its slot and opaque in the end
        g.set_from("y:4", 3.8, 3.0);
        g.set_from("a:4", 0.0, 1.0);
        assert_eq!((g.get("y:4"), g.get("a:4")), (Some(3.8), Some(0.0)));
        while g.step(1.0 / 60.0) {}
        assert_eq!((g.get("y:4"), g.get("a:4")), (Some(3.0), Some(1.0)));
    }
}
