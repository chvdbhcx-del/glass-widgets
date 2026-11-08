//! Screen-reader / UI Automation support. The desk is drawn with Direct2D into one layered window, so without help UIA sees an
//! empty window. This publishes a small tree: the window ("Glass Widgets", a pane) with one child per card (a group named like the
//! widget, with its bounds). It is read-only: no patterns, no focus changes.
//!
//! UIA calls the providers from its own threads, so they never touch the host: the UI thread publishes a `Snap` (owned data) when
//! UIA asks for the window (`WM_GETOBJECT`), and the providers only read that.
#![allow(non_upper_case_globals)] // the UIA constants of the windows crate keep their Win32 names (UIA_NamePropertyId, ...)

use std::sync::{Arc, Mutex};
use windows::core::{implement, Error, Interface, Result, BSTR, VARIANT};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::System::Com::SAFEARRAY;
use windows::Win32::System::Ole::{SafeArrayAccessData, SafeArrayCreateVector, SafeArrayUnaccessData};
use windows::Win32::System::Variant::VT_I4;
use windows::Win32::UI::Accessibility::*;
use windows::Win32::UI::WindowsAndMessaging::GetWindowRect;

/// `WM_GETOBJECT`
pub const WM_GETOBJECT: u32 = 0x003D;

/// One card as the screen reader sees it. `rect` is in screen pixels.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    pub name: String,
    pub id: String,
    pub rect: RECT,
}

/// What the providers read: the window title and the cards in desk order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Snap {
    pub title: String,
    pub items: Vec<Item>,
}

static SNAP: Mutex<Option<Arc<Snap>>> = Mutex::new(None);

pub fn publish(s: Snap) {
    if let Ok(mut g) = SNAP.lock() {
        *g = Some(Arc::new(s));
    }
}

fn snap() -> Arc<Snap> {
    SNAP.lock().ok().and_then(|g| g.clone()).unwrap_or_default()
}

/// Is a Windows high-contrast theme on? `Some(true)` = the theme's window colour is light, `Some(false)` = dark, `None` = off.
pub fn high_contrast() -> Option<bool> {
    use windows::Win32::Graphics::Gdi::{GetSysColor, COLOR_WINDOW};
    use windows::Win32::UI::WindowsAndMessaging::{SystemParametersInfoW, SPI_GETHIGHCONTRAST, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS};
    let mut hc = HIGHCONTRASTW { cbSize: std::mem::size_of::<HIGHCONTRASTW>() as u32, ..Default::default() };
    unsafe { SystemParametersInfoW(SPI_GETHIGHCONTRAST, hc.cbSize, Some(&mut hc as *mut _ as *mut _), SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0)) }.ok()?;
    if hc.dwFlags.0 & HCF_HIGHCONTRASTON.0 == 0 {
        return None;
    }
    let c = unsafe { GetSysColor(COLOR_WINDOW) };
    Some((c & 0xFF) + ((c >> 8) & 0xFF) + ((c >> 16) & 0xFF) > 382) // COLORREF is 0x00BBGGRR; the mean channel above 127
}

/// Index of the card under the screen point (the last one wins: it is drawn on top).
pub fn item_at(items: &[Item], x: i32, y: i32) -> Option<usize> {
    items.iter().rposition(|i| x >= i.rect.left && x < i.rect.right && y >= i.rect.top && y < i.rect.bottom)
}

/// Answer `WM_GETOBJECT` for the UIA root object; None for every other request (the caller falls through to DefWindowProc).
/// The caller publishes the snapshot first.
pub unsafe fn get_object(hwnd: HWND, wp: WPARAM, lp: LPARAM) -> Option<LRESULT> {
    if lp.0 as i32 != UiaRootObjectId {
        return None;
    }
    let p: IRawElementProviderSimple = Root { hwnd: hwnd.0 as isize }.into();
    Some(UiaReturnRawElementProvider(hwnd, wp, lp, &p))
}

fn no_element<T>() -> Result<T> {
    Err(Error::empty()) // S_OK + null: "there is none"
}

fn rid(items: &[i32]) -> Result<*mut SAFEARRAY> {
    unsafe {
        let sa = SafeArrayCreateVector(VT_I4, 0, items.len() as u32);
        if sa.is_null() {
            return Err(Error::from_win32());
        }
        let mut data = std::ptr::null_mut();
        SafeArrayAccessData(sa, &mut data)?;
        std::ptr::copy_nonoverlapping(items.as_ptr(), data as *mut i32, items.len());
        SafeArrayUnaccessData(sa)?;
        Ok(sa)
    }
}

