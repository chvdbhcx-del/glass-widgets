//! D3D11 + Direct2D 1.1 + DirectWrite + WIC plumbing shared by every other module.
//! All drawing is GPU side; the only CPU readbacks are the final dirty rectangle of the window
//! (to hand it to UpdateLayeredWindow) and a 8x8 luminance probe (at most 2 Hz).
use std::mem::ManuallyDrop;
use windows::core::{Interface, Result, HSTRING};
use windows::Foundation::Numerics::Matrix3x2;
use windows::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE, HMODULE, RECT};
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::Graphics::Imaging::*;
use windows::Win32::System::Com::*;

pub struct Gfx {
    pub d3d: ID3D11Device,
    pub ctx: ID3D11DeviceContext,
    pub d2d: ID2D1Factory1,
    pub dc: ID2D1DeviceContext,
    pub dw: IDWriteFactory,
    pub wic: IWICImagingFactory,
    pub warp: bool,
    /// the glass pass renders into 16-bit float targets (`target_hdr`), effect buffers are 16-bit float, and `quantize_into` dithers down to 8 bit;
    /// false = the old 8-bit pipeline (`MSWIDGETS_NO_FP16=1`, or a device without FP16 render targets)
    pub fp16: bool,
    /// strength of the final dither, 1 = normal (0 only for the banding diagnostics)
    dither: f32,
    quant: std::cell::RefCell<Option<Quantizer>>,
    _dev: ID2D1Device,
}

/// The FP16 -> dithered 8-bit conversion (see `Gfx::quantize`): noise tile -> Tile effect -> Composite PLUS with the source, drawn into `out`.
struct Quantizer {
    comp: ID2D1Effect,
    out: Option<(u32, u32, ID2D1Bitmap1)>,
    mid: Option<(u32, u32, ID2D1Bitmap1)>,
    _tile: ID2D1Effect,
    _noise: ID2D1Bitmap1,
    /// the same tile as a wrapping brush at full +-0.49 level strength, for `add_noise`
    brush: ID2D1BitmapBrush1,
    _noise_full: ID2D1Bitmap1,
}

// ---------------------------------------------------------------- small value helpers

pub fn col(r: f32, g: f32, b: f32, a: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F { r, g, b, a }
}
pub fn rf(l: f32, t: f32, r: f32, b: f32) -> D2D_RECT_F {
    D2D_RECT_F { left: l, top: t, right: r, bottom: b }
}
pub fn pt(x: f32, y: f32) -> D2D_POINT_2F {
    D2D_POINT_2F { x, y }
}
pub fn mat(s: f32, tx: f32, ty: f32) -> Matrix3x2 {
    Matrix3x2 { M11: s, M12: 0.0, M21: 0.0, M22: s, M31: tx, M32: ty }
}
pub fn rr(x: f32, y: f32, w: f32, h: f32, r: f32) -> D2D1_ROUNDED_RECT {
    D2D1_ROUNDED_RECT { rect: rf(x, y, x + w, y + h), radiusX: r, radiusY: r }
}
fn raw<T>(v: &T) -> &[u8] {
    unsafe { std::slice::from_raw_parts(v as *const T as *const u8, std::mem::size_of::<T>()) }
}

// effect property setters (index = the D2D1_*_PROP_* value)
pub fn fx_f32(e: &ID2D1Effect, i: u32, v: f32) -> Result<()> {
    unsafe { e.SetValue(i, D2D1_PROPERTY_TYPE_FLOAT, raw(&v)) }
}
pub fn fx_u32(e: &ID2D1Effect, i: u32, v: u32) -> Result<()> {
    unsafe { e.SetValue(i, D2D1_PROPERTY_TYPE_ENUM, raw(&v)) }
}
pub fn fx_bool(e: &ID2D1Effect, i: u32, v: bool) -> Result<()> {
    let b: i32 = v as i32;
    unsafe { e.SetValue(i, D2D1_PROPERTY_TYPE_BOOL, raw(&b)) }
}
pub fn fx_vec2(e: &ID2D1Effect, i: u32, v: [f32; 2]) -> Result<()> {
    unsafe { e.SetValue(i, D2D1_PROPERTY_TYPE_VECTOR2, raw(&v)) }
}
pub fn fx_vec4(e: &ID2D1Effect, i: u32, v: [f32; 4]) -> Result<()> {
    unsafe { e.SetValue(i, D2D1_PROPERTY_TYPE_VECTOR4, raw(&v)) }
}
pub fn fx_mat5x4(e: &ID2D1Effect, i: u32, v: [f32; 20]) -> Result<()> {
    unsafe { e.SetValue(i, D2D1_PROPERTY_TYPE_MATRIX_5X4, raw(&v)) }
}

