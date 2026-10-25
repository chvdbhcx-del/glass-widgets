//! Weather: S city + big temperature + condition + H/L; M current block + 5 hourly columns; L current block + 7 hourly
//! columns + 5-day list with gradient range bars. Data: Open-Meteo (keyless), refreshed every 15 min in a `cx.spawn` job.
//! Settings: `cfg.city` (string, optional; geocoded via Open-Meteo, empty = auto-detect once by IP through ipwho.is),
//! `cfg.units` ("c" default | "f"). Clicking the card asks for a city (`cx.prompt_text`) and stores it into `cfg.city`.
//! Files (%APPDATA%\macshell\cache): `weather_loc.json` (resolved city/auto -> lat/lon/name), `weather_<lat>_<lon>.json`
//! (last good forecast, shown instantly at start; offline keeps it with a dim "нет сети").
//! Core gap: `tick` only gets `&Cx`, so it cannot write `cfg`; a city typed in the prompt is flushed to `cfg.city` on the
//! next mouse event (and is kept in memory meanwhile), lat/lon of auto-detect live in weather_loc.json instead of cfg.
use crate::registry::{Registry, WidgetDef};
use crate::widget::*;
use serde_json::Value as Json;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

type R<T> = std::result::Result<T, String>;
const REFRESH_SECS: i64 = 900;
const RETRY_SECS: i64 = 120;

pub fn register(r: &mut Registry) {
    r.register(WidgetDef {
        type_id: "weather",
        name: ("Погода", "Weather"),
        desc: ("Текущая погода и прогноз", "Current weather and forecast"),
        group: "weather",
        icon: Glyph::PartlyCloudy,
        sizes: &[Size::S, Size::M, Size::L],
        default_size: Size::M,
        make: || Box::new(Weather::default()),
    });
}

// ================================================================ data model + pure logic

#[derive(Clone, Debug, PartialEq)]
struct Hour {
    /// local hours since the unix epoch (days_from_civil * 24 + hour)
    key: i64,
    hh: u32,
    temp: f32,
    code: u8,
    day: bool,
}
#[derive(Clone, Debug, PartialEq)]
struct Day {
    /// local days since the unix epoch
    key: i64,
    /// 0 = Monday
    dow: u32,
    code: u8,
    lo: f32,
    hi: f32,
}
#[derive(Clone, Debug, Default, PartialEq)]
struct Wx {
    temp: f32,
    code: u8,
    day: bool,
    /// UTC offset of the place in seconds (Open-Meteo `timezone=auto`)
    off: i32,
    hours: Vec<Hour>,
    days: Vec<Day>,
}

/// "YYYY-MM-DD" or "YYYY-MM-DDTHH:MM" -> (days since epoch, hour)
fn split_time(s: &str) -> Option<(i64, u32)> {
    let (d, t) = match s.split_once('T') {
        Some((d, t)) => (d, Some(t)),
        None => (s, None),
    };
    let mut it = d.split('-');
    let y: i32 = it.next()?.parse().ok()?;
    let m: u32 = it.next()?.parse().ok()?;
    let dd: u32 = it.next()?.parse().ok()?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&dd) {
        return None;
    }
    let h: u32 = match t {
        Some(t) => t.get(..2)?.parse().ok()?,
        None => 0,
    };
    Some((tz::days_from_civil(y, m, dd), h))
}

fn code_of(v: Option<&Json>) -> u8 {
    v.and_then(Json::as_u64).map_or(255, |c| c.min(255) as u8)
}

/// Open-Meteo forecast response -> `Wx` (None when the mandatory parts are missing). Null entries are skipped.
fn parse_wx(v: &Json) -> Option<Wx> {
    let cur = v.get("current")?;
    let temp = cur.get("temperature_2m")?.as_f64()? as f32;
    let code = code_of(cur.get("weather_code"));
    let day = cur.get("is_day").and_then(Json::as_i64).map_or(true, |d| d != 0);
    let off = v.get("utc_offset_seconds").and_then(Json::as_i64).unwrap_or(0) as i32;
    let mut hours = Vec::new();
    if let Some(h) = v.get("hourly") {
        if let (Some(times), Some(temps), Some(codes)) = (h.get("time").and_then(Json::as_array), h.get("temperature_2m").and_then(Json::as_array), h.get("weather_code").and_then(Json::as_array)) {
            let isd = h.get("is_day").and_then(Json::as_array);
            hours = times
                .iter()
                .enumerate()
                .filter_map(|(i, t)| {
                    let (d, hh) = split_time(t.as_str()?)?;
                    Some(Hour {
                        key: d * 24 + hh as i64,
                        hh,
                        temp: temps.get(i)?.as_f64()? as f32,
                        code: code_of(codes.get(i)),
                        day: isd.and_then(|a| a.get(i)).and_then(Json::as_i64).map_or((6..19).contains(&hh), |x| x != 0),
                    })
                })
                .collect();
        }
    }
    let mut days = Vec::new();
    if let Some(d) = v.get("daily") {
        if let (Some(times), Some(his), Some(los), Some(codes)) = (
            d.get("time").and_then(Json::as_array),
            d.get("temperature_2m_max").and_then(Json::as_array),
            d.get("temperature_2m_min").and_then(Json::as_array),
            d.get("weather_code").and_then(Json::as_array),
        ) {
            days = times
                .iter()
                .enumerate()
                .filter_map(|(i, t)| {
                    let (key, _) = split_time(t.as_str()?)?;
                    Some(Day { key, dow: (key + 3).rem_euclid(7) as u32, code: code_of(codes.get(i)), hi: his.get(i)?.as_f64()? as f32, lo: los.get(i)?.as_f64()? as f32 })
                })
                .collect();
        }
    }
    Some(Wx { temp, code, day, off, hours, days })
}

