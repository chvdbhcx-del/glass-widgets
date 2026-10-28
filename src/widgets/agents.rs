//! Agents: the user's coding-agent sessions (Claude Code, Codex) as seen by the Pill island. S and M up to 4 rows (M adds a state word): a dot (amber =
//! waiting for the user, accent = working), the project (last folder of the session's cwd) and a count badge. Waiting sessions come first.
//! Data: the Pill's hook helper `agent-hook.ps1` writes one small JSON file per session into `%APPDATA%\pill\agents\`
//! (`{tool, session, state, title, cwd, transcript, ts}`); this widget only READS that directory, like Pill's agents.rs does: at most 64
//! regular `.json` files of at most 4 KB, anything unparsable is skipped, `done` and stale (no update for 30 min) sessions are hidden.
//! A session marked waiting whose transcript was written afterwards is working again (Claude Code has no "resumed" hook).
//! The scan runs on a worker every 2 s (`Tick::Poll`, no animation); the card is static between changes: `draw` + `version()`.
use crate::registry::{Registry, WidgetDef};
use crate::widget::*;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Instant;

pub fn register(r: &mut Registry) {
    r.register(WidgetDef {
        type_id: "agents",
        name: ("Агенты", "Agents"),
        desc: ("Сессии Claude Code и Codex: работают или ждут вас", "Claude Code and Codex sessions: working or waiting for you"),
        group: "utilities",
        icon: Glyph::Bolt,
        sizes: &[Size::S, Size::M],
        default_size: Size::S,
        make: || Box::new(Agents::default()),
    });
}

const MAX_FILE: u64 = 4096;
const MAX_FILES: usize = 64;
const STALE_MS: i64 = 30 * 60_000;
/// A transcript written this long after the "waiting" mark means the prompt was answered.
const RESUME_GAP_MS: i64 = 2_000;
const NAME_MAX: usize = 60;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Working,
    Waiting,
    Done,
}

#[derive(Clone, Debug, PartialEq)]
struct Session {
    tool: String,
    session: String,
    state: State,
    cwd: String,
    ts: i64,
    transcript: String,
}

/// One line of the card.
#[derive(Clone, Debug, PartialEq)]
struct Row {
    project: String,
    waiting: bool,
}

/// Text only: control and zero-width / bidi marks removed, whitespace collapsed, capped at `max` chars.
fn clean_text(s: &str, max: usize) -> String {
    let flat: String = s
        .chars()
        .filter(|c| !matches!(*c, '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' | '\u{feff}'))
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    flat.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(max).collect()
}

/// Parse one status file; `mtime` stands in for a missing or absurd `ts`. None = garbage, oversized, unknown state or no usable id.
fn parse_session(bytes: &[u8], mtime: i64, now: i64) -> Option<Session> {
    if bytes.is_empty() || bytes.len() as u64 > MAX_FILE {
        return None;
    }
    let v: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    let o = v.as_object()?;
    let st = |k: &str| o.get(k).and_then(|v| v.as_str()).unwrap_or("");
    let state = match st("state") {
        "working" => State::Working,
        "waiting" => State::Waiting,
        "done" => State::Done,
        _ => return None,
    };
    let session: String = st("session").chars().filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')).take(64).collect();
    let session = session.trim_matches('.').to_string();
    if session.is_empty() {
        return None;
    }
    let tool = match st("tool") {
        t @ ("claude-code" | "codex") => t.to_string(),
        _ => "other".to_string(),
    };
    let ts = o.get("ts").and_then(|v| v.as_i64()).filter(|t| *t > 0 && *t <= now + 300_000).unwrap_or(mtime);
    Some(Session { tool, session, state, cwd: clean_text(st("cwd"), 2048), ts, transcript: st("transcript").chars().take(260).collect() })
}