pub fn fx_mat3x2(e: &ID2D1Effect, i: u32, v: Matrix3x2) -> Result<()> {
    unsafe { e.SetValue(i, D2D1_PROPERTY_TYPE_MATRIX_3X2, raw(&v)) }
}

pub fn bmp_props(opts: D2D1_BITMAP_OPTIONS, alpha: D2D1_ALPHA_MODE) -> D2D1_BITMAP_PROPERTIES1 {
    bmp_props_fmt(DXGI_FORMAT_B8G8R8A8_UNORM, opts, alpha)
}

pub fn bmp_props_fmt(format: DXGI_FORMAT, opts: D2D1_BITMAP_OPTIONS, alpha: D2D1_ALPHA_MODE) -> D2D1_BITMAP_PROPERTIES1 {
    D2D1_BITMAP_PROPERTIES1 {
        pixelFormat: D2D1_PIXEL_FORMAT { format, alphaMode: alpha },
        dpiX: 96.0,
        dpiY: 96.0,
        bitmapOptions: opts,
        colorContext: ManuallyDrop::new(None),
    }
}

/// Unhinted symmetric grayscale text with mild stem darkening: light text on glass keeps the weight of macOS font smoothing instead of the
/// thin default look (the layered window has no ClearType anyway). `MSW_TEXT_GAMMA` / `MSW_TEXT_CONTRAST` override the defaults for tuning.
fn set_text_params(dw: &IDWriteFactory, dc: &ID2D1DeviceContext) {
    let knob = |name: &str, default: f32| std::env::var(name).ok().and_then(|v| v.parse::<f32>().ok()).unwrap_or(default);
    let (gamma, contrast) = (knob("MSW_TEXT_GAMMA", 2.2), knob("MSW_TEXT_CONTRAST", 0.6));
    unsafe {
        if let Ok(f3) = dw.cast::<IDWriteFactory3>() {
            // `MSW_TEXT_MODE`: sym (default: unhinted symmetric grayscale, smooth curves like macOS, no pixel-snapped stems) | natural (hinted
            // vertically: crisper but blocky edges) | gdi (fully hinted: breaks kerning, kept for comparison) | natural_nofit
            let (mode, fit) = match std::env::var("MSW_TEXT_MODE").unwrap_or_default().as_str() {
                "natural" => (DWRITE_RENDERING_MODE1_NATURAL, DWRITE_GRID_FIT_MODE_ENABLED),
                "gdi" => (DWRITE_RENDERING_MODE1_GDI_NATURAL, DWRITE_GRID_FIT_MODE_ENABLED),
                "natural_nofit" => (DWRITE_RENDERING_MODE1_NATURAL, DWRITE_GRID_FIT_MODE_DISABLED),
                _ => (DWRITE_RENDERING_MODE1_NATURAL_SYMMETRIC, DWRITE_GRID_FIT_MODE_DISABLED),
            };
            if let Ok(p) = f3.CreateCustomRenderingParams(gamma, contrast, contrast, 0.0, DWRITE_PIXEL_GEOMETRY_FLAT, mode, fit) {
                dc.SetTextRenderingParams(&p);
            }
        }
    }
}

impl Gfx {
    /// Device on `adapter` (the GPU that owns the monitor we duplicate) or the default hardware adapter;
    /// falls back to WARP (software) when no hardware device can be created.
    pub fn new(adapter: Option<&IDXGIAdapter1>) -> Result<Gfx> {
        Gfx::create(adapter, false)
    }

    /// A software (WARP) device: what the headless renderers and tests use, so they run the same on a machine without a GPU.
    pub fn warp() -> Result<Gfx> {
        Gfx::create(None, true)
    }