/// First hourly / daily entry that is not in the past at UTC instant `utc` (in the place's local time).
fn view_idx(wx: &Wx, utc: i64) -> (usize, usize) {
    let l = utc + wx.off as i64;
    let (hk, dk) = (l.div_euclid(3600), l.div_euclid(86400));
    (wx.hours.iter().position(|h| h.key >= hk).unwrap_or(wx.hours.len()), wx.days.iter().position(|d| d.key >= dk).unwrap_or(wx.days.len()))
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Kind {
    Clear,
    Partly,
    Cloud,
    Fog,
    Rain,
    Snow,
    Storm,
}

/// WMO weather code -> (icon kind, Russian text, English text). Clear sky differs by day/night.
fn wmo(code: u8, day: bool) -> (Kind, &'static str, &'static str) {
    match code {
        0 if day => (Kind::Clear, "Солнечно", "Sunny"),
        0 => (Kind::Clear, "Ясно", "Clear"),
        1 => (Kind::Partly, "Малооблачно", "Mostly clear"),
        2 => (Kind::Partly, "Облачно с прояснениями", "Partly cloudy"),
        3 => (Kind::Cloud, "Пасмурно", "Overcast"),
        45 | 48 => (Kind::Fog, "Туман", "Fog"),
        51 | 53 | 55 => (Kind::Rain, "Морось", "Drizzle"),
        56 | 57 => (Kind::Rain, "Ледяная морось", "Freezing drizzle"),
        61 => (Kind::Rain, "Небольшой дождь", "Light rain"),
        63 => (Kind::Rain, "Дождь", "Rain"),
        65 => (Kind::Rain, "Сильный дождь", "Heavy rain"),
        66 | 67 => (Kind::Rain, "Ледяной дождь", "Freezing rain"),
        71 => (Kind::Snow, "Небольшой снег", "Light snow"),
        73 => (Kind::Snow, "Снег", "Snow"),
        75 => (Kind::Snow, "Сильный снег", "Heavy snow"),
        77 => (Kind::Snow, "Снежная крупа", "Snow grains"),
        80 => (Kind::Rain, "Небольшие ливни", "Light showers"),
        81 => (Kind::Rain, "Ливень", "Showers"),
        82 => (Kind::Rain, "Сильный ливень", "Heavy showers"),
        85 | 86 => (Kind::Snow, "Снегопад", "Snow showers"),
        95 => (Kind::Storm, "Гроза", "Thunderstorm"),
        96 | 99 => (Kind::Storm, "Гроза с градом", "Thunderstorm with hail"),
        _ => (Kind::Cloud, "Нет данных", "No data"),
    }
}

/// "7°" / "−3°" (U+2212), Celsius input, optional Fahrenheit output.
fn fmt_t(c: f32, f: bool) -> String {
    let v = if f { c * 1.8 + 32.0 } else { c };
    let r = v.round() as i32;
    if r < 0 {
        format!("−{}°", -r)
    } else {
        format!("{r}°")
    }
}

/// Percent-encode a query value (UTF-8 bytes, RFC 3986 unreserved kept).
fn url_enc(s: &str) -> String {
    let mut o = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            o.push(b as char);
        } else {
            o.push_str(&format!("%{b:02X}"));
        }
    }
    o
}

/// geocoding-api response -> (lat, lon, name)
fn parse_geo(v: &Json) -> Option<(f64, f64, String)> {
    let r = v.get("results")?.as_array()?.first()?;
    Some((r.get("latitude")?.as_f64()?, r.get("longitude")?.as_f64()?, r.get("name")?.as_str()?.to_string()))
}

/// ipwho.is response -> (lat, lon, city)
fn parse_ip(v: &Json) -> Option<(f64, f64, String)> {
    if v.get("success").and_then(Json::as_bool) == Some(false) {
        return None;
    }
    Some((v.get("latitude")?.as_f64()?, v.get("longitude")?.as_f64()?, v.get("city")?.as_str()?.to_string()))
}

fn forecast_url(lat: f64, lon: f64) -> String {
    format!("https://api.open-meteo.com/v1/forecast?latitude={lat:.4}&longitude={lon:.4}&current=temperature_2m,weather_code,is_day&hourly=temperature_2m,weather_code,is_day&daily=weather_code,temperature_2m_max,temperature_2m_min&timezone=auto&forecast_days=6")
}

