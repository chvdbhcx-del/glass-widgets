//! Disk health: the Windows health verdict of every physical disk (Healthy / Warning / Unhealthy) with temperature, wear and power-on
//! hours where the system reports them. S = the worst disk, M = up to 3 disks as rows.
//! Data: one hidden PowerShell run (fixed script, `Get-PhysicalDisk | Get-StorageReliabilityCounter`, read-only, no elevation) on a
//! worker every 60 s. Without administrator rights Windows often leaves the reliability counters empty: those fields show "n/a" and
//! only the health verdict is real. Raw SMART attributes (DeviceIoControl) need admin and are NOT used.
//! Nothing is drawn per frame: a new reading bumps `version()`; previews use sample data and start no process.
use super::sysmon::fmt_size;
use super::tasks::{diff_ids, edge_fade, unique_keys};
use crate::registry::{Registry, WidgetDef};
use crate::sys;
use crate::widget::*;
use std::time::Instant;

pub fn register(r: &mut Registry) {
    r.register(WidgetDef {
        type_id: "diskhealth",
        name: ("Здоровье дисков", "Disk health"),
        desc: ("Состояние, температура и износ дисков", "Health, temperature and wear of your drives"),
        group: "system",
        icon: Glyph::Disk,
        sizes: &[Size::S, Size::M],
        default_size: Size::S,
        make: || Box::new(DiskHealth::default()),
    });
}

const SCRIPT: &str = "$ErrorActionPreference='SilentlyContinue'; \
$r = foreach ($d in Get-PhysicalDisk) { $c = $d | Get-StorageReliabilityCounter; \
[pscustomobject]@{ name=[string]$d.FriendlyName; health=[string]$d.HealthStatus; media=[string]$d.MediaType; size=[int64]$d.Size; temp=$c.Temperature; wear=$c.Wear; hours=$c.PowerOnHours } }; \
ConvertTo-Json -Compress -InputObject @($r)";
const MAX_DISKS: usize = 8;

// ================================================================ pure parsing

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Health {
    /// worst first: this order is the sort order of the card
    Unhealthy,
    Warning,
    Unknown,
    Healthy,
}

#[derive(Clone, Debug, PartialEq)]
struct Disk {
    name: String,
    health: Health,
    media: String,
    size: u64,
    /// degrees C
    temp: Option<u32>,
    /// share of the rated endurance used, 0..100
    wear: Option<u32>,
    hours: Option<u64>,
}

fn health_of(s: &str) -> Health {
    match s.trim().to_ascii_lowercase().as_str() {
        "healthy" => Health::Healthy,
        "warning" => Health::Warning,
        "unhealthy" => Health::Unhealthy,
        _ => Health::Unknown,
    }
}

fn clean(s: &str, max: usize) -> String {
    s.chars().map(|c| if c.is_control() { ' ' } else { c }).collect::<String>().split_whitespace().collect::<Vec<_>>().join(" ").chars().take(max).collect()
}

/// The script's JSON: an array (or, from other tools, one object) of disks. Garbage gives an empty list; unusable fields give `None`.
fn parse_disks(json: &str) -> Vec<Disk> {
    let Ok(v) = serde_json::from_str::<Value>(json.trim().trim_start_matches('\u{feff}')) else { return Vec::new() };
    let items: Vec<&Value> = match &v {
        Value::Array(a) => a.iter().collect(),
        Value::Object(_) => vec![&v],
        _ => Vec::new(),
    };
    let num = |e: &Value, k: &str| e.get(k).and_then(Value::as_f64).filter(|x| x.is_finite() && *x >= 0.0);
    let mut out: Vec<Disk> = items
        .into_iter()
        .filter_map(|e| {
            let name = clean(e.get("name")?.as_str()?, 40);
            (!name.is_empty()).then(|| Disk {
                name,
                health: health_of(e.get("health").and_then(Value::as_str).unwrap_or("")),
                media: clean(e.get("media").and_then(Value::as_str).unwrap_or(""), 12).replace("Unspecified", ""),
                size: num(e, "size").map_or(0, |x| x as u64),
                temp: num(e, "temp").filter(|t| (1.0..=150.0).contains(t)).map(|t| t as u32), // 0 = "no sensor"
                wear: num(e, "wear").filter(|w| *w <= 100.0).map(|w| w as u32),
                hours: num(e, "hours").map(|h| h as u64),
            })
        })
        .take(MAX_DISKS)
        .collect();
    out.sort_by_key(|d| d.health); // stable: Windows' own order inside a verdict
    out
}

