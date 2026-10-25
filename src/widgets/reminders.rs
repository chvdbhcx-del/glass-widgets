//! Reminders: S 3 rows (due label under the text), M 4 rows (due label at the right), L 7 rows + a "+ Новое напоминание" row.
//! Port of src/widgets/reminders.js + src-tauri/src/reminders.rs (same storage, same date parser).
//!
//! Storage: `%APPDATA%\macshell\reminders.json` = `[{"id":u64,"text":str,"done":bool,"due_ms":i64|null,"created_ms":i64}]`, exactly the
//! schema the Tauri app writes (unix ms). Every change is read-modify-write on the FRESH file (so edits made by the Tauri app are never
//! lost), written atomically (temp file + rename); a corrupt file is never overwritten. The file's mtime is checked every 5 s.
//! Interaction: click the circle = complete (fill, strike, fade via the overlay, then persisted), click the text = edit (popup),
//! click "+" (L: the bottom row, M: the plus in the header, S: the title) = new reminder (popup, the typed text may contain
//! "завтра в 18:30"), right click on a row = delete.
//! Also hosts the tiny JSON-store helpers (`store_path`, `load_json`, `save_json`, `mtime`) shared with notes.rs.
use crate::registry::{Registry, WidgetDef};
use crate::widget::*;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime};

pub fn register(r: &mut Registry) {
    r.register(WidgetDef {
        type_id: "reminders",
        name: ("Напоминания", "Reminders"),
        desc: ("Список дел и задач на сегодня", "Your tasks and to-dos"),
        group: "reminders",
        icon: Glyph::CheckCircle,
        sizes: &[Size::S, Size::M, Size::L],
        default_size: Size::M,
        make: || Box::new(Reminders::default()),
    });
}

// ================================================================ shared tiny JSON store (also used by notes.rs)

pub(crate) enum Load {
    Missing,
    Ok(Value),
    /// unreadable or not JSON: the caller must never overwrite the file
    Corrupt,
}

/// `%APPDATA%\macshell\<name>`
pub(crate) fn store_path(name: &str) -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|a| PathBuf::from(a).join("macshell").join(name))
}

pub(crate) fn load_json(p: &Path) -> Load {
    match std::fs::read_to_string(p) {
        Ok(s) => match serde_json::from_str::<Value>(s.trim_start_matches('\u{feff}')) {
            Ok(v) => Load::Ok(v),
            Err(_) => Load::Corrupt,
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Load::Missing,
        Err(_) => Load::Corrupt,
    }
}

/// Atomic write: sibling temp file, then rename over the target.
pub(crate) fn save_json(p: &Path, v: &Value) -> std::io::Result<()> {
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d)?;
    }
    let body = serde_json::to_string_pretty(v).map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;
    let tmp = p.with_extension("json.mswtmp");
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, p).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

pub(crate) fn mtime(p: &Path) -> Option<SystemTime> {
    std::fs::metadata(p).and_then(|m| m.modified()).ok()
}

// ================================================================ time helpers (pure)

/// Local civil time -> unix ms, using the given UTC offset (minutes).
pub(crate) fn local_to_ms(y: i32, mo: u32, d: u32, h: u32, mi: u32, off_min: i32) -> i64 {
    (tz::days_from_civil(y, mo, d) * 86400 + (h * 3600 + mi * 60) as i64 - off_min as i64 * 60) * 1000
}

fn day_plus(y: i32, mo: u32, d: u32, k: i64) -> (i32, u32, u32) {
    tz::civil_from_days(tz::days_from_civil(y, mo, d) + k)
}

pub(crate) fn now_ms(cx: &Cx) -> i64 {
    cx.utc_secs() * 1000 + cx.now.ms as i64
}

// ================================================================ the due-date parser (port of parseDue in reminders.js)

fn is_word(c: char) -> bool {
    c.is_alphabetic() || c.is_ascii_digit()
}
fn low(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}
fn starts_at(cs: &[char], i: usize, w: &str) -> bool {
    w.chars().enumerate().all(|(k, wc)| cs.get(i + k).map(|&c| low(c)) == Some(wc))
}
const DAY_WORDS: [(&str, i64); 5] = [("сегодня", 0), ("завтра", 1), ("послезавтра", 2), ("today", 0), ("tomorrow", 1)];

