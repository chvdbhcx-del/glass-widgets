//! Next event: the upcoming events of a local .ics (iCalendar) file or of a calendar subscription link.
//! S = the next event (day, title, start time, "через 52 мин"); M = that plus the next three on the right; L = a list of six.
//! Settings: `cfg.ics` = path of the .ics file OR an `https://` / `webcal://` link (default none: the card says how to set it; a click opens a
//! text prompt that writes it). A link is fetched by the worker (WinHTTP, <= 2 MB, 15 s, redirects followed by hand and re-checked, If-None-Match /
//! If-Modified-Since when known) every 30 min, the last good body is cached in `%APPDATA%\macshell\cache\nextevent-<hash>.ics` so the card works
//! offline. The link often holds a secret token: it is never logged (only the host), never put into error text, and the cache name is a hash.
//! Parsed: VEVENT DTSTART / DTEND / SUMMARY / RRULE / EXDATE / STATUS:CANCELLED. DTSTART is UTC ("...Z", moved to local time with the
//! CURRENT UTC offset), a floating local time or an all-day date (VALUE=DATE); TZID is ignored (the value is read as local time).
//! RRULE: FREQ=DAILY|WEEKLY|MONTHLY|YEARLY with INTERVAL, COUNT, UNTIL and (weekly) BYDAY=MO,WE; anything fancier is shown as a single event.
//! A file is read by a worker every 5 min (never in preview: previews parse a built-in sample ics and never touch the network). Times inside are "local seconds"
//! (`days_from_civil * 86400 + time of day`, like `Now::local_secs`), so no OS time-zone call is needed.
use super::reminders::wrap_lines;
use crate::registry::{Registry, WidgetDef};
use crate::tz;
use crate::widget::*;

const REFRESH_SECS: i64 = 300;
/// a calendar link is polled every 30 min, again after 5 min when only the cache could be shown
const URL_REFRESH_SECS: i64 = 1800;
const URL_RETRY_SECS: i64 = 300;
const MAX_BODY: usize = 2 << 20;
const FETCH_TIMEOUT_MS: i32 = 15_000;
const MAX_HOPS: usize = 5;
const HORIZON_DAYS: i64 = 400;
const MAX_STEPS: i64 = 20_000;
const MAX_FILE: u64 = 8 << 20;
const MAX_ITEMS: usize = 6;

// ================================================================ ics parsing

#[derive(Clone, Copy, Debug, PartialEq)]
enum Freq {
    Daily,
    Weekly,
    Monthly,
    Yearly,
}

#[derive(Clone, Debug, PartialEq)]
struct Rule {
    freq: Freq,
    interval: i64,
    count: Option<u32>,
    until: Option<i64>,
    /// weekly BYDAY, 0 = Monday (empty = the weekday of DTSTART)
    days: Vec<i64>,
}

#[derive(Clone, Debug, PartialEq)]
struct Event {
    start: i64,
    end: i64,
    all_day: bool,
    title: String,
    rule: Option<Rule>,
    exdates: Vec<i64>,
}

#[derive(Default)]
struct Draft {
    start: Option<(i64, bool)>,
    end: Option<i64>,
    title: String,
    rule: Option<Rule>,
    ex: Vec<i64>,
    cancelled: bool,
}

impl Draft {
    fn finish(self) -> Option<Event> {
        let (start, all_day) = self.start?;
        if self.cancelled {
            return None;
        }
        let end = self.end.filter(|e| *e >= start).unwrap_or(if all_day { start + 86400 } else { start });
        Some(Event { start, end, all_day, title: self.title, rule: self.rule, exdates: self.ex })
    }
}

/// RFC 5545 line unfolding (a line starting with a space or tab continues the previous one).
fn unfold(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for raw in text.split('\n') {
        let l = raw.strip_suffix('\r').unwrap_or(raw);
        match (l.strip_prefix([' ', '\t']), out.last_mut()) {
            (Some(rest), Some(last)) => last.push_str(rest),
            _ => out.push(l.to_string()),
        }
    }
    out
}

fn unescape(s: &str) -> String {
    let mut o = String::new();
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            o.push(c);
            continue;
        }
        match it.next() {
            Some('n') | Some('N') => o.push(' '),
            Some(x) => o.push(x),
            None => {}
        }
    }
    o.trim().to_string()
}

/// "20261024T103000Z" / "20261024T103000" / "20261024" -> (local seconds, all-day)
fn parse_dt(v: &str, utc_off: i32) -> Option<(i64, bool)> {
    let v = v.trim();
    let (date, time) = match v.split_once(['T', 't']) {
        Some((d, t)) => (d, Some(t)),
        None => (v, None),
    };
    if date.len() != 8 || !date.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let (y, m, d): (i32, u32, u32) = (date[..4].parse().ok()?, date[4..6].parse().ok()?, date[6..8].parse().ok()?);
    if !(1..=12).contains(&m) || d == 0 || d > tz::days_in(y, m) {
        return None;
    }
    let day = tz::days_from_civil(y, m, d) * 86400;
    let Some(t) = time else { return Some((day, true)) };
    let utc = t.ends_with(['Z', 'z']);
    let t = t.trim_end_matches(['Z', 'z']);
    if t.len() < 4 || !t.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let h: i64 = t[..2].parse().ok()?;
    let mi: i64 = t[2..4].parse().ok()?;
    let s: i64 = t.get(4..6).and_then(|x| x.parse().ok()).unwrap_or(0);
    if h > 23 || mi > 59 || s > 60 {
        return None;
    }
    let local = day + h * 3600 + mi * 60 + s;
    Some((if utc { local + utc_off as i64 * 60 } else { local }, false))
}

fn wd_code(s: &str) -> Option<i64> {
    ["MO", "TU", "WE", "TH", "FR", "SA", "SU"].iter().position(|c| c.eq_ignore_ascii_case(s.trim())).map(|i| i as i64)
}

fn parse_rule(v: &str, utc_off: i32) -> Option<Rule> {
    let (mut freq, mut interval, mut count, mut until, mut days) = (None, 1i64, None, None, Vec::new());
    for part in v.split(';').filter(|p| !p.trim().is_empty()) {
        let (k, x) = part.split_once('=')?;
        match k.trim().to_ascii_uppercase().as_str() {
            "FREQ" => {
                freq = Some(match x.trim().to_ascii_uppercase().as_str() {
                    "DAILY" => Freq::Daily,
                    "WEEKLY" => Freq::Weekly,
                    "MONTHLY" => Freq::Monthly,
                    "YEARLY" => Freq::Yearly,
                    _ => return None,
                })
            }
            "INTERVAL" => interval = x.trim().parse().ok().filter(|n| (1..=1000).contains(n))?,
            "COUNT" => count = Some(x.trim().parse().ok()?),
            "UNTIL" => {
                let (s, all) = parse_dt(x, utc_off)?;
                until = Some(if all { s + 86399 } else { s });
            }
            "BYDAY" => {
                for d in x.split(',') {
                    days.push(wd_code(d)?); // "1MO" / "-1FR" (nth weekday) is not supported: the whole rule is dropped
                }
            }
            "WKST" => {}
            _ => return None, // BYMONTHDAY, BYSETPOS, ... : not simple
        }
    }
    let freq = freq?;
    if !days.is_empty() && freq != Freq::Weekly {
        return None;
    }
    days.sort_unstable();
    days.dedup();
    Some(Rule { freq, interval, count, until, days })
}

