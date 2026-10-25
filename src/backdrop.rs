//! What is behind the cards. Three sources behind one trait:
//!  * `FileBackdrop`  - a static PNG (headless dump / tests);
//!  * `DuplBackdrop`  - live DXGI Desktop Duplication, GPU only (see below);
//!  * `PlainBackdrop` - a dark translucent base when duplication is unavailable.
//!
//! Live path (no CPU readback, no per-frame allocation): a capture thread blocks in AcquireNextFrame (250 ms timeout,
//! so a static wallpaper means zero wakeups besides the timeout), reads the frame's dirty/move rects, and for every card
//! whose region (+margin) intersects one of them issues CopySubresourceRegion desktop -> a small window-sized D3D11
//! texture. That texture is wrapped as a D2D bitmap (DXGI surface), which is the input of the glass effect chain. The thread
//! then sets the card's bit in an atomic mask and posts WM_APP+1 to the UI thread (only when the mask was empty), and sleeps
//! to honour the fps cap. The D3D device is multithread-protected, the D2D factory is multithreaded.
use crate::gfx::*;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use windows::core::{Interface, Result};
use windows::Win32::Foundation::{HWND, LPARAM, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::Graphics::Gdi::{MonitorFromPoint, MONITOR_DEFAULTTOPRIMARY};
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_APP};

pub const WM_BACKDROP: u32 = WM_APP + 1;

mod gdi;
mod wgc;
pub use gdi::GdiBackdrop;
pub use wgc::WgcBackdrop;

pub trait Backdrop {
    /// Window-sized bitmap the glass effects read.
    fn bitmap(&self) -> &ID2D1Bitmap1;
    fn name(&self) -> String;
    /// Bit i = the backdrop behind card i changed since the last call.
    fn take_dirty(&self) -> u64 {
        0
    }
    fn set_paused(&self, _paused: bool) {}
    /// Begin capturing (called once, after the scene's GPU objects are created); static sources ignore it.
    fn start(&self) {}
    /// The places (backdrop bitmap px) whose pixels matter: bit i of `take_dirty` = region i changed. Called when cards move / are added.
    fn set_regions(&self, _regions: &[RECT]) {}
    /// The window exists: the capture thread may post WM_BACKDROP to it.
    fn set_hwnd(&self, _h: HWND) {}
    /// (frames acquired from the duplication API, frames that caused GPU copies)
    fn stats(&self) -> (u64, u64) {
        (0, 0)
    }
}

// ---------------------------------------------------------------- static sources

pub struct FileBackdrop(ID2D1Bitmap1);
impl FileBackdrop {
    pub fn new(gfx: &Gfx, path: &str, w: u32, h: u32) -> Result<Self> {
        let px = gfx.load_png(path, w, h)?;
        Ok(FileBackdrop(gfx.from_bgra(w, h, &px)?))
    }
}
impl Backdrop for FileBackdrop {
    fn bitmap(&self) -> &ID2D1Bitmap1 {
        &self.0
    }
    fn name(&self) -> String {
        "file".into()
    }
}

/// Fallback base: translucent dark (the CSS "plain" glass: rgb(30 30 36) at ~.55).
pub struct PlainBackdrop(ID2D1Bitmap1);
impl PlainBackdrop {
    pub fn new(gfx: &Gfx, w: u32, h: u32) -> Result<Self> {
        let a = 0.55f32;
        let px: Vec<u8> = (0..w * h).flat_map(|_| [(36.0 * a) as u8, (30.0 * a) as u8, (30.0 * a) as u8, (255.0 * a) as u8]).collect();
        Ok(PlainBackdrop(gfx.from_bgra(w, h, &px)?))
    }
}
impl Backdrop for PlainBackdrop {
    fn bitmap(&self) -> &ID2D1Bitmap1 {
        &self.0
    }
    fn name(&self) -> String {
        "plain".into()
    }
}

// ---------------------------------------------------------------- output lookup

pub struct OutputInfo {
    pub adapter: IDXGIAdapter1,
    pub output: IDXGIOutput1,
    /// Desktop coordinates of the monitor.
    pub rect: RECT,
    pub adapter_name: String,
    /// LUID of the adapter (high part << 32 | low part): a shared capture texture only works between devices on the same GPU
    pub luid: i64,
}

/// The adapter/output pair that owns the primary monitor (the machine has two GPUs: the device must live on the owner).
pub fn find_primary_output() -> Option<OutputInfo> {
    find_output(None)
}