/// leftmost whole-word day keyword: (start, end, day offset)
fn find_day(cs: &[char]) -> Option<(usize, usize, i64)> {
    for i in 0..cs.len() {
        if i > 0 && is_word(cs[i - 1]) {
            continue;
        }
        for (w, off) in DAY_WORDS {
            let e = i + w.chars().count();
            if starts_at(cs, i, w) && cs.get(e).map_or(true, |&c| !is_word(c)) {
                return Some((i, e, off));
            }
        }
    }
    None
}
/// after "в " / "at " (one keyword + at least one whitespace): index of the first char after the blanks
fn skip_prefix(cs: &[char], i: usize) -> Option<usize> {
    let n = if starts_at(cs, i, "в") {
        1
    } else if starts_at(cs, i, "at") {
        2
    } else {
        return None;
    };
    let mut j = i + n;
    while cs.get(j).is_some_and(|c| c.is_whitespace()) {
        j += 1;
    }
    (j > i + n).then_some(j)
}
/// a run of exactly 1..=2 ASCII digits starting at `p`: (value, end)
fn digits12(cs: &[char], p: usize) -> Option<(u32, usize)> {
    let mut j = p;
    while cs.get(j).is_some_and(|c| c.is_ascii_digit()) {
        j += 1;
    }
    if !(1..=2).contains(&(j - p)) {
        return None;
    }
    Some((cs[p..j].iter().collect::<String>().parse().ok()?, j))
}
/// `(?:(?:в|at)\s+)?(\d{1,2}):(\d{2})` not followed by a letter, digit or colon
fn colon_at(cs: &[char], i: usize) -> Option<(usize, u32, u32)> {
    for p in [skip_prefix(cs, i), Some(i)].into_iter().flatten() {
        let Some((h, j)) = digits12(cs, p) else { continue };
        if cs.get(j) != Some(&':') {
            continue;
        }
        let (a, b) = (cs.get(j + 1), cs.get(j + 2));
        if let (Some(a), Some(b)) = (a, b) {
            if a.is_ascii_digit() && b.is_ascii_digit() {
                let e = j + 3;
                if cs.get(e).map_or(true, |&c| !is_word(c) && c != ':') {
                    return Some((e, h, a.to_digit(10)? * 10 + b.to_digit(10)?));
                }
            }
        }
    }
    None
}
/// `(?:в|at)\s+(\d{1,2})` not followed by a letter, digit or colon
fn hour_at(cs: &[char], i: usize) -> Option<(usize, u32, u32)> {
    let (h, j) = digits12(cs, skip_prefix(cs, i)?)?;
    cs.get(j).map_or(true, |&c| !is_word(c) && c != ':').then_some((j, h, 0))
}
/// First match of `f` (lookbehind: no letter/digit before); replaced by a blank when the time is valid. Returns the (h, m) found.
fn take_time(cs: &mut Vec<char>, f: fn(&[char], usize) -> Option<(usize, u32, u32)>) -> Option<(u32, u32)> {
    for i in 0..cs.len() {
        if i > 0 && is_word(cs[i - 1]) {
            continue;
        }
        if let Some((e, h, m)) = f(cs, i) {
            if h > 23 || m > 59 {
                return None; // like the JS: the first match decides, an invalid time stays in the text
            }
            cs.splice(i..e, [' ']);
            return Some((h, m));
        }
    }
    None
}

/// "<text> завтра в 18:30" -> (text, due unix ms). A due at exactly local 00:00 means "all day".
/// Recognised: сегодня / завтра / послезавтра (today / tomorrow), "в 18:30", "в 9", "at 7:05", a bare "18:30". A time that has already
/// passed today means tomorrow. `now` is local time, `off_min` the UTC offset (minutes).
pub(crate) fn parse_due(input: &str, now: &Now, off_min: i32) -> (String, Option<i64>) {
    let orig = input.trim().to_string();
    let mut cs: Vec<char> = orig.chars().collect();
    let mut off = None;
    if let Some((s, e, o)) = find_day(&cs) {
        cs.splice(s..e, [' ']);
        off = Some(o);
    }
    // ponytail: "в 5 яблок" is read as 05:00 (same as the JS version); add a stop-word list if that ever annoys
    let hm = take_time(&mut cs, colon_at).or_else(|| take_time(&mut cs, hour_at));
    // collapse blank runs, strip leading `[\s,.;:-]` and trailing `[\s,;:-]`
    let mut text = String::new();
    let mut blanks = 0;
    for &c in &cs {
        if c.is_whitespace() {
            blanks += 1;
            continue;
        }
        if blanks > 0 {
            text.push(' ');
            blanks = 0;
        }
        text.push(c);
    }
    if blanks > 0 {
        text.push(' ');
    }
    let text = text.trim_start_matches(|c: char| c.is_whitespace() || ",.;:-".contains(c)).trim_end_matches(|c: char| c.is_whitespace() || ",;:-".contains(c)).to_string();
    if off.is_none() && hm.is_none() {
        return (orig, None);
    }
    let text = if text.is_empty() { orig } else { text };
    let (h, m) = hm.unwrap_or((0, 0));
    let k = off.unwrap_or(if h * 60 + m <= now.h * 60 + now.mi { 1 } else { 0 });
    let (y, mo, d) = day_plus(now.y, now.mo, now.d, k);
    (text, Some(local_to_ms(y, mo, d, h, m, off_min)))
}

const MON_RU: [&str; 12] = ["янв", "фев", "мар", "апр", "мая", "июн", "июл", "авг", "сен", "окт", "ноя", "дек"];
const MON_EN: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