fn prop(id: UIA_PROPERTY_ID, name: &str, auto_id: Option<&str>, control: UIA_CONTROLTYPE_ID, content: bool) -> VARIANT {
    match id {
        UIA_NamePropertyId => VARIANT::from(BSTR::from(name)),
        UIA_AutomationIdPropertyId => auto_id.map_or_else(VARIANT::default, |s| VARIANT::from(BSTR::from(s))),
        UIA_ControlTypePropertyId => VARIANT::from(control.0),
        UIA_IsControlElementPropertyId | UIA_IsEnabledPropertyId => VARIANT::from(true),
        UIA_IsContentElementPropertyId => VARIANT::from(content),
        UIA_IsKeyboardFocusablePropertyId | UIA_HasKeyboardFocusPropertyId => VARIANT::from(false),
        UIA_FrameworkIdPropertyId => VARIANT::from(BSTR::from("GlassWidgets")),
        _ => VARIANT::default(),
    }
}

// ---------------------------------------------------------------- the window

#[implement(IRawElementProviderSimple, IRawElementProviderFragment, IRawElementProviderFragmentRoot)]
struct Root {
    hwnd: isize,
}

impl IRawElementProviderSimple_Impl for Root_Impl {
    fn ProviderOptions(&self) -> Result<ProviderOptions> {
        Ok(ProviderOptions_ServerSideProvider)
    }
    fn GetPatternProvider(&self, _: UIA_PATTERN_ID) -> Result<windows::core::IUnknown> {
        no_element()
    }
    fn GetPropertyValue(&self, id: UIA_PROPERTY_ID) -> Result<VARIANT> {
        Ok(prop(id, &snap().title, None, UIA_PaneControlTypeId, false))
    }
    fn HostRawElementProvider(&self) -> Result<IRawElementProviderSimple> {
        unsafe { UiaHostProviderFromHwnd(HWND(self.hwnd as *mut _)) }
    }
}

impl IRawElementProviderFragment_Impl for Root_Impl {
    fn Navigate(&self, dir: NavigateDirection) -> Result<IRawElementProviderFragment> {
        let n = snap().items.len();
        match dir {
            NavigateDirection_FirstChild if n > 0 => Ok(ItemProv { hwnd: self.hwnd, idx: 0 }.into()),
            NavigateDirection_LastChild if n > 0 => Ok(ItemProv { hwnd: self.hwnd, idx: n - 1 }.into()),
            _ => no_element(),
        }
    }
    fn GetRuntimeId(&self) -> Result<*mut SAFEARRAY> {
        Ok(std::ptr::null_mut()) // the HWND provider supplies the id of the root
    }
    fn BoundingRectangle(&self) -> Result<UiaRect> {
        let mut r = RECT::default();
        unsafe { GetWindowRect(HWND(self.hwnd as *mut _), &mut r)? };
        Ok(UiaRect { left: r.left as f64, top: r.top as f64, width: (r.right - r.left) as f64, height: (r.bottom - r.top) as f64 })
    }
    fn GetEmbeddedFragmentRoots(&self) -> Result<*mut SAFEARRAY> {
        Ok(std::ptr::null_mut())
    }
    fn SetFocus(&self) -> Result<()> {
        Ok(())
    }
    fn FragmentRoot(&self) -> Result<IRawElementProviderFragmentRoot> {
        Ok(Root { hwnd: self.hwnd }.into())
    }
}

impl IRawElementProviderFragmentRoot_Impl for Root_Impl {
    fn ElementProviderFromPoint(&self, x: f64, y: f64) -> Result<IRawElementProviderFragment> {
        match item_at(&snap().items, x as i32, y as i32) {
            Some(idx) => Ok(ItemProv { hwnd: self.hwnd, idx }.into()),
            None => no_element(),
        }
    }
    fn GetFocus(&self) -> Result<IRawElementProviderFragment> {
        no_element()
    }
}

// ---------------------------------------------------------------- one card

#[implement(IRawElementProviderSimple, IRawElementProviderFragment)]
struct ItemProv {
    hwnd: isize,
    idx: usize,
}

impl ItemProv_Impl {
    fn item(&self) -> Result<Item> {
        snap().items.get(self.idx).cloned().ok_or_else(Error::empty)
    }
}

