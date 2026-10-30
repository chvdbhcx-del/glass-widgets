//! The clipboard seen by an editor: a two-method trait, the Win32 implementation and an in-memory fake for tests.
//!
//! Text only. On the wire (`CF_UNICODETEXT`) line breaks are CR LF and the buffer is NUL-terminated; the helpers [`to_wire`] / [`from_wire`]
//! convert (pure, unit-tested). The Win32 calls retry opening the clipboard a few times because another program may hold it for a moment.
use std::cell::RefCell;
use std::rc::Rc;

/// Where Ctrl+C / X / V go.
pub trait Clipboard {
    /// The text on the clipboard (None when it holds no text or cannot be read).
    fn get_text(&self) -> Option<String>;
    fn set_text(&mut self, text: &str);
}

/// In-memory clipboard. Clones share the same content, so a test keeps one handle and gives another to the editor.
#[derive(Clone, Default, Debug)]
pub struct FakeClipboard(pub Rc<RefCell<Option<String>>>);

impl FakeClipboard {
    pub fn with(text: &str) -> FakeClipboard {
        FakeClipboard(Rc::new(RefCell::new(Some(text.to_string()))))
    }
    pub fn content(&self) -> Option<String> {
        self.0.borrow().clone()
    }
}

impl Clipboard for FakeClipboard {
    fn get_text(&self) -> Option<String> {
        self.0.borrow().clone()
    }
    fn set_text(&mut self, text: &str) {
        *self.0.borrow_mut() = Some(text.to_string());
    }
}

/// Text -> clipboard UTF-16: LF becomes CR LF (a lone CR is left alone), NUL-terminated.
pub fn to_wire(s: &str) -> Vec<u16> {
    let mut v: Vec<u16> = Vec::with_capacity(s.len() + 1);
    let mut prev_cr = false;
    for u in s.encode_utf16() {
        if u == 0x0A && !prev_cr {
            v.push(0x0D);
        }
        prev_cr = u == 0x0D;
        v.push(u);
    }
    v.push(0);
    v
}

/// Clipboard UTF-16 -> text: cut at the first NUL, CR LF becomes LF, invalid UTF-16 is replaced.
pub fn from_wire(w: &[u16]) -> String {
    let end = w.iter().position(|&u| u == 0).unwrap_or(w.len());
    String::from_utf16_lossy(&w[..end]).replace("\r\n", "\n")
}

/// The system clipboard (Win32 `CF_UNICODETEXT`).
#[derive(Clone, Copy, Debug, Default)]
pub struct WinClipboard;

impl WinClipboard {
    fn open() -> bool {
        use windows::Win32::Foundation::HWND;
        use windows::Win32::System::DataExchange::OpenClipboard;
        for _ in 0..6 {
            if unsafe { OpenClipboard(HWND::default()) }.is_ok() {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(8));
        }
        false
    }
}

impl Clipboard for WinClipboard {
    fn get_text(&self) -> Option<String> {
        use windows::Win32::Foundation::HGLOBAL;
        use windows::Win32::System::DataExchange::{CloseClipboard, GetClipboardData, IsClipboardFormatAvailable};
        use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
        use windows::Win32::System::Ole::CF_UNICODETEXT;
        unsafe {
            if IsClipboardFormatAvailable(CF_UNICODETEXT.0 as u32).is_err() || !WinClipboard::open() {
                return None;
            }
            let mut out = None;
            if let Ok(h) = GetClipboardData(CF_UNICODETEXT.0 as u32) {
                let g = HGLOBAL(h.0);
                let p = GlobalLock(g) as *const u16;
                if !p.is_null() {
                    let words = GlobalSize(g) / 2;
                    out = Some(from_wire(std::slice::from_raw_parts(p, words)));
                    let _ = GlobalUnlock(g);
                }
            }
            let _ = CloseClipboard();
            out
        }
    }

    fn set_text(&mut self, text: &str) {
        use windows::Win32::Foundation::HANDLE;
        use windows::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, SetClipboardData};
        use windows::Win32::Foundation::GlobalFree;
        use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
        use windows::Win32::System::Ole::CF_UNICODETEXT;
        let w = to_wire(text);
        unsafe {
            if !WinClipboard::open() {
                return;
            }
            let _ = EmptyClipboard();
            if let Ok(g) = GlobalAlloc(GMEM_MOVEABLE, w.len() * 2) {
                let p = GlobalLock(g) as *mut u16;
                if p.is_null() {
                    let _ = GlobalFree(g);
                } else {
                    std::ptr::copy_nonoverlapping(w.as_ptr(), p, w.len());
                    let _ = GlobalUnlock(g);
                    // on success the system owns the memory; on failure it is ours to free
                    if SetClipboardData(CF_UNICODETEXT.0 as u32, HANDLE(g.0)).is_err() {
                        let _ = GlobalFree(g);
                    }
                }
            }
            let _ = CloseClipboard();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_format_round_trips_line_breaks() {
        let w = to_wire("a\nb\r\nc");
        assert_eq!(*w.last().unwrap(), 0);
        let s = String::from_utf16_lossy(&w[..w.len() - 1]);
        assert_eq!(s, "a\r\nb\r\nc", "LF becomes CR LF, an existing CR LF is not doubled");
        assert_eq!(from_wire(&w), "a\nb\nc");
        assert_eq!(from_wire(&to_wire("Привет 😀")), "Привет 😀");
        assert_eq!(from_wire(&[0x61, 0, 0x62]), "a", "cut at the NUL (the buffer of GlobalSize may be longer)");
        assert_eq!(from_wire(&[]), "");
        assert_eq!(from_wire(&[0xD800, 0x61]), "\u{FFFD}a", "a lone surrogate does not panic");
    }

    #[test]
    fn fake_clipboard_clones_share_content() {
        let a = FakeClipboard::default();
        let mut b = a.clone();
        assert_eq!(a.get_text(), None);
        b.set_text("x");
        assert_eq!(a.content().as_deref(), Some("x"));
        assert_eq!(FakeClipboard::with("y").get_text().as_deref(), Some("y"));
    }
}
