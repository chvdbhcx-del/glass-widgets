//! Rates: currency and crypto pairs. S = one pair big (value, 24 h change, sparkline), a click cycles the pairs;
//! M = a 2x2 grid (4 pairs) or rows (1-3 pairs) with pair, value, change and a tiny sparkline, a click asks for new pairs.
//! Settings: `cfg.pairs` = ["USD/RUB", "EUR/RUB", "BTC/USD", "ETH/USD"] (or one string "USD/RUB, EUR/RUB"), `cfg.sel` = pair shown by S.
//! Data: fiat from open.er-api.com (USD based, keyless), crypto from CoinGecko simple/price (usd + 24 h change), refreshed every
//! 10 min in a worker job. The last good response and the sample history (24 h, used by the sparklines and as the 24 h change of
//! pure fiat pairs) live in `%APPDATA%\macshell\cache\rates.json`; it is shown at once on start. Offline = stale values + "нет сети".
//! Note: the history is kept in that cache file, not in `cfg`, because `tick` only gets `&Cx` and cannot write the settings.
use crate::net;
use crate::registry::{Registry, WidgetDef};
use crate::widget::*;
use std::collections::HashMap;

const FIAT_URL: &str = "https://open.er-api.com/v6/latest/USD";
const CRYPTO_URL: &str = "https://api.coingecko.com/api/v3/simple/price?vs_currencies=usd&include_24hr_change=true&ids=";
const REFRESH_SECS: i64 = 600;
const RETRY_SECS: i64 = 120;
/// history window and cap
pub(crate) const KEEP_SECS: i64 = 25 * 3600;
pub(crate) const KEEP_MAX: usize = 200;
const MAX_PAIRS: usize = 4;
/// deeper than the palette green/red: white text stays legible on any backdrop
pub(crate) const GREEN_D: Color = Color::rgb8(31, 158, 66);
pub(crate) const RED_D: Color = Color::rgb8(224, 58, 48);
const DEFAULT_PAIRS: [&str; 4] = ["USD/RUB", "EUR/RUB", "BTC/USD", "ETH/USD"];
/// (ticker, CoinGecko id)
pub(crate) const CRYPTO: &[(&str, &str)] = &[
    ("BTC", "bitcoin"),
    ("ETH", "ethereum"),
    ("SOL", "solana"),
    ("TON", "the-open-network"),
    ("DOGE", "dogecoin"),
    ("LTC", "litecoin"),
    ("XRP", "ripple"),
    ("BNB", "binancecoin"),
    ("ADA", "cardano"),
    ("TRX", "tron"),
    ("USDT", "tether"),
    ("DOT", "polkadot"),
];

// ================================================================ pure logic

#[derive(Default, Clone, Debug)]
struct Market {
    /// units per 1 USD
    fiat: HashMap<String, f64>,
    /// ticker -> (price in USD, 24 h change %)
    crypto: HashMap<String, (f64, f64)>,
}

impl Market {
    fn usd(&self, c: &str) -> Option<f64> {
        if c == "USD" {
            return Some(1.0);
        }
        if let Some(&(p, _)) = self.crypto.get(c) {
            return (p > 0.0).then_some(p);
        }
        self.fiat.get(c).copied().filter(|r| *r > 0.0).map(|r| 1.0 / r)
    }
    /// price of one `a` in `b`
    fn value(&self, a: &str, b: &str) -> Option<f64> {
        Some(self.usd(a)? / self.usd(b)?)
    }
    /// 24 h change of the pair in %, known when a crypto side is involved (fiat sides count as flat against USD)
    fn change(&self, a: &str, b: &str) -> Option<f64> {
        let (ca, cb) = (self.crypto.get(a).map(|x| x.1), self.crypto.get(b).map(|x| x.1));
        if ca.is_none() && cb.is_none() {
            return None;
        }
        Some(((1.0 + ca.unwrap_or(0.0) / 100.0) / (1.0 + cb.unwrap_or(0.0) / 100.0) - 1.0) * 100.0)
    }
}

fn parse_fiat(v: &Value) -> Option<HashMap<String, f64>> {
    if v.get("result")?.as_str()? != "success" {
        return None;
    }
    let m: HashMap<String, f64> = v.get("rates")?.as_object()?.iter().filter_map(|(k, x)| Some((k.clone(), x.as_f64().filter(|r| *r > 0.0)?))).collect();
    (!m.is_empty()).then_some(m)
}

