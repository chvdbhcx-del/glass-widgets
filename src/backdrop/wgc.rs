//! Backdrop from Windows Graphics Capture of the desktop window (`Progman`: wallpaper, the animated wallpaper window and the icons - but
//! never other top-level windows, and so never our own layer). That is what lets the layer be visible to screenshots and recorders: the
//! glass does not see itself. Frames arrive as GPU textures and are copied on a private device into the shared backdrop texture (like the
//! duplication path, but without the 250 ms waits); the UI thread only reads the shared texture.
//!
//! Pace: every frame is copied (the texture is always current), but a card is only flagged dirty when its region really changed: a GPU mip chain
//! shrinks the frame by 8x, the small level is read back and hashed per card region. While nothing changes the capture thread backs off
//! 15 -> 8 -> 4 -> 2 fps (`pace.rs`); any change, a card move or a host `boost()` snaps it back to full rate.
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
    rate: Rate,
    sampler: Option<Sampler>,
    shared: Arc<Shared>,
    /// keeps the capture session (and its device) alive
    _session: windows::Graphics::Capture::GraphicsCaptureSession,
    _dev: ID3D11Device,
}

impl WgcBackdrop {
    /// `out`: the monitor's output (its rect is the area the backdrop texture covers); `cards`: card rects in monitor px.
    pub fn new(gfx: &Gfx, out: &OutputInfo, cards: &[RECT], margin: i32, rate: Rate) -> Result<Self> {
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

            let shared = Shared::new(pad_regions(cards, margin, w as i32, h as i32));
            let sampler = Sampler::new(&cap_dev, w, h).map_err(|e| eprintln!("mswidgets: capture change detection unavailable ({e}), full rate")).ok();
            let cap = SendBox(WgcCap {
                ctx: cap_ctx,
                tex,
                pool,
                org: origin_in_window(&mon, &pr),
                size: (w as i32, h as i32),
                // the glass is blurred: a wallpaper video under it needs no more than ~15 fps (a fixed `captureFps` may ask for up to 30)
                rate: if rate.auto { Rate { fps: rate.fps.clamp(1, 15), auto: true } } else { Rate { fps: rate.fps.clamp(1, 30), auto: false } },
                sampler,
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
    fn boost(&self) {
        self.shared.boost.fetch_add(1, Relaxed);
    }
    fn rate_hz(&self) -> u32 {
        self.shared.rate.load(Relaxed)
    }
}

/// Which part of a captured frame lands in the monitor's backdrop texture. `org` = the monitor's top-left inside the captured window
/// (monitor origin minus the window origin, desktop px; negative or positive: the desktop window spans ALL monitors, so a monitor left of
/// or above the primary one has a smaller origin), `size` = the monitor size, `content` = the size of the frame. Returns the source box
/// `[left, top, right, bottom]` in the frame and the destination offset in the texture, None when none of the monitor is in the frame.
fn crop(org: (i32, i32), size: (i32, i32), content: (i32, i32)) -> Option<([i32; 4], (i32, i32))> {
    let (l, t) = (org.0.max(0), org.1.max(0));
    let (r, b) = ((org.0 + size.0).min(content.0), (org.1 + size.1).min(content.1));
    (r > l && b > t).then_some(([l, t, r, b], (l - org.0, t - org.1)))
}

/// The monitor origin inside the captured desktop window.
fn origin_in_window(mon: &RECT, window: &RECT) -> (i32, i32) {
    (mon.left - window.left, mon.top - window.top)
}

/// Mip level read back for change detection: 1/8 of the frame in each direction (a 4K monitor gives a 480x270 read).
const LEVEL: u32 = 3;

/// GPU downscale of the captured crop + a small staging readback. Cheap on the CPU: the host never sees the full-size pixels.
struct Sampler {
    tex: ID3D11Texture2D,
    srv: ID3D11ShaderResourceView,
    stage: ID3D11Texture2D,
    /// size of the read level in px
    dims: (u32, u32),
}

impl Sampler {
    unsafe fn new(dev: &ID3D11Device, w: u32, h: u32) -> Result<Sampler> {
        let dims = (level_size(w), level_size(h));
        let mut desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: LEVEL + 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: D3D11_RESOURCE_MISC_GENERATE_MIPS.0 as u32,
        };
        let mut tex = None;
        dev.CreateTexture2D(&desc, None, Some(&mut tex))?;
        let tex: ID3D11Texture2D = tex.ok_or_else(windows::core::Error::empty)?;
        let mut srv = None;
        dev.CreateShaderResourceView(&tex, None, Some(&mut srv))?;
        desc = D3D11_TEXTURE2D_DESC { Width: dims.0, Height: dims.1, MipLevels: 1, Usage: D3D11_USAGE_STAGING, BindFlags: 0, CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32, MiscFlags: 0, ..desc };
        let mut stage = None;
        dev.CreateTexture2D(&desc, None, Some(&mut stage))?;
        Ok(Sampler { tex, srv: srv.ok_or_else(windows::core::Error::empty)?, stage: stage.ok_or_else(windows::core::Error::empty)?, dims })
    }

    /// Copy `bx` of `src` to (`dx`,`dy`), build the mips and return the small level as tight BGRA rows.
    unsafe fn read(&self, ctx: &ID3D11DeviceContext, src: &ID3D11Texture2D, bx: &D3D11_BOX, dx: u32, dy: u32, out: &mut Vec<u8>) -> Option<()> {
        ctx.CopySubresourceRegion(&self.tex, 0, dx, dy, 0, src, 0, Some(bx));
        ctx.GenerateMips(&self.srv);
        ctx.CopySubresourceRegion(&self.stage, 0, 0, 0, 0, &self.tex, LEVEL, None);
        let mut m = D3D11_MAPPED_SUBRESOURCE::default();
        ctx.Map(&self.stage, 0, D3D11_MAP_READ, 0, Some(&mut m)).ok()?;
        let row = self.dims.0 as usize * 4;
        out.clear();
        for y in 0..self.dims.1 as usize {
            out.extend_from_slice(std::slice::from_raw_parts((m.pData as *const u8).add(y * m.RowPitch as usize), row));
        }
        ctx.Unmap(&self.stage, 0);
        Some(())
    }
}

/// Size of one dimension at mip `LEVEL` (D3D: floor, at least 1).
fn level_size(n: u32) -> u32 {
    (n >> LEVEL).max(1)
}

/// One hash per region (monitor px, padded) over the pixels of the small level it covers; empty regions hash to 0.
fn region_hashes(px: &[u8], dims: (u32, u32), regions: &[RECT]) -> Vec<u64> {
    let up = (1i32 << LEVEL) - 1;
    regions
        .iter()
        .take(64)
        .map(|r| {
            let (l, t) = ((r.left.max(0) >> LEVEL) as u32, (r.top.max(0) >> LEVEL) as u32);
            let (rr, b) = (((r.right.max(0) + up) >> LEVEL) as u32, ((r.bottom.max(0) + up) >> LEVEL) as u32);
            let (rr, b) = (rr.min(dims.0), b.min(dims.1));
            if r.right <= r.left || r.bottom <= r.top || rr <= l || b <= t {
                return 0;
            }
            let mut h = 0xcbf29ce484222325u64; // FNV-1a over 4-byte pixels
            for y in t..b {
                let o = (y * dims.0 + l) as usize * 4;
                for chunk in px[o..o + (rr - l) as usize * 4].chunks_exact(4) {
                    h = (h ^ u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]) as u64).wrapping_mul(0x100000001b3);
                }
            }
            h | 1 // never 0, so an empty region is told apart
        })
        .collect()
}