fn parse_ics(text: &str, utc_off: i32) -> Vec<Event> {
    let mut out = Vec::new();
    let mut cur: Option<Draft> = None;
    let mut alarm = false;
    for l in unfold(text.trim_start_matches('\u{feff}')) {
        let t = l.trim();
        if t.eq_ignore_ascii_case("BEGIN:VEVENT") {
            cur = Some(Draft::default());
            alarm = false;
            continue;
        }
        if t.eq_ignore_ascii_case("END:VEVENT") {
            if let Some(e) = cur.take().and_then(Draft::finish) {
                out.push(e);
            }
            continue;
        }
        let Some(d) = cur.as_mut() else { continue };
        if t.eq_ignore_ascii_case("BEGIN:VALARM") || t.eq_ignore_ascii_case("END:VALARM") {
            alarm = t.eq_ignore_ascii_case("BEGIN:VALARM");
            continue;
        }
        let Some((name, val)) = t.split_once(':') else { continue };
        if alarm {
            continue;
        }
        match name.split(';').next().unwrap_or("").to_ascii_uppercase().as_str() {
            "DTSTART" => d.start = parse_dt(val, utc_off),
            "DTEND" => d.end = parse_dt(val, utc_off).map(|x| x.0),
            "SUMMARY" => d.title = unescape(val),
            "RRULE" => d.rule = parse_rule(val, utc_off),
            "EXDATE" => d.ex.extend(val.split(',').filter_map(|x| parse_dt(x, utc_off).map(|p| p.0))),
            "STATUS" => d.cancelled = val.trim().eq_ignore_ascii_case("CANCELLED"),
            _ => {}
        }
    }
    out
}

// ================================================================ recurrence

/// Start times (local seconds, ascending) of every occurrence of `ev` up to `until`, honouring COUNT / UNTIL / EXDATE.
/// ponytail: at most MAX_STEPS candidate periods are walked (a DAILY event from 2005 is fine; upgrade to a closed form if ever needed).
fn occurrences(ev: &Event, until: i64) -> Vec<i64> {
    let Some(r) = &ev.rule else {
        return if ev.start <= until && !ev.exdates.contains(&ev.start) { vec![ev.start] } else { Vec::new() };
    };
    let (d0, sod) = (ev.start.div_euclid(86400), ev.start.rem_euclid(86400));
    let (y0, m0, dd0) = tz::civil_from_days(d0);
    let dow0 = (d0 + 3).rem_euclid(7);
    let wdays = if r.days.is_empty() { vec![dow0] } else { r.days.clone() };
    let (mut out, mut n) = (Vec::new(), 0u32);
    'steps: for k in 0..MAX_STEPS {
        let days: Vec<i64> = match r.freq {
            Freq::Daily => vec![d0 + k * r.interval],
            Freq::Weekly => {
                let base = d0 - dow0 + 7 * k * r.interval;
                wdays.iter().map(|w| base + w).collect()
            }
            Freq::Monthly => {
                let m = y0 as i64 * 12 + m0 as i64 - 1 + k * r.interval;
                let (y, mo) = (m.div_euclid(12) as i32, m.rem_euclid(12) as u32 + 1);
                if dd0 <= tz::days_in(y, mo) {
                    vec![tz::days_from_civil(y, mo, dd0)]
                } else {
                    Vec::new() // the 31st does not exist in this month: skipped, like RFC 5545
                }
            }
            Freq::Yearly => {
                let y = y0 + (k * r.interval) as i32;
                if dd0 <= tz::days_in(y, m0) {
                    vec![tz::days_from_civil(y, m0, dd0)]
                } else {
                    Vec::new()
                }
            }
        };
        for d in days {
            let s = d * 86400 + sod;
            if s < ev.start {
                continue;
            }
            if s > until || r.until.is_some_and(|u| s > u) {
                break 'steps;
            }
            n += 1;
            if r.count.is_some_and(|c| n > c) {
                break 'steps;
            }
            if !ev.exdates.contains(&s) {
                out.push(s);
            }
        }
    }
    out
}

#[derive(Clone, Debug, PartialEq)]
struct Item {
    start: i64,
    end: i64,
    all_day: bool,
    title: String,
}

/// The next `limit` occurrences that have not finished at `now` (local seconds). All-day events sort after the timed ones of their day.
fn upcoming(events: &[Event], now: i64, limit: usize) -> Vec<Item> {
    let mut v: Vec<Item> = Vec::new();
    for ev in events {
        let dur = ev.end - ev.start;
        // a point event (no DTEND) still counts as "now" for 5 minutes
        let eff = if ev.all_day { dur } else { dur.max(300) };
        let mut taken = 0;
        for s in occurrences(ev, now + HORIZON_DAYS * 86400) {
            if s + eff <= now {
                continue;
            }
            v.push(Item { start: s, end: s + dur, all_day: ev.all_day, title: ev.title.clone() });
            taken += 1;
            if taken >= limit {
                break;
            }
        }
    }
    v.sort_by_key(|i| (if i.all_day { i.start + 86399 } else { i.start }, i.start));
    v.truncate(limit);
    v
}

// ================================================================ text

const WD_SHORT_RU: [&str; 7] = ["Пн", "Вт", "Ср", "Чт", "Пт", "Сб", "Вс"];
const WD_SHORT_EN: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
const MO_SHORT_RU: [&str; 12] = ["янв", "фев", "мар", "апр", "мая", "июн", "июл", "авг", "сен", "окт", "ноя", "дек"];
const MO_SHORT_EN: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

fn hm(secs: i64) -> String {
    let r = secs.rem_euclid(86400);
    format!("{:02}:{:02}", r / 3600, r % 3600 / 60)
}

/// "Сегодня" / "Завтра" / weekday (within a week) / "26 окт" ("Oct 26"); `long` = full weekday and month names.
fn day_label(day: i64, today: i64, lang: Lang, long: bool) -> String {
    let ru = lang == Lang::Ru;
    let d = day - today;
    if d <= 0 {
        return lang.pick("Сегодня", "Today").to_string();
    }
    if d == 1 {
        return lang.pick("Завтра", "Tomorrow").to_string();
    }
    let (_, mo, dd) = tz::civil_from_days(day);
    let dow = (day + 3).rem_euclid(7) as u32;
    if d <= 6 {
        return if long { weekday_name(lang, dow).to_string() } else { (if ru { WD_SHORT_RU } else { WD_SHORT_EN })[dow as usize].to_string() };
    }
    match (ru, long) {
        (true, true) => format!("{dd} {}", month_genitive(lang, mo)),
        (true, false) => format!("{dd} {}", MO_SHORT_RU[mo as usize - 1]),
        (false, true) => format!("{} {dd}", month_genitive(lang, mo)),
        (false, false) => format!("{} {dd}", MO_SHORT_EN[mo as usize - 1]),
    }
}

/// "через 52 мин" / "через 2 ч 20 мин" / "Завтра" / "через 3 дня" / "Сейчас, до 11:30"
fn rel_text(i: &Item, now: i64, lang: Lang) -> String {
    let ru = lang == Lang::Ru;
    let (sd, today) = (i.start.div_euclid(86400), now.div_euclid(86400));
    if i.all_day {
        return match sd - today {
            d if d <= 0 => lang.pick("Сегодня", "Today").to_string(),
            1 => lang.pick("Завтра", "Tomorrow").to_string(),
            d if ru => format!("через {d} {}", plural(d as u32, "день", "дня", "дней")),
            d => format!("in {d} days"),
        };
    }
    if i.start <= now {
        let (a, b) = (lang.pick("Сейчас", "Now"), lang.pick("до", "until"));
        return if i.end > i.start { format!("{a}, {b} {}", hm(i.end)) } else { a.to_string() };
    }
    let m = (i.start - now + 59) / 60;
    if m < 60 {
        return if ru { format!("через {m} мин") } else { format!("in {m} min") };
    }
    if m < 24 * 60 {
        let (h, mm) = (m / 60, m % 60);
        return match (ru, mm) {
            (true, 0) => format!("через {h} ч"),
            (true, _) => format!("через {h} ч {mm} мин"),
            (false, 0) => format!("in {h} h"),
            (false, _) => format!("in {h} h {mm} min"),
        };
    }
    let d = sd - today;
    if d <= 1 {
        lang.pick("Завтра", "Tomorrow").to_string()
    } else if ru {
        format!("через {d} {}", plural(d as u32, "день", "дня", "дней"))
    } else {
        format!("in {d} days")
    }
}