fn parse_crypto(v: &Value) -> HashMap<String, (f64, f64)> {
    CRYPTO
        .iter()
        .filter_map(|(code, id)| {
            let o = v.get(*id)?;
            Some((code.to_string(), (o.get("usd")?.as_f64()?, o.get("usd_24h_change").and_then(|c| c.as_f64()).unwrap_or(0.0))))
        })
        .collect()
}

/// "usd/rub", "USD-RUB", "usd rub", "USDRUB" -> ("USD", "RUB")
fn parse_pair(s: &str) -> Option<(String, String)> {
    let up = s.trim().to_uppercase();
    let parts: Vec<&str> = up.split(|c: char| c == '/' || c == '\\' || c == '-' || c.is_whitespace()).filter(|p| !p.is_empty()).collect();
    let (a, b) = match parts.as_slice() {
        [a, b] => (a.to_string(), b.to_string()),
        [one] if one.len() == 6 => (one[..3].to_string(), one[3..].to_string()),
        _ => return None,
    };
    let ok = |c: &str| (2..=5).contains(&c.len()) && c.chars().all(|ch| ch.is_ascii_alphanumeric());
    (ok(&a) && ok(&b) && a != b).then_some((a, b))
}

/// "USD/RUB, EUR/RUB" -> ["USD/RUB", "EUR/RUB"] (deduplicated, at most MAX_PAIRS)
fn parse_list(s: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for p in s.split(|c| c == ',' || c == ';' || c == '\n').filter_map(parse_pair) {
        let k = format!("{}/{}", p.0, p.1);
        if !out.contains(&k) && out.len() < MAX_PAIRS {
            out.push(k);
        }
    }
    out
}

fn pairs_from_cfg(v: Option<&Value>) -> Vec<String> {
    let list = match v {
        Some(Value::Array(a)) => parse_list(&a.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(",")),
        Some(Value::String(s)) => parse_list(s),
        _ => Vec::new(),
    };
    if list.is_empty() {
        DEFAULT_PAIRS.iter().map(|s| s.to_string()).collect()
    } else {
        list
    }
}

fn split_pair(p: &str) -> (&str, &str) {
    p.split_once('/').unwrap_or((p, "USD"))
}

fn group_digits(int: &str) -> String {
    let n = int.len();
    let mut s = String::new();
    for (i, ch) in int.chars().enumerate() {
        if i > 0 && (n - i) % 3 == 0 {
            s.push('\u{a0}');
        }
        s.push(ch);
    }
    s
}

/// 67 412 / 100,41 / 0,0123 (decimals shrink as the value grows)
pub(crate) fn fmt_value(v: f64, ru: bool) -> String {
    let dec = if v >= 1000.0 {
        0
    } else if v >= 1.0 {
        2
    } else if v >= 0.01 {
        4
    } else {
        6
    };
    let s = format!("{:.*}", dec, v);
    let (int, frac) = s.split_once('.').unwrap_or((&s, ""));
    let mut out = group_digits(int);
    if !frac.is_empty() {
        out.push(if ru { ',' } else { '.' });
        out += frac;
    }
    out
}

/// "+1,24%" / "−0,18%"
pub(crate) fn fmt_pct(c: f64, ru: bool) -> String {
    let s = format!("{:.2}", c.abs());
    let s = if ru { s.replace('.', ",") } else { s };
    let sign = if c < -0.004 {
        "−"
    } else if c > 0.004 {
        "+"
    } else {
        ""
    };
    format!("{sign}{s}%")
}

/// (colour, arrow glyph) of a change; flat = dim without an arrow
pub(crate) fn change_look(c: f64) -> (Color, Option<Glyph>) {
    if c > 0.004 {
        (GREEN, Some(Glyph::NetUp))
    } else if c < -0.004 {
        (RED, Some(Glyph::NetDown))
    } else {
        (DIM, None)
    }
}

pub(crate) type Hist = Vec<(i64, f64)>;

pub(crate) fn push_sample(h: &mut Hist, t: i64, v: f64) {
    h.push((t, v));
    h.retain(|s| t - s.0 <= KEEP_SECS);
    if h.len() > KEEP_MAX {
        let extra = h.len() - KEEP_MAX;
        h.drain(..extra);
    }
}