/// 12345 -> "12 345" (a thin gap, the thousands are readable in a narrow column).
fn grouped(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push('\u{2009}');
        }
        out.push(c);
    }
    out
}

// ================================================================ motion helpers (pure)

/// A unique glide key per disk: its name, with "#2", "#3" for a second and third disk of the same name.
fn disk_keys(disks: &[Disk]) -> Vec<String> {
    unique_keys(disks.iter().map(|d| d.name.as_str()))
}

/// The verdict as a glidable number: healthy 0, warning 1, unhealthy 2, unknown 3.
fn rank(h: Health) -> f32 {
    match h {
        Health::Healthy => 0.0,
        Health::Warning => 1.0,
        Health::Unhealthy => 2.0,
        Health::Unknown => 3.0,
    }
}

/// The colour of a (glided, fractional) verdict rank: green -> amber -> red -> grey.
fn colour_at(x: f32) -> Color {
    let x = x.clamp(0.0, 3.0);
    if x <= 1.0 {
        GREEN.mix(ORANGE, x)
    } else if x <= 2.0 {
        ORANGE.mix(RED, x - 1.0)
    } else {
        RED.mix(DIM, x - 2.0)
    }
}

// ================================================================ widget

/// A disk that vanished: it fades out in its slot.
struct Ghost {
    key: String,
    disk: Disk,
}

#[derive(Default)]
struct DiskHealth {
    disks: Vec<Disk>,
    /// a reading arrived (an empty list then means "no disks reported")
    loaded: bool,
    job: Option<Pending<Option<Vec<Disk>>>>,
    last: Option<Instant>,
    ver: u64,
    started: bool,
    /// per disk key: "y:" slot, "a:" opacity (a disk appears / vanishes), "w:" word cross-fade 0 -> 1, "s:" the S card's fade when the worst disk changes
    slots: GlideAnim,
    /// per disk key: "c:" verdict rank (the colour cross-fades), "t:" temperature, "r:" wear, "h:" hours
    vals: GlideAnim,
    keys: Vec<String>,
    /// the verdict a word is fading away from
    old_health: Vec<(String, Health)>,
    ghosts: Vec<Ghost>,
    primed: bool,
    moving: bool,
}

/// Worst first, like `parse_disks`.
fn sample() -> Vec<Disk> {
    const GB: u64 = 1_000_000_000;
    let d = |name: &str, health, media: &str, size, temp, wear, hours| Disk { name: name.into(), health, media: media.into(), size, temp, wear, hours };
    let mut v = vec![
        d("Samsung SSD 990 PRO 2TB", Health::Healthy, "SSD", 2000 * GB, Some(41), Some(3), Some(5_812)),
        d("WD Blue SN570 1TB", Health::Warning, "SSD", 1000 * GB, Some(52), Some(88), Some(21_340)),
        d("Seagate Barracuda 4TB", Health::Healthy, "HDD", 4000 * GB, None, None, None),
    ];
    v.sort_by_key(|d| d.health);
    v
}

fn word(cx: &Cx, h: Health) -> &'static str {
    match h {
        Health::Healthy => cx.t("Исправен", "Healthy"),
        Health::Warning => cx.t("Внимание", "Warning"),
        Health::Unhealthy => cx.t("Неисправен", "Unhealthy"),
        Health::Unknown => cx.t("Неизвестно", "Unknown"),
    }
}
fn na(cx: &Cx) -> &'static str {
    cx.t("н/д", "n/a")
}
fn grouped_hours(cx: &Cx, h: u64) -> String {
    format!("{} {}", grouped(h), cx.t("ч", "h"))
}

const ROW_H: f32 = 38.0;

