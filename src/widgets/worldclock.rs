//! World clock: M = up to 4 city columns (mini analog clock, city, digital time, "Сегодня, +3 ч"); L = up to 6 rows
//! (mini clock, city, day + offset, 28 light digital time). A face is light by day and dark by night (like Apple's widget).
//! The small title "МИРОВЫЕ ЧАСЫ" at the top opens Alarms & Clock (`cx.open_card_target`, honours the card's "Click action").
//! Click a city = remove it; the "+" slot (while there is room) asks for a name (`cx.prompt_text`) and fuzzy-matches it
//! (case-insensitive exact / prefix / contains, Russian or English or id, spaces and hyphens ignored) against the zone table.
//! Settings: `cfg.zones = ["Москва", "london", ...]` (names or ids; absent = Москва, Лондон, Нью-Йорк, Токио (+ Дубай, Сидней on L)).
//! Core gap: `tick` only gets `&Cx` and cannot write `cfg`, so a city added through the prompt is written to `cfg.zones`
//! on the next mouse event (it is shown immediately).
use crate::registry::{Registry, WidgetDef};
use crate::widget::*;

pub fn register(r: &mut Registry) {
    r.register(WidgetDef {
        type_id: "worldclock",
        name: ("Мировые часы", "World Clock"),
        desc: ("Время в разных городах мира", "The time in cities around the world"),
        group: "clock",
        icon: Glyph::Clock,
        sizes: &[Size::M, Size::L],
        default_size: Size::M,
        make: || Box::new(WorldClock::default()),
    });
}

// ================================================================ zone table (tz.rs + extra cities with the same DST rules)

const fn z(id: &'static str, ru: &'static str, en: &'static str, std_min: i32, dst: tz::Dst) -> tz::Zone {
    tz::Zone { id, ru, en, std_min, dst }
}
use tz::Dst::{Au, Eu, None as No, Us};

static EXTRA: &[tz::Zone] = &[
    z("spb", "Санкт-Петербург", "Saint Petersburg", 180, No),
    z("minsk", "Минск", "Minsk", 180, No),
    z("samara", "Самара", "Samara", 240, No),
    z("omsk", "Омск", "Omsk", 360, No),
    z("krasnoyarsk", "Красноярск", "Krasnoyarsk", 420, No),
    z("irkutsk", "Иркутск", "Irkutsk", 480, No),
    z("yakutsk", "Якутск", "Yakutsk", 540, No),
    z("magadan", "Магадан", "Magadan", 660, No),
    z("kamchatka", "Петропавловск-Камчатский", "Petropavlovsk-Kamchatsky", 720, No),
    z("tbilisi", "Тбилиси", "Tbilisi", 240, No),
    z("yerevan", "Ереван", "Yerevan", 240, No),
    z("baku", "Баку", "Baku", 240, No),
    z("tashkent", "Ташкент", "Tashkent", 300, No),
    z("almaty", "Алматы", "Almaty", 300, No),
    z("rome", "Рим", "Rome", 60, Eu),
    z("madrid", "Мадрид", "Madrid", 60, Eu),
    z("amsterdam", "Амстердам", "Amsterdam", 60, Eu),
    z("vienna", "Вена", "Vienna", 60, Eu),
    z("prague", "Прага", "Prague", 60, Eu),
    z("warsaw", "Варшава", "Warsaw", 60, Eu),
    z("stockholm", "Стокгольм", "Stockholm", 60, Eu),
    z("athens", "Афины", "Athens", 120, Eu),
    z("helsinki", "Хельсинки", "Helsinki", 120, Eu),
    z("lisbon", "Лиссабон", "Lisbon", 0, Eu),
    z("dublin", "Дублин", "Dublin", 0, Eu),
    z("beijing", "Пекин", "Beijing", 480, No),
    z("hong_kong", "Гонконг", "Hong Kong", 480, No),
    z("singapore", "Сингапур", "Singapore", 480, No),
    z("jakarta", "Джакарта", "Jakarta", 420, No),
    z("mumbai", "Мумбаи", "Mumbai", 330, No),
    z("melbourne", "Мельбурн", "Melbourne", 600, Au),
    z("toronto", "Торонто", "Toronto", -300, Us),
    z("vancouver", "Ванкувер", "Vancouver", -480, Us),
    z("mexico_city", "Мехико", "Mexico City", -360, No),
    z("buenos_aires", "Буэнос-Айрес", "Buenos Aires", -180, No),
];

