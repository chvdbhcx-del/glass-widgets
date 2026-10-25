//! Live screen capture with plain GDI `BitBlt` of just the regions under the cards (a few 100k px), compared with the previous copy so
//! only real changes are uploaded and flagged. It does not depend on DXGI Desktop Duplication (whose frames can stop arriving in a
//! process with capture hooks, and whose 250 ms waits on a shared device starved the UI thread). The layer itself is excluded from
//! capture (WDA), so the copy is what is BEHIND the widgets. Polls at `fps` while things change and backs off to 2 Hz while
//! the screen is still (`pace.rs`).
use super::*;

pub struct GdiBackdrop {
    bmp: ID2D1Bitmap1,
    shared: Arc<Shared>,
    pending: Mutex<Option<SendBox<GdiCap>>>,
    th: Mutex<Option<std::thread::JoinHandle<()>>>,
    margin: i32,
    size: (i32, i32),
}

struct GdiCap {
    ctx: ID3D11DeviceContext,
    tex: ID3D11Texture2D,
    org: (i32, i32),
    size: (i32, i32),
    rate: Rate,
    shared: Arc<Shared>,
}

impl GdiBackdrop {
    /// `mon`: the monitor rectangle in desktop px (the backdrop texture covers all of it); `cards`: card rects in monitor px.
    pub fn new(gfx: &Gfx, mon: RECT, cards: &[RECT], margin: i32, rate: Rate) -> Result<Self> {
        unsafe {
            let (w, h) = ((mon.right - mon.left) as u32, (mon.bottom - mon.top) as u32);
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
                MiscFlags: 0,
            };
            let mut tex: Option<ID3D11Texture2D> = None;
            gfx.d3d.CreateTexture2D(&desc, None, Some(&mut tex))?;
            let tex = tex.ok_or_else(windows::core::Error::empty)?;
            let surf: IDXGISurface = tex.cast()?;
            let bmp = gfx.dc.CreateBitmapFromDxgiSurface(&surf, Some(&bmp_props(D2D1_BITMAP_OPTIONS_NONE, D2D1_ALPHA_MODE_IGNORE)))?;
            let shared = Shared::new(pad_regions(cards, margin, w as i32, h as i32));
            let cap = SendBox(GdiCap {
                ctx: gfx.ctx.clone(),
                tex,
                org: (mon.left, mon.top),
                size: (w as i32, h as i32),
                rate: Rate { fps: rate.fps.clamp(1, 120), auto: rate.auto },
                shared: shared.clone(),
            });
            Ok(GdiBackdrop { bmp, shared, pending: Mutex::new(Some(cap)), th: Mutex::new(None), margin, size: (w as i32, h as i32) })
        }
    }
}

impl Drop for GdiBackdrop {
    fn drop(&mut self) {
        self.shared.stop.store(true, Relaxed);
        if let Some(t) = self.th.lock().ok().and_then(|mut g| g.take()) {
            let _ = t.join();
        }
    }
}