/// (label, overdue): "Сегодня" / "18:30" / "Завтра, 09:00" / "22 окт"; an all-day due (local 00:00) shows no time and is overdue only after its day.
pub(crate) fn due_label(due_ms: i64, now_ms: i64, off_min: i32, lang: Lang) -> (String, bool) {
    let d = tz::civil_from_unix(due_ms.div_euclid(1000), off_min);
    let n = tz::civil_from_unix(now_ms.div_euclid(1000), off_min);
    let diff = tz::days_from_civil(d.y, d.mo, d.d) - tz::days_from_civil(n.y, n.mo, n.d);
    let all_day = d.h == 0 && d.mi == 0 && d.sec == 0;
    let day = match diff {
        0 => lang.pick("Сегодня", "Today").to_string(),
        1 => lang.pick("Завтра", "Tomorrow").to_string(),
        -1 => lang.pick("Вчера", "Yesterday").to_string(),
        _ => format!("{} {}", d.d, lang.pick(MON_RU[(d.mo as usize - 1) % 12], MON_EN[(d.mo as usize - 1) % 12])),
    };
    let hm = format!("{:02}:{:02}", d.h, d.mi);
    if all_day {
        (day, diff < 0)
    } else {
        (if diff == 0 { hm } else { format!("{day}, {hm}") }, due_ms < now_ms)
    }
}

/// Greedy word wrap into at most `max_lines` lines of width `max_w` (`mw` measures a string); the last line gets an ellipsis when
/// text was cut; a word wider than a line is split by characters.
pub(crate) fn wrap_lines(text: &str, max_lines: usize, max_w: f32, mw: &dyn Fn(&str) -> f32) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    for w in text.split_whitespace() {
        let mut word = w.to_string();
        loop {
            let cand = if cur.is_empty() { word.clone() } else { format!("{cur} {word}") };
            if mw(&cand) <= max_w {
                cur = cand;
                break;
            }
            if !cur.is_empty() {
                lines.push(std::mem::take(&mut cur));
                continue;
            }
            let chars: Vec<char> = word.chars().collect();
            let mut k = chars.len();
            while k > 1 && mw(&chars[..k].iter().collect::<String>()) > max_w {
                k -= 1;
            }
            if k >= chars.len() {
                cur = word;
                break;
            }
            lines.push(chars[..k].iter().collect());
            word = chars[k..].iter().collect();
        }
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    if lines.len() > max_lines.max(1) {
        lines.truncate(max_lines.max(1));
        let mut l = lines.pop().unwrap_or_default();
        loop {
            let t = format!("{}…", l.trim_end());
            if mw(&t) <= max_w || l.chars().count() <= 1 {
                lines.push(t);
                break;
            }
            l.pop();
        }
    }
    lines
}

// ================================================================ the store (pure list operations)

const MAX_ITEMS: usize = 500;
const MAX_TEXT: usize = 500;

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Item {
    pub id: u64,
    pub text: String,
    pub done: bool,
    pub due_ms: Option<i64>,
    pub created_ms: i64,
}

fn items_from_json(v: &Value) -> Option<Vec<Item>> {
    let a = v.as_array()?;
    Some(
        a.iter()
            .filter_map(|e| {
                Some(Item {
                    id: e.get("id")?.as_u64()?,
                    text: e.get("text")?.as_str()?.to_string(),
                    done: e.get("done").and_then(Value::as_bool).unwrap_or(false),
                    due_ms: e.get("due_ms").and_then(Value::as_i64),
                    created_ms: e.get("created_ms").and_then(Value::as_i64).unwrap_or(0),
                })
            })
            .collect(),
    )
}
fn items_to_json(l: &[Item]) -> Value {
    Value::Array(l.iter().map(|r| json!({"id": r.id, "text": r.text, "done": r.done, "due_ms": r.due_ms, "created_ms": r.created_ms})).collect())
}

fn add_item(l: &mut Vec<Item>, text: &str, due_ms: Option<i64>, now_ms: i64) -> bool {
    let text: String = text.trim().chars().take(MAX_TEXT).collect();
    if text.is_empty() {
        return false;
    }
    if l.len() >= MAX_ITEMS {
        // make room by dropping the oldest completed item; open ones are never discarded
        match l.iter().position(|r| r.done) {
            Some(i) => {
                l.remove(i);
            }
            None => return false,
        }
    }
    let id = l.iter().map(|r| r.id).max().unwrap_or(0) + 1;
    l.push(Item { id, text, done: false, due_ms, created_ms: now_ms });
    true
}
fn set_done(l: &mut [Item], id: u64, done: bool) -> bool {
    l.iter_mut().find(|r| r.id == id).map(|r| r.done = done).is_some()
}
fn delete_item(l: &mut Vec<Item>, id: u64) -> bool {
    let n = l.len();
    l.retain(|r| r.id != id);
    l.len() != n
}
fn edit_item(l: &mut [Item], id: u64, text: &str, due: Option<i64>) -> bool {
    let text: String = text.trim().chars().take(MAX_TEXT).collect();
    match l.iter_mut().find(|r| r.id == id) {
        Some(r) if !text.is_empty() && (r.text != text || due.is_some_and(|d| r.due_ms != Some(d))) => {
            r.text = text;
            if due.is_some() {
                r.due_ms = due;
            }
            true
        }
        _ => false,
    }
}