fn all() -> impl Iterator<Item = &'static tz::Zone> {
    tz::ZONES.iter().chain(EXTRA.iter())
}

/// lower case, no spaces / hyphens / underscores / dots / apostrophes, "ё" = "е"
fn norm(s: &str) -> String {
    s.chars().filter(|c| !matches!(c, ' ' | '-' | '_' | '.' | '\'')).flat_map(char::to_lowercase).map(|c| if c == 'ё' { 'е' } else { c }).collect()
}

/// Zone for a typed or configured name: exact match of id / ru / en, else prefix, else substring (>= 2 chars); shortest name wins.
fn find_zone(q: &str) -> Option<&'static tz::Zone> {
    let q = norm(q);
    if q.is_empty() {
        return None;
    }
    let names = |z: &'static tz::Zone| [norm(z.id), norm(z.ru), norm(z.en)];
    if let Some(z) = all().find(|z| names(z).iter().any(|n| *n == q)) {
        return Some(z);
    }
    let best = |f: &dyn Fn(&str) -> bool| all().filter(|z| names(z).iter().any(|n| f(n))).min_by_key(|z| z.en.len());
    best(&|n| n.starts_with(&q)).or_else(|| if q.chars().count() >= 2 { best(&|n| n.contains(&q)) } else { None })
}

const DEFAULTS: [&str; 6] = ["moscow", "london", "new_york", "tokyo", "dubai", "sydney"];

fn cap(size: Size) -> usize {
    if size == Size::L {
        6
    } else {
        4
    }
}

/// The configured list (or the defaults), unknown names dropped, duplicates removed, capped.
fn load_list(cfg: &Value, size: Size) -> Vec<&'static tz::Zone> {
    let mut out: Vec<&'static tz::Zone> = Vec::new();
    let mut add = |name: &str| {
        if let Some(z) = find_zone(name) {
            if !out.iter().any(|o| o.id == z.id) && out.len() < cap(size) {
                out.push(z);
            }
        }
    };
    match cfg.get("zones").and_then(Value::as_array) {
        Some(a) => a.iter().filter_map(Value::as_str).for_each(&mut add),
        None => DEFAULTS.iter().for_each(|n| add(n)),
    }
    out
}

/// "+3" / "−5" / "+5:30" (hours difference to the local clock), empty when equal.
fn offset_text(diff_min: i32) -> String {
    if diff_min == 0 {
        return String::new();
    }
    let a = diff_min.abs();
    let h = if a % 60 != 0 { format!("{}:{:02}", a / 60, a % 60) } else { format!("{}", a / 60) };
    format!("{}{}", if diff_min < 0 { '−' } else { '+' }, h)
}

// ================================================================ widget

#[derive(Default)]
struct WorldClock {
    list: Vec<&'static tz::Zone>,
    started: bool,
    min: u64,
    ver: u64,
    hover: Option<usize>,
    /// the cursor is over the title
    head_hot: bool,
    prompt: Option<Pending<Option<String>>>,
    /// last typed name that matched nothing (shown in the "+" slot)
    miss: String,
    dirty: bool,
}

fn cell(size: Size, i: usize) -> Rect {
    if size == Size::L {
        Rect::new(16.0, 30.0 + 52.0 * i as f32, 324.0, 52.0)
    } else {
        Rect::new(16.0 + 81.0 * i as f32, 26.0, 81.0, 128.0)
    }
}