/// at most `max` evenly spaced points, always ending with the newest sample
pub(crate) fn spark(h: &[(i64, f64)], max: usize) -> Vec<f32> {
    if h.len() <= max {
        return h.iter().map(|s| s.1 as f32).collect();
    }
    let step = h.len() as f32 / max as f32;
    let mut v: Vec<f32> = (0..max).map(|i| h[((i as f32 * step) as usize).min(h.len() - 1)].1 as f32).collect();
    if let Some(l) = v.last_mut() {
        *l = h[h.len() - 1].1 as f32;
    }
    v
}

/// change of the newest sample against the oldest one, once the history spans at least 30 minutes
fn hist_change(h: &[(i64, f64)]) -> Option<f64> {
    let (a, b) = (h.first()?, h.last()?);
    (b.0 - a.0 >= 1800 && a.1 > 0.0).then(|| (b.1 / a.1 - 1.0) * 100.0)
}

// ================================================================ cache file

fn cache_path() -> Option<std::path::PathBuf> {
    Some(std::path::PathBuf::from(std::env::var_os("APPDATA")?).join("macshell").join("cache").join("rates.json"))
}

fn cache_json(m: &Market, hist: &HashMap<String, Hist>, t: i64) -> Value {
    json!({
        "t": t,
        "fiat": m.fiat,
        "crypto": m.crypto.iter().map(|(k, v)| (k.clone(), json!([v.0, v.1]))).collect::<serde_json::Map<String, Value>>(),
        "hist": hist.iter().map(|(k, h)| (k.clone(), json!(h.iter().map(|s| json!([s.0, s.1])).collect::<Vec<_>>()))).collect::<serde_json::Map<String, Value>>(),
    })
}

fn from_cache_json(v: &Value) -> (Market, HashMap<String, Hist>, i64) {
    let mut m = Market::default();
    if let Some(o) = v.get("fiat").and_then(|x| x.as_object()) {
        m.fiat = o.iter().filter_map(|(k, x)| Some((k.clone(), x.as_f64()?))).collect();
    }
    if let Some(o) = v.get("crypto").and_then(|x| x.as_object()) {
        m.crypto = o.iter().filter_map(|(k, x)| Some((k.clone(), (x.get(0)?.as_f64()?, x.get(1)?.as_f64()?)))).collect();
    }
    let mut hist = HashMap::new();
    if let Some(o) = v.get("hist").and_then(|x| x.as_object()) {
        for (k, a) in o {
            let h: Hist = a.as_array().map(|a| a.iter().filter_map(|s| Some((s.get(0)?.as_i64()?, s.get(1)?.as_f64()?))).collect()).unwrap_or_default();
            hist.insert(k.clone(), h);
        }
    }
    (m, hist, v.get("t").and_then(|x| x.as_i64()).unwrap_or(0))
}

fn load_cache() -> Option<(Market, HashMap<String, Hist>, i64)> {
    let txt = std::fs::read_to_string(cache_path()?).ok()?;
    let r = from_cache_json(&serde_json::from_str::<Value>(&txt).ok()?);
    (!r.0.fiat.is_empty() || !r.0.crypto.is_empty()).then_some(r)
}

fn save_cache(m: &Market, hist: &HashMap<String, Hist>, t: i64) {
    let Some(p) = cache_path() else { return };
    if let Some(d) = p.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    let tmp = p.with_extension("tmp");
    if std::fs::write(&tmp, cache_json(m, hist, t).to_string()).is_ok() {
        let _ = std::fs::rename(&tmp, &p);
    }
}

// ================================================================ network job

struct Fetched {
    fiat: Option<HashMap<String, f64>>,
    crypto: HashMap<String, (f64, f64)>,
    /// every needed endpoint answered
    ok: bool,
}

/// Worker thread only.
fn fetch(need_fiat: bool, ids: Vec<&'static str>) -> Fetched {
    let mut f = Fetched { fiat: None, crypto: HashMap::new(), ok: true };
    if need_fiat {
        f.fiat = net::http_get_json(FIAT_URL, 8000).ok().and_then(|v| parse_fiat(&v));
        f.ok &= f.fiat.is_some();
    }
    if !ids.is_empty() {
        f.crypto = net::http_get_json(&format!("{CRYPTO_URL}{}", ids.join(",")), 8000).ok().map(|v| parse_crypto(&v)).unwrap_or_default();
        f.ok &= !f.crypto.is_empty();
    }
    f
}