// ================================================================ layout

#[derive(Clone, Copy)]
struct Lay {
    head_h: f32,
    rows: usize,
    row_h: f32,
    lines: usize,
    inline_due: bool,
    sep: bool,
    /// a title that does not fit one line steps down 13 -> 12 px, then takes two 11 px lines (the row is tall enough)
    squeeze: bool,
}
impl Lay {
    fn of(s: Size) -> Lay {
        match s {
            Size::S => Lay { head_h: 28.0, rows: 3, row_h: 36.0, lines: 2, inline_due: false, sep: false, squeeze: false },
            Size::M => Lay { head_h: 28.0, rows: 4, row_h: 27.5, lines: 1, inline_due: true, sep: false, squeeze: true },
            Size::L => Lay { head_h: 36.0, rows: 7, row_h: 36.0, lines: 2, inline_due: true, sep: true, squeeze: false },
        }
    }
    fn list(&self, c: Rect) -> Rect {
        Rect::new(c.x, c.y + self.head_h, c.w, self.rows as f32 * self.row_h)
    }
    fn count_rect(c: Rect) -> Rect {
        Rect::new(c.right() - 44.0, c.y - 3.0, 44.0, 34.0)
    }
    /// where a click starts a NEW reminder: S the title, M the plus left of the count, L the bottom row
    fn add_hit(s: Size, c: Rect) -> Rect {
        match s {
            Size::S => Rect::new(c.x, c.y, c.w - 44.0, 24.0),
            Size::M => Rect::new(c.right() - 44.0 - 30.0, c.y - 3.0, 30.0, 30.0),
            Size::L => Rect::new(c.x, c.bottom() - 36.0, c.w, 36.0),
        }
    }
}

/// Everything needed to paint one row (computed once, reused by the overlay while the row completes).
struct RowGeo {
    c: Pt,
    lines: Vec<(String, Rect, f32)>,
    /// (label, overdue, rect)
    due: Option<(String, bool, Rect)>,
    inline: bool,
    /// title font size (13, or less when a squeezed row had to shrink)
    size: f32,
}

const CIRCLE: Color = Color::rgba(1.0, 1.0, 1.0, 0.6);
const CIRCLE_R: f32 = 5.6;

fn row_geo(cx: &Cx, lay: &Lay, row: Rect, text: &str, due: Option<(String, bool)>) -> RowGeo {
    let tx = row.x + 22.0;
    let due_w = match (&due, lay.inline_due) {
        (Some((s, _)), true) => (cx.measure(s, Style::CAPTION, 0.0).0 + 6.0).min(120.0),
        _ => 0.0,
    };
    let tw = (row.w - 22.0 - due_w - if due_w > 0.0 { 6.0 } else { 0.0 }).max(10.0);
    let stacked = due.is_some() && !lay.inline_due;
    let mut size = 13.0;
    let mut lines = wrap_lines(text, if stacked { 1 } else { lay.lines }, tw, &|s| cx.measure(s, Style::BODY, 0.0).0);
    if lay.squeeze && !stacked {
        let one = |z: f32| cx.measure(text, Style::BODY.size(z), 0.0).0 <= tw;
        if one(13.0) {
        } else if one(12.0) {
            size = 12.0;
            lines = vec![text.to_string()];
        } else {
            size = 11.0;
            lines = wrap_lines(text, 2, tw, &|s| cx.measure(s, Style::BODY.size(11.0), 0.0).0);
        }
    }
    let (lh, dh) = (if size < 11.5 && lines.len() > 1 { 12.5 } else { 16.0 }, if stacked { 14.0 } else { 0.0 });
    let y0 = row.cy() - (lines.len() as f32 * lh + dh) / 2.0;
    let n = lines.len() as f32;
    let rects: Vec<(String, Rect, f32)> = lines
        .into_iter()
        .enumerate()
        .map(|(k, s)| {
            let w = cx.measure(&s, Style::BODY.size(size), 0.0).0;
            (s, Rect::new(tx, y0 + k as f32 * lh, tw, lh), w)
        })
        .collect();
    let due = due.map(|(s, over)| {
        let r = if lay.inline_due { Rect::new(row.right() - due_w, row.y, due_w, row.h) } else { Rect::new(tx, y0 + n * lh, tw, dh) };
        (s, over, r)
    });
    RowGeo { c: (row.x + 6.0, y0 + lh / 2.0), lines: rects, due, inline: lay.inline_due, size }
}

fn due_style(inline: bool, over: bool) -> Style {
    let s = if over { Style::CAPTION.weight(600) } else { Style::CAPTION };
    if inline {
        s.right().ellipsis()
    } else {
        s.ellipsis()
    }
}

// ================================================================ the widget

