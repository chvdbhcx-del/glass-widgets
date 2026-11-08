//! Update check (opt-in: the "Check for updates" button of the settings panel and the `checkUpdates` switch). One HTTPS GET of the
//! latest GitHub release on a worker thread; only the version, the release page and the installer asset are read.
//!
//! One-click update (only after the user clicks "Update now", never automatic): the installer asset `GlassWidgets_<version>_x64-setup.exe`
//! is downloaded over HTTPS from github.com / its CDN hosts into `%TEMP%\GlassWidgets-update`, its SHA-256 is compared with the `digest`
//! GitHub publishes for the asset, and only a file that matches (`Verified`, the only thing `launch` accepts) is started with `/S /UPDATE`.
use serde_json::Value;
use std::fs::File;
use std::io::Read;
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_APP};

/// posted to the host window when the worker is done
pub const WM_UPDATE: u32 = WM_APP + 5;
const API: &str = "https://api.github.com/repos/chvdbhcx-del/glass-widgets/releases/latest";
/// the release page must live here: the URL comes from the network and is handed to ShellExecute
const PAGE_PREFIX: &str = "https://github.com/";
/// the installer asset must be served from this project's releases
const ASSET_PREFIX: &str = "https://github.com/chvdbhcx-del/glass-widgets/releases/download/";
/// hosts a download may touch: the asset URL itself and GitHub's release CDN (redirects are checked hop by hop)
const HOSTS: [&str; 3] = ["github.com", "objects.githubusercontent.com", "release-assets.githubusercontent.com"];
/// an installer bigger than this is refused (the real one is a few MB)
const MAX_BYTES: u64 = 20 << 20;
/// posted to the host window while installing: wParam 0..=100 = download percent, `DL_VERIFYING`, `DL_DONE` (the result is in `take_install`)
pub const WM_UPDATE_DL: u32 = WM_APP + 6;
pub const DL_VERIFYING: usize = 101;
pub const DL_DONE: usize = 102;

/// Ed25519 public key the installers are signed with (private half: `keys/update-signing.key` in the project folder, never in git; see
/// tools/release.ps1). GitHub alone is not trusted: a release without a valid signature by this key is refused.
const PUBKEY: [u8; 32] = [
    0x80, 0xf8, 0xe0, 0xec, 0x13, 0xfb, 0xd9, 0x58, 0x48, 0xd7, 0x27, 0xb6, 0x55, 0xc5, 0x87, 0x4a, 0x8d, 0x74, 0xde, 0x47, 0xcf, 0x26, 0x23, 0x04, 0x29, 0x07, 0x34, 0x15, 0x46, 0xa6,
    0xb2, 0xef,
];
/// the signed message is this tag + SHA-256 of the installer (examples/sign.rs builds the same bytes)
const MSG_TAG: &[u8] = b"glasswidgets-update-v1\0";
/// a signature file (128 hex digits) bigger than this is refused
const MAX_SIG_BYTES: u64 = 1024;

/// The installer of a release, the SHA-256 GitHub publishes for it and the address of its detached signature (`<installer>.sig`).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Asset {
    pub url: String,
    pub sha256: [u8; 32],
    pub sig_url: String,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Release {
    pub tag: String,
    pub url: String,
    /// `None` when the release has no installer with a well-formed digest ("Update now" then refuses, the release page still works)
    pub asset: Option<Asset>,
}

/// What the settings panel shows.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub enum UpdateState {
    #[default]
    Idle,
    Checking,
    Latest,
    /// `error`: why the last "Update now" failed (shown instead of the version line; the button stays for a retry)
    Available { version: String, url: String, asset: Option<Asset>, error: Option<String> },
    /// "Update now" in progress: download percent, then the checksum
    Downloading(u8),
    Verifying,
    Failed,
}

/// `tag_name` and `html_url` of a "latest release" reply (None when either is missing or the page is not on github.com) and the installer asset.
pub fn parse_release(json: &str) -> Option<Release> {
    let v: Value = serde_json::from_str(json).ok()?;
    let tag = v.get("tag_name")?.as_str()?.trim().to_string();
    let url = v.get("html_url")?.as_str()?.trim().to_string();
    if tag.is_empty() || !url.starts_with(PAGE_PREFIX) {
        return None;
    }
    let asset = installer_name(tag.trim_start_matches(['v', 'V'])).and_then(|want| {
        let a = v.get("assets")?.as_array()?.iter().find(|a| a.get("name").and_then(|n| n.as_str()) == Some(want.as_str()))?;
        let url = a.get("browser_download_url")?.as_str()?.trim().to_string();
        let sha256 = parse_digest(a.get("digest")?.as_str()?)?;
        let sig_name = format!("{want}.sig");
        let s = v.get("assets")?.as_array()?.iter().find(|a| a.get("name").and_then(|n| n.as_str()) == Some(sig_name.as_str()))?;
        let sig_url = s.get("browser_download_url")?.as_str()?.trim().to_string();
        (asset_url_ok(&url) && asset_url_ok(&sig_url)).then_some(Asset { url, sha256, sig_url })
    });
    Some(Release { tag, url, asset })
}