impl DiskHealth {
    fn init(&mut self, cx: &Cx) {
        if self.started {
            return;
        }
        self.started = true;
        self.ver += 1;
        self.slots = GlideAnim::smooth(0.4);
        if cx.preview {
            self.disks = sample();
            self.loaded = true;
        }
        self.retarget(cx, Vec::new());
    }

    /// Aim every glide at the current list. `gone` = the disks of the previous list (to fade the vanished ones out).
    fn retarget(&mut self, cx: &Cx, gone: Vec<Disk>) {
        let new_keys = disk_keys(&self.disks);
        let anim = self.primed && !cx.preview && !reduced_motion();
        let (entering, leaving) = diff_ids(&self.keys, &new_keys);
        for k in &leaving {
            if let Some(d) = self.keys.iter().position(|x| x == k).and_then(|i| gone.get(i)).filter(|_| anim) {
                self.ghosts.retain(|g| g.key != *k);
                self.ghosts.push(Ghost { key: k.clone(), disk: d.clone() });
                self.slots.set(&format!("a:{k}"), 0.0, cx);
            }
        }
        let first_changed = new_keys.first() != self.keys.first();
        for (i, (k, d)) in new_keys.iter().zip(&self.disks).enumerate() {
            self.ghosts.retain(|g| g.key != *k);
            let (y, a) = (format!("y:{k}"), format!("a:{k}"));
            if anim && entering.contains(k) {
                self.slots.set_from(&y, i as f32 + 0.8, i as f32, cx);
                self.slots.set_from(&a, 0.0, 1.0, cx);
            } else {
                self.slots.set(&y, i as f32, cx);
                self.slots.set(&a, 1.0, cx);
            }
            // the verdict: colour glides, the word cross-fades from the old one
            if let (Some(old), true) = (gone.iter().zip(&self.keys).find(|(_, ok)| *ok == k).map(|(o, _)| o.health), anim) {
                if old != d.health {
                    self.old_health.retain(|(x, _)| x != k);
                    self.old_health.push((k.clone(), old));
                    self.slots.start(&format!("w:{k}"), 0.0, 1.0, cx);
                }
            }
            self.vals.set(&format!("c:{k}"), rank(d.health), cx);
            for (p, v) in [("t", d.temp.map(|v| v as f32)), ("r", d.wear.map(|v| v as f32)), ("h", d.hours.map(|v| v as f32))] {
                match v {
                    Some(v) => self.vals.set(&format!("{p}:{k}"), v, cx),
                    None => self.vals.forget(&format!("{p}:{k}")),
                }
            }
        }
        if anim && first_changed {
            if let Some(k) = new_keys.first() {
                self.slots.start(&format!("s:{k}"), 0.0, 1.0, cx);
            }
        }
        self.keys = new_keys;
        self.primed = true;
        cx.waker().wake();
    }

    /// The verdict word of a disk: while it changes the old word fades out and the new one in.
    fn paint_word(&self, cx: &Cx, key: &str, d: &Disk, r: Rect, st: Style) {
        let col = colour_at(self.vals.or(&format!("c:{key}"), rank(d.health)));
        let w = self.slots.or(&format!("w:{key}"), 1.0);
        match self.old_health.iter().find(|(k, _)| k == key).filter(|_| w < 1.0) {
            Some((_, old)) => {
                cx.text(r, word(cx, *old), st, col.fade(1.0 - w));
                cx.text(r, word(cx, d.health), st, col.fade(w));
            }
            None => cx.text(r, word(cx, d.health), st, col),
        }
    }

    fn glided(&self, p: &str, key: &str, v: Option<u32>) -> Option<u32> {
        v.map(|v| self.vals.or(&format!("{p}:{key}"), v as f32).round() as u32)
    }

