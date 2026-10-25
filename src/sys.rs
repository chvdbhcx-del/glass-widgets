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
