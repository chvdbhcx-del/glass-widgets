//! OS helpers for widgets and their worker threads (everything here is safe to call from ANY thread):
//! hidden child processes (never a console window), HTTP through curl.exe, PowerShell, WIC image decoding,
//! the local UTC offset and the `Action` executor behind `Cx::invoke_system`.
use std::os::windows::process::CommandExt;
use std::process::{Command, Stdio};
use windows::core::{w, HSTRING};
use windows::Win32::Foundation::GENERIC_READ;
use windows::Win32::Graphics::Imaging::*;
use windows::Win32::System::Com::*;
use windows::Win32::System::Time::GetTimeZoneInformation;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Run `exe args...` with no window, wait, return stdout (lossy UTF-8). Err = spawn failure or non-zero exit (message has stderr).
pub fn run_hidden(exe: &str, args: &[&str]) -> Result<String, String> {
    let out = Command::new(exe).args(args).stdin(Stdio::null()).creation_flags(CREATE_NO_WINDOW).output().map_err(|e| format!("{exe}: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(format!("{exe}: exit {:?}: {}", out.status.code(), String::from_utf8_lossy(&out.stderr).trim()))
    }
}

/// Start a program with no window and do not wait (fire and forget).
pub fn spawn_hidden(exe: &str, args: &[&str]) {
    let _ = Command::new(exe).args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).creation_flags(CREATE_NO_WINDOW).spawn();
}

/// Run a PowerShell script hidden; stdout is UTF-8 (the console encoding is forced). Typical use: WMI / CIM queries from a worker job,
/// ending the script with `| ConvertTo-Json -Compress`.
pub fn powershell(script: &str) -> Result<String, String> {
    let s = format!("[Console]::OutputEncoding=[Text.Encoding]::UTF8; $ProgressPreference='SilentlyContinue'; {script}");
    run_hidden("powershell.exe", &["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", &s])
}

/// GET a URL through the curl.exe that ships with Windows 10+ (follows redirects). Call from a worker job only, never from draw/tick.
pub fn http_get(url: &str, timeout_secs: u32) -> Result<String, String> {
    let t = timeout_secs.max(1).to_string();
    run_hidden("curl.exe", &["-sSL", "--max-time", &t, "-A", "macshell-widgets/1.0", url])
}

/// Local offset from UTC in minutes (east positive), DST included.
pub fn local_utc_offset_min() -> i32 {
    unsafe {
        let mut tz = windows::Win32::System::Time::TIME_ZONE_INFORMATION::default();
        let id = GetTimeZoneInformation(&mut tz);
        let extra = match id {
            1 => tz.StandardBias, // TIME_ZONE_ID_STANDARD
            2 => tz.DaylightBias, // TIME_ZONE_ID_DAYLIGHT
            _ => 0,
        };
        -(tz.Bias + extra)
    }
}

// ---------------------------------------------------------------- images

/// Decoded image, premultiplied BGRA, ready for `Cx::image` (device bitmaps must be created on the UI thread).
pub struct Decoded {
    pub w: u32,
    pub h: u32,
    pub bgra: Vec<u8>,
}

/// Decode any WIC format (png, jpg, bmp, gif, webp when the codec is installed, ...) and shrink (never enlarge) to fit max_w x max_h.
/// Safe on any thread (creates its own WIC factory).
pub fn decode_image_file(path: &str, max_w: u32, max_h: u32) -> Option<Decoded> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let wic: IWICImagingFactory = CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER).ok()?;
        let dec = wic.CreateDecoderFromFilename(&HSTRING::from(path), None, GENERIC_READ, WICDecodeMetadataCacheOnDemand).ok()?;
        finish(&wic, &dec, max_w, max_h)
    }
}

/// Same as `decode_image_file` for an in-memory encoded image (album art from a media session, downloaded thumbnails).
pub fn decode_image_bytes(bytes: &[u8], max_w: u32, max_h: u32) -> Option<Decoded> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let wic: IWICImagingFactory = CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER).ok()?;
        let st = wic.CreateStream().ok()?;
        st.InitializeFromMemory(bytes).ok()?;
        let dec = wic.CreateDecoderFromStream(&st, std::ptr::null(), WICDecodeMetadataCacheOnDemand).ok()?;
        finish(&wic, &dec, max_w, max_h)
    }
}