    fn create(adapter: Option<&IDXGIAdapter1>, warp_only: bool) -> Result<Gfx> {
        unsafe {
            let flags = D3D11_CREATE_DEVICE_BGRA_SUPPORT;
            let levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_10_1, D3D_FEATURE_LEVEL_10_0];
            let mut dev: Option<ID3D11Device> = None;
            let mut ctx: Option<ID3D11DeviceContext> = None;
            let mut warp = false;
            let hw = match adapter {
                _ if warp_only => Err(windows::core::Error::empty()),
                Some(a) => {
                    let a: IDXGIAdapter = a.cast()?;
                    D3D11CreateDevice(&a, D3D_DRIVER_TYPE_UNKNOWN, HMODULE::default(), flags, Some(&levels), D3D11_SDK_VERSION, Some(&mut dev), None, Some(&mut ctx))
                }
                None => D3D11CreateDevice(None::<&IDXGIAdapter>, D3D_DRIVER_TYPE_HARDWARE, HMODULE::default(), flags, Some(&levels), D3D11_SDK_VERSION, Some(&mut dev), None, Some(&mut ctx)),
            };
            if hw.is_err() || dev.is_none() {
                if !warp_only {
                    eprintln!("mswidgets: hardware D3D11 device failed ({:?}), using WARP", hw.err());
                }
                warp = true;
                dev = None;
                ctx = None;
                D3D11CreateDevice(None::<&IDXGIAdapter>, D3D_DRIVER_TYPE_WARP, HMODULE::default(), flags, Some(&levels), D3D11_SDK_VERSION, Some(&mut dev), None, Some(&mut ctx))?;
            }
            let d3d = dev.ok_or_else(windows::core::Error::empty)?;
            let ctx = ctx.ok_or_else(windows::core::Error::empty)?;
            // the capture thread copies into the backdrop texture while the UI thread draws with D2D
            if let Ok(mt) = d3d.cast::<ID3D11Multithread>() {
                let _ = mt.SetMultithreadProtected(true);
            }
            let dxgi: IDXGIDevice = d3d.cast()?;
            let opts = D2D1_FACTORY_OPTIONS { debugLevel: D2D1_DEBUG_LEVEL_NONE };
            let d2d: ID2D1Factory1 = D2D1CreateFactory(D2D1_FACTORY_TYPE_MULTI_THREADED, Some(&opts))?;
            let dev2d = d2d.CreateDevice(&dxgi)?;
            let dc = dev2d.CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE)?;
            dc.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
            let dw: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
            set_text_params(&dw, &dc);
            let wic: IWICImagingFactory = CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;
            // `MSWIDGETS_NO_FP16=1` = the old 8-bit pipeline; diagnostics for the banding analysis: `MSWIDGETS_FP16=fx` (16-bit effect buffers, 8-bit
            // targets) and `MSWIDGETS_FP16=nodither` (FP16 targets, plain rounding at the end)
            let off = std::env::var_os("MSWIDGETS_NO_FP16").is_some_and(|v| !v.is_empty() && v != "0");
            let mode = std::env::var("MSWIDGETS_FP16").unwrap_or_default();
            let can = !off && Gfx::probe_fp16(&dc);
            let fp16 = can && mode != "fx";
            let dither = if mode == "nodither" { 0.0 } else { 1.0 };
            if can {
                // effect graphs (blur, displacement, shadow, colour matrix ...) keep 16-bit float intermediates instead of 8-bit ones
                let mut rc = dc.GetRenderingControls();
                rc.bufferPrecision = D2D1_BUFFER_PRECISION_16BPC_FLOAT;
                dc.SetRenderingControls(&rc);
            }
            Ok(Gfx { d3d, ctx, d2d, dc, dw, wic, warp, fp16, dither, quant: Default::default(), _dev: dev2d })
        }
    }

    /// Can this device render into R16G16B16A16_FLOAT bitmaps?
    fn probe_fp16(dc: &ID2D1DeviceContext) -> bool {
        let p = bmp_props_fmt(DXGI_FORMAT_R16G16B16A16_FLOAT, D2D1_BITMAP_OPTIONS_TARGET, D2D1_ALPHA_MODE_PREMULTIPLIED);
        unsafe { dc.CreateBitmap(D2D_SIZE_U { width: 4, height: 4 }, None, 0, &p) }.is_ok()
    }

    // ------------------------------------------------------------ bitmaps

    /// GPU render target, premultiplied BGRA, 8-bit: everything that is composed into the window or read back directly.
    pub fn target(&self, w: u32, h: u32) -> Result<ID2D1Bitmap1> {
        self.target8(w, h)
    }
    /// Premultiplied 16-bit float RGBA target (8-bit BGRA when `fp16` is off): the high-precision surface of the glass pass. Never read it back
    /// directly; `quantize_into` converts it to dithered 8 bit.
    pub fn target_hdr(&self, w: u32, h: u32) -> Result<ID2D1Bitmap1> {
        if !self.fp16 {
            return self.target8(w, h);
        }
        let p = bmp_props_fmt(DXGI_FORMAT_R16G16B16A16_FLOAT, D2D1_BITMAP_OPTIONS_TARGET, D2D1_ALPHA_MODE_PREMULTIPLIED);
        unsafe { self.dc.CreateBitmap(D2D_SIZE_U { width: w, height: h }, None, 0, &p) }
    }
    /// GPU render target, premultiplied BGRA, always 8-bit (what `cpu_read` can copy from directly).
    pub fn target8(&self, w: u32, h: u32) -> Result<ID2D1Bitmap1> {
        let p = bmp_props(D2D1_BITMAP_OPTIONS_TARGET, D2D1_ALPHA_MODE_PREMULTIPLIED);
        unsafe { self.dc.CreateBitmap(D2D_SIZE_U { width: w, height: h }, None, 0, &p) }
    }
    /// CPU-readable staging bitmap (copy a target into it with CopyFromBitmap, then Map).
    pub fn cpu_read(&self, w: u32, h: u32) -> Result<ID2D1Bitmap1> {
        let p = bmp_props(D2D1_BITMAP_OPTIONS_CPU_READ | D2D1_BITMAP_OPTIONS_CANNOT_DRAW, D2D1_ALPHA_MODE_PREMULTIPLIED);
        unsafe { self.dc.CreateBitmap(D2D_SIZE_U { width: w, height: h }, None, 0, &p) }
    }
    /// Immutable bitmap from premultiplied BGRA pixels.
    pub fn from_bgra(&self, w: u32, h: u32, px: &[u8]) -> Result<ID2D1Bitmap1> {
        let p = bmp_props(D2D1_BITMAP_OPTIONS_NONE, D2D1_ALPHA_MODE_PREMULTIPLIED);
        unsafe { self.dc.CreateBitmap(D2D_SIZE_U { width: w, height: h }, Some(px.as_ptr() as *const _), w * 4, &p) }
    }

    /// Is `bmp` a 16-bit float bitmap (it must go through `quantize` before a CPU read)?
    pub fn is_fp16(bmp: &ID2D1Bitmap1) -> bool {
        unsafe { bmp.GetPixelFormat().format == DXGI_FORMAT_R16G16B16A16_FLOAT }
    }

    /// `r` of the FP16 bitmap `src` as a dithered 8-bit bitmap of the same size (only `r` is written; the bitmap is reused by the next call).
    /// The conversion is `round(value * 255 + noise)` with a blue-noise tile of +-0.375 level anchored at the bitmap's own pixel grid:
    /// the 16-bit gradients come out as fine, fixed noise instead of 1-level steps, a flat colour that is a whole number of levels stays exact,
    /// and the same pixels always get the same noise (a static card does not shimmer).
    pub fn quantize(&self, src: &ID2D1Bitmap1, r: RECT) -> Result<ID2D1Bitmap1> {
        let size = unsafe { src.GetPixelSize() };
        let out = {
            let mut st = self.quant.borrow_mut();
            if st.is_none() {
                *st = Some(self.make_quantizer()?);
            }
            let q = st.as_mut().ok_or_else(windows::core::Error::empty)?;
            if q.out.as_ref().is_none_or(|o| (o.0, o.1) != (size.width, size.height)) {
                q.out = Some((size.width, size.height, self.target8(size.width, size.height)?));
            }
            q.out.as_ref().map(|o| o.2.clone()).ok_or_else(windows::core::Error::empty)?
        };
        self.quantize_into(src, &out, r)?;
        Ok(out)
    }

    /// `quantize` into a bitmap of the caller's (8-bit, the size of `src`): only `r` is written.
    pub fn quantize_into(&self, src: &ID2D1Bitmap1, out: &ID2D1Bitmap1, r: RECT) -> Result<()> {
        let mut st = self.quant.borrow_mut();
        if st.is_none() {
            *st = Some(self.make_quantizer()?);
        }
        let q = st.as_mut().ok_or_else(windows::core::Error::empty)?;
        // stage 1: source + noise, rendered into a float32 scratch (the sum is not rounded to half-float) (an effect graph drawn straight into an 8-bit target is evaluated at 8 bit,
        // which would round the source before the noise is added); stage 2: the plain copy into the 8-bit target rounds to the nearest level
        let (rw, rh) = ((r.right - r.left).max(1) as u32, (r.bottom - r.top).max(1) as u32);
        if q.mid.as_ref().is_none_or(|m| m.0 < rw || m.1 < rh) {
            let (mw, mh) = ((rw + 255) / 256 * 256, (rh + 255) / 256 * 256);
            let p = bmp_props_fmt(DXGI_FORMAT_R32G32B32A32_FLOAT, D2D1_BITMAP_OPTIONS_TARGET, D2D1_ALPHA_MODE_PREMULTIPLIED);
            q.mid = Some((mw, mh, unsafe { self.dc.CreateBitmap(D2D_SIZE_U { width: mw, height: mh }, None, 0, &p)? }));
        }
        let mid = q.mid.as_ref().map(|m| m.2.clone()).ok_or_else(windows::core::Error::empty)?;
        unsafe {
            q.comp.SetInput(0, src, true);
            let img = q.comp.GetOutput()?;
            let dc = &self.dc;
            // float32 intermediates for this one pass: the half-float default would round (truncate, on some drivers) the sum by up to a whole
            // half-float step, 1/8 level for bright values
            let mut rc = dc.GetRenderingControls();
            let prec = rc.bufferPrecision;
            rc.bufferPrecision = D2D1_BUFFER_PRECISION_32BPC_FLOAT;
            dc.SetRenderingControls(&rc);
            dc.SetTarget(&mid);
            dc.BeginDraw();
            dc.SetTransform(&mat(1.0, 0.0, 0.0));
            // the tile is anchored at the SOURCE's pixel grid: shift the rectangle to the scratch origin through the image, not the noise
            dc.DrawImage(&img, Some(&pt(0.0, 0.0)), Some(&rf(r.left as f32, r.top as f32, r.right as f32, r.bottom as f32)), D2D1_INTERPOLATION_MODE_NEAREST_NEIGHBOR, D2D1_COMPOSITE_MODE_SOURCE_COPY);
            let mut e = dc.EndDraw(None, None);
            dc.SetTarget(None::<&ID2D1Image>);
            rc.bufferPrecision = prec;
            dc.SetRenderingControls(&rc);
            if e.is_ok() {
                dc.SetTarget(out);
                dc.BeginDraw();
                dc.SetTransform(&mat(1.0, 0.0, 0.0));
                let dst = rf(r.left as f32, r.top as f32, r.right as f32, r.bottom as f32);
                dc.SetPrimitiveBlend(D2D1_PRIMITIVE_BLEND_COPY); // a pixel with alpha 254.6 / 255 must not let the previous contents shine through
                dc.DrawBitmap(&mid, Some(&dst), 1.0, D2D1_INTERPOLATION_MODE_NEAREST_NEIGHBOR, Some(&rf(0.0, 0.0, rw as f32, rh as f32)), None);
                dc.SetPrimitiveBlend(D2D1_PRIMITIVE_BLEND_SOURCE_OVER);
                e = dc.EndDraw(None, None);
                dc.SetTarget(None::<&ID2D1Image>);
            }
            e?;
        }
        Ok(())
    }

    /// Inside an open draw session on an FP16 target: add the blue-noise dither (+-0.49 level, unbiased) to the rectangle (0, 0, w, h) of the
    /// target, anchored at its pixel grid. The caller then copies the target into an 8-bit one with a plain copy, which rounds to the nearest level.
    pub fn add_noise(&self, w: f32, h: f32) -> Result<()> {
        let mut st = self.quant.borrow_mut();
        if st.is_none() {
            *st = Some(self.make_quantizer()?);
        }
        let q = st.as_ref().ok_or_else(windows::core::Error::empty)?;
        unsafe {
            q.brush.SetTransform(&mat(1.0, 0.0, 0.0));
            self.dc.SetPrimitiveBlend(D2D1_PRIMITIVE_BLEND_ADD);
            self.dc.FillRectangle(&rf(0.0, 0.0, w, h), &q.brush);
            self.dc.SetPrimitiveBlend(D2D1_PRIMITIVE_BLEND_SOURCE_OVER);
        }
        Ok(())
    }

    fn make_quantizer(&self) -> Result<Quantizer> {
        use crate::dither::{offset, ranks, N};
        // the noise tile: grey, the same offset in R, G, B and A (so a premultiplied pixel keeps colour <= alpha)
        let mut px = Vec::<u16>::with_capacity(N * N * 4);
        for &rk in ranks() {
            let v = f32_to_f16(offset(rk) * self.dither / 255.0);
            px.extend_from_slice(&[v, v, v, v]);
        }
        let p = bmp_props_fmt(DXGI_FORMAT_R16G16B16A16_FLOAT, D2D1_BITMAP_OPTIONS_NONE, D2D1_ALPHA_MODE_PREMULTIPLIED);
        let noise = unsafe { self.dc.CreateBitmap(D2D_SIZE_U { width: N as u32, height: N as u32 }, Some(px.as_ptr() as *const _), (N * 8) as u32, &p)? };
        unsafe {
            let tile = self.dc.CreateEffect(&CLSID_D2D1Tile)?;
            tile.SetInput(0, &noise, true);
            fx_vec4(&tile, 0, [0.0, 0.0, N as f32, N as f32])?; // D2D1_TILE_PROP_RECT
            let comp = self.dc.CreateEffect(&CLSID_D2D1Composite)?;
            comp.SetInputCount(2)?;
            fx_u32(&comp, 0, D2D1_COMPOSITE_MODE_PLUS.0 as u32)?;
            comp.SetInput(1, &tile.GetOutput()?, true);
            for e in [&tile, &comp] {
                e.SetValue(D2D1_PROPERTY_PRECISION.0 as u32, D2D1_PROPERTY_TYPE_ENUM, raw(&(D2D1_BUFFER_PRECISION_32BPC_FLOAT.0 as u32)))?;
            }
            let mut px2 = Vec::<u16>::with_capacity(N * N * 4);
            for &rk in ranks() {
                let v = f32_to_f16(offset(rk) / 0.75 * 0.98 * self.dither / 255.0);
                px2.extend_from_slice(&[v, v, v, v]);
            }
            let noise_full = self.dc.CreateBitmap(D2D_SIZE_U { width: N as u32, height: N as u32 }, Some(px2.as_ptr() as *const _), (N * 8) as u32, &p)?;
            let bp = D2D1_BITMAP_BRUSH_PROPERTIES1 { extendModeX: D2D1_EXTEND_MODE_WRAP, extendModeY: D2D1_EXTEND_MODE_WRAP, interpolationMode: D2D1_INTERPOLATION_MODE_NEAREST_NEIGHBOR };
            let brush = self.dc.CreateBitmapBrush(&noise_full, Some(&bp), None)?;
            Ok(Quantizer { comp, out: None, mid: None, _tile: tile, _noise: noise, brush, _noise_full: noise_full })
        }
    }

    /// Copy `r` of `src` (a target bitmap) through `cpu` and call `f(y, row_bytes)` for each row (an FP16 `src` is dithered to 8 bit first).
    pub fn read_rect(&self, src: &ID2D1Bitmap1, cpu: &ID2D1Bitmap1, r: RECT, mut f: impl FnMut(i32, &[u8])) -> Result<()> {
        let q;
        let src = if Gfx::is_fp16(src) {
            q = self.quantize(src, r)?;
            &q
        } else {
            src
        };
        unsafe {
            let dst = D2D_POINT_2U { x: r.left as u32, y: r.top as u32 };
            let sr = D2D_RECT_U { left: r.left as u32, top: r.top as u32, right: r.right as u32, bottom: r.bottom as u32 };
            cpu.CopyFromBitmap(Some(&dst), src, Some(&sr))?;
            let m = cpu.Map(D2D1_MAP_OPTIONS_READ)?;
            let n = ((r.right - r.left) * 4) as usize;
            for y in r.top..r.bottom {
                let row = std::slice::from_raw_parts(m.bits.add(y as usize * m.pitch as usize + r.left as usize * 4), n);
                f(y, row);
            }
            cpu.Unmap()
        }
    }

    /// Record drawing into a command list (the device context target is restored to none afterwards).
    pub fn record(&self, f: impl FnOnce(&ID2D1DeviceContext) -> Result<()>) -> Result<ID2D1CommandList> {
        unsafe {
            let cl = self.dc.CreateCommandList()?;
            self.dc.SetTarget(&cl);
            self.dc.BeginDraw();
            self.dc.SetTransform(&mat(1.0, 0.0, 0.0));
            let r = f(&self.dc);
            let e = self.dc.EndDraw(None, None);
            self.dc.SetTarget(None::<&ID2D1Image>);
            r?;
            e?;
            cl.Close()?;
            Ok(cl)
        }
    }

    /// Linear gradient of white with the given (position, alpha) stops.
    pub fn white_gradient(&self, p0: (f32, f32), p1: (f32, f32), stops: &[(f32, f32)]) -> Result<ID2D1LinearGradientBrush> {
        unsafe {
            let st: Vec<D2D1_GRADIENT_STOP> = stops.iter().map(|&(p, a)| D2D1_GRADIENT_STOP { position: p, color: col(1.0, 1.0, 1.0, a) }).collect();
            let coll = self.dc.CreateGradientStopCollection(&st, D2D1_COLOR_SPACE_SRGB, D2D1_COLOR_SPACE_SRGB, D2D1_BUFFER_PRECISION_16BPC_FLOAT, D2D1_EXTEND_MODE_CLAMP, D2D1_COLOR_INTERPOLATION_MODE_STRAIGHT)?;
            let props = D2D1_LINEAR_GRADIENT_BRUSH_PROPERTIES { startPoint: pt(p0.0, p0.1), endPoint: pt(p1.0, p1.1) };
            self.dc.CreateLinearGradientBrush(&props, None, &coll)
        }
    }

    // ------------------------------------------------------------ WIC png

    /// Image -> premultiplied BGRA exactly w x h: scaled (Fant) to COVER the box, the overflow cropped around the centre
    /// (a backdrop of another aspect ratio is never stretched).
    pub fn load_png(&self, path: &str, w: u32, h: u32) -> Result<Vec<u8>> {
        unsafe {
            let dec = self.wic.CreateDecoderFromFilename(&HSTRING::from(path), None, GENERIC_READ, WICDecodeMetadataCacheOnDemand)?;
            let fr = dec.GetFrame(0)?;
            let (mut iw, mut ih) = (0u32, 0u32);
            fr.GetSize(&mut iw, &mut ih)?;
            let k = (w as f32 / iw.max(1) as f32).max(h as f32 / ih.max(1) as f32);
            let (sw, sh) = (((iw as f32 * k).ceil() as u32).max(w), ((ih as f32 * k).ceil() as u32).max(h));
            let sc = self.wic.CreateBitmapScaler()?;
            sc.Initialize(&fr, sw, sh, WICBitmapInterpolationModeFant)?;
            let cl = self.wic.CreateBitmapClipper()?;
            cl.Initialize(&sc, &WICRect { X: ((sw - w) / 2) as i32, Y: ((sh - h) / 2) as i32, Width: w as i32, Height: h as i32 })?;
            let cv = self.wic.CreateFormatConverter()?;
            cv.Initialize(&cl, &GUID_WICPixelFormat32bppPBGRA, WICBitmapDitherTypeNone, None, 0.0, WICBitmapPaletteTypeCustom)?;
            let mut buf = vec![0u8; (w * h * 4) as usize];
            cv.CopyPixels(std::ptr::null(), w * 4, &mut buf)?;
            Ok(buf)
        }
    }

    /// Straight BGRA pixels -> PNG file.
    pub fn save_png(&self, path: &str, w: u32, h: u32, bgra: &[u8]) -> Result<()> {
        unsafe {
            let stream = self.wic.CreateStream()?;
            stream.InitializeFromFilename(&HSTRING::from(path), GENERIC_WRITE.0)?;
            let enc = self.wic.CreateEncoder(&GUID_ContainerFormatPng, std::ptr::null())?;
            enc.Initialize(&stream, WICBitmapEncoderNoCache)?;
            let mut fr: Option<IWICBitmapFrameEncode> = None;
            enc.CreateNewFrame(&mut fr, std::ptr::null_mut())?;
            let fr = fr.ok_or_else(windows::core::Error::empty)?;
            fr.Initialize(None)?;
            fr.SetSize(w, h)?;
            let mut fmt = GUID_WICPixelFormat32bppBGRA;
            fr.SetPixelFormat(&mut fmt)?;
            fr.WritePixels(h, w * 4, bgra)?;
            fr.Commit()?;
            enc.Commit()
        }
    }
}

