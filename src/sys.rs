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