// ================================================================ widget

#[derive(Clone, Default)]
struct Row {
    label: String,
    value: Option<f64>,
    change: Option<f64>,
    spark: Vec<f32>,
}

#[derive(Default)]
struct Rates {
    started: bool,
    ver: u64,
    pairs: Vec<String>,
    sel: usize,
    market: Market,
    hist: HashMap<String, Hist>,
    rows: Vec<Row>,
    have_data: bool,
    stale: bool,
    job: Option<Pending<Fetched>>,
    prompt: Option<Pending<Option<String>>>,
    last_try: i64,
    /// new pairs not yet written to cfg (written by `draw` / `mouse`, which own `&mut Cx`)
    save_pairs: bool,
}

pub fn register(r: &mut Registry) {
    r.register(WidgetDef {
        type_id: "rates",
        name: ("Курсы", "Rates"),
        desc: ("Курсы валют и криптовалют", "Currency and crypto rates"),
        group: "info",
        icon: Glyph::NetUp,
        sizes: &[Size::S, Size::M],
        default_size: Size::M,
        make: || Box::new(Rates::default()),
    });
}

impl Rates {
    fn rebuild(&mut self) {
        self.rows = self
            .pairs
            .iter()
            .map(|p| {
                let (a, b) = split_pair(p);
                let h = self.hist.get(p).map(|h| h.as_slice()).unwrap_or(&[]);
                Row { label: p.clone(), value: self.market.value(a, b), change: self.market.change(a, b).or_else(|| hist_change(h)), spark: spark(h, 48) }
            })
            .collect();
        if self.sel >= self.rows.len() {
            self.sel = 0;
        }
    }

    fn start_job(&mut self, cx: &Cx, now: i64) {
        let need_fiat = self.pairs.iter().any(|p| {
            let (a, b) = split_pair(p);
            [a, b].iter().any(|c| *c != "USD" && !CRYPTO.iter().any(|(t, _)| t == c))
        });
        let mut ids: Vec<&'static str> = Vec::new();
        for p in &self.pairs {
            let (a, b) = split_pair(p);
            for c in [a, b] {
                if let Some((_, id)) = CRYPTO.iter().find(|(t, _)| *t == c) {
                    if !ids.contains(id) {
                        ids.push(id);
                    }
                }
            }
        }
        self.last_try = now;
        self.job = Some(cx.spawn(move || fetch(need_fiat, ids)));
    }

    /// merge a finished fetch: market, history, cache file
    fn apply(&mut self, f: Fetched, now: i64) {
        if let Some(fi) = f.fiat {
            self.market.fiat = fi;
        }
        self.market.crypto.extend(f.crypto);
        for p in &self.pairs {
            let (a, b) = split_pair(p);
            if let Some(v) = self.market.value(a, b) {
                push_sample(self.hist.entry(p.clone()).or_default(), now, v);
            }
        }
        self.stale = !f.ok;
        self.have_data = !self.market.fiat.is_empty() || !self.market.crypto.is_empty();
        self.rebuild();
        save_cache(&self.market, &self.hist, now);
    }

    fn change_text(&self, cx: &Cx, c: Option<f64>) -> (String, Color, Option<Glyph>) {
        match c {
            Some(c) => {
                let (col, g) = change_look(c);
                (fmt_pct(c, cx.lang == Lang::Ru), col, g)
            }
            None => ("—".to_string(), FAINT, None),
        }
    }

    fn value_text(&self, cx: &Cx, v: Option<f64>) -> String {
        v.map(|v| fmt_value(v, cx.lang == Lang::Ru)).unwrap_or_else(|| "—".to_string())
    }

    fn spark_color(c: Option<f64>) -> Color {
        match c {
            Some(c) if c < -0.004 => RED,
            Some(c) if c > 0.004 => GREEN,
            _ => DIM,
        }
    }

    /// sparkline, or a faint flat line while there are fewer than two samples
    fn draw_spark(&self, cx: &Cx, r: Rect, row: &Row) {
        if row.spark.len() >= 2 {
            cx.sparkline(r, &row.spark, Self::spark_color(row.change));
        } else {
            cx.line((r.x, r.cy()), (r.right(), r.cy()), 1.2, FILL);
        }
    }