/// IEEE binary16 of an f32 (round to nearest; the noise offsets are small normal numbers).
fn f32_to_f16(v: f32) -> u16 {
    let b = v.to_bits();
    let sign = ((b >> 16) & 0x8000) as u16;
    let e = ((b >> 23) & 0xff) as i32 - 127 + 15;
    let m = b & 0x7f_ffff;
    if e <= 0 {
        return sign; // below the normal range: the noise offsets never get there
    }
    if e >= 31 {
        return sign | 0x7bff;
    }
    let mut h = ((e as u32) << 10) | (m >> 13);
    if m & 0x1000 != 0 {
        h += 1; // round half up (a carry into the exponent is correct)
    }
    sign | h as u16
}

#[cfg(test)]
mod f16_tests {
    use super::f32_to_f16;
    fn half_to_f32(h: u16) -> f32 {
        let (s, e, m) = ((h >> 15) & 1, ((h >> 10) & 31) as i32, (h & 1023) as f32);
        let v = if e == 0 { m / 1024.0 * 2f32.powi(-14) } else { (1.0 + m / 1024.0) * 2f32.powi(e - 15) };
        if s == 1 {
            -v
        } else {
            v
        }
    }
    #[test]
    fn half_floats() {
        assert_eq!(f32_to_f16(1.0), 0x3c00);
        assert_eq!(f32_to_f16(-2.0), 0xc000);
        assert_eq!(f32_to_f16(0.5), 0x3800);
        assert_eq!(f32_to_f16(0.0), 0);
        assert!((half_to_f32(f32_to_f16(0.001)) - 0.001).abs() < 1e-6);
        assert!((half_to_f32(f32_to_f16(-0.0019)) + 0.0019).abs() < 2e-6);
    }
}