/// "11:00" today, "Завтра, 15:30", "Пт, весь день", "26 окт, 12:00"
fn when_text(i: &Item, now: i64, lang: Lang) -> String {
    let today = now.div_euclid(86400);
    let day = i.start.div_euclid(86400).max(today);
    let t = if i.all_day { lang.pick("весь день", "all day").to_string() } else { hm(i.start) };
    if day == today && !i.all_day {
        t
    } else {
        format!("{}, {t}", day_label(day, today, lang, false))
    }
}

/// Path typed into the prompt: surrounding quotes ("Copy as path") and spaces dropped.
fn clean_path(s: &str) -> String {
    s.trim().trim_matches('"').trim().to_string()
}

// ================================================================ calendar link (https / webcal)

/// Where the calendar comes from. `BadUrl` = looks like a link (`scheme://`) but is not an acceptable one.
#[derive(Clone, Debug, PartialEq)]
enum Source {
    Path(String),
    /// normalized: `https://host[:port]/path?query`
    Url(String),
    BadUrl,
}

/// The `scheme` of a `scheme://...` text (a drive letter `C:\\` or a UNC path is not one).
fn scheme_of(s: &str) -> Option<&str> {
    let (sch, _) = s.split_once("://")?;
    (sch.len() >= 2 && sch.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'))).then_some(sch)
}

fn source_of(s: &str) -> Source {
    match scheme_of(s) {
        None => Source::Path(s.to_string()),
        Some(_) => normalize_url(s).map_or(Source::BadUrl, Source::Url),
    }
}

/// IPv4 addresses that are not on the public internet (loopback, private, link-local incl. cloud metadata, CGNAT, multicast / reserved).
fn private_v4(a: [u8; 4]) -> bool {
    matches!(a, [0 | 10 | 127, ..] | [169, 254, ..] | [192, 168, ..] | [100, 64..=127, ..] | [192, 0, 0, _] | [224..=255, ..]) || (a[0] == 172 && (16..=31).contains(&a[1]))
}

/// A subscription link made safe to fetch: `webcal(s)://` becomes `https://`, only https is accepted (a token in a plain-http link would travel
/// in the clear). Rejected: other schemes, user:password@ tricks, backslashes / spaces / non-ASCII, IPv6 literals, odd IPv4 spellings (decimal,
/// hex), single-label and local-looking hosts (`localhost`, `*.local`, `*.internal`, ...) and private / loopback / link-local addresses, so the
/// widget cannot be pointed at the user's own network. ponytail: a public name that resolves to a private address (DNS rebinding) is not caught.
fn normalize_url(raw: &str) -> std::result::Result<String, &'static str> {
    let raw = raw.trim();
    if raw.len() > 2048 || !raw.bytes().all(|b| b.is_ascii_graphic()) || raw.contains('\\') {
        return Err("characters");
    }
    let (scheme, rest) = raw.split_once("://").ok_or("scheme")?;
    if !["https", "webcal", "webcals"].iter().any(|s| s.eq_ignore_ascii_case(scheme)) {
        return Err("scheme");
    }
    let rest = rest.split('#').next().unwrap_or("");
    let end = rest.find(['/', '?']).unwrap_or(rest.len());
    let (auth, tail) = rest.split_at(end);
    if auth.contains('@') {
        return Err("userinfo");
    }
    if auth.starts_with('[') {
        return Err("ipv6");
    }
    let (host, port) = match auth.rsplit_once(':') {
        Some((h, p)) => (h, Some(p.parse::<u16>().ok().filter(|&n| n != 0 && p.bytes().all(|b| b.is_ascii_digit())).ok_or("port")?)),
        None => (auth, None),
    };
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    let labels: Vec<&str> = host.split('.').collect();
    if labels.len() < 2 || labels.iter().any(|l| l.is_empty() || l.starts_with('-') || l.ends_with('-') || !l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')) {
        return Err("host");
    }
    if ["localhost", "local", "localdomain", "internal", "lan", "home", "corp", "arpa"].contains(&labels[labels.len() - 1]) {
        return Err("local host");
    }
    let last = labels[labels.len() - 1];
    if last.bytes().all(|b| b.is_ascii_digit()) || last.starts_with("0x") {
        // an address: only the plain dotted quad (decimal / hex / octal shortcuts are how filters get bypassed)
        let oct: Vec<u8> = labels.iter().filter(|l| !l.is_empty() && l.len() <= 3 && l.bytes().all(|b| b.is_ascii_digit())).filter_map(|l| l.parse().ok()).collect();
        let quad: [u8; 4] = oct.try_into().map_err(|_| "ip")?;
        if labels.len() != 4 || private_v4(quad) {
            return Err("private address");
        }
    }
    let tail = if tail.is_empty() { "/".to_string() } else if tail.starts_with('?') { format!("/{tail}") } else { tail.to_string() };
    Ok(match port {
        Some(p) if p != 443 => format!("https://{host}:{p}{tail}"),
        _ => format!("https://{host}{tail}"),
    })
}

/// `host[:port]` of a normalized link: the only part of it that is ever logged.
fn log_host(url: &str) -> &str {
    let r = url.strip_prefix("https://").unwrap_or(url);
    &r[..r.find(['/', '?']).unwrap_or(r.len())]
}

/// An absolute link for a redirect `Location` (absolute, `//host/..`, `/path` or a relative path) based on `base`.
fn resolve_location(base: &str, loc: &str) -> Option<String> {
    let loc = loc.trim();
    if scheme_of(loc).is_some() {
        return Some(loc.to_string());
    }
    if let Some(r) = loc.strip_prefix("//") {
        return Some(format!("https://{r}"));
    }
    let origin_end = "https://".len() + log_host(base).len();
    let origin = base.get(..origin_end)?;
    if loc.starts_with('/') {
        return Some(format!("{origin}{loc}"));
    }
    let path = base.get(origin_end..)?.split(['?', '#']).next()?;
    let dir = path.rfind('/').map_or("/", |i| &path[..=i]);
    (!loc.is_empty()).then(|| format!("{origin}{dir}{loc}"))
}

/// FNV-1a 64: a stable, dependency-free name for the cache files (the link itself is a secret and stays out of file names).
fn fnv64(s: &str) -> u64 {
    s.bytes().fold(0xcbf29ce484222325u64, |h, b| (h ^ b as u64).wrapping_mul(0x100000001b3))
}

/// `nextevent-<16 hex>.ics` for a normalized link.
fn cache_name(url: &str) -> String {
    format!("nextevent-{:016x}.ics", fnv64(url))
}

fn cache_dir() -> Option<std::path::PathBuf> {
    Some(std::path::PathBuf::from(std::env::var_os("APPDATA")?).join("macshell").join("cache"))
}

/// What the server told us about the cached body (sent back as If-None-Match / If-Modified-Since).
#[derive(Clone, Debug, Default, PartialEq)]
struct Validators {
    etag: Option<String>,
    last_modified: Option<String>,
}

/// A header value we are willing to send back: printable ASCII, no CR / LF (the value came from a server).
fn header_safe(v: &str) -> bool {
    !v.is_empty() && v.len() <= 512 && v.bytes().all(|b| b == b' ' || b.is_ascii_graphic())
}

impl Validators {
    fn encode(&self) -> String {
        format!("{}\n{}\n", self.etag.as_deref().unwrap_or(""), self.last_modified.as_deref().unwrap_or(""))
    }
    fn decode(s: &str) -> Validators {
        let mut l = s.lines().map(|x| x.trim()).map(|x| Some(x.to_string()).filter(|x| header_safe(x)));
        Validators { etag: l.next().flatten(), last_modified: l.next().flatten() }
    }
    /// Request header lines (CRLF-terminated). The validators only describe a body we still have, so callers pass `Default` without a cache.
    fn request_headers(&self) -> String {
        let mut h = String::from("Accept: text/calendar, text/plain;q=0.8, */*;q=0.5\r\n");
        if let Some(e) = self.etag.as_ref().filter(|e| header_safe(e)) {
            h.push_str(&format!("If-None-Match: {e}\r\n"));
        }
        if let Some(m) = self.last_modified.as_ref().filter(|m| header_safe(m)) {
            h.push_str(&format!("If-Modified-Since: {m}\r\n"));
        }
        h
    }
}