/// The adapter/output pair of the monitor with GDI device name `device` (`\\.\DISPLAY2`); `None` = the primary monitor.
pub fn find_output(device: Option<&str>) -> Option<OutputInfo> {
    unsafe {
        let f: IDXGIFactory1 = CreateDXGIFactory1().ok()?;
        let prim = MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY);
        let mut ai = 0;
        while let Ok(a) = f.EnumAdapters1(ai) {
            ai += 1;
            let mut oi = 0;
            while let Ok(o) = a.EnumOutputs(oi) {
                oi += 1;
                let Ok(d) = o.GetDesc() else { continue };
                let hit = match device {
                    None => d.Monitor == prim,
                    Some(n) => {
                        let len = d.DeviceName.iter().position(|&c| c == 0).unwrap_or(d.DeviceName.len());
                        String::from_utf16_lossy(&d.DeviceName[..len]).eq_ignore_ascii_case(n)
                    }
                };
                if hit {
                    let desc = a.GetDesc1().ok();
                    let name = desc.as_ref().map(|d| String::from_utf16_lossy(&d.Description).trim_end_matches('\0').to_string()).unwrap_or_default();
                    let luid = desc.map_or(0, |d| ((d.AdapterLuid.HighPart as i64) << 32) | d.AdapterLuid.LowPart as i64);
                    let output = o.cast::<IDXGIOutput1>().ok()?;
                    return Some(OutputInfo { adapter: a, output, rect: d.DesktopCoordinates, adapter_name: name, luid });
                }
            }
        }
        None
    }
}

// ---------------------------------------------------------------- live duplication

struct Shared {
    dirty: AtomicU64,
    stop: AtomicBool,
    paused: AtomicBool,
    hwnd: AtomicIsize,
    acquired: AtomicU64,
    copied: AtomicU64,
    /// regions (backdrop px, margin included) that raise a dirty bit + a version the capture thread polls
    regions: Mutex<Vec<RECT>>,
    regions_ver: AtomicU64,
}

struct SendBox<T>(T);
unsafe impl<T> Send for SendBox<T> {}

struct Capture {
    dupl: IDXGIOutputDuplication,
    output: IDXGIOutput1,
    dev: ID3D11Device,
    ctx: ID3D11DeviceContext,
    tex: ID3D11Texture2D,
    /// backdrop texture size (px); it covers the output rectangle at `org`
    size: (i32, i32),
    /// texture origin inside the output
    org: (i32, i32),
    interval: Duration,
    shared: Arc<Shared>,
}

pub struct DuplBackdrop {
    bmp: ID2D1Bitmap1,
    shared: Arc<Shared>,
    /// the capture thread starts in `start()` (after the scene's GPU objects exist), not in `new()`
    pending: Mutex<Option<SendBox<Capture>>>,
    th: Mutex<Option<std::thread::JoinHandle<()>>>,
    adapter: String,
    margin: i32,
    size: (i32, i32),
}

impl DuplBackdrop {
    /// `win`: the area of the monitor the backdrop texture covers (the whole monitor: card positions are monitor px); `cards`: card rects in
    /// those px; `margin`: px around each card whose changes raise the card's dirty bit. The texture always holds the CURRENT desktop
    /// everywhere (every dirty rect is copied), so a card dragged to a never-seen place has its backdrop at once.
    pub fn new(gfx: &Gfx, out: &OutputInfo, win: RECT, cards: &[RECT], margin: i32, fps: u32) -> Result<Self> {
        unsafe {
            let (w, h) = ((win.right - win.left) as u32, (win.bottom - win.top) as u32);
            // The capture thread gets its OWN device: duplication calls can sit in the kernel with the device lock held (capture hooks such as
            // NVIDIA's make it worse) and would stall the UI thread's D2D work on the shared device. The backdrop texture is shared.
            let (cap_dev, cap_ctx) = {
                let a: IDXGIAdapter = out.adapter.cast()?;
                let (mut dev, mut ctx): (Option<ID3D11Device>, Option<ID3D11DeviceContext>) = (None, None);
                windows::Win32::Graphics::Direct3D11::D3D11CreateDevice(
                    &a,
                    windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN,
                    windows::Win32::Foundation::HMODULE::default(),
                    D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                    None,
                    D3D11_SDK_VERSION,
                    Some(&mut dev),
                    None,
                    Some(&mut ctx),
                )?;
                (dev.ok_or_else(windows::core::Error::empty)?, ctx.ok_or_else(windows::core::Error::empty)?)
            };
            let dupl = out.output.DuplicateOutput(&cap_dev)?;
            let desc = D3D11_TEXTURE2D_DESC {
                Width: w,
                Height: h,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
                CPUAccessFlags: 0,
                MiscFlags: D3D11_RESOURCE_MISC_SHARED.0 as u32,
            };
            let mut tex: Option<ID3D11Texture2D> = None;
            cap_dev.CreateTexture2D(&desc, None, Some(&mut tex))?;
            let tex = tex.ok_or_else(windows::core::Error::empty)?;
            let handle = tex.cast::<IDXGIResource>()?.GetSharedHandle()?;
            let mut tex_ui: Option<ID3D11Texture2D> = None;
            gfx.d3d.OpenSharedResource(handle, &mut tex_ui)?;
            let tex_ui = tex_ui.ok_or_else(windows::core::Error::empty)?;
            let surf: IDXGISurface = tex_ui.cast()?;
            let props = bmp_props(D2D1_BITMAP_OPTIONS_NONE, D2D1_ALPHA_MODE_IGNORE);
            let bmp = gfx.dc.CreateBitmapFromDxgiSurface(&surf, Some(&props))?;
            let regions = pad_regions(cards, margin, w as i32, h as i32);
            let shared = Arc::new(Shared {
                regions: Mutex::new(regions),
                regions_ver: AtomicU64::new(1),
                dirty: AtomicU64::new(0),
                stop: AtomicBool::new(false),
                paused: AtomicBool::new(false),
                hwnd: AtomicIsize::new(0),
                acquired: AtomicU64::new(0),
                copied: AtomicU64::new(0),
            });
            let cap = SendBox(Capture {
                dupl,
                output: out.output.clone(),
                dev: cap_dev,
                ctx: cap_ctx,
                tex,
                size: (w as i32, h as i32),
                org: (win.left, win.top),
                interval: Duration::from_micros(1_000_000 / fps.clamp(1, 240) as u64),
                shared: shared.clone(),
            });
            Ok(DuplBackdrop { bmp, shared, pending: Mutex::new(Some(cap)), th: Mutex::new(None), adapter: out.adapter_name.clone(), margin, size: (w as i32, h as i32) })
        }
    }
}

