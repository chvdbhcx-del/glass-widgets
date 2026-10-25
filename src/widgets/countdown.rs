//! Countdown: the days left to a date you choose, with a progress ring (how much of the waiting time has passed).
//! S = ring with the number of days; M = ring + title, weeks and days, the date and a progress bar; L = a big ring with the same facts below.
//! Settings: `cfg.date` = "2026-12-31" (also "31.12.2026" and "31/12/2026"), `cfg.title` = optional name, `cfg.from` = the day the wait
//! started ("2026-10-01", set automatically to today when the date is chosen; without it the ring spans the last 365 days).
//! A click opens a text prompt ("31.12.2026 Новый год") that writes all three. Fully local: no files, no network; `Tick::Never`
//! (the framework re-records the card at midnight, which is all the day count needs).
use crate::registry::{Registry, WidgetDef};
use crate::tz;
use crate::widget::*;

const DEFAULT_SPAN_DAYS: i64 = 365;

pub fn register(r: &mut Registry) {
    r.register(WidgetDef {
        type_id: "countdown",
        name: ("Обратный отсчёт", "Countdown"),
        desc: ("Сколько дней осталось до даты", "Days left until a date you choose"),
        group: "calendar",
        icon: Glyph::Bell,
        sizes: &[Size::S, Size::M, Size::L],
        default_size: Size::S,
        make: || Box::new(Countdown::default()),
    });
}

// ================================================================ pure logic

/// "2026-12-31" / "31.12.2026" / "31/12/2026" -> day number (days since 1970-01-01)
fn parse_date(s: &str) -> Option<i64> {
    let parts: Vec<&str> = s.trim().split(['-', '.', '/']).collect();
    if parts.len() != 3 || parts.iter().any(|p| p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit())) {
        return None;
    }
    let (y, m, d): (i32, u32, u32) = if parts[0].len() == 4 { (parts[0].parse().ok()?, parts[1].parse().ok()?, parts[2].parse().ok()?) } else if parts[2].len() == 4 { (parts[2].parse().ok()?, parts[1].parse().ok()?, parts[0].parse().ok()?) } else { return None };
    if !(1..=12).contains(&m) || d == 0 || d > tz::days_in(y, m) {
        return None;
    }
    Some(tz::days_from_civil(y, m, d))
}

fn iso(day: i64) -> String {
    let (y, m, d) = tz::civil_from_days(day);
    format!("{y:04}-{m:02}-{d:02}")
}

/// The prompt answer: the first word that is a date, the rest is the title ("31.12.2026 Новый год", "Новый год 31.12.2026").
fn parse_input(s: &str) -> Option<(i64, String)> {
    let words: Vec<&str> = s.split_whitespace().collect();
    let i = words.iter().position(|w| parse_date(w).is_some())?;
    let title: Vec<&str> = words.iter().enumerate().filter(|(k, _)| *k != i).map(|(_, w)| *w).collect();
    Some((parse_date(words[i])?, title.join(" ")))
}

/// Share of the waiting time that has passed, 0..1. Without a start day the ring spans the last 365 days.
fn progress(from: Option<i64>, target: i64, today: i64) -> f32 {
    let span = match from {
        Some(f) if f < target => target - f,
        _ => DEFAULT_SPAN_DAYS,
    };
    ((today - (target - span)) as f32 / span as f32).clamp(0.0, 1.0)
}

/// "5 нед. 2 дн." / "5 wk 2 d" (empty below a week)
fn weeks_text(days: i64, ru: bool) -> String {
    let (w, d) = (days / 7, days % 7);
    if w == 0 {
        return String::new();
    }
    let (wu, du) = if ru { ("нед.", "дн.") } else { ("wk", "d") };
    if d == 0 {
        format!("{w} {wu}")
    } else {
        format!("{w} {wu} {d} {du}")
    }
}

