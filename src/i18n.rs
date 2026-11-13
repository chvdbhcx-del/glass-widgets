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
}