impl Drop for DuplBackdrop {
    fn drop(&mut self) {
        self.shared.stop.store(true, Relaxed);
        if let Some(t) = self.th.lock().ok().and_then(|mut g| g.take()) {
            let _ = t.join(); // bounded by the 250 ms AcquireNextFrame timeout
        }
    }
}

impl Backdrop for DuplBackdrop {
    fn bitmap(&self) -> &ID2D1Bitmap1 {
        &self.bmp
    }
    fn name(&self) -> String {
        format!("desktop-duplication on {}", self.adapter)
    }
    fn take_dirty(&self) -> u64 {
        self.shared.dirty.swap(0, Relaxed)
    }
    fn start(&self) {
        let cap = self.pending.lock().ok().and_then(|mut g| g.take());
        if let Some(cap) = cap {
            let th = std::thread::Builder::new().name("dxgi-capture".into()).spawn(move || capture_main(cap)).ok();
            if let Ok(mut g) = self.th.lock() {
                *g = th;
            }
        }
    }
    fn set_paused(&self, p: bool) {
        self.shared.paused.store(p, Relaxed);
    }
    fn set_hwnd(&self, h: HWND) {
        self.shared.hwnd.store(h.0 as isize, Relaxed);
    }
    fn set_regions(&self, r: &[RECT]) {
        if let Ok(mut g) = self.shared.regions.lock() {
            *g = pad_regions(r, self.margin, self.size.0, self.size.1);
        }
        self.shared.regions_ver.fetch_add(1, Relaxed);
    }
    fn stats(&self) -> (u64, u64) {
        (self.shared.acquired.load(Relaxed), self.shared.copied.load(Relaxed))
    }
}

fn pad_regions(cards: &[RECT], margin: i32, w: i32, h: i32) -> Vec<RECT> {
    cards
        .iter()
        .map(|c| if c.right <= c.left || c.bottom <= c.top { RECT::default() } else { RECT { left: (c.left - margin).max(0), top: (c.top - margin).max(0), right: (c.right + margin).min(w), bottom: (c.bottom + margin).min(h) } })
        .collect()
}

fn hit(a: &RECT, b: &RECT) -> bool {
    a.left < b.right && b.left < a.right && a.top < b.bottom && b.top < a.bottom
}

fn capture_main(cap: SendBox<Capture>) {
    let c = cap.0;
    unsafe { capture_loop(c) }
}