/// Unit under the big number: "дней" / "days"; for a date in the past "дн. назад" / "days ago".
fn unit_text(days: i64, ru: bool) -> String {
    let n = days.unsigned_abs() as u32;
    match (ru, days < 0) {
        (true, false) => plural(n, "день", "дня", "дней").to_string(),
        (true, true) => "дн. назад".to_string(),
        (false, false) => (if n == 1 { "day" } else { "days" }).to_string(),
        (false, true) => (if n == 1 { "day ago" } else { "days ago" }).to_string(),
    }
}

fn ring_color(left: i64) -> Color {
    match left {
        l if l < 0 => FAINT,
        0 => GREEN,
        1..=7 => ORANGE,
        _ => ACCENT,
    }
}

/// "Четверг, 31 декабря 2026" / "Thursday, December 31, 2026"
fn date_text(day: i64, lang: Lang) -> String {
    let (y, m, d) = tz::civil_from_days(day);
    let wd = weekday_name(lang, (day + 3).rem_euclid(7) as u32);
    if lang == Lang::Ru {
        format!("{wd}, {d} {} {y}", month_genitive(lang, m))
    } else {
        format!("{wd}, {} {d}, {y}", month_genitive(lang, m))
    }
}

// ================================================================ widget

#[derive(Default)]
struct Countdown {
    started: bool,
    ver: u64,
    /// target day number
    target: Option<i64>,
    from: Option<i64>,
    title: String,
    prompt: Option<Pending<Option<String>>>,
    /// answer of the prompt not yet written to cfg (written by `draw` / `mouse`, which own `&mut Cx`)
    save: Option<(Option<i64>, String, i64)>,
}

impl Countdown {
    fn read_cfg(&mut self, cx: &Cx) {
        self.target = cx.cfg_str("date").and_then(parse_date);
        self.from = cx.cfg_str("from").and_then(parse_date);
        self.title = cx.cfg_str("title").unwrap_or("").trim().to_string();
    }

    fn flush(&mut self, cx: &mut Cx) {
        if let Some((target, title, today)) = self.save.take() {
            match target {
                Some(t) => {
                    cx.cfg_set("date", json!(iso(t)));
                    cx.cfg_set("from", json!(iso(today)));
                }
                None => {
                    cx.cfg_set("date", Value::Null);
                    cx.cfg_set("from", Value::Null);
                }
            }
            cx.cfg_set("title", json!(title));
        }
    }

    fn empty(&self, cx: &Cx) {
        let b = cx.content();
        let y = if cx.size == Size::S { b.y + 12.0 } else { b.y + (b.h - 100.0) / 2.0 };
        cx.icon(Glyph::Bell, Rect::new(b.cx() - 14.0, y, 28.0, 28.0), FAINT);
        cx.text(Rect::new(b.x, y + 36.0, b.w, 20.0), cx.t("Задайте дату", "Set a date"), Style::new(13.0, 600).center().ellipsis(), TEXT);
        cx.text(Rect::new(b.x, y + 58.0, b.w, 44.0), cx.t("Нажмите и введите дату, например 31.12.2026 Новый год", "Click and type a date, e.g. 31.12.2026 New Year"), Style::CAPTION.center().wrap(), DIM);
    }

    /// Ring with the day count (or "Сегодня") inside; `big` is the number size.
    fn ring(&self, cx: &Cx, c: Pt, r: f32, stroke: f32, big: f32, left: i64, frac: f32) {
        let col = ring_color(left);
        cx.ring(c, r, stroke, if left <= 0 { 1.0 } else { frac }, col);
        let inner = r * 1.7;
        if left == 0 {
            cx.text(Rect::new(c.0 - inner / 2.0, c.1 - big * 0.5, inner, big), cx.t("Сегодня", "Today"), Style::new(big * 0.55, 500).center().ellipsis(), TEXT);
            return;
        }
        let n = left.abs().to_string();
        let ns = if n.len() > 3 { big * 0.8 } else { big };
        cx.text(Rect::new(c.0 - inner / 2.0, c.1 - big * 0.85, inner, big * 1.1), &n, Style::new(ns, 300).mono().center().ellipsis(), TEXT);
        let unit = Style::new((big * 0.32).clamp(11.0, 15.0), 500);
        cx.text(Rect::new(c.0 - inner / 2.0, c.1 + big * 0.25, inner, 18.0), &unit_text(left, cx.lang == Lang::Ru), unit.center().ellipsis(), DIM);
    }

