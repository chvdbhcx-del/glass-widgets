//! Disk health: the Windows health verdict of every physical disk (Healthy / Warning / Unhealthy) with temperature, wear and power-on
//! hours where the system reports them. S = the worst disk, M = up to 3 disks as rows.
//! Data: one hidden PowerShell run (fixed script, `Get-PhysicalDisk | Get-StorageReliabilityCounter`, read-only, no elevation) on a
//! worker every 60 s. Without administrator rights Windows often leaves the reliability counters empty: those fields show "n/a" and
//! only the health verdict is real. Raw SMART attributes (DeviceIoControl) need admin and are NOT used.
//! Nothing is drawn per frame: a new reading bumps `version()`; previews use sample data and start no process.
use super::sysmon::fmt_size;
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

// ================================================================ widget

#[derive(Default)]
struct DiskHealth {
    disks: Vec<Disk>,
    /// a reading arrived (an empty list then means "no disks reported")
    loaded: bool,
    job: Option<Pending<Option<Vec<Disk>>>>,
    last: Option<Instant>,
    ver: u64,
    started: bool,
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

fn colour(h: Health) -> Color {
    match h {
        Health::Healthy => GREEN,
        Health::Warning => ORANGE,
        Health::Unhealthy => RED,
        Health::Unknown => DIM,
    }
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
fn temp_text(cx: &Cx, d: &Disk) -> String {
    d.temp.map_or(na(cx).to_string(), |t| format!("{t} °C"))
}
fn wear_text(cx: &Cx, d: &Disk) -> String {
    d.wear.map_or(na(cx).to_string(), |w| format!("{w} %"))
}
fn hours_text(cx: &Cx, d: &Disk) -> String {
    d.hours.map_or(na(cx).to_string(), |h| format!("{} {}", grouped(h), cx.t("ч", "h")))
}

impl DiskHealth {
    fn init(&mut self, cx: &Cx) {
        if self.started {
            return;
        }
        self.started = true;
        self.ver += 1;
        if cx.preview {
            self.disks = sample();
            self.loaded = true;
        }
    }
}

impl Widget for DiskHealth {
    fn draw(&mut self, cx: &mut Cx) -> Result<()> {
        self.init(cx);
        let c = cx.content();
        cx.text(Rect::new(c.x, c.y - 1.0, c.w - 24.0, 20.0), cx.t("Здоровье дисков", "Disk health"), Style::TITLE.ellipsis(), SUBTLE);
        let Some(worst) = self.disks.first() else {
            let msg = if self.loaded { cx.t("Нет данных о дисках", "No disk data") } else { cx.t("Проверка…", "Checking…") };
            cx.icon(Glyph::Disk, Rect::new(c.cx() - 18.0, c.y + 40.0, 36.0, 36.0), FAINT);
            cx.text(Rect::new(c.x, c.y + 84.0, c.w, 20.0), msg, Style::BODY.center().ellipsis(), DIM);
            return Ok(());
        };
        match cx.size {
            Size::M => {
                for (i, d) in self.disks.iter().take(3).enumerate() {
                    let row = Rect::new(c.x, c.y + 24.0 + i as f32 * 38.0, c.w, 36.0);
                    cx.circle((row.x + 4.0, row.y + 9.0), 3.5, colour(d.health));
                    cx.text(Rect::new(row.x + 14.0, row.y, row.w - 14.0 - 96.0, 18.0), &d.name, Style::BODY.ellipsis(), TEXT);
                    cx.text(Rect::new(row.right() - 94.0, row.y, 94.0, 18.0), word(cx, d.health), Style::CAPTION.weight(600).right().ellipsis(), colour(d.health));
                    let kind = if d.size > 0 { format!("{} {}", d.media, fmt_size(d.size, cx.lang)).trim().to_string() } else { d.media.clone() };
                    let detail = format!("{}{}{} · {} {} · {}", kind, if kind.is_empty() { "" } else { " · " }, temp_text(cx, d), cx.t("износ", "wear"), wear_text(cx, d), hours_text(cx, d));
                    cx.text(Rect::new(row.x + 14.0, row.y + 18.0, row.w - 14.0, 16.0), &detail, Style::CAPTION.ellipsis(), DIM);
                }
                if self.disks.len() > 3 {
                    cx.text(Rect::new(c.right() - 60.0, c.y - 1.0, 60.0 - 22.0, 18.0), &format!("+{}", self.disks.len() - 3), Style::CAPTION.right().mono(), FAINT);
                }
            }
            _ => {
                cx.circle((c.x + 5.0, c.y + 35.0), 4.0, colour(worst.health));
                cx.text(Rect::new(c.x + 16.0, c.y + 22.0, c.w - 16.0, 26.0), word(cx, worst.health), Style::new(20.0, 600).ellipsis(), colour(worst.health));
                cx.text(Rect::new(c.x, c.y + 50.0, c.w, 18.0), &worst.name, Style::BODY.ellipsis(), TEXT);
                let rows = [(cx.t("Температура", "Temperature"), temp_text(cx, worst)), (cx.t("Износ", "Wear"), wear_text(cx, worst)), (cx.t("Наработка", "Power on"), hours_text(cx, worst))];
                for (i, (k, v)) in rows.iter().enumerate() {
                    let y = c.y + 78.0 + i as f32 * 20.0;
                    cx.text(Rect::new(c.x, y, 74.0, 18.0), k, Style::CAPTION.ellipsis(), DIM);
                    cx.text(Rect::new(c.x + 74.0, y, c.w - 74.0, 18.0), v, Style::new(12.0, 500).right().ellipsis(), if v == na(cx) { FAINT } else { TEXT });
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
        if let Some(j) = &self.job {
            match j.take() {
                Some(r) => {
                    self.job = None;
                    // a failed run keeps what was shown; the very first failure shows "no disk data"
                    if let Some(list) = r.filter(|l| !l.is_empty() || !self.loaded) {
                        if list != self.disks || !self.loaded {
                            self.disks = list;
                            self.ver += 1;
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
}