fn temp_color(c: f32) -> Color {
    const S: [(f32, Color); 5] = [(-15.0, Color::rgb8(90, 150, 255)), (0.0, Color::rgb8(70, 200, 230)), (12.0, Color::rgb8(120, 220, 130)), (22.0, Color::rgb8(255, 214, 10)), (34.0, Color::rgb8(255, 100, 50))];
    if c <= S[0].0 {
        return S[0].1;
    }
    for w in S.windows(2) {
        if c <= w[1].0 {
            return w[0].1.mix(w[1].1, (c - w[0].0) / (w[1].0 - w[0].0));
        }
    }
    S[4].1
}

fn dow_short(l: Lang, dow: u32) -> &'static str {
    const RU: [&str; 7] = ["Пн", "Вт", "Ср", "Чт", "Пт", "Сб", "Вс"];
    const EN: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
    (if l == Lang::Ru { &RU } else { &EN })[(dow % 7) as usize]
}

// ================================================================ worker side: location, files, network

#[derive(Clone, Debug, PartialEq)]
enum Want {
    City(String),
    Auto,
}
impl Want {
    fn key(&self) -> String {
        match self {
            Want::City(c) => format!("city:{}", c.to_lowercase()),
            Want::Auto => "auto".into(),
        }
    }
}

struct Loc {
    lat: f64,
    lon: f64,
    name: String,
    auto: bool,
}
struct Loaded {
    key: String,
    loc: Loc,
    raw: Json,
    fetched: i64,
}

fn unix_now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}
fn cache_dir() -> Option<PathBuf> {
    Some(PathBuf::from(std::env::var_os("APPDATA")?).join("macshell").join("cache"))
}
fn write_file(name: &str, v: &Json) {
    if let Some(dir) = cache_dir() {
        let _ = std::fs::create_dir_all(&dir);
        let (tmp, dst) = (dir.join(format!("{name}.tmp")), dir.join(name));
        if let Ok(b) = serde_json::to_vec(v) {
            if std::fs::write(&tmp, b).is_ok() {
                let _ = std::fs::rename(&tmp, &dst);
            }
        }
    }
}
fn read_file(name: &str) -> Option<Json> {
    serde_json::from_slice(&std::fs::read(cache_dir()?.join(name)).ok()?).ok()
}
fn wx_file(lat: f64, lon: f64) -> String {
    format!("weather_{lat:.2}_{lon:.2}.json")
}

/// Location previously resolved for `key` (auto-detect results expire after a week).
fn read_loc(key: &str, auto: bool) -> Option<Loc> {
    let v = read_file("weather_loc.json")?;
    if v.get("key")?.as_str()? != key || (auto && unix_now() - v.get("at")?.as_i64()? > 7 * 86400) {
        return None;
    }
    Some(Loc { lat: v.get("lat")?.as_f64()?, lon: v.get("lon")?.as_f64()?, name: v.get("name")?.as_str()?.to_string(), auto })
}

fn geocode(q: &str, ru: bool) -> R<(f64, f64, String)> {
    let url = format!("https://geocoding-api.open-meteo.com/v1/search?name={}&count=1&language={}&format=json", url_enc(q), if ru { "ru" } else { "en" });
    parse_geo(&crate::net::http_get_json(&url, 8000)?).ok_or_else(|| "not found".to_string())
}

/// Resolve what the user wants into coordinates (disk cache first, then the network).
fn resolve(w: &Want, ru: bool) -> R<Loc> {
    let (key, auto) = (w.key(), matches!(w, Want::Auto));
    if let Some(l) = read_loc(&key, auto) {
        return Ok(l);
    }
    let (lat, lon, name) = match w {
        Want::City(c) => geocode(c, ru)?,
        Want::Auto => {
            let (lat, lon, city) = parse_ip(&crate::net::http_get_json("https://ipwho.is/", 8000)?).ok_or_else(|| "ip lookup failed".to_string())?;
            // ipwho names are English: prefer the localized name of the same place
            match geocode(&city, ru) {
                Ok((la, lo, n)) if (la - lat).abs() < 1.5 && (lo - lon).abs() < 2.5 => (lat, lon, n),
                _ => (lat, lon, city),
            }
        }
    };
    write_file("weather_loc.json", &json!({"key": key, "lat": lat, "lon": lon, "name": name, "at": unix_now()}));
    Ok(Loc { lat, lon, name, auto })
}

/// Instant start: last good forecast from disk, no network.
fn load_cached(w: &Want) -> Option<Loaded> {
    let loc = read_loc(&w.key(), matches!(w, Want::Auto))?;
    let v = read_file(&wx_file(loc.lat, loc.lon))?;
    Some(Loaded { key: w.key(), raw: v.get("raw")?.clone(), fetched: v.get("fetched")?.as_i64()?, loc })
}