impl IRawElementProviderSimple_Impl for ItemProv_Impl {
    fn ProviderOptions(&self) -> Result<ProviderOptions> {
        Ok(ProviderOptions_ServerSideProvider)
    }
    fn GetPatternProvider(&self, _: UIA_PATTERN_ID) -> Result<windows::core::IUnknown> {
        no_element()
    }
    fn GetPropertyValue(&self, id: UIA_PROPERTY_ID) -> Result<VARIANT> {
        let it = self.item()?;
        Ok(prop(id, &it.name, Some(&it.id), UIA_GroupControlTypeId, true))
    }
    fn HostRawElementProvider(&self) -> Result<IRawElementProviderSimple> {
        no_element()
    }
}

impl IRawElementProviderFragment_Impl for ItemProv_Impl {
    fn Navigate(&self, dir: NavigateDirection) -> Result<IRawElementProviderFragment> {
        let n = snap().items.len();
        match dir {
            NavigateDirection_Parent => Ok(Root { hwnd: self.hwnd }.into()),
            NavigateDirection_NextSibling if self.idx + 1 < n => Ok(ItemProv { hwnd: self.hwnd, idx: self.idx + 1 }.into()),
            NavigateDirection_PreviousSibling if self.idx > 0 && self.idx <= n => Ok(ItemProv { hwnd: self.hwnd, idx: self.idx - 1 }.into()),
            _ => no_element(),
        }
    }
    fn GetRuntimeId(&self) -> Result<*mut SAFEARRAY> {
        rid(&[UiaAppendRuntimeId as i32, self.idx as i32 + 1])
    }
    fn BoundingRectangle(&self) -> Result<UiaRect> {
        let r = self.item()?.rect;
        Ok(UiaRect { left: r.left as f64, top: r.top as f64, width: (r.right - r.left) as f64, height: (r.bottom - r.top) as f64 })
    }
    fn GetEmbeddedFragmentRoots(&self) -> Result<*mut SAFEARRAY> {
        Ok(std::ptr::null_mut())
    }
    fn SetFocus(&self) -> Result<()> {
        Ok(())
    }
    fn FragmentRoot(&self) -> Result<IRawElementProviderFragmentRoot> {
        Ok(Root { hwnd: self.hwnd }.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(name: &str, l: i32, t: i32, r: i32, b: i32) -> Item {
        Item { name: name.into(), id: name.into(), rect: RECT { left: l, top: t, right: r, bottom: b } }
    }

    #[test]
    fn a_point_picks_the_card_drawn_on_top() {
        let items = [item("a", 0, 0, 100, 100), item("b", 50, 50, 150, 150)];
        assert_eq!(item_at(&items, 10, 10), Some(0));
        assert_eq!(item_at(&items, 60, 60), Some(1)); // overlap: the later card is on top
        assert_eq!(item_at(&items, 149, 149), Some(1));
        assert_eq!(item_at(&items, 150, 150), None); // right / bottom edges are exclusive
        assert_eq!(item_at(&items, -1, 5), None);
        assert_eq!(item_at(&[], 0, 0), None);
    }

    #[test]
    fn providers_expose_name_bounds_and_the_tree() {
        publish(Snap { title: "Glass Widgets".into(), items: vec![item("Clock", 10, 20, 110, 70), item("Weather", 120, 20, 220, 70)] });
        let root: IRawElementProviderSimple = Root { hwnd: 0 }.into();
        let name = unsafe { root.GetPropertyValue(UIA_NamePropertyId) }.unwrap();
        assert_eq!(BSTR::try_from(&name).unwrap().to_string(), "Glass Widgets");
        let frag: IRawElementProviderFragment = root.cast().unwrap();
        let first = unsafe { frag.Navigate(NavigateDirection_FirstChild) }.unwrap();
        let rect = unsafe { first.BoundingRectangle() }.unwrap();
        assert_eq!((rect.left, rect.top, rect.width, rect.height), (10.0, 20.0, 100.0, 50.0));
        let second = unsafe { first.Navigate(NavigateDirection_NextSibling) }.unwrap();
        let simple: IRawElementProviderSimple = second.cast().unwrap();
        let n = unsafe { simple.GetPropertyValue(UIA_NamePropertyId) }.unwrap();
        assert_eq!(BSTR::try_from(&n).unwrap().to_string(), "Weather");
        assert!(unsafe { second.Navigate(NavigateDirection_NextSibling) }.is_err(), "no card after the last one");
        assert!(unsafe { first.Navigate(NavigateDirection_PreviousSibling) }.is_err(), "no card before the first one");
        let up = unsafe { second.Navigate(NavigateDirection_Parent) }.unwrap();
        let up_simple: IRawElementProviderSimple = up.cast().unwrap();
        assert_eq!(unsafe { up_simple.ProviderOptions() }.unwrap(), ProviderOptions_ServerSideProvider);
    }
}