/// The small title at the top ("МИРОВЫЕ ЧАСЫ"): a click opens Alarms & Clock (`cx.open_card_target`), the cells below keep their own clicks.
fn head_label(cx: &Cx) -> &'static str {
    cx.t("МИРОВЫЕ ЧАСЫ", "WORLD CLOCK")
}
fn head_style() -> Style {
    Style::LABEL.size(10.5)
}
fn head_zone(cx: &Cx) -> Rect {
    let (w, _) = cx.measure(head_label(cx), head_style(), 0.0);
    Rect::new(10.0, 6.0, (w + 20.0).min(cx.w - 20.0), 22.0)
}

/// Mini analog clock: light face + dark hands by day, dark face + light hands by night.
fn mini_clock(cx: &Cx, c: Pt, d: f32, h: u32, mi: u32, day: bool) {
    let r = d / 2.0;
    let (face, ink) = if day { (Color::rgba(1.0, 1.0, 1.0, 0.92), INK) } else { (Color::rgba(0.0, 0.0, 0.0, 0.42), TEXT) };
    cx.circle(c, r, face);
    cx.circle_stroke(c, r - 0.4, 0.8, if day { Color::rgba(0.0, 0.0, 0.0, 0.10) } else { HAIR });
    for i in 0..4 {
        let (s, k) = (i as f32 * 90.0).to_radians().sin_cos();
        cx.line((c.0 + s * r * 0.78, c.1 - k * r * 0.78), (c.0 + s * r * 0.88, c.1 - k * r * 0.88), (d * 0.025).max(0.8), ink.fade(0.55));
    }
    let hand = |deg: f32, len: f32, w: f32| {
        let (s, k) = deg.to_radians().sin_cos();
        cx.line(c, (c.0 + s * r * len, c.1 - k * r * len), w, ink);
    };
    hand(((h % 12) as f32 + mi as f32 / 60.0) * 30.0, 0.5, (d * 0.065).max(1.6));
    hand(mi as f32 * 6.0, 0.76, (d * 0.045).max(1.2));
    cx.circle(c, (d * 0.045).max(1.4), ink);
}

struct Info {
    h: u32,
    mi: u32,
    day: bool,
    /// "Сегодня" / "Завтра" / "Вчера" + ", +3 ч"
    caption: String,
}

fn info(cx: &Cx, z: &tz::Zone) -> Info {
    let t = cx.zone_time(z);
    let local = tz::civil_from_unix(cx.utc_secs(), cx.utc_offset_min());
    let day_word = match t.day_number() - local.day_number() {
        0 => cx.t("Сегодня", "Today"),
        d if d > 0 => cx.t("Завтра", "Tomorrow"),
        _ => cx.t("Вчера", "Yesterday"),
    };
    let off = offset_text(t.offset_min - local.offset_min);
    let caption = if off.is_empty() { day_word.to_string() } else { format!("{}, {} {}", day_word, off, cx.t("ч", "h")) };
    Info { h: t.h, mi: t.mi, day: (6..19).contains(&t.h), caption }
}

impl WorldClock {
    fn slots(&self, size: Size) -> usize {
        self.list.len() + (self.list.len() < cap(size)) as usize
    }
    fn hit(&self, size: Size, x: f32, y: f32) -> Option<usize> {
        (0..self.slots(size)).find(|&i| cell(size, i).contains(x, y))
    }
    fn persist(&mut self, cx: &mut Cx) {
        let ids: Vec<&str> = self.list.iter().map(|z| z.id).collect();
        cx.cfg_set("zones", json!(ids));
        self.dirty = false;
    }
}

