//! Crypto: prices and the 24 h change of a few coins. S = one coin big (price, change pill, sparkline), a click cycles the coins;
//! M = up to 4 rows (badge, ticker, sparkline, price, change); L = up to 6 rows with the coin name. A click on M / L asks for the coin list.
//! Settings: `cfg.coins` = ["BTC", "ETH", "SOL", "TON", "DOGE", "XRP"] (or one string "BTC, ETH"; tickers of the `rates` coin table, at most 6),
//! `cfg.vs` = "usd" (default) | "eur" | "rub" | "gbp" | "jpy" | "uah" | "try", `cfg.sel` = coin shown by S.
//! Data: CoinGecko public API `simple/price` (price + 24 h change in `vs`), fetched by a worker every 10 min (retry after 2 min). The network
//! is touched only by a live card (adding the widget is the opt-in; previews and the gallery use built-in sample data and never connect).
//! The last good answer and the sample history (the sparkline) live in `%APPDATA%\macshell\cache\crypto.json` and are shown at once on start;
//! offline = stale values + "нет сети".
use super::rates::{change_look, fmt_pct, fmt_value, push_sample, spark, Hist, CRYPTO, GREEN_D, RED_D};
use crate::net;
use crate::registry::{Registry, WidgetDef};
use crate::widget::*;
use std::collections::HashMap;

const URL: &str = "https://api.coingecko.com/api/v3/simple/price?include_24hr_change=true";
const REFRESH_SECS: i64 = 600;
const RETRY_SECS: i64 = 120;
const MAX_COINS: usize = 6;
const DEFAULT_COINS: [&str; 6] = ["BTC", "ETH", "SOL", "TON", "DOGE", "XRP"];
/// (CoinGecko vs_currency, symbol, symbol goes after the number)
const VS: &[(&str, &str, bool)] = &[("usd", "$", false), ("eur", "€", false), ("rub", "₽", true), ("gbp", "£", false), ("jpy", "¥", false), ("uah", "₴", true), ("try", "₺", true)];
/// (ticker, name, badge colour)
const COINS: &[(&str, &str, Color)] = &[
    ("BTC", "Bitcoin", Color::rgb8(247, 147, 26)),
    ("ETH", "Ethereum", Color::rgb8(98, 126, 234)),
    ("SOL", "Solana", Color::rgb8(140, 80, 240)),
    ("TON", "Toncoin", Color::rgb8(0, 152, 234)),
    ("DOGE", "Dogecoin", Color::rgb8(194, 166, 51)),
    ("LTC", "Litecoin", Color::rgb8(80, 120, 190)),
    ("XRP", "XRP", Color::rgb8(90, 100, 110)),
    ("BNB", "BNB", Color::rgb8(220, 165, 30)),
    ("ADA", "Cardano", Color::rgb8(40, 90, 200)),
    ("TRX", "TRON", Color::rgb8(225, 40, 60)),
    ("USDT", "Tether", Color::rgb8(38, 161, 123)),
    ("DOT", "Polkadot", Color::rgb8(220, 40, 130)),
];

// ================================================================ pure logic

type Quotes = HashMap<String, (f64, f64)>;

fn coin_id(ticker: &str) -> Option<&'static str> {
    CRYPTO.iter().find(|(t, _)| *t == ticker).map(|(_, id)| *id)
}

fn coin_info(ticker: &str) -> (&'static str, Color) {
    COINS.iter().find(|(t, _, _)| *t == ticker).map(|(_, n, c)| (*n, *c)).unwrap_or(("", FAINT))
}

/// "btc, eth; DOGE xyz btc" -> ["BTC", "ETH", "DOGE"] (known tickers only, deduplicated, at most MAX_COINS)
fn parse_coins(s: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for w in s.split(|c: char| c == ',' || c == ';' || c.is_whitespace()).filter(|w| !w.is_empty()) {
        let t = w.to_uppercase();
        if coin_id(&t).is_some() && !out.contains(&t) && out.len() < MAX_COINS {
            out.push(t);
        }
    }
    out
}

fn coins_from_cfg(v: Option<&Value>) -> Vec<String> {
    let list = match v {
        Some(Value::Array(a)) => parse_coins(&a.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(",")),
        Some(Value::String(s)) => parse_coins(s),
        _ => Vec::new(),
    };
    if list.is_empty() {
        DEFAULT_COINS.iter().map(|s| s.to_string()).collect()
    } else {
        list
    }
}