    fn paint_m_row(&self, cx: &Cx, c: Rect, key: &str, d: &Disk, y: f32, a: f32) {
        let a = a * edge_fade(y, 3);
        if a <= 0.0 {
            return;
        }
        let row = Rect::new(c.x, c.y + 24.0 + y * ROW_H, c.w, 36.0);
        cx.with_opacity(a, |cx| {
            let col = colour_at(self.vals.or(&format!("c:{key}"), rank(d.health)));
            cx.circle((row.x + 4.0, row.y + 9.0), 3.5, col);
            cx.text(Rect::new(row.x + 14.0, row.y, row.w - 14.0 - 96.0, 18.0), &d.name, Style::BODY.ellipsis(), TEXT);
            self.paint_word(cx, key, d, Rect::new(row.right() - 94.0, row.y, 94.0, 18.0), Style::CAPTION.weight(600).right().ellipsis());
            let kind = if d.size > 0 { format!("{} {}", d.media, fmt_size(d.size, cx.lang)).trim().to_string() } else { d.media.clone() };
            let t = self.glided("t", key, d.temp).map_or(na(cx).to_string(), |t| format!("{t} °C"));
            let w = self.glided("r", key, d.wear).map_or(na(cx).to_string(), |w| format!("{w} %"));
            let h = self.glided("h", key, d.hours.map(|h| h as u32)).map_or(na(cx).to_string(), |h| grouped_hours(cx, h as u64));
            let detail = format!("{}{}{} · {} {} · {}", kind, if kind.is_empty() { "" } else { " · " }, t, cx.t("износ", "wear"), w, h);
            cx.text(Rect::new(row.x + 14.0, row.y + 18.0, row.w - 14.0, 16.0), &detail, Style::CAPTION.ellipsis(), DIM);
        });
    }

    /// Everything that changes. `draw` calls it when nothing moves, `overlay` while something does.
    fn paint(&self, cx: &Cx) {
        let c = cx.content();
        let Some(worst) = self.disks.first() else {
            let msg = if self.loaded { cx.t("Нет данных о дисках", "No disk data") } else { cx.t("Проверка…", "Checking…") };
            cx.icon(Glyph::Disk, Rect::new(c.cx() - 18.0, c.y + 40.0, 36.0, 36.0), FAINT);
            cx.text(Rect::new(c.x, c.y + 84.0, c.w, 20.0), msg, Style::BODY.center().ellipsis(), DIM);
            return;
        };
        match cx.size {
            Size::M => {
                for (i, (k, d)) in self.keys.iter().zip(&self.disks).enumerate() {
                    let g = |p: &str, def: f32| self.slots.or(&format!("{p}:{k}"), def);
                    self.paint_m_row(cx, c, k, d, g("y", i as f32), g("a", 1.0));
                }
                for gh in &self.ghosts {
                    let g = |p: &str, def: f32| self.slots.or(&format!("{p}:{}", gh.key), def);
                    self.paint_m_row(cx, c, &gh.key, &gh.disk, g("y", 0.0), g("a", 0.0));
                }
                if self.disks.len() > 3 {
                    cx.text(Rect::new(c.right() - 60.0, c.y - 1.0, 60.0 - 22.0, 18.0), &format!("+{}", self.disks.len() - 3), Style::CAPTION.right().mono(), FAINT);
                }
            }
            _ => {
                let Some(k) = self.keys.first() else { return };
                let f = self.slots.or(&format!("s:{k}"), 1.0); // a new worst disk fades in
                cx.with_opacity(f, |cx| {
                    let col = colour_at(self.vals.or(&format!("c:{k}"), rank(worst.health)));
                    cx.circle((c.x + 5.0, c.y + 35.0), 4.0, col);
                    self.paint_word(cx, k, worst, Rect::new(c.x + 16.0, c.y + 22.0, c.w - 16.0, 26.0), Style::new(20.0, 600).ellipsis());
                    cx.text(Rect::new(c.x, c.y + 50.0, c.w, 18.0), &worst.name, Style::BODY.ellipsis(), TEXT);
                    let na = na(cx);
                    let rows = [
                        (cx.t("Температура", "Temperature"), self.glided("t", k, worst.temp).map(|t| format!("{t} °C"))),
                        (cx.t("Износ", "Wear"), self.glided("r", k, worst.wear).map(|w| format!("{w} %"))),
                        (cx.t("Наработка", "Power on"), self.glided("h", k, worst.hours.map(|h| h as u32)).map(|h| grouped_hours(cx, h as u64))),
                    ];
                    for (i, (label, v)) in rows.iter().enumerate() {
                        let y = c.y + 78.0 + i as f32 * 20.0;
                        cx.text(Rect::new(c.x, y, 74.0, 18.0), label, Style::CAPTION.ellipsis(), DIM);
                        cx.text(Rect::new(c.x + 74.0, y, c.w - 74.0, 18.0), v.as_deref().unwrap_or(na), Style::new(12.0, 500).right().ellipsis(), if v.is_some() { TEXT } else { FAINT });
                    }
                });
            }
        }
    }
}