impl Widget for WorldClock {
    fn draw(&mut self, cx: &mut Cx) -> Result<()> {
        let size = cx.size;
        let hz = head_zone(cx);
        if self.head_hot {
            cx.rrect(hz, 9.0, Color::rgba(1.0, 1.0, 1.0, 0.10));
        }
        cx.text(Rect::new(hz.x + 10.0, hz.y, hz.w - 10.0, hz.h), head_label(cx), head_style().ellipsis(), if self.head_hot { TEXT } else { DIM });
        for i in 0..self.slots(size) {
            let c = cell(size, i);
            let hovered = self.hover == Some(i);
            if size == Size::L && i > 0 {
                cx.rect(Rect::new(c.x, c.y, c.w, 0.6), FILL);
            }
            if hovered {
                cx.rrect(c.inset(2.0), 14.0, Color::rgba(1.0, 1.0, 1.0, 0.09));
            }
            match self.list.get(i) {
                Some(zone) => {
                    let inf = info(cx, zone);
                    let time = format!("{:02}:{:02}", inf.h, inf.mi);
                    let city = cx.t(zone.ru, zone.en);
                    if size == Size::L {
                        mini_clock(cx, (c.x + 19.0, c.cy()), 38.0, inf.h, inf.mi, inf.day);
                        cx.text(Rect::new(c.x + 54.0, c.y + 7.0, 166.0, 21.0), city, Style::new(15.0, 600).ellipsis(), TEXT);
                        cx.text(Rect::new(c.x + 54.0, c.y + 28.0, 166.0, 16.0), &inf.caption, Style::new(11.5, 400).ellipsis(), DIM);
                        cx.text(Rect::new(c.right() - 110.0, c.y, 110.0, c.h), &time, Style::new(28.0, 300).mono().right(), TEXT);
                        if hovered {
                            cx.icon(Glyph::X, Rect::new(c.x + 226.0, c.cy() - 7.0, 14.0, 14.0), DIM);
                        }
                    } else {
                        mini_clock(cx, (c.cx(), c.y + 37.0), 56.0, inf.h, inf.mi, inf.day);
                        cx.text(Rect::new(c.x + 3.0, c.y + 71.0, c.w - 6.0, 18.0), city, Style::new(13.0, 600).center().ellipsis(), TEXT);
                        cx.text(Rect::new(c.x, c.y + 89.0, c.w, 24.0), &time, Style::new(20.0, 300).mono().center(), TEXT);
                        cx.text(Rect::new(c.x + 2.0, c.y + 113.0, c.w - 4.0, 16.0), &inf.caption, Style::new(11.0, 400).center().ellipsis(), DIM);
                        if hovered {
                            cx.icon(Glyph::X, Rect::new(c.right() - 19.0, c.y + 5.0, 12.0, 12.0), DIM);
                        }
                    }
                }
                None => {
                    // the "+" slot
                    let (label, col) = if self.miss.is_empty() {
                        (cx.t("Добавить город", "Add a city").to_string(), DIM)
                    } else if size == Size::L {
                        (format!("{} «{}»", cx.t("Не найден", "Not found"), self.miss), ORANGE)
                    } else {
                        (cx.t("Не найден", "Not found").to_string(), ORANGE)
                    };
                    if size == Size::L {
                        let m = (c.x + 19.0, c.cy());
                        cx.circle_stroke(m, 18.6, 1.2, FAINT);
                        cx.icon(Glyph::Plus, Rect::new(m.0 - 9.0, m.1 - 9.0, 18.0, 18.0), DIM);
                        cx.text(Rect::new(c.x + 54.0, c.y, 270.0, c.h), &label, Style::new(14.0, 500).ellipsis(), col);
                    } else {
                        let m = (c.cx(), c.y + 37.0);
                        cx.circle_stroke(m, 27.4, 1.2, FAINT);
                        cx.icon(Glyph::Plus, Rect::new(m.0 - 11.0, m.1 - 11.0, 22.0, 22.0), DIM);
                        cx.text(Rect::new(c.x + 3.0, c.y + 71.0, c.w - 6.0, 36.0), &label, Style::new(12.0, 500).center().wrap(), col);
                    }
                }
            }
        }
        Ok(())
    }