fn fetch(w: &Want, ru: bool) -> R<Loaded> {
    let loc = resolve(w, ru)?;
    let raw = crate::net::http_get_json(&forecast_url(loc.lat, loc.lon), 10_000)?;
    parse_wx(&raw).ok_or_else(|| "bad forecast".to_string())?;
    let fetched = unix_now();
    write_file(&wx_file(loc.lat, loc.lon), &json!({"fetched": fetched, "raw": raw}));
    Ok(Loaded { key: w.key(), loc, raw, fetched })
}

// ================================================================ the widget

#[derive(Default)]
struct Weather {
    ver: u64,
    started: bool,
    wx: Option<Wx>,
    place: String,
    auto: bool,
    /// view window: first hour / day that is not in the past
    hs: usize,
    ds: usize,
    hour_key: i64,
    fetched: i64,
    next_try: i64,
    offline: bool,
    failed: bool,
    not_found: bool,
    cur_key: String,
    cache_job: Option<Pending<Option<Loaded>>>,
    net_job: Option<Pending<R<Loaded>>>,
    prompt: Option<Pending<Option<String>>>,
    /// typed in the prompt, not yet in cfg
    city_override: Option<String>,
    flush_city: Option<String>,
}

impl Weather {
    fn want(&self, cx: &Cx) -> Want {
        let c = self.city_override.clone().or_else(|| cx.cfg_str("city").map(str::to_string)).unwrap_or_default();
        let c = c.trim();
        if c.is_empty() {
            Want::Auto
        } else {
            Want::City(c.to_string())
        }
    }

    fn apply(&mut self, l: Loaded) {
        if l.key != self.cur_key || (self.wx.is_some() && l.fetched < self.fetched) {
            return;
        }
        if let Some(wx) = parse_wx(&l.raw) {
            self.wx = Some(wx);
            self.place = l.loc.name;
            self.auto = l.loc.auto;
            self.fetched = l.fetched;
            self.hour_key = i64::MIN; // force the window recompute
            self.ver += 1;
        }
    }

    fn load_sample(&mut self, now: &Now) {
        let h0 = now.h as i64;
        let d0 = tz::days_from_civil(now.y, now.mo, now.d);
        const T: [f32; 24] = [6.8, 7.4, 8.1, 8.7, 8.9, 8.4, 7.6, 6.9, 6.2, 5.6, 5.0, 4.5, 4.0, 3.6, 3.4, 3.3, 3.5, 4.2, 5.0, 5.8, 6.4, 6.9, 7.2, 7.4];
        const C: [u8; 24] = [2, 2, 3, 3, 61, 61, 3, 2, 2, 2, 1, 1, 0, 0, 1, 2, 2, 3, 3, 3, 61, 61, 3, 3];
        let hours = (0..24i64)
            .map(|i| {
                let hh = ((h0 + i) % 24) as u32;
                Hour { key: d0 * 24 + h0 + i, hh, temp: T[i as usize], code: C[i as usize], day: (7..17).contains(&hh) }
            })
            .collect();
        const D: [(u8, f32, f32); 6] = [(2, 3.0, 8.9), (61, 2.2, 7.1), (3, 1.0, 6.0), (0, -0.8, 4.9), (71, -2.4, 1.1), (3, -1.0, 3.0)];
        let days = D.iter().enumerate().map(|(i, &(code, lo, hi))| Day { key: d0 + i as i64, dow: (d0 + i as i64 + 3).rem_euclid(7) as u32, code, lo, hi }).collect();
        self.wx = Some(Wx { temp: 6.8, code: 2, day: (7..17).contains(&now.h), off: 0, hours, days });
        self.place = "Москва".into();
        self.auto = true;
    }

    // ---------------------------------------------------------------- drawing

    fn empty(&self, cx: &Cx) {
        let b = cx.content();
        let (mx, my) = b.center();
        cx.icon(Glyph::PartlyCloudy, Rect::new(mx - 16.0, my - 34.0, 32.0, 32.0), FAINT);
        let (msg, sub) = if self.not_found {
            (cx.t("Город не найден", "City not found"), cx.t("Нажмите, чтобы изменить", "Click to change"))
        } else if self.failed {
            (cx.t("Нет данных", "No data"), cx.t("Проверьте подключение", "Check your connection"))
        } else {
            (cx.t("Загрузка…", "Loading…"), "")
        };
        cx.text(Rect::new(b.x, my + 4.0, b.w, 18.0), msg, Style::BODY.center().ellipsis(), DIM);
        cx.text(Rect::new(b.x, my + 22.0, b.w, 16.0), sub, Style::CAPTION.center().ellipsis(), FAINT);
    }

