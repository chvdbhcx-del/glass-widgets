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