    fn tick(&mut self, cx: &Cx) -> Tick {
        if !self.started {
            self.started = true;
            self.list = load_list(cx.cfg, cx.size);
            self.ver += 1;
        }
        self.min = cx.now.minute_key();
        let answer = self.prompt.as_ref().map(|p| (!p.busy(), p.take()));
        if let Some((done, res)) = answer {
            if res.is_some() || done {
                self.prompt = None;
            }
            if let Some(Some(text)) = res {
                match find_zone(&text) {
                    Some(zone) => {
                        self.miss.clear();
                        if self.list.len() < cap(cx.size) && !self.list.iter().any(|o| o.id == zone.id) {
                            self.list.push(zone);
                            self.dirty = true;
                        }
                    }
                    None => self.miss = text.trim().chars().take(24).collect(),
                }
                self.ver += 1;
            }
        }
        if cx.preview {
            Tick::Never
        } else {
            Tick::Minute
        }
    }

    fn mouse(&mut self, ev: &MouseEv, cx: &mut Cx) -> bool {
        if self.dirty {
            self.persist(cx);
        }
        let size = cx.size;
        let head = ev.inside(head_zone(cx));
        let h = if ev.kind == MouseKind::Leave || head { None } else { self.hit(size, ev.x, ev.y) };
        let mut changed = h != self.hover || head != self.head_hot;
        (self.hover, self.head_hot) = (h, head);
        if head && ev.clicked(cx.bounds()) {
            cx.open_card_target("worldclock"); // Alarms & Clock, or the card's "Click action"
        }
        if ev.kind == MouseKind::Up && ev.button == Button::Left {
            if let Some(i) = h {
                if i < self.list.len() {
                    self.list.remove(i);
                    self.persist(cx);
                    self.miss.clear();
                    changed = true;
                } else if self.prompt.as_ref().map_or(true, |p| !p.busy()) {
                    self.prompt = Some(cx.prompt_text(cx.t("Город", "City"), "", false));
                }
            }
        }
        if changed {
            self.ver += 1;
        }
        changed
    }

    fn version(&self) -> u64 {
        // the minute is part of the version so the hands and digits are re-recorded once a minute
        self.ver.wrapping_mul(1 << 20) ^ self.min
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuzzy_matching() {
        assert_eq!(find_zone("москва").unwrap().id, "moscow");
        assert_eq!(find_zone("Нью Йорк").unwrap().id, "new_york");
        assert_eq!(find_zone("new-york").unwrap().id, "new_york");
        assert_eq!(find_zone("NEW").unwrap().id, "new_york"); // prefix, shortest name first
        assert_eq!(find_zone("токио").unwrap().id, "tokyo");
        assert_eq!(find_zone("Tok").unwrap().id, "tokyo");
        assert_eq!(find_zone("лос").unwrap().id, "los_angeles");
        assert_eq!(find_zone("петербург").unwrap().id, "spb"); // contains
        assert_eq!(find_zone("Рим").unwrap().id, "rome");
        assert!(find_zone("Кёльн").is_none());
        assert!(find_zone("").is_none() && find_zone("   ").is_none() && find_zone("ъ").is_none());
    }

    #[test]
    fn table_ids_unique() {
        let mut ids: Vec<&str> = all().map(|z| z.id).collect();
        let n = ids.len();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), n);
    }

    #[test]
    fn list_loading() {
        let l = load_list(&json!({}), Size::M);
        assert_eq!(l.iter().map(|z| z.id).collect::<Vec<_>>(), ["moscow", "london", "new_york", "tokyo"]);
        assert_eq!(load_list(&json!({}), Size::L).len(), 6);
        let l = load_list(&json!({"zones": ["Токио", "tokyo", "nowhere", "Лондон"]}), Size::L);
        assert_eq!(l.iter().map(|z| z.id).collect::<Vec<_>>(), ["tokyo", "london"]);
        assert!(load_list(&json!({"zones": []}), Size::M).is_empty());
    }

    #[test]
    fn offsets() {
        assert_eq!(offset_text(0), "");
        assert_eq!(offset_text(180), "+3");
        assert_eq!(offset_text(-300), "−5");
        assert_eq!(offset_text(330), "+5:30");
        assert_eq!(offset_text(-570), "−9:30");
    }
}