const FILL_MS: f32 = 160.0;
const STRIKE_MS: f32 = 260.0;
const FADE_FROM_MS: f32 = 600.0;
const GONE_MS: f32 = 880.0;

struct Comp {
    id: u64,
    t0: Instant,
    geo: RowGeo,
}
#[derive(Clone, Copy)]
enum Ask {
    New,
    Edit(u64),
}

#[derive(Default)]
struct Reminders {
    items: Vec<Item>,
    mtime: Option<SystemTime>,
    corrupt: bool,
    started: bool,
    ver: u64,
    min: u64,
    comps: Vec<Comp>,
    ask: Option<(Pending<Option<String>>, Ask)>,
}

fn sample(now: &Now, off: i32) -> Vec<Item> {
    let at = |k: i64, h: u32, m: u32| {
        let (y, mo, d) = day_plus(now.y, now.mo, now.d, k);
        Some(local_to_ms(y, mo, d, h, m, off))
    };
    let mk = |id: u64, t: &str, due: Option<i64>| Item { id, text: t.into(), done: false, due_ms: due, created_ms: id as i64 };
    vec![
        mk(1, "Отправить отчёт", at(0, 10, 0)),
        mk(2, "Позвонить маме", at(0, 18, 30)),
        mk(3, "Купить билеты", at(1, 0, 0)),
        mk(4, "Забрать посылку в пункте выдачи на Ленина", at(2, 12, 0)),
        mk(5, "Записаться к врачу", at(3, 0, 0)),
        mk(6, "Оплатить интернет", None),
        mk(7, "Прибраться на столе", None),
    ]
}

fn ease(x: f32) -> f32 {
    let x = x.clamp(0.0, 1.0);
    x * x * (3.0 - 2.0 * x)
}

impl Reminders {
    fn init(&mut self, cx: &Cx) {
        if self.started {
            return;
        }
        self.started = true;
        self.ver += 1;
        if cx.preview {
            self.items = sample(&cx.now, cx.utc_offset_min());
        } else {
            self.reload();
        }
    }

    fn reload(&mut self) {
        let Some(p) = store_path("reminders.json") else { return };
        let m = mtime(&p);
        let (list, corrupt) = match load_json(&p) {
            Load::Missing => (Vec::new(), false),
            Load::Ok(v) => match items_from_json(&v) {
                Some(l) => (l, false),
                None => (Vec::new(), true),
            },
            Load::Corrupt => (Vec::new(), true),
        };
        self.mtime = m;
        if corrupt != self.corrupt || (!corrupt && list != self.items) || (corrupt && !self.items.is_empty()) {
            self.items = list;
            self.corrupt = corrupt;
            self.ver += 1;
        }
    }

    /// Read-modify-write on the fresh file. `f` returns true when it changed the list.
    fn modify(&mut self, f: impl FnOnce(&mut Vec<Item>) -> bool) {
        let Some(p) = store_path("reminders.json") else { return };
        let mut list = match load_json(&p) {
            Load::Missing => Vec::new(),
            Load::Ok(v) => match items_from_json(&v) {
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
            let _ = save_json(&p, &items_to_json(&list)); // on failure the in-memory list still shows the change until the next reload
            self.items = list;
            self.mtime = mtime(&p);
            self.corrupt = false;
            self.ver += 1;
        }
    }

    /// open items, soonest due first (no due last), at most `n`
    fn visible(&self, n: usize) -> Vec<&Item> {
        let mut v: Vec<&Item> = self.items.iter().filter(|r| !r.done).collect();
        v.sort_by_key(|r| (r.due_ms.unwrap_or(i64::MAX), r.id));
        v.truncate(n);
        v
    }

    fn ask_text(&mut self, cx: &Cx, title: &str, initial: &str, a: Ask) {
        if self.ask.is_none() {
            let ok = if matches!(a, Ask::New) { cx.t("Добавить", "Add") } else { cx.t("Сохранить", "Save") };
            self.ask = Some((cx.prompt_text_ex(title, initial, false, cx.t("Что напомнить?", "Remind me to…"), ok), a));
        }
    }

    fn apply_ask(&mut self, a: Ask, s: &str, now: &Now, off: i32, now_ms: i64) {
        let (text, due) = parse_due(s, now, off);
        match a {
            Ask::New => self.modify(|l| add_item(l, &text, due, now_ms)),
            Ask::Edit(id) => self.modify(|l| edit_item(l, id, &text, due)),
        }
    }
}

impl Widget for Reminders {
    fn draw(&mut self, cx: &mut Cx) -> Result<()> {
        self.init(cx);
        let (lay, c) = (Lay::of(cx.size), cx.content());
        let (nm, off) = (now_ms(cx), cx.utc_offset_min());
        let open = self.items.iter().filter(|r| !r.done).count().saturating_sub(self.comps.len());
        cx.text(Rect::new(c.x, c.y, c.w - 44.0, 22.0), cx.t("Напоминания", "Reminders"), Style::TITLE.ellipsis(), ACCENT);
        let big = if cx.size == Size::L { 34.0 } else { 30.0 };
        cx.text(Lay::count_rect(c), &open.to_string(), Style::new(big, 300).mono().right(), TEXT);
        if cx.size == Size::M {
            cx.icon(Glyph::Plus, Rect::new(c.right() - 44.0 - 24.0, c.y - 1.0, 22.0, 22.0), ACCENT);
        }
        let list = lay.list(c);
        let vis = self.visible(lay.rows);
        if vis.is_empty() {
            let msg = if self.corrupt { cx.t("Не удалось прочитать файл", "Cannot read the file") } else { cx.t("Нет напоминаний", "No reminders") };
            cx.text(list.inset_xy(0.0, 8.0), msg, Style::BODY.center(), DIM);
        }
        for (i, it) in vis.iter().enumerate() {
            let row = Rect::new(list.x, list.y + i as f32 * lay.row_h, list.w, lay.row_h);
            if lay.sep && i + 1 < vis.len() {
                cx.rect(Rect::new(row.x + 22.0, row.bottom() - 0.3, row.w - 22.0, 0.6), FILL);
            }
            if self.comps.iter().any(|k| k.id == it.id) {
                continue; // painted by the overlay while it completes
            }
            let g = row_geo(cx, &lay, row, &it.text, it.due_ms.map(|d| due_label(d, nm, off, cx.lang)));
            cx.circle_stroke(g.c, CIRCLE_R, 1.5, CIRCLE);
            for (s, r, _) in &g.lines {
                cx.text(*r, s, Style::BODY.size(g.size).ellipsis(), TEXT);
            }
            if let Some((s, over, r)) = &g.due {
                cx.text(*r, s, due_style(g.inline, *over), if *over { RED } else { DIM });
            }
        }
        if cx.size == Size::L {
            let a = Lay::add_hit(Size::L, c);
            cx.rect(Rect::new(a.x, a.y, a.w, 0.6), FILL);
            cx.icon(Glyph::Plus, Rect::new(a.x - 3.0, a.cy() - 9.0, 18.0, 18.0), ACCENT);
            cx.text(Rect::new(a.x + 22.0, a.y, a.w - 22.0, a.h), cx.t("Новое напоминание", "New reminder"), Style::BODY, ACCENT);
        }
        Ok(())
    }