fn vs_from_cfg(v: Option<&str>) -> &'static str {
    let v = v.unwrap_or("usd").trim().to_lowercase();
    VS.iter().find(|(k, _, _)| *k == v).map(|(k, _, _)| *k).unwrap_or("usd")
}

/// "$67 412" / "67 412 ₽"
fn money(vs: &str, v: f64, ru: bool) -> String {
    let (sym, after) = VS.iter().find(|(k, _, _)| *k == vs).map(|(_, s, a)| (*s, *a)).unwrap_or(("$", false));
    let n = fmt_value(v, ru);
    if after {
        format!("{n}\u{a0}{sym}")
    } else {
        format!("{sym}{n}")
    }
}

/// `{"bitcoin":{"usd":67412.35,"usd_24h_change":2.31}}` -> BTC -> (67412.35, 2.31)
fn parse_quotes(v: &Value, vs: &str) -> Quotes {
    CRYPTO
        .iter()
        .filter_map(|(ticker, id)| {
            let o = v.get(*id)?;
            let p = o.get(vs)?.as_f64().filter(|p| *p > 0.0)?;
            Some((ticker.to_string(), (p, o.get(format!("{vs}_24h_change")).and_then(|c| c.as_f64()).unwrap_or(0.0))))
        })
        .collect()
}

fn request_url(coins: &[String], vs: &str) -> String {
    let ids: Vec<&str> = coins.iter().filter_map(|c| coin_id(c)).collect();
    format!("{URL}&vs_currencies={vs}&ids={}", ids.join(","))
}

// ================================================================ cache file

fn cache_path() -> Option<std::path::PathBuf> {
    Some(std::path::PathBuf::from(std::env::var_os("APPDATA")?).join("macshell").join("cache").join("crypto.json"))
}

fn cache_json(vs: &str, q: &Quotes, hist: &HashMap<String, Hist>) -> Value {
    json!({
        "vs": vs,
        "q": q.iter().map(|(k, v)| (k.clone(), json!([v.0, v.1]))).collect::<serde_json::Map<String, Value>>(),
        "hist": hist.iter().map(|(k, h)| (k.clone(), json!(h.iter().map(|s| json!([s.0, s.1])).collect::<Vec<_>>()))).collect::<serde_json::Map<String, Value>>(),
    })
}

/// (quotes, history) when the cache was written for the same `vs`
fn from_cache_json(v: &Value, vs: &str) -> Option<(Quotes, HashMap<String, Hist>)> {
    if v.get("vs")?.as_str()? != vs {
        return None;
    }
    let q: Quotes = v.get("q")?.as_object()?.iter().filter_map(|(k, x)| Some((k.clone(), (x.get(0)?.as_f64()?, x.get(1)?.as_f64()?)))).collect();
    let mut hist = HashMap::new();
    if let Some(o) = v.get("hist").and_then(|x| x.as_object()) {
        for (k, a) in o {
            let h: Hist = a.as_array().map(|a| a.iter().filter_map(|s| Some((s.get(0)?.as_i64()?, s.get(1)?.as_f64()?))).collect()).unwrap_or_default();
            hist.insert(k.clone(), h);
        }
    }
    (!q.is_empty()).then_some((q, hist))
}

fn load_cache(vs: &str) -> Option<(Quotes, HashMap<String, Hist>)> {
    from_cache_json(&serde_json::from_str::<Value>(&std::fs::read_to_string(cache_path()?).ok()?).ok()?, vs)
}

fn save_cache(vs: &str, q: &Quotes, hist: &HashMap<String, Hist>) {
    let Some(p) = cache_path() else { return };
    if let Some(d) = p.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    let tmp = p.with_extension("tmp");
    if std::fs::write(&tmp, cache_json(vs, q, hist).to_string()).is_ok() {
        let _ = std::fs::rename(&tmp, &p);
    }
}

// ================================================================ widget

#[derive(Clone, Default)]
struct Row {
    ticker: String,
    price: Option<f64>,
    change: Option<f64>,
    spark: Vec<f32>,
}

#[derive(Default)]
struct Crypto {
    started: bool,
    ver: u64,
    coins: Vec<String>,
    vs: &'static str,
    sel: usize,
    quotes: Quotes,
    hist: HashMap<String, Hist>,
    rows: Vec<Row>,
    have_data: bool,
    stale: bool,
    job: Option<Pending<Option<Quotes>>>,
    prompt: Option<Pending<Option<String>>>,
    last_try: i64,
    /// new coin list not yet written to cfg (written by `draw` / `mouse`, which own `&mut Cx`)
    save_coins: bool,
}