/// A waiting session whose transcript changed after the mark is working again.
fn resume_if_answered(s: &mut Session, transcript_mtime: Option<i64>) {
    if let (State::Waiting, Some(m)) = (s.state, transcript_mtime) {
        if m > s.ts + RESUME_GAP_MS {
            s.state = State::Working;
            s.ts = m;
        }
    }
}

/// The last folder of a cwd (either slash style); a bare drive keeps its name, nothing usable gives "?".
fn project_name(cwd: &str) -> String {
    let t = cwd.trim_end_matches(['/', '\\']);
    let name = t.rsplit(['/', '\\']).next().unwrap_or("");
    if name.is_empty() {
        "?".to_string()
    } else {
        clean_text(name, NAME_MAX)
    }
}

/// What the card shows: the newest file per session, no `done`, no stale; waiting first, then the most recently updated.
fn active(mut all: Vec<Session>, now: i64) -> Vec<Row> {
    all.sort_by(|a, b| b.ts.cmp(&a.ts));
    let mut seen = HashSet::new();
    all.retain(|s| seen.insert((s.tool.clone(), s.session.clone())));
    all.retain(|s| s.state != State::Done && now - s.ts <= STALE_MS);
    all.sort_by_key(|s| s.state != State::Waiting); // stable: the newest stays first inside each group
    all.iter().map(|s| Row { project: project_name(&s.cwd), waiting: s.state == State::Waiting }).collect()
}

/// mtime (ms) of a regular `.jsonl` file; nothing else is ever stat-ed.
fn transcript_mtime(path: &str) -> Option<i64> {
    let p = Path::new(path);
    if !p.extension().is_some_and(|x| x.eq_ignore_ascii_case("jsonl")) {
        return None;
    }
    let md = std::fs::metadata(p).ok().filter(|m| m.is_file())?;
    Some(md.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?.as_millis() as i64)
}

fn status_dir() -> Option<PathBuf> {
    Some(PathBuf::from(std::env::var_os("APPDATA")?).join("pill").join("agents"))
}

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

/// Reads the status directory (worker thread: file I/O). A missing directory or an unreadable file just gives fewer rows.
fn scan() -> Vec<Row> {
    use std::io::Read;
    let Some(dir) = status_dir() else { return Vec::new() };
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut files: Vec<PathBuf> = rd
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file())) // folders and symlinks are not followed
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("json")))
        .collect();
    files.sort();
    files.truncate(MAX_FILES);
    let now = now_ms();
    let all = files
        .iter()
        .filter_map(|p| {
            let f = std::fs::File::open(p).ok()?;
            let md = f.metadata().ok()?;
            if md.len() > MAX_FILE {
                return None;
            }
            let mtime = md.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?.as_millis() as i64;
            let mut buf = Vec::new();
            f.take(MAX_FILE + 1).read_to_end(&mut buf).ok()?;
            let mut s = parse_session(&buf, mtime, now)?;
            if s.state == State::Waiting && !s.transcript.is_empty() {
                let m = transcript_mtime(&s.transcript);
                resume_if_answered(&mut s, m);
            }
            Some(s)
        })
        .collect();
    active(all, now)
}

fn sample() -> Vec<Row> {
    let r = |p: &str, waiting| Row { project: p.to_string(), waiting };
    vec![r("island-replica", true), r("macshell", false), r("local-ai-telegram-bot", false), r("kadr", false), r("catcode", false)]
}

#[derive(Default)]
struct Agents {
    rows: Vec<Row>,
    loaded: bool,
    job: Option<Pending<Vec<Row>>>,
    last: Option<Instant>,
    ver: u64,
}

impl Agents {
    fn ensure_preview(&mut self, cx: &Cx) {
        if cx.preview && !self.loaded {
            self.loaded = true;
            self.rows = sample();
            self.ver += 1;
        }
    }
}

const ROW_H: f32 = 26.0;