    /// Completing rows: circle fills, check appears, text is struck, the whole row fades out.
    fn overlay(&mut self, cx: &mut Cx) -> Result<()> {
        for k in &self.comps {
            let t = k.t0.elapsed().as_secs_f32() * 1000.0;
            let fade = if t < FADE_FROM_MS { 1.0 } else { (1.0 - (t - FADE_FROM_MS) / (GONE_MS - FADE_FROM_MS)).clamp(0.0, 1.0) };
            let (fill, sp) = (ease(t / FILL_MS), ease(t / STRIKE_MS));
            let g = &k.geo;
            cx.circle_stroke(g.c, CIRCLE_R, 1.5, CIRCLE.fade(fade * (1.0 - fill)));
            if fill > 0.0 {
                cx.circle(g.c, CIRCLE_R * fill, ACCENT.fade(fade));
                cx.icon(Glyph::Check, Rect::new(g.c.0 - 6.5, g.c.1 - 6.5, 13.0, 13.0), WHITE.fade(fade * fill));
            }
            for (s, r, w) in &g.lines {
                cx.text(*r, s, Style::BODY.size(g.size).ellipsis(), TEXT.fade(fade * (1.0 - 0.45 * sp)));
                cx.line((r.x, r.cy() + 0.5), (r.x + w * sp, r.cy() + 0.5), 1.3, DIM.fade(fade));
            }
            if let Some((s, over, r)) = &g.due {
                cx.text(*r, s, due_style(g.inline, *over), (if *over { RED } else { DIM }).fade(fade * 0.7));
            }
        }
        Ok(())
    }

    fn tick(&mut self, cx: &Cx) -> Tick {
        self.init(cx);
        if !cx.preview {
            let (nm, off) = (now_ms(cx), cx.utc_offset_min());
            // answer of the text popup
            let polled = self.ask.as_ref().map(|(p, a)| (p.take(), p.busy(), *a));
            if let Some((res, busy, a)) = polled {
                match res {
                    Some(r) => {
                        self.ask = None;
                        if let Some(s) = r {
                            self.apply_ask(a, &s, &cx.now, off, nm);
                        }
                    }
                    None if !busy => self.ask = None,
                    None => {}
                }
            }
            // finished completions are persisted and leave the list
            let gone: Vec<u64> = self.comps.iter().filter(|k| k.t0.elapsed().as_secs_f32() * 1000.0 >= GONE_MS).map(|k| k.id).collect();
            if !gone.is_empty() {
                self.comps.retain(|k| !gone.contains(&k.id));
                self.modify(|l| gone.iter().fold(false, |ch, id| set_done(l, *id, true) | ch));
                self.ver += 1;
            }
            // edits made elsewhere (the Tauri app, another card)
            if self.comps.is_empty() && store_path("reminders.json").is_some_and(|p| mtime(&p) != self.mtime) {
                self.reload();
            }
        }
        let mk = cx.now.minute_key(); // due labels / "overdue" change with the clock
        if mk != self.min {
            self.min = mk;
            self.ver += 1;
        }
        if !self.comps.is_empty() {
            Tick::Fps(30)
        } else if cx.preview {
            Tick::Never
        } else {
            Tick::Poll(Duration::from_secs(5))
        }
    }