/// `GlassWidgets_<version>_x64-setup.exe`; None for a version that is not plain (it comes from the network and ends up in a file name).
pub fn installer_name(version: &str) -> Option<String> {
    let ok = !version.is_empty() && version.len() <= 32 && version.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-');
    ok.then(|| format!("GlassWidgets_{version}_x64-setup.exe"))
}

/// GitHub's asset `digest` ("sha256:" + 64 hex digits) as bytes; None for anything else (missing, other algorithm, wrong length).
pub fn parse_digest(s: &str) -> Option<[u8; 32]> {
    let hex = s.trim().strip_prefix("sha256:")?;
    if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut out = [0u8; 32];
    for (o, p) in out.iter_mut().zip(hex.as_bytes().chunks(2)) {
        *o = u8::from_str_radix(std::str::from_utf8(p).ok()?, 16).ok()?;
    }
    Some(out)
}

/// Host of an `https://` URL, lower case; None for another scheme, userinfo (`a@b`), a backslash or an explicit port.
fn https_host(url: &str) -> Option<String> {
    let rest = url.strip_prefix("https://")?;
    let host = &rest[..rest.find(['/', '?', '#']).unwrap_or(rest.len())];
    (!host.is_empty() && !host.contains(['@', ':', '\\'])).then(|| host.to_ascii_lowercase())
}

/// The asset must be a file of this project's releases.
pub fn asset_url_ok(url: &str) -> bool {
    url.starts_with(ASSET_PREFIX) && url.len() > ASSET_PREFIX.len() && !url.contains("..") && !url.contains('\\')
}