    /// City name (+ location arrow when auto-detected) and the dim "нет сети" caption on the right.
    fn city_row(&self, cx: &Cx, r: Rect) {
        let room = if self.offline { 56.0 } else { 0.0 };
        let tw = r.w - room;
        cx.text(Rect::new(r.x, r.y, tw, r.h), &self.place, Style::TITLE.ellipsis(), TEXT);
        if self.auto {
            let (w, _) = cx.measure(&self.place, Style::TITLE, 0.0);
            if w + 18.0 < tw {
                cx.icon(Glyph::Location, Rect::new(r.x + w + 4.0, r.y + 3.0, 12.0, 12.0), TEXT);
            }
        }
        if self.offline {
            cx.text(Rect::new(r.right() - 56.0, r.y, 56.0, r.h), cx.t("нет сети", "offline"), Style::CAPTION.right(), FAINT);
        }
    }

    /// S and the left half of M: city, big temperature, then glyph / condition / H:L anchored to the bottom.
    fn left_block(&self, cx: &Cx, wx: &Wx, today: Option<&Day>, x: f32, w: f32, f: bool) {
        let bottom = 154.0;
        self.city_row(cx, Rect::new(x, 16.0, w, 18.0));
        cx.text(Rect::new(x - 1.0, 32.0, w + 2.0, 56.0), &fmt_t(wx.temp, f), Style::BIG.size(50.0).track(-1.0), TEXT);
        let (kind, ru, en) = wmo(wx.code, wx.day);
        draw_kind(cx, kind, wx.day, Rect::new(x, bottom - 56.0, 22.0, 22.0), TEXT);
        let cond = cx.t(ru, en);
        cx.text(Rect::new(x, bottom - 34.0, w, 17.0), cond, fit_cond(cx, cond, w).ellipsis(), TEXT);
        if let Some(d) = today {
            cx.text(Rect::new(x, bottom - 16.0, w, 16.0), &hl_text(cx, d, f), Style::new(13.0, 500).ellipsis(), TEXT);
        }
    }

    /// One hourly column: label on top, glyph, temperature.
    #[allow(clippy::too_many_arguments)]
    fn hour_col(&self, cx: &Cx, c: Rect, label: &str, first: bool, kind: Kind, day: bool, temp: &str, ys: (f32, f32, f32), icon: f32) {
        let st = if first { Style::new(11.0, 600) } else { Style::new(11.0, 500) };
        cx.text(Rect::new(c.x - 4.0, ys.0, c.w + 8.0, 14.0), label, st.center(), if first { TEXT } else { DIM });
        draw_kind(cx, kind, day, Rect::new(c.cx() - icon / 2.0, ys.1 - icon / 2.0, icon, icon), TEXT);
        cx.text(Rect::new(c.x - 4.0, ys.2, c.w + 8.0, 18.0), temp, Style::new(15.0, 500).center(), TEXT);
    }

    fn hours_row(&self, cx: &Cx, wx: &Wx, hours: &[Hour], area: Rect, n: u32, f: bool, ys: (f32, f32, f32), icon: f32) {
        for i in 0..n {
            let c = area.col(i, n, 0.0);
            let Some(h) = hours.get(i as usize) else { break };
            if i == 0 {
                let (k, _, _) = wmo(wx.code, wx.day);
                self.hour_col(cx, c, cx.t("Сейчас", "Now"), true, k, wx.day, &fmt_t(wx.temp, f), ys, icon);
            } else {
                let (k, _, _) = wmo(h.code, h.day);
                self.hour_col(cx, c, &h.hh.to_string(), false, k, h.day, &fmt_t(h.temp, f), ys, icon);
            }
        }
    }

    fn days_list(&self, cx: &Cx, wx: &Wx, days: &[Day], f: bool) {
        let shown: Vec<&Day> = days.iter().take(5).collect();
        let gmin = shown.iter().map(|d| d.lo).fold(f32::MAX, f32::min);
        let gmax = shown.iter().map(|d| d.hi).fold(f32::MIN, f32::max);
        let span = (gmax - gmin).max(1.0);
        for (i, d) in shown.iter().enumerate() {
            let y = 180.0 + i as f32 * 32.0;
            let name = if i == 0 { cx.t("Сегодня", "Today") } else { dow_short(cx.lang, d.dow) };
            cx.text(Rect::new(16.0, y, 62.0, 32.0), name, Style::new(14.0, 500).ellipsis(), TEXT);
            let (k, _, _) = wmo(d.code, true);
            draw_kind(cx, k, true, Rect::new(82.0, y + 6.0, 20.0, 20.0), TEXT);
            cx.text(Rect::new(106.0, y, 36.0, 32.0), &fmt_t(d.lo, f), Style::new(14.0, 400).mono().right(), DIM);
            let bar = Rect::new(150.0, y + 14.0, 148.0, 4.0);
            cx.pill(bar, FILL);
            let x0 = bar.x + bar.w * (d.lo - gmin) / span;
            let x1 = bar.x + bar.w * (d.hi - gmin) / span;
            cx.gradient_rrect(Rect::new(x0, bar.y, (x1 - x0).max(4.0), 4.0), 2.0, temp_color(d.lo), temp_color(d.hi), 90.0);
            if i == 0 {
                let px = (bar.x + bar.w * (wx.temp - gmin) / span).clamp(x0 + 1.0, x1.max(x0 + 4.0) - 1.0);
                cx.circle((px, bar.cy()), 3.6, Color::rgba(0.0, 0.0, 0.0, 0.28));
                cx.circle((px, bar.cy()), 2.7, WHITE);
            }
            cx.text(Rect::new(304.0, y, 36.0, 32.0), &fmt_t(d.hi, f), Style::new(14.0, 500).mono().right(), TEXT);
            if i + 1 < shown.len() {
                cx.rect(Rect::new(16.0, y + 31.7, 324.0, 0.6), HAIR.fade(0.6));
            }
        }
    }
}

