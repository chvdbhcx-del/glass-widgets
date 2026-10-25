//! Minimal HTTP(S) GET over the system WinHTTP (TLS, proxy settings and certificates come from Windows; no extra crates).
//! Widgets call it from a `cx.spawn` worker (never on the UI thread, never in preview mode).
use std::ffi::c_void;
use windows::core::{w, PCWSTR};
use windows::Win32::Networking::WinHttp::*;

/// GET `url` (http or https) and return the body. `Err` carries a short human-readable reason.
pub fn http_get(url: &str, timeout_ms: i32) -> Result<Vec<u8>, String> {
    let (https, host, port, path) = split(url).ok_or_else(|| format!("bad url: {url}"))?;
    unsafe {
        let ses = WinHttpOpen(w!("macshell-widgets/0.1"), WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, PCWSTR::null(), PCWSTR::null(), 0);
        if ses.is_null() {
            return Err("WinHttpOpen failed".into());
        }
        let r = fetch(ses, https, &host, port, &path, timeout_ms);
        let _ = WinHttpCloseHandle(ses);
        r
    }
}

/// GET and parse JSON.
pub fn http_get_json(url: &str, timeout_ms: i32) -> Result<serde_json::Value, String> {
    let b = http_get(url, timeout_ms)?;
    serde_json::from_slice(&b).map_err(|e| format!("json: {e}"))
}

/// Redirect hops `download` follows (GitHub release asset: github.com -> a CDN host, one or two hops).
const MAX_HOPS: usize = 4;

/// GET `url` (HTTPS only) into the file `dest`, at most `cap` bytes. Redirects are followed by hand and every target (the first URL
/// included) must pass `allow`, so a redirect can never leave the allowed hosts. `progress(bytes so far, Content-Length)` is called per
/// chunk. On `Err` the file may be partial: the caller deletes it.
pub fn download(url: &str, dest: &std::path::Path, cap: u64, allow: fn(&str) -> bool, mut progress: impl FnMut(u64, Option<u64>)) -> Result<(), String> {
    let mut url = url.to_string();
    for _ in 0..=MAX_HOPS {
        if !allow(&url) {
            return Err(format!("address not allowed: {url}"));
        }
        match download_once(&url, dest, cap, &mut progress)? {
            None => return Ok(()),
            Some(next) => url = next,
        }
    }
    Err("too many redirects".into())
}

/// One request: `Ok(None)` = the body was saved, `Ok(Some(url))` = a redirect to `url`.
fn download_once(url: &str, dest: &std::path::Path, cap: u64, progress: &mut dyn FnMut(u64, Option<u64>)) -> Result<Option<String>, String> {
    use std::io::Write;
    let (https, host, port, path) = split(url).ok_or_else(|| format!("bad url: {url}"))?;
    if !https {
        return Err("https only".into());
    }
    unsafe {
        let ses = WinHttpOpen(w!("macshell-widgets/0.1"), WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, PCWSTR::null(), PCWSTR::null(), 0);
        if ses.is_null() {
            return Err("WinHttpOpen failed".into());
        }
        let _ = WinHttpSetTimeouts(ses, 15_000, 15_000, 15_000, 15_000);
        let (h, p) = (wide(&host), wide(&path));
        let con = WinHttpConnect(ses, PCWSTR(h.as_ptr()), port, 0);
        let req = if con.is_null() { std::ptr::null_mut() } else { WinHttpOpenRequest(con, w!("GET"), PCWSTR(p.as_ptr()), PCWSTR::null(), PCWSTR::null(), std::ptr::null(), WINHTTP_FLAG_SECURE) };
        let out = (|| -> Result<Option<String>, String> {
            if req.is_null() {
                return Err("connect failed".into());
            }
            let never = WINHTTP_OPTION_REDIRECT_POLICY_NEVER.to_ne_bytes();
            WinHttpSetOption(Some(req as *const c_void), WINHTTP_OPTION_REDIRECT_POLICY, Some(&never)).map_err(|e| format!("option: {e}"))?;
            WinHttpSendRequest(req, None, None, 0, 0, 0).map_err(|e| format!("send: {e}"))?;
            WinHttpReceiveResponse(req, std::ptr::null_mut()).map_err(|e| format!("receive: {e}"))?;
            let num = |what: u32| {
                let (mut v, mut len) = (0u32, 4u32);
                WinHttpQueryHeaders(req, what | WINHTTP_QUERY_FLAG_NUMBER, PCWSTR::null(), Some(&mut v as *mut u32 as *mut c_void), &mut len, std::ptr::null_mut()).ok().map(|_| v)
            };
            let status = num(WINHTTP_QUERY_STATUS_CODE).unwrap_or(0);
            if matches!(status, 301 | 302 | 303 | 307 | 308) {
                let mut buf = [0u16; 2048];
                let mut len = (buf.len() * 2) as u32;
                WinHttpQueryHeaders(req, WINHTTP_QUERY_LOCATION, PCWSTR::null(), Some(buf.as_mut_ptr() as *mut c_void), &mut len, std::ptr::null_mut()).map_err(|e| format!("location: {e}"))?;
                return Ok(Some(String::from_utf16_lossy(&buf[..len as usize / 2])));
            }
            if status != 200 {
                return Err(format!("http {status}"));
            }
            let total = num(WINHTTP_QUERY_CONTENT_LENGTH).map(u64::from);
            if total.is_some_and(|t| t > cap) {
                return Err("file too large".into());
            }
            let mut f = std::fs::File::create(dest).map_err(|e| format!("create: {e}"))?;
            let mut got_total = 0u64;
            let mut buf = vec![0u8; 64 << 10];
            loop {
                let mut got = 0u32;
                WinHttpReadData(req, buf.as_mut_ptr() as *mut c_void, buf.len() as u32, &mut got).map_err(|e| format!("read: {e}"))?;
                if got == 0 {
                    break;
                }
                got_total += got as u64;
                if got_total > cap {
                    return Err("file too large".into());
                }
                f.write_all(&buf[..got as usize]).map_err(|e| format!("write: {e}"))?;
                progress(got_total, total);
            }
            if total.is_some_and(|t| t != got_total) {
                return Err("download cut short".into());
            }
            f.flush().map_err(|e| format!("write: {e}"))?;
            Ok(None)
        })();
        if !req.is_null() {
            let _ = WinHttpCloseHandle(req);
        }
        if !con.is_null() {
            let _ = WinHttpCloseHandle(con);
        }
        let _ = WinHttpCloseHandle(ses);
        out
    }
}