unsafe fn finish(wic: &IWICImagingFactory, dec: &IWICBitmapDecoder, max_w: u32, max_h: u32) -> Option<Decoded> {
    let fr = dec.GetFrame(0).ok()?;
    let (mut w, mut h) = (0u32, 0u32);
    fr.GetSize(&mut w, &mut h).ok()?;
    if w == 0 || h == 0 {
        return None;
    }
    let k = (max_w as f32 / w as f32).min(max_h as f32 / h as f32).min(1.0);
    let (tw, th) = (((w as f32 * k).round() as u32).max(1), ((h as f32 * k).round() as u32).max(1));
    let cv = wic.CreateFormatConverter().ok()?;
    if tw != w || th != h {
        let sc = wic.CreateBitmapScaler().ok()?;
        sc.Initialize(&fr, tw, th, WICBitmapInterpolationModeFant).ok()?;
        cv.Initialize(&sc, &GUID_WICPixelFormat32bppPBGRA, WICBitmapDitherTypeNone, None, 0.0, WICBitmapPaletteTypeCustom).ok()?;
    } else {
        cv.Initialize(&fr, &GUID_WICPixelFormat32bppPBGRA, WICBitmapDitherTypeNone, None, 0.0, WICBitmapPaletteTypeCustom).ok()?;
    }
    let mut buf = vec![0u8; (tw * th * 4) as usize];
    cv.CopyPixels(std::ptr::null(), tw * 4, &mut buf).ok()?;
    Some(Decoded { w: tw, h: th, bgra: buf })
}