#[cfg(test)]
mod quantize_tests {
    use super::*;

    fn fill(g: &Gfx, w: u32, h: u32, c: D2D1_COLOR_F) -> ID2D1Bitmap1 {
        let t = g.target_hdr(w, h).unwrap();
        unsafe {
            g.dc.SetTarget(&t);
            g.dc.BeginDraw();
            g.dc.Clear(Some(&c));
            g.dc.EndDraw(None, None).unwrap();
            g.dc.SetTarget(None::<&ID2D1Image>);
        }
        t
    }

    fn read(g: &Gfx, t: &ID2D1Bitmap1, w: u32, h: u32) -> Vec<u8> {
        let cpu = g.cpu_read(w, h).unwrap();
        let mut out = Vec::new();
        g.read_rect(t, &cpu, RECT { left: 0, top: 0, right: w as i32, bottom: h as i32 }, |_, row| out.extend_from_slice(row)).unwrap();
        out
    }

    #[test]
    fn dither_turns_a_fraction_of_a_level_into_the_right_mix_and_keeps_whole_levels() {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        let g = Gfx::warp().unwrap();
        if !g.fp16 {
            return; // MSWIDGETS_NO_FP16=1
        }
        // opaque grey at 51.3 levels: the offsets are uniform in +-A (A = 0.375), so P(upper level) = (A - 0.2) / 2A = 0.233, the rest stays 51
        let t = fill(&g, 128, 128, col(51.3 / 255.0, 51.3 / 255.0, 51.3 / 255.0, 1.0));
        let px = read(&g, &t, 128, 128);
        let n = px.chunks(4).count();
        let hi = px.chunks(4).filter(|p| p[1] == 52).count();
        let lo = px.chunks(4).filter(|p| p[1] == 51).count();
        assert_eq!(hi + lo, n, "only the two neighbouring levels appear");
        let f = hi as f32 / n as f32;
        assert!((f - 0.233).abs() < 0.03, "share of the upper level {f}");
        assert!(px.chunks(4).all(|p| p[3] == 255), "opaque stays opaque");
        // a whole number of levels (and 0 / 255, and a fully transparent pixel) never moves, even though half-float storage costs up to 1/8 level
        for v in [0u8, 1, 17, 64, 100, 127, 128, 129, 160, 200, 230, 250, 252, 253, 254, 255] {
            let t = fill(&g, 64, 64, col(v as f32 / 255.0, v as f32 / 255.0, v as f32 / 255.0, 1.0));
            assert!(read(&g, &t, 64, 64).chunks(4).all(|p| p[0] == v && p[1] == v && p[2] == v && p[3] == 255), "level {v} moved");
        }
        let t = fill(&g, 64, 64, col(0.0, 0.0, 0.0, 0.0));
        assert!(read(&g, &t, 64, 64).iter().all(|&b| b == 0), "transparent stays transparent");
        // deterministic: the same bitmap quantized twice is identical
        let t = fill(&g, 64, 64, col(0.1234, 0.2, 0.3456, 1.0));
        assert_eq!(read(&g, &t, 64, 64), read(&g, &t, 64, 64));
    }
}