/// Bit i = region i is not empty and its hash differs from the previous look (or there was none). No hashes (sampler unavailable) = all.
fn changed_mask(regions: &[RECT], now: Option<&[u64]>, prev: &[u64]) -> u64 {
    regions
        .iter()
        .take(64)
        .enumerate()
        .filter(|(_, r)| r.right > r.left && r.bottom > r.top)
        .filter(|(i, _)| now.is_none_or(|h| prev.get(*i) != h.get(*i)))
        .fold(0u64, |m, (i, _)| m | 1 << i)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rc(l: i32, t: i32, w: i32, h: i32) -> RECT {
        RECT { left: l, top: t, right: l + w, bottom: t + h }
    }

    #[test]
    fn region_hash_sees_changes_only_inside_the_region() {
        // 480x270 level (a 3840x2160 frame)
        let dims = (480u32, 270u32);
        let mut px = vec![7u8; (dims.0 * dims.1 * 4) as usize];
        let regs = [rc(80, 80, 800, 400), rc(2000, 1000, 400, 400), RECT::default()];
        let a = region_hashes(&px, dims, &regs);
        assert_eq!(a.len(), 3);
        assert_eq!(a[2], 0);
        assert_eq!(a, region_hashes(&px, dims, &regs));
        // a pixel inside card 0 (level px 20,20 = frame px 160,160)
        px[((20 * dims.0 + 20) * 4) as usize] = 9;
        let b = region_hashes(&px, dims, &regs);
        assert_ne!(a[0], b[0]);
        assert_eq!(a[1], b[1]);
        // first look: everything non-empty is changed; identical look: nothing; one card changed: its bit only
        assert_eq!(changed_mask(&regs, Some(&a), &[]), 0b011);
        assert_eq!(changed_mask(&regs, Some(&a), &a), 0);
        assert_eq!(changed_mask(&regs, Some(&b), &a), 0b001);
        // no change detection: every non-empty region is dirty, as before
        assert_eq!(changed_mask(&regs, None, &a), 0b011);
    }

    #[test]
    fn region_hash_clips_to_the_level() {
        let dims = (level_size(1920), level_size(1080));
        assert_eq!(dims, (240, 135));
        let px = vec![1u8; (dims.0 * dims.1 * 4) as usize];
        // a region hanging over the frame edge, one entirely outside it
        let h = region_hashes(&px, dims, &[rc(1900, 1070, 100, 100), rc(5000, 0, 10, 10)]);
        assert_ne!(h[0], 0);
        assert_eq!(h[1], 0);
    }

    /// Progman spans the virtual desktop: primary 1920x1080 at 0,0, a second monitor LEFT of it (negative x, shifted down), a third
    /// to the right and above. Every monitor's crop is its own rectangle of the frame, whatever the coordinates.
    #[test]
    fn each_monitor_crops_its_own_part_of_the_virtual_desktop() {
        let prim = rc(0, 0, 1920, 1080);
        let left = rc(-1280, 100, 1280, 1024);
        let right = rc(1920, -200, 2560, 1440);
        let desk = rc(-1280, -200, 1280 + 1920 + 2560, 1440); // union: x -1280..4480, y -200..1240
        let frame = (desk.right - desk.left, desk.bottom - desk.top);
        assert_eq!(frame, (5760, 1440));
        let c = |m: &RECT| crop(origin_in_window(m, &desk), (m.right - m.left, m.bottom - m.top), frame);
        assert_eq!(c(&prim), Some(([1280, 200, 3200, 1280], (0, 0))));
        assert_eq!(c(&left), Some(([0, 300, 1280, 1324], (0, 0))));
        assert_eq!(c(&right), Some(([3200, 0, 5760, 1440], (0, 0))));
        // the crop is as big as the monitor in every case
        for m in [&prim, &left, &right] {
            let ([l, t, r, b], _) = c(m).unwrap();
            assert_eq!((r - l, b - t), (m.right - m.left, m.bottom - m.top));
        }
    }

    /// A window that covers only part of the monitor (the desktop window smaller than the monitor, a frame not yet resized): the
    /// visible part is copied to the matching place of the texture, nothing outside the frame is read.
    #[test]
    fn partial_overlap_keeps_the_destination_offset() {
        // monitor starts 100 px left of / 50 px above the window: the texture's first 100 columns and 50 rows stay empty
        assert_eq!(crop((-100, -50), (1920, 1080), (4000, 3000)), Some(([0, 0, 1820, 1030], (100, 50))));
        // the frame is smaller than the monitor
        assert_eq!(crop((10, 20), (1920, 1080), (1000, 500)), Some(([10, 20, 1000, 500], (0, 0))));
        // the monitor is entirely outside the frame, or the frame is empty
        assert_eq!(crop((5000, 0), (1920, 1080), (4000, 3000)), None);
        assert_eq!(crop((-2000, 0), (1920, 1080), (4000, 3000)), None);
        assert_eq!(crop((0, 0), (1920, 1080), (0, 0)), None);
    }
}