fn looks_like_ics(b: &[u8]) -> bool {
    b.windows(15).any(|w| w.eq_ignore_ascii_case(b"BEGIN:VCALENDAR"))
}

/// temp file + rename: the cache is the old or the new body, never half of one
fn write_atomic(path: &std::path::Path, bytes: &[u8]) {
    let tmp = path.with_extension("tmp");
    if std::fs::write(&tmp, bytes).and_then(|_| std::fs::rename(&tmp, path)).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

enum Fetched {
    Fresh(Vec<u8>, Validators),
    NotModified,
}

/// GET with the 15 s / 2 MB limits; redirects are followed by hand (<= 5) and every hop is validated like the first link. `Err` = a short reason
/// that never contains the link.
fn fetch_calendar(url: &str, v: &Validators) -> std::result::Result<Fetched, String> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(FETCH_TIMEOUT_MS as u64);
    let headers = v.request_headers();
    let mut cur = url.to_string();
    for _ in 0..=MAX_HOPS {
        let left = deadline.saturating_duration_since(std::time::Instant::now()).as_millis() as i32;
        if left <= 0 {
            return Err("timeout".into());
        }
        let r = crate::net::http_get_once(&cur, left.min(FETCH_TIMEOUT_MS), MAX_BODY, &headers, Some(deadline))?;
        match r.status {
            200..=299 => {
                let keep = |v: Option<String>| v.filter(|x| header_safe(x));
                return Ok(Fetched::Fresh(r.body, Validators { etag: keep(r.etag), last_modified: keep(r.last_modified) }));
            }
            304 => return Ok(Fetched::NotModified),
            301 | 302 | 303 | 307 | 308 => {
                let next = r.location.and_then(|l| resolve_location(&cur, &l)).ok_or("bad redirect")?;
                cur = normalize_url(&next).map_err(|why| format!("redirect refused ({why})"))?;
            }
            st => return Err(format!("http {st}")),
        }
    }
    Err("too many redirects".into())
}

/// The cached body of `url`, parsed (`Missing` = nothing cached yet).
fn load_cache(url: &str, utc_off: i32) -> Loaded {
    let bytes = cache_dir().and_then(|d| std::fs::read(d.join(cache_name(url))).ok()).filter(|b| b.len() <= MAX_BODY).ok_or(Status::Missing)?;
    Ok(parse_ics(&String::from_utf8_lossy(&bytes), utc_off))
}

/// Worker for a link: conditional GET; a fresh body replaces the cache, 304 / any failure falls back to the cache (`stale` = the network
/// did not answer, so the caller retries soon), no cache and no answer = `NetFailed`.
fn load_url(url: &str, utc_off: i32, stale: &std::sync::atomic::AtomicBool) -> Loaded {
    use std::sync::atomic::Ordering::Relaxed;
    let dir = cache_dir();
    let body_path = dir.as_ref().map(|d| d.join(cache_name(url)));
    let meta_path = body_path.as_ref().map(|p| p.with_extension("meta"));
    let have_cache = body_path.as_ref().is_some_and(|p| p.is_file());
    let known = if have_cache { meta_path.as_ref().and_then(|p| std::fs::read_to_string(p).ok()).map(|t| Validators::decode(&t)).unwrap_or_default() } else { Validators::default() };
    stale.store(false, Relaxed);
    match fetch_calendar(url, &known) {
        Ok(Fetched::Fresh(body, v)) if looks_like_ics(&body) => {
            if let (Some(d), Some(bp), Some(mp)) = (&dir, &body_path, &meta_path) {
                let _ = std::fs::create_dir_all(d);
                write_atomic(bp, &body);
                write_atomic(mp, v.encode().as_bytes());
            }
            return Ok(parse_ics(&String::from_utf8_lossy(&body), utc_off));
        }
        Ok(Fetched::NotModified) => {
            if let Ok(ev) = load_cache(url, utc_off) {
                return Ok(ev);
            }
        }
        Ok(Fetched::Fresh(..)) => {
            eprintln!("mswidgets: calendar {}: the answer is not an iCalendar file", log_host(url));
            stale.store(true, Relaxed);
            return load_cache(url, utc_off).map_err(|_| Status::NotIcs);
        }
        Err(why) => eprintln!("mswidgets: calendar {}: {why}", log_host(url)),
    }
    stale.store(true, Relaxed);
    load_cache(url, utc_off).map_err(|_| Status::NetFailed)
}

// ================================================================ sample (preview)

/// A small ics relative to `now` (local seconds): exercises the real parser in previews.
fn sample_ics(now: i64, lang: Lang) -> String {
    let ts = |s: i64| {
        let c = tz::civil_from_unix(s, 0);
        format!("{:04}{:02}{:02}T{:02}{:02}00", c.y, c.mo, c.d, c.h, c.mi)
    };
    // the birthday series started 4 years ago (a leap-year multiple, so Feb 29 stays valid)
    let dt = |d: i64| {
        let (y, m, dd) = tz::civil_from_days(d);
        format!("{:04}{m:02}{dd:02}", y - 4)
    };
    let (today, minute) = (now.div_euclid(86400), now.div_euclid(60) * 60);
    let mut s = String::from("BEGIN:VCALENDAR\r\nVERSION:2.0\r\n");
    let mut add = |extra: String, title: &str| s.push_str(&format!("BEGIN:VEVENT\r\n{extra}SUMMARY:{title}\r\nEND:VEVENT\r\n"));
    let t = |ru: &'static str, en: &'static str| lang.pick(ru, en);
    add(format!("DTSTART:{}\r\nDTEND:{}\r\n", ts(minute + 52 * 60), ts(minute + 82 * 60)), t("Планёрка команды", "Team stand-up"));
    add(format!("DTSTART:{}\r\nDTEND:{}\r\n", ts(minute + 210 * 60), ts(minute + 270 * 60)), t("Обед с Анной", "Lunch with Anna"));
    add(format!("DTSTART:{}\r\nDTEND:{}\r\n", ts((today + 1) * 86400 + 15 * 3600 + 1800), ts((today + 1) * 86400 + 16 * 3600)), t("Созвон с клиентом", "Client call"));
    add(format!("DTSTART:{}\r\nRRULE:FREQ=WEEKLY;BYDAY=MO\r\n", ts((today - 14) * 86400 + 9 * 3600)), t("Стоматолог", "Dentist"));
    add(format!("DTSTART;VALUE=DATE:{}\r\nRRULE:FREQ=YEARLY\r\n", dt(today + 5)), t("День рождения Максима", "Maxim's birthday"));
    add(format!("DTSTART:{}\r\n", ts((today + 6) * 86400 + 18 * 3600)), t("Релиз 2.1", "Release 2.1"));
    s.push_str("END:VCALENDAR\r\n");
    s
}

// ================================================================ widget

#[derive(Clone, Copy, PartialEq, Debug)]
enum Status {
    Unset,
    Loading,
    Missing,
    Failed,
    /// a calendar link that is not acceptable (`https://` or `webcal://` only, no local addresses)
    BadUrl,
    /// the first fetch of a link failed and nothing is cached
    NetFailed,
    /// the link answered, but not with a calendar
    NotIcs,
    Ok,
}

/// The worker's answer: events, or the failing status.
type Loaded = std::result::Result<Vec<Event>, Status>;

#[derive(Default)]
struct NextEvent {
    started: bool,
    ver: u64,
    /// the setting as typed (path or link)
    path: Option<String>,
    src: Option<Source>,
    /// the cached copy of a link has been looked at (the network fetch follows)
    warm: bool,
    /// the last fetch of a link only got the cache: retry sooner
    stale: std::sync::Arc<std::sync::atomic::AtomicBool>,
    events: Vec<Event>,
    status: Option<Status>,
    job: Option<Pending<Loaded>>,
    prompt: Option<Pending<Option<String>>>,
    last_read: i64,
    mkey: u64,
    /// new path not yet written to cfg (written by `draw` / `mouse`, which own `&mut Cx`)
    save_path: Option<String>,
}