    /// change as a small colour pill (white arrow + text); `right` aligns it to the right edge of `r`, else to the left
    fn draw_change(&self, cx: &Cx, r: Rect, c: Option<f64>, size: f32, right: bool) {
        let (t, col, g) = self.change_text(cx, c);
        let st = Style::new(size, 600).mono();
        let tw = cx.measure(&t, st, 0.0).0;
        let Some(c) = c else {
            let x = if right { r.right() - tw } else { r.x };
            return cx.text(Rect::new(x, r.y, tw + 2.0, r.h), &t, st, col);
        };
        let fill = if c > 0.004 {
            GREEN_D
        } else if c < -0.004 {
            RED_D
        } else {
            FILL
        };
        let icon = if g.is_some() { size - 1.0 } else { 0.0 };
        let (pw, ph) = (tw + icon + 12.0, size + 6.0);
        let x = if right { r.right() - pw } else { r.x };
        let pr = Rect::new(x, r.cy() - ph / 2.0, pw, ph);
        cx.pill(pr, fill);
        let mut tx = pr.x + 6.0;
        if let Some(g) = g {
            cx.icon(g, Rect::new(tx - 1.0, pr.cy() - icon / 2.0, icon, icon), WHITE);
            tx += icon;
        }
        cx.text(Rect::new(tx, pr.y, tw + 2.0, ph), &t, st, WHITE);
    }

    fn draw_empty(&self, cx: &Cx) {
        let busy = self.job.as_ref().is_some_and(|j| j.busy());
        let msg = if busy { cx.t("Загрузка…", "Loading…") } else { cx.t("нет данных", "no data") };
        cx.text(cx.content(), msg, Style::new(13.0, 400).center(), DIM);
    }

    fn draw_small(&self, cx: &Cx) {
        let Some(row) = self.rows.get(self.sel) else { return self.draw_empty(cx) };
        let c = cx.content();
        cx.text(Rect::new(c.x, c.y, 90.0, 18.0), &row.label.replace('/', " / "), Style::LABEL.ellipsis(), accent());
        let n = self.rows.len();
        if n > 1 {
            for i in 0..n {
                let x = c.right() - 3.0 - (n - 1 - i) as f32 * 9.0;
                cx.circle((x, c.y + 9.0), 2.5, if i == self.sel { TEXT } else { FAINT });
            }
        }
        let vt = self.value_text(cx, row.value);
        let mut size = 40.0f32;
        while size > 22.0 && cx.measure(&vt, Style::new(size, 300).mono(), 0.0).0 > c.w {
            size -= 2.0;
        }
        cx.text(Rect::new(c.x, c.y + 22.0, c.w, 50.0), &vt, Style::new(size, 300).mono().ellipsis(), TEXT);
        let line = Rect::new(c.x, c.y + 72.0, c.w, 22.0);
        self.draw_change(cx, line, row.change, 12.0, false);
        if self.stale {
            cx.text(line, cx.t("нет сети", "offline"), Style::CAPTION.right(), FAINT);
        }
        self.draw_spark(cx, Rect::new(c.x, c.y + 98.0, c.w, 40.0), row);
    }

    fn draw_grid(&self, cx: &Cx, area: Rect) {
        let (gap_x, gap_y) = (24.0, 12.0);
        let cw = (area.w - gap_x) / 2.0;
        let ch = (area.h - gap_y) / 2.0;
        cx.rect(Rect::new(area.cx() - 0.3, area.y + 4.0, 0.6, area.h - 8.0), FILL);
        cx.rect(Rect::new(area.x, area.cy() - 0.3, area.w, 0.6), FILL);
        for (i, row) in self.rows.iter().take(4).enumerate() {
            let (col, rw) = ((i % 2) as f32, (i / 2) as f32);
            let cell = Rect::new(area.x + col * (cw + gap_x), area.y + rw * (ch + gap_y), cw, ch);
            cx.text(Rect::new(cell.x, cell.y, 74.0, 16.0), &row.label, Style::new(11.0, 700).track(0.4).ellipsis(), accent());
            self.draw_change(cx, Rect::new(cell.x + 60.0, cell.y, cell.w - 60.0, 16.0), row.change, 10.5, true);
            let vt = self.value_text(cx, row.value);
            let vw = cell.w - 62.0;
            let mut size = 22.0f32;
            while size > 14.0 && cx.measure(&vt, Style::new(size, 300).mono(), 0.0).0 > vw {
                size -= 1.0;
            }
            cx.text(Rect::new(cell.x, cell.y + 19.0, vw, 28.0), &vt, Style::new(size, 300).mono().ellipsis(), TEXT);
            self.draw_spark(cx, Rect::new(cell.right() - 54.0, cell.y + 22.0, 54.0, (cell.h - 26.0).max(16.0)), row);
        }
    }