unsafe fn wgc_loop(c: WgcCap) {
    let sh = &c.shared;
    let mut regions: Vec<RECT> = Vec::new();
    let mut ver = 0u64;
    let mut pace = Pace::new(c.rate);
    let mut boost = sh.boost.load(Relaxed);
    let (mut px, mut prev): (Vec<u8>, Vec<u64>) = (Vec::new(), Vec::new());
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
            prev.clear(); // the first look after the pause counts as a change
            pace.snap();
            std::thread::sleep(Duration::from_millis(100));
            continue;
        }
        let v = sh.regions_ver.load(Relaxed);
        if v != ver {
            if let Ok(g) = sh.regions.lock() {
                regions = g.clone();
                ver = v;
                prev.clear(); // other places: compare with nothing
                pace.snap();
            }
        }
        let bst = sh.boost.load(Relaxed);
        if bst != boost {
            boost = bst;
            pace.snap();
        }
        let mut changed = false;
        if let Some(f) = latest {
            sh.acquired.fetch_add(1, Relaxed);
            let src = f.Surface().ok().and_then(|s| s.cast::<IDirect3DDxgiInterfaceAccess>().ok()).and_then(|a| a.GetInterface::<ID3D11Texture2D>().ok());
            if let (Some(src), Ok(cs)) = (src, f.ContentSize()) {
                // the visible part of the monitor inside the captured window
                if let Some(([l, t, r, b], (dx, dy))) = crop(c.org, c.size, (cs.Width, cs.Height)) {
                    let bx = D3D11_BOX { left: l as u32, top: t as u32, front: 0, right: r as u32, bottom: b as u32, back: 1 };
                    c.ctx.CopySubresourceRegion(&c.tex, 0, dx as u32, dy as u32, 0, &src, 0, Some(&bx));
                    c.ctx.Flush();
                    // which cards see a different picture than at the last look (all of them when the sampler is missing / fails)
                    let hashes = c.sampler.as_ref().and_then(|s| s.read(&c.ctx, &src, &bx, dx as u32, dy as u32, &mut px).map(|_| region_hashes(&px, s.dims, &regions)));
                    let mask = changed_mask(&regions, hashes.as_deref(), &prev);
                    if let Some(h) = hashes {
                        prev = h;
                    }
                    if mask != 0 {
                        changed = true;
                        sh.copied.fetch_add(1, Relaxed);
                        let prevm = sh.dirty.fetch_or(mask, Relaxed);
                        let h = sh.hwnd.load(Relaxed);
                        if prevm == 0 && h != 0 {
                            let _ = PostMessageW(HWND(h as *mut _), WM_BACKDROP, WPARAM(0), LPARAM(0));
                        }
                    }
                }
            }
            let _ = f.Close();
        }
        let period = pace.observe(changed, Instant::now());
        sh.rate.store(pace.fps(), Relaxed);
        if let Some(rest) = period.checked_sub(t0.elapsed()) {
            sh.nap(rest, boost, ver);
        }
    }
}