/// Condition text style: 13 semibold, stepping down to 11 until it fits `w`.
fn fit_cond(cx: &Cx, s: &str, w: f32) -> Style {
    let st = Style::new(13.0, 600);
    [13.0, 12.0, 11.5, 11.0].iter().map(|&z| st.size(z)).find(|&t| cx.measure(s, t, 0.0).0 <= w).unwrap_or(st.size(11.0))
}

fn hl_text(cx: &Cx, d: &Day, f: bool) -> String {
    if cx.lang == Lang::Ru {
        format!("Макс.: {}, мин.: {}", fmt_t(d.hi, f), fmt_t(d.lo, f))
    } else {
        format!("H:{} L:{}", fmt_t(d.hi, f), fmt_t(d.lo, f))
    }
}

/// Weather glyph for a kind (own night / fog art where icons.rs has no glyph).
fn draw_kind(cx: &Cx, kind: Kind, day: bool, r: Rect, c: Color) {
    match (kind, day) {
        (Kind::Clear, true) => cx.icon(Glyph::Sun, r, c),
        (Kind::Clear, false) => cx.icon(Glyph::Moon, r, c),
        (Kind::Partly, true) => cx.icon(Glyph::PartlyCloudy, r, c),
        (Kind::Partly, false) => {
            cx.icon(Glyph::Moon, Rect::new(r.x - r.w * 0.04, r.y - r.h * 0.04, r.w * 0.58, r.h * 0.58), c);
            cx.icon(Glyph::Cloud, Rect::new(r.x + r.w * 0.2, r.y + r.h * 0.26, r.w * 0.8, r.h * 0.8), c);
        }
        (Kind::Cloud, _) => cx.icon(Glyph::Cloud, r, c),
        (Kind::Rain, _) => cx.icon(Glyph::Rain, r, c),
        (Kind::Snow, _) => cx.icon(Glyph::Snow, r, c),
        (Kind::Storm, _) => cx.icon(Glyph::Thunder, r, c),
        (Kind::Fog, _) => {
            let k = r.w.min(r.h) / 24.0;
            let (ox, oy) = (r.cx() - 12.0 * k, r.cy() - 12.0 * k);
            for &(y, x0, x1) in &[(6.5f32, 6.0f32, 18.0f32), (10.5, 3.5, 20.5), (14.5, 5.0, 19.0), (18.5, 8.0, 16.0)] {
                cx.line((ox + x0 * k, oy + y * k), (ox + x1 * k, oy + y * k), 1.9 * k, c);
            }
        }
    }
}

impl Widget for Weather {
    fn draw(&mut self, cx: &mut Cx) -> Result<()> {
        let Some(wx) = &self.wx else {
            self.empty(cx);
            return Ok(());
        };
        let f = cx.cfg_str("units").is_some_and(|u| u.eq_ignore_ascii_case("f"));
        let hours = wx.hours.get(self.hs..).unwrap_or(&[]);
        let days = wx.days.get(self.ds..).unwrap_or(&[]);
        match cx.size {
            Size::S => self.left_block(cx, wx, days.first(), 16.0, 138.0, f),
            Size::M => {
                self.left_block(cx, wx, days.first(), 16.0, 140.0, f);
                self.hours_row(cx, wx, hours, Rect::new(168.0, 16.0, 172.0, 138.0), 5, f, (16.0, 86.0, 136.0), 22.0);
            }
            Size::L => {
                self.city_row(cx, Rect::new(16.0, 16.0, 324.0, 18.0));
                cx.text(Rect::new(15.0, 32.0, 170.0, 62.0), &fmt_t(wx.temp, f), Style::BIG.size(54.0).track(-1.2), TEXT);
                let (kind, ru, en) = wmo(wx.code, wx.day);
                draw_kind(cx, kind, wx.day, Rect::new(340.0 - 26.0, 36.0, 26.0, 26.0), TEXT);
                let cond = cx.t(ru, en);
                cx.text(Rect::new(190.0, 64.0, 150.0, 16.0), cond, fit_cond(cx, cond, 150.0).right().ellipsis(), TEXT);
                if let Some(d) = days.first() {
                    cx.text(Rect::new(190.0, 80.0, 150.0, 16.0), &hl_text(cx, d, f), Style::new(13.0, 500).right().ellipsis(), TEXT);
                }
                cx.rect(Rect::new(16.0, 101.0, 324.0, 0.6), FILL);
                self.hours_row(cx, wx, hours, Rect::new(16.0, 104.0, 324.0, 66.0), 7, f, (106.0, 136.0, 150.0), 22.0);
                cx.rect(Rect::new(16.0, 175.0, 324.0, 0.6), FILL);
                self.days_list(cx, wx, days, f);
            }
        }
        Ok(())
    }