// ---------------------------------------------------------------- actions

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PowerOp {
    Lock,
    Sleep,
    Restart,
    Shutdown,
    SignOut,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MediaKey {
    PlayPause,
    Next,
    Prev,
    Stop,
    VolUp,
    VolDown,
    Mute,
}

/// Things a widget may ask the system to do (`Cx::invoke_system`). All are no-ops in preview mode.
#[derive(Clone, PartialEq, Debug)]
pub enum Action {
    /// URL, file, folder or program: ShellExecute "open".
    Open(String),
    /// Program + arguments, no window, not waited for.
    Run(String, Vec<String>),
    Power(PowerOp),
    Media(MediaKey),
}

pub fn perform(a: &Action) {
    unsafe {
        match a {
            Action::Open(t) => {
                ShellExecuteW(None, w!("open"), &HSTRING::from(t.as_str()), None, None, SW_SHOWNORMAL);
            }
            Action::Run(exe, args) => {
                let a: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
                spawn_hidden(exe, &a);
            }
            Action::Power(p) => match p {
                PowerOp::Lock => spawn_hidden("rundll32.exe", &["user32.dll,LockWorkStation"]),
                PowerOp::Sleep => spawn_hidden("rundll32.exe", &["powrprof.dll,SetSuspendState", "0,1,0"]),
                PowerOp::Restart => spawn_hidden("shutdown.exe", &["/r", "/t", "0"]),
                PowerOp::Shutdown => spawn_hidden("shutdown.exe", &["/s", "/t", "0"]),
                PowerOp::SignOut => spawn_hidden("shutdown.exe", &["/l"]),
            },
            Action::Media(k) => {
                let vk = match k {
                    MediaKey::PlayPause => 0xB3,
                    MediaKey::Next => 0xB0,
                    MediaKey::Prev => 0xB1,
                    MediaKey::Stop => 0xB2,
                    MediaKey::VolUp => 0xAF,
                    MediaKey::VolDown => 0xAE,
                    MediaKey::Mute => 0xAD,
                };
                keybd_event(vk, 0, KEYBD_EVENT_FLAGS(0), 0);
                keybd_event(vk, 0, KEYEVENTF_KEYUP, 0);
            }
        }
    }
}

/// Is Ctrl held right now (as of the message being handled)? For hidden-but-safe alternatives to a card's plain click.
pub fn ctrl_down() -> bool {
    unsafe { GetKeyState(VK_CONTROL.0 as i32) < 0 }
}

// ---------------------------------------------------------------- click targets

/// What a left click on a card opens (`Cx::open_card_target`). Every target lives here so the mapping can be tested; all go through
/// ShellExecute "open" (`Action::Open`): a URI scheme, a `shell:` folder, a program or a URL.
pub mod targets {
    use windows::core::w;
    use windows::Win32::Foundation::ERROR_SUCCESS;
    use windows::Win32::System::Registry::*;

    pub const TASK_MANAGER: &str = "taskmgr.exe";
    pub const THIS_PC: &str = "shell:MyComputerFolder";
    pub const CLOCK: &str = "ms-clock:";
    pub const BLUETOOTH: &str = "ms-settings:bluetooth";
    pub const SOUND: &str = "ms-settings:sound";
    pub const DISPLAY: &str = "ms-settings:display";
    /// Outlook / Calendar app, only when the `outlookcal` protocol is registered
    pub const CALENDAR_APP: &str = "outlookcal:";
    pub const CALENDAR_WEB: &str = "https://calendar.google.com";

    /// Is the URI scheme `name` registered (HKCR\<name> exists)? Cached by the caller, this reads the registry.
    fn protocol_registered(name: windows::core::PCWSTR) -> bool {
        unsafe {
            let mut hk = HKEY::default();
            let ok = RegOpenKeyExW(HKEY_CLASSES_ROOT, name, 0, KEY_QUERY_VALUE, &mut hk) == ERROR_SUCCESS;
            if ok {
                let _ = RegCloseKey(hk);
            }
            ok
        }
    }

    /// The calendar to open: the Windows Calendar / Outlook app when `outlookcal:` is registered, else Google Calendar.
    pub fn calendar() -> &'static str {
        static C: std::sync::OnceLock<&'static str> = std::sync::OnceLock::new();
        C.get_or_init(|| if protocol_registered(w!("outlookcal")) { CALENDAR_APP } else { CALENDAR_WEB })
    }

    /// Widget type id -> what its card body opens (`None` = the widget has its own clicks).
    pub fn of(widget: &str) -> Option<&'static str> {
        Some(match widget {
            "sysmon" | "network" | "processes" => TASK_MANAGER,
            "disks" => THIS_PC,
            "clock" | "worldclock" => CLOCK,
            "batteries" => BLUETOOTH,
            "calendar" | "nextevent" => calendar(),
            _ => return None,
        })
    }

    // ------------------------------------------------ "Click action": what the user chose for a card (cfg.click)

    /// The user's choice for a card click, stored in the card's own cfg as `click: {kind: "preset"|"custom"|"none", value}`.
    /// No key (or an unknown / empty one) = `Default`: the table `of`.
    #[derive(Clone, Debug, PartialEq)]
    pub enum Click {
        Default,
        Nothing,
        /// `Preset::key` of one of `presets(widget)`
        Preset(String),
        /// a value that passed `parse_custom`
        Custom(String),
    }

    impl Click {
        pub fn from_cfg(cfg: &serde_json::Value) -> Click {
            let Some(c) = cfg.get("click") else { return Click::Default };
            let v = c.get("value").and_then(|v| v.as_str()).unwrap_or("");
            match c.get("kind").and_then(|k| k.as_str()) {
                Some("none") => Click::Nothing,
                Some("preset") if !v.is_empty() => Click::Preset(v.to_string()),
                Some("custom") if !v.is_empty() => Click::Custom(v.to_string()),
                _ => Click::Default,
            }
        }

        /// Write the choice into a card's cfg (`Default` removes the key, so an untouched card keeps a clean config).
        pub fn store(&self, cfg: &mut serde_json::Value) {
            let (kind, value) = match self {
                Click::Default => {
                    if let Some(o) = cfg.as_object_mut() {
                        o.remove("click");
                    }
                    return;
                }
                Click::Nothing => ("none", ""),
                Click::Preset(k) => ("preset", k.as_str()),
                Click::Custom(v) => ("custom", v.as_str()),
            };
            if !cfg.is_object() {
                *cfg = serde_json::json!({});
            }
            cfg["click"] = serde_json::json!({ "kind": kind, "value": value });
        }
    }

    /// One ready-made choice of the "Click action" menu. `target` is what ShellExecute opens; for the weather sites it is the bare site,
    /// used when no city is known (`weather_site` builds the city search).
    pub struct Preset {
        pub key: &'static str,
        pub ru: &'static str,
        pub en: &'static str,
        pub target: &'static str,
    }
    const fn pr(key: &'static str, ru: &'static str, en: &'static str, target: &'static str) -> Preset {
        Preset { key, ru, en, target }
    }

    static P_CALENDAR: &[Preset] = &[
        pr("windows", "Календарь Windows", "Windows Calendar", CALENDAR_APP),
        pr("google", "Google Calendar", "Google Calendar", CALENDAR_WEB),
        pr("outlook", "Outlook (веб)", "Outlook (web)", "https://outlook.live.com/calendar/"),
    ];
    static P_WEATHER: &[Preset] = &[
        pr("google", "Google", "Google", "https://www.google.com/search?q=weather"),
        pr("yandex", "Яндекс", "Yandex", "https://yandex.ru/pogoda/"),
        pr("gismeteo", "Gismeteo", "Gismeteo", "https://www.gismeteo.ru/"),
        pr("open-meteo", "Open-Meteo", "Open-Meteo", "https://open-meteo.com/"),
    ];
    static P_SYSTEM: &[Preset] = &[
        pr("taskmgr", "Диспетчер задач", "Task Manager", TASK_MANAGER),
        pr("resmon", "Монитор ресурсов", "Resource Monitor", "resmon.exe"),
        pr("about", "Параметры > Система", "Settings > System", "ms-settings:about"),
    ];
    static P_NETWORK: &[Preset] = &[
        pr("taskmgr", "Диспетчер задач", "Task Manager", TASK_MANAGER),
        pr("resmon", "Монитор ресурсов", "Resource Monitor", "resmon.exe"),
        pr("network", "Параметры > Сеть", "Settings > Network", "ms-settings:network-status"),
        pr("about", "Параметры > Система", "Settings > System", "ms-settings:about"),
    ];
    static P_DISKS: &[Preset] = &[pr("thispc", "Этот компьютер", "This PC", THIS_PC), pr("storage", "Параметры > Память", "Storage settings", "ms-settings:storagesense")];
    static P_CLOCK: &[Preset] = &[pr("clock", "Будильники и часы", "Alarms & Clock", CLOCK), pr("datetime", "Параметры > Дата и время", "Date & time settings", "ms-settings:dateandtime")];
    static P_BATTERIES: &[Preset] = &[pr("bluetooth", "Параметры Bluetooth", "Bluetooth settings", BLUETOOTH), pr("battery", "Параметры батареи", "Battery settings", "ms-settings:batterysaver")];

    /// The presets of a widget's "Click action" menu (empty = the widget has no card-body click, so no menu item).
    pub fn presets(widget: &str) -> &'static [Preset] {
        match widget {
            "calendar" | "nextevent" => P_CALENDAR,
            "weather" => P_WEATHER,
            "sysmon" | "processes" => P_SYSTEM,
            "network" => P_NETWORK,
            "disks" => P_DISKS,
            "clock" | "worldclock" => P_CLOCK,
            "batteries" => P_BATTERIES,
            _ => &[],
        }
    }

    /// The city search of a weather site preset (`word` = the localized "weather"; only the escaped city ever enters the URL).
    pub fn weather_site(key: &str, word: &str, city: &str) -> Option<String> {
        let base = P_WEATHER.iter().find(|p| p.key == key)?.target;
        let c = query_escape(city);
        if c.is_empty() {
            return Some(base.to_string());
        }
        Some(match key {
            "google" => weather_url(word, city),
            "yandex" => format!("https://yandex.ru/search/?text={}+{c}", query_escape(word)),
            "gismeteo" => format!("https://www.gismeteo.ru/search/{}/", c.replace('+', "%20")),
            _ => base.to_string(), // open-meteo.com has no city search
        })
    }

    /// Validate a user-typed custom click target. Accepted (and returned trimmed, without surrounding quotes): an `http(s)://` URL, a
    /// `ms-settings:` / `ms-clock:` / `shell:` URI, or an absolute local drive path for which `exists` says yes. Everything else is
    /// rejected: program names, command lines with arguments, UNC paths, other schemes, control characters. `exists` is a parameter so
    /// the check is testable and a click-time re-check can skip the filesystem.
    pub fn parse_custom(s: &str, exists: impl Fn(&str) -> bool) -> Option<String> {
        let s = s.trim();
        let s = s.strip_prefix('"').and_then(|x| x.strip_suffix('"')).unwrap_or(s).trim();
        if s.is_empty() || s.len() > 2048 || s.chars().any(|c| c.is_control()) {
            return None;
        }
        let lower = s.to_ascii_lowercase();
        let rest_of = |p: &str| lower.strip_prefix(p).map(|_| &s[p.len()..]);
        // web: scheme, a host, no blanks or quotes anywhere, no user:password@ in front of the host
        if let Some(r) = rest_of("https://").or_else(|| rest_of("http://")) {
            let host = r.split(['/', '?', '#']).next().unwrap_or("");
            let ok = !host.is_empty() && !host.contains('@') && !s.contains(|c: char| c.is_whitespace() || c == '"' || c == '<' || c == '>');
            return ok.then(|| s.to_string());
        }
        // system URIs: ms-settings:page, ms-clock:, shell:Downloads, shell:::{CLSID}; no "..", only name characters
        for (p, spaces) in [("ms-settings:", false), ("ms-clock:", false), ("shell:", true)] {
            if let Some(r) = rest_of(p) {
                let ok = r.chars().all(|c| c.is_ascii_alphanumeric() || "-_.:{}\\!".contains(c) || (spaces && c == ' ') || (p == "ms-settings:" && "?=&".contains(c))) && !r.contains("..");
                return ok.then(|| s.to_string());
            }
        }
        // local drive path `C:\dir\file` (no UNC / device paths, no wildcards): the whole string must be a file or folder that exists
        let b = s.as_bytes();
        let drive = b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/');
        (drive && !s[2..].contains(['<', '>', '|', '?', '*', ':']) && exists(s)).then(|| s.to_string())
    }

    /// What a click on the card of `widget` opens given its `cfg` (None = nothing). `weather` = (localized word, city) for the weather card.
    /// A preset the table no longer knows or a custom value that fails the syntax check (a hand-edited config) falls back to the default.
    pub fn resolve(widget: &str, cfg: &serde_json::Value, weather: (&str, &str)) -> Option<String> {
        let default = || if widget == "weather" { Some(weather_url(weather.0, weather.1)) } else { of(widget).map(str::to_string) };
        match Click::from_cfg(cfg) {
            Click::Default => default(),
            Click::Nothing => None,
            Click::Custom(v) => parse_custom(&v, |_| true).or_else(default),
            Click::Preset(k) => match presets(widget).iter().find(|p| p.key == k) {
                Some(_) if widget == "weather" => weather_site(&k, weather.0, weather.1),
                Some(p) if p.target == CALENDAR_APP && calendar() != CALENDAR_APP => Some(CALENDAR_WEB.to_string()), // no Calendar app installed
                Some(p) => Some(p.target.to_string()),
                None => default(),
            },
        }
    }

    /// Percent-encode `s` for a URL query value: unreserved ASCII stays, a space becomes '+', everything else is %XX of its UTF-8 bytes.
    pub fn query_escape(s: &str) -> String {
        let mut o = String::with_capacity(s.len() * 3);
        for b in s.trim().bytes() {
            match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => o.push(b as char),
                b' ' => o.push('+'),
                _ => o.push_str(&format!("%{b:02X}")),
            }
        }
        o
    }

    /// A web search for the weather: `https://www.google.com/search?q=<word>+<city>`; `word` is the localized "weather", nothing else
    /// but the escaped city ever ends up in the URL. An empty city searches for the bare word.
    pub fn weather_url(word: &str, city: &str) -> String {
        let city = query_escape(city);
        let word = query_escape(word);
        if city.is_empty() {
            format!("https://www.google.com/search?q={word}")
        } else {
            format!("https://www.google.com/search?q={word}+{city}")
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn escapes_city() {
            assert_eq!(query_escape("London"), "London");
            assert_eq!(query_escape(" New York "), "New+York");
            assert_eq!(query_escape("Москва"), "%D0%9C%D0%BE%D1%81%D0%BA%D0%B2%D0%B0");
            assert_eq!(query_escape("a&b=c#d%e/?"), "a%26b%3Dc%23d%25e%2F%3F");
            assert_eq!(query_escape("St. John's"), "St.+John%27s");
        }

        #[test]
        fn weather_urls() {
            assert_eq!(weather_url("погода", "Санкт-Петербург"), "https://www.google.com/search?q=%D0%BF%D0%BE%D0%B3%D0%BE%D0%B4%D0%B0+%D0%A1%D0%B0%D0%BD%D0%BA%D1%82-%D0%9F%D0%B5%D1%82%D0%B5%D1%80%D0%B1%D1%83%D1%80%D0%B3");
            assert_eq!(weather_url("weather", "New York"), "https://www.google.com/search?q=weather+New+York");
            assert_eq!(weather_url("weather", ""), "https://www.google.com/search?q=weather");
            assert_eq!(weather_url("weather", "  "), "https://www.google.com/search?q=weather");
            // a hostile city name cannot add a parameter or leave the query
            let u = weather_url("weather", "x&y=1 http://evil");
            assert!(u.starts_with("https://www.google.com/search?q=weather+x%26y%3D1+http%3A%2F%2Fevil"));
            assert_eq!(u.matches('&').count(), 0);
        }

        #[test]
        fn mapping() {
            for (id, want) in [
                ("sysmon", TASK_MANAGER),
                ("network", TASK_MANAGER),
                ("processes", TASK_MANAGER),
                ("disks", THIS_PC),
                ("clock", CLOCK),
                ("worldclock", CLOCK),
                ("batteries", BLUETOOTH),
            ] {
                assert_eq!(of(id), Some(want), "{id}");
            }
            for id in ["calendar", "nextevent"] {
                assert!(matches!(of(id), Some(CALENDAR_APP | CALENDAR_WEB)), "{id}");
            }
            // widgets with their own clicks stay out of the table
            for id in ["timer", "weather", "controls", "rates", "crypto", "quote", "photos", "countdown", "reminders", "notes", "clipboard", "music", "shortcuts"] {
                assert_eq!(of(id), None, "{id}");
            }
        }

        fn cfg(kind: &str, value: &str) -> serde_json::Value {
            serde_json::json!({ "click": { "kind": kind, "value": value } })
        }
        const NOWHERE: (&str, &str) = ("weather", "Paris");

        #[test]
        fn click_defaults_and_roundtrip() {
            assert_eq!(Click::from_cfg(&serde_json::json!({})), Click::Default);
            assert_eq!(Click::from_cfg(&serde_json::json!(null)), Click::Default);
            assert_eq!(Click::from_cfg(&cfg("preset", "")), Click::Default); // empty value
            assert_eq!(Click::from_cfg(&cfg("bogus", "x")), Click::Default);
            assert_eq!(Click::from_cfg(&cfg("none", "")), Click::Nothing);
            for c in [Click::Nothing, Click::Preset("resmon".into()), Click::Custom("https://example.com".into())] {
                let mut v = serde_json::json!({ "city": "Oslo" });
                c.store(&mut v);
                assert_eq!(Click::from_cfg(&v), c);
                assert_eq!(v["city"], "Oslo", "other keys survive");
            }
            let mut v = cfg("none", "");
            Click::Default.store(&mut v);
            assert!(v.get("click").is_none());
            let mut n = serde_json::json!(null);
            Click::Preset("a".into()).store(&mut n);
            assert_eq!(n["click"]["kind"], "preset");
        }

        #[test]
        fn resolve_default_none_preset() {
            let empty = serde_json::json!({});
            for id in ["sysmon", "network", "processes", "disks", "clock", "worldclock", "batteries"] {
                assert_eq!(resolve(id, &empty, NOWHERE).as_deref(), of(id), "{id}: default is the table");
                assert_eq!(resolve(id, &cfg("none", ""), NOWHERE), None, "{id}: nothing");
            }
            assert_eq!(resolve("weather", &empty, ("weather", "Paris")).unwrap(), weather_url("weather", "Paris"));
            assert_eq!(resolve("timer", &empty, NOWHERE), None, "no target, no default");
            assert_eq!(resolve("sysmon", &cfg("preset", "resmon"), NOWHERE).as_deref(), Some("resmon.exe"));
            assert_eq!(resolve("network", &cfg("preset", "network"), NOWHERE).as_deref(), Some("ms-settings:network-status"));
            assert_eq!(resolve("clock", &cfg("preset", "datetime"), NOWHERE).as_deref(), Some("ms-settings:dateandtime"));
            assert_eq!(resolve("disks", &cfg("preset", "storage"), NOWHERE).as_deref(), Some("ms-settings:storagesense"));
            assert_eq!(resolve("batteries", &cfg("preset", "battery"), NOWHERE).as_deref(), Some("ms-settings:batterysaver"));
            assert_eq!(resolve("calendar", &cfg("preset", "google"), NOWHERE).as_deref(), Some(CALENDAR_WEB));
            assert!(resolve("nextevent", &cfg("preset", "outlook"), NOWHERE).unwrap().starts_with("https://outlook."));
            assert!(matches!(resolve("calendar", &cfg("preset", "windows"), NOWHERE).as_deref(), Some(CALENDAR_APP | CALENDAR_WEB)));
            // a preset of another widget / a removed one: back to the default
            assert_eq!(resolve("clock", &cfg("preset", "resmon"), NOWHERE).as_deref(), Some(CLOCK));
            // a widget without presets still honours "nothing" but a preset key means nothing there
            assert_eq!(resolve("timer", &cfg("preset", "x"), NOWHERE), None);
        }

        #[test]
        fn resolve_weather_sites() {
            let w = ("погода", "Санкт-Петербург");
            assert_eq!(resolve("weather", &cfg("preset", "google"), w).unwrap(), weather_url(w.0, w.1));
            assert!(resolve("weather", &cfg("preset", "yandex"), w).unwrap().starts_with("https://yandex.ru/search/?text=%D0%BF"));
            assert!(resolve("weather", &cfg("preset", "gismeteo"), w).unwrap().starts_with("https://www.gismeteo.ru/search/%D0%A1"));
            assert_eq!(resolve("weather", &cfg("preset", "open-meteo"), w).unwrap(), "https://open-meteo.com/");
            assert_eq!(resolve("weather", &cfg("preset", "gismeteo"), ("weather", " ")).unwrap(), "https://www.gismeteo.ru/");
            assert_eq!(resolve("weather", &cfg("preset", "gismeteo"), ("weather", "New York")).unwrap(), "https://www.gismeteo.ru/search/New%20York/");
            // a hostile city cannot leave the query
            let u = resolve("weather", &cfg("preset", "yandex"), ("weather", "x&y=1 #z")).unwrap();
            assert_eq!(u.matches('&').count(), 0);
            assert_eq!(resolve("weather", &cfg("none", ""), w), None);
        }

        #[test]
        fn custom_accepts_only_safe_targets() {
            let yes = |_: &str| true;
            for ok in [
                "https://example.com",
                "HTTP://example.com/a?b=c&d=e#f",
                "  https://calendar.google.com/  ",
                "ms-settings:display",
                "ms-settings:network-wifi",
                "ms-clock:",
                "shell:Downloads",
                "shell:Common Start Menu",
                "shell:::{20D04FE0-3AEA-1069-A2D8-08002B30309D}",
                "C:\\Windows\\notepad.exe",
                "\"C:\\Program Files\\App\\app.exe\"",
                "D:/Games",
            ] {
                assert!(parse_custom(ok, yes).is_some(), "{ok}");
            }
            assert_eq!(parse_custom("  \"C:\\Games\"  ", yes).as_deref(), Some("C:\\Games"));
            for bad in [
                "",
                "   ",
                "calc.exe",
                "cmd /c calc",
                "powershell -enc AAAA",
                "C:\\Windows\\notepad.exe C:\\secret.txt",
                "javascript:alert(1)",
                "file:///C:/Windows/notepad.exe",
                "ftp://example.com",
                "https://",
                "https:///path",
                "https://user:pw@evil.example",
                "https://exa mple.com",
                "https://example.com\" --flag",
                "http://a.b\nc",
                "\\\\server\\share\\x.exe",
                "\\\\?\\C:\\x",
                "relative\\path.exe",
                "C:file.txt",
                "C:\\*.exe",
                "C:\\a|b",
                "ms-settings:display && calc",
                "ms-settings:..\\x",
                "ms-windows-store:",
                "shell:..\\..\\x.exe",
                "shell:Downloads\" /x",
                "ms-clock:\u{0}",
            ] {
                assert!(parse_custom(bad, yes).is_none(), "{bad:?}");
            }
            // a drive path must exist
            assert!(parse_custom("C:\\definitely\\not\\here", |_| false).is_none());
            assert!(parse_custom(&std::env::temp_dir().to_string_lossy(), |p| std::path::Path::new(p).exists()).is_some());
            assert!(parse_custom(&std::env::current_exe().unwrap().to_string_lossy(), |p| std::path::Path::new(p).is_file()).is_some());
            // arguments after an existing program: the whole string is not a file, so it is refused
            let real = |p: &str| std::path::Path::new(p).exists();
            assert!(parse_custom("C:\\Windows\\notepad.exe /s", real).is_none());
            assert!(parse_custom("C:\\Windows\\notepad.exe", real).is_some());
            // URLs and URIs never touch the filesystem check
            assert!(parse_custom("https://example.com", |_| false).is_some());
            assert!(parse_custom(&"a".repeat(3000), yes).is_none());
        }

        #[test]
        fn hand_edited_custom_falls_back_to_default() {
            assert_eq!(resolve("sysmon", &cfg("custom", "cmd /c calc"), NOWHERE).as_deref(), Some(TASK_MANAGER));
            assert_eq!(resolve("sysmon", &cfg("custom", "https://example.com/x"), NOWHERE).as_deref(), Some("https://example.com/x"));
            assert_eq!(resolve("sysmon", &cfg("custom", "C:\\Tools\\mon.exe"), NOWHERE).as_deref(), Some("C:\\Tools\\mon.exe"));
        }

        #[test]
        fn preset_table_is_wellformed() {
            for id in ["calendar", "nextevent", "weather", "sysmon", "network", "processes", "disks", "clock", "worldclock", "batteries"] {
                let ps = presets(id);
                assert!(ps.len() >= 2, "{id}");
                for (i, p) in ps.iter().enumerate() {
                    assert!(!p.key.is_empty() && !p.ru.is_empty() && !p.en.is_empty() && !p.target.is_empty() && !p.target.contains(' '), "{id}/{}", p.key);
                    assert!(ps.iter().skip(i + 1).all(|q| q.key != p.key), "{id}: duplicate key {}", p.key);
                    assert!(p.target == CALENDAR_APP || parse_custom(p.target, |_| true).is_some() || !p.target.contains(':') && p.target.ends_with(".exe"), "{id}/{}: {}", p.key, p.target);
                }
                assert!(ps.len() < 20, "menu id range");
            }
            // every widget with a table default has presets, and only those
            for id in ["sysmon", "network", "processes", "disks", "clock", "worldclock", "batteries", "calendar", "nextevent"] {
                assert!(!presets(id).is_empty());
            }
            for id in ["timer", "controls", "rates", "notes", "music"] {
                assert!(presets(id).is_empty(), "{id}");
            }
        }

        #[test]
        fn targets_are_wellformed() {
            for t in [TASK_MANAGER, THIS_PC, CLOCK, BLUETOOTH, SOUND, DISPLAY, CALENDAR_APP, CALENDAR_WEB] {
                assert!(!t.is_empty() && !t.contains(' '), "{t}");
            }
            assert!(CALENDAR_WEB.starts_with("https://"));
            assert!(SOUND.starts_with("ms-settings:") && DISPLAY.starts_with("ms-settings:"));
        }
    }
}