pub fn register(r: &mut Registry) {
    r.register(WidgetDef {
        type_id: "nextevent",
        name: ("Ближайшее событие", "Next event"),
        desc: ("События из файла .ics или по ссылке на календарь", "Upcoming events from an .ics file or a calendar link"),
        group: "calendar",
        icon: Glyph::Calendar,
        sizes: &[Size::S, Size::M, Size::L],
        default_size: Size::M,
        make: || Box::new(NextEvent::default()),
    });
}

/// Worker: read + parse the file.
fn load(path: &str, utc_off: i32) -> Loaded {
    let meta = std::fs::metadata(path).map_err(|e| if e.kind() == std::io::ErrorKind::NotFound { Status::Missing } else { Status::Failed })?;
    if !meta.is_file() || meta.len() > MAX_FILE {
        return Err(Status::Failed);
    }
    let bytes = std::fs::read(path).map_err(|_| Status::Failed)?;
    Ok(parse_ics(&String::from_utf8_lossy(&bytes), utc_off))
}

impl NextEvent {
    fn status(&self) -> Status {
        self.status.unwrap_or(Status::Unset)
    }

    /// A new setting value (path or link): forget the old events, read again at once.
    fn set_source(&mut self, raw: Option<String>) {
        self.src = raw.as_deref().map(source_of);
        self.path = raw;
        self.warm = false;
        self.last_read = 0;
        self.stale.store(false, std::sync::atomic::Ordering::Relaxed);
        self.status = Some(match &self.src {
            None => Status::Unset,
            Some(Source::BadUrl) => Status::BadUrl,
            Some(_) => Status::Loading,
        });
    }

    fn empty(&self, cx: &Cx, st: Status) {
        let b = cx.content();
        let (msg, sub) = match st {
            Status::Unset => (cx.t("Укажите календарь", "Set a calendar"), cx.t("Нажмите и вставьте путь к .ics или ссылку на календарь", "Click and paste an .ics path or a calendar link")),
            Status::Loading => (cx.t("Загрузка…", "Loading…"), ""),
            Status::Missing => (cx.t("Файл не найден", "File not found"), cx.t("Нажмите, чтобы изменить путь", "Click to change the path")),
            Status::Failed => (cx.t("Не удалось прочитать", "Cannot read the file"), cx.t("Нажмите, чтобы изменить путь", "Click to change the path")),
            Status::BadUrl => (cx.t("Ссылка не подходит", "This link is not accepted"), cx.t("Нужна ссылка https:// или webcal:// на публичный календарь", "Use an https:// or webcal:// link to a public calendar")),
            Status::NetFailed => (cx.t("Не удалось загрузить календарь", "Cannot load the calendar"), cx.t("Проверьте ссылку и сеть; нажмите, чтобы изменить", "Check the link and the network; click to change it")),
            Status::NotIcs => (cx.t("Это не календарь", "Not a calendar"), cx.t("Ссылка отвечает не файлом .ics; нажмите, чтобы изменить", "The link does not return an .ics file; click to change it")),
            Status::Ok => (cx.t("Нет событий", "No events"), cx.t("Ближайшие 13 месяцев пусты", "Nothing in the next 13 months")),
        };
        let small = cx.size == Size::S;
        let y = if small { b.y + 12.0 } else { b.y + (b.h - 110.0) / 2.0 };
        cx.icon(Glyph::Calendar, Rect::new(b.cx() - 14.0, y, 28.0, 28.0), FAINT);
        cx.text(Rect::new(b.x, y + 36.0, b.w, 36.0), msg, Style::new(13.0, 600).center().wrap(), TEXT);
        cx.text(Rect::new(b.x, y + 76.0, b.w, 30.0), sub, Style::CAPTION.center().wrap(), DIM);
        if st == Status::Unset && !small {
            cx.text(Rect::new(b.x, y + 110.0, b.w, 16.0), "config.json: \"ics\": \"C:\\\\…\\\\calendar.ics\" | \"https://…\"", Style::CAPTION.center().ellipsis(), FAINT);
        }
    }

    /// The first event as a hero block in `r` (138 px tall): day label, title, big time, relative text.
    fn hero(&self, cx: &Cx, r: Rect, it: &Item, now: i64) {
        let lang = cx.lang;
        let today = now.div_euclid(86400);
        let day = it.start.div_euclid(86400).max(today);
        cx.text(Rect::new(r.x, r.y, r.w, 16.0), &day_label(day, today, lang, true).to_uppercase(), Style::LABEL.ellipsis(), ACCENT);
        cx.text(Rect::new(r.x, r.bottom() - 16.0, r.w, 16.0), &rel_text(it, now, lang), Style::CAPTION.ellipsis(), DIM);
        let (big, tm) = if it.all_day { (Style::new(20.0, 500), lang.pick("Весь день", "All day").to_string()) } else { (Style::new(32.0, 300).mono(), hm(it.start)) };
        cx.text(Rect::new(r.x, r.bottom() - 54.0, r.w, 38.0), &tm, big.ellipsis(), TEXT);
        let title = if it.title.is_empty() { cx.t("Без названия", "Untitled") } else { it.title.as_str() };
        let top = r.y + 22.0;
        let (lines, size, lh) = fit_title(cx, title, r.w, r.bottom() - 58.0 - top);
        for (i, l) in lines.iter().enumerate() {
            cx.text(Rect::new(r.x, top + i as f32 * lh, r.w, lh), l, Style::new(size, 600).ellipsis(), TEXT);
        }
    }

    /// Two-line mini row of the M list: when (dim) over the title.
    fn mini_row(&self, cx: &Cx, r: Rect, it: &Item, now: i64) {
        cx.text(Rect::new(r.x, r.y, r.w, 14.0), &when_text(it, now, cx.lang), Style::CAPTION.ellipsis(), DIM);
        let title = if it.title.is_empty() { cx.t("Без названия", "Untitled") } else { it.title.as_str() };
        let st = [13.0, 12.0].iter().map(|&z| Style::new(z, 500)).find(|&s| cx.measure(title, s, 0.0).0 <= r.w).unwrap_or(Style::new(12.0, 500));
        cx.text(Rect::new(r.x, r.y + 15.0, r.w, 18.0), title, st.ellipsis(), TEXT);
    }

    /// L row (48 px): time / day on the left, a marker, the title (up to 2 lines) or title + relative text.
    fn list_row(&self, cx: &Cx, r: Rect, it: &Item, now: i64, first: bool) {
        let lang = cx.lang;
        let today = now.div_euclid(86400);
        let day = it.start.div_euclid(86400).max(today);
        let tm = if it.all_day { lang.pick("Весь день", "All day").to_string() } else { hm(it.start) };
        let ts = if it.all_day { Style::new(12.0, 600) } else { Style::new(16.0, 400).mono() };
        cx.text(Rect::new(r.x, r.y + 6.0, 66.0, 20.0), &tm, ts.ellipsis(), TEXT);
        cx.text(Rect::new(r.x, r.y + 26.0, 66.0, 15.0), &day_label(day, today, lang, false), Style::CAPTION.ellipsis(), DIM);
        cx.pill(Rect::new(r.x + 72.0, r.y + 8.0, 3.0, r.h - 16.0), if first { ACCENT } else { FILL });
        let x = r.x + 86.0;
        let w = r.right() - x;
        let title = if it.title.is_empty() { cx.t("Без названия", "Untitled") } else { it.title.as_str() };
        let st = Style::new(13.0, 500);
        let lines = wrap_lines(title, 2, w, &|s| cx.measure(s, st, 0.0).0);
        if lines.len() > 1 {
            for (k, l) in lines.iter().enumerate() {
                cx.text(Rect::new(x, r.y + 5.0 + k as f32 * 17.0, w, 17.0), l, st.ellipsis(), TEXT);
            }
        } else {
            cx.text(Rect::new(x, r.y + 6.0, w, 18.0), title, st.ellipsis(), TEXT);
            cx.text(Rect::new(x, r.y + 25.0, w, 16.0), &rel_text(it, now, lang), Style::CAPTION.ellipsis(), DIM);
        }
    }
}

