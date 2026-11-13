//! UI language. Every user-visible string in the app is a `(ru, en)` pair picked with `Lang::pick` / `Cx::t` / `i18n::tr`; this module
//! owns the SETTING (`lang` in config.json: "auto" | "ru" | "en", a missing key = auto), its resolution against the Windows UI language
//! and the one process-wide current language that headless tools, prompts and the tray read.
//!
//! * Auto = Russian when the Windows UI language is Russian, English for every other language.
//! * The host changes the language live (`App::set_language`): it stores the preference here, then tells the UI and the scene.
//! * `current()` is a relaxed atomic load: free to call on any thread, even per frame.
use crate::widget::Lang;
use serde_json::Value;
use std::sync::atomic::{AtomicU8, Ordering};

/// What the user picked: follow Windows, or a fixed language.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum LangPref {
    #[default]
    Auto,
    Ru,
    En,
}

impl LangPref {
    pub const ALL: [LangPref; 3] = [LangPref::Auto, LangPref::Ru, LangPref::En];

    /// The `lang` value of config.json.
    pub fn key(self) -> &'static str {
        match self {
            LangPref::Auto => "auto",
            LangPref::Ru => "ru",
            LangPref::En => "en",
        }
    }

    /// "auto" | "ru" | "en" (case and region ignored: "EN-us" is English).
    pub fn parse(s: &str) -> Option<LangPref> {
        let s = s.trim().to_ascii_lowercase();
        match s.as_str() {
            "auto" | "" => Some(LangPref::Auto),
            _ if s.starts_with("ru") => Some(LangPref::Ru),
            _ if s.starts_with("en") => Some(LangPref::En),
            _ => None,
        }
    }

    /// The preference stored in a config.json root: a missing or unknown `lang` is Auto.
    pub fn from_value(root: &Value) -> LangPref {
        root.get("lang").and_then(Value::as_str).and_then(LangPref::parse).unwrap_or_default()
    }

    /// Position in `ALL` (the segment of the settings panel).
    pub fn index(self) -> usize {
        self as usize
    }
    pub fn from_index(k: usize) -> LangPref {
        Self::ALL[k.min(2)]
    }
}

/// The language of a Windows UI language id (`LANGID`): Russian for the Russian primary language, English for everything else.
pub fn lang_from_langid(id: u16) -> Lang {
    const LANG_RUSSIAN: u16 = 0x19;
    if id & 0x3ff == LANG_RUSSIAN {
        Lang::Ru
    } else {
        Lang::En
    }
}

#[cfg(windows)]
#[link(name = "kernel32")]
extern "system" {
    fn GetUserDefaultUILanguage() -> u16;
}

/// The language Windows displays its own UI in (the first preferred display language of the user).
pub fn system_lang() -> Lang {
    #[cfg(windows)]
    {
        lang_from_langid(unsafe { GetUserDefaultUILanguage() })
    }
    #[cfg(not(windows))]
    {
        Lang::En
    }
}

/// The language a preference means on a system whose UI language is `sys`.
pub fn resolve(pref: LangPref, sys: Lang) -> Lang {
    match pref {
        LangPref::Auto => sys,
        LangPref::Ru => Lang::Ru,
        LangPref::En => Lang::En,
    }
}

const UNSET: u8 = 255;
static PREF: AtomicU8 = AtomicU8::new(UNSET);
static CUR: AtomicU8 = AtomicU8::new(UNSET);

fn code(l: Lang) -> u8 {
    (l == Lang::En) as u8
}

/// Set the preference and recompute the current language. Returns the language now in force.
pub fn set_pref(p: LangPref) -> Lang {
    let l = resolve(p, system_lang());
    PREF.store(p as u8, Ordering::Relaxed);
    CUR.store(code(l), Ordering::Relaxed);
    l
}

/// The preference in force (read from config.json on first use).
pub fn pref() -> LangPref {
    init_once();
    LangPref::from_index(PREF.load(Ordering::Relaxed) as usize)
}

/// The language in force. The first call reads `lang` from config.json (headless tools and prompts get the right language
/// without any set-up); the host calls `set_pref` itself at start and on every change.
pub fn current() -> Lang {
    let c = CUR.load(Ordering::Relaxed);
    if c == UNSET {
        init_once();
        return from_code(CUR.load(Ordering::Relaxed));
    }
    from_code(c)
}