/// May the download talk to this URL: https and one of the GitHub hosts (every redirect hop is checked with this).
pub fn host_ok(url: &str) -> bool {
    https_host(url).is_some_and(|h| HOSTS.contains(&h.as_str()))
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
        Ok(rel) if is_newer(&rel.tag, current) => UpdateState::Available { version: rel.tag.trim_start_matches(['v', 'V']).to_string(), url: rel.url, asset: rel.asset, error: None },
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

// ---------------------------------------------------------------- one-click update

/// An installer whose SHA-256 matched. The open handle keeps writers out while the process is started from `path`.
pub struct Verified {
    _file: File,
    path: PathBuf,
}

static INSTALL: Mutex<Option<Result<Verified, String>>> = Mutex::new(None);

/// 128 hex digits -> 64 signature bytes.
fn parse_sig(s: &str) -> Option<[u8; 64]> {
    let s = s.trim();
    if s.len() != 128 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut out = [0u8; 64];
    for (o, p) in out.iter_mut().zip(s.as_bytes().chunks(2)) {
        *o = u8::from_str_radix(std::str::from_utf8(p).ok()?, 16).ok()?;
    }
    Some(out)
}

/// Is `sig` a valid signature by `key` over the tag + `hash`?
fn signature_ok(key: &[u8; 32], hash: &[u8; 32], sig: &[u8; 64]) -> bool {
    use ed25519_dalek::{Signature, VerifyingKey};
    let mut msg = MSG_TAG.to_vec();
    msg.extend_from_slice(hash);
    VerifyingKey::from_bytes(key).is_ok_and(|k| k.verify_strict(&msg, &Signature::from_bytes(sig)).is_ok())
}

/// Hash `path` (opened read-only, writers kept out) and compare with `want`, then check the signature `sig` over the hash against `key`.
/// A mismatch of either deletes the file.
fn verify_file(path: &Path, want: &[u8; 32], sig: &[u8; 64], key: &[u8; 32]) -> Result<Verified, String> {
    let mut f = std::fs::OpenOptions::new().read(true).share_mode(1).open(path).map_err(|e| format!("open: {e}"))?; // 1 = FILE_SHARE_READ
    let mut h = crate::sha256::Sha256::new();
    let mut buf = vec![0u8; 64 << 10];
    loop {
        let n = f.read(&mut buf).map_err(|e| format!("read: {e}"))?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    let got = h.finish();
    if got != *want {
        drop(f);
        let _ = std::fs::remove_file(path);
        return Err("checksum mismatch".into());
    }
    if !signature_ok(key, &got, sig) {
        drop(f);
        let _ = std::fs::remove_file(path);
        return Err("bad signature".into());
    }
    Ok(Verified { _file: f, path: path.to_path_buf() })
}

fn post(hwnd: isize, wp: usize) {
    unsafe {
        let _ = PostMessageW(HWND(hwnd as *mut _), WM_UPDATE_DL, WPARAM(wp), LPARAM(0));
    }
}

/// Download + verify on a worker thread; progress and the end arrive as `WM_UPDATE_DL`, the result is picked up with `take_install`.
pub fn start_install(hwnd: isize, version: &str, asset: Asset) {
    let version = version.to_string();
    std::thread::spawn(move || {
        let r = (|| {
            let name = installer_name(&version).ok_or("bad version")?;
            if !asset_url_ok(&asset.url) {
                return Err("bad installer address".to_string());
            }
            let dir = std::env::temp_dir().join("GlassWidgets-update");
            std::fs::create_dir_all(&dir).map_err(|e| format!("temp dir: {e}"))?;
            let path = dir.join(name);
            let _ = std::fs::remove_file(&path);
            let mut last = usize::MAX;
            let got = crate::net::download(&asset.url, &path, MAX_BYTES, host_ok, |n, total| {
                let pct = total.filter(|t| *t > 0).map_or(0, |t| (n * 100 / t).min(100) as usize);
                if pct != last {
                    last = pct;
                    post(hwnd, pct);
                }
            });
            if let Err(e) = got {
                let _ = std::fs::remove_file(&path);
                return Err(e);
            }
            post(hwnd, DL_VERIFYING);
            let sig_path = dir.join(format!("{}.sig", path.file_name().and_then(|n| n.to_str()).unwrap_or("installer")));
            let _ = std::fs::remove_file(&sig_path);
            let sig_got = crate::net::download(&asset.sig_url, &sig_path, MAX_SIG_BYTES, host_ok, |_, _| {});
            let sig_text = sig_got.and_then(|_| std::fs::read_to_string(&sig_path).map_err(|e| format!("signature: {e}")));
            let _ = std::fs::remove_file(&sig_path);
            let sig = parse_sig(&sig_text.inspect_err(|_| drop(std::fs::remove_file(&path)))?).ok_or_else(|| {
                let _ = std::fs::remove_file(&path);
                "malformed signature".to_string()
            })?;
            verify_file(&path, &asset.sha256, &sig, &PUBKEY)
        })();
        if let Ok(mut g) = INSTALL.lock() {
            *g = Some(r);
        }
        post(hwnd, DL_DONE);
    });
}

pub fn take_install() -> Option<Result<Verified, String>> {
    INSTALL.lock().ok().and_then(|mut g| g.take())
}

/// Start the verified installer silently (`/S`; `/UPDATE` makes it start the app again when it is done). The caller exits afterwards.
pub fn launch(v: Verified) -> Result<(), String> {
    std::process::Command::new(&v.path).args(["/S", "/UPDATE"]).spawn().map(|_| ()).map_err(|e| format!("start: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const REPLY: &str = r#"{"url":"https://api.github.com/x","html_url":"https://github.com/chvdbhcx-del/glass-widgets/releases/tag/v0.3.0","tag_name":"v0.3.0","name":"0.3.0","draft":false}"#;
    const ABC: &str = "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
    const DL: &str = "https://github.com/chvdbhcx-del/glass-widgets/releases/download/v0.3.0/GlassWidgets_0.3.0_x64-setup.exe";

    fn reply_with(assets: &str) -> String {
        format!(r#"{{"html_url":"https://github.com/chvdbhcx-del/glass-widgets/releases/tag/v0.3.0","tag_name":"v0.3.0","assets":{assets}}}"#)
    }

    #[test]
    fn reply_is_parsed_and_checked() {
        let r = parse_release(REPLY).unwrap();
        assert_eq!((r.tag.as_str(), r.url.as_str(), r.asset.is_none()), ("v0.3.0", "https://github.com/chvdbhcx-del/glass-widgets/releases/tag/v0.3.0", true));
        assert!(parse_release("{}").is_none() && parse_release("nope").is_none());
        assert!(parse_release(r#"{"tag_name":"v1","html_url":"file:///c:/windows/system32/calc.exe"}"#).is_none());
        assert!(parse_release(r#"{"tag_name":"v1","html_url":"https://evil.example/x"}"#).is_none());
        assert!(parse_release(r#"{"tag_name":"","html_url":"https://github.com/a/b"}"#).is_none());
    }

    #[test]
    fn installer_asset_and_digest_are_picked_from_the_release() {
        let sig = format!(r#"{{"name":"GlassWidgets_0.3.0_x64-setup.exe.sig","browser_download_url":"{DL}.sig"}}"#);
        let good = reply_with(&format!(r#"[{{"name":"other.zip","browser_download_url":"{DL}","digest":"{ABC}"}},{{"name":"GlassWidgets_0.3.0_x64-setup.exe","browser_download_url":"{DL}","digest":"{ABC}"}},{sig}]"#));
        let a = parse_release(&good).unwrap().asset.unwrap();
        assert_eq!((a.url.as_str(), a.sha256[0], a.sha256[31]), (DL, 0xba, 0xad));
        assert_eq!(a.sig_url, format!("{DL}.sig"));
        // no signature file in the release: no installer to offer
        let unsigned = reply_with(&format!(r#"[{{"name":"GlassWidgets_0.3.0_x64-setup.exe","browser_download_url":"{DL}","digest":"{ABC}"}}]"#));
        assert!(parse_release(&unsigned).unwrap().asset.is_none());
        // a signature file served from somewhere else is refused
        let evil_sig = reply_with(&format!(r#"[{{"name":"GlassWidgets_0.3.0_x64-setup.exe","browser_download_url":"{DL}","digest":"{ABC}"}},{{"name":"GlassWidgets_0.3.0_x64-setup.exe.sig","browser_download_url":"https://evil.example/x.sig"}}]"#));
        assert!(parse_release(&evil_sig).unwrap().asset.is_none());
        let named = |name: &str, url: &str, digest: &str| reply_with(&format!(r#"[{{"name":"{name}","browser_download_url":"{url}","digest":"{digest}"}},{sig}]"#));
        let none = |json: String| parse_release(&json).unwrap().asset.is_none();
        let n = "GlassWidgets_0.3.0_x64-setup.exe";
        assert!(none(reply_with("[]")) && none(reply_with("null")));
        assert!(none(named("GlassWidgets_0.2.0_x64-setup.exe", DL, ABC))); // another version
        assert!(none(named(n, DL, "")) && none(named(n, DL, "sha256:abcd")) && none(named(n, DL, &ABC.replace("sha256", "sha1")))); // malformed digest
        assert!(none(reply_with(&format!(r#"[{{"name":"{n}","browser_download_url":"{DL}"}}]"#)))); // no digest at all
        assert!(none(named(n, "https://evil.example/GlassWidgets_0.3.0_x64-setup.exe", ABC)));
        assert!(none(named(n, "https://github.com/someone/else/releases/download/v1/x.exe", ABC)));
    }

    #[test]
    fn digest_parsing() {
        let d = parse_digest(ABC).unwrap();
        assert_eq!((d[0], d[1], d[31]), (0xba, 0x78, 0xad));
        assert_eq!(parse_digest(&ABC.to_uppercase().replace("SHA256", "sha256")), Some(d)); // hex case does not matter
        assert_eq!(parse_digest(&format!("  {ABC}\n")), Some(d));
        for bad in ["", "sha256:", "ba7816bf", "md5:d41d8cd98f00b204e9800998ecf8427e", &ABC[..70], &format!("{ABC}0"), &ABC.replace("ba78", "+a78"), &ABC.replace("ba78", "zz78"), &ABC.replace("sha256:", "SHA256:")] {
            assert_eq!(parse_digest(bad), None, "{bad}");
        }
    }

    #[test]
    fn urls_are_allow_listed() {
        assert!(asset_url_ok(DL));
        for bad in [
            "http://github.com/chvdbhcx-del/glass-widgets/releases/download/v1/x.exe",
            "https://github.com/chvdbhcx-del/glass-widgets/releases/download/",
            "https://github.com/chvdbhcx-del/glass-widgets/releases/download/../../../evil/x.exe",
            "https://github.com/chvdbhcx-del/glass-widgets/releases/latest",
            "https://github.com/chvdbhcx-del/glass-widgets-evil/releases/download/v1/x.exe",
            "https://github.com.evil.example/chvdbhcx-del/glass-widgets/releases/download/v1/x.exe",
        ] {
            assert!(!asset_url_ok(bad), "{bad}");
        }
        for ok in ["https://github.com/a/b", DL, "https://objects.githubusercontent.com/github-production-release-asset/1?X-Amz=2", "https://release-assets.githubusercontent.com/x/y?sig=1", "https://GITHUB.com/x"] {
            assert!(host_ok(ok), "{ok}");
        }
        for bad in [
            "http://github.com/x",
            "https://github.com.evil.example/x",
            "https://evil.example/github.com/x",
            "https://evil.example/?https://github.com/x",
            "https://github.com@evil.example/x",
            "https://evil.example\\@github.com/x",
            "https://github.com:8443/x",
            "https://api.github.com/x",
            "https://githubusercontent.com/x",
            "https://evilobjects.githubusercontent.com/x",
            "https://objects.githubusercontent.com.evil.example/x",
            "https://raw.githubusercontent.com/x",
            "ftp://github.com/x",
            "https://",
            "github.com/x",
        ] {
            assert!(!host_ok(bad), "{bad}");
        }
    }

    #[test]
    fn installer_name_only_for_plain_versions() {
        assert_eq!(installer_name("0.3.0").as_deref(), Some("GlassWidgets_0.3.0_x64-setup.exe"));
        assert_eq!(installer_name("1.0.0-rc1").as_deref(), Some("GlassWidgets_1.0.0-rc1_x64-setup.exe"));
        for bad in ["", "../x", "1/2", "1\\2", "a b", "1.0:x", &"9".repeat(40)] {
            assert_eq!(installer_name(bad), None, "{bad}");
        }
    }

    #[test]
    fn a_file_is_launched_only_when_its_hash_matches() {
        let dir = std::env::temp_dir().join(format!("gw-update-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (good, bad) = (dir.join("good.exe"), dir.join("bad.exe"));
        std::fs::write(&good, b"abc").unwrap();
        std::fs::write(&bad, b"abd").unwrap();
        let want = parse_digest(ABC).unwrap();
        let (key, sig) = signed(&want);
        assert!(verify_file(&good, &want, &sig, &key).is_ok());
        assert_eq!(verify_file(&bad, &want, &sig, &key).err().as_deref(), Some("checksum mismatch"));
        assert!(!bad.exists(), "a file with a wrong hash is deleted");
        assert!(verify_file(&dir.join("missing.exe"), &want, &sig, &key).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// (public key, signature over tag + `hash`) of a throwaway test key.
    fn signed(hash: &[u8; 32]) -> ([u8; 32], [u8; 64]) {
        use ed25519_dalek::{Signer, SigningKey};
        let sk = SigningKey::from_bytes(&[7u8; 32]);
        let mut msg = MSG_TAG.to_vec();
        msg.extend_from_slice(hash);
        (sk.verifying_key().to_bytes(), sk.sign(&msg).to_bytes())
    }

    #[test]
    fn only_the_right_signature_by_the_right_key_passes() {
        let hash = parse_digest(ABC).unwrap();
        let (key, sig) = signed(&hash);
        assert!(signature_ok(&key, &hash, &sig));
        let mut other = hash;
        other[0] ^= 1;
        assert!(!signature_ok(&key, &other, &sig), "another file");
        let mut bent = sig;
        bent[10] ^= 1;
        assert!(!signature_ok(&key, &hash, &bent), "a damaged signature");
        assert!(!signature_ok(&PUBKEY, &hash, &sig), "a signature by another key must not pass for the shipped key");
        assert!(!signature_ok(&[0u8; 32], &hash, &sig), "a degenerate key");
    }

    #[test]
    fn a_signed_file_with_a_wrong_signature_is_deleted() {
        let dir = std::env::temp_dir().join(format!("gw-update-sig-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("good.exe");
        std::fs::write(&f, b"abc").unwrap();
        let want = parse_digest(ABC).unwrap();
        let (key, mut sig) = signed(&want);
        sig[0] ^= 1;
        assert_eq!(verify_file(&f, &want, &sig, &key).err().as_deref(), Some("bad signature"));
        assert!(!f.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn signature_files_are_parsed_strictly() {
        let ok = "ab".repeat(64);
        assert!(parse_sig(&ok).is_some() && parse_sig(&format!("{ok}\n")).is_some());
        for bad in ["".to_string(), "ab".repeat(63), "ab".repeat(65), "zz".repeat(64)] {
            assert!(parse_sig(&bad).is_none());
        }
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
        assert_eq!(state_from(Ok(rel.clone()), "0.2.1"), UpdateState::Available { version: "0.3.0".into(), url: rel.url.clone(), asset: None, error: None });
        assert_eq!(state_from(Ok(rel), "0.3.0"), UpdateState::Latest);
        assert_eq!(state_from(Err("http 403".into()), "0.2.1"), UpdateState::Failed);
    }
}