    fn mouse(&mut self, ev: &MouseEv, cx: &mut Cx) -> bool {
        if cx.preview || ev.kind != MouseKind::Up || !matches!(ev.button, Button::Left | Button::Right) {
            return false;
        }
        let (lay, c) = (Lay::of(cx.size), cx.content());
        let (nm, off) = (now_ms(cx), cx.utc_offset_min());
        if ev.button == Button::Left && ev.inside(Lay::add_hit(cx.size, c)) {
            self.ask_text(cx, cx.t("Новое напоминание", "New reminder"), "", Ask::New);
            return false;
        }
        let list = lay.list(c);
        let vis: Vec<(u64, String, Option<i64>)> = self.visible(lay.rows).iter().map(|r| (r.id, r.text.clone(), r.due_ms)).collect();
        if vis.is_empty() && ev.button == Button::Left && ev.inside(list) {
            self.ask_text(cx, cx.t("Новое напоминание", "New reminder"), "", Ask::New);
            return false;
        }
        for (i, (id, text, due)) in vis.iter().enumerate() {
            let row = Rect::new(list.x, list.y + i as f32 * lay.row_h, list.w, lay.row_h);
            if !ev.inside(row) {
                continue;
            }
            if self.comps.iter().any(|k| k.id == *id) {
                return false;
            }
            if ev.button == Button::Right {
                let id = *id;
                self.modify(|l| delete_item(l, id));
                return true;
            }
            if ev.inside(Rect::new(row.x - 4.0, row.y, 30.0, row.h)) {
                let geo = row_geo(cx, &lay, row, text, due.map(|d| due_label(d, nm, off, cx.lang)));
                self.comps.push(Comp { id: *id, t0: Instant::now(), geo });
                self.ver += 1;
                let _ = cx.spawn(|| ()); // pokes tick() so the 30 fps animation schedule starts now (mouse events do not call tick)
                return true;
            }
            self.ask_text(cx, cx.t("Напоминание", "Reminder"), text, Ask::Edit(*id));
            return false;
        }
        false
    }