    fn draw_rows(&self, cx: &Cx, area: Rect) {
        let n = self.rows.len().min(3);
        let rh = (area.h / n as f32).min(46.0);
        let y0 = area.y + (area.h - rh * n as f32) / 2.0;
        for (i, row) in self.rows.iter().take(3).enumerate() {
            let r = Rect::new(area.x, y0 + i as f32 * rh, area.w, rh);
            if i > 0 {
                cx.rect(Rect::new(r.x, r.y, r.w, 0.6), FILL);
            }
            cx.text(Rect::new(r.x, r.y + rh / 2.0 - 19.0, 100.0, 18.0), &row.label.replace('/', " / "), Style::TITLE.ellipsis(), TEXT);
            self.draw_change(cx, Rect::new(r.x, r.y + rh / 2.0 + 1.0, 100.0, 18.0), row.change, 11.0, false);
            self.draw_spark(cx, Rect::new(r.x + 112.0, r.y + 9.0, 78.0, rh - 18.0), row);
            let vt = self.value_text(cx, row.value);
            let vw = r.w - 204.0;
            let mut size = 24.0f32;
            while size > 14.0 && cx.measure(&vt, Style::new(size, 300).mono(), 0.0).0 > vw {
                size -= 1.0;
            }
            cx.text(Rect::new(r.x + 204.0, r.y, vw, rh), &vt, Style::new(size, 300).mono().right().ellipsis(), TEXT);
        }
    }
}

impl Widget for Rates {
    fn draw(&mut self, cx: &mut Cx) -> Result<()> {
        if self.save_pairs {
            self.save_pairs = false;
            cx.cfg_set("pairs", json!(self.pairs));
        }
        if self.rows.is_empty() || (!self.have_data && !cx.preview) {
            if cx.size == Size::M {
                self.draw_empty(cx);
            } else {
                let c = cx.content();
                cx.text(Rect::new(c.x, c.y, c.w, 18.0), cx.t("КУРСЫ", "RATES"), Style::LABEL, accent());
                self.draw_empty(cx);
            }
            return Ok(());
        }
        if cx.size == Size::S {
            self.draw_small(cx);
            return Ok(());
        }
        let mut area = cx.content();
        if self.stale {
            let (a, b) = area.cut_bottom(14.0);
            cx.text(b, cx.t("нет сети", "offline"), Style::CAPTION.center().ellipsis(), FAINT);
            area = a;
            area.h -= 4.0;
        }
        if self.rows.len() >= 4 {
            self.draw_grid(cx, area);
        } else {
            self.draw_rows(cx, area);
        }
        Ok(())
    }

    fn tick(&mut self, cx: &Cx) -> Tick {
        let now = cx.utc_secs();
        if !self.started {
            self.started = true;
            self.pairs = pairs_from_cfg(cx.cfg.get("pairs"));
            self.sel = cx.cfg_f64("sel").map(|v| v as usize).unwrap_or(0);
            if cx.preview {
                sample(self, now);
                self.stale = cx.cfg_bool("offline").unwrap_or(false); // preview only: check the offline look
                self.ver += 1;
                return Tick::Never;
            }
            if let Some((m, h, _t)) = load_cache() {
                self.market = m;
                self.hist = h;
                self.have_data = true;
                self.stale = true; // until the first refresh succeeds
            }
            self.rebuild();
            self.ver += 1;
        }
        if cx.preview {
            return Tick::Never;
        }
        if let Some(p) = &self.prompt {
            if let Some(r) = p.take() {
                self.prompt = None;
                if let Some(list) = r.map(|t| parse_list(&t)).filter(|l| !l.is_empty()) {
                    self.pairs = list;
                    self.sel = 0;
                    self.save_pairs = true;
                    self.last_try = 0; // refetch now
                    self.rebuild();
                    self.ver += 1;
                }
            } else if !p.busy() {
                self.prompt = None;
            }
        }
        if let Some(p) = &self.job {
            if let Some(f) = p.take() {
                self.job = None;
                self.apply(f, now);
                self.ver += 1;
            } else if !p.busy() {
                self.job = None; // the worker died: treat as offline
                self.stale = true;
                self.ver += 1;
            }
        }
        let interval = if self.stale { RETRY_SECS } else { REFRESH_SECS };
        if self.job.is_none() && now - self.last_try >= interval {
            self.start_job(cx, now);
            self.ver += 1; // "Загрузка…" while there is nothing to show yet
        }
        let wait = (interval - (now - self.last_try)).clamp(5, interval);
        Tick::Poll(Duration::from_secs(wait as u64))
    }