pub fn register(r: &mut Registry) {
    r.register(WidgetDef {
        type_id: "crypto",
        name: ("Криптовалюты", "Crypto"),
        desc: ("Курс и изменение за сутки", "Prices and the 24 h change"),
        group: "info",
        icon: Glyph::Bolt,
        sizes: &[Size::S, Size::M, Size::L],
        default_size: Size::M,
        make: || Box::new(Crypto::default()),
    });
}

/// Change as a small colour pill (arrow + text), right-aligned to `right` (or left-aligned at `x` when `right` is None); returns nothing.
fn change_pill(cx: &Cx, r: Rect, c: Option<f64>, size: f32, right: bool) {
    let ru = cx.lang == Lang::Ru;
    let st = Style::new(size, 600).mono();
    let Some(c) = c else {
        let tw = cx.measure("—", st, 0.0).0;
        let x = if right { r.right() - tw } else { r.x };
        return cx.text(Rect::new(x, r.y, tw + 2.0, r.h), "—", st, FAINT);
    };
    let t = fmt_pct(c, ru);
    let tw = cx.measure(&t, st, 0.0).0;
    let (_, g) = change_look(c);
    let fill = if g.is_none() { FILL } else if c > 0.0 { GREEN_D } else { RED_D };
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

fn badge(cx: &Cx, c: Pt, r: f32, ticker: &str) {
    cx.circle(c, r, coin_info(ticker).1);
    let ch: String = ticker.chars().take(1).collect();
    cx.text(Rect::new(c.0 - r, c.1 - r, r * 2.0, r * 2.0), &ch, Style::new(r * 1.15, 700).center(), WHITE);
}

fn flat_or_spark(cx: &Cx, r: Rect, row: &Row) {
    if row.spark.len() >= 2 {
        let col = match row.change {
            Some(c) if c < -0.004 => RED,
            Some(c) if c > 0.004 => GREEN,
            _ => DIM,
        };
        cx.sparkline(r, &row.spark, col);
    } else {
        cx.line((r.x, r.cy()), (r.right(), r.cy()), 1.2, FILL);
    }
}

impl Crypto {
    fn rebuild(&mut self) {
        self.rows = self
            .coins
            .iter()
            .map(|t| {
                let q = self.quotes.get(t);
                let h = self.hist.get(t).map(|h| h.as_slice()).unwrap_or(&[]);
                Row { ticker: t.clone(), price: q.map(|q| q.0), change: q.map(|q| q.1), spark: spark(h, 48) }
            })
            .collect();
        if self.sel >= self.rows.len() {
            self.sel = 0;
        }
    }

    fn start_job(&mut self, cx: &Cx, now: i64) {
        self.last_try = now;
        let url = request_url(&self.coins, self.vs);
        let vs = self.vs;
        self.job = Some(cx.spawn(move || net::http_get_json(&url, 8000).ok().map(|v| parse_quotes(&v, vs)).filter(|q| !q.is_empty())));
    }

    fn apply(&mut self, q: Option<Quotes>, now: i64) {
        match q {
            Some(q) => {
                for (t, (p, _)) in &q {
                    push_sample(self.hist.entry(t.clone()).or_default(), now, *p);
                }
                self.quotes.extend(q);
                self.stale = false;
                self.have_data = true;
                save_cache(self.vs, &self.quotes, &self.hist);
            }
            None => self.stale = true,
        }
        self.rebuild();
    }

    fn price_text(&self, cx: &Cx, p: Option<f64>) -> String {
        p.map(|p| money(self.vs, p, cx.lang == Lang::Ru)).unwrap_or_else(|| "—".to_string())
    }

    fn fit_price(&self, cx: &Cx, s: &str, w: f32, from: f32, min: f32, weight: u16) -> Style {
        let mut size = from;
        while size > min && cx.measure(s, Style::new(size, weight).mono(), 0.0).0 > w {
            size -= 1.0;
        }
        Style::new(size, weight).mono()
    }

    fn draw_empty(&self, cx: &Cx) {
        let busy = self.job.as_ref().is_some_and(|j| j.busy());
        let msg = if busy { cx.t("Загрузка…", "Loading…") } else { cx.t("нет данных", "no data") };
        cx.text(cx.content(), msg, Style::new(13.0, 400).center(), DIM);
    }

    fn draw_small(&self, cx: &Cx) {
        let Some(row) = self.rows.get(self.sel) else { return self.draw_empty(cx) };
        let c = cx.content();
        badge(cx, (c.x + 10.0, c.y + 10.0), 10.0, &row.ticker);
        cx.text(Rect::new(c.x + 26.0, c.y, 70.0, 20.0), &row.ticker, Style::TITLE.ellipsis(), TEXT);
        let n = self.rows.len();
        if n > 1 {
            for i in 0..n {
                let x = c.right() - 3.0 - (n - 1 - i) as f32 * 9.0;
                cx.circle((x, c.y + 10.0), 2.5, if i == self.sel { TEXT } else { FAINT });
            }
        }
        let pt = self.price_text(cx, row.price);
        cx.text(Rect::new(c.x, c.y + 26.0, c.w, 46.0), &pt, self.fit_price(cx, &pt, c.w, 36.0, 20.0, 300).ellipsis(), TEXT);
        let line = Rect::new(c.x, c.y + 76.0, c.w, 22.0);
        change_pill(cx, line, row.change, 12.0, false);
        if self.stale {
            cx.text(line, cx.t("нет сети", "offline"), Style::CAPTION.right(), FAINT);
        }
        flat_or_spark(cx, Rect::new(c.x, c.y + 102.0, c.w, 36.0), row);
    }

    fn draw_rows(&self, cx: &Cx, area: Rect, n: usize) {
        let rh = (area.h / n as f32).min(40.0);
        let y0 = area.y + (area.h - rh * n as f32) / 2.0;
        for (i, row) in self.rows.iter().take(n).enumerate() {
            let r = Rect::new(area.x, y0 + i as f32 * rh, area.w, rh);
            if i > 0 {
                cx.rect(Rect::new(r.x, r.y, r.w, 0.6), FILL);
            }
            badge(cx, (r.x + 11.0, r.cy()), 10.0, &row.ticker);
            cx.text(Rect::new(r.x + 28.0, r.y, 56.0, r.h), &row.ticker, Style::TITLE.ellipsis(), TEXT);
            flat_or_spark(cx, Rect::new(r.x + 88.0, r.y + 8.0, 66.0, r.h - 16.0), row);
            change_pill(cx, Rect::new(r.x, r.y, r.w, r.h), row.change, 11.0, true);
            let pt = self.price_text(cx, row.price);
            let pw = r.w - 164.0 - 76.0;
            cx.text(Rect::new(r.x + 164.0, r.y, pw, r.h), &pt, self.fit_price(cx, &pt, pw, 15.0, 11.0, 500).right().ellipsis(), TEXT);
        }
    }

    fn draw_list(&self, cx: &Cx, area: Rect) {
        let rh = 46.0;
        for (i, row) in self.rows.iter().take(MAX_COINS).enumerate() {
            let r = Rect::new(area.x, area.y + i as f32 * rh, area.w, rh);
            if i > 0 {
                cx.rect(Rect::new(r.x, r.y, r.w, 0.6), FILL);
            }
            badge(cx, (r.x + 14.0, r.cy()), 14.0, &row.ticker);
            cx.text(Rect::new(r.x + 38.0, r.y + 5.0, 80.0, 18.0), &row.ticker, Style::TITLE.ellipsis(), TEXT);
            cx.text(Rect::new(r.x + 38.0, r.y + 23.0, 80.0, 16.0), coin_info(&row.ticker).0, Style::CAPTION.ellipsis(), DIM);
            flat_or_spark(cx, Rect::new(r.x + 124.0, r.y + 9.0, 78.0, r.h - 18.0), row);
            let pt = self.price_text(cx, row.price);
            let pw = r.w - 214.0;
            cx.text(Rect::new(r.x + 214.0, r.y + 3.0, pw, 20.0), &pt, self.fit_price(cx, &pt, pw, 15.0, 11.0, 500).right().ellipsis(), TEXT);
            change_pill(cx, Rect::new(r.x + 214.0, r.y + 24.0, pw, 18.0), row.change, 11.0, true);
        }
    }
}

impl Widget for Crypto {
    fn draw(&mut self, cx: &mut Cx) -> Result<()> {
        if self.save_coins {
            self.save_coins = false;
            cx.cfg_set("coins", json!(self.coins));
        }
        if self.rows.is_empty() || (!self.have_data && !cx.preview) {
            self.draw_empty(cx);
            return Ok(());
        }
        if cx.size == Size::S {
            self.draw_small(cx);
            return Ok(());
        }
        let mut area = cx.content();
        let offline = if self.stale { cx.t("нет сети", "offline") } else { "" };
        match cx.size {
            Size::M => {
                if self.stale {
                    let (a, b) = area.cut_bottom(14.0);
                    cx.text(b, offline, Style::CAPTION.center().ellipsis(), FAINT);
                    area = a;
                    area.h -= 2.0;
                }
                self.draw_rows(cx, area, self.rows.len().min(4));
            }
            _ => {
                cx.text(Rect::new(area.x, area.y, area.w - 70.0, 18.0), cx.t("КРИПТОВАЛЮТЫ", "CRYPTO"), Style::LABEL.ellipsis(), ACCENT);
                cx.text(Rect::new(area.right() - 70.0, area.y, 70.0, 18.0), offline, Style::CAPTION.right(), FAINT);
                self.draw_list(cx, Rect::new(area.x, area.y + 30.0, area.w, area.h - 30.0));
            }
        }
        Ok(())
    }

    fn tick(&mut self, cx: &Cx) -> Tick {
        let now = cx.utc_secs();
        if !self.started {
            self.started = true;
            self.coins = coins_from_cfg(cx.cfg.get("coins"));
            self.vs = vs_from_cfg(cx.cfg_str("vs"));
            self.sel = cx.cfg_f64("sel").map(|v| v as usize).unwrap_or(0);
            if cx.preview {
                sample(self, now);
                self.stale = cx.cfg_bool("offline").unwrap_or(false); // preview only: check the offline look
                self.ver += 1;
                return Tick::Never;
            }
            if let Some((q, h)) = load_cache(self.vs) {
                self.quotes = q;
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
                if let Some(list) = r.map(|t| parse_coins(&t)).filter(|l| !l.is_empty()) {
                    self.coins = list;
                    self.sel = 0;
                    self.save_coins = true;
                    self.last_try = 0; // refetch now
                    self.rebuild();
                    self.ver += 1;
                }
            } else if !p.busy() {
                self.prompt = None;
            }
        }
        if let Some(p) = &self.job {
            if let Some(q) = p.take() {
                self.job = None;
                self.apply(q, now);
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
        if self.save_coins {
            self.save_coins = false;
            cx.cfg_set("coins", json!(self.coins));
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
        if self.prompt.is_none() && !cx.preview {
            self.prompt = Some(cx.prompt_text(cx.t("Монеты через запятую (BTC, ETH, SOL…)", "Coins, comma separated (BTC, ETH, SOL…)"), &self.coins.join(", "), false));
        }
        false
    }

    fn version(&self) -> u64 {
        self.ver
    }
}

/// Deterministic preview data (no I/O): every coin gets a 24 h history that ends at its price.
fn sample(w: &mut Crypto, now: i64) {
    const S: &[(&str, f64, f64)] = &[("BTC", 67412.35, 2.31), ("ETH", 2614.5, -1.07), ("SOL", 148.62, 4.85), ("TON", 5.41, -0.62), ("DOGE", 0.1234, 7.9), ("XRP", 0.5231, -2.4), ("LTC", 84.2, 0.4), ("BNB", 585.3, 1.2), ("ADA", 0.452, -3.1), ("TRX", 0.121, 0.9), ("USDT", 1.0, 0.0), ("DOT", 6.84, -0.8)];
    let rate = match w.vs {
        "eur" => 0.9176,
        "rub" => 92.15,
        "gbp" => 0.79,
        "jpy" => 151.3,
        "uah" => 41.3,
        "try" => 32.4,
        _ => 1.0,
    };
    for (k, (t, p, c)) in S.iter().enumerate() {
        let cur = p * rate;
        let start = cur / (1.0 + c / 100.0);
        let h: Hist = (0..48)
            .map(|i| {
                let f = i as f64 / 47.0;
                let wob = (i as f64 * 0.5 + k as f64 * 1.9).sin() * 0.012 + (i as f64 * 1.4 + k as f64).cos() * 0.004;
                (now - (47 - i) * 1800, (start + (cur - start) * f) * (1.0 + wob * (1.0 - f * 0.7)))
            })
            .collect();
        w.hist.insert(t.to_string(), h);
        w.quotes.insert(t.to_string(), (cur, *c));
    }
    w.have_data = true;
    w.rebuild();
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{"bitcoin":{"usd":67412.35,"usd_24h_change":2.31},"ethereum":{"usd":2614.5,"usd_24h_change":-1.07},"dogecoin":{"usd":0.1234}}"#;

    #[test]
    fn parses_prices_and_changes() {
        let q = parse_quotes(&serde_json::from_str(SAMPLE).unwrap(), "usd");
        assert_eq!(q["BTC"], (67412.35, 2.31));
        assert_eq!(q["ETH"], (2614.5, -1.07));
        assert_eq!(q["DOGE"], (0.1234, 0.0), "a missing change reads as flat");
        assert!(!q.contains_key("SOL"));
        assert!(parse_quotes(&serde_json::from_str(SAMPLE).unwrap(), "rub").is_empty(), "other currency is not mixed in");
        assert!(parse_quotes(&json!({"bitcoin": {"usd": 0}}), "usd").is_empty(), "zero price rejected");
        assert!(parse_quotes(&json!({"error": "rate limit"}), "usd").is_empty());
    }

    #[test]
    fn coin_lists() {
        assert_eq!(parse_coins("btc, eth; DOGE xyz btc"), vec!["BTC", "ETH", "DOGE"]);
        assert_eq!(parse_coins("BTC,ETH,SOL,TON,DOGE,XRP,LTC,BNB").len(), MAX_COINS);
        assert!(parse_coins("nope").is_empty());
        assert_eq!(coins_from_cfg(None).len(), 6);
        assert_eq!(coins_from_cfg(Some(&json!(["eth", "zzz"]))), vec!["ETH"]);
        assert_eq!(coins_from_cfg(Some(&json!("btc,ton"))), vec!["BTC", "TON"]);
        assert_eq!(coins_from_cfg(Some(&json!([]))).len(), 6, "empty falls back to the default list");
    }

    #[test]
    fn urls_and_currency() {
        let u = request_url(&["BTC".to_string(), "TON".to_string(), "NOPE".to_string()], "eur");
        assert!(u.starts_with("https://api.coingecko.com/api/v3/simple/price?"));
        assert!(u.ends_with("&vs_currencies=eur&ids=bitcoin,the-open-network"), "{u}");
        assert_eq!(vs_from_cfg(Some(" RUB ")), "rub");
        assert_eq!(vs_from_cfg(Some("zzz")), "usd");
        assert_eq!(vs_from_cfg(None), "usd");
        assert_eq!(money("usd", 67412.35, false), "$67\u{a0}412");
        assert_eq!(money("rub", 92.15, true), "92,15\u{a0}₽");
        assert_eq!(money("eur", 0.0123456, false), "€0.0123");
    }

    #[test]
    fn cache_roundtrip() {
        let q = parse_quotes(&serde_json::from_str(SAMPLE).unwrap(), "usd");
        let mut hist = HashMap::new();
        hist.insert("BTC".to_string(), vec![(10, 67000.0), (20, 67412.0)]);
        let j = cache_json("usd", &q, &hist);
        let (q2, h2) = from_cache_json(&j, "usd").unwrap();
        assert_eq!(q2["BTC"], q["BTC"]);
        assert_eq!(h2["BTC"], vec![(10, 67000.0), (20, 67412.0)]);
        assert!(from_cache_json(&j, "eur").is_none(), "a cache of another currency is ignored");
        assert!(from_cache_json(&json!({}), "usd").is_none());
    }

    #[test]
    fn every_coin_has_a_name_and_an_id() {
        for (t, _, _) in COINS {
            assert!(coin_id(t).is_some(), "{t}");
        }
        for (t, _) in CRYPTO {
            assert!(COINS.iter().any(|(c, _, _)| c == t), "{t} has no badge entry");
        }
    }

    /// Real request, run once by hand: `cargo test crypto_live -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn crypto_live() {
        let coins: Vec<String> = DEFAULT_COINS.iter().map(|s| s.to_string()).collect();
        let v = net::http_get_json(&request_url(&coins, "usd"), 10000).unwrap();
        let q = parse_quotes(&v, "usd");
        println!("{q:?}");
        assert!(q.contains_key("BTC"));
    }
}