    fn title_text<'a>(&'a self, cx: &'a Cx) -> &'a str {
        if self.title.is_empty() {
            cx.t("Обратный отсчёт", "Countdown")
        } else {
            &self.title
        }
    }
}

impl Widget for Countdown {
    fn draw(&mut self, cx: &mut Cx) -> Result<()> {
        self.flush(cx);
        let Some(target) = self.target else {
            self.empty(cx);
            return Ok(());
        };
        let today = tz::days_from_civil(cx.now.y, cx.now.mo, cx.now.d);
        let left = target - today;
        let frac = progress(self.from, target, today);
        let ru = cx.lang == Lang::Ru;
        let c = cx.content();
        let title = self.title_text(cx);
        match cx.size {
            Size::S => {
                cx.text(Rect::new(c.x, c.y, c.w, 18.0), title, Style::TITLE.ellipsis(), DIM);
                self.ring(cx, (c.cx(), c.y + 22.0 + 55.0), 51.0, 7.0, 36.0, left, frac);
            }
            Size::M => {
                self.ring(cx, (c.x + 56.0, c.cy()), 54.0, 8.0, 34.0, left, frac);
                let x = c.x + 130.0;
                let w = c.w - 130.0;
                let st = [16.0, 14.0, 13.0].iter().map(|&z| Style::new(z, 600)).find(|&s| cx.measure(title, s, 0.0).0 <= w).unwrap_or(Style::new(13.0, 600));
                cx.text(Rect::new(x, c.y + 6.0, w, 22.0), title, st.ellipsis(), TEXT);
                let weeks = if left > 0 { weeks_text(left, ru) } else { String::new() };
                cx.text(Rect::new(x, c.y + 32.0, w, 20.0), &weeks, Style::new(14.0, 500).ellipsis(), TEXT);
                cx.text(Rect::new(x, c.bottom() - 52.0, w, 16.0), &date_text(target, cx.lang), Style::CAPTION.ellipsis(), DIM);
                let pct = (frac * 100.0).round() as i32;
                cx.bar(Rect::new(x, c.bottom() - 26.0, w - 40.0, 5.0), frac, ring_color(left));
                cx.text(Rect::new(c.right() - 36.0, c.bottom() - 32.0, 36.0, 16.0), &format!("{pct}%"), Style::CAPTION.mono().right(), DIM);
            }
            Size::L => {
                cx.text(Rect::new(c.x, c.y, c.w, 20.0), title, Style::new(15.0, 600).center().ellipsis(), TEXT);
                self.ring(cx, (c.cx(), c.y + 32.0 + 104.0), 100.0, 14.0, 78.0, left, frac);
                let weeks = if left > 0 { weeks_text(left, ru) } else { String::new() };
                if !weeks.is_empty() {
                    cx.text(Rect::new(c.x, c.bottom() - 62.0, c.w, 22.0), &weeks, Style::new(16.0, 500).center().ellipsis(), TEXT);
                }
                cx.text(Rect::new(c.x, c.bottom() - 38.0, c.w, 18.0), &date_text(target, cx.lang), Style::BODY.center().ellipsis(), DIM);
                let pct = (frac * 100.0).round() as i32;
                let passed = format!("{} {pct}%", cx.t("Прошло", "Elapsed"));
                cx.text(Rect::new(c.x, c.bottom() - 18.0, c.w, 16.0), &passed, Style::CAPTION.center(), FAINT);
            }
        }
        Ok(())
    }