    fn tick(&mut self, cx: &Cx) -> Tick {
        if cx.preview {
            if !self.started {
                self.started = true;
                self.load_sample(&cx.now);
                self.ver += 1;
            }
            return Tick::Never;
        }
        self.started = true;
        let utc = cx.utc_secs();
        // the city typed in the prompt reached cfg -> forget the in-memory copy
        if self.city_override.as_deref().is_some_and(|c| cx.cfg_str("city") == Some(c)) {
            self.city_override = None;
        }
        // the card asked for a new city (prompt result)
        let answer = self.prompt.as_ref().map(|p| (!p.busy(), p.take()));
        if let Some((done, res)) = answer {
            if res.is_some() || done {
                self.prompt = None;
            }
            if let Some(Some(text)) = res {
                let t = text.trim().chars().take(60).collect::<String>();
                if !t.is_empty() {
                    self.city_override = Some(t.clone());
                    self.flush_city = Some(t);
                }
            }
        }
        let want = self.want(cx);
        let key = want.key();
        if key != self.cur_key {
            if !self.cur_key.is_empty() {
                self.wx = None;
                self.place.clear();
                self.fetched = 0;
                self.offline = false;
                self.failed = false;
                self.not_found = false;
                self.ver += 1;
            }
            self.cur_key = key;
            self.net_job = None;
            self.next_try = 0;
            let w = want.clone();
            self.cache_job = Some(cx.spawn(move || load_cached(&w)));
        }
        let cached = self.cache_job.as_ref().map(|j| (!j.busy(), j.take()));
        if let Some((done, res)) = cached {
            if res.is_some() || done {
                self.cache_job = None;
            }
            if let Some(Some(l)) = res {
                if self.wx.is_none() {
                    self.apply(l);
                }
            }
        }
        let net = self.net_job.as_ref().map(|j| (!j.busy(), j.take()));
        if let Some((done, res)) = net {
            match res {
                Some(Ok(l)) => {
                    self.net_job = None;
                    self.next_try = utc + REFRESH_SECS;
                    if self.offline || self.failed || self.not_found {
                        self.offline = false;
                        self.failed = false;
                        self.not_found = false;
                        self.ver += 1;
                    }
                    self.apply(l);
                }
                Some(Err(e)) => {
                    self.net_job = None;
                    let nf = e == "not found";
                    self.next_try = utc + if nf { 600 } else { RETRY_SECS };
                    self.offline = !nf;
                    if self.wx.is_none() {
                        self.failed = true;
                        self.not_found = nf;
                    }
                    self.ver += 1;
                }
                None if done => {
                    self.net_job = None;
                    self.next_try = utc + RETRY_SECS;
                }
                None => {}
            }
        }
        if self.net_job.is_none() && utc >= self.next_try {
            let (w, ru) = (want, cx.lang == Lang::Ru);
            self.net_job = Some(cx.spawn(move || fetch(&w, ru)));
        }
        // roll the hourly / daily window forward as the local hour changes
        if let Some(wx) = &self.wx {
            let hk = (utc + wx.off as i64).div_euclid(3600);
            if hk != self.hour_key {
                self.hour_key = hk;
                let (hs, ds) = view_idx(wx, utc);
                if (hs, ds) != (self.hs, self.ds) {
                    self.hs = hs;
                    self.ds = ds;
                }
                self.ver += 1;
            }
        }
        Tick::Poll(Duration::from_secs(60))
    }

    fn mouse(&mut self, ev: &MouseEv, cx: &mut Cx) -> bool {
        if let Some(c) = self.flush_city.take() {
            cx.cfg_set("city", json!(c));
        }
        if ev.clicked(cx.bounds()) && self.prompt.as_ref().map_or(true, |p| !p.busy()) {
            let cur = self.city_override.clone().or_else(|| cx.cfg_str("city").map(str::to_string)).unwrap_or_else(|| self.place.clone());
            self.prompt = Some(cx.prompt_text(cx.t("Город", "City"), &cur, false));
        }
        false
    }

    fn version(&self) -> u64 {
        self.ver
    }
}