impl Widget for Agents {
    fn draw(&mut self, cx: &mut Cx) -> Result<()> {
        self.ensure_preview(cx);
        let (head, rest) = cx.content().cut_top(20.0);
        cx.text(Rect::new(head.x, head.y, head.w - 34.0, head.h), cx.t("Агенты", "Agents"), Style::TITLE.ellipsis(), SUBTLE);
        if self.rows.is_empty() {
            cx.icon(Glyph::Bolt, rest.centered(32.0, 32.0).offset(0.0, -8.0), FAINT);
            let msg = if self.loaded { cx.t("Нет активных сессий", "No active sessions") } else { cx.t("Загрузка…", "Loading…") };
            cx.text(rest.cut_bottom(18.0).1, msg, Style::CAPTION.center().ellipsis(), DIM);
            return Ok(());
        }
        // the count badge: amber while anything waits for the user
        let col = if self.rows.iter().any(|r| r.waiting) { ORANGE } else { accent() };
        let badge = Rect::new(head.right() - 26.0, head.y + 1.0, 26.0, 18.0);
        cx.pill(badge, col.fade(0.22));
        cx.text(badge, &self.rows.len().to_string(), Style::new(11.0, 600).mono().center(), col);
        let max = 4; // 20 (title) + 6 + 4 x 26 fits the 138 px content height of both sizes
        for (i, row) in self.rows.iter().take(max).enumerate() {
            let r = Rect::new(rest.x, rest.y + 6.0 + i as f32 * ROW_H, rest.w, ROW_H);
            cx.circle((r.x + 5.0, r.cy()), 4.5, if row.waiting { ORANGE } else { accent() });
            let label_w = if cx.size == Size::M { 96.0 } else { 0.0 };
            cx.text(Rect::new(r.x + 18.0, r.y, r.w - 18.0 - label_w, r.h), &row.project, Style::BODY.ellipsis(), if row.waiting { TEXT } else { SUBTLE });
            if label_w > 0.0 {
                let state = if row.waiting { cx.t("ждёт", "waiting") } else { cx.t("работает", "working") };
                cx.text(Rect::new(r.right() - label_w, r.y, label_w, r.h), state, Style::CAPTION.right().ellipsis(), DIM);
            }
        }
        Ok(())
    }

    fn tick(&mut self, cx: &Cx) -> Tick {
        if cx.preview {
            self.ensure_preview(cx);
            return Tick::Never;
        }
        if let Some(j) = &self.job {
            match j.take() {
                Some(rows) => {
                    self.job = None;
                    if !self.loaded || rows != self.rows {
                        self.rows = rows;
                        self.loaded = true;
                        self.ver += 1;
                    }
                }
                None if !j.busy() => self.job = None, // the worker died, try again below
                None => {}
            }
        }
        if self.job.is_none() && self.last.map_or(true, |t| t.elapsed() >= Duration::from_millis(1900)) {
            self.last = Some(Instant::now());
            self.job = Some(cx.spawn(scan));
        }
        Tick::Poll(Duration::from_secs(2))
    }