/// Title lines: the biggest of 16 / 14 / 13 px whose wrapped height fits `h`, else 13 px cut with an ellipsis.
fn fit_title(cx: &Cx, text: &str, w: f32, h: f32) -> (Vec<String>, f32, f32) {
    for (size, lh) in [(16.0, 20.0), (14.0, 18.0), (13.0, 17.0)] {
        let st = Style::new(size, 600);
        let lines = wrap_lines(text, 99, w, &|s| cx.measure(s, st, 0.0).0);
        if lines.len() as f32 * lh <= h {
            return (lines, size, lh);
        }
    }
    let st = Style::new(13.0, 600);
    let n = ((h / 17.0) as usize).max(1);
    (wrap_lines(text, n, w, &|s| cx.measure(s, st, 0.0).0), 13.0, 17.0)
}

impl Widget for NextEvent {
    fn draw(&mut self, cx: &mut Cx) -> Result<()> {
        if let Some(p) = self.save_path.take() {
            cx.cfg_set("ics", json!(p));
        }
        if self.status() != Status::Ok {
            self.empty(cx, self.status());
            return Ok(());
        }
        let now = cx.now.local_secs();
        let items = upcoming(&self.events, now, MAX_ITEMS);
        let Some(first) = items.first() else {
            self.empty(cx, Status::Ok);
            return Ok(());
        };
        let c = cx.content();
        match cx.size {
            Size::S => self.hero(cx, c, first, now),
            Size::M => {
                self.hero(cx, Rect::new(c.x, c.y, 148.0, c.h), first, now);
                cx.rect(Rect::new(c.x + 160.0, c.y + 2.0, 0.6, c.h - 4.0), FILL);
                let list = Rect::new(c.x + 174.0, c.y, c.w - 174.0, c.h);
                if items.len() < 2 {
                    cx.text(list, cx.t("Дальше пусто", "Nothing after that"), Style::BODY.center(), FAINT);
                }
                for (i, it) in items.iter().skip(1).take(3).enumerate() {
                    self.mini_row(cx, Rect::new(list.x, list.y + 2.0 + i as f32 * 46.0, list.w, 40.0), it, now);
                }
            }
            Size::L => {
                cx.text(Rect::new(c.x, c.y, c.w, 18.0), cx.t("БЛИЖАЙШИЕ СОБЫТИЯ", "UPCOMING EVENTS"), Style::LABEL.ellipsis(), ACCENT);
                for (i, it) in items.iter().enumerate() {
                    let r = Rect::new(c.x, c.y + 30.0 + i as f32 * 48.0, c.w, 48.0);
                    if i > 0 {
                        cx.rect(Rect::new(r.x, r.y, r.w, 0.6), FILL);
                    }
                    self.list_row(cx, r, it, now, i == 0);
                }
            }
        }
        Ok(())
    }

    fn tick(&mut self, cx: &Cx) -> Tick {
        if !self.started {
            self.started = true;
            self.mkey = cx.now.minute_key();
            if cx.preview {
                self.events = parse_ics(&sample_ics(cx.now.local_secs(), cx.lang), cx.utc_offset_min());
                self.status = Some(Status::Ok);
                self.ver += 1;
                return Tick::Never;
            }
            self.set_source(cx.cfg_str("ics").map(clean_path).filter(|p| !p.is_empty()));
            self.ver += 1;
        }
        if cx.preview {
            return Tick::Never;
        }
        if let Some(p) = &self.prompt {
            if let Some(r) = p.take() {
                self.prompt = None;
                if let Some(s) = r {
                    let s = clean_path(&s);
                    self.set_source(Some(s.clone()).filter(|p| !p.is_empty()));
                    self.save_path = Some(s);
                    self.events.clear();
                    self.job = None;
                    self.ver += 1;
                }
            } else if !p.busy() {
                self.prompt = None;
            }
        }
        let now = cx.utc_secs();
        if let Some(j) = &self.job {
            if let Some(r) = j.take() {
                self.job = None;
                match r {
                    Ok(ev) => {
                        self.events = ev;
                        self.status = Some(Status::Ok);
                    }
                    // a link with nothing cached yet: stay on "Loading…" until the network answers
                    Err(Status::Missing) if matches!(self.src, Some(Source::Url(_))) => {}
                    Err(st) => {
                        self.events.clear();
                        self.status = Some(st);
                    }
                }
                self.ver += 1;
            } else if !j.busy() {
                self.job = None;
                self.status = Some(Status::Failed);
                self.ver += 1;
            }
        }
        if self.job.is_none() {
            let off = cx.utc_offset_min();
            match self.src.clone() {
                Some(Source::Path(path)) if now - self.last_read >= REFRESH_SECS => {
                    self.last_read = now;
                    self.job = Some(cx.spawn(move || load(&path, off)));
                }
                // a link: the cached copy first (instant, works offline), then the network every 30 min
                Some(Source::Url(url)) if !self.warm => {
                    self.warm = true;
                    self.job = Some(cx.spawn(move || load_cache(&url, off)));
                }
                Some(Source::Url(url)) if now - self.last_read >= URL_REFRESH_SECS => {
                    self.last_read = now;
                    let stale = self.stale.clone();
                    self.job = Some(cx.spawn(move || load_url(&url, off, &stale)));
                }
                _ => {}
            }
        }
        if self.job.is_none() && self.stale.swap(false, std::sync::atomic::Ordering::Relaxed) {
            self.last_read = now - (URL_REFRESH_SECS - URL_RETRY_SECS); // only the cache could be shown: look again in 5 min
        }
        let mk = cx.now.minute_key();
        if mk != self.mkey {
            self.mkey = mk;
            self.ver += 1; // "через 52 мин" counts down
        }
        Tick::Minute
    }

    fn mouse(&mut self, ev: &MouseEv, cx: &mut Cx) -> bool {
        if let Some(p) = self.save_path.take() {
            cx.cfg_set("ics", json!(p));
        }
        if ev.clicked(cx.bounds()) && !cx.preview && self.prompt.is_none() {
            let cur = self.path.clone().unwrap_or_default();
            self.prompt = Some(cx.prompt_text_ex(cx.t("Путь к .ics или ссылка на календарь", "Path to an .ics file or a calendar link"), &cur, false, r"C:\Users\you\calendar.ics  |  https://…", ""));
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

    const ICS: &str = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nDTSTART:20261024T100000Z\r\nDTEND:20261024T110000Z\r\nSUMMARY:Team\\, sync\r\n  with notes\r\nEND:VEVENT\r\nBEGIN:VEVENT\r\nDTSTART;VALUE=DATE:20261030\r\nSUMMARY:Holiday\r\nEND:VEVENT\r\nBEGIN:VEVENT\r\nDTSTART:20261101T090000\r\nSTATUS:CANCELLED\r\nSUMMARY:Gone\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";

    fn day(y: i32, m: u32, d: u32) -> i64 {
        tz::days_from_civil(y, m, d) * 86400
    }

    #[test]
    fn parses_events() {
        let ev = parse_ics(ICS, 180); // UTC+3
        assert_eq!(ev.len(), 2, "cancelled one dropped");
        assert_eq!(ev[0].title, "Team, sync with notes", "unfold + unescape");
        assert_eq!(ev[0].start, day(2026, 10, 24) + 13 * 3600, "UTC 10:00 = 13:00 local");
        assert_eq!(ev[0].end - ev[0].start, 3600);
        assert!(!ev[0].all_day);
        assert!(ev[1].all_day);
        assert_eq!(ev[1].start, day(2026, 10, 30));
        assert_eq!(ev[1].end, day(2026, 10, 31), "all-day without DTEND lasts one day");
    }

    #[test]
    fn date_times() {
        assert_eq!(parse_dt("20261024T103000", 0), Some((day(2026, 10, 24) + 10 * 3600 + 1800, false)));
        assert_eq!(parse_dt("20261024T103000Z", -300), Some((day(2026, 10, 24) + 5 * 3600 + 1800, false)));
        assert_eq!(parse_dt("20261024", 60), Some((day(2026, 10, 24), true)));
        assert_eq!(parse_dt("20261340", 0), None);
        assert_eq!(parse_dt("20260230", 0), None);
        assert_eq!(parse_dt("20261024T2561", 0), None);
        assert_eq!(parse_dt("junk", 0), None);
    }

    #[test]
    fn alarms_are_ignored() {
        let t = "BEGIN:VEVENT\nDTSTART:20261024T100000\nSUMMARY:Real\nBEGIN:VALARM\nSUMMARY:Alarm text\nEND:VALARM\nEND:VEVENT\n";
        let ev = parse_ics(t, 0);
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].title, "Real");
    }

