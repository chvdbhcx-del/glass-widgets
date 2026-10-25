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

/// A raw HTTP answer: status, the few headers we use and the body (empty unless the status is 2xx).
pub struct Reply {
    pub status: u32,
    pub location: Option<String>,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub body: Vec<u8>,
}

/// ONE GET of `url` (http or https), redirects NOT followed (the caller checks every hop), `extra` = request header lines (each ending in
/// CRLF), a 2xx body capped at `max_bytes` (`Err` when bigger), `deadline` bounds the whole transfer. Errors never contain the URL
/// (calendar links carry secret tokens).
pub fn http_get_once(url: &str, timeout_ms: i32, max_bytes: usize, extra: &str, deadline: Option<std::time::Instant>) -> Result<Reply, String> {
    let (https, host, port, path) = split(url).ok_or("bad url")?;
    unsafe {
        let ses = WinHttpOpen(w!("macshell-widgets/0.1"), WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, PCWSTR::null(), PCWSTR::null(), 0);
        if ses.is_null() {
            return Err("WinHttpOpen failed".into());
        }
        let r = fetch_ex(ses, https, &host, port, &path, timeout_ms, max_bytes, extra, true, deadline);
        let _ = WinHttpCloseHandle(ses);
        r
    }
}

unsafe fn query_str(req: *mut c_void, what: u32) -> Option<String> {
    let mut buf = [0u16; 1024];
    let mut len = std::mem::size_of_val(&buf) as u32;
    WinHttpQueryHeaders(req, what, PCWSTR::null(), Some(buf.as_mut_ptr() as *mut c_void), &mut len, std::ptr::null_mut()).ok()?;
    let s = String::from_utf16_lossy(&buf[..(len as usize / 2).min(buf.len())]);
    Some(s.trim_matches('\0').trim().to_string()).filter(|s| !s.is_empty())
}

unsafe fn fetch(ses: *mut c_void, https: bool, host: &str, port: u16, path: &str, timeout_ms: i32) -> Result<Vec<u8>, String> {
    let r = fetch_ex(ses, https, host, port, path, timeout_ms, 16 << 20, "", false, None)?;
    if r.status != 0 && !(200..300).contains(&r.status) {
        return Err(format!("http {}", r.status));
    }
    Ok(r.body)
}

#[allow(clippy::too_many_arguments)]
unsafe fn fetch_ex(ses: *mut c_void, https: bool, host: &str, port: u16, path: &str, timeout_ms: i32, max_bytes: usize, extra: &str, no_redirect: bool, deadline: Option<std::time::Instant>) -> Result<Reply, String> {
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
    let out = (|| -> Result<Reply, String> {
        if no_redirect {
            let policy = WINHTTP_OPTION_REDIRECT_POLICY_NEVER.to_ne_bytes();
            let _ = WinHttpSetOption(Some(req as *const c_void), WINHTTP_OPTION_REDIRECT_POLICY, Some(&policy));
        }
        let hdr = wide(extra);
        let hdr = (!extra.is_empty()).then(|| &hdr[..hdr.len() - 1]);
        WinHttpSendRequest(req, hdr, None, 0, 0, 0).map_err(|e| format!("send: {e}"))?;
        WinHttpReceiveResponse(req, std::ptr::null_mut()).map_err(|e| format!("receive: {e}"))?;
        let mut status = 0u32;
        let mut len = 4u32;
        let _ = WinHttpQueryHeaders(req, WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER, PCWSTR::null(), Some(&mut status as *mut u32 as *mut c_void), &mut len, std::ptr::null_mut());
        let mut r = Reply { status, location: None, etag: None, last_modified: None, body: Vec::new() };
        if status != 0 && !(200..300).contains(&status) {
            if (300..400).contains(&status) {
                r.location = query_str(req, WINHTTP_QUERY_LOCATION);
            }
            return Ok(r);
        }
        r.etag = query_str(req, WINHTTP_QUERY_ETAG);
        r.last_modified = query_str(req, WINHTTP_QUERY_LAST_MODIFIED);
        loop {
            if deadline.is_some_and(|d| std::time::Instant::now() > d) {
                return Err("timeout".into());
            }
            let mut avail = 0u32;
            WinHttpQueryDataAvailable(req, &mut avail).map_err(|e| format!("avail: {e}"))?;
            if avail == 0 {
                break;
            }
            let mut buf = vec![0u8; avail.min(1 << 20) as usize];
            let mut got = 0u32;
            WinHttpReadData(req, buf.as_mut_ptr() as *mut c_void, buf.len() as u32, &mut got).map_err(|e| format!("read: {e}"))?;
            r.body.extend_from_slice(&buf[..got as usize]);
            if r.body.len() > max_bytes {
                return Err("response too large".into());
            }
        }
        Ok(r)
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