    fn version(&self) -> u64 {
        self.ver
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_800_000_000_000;

    fn parse(s: &str) -> Option<Session> {
        parse_session(s.as_bytes(), NOW - 1000, NOW)
    }
    fn sess(id: &str, state: State, ts: i64, cwd: &str) -> Session {
        Session { tool: "claude-code".into(), session: id.into(), state, cwd: cwd.into(), ts, transcript: String::new() }
    }

    #[test]
    fn parses_working_and_waiting() {
        let w = parse(r#"{"tool":"claude-code","session":"abc-1","state":"waiting","title":"x","cwd":"C:\\Users\\usr\\macshell","transcript":"C:\\t.jsonl","ts":1799999990000}"#).unwrap();
        assert_eq!((w.state, w.ts, w.session.as_str(), w.tool.as_str()), (State::Waiting, 1_799_999_990_000, "abc-1", "claude-code"));
        assert_eq!(w.cwd, "C:\\Users\\usr\\macshell");
        assert_eq!(parse(r#"{"session":"s","state":"working"}"#).unwrap().tool, "other");
        assert_eq!(parse(r#"{"session":"s","state":"working"}"#).unwrap().ts, NOW - 1000); // no ts: the file's mtime
        assert_eq!(parse(&format!(r#"{{"session":"s","state":"working","ts":{}}}"#, NOW + 10_000_000)).unwrap().ts, NOW - 1000); // a future ts is absurd
    }

    #[test]
    fn rejects_garbage_and_oversized() {
        assert!(parse("not json").is_none());
        assert!(parse("").is_none());
        assert!(parse("[1,2]").is_none());
        assert!(parse(r#"{"session":"s","state":"running"}"#).is_none()); // unknown state
        assert!(parse(r#"{"session":"///","state":"working"}"#).is_none()); // no id left after cleaning
        assert!(parse(r#"{"session":5,"state":["working"],"cwd":{}}"#).is_none());
        let big = format!(r#"{{"session":"s","state":"working","cwd":"{}"}}"#, "a".repeat(5000));
        assert!(parse(&big).is_none());
        // control characters are gone; a path longer than MAX_PATH still ends in its real last folder (the cap is far above 260)
        let s = parse(&format!(r#"{{"session":"s","state":"working","cwd":"C:\\{}\\pro\u0007j"}}"#, "b".repeat(300))).unwrap();
        assert!(!s.cwd.chars().any(|c| c.is_control()));
        assert_eq!(project_name(&s.cwd), "pro j");
    }

    #[test]
    fn project_is_the_last_folder() {
        assert_eq!(project_name("C:\\Users\\usr\\macshell\\native"), "native");
        assert_eq!(project_name("/home/me/proj/"), "proj");
        assert_eq!(project_name("C:\\"), "C:");
        assert_eq!(project_name(""), "?");
        assert_eq!(project_name("\\\\"), "?");
    }

    #[test]
    fn waiting_first_stale_and_done_hidden_duplicates_collapsed() {
        let all = vec![
            sess("a", State::Working, NOW - 1_000, "C:\\p\\alpha"),
            sess("b", State::Waiting, NOW - 5_000, "C:\\p\\beta"),
            sess("c", State::Done, NOW - 1_000, "C:\\p\\gamma"),
            sess("d", State::Working, NOW - STALE_MS - 1, "C:\\p\\delta"), // stale
            sess("e", State::Working, NOW - 2_000, "C:\\p\\epsilon"),
            sess("a", State::Waiting, NOW - 60_000, "C:\\p\\alpha-old"), // older file of the same session: ignored
            sess("f", State::Waiting, NOW - 100, "C:\\p\\zeta"),
        ];
        let rows = active(all, NOW);
        let names: Vec<(&str, bool)> = rows.iter().map(|r| (r.project.as_str(), r.waiting)).collect();
        assert_eq!(names, vec![("zeta", true), ("beta", true), ("alpha", false), ("epsilon", false)]);
        assert!(active(Vec::new(), NOW).is_empty());
    }

    #[test]
    fn answered_prompt_resumes_working() {
        let mut s = sess("a", State::Waiting, NOW - 10_000, "x");
        resume_if_answered(&mut s, None);
        assert_eq!(s.state, State::Waiting);
        resume_if_answered(&mut s, Some(NOW - 10_000 + RESUME_GAP_MS)); // within the gap: still waiting
        assert_eq!(s.state, State::Waiting);
        resume_if_answered(&mut s, Some(NOW - 5_000));
        assert_eq!((s.state, s.ts), (State::Working, NOW - 5_000));
        let mut w = sess("b", State::Working, NOW - 10_000, "x");
        resume_if_answered(&mut w, Some(NOW));
        assert_eq!(w.ts, NOW - 10_000); // only waiting sessions are touched
        assert_eq!(transcript_mtime("C:/definitely/missing.jsonl"), None);
        assert_eq!(transcript_mtime("C:/Windows/notepad.exe"), None); // not a .jsonl: never stat-ed
    }
}