fn split(url: &str) -> Option<(bool, String, u16, String)> {
    let (https, rest) = if let Some(r) = url.strip_prefix("https://") { (true, r) } else { (false, url.strip_prefix("http://")?) };
    let (hp, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = match hp.rsplit_once(':') {
        Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) && !p.is_empty() => (h, p.parse().ok()?),
        _ => (hp, if https { 443 } else { 80 }),
    };
    Some((https, host.to_string(), port, path.to_string()))
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

unsafe fn fetch(ses: *mut c_void, https: bool, host: &str, port: u16, path: &str, timeout_ms: i32) -> Result<Vec<u8>, String> {
    let _ = WinHttpSetTimeouts(ses, timeout_ms, timeout_ms, timeout_ms, timeout_ms);
    let h = wide(host);
    let con = WinHttpConnect(ses, PCWSTR(h.as_ptr()), port, 0);
    if con.is_null() {
        return Err("connect failed".into());
    }
    let p = wide(path);
    let flags = if https { WINHTTP_FLAG_SECURE } else { WINHTTP_OPEN_REQUEST_FLAGS(0) };
    let req = WinHttpOpenRequest(con, w!("GET"), PCWSTR(p.as_ptr()), PCWSTR::null(), PCWSTR::null(), std::ptr::null(), flags);
    if req.is_null() {
        let _ = WinHttpCloseHandle(con);
        return Err("open request failed".into());
    }
    let out = (|| -> Result<Vec<u8>, String> {
        WinHttpSendRequest(req, None, None, 0, 0, 0).map_err(|e| format!("send: {e}"))?;
        WinHttpReceiveResponse(req, std::ptr::null_mut()).map_err(|e| format!("receive: {e}"))?;
        let mut status = 0u32;
        let mut len = 4u32;
        let _ = WinHttpQueryHeaders(req, WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER, PCWSTR::null(), Some(&mut status as *mut u32 as *mut c_void), &mut len, std::ptr::null_mut());
        if status != 0 && !(200..300).contains(&status) {
            return Err(format!("http {status}"));
        }
        let mut body = Vec::new();
        loop {
            let mut avail = 0u32;
            WinHttpQueryDataAvailable(req, &mut avail).map_err(|e| format!("avail: {e}"))?;
            if avail == 0 {
                break;
            }
            let mut buf = vec![0u8; avail.min(1 << 20) as usize];
            let mut got = 0u32;
            WinHttpReadData(req, buf.as_mut_ptr() as *mut c_void, buf.len() as u32, &mut got).map_err(|e| format!("read: {e}"))?;
            body.extend_from_slice(&buf[..got as usize]);
            if body.len() > 16 << 20 {
                return Err("response too large".into());
            }
        }
        Ok(body)
    })();
    let _ = WinHttpCloseHandle(req);
    let _ = WinHttpCloseHandle(con);
    out
}

#[cfg(test)]
mod tests {
    use super::split;
    #[test]
    fn urls() {
        assert_eq!(split("https://a.b/c?d=1"), Some((true, "a.b".into(), 443, "/c?d=1".into())));
        assert_eq!(split("http://a.b:8080"), Some((false, "a.b".into(), 8080, "/".into())));
        assert!(split("ftp://x").is_none());
    }
}
