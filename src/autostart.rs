//! "Start with Windows": the per-user Run value `GlassWidgets` -> this exe (HKCU, no administrator rights). Only ever written when the
//! user flips the switch (tray menu / settings panel); the installer's optional "Start with Windows" component writes the same value.
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::System::Registry::*;

const RUN_KEY: PCWSTR = w!(r"Software\Microsoft\Windows\CurrentVersion\Run");
const VALUE: PCWSTR = w!("GlassWidgets");

/// Is the autostart value present?
pub fn enabled() -> bool {
    value_in(RUN_KEY).is_some_and(|v| !v.is_empty())
}

/// Turn the autostart value on (pointing at the running exe) or off.
pub fn set(on: bool) -> Result<(), String> {
    set_in(RUN_KEY, on)
}

fn command() -> Result<String, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    Ok(format!("\"{}\"", exe.display()))
}

fn value_in(key: PCWSTR) -> Option<String> {
    unsafe {
        let mut hk = HKEY::default();
        if RegOpenKeyExW(HKEY_CURRENT_USER, key, 0, KEY_QUERY_VALUE, &mut hk) != ERROR_SUCCESS {
            return None;
        }
        let mut len = 0u32;
        let mut ty = REG_VALUE_TYPE::default();
        let probe = RegQueryValueExW(hk, VALUE, None, Some(&mut ty), None, Some(&mut len));
        let out = if probe == ERROR_SUCCESS && len >= 2 {
            let mut buf = vec![0u16; (len as usize).div_ceil(2)];
            let r = RegQueryValueExW(hk, VALUE, None, Some(&mut ty), Some(buf.as_mut_ptr() as *mut u8), Some(&mut len));
            (r == ERROR_SUCCESS).then(|| String::from_utf16_lossy(&buf).trim_end_matches('\0').to_string())
        } else {
            None
        };
        let _ = RegCloseKey(hk);
        out
    }
}

fn set_in(key: PCWSTR, on: bool) -> Result<(), String> {
    unsafe {
        let mut hk = HKEY::default();
        let r = RegCreateKeyExW(HKEY_CURRENT_USER, key, 0, PCWSTR::null(), REG_OPTION_NON_VOLATILE, KEY_SET_VALUE, None, &mut hk, None);
        if r != ERROR_SUCCESS {
            return Err(format!("registry: error {}", r.0));
        }
        let res = if on {
            let cmd = command()?;
            let wide: Vec<u16> = cmd.encode_utf16().chain(Some(0)).collect();
            let bytes = std::slice::from_raw_parts(wide.as_ptr() as *const u8, wide.len() * 2);
            RegSetValueExW(hk, VALUE, 0, REG_SZ, Some(bytes))
        } else {
            let r = RegDeleteValueW(hk, VALUE);
            if r.0 == 2 { ERROR_SUCCESS } else { r } // 2 = the value was not there: already off
        };
        let _ = RegCloseKey(hk);
        if res == ERROR_SUCCESS { Ok(()) } else { Err(format!("registry: error {}", res.0)) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Round trip on a private test key (never the real Run key, so running the tests does not touch the user's autostart).
    #[test]
    fn value_round_trip() {
        let key = w!(r"Software\GlassWidgetsTest\Run");
        set_in(key, false).unwrap(); // absent -> still Ok
        assert_eq!(value_in(key), None);
        set_in(key, true).unwrap();
        let v = value_in(key).unwrap();
        assert!(v.starts_with('"') && v.ends_with("\""), "{v}");
        set_in(key, false).unwrap();
        assert_eq!(value_in(key), None);
        unsafe {
            let _ = RegDeleteKeyW(HKEY_CURRENT_USER, w!(r"Software\GlassWidgetsTest\Run"));
            let _ = RegDeleteKeyW(HKEY_CURRENT_USER, w!(r"Software\GlassWidgetsTest"));
        }
    }
}