    fn mouse(&mut self, ev: &MouseEv, cx: &mut Cx) -> bool {
        if self.save_pairs {
            self.save_pairs = false;
            cx.cfg_set("pairs", json!(self.pairs));
        }
        if !ev.clicked(cx.bounds()) {
            return false;
        }
        if cx.size == Size::S {
            if self.rows.len() > 1 {
                self.sel = (self.sel + 1) % self.rows.len();
                cx.cfg_set("sel", json!(self.sel));
                self.ver += 1;
                return true;
            }
            return false;
        }
        if self.prompt.is_none() {
            self.prompt = Some(cx.prompt_text(cx.t("Пары через запятую", "Pairs, comma separated"), &self.pairs.join(", "), false));
        }
        false
    }

    fn version(&self) -> u64 {
        self.ver
    }
}

/// Deterministic preview data (no I/O): four pairs, two up and two down.
fn sample(w: &mut Rates, now: i64) {
    w.market.fiat = [("USD", 1.0), ("RUB", 92.15), ("EUR", 0.9176)].iter().map(|(k, v)| (k.to_string(), *v)).collect();
    w.market.crypto = [("BTC", (67412.35, 2.31)), ("ETH", (2614.5, -1.07))].iter().map(|(k, v)| (k.to_string(), *v)).collect();
    for (k, p) in w.pairs.clone().iter().enumerate() {
        let (a, b) = split_pair(p);
        let cur = w.market.value(a, b).unwrap_or(1.0);
        let chg = w.market.change(a, b).unwrap_or(if k % 2 == 0 { 0.42 } else { -0.18 });
        let start = cur / (1.0 + chg / 100.0);
        let h: Hist = (0..48)
            .map(|i| {
                let f = i as f64 / 47.0;
                let wob = (i as f64 * 0.45 + k as f64 * 1.7).sin() * 0.0014 + (i as f64 * 1.3 + k as f64).cos() * 0.0005;
                (now - (47 - i) * 1800, (start + (cur - start) * f) * (1.0 + wob * (1.0 - f * 0.6)))
            })
            .collect();
        w.hist.insert(p.clone(), h);
    }
    w.have_data = true;
    w.rebuild(); // fiat-only pairs take their change from the history, like live
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIAT_SAMPLE: &str = r#"{"result":"success","provider":"https://www.exchangerate-api.com","base_code":"USD","rates":{"USD":1,"EUR":0.9176,"RUB":92.15,"JPY":151.3}}"#;
    const CRYPTO_SAMPLE: &str = r#"{"bitcoin":{"usd":67412.35,"usd_24h_change":2.31},"ethereum":{"usd":2614.5,"usd_24h_change":-1.07}}"#;

    fn market() -> Market {
        Market { fiat: parse_fiat(&serde_json::from_str(FIAT_SAMPLE).unwrap()).unwrap(), crypto: parse_crypto(&serde_json::from_str(CRYPTO_SAMPLE).unwrap()) }
    }

    #[test]
    fn parses_and_converts() {
        let m = market();
        assert!((m.value("USD", "RUB").unwrap() - 92.15).abs() < 1e-9);
        assert!((m.value("EUR", "RUB").unwrap() - 92.15 / 0.9176).abs() < 1e-9);
        assert!((m.value("BTC", "USD").unwrap() - 67412.35).abs() < 1e-9);
        assert!((m.value("BTC", "RUB").unwrap() - 67412.35 * 92.15).abs() < 1e-6);
        assert!(m.value("USD", "XXX").is_none());
        assert!(parse_fiat(&serde_json::from_str::<Value>(r#"{"result":"error"}"#).unwrap()).is_none());
        // fiat pairs have no 24 h change of their own; crypto pairs do
        assert!(m.change("USD", "RUB").is_none());
        assert!((m.change("BTC", "USD").unwrap() - 2.31).abs() < 1e-9);
        let eth_btc = m.change("ETH", "BTC").unwrap();
        assert!(eth_btc < -3.2 && eth_btc > -3.5, "{eth_btc}");
    }

    #[test]
    fn pairs() {
        assert_eq!(parse_list("usd/rub, EUR-RUB; btcusd , usd/usd, junk, usd/rub"), vec!["USD/RUB", "EUR/RUB", "BTC/USD"]);
        assert_eq!(parse_list("a/b, c/d/e").len(), 0);
        assert_eq!(parse_list("USD/RUB, EUR/RUB, GBP/RUB, JPY/RUB, CHF/RUB").len(), MAX_PAIRS);
        assert_eq!(pairs_from_cfg(None).len(), 4);
        assert_eq!(pairs_from_cfg(Some(&json!(["usd/eur", "x"]))), vec!["USD/EUR"]);
        assert_eq!(pairs_from_cfg(Some(&json!("btc/usd,eth/usd"))), vec!["BTC/USD", "ETH/USD"]);
    }

    #[test]
    fn formatting() {
        assert_eq!(fmt_value(67412.35, true), "67\u{a0}412");
        assert_eq!(fmt_value(100.409, true), "100,41");
        assert_eq!(fmt_value(92.15, false), "92.15");
        assert_eq!(fmt_value(1234567.0, true), "1\u{a0}234\u{a0}567");
        assert_eq!(fmt_value(0.0123456, true), "0,0123");
        assert_eq!(fmt_value(0.000123, false), "0.000123");
        assert_eq!(fmt_pct(1.236, true), "+1,24%");
        assert_eq!(fmt_pct(-0.18, false), "−0.18%");
        assert_eq!(fmt_pct(0.0, true), "0,00%");
        assert_eq!(change_look(1.0).0, GREEN);
        assert_eq!(change_look(-1.0).0, RED);
        assert_eq!(change_look(0.001).1, None);
    }

    #[test]
    fn history() {
        let mut h = Hist::new();
        for i in 0..300 {
            push_sample(&mut h, i * 600, 100.0 + i as f64);
        }
        assert!(h.len() <= KEEP_MAX);
        assert!(h.last().unwrap().0 - h.first().unwrap().0 <= KEEP_SECS);
        let s = spark(&h, 48);
        assert_eq!(s.len(), 48);
        assert_eq!(*s.last().unwrap(), 399.0);
        assert!(hist_change(&h).unwrap() > 0.0);
        assert!(hist_change(&h[..1]).is_none());
        assert!(hist_change(&[(0, 1.0), (60, 2.0)]).is_none());
    }

    #[test]
    fn cache_roundtrip() {
        let m = market();
        let mut hist = HashMap::new();
        hist.insert("USD/RUB".to_string(), vec![(10, 91.0), (20, 92.0)]);
        let (m2, h2, t) = from_cache_json(&cache_json(&m, &hist, 123));
        assert_eq!(t, 123);
        assert_eq!(m2.fiat.len(), m.fiat.len());
        assert_eq!(m2.crypto.get("BTC"), m.crypto.get("BTC"));
        assert_eq!(h2["USD/RUB"], vec![(10, 91.0), (20, 92.0)]);
    }

    /// Real request, run once by hand: `cargo test -- --ignored rates_live`
    #[test]
    #[ignore]
    fn rates_live() {
        let f = net::http_get_json(FIAT_URL, 10000).unwrap();
        let fiat = parse_fiat(&f).expect("fiat format");
        assert!(fiat.contains_key("RUB") && fiat.contains_key("EUR"));
        let c = net::http_get_json(&format!("{CRYPTO_URL}bitcoin,ethereum"), 10000).unwrap();
        let cr = parse_crypto(&c);
        println!("RUB={} BTC={:?} ETH={:?}", fiat["RUB"], cr.get("BTC"), cr.get("ETH"));
        assert!(cr.contains_key("BTC") && cr.contains_key("ETH"));
    }
}