    fn version(&self) -> u64 {
        self.ver
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> Now {
        Now { y: 2026, mo: 10, d: 24, h: 14, mi: 0, sec: 0, ms: 0 } // Sat 24 Oct 2026 14:00
    }
    const OFF: i32 = 180;
    fn ms(d: i64, h: u32, m: u32) -> i64 {
        let (y, mo, dd) = day_plus(2026, 10, 24, d);
        local_to_ms(y, mo, dd, h, m, OFF)
    }
    fn p(s: &str) -> (String, Option<i64>) {
        parse_due(s, &now(), OFF)
    }

    #[test]
    fn parser_matches_the_js_tests() {
        assert_eq!(p("Купить молоко"), ("Купить молоко".into(), None));
        assert_eq!(p("Купить молоко завтра"), ("Купить молоко".into(), Some(ms(1, 0, 0))));
        assert_eq!(p("завтра в 18:30 позвонить"), ("позвонить".into(), Some(ms(1, 18, 30))));
        assert_eq!(p("Сегодня отчёт"), ("отчёт".into(), Some(ms(0, 0, 0))));
        assert_eq!(p("отчёт послезавтра"), ("отчёт".into(), Some(ms(2, 0, 0))));
        assert_eq!(p("созвон в 18:30"), ("созвон".into(), Some(ms(0, 18, 30))));
        assert_eq!(p("зарядка в 9"), ("зарядка".into(), Some(ms(1, 9, 0)))); // already past -> tomorrow
        assert_eq!(p("call mom tomorrow at 7:05"), ("call mom".into(), Some(ms(1, 7, 5))));
        assert_eq!(p("lunch 12:15"), ("lunch".into(), Some(ms(1, 12, 15))));
        assert_eq!(p("сегодня в 23:59 сон"), ("сон".into(), Some(ms(0, 23, 59))));
        assert_eq!(p("матч в 25:70"), ("матч в 25:70".into(), None));
        assert_eq!(p("Позавтракать"), ("Позавтракать".into(), None));
        assert_eq!(p("завтра"), ("завтра".into(), Some(ms(1, 0, 0))));
        assert_eq!(p("   "), (String::new(), None));
        assert_eq!(p("Today, buy bread at 17"), ("buy bread".into(), Some(ms(0, 17, 0))));
        assert_eq!(p("встреча в 123"), ("встреча в 123".into(), None));
    }

    #[test]
    fn due_labels() {
        let nm = ms(0, 10, 8);
        assert_eq!(due_label(ms(0, 18, 30), nm, OFF, Lang::Ru), ("18:30".into(), false));
        assert_eq!(due_label(ms(0, 10, 0), nm, OFF, Lang::Ru), ("10:00".into(), true));
        assert_eq!(due_label(ms(1, 0, 0), nm, OFF, Lang::Ru), ("Завтра".into(), false));
        assert_eq!(due_label(ms(0, 0, 0), nm, OFF, Lang::Ru), ("Сегодня".into(), false)); // all-day today is not overdue
        assert_eq!(due_label(ms(-1, 0, 0), nm, OFF, Lang::Ru), ("Вчера".into(), true));
        assert_eq!(due_label(ms(1, 9, 5), nm, OFF, Lang::En), ("Tomorrow, 09:05".into(), false));
        assert_eq!(due_label(ms(3, 0, 0), nm, OFF, Lang::Ru), ("27 окт".into(), false));
    }

    fn mw(s: &str) -> f32 {
        s.chars().count() as f32 * 7.0
    }
    #[test]
    fn wrapping() {
        assert_eq!(wrap_lines("aaa bbb ccc ddd eee", 5, 70.0, &mw), vec!["aaa bbb", "ccc ddd", "eee"]);
        assert_eq!(wrap_lines("aaa bbb ccc ddd eee", 2, 70.0, &mw), vec!["aaa bbb", "ccc ddd…"]);
        assert_eq!(wrap_lines("abcdefghijklmnopqrstuvwxyz", 5, 70.0, &mw), vec!["abcdefghij", "klmnopqrst", "uvwxyz"]);
        assert_eq!(wrap_lines("abcdefghijklmnopqrstuvwxyz", 1, 70.0, &mw), vec!["abcdefghi…"]);
        assert_eq!(wrap_lines("short", 2, 70.0, &mw), vec!["short"]);
        assert!(wrap_lines("   ", 2, 70.0, &mw).is_empty());
        assert_eq!(wrap_lines("a\nb  c", 2, 700.0, &mw), vec!["a b c"]);
    }

    #[test]
    fn list_operations() {
        let mut l = Vec::new();
        assert!(add_item(&mut l, "  milk ", None, 10));
        assert!(add_item(&mut l, "bread", Some(99), 11));
        assert!(!add_item(&mut l, "   ", None, 12));
        assert_eq!((l[0].id, l[1].id, l[0].text.as_str()), (1, 2, "milk"));
        assert!(set_done(&mut l, 1, true) && l[0].done && !set_done(&mut l, 42, true));
        assert!(edit_item(&mut l, 2, "rye bread", None) && l[1].text == "rye bread" && l[1].due_ms == Some(99));
        assert!(edit_item(&mut l, 2, "rye bread", Some(5)) && l[1].due_ms == Some(5));
        assert!(!edit_item(&mut l, 2, "rye bread", Some(5)) && !edit_item(&mut l, 9, "x", None));
        assert!(delete_item(&mut l, 1) && !delete_item(&mut l, 1));
        let long = "я".repeat(900);
        assert!(add_item(&mut l, &long, None, 0) && l[1].text.chars().count() == MAX_TEXT);
    }

    #[test]
    fn cap_drops_oldest_done_else_refuses() {
        let mut l = Vec::new();
        for i in 0..MAX_ITEMS {
            assert!(add_item(&mut l, &format!("r{i}"), None, 0));
        }
        assert!(!add_item(&mut l, "over", None, 0));
        l[3].done = true;
        assert!(add_item(&mut l, "over", None, 0));
        assert_eq!(l.len(), MAX_ITEMS);
        assert!(l.iter().all(|x| x.id != 4));
    }

    #[test]
    fn json_round_trip_atomic_and_corrupt() {
        let dir = std::env::temp_dir().join(format!("mswtest-rem-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let f = dir.join("reminders.json");
        assert!(matches!(load_json(&f), Load::Missing));
        let l = vec![
            Item { id: 3, text: "один".into(), done: false, due_ms: Some(1_790_000_000_000), created_ms: 5 },
            Item { id: 9, text: "два \"кавычки\"".into(), done: true, due_ms: None, created_ms: 6 },
        ];
        save_json(&f, &items_to_json(&l)).unwrap();
        assert!(!dir.join("reminders.json.mswtmp").exists());
        let Load::Ok(v) = load_json(&f) else { panic!("load") };
        assert_eq!(items_from_json(&v).unwrap(), l);
        // the Tauri app's own format (extra / missing fields) is read as well
        std::fs::write(&f, "\u{feff}[{\"id\":1,\"text\":\"t\"},{\"junk\":1}]").unwrap();
        let Load::Ok(v) = load_json(&f) else { panic!("load2") };
        assert_eq!(items_from_json(&v).unwrap(), vec![Item { id: 1, text: "t".into(), done: false, due_ms: None, created_ms: 0 }]);
        std::fs::write(&f, "{not json").unwrap();
        assert!(matches!(load_json(&f), Load::Corrupt));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