// ================================================================ tests

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{"latitude":55.75,"longitude":37.625,"utc_offset_seconds":10800,"timezone":"Europe/Moscow",
      "current":{"time":"2026-10-24T10:00","interval":900,"temperature_2m":6.4,"weather_code":3,"is_day":1},
      "hourly":{"time":["2026-10-24T09:00","2026-10-24T10:00","2026-10-24T11:00","2026-10-25T00:00"],"temperature_2m":[5.9,6.4,null,3.0],"weather_code":[3,3,61,0],"is_day":[1,1,1,0]},
      "daily":{"time":["2026-10-24","2026-10-25"],"weather_code":[3,61],"temperature_2m_max":[8.9,7.1],"temperature_2m_min":[3.0,2.2]}}"#;

    #[test]
    fn parses_forecast() {
        let v: Json = serde_json::from_str(SAMPLE).unwrap();
        let wx = parse_wx(&v).unwrap();
        assert_eq!((wx.temp, wx.code, wx.day, wx.off), (6.4, 3, true, 10800));
        assert_eq!(wx.hours.len(), 3); // the null temperature is skipped
        assert_eq!(wx.hours[2].hh, 0);
        assert!(!wx.hours[2].day);
        assert_eq!(wx.days.len(), 2);
        assert_eq!(wx.days[0].dow, 5); // Saturday
        assert_eq!((wx.days[1].hi, wx.days[1].lo), (7.1, 2.2));
        assert!(parse_wx(&json!({"current": {}})).is_none());
    }

    #[test]
    fn window_follows_local_time() {
        let wx = parse_wx(&serde_json::from_str(SAMPLE).unwrap()).unwrap();
        // 07:30 UTC = 10:30 local (+3) on 2026-10-24
        let utc = tz::days_from_civil(2026, 10, 24) * 86400 + 7 * 3600 + 1800;
        assert_eq!(view_idx(&wx, utc), (1, 0)); // 11:00 is the first hour that is not in the past
        assert_eq!(view_idx(&wx, utc + 86400), (3, 1));
        assert_eq!(view_idx(&wx, utc + 3 * 86400), (3, 2));
    }

    #[test]
    fn wmo_table() {
        for c in [0u8, 1, 2, 3, 45, 48, 51, 53, 55, 56, 57, 61, 63, 65, 66, 67, 71, 73, 75, 77, 80, 81, 82, 85, 86, 95, 96, 99] {
            let (_, ru, en) = wmo(c, true);
            assert!(!ru.is_empty() && !en.is_empty());
            assert_ne!(ru, "Нет данных", "code {c}");
        }
        assert_eq!(wmo(0, true).1, "Солнечно");
        assert_eq!(wmo(0, false).1, "Ясно");
        assert_eq!(wmo(2, true).0, Kind::Partly);
        assert_eq!(wmo(75, false).0, Kind::Snow);
        assert_eq!(wmo(96, true).0, Kind::Storm);
        assert_eq!(wmo(250, true).1, "Нет данных");
    }

    #[test]
    fn formatting_and_urls() {
        assert_eq!(fmt_t(6.4, false), "6°");
        assert_eq!(fmt_t(-0.4, false), "0°");
        assert_eq!(fmt_t(-2.6, false), "−3°");
        assert_eq!(fmt_t(20.0, true), "68°");
        assert_eq!(url_enc("Санкт-Петербург"), "%D0%A1%D0%B0%D0%BD%D0%BA%D1%82-%D0%9F%D0%B5%D1%82%D0%B5%D1%80%D0%B1%D1%83%D1%80%D0%B3");
        assert_eq!(url_enc("New York"), "New%20York");
        assert!(forecast_url(55.7, 37.6).contains("latitude=55.7000&longitude=37.6000"));
    }

    #[test]
    fn parses_geo_and_ip() {
        let g = json!({"results": [{"name": "Москва", "latitude": 55.75222, "longitude": 37.61556, "country": "Россия"}]});
        assert_eq!(parse_geo(&g), Some((55.75222, 37.61556, "Москва".to_string())));
        assert!(parse_geo(&json!({"generationtime_ms": 0.5})).is_none());
        let ip = json!({"ip": "1.2.3.4", "success": true, "city": "Moscow", "latitude": 55.7, "longitude": 37.6});
        assert_eq!(parse_ip(&ip), Some((55.7, 37.6, "Moscow".to_string())));
        assert!(parse_ip(&json!({"success": false, "message": "x"})).is_none());
    }

    /// One real request per endpoint to confirm the live formats. Run once: cargo test ... -- --ignored --nocapture
    #[test]
    #[ignore]
    fn live_formats() {
        let g = crate::net::http_get_json("https://geocoding-api.open-meteo.com/v1/search?name=%D0%9C%D0%BE%D1%81%D0%BA%D0%B2%D0%B0&count=1&language=ru&format=json", 10000).unwrap();
        let (lat, lon, name) = parse_geo(&g).expect("geocode format");
        println!("geo: {lat} {lon} {name}");
        let ip = crate::net::http_get_json("https://ipwho.is/", 10000).unwrap();
        println!("ip: {:?}", parse_ip(&ip));
        let raw = crate::net::http_get_json(&forecast_url(lat, lon), 10000).unwrap();
        let wx = parse_wx(&raw).expect("forecast format");
        println!("wx: temp {} code {} off {} hours {} days {}", wx.temp, wx.code, wx.off, wx.hours.len(), wx.days.len());
        assert!(wx.hours.len() >= 100 && wx.days.len() >= 5);
    }
}