    fn ev(start: i64, rule: &str) -> Event {
        Event { start, end: start + 3600, all_day: false, title: "x".into(), rule: parse_rule(rule, 0), exdates: vec![] }
    }

    #[test]
    fn rules() {
        assert!(parse_rule("FREQ=YEARLY;BYMONTHDAY=3", 0).is_none());
        assert!(parse_rule("FREQ=MONTHLY;BYDAY=1MO", 0).is_none());
        assert!(parse_rule("FREQ=SECONDLY", 0).is_none());
        assert!(parse_rule("INTERVAL=2", 0).is_none());
        let r = parse_rule("FREQ=WEEKLY;INTERVAL=2;BYDAY=WE,MO;COUNT=5;WKST=MO", 0).unwrap();
        assert_eq!((r.freq, r.interval, r.count, r.days.clone()), (Freq::Weekly, 2, Some(5), vec![0, 2]));
        assert_eq!(parse_rule("FREQ=DAILY;UNTIL=20261031", 0).unwrap().until, Some(day(2026, 10, 31) + 86399));
    }

    #[test]
    fn daily_weekly_monthly_yearly() {
        let t = |d: i64| day(2026, 10, 24) + d * 86400 + 9 * 3600; // Saturday 09:00 + d days
        let daily = occurrences(&ev(t(0), "FREQ=DAILY;INTERVAL=2;COUNT=3"), t(100));
        assert_eq!(daily, vec![t(0), t(2), t(4)]);
        let until = occurrences(&ev(t(0), "FREQ=DAILY;UNTIL=20261026"), t(100));
        assert_eq!(until, vec![t(0), t(1), t(2)]);
        // weekly Mon+Wed starting on a Saturday: first hits are Mon 26 and Wed 28; Saturday itself is NOT in BYDAY
        let w = occurrences(&ev(t(0), "FREQ=WEEKLY;BYDAY=MO,WE"), t(12));
        assert_eq!(w, vec![t(2), t(4), t(9), t(11)]);
        // weekly without BYDAY repeats the weekday of DTSTART
        assert_eq!(occurrences(&ev(t(0), "FREQ=WEEKLY"), t(15)), vec![t(0), t(7), t(14)]);
        // monthly on the 31st skips short months
        let m = occurrences(&ev(day(2026, 1, 31) + 3600, "FREQ=MONTHLY;COUNT=4"), day(2027, 1, 1));
        let days: Vec<_> = m.iter().map(|s| tz::civil_from_days(s.div_euclid(86400))).collect();
        assert_eq!(days, vec![(2026, 1, 31), (2026, 3, 31), (2026, 5, 31), (2026, 7, 31)]);
        // yearly + exdate
        let mut y = ev(day(2020, 2, 29), "FREQ=YEARLY");
        y.exdates.push(day(2028, 2, 29));
        let ys = occurrences(&y, day(2033, 1, 1));
        assert_eq!(ys, vec![day(2020, 2, 29), day(2024, 2, 29), day(2032, 2, 29)], "non-leap years skipped, 2028 excluded");
    }

    #[test]
    fn upcoming_orders_and_filters() {
        let now = day(2026, 10, 24) + 10 * 3600;
        let events = vec![
            Event { start: day(2026, 10, 24) + 9 * 3600, end: day(2026, 10, 24) + 11 * 3600, all_day: false, title: "ongoing".into(), rule: None, exdates: vec![] },
            Event { start: day(2026, 10, 24) + 8 * 3600, end: day(2026, 10, 24) + 9 * 3600, all_day: false, title: "past".into(), rule: None, exdates: vec![] },
            Event { start: day(2026, 10, 24), end: day(2026, 10, 25), all_day: true, title: "today all-day".into(), rule: None, exdates: vec![] },
            Event { start: day(2026, 10, 24) + 15 * 3600, end: day(2026, 10, 24) + 16 * 3600, all_day: false, title: "later".into(), rule: None, exdates: vec![] },
            ev(day(2026, 10, 20) + 12 * 3600, "FREQ=DAILY"),
        ];
        let u = upcoming(&events, now, 6);
        let titles: Vec<&str> = u.iter().map(|i| i.title.as_str()).collect();
        assert_eq!(&titles[..4], ["ongoing", "x", "later", "today all-day"], "all-day sorts after the timed events of its day: {titles:?}");
        assert!(!titles.contains(&"past"));
        assert_eq!(upcoming(&events, now, 2).len(), 2);
    }

    #[test]
    fn labels() {
        let now = day(2026, 10, 24) + 10 * 3600 + 8 * 60; // Saturday 10:08
        let it = |s: i64, e: i64, all| Item { start: s, end: e, all_day: all, title: String::new() };
        assert_eq!(rel_text(&it(now + 52 * 60, now + 82 * 60, false), now, Lang::Ru), "через 52 мин");
        assert_eq!(rel_text(&it(now + 140 * 60, now + 150 * 60, false), now, Lang::Ru), "через 2 ч 20 мин");
        assert_eq!(rel_text(&it(now + 120 * 60, now + 130 * 60, false), now, Lang::En), "in 2 h");
        assert_eq!(rel_text(&it(now - 60, now + 1800, false), now, Lang::Ru), format!("Сейчас, до {}", hm(now + 1800)));
        assert_eq!(rel_text(&it(day(2026, 10, 25) + 15 * 3600, day(2026, 10, 25) + 16 * 3600, false), now, Lang::Ru), "Завтра");
        assert_eq!(rel_text(&it(day(2026, 10, 27) + 9 * 3600, day(2026, 10, 27) + 10 * 3600, false), now, Lang::Ru), "через 3 дня");
        assert_eq!(rel_text(&it(day(2026, 10, 31), day(2026, 11, 1), true), now, Lang::En), "in 7 days");
        assert_eq!(when_text(&it(now + 3600, now + 7200, false), now, Lang::Ru), "11:08");
        assert_eq!(when_text(&it(day(2026, 10, 25) + 15 * 3600 + 1800, 0, false), now, Lang::Ru), "Завтра, 15:30");
        assert_eq!(when_text(&it(day(2026, 10, 27) + 9 * 3600, 0, false), now, Lang::En), "Tue, 09:00");
        assert_eq!(when_text(&it(day(2026, 11, 3), day(2026, 11, 4), true), now, Lang::Ru), "3 ноя, весь день");
        assert_eq!(day_label(day(2026, 10, 27).div_euclid(86400), now.div_euclid(86400), Lang::Ru, true), "Вторник");
        assert_eq!(day_label(day(2026, 11, 3).div_euclid(86400), now.div_euclid(86400), Lang::En, true), "November 3");
    }

    #[test]
    fn sample_parses_and_is_stable() {
        let now = Now::sample().local_secs();
        for lang in [Lang::Ru, Lang::En] {
            let ev = parse_ics(&sample_ics(now, lang), 180);
            assert_eq!(ev.len(), 6);
            let u = upcoming(&ev, now, MAX_ITEMS);
            assert_eq!(u.len(), 6);
            assert_eq!(u[0].start - now, 51 * 60 + 25, "first event 52 min away (ceil)");
            assert!(u.iter().any(|i| i.all_day), "yearly all-day birthday expanded");
            assert!(u.windows(2).all(|w| w[0].start <= w[1].start || w[1].all_day));
        }
    }