impl Backdrop for GdiBackdrop {
    fn bitmap(&self) -> &ID2D1Bitmap1 {
        &self.bmp
    }
    fn name(&self) -> String {
        "desktop-gdi".into()
    }
    fn take_dirty(&self) -> u64 {
        self.shared.dirty.swap(0, Relaxed)
    }
    fn start(&self) {
        let cap = self.pending.lock().ok().and_then(|mut g| g.take());
        if let Some(cap) = cap {
            let th = std::thread::Builder::new().name("gdi-capture".into()).spawn(move || unsafe { gdi_loop(cap.0) }).ok();
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
    fn boost(&self) {
        self.shared.boost.fetch_add(1, Relaxed);
    }
    fn rate_hz(&self) -> u32 {
        self.shared.rate.load(Relaxed)
    }
}

unsafe fn gdi_loop(c: GdiCap) {
    use windows::Win32::Graphics::Gdi::*;
    let sh = &c.shared;
    let screen = GetDC(None);
    let mem = CreateCompatibleDC(screen);
    let mut regions: Vec<RECT> = Vec::new();
    let mut ver = 0u64;
    let mut prev: Vec<Vec<u8>> = Vec::new();
    let (mut dib, mut bits, mut cw, mut ch) = (HBITMAP::default(), std::ptr::null_mut::<u8>(), 0i32, 0i32);
    let mut buf: Vec<u8> = Vec::new();
    let mut pace = Pace::new(c.rate);
    let mut boost = sh.boost.load(Relaxed);
    while !sh.stop.load(Relaxed) {
        let t0 = Instant::now();
        if sh.paused.load(Relaxed) {
            prev.iter_mut().for_each(|p| p.clear());
            pace.snap();
            std::thread::sleep(Duration::from_millis(100));
            continue;
        }
        let v = sh.regions_ver.load(Relaxed);
        if v != ver {
            if let Ok(g) = sh.regions.lock() {
                regions = g.clone();
                ver = v;
                prev = vec![Vec::new(); regions.len()];
            }
        }
        let (mw, mh) = regions.iter().fold((0, 0), |(w, h), r| (w.max(r.right - r.left), h.max(r.bottom - r.top)));
        if mw > cw || mh > ch {
            if !dib.is_invalid() {
                let _ = DeleteObject(dib);
            }
            let (nw, nh) = (mw.max(cw), mh.max(ch));
            let bmi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER { biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32, biWidth: nw, biHeight: -nh, biPlanes: 1, biBitCount: 32, biCompression: BI_RGB.0, ..Default::default() },
                ..Default::default()
            };
            let mut p: *mut std::ffi::c_void = std::ptr::null_mut();
            match CreateDIBSection(screen, &bmi, DIB_RGB_COLORS, &mut p, None, 0) {
                Ok(d) => {
                    dib = d;
                    bits = p as *mut u8;
                    cw = nw;
                    ch = nh;
                    SelectObject(mem, dib);
                }
                Err(_) => {
                    std::thread::sleep(Duration::from_millis(200));
                    continue;
                }
            }
        }
        sh.acquired.fetch_add(1, Relaxed);
        let mut mask = 0u64;
        for (i, r) in regions.iter().enumerate().take(64) {
            let (l, t, rr, b) = (r.left.max(0), r.top.max(0), r.right.min(c.size.0), r.bottom.min(c.size.1));
            let (w, h) = (rr - l, b - t);
            if w <= 0 || h <= 0 || bits.is_null() {
                continue;
            }
            if BitBlt(mem, 0, 0, w, h, screen, c.org.0 + l, c.org.1 + t, SRCCOPY | CAPTUREBLT).is_err() {
                continue;
            }
            let row = (w * 4) as usize;
            buf.clear();
            for y in 0..h as usize {
                buf.extend_from_slice(std::slice::from_raw_parts(bits.add(y * cw as usize * 4), row));
            }
            if prev[i] != buf {
                let bx = D3D11_BOX { left: l as u32, top: t as u32, front: 0, right: rr as u32, bottom: b as u32, back: 1 };
                c.ctx.UpdateSubresource(&c.tex, 0, Some(&bx), buf.as_ptr() as *const _, row as u32, 0);
                prev[i].clone_from(&buf);
                mask |= 1u64 << i;
            }
        }
        if mask != 0 {
            sh.copied.fetch_add(1, Relaxed);
            c.ctx.Flush();
            let prevm = sh.dirty.fetch_or(mask, Relaxed);
            let h = sh.hwnd.load(Relaxed);
            if prevm == 0 && h != 0 {
                let _ = PostMessageW(HWND(h as *mut _), WM_BACKDROP, WPARAM(0), LPARAM(0));
            }
        }
        // poll at the capture fps while things move, backing off to 2 fps while the screen is still
        if sh.boost.load(Relaxed) != boost {
            boost = sh.boost.load(Relaxed);
            pace.snap();
        }
        let period = pace.observe(mask != 0, Instant::now());
        sh.rate.store(pace.fps(), Relaxed);
        if let Some(rest) = period.checked_sub(t0.elapsed()) {
            sh.nap(rest, boost, ver);
        }
    }
    let _ = DeleteDC(mem);
    ReleaseDC(None, screen);
}