unsafe fn capture_loop(c: Capture) {
    let sh = &c.shared;
    let mut dupl = Some(c.dupl.clone());
    let mut dirty_buf: Vec<RECT> = Vec::new(); // grown once, reused: no per-frame allocation
    let mut move_buf: Vec<DXGI_OUTDUPL_MOVE_RECT> = Vec::new();
    let mut need_full = true;
    let mut regions: Vec<RECT> = Vec::new();
    let mut regions_ver = 0u64;
    let mut last = Instant::now();
    while !sh.stop.load(Relaxed) {
        if sh.paused.load(Relaxed) {
            std::thread::sleep(Duration::from_millis(100));
            need_full = true;
            continue;
        }
        let Some(d) = dupl.as_ref() else {
            std::thread::sleep(Duration::from_millis(400)); // ACCESS_LOST (mode switch, UAC, lock screen): retry
            dupl = c.output.DuplicateOutput(&c.dev).ok();
            need_full = true;
            continue;
        };
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut res: Option<IDXGIResource> = None;
        match d.AcquireNextFrame(250, &mut info, &mut res) {
            Ok(()) => {}
            Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => {
                continue; // nothing changed: stay idle
            }
            Err(e) => {
                eprintln!("mswidgets: AcquireNextFrame failed: {e} (re-duplicating)");
                dupl = None;
                continue;
            }
        }
        sh.acquired.fetch_add(1, Relaxed);
        let mut mask = 0u64;
        let updated = info.LastPresentTime != 0 || need_full; // LastPresentTime == 0: pointer-only update
        if let (Some(res), true) = (res.as_ref(), updated) {
            // dirty + move rectangles (desktop-output coordinates)
            let (mut nd, mut nm, mut known) = (0usize, 0usize, need_full == false);
            let total = info.TotalMetadataBufferSize as usize;
            if total > 0 {
                let (cm, cd) = (total / std::mem::size_of::<DXGI_OUTDUPL_MOVE_RECT>() + 1, total / 16 + 1);
                if move_buf.len() < cm {
                    move_buf.resize(cm, DXGI_OUTDUPL_MOVE_RECT::default());
                }
                if dirty_buf.len() < cd {
                    dirty_buf.resize(cd, RECT::default());
                }
                let mut req = 0u32;
                if d.GetFrameMoveRects((move_buf.len() * std::mem::size_of::<DXGI_OUTDUPL_MOVE_RECT>()) as u32, move_buf.as_mut_ptr(), &mut req).is_ok() {
                    nm = req as usize / std::mem::size_of::<DXGI_OUTDUPL_MOVE_RECT>();
                } else {
                    known = false;
                }
                if d.GetFrameDirtyRects((dirty_buf.len() * 16) as u32, dirty_buf.as_mut_ptr(), &mut req).is_ok() {
                    nd = req as usize / 16;
                } else {
                    known = false;
                }
            } else if info.AccumulatedFrames > 0 {
                known = false; // an update without metadata: assume everything changed
            }
            if let Ok(src) = res.cast::<ID3D11Texture2D>() {
                let v = sh.regions_ver.load(Relaxed);
                if v != regions_ver {
                    if let Ok(g) = sh.regions.lock() {
                        regions.clear();
                        regions.extend_from_slice(&g);
                        regions_ver = v;
                    }
                }
                // keep the whole texture current: every dirty / moved rectangle is copied (clipped to the texture)
                let (ox, oy, tw, th) = (c.org.0, c.org.1, c.size.0, c.size.1);
                let copy = |g: &RECT| {
                    let (l, t, r, b) = (g.left.max(ox), g.top.max(oy), g.right.min(ox + tw), g.bottom.min(oy + th));
                    if r > l && b > t {
                        let bx = D3D11_BOX { left: l as u32, top: t as u32, front: 0, right: r as u32, bottom: b as u32, back: 1 };
                        c.ctx.CopySubresourceRegion(&c.tex, 0, (l - ox) as u32, (t - oy) as u32, 0, &src, 0, Some(&bx));
                    }
                };
                if known {
                    for d in &dirty_buf[..nd] {
                        copy(d);
                    }
                    for m in &move_buf[..nm] {
                        copy(&m.DestinationRect);
                    }
                } else {
                    copy(&RECT { left: ox, top: oy, right: ox + tw, bottom: oy + th });
                }
                for (i, r) in regions.iter().enumerate().take(64) {
                    let g = RECT { left: r.left + ox, top: r.top + oy, right: r.right + ox, bottom: r.bottom + oy };
                    let changed = !known || dirty_buf[..nd].iter().any(|d| hit(d, &g)) || move_buf[..nm].iter().any(|m| hit(&m.DestinationRect, &g));
                    if changed && g.right > g.left && g.bottom > g.top {
                        mask |= 1u64 << i;
                    }
                }
            }
        }
        c.ctx.Flush(); // the texture is shared with the UI device: make the copies visible there
        let _ = d.ReleaseFrame();
        need_full = false;
        if mask != 0 {
            sh.copied.fetch_add(1, Relaxed);
            let prev = sh.dirty.fetch_or(mask, Relaxed);
            let h = sh.hwnd.load(Relaxed);
            if prev == 0 && h != 0 {
                let _ = PostMessageW(HWND(h as *mut _), WM_BACKDROP, WPARAM(0), LPARAM(0));
            }
        }
        // fps cap: frames that arrive meanwhile are accumulated by the OS and picked up by the next acquire
        let el = last.elapsed();
        if el < c.interval {
            std::thread::sleep(c.interval - el);
        }
        last = Instant::now();
    }
}