    fn tick(&mut self, cx: &Cx) -> Tick {
        if !self.started {
            self.started = true;
            self.read_cfg(cx);
            if cx.preview && self.target.is_none() {
                let today = tz::days_from_civil(cx.now.y, cx.now.mo, cx.now.d);
                self.target = Some(today + 37);
                self.from = Some(today - 53);
                self.title = cx.t("Отпуск", "Vacation").to_string();
            }
            self.ver += 1;
        }
        if let Some(p) = &self.prompt {
            if let Some(r) = p.take() {
                self.prompt = None;
                if let Some(text) = r {
                    let today = tz::days_from_civil(cx.now.y, cx.now.mo, cx.now.d);
                    let (target, title) = match parse_input(&text) {
                        Some((d, t)) => (Some(d), t),
                        None if text.trim().is_empty() => (None, String::new()), // an empty answer clears the card
                        None => return Tick::Never, // not a date: keep what we have
                    };
                    if target != self.target {
                        self.from = Some(today);
                    }
                    self.target = target;
                    self.title = title.clone();
                    self.save = Some((target, title, self.from.unwrap_or(today)));
                    self.ver += 1;
                }
            } else if !p.busy() {
                self.prompt = None;
            }
        }
        Tick::Never
    }

    fn mouse(&mut self, ev: &MouseEv, cx: &mut Cx) -> bool {
        self.flush(cx);
        if ev.clicked(cx.bounds()) && !cx.preview && self.prompt.is_none() {
            let cur = match self.target {
                Some(t) => format!("{} {}", iso(t), self.title).trim().to_string(),
                None => String::new(),
            };
            self.prompt = Some(cx.prompt_text_ex(cx.t("Дата и название", "Date and title"), &cur, false, cx.t("31.12.2026 Новый год", "2026-12-31 New Year"), ""));
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

    #[test]
    fn dates() {
        let d = tz::days_from_civil(2026, 12, 31);
        assert_eq!(parse_date("2026-12-31"), Some(d));
        assert_eq!(parse_date("31.12.2026"), Some(d));
        assert_eq!(parse_date(" 31/12/2026 "), Some(d));
        assert_eq!(parse_date("2026-02-30"), None);
        assert_eq!(parse_date("31.13.2026"), None);
        assert_eq!(parse_date("1.1.26"), None);
        assert_eq!(parse_date("hello"), None);
        assert_eq!(iso(d), "2026-12-31");
    }

    #[test]
    fn prompt_input() {
        let d = tz::days_from_civil(2027, 1, 1);
        assert_eq!(parse_input("01.01.2027 Новый год"), Some((d, "Новый год".to_string())));
        assert_eq!(parse_input("Новый год 2027-01-01"), Some((d, "Новый год".to_string())));
        assert_eq!(parse_input("2027-01-01"), Some((d, String::new())));
        assert_eq!(parse_input("скоро"), None);
    }

    #[test]
    fn ring_progress() {
        let t = tz::days_from_civil(2026, 12, 31);
        assert_eq!(progress(Some(t - 100), t, t - 75), 0.25);
        assert_eq!(progress(Some(t - 100), t, t), 1.0);
        assert_eq!(progress(Some(t - 100), t, t + 5), 1.0);
        assert_eq!(progress(Some(t - 100), t, t - 200), 0.0);
        // no start (or one after the target): the last 365 days
        assert!((progress(None, t, t - 365 / 2) - 0.5).abs() < 0.01);
        assert_eq!(progress(Some(t + 3), t, t - 365), 0.0);
    }

    #[test]
    fn texts() {
        assert_eq!(weeks_text(37, true), "5 нед. 2 дн.");
        assert_eq!(weeks_text(14, false), "2 wk");
        assert_eq!(weeks_text(6, true), "");
        assert_eq!(unit_text(1, true), "день");
        assert_eq!(unit_text(3, true), "дня");
        assert_eq!(unit_text(37, true), "дней");
        assert_eq!(unit_text(1, false), "day");
        assert_eq!(unit_text(-4, true), "дн. назад");
        assert_eq!(unit_text(-4, false), "days ago");
        assert_eq!(ring_color(0), GREEN);
        assert_eq!(ring_color(5), ORANGE);
        assert_eq!(ring_color(30), ACCENT);
        assert_eq!(ring_color(-1), FAINT);
        let d = tz::days_from_civil(2026, 12, 31);
        assert_eq!(date_text(d, Lang::Ru), "Четверг, 31 декабря 2026");
        assert_eq!(date_text(d, Lang::En), "Thursday, December 31, 2026");
    }
}