impl Widget for DiskHealth {
    fn draw(&mut self, cx: &mut Cx) -> Result<()> {
        self.init(cx);
        let c = cx.content();
        cx.text(Rect::new(c.x, c.y - 1.0, c.w - 24.0, 20.0), cx.t("Здоровье дисков", "Disk health"), Style::TITLE.ellipsis(), SUBTLE);
        if !self.moving {
            self.paint(cx);
        }
        Ok(())
    }

    /// The moving part: a verdict cross-fading, numbers counting, rows entering and leaving.
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
        if let Some(j) = &self.job {
            match j.take() {
                Some(r) => {
                    self.job = None;
                    // a failed run keeps what was shown; the very first failure shows "no disk data"
                    if let Some(list) = r.filter(|l| !l.is_empty() || !self.loaded) {
                        if list != self.disks || !self.loaded {
                            let old = std::mem::replace(&mut self.disks, list);
                            self.ver += 1;
                            self.retarget(cx, old);
                        }
                    }
                    self.loaded = true;
                }
                None if !j.busy() => self.job = None,
                None => {}
            }
        }
        if self.job.is_none() && self.last.map_or(true, |t| t.elapsed() >= Duration::from_secs(59)) {
            self.last = Some(Instant::now());
            self.job = Some(cx.spawn(|| sys::powershell(SCRIPT).ok().map(|s| parse_disks(&s))));
        }
        let busy = self.slots.step(cx) | self.vals.step(cx);
        if busy != self.moving {
            self.moving = busy;
            self.ver += 1; // `draw` takes the dynamic parts over again (or hands them to `overlay`)
        }
        if !busy && (!self.ghosts.is_empty() || !self.old_health.is_empty()) {
            self.ghosts.clear();
            self.old_health.clear();
            let keep = self.keys.clone();
            let alive = |k: &str| k.split_once(':').is_some_and(|(_, n)| keep.iter().any(|x| x == n));
            self.slots.retain(alive);
            self.vals.retain(alive);
            self.ver += 1;
        }
        if busy {
            return Tick::Fps(60); // the verdict cross-fades, the numbers count, rows glide
        }
        Tick::Poll(Duration::from_secs(60))
    }

    fn version(&self) -> u64 {
        self.ver
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // what this script printed on a machine without administrator rights
    const NON_ADMIN: &str = r#"[{"name":"KINGSTON SKC3000D2048G","health":"Healthy","media":"SSD","size":2048408248320,"temp":null,"wear":null,"hours":null}]"#;

    #[test]
    fn reading_without_admin_rights_has_only_the_verdict() {
        let d = parse_disks(NON_ADMIN);
        assert_eq!(d.len(), 1);
        assert_eq!((d[0].name.as_str(), d[0].health, d[0].media.as_str(), d[0].size), ("KINGSTON SKC3000D2048G", Health::Healthy, "SSD", 2_048_408_248_320));
        assert_eq!((d[0].temp, d[0].wear, d[0].hours), (None, None, None));
    }

    #[test]
    fn full_reading_worst_disk_first() {
        let j = r#"[{"name":"A","health":"Healthy","media":"HDD","size":1000,"temp":35,"wear":0,"hours":100},
                    {"name":"B","health":"Unhealthy","media":"SSD","size":2000,"temp":61,"wear":97,"hours":23456},
                    {"name":"C","health":"Warning","media":"Unspecified","size":3000,"temp":0,"wear":101,"hours":-5}]"#;
        let d = parse_disks(j);
        assert_eq!(d.iter().map(|d| d.name.as_str()).collect::<Vec<_>>(), vec!["B", "C", "A"]);
        assert_eq!((d[0].temp, d[0].wear, d[0].hours), (Some(61), Some(97), Some(23_456)));
        // temperature 0 = no sensor, wear above 100 and negative hours are not believable
        assert_eq!((d[1].temp, d[1].wear, d[1].hours, d[1].media.as_str()), (None, None, None, ""));
        assert_eq!(d[2].wear, Some(0)); // a brand new disk is 0 %, not "n/a"
    }

    #[test]
    fn one_object_unknown_verdicts_and_garbage() {
        assert_eq!(parse_disks(r#"{"name":"Solo","health":"Healthy"}"#).len(), 1); // PowerShell prints one object without the array
        assert_eq!(parse_disks(r#"[{"name":"X","health":"Degraded"}]"#)[0].health, Health::Unknown);
        assert_eq!(parse_disks(r#"[{"name":"  \u0007 ","health":"Healthy"},{"health":"Healthy"},5,null,"x"]"#).len(), 0);
        for bad in ["", "null", "[]", "not json", "{", "[{\"name\":", "42", "\"Healthy\""] {
            assert!(parse_disks(bad).is_empty(), "{bad}");
        }
        assert_eq!(parse_disks("\u{feff}[{\"name\":\"Bom\"}]").len(), 1);
        let many = format!("[{}]", (0..20).map(|i| format!(r#"{{"name":"D{i}"}}"#)).collect::<Vec<_>>().join(","));
        assert_eq!(parse_disks(&many).len(), MAX_DISKS);
        assert_eq!(parse_disks(&format!(r#"[{{"name":"{}"}}]"#, "n".repeat(300)))[0].name.chars().count(), 40);
    }

    #[test]
    fn number_grouping() {
        assert_eq!(grouped(5), "5");
        assert_eq!(grouped(1234), "1\u{2009}234");
        assert_eq!(grouped(21_340), "21\u{2009}340");
        assert_eq!(grouped(1_234_567), "1\u{2009}234\u{2009}567");
    }

    /// Runs the real script: `cargo test diskhealth_live -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn diskhealth_live() {
        let t = Instant::now();
        let out = sys::powershell(SCRIPT).unwrap();
        println!("{:?}: {out}", t.elapsed());
        println!("{:?}", parse_disks(&out));
    }

    #[test]
    fn keys_ranks_and_colours() {
        let d = |n: &str| Disk { name: n.into(), health: Health::Healthy, media: String::new(), size: 0, temp: None, wear: None, hours: None };
        assert_eq!(disk_keys(&[d("A"), d("B"), d("A"), d("A")]), vec!["A", "B", "A#2", "A#3"]);
        assert_eq!((rank(Health::Healthy), rank(Health::Warning), rank(Health::Unhealthy), rank(Health::Unknown)), (0.0, 1.0, 2.0, 3.0));
        let near = |a: Color, b: Color| (a.r - b.r).abs() + (a.g - b.g).abs() + (a.b - b.b).abs() < 1e-4;
        assert!(near(colour_at(0.0), GREEN) && near(colour_at(1.0), ORANGE) && near(colour_at(2.0), RED) && near(colour_at(3.0), DIM) && near(colour_at(-4.0), GREEN));
        assert!(!near(colour_at(0.5), GREEN) && !near(colour_at(0.5), ORANGE), "a verdict change cross-fades");
    }

    #[test]
    fn a_verdict_change_glides_and_the_numbers_count() {
        use crate::motion::Glide;
        let mut g = Glide::default();
        g.set("c:x", rank(Health::Healthy));
        g.set("t:x", 40.0);
        g.set("c:x", rank(Health::Warning));
        g.set("t:x", 52.0);
        let mut seen_mid = false;
        let mut n = 0;
        while g.step(1.0 / 60.0) {
            let c = g.get("c:x").unwrap();
            seen_mid |= c > 0.2 && c < 0.8;
            assert!(g.get("t:x").unwrap() >= 40.0 && g.get("t:x").unwrap() <= 52.0);
            n += 1;
            assert!(n < 120);
        }
        assert!(seen_mid);
        assert_eq!((g.get("c:x"), g.get("t:x")), (Some(1.0), Some(52.0)));
    }
}
