//! Backdrop from Windows Graphics Capture of the desktop window (`Progman`: wallpaper, the animated wallpaper window and the icons - but
//! never other top-level windows, and so never our own layer). That is what lets the layer be visible to screenshots and recorders: the
//! glass does not see itself. Frames arrive as GPU textures and are copied on a private device into the shared backdrop texture (like the
//! duplication path, but without the 250 ms waits); the UI thread only reads the shared texture.
use super::*;
use windows::Graphics::Capture::{Direct3D11CaptureFramePool, GraphicsCaptureItem};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Win32::System::WinRT::Direct3D11::{CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::UI::WindowsAndMessaging::{FindWindowW, GetWindowRect};

pub struct WgcBackdrop {
    bmp: ID2D1Bitmap1,
    shared: Arc<Shared>,
    pending: Mutex<Option<SendBox<WgcCap>>>,
    th: Mutex<Option<std::thread::JoinHandle<()>>>,
    margin: i32,
    size: (i32, i32),
}

struct WgcCap {
    ctx: ID3D11DeviceContext,
    tex: ID3D11Texture2D,
    pool: Direct3D11CaptureFramePool,
    /// monitor origin inside the captured window
    org: (i32, i32),
    size: (i32, i32),
    interval: Duration,
    shared: Arc<Shared>,
    /// keeps the capture session (and its device) alive
    _session: windows::Graphics::Capture::GraphicsCaptureSession,
    _dev: ID3D11Device,
}

impl WgcBackdrop {
    /// `out`: the monitor's output (its rect is the area the backdrop texture covers); `cards`: card rects in monitor px.
    pub fn new(gfx: &Gfx, out: &OutputInfo, cards: &[RECT], margin: i32, fps: u32) -> Result<Self> {
        unsafe {
            let mon = out.rect;
            let (w, h) = ((mon.right - mon.left) as u32, (mon.bottom - mon.top) as u32);
            let prog = FindWindowW(windows::core::w!("Progman"), None)?;
            let mut pr = RECT::default();
            GetWindowRect(prog, &mut pr)?;
            let (cap_dev, cap_ctx) = {
                let a: IDXGIAdapter = out.adapter.cast()?;
                let (mut dev, mut ctx): (Option<ID3D11Device>, Option<ID3D11DeviceContext>) = (None, None);
                D3D11CreateDevice(&a, windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN, windows::Win32::Foundation::HMODULE::default(), D3D11_CREATE_DEVICE_BGRA_SUPPORT, None, D3D11_SDK_VERSION, Some(&mut dev), None, Some(&mut ctx))?;
                (dev.ok_or_else(windows::core::Error::empty)?, ctx.ok_or_else(windows::core::Error::empty)?)
            };
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
            let surf: IDXGISurface = tex_ui.ok_or_else(windows::core::Error::empty)?.cast()?;
            let bmp = gfx.dc.CreateBitmapFromDxgiSurface(&surf, Some(&bmp_props(D2D1_BITMAP_OPTIONS_NONE, D2D1_ALPHA_MODE_IGNORE)))?;

            let interop: IGraphicsCaptureItemInterop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
            let item: GraphicsCaptureItem = interop.CreateForWindow(prog)?;
            let dxgi: IDXGIDevice = cap_dev.cast()?;
            let winrt_dev: IDirect3DDevice = CreateDirect3D11DeviceFromDXGIDevice(&dxgi)?.cast()?;
            let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(&winrt_dev, DirectXPixelFormat::B8G8R8A8UIntNormalized, 2, item.Size()?)?;
            let session = pool.CreateCaptureSession(&item)?;
            let _ = session.SetIsCursorCaptureEnabled(false);
            let _ = session.SetIsBorderRequired(false);
            session.StartCapture()?;

            let shared = Arc::new(Shared {
                regions: Mutex::new(pad_regions(cards, margin, w as i32, h as i32)),
                regions_ver: AtomicU64::new(1),
                dirty: AtomicU64::new(0),
                stop: AtomicBool::new(false),
                paused: AtomicBool::new(false),
                hwnd: AtomicIsize::new(0),
                acquired: AtomicU64::new(0),
                copied: AtomicU64::new(0),
            });
            let cap = SendBox(WgcCap {
                ctx: cap_ctx,
                tex,
                pool,
                org: (mon.left - pr.left, mon.top - pr.top),
                size: (w as i32, h as i32),
                // the glass is blurred: a wallpaper video under it needs no more than ~15 fps
                interval: Duration::from_micros(1_000_000 / fps.clamp(1, 15) as u64),
                shared: shared.clone(),
                _session: session,
                _dev: cap_dev,
            });
            Ok(WgcBackdrop { bmp, shared, pending: Mutex::new(Some(cap)), th: Mutex::new(None), margin, size: (w as i32, h as i32) })
        }
    }
}

impl Drop for WgcBackdrop {
    fn drop(&mut self) {
        self.shared.stop.store(true, Relaxed);
        if let Some(t) = self.th.lock().ok().and_then(|mut g| g.take()) {
            let _ = t.join();
        }
    }
}

impl Backdrop for WgcBackdrop {
    fn bitmap(&self) -> &ID2D1Bitmap1 {
        &self.bmp
    }
    fn name(&self) -> String {
        "desktop-wgc".into()
    }
    fn take_dirty(&self) -> u64 {
        self.shared.dirty.swap(0, Relaxed)
    }
    fn start(&self) {
        let cap = self.pending.lock().ok().and_then(|mut g| g.take());
        if let Some(cap) = cap {
            let th = std::thread::Builder::new().name("wgc-capture".into()).spawn(move || unsafe { wgc_loop(cap.0) }).ok();
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

unsafe fn wgc_loop(c: WgcCap) {
    let sh = &c.shared;
    let mut regions: Vec<RECT> = Vec::new();
    let mut ver = 0u64;
    while !sh.stop.load(Relaxed) {
        let t0 = Instant::now();
        // newest frame only; older ones go back to the pool
        let mut latest = None;
        while let Ok(f) = c.pool.TryGetNextFrame() {
            if let Some(old) = latest.replace(f) {
                let _ = old.Close();
            }
        }
        if sh.paused.load(Relaxed) {
            if let Some(f) = latest {
                let _ = f.Close();
            }
            std::thread::sleep(Duration::from_millis(100));
            continue;
        }
        let v = sh.regions_ver.load(Relaxed);
        if v != ver {
            if let Ok(g) = sh.regions.lock() {
                regions = g.clone();
                ver = v;
            }
        }
        if let Some(f) = latest {
            sh.acquired.fetch_add(1, Relaxed);
            let src = f.Surface().ok().and_then(|s| s.cast::<IDirect3DDxgiInterfaceAccess>().ok()).and_then(|a| a.GetInterface::<ID3D11Texture2D>().ok());
            if let (Some(src), Ok(cs)) = (src, f.ContentSize()) {
                // the visible part of the monitor inside the captured window
                let (l, t) = (c.org.0.max(0), c.org.1.max(0));
                let (r, b) = ((c.org.0 + c.size.0).min(cs.Width), (c.org.1 + c.size.1).min(cs.Height));
                if r > l && b > t {
                    let bx = D3D11_BOX { left: l as u32, top: t as u32, front: 0, right: r as u32, bottom: b as u32, back: 1 };
                    c.ctx.CopySubresourceRegion(&c.tex, 0, (l - c.org.0) as u32, (t - c.org.1) as u32, 0, &src, 0, Some(&bx));
                    c.ctx.Flush();
                    sh.copied.fetch_add(1, Relaxed);
                    let mask = regions.iter().take(64).enumerate().filter(|(_, r)| r.right > r.left && r.bottom > r.top).fold(0u64, |m, (i, _)| m | 1 << i);
                    let prevm = sh.dirty.fetch_or(mask, Relaxed);
                    let h = sh.hwnd.load(Relaxed);
                    if prevm == 0 && h != 0 && mask != 0 {
                        let _ = PostMessageW(HWND(h as *mut _), WM_BACKDROP, WPARAM(0), LPARAM(0));
                    }
                }
            }
            let _ = f.Close();
        }
        let el = t0.elapsed();
        if el < c.interval {
            std::thread::sleep(c.interval - el);
        }
    }
}
