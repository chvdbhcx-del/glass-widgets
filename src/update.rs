//! Update check (opt-in: the "Check for updates" button of the settings panel and the `checkUpdates` switch). One HTTPS GET of the
//! latest GitHub release on a worker thread; only the version and the release page are read. Nothing is ever downloaded or run:
//! "Download" just opens the release page in the default browser.
use serde_json::Value;
use std::sync::Mutex;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_APP};

/// posted to the host window when the worker is done
pub const WM_UPDATE: u32 = WM_APP + 5;
const API: &str = "https://api.github.com/repos/chvdbhcx-del/glass-widgets/releases/latest";
/// the release page must live here: the URL comes from the network and is handed to ShellExecute
const PAGE_PREFIX: &str = "https://github.com/";

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Release {
    pub tag: String,
    pub url: String,
}

/// What the settings panel shows.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub enum UpdateState {
    #[default]
    Idle,
    Checking,
    Latest,
    Available { version: String, url: String },
    Failed,
}

/// `tag_name` and `html_url` of a "latest release" reply; None when either is missing or the page is not on github.com.
pub fn parse_release(json: &str) -> Option<Release> {
    let v: Value = serde_json::from_str(json).ok()?;
    let tag = v.get("tag_name")?.as_str()?.trim().to_string();
    let url = v.get("html_url")?.as_str()?.trim().to_string();
    (!tag.is_empty() && url.starts_with(PAGE_PREFIX)).then_some(Release { tag, url })
}

/// "v1.2.3", "1.2", "1.2.3-beta.1+build": (major, minor, patch, is_prerelease). Anything else is None.
fn parse_version(s: &str) -> Option<(u64, u64, u64, bool)> {
    let s = s.trim().trim_start_matches(['v', 'V']);
    let s = s.split('+').next()?;
    let (core, pre) = match s.split_once('-') {
        Some((c, _)) => (c, true),
        None => (s, false),
    };
    let mut it = core.split('.');
    let mut num = || it.next().map_or(Some(0), |p| p.parse::<u64>().ok());
    let (a, b, c) = (num()?, num()?, num()?);
    it.next().is_none().then_some((a, b, c, pre))
}

/// Is `latest` newer than `current`? A pre-release is older than its release; unparsable tags are never "newer".
pub fn is_newer(latest: &str, current: &str) -> bool {
    match (parse_version(latest), parse_version(current)) {
        (Some((a, b, c, pre)), Some((x, y, z, cpre))) => (a, b, c, !pre) > (x, y, z, !cpre),
        _ => false,
    }
}

/// The panel state for a finished check.
pub fn state_from(r: Result<Release, String>, current: &str) -> UpdateState {
    match r {
        Ok(rel) if is_newer(&rel.tag, current) => UpdateState::Available { version: rel.tag.trim_start_matches(['v', 'V']).to_string(), url: rel.url },
        Ok(_) => UpdateState::Latest,
        Err(_) => UpdateState::Failed,
    }
}

static RESULT: Mutex<Option<Result<Release, String>>> = Mutex::new(None);

/// One GET on a worker thread; the result is picked up with `take` after `WM_UPDATE` arrives at `hwnd`.
pub fn start(hwnd: isize) {
    std::thread::spawn(move || {
        let r = crate::net::http_get(API, 10_000).and_then(|b| parse_release(&String::from_utf8_lossy(&b)).ok_or_else(|| "unexpected reply".to_string()));
        if let Ok(mut g) = RESULT.lock() {
            *g = Some(r);
        }
        unsafe {
            let _ = PostMessageW(HWND(hwnd as *mut _), WM_UPDATE, WPARAM(0), LPARAM(0));
        }
    });
}

pub fn take() -> Option<Result<Release, String>> {
    RESULT.lock().ok().and_then(|mut g| g.take())
}

#[cfg(test)]
mod tests {
    use super::*;

    const REPLY: &str = r#"{"url":"https://api.github.com/x","html_url":"https://github.com/chvdbhcx-del/glass-widgets/releases/tag/v0.3.0","tag_name":"v0.3.0","name":"0.3.0","draft":false}"#;

    #[test]
    fn reply_is_parsed_and_checked() {
        let r = parse_release(REPLY).unwrap();
        assert_eq!((r.tag.as_str(), r.url.as_str()), ("v0.3.0", "https://github.com/chvdbhcx-del/glass-widgets/releases/tag/v0.3.0"));
        assert!(parse_release("{}").is_none() && parse_release("nope").is_none());
        assert!(parse_release(r#"{"tag_name":"v1","html_url":"file:///c:/windows/system32/calc.exe"}"#).is_none());
        assert!(parse_release(r#"{"tag_name":"v1","html_url":"https://evil.example/x"}"#).is_none());
        assert!(parse_release(r#"{"tag_name":"","html_url":"https://github.com/a/b"}"#).is_none());
    }

    #[test]
    fn versions_compare_like_semver() {
        assert!(is_newer("v0.3.0", "0.2.1") && is_newer("0.2.2", "0.2.1") && is_newer("1.0", "0.9.9"));
        assert!(is_newer("0.10.0", "0.9.0")); // numeric, not text
        assert!(!is_newer("0.2.1", "0.2.1") && !is_newer("v0.2.0", "0.2.1"));
        assert!(!is_newer("0.3.0-beta.1", "0.3.0") && is_newer("0.3.0", "0.3.0-beta.1")); // a pre-release is older than its release
        assert!(is_newer("0.3.0-rc1", "0.2.1"));
        assert!(!is_newer("0.3.0+build5", "0.3.0"));
        assert!(!is_newer("nightly", "0.2.1") && !is_newer("0.3.x", "0.2.1") && !is_newer("1.2.3.4", "0.2.1"));
    }

    #[test]
    fn state_follows_the_check() {
        let rel = parse_release(REPLY).unwrap();
        assert_eq!(state_from(Ok(rel.clone()), "0.2.1"), UpdateState::Available { version: "0.3.0".into(), url: rel.url.clone() });
        assert_eq!(state_from(Ok(rel), "0.3.0"), UpdateState::Latest);
        assert_eq!(state_from(Err("http 403".into()), "0.2.1"), UpdateState::Failed);
    }
}