    #[test]
    fn path_cleanup_and_missing_file() {
        assert_eq!(clean_path("  \"C:\\a b\\c.ics\" "), "C:\\a b\\c.ics");
        assert_eq!(load("Z:\\definitely\\missing.ics", 0), Err(Status::Missing));
    }

    #[test]
    fn webcal_is_rewritten_to_https() {
        assert_eq!(normalize_url("webcal://example.com/cal.ics?token=abc").unwrap(), "https://example.com/cal.ics?token=abc");
        assert_eq!(normalize_url("WEBCAL://Example.COM").unwrap(), "https://example.com/");
        assert_eq!(normalize_url("webcals://example.com/a#frag").unwrap(), "https://example.com/a");
        assert_eq!(normalize_url("  https://example.com?x=1  ").unwrap(), "https://example.com/?x=1");
        assert_eq!(normalize_url("https://example.com:443/x").unwrap(), "https://example.com/x");
        assert_eq!(normalize_url("https://example.com:8443/x").unwrap(), "https://example.com:8443/x");
        assert_eq!(normalize_url("https://calendar.google.com/calendar/ical/me%40gmail.com/private-0f3a/basic.ics").unwrap(), "https://calendar.google.com/calendar/ical/me%40gmail.com/private-0f3a/basic.ics");
    }

    #[test]
    fn urls_that_must_be_refused() {
        for bad in [
            "http://example.com/a.ics",
            "ftp://example.com/a.ics",
            "file:///C:/cal.ics",
            "javascript://example.com/%0aalert(1)",
            "https://good.com@evil.com/x",
            "https://user:pw@example.com/",
            "https://@example.com/",
            "https://example.com\\@evil.com/",
            "https://example.com%40evil.com/",
            "https://exa mple.com/",
            "https://example.com/a b",
            "https:///path",
            "https://",
            "https://example.com:0/",
            "https://example.com:99999/",
            "https://example.com:80a/",
            "https://пример.рф/",
            "https://localhost/a.ics",
            "https://app.localhost/a.ics",
            "https://nas/a.ics",
            "https://printer.local/a.ics",
            "https://host.internal/a.ics",
            "https://127.0.0.1/a.ics",
            "https://10.1.2.3/a.ics",
            "https://172.16.0.9/a.ics",
            "https://172.31.255.1/a.ics",
            "https://192.168.0.1/a.ics",
            "https://169.254.169.254/latest/meta-data",
            "https://100.64.0.1/a.ics",
            "https://0.0.0.0/a.ics",
            "https://224.0.0.1/a.ics",
            "https://2130706433/a.ics",
            "https://0x7f.0.0.1/a.ics",
            "https://127.1/a.ics",
            "https://[::1]/a.ics",
            "https://[fe80::1]/a.ics",
        ] {
            assert!(normalize_url(bad).is_err(), "{bad} must be refused");
            assert_eq!(source_of(bad), Source::BadUrl, "{bad}");
        }
        assert!(normalize_url(&format!("https://example.com/{}", "a".repeat(2100))).is_err());
        // public addresses next to the private ranges are fine
        assert!(normalize_url("https://172.32.0.1/a.ics").is_ok());
        assert!(normalize_url("https://8.8.8.8/a.ics").is_ok());
        assert!(normalize_url("https://100.128.0.1/a.ics").is_ok());
    }

    #[test]
    fn paths_stay_paths() {
        assert_eq!(source_of(r"C:\Users\me\cal.ics"), Source::Path(r"C:\Users\me\cal.ics".into()));
        assert_eq!(source_of(r"\\nas\share\cal.ics"), Source::Path(r"\\nas\share\cal.ics".into()));
        assert_eq!(source_of("cal.ics"), Source::Path("cal.ics".into()));
        assert_eq!(source_of("webcal://example.com/c.ics"), Source::Url("https://example.com/c.ics".into()));
        assert_eq!(source_of("http://example.com/c.ics"), Source::BadUrl);
    }

    #[test]
    fn redirects_are_resolved_then_revalidated() {
        let base = "https://example.com/cal/feed.ics?token=s3cret";
        assert_eq!(resolve_location(base, "https://cdn.example.net/x.ics").as_deref(), Some("https://cdn.example.net/x.ics"));
        assert_eq!(resolve_location(base, "//cdn.example.net/x.ics").as_deref(), Some("https://cdn.example.net/x.ics"));
        assert_eq!(resolve_location(base, "/other.ics?a=1").as_deref(), Some("https://example.com/other.ics?a=1"));
        assert_eq!(resolve_location(base, "next.ics").as_deref(), Some("https://example.com/cal/next.ics"));
        assert_eq!(resolve_location("https://example.com:8443", "x.ics").as_deref(), Some("https://example.com:8443/x.ics"));
        assert_eq!(normalize_url("https://example.com./a").unwrap(), "https://example.com/a", "a trailing dot is dropped");
        assert_eq!(resolve_location(base, ""), None);
        // a redirect to http, to localhost or to a private address dies in normalize_url
        for loc in ["http://example.com/x", "https://localhost/x", "https://192.168.1.1/x", "//169.254.169.254/x"] {
            let next = resolve_location(base, loc).unwrap();
            assert!(normalize_url(&next).is_err(), "{loc}");
        }
    }

    #[test]
    fn cache_names_are_stable_and_hide_the_link() {
        let u = "https://example.com/cal.ics?token=s3cret";
        let n = cache_name(u);
        assert_eq!(n, cache_name(u));
        assert_ne!(n, cache_name("https://example.com/cal.ics?token=other"));
        assert!(n.starts_with("nextevent-") && n.ends_with(".ics") && n.len() == "nextevent-".len() + 16 + ".ics".len(), "{n}");
        assert!(n["nextevent-".len()..n.len() - 4].bytes().all(|b| b.is_ascii_hexdigit()));
        assert!(!n.contains("example") && !n.contains("s3cret"));
        assert_eq!(log_host(u), "example.com");
        assert_eq!(log_host("https://example.com:8443/x?t=1"), "example.com:8443");
        assert_eq!(log_host("https://example.com?t=1"), "example.com");
    }

    #[test]
    fn validators_round_trip_and_headers_are_safe() {
        let v = Validators { etag: Some("\"abc-123\"".into()), last_modified: Some("Sat, 24 Oct 2026 10:00:00 GMT".into()) };
        assert_eq!(Validators::decode(&v.encode()), v);
        let h = v.request_headers();
        assert!(h.contains("If-None-Match: \"abc-123\"\r\n") && h.contains("If-Modified-Since: Sat, 24 Oct 2026 10:00:00 GMT\r\n"));
        assert!(h.ends_with("\r\n") && h.starts_with("Accept: text/calendar"));
        assert_eq!(Validators::default().request_headers().matches("\r\n").count(), 1, "no validators, no conditional headers");
        assert_eq!(Validators::decode("\n\n"), Validators::default());
        // a value with a line break would split the header: dropped
        assert!(!header_safe("a\r\nX-Evil: 1"));
        assert!(!header_safe(""));
        assert_eq!(Validators::decode("a\u{7f}b\nok").last_modified.as_deref(), Some("ok"));
        let evil = Validators { etag: Some("x\r\nHost: evil".into()), last_modified: None };
        assert_eq!(evil.request_headers().matches("\r\n").count(), 1);
    }

    #[test]
    fn calendar_detection() {
        assert!(looks_like_ics(b"\xef\xbb\xbfBEGIN:VCALENDAR\r\nVERSION:2.0"));
        assert!(looks_like_ics(b"begin:vcalendar"));
        assert!(!looks_like_ics(b"<html><body>Sign in</body></html>"));
        assert!(!looks_like_ics(b""));
    }

    #[test]
    fn cached_calendar_is_read_back() {
        let dir = std::env::temp_dir().join(format!("msw-ne-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(cache_name("https://example.com/x.ics"));
        write_atomic(&p, ICS.as_bytes());
        assert!(!p.with_extension("tmp").exists());
        let ev = parse_ics(&String::from_utf8_lossy(&std::fs::read(&p).unwrap()), 180);
        assert_eq!(ev.len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