fn from_code(c: u8) -> Lang {
    if c == 1 {
        Lang::En
    } else {
        Lang::Ru
    }
}

fn init_once() {
    if CUR.load(Ordering::Relaxed) == UNSET {
        let p = crate::layout::try_read_config().map_or(LangPref::Auto, |r| LangPref::from_value(&r));
        set_pref(p);
    }
}

/// `ru` or `en` by the current language, for code that has no `Cx` / `Lang` at hand (prompts, worker threads, OS messages).
pub fn tr(ru: &'static str, en: &'static str) -> &'static str {
    current().pick(ru, en)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_and_keys_round_trip() {
        for p in LangPref::ALL {
            assert_eq!(LangPref::parse(p.key()), Some(p));
            assert_eq!(LangPref::from_index(p.index()), p);
        }
        assert_eq!(LangPref::parse("EN-us"), Some(LangPref::En));
        assert_eq!(LangPref::parse(" ru-RU "), Some(LangPref::Ru));
        assert_eq!(LangPref::parse("de"), None);
        assert_eq!(LangPref::from_index(99), LangPref::En);
    }

    #[test]
    fn old_configs_keep_working() {
        // no key (every config written before the setting existed) = auto
        assert_eq!(LangPref::from_value(&json!({})), LangPref::Auto);
        assert_eq!(LangPref::from_value(&json!({"lang": 5})), LangPref::Auto);
        assert_eq!(LangPref::from_value(&json!({"lang": "klingon"})), LangPref::Auto);
        // the two values the key already had before the setting existed
        assert_eq!(LangPref::from_value(&json!({"lang": "en"})), LangPref::En);
        assert_eq!(LangPref::from_value(&json!({"lang": "ru"})), LangPref::Ru);
    }

    #[test]
    fn auto_follows_the_windows_ui_language() {
        assert_eq!(lang_from_langid(0x0419), Lang::Ru, "ru-RU");
        assert_eq!(lang_from_langid(0x0409), Lang::En, "en-US");
        assert_eq!(lang_from_langid(0x0407), Lang::En, "de-DE");
        assert_eq!(lang_from_langid(0x0422), Lang::En, "uk-UA is not Russian");
        assert_eq!(lang_from_langid(0x0809), Lang::En, "en-GB");
        assert_eq!(resolve(LangPref::Auto, Lang::Ru), Lang::Ru);
        assert_eq!(resolve(LangPref::Auto, Lang::En), Lang::En);
    }

    #[test]
    fn fixed_choices_ignore_the_system() {
        for sys in [Lang::Ru, Lang::En] {
            assert_eq!(resolve(LangPref::Ru, sys), Lang::Ru);
            assert_eq!(resolve(LangPref::En, sys), Lang::En);
        }
    }

    #[test]
    fn current_follows_set_pref() {
        // the only test that touches the process-wide state
        assert_eq!(set_pref(LangPref::En), Lang::En);
        assert_eq!((current(), pref(), tr("да", "yes")), (Lang::En, LangPref::En, "yes"));
        assert_eq!(set_pref(LangPref::Ru), Lang::Ru);
        assert_eq!((current(), pref(), tr("да", "yes")), (Lang::Ru, LangPref::Ru, "да"));
        set_pref(LangPref::Auto);
        assert_eq!(current(), system_lang());
    }

    #[test]
    fn every_widget_has_english_text() {
        let all = crate::registry::global().all();
        assert!(all.len() >= 25);
        for d in all {
            for (what, (ru, en)) in [("name", d.name), ("description", d.desc)] {
                assert!(!en.trim().is_empty(), "{} {what}: empty English", d.type_id);
                assert!(!en.chars().any(is_cyr), "{} {what}: Cyrillic in the English text {en:?}", d.type_id);
                assert!(ru.chars().any(is_cyr), "{} {what}: no Russian text {ru:?}", d.type_id);
            }
            // the gallery search finds a widget by either language
            assert!(crate::grid::matches(&d.name.1.to_lowercase(), &[d.name.0, d.name.1, d.desc.0, d.desc.1]), "{}", d.type_id);
        }
    }

    // ---------------------------------------------------------------- the source lint
    // Every user-visible string is a `(ru, en)` pair of literals. This walks the sources and fails when an English half is empty,
    // still Cyrillic, or when a NEW Russian-only literal appears (the per-file allowance below is a ratchet: it may only shrink).

    /// (byte offset of the opening quote, byte offset after the closing quote, the text between, 1-based line)
    type Lit<'a> = (usize, usize, &'a str, usize);

    fn is_cyr(c: char) -> bool {
        ('\u{0400}'..='\u{04ff}').contains(&c)
    }

    /// The string literals of a Rust source (comments, char literals and everything from `#[cfg(test)]` on are skipped).
    fn literals(src: &str) -> Vec<Lit<'_>> {
        let end = src.find("#[cfg(test)]").unwrap_or(src.len());
        let b = &src.as_bytes()[..end];
        let (mut i, mut line, mut out) = (0, 1, Vec::new());
        while i < b.len() {
            match b[i] {
                b'\n' => {
                    line += 1;
                    i += 1;
                }
                b'/' if b.get(i + 1) == Some(&b'/') => {
                    while i < b.len() && b[i] != b'\n' {
                        i += 1;
                    }
                }
                b'/' if b.get(i + 1) == Some(&b'*') => {
                    while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                        line += (b[i] == b'\n') as usize;
                        i += 1;
                    }
                    i += 2;
                }
                b'\'' => {
                    // a char literal ('"', '\n', 'я') is skipped whole; a lifetime has no closing quote right behind its letter
                    let rest = &src[i + 1..end];
                    let mut it = rest.chars();
                    match it.next() {
                        Some('\\') => i += 1 + rest[1..].find('\'').map_or(1, |k| k + 2),
                        Some(c) if it.next() == Some('\'') => i += 1 + c.len_utf8() + 1,
                        _ => i += 1,
                    }
                }
                b'r' if (i == 0 || !(b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'_')) && matches!(b.get(i + 1), Some(b'"' | b'#')) => {
                    let hashes = b[i + 1..].iter().take_while(|&&c| c == b'#').count();
                    if b.get(i + 1 + hashes) != Some(&b'"') {
                        i += 1;
                        continue;
                    }
                    let open = i;
                    let body = i + 2 + hashes;
                    let close = format!("\"{}", "#".repeat(hashes));
                    let Some(k) = src[body..end].find(&close) else { break };
                    out.push((open, body + k + close.len(), &src[body..body + k], line));
                    line += src[body..body + k].matches('\n').count();
                    i = body + k + close.len();
                }
                b'"' => {
                    let open = i;
                    i += 1;
                    while i < b.len() && b[i] != b'"' {
                        i += if b[i] == b'\\' { 2 } else { 1 };
                    }
                    out.push((open, i + 1, &src[open + 1..i], line));
                    line += src[open..i].matches('\n').count();
                    i += 1;
                }
                _ => i += 1,
            }
        }
        out
    }

    fn sources() -> Vec<(String, String)> {
        fn walk(dir: &std::path::Path, out: &mut Vec<(String, String)>) {
            let mut es: Vec<_> = std::fs::read_dir(dir).unwrap().flatten().collect();
            es.sort_by_key(|e| e.path());
            for e in es {
                let p = e.path();
                if p.is_dir() {
                    walk(&p, out);
                } else if p.extension().is_some_and(|x| x == "rs") {
                    let name = p.strip_prefix(concat!(env!("CARGO_MANIFEST_DIR"), "/src")).unwrap().to_string_lossy().replace('\\', "/");
                    out.push((name, std::fs::read_to_string(&p).unwrap().replace("\r\n", "\n")));
                }
            }
        }
        let mut v = Vec::new();
        walk(std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/src")), &mut v);
        v
    }

    type Pairs<'a> = Vec<(usize, &'a str, &'a str)>;
    type Orphans<'a> = Vec<(usize, &'a str)>;

    /// (Russian, English) literal pairs: a Cyrillic literal, a comma, a literal; plus the Cyrillic literals with no partner.
    fn pairs_and_orphans(src: &str) -> (Pairs<'_>, Orphans<'_>) {
        let l = literals(src);
        let (mut pairs, mut orphans) = (Vec::new(), Vec::new());
        for (k, &(_, after, ru, line)) in l.iter().enumerate() {
            if !ru.chars().any(is_cyr) {
                continue;
            }
            match l.get(k + 1) {
                // the language names are the one pair that is Cyrillic on both sides
                Some(&(open, _, en, _)) if src[after..open].trim() == "," && (!en.chars().any(is_cyr) || en == ru) => pairs.push((line, ru, en)),
                _ => orphans.push((line, ru)),
            }
        }
        (pairs, orphans)
    }

    #[test]
    fn english_halves_are_filled_and_not_russian() {
        let mut n = 0;
        for (file, src) in sources() {
            for (line, ru, en) in pairs_and_orphans(&src).0 {
                n += 1;
                // the language name is the only string that reads the same in both UIs
                if ru == en {
                    assert_eq!(ru, "Русский", "{file}:{line}: the English half is the Russian text");
                    continue;
                }
                assert!(!en.trim().is_empty(), "{file}:{line}: empty English for {ru:?}");
                assert!(en.chars().any(|c| c.is_alphanumeric()), "{file}:{line}: English {en:?} has no letters (for {ru:?})");
            }
        }
        assert!(n > 500, "the scanner found only {n} pairs: it is broken");
    }

    /// Cyrillic literals with no English partner next to them, per file. Each is Russian DATA (weekday tables, parser words, the Russian
    /// edition of the quotes, the arms of an `if ru { .. } else { .. }` whose other arm is English), not a lone UI string; a new UI string
    /// must be written as a pair. Lower a number when strings go away; raising one needs a reason in the commit.
    const RUSSIAN_ONLY: &[(&str, usize)] = &[
        ("host/settings_edit.rs", 1), // the Russian arm of a `pick(&format!(..), &format!(..))`
        ("input.rs", 2),              // match arms (Lang::Ru, ..) next to the (Lang::En, ..) ones
        ("menu.rs", 2),               // `pick(if editing { ru } else { ru }, if editing { en } else { en })`
        ("tray.rs", 1),               // the language name "Русский"
        ("ui/anim.rs", 3),            // update status lines: `if lang == Ru { format!(ru) } else { format!(en) }`
        ("ui/draw.rs", 1),            // the language name "Русский"
        ("widget.rs", 31),            // weekday / month name tables (the English tables sit next to them)
        ("widgets/batteries.rs", 2),  // device-name keywords to find the icon
        ("widgets/calendar.rs", 7),   // weekday initials (Monday first in Russian)
        ("widgets/clipboard.rs", 4),  // age units
        ("widgets/controls.rs", 1),   // `if ru { .. } else { "Click again" }`
        ("widgets/countdown.rs", 6),  // units and plural forms
        ("widgets/dev.rs", 4),        // the developer gallery (hidden)
        ("widgets/disks.rs", 2),      // default drive names
        ("widgets/nextevent.rs", 30), // short weekday / month tables, "через .." forms
        ("widgets/quote.rs", 278),    // the Russian edition of the quotes (QUOTES_EN is the English one)
        ("widgets/reminders.rs", 16), // the due-date parser words and month tables
        ("widgets/sysmon.rs", 11),    // unit tables
        ("widgets/timer.rs", 10),     // unit words the parser accepts, mode names
        ("widgets/weather.rs", 8),    // weekday table, "Макс.: .., мин.: .."
    ];

    #[test]
    fn no_new_russian_only_strings() {
        let allowed: std::collections::HashMap<&str, usize> = RUSSIAN_ONLY.iter().copied().collect();
        let mut report = String::new();
        for (file, src) in sources() {
            let orphans = pairs_and_orphans(&src).1;
            let max = allowed.get(file.as_str()).copied().unwrap_or(0);
            if orphans.len() > max {
                report += &format!("{file}: {} Russian-only literals, {max} allowed\n", orphans.len());
                for (line, s) in orphans {
                    report += &format!("    {file}:{line}: {s}\n");
                }
            }
        }
        assert!(report.is_empty(), "Russian-only strings (give them an English half, `(ru, en)`):\n{report}");
    }
}
